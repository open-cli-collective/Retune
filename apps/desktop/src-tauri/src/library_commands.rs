use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use retune_core::{
    browse::{self, Selection},
    model::{
        AlbumKey, EffectiveRating, Library, Rating, SourceId, TrackEdit, TrackId, TrackRecord,
    },
};
use retune_spotify::normalize::UNCATEGORIZED;
use serde::{Deserialize, Serialize};
use tauri::Manager;

use crate::{localfiles, notify_error, AppState};

const MAX_LIBRARY_METADATA_BYTES: usize = 4 * 1024;
const MAX_LIBRARY_BATCH_IDS: usize = 10_000;
const MAX_LIBRARY_SELECTION_ITEMS: usize = 1024;

pub(super) fn project_library<T>(
    library: &crate::library_state::LibraryState,
    project: impl FnOnce(&Library) -> T,
) -> T {
    let snapshot = library.snapshot();
    project(&snapshot)
}

#[derive(Deserialize)]
pub(super) struct SelectionDto {
    #[serde(default)]
    cat: Vec<String>,
    #[serde(default)]
    art: Vec<String>,
    #[serde(default)]
    alb: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct BrowseView {
    facets: FacetsView,
    tracks: Vec<TrackView>,
    album_rating: Option<u8>,
    album_rating_artist: Option<String>,
    album_rating_ambiguous: bool,
    counts: CountsView,
}

#[derive(Serialize)]
struct FacetsView {
    cats: Vec<String>,
    arts: Vec<String>,
    albs: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MetadataValues {
    arts: Vec<String>,
    albs: Vec<String>,
    cats: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TrackView {
    id: u64,
    uri: String,
    name: String,
    art: String,
    alb: String,
    cat: String,
    disc_no: Option<u32>,
    track_no: Option<u32>,
    duration_secs: u64,
    enabled: bool,
    play_count: u32,
    last_played_at: Option<u64>,
    added_at: Option<u64>,
    release_date: Option<String>,
    kind: Option<String>,
    bitrate_kbps: Option<u32>,
    overridden: bool,
    is_local: bool,
    rating: Option<RatingView>,
}

impl TrackView {
    fn from_track(track: &TrackRecord, rating: Option<EffectiveRating>) -> Self {
        Self {
            id: track.id.0,
            uri: track.uri.clone(),
            name: track.name.clone(),
            art: track.art.clone(),
            alb: track.alb.clone(),
            cat: track.cat.clone(),
            disc_no: track.disc_no,
            track_no: track.track_no,
            duration_secs: track.duration.as_secs(),
            enabled: track.enabled,
            play_count: track.play_count,
            last_played_at: track.last_played_at,
            added_at: track.added_at,
            release_date: track.release_date.clone(),
            kind: track.kind.clone(),
            bitrate_kbps: track.bitrate_kbps,
            overridden: track
                .orig_cat
                .as_ref()
                .is_some_and(|original| original != &track.cat),
            is_local: track.uri.starts_with("file:"),
            rating: rating.map(rating_view),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TrackInfoView {
    id: u64,
    uri: String,
    local_path: Option<String>,
    source: SourceId,
    name: String,
    art: String,
    alb: String,
    cat: String,
    orig_cat: Option<String>,
    rating: Option<RatingView>,
    inherited_rating: Option<u8>,
    genres: Vec<String>,
}

impl TrackInfoView {
    fn from_track(library: &Library, track: &TrackRecord) -> Self {
        Self {
            id: track.id.0,
            uri: track.uri.clone(),
            local_path: localfiles::path_from_file_uri(&track.uri)
                .ok()
                .map(|path| path.to_string_lossy().into_owned()),
            source: track.source,
            name: track.name.clone(),
            art: track.art.clone(),
            alb: track.alb.clone(),
            cat: track.cat.clone(),
            orig_cat: track.orig_cat.clone(),
            rating: library.effective_rating(track.id).map(rating_view),
            inherited_rating: library
                .album_rating(&AlbumKey::of(track))
                .map(Rating::stars),
            genres: library
                .tracks()
                .iter()
                .filter(|candidate| candidate.source == track.source)
                .map(|candidate| candidate.cat.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TrackEditDto {
    name: Option<String>,
    art: Option<String>,
    alb: Option<String>,
    cat: Option<String>,
    /// Present = set (`stars: n`) or clear (`stars: null`) the explicit
    /// track rating in the same transaction; absent = leave it untouched.
    rating_change: Option<RatingChangeDto>,
}

#[derive(Deserialize)]
struct RatingChangeDto {
    stars: Option<u8>,
}

fn validate_metadata(label: &str, value: &str) -> Result<(), String> {
    if value.len() > MAX_LIBRARY_METADATA_BYTES {
        return Err(format!("{label} is too long."));
    }
    Ok(())
}

fn validate_track_edit(edit: &TrackEditDto) -> Result<(), String> {
    for (label, value) in [
        ("Track name", edit.name.as_deref()),
        ("Artist", edit.art.as_deref()),
        ("Album", edit.alb.as_deref()),
        ("Genre", edit.cat.as_deref()),
    ] {
        if let Some(value) = value {
            validate_metadata(label, value)?;
        }
    }
    Ok(())
}

fn validate_track_ids(ids: &[u64]) -> Result<(), String> {
    if ids.len() > MAX_LIBRARY_BATCH_IDS {
        return Err("Too many track IDs were supplied.".into());
    }
    Ok(())
}

fn validate_browse_input(sel: &SelectionDto, query: Option<&str>) -> Result<(), String> {
    for (label, values) in [
        ("Genre selection", sel.cat.as_slice()),
        ("Artist selection", sel.art.as_slice()),
        ("Album selection", sel.alb.as_slice()),
    ] {
        if values.len() > MAX_LIBRARY_SELECTION_ITEMS {
            return Err(format!("{label} has too many items."));
        }
        values
            .iter()
            .try_for_each(|value| validate_metadata(label, value))?;
    }
    if let Some(query) = query {
        validate_metadata("Search query", query)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct RatingView {
    stars: u8,
    explicit: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CountsView {
    tracks: usize,
    total_secs: u64,
    overlay_edits: usize,
    per_source: PerSourceView,
}

#[derive(Serialize)]
struct PerSourceView {
    music: usize,
    podcasts: usize,
    audiobooks: usize,
}

fn collect_metadata_values(library: &Library) -> MetadataValues {
    let mut arts = BTreeSet::new();
    let mut albs = BTreeSet::new();
    let mut cats = BTreeSet::new();
    for track in library.tracks() {
        if !track.art.is_empty() {
            arts.insert(track.art.clone());
        }
        if !track.alb.is_empty() {
            albs.insert(track.alb.clone());
        }
        if !track.cat.is_empty() && track.cat != UNCATEGORIZED {
            cats.insert(track.cat.clone());
        }
    }
    let sort = |values: BTreeSet<String>| {
        let mut values = values.into_iter().collect::<Vec<_>>();
        values.sort_by_key(|value| value.to_lowercase());
        values
    };
    MetadataValues {
        arts: sort(arts),
        albs: sort(albs),
        cats: sort(cats),
    }
}

fn album_rating_view(
    library: &Library,
    selection: &Selection,
    tracks: &[&TrackRecord],
) -> (Option<u8>, Option<String>, bool) {
    if selection.alb().len() != 1 {
        return (None, None, false);
    }
    let albums = tracks
        .iter()
        .map(|track| AlbumKey::of(track))
        .collect::<BTreeSet<_>>();
    if albums.len() != 1 {
        return (None, None, true);
    }
    let album = albums.into_iter().next().expect("one album key");
    (
        library.album_rating(&album).map(Rating::stars),
        Some(album.art),
        false,
    )
}

fn counts(library: &Library, visible: &[&TrackRecord]) -> CountsView {
    let mut albums = BTreeSet::new();
    let mut overlay_edits = 0;
    let mut per_source = PerSourceView {
        music: 0,
        podcasts: 0,
        audiobooks: 0,
    };
    for track in library.tracks() {
        match track.source {
            SourceId::Music => per_source.music += 1,
            SourceId::Podcasts => per_source.podcasts += 1,
            SourceId::Audiobooks => per_source.audiobooks += 1,
        }
        albums.insert(AlbumKey::of(track));
        overlay_edits += usize::from(track.rating.is_some())
            + usize::from(!track.enabled)
            + usize::from(
                track
                    .orig_cat
                    .as_ref()
                    .is_some_and(|original| original != &track.cat),
            );
    }
    overlay_edits += albums
        .iter()
        .filter(|album| library.album_rating(album).is_some())
        .count();

    CountsView {
        tracks: visible.len(),
        total_secs: visible.iter().map(|track| track.duration.as_secs()).sum(),
        overlay_edits,
        per_source,
    }
}

pub(crate) fn rating_view(rating: EffectiveRating) -> RatingView {
    match rating {
        EffectiveRating::Explicit(rating) => RatingView {
            stars: rating.stars(),
            explicit: true,
        },
        EffectiveRating::Inherited(rating) => RatingView {
            stars: rating.stars(),
            explicit: false,
        },
    }
}

fn rating_change(edit: &TrackEditDto) -> Result<Option<Option<Rating>>, String> {
    edit.rating_change
        .as_ref()
        .map(|change| {
            change
                .stars
                .map(|stars| {
                    Rating::new(stars).ok_or_else(|| format!("invalid star rating {stars}"))
                })
                .transpose()
        })
        .transpose()
}

fn apply_track_info(
    library: &mut Library,
    id: u64,
    edit: &TrackEditDto,
    rating_change: Option<Option<Rating>>,
) -> Result<(), String> {
    validate_track_edit(edit)?;
    library
        .edit(
            TrackId(id),
            TrackEdit {
                name: edit.name.clone(),
                art: edit.art.clone(),
                alb: edit.alb.clone(),
                cat: edit.cat.clone(),
            },
        )
        .map_err(|error| error.to_string())?;
    if let Some(rating) = rating_change {
        library
            .set_track_rating(TrackId(id), rating)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn apply_track_infos(
    library: &mut Library,
    ids: &[u64],
    edit: &TrackEditDto,
) -> Result<(), String> {
    validate_track_ids(ids)?;
    validate_track_edit(edit)?;
    if ids.is_empty() {
        return Err("at least one track id is required".into());
    }
    for &id in ids {
        if library.get(TrackId(id)).is_none() {
            return Err(format!("unknown track id {id}"));
        }
    }
    let rating_change = rating_change(edit)?;
    for &id in ids {
        apply_track_info(library, id, edit, rating_change)?;
    }
    Ok(())
}

fn run_local_import(
    app: &tauri::AppHandle,
    paths: &[PathBuf],
) -> Result<localfiles::ImportSummary, String> {
    let state = app.state::<AppState>();
    let summary = prepare_and_commit_import(&state.library, || localfiles::prepare_paths(paths))?;
    for failure in &summary.failed {
        log::warn!(
            "local import failed for {}: {}",
            failure.path,
            failure.reason
        );
    }
    crate::emit_main(app, "library-changed", ()).map_err(|error| error.to_string())?;
    if let Some(error) = format_import_failures(&summary) {
        crate::emit_main_event(app, crate::main_events::MainEvent::OperationError(error))
            .map_err(|error| error.to_string())?;
    }
    crate::emit_main_event(
        app,
        crate::main_events::MainEvent::LocalImportComplete(summary.clone()),
    )
    .map_err(|error| error.to_string())?;
    Ok(summary)
}

fn prepare_and_commit_import(
    library: &crate::library_state::LibraryState,
    prepare: impl FnOnce() -> localfiles::PreparedImport,
) -> Result<localfiles::ImportSummary, String> {
    let prepared = prepare();
    library.mutate(move |library| Ok(localfiles::commit_prepared(library, prepared)))
}

pub(super) fn launch_local_import(app: tauri::AppHandle, paths: Vec<PathBuf>) {
    let Some(_claim) =
        LocalImportClaim::acquire(Arc::clone(&app.state::<AppState>().local_import_active))
    else {
        notify_error(&app, "A local import is already running.".into());
        return;
    };
    let _ = crate::emit_main(&app, "local-import-started", ());
    drop(tauri::async_runtime::spawn_blocking(move || {
        let _claim = _claim;
        if let Err(error) = run_local_import(&app, &paths) {
            notify_error(&app, error);
            let _ = crate::emit_main(&app, "local-import-failed", ());
        }
    }));
}

struct LocalImportClaim(Arc<AtomicBool>);

impl LocalImportClaim {
    fn acquire(active: Arc<AtomicBool>) -> Option<Self> {
        active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            .then_some(Self(active))
    }
}

impl Drop for LocalImportClaim {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn format_import_failures(summary: &localfiles::ImportSummary) -> Option<String> {
    if summary.failure_count == 0 {
        return None;
    }
    let mut lines = summary
        .failed
        .iter()
        .take(5)
        .map(|failure| format!("{} — {}", failure.path, failure.reason))
        .collect::<Vec<_>>();
    if summary.failure_count > lines.len() {
        lines.push(format!("+ {} more", summary.failure_count - lines.len()));
    }
    Some(format!(
        "Some files could not be imported:\n{}",
        lines.join("\n")
    ))
}

#[tauri::command]
pub(super) async fn browse(
    app: tauri::AppHandle,
    source: SourceId,
    sel: SelectionDto,
    query: Option<String>,
) -> Result<BrowseView, String> {
    validate_browse_input(&sel, query.as_deref())?;
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        project_library(&state.library, |library| {
            browse_view(library, source, sel, query)
        })
    })
    .await
    .map_err(|error| error.to_string())
}

fn browse_view(
    library: &Library,
    source: SourceId,
    sel: SelectionDto,
    query: Option<String>,
) -> BrowseView {
    let mut selection = Selection::default();
    selection.select_cat(sel.cat);
    selection.select_art(sel.art);
    selection.select_alb(sel.alb);

    let facet_view = browse::facets(library, source, &selection);
    let query = query.unwrap_or_default().trim().to_lowercase();
    let selected_tracks = browse::tracks(library, source, &selection)
        .into_iter()
        .filter(|track| {
            query.is_empty()
                || [&track.name, &track.art, &track.alb, &track.cat]
                    .iter()
                    .any(|value| value.to_lowercase().contains(&query))
        })
        .collect::<Vec<_>>();
    let (album_rating, album_rating_artist, album_rating_ambiguous) =
        album_rating_view(library, &selection, &selected_tracks);
    let counts = counts(library, &selected_tracks);
    let tracks = selected_tracks
        .into_iter()
        .map(|track| TrackView::from_track(track, library.effective_rating(track.id)))
        .collect();

    BrowseView {
        facets: FacetsView {
            cats: facet_view.cats,
            arts: facet_view.arts,
            albs: facet_view.albs,
        },
        tracks,
        album_rating,
        album_rating_artist,
        album_rating_ambiguous,
        counts,
    }
}

#[tauri::command]
pub(super) async fn metadata_values(app: tauri::AppHandle) -> Result<MetadataValues, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        project_library(&state.library, collect_metadata_values)
    })
    .await
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub(super) async fn genre_values(app: tauri::AppHandle) -> Result<Vec<String>, String> {
    Ok(metadata_values(app).await?.cats)
}

#[tauri::command]
pub(super) async fn click_track_star(
    app: tauri::AppHandle,
    id: u64,
    stars: u8,
) -> Result<(), String> {
    let rating = Rating::new(stars).ok_or_else(|| "rating must be 1 through 5".to_string())?;
    tauri::async_runtime::spawn_blocking(move || {
        app.state::<AppState>().library.mutate(|library| {
            library
                .click_track_star(TrackId(id), rating)
                .map_err(|error| error.to_string())
        })
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub(super) async fn set_track_enabled(
    app: tauri::AppHandle,
    id: u64,
    enabled: bool,
) -> Result<(), String> {
    let mutation_app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        mutation_app.state::<AppState>().library.mutate(|library| {
            library
                .set_track_enabled(TrackId(id), enabled)
                .map_err(|error| error.to_string())
        })
    })
    .await
    .map_err(|error| error.to_string())??;
    let state = app.state::<AppState>();
    if !enabled {
        state.playback.exclude_track(id).await;
    }
    Ok(())
}

#[tauri::command]
pub(super) async fn set_album_rating(
    app: tauri::AppHandle,
    source: SourceId,
    art: String,
    alb: String,
    stars: Option<u8>,
) -> Result<(), String> {
    validate_metadata("Artist", &art)?;
    validate_metadata("Album", &alb)?;
    let rating = stars
        .map(|stars| Rating::new(stars).ok_or_else(|| "rating must be 1 through 5".to_string()))
        .transpose()?;
    tauri::async_runtime::spawn_blocking(move || {
        app.state::<AppState>().library.mutate(|library| {
            library.set_album_rating(AlbumKey { source, art, alb }, rating);
            Ok(())
        })
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub(super) async fn get_track(app: tauri::AppHandle, id: u64) -> Result<TrackInfoView, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let library = state.library.lock().expect("library mutex poisoned");
        let track = library
            .get(TrackId(id))
            .ok_or_else(|| format!("unknown track id {id}"))?;
        Ok(TrackInfoView::from_track(&library, track))
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub(super) async fn edit_track(
    app: tauri::AppHandle,
    id: u64,
    edit: TrackEditDto,
) -> Result<(), String> {
    validate_track_edit(&edit)?;
    let rating_change = rating_change(&edit)?;
    tauri::async_runtime::spawn_blocking(move || {
        app.state::<AppState>()
            .library
            .mutate(|library| apply_track_info(library, id, &edit, rating_change))
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub(super) async fn set_track_infos(
    app: tauri::AppHandle,
    ids: Vec<u64>,
    edit: TrackEditDto,
) -> Result<(), String> {
    validate_track_ids(&ids)?;
    validate_track_edit(&edit)?;
    let mutation_app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        mutation_app
            .state::<AppState>()
            .library
            .mutate(|library| apply_track_infos(library, &ids, &edit))
    })
    .await
    .map_err(|error| error.to_string())??;
    crate::emit_main(&app, "library-changed", ()).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use std::{sync::mpsc, time::Duration};

    use retune_core::model::NewTrack;

    use crate::{
        fixture,
        library_state::LibraryState,
        store::{FsOverlayStore, OverlayStore},
    };

    use super::*;

    #[test]
    fn local_import_claim_is_single_flight_and_releases_on_drop() {
        let active = Arc::new(AtomicBool::new(false));
        let claim = LocalImportClaim::acquire(Arc::clone(&active)).unwrap();
        assert!(LocalImportClaim::acquire(Arc::clone(&active)).is_none());
        drop(claim);
        assert!(LocalImportClaim::acquire(active).is_some());
    }

    #[test]
    fn slow_import_preparation_does_not_block_reads_or_lose_a_concurrent_write() {
        let directory = tempfile::tempdir().unwrap();
        let state = Arc::new(LibraryState::new(
            Library::new(),
            FsOverlayStore::new(directory.path()),
        ));
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker_state = Arc::clone(&state);
        let worker = std::thread::spawn(move || {
            prepare_and_commit_import(&worker_state, || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                localfiles::prepare_paths(Vec::<PathBuf>::new())
            })
            .unwrap()
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        assert_eq!(state.lock().unwrap().tracks().len(), 0);
        state
            .mutate(|library| {
                library.add(metadata_track(
                    "spotify:track:concurrent",
                    "Rock",
                    "Artist",
                    "Album",
                ));
                Ok(())
            })
            .unwrap();
        release_tx.send(()).unwrap();
        assert_eq!(worker.join().unwrap().imported, 0);
        assert_eq!(state.lock().unwrap().tracks().len(), 1);
    }

    #[test]
    fn delayed_projection_does_not_block_a_library_writer() {
        let directory = tempfile::tempdir().unwrap();
        let state = Arc::new(LibraryState::new(
            Library::new(),
            FsOverlayStore::new(directory.path()),
        ));
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let projection_state = Arc::clone(&state);
        let projection = std::thread::spawn(move || {
            project_library(&projection_state, |_| {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            });
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let (writer_done_tx, writer_done_rx) = mpsc::channel();
        let writer_state = Arc::clone(&state);
        let writer = std::thread::spawn(move || {
            writer_state
                .mutate(|library| {
                    library.add(metadata_track(
                        "spotify:track:writer",
                        "Rock",
                        "Artist",
                        "Album",
                    ));
                    Ok(())
                })
                .unwrap();
            writer_done_tx.send(()).unwrap();
        });
        writer_done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("a delayed projection must not retain the library guard");

        release_tx.send(()).unwrap();
        writer.join().unwrap();
        projection.join().unwrap();
        assert_eq!(state.snapshot().tracks().len(), 1);
    }

    fn metadata_track(uri: &str, cat: &str, art: &str, alb: &str) -> NewTrack {
        NewTrack {
            uri: uri.into(),
            cat: cat.into(),
            art: art.into(),
            alb: alb.into(),
            name: uri.into(),
            duration: Duration::from_secs(1),
            ..NewTrack::default()
        }
    }

    #[test]
    fn track_view_derives_local_state_from_uri() {
        let mut library = Library::new();
        let local = library.add(metadata_track(
            "file:///tmp/song.mp3",
            "Genre",
            "Artist",
            "Album",
        ));
        let spotify = library.add(metadata_track(
            "spotify:track:track",
            "Genre",
            "Artist",
            "Album",
        ));

        assert!(TrackView::from_track(library.get(local).unwrap(), None).is_local);
        assert!(!TrackView::from_track(library.get(spotify).unwrap(), None).is_local);
    }

    #[test]
    fn track_view_exposes_metadata_fields() {
        let mut library = Library::new();
        let id = library.add(metadata_track(
            "spotify:track:track",
            "Genre",
            "Artist",
            "Album",
        ));
        library.merge_history_absolute("spotify:track:track", Some(4), Some(100), Some(123));
        library
            .fill_missing_metadata(id, None, Some("Spotify".into()), None)
            .unwrap();
        let view = TrackView::from_track(library.get(id).unwrap(), None);

        assert_eq!((view.play_count, view.last_played_at), (4, Some(123)));
        assert_eq!(view.added_at, Some(100));
        assert_eq!(view.kind.as_deref(), Some("Spotify"));
        assert_eq!(view.bitrate_kbps, None);
    }

    #[test]
    fn track_info_view_decodes_only_local_file_paths() {
        let mut library = Library::new();
        let local_path = std::env::temp_dir().join("Rétune song.mp3");
        let local = library.add(metadata_track(
            &localfiles::file_uri(&local_path),
            "Genre",
            "Artist",
            "Album",
        ));
        let spotify = library.add(metadata_track(
            "spotify:track:track",
            "Genre",
            "Artist",
            "Album",
        ));

        assert_eq!(
            TrackInfoView::from_track(&library, library.get(local).unwrap()).local_path,
            Some(local_path.to_string_lossy().into_owned())
        );
        assert_eq!(
            TrackInfoView::from_track(&library, library.get(spotify).unwrap()).local_path,
            None
        );
    }

    #[test]
    fn import_failure_message_is_bounded() {
        let failures = (1..=7)
            .map(|index| localfiles::FailedImport {
                path: format!("file-{index}"),
                reason: "bad".into(),
            })
            .collect::<Vec<_>>();

        let summary = localfiles::ImportSummary {
            failure_count: failures.len(),
            failed: failures,
            ..localfiles::ImportSummary::default()
        };
        let message = format_import_failures(&summary).unwrap();
        assert!(message.contains("file-1 — bad"));
        assert!(message.contains("file-5 — bad"));
        assert!(!message.contains("file-6 — bad"));
        assert!(message.ends_with("+ 2 more"));
        assert_eq!(
            format_import_failures(&localfiles::ImportSummary::default()),
            None
        );
    }

    #[test]
    fn metadata_values_are_distinct_sorted_and_categorized() {
        let mut library = Library::new();
        library.add(metadata_track("one", "rock", "zebra", "Yellow"));
        library.add(metadata_track("two", "Jazz", "Alpha", "beta"));
        library.add(metadata_track("three", UNCATEGORIZED, "Alpha", "Yellow"));
        library.add(metadata_track("empty", "", "", ""));

        let values = collect_metadata_values(&library);

        assert_eq!(values.arts, ["Alpha", "zebra"]);
        assert_eq!(values.albs, ["beta", "Yellow"]);
        assert_eq!(values.cats, ["Jazz", "rock"]);
    }

    #[test]
    fn fixture_counts_cover_visible_tracks_and_global_overlay_edits() {
        let library = fixture::library();
        let selection = Selection::default();
        let all_tracks = browse::tracks(&library, SourceId::Music, &selection);
        let all = counts(&library, &all_tracks);
        assert_eq!(all.tracks, 26);
        assert_eq!(all.per_source.music, 26);
        assert_eq!(all.per_source.podcasts, 4);
        assert_eq!(all.per_source.audiobooks, 3);
        assert_eq!(all.overlay_edits, 5);

        let filtered_tracks = all_tracks
            .into_iter()
            .filter(|track| track.name.to_lowercase().contains("bohemian"))
            .collect::<Vec<_>>();
        let filtered = counts(&library, &filtered_tracks);
        assert_eq!(filtered.tracks, 1);
    }

    #[test]
    fn batch_edit_applies_category_and_preserves_each_original() {
        let mut library = fixture::library();
        let ids = library
            .tracks()
            .iter()
            .take(2)
            .map(|track| track.id.0)
            .collect::<Vec<_>>();

        apply_track_infos(
            &mut library,
            &ids,
            &TrackEditDto {
                cat: Some("Personal".into()),
                ..TrackEditDto::default()
            },
        )
        .unwrap();

        for id in ids {
            let track = library.get(TrackId(id)).unwrap();
            assert_eq!(track.cat, "Personal");
            assert_eq!(track.orig_cat.as_deref(), Some("Rock"));
        }
    }

    #[test]
    fn batch_edit_without_rating_change_leaves_ratings_unchanged() {
        let mut library = fixture::library();
        let ids = library
            .tracks()
            .iter()
            .take(2)
            .map(|track| track.id.0)
            .collect::<Vec<_>>();
        library
            .set_track_rating(TrackId(ids[0]), Rating::new(2))
            .unwrap();
        library
            .set_track_rating(TrackId(ids[1]), Rating::new(5))
            .unwrap();

        apply_track_infos(
            &mut library,
            &ids,
            &TrackEditDto {
                art: Some("Various Artists".into()),
                ..TrackEditDto::default()
            },
        )
        .unwrap();

        assert_eq!(library.get(TrackId(ids[0])).unwrap().rating, Rating::new(2));
        assert_eq!(library.get(TrackId(ids[1])).unwrap().rating, Rating::new(5));
    }

    #[test]
    fn batch_edit_with_unknown_id_is_atomic() {
        let mut library = fixture::library();
        let before = library.clone();
        let known = library.tracks()[0].id.0;

        let error = apply_track_infos(
            &mut library,
            &[known, u64::MAX],
            &TrackEditDto {
                cat: Some("Personal".into()),
                ..TrackEditDto::default()
            },
        )
        .unwrap_err();

        assert!(error.contains(&u64::MAX.to_string()));
        assert_eq!(library, before);
    }

    #[test]
    fn batch_edit_rejects_empty_ids() {
        let mut library = fixture::library();
        let before = library.clone();

        assert!(apply_track_infos(&mut library, &[], &TrackEditDto::default()).is_err());
        assert_eq!(library, before);
    }

    #[test]
    fn library_edit_limits_accept_exact_and_reject_one_over_without_store_effects() {
        let directory = tempfile::tempdir().unwrap();
        let store = FsOverlayStore::new(directory.path());
        let mut initial = Library::new();
        let id = initial.add(metadata_track(
            "spotify:track:bounded",
            "Rock",
            "Artist",
            "Album",
        ));
        store.save(&initial).unwrap();
        let state = LibraryState::new(initial.clone(), FsOverlayStore::new(directory.path()));

        state
            .mutate(|library| {
                apply_track_infos(
                    library,
                    &vec![id.0; MAX_LIBRARY_BATCH_IDS],
                    &TrackEditDto {
                        name: Some("n".repeat(MAX_LIBRARY_METADATA_BYTES)),
                        ..TrackEditDto::default()
                    },
                )
            })
            .unwrap();
        assert_eq!(
            state.lock().unwrap().get(id).unwrap().name.len(),
            MAX_LIBRARY_METADATA_BYTES
        );
        let accepted = state.lock().unwrap().clone();

        for (ids, edit) in [
            (
                vec![id.0],
                TrackEditDto {
                    cat: Some("g".repeat(MAX_LIBRARY_METADATA_BYTES + 1)),
                    ..TrackEditDto::default()
                },
            ),
            (
                vec![id.0; MAX_LIBRARY_BATCH_IDS + 1],
                TrackEditDto {
                    cat: Some("Jazz".into()),
                    ..TrackEditDto::default()
                },
            ),
        ] {
            assert!(state
                .mutate(|library| apply_track_infos(library, &ids, &edit))
                .is_err());
            assert_eq!(*state.lock().unwrap(), accepted);
            assert_eq!(store.load().unwrap(), Some(accepted.clone()));
        }
    }

    #[test]
    fn album_rating_is_ambiguous_for_same_named_albums_by_different_artists() {
        let mut library = fixture::library();
        let fleetwood_track = library
            .tracks()
            .iter()
            .find(|track| track.art == "Fleetwood Mac")
            .expect("fixture has Fleetwood Mac")
            .id;
        library
            .edit(
                fleetwood_track,
                TrackEdit {
                    alb: Some("Hotel California".into()),
                    ..TrackEdit::default()
                },
            )
            .expect("fixture track exists");
        let mut selection = Selection::default();
        selection.select_alb(vec!["Hotel California".into()]);
        let tracks = browse::tracks(&library, SourceId::Music, &selection);

        assert_eq!(
            album_rating_view(&library, &selection, &tracks),
            (None, None, true)
        );
    }
}

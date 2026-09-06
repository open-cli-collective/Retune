use std::{
    collections::{HashMap, HashSet},
    hash::{DefaultHasher, Hash, Hasher},
};

use retune_core::model::{
    EffectiveRating, Library, MergePlayCount, NewTrack, Rating, TrackEdit, TrackId,
    TrackMergeOptions, TrackMergeTarget, TrackRecord,
};
use serde::{Deserialize, Serialize};
use tauri::Manager;

use crate::{store::SpotifyLibraryState, AppState};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LibrarySources {
    pub saved_track: Option<bool>,
    pub saved_albums: Vec<String>,
    pub whole_album: bool,
    pub membership_known: bool,
    pub retained: bool,
    pub local_file: bool,
}

pub(super) fn library_sources(
    library: &Library,
    membership: &SpotifyLibraryState,
    uri: &str,
) -> LibrarySources {
    let local_file = uri.starts_with("file:");
    LibrarySources {
        saved_track: if !uri.starts_with("spotify:track:") {
            Some(false)
        } else if membership.saved_tracks.contains_key(uri) {
            Some(true)
        } else {
            membership.is_exact().then_some(false)
        },
        saved_albums: membership
            .saved_albums
            .values()
            .filter(|album| album.track_uris.iter().any(|track| track == uri))
            .map(|album| album.name.clone())
            .collect(),
        whole_album: membership.saved_albums.values().any(|album| {
            album.album_type.as_deref() == Some("album")
                && album.track_uris.iter().any(|track| track == uri)
        }),
        membership_known: local_file || membership.is_exact(),
        retained: library.is_retained(uri),
        local_file,
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DecisionTrack {
    id: Option<u64>,
    uri: String,
    name: String,
    art: String,
    alb: String,
    cat: String,
    rating: Option<u8>,
    play_count: u32,
    added_at: Option<u64>,
    last_played_at: Option<u64>,
    duration_secs: u64,
    enabled: bool,
    sources: LibrarySources,
}

impl DecisionTrack {
    pub(super) fn from_track(
        library: &Library,
        membership: &SpotifyLibraryState,
        track: &TrackRecord,
    ) -> Self {
        Self {
            id: Some(track.id.0),
            uri: track.uri.clone(),
            name: track.name.clone(),
            art: track.art.clone(),
            alb: track.alb.clone(),
            cat: track.cat.clone(),
            rating: library.effective_rating(track).map(|rating| match rating {
                EffectiveRating::Explicit(r) | EffectiveRating::Inherited(r) => r.stars(),
            }),
            play_count: track.play_count,
            added_at: track.added_at,
            last_played_at: track.last_played_at,
            duration_secs: track.duration.as_secs(),
            enabled: track.enabled,
            sources: library_sources(library, membership, &track.uri),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TrackMergePreview {
    tracks: Vec<DecisionTrack>,
    target: Option<DecisionTrack>,
    revision: String,
}

fn selected_tracks<'a>(library: &'a Library, ids: &[u64]) -> Result<Vec<&'a TrackRecord>, String> {
    crate::library_commands::validate_track_ids(ids)?;
    if ids.len() < 2 || ids.iter().collect::<HashSet<_>>().len() != ids.len() {
        return Err("Select at least two distinct tracks to merge.".into());
    }
    let by_id = library
        .tracks()
        .iter()
        .map(|track| (track.id.0, track))
        .collect::<HashMap<_, _>>();
    let tracks = ids
        .iter()
        .map(|id| {
            by_id.get(id).copied().ok_or_else(|| {
                "A selected track is no longer in the library. Reopen the merge dialog.".to_string()
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if tracks.iter().any(|track| track.source != tracks[0].source) {
        return Err("Choose tracks of the same media type.".into());
    }
    Ok(tracks)
}

fn revision(library: &Library, tracks: &[&TrackRecord], target: Option<&TrackRecord>) -> String {
    let mut hasher = DefaultHasher::new();
    for track in tracks.iter().copied().chain(target) {
        serde_json::to_vec(track)
            .expect("track serialization")
            .hash(&mut hasher);
        serde_json::to_vec(&library.album_rating(&retune_core::model::AlbumKey::of(track)))
            .expect("rating serialization")
            .hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

async fn external_target(
    app: &tauri::AppHandle,
    uri: Option<&str>,
) -> Result<Option<NewTrack>, String> {
    let Some(uri) = uri else { return Ok(None) };
    if uri.len() > 2048 {
        return Err("Recording URI is too long.".into());
    }
    let state = app.state::<AppState>();
    let library = state.library.snapshot();
    if library.get_by_uri(uri).is_some() {
        return Ok(None);
    }
    if library.known_tracks().any(|track| track.uri == uri) {
        return Err("Restore this recording to Retune before choosing it for a merge.".into());
    }
    let id = crate::track_id(uri).ok_or("Choose an existing library entry or a Spotify track.")?;
    let track = crate::provider_from(&state)?
        .track(id)
        .await
        .map_err(|error| {
            crate::spotify_membership::spotify_action_error(&state.cooldown_store, error)
                .into_message()
        })?;
    Ok(Some(retune_spotify::normalize::track(&track, None, None)))
}

#[tauri::command]
pub(super) async fn get_track_merge(
    app: tauri::AppHandle,
    ids: Vec<u64>,
    target_uri: Option<String>,
) -> Result<TrackMergePreview, String> {
    crate::library_commands::validate_track_ids(&ids)?;
    let external = external_target(&app, target_uri.as_deref()).await?;
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let membership = state.spotify_membership.snapshot();
        let library = state.library.snapshot();
        let tracks = selected_tracks(&library, &ids)?;
        let mut temporary = Library::new();
        let external_id = external.map(|track| temporary.add(track));
        let target = if let Some(uri) = target_uri.as_deref() {
            library
                .get_by_uri(uri)
                .or_else(|| external_id.and_then(|id| temporary.get(id)))
        } else {
            tracks.iter().copied().max_by_key(|track| {
                (
                    library_sources(&library, &membership, &track.uri).whole_album,
                    track.play_count,
                )
            })
        };
        let revision = revision(&library, &tracks, target);
        let target = target.map(|track| {
            let mut view = DecisionTrack::from_track(&library, &membership, track);
            if library.get_by_uri(&track.uri).is_none() {
                view.id = None;
            }
            view
        });
        Ok(TrackMergePreview {
            tracks: tracks
                .into_iter()
                .map(|track| DecisionTrack::from_track(&library, &membership, track))
                .collect(),
            target,
            revision,
        })
    })
    .await
    .map_err(|error| error.to_string())?
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MergeEdit {
    name: String,
    art: String,
    alb: String,
    cat: String,
    rating: Option<u8>,
    play_count: MergePlayCount,
}

fn apply_merge(
    library: &mut Library,
    ids: &[u64],
    target_uri: &str,
    external: Option<NewTrack>,
    edit: MergeEdit,
    expected_revision: &str,
) -> Result<u64, String> {
    let tracks = selected_tracks(library, ids)?;
    let mut temporary = Library::new();
    let external_id = external.clone().map(|track| temporary.add(track));
    let target = library
        .get_by_uri(target_uri)
        .or_else(|| external_id.and_then(|id| temporary.get(id)))
        .ok_or("The chosen recording is no longer available.")?;
    if revision(library, &tracks, Some(target)) != expected_revision {
        return Err("These tracks changed while the dialog was open. Reload the merge preview and review the updated history.".into());
    }
    let target = if let Some(track) = library.get_by_uri(target_uri) {
        TrackMergeTarget::Existing(track.id)
    } else {
        TrackMergeTarget::New(Box::new(
            external.ok_or("The chosen recording is no longer available.")?,
        ))
    };
    let rating = edit
        .rating
        .map(|stars| Rating::new(stars).ok_or("Rating must be 1 through 5."))
        .transpose()?;
    for (label, value) in [
        ("Title", &edit.name),
        ("Artist", &edit.art),
        ("Album", &edit.alb),
        ("Genre", &edit.cat),
    ] {
        crate::library_commands::validate_metadata(label, value)?;
    }
    let ids = ids.iter().map(|id| TrackId(*id)).collect::<Vec<_>>();
    library
        .merge_tracks(
            &ids,
            target,
            TrackMergeOptions {
                edit: TrackEdit {
                    name: Some(edit.name),
                    art: Some(edit.art),
                    alb: Some(edit.alb),
                    cat: Some(edit.cat),
                },
                rating,
                play_count: edit.play_count,
                merged_at: crate::unix_now(),
            },
        )
        .map(|id| id.0)
}

#[tauri::command]
pub(super) async fn merge_library_tracks(
    app: tauri::AppHandle,
    ids: Vec<u64>,
    target_uri: String,
    edit: MergeEdit,
    expected_revision: String,
) -> Result<u64, String> {
    crate::library_commands::validate_track_ids(&ids)?;
    let external = external_target(&app, Some(&target_uri)).await?;
    let mutation_app = app.clone();
    let merged_ids = ids.clone();
    let id = tauri::async_runtime::spawn_blocking(move || {
        mutation_app.state::<AppState>().library.mutate(|library| {
            apply_merge(
                library,
                &ids,
                &target_uri,
                external,
                edit,
                &expected_revision,
            )
        })
    })
    .await
    .map_err(|error| error.to_string())??;
    for original in merged_ids.into_iter().filter(|original| *original != id) {
        app.state::<AppState>()
            .playback
            .exclude_track(original)
            .await;
    }
    crate::emit_main(&app, "library-changed", ()).map_err(|error| error.to_string())?;
    Ok(id)
}

#[tauri::command]
pub(super) async fn undo_track_merge(app: tauri::AppHandle, id: u64) -> Result<(), String> {
    let mutation_app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        mutation_app
            .state::<AppState>()
            .library
            .mutate(|library| library.undo_track_merge(TrackId(id)).map(|_| ()))
    })
    .await
    .map_err(|error| error.to_string())??;
    crate::emit_main(&app, "library-changed", ()).map_err(|error| error.to_string())
}

#[tauri::command]
pub(super) async fn remove_retune_tracks(
    app: tauri::AppHandle,
    ids: Vec<u64>,
) -> Result<(), String> {
    crate::library_commands::validate_track_ids(&ids)?;
    let mutation_app = app.clone();
    let removed_ids = ids.clone();
    tauri::async_runtime::spawn_blocking(move || {
        mutation_app.state::<AppState>().library.mutate(|library| {
            library.remove_tracks(&ids.into_iter().map(TrackId).collect::<Vec<_>>())
        })
    })
    .await
    .map_err(|error| error.to_string())??;
    for id in removed_ids {
        app.state::<AppState>().playback.exclude_track(id).await;
    }
    crate::emit_main(&app, "library-changed", ()).map_err(|error| error.to_string())
}

#[tauri::command]
pub(super) async fn removed_retune_tracks(
    app: tauri::AppHandle,
) -> Result<Vec<DecisionTrack>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let membership = state.spotify_membership.snapshot();
        let library = state.library.snapshot();
        library
            .removed_tracks()
            .iter()
            .map(|track| DecisionTrack::from_track(&library, &membership, track))
            .collect()
    })
    .await
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub(super) async fn restore_retune_track(
    app: tauri::AppHandle,
    uri: String,
) -> Result<u64, String> {
    if uri.len() > 2048 {
        return Err("Recording URI is too long.".into());
    }
    let mutation_app = app.clone();
    let id = tauri::async_runtime::spawn_blocking(move || {
        mutation_app.state::<AppState>().library.mutate(|library| {
            library
                .restore_track(&uri)
                .or_else(|| library.get_by_uri(&uri).map(|track| track.id))
                .map(|id| id.0)
                .ok_or_else(|| "This track is no longer in Removed tracks.".to_string())
        })
    })
    .await
    .map_err(|error| error.to_string())??;
    crate::emit_main(&app, "library-changed", ()).map_err(|error| error.to_string())?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        library_state::LibraryState,
        store::{FsOverlayStore, OverlayStore, SavedAlbumRecord},
        sync,
    };

    fn edit() -> MergeEdit {
        MergeEdit {
            name: "Chosen title".into(),
            art: "Artist".into(),
            alb: "Album".into(),
            cat: "Rock".into(),
            rating: Some(4),
            play_count: MergePlayCount::Highest,
        }
    }
    fn fixture() -> (Library, Vec<u64>) {
        let mut library = Library::new();
        let ids = ["one", "two", "three"]
            .map(|name| {
                library
                    .add(NewTrack {
                        uri: format!("spotify:track:{name}"),
                        name: name.into(),
                        art: "Artist".into(),
                        alb: "Album".into(),
                        ..NewTrack::default()
                    })
                    .0
            })
            .to_vec();
        library.merge_history_absolute("spotify:track:one", Some(114), Some(10), Some(20));
        library.merge_history_absolute("spotify:track:three", Some(18), Some(5), Some(30));
        (library, ids)
    }
    fn token(library: &Library, ids: &[u64]) -> String {
        revision(
            library,
            &selected_tracks(library, ids).unwrap(),
            library.get_by_uri("spotify:track:one"),
        )
    }

    #[test]
    fn merge_rejects_stale_history_and_failed_save_leaves_live_and_durable_library_unchanged() {
        let (mut library, ids) = fixture();
        let stale = token(&library, &ids);
        library.record_play("spotify:track:one", 40);
        let unchanged = library.clone();
        assert!(apply_merge(
            &mut library,
            &ids,
            "spotify:track:one",
            None,
            edit(),
            &stale
        )
        .unwrap_err()
        .contains("changed"));
        assert_eq!(library, unchanged);
        let directory = tempfile::tempdir().unwrap();
        let store = FsOverlayStore::new(directory.path());
        store.save(&library).unwrap();
        let blocked = directory.path().join("blocked");
        std::fs::write(&blocked, b"not a directory").unwrap();
        let state = LibraryState::new(library.clone(), FsOverlayStore::new(&blocked));
        let current = token(&library, &ids);
        // A blocked destination proves that the owning transaction never
        // publishes a merge before its durable overlay has been saved.
        assert!(state
            .mutate(|library| apply_merge(
                library,
                &ids,
                "spotify:track:one",
                None,
                edit(),
                &current
            ))
            .is_err());
        assert_eq!(state.snapshot(), unchanged);
        assert_eq!(store.load().unwrap(), Some(unchanged));
    }

    #[test]
    fn sync_and_explicit_unsave_respect_merged_and_hidden_tracks() {
        let (mut library, ids) = fixture();
        let current = token(&library, &ids);
        apply_merge(
            &mut library,
            &ids,
            "spotify:track:one",
            None,
            edit(),
            &current,
        )
        .unwrap();
        let mut membership = SpotifyLibraryState {
            account_id: "account".into(),
            complete: true,
            ..Default::default()
        };
        membership.saved_albums.insert(
            "spotify:album:album".into(),
            SavedAlbumRecord {
                uri: "spotify:album:album".into(),
                name: "Album".into(),
                artists: vec!["Artist".into()],
                release_date: None,
                album_type: Some("album".into()),
                added_at: None,
                track_uris: vec!["spotify:track:two".into()],
            },
        );
        let incoming = vec![NewTrack {
            uri: "spotify:track:two".into(),
            name: "Provider title".into(),
            ..NewTrack::default()
        }];
        library =
            sync::candidate_from_snapshot(&library, false, incoming.clone(), Some(&membership))
                .unwrap();
        assert_eq!(library.tracks().len(), 1);
        assert_eq!(library.tracks()[0].play_count, 114);
        assert_eq!(library.tracks()[0].name, "Chosen title");
        crate::spotify_membership::apply_track_removal(
            &mut library,
            &membership,
            "spotify:track:one",
        )
        .unwrap();
        assert!(!library.tracks()[0].enabled);
        membership.saved_albums.clear();
        crate::spotify_membership::apply_track_removal(
            &mut library,
            &membership,
            "spotify:track:one",
        )
        .unwrap();
        assert!(library.tracks().is_empty());
        assert_eq!(library.removed_tracks()[0].play_count, 114);
        library =
            sync::candidate_from_snapshot(&library, false, incoming, Some(&membership)).unwrap();
        assert!(library.tracks().is_empty());
        assert_eq!(
            crate::spotify_membership::apply_track_removal(
                &mut library,
                &membership,
                "spotify:track:one"
            )
            .unwrap(),
            None
        );
        let mut library = retune_core::io::import(&retune_core::io::export_json(&library)).unwrap();
        library.restore_track("spotify:track:two").unwrap();
        library.undo_track_merge(TrackId(ids[0])).unwrap();
        assert_eq!(library.tracks().len(), 3);
    }

    #[test]
    fn an_unsave_with_incomplete_local_finalization_cannot_be_pruned_by_sync() {
        let (mut library, ids) = fixture();
        crate::spotify_membership::preserve_track_before_removal(&mut library, "spotify:track:one");
        let mut library = retune_core::io::import(&retune_core::io::export_json(&library)).unwrap();
        let membership = SpotifyLibraryState {
            account_id: "account".into(),
            complete: true,
            ..Default::default()
        };
        library =
            sync::candidate_from_snapshot(&library, false, vec![], Some(&membership)).unwrap();
        assert_eq!(library.get(TrackId(ids[0])).unwrap().play_count, 114);
        crate::spotify_membership::apply_track_removal(
            &mut library,
            &membership,
            "spotify:track:one",
        )
        .unwrap();
        assert!(library.tracks().is_empty());
        assert_eq!(library.removed_tracks()[0].play_count, 114);
    }

    #[test]
    fn sources_separate_saved_tracks_albums_files_and_unknown_membership() {
        let (library, _) = fixture();
        let mut membership = SpotifyLibraryState::default();
        assert_eq!(
            library_sources(&library, &membership, "spotify:track:one").saved_track,
            None
        );
        membership.add_saved_track("spotify:track:one".into(), None);
        assert_eq!(
            library_sources(&library, &membership, "spotify:track:one").saved_track,
            Some(true)
        );
        assert!(library_sources(&library, &membership, "file:///song.mp3").local_file);
    }
}

mod fixture;
mod store;

use std::{collections::BTreeSet, fs, sync::Mutex};

use retune_core::{
    browse::{self, Selection},
    io::{export_json, export_json_gz, import},
    model::{AlbumKey, EffectiveRating, Library, Rating, SourceId, TrackEdit, TrackId},
};
use serde::{Deserialize, Serialize};
use store::{FsOverlayStore, OverlayStore, StoreError};
use tauri::{
    menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder},
    Emitter, Manager,
};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};

struct AppState {
    library: Mutex<Library>,
    store: FsOverlayStore,
    recovery_notice: Mutex<Option<String>>,
}

#[derive(Deserialize)]
struct SelectionDto {
    cat: Option<String>,
    art: Option<String>,
    alb: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowseView {
    facets: FacetsView,
    tracks: Vec<TrackView>,
    album_rating: Option<u8>,
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
struct TrackView {
    id: u64,
    name: String,
    art: String,
    alb: String,
    cat: String,
    duration_secs: u64,
    overridden: bool,
    rating: Option<RatingView>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TrackInfoView {
    id: u64,
    uri: String,
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

#[derive(Deserialize)]
struct TrackEditDto {
    name: Option<String>,
    art: Option<String>,
    alb: Option<String>,
    cat: Option<String>,
}

#[derive(Serialize)]
struct RatingView {
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

#[tauri::command]
fn browse(
    state: tauri::State<'_, AppState>,
    source: SourceId,
    sel: SelectionDto,
    query: Option<String>,
) -> BrowseView {
    let library = state.library.lock().expect("library mutex poisoned");
    let mut selection = Selection::default();
    selection.select_cat(sel.cat);
    selection.select_art(sel.art);
    selection.select_alb(sel.alb);

    let facet_view = browse::facets(&library, source, &selection);
    let query = query.unwrap_or_default().trim().to_lowercase();
    let selected_tracks = browse::tracks(&library, source, &selection);
    let album_rating = selection.alb().and_then(|alb| {
        let art = selection
            .art()
            .or_else(|| selected_tracks.first().map(|track| track.art.as_str()))?;
        library.album_rating(&AlbumKey {
            source,
            art: art.into(),
            alb: alb.into(),
        })
    });
    let tracks = selected_tracks
        .into_iter()
        .filter(|track| {
            query.is_empty()
                || [&track.name, &track.art, &track.alb, &track.cat]
                    .iter()
                    .any(|value| value.to_lowercase().contains(&query))
        })
        .map(|track| TrackView {
            id: track.id.0,
            name: track.name.clone(),
            art: track.art.clone(),
            alb: track.alb.clone(),
            cat: track.cat.clone(),
            duration_secs: track.duration.as_secs(),
            overridden: track
                .orig_cat
                .as_ref()
                .is_some_and(|original| original != &track.cat),
            rating: library
                .effective_rating(track.id)
                .map(|rating| match rating {
                    EffectiveRating::Explicit(rating) => RatingView {
                        stars: rating.stars(),
                        explicit: true,
                    },
                    EffectiveRating::Inherited(rating) => RatingView {
                        stars: rating.stars(),
                        explicit: false,
                    },
                }),
        })
        .collect();

    BrowseView {
        facets: FacetsView {
            cats: facet_view.cats,
            arts: facet_view.arts,
            albs: facet_view.albs,
        },
        tracks,
        album_rating: album_rating.map(Rating::stars),
        counts: counts(&library, source, &selection, &query),
    }
}

fn counts(library: &Library, source: SourceId, selection: &Selection, query: &str) -> CountsView {
    let visible = browse::tracks(library, source, selection)
        .into_iter()
        .filter(|track| {
            query.is_empty()
                || [&track.name, &track.art, &track.alb, &track.cat]
                    .iter()
                    .any(|value| value.to_lowercase().contains(query))
        })
        .collect::<Vec<_>>();
    let source_count = |source| {
        library
            .tracks()
            .iter()
            .filter(|track| track.source == source)
            .count()
    };
    let albums = library
        .tracks()
        .iter()
        .map(AlbumKey::of)
        .collect::<BTreeSet<_>>();
    let overlay_edits = library
        .tracks()
        .iter()
        .filter(|track| track.rating.is_some())
        .count()
        + library
            .tracks()
            .iter()
            .filter(|track| {
                track
                    .orig_cat
                    .as_ref()
                    .is_some_and(|original| original != &track.cat)
            })
            .count()
        + albums
            .iter()
            .filter(|album| library.album_rating(album).is_some())
            .count();

    CountsView {
        tracks: visible.len(),
        total_secs: visible.iter().map(|track| track.duration.as_secs()).sum(),
        overlay_edits,
        per_source: PerSourceView {
            music: source_count(SourceId::Music),
            podcasts: source_count(SourceId::Podcasts),
            audiobooks: source_count(SourceId::Audiobooks),
        },
    }
}

#[tauri::command]
fn click_track_star(state: tauri::State<'_, AppState>, id: u64, stars: u8) -> Result<(), String> {
    let rating = Rating::new(stars).ok_or_else(|| "rating must be 1 through 5".to_string())?;
    mutate_library(&state, |library| {
        library
            .click_track_star(TrackId(id), rating)
            .map_err(|error| error.to_string())
    })
}

#[tauri::command]
fn set_album_rating(
    state: tauri::State<'_, AppState>,
    source: SourceId,
    art: String,
    alb: String,
    stars: Option<u8>,
) -> Result<(), String> {
    let rating = stars
        .map(|stars| Rating::new(stars).ok_or_else(|| "rating must be 1 through 5".to_string()))
        .transpose()?;
    mutate_library(&state, |library| {
        library.set_album_rating(AlbumKey { source, art, alb }, rating);
        Ok(())
    })
}

#[tauri::command]
fn get_track(state: tauri::State<'_, AppState>, id: u64) -> Result<TrackInfoView, String> {
    let library = state.library.lock().expect("library mutex poisoned");
    let track = library
        .get(TrackId(id))
        .ok_or_else(|| format!("unknown track id {id}"))?;
    let rating = library.effective_rating(track.id).map(rating_view);
    let inherited_rating = library
        .album_rating(&AlbumKey::of(track))
        .map(Rating::stars);
    let genres = library
        .tracks()
        .iter()
        .filter(|candidate| candidate.source == track.source)
        .map(|candidate| candidate.cat.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(TrackInfoView {
        id,
        uri: track.uri.clone(),
        source: track.source,
        name: track.name.clone(),
        art: track.art.clone(),
        alb: track.alb.clone(),
        cat: track.cat.clone(),
        orig_cat: track.orig_cat.clone(),
        rating,
        inherited_rating,
        genres,
    })
}

fn rating_view(rating: EffectiveRating) -> RatingView {
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

#[tauri::command]
fn edit_track(
    state: tauri::State<'_, AppState>,
    id: u64,
    edit: TrackEditDto,
) -> Result<(), String> {
    mutate_library(&state, |library| {
        library
            .edit(
                TrackId(id),
                TrackEdit {
                    name: edit.name,
                    art: edit.art,
                    alb: edit.alb,
                    cat: edit.cat,
                },
            )
            .map_err(|error| error.to_string())
    })
}

#[tauri::command]
fn startup_notice(state: tauri::State<'_, AppState>) -> Option<String> {
    state
        .recovery_notice
        .lock()
        .expect("notice mutex poisoned")
        .take()
}

fn mutate_library<T>(
    state: &AppState,
    mutation: impl FnOnce(&mut Library) -> Result<T, String>,
) -> Result<T, String> {
    let mut current = state.library.lock().expect("library mutex poisoned");
    let mut next = current.clone();
    let value = mutation(&mut next)?;
    state.store.save(&next).map_err(|error| error.to_string())?;
    *current = next;
    Ok(value)
}

fn install_file_menu(app: &tauri::App) -> tauri::Result<()> {
    let get_info = MenuItemBuilder::with_id("get_info", "Get Info")
        .accelerator("CmdOrCtrl+I")
        .build(app)?;
    let file = SubmenuBuilder::new(app, "File")
        .item(&get_info)
        .separator()
        .text("backup_library", "Back Up Library…")
        .text("export_library", "Export Library…")
        .separator()
        .text("restore_library", "Restore Library…")
        .text("merge_library", "Merge Library…")
        .build()?;
    let menu = MenuBuilder::new(app).item(&file).build()?;
    app.set_menu(menu)?;
    app.on_menu_event(|app, event| match event.id().as_ref() {
        "get_info" => {
            let _ = app.emit("get-info", ());
        }
        "backup_library" => export_library(app, false),
        "export_library" => export_library(app, true),
        "restore_library" => import_library(app, true),
        "merge_library" => import_library(app, false),
        _ => {}
    });
    Ok(())
}

fn export_library(app: &tauri::AppHandle, compressed: bool) {
    let handle = app.clone();
    let (name, extensions) = if compressed {
        ("Retune Library.json.gz", &["json.gz"] as &[_])
    } else {
        ("Retune Library.json", &["json"] as &[_])
    };
    app.dialog()
        .file()
        .set_file_name(name)
        .add_filter("Retune Library", extensions)
        .save_file(move |path| {
            let Some(path) = path else { return };
            let result = (|| -> Result<(), String> {
                let path = path.into_path().map_err(|error| error.to_string())?;
                let state = handle.state::<AppState>();
                let library = state.library.lock().expect("library mutex poisoned");
                let bytes = if compressed {
                    export_json_gz(&library)
                } else {
                    export_json(&library)
                };
                fs::write(path, bytes).map_err(|error| error.to_string())
            })();
            if let Err(error) = result {
                notify_error(&handle, error);
            }
        });
}

fn import_library(app: &tauri::AppHandle, replace: bool) {
    let handle = app.clone();
    app.dialog()
        .file()
        .add_filter("Retune Library", &["json", "json.gz", "gz"])
        .pick_file(move |path| {
            let Some(path) = path else { return };
            let result = (|| -> Result<Library, String> {
                let path = path.into_path().map_err(|error| error.to_string())?;
                let bytes = fs::read(path).map_err(|error| error.to_string())?;
                import(&bytes).map_err(|error| error.to_string())
            })();
            match result {
                Ok(library) if replace => {
                    let confirmed_handle = handle.clone();
                    handle
                        .dialog()
                        .message("Replace your library? This cannot be undone.")
                        .buttons(MessageDialogButtons::OkCancelCustom(
                            "Replace".into(),
                            "Cancel".into(),
                        ))
                        .show(move |confirmed| {
                            if confirmed {
                                apply_import(&confirmed_handle, library, true);
                            }
                        });
                }
                Ok(library) => apply_import(&handle, library, false),
                Err(error) => notify_error(&handle, error),
            }
        });
}

fn apply_import(app: &tauri::AppHandle, imported: Library, replace: bool) {
    let state = app.state::<AppState>();
    let result = mutate_library(&state, |library| {
        if replace {
            library.restore(imported);
        } else {
            library.merge(imported);
        }
        Ok(())
    });
    match result {
        Ok(()) => {
            let _ = app.emit("library-changed", ());
        }
        Err(error) => notify_error(app, error),
    }
}

fn notify_error(app: &tauri::AppHandle, error: String) {
    let _ = app.emit("operation-error", error);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            browse,
            click_track_star,
            set_album_rating,
            get_track,
            edit_track,
            startup_notice
        ])
        .setup(|app| {
            let store = FsOverlayStore::new(app.path().app_data_dir()?);
            let (library, recovery_notice) = match store.load() {
                Ok(Some(library)) => (library, None),
                Ok(None) => {
                    let library = fixture::library();
                    store.save(&library)?;
                    (library, None)
                }
                Err(StoreError::Import(error)) => {
                    let corrupt = store.quarantine_corrupt()?;
                    let library = Library::new();
                    store.save(&library)?;
                    (
                        library,
                        Some(format!(
                            "Retune could not load your library ({error}). The corrupt file was moved to {} and an empty library was started.",
                            corrupt.display()
                        )),
                    )
                }
                Err(error) => return Err(error.into()),
            };
            app.manage(AppState {
                library: Mutex::new(library),
                store,
                recovery_notice: Mutex::new(recovery_notice),
            });
            install_file_menu(app)?;
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_counts_cover_visible_tracks_and_global_overlay_edits() {
        let library = fixture::library();
        let all = counts(&library, SourceId::Music, &Selection::default(), "");
        assert_eq!(all.tracks, 26);
        assert_eq!(all.per_source.music, 26);
        assert_eq!(all.per_source.podcasts, 4);
        assert_eq!(all.per_source.audiobooks, 3);
        assert_eq!(all.overlay_edits, 5);

        let filtered = counts(&library, SourceId::Music, &Selection::default(), "bohemian");
        assert_eq!(filtered.tracks, 1);
    }
}

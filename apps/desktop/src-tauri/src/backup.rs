use std::{fs::File, io::Read, path::Path, sync::Arc};

use flate2::read::GzDecoder;
use retune_core::{io::SCHEMA_VERSION, model::Library};
use serde::{Deserialize, Serialize};
use tauri::Manager;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};

use crate::{
    lastfm_import,
    library_state::LibraryState,
    notify_error,
    persistence::atomic_write,
    playback::Playback,
    playlist_state::PlaylistState,
    playlists, restore,
    restore_latch::RestoreMutationState,
    settings_commands,
    store::{BrowserPanes, LastFmScrobblingProfile, Settings, SettingsState, Theme},
    AppState, MenuChecks,
};

const MAX_BACKUP_BYTES: u64 = 128 * 1024 * 1024;
const MAX_DECOMPRESSED_BACKUP_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ExportSettings {
    theme: Theme,
    zoom: f64,
    zebra: bool,
    #[serde(default)]
    pl_collapsed: bool,
    #[serde(default = "default_browser_visible")]
    browser_visible: bool,
    #[serde(default)]
    pub(super) browser_panes: BrowserPanes,
    column_order: Vec<String>,
    #[serde(default)]
    column_widths: std::collections::BTreeMap<String, u32>,
    hidden_columns: Vec<String>,
    #[serde(default)]
    playlist_hidden_columns: std::collections::BTreeMap<String, Vec<String>>,
    #[serde(default)]
    playlist_column_orders: std::collections::BTreeMap<String, Vec<String>>,
    #[serde(default)]
    playlist_column_widths:
        std::collections::BTreeMap<String, std::collections::BTreeMap<String, u32>>,
    #[serde(default)]
    sort_column: Option<String>,
    #[serde(default)]
    sort_desc: bool,
    #[serde(default)]
    shuffle: bool,
    #[serde(default)]
    pub(super) lastfm_scrobbling_profile: Option<LastFmScrobblingProfile>,
}

impl ExportSettings {
    pub(super) fn from_settings(settings: &Settings) -> Self {
        Self {
            theme: settings.theme,
            zoom: settings.zoom,
            zebra: settings.zebra,
            pl_collapsed: settings.pl_collapsed,
            browser_visible: settings.browser_visible,
            browser_panes: settings.browser_panes,
            column_order: settings.column_order.clone(),
            column_widths: settings.column_widths.clone(),
            hidden_columns: settings.hidden_columns.clone(),
            playlist_hidden_columns: settings.playlist_hidden_columns.clone(),
            playlist_column_orders: settings.playlist_column_orders.clone(),
            playlist_column_widths: settings.playlist_column_widths.clone(),
            sort_column: settings.sort_column.clone(),
            sort_desc: settings.sort_desc,
            shuffle: settings.shuffle,
            lastfm_scrobbling_profile: settings.lastfm_scrobbling_profile.clone(),
        }
    }

    pub(super) fn apply_to(self, settings: &mut Settings) -> Result<(), String> {
        settings.theme = self.theme;
        settings.zoom = self.zoom;
        settings.zebra = self.zebra;
        settings.pl_collapsed = self.pl_collapsed;
        settings.browser_visible = self.browser_visible;
        settings.browser_panes = self.browser_panes;
        settings.column_order = self.column_order;
        settings.column_widths = self.column_widths;
        settings.hidden_columns = self.hidden_columns;
        settings.playlist_hidden_columns = self.playlist_hidden_columns;
        settings.playlist_column_orders = self.playlist_column_orders;
        settings.playlist_column_widths = self.playlist_column_widths;
        settings.sort_column = self.sort_column;
        settings.sort_desc = self.sort_desc;
        settings.shuffle = self.shuffle;
        if self.lastfm_scrobbling_profile.is_some() {
            settings.lastfm_scrobbling_profile = self.lastfm_scrobbling_profile;
        }
        settings.normalize();
        settings.validate().map_err(|error| error.to_string())
    }
}

fn default_browser_visible() -> bool {
    true
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct BackupV1 {
    version: u32,
    library: Library,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    settings: Option<ExportSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    playlists: Option<playlists::PlaylistCache>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lastfm_mappings: Option<lastfm_import::PersistedLastFmMappings>,
}

pub(super) fn export_library(app: &tauri::AppHandle) {
    let handle = app.clone();
    app.dialog()
        .file()
        .set_file_name("Retune Library.json")
        .add_filter("Retune Library", &["json"])
        .save_file(move |path| {
            let Some(path) = path else { return };
            tauri::async_runtime::spawn(async move {
                let result = async {
                    let path = path.into_path().map_err(|error| error.to_string())?;
                    let state = handle.state::<AppState>();
                    let lastfm_mappings = state.lastfm_import.export_mappings().await;
                    let (library, settings, playlists) = snapshot_export_aggregates(
                        &state.library,
                        &state.settings,
                        &state.playlists,
                    )?;
                    tauri::async_runtime::spawn_blocking(move || {
                        write_export_with(
                            export_with_settings_and_mappings(
                                &library,
                                &settings,
                                &playlists,
                                Some(&lastfm_mappings),
                            ),
                            |bytes| atomic_write(&path, bytes, None),
                        )
                    })
                    .await
                    .map_err(|error| error.to_string())?
                }
                .await;
                if let Err(error) = result {
                    notify_error(&handle, error);
                }
            });
        });
}

fn write_export_with(
    serialized: Result<Vec<u8>, String>,
    write: impl FnOnce(&[u8]) -> std::io::Result<()>,
) -> Result<(), String> {
    write(&serialized?).map_err(|error| error.to_string())
}

fn snapshot_export_aggregates(
    library: &LibraryState,
    settings: &SettingsState,
    playlists: &PlaylistState,
) -> Result<(Library, Settings, playlists::PlaylistCache), String> {
    let library = library.lock().expect("library mutex poisoned").clone();
    let settings = settings.snapshot();
    let playlists = playlists.snapshot()?;
    Ok((library, settings, playlists))
}

#[cfg(test)]
pub(super) fn export_with_settings(
    library: &Library,
    settings: &Settings,
    playlists: &playlists::PlaylistCache,
) -> Result<Vec<u8>, String> {
    export_with_settings_and_mappings(library, settings, playlists, None)
}

pub(super) fn export_with_settings_and_mappings(
    library: &Library,
    settings: &Settings,
    playlists: &playlists::PlaylistCache,
    lastfm_mappings: Option<&lastfm_import::PersistedLastFmMappings>,
) -> Result<Vec<u8>, String> {
    serde_json::to_vec(&BackupV1 {
        version: SCHEMA_VERSION,
        library: library.clone(),
        settings: Some(ExportSettings::from_settings(settings)),
        playlists: Some(playlists.clone()),
        lastfm_mappings: lastfm_mappings.cloned(),
    })
    .map_err(|error| error.to_string())
}

#[cfg(test)]
pub(super) fn import_with_settings(
    bytes: &[u8],
    restore: bool,
) -> Result<
    (
        Library,
        Option<ExportSettings>,
        Option<playlists::PlaylistCache>,
    ),
    String,
> {
    import_with_settings_and_mappings(bytes, restore)
        .map(|(library, settings, playlists, _)| (library, settings, playlists))
}

type ImportedBackup = (
    Library,
    Option<ExportSettings>,
    Option<playlists::PlaylistCache>,
    Option<lastfm_import::PersistedLastFmMappings>,
);

pub(super) fn import_with_settings_and_mappings(
    bytes: &[u8],
    restore: bool,
) -> Result<ImportedBackup, String> {
    let json = decode_backup(bytes)?;
    let envelope: BackupV1 = serde_json::from_slice(&json).map_err(|error| error.to_string())?;
    if envelope.version != SCHEMA_VERSION {
        return Err(format!(
            "The backup version {} is unsupported.",
            envelope.version
        ));
    }
    let settings = envelope.settings.filter(|_| restore);
    if let Some(settings) = &settings {
        settings.clone().apply_to(&mut Settings::default())?;
    }
    let playlists = envelope.playlists.filter(|_| restore);
    let lastfm_mappings = envelope.lastfm_mappings.filter(|_| restore);
    if lastfm_mappings
        .as_ref()
        .is_some_and(|mappings: &lastfm_import::PersistedLastFmMappings| {
            mappings.version != lastfm_import::LASTFM_MAPPINGS_VERSION
        })
    {
        return Err("The Last.fm mappings version is unsupported.".into());
    }
    Ok((envelope.library, settings, playlists, lastfm_mappings))
}

fn read_backup(path: &Path) -> Result<Vec<u8>, String> {
    read_backup_with_limit(path, MAX_BACKUP_BYTES)
}

fn read_backup_with_limit(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|error| error.to_string())?;
    if file.metadata().map_err(|error| error.to_string())?.len() > limit {
        return Err(limit_error("Backup file", limit));
    }
    read_limited(file, limit, "Backup file")
}

fn decode_backup(bytes: &[u8]) -> Result<Vec<u8>, String> {
    decode_backup_with_limits(bytes, MAX_BACKUP_BYTES, MAX_DECOMPRESSED_BACKUP_BYTES)
}

fn decode_backup_with_limits(
    bytes: &[u8],
    compressed_limit: u64,
    decompressed_limit: u64,
) -> Result<Vec<u8>, String> {
    if bytes.len() as u64 > compressed_limit {
        return Err(limit_error("Backup file", compressed_limit));
    }
    if bytes.starts_with(&[0x1f, 0x8b]) {
        read_limited(
            GzDecoder::new(bytes),
            decompressed_limit,
            "Decompressed backup",
        )
    } else if bytes.len() as u64 > decompressed_limit {
        Err(limit_error("Decompressed backup", decompressed_limit))
    } else {
        Ok(bytes.to_vec())
    }
}

fn read_limited(reader: impl Read, limit: u64, description: &str) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > limit {
        Err(limit_error(description, limit))
    } else {
        Ok(bytes)
    }
}

fn limit_error(description: &str, limit: u64) -> String {
    format!(
        "{description} exceeds the {} MiB limit.",
        limit / 1024 / 1024
    )
}

pub(super) fn import_library(app: &tauri::AppHandle, replace: bool) {
    let handle = app.clone();
    app.dialog()
        .file()
        .add_filter("Retune Library", &["json", "json.gz", "gz"])
        .pick_file(move |path| {
            let Some(path) = path else { return };
            tauri::async_runtime::spawn(async move {
                let result = async {
                    let path = path.into_path().map_err(|error| error.to_string())?;
                    tauri::async_runtime::spawn_blocking(move || {
                        let bytes = read_backup(&path)?;
                        import_with_settings_and_mappings(&bytes, replace)
                    })
                    .await
                    .map_err(|error| error.to_string())?
                }
                .await;
                match result {
                    Ok((library, settings, playlists, lastfm_mappings)) if replace => {
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
                                    tauri::async_runtime::spawn_blocking(move || {
                                        let state = confirmed_handle.state::<AppState>();
                                        apply_import(
                                            &confirmed_handle,
                                            &state.library,
                                            &state.settings,
                                            &state.playlists,
                                            state.lastfm_import.as_ref(),
                                            state.restore_mutations.as_ref(),
                                            state.menu_checks.as_ref(),
                                            Arc::clone(&state.playback),
                                            library,
                                            settings,
                                            playlists,
                                            lastfm_mappings,
                                            true,
                                        );
                                    });
                                }
                            });
                    }
                    Ok((library, _, _, _)) => {
                        let merge_handle = handle.clone();
                        tauri::async_runtime::spawn_blocking(move || {
                            let state = merge_handle.state::<AppState>();
                            apply_import(
                                &merge_handle,
                                &state.library,
                                &state.settings,
                                &state.playlists,
                                state.lastfm_import.as_ref(),
                                state.restore_mutations.as_ref(),
                                state.menu_checks.as_ref(),
                                Arc::clone(&state.playback),
                                library,
                                None,
                                None,
                                None,
                                false,
                            );
                        });
                    }
                    Err(error) => notify_error(&handle, error),
                }
            });
        });
}

#[allow(clippy::too_many_arguments)]
fn apply_import(
    app: &tauri::AppHandle,
    library: &LibraryState,
    settings: &SettingsState,
    playlists: &PlaylistState,
    lastfm_import: &lastfm_import::Service,
    restore_mutations: &RestoreMutationState,
    menu_checks: Option<&MenuChecks>,
    playback: Arc<Playback>,
    imported: Library,
    export_settings: Option<ExportSettings>,
    imported_playlists: Option<playlists::PlaylistCache>,
    imported_lastfm_mappings: Option<lastfm_import::PersistedLastFmMappings>,
    replace: bool,
) {
    if replace {
        if let Err(error) = apply_restore(
            app,
            library,
            settings,
            playlists,
            lastfm_import,
            restore_mutations,
            menu_checks,
            playback,
            imported,
            export_settings,
            imported_playlists,
            imported_lastfm_mappings,
        ) {
            notify_error(app, error);
        }
        return;
    }
    let result = library.mutate(|library| {
        library.merge(imported);
        Ok(())
    });
    match result {
        Ok(()) => {
            let _ = crate::emit_main(app, "library-changed", ());
        }
        Err(error) => notify_error(app, error),
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_restore(
    app: &tauri::AppHandle,
    library: &LibraryState,
    settings: &SettingsState,
    playlists: &PlaylistState,
    lastfm_import: &lastfm_import::Service,
    restore_mutations: &RestoreMutationState,
    menu_checks: Option<&MenuChecks>,
    playback: Arc<Playback>,
    imported: Library,
    export_settings: Option<ExportSettings>,
    imported_playlists: Option<playlists::PlaylistCache>,
    imported_lastfm_mappings: Option<lastfm_import::PersistedLastFmMappings>,
) -> Result<(), String> {
    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?;
    commit_restore(
        library,
        settings,
        playlists,
        lastfm_import,
        restore_mutations,
        &app_data_dir,
        imported,
        export_settings,
        imported_playlists,
        imported_lastfm_mappings,
        |refresh| refresh_completed_restore(app, menu_checks, playback, refresh),
    )
}

pub(super) struct RestoreRefresh {
    settings: Option<(Settings, Settings)>,
    playlists: bool,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn commit_restore(
    library: &LibraryState,
    settings: &SettingsState,
    playlists: &PlaylistState,
    lastfm_import: &lastfm_import::Service,
    restore_mutations: &RestoreMutationState,
    app_data_dir: &Path,
    imported: Library,
    export_settings: Option<ExportSettings>,
    imported_playlists: Option<playlists::PlaylistCache>,
    imported_lastfm_mappings: Option<lastfm_import::PersistedLastFmMappings>,
    after_complete: impl FnOnce(RestoreRefresh),
) -> Result<(), String> {
    let library_restore = library.begin_restore()?;
    let mut settings_restore = tauri::async_runtime::block_on(settings.begin_restore())?;
    let playlist_restore = tauri::async_runtime::block_on(playlists.begin_restore())?;
    let mut mappings_restore =
        tauri::async_runtime::block_on(lastfm_import.begin_mappings_restore())?;

    let before_library = library_restore.snapshot();
    let before_settings = settings_restore.snapshot();
    let after_settings = export_settings
        .map(|settings| {
            let mut next = before_settings.clone();
            settings.apply_to(&mut next)?;
            Ok::<_, String>(next)
        })
        .transpose()?;
    let before_playlists = imported_playlists
        .as_ref()
        .map(|_| playlist_restore.snapshot());
    let after_mappings = imported_lastfm_mappings
        .map(lastfm_import::normalize_restored_mappings)
        .transpose()?;
    let before_mappings = if after_mappings.is_some() {
        Some(mappings_restore.snapshot())
    } else {
        None
    };

    let journal = restore::RestoreJournal::applying(
        restore::Change {
            before: before_library.clone(),
            after: imported.clone(),
        },
        after_settings.clone().map(|after| restore::Change {
            before: before_settings.clone(),
            after,
        }),
        imported_playlists.clone().map(|after| restore::Change {
            before: before_playlists.expect("playlist snapshot exists"),
            after,
        }),
        after_mappings.clone().map(|after| restore::Change {
            before: before_mappings.expect("mapping snapshot exists"),
            after,
        }),
    );
    let refresh = RestoreRefresh {
        settings: after_settings
            .clone()
            .map(|after| (before_settings.clone(), after)),
        playlists: imported_playlists.is_some(),
    };
    let restore_store = restore::RestoreStore::new(app_data_dir);
    restore_store
        .begin(&journal)
        .map_err(|error| error.to_string())?;

    let primary = (|| {
        library_restore.replace(imported)?;
        if let Some(settings) = after_settings.as_ref() {
            settings_restore.replace(settings.clone())?;
        }
        if let Some(playlists) = imported_playlists.as_ref() {
            playlist_restore.replace(playlists.clone())?;
        }
        if let Some(mappings) = after_mappings.as_ref() {
            mappings_restore.replace(mappings.clone())?;
        }
        restore_store
            .complete(&journal)
            .map_err(|error| error.to_string())
    })();
    if let Err(primary_error) = primary {
        if let Err(recovery_error) = restore_store.recover() {
            restore_mutations.mark_recovery_required();
            drop(mappings_restore);
            drop(playlist_restore);
            drop(settings_restore);
            drop(library_restore);
            return Err(format!(
                "Restore failed ({primary_error}) and immediate recovery failed ({recovery_error}). Restart Retune to recover before making more changes."
            ));
        }
        log::warn!("Restore write failed but was rolled forward immediately: {primary_error}");
        library_restore.install_recovered(journal.library.after.clone());
        if let Some(change) = &journal.settings {
            settings_restore.install_recovered(change.after.clone());
        }
        if let Some(change) = &journal.playlists {
            playlist_restore.install_recovered(change.after.clone());
        }
        if let Some(change) = &journal.lastfm_mappings {
            mappings_restore.install_recovered(change.after.clone());
        }
    }
    drop(mappings_restore);
    drop(playlist_restore);
    drop(settings_restore);
    drop(library_restore);
    after_complete(refresh);
    if let Err(error) = restore_store.cleanup() {
        log::warn!("{error}");
    }
    Ok(())
}

fn refresh_completed_restore(
    app: &tauri::AppHandle,
    menu_checks: Option<&MenuChecks>,
    playback: Arc<Playback>,
    refresh: RestoreRefresh,
) {
    refresh_completed_restore_with(
        refresh,
        |previous, settings| apply_settings_effects(app, menu_checks, playback, previous, settings),
        || crate::emit_main(app, "playlists-changed", ()).map_err(|error| error.to_string()),
        || crate::emit_main(app, "library-changed", ()).map_err(|error| error.to_string()),
    );
}

pub(super) fn refresh_completed_restore_with(
    refresh: RestoreRefresh,
    settings_refresh: impl FnOnce(&Settings, &Settings) -> Result<(), String>,
    playlist_refresh: impl FnOnce() -> Result<(), String>,
    library_refresh: impl FnOnce() -> Result<(), String>,
) {
    if let Some((previous, settings)) = refresh.settings {
        if let Err(error) = settings_refresh(&previous, &settings) {
            log::warn!("Could not refresh restored settings: {error}");
        }
    }
    if refresh.playlists {
        if let Err(error) = playlist_refresh() {
            log::warn!("Could not refresh restored playlists: {error}");
        }
    }
    if let Err(error) = library_refresh() {
        log::warn!("Could not refresh restored library: {error}");
    }
}

fn apply_settings_effects(
    app: &tauri::AppHandle,
    menu_checks: Option<&MenuChecks>,
    playback: Arc<Playback>,
    previous: &Settings,
    settings: &Settings,
) -> Result<(), String> {
    settings_commands::emit_settings_changed(app, previous, settings)?;
    if let Some(menu_checks) = menu_checks {
        menu_checks
            .sync(settings)
            .map_err(|error| error.to_string())?;
    }
    let app = app.clone();
    let shuffle = settings.shuffle;
    tauri::async_runtime::spawn(async move {
        let event = playback.set_shuffle(shuffle).await;
        let _ = crate::emit_main_event(&app, crate::main_events::MainEvent::PlayerState(event));
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write,
        sync::{mpsc, Arc, Barrier},
        time::Duration,
    };

    use flate2::{write::GzEncoder, Compression};

    use super::*;
    use crate::persistence::{atomic_write_with_failure, FailureStage};

    #[test]
    fn backup_snapshot_and_playlist_projection_complete_without_lock_inversion() {
        let directory = tempfile::tempdir().unwrap();
        let library = Arc::new(LibraryState::new(
            Library::new(),
            crate::store::FsOverlayStore::new(directory.path()),
        ));
        let settings = Arc::new(SettingsState::new(
            Settings::default(),
            crate::store::FsSettingsStore::new(directory.path()),
        ));
        let playlists = Arc::new(PlaylistState::new(
            playlists::PlaylistCache {
                playlists: vec![playlists::CachedPlaylist {
                    id: "playlist".into(),
                    name: "Playlist".into(),
                    snapshot_id: "snapshot".into(),
                    owned: true,
                    owner: None,
                    track_count: 0,
                    tracks: Vec::new(),
                    track_metadata_version: playlists::TRACK_METADATA_VERSION,
                    spotify_tracks: Vec::new(),
                }],
            },
            crate::store::FsPlaylistStore::new(directory.path()),
        ));
        let barrier = Arc::new(Barrier::new(3));
        let (done_tx, done_rx) = mpsc::channel();

        let export = {
            let library = Arc::clone(&library);
            let settings = Arc::clone(&settings);
            let playlists = Arc::clone(&playlists);
            let barrier = Arc::clone(&barrier);
            let done = done_tx.clone();
            std::thread::spawn(move || {
                barrier.wait();
                snapshot_export_aggregates(&library, &settings, &playlists).unwrap();
                done.send(()).unwrap();
            })
        };
        let projection = {
            let library = Arc::clone(&library);
            let playlists = Arc::clone(&playlists);
            let barrier = Arc::clone(&barrier);
            let done = done_tx;
            std::thread::spawn(move || {
                barrier.wait();
                let views = crate::playlist_commands::playlist_track_views_from_state(
                    &playlists, &library, "playlist",
                )
                .unwrap();
                done.send(()).unwrap();
                views
            })
        };

        barrier.wait();
        for _ in 0..2 {
            done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        }
        export.join().unwrap();
        assert!(projection.join().unwrap().is_empty());
    }

    #[test]
    fn every_backup_export_failure_preserves_the_previous_backup() {
        let library = Library::new();
        let settings = Settings::default();
        let playlists = playlists::PlaylistCache::default();

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("backup.json");
        std::fs::write(&path, b"previous valid backup").unwrap();
        assert!(
            write_export_with(Err("injected serialization failure".into()), |bytes| {
                atomic_write(&path, bytes, None)
            },)
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"previous valid backup");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);

        for stage in [
            FailureStage::Create,
            FailureStage::Write,
            FailureStage::Sync,
            FailureStage::Rename,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("backup.json");
            std::fs::write(&path, b"previous valid backup").unwrap();

            assert!(write_export_with(
                export_with_settings(&library, &settings, &playlists),
                |bytes| atomic_write_with_failure(&path, bytes, None, stage),
            )
            .is_err());
            assert_eq!(std::fs::read(&path).unwrap(), b"previous valid backup");
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        }
    }

    const CORE_ONLY_V1: &[u8] = br#"{
        "version": 1,
        "library": {"tracks": [], "album_ratings": [], "next_id": 0}
    }"#;

    const FULL_V1: &[u8] = br#"{
        "version": 1,
        "library": {"tracks": [], "album_ratings": [], "next_id": 0},
        "settings": {
            "theme": "dark",
            "zoom": 1.25,
            "zebra": false,
            "columnOrder": [],
            "hiddenColumns": []
        },
        "playlists": {"playlists": []}
    }"#;

    #[test]
    fn v1_core_only_and_full_backups_keep_their_meaning() {
        let (library, settings, playlists, mappings) =
            import_with_settings_and_mappings(CORE_ONLY_V1, true).unwrap();
        assert_eq!(library, Library::new());
        assert!(settings.is_none());
        assert!(playlists.is_none());
        assert!(mappings.is_none());

        let (library, settings, playlists, mappings) =
            import_with_settings_and_mappings(FULL_V1, true).unwrap();
        assert_eq!(library, Library::new());
        let settings = settings.unwrap();
        assert_eq!(settings.theme, Theme::Dark);
        assert_eq!(settings.zoom, 1.25);
        assert_eq!(playlists, Some(playlists::PlaylistCache::default()));
        assert!(mappings.is_none());

        let bytes = export_with_settings(
            &library,
            &Settings::default(),
            &playlists::PlaylistCache::default(),
        )
        .unwrap();
        assert_eq!(import_with_settings(&bytes, true).unwrap().0, library);
    }

    #[test]
    fn unknown_backup_version_is_rejected() {
        assert!(import_with_settings_and_mappings(
            br#"{"version":999,"library":{"tracks":[],"album_ratings":[],"next_id":0}}"#,
            true,
        )
        .is_err());
    }

    #[test]
    fn backup_input_limits_accept_exact_and_reject_one_over() {
        assert_eq!(
            read_limited(&b"12345678"[..], 8, "test").unwrap(),
            b"12345678"
        );
        assert!(read_limited(&b"123456789"[..], 8, "test").is_err());

        let directory = tempfile::tempdir().unwrap();
        let sparse = directory.path().join("oversized.json");
        File::create(&sparse).unwrap().set_len(9).unwrap();
        assert!(read_backup_with_limit(&sparse, 8).is_err());
    }

    #[test]
    fn gzip_expansion_has_an_independent_limit() {
        let decoded = vec![b'x'; 1024];
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&decoded).unwrap();
        let compressed = encoder.finish().unwrap();
        let compressed_limit = compressed.len() as u64;

        assert_eq!(
            decode_backup_with_limits(&compressed, compressed_limit, decoded.len() as u64).unwrap(),
            decoded
        );
        assert!(decode_backup_with_limits(&compressed, compressed_limit, 1023).is_err());
        assert!(decode_backup_with_limits(&compressed, compressed_limit - 1, 1024).is_err());
    }
}

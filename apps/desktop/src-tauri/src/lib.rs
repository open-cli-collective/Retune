mod backup;
mod diagnostics;
mod external_links;
mod fixture;
mod lastfm;
mod lastfm_import;
mod library_commands;
mod library_state;
mod localfiles;
mod main_events;
mod media_keys;
mod persistence;
mod playback;
mod playback_commands;
mod playback_resources;
mod playlist_commands;
mod playlist_state;
mod playlists;
mod provider;
mod restore;
mod restore_latch;
mod settings_commands;
mod spotify_commands;
mod spotify_membership;
mod spotify_sync_commit;
mod store;
mod sync;
mod sync_orchestrator;

use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine};
use library_state::LibraryState;
#[cfg(test)]
use library_state::{
    commit_library_candidate, record_play_with as record_play, LibraryTransactionState,
};

#[cfg(test)]
use playback::RepeatMode;
use playback::{AudioSettings, Playback, PlaybackBackend, PlayerStateEvent};
use playlist_state::PlaylistState;
#[cfg(test)]
use provider::SyncBatch;
use provider::{image_url, image_url_at_least, spotify_id};
use retune_core::model::Library;
#[cfg(test)]
use retune_core::model::{AlbumKey, Rating, SourceId};
#[cfg(test)]
use retune_spotify::client::{Album, Track as SpotifyTrack};
use retune_spotify::{
    auth::{self, LoopbackListener, Pkce},
    catalog::SpotifyCatalog,
    client::{HttpTransport, SpotifyClient, Transport},
    tokens::{CachedTokenStore, EncryptedFsTokenStore, TokenStore, Tokens},
};
use serde::Serialize;
use spotify_commands::sync_spotify;
#[cfg(test)]
use spotify_commands::{
    artist_albums_outcome, mark_album_membership, mark_track_membership, partial_import_message,
    record_full_sync, SyncProgressState,
};
#[cfg(test)]
use spotify_commands::{
    spotify_item_link, spotify_track_destination, SpotifyDestination, SpotifyNavigation,
    SpotifyOpenTarget,
};
#[cfg(test)]
use spotify_membership::album_track_uris;
#[cfg(test)]
use store::SpotifyLibraryState;
use store::{
    FsArtistGenresStore, FsCooldownStore, FsOverlayStore, FsPlaylistStore, FsSettingsStore,
    FsSpotifyCatalogStore, FsSpotifyLibraryStore, OverlayStore, Settings, SettingsState,
    StoreError, Theme,
};
use sync_orchestrator::SyncOrchestrator;
use tauri::{
    menu::{CheckMenuItem, CheckMenuItemBuilder, MenuBuilder, MenuItemBuilder, SubmenuBuilder},
    Emitter, Manager,
};
use tauri_plugin_opener::OpenerExt;

pub(crate) type SharedTokenStore = Arc<CachedTokenStore<Box<dyn TokenStore>>>;
type SpotifyProvider = SpotifyClient<HttpTransport, SharedTokenStore>;
use tauri_plugin_dialog::DialogExt;

enum PlaybackDurableEffect {
    RecordPlay(String),
    Listening(playback::ListeningFact),
    Shutdown(tokio::sync::oneshot::Sender<()>),
}

struct PlaybackEffects {
    sender: Mutex<Option<tokio::sync::mpsc::Sender<PlaybackDurableEffect>>>,
    task: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExitAction {
    StartDrain,
    WaitForDrain,
    Allow,
}

fn exit_action(state: &AtomicU8) -> ExitAction {
    loop {
        match state.load(Ordering::Acquire) {
            0 => {
                if state
                    .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return ExitAction::StartDrain;
                }
            }
            1 => return ExitAction::WaitForDrain,
            _ => return ExitAction::Allow,
        }
    }
}

impl PlaybackEffects {
    #[cfg(test)]
    fn disabled() -> Self {
        Self {
            sender: Mutex::new(None),
            task: Mutex::new(None),
        }
    }

    fn start(app: tauri::AppHandle, lastfm: Arc<lastfm::Service>) -> Self {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(64);
        let task = tauri::async_runtime::spawn(async move {
            while let Some(effect) = receiver.recv().await {
                match effect {
                    PlaybackDurableEffect::RecordPlay(uri) => {
                        let handle = app.clone();
                        match tauri::async_runtime::spawn_blocking(move || {
                            let state = handle.state::<AppState>();
                            state.library.record_play(&uri, unix_now())
                        })
                        .await
                        {
                            Ok(Ok(true)) => {
                                if let Err(error) = emit_main(&app, "library-changed", ()) {
                                    notify_error(&app, error.to_string());
                                }
                            }
                            Ok(Ok(false)) => {}
                            Ok(Err(error)) => notify_error(&app, error),
                            Err(error) => notify_error(&app, error.to_string()),
                        }
                    }
                    PlaybackDurableEffect::Listening(fact) => {
                        lastfm.handle_listening_fact(fact).await;
                    }
                    PlaybackDurableEffect::Shutdown(done) => {
                        let _ = done.send(());
                        break;
                    }
                }
            }
        });
        Self {
            sender: Mutex::new(Some(sender)),
            task: Mutex::new(Some(task)),
        }
    }

    fn submit(&self, effect: PlaybackDurableEffect) -> Result<(), String> {
        self.sender
            .lock()
            .expect("playback effects sender mutex poisoned")
            .as_ref()
            .ok_or_else(|| "Playback persistence is unavailable.".to_string())?
            .try_send(effect)
            .map_err(|_| "Playback persistence queue is full.".to_string())
    }

    async fn shutdown(&self) {
        let sender = self
            .sender
            .lock()
            .expect("playback effects sender mutex poisoned")
            .take();
        if let Some(sender) = sender {
            let (done, drained) = tokio::sync::oneshot::channel();
            if sender
                .send(PlaybackDurableEffect::Shutdown(done))
                .await
                .is_ok()
            {
                let _ = drained.await;
            }
        }
        let task = self
            .task
            .lock()
            .expect("playback effects mutex poisoned")
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

struct AppState {
    library: LibraryState,
    spotify_membership: spotify_membership::SpotifyMembership,
    settings: SettingsState,
    cooldown_store: FsCooldownStore,
    artist_genres_store: FsArtistGenresStore,
    playlists: PlaylistState,
    menu_checks: Option<MenuChecks>,
    main_events: main_events::MainEventSink,
    token_store: SharedTokenStore,
    spotify_catalog: Arc<Mutex<SpotifyCatalog>>,
    spotify_catalog_store: FsSpotifyCatalogStore,
    spotify_catalog_saved_generation: Arc<AtomicU64>,
    spotify_catalog_flush_gate: Arc<Mutex<()>>,
    spotify_catalog_hydration_epoch: Arc<AtomicU64>,
    spotify: Mutex<Option<Arc<SpotifyProvider>>>,
    artwork_cache: Mutex<HashMap<(String, u32), Option<String>>>,
    playback: Arc<Playback>,
    playback_effects: PlaybackEffects,
    lastfm: Arc<lastfm::Service>,
    lastfm_import: Arc<lastfm_import::Service>,
    media_keys: media_keys::MediaKeys,
    spotify_session: spotify_commands::SpotifySession,
    sync_orchestrator: SyncOrchestrator,
    catalog_flush_task: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    playlist_reauth_notified: AtomicBool,
    local_import_active: Arc<AtomicBool>,
    restore_mutations: Arc<restore_latch::RestoreMutationState>,
    shutdown_state: AtomicU8,
}

#[cfg(test)]
pub(crate) fn test_app_state(
    app_data_dir: impl AsRef<std::path::Path>,
    library: Library,
    spotify_library: SpotifyLibraryState,
    lastfm: Arc<lastfm::Service>,
    lastfm_import: Arc<lastfm_import::Service>,
) -> AppState {
    let app_data_dir = app_data_dir.as_ref().to_path_buf();
    let token_store = Arc::new(CachedTokenStore::new(Box::new(store::FsTokenStore::new(
        &app_data_dir,
    )) as Box<dyn TokenStore>));
    let spotify_catalog = Arc::new(Mutex::new(SpotifyCatalog::default()));
    let restore_mutations = lastfm_import.restore_mutations();
    AppState {
        library: LibraryState::new_with_restore_state(
            library,
            FsOverlayStore::new(&app_data_dir),
            Arc::clone(&restore_mutations),
        ),
        spotify_membership: spotify_membership::SpotifyMembership::new_with_restore_state(
            spotify_library,
            FsSpotifyLibraryStore::new(&app_data_dir),
            Arc::clone(&restore_mutations),
        ),
        settings: SettingsState::new_with_restore_state(
            Settings::default(),
            FsSettingsStore::new(&app_data_dir),
            Arc::clone(&restore_mutations),
        ),
        cooldown_store: FsCooldownStore::new(&app_data_dir),
        artist_genres_store: FsArtistGenresStore::new(&app_data_dir),
        playlists: PlaylistState::new_with_restore_state(
            playlists::PlaylistCache::default(),
            FsPlaylistStore::new(&app_data_dir),
            Arc::clone(&restore_mutations),
        ),
        menu_checks: None,
        main_events: main_events::MainEventSink::default(),
        token_store,
        spotify_catalog,
        spotify_catalog_store: FsSpotifyCatalogStore::new(&app_data_dir),
        spotify_catalog_saved_generation: Arc::new(AtomicU64::new(0)),
        spotify_catalog_flush_gate: Arc::new(Mutex::new(())),
        spotify_catalog_hydration_epoch: Arc::new(AtomicU64::new(0)),
        spotify: Mutex::new(None),
        artwork_cache: Mutex::default(),
        playback: Arc::new(Playback::default()),
        playback_effects: PlaybackEffects::disabled(),
        lastfm,
        lastfm_import,
        media_keys: media_keys::MediaKeys::disabled(),
        spotify_session: spotify_commands::SpotifySession::default(),
        sync_orchestrator: SyncOrchestrator::default(),
        catalog_flush_task: Mutex::new(None),
        playlist_reauth_notified: AtomicBool::new(false),
        local_import_active: Arc::new(AtomicBool::new(false)),
        restore_mutations,
        shutdown_state: AtomicU8::new(0),
    }
}

struct MenuChecks {
    zebra: CheckMenuItem<tauri::Wry>,
    theme_system: CheckMenuItem<tauri::Wry>,
    theme_light: CheckMenuItem<tauri::Wry>,
    theme_dark: CheckMenuItem<tauri::Wry>,
    account_status: tauri::menu::MenuItem<tauri::Wry>,
    connect: tauri::menu::MenuItem<tauri::Wry>,
    disconnect: tauri::menu::MenuItem<tauri::Wry>,
}

impl MenuChecks {
    fn sync(&self, settings: &Settings) -> tauri::Result<()> {
        self.zebra.set_checked(settings.zebra)?;
        self.theme_system
            .set_checked(settings.theme == Theme::System)?;
        self.theme_light
            .set_checked(settings.theme == Theme::Light)?;
        self.theme_dark.set_checked(settings.theme == Theme::Dark)
    }

    fn sync_connection(&self, connection: &ConnectionState) -> tauri::Result<()> {
        self.account_status.set_text(if connection.needs_reauth {
            "Reconnect required"
        } else if connection.connected {
            "Connected"
        } else {
            "Not connected"
        })?;
        self.connect
            .set_enabled(!connection.connected || connection.needs_reauth)?;
        self.disconnect.set_enabled(connection.connected)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct ConnectionState {
    connected: bool,
    needs_reauth: bool,
    playback_authorized: bool,
    missing_scopes: Vec<String>,
}

impl ConnectionState {
    fn from_tokens(tokens: Option<Tokens>) -> Self {
        let missing_scopes = tokens
            .as_ref()
            .map(Tokens::missing_scopes)
            .unwrap_or_default()
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();
        Self {
            connected: tokens.is_some(),
            needs_reauth: !missing_scopes.is_empty(),
            playback_authorized: tokens
                .as_ref()
                .and_then(|tokens| tokens.playback_credentials.as_ref())
                .is_some_and(|credentials| {
                    !credentials.username.is_empty() && !credentials.auth_data.is_empty()
                }),
            missing_scopes,
        }
    }
}

#[tauri::command]
async fn finish_lastfm(app: tauri::AppHandle) -> Result<lastfm::LastFmState, String> {
    let state = app.state::<AppState>();
    let account_changed = state.lastfm.finish().await?;
    if account_changed {
        state.lastfm_import.clear_sync_state().await?;
    }
    state.lastfm.activate_connection().await;
    let sync_app = app.clone();
    tauri::async_runtime::spawn(async move {
        let _ = lastfm_import::commands::sync_lastfm_plays(sync_app).await;
    });
    state.lastfm.set_enabled(true).await;
    settings_commands::set_lastfm_scrobbling(&app, true).await?;
    let result = state.lastfm.state().await;
    Ok(result)
}

#[tauri::command]
async fn disconnect_lastfm(app: tauri::AppHandle) -> Result<lastfm::LastFmState, String> {
    let state = app.state::<AppState>();
    let result = state.lastfm.disconnect().await?;
    state.lastfm_import.clear_sync_state().await?;
    Ok(result)
}

async fn switch_to_local(state: &AppState, volume: u8) -> Result<(), String> {
    state
        .token_store
        .load()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Connect to Spotify before enabling built-in playback.".to_string())?;
    let client = provider_from(state)?;
    state
        .playback
        .switch_to_local(client.as_ref(), volume)
        .await
}

fn spotify_provider(
    client_id: &str,
    token_store: SharedTokenStore,
    catalog: Arc<Mutex<SpotifyCatalog>>,
) -> Result<Option<Arc<SpotifyProvider>>, String> {
    if client_id.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(Arc::new(SpotifyClient::new_with_catalog(
        client_id.trim(),
        HttpTransport::new(),
        token_store,
        catalog,
    ))))
}

fn stored_connection_state(token_store: &SharedTokenStore) -> Result<ConnectionState, String> {
    token_store
        .load()
        .map(ConnectionState::from_tokens)
        .map_err(|error| error.to_string())
}

fn quarantine_token_file(
    app_data_dir: &std::path::Path,
    use_dev_token_store: bool,
    timestamp: u64,
) -> std::io::Result<PathBuf> {
    let name = if use_dev_token_store {
        "dev-tokens.json"
    } else {
        "tokens.enc"
    };
    let path = app_data_dir.join(name);
    let quarantined = app_data_dir.join(format!("{name}.corrupt-{timestamp}"));
    fs::rename(path, &quarantined)?;
    Ok(quarantined)
}

async fn load_startup_credentials<T: Send + 'static>(
    load: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(load)
        .await
        .map_err(|error| error.to_string())
}

fn spawn_catalog_hydration<E: Send + 'static>(
    catalog: Arc<Mutex<SpotifyCatalog>>,
    saved_generation: Arc<AtomicU64>,
    flush_gate: Arc<Mutex<()>>,
    hydration_epoch: Arc<AtomicU64>,
    load: impl FnOnce() -> Result<SpotifyCatalog, E> + Send + 'static,
) -> tauri::async_runtime::JoinHandle<Result<bool, E>> {
    let baseline = catalog
        .lock()
        .expect("Spotify catalog mutex poisoned")
        .generation();
    let baseline_epoch = hydration_epoch.load(Ordering::Acquire);
    tauri::async_runtime::spawn_blocking(move || {
        let loaded = load()?;
        let _flush = flush_gate
            .lock()
            .expect("Spotify catalog flush gate poisoned");
        let mut current = catalog.lock().expect("Spotify catalog mutex poisoned");
        if current.generation() != baseline
            || hydration_epoch.load(Ordering::Acquire) != baseline_epoch
        {
            return Ok(false);
        }
        let generation = loaded.generation();
        *current = loaded;
        saved_generation.store(generation, Ordering::Release);
        Ok(true)
    })
}

fn flush_spotify_catalog(state: &AppState) -> Result<(), String> {
    let _flush = state
        .spotify_catalog_flush_gate
        .lock()
        .expect("Spotify catalog flush gate poisoned");
    let (catalog, generation) = {
        let catalog = state
            .spotify_catalog
            .lock()
            .expect("Spotify catalog mutex poisoned");
        if catalog.generation()
            == state
                .spotify_catalog_saved_generation
                .load(Ordering::Acquire)
        {
            return Ok(());
        }
        (catalog.clone(), catalog.generation())
    };
    state
        .spotify_catalog_store
        .save(&catalog)
        .map_err(|error| error.to_string())?;
    let current_generation = state
        .spotify_catalog
        .lock()
        .expect("Spotify catalog mutex poisoned")
        .generation();
    if current_generation == generation {
        state
            .spotify_catalog_saved_generation
            .store(generation, Ordering::Release);
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn clear_spotify_catalog(state: &AppState) -> Result<(), String> {
    state
        .spotify_catalog_hydration_epoch
        .fetch_add(1, Ordering::AcqRel);
    state
        .spotify_catalog
        .lock()
        .expect("Spotify catalog mutex poisoned")
        .clear();
    flush_spotify_catalog(state)
}

pub(crate) async fn clear_spotify_catalog_async(app: &tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    state
        .spotify_catalog_hydration_epoch
        .fetch_add(1, Ordering::AcqRel);
    state
        .spotify_catalog
        .lock()
        .expect("Spotify catalog mutex poisoned")
        .clear();
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || flush_spotify_catalog(&handle.state::<AppState>()))
        .await
        .map_err(|error| error.to_string())?
}

pub(crate) async fn emit_connection_state_async(app: &tauri::AppHandle) -> Result<(), String> {
    let token_store = Arc::clone(&app.state::<AppState>().token_store);
    let connection =
        tauri::async_runtime::spawn_blocking(move || stored_connection_state(&token_store))
            .await
            .map_err(|error| error.to_string())??;
    let state = app.state::<AppState>();
    if let Some(menu_checks) = &state.menu_checks {
        menu_checks
            .sync_connection(&connection)
            .map_err(|error| error.to_string())?;
    }
    emit_main(app, "connection-changed", ()).map_err(|error| error.to_string())
}

pub(crate) fn emit_main<R: tauri::Runtime, S: serde::Serialize + Clone>(
    app: &tauri::AppHandle<R>,
    event: &str,
    payload: S,
) -> tauri::Result<()> {
    app.emit_to("main", event, payload)
}

fn empty_player_state(shuffle: bool) -> PlayerStateEvent {
    PlayerStateEvent {
        track_id: None,
        uri: None,
        elapsed: 0,
        is_playing: false,
        external: false,
        name: None,
        art: None,
        alb: None,
        duration_secs: None,
        volume_supported: false,
        shuffle,
    }
}

fn provider_from(state: &AppState) -> Result<Arc<SpotifyProvider>, String> {
    state
        .spotify
        .lock()
        .expect("spotify mutex poisoned")
        .clone()
        .ok_or_else(|| {
            "Spotify Client ID is missing. Add it in Preferences, then try again.".into()
        })
}

fn track_id(uri: &str) -> Option<&str> {
    provider::spotify_id(uri, "track").ok()
}

fn album_id(uri: &str) -> Option<&str> {
    provider::spotify_id(uri, "album").ok()
}

async fn resolve_track_artwork<T: Transport, S: TokenStore>(
    client: Option<&SpotifyClient<T, S>>,
    cache: &Mutex<HashMap<(String, u32), Option<String>>>,
    local_path: Result<Option<PathBuf>, String>,
    uri: &str,
    min_width: u32,
) -> Result<Option<String>, String> {
    resolve_track_artwork_with(client, cache, local_path, uri, min_width, |path| {
        retune_audio::read_artwork(path, MAX_LOCAL_ARTWORK_BYTES)
            .map_err(|error| error.to_string())
            .and_then(|artwork| artwork.map(local_artwork_data_url).transpose())
    })
    .await
}

async fn resolve_track_artwork_with<T, S, F>(
    client: Option<&SpotifyClient<T, S>>,
    cache: &Mutex<HashMap<(String, u32), Option<String>>>,
    local_path: Result<Option<PathBuf>, String>,
    uri: &str,
    min_width: u32,
    read_local: F,
) -> Result<Option<String>, String>
where
    T: Transport,
    S: TokenStore,
    F: FnOnce(PathBuf) -> Result<Option<String>, String> + Send + 'static,
{
    let local_path = local_path?;
    let id = local_path.is_none().then(|| track_id(uri)).flatten();
    if local_path.is_none() && id.is_none() {
        return Ok(None);
    }
    let cache_key = (uri.into(), min_width);
    if let Some(cached) = cache
        .lock()
        .expect("artwork cache mutex poisoned")
        .get(&cache_key)
        .cloned()
    {
        return Ok(cached);
    }
    let artwork = if let Some(path) = local_path {
        tauri::async_runtime::spawn_blocking(move || read_local(path))
            .await
            .map_err(|error| error.to_string())??
    } else {
        client
            .ok_or_else(|| "Connect to Spotify to load artwork.".to_string())?
            .track(id.expect("validated Spotify track URI"))
            .await
            .ok()
            .and_then(|track| track.album)
            .and_then(|album| image_url_at_least(&album.images, min_width))
    };
    let mut cache = cache.lock().expect("artwork cache mutex poisoned");
    if cache.len() >= 512 {
        cache.clear();
    }
    cache.insert(cache_key, artwork.clone());
    Ok(artwork)
}

const MAX_LOCAL_ARTWORK_BYTES: usize = 8 * 1024 * 1024;

fn local_artwork_data_url(artwork: retune_audio::Artwork) -> Result<String, String> {
    if artwork.bytes.len() > MAX_LOCAL_ARTWORK_BYTES {
        return Err(format!(
            "Embedded artwork exceeds the {MAX_LOCAL_ARTWORK_BYTES}-byte limit."
        ));
    }
    Ok(format!(
        "data:{};base64,{}",
        artwork
            .mime
            .as_deref()
            .unwrap_or("application/octet-stream"),
        BASE64_STANDARD.encode(artwork.bytes)
    ))
}

fn authorized_local_artwork_path(library: &Library, uri: &str) -> Result<Option<PathBuf>, String> {
    if !uri.starts_with("file:") {
        return Ok(None);
    }
    let canonical_uri = library
        .tracks()
        .iter()
        .find(|track| track.uri == uri)
        .map(|track| track.uri.as_str())
        .ok_or_else(|| "Local artwork resource is not in the library.".to_string())?;
    localfiles::path_from_file_uri(canonical_uri).map(Some)
}

pub(crate) async fn publish_media_artwork(app: tauri::AppHandle, event: PlayerStateEvent) {
    let state = app.state::<AppState>();
    let provider = provider_from(&state).ok();
    let local_path = authorized_local_artwork_path(
        &state.library.lock().expect("library mutex poisoned"),
        event.uri.as_deref().unwrap_or_default(),
    )
    .ok()
    .flatten();
    let Ok(Some(url)) = resolve_track_artwork(
        provider.as_deref(),
        &state.artwork_cache,
        Ok(local_path),
        event.uri.as_deref().unwrap_or_default(),
        300,
    )
    .await
    else {
        return;
    };
    state.media_keys.update_artwork(&event, &url);
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn initial_library(debug: bool) -> Library {
    if debug {
        fixture::library()
    } else {
        Library::new()
    }
}

#[cfg(test)]
fn remove_album_tracks(library: &mut Library, album: &Album) -> usize {
    library.remove_uris(&album_track_uris(album))
}

fn import_local_files(app: &tauri::AppHandle) {
    let handle = app.clone();
    app.dialog()
        .file()
        .add_filter(
            "Audio",
            &[
                "aac", "aif", "aiff", "flac", "m4a", "mp3", "mp4", "oga", "ogg", "opus", "wav",
                "webm",
            ],
        )
        .pick_files(move |paths| {
            let Some(paths) = paths else { return };
            let paths = paths
                .into_iter()
                .map(|path| path.into_path().map_err(|error| error.to_string()))
                .collect::<Result<Vec<_>, _>>();
            match paths {
                Ok(paths) => library_commands::launch_local_import(handle.clone(), paths),
                Err(error) => {
                    notify_error(&handle, error);
                    let _ = emit_main(&handle, "local-import-failed", ());
                }
            }
        });
}

fn import_local_folder(app: &tauri::AppHandle) {
    let handle = app.clone();
    app.dialog().file().pick_folder(move |path| {
        let Some(path) = path else { return };
        match path.into_path() {
            Ok(path) => library_commands::launch_local_import(handle.clone(), vec![path]),
            Err(error) => {
                notify_error(&handle, error.to_string());
                let _ = emit_main(&handle, "local-import-failed", ());
            }
        }
    });
}

fn handle_local_drag_event(app: &tauri::AppHandle, label: &str, event: &tauri::WindowEvent) {
    if label != "main" {
        return;
    }
    let tauri::WindowEvent::DragDrop(event) = event else {
        return;
    };
    match event {
        tauri::DragDropEvent::Enter { .. } => {
            let _ = app.emit_to("main", "local-drag-changed", true);
        }
        tauri::DragDropEvent::Drop { paths, .. } => {
            let _ = app.emit_to("main", "local-drag-changed", false);
            if !paths.is_empty() {
                library_commands::launch_local_import(app.clone(), paths.clone());
            }
        }
        tauri::DragDropEvent::Leave => {
            let _ = app.emit_to("main", "local-drag-changed", false);
        }
        tauri::DragDropEvent::Over { .. } => {}
        _ => {}
    }
}

fn install_file_menu(app: &tauri::App, settings: &Settings) -> tauri::Result<MenuChecks> {
    let preferences = MenuItemBuilder::with_id("preferences", "Preferences…")
        .accelerator("CmdOrCtrl+,")
        .build(app)?;
    let app_menu = SubmenuBuilder::new(app, "Retune")
        .item(&preferences)
        .separator()
        .hide()
        .hide_others()
        .show_all()
        .separator()
        .quit()
        .build()?;
    let window_menu = SubmenuBuilder::new(app, "Window")
        .minimize()
        .maximize()
        .separator()
        .close_window()
        .build()?;
    let get_info = MenuItemBuilder::with_id("get_info", "Get Info")
        .accelerator("CmdOrCtrl+I")
        .build(app)?;
    let file = SubmenuBuilder::new(app, "File")
        .text("setup_library", "Set Up Library…")
        .separator()
        .item(&get_info)
        .separator()
        .text("sync_spotify", "Sync from Spotify")
        .separator()
        .text("add_local_files", "Add Local Files…")
        .text("add_local_folder", "Add Local Folder…")
        .separator()
        .text("export_library", "Export Library…")
        .separator()
        .text("restore_library", "Restore Library…")
        .text("merge_library", "Merge Library…")
        .build()?;
    let edit = SubmenuBuilder::new(app, "Edit")
        .undo()
        .redo()
        .separator()
        .cut()
        .copy()
        .paste()
        .select_all()
        .build()?;
    let zoom_in = MenuItemBuilder::with_id("zoom_in", "Zoom In")
        .accelerator("CmdOrCtrl+=")
        .build(app)?;
    let zoom_out = MenuItemBuilder::with_id("zoom_out", "Zoom Out")
        .accelerator("CmdOrCtrl+-")
        .build(app)?;
    let actual_size = MenuItemBuilder::with_id("actual_size", "Actual Size")
        .accelerator("CmdOrCtrl+0")
        .build(app)?;
    let zebra = CheckMenuItemBuilder::with_id("toggle_zebra", "Toggle Zebra Striping")
        .checked(settings.zebra)
        .build(app)?;
    let browser = MenuItemBuilder::with_id("toggle_browser", "Show/Hide Column Browser")
        .accelerator("CmdOrCtrl+B")
        .build(app)?;
    let theme_system = CheckMenuItemBuilder::with_id("theme_system", "System")
        .checked(settings.theme == Theme::System)
        .build(app)?;
    let theme_light = CheckMenuItemBuilder::with_id("theme_light", "Light")
        .checked(settings.theme == Theme::Light)
        .build(app)?;
    let theme_dark = CheckMenuItemBuilder::with_id("theme_dark", "Dark")
        .checked(settings.theme == Theme::Dark)
        .build(app)?;
    let theme = SubmenuBuilder::new(app, "Theme")
        .items(&[&theme_system, &theme_light, &theme_dark])
        .build()?;
    let view = SubmenuBuilder::new(app, "View")
        .items(&[&zoom_in, &zoom_out, &actual_size])
        .separator()
        .item(&theme)
        .item(&browser)
        .item(&zebra)
        .build()?;
    let controls = SubmenuBuilder::new(app, "Controls")
        .text("play_pause", "Play/Pause\tSpace")
        .text("previous", "Previous")
        .text("next", "Next")
        .build()?;
    let connect = MenuItemBuilder::with_id("connect_spotify", "Connect to Spotify…").build(app)?;
    let disconnect = MenuItemBuilder::with_id("disconnect_spotify", "Disconnect").build(app)?;
    let account_status = MenuItemBuilder::with_id("spotify_status", "Not connected")
        .enabled(false)
        .build(app)?;
    let account = SubmenuBuilder::new(app, "Account")
        .items(&[&connect, &disconnect])
        .separator()
        .item(&account_status)
        .build()?;
    let help = SubmenuBuilder::new(app, "Help")
        .text("about_retune", "About Retune")
        .build()?;
    let menu = MenuBuilder::new(app).items(&[
        &app_menu,
        &file,
        &edit,
        &view,
        &controls,
        &account,
        &window_menu,
    ]);
    let menu = menu.item(&help).build()?;
    app.set_menu(menu)?;
    app.on_menu_event(|app, event| match event.id().as_ref() {
        "get_info" => {
            let _ = emit_main(app, "get-info", ());
        }
        "setup_library" => {
            let _ = emit_main(app, "open-setup", ());
        }
        "add_local_files" => import_local_files(app),
        "add_local_folder" => import_local_folder(app),
        "export_library" => backup::export_library(app),
        "restore_library" => backup::import_library(app, true),
        "merge_library" => backup::import_library(app, false),
        "sync_spotify" => {
            let handle = app.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(error) = sync_spotify(&handle).await {
                    notify_error(&handle, error);
                }
            });
        }
        "connect_spotify" => {
            let handle = app.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(error) = spotify_commands::connect_spotify(handle.clone()).await {
                    notify_error(&handle, error);
                }
            });
        }
        "disconnect_spotify" => {
            let handle = app.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(error) = spotify_commands::disconnect_spotify(handle.clone()).await {
                    notify_error(&handle, error);
                }
            });
        }
        "about_retune" => {
            app.dialog()
                .message(format!(
                    "Retune {}\n\nOverlay edits stay local",
                    app.package_info().version
                ))
                .title("About Retune")
                .show(|_| {});
        }
        "preferences" => {
            let _ = emit_main(app, "open-preferences", ());
        }
        "zoom_in" | "zoom_out" | "actual_size" | "toggle_zebra" | "toggle_browser"
        | "theme_system" | "theme_light" | "theme_dark" => {
            let _ = emit_main(app, "view-action", event.id().as_ref());
        }
        "play_pause" | "previous" | "next" => {
            let _ = emit_main(app, "player-action", event.id().as_ref());
        }
        _ => {}
    });
    Ok(MenuChecks {
        zebra,
        theme_system,
        theme_light,
        theme_dark,
        account_status,
        connect,
        disconnect,
    })
}

fn notify_error(app: &tauri::AppHandle, error: String) {
    log::error!("{error}");
    let _ = emit_main_event(app, main_events::MainEvent::OperationError(error));
}

pub(crate) fn emit_main_event<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    event: main_events::MainEvent,
) -> tauri::Result<()> {
    app.state::<AppState>().main_events.send(event)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _, _| {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.show();
            let _ = window.unminimize();
            let _ = window.set_focus();
        }
    }));
    let builder = builder
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init());
    #[cfg(all(desktop, not(test)))]
    let builder = builder.plugin(
        tauri_plugin_window_state::Builder::default()
            .with_denylist(&["lastfm-importer"])
            .with_state_flags(
                tauri_plugin_window_state::StateFlags::SIZE
                    | tauri_plugin_window_state::StateFlags::POSITION
                    | tauri_plugin_window_state::StateFlags::MAXIMIZED,
            )
            .build(),
    );
    let app = builder.invoke_handler(tauri::generate_handler![
            library_commands::browse,
            library_commands::metadata_values,
            library_commands::genre_values,
            library_commands::click_track_star,
            library_commands::set_track_enabled,
            library_commands::set_album_rating,
            library_commands::get_track,
            library_commands::edit_track,
            library_commands::set_track_infos,
            main_events::subscribe_main_events,
            main_events::unsubscribe_main_events,
            settings_commands::get_settings,
            settings_commands::get_appearance,
            settings_commands::update_settings,
            spotify_commands::connection_state,
            spotify_commands::connect_spotify,
            spotify_commands::authorize_spotify_playback,
            spotify_commands::disconnect_spotify,
            spotify_commands::sync_from_spotify,
            spotify_commands::spotify_search,
            spotify_commands::spotify_album_page,
            spotify_commands::spotify_artist_page,
            spotify_commands::spotify_follow_artist,
            spotify_commands::spotify_artist_albums,
            spotify_commands::add_spotify_album,
            spotify_commands::remove_spotify_album,
            spotify_commands::add_spotify_track,
            spotify_commands::add_spotify_tracks,
            spotify_commands::remove_spotify_track,
            playlist_commands::playlists_list,
            playlist_commands::open_spotify_playlist,
            playlist_commands::resolve_spotify_track_destination,
            playlist_commands::reorder_playlists,
            playlist_commands::playlist_unfollow,
            playlist_commands::playlist_create,
            playlist_commands::playlist_tracks,
            playlist_commands::playlist_add,
            playlist_commands::playlist_add_album,
            playlist_commands::playlist_reorder,
            playlist_commands::playlist_remove,
            playback_commands::play_tracks,
            playback_commands::player_toggle,
            playback_commands::player_next,
            playback_commands::player_prev,
            playback_commands::player_seek,
            playback_commands::player_set_volume,
            playback_commands::set_repeat,
            playback_commands::set_shuffle,
            spotify_commands::track_artwork,
            lastfm::lastfm_state,
            lastfm::connect_lastfm,
            finish_lastfm,
            disconnect_lastfm,
            lastfm_import::commands::open_lastfm_importer,
            lastfm_import::commands::lastfm_import_state,
            lastfm_import::commands::lastfm_import_queue,
            lastfm_import::commands::lastfm_import_page,
            lastfm_import::commands::start_lastfm_import,
            lastfm_import::commands::sync_lastfm_plays,
            lastfm_import::commands::lastfm_import_review,
            lastfm_import::commands::lastfm_import_options,
            lastfm_import::commands::lastfm_import_count_mode,
            lastfm_import::commands::lastfm_import_search_terms,
            lastfm_import::commands::lastfm_import_select_match,
            lastfm_import::commands::lastfm_import_select_matches,
            lastfm_import::commands::lastfm_import_collection_search_albums,
            lastfm_import::commands::lastfm_import_collection_preview_album,
            lastfm_import::commands::lastfm_import_collection_add_album,
            lastfm_import::commands::lastfm_import_collection_remove_album,
            lastfm_import::commands::lastfm_import_activate_collection,
            lastfm_import::commands::lastfm_import_change_track,
            lastfm_import::commands::lastfm_import_change_album,
            lastfm_import::commands::lastfm_import_apply,
            lastfm_import::commands::lastfm_import_retry_apply,
            lastfm_import::commands::lastfm_import_prepare_accept_all,
            lastfm_import::commands::lastfm_import_accept_all,
            diagnostics::load_diagnostics,
            diagnostics::email_diagnostics,
            external_links::open_external_destination
        ])
        .setup(|app| {
            app.handle().plugin(
                tauri_plugin_log::Builder::default()
                    .level(log::LevelFilter::Info)
                    .max_file_size(5_000_000)
                    .timezone_strategy(tauri_plugin_log::TimezoneStrategy::UseLocal)
                    .build(),
            )?;
            log::info!(
                target: diagnostics::LOG_TARGET,
                "{}",
                diagnostics::SESSION_START_MARKER
            );
            let app_data_dir = app.path().app_data_dir()?;
            spotify_sync_commit::Store::new(&app_data_dir)
                .recover()
                .map_err(std::io::Error::other)?;
            restore::RestoreStore::new(&app_data_dir)
                .recover()
                .map_err(std::io::Error::other)?;
            let store = FsOverlayStore::new(&app_data_dir);
            let (library, recovery_notice, needs_save) = match store.load() {
                Ok(Some(library)) => (library, None, false),
                Ok(None) => {
                    let library = initial_library(cfg!(debug_assertions));
                    (library, None, true)
                }
                Err(StoreError::Import(error)) => {
                    let corrupt = store.quarantine_corrupt()?;
                    let library = Library::new();
                    (
                        library,
                        Some(format!(
                            "Retune could not load your library ({error}). The corrupt file was moved to {} and an empty library was started.",
                            corrupt.display()
                        )),
                        true,
                    )
                }
                Err(error) => return Err(error.into()),
            };
            if needs_save {
                store.save(&library)?;
            }
            let settings_store = FsSettingsStore::new(&app_data_dir);
            let cooldown_store = FsCooldownStore::new(&app_data_dir);
            let artist_genres_store = FsArtistGenresStore::new(&app_data_dir);
            let spotify_library_store = FsSpotifyLibraryStore::new(&app_data_dir);
            let spotify_library = spotify_library_store.load()?;
            let spotify_catalog_store = FsSpotifyCatalogStore::new(&app_data_dir);
            let spotify_catalog = Arc::new(Mutex::new(SpotifyCatalog::default()));
            let spotify_catalog_saved_generation = Arc::new(AtomicU64::new(0));
            let spotify_catalog_flush_gate = Arc::new(Mutex::new(()));
            let spotify_catalog_hydration_epoch = Arc::new(AtomicU64::new(0));
            let catalog_hydration = spawn_catalog_hydration(
                Arc::clone(&spotify_catalog),
                Arc::clone(&spotify_catalog_saved_generation),
                Arc::clone(&spotify_catalog_flush_gate),
                Arc::clone(&spotify_catalog_hydration_epoch),
                {
                    let store = spotify_catalog_store.clone();
                    move || store.load()
                },
            );
            let playlist_store = FsPlaylistStore::new(&app_data_dir);
            let playlists = playlist_store.load()?;
            let settings = settings_store.load()?.unwrap_or_default();
            settings_store.save(&settings)?;
            let menu_checks = install_file_menu(app, &settings)?;
            // Dev builds keep tokens in a 0600 plaintext file. Release keeps
            // only the encryption key in the native credential store.
            let use_dev_token_store = cfg!(any(debug_assertions, feature = "dev-token-store"));
            let backing: Box<dyn TokenStore> = if use_dev_token_store {
                Box::new(store::FsTokenStore::new(&app_data_dir))
            } else {
                Box::new(EncryptedFsTokenStore::new(&app_data_dir).map_err(std::io::Error::other)?)
            };
            let token_store = Arc::new(CachedTokenStore::new(backing));
            let lastfm_app = app.handle().clone();
            let lastfm = lastfm::Service::new_unhydrated(
                &app_data_dir,
                use_dev_token_store,
                settings.lastfm_scrobbling,
                lastfm::credentials_from(
                    option_env!("RETUNE_LASTFM_API_KEY"),
                    option_env!("RETUNE_LASTFM_SHARED_SECRET"),
                ),
                Arc::new(move |_| {
                    let _ = emit_main(&lastfm_app, "lastfm-changed", ());
                }),
            );
            let restore_mutations = Arc::new(restore_latch::RestoreMutationState::default());
            let lastfm_import = lastfm_import::Service::new_unhydrated_with_restore_state(
                &app_data_dir,
                Arc::clone(&restore_mutations),
            );
            let connection = ConnectionState::from_tokens(None);
            menu_checks.sync_connection(&connection)?;
            let spotify = spotify_provider(
                &settings.spotify_client_id,
                Arc::clone(&token_store),
                Arc::clone(&spotify_catalog),
            )
                .map_err(std::io::Error::other)?;
            let startup_client_id = settings.spotify_client_id.clone();
            let startup_auto_connect = settings.auto_connect;
            let startup_last_full_sync = settings.last_full_sync;
            let startup_backend = settings.playback_backend;
            let initial_volume = settings.volume;
            let playback = Arc::new(Playback::new(
                settings.repeat,
                settings.shuffle,
                settings.play_threshold_percent,
                AudioSettings {
                    bitrate: settings.streaming_bitrate,
                    normalize: settings.normalize_volume,
                    gapless: settings.gapless,
                },
                Some(app_data_dir.clone()),
            ));
            playback.set_requested_backend(settings.playback_backend);
            let media_control_app = app.handle().clone();
            let media_keys = media_keys::MediaKeys::spawn(app.handle(), move |control| {
                let app = media_control_app.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(error) = playback_commands::handle_media_control(&app, control).await
                    {
                        notify_error(&app, error);
                    }
                });
            });
            let lastfm_enabled = settings.lastfm_scrobbling;
            let lastfm_import_startup = Arc::clone(&lastfm_import);
            let playback_effects =
                PlaybackEffects::start(app.handle().clone(), Arc::clone(&lastfm));
            app.manage(AppState {
                library: LibraryState::new_with_restore_state(
                    library,
                    store,
                    Arc::clone(&restore_mutations),
                ),
                spotify_membership: spotify_membership::SpotifyMembership::new_with_restore_state(
                    spotify_library,
                    spotify_library_store,
                    Arc::clone(&restore_mutations),
                ),
                settings: SettingsState::new_with_restore_state(
                    settings,
                    settings_store,
                    Arc::clone(&restore_mutations),
                ),
                cooldown_store,
                artist_genres_store,
                playlists: PlaylistState::new_with_restore_state(
                    playlists,
                    playlist_store,
                    Arc::clone(&restore_mutations),
                ),
                menu_checks: Some(menu_checks),
                main_events: main_events::MainEventSink::new(recovery_notice),
                token_store,
                spotify_catalog,
                spotify_catalog_store,
                spotify_catalog_saved_generation,
                spotify_catalog_flush_gate,
                spotify_catalog_hydration_epoch,
                spotify: Mutex::new(spotify),
                artwork_cache: Mutex::default(),
                playback: Arc::clone(&playback),
                playback_effects,
                lastfm: Arc::clone(&lastfm),
                lastfm_import,
                media_keys,
                spotify_session: spotify_commands::SpotifySession::default(),
                sync_orchestrator: SyncOrchestrator::default(),
                catalog_flush_task: Mutex::new(None),
                playlist_reauth_notified: AtomicBool::new(false),
                local_import_active: Arc::new(AtomicBool::new(false)),
                restore_mutations,
                shutdown_state: AtomicU8::new(0),
            });
            let catalog_app = app.handle().clone();
            let catalog_task = tauri::async_runtime::spawn(async move {
                match catalog_hydration.await {
                    Ok(Ok(true)) => {
                        let _ = emit_main(&catalog_app, "library-changed", ());
                    }
                    Ok(Ok(false)) => {
                        log::debug!("Skipped stale Spotify catalog hydration");
                    }
                    Ok(Err(error)) => notify_error(
                        &catalog_app,
                        format!("Could not load the Spotify catalog: {error}"),
                    ),
                    Err(error) => notify_error(
                        &catalog_app,
                        format!("Spotify catalog hydration failed: {error}"),
                    ),
                }
                let mut interval = tokio::time::interval(Duration::from_secs(30));
                interval.tick().await;
                loop {
                    interval.tick().await;
                    let flush_app = catalog_app.clone();
                    match tauri::async_runtime::spawn_blocking(move || {
                        let state = flush_app.state::<AppState>();
                        flush_spotify_catalog(&state)
                    })
                    .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            log::warn!("Could not persist Spotify catalog: {error}");
                        }
                        Err(error) => {
                            log::warn!("Spotify catalog persistence task failed: {error}");
                        }
                    }
                }
            });
            *app
                .state::<AppState>()
                .catalog_flush_task
                .lock()
                .expect("catalog flush-task mutex poisoned") = Some(catalog_task);
            let backfill_app = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let event_app = backfill_app.clone();
                match tauri::async_runtime::spawn_blocking(move || {
                    backfill_app
                        .state::<AppState>()
                        .library
                        .mutate(|library| Ok(localfiles::backfill_metadata(library)))
                })
                .await
                {
                    Ok(Ok(true)) => {
                        let _ = emit_main(&event_app, "library-changed", ());
                    }
                    Ok(Ok(false)) => {}
                    Ok(Err(error)) => notify_error(&event_app, error),
                    Err(error) => notify_error(&event_app, error.to_string()),
                }
            });
            let lastfm_startup = Arc::clone(&lastfm);
            let profile_app = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let import_hydration = async {
                    let result = lastfm_import_startup.hydrate().await;
                    if result.is_ok() {
                        let _ = profile_app.emit_to("main", "lastfm-import-changed", ());
                        let _ = profile_app.emit_to(
                            "lastfm-importer",
                            "lastfm-import-changed",
                            (),
                        );
                    }
                    result
                };
                let (lastfm_result, import_result) =
                    tokio::join!(lastfm_startup.hydrate(), import_hydration);
                if let Err(error) = &lastfm_result {
                    notify_error(&profile_app, error.clone());
                }
                if let Err(error) = &import_result {
                    notify_error(&profile_app, error.clone());
                }
                lastfm_startup.set_enabled(lastfm_enabled).await;
                let _ = settings_commands::set_lastfm_scrobbling(&profile_app, lastfm_enabled).await;
                if import_result.is_ok() {
                    lastfm_import::resume_persisted_import(profile_app.clone()).await;
                    lastfm_import::resume_persisted_apply(profile_app.clone()).await;
                    let _ = lastfm_import_startup.backfill_completed_mappings().await;
                    if lastfm_result.is_ok() && lastfm_startup.state().await.connected {
                        let _ =
                            lastfm_import::commands::sync_lastfm_plays(profile_app.clone()).await;
                    }
                }
            });
            let provider_app = app.handle().clone();
            let playback_effect_app = app.handle().clone();
            playback.listen(
                move || provider_from(&provider_app.state::<AppState>()),
                move |effect| playback_commands::execute_effect(&playback_effect_app, effect),
            );
            {
                let handle = app.handle().clone();
                let credential_dir = app_data_dir.clone();
                tauri::async_runtime::spawn(async move {
                    let load_handle = handle.clone();
                    let loaded = load_startup_credentials(move || {
                        match load_handle.state::<AppState>().token_store.load() {
                            Ok(tokens) => Ok((ConnectionState::from_tokens(tokens), None)),
                            Err(retune_spotify::Error::TokenStoreCorrupt(error)) => {
                                let corrupt = quarantine_token_file(
                                    &credential_dir,
                                    use_dev_token_store,
                                    unix_now(),
                                )
                                .map_err(|error| error.to_string())?;
                                Ok((
                                    ConnectionState::from_tokens(None),
                                    Some(format!(
                                        "Retune could not load your Spotify credentials ({error}). The damaged file was moved to {}. Reconnect Spotify to continue.",
                                        corrupt.display()
                                    )),
                                ))
                            }
                            Err(error) => {
                                log::warn!("Token store unavailable at startup: {error}");
                                Ok((ConnectionState::from_tokens(None), None))
                            }
                        }
                    })
                    .await;
                    let (connection, notice) = match loaded {
                        Ok(Ok(loaded)) => loaded,
                        Ok(Err(error)) => {
                            notify_error(&handle, error);
                            (ConnectionState::from_tokens(None), None)
                        }
                        Err(error) => {
                            notify_error(&handle, error.to_string());
                            (ConnectionState::from_tokens(None), None)
                        }
                    };
                    if let Some(notice) = notice {
                        log::warn!("{notice}");
                        let _ = emit_main_event(
                            &handle,
                            main_events::MainEvent::StartupNotice(notice),
                        );
                    }
                    if let Err(error) = emit_connection_state_async(&handle).await {
                        notify_error(&handle, error);
                    }
                    let startup_action = startup_action(
                        &connection,
                        &startup_client_id,
                        startup_auto_connect,
                        startup_last_full_sync,
                        unix_now(),
                    );
                    let activate_local = connection.connected
                        && connection.playback_authorized
                        && startup_backend == PlaybackBackend::Local;
                    if activate_local {
                        let state = handle.state::<AppState>();
                        if let Err(error) = switch_to_local(&state, initial_volume).await {
                            notify_error(&handle, error);
                        }
                    }
                    let result = match startup_action {
                        StartupAction::Sync => sync_spotify(&handle).await,
                        StartupAction::Connect => spotify_commands::connect_spotify(handle.clone()).await,
                        StartupAction::Nothing => match provider_from(&handle.state::<AppState>()) {
                            Ok(client) => {
                                playlist_commands::sync_playlists(&handle, client.as_ref()).await
                            }
                            Err(error) => Err(error),
                        },
                    };
                    if let Err(error) = result {
                        notify_error(&handle, error);
                    }
                });
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while running tauri application");
    let mut ready = false;
    app.run(move |app, event| match event {
        tauri::RunEvent::Ready => ready = true,
        tauri::RunEvent::WindowEvent { label, event, .. } => {
            handle_local_drag_event(app, &label, &event);
        }
        tauri::RunEvent::Resumed if ready => {
            let playback = Arc::clone(&app.state::<AppState>().playback);
            tauri::async_runtime::spawn(async move {
                playback.invalidate_local().await;
            });
        }
        tauri::RunEvent::ExitRequested { api, .. } => {
            let state = app.state::<AppState>();
            match exit_action(&state.shutdown_state) {
                ExitAction::Allow => return,
                ExitAction::WaitForDrain => {
                    api.prevent_exit();
                    return;
                }
                ExitAction::StartDrain => api.prevent_exit(),
            }
            let handle = app.clone();
            tauri::async_runtime::spawn(async move {
                let state = handle.state::<AppState>();
                let catalog_task = state
                    .catalog_flush_task
                    .lock()
                    .expect("catalog flush-task mutex poisoned")
                    .take();
                if let Some(task) = catalog_task {
                    task.abort();
                }
                state.sync_orchestrator.cancel_retry();
                if tokio::time::timeout(Duration::from_secs(3), state.playback_effects.shutdown())
                    .await
                    .is_err()
                {
                    log::warn!("Timed out draining playback persistence at exit");
                }
                state.lastfm.shutdown().await;
                let flush_handle = handle.clone();
                match tauri::async_runtime::spawn_blocking(move || {
                    flush_spotify_catalog(&flush_handle.state::<AppState>())
                })
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        log::warn!("Could not persist Spotify catalog at exit: {error}")
                    }
                    Err(error) => log::warn!("Spotify catalog exit task failed: {error}"),
                }
                state.shutdown_state.store(2, Ordering::Release);
                handle.exit(0);
            });
        }
        _ => {}
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StartupAction {
    Sync,
    Connect,
    Nothing,
}

fn startup_action(
    connection: &ConnectionState,
    client_id: &str,
    auto_connect: bool,
    last_full_sync: Option<u64>,
    now: u64,
) -> StartupAction {
    if connection.connected {
        if last_full_sync.is_some_and(|last| now.saturating_sub(last) <= 15 * 60) {
            StartupAction::Nothing
        } else {
            StartupAction::Sync
        }
    } else if auto_connect && !client_id.trim().is_empty() {
        StartupAction::Connect
    } else {
        StartupAction::Nothing
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    #[tokio::test]
    async fn playback_effect_shutdown_drains_queued_work_and_is_idempotent() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(2);
        let completed = Arc::new(AtomicBool::new(false));
        let worker_completed = Arc::clone(&completed);
        let task = tauri::async_runtime::spawn(async move {
            while let Some(effect) = receiver.recv().await {
                match effect {
                    PlaybackDurableEffect::RecordPlay(uri) => {
                        assert_eq!(uri, "file:///song.mp3");
                        worker_completed.store(true, Ordering::Release);
                    }
                    PlaybackDurableEffect::Shutdown(done) => {
                        let _ = done.send(());
                        break;
                    }
                    PlaybackDurableEffect::Listening(_) => unreachable!(),
                }
            }
        });
        let effects = PlaybackEffects {
            sender: Mutex::new(Some(sender)),
            task: Mutex::new(Some(task)),
        };

        effects
            .submit(PlaybackDurableEffect::RecordPlay("file:///song.mp3".into()))
            .unwrap();
        effects.shutdown().await;
        effects.shutdown().await;
        assert!(completed.load(Ordering::Acquire));
        assert!(effects
            .submit(PlaybackDurableEffect::RecordPlay("file:///late.mp3".into()))
            .is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn delayed_credential_store_does_not_block_initializing_ui_work() {
        let started = Arc::new(tokio::sync::Notify::new());
        let (release, released) = std::sync::mpsc::channel();
        let load = {
            let started = Arc::clone(&started);
            tokio::spawn(async move {
                load_startup_credentials(move || {
                    started.notify_one();
                    released.recv().unwrap();
                    ConnectionState::from_tokens(None)
                })
                .await
            })
        };
        started.notified().await;

        tokio::time::timeout(
            Duration::from_millis(100),
            tokio::time::sleep(Duration::from_millis(1)),
        )
        .await
        .expect("delayed credential storage must not block initializing UI work");
        release.send(()).unwrap();
        assert!(!load.await.unwrap().unwrap().connected);
    }

    #[test]
    fn catalog_hydration_starts_without_a_current_tokio_runtime() {
        assert!(tokio::runtime::Handle::try_current().is_err());
        let hydration = spawn_catalog_hydration(
            Arc::new(Mutex::new(SpotifyCatalog::default())),
            Arc::new(AtomicU64::new(0)),
            Arc::new(Mutex::new(())),
            Arc::new(AtomicU64::new(0)),
            || Ok::<_, std::io::Error>(SpotifyCatalog::default()),
        );

        assert!(tauri::async_runtime::block_on(hydration).unwrap().unwrap());
    }

    #[tokio::test]
    async fn catalog_hydration_is_non_blocking_stale_safe_and_reports_load_failure() {
        fn catalog(uri: &str) -> SpotifyCatalog {
            let mut catalog = SpotifyCatalog::default();
            catalog.observe_track(&SpotifyTrack {
                uri: uri.into(),
                name: uri.into(),
                duration_ms: Some(1_000),
                track_number: Some(1),
                disc_number: Some(1),
                artists: vec![],
                album: None,
            });
            catalog
        }

        let current = Arc::new(Mutex::new(SpotifyCatalog::default()));
        let saved = Arc::new(AtomicU64::new(0));
        let gate = Arc::new(Mutex::new(()));
        let epoch = Arc::new(AtomicU64::new(0));
        let (entered, started) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let hydration = spawn_catalog_hydration(
            Arc::clone(&current),
            Arc::clone(&saved),
            Arc::clone(&gate),
            Arc::clone(&epoch),
            move || {
                entered.send(()).unwrap();
                released.recv().unwrap();
                Ok::<_, std::io::Error>(catalog("spotify:track:loaded"))
            },
        );
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(current.lock().unwrap().counts().tracks, 0);
        release.send(()).unwrap();
        assert!(hydration.await.unwrap().unwrap());
        assert!(current
            .lock()
            .unwrap()
            .complete_track("spotify:track:loaded")
            .is_some());
        assert_eq!(
            saved.load(Ordering::Acquire),
            current.lock().unwrap().generation()
        );

        let (entered, started) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let hydration = spawn_catalog_hydration(
            Arc::clone(&current),
            Arc::clone(&saved),
            Arc::clone(&gate),
            Arc::clone(&epoch),
            move || {
                entered.send(()).unwrap();
                released.recv().unwrap();
                Ok::<_, std::io::Error>(catalog("spotify:track:stale"))
            },
        );
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        current.lock().unwrap().observe_track(&SpotifyTrack {
            uri: "spotify:track:current".into(),
            name: "Current".into(),
            duration_ms: Some(1_000),
            track_number: Some(1),
            disc_number: Some(1),
            artists: vec![],
            album: None,
        });
        release.send(()).unwrap();
        assert!(!hydration.await.unwrap().unwrap());
        {
            let snapshot = current.lock().unwrap();
            assert!(snapshot.complete_track("spotify:track:current").is_some());
            assert!(snapshot.complete_track("spotify:track:stale").is_none());
        }

        let failed = spawn_catalog_hydration(
            Arc::clone(&current),
            Arc::clone(&saved),
            gate,
            epoch,
            || Err::<SpotifyCatalog, _>(std::io::Error::other("broken")),
        )
        .await
        .unwrap();
        assert_eq!(failed.unwrap_err().to_string(), "broken");
        assert!(current
            .lock()
            .unwrap()
            .complete_track("spotify:track:current")
            .is_some());
    }

    #[tokio::test]
    async fn playback_effect_queue_full_is_reported() {
        let (sender, _receiver) = tokio::sync::mpsc::channel(1);
        let effects = PlaybackEffects {
            sender: Mutex::new(Some(sender)),
            task: Mutex::new(None),
        };
        effects
            .submit(PlaybackDurableEffect::RecordPlay("file:///one.mp3".into()))
            .unwrap();
        assert_eq!(
            effects
                .submit(PlaybackDurableEffect::RecordPlay("file:///two.mp3".into()))
                .unwrap_err(),
            "Playback persistence queue is full."
        );
    }

    #[test]
    fn repeated_exit_requests_wait_until_the_drain_is_complete() {
        let state = AtomicU8::new(0);
        assert_eq!(exit_action(&state), ExitAction::StartDrain);
        assert_eq!(exit_action(&state), ExitAction::WaitForDrain);
        state.store(2, Ordering::Release);
        assert_eq!(exit_action(&state), ExitAction::Allow);
    }

    #[tokio::test]
    async fn successful_submit_is_always_ordered_before_shutdown() {
        for _ in 0..32 {
            let (sender, mut receiver) = tokio::sync::mpsc::channel(2);
            let processed = Arc::new(AtomicBool::new(false));
            let worker_processed = Arc::clone(&processed);
            let task = tauri::async_runtime::spawn(async move {
                while let Some(effect) = receiver.recv().await {
                    match effect {
                        PlaybackDurableEffect::RecordPlay(_) => {
                            worker_processed.store(true, Ordering::Release)
                        }
                        PlaybackDurableEffect::Shutdown(done) => {
                            let _ = done.send(());
                            break;
                        }
                        PlaybackDurableEffect::Listening(_) => unreachable!(),
                    }
                }
            });
            let effects = Arc::new(PlaybackEffects {
                sender: Mutex::new(Some(sender)),
                task: Mutex::new(Some(task)),
            });
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let submit = {
                let effects = Arc::clone(&effects);
                let barrier = Arc::clone(&barrier);
                tauri::async_runtime::spawn_blocking(move || {
                    barrier.wait();
                    effects.submit(PlaybackDurableEffect::RecordPlay("file:///song.mp3".into()))
                })
            };
            let shutdown = {
                let effects = Arc::clone(&effects);
                let barrier = Arc::clone(&barrier);
                tauri::async_runtime::spawn(async move {
                    tauri::async_runtime::spawn_blocking(move || barrier.wait())
                        .await
                        .unwrap();
                    effects.shutdown().await;
                })
            };
            let submitted = submit.await.unwrap().is_ok();
            shutdown.await.unwrap();
            assert_eq!(processed.load(Ordering::Acquire), submitted);
        }
    }

    #[derive(Default)]
    struct RecordingOverlayStore(Mutex<Vec<Library>>);

    impl OverlayStore for RecordingOverlayStore {
        fn load(&self) -> store::StoreResult<Option<Library>> {
            Ok(None)
        }

        fn save(&self, library: &Library) -> store::StoreResult<()> {
            self.0
                .lock()
                .expect("recording store mutex poisoned")
                .push(library.clone());
            Ok(())
        }
    }

    struct RejectingOverlayStore;

    impl OverlayStore for RejectingOverlayStore {
        fn load(&self) -> store::StoreResult<Option<Library>> {
            Ok(None)
        }

        fn save(&self, _library: &Library) -> store::StoreResult<()> {
            Err(store::StoreError::Io(std::io::Error::other(
                "save rejected",
            )))
        }
    }

    use retune_core::model::NewTrack;
    use retune_spotify::{
        auth,
        client::{
            fake_client, FakeTransport, Image, Page, Response, SimplifiedArtist, SpotifyClient,
            Track,
        },
        tokens::{InMemoryTokenStore, Tokens},
    };

    use super::*;
    use crate::backup::{
        commit_restore, export_with_settings, export_with_settings_and_mappings,
        import_with_settings, import_with_settings_and_mappings, refresh_completed_restore_with,
        ExportSettings,
    };
    use crate::store::{BrowserPanes, LastFmScrobblingProfile};

    #[test]
    fn failed_library_candidate_save_does_not_swap_live_memory() {
        let mut live = Library::new();
        live.add(metadata_track(
            "spotify:track:existing",
            "Rock",
            "Artist",
            "Album",
        ));
        let before = retune_core::io::export_json(&live);
        let mut candidate = live.clone();
        candidate.add(metadata_track(
            "spotify:track:new",
            "Rock",
            "Artist",
            "Album",
        ));

        assert!(commit_library_candidate(&RejectingOverlayStore, &mut live, candidate).is_err());
        assert_eq!(retune_core::io::export_json(&live), before);
    }

    #[test]
    fn corrupt_token_quarantine_preserves_the_damaged_file() {
        for (use_dev_store, name) in [(true, "dev-tokens.json"), (false, "tokens.enc")] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join(name);
            fs::write(&path, b"damaged").unwrap();

            let quarantined = quarantine_token_file(directory.path(), use_dev_store, 42).unwrap();

            assert!(!path.exists());
            assert_eq!(
                quarantined.file_name().unwrap().to_string_lossy(),
                format!("{name}.corrupt-42")
            );
            assert_eq!(fs::read(quarantined).unwrap(), b"damaged");
        }
    }

    #[test]
    fn spotify_album_id_accepts_only_strict_album_uris() {
        assert_eq!(album_id("spotify:album:album"), Some("album"));
        assert_eq!(album_id("spotify:album:"), None);
        assert_eq!(album_id("spotify:album:album:extra"), None);
        assert_eq!(album_id("spotify:track:track"), None);
        assert_eq!(album_id("file:///tmp/album"), None);
    }

    fn shared_token_store(tokens: Option<Tokens>) -> SharedTokenStore {
        Arc::new(CachedTokenStore::new(
            Box::new(InMemoryTokenStore::new(tokens)) as Box<dyn TokenStore>,
        ))
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
    fn record_play_updates_known_uri_once_and_ignores_unknown_uri() {
        let mut library = Library::new();
        let id = library.add(metadata_track(
            "spotify:track:track",
            "Genre",
            "Artist",
            "Album",
        ));
        library.merge_history_absolute("spotify:track:track", Some(3), None, None);
        let library = Mutex::new(library);
        let store = RecordingOverlayStore::default();
        let write_gate = Mutex::new(());
        let transaction = LibraryTransactionState::default();
        let restore_mutations = restore_latch::RestoreMutationState::default();

        assert!(record_play(
            &store,
            &library,
            &write_gate,
            &transaction,
            &restore_mutations,
            "spotify:track:track",
            123,
        )
        .unwrap());
        assert!(!record_play(
            &store,
            &library,
            &write_gate,
            &transaction,
            &restore_mutations,
            "spotify:track:missing",
            456,
        )
        .unwrap());

        let current = library.lock().unwrap();
        let track = current.get(id).unwrap();
        assert_eq!(track.play_count, 4);
        assert_eq!(track.last_played_at, Some(123));
        let saves = store.0.lock().unwrap();
        assert_eq!(saves.len(), 1);
        assert_eq!(saves[0], *current);
    }

    #[test]
    fn record_play_waits_for_library_transaction_and_wakes() {
        let directory = tempfile::tempdir().unwrap();
        let mut library = Library::new();
        let id = library.add(metadata_track(
            "spotify:track:track",
            "Genre",
            "Artist",
            "Album",
        ));
        let library = Arc::new(LibraryState::new(
            library,
            FsOverlayStore::new(directory.path()),
        ));
        let transaction = library.begin_transaction().unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn({
            let library = Arc::clone(&library);
            move || {
                started_tx.send(()).unwrap();
                let result = library.record_play("spotify:track:track", 123);
                finished_tx.send(result).unwrap();
            }
        });
        started_rx.recv().unwrap();
        assert!(finished_rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(transaction);
        assert!(finished_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap());
        worker.join().unwrap();

        assert_eq!(library.lock().unwrap().get(id).unwrap().play_count, 1);
        assert_eq!(
            FsOverlayStore::new(directory.path())
                .load()
                .unwrap()
                .unwrap(),
            *library.lock().unwrap()
        );
    }

    fn spotify_album() -> Album {
        Album {
            id: "album".into(),
            uri: "spotify:album:album".into(),
            name: "Album".into(),
            artists: vec![SimplifiedArtist {
                id: "artist".into(),
                name: "Artist".into(),
            }],
            images: vec![Image {
                url: "cover".into(),
                width: Some(300),
            }],
            release_date: Some("2024-02-03".into()),
            album_type: Some("compilation".into()),
            total_tracks: 2,
            tracks: Some(Page {
                items: vec![
                    Track {
                        uri: "spotify:track:one".into(),
                        name: "One".into(),
                        duration_ms: Some(1_500),
                        track_number: Some(1),
                        disc_number: Some(1),
                        artists: vec![],
                        album: None,
                    },
                    Track {
                        uri: "spotify:track:two".into(),
                        name: "Two".into(),
                        duration_ms: Some(2_500),
                        track_number: Some(2),
                        disc_number: Some(1),
                        artists: vec![],
                        album: None,
                    },
                ],
                next: None,
                skipped: 0,
                total: 2,
            }),
        }
    }

    fn playlist_cache() -> playlists::PlaylistCache {
        playlists::PlaylistCache {
            playlists: vec![playlists::CachedPlaylist {
                id: "playlist".into(),
                name: "Playlist".into(),
                snapshot_id: "old".into(),
                owned: true,
                owner: None,
                track_count: 2,
                tracks: vec!["one".into(), "two".into()],
                track_metadata_version: playlists::TRACK_METADATA_VERSION,
                spotify_tracks: vec![],
            }],
        }
    }

    #[tokio::test]
    async fn playlist_sync_and_later_mutation_commit_in_gate_order() {
        let directory = tempfile::tempdir().unwrap();
        let state = Arc::new(PlaylistState::new(
            playlist_cache(),
            FsPlaylistStore::new(directory.path()),
        ));

        let (sync_guard, mut synced) = state.begin_mutation().await.unwrap();
        synced.playlists[0].name = "Synced".into();

        let (mutation_started, mutation_waiting) = tokio::sync::oneshot::channel();
        let mutation = {
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                mutation_started.send(()).unwrap();
                let (operation, mut next) = state.begin_mutation().await.unwrap();
                let mut created = next.playlists[0].clone();
                created.id = "created".into();
                created.name = "Created".into();
                next.playlists.push(created);
                state.commit(operation, next, true).await.unwrap();
            })
        };

        mutation_waiting.await.unwrap();
        state.commit(sync_guard, synced, false).await.unwrap();
        mutation.await.unwrap();

        let final_cache = state.snapshot().unwrap();
        assert_eq!(final_cache.playlists[0].name, "Synced");
        assert!(final_cache
            .playlists
            .iter()
            .any(|playlist| playlist.id == "created"));
        assert_eq!(
            FsPlaylistStore::new(directory.path()).load().unwrap(),
            final_cache
        );
    }

    #[tokio::test]
    async fn failed_playlist_save_makes_old_memory_non_authoritative() {
        let directory = tempfile::tempdir().unwrap();
        let before = playlist_cache();
        let store = FsPlaylistStore::new(directory.path());
        store.save(&before).unwrap();
        let control = store.clone();
        let state = PlaylistState::new(before.clone(), store);
        let hook = store::SaveHook::new(true);
        control.arm_save(Arc::clone(&hook));
        let mut next = before.clone();
        next.playlists[0].name = "Changed".into();

        let (operation, _) = state.begin_mutation().await.unwrap();
        let failed = state.commit(operation, next, true);
        let release = std::thread::spawn(move || {
            hook.wait_until_reached();
            hook.release();
        });
        assert!(failed.await.is_err());
        release.join().unwrap();
        assert!(state.snapshot().is_err());
        assert!(state.begin_mutation().await.is_err());
        let (_guard, refresh_base) = state.begin_sync().await.unwrap();
        assert_eq!(refresh_base, playlists::PlaylistCache::default());
        assert_eq!(
            FsPlaylistStore::new(directory.path()).load().unwrap(),
            before
        );
    }

    fn playlist_client(
        responses: impl IntoIterator<Item = Response>,
    ) -> SpotifyClient<FakeTransport, InMemoryTokenStore> {
        fake_client(responses, &auth::SCOPES)
    }

    #[tokio::test]
    async fn artist_albums_reports_persisted_quota_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsCooldownStore::new(dir.path());
        store
            .save_cooldowns(&BTreeMap::from([(
                "/artists".into(),
                store::Cooldown {
                    kind: store::CooldownKind::Quota,
                    deadline: 200,
                },
            )]))
            .unwrap();
        let client = playlist_client([]);

        let error = artist_albums_outcome(&client, &store, "artist", 0, 100, chrono::Local::now())
            .await
            .unwrap_err();

        assert!(error.contains("quota is still exhausted"));
        assert!(client.transport().requests().is_empty());
        assert_eq!(
            store.cooldowns(100).unwrap()["/artists"].kind,
            store::CooldownKind::Quota
        );
    }

    #[tokio::test]
    async fn artist_albums_persists_live_quota_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsCooldownStore::new(dir.path());
        let client = playlist_client([Response::quota_exceeded(Some(120))]);

        let error = artist_albums_outcome(&client, &store, "artist", 0, 100, chrono::Local::now())
            .await
            .unwrap_err();

        assert!(error.contains("quota is exhausted; try artist albums again"));
        assert!(!error.contains("still exhausted"));
        assert_eq!(
            store.cooldowns(100).unwrap()["/artists"],
            store::Cooldown {
                kind: store::CooldownKind::Quota,
                deadline: 220,
            }
        );
    }

    #[tokio::test]
    async fn artist_albums_does_not_invent_live_quota_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsCooldownStore::new(dir.path());
        let client = playlist_client([Response::quota_exceeded(None)]);

        let error = artist_albums_outcome(&client, &store, "artist", 0, 100, chrono::Local::now())
            .await
            .unwrap_err();

        assert!(error.contains("after Spotify resets it"));
        assert!(store.cooldowns(100).unwrap().is_empty());
    }

    #[test]
    fn partial_import_message_covers_the_variant_matrix() {
        let now = chrono::Local::now();
        let time = provider::format_resume_time(90, now);
        let detail = "2 of 5 albums";
        assert_eq!(
            partial_import_message("", true, None, now),
            "Partial import — Spotify Development Mode quota is exhausted; sync again after Spotify resets it."
        );
        assert_eq!(
            partial_import_message(detail, true, None, now),
            format!(
                "Partial import ({detail}) — Spotify Development Mode quota is exhausted; sync again after Spotify resets it."
            )
        );
        assert_eq!(
            partial_import_message("", false, None, now),
            "Partial import (Spotify rate limit) — run File → Sync later to finish."
        );
        assert_eq!(
            partial_import_message(detail, false, None, now),
            format!("Partial import ({detail}) — run File → Sync later to finish.")
        );
        assert_eq!(
            partial_import_message("", true, Some(90), now),
            format!(
                "Partial import (Spotify Development Mode quota) — will finish automatically after {time}."
            )
        );
        assert_eq!(
            partial_import_message(detail, true, Some(90), now),
            format!(
                "Partial import (Spotify Development Mode quota) — {detail} — will finish automatically after {time}."
            )
        );
        assert_eq!(
            partial_import_message("", false, Some(90), now),
            format!("Partial import — will finish automatically after {time}.")
        );
        assert_eq!(
            partial_import_message(detail, false, Some(90), now),
            format!("Partial import — {detail} — will finish automatically after {time}.")
        );
    }

    #[tokio::test]
    async fn track_artwork_resolves_requested_sizes_and_caches_each() {
        let track = serde_json::json!({
            "uri": "spotify:track:track",
            "name": "Track",
            "album": {
                "id": "album",
                "uri": "spotify:album:album",
                "name": "Album",
                "images": [
                    {"url": "large", "width": 300},
                    {"url": "small", "width": 64},
                    {"url": "tiny", "width": 63}
                ]
            }
        });
        let client = playlist_client([
            Response::json(200, track.clone()),
            Response::json(
                200,
                serde_json::json!({"uri": "spotify:track:missing", "name": "Missing"}),
            ),
        ]);
        let cache = Mutex::default();

        assert_eq!(track_id("spotify:track:track"), Some("track"));
        assert_eq!(track_id("spotify:album:album"), None);
        assert_eq!(track_id("spotify:track:"), None);
        assert_eq!(track_id("spotify:track:../albums"), None);
        assert_eq!(
            resolve_track_artwork(Some(&client), &cache, Ok(None), "spotify:track:track", 64)
                .await
                .unwrap()
                .as_deref(),
            Some("small")
        );
        assert_eq!(
            resolve_track_artwork(Some(&client), &cache, Ok(None), "spotify:track:track", 64)
                .await
                .unwrap(),
            Some("small".into())
        );
        assert_eq!(
            resolve_track_artwork(Some(&client), &cache, Ok(None), "spotify:track:track", 300)
                .await
                .unwrap(),
            Some("large".into())
        );
        assert_eq!(
            resolve_track_artwork(Some(&client), &cache, Ok(None), "spotify:album:album", 64)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            resolve_track_artwork(Some(&client), &cache, Ok(None), "spotify:track:missing", 64)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            resolve_track_artwork(Some(&client), &cache, Ok(None), "spotify:track:missing", 64)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            resolve_track_artwork(
                Some(&client),
                &cache,
                Ok(None),
                "spotify:track:../albums",
                64
            )
            .await
            .unwrap(),
            None
        );
        assert_eq!(client.transport().requests().len(), 2);
    }

    #[tokio::test]
    async fn local_track_artwork_reads_tags_and_caches_missing_files() {
        let cache = Mutex::default();
        let dir = tempfile::tempdir().unwrap();
        let tagged = dir.path().join("tagged.mp3");
        let tagged_fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../crates/retune-audio/tests/fixtures/cc0-audio-tagged.mp3");
        fs::copy(&tagged_fixture, &tagged).unwrap();
        let uri = localfiles::file_uri(&tagged.canonicalize().unwrap());

        let artwork = resolve_track_artwork(
            None::<&SpotifyClient<FakeTransport, InMemoryTokenStore>>,
            &cache,
            Ok(Some(tagged.clone())),
            &uri,
            64,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(artwork.starts_with("data:image/png;base64,"));
        let encoded = artwork.split_once(',').unwrap().1;
        assert_eq!(
            BASE64_STANDARD.decode(encoded).unwrap(),
            retune_audio::read_tags(&tagged_fixture)
                .unwrap()
                .artwork
                .unwrap()
                .bytes
        );
        fs::remove_file(&tagged).unwrap();
        assert_eq!(
            resolve_track_artwork(
                None::<&SpotifyClient<FakeTransport, InMemoryTokenStore>>,
                &cache,
                Ok(Some(tagged.clone())),
                &uri,
                64
            )
            .await
            .unwrap(),
            Some(artwork)
        );

        let wav = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../crates/retune-audio/tests/fixtures/cc0-audio.wav");
        assert!(resolve_track_artwork(
            None::<&SpotifyClient<FakeTransport, InMemoryTokenStore>>,
            &cache,
            Ok(Some(wav.clone())),
            &localfiles::file_uri(&wav),
            64
        )
        .await
        .is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn slow_local_artwork_read_does_not_block_the_async_worker() {
        let cache = Arc::new(Mutex::default());
        let started = Arc::new(tokio::sync::Notify::new());
        let (release, released) = std::sync::mpsc::channel();
        let resolver = {
            let cache = Arc::clone(&cache);
            let started = Arc::clone(&started);
            tokio::spawn(async move {
                resolve_track_artwork_with(
                    None::<&SpotifyClient<FakeTransport, InMemoryTokenStore>>,
                    &cache,
                    Ok(Some(PathBuf::from("slow.mp3"))),
                    "file:///slow.mp3",
                    64,
                    move |_| {
                        started.notify_one();
                        released.recv().unwrap();
                        Ok(None)
                    },
                )
                .await
            })
        };
        started.notified().await;

        tokio::time::timeout(
            Duration::from_millis(100),
            tokio::time::sleep(Duration::from_millis(1)),
        )
        .await
        .expect("slow tag I/O must not block the Tokio worker");
        release.send(()).unwrap();
        assert_eq!(resolver.await.unwrap().unwrap(), None);
    }

    #[test]
    fn local_artwork_requires_membership_and_caps_decoded_bytes() {
        let library = Library::new();
        assert!(authorized_local_artwork_path(&library, "file:///etc/passwd").is_err());
        assert!(local_artwork_data_url(retune_audio::Artwork {
            mime: Some("image/png".into()),
            bytes: vec![0; MAX_LOCAL_ARTWORK_BYTES + 1],
        })
        .is_err());
    }

    #[tokio::test]
    async fn rejected_local_artwork_never_reads_tags() {
        let cache = Mutex::default();
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reader_reads = Arc::clone(&reads);

        let result = resolve_track_artwork_with(
            None::<&SpotifyClient<FakeTransport, InMemoryTokenStore>>,
            &cache,
            Err("Local artwork resource is not in the library.".into()),
            "file:///etc/passwd",
            64,
            move |_| {
                reader_reads.fetch_add(1, Ordering::SeqCst);
                Ok(None)
            },
        )
        .await;

        assert!(result.is_err());
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        assert!(cache.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn local_artwork_errors_are_not_cached() {
        let cache = Mutex::default();
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        for _ in 0..2 {
            let reader_reads = Arc::clone(&reads);
            let result = resolve_track_artwork_with(
                None::<&SpotifyClient<FakeTransport, InMemoryTokenStore>>,
                &cache,
                Ok(Some(PathBuf::from("oversized.mp3"))),
                "file:///oversized.mp3",
                64,
                move |_| {
                    reader_reads.fetch_add(1, Ordering::SeqCst);
                    Err("embedded artwork exceeds the byte limit".into())
                },
            )
            .await;
            assert!(result.is_err());
        }

        assert_eq!(reads.load(Ordering::SeqCst), 2);
        assert!(cache.lock().unwrap().is_empty());
    }

    #[test]
    fn spotify_links_and_track_destinations_are_canonical() {
        assert_eq!(
            spotify_item_link("playlist", "abc123", SpotifyOpenTarget::Web).unwrap(),
            "https://open.spotify.com/playlist/abc123"
        );
        assert_eq!(
            spotify_item_link("playlist", "abc123", SpotifyOpenTarget::App).unwrap(),
            "spotify:playlist:abc123"
        );
        assert!(spotify_item_link("playlist", "", SpotifyOpenTarget::App).is_err());
        assert!(spotify_item_link("playlist", "../account", SpotifyOpenTarget::Web).is_err());

        let track: SpotifyTrack = serde_json::from_value(serde_json::json!({
            "uri": "spotify:track:track1", "name": "Song", "artists": [{"id": "artist1", "name": "Artist"}],
            "album": {"id": "album1", "uri": "spotify:album:album1", "name": "Album"}
        })).unwrap();
        assert_eq!(
            spotify_track_destination(&track, SpotifyDestination::Album).unwrap(),
            SpotifyNavigation::Album {
                uri: "spotify:album:album1".into(),
                highlight: "spotify:track:track1".into()
            }
        );
        assert_eq!(
            spotify_track_destination(&track, SpotifyDestination::Artist).unwrap(),
            SpotifyNavigation::Artist {
                id: "artist1".into()
            }
        );
    }

    #[tokio::test]
    async fn playlist_album_add_reloads_canonical_tracks() {
        let client = playlist_client([
            Response::json(
                200,
                serde_json::json!({
                    "id": "album", "uri": "spotify:album:album", "name": "Album",
                    "artists": [], "release_date": "2024", "total_tracks": 2,
                    "tracks": {
                        "items": [
                            {"uri": "spotify:track:one", "name": "One", "artists": []},
                            {"uri": "spotify:track:two", "name": "Two", "artists": []}
                        ],
                        "next": null, "total": 2
                    }
                }),
            ),
            Response::json(201, serde_json::json!({"snapshot_id": "new"})),
            Response::json(
                200,
                serde_json::json!({
                    "items": [
                        {"is_local": false, "item": {
                            "uri": "spotify:track:external", "name": "External", "artists": []
                        }},
                        {"is_local": false, "item": {
                            "uri": "spotify:track:one", "name": "One", "artists": []
                        }},
                        {"is_local": false, "item": {
                            "uri": "spotify:track:two", "name": "Two", "artists": []
                        }}
                    ],
                    "next": null, "total": 3
                }),
            ),
        ]);
        let tracks = provider::album_tracks(&client, "spotify:album:album")
            .await
            .unwrap();
        let mut cache = playlist_cache();
        cache.playlists[0].track_count = 0;
        cache.playlists[0].tracks.clear();

        playlists::add(
            &client,
            &mut cache,
            &Library::new(),
            "playlist",
            tracks.into_iter().map(|track| track.uri).collect(),
        )
        .await
        .unwrap();

        assert_eq!(
            cache.playlists[0].tracks,
            [
                "spotify:track:external",
                "spotify:track:one",
                "spotify:track:two"
            ]
        );
        assert_eq!(
            cache.playlists[0]
                .spotify_tracks
                .iter()
                .map(|track| track.uri.as_str())
                .collect::<Vec<_>>(),
            [
                "spotify:track:external",
                "spotify:track:one",
                "spotify:track:two"
            ]
        );
        assert_eq!(client.transport().requests().len(), 3);
    }

    #[test]
    fn exact_membership_distinguishes_album_and_individual_track_states() {
        for (saved_album, saved_track) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let mut library = Library::new();
            library.add(metadata_track(
                "spotify:track:one",
                "Rock",
                "Artist",
                "Album",
            ));
            library.add(metadata_track(
                "spotify:track:two",
                "Rock",
                "Artist",
                "Album",
            ));
            library.set_album_rating(
                AlbumKey {
                    source: SourceId::Music,
                    art: "Artist".into(),
                    alb: "Album".into(),
                },
                Rating::new(4),
            );
            let mut spotify_library = SpotifyLibraryState {
                account_id: "account".into(),
                complete: true,
                ..SpotifyLibraryState::default()
            };
            if saved_track {
                spotify_library
                    .saved_tracks
                    .insert("spotify:track:one".into(), Some(10));
            }
            if saved_album {
                spotify_library.saved_albums.insert(
                    "spotify:album:album".into(),
                    store::SavedAlbumRecord {
                        uri: "spotify:album:album".into(),
                        name: "Album".into(),
                        artists: vec!["Artist".into()],
                        release_date: Some("2024-02-03".into()),
                        album_type: Some("album".into()),
                        added_at: Some(11),
                        track_uris: vec!["spotify:track:one".into(), "spotify:track:two".into()],
                    },
                );
            }

            let mut albums = vec![provider::SearchAlbum {
                uri: "spotify:album:album".into(),
                name: "Album".into(),
                artist: "Artist".into(),
                year: Some("2024".into()),
                image_url: None,
                album_type: Some("Album".into()),
                track_count: 2,
                in_library: false,
            }];
            let mut tracks = vec![provider::SearchTrack {
                uri: "spotify:track:one".into(),
                name: "One".into(),
                artist: "Artist".into(),
                alb: "Album".into(),
                duration_secs: 1,
                image_url: None,
                album_uri: Some("spotify:album:album".into()),
                in_library: false,
            }];

            mark_album_membership(&library, &spotify_library, &mut albums);
            mark_track_membership(&library, &spotify_library, &mut tracks);
            let page =
                spotify_commands::album_page_view(&library, &spotify_library, spotify_album());

            assert_eq!(albums[0].in_library, saved_album);
            assert_eq!(tracks[0].in_library, saved_track);
            assert!(page.content_complete);
            assert_eq!(page.saved_album, saved_album);
            assert_eq!(page.tracks[0].saved_individually, saved_track);
            assert!(!page.tracks[1].saved_individually);
            assert_eq!(page.album_rating, Some(4));
        }
    }

    #[test]
    fn album_rows_reflect_library_album_identity() {
        let mut library = Library::new();
        library.add(metadata_track(
            "spotify:track:one",
            "Rock",
            "Sum 41",
            "All Killer No Filler",
        ));
        let mut albums = vec![
            provider::SearchAlbum {
                uri: "spotify:album:one".into(),
                name: "All Killer No Filler".into(),
                artist: "Sum 41".into(),
                year: None,
                image_url: None,
                album_type: Some("Album".into()),
                track_count: 2,
                in_library: false,
            },
            provider::SearchAlbum {
                uri: "spotify:album:two".into(),
                name: "Chuck".into(),
                artist: "Sum 41".into(),
                year: None,
                image_url: None,
                album_type: Some("Album".into()),
                track_count: 1,
                in_library: false,
            },
        ];

        mark_album_membership(&library, &SpotifyLibraryState::default(), &mut albums);

        assert!(!albums[0].in_library);
        assert!(!albums[1].in_library);

        library.add(metadata_track(
            "spotify:track:two",
            "Rock",
            "Sum 41",
            "All Killer No Filler",
        ));
        mark_album_membership(&library, &SpotifyLibraryState::default(), &mut albums);

        assert!(albums[0].in_library);
    }

    #[test]
    fn track_rows_reflect_library_track_identity() {
        let mut library = Library::new();
        library.add(metadata_track(
            "spotify:track:one",
            "Rock",
            "Artist",
            "Album",
        ));
        let mut tracks = vec![
            provider::SearchTrack {
                uri: "spotify:track:one".into(),
                name: "One".into(),
                artist: "Artist".into(),
                alb: "Album".into(),
                duration_secs: 1,
                image_url: None,
                album_uri: Some("spotify:album:one".into()),
                in_library: false,
            },
            provider::SearchTrack {
                uri: "spotify:track:two".into(),
                name: "Two".into(),
                artist: "Artist".into(),
                alb: "Album".into(),
                duration_secs: 1,
                image_url: None,
                album_uri: Some("spotify:album:one".into()),
                in_library: true,
            },
        ];

        mark_track_membership(&library, &SpotifyLibraryState::default(), &mut tracks);

        assert!(tracks[0].in_library);
        assert!(!tracks[1].in_library);
    }

    #[test]
    fn remove_album_tracks_removes_exactly_the_album_uris() {
        let mut library = Library::new();
        library.add(metadata_track(
            "spotify:track:one",
            "Rock",
            "Artist",
            "Album",
        ));
        library.add(metadata_track(
            "spotify:track:two",
            "Rock",
            "Artist",
            "Album",
        ));
        library.add(metadata_track(
            "spotify:track:other",
            "Rock",
            "Artist",
            "Other",
        ));

        assert_eq!(remove_album_tracks(&mut library, &spotify_album()), 2);
        assert_eq!(
            library
                .tracks()
                .iter()
                .map(|track| track.uri.as_str())
                .collect::<Vec<_>>(),
            ["spotify:track:other"]
        );
    }

    #[test]
    fn sync_progress_is_section_weighted_and_never_moves_backwards() {
        let mut progress = SyncProgressState::default();
        let first = progress.update(&SyncBatch {
            tracks: vec![
                metadata_track("one", "rock", "Artist", "Album"),
                metadata_track("two", "rock", "Artist", "Album"),
            ],
            done: 1,
            total: Some(2),
            section: "albums",
        });
        let second = progress.update(&SyncBatch {
            tracks: vec![],
            done: 0,
            total: None,
            section: "albums",
        });

        assert_eq!(first.tracks, 2);
        assert!((first.fraction - 0.3).abs() < f64::EPSILON);
        assert_eq!(second.fraction, first.fraction);
    }

    #[test]
    fn spotify_account_reconciliation_migrates_legacy_ids_and_resets_true_mismatches() {
        let profile = retune_spotify::client::Profile {
            id: "legacy-profile".into(),
            account_id: Some("immutable-account".into()),
            product: None,
        };
        let mut legacy = SpotifyLibraryState {
            account_id: "legacy-profile".into(),
            complete: true,
            ..SpotifyLibraryState::default()
        };
        legacy
            .saved_tracks
            .insert("spotify:track:one".into(), Some(1));

        let (migrated, changed) =
            spotify_commands::reconcile_spotify_account(legacy, &profile).unwrap();
        assert!(!changed);
        assert_eq!(migrated.account_id, "immutable-account");
        assert!(migrated.saved_tracks.contains_key("spotify:track:one"));

        let (reset, changed) = spotify_commands::reconcile_spotify_account(
            SpotifyLibraryState {
                account_id: "another-account".into(),
                complete: true,
                ..SpotifyLibraryState::default()
            },
            &profile,
        )
        .unwrap();
        assert!(changed);
        assert_eq!(reset.account_id, "immutable-account");
        assert!(!reset.complete);

        let missing = retune_spotify::client::Profile {
            id: "profile".into(),
            account_id: None,
            product: None,
        };
        assert!(spotify_commands::reconcile_spotify_account(reset, &missing).is_err());
    }

    #[test]
    fn release_first_run_starts_empty() {
        assert!(initial_library(false).tracks().is_empty());
        assert!(!initial_library(true).tracks().is_empty());
    }

    #[test]
    fn startup_action_syncs_connects_or_does_nothing() {
        let connected = ConnectionState {
            connected: true,
            needs_reauth: false,
            playback_authorized: false,
            missing_scopes: vec![],
        };
        let disconnected = ConnectionState::from_tokens(None);
        let needs_reauth = ConnectionState {
            connected: true,
            needs_reauth: true,
            playback_authorized: false,
            missing_scopes: vec!["playlist-read-private".into()],
        };
        assert_eq!(
            startup_action(&connected, "", false, None, 1_000),
            StartupAction::Sync
        );
        assert_eq!(
            startup_action(&connected, "", false, Some(999), 1_000),
            StartupAction::Nothing
        );
        assert_eq!(
            startup_action(&connected, "", false, Some(99), 1_000),
            StartupAction::Sync
        );
        assert_eq!(
            startup_action(&disconnected, "client-id", true, None, 1_000),
            StartupAction::Connect
        );
        assert_eq!(
            startup_action(&disconnected, "", true, None, 1_000),
            StartupAction::Nothing
        );
        assert_eq!(
            startup_action(&disconnected, "client-id", false, None, 1_000),
            StartupAction::Nothing
        );
        assert_eq!(
            startup_action(&needs_reauth, "client-id", true, None, 1_000),
            StartupAction::Sync
        );
    }

    #[test]
    fn connection_state_reports_missing_scopes() {
        let legacy = Tokens {
            access: String::new(),
            refresh: String::new(),
            expires_at: 0,
            scopes: "user-library-read".into(),
            playback_credentials: None,
        };
        let current = Tokens {
            scopes: auth::SCOPES.clone(),
            playback_credentials: Some(retune_spotify::tokens::PlaybackCredentials {
                username: "user".into(),
                auth_data: vec![1, 2, 3],
            }),
            ..legacy.clone()
        };

        assert_eq!(
            stored_connection_state(&shared_token_store(Some(legacy))).unwrap(),
            ConnectionState {
                connected: true,
                needs_reauth: true,
                playback_authorized: false,
                missing_scopes: auth::REQUIRED_SCOPES
                    .into_iter()
                    .filter(|scope| *scope != "user-library-read")
                    .map(String::from)
                    .collect(),
            }
        );
        assert_eq!(
            stored_connection_state(&shared_token_store(Some(current.clone()))).unwrap(),
            ConnectionState {
                connected: true,
                needs_reauth: false,
                playback_authorized: true,
                missing_scopes: vec![],
            }
        );
        let empty_playback = Tokens {
            playback_credentials: Some(retune_spotify::tokens::PlaybackCredentials {
                username: String::new(),
                auth_data: vec![],
            }),
            ..current
        };
        assert!(
            !stored_connection_state(&shared_token_store(Some(empty_playback)))
                .unwrap()
                .playback_authorized
        );
    }

    #[test]
    fn partial_completion_does_not_mark_a_full_sync() {
        let mut settings = Settings::default();
        assert!(!record_full_sync(&mut settings, true, 42));
        assert_eq!(settings.last_full_sync, None);
        assert!(!settings.spotify_sync_completed);

        assert!(record_full_sync(&mut settings, false, 42));
        assert_eq!(settings.last_full_sync, Some(42));
        assert!(settings.spotify_sync_completed);
    }

    #[test]
    fn lastfm_profile_is_account_bound_and_survives_toggles() {
        let mut settings = Settings::default();
        settings_commands::reconcile_lastfm_scrobbling_profile(&mut settings, "first-user", 10);
        assert_eq!(
            settings.lastfm_scrobbling_profile,
            Some(LastFmScrobblingProfile {
                username: "first-user".into(),
                started_at: 10,
            })
        );
        settings.lastfm_scrobbling = false;
        settings_commands::reconcile_lastfm_scrobbling_profile(&mut settings, "first-user", 20);
        assert_eq!(
            settings
                .lastfm_scrobbling_profile
                .as_ref()
                .unwrap()
                .started_at,
            10
        );
        settings_commands::reconcile_lastfm_scrobbling_profile(&mut settings, "second-user", 30);
        assert_eq!(
            settings.lastfm_scrobbling_profile,
            Some(LastFmScrobblingProfile {
                username: "second-user".into(),
                started_at: 30,
            })
        );
    }

    #[test]
    fn export_restore_round_trips_visual_settings_and_playlist_order() {
        let library = fixture::library();
        let playlists = playlists::PlaylistCache {
            playlists: ["second", "first"]
                .map(|id| playlists::CachedPlaylist {
                    id: id.into(),
                    name: id.into(),
                    snapshot_id: "snapshot".into(),
                    owned: true,
                    owner: None,
                    track_count: 0,
                    tracks: vec![],
                    track_metadata_version: playlists::TRACK_METADATA_VERSION,
                    spotify_tracks: vec![],
                })
                .to_vec(),
        };
        let exported = Settings {
            theme: Theme::Dark,
            zoom: 1.4,
            zebra: false,
            pl_collapsed: true,
            browser_visible: false,
            browser_panes: BrowserPanes {
                cat: false,
                art: true,
                alb: false,
            },
            column_order: [
                "name",
                "plays",
                "track",
                "rating",
                "artist",
                "album",
                "disc",
                "genre",
                "time",
                "kind",
                "bitrate",
                "lastPlayed",
                "added",
                "releaseDate",
            ]
            .map(String::from)
            .to_vec(),
            column_widths: BTreeMap::from([("name".into(), 260), ("artist".into(), 140)]),
            hidden_columns: vec![
                "genre".into(),
                "disc".into(),
                "kind".into(),
                "bitrate".into(),
                "added".into(),
                "releaseDate".into(),
            ],
            playlist_hidden_columns: BTreeMap::from([(
                "first".into(),
                vec!["plays".into(), "genre".into()],
            )]),
            playlist_column_orders: BTreeMap::from([(
                "first".into(),
                [
                    "genre",
                    "name",
                    "artist",
                    "album",
                    "time",
                    "rating",
                    "plays",
                    "disc",
                    "kind",
                    "bitrate",
                    "lastPlayed",
                    "added",
                    "releaseDate",
                    "track",
                ]
                .map(String::from)
                .to_vec(),
            )]),
            playlist_column_widths: BTreeMap::from([(
                "first".into(),
                BTreeMap::from([("name".into(), 220), ("genre".into(), 120)]),
            )]),
            sort_column: Some("plays".into()),
            sort_desc: true,
            auto_add_spotify_library: false,
            auto_connect: false,
            spotify_client_id: "exported-machine".into(),
            spotify_sync_completed: true,
            last_full_sync: Some(42),
            playback_backend: PlaybackBackend::Local,
            repeat: RepeatMode::All,
            shuffle: true,
            volume: 40,
            streaming_bitrate: 160,
            normalize_volume: true,
            gapless: false,
            play_threshold_percent: 100,
            lastfm_scrobbling: true,
            lastfm_scrobbling_profile: Some(LastFmScrobblingProfile {
                username: "exported-user".into(),
                started_at: 1786804381,
            }),
        };
        let plain = export_with_settings(&library, &exported, &playlists).unwrap();
        // Gzip the export ourselves so import's GzDecoder path stays covered
        // (the import file filter still accepts .gz).
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, &plain).unwrap();
        let bytes = encoder.finish().unwrap();
        let (restored_library, visual, restored_playlists) =
            import_with_settings(&bytes, true).unwrap();
        let mut restored = Settings {
            spotify_client_id: "local-machine".into(),
            auto_add_spotify_library: true,
            auto_connect: true,
            spotify_sync_completed: false,
            ..Settings::default()
        };
        visual.unwrap().apply_to(&mut restored).unwrap();

        assert_eq!(restored_library, library);
        assert_eq!(restored_playlists, Some(playlists));
        assert_eq!(restored.theme, Theme::Dark);
        assert!(restored.pl_collapsed);
        assert!(!restored.browser_visible);
        assert_eq!(restored.browser_panes, exported.browser_panes);
        assert_eq!(restored.column_order, exported.column_order);
        assert_eq!(restored.column_widths, exported.column_widths);
        assert_eq!(restored.hidden_columns, exported.hidden_columns);
        assert_eq!(
            restored.playlist_hidden_columns,
            exported.playlist_hidden_columns
        );
        assert_eq!(
            restored.playlist_column_orders,
            exported.playlist_column_orders
        );
        assert_eq!(
            restored.playlist_column_widths,
            exported.playlist_column_widths
        );
        assert_eq!(restored.sort_column.as_deref(), Some("plays"));
        assert!(restored.sort_desc);
        assert!(restored.shuffle);
        assert_eq!(restored.spotify_client_id, "local-machine");
        assert!(restored.auto_add_spotify_library);
        assert!(restored.auto_connect);
        assert!(!restored.spotify_sync_completed);
        assert_eq!(
            restored.lastfm_scrobbling_profile,
            exported.lastfm_scrobbling_profile
        );
    }

    #[test]
    fn visual_settings_apply_restored_browser_visibility() {
        let mut settings = Settings::default();
        let mut visual = ExportSettings::from_settings(&settings);
        visual.browser_panes = BrowserPanes {
            cat: true,
            art: false,
            alb: true,
        };

        visual.apply_to(&mut settings).unwrap();

        assert_eq!(
            settings.browser_panes,
            BrowserPanes {
                cat: true,
                art: false,
                alb: true,
            }
        );
    }

    #[test]
    fn backup_round_trip_exports_mappings_but_not_machine_sync_state() {
        let mappings = lastfm_import::PersistedLastFmMappings {
            version: lastfm_import::LASTFM_MAPPINGS_VERSION,
            lastfm_username: Some("lastfm-user".into()),
            spotify_account_id: Some("spotify-user".into()),
            dormant: false,
            mappings: lastfm_import::LastFmMappings {
                track_mappings: BTreeMap::from([(
                    "artist\u{1f}album\u{1f}song".into(),
                    "spotify:track:song".into(),
                )]),
                ..lastfm_import::LastFmMappings::default()
            },
        };
        let bytes = export_with_settings_and_mappings(
            &fixture::library(),
            &Settings::default(),
            &playlists::PlaylistCache::default(),
            Some(&mappings),
        )
        .unwrap();
        let exported: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(exported.get("lastfmMappings").is_some());
        assert!(exported.get("lastfm-sync").is_none());
        assert!(exported.get("spotifyCatalog").is_none());

        let (_, _, _, restored) = import_with_settings_and_mappings(&bytes, true).unwrap();
        assert_eq!(restored, Some(mappings.clone()));
        let (_, _, _, merged) = import_with_settings_and_mappings(&bytes, false).unwrap();
        assert!(merged.is_none());
    }

    #[test]
    fn successful_runtime_restore_agrees_in_memory_and_on_disk_before_refresh() {
        use std::sync::atomic::AtomicUsize;

        let directory = tempfile::tempdir().unwrap();
        let state = test_app_state(
            directory.path(),
            Library::new(),
            SpotifyLibraryState::default(),
            lastfm::Service::new_for_test(directory.path(), true, false),
            lastfm_import::Service::new(directory.path()),
        );
        let library_events = Arc::new(AtomicUsize::new(0));
        let playlist_events = Arc::new(AtomicUsize::new(0));
        let settings_events = Arc::new(AtomicUsize::new(0));

        let imported = fixture::library();
        let restored_settings = Settings {
            zebra: false,
            ..Settings::default()
        };
        let export_settings = ExportSettings::from_settings(&restored_settings);
        let restored_playlists = playlists::PlaylistCache {
            playlists: vec![playlists::CachedPlaylist {
                id: "restored".into(),
                name: "Restored".into(),
                snapshot_id: "snapshot".into(),
                owned: true,
                owner: None,
                track_count: 1,
                tracks: vec!["spotify:track:restored".into()],
                track_metadata_version: playlists::TRACK_METADATA_VERSION,
                spotify_tracks: vec![],
            }],
        };
        let restored_mappings = lastfm_import::PersistedLastFmMappings {
            version: lastfm_import::LASTFM_MAPPINGS_VERSION,
            lastfm_username: Some("listener".into()),
            ..lastfm_import::PersistedLastFmMappings::default()
        };

        commit_restore(
            &state.library,
            &state.settings,
            &state.playlists,
            state.lastfm_import.as_ref(),
            state.restore_mutations.as_ref(),
            directory.path(),
            imported.clone(),
            Some(export_settings),
            Some(restored_playlists.clone()),
            Some(restored_mappings.clone()),
            |refresh| {
                let journal: serde_json::Value = serde_json::from_slice(
                    &fs::read(directory.path().join("restore-journal.json")).unwrap(),
                )
                .unwrap();
                assert_eq!(journal["phase"], "complete");
                assert_eq!(library_events.load(Ordering::SeqCst), 0);
                assert_eq!(playlist_events.load(Ordering::SeqCst), 0);
                assert_eq!(settings_events.load(Ordering::SeqCst), 0);
                refresh_completed_restore_with(
                    refresh,
                    |_, _| {
                        settings_events.fetch_add(1, Ordering::SeqCst);
                        Err("settings refresh rejected".into())
                    },
                    || {
                        playlist_events.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    },
                    || {
                        library_events.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    },
                );
            },
        )
        .unwrap();

        assert_eq!(*state.library.lock().unwrap(), imported);
        assert_eq!(state.settings.snapshot(), restored_settings);
        assert_eq!(state.playlists.snapshot().unwrap(), restored_playlists);
        let expected_mappings =
            lastfm_import::normalize_restored_mappings(restored_mappings).unwrap();
        assert_eq!(
            tauri::async_runtime::block_on(state.lastfm_import.export_mappings()),
            expected_mappings
        );
        assert_eq!(
            FsOverlayStore::new(directory.path()).load().unwrap(),
            Some(imported)
        );
        assert_eq!(
            FsSettingsStore::new(directory.path()).load().unwrap(),
            Some(restored_settings)
        );
        assert_eq!(
            FsPlaylistStore::new(directory.path()).load().unwrap(),
            restored_playlists
        );
        assert_eq!(
            lastfm_import::load_mappings_for_recovery(directory.path()).unwrap(),
            expected_mappings
        );
        assert_eq!(library_events.load(Ordering::SeqCst), 1);
        assert_eq!(playlist_events.load(Ordering::SeqCst), 1);
        assert_eq!(settings_events.load(Ordering::SeqCst), 1);
        assert!(!directory.path().join("restore-journal.json").exists());
    }

    #[test]
    fn runtime_restore_rolls_forward_after_transient_completion_failure() {
        let directory = tempfile::tempdir().unwrap();
        let service = lastfm_import::Service::new(directory.path());
        let state = test_app_state(
            directory.path(),
            Library::new(),
            SpotifyLibraryState::default(),
            lastfm::Service::new_for_test(directory.path(), true, false),
            Arc::clone(&service),
        );
        let mut imported = Library::new();
        imported.add(NewTrack {
            uri: "file:///restored.mp3".into(),
            name: "Restored".into(),
            ..NewTrack::default()
        });
        let restored_settings = Settings {
            zebra: false,
            ..Settings::default()
        };
        let restored_playlists = playlists::PlaylistCache {
            playlists: vec![playlists::CachedPlaylist {
                id: "restored".into(),
                name: "Restored".into(),
                snapshot_id: "snapshot".into(),
                owned: true,
                owner: None,
                track_count: 0,
                tracks: vec![],
                track_metadata_version: playlists::TRACK_METADATA_VERSION,
                spotify_tracks: vec![],
            }],
        };
        let restored_mappings = lastfm_import::PersistedLastFmMappings {
            version: lastfm_import::LASTFM_MAPPINGS_VERSION,
            lastfm_username: Some("listener".into()),
            ..lastfm_import::PersistedLastFmMappings::default()
        };
        restore::RestoreStore::new(directory.path()).fail_next_complete();

        commit_restore(
            &state.library,
            &state.settings,
            &state.playlists,
            state.lastfm_import.as_ref(),
            state.restore_mutations.as_ref(),
            directory.path(),
            imported.clone(),
            Some(ExportSettings::from_settings(&restored_settings)),
            Some(restored_playlists.clone()),
            Some(restored_mappings.clone()),
            |_| {},
        )
        .unwrap();

        let expected_mappings =
            lastfm_import::normalize_restored_mappings(restored_mappings).unwrap();
        assert_eq!(*state.library.lock().unwrap(), imported);
        assert_eq!(state.settings.snapshot(), restored_settings);
        assert_eq!(state.playlists.snapshot().unwrap(), restored_playlists);
        assert_eq!(
            tauri::async_runtime::block_on(service.export_mappings()),
            expected_mappings
        );
        assert_eq!(
            FsOverlayStore::new(directory.path()).load().unwrap(),
            Some(imported)
        );
        assert_eq!(
            FsSettingsStore::new(directory.path()).load().unwrap(),
            Some(restored_settings)
        );
        assert_eq!(
            FsPlaylistStore::new(directory.path()).load().unwrap(),
            restored_playlists
        );
        assert_eq!(
            lastfm_import::load_mappings_for_recovery(directory.path()).unwrap(),
            expected_mappings
        );
        assert!(!directory.path().join("restore-journal.json").exists());

        state.library.mutate(|_| Ok(())).unwrap();
        tauri::async_runtime::block_on(state.settings.mutate(
            |settings| {
                settings.zebra = !settings.zebra;
                Ok(())
            },
            |_, _| Ok(()),
        ))
        .unwrap();
        drop(tauri::async_runtime::block_on(state.playlists.begin_mutation()).unwrap());
        tauri::async_runtime::block_on(service.begin_mappings_restore())
            .unwrap()
            .replace(lastfm_import::PersistedLastFmMappings {
                version: lastfm_import::LASTFM_MAPPINGS_VERSION,
                ..lastfm_import::PersistedLastFmMappings::default()
            })
            .unwrap();
    }

    #[test]
    fn failed_runtime_recovery_latches_all_restore_owners_until_restart() {
        let directory = tempfile::tempdir().unwrap();
        let service = lastfm_import::Service::new(directory.path());
        let state = Arc::new(test_app_state(
            directory.path(),
            Library::new(),
            SpotifyLibraryState::default(),
            lastfm::Service::new_for_test(directory.path(), true, false),
            Arc::clone(&service),
        ));
        let mut imported = Library::new();
        imported.add(NewTrack {
            uri: "file:///restored.mp3".into(),
            ..NewTrack::default()
        });
        let baseline_playlists = playlists::PlaylistCache::default();
        FsPlaylistStore::new(directory.path())
            .save(&baseline_playlists)
            .unwrap();
        let restored_playlists = playlists::PlaylistCache {
            playlists: vec![playlists::CachedPlaylist {
                id: "restored".into(),
                name: "Restored".into(),
                snapshot_id: "snapshot".into(),
                owned: true,
                owner: None,
                track_count: 0,
                tracks: vec![],
                track_metadata_version: playlists::TRACK_METADATA_VERSION,
                spotify_tracks: vec![],
            }],
        };
        let baseline_mappings = lastfm_import::PersistedLastFmMappings {
            version: lastfm_import::LASTFM_MAPPINGS_VERSION,
            ..lastfm_import::PersistedLastFmMappings::default()
        };
        lastfm_import::save_mappings_for_recovery(directory.path(), &baseline_mappings).unwrap();
        let restored_mappings = lastfm_import::PersistedLastFmMappings {
            version: lastfm_import::LASTFM_MAPPINGS_VERSION,
            lastfm_username: Some("restored".into()),
            ..lastfm_import::PersistedLastFmMappings::default()
        };
        let settings_path = directory.path().join("settings.json");
        fs::create_dir(&settings_path).unwrap();

        let mut third_library = imported.clone();
        third_library.add(NewTrack {
            uri: "file:///third.mp3".into(),
            ..NewTrack::default()
        });
        let third_settings = Settings {
            zebra: false,
            ..Settings::default()
        };
        let library_mutated = AtomicBool::new(false);
        let settings_mutated = AtomicBool::new(false);

        let error = commit_restore(
            &state.library,
            &state.settings,
            &state.playlists,
            state.lastfm_import.as_ref(),
            state.restore_mutations.as_ref(),
            directory.path(),
            imported.clone(),
            Some(ExportSettings::from_settings(&Settings::default())),
            Some(restored_playlists.clone()),
            Some(restored_mappings.clone()),
            |_| panic!("failed recovery must not refresh"),
        )
        .unwrap_err();
        assert!(error.contains("Restore failed"));
        assert!(error.contains("immediate recovery failed"));
        assert!(error.contains("Restart Retune"));

        assert!(state
            .library
            .mutate(|library| {
                library_mutated.store(true, Ordering::SeqCst);
                *library = third_library.clone();
                Ok(())
            })
            .is_err());
        assert!(tauri::async_runtime::block_on(state.settings.mutate(
            |settings| {
                settings_mutated.store(true, Ordering::SeqCst);
                *settings = third_settings;
                Ok(())
            },
            |_, _| Ok(()),
        ))
        .is_err());
        assert!(tauri::async_runtime::block_on(state.playlists.begin_mutation()).is_err());
        assert!(tauri::async_runtime::block_on(service.begin_mappings_restore()).is_err());
        assert!(!library_mutated.load(Ordering::SeqCst));
        assert!(!settings_mutated.load(Ordering::SeqCst));
        assert_eq!(
            FsOverlayStore::new(directory.path()).load().unwrap(),
            Some(imported.clone())
        );
        assert_eq!(
            FsPlaylistStore::new(directory.path()).load().unwrap(),
            baseline_playlists
        );
        assert_eq!(
            lastfm_import::load_mappings_for_recovery(directory.path()).unwrap(),
            baseline_mappings
        );
        assert!(settings_path.is_dir());
        let journal: serde_json::Value = serde_json::from_slice(
            &fs::read(directory.path().join("restore-journal.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(journal["phase"], "applying");

        fs::remove_dir(&settings_path).unwrap();
        restore::RestoreStore::new(directory.path())
            .recover()
            .unwrap();
        assert!(!directory.path().join("restore-journal.json").exists());
        assert_eq!(
            FsPlaylistStore::new(directory.path()).load().unwrap(),
            restored_playlists
        );
        assert_eq!(
            lastfm_import::load_mappings_for_recovery(directory.path()).unwrap(),
            lastfm_import::normalize_restored_mappings(restored_mappings).unwrap()
        );
        let restarted_service = lastfm_import::Service::new(directory.path());
        let restarted = test_app_state(
            directory.path(),
            imported,
            SpotifyLibraryState::default(),
            lastfm::Service::new_for_test(directory.path(), true, false),
            restarted_service,
        );
        restarted
            .library
            .mutate(|library| {
                *library = third_library.clone();
                Ok(())
            })
            .unwrap();
        assert_eq!(
            FsOverlayStore::new(directory.path()).load().unwrap(),
            Some(third_library)
        );
    }

    #[test]
    fn spotify_catalog_flush_persists_dirty_data_and_clear() {
        let directory = tempfile::tempdir().unwrap();
        let state = test_app_state(
            directory.path(),
            Library::new(),
            SpotifyLibraryState::default(),
            lastfm::Service::new_for_test(directory.path(), true, false),
            lastfm_import::Service::new(directory.path()),
        );
        state
            .spotify_catalog
            .lock()
            .unwrap()
            .observe_track(&SpotifyTrack {
                uri: "spotify:track:one".into(),
                name: "One".into(),
                duration_ms: Some(1_000),
                track_number: Some(1),
                disc_number: Some(1),
                artists: vec![],
                album: None,
            });

        flush_spotify_catalog(&state).unwrap();
        let store = FsSpotifyCatalogStore::new(directory.path());
        assert_eq!(store.load().unwrap().counts().tracks, 1);

        clear_spotify_catalog(&state).unwrap();
        assert_eq!(store.load().unwrap().counts().tracks, 0);
    }

    #[test]
    fn concurrent_catalog_flushes_leave_a_parseable_current_generation() {
        let directory = tempfile::tempdir().unwrap();
        let state = Arc::new(test_app_state(
            directory.path(),
            Library::new(),
            SpotifyLibraryState::default(),
            lastfm::Service::new_for_test(directory.path(), true, false),
            lastfm_import::Service::new(directory.path()),
        ));
        state
            .spotify_catalog
            .lock()
            .unwrap()
            .observe_track(&SpotifyTrack {
                uri: "spotify:track:current".into(),
                name: "Current".into(),
                duration_ms: Some(1_000),
                track_number: Some(1),
                disc_number: Some(1),
                artists: vec![],
                album: None,
            });
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let state = Arc::clone(&state);
                scope.spawn(move || flush_spotify_catalog(&state).unwrap());
            }
        });

        let persisted = FsSpotifyCatalogStore::new(directory.path()).load().unwrap();
        assert_eq!(persisted.counts().tracks, 1);
        assert_eq!(
            state
                .spotify_catalog_saved_generation
                .load(Ordering::Acquire),
            state.spotify_catalog.lock().unwrap().generation()
        );
    }

    #[test]
    fn clean_spotify_catalog_flush_does_not_write() {
        let directory = tempfile::tempdir().unwrap();
        let state = test_app_state(
            directory.path(),
            Library::new(),
            SpotifyLibraryState::default(),
            lastfm::Service::new_for_test(directory.path(), true, false),
            lastfm_import::Service::new(directory.path()),
        );

        flush_spotify_catalog(&state).unwrap();

        assert!(!directory.path().join("spotify-catalog.json").exists());
    }

    #[test]
    fn export_restore_rejects_invalid_lastfm_profile() {
        for profile in [
            LastFmScrobblingProfile {
                username: " ".into(),
                started_at: 1,
            },
            LastFmScrobblingProfile {
                username: "user".into(),
                started_at: 0,
            },
        ] {
            let mut settings = Settings::default();
            let mut export = ExportSettings::from_settings(&settings);
            export.lastfm_scrobbling_profile = Some(profile);
            assert!(export.apply_to(&mut settings).is_err());
        }

        let invalid = Settings {
            lastfm_scrobbling_profile: Some(LastFmScrobblingProfile {
                username: " ".into(),
                started_at: 1,
            }),
            ..Settings::default()
        };
        let bytes = export_with_settings(
            &fixture::library(),
            &invalid,
            &playlists::PlaylistCache { playlists: vec![] },
        )
        .unwrap();
        assert!(import_with_settings(&bytes, true).is_err());
    }

    #[test]
    fn visual_settings_apply_normalizes_legacy_columns() {
        let mut json =
            serde_json::to_value(ExportSettings::from_settings(&Settings::default())).unwrap();
        let object = json.as_object_mut().unwrap();
        object.insert(
            "columnOrder".into(),
            serde_json::json!(["track", "name", "time", "artist", "album", "genre", "rating"]),
        );
        object.insert("hiddenColumns".into(), serde_json::json!(["name", "genre"]));
        object.remove("playlistColumnOrders");
        object.remove("playlistColumnWidths");
        object.remove("sortColumn");
        object.remove("sortDesc");
        let visual: ExportSettings = serde_json::from_value(json).unwrap();
        let mut settings = Settings::default();

        visual.apply_to(&mut settings).unwrap();

        assert_eq!(
            settings.column_order,
            [
                "track",
                "name",
                "time",
                "artist",
                "album",
                "genre",
                "rating",
                "plays",
                "disc",
                "kind",
                "bitrate",
                "lastPlayed",
                "added",
                "releaseDate",
            ]
        );
        assert_eq!(
            settings.hidden_columns,
            [
                "genre",
                "disc",
                "kind",
                "bitrate",
                "lastPlayed",
                "added",
                "releaseDate"
            ]
        );
        assert_eq!(settings.sort_column, None);
        assert!(!settings.sort_desc);
        assert!(settings.playlist_column_orders.is_empty());
        assert!(settings.playlist_column_widths.is_empty());
    }

    #[test]
    fn merge_ignores_exported_visual_settings() {
        let library = fixture::library();
        let bytes = export_with_settings(
            &library,
            &Settings::default(),
            &playlists::PlaylistCache::default(),
        )
        .unwrap();
        let (merged, visual, playlists) = import_with_settings(&bytes, false).unwrap();
        assert_eq!(merged, library);
        assert_eq!(visual, None);
        assert_eq!(playlists, None);
    }
}

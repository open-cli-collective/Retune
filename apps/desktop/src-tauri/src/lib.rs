mod diagnostics;
mod fixture;
mod lastfm;
mod lastfm_import;
mod library_commands;
mod localfiles;
mod media_keys;
mod playback;
mod playback_commands;
mod playlist_commands;
mod playlists;
mod provider;
mod spotify_commands;
mod store;
mod sync;
mod sync_orchestrator;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    io::Read,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine};
use flate2::read::GzDecoder;

use playback::{AudioSettings, Playback, PlayerStateEvent, SnapshotTrack};
use provider::{
    artist_albums_page, artist_descriptor, image_url, image_url_at_least, spotify_id, title_case,
    ArtistAlbumsPage, SearchResults, SpotifySyncProvider, SyncBatch,
};
use retune_core::{
    browse::{self, Selection},
    io::{export_json, import},
    model::{
        AlbumKey, EffectiveRating, Library, NewTrack, Rating, SourceId, TrackEdit, TrackId,
        TrackRecord,
    },
};
use retune_spotify::{
    auth::{self, LoopbackListener, Pkce},
    client::{
        endpoint_family, Album, HttpTransport, SpotifyClient, Track as SpotifyTrack, Transport,
    },
    normalize::UNCATEGORIZED,
    tokens::{CachedTokenStore, EncryptedFsTokenStore, TokenStore, Tokens},
};
use serde::{Deserialize, Serialize};
use store::{
    BrowserPanes, FsOverlayStore, FsPlaylistStore, FsSettingsStore, FsSyncStore,
    LastFmScrobblingProfile, OverlayStore, Settings, SpotifyLibraryState, StoreError, Theme,
};
use sync_orchestrator::SyncOrchestrator;
use tauri::{
    menu::{CheckMenuItem, CheckMenuItemBuilder, MenuBuilder, MenuItemBuilder, SubmenuBuilder},
    Emitter, Manager,
};
use tauri_plugin_opener::OpenerExt;

pub(crate) type SharedTokenStore = Arc<CachedTokenStore<Box<dyn TokenStore>>>;
type SpotifyProvider = SpotifyClient<HttpTransport, SharedTokenStore>;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};

struct LibraryTransactionState {
    active: Mutex<bool>,
    changed: Condvar,
}

impl Default for LibraryTransactionState {
    fn default() -> Self {
        Self {
            active: Mutex::new(false),
            changed: Condvar::new(),
        }
    }
}

struct AppState {
    library: Mutex<Library>,
    store: FsOverlayStore,
    library_write_gate: Arc<Mutex<()>>,
    library_transaction: Arc<LibraryTransactionState>,
    spotify_library: Mutex<SpotifyLibraryState>,
    spotify_library_gate: tokio::sync::Mutex<()>,
    settings: Mutex<Settings>,
    settings_store: FsSettingsStore,
    sync_store: FsSyncStore,
    playlists: Mutex<playlists::PlaylistCache>,
    playlist_store: FsPlaylistStore,
    menu_checks: Option<MenuChecks>,
    recovery_notice: Mutex<Option<String>>,
    token_store: SharedTokenStore,
    spotify: Mutex<Option<Arc<SpotifyProvider>>>,
    artwork_cache: Mutex<HashMap<(String, u32), Option<String>>>,
    playback: Arc<Playback>,
    lastfm: Arc<lastfm::Service>,
    lastfm_import: Arc<lastfm_import::Service>,
    media_keys: media_keys::MediaKeys,
    sync_orchestrator: SyncOrchestrator,
    playlist_reauth_notified: AtomicBool,
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
    AppState {
        library: Mutex::new(library),
        store: FsOverlayStore::new(&app_data_dir),
        library_write_gate: Arc::new(Mutex::new(())),
        library_transaction: Arc::new(LibraryTransactionState::default()),
        spotify_library: Mutex::new(spotify_library),
        spotify_library_gate: tokio::sync::Mutex::new(()),
        settings: Mutex::new(Settings::default()),
        settings_store: FsSettingsStore::new(&app_data_dir),
        sync_store: FsSyncStore::new(&app_data_dir),
        playlists: Mutex::new(playlists::PlaylistCache::default()),
        playlist_store: FsPlaylistStore::new(&app_data_dir),
        menu_checks: None,
        recovery_notice: Mutex::new(None),
        token_store,
        spotify: Mutex::new(None),
        artwork_cache: Mutex::default(),
        playback: Arc::new(Playback::default()),
        lastfm,
        lastfm_import,
        media_keys: media_keys::MediaKeys::disabled(),
        sync_orchestrator: SyncOrchestrator::default(),
        playlist_reauth_notified: AtomicBool::new(false),
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

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportSettings {
    theme: Theme,
    zoom: f64,
    zebra: bool,
    #[serde(default)]
    pl_collapsed: bool,
    #[serde(default = "default_browser_visible")]
    browser_visible: bool,
    #[serde(default)]
    browser_panes: BrowserPanes,
    column_order: Vec<String>,
    #[serde(default)]
    column_widths: BTreeMap<String, u32>,
    hidden_columns: Vec<String>,
    #[serde(default)]
    playlist_hidden_columns: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    playlist_column_orders: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    playlist_column_widths: BTreeMap<String, BTreeMap<String, u32>>,
    #[serde(default)]
    sort_column: Option<String>,
    #[serde(default)]
    sort_desc: bool,
    #[serde(default)]
    shuffle: bool,
    #[serde(default)]
    lastfm_scrobbling_profile: Option<LastFmScrobblingProfile>,
}

impl ExportSettings {
    fn from_settings(settings: &Settings) -> Self {
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

    fn apply_to(self, settings: &mut Settings) -> Result<(), String> {
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

#[derive(Deserialize)]
struct SelectionDto {
    #[serde(default)]
    cat: Vec<String>,
    #[serde(default)]
    art: Vec<String>,
    #[serde(default)]
    alb: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowseView {
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
struct MetadataValues {
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
struct TrackInfoView {
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
struct TrackEditDto {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
struct RatingView {
    stars: u8,
    explicit: bool,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct AlbumPageTrackView {
    uri: String,
    name: String,
    track_no: Option<u32>,
    duration_secs: u64,
    enabled: bool,
    track_id: Option<u64>,
    saved_individually: bool,
    rating: Option<RatingView>,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct AlbumPageView {
    uri: String,
    name: String,
    artist: String,
    artist_id: String,
    album_type: String,
    year: Option<String>,
    image_url: Option<String>,
    total_duration_secs: u64,
    saved_album: bool,
    content_complete: bool,
    added_at: Option<u64>,
    album_rating: Option<u8>,
    tracks: Vec<AlbumPageTrackView>,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct ArtistPageView {
    id: String,
    name: String,
    descriptor: String,
    image_url: Option<String>,
    following: bool,
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlaylistListView {
    id: String,
    name: String,
    owned: bool,
    owner: Option<String>,
    contains: bool,
    track_count: usize,
    items_available: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlaylistTrackView {
    id: Option<u64>,
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
            .filter(|track| !track.enabled)
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

#[tauri::command]
fn startup_notice(state: tauri::State<'_, AppState>) -> Option<String> {
    state
        .recovery_notice
        .lock()
        .expect("notice mutex poisoned")
        .take()
}

#[tauri::command]
fn get_settings(state: tauri::State<'_, AppState>) -> Settings {
    state
        .settings
        .lock()
        .expect("settings mutex poisoned")
        .clone()
}

#[tauri::command]
async fn set_settings(app: tauri::AppHandle, mut settings: Settings) -> Result<(), String> {
    let state = app.state::<AppState>();
    settings.normalize();
    let current = state
        .settings
        .lock()
        .expect("settings mutex poisoned")
        .clone();
    let client_id_changed = current.spotify_client_id != settings.spotify_client_id;
    settings.spotify_sync_completed = current.spotify_sync_completed;
    settings.last_full_sync = current.last_full_sync;
    if settings.lastfm_scrobbling {
        if let Some(username) = state.lastfm.state().await.username {
            reconcile_lastfm_scrobbling_profile(&mut settings, &username, unix_now());
        }
    }
    settings.validate().map_err(|error| error.to_string())?;
    let wants_local = settings.playback_backend == "local";
    state.playback.set_local_requested(wants_local);
    // Local activation is intentionally lazy: playback owns authorization
    // prompts, and unrelated preference saves must remain offline-safe.
    if !wants_local && state.playback.is_local_active().await {
        state.playback.switch_to_connect().await;
    }
    state
        .settings_store
        .save(&settings)
        .map_err(|error| error.to_string())?;
    *state.settings.lock().expect("settings mutex poisoned") = settings.clone();
    state.lastfm.set_enabled(settings.lastfm_scrobbling).await;
    state
        .playback
        .set_play_threshold_percent(settings.play_threshold_percent)
        .await;
    if let Some(menu_checks) = &state.menu_checks {
        menu_checks
            .sync(&settings)
            .map_err(|error| error.to_string())?;
    }
    if client_id_changed {
        *state.spotify.lock().expect("spotify mutex poisoned") =
            spotify_provider(&settings.spotify_client_id, Arc::clone(&state.token_store))?;
    }
    Ok(())
}

pub(crate) async fn set_lastfm_scrobbling(
    app: &tauri::AppHandle,
    enabled: bool,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let current = state
        .settings
        .lock()
        .expect("settings mutex poisoned")
        .clone();
    let mut settings = current.clone();
    settings.lastfm_scrobbling = enabled;
    if enabled {
        if let Some(username) = state.lastfm.state().await.username {
            reconcile_lastfm_scrobbling_profile(&mut settings, &username, unix_now());
        }
    }
    state
        .settings_store
        .save(&settings)
        .map_err(|error| error.to_string())?;
    *state.settings.lock().expect("settings mutex poisoned") = settings.clone();
    app.emit("settings-changed", settings)
        .map_err(|error| error.to_string())
}

fn reconcile_lastfm_scrobbling_profile(settings: &mut Settings, username: &str, now: u64) {
    if username.trim().is_empty()
        || settings
            .lastfm_scrobbling_profile
            .as_ref()
            .is_some_and(|profile| profile.username == username)
    {
        return;
    }
    settings.lastfm_scrobbling_profile = Some(LastFmScrobblingProfile {
        username: username.to_owned(),
        started_at: now,
    });
}

async fn history_cutoff_for_import(app: &tauri::AppHandle, username: &str) -> Result<u64, String> {
    let state = app.state::<AppState>();
    let current = state
        .settings
        .lock()
        .expect("settings mutex poisoned")
        .clone();
    let mut settings = current.clone();
    reconcile_lastfm_scrobbling_profile(&mut settings, username, unix_now());
    let cutoff = settings
        .lastfm_scrobbling_profile
        .as_ref()
        .map(|profile| profile.started_at)
        .ok_or_else(|| "Could not establish the Last.fm history cutoff.".to_string())?;
    if settings != current {
        state
            .settings_store
            .save(&settings)
            .map_err(|error| error.to_string())?;
        *state.settings.lock().expect("settings mutex poisoned") = settings;
    }
    Ok(cutoff)
}

async fn switch_to_local(state: &AppState, volume: u8) -> Result<(), String> {
    state
        .token_store
        .load()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Connect to Spotify before enabling built-in playback.".to_string())?;
    let client = provider_from(state)?;
    if !client
        .me()
        .await
        .map_err(|error| format!("Could not verify Spotify Premium: {error}"))?
        .is_premium()
    {
        return Err("Playback requires Spotify Premium.".into());
    }
    state
        .playback
        .switch_to_local(client.as_ref(), volume)
        .await
}

fn spotify_provider(
    client_id: &str,
    token_store: SharedTokenStore,
) -> Result<Option<Arc<SpotifyProvider>>, String> {
    if client_id.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(Arc::new(SpotifyClient::new(
        client_id.trim(),
        HttpTransport::new(),
        token_store,
    ))))
}

fn stored_connection_state(token_store: &SharedTokenStore) -> Result<ConnectionState, String> {
    token_store
        .load()
        .map(ConnectionState::from_tokens)
        .map_err(|error| error.to_string())
}

pub(crate) fn emit_connection_state(app: &tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let connection = stored_connection_state(&state.token_store)?;
    if let Some(menu_checks) = &state.menu_checks {
        menu_checks
            .sync_connection(&connection)
            .map_err(|error| error.to_string())?;
    }
    app.emit("connection-changed", connection)
        .map_err(|error| error.to_string())
}

fn set_auto_connect(app: &tauri::AppHandle, auto_connect: bool) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut settings = state
        .settings
        .lock()
        .expect("settings mutex poisoned")
        .clone();
    settings.auto_connect = auto_connect;
    state
        .settings_store
        .save(&settings)
        .map_err(|error| error.to_string())?;
    *state.settings.lock().expect("settings mutex poisoned") = settings.clone();
    app.emit("settings-changed", settings)
        .map_err(|error| error.to_string())
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
    uri.strip_prefix("spotify:track:")
        .filter(|id| !id.is_empty() && !id.contains(':'))
}

fn album_id(uri: &str) -> Option<&str> {
    uri.strip_prefix("spotify:album:")
        .filter(|id| !id.is_empty() && !id.contains(':'))
}

async fn resolve_track_artwork<T: Transport, S: TokenStore>(
    client: Option<&SpotifyClient<T, S>>,
    cache: &Mutex<HashMap<(String, u32), Option<String>>>,
    uri: &str,
    min_width: u32,
) -> Option<String> {
    let local = uri.starts_with("file:");
    let id = (!local).then(|| track_id(uri)).flatten();
    if !local && id.is_none() {
        return None;
    }
    let cache_key = (uri.into(), min_width);
    if let Some(cached) = cache
        .lock()
        .expect("artwork cache mutex poisoned")
        .get(&cache_key)
        .cloned()
    {
        return cached;
    }
    let artwork = if local {
        localfiles::path_from_file_uri(uri)
            .ok()
            .and_then(|path| retune_audio::read_tags(path).ok())
            .and_then(|tags| tags.artwork)
            .map(|artwork| {
                format!(
                    "data:{};base64,{}",
                    artwork
                        .mime
                        .as_deref()
                        .unwrap_or("application/octet-stream"),
                    BASE64_STANDARD.encode(artwork.bytes)
                )
            })
    } else {
        client?
            .track(id?)
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
    artwork
}

pub(crate) async fn publish_media_artwork(app: tauri::AppHandle, event: PlayerStateEvent) {
    let state = app.state::<AppState>();
    let provider = provider_from(&state).ok();
    let Some(url) = resolve_track_artwork(
        provider.as_deref(),
        &state.artwork_cache,
        event.uri.as_deref().unwrap_or_default(),
        300,
    )
    .await
    else {
        return;
    };
    state.media_keys.update_artwork(&event, &url);
}

async fn sync_spotify(app: &tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    state.sync_orchestrator.cancel_retry();
    if !state.sync_orchestrator.begin() {
        return Ok(());
    }
    run_sync_loop(app).await
}

async fn run_sync_loop(app: &tauri::AppHandle) -> Result<(), String> {
    loop {
        let result = sync_spotify_inner(app).await;
        if !result.as_ref().is_ok_and(|completion| completion.partial) {
            let _ = app.emit("sync-progress", "");
        }
        if app.state::<AppState>().sync_orchestrator.finish() {
            continue;
        }
        if let Ok(SyncCompletion {
            auto_resume: Some(deadline),
            ..
        }) = &result
        {
            schedule_auto_resume(app, *deadline);
        }
        return result.map(|_| ());
    }
}

fn schedule_auto_resume(app: &tauri::AppHandle, deadline: u64) {
    let now = unix_now();
    let jitter = 30
        + SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as u64
            % 61;
    let delay = Duration::from_secs(deadline.saturating_sub(now).saturating_add(jitter));
    let handle = app.clone();
    let retry = tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let state = handle.state::<AppState>();
        state.sync_orchestrator.retry_fired();
        if state.sync_orchestrator.begin() {
            if let Err(error) = Box::pin(run_sync_loop(&handle)).await {
                notify_error(&handle, error);
            }
        }
    });
    app.state::<AppState>()
        .sync_orchestrator
        .replace_retry(retry);
}

struct SyncCompletion {
    partial: bool,
    auto_resume: Option<u64>,
}

#[derive(Clone, Copy, Serialize)]
struct SyncProgressCount {
    tracks: u64,
    fraction: f64,
}

#[derive(Default)]
struct SyncProgressState {
    tracks: u64,
    sections: [(u32, Option<u32>); 5],
    high_water: f64,
}

impl SyncProgressState {
    fn update(&mut self, batch: &SyncBatch) -> SyncProgressCount {
        self.tracks += batch.tracks.len() as u64;
        let index = provider::LibraryKind::ALL
            .iter()
            .position(|kind| kind.label() == batch.section)
            .expect("unknown sync section");
        self.sections[index] = (batch.done, batch.total);
        let fraction = self
            .sections
            .iter()
            .enumerate()
            .map(|(section, (done, total))| {
                if section < index {
                    1.0
                } else {
                    total.map_or(0.0, |total| {
                        if total == 0 {
                            1.0
                        } else {
                            f64::from(*done) / f64::from(total)
                        }
                    })
                }
            })
            .sum::<f64>()
            / provider::LibraryKind::ALL.len() as f64;
        self.high_water = self.high_water.max(fraction.clamp(0.0, 1.0));
        SyncProgressCount {
            tracks: self.tracks,
            fraction: self.high_water,
        }
    }
}

const GENRES_DEGRADED_MSG: &str =
    "Imported without genres (Spotify rate limit) — genres will fill in on a later sync.";

fn partial_import_message(
    detail: &str,
    quota_exhausted: bool,
    earliest_cooldown: Option<u64>,
    now: chrono::DateTime<chrono::Local>,
) -> String {
    earliest_cooldown.map_or_else(
        || {
            if quota_exhausted {
                if detail.is_empty() {
                    "Partial import — Spotify Development Mode quota is exhausted; sync again after Spotify resets it.".into()
                } else {
                    format!("Partial import ({detail}) — Spotify Development Mode quota is exhausted; sync again after Spotify resets it.")
                }
            } else if detail.is_empty() {
                "Partial import (Spotify rate limit) — run File → Sync later to finish.".into()
            } else {
                format!("Partial import ({detail}) — run File → Sync later to finish.")
            }
        },
        |deadline| {
            let time = provider::format_resume_time(deadline, now);
            if quota_exhausted && detail.is_empty() {
                format!("Partial import (Spotify Development Mode quota) — will finish automatically after {time}.")
            } else if quota_exhausted {
                format!("Partial import (Spotify Development Mode quota) — {detail} — will finish automatically after {time}.")
            } else if detail.is_empty() {
                format!("Partial import — will finish automatically after {time}.")
            } else {
                format!("Partial import — {detail} — will finish automatically after {time}.")
            }
        },
    )
}

async fn sync_spotify_inner(app: &tauri::AppHandle) -> Result<SyncCompletion, String> {
    log::info!("Starting Spotify sync");
    let state = app.state::<AppState>();
    let _membership_guard = state.spotify_library_gate.lock().await;
    if !stored_connection_state(&state.token_store)?.connected {
        return Err("Connect to Spotify before syncing.".into());
    }
    let provider = provider_from(&state)?;
    let account_id = provider
        .me()
        .await
        .map_err(|error| format!("Could not identify the Spotify account: {error}"))?
        .id;
    {
        let current = state
            .spotify_library
            .lock()
            .expect("Spotify library mutex poisoned")
            .clone();
        if !current.account_id.is_empty() && current.account_id != account_id {
            let reset = SpotifyLibraryState {
                account_id: account_id.clone(),
                ..SpotifyLibraryState::default()
            };
            spotify_commands::replace_spotify_library_state(
                &state.sync_store,
                &state.spotify_library,
                reset,
            )?;
        }
    }
    let sync_provider =
        SpotifySyncProvider::for_account(provider.as_ref(), &state.sync_store, account_id)?;
    let first_sync = !state
        .settings
        .lock()
        .expect("settings mutex poisoned")
        .spotify_sync_completed;
    if first_sync {
        mutate_library(&state, |library| {
            *library = sync::without_fixtures(library)?;
            Ok(())
        })?;
        app.emit("library-changed", ())
            .map_err(|error| error.to_string())?;
    }
    let sync_progress = Mutex::new(SyncProgressState::default());
    let on_batch = |batch: SyncBatch| {
        let mut counts = sync_progress.lock().expect("sync progress mutex poisoned");
        let payload = counts.update(&batch);
        drop(counts);
        if let Err(error) = with_library_gate(&state, |library| {
            sync::apply_in_memory(library, batch.tracks);
            Ok(())
        }) {
            notify_error(app, error);
            return;
        }
        let _ = app.emit("library-changed", ());
        let _ = app.emit("sync-progress-count", payload);
    };
    let outcome = sync::snapshot(
        &sync_provider,
        |phase| {
            log::info!("{phase}");
            let _ = app.emit("sync-progress", phase);
        },
        &on_batch,
    )
    .await?;
    let sync::SnapshotOutcome {
        tracks,
        genres_degraded,
        partial,
        quota_exhausted,
        progress,
        earliest_cooldown,
        request_counts,
        spotify_library,
    } = outcome;
    let tracks_synced = sync_progress
        .lock()
        .expect("sync progress mutex poisoned")
        .tracks;
    app.emit(
        "sync-progress-count",
        SyncProgressCount {
            tracks: tracks_synced,
            fraction: 1.0,
        },
    )
    .map_err(|error| error.to_string())?;
    app.emit("sync-progress", "Saving library…")
        .map_err(|error| error.to_string())?;
    let spotify_library = spotify_library.map(|incoming| {
        let current = state
            .spotify_library
            .lock()
            .expect("Spotify library mutex poisoned")
            .clone();
        let mut merged = if current.is_exact() {
            current.merge_earliest_times(incoming)
        } else {
            incoming
        };
        let library = state.library.lock().expect("library mutex poisoned");
        let added_times = library
            .tracks()
            .iter()
            .map(|track| (track.uri.as_str(), track.added_at))
            .collect::<HashMap<_, _>>();
        let aliases = sync::spotify_track_aliases(&library, &tracks);
        for album in merged.saved_albums.values_mut() {
            if album.added_at.is_none() {
                album.added_at = album
                    .track_uris
                    .iter()
                    .filter_map(|uri| {
                        let local_uri = aliases.get(uri).unwrap_or(uri);
                        added_times.get(local_uri.as_str()).copied().flatten()
                    })
                    .min()
                    .or_else(|| Some(unix_now()));
            }
        }
        merged
    });
    if let Some(spotify_library) = spotify_library.as_ref() {
        state
            .sync_store
            .save_spotify_library(spotify_library)
            .map_err(|error| error.to_string())?;
        *state
            .spotify_library
            .lock()
            .expect("Spotify library mutex poisoned") = spotify_library.clone();
    }
    with_library_gate(&state, |library| {
        sync::apply(
            library,
            &state.store,
            first_sync,
            tracks,
            spotify_library.as_ref(),
        )
    })?;
    {
        let library = state.library.lock().expect("library mutex poisoned");
        log::info!(
            "Spotify sync applied; {} library tracks",
            library.tracks().len()
        );
    }
    if partial {
        let detail = progress
            .iter()
            .map(|progress| match progress.total {
                Some(total) => format!("{} of {total} {}", progress.done, progress.label),
                None => format!("{} pending", progress.label),
            })
            .collect::<Vec<_>>()
            .join(", ");
        let message = partial_import_message(
            &detail,
            quota_exhausted,
            earliest_cooldown,
            chrono::Local::now(),
        );
        log::warn!("{message}");
        if genres_degraded {
            log::warn!("{GENRES_DEGRADED_MSG}");
        }
        app.emit("sync-progress", message)
            .map_err(|error| error.to_string())?;
    } else if genres_degraded {
        log::warn!("{GENRES_DEGRADED_MSG}");
        app.emit("sync-progress", GENRES_DEGRADED_MSG)
            .map_err(|error| error.to_string())?;
    }
    {
        let mut settings = state.settings.lock().expect("settings mutex poisoned");
        if record_full_sync(&mut settings, partial, unix_now()) {
            state
                .settings_store
                .save(&settings)
                .map_err(|error| error.to_string())?;
        }
    }
    log::info!(
        "sync requests:{}",
        request_counts
            .iter()
            .map(|(family, count)| format!(" {family}={count}"))
            .collect::<String>()
    );
    app.emit("library-changed", ())
        .map_err(|error| error.to_string())?;
    if let Err(error) = sync_playlists(app, provider.as_ref()).await {
        log::warn!("Playlist sync failed: {error}");
    }
    Ok(SyncCompletion {
        partial,
        auto_resume: partial.then_some(earliest_cooldown).flatten(),
    })
}

async fn sync_playlists(app: &tauri::AppHandle, client: &SpotifyProvider) -> Result<(), String> {
    let state = app.state::<AppState>();
    let current = state
        .playlists
        .lock()
        .expect("playlist mutex poisoned")
        .clone();
    let synced = match playlists::sync(client, &current).await {
        Ok(synced) => synced,
        Err(error) => {
            let tokens = state.token_store.load().ok().flatten();
            if let Some(error) = dispatch_playlist_error(
                &state.playlist_reauth_notified,
                error,
                tokens.as_ref(),
                |error| notify_error(app, error),
            ) {
                return Err(error);
            }
            return Ok(());
        }
    };
    save_playlists(app, synced)
}

fn playlist_error(state: &AppState, error: retune_spotify::Error) -> String {
    let tokens = state.token_store.load().ok().flatten();
    playlists::map_error(error, tokens.as_ref())
}

fn dispatch_playlist_error(
    notified: &AtomicBool,
    error: retune_spotify::Error,
    tokens: Option<&Tokens>,
    notify: impl FnOnce(String),
) -> Option<String> {
    let error = playlists::map_error(error, tokens);
    if error != playlists::RECONNECT_HINT {
        return Some(error);
    }
    if !notified.swap(true, Ordering::Relaxed) {
        notify(error);
    }
    None
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn record_full_sync(settings: &mut Settings, partial: bool, now: u64) -> bool {
    if partial {
        return false;
    }
    settings.spotify_sync_completed = true;
    settings.last_full_sync = Some(now);
    true
}

fn initial_library(debug: bool) -> Library {
    if debug {
        fixture::library()
    } else {
        Library::new()
    }
}

fn album_page_view(
    library: &Library,
    spotify_library: &SpotifyLibraryState,
    album: Album,
) -> AlbumPageView {
    let artist = album.artists.first();
    let artist_name = artist.map(|artist| artist.name.clone()).unwrap_or_default();
    let total_duration_secs = album
        .tracks
        .as_ref()
        .into_iter()
        .flat_map(|page| &page.items)
        .map(|track| track.duration_ms.unwrap_or_default())
        .sum::<u64>()
        / 1_000;
    let local_added_at = album
        .tracks
        .as_ref()
        .into_iter()
        .flat_map(|page| &page.items)
        .filter_map(|track| {
            let normalized = retune_spotify::normalize::track(track, None, Some(&album));
            spotify_track_match(library, &normalized).and_then(|track| track.added_at)
        })
        .min();
    let tracks = album
        .tracks
        .clone()
        .map(|page| {
            page.items
                .into_iter()
                .map(|track| {
                    let uri = track.uri.clone();
                    let normalized = retune_spotify::normalize::track(&track, None, Some(&album));
                    let local = spotify_track_match(library, &normalized);
                    AlbumPageTrackView {
                        uri: uri.clone(),
                        name: track.name,
                        track_no: track.track_number,
                        duration_secs: track.duration_ms.unwrap_or_default() / 1_000,
                        enabled: local.is_none_or(|track| track.enabled),
                        track_id: local.map(|track| track.id.0),
                        saved_individually: if spotify_library.is_exact() {
                            spotify_library.saved_tracks.contains_key(&uri)
                        } else {
                            local.is_some()
                        },
                        rating: local
                            .and_then(|track| library.effective_rating(track.id).map(rating_view)),
                    }
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let content_complete =
        !tracks.is_empty() && tracks.iter().all(|track| track.track_id.is_some());
    let saved_album = if spotify_library.is_exact() {
        spotify_library.saved_albums.contains_key(&album.uri)
    } else {
        content_complete
    };
    let added_at = spotify_library
        .is_exact()
        .then(|| {
            spotify_library
                .saved_albums
                .get(&album.uri)
                .and_then(|album| album.added_at)
        })
        .flatten()
        .or(local_added_at);
    let album_rating = content_complete
        .then(|| {
            library.album_rating(&AlbumKey {
                source: SourceId::Music,
                art: artist_name.clone(),
                alb: album.name.clone(),
            })
        })
        .flatten()
        .map(Rating::stars);
    AlbumPageView {
        uri: album.uri,
        name: album.name,
        artist: artist_name,
        artist_id: artist.map(|artist| artist.id.clone()).unwrap_or_default(),
        album_type: title_case(album.album_type.as_deref().unwrap_or("album")),
        year: album
            .release_date
            .as_deref()
            .and_then(|date| date.get(..4))
            .map(str::to_owned),
        image_url: image_url(&album.images),
        total_duration_secs,
        saved_album,
        content_complete,
        added_at,
        album_rating,
        tracks,
    }
}

fn mark_album_membership(
    library: &Library,
    spotify_library: &SpotifyLibraryState,
    albums: &mut [provider::SearchAlbum],
) {
    for album in albums {
        if spotify_library.is_exact() {
            album.in_library = spotify_library.saved_albums.contains_key(&album.uri);
            continue;
        }
        // ponytail: local album identity is artist/title; store Spotify album URIs if
        // same-named editions become a real ambiguity.
        album.in_library = album.track_count > 0
            && library
                .tracks()
                .iter()
                .filter(|track| {
                    track.source == SourceId::Music
                        && track.art == album.artist
                        && track.alb == album.name
                })
                .count()
                >= album.track_count as usize;
    }
}

fn mark_track_membership(
    library: &Library,
    spotify_library: &SpotifyLibraryState,
    tracks: &mut [provider::SearchTrack],
) {
    for track in tracks {
        track.in_library = if spotify_library.is_exact() {
            spotify_library.saved_tracks.contains_key(&track.uri)
        } else {
            library
                .tracks()
                .iter()
                .any(|candidate| candidate.source == SourceId::Music && candidate.uri == track.uri)
        };
    }
}

fn album_track_uris(album: &Album) -> Vec<String> {
    album
        .tracks
        .as_ref()
        .into_iter()
        .flat_map(|page| &page.items)
        .map(|track| track.uri.clone())
        .collect()
}

#[cfg(test)]
fn remove_album_tracks(library: &mut Library, album: &Album) -> usize {
    library.remove_uris(&album_track_uris(album))
}

fn record_cooldown(
    sync_store: &FsSyncStore,
    endpoint: &str,
    kind: store::CooldownKind,
    deadline: u64,
    now: u64,
) -> Result<(), String> {
    let mut cooldowns = sync_store
        .cooldowns(now)
        .map_err(|error| error.to_string())?;
    cooldowns.insert(
        endpoint_family(endpoint),
        store::Cooldown { kind, deadline },
    );
    sync_store
        .save_cooldowns(&cooldowns)
        .map_err(|error| error.to_string())
}

async fn artist_albums_outcome<T: Transport, S: TokenStore>(
    provider: &SpotifyClient<T, S>,
    sync_store: &FsSyncStore,
    artist_id: &str,
    offset: u32,
    now: u64,
    display_now: chrono::DateTime<chrono::Local>,
) -> Result<ArtistAlbumsPage, String> {
    if let Some(cooldown) = sync_store
        .cooldowns(now)
        .map_err(|error| error.to_string())?
        .get("/artists")
        .copied()
    {
        let time = provider::format_resume_time(cooldown.deadline, display_now);
        return Err(match cooldown.kind {
            store::CooldownKind::Transient => {
                format!("Spotify artist albums are rate limited; try again {time}.")
            }
            store::CooldownKind::Quota => format!(
                "Spotify Development Mode quota is still exhausted; try artist albums again {time}."
            ),
        });
    }
    match artist_albums_page(provider, artist_id, offset).await {
        Ok(page) => Ok(page),
        Err(retune_spotify::Error::RateLimited {
            endpoint,
            retry_after_secs,
        }) => {
            let deadline = now.saturating_add(retry_after_secs);
            record_cooldown(
                sync_store,
                &endpoint,
                store::CooldownKind::Transient,
                deadline,
                now,
            )?;
            Err(format!(
                "Spotify artist albums are rate limited; try again {}.",
                provider::format_resume_time(deadline, display_now)
            ))
        }
        Err(retune_spotify::Error::QuotaExceeded {
            endpoint,
            retry_after_secs,
        }) => {
            if let Some(retry_after_secs) = retry_after_secs {
                let deadline = now.saturating_add(retry_after_secs);
                record_cooldown(
                    sync_store,
                    &endpoint,
                    store::CooldownKind::Quota,
                    deadline,
                    now,
                )?;
                Err(format!(
                    "Spotify Development Mode quota is exhausted; try artist albums again {}.",
                    provider::format_resume_time(deadline, display_now)
                ))
            } else {
                Err("Spotify Development Mode quota is exhausted; try artist albums again after Spotify resets it.".into())
            }
        }
        Err(error) => Err(error.to_string()),
    }
}

fn playlist_list_views(cache: &playlists::PlaylistCache, uris: &[String]) -> Vec<PlaylistListView> {
    cache
        .playlists
        .iter()
        .map(|playlist| PlaylistListView {
            id: playlist.id.clone(),
            name: playlist.name.clone(),
            owned: playlist.owned,
            owner: playlist.owner.clone(),
            contains: !uris.is_empty() && uris.iter().all(|uri| playlist.tracks.contains(uri)),
            track_count: playlist.track_count,
            items_available: playlist.owned
                || playlist.track_count == 0
                || !playlist.tracks.is_empty(),
        })
        .collect()
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SpotifyOpenTarget {
    App,
    Web,
}

fn spotify_item_link(kind: &str, id: &str, target: SpotifyOpenTarget) -> Result<String, String> {
    if id.is_empty()
        || !id
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
    {
        return Err("Invalid Spotify ID.".into());
    }
    Ok(match target {
        SpotifyOpenTarget::App => format!("spotify:{kind}:{id}"),
        SpotifyOpenTarget::Web => format!("https://open.spotify.com/{kind}/{id}"),
    })
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SpotifyDestination {
    Album,
    Artist,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum SpotifyNavigation {
    Album { uri: String, highlight: String },
    Artist { id: String },
}

fn spotify_track_destination(
    track: &SpotifyTrack,
    destination: SpotifyDestination,
) -> Result<SpotifyNavigation, String> {
    match destination {
        SpotifyDestination::Album => track
            .album
            .as_ref()
            .map(|album| SpotifyNavigation::Album {
                uri: album.uri.clone(),
                highlight: track.uri.clone(),
            })
            .ok_or_else(|| "Spotify album is unavailable.".into()),
        SpotifyDestination::Artist => track
            .artists
            .first()
            .map(|artist| SpotifyNavigation::Artist {
                id: artist.id.clone(),
            })
            .ok_or_else(|| "Spotify artist is unavailable.".into()),
    }
}

async fn playlist_add_inner(
    app: &tauri::AppHandle,
    id: String,
    uris: Vec<String>,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut cache = state
        .playlists
        .lock()
        .expect("playlist mutex poisoned")
        .clone();
    let library = state
        .library
        .lock()
        .expect("library mutex poisoned")
        .clone();
    let client = provider_from(&state)?;
    playlists::add(client.as_ref(), &mut cache, &library, &id, uris)
        .await
        .map_err(|error| match error {
            playlists::PlaylistAddError::Local(message) => message,
            playlists::PlaylistAddError::Unknown(id) => format!("Unknown playlist {id}"),
            playlists::PlaylistAddError::ReadOnly => "Only your playlists can be changed.".into(),
            playlists::PlaylistAddError::Spotify(error) => playlist_error(&state, error),
        })?;
    save_playlists(app, cache)
}

fn save_playlists(app: &tauri::AppHandle, cache: playlists::PlaylistCache) -> Result<(), String> {
    let state = app.state::<AppState>();
    state
        .playlist_store
        .save(&cache)
        .map_err(|error| error.to_string())?;
    *state.playlists.lock().expect("playlist mutex poisoned") = cache;
    app.emit("playlists-changed", ())
        .map_err(|error| error.to_string())
}

struct LibraryTransactionGuard {
    state: Arc<LibraryTransactionState>,
}

impl Drop for LibraryTransactionGuard {
    fn drop(&mut self) {
        let mut active = self
            .state
            .active
            .lock()
            .expect("library transaction mutex poisoned");
        *active = false;
        self.state.changed.notify_all();
    }
}

fn begin_library_transaction(state: &AppState) -> Result<LibraryTransactionGuard, String> {
    let mut active = state
        .library_transaction
        .active
        .lock()
        .map_err(|_| "library transaction mutex poisoned".to_string())?;
    if *active {
        return Err("Another library transaction is already applying.".to_string());
    }
    *active = true;
    drop(active);
    Ok(LibraryTransactionGuard {
        state: Arc::clone(&state.library_transaction),
    })
}

fn wait_for_library_transaction(
    state: &LibraryTransactionState,
) -> Result<std::sync::MutexGuard<'_, bool>, String> {
    let mut active = state
        .active
        .lock()
        .map_err(|_| "library transaction mutex poisoned".to_string())?;
    while *active {
        active = state
            .changed
            .wait(active)
            .map_err(|_| "library transaction mutex poisoned".to_string())?;
    }
    Ok(active)
}

fn with_library_gate<T>(
    state: &AppState,
    mutation: impl FnOnce(&mut Library) -> Result<T, String>,
) -> Result<T, String> {
    let _transaction_state = wait_for_library_transaction(&state.library_transaction)?;
    let _write_gate = state
        .library_write_gate
        .lock()
        .expect("library write gate poisoned");
    let mut library = state.library.lock().expect("library mutex poisoned");
    mutation(&mut library)
}

fn mutate_library<T>(
    state: &AppState,
    mutation: impl FnOnce(&mut Library) -> Result<T, String>,
) -> Result<T, String> {
    let _transaction_state = wait_for_library_transaction(&state.library_transaction)?;
    let write_gate = state
        .library_write_gate
        .lock()
        .expect("library write gate poisoned");
    mutate_library_locked(state, write_gate, mutation)
}

fn mutate_library_in_transaction<T>(
    state: &AppState,
    mutation: impl FnOnce(&mut Library) -> Result<T, String>,
) -> Result<T, String> {
    let write_gate = state
        .library_write_gate
        .lock()
        .expect("library write gate poisoned");
    mutate_library_locked(state, write_gate, mutation)
}

fn mutate_library_locked<T>(
    state: &AppState,
    _write_gate: std::sync::MutexGuard<'_, ()>,
    mutation: impl FnOnce(&mut Library) -> Result<T, String>,
) -> Result<T, String> {
    let mut current = state.library.lock().expect("library mutex poisoned");
    let mut next = current.clone();
    let value = mutation(&mut next)?;
    state.store.save(&next).map_err(|error| error.to_string())?;
    *current = next;
    Ok(value)
}

fn spotify_track_match<'a>(library: &'a Library, incoming: &NewTrack) -> Option<&'a TrackRecord> {
    library.tracks().iter().find(|existing| {
        existing.uri == incoming.uri
            || (existing.uri.starts_with("spotify:track:")
                && incoming.uri.starts_with("spotify:track:")
                && existing.source == incoming.source
                && existing.art == incoming.art
                && existing.alb == incoming.alb
                && existing.disc_no == incoming.disc_no
                && existing.track_no == incoming.track_no
                && existing.name == incoming.name
                && existing.duration == incoming.duration
                && existing.release_date == incoming.release_date)
    })
}

fn record_play(
    store: &impl OverlayStore,
    library: &Mutex<Library>,
    write_gate: &Mutex<()>,
    transaction: &LibraryTransactionState,
    uri: &str,
    played_at: u64,
) -> Result<bool, String> {
    let _transaction_state = wait_for_library_transaction(transaction)?;
    let _write_gate = write_gate.lock().expect("library write gate poisoned");
    let mut current = library.lock().expect("library mutex poisoned");
    if !current.tracks().iter().any(|track| track.uri == uri) {
        return Ok(false);
    }
    let mut next = current.clone();
    let track = next
        .tracks_mut()
        .iter_mut()
        .find(|track| track.uri == uri)
        .expect("track existence checked before cloning");
    track.play_count = track.play_count.saturating_add(1);
    track.last_played_at = Some(played_at);
    store.save(&next).map_err(|error| error.to_string())?;
    *current = next;
    Ok(true)
}

fn run_local_import(
    app: &tauri::AppHandle,
    paths: &[PathBuf],
) -> Result<localfiles::ImportSummary, String> {
    let state = app.state::<AppState>();
    let summary = with_library_gate(&state, |library| {
        localfiles::import_transaction(&state.store, library, paths)
    })?;
    for failure in &summary.failed {
        log::warn!(
            "local import failed for {}: {}",
            failure.path,
            failure.reason
        );
    }
    app.emit("library-changed", ())
        .map_err(|error| error.to_string())?;
    if let Some(error) = format_import_failures(&summary.failed) {
        app.emit("operation-error", error)
            .map_err(|error| error.to_string())?;
    }
    app.emit("local-import-complete", summary.clone())
        .map_err(|error| error.to_string())?;
    Ok(summary)
}

fn launch_local_import(app: tauri::AppHandle, paths: Vec<PathBuf>) {
    let _ = app.emit("local-import-started", ());
    drop(tauri::async_runtime::spawn_blocking(move || {
        if let Err(error) = run_local_import(&app, &paths) {
            notify_error(&app, error);
            let _ = app.emit("local-import-failed", ());
        }
    }));
}

fn format_import_failures(failures: &[localfiles::FailedImport]) -> Option<String> {
    if failures.is_empty() {
        return None;
    }
    let mut lines = failures
        .iter()
        .take(5)
        .map(|failure| format!("{} — {}", failure.path, failure.reason))
        .collect::<Vec<_>>();
    if failures.len() > 5 {
        lines.push(format!("+ {} more", failures.len() - 5));
    }
    Some(format!(
        "Some files could not be imported:\n{}",
        lines.join("\n")
    ))
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
                Ok(paths) => launch_local_import(handle.clone(), paths),
                Err(error) => {
                    notify_error(&handle, error);
                    let _ = handle.emit("local-import-failed", ());
                }
            }
        });
}

fn import_local_folder(app: &tauri::AppHandle) {
    let handle = app.clone();
    app.dialog().file().pick_folder(move |path| {
        let Some(path) = path else { return };
        match path.into_path() {
            Ok(path) => launch_local_import(handle.clone(), vec![path]),
            Err(error) => {
                notify_error(&handle, error.to_string());
                let _ = handle.emit("local-import-failed", ());
            }
        }
    });
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
            let _ = app.emit("get-info", ());
        }
        "setup_library" => {
            let _ = app.emit("open-setup", ());
        }
        "add_local_files" => import_local_files(app),
        "add_local_folder" => import_local_folder(app),
        "export_library" => export_library(app),
        "restore_library" => import_library(app, true),
        "merge_library" => import_library(app, false),
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
            let _ = app.emit("open-preferences", ());
        }
        "zoom_in" | "zoom_out" | "actual_size" | "toggle_zebra" | "toggle_browser"
        | "theme_system" | "theme_light" | "theme_dark" => {
            let _ = app.emit("view-action", event.id().as_ref());
        }
        "play_pause" | "previous" | "next" => {
            let _ = app.emit("player-action", event.id().as_ref());
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

fn export_library(app: &tauri::AppHandle) {
    let handle = app.clone();
    app.dialog()
        .file()
        .set_file_name("Retune Library.json")
        .add_filter("Retune Library", &["json"])
        .save_file(move |path| {
            let Some(path) = path else { return };
            let result = (|| -> Result<(), String> {
                let path = path.into_path().map_err(|error| error.to_string())?;
                let state = handle.state::<AppState>();
                let lastfm_mappings =
                    tauri::async_runtime::block_on(state.lastfm_import.export_mappings());
                let library = state.library.lock().expect("library mutex poisoned");
                let settings = state.settings.lock().expect("settings mutex poisoned");
                let playlists = state.playlists.lock().expect("playlist mutex poisoned");
                let bytes = export_with_settings_and_mappings(
                    &library,
                    &settings,
                    &playlists,
                    Some(&lastfm_mappings),
                )?;
                fs::write(path, bytes).map_err(|error| error.to_string())
            })();
            if let Err(error) = result {
                notify_error(&handle, error);
            }
        });
}

#[cfg(test)]
fn export_with_settings(
    library: &Library,
    settings: &Settings,
    playlists: &playlists::PlaylistCache,
) -> Result<Vec<u8>, String> {
    export_with_settings_and_mappings(library, settings, playlists, None)
}

fn export_with_settings_and_mappings(
    library: &Library,
    settings: &Settings,
    playlists: &playlists::PlaylistCache,
    lastfm_mappings: Option<&lastfm_import::PersistedLastFmMappings>,
) -> Result<Vec<u8>, String> {
    let mut envelope: serde_json::Value =
        serde_json::from_slice(&export_json(library)).map_err(|error| error.to_string())?;
    envelope
        .as_object_mut()
        .expect("core export is an object")
        .insert(
            "settings".into(),
            serde_json::to_value(ExportSettings::from_settings(settings))
                .map_err(|error| error.to_string())?,
        );
    envelope
        .as_object_mut()
        .expect("core export is an object")
        .insert(
            "playlists".into(),
            serde_json::to_value(playlists).map_err(|error| error.to_string())?,
        );
    if let Some(lastfm_mappings) = lastfm_mappings {
        envelope
            .as_object_mut()
            .expect("core export is an object")
            .insert(
                "lastfmMappings".into(),
                serde_json::to_value(lastfm_mappings).map_err(|error| error.to_string())?,
            );
    }
    serde_json::to_vec(&envelope).map_err(|error| error.to_string())
}

#[cfg(test)]
fn import_with_settings(
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

fn import_with_settings_and_mappings(
    bytes: &[u8],
    restore: bool,
) -> Result<ImportedBackup, String> {
    let mut json = Vec::new();
    if bytes.starts_with(&[0x1f, 0x8b]) {
        GzDecoder::new(bytes)
            .read_to_end(&mut json)
            .map_err(|error| error.to_string())?;
    } else {
        json.extend_from_slice(bytes);
    }
    let mut envelope: serde_json::Value =
        serde_json::from_slice(&json).map_err(|error| error.to_string())?;
    let settings: Option<ExportSettings> = envelope
        .as_object_mut()
        .and_then(|object| object.remove("settings"))
        .filter(|_| restore)
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| error.to_string())?;
    if let Some(settings) = &settings {
        settings.clone().apply_to(&mut Settings::default())?;
    }
    let playlists = envelope
        .as_object_mut()
        .and_then(|object| object.remove("playlists"))
        .filter(|_| restore)
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| error.to_string())?;
    let lastfm_mappings = envelope
        .as_object_mut()
        .and_then(|object| object.remove("lastfmMappings"))
        .filter(|_| restore)
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| error.to_string())?;
    if lastfm_mappings
        .as_ref()
        .is_some_and(|mappings: &lastfm_import::PersistedLastFmMappings| {
            mappings.version != lastfm_import::LASTFM_MAPPINGS_VERSION
        })
    {
        return Err("The Last.fm mappings version is unsupported.".into());
    }
    let library = import(&serde_json::to_vec(&envelope).map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())?;
    Ok((library, settings, playlists, lastfm_mappings))
}

fn import_library(app: &tauri::AppHandle, replace: bool) {
    let handle = app.clone();
    app.dialog()
        .file()
        .add_filter("Retune Library", &["json", "json.gz", "gz"])
        .pick_file(move |path| {
            let Some(path) = path else { return };
            let result = (|| -> Result<_, String> {
                let path = path.into_path().map_err(|error| error.to_string())?;
                let bytes = fs::read(path).map_err(|error| error.to_string())?;
                import_with_settings_and_mappings(&bytes, replace)
            })();
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
                                apply_import(
                                    &confirmed_handle,
                                    library,
                                    settings,
                                    playlists,
                                    lastfm_mappings,
                                    true,
                                );
                            }
                        });
                }
                Ok((library, _, _, _)) => apply_import(&handle, library, None, None, None, false),
                Err(error) => notify_error(&handle, error),
            }
        });
}

fn apply_import(
    app: &tauri::AppHandle,
    imported: Library,
    export_settings: Option<ExportSettings>,
    imported_playlists: Option<playlists::PlaylistCache>,
    imported_lastfm_mappings: Option<lastfm_import::PersistedLastFmMappings>,
    replace: bool,
) {
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
            if let Some(export_settings) = export_settings {
                if let Err(error) = apply_export_settings(app, export_settings) {
                    notify_error(app, error);
                    return;
                }
            }
            if let Some(playlists) = imported_playlists {
                if let Err(error) = save_playlists(app, playlists) {
                    notify_error(app, error);
                    return;
                }
            }
            if let Some(lastfm_mappings) = imported_lastfm_mappings {
                if let Err(error) = tauri::async_runtime::block_on(
                    state.lastfm_import.restore_mappings(lastfm_mappings),
                ) {
                    notify_error(app, error);
                    return;
                }
            }
            let _ = app.emit("library-changed", ());
        }
        Err(error) => notify_error(app, error),
    }
}

fn apply_export_settings(
    app: &tauri::AppHandle,
    export_settings: ExportSettings,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut settings = state
        .settings
        .lock()
        .expect("settings mutex poisoned")
        .clone();
    export_settings.apply_to(&mut settings)?;
    state
        .settings_store
        .save(&settings)
        .map_err(|error| error.to_string())?;
    if let Some(menu_checks) = &state.menu_checks {
        menu_checks
            .sync(&settings)
            .map_err(|error| error.to_string())?;
    }
    *state.settings.lock().expect("settings mutex poisoned") = settings.clone();
    app.emit("settings-changed", settings.clone())
        .map_err(|error| error.to_string())?;
    let playback = Arc::clone(&state.playback);
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let event = playback.set_shuffle(settings.shuffle).await;
        let _ = app.emit("player-state", event);
    });
    Ok(())
}

fn notify_error(app: &tauri::AppHandle, error: String) {
    log::error!("{error}");
    let _ = app.emit("operation-error", error);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init());
    #[cfg(all(desktop, not(test)))]
    let builder = builder.plugin(tauri_plugin_window_state::Builder::default().build());
    let app = builder.invoke_handler(tauri::generate_handler![
            library_commands::browse,
            library_commands::metadata_values,
            library_commands::click_track_star,
            library_commands::set_track_enabled,
            library_commands::set_album_rating,
            library_commands::get_track,
            library_commands::edit_track,
            library_commands::set_track_infos,
            library_commands::import_local,
            startup_notice,
            get_settings,
            set_settings,
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
            playback_commands::set_audio_settings,
            spotify_commands::track_artwork,
            lastfm::lastfm_state,
            lastfm::connect_lastfm,
            lastfm::finish_lastfm,
            lastfm::disconnect_lastfm,
            lastfm_import::open_lastfm_importer,
            lastfm_import::lastfm_import_state,
            lastfm_import::lastfm_import_queue,
            lastfm_import::lastfm_import_page,
            lastfm_import::start_lastfm_import,
            lastfm_import::sync_lastfm_plays,
            lastfm_import::lastfm_import_review,
            lastfm_import::lastfm_import_options,
            lastfm_import::lastfm_import_count_mode,
            lastfm_import::lastfm_import_search_terms,
            lastfm_import::lastfm_import_select_match,
            lastfm_import::lastfm_import_change_track,
            lastfm_import::lastfm_import_change_album,
            lastfm_import::lastfm_import_apply,
            lastfm_import::lastfm_import_prepare_accept_all,
            lastfm_import::lastfm_import_accept_all_page,
            diagnostics::load_diagnostics,
            diagnostics::email_diagnostics
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
            let store = FsOverlayStore::new(&app_data_dir);
            let (mut library, recovery_notice, needs_save) = match store.load() {
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
            let backfilled = localfiles::backfill_transaction(&store, &mut library)
                .map_err(std::io::Error::other)?;
            if needs_save && !backfilled {
                store.save(&library)?;
            }
            let settings_store = FsSettingsStore::new(&app_data_dir);
            let sync_store = FsSyncStore::new(&app_data_dir);
            let spotify_library = sync_store.spotify_library()?;
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
            let lastfm = lastfm::Service::new(
                &app_data_dir,
                use_dev_token_store,
                settings.lastfm_scrobbling,
            );
            let lastfm_import = lastfm_import::Service::new(&app_data_dir);
            // Native credential-store access can fail transiently; start
            // disconnected rather than aborting startup.
            let connection = match token_store.load() {
                Ok(tokens) => ConnectionState::from_tokens(tokens),
                Err(error) => {
                    log::warn!("Token store unavailable at startup: {error}");
                    ConnectionState::from_tokens(None)
                }
            };
            if connection.needs_reauth {
                log::info!(
                    "Spotify connection is missing playlist scopes: {}",
                    connection.missing_scopes.join(" ")
                );
            }
            menu_checks.sync_connection(&connection)?;
            let spotify = spotify_provider(&settings.spotify_client_id, Arc::clone(&token_store))
                .map_err(std::io::Error::other)?;
            let startup_action = startup_action(
                &connection,
                &settings.spotify_client_id,
                settings.auto_connect,
                settings.last_full_sync,
                unix_now(),
            );
            if connection.connected && startup_action == StartupAction::Nothing {
                log::info!("startup sync skipped; library fresh");
            }
            let activate_local = connection.connected
                && connection.playback_authorized
                && settings.playback_backend == "local";
            let initial_volume = settings.volume;
            let playback = Arc::new(Playback::new(
                &settings.repeat,
                settings.shuffle,
                settings.play_threshold_percent,
                AudioSettings {
                    bitrate: settings.streaming_bitrate,
                    normalize: settings.normalize_volume,
                    gapless: settings.gapless,
                },
                Some(app_data_dir.clone()),
            ));
            playback.set_local_requested(settings.playback_backend == "local");
            let media_keys = media_keys::MediaKeys::spawn(app.handle().clone());
            let lastfm_enabled = settings.lastfm_scrobbling;
            let lastfm_import_startup = Arc::clone(&lastfm_import);
            app.manage(AppState {
                library: Mutex::new(library),
                store,
                library_write_gate: Arc::new(Mutex::new(())),
                library_transaction: Arc::new(LibraryTransactionState::default()),
                spotify_library: Mutex::new(spotify_library),
                spotify_library_gate: tokio::sync::Mutex::new(()),
                settings: Mutex::new(settings),
                settings_store,
                sync_store,
                playlists: Mutex::new(playlists),
                playlist_store,
                menu_checks: Some(menu_checks),
                recovery_notice: Mutex::new(recovery_notice),
                token_store,
                spotify: Mutex::new(spotify),
                artwork_cache: Mutex::default(),
                playback: Arc::clone(&playback),
                lastfm: Arc::clone(&lastfm),
                lastfm_import,
                media_keys,
                sync_orchestrator: SyncOrchestrator::default(),
                playlist_reauth_notified: AtomicBool::new(false),
            });
            lastfm.attach_app(app.handle().clone());
            let lastfm_startup = Arc::clone(&lastfm);
            let profile_app = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                lastfm_startup.set_enabled(lastfm_enabled).await;
                let _ = set_lastfm_scrobbling(&profile_app, lastfm_enabled).await;
                lastfm_import::resume_persisted_import(profile_app.clone()).await;
                let _ = lastfm_import_startup.backfill_completed_mappings().await;
                if lastfm_startup.state().await.connected {
                    let _ = lastfm_import::sync_lastfm_plays(profile_app.clone()).await;
                }
            });
            let completion_app = app.handle().clone();
            let lastfm = Arc::clone(&lastfm);
            playback.listen(
                app.handle().clone(),
                move |uri| {
                    let handle = completion_app.clone();
                    drop(tauri::async_runtime::spawn_blocking(move || {
                        let state = handle.state::<AppState>();
                        match record_play(
                            &state.store,
                            &state.library,
                            &state.library_write_gate,
                            &state.library_transaction,
                            &uri,
                            unix_now(),
                        ) {
                            Ok(true) => {
                                if let Err(error) = handle.emit("library-changed", ()) {
                                    notify_error(&handle, error.to_string());
                                }
                            }
                            Ok(false) => {}
                            Err(error) => notify_error(&handle, error),
                        }
                    }));
                },
                move |fact| lastfm.handle_listening_fact(fact),
            );
            if activate_local
                || connection.connected
                || startup_action != StartupAction::Nothing
            {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
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
                            Ok(client) => sync_playlists(&handle, client.as_ref()).await,
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
        tauri::RunEvent::Resumed if ready => {
            let playback = Arc::clone(&app.state::<AppState>().playback);
            tauri::async_runtime::spawn(async move {
                playback.invalidate_local().await;
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
        library.tracks_mut()[0].play_count = 3;
        let library = Mutex::new(library);
        let store = RecordingOverlayStore::default();
        let write_gate = Mutex::new(());
        let transaction = LibraryTransactionState::default();

        assert!(record_play(
            &store,
            &library,
            &write_gate,
            &transaction,
            "spotify:track:track",
            123,
        )
        .unwrap());
        assert!(!record_play(
            &store,
            &library,
            &write_gate,
            &transaction,
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
        let mut library = Library::new();
        let id = library.add(metadata_track(
            "spotify:track:track",
            "Genre",
            "Artist",
            "Album",
        ));
        let library = Arc::new(Mutex::new(library));
        let store = Arc::new(RecordingOverlayStore::default());
        let write_gate = Arc::new(Mutex::new(()));
        let transaction = Arc::new(LibraryTransactionState::default());
        *transaction.active.lock().unwrap() = true;
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn({
            let library = Arc::clone(&library);
            let store = Arc::clone(&store);
            let write_gate = Arc::clone(&write_gate);
            let transaction = Arc::clone(&transaction);
            move || {
                started_tx.send(()).unwrap();
                let result = record_play(
                    store.as_ref(),
                    library.as_ref(),
                    write_gate.as_ref(),
                    transaction.as_ref(),
                    "spotify:track:track",
                    123,
                );
                finished_tx.send(result).unwrap();
            }
        });
        started_rx.recv().unwrap();
        assert!(finished_rx.recv_timeout(Duration::from_millis(50)).is_err());
        {
            let mut active = transaction.active.lock().unwrap();
            *active = false;
            transaction.changed.notify_all();
        }
        assert!(finished_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap());
        worker.join().unwrap();

        assert_eq!(library.lock().unwrap().get(id).unwrap().play_count, 1);
        assert_eq!(store.0.lock().unwrap().len(), 1);
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
        let track = &mut library.tracks_mut()[0];
        track.play_count = 4;
        track.last_played_at = Some(123);
        track.added_at = Some(100);
        track.kind = Some("Spotify".into());
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

    fn playlist_client(
        responses: impl IntoIterator<Item = Response>,
    ) -> SpotifyClient<FakeTransport, InMemoryTokenStore> {
        fake_client(responses, &auth::SCOPES)
    }

    #[tokio::test]
    async fn artist_albums_reports_persisted_quota_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSyncStore::new(dir.path());
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
        let store = FsSyncStore::new(dir.path());
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
        let store = FsSyncStore::new(dir.path());
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
            format!("Partial import ({detail}) — Spotify Development Mode quota is exhausted; sync again after Spotify resets it.")
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
            Response::json(200, track),
            Response::json(
                200,
                serde_json::json!({"uri": "spotify:track:missing", "name": "Missing"}),
            ),
        ]);
        let cache = Mutex::default();

        assert_eq!(track_id("spotify:track:track"), Some("track"));
        assert_eq!(track_id("spotify:album:album"), None);
        assert_eq!(track_id("spotify:track:"), None);
        assert_eq!(
            resolve_track_artwork(Some(&client), &cache, "spotify:track:track", 64)
                .await
                .as_deref(),
            Some("small")
        );
        assert_eq!(
            resolve_track_artwork(Some(&client), &cache, "spotify:track:track", 64).await,
            Some("small".into())
        );
        assert_eq!(
            resolve_track_artwork(Some(&client), &cache, "spotify:track:track", 300).await,
            Some("large".into())
        );
        assert_eq!(
            resolve_track_artwork(Some(&client), &cache, "spotify:album:album", 64).await,
            None
        );
        assert_eq!(
            resolve_track_artwork(Some(&client), &cache, "spotify:track:missing", 64).await,
            None
        );
        assert_eq!(
            resolve_track_artwork(Some(&client), &cache, "spotify:track:missing", 64).await,
            None
        );
        assert_eq!(client.transport().requests().len(), 3);
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
            &uri,
            64,
        )
        .await
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
                &uri,
                64
            )
            .await,
            Some(artwork)
        );

        let wav = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../crates/retune-audio/tests/fixtures/cc0-audio.wav");
        assert_eq!(
            resolve_track_artwork(
                None::<&SpotifyClient<FakeTransport, InMemoryTokenStore>>,
                &cache,
                &localfiles::file_uri(&wav),
                64
            )
            .await,
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

        let message = format_import_failures(&failures).unwrap();
        assert!(message.contains("file-1 — bad"));
        assert!(message.contains("file-5 — bad"));
        assert!(!message.contains("file-6 — bad"));
        assert!(message.ends_with("+ 2 more"));
        assert_eq!(format_import_failures(&[]), None);
    }

    #[test]
    fn playlist_membership_requires_all_nonempty_uris() {
        let mut cache = playlist_cache();

        assert!(playlist_list_views(&cache, &["one".into(), "two".into()])[0].contains);
        assert!(!playlist_list_views(&cache, &["one".into(), "missing".into()])[0].contains);
        assert!(!playlist_list_views(&cache, &["missing".into()])[0].contains);
        assert!(!playlist_list_views(&cache, &[])[0].contains);
        assert!(playlist_list_views(&cache, &[])[0].items_available);
        cache.playlists[0].owned = false;
        cache.playlists[0].tracks.clear();
        assert!(!playlist_list_views(&cache, &[])[0].items_available);
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
    fn album_page_resolves_library_ids_ratings_and_completeness() {
        let mut library = Library::new();
        let mut first = metadata_track("spotify:track:one", "Rock", "Artist", "Album");
        first.added_at = Some(42);
        let id = library.add(first);
        library.set_track_rating(id, Rating::new(4)).unwrap();
        library.set_album_rating(
            AlbumKey {
                source: SourceId::Music,
                art: "Artist".into(),
                alb: "Album".into(),
            },
            Rating::new(5),
        );

        let page = album_page_view(&library, &SpotifyLibraryState::default(), spotify_album());

        assert_eq!(page.album_type, "Compilation");
        assert_eq!(page.year.as_deref(), Some("2024"));
        assert_eq!(page.total_duration_secs, 4);
        assert!(!page.saved_album);
        assert!(!page.content_complete);
        assert_eq!(page.added_at, Some(42));
        assert_eq!(page.album_rating, None);
        assert_eq!(page.tracks[0].track_id, Some(id.0));
        assert_eq!(
            page.tracks[0].rating,
            Some(RatingView {
                stars: 4,
                explicit: true
            })
        );
        assert_eq!(page.tracks[1].track_id, None);

        library.add(metadata_track(
            "spotify:track:two",
            "Rock",
            "Artist",
            "Album",
        ));
        let page = album_page_view(&library, &SpotifyLibraryState::default(), spotify_album());
        assert!(page.saved_album);
        assert!(page.content_complete);
        assert_eq!(page.album_rating, Some(5));
    }

    #[test]
    fn album_page_resolves_alternate_track_uri_to_retained_overlay() {
        let mut library = Library::new();
        let mut retained = metadata_track("spotify:track:retained", "Rock", "Artist", "Album");
        retained.name = "One".into();
        retained.duration = Duration::from_millis(1_500);
        retained.track_no = Some(1);
        retained.disc_no = Some(1);
        retained.release_date = Some("2024-02-03".into());
        retained.kind = Some("Spotify".into());
        retained.added_at = Some(42);
        let id = library.add(retained);
        library.set_track_rating(id, Rating::new(4)).unwrap();
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
            Rating::new(5),
        );

        let mut album = spotify_album();
        album.tracks.as_mut().unwrap().items[0].uri = "spotify:track:alternate".into();
        let page = album_page_view(&library, &SpotifyLibraryState::default(), album);

        assert_eq!(page.tracks[0].track_id, Some(id.0));
        assert_eq!(
            page.tracks[0].rating,
            Some(RatingView {
                stars: 4,
                explicit: true
            })
        );
        assert!(page.content_complete);
        assert_eq!(page.album_rating, Some(5));
        assert_eq!(page.added_at, Some(42));
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
            let page = album_page_view(&library, &spotify_library, spotify_album());

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
    fn playlist_reconnect_hint_is_dispatched_once() {
        let notified = AtomicBool::new(false);
        let mut messages = vec![];
        let legacy = Tokens {
            access: String::new(),
            refresh: String::new(),
            expires_at: 0,
            scopes: "user-library-read".into(),
            playback_credentials: None,
        };
        let forbidden = || retune_spotify::Error::Http {
            endpoint: "/playlists/id/tracks".into(),
            status: 403,
            body: "Insufficient client scope".into(),
        };

        assert_eq!(
            dispatch_playlist_error(&notified, forbidden(), Some(&legacy), |message| {
                messages.push(message)
            }),
            None
        );
        assert_eq!(
            dispatch_playlist_error(&notified, forbidden(), Some(&legacy), |message| {
                messages.push(message)
            }),
            None
        );
        assert_eq!(messages, [playlists::RECONNECT_HINT]);
        let unrelated = retune_spotify::Error::Http {
            endpoint: "/playlists/id/tracks".into(),
            status: 500,
            body: "Server error".into(),
        };
        let expected = unrelated.to_string();
        assert_eq!(
            dispatch_playlist_error(&notified, unrelated, Some(&legacy), |_| panic!()),
            Some(expected)
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
        reconcile_lastfm_scrobbling_profile(&mut settings, "first-user", 10);
        assert_eq!(
            settings.lastfm_scrobbling_profile,
            Some(LastFmScrobblingProfile {
                username: "first-user".into(),
                started_at: 10,
            })
        );
        settings.lastfm_scrobbling = false;
        reconcile_lastfm_scrobbling_profile(&mut settings, "first-user", 20);
        assert_eq!(
            settings
                .lastfm_scrobbling_profile
                .as_ref()
                .unwrap()
                .started_at,
            10
        );
        reconcile_lastfm_scrobbling_profile(&mut settings, "second-user", 30);
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
            playback_backend: "local".into(),
            repeat: "all".into(),
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

        let (_, _, _, restored) = import_with_settings_and_mappings(&bytes, true).unwrap();
        assert_eq!(restored, Some(mappings.clone()));
        let (_, _, _, merged) = import_with_settings_and_mappings(&bytes, false).unwrap();
        assert!(merged.is_none());
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

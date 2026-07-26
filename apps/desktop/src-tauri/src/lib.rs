mod fixture;
#[cfg(debug_assertions)]
mod local_spike;
mod localfiles;
mod media_keys;
mod playback;
mod playlists;
mod provider;
mod store;
mod sync;
mod sync_orchestrator;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    io::{Read, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};

use playback::{AudioSettings, Playback, PlayerStateEvent, SnapshotTrack};
use provider::{
    artist_descriptor, image_url, spotify_id, title_case, MediaProvider, SearchAlbum,
    SearchResults, SpotifySyncProvider, SyncBatch,
};
use retune_core::{
    browse::{self, Selection},
    io::{export_json, import},
    model::{
        AlbumKey, EffectiveRating, Library, Rating, SourceId, TrackEdit, TrackId, TrackRecord,
    },
};
use retune_spotify::{
    auth::{self, LoopbackListener, Pkce},
    client::{Album, HttpTransport, SpotifyClient, Track as SpotifyTrack, Transport},
    normalize::UNCATEGORIZED,
    tokens::{
        migrate_token_store, CachedTokenStore, EncryptedFsTokenStore, KeychainTokenStore,
        TokenStore, Tokens,
    },
};
use serde::{Deserialize, Serialize};
use store::{
    BrowserPanes, FsOverlayStore, FsPlaylistStore, FsSettingsStore, FsSyncStore, OverlayStore,
    Settings, StoreError, Theme,
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

struct AppState {
    library: Mutex<Library>,
    store: FsOverlayStore,
    settings: Mutex<Settings>,
    settings_store: FsSettingsStore,
    sync_store: FsSyncStore,
    playlists: Mutex<playlists::PlaylistCache>,
    playlist_store: FsPlaylistStore,
    menu_checks: MenuChecks,
    recovery_notice: Mutex<Option<String>>,
    token_store: SharedTokenStore,
    spotify: Mutex<Option<Arc<SpotifyProvider>>>,
    artwork_cache: Mutex<HashMap<String, Option<String>>>,
    playback: Arc<Playback>,
    media_keys: media_keys::MediaKeys,
    sync_orchestrator: SyncOrchestrator,
    playlist_reauth_notified: AtomicBool,
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
            missing_scopes,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct VisualSettings {
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
    sort_column: Option<String>,
    #[serde(default)]
    sort_desc: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct BehavioralSettings {
    #[serde(default)]
    shuffle: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct ExportSettings {
    #[serde(flatten)]
    visual: VisualSettings,
    #[serde(flatten)]
    behavioral: BehavioralSettings,
}

impl ExportSettings {
    fn from_settings(settings: &Settings) -> Self {
        Self {
            visual: VisualSettings::from_settings(settings),
            behavioral: BehavioralSettings {
                shuffle: settings.shuffle,
            },
        }
    }

    fn apply_to(self, settings: &mut Settings) {
        self.visual.apply_to(settings);
        settings.shuffle = self.behavioral.shuffle;
    }
}

impl VisualSettings {
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
            sort_column: settings.sort_column.clone(),
            sort_desc: settings.sort_desc,
        }
    }

    fn apply_to(self, settings: &mut Settings) {
        settings.theme = self.theme;
        settings.zoom = self.zoom;
        settings.zebra = self.zebra;
        settings.pl_collapsed = self.pl_collapsed;
        settings.browser_visible = self.browser_visible;
        settings.browser_panes = self.browser_panes;
        settings.column_order = self.column_order;
        settings.column_widths = self.column_widths;
        settings.hidden_columns = self.hidden_columns;
        settings.sort_column = self.sort_column;
        settings.sort_desc = self.sort_desc;
        settings.normalize();
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
    track_no: Option<u32>,
    duration_secs: u64,
    play_count: u32,
    last_played_at: Option<u64>,
    added_at: Option<u64>,
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
            track_no: track.track_no,
            duration_secs: track.duration.as_secs(),
            play_count: track.play_count,
            last_played_at: track.last_played_at,
            added_at: track.added_at,
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
    track_id: Option<u64>,
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
    in_library: bool,
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
    albums: Vec<SearchAlbum>,
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
    duration_secs: u64,
    rating: Option<RatingView>,
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
    let selected_tracks = browse::tracks(&library, source, &selection)
        .into_iter()
        .filter(|track| {
            query.is_empty()
                || [&track.name, &track.art, &track.alb, &track.cat]
                    .iter()
                    .any(|value| value.to_lowercase().contains(&query))
        })
        .collect::<Vec<_>>();
    let (album_rating, album_rating_artist, album_rating_ambiguous) =
        album_rating_view(&library, &selection, &selected_tracks);
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
        counts: counts(&library, source, &selection, &query),
    }
}

#[tauri::command]
fn metadata_values(state: tauri::State<'_, AppState>) -> MetadataValues {
    let library = state.library.lock().expect("library mutex poisoned");
    collect_metadata_values(&library)
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
    Ok(TrackInfoView::from_track(&library, track))
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
    let rating_change = rating_change(&edit)?;
    mutate_library(&state, |library| {
        apply_track_info(library, id, &edit, rating_change)
    })
}

#[tauri::command]
fn set_track_infos(app: tauri::AppHandle, ids: Vec<u64>, edit: TrackEditDto) -> Result<(), String> {
    let state = app.state::<AppState>();
    mutate_library(&state, |library| apply_track_infos(library, &ids, &edit))?;
    app.emit("library-changed", ())
        .map_err(|error| error.to_string())
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
    settings.validate().map_err(|error| error.to_string())?;
    // Compare against the ACTIVE backend, not the persisted setting: a failed
    // activation (e.g. under-scoped token at startup) leaves the setting on
    // "local" while playback fell back to Connect, and re-selecting the radio
    // must retry the switch.
    let wants_local = settings.playback_backend == "local";
    if wants_local != state.playback.is_local_active().await {
        let switch = if wants_local {
            switch_to_local(&state, settings.volume).await
        } else {
            state.playback.switch_to_connect().await;
            Ok(())
        };
        if let Err(error) = switch {
            app.emit("operation-error", error)
                .map_err(|error| error.to_string())?;
            app.emit("settings-changed", current)
                .map_err(|error| error.to_string())?;
            return Ok(());
        }
    }
    state
        .settings_store
        .save(&settings)
        .map_err(|error| error.to_string())?;
    *state.settings.lock().expect("settings mutex poisoned") = settings.clone();
    state
        .playback
        .set_play_threshold_percent(settings.play_threshold_percent)
        .await;
    state
        .menu_checks
        .sync(&settings)
        .map_err(|error| error.to_string())?;
    if client_id_changed {
        *state.spotify.lock().expect("spotify mutex poisoned") =
            spotify_provider(&settings.spotify_client_id, Arc::clone(&state.token_store))?;
    }
    Ok(())
}

async fn switch_to_local(state: &AppState, volume: u8) -> Result<(), String> {
    let stored = state
        .token_store
        .load()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Connect to Spotify before enabling built-in playback.".to_string())?;
    if !stored
        .scopes
        .split_whitespace()
        .any(|scope| scope == "streaming")
    {
        return Err("Reconnect to Spotify to grant playback permission (Account → Disconnect, then Connect).".into());
    }
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

#[tauri::command]
fn connection_state(state: tauri::State<'_, AppState>) -> Result<ConnectionState, String> {
    stored_connection_state(&state.token_store)
}

fn emit_connection_state(app: &tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let connection = stored_connection_state(&state.token_store)?;
    state
        .menu_checks
        .sync_connection(&connection)
        .map_err(|error| error.to_string())?;
    app.emit("connection-changed", connection)
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn connect_spotify(app: tauri::AppHandle) -> Result<(), String> {
    let client_id = app
        .state::<AppState>()
        .settings
        .lock()
        .expect("settings mutex poisoned")
        .spotify_client_id
        .trim()
        .to_owned();
    if client_id.is_empty() {
        return Err("Spotify Client ID is missing. Add it in Preferences, then try again.".into());
    }

    // Fixed port: Spotify matches redirect URIs exactly, so the dashboard
    // registration must be http://127.0.0.1:8898/callback.
    let listener = LoopbackListener::bind_on(8898).map_err(|error| error.to_string())?;
    let redirect_uri = listener.redirect_uri().map_err(|error| error.to_string())?;
    let state = auth::random_state();
    let pkce = Pkce::generate();
    let url = auth::authorize_url(&client_id, &redirect_uri, &state, &pkce.challenge)
        .map_err(|error| error.to_string())?;
    app.opener()
        .open_url(url.to_string(), None::<String>)
        .map_err(|error| error.to_string())?;
    let callback = tauri::async_runtime::spawn_blocking(move || {
        listener.accept(&state, Duration::from_secs(180))
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())?;
    let token = auth::exchange_code(
        &reqwest::Client::new(),
        &client_id,
        &callback.code,
        &redirect_uri,
        &pkce.verifier,
    )
    .await
    .map_err(|error| error.to_string())?;
    let refresh = token
        .refresh_token
        .ok_or_else(|| "Spotify did not return a refresh token".to_string())?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_secs();
    let state = app.state::<AppState>();
    let granted_scopes = token.scope.unwrap_or_else(|| auth::SCOPES.into());
    state
        .token_store
        .save(&Tokens {
            access: token.access_token,
            refresh,
            expires_at: now.saturating_add(token.expires_in),
            scopes: granted_scopes,
        })
        .map_err(|error| error.to_string())?;
    *state.spotify.lock().expect("spotify mutex poisoned") =
        spotify_provider(&client_id, Arc::clone(&state.token_store))?;
    set_auto_connect(&app, true)?;
    emit_connection_state(&app)?;
    sync_spotify(&app).await
}

#[tauri::command]
async fn disconnect_spotify(app: tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let client = state
        .spotify
        .lock()
        .expect("spotify mutex poisoned")
        .clone();
    state.playback.stop(client.as_deref()).await?;
    state.playback.switch_to_connect().await;
    set_auto_connect(&app, false)?;
    app.state::<AppState>()
        .token_store
        .clear()
        .map_err(|error| error.to_string())?;
    emit_connection_state(&app)?;
    let shuffle = state
        .settings
        .lock()
        .expect("settings mutex poisoned")
        .shuffle;
    app.emit("player-state", empty_player_state(shuffle))
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

async fn resolve_track_artwork<T: Transport, S: TokenStore>(
    client: Option<&SpotifyClient<T, S>>,
    cache: &Mutex<HashMap<String, Option<String>>>,
    uri: &str,
) -> Option<String> {
    let local = uri.starts_with("file:");
    let id = (!local).then(|| track_id(uri)).flatten();
    if !local && id.is_none() {
        return None;
    }
    if let Some(cached) = cache
        .lock()
        .expect("artwork cache mutex poisoned")
        .get(uri)
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
            .and_then(|album| image_url(&album.images))
    };
    let mut cache = cache.lock().expect("artwork cache mutex poisoned");
    if cache.len() >= 512 {
        cache.clear();
    }
    cache.insert(uri.into(), artwork.clone());
    artwork
}

#[tauri::command]
async fn track_artwork(
    state: tauri::State<'_, AppState>,
    uri: String,
) -> Result<Option<String>, String> {
    let provider = provider_from(&state).ok();
    Ok(resolve_track_artwork(provider.as_deref(), &state.artwork_cache, &uri).await)
}

pub(crate) async fn publish_media_artwork(app: tauri::AppHandle, event: PlayerStateEvent) {
    let state = app.state::<AppState>();
    let provider = provider_from(&state).ok();
    let Some(url) = resolve_track_artwork(
        provider.as_deref(),
        &state.artwork_cache,
        event.uri.as_deref().unwrap_or_default(),
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

async fn sync_spotify_inner(app: &tauri::AppHandle) -> Result<SyncCompletion, String> {
    log::info!("Starting Spotify sync");
    let state = app.state::<AppState>();
    if !stored_connection_state(&state.token_store)?.connected {
        return Err("Connect to Spotify before syncing.".into());
    }
    let client_id = state
        .settings
        .lock()
        .expect("settings mutex poisoned")
        .spotify_client_id
        .clone();
    let provider =
        spotify_provider(&client_id, Arc::clone(&state.token_store))?.ok_or_else(|| {
            "Spotify Client ID is missing. Add it in Preferences, then try again.".to_string()
        })?;
    *state.spotify.lock().expect("spotify mutex poisoned") = Some(provider.clone());
    let sync_provider = SpotifySyncProvider::new(provider.as_ref(), &state.sync_store)?;
    let first_sync = !state
        .settings
        .lock()
        .expect("settings mutex poisoned")
        .spotify_sync_completed;
    if first_sync {
        let mut library = state.library.lock().expect("library mutex poisoned");
        *library = sync::without_fixtures(&library)?;
        drop(library);
        app.emit("library-changed", ())
            .map_err(|error| error.to_string())?;
    }
    let sync_progress = Mutex::new(SyncProgressState::default());
    let on_batch = |batch: SyncBatch| {
        let mut counts = sync_progress.lock().expect("sync progress mutex poisoned");
        let payload = counts.update(&batch);
        drop(counts);
        let mut library = state.library.lock().expect("library mutex poisoned");
        sync::apply_in_memory(&mut library, batch.tracks);
        drop(library);
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
        progress,
        earliest_cooldown,
        request_counts,
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
    {
        let mut library = state.library.lock().expect("library mutex poisoned");
        sync::apply(&mut library, &state.store, first_sync, tracks)?;
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
        let message = earliest_cooldown.map_or_else(
            || {
                if detail.is_empty() {
                    "Partial import (Spotify rate limit) — run File → Sync later to finish.".into()
                } else {
                    format!("Partial import ({detail}) — run File → Sync later to finish.")
                }
            },
            |deadline| {
                let time = provider::format_resume_time(deadline, chrono::Local::now());
                if detail.is_empty() {
                    format!("Partial import — will finish automatically after {time}.")
                } else {
                    format!("Partial import — {detail} — will finish automatically after {time}.")
                }
            },
        );
        log::warn!("{message}");
        if genres_degraded {
            log::warn!(
                "Imported without genres (Spotify rate limit) — genres will fill in on a later sync."
            );
        }
        app.emit("sync-progress", message)
            .map_err(|error| error.to_string())?;
    } else if genres_degraded {
        let message =
            "Imported without genres (Spotify rate limit) — genres will fill in on a later sync.";
        log::warn!("{message}");
        app.emit("sync-progress", message)
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
    let library = state
        .library
        .lock()
        .expect("library mutex poisoned")
        .clone();
    let synced = match playlists::sync(client, &current, &library).await {
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
    state
        .playlist_store
        .save(&synced)
        .map_err(|error| error.to_string())?;
    *state.playlists.lock().expect("playlist mutex poisoned") = synced;
    app.emit("playlists-changed", ())
        .map_err(|error| error.to_string())
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

fn unix_now() -> u64 {
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

#[tauri::command]
async fn sync_from_spotify(app: tauri::AppHandle) -> Result<(), String> {
    sync_spotify(&app).await
}

#[tauri::command]
async fn spotify_search(
    state: tauri::State<'_, AppState>,
    query: String,
) -> Result<SearchResults, String> {
    if query.trim().is_empty() {
        return Ok(SearchResults {
            artists: vec![],
            albums: vec![],
            tracks: vec![],
        });
    }
    if !stored_connection_state(&state.token_store)?.connected {
        return Err("Connect to Spotify to search.".into());
    }
    let provider = provider_from(&state)?;
    MediaProvider::search(provider.as_ref(), query.trim()).await
}

fn album_page_view(library: &Library, album: Album) -> AlbumPageView {
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
    let tracks = album
        .tracks
        .map(|page| {
            page.items
                .into_iter()
                .map(|track| {
                    let local = library
                        .tracks()
                        .iter()
                        .find(|candidate| candidate.uri == track.uri);
                    AlbumPageTrackView {
                        uri: track.uri,
                        name: track.name,
                        track_no: track.track_number,
                        duration_secs: track.duration_ms.unwrap_or_default() / 1_000,
                        track_id: local.map(|track| track.id.0),
                        rating: local
                            .and_then(|track| library.effective_rating(track.id).map(rating_view)),
                    }
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let in_library = !tracks.is_empty() && tracks.iter().all(|track| track.track_id.is_some());
    let album_rating = in_library
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
        in_library,
        album_rating,
        tracks,
    }
}

fn remove_album_tracks(library: &mut Library, album: &Album) -> usize {
    let uris = album
        .tracks
        .as_ref()
        .into_iter()
        .flat_map(|page| &page.items)
        .map(|track| track.uri.clone())
        .collect::<Vec<_>>();
    library.remove_uris(&uris)
}

#[tauri::command]
async fn spotify_album_page(
    state: tauri::State<'_, AppState>,
    uri: String,
) -> Result<AlbumPageView, String> {
    let provider = provider_from(&state)?;
    let album = provider
        .album(spotify_id(&uri))
        .await
        .map_err(|error| error.to_string())?;
    let library = state.library.lock().expect("library mutex poisoned");
    Ok(album_page_view(&library, album))
}

#[tauri::command(rename_all = "camelCase")]
async fn spotify_artist_page(
    state: tauri::State<'_, AppState>,
    artist_id: String,
) -> Result<ArtistPageView, String> {
    let provider = provider_from(&state)?;
    let id = spotify_id(&artist_id);
    let artist = provider
        .artist(id)
        .await
        .map_err(|error| error.to_string())?;
    let following = match provider.is_following_artist(id).await {
        Ok(following) => following,
        Err(error) => {
            log::warn!("Could not read Spotify follow state for artist {id}: {error}");
            false
        }
    };
    let albums = MediaProvider::artist_albums(provider.as_ref(), id).await?;
    Ok(ArtistPageView {
        id: artist.id.clone(),
        name: artist.name.clone(),
        descriptor: artist_descriptor(&artist),
        image_url: image_url(&artist.images),
        following,
        albums,
    })
}

#[tauri::command(rename_all = "camelCase")]
async fn spotify_follow_artist(
    state: tauri::State<'_, AppState>,
    artist_id: String,
    follow: bool,
) -> Result<(), String> {
    provider_from(&state)?
        .follow_artist(spotify_id(&artist_id), follow)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "camelCase")]
async fn spotify_artist_albums(
    state: tauri::State<'_, AppState>,
    artist_id: String,
) -> Result<Vec<SearchAlbum>, String> {
    let provider = provider_from(&state)?;
    MediaProvider::artist_albums(provider.as_ref(), &artist_id).await
}

#[tauri::command]
async fn add_spotify_album(
    app: tauri::AppHandle,
    uri: String,
    name: String,
    artist: String,
) -> Result<(), String> {
    playlists::reject_local_uris(std::slice::from_ref(&uri), |_| Some(name.clone()))?;
    let state = app.state::<AppState>();
    let provider = provider_from(&state)?;
    let mut tracks = MediaProvider::album_tracks(provider.as_ref(), &uri).await?;
    for track in &mut tracks {
        if track.alb.is_empty() {
            track.alb.clone_from(&name);
        }
        if track.art.is_empty() {
            track.art.clone_from(&artist);
        }
    }
    let uris = tracks
        .iter()
        .map(|track| track.uri.clone())
        .collect::<Vec<_>>();
    playlists::reject_local_uris(&uris, |uri| {
        tracks
            .iter()
            .find(|track| track.uri == uri)
            .map(|track| track.name.clone())
    })?;
    provider.save_to_spotify(&uris).await?;
    mutate_library(&state, |library| {
        for track in tracks {
            library.add(track);
        }
        Ok(())
    })?;
    app.emit("library-changed", ())
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn remove_spotify_album(app: tauri::AppHandle, uri: String) -> Result<(), String> {
    let state = app.state::<AppState>();
    let provider = provider_from(&state)?;
    let id = spotify_id(&uri);
    let album = provider
        .album(id)
        .await
        .map_err(|error| error.to_string())?;
    provider
        .remove_saved_album(id)
        .await
        .map_err(|error| error.to_string())?;
    mutate_library(&state, |library| {
        remove_album_tracks(library, &album);
        Ok(())
    })?;
    app.emit("library-changed", ())
        .map_err(|error| error.to_string())
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

#[tauri::command(rename_all = "camelCase")]
fn playlists_list(
    state: tauri::State<'_, AppState>,
    uris: Option<Vec<String>>,
) -> Vec<PlaylistListView> {
    let uris = uris.unwrap_or_default();
    playlist_list_views(
        &state.playlists.lock().expect("playlist mutex poisoned"),
        &uris,
    )
}

fn spotify_item_url(kind: &str, id: &str) -> Result<String, String> {
    if id.is_empty()
        || !id
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
    {
        return Err("Invalid Spotify ID.".into());
    }
    Ok(format!("https://open.spotify.com/{kind}/{id}"))
}

#[tauri::command]
fn open_spotify_playlist(id: String) -> Result<(), String> {
    tauri_plugin_opener::open_url(spotify_item_url("playlist", &id)?, None::<&str>)
        .map_err(|error| error.to_string())
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

#[tauri::command]
async fn resolve_spotify_track_destination(
    state: tauri::State<'_, AppState>,
    uri: String,
    destination: SpotifyDestination,
) -> Result<SpotifyNavigation, String> {
    let id = track_id(&uri).ok_or("This track is not from Spotify.")?;
    let track = provider_from(&state)?
        .track(id)
        .await
        .map_err(|error| error.to_string())?;
    spotify_track_destination(&track, destination)
}

#[tauri::command]
fn reorder_playlists(app: tauri::AppHandle, ids: Vec<String>) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut cache = state
        .playlists
        .lock()
        .expect("playlist mutex poisoned")
        .clone();
    playlists::reorder_playlists(&mut cache, &ids)?;
    state
        .playlist_store
        .save(&cache)
        .map_err(|error| error.to_string())?;
    *state.playlists.lock().expect("playlist mutex poisoned") = cache;
    app.emit("playlists-changed", ())
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn playlist_unfollow(app: tauri::AppHandle, id: String) -> Result<(), String> {
    let state = app.state::<AppState>();
    let client = provider_from(&state)?;
    let mut cache = state
        .playlists
        .lock()
        .expect("playlist mutex poisoned")
        .clone();
    playlists::unfollow(client.as_ref(), &mut cache, &id)
        .await
        .map_err(|error| playlist_error(&state, error))?;
    state
        .playlist_store
        .save(&cache)
        .map_err(|error| error.to_string())?;
    *state.playlists.lock().expect("playlist mutex poisoned") = cache;
    app.emit("playlists-changed", ())
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn playlist_tracks(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<Vec<PlaylistTrackView>, String> {
    let playlists = state.playlists.lock().expect("playlist mutex poisoned");
    let playlist = playlists
        .playlists
        .iter()
        .find(|playlist| playlist.id == id)
        .ok_or_else(|| format!("Unknown playlist {id}"))?;
    let library = state.library.lock().expect("library mutex poisoned");
    Ok(playlist
        .tracks
        .iter()
        .map(|uri| {
            if let Some(track) = library.tracks().iter().find(|track| &track.uri == uri) {
                PlaylistTrackView {
                    id: Some(track.id.0),
                    uri: track.uri.clone(),
                    name: track.name.clone(),
                    art: track.art.clone(),
                    alb: track.alb.clone(),
                    duration_secs: track.duration.as_secs(),
                    rating: library.effective_rating(track.id).map(rating_view),
                }
            } else {
                let cached = playlist
                    .non_library_tracks
                    .iter()
                    .find(|track| &track.uri == uri);
                PlaylistTrackView {
                    id: None,
                    uri: uri.clone(),
                    name: cached.map(|track| track.name.clone()).unwrap_or_default(),
                    art: cached.map(|track| track.art.clone()).unwrap_or_default(),
                    alb: cached.map(|track| track.alb.clone()).unwrap_or_default(),
                    duration_secs: cached.map_or(0, |track| track.duration / 1000),
                    rating: None,
                }
            }
        })
        .collect())
}

#[tauri::command]
async fn playlist_create(app: tauri::AppHandle, name: String) -> Result<PlaylistListView, String> {
    let state = app.state::<AppState>();
    let name = name.trim();
    if name.is_empty() {
        return Err("Playlist name is required.".into());
    }
    let mut cache = state
        .playlists
        .lock()
        .expect("playlist mutex poisoned")
        .clone();
    let client = provider_from(&state)?;
    playlists::create(client.as_ref(), &mut cache, name)
        .await
        .map_err(|error| playlist_error(&state, error))?;
    let created = playlist_list_views(&cache, &[])
        .pop()
        .expect("create inserted playlist");
    state
        .playlist_store
        .save(&cache)
        .map_err(|error| error.to_string())?;
    *state.playlists.lock().expect("playlist mutex poisoned") = cache;
    app.emit("playlists-changed", ())
        .map_err(|error| error.to_string())?;
    Ok(created)
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
    state
        .playlist_store
        .save(&cache)
        .map_err(|error| error.to_string())?;
    *state.playlists.lock().expect("playlist mutex poisoned") = cache;
    app.emit("playlists-changed", ())
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn playlist_add(app: tauri::AppHandle, id: String, uris: Vec<String>) -> Result<(), String> {
    playlist_add_inner(&app, id, uris).await
}

#[tauri::command(rename_all = "camelCase")]
async fn playlist_add_album(
    app: tauri::AppHandle,
    id: String,
    album_uri: String,
    album_label: Option<String>,
) -> Result<(), String> {
    playlists::reject_local_uris(std::slice::from_ref(&album_uri), |_| album_label.clone())?;
    let provider = provider_from(&app.state::<AppState>())?;
    let tracks = MediaProvider::album_tracks(provider.as_ref(), &album_uri).await?;
    let uris = tracks.into_iter().map(|track| track.uri).collect();
    playlist_add_inner(&app, id, uris).await
}

#[tauri::command(rename_all = "camelCase")]
async fn playlist_reorder(
    app: tauri::AppHandle,
    id: String,
    range_start: u32,
    insert_before: u32,
    range_length: u32,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let client = provider_from(&state)?;
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
    let result = playlists::reorder(
        client.as_ref(),
        &mut cache,
        &library,
        &id,
        range_start,
        insert_before,
        range_length,
    )
    .await;
    match result {
        Ok(()) => {
            state
                .playlist_store
                .save(&cache)
                .map_err(|error| error.to_string())?;
            *state.playlists.lock().expect("playlist mutex poisoned") = cache;
            app.emit("playlists-changed", ())
                .map_err(|error| error.to_string())?;
            Ok(())
        }
        Err(playlists::PlaylistReorderError::Reloaded) => {
            state
                .playlist_store
                .save(&cache)
                .map_err(|error| error.to_string())?;
            *state.playlists.lock().expect("playlist mutex poisoned") = cache;
            app.emit("playlists-changed", ())
                .map_err(|error| error.to_string())?;
            Err(playlists::STALE_PLAYLIST.into())
        }
        Err(playlists::PlaylistReorderError::Spotify(error)) => Err(playlist_error(&state, error)),
        Err(playlists::PlaylistReorderError::Other(error)) => Err(error),
    }
}

#[tauri::command]
async fn playlist_remove(
    app: tauri::AppHandle,
    id: String,
    indices: Vec<u32>,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let client = provider_from(&state)?;
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
    let result = playlists::remove(client.as_ref(), &mut cache, &library, &id, &indices).await;
    match result {
        Ok(()) => {
            state
                .playlist_store
                .save(&cache)
                .map_err(|error| error.to_string())?;
            *state.playlists.lock().expect("playlist mutex poisoned") = cache;
            app.emit("playlists-changed", ())
                .map_err(|error| error.to_string())?;
            Ok(())
        }
        Err(playlists::PlaylistRemoveError::Reloaded) => {
            state
                .playlist_store
                .save(&cache)
                .map_err(|error| error.to_string())?;
            *state.playlists.lock().expect("playlist mutex poisoned") = cache;
            app.emit("playlists-changed", ())
                .map_err(|error| error.to_string())?;
            Err(playlists::STALE_PLAYLIST.into())
        }
        Err(playlists::PlaylistRemoveError::Spotify(error)) => Err(playlist_error(&state, error)),
        Err(playlists::PlaylistRemoveError::Other(error)) => Err(error),
    }
}

#[tauri::command(rename_all = "camelCase")]
async fn play_tracks(
    app: tauri::AppHandle,
    snapshot: Vec<SnapshotTrack>,
    start_index: usize,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let client = provider_from(&state).ok();
    state.playback.play(client, snapshot, start_index).await
}

#[tauri::command]
async fn player_toggle(app: tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let client = provider_from(&state).ok();
    state.playback.toggle(client.as_deref()).await
}

#[tauri::command]
async fn player_next(app: tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let client = provider_from(&state).ok();
    state.playback.next(client).await
}

#[tauri::command]
async fn player_prev(app: tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let client = provider_from(&state).ok();
    state.playback.prev(client).await
}

#[tauri::command]
async fn player_seek(app: tauri::AppHandle, seconds: u64) -> Result<(), String> {
    let state = app.state::<AppState>();
    let client = provider_from(&state).ok();
    state.playback.seek(client.as_deref(), seconds).await
}

#[tauri::command]
async fn player_set_volume(app: tauri::AppHandle, volume: u8) -> Result<(), String> {
    let state = app.state::<AppState>();
    // Persist first: the volume preference must survive even when applying it
    // live fails (disconnected, no active device).
    let mut settings = state
        .settings
        .lock()
        .expect("settings mutex poisoned")
        .clone();
    settings.volume = volume;
    state
        .settings_store
        .save(&settings)
        .map_err(|error| error.to_string())?;
    *state.settings.lock().expect("settings mutex poisoned") = settings.clone();
    app.emit("settings-changed", settings)
        .map_err(|error| error.to_string())?;
    let client = provider_from(&state).ok();
    let playback = Arc::clone(&state.playback);
    tauri::async_runtime::spawn_blocking(move || {
        tauri::async_runtime::block_on(playback.set_volume(client.as_deref(), volume))
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn set_repeat(app: tauri::AppHandle, mode: String) -> Result<(), String> {
    let state = app.state::<AppState>();
    let client = provider_from(&state).ok();
    state.playback.set_repeat(client.as_deref(), &mode).await?;
    let mut settings = state
        .settings
        .lock()
        .expect("settings mutex poisoned")
        .clone();
    settings.repeat = mode;
    state
        .settings_store
        .save(&settings)
        .map_err(|error| error.to_string())?;
    *state.settings.lock().expect("settings mutex poisoned") = settings;
    Ok(())
}

#[tauri::command]
async fn set_shuffle(app: tauri::AppHandle, shuffle: bool) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut settings = state
        .settings
        .lock()
        .expect("settings mutex poisoned")
        .clone();
    settings.shuffle = shuffle;
    state
        .settings_store
        .save(&settings)
        .map_err(|error| error.to_string())?;
    *state.settings.lock().expect("settings mutex poisoned") = settings.clone();
    app.emit("settings-changed", settings)
        .map_err(|error| error.to_string())?;
    let event = state.playback.set_shuffle(shuffle).await;
    app.emit("player-state", event)
        .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "camelCase")]
async fn set_audio_settings(
    app: tauri::AppHandle,
    streaming_bitrate: u16,
    normalize_volume: bool,
    gapless: bool,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut settings = state
        .settings
        .lock()
        .expect("settings mutex poisoned")
        .clone();
    let changed = settings.streaming_bitrate != streaming_bitrate
        || settings.normalize_volume != normalize_volume
        || settings.gapless != gapless;
    settings.streaming_bitrate = streaming_bitrate;
    settings.normalize_volume = normalize_volume;
    settings.gapless = gapless;
    state
        .settings_store
        .save(&settings)
        .map_err(|error| error.to_string())?;
    *state.settings.lock().expect("settings mutex poisoned") = settings.clone();
    app.emit("settings-changed", settings)
        .map_err(|error| error.to_string())?;
    state.playback.set_audio(AudioSettings {
        bitrate: streaming_bitrate,
        normalize: normalize_volume,
        gapless,
    });
    if changed && state.playback.is_local_active().await {
        state.playback.invalidate_local().await;
        if let Ok(client) = provider_from(&state) {
            if let Err(error) = state.playback.revalidate(client.as_ref()).await {
                log::warn!("Audio settings applied; session recreation deferred: {error}");
            }
        }
    }
    Ok(())
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

fn record_play(
    store: &impl OverlayStore,
    library: &Mutex<Library>,
    uri: &str,
    played_at: u64,
) -> Result<bool, String> {
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

#[tauri::command]
fn import_local(app: tauri::AppHandle, paths: Vec<String>) {
    launch_local_import(app, paths.into_iter().map(PathBuf::from).collect());
}

fn run_local_import(
    app: &tauri::AppHandle,
    paths: &[PathBuf],
) -> Result<localfiles::ImportSummary, String> {
    let state = app.state::<AppState>();
    let mut library = state.library.lock().expect("library mutex poisoned");
    let summary = localfiles::import_transaction(&state.store, &mut library, paths)?;
    drop(library);
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
    #[cfg(debug_assertions)]
    let menu = menu.item(&local_spike::menu(app)?);
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
        "export_library" => export_library(app, false),
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
                if let Err(error) = connect_spotify(handle.clone()).await {
                    notify_error(&handle, error);
                }
            });
        }
        "disconnect_spotify" => {
            let handle = app.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(error) = disconnect_spotify(handle.clone()).await {
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
        #[cfg(debug_assertions)]
        id if local_spike::handles(id) => local_spike::start(app),
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
                let settings = state.settings.lock().expect("settings mutex poisoned");
                let playlists = state.playlists.lock().expect("playlist mutex poisoned");
                let bytes = export_with_settings(&library, &settings, &playlists, compressed)?;
                fs::write(path, bytes).map_err(|error| error.to_string())
            })();
            if let Err(error) = result {
                notify_error(&handle, error);
            }
        });
}

fn export_with_settings(
    library: &Library,
    settings: &Settings,
    playlists: &playlists::PlaylistCache,
    compressed: bool,
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
    let json = serde_json::to_vec(&envelope).map_err(|error| error.to_string())?;
    if !compressed {
        return Ok(json);
    }
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(&json)
        .map_err(|error| error.to_string())?;
    encoder.finish().map_err(|error| error.to_string())
}

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
    let settings = envelope
        .as_object_mut()
        .and_then(|object| object.remove("settings"))
        .filter(|_| restore)
        .map(|mut value| {
            if value.get("browserVisible").is_none() {
                let visible = value
                    .get("browserPanes")
                    .and_then(serde_json::Value::as_object)
                    .is_none_or(|panes| {
                        ["cat", "art", "alb"].into_iter().any(|pane| {
                            panes.get(pane).and_then(serde_json::Value::as_bool) != Some(false)
                        })
                    });
                value["browserVisible"] = visible.into();
                if !visible {
                    value["browserPanes"] =
                        serde_json::json!({"cat": true, "art": true, "alb": true});
                }
            }
            serde_json::from_value(value)
        })
        .transpose()
        .map_err(|error| error.to_string())?;
    let playlists = envelope
        .as_object_mut()
        .and_then(|object| object.remove("playlists"))
        .filter(|_| restore)
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| error.to_string())?;
    let library = import(&serde_json::to_vec(&envelope).map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())?;
    Ok((library, settings, playlists))
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
                import_with_settings(&bytes, replace)
            })();
            match result {
                Ok((library, settings, playlists)) if replace => {
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
                                apply_import(&confirmed_handle, library, settings, playlists, true);
                            }
                        });
                }
                Ok((library, _, _)) => apply_import(&handle, library, None, None, false),
                Err(error) => notify_error(&handle, error),
            }
        });
}

fn apply_import(
    app: &tauri::AppHandle,
    imported: Library,
    export_settings: Option<ExportSettings>,
    imported_playlists: Option<playlists::PlaylistCache>,
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
                if let Err(error) = state.playlist_store.save(&playlists) {
                    notify_error(app, error.to_string());
                    return;
                }
                *state.playlists.lock().expect("playlist mutex poisoned") = playlists;
                let _ = app.emit("playlists-changed", ());
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
    export_settings.apply_to(&mut settings);
    state
        .settings_store
        .save(&settings)
        .map_err(|error| error.to_string())?;
    state
        .menu_checks
        .sync(&settings)
        .map_err(|error| error.to_string())?;
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
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            browse,
            metadata_values,
            click_track_star,
            set_album_rating,
            get_track,
            edit_track,
            set_track_infos,
            import_local,
            startup_notice,
            get_settings,
            set_settings,
            connection_state,
            connect_spotify,
            disconnect_spotify,
            sync_from_spotify,
            spotify_search,
            spotify_album_page,
            spotify_artist_page,
            spotify_follow_artist,
            spotify_artist_albums,
            add_spotify_album,
            remove_spotify_album,
            playlists_list,
            open_spotify_playlist,
            resolve_spotify_track_destination,
            reorder_playlists,
            playlist_unfollow,
            playlist_create,
            playlist_tracks,
            playlist_add,
            playlist_add_album,
            playlist_reorder,
            playlist_remove,
            play_tracks,
            player_toggle,
            player_next,
            player_prev,
            player_seek,
            player_set_volume,
            set_repeat,
            set_shuffle,
            set_audio_settings,
            track_artwork
        ])
        .setup(|app| {
            app.handle().plugin(
                tauri_plugin_log::Builder::default()
                    .level(log::LevelFilter::Info)
                    .max_file_size(5_000_000)
                    .timezone_strategy(tauri_plugin_log::TimezoneStrategy::UseLocal)
                    .build(),
            )?;
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
            let playlist_store = FsPlaylistStore::new(&app_data_dir);
            let playlists = playlist_store.load()?;
            let settings = settings_store.load()?.unwrap_or_default();
            settings_store.save(&settings)?;
            let menu_checks = install_file_menu(app, &settings)?;
            // Dev builds keep tokens in a 0600 plaintext file. Release keeps
            // only the encryption key in Keychain and migrates legacy tokens.
            let backing: Box<dyn TokenStore> = if cfg!(debug_assertions) {
                Box::new(store::FsTokenStore::new(&app_data_dir))
            } else {
                let encrypted =
                    EncryptedFsTokenStore::new(&app_data_dir).map_err(std::io::Error::other)?;
                let legacy = KeychainTokenStore::new().map_err(std::io::Error::other)?;
                if let Err(error) = migrate_token_store(&legacy, &encrypted) {
                    log::warn!("Could not migrate legacy Spotify tokens: {error}");
                }
                Box::new(encrypted)
            };
            let token_store = Arc::new(CachedTokenStore::new(backing));
            // Keychain access can fail transiently; start disconnected rather
            // than aborting startup.
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
            let activate_local = connection.connected && settings.playback_backend == "local";
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
            let media_keys = media_keys::MediaKeys::spawn(app.handle().clone());
            app.manage(AppState {
                library: Mutex::new(library),
                store,
                settings: Mutex::new(settings),
                settings_store,
                sync_store,
                playlists: Mutex::new(playlists),
                playlist_store,
                menu_checks,
                recovery_notice: Mutex::new(recovery_notice),
                token_store,
                spotify: Mutex::new(spotify),
                artwork_cache: Mutex::default(),
                playback: Arc::clone(&playback),
                media_keys,
                sync_orchestrator: SyncOrchestrator::default(),
                playlist_reauth_notified: AtomicBool::new(false),
            });
            let completion_app = app.handle().clone();
            playback.listen(app.handle().clone(), move |uri| {
                let handle = completion_app.clone();
                drop(tauri::async_runtime::spawn_blocking(move || {
                    let state = handle.state::<AppState>();
                    match record_play(&state.store, &state.library, &uri, unix_now()) {
                        Ok(true) => {
                            if let Err(error) = handle.emit("library-changed", ()) {
                                notify_error(&handle, error.to_string());
                            }
                        }
                        Ok(false) => {}
                        Err(error) => notify_error(&handle, error),
                    }
                }));
            });
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
                        StartupAction::Connect => connect_spotify(handle.clone()).await,
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
        client::{FakeTransport, Image, Page, Response, SimplifiedArtist, SpotifyClient, Track},
        tokens::{InMemoryTokenStore, Tokens},
    };

    use super::*;

    fn shared_token_store(tokens: Option<Tokens>) -> SharedTokenStore {
        Arc::new(CachedTokenStore::new(
            Box::new(InMemoryTokenStore::new(tokens)) as Box<dyn TokenStore>,
        ))
    }

    fn metadata_track(uri: &str, cat: &str, art: &str, alb: &str) -> NewTrack {
        NewTrack {
            uri: uri.into(),
            source: SourceId::Music,
            cat: cat.into(),
            art: art.into(),
            alb: alb.into(),
            name: uri.into(),
            duration: Duration::from_secs(1),
            track_no: None,
            disc_no: None,
            added_at: None,
            kind: None,
            bitrate_kbps: None,
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

        assert!(record_play(&store, &library, "spotify:track:track", 123).unwrap());
        assert!(!record_play(&store, &library, "spotify:track:missing", 456).unwrap());

        let current = library.lock().unwrap();
        let track = current.get(id).unwrap();
        assert_eq!(track.play_count, 4);
        assert_eq!(track.last_played_at, Some(123));
        let saves = store.0.lock().unwrap();
        assert_eq!(saves.len(), 1);
        assert_eq!(saves[0], *current);
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
        let local = library.add(metadata_track(
            "file:///tmp/R%C3%A9tune%20song.mp3",
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
            Some("/tmp/Rétune song.mp3".into())
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
                non_library_tracks: vec![],
            }],
        }
    }

    fn playlist_client(
        responses: impl IntoIterator<Item = Response>,
    ) -> SpotifyClient<FakeTransport, InMemoryTokenStore> {
        SpotifyClient::new(
            "client",
            FakeTransport::new(responses),
            InMemoryTokenStore::new(Some(Tokens {
                access: "access".into(),
                refresh: "refresh".into(),
                expires_at: u64::MAX,
                scopes: auth::SCOPES.into(),
            })),
        )
    }

    #[tokio::test]
    async fn track_artwork_resolves_smallest_usable_image_and_caches() {
        let client = playlist_client([
            Response::json(
                200,
                serde_json::json!({
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
                }),
            ),
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
            resolve_track_artwork(Some(&client), &cache, "spotify:track:track")
                .await
                .as_deref(),
            Some("small")
        );
        assert_eq!(
            resolve_track_artwork(Some(&client), &cache, "spotify:track:track").await,
            Some("small".into())
        );
        assert_eq!(
            resolve_track_artwork(Some(&client), &cache, "spotify:album:album").await,
            None
        );
        assert_eq!(
            resolve_track_artwork(Some(&client), &cache, "spotify:track:missing").await,
            None
        );
        assert_eq!(
            resolve_track_artwork(Some(&client), &cache, "spotify:track:missing").await,
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
            &uri,
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
                &uri
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
                &localfiles::file_uri(&wav)
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
            spotify_item_url("playlist", "abc123").unwrap(),
            "https://open.spotify.com/playlist/abc123"
        );
        assert!(spotify_item_url("playlist", "").is_err());
        assert!(spotify_item_url("playlist", "../account").is_err());

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
                    "items": [
                        {"uri": "spotify:track:one", "name": "One", "artists": []},
                        {"uri": "spotify:track:two", "name": "Two", "artists": []}
                    ],
                    "next": null
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
        let tracks = MediaProvider::album_tracks(&client, "spotify:album:album")
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
                .non_library_tracks
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
        let id = library.add(metadata_track(
            "spotify:track:one",
            "Rock",
            "Artist",
            "Album",
        ));
        library.set_track_rating(id, Rating::new(4)).unwrap();
        library.set_album_rating(
            AlbumKey {
                source: SourceId::Music,
                art: "Artist".into(),
                alb: "Album".into(),
            },
            Rating::new(5),
        );

        let page = album_page_view(&library, spotify_album());

        assert_eq!(page.album_type, "Compilation");
        assert_eq!(page.year.as_deref(), Some("2024"));
        assert_eq!(page.total_duration_secs, 4);
        assert!(!page.in_library);
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
        let page = album_page_view(&library, spotify_album());
        assert!(page.in_library);
        assert_eq!(page.album_rating, Some(5));
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
            missing_scopes: vec![],
        };
        let disconnected = ConnectionState::from_tokens(None);
        let needs_reauth = ConnectionState {
            connected: true,
            needs_reauth: true,
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
        };
        let current = Tokens {
            scopes: auth::SCOPES.into(),
            ..legacy.clone()
        };

        assert_eq!(
            stored_connection_state(&shared_token_store(Some(legacy))).unwrap(),
            ConnectionState {
                connected: true,
                needs_reauth: true,
                missing_scopes: auth::REQUIRED_SCOPES
                    .into_iter()
                    .filter(|scope| *scope != "user-library-read")
                    .map(String::from)
                    .collect(),
            }
        );
        assert_eq!(
            stored_connection_state(&shared_token_store(Some(current))).unwrap(),
            ConnectionState {
                connected: true,
                needs_reauth: false,
                missing_scopes: vec![],
            }
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
                    non_library_tracks: vec![],
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
                "genre",
                "time",
                "kind",
                "bitrate",
                "lastPlayed",
                "added",
            ]
            .map(String::from)
            .to_vec(),
            column_widths: BTreeMap::from([("name".into(), 260), ("artist".into(), 140)]),
            hidden_columns: vec![
                "genre".into(),
                "kind".into(),
                "bitrate".into(),
                "added".into(),
            ],
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
        };
        let bytes = export_with_settings(&library, &exported, &playlists, true).unwrap();
        let (restored_library, visual, restored_playlists) =
            import_with_settings(&bytes, true).unwrap();
        let mut restored = Settings {
            spotify_client_id: "local-machine".into(),
            auto_add_spotify_library: true,
            auto_connect: true,
            spotify_sync_completed: false,
            ..Settings::default()
        };
        visual.unwrap().apply_to(&mut restored);

        assert_eq!(restored_library, library);
        assert_eq!(restored_playlists, Some(playlists));
        assert_eq!(restored.theme, Theme::Dark);
        assert!(restored.pl_collapsed);
        assert!(!restored.browser_visible);
        assert_eq!(restored.browser_panes, exported.browser_panes);
        assert_eq!(restored.column_order, exported.column_order);
        assert_eq!(restored.column_widths, exported.column_widths);
        assert_eq!(restored.hidden_columns, exported.hidden_columns);
        assert_eq!(restored.sort_column.as_deref(), Some("plays"));
        assert!(restored.sort_desc);
        assert!(restored.shuffle);
        assert_eq!(restored.spotify_client_id, "local-machine");
        assert!(restored.auto_add_spotify_library);
        assert!(restored.auto_connect);
        assert!(!restored.spotify_sync_completed);
    }

    #[test]
    fn legacy_visual_settings_default_all_browser_panes_visible() {
        let mut json =
            serde_json::to_value(VisualSettings::from_settings(&Settings::default())).unwrap();
        json.as_object_mut().unwrap().remove("browserPanes");

        let visual: VisualSettings = serde_json::from_value(json).unwrap();

        assert_eq!(visual.browser_panes, BrowserPanes::default());
    }

    #[test]
    fn legacy_visual_settings_default_playlists_expanded() {
        let mut json =
            serde_json::to_value(VisualSettings::from_settings(&Settings::default())).unwrap();
        json.as_object_mut().unwrap().remove("plCollapsed");

        let visual: VisualSettings = serde_json::from_value(json).unwrap();

        assert!(!visual.pl_collapsed);
    }

    #[test]
    fn legacy_export_defaults_shuffle_off_and_visual_settings_exclude_it() {
        let visual =
            serde_json::to_value(VisualSettings::from_settings(&Settings::default())).unwrap();
        assert!(visual.get("shuffle").is_none());

        let exported: ExportSettings = serde_json::from_value(visual).unwrap();

        assert!(!exported.behavioral.shuffle);
    }

    #[test]
    fn legacy_visual_settings_default_column_widths_to_empty() {
        let mut json =
            serde_json::to_value(VisualSettings::from_settings(&Settings::default())).unwrap();
        json.as_object_mut().unwrap().remove("columnWidths");

        let visual: VisualSettings = serde_json::from_value(json).unwrap();

        assert!(visual.column_widths.is_empty());
    }

    #[test]
    fn visual_settings_apply_restored_browser_visibility() {
        let mut settings = Settings::default();
        let mut visual = VisualSettings::from_settings(&settings);
        visual.browser_panes = BrowserPanes {
            cat: true,
            art: false,
            alb: true,
        };

        visual.apply_to(&mut settings);

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
    fn visual_settings_apply_normalizes_legacy_columns() {
        let mut json =
            serde_json::to_value(VisualSettings::from_settings(&Settings::default())).unwrap();
        let object = json.as_object_mut().unwrap();
        object.insert(
            "columnOrder".into(),
            serde_json::json!(["track", "name", "time", "artist", "album", "genre", "rating"]),
        );
        object.insert("hiddenColumns".into(), serde_json::json!(["name", "genre"]));
        object.remove("sortColumn");
        object.remove("sortDesc");
        let visual: VisualSettings = serde_json::from_value(json).unwrap();
        let mut settings = Settings::default();

        visual.apply_to(&mut settings);

        assert_eq!(settings.column_order, Settings::default().column_order);
        assert_eq!(
            settings.hidden_columns,
            ["genre", "kind", "bitrate", "lastPlayed", "added"]
        );
        assert_eq!(settings.sort_column, None);
        assert!(!settings.sort_desc);
    }

    #[test]
    fn merge_ignores_exported_visual_settings() {
        let library = fixture::library();
        let bytes = export_with_settings(
            &library,
            &Settings::default(),
            &playlists::PlaylistCache::default(),
            false,
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

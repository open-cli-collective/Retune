mod fixture;
#[cfg(debug_assertions)]
mod local_spike;
mod playback;
mod provider;
mod store;
mod sync;
mod sync_orchestrator;

use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use flate2::{read::GzDecoder, write::GzEncoder, Compression};

use playback::{Playback, PlayerStateEvent, SnapshotTrack};
use provider::{MediaProvider, SearchAlbum, SearchResults, SpotifySyncProvider};
use retune_core::{
    browse::{self, Selection},
    io::{export_json, import},
    model::{
        AlbumKey, EffectiveRating, Library, Rating, SourceId, TrackEdit, TrackId, TrackRecord,
    },
};
use retune_spotify::{
    auth::{self, LoopbackListener, Pkce},
    client::{HttpTransport, SpotifyClient},
    tokens::{CachedTokenStore, KeychainTokenStore, TokenStore, Tokens},
};
use serde::{Deserialize, Serialize};
use store::{
    FsOverlayStore, FsSettingsStore, FsSyncStore, OverlayStore, Settings, StoreError, Theme,
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
    menu_checks: MenuChecks,
    recovery_notice: Mutex<Option<String>>,
    token_store: SharedTokenStore,
    spotify: Mutex<Option<Arc<SpotifyProvider>>>,
    playback: Arc<Playback>,
    sync_orchestrator: SyncOrchestrator,
}

struct MenuChecks {
    zebra: CheckMenuItem<tauri::Wry>,
    account_status: tauri::menu::MenuItem<tauri::Wry>,
    connect: tauri::menu::MenuItem<tauri::Wry>,
    disconnect: tauri::menu::MenuItem<tauri::Wry>,
}

impl MenuChecks {
    fn sync(&self, settings: &Settings) -> tauri::Result<()> {
        self.zebra.set_checked(settings.zebra)
    }

    fn sync_connection(&self, connected: bool) -> tauri::Result<()> {
        self.account_status.set_text(if connected {
            "Connected"
        } else {
            "Not connected"
        })?;
        self.connect.set_enabled(!connected)?;
        self.disconnect.set_enabled(connected)
    }
}

#[derive(Clone, Copy, Serialize)]
struct ConnectionState {
    connected: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct VisualSettings {
    theme: Theme,
    zoom: f64,
    zebra: bool,
    column_order: Vec<String>,
    hidden_columns: Vec<String>,
}

impl VisualSettings {
    fn from_settings(settings: &Settings) -> Self {
        Self {
            theme: settings.theme,
            zoom: settings.zoom,
            zebra: settings.zebra,
            column_order: settings.column_order.clone(),
            hidden_columns: settings.hidden_columns.clone(),
        }
    }

    fn apply_to(self, settings: &mut Settings) {
        settings.theme = self.theme;
        settings.zoom = self.zoom;
        settings.zebra = self.zebra;
        settings.column_order = self.column_order;
        settings.hidden_columns = self.hidden_columns;
    }
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
struct TrackView {
    id: u64,
    uri: String,
    name: String,
    art: String,
    alb: String,
    cat: String,
    track_no: Option<u32>,
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
        .map(|track| TrackView {
            id: track.id.0,
            uri: track.uri.clone(),
            name: track.name.clone(),
            art: track.art.clone(),
            alb: track.alb.clone(),
            cat: track.cat.clone(),
            track_no: track.track_no,
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
        album_rating,
        album_rating_artist,
        album_rating_ambiguous,
        counts: counts(&library, source, &selection, &query),
    }
}

fn album_rating_view(
    library: &Library,
    selection: &Selection,
    tracks: &[&TrackRecord],
) -> (Option<u8>, Option<String>, bool) {
    if selection.alb().is_none() {
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
            .map_err(|error| error.to_string())?;
        if let Some(change) = edit.rating_change {
            let rating = change
                .stars
                .map(|stars| {
                    Rating::new(stars).ok_or_else(|| format!("invalid star rating {stars}"))
                })
                .transpose()?;
            library
                .set_track_rating(TrackId(id), rating)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
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
    let current = state
        .settings
        .lock()
        .expect("settings mutex poisoned")
        .clone();
    let client_id_changed = current.spotify_client_id != settings.spotify_client_id;
    settings.spotify_sync_completed = current.spotify_sync_completed;
    settings.last_full_sync = current.last_full_sync;
    settings.validate().map_err(|error| error.to_string())?;
    if current.playback_backend != settings.playback_backend {
        let switch = if settings.playback_backend == "local" {
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
    Ok(ConnectionState {
        connected: token_store
            .load()
            .map_err(|error| error.to_string())?
            .is_some(),
    })
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
        .sync_connection(connection.connected)
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
    state
        .token_store
        .save(&Tokens {
            access: token.access_token,
            refresh,
            expires_at: now.saturating_add(token.expires_in),
            scopes: auth::SCOPES.into(),
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
    app.emit("player-state", empty_player_state())
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

fn empty_player_state() -> PlayerStateEvent {
    PlayerStateEvent {
        track_id: None,
        elapsed: 0,
        is_playing: false,
        external: false,
        name: None,
        art: None,
        alb: None,
        duration_secs: None,
        volume_supported: false,
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
    let outcome = sync::snapshot(&sync_provider, |phase| {
        log::info!("{phase}");
        let _ = app.emit("sync-progress", phase);
    })
    .await?;
    let sync::SnapshotOutcome {
        tracks,
        genres_degraded,
        partial,
        earliest_cooldown,
        request_counts,
    } = outcome;
    app.emit("sync-progress", "Saving library…")
        .map_err(|error| error.to_string())?;
    let first_sync = !state
        .settings
        .lock()
        .expect("settings mutex poisoned")
        .spotify_sync_completed;
    {
        let mut library = state.library.lock().expect("library mutex poisoned");
        sync::apply(&mut library, &state.store, first_sync, tracks)?;
        log::info!(
            "Spotify sync applied; {} library tracks",
            library.tracks().len()
        );
    }
    if partial {
        let message = earliest_cooldown.map_or_else(
            || "Partial import (Spotify rate limit) — run File → Sync later to finish.".into(),
            |deadline| {
                format!(
                    "Partial import — will finish automatically after {}.",
                    provider::format_resume_time(deadline)
                )
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
    let mut settings = state.settings.lock().expect("settings mutex poisoned");
    if record_full_sync(&mut settings, partial, unix_now()) {
        state
            .settings_store
            .save(&settings)
            .map_err(|error| error.to_string())?;
    }
    drop(settings);
    log::info!(
        "sync requests:{}",
        request_counts
            .iter()
            .map(|(family, count)| format!(" {family}={count}"))
            .collect::<String>()
    );
    app.emit("library-changed", ())
        .map_err(|error| error.to_string())?;
    Ok(SyncCompletion {
        partial,
        auto_resume: partial.then_some(earliest_cooldown).flatten(),
    })
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
        });
    }
    if !stored_connection_state(&state.token_store)?.connected {
        return Err("Connect to Spotify to search.".into());
    }
    let provider = provider_from(&state)?;
    MediaProvider::search(provider.as_ref(), query.trim()).await
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

#[tauri::command(rename_all = "camelCase")]
async fn play_tracks(
    app: tauri::AppHandle,
    snapshot: Vec<SnapshotTrack>,
    start_index: usize,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let client = provider_from(&state)?;
    state.playback.play(client, snapshot, start_index).await
}

#[tauri::command]
async fn player_toggle(app: tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let client = provider_from(&state)?;
    state.playback.toggle(client.as_ref()).await
}

#[tauri::command]
async fn player_next(app: tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let client = provider_from(&state)?;
    state.playback.next(client.as_ref()).await
}

#[tauri::command]
async fn player_prev(app: tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let client = provider_from(&state)?;
    state.playback.prev(client.as_ref()).await
}

#[tauri::command]
async fn player_seek(app: tauri::AppHandle, seconds: u64) -> Result<(), String> {
    let state = app.state::<AppState>();
    let client = provider_from(&state)?;
    state.playback.seek(client.as_ref(), seconds).await
}

#[tauri::command]
async fn player_set_volume(app: tauri::AppHandle, volume: u8) -> Result<(), String> {
    let state = app.state::<AppState>();
    let client = provider_from(&state)?;
    let playback = Arc::clone(&state.playback);
    tauri::async_runtime::spawn_blocking(move || {
        tauri::async_runtime::block_on(playback.set_volume(client.as_ref(), volume))
    })
    .await
    .map_err(|error| error.to_string())??;
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
        .map_err(|error| error.to_string())
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

fn install_file_menu(app: &tauri::App, settings: &Settings) -> tauri::Result<MenuChecks> {
    let preferences = MenuItemBuilder::with_id("preferences", "Preferences…")
        .accelerator("CmdOrCtrl+,")
        .build(app)?;
    let app_menu = SubmenuBuilder::new(app, "Retune")
        .item(&preferences)
        .separator()
        .quit()
        .build()?;
    let get_info = MenuItemBuilder::with_id("get_info", "Get Info")
        .accelerator("CmdOrCtrl+I")
        .build(app)?;
    let file = SubmenuBuilder::new(app, "File")
        .item(&get_info)
        .separator()
        .text("sync_spotify", "Sync from Spotify")
        .separator()
        .text("backup_library", "Back Up Library…")
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
    let view = SubmenuBuilder::new(app, "View")
        .items(&[&zoom_in, &zoom_out, &actual_size])
        .separator()
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
    let menu = MenuBuilder::new(app).items(&[&app_menu, &file, &edit, &view, &controls, &account]);
    #[cfg(debug_assertions)]
    let menu = menu.item(&local_spike::menu(app)?);
    let menu = menu.item(&help).build()?;
    app.set_menu(menu)?;
    app.on_menu_event(|app, event| match event.id().as_ref() {
        "get_info" => {
            let _ = app.emit("get-info", ());
        }
        "backup_library" => export_library(app, false),
        "export_library" => export_library(app, true),
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
        "zoom_in" | "zoom_out" | "actual_size" | "toggle_zebra" => {
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
                let bytes = export_with_settings(&library, &settings, compressed)?;
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
    compressed: bool,
) -> Result<Vec<u8>, String> {
    let mut envelope: serde_json::Value =
        serde_json::from_slice(&export_json(library)).map_err(|error| error.to_string())?;
    envelope
        .as_object_mut()
        .expect("core export is an object")
        .insert(
            "settings".into(),
            serde_json::to_value(VisualSettings::from_settings(settings))
                .map_err(|error| error.to_string())?,
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
) -> Result<(Library, Option<VisualSettings>), String> {
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
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| error.to_string())?;
    let library = import(&serde_json::to_vec(&envelope).map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())?;
    Ok((library, settings))
}

fn import_library(app: &tauri::AppHandle, replace: bool) {
    let handle = app.clone();
    app.dialog()
        .file()
        .add_filter("Retune Library", &["json", "json.gz", "gz"])
        .pick_file(move |path| {
            let Some(path) = path else { return };
            let result = (|| -> Result<(Library, Option<VisualSettings>), String> {
                let path = path.into_path().map_err(|error| error.to_string())?;
                let bytes = fs::read(path).map_err(|error| error.to_string())?;
                import_with_settings(&bytes, replace)
            })();
            match result {
                Ok((library, settings)) if replace => {
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
                                apply_import(&confirmed_handle, library, settings, true);
                            }
                        });
                }
                Ok((library, _)) => apply_import(&handle, library, None, false),
                Err(error) => notify_error(&handle, error),
            }
        });
}

fn apply_import(
    app: &tauri::AppHandle,
    imported: Library,
    visual_settings: Option<VisualSettings>,
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
            if let Some(visual_settings) = visual_settings {
                if let Err(error) = apply_visual_settings(app, visual_settings) {
                    notify_error(app, error);
                    return;
                }
            }
            let _ = app.emit("library-changed", ());
        }
        Err(error) => notify_error(app, error),
    }
}

fn apply_visual_settings(
    app: &tauri::AppHandle,
    visual_settings: VisualSettings,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut settings = state
        .settings
        .lock()
        .expect("settings mutex poisoned")
        .clone();
    visual_settings.apply_to(&mut settings);
    state
        .settings_store
        .save(&settings)
        .map_err(|error| error.to_string())?;
    state
        .menu_checks
        .sync(&settings)
        .map_err(|error| error.to_string())?;
    *state.settings.lock().expect("settings mutex poisoned") = settings.clone();
    app.emit("settings-changed", settings)
        .map_err(|error| error.to_string())
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
            click_track_star,
            set_album_rating,
            get_track,
            edit_track,
            startup_notice,
            get_settings,
            set_settings,
            connection_state,
            connect_spotify,
            disconnect_spotify,
            sync_from_spotify,
            spotify_search,
            spotify_artist_albums,
            add_spotify_album,
            play_tracks,
            player_toggle,
            player_next,
            player_prev,
            player_seek,
            player_set_volume
        ])
        .setup(|app| {
            app.handle().plugin(
                tauri_plugin_log::Builder::default()
                    .level(log::LevelFilter::Info)
                    .max_file_size(5_000_000)
                    .build(),
            )?;
            let app_data_dir = app.path().app_data_dir()?;
            let store = FsOverlayStore::new(&app_data_dir);
            let (library, recovery_notice) = match store.load() {
                Ok(Some(library)) => (library, None),
                Ok(None) => {
                    let library = initial_library(cfg!(debug_assertions));
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
            let settings_store = FsSettingsStore::new(&app_data_dir);
            let sync_store = FsSyncStore::new(&app_data_dir);
            let settings = settings_store.load()?.unwrap_or_default();
            settings_store.save(&settings)?;
            let menu_checks = install_file_menu(app, &settings)?;
            // Dev builds keep tokens in a 0600 file: Keychain ACL grants are
            // keyed to the binary signature, which changes every rebuild, so
            // dev iteration would re-prompt constantly. Release uses Keychain.
            let backing: Box<dyn TokenStore> = if cfg!(debug_assertions) {
                Box::new(store::FsTokenStore::new(&app_data_dir))
            } else {
                Box::new(KeychainTokenStore::new().map_err(std::io::Error::other)?)
            };
            let token_store = Arc::new(CachedTokenStore::new(backing));
            // Keychain access can fail transiently (e.g. "In dark wake, no UI
            // possible" while the display sleeps); start disconnected rather
            // than abort — the cache retries the Keychain on the next access.
            let connected = match stored_connection_state(&token_store) {
                Ok(state) => state.connected,
                Err(error) => {
                    log::warn!("Token store unavailable at startup: {error}");
                    false
                }
            };
            menu_checks.sync_connection(connected)?;
            let spotify = spotify_provider(&settings.spotify_client_id, Arc::clone(&token_store))
                .map_err(std::io::Error::other)?;
            let startup_action = startup_action(
                connected,
                &settings.spotify_client_id,
                settings.auto_connect,
                settings.last_full_sync,
                unix_now(),
            );
            if connected && startup_action == StartupAction::Nothing {
                log::info!("startup sync skipped; library fresh");
            }
            let activate_local = connected && settings.playback_backend == "local";
            let initial_volume = settings.volume;
            let playback = Arc::new(Playback::default());
            app.manage(AppState {
                library: Mutex::new(library),
                store,
                settings: Mutex::new(settings),
                settings_store,
                sync_store,
                menu_checks,
                recovery_notice: Mutex::new(recovery_notice),
                token_store,
                spotify: Mutex::new(spotify),
                playback: Arc::clone(&playback),
                sync_orchestrator: SyncOrchestrator::default(),
            });
            playback.listen(app.handle().clone());
            if activate_local || startup_action != StartupAction::Nothing {
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
                        StartupAction::Nothing => Ok(()),
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
    connected: bool,
    client_id: &str,
    auto_connect: bool,
    last_full_sync: Option<u64>,
    now: u64,
) -> StartupAction {
    if connected {
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
    use super::*;

    #[test]
    fn release_first_run_starts_empty() {
        assert!(initial_library(false).tracks().is_empty());
        assert!(!initial_library(true).tracks().is_empty());
    }

    #[test]
    fn startup_action_syncs_connects_or_does_nothing() {
        assert_eq!(
            startup_action(true, "", false, None, 1_000),
            StartupAction::Sync
        );
        assert_eq!(
            startup_action(true, "", false, Some(999), 1_000),
            StartupAction::Nothing
        );
        assert_eq!(
            startup_action(true, "", false, Some(99), 1_000),
            StartupAction::Sync
        );
        assert_eq!(
            startup_action(false, "client-id", true, None, 1_000),
            StartupAction::Connect
        );
        assert_eq!(
            startup_action(false, "", true, None, 1_000),
            StartupAction::Nothing
        );
        assert_eq!(
            startup_action(false, "client-id", false, None, 1_000),
            StartupAction::Nothing
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
    fn export_restore_round_trips_visual_settings_only() {
        let library = fixture::library();
        let exported = Settings {
            theme: Theme::Dark,
            zoom: 1.4,
            zebra: false,
            column_order: [
                "name", "track", "rating", "artist", "album", "genre", "time",
            ]
            .map(String::from)
            .to_vec(),
            hidden_columns: vec!["genre".into()],
            auto_add_spotify_library: false,
            auto_connect: false,
            spotify_client_id: "exported-machine".into(),
            spotify_sync_completed: true,
            last_full_sync: Some(42),
            playback_backend: "local".into(),
            volume: 40,
        };
        let bytes = export_with_settings(&library, &exported, true).unwrap();
        let (restored_library, visual) = import_with_settings(&bytes, true).unwrap();
        let mut restored = Settings {
            spotify_client_id: "local-machine".into(),
            auto_add_spotify_library: true,
            auto_connect: true,
            spotify_sync_completed: false,
            ..Settings::default()
        };
        visual.unwrap().apply_to(&mut restored);

        assert_eq!(restored_library, library);
        assert_eq!(restored.theme, Theme::Dark);
        assert_eq!(restored.hidden_columns, ["genre"]);
        assert_eq!(restored.spotify_client_id, "local-machine");
        assert!(restored.auto_add_spotify_library);
        assert!(restored.auto_connect);
        assert!(!restored.spotify_sync_completed);
    }

    #[test]
    fn merge_ignores_exported_visual_settings() {
        let library = fixture::library();
        let bytes = export_with_settings(&library, &Settings::default(), false).unwrap();
        let (merged, visual) = import_with_settings(&bytes, false).unwrap();
        assert_eq!(merged, library);
        assert_eq!(visual, None);
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
        selection.select_alb(Some("Hotel California".into()));
        let tracks = browse::tracks(&library, SourceId::Music, &selection);

        assert_eq!(
            album_rating_view(&library, &selection, &tracks),
            (None, None, true)
        );
    }
}

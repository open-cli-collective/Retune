mod fixture;
mod provider;
mod store;
mod sync;

use std::{
    collections::BTreeSet,
    fs,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use provider::{MediaProvider, SearchResults};
use retune_core::{
    browse::{self, Selection},
    io::{export_json, export_json_gz, import},
    model::{
        AlbumKey, EffectiveRating, Library, Rating, SourceId, TrackEdit, TrackId, TrackRecord,
    },
};
use retune_spotify::{
    auth::{self, LoopbackListener, Pkce},
    client::{HttpTransport, SpotifyClient},
    tokens::{KeychainTokenStore, TokenStore, Tokens},
};
use serde::{Deserialize, Serialize};
use store::{FsOverlayStore, FsSettingsStore, OverlayStore, Settings, StoreError, Theme};
use tauri::{
    menu::{CheckMenuItem, CheckMenuItemBuilder, MenuBuilder, MenuItemBuilder, SubmenuBuilder},
    Emitter, Manager,
};
use tauri_plugin_opener::OpenerExt;

type SpotifyProvider = SpotifyClient<HttpTransport, KeychainTokenStore>;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};

struct AppState {
    library: Mutex<Library>,
    store: FsOverlayStore,
    settings: Mutex<Settings>,
    settings_store: FsSettingsStore,
    menu_checks: MenuChecks,
    recovery_notice: Mutex<Option<String>>,
    spotify: Mutex<Option<Arc<SpotifyProvider>>>,
    syncing: tokio::sync::Mutex<()>,
}

struct MenuChecks {
    zebra: CheckMenuItem<tauri::Wry>,
    light: CheckMenuItem<tauri::Wry>,
    dark: CheckMenuItem<tauri::Wry>,
    system: CheckMenuItem<tauri::Wry>,
    account_status: tauri::menu::MenuItem<tauri::Wry>,
    connect: tauri::menu::MenuItem<tauri::Wry>,
    disconnect: tauri::menu::MenuItem<tauri::Wry>,
}

impl MenuChecks {
    fn sync(&self, settings: &Settings) -> tauri::Result<()> {
        self.zebra.set_checked(settings.zebra)?;
        self.light.set_checked(settings.theme == Theme::Light)?;
        self.dark.set_checked(settings.theme == Theme::Dark)?;
        self.system.set_checked(settings.theme == Theme::System)
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
fn set_settings(state: tauri::State<'_, AppState>, mut settings: Settings) -> Result<(), String> {
    let current = state.settings.lock().expect("settings mutex poisoned");
    let client_id_changed = current.spotify_client_id != settings.spotify_client_id;
    settings.spotify_sync_completed = current.spotify_sync_completed;
    drop(current);
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
            spotify_provider(&settings.spotify_client_id)?;
    }
    Ok(())
}

fn spotify_provider(client_id: &str) -> Result<Option<Arc<SpotifyProvider>>, String> {
    if client_id.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(Arc::new(SpotifyClient::new(
        client_id.trim(),
        HttpTransport::new(),
        KeychainTokenStore::new().map_err(|error| error.to_string())?,
    ))))
}

fn stored_connection_state() -> Result<ConnectionState, String> {
    Ok(ConnectionState {
        connected: KeychainTokenStore::new()
            .and_then(|store| store.load())
            .map_err(|error| error.to_string())?
            .is_some(),
    })
}

#[tauri::command]
fn connection_state() -> Result<ConnectionState, String> {
    stored_connection_state()
}

fn emit_connection_state(app: &tauri::AppHandle) -> Result<(), String> {
    let connection = stored_connection_state()?;
    app.state::<AppState>()
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

    let listener = LoopbackListener::bind().map_err(|error| error.to_string())?;
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
    KeychainTokenStore::new()
        .and_then(|store| {
            store.save(&Tokens {
                access: token.access_token,
                refresh,
                expires_at: now.saturating_add(token.expires_in),
            })
        })
        .map_err(|error| error.to_string())?;
    *app.state::<AppState>()
        .spotify
        .lock()
        .expect("spotify mutex poisoned") = spotify_provider(&client_id)?;
    emit_connection_state(&app)?;
    sync_spotify(&app).await
}

#[tauri::command]
fn disconnect_spotify(app: tauri::AppHandle) -> Result<(), String> {
    KeychainTokenStore::new()
        .and_then(|store| store.clear())
        .map_err(|error| error.to_string())?;
    emit_connection_state(&app)
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
    let result = sync_spotify_inner(app).await;
    let _ = app.emit("sync-progress", "");
    result
}

async fn sync_spotify_inner(app: &tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let _sync = state.syncing.lock().await;
    if !stored_connection_state()?.connected {
        return Err("Connect to Spotify before syncing.".into());
    }
    let client_id = state
        .settings
        .lock()
        .expect("settings mutex poisoned")
        .spotify_client_id
        .clone();
    let provider = spotify_provider(&client_id)?.ok_or_else(|| {
        "Spotify Client ID is missing. Add it in Preferences, then try again.".to_string()
    })?;
    *state.spotify.lock().expect("spotify mutex poisoned") = Some(provider.clone());
    let incoming = sync::snapshot(provider.as_ref(), |phase| {
        let _ = app.emit("sync-progress", phase);
    })
    .await?;
    app.emit("sync-progress", "Saving library…")
        .map_err(|error| error.to_string())?;
    let first_sync = !state
        .settings
        .lock()
        .expect("settings mutex poisoned")
        .spotify_sync_completed;
    {
        let mut library = state.library.lock().expect("library mutex poisoned");
        sync::apply(&mut library, &state.store, first_sync, incoming)?;
    }
    if first_sync {
        let mut settings = state.settings.lock().expect("settings mutex poisoned");
        settings.spotify_sync_completed = true;
        state
            .settings_store
            .save(&settings)
            .map_err(|error| error.to_string())?;
    }
    app.emit("library-changed", ())
        .map_err(|error| error.to_string())
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
    if !stored_connection_state()?.connected {
        return Err("Connect to Spotify to search.".into());
    }
    let provider = provider_from(&state)?;
    MediaProvider::search(provider.as_ref(), query.trim()).await
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
    let light = CheckMenuItemBuilder::with_id("theme_light", "Light")
        .checked(settings.theme == Theme::Light)
        .build(app)?;
    let dark = CheckMenuItemBuilder::with_id("theme_dark", "Dark")
        .checked(settings.theme == Theme::Dark)
        .build(app)?;
    let system = CheckMenuItemBuilder::with_id("theme_system", "System")
        .checked(settings.theme == Theme::System)
        .build(app)?;
    let theme = SubmenuBuilder::new(app, "Theme")
        .items(&[&light, &dark, &system])
        .build()?;
    let view = SubmenuBuilder::new(app, "View")
        .items(&[&zoom_in, &zoom_out, &actual_size])
        .separator()
        .item(&zebra)
        .separator()
        .item(&theme)
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
    let menu = MenuBuilder::new(app)
        .items(&[&app_menu, &file, &edit, &view, &controls, &account, &help])
        .build()?;
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
            if let Err(error) = disconnect_spotify(app.clone()) {
                notify_error(app, error);
            }
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
        "zoom_in" | "zoom_out" | "actual_size" | "toggle_zebra" | "theme_light" | "theme_dark"
        | "theme_system" => {
            let _ = app.emit("view-action", event.id().as_ref());
        }
        "play_pause" | "previous" | "next" => {
            let _ = app.emit("player-action", event.id().as_ref());
        }
        _ => {}
    });
    Ok(MenuChecks {
        zebra,
        light,
        dark,
        system,
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
            add_spotify_album
        ])
        .setup(|app| {
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
            let settings = settings_store.load()?.unwrap_or_default();
            settings_store.save(&settings)?;
            let menu_checks = install_file_menu(app, &settings)?;
            let connected = stored_connection_state().map_err(std::io::Error::other)?.connected;
            menu_checks.sync_connection(connected)?;
            let spotify = spotify_provider(&settings.spotify_client_id)
                .map_err(std::io::Error::other)?;
            let startup_sync = connected && settings.auto_add_spotify_library;
            app.manage(AppState {
                library: Mutex::new(library),
                store,
                settings: Mutex::new(settings),
                settings_store,
                menu_checks,
                recovery_notice: Mutex::new(recovery_notice),
                spotify: Mutex::new(spotify),
                syncing: tokio::sync::Mutex::new(()),
            });
            if startup_sync {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(error) = sync_spotify(&handle).await {
                        notify_error(&handle, error);
                    }
                });
            }
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
    fn release_first_run_starts_empty() {
        assert!(initial_library(false).tracks().is_empty());
        assert!(!initial_library(true).tracks().is_empty());
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

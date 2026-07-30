use super::*;

#[tauri::command(rename_all = "camelCase")]
pub(super) fn playlists_list(
    state: tauri::State<'_, AppState>,
    uris: Option<Vec<String>>,
) -> Vec<PlaylistListView> {
    let uris = uris.unwrap_or_default();
    playlist_list_views(
        &state.playlists.lock().expect("playlist mutex poisoned"),
        &uris,
    )
}

#[tauri::command(rename_all = "camelCase")]
pub(super) fn open_spotify_playlist(id: String, target: SpotifyOpenTarget) -> Result<(), String> {
    tauri_plugin_opener::open_url(spotify_item_link("playlist", &id, target)?, None::<&str>)
        .map_err(|error| match target {
            SpotifyOpenTarget::App => format!("Could not open the Spotify app: {error}"),
            SpotifyOpenTarget::Web => error.to_string(),
        })
}

#[tauri::command]
pub(super) async fn resolve_spotify_track_destination(
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
pub(super) fn reorder_playlists(app: tauri::AppHandle, ids: Vec<String>) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut cache = state
        .playlists
        .lock()
        .expect("playlist mutex poisoned")
        .clone();
    playlists::reorder_playlists(&mut cache, &ids)?;
    save_playlists(&app, cache)
}

#[tauri::command]
pub(super) async fn playlist_unfollow(app: tauri::AppHandle, id: String) -> Result<(), String> {
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
    save_playlists(&app, cache)
}

#[tauri::command]
pub(super) fn playlist_tracks(
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
                    enabled: track.enabled,
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
                    enabled: true,
                    rating: None,
                }
            }
        })
        .collect())
}

#[tauri::command]
pub(super) async fn playlist_create(
    app: tauri::AppHandle,
    name: String,
) -> Result<PlaylistListView, String> {
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
    save_playlists(&app, cache)?;
    Ok(created)
}

#[tauri::command]
pub(super) async fn playlist_add(
    app: tauri::AppHandle,
    id: String,
    uris: Vec<String>,
) -> Result<(), String> {
    playlist_add_inner(&app, id, uris).await
}

#[tauri::command(rename_all = "camelCase")]
pub(super) async fn playlist_add_album(
    app: tauri::AppHandle,
    id: String,
    album_uri: String,
    album_label: Option<String>,
) -> Result<(), String> {
    playlists::reject_local_uris(std::slice::from_ref(&album_uri), |_| album_label.clone())?;
    let provider = provider_from(&app.state::<AppState>())?;
    let tracks = provider::album_tracks(provider.as_ref(), &album_uri).await?;
    let uris = tracks.into_iter().map(|track| track.uri).collect();
    playlist_add_inner(&app, id, uris).await
}

#[tauri::command(rename_all = "camelCase")]
pub(super) async fn playlist_reorder(
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
    finish_playlist_mutation(&app, &state, cache, result)
}

fn finish_playlist_mutation(
    app: &tauri::AppHandle,
    state: &AppState,
    cache: playlists::PlaylistCache,
    result: Result<(), playlists::PlaylistMutationError>,
) -> Result<(), String> {
    match result {
        Ok(()) => save_playlists(app, cache),
        Err(playlists::PlaylistMutationError::Reloaded) => {
            save_playlists(app, cache)?;
            Err(playlists::STALE_PLAYLIST.into())
        }
        Err(playlists::PlaylistMutationError::Spotify(error)) => Err(playlist_error(state, error)),
        Err(playlists::PlaylistMutationError::Other(error)) => Err(error),
    }
}

#[tauri::command]
pub(super) async fn playlist_remove(
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
    finish_playlist_mutation(&app, &state, cache, result)
}

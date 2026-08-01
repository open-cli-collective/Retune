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
    let library = state.library.lock().expect("library mutex poisoned");
    playlist_track_views(&playlists, &library, &id)
}

fn playlist_track_views(
    playlists: &playlists::PlaylistCache,
    library: &Library,
    id: &str,
) -> Result<Vec<PlaylistTrackView>, String> {
    let playlist = playlists
        .playlists
        .iter()
        .find(|playlist| playlist.id == id)
        .ok_or_else(|| format!("Unknown playlist {id}"))?;
    Ok(playlist
        .tracks
        .iter()
        .map(|uri| {
            let cached = playlist
                .spotify_tracks
                .iter()
                .find(|track| &track.uri == uri);
            if let Some(track) = library.tracks().iter().find(|track| &track.uri == uri) {
                PlaylistTrackView {
                    id: Some(track.id.0),
                    uri: track.uri.clone(),
                    name: track.name.clone(),
                    art: track.art.clone(),
                    alb: track.alb.clone(),
                    cat: track.cat.clone(),
                    disc_no: track
                        .disc_no
                        .or_else(|| cached.and_then(|track| track.disc_no)),
                    track_no: track
                        .track_no
                        .or_else(|| cached.and_then(|track| track.track_no)),
                    duration_secs: track.duration.as_secs(),
                    enabled: track.enabled,
                    play_count: track.play_count,
                    last_played_at: track.last_played_at,
                    added_at: track.added_at,
                    release_date: track
                        .release_date
                        .clone()
                        .or_else(|| cached.and_then(|track| track.release_date.clone())),
                    kind: track.kind.clone(),
                    bitrate_kbps: track.bitrate_kbps,
                    overridden: track
                        .orig_cat
                        .as_ref()
                        .is_some_and(|original| original != &track.cat),
                    is_local: false,
                    rating: library.effective_rating(track.id).map(rating_view),
                }
            } else {
                PlaylistTrackView {
                    id: None,
                    uri: uri.clone(),
                    name: cached.map(|track| track.name.clone()).unwrap_or_default(),
                    art: cached.map(|track| track.art.clone()).unwrap_or_default(),
                    alb: cached.map(|track| track.alb.clone()).unwrap_or_default(),
                    cat: String::new(),
                    disc_no: cached.and_then(|track| track.disc_no),
                    track_no: cached.and_then(|track| track.track_no),
                    duration_secs: cached.map_or(0, |track| track.duration / 1000),
                    enabled: true,
                    play_count: 0,
                    last_played_at: None,
                    added_at: None,
                    release_date: cached.and_then(|track| track.release_date.clone()),
                    kind: None,
                    bitrate_kbps: None,
                    overridden: false,
                    is_local: false,
                    rating: None,
                }
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use retune_core::model::NewTrack;

    use super::*;

    #[test]
    fn playlist_view_fills_missing_library_fields_from_spotify() {
        let mut library = Library::new();
        let id = library.add(NewTrack {
            uri: "spotify:track:one".into(),
            name: "Song".into(),
            duration: Duration::from_secs(180),
            ..NewTrack::default()
        });
        let playlists = playlists::PlaylistCache {
            playlists: vec![playlists::CachedPlaylist {
                id: "playlist".into(),
                name: "Playlist".into(),
                snapshot_id: "snapshot".into(),
                owned: true,
                owner: None,
                track_count: 1,
                tracks: vec!["spotify:track:one".into()],
                track_metadata_version: playlists::TRACK_METADATA_VERSION,
                spotify_tracks: vec![playlists::CachedTrack {
                    uri: "spotify:track:one".into(),
                    name: "Song".into(),
                    art: "Artist".into(),
                    alb: "Album".into(),
                    duration: 180_000,
                    disc_no: Some(2),
                    track_no: Some(7),
                    release_date: Some("1999".into()),
                }],
            }],
        };

        let tracks = playlist_track_views(&playlists, &library, "playlist").unwrap();

        assert_eq!(tracks[0].id, Some(id.0));
        assert_eq!((tracks[0].disc_no, tracks[0].track_no), (Some(2), Some(7)));
        assert_eq!(tracks[0].release_date.as_deref(), Some("1999"));
    }
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
    let result = playlists::reorder(
        client.as_ref(),
        &mut cache,
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
    let result = playlists::remove(client.as_ref(), &mut cache, &id, &indices).await;
    finish_playlist_mutation(&app, &state, cache, result)
}

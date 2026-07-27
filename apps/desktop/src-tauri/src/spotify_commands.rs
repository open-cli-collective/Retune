use super::*;

#[tauri::command]
pub(super) fn connection_state(
    state: tauri::State<'_, AppState>,
) -> Result<ConnectionState, String> {
    stored_connection_state(&state.token_store)
}

#[tauri::command]
pub(super) async fn connect_spotify(app: tauri::AppHandle) -> Result<(), String> {
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
pub(super) async fn disconnect_spotify(app: tauri::AppHandle) -> Result<(), String> {
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

#[tauri::command]
pub(super) async fn track_artwork(
    state: tauri::State<'_, AppState>,
    uri: String,
) -> Result<Option<String>, String> {
    let provider = provider_from(&state).ok();
    Ok(resolve_track_artwork(provider.as_deref(), &state.artwork_cache, &uri).await)
}

#[tauri::command]
pub(super) async fn sync_from_spotify(app: tauri::AppHandle) -> Result<(), String> {
    sync_spotify(&app).await
}

#[tauri::command]
pub(super) async fn spotify_search(
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
    let mut results = MediaProvider::search(provider.as_ref(), query.trim()).await?;
    mark_album_membership(
        &state.library.lock().expect("library mutex poisoned"),
        &mut results.albums,
    );
    Ok(results)
}

#[tauri::command]
pub(super) async fn spotify_album_page(
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
pub(super) async fn spotify_artist_page(
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
    Ok(ArtistPageView {
        id: artist.id.clone(),
        name: artist.name.clone(),
        descriptor: artist_descriptor(&artist),
        image_url: image_url(&artist.images),
        following,
    })
}

#[tauri::command(rename_all = "camelCase")]
pub(super) async fn spotify_follow_artist(
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
pub(super) async fn spotify_artist_albums(
    state: tauri::State<'_, AppState>,
    artist_id: String,
    offset: u32,
) -> Result<ArtistAlbumsPage, String> {
    let provider = provider_from(&state)?;
    let mut page = artist_albums_outcome(
        provider.as_ref(),
        &state.sync_store,
        &artist_id,
        offset,
        unix_now(),
        chrono::Local::now(),
    )
    .await?;
    mark_album_membership(
        &state.library.lock().expect("library mutex poisoned"),
        &mut page.albums,
    );
    Ok(page)
}

#[tauri::command]
pub(super) async fn add_spotify_album(
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
pub(super) async fn remove_spotify_album(app: tauri::AppHandle, uri: String) -> Result<(), String> {
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

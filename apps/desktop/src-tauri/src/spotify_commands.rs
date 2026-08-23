use super::*;
use crate::provider::{saved_album_record, SearchGroup};
use librespot_core::{authentication::Credentials, config::SessionConfig, session::Session};
use retune_spotify::tokens::PlaybackCredentials;

fn album_library_uris(uri: &str) -> Vec<String> {
    vec![uri.to_owned()]
}

fn web_oauth_tokens(access: String, refresh: String, expires_at: u64, scopes: String) -> Tokens {
    Tokens {
        access,
        refresh,
        expires_at,
        scopes,
        playback_credentials: None,
    }
}

pub(super) fn replace_spotify_library_state(
    store: &FsSyncStore,
    current: &Mutex<SpotifyLibraryState>,
    next: SpotifyLibraryState,
) -> Result<(), String> {
    store
        .save_spotify_library(&next)
        .map_err(|error| error.to_string())?;
    *current.lock().expect("Spotify library mutex poisoned") = next;
    Ok(())
}

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
    let now = unix_now();
    let state = app.state::<AppState>();
    let membership_guard = state.spotify_library_gate.lock().await;
    let granted_scopes = token.scope.unwrap_or_else(|| auth::SCOPES.clone());
    replace_spotify_library_state(
        &state.sync_store,
        &state.spotify_library,
        SpotifyLibraryState::default(),
    )?;
    state
        .token_store
        .save(&web_oauth_tokens(
            token.access_token,
            refresh,
            now.saturating_add(token.expires_in),
            granted_scopes,
        ))
        .map_err(|error| error.to_string())?;
    *state.spotify.lock().expect("spotify mutex poisoned") =
        spotify_provider(&client_id, Arc::clone(&state.token_store))?;
    set_auto_connect(&app, true)?;
    emit_connection_state(&app)?;
    drop(membership_guard);
    sync_spotify(&app).await
}

#[tauri::command]
pub(super) async fn authorize_spotify_playback(app: tauri::AppHandle) -> Result<(), String> {
    let client_id = SessionConfig::default().client_id;
    let listener = LoopbackListener::bind_on(8898).map_err(|error| error.to_string())?;
    let redirect_uri = listener
        .redirect_uri_for("/login")
        .map_err(|error| error.to_string())?;
    let state = auth::random_state();
    let pkce = Pkce::generate();
    let url = auth::authorize_url_with_scopes(
        &client_id,
        &redirect_uri,
        &state,
        &pkce.challenge,
        auth::PLAYBACK_SCOPE,
    )
    .map_err(|error| error.to_string())?;
    app.opener()
        .open_url(url.to_string(), None::<String>)
        .map_err(|error| error.to_string())?;
    let callback = tauri::async_runtime::spawn_blocking(move || {
        listener.accept_path(&state, "/login", Duration::from_secs(180))
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

    let session = Session::new(SessionConfig::default(), None);
    session
        .connect(Credentials::with_access_token(token.access_token), false)
        .await
        .map_err(|error| error.to_string())?;
    if let Err(error) = session.login5().auth_token().await {
        session.shutdown();
        return Err(error.to_string());
    }
    let playback_username = session.username();
    let playback_auth_data = session.auth_data();
    session.shutdown();

    let state = app.state::<AppState>();
    let _membership_guard = state.spotify_library_gate.lock().await;
    let web_account_id = provider_from(&state)?
        .me()
        .await
        .map_err(|error| format!("Could not identify the connected Spotify account: {error}"))?
        .id;
    let playback_credentials =
        playback_credentials(&web_account_id, playback_username, playback_auth_data)?;
    let mut tokens = state
        .token_store
        .load()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Connect to Spotify before authorizing playback.".to_string())?;
    tokens.playback_credentials = Some(playback_credentials);
    state
        .token_store
        .save(&tokens)
        .map_err(|error| error.to_string())?;
    emit_connection_state(&app)
}

fn playback_credentials(
    web_account_id: &str,
    username: String,
    auth_data: Vec<u8>,
) -> Result<PlaybackCredentials, String> {
    if username.is_empty() || auth_data.is_empty() {
        return Err("Spotify did not return reusable playback credentials.".into());
    }
    if username != web_account_id {
        return Err(
            "Playback authorization used a different Spotify account. Try again with the account connected to Retune."
                .into(),
        );
    }
    Ok(PlaybackCredentials {
        username,
        auth_data,
    })
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
    let _membership_guard = state.spotify_library_gate.lock().await;
    set_auto_connect(&app, false)?;
    replace_spotify_library_state(
        &state.sync_store,
        &state.spotify_library,
        SpotifyLibraryState::default(),
    )?;
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

#[tauri::command(rename_all = "camelCase")]
pub(super) async fn track_artwork(
    state: tauri::State<'_, AppState>,
    uri: String,
    min_width: Option<u32>,
) -> Result<Option<String>, String> {
    let provider = provider_from(&state).ok();
    Ok(resolve_track_artwork(
        provider.as_deref(),
        &state.artwork_cache,
        &uri,
        min_width.unwrap_or(64).clamp(1, 2048),
    )
    .await)
}

#[tauri::command]
pub(super) async fn sync_from_spotify(app: tauri::AppHandle) -> Result<(), String> {
    sync_spotify(&app).await
}

#[tauri::command]
pub(super) async fn spotify_search(
    state: tauri::State<'_, AppState>,
    query: String,
    offset: u32,
) -> Result<SearchResults, String> {
    if query.trim().is_empty() {
        return Ok(SearchResults {
            artists: SearchGroup {
                items: vec![],
                total: 0,
                next_offset: None,
            },
            albums: SearchGroup {
                items: vec![],
                total: 0,
                next_offset: None,
            },
            tracks: SearchGroup {
                items: vec![],
                total: 0,
                next_offset: None,
            },
        });
    }
    if !stored_connection_state(&state.token_store)?.connected {
        return Err("Connect to Spotify to search.".into());
    }
    let provider = provider_from(&state)?;
    let mut results = provider::search(provider.as_ref(), query.trim(), offset).await?;
    let spotify_library = state
        .spotify_library
        .lock()
        .expect("Spotify library mutex poisoned")
        .clone();
    mark_album_membership(
        &state.library.lock().expect("library mutex poisoned"),
        &spotify_library,
        &mut results.albums.items,
    );
    mark_track_membership(
        &state.library.lock().expect("library mutex poisoned"),
        &spotify_library,
        &mut results.tracks.items,
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
    let spotify_library = state
        .spotify_library
        .lock()
        .expect("Spotify library mutex poisoned")
        .clone();
    Ok(album_page_view(&library, &spotify_library, album))
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
        &state
            .spotify_library
            .lock()
            .expect("Spotify library mutex poisoned")
            .clone(),
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
    let state = app.state::<AppState>();
    let _membership_guard = state.spotify_library_gate.lock().await;
    let provider = provider_from(&state)?;
    save_album_operation(&state, provider.as_ref(), &uri, &name, &artist, unix_now()).await?;
    app.emit("library-changed", ())
        .map_err(|error| error.to_string())
}

pub(crate) struct AlbumSaveResult {
    pub album_uri: String,
}

/// Saves one album entity upstream and mirrors its content locally. The
/// caller holds `spotify_library_gate`; replaying this operation is safe
/// because Spotify's library PUT is idempotent and local upsert deduplicates.
pub(crate) async fn save_album_operation<T: Transport, S: TokenStore>(
    state: &AppState,
    provider: &SpotifyClient<T, S>,
    uri: &str,
    name: &str,
    artist: &str,
    added_at: u64,
) -> Result<AlbumSaveResult, String> {
    album_id(uri).ok_or_else(|| "Expected a Spotify album URI".to_string())?;
    let (album, mut tracks) = provider::album_content(provider, uri, Some(added_at)).await?;
    for track in &mut tracks {
        if track.alb.is_empty() {
            track.alb = name.to_owned();
        }
        if track.art.is_empty() {
            track.art = artist.to_owned();
        }
    }
    let track_uris = album_track_uris(&album);
    let album_record = saved_album_record(&album, track_uris.clone(), Some(added_at));
    let album_uris = album_library_uris(&album.uri);
    let current = state
        .spotify_library
        .lock()
        .expect("Spotify library mutex poisoned")
        .clone();
    provider
        .save_to_library(&album_uris)
        .await
        .map_err(|error| error.to_string())?;
    if current.is_exact() {
        let mut next = current;
        next.add_saved_album(album_record);
        state
            .sync_store
            .save_spotify_library(&next)
            .map_err(|error| error.to_string())?;
        *state
            .spotify_library
            .lock()
            .expect("Spotify library mutex poisoned") = next;
    }
    mutate_library(state, |library| {
        for track in tracks {
            if spotify_track_match(library, &track).is_none_or(|existing| existing.uri == track.uri)
            {
                library.upsert(track);
            }
        }
        Ok(())
    })?;
    Ok(AlbumSaveResult {
        album_uri: album.uri,
    })
}

#[tauri::command]
pub(super) async fn remove_spotify_album(app: tauri::AppHandle, uri: String) -> Result<(), String> {
    album_id(&uri).ok_or_else(|| "Expected a Spotify album URI".to_string())?;
    let state = app.state::<AppState>();
    let _membership_guard = state.spotify_library_gate.lock().await;
    let provider = provider_from(&state)?;
    let current = state
        .spotify_library
        .lock()
        .expect("Spotify library mutex poisoned")
        .clone();
    if !current.is_exact() {
        provider
            .remove_from_library(&album_library_uris(&uri))
            .await
            .map_err(|error| error.to_string())?;
        return app
            .emit("library-changed", ())
            .map_err(|error| error.to_string());
    }
    let (album, tracks) = provider::album_content(provider.as_ref(), &uri, None)
        .await
        .map_err(|error| error.to_string())?;
    let uris = tracks
        .iter()
        .map(|track| track.uri.clone())
        .collect::<Vec<_>>();
    let aliases = {
        let library = state.library.lock().expect("library mutex poisoned");
        sync::spotify_track_aliases(&library, &tracks)
    };
    let album_uris = album_library_uris(&album.uri);
    provider
        .remove_from_library(&album_uris)
        .await
        .map_err(|error| error.to_string())?;
    let mut next = current;
    next.saved_albums.remove(&album.uri);
    state
        .sync_store
        .save_spotify_library(&next)
        .map_err(|error| error.to_string())?;
    *state
        .spotify_library
        .lock()
        .expect("Spotify library mutex poisoned") = next.clone();
    mutate_library(&state, |library| {
        sync::prune_unreferenced_spotify_tracks_with_aliases(library, &next, &uris, &aliases);
        Ok(())
    })?;
    app.emit("library-changed", ())
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub(super) async fn add_spotify_track(app: tauri::AppHandle, uri: String) -> Result<(), String> {
    add_spotify_tracks(app, vec![uri]).await.map(|_| ())
}

#[tauri::command]
pub(super) async fn add_spotify_tracks(
    app: tauri::AppHandle,
    uris: Vec<String>,
) -> Result<Vec<u64>, String> {
    let state = app.state::<AppState>();
    let _membership_guard = state.spotify_library_gate.lock().await;
    let provider = provider_from(&state)?;
    let ids = save_tracks_operation(&state, provider.as_ref(), uris, unix_now()).await?;
    app.emit("library-changed", ())
        .map_err(|error| error.to_string())?;
    Ok(ids)
}

/// Saves only track entities upstream and mirrors those tracks locally. The
/// caller holds `spotify_library_gate`; no album URI is synthesized here.
pub(crate) async fn save_tracks_operation<T: Transport, S: TokenStore>(
    state: &AppState,
    provider: &SpotifyClient<T, S>,
    uris: Vec<String>,
    added_at: u64,
) -> Result<Vec<u64>, String> {
    let mut seen = HashSet::new();
    let uris = uris
        .into_iter()
        .filter(|uri| seen.insert(uri.clone()))
        .collect::<Vec<_>>();
    if uris.iter().any(|uri| track_id(uri).is_none()) {
        return Err("Expected Spotify track URIs".into());
    }
    let mut ids = vec![];
    let requested_uris = uris.clone();
    if requested_uris.is_empty() {
        return Ok(ids);
    }
    let mut missing_uris = uris;
    {
        let library = state.library.lock().expect("library mutex poisoned");
        missing_uris.retain(
            |uri| match library.tracks().iter().find(|track| &track.uri == uri) {
                Some(track) => {
                    ids.push(track.id.0);
                    false
                }
                None => true,
            },
        );
    }
    let mut tracks = Vec::with_capacity(missing_uris.len());
    for uri in &missing_uris {
        let track = provider
            .track(track_id(uri).expect("validated above"))
            .await
            .map_err(|error| error.to_string())?;
        let artist = match track.artists.first() {
            Some(artist) => provider.artist(&artist.id).await.ok(),
            None => None,
        };
        tracks.push(retune_spotify::normalize::track(
            &track,
            artist.as_ref(),
            None,
        ));
        tracks.last_mut().expect("track was just pushed").added_at = Some(added_at);
    }
    let current = state
        .spotify_library
        .lock()
        .expect("Spotify library mutex poisoned")
        .clone();
    let next = if current.is_exact() {
        let mut next = current;
        for uri in &requested_uris {
            next.add_saved_track(uri.clone(), Some(added_at));
        }
        Some(next)
    } else {
        None
    };
    provider
        .save_to_library(&requested_uris)
        .await
        .map_err(|error| error.to_string())?;
    if let Some(next) = next {
        state
            .sync_store
            .save_spotify_library(&next)
            .map_err(|error| error.to_string())?;
        *state
            .spotify_library
            .lock()
            .expect("Spotify library mutex poisoned") = next;
    }
    ids.extend(mutate_library(state, |library| {
        Ok(tracks
            .into_iter()
            .map(|track| library.upsert(track).0)
            .collect::<Vec<_>>())
    })?);
    Ok(ids)
}

#[tauri::command]
pub(super) async fn remove_spotify_track(app: tauri::AppHandle, uri: String) -> Result<(), String> {
    let id = track_id(&uri).ok_or_else(|| "Expected a Spotify track URI".to_string())?;
    let state = app.state::<AppState>();
    let _membership_guard = state.spotify_library_gate.lock().await;
    let provider = provider_from(&state)?;
    let current = state
        .spotify_library
        .lock()
        .expect("Spotify library mutex poisoned")
        .clone();
    let needs_alias = current.is_exact() && {
        let library = state.library.lock().expect("library mutex poisoned");
        !library.tracks().iter().any(|track| track.uri == uri)
            && library.tracks().iter().any(|track| {
                track.source == SourceId::Music && track.uri.starts_with("spotify:track:")
            })
    };
    let aliases = if needs_alias {
        let remote_track = provider
            .track(id)
            .await
            .map_err(|error| error.to_string())?;
        let candidate = retune_spotify::normalize::track(&remote_track, None, None);
        let library = state.library.lock().expect("library mutex poisoned");
        sync::spotify_track_aliases(&library, std::slice::from_ref(&candidate))
    } else {
        std::collections::HashMap::new()
    };
    let next = if current.is_exact() {
        let mut next = current;
        next.saved_tracks.remove(&uri);
        Some(next)
    } else {
        None
    };
    provider
        .remove_from_library(std::slice::from_ref(&uri))
        .await
        .map_err(|error| error.to_string())?;
    if let Some(next) = next {
        state
            .sync_store
            .save_spotify_library(&next)
            .map_err(|error| error.to_string())?;
        *state
            .spotify_library
            .lock()
            .expect("Spotify library mutex poisoned") = next.clone();
        mutate_library(&state, |library| {
            sync::prune_unreferenced_spotify_tracks_with_aliases(
                library,
                &next,
                std::slice::from_ref(&uri),
                &aliases,
            );
            Ok(())
        })?;
    }
    app.emit("library-changed", ())
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        playback_credentials, replace_spotify_library_state, web_oauth_tokens, FsSyncStore, Mutex,
        SpotifyLibraryState,
    };

    #[test]
    fn replacing_oauth_state_clears_prior_exact_membership() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSyncStore::new(dir.path());
        let current = Mutex::new(SpotifyLibraryState {
            account_id: "prior-account".into(),
            complete: true,
            ..SpotifyLibraryState::default()
        });

        replace_spotify_library_state(&store, &current, SpotifyLibraryState::default()).unwrap();

        assert!(!current.lock().unwrap().is_exact());
        assert!(!store.spotify_library().unwrap().is_exact());
    }

    #[test]
    fn web_oauth_replacement_requires_playback_reauthorization() {
        let tokens = web_oauth_tokens(
            "new-access".into(),
            "new-refresh".into(),
            10,
            "scope".into(),
        );

        assert!(tokens.playback_credentials.is_none());
    }

    #[test]
    fn album_library_actions_use_only_the_album_uri() {
        assert_eq!(
            super::album_library_uris("spotify:album:album"),
            ["spotify:album:album"]
        );
    }

    #[test]
    fn playback_credentials_require_both_session_parts() {
        assert!(playback_credentials("user", String::new(), vec![1]).is_err());
        assert!(playback_credentials("user", "user".into(), vec![]).is_err());
        assert_eq!(
            playback_credentials("user", "user".into(), vec![1, 2, 3])
                .unwrap()
                .auth_data,
            [1, 2, 3]
        );
    }

    #[test]
    fn playback_credentials_reject_a_different_web_account() {
        let error = playback_credentials("web-user", "playback-user".into(), vec![1])
            .err()
            .unwrap();

        assert!(error.contains("different Spotify account"));
    }
}

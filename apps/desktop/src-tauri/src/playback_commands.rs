use retune_core::model::Library;
use retune_spotify::{
    catalog::SpotifyCatalog,
    client::{SpotifyClient, Transport},
    tokens::TokenStore,
};
use tauri::Manager;

use crate::{
    media_keys::MediaControl,
    playback::{PlayOutcome, PlaybackEffect, RepeatMode},
    playback_resources::{self, PlaybackResource},
    provider_from,
    settings_commands::emit_settings_changed,
    AppState,
};

pub(crate) fn execute_effect(app: &tauri::AppHandle, effect: PlaybackEffect) {
    match effect {
        PlaybackEffect::PlayerState(event) => {
            let _ = crate::emit_main_event(
                app,
                crate::main_events::MainEvent::PlayerState(event.clone()),
            );
            let state = app.state::<AppState>();
            if state.media_keys.update(&event) && event.uri.is_some() {
                tauri::async_runtime::spawn(crate::publish_media_artwork(app.clone(), event));
            }
        }
        PlaybackEffect::OperationError(error) => {
            let _ =
                crate::emit_main_event(app, crate::main_events::MainEvent::OperationError(error));
        }
        PlaybackEffect::OperationRecovered => {
            let _ = crate::emit_main_event(app, crate::main_events::MainEvent::OperationRecovered);
        }
        PlaybackEffect::ConnectionRefresh => {
            let handle = app.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(error) = crate::emit_connection_state_async(&handle).await {
                    crate::notify_error(&handle, error);
                }
            });
        }
        PlaybackEffect::AuthorizationRequired(prompt) => {
            let _ = crate::emit_main_event(
                app,
                crate::main_events::MainEvent::PlaybackAuthorizationRequired(prompt),
            );
        }
        PlaybackEffect::TrackCompleted(uri) => {
            if let Err(error) = app
                .state::<AppState>()
                .playback_effects
                .submit(crate::PlaybackDurableEffect::RecordPlay(uri))
            {
                crate::notify_error(app, error);
            }
        }
        PlaybackEffect::Listening(fact) => {
            if let Err(error) = app
                .state::<AppState>()
                .playback_effects
                .submit(crate::PlaybackDurableEffect::Listening(fact))
            {
                crate::notify_error(app, error);
            }
        }
    }
}

pub(crate) async fn handle_media_control(
    app: &tauri::AppHandle,
    control: MediaControl,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let client = provider_from(&state).ok();
    match control {
        MediaControl::SetPlaying(playing) => {
            state.playback.set_playing(client.as_deref(), playing).await
        }
        MediaControl::Toggle => state.playback.toggle(client.as_deref()).await,
        MediaControl::Next => player_step(app, 1).await,
        MediaControl::Previous => player_step(app, -1).await,
        MediaControl::Seek(seconds) => state.playback.seek(client.as_deref(), seconds).await,
    }
}

#[tauri::command(rename_all = "camelCase")]
pub(super) async fn play_tracks(
    app: tauri::AppHandle,
    resources: Vec<PlaybackResource>,
    start_index: usize,
) -> Result<PlayOutcome, String> {
    let state = app.state::<AppState>();
    let intent = state.playback.begin_play_intent();
    let library = state
        .library
        .lock()
        .expect("library mutex poisoned")
        .clone();
    let playlists = state.playlists.snapshot()?;
    let catalog = state
        .spotify_catalog
        .lock()
        .expect("Spotify catalog mutex poisoned")
        .clone();
    let client = provider_from(&state).ok();
    let (snapshot, start_index) = resolve_resources(
        resources,
        start_index,
        library,
        playlists,
        catalog,
        client.as_deref(),
    )
    .await?;
    let outcome = state
        .playback
        .play_for_intent(client.clone(), snapshot, start_index, intent)
        .await?;
    if let PlayOutcome::PlaybackAuthorizationRequired(prompt) = &outcome {
        let client = provider_from(&state).ok();
        for effect in state
            .playback
            .stop_for_authorization(client.as_deref(), prompt.clone())
            .await
        {
            execute_effect(&app, effect);
        }
    }
    Ok(outcome)
}

async fn resolve_resources<T: Transport, S: TokenStore>(
    resources: Vec<PlaybackResource>,
    start_index: usize,
    library: Library,
    playlists: crate::playlists::PlaylistCache,
    catalog: SpotifyCatalog,
    client: Option<&SpotifyClient<T, S>>,
) -> Result<(Vec<crate::playback::SnapshotTrack>, usize), String> {
    let (resources, mut resolved, enabled) = tauri::async_runtime::spawn_blocking(move || {
        let (resolved, enabled) = playback_resources::resolve_cached(
            &resources,
            start_index,
            &library,
            &playlists,
            &catalog,
        )?;
        Ok::<_, String>((resources, resolved, enabled))
    })
    .await
    .map_err(|error| error.to_string())??;
    for (resource, track) in resources.iter().zip(&mut resolved) {
        if track.is_some() {
            continue;
        }
        let id = crate::provider::spotify_id(&resource.uri, "track")
            .map_err(|_| "Spotify episode metadata is unavailable.".to_string())?;
        let fetched = client
            .ok_or_else(|| "Connect to Spotify to play this track.".to_string())?
            .track(id)
            .await
            .map_err(|error| error.to_string())?;
        if fetched.uri != resource.uri {
            return Err("Spotify returned a different playback resource.".into());
        }
        *track = Some(playback_resources::from_spotify(resource, &fetched));
    }
    let tracks = resolved
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| "Playback resource metadata is unavailable.".to_string())?;
    let (snapshot, start_index) = playback_resources::finish(tracks, start_index, &enabled)?;
    Ok((snapshot, start_index))
}

#[tauri::command]
pub(super) async fn player_toggle(app: tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let client = provider_from(&state).ok();
    state.playback.toggle(client.as_deref()).await
}

#[tauri::command]
pub(super) async fn player_next(app: tauri::AppHandle) -> Result<(), String> {
    player_step(&app, 1).await
}

pub(crate) async fn player_step(app: &tauri::AppHandle, direction: i8) -> Result<(), String> {
    let state = app.state::<AppState>();
    let client = provider_from(&state).ok();
    let outcome = if direction < 0 {
        state.playback.prev(client).await?
    } else {
        state.playback.next(client).await?
    };
    if let PlayOutcome::PlaybackAuthorizationRequired(prompt) = outcome {
        let client = provider_from(&state).ok();
        for effect in state
            .playback
            .stop_for_authorization(client.as_deref(), prompt)
            .await
        {
            execute_effect(app, effect);
        }
    }
    Ok(())
}

#[tauri::command]
pub(super) async fn player_prev(app: tauri::AppHandle) -> Result<(), String> {
    player_step(&app, -1).await
}

#[tauri::command]
pub(super) async fn player_seek(app: tauri::AppHandle, seconds: u64) -> Result<(), String> {
    let state = app.state::<AppState>();
    let client = provider_from(&state).ok();
    state.playback.seek(client.as_deref(), seconds).await
}

#[tauri::command]
pub(super) async fn player_set_volume(app: tauri::AppHandle, volume: u8) -> Result<(), String> {
    let state = app.state::<AppState>();
    // Persist first: the volume preference must survive even when applying it
    // live fails (disconnected, no active device).
    state
        .settings
        .mutate(
            |settings| {
                settings.volume = volume;
                Ok(())
            },
            |previous, current| emit_settings_changed(&app, previous, current),
        )
        .await?;
    let client = provider_from(&state).ok();
    state
        .playback
        .set_volume(client.as_deref(), volume)
        .await
        .map_err(|error| format!("Volume was saved, but could not be applied: {error}"))
}

#[tauri::command]
pub(super) async fn set_repeat(app: tauri::AppHandle, mode: RepeatMode) -> Result<(), String> {
    let state = app.state::<AppState>();
    state
        .settings
        .mutate(
            |settings| {
                settings.repeat = mode;
                Ok(())
            },
            |previous, current| emit_settings_changed(&app, previous, current),
        )
        .await?;
    let client = provider_from(&state).ok();
    state
        .playback
        .set_repeat(client.as_deref(), mode)
        .await
        .map_err(|error| format!("Repeat mode was saved, but could not be applied: {error}"))
}

#[tauri::command]
pub(super) async fn set_shuffle(app: tauri::AppHandle, shuffle: bool) -> Result<(), String> {
    let state = app.state::<AppState>();
    state
        .settings
        .mutate(
            |settings| {
                settings.shuffle = shuffle;
                Ok(())
            },
            |previous, current| emit_settings_changed(&app, previous, current),
        )
        .await?;
    let event = state.playback.set_shuffle(shuffle).await;
    crate::emit_main_event(&app, crate::main_events::MainEvent::PlayerState(event)).map_err(
        |error| format!("Shuffle was saved, but the player view could not be updated: {error}"),
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use retune_core::model::{NewTrack, SourceId};
    use retune_spotify::client::{fake_client, Response};

    use super::*;
    use crate::playlists::{CachedPlaylist, CachedTrack, PlaylistCache, TRACK_METADATA_VERSION};

    fn resource(id: u64, uri: &str) -> PlaybackResource {
        PlaybackResource {
            id,
            uri: uri.into(),
        }
    }

    fn playlist_track(uri: &str, name: &str) -> PlaylistCache {
        PlaylistCache {
            playlists: vec![CachedPlaylist {
                id: "playlist".into(),
                name: "Playlist".into(),
                snapshot_id: "snapshot".into(),
                owned: true,
                owner: None,
                track_count: 1,
                tracks: vec![uri.into()],
                track_metadata_version: TRACK_METADATA_VERSION,
                spotify_tracks: vec![CachedTrack {
                    uri: uri.into(),
                    name: name.into(),
                    art: "Artist".into(),
                    alb: "Album".into(),
                    duration: 42_000,
                    disc_no: None,
                    track_no: None,
                    release_date: None,
                }],
            }],
        }
    }

    #[tokio::test]
    async fn resolver_hydrates_episode_and_track_from_playlist_metadata() {
        let client = fake_client(Vec::<Response>::new(), "");
        for (uri, name) in [
            ("spotify:episode:episode1", "Episode"),
            ("spotify:track:track1", "Playlist Track"),
        ] {
            let (tracks, index) = resolve_resources(
                vec![resource(7, uri)],
                0,
                Library::new(),
                playlist_track(uri, name),
                SpotifyCatalog::default(),
                Some(&client),
            )
            .await
            .unwrap();
            assert_eq!((tracks[0].id, tracks[0].name.as_str(), index), (7, name, 0));
        }
        assert!(client.transport().requests().is_empty());
    }

    #[tokio::test]
    async fn resolver_hydrates_an_uncached_spotify_track_from_the_provider() {
        let client = fake_client(
            [Response::json(
                200,
                serde_json::json!({
                    "uri": "spotify:track:track2",
                    "name": "Remote Track",
                    "duration_ms": 42000,
                    "artists": [{"id": "artist1", "name": "Artist"}],
                    "album": {"id": "album1", "uri": "spotify:album:album1", "name": "Album"}
                }),
            )],
            "",
        );
        let (tracks, _) = resolve_resources(
            vec![resource(8, "spotify:track:track2")],
            0,
            Library::new(),
            PlaylistCache::default(),
            SpotifyCatalog::default(),
            Some(&client),
        )
        .await
        .unwrap();
        assert_eq!((tracks[0].id, tracks[0].name.as_str()), (8, "Remote Track"));
        assert_eq!(client.transport().requests().len(), 1);
    }

    #[tokio::test]
    async fn resolver_preserves_a_mixed_local_and_spotify_queue() {
        let mut library = Library::new();
        let local_id = library
            .add(NewTrack {
                uri: "file:///music/local.flac".into(),
                source: SourceId::Music,
                name: "Local".into(),
                duration: Duration::from_secs(10),
                ..NewTrack::default()
            })
            .0;
        let client = fake_client(Vec::<Response>::new(), "");
        let resources = [
            resource(999, "file:///music/local.flac"),
            resource(9, "spotify:track:track3"),
        ];
        let (tracks, index) = resolve_resources(
            resources.to_vec(),
            1,
            library,
            playlist_track("spotify:track:track3", "Spotify"),
            SpotifyCatalog::default(),
            Some(&client),
        )
        .await
        .unwrap();
        assert_eq!(
            tracks.iter().map(|track| track.id).collect::<Vec<_>>(),
            vec![local_id, 9]
        );
        assert_eq!(index, 1);
        assert!(client.transport().requests().is_empty());
    }
}

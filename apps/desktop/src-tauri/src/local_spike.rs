#![cfg(debug_assertions)]

use std::time::Duration;

use librespot_core::{
    authentication::Credentials, config::SessionConfig, session::Session, spotify_uri::SpotifyUri,
};
use librespot_playback::{
    audio_backend,
    config::{AudioFormat, PlayerConfig},
    mixer::NoOpVolume,
    player::Player,
};
use retune_spotify::tokens::TokenStore;
use tauri::{menu::Submenu, Emitter, Manager};

use crate::AppState;

const MENU_ID: &str = "local_playback_spike";

pub fn menu(app: &tauri::App) -> tauri::Result<Submenu<tauri::Wry>> {
    tauri::menu::SubmenuBuilder::new(app, "Debug")
        .text(MENU_ID, "Local Playback Spike")
        .build()
}

pub fn handles(id: &str) -> bool {
    id == MENU_ID
}

pub fn start(app: &tauri::AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(error) = run(&app).await {
            log::error!("{error}");
            let _ = app.emit("operation-error", error);
        }
    });
}

async fn run(app: &tauri::AppHandle) -> Result<(), String> {
    let (client, uri) = {
        let state = app.state::<AppState>();
        let stored = state
            .token_store
            .load()
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "Connect to Spotify before starting local playback.".to_string())?;
        if !stored
            .scopes
            .split_whitespace()
            .any(|scope| scope == "streaming")
        {
            return Err("Reconnect to Spotify to grant playback permission (Account → Disconnect, then Connect)".into());
        }
        let uri = state
            .library
            .lock()
            .expect("library mutex poisoned")
            .tracks()
            .iter()
            .find(|track| track.uri.starts_with("spotify:track:"))
            .map(|track| track.uri.clone())
            .ok_or_else(|| "No Spotify track is available in the current library.".to_string())?;
        (super::provider_from(&state)?, uri)
    };

    if !client
        .me()
        .await
        .map_err(|error| error.to_string())?
        .is_premium()
    {
        return Err("Local playback requires Spotify Premium.".into());
    }
    let access = app
        .state::<AppState>()
        .token_store
        .load()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Spotify disconnected during playback setup.".to_string())?
        .access;

    let session = Session::new(SessionConfig::default(), None);
    session
        .connect(Credentials::with_access_token(access), false)
        .await
        .map_err(|error| error.to_string())?;

    let sink = audio_backend::find(Some("rodio".into()))
        .ok_or_else(|| "librespot rodio output is unavailable.".to_string())?;
    let player = Player::new(
        PlayerConfig {
            position_update_interval: Some(Duration::from_secs(1)),
            ..PlayerConfig::default()
        },
        session.clone(),
        Box::new(NoOpVolume),
        move || sink(None, AudioFormat::default()),
    );
    let mut events = player.get_player_event_channel();
    let uri = SpotifyUri::from_uri(&uri).map_err(|error| error.to_string())?;
    player.load(uri, true, 0);

    let events = tauri::async_runtime::spawn(async move {
        let _ = tokio::time::timeout(Duration::from_secs(30), async {
            while let Some(event) = events.recv().await {
                log::info!("Local playback spike: {event:?}");
            }
        })
        .await;
    });
    let _ = events.await;
    player.stop();
    drop(player);
    session.shutdown();
    Ok(())
}

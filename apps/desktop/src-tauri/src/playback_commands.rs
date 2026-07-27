use super::*;

#[tauri::command(rename_all = "camelCase")]
pub(super) async fn play_tracks(
    app: tauri::AppHandle,
    snapshot: Vec<SnapshotTrack>,
    start_index: usize,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let client = provider_from(&state).ok();
    state.playback.play(client, snapshot, start_index).await
}

#[tauri::command]
pub(super) async fn player_toggle(app: tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let client = provider_from(&state).ok();
    state.playback.toggle(client.as_deref()).await
}

#[tauri::command]
pub(super) async fn player_next(app: tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let client = provider_from(&state).ok();
    state.playback.next(client).await
}

#[tauri::command]
pub(super) async fn player_prev(app: tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let client = provider_from(&state).ok();
    state.playback.prev(client).await
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
pub(super) async fn set_repeat(app: tauri::AppHandle, mode: String) -> Result<(), String> {
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
pub(super) async fn set_shuffle(app: tauri::AppHandle, shuffle: bool) -> Result<(), String> {
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
pub(super) async fn set_audio_settings(
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

mod connect;
mod local;
mod reducer;

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use connect::ConnectBackend;
use local::LocalBackend;
use reducer::{EventReducer, ReducerAction};
use retune_spotify::client::{HttpTransport, SpotifyClient};
use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager};
use tokio::sync::mpsc;

type LiveClient = SpotifyClient<HttpTransport, crate::SharedTokenStore>;

const AUDIOBOOK_ERROR: &str = "Audiobook playback isn't supported yet.";
const RECONNECT_DELAYS: &[u64] = &[0, 1, 2, 4, 8, 15, 30];

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotTrack {
    pub id: u64,
    pub uri: String,
    pub name: String,
    pub art: String,
    pub alb: String,
    pub duration_secs: u64,
}

#[derive(Clone, Debug)]
pub(super) struct Snapshot {
    tracks: Vec<SnapshotTrack>,
    index: usize,
}

impl Snapshot {
    fn current(&self) -> &SnapshotTrack {
        &self.tracks[self.index]
    }

    fn has_next(&self) -> bool {
        self.index + 1 < self.tracks.len()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerStateEvent {
    pub track_id: Option<u64>,
    pub elapsed: u64,
    pub is_playing: bool,
    pub external: bool,
    pub name: Option<String>,
    pub art: Option<String>,
    pub alb: Option<String>,
    pub duration_secs: Option<u64>,
    pub volume_supported: bool,
}

#[derive(Clone, Debug)]
pub(super) struct NeutralState {
    uri: Option<String>,
    position_ms: u32,
    is_playing: bool,
    external: bool,
    name: Option<String>,
    art: Option<String>,
    alb: Option<String>,
    duration_ms: Option<u32>,
    volume_supported: bool,
}

#[derive(Clone, Debug)]
pub(super) enum NeutralEvent {
    ConnectState {
        generation: u64,
        state: NeutralState,
    },
    RequestIdChanged {
        generation: u64,
        request_id: u64,
    },
    Loading {
        generation: u64,
        request_id: u64,
        uri: String,
        position_ms: u32,
    },
    Playing {
        generation: u64,
        request_id: u64,
        uri: String,
        position_ms: u32,
    },
    Paused {
        generation: u64,
        request_id: u64,
        uri: String,
        position_ms: u32,
    },
    PositionChanged {
        generation: u64,
        request_id: u64,
        uri: String,
        position_ms: u32,
    },
    Seeked {
        generation: u64,
        request_id: u64,
        uri: String,
        position_ms: u32,
    },
    PositionCorrection {
        generation: u64,
        request_id: u64,
        uri: String,
        position_ms: u32,
    },
    Unavailable {
        generation: u64,
        request_id: u64,
        uri: String,
    },
    Stopped {
        generation: u64,
        request_id: u64,
        uri: String,
    },
    EndOfTrack {
        generation: u64,
        request_id: u64,
        uri: String,
    },
    Error {
        generation: u64,
        message: String,
    },
    Disconnected {
        generation: u64,
    },
}

impl NeutralEvent {
    fn generation(&self) -> u64 {
        match self {
            Self::ConnectState { generation, .. }
            | Self::RequestIdChanged { generation, .. }
            | Self::Loading { generation, .. }
            | Self::Playing { generation, .. }
            | Self::Paused { generation, .. }
            | Self::PositionChanged { generation, .. }
            | Self::Seeked { generation, .. }
            | Self::PositionCorrection { generation, .. }
            | Self::Unavailable { generation, .. }
            | Self::Stopped { generation, .. }
            | Self::EndOfTrack { generation, .. }
            | Self::Error { generation, .. }
            | Self::Disconnected { generation } => *generation,
        }
    }
}

enum PlayerBackend {
    Connect(ConnectBackend),
    Local(LocalBackend),
}

impl PlayerBackend {
    fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
    }

    async fn play(
        &mut self,
        client: Arc<LiveClient>,
        snapshot: Snapshot,
        repeat: &str,
    ) -> Result<(), String> {
        match self {
            Self::Connect(backend) => backend.play(client, snapshot, repeat).await,
            Self::Local(backend) => backend.play(snapshot, true, 0),
        }
    }

    async fn toggle(&mut self, client: &LiveClient) -> Result<(), String> {
        match self {
            Self::Connect(backend) => backend.toggle(client).await,
            Self::Local(backend) => backend.toggle(),
        }
    }

    async fn step(
        &mut self,
        client: &LiveClient,
        reducer: &mut EventReducer,
        direction: i8,
        wrap: bool,
    ) -> Result<(), String> {
        match self {
            Self::Connect(backend) => backend.step(client, direction).await,
            Self::Local(backend) => {
                let Some(snapshot) = reducer.snapshot_mut() else {
                    return Err("Nothing is playing".into());
                };
                let Some(next) = step_index(snapshot.index, snapshot.tracks.len(), direction, wrap)
                else {
                    backend.stop();
                    return Ok(());
                };
                if reject_chapter(&snapshot.tracks[next].uri) {
                    return Err(AUDIOBOOK_ERROR.into());
                }
                snapshot.index = next;
                let uri = snapshot.current().uri.clone();
                let snapshot = snapshot.clone();
                reducer.queue_load(&uri, true);
                backend.play(snapshot, true, 0)
            }
        }
    }

    async fn seek(&mut self, client: &LiveClient, seconds: u64) -> Result<(), String> {
        match self {
            Self::Connect(backend) => backend.seek(client, seconds).await,
            Self::Local(backend) => backend.seek(seconds),
        }
    }

    async fn set_volume(&mut self, client: &LiveClient, volume: u8) -> Result<(), String> {
        match self {
            Self::Connect(backend) => backend.set_live_volume(client, volume).await,
            Self::Local(backend) => backend.set_volume(volume),
        }
    }

    async fn set_repeat(
        &mut self,
        client: Option<&LiveClient>,
        repeat: &str,
    ) -> Result<(), String> {
        match (self, client) {
            (Self::Connect(backend), Some(client)) => backend.set_live_repeat(client, repeat).await,
            _ => Ok(()),
        }
    }

    fn reload(&mut self, reducer: &mut EventReducer) -> Result<(), String> {
        let snapshot = reducer.snapshot().cloned().ok_or("Nothing is playing")?;
        let uri = snapshot.current().uri.clone();
        reducer.queue_load(&uri, true);
        match self {
            Self::Local(backend) => backend.play(snapshot, true, 0),
            Self::Connect(_) => Err("Connect repeat is controlled by Spotify".into()),
        }
    }

    async fn stop(&mut self, client: Option<&LiveClient>) -> Result<(), String> {
        match self {
            Self::Connect(backend) => backend.stop(client).await,
            Self::Local(backend) => {
                backend.stop();
                Ok(())
            }
        }
    }
}

struct ControllerState {
    backend: PlayerBackend,
    generation: u64,
    reducer: EventReducer,
    volume: u8,
}

pub struct Playback {
    state: tokio::sync::Mutex<ControllerState>,
    events: mpsc::UnboundedSender<NeutralEvent>,
    receiver: Mutex<Option<mpsc::UnboundedReceiver<NeutralEvent>>>,
    cache_dir: Option<PathBuf>,
}

impl Default for Playback {
    fn default() -> Self {
        Self::new("off", None)
    }
}

impl Playback {
    pub fn new(repeat: &str, cache_dir: Option<PathBuf>) -> Self {
        let (events, receiver) = mpsc::unbounded_channel();
        let generation = 1;
        let mut reducer = EventReducer::default();
        reducer.activate(generation);
        reducer.set_repeat(repeat);
        Self {
            state: tokio::sync::Mutex::new(ControllerState {
                backend: PlayerBackend::Connect(ConnectBackend::new(events.clone(), generation)),
                generation,
                reducer,
                volume: 62,
            }),
            events,
            receiver: Mutex::new(Some(receiver)),
            cache_dir,
        }
    }

    pub fn listen(self: &Arc<Self>, app: tauri::AppHandle) {
        let mut receiver = self
            .receiver
            .lock()
            .expect("playback receiver mutex poisoned")
            .take()
            .expect("playback event loop starts once");
        let playback = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            while let Some(event) = receiver.recv().await {
                playback.handle_event(&app, event).await;
            }
        });
    }

    pub async fn play(
        &self,
        client: Arc<LiveClient>,
        tracks: Vec<SnapshotTrack>,
        index: usize,
    ) -> Result<(), String> {
        if tracks.is_empty() || index >= tracks.len() {
            return Err("Choose a track to play".into());
        }
        if reject_chapter(&tracks[index].uri) {
            let generation = self.state.lock().await.generation;
            let _ = self.events.send(NeutralEvent::Error {
                generation,
                message: AUDIOBOOK_ERROR.into(),
            });
            return Ok(());
        }
        let snapshot = Snapshot { tracks, index };
        let mut state = self.state.lock().await;
        self.ensure_valid(&mut state, client.as_ref()).await?;
        if matches!(state.backend, PlayerBackend::Local(_)) {
            state.reducer.set_snapshot(Some(snapshot.clone()));
            state.reducer.queue_load(&snapshot.current().uri, true);
            state.backend.play(client, snapshot, "off").await
        } else {
            let repeat = state.reducer.repeat().to_owned();
            state
                .backend
                .play(client, snapshot.clone(), &repeat)
                .await?;
            state.reducer.set_snapshot(Some(snapshot));
            Ok(())
        }
    }

    pub async fn toggle(&self, client: &LiveClient) -> Result<(), String> {
        let mut state = self.state.lock().await;
        self.ensure_valid(&mut state, client).await?;
        state.backend.toggle(client).await
    }

    pub async fn next(&self, client: &LiveClient) -> Result<(), String> {
        self.step(client, 1).await
    }

    pub async fn prev(&self, client: &LiveClient) -> Result<(), String> {
        self.step(client, -1).await
    }

    async fn step(&self, client: &LiveClient, direction: i8) -> Result<(), String> {
        let mut state = self.state.lock().await;
        self.ensure_valid(&mut state, client).await?;
        self.step_locked(&mut state, client, direction).await
    }

    pub async fn seek(&self, client: &LiveClient, seconds: u64) -> Result<(), String> {
        let mut state = self.state.lock().await;
        self.ensure_valid(&mut state, client).await?;
        state.backend.seek(client, seconds).await
    }

    pub async fn set_volume(&self, client: &LiveClient, volume: u8) -> Result<(), String> {
        if volume > 100 {
            return Err("volume must be between 0 and 100".into());
        }
        let mut state = self.state.lock().await;
        self.ensure_valid(&mut state, client).await?;
        state.backend.set_volume(client, volume).await?;
        state.volume = volume;
        Ok(())
    }

    pub async fn set_repeat(
        &self,
        client: Option<&LiveClient>,
        repeat: &str,
    ) -> Result<(), String> {
        if !matches!(repeat, "off" | "all" | "one") {
            return Err("repeat must be off, all, or one".into());
        }
        let mut state = self.state.lock().await;
        state.backend.set_repeat(client, repeat).await?;
        state.reducer.set_repeat(repeat);
        Ok(())
    }

    pub async fn stop(&self, client: Option<&LiveClient>) -> Result<(), String> {
        let mut state = self.state.lock().await;
        state.backend.stop(client).await?;
        state.reducer.set_snapshot(None);
        Ok(())
    }

    pub async fn switch_to_local(&self, client: &LiveClient, volume: u8) -> Result<(), String> {
        self.switch_to_local_with(Some(client), || async {
            let state = self.state.lock().await;
            let generation = state.generation.wrapping_add(1);
            drop(state);
            LocalBackend::activate(
                client,
                self.events.clone(),
                generation,
                volume,
                self.cache_dir.as_deref(),
            )
            .await
        })
        .await
    }

    async fn switch_to_local_with<F, Fut>(
        &self,
        client: Option<&LiveClient>,
        prepare: F,
    ) -> Result<(), String>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<LocalBackend, String>>,
    {
        let local = prepare().await?;
        let volume = local.volume();
        let mut state = self.state.lock().await;
        if let PlayerBackend::Connect(connect) = &mut state.backend {
            connect.stop(client).await?;
        }
        state.generation = local.generation();
        let generation = state.generation;
        state.reducer.activate(generation);
        state.backend = PlayerBackend::Local(local);
        state.volume = volume;
        Ok(())
    }

    pub async fn switch_to_connect(&self) {
        let mut state = self.state.lock().await;
        state.generation = state.generation.wrapping_add(1);
        let generation = state.generation;
        if let PlayerBackend::Local(local) = &mut state.backend {
            local.teardown();
        }
        state.reducer.activate(generation);
        state.backend =
            PlayerBackend::Connect(ConnectBackend::new(self.events.clone(), generation));
    }

    pub async fn invalidate_local(&self) {
        let mut state = self.state.lock().await;
        if matches!(state.backend, PlayerBackend::Local(_)) {
            state.generation = state.generation.wrapping_add(1);
            let generation = state.generation;
            state.reducer.activate(generation);
            if let PlayerBackend::Local(local) = &mut state.backend {
                local.teardown();
            }
        }
    }

    /// Whether the LOCAL backend is what commands currently dispatch to —
    /// can differ from the persisted setting when activation failed and
    /// playback fell back to Connect.
    pub async fn is_local_active(&self) -> bool {
        self.state.lock().await.backend.is_local()
    }

    async fn ensure_valid(
        &self,
        state: &mut ControllerState,
        client: &LiveClient,
    ) -> Result<(), String> {
        let invalid = matches!(&state.backend, PlayerBackend::Local(local) if local.is_invalid());
        if !invalid {
            return Ok(());
        }
        log::info!("Recreating local playback session");
        let generation = state.generation.wrapping_add(1);
        let mut local = LocalBackend::activate(
            client,
            self.events.clone(),
            generation,
            state.volume,
            self.cache_dir.as_deref(),
        )
        .await?;
        let restore = state.reducer.snapshot().cloned().map(|snapshot| {
            let playing = state.reducer.state().is_playing;
            let position_ms = u32::try_from(state.reducer.state().elapsed.saturating_mul(1000))
                .unwrap_or(u32::MAX);
            (snapshot, playing, position_ms)
        });
        state.generation = generation;
        state.reducer.activate(generation);
        if let Some((snapshot, playing, position_ms)) = restore {
            state.reducer.queue_load(&snapshot.current().uri, playing);
            local.play(snapshot, playing, position_ms)?;
        }
        state.backend = PlayerBackend::Local(local);
        Ok(())
    }

    /// Outcome of one reconnect attempt. Superseded means a newer
    /// generation exists or there is nothing to resume — stop retrying.
    async fn try_reconnect(&self, client: &LiveClient, generation: u64) -> Result<bool, String> {
        let mut state = self.state.lock().await;
        if state.generation != generation
            || !state.backend.is_local()
            || state.reducer.snapshot().is_none()
        {
            return Ok(false);
        }
        self.ensure_valid(&mut state, client).await?;
        Ok(true)
    }

    async fn step_locked(
        &self,
        state: &mut ControllerState,
        client: &LiveClient,
        direction: i8,
    ) -> Result<(), String> {
        let wrap = direction > 0 && state.reducer.repeat() == "all";
        let ControllerState {
            backend, reducer, ..
        } = state;
        backend.step(client, reducer, direction, wrap).await
    }

    async fn handle_event(&self, app: &tauri::AppHandle, event: NeutralEvent) {
        let mut state = self.state.lock().await;
        let actions = state.reducer.handle(event);
        for action in actions {
            match action {
                ReducerAction::Emit(event) => {
                    let _ = app.emit("player-state", &event);
                    app.state::<crate::AppState>().media_keys.update(&event);
                }
                ReducerAction::Error(error) => {
                    let _ = app.emit("operation-error", error);
                }
                ReducerAction::Advance => {
                    let client = match crate::provider_from(&app.state::<crate::AppState>()) {
                        Ok(client) => client,
                        Err(error) => {
                            let _ = app.emit("operation-error", error);
                            continue;
                        }
                    };
                    if let Err(error) = self.step_locked(&mut state, client.as_ref(), 1).await {
                        let _ = app.emit("operation-error", error);
                    }
                }
                ReducerAction::Reload => {
                    let ControllerState {
                        backend, reducer, ..
                    } = &mut *state;
                    if let Err(error) = backend.reload(reducer) {
                        let _ = app.emit("operation-error", error);
                    }
                }
                ReducerAction::Invalidate => {
                    log::info!("Local playback session lost; will reconnect on next use");
                    state.generation = state.generation.wrapping_add(1);
                    let generation = state.generation;
                    state.reducer.activate(generation);
                    if let PlayerBackend::Local(local) = &mut state.backend {
                        local.teardown();
                    }
                }
                ReducerAction::Reconnect => {
                    let generation = state.generation;
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        for (attempt, delay) in RECONNECT_DELAYS.iter().enumerate() {
                            if *delay > 0 {
                                tokio::time::sleep(Duration::from_secs(*delay)).await;
                            }
                            let app_state = app.state::<crate::AppState>();
                            let playback = Arc::clone(&app_state.playback);
                            let client = match crate::provider_from(&app_state) {
                                Ok(client) => client,
                                Err(error) => {
                                    log::info!("Stopping playback reconnect: {error}");
                                    return;
                                }
                            };
                            match playback.try_reconnect(client.as_ref(), generation).await {
                                Ok(true) => {
                                    let _ = app.emit("operation-recovered", ());
                                    log::info!("Local playback session reconnected");
                                    return;
                                }
                                Ok(false) => {
                                    // A user action took over (play, stop, backend
                                    // switch) — the reconnect banner no longer
                                    // describes reality, so clear it.
                                    let _ = app.emit("operation-recovered", ());
                                    log::debug!("Playback reconnect superseded");
                                    return;
                                }
                                Err(error) => {
                                    log::info!(
                                        "Playback reconnect attempt {} failed: {error}",
                                        attempt + 1
                                    );
                                    if attempt == 0 {
                                        let _ =
                                            app.emit("operation-error", "Reconnecting to Spotify…");
                                    }
                                }
                            }
                        }
                        let _ = app.emit(
                            "operation-error",
                            "Spotify playback lost its network connection.",
                        );
                    });
                }
            }
        }
    }
}

fn step_index(index: usize, len: usize, direction: i8, wrap: bool) -> Option<usize> {
    if direction < 0 {
        Some(index.saturating_sub(1))
    } else if index + 1 < len {
        Some(index + 1)
    } else {
        wrap.then_some(0)
    }
}

fn reject_chapter(uri: &str) -> bool {
    uri.starts_with("spotify:chapter:")
}

fn local_event(
    track: &SnapshotTrack,
    elapsed: u64,
    is_playing: bool,
    volume_supported: bool,
) -> PlayerStateEvent {
    PlayerStateEvent {
        track_id: Some(track.id),
        elapsed,
        is_playing,
        external: false,
        name: Some(track.name.clone()),
        art: Some(track.art.clone()),
        alb: Some(track.alb.clone()),
        duration_secs: Some(track.duration_secs),
        volume_supported,
    }
}

pub fn empty_event(external: bool) -> PlayerStateEvent {
    PlayerStateEvent {
        track_id: None,
        elapsed: 0,
        is_playing: false,
        external,
        name: None,
        art: None,
        alb: None,
        duration_secs: None,
        volume_supported: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use retune_spotify::tokens::{CachedTokenStore, InMemoryTokenStore, TokenStore};

    #[tokio::test]
    async fn failed_switch_keeps_connect_backend() {
        let playback = Playback::default();
        let result = playback
            .switch_to_local_with(None, || async { Err("preflight failed".into()) })
            .await;
        assert_eq!(result.unwrap_err(), "preflight failed");
        assert!(!playback.is_local_active().await);
    }

    #[tokio::test]
    async fn reconnect_stops_when_generation_is_superseded() {
        let tokens: Box<dyn TokenStore> = Box::new(InMemoryTokenStore::new(None));
        let client = SpotifyClient::new(
            "test",
            HttpTransport::new(),
            Arc::new(CachedTokenStore::new(tokens)),
        );
        assert!(!Playback::default().try_reconnect(&client, 0).await.unwrap());
    }

    #[test]
    fn volume_mapping_endpoints_are_exact() {
        assert_eq!(local::soft_volume(0), 0);
        assert_eq!(local::soft_volume(100), u16::MAX);
    }

    #[test]
    fn repeat_all_wraps_and_off_stops_at_queue_end() {
        assert_eq!(step_index(1, 2, 1, true), Some(0));
        assert_eq!(step_index(1, 2, 1, false), None);
    }
}

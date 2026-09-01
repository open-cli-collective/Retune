mod connect;
mod file;
mod local;
mod reducer;

use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use connect::ConnectBackend;
use file::FileEngine;
use local::LocalBackend;
use rand::seq::SliceRandom;
use reducer::{EventReducer, ReducerAction};
use retune_spotify::client::{HttpTransport, SpotifyClient};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

type LiveClient = SpotifyClient<HttpTransport, crate::SharedTokenStore>;
type ProviderResolver = dyn Fn() -> Result<Arc<LiveClient>, String> + Send + Sync;
type EffectSink = dyn Fn(PlaybackEffect) + Send + Sync;

const AUDIOBOOK_ERROR: &str = "Audiobook playback isn't supported yet.";
const RECONNECT_DELAYS: &[u64] = &[0, 1, 2, 4, 8, 15, 30];

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PlaybackBackend {
    Connect,
    #[default]
    Local,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RepeatMode {
    #[default]
    Off,
    All,
    One,
}

impl RepeatMode {
    fn wraps(self) -> bool {
        match self {
            Self::Off | Self::One => false,
            Self::All => true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PlaybackAuthorizationReason {
    Missing,
    Rejected,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackAuthorizationPrompt {
    reason: PlaybackAuthorizationReason,
    message: String,
    target_track_id: u64,
    target_track_uri: String,
    #[serde(skip)]
    intent: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PlayOutcome {
    Started,
    PlaybackAuthorizationRequired(PlaybackAuthorizationPrompt),
}

#[derive(Clone, Debug)]
pub(crate) enum PlaybackEffect {
    PlayerState(PlayerStateEvent),
    OperationError(String),
    OperationRecovered,
    ConnectionRefresh,
    AuthorizationRequired(PlaybackAuthorizationPrompt),
    TrackCompleted(String),
    Listening(ListeningFact),
}

#[derive(Debug)]
pub(super) enum PlaybackError {
    AuthorizationRequired {
        reason: PlaybackAuthorizationReason,
        target_track_id: Option<u64>,
        target_track_uri: Option<String>,
    },
    Message(String),
}

impl PlaybackError {
    fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }

    fn authorization(reason: PlaybackAuthorizationReason) -> Self {
        Self::AuthorizationRequired {
            reason,
            target_track_id: None,
            target_track_uri: None,
        }
    }

    fn with_target(self, target_track_id: u64, target_track_uri: &str) -> Self {
        match self {
            Self::AuthorizationRequired { reason, .. } => Self::AuthorizationRequired {
                reason,
                target_track_id: Some(target_track_id),
                target_track_uri: Some(target_track_uri.into()),
            },
            error => error,
        }
    }

    fn into_prompt(
        self,
        fallback_target_track_id: u64,
        fallback_target_track_uri: &str,
        intent: u64,
    ) -> Option<PlaybackAuthorizationPrompt> {
        match self {
            Self::AuthorizationRequired {
                reason,
                target_track_id,
                target_track_uri,
            } => Some(PlaybackAuthorizationPrompt {
                reason,
                message: reason.message().into(),
                target_track_id: target_track_id.unwrap_or(fallback_target_track_id),
                target_track_uri: target_track_uri
                    .or_else(|| {
                        (!fallback_target_track_uri.is_empty())
                            .then(|| fallback_target_track_uri.into())
                    })
                    .expect("authorization prompts carry a target URI"),
                intent,
            }),
            Self::Message(_) => None,
        }
    }

    fn into_string(self) -> String {
        match self {
            Self::AuthorizationRequired { reason, .. } => reason.message().into(),
            Self::Message(message) => message,
        }
    }
}

impl std::fmt::Display for PlaybackError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AuthorizationRequired { reason, .. } => formatter.write_str(reason.message()),
            Self::Message(message) => formatter.write_str(message),
        }
    }
}

impl PlaybackAuthorizationReason {
    fn message(self) -> &'static str {
        match self {
            Self::Missing => {
                "Spotify playback needs one-time authorization before this track can play."
            }
            Self::Rejected => {
                "Spotify rejected playback authorization. Spotify Premium is required; authorize playback again before retrying this track."
            }
        }
    }
}

impl From<String> for PlaybackError {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}

impl From<&str> for PlaybackError {
    fn from(message: &str) -> Self {
        Self::Message(message.into())
    }
}

impl From<PlaybackError> for String {
    fn from(error: PlaybackError) -> Self {
        error.into_string()
    }
}

#[derive(Clone, Copy)]
pub struct AudioSettings {
    pub bitrate: u16,
    pub normalize: bool,
    pub gapless: bool,
}

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
pub(crate) enum ListeningFact {
    Started {
        generation: u64,
        track: SnapshotTrack,
    },
    Forward {
        generation: u64,
        track: SnapshotTrack,
        played_ms: u64,
    },
    Discontinuity {
        generation: u64,
        track: SnapshotTrack,
    },
    Completed {
        generation: u64,
        track: SnapshotTrack,
    },
}

#[derive(Clone, Debug)]
pub(super) struct Snapshot {
    tracks: Vec<SnapshotTrack>,
    order: Vec<usize>,
    index: usize,
}

impl Snapshot {
    fn new(tracks: Vec<SnapshotTrack>, index: usize) -> Self {
        Self {
            order: (0..tracks.len()).collect(),
            tracks,
            index,
        }
    }

    fn new_with(
        tracks: Vec<SnapshotTrack>,
        index: usize,
        shuffle: bool,
        permute: impl FnOnce(&mut [usize]),
    ) -> Self {
        let mut snapshot = Self::new(tracks, index);
        if shuffle {
            snapshot.set_shuffle_with(true, permute);
        }
        snapshot
    }

    fn current(&self) -> &SnapshotTrack {
        self.track_at(self.index)
    }

    fn track_at(&self, index: usize) -> &SnapshotTrack {
        &self.tracks[self.order[index]]
    }

    fn len(&self) -> usize {
        self.order.len()
    }

    fn active_tracks(&self) -> Vec<SnapshotTrack> {
        self.order
            .iter()
            .map(|&index| self.tracks[index].clone())
            .collect()
    }

    fn active_position(&self, uri: &str) -> Option<usize> {
        if self.current().uri == uri {
            return Some(self.index);
        }
        self.order[self.index + 1..]
            .iter()
            .position(|&index| self.tracks[index].uri == uri)
            .map(|offset| self.index + 1 + offset)
            .or_else(|| {
                self.order[..self.index]
                    .iter()
                    .position(|&index| self.tracks[index].uri == uri)
            })
    }

    fn set_shuffle_with(&mut self, shuffle: bool, permute: impl FnOnce(&mut [usize])) {
        if shuffle {
            permute(&mut self.order[self.index + 1..]);
        } else {
            let canonical_index = self.order[self.index];
            self.order = (0..self.tracks.len()).collect();
            self.index = canonical_index;
        }
    }

    fn has_next(&self) -> bool {
        self.index + 1 < self.len()
    }

    fn exclude(&mut self, id: u64) -> bool {
        let current = self.order[self.index];
        let previous_len = self.order.len();
        self.order
            .retain(|&index| index == current || self.tracks[index].id != id);
        self.index = self
            .order
            .iter()
            .position(|&index| index == current)
            .expect("current track is retained");
        self.order.len() != previous_len
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerStateEvent {
    pub track_id: Option<u64>,
    pub uri: Option<String>,
    pub elapsed: u64,
    pub is_playing: bool,
    pub external: bool,
    pub name: Option<String>,
    pub art: Option<String>,
    pub alb: Option<String>,
    pub duration_secs: Option<u64>,
    pub volume_supported: bool,
    pub shuffle: bool,
}

#[derive(Clone, Debug, Default)]
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
    PreloadSuggested {
        generation: u64,
        request_id: u64,
        uri: String,
    },
    EndOfTrack {
        generation: u64,
        request_id: u64,
        uri: String,
    },
    ConnectBoundary {
        generation: u64,
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
            | Self::PreloadSuggested { generation, .. }
            | Self::EndOfTrack { generation, .. }
            | Self::ConnectBoundary { generation, .. }
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
        repeat: RepeatMode,
    ) -> Result<(), String> {
        match self {
            Self::Connect(backend) => backend.play(client, snapshot, repeat).await,
            Self::Local(backend) => backend.play(snapshot, true, 0),
        }
    }

    fn preload(&self, uri: &str) -> Result<bool, String> {
        match self {
            Self::Connect(_) => Ok(false),
            Self::Local(backend) => backend.preload(uri),
        }
    }

    async fn toggle(&mut self, client: &LiveClient) -> Result<(), String> {
        match self {
            Self::Connect(backend) => backend.toggle(client).await,
            Self::Local(backend) => backend.toggle(),
        }
    }

    async fn set_playing(&mut self, client: &LiveClient, playing: bool) -> Result<(), String> {
        match self {
            Self::Connect(backend) => backend.set_playing(client, playing).await,
            Self::Local(backend) => backend.set_playing(playing),
        }
    }

    async fn step(&mut self, client: &LiveClient, direction: i8) -> Result<(), String> {
        match self {
            Self::Connect(backend) => backend.step(client, direction).await,
            Self::Local(backend) => {
                backend.stop();
                Ok(())
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
            Self::Connect(backend) => backend.set_volume(client, volume).await,
            Self::Local(backend) => backend.set_volume(volume),
        }
    }

    async fn set_repeat(
        &mut self,
        client: Option<&LiveClient>,
        repeat: RepeatMode,
    ) -> Result<(), String> {
        match (self, client) {
            (Self::Connect(backend), Some(client)) => {
                backend.set_repeat_state(client, repeat).await
            }
            _ => Ok(()),
        }
    }

    async fn set_shuffle_snapshot(&mut self, snapshot: Option<Snapshot>, repeat: RepeatMode) {
        if let (Self::Connect(backend), Some(snapshot)) = (self, snapshot) {
            backend.update_snapshot(snapshot, repeat).await;
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
    file: FileEngine,
    generation: u64,
    reducer: EventReducer,
    volume: u8,
}

pub struct Playback {
    state: tokio::sync::Mutex<ControllerState>,
    backend: tokio::sync::Mutex<PlayerBackend>,
    events: mpsc::UnboundedSender<NeutralEvent>,
    receiver: Mutex<Option<mpsc::UnboundedReceiver<NeutralEvent>>>,
    cache_dir: Option<PathBuf>,
    audio: Mutex<AudioSettings>,
    local_requested: AtomicBool,
    local_active: AtomicBool,
    backend_intent: AtomicU64,
    play_intent: AtomicU64,
}

impl Default for Playback {
    fn default() -> Self {
        Self::new(
            RepeatMode::Off,
            false,
            100,
            AudioSettings {
                bitrate: 320,
                normalize: false,
                gapless: true,
            },
            None,
        )
    }
}

impl Playback {
    async fn commit_if_generation<T>(
        &self,
        generation: u64,
        commit: impl FnOnce(&mut ControllerState) -> T,
    ) -> Option<T> {
        let mut state = self.state.lock().await;
        (state.generation == generation).then(|| commit(&mut state))
    }

    pub fn new(
        repeat: RepeatMode,
        shuffle: bool,
        play_threshold_percent: u8,
        audio: AudioSettings,
        cache_dir: Option<PathBuf>,
    ) -> Self {
        let (events, receiver) = mpsc::unbounded_channel();
        let generation = 1;
        let mut reducer = EventReducer::default();
        reducer.activate(generation);
        reducer.set_repeat(repeat);
        reducer.set_shuffle_with(shuffle, |suffix| suffix.shuffle(&mut rand::rng()));
        reducer.set_play_threshold_percent(play_threshold_percent);
        Self {
            state: tokio::sync::Mutex::new(ControllerState {
                file: FileEngine::new(events.clone(), generation, 62),
                generation,
                reducer,
                volume: 62,
            }),
            backend: tokio::sync::Mutex::new(PlayerBackend::Connect(ConnectBackend::new(
                events.clone(),
                generation,
            ))),
            events,
            receiver: Mutex::new(Some(receiver)),
            cache_dir,
            audio: Mutex::new(audio),
            local_requested: AtomicBool::new(false),
            local_active: AtomicBool::new(false),
            backend_intent: AtomicU64::new(0),
            play_intent: AtomicU64::new(0),
        }
    }

    pub fn set_requested_backend(&self, backend: PlaybackBackend) -> u64 {
        self.begin_play_intent();
        let local = match backend {
            PlaybackBackend::Connect => false,
            PlaybackBackend::Local => true,
        };
        self.local_requested.store(local, Ordering::Relaxed);
        self.backend_intent
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1)
    }

    pub(crate) fn begin_play_intent(&self) -> u64 {
        self.play_intent
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1)
    }

    fn current_play_intent(&self) -> u64 {
        self.play_intent.load(Ordering::Acquire)
    }

    fn is_current_play_intent(&self, intent: u64) -> bool {
        self.current_play_intent() == intent
    }

    fn local_requested(&self) -> bool {
        self.local_requested.load(Ordering::Relaxed)
    }

    pub fn set_audio(&self, audio: AudioSettings) {
        *self.audio.lock().expect("audio settings mutex poisoned") = audio;
    }

    pub async fn set_play_threshold_percent(&self, percent: u8) {
        self.state
            .lock()
            .await
            .reducer
            .set_play_threshold_percent(percent);
    }

    pub async fn exclude_track(&self, id: u64) {
        let mut backend = self.backend.lock().await;
        let mut state = self.state.lock().await;
        if !state
            .reducer
            .snapshot_mut()
            .is_some_and(|snapshot| snapshot.exclude(id))
        {
            return;
        }
        let snapshot = state.reducer.snapshot().cloned();
        let repeat = state.reducer.repeat();
        drop(state);
        backend.set_shuffle_snapshot(snapshot, repeat).await;
    }

    pub fn listen(
        self: &Arc<Self>,
        resolve_provider: impl Fn() -> Result<Arc<LiveClient>, String> + Send + Sync + 'static,
        on_effect: impl Fn(PlaybackEffect) + Send + Sync + 'static,
    ) -> tauri::async_runtime::JoinHandle<()> {
        let mut receiver = self
            .receiver
            .lock()
            .expect("playback receiver mutex poisoned")
            .take()
            .expect("playback event loop starts once");
        let playback = Arc::clone(self);
        let resolve_provider: Arc<ProviderResolver> = Arc::new(resolve_provider);
        let on_effect: Arc<EffectSink> = Arc::new(on_effect);
        tauri::async_runtime::spawn(async move {
            while let Some(event) = receiver.recv().await {
                playback
                    .handle_event(event, &resolve_provider, &on_effect)
                    .await;
            }
        })
    }

    #[cfg(test)]
    pub async fn play(
        &self,
        client: Option<Arc<LiveClient>>,
        tracks: Vec<SnapshotTrack>,
        index: usize,
    ) -> Result<PlayOutcome, String> {
        let intent = self.begin_play_intent();
        self.play_for_intent(client, tracks, index, intent).await
    }

    pub(crate) async fn play_for_intent(
        &self,
        client: Option<Arc<LiveClient>>,
        tracks: Vec<SnapshotTrack>,
        index: usize,
        intent: u64,
    ) -> Result<PlayOutcome, String> {
        let target_track_id = tracks.get(index).map(|track| track.id).unwrap_or(0);
        let target_track_uri = tracks
            .get(index)
            .map(|track| track.uri.clone())
            .unwrap_or_default();
        match self
            .play_with(client, tracks, index, intent, |suffix| {
                suffix.shuffle(&mut rand::rng())
            })
            .await
        {
            Ok(outcome) => Ok(outcome),
            Err(error @ PlaybackError::AuthorizationRequired { .. }) => {
                Ok(PlayOutcome::PlaybackAuthorizationRequired(
                    error
                        .into_prompt(target_track_id, &target_track_uri, intent)
                        .expect("authorization errors produce prompts"),
                ))
            }
            Err(error) => Err(error.into_string()),
        }
    }

    async fn play_with(
        &self,
        client: Option<Arc<LiveClient>>,
        tracks: Vec<SnapshotTrack>,
        index: usize,
        intent: u64,
        permute: impl FnOnce(&mut [usize]),
    ) -> Result<PlayOutcome, PlaybackError> {
        if tracks.is_empty() || index >= tracks.len() {
            return Err("Choose a track to play".into());
        }
        if !self.is_current_play_intent(intent) {
            return Ok(PlayOutcome::Started);
        }
        if reject_chapter(&tracks[index].uri) {
            let generation = self.state.lock().await.generation;
            let _ = self.events.send(NeutralEvent::Error {
                generation,
                message: AUDIOBOOK_ERROR.into(),
            });
            return Ok(PlayOutcome::Started);
        }
        let mut backend = self.backend.lock().await;
        let shuffle = self.state.lock().await.reducer.shuffle();
        let snapshot = Snapshot::new_with(tracks, index, shuffle, permute);
        if self.local_requested() && !is_file_uri(&snapshot.current().uri) {
            let client = require_spotify(client.as_deref())?;
            if let Err(error) = self.ensure_local_backend(&mut backend, client).await {
                log_authorization_required("load", &snapshot.current().uri, &error);
                return Err(error.with_target(snapshot.current().id, &snapshot.current().uri));
            }
        }
        if !self.is_current_play_intent(intent) {
            return Ok(PlayOutcome::Started);
        }
        self.state.lock().await.reducer.set_snapshot(Some(snapshot));
        self.load_current_locked(&mut backend, client, true, 0, intent)
            .await
            .map(|_| PlayOutcome::Started)
    }

    pub async fn toggle(&self, client: Option<&LiveClient>) -> Result<(), String> {
        let mut backend = self.backend.lock().await;
        let mut state = self.state.lock().await;
        if state.file.is_active() {
            return state.file.toggle();
        }
        if state.reducer.snapshot().is_none() {
            return Ok(());
        }
        let client = require_spotify(client)?;
        drop(state);
        self.ensure_player(&mut backend, client).await?;
        backend.toggle(client).await
    }

    pub async fn set_playing(
        &self,
        client: Option<&LiveClient>,
        playing: bool,
    ) -> Result<(), String> {
        let mut backend = self.backend.lock().await;
        let mut state = self.state.lock().await;
        if state.file.is_active() {
            return state.file.set_playing(playing);
        }
        if state.reducer.snapshot().is_none() {
            return Ok(());
        }
        let client = require_spotify(client)?;
        drop(state);
        self.ensure_player(&mut backend, client).await?;
        backend.set_playing(client, playing).await
    }

    pub async fn next(&self, client: Option<Arc<LiveClient>>) -> Result<PlayOutcome, String> {
        let intent = self.begin_play_intent();
        self.step_for_intent(client, 1, intent)
            .await
            .map(|_| PlayOutcome::Started)
            .or_else(|error| play_outcome_from_error(error, intent))
    }

    pub async fn prev(&self, client: Option<Arc<LiveClient>>) -> Result<PlayOutcome, String> {
        let intent = self.begin_play_intent();
        self.step_for_intent(client, -1, intent)
            .await
            .map(|_| PlayOutcome::Started)
            .or_else(|error| play_outcome_from_error(error, intent))
    }

    #[cfg(test)]
    async fn step(
        &self,
        client: Option<Arc<LiveClient>>,
        direction: i8,
    ) -> Result<(), PlaybackError> {
        let intent = self.begin_play_intent();
        self.step_for_intent(client, direction, intent).await
    }

    pub(crate) async fn step_for_intent(
        &self,
        client: Option<Arc<LiveClient>>,
        direction: i8,
        intent: u64,
    ) -> Result<(), PlaybackError> {
        let mut backend = self.backend.lock().await;
        self.step_locked(&mut backend, client, direction, intent)
            .await
    }

    pub async fn seek(&self, client: Option<&LiveClient>, seconds: u64) -> Result<(), String> {
        let mut backend = self.backend.lock().await;
        let state = self.state.lock().await;
        if state.file.is_active() {
            return state.file.seek(seconds);
        }
        if state.reducer.snapshot().is_none() {
            return Ok(());
        }
        let client = require_spotify(client)?;
        drop(state);
        self.ensure_player(&mut backend, client).await?;
        backend.seek(client, seconds).await
    }

    pub async fn set_volume(&self, client: Option<&LiveClient>, volume: u8) -> Result<(), String> {
        if volume > 100 {
            return Err("volume must be between 0 and 100".into());
        }
        let mut backend = self.backend.lock().await;
        let mut state = self.state.lock().await;
        if state.file.is_active() {
            state.file.set_volume(volume);
            state.volume = volume;
            return Ok(());
        }
        let client = require_spotify(client)?;
        let generation = state.generation;
        drop(state);
        self.ensure_player(&mut backend, client).await?;
        backend.set_volume(client, volume).await?;
        self.commit_if_generation(generation, |state| state.volume = volume)
            .await;
        Ok(())
    }

    pub async fn set_repeat(
        &self,
        client: Option<&LiveClient>,
        repeat: RepeatMode,
    ) -> Result<(), String> {
        let mut backend = self.backend.lock().await;
        let generation = self.state.lock().await.generation;
        backend.set_repeat(client, repeat).await?;
        self.commit_if_generation(generation, |state| state.reducer.set_repeat(repeat))
            .await;
        Ok(())
    }

    pub async fn set_shuffle(&self, shuffle: bool) -> PlayerStateEvent {
        self.set_shuffle_with(shuffle, |suffix| suffix.shuffle(&mut rand::rng()))
            .await
    }

    async fn set_shuffle_with(
        &self,
        shuffle: bool,
        permute: impl FnOnce(&mut [usize]),
    ) -> PlayerStateEvent {
        let mut backend = self.backend.lock().await;
        let mut state = self.state.lock().await;
        state.reducer.set_shuffle_with(shuffle, permute);
        let snapshot = state.reducer.snapshot().cloned();
        let repeat = state.reducer.repeat();
        let event = state.reducer.state().clone();
        drop(state);
        backend.set_shuffle_snapshot(snapshot, repeat).await;
        event
    }

    pub async fn stop(&self, client: Option<&LiveClient>) -> Result<(), String> {
        self.begin_play_intent();
        let mut backend = self.backend.lock().await;
        let mut state = self.state.lock().await;
        if state.file.is_active() {
            state.file.stop();
        }
        let generation = state.generation;
        drop(state);
        backend.stop(client).await?;
        self.commit_if_generation(generation, |state| state.reducer.set_snapshot(None))
            .await;
        Ok(())
    }

    pub(crate) async fn stop_for_authorization(
        &self,
        client: Option<&LiveClient>,
        prompt: PlaybackAuthorizationPrompt,
    ) -> Vec<PlaybackEffect> {
        let mut backend = self.backend.lock().await;
        self.authorization_effects(&mut backend, client, prompt)
            .await
    }

    pub async fn switch_to_local(&self, client: &LiveClient, volume: u8) -> Result<(), String> {
        let intent = self.set_requested_backend(PlaybackBackend::Local);
        let audio = *self.audio.lock().expect("audio settings mutex poisoned");
        self.switch_to_local_with(Some(client), intent, || async {
            let state = self.state.lock().await;
            let generation = state.generation.wrapping_add(1);
            drop(state);
            LocalBackend::activate(
                client,
                self.events.clone(),
                generation,
                volume,
                self.cache_dir.as_deref(),
                audio,
            )
            .await
            .map_err(PlaybackError::into_string)
        })
        .await
    }

    async fn switch_to_local_with<F, Fut>(
        &self,
        client: Option<&LiveClient>,
        intent: u64,
        prepare: F,
    ) -> Result<(), String>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<LocalBackend, String>>,
    {
        let mut local = prepare().await?;
        let volume = local.volume();
        let mut backend = self.backend.lock().await;
        let state = self.state.lock().await;
        if self.backend_intent.load(Ordering::Acquire) != intent {
            local.teardown();
            return Ok(());
        }
        drop(state);
        if let PlayerBackend::Connect(connect) = &mut *backend {
            connect.stop(client).await?;
        }
        let mut state = self.state.lock().await;
        if self.backend_intent.load(Ordering::Acquire) != intent {
            local.teardown();
            return Ok(());
        }
        state.generation = local.generation();
        let generation = state.generation;
        state.file.set_generation(generation);
        state.reducer.activate(generation);
        *backend = PlayerBackend::Local(local);
        self.local_active.store(true, Ordering::Release);
        state.volume = volume;
        Ok(())
    }

    pub async fn switch_to_connect(&self) {
        self.set_requested_backend(PlaybackBackend::Connect);
        let generation = {
            let mut state = self.state.lock().await;
            state.generation = state.generation.wrapping_add(1);
            let generation = state.generation;
            state.file.set_generation(generation);
            state.reducer.activate(generation);
            generation
        };
        let mut backend = self.backend.lock().await;
        if let PlayerBackend::Local(local) = &mut *backend {
            local.teardown();
        }
        *backend = PlayerBackend::Connect(ConnectBackend::new(self.events.clone(), generation));
        self.local_active.store(false, Ordering::Release);
    }

    pub async fn invalidate_local(&self) {
        if self.local_active.load(Ordering::Acquire) {
            let mut state = self.state.lock().await;
            state.generation = state.generation.wrapping_add(1);
            let generation = state.generation;
            state.file.set_generation(generation);
            state.reducer.activate(generation);
            drop(state);
            let mut backend = self.backend.lock().await;
            if let PlayerBackend::Local(local) = &mut *backend {
                local.teardown();
            }
        }
    }

    /// Whether the LOCAL backend is what commands currently dispatch to —
    /// can differ from the persisted setting when activation failed and
    /// playback fell back to Connect.
    pub async fn is_local_active(&self) -> bool {
        self.local_active.load(Ordering::Acquire)
    }

    /// Recreate an invalidated local player now rather than lazily on the
    /// next command, so playback resumes on its own after a config change.
    pub async fn revalidate(&self, client: &LiveClient) -> Result<(), String> {
        let mut backend = self.backend.lock().await;
        if !backend.is_local() || self.state.lock().await.reducer.snapshot().is_none() {
            return Ok(());
        }
        self.ensure_player(&mut backend, client)
            .await
            .map_err(PlaybackError::into_string)
    }

    async fn ensure_local_backend(
        &self,
        backend: &mut PlayerBackend,
        client: &LiveClient,
    ) -> Result<(), PlaybackError> {
        if backend.is_local() {
            self.ensure_session(backend, client).await?;
            return Ok(());
        }

        let (previous_generation, generation, volume) = {
            let state = self.state.lock().await;
            (
                state.generation,
                state.generation.wrapping_add(1),
                state.volume,
            )
        };
        let audio = *self.audio.lock().expect("audio settings mutex poisoned");
        let local = LocalBackend::activate(
            client,
            self.events.clone(),
            generation,
            volume,
            self.cache_dir.as_deref(),
            audio,
        )
        .await?;
        if let PlayerBackend::Connect(connect) = backend {
            connect.stop(Some(client)).await?;
        }
        let mut state = self.state.lock().await;
        if state.generation != previous_generation || !self.local_requested() {
            return Ok(());
        }
        state.generation = generation;
        state.file.set_generation(generation);
        state.reducer.activate(generation);
        state.volume = local.volume();
        *backend = PlayerBackend::Local(local);
        self.local_active.store(true, Ordering::Release);
        Ok(())
    }

    async fn ensure_player(
        &self,
        backend: &mut PlayerBackend,
        client: &LiveClient,
    ) -> Result<(), PlaybackError> {
        let invalid = matches!(backend, PlayerBackend::Local(local) if local.player_is_invalid());
        if !invalid {
            return Ok(());
        }
        log::info!("Recreating local playback player");
        let (previous_generation, generation, volume, restore) = {
            let state = self.state.lock().await;
            let restore = state.reducer.snapshot().cloned().map(|snapshot| {
                let playing = state.reducer.state().is_playing;
                let position_ms = state.reducer.position_ms();
                (snapshot, playing, position_ms)
            });
            (
                state.generation,
                state.generation.wrapping_add(1),
                state.volume,
                restore,
            )
        };
        let audio = *self.audio.lock().expect("audio settings mutex poisoned");
        let mut local = LocalBackend::activate(
            client,
            self.events.clone(),
            generation,
            volume,
            self.cache_dir.as_deref(),
            audio,
        )
        .await?;
        let mut state = self.state.lock().await;
        if state.generation != previous_generation {
            local.teardown();
            return Ok(());
        }
        state.generation = generation;
        state.file.set_generation(generation);
        state.reducer.activate(generation);
        if let Some((snapshot, playing, position_ms)) = restore {
            state.reducer.queue_load(&snapshot.current().uri, playing);
            local.play(snapshot, playing, position_ms)?;
        }
        *backend = PlayerBackend::Local(local);
        self.local_active.store(true, Ordering::Release);
        Ok(())
    }

    async fn ensure_session(
        &self,
        backend: &mut PlayerBackend,
        client: &LiveClient,
    ) -> Result<(), PlaybackError> {
        self.ensure_player(backend, client).await?;
        let invalid = matches!(backend, PlayerBackend::Local(local) if local.session_is_invalid());
        if !invalid {
            if let PlayerBackend::Local(local) = backend {
                local.preflight(client).await?;
            }
            return Ok(());
        }
        log::info!(
            "Refreshing Spotify control session; active player preserved generation={}",
            self.state.lock().await.generation
        );
        if let PlayerBackend::Local(local) = backend {
            local.refresh_session(client).await?;
        }
        log::info!(
            "Spotify control session replaced; active player preserved generation={}",
            self.state.lock().await.generation
        );
        if let PlayerBackend::Local(local) = backend {
            local.preflight(client).await?;
        }
        Ok(())
    }

    /// Outcome of one reconnect attempt. Superseded means a newer
    /// generation exists or there is nothing to resume — stop retrying.
    async fn try_reconnect(
        &self,
        client: &LiveClient,
        generation: u64,
    ) -> Result<bool, PlaybackError> {
        let mut backend = self.backend.lock().await;
        let state = self.state.lock().await;
        if state.generation != generation
            || !backend.is_local()
            || state.reducer.snapshot().is_none()
        {
            return Ok(false);
        }
        let target = state.reducer.snapshot().unwrap().current();
        let target_track_id = target.id;
        let target_track_uri = target.uri.clone();
        drop(state);
        self.ensure_player(&mut backend, client)
            .await
            .map_err(|error| error.with_target(target_track_id, &target_track_uri))?;
        Ok(true)
    }

    async fn step_locked(
        &self,
        backend: &mut PlayerBackend,
        client: Option<Arc<LiveClient>>,
        direction: i8,
        intent: u64,
    ) -> Result<(), PlaybackError> {
        if !self.is_current_play_intent(intent) {
            return Ok(());
        }
        let mut state = self.state.lock().await;
        let wrap = direction > 0 && state.reducer.repeat().wraps();
        let Some(snapshot) = state.reducer.snapshot() else {
            return Ok(());
        };
        let current_track_id = snapshot.current().id;
        let current_uri = snapshot.current().uri.clone();
        let Some(next) = step_index(snapshot.index, snapshot.len(), direction, wrap) else {
            if state.file.is_active() {
                state.file.stop();
                return Ok(());
            }
            if self.local_requested() {
                drop(state);
                return Ok(backend.stop(client.as_deref()).await?);
            }
            let client = require_spotify(client.as_deref())?;
            drop(state);
            self.ensure_player(backend, client)
                .await
                .map_err(|error| error.with_target(current_track_id, &current_uri))?;
            return backend
                .step(client, direction)
                .await
                .map_err(PlaybackError::from)
                .map_err(|error| error.with_target(current_track_id, &current_uri));
        };
        if reject_chapter(&snapshot.track_at(next).uri) {
            return Err(AUDIOBOOK_ERROR.into());
        }
        let next_track_id = snapshot.track_at(next).id;
        let next_uri = snapshot.track_at(next).uri.clone();
        if self.local_requested() && !is_file_uri(&next_uri) {
            let client = require_spotify(client.as_deref())?;
            drop(state);
            if let Err(error) = self.ensure_local_backend(backend, client).await {
                log_authorization_required("advance", &next_uri, &error);
                return Err(error.with_target(next_track_id, &next_uri));
            }
            state = self.state.lock().await;
        }
        if !self.is_current_play_intent(intent) {
            return Ok(());
        }
        state.reducer.snapshot_mut().unwrap().index = next;
        drop(state);
        self.load_current_locked(backend, client, true, 0, intent)
            .await
    }

    async fn load_current_locked(
        &self,
        backend: &mut PlayerBackend,
        client: Option<Arc<LiveClient>>,
        playing: bool,
        position_ms: u32,
        intent: u64,
    ) -> Result<(), PlaybackError> {
        if !self.is_current_play_intent(intent) {
            return Ok(());
        }
        let state = self.state.lock().await;
        let snapshot = state
            .reducer
            .snapshot()
            .cloned()
            .ok_or("Nothing is playing")?;
        let uri = snapshot.current().uri.clone();
        let target_track_id = snapshot.current().id;
        let generation = state.generation;
        drop(state);
        if is_file_uri(&uri) {
            backend.stop(client.as_deref()).await?;
            let mut state = self.state.lock().await;
            if state.generation != generation
                || !self.is_current_play_intent(intent)
                || state
                    .reducer
                    .snapshot()
                    .is_none_or(|snapshot| snapshot.current().uri != uri)
            {
                return Ok(());
            }
            state.reducer.queue_load(&uri, playing);
            return Ok(state.file.load(&uri, playing, position_ms)?);
        }

        self.state.lock().await.file.stop_silently();
        let client = client.ok_or_else(missing_spotify)?;
        if self.local_requested() {
            if let Err(error) = self.ensure_local_backend(backend, client.as_ref()).await {
                log_authorization_required("load", &uri, &error);
                return Err(error.with_target(target_track_id, &uri));
            }
        } else {
            self.ensure_session(backend, client.as_ref())
                .await
                .map_err(|error| error.with_target(target_track_id, &uri))?;
        }
        let mut state = self.state.lock().await;
        if state.generation != generation
            || !self.is_current_play_intent(intent)
            || state
                .reducer
                .snapshot()
                .is_none_or(|snapshot| snapshot.current().uri != uri)
        {
            return Ok(());
        }
        state.reducer.queue_load(&uri, playing);
        let repeat = state.reducer.repeat();
        drop(state);
        backend
            .play(client, snapshot, repeat)
            .await
            .map_err(PlaybackError::from)
            .map_err(|error| error.with_target(target_track_id, &uri))
    }

    async fn authorization_effects(
        &self,
        backend: &mut PlayerBackend,
        client: Option<&LiveClient>,
        prompt: PlaybackAuthorizationPrompt,
    ) -> Vec<PlaybackEffect> {
        if !self.is_current_play_intent(prompt.intent) {
            return Vec::new();
        }
        let mut state = self.state.lock().await;
        if state.file.is_active() {
            state.file.stop();
        }
        let generation = state.generation;
        drop(state);
        if let Err(error) = backend.stop(client).await {
            log::warn!("Could not stop playback after authorization rejection: {error}");
        }
        let mut state = self.state.lock().await;
        if state.generation != generation || !self.is_current_play_intent(prompt.intent) {
            return Vec::new();
        }
        let shuffle = state.reducer.state().shuffle;
        state.reducer.set_snapshot(None);
        vec![
            PlaybackEffect::PlayerState(empty_event(false, shuffle)),
            PlaybackEffect::ConnectionRefresh,
            PlaybackEffect::AuthorizationRequired(prompt),
        ]
    }

    async fn handle_event(
        self: &Arc<Self>,
        event: NeutralEvent,
        resolve_provider: &Arc<ProviderResolver>,
        on_effect: &Arc<EffectSink>,
    ) {
        let (actions, facts) = {
            let mut state = self.state.lock().await;
            let actions = state.reducer.handle(event);
            let facts = state.reducer.take_listening_facts();
            (actions, facts)
        };
        for fact in facts {
            on_effect(PlaybackEffect::Listening(fact));
        }
        for action in actions {
            let action = match immediate_effect(action) {
                ImmediateAction::Effect(effect) => {
                    on_effect(effect);
                    continue;
                }
                ImmediateAction::Deferred(action) => action,
            };
            match action {
                ReducerAction::PreloadNext => {
                    let (candidate, generation) = {
                        let state = self.state.lock().await;
                        let repeat = state.reducer.repeat();
                        (
                            state.reducer.snapshot().and_then(|snapshot| {
                                preload_track(snapshot, repeat).map(|track| {
                                    (snapshot.current().uri.clone(), track.uri.clone())
                                })
                            }),
                            state.generation,
                        )
                    };
                    let Some((current, uri)) = candidate else {
                        continue;
                    };
                    if is_file_uri(&uri) || reject_chapter(&uri) {
                        continue;
                    }
                    log::info!("Preload suggested: current={current} next={uri}");
                    let client = resolve_provider().ok();
                    let mut backend = self.backend.lock().await;
                    let result = async {
                        let client = require_spotify(client.as_deref())?;
                        if self.local_requested() {
                            self.ensure_local_backend(&mut backend, client).await?;
                        } else {
                            self.ensure_session(&mut backend, client).await?;
                        }
                        if self.state.lock().await.generation != generation {
                            return Ok(false);
                        }
                        Ok(backend.preload(&uri)?)
                    }
                    .await;
                    match result {
                        Ok(true) => log::info!("Preload requested: {uri}"),
                        Ok(false) => log::debug!("Preload ignored: {uri}"),
                        Err(error @ PlaybackError::AuthorizationRequired { .. }) => {
                            log::warn!(
                                "Spotify playback authorization failed during speculative preload; current track continues: {error}"
                            );
                            on_effect(PlaybackEffect::ConnectionRefresh);
                        }
                        Err(error) => log::warn!("Unable to preload {uri}: {error}"),
                    }
                }
                ReducerAction::Advance => {
                    let client = resolve_provider().ok();
                    let intent = self.current_play_intent();
                    if let Err(error) = self.step_for_intent(client, 1, intent).await {
                        match error {
                            error @ PlaybackError::AuthorizationRequired { .. } => {
                                let prompt = error
                                    .into_prompt(0, "", intent)
                                    .expect("authorization errors produce prompts");
                                let client = resolve_provider().ok();
                                let mut backend = self.backend.lock().await;
                                for effect in self
                                    .authorization_effects(&mut backend, client.as_deref(), prompt)
                                    .await
                                {
                                    on_effect(effect);
                                }
                            }
                            error => {
                                on_effect(PlaybackEffect::OperationError(error.into_string()));
                            }
                        }
                    }
                }
                ReducerAction::Reload => {
                    let client = resolve_provider().ok();
                    let intent = self.current_play_intent();
                    let mut backend = self.backend.lock().await;
                    let result = self
                        .load_current_locked(&mut backend, client, true, 0, intent)
                        .await;
                    if let Err(error) = result {
                        match error {
                            error @ PlaybackError::AuthorizationRequired { .. } => {
                                let prompt = error
                                    .into_prompt(0, "", intent)
                                    .expect("authorization errors produce prompts");
                                let client = resolve_provider().ok();
                                for effect in self
                                    .authorization_effects(&mut backend, client.as_deref(), prompt)
                                    .await
                                {
                                    on_effect(effect);
                                }
                            }
                            error => {
                                on_effect(PlaybackEffect::OperationError(error.into_string()));
                            }
                        }
                    }
                }
                ReducerAction::Invalidate => {
                    log::info!("Local playback player stopped; will recreate on next use");
                    let mut backend = self.backend.lock().await;
                    let mut state = self.state.lock().await;
                    state.generation = state.generation.wrapping_add(1);
                    let generation = state.generation;
                    state.file.set_generation(generation);
                    state.reducer.recover(generation);
                    if let PlayerBackend::Local(local) = &mut *backend {
                        local.teardown();
                    }
                }
                ReducerAction::Reconnect => {
                    let generation = self.state.lock().await.generation;
                    let playback = Arc::clone(self);
                    let resolve_provider = Arc::clone(resolve_provider);
                    let on_effect = Arc::clone(on_effect);
                    tokio::spawn(async move {
                        for (attempt, delay) in RECONNECT_DELAYS.iter().enumerate() {
                            if *delay > 0 {
                                tokio::time::sleep(Duration::from_secs(*delay)).await;
                            }
                            let client = match resolve_provider() {
                                Ok(client) => client,
                                Err(error) => {
                                    log::info!("Stopping playback reconnect: {error}");
                                    return;
                                }
                            };
                            match playback.try_reconnect(client.as_ref(), generation).await {
                                Ok(true) => {
                                    on_effect(PlaybackEffect::OperationRecovered);
                                    log::info!("Local playback session reconnected");
                                    return;
                                }
                                Ok(false) => {
                                    // A user action took over (play, stop, backend
                                    // switch) — the reconnect banner no longer
                                    // describes reality, so clear it.
                                    on_effect(PlaybackEffect::OperationRecovered);
                                    log::debug!("Playback reconnect superseded");
                                    return;
                                }
                                Err(error @ PlaybackError::AuthorizationRequired { .. }) => {
                                    let prompt = error
                                        .into_prompt(0, "", playback.current_play_intent())
                                        .expect("authorization errors produce prompts");
                                    let client = resolve_provider().ok();
                                    let mut backend = playback.backend.lock().await;
                                    for effect in playback
                                        .authorization_effects(
                                            &mut backend,
                                            client.as_deref(),
                                            prompt,
                                        )
                                        .await
                                    {
                                        on_effect(effect);
                                    }
                                    return;
                                }
                                Err(error) => {
                                    log::info!(
                                        "Playback reconnect attempt {} failed: {error}",
                                        attempt + 1
                                    );
                                    if attempt == 0 {
                                        on_effect(PlaybackEffect::OperationError(
                                            "Restarting built-in playback…".into(),
                                        ));
                                    }
                                }
                            }
                        }
                        on_effect(PlaybackEffect::OperationError(
                            "Built-in playback stopped unexpectedly.".into(),
                        ));
                    });
                }
                ReducerAction::Emit(_)
                | ReducerAction::Error(_)
                | ReducerAction::TrackCompleted(_) => {
                    unreachable!("immediate effects are consumed")
                }
            }
        }
    }
}

enum ImmediateAction {
    Effect(PlaybackEffect),
    Deferred(ReducerAction),
}

fn immediate_effect(action: ReducerAction) -> ImmediateAction {
    match action {
        ReducerAction::Emit(event) => ImmediateAction::Effect(PlaybackEffect::PlayerState(event)),
        ReducerAction::Error(error) => {
            ImmediateAction::Effect(PlaybackEffect::OperationError(error))
        }
        ReducerAction::TrackCompleted(uri) => {
            ImmediateAction::Effect(PlaybackEffect::TrackCompleted(uri))
        }
        action => ImmediateAction::Deferred(action),
    }
}

fn play_outcome_from_error(error: PlaybackError, intent: u64) -> Result<PlayOutcome, String> {
    match error {
        error @ PlaybackError::AuthorizationRequired { .. } => {
            Ok(PlayOutcome::PlaybackAuthorizationRequired(
                error
                    .into_prompt(0, "", intent)
                    .expect("authorization errors produce prompts"),
            ))
        }
        error => Err(error.into_string()),
    }
}

fn log_authorization_required(operation: &str, uri: &str, error: &PlaybackError) {
    if let PlaybackError::AuthorizationRequired { reason, .. } = error {
        log::warn!(
            "Spotify playback authorization required operation={operation} uri={uri} reason={reason:?}"
        );
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

fn preload_track(snapshot: &Snapshot, repeat: RepeatMode) -> Option<&SnapshotTrack> {
    let wrap = match repeat {
        RepeatMode::Off => false,
        RepeatMode::All => true,
        RepeatMode::One => return None,
    };
    let next = step_index(snapshot.index, snapshot.len(), 1, wrap)?;
    Some(snapshot.track_at(next))
}

fn reject_chapter(uri: &str) -> bool {
    uri.starts_with("spotify:chapter:")
}

fn is_file_uri(uri: &str) -> bool {
    uri.starts_with("file:")
}

fn require_spotify(client: Option<&LiveClient>) -> Result<&LiveClient, String> {
    client.ok_or_else(missing_spotify)
}

fn missing_spotify() -> String {
    "Spotify Client ID is missing. Add it in Preferences, then try again.".into()
}

fn local_event(
    track: &SnapshotTrack,
    elapsed: u64,
    is_playing: bool,
    volume_supported: bool,
    shuffle: bool,
) -> PlayerStateEvent {
    PlayerStateEvent {
        track_id: Some(track.id),
        uri: Some(track.uri.clone()),
        elapsed,
        is_playing,
        external: false,
        name: Some(track.name.clone()),
        art: Some(track.art.clone()),
        alb: Some(track.alb.clone()),
        duration_secs: Some(track.duration_secs),
        volume_supported,
        shuffle,
    }
}

pub fn empty_event(external: bool, shuffle: bool) -> PlayerStateEvent {
    PlayerStateEvent {
        track_id: None,
        uri: None,
        elapsed: 0,
        is_playing: false,
        external,
        name: None,
        art: None,
        alb: None,
        duration_secs: None,
        volume_supported: false,
        shuffle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use retune_spotify::tokens::{CachedTokenStore, InMemoryTokenStore, TokenStore};

    #[test]
    fn listener_starts_without_a_current_tokio_runtime() {
        assert!(tokio::runtime::Handle::try_current().is_err());
        let playback = Arc::new(Playback::default());
        let listener = playback.listen(|| Err("offline".into()), |_| {});

        listener.abort();
    }

    async fn delayed_backend_effect(
        playback: Arc<Playback>,
        id: u8,
        entered: mpsc::UnboundedSender<u8>,
        release: Arc<tokio::sync::Notify>,
        generation: u64,
        volume: u8,
    ) {
        let _backend = playback.backend.lock().await;
        entered.send(id).unwrap();
        release.notified().await;
        playback
            .commit_if_generation(generation, |state| state.volume = volume)
            .await;
    }

    #[tokio::test]
    async fn delayed_backend_effects_are_ordered_without_blocking_controller_state() {
        let playback = Arc::new(Playback::default());
        let generation = playback.state.lock().await.generation;
        let (entered, mut entries) = mpsc::unbounded_channel();
        let first_release = Arc::new(tokio::sync::Notify::new());
        let second_release = Arc::new(tokio::sync::Notify::new());
        let first = tokio::spawn(delayed_backend_effect(
            Arc::clone(&playback),
            1,
            entered.clone(),
            Arc::clone(&first_release),
            generation,
            25,
        ));
        assert_eq!(entries.recv().await, Some(1));
        let second = tokio::spawn(delayed_backend_effect(
            Arc::clone(&playback),
            2,
            entered,
            Arc::clone(&second_release),
            generation,
            50,
        ));

        let _ = tokio::time::timeout(Duration::from_millis(50), playback.state.lock())
            .await
            .expect("controller reads must not wait for a remote effect");
        assert!(
            entries.try_recv().is_err(),
            "remote effects must stay ordered"
        );

        playback.state.lock().await.generation = generation.wrapping_add(1);
        first_release.notify_one();
        assert_eq!(entries.recv().await, Some(2));
        second_release.notify_one();
        first.await.unwrap();
        second.await.unwrap();

        let state = playback.state.lock().await;
        assert_eq!(state.volume, 62, "stale effects must not commit");
    }

    #[test]
    fn immediate_reducer_actions_map_to_tauri_free_effects() {
        let event = empty_event(false, true);

        assert!(matches!(
            immediate_effect(ReducerAction::Emit(event.clone())),
            ImmediateAction::Effect(PlaybackEffect::PlayerState(mapped)) if mapped == event
        ));
        assert!(matches!(
            immediate_effect(ReducerAction::Error("offline".into())),
            ImmediateAction::Effect(PlaybackEffect::OperationError(error)) if error == "offline"
        ));
        assert!(matches!(
            immediate_effect(ReducerAction::TrackCompleted("spotify:track:one".into())),
            ImmediateAction::Effect(PlaybackEffect::TrackCompleted(uri)) if uri == "spotify:track:one"
        ));
        assert!(matches!(
            immediate_effect(ReducerAction::Advance),
            ImmediateAction::Deferred(ReducerAction::Advance)
        ));
    }

    #[test]
    fn playback_setting_enums_use_exact_lowercase_wire_values() {
        for (backend, wire) in [
            (PlaybackBackend::Connect, "connect"),
            (PlaybackBackend::Local, "local"),
        ] {
            assert_eq!(
                serde_json::to_string(&backend).unwrap(),
                format!("\"{wire}\"")
            );
            assert_eq!(
                serde_json::from_str::<PlaybackBackend>(&format!("\"{wire}\"")).unwrap(),
                backend
            );
        }
        for (repeat, wire) in [
            (RepeatMode::Off, "off"),
            (RepeatMode::All, "all"),
            (RepeatMode::One, "one"),
        ] {
            assert_eq!(
                serde_json::to_string(&repeat).unwrap(),
                format!("\"{wire}\"")
            );
            assert_eq!(
                serde_json::from_str::<RepeatMode>(&format!("\"{wire}\"")).unwrap(),
                repeat
            );
        }
        assert!(!RepeatMode::Off.wraps());
        assert!(RepeatMode::All.wraps());
        assert!(!RepeatMode::One.wraps());
    }

    fn mixed_tracks() -> Vec<SnapshotTrack> {
        vec![
            SnapshotTrack {
                id: 1,
                uri: "file:///definitely/missing/one.mp3".into(),
                name: "File".into(),
                art: "Artist".into(),
                alb: "Album".into(),
                duration_secs: 60,
            },
            SnapshotTrack {
                id: 2,
                uri: "spotify:track:two".into(),
                name: "Spotify".into(),
                art: "Artist".into(),
                alb: "Album".into(),
                duration_secs: 60,
            },
        ]
    }

    fn file_tracks(count: u64) -> Vec<SnapshotTrack> {
        (1..=count)
            .map(|id| SnapshotTrack {
                id,
                uri: format!("file:///definitely/missing/{id}.mp3"),
                name: id.to_string(),
                art: String::new(),
                alb: String::new(),
                duration_secs: 60,
            })
            .collect()
    }

    fn client_without_playback_credentials() -> Arc<LiveClient> {
        let tokens: Box<dyn TokenStore> = Box::new(InMemoryTokenStore::new(None));
        Arc::new(SpotifyClient::new(
            "test",
            HttpTransport::new(),
            Arc::new(CachedTokenStore::new(tokens)),
        ))
    }

    #[test]
    fn shuffle_only_permutes_future_and_restores_duplicate_occurrence() {
        let duplicate = SnapshotTrack {
            id: 1,
            uri: "file:///duplicate.mp3".into(),
            name: "Duplicate".into(),
            art: "Artist".into(),
            alb: "Album".into(),
            duration_secs: 60,
        };
        let mut snapshot = Snapshot::new(
            vec![
                duplicate.clone(),
                SnapshotTrack {
                    id: 2,
                    ..duplicate.clone()
                },
                duplicate.clone(),
                SnapshotTrack { id: 3, ..duplicate },
            ],
            0,
        );

        snapshot.set_shuffle_with(true, |suffix| suffix.reverse());
        assert_eq!(snapshot.order, [0, 3, 2, 1]);
        snapshot.index = 2;
        assert_eq!(snapshot.current().id, 1);

        snapshot.set_shuffle_with(false, |_| unreachable!());
        assert_eq!(snapshot.order, [0, 1, 2, 3]);
        assert_eq!(snapshot.index, 2);
        assert_eq!(snapshot.current().id, 1);
    }

    #[test]
    fn deterministic_shuffle_preserves_history_current_and_multiset() {
        let tracks = (1..=5)
            .map(|id| SnapshotTrack {
                id,
                uri: format!("file:///{id}.mp3"),
                name: id.to_string(),
                art: String::new(),
                alb: String::new(),
                duration_secs: 60,
            })
            .collect();
        let mut snapshot = Snapshot::new(tracks, 1);

        snapshot.set_shuffle_with(true, |suffix| suffix.reverse());

        assert_eq!(snapshot.order, [0, 1, 4, 3, 2]);
        assert_eq!(snapshot.index, 1);
        assert_eq!(snapshot.current().id, 2);
        let mut ids = snapshot
            .active_tracks()
            .into_iter()
            .map(|track| track.id)
            .collect::<Vec<_>>();
        ids.sort_unstable();
        assert_eq!(ids, [1, 2, 3, 4, 5]);
    }

    #[test]
    fn excluding_a_track_keeps_the_current_instance_and_removes_the_rest() {
        let mut tracks = file_tracks(3);
        tracks.push(tracks[1].clone());
        let mut snapshot = Snapshot::new(tracks, 1);

        assert!(snapshot.exclude(2));
        assert_eq!(snapshot.current().id, 2);
        assert_eq!(
            snapshot
                .active_tracks()
                .into_iter()
                .map(|track| track.id)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert!(snapshot.exclude(1));
        assert_eq!(snapshot.current().id, 2);
        assert_eq!(snapshot.index, 0);
    }

    #[tokio::test]
    async fn toggling_shuffle_mid_song_does_not_load() {
        let playback = Playback::default();
        let mut tracks = mixed_tracks();
        tracks[1].uri = "file:///definitely/missing/two.mp3".into();
        playback.play(None, tracks, 0).await.unwrap();
        let before = playback.state.lock().await.file.request_id();

        playback
            .set_shuffle_with(true, |suffix| suffix.reverse())
            .await;

        let state = playback.state.lock().await;
        assert_eq!(state.file.request_id(), before);
        assert!(state.reducer.state().shuffle);
    }

    #[tokio::test]
    async fn enabled_shuffle_constructs_exact_queue_then_event_advance_and_prev_follow_it() {
        let playback = Playback::default();
        playback.set_shuffle_with(true, |_| unreachable!()).await;
        let intent = playback.begin_play_intent();
        playback
            .play_with(None, file_tracks(5), 1, intent, |suffix| suffix.reverse())
            .await
            .unwrap();
        let mut state = playback.state.lock().await;
        let snapshot = state.reducer.snapshot().unwrap();
        assert_eq!(snapshot.order, [0, 1, 4, 3, 2]);
        assert_eq!(snapshot.index, 1);
        assert_eq!(snapshot.current().id, 2);
        let generation = state.generation;
        assert!(state
            .reducer
            .handle(NeutralEvent::RequestIdChanged {
                generation,
                request_id: 99,
            })
            .is_empty());
        let actions = state.reducer.handle(NeutralEvent::EndOfTrack {
            generation,
            request_id: 99,
            uri: "file:///definitely/missing/2.mp3".into(),
        });
        assert!(matches!(
            actions.as_slice(),
            [ReducerAction::TrackCompleted(_), ReducerAction::Advance]
        ));

        drop(state);
        playback.step(None, 1).await.unwrap();
        assert_eq!(
            playback
                .state
                .lock()
                .await
                .reducer
                .snapshot()
                .unwrap()
                .current()
                .id,
            5
        );
        playback.step(None, -1).await.unwrap();
        assert_eq!(
            playback
                .state
                .lock()
                .await
                .reducer
                .snapshot()
                .unwrap()
                .current()
                .id,
            2
        );
    }

    #[tokio::test]
    async fn advance_follows_shuffle_and_disable_restores_canonical_current() {
        let playback = Playback::default();
        let tracks = (1..=4)
            .map(|id| SnapshotTrack {
                id,
                uri: format!("file:///definitely/missing/{id}.mp3"),
                name: id.to_string(),
                art: String::new(),
                alb: String::new(),
                duration_secs: 60,
            })
            .collect();
        playback.play(None, tracks, 1).await.unwrap();
        playback
            .set_shuffle_with(true, |suffix| suffix.reverse())
            .await;

        playback.next(None).await.unwrap();
        {
            let state = playback.state.lock().await;
            let snapshot = state.reducer.snapshot().unwrap();
            assert_eq!(snapshot.current().id, 4);
            assert_eq!(snapshot.order, [0, 1, 3, 2]);
        }

        playback.set_shuffle_with(false, |_| unreachable!()).await;
        let state = playback.state.lock().await;
        let snapshot = state.reducer.snapshot().unwrap();
        assert_eq!(snapshot.current().id, 4);
        assert_eq!(snapshot.index, 3);
        assert_eq!(snapshot.order, [0, 1, 2, 3]);
    }

    #[tokio::test]
    async fn repeat_all_wraps_shuffled_active_order_while_shuffle_stays_on() {
        let playback = Playback::default();
        let tracks = (1..=4)
            .map(|id| SnapshotTrack {
                id,
                uri: format!("file:///definitely/missing/{id}.mp3"),
                name: id.to_string(),
                art: String::new(),
                alb: String::new(),
                duration_secs: 60,
            })
            .collect();
        playback.play(None, tracks, 1).await.unwrap();
        playback
            .set_shuffle_with(true, |suffix| suffix.reverse())
            .await;
        playback.set_repeat(None, RepeatMode::All).await.unwrap();

        playback.next(None).await.unwrap();
        playback.next(None).await.unwrap();
        playback.next(None).await.unwrap();

        let state = playback.state.lock().await;
        let snapshot = state.reducer.snapshot().unwrap();
        assert!(state.reducer.state().shuffle);
        assert_eq!(snapshot.order, [0, 1, 3, 2]);
        assert_eq!(snapshot.index, 0);
        assert_eq!(snapshot.current().id, 1);
    }

    #[test]
    fn file_volume_uses_squared_percent_curve() {
        assert_eq!(file::sink_volume(0), 0.0);
        assert_eq!(file::sink_volume(50), 0.25);
        assert_eq!(file::sink_volume(100), 1.0);
        assert_eq!(file::sink_volume(200), 1.0);
    }

    #[tokio::test]
    async fn all_file_queue_controls_work_without_spotify() {
        let playback = Playback::default();
        let tracks = vec![
            SnapshotTrack {
                id: 1,
                uri: "file:///definitely/missing/one.mp3".into(),
                name: "One".into(),
                art: "Artist".into(),
                alb: "Album".into(),
                duration_secs: 60,
            },
            SnapshotTrack {
                id: 2,
                uri: "file:///definitely/missing/two.mp3".into(),
                name: "Two".into(),
                art: "Artist".into(),
                alb: "Album".into(),
                duration_secs: 60,
            },
        ];

        playback.play(None, tracks, 0).await.unwrap();
        playback.toggle(None).await.unwrap();
        playback.seek(None, 10).await.unwrap();
        playback.set_volume(None, 25).await.unwrap();
        playback.next(None).await.unwrap();
        assert_eq!(
            playback
                .state
                .lock()
                .await
                .reducer
                .snapshot()
                .unwrap()
                .index,
            1
        );
        playback.prev(None).await.unwrap();
        assert_eq!(
            playback
                .state
                .lock()
                .await
                .reducer
                .snapshot()
                .unwrap()
                .index,
            0
        );
        playback.stop(None).await.unwrap();
        let state = playback.state.lock().await;
        assert!(!state.file.is_active());
        assert!(state.reducer.snapshot().is_none());
    }

    #[tokio::test]
    async fn missing_playback_authorization_does_not_commit_a_spotify_queue() {
        let playback = Playback::default();
        playback.set_requested_backend(PlaybackBackend::Local);
        let track = mixed_tracks().pop().unwrap();
        let outcome = playback
            .play(Some(client_without_playback_credentials()), vec![track], 0)
            .await
            .unwrap();

        assert!(matches!(
            outcome,
            PlayOutcome::PlaybackAuthorizationRequired(PlaybackAuthorizationPrompt {
                reason: PlaybackAuthorizationReason::Missing,
                target_track_id: 2,
                ref target_track_uri,
                ..
            }) if target_track_uri == "spotify:track:two"
        ));
        assert!(playback.state.lock().await.reducer.snapshot().is_none());
    }

    #[test]
    fn play_outcome_wire_shape_matches_the_shared_frontend_fixture() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../test/fixtures/play-outcomes.json"))
                .unwrap();
        assert_eq!(
            serde_json::to_value(PlayOutcome::Started).unwrap(),
            fixture["started"]
        );
        assert_eq!(
            serde_json::to_value(PlayOutcome::PlaybackAuthorizationRequired(
                PlaybackAuthorizationPrompt {
                    reason: PlaybackAuthorizationReason::Missing,
                    message: "Authorize playback.".into(),
                    target_track_id: 2,
                    target_track_uri: "spotify:track:two".into(),
                    intent: 1,
                }
            ))
            .unwrap(),
            fixture["playbackAuthorizationRequired"]
        );
    }

    #[tokio::test]
    async fn local_files_remain_playable_without_playback_authorization() {
        let playback = Playback::default();
        playback.set_requested_backend(PlaybackBackend::Local);

        assert_eq!(
            playback.play(None, file_tracks(1), 0).await.unwrap(),
            PlayOutcome::Started
        );
        assert!(playback.state.lock().await.file.is_active());
    }

    #[tokio::test]
    async fn playback_auth_failure_does_not_advance_from_a_local_file() {
        let playback = Playback::default();
        playback.set_requested_backend(PlaybackBackend::Local);
        playback.play(None, mixed_tracks(), 0).await.unwrap();

        assert!(matches!(
            playback
                .next(Some(client_without_playback_credentials()))
                .await
                .unwrap(),
            PlayOutcome::PlaybackAuthorizationRequired(PlaybackAuthorizationPrompt {
                reason: PlaybackAuthorizationReason::Missing,
                target_track_id: 2,
                ref target_track_uri,
                ..
            }) if target_track_uri == "spotify:track:two"
        ));
        let state = playback.state.lock().await;
        assert_eq!(state.reducer.snapshot().unwrap().index, 0);
        assert!(state.file.is_active());
    }

    #[tokio::test]
    async fn manual_next_and_prev_do_not_complete_tracks() {
        let playback = Playback::default();
        let mut receiver = playback
            .receiver
            .lock()
            .expect("playback receiver mutex poisoned")
            .take()
            .unwrap();
        let mut tracks = mixed_tracks();
        tracks[1].uri = "file:///definitely/missing/two.mp3".into();
        playback
            .state
            .lock()
            .await
            .reducer
            .set_snapshot(Some(Snapshot::new(tracks, 0)));

        for (step, expected_uri) in [(1, "two.mp3"), (-1, "one.mp3")] {
            if step == 1 {
                playback.next(None).await.unwrap();
            } else {
                playback.prev(None).await.unwrap();
            }
            let mut state = playback.state.lock().await;
            loop {
                let event = receiver.recv().await.unwrap();
                let loaded_destination = matches!(&event, NeutralEvent::Loading { uri, .. } if uri.ends_with(expected_uri));
                assert!(!state
                    .reducer
                    .handle(event)
                    .iter()
                    .any(|action| matches!(action, ReducerAction::TrackCompleted(_))));
                if loaded_destination {
                    break;
                }
            }
        }
    }

    #[tokio::test]
    async fn mixed_boundaries_route_by_destination_without_spotify() {
        let playback = Playback::default();
        playback.play(None, mixed_tracks(), 0).await.unwrap();

        assert_eq!(playback.next(None).await.unwrap_err(), missing_spotify());
        {
            let state = playback.state.lock().await;
            assert_eq!(state.reducer.snapshot().unwrap().index, 1);
            assert!(!state.file.is_active());
        }

        playback.prev(None).await.unwrap();
        let state = playback.state.lock().await;
        assert_eq!(state.reducer.snapshot().unwrap().index, 0);
        assert!(state.file.is_active());
    }

    #[tokio::test]
    async fn clientless_spotify_failure_keeps_queue_for_later_file_selection() {
        let playback = Playback::default();

        assert_eq!(
            playback.play(None, mixed_tracks(), 1).await.unwrap_err(),
            missing_spotify()
        );
        {
            let state = playback.state.lock().await;
            assert_eq!(state.reducer.snapshot().unwrap().index, 1);
            assert!(!state.file.is_active());
        }

        playback.prev(None).await.unwrap();
        let state = playback.state.lock().await;
        assert_eq!(state.reducer.snapshot().unwrap().index, 0);
        assert!(state.file.is_active());
    }

    #[tokio::test]
    async fn file_boundary_loading_identifies_destination() {
        let playback = Playback::default();
        let mut receiver = playback
            .receiver
            .lock()
            .expect("playback receiver mutex poisoned")
            .take()
            .unwrap();

        assert_eq!(
            playback.play(None, mixed_tracks(), 1).await.unwrap_err(),
            missing_spotify()
        );
        playback.prev(None).await.unwrap();

        let request = receiver.recv().await.unwrap();
        let loading = receiver.recv().await.unwrap();
        let mut state = playback.state.lock().await;
        assert!(state.reducer.handle(request).is_empty());
        let actions = state.reducer.handle(loading);
        let [ReducerAction::Emit(event)] = actions.as_slice() else {
            panic!("file load should emit destination state");
        };
        assert_eq!(
            event.uri.as_deref(),
            Some("file:///definitely/missing/one.mp3")
        );
        assert_eq!(event.track_id, Some(1));
    }

    #[tokio::test]
    async fn repeat_all_wraps_from_spotify_to_file_without_spotify() {
        let playback = Playback::default();
        assert_eq!(
            playback.play(None, mixed_tracks(), 1).await.unwrap_err(),
            missing_spotify()
        );
        playback.set_repeat(None, RepeatMode::All).await.unwrap();

        playback.next(None).await.unwrap();
        let state = playback.state.lock().await;
        assert_eq!(state.reducer.snapshot().unwrap().index, 0);
        assert!(state.file.is_active());
    }

    #[tokio::test]
    async fn repeat_all_wraps_last_file_to_first_file() {
        let playback = Playback::default();
        let mut tracks = mixed_tracks();
        tracks[1].uri = "file:///definitely/missing/two.mp3".into();
        playback.set_repeat(None, RepeatMode::All).await.unwrap();
        playback.play(None, tracks, 1).await.unwrap();

        playback.next(None).await.unwrap();
        let state = playback.state.lock().await;
        assert_eq!(state.reducer.snapshot().unwrap().index, 0);
        assert!(state.file.is_active());
    }

    #[tokio::test]
    async fn repeat_off_stops_after_the_remaining_tracks() {
        let playback = Playback::default();
        playback.play(None, file_tracks(5), 2).await.unwrap();

        playback.next(None).await.unwrap();
        playback.next(None).await.unwrap();

        {
            let state = playback.state.lock().await;
            let snapshot = state.reducer.snapshot().unwrap();
            assert_eq!(snapshot.order, [0, 1, 2, 3, 4]);
            assert_eq!(snapshot.index, 4);
            assert_eq!(snapshot.current().id, 5);
        }

        playback.next(None).await.unwrap();
        let state = playback.state.lock().await;
        let snapshot = state.reducer.snapshot().unwrap();
        assert_eq!(snapshot.order, [0, 1, 2, 3, 4]);
        assert_eq!(snapshot.index, 4);
        assert_eq!(snapshot.current().id, 5);
        assert!(!state.file.is_active());
    }

    #[tokio::test]
    async fn repeat_one_completion_reloads_shuffled_active_current() {
        let playback = Playback::default();
        let mut receiver = playback
            .receiver
            .lock()
            .expect("playback receiver mutex poisoned")
            .take()
            .unwrap();
        playback.set_shuffle_with(true, |_| unreachable!()).await;
        let intent = playback.begin_play_intent();
        playback
            .play_with(None, file_tracks(4), 1, intent, |suffix| suffix.reverse())
            .await
            .unwrap();
        let request = receiver.recv().await.unwrap();
        let mut state = playback.state.lock().await;
        assert!(state.reducer.handle(request).is_empty());
        state.reducer.set_repeat(RepeatMode::One);
        let generation = state.generation;
        assert!(matches!(
            state
                .reducer
                .handle(NeutralEvent::EndOfTrack {
                    generation,
                    request_id: 1,
                    uri: "file:///definitely/missing/2.mp3".into(),
                })
                .as_slice(),
            [ReducerAction::TrackCompleted(_), ReducerAction::Reload]
        ));
        let before = state.file.request_id();
        drop(state);
        let mut backend = playback.backend.lock().await;
        playback
            .load_current_locked(&mut backend, None, true, 0, intent)
            .await
            .unwrap();
        drop(backend);
        let state = playback.state.lock().await;
        let snapshot = state.reducer.snapshot().unwrap();
        assert_eq!(snapshot.order, [0, 1, 3, 2]);
        assert_eq!(snapshot.index, 1);
        assert_eq!(snapshot.current().id, 2);
        assert_eq!(state.file.request_id(), before + 1);
        assert!(state.file.is_active());
    }

    #[tokio::test]
    async fn missing_file_advance_routes_into_spotify_destination() {
        let playback = Playback::default();
        let mut receiver = playback
            .receiver
            .lock()
            .expect("playback receiver mutex poisoned")
            .take()
            .unwrap();
        playback.play(None, mixed_tracks(), 0).await.unwrap();

        let mut state = playback.state.lock().await;
        assert!(state
            .reducer
            .handle(receiver.recv().await.unwrap())
            .is_empty());
        assert!(matches!(
            state
                .reducer
                .handle(receiver.recv().await.unwrap())
                .as_slice(),
            [ReducerAction::Emit(_)]
        ));
        assert!(matches!(
            state
                .reducer
                .handle(receiver.recv().await.unwrap())
                .as_slice(),
            [ReducerAction::Error(_), ReducerAction::Advance]
        ));

        drop(state);
        assert_eq!(
            playback.step(None, 1).await.unwrap_err().into_string(),
            missing_spotify()
        );
        let state = playback.state.lock().await;
        assert_eq!(state.reducer.snapshot().unwrap().index, 1);
        assert!(!state.file.is_active());
    }

    #[tokio::test]
    async fn active_local_backend_stops_before_file_load_without_client() {
        let playback = Playback::default();
        let tracks = mixed_tracks();
        let mut backend = playback.backend.lock().await;
        let mut state = playback.state.lock().await;
        *backend = PlayerBackend::Local(LocalBackend::with_snapshot_for_test(Snapshot::new(
            tracks.clone(),
            1,
        )));
        state.reducer.set_snapshot(Some(Snapshot::new(tracks, 0)));
        drop(state);
        let intent = playback.current_play_intent();

        playback
            .load_current_locked(&mut backend, None, true, 0, intent)
            .await
            .unwrap();

        let PlayerBackend::Local(local) = &*backend else {
            panic!("local backend should remain selected");
        };
        assert!(!local.has_snapshot());
        assert!(playback.state.lock().await.file.is_active());
    }

    #[tokio::test]
    async fn connect_boundary_returns_to_controller_for_file_load() {
        let playback = Playback::default();
        let mut receiver = playback
            .receiver
            .lock()
            .expect("playback receiver mutex poisoned")
            .take()
            .unwrap();
        let mut tracks = mixed_tracks();
        tracks.reverse();
        let mut state = playback.state.lock().await;
        state.reducer.set_snapshot(Some(Snapshot::new(tracks, 0)));
        let generation = state.generation;

        assert_eq!(
            state.reducer.handle(NeutralEvent::ConnectBoundary {
                generation,
                uri: "spotify:track:two".into(),
            }),
            [
                ReducerAction::TrackCompleted("spotify:track:two".into()),
                ReducerAction::Advance
            ]
        );
        drop(state);
        playback.step(None, 1).await.unwrap();
        let mut state = playback.state.lock().await;
        assert_eq!(state.reducer.snapshot().unwrap().index, 1);
        assert!(state.file.is_active());

        assert!(state
            .reducer
            .handle(receiver.recv().await.unwrap())
            .is_empty());
        let actions = state.reducer.handle(receiver.recv().await.unwrap());
        let [ReducerAction::Emit(event)] = actions.as_slice() else {
            panic!("file load should emit destination state");
        };
        assert_eq!(
            event.uri.as_deref(),
            Some("file:///definitely/missing/one.mp3")
        );
        assert_eq!(event.track_id, Some(1));
    }

    #[tokio::test]
    async fn failed_switch_keeps_connect_backend() {
        let playback = Playback::default();
        let intent = playback.set_requested_backend(PlaybackBackend::Local);
        let result = playback
            .switch_to_local_with(None, intent, || async { Err("preflight failed".into()) })
            .await;
        assert_eq!(result.unwrap_err(), "preflight failed");
        assert!(!playback.is_local_active().await);
    }

    #[tokio::test]
    async fn stale_local_activation_cannot_replace_newer_connect_intent() {
        let playback = Arc::new(Playback::default());
        let prepared = Arc::new(tokio::sync::Barrier::new(2));
        let release = Arc::new(tokio::sync::Barrier::new(2));
        let intent = playback.set_requested_backend(PlaybackBackend::Local);
        let activation = tokio::spawn({
            let playback = Arc::clone(&playback);
            let prepared = Arc::clone(&prepared);
            let release = Arc::clone(&release);
            async move {
                playback
                    .switch_to_local_with(None, intent, || async move {
                        prepared.wait().await;
                        release.wait().await;
                        Ok(LocalBackend::with_snapshot_for_test(Snapshot::new(
                            file_tracks(1),
                            0,
                        )))
                    })
                    .await
            }
        });

        prepared.wait().await;
        playback.switch_to_connect().await;
        let connect_generation = playback.state.lock().await.generation;
        release.wait().await;
        activation.await.unwrap().unwrap();

        assert!(matches!(
            *playback.backend.lock().await,
            PlayerBackend::Connect(_)
        ));
        assert_eq!(playback.state.lock().await.generation, connect_generation);
    }

    #[tokio::test]
    async fn older_prepared_play_cannot_replace_a_newer_play_intent() {
        let playback = Arc::new(Playback::default());
        let older = playback.begin_play_intent();
        let release = Arc::new(tokio::sync::Notify::new());
        let delayed = tokio::spawn({
            let playback = Arc::clone(&playback);
            let release = Arc::clone(&release);
            async move {
                release.notified().await;
                let mut stale = file_tracks(1);
                stale[0].id = 99;
                playback
                    .play_for_intent(None, stale, 0, older)
                    .await
                    .unwrap();
            }
        });
        let newer = playback.begin_play_intent();

        playback
            .play_for_intent(None, file_tracks(1), 0, newer)
            .await
            .unwrap();
        release.notify_one();
        delayed.await.unwrap();

        assert_eq!(
            playback
                .state
                .lock()
                .await
                .reducer
                .snapshot()
                .unwrap()
                .current()
                .id,
            1
        );
    }

    #[tokio::test]
    async fn stale_authorization_cleanup_cannot_stop_a_newer_play() {
        let playback = Playback::default();
        let stale_intent = playback.begin_play_intent();
        let prompt = PlaybackAuthorizationPrompt {
            reason: PlaybackAuthorizationReason::Missing,
            message: "Authorize playback.".into(),
            target_track_id: 99,
            target_track_uri: "spotify:track:stale".into(),
            intent: stale_intent,
        };
        playback.play(None, file_tracks(1), 0).await.unwrap();

        assert!(playback
            .stop_for_authorization(None, prompt)
            .await
            .is_empty());
        let state = playback.state.lock().await;
        assert!(state.file.is_active());
        assert_eq!(state.reducer.snapshot().unwrap().current().id, 1);
    }

    #[tokio::test]
    async fn stop_and_backend_switch_invalidate_a_pending_play() {
        let playback = Playback::default();
        let stopped = playback.begin_play_intent();
        playback.stop(None).await.unwrap();
        playback
            .play_for_intent(None, file_tracks(1), 0, stopped)
            .await
            .unwrap();
        assert!(playback.state.lock().await.reducer.snapshot().is_none());

        let switched = playback.begin_play_intent();
        playback.set_requested_backend(PlaybackBackend::Connect);
        playback
            .play_for_intent(None, file_tracks(1), 0, switched)
            .await
            .unwrap();
        assert!(playback.state.lock().await.reducer.snapshot().is_none());
    }

    #[tokio::test]
    async fn idle_transport_commands_are_noops() {
        let playback = Playback::default();

        playback.toggle(None).await.unwrap();
        playback.next(None).await.unwrap();
        playback.prev(None).await.unwrap();
        playback.seek(None, 42).await.unwrap();
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

    #[test]
    fn preload_follows_active_order_and_repeat_policy() {
        let mut snapshot = Snapshot::new(file_tracks(3), 0);
        assert_eq!(preload_track(&snapshot, RepeatMode::Off).unwrap().id, 2);

        snapshot.set_shuffle_with(true, |suffix| suffix.reverse());
        assert_eq!(preload_track(&snapshot, RepeatMode::Off).unwrap().id, 3);

        snapshot.index = 2;
        assert!(preload_track(&snapshot, RepeatMode::Off).is_none());
        assert_eq!(preload_track(&snapshot, RepeatMode::All).unwrap().id, 1);
        assert!(preload_track(&snapshot, RepeatMode::One).is_none());
    }
}

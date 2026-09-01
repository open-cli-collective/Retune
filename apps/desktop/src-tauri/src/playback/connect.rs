use std::sync::{Arc, Once};

use retune_spotify::{
    client::{Device, PlayerState, SpotifyClient, Transport},
    tokens::TokenStore,
};
use tokio::sync::mpsc;

use super::{LiveClient, NeutralEvent, NeutralState, RepeatMode, Snapshot, SnapshotTrack};

const MAX_QUEUE_URIS: usize = 200;
static QUEUE_CAP_WARNING: Once = Once::new();

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolledState {
    pub track_id: Option<String>,
    pub elapsed: u64,
    pub is_playing: bool,
    pub device_present: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackDecision {
    Tick,
    Advance,
    Takeover,
    Stop,
    DeviceGone,
}

#[cfg(test)]
pub fn resolve(prev: PolledState, now: PolledState, expected: &Snapshot) -> PlaybackDecision {
    resolve_with_wrap(prev, now, expected, false)
}

fn resolve_with_wrap(
    prev: PolledState,
    now: PolledState,
    expected: &Snapshot,
    wrap: bool,
) -> PlaybackDecision {
    let terminal = || {
        if expected.has_next() || wrap {
            PlaybackDecision::Advance
        } else {
            PlaybackDecision::Stop
        }
    };
    let near_end = |state: &PolledState| {
        state.track_id.as_deref() == Some(expected.current().uri.as_str())
            && state.elapsed.saturating_add(5) >= expected.current().duration_secs
    };
    if near_end(&now) && !now.is_playing {
        return terminal();
    }
    if !now.device_present {
        return if near_end(&prev) {
            terminal()
        } else {
            PlaybackDecision::DeviceGone
        };
    }
    if now.track_id.as_deref() == Some(expected.current().uri.as_str()) {
        return PlaybackDecision::Tick;
    }
    if near_end(&prev) {
        return terminal();
    }
    if now.track_id.as_ref().is_some_and(|uri| {
        expected
            .order
            .iter()
            .any(|&index| expected.tracks[index].uri.as_str() == uri)
    }) {
        return PlaybackDecision::Advance;
    }
    PlaybackDecision::Takeover
}

fn poll_decision(context: &Context, now: PolledState, epoch: u64) -> Option<PlaybackDecision> {
    (context.epoch == epoch).then(|| {
        resolve_with_wrap(
            context.previous.clone(),
            now,
            &context.snapshot,
            context.wrap,
        )
    })
}

struct Context {
    snapshot: Snapshot,
    device_id: String,
    volume_supported: bool,
    previous: PolledState,
    generation: u64,
    epoch: u64,
    route_at_end: bool,
    wrap: bool,
    file_after_segment: bool,
    file_in_queue: bool,
}

#[derive(Default)]
struct State {
    context: Option<Context>,
    generation: u64,
    revision: u64,
}

#[derive(Clone)]
pub(super) struct ConnectBackend {
    state: Arc<tokio::sync::Mutex<State>>,
    operations: Arc<tokio::sync::Mutex<()>>,
    events: mpsc::UnboundedSender<NeutralEvent>,
    backend_generation: u64,
}

impl ConnectBackend {
    pub(super) fn new(
        events: mpsc::UnboundedSender<NeutralEvent>,
        backend_generation: u64,
    ) -> Self {
        Self {
            state: Arc::new(tokio::sync::Mutex::new(State::default())),
            operations: Arc::new(tokio::sync::Mutex::new(())),
            events,
            backend_generation,
        }
    }

    pub(super) async fn play(
        &self,
        client: Arc<LiveClient>,
        snapshot: Snapshot,
        repeat: RepeatMode,
    ) -> Result<(), String> {
        let Some((event, generation)) = self.begin(client.as_ref(), snapshot, repeat).await? else {
            return Ok(());
        };
        self.emit(event);
        let playback = self.clone();
        tokio::spawn(async move {
            playback.poll(client, generation).await;
        });
        Ok(())
    }

    async fn begin<T: Transport, S: TokenStore>(
        &self,
        client: &SpotifyClient<T, S>,
        snapshot: Snapshot,
        repeat: RepeatMode,
    ) -> Result<Option<(NeutralState, u64)>, String> {
        let (snapshot, route_at_end, file_after_segment, file_in_queue) =
            connect_snapshot_parts(snapshot, repeat);
        let revision = {
            let mut state = self.state.lock().await;
            state.revision = state.revision.wrapping_add(1);
            state.revision
        };
        let _operation = self.operations.lock().await;
        let device = select_device(client.devices().await.map_err(|error| error.to_string())?)?;
        let device_id = device.id.expect("selected devices have ids");
        client
            .set_repeat(
                connect_repeat(if route_at_end {
                    RepeatMode::Off
                } else {
                    repeat
                }),
                Some(&device_id),
            )
            .await
            .map_err(|error| error.to_string())?;
        play_snapshot(
            client,
            &device_id,
            &snapshot.active_tracks(),
            snapshot.index,
        )
        .await?;
        let mut state = self.state.lock().await;
        if state.revision != revision {
            return Ok(None);
        }
        state.generation = state.generation.wrapping_add(1);
        let generation = state.generation;
        let event = local_state(&snapshot, 0, true, device.supports_volume);
        state.context = Some(Context {
            previous: PolledState {
                track_id: Some(snapshot.current().uri.clone()),
                elapsed: 0,
                is_playing: true,
                device_present: true,
            },
            snapshot,
            device_id,
            volume_supported: device.supports_volume,
            generation,
            epoch: 1,
            route_at_end,
            wrap: repeat.wraps() && !route_at_end,
            file_after_segment,
            file_in_queue,
        });
        Ok(Some((event, generation)))
    }

    async fn toggle_pause<T: Transport, S: TokenStore>(
        &self,
        client: &SpotifyClient<T, S>,
    ) -> Result<NeutralState, String> {
        self.set_context_playing(client, None)
            .await?
            .ok_or_else(|| "Nothing to toggle".into())
    }

    async fn set_playing_state<T: Transport, S: TokenStore>(
        &self,
        client: &SpotifyClient<T, S>,
        playing: bool,
    ) -> Result<Option<NeutralState>, String> {
        self.set_context_playing(client, Some(playing)).await
    }

    async fn set_context_playing<T: Transport, S: TokenStore>(
        &self,
        client: &SpotifyClient<T, S>,
        requested: Option<bool>,
    ) -> Result<Option<NeutralState>, String> {
        let (revision, generation, device_id, playing) = {
            let mut state = self.state.lock().await;
            let context = state.context.as_ref().ok_or("Nothing is playing")?;
            let playing = requested.unwrap_or(!context.previous.is_playing);
            if context.previous.is_playing == playing {
                return Ok(None);
            }
            let generation = context.generation;
            let device_id = context.device_id.clone();
            state.revision = state.revision.wrapping_add(1);
            (state.revision, generation, device_id, playing)
        };
        let _operation = self.operations.lock().await;
        if playing {
            client.resume(Some(&device_id)).await
        } else {
            client.pause(Some(&device_id)).await
        }
        .map_err(|error| error.to_string())?;
        let mut state = self.state.lock().await;
        if state.revision != revision {
            return Ok(None);
        }
        let Some(context) = state
            .context
            .as_mut()
            .filter(|context| context.generation == generation)
        else {
            return Ok(None);
        };
        context.epoch = context.epoch.wrapping_add(1);
        context.previous.is_playing = playing;
        Ok(Some(local_state(
            &context.snapshot,
            context.previous.elapsed,
            playing,
            context.volume_supported,
        )))
    }

    async fn seek_state<T: Transport, S: TokenStore>(
        &self,
        client: &SpotifyClient<T, S>,
        seconds: u64,
    ) -> Result<Option<NeutralState>, String> {
        let (revision, generation, device_id, seconds) = {
            let mut state = self.state.lock().await;
            let context = state.context.as_ref().ok_or("Nothing is playing")?;
            let seconds = seconds.min(context.snapshot.current().duration_secs);
            let generation = context.generation;
            let device_id = context.device_id.clone();
            state.revision = state.revision.wrapping_add(1);
            (state.revision, generation, device_id, seconds)
        };
        let position_ms = u32::try_from(seconds.saturating_mul(1000))
            .map_err(|_| "seek position out of range".to_string())?;
        let _operation = self.operations.lock().await;
        client
            .seek(position_ms, Some(&device_id))
            .await
            .map_err(|error| error.to_string())?;
        let mut state = self.state.lock().await;
        if state.revision != revision {
            return Ok(None);
        }
        let context = state
            .context
            .as_mut()
            .filter(|context| context.generation == generation)
            .ok_or("Nothing is playing")?;
        context.epoch = context.epoch.wrapping_add(1);
        context.previous.elapsed = seconds;
        Ok(Some(local_state(
            &context.snapshot,
            seconds,
            context.previous.is_playing,
            context.volume_supported,
        )))
    }

    async fn step_state<T: Transport, S: TokenStore>(
        &self,
        client: &SpotifyClient<T, S>,
        direction: i8,
    ) -> Result<Option<NeutralState>, String> {
        let (revision, generation, device_id, snapshot, next) = {
            let mut state = self.state.lock().await;
            let context = state.context.as_ref().ok_or("Nothing is playing")?;
            let next = if direction < 0 {
                context.snapshot.index.saturating_sub(1)
            } else {
                context.snapshot.index + 1
            };
            let generation = context.generation;
            let device_id = context.device_id.clone();
            let snapshot = context.snapshot.clone();
            state.revision = state.revision.wrapping_add(1);
            (state.revision, generation, device_id, snapshot, next)
        };
        let _operation = self.operations.lock().await;
        if next >= snapshot.len() {
            client
                .pause(Some(&device_id))
                .await
                .map_err(|error| error.to_string())?;
            let mut state = self.state.lock().await;
            if state.revision != revision
                || state
                    .context
                    .as_ref()
                    .is_none_or(|context| context.generation != generation)
            {
                return Ok(None);
            }
            state.context = None;
            return Ok(Some(NeutralState::default()));
        }
        play_snapshot(client, &device_id, &snapshot.active_tracks(), next).await?;
        let mut state = self.state.lock().await;
        if state.revision != revision {
            return Ok(None);
        }
        let context = state
            .context
            .as_mut()
            .filter(|context| context.generation == generation)
            .ok_or("Nothing is playing")?;
        context.snapshot.index = next;
        context.epoch = context.epoch.wrapping_add(1);
        context.previous = PolledState {
            track_id: Some(context.snapshot.current().uri.clone()),
            elapsed: 0,
            is_playing: true,
            device_present: true,
        };
        Ok(Some(local_state(
            &context.snapshot,
            0,
            true,
            context.volume_supported,
        )))
    }

    pub(super) async fn set_volume<T: Transport, S: TokenStore>(
        &self,
        client: &SpotifyClient<T, S>,
        volume: u8,
    ) -> Result<(), String> {
        let (revision, device_id) = {
            let state = self.state.lock().await;
            let context = state.context.as_ref().ok_or("Nothing is playing")?;
            if !context.volume_supported {
                return Ok(());
            }
            (state.revision, context.device_id.clone())
        };
        let _operation = self.operations.lock().await;
        if self.state.lock().await.revision != revision {
            return Ok(());
        }
        client
            .set_volume(volume, Some(&device_id))
            .await
            .map_err(|error| error.to_string())
    }

    pub(super) async fn set_repeat_state<T: Transport, S: TokenStore>(
        &self,
        client: &SpotifyClient<T, S>,
        repeat: RepeatMode,
    ) -> Result<(), String> {
        let (revision, generation, device_id, route_at_end, wrap) = {
            let mut state = self.state.lock().await;
            let Some(context) = state
                .context
                .as_ref()
                .filter(|context| context.previous.is_playing)
            else {
                return Ok(());
            };
            let generation = context.generation;
            let device_id = context.device_id.clone();
            let route_at_end =
                context.file_after_segment || (repeat.wraps() && context.file_in_queue);
            let wrap = repeat.wraps() && !route_at_end;
            state.revision = state.revision.wrapping_add(1);
            (state.revision, generation, device_id, route_at_end, wrap)
        };
        let _operation = self.operations.lock().await;
        if self.state.lock().await.revision != revision {
            return Ok(());
        }
        client
            .set_repeat(
                connect_repeat(if route_at_end {
                    RepeatMode::Off
                } else {
                    repeat
                }),
                Some(&device_id),
            )
            .await
            .map_err(|error| error.to_string())?;
        let mut state = self.state.lock().await;
        if state.revision != revision {
            return Ok(());
        }
        let Some(context) = state
            .context
            .as_mut()
            .filter(|context| context.generation == generation)
        else {
            return Ok(());
        };
        context.route_at_end = route_at_end;
        context.wrap = wrap;
        context.epoch = context.epoch.wrapping_add(1);
        Ok(())
    }

    pub(super) async fn update_snapshot(&self, snapshot: Snapshot, repeat: RepeatMode) {
        let (snapshot, route_at_end, file_after_segment, file_in_queue) =
            connect_snapshot_parts(snapshot, repeat);
        let mut state = self.state.lock().await;
        let Some(context) = state.context.as_mut() else {
            return;
        };
        if snapshot.current().uri != context.snapshot.current().uri {
            return;
        }
        context.snapshot = snapshot;
        context.route_at_end = route_at_end;
        context.wrap = repeat.wraps() && !route_at_end;
        context.file_after_segment = file_after_segment;
        context.file_in_queue = file_in_queue;
        context.epoch = context.epoch.wrapping_add(1);
        state.revision = state.revision.wrapping_add(1);
    }

    pub(super) async fn toggle(&self, client: &LiveClient) -> Result<(), String> {
        let state = self.toggle_pause(client).await?;
        self.emit(state);
        Ok(())
    }

    pub(super) async fn set_playing(
        &self,
        client: &LiveClient,
        playing: bool,
    ) -> Result<(), String> {
        if let Some(state) = self.set_playing_state(client, playing).await? {
            self.emit(state);
        }
        Ok(())
    }

    pub(super) async fn seek(&self, client: &LiveClient, seconds: u64) -> Result<(), String> {
        if let Some(state) = self.seek_state(client, seconds).await? {
            self.emit(state);
        }
        Ok(())
    }

    pub(super) async fn step(&self, client: &LiveClient, direction: i8) -> Result<(), String> {
        if let Some(state) = self.step_state(client, direction).await? {
            self.emit(state);
        }
        Ok(())
    }

    pub(super) async fn stop(&self, client: Option<&LiveClient>) -> Result<(), String> {
        let (revision, device_id) = {
            let mut state = self.state.lock().await;
            if state.context.is_some() && client.is_none() {
                return Err(super::missing_spotify());
            }
            let device_id = state
                .context
                .as_ref()
                .map(|context| context.device_id.clone());
            state.revision = state.revision.wrapping_add(1);
            (state.revision, device_id)
        };
        let _operation = self.operations.lock().await;
        if let (Some(client), Some(device_id)) = (client, device_id) {
            client
                .pause(Some(&device_id))
                .await
                .map_err(|error| error.to_string())?;
        }
        let mut state = self.state.lock().await;
        if state.revision == revision {
            state.context = None;
        }
        Ok(())
    }

    fn emit(&self, state: NeutralState) {
        let _ = self.events.send(NeutralEvent::ConnectState {
            generation: self.backend_generation,
            state,
        });
    }

    async fn poll<T: Transport + 'static, S: TokenStore + 'static>(
        self,
        client: Arc<SpotifyClient<T, S>>,
        generation: u64,
    ) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        interval.tick().await;
        loop {
            interval.tick().await;
            let (revision, epoch) = {
                let state = self.state.lock().await;
                let Some(context) = state
                    .context
                    .as_ref()
                    .filter(|context| context.generation == generation)
                else {
                    return;
                };
                (state.revision, context.epoch)
            };
            let _operation = self.operations.lock().await;
            if self.state.lock().await.revision != revision {
                continue;
            }
            let polled = match client.player().await {
                Ok(player) => player,
                Err(error) => {
                    let mut state = self.state.lock().await;
                    if state.revision != revision
                        || state.context.as_ref().is_none_or(|context| {
                            context.generation != generation || context.epoch != epoch
                        })
                    {
                        continue;
                    }
                    state.revision = state.revision.wrapping_add(1);
                    state.context = None;
                    drop(state);
                    let _ = self.events.send(NeutralEvent::Error {
                        generation: self.backend_generation,
                        message: error.to_string(),
                    });
                    return;
                }
            };
            let now = polled_state(polled.as_ref());
            let decision = {
                let state = self.state.lock().await;
                let Some(context) = state.context.as_ref().filter(|context| {
                    context.generation == generation && state.revision == revision
                }) else {
                    continue;
                };
                let Some(decision) = poll_decision(context, now.clone(), epoch) else {
                    continue;
                };
                decision
            };
            let event = match decision {
                PlaybackDecision::Tick => {
                    let mut state = self.state.lock().await;
                    if state.revision != revision {
                        continue;
                    }
                    let Some(context) = state.context.as_mut().filter(|context| {
                        context.generation == generation && context.epoch == epoch
                    }) else {
                        continue;
                    };
                    context.previous = now;
                    local_state(
                        &context.snapshot,
                        context.previous.elapsed,
                        context.previous.is_playing,
                        context.volume_supported,
                    )
                }
                PlaybackDecision::Advance => {
                    let (mut snapshot, device_id, volume_supported, wrap, polled_index) = {
                        let state = self.state.lock().await;
                        if state.revision != revision {
                            continue;
                        }
                        let Some(context) = state.context.as_ref().filter(|context| {
                            context.generation == generation && context.epoch == epoch
                        }) else {
                            continue;
                        };
                        let polled_index = now
                            .track_id
                            .as_ref()
                            .and_then(|uri| context.snapshot.active_position(uri));
                        (
                            context.snapshot.clone(),
                            context.device_id.clone(),
                            context.volume_supported,
                            context.wrap,
                            polled_index,
                        )
                    };
                    let previous_index = snapshot.index;
                    snapshot.index = if previous_index + 1 < snapshot.len() {
                        previous_index + 1
                    } else if wrap {
                        0
                    } else {
                        polled_index.unwrap_or(previous_index)
                    };
                    let previous = if polled_index != Some(snapshot.index) {
                        if let Err(error) = play_snapshot(
                            client.as_ref(),
                            &device_id,
                            &snapshot.active_tracks(),
                            snapshot.index,
                        )
                        .await
                        {
                            let mut state = self.state.lock().await;
                            if state.revision != revision
                                || state.context.as_ref().is_none_or(|context| {
                                    context.generation != generation || context.epoch != epoch
                                })
                            {
                                continue;
                            }
                            state.revision = state.revision.wrapping_add(1);
                            state.context = None;
                            drop(state);
                            let _ = self.events.send(NeutralEvent::Error {
                                generation: self.backend_generation,
                                message: error,
                            });
                            return;
                        }
                        PolledState {
                            track_id: Some(snapshot.current().uri.clone()),
                            elapsed: 0,
                            is_playing: true,
                            device_present: true,
                        }
                    } else {
                        now
                    };
                    let mut state = self.state.lock().await;
                    if state.revision != revision {
                        continue;
                    }
                    let Some(context) = state.context.as_mut().filter(|context| {
                        context.generation == generation && context.epoch == epoch
                    }) else {
                        continue;
                    };
                    context.snapshot = snapshot;
                    context.previous = previous;
                    local_state(
                        &context.snapshot,
                        context.previous.elapsed,
                        context.previous.is_playing,
                        volume_supported,
                    )
                }
                PlaybackDecision::Stop => {
                    let (route_at_end, uri, device_id) = {
                        let state = self.state.lock().await;
                        if state.revision != revision {
                            continue;
                        }
                        let Some(context) = state.context.as_ref().filter(|context| {
                            context.generation == generation && context.epoch == epoch
                        }) else {
                            continue;
                        };
                        (
                            context.route_at_end,
                            context.snapshot.current().uri.clone(),
                            context.device_id.clone(),
                        )
                    };
                    let _ = client.pause(Some(&device_id)).await;
                    let mut state = self.state.lock().await;
                    if state.revision != revision {
                        continue;
                    }
                    state.revision = state.revision.wrapping_add(1);
                    state.context = None;
                    drop(state);
                    if route_at_end {
                        let _ = self.events.send(NeutralEvent::ConnectBoundary {
                            generation: self.backend_generation,
                            uri,
                        });
                    } else {
                        self.emit(NeutralState::default());
                    }
                    return;
                }
                PlaybackDecision::Takeover | PlaybackDecision::DeviceGone => {
                    let mut state = self.state.lock().await;
                    if state.revision != revision {
                        continue;
                    }
                    state.revision = state.revision.wrapping_add(1);
                    state.context = None;
                    external_state(polled.as_ref())
                }
            };
            self.emit(event);
            if matches!(
                decision,
                PlaybackDecision::Takeover | PlaybackDecision::DeviceGone
            ) {
                return;
            }
        }
    }
}

fn connect_repeat(repeat: RepeatMode) -> &'static str {
    match repeat {
        RepeatMode::Off => "off",
        RepeatMode::All => "context",
        RepeatMode::One => "track",
    }
}

#[cfg(test)]
fn connect_snapshot(snapshot: Snapshot, repeat: RepeatMode) -> (Snapshot, bool) {
    let (snapshot, route_at_end, _, _) = connect_snapshot_parts(snapshot, repeat);
    (snapshot, route_at_end)
}

fn connect_snapshot_parts(snapshot: Snapshot, repeat: RepeatMode) -> (Snapshot, bool, bool, bool) {
    let tracks = snapshot.active_tracks();
    let start = tracks[..snapshot.index]
        .iter()
        .rposition(|track| track.uri.starts_with("file:"))
        .map_or(0, |index| index + 1);
    let end = tracks[start..]
        .iter()
        .position(|track| track.uri.starts_with("file:"))
        .map_or(tracks.len(), |offset| start + offset);
    let file_after_segment = end < tracks.len();
    let file_in_queue = tracks.iter().any(|track| track.uri.starts_with("file:"));
    let route_at_end = file_after_segment || (repeat.wraps() && file_in_queue);
    (
        Snapshot::new(tracks[start..end].to_vec(), snapshot.index - start),
        route_at_end,
        file_after_segment,
        file_in_queue,
    )
}

fn queue_request(tracks: &[SnapshotTrack], index: usize) -> (Vec<String>, usize) {
    let start = if tracks.len() > MAX_QUEUE_URIS {
        index
    } else {
        0
    };
    let uris = tracks[start..]
        .iter()
        .take_while(|track| track.uri.starts_with("spotify:"))
        .take(MAX_QUEUE_URIS)
        .map(|track| track.uri.clone())
        .collect();
    (uris, index - start)
}

async fn play_snapshot<T: Transport, S: TokenStore>(
    client: &SpotifyClient<T, S>,
    device_id: &str,
    tracks: &[SnapshotTrack],
    index: usize,
) -> Result<(), String> {
    if tracks.len() > MAX_QUEUE_URIS {
        QUEUE_CAP_WARNING.call_once(|| {
            log::warn!("Spotify Connect queue capped at {MAX_QUEUE_URIS} tracks");
        });
    }
    let (uris, offset) = queue_request(tracks, index);
    client
        .play(Some(device_id), &uris, offset)
        .await
        .map_err(|error| error.to_string())
}

fn select_device(devices: Vec<Device>) -> Result<Device, String> {
    devices
        .iter()
        .find(|device| usable_desktop(device) && device.is_active)
        .or_else(|| devices.iter().find(|device| usable_desktop(device)))
        .cloned()
        .ok_or_else(|| "Open Spotify on your desktop".into())
}

fn usable_desktop(device: &Device) -> bool {
    device.device_type.eq_ignore_ascii_case("computer")
        && !device.is_restricted
        && device.id.is_some()
}

fn polled_state(player: Option<&PlayerState>) -> PolledState {
    PolledState {
        track_id: player.and_then(|state| state.item.as_ref().map(|item| item.uri.clone())),
        elapsed: player.and_then(|state| state.progress_ms).unwrap_or(0) / 1000,
        is_playing: player.is_some_and(|state| state.is_playing),
        device_present: player.is_some(),
    }
}

fn local_state(
    snapshot: &Snapshot,
    elapsed: u64,
    is_playing: bool,
    volume_supported: bool,
) -> NeutralState {
    let track = snapshot.current();
    NeutralState {
        uri: Some(track.uri.clone()),
        position_ms: u32::try_from(elapsed.saturating_mul(1000)).unwrap_or(u32::MAX),
        is_playing,
        external: false,
        name: Some(track.name.clone()),
        art: Some(track.art.clone()),
        alb: Some(track.alb.clone()),
        duration_ms: u32::try_from(track.duration_secs.saturating_mul(1000)).ok(),
        volume_supported,
    }
}

fn external_state(player: Option<&PlayerState>) -> NeutralState {
    let item = player.and_then(|state| state.item.as_ref());
    NeutralState {
        uri: item.map(|track| track.uri.clone()),
        position_ms: player
            .and_then(|state| state.progress_ms)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(0),
        is_playing: player.is_some_and(|state| state.is_playing),
        external: true,
        name: item.map(|track| track.name.clone()),
        art: item.and_then(|track| track.artists.first().map(|artist| artist.name.clone())),
        alb: item.and_then(|track| track.album.as_ref().map(|album| album.name.clone())),
        duration_ms: item.and_then(|track| {
            track
                .duration_ms
                .and_then(|value| u32::try_from(value).ok())
        }),
        volume_supported: false,
    }
}

#[cfg(test)]
impl Default for ConnectBackend {
    fn default() -> Self {
        let (events, _) = mpsc::unbounded_channel();
        Self::new(events, 1)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{atomic::AtomicUsize, Mutex},
        time::Duration,
    };

    use retune_spotify::{
        client::{fake_client, Request, Response, SendFuture},
        tokens::{InMemoryTokenStore, Tokens},
        Error,
    };
    use tokio::sync::Notify;

    use super::*;

    struct DelayedTransport {
        responses: Mutex<VecDeque<Response>>,
        requests: Mutex<Vec<Request>>,
        releases: Vec<Arc<Notify>>,
        entered: mpsc::UnboundedSender<String>,
        next: AtomicUsize,
    }

    impl Transport for DelayedTransport {
        fn send(&self, request: Request) -> SendFuture<'_> {
            let index = self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.requests.lock().unwrap().push(request.clone());
            let response = self.responses.lock().unwrap().pop_front();
            let release = self.releases.get(index).cloned();
            let _ = self.entered.send(request.url);
            Box::pin(async move {
                let release = release.ok_or_else(|| Error::Transport("missing release".into()))?;
                release.notified().await;
                response.ok_or_else(|| Error::Transport("missing response".into()))
            })
        }
    }

    fn delayed_client(
        responses: impl IntoIterator<Item = Response>,
        releases: Vec<Arc<Notify>>,
        entered: mpsc::UnboundedSender<String>,
    ) -> SpotifyClient<DelayedTransport, InMemoryTokenStore> {
        SpotifyClient::new(
            "client",
            DelayedTransport {
                responses: Mutex::new(responses.into_iter().collect()),
                requests: Mutex::new(Vec::new()),
                releases,
                entered,
                next: AtomicUsize::new(0),
            },
            InMemoryTokenStore::new(Some(Tokens {
                access: "access".into(),
                refresh: "refresh".into(),
                expires_at: u64::MAX,
                scopes: String::new(),
                playback_credentials: None,
            })),
        )
    }

    async fn install_context(backend: &ConnectBackend) {
        let mut state = backend.state.lock().await;
        state.context = Some(Context {
            snapshot: Snapshot::new(vec![track(1, 100), track(2, 100)], 0),
            device_id: "desk".into(),
            volume_supported: true,
            previous: polled(Some("spotify:track:1"), 10, true),
            generation: 1,
            epoch: 1,
            route_at_end: false,
            wrap: false,
            file_after_segment: false,
            file_in_queue: false,
        });
        state.generation = 1;
    }

    #[tokio::test]
    async fn delayed_transport_does_not_block_state_and_stale_completion_cannot_commit() {
        let backend = ConnectBackend::default();
        install_context(&backend).await;
        let release = Arc::new(Notify::new());
        let (entered, mut entries) = mpsc::unbounded_channel();
        let client = Arc::new(delayed_client(
            [Response::json(204, serde_json::Value::Null)],
            vec![Arc::clone(&release)],
            entered,
        ));
        let delayed_backend = backend.clone();
        let delayed_client = Arc::clone(&client);
        let delayed = tokio::spawn(async move {
            delayed_backend
                .seek_state(delayed_client.as_ref(), 40)
                .await
        });

        assert!(entries
            .recv()
            .await
            .unwrap()
            .contains("/seek?position_ms=40000"));
        let _guard = tokio::time::timeout(Duration::from_millis(50), backend.state.lock())
            .await
            .expect("state reads must not wait for Spotify");
        drop(_guard);
        backend.state.lock().await.revision += 1;
        release.notify_one();
        assert!(delayed.await.unwrap().unwrap().is_none());

        assert_eq!(
            backend
                .state
                .lock()
                .await
                .context
                .as_ref()
                .unwrap()
                .previous
                .elapsed,
            10
        );
    }

    #[tokio::test]
    async fn stale_end_step_cannot_clear_a_newer_snapshot() {
        let backend = ConnectBackend::default();
        install_context(&backend).await;
        {
            let mut state = backend.state.lock().await;
            let context = state.context.as_mut().unwrap();
            context.snapshot.index = 1;
            context.previous = polled(Some("spotify:track:2"), 10, true);
        }
        let release = Arc::new(Notify::new());
        let (entered, mut entries) = mpsc::unbounded_channel();
        let client = Arc::new(delayed_client(
            [Response::json(204, serde_json::Value::Null)],
            vec![Arc::clone(&release)],
            entered,
        ));
        let delayed_backend = backend.clone();
        let delayed_client = Arc::clone(&client);
        let delayed =
            tokio::spawn(
                async move { delayed_backend.step_state(delayed_client.as_ref(), 1).await },
            );

        assert!(entries
            .recv()
            .await
            .unwrap()
            .contains("/pause?device_id=desk"));
        backend
            .update_snapshot(
                Snapshot::new(vec![track(2, 100), track(3, 100)], 0),
                RepeatMode::Off,
            )
            .await;
        release.notify_one();

        assert!(delayed.await.unwrap().unwrap().is_none());
        let state = backend.state.lock().await;
        assert_eq!(
            state.context.as_ref().unwrap().snapshot.current().uri,
            "spotify:track:2"
        );
    }

    #[tokio::test]
    async fn poll_and_command_remote_calls_share_one_ordered_gate() {
        let backend = ConnectBackend::default();
        install_context(&backend).await;
        let poll_release = Arc::new(Notify::new());
        let seek_release = Arc::new(Notify::new());
        let (entered, mut entries) = mpsc::unbounded_channel();
        let client = Arc::new(delayed_client(
            [
                Response::json(204, serde_json::Value::Null),
                Response::json(204, serde_json::Value::Null),
            ],
            vec![Arc::clone(&poll_release), Arc::clone(&seek_release)],
            entered,
        ));
        let poll = tokio::spawn(backend.clone().poll(Arc::clone(&client), 1));

        tokio::time::sleep(Duration::from_millis(1_050)).await;
        assert!(entries.recv().await.unwrap().ends_with("/me/player"));
        let command_backend = backend.clone();
        let command_client = Arc::clone(&client);
        let command = tokio::spawn(async move {
            command_backend
                .seek_state(command_client.as_ref(), 50)
                .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), entries.recv())
                .await
                .is_err()
        );

        poll_release.notify_one();
        assert!(entries
            .recv()
            .await
            .unwrap()
            .contains("/seek?position_ms=50000"));
        seek_release.notify_one();
        assert!(command.await.unwrap().unwrap().is_some());
        poll.abort();

        assert_eq!(
            backend
                .state
                .lock()
                .await
                .context
                .as_ref()
                .unwrap()
                .previous
                .elapsed,
            50
        );
    }

    #[tokio::test]
    async fn stale_poll_failure_does_not_emit_or_clear_current_context() {
        let (events, mut receiver) = mpsc::unbounded_channel();
        let backend = ConnectBackend::new(events, 1);
        install_context(&backend).await;
        let release = Arc::new(Notify::new());
        let (entered, mut entries) = mpsc::unbounded_channel();
        let client = Arc::new(delayed_client(
            [Response::json(500, serde_json::json!({"error": "offline"}))],
            vec![Arc::clone(&release)],
            entered,
        ));
        let poll = tokio::spawn(backend.clone().poll(client, 1));

        assert!(entries.recv().await.unwrap().ends_with("/me/player"));
        backend.state.lock().await.revision += 1;
        release.notify_one();
        tokio::time::sleep(Duration::from_millis(50)).await;
        poll.abort();

        assert!(receiver.try_recv().is_err());
        assert!(backend.state.lock().await.context.is_some());
    }

    #[tokio::test]
    async fn live_repeat_change_commits_wrap_context_after_transport_completes() {
        let backend = ConnectBackend::default();
        install_context(&backend).await;
        let release = Arc::new(Notify::new());
        let (entered, mut entries) = mpsc::unbounded_channel();
        let client = Arc::new(delayed_client(
            [Response::json(204, serde_json::Value::Null)],
            vec![Arc::clone(&release)],
            entered,
        ));
        let changed = tokio::spawn({
            let backend = backend.clone();
            let client = Arc::clone(&client);
            async move {
                backend
                    .set_repeat_state(client.as_ref(), RepeatMode::All)
                    .await
            }
        });

        assert!(entries
            .recv()
            .await
            .unwrap()
            .contains("/repeat?state=context"));
        assert!(!backend.state.lock().await.context.as_ref().unwrap().wrap);
        release.notify_one();
        changed.await.unwrap().unwrap();

        let state = backend.state.lock().await;
        let context = state.context.as_ref().unwrap();
        assert!(context.wrap);
        assert!(!context.route_at_end);
        assert_eq!(context.epoch, 2);
    }

    #[tokio::test]
    async fn live_repeat_all_keeps_spotify_repeat_off_when_wrap_routes_to_a_file() {
        let backend = ConnectBackend::default();
        install_context(&backend).await;
        backend
            .state
            .lock()
            .await
            .context
            .as_mut()
            .unwrap()
            .file_in_queue = true;
        let release = Arc::new(Notify::new());
        let (entered, mut entries) = mpsc::unbounded_channel();
        let client = Arc::new(delayed_client(
            [Response::json(204, serde_json::Value::Null)],
            vec![Arc::clone(&release)],
            entered,
        ));
        let changed = tokio::spawn({
            let backend = backend.clone();
            let client = Arc::clone(&client);
            async move {
                backend
                    .set_repeat_state(client.as_ref(), RepeatMode::All)
                    .await
            }
        });

        assert!(entries.recv().await.unwrap().contains("/repeat?state=off"));
        release.notify_one();
        changed.await.unwrap().unwrap();

        let state = backend.state.lock().await;
        let context = state.context.as_ref().unwrap();
        assert!(context.route_at_end);
        assert!(!context.wrap);
    }

    #[test]
    fn repeat_modes_map_exhaustively_to_connect_values() {
        assert_eq!(connect_repeat(RepeatMode::Off), "off");
        assert_eq!(connect_repeat(RepeatMode::All), "context");
        assert_eq!(connect_repeat(RepeatMode::One), "track");
    }

    fn track(id: u64, duration_secs: u64) -> SnapshotTrack {
        SnapshotTrack {
            id,
            uri: format!("spotify:track:{id}"),
            name: format!("Track {id}"),
            art: "Artist".into(),
            alb: "Album".into(),
            duration_secs,
        }
    }

    fn snapshot(index: usize) -> Snapshot {
        Snapshot::new(vec![track(1, 100), track(2, 100)], index)
    }

    fn polled(id: Option<&str>, elapsed: u64, playing: bool) -> PolledState {
        PolledState {
            track_id: id.map(str::to_owned),
            elapsed,
            is_playing: playing,
            device_present: true,
        }
    }

    #[test]
    fn natural_completion_advances() {
        assert_eq!(
            resolve(
                polled(Some("spotify:track:1"), 96, true),
                polled(Some("spotify:track:2"), 0, true),
                &snapshot(0),
            ),
            PlaybackDecision::Advance
        );
    }

    #[test]
    fn mid_track_external_jump_is_takeover() {
        assert_eq!(
            resolve(
                polled(Some("spotify:track:1"), 40, true),
                polled(Some("spotify:track:else"), 0, true),
                &snapshot(0),
            ),
            PlaybackDecision::Takeover
        );
    }

    #[test]
    fn old_queue_transition_after_expected_end_advances_active_order() {
        assert_eq!(
            resolve(
                polled(Some("spotify:track:1"), 96, true),
                polled(Some("spotify:track:old-next"), 0, true),
                &snapshot(0),
            ),
            PlaybackDecision::Advance
        );
    }

    #[test]
    fn old_queue_transition_after_shuffled_mixed_boundary_stops_connect_run() {
        let mut snapshot = Snapshot::new(
            vec![
                track(1, 100),
                track(2, 100),
                SnapshotTrack {
                    uri: "file:///three.mp3".into(),
                    ..track(3, 100)
                },
            ],
            0,
        );
        snapshot.set_shuffle_with(true, |suffix| suffix.reverse());
        let (expected, route_at_end) = connect_snapshot(snapshot, RepeatMode::Off);

        assert!(route_at_end);
        assert_eq!(
            resolve(
                polled(Some("spotify:track:1"), 96, true),
                polled(Some("spotify:track:2"), 0, true),
                &expected,
            ),
            PlaybackDecision::Stop
        );
    }

    #[test]
    fn pause_and_resume_are_ticks() {
        let expected = snapshot(0);
        assert_eq!(
            resolve(
                polled(Some("spotify:track:1"), 10, true),
                polled(Some("spotify:track:1"), 10, false),
                &expected,
            ),
            PlaybackDecision::Tick
        );
        assert_eq!(
            resolve(
                polled(Some("spotify:track:1"), 10, false),
                polled(Some("spotify:track:1"), 11, true),
                &expected,
            ),
            PlaybackDecision::Tick
        );
    }

    #[test]
    fn paused_expected_track_at_end_advances() {
        assert_eq!(
            resolve(
                polled(Some("spotify:track:1"), 94, true),
                polled(Some("spotify:track:1"), 96, false),
                &snapshot(0),
            ),
            PlaybackDecision::Advance
        );
    }

    #[test]
    fn missing_player_after_near_end_advances() {
        let mut missing = polled(None, 0, false);
        missing.device_present = false;
        assert_eq!(
            resolve(
                polled(Some("spotify:track:1"), 96, true),
                missing,
                &snapshot(0),
            ),
            PlaybackDecision::Advance
        );
    }

    #[test]
    fn null_item_after_near_end_advances() {
        assert_eq!(
            resolve(
                polled(Some("spotify:track:1"), 96, true),
                polled(None, 0, false),
                &snapshot(0),
            ),
            PlaybackDecision::Advance
        );
    }

    #[test]
    fn stale_poll_epoch_is_discarded() {
        let expected = snapshot(0);
        let context = Context {
            previous: polled(Some("spotify:track:1"), 20, true),
            snapshot: expected,
            device_id: "desk".into(),
            volume_supported: true,
            generation: 1,
            epoch: 2,
            route_at_end: false,
            wrap: false,
            file_after_segment: false,
            file_in_queue: false,
        };

        assert_eq!(
            poll_decision(&context, polled(Some("spotify:track:else"), 0, true), 1),
            None
        );
    }

    #[test]
    fn end_of_snapshot_external_jump_is_terminal() {
        assert_eq!(
            resolve(
                polled(Some("spotify:track:2"), 99, true),
                polled(Some("spotify:track:else"), 0, true),
                &snapshot(1),
            ),
            PlaybackDecision::Stop
        );
    }

    #[test]
    fn repeat_all_advances_from_the_end_to_wrap() {
        assert_eq!(
            resolve_with_wrap(
                polled(Some("spotify:track:2"), 99, true),
                polled(Some("spotify:track:1"), 0, true),
                &snapshot(1),
                true,
            ),
            PlaybackDecision::Advance
        );
    }

    #[test]
    fn capped_queue_starts_at_selected_track_and_resets_offset() {
        let tracks = (0..250).map(|id| track(id, 100)).collect::<Vec<_>>();
        let (uris, offset) = queue_request(&tracks, 25);
        assert_eq!(uris.len(), 200);
        assert_eq!(uris.first().map(String::as_str), Some("spotify:track:25"));
        assert_eq!(uris.last().map(String::as_str), Some("spotify:track:224"));
        assert_eq!(offset, 0);

        let short = &tracks[..100];
        let (uris, offset) = queue_request(short, 25);
        assert_eq!(uris.len(), 100);
        assert_eq!(offset, 25);
    }

    #[test]
    fn mixed_queue_request_never_includes_file_uri() {
        let tracks = vec![
            track(1, 100),
            SnapshotTrack {
                uri: "file:///tmp/local.mp3".into(),
                ..track(2, 100)
            },
        ];

        let (uris, _) = queue_request(&tracks, 0);
        assert_eq!(uris, ["spotify:track:1"]);
    }

    #[test]
    fn mixed_snapshot_returns_boundary_to_controller() {
        let tracks = vec![
            track(1, 100),
            track(2, 100),
            SnapshotTrack {
                uri: "file:///tmp/local.mp3".into(),
                ..track(3, 100)
            },
        ];

        let (snapshot, route_at_end) = connect_snapshot(Snapshot::new(tracks, 0), RepeatMode::All);
        assert_eq!(
            snapshot
                .tracks
                .iter()
                .map(|track| track.uri.as_str())
                .collect::<Vec<_>>(),
            ["spotify:track:1", "spotify:track:2"]
        );
        assert!(route_at_end);
    }

    #[test]
    fn mixed_snapshot_splits_runs_in_active_shuffle_order() {
        let mut tracks = vec![
            SnapshotTrack {
                uri: "file:///one.mp3".into(),
                ..track(1, 100)
            },
            track(2, 100),
            SnapshotTrack {
                uri: "file:///three.mp3".into(),
                ..track(3, 100)
            },
            track(4, 100),
        ];
        let mut snapshot = Snapshot::new(std::mem::take(&mut tracks), 0);
        snapshot.set_shuffle_with(true, |suffix| {
            suffix.swap(0, 2);
            suffix.swap(1, 2);
        });
        snapshot.index = 2;

        let (run, route_at_end) = connect_snapshot(snapshot, RepeatMode::All);

        assert_eq!(run.current().uri, "spotify:track:2");
        assert_eq!(
            run.active_tracks()
                .iter()
                .map(|track| track.uri.as_str())
                .collect::<Vec<_>>(),
            ["spotify:track:4", "spotify:track:2"]
        );
        assert_eq!(run.index, 1);
        assert!(route_at_end);
    }

    #[tokio::test]
    async fn update_snapshot_rewrites_context_to_shuffled_active_run_without_playing() {
        let (events, _) = mpsc::unbounded_channel();
        let backend = ConnectBackend::new(events, 7);
        backend.state.lock().await.context = Some(Context {
            snapshot: Snapshot::new(vec![track(1, 100), track(2, 100)], 0),
            device_id: "desk".into(),
            volume_supported: true,
            previous: polled(Some("spotify:track:1"), 40, true),
            generation: 1,
            epoch: 7,
            route_at_end: false,
            wrap: false,
            file_after_segment: false,
            file_in_queue: false,
        });
        let mut snapshot = Snapshot::new(
            vec![
                track(1, 100),
                track(2, 100),
                SnapshotTrack {
                    uri: "file:///three.mp3".into(),
                    ..track(3, 100)
                },
                track(4, 100),
            ],
            0,
        );
        snapshot.set_shuffle_with(true, |suffix| suffix.reverse());

        backend.update_snapshot(snapshot, RepeatMode::All).await;

        let state = backend.state.lock().await;
        let context = state.context.as_ref().unwrap();
        assert_eq!(
            context
                .snapshot
                .active_tracks()
                .iter()
                .map(|track| track.uri.as_str())
                .collect::<Vec<_>>(),
            ["spotify:track:1", "spotify:track:4"]
        );
        assert_eq!(context.snapshot.index, 0);
        assert!(context.route_at_end);
        assert!(!context.wrap);
        assert_eq!(context.epoch, 8);
    }

    #[test]
    fn device_gone_is_distinct() {
        let mut now = polled(None, 0, false);
        now.device_present = false;
        assert_eq!(
            resolve(polled(Some("spotify:track:1"), 20, true), now, &snapshot(0),),
            PlaybackDecision::DeviceGone
        );
    }

    #[tokio::test]
    async fn start_prefers_active_desktop_and_plays_full_queue_at_offset() {
        let client = fake_client(
            [
                Response::json(
                    200,
                    serde_json::json!({"devices": [
                        {"id": "first", "name": "First", "is_restricted": false, "type": "Computer", "is_active": false},
                        {"id": "active", "name": "Active", "is_restricted": false, "type": "Computer", "is_active": true, "supports_volume": true}
                    ]}),
                ),
                Response::json(204, serde_json::Value::Null),
                Response::json(204, serde_json::Value::Null),
            ],
            "",
        );
        let playback = ConnectBackend::default();
        let event = playback
            .begin(
                &client,
                Snapshot::new(vec![track(1, 100), track(2, 100)], 1),
                RepeatMode::All,
            )
            .await
            .unwrap()
            .unwrap()
            .0;

        assert!(event.volume_supported);
        let requests = client.transport().requests();
        assert!(requests[1]
            .url
            .ends_with("/me/player/repeat?state=context&device_id=active"));
        assert!(requests[2]
            .url
            .ends_with("/me/player/play?device_id=active"));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&requests[2].body).unwrap(),
            serde_json::json!({
                "uris": ["spotify:track:1", "spotify:track:2"],
                "offset": {"position": 1}
            })
        );
    }

    #[tokio::test]
    async fn start_requires_desktop_client() {
        let client = fake_client(
            [Response::json(
                200,
                serde_json::json!({"devices": [{
                    "id": "phone", "name": "Phone", "is_restricted": false,
                    "type": "Smartphone", "is_active": true
                }]}),
            )],
            "",
        );

        let error = ConnectBackend::default()
            .begin(
                &client,
                Snapshot::new(vec![track(1, 100)], 0),
                RepeatMode::Off,
            )
            .await
            .unwrap_err();
        assert_eq!(error, "Open Spotify on your desktop");
    }

    #[tokio::test]
    async fn stop_without_client_keeps_active_connect_context() {
        let client = fake_client(
            [
                Response::json(
                    200,
                    serde_json::json!({"devices": [{
                        "id": "desk", "name": "Desk", "is_restricted": false,
                        "type": "Computer", "is_active": true
                    }]}),
                ),
                Response::json(204, serde_json::Value::Null),
                Response::json(204, serde_json::Value::Null),
            ],
            "",
        );
        let playback = ConnectBackend::default();
        playback
            .begin(
                &client,
                Snapshot::new(vec![track(1, 100)], 0),
                RepeatMode::Off,
            )
            .await
            .unwrap();

        assert_eq!(
            playback.stop(None).await.unwrap_err(),
            super::super::missing_spotify()
        );
        assert!(playback.state.lock().await.context.is_some());
    }

    #[tokio::test]
    async fn controls_use_explicit_player_calls_and_stop_at_the_end() {
        let mut responses = vec![Response::json(
            200,
            serde_json::json!({"devices": [{
                "id": "desk", "name": "Desk", "is_restricted": false,
                "type": "Computer", "is_active": true, "supports_volume": true
            }]}),
        )];
        responses.extend((0..9).map(|_| Response::json(204, serde_json::Value::Null)));
        let client = fake_client(responses, "");
        let playback = ConnectBackend::default();
        playback
            .begin(
                &client,
                Snapshot::new(vec![track(1, 100), track(2, 100)], 0),
                RepeatMode::Off,
            )
            .await
            .unwrap();

        assert!(playback
            .set_playing_state(&client, true)
            .await
            .unwrap()
            .is_none());
        assert!(!playback.toggle_pause(&client).await.unwrap().is_playing);
        assert!(playback
            .set_playing_state(&client, false)
            .await
            .unwrap()
            .is_none());
        assert!(playback.toggle_pause(&client).await.unwrap().is_playing);
        assert_eq!(
            playback
                .step_state(&client, 1)
                .await
                .unwrap()
                .unwrap()
                .uri
                .as_deref(),
            Some("spotify:track:2")
        );
        assert_eq!(
            playback
                .step_state(&client, -1)
                .await
                .unwrap()
                .unwrap()
                .uri
                .as_deref(),
            Some("spotify:track:1")
        );
        playback.set_volume(&client, 42).await.unwrap();
        assert_eq!(
            playback
                .step_state(&client, 1)
                .await
                .unwrap()
                .unwrap()
                .uri
                .as_deref(),
            Some("spotify:track:2")
        );
        assert_eq!(
            playback.step_state(&client, 1).await.unwrap().unwrap().uri,
            None
        );

        let requests = client.transport().requests();
        assert!(requests[1]
            .url
            .ends_with("/me/player/repeat?state=off&device_id=desk"));
        assert!(requests[3].url.ends_with("/me/player/pause?device_id=desk"));
        assert!(requests[4].url.ends_with("/me/player/play?device_id=desk"));
        assert_eq!(requests[4].body, Vec::<u8>::new());
        assert!(requests[5].url.ends_with("/me/player/play?device_id=desk"));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&requests[5].body).unwrap(),
            serde_json::json!({
                "uris": ["spotify:track:1", "spotify:track:2"],
                "offset": {"position": 1}
            })
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&requests[6].body).unwrap(),
            serde_json::json!({
                "uris": ["spotify:track:1", "spotify:track:2"],
                "offset": {"position": 0}
            })
        );
        assert!(requests[7]
            .url
            .ends_with("/me/player/volume?volume_percent=42&device_id=desk"));
        assert!(requests[9].url.ends_with("/me/player/pause?device_id=desk"));
    }
}

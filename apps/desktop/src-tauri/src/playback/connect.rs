use std::sync::{Arc, Once};

use retune_spotify::{
    client::{Device, PlayerState, SpotifyClient, Transport},
    tokens::TokenStore,
};
use tokio::sync::mpsc;

use super::{LiveClient, NeutralEvent, NeutralState, Snapshot, SnapshotTrack};

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

pub fn resolve(prev: PolledState, now: PolledState, expected: &Snapshot) -> PlaybackDecision {
    let terminal = || {
        if expected.has_next() {
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
    if now.track_id.as_ref().is_some_and(|uri| {
        expected
            .tracks
            .iter()
            .any(|track| track.uri.as_str() == uri)
    }) {
        return PlaybackDecision::Advance;
    }
    if now.track_id.is_some() {
        return PlaybackDecision::Takeover;
    }
    if near_end(&prev) {
        terminal()
    } else {
        PlaybackDecision::Takeover
    }
}

fn poll_decision(context: &Context, now: PolledState, epoch: u64) -> Option<PlaybackDecision> {
    (context.epoch == epoch).then(|| resolve(context.previous.clone(), now, &context.snapshot))
}

struct Context {
    snapshot: Snapshot,
    device_id: String,
    volume_supported: bool,
    previous: PolledState,
    generation: u64,
    epoch: u64,
    route_at_end: bool,
}

#[derive(Default)]
struct State {
    context: Option<Context>,
    generation: u64,
}

#[derive(Clone)]
pub(super) struct ConnectBackend {
    state: Arc<tokio::sync::Mutex<State>>,
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
            events,
            backend_generation,
        }
    }

    pub(super) async fn play(
        &self,
        client: Arc<LiveClient>,
        snapshot: Snapshot,
        repeat: &str,
    ) -> Result<(), String> {
        let event = self
            .begin(client.as_ref(), snapshot.tracks, snapshot.index, repeat)
            .await?;
        let generation = self
            .state
            .lock()
            .await
            .context
            .as_ref()
            .expect("start creates context")
            .generation;
        self.emit(event);
        let playback = self.clone();
        tauri::async_runtime::spawn(async move {
            playback.poll(client, generation).await;
        });
        Ok(())
    }

    async fn begin<T: Transport, S: TokenStore>(
        &self,
        client: &SpotifyClient<T, S>,
        tracks: Vec<SnapshotTrack>,
        index: usize,
        repeat: &str,
    ) -> Result<NeutralState, String> {
        if tracks.is_empty() || index >= tracks.len() {
            return Err("Choose a track to play".into());
        }
        let (snapshot, route_at_end) = connect_snapshot(Snapshot { tracks, index }, repeat);
        let tracks = snapshot.tracks;
        let index = snapshot.index;
        let device = select_device(client.devices().await.map_err(|error| error.to_string())?)?;
        let device_id = device.id.expect("selected devices have ids");
        client
            .set_repeat(
                connect_repeat(if route_at_end { "off" } else { repeat })?,
                Some(&device_id),
            )
            .await
            .map_err(|error| error.to_string())?;
        play_snapshot(client, &device_id, &tracks, index).await?;
        let mut state = self.state.lock().await;
        state.generation = state.generation.wrapping_add(1);
        let generation = state.generation;
        let snapshot = Snapshot { tracks, index };
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
        });
        Ok(event)
    }

    async fn toggle_pause<T: Transport, S: TokenStore>(
        &self,
        client: &SpotifyClient<T, S>,
    ) -> Result<NeutralState, String> {
        let mut state = self.state.lock().await;
        let context = state.context.as_mut().ok_or("Nothing is playing")?;
        let playing = !context.previous.is_playing;
        if playing {
            client.resume(Some(&context.device_id)).await
        } else {
            client.pause(Some(&context.device_id)).await
        }
        .map_err(|error| error.to_string())?;
        context.epoch = context.epoch.wrapping_add(1);
        context.previous.is_playing = playing;
        Ok(local_state(
            &context.snapshot,
            context.previous.elapsed,
            playing,
            context.volume_supported,
        ))
    }

    async fn seek_state<T: Transport, S: TokenStore>(
        &self,
        client: &SpotifyClient<T, S>,
        seconds: u64,
    ) -> Result<NeutralState, String> {
        let mut state = self.state.lock().await;
        let context = state.context.as_mut().ok_or("Nothing is playing")?;
        let seconds = seconds.min(context.snapshot.current().duration_secs);
        let position_ms = u32::try_from(seconds.saturating_mul(1000))
            .map_err(|_| "seek position out of range".to_string())?;
        client
            .seek(position_ms, Some(&context.device_id))
            .await
            .map_err(|error| error.to_string())?;
        context.epoch = context.epoch.wrapping_add(1);
        context.previous.elapsed = seconds;
        Ok(local_state(
            &context.snapshot,
            seconds,
            context.previous.is_playing,
            context.volume_supported,
        ))
    }

    async fn step_state<T: Transport, S: TokenStore>(
        &self,
        client: &SpotifyClient<T, S>,
        direction: i8,
    ) -> Result<NeutralState, String> {
        let mut state = self.state.lock().await;
        let context = state.context.as_mut().ok_or("Nothing is playing")?;
        let next = if direction < 0 {
            context.snapshot.index.saturating_sub(1)
        } else {
            context.snapshot.index + 1
        };
        if next >= context.snapshot.tracks.len() {
            client
                .pause(Some(&context.device_id))
                .await
                .map_err(|error| error.to_string())?;
            state.context = None;
            return Ok(empty_state());
        }
        context.snapshot.index = next;
        play_snapshot(
            client,
            &context.device_id,
            &context.snapshot.tracks,
            context.snapshot.index,
        )
        .await?;
        context.epoch = context.epoch.wrapping_add(1);
        context.previous = PolledState {
            track_id: Some(context.snapshot.current().uri.clone()),
            elapsed: 0,
            is_playing: true,
            device_present: true,
        };
        Ok(local_state(
            &context.snapshot,
            0,
            true,
            context.volume_supported,
        ))
    }

    #[cfg(test)]
    async fn next<T: Transport, S: TokenStore>(
        &self,
        client: &SpotifyClient<T, S>,
    ) -> Result<NeutralState, String> {
        self.step_state(client, 1).await
    }

    #[cfg(test)]
    async fn prev<T: Transport, S: TokenStore>(
        &self,
        client: &SpotifyClient<T, S>,
    ) -> Result<NeutralState, String> {
        self.step_state(client, -1).await
    }

    async fn set_volume<T: Transport, S: TokenStore>(
        &self,
        client: &SpotifyClient<T, S>,
        volume: u8,
    ) -> Result<(), String> {
        let state = self.state.lock().await;
        let context = state.context.as_ref().ok_or("Nothing is playing")?;
        if !context.volume_supported {
            return Ok(());
        }
        client
            .set_volume(volume, Some(&context.device_id))
            .await
            .map_err(|error| error.to_string())
    }

    async fn set_repeat_state<T: Transport, S: TokenStore>(
        &self,
        client: &SpotifyClient<T, S>,
        repeat: &str,
    ) -> Result<(), String> {
        let state = self.state.lock().await;
        let Some(context) = state
            .context
            .as_ref()
            .filter(|context| context.previous.is_playing)
        else {
            return Ok(());
        };
        client
            .set_repeat(connect_repeat(repeat)?, Some(&context.device_id))
            .await
            .map_err(|error| error.to_string())
    }

    async fn clear(&self) {
        self.state.lock().await.context = None;
    }

    pub(super) async fn toggle(&self, client: &LiveClient) -> Result<(), String> {
        let state = self.toggle_pause(client).await?;
        self.emit(state);
        Ok(())
    }

    pub(super) async fn seek(&self, client: &LiveClient, seconds: u64) -> Result<(), String> {
        let state = self.seek_state(client, seconds).await?;
        self.emit(state);
        Ok(())
    }

    pub(super) async fn step(&self, client: &LiveClient, direction: i8) -> Result<(), String> {
        let state = self.step_state(client, direction).await?;
        self.emit(state);
        Ok(())
    }

    pub(super) async fn set_live_volume(
        &self,
        client: &LiveClient,
        volume: u8,
    ) -> Result<(), String> {
        self.set_volume(client, volume).await
    }

    pub(super) async fn set_live_repeat(
        &self,
        client: &LiveClient,
        repeat: &str,
    ) -> Result<(), String> {
        self.set_repeat_state(client, repeat).await
    }

    pub(super) async fn stop(&self, client: Option<&LiveClient>) -> Result<(), String> {
        let mut state = self.state.lock().await;
        if state.context.is_some() && client.is_none() {
            return Err(super::missing_spotify());
        }
        if let (Some(client), Some(context)) = (client, state.context.as_ref()) {
            client
                .pause(Some(&context.device_id))
                .await
                .map_err(|error| error.to_string())?;
        }
        state.context = None;
        Ok(())
    }

    fn emit(&self, state: NeutralState) {
        let _ = self.events.send(NeutralEvent::ConnectState {
            generation: self.backend_generation,
            state,
        });
    }

    async fn poll(self, client: Arc<LiveClient>, generation: u64) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        interval.tick().await;
        loop {
            interval.tick().await;
            let epoch = {
                let state = self.state.lock().await;
                let Some(context) = state
                    .context
                    .as_ref()
                    .filter(|context| context.generation == generation)
                else {
                    return;
                };
                context.epoch
            };
            let polled = match client.player().await {
                Ok(player) => player,
                Err(error) => {
                    let _ = self.events.send(NeutralEvent::Error {
                        generation: self.backend_generation,
                        message: error.to_string(),
                    });
                    self.clear().await;
                    return;
                }
            };
            let mut state = self.state.lock().await;
            let Some(context) = state
                .context
                .as_mut()
                .filter(|context| context.generation == generation)
            else {
                return;
            };
            let now = polled_state(polled.as_ref());
            let Some(decision) = poll_decision(context, now.clone(), epoch) else {
                continue;
            };
            let event = match decision {
                PlaybackDecision::Tick => {
                    context.previous = now;
                    local_state(
                        &context.snapshot,
                        context.previous.elapsed,
                        context.previous.is_playing,
                        context.volume_supported,
                    )
                }
                PlaybackDecision::Advance => {
                    let previous_index = context.snapshot.index;
                    let polled_index = now.track_id.as_ref().and_then(|uri| {
                        context
                            .snapshot
                            .tracks
                            .iter()
                            .position(|track| &track.uri == uri)
                    });
                    context.snapshot.index = polled_index
                        .filter(|index| *index != previous_index)
                        .unwrap_or(previous_index + 1);
                    if polled_index == Some(previous_index) || polled_index.is_none() {
                        if let Err(error) = play_snapshot(
                            client.as_ref(),
                            &context.device_id,
                            &context.snapshot.tracks,
                            context.snapshot.index,
                        )
                        .await
                        {
                            let _ = self.events.send(NeutralEvent::Error {
                                generation: self.backend_generation,
                                message: error,
                            });
                            state.context = None;
                            return;
                        }
                        context.previous = PolledState {
                            track_id: Some(context.snapshot.current().uri.clone()),
                            elapsed: 0,
                            is_playing: true,
                            device_present: true,
                        };
                    } else {
                        context.previous = now;
                    }
                    local_state(
                        &context.snapshot,
                        context.previous.elapsed,
                        context.previous.is_playing,
                        context.volume_supported,
                    )
                }
                PlaybackDecision::Stop => {
                    let route_at_end = context.route_at_end;
                    let uri = context.snapshot.current().uri.clone();
                    let _ = client.pause(Some(&context.device_id)).await;
                    state.context = None;
                    drop(state);
                    if route_at_end {
                        let _ = self.events.send(NeutralEvent::ConnectBoundary {
                            generation: self.backend_generation,
                            uri,
                        });
                    } else {
                        self.emit(empty_state());
                    }
                    return;
                }
                PlaybackDecision::Takeover | PlaybackDecision::DeviceGone => {
                    state.context = None;
                    external_state(polled.as_ref())
                }
            };
            drop(state);
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

fn connect_repeat(repeat: &str) -> Result<&'static str, String> {
    match repeat {
        "off" => Ok("off"),
        "all" => Ok("context"),
        "one" => Ok("track"),
        _ => Err("repeat must be off, all, or one".into()),
    }
}

fn connect_snapshot(snapshot: Snapshot, repeat: &str) -> (Snapshot, bool) {
    let start = snapshot.tracks[..snapshot.index]
        .iter()
        .rposition(|track| track.uri.starts_with("file:"))
        .map_or(0, |index| index + 1);
    let end = snapshot.tracks[start..]
        .iter()
        .position(|track| track.uri.starts_with("file:"))
        .map_or(snapshot.tracks.len(), |offset| start + offset);
    let route_at_end = end < snapshot.tracks.len()
        || (repeat == "all"
            && snapshot
                .tracks
                .iter()
                .any(|track| track.uri.starts_with("file:")));
    (
        Snapshot {
            tracks: snapshot.tracks[start..end].to_vec(),
            index: snapshot.index - start,
        },
        route_at_end,
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

impl Default for ConnectBackend {
    fn default() -> Self {
        let (events, _) = mpsc::unbounded_channel();
        Self::new(events, 1)
    }
}

#[cfg(test)]
type Playback = ConnectBackend;

fn empty_state() -> NeutralState {
    NeutralState {
        uri: None,
        position_ms: 0,
        is_playing: false,
        external: false,
        name: None,
        art: None,
        alb: None,
        duration_ms: None,
        volume_supported: false,
    }
}

#[cfg(test)]
mod tests {
    use retune_spotify::{
        client::{FakeTransport, Response},
        tokens::{InMemoryTokenStore, Tokens},
    };

    use super::*;

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
        Snapshot {
            tracks: vec![track(1, 100), track(2, 100)],
            index,
        }
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
        };

        assert_eq!(
            poll_decision(&context, polled(Some("spotify:track:else"), 0, true), 1),
            None
        );
    }

    #[test]
    fn end_of_snapshot_takeover_is_external() {
        assert_eq!(
            resolve(
                polled(Some("spotify:track:2"), 99, true),
                polled(Some("spotify:track:else"), 0, true),
                &snapshot(1),
            ),
            PlaybackDecision::Takeover
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

        let (snapshot, route_at_end) = connect_snapshot(Snapshot { tracks, index: 0 }, "all");
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
        let transport = FakeTransport::new([
            Response::json(
                200,
                serde_json::json!({"devices": [
                    {"id": "first", "name": "First", "is_restricted": false, "type": "Computer", "is_active": false},
                    {"id": "active", "name": "Active", "is_restricted": false, "type": "Computer", "is_active": true, "supports_volume": true}
                ]}),
            ),
            Response::json(204, serde_json::Value::Null),
            Response::json(204, serde_json::Value::Null),
        ]);
        let client = SpotifyClient::new(
            "client",
            transport,
            InMemoryTokenStore::new(Some(Tokens {
                access: "access".into(),
                refresh: "refresh".into(),
                expires_at: u64::MAX,
                scopes: String::new(),
            })),
        );
        let playback = Playback::default();
        let event = playback
            .begin(&client, vec![track(1, 100), track(2, 100)], 1, "all")
            .await
            .unwrap();

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
        let transport = FakeTransport::new([Response::json(
            200,
            serde_json::json!({"devices": [{
                "id": "phone", "name": "Phone", "is_restricted": false,
                "type": "Smartphone", "is_active": true
            }]}),
        )]);
        let client = SpotifyClient::new(
            "client",
            transport,
            InMemoryTokenStore::new(Some(Tokens {
                access: "access".into(),
                refresh: "refresh".into(),
                expires_at: u64::MAX,
                scopes: String::new(),
            })),
        );

        let error = Playback::default()
            .begin(&client, vec![track(1, 100)], 0, "off")
            .await
            .unwrap_err();
        assert_eq!(error, "Open Spotify on your desktop");
    }

    #[tokio::test]
    async fn stop_without_client_keeps_active_connect_context() {
        let transport = FakeTransport::new([
            Response::json(
                200,
                serde_json::json!({"devices": [{
                    "id": "desk", "name": "Desk", "is_restricted": false,
                    "type": "Computer", "is_active": true
                }]}),
            ),
            Response::json(204, serde_json::Value::Null),
            Response::json(204, serde_json::Value::Null),
        ]);
        let client = SpotifyClient::new(
            "client",
            transport,
            InMemoryTokenStore::new(Some(Tokens {
                access: "access".into(),
                refresh: "refresh".into(),
                expires_at: u64::MAX,
                scopes: String::new(),
            })),
        );
        let playback = Playback::default();
        playback
            .begin(&client, vec![track(1, 100)], 0, "off")
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
        let client = SpotifyClient::new(
            "client",
            FakeTransport::new(responses),
            InMemoryTokenStore::new(Some(Tokens {
                access: "access".into(),
                refresh: "refresh".into(),
                expires_at: u64::MAX,
                scopes: String::new(),
            })),
        );
        let playback = Playback::default();
        playback
            .begin(&client, vec![track(1, 100), track(2, 100)], 0, "off")
            .await
            .unwrap();

        assert!(!playback.toggle_pause(&client).await.unwrap().is_playing);
        assert!(playback.toggle_pause(&client).await.unwrap().is_playing);
        assert_eq!(
            playback.next(&client).await.unwrap().uri.as_deref(),
            Some("spotify:track:2")
        );
        assert_eq!(
            playback.prev(&client).await.unwrap().uri.as_deref(),
            Some("spotify:track:1")
        );
        playback.set_volume(&client, 42).await.unwrap();
        assert_eq!(
            playback.next(&client).await.unwrap().uri.as_deref(),
            Some("spotify:track:2")
        );
        assert_eq!(playback.next(&client).await.unwrap().uri, None);

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

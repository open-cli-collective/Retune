use std::sync::Arc;

use retune_spotify::{
    client::{Device, HttpTransport, PlayerState, SpotifyClient, Transport},
    tokens::TokenStore,
};
use serde::{Deserialize, Serialize};
use tauri::Emitter;

type LiveClient = SpotifyClient<HttpTransport, crate::SharedTokenStore>;

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
pub struct Snapshot {
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
    if near_end(&prev) {
        terminal()
    } else {
        PlaybackDecision::Takeover
    }
}

fn poll_decision(context: &Context, now: PolledState, epoch: u64) -> Option<PlaybackDecision> {
    (context.epoch == epoch).then(|| resolve(context.previous.clone(), now, &context.snapshot))
}

#[derive(Clone, Debug, Serialize)]
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

struct Context {
    snapshot: Snapshot,
    device_id: String,
    volume_supported: bool,
    previous: PolledState,
    generation: u64,
    epoch: u64,
}

#[derive(Default)]
struct State {
    context: Option<Context>,
    generation: u64,
}

#[derive(Default)]
pub struct Playback {
    state: tokio::sync::Mutex<State>,
}

impl Playback {
    pub async fn start(
        self: &Arc<Self>,
        client: Arc<LiveClient>,
        app: tauri::AppHandle,
        tracks: Vec<SnapshotTrack>,
        index: usize,
    ) -> Result<PlayerStateEvent, String> {
        let event = self.begin(client.as_ref(), tracks, index).await?;
        let generation = self
            .state
            .lock()
            .await
            .context
            .as_ref()
            .expect("start creates context")
            .generation;
        let playback = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            playback.poll(client, app, generation).await;
        });
        Ok(event)
    }

    async fn begin<T: Transport, S: TokenStore>(
        &self,
        client: &SpotifyClient<T, S>,
        tracks: Vec<SnapshotTrack>,
        index: usize,
    ) -> Result<PlayerStateEvent, String> {
        if tracks.is_empty() || index >= tracks.len() {
            return Err("Choose a track to play".into());
        }
        let device = select_device(client.devices().await.map_err(|error| error.to_string())?)?;
        let device_id = device.id.expect("selected devices have ids");
        client
            .play(Some(&device_id), &[tracks[index].uri.clone()])
            .await
            .map_err(|error| error.to_string())?;
        let mut state = self.state.lock().await;
        state.generation = state.generation.wrapping_add(1);
        let generation = state.generation;
        let snapshot = Snapshot { tracks, index };
        let event = local_event(&snapshot, 0, true, device.supports_volume);
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
        });
        Ok(event)
    }

    pub async fn toggle_pause<T: Transport, S: TokenStore>(
        &self,
        client: &SpotifyClient<T, S>,
    ) -> Result<PlayerStateEvent, String> {
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
        Ok(local_event(
            &context.snapshot,
            context.previous.elapsed,
            playing,
            context.volume_supported,
        ))
    }

    pub async fn seek<T: Transport, S: TokenStore>(
        &self,
        client: &SpotifyClient<T, S>,
        seconds: u64,
    ) -> Result<PlayerStateEvent, String> {
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
        Ok(local_event(
            &context.snapshot,
            seconds,
            context.previous.is_playing,
            context.volume_supported,
        ))
    }

    pub async fn next<T: Transport, S: TokenStore>(
        &self,
        client: &SpotifyClient<T, S>,
    ) -> Result<PlayerStateEvent, String> {
        self.step(client, 1).await
    }

    pub async fn prev<T: Transport, S: TokenStore>(
        &self,
        client: &SpotifyClient<T, S>,
    ) -> Result<PlayerStateEvent, String> {
        self.step(client, -1).await
    }

    async fn step<T: Transport, S: TokenStore>(
        &self,
        client: &SpotifyClient<T, S>,
        direction: i8,
    ) -> Result<PlayerStateEvent, String> {
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
            return Ok(empty_event(false));
        }
        context.snapshot.index = next;
        client
            .play(
                Some(&context.device_id),
                &[context.snapshot.current().uri.clone()],
            )
            .await
            .map_err(|error| error.to_string())?;
        context.epoch = context.epoch.wrapping_add(1);
        context.previous = PolledState {
            track_id: Some(context.snapshot.current().uri.clone()),
            elapsed: 0,
            is_playing: true,
            device_present: true,
        };
        Ok(local_event(
            &context.snapshot,
            0,
            true,
            context.volume_supported,
        ))
    }

    pub async fn set_volume<T: Transport, S: TokenStore>(
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

    pub async fn clear(&self) {
        self.state.lock().await.context = None;
    }

    async fn poll(
        self: Arc<Self>,
        client: Arc<LiveClient>,
        app: tauri::AppHandle,
        generation: u64,
    ) {
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
                    let _ = app.emit("operation-error", error.to_string());
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
                    local_event(
                        &context.snapshot,
                        context.previous.elapsed,
                        context.previous.is_playing,
                        context.volume_supported,
                    )
                }
                PlaybackDecision::Advance => {
                    context.snapshot.index += 1;
                    if let Err(error) = client
                        .play(
                            Some(&context.device_id),
                            &[context.snapshot.current().uri.clone()],
                        )
                        .await
                    {
                        let _ = app.emit("operation-error", error.to_string());
                        state.context = None;
                        return;
                    }
                    context.previous = PolledState {
                        track_id: Some(context.snapshot.current().uri.clone()),
                        elapsed: 0,
                        is_playing: true,
                        device_present: true,
                    };
                    local_event(&context.snapshot, 0, true, context.volume_supported)
                }
                PlaybackDecision::Stop => {
                    let _ = client.pause(Some(&context.device_id)).await;
                    state.context = None;
                    empty_event(false)
                }
                PlaybackDecision::Takeover | PlaybackDecision::DeviceGone => {
                    state.context = None;
                    external_event(polled.as_ref())
                }
            };
            drop(state);
            let _ = app.emit("player-state", event);
            if matches!(
                decision,
                PlaybackDecision::Stop | PlaybackDecision::Takeover | PlaybackDecision::DeviceGone
            ) {
                return;
            }
        }
    }
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

fn local_event(
    snapshot: &Snapshot,
    elapsed: u64,
    is_playing: bool,
    volume_supported: bool,
) -> PlayerStateEvent {
    let track = snapshot.current();
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

fn external_event(player: Option<&PlayerState>) -> PlayerStateEvent {
    let item = player.and_then(|state| state.item.as_ref());
    PlayerStateEvent {
        track_id: None,
        elapsed: player.and_then(|state| state.progress_ms).unwrap_or(0) / 1000,
        is_playing: player.is_some_and(|state| state.is_playing),
        external: true,
        name: item.map(|track| track.name.clone()),
        art: item.and_then(|track| track.artists.first().map(|artist| artist.name.clone())),
        alb: item.and_then(|track| track.album.as_ref().map(|album| album.name.clone())),
        duration_secs: item.and_then(|track| track.duration_ms).map(|ms| ms / 1000),
        volume_supported: false,
    }
}

fn empty_event(external: bool) -> PlayerStateEvent {
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
                polled(Some("spotify:track:else"), 0, true),
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
        };

        assert_eq!(
            poll_decision(&context, polled(Some("spotify:track:else"), 0, true), 1),
            None
        );
    }

    #[test]
    fn end_of_snapshot_stops() {
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
    fn device_gone_is_distinct() {
        let mut now = polled(None, 0, false);
        now.device_present = false;
        assert_eq!(
            resolve(polled(Some("spotify:track:1"), 20, true), now, &snapshot(0),),
            PlaybackDecision::DeviceGone
        );
    }

    #[tokio::test]
    async fn start_prefers_active_desktop_and_plays_explicit_track() {
        let transport = FakeTransport::new([
            Response::json(
                200,
                serde_json::json!({"devices": [
                    {"id": "first", "name": "First", "is_restricted": false, "type": "Computer", "is_active": false},
                    {"id": "active", "name": "Active", "is_restricted": false, "type": "Computer", "is_active": true, "supports_volume": true}
                ]}),
            ),
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
            .begin(&client, vec![track(1, 100)], 0)
            .await
            .unwrap();

        assert!(event.volume_supported);
        let requests = client.transport().requests();
        assert!(requests[1]
            .url
            .ends_with("/me/player/play?device_id=active"));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&requests[1].body).unwrap(),
            serde_json::json!({"uris": ["spotify:track:1"]})
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
            .begin(&client, vec![track(1, 100)], 0)
            .await
            .unwrap_err();
        assert_eq!(error, "Open Spotify on your desktop");
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
        responses.extend((0..8).map(|_| Response::json(204, serde_json::Value::Null)));
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
            .begin(&client, vec![track(1, 100), track(2, 100)], 0)
            .await
            .unwrap();

        assert!(!playback.toggle_pause(&client).await.unwrap().is_playing);
        assert!(playback.toggle_pause(&client).await.unwrap().is_playing);
        assert_eq!(playback.next(&client).await.unwrap().track_id, Some(2));
        assert_eq!(playback.prev(&client).await.unwrap().track_id, Some(1));
        playback.set_volume(&client, 42).await.unwrap();
        assert_eq!(playback.next(&client).await.unwrap().track_id, Some(2));
        assert_eq!(playback.next(&client).await.unwrap().track_id, None);

        let requests = client.transport().requests();
        assert!(requests[2].url.ends_with("/me/player/pause?device_id=desk"));
        assert!(requests[3].url.ends_with("/me/player/play?device_id=desk"));
        assert_eq!(requests[3].body, Vec::<u8>::new());
        assert!(requests[4].url.ends_with("/me/player/play?device_id=desk"));
        assert!(requests[6]
            .url
            .ends_with("/me/player/volume?volume_percent=42&device_id=desk"));
        assert!(requests[8].url.ends_with("/me/player/pause?device_id=desk"));
    }
}

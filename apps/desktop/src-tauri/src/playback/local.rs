use std::{path::Path, sync::Arc, time::Duration};

use librespot_audio::AudioFetchParams;
use librespot_core::{
    authentication::Credentials, cache::Cache, config::SessionConfig, session::Session,
    spotify_uri::SpotifyUri,
};
use librespot_playback::{
    audio_backend,
    config::{AudioFormat, Bitrate, PlayerConfig, VolumeCtrl},
    mixer::{self, Mixer, MixerConfig},
    player::{Player, PlayerEvent},
};
use tokio::sync::mpsc;

use super::{AudioSettings, LiveClient, NeutralEvent, Snapshot};

struct Runtime {
    session: Session,
    player: Arc<Player>,
    mixer: Arc<dyn Mixer>,
}

pub(super) struct LocalBackend {
    runtime: Option<Runtime>,
    generation: u64,
    snapshot: Option<Snapshot>,
    playing: bool,
    volume: u8,
}

impl LocalBackend {
    #[cfg(test)]
    pub(super) fn with_snapshot_for_test(snapshot: Snapshot) -> Self {
        Self {
            runtime: None,
            generation: 1,
            snapshot: Some(snapshot),
            playing: true,
            volume: 62,
        }
    }

    #[cfg(test)]
    pub(super) fn has_snapshot(&self) -> bool {
        self.snapshot.is_some()
    }

    pub(super) async fn activate(
        client: &LiveClient,
        events: mpsc::UnboundedSender<NeutralEvent>,
        generation: u64,
        volume: u8,
        cache_dir: Option<&Path>,
        audio: AudioSettings,
    ) -> Result<Self, String> {
        let access = client
            .access_token()
            .await
            .map_err(|error| error.to_string())?;
        // Read farther ahead to survive short network stalls.
        let _ = AudioFetchParams::set(AudioFetchParams {
            read_ahead_during_playback: Duration::from_secs(30),
            ..AudioFetchParams::default()
        });
        let cache = cache_dir.and_then(audio_cache);
        let session = Session::new(SessionConfig::default(), cache);
        session
            .connect(Credentials::with_access_token(access), false)
            .await
            .map_err(|error| error.to_string())?;

        let mixer = mixer::find(None)
            .ok_or_else(|| "librespot soft mixer is unavailable.".to_string())?(
            MixerConfig {
                volume_ctrl: VolumeCtrl::Linear,
                ..MixerConfig::default()
            },
        )
        .map_err(|error| error.to_string())?;
        mixer.set_volume(soft_volume(volume));
        let volume_getter = mixer.get_soft_volume();
        let sink = audio_backend::find(Some("rodio".into()))
            .ok_or_else(|| "librespot rodio output is unavailable.".to_string())?;
        let player = Player::new(
            PlayerConfig {
                bitrate: bitrate(audio.bitrate),
                gapless: audio.gapless,
                normalisation: audio.normalize,
                position_update_interval: Some(Duration::from_secs(1)),
                ..PlayerConfig::default()
            },
            session.clone(),
            volume_getter,
            move || sink(None, AudioFormat::default()),
        );
        let receiver = player.get_player_event_channel();
        monitor(
            receiver,
            session.clone(),
            Arc::clone(&player),
            events,
            generation,
        );
        Ok(Self {
            runtime: Some(Runtime {
                session,
                player,
                mixer,
            }),
            generation,
            snapshot: None,
            playing: false,
            volume,
        })
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    pub(super) fn volume(&self) -> u8 {
        self.volume
    }

    pub(super) fn is_invalid(&self) -> bool {
        self.runtime
            .as_ref()
            .is_none_or(|runtime| runtime.session.is_invalid() || runtime.player.is_invalid())
    }

    pub(super) fn play(
        &mut self,
        snapshot: Snapshot,
        start_playing: bool,
        position_ms: u32,
    ) -> Result<(), String> {
        let uri = snapshot.current().uri.clone();
        self.snapshot = Some(snapshot);
        self.load(&uri, start_playing, position_ms)
    }

    pub(super) fn load(
        &mut self,
        uri: &str,
        start_playing: bool,
        position_ms: u32,
    ) -> Result<(), String> {
        let uri = SpotifyUri::from_uri(uri).map_err(|error| error.to_string())?;
        let runtime = self
            .runtime
            .as_ref()
            .ok_or("Local playback is unavailable")?;
        runtime.player.load(uri, start_playing, position_ms);
        self.playing = start_playing;
        Ok(())
    }

    pub(super) fn toggle(&mut self) -> Result<(), String> {
        let runtime = self.runtime.as_ref().ok_or("Nothing is playing")?;
        if self.snapshot.is_none() {
            return Err("Nothing is playing".into());
        }
        self.playing = !self.playing;
        if self.playing {
            runtime.player.play();
        } else {
            runtime.player.pause();
        }
        Ok(())
    }

    pub(super) fn seek(&self, seconds: u64) -> Result<(), String> {
        let runtime = self.runtime.as_ref().ok_or("Nothing is playing")?;
        let duration = self
            .snapshot
            .as_ref()
            .ok_or("Nothing is playing")?
            .current()
            .duration_secs;
        let position_ms = u32::try_from(seconds.min(duration).saturating_mul(1000))
            .map_err(|_| "seek position out of range".to_string())?;
        runtime.player.seek(position_ms);
        Ok(())
    }

    pub(super) fn set_volume(&mut self, volume: u8) -> Result<(), String> {
        let runtime = self
            .runtime
            .as_ref()
            .ok_or("Local playback is unavailable")?;
        runtime.mixer.set_volume(soft_volume(volume));
        self.volume = volume;
        Ok(())
    }

    pub(super) fn stop(&mut self) {
        if let Some(runtime) = &self.runtime {
            runtime.player.stop();
        }
        self.snapshot = None;
        self.playing = false;
    }

    pub(super) fn teardown(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.player.stop();
            runtime.session.shutdown();
        }
        self.playing = false;
    }
}

fn bitrate(value: u16) -> Bitrate {
    match value {
        96 => Bitrate::Bitrate96,
        160 => Bitrate::Bitrate160,
        _ => Bitrate::Bitrate320,
    }
}

fn audio_cache(app_data_dir: &Path) -> Option<Cache> {
    let audio_path = app_data_dir.join("audio-cache");
    match Cache::new(
        None::<&Path>,
        None::<&Path>,
        Some(audio_path.as_path()),
        Some(2 * 1024 * 1024 * 1024),
    ) {
        Ok(cache) => Some(cache),
        Err(error) => {
            log::warn!("Audio cache unavailable: {error}");
            None
        }
    }
}

impl Drop for LocalBackend {
    fn drop(&mut self) {
        self.teardown();
    }
}

pub(super) fn soft_volume(volume: u8) -> u16 {
    ((u32::from(volume.min(100)) * u32::from(u16::MAX)) / 100) as u16
}

fn monitor(
    mut receiver: mpsc::UnboundedReceiver<PlayerEvent>,
    session: Session,
    player: Arc<Player>,
    events: mpsc::UnboundedSender<NeutralEvent>,
    generation: u64,
) {
    tauri::async_runtime::spawn(async move {
        let mut health = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                event = receiver.recv() => {
                    let Some(event) = event else { return };
                    if let Some(event) = neutral_event(event, generation) {
                        let _ = events.send(event);
                    }
                }
                _ = health.tick() => {
                    if session.is_invalid() || player.is_invalid() {
                        let _ = events.send(NeutralEvent::Disconnected { generation });
                        return;
                    }
                }
            }
        }
    });
}

fn neutral_event(event: PlayerEvent, generation: u64) -> Option<NeutralEvent> {
    let uri = |track_id: SpotifyUri| track_id.to_uri().ok();
    match event {
        PlayerEvent::PlayRequestIdChanged { play_request_id } => {
            Some(NeutralEvent::RequestIdChanged {
                generation,
                request_id: play_request_id,
            })
        }
        PlayerEvent::Loading {
            play_request_id,
            track_id,
            position_ms,
        } => Some(NeutralEvent::Loading {
            generation,
            request_id: play_request_id,
            uri: uri(track_id)?,
            position_ms,
        }),
        PlayerEvent::Playing {
            play_request_id,
            track_id,
            position_ms,
        } => Some(NeutralEvent::Playing {
            generation,
            request_id: play_request_id,
            uri: uri(track_id)?,
            position_ms,
        }),
        PlayerEvent::Paused {
            play_request_id,
            track_id,
            position_ms,
        } => Some(NeutralEvent::Paused {
            generation,
            request_id: play_request_id,
            uri: uri(track_id)?,
            position_ms,
        }),
        PlayerEvent::PositionChanged {
            play_request_id,
            track_id,
            position_ms,
        } => Some(NeutralEvent::PositionChanged {
            generation,
            request_id: play_request_id,
            uri: uri(track_id)?,
            position_ms,
        }),
        PlayerEvent::Seeked {
            play_request_id,
            track_id,
            position_ms,
        } => Some(NeutralEvent::Seeked {
            generation,
            request_id: play_request_id,
            uri: uri(track_id)?,
            position_ms,
        }),
        PlayerEvent::PositionCorrection {
            play_request_id,
            track_id,
            position_ms,
        } => Some(NeutralEvent::PositionCorrection {
            generation,
            request_id: play_request_id,
            uri: uri(track_id)?,
            position_ms,
        }),
        PlayerEvent::Unavailable {
            play_request_id,
            track_id,
        } => Some(NeutralEvent::Unavailable {
            generation,
            request_id: play_request_id,
            uri: uri(track_id)?,
        }),
        PlayerEvent::Stopped {
            play_request_id,
            track_id,
        } => Some(NeutralEvent::Stopped {
            generation,
            request_id: play_request_id,
            uri: uri(track_id)?,
        }),
        PlayerEvent::EndOfTrack {
            play_request_id,
            track_id,
        } => Some(NeutralEvent::EndOfTrack {
            generation,
            request_id: play_request_id,
            uri: uri(track_id)?,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_streaming_bitrate() {
        assert_eq!(bitrate(96), Bitrate::Bitrate96);
        assert_eq!(bitrate(160), Bitrate::Bitrate160);
        assert_eq!(bitrate(320), Bitrate::Bitrate320);
        assert_eq!(bitrate(0), Bitrate::Bitrate320);
    }
}

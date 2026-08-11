use std::{path::Path, sync::Arc, time::Duration};

use librespot_audio::AudioFetchParams;
use librespot_core::{
    authentication::Credentials,
    cache::Cache,
    config::SessionConfig,
    error::{Error as LibrespotError, ErrorKind},
    session::Session,
    spotify_uri::SpotifyUri,
};
use librespot_playback::{
    audio_backend,
    config::{AudioFormat, Bitrate, PlayerConfig, VolumeCtrl},
    mixer::{self, Mixer, MixerConfig},
    player::{Player, PlayerEvent},
};
use librespot_protocol::authentication::AuthenticationType;
use retune_spotify::tokens::TokenStore;
use tokio::sync::mpsc;

use super::{
    AudioSettings, LiveClient, NeutralEvent, PlaybackAuthorizationReason, PlaybackError, Snapshot,
};

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
    ) -> Result<Self, PlaybackError> {
        let credentials = stored_credentials(client)?;
        // Read farther ahead to survive short network stalls.
        let _ = AudioFetchParams::set(AudioFetchParams {
            read_ahead_during_playback: Duration::from_secs(30),
            ..AudioFetchParams::default()
        });
        let cache = cache_dir.and_then(audio_cache);
        let session = Session::new(SessionConfig::default(), cache);
        session
            .connect(credentials, false)
            .await
            .map_err(|error| session_error(client, error))?;
        if let Err(error) = session.login5().auth_token().await {
            session.shutdown();
            return Err(session_error(client, error));
        }

        let mixer = mixer::find(None)
            .ok_or_else(|| PlaybackError::message("librespot soft mixer is unavailable."))?(
            MixerConfig {
                volume_ctrl: VolumeCtrl::Linear,
                ..MixerConfig::default()
            },
        )
        .map_err(|error| PlaybackError::message(error.to_string()))?;
        mixer.set_volume(soft_volume(volume));
        let volume_getter = mixer.get_soft_volume();
        let sink = audio_backend::find(Some("rodio".into()))
            .ok_or_else(|| PlaybackError::message("librespot rodio output is unavailable."))?;
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
        monitor(receiver, events, generation);
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

    pub(super) fn player_is_invalid(&self) -> bool {
        self.runtime
            .as_ref()
            .is_none_or(|runtime| runtime.player.is_invalid())
    }

    pub(super) fn session_is_invalid(&self) -> bool {
        self.runtime
            .as_ref()
            .is_none_or(|runtime| runtime.session.is_invalid())
    }

    pub(super) async fn refresh_session(
        &mut self,
        client: &LiveClient,
    ) -> Result<(), PlaybackError> {
        let credentials = stored_credentials(client)?;
        let (config, cache) = {
            let runtime = self
                .runtime
                .as_ref()
                .ok_or_else(|| PlaybackError::message("Local playback is unavailable"))?;
            (
                runtime.session.config().clone(),
                runtime.session.cache().map(|cache| cache.as_ref().clone()),
            )
        };
        let session = Session::new(config, cache);
        session
            .connect(credentials, false)
            .await
            .map_err(|error| session_error(client, error))?;
        if let Err(error) = session.login5().auth_token().await {
            session.shutdown();
            return Err(session_error(client, error));
        }
        let runtime = self
            .runtime
            .as_mut()
            .ok_or_else(|| PlaybackError::message("Local playback is unavailable"))?;
        if runtime.player.is_invalid() {
            session.shutdown();
            return Err(PlaybackError::message(
                "Local playback stopped while reconnecting to Spotify",
            ));
        }
        runtime.player.set_session(session.clone());
        runtime.session = session;
        Ok(())
    }

    pub(super) async fn preflight(&self, client: &LiveClient) -> Result<(), PlaybackError> {
        let runtime = self
            .runtime
            .as_ref()
            .ok_or_else(|| PlaybackError::message("Local playback is unavailable"))?;
        runtime
            .session
            .login5()
            .auth_token()
            .await
            .map(|_| ())
            .map_err(|error| session_error(client, error))
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

    pub(super) fn preload(&self, uri: &str) -> Result<bool, String> {
        let uri = SpotifyUri::from_uri(uri).map_err(|error| error.to_string())?;
        if !uri.is_playable() {
            return Ok(false);
        }
        let runtime = self
            .runtime
            .as_ref()
            .ok_or("Local playback is unavailable")?;
        runtime.player.preload(uri);
        Ok(true)
    }

    pub(super) fn toggle(&mut self) -> Result<(), String> {
        self.set_playing(!self.playing)
    }

    pub(super) fn set_playing(&mut self, playing: bool) -> Result<(), String> {
        if self.playing == playing {
            return Ok(());
        }
        let runtime = self.runtime.as_ref().ok_or("Nothing is playing")?;
        if self.snapshot.is_none() {
            return Err("Nothing is playing".into());
        }
        self.playing = playing;
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

fn stored_credentials(client: &LiveClient) -> Result<Credentials, PlaybackError> {
    let tokens = client
        .token_store()
        .load()
        .map_err(|error| PlaybackError::message(error.to_string()))?;
    let credentials = tokens
        .and_then(|tokens| tokens.playback_credentials)
        .filter(|credentials| !credentials.username.is_empty() && !credentials.auth_data.is_empty())
        .ok_or_else(|| PlaybackError::authorization(PlaybackAuthorizationReason::Missing))?;
    Ok(Credentials {
        username: Some(credentials.username),
        auth_type: AuthenticationType::AUTHENTICATION_STORED_SPOTIFY_CREDENTIALS,
        auth_data: credentials.auth_data,
    })
}

fn session_error(client: &LiveClient, error: LibrespotError) -> PlaybackError {
    if !matches!(
        error.kind,
        ErrorKind::PermissionDenied | ErrorKind::Unauthenticated | ErrorKind::FailedPrecondition
    ) {
        return PlaybackError::message(error.to_string());
    }
    log::warn!(
        "Spotify playback authorization rejected during session verification; clearing stored credential (kind={:?}, error={error:?})",
        error.kind
    );
    match client.token_store().load() {
        Ok(Some(mut tokens)) => {
            tokens.playback_credentials = None;
            match client.token_store().save(&tokens) {
                Ok(()) => PlaybackError::authorization(PlaybackAuthorizationReason::Rejected),
                Err(clear_error) => PlaybackError::message(format!(
                    "Spotify playback authorization was rejected and could not be cleared: {clear_error}"
                )),
            }
        }
        Ok(None) => PlaybackError::authorization(PlaybackAuthorizationReason::Rejected),
        Err(load_error) => PlaybackError::message(format!(
            "Spotify playback authorization was rejected and could not be cleared: {load_error}"
        )),
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
    events: mpsc::UnboundedSender<NeutralEvent>,
    generation: u64,
) {
    tauri::async_runtime::spawn(async move {
        while let Some(event) = receiver.recv().await {
            if let PlayerEvent::Preloading { track_id } = &event {
                if let Ok(uri) = track_id.to_uri() {
                    log::info!("Spotify playback operation=preload ready: generation={generation} uri={uri}");
                }
            }
            if let PlayerEvent::Unavailable {
                play_request_id,
                track_id,
            } = &event
            {
                match track_id.to_uri() {
                    Ok(uri) => log::warn!(
                        "Spotify playback operation=load unavailable: generation={generation} request_id={play_request_id} uri={uri}"
                    ),
                    Err(_) => log::warn!(
                        "Spotify playback operation=load unavailable: generation={generation} request_id={play_request_id}"
                    ),
                }
            }
            if let Some(event) = neutral_event(event, generation) {
                let _ = events.send(event);
            }
        }
        let _ = events.send(NeutralEvent::Disconnected { generation });
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
        PlayerEvent::TimeToPreloadNextTrack {
            play_request_id,
            track_id,
        } => Some(NeutralEvent::PreloadSuggested {
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
    use retune_spotify::{
        client::{HttpTransport, SpotifyClient},
        tokens::{CachedTokenStore, InMemoryTokenStore, PlaybackCredentials, Tokens},
    };

    #[tokio::test]
    async fn monitor_reports_only_player_event_channel_closure() {
        let (player_events, receiver) = mpsc::unbounded_channel();
        let (events, mut event_receiver) = mpsc::unbounded_channel();
        monitor(receiver, events, 7);

        tokio::task::yield_now().await;
        assert!(event_receiver.try_recv().is_err());
        drop(player_events);

        assert!(matches!(
            event_receiver.recv().await,
            Some(NeutralEvent::Disconnected { generation: 7 })
        ));
    }

    #[test]
    fn maps_preload_suggestion() {
        let track_id = SpotifyUri::from_uri("spotify:track:5sWHDYs0csV6RS48xBl0tH").unwrap();
        assert!(matches!(
            neutral_event(
                PlayerEvent::TimeToPreloadNextTrack {
                    play_request_id: 42,
                    track_id,
                },
                7,
            ),
            Some(NeutralEvent::PreloadSuggested {
                generation: 7,
                request_id: 42,
                ref uri,
            }) if uri == "spotify:track:5sWHDYs0csV6RS48xBl0tH"
        ));
    }

    #[test]
    fn maps_streaming_bitrate() {
        assert_eq!(bitrate(96), Bitrate::Bitrate96);
        assert_eq!(bitrate(160), Bitrate::Bitrate160);
        assert_eq!(bitrate(320), Bitrate::Bitrate320);
        assert_eq!(bitrate(0), Bitrate::Bitrate320);
    }

    #[test]
    fn semantic_playback_rejection_clears_only_playback_credentials() {
        let tokens: Box<dyn TokenStore> = Box::new(InMemoryTokenStore::new(Some(Tokens {
            access: "web-access".into(),
            refresh: "web-refresh".into(),
            expires_at: 0,
            scopes: "user-library-read".into(),
            playback_credentials: Some(PlaybackCredentials {
                username: "user".into(),
                auth_data: vec![1, 2, 3],
            }),
        })));
        let store = Arc::new(CachedTokenStore::new(tokens));
        let client = SpotifyClient::new("test", HttpTransport::new(), Arc::clone(&store));
        let error = LibrespotError::new(
            ErrorKind::PermissionDenied,
            std::io::Error::other("rejected"),
        );

        assert!(matches!(
            session_error(&client, error),
            PlaybackError::AuthorizationRequired {
                reason: PlaybackAuthorizationReason::Rejected,
                ..
            }
        ));
        let saved = store.load().unwrap().unwrap();
        assert_eq!(saved.access, "web-access");
        assert_eq!(saved.refresh, "web-refresh");
        assert!(saved.playback_credentials.is_none());
    }

    #[test]
    fn transient_session_error_keeps_playback_credentials() {
        let tokens: Box<dyn TokenStore> = Box::new(InMemoryTokenStore::new(Some(Tokens {
            access: "web-access".into(),
            refresh: "web-refresh".into(),
            expires_at: 0,
            scopes: "user-library-read".into(),
            playback_credentials: Some(PlaybackCredentials {
                username: "user".into(),
                auth_data: vec![1, 2, 3],
            }),
        })));
        let store = Arc::new(CachedTokenStore::new(tokens));
        let client = SpotifyClient::new("test", HttpTransport::new(), Arc::clone(&store));
        let error = LibrespotError::new(
            ErrorKind::Unavailable,
            std::io::Error::other("network unavailable"),
        );

        assert!(matches!(
            session_error(&client, error),
            PlaybackError::Message(_)
        ));
        assert!(store
            .load()
            .unwrap()
            .unwrap()
            .playback_credentials
            .is_some());
    }
}

use std::{sync::Mutex, time::Duration};

use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
};
use tauri::{Emitter, Manager};

use crate::{playback::PlayerStateEvent, AppState};

pub struct MediaKeys {
    controls: Option<Mutex<MediaControlsState>>,
}

struct MediaControlsState {
    controls: MediaControls,
    metadata_key: Option<MetadataKey>,
}

type MetadataKey = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<u64>,
);

impl MediaKeys {
    pub fn spawn(app: tauri::AppHandle) -> Self {
        let result = MediaControls::new(PlatformConfig {
            dbus_name: "retune",
            display_name: "Retune",
            hwnd: None,
        })
        .and_then(|mut controls| {
            controls.attach(move |event| handle_control(&app, event))?;
            Ok(controls)
        });

        match result {
            Ok(controls) => Self {
                controls: Some(Mutex::new(MediaControlsState {
                    controls,
                    metadata_key: None,
                })),
            },
            Err(error) => {
                log::warn!("Media key setup failed: {error}");
                Self { controls: None }
            }
        }
    }

    pub fn update(&self, event: &PlayerStateEvent) -> bool {
        let Some(controls) = &self.controls else {
            return false;
        };
        let mut state = controls.lock().expect("media controls mutex poisoned");
        let metadata_key = metadata_key(event);
        let mut metadata_changed = false;
        if state.metadata_key != metadata_key {
            let metadata = if metadata_key.is_some() {
                MediaMetadata {
                    title: event.name.as_deref(),
                    artist: event.art.as_deref(),
                    album: event.alb.as_deref(),
                    duration: event.duration_secs.map(Duration::from_secs),
                    cover_url: None,
                }
            } else {
                MediaMetadata::default()
            };
            if let Err(error) = state.controls.set_metadata(metadata) {
                log::warn!("Media metadata update failed: {error}");
            } else {
                state.metadata_key = metadata_key;
                metadata_changed = true;
            }
        }
        if let Err(error) = state.controls.set_playback(media_playback(event)) {
            log::warn!("Media playback state update failed: {error}");
        }
        metadata_changed
    }

    pub fn update_artwork(&self, event: &PlayerStateEvent, url: &str) {
        let Some(controls) = &self.controls else {
            return;
        };
        let mut state = controls.lock().expect("media controls mutex poisoned");
        if state.metadata_key != metadata_key(event) {
            return;
        }
        if let Err(error) = state.controls.set_metadata(MediaMetadata {
            title: event.name.as_deref(),
            artist: event.art.as_deref(),
            album: event.alb.as_deref(),
            duration: event.duration_secs.map(Duration::from_secs),
            cover_url: Some(url),
        }) {
            log::warn!("Media artwork update failed: {error}");
        }
    }
}

#[derive(Clone, Copy)]
enum PlaybackCommand {
    Toggle,
    Next,
    Previous,
    Seek(u64),
}

fn handle_control(app: &tauri::AppHandle, event: MediaControlEvent) {
    let command = match event {
        MediaControlEvent::Play | MediaControlEvent::Pause | MediaControlEvent::Toggle => {
            PlaybackCommand::Toggle
        }
        MediaControlEvent::Next => PlaybackCommand::Next,
        MediaControlEvent::Previous => PlaybackCommand::Previous,
        MediaControlEvent::SetPosition(MediaPosition(position)) => {
            PlaybackCommand::Seek(position.as_secs())
        }
        _ => return,
    };
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        let client = crate::provider_from(&state).ok();
        let result = match command {
            PlaybackCommand::Toggle => state.playback.toggle(client.as_deref()).await,
            PlaybackCommand::Next => state.playback.next(client.as_deref()).await,
            PlaybackCommand::Previous => state.playback.prev(client.as_deref()).await,
            PlaybackCommand::Seek(seconds) => state.playback.seek(client.as_deref(), seconds).await,
        };
        if let Err(error) = result {
            let _ = app.emit("operation-error", error);
        }
    });
}

fn metadata_key(event: &PlayerStateEvent) -> Option<MetadataKey> {
    has_track(event).then(|| {
        (
            event.uri.clone(),
            event.name.clone(),
            event.art.clone(),
            event.alb.clone(),
            event.duration_secs,
        )
    })
}

fn has_track(event: &PlayerStateEvent) -> bool {
    event.track_id.is_some() || event.name.is_some()
}

fn media_playback(event: &PlayerStateEvent) -> MediaPlayback {
    if !has_track(event) {
        return MediaPlayback::Stopped;
    }
    let progress = Some(MediaPosition(Duration::from_secs(event.elapsed)));
    if event.is_playing {
        MediaPlayback::Playing { progress }
    } else {
        MediaPlayback::Paused { progress }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(track_id: Option<u64>, name: Option<&str>, is_playing: bool) -> PlayerStateEvent {
        PlayerStateEvent {
            track_id,
            uri: track_id.map(|id| format!("spotify:track:{id}")),
            elapsed: 42,
            is_playing,
            external: false,
            name: name.map(str::to_owned),
            art: None,
            alb: None,
            duration_secs: None,
            volume_supported: false,
        }
    }

    #[test]
    fn maps_playback_state() {
        assert_eq!(
            media_playback(&event(None, None, false)),
            MediaPlayback::Stopped
        );
        assert_eq!(
            media_playback(&event(Some(1), Some("Track"), true)),
            MediaPlayback::Playing {
                progress: Some(MediaPosition(Duration::from_secs(42)))
            }
        );
        assert_eq!(
            media_playback(&event(Some(1), Some("Track"), false)),
            MediaPlayback::Paused {
                progress: Some(MediaPosition(Duration::from_secs(42)))
            }
        );
    }

    #[test]
    fn keys_metadata_by_track_fields() {
        assert_eq!(metadata_key(&event(None, None, false)), None);

        let first = event(Some(1), Some("Track"), true);
        let renamed = event(Some(1), Some("Renamed"), true);
        let mut progressed = first.clone();
        progressed.elapsed += 1;

        assert_ne!(metadata_key(&first), metadata_key(&renamed));
        assert_eq!(metadata_key(&first), metadata_key(&progressed));
    }
}

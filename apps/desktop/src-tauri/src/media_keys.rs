use std::{sync::Mutex, time::Duration};

use souvlaki::{MediaControlEvent, MediaControls, MediaPlayback, MediaPosition, PlatformConfig};
use tauri::{Emitter, Manager};

use crate::{playback::PlayerStateEvent, AppState};

pub struct MediaKeys {
    controls: Option<Mutex<MediaControls>>,
}

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
                controls: Some(Mutex::new(controls)),
            },
            Err(error) => {
                log::warn!("Media key setup failed: {error}");
                Self { controls: None }
            }
        }
    }

    pub fn update(&self, event: &PlayerStateEvent) {
        let Some(controls) = &self.controls else {
            return;
        };
        if let Err(error) = controls
            .lock()
            .expect("media controls mutex poisoned")
            .set_playback(media_playback(event))
        {
            log::warn!("Media playback state update failed: {error}");
        }
    }
}

#[derive(Clone, Copy)]
enum PlaybackCommand {
    Toggle,
    Next,
    Previous,
}

fn handle_control(app: &tauri::AppHandle, event: MediaControlEvent) {
    let command = match event {
        MediaControlEvent::Play | MediaControlEvent::Pause | MediaControlEvent::Toggle => {
            PlaybackCommand::Toggle
        }
        MediaControlEvent::Next => PlaybackCommand::Next,
        MediaControlEvent::Previous => PlaybackCommand::Previous,
        _ => return,
    };
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        let client = match crate::provider_from(&state) {
            Ok(client) => client,
            Err(error) => {
                log::debug!("Ignoring media key while disconnected: {error}");
                return;
            }
        };
        let result = match command {
            PlaybackCommand::Toggle => state.playback.toggle(client.as_ref()).await,
            PlaybackCommand::Next => state.playback.next(client.as_ref()).await,
            PlaybackCommand::Previous => state.playback.prev(client.as_ref()).await,
        };
        if let Err(error) = result {
            let _ = app.emit("operation-error", error);
        }
    });
}

fn media_playback(event: &PlayerStateEvent) -> MediaPlayback {
    if event.track_id.is_none() && event.name.is_none() {
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
}

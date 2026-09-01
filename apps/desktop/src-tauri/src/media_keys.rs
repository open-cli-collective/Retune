use std::{sync::Mutex, time::Duration};

use playwire::{Event, MediaControls, PlaybackState, PlayerConfig, Track};
#[cfg(target_os = "windows")]
use tauri::Manager;

use crate::playback::PlayerStateEvent;

pub struct MediaKeys {
    controls: Option<Mutex<MediaControlsState>>,
}

struct MediaControlsState {
    controls: MediaControls,
    metadata_key: Option<MetadataKey>,
    playback: PlaybackState,
}

type MetadataKey = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<u64>,
);

impl MediaKeys {
    #[cfg(test)]
    pub(crate) fn disabled() -> Self {
        Self { controls: None }
    }

    pub(crate) fn spawn(
        _app: &tauri::AppHandle,
        handle_control: impl Fn(MediaControl) + Send + Sync + 'static,
    ) -> Self {
        #[cfg(target_os = "windows")]
        let hwnd = _app
            .get_webview_window("main")
            .and_then(|window| window.hwnd().ok())
            .map(|hwnd| hwnd.0 as usize as u64);

        #[cfg(target_os = "windows")]
        let Some(config) = player_config(hwnd) else {
            log::warn!("Media key setup failed: main window has no HWND");
            return Self { controls: None };
        };
        #[cfg(not(target_os = "windows"))]
        let config = player_config();

        let result = MediaControls::new(config, move |event| {
            dispatch_control(&handle_control, event)
        });

        match result {
            Ok(controls) => Self {
                controls: Some(Mutex::new(MediaControlsState {
                    controls,
                    metadata_key: None,
                    playback: PlaybackState::default(),
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
        let metadata_changed = state.metadata_key != metadata_key;
        let playback = playback_state(event);
        if let Err(error) = state.controls.set_state(&playback) {
            log::warn!("Media controls update failed: {error}");
            return false;
        }
        state.metadata_key = metadata_key;
        state.playback = playback;
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
        let mut playback = state.playback.clone();
        let Some(track) = &mut playback.track else {
            return;
        };
        track.artwork_url = url.to_owned();
        if let Err(error) = state.controls.set_state(&playback) {
            log::warn!("Media artwork update failed: {error}");
        } else {
            state.playback = playback;
        }
    }
}

#[cfg(target_os = "windows")]
fn player_config(hwnd: Option<u64>) -> Option<PlayerConfig> {
    hwnd.filter(|hwnd| *hwnd != 0)
        .map(|hwnd| common_player_config().hwnd(hwnd))
}

#[cfg(not(target_os = "windows"))]
fn player_config() -> PlayerConfig {
    common_player_config()
}

fn common_player_config() -> PlayerConfig {
    let mut config = PlayerConfig::new("Retune").desktop_entry("retune");
    config.bus_name = "retune".into();
    config
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MediaControl {
    SetPlaying(bool),
    Toggle,
    Next,
    Previous,
    Seek(u64),
}

fn dispatch_control(handle: &impl Fn(MediaControl), event: Event) {
    if let Some(command) = media_control(event) {
        handle(command);
    }
}

fn media_control(event: Event) -> Option<MediaControl> {
    Some(match event {
        Event::Play => MediaControl::SetPlaying(true),
        Event::Pause => MediaControl::SetPlaying(false),
        Event::PlayPause => MediaControl::Toggle,
        Event::Next => MediaControl::Next,
        Event::Previous => MediaControl::Previous,
        Event::SeekTo(position) => MediaControl::Seek(position.as_secs()),
        _ => return None,
    })
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

fn playback_state(event: &PlayerStateEvent) -> PlaybackState {
    PlaybackState {
        track: has_track(event).then(|| Track {
            id: event
                .uri
                .clone()
                .or_else(|| event.track_id.map(|id| id.to_string()))
                .or_else(|| event.name.clone())
                .unwrap_or_default(),
            title: event.name.clone().unwrap_or_default(),
            artists: event.art.clone().into_iter().collect(),
            album: event.alb.clone().unwrap_or_default(),
            ..Track::default()
        }),
        playing: event.is_playing,
        position: Duration::from_secs(event.elapsed),
        duration: event.duration_secs.map(Duration::from_secs),
        volume: 1.0,
        shuffle: event.shuffle,
        ..PlaybackState::default()
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
            shuffle: false,
        }
    }

    #[test]
    fn maps_playback_state() {
        assert_eq!(
            playback_state(&event(None, None, false)),
            PlaybackState {
                position: Duration::from_secs(42),
                volume: 1.0,
                ..PlaybackState::default()
            }
        );
        let playing = playback_state(&event(Some(1), Some("Track"), true));
        assert!(playing.playing);
        assert_eq!(playing.position, Duration::from_secs(42));
        assert_eq!(playing.track.unwrap().title, "Track");

        let paused = playback_state(&event(Some(1), Some("Track"), false));
        assert!(!paused.playing);
        assert!(paused.track.is_some());
    }

    #[test]
    fn preserves_explicit_media_control_intent() {
        assert!(matches!(
            media_control(Event::Play),
            Some(MediaControl::SetPlaying(true))
        ));
        assert!(matches!(
            media_control(Event::Pause),
            Some(MediaControl::SetPlaying(false))
        ));
        assert!(matches!(
            media_control(Event::PlayPause),
            Some(MediaControl::Toggle)
        ));
        assert!(matches!(
            media_control(Event::SeekTo(Duration::from_secs(17))),
            Some(MediaControl::Seek(17))
        ));
    }

    #[test]
    fn dispatches_media_control_to_supplied_handler() {
        let received = Mutex::new(Vec::new());

        dispatch_control(
            &|control| received.lock().unwrap().push(control),
            Event::Next,
        );

        assert_eq!(*received.lock().unwrap(), vec![MediaControl::Next]);
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

    #[test]
    fn preserves_player_identity() {
        let config = common_player_config();

        assert_eq!(config.identity, "Retune");
        assert_eq!(config.bus_name, "retune");
        assert_eq!(config.desktop_entry, "retune");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn builds_platform_config_only_for_non_null_hwnd() {
        let hwnd = 0x1234;
        let config = player_config(Some(hwnd)).expect("non-zero HWND");

        assert_eq!(config.hwnd, Some(hwnd));
        assert!(player_config(Some(0)).is_none());
        assert!(player_config(None).is_none());
    }
}

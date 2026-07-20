use std::collections::{HashMap, VecDeque};

use super::{empty_event, local_event, NeutralEvent, NeutralState, PlayerStateEvent, Snapshot};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ReducerAction {
    Emit(PlayerStateEvent),
    Error(String),
    Advance,
    Reload,
    Invalidate,
}

#[derive(Clone, Debug)]
struct LoadIntent {
    id: u64,
    uri: String,
    playing: bool,
}

pub(super) struct EventReducer {
    generation: u64,
    snapshot: Option<Snapshot>,
    state: PlayerStateEvent,
    next_intent: u64,
    latest_intent: Option<u64>,
    pending: VecDeque<LoadIntent>,
    bindings: HashMap<u64, LoadIntent>,
    repeat: String,
}

impl Default for EventReducer {
    fn default() -> Self {
        Self {
            generation: 0,
            snapshot: None,
            state: empty_event(false),
            next_intent: 0,
            latest_intent: None,
            pending: VecDeque::new(),
            bindings: HashMap::new(),
            repeat: "off".into(),
        }
    }
}

impl EventReducer {
    pub(super) fn activate(&mut self, generation: u64) {
        self.generation = generation;
        self.pending.clear();
        self.bindings.clear();
        self.latest_intent = None;
    }

    pub(super) fn set_snapshot(&mut self, snapshot: Option<Snapshot>) {
        self.snapshot = snapshot;
    }

    pub(super) fn snapshot_mut(&mut self) -> Option<&mut Snapshot> {
        self.snapshot.as_mut()
    }

    pub(super) fn snapshot(&self) -> Option<&Snapshot> {
        self.snapshot.as_ref()
    }

    pub(super) fn state(&self) -> &PlayerStateEvent {
        &self.state
    }

    pub(super) fn repeat(&self) -> &str {
        &self.repeat
    }

    pub(super) fn set_repeat(&mut self, repeat: &str) {
        self.repeat = repeat.to_owned();
    }

    pub(super) fn queue_load(&mut self, uri: &str, playing: bool) {
        self.next_intent = self.next_intent.wrapping_add(1);
        let intent = LoadIntent {
            id: self.next_intent,
            uri: uri.to_owned(),
            playing,
        };
        self.latest_intent = Some(intent.id);
        self.pending.push_back(intent);
    }

    pub(super) fn handle(&mut self, event: NeutralEvent) -> Vec<ReducerAction> {
        if event.generation() != self.generation {
            return vec![];
        }
        match event {
            NeutralEvent::ConnectState { state, .. } => self.connect_state(state),
            NeutralEvent::Error { message, .. } => vec![ReducerAction::Error(message)],
            NeutralEvent::Disconnected { .. } => vec![
                ReducerAction::Error("Spotify playback lost its network connection.".into()),
                ReducerAction::Invalidate,
            ],
            NeutralEvent::RequestIdChanged { request_id, .. } => {
                if let Some(intent) = self.pending.pop_front() {
                    self.bindings.insert(request_id, intent);
                }
                vec![]
            }
            NeutralEvent::Loading {
                request_id,
                uri,
                position_ms,
                ..
            } => {
                let Some(intent) = self.current_intent(request_id, &uri) else {
                    return vec![];
                };
                self.emit_track(&uri, position_ms, intent.playing)
            }
            NeutralEvent::Playing {
                request_id,
                uri,
                position_ms,
                ..
            } => {
                if self.accepts(request_id, &uri) {
                    self.emit_track(&uri, position_ms, true)
                } else {
                    vec![]
                }
            }
            NeutralEvent::Paused {
                request_id,
                uri,
                position_ms,
                ..
            } => {
                if self.accepts(request_id, &uri) {
                    self.emit_track(&uri, position_ms, false)
                } else {
                    vec![]
                }
            }
            NeutralEvent::PositionChanged {
                request_id,
                uri,
                position_ms,
                ..
            }
            | NeutralEvent::Seeked {
                request_id,
                uri,
                position_ms,
                ..
            }
            | NeutralEvent::PositionCorrection {
                request_id,
                uri,
                position_ms,
                ..
            } => {
                if !self.accepts(request_id, &uri) {
                    return vec![];
                }
                self.state.elapsed = u64::from(position_ms) / 1000;
                vec![ReducerAction::Emit(self.state.clone())]
            }
            NeutralEvent::Unavailable {
                request_id, uri, ..
            } => {
                if !self.accepts(request_id, &uri) {
                    return vec![];
                }
                let name = self
                    .track(&uri)
                    .map(|track| track.name.as_str())
                    .unwrap_or(uri.as_str());
                vec![
                    ReducerAction::Error(format!("{name} is unavailable.")),
                    ReducerAction::Advance,
                ]
            }
            NeutralEvent::Stopped {
                request_id, uri, ..
            } => {
                if !self.accepts(request_id, &uri) {
                    return vec![];
                }
                self.state = empty_event(false);
                self.snapshot = None;
                vec![ReducerAction::Emit(self.state.clone())]
            }
            NeutralEvent::EndOfTrack {
                request_id, uri, ..
            } => self
                .accepts(request_id, &uri)
                .then_some(vec![if self.repeat == "one" {
                    ReducerAction::Reload
                } else {
                    ReducerAction::Advance
                }])
                .unwrap_or_default(),
        }
    }

    fn current_intent(&self, request_id: u64, uri: &str) -> Option<LoadIntent> {
        self.bindings
            .get(&request_id)
            .filter(|intent| Some(intent.id) == self.latest_intent && intent.uri == uri)
            .cloned()
    }

    fn accepts(&self, request_id: u64, uri: &str) -> bool {
        self.current_intent(request_id, uri).is_some()
    }

    fn track(&self, uri: &str) -> Option<&super::SnapshotTrack> {
        let snapshot = self.snapshot.as_ref()?;
        snapshot
            .tracks
            .get(snapshot.index)
            .filter(|track| track.uri == uri)
            .or_else(|| snapshot.tracks.iter().find(|track| track.uri == uri))
    }

    fn emit_track(&mut self, uri: &str, position_ms: u32, playing: bool) -> Vec<ReducerAction> {
        let Some(track) = self.track(uri).cloned() else {
            return vec![ReducerAction::Error(format!(
                "Playback returned a track outside the active queue: {uri}"
            ))];
        };
        self.state = local_event(&track, u64::from(position_ms) / 1000, playing, true);
        vec![ReducerAction::Emit(self.state.clone())]
    }

    fn connect_state(&mut self, state: NeutralState) -> Vec<ReducerAction> {
        self.state = if state.external {
            PlayerStateEvent {
                track_id: None,
                elapsed: u64::from(state.position_ms) / 1000,
                is_playing: state.is_playing,
                external: true,
                name: state.name,
                art: state.art,
                alb: state.alb,
                duration_secs: state.duration_ms.map(|value| u64::from(value) / 1000),
                volume_supported: state.volume_supported,
            }
        } else if let Some(uri) = state.uri {
            if let Some(snapshot) = &mut self.snapshot {
                if let Some(index) = snapshot.tracks.iter().position(|track| track.uri == uri) {
                    snapshot.index = index;
                }
            }
            let Some(track) = self.track(&uri).cloned() else {
                return vec![ReducerAction::Error(format!(
                    "Playback returned a track outside the active queue: {uri}"
                ))];
            };
            local_event(
                &track,
                u64::from(state.position_ms) / 1000,
                state.is_playing,
                state.volume_supported,
            )
        } else {
            empty_event(false)
        };
        vec![ReducerAction::Emit(self.state.clone())]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playback::SnapshotTrack;

    fn track(id: u64) -> SnapshotTrack {
        SnapshotTrack {
            id,
            uri: format!("spotify:track:{id}"),
            name: format!("Track {id}"),
            art: "Artist".into(),
            alb: "Album".into(),
            duration_secs: 100,
        }
    }

    fn reducer() -> EventReducer {
        let mut reducer = EventReducer::default();
        reducer.activate(7);
        reducer.set_snapshot(Some(Snapshot {
            tracks: vec![track(1), track(2)],
            index: 0,
        }));
        reducer
    }

    fn bind(reducer: &mut EventReducer, uri: &str, playing: bool, request_id: u64) {
        reducer.queue_load(uri, playing);
        reducer.handle(NeutralEvent::RequestIdChanged {
            generation: 7,
            request_id,
        });
    }

    fn emitted(actions: Vec<ReducerAction>) -> PlayerStateEvent {
        match actions.as_slice() {
            [ReducerAction::Emit(event)] => event.clone(),
            other => panic!("expected one emitted state, got {other:?}"),
        }
    }

    #[test]
    fn stale_generation_is_discarded() {
        let mut reducer = reducer();
        bind(&mut reducer, "spotify:track:1", true, 1);
        assert!(reducer
            .handle(NeutralEvent::Playing {
                generation: 6,
                request_id: 1,
                uri: "spotify:track:1".into(),
                position_ms: 1000,
            })
            .is_empty());
    }

    #[test]
    fn rapid_loads_discard_superseded_end_without_flicker_or_double_advance() {
        let mut reducer = reducer();
        bind(&mut reducer, "spotify:track:1", true, 1);
        reducer.snapshot_mut().unwrap().index = 1;
        reducer.queue_load("spotify:track:2", true);
        assert!(reducer
            .handle(NeutralEvent::EndOfTrack {
                generation: 7,
                request_id: 1,
                uri: "spotify:track:1".into(),
            })
            .is_empty());
        reducer.handle(NeutralEvent::RequestIdChanged {
            generation: 7,
            request_id: 2,
        });
        let state = emitted(reducer.handle(NeutralEvent::Loading {
            generation: 7,
            request_id: 2,
            uri: "spotify:track:2".into(),
            position_ms: 0,
        }));
        assert_eq!(state.track_id, Some(2));
        assert!(reducer
            .handle(NeutralEvent::EndOfTrack {
                generation: 7,
                request_id: 1,
                uri: "spotify:track:1".into(),
            })
            .is_empty());
    }

    #[test]
    fn unavailable_reports_error_then_advances() {
        let mut reducer = reducer();
        bind(&mut reducer, "spotify:track:1", true, 1);
        assert!(matches!(
            reducer
                .handle(NeutralEvent::Unavailable {
                    generation: 7,
                    request_id: 1,
                    uri: "spotify:track:1".into(),
                })
                .as_slice(),
            [ReducerAction::Error(_), ReducerAction::Advance]
        ));
    }

    #[test]
    fn stopped_clears_player_state() {
        let mut reducer = reducer();
        bind(&mut reducer, "spotify:track:1", true, 1);
        reducer.handle(NeutralEvent::Playing {
            generation: 7,
            request_id: 1,
            uri: "spotify:track:1".into(),
            position_ms: 1000,
        });
        let state = emitted(reducer.handle(NeutralEvent::Stopped {
            generation: 7,
            request_id: 1,
            uri: "spotify:track:1".into(),
        }));
        assert_eq!(state.track_id, None);
        assert!(!state.is_playing);
    }

    #[test]
    fn natural_advance_has_no_intermediate_empty_state() {
        let mut reducer = reducer();
        bind(&mut reducer, "spotify:track:1", true, 1);
        assert_eq!(
            reducer.handle(NeutralEvent::EndOfTrack {
                generation: 7,
                request_id: 1,
                uri: "spotify:track:1".into(),
            }),
            [ReducerAction::Advance]
        );
    }

    #[test]
    fn repeat_one_reloads_same_track_on_end() {
        let mut reducer = reducer();
        reducer.set_repeat("one");
        bind(&mut reducer, "spotify:track:1", true, 1);
        assert_eq!(
            reducer.handle(NeutralEvent::EndOfTrack {
                generation: 7,
                request_id: 1,
                uri: "spotify:track:1".into(),
            }),
            [ReducerAction::Reload]
        );
    }

    #[test]
    fn unavailable_advances_under_repeat_one() {
        let mut reducer = reducer();
        reducer.set_repeat("one");
        bind(&mut reducer, "spotify:track:1", true, 1);
        assert!(matches!(
            reducer
                .handle(NeutralEvent::Unavailable {
                    generation: 7,
                    request_id: 1,
                    uri: "spotify:track:1".into(),
                })
                .as_slice(),
            [ReducerAction::Error(_), ReducerAction::Advance]
        ));
    }

    #[test]
    fn loading_preserves_requested_paused_intent() {
        let mut reducer = reducer();
        bind(&mut reducer, "spotify:track:1", false, 1);
        let state = emitted(reducer.handle(NeutralEvent::Loading {
            generation: 7,
            request_id: 1,
            uri: "spotify:track:1".into(),
            position_ms: 2500,
        }));
        assert!(!state.is_playing);
        assert_eq!(state.elapsed, 2);
    }

    #[test]
    fn position_changed_ticks_without_changing_play_state() {
        let mut reducer = reducer();
        bind(&mut reducer, "spotify:track:1", true, 1);
        reducer.handle(NeutralEvent::Playing {
            generation: 7,
            request_id: 1,
            uri: "spotify:track:1".into(),
            position_ms: 0,
        });
        let state = emitted(reducer.handle(NeutralEvent::PositionChanged {
            generation: 7,
            request_id: 1,
            uri: "spotify:track:1".into(),
            position_ms: 3456,
        }));
        assert!(state.is_playing);
        assert_eq!(state.elapsed, 3);
    }

    #[test]
    fn every_position_event_converts_milliseconds_to_seconds() {
        let mut reducer = reducer();
        bind(&mut reducer, "spotify:track:1", true, 1);
        let events = [
            NeutralEvent::Loading {
                generation: 7,
                request_id: 1,
                uri: "spotify:track:1".into(),
                position_ms: 9876,
            },
            NeutralEvent::Playing {
                generation: 7,
                request_id: 1,
                uri: "spotify:track:1".into(),
                position_ms: 9876,
            },
            NeutralEvent::Paused {
                generation: 7,
                request_id: 1,
                uri: "spotify:track:1".into(),
                position_ms: 9876,
            },
            NeutralEvent::PositionChanged {
                generation: 7,
                request_id: 1,
                uri: "spotify:track:1".into(),
                position_ms: 9876,
            },
            NeutralEvent::Seeked {
                generation: 7,
                request_id: 1,
                uri: "spotify:track:1".into(),
                position_ms: 9876,
            },
            NeutralEvent::PositionCorrection {
                generation: 7,
                request_id: 1,
                uri: "spotify:track:1".into(),
                position_ms: 9876,
            },
        ];
        for event in events {
            assert_eq!(emitted(reducer.handle(event)).elapsed, 9);
        }
    }
}

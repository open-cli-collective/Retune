use std::collections::{HashMap, VecDeque};

use super::{empty_event, local_event, NeutralEvent, NeutralState, PlayerStateEvent, Snapshot};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ReducerAction {
    Emit(PlayerStateEvent),
    Error(String),
    TrackCompleted(String),
    Advance,
    Reload,
    Invalidate,
    Reconnect,
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
    play_threshold_percent: u8,
    previous_position_ms: u32,
    counted: bool,
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
            play_threshold_percent: 100,
            previous_position_ms: 0,
            counted: false,
        }
    }
}

impl EventReducer {
    pub(super) fn activate(&mut self, generation: u64) {
        self.generation = generation;
        self.pending.clear();
        self.bindings.clear();
        self.latest_intent = None;
        self.reset_playthrough();
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

    pub(super) fn set_play_threshold_percent(&mut self, percent: u8) {
        self.play_threshold_percent = percent;
    }

    pub(super) fn queue_load(&mut self, uri: &str, playing: bool) {
        self.reset_playthrough();
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
            // Idle disconnects are routine and recover lazily. Active playback
            // invalidates first, then retries in the controller so brief drops
            // stay invisible and only exhausted retries surface a hard error.
            NeutralEvent::Disconnected { .. } => {
                if self.state.is_playing {
                    vec![ReducerAction::Invalidate, ReducerAction::Reconnect]
                } else {
                    vec![ReducerAction::Invalidate]
                }
            }
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
            } => self.position_changed(request_id, &uri, position_ms),
            NeutralEvent::Seeked {
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
                self.previous_position_ms = position_ms;
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
            } => {
                if self.accepts(request_id, &uri) {
                    self.complete(uri)
                } else {
                    vec![]
                }
            }
            NeutralEvent::ConnectBoundary { uri, .. } => {
                if self
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.current().uri == uri)
                {
                    self.complete(uri)
                } else {
                    vec![]
                }
            }
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
        self.previous_position_ms = position_ms;
        self.state = local_event(&track, u64::from(position_ms) / 1000, playing, true);
        vec![ReducerAction::Emit(self.state.clone())]
    }

    fn position_changed(
        &mut self,
        request_id: u64,
        uri: &str,
        position_ms: u32,
    ) -> Vec<ReducerAction> {
        if !self.accepts(request_id, uri) {
            return vec![];
        }
        let threshold_ms = self
            .track(uri)
            .map(|track| track.duration_secs * 1000 * u64::from(self.play_threshold_percent) / 100);
        let crossed = self.play_threshold_percent < 100
            && !self.counted
            && threshold_ms.is_some_and(|threshold| {
                u64::from(self.previous_position_ms) < threshold
                    && u64::from(position_ms) >= threshold
            });
        self.previous_position_ms = position_ms;
        self.state.elapsed = u64::from(position_ms) / 1000;
        let mut actions = vec![ReducerAction::Emit(self.state.clone())];
        if crossed {
            self.counted = true;
            actions.push(ReducerAction::TrackCompleted(uri.to_owned()));
        }
        actions
    }

    fn complete(&mut self, uri: String) -> Vec<ReducerAction> {
        let mut actions = Vec::with_capacity(2);
        if !self.counted {
            self.counted = true;
            actions.push(ReducerAction::TrackCompleted(uri));
        }
        actions.push(if self.repeat == "one" {
            ReducerAction::Reload
        } else {
            ReducerAction::Advance
        });
        actions
    }

    fn reset_playthrough(&mut self) {
        self.previous_position_ms = 0;
        self.counted = false;
    }

    fn connect_state(&mut self, state: NeutralState) -> Vec<ReducerAction> {
        self.previous_position_ms = state.position_ms;
        self.state = if state.external {
            PlayerStateEvent {
                track_id: None,
                uri: state.uri,
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
    fn stale_completion_events_are_discarded() {
        let mut reducer = reducer();
        bind(&mut reducer, "spotify:track:1", true, 1);

        assert!(reducer
            .handle(NeutralEvent::EndOfTrack {
                generation: 6,
                request_id: 1,
                uri: "spotify:track:1".into(),
            })
            .is_empty());
        assert!(reducer
            .handle(NeutralEvent::ConnectBoundary {
                generation: 6,
                uri: "spotify:track:1".into(),
            })
            .is_empty());
    }

    #[test]
    fn connect_boundary_advances_full_snapshot() {
        assert_eq!(
            reducer().handle(NeutralEvent::ConnectBoundary {
                generation: 7,
                uri: "spotify:track:1".into(),
            }),
            [
                ReducerAction::TrackCompleted("spotify:track:1".into()),
                ReducerAction::Advance
            ]
        );
    }

    #[test]
    fn connect_boundary_reloads_under_repeat_one() {
        let mut reducer = reducer();
        reducer.set_repeat("one");

        assert_eq!(
            reducer.handle(NeutralEvent::ConnectBoundary {
                generation: 7,
                uri: "spotify:track:1".into(),
            }),
            [
                ReducerAction::TrackCompleted("spotify:track:1".into()),
                ReducerAction::Reload
            ]
        );
    }

    #[test]
    fn delayed_connect_boundary_for_previous_track_is_discarded() {
        let mut reducer = reducer();
        reducer.snapshot_mut().unwrap().index = 1;

        assert!(reducer
            .handle(NeutralEvent::ConnectBoundary {
                generation: 7,
                uri: "spotify:track:1".into(),
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
    fn disconnect_while_playing_invalidates_and_reconnects() {
        let mut reducer = reducer();
        bind(&mut reducer, "spotify:track:1", true, 1);
        reducer.handle(NeutralEvent::Playing {
            generation: 7,
            request_id: 1,
            uri: "spotify:track:1".into(),
            position_ms: 1000,
        });
        assert!(matches!(
            reducer
                .handle(NeutralEvent::Disconnected { generation: 7 })
                .as_slice(),
            [ReducerAction::Invalidate, ReducerAction::Reconnect]
        ));
    }

    #[test]
    fn disconnect_while_idle_invalidates_silently() {
        let mut reducer = reducer();
        assert_eq!(
            reducer.handle(NeutralEvent::Disconnected { generation: 7 }),
            [ReducerAction::Invalidate]
        );
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
            [
                ReducerAction::TrackCompleted("spotify:track:1".into()),
                ReducerAction::Advance
            ]
        );
    }

    #[test]
    fn repeat_one_reloads_same_track_on_end() {
        let mut reducer = reducer();
        reducer.set_repeat("one");
        bind(&mut reducer, "spotify:track:1", true, 1);
        for request_id in [1, 2] {
            assert_eq!(
                reducer.handle(NeutralEvent::EndOfTrack {
                    generation: 7,
                    request_id,
                    uri: "spotify:track:1".into(),
                }),
                [
                    ReducerAction::TrackCompleted("spotify:track:1".into()),
                    ReducerAction::Reload
                ]
            );
            if request_id == 1 {
                bind(&mut reducer, "spotify:track:1", true, 2);
            }
        }
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
    fn file_events_bind_load_play_and_advance() {
        let mut reducer = reducer();
        reducer.snapshot_mut().unwrap().tracks[0].uri = "file:///tmp/song.mp3".into();
        bind(&mut reducer, "file:///tmp/song.mp3", true, 41);

        assert_eq!(
            emitted(reducer.handle(NeutralEvent::Loading {
                generation: 7,
                request_id: 41,
                uri: "file:///tmp/song.mp3".into(),
                position_ms: 0,
            }))
            .track_id,
            Some(1)
        );
        assert!(
            emitted(reducer.handle(NeutralEvent::Playing {
                generation: 7,
                request_id: 41,
                uri: "file:///tmp/song.mp3".into(),
                position_ms: 0,
            }))
            .is_playing
        );
        assert_eq!(
            reducer.handle(NeutralEvent::EndOfTrack {
                generation: 7,
                request_id: 41,
                uri: "file:///tmp/song.mp3".into(),
            }),
            [
                ReducerAction::TrackCompleted("file:///tmp/song.mp3".into()),
                ReducerAction::Advance
            ]
        );
    }

    #[test]
    fn file_end_reloads_under_repeat_one_and_second_request_is_accepted() {
        let mut reducer = reducer();
        reducer.snapshot_mut().unwrap().tracks[0].uri = "file:///tmp/song.mp3".into();
        reducer.set_repeat("one");
        bind(&mut reducer, "file:///tmp/song.mp3", true, 42);

        assert_eq!(
            reducer.handle(NeutralEvent::EndOfTrack {
                generation: 7,
                request_id: 42,
                uri: "file:///tmp/song.mp3".into(),
            }),
            [
                ReducerAction::TrackCompleted("file:///tmp/song.mp3".into()),
                ReducerAction::Reload
            ]
        );
        bind(&mut reducer, "file:///tmp/song.mp3", true, 43);
        assert!(
            emitted(reducer.handle(NeutralEvent::Playing {
                generation: 7,
                request_id: 43,
                uri: "file:///tmp/song.mp3".into(),
                position_ms: 0,
            }))
            .is_playing
        );
        assert!(matches!(
            reducer
                .handle(NeutralEvent::Unavailable {
                    generation: 7,
                    request_id: 43,
                    uri: "file:///tmp/song.mp3".into(),
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

    #[test]
    fn organic_crossing_counts_once_and_completion_does_not_count_again() {
        let mut reducer = reducer();
        reducer.set_play_threshold_percent(75);
        bind(&mut reducer, "spotify:track:1", true, 1);
        reducer.handle(NeutralEvent::Playing {
            generation: 7,
            request_id: 1,
            uri: "spotify:track:1".into(),
            position_ms: 74_000,
        });

        assert!(matches!(
            reducer
                .handle(NeutralEvent::PositionChanged {
                    generation: 7,
                    request_id: 1,
                    uri: "spotify:track:1".into(),
                    position_ms: 75_000,
                })
                .as_slice(),
            [ReducerAction::Emit(_), ReducerAction::TrackCompleted(uri)] if uri == "spotify:track:1"
        ));
        assert!(matches!(
            reducer
                .handle(NeutralEvent::PositionChanged {
                    generation: 7,
                    request_id: 1,
                    uri: "spotify:track:1".into(),
                    position_ms: 90_000,
                })
                .as_slice(),
            [ReducerAction::Emit(_)]
        ));
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
    fn connect_boundary_does_not_count_again_after_threshold_crossing() {
        let mut reducer = reducer();
        reducer.set_play_threshold_percent(75);
        bind(&mut reducer, "spotify:track:1", true, 1);
        assert!(matches!(
            reducer
                .handle(NeutralEvent::PositionChanged {
                    generation: 7,
                    request_id: 1,
                    uri: "spotify:track:1".into(),
                    position_ms: 75_000,
                })
                .as_slice(),
            [ReducerAction::Emit(_), ReducerAction::TrackCompleted(_)]
        ));

        assert_eq!(
            reducer.handle(NeutralEvent::ConnectBoundary {
                generation: 7,
                uri: "spotify:track:1".into(),
            }),
            [ReducerAction::Advance]
        );
    }

    #[test]
    fn seeks_and_corrections_do_not_count() {
        let mut reducer = reducer();
        reducer.set_play_threshold_percent(50);
        bind(&mut reducer, "spotify:track:1", true, 1);

        assert!(matches!(
            reducer
                .handle(NeutralEvent::Seeked {
                    generation: 7,
                    request_id: 1,
                    uri: "spotify:track:1".into(),
                    position_ms: 60_000,
                })
                .as_slice(),
            [ReducerAction::Emit(_)]
        ));
        assert!(matches!(
            reducer
                .handle(NeutralEvent::PositionChanged {
                    generation: 7,
                    request_id: 1,
                    uri: "spotify:track:1".into(),
                    position_ms: 61_000,
                })
                .as_slice(),
            [ReducerAction::Emit(_)]
        ));
        assert!(matches!(
            reducer
                .handle(NeutralEvent::PositionCorrection {
                    generation: 7,
                    request_id: 1,
                    uri: "spotify:track:1".into(),
                    position_ms: 40_000,
                })
                .as_slice(),
            [ReducerAction::Emit(_)]
        ));
        assert!(matches!(
            reducer
                .handle(NeutralEvent::PositionChanged {
                    generation: 7,
                    request_id: 1,
                    uri: "spotify:track:1".into(),
                    position_ms: 50_000,
                })
                .as_slice(),
            [ReducerAction::Emit(_), ReducerAction::TrackCompleted(_)]
        ));
        assert!(matches!(
            reducer
                .handle(NeutralEvent::PositionChanged {
                    generation: 7,
                    request_id: 1,
                    uri: "spotify:track:1".into(),
                    position_ms: 51_000,
                })
                .as_slice(),
            [ReducerAction::Emit(_)]
        ));
    }

    #[test]
    fn completion_counts_when_sparse_events_never_observe_threshold_crossing() {
        let mut reducer = reducer();
        reducer.set_play_threshold_percent(50);
        bind(&mut reducer, "spotify:track:1", true, 1);

        assert_eq!(
            reducer.handle(NeutralEvent::EndOfTrack {
                generation: 7,
                request_id: 1,
                uri: "spotify:track:1".into(),
            }),
            [
                ReducerAction::TrackCompleted("spotify:track:1".into()),
                ReducerAction::Advance
            ]
        );
    }

    #[test]
    fn queued_skip_resets_crossing_and_does_not_count_skipped_track() {
        let mut reducer = reducer();
        reducer.set_play_threshold_percent(50);
        bind(&mut reducer, "spotify:track:1", true, 1);
        reducer.handle(NeutralEvent::PositionChanged {
            generation: 7,
            request_id: 1,
            uri: "spotify:track:1".into(),
            position_ms: 49_000,
        });
        reducer.snapshot_mut().unwrap().index = 1;
        bind(&mut reducer, "spotify:track:2", true, 2);

        assert!(reducer
            .handle(NeutralEvent::PositionChanged {
                generation: 7,
                request_id: 1,
                uri: "spotify:track:1".into(),
                position_ms: 51_000,
            })
            .is_empty());
        assert!(matches!(
            reducer
                .handle(NeutralEvent::PositionChanged {
                    generation: 7,
                    request_id: 2,
                    uri: "spotify:track:2".into(),
                    position_ms: 51_000,
                })
                .as_slice(),
            [ReducerAction::Emit(_), ReducerAction::TrackCompleted(uri)] if uri == "spotify:track:2"
        ));
    }

    #[test]
    fn queued_skip_after_crossing_does_not_count_previous_track_again() {
        let mut reducer = reducer();
        reducer.set_play_threshold_percent(50);
        bind(&mut reducer, "spotify:track:1", true, 1);
        assert!(matches!(
            reducer
                .handle(NeutralEvent::PositionChanged {
                    generation: 7,
                    request_id: 1,
                    uri: "spotify:track:1".into(),
                    position_ms: 50_000,
                })
                .as_slice(),
            [ReducerAction::Emit(_), ReducerAction::TrackCompleted(_)]
        ));
        reducer.snapshot_mut().unwrap().index = 1;
        bind(&mut reducer, "spotify:track:2", true, 2);

        assert!(reducer
            .handle(NeutralEvent::PositionChanged {
                generation: 7,
                request_id: 1,
                uri: "spotify:track:1".into(),
                position_ms: 60_000,
            })
            .is_empty());
    }

    #[test]
    fn repeat_one_counts_each_loaded_pass() {
        let mut reducer = reducer();
        reducer.set_play_threshold_percent(90);
        reducer.set_repeat("one");

        for request_id in [1, 2] {
            bind(&mut reducer, "spotify:track:1", true, request_id);
            assert!(matches!(
                reducer
                    .handle(NeutralEvent::PositionChanged {
                        generation: 7,
                        request_id,
                        uri: "spotify:track:1".into(),
                        position_ms: 90_000,
                    })
                    .as_slice(),
                [ReducerAction::Emit(_), ReducerAction::TrackCompleted(_)]
            ));
            assert_eq!(
                reducer.handle(NeutralEvent::EndOfTrack {
                    generation: 7,
                    request_id,
                    uri: "spotify:track:1".into(),
                }),
                [ReducerAction::Reload]
            );
        }
    }

    #[test]
    fn stale_crossing_is_ignored_and_completion_fallback_remains_at_100_percent() {
        let mut reducer = reducer();
        reducer.set_play_threshold_percent(75);
        bind(&mut reducer, "spotify:track:1", true, 1);
        assert!(reducer
            .handle(NeutralEvent::PositionChanged {
                generation: 6,
                request_id: 1,
                uri: "spotify:track:1".into(),
                position_ms: 80_000,
            })
            .is_empty());

        reducer.set_play_threshold_percent(100);
        assert!(matches!(
            reducer
                .handle(NeutralEvent::PositionChanged {
                    generation: 7,
                    request_id: 1,
                    uri: "spotify:track:1".into(),
                    position_ms: 100_000,
                })
                .as_slice(),
            [ReducerAction::Emit(_)]
        ));
        assert_eq!(
            reducer.handle(NeutralEvent::EndOfTrack {
                generation: 7,
                request_id: 1,
                uri: "spotify:track:1".into(),
            }),
            [
                ReducerAction::TrackCompleted("spotify:track:1".into()),
                ReducerAction::Advance
            ]
        );
    }
}

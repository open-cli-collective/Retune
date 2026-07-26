use std::{
    sync::mpsc::{self, RecvTimeoutError},
    time::{Duration, Instant},
};

use retune_audio::FileSource;
use rodio::{OutputStream, OutputStreamBuilder, Sink, Source};
use tokio::sync::mpsc as tokio_mpsc;

use super::NeutralEvent;

enum Command {
    Load {
        generation: u64,
        request_id: u64,
        uri: String,
        position_ms: u32,
        playing: bool,
    },
    Play,
    Pause,
    Seek(u64),
    Volume(u8),
    Stop(Option<Stopped>),
}

struct Stopped {
    generation: u64,
    request_id: u64,
    uri: String,
}

struct Active {
    sink: Sink,
    generation: u64,
    request_id: u64,
    uri: String,
    last_tick: Instant,
}

pub(super) struct FileEngine {
    commands: mpsc::Sender<Command>,
    events: tokio_mpsc::UnboundedSender<NeutralEvent>,
    generation: u64,
    request_id: u64,
    uri: Option<String>,
    playing: bool,
    volume: u8,
}

impl FileEngine {
    pub(super) fn new(
        events: tokio_mpsc::UnboundedSender<NeutralEvent>,
        generation: u64,
        volume: u8,
    ) -> Self {
        let (commands, receiver) = mpsc::channel();
        let worker_events = events.clone();
        std::thread::Builder::new()
            .name("retune-file-playback".into())
            .spawn(move || run(receiver, worker_events, volume))
            .expect("file playback thread starts");
        Self {
            commands,
            events,
            generation,
            request_id: 0,
            uri: None,
            playing: false,
            volume,
        }
    }

    pub(super) fn set_generation(&mut self, generation: u64) {
        self.stop_silently();
        self.generation = generation;
    }

    pub(super) fn is_active(&self) -> bool {
        self.uri.is_some()
    }

    #[cfg(test)]
    pub(super) fn request_id(&self) -> u64 {
        self.request_id
    }

    pub(super) fn load(
        &mut self,
        uri: &str,
        start_playing: bool,
        position_ms: u32,
    ) -> Result<(), String> {
        self.request_id = self.request_id.wrapping_add(1);
        self.uri = Some(uri.to_owned());
        self.playing = start_playing;
        self.send_event(NeutralEvent::RequestIdChanged {
            generation: self.generation,
            request_id: self.request_id,
        });
        self.send_event(NeutralEvent::Loading {
            generation: self.generation,
            request_id: self.request_id,
            uri: uri.to_owned(),
            position_ms,
        });
        self.send(Command::Load {
            generation: self.generation,
            request_id: self.request_id,
            uri: uri.to_owned(),
            position_ms,
            playing: start_playing,
        })
    }

    pub(super) fn toggle(&mut self) -> Result<(), String> {
        if self.playing {
            self.pause()
        } else {
            self.resume()
        }
    }

    pub(super) fn pause(&mut self) -> Result<(), String> {
        if self.uri.is_none() {
            return Err("Nothing is playing".into());
        }
        self.playing = false;
        self.send(Command::Pause)
    }

    pub(super) fn resume(&mut self) -> Result<(), String> {
        if self.uri.is_none() {
            return Err("Nothing is playing".into());
        }
        self.playing = true;
        self.send(Command::Play)
    }

    pub(super) fn seek(&self, seconds: u64) -> Result<(), String> {
        if self.uri.is_none() {
            return Err("Nothing is playing".into());
        }
        self.send(Command::Seek(seconds))
    }

    pub(super) fn set_volume(&mut self, volume: u8) {
        self.volume = volume;
        let _ = self.send(Command::Volume(volume));
    }

    pub(super) fn stop(&mut self) {
        let uri = self.uri.take();
        self.playing = false;
        if let Some(uri) = uri {
            let _ = self.send(Command::Stop(Some(Stopped {
                generation: self.generation,
                request_id: self.request_id,
                uri,
            })));
        }
    }

    pub(super) fn stop_silently(&mut self) {
        self.uri = None;
        self.playing = false;
        let _ = self.send(Command::Stop(None));
    }

    fn send(&self, command: Command) -> Result<(), String> {
        self.commands
            .send(command)
            .map_err(|_| "File playback is unavailable".into())
    }

    fn send_event(&self, event: NeutralEvent) {
        let _ = self.events.send(event);
    }
}

pub(super) fn sink_volume(volume: u8) -> f32 {
    (f32::from(volume.min(100)) / 100.0).powi(2)
}

fn run(
    commands: mpsc::Receiver<Command>,
    events: tokio_mpsc::UnboundedSender<NeutralEvent>,
    mut volume: u8,
) {
    let mut stream = None;
    let mut runtime: Option<Active> = None;
    loop {
        match commands.recv_timeout(Duration::from_millis(100)) {
            Ok(Command::Load {
                generation,
                request_id,
                uri,
                position_ms,
                playing,
            }) => {
                if let Some(current) = runtime.take() {
                    current.sink.stop();
                }
                match load(
                    &mut stream,
                    generation,
                    request_id,
                    uri.clone(),
                    position_ms,
                    playing,
                    volume,
                ) {
                    Ok(loaded) => {
                        let _ = events.send(if playing {
                            NeutralEvent::Playing {
                                generation,
                                request_id,
                                uri,
                                position_ms,
                            }
                        } else {
                            NeutralEvent::Paused {
                                generation,
                                request_id,
                                uri,
                                position_ms,
                            }
                        });
                        runtime = Some(loaded);
                    }
                    Err(error) => {
                        log::warn!("File playback unavailable for {uri}: {error}");
                        let _ = events.send(NeutralEvent::Unavailable {
                            generation,
                            request_id,
                            uri,
                        });
                    }
                }
            }
            Ok(Command::Play) => {
                if let Some(current) = &runtime {
                    current.sink.play();
                    let _ = events.send(NeutralEvent::Playing {
                        generation: current.generation,
                        request_id: current.request_id,
                        uri: current.uri.clone(),
                        position_ms: position_ms(&current.sink),
                    });
                }
            }
            Ok(Command::Pause) => {
                if let Some(current) = &runtime {
                    current.sink.pause();
                    let _ = events.send(NeutralEvent::Paused {
                        generation: current.generation,
                        request_id: current.request_id,
                        uri: current.uri.clone(),
                        position_ms: position_ms(&current.sink),
                    });
                }
            }
            Ok(Command::Seek(seconds)) => {
                if let Some(current) = &runtime {
                    let position = Duration::from_secs(seconds);
                    if let Err(error) = current.sink.try_seek(position) {
                        log::warn!("File seek failed: {error}");
                    } else {
                        let _ = events.send(NeutralEvent::Seeked {
                            generation: current.generation,
                            request_id: current.request_id,
                            uri: current.uri.clone(),
                            position_ms: duration_ms(position),
                        });
                    }
                }
            }
            Ok(Command::Volume(next)) => {
                volume = next;
                if let Some(current) = &runtime {
                    current.sink.set_volume(sink_volume(volume));
                }
            }
            Ok(Command::Stop(stopped)) => {
                if let Some(current) = runtime.take() {
                    current.sink.stop();
                }
                if let Some(stopped) = stopped {
                    let _ = events.send(NeutralEvent::Stopped {
                        generation: stopped.generation,
                        request_id: stopped.request_id,
                        uri: stopped.uri,
                    });
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }

        let Some(current) = &mut runtime else {
            continue;
        };
        if !current.sink.is_paused() && current.sink.empty() {
            let _ = events.send(NeutralEvent::EndOfTrack {
                generation: current.generation,
                request_id: current.request_id,
                uri: current.uri.clone(),
            });
            runtime = None;
        } else if !current.sink.is_paused() && current.last_tick.elapsed() >= Duration::from_secs(1)
        {
            current.last_tick = Instant::now();
            let _ = events.send(NeutralEvent::PositionChanged {
                generation: current.generation,
                request_id: current.request_id,
                uri: current.uri.clone(),
                position_ms: position_ms(&current.sink),
            });
        }
    }
}

fn load(
    stream: &mut Option<OutputStream>,
    generation: u64,
    request_id: u64,
    uri: String,
    position_ms: u32,
    playing: bool,
    volume: u8,
) -> Result<Active, String> {
    let path = crate::localfiles::path_from_file_uri(&uri)?;
    let mut source = FileSource::open(path).map_err(|error| error.to_string())?;
    if position_ms != 0 {
        source
            .try_seek(Duration::from_millis(u64::from(position_ms)))
            .map_err(|error| error.to_string())?;
    }
    if stream.is_none() {
        *stream =
            Some(OutputStreamBuilder::open_default_stream().map_err(|error| error.to_string())?);
    }
    let sink = Sink::connect_new(stream.as_ref().expect("output stream initialized").mixer());
    sink.set_volume(sink_volume(volume));
    sink.append(source);
    if !playing {
        sink.pause();
    }
    Ok(Active {
        sink,
        generation,
        request_id,
        uri,
        last_tick: Instant::now(),
    })
}

fn position_ms(sink: &Sink) -> u32 {
    duration_ms(sink.get_pos())
}

fn duration_ms(duration: Duration) -> u32 {
    u32::try_from(duration.as_millis()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recv_until(
        receiver: &mut tokio_mpsc::UnboundedReceiver<NeutralEvent>,
        timeout: Duration,
        matches: impl Fn(&NeutralEvent) -> bool,
    ) -> NeutralEvent {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(event) = receiver.try_recv() {
                if matches(&event) {
                    return event;
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for playback event"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn missing_file_emits_unavailable() {
        let (events, mut receiver) = tokio_mpsc::unbounded_channel();
        let mut engine = FileEngine::new(events, 7, 50);
        engine
            .load("file:///definitely/missing/song.mp3", true, 0)
            .unwrap();
        assert!(matches!(
            receiver.blocking_recv(),
            Some(NeutralEvent::RequestIdChanged {
                generation: 7,
                request_id: 1,
            })
        ));
        assert!(matches!(
            receiver.blocking_recv(),
            Some(NeutralEvent::Loading {
                generation: 7,
                request_id: 1,
                ref uri,
                position_ms: 0,
            }) if uri == "file:///definitely/missing/song.mp3"
        ));
        assert!(matches!(
            receiver.blocking_recv(),
            Some(NeutralEvent::Unavailable {
                generation: 7,
                request_id: 1,
                ref uri,
            }) if uri == "file:///definitely/missing/song.mp3"
        ));
    }

    #[test]
    fn queued_load_and_stop_have_one_ordered_event_producer() {
        let (events, mut event_receiver) = tokio_mpsc::unbounded_channel();
        let (commands, command_receiver) = mpsc::channel();
        let mut engine = FileEngine {
            commands,
            events,
            generation: 7,
            request_id: 0,
            uri: None,
            playing: false,
            volume: 50,
        };

        engine
            .load("file:///definitely/missing/song.mp3", true, 0)
            .unwrap();
        engine.stop();

        assert!(matches!(
            event_receiver.try_recv(),
            Ok(NeutralEvent::RequestIdChanged { .. })
        ));
        assert!(matches!(
            event_receiver.try_recv(),
            Ok(NeutralEvent::Loading { .. })
        ));
        assert!(event_receiver.try_recv().is_err());
        assert!(matches!(
            command_receiver.recv().unwrap(),
            Command::Load { .. }
        ));
        assert!(matches!(
            command_receiver.recv().unwrap(),
            Command::Stop(Some(Stopped {
                generation: 7,
                request_id: 1,
                ..
            }))
        ));
    }

    #[test]
    #[ignore = "requires a real audio output device"]
    fn plays_wav_on_real_device_for_200ms() {
        let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../crates/retune-audio/tests/fixtures/cc0-audio.wav");
        let (events, mut receiver) = tokio_mpsc::unbounded_channel();
        let mut engine = FileEngine::new(events, 7, 50);
        let uri = crate::localfiles::file_uri(&fixture);
        engine.load(&uri, true, 0).unwrap();
        assert!(matches!(
            receiver.blocking_recv(),
            Some(NeutralEvent::RequestIdChanged {
                generation: 7,
                request_id: 1,
            })
        ));
        assert!(matches!(
            receiver.blocking_recv(),
            Some(NeutralEvent::Loading {
                generation: 7,
                request_id: 1,
                uri: ref event_uri,
                position_ms: 0,
            }) if event_uri == &uri
        ));
        assert!(matches!(
            receiver.blocking_recv(),
            Some(NeutralEvent::Playing {
                generation: 7,
                request_id: 1,
                uri: ref event_uri,
                position_ms: 0,
            }) if event_uri == &uri
        ));
        std::thread::sleep(Duration::from_millis(200));

        engine.pause().unwrap();
        let paused = recv_until(&mut receiver, Duration::from_secs(1), |event| {
            matches!(
                event,
                NeutralEvent::Paused {
                    generation: 7,
                    request_id: 1,
                    uri: event_uri,
                    ..
                } if event_uri == &uri
            )
        });
        assert!(matches!(paused, NeutralEvent::Paused { .. }));

        engine.resume().unwrap();
        let resumed = recv_until(&mut receiver, Duration::from_secs(1), |event| {
            matches!(
                event,
                NeutralEvent::Playing {
                    generation: 7,
                    request_id: 1,
                    uri: event_uri,
                    ..
                } if event_uri == &uri
            )
        });
        assert!(matches!(resumed, NeutralEvent::Playing { .. }));

        engine.seek(1).unwrap();
        let seeked = recv_until(&mut receiver, Duration::from_secs(1), |event| {
            matches!(
                event,
                NeutralEvent::Seeked {
                    generation: 7,
                    request_id: 1,
                    uri: event_uri,
                    position_ms: 1000,
                } if event_uri == &uri
            )
        });
        assert!(matches!(seeked, NeutralEvent::Seeked { .. }));
        engine.set_volume(25);

        let position = recv_until(&mut receiver, Duration::from_secs(2), |event| {
            matches!(
                event,
                NeutralEvent::PositionChanged {
                    generation: 7,
                    request_id: 1,
                    uri: event_uri,
                    position_ms,
                } if event_uri == &uri && *position_ms > 0
            )
        });
        assert!(matches!(position, NeutralEvent::PositionChanged { .. }));
        let ended = recv_until(&mut receiver, Duration::from_secs(5), |event| {
            matches!(
                event,
                NeutralEvent::EndOfTrack {
                    generation: 7,
                    request_id: 1,
                    uri: event_uri,
                } if event_uri == &uri
            )
        });
        assert!(matches!(ended, NeutralEvent::EndOfTrack { .. }));
    }
}

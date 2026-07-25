//! Audio file probing, decoding, seeking, and metadata without an audio device.

mod import;

pub use import::{ImportedFile, import_file, scan_paths};

use std::{fs::File, path::Path, sync::OnceLock, time::Duration};

use lofty::{
    file::{AudioFile, TaggedFileExt},
    tag::Accessor,
};
use rodio::{ChannelCount, SampleRate, Source, source::SeekError};
use symphonia::core::{
    audio::{Channels, SampleBuffer},
    codecs::{CODEC_TYPE_NULL, CODEC_TYPE_OPUS, CodecRegistry, Decoder, DecoderOptions},
    errors::Error as SymphoniaError,
    formats::{FormatOptions, FormatReader, SeekMode, SeekTo},
    io::MediaSourceStream,
    meta::MetadataOptions,
    probe::{Hint, Probe},
};
use symphonia_adapter_libopus::OpusDecoder;
use thiserror::Error;

/// An error while reading an audio file.
#[derive(Debug, Error)]
pub enum AudioError {
    /// The file or its codec is not supported.
    #[error("unsupported audio: {0}")]
    Unsupported(String),
    /// The file could not be identified as audio.
    #[error("audio probe failed: {0}")]
    ProbeFailed(String),
    /// An audio packet could not be decoded.
    #[error("audio decode failed: {0}")]
    Decode(String),
    /// The container does not declare a duration.
    #[error("audio duration is unavailable")]
    DurationUnavailable,
    /// File access failed.
    #[error("audio I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

/// Basic stream properties available before playback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioInfo {
    /// Samples per channel per second.
    pub sample_rate: u32,
    /// Interleaved channel count.
    pub channels: u16,
    /// Container duration, when declared by the format.
    pub duration: Option<Duration>,
}

/// Embedded artwork bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Artwork {
    /// Declared image MIME type, when present.
    pub mime: Option<String>,
    /// Encoded image bytes.
    pub bytes: Vec<u8>,
}

/// Common read-only audio tags.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FileTags {
    /// Track title.
    pub title: Option<String>,
    /// Track artist.
    pub artist: Option<String>,
    /// Album title.
    pub album: Option<String>,
    /// Genre.
    pub genre: Option<String>,
    /// Track number.
    pub track_no: Option<u32>,
    /// Disc number.
    pub disc_no: Option<u32>,
    /// Audio duration reported by Lofty.
    pub duration: Duration,
    /// First embedded picture.
    pub artwork: Option<Artwork>,
}

/// A synchronous decoded PCM stream suitable for a rodio sink.
pub struct FileSource {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    info: AudioInfo,
    samples: Vec<f32>,
    sample_offset: usize,
    finished: bool,
}

type Opened = (
    Box<dyn FormatReader>,
    Box<dyn Decoder>,
    u32,
    AudioInfo,
    Vec<f32>,
);

impl std::fmt::Debug for FileSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FileSource")
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

impl FileSource {
    /// Opens an audio file and prepares its decoder.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AudioError> {
        let (format, decoder, track_id, info, samples) = open(path.as_ref())?;
        Ok(Self {
            format,
            decoder,
            track_id,
            info,
            samples,
            sample_offset: 0,
            finished: false,
        })
    }

    /// Returns the interleaved channel count.
    pub fn channels(&self) -> u16 {
        self.info.channels
    }

    /// Returns samples per channel per second.
    pub fn sample_rate(&self) -> u32 {
        self.info.sample_rate
    }

    /// Returns the container duration, when declared.
    pub fn duration(&self) -> Option<Duration> {
        self.info.duration
    }

    fn decode_packet(&mut self) -> bool {
        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(SymphoniaError::IoError(error))
                    if error.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    self.finished = true;
                    return false;
                }
                Err(error) => {
                    log::warn!("audio decode stopped: {error}");
                    self.finished = true;
                    return false;
                }
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    let mut buffer =
                        SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
                    buffer.copy_interleaved_ref(decoded);
                    self.samples.clear();
                    self.samples.extend_from_slice(buffer.samples());
                    self.sample_offset = 0;
                    if !self.samples.is_empty() {
                        return true;
                    }
                }
                Err(error) => {
                    log::warn!("audio decode stopped: {error}");
                    self.finished = true;
                    return false;
                }
            }
        }
    }
}

impl Iterator for FileSource {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.sample_offset == self.samples.len() && !self.finished && !self.decode_packet() {
            return None;
        }
        let sample = self.samples.get(self.sample_offset).copied();
        self.sample_offset += usize::from(sample.is_some());
        sample
    }
}

impl Source for FileSource {
    fn current_span_len(&self) -> Option<usize> {
        let remaining = self.samples.len().saturating_sub(self.sample_offset);
        (remaining > 0).then_some(remaining)
    }

    fn channels(&self) -> ChannelCount {
        FileSource::channels(self)
    }

    fn sample_rate(&self) -> SampleRate {
        FileSource::sample_rate(self)
    }

    fn total_duration(&self) -> Option<Duration> {
        self.duration()
    }

    fn try_seek(&mut self, position: Duration) -> Result<(), SeekError> {
        self.format
            .seek(
                SeekMode::Coarse,
                SeekTo::Time {
                    time: position.into(),
                    track_id: Some(self.track_id),
                },
            )
            .map_err(|error| SeekError::Other(Box::new(AudioError::Decode(error.to_string()))))?;
        self.decoder.reset();
        self.samples.clear();
        self.sample_offset = 0;
        self.finished = false;
        Ok(())
    }
}

/// Probes an audio file without opening an audio device.
pub fn probe(path: impl AsRef<Path>) -> Result<AudioInfo, AudioError> {
    open(path.as_ref()).map(|(_, _, _, info, _)| info)
}

/// Returns the container duration without opening an audio device.
pub fn probe_duration(path: impl AsRef<Path>) -> Result<Duration, AudioError> {
    probe(path)?.duration.ok_or(AudioError::DurationUnavailable)
}

/// Reads common tags and the first embedded artwork image.
pub fn read_tags(path: impl AsRef<Path>) -> Result<FileTags, AudioError> {
    let tagged = lofty::probe::Probe::open(path.as_ref())
        .map_err(|error| AudioError::ProbeFailed(error.to_string()))?
        .guess_file_type()
        .map_err(|error| AudioError::ProbeFailed(error.to_string()))?
        .read()
        .map_err(|error| AudioError::ProbeFailed(error.to_string()))?;
    let duration = tagged.properties().duration();
    let mut tags = FileTags {
        duration,
        ..Default::default()
    };
    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        tags.title = tag.title().map(|value| value.into_owned());
        tags.artist = tag.artist().map(|value| value.into_owned());
        tags.album = tag.album().map(|value| value.into_owned());
        tags.genre = tag.genre().map(|value| value.into_owned());
        tags.track_no = tag.track();
        tags.disc_no = tag.disk();
        tags.artwork = tag.pictures().first().map(|picture| Artwork {
            mime: picture.mime_type().map(|mime| mime.as_str().to_owned()),
            bytes: picture.data().to_vec(),
        });
    }
    Ok(tags)
}

fn open(path: &Path) -> Result<Opened, AudioError> {
    let file = File::open(path)?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|extension| extension.to_str()) {
        hint.with_extension(extension);
    }
    let format_options = FormatOptions {
        enable_gapless: true,
        ..Default::default()
    };
    let probed = probes()
        .format(&hint, stream, &format_options, &MetadataOptions::default())
        .map_err(map_probe_error)?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|track| track.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| AudioError::Unsupported("no audio track".to_owned()))?;
    let track_id = track.id;
    let mut params = track.codec_params.clone();
    if params.codec == CODEC_TYPE_OPUS && params.channels.is_none() {
        params.channels = params.extra_data.as_deref().and_then(opus_channels);
    }
    let duration = params
        .time_base
        .zip(params.n_frames)
        .map(|(base, frames)| base.calc_time(frames).into());
    let mut decoder = codecs()
        .make(&params, &DecoderOptions::default())
        .map_err(map_decode_error)?;
    let (sample_rate, channels, samples) = loop {
        let packet = format.next_packet().map_err(map_probe_error)?;
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(decoded) => {
                let sample_rate = decoded.spec().rate;
                let channels = decoded.spec().channels.count() as u16;
                let mut buffer =
                    SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
                buffer.copy_interleaved_ref(decoded);
                break (sample_rate, channels, buffer.samples().to_vec());
            }
            Err(error) => return Err(map_decode_error(error)),
        }
    };
    Ok((
        format,
        decoder,
        track_id,
        AudioInfo {
            sample_rate,
            channels,
            duration,
        },
        samples,
    ))
}

fn opus_channels(extra_data: &[u8]) -> Option<Channels> {
    let header = extra_data
        .windows(8)
        .position(|bytes| bytes == b"OpusHead")?;
    match extra_data.get(header + 9) {
        Some(1) => Some(Channels::FRONT_CENTRE),
        Some(2) => Some(Channels::FRONT_LEFT | Channels::FRONT_RIGHT),
        _ => None,
    }
}

fn codecs() -> &'static CodecRegistry {
    static CODECS: OnceLock<CodecRegistry> = OnceLock::new();
    CODECS.get_or_init(|| {
        let mut codecs = CodecRegistry::new();
        symphonia::default::register_enabled_codecs(&mut codecs);
        codecs.register_all::<OpusDecoder>();
        codecs
    })
}

fn probes() -> &'static Probe {
    static PROBES: OnceLock<Probe> = OnceLock::new();
    PROBES.get_or_init(|| {
        let mut probes = Probe::default();
        symphonia::default::register_enabled_formats(&mut probes);
        probes
    })
}

fn map_probe_error(error: SymphoniaError) -> AudioError {
    match error {
        SymphoniaError::Unsupported(message) => AudioError::Unsupported(message.to_owned()),
        error => AudioError::ProbeFailed(error.to_string()),
    }
}

fn map_decode_error(error: SymphoniaError) -> AudioError {
    match error {
        SymphoniaError::Unsupported(message) => AudioError::Unsupported(message.to_owned()),
        error => AudioError::Decode(error.to_string()),
    }
}

//! Audio file probing, decoding, seeking, and metadata without an audio device.

mod import;

pub use import::{
    ImportedFile, MAX_SCAN_DEPTH, MAX_SCAN_FAILURE_DETAILS, MAX_SCAN_FILES, MAX_SCAN_ROOTS,
    ScanFailure, ScanResult, import_file, scan_path, scan_paths,
};

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use lofty::{
    config::ParseOptions,
    file::{AudioFile, TaggedFileExt},
    tag::Accessor,
};
use rodio::{ChannelCount, SampleRate, Source, source::SeekError};
use symphonia::core::{
    audio::{Channels, SampleBuffer},
    codecs::{
        CODEC_TYPE_AAC, CODEC_TYPE_ALAC, CODEC_TYPE_FLAC, CODEC_TYPE_MP3, CODEC_TYPE_NULL,
        CODEC_TYPE_OPUS, CODEC_TYPE_VORBIS, CodecRegistry, CodecType, Decoder, DecoderOptions,
    },
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
    /// File access failed.
    #[error("audio I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// An embedded artwork field exceeds the caller's byte ceiling.
    #[error("embedded artwork exceeds the {max_bytes}-byte limit")]
    ArtworkTooLarge {
        /// Maximum accepted encoded artwork bytes.
        max_bytes: usize,
    },
}

/// Basic stream properties available before playback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioInfo {
    /// Codec declared by the selected audio stream.
    pub codec: CodecType,
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
    status: FileSourceStatus,
}

/// Shared terminal failure state for a [`FileSource`] consumed by a rodio sink.
#[derive(Clone, Debug, Default)]
pub struct FileSourceStatus(Arc<Mutex<Option<String>>>);

impl FileSourceStatus {
    /// Takes the fatal demux or decode error that ended the source, if any.
    pub fn take_failure(&self) -> Option<String> {
        self.0.lock().expect("file source status poisoned").take()
    }

    fn fail(&self, error: &SymphoniaError) {
        *self.0.lock().expect("file source status poisoned") = Some(error.to_string());
    }

    fn reset(&self) {
        *self.0.lock().expect("file source status poisoned") = None;
    }
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
            status: FileSourceStatus::default(),
        })
    }

    /// Returns a handle to the source's terminal failure state.
    pub fn status(&self) -> FileSourceStatus {
        self.status.clone()
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
                Err(error) => {
                    if !handle_demux_error(&self.status, &error) {
                        log::warn!("audio decode stopped: {error}");
                    }
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
                Err(error) if handle_decoder_error(&self.status, &error) => {
                    log::warn!("skipping undecodable audio packet: {error}");
                }
                Err(error) => {
                    log::warn!("audio decode stopped: {error}");
                    self.status.fail(&error);
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
        self.status.reset();
        Ok(())
    }
}

/// Probes an audio file without opening an audio device.
pub fn probe(path: impl AsRef<Path>) -> Result<AudioInfo, AudioError> {
    open(path.as_ref()).map(|(_, _, _, info, _)| info)
}

/// Returns the iTunes-style kind for a codec/path pair.
///
/// The codec disambiguates containers such as M4A and Ogg; the extension
/// keeps legacy-library backfill cheap when probing is unavailable.
pub fn audio_kind(codec: Option<CodecType>, path: impl AsRef<Path>) -> Option<&'static str> {
    let extension = path
        .as_ref()
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    match (codec, extension.as_deref()) {
        (_, Some("webm")) => Some("WebM audio file"),
        (_, Some("wav")) => Some("WAV audio file"),
        (_, Some("aif" | "aiff")) => Some("AIFF audio file"),
        (Some(CODEC_TYPE_ALAC), _) => Some("Apple Lossless audio file"),
        (Some(CODEC_TYPE_AAC), _) | (None, Some("aac" | "m4a" | "mp4")) => Some("AAC audio file"),
        (Some(CODEC_TYPE_MP3), _) | (None, Some("mp3")) => Some("MPEG audio file"),
        (Some(CODEC_TYPE_FLAC), _) | (None, Some("flac")) => Some("FLAC audio file"),
        (Some(CODEC_TYPE_OPUS), _) | (None, Some("opus")) => Some("Opus audio file"),
        (Some(CODEC_TYPE_VORBIS), _) | (None, Some("oga" | "ogg")) => Some("Ogg Vorbis audio file"),
        _ => None,
    }
}

/// Reads common tags and the first embedded artwork image.
pub fn read_tags(path: impl AsRef<Path>) -> Result<FileTags, AudioError> {
    read_tags_with_artwork(path.as_ref(), true)
}

/// Reads the first embedded artwork image when it fits the caller's byte ceiling.
pub fn read_artwork(
    path: impl AsRef<Path>,
    max_bytes: usize,
) -> Result<Option<Artwork>, AudioError> {
    let path = path.as_ref();
    let file_type = lofty::probe::Probe::open(path)
        .map_err(|error| AudioError::ProbeFailed(error.to_string()))?
        .guess_file_type()
        .map_err(|error| AudioError::ProbeFailed(error.to_string()))?
        .file_type();
    match file_type {
        Some(lofty::file::FileType::Mpeg) => {
            reject_ape_tag(path)?;
            check_id3_artwork_size(path, max_bytes)?;
        }
        Some(lofty::file::FileType::Flac) => check_flac_artwork_size(path, max_bytes)?,
        Some(lofty::file::FileType::Mp4) => check_mp4_artwork_size(path, max_bytes)?,
        Some(file_type) => {
            return Err(AudioError::Unsupported(format!(
                "bounded embedded artwork is not supported for {file_type:?} files"
            )));
        }
        None => return Err(AudioError::Unsupported("unknown file type".into())),
    }
    read_tags(path).map(|tags| tags.artwork)
}

fn check_id3_artwork_size(path: &Path, max_bytes: usize) -> Result<(), AudioError> {
    let mut file = File::open(path)?;
    let mut header = [0; 10];
    file.read_exact(&mut header)?;
    if &header[..3] != b"ID3" {
        return Ok(());
    }
    let version = header[3];
    if !matches!(version, 2..=4) || header[5] & 0x80 != 0 || (version == 2 && header[5] & 0x40 != 0)
    {
        return Err(AudioError::Unsupported(
            "bounded artwork does not support this ID3 tag".into(),
        ));
    }
    let tag_size = synchsafe_u32(&header[6..10])? as u64;
    let mut remaining = tag_size;
    if version >= 3 && header[5] & 0x40 != 0 {
        let mut size = [0; 4];
        file.read_exact(&mut size)?;
        let extended_size = if version == 4 {
            synchsafe_u32(&size)? as u64
        } else {
            u32::from_be_bytes(size) as u64 + 4
        };
        if extended_size < 4 || extended_size > remaining {
            return Err(AudioError::ProbeFailed(
                "invalid ID3 extended header".into(),
            ));
        }
        file.seek(SeekFrom::Current((extended_size - 4) as i64))?;
        remaining -= extended_size;
    }
    let (header_len, picture_id) = if version == 2 {
        (6usize, b"PIC".as_slice())
    } else {
        (10usize, b"APIC".as_slice())
    };
    let mut artwork_bytes = 0;
    while remaining >= header_len as u64 {
        let mut frame = [0; 10];
        file.read_exact(&mut frame[..header_len])?;
        remaining -= header_len as u64;
        let id = &frame[..picture_id.len()];
        if id.iter().all(|byte| *byte == 0) {
            break;
        }
        let size = if version == 2 {
            u32::from_be_bytes([0, frame[3], frame[4], frame[5]])
        } else if version == 4 {
            synchsafe_u32(&frame[4..8])?
        } else {
            u32::from_be_bytes(frame[4..8].try_into().expect("four-byte frame size"))
        } as u64;
        if size > remaining {
            return Err(AudioError::ProbeFailed("invalid ID3 frame size".into()));
        }
        if id == picture_id {
            if version == 3 && frame[9] & 0xc0 != 0 || version == 4 && frame[9] & 0x0e != 0 {
                return Err(AudioError::Unsupported(
                    "bounded artwork does not support transformed ID3 picture frames".into(),
                ));
            }
            add_artwork_bytes(&mut artwork_bytes, size, max_bytes)?;
        }
        file.seek(SeekFrom::Current(size as i64))?;
        remaining -= size;
    }
    Ok(())
}

fn add_artwork_bytes(total: &mut u64, bytes: u64, max_bytes: usize) -> Result<(), AudioError> {
    *total = total
        .checked_add(bytes)
        .ok_or_else(|| AudioError::ProbeFailed("embedded artwork size overflow".into()))?;
    if *total > max_bytes as u64 {
        return Err(AudioError::ArtworkTooLarge { max_bytes });
    }
    Ok(())
}

fn reject_ape_tag(path: &Path) -> Result<(), AudioError> {
    let mut file = File::open(path)?;
    let mut buffer = [0; 8192];
    let mut overlap = Vec::new();
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        overlap.extend_from_slice(&buffer[..read]);
        if overlap.windows(8).any(|window| window == b"APETAGEX") {
            return Err(AudioError::Unsupported(
                "bounded artwork does not support APE tags in MPEG files".into(),
            ));
        }
        let keep_from = overlap.len().saturating_sub(7);
        overlap.drain(..keep_from);
    }
}

fn synchsafe_u32(bytes: &[u8]) -> Result<u32, AudioError> {
    if bytes.len() != 4 || bytes.iter().any(|byte| byte & 0x80 != 0) {
        return Err(AudioError::ProbeFailed("invalid synchsafe integer".into()));
    }
    Ok(bytes
        .iter()
        .fold(0, |value, byte| (value << 7) | u32::from(*byte)))
}

fn check_flac_artwork_size(path: &Path, max_bytes: usize) -> Result<(), AudioError> {
    let mut file = File::open(path)?;
    let mut magic = [0; 4];
    file.read_exact(&mut magic)?;
    if &magic != b"fLaC" {
        return Err(AudioError::Unsupported(
            "bounded artwork requires a native FLAC header".into(),
        ));
    }
    let mut artwork_bytes = 0;
    loop {
        let mut header = [0; 4];
        file.read_exact(&mut header)?;
        let last = header[0] & 0x80 != 0;
        let kind = header[0] & 0x7f;
        let size = u32::from_be_bytes([0, header[1], header[2], header[3]]) as u64;
        if kind == 6 {
            check_flac_picture_block(&mut file, size, max_bytes, &mut artwork_bytes)?;
        } else if kind == 4 {
            check_flac_comments(&mut file, size)?;
        } else {
            file.seek(SeekFrom::Current(size as i64))?;
        }
        if last {
            return Ok(());
        }
    }
}

fn check_flac_comments(file: &mut File, block_size: u64) -> Result<(), AudioError> {
    const PICTURE: &[u8] = b"METADATA_BLOCK_PICTURE=";
    const COVERART: &[u8] = b"COVERART=";

    let start = file.stream_position()?;
    let vendor_len = read_le_u32(file)? as u64;
    file.seek(SeekFrom::Current(vendor_len as i64))?;
    let comments = read_le_u32(file)?;
    for _ in 0..comments {
        let length = read_le_u32(file)? as u64;
        let mut prefix = [0; 23];
        let prefix_len = usize::try_from(length.min(prefix.len() as u64)).unwrap();
        file.read_exact(&mut prefix[..prefix_len])?;
        if ascii_prefix(&prefix[..prefix_len], PICTURE)
            || ascii_prefix(&prefix[..prefix_len], COVERART)
        {
            return Err(AudioError::Unsupported(
                "bounded artwork does not support pictures in FLAC comments".into(),
            ));
        }
        file.seek(SeekFrom::Current((length - prefix_len as u64) as i64))?;
    }
    let consumed = file.stream_position()?.saturating_sub(start);
    if consumed > block_size {
        return Err(AudioError::ProbeFailed("invalid FLAC comment block".into()));
    }
    file.seek(SeekFrom::Start(start + block_size))?;
    Ok(())
}

fn ascii_prefix(bytes: &[u8], prefix: &[u8]) -> bool {
    bytes
        .get(..prefix.len())
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
}

fn check_flac_picture_block(
    file: &mut File,
    block_size: u64,
    max_bytes: usize,
    artwork_bytes: &mut u64,
) -> Result<(), AudioError> {
    let start = file.stream_position()?;
    file.seek(SeekFrom::Current(4))?; // picture type
    let mime_len = read_be_u32(file)? as u64;
    file.seek(SeekFrom::Current(mime_len as i64))?;
    let description_len = read_be_u32(file)? as u64;
    file.seek(SeekFrom::Current(description_len as i64 + 16))?; // dimensions and color data
    let artwork_len = read_be_u32(file)? as usize;
    if file.stream_position()?.saturating_sub(start) > block_size
        || artwork_len as u64 > block_size.saturating_sub(file.stream_position()? - start)
    {
        return Err(AudioError::ProbeFailed("invalid FLAC picture block".into()));
    }
    add_artwork_bytes(artwork_bytes, artwork_len as u64, max_bytes)?;
    file.seek(SeekFrom::Start(start.checked_add(block_size).ok_or_else(
        || AudioError::ProbeFailed("FLAC picture block size overflow".into()),
    )?))?;
    Ok(())
}

fn read_be_u32(reader: &mut impl Read) -> Result<u32, AudioError> {
    let mut bytes = [0; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_be_bytes(bytes))
}

fn read_le_u32(reader: &mut impl Read) -> Result<u32, AudioError> {
    let mut bytes = [0; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn check_mp4_artwork_size(path: &Path, max_bytes: usize) -> Result<(), AudioError> {
    let mut file = File::open(path)?;
    let end = file.metadata()?.len();
    let mut artwork_bytes = 0;
    check_mp4_atoms(&mut file, end, max_bytes, 0, &mut artwork_bytes)
}

fn check_mp4_atoms(
    file: &mut File,
    end: u64,
    max_bytes: usize,
    depth: u8,
    artwork_bytes: &mut u64,
) -> Result<(), AudioError> {
    if depth > 8 {
        return Err(AudioError::ProbeFailed("invalid MP4 atom nesting".into()));
    }
    while file.stream_position()? < end {
        let start = file.stream_position()?;
        if end - start < 8 {
            return Err(AudioError::ProbeFailed("truncated MP4 atom".into()));
        }
        let size32 = read_be_u32(file)? as u64;
        let mut kind = [0; 4];
        file.read_exact(&mut kind)?;
        let atom_size = if size32 == 1 {
            let mut size = [0; 8];
            file.read_exact(&mut size)?;
            u64::from_be_bytes(size)
        } else if size32 == 0 {
            end - start
        } else {
            size32
        };
        let minimum = if size32 == 1 { 16 } else { 8 };
        let atom_end = start
            .checked_add(atom_size)
            .ok_or_else(|| AudioError::ProbeFailed("MP4 atom size overflow".into()))?;
        if atom_size < minimum || atom_end > end {
            return Err(AudioError::ProbeFailed("invalid MP4 atom size".into()));
        }
        match &kind {
            b"moov" | b"udta" | b"ilst" => {
                check_mp4_atoms(file, atom_end, max_bytes, depth + 1, artwork_bytes)?;
            }
            b"meta" => {
                if atom_end - file.stream_position()? < 4 {
                    return Err(AudioError::ProbeFailed("invalid MP4 meta atom".into()));
                }
                file.seek(SeekFrom::Current(4))?;
                check_mp4_atoms(file, atom_end, max_bytes, depth + 1, artwork_bytes)?;
            }
            b"covr" => check_mp4_cover(file, atom_end, max_bytes, artwork_bytes)?,
            _ => {}
        }
        file.seek(SeekFrom::Start(atom_end))?;
    }
    Ok(())
}

fn check_mp4_cover(
    file: &mut File,
    end: u64,
    max_bytes: usize,
    artwork_bytes: &mut u64,
) -> Result<(), AudioError> {
    while file.stream_position()? < end {
        let start = file.stream_position()?;
        if end - start < 8 {
            return Err(AudioError::ProbeFailed("truncated MP4 cover atom".into()));
        }
        let size32 = read_be_u32(file)? as u64;
        let mut kind = [0; 4];
        file.read_exact(&mut kind)?;
        let size = if size32 == 1 {
            let mut size = [0; 8];
            file.read_exact(&mut size)?;
            u64::from_be_bytes(size)
        } else if size32 == 0 {
            end - start
        } else {
            size32
        };
        let header_len = if size32 == 1 { 16 } else { 8 };
        let atom_end = start
            .checked_add(size)
            .ok_or_else(|| AudioError::ProbeFailed("MP4 cover atom size overflow".into()))?;
        if size < header_len || atom_end > end {
            return Err(AudioError::ProbeFailed(
                "invalid MP4 cover atom size".into(),
            ));
        }
        if &kind == b"data" {
            let content_len = size - header_len;
            if content_len < 8 {
                return Err(AudioError::ProbeFailed("invalid MP4 cover data".into()));
            }
            add_artwork_bytes(artwork_bytes, content_len - 8, max_bytes)?;
        }
        file.seek(SeekFrom::Start(atom_end))?;
    }
    Ok(())
}

/// Reads common tags without loading embedded artwork.
pub fn read_basic_tags(path: impl AsRef<Path>) -> Result<FileTags, AudioError> {
    read_tags_with_artwork(path.as_ref(), false)
}

fn read_tags_with_artwork(path: &Path, artwork: bool) -> Result<FileTags, AudioError> {
    let tagged = lofty::probe::Probe::open(path)
        .map_err(|error| AudioError::ProbeFailed(error.to_string()))?
        .guess_file_type()
        .map_err(|error| AudioError::ProbeFailed(error.to_string()))?
        .options(ParseOptions::new().read_cover_art(artwork))
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
        if artwork {
            tags.artwork = tag.pictures().first().map(|picture| Artwork {
                mime: picture.mime_type().map(|mime| mime.as_str().to_owned()),
                bytes: picture.data().to_vec(),
            });
        }
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
            Err(error) if decoder_error_is_recoverable(&error) => {
                log::warn!("skipping undecodable audio packet: {error}");
            }
            Err(error) => return Err(map_decode_error(error)),
        }
    };
    Ok((
        format,
        decoder,
        track_id,
        AudioInfo {
            codec: params.codec,
            sample_rate,
            channels,
            duration,
        },
        samples,
    ))
}

fn opus_channels(extra_data: &[u8]) -> Option<Channels> {
    if extra_data.len() < 19 || !extra_data.starts_with(b"OpusHead") || extra_data[8] > 0x0f {
        return None;
    }
    match extra_data[9] {
        1 => Some(Channels::FRONT_CENTRE),
        2 => Some(Channels::FRONT_LEFT | Channels::FRONT_RIGHT),
        _ => None,
    }
}

fn decoder_error_is_recoverable(error: &SymphoniaError) -> bool {
    matches!(
        error,
        SymphoniaError::DecodeError(_) | SymphoniaError::IoError(_)
    )
}

fn handle_decoder_error(status: &FileSourceStatus, error: &SymphoniaError) -> bool {
    let recoverable = decoder_error_is_recoverable(error);
    if !recoverable {
        status.fail(error);
    }
    recoverable
}

fn handle_demux_error(status: &FileSourceStatus, error: &SymphoniaError) -> bool {
    let clean_eof = matches!(
        error,
        SymphoniaError::IoError(error)
            if error.kind() == std::io::ErrorKind::UnexpectedEof
    );
    if !clean_eof {
        status.fail(error);
    }
    clean_eof
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

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    #[test]
    fn recoverable_packet_error_keeps_later_pcm() {
        let mut bytes = include_bytes!("../tests/fixtures/cc0-audio-aac-lc.aac").to_vec();
        let corrupt_at = bytes.len() / 4;
        bytes[corrupt_at..corrupt_at + 64].fill(0);
        let mut file = tempfile::Builder::new().suffix(".aac").tempfile().unwrap();
        file.write_all(&bytes).unwrap();
        file.flush().unwrap();

        let mut source = FileSource::open(file.path()).unwrap();
        let status = source.status();
        let samples = source.by_ref().count();
        let clean_samples = FileSource::open(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cc0-audio-aac-lc.aac"),
        )
        .unwrap()
        .count();

        assert!(samples > clean_samples / 2);
        assert_eq!(status.take_failure(), None);
    }

    #[test]
    fn clean_eof_has_no_failure() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cc0-audio.wav");
        let mut source = FileSource::open(path).unwrap();
        let status = source.status();

        assert!(source.by_ref().count() > 0);
        assert_eq!(status.take_failure(), None);
    }

    #[test]
    fn only_packet_local_decoder_errors_are_recoverable() {
        let status = FileSourceStatus::default();
        assert!(handle_decoder_error(
            &status,
            &SymphoniaError::DecodeError("damaged packet")
        ));
        assert!(handle_decoder_error(
            &status,
            &SymphoniaError::IoError(std::io::Error::other("packet read"))
        ));
        assert_eq!(status.take_failure(), None);

        assert!(!handle_decoder_error(
            &status,
            &SymphoniaError::Unsupported("codec changed")
        ));
        assert_eq!(
            status.take_failure().as_deref(),
            Some("unsupported feature: codec changed")
        );
        assert_eq!(status.take_failure(), None);
    }

    #[test]
    fn only_unexpected_eof_is_a_clean_demux_terminal() {
        let status = FileSourceStatus::default();
        assert!(handle_demux_error(
            &status,
            &SymphoniaError::IoError(std::io::Error::from(std::io::ErrorKind::UnexpectedEof))
        ));
        assert_eq!(status.take_failure(), None);

        assert!(!handle_demux_error(&status, &SymphoniaError::ResetRequired));
        assert_eq!(
            status.take_failure().as_deref(),
            Some("decoder needs to be reset")
        );
    }

    #[test]
    fn opus_head_requires_valid_fixed_header() {
        let mut header = [0; 19];
        header[..8].copy_from_slice(b"OpusHead");
        header[8] = 1;

        header[9] = 1;
        assert_eq!(opus_channels(&header), Some(Channels::FRONT_CENTRE));
        header[9] = 2;
        assert_eq!(
            opus_channels(&header),
            Some(Channels::FRONT_LEFT | Channels::FRONT_RIGHT)
        );

        assert_eq!(opus_channels(&header[..18]), None);
        let mut prefixed = [0; 20];
        prefixed[1..9].copy_from_slice(b"OpusHead");
        prefixed[9] = 1;
        prefixed[10] = 2;
        assert_eq!(opus_channels(&prefixed), None);

        header[8] = 0x10;
        assert_eq!(opus_channels(&header), None);
        header[8] = 1;
        for channels in [0, 3] {
            header[9] = channels;
            assert_eq!(opus_channels(&header), None);
        }
    }
}

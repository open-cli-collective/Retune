use std::{path::PathBuf, time::Duration};

use retune_audio::{AudioError, FileSource, probe};
use rodio::Source;

const FORMATS: &[&str] = &[
    "cc0-audio.mp3",
    "cc0-audio.flac",
    "cc0-audio-vorbis.ogg",
    "cc0-audio-aac-lc.aac",
    "cc0-audio-aac-lc.m4a",
    "cc0-audio-alac.m4a",
    "cc0-audio.wav",
    "cc0-audio.aiff",
    "cc0-audio-opus.ogg",
    "cc0-audio-opus.webm",
];

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn all_supported_formats_probe_and_decode_to_eof() {
    for name in FORMATS {
        let info = probe(fixture(name)).unwrap_or_else(|error| panic!("{name}: {error}"));
        let mut source =
            FileSource::open(fixture(name)).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(source.sample_rate(), info.sample_rate, "{name}");
        assert_eq!(source.channels(), info.channels, "{name}");

        let samples = source.by_ref().count();
        let decoded_seconds = samples as f64 / (info.sample_rate as f64 * info.channels as f64);
        assert!(
            (decoded_seconds - 2.4).abs() <= 0.5,
            "{name}: {decoded_seconds}s"
        );

        // Raw ADTS has no container duration and may therefore report None.
        if let Some(duration) = info.duration {
            assert!(
                (duration.as_secs_f64() - 2.4).abs() <= 0.5,
                "{name}: {duration:?}"
            );
        } else {
            assert!(name.ends_with(".aac"), "{name}: missing duration");
        }
    }
}

#[test]
fn wav_reports_48_khz() {
    let source = FileSource::open(fixture("cc0-audio.wav")).unwrap();
    assert_eq!(source.sample_rate(), 48_000);
    assert_eq!(source.channels(), 2);
    assert!((source.duration().unwrap().as_secs_f64() - 2.4).abs() <= 0.5);
}

#[test]
fn seek_reduces_remaining_audio_for_containers() {
    for name in FORMATS.iter().filter(|name| !name.ends_with(".aac")) {
        let mut source =
            FileSource::open(fixture(name)).unwrap_or_else(|error| panic!("{name}: {error}"));
        let prefix_samples = source.sample_rate() as usize * source.channels() as usize / 2;
        assert_eq!(source.by_ref().take(prefix_samples).count(), prefix_samples);
        source
            .try_seek(Duration::from_secs(1))
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let rate = source.sample_rate() as f64;
        let channels = source.channels() as f64;
        let actual_prefix: Vec<_> = source.by_ref().take(4096).collect();
        if *name == "cc0-audio.mp3" {
            let mut reference =
                FileSource::open(fixture(name)).unwrap_or_else(|error| panic!("{name}: {error}"));
            reference
                .try_seek(Duration::from_secs(1))
                .unwrap_or_else(|error| panic!("{name}: reference seek: {error}"));
            let expected_prefix: Vec<_> = reference.by_ref().take(4096).collect();
            assert!(
                actual_prefix == expected_prefix,
                "{name}: decoder was not reset"
            );
        }
        let samples = actual_prefix.len() + source.count();
        let seconds = samples as f64 / (rate * channels);
        assert!(
            (seconds - 1.4).abs() <= 0.5,
            "{name}: {seconds}s after seek"
        );
    }
}

#[test]
fn malformed_input_fails_cleanly() {
    for error in [
        probe(fixture("not-audio.mp3")).unwrap_err(),
        FileSource::open(fixture("not-audio.mp3")).unwrap_err(),
    ] {
        assert!(matches!(
            error,
            AudioError::Unsupported(_) | AudioError::ProbeFailed(_)
        ));
    }
}

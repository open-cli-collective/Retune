use std::path::PathBuf;

use retune_audio::{AudioError, read_tags};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn known_tags_and_artwork_are_read() {
    for name in [
        "cc0-audio-tagged.mp3",
        "cc0-audio-tagged.flac",
        "cc0-audio-tagged.m4a",
    ] {
        let tags = read_tags(fixture(name)).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(tags.title.as_deref(), Some("Fixture Song"), "{name}");
        assert_eq!(tags.artist.as_deref(), Some("Fixture Artist"), "{name}");
        assert_eq!(tags.album.as_deref(), Some("Fixture Album"), "{name}");
        assert_eq!(tags.genre.as_deref(), Some("Fixture Genre"), "{name}");
        assert_eq!(tags.track_no, Some(7), "{name}");
        assert_eq!(tags.disc_no, Some(2), "{name}");
        assert!((tags.duration.as_secs_f64() - 2.4).abs() <= 0.5, "{name}");

        if name.ends_with(".m4a") {
            assert!(tags.artwork.is_none(), "{name}");
        } else {
            let artwork = tags
                .artwork
                .as_ref()
                .unwrap_or_else(|| panic!("{name}: artwork"));
            assert!(
                artwork.mime.as_deref() == Some("image/png")
                    || artwork.bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
                "{name}: artwork is not PNG"
            );
        }
    }
}

#[test]
fn untagged_audio_has_no_tags_or_artwork() {
    let tags = read_tags(fixture("cc0-audio.wav")).unwrap();
    assert_eq!(tags.title, None);
    assert_eq!(tags.artist, None);
    assert_eq!(tags.album, None);
    assert_eq!(tags.genre, None);
    assert_eq!(tags.track_no, None);
    assert_eq!(tags.disc_no, None);
    assert!((tags.duration.as_secs_f64() - 2.4).abs() <= 0.5);
    assert_eq!(tags.artwork, None);
}

#[test]
fn malformed_input_tag_read_fails_cleanly() {
    assert!(matches!(
        read_tags(fixture("not-audio.mp3")).unwrap_err(),
        AudioError::Unsupported(_) | AudioError::ProbeFailed(_)
    ));
}

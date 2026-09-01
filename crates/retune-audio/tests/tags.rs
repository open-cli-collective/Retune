use std::path::PathBuf;

use lofty::{
    config::WriteOptions,
    picture::{MimeType, Picture},
    tag::{Tag, TagExt, TagType},
};
use retune_audio::{AudioError, read_artwork, read_basic_tags, read_tags};

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
        assert_eq!(
            read_artwork(fixture(name), 8 * 1024 * 1024).unwrap(),
            tags.artwork,
            "{name}"
        );
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
fn basic_tag_reads_preserve_text_without_materializing_embedded_artwork() {
    for name in ["cc0-audio-tagged.mp3", "cc0-audio-tagged.flac"] {
        let full = read_tags(fixture(name)).unwrap();
        let basic = read_basic_tags(fixture(name)).unwrap();

        assert!(full.artwork.is_some(), "{name}");
        assert!(basic.artwork.is_none(), "{name}");
        assert_eq!(basic.title, full.title, "{name}");
        assert_eq!(basic.artist, full.artist, "{name}");
        assert_eq!(basic.album, full.album, "{name}");
        assert_eq!(basic.genre, full.genre, "{name}");
        assert_eq!(basic.track_no, full.track_no, "{name}");
        assert_eq!(basic.disc_no, full.disc_no, "{name}");
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

#[test]
fn oversized_embedded_artwork_is_rejected_before_tag_parsing() {
    const MAX_ARTWORK_BYTES: usize = 8 * 1024 * 1024;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oversized-artwork.mp3");
    write_id3_fixture(&path, 3, &[(MAX_ARTWORK_BYTES + 1, 0)]);

    assert!(matches!(
        read_artwork(&path, MAX_ARTWORK_BYTES),
        Err(AudioError::ArtworkTooLarge {
            max_bytes: MAX_ARTWORK_BYTES
        })
    ));
}

fn write_id3_fixture(path: &std::path::Path, version: u8, pictures: &[(usize, u16)]) {
    const APIC_PREFIX: &[u8] = b"\0image/png\0\x03\0";

    let mut body = Vec::new();
    for (image_len, flags) in pictures {
        let frame_len = image_len + APIC_PREFIX.len();
        body.extend_from_slice(b"APIC");
        if version == 4 {
            body.extend_from_slice(&synchsafe(frame_len as u32));
        } else {
            body.extend_from_slice(&(frame_len as u32).to_be_bytes());
        }
        body.extend_from_slice(&flags.to_be_bytes());
        body.extend_from_slice(APIC_PREFIX);
        body.resize(body.len() + image_len, 0x5a);
    }
    let mut bytes = b"ID3".to_vec();
    bytes.extend_from_slice(&[version, 0, 0]);
    bytes.extend_from_slice(&synchsafe(body.len() as u32));
    bytes.extend_from_slice(&body);
    bytes.extend_from_slice(&std::fs::read(fixture("cc0-audio.mp3")).unwrap());
    std::fs::write(path, bytes).unwrap();
}

fn synchsafe(value: u32) -> [u8; 4] {
    [
        ((value >> 21) & 0x7f) as u8,
        ((value >> 14) & 0x7f) as u8,
        ((value >> 7) & 0x7f) as u8,
        (value & 0x7f) as u8,
    ]
}

#[test]
fn mp4_cover_art_accepts_the_exact_limit_and_rejects_one_more_byte() {
    const MAX_ARTWORK_BYTES: usize = 4096;

    let dir = tempfile::tempdir().unwrap();
    for image_len in [MAX_ARTWORK_BYTES, MAX_ARTWORK_BYTES + 1] {
        let path = dir.path().join(format!("cover-{image_len}.m4a"));
        std::fs::copy(fixture("cc0-audio-aac-lc.m4a"), &path).unwrap();
        let picture = Picture::unchecked(vec![0x5a; image_len])
            .mime_type(MimeType::Png)
            .build();
        let mut tag = Tag::new(TagType::Mp4Ilst);
        tag.push_picture(picture);
        tag.save_to_path(&path, WriteOptions::new()).unwrap();

        if image_len == MAX_ARTWORK_BYTES {
            assert_eq!(
                read_artwork(&path, MAX_ARTWORK_BYTES)
                    .unwrap()
                    .unwrap()
                    .bytes,
                vec![0x5a; image_len]
            );
        } else {
            assert!(matches!(
                read_artwork(&path, MAX_ARTWORK_BYTES),
                Err(AudioError::ArtworkTooLarge {
                    max_bytes: MAX_ARTWORK_BYTES
                })
            ));
        }
    }
}

#[test]
fn aggregate_artwork_bytes_are_bounded_for_each_supported_container() {
    const MAX_ARTWORK_BYTES: usize = 4096;
    const PICTURE_BYTES: usize = 2500;

    let dir = tempfile::tempdir().unwrap();
    let mp3 = dir.path().join("two-pictures.mp3");
    write_id3_fixture(&mp3, 3, &[(PICTURE_BYTES, 0), (PICTURE_BYTES, 0)]);
    assert_too_large(&mp3, MAX_ARTWORK_BYTES);

    let m4a = dir.path().join("two-pictures.m4a");
    std::fs::copy(fixture("cc0-audio-aac-lc.m4a"), &m4a).unwrap();
    let mut tag = Tag::new(TagType::Mp4Ilst);
    for _ in 0..2 {
        tag.push_picture(
            Picture::unchecked(vec![0x5a; PICTURE_BYTES])
                .mime_type(MimeType::Png)
                .build(),
        );
    }
    tag.save_to_path(&m4a, WriteOptions::new()).unwrap();
    assert_too_large(&m4a, MAX_ARTWORK_BYTES);

    let flac = dir.path().join("two-pictures.flac");
    let mut bytes = b"fLaC".to_vec();
    push_flac_picture(&mut bytes, PICTURE_BYTES, false);
    push_flac_picture(&mut bytes, PICTURE_BYTES, true);
    std::fs::write(&flac, bytes).unwrap();
    assert_too_large(&flac, MAX_ARTWORK_BYTES);
}

fn push_flac_picture(bytes: &mut Vec<u8>, image_len: usize, last: bool) {
    let block_len = 32 + image_len;
    bytes.extend_from_slice(&[
        if last { 0x86 } else { 0x06 },
        ((block_len >> 16) & 0xff) as u8,
        ((block_len >> 8) & 0xff) as u8,
        (block_len & 0xff) as u8,
    ]);
    bytes.extend_from_slice(&0u32.to_be_bytes()); // picture type
    bytes.extend_from_slice(&0u32.to_be_bytes()); // MIME length
    bytes.extend_from_slice(&0u32.to_be_bytes()); // description length
    bytes.extend_from_slice(&[0; 16]); // dimensions and color data
    bytes.extend_from_slice(&(image_len as u32).to_be_bytes());
    bytes.resize(bytes.len() + image_len, 0x5a);
}

fn assert_too_large(path: &std::path::Path, max_bytes: usize) {
    assert!(matches!(
        read_artwork(path, max_bytes),
        Err(AudioError::ArtworkTooLarge { max_bytes: actual }) if actual == max_bytes
    ));
}

#[test]
fn transformed_id3_pictures_and_overflowing_mp4_atoms_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    for (version, flag) in [
        (3, 0x0080),
        (3, 0x0040),
        (4, 0x0008),
        (4, 0x0004),
        (4, 0x0002),
    ] {
        let path = dir.path().join(format!("id3v2-{version}-{flag}.mp3"));
        write_id3_fixture(&path, version, &[(32, flag)]);
        assert!(matches!(
            read_artwork(&path, 4096),
            Err(AudioError::Unsupported(_))
        ));
    }

    let mp4 = dir.path().join("overflow.m4a");
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&24u32.to_be_bytes());
    bytes.extend_from_slice(b"ftypM4A \0\0\0\0M4A isom");
    bytes.extend_from_slice(&1u32.to_be_bytes());
    bytes.extend_from_slice(b"moov");
    bytes.extend_from_slice(&u64::MAX.to_be_bytes());
    std::fs::write(&mp4, bytes).unwrap();
    assert!(matches!(
        read_artwork(&mp4, 4096),
        Err(AudioError::ProbeFailed(_))
    ));
}

#[test]
fn unbounded_picture_paths_return_explicit_errors() {
    assert!(matches!(
        read_artwork(fixture("cc0-audio.wav"), 8 * 1024 * 1024),
        Err(AudioError::Unsupported(_))
    ));

    let dir = tempfile::tempdir().unwrap();
    let mp3 = dir.path().join("ape-tagged.mp3");
    let mut mp3_bytes = std::fs::read(fixture("cc0-audio.mp3")).unwrap();
    mp3_bytes.extend_from_slice(b"APETAGEX");
    std::fs::write(&mp3, mp3_bytes).unwrap();
    assert!(matches!(
        read_artwork(&mp3, 8 * 1024 * 1024),
        Err(AudioError::Unsupported(_))
    ));

    let flac = dir.path().join("comment-picture.flac");
    let comment = b"METADATA_BLOCK_PICTURE=AAAA";
    let block_len = 4 + 1 + 4 + 4 + comment.len();
    let mut flac_bytes = b"fLaC".to_vec();
    flac_bytes.extend_from_slice(&[
        0x84,
        ((block_len >> 16) & 0xff) as u8,
        ((block_len >> 8) & 0xff) as u8,
        (block_len & 0xff) as u8,
    ]);
    flac_bytes.extend_from_slice(&1u32.to_le_bytes());
    flac_bytes.push(b'x');
    flac_bytes.extend_from_slice(&1u32.to_le_bytes());
    flac_bytes.extend_from_slice(&(comment.len() as u32).to_le_bytes());
    flac_bytes.extend_from_slice(comment);
    std::fs::write(&flac, flac_bytes).unwrap();
    assert!(matches!(
        read_artwork(&flac, 8 * 1024 * 1024),
        Err(AudioError::Unsupported(_))
    ));
}

use std::{fs, path::PathBuf};

use retune_audio::{import_file, scan_paths};
use tempfile::tempdir;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn scan_is_sorted_recursive_and_filters_extensions() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("visible");
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join("nested")).unwrap();
    for name in [
        "z.MP3",
        "nested/a.flac",
        "nested/b.MP4",
        "nested/c.OpUs",
        "ignore.txt",
    ] {
        fs::write(root.join(name), []).unwrap();
    }

    let files = scan_paths(std::slice::from_ref(&root));
    assert_eq!(
        files,
        [
            root.join("nested/a.flac"),
            root.join("nested/b.MP4"),
            root.join("nested/c.OpUs"),
            root.join("z.MP3"),
        ]
        .map(|path| path.canonicalize().unwrap())
    );
}

#[cfg(unix)]
#[test]
fn scan_skips_hidden_entries_and_directory_symlinks_but_accepts_file_symlinks() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().unwrap();
    let root = dir.path().join("visible");
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join("real")).unwrap();
    fs::create_dir(root.join(".hidden")).unwrap();
    fs::write(root.join("real/song.mp3"), []).unwrap();
    fs::write(root.join(".hidden/hidden.mp3"), []).unwrap();
    fs::write(root.join(".hidden.mp3"), []).unwrap();
    symlink(root.join("real"), root.join("linked-dir")).unwrap();
    symlink(root.join("real/song.mp3"), root.join("linked-song.mp3")).unwrap();

    assert_eq!(
        scan_paths(std::slice::from_ref(&root)),
        [root.join("real/song.mp3").canonicalize().unwrap()]
    );
    assert!(scan_paths(&[root.join(".hidden.mp3")]).is_empty());
    assert!(scan_paths(&[root.join(".hidden")]).is_empty());
}

#[test]
fn import_preserves_tags_and_canonical_path() {
    let path = fixture("cc0-audio-tagged.mp3");
    let imported = import_file(&path).unwrap();
    assert_eq!(imported.canonical_path, path.canonicalize().unwrap());
    assert_eq!(imported.tags.title.as_deref(), Some("Fixture Song"));
    assert_eq!(imported.tags.artist.as_deref(), Some("Fixture Artist"));
    assert_eq!(imported.tags.album.as_deref(), Some("Fixture Album"));
    assert_eq!(imported.tags.genre.as_deref(), Some("Fixture Genre"));
    assert_eq!(imported.tags.track_no, Some(7));
    assert_eq!(imported.tags.disc_no, Some(2));
}

#[test]
fn untagged_file_imports_and_garbage_fails() {
    for name in [
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
    ] {
        import_file(fixture(name)).unwrap_or_else(|error| panic!("{name}: {error}"));
    }
    assert!(import_file(fixture("not-audio.mp3")).is_err());
}

use std::{
    fs::{self, File, FileTimes},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use md5::{Digest, Md5};
use retune_audio::Artwork;

use crate::persistence;

const MAGIC: &[u8; 8] = b"RTNART01";
const FIXED_HEADER_BYTES: usize = 40;
const MAX_MIME_BYTES: usize = 255;
const MAX_CACHE_ENTRY_BYTES: u64 =
    FIXED_HEADER_BYTES as u64 + MAX_MIME_BYTES as u64 + MAX_LOCAL_ARTWORK_BYTES as u64;

pub(crate) const CACHE_LIMIT_BYTES: u64 = 512 * 1024 * 1024;
pub(crate) const MAX_LOCAL_ARTWORK_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SourceStamp {
    length: u64,
    modified_seconds: u64,
    modified_nanos: u32,
}

impl SourceStamp {
    fn read(path: &Path) -> io::Result<Self> {
        let metadata = fs::metadata(path)?;
        let modified = metadata
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(Self {
            length: metadata.len(),
            modified_seconds: modified.as_secs(),
            modified_nanos: modified.subsec_nanos(),
        })
    }
}

#[derive(Clone)]
pub(crate) struct LocalArtworkCache {
    root: PathBuf,
    max_bytes: u64,
    io_gate: Arc<Mutex<()>>,
}

impl LocalArtworkCache {
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_max_bytes(root, CACHE_LIMIT_BYTES)
    }

    fn with_max_bytes(root: impl Into<PathBuf>, max_bytes: u64) -> Self {
        Self {
            root: root.into(),
            max_bytes,
            io_gate: Arc::new(Mutex::new(())),
        }
    }

    pub(crate) fn get_or_load<F>(
        &self,
        uri: &str,
        source_path: &Path,
        load: F,
    ) -> Result<Option<Artwork>, String>
    where
        F: FnOnce(&Path) -> Result<Option<Artwork>, String>,
    {
        let _guard = self
            .io_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let cache_path = self.entry_path(uri);
        let source_before = SourceStamp::read(source_path).ok();

        if let Some(source_before) = source_before {
            match read_entry(&cache_path, source_before) {
                Ok(artwork) if SourceStamp::read(source_path).ok() == Some(source_before) => {
                    touch(&cache_path);
                    return Ok(Some(artwork));
                }
                Ok(_) => {
                    let _ = fs::remove_file(&cache_path);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => {
                    let _ = fs::remove_file(&cache_path);
                }
            }
        }

        let artwork = load(source_path)?;
        if let (Some(source_before), Some(artwork)) = (source_before, artwork.as_ref()) {
            if SourceStamp::read(source_path).ok() == Some(source_before) {
                self.store_entry(&cache_path, source_before, artwork);
            }
        }
        Ok(artwork)
    }

    fn entry_path(&self, uri: &str) -> PathBuf {
        let digest = Md5::digest(uri.as_bytes());
        self.root.join(format!("{digest:x}"))
    }

    fn store_entry(&self, path: &Path, source: SourceStamp, artwork: &Artwork) {
        let Some(encoded) = encode_entry(source, artwork) else {
            return;
        };
        let entry_bytes = encoded.len() as u64;
        if entry_bytes > self.max_bytes
            || self
                .prune_other_entries(self.max_bytes - entry_bytes, path)
                .is_err()
        {
            return;
        }
        let _ = persistence::atomic_write(path, &encoded, Some(0o600));
    }

    fn prune_other_entries(&self, max_bytes: u64, replacing: &Path) -> io::Result<()> {
        // ponytail: scan owned entries on writes; add an index only if profiling finds this hot.
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let mut files = Vec::new();
        let mut total = 0u64;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path == replacing || !is_cache_entry_name(&entry.file_name()) {
                continue;
            }
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_file() => metadata,
                _ => continue,
            };
            let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
            let length = metadata.len();
            total = total.saturating_add(length);
            files.push((path, length, modified));
        }
        if total <= max_bytes {
            return Ok(());
        }
        files.sort_by_key(|(_, _, modified)| *modified);
        for (path, length, _) in files {
            if total <= max_bytes {
                break;
            }
            if fs::remove_file(path).is_ok() {
                total = total.saturating_sub(length);
            }
        }
        if total > max_bytes {
            return Err(io::Error::other(
                "could not trim artwork cache to its byte limit",
            ));
        }
        Ok(())
    }
}

fn encode_entry(source: SourceStamp, artwork: &Artwork) -> Option<Vec<u8>> {
    if artwork.bytes.len() > MAX_LOCAL_ARTWORK_BYTES {
        return None;
    }
    let mime = artwork
        .mime
        .as_deref()
        .unwrap_or("application/octet-stream");
    if mime.is_empty()
        || mime.len() > MAX_MIME_BYTES
        || !mime
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b';' && byte != b',')
    {
        return None;
    }

    let mut encoded = Vec::with_capacity(FIXED_HEADER_BYTES + mime.len() + artwork.bytes.len());
    encoded.extend_from_slice(MAGIC);
    encoded.extend_from_slice(&source.length.to_le_bytes());
    encoded.extend_from_slice(&source.modified_seconds.to_le_bytes());
    encoded.extend_from_slice(&source.modified_nanos.to_le_bytes());
    encoded.extend_from_slice(&(mime.len() as u32).to_le_bytes());
    encoded.extend_from_slice(&(artwork.bytes.len() as u64).to_le_bytes());
    encoded.extend_from_slice(mime.as_bytes());
    encoded.extend_from_slice(&artwork.bytes);
    Some(encoded)
}

fn read_entry(path: &Path, expected_source: SourceStamp) -> io::Result<Artwork> {
    let mut encoded = persistence::read_limited(path, MAX_CACHE_ENTRY_BYTES)?;
    if encoded.len() < FIXED_HEADER_BYTES || encoded.get(..MAGIC.len()) != Some(MAGIC.as_slice()) {
        return Err(invalid_entry());
    }

    let length = read_u64(&encoded, 8)?;
    let modified_seconds = read_u64(&encoded, 16)?;
    let modified_nanos = read_u32(&encoded, 24)?;
    let mime_length = read_u32(&encoded, 28)? as usize;
    let artwork_length = read_u64(&encoded, 32)?;
    let source = SourceStamp {
        length,
        modified_seconds,
        modified_nanos,
    };
    if source != expected_source
        || mime_length == 0
        || mime_length > MAX_MIME_BYTES
        || artwork_length > MAX_LOCAL_ARTWORK_BYTES as u64
    {
        return Err(invalid_entry());
    }
    let payload_start = FIXED_HEADER_BYTES
        .checked_add(mime_length)
        .ok_or_else(invalid_entry)?;
    let expected_length = (payload_start as u64)
        .checked_add(artwork_length)
        .ok_or_else(invalid_entry)?;
    if expected_length != encoded.len() as u64 {
        return Err(invalid_entry());
    }
    let mime = std::str::from_utf8(&encoded[FIXED_HEADER_BYTES..payload_start])
        .map_err(|_| invalid_entry())?
        .to_owned();
    if !mime
        .bytes()
        .all(|byte| byte.is_ascii_graphic() && byte != b';' && byte != b',')
    {
        return Err(invalid_entry());
    }
    encoded.drain(..payload_start);
    Ok(Artwork {
        mime: Some(mime),
        bytes: encoded,
    })
}

fn read_u32(bytes: &[u8], offset: usize) -> io::Result<u32> {
    let value = bytes.get(offset..offset + 4).ok_or_else(invalid_entry)?;
    Ok(u32::from_le_bytes(
        value.try_into().expect("four-byte slice"),
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> io::Result<u64> {
    let value = bytes.get(offset..offset + 8).ok_or_else(invalid_entry)?;
    Ok(u64::from_le_bytes(
        value.try_into().expect("eight-byte slice"),
    ))
}

fn invalid_entry() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid artwork cache entry")
}

fn touch(path: &Path) {
    let Ok(file) = File::options().write(true).open(path) else {
        return;
    };
    let _ = file.set_times(FileTimes::new().set_modified(SystemTime::now()));
}

fn is_cache_entry_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    name.len() == 32 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        fs::{File, FileTimes},
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::{Duration, UNIX_EPOCH},
    };

    use super::{encode_entry, LocalArtworkCache, SourceStamp};
    use retune_audio::Artwork;

    fn artwork(bytes: &[u8]) -> Artwork {
        Artwork {
            mime: Some("image/test".into()),
            bytes: bytes.to_vec(),
        }
    }

    fn source(dir: &std::path::Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn same_uri_reuses_raw_artwork_without_a_width_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = source(dir.path(), "track.mp3", b"audio source");
        let cache = LocalArtworkCache::new(dir.path().join("artwork-cache"));
        let reads = Arc::new(AtomicUsize::new(0));
        let reader = Arc::clone(&reads);
        let first = cache
            .get_or_load("file:///track.mp3", &path, move |_| {
                reader.fetch_add(1, Ordering::SeqCst);
                Ok(Some(artwork(b"raw image")))
            })
            .unwrap()
            .unwrap();
        let second = cache
            .get_or_load("file:///track.mp3", &path, |_| {
                panic!("a valid cache entry should skip extraction")
            })
            .unwrap()
            .unwrap();

        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert_eq!(first, second);
        let entry = fs::read(cache.entry_path("file:///track.mp3")).unwrap();
        assert!(entry.starts_with(b"RTNART01"));
        assert!(!entry
            .windows(b"data:image/test;base64".len())
            .any(|window| window == b"data:image/test;base64"));
    }

    #[test]
    fn source_modification_invalidates_its_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = source(dir.path(), "track.mp3", b"audio source");
        let cache = LocalArtworkCache::new(dir.path().join("artwork-cache"));
        let reads = Arc::new(AtomicUsize::new(0));
        for expected in [b"old".as_slice(), b"newer image".as_slice()] {
            let reader = Arc::clone(&reads);
            let artwork = cache
                .get_or_load("file:///track.mp3", &path, move |_| {
                    reader.fetch_add(1, Ordering::SeqCst);
                    Ok(Some(artwork(expected)))
                })
                .unwrap()
                .unwrap();
            if expected == b"old" {
                assert_eq!(artwork.bytes, b"old");
                fs::write(&path, b"changed source with a different length").unwrap();
            } else {
                assert_eq!(artwork.bytes, b"newer image");
            }
        }
        assert_eq!(reads.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn source_change_during_extraction_does_not_cache_stale_artwork() {
        let dir = tempfile::tempdir().unwrap();
        let path = source(dir.path(), "track.mp3", b"original source");
        let cache = LocalArtworkCache::new(dir.path().join("artwork-cache"));
        let first = cache
            .get_or_load("file:///track.mp3", &path, |path| {
                fs::write(path, b"replacement source with a different length").unwrap();
                Ok(Some(artwork(b"stale artwork")))
            })
            .unwrap()
            .unwrap();
        assert_eq!(first.bytes, b"stale artwork");

        let reads = Arc::new(AtomicUsize::new(0));
        let reader = Arc::clone(&reads);
        let second = cache
            .get_or_load("file:///track.mp3", &path, move |_| {
                reader.fetch_add(1, Ordering::SeqCst);
                Ok(Some(artwork(b"current artwork")))
            })
            .unwrap()
            .unwrap();

        assert_eq!(second.bytes, b"current artwork");
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn corrupt_cache_entry_falls_back_to_source_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = source(dir.path(), "track.mp3", b"audio source");
        let cache = LocalArtworkCache::new(dir.path().join("artwork-cache"));
        cache
            .get_or_load("file:///track.mp3", &path, |_| Ok(Some(artwork(b"first"))))
            .unwrap();
        fs::write(cache.entry_path("file:///track.mp3"), b"broken").unwrap();
        let reads = Arc::new(AtomicUsize::new(0));
        let reader = Arc::clone(&reads);
        let result = cache
            .get_or_load("file:///track.mp3", &path, move |_| {
                reader.fetch_add(1, Ordering::SeqCst);
                Ok(Some(artwork(b"recovered")))
            })
            .unwrap()
            .unwrap();

        assert_eq!(result.bytes, b"recovered");
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn unwritable_cache_falls_back_to_source_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = source(dir.path(), "track.mp3", b"audio source");
        let cache_dir = dir.path().join("artwork-cache");
        fs::write(&cache_dir, b"cache directory blocked").unwrap();
        let cache = LocalArtworkCache::new(cache_dir);
        let reads = Arc::new(AtomicUsize::new(0));

        for _ in 0..2 {
            let reader = Arc::clone(&reads);
            let result = cache
                .get_or_load("file:///track.mp3", &path, move |_| {
                    reader.fetch_add(1, Ordering::SeqCst);
                    Ok(Some(artwork(b"available despite cache error")))
                })
                .unwrap()
                .unwrap();
            assert_eq!(result.bytes, b"available despite cache error");
        }

        assert_eq!(reads.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn cache_limit_evicts_oldest_entry_and_keeps_recent_hit() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("artwork-cache");
        let first = source(dir.path(), "first.mp3", b"first source");
        let second = source(dir.path(), "second.mp3", b"second source");
        let third = source(dir.path(), "third.mp3", b"third source");
        let first_art = artwork(b"first image");
        let stamp = SourceStamp::read(&first).unwrap();
        let two_entries = (encode_entry(stamp, &first_art).unwrap().len() * 2) as u64;
        let cache = LocalArtworkCache::with_max_bytes(&cache_dir, two_entries);

        cache
            .get_or_load("file:///first.mp3", &first, |_| Ok(Some(first_art.clone())))
            .unwrap();
        cache
            .get_or_load("file:///second.mp3", &second, |_| {
                Ok(Some(artwork(b"first image")))
            })
            .unwrap();
        let first_entry = cache.entry_path("file:///first.mp3");
        let second_entry = cache.entry_path("file:///second.mp3");
        let old = UNIX_EPOCH + Duration::from_secs(1);
        File::options()
            .write(true)
            .open(&first_entry)
            .unwrap()
            .set_times(FileTimes::new().set_modified(old))
            .unwrap();
        let recent = old + Duration::from_secs(1);
        File::options()
            .write(true)
            .open(&second_entry)
            .unwrap()
            .set_times(FileTimes::new().set_modified(recent))
            .unwrap();

        cache
            .get_or_load("file:///first.mp3", &first, |_| {
                panic!("cache hit should update recency without reading source")
            })
            .unwrap();
        cache
            .get_or_load("file:///third.mp3", &third, |_| {
                Ok(Some(artwork(b"first image")))
            })
            .unwrap();

        assert!(first_entry.exists());
        assert!(!second_entry.exists());
        assert!(cache.entry_path("file:///third.mp3").exists());
        let total = fs::read_dir(&cache_dir)
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum::<u64>();
        assert!(total <= two_entries);
    }
}

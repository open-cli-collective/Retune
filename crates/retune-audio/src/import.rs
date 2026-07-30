use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
};

use crate::{AudioError, AudioInfo, FileTags, probe, read_tags};

const EXTENSIONS: &[&str] = &[
    "aac", "aif", "aiff", "flac", "m4a", "mp3", "mp4", "oga", "ogg", "opus", "wav", "webm",
];

/// A decodable local file and its read-only metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedFile {
    pub canonical_path: PathBuf,
    pub info: AudioInfo,
    pub tags: FileTags,
}

/// Finds supported files without following directory symlinks.
pub fn scan_path(path: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if scan(path, &mut found).is_err() {
        return Vec::new();
    }
    found.sort();
    found
        .into_iter()
        .filter_map(|path| fs::canonicalize(path).ok())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Validates and reads one file without modifying it.
pub fn import_file(path: impl AsRef<Path>) -> Result<ImportedFile, AudioError> {
    let path = fs::canonicalize(path)?;
    let info = probe(&path)?;
    let tags = read_tags(&path).unwrap_or_else(|error| {
        log::debug!("audio tags unavailable for {}: {error}", path.display());
        FileTags {
            duration: info.duration.unwrap_or_default(),
            ..Default::default()
        }
    });
    Ok(ImportedFile {
        info,
        tags,
        canonical_path: path,
    })
}

fn scan(path: &Path, found: &mut Vec<PathBuf>) -> io::Result<()> {
    let link = fs::symlink_metadata(path)?;
    if hidden(path) {
        return Ok(());
    }
    if link.file_type().is_symlink() {
        if fs::metadata(path)?.is_file() && supported(path) {
            found.push(path.to_owned());
        }
    } else if link.is_dir() {
        let mut entries = fs::read_dir(path)?.collect::<io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            scan(&entry.path(), found)?;
        }
    } else if link.is_file() && supported(path) {
        found.push(path.to_owned());
    }
    Ok(())
}

fn hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.'))
}

fn supported(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()))
}

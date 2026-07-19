use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use retune_core::io::{export_json, import};
use retune_core::model::Library;

#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
    Import(retune_core::io::ImportError),
    Clock(std::time::SystemTimeError),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::Import(error) => error.fmt(formatter),
            Self::Clock(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<std::io::Error> for StoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<retune_core::io::ImportError> for StoreError {
    fn from(error: retune_core::io::ImportError) -> Self {
        Self::Import(error)
    }
}

impl From<std::time::SystemTimeError> for StoreError {
    fn from(error: std::time::SystemTimeError) -> Self {
        Self::Clock(error)
    }
}

pub type StoreResult<T> = Result<T, StoreError>;

pub trait OverlayStore {
    fn load(&self) -> StoreResult<Option<Library>>;
    fn save(&self, library: &Library) -> StoreResult<()>;
}

pub struct FsOverlayStore {
    path: PathBuf,
}

impl FsOverlayStore {
    pub fn new(app_data_dir: impl AsRef<Path>) -> Self {
        Self {
            path: app_data_dir.as_ref().join("library.json"),
        }
    }

    pub fn quarantine_corrupt(&self) -> StoreResult<PathBuf> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let corrupt = self
            .path
            .with_file_name(format!("library.json.corrupt-{timestamp}"));
        fs::rename(&self.path, &corrupt)?;
        Ok(corrupt)
    }
}

impl OverlayStore for FsOverlayStore {
    fn load(&self) -> StoreResult<Option<Library>> {
        match fs::read(&self.path) {
            Ok(bytes) => Ok(Some(import(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn save(&self, library: &Library) -> StoreResult<()> {
        let parent = self.path.parent().expect("library path has a parent");
        fs::create_dir_all(parent)?;
        let temporary = self.path.with_file_name("library.json.tmp");
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&temporary)?;
            file.write_all(&export_json(library))?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::fixture;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsOverlayStore::new(dir.path());
        let library = fixture::library();

        assert!(store.load().unwrap().is_none());
        store.save(&library).unwrap();
        assert_eq!(store.load().unwrap(), Some(library));
    }

    #[test]
    fn atomic_save_leaves_no_partial_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsOverlayStore::new(dir.path());

        store.save(&fixture::library()).unwrap();

        assert!(dir.path().join("library.json").is_file());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn corrupt_file_is_renamed_aside() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsOverlayStore::new(dir.path());
        let path = dir.path().join("library.json");
        fs::write(&path, b"not json").unwrap();

        assert!(store.load().is_err());
        let corrupt = store.quarantine_corrupt().unwrap();

        assert!(!path.exists());
        assert!(corrupt.is_file());
        assert!(corrupt
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("library.json.corrupt-"));
        assert_eq!(fs::read(corrupt).unwrap(), b"not json");
    }
}

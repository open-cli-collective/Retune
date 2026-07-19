use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use retune_core::io::{export_json, import};
use retune_core::model::Library;
use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
    Import(retune_core::io::ImportError),
    Json(serde_json::Error),
    InvalidSettings(&'static str),
    Clock(std::time::SystemTimeError),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::Import(error) => error.fmt(formatter),
            Self::Json(error) => error.fmt(formatter),
            Self::InvalidSettings(error) => formatter.write_str(error),
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

impl From<serde_json::Error> for StoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
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

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub theme: Theme,
    pub zoom: f64,
    pub zebra: bool,
    pub column_order: Vec<String>,
    pub auto_add_spotify_library: bool,
    #[serde(default)]
    pub spotify_client_id: String,
    #[serde(default)]
    pub spotify_sync_completed: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    Light,
    Dark,
    System,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: Theme::System,
            zoom: 1.0,
            zebra: true,
            column_order: ["name", "time", "artist", "album", "genre", "rating"]
                .map(String::from)
                .to_vec(),
            auto_add_spotify_library: true,
            spotify_client_id: String::new(),
            spotify_sync_completed: false,
        }
    }
}

impl Settings {
    fn validate(&self) -> StoreResult<()> {
        const COLUMNS: [&str; 6] = ["name", "time", "artist", "album", "genre", "rating"];
        if !(0.7..=1.8).contains(&self.zoom) {
            return Err(StoreError::InvalidSettings(
                "settings zoom must be between 0.7 and 1.8",
            ));
        }
        if self.column_order.len() != COLUMNS.len()
            || COLUMNS.iter().any(|column| {
                self.column_order
                    .iter()
                    .filter(|item| item == column)
                    .count()
                    != 1
            })
        {
            return Err(StoreError::InvalidSettings(
                "settings columnOrder must contain each track column exactly once",
            ));
        }
        Ok(())
    }
}

pub struct FsSettingsStore {
    path: PathBuf,
}

impl FsSettingsStore {
    pub fn new(app_data_dir: impl AsRef<Path>) -> Self {
        Self {
            path: app_data_dir.as_ref().join("settings.json"),
        }
    }

    pub fn load(&self) -> StoreResult<Option<Settings>> {
        match fs::read(&self.path) {
            Ok(bytes) => {
                let settings: Settings = serde_json::from_slice(&bytes)?;
                settings.validate()?;
                Ok(Some(settings))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn save(&self, settings: &Settings) -> StoreResult<()> {
        settings.validate()?;
        atomic_write(&self.path, &serde_json::to_vec(settings)?)
    }
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
        atomic_write(&self.path, &export_json(library))
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> StoreResult<()> {
    fs::create_dir_all(path.parent().expect("store path has a parent"))?;
    let temporary = path.with_extension("json.tmp");
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(Into::into)
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
    fn settings_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSettingsStore::new(dir.path());
        let settings = Settings {
            theme: Theme::Dark,
            zoom: 1.3,
            zebra: false,
            column_order: ["rating", "name", "artist", "album", "genre", "time"]
                .map(String::from)
                .to_vec(),
            auto_add_spotify_library: true,
            spotify_client_id: "client-id".into(),
            spotify_sync_completed: true,
        };

        assert!(store.load().unwrap().is_none());
        store.save(&settings).unwrap();
        assert_eq!(store.load().unwrap(), Some(settings));
    }

    #[test]
    fn spotify_startup_sync_defaults_on() {
        assert!(Settings::default().auto_add_spotify_library);
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

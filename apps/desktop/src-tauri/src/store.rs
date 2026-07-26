use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use retune_core::io::{export_json, import};
use retune_core::model::Library;
use retune_spotify::tokens::{TokenStore, Tokens};
use serde::{Deserialize, Serialize};

use crate::playlists::PlaylistCache;

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
    #[serde(default)]
    pub browser_panes: BrowserPanes,
    pub column_order: Vec<String>,
    #[serde(default)]
    pub column_widths: BTreeMap<String, u32>,
    #[serde(default)]
    pub hidden_columns: Vec<String>,
    #[serde(default)]
    pub sort_column: Option<String>,
    #[serde(default)]
    pub sort_desc: bool,
    pub auto_add_spotify_library: bool,
    #[serde(default = "default_true")]
    pub auto_connect: bool,
    #[serde(default)]
    pub spotify_client_id: String,
    #[serde(default)]
    pub spotify_sync_completed: bool,
    #[serde(default)]
    pub last_full_sync: Option<u64>,
    #[serde(default = "default_playback_backend")]
    pub playback_backend: String,
    #[serde(default = "default_repeat")]
    pub repeat: String,
    #[serde(default = "default_volume")]
    pub volume: u8,
    #[serde(default = "default_streaming_bitrate")]
    pub streaming_bitrate: u16,
    #[serde(default)]
    pub normalize_volume: bool,
    #[serde(default = "default_true")]
    pub gapless: bool,
    #[serde(default = "default_play_threshold_percent")]
    pub play_threshold_percent: u8,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct BrowserPanes {
    pub cat: bool,
    pub art: bool,
    pub alb: bool,
}

impl Default for BrowserPanes {
    fn default() -> Self {
        Self {
            cat: true,
            art: true,
            alb: true,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_playback_backend() -> String {
    "connect".into()
}

fn default_volume() -> u8 {
    62
}

fn default_streaming_bitrate() -> u16 {
    320
}

fn default_repeat() -> String {
    "off".into()
}

fn default_play_threshold_percent() -> u8 {
    100
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
            browser_panes: BrowserPanes::default(),
            column_order: [
                "name",
                "artist",
                "album",
                "track",
                "time",
                "rating",
                "genre",
                "plays",
                "kind",
                "bitrate",
                "lastPlayed",
                "added",
            ]
            .map(String::from)
            .to_vec(),
            column_widths: BTreeMap::new(),
            hidden_columns: ["kind", "bitrate", "lastPlayed", "added"]
                .map(String::from)
                .to_vec(),
            sort_column: None,
            sort_desc: false,
            auto_add_spotify_library: true,
            auto_connect: true,
            spotify_client_id: String::new(),
            spotify_sync_completed: false,
            last_full_sync: None,
            playback_backend: default_playback_backend(),
            repeat: default_repeat(),
            volume: default_volume(),
            streaming_bitrate: default_streaming_bitrate(),
            normalize_volume: false,
            gapless: true,
            play_threshold_percent: default_play_threshold_percent(),
        }
    }
}

impl Settings {
    const RESIZABLE_COLUMNS: [&'static str; 7] = [
        "name",
        "artist",
        "album",
        "genre",
        "kind",
        "lastPlayed",
        "added",
    ];
    const COLUMNS: [&'static str; 12] = [
        "name",
        "artist",
        "album",
        "track",
        "time",
        "rating",
        "genre",
        "plays",
        "kind",
        "bitrate",
        "lastPlayed",
        "added",
    ];
    const OLD_COLUMNS: [&'static str; 7] = [
        "track", "name", "time", "artist", "album", "genre", "rating",
    ];

    pub(crate) fn normalize(&mut self) {
        if !self.column_order.iter().any(|column| column == "track") {
            self.column_order.insert(0, "track".into());
        }
        if self.column_order == Self::OLD_COLUMNS {
            self.column_order = Self::default().column_order;
            self.hidden_columns
                .extend(["kind", "bitrate", "lastPlayed", "added"].map(String::from));
        } else {
            for column in Self::COLUMNS {
                if !self.column_order.iter().any(|item| item == column) {
                    self.column_order.push(column.into());
                    if matches!(column, "kind" | "bitrate" | "lastPlayed" | "added") {
                        self.hidden_columns.push(column.into());
                    }
                }
            }
        }
        self.hidden_columns.retain(|column| column != "name");
        self.hidden_columns.sort_by_key(|column| {
            Self::COLUMNS
                .iter()
                .position(|candidate| candidate == column)
                .unwrap_or(usize::MAX)
        });
        self.hidden_columns.dedup();
        if !matches!(self.play_threshold_percent, 50 | 75 | 90 | 100) {
            self.play_threshold_percent = default_play_threshold_percent();
        }
    }

    pub(crate) fn validate(&self) -> StoreResult<()> {
        if !(0.7..=1.8).contains(&self.zoom) {
            return Err(StoreError::InvalidSettings(
                "settings zoom must be between 0.7 and 1.8",
            ));
        }
        if self.column_order.len() != Self::COLUMNS.len()
            || Self::COLUMNS.iter().any(|column| {
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
        if self.hidden_columns.iter().any(|column| {
            column == "name"
                || !Self::COLUMNS.contains(&column.as_str())
                || self
                    .hidden_columns
                    .iter()
                    .filter(|item| *item == column)
                    .count()
                    != 1
        }) {
            return Err(StoreError::InvalidSettings(
                "settings hiddenColumns must be unique track columns other than name",
            ));
        }
        if self.column_widths.iter().any(|(column, width)| {
            !Self::RESIZABLE_COLUMNS.contains(&column.as_str()) || *width < 60
        }) {
            return Err(StoreError::InvalidSettings(
                "settings columnWidths must contain resizable track columns at least 60px wide",
            ));
        }
        if self
            .sort_column
            .as_deref()
            .is_some_and(|column| !Self::COLUMNS.contains(&column))
        {
            return Err(StoreError::InvalidSettings(
                "settings sortColumn must be a track column",
            ));
        }
        if !matches!(self.playback_backend.as_str(), "connect" | "local") {
            return Err(StoreError::InvalidSettings(
                "settings playbackBackend must be connect or local",
            ));
        }
        if !matches!(self.repeat.as_str(), "off" | "all" | "one") {
            return Err(StoreError::InvalidSettings(
                "settings repeat must be off, all, or one",
            ));
        }
        if self.volume > 100 {
            return Err(StoreError::InvalidSettings(
                "settings volume must be between 0 and 100",
            ));
        }
        if !matches!(self.streaming_bitrate, 96 | 160 | 320) {
            return Err(StoreError::InvalidSettings(
                "settings streamingBitrate must be 96, 160, or 320",
            ));
        }
        Ok(())
    }
}

pub struct FsSettingsStore {
    path: PathBuf,
}

pub struct FsSyncStore {
    cooldowns_path: PathBuf,
    artist_genres_path: PathBuf,
}

pub struct FsPlaylistStore {
    path: PathBuf,
}

impl FsPlaylistStore {
    pub fn new(app_data_dir: impl AsRef<Path>) -> Self {
        Self {
            path: app_data_dir.as_ref().join("playlists.json"),
        }
    }

    pub fn load(&self) -> StoreResult<PlaylistCache> {
        read_json_or_default(&self.path)
    }

    pub fn save(&self, playlists: &PlaylistCache) -> StoreResult<()> {
        atomic_write(&self.path, &serde_json::to_vec(playlists)?)
    }
}

impl FsSyncStore {
    pub fn new(app_data_dir: impl AsRef<Path>) -> Self {
        Self {
            cooldowns_path: app_data_dir.as_ref().join("cooldowns.json"),
            artist_genres_path: app_data_dir.as_ref().join("artist-genres.json"),
        }
    }

    pub fn cooldowns(&self, now: u64) -> StoreResult<BTreeMap<String, u64>> {
        let mut cooldowns: BTreeMap<String, u64> = read_json_or_default(&self.cooldowns_path)?;
        let original_len = cooldowns.len();
        cooldowns.retain(|_, deadline| *deadline > now);
        if cooldowns.len() != original_len {
            atomic_write(&self.cooldowns_path, &serde_json::to_vec(&cooldowns)?)?;
        }
        Ok(cooldowns)
    }

    pub fn save_cooldowns(&self, cooldowns: &BTreeMap<String, u64>) -> StoreResult<()> {
        atomic_write(&self.cooldowns_path, &serde_json::to_vec(cooldowns)?)
    }

    pub fn artist_genres(&self) -> StoreResult<BTreeMap<String, Vec<String>>> {
        read_json_or_default(&self.artist_genres_path)
    }

    pub fn save_artist_genres(&self, genres: &BTreeMap<String, Vec<String>>) -> StoreResult<()> {
        atomic_write(&self.artist_genres_path, &serde_json::to_vec(genres)?)
    }
}

fn read_json_or_default<T: serde::de::DeserializeOwned + Default>(path: &Path) -> StoreResult<T> {
    match fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error.into()),
    }
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
                let mut settings: Settings = serde_json::from_slice(&bytes)?;
                settings.normalize();
                settings.validate()?;
                Ok(Some(settings))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn save(&self, settings: &Settings) -> StoreResult<()> {
        let mut settings = settings.clone();
        settings.normalize();
        settings.validate()?;
        atomic_write(&self.path, &serde_json::to_vec(&settings)?)
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

/// Debug-build token store: a 0600 JSON file beside the overlay, so dev
/// iteration never touches the Keychain (whose ACL grants reset with every
/// rebuild's ad-hoc signature). Release builds use the Keychain.
pub struct FsTokenStore {
    path: PathBuf,
}

impl FsTokenStore {
    pub fn new(app_data_dir: impl AsRef<Path>) -> Self {
        Self {
            path: app_data_dir.as_ref().join("dev-tokens.json"),
        }
    }
}

fn token_error(error: impl std::fmt::Display) -> retune_spotify::Error {
    retune_spotify::Error::TokenStore(error.to_string())
}

impl TokenStore for FsTokenStore {
    fn load(&self) -> retune_spotify::Result<Option<Tokens>> {
        match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(token_error),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(token_error(error)),
        }
    }

    fn save(&self, tokens: &Tokens) -> retune_spotify::Result<()> {
        let bytes = serde_json::to_vec(tokens).map_err(token_error)?;
        atomic_write(&self.path, &bytes).map_err(token_error)?;
        fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600)).map_err(token_error)
    }

    fn clear(&self) -> retune_spotify::Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(token_error(error)),
        }
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
    fn token_store_round_trip_clear_and_owner_only_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsTokenStore::new(dir.path());
        let tokens = Tokens {
            access: "access".into(),
            refresh: "refresh".into(),
            expires_at: 42,
            scopes: "streaming".into(),
        };

        assert!(store.load().unwrap().is_none());
        store.save(&tokens).unwrap();
        assert_eq!(store.load().unwrap(), Some(tokens));
        let mode = fs::metadata(dir.path().join("dev-tokens.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        store.clear().unwrap();
        assert!(store.load().unwrap().is_none());
        store.clear().unwrap();
    }

    #[test]
    fn settings_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSettingsStore::new(dir.path());
        let settings = Settings {
            theme: Theme::Dark,
            zoom: 1.3,
            zebra: false,
            browser_panes: BrowserPanes {
                cat: false,
                art: true,
                alb: false,
            },
            column_order: [
                "rating",
                "name",
                "artist",
                "album",
                "genre",
                "time",
                "track",
                "plays",
                "kind",
                "bitrate",
                "lastPlayed",
                "added",
            ]
            .map(String::from)
            .to_vec(),
            column_widths: BTreeMap::from([("name".into(), 240), ("lastPlayed".into(), 120)]),
            hidden_columns: vec!["genre".into(), "added".into()],
            sort_column: Some("artist".into()),
            sort_desc: true,
            auto_add_spotify_library: true,
            auto_connect: false,
            spotify_client_id: "client-id".into(),
            spotify_sync_completed: true,
            last_full_sync: Some(42),
            playback_backend: "local".into(),
            repeat: "all".into(),
            volume: 40,
            streaming_bitrate: 160,
            normalize_volume: true,
            gapless: false,
            play_threshold_percent: 75,
        };

        assert!(store.load().unwrap().is_none());
        store.save(&settings).unwrap();
        assert_eq!(store.load().unwrap(), Some(settings));
    }

    #[test]
    fn legacy_settings_default_column_widths_to_empty() {
        let mut json = serde_json::to_value(Settings::default()).unwrap();
        json.as_object_mut().unwrap().remove("columnWidths");

        let settings: Settings = serde_json::from_value(json).unwrap();

        assert!(settings.column_widths.is_empty());
    }

    #[test]
    fn legacy_settings_default_all_browser_panes_visible() {
        let mut json = serde_json::to_value(Settings::default()).unwrap();
        json.as_object_mut().unwrap().remove("browserPanes");

        let settings: Settings = serde_json::from_value(json).unwrap();

        assert_eq!(settings.browser_panes, BrowserPanes::default());
    }

    #[test]
    fn legacy_settings_default_play_threshold_to_completion() {
        let mut json = serde_json::to_value(Settings::default()).unwrap();
        json.as_object_mut().unwrap().remove("playThresholdPercent");

        let settings: Settings = serde_json::from_value(json).unwrap();

        assert_eq!(settings.play_threshold_percent, 100);
    }

    #[test]
    fn settings_load_normalizes_invalid_play_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSettingsStore::new(dir.path());
        let mut json = serde_json::to_value(Settings::default()).unwrap();
        json["playThresholdPercent"] = 42.into();
        fs::write(
            dir.path().join("settings.json"),
            serde_json::to_vec(&json).unwrap(),
        )
        .unwrap();

        assert_eq!(store.load().unwrap().unwrap().play_threshold_percent, 100);
    }

    #[test]
    fn settings_round_trip_every_allowed_play_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSettingsStore::new(dir.path());
        let mut settings = Settings::default();

        for threshold in [50, 75, 90, 100] {
            settings.play_threshold_percent = threshold;
            store.save(&settings).unwrap();
            assert_eq!(
                store.load().unwrap().unwrap().play_threshold_percent,
                threshold
            );
        }
    }

    #[test]
    fn cooldowns_persist_and_expire_across_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSyncStore::new(dir.path());
        store
            .save_cooldowns(&BTreeMap::from([
                ("/albums".into(), 200),
                ("/shows".into(), 50),
            ]))
            .unwrap();

        let reloaded = FsSyncStore::new(dir.path());
        assert_eq!(
            reloaded.cooldowns(100).unwrap(),
            BTreeMap::from([("/albums".into(), 200)])
        );
        assert_eq!(reloaded.cooldowns(201).unwrap(), BTreeMap::new());
    }

    #[test]
    fn artist_genres_persist_across_reloads() {
        let dir = tempfile::tempdir().unwrap();
        FsSyncStore::new(dir.path())
            .save_artist_genres(&BTreeMap::from([("artist-1".into(), vec!["rock".into()])]))
            .unwrap();

        assert_eq!(
            FsSyncStore::new(dir.path()).artist_genres().unwrap()["artist-1"],
            ["rock"]
        );
    }

    #[test]
    fn playlists_persist_across_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsPlaylistStore::new(dir.path());
        let playlists = PlaylistCache {
            playlists: vec![crate::playlists::CachedPlaylist {
                id: "playlist".into(),
                name: "Playlist".into(),
                snapshot_id: "snapshot".into(),
                owned: false,
                owner: Some("Owner Name".into()),
                track_count: 0,
                tracks: vec![],
                non_library_tracks: vec![],
            }],
        };

        store.save(&playlists).unwrap();
        let reloaded = FsPlaylistStore::new(dir.path()).load().unwrap();
        assert_eq!(reloaded, playlists);
        assert_eq!(reloaded.playlists[0].owner.as_deref(), Some("Owner Name"));
        assert!(dir.path().join("playlists.json").is_file());
    }

    #[test]
    fn spotify_startup_sync_defaults_on() {
        assert!(Settings::default().auto_add_spotify_library);
        assert!(Settings::default().auto_connect);
    }

    #[test]
    fn settings_load_heals_legacy_column_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSettingsStore::new(dir.path());
        let legacy = serde_json::json!({
            "theme": "system", "zoom": 1.0, "zebra": true,
            "columnOrder": ["name", "time", "artist", "album", "genre", "rating"],
            "autoAddSpotifyLibrary": true
        });
        fs::write(
            dir.path().join("settings.json"),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();

        let settings = store.load().unwrap().unwrap();
        assert_eq!(
            settings.column_order,
            [
                "name",
                "artist",
                "album",
                "track",
                "time",
                "rating",
                "genre",
                "plays",
                "kind",
                "bitrate",
                "lastPlayed",
                "added",
            ]
        );
        assert_eq!(
            settings.hidden_columns,
            ["kind", "bitrate", "lastPlayed", "added"]
        );
        assert_eq!(settings.playback_backend, "connect");
        assert_eq!(settings.repeat, "off");
        assert_eq!(settings.volume, 62);
        assert_eq!(settings.streaming_bitrate, 320);
        assert!(!settings.normalize_volume);
        assert!(settings.gapless);
    }

    #[test]
    fn settings_load_upgrades_exact_old_default_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSettingsStore::new(dir.path());
        let legacy = serde_json::json!({
            "theme": "system", "zoom": 1.0, "zebra": true,
            "columnOrder": ["track", "name", "time", "artist", "album", "genre", "rating"],
            "autoAddSpotifyLibrary": true
        });
        fs::write(
            dir.path().join("settings.json"),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();

        let settings = store.load().unwrap().unwrap();
        assert_eq!(settings.column_order, Settings::default().column_order);
        assert_eq!(settings.hidden_columns, Settings::default().hidden_columns);
    }

    #[test]
    fn settings_load_appends_new_columns_and_hides_new_optional_columns() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSettingsStore::new(dir.path());
        let legacy = serde_json::json!({
            "theme": "system", "zoom": 1.0, "zebra": true,
            "columnOrder": ["name", "track", "artist", "album", "time", "genre", "rating"],
            "hiddenColumns": ["name", "genre"],
            "autoAddSpotifyLibrary": true
        });
        fs::write(
            dir.path().join("settings.json"),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();

        let settings = store.load().unwrap().unwrap();
        assert_eq!(
            settings.column_order,
            [
                "name",
                "track",
                "artist",
                "album",
                "time",
                "genre",
                "rating",
                "plays",
                "kind",
                "bitrate",
                "lastPlayed",
                "added",
            ]
        );
        assert_eq!(
            settings.hidden_columns,
            ["genre", "kind", "bitrate", "lastPlayed", "added"]
        );
    }

    #[test]
    fn settings_load_adds_date_added_after_last_played_and_hides_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSettingsStore::new(dir.path());
        let legacy = serde_json::json!({
            "theme": "system", "zoom": 1.0, "zebra": true,
            "columnOrder": ["name", "artist", "album", "track", "time", "rating", "genre", "plays", "kind", "bitrate", "lastPlayed"],
            "hiddenColumns": ["kind", "bitrate", "lastPlayed"],
            "autoAddSpotifyLibrary": true
        });
        fs::write(
            dir.path().join("settings.json"),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();

        let settings = store.load().unwrap().unwrap();
        assert_eq!(
            settings.column_order.last().map(String::as_str),
            Some("added")
        );
        assert!(settings
            .hidden_columns
            .iter()
            .any(|column| column == "added"));
    }

    #[test]
    fn legacy_settings_json_defaults_to_no_sort() {
        let settings: Settings = serde_json::from_value(serde_json::json!({
            "theme": "system", "zoom": 1.0, "zebra": true,
            "columnOrder": ["track", "name", "time", "artist", "album", "genre", "rating"],
            "autoAddSpotifyLibrary": true
        }))
        .unwrap();

        assert_eq!(settings.sort_column, None);
        assert!(!settings.sort_desc);
    }

    #[test]
    fn missing_playback_backend_defaults_to_connect() {
        let settings: Settings = serde_json::from_value(serde_json::json!({
            "theme": "system",
            "zoom": 1.0,
            "zebra": true,
            "columnOrder": ["track", "name", "time", "artist", "album", "genre", "rating"],
            "autoAddSpotifyLibrary": true
        }))
        .unwrap();
        assert_eq!(settings.playback_backend, "connect");
        assert_eq!(settings.repeat, "off");
        assert_eq!(settings.streaming_bitrate, 320);
        assert!(!settings.normalize_volume);
        assert!(settings.gapless);
    }

    #[test]
    fn invalid_repeat_is_rejected() {
        let mut settings = Settings::default();
        assert_eq!(settings.repeat, "off");
        settings.repeat = "sometimes".into();
        assert!(settings.validate().is_err());
    }

    #[test]
    fn invalid_sort_column_is_rejected() {
        let settings = Settings {
            sort_column: Some("composer".into()),
            ..Settings::default()
        };
        assert!(settings.validate().is_err());
    }

    #[test]
    fn column_widths_only_allow_resizable_columns_at_least_60px() {
        for column in [
            "name",
            "artist",
            "album",
            "genre",
            "kind",
            "lastPlayed",
            "added",
        ] {
            let settings = Settings {
                column_widths: BTreeMap::from([(column.into(), 60)]),
                ..Settings::default()
            };
            assert!(settings.validate().is_ok(), "{column}");
        }

        let mut settings = Settings::default();
        settings.column_widths.insert("name".into(), u32::MAX);
        assert!(settings.validate().is_ok());
        settings.column_widths.insert("artist".into(), 59);
        assert!(settings.validate().is_err());

        for column in ["rating", "composer"] {
            settings.column_widths = BTreeMap::from([(column.into(), 84)]);
            assert!(settings.validate().is_err(), "{column}");
        }
    }

    #[test]
    fn hidden_columns_are_known_unique_and_keep_song_visible() {
        let mut settings = Settings {
            hidden_columns: vec!["genre".into()],
            ..Settings::default()
        };
        assert!(settings.validate().is_ok());
        settings.hidden_columns = vec!["name".into()];
        assert!(settings.validate().is_err());
        settings.hidden_columns = vec!["unknown".into()];
        assert!(settings.validate().is_err());
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

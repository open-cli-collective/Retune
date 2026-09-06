use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(test)]
use std::sync::{atomic::AtomicBool, atomic::AtomicUsize, Barrier};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use retune_core::io::{export_json, import};
use retune_core::model::Library;
use retune_spotify::catalog::SpotifyCatalog;
use retune_spotify::client::{endpoint_family, SearchSource};
use retune_spotify::tokens::{TokenStore, Tokens};
use serde::{Deserialize, Serialize};

use crate::{
    persistence::{atomic_write, read_limited, read_limited_file},
    playback::{PlaybackBackend, RepeatMode},
    playlists::PlaylistCache,
};

const MAX_SETTINGS_BYTES: u64 = 1024 * 1024;
const MAX_CREDENTIAL_BYTES: u64 = 1024 * 1024;
const MAX_COOLDOWN_BYTES: u64 = 1024 * 1024;
const MAX_ARTIST_GENRES_BYTES: u64 = 32 * 1024 * 1024;
const MAX_PLAYLIST_BYTES: u64 = 256 * 1024 * 1024;
const MAX_SPOTIFY_STATE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_LIBRARY_BYTES: u64 = 512 * 1024 * 1024;
const MAX_SETTINGS_PATCH_STRING_BYTES: usize = 4 * 1024;
pub(crate) const MAX_SETTINGS_PATCH_COLLECTION_ITEMS: usize = 4 * 1024;
pub(crate) const GLOBAL_QUOTA_KEY: &str = "__global_quota__";

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

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Import(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::Clock(error) => Some(error),
            Self::InvalidSettings(_) => None,
        }
    }
}

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

#[derive(Clone)]
pub struct FsOverlayStore {
    path: PathBuf,
    #[cfg(test)]
    save_hook: Arc<Mutex<Option<Arc<SaveHook>>>>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastFmScrobblingProfile {
    pub username: String,
    pub started_at: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub theme: Theme,
    pub zoom: f64,
    pub zebra: bool,
    #[serde(default)]
    pub pl_collapsed: bool,
    #[serde(default = "default_true")]
    pub browser_visible: bool,
    #[serde(default)]
    pub browser_panes: BrowserPanes,
    pub column_order: Vec<String>,
    #[serde(default)]
    pub column_widths: BTreeMap<String, u32>,
    #[serde(default)]
    pub hidden_columns: Vec<String>,
    #[serde(default)]
    pub playlist_hidden_columns: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub playlist_column_orders: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub playlist_column_widths: BTreeMap<String, BTreeMap<String, u32>>,
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
    #[serde(default)]
    pub next_spotify_sync: Option<u64>,
    #[serde(default)]
    pub playback_backend: PlaybackBackend,
    #[serde(default)]
    pub repeat: RepeatMode,
    #[serde(default)]
    pub shuffle: bool,
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
    #[serde(default = "default_true")]
    pub lastfm_scrobbling: bool,
    #[serde(default)]
    pub lastfm_scrobbling_profile: Option<LastFmScrobblingProfile>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsView {
    pub theme: Theme,
    pub zoom: f64,
    pub zebra: bool,
    pub pl_collapsed: bool,
    pub browser_visible: bool,
    pub browser_panes: BrowserPanes,
    pub column_order: Vec<String>,
    pub column_widths: BTreeMap<String, u32>,
    pub hidden_columns: Vec<String>,
    pub playlist_hidden_columns: BTreeMap<String, Vec<String>>,
    pub playlist_column_orders: BTreeMap<String, Vec<String>>,
    pub playlist_column_widths: BTreeMap<String, BTreeMap<String, u32>>,
    pub sort_column: Option<String>,
    pub sort_desc: bool,
    pub auto_add_spotify_library: bool,
    pub auto_connect: bool,
    pub spotify_client_id: String,
    pub playback_backend: PlaybackBackend,
    pub repeat: RepeatMode,
    pub shuffle: bool,
    pub volume: u8,
    pub streaming_bitrate: u16,
    pub normalize_volume: bool,
    pub gapless: bool,
    pub play_threshold_percent: u8,
    pub lastfm_scrobbling: bool,
}

impl From<&Settings> for SettingsView {
    fn from(settings: &Settings) -> Self {
        Self {
            theme: settings.theme,
            zoom: settings.zoom,
            zebra: settings.zebra,
            pl_collapsed: settings.pl_collapsed,
            browser_visible: settings.browser_visible,
            browser_panes: settings.browser_panes,
            column_order: settings.column_order.clone(),
            column_widths: settings.column_widths.clone(),
            hidden_columns: settings.hidden_columns.clone(),
            playlist_hidden_columns: settings.playlist_hidden_columns.clone(),
            playlist_column_orders: settings.playlist_column_orders.clone(),
            playlist_column_widths: settings.playlist_column_widths.clone(),
            sort_column: settings.sort_column.clone(),
            sort_desc: settings.sort_desc,
            auto_add_spotify_library: settings.auto_add_spotify_library,
            auto_connect: settings.auto_connect,
            spotify_client_id: settings.spotify_client_id.clone(),
            playback_backend: settings.playback_backend,
            repeat: settings.repeat,
            shuffle: settings.shuffle,
            volume: settings.volume,
            streaming_bitrate: settings.streaming_bitrate,
            normalize_volume: settings.normalize_volume,
            gapless: settings.gapless,
            play_threshold_percent: settings.play_threshold_percent,
            lastfm_scrobbling: settings.lastfm_scrobbling,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct SettingsPatch {
    pub theme: Option<Theme>,
    pub zoom: Option<f64>,
    pub zebra: Option<bool>,
    pub pl_collapsed: Option<bool>,
    pub browser_visible: Option<bool>,
    pub browser_panes: Option<BrowserPanes>,
    pub column_order: Option<Vec<String>>,
    pub column_widths: Option<BTreeMap<String, u32>>,
    pub hidden_columns: Option<Vec<String>>,
    pub playlist_hidden_columns: Option<BTreeMap<String, Vec<String>>>,
    pub playlist_column_orders: Option<BTreeMap<String, Vec<String>>>,
    pub playlist_column_widths: Option<BTreeMap<String, BTreeMap<String, u32>>>,
    #[serde(deserialize_with = "present_nullable")]
    pub sort_column: Option<Option<String>>,
    pub sort_desc: Option<bool>,
    pub auto_add_spotify_library: Option<bool>,
    pub auto_connect: Option<bool>,
    pub spotify_client_id: Option<String>,
    pub playback_backend: Option<PlaybackBackend>,
    pub streaming_bitrate: Option<u16>,
    pub normalize_volume: Option<bool>,
    pub gapless: Option<bool>,
    pub play_threshold_percent: Option<u8>,
    pub lastfm_scrobbling: Option<bool>,
}

fn present_nullable<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

impl SettingsPatch {
    pub(crate) fn validate(&self) -> Result<(), String> {
        fn string(label: &str, value: &str) -> Result<(), String> {
            (value.len() <= MAX_SETTINGS_PATCH_STRING_BYTES)
                .then_some(())
                .ok_or_else(|| format!("{label} is too long."))
        }
        fn strings(label: &str, values: &[String]) -> Result<(), String> {
            if values.len() > MAX_SETTINGS_PATCH_COLLECTION_ITEMS {
                return Err(format!("{label} has too many items."));
            }
            values.iter().try_for_each(|value| string(label, value))
        }
        fn keys<V>(label: &str, values: &BTreeMap<String, V>) -> Result<(), String> {
            if values.len() > MAX_SETTINGS_PATCH_COLLECTION_ITEMS {
                return Err(format!("{label} has too many items."));
            }
            values.keys().try_for_each(|key| string(label, key))
        }

        if let Some(value) = &self.spotify_client_id {
            string("Spotify client ID", value)?;
        }
        if let Some(Some(value)) = &self.sort_column {
            string("Sort column", value)?;
        }
        for (label, values) in [
            ("Column order", self.column_order.as_deref()),
            ("Hidden columns", self.hidden_columns.as_deref()),
        ] {
            if let Some(values) = values {
                strings(label, values)?;
            }
        }
        if let Some(values) = &self.column_widths {
            keys("Column widths", values)?;
        }
        for (label, maps) in [
            ("Playlist hidden columns", &self.playlist_hidden_columns),
            ("Playlist column orders", &self.playlist_column_orders),
        ] {
            if let Some(maps) = maps {
                keys(label, maps)?;
                maps.values()
                    .try_for_each(|values| strings(label, values))?;
            }
        }
        if let Some(maps) = &self.playlist_column_widths {
            keys("Playlist column widths", maps)?;
            for widths in maps.values() {
                keys("Playlist column widths", widths)?;
            }
        }
        Ok(())
    }

    pub fn apply(self, settings: &mut Settings) {
        macro_rules! set {
            ($($field:ident),+ $(,)?) => {
                $(if let Some(value) = self.$field { settings.$field = value; })+
            };
        }
        set!(
            theme,
            zoom,
            zebra,
            pl_collapsed,
            browser_visible,
            browser_panes,
            column_order,
            column_widths,
            hidden_columns,
            playlist_hidden_columns,
            playlist_column_orders,
            playlist_column_widths,
            sort_column,
            sort_desc,
            auto_add_spotify_library,
            auto_connect,
            spotify_client_id,
            playback_backend,
            streaming_bitrate,
            normalize_volume,
            gapless,
            play_threshold_percent,
            lastfm_scrobbling,
        );
    }
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

fn default_volume() -> u8 {
    62
}

fn default_streaming_bitrate() -> u16 {
    320
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
            pl_collapsed: false,
            browser_visible: true,
            browser_panes: BrowserPanes::default(),
            column_order: Self::COLUMNS.map(String::from).to_vec(),
            column_widths: BTreeMap::new(),
            hidden_columns: Self::OPTIONAL_COLUMNS.map(String::from).to_vec(),
            playlist_hidden_columns: BTreeMap::new(),
            playlist_column_orders: BTreeMap::new(),
            playlist_column_widths: BTreeMap::new(),
            sort_column: None,
            sort_desc: false,
            auto_add_spotify_library: true,
            auto_connect: true,
            spotify_client_id: String::new(),
            spotify_sync_completed: false,
            last_full_sync: None,
            next_spotify_sync: None,
            playback_backend: PlaybackBackend::default(),
            repeat: RepeatMode::default(),
            shuffle: false,
            volume: default_volume(),
            streaming_bitrate: default_streaming_bitrate(),
            normalize_volume: false,
            gapless: true,
            play_threshold_percent: default_play_threshold_percent(),
            lastfm_scrobbling: true,
            lastfm_scrobbling_profile: None,
        }
    }
}

impl Settings {
    const COLUMNS: [&'static str; 14] = [
        "track",
        "name",
        "artist",
        "album",
        "time",
        "plays",
        "rating",
        "genre",
        "disc",
        "kind",
        "bitrate",
        "lastPlayed",
        "added",
        "releaseDate",
    ];
    const LEGACY_DEFAULT_COLUMN_ORDER: [&'static str; 14] = [
        "name",
        "artist",
        "album",
        "disc",
        "track",
        "time",
        "rating",
        "genre",
        "plays",
        "kind",
        "bitrate",
        "lastPlayed",
        "added",
        "releaseDate",
    ];
    const PLAYLIST_COLUMNS: [&'static str; 14] = [
        "name",
        "artist",
        "album",
        "time",
        "rating",
        "plays",
        "genre",
        "disc",
        "kind",
        "bitrate",
        "lastPlayed",
        "added",
        "releaseDate",
        "track",
    ];
    const OPTIONAL_COLUMNS: [&'static str; 6] = [
        "disc",
        "kind",
        "bitrate",
        "lastPlayed",
        "added",
        "releaseDate",
    ];
    const PLAYLIST_OPTIONAL_COLUMNS: [&'static str; 7] = [
        "disc",
        "kind",
        "bitrate",
        "lastPlayed",
        "added",
        "releaseDate",
        "track",
    ];
    pub(crate) fn normalize(&mut self) {
        if self
            .column_order
            .iter()
            .map(String::as_str)
            .eq(Self::LEGACY_DEFAULT_COLUMN_ORDER)
        {
            self.column_order = Self::COLUMNS.map(String::from).to_vec();
        }
        self.column_order
            .retain(|column| Self::COLUMNS.contains(&column.as_str()));
        self.column_widths
            .retain(|column, _| Self::COLUMNS.contains(&column.as_str()));
        if self
            .sort_column
            .as_deref()
            .is_some_and(|column| !Self::COLUMNS.contains(&column))
        {
            self.sort_column = None;
        }
        if !self.column_order.iter().any(|column| column == "track") {
            self.column_order.insert(0, "track".into());
        }
        for column in Self::COLUMNS {
            if !self.column_order.iter().any(|item| item == column) {
                self.column_order.push(column.into());
                if Self::OPTIONAL_COLUMNS.contains(&column) {
                    self.hidden_columns.push(column.into());
                }
            }
        }
        Self::normalize_hidden_columns(&mut self.hidden_columns);
        for hidden_columns in self.playlist_hidden_columns.values_mut() {
            Self::normalize_playlist_hidden_columns(hidden_columns);
        }
        self.playlist_hidden_columns
            .retain(|_, hidden_columns| !Self::is_default_playlist_hidden(hidden_columns));
        for order in self.playlist_column_orders.values_mut() {
            Self::normalize_playlist_column_order(order);
        }
        self.playlist_column_orders
            .retain(|_, order| !Self::is_default_playlist_order(order));
        for widths in self.playlist_column_widths.values_mut() {
            widths.retain(|column, _| Self::PLAYLIST_COLUMNS.contains(&column.as_str()));
        }
        self.playlist_column_widths
            .retain(|_, widths| !widths.is_empty());
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
        if !Self::hidden_columns_valid(&self.hidden_columns)
            || self
                .playlist_hidden_columns
                .values()
                .any(|columns| !Self::playlist_hidden_columns_valid(columns))
        {
            return Err(StoreError::InvalidSettings(
                "settings hiddenColumns must contain unique known columns other than name",
            ));
        }
        if self.playlist_column_orders.values().any(|order| {
            order.len() != Self::PLAYLIST_COLUMNS.len()
                || Self::PLAYLIST_COLUMNS
                    .iter()
                    .any(|column| order.iter().filter(|item| item.as_str() == *column).count() != 1)
        }) {
            return Err(StoreError::InvalidSettings(
                "settings playlistColumnOrders must contain each playlist column exactly once",
            ));
        }
        if self.playlist_column_widths.values().any(|widths| {
            widths.iter().any(|(column, width)| {
                !Self::PLAYLIST_COLUMNS.contains(&column.as_str()) || *width < 28
            })
        }) {
            return Err(StoreError::InvalidSettings(
                "settings playlistColumnWidths must contain playlist columns at least 28px wide",
            ));
        }
        if self
            .column_widths
            .iter()
            .any(|(column, width)| !Self::COLUMNS.contains(&column.as_str()) || *width < 28)
        {
            return Err(StoreError::InvalidSettings(
                "settings columnWidths must contain track columns at least 28px wide",
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
        if self
            .lastfm_scrobbling_profile
            .as_ref()
            .is_some_and(|profile| profile.username.trim().is_empty() || profile.started_at == 0)
        {
            return Err(StoreError::InvalidSettings(
                "settings lastfmScrobblingProfile must have a username and positive startedAt",
            ));
        }
        Ok(())
    }

    fn normalize_hidden_columns(columns: &mut Vec<String>) {
        columns.retain(|column| column != "name" && Self::COLUMNS.contains(&column.as_str()));
        columns.sort_by_key(|column| {
            Self::COLUMNS
                .iter()
                .position(|candidate| candidate == column)
                .unwrap_or(usize::MAX)
        });
        columns.dedup();
    }

    fn hidden_columns_valid(columns: &[String]) -> bool {
        columns.iter().all(|column| {
            column != "name"
                && Self::COLUMNS.contains(&column.as_str())
                && columns.iter().filter(|item| *item == column).count() == 1
        })
    }

    fn normalize_playlist_column_order(order: &mut Vec<String>) {
        let mut normalized = Vec::with_capacity(Self::PLAYLIST_COLUMNS.len());
        for column in order.drain(..) {
            if Self::PLAYLIST_COLUMNS.contains(&column.as_str()) && !normalized.contains(&column) {
                normalized.push(column);
            }
        }
        for column in Self::PLAYLIST_COLUMNS {
            if !normalized.iter().any(|item| item == column) {
                normalized.push(column.into());
            }
        }
        *order = normalized;
    }

    fn is_default_playlist_order(order: &[String]) -> bool {
        order.iter().map(String::as_str).eq(Self::PLAYLIST_COLUMNS)
    }

    fn is_default_playlist_hidden(columns: &[String]) -> bool {
        columns
            .iter()
            .map(String::as_str)
            .eq(Self::PLAYLIST_OPTIONAL_COLUMNS)
    }

    fn normalize_playlist_hidden_columns(columns: &mut Vec<String>) {
        columns
            .retain(|column| column != "name" && Self::PLAYLIST_COLUMNS.contains(&column.as_str()));
        columns.sort_by_key(|column| {
            Self::PLAYLIST_COLUMNS
                .iter()
                .position(|candidate| candidate == column)
                .unwrap_or(usize::MAX)
        });
        columns.dedup();
    }

    fn playlist_hidden_columns_valid(columns: &[String]) -> bool {
        columns.iter().all(|column| {
            column != "name"
                && Self::PLAYLIST_COLUMNS.contains(&column.as_str())
                && columns.iter().filter(|item| *item == column).count() == 1
        })
    }
}

#[derive(Clone)]
pub struct FsSettingsStore {
    path: PathBuf,
    #[cfg(test)]
    save_hook: Arc<Mutex<Option<Arc<SaveHook>>>>,
}

#[derive(Clone)]
pub struct SettingsState {
    current: Arc<Mutex<Settings>>,
    mutation_gate: Arc<tokio::sync::Mutex<()>>,
    store: FsSettingsStore,
    restore_mutations: Arc<crate::restore_latch::RestoreMutationState>,
}

pub(crate) struct SettingsRestore<'a> {
    state: &'a SettingsState,
    _mutation_guard: tokio::sync::MutexGuard<'a, ()>,
}

pub(crate) struct SettingsSyncGuard {
    current: Arc<Mutex<Settings>>,
    _mutation_guard: tokio::sync::OwnedMutexGuard<()>,
}

pub struct FsCooldownStore {
    path: PathBuf,
    state: Mutex<CooldownState>,
}

#[derive(Default)]
struct CooldownState {
    cooldowns: Option<BTreeMap<String, Cooldown>>,
}

#[derive(Clone)]
pub struct FsArtistGenresStore {
    path: PathBuf,
    state: Arc<Mutex<ArtistGenresState>>,
    #[cfg(test)]
    save_hook: Arc<Mutex<Option<Arc<SaveHook>>>>,
    #[cfg(test)]
    save_count: Arc<AtomicUsize>,
}

#[derive(Default)]
struct ArtistGenresState {
    genres: Option<BTreeMap<String, Vec<String>>>,
    generation: u64,
    persisted_generation: u64,
}

#[derive(Clone)]
pub struct FsSpotifyLibraryStore {
    path: PathBuf,
    #[cfg(test)]
    save_hook: Arc<Mutex<Option<Arc<SaveHook>>>>,
}

#[derive(Clone)]
pub struct FsPlaylistStore {
    path: PathBuf,
    #[cfg(test)]
    save_hook: Arc<Mutex<Option<Arc<SaveHook>>>>,
}

#[cfg(test)]
pub(crate) struct SaveHook {
    reached: Barrier,
    release: Barrier,
    is_reached: AtomicBool,
    fail: AtomicBool,
}

#[cfg(test)]
impl SaveHook {
    pub(crate) fn new(fail: bool) -> Arc<Self> {
        Arc::new(Self {
            reached: Barrier::new(2),
            release: Barrier::new(2),
            is_reached: AtomicBool::new(false),
            fail: AtomicBool::new(fail),
        })
    }

    pub(crate) fn wait_until_reached(&self) {
        self.reached.wait();
    }

    pub(crate) fn release(&self) {
        self.release.wait();
    }

    pub(crate) fn is_reached(&self) -> bool {
        self.is_reached.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub(crate) fn pause(&self) -> StoreResult<()> {
        self.is_reached
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.reached.wait();
        self.release.wait();
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            Err(std::io::Error::other("injected save failure").into())
        } else {
            Ok(())
        }
    }
}

#[derive(Clone)]
pub struct FsSpotifyCatalogStore {
    path: PathBuf,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct SpotifyLibraryState {
    #[serde(default)]
    pub account_id: String,
    #[serde(default)]
    pub complete: bool,
    #[serde(default)]
    pub saved_tracks: BTreeMap<String, Option<u64>>,
    #[serde(default)]
    pub saved_albums: BTreeMap<String, SavedAlbumRecord>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SavedAlbumRecord {
    pub uri: String,
    pub name: String,
    pub artists: Vec<String>,
    pub release_date: Option<String>,
    pub album_type: Option<String>,
    pub added_at: Option<u64>,
    pub track_uris: Vec<String>,
}

impl SpotifyLibraryState {
    pub fn is_exact(&self) -> bool {
        self.complete && !self.account_id.is_empty()
    }

    pub fn add_saved_track(&mut self, uri: String, added_at: Option<u64>) {
        self.saved_tracks
            .entry(uri)
            .and_modify(|known| *known = earliest_added_at(*known, added_at))
            .or_insert(added_at);
    }

    pub fn add_saved_album(&mut self, mut album: SavedAlbumRecord) {
        if let Some(existing) = self.saved_albums.get(&album.uri) {
            album.added_at = earliest_added_at(existing.added_at, album.added_at);
        }
        self.saved_albums.insert(album.uri.clone(), album);
    }

    pub fn merge_earliest_times(self, incoming: Self) -> Self {
        if self.account_id != incoming.account_id {
            return incoming;
        }
        let mut merged = incoming;
        for (uri, added_at) in self.saved_tracks {
            if let Some(current) = merged.saved_tracks.get_mut(&uri) {
                *current = earliest_added_at(*current, added_at);
            }
        }
        for (uri, existing) in self.saved_albums {
            if let Some(current) = merged.saved_albums.get_mut(&uri) {
                current.added_at = earliest_added_at(existing.added_at, current.added_at);
            }
        }
        merged
    }
}

fn earliest_added_at(current: Option<u64>, discovered: Option<u64>) -> Option<u64> {
    match (current, discovered) {
        (Some(current), Some(discovered)) => Some(current.min(discovered)),
        (None, discovered) => discovered,
        (current, None) => current,
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CooldownKind {
    Transient,
    Quota,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Cooldown {
    pub kind: CooldownKind,
    pub deadline: u64,
}

impl FsPlaylistStore {
    pub fn new(app_data_dir: impl AsRef<Path>) -> Self {
        Self {
            path: app_data_dir.as_ref().join("playlists.json"),
            #[cfg(test)]
            save_hook: Arc::new(Mutex::new(None)),
        }
    }

    #[cfg(test)]
    pub(crate) fn arm_save(&self, hook: Arc<SaveHook>) {
        *self.save_hook.lock().unwrap() = Some(hook);
    }

    pub fn load(&self) -> StoreResult<PlaylistCache> {
        read_json_or_default(&self.path, MAX_PLAYLIST_BYTES)
    }

    pub fn save(&self, playlists: &PlaylistCache) -> StoreResult<()> {
        #[cfg(test)]
        if let Some(hook) = self.save_hook.lock().unwrap().take() {
            hook.pause()?;
        }
        atomic_write(&self.path, &serde_json::to_vec(playlists)?, None).map_err(Into::into)
    }
}

impl FsSpotifyCatalogStore {
    pub fn new(app_data_dir: impl AsRef<Path>) -> Self {
        Self {
            path: app_data_dir.as_ref().join("spotify-catalog.json"),
        }
    }

    pub fn load(&self) -> StoreResult<SpotifyCatalog> {
        let bytes = match read_limited(&self.path, MAX_SPOTIFY_STATE_BYTES) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SpotifyCatalog::default());
            }
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                self.quarantine("oversized file")?;
                log::warn!("Ignored oversized Spotify catalog; started empty");
                return Ok(SpotifyCatalog::default());
            }
            Err(error) => return Err(error.into()),
        };
        match serde_json::from_slice::<SpotifyCatalog>(&bytes) {
            Ok(catalog) if catalog.validate().is_ok() => {
                let counts = catalog.counts();
                log::info!(
                    "Loaded Spotify catalog generation={} artists={} albums={} tracks={}",
                    catalog.generation(),
                    counts.artists,
                    counts.albums,
                    counts.tracks
                );
                Ok(catalog)
            }
            Ok(catalog) => {
                let reason = catalog.validate().unwrap_err();
                self.quarantine(reason)?;
                log::warn!(
                    "Ignored invalid Spotify catalog version {} ({reason}); started empty",
                    catalog.version(),
                );
                Ok(SpotifyCatalog::default())
            }
            Err(error) => {
                self.quarantine("invalid JSON")?;
                log::warn!("Ignored corrupt Spotify catalog ({error}); started empty");
                Ok(SpotifyCatalog::default())
            }
        }
    }

    pub fn save(&self, catalog: &SpotifyCatalog) -> StoreResult<()> {
        catalog.validate().map_err(StoreError::InvalidSettings)?;
        atomic_write(&self.path, &serde_json::to_vec(catalog)?, None)?;
        let counts = catalog.counts();
        log::info!(
            "Saved Spotify catalog generation={} artists={} albums={} tracks={}",
            catalog.generation(),
            counts.artists,
            counts.albums,
            counts.tracks
        );
        Ok(())
    }

    fn quarantine(&self, reason: &str) -> StoreResult<PathBuf> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let quarantined = self
            .path
            .with_file_name(format!("spotify-catalog.json.corrupt-{timestamp}"));
        fs::rename(&self.path, &quarantined)?;
        log::warn!(
            "Quarantined Spotify catalog ({reason}) at {}",
            quarantined.display()
        );
        Ok(quarantined)
    }
}

impl FsCooldownStore {
    pub fn new(app_data_dir: impl AsRef<Path>) -> Self {
        Self {
            path: app_data_dir.as_ref().join("cooldowns.json"),
            state: Mutex::new(CooldownState::default()),
        }
    }

    pub fn cooldowns(&self, now: u64) -> StoreResult<BTreeMap<String, Cooldown>> {
        self.load_cooldowns()?;
        let mut cooldowns = self
            .state
            .lock()
            .expect("cooldown state mutex poisoned")
            .cooldowns
            .as_ref()
            .expect("cooldowns were loaded")
            .clone();
        coalesce_legacy_quota_entries(&mut cooldowns);
        cooldowns.retain(|_, cooldown| cooldown.deadline > now);
        Ok(cooldowns)
    }

    pub fn effective_cooldown(&self, now: u64) -> StoreResult<Option<Cooldown>> {
        self.cooldowns(now)
            .map(|cooldowns| effective_cooldown_in_map(&cooldowns))
    }

    pub fn cooldown_for(&self, family: &str, now: u64) -> StoreResult<Option<Cooldown>> {
        self.cooldowns(now)
            .map(|cooldowns| cooldown_for_family_in_map(&cooldowns, family))
    }

    pub fn record_cooldown(
        &self,
        endpoint: &str,
        kind: CooldownKind,
        deadline: u64,
        now: u64,
    ) -> StoreResult<()> {
        let key = match kind {
            CooldownKind::Transient => endpoint_family(endpoint),
            CooldownKind::Quota => GLOBAL_QUOTA_KEY.to_owned(),
        };
        self.update_cooldowns(now, |cooldowns| {
            if deadline > now {
                cooldowns.insert(key, Cooldown { kind, deadline });
            } else {
                cooldowns.remove(&key);
            }
        })
    }

    pub fn clear_quota(&self, now: u64) -> StoreResult<()> {
        self.update_cooldowns(now, |cooldowns| {
            cooldowns.retain(|key, cooldown| {
                key != GLOBAL_QUOTA_KEY && cooldown.kind != CooldownKind::Quota
            });
        })
    }

    pub fn clear_quota_after_search(&self, source: SearchSource, now: u64) -> StoreResult<()> {
        if source == SearchSource::Network {
            self.clear_quota(now)
        } else {
            Ok(())
        }
    }

    pub fn update_cooldowns<R>(
        &self,
        now: u64,
        update: impl FnOnce(&mut BTreeMap<String, Cooldown>) -> R,
    ) -> StoreResult<R> {
        self.load_cooldowns()?;
        let mut state = self.state.lock().expect("cooldown state mutex poisoned");
        let current = state.cooldowns.as_ref().expect("cooldowns were loaded");
        let mut next = current.clone();
        coalesce_legacy_quota_entries(&mut next);
        next.retain(|_, cooldown| cooldown.deadline > now);
        let result = update(&mut next);
        coalesce_legacy_quota_entries(&mut next);
        if &next != current {
            atomic_write(&self.path, &serde_json::to_vec(&next)?, None)?;
            state.cooldowns = Some(next);
        }
        Ok(result)
    }

    #[cfg(test)]
    pub fn save_cooldowns(&self, cooldowns: &BTreeMap<String, Cooldown>) -> StoreResult<()> {
        self.load_cooldowns()?;
        let mut state = self.state.lock().expect("cooldown state mutex poisoned");
        if state.cooldowns.as_ref() != Some(cooldowns) {
            atomic_write(&self.path, &serde_json::to_vec(cooldowns)?, None)?;
            state.cooldowns = Some(cooldowns.clone());
        }
        Ok(())
    }

    fn load_cooldowns(&self) -> StoreResult<()> {
        if self
            .state
            .lock()
            .expect("cooldown state mutex poisoned")
            .cooldowns
            .is_some()
        {
            return Ok(());
        }
        let cooldowns = read_json_or_default(&self.path, MAX_COOLDOWN_BYTES)?;
        let mut state = self.state.lock().expect("cooldown state mutex poisoned");
        state.cooldowns.get_or_insert(cooldowns);
        Ok(())
    }
}

pub(crate) fn effective_cooldown_in_map(
    cooldowns: &BTreeMap<String, Cooldown>,
) -> Option<Cooldown> {
    cooldowns.get(GLOBAL_QUOTA_KEY).copied().or_else(|| {
        cooldowns
            .values()
            .min_by_key(|cooldown| cooldown.deadline)
            .copied()
    })
}

pub(crate) fn cooldown_for_family_in_map(
    cooldowns: &BTreeMap<String, Cooldown>,
    family: &str,
) -> Option<Cooldown> {
    cooldowns
        .get(GLOBAL_QUOTA_KEY)
        .copied()
        .or_else(|| cooldowns.get(family).copied())
}

fn coalesce_legacy_quota_entries(cooldowns: &mut BTreeMap<String, Cooldown>) -> bool {
    let legacy_deadline = cooldowns
        .iter()
        .filter(|(key, cooldown)| {
            key.as_str() != GLOBAL_QUOTA_KEY && cooldown.kind == CooldownKind::Quota
        })
        .map(|(_, cooldown)| cooldown.deadline)
        .max();
    let Some(legacy_deadline) = legacy_deadline else {
        return false;
    };
    let global_deadline = cooldowns
        .get(GLOBAL_QUOTA_KEY)
        .filter(|cooldown| cooldown.kind == CooldownKind::Quota)
        .map(|cooldown| cooldown.deadline);
    let deadline = global_deadline.map_or(legacy_deadline, |current| current.max(legacy_deadline));
    let mut changed = false;
    if global_deadline != Some(deadline) {
        cooldowns.insert(
            GLOBAL_QUOTA_KEY.to_owned(),
            Cooldown {
                kind: CooldownKind::Quota,
                deadline,
            },
        );
        changed = true;
    }
    let legacy_keys = cooldowns
        .iter()
        .filter(|(key, cooldown)| {
            key.as_str() != GLOBAL_QUOTA_KEY && cooldown.kind == CooldownKind::Quota
        })
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    for key in legacy_keys {
        changed |= cooldowns.remove(&key).is_some();
    }
    changed
}

impl FsArtistGenresStore {
    pub fn new(app_data_dir: impl AsRef<Path>) -> Self {
        Self {
            path: app_data_dir.as_ref().join("artist-genres.json"),
            state: Arc::new(Mutex::new(ArtistGenresState::default())),
            #[cfg(test)]
            save_hook: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            save_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    #[cfg(test)]
    pub fn artist_genres(&self) -> StoreResult<BTreeMap<String, Vec<String>>> {
        self.load_artist_genres()?;
        Ok(self
            .state
            .lock()
            .expect("artist genres state mutex poisoned")
            .genres
            .as_ref()
            .expect("artist genres were loaded")
            .clone())
    }

    pub fn artist_genres_for(&self, artist_id: &str) -> StoreResult<Option<Vec<String>>> {
        self.load_artist_genres()?;
        Ok(self
            .state
            .lock()
            .expect("artist genres state mutex poisoned")
            .genres
            .as_ref()
            .expect("artist genres were loaded")
            .get(artist_id)
            .cloned())
    }

    #[cfg(test)]
    pub fn save_artist_genres(&self, genres: &BTreeMap<String, Vec<String>>) -> StoreResult<()> {
        self.load_artist_genres()?;
        {
            let mut state = self
                .state
                .lock()
                .expect("artist genres state mutex poisoned");
            if state.genres.as_ref() != Some(genres) {
                state.genres = Some(genres.clone());
                state.generation = state.generation.wrapping_add(1);
            }
        }
        self.flush_artist_genres()
    }

    pub fn cache_artist_genres(&self, artist_id: String, genres: Vec<String>) -> StoreResult<()> {
        self.load_artist_genres()?;
        let mut state = self
            .state
            .lock()
            .expect("artist genres state mutex poisoned");
        let cache = state.genres.as_mut().expect("artist genres were loaded");
        if cache.get(&artist_id) != Some(&genres) {
            cache.insert(artist_id, genres);
            state.generation = state.generation.wrapping_add(1);
        }
        Ok(())
    }

    pub fn flush_artist_genres(&self) -> StoreResult<()> {
        loop {
            let (generation, bytes) = {
                let state = self
                    .state
                    .lock()
                    .expect("artist genres state mutex poisoned");
                if state.persisted_generation == state.generation {
                    return Ok(());
                }
                (
                    state.generation,
                    serde_json::to_vec(state.genres.as_ref().expect("artist genres were loaded"))?,
                )
            };
            #[cfg(test)]
            {
                self.save_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if let Some(hook) = self.save_hook.lock().unwrap().take() {
                    hook.pause()?;
                }
            }
            atomic_write(&self.path, &bytes, None)?;
            let mut state = self
                .state
                .lock()
                .expect("artist genres state mutex poisoned");
            state.persisted_generation = generation;
            if state.generation == generation {
                return Ok(());
            }
        }
    }

    fn load_artist_genres(&self) -> StoreResult<()> {
        if self
            .state
            .lock()
            .expect("artist genres state mutex poisoned")
            .genres
            .is_some()
        {
            return Ok(());
        }
        let genres = read_json_or_default(&self.path, MAX_ARTIST_GENRES_BYTES)?;
        self.state
            .lock()
            .expect("artist genres state mutex poisoned")
            .genres
            .get_or_insert(genres);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn arm_save(&self, hook: Arc<SaveHook>) {
        *self.save_hook.lock().unwrap() = Some(hook);
    }

    #[cfg(test)]
    pub(crate) fn save_count(&self) -> usize {
        self.save_count.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl FsSpotifyLibraryStore {
    pub fn new(app_data_dir: impl AsRef<Path>) -> Self {
        Self {
            path: app_data_dir.as_ref().join("spotify-library.json"),
            #[cfg(test)]
            save_hook: Arc::new(Mutex::new(None)),
        }
    }

    #[cfg(test)]
    pub(crate) fn arm_save(&self, hook: Arc<SaveHook>) {
        *self.save_hook.lock().unwrap() = Some(hook);
    }

    pub fn load(&self) -> StoreResult<SpotifyLibraryState> {
        read_json_or_default(&self.path, MAX_SPOTIFY_STATE_BYTES)
    }

    pub fn save(&self, state: &SpotifyLibraryState) -> StoreResult<()> {
        #[cfg(test)]
        if let Some(hook) = self.save_hook.lock().unwrap().take() {
            hook.pause()?;
        }
        atomic_write(&self.path, &serde_json::to_vec(state)?, None).map_err(Into::into)
    }
}

fn read_json_or_default<T: serde::de::DeserializeOwned + Default>(
    path: &Path,
    limit: u64,
) -> StoreResult<T> {
    match read_limited(path, limit) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let name = path
                .file_name()
                .ok_or_else(|| std::io::Error::other("store path has no name"))?
                .to_string_lossy();
            let quarantined = path.with_file_name(format!("{name}.oversized-{stamp}"));
            fs::rename(path, &quarantined)?;
            log::warn!(
                "Quarantined oversized reconstructible state at {}",
                quarantined.display()
            );
            Ok(T::default())
        }
        Err(error) => Err(error.into()),
    }
}

impl FsSettingsStore {
    pub fn new(app_data_dir: impl AsRef<Path>) -> Self {
        Self {
            path: app_data_dir.as_ref().join("settings.json"),
            #[cfg(test)]
            save_hook: Arc::new(Mutex::new(None)),
        }
    }

    #[cfg(test)]
    pub(crate) fn arm_save(&self, hook: Arc<SaveHook>) {
        *self.save_hook.lock().unwrap() = Some(hook);
    }

    pub fn load(&self) -> StoreResult<Option<Settings>> {
        match read_limited(&self.path, MAX_SETTINGS_BYTES) {
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
        #[cfg(test)]
        if let Some(hook) = self.save_hook.lock().unwrap().take() {
            hook.pause()?;
        }
        let mut settings = settings.clone();
        settings.normalize();
        settings.validate()?;
        atomic_write(&self.path, &serde_json::to_vec(&settings)?, None).map_err(Into::into)
    }
}

impl SettingsState {
    #[cfg(test)]
    pub fn new(current: Settings, store: FsSettingsStore) -> Self {
        Self::new_with_restore_state(
            current,
            store,
            Arc::new(crate::restore_latch::RestoreMutationState::default()),
        )
    }

    pub(crate) fn new_with_restore_state(
        current: Settings,
        store: FsSettingsStore,
        restore_mutations: Arc<crate::restore_latch::RestoreMutationState>,
    ) -> Self {
        Self {
            current: Arc::new(Mutex::new(current)),
            mutation_gate: Arc::new(tokio::sync::Mutex::new(())),
            store,
            restore_mutations,
        }
    }

    pub fn snapshot(&self) -> Settings {
        self.current
            .lock()
            .expect("settings mutex poisoned")
            .clone()
    }

    pub(crate) async fn begin_restore(&self) -> Result<SettingsRestore<'_>, String> {
        let mutation_guard = self.mutation_gate.lock().await;
        self.restore_mutations.ensure_allowed()?;
        Ok(SettingsRestore {
            state: self,
            _mutation_guard: mutation_guard,
        })
    }

    pub(crate) async fn begin_sync_commit(&self) -> Result<SettingsSyncGuard, String> {
        let mutation_guard = Arc::clone(&self.mutation_gate).lock_owned().await;
        self.restore_mutations.ensure_allowed()?;
        Ok(SettingsSyncGuard {
            current: Arc::clone(&self.current),
            _mutation_guard: mutation_guard,
        })
    }

    pub(crate) async fn mutate_private<T>(
        &self,
        update: impl FnOnce(&mut Settings) -> Result<T, String>,
    ) -> Result<(T, Settings), String> {
        self.mutate(update, |_, _| Ok(())).await
    }

    pub async fn mutate<T>(
        &self,
        update: impl FnOnce(&mut Settings) -> Result<T, String>,
        committed: impl FnOnce(&Settings, &Settings) -> Result<(), String>,
    ) -> Result<(T, Settings), String> {
        let mutation_guard = Arc::clone(&self.mutation_gate).lock_owned().await;
        #[cfg(test)]
        self.restore_mutations.after_wait();
        self.restore_mutations.ensure_allowed()?;
        let previous = self.snapshot();
        let mut next = previous.clone();
        let value = update(&mut next)?;
        next.normalize();
        next.validate().map_err(|error| error.to_string())?;
        if next == previous {
            return Ok((value, previous));
        }
        let store = self.store.clone();
        let saved = next.clone();
        let current = Arc::clone(&self.current);
        let completion = tauri::async_runtime::spawn(async move {
            tauri::async_runtime::spawn_blocking(move || store.save(&saved))
                .await
                .map_err(|error| error.to_string())?
                .map_err(|error| error.to_string())?;
            *current.lock().expect("settings mutex poisoned") = next.clone();
            Ok::<_, String>((mutation_guard, next))
        });
        let (_mutation_guard, next) = completion
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        committed(&previous, &next).map_err(|error| {
            format!("Settings were saved, but a follow-up action failed: {error}")
        })?;
        Ok((value, next))
    }
}

impl SettingsSyncGuard {
    pub(crate) fn snapshot(&self) -> Settings {
        self.current
            .lock()
            .expect("settings mutex poisoned")
            .clone()
    }

    pub(crate) fn install(&self, next: Settings) {
        *self.current.lock().expect("settings mutex poisoned") = next;
    }
}

impl SettingsRestore<'_> {
    pub(crate) fn snapshot(&self) -> Settings {
        self.state.snapshot()
    }

    pub(crate) fn replace(&mut self, next: Settings) -> Result<(), String> {
        self.state
            .store
            .save(&next)
            .map_err(|error| error.to_string())?;
        *self.state.current.lock().expect("settings mutex poisoned") = next;
        Ok(())
    }

    pub(crate) fn install_recovered(&mut self, next: Settings) {
        *self.state.current.lock().expect("settings mutex poisoned") = next;
    }
}

impl FsOverlayStore {
    pub fn new(app_data_dir: impl AsRef<Path>) -> Self {
        Self {
            path: app_data_dir.as_ref().join("library.json"),
            #[cfg(test)]
            save_hook: Arc::new(Mutex::new(None)),
        }
    }

    #[cfg(test)]
    pub(crate) fn arm_save(&self, hook: Arc<SaveHook>) {
        *self.save_hook.lock().unwrap() = Some(hook);
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
        match read_limited(&self.path, MAX_LIBRARY_BYTES) {
            Ok(bytes) => Ok(Some(import(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn save(&self, library: &Library) -> StoreResult<()> {
        #[cfg(test)]
        if let Some(hook) = self.save_hook.lock().unwrap().take() {
            hook.pause()?;
        }
        atomic_write(&self.path, &export_json(library), None).map_err(Into::into)
    }
}

/// Debug-build token store: a permission-restricted JSON file beside the
/// overlay, so dev iteration never touches the native credential store.
/// Release builds use encrypted tokens with a native credential-store key.
pub struct FsTokenStore {
    path: PathBuf,
    lifecycle: Mutex<()>,
}

impl FsTokenStore {
    pub fn new(app_data_dir: impl AsRef<Path>) -> Self {
        Self {
            path: app_data_dir.as_ref().join("dev-tokens.json"),
            lifecycle: Mutex::new(()),
        }
    }

    fn load_file(&self) -> retune_spotify::Result<Option<Tokens>> {
        let file = match fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(token_error(error)),
        };
        let metadata = file.metadata().map_err(token_error)?;
        if !metadata.is_file() {
            return Err(token_error("development token path is not a regular file"));
        }
        #[cfg(unix)]
        {
            let mode = metadata.permissions().mode() & 0o777;
            if mode != 0o600 {
                file.set_permissions(fs::Permissions::from_mode(0o600))
                    .map_err(token_error)?;
                if file.metadata().map_err(token_error)?.permissions().mode() & 0o777 != 0o600 {
                    return Err(token_error(
                        "development token permissions are not owner-only",
                    ));
                }
            }
        }
        match read_limited_file(file, &self.path, MAX_CREDENTIAL_BYTES) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| retune_spotify::Error::TokenStoreCorrupt(error.to_string())),
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                Err(retune_spotify::Error::TokenStoreCorrupt(error.to_string()))
            }
            Err(error) => Err(token_error(error)),
        }
    }

    fn save_file(&self, tokens: &Tokens) -> retune_spotify::Result<()> {
        let bytes = serde_json::to_vec(tokens).map_err(token_error)?;
        atomic_write(&self.path, &bytes, Some(0o600)).map_err(token_error)
    }

    fn clear_file(&self) -> retune_spotify::Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(token_error(error)),
        }
    }
}

fn token_error(error: impl std::fmt::Display) -> retune_spotify::Error {
    retune_spotify::Error::TokenStore(error.to_string())
}

impl TokenStore for FsTokenStore {
    fn load(&self) -> retune_spotify::Result<Option<Tokens>> {
        let _guard = self.lifecycle.lock().map_err(token_error)?;
        self.load_file()
    }

    fn save(&self, tokens: &Tokens) -> retune_spotify::Result<()> {
        let _guard = self.lifecycle.lock().map_err(token_error)?;
        self.save_file(tokens)
    }

    fn clear(&self) -> retune_spotify::Result<()> {
        let _guard = self.lifecycle.lock().map_err(token_error)?;
        self.clear_file()
    }

    fn replace_if_current(
        &self,
        expected: &Tokens,
        tokens: &Tokens,
    ) -> retune_spotify::Result<bool> {
        let _guard = self.lifecycle.lock().map_err(token_error)?;
        if self.load_file()?.as_ref() != Some(expected) {
            return Ok(false);
        }
        self.save_file(tokens)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            mpsc, Arc, Barrier,
        },
        thread,
    };

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::fixture;

    #[test]
    fn delayed_user_patch_preserves_newer_sync_bookkeeping_in_memory_and_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Arc::new(SettingsState::new(
            Settings::default(),
            FsSettingsStore::new(dir.path()),
        ));
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let user_settings = Arc::clone(&settings);
        let user = thread::spawn(move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            tauri::async_runtime::block_on(user_settings.mutate(
                |current| {
                    SettingsPatch {
                        theme: Some(Theme::Dark),
                        ..SettingsPatch::default()
                    }
                    .apply(current);
                    Ok(())
                },
                |_, _| Ok(()),
            ))
            .unwrap();
        });

        started_rx.recv().unwrap();
        tauri::async_runtime::block_on(settings.mutate(
            |current| {
                current.spotify_sync_completed = true;
                current.last_full_sync = Some(42);
                Ok(())
            },
            |_, _| Ok(()),
        ))
        .unwrap();
        release_tx.send(()).unwrap();
        user.join().unwrap();

        let memory = settings.snapshot();
        let disk = FsSettingsStore::new(dir.path()).load().unwrap().unwrap();
        assert_eq!(memory, disk);
        assert_eq!(memory.theme, Theme::Dark);
        assert!(memory.spotify_sync_completed);
        assert_eq!(memory.last_full_sync, Some(42));
    }

    #[test]
    fn concurrent_disjoint_settings_mutations_both_survive() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Arc::new(SettingsState::new(
            Settings::default(),
            FsSettingsStore::new(dir.path()),
        ));
        let barrier = Arc::new(Barrier::new(3));
        let workers = [
            {
                let settings = Arc::clone(&settings);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    tauri::async_runtime::block_on(settings.mutate(
                        |current| {
                            current.zebra = false;
                            Ok(())
                        },
                        |_, _| Ok(()),
                    ))
                    .unwrap();
                })
            },
            {
                let settings = Arc::clone(&settings);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    tauri::async_runtime::block_on(settings.mutate(
                        |current| {
                            current.auto_connect = false;
                            Ok(())
                        },
                        |_, _| Ok(()),
                    ))
                    .unwrap();
                })
            },
        ];
        barrier.wait();
        for worker in workers {
            worker.join().unwrap();
        }

        let memory = settings.snapshot();
        let disk = FsSettingsStore::new(dir.path()).load().unwrap().unwrap();
        assert_eq!(memory, disk);
        assert!(!memory.zebra);
        assert!(!memory.auto_connect);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn paused_settings_save_serializes_latest_read_save_and_publish() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSettingsStore::new(dir.path());
        let control = store.clone();
        let settings = Arc::new(SettingsState::new(Settings::default(), store));
        let hook = SaveHook::new(false);
        control.arm_save(Arc::clone(&hook));

        let first = {
            let settings = Arc::clone(&settings);
            tokio::spawn(async move {
                settings
                    .mutate(
                        |current| {
                            current.volume = 37;
                            current.normalize_volume = true;
                            Ok(())
                        },
                        |_, _| Ok(()),
                    )
                    .await
            })
        };
        hook.wait_until_reached();
        let second = {
            let settings = Arc::clone(&settings);
            tokio::spawn(async move {
                settings
                    .mutate(
                        |current| {
                            current.shuffle = true;
                            Ok(())
                        },
                        |_, _| Ok(()),
                    )
                    .await
            })
        };

        assert_eq!(settings.snapshot(), Settings::default());
        assert!(FsSettingsStore::new(dir.path()).load().unwrap().is_none());
        hook.release();
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();

        let memory = settings.snapshot();
        let disk = FsSettingsStore::new(dir.path()).load().unwrap().unwrap();
        assert_eq!(memory, disk);
        assert_eq!(memory.volume, 37);
        assert!(memory.normalize_volume);
        assert!(memory.shuffle);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn aborted_settings_mutation_finishes_disk_and_memory_commit() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSettingsStore::new(dir.path());
        let control = store.clone();
        let settings = Arc::new(SettingsState::new(Settings::default(), store));
        let hook = SaveHook::new(false);
        control.arm_save(Arc::clone(&hook));

        let mutation = {
            let settings = Arc::clone(&settings);
            tokio::spawn(async move {
                settings
                    .mutate(
                        |current| {
                            current.volume = 37;
                            Ok(())
                        },
                        |_, _| Ok(()),
                    )
                    .await
            })
        };
        hook.wait_until_reached();
        mutation.abort();
        hook.release();
        assert!(mutation.await.unwrap_err().is_cancelled());

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if settings.snapshot().volume == 37 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(control.load().unwrap().unwrap(), settings.snapshot());
    }

    #[tokio::test]
    async fn post_commit_failure_reports_that_settings_remain_saved() {
        let dir = tempfile::tempdir().unwrap();
        let settings = SettingsState::new(Settings::default(), FsSettingsStore::new(dir.path()));

        let error = settings
            .mutate(
                |current| {
                    current.volume = 23;
                    Ok(())
                },
                |_, _| Err("event unavailable".into()),
            )
            .await
            .unwrap_err();

        assert!(error.contains("Settings were saved"));
        assert_eq!(settings.snapshot().volume, 23);
        assert_eq!(
            FsSettingsStore::new(dir.path())
                .load()
                .unwrap()
                .unwrap()
                .volume,
            23
        );
    }

    #[tokio::test]
    async fn failed_settings_save_changes_neither_memory_nor_disk() {
        let dir = tempfile::tempdir().unwrap();
        let before = Settings::default();
        let store = FsSettingsStore::new(dir.path());
        store.save(&before).unwrap();
        let control = store.clone();
        let settings = SettingsState::new(before.clone(), store);
        let hook = SaveHook::new(true);
        control.arm_save(Arc::clone(&hook));
        let release = std::thread::spawn(move || {
            hook.wait_until_reached();
            hook.release();
        });

        assert!(settings
            .mutate(
                |current| {
                    current.volume = 12;
                    Ok(())
                },
                |_, _| Ok(()),
            )
            .await
            .is_err());
        release.join().unwrap();

        assert_eq!(settings.snapshot(), before);
        assert_eq!(
            FsSettingsStore::new(dir.path()).load().unwrap(),
            Some(before)
        );
    }

    #[test]
    fn queued_settings_mutation_checks_restore_latch_after_mutex_wait() {
        let dir = tempfile::tempdir().unwrap();
        let restore_mutations = Arc::new(crate::restore_latch::RestoreMutationState::default());
        let settings = Arc::new(SettingsState::new_with_restore_state(
            Settings::default(),
            FsSettingsStore::new(dir.path()),
            Arc::clone(&restore_mutations),
        ));
        let restore = tauri::async_runtime::block_on(settings.begin_restore()).unwrap();
        let hook = restore_mutations.arm_after_wait();
        let mutated = Arc::new(AtomicBool::new(false));
        let worker = {
            let settings = Arc::clone(&settings);
            let mutated = Arc::clone(&mutated);
            thread::spawn(move || {
                tauri::async_runtime::block_on(settings.mutate(
                    |current| {
                        mutated.store(true, Ordering::SeqCst);
                        current.zebra = false;
                        Ok(())
                    },
                    |_, _| Ok(()),
                ))
                .map(|_| ())
            })
        };

        drop(restore);
        hook.wait_until_reached();
        restore_mutations.mark_recovery_required();
        hook.release();

        assert!(worker.join().unwrap().is_err());
        assert!(!mutated.load(Ordering::SeqCst));
        assert!(FsSettingsStore::new(dir.path()).load().unwrap().is_none());
    }

    #[test]
    fn one_changed_settings_mutation_runs_one_commit_callback() {
        let dir = tempfile::tempdir().unwrap();
        let settings = SettingsState::new(Settings::default(), FsSettingsStore::new(dir.path()));
        let committed = AtomicUsize::new(0);

        tauri::async_runtime::block_on(settings.mutate(
            |current| {
                current.zoom = 1.2;
                Ok(())
            },
            |_, _| {
                committed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        ))
        .unwrap();
        tauri::async_runtime::block_on(settings.mutate(
            |current| {
                current.zoom = 1.2;
                Ok(())
            },
            |_, _| {
                committed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        ))
        .unwrap();

        assert_eq!(committed.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn settings_ipc_types_exclude_private_bookkeeping() {
        assert!(serde_json::from_value::<SettingsPatch>(serde_json::json!({
            "spotifySyncCompleted": true
        }))
        .is_err());
        assert!(serde_json::from_value::<SettingsPatch>(serde_json::json!({
            "lastFullSync": 42
        }))
        .is_err());

        let value = serde_json::to_value(SettingsView::from(&Settings::default())).unwrap();
        assert!(value.get("spotifySyncCompleted").is_none());
        assert!(value.get("lastFullSync").is_none());
        assert!(value.get("lastfmScrobblingProfile").is_none());

        let patch: SettingsPatch = serde_json::from_value(serde_json::json!({
            "sortColumn": null
        }))
        .unwrap();
        assert_eq!(patch.sort_column, Some(None));
    }

    #[test]
    fn settings_patch_matches_the_shared_frontend_fixture() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../test/fixtures/ipc-contracts.json")).unwrap();
        let patch: SettingsPatch =
            serde_json::from_value(fixture["settingsPatch"].clone()).unwrap();

        assert_eq!(patch.theme, Some(Theme::Dark));
        assert_eq!(patch.sort_column, Some(None));
        assert_eq!(patch.spotify_client_id.as_deref(), Some("fixture-client"));
        assert_eq!(patch.playback_backend, Some(PlaybackBackend::Local));
        assert_eq!(patch.streaming_bitrate, Some(320));
        assert_eq!(patch.play_threshold_percent, Some(90));
        assert_eq!(patch.lastfm_scrobbling, Some(false));
    }

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
            playback_credentials: None,
        };

        assert!(store.load().unwrap().is_none());
        store.save(&tokens).unwrap();
        assert_eq!(store.load().unwrap(), Some(tokens));
        #[cfg(unix)]
        {
            let mode = fs::metadata(dir.path().join("dev-tokens.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        store.clear().unwrap();
        assert!(store.load().unwrap().is_none());
        store.clear().unwrap();

        let path = dir.path().join("dev-tokens.json");
        fs::write(&path, b"not json").unwrap();
        assert!(matches!(
            store.load(),
            Err(retune_spotify::Error::TokenStoreCorrupt(_))
        ));
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(matches!(
            store.load(),
            Err(retune_spotify::Error::TokenStore(_))
        ));

        fs::remove_dir(&path).unwrap();
        fs::File::create(&path)
            .unwrap()
            .set_len(MAX_CREDENTIAL_BYTES + 1)
            .unwrap();
        assert!(matches!(
            store.load(),
            Err(retune_spotify::Error::TokenStoreCorrupt(_))
        ));
        assert!(path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn token_store_repairs_legacy_permissions_before_loading() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dev-tokens.json");
        let tokens = Tokens {
            access: "access".into(),
            refresh: "refresh".into(),
            expires_at: 42,
            scopes: "streaming".into(),
            playback_credentials: None,
        };
        fs::write(&path, serde_json::to_vec(&tokens).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        assert_eq!(FsTokenStore::new(dir.path()).load().unwrap(), Some(tokens));
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn settings_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSettingsStore::new(dir.path());
        let settings = Settings {
            theme: Theme::Dark,
            zoom: 1.3,
            zebra: false,
            pl_collapsed: true,
            browser_visible: false,
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
                "disc",
                "genre",
                "time",
                "track",
                "plays",
                "kind",
                "bitrate",
                "lastPlayed",
                "added",
                "releaseDate",
            ]
            .map(String::from)
            .to_vec(),
            column_widths: BTreeMap::from([("name".into(), 240), ("lastPlayed".into(), 120)]),
            hidden_columns: vec![
                "genre".into(),
                "disc".into(),
                "added".into(),
                "releaseDate".into(),
            ],
            playlist_hidden_columns: BTreeMap::from([
                ("road-trip".into(), vec!["plays".into(), "genre".into()]),
                ("focus".into(), vec!["disc".into(), "track".into()]),
            ]),
            playlist_column_orders: BTreeMap::from([
                (
                    "road-trip".into(),
                    [
                        "genre",
                        "name",
                        "artist",
                        "album",
                        "time",
                        "rating",
                        "plays",
                        "disc",
                        "kind",
                        "bitrate",
                        "lastPlayed",
                        "added",
                        "releaseDate",
                        "track",
                    ]
                    .map(String::from)
                    .to_vec(),
                ),
                (
                    "focus".into(),
                    [
                        "plays",
                        "name",
                        "artist",
                        "album",
                        "time",
                        "rating",
                        "genre",
                        "disc",
                        "kind",
                        "bitrate",
                        "lastPlayed",
                        "added",
                        "releaseDate",
                        "track",
                    ]
                    .map(String::from)
                    .to_vec(),
                ),
            ]),
            playlist_column_widths: BTreeMap::from([
                (
                    "road-trip".into(),
                    BTreeMap::from([("name".into(), 220), ("genre".into(), 120)]),
                ),
                (
                    "focus".into(),
                    BTreeMap::from([("plays".into(), 180), ("genre".into(), 140)]),
                ),
            ]),
            sort_column: Some("artist".into()),
            sort_desc: true,
            auto_add_spotify_library: true,
            auto_connect: false,
            spotify_client_id: "client-id".into(),
            spotify_sync_completed: true,
            last_full_sync: Some(42),
            next_spotify_sync: Some(86_442),
            playback_backend: PlaybackBackend::Local,
            repeat: RepeatMode::All,
            shuffle: true,
            volume: 40,
            streaming_bitrate: 160,
            normalize_volume: true,
            gapless: false,
            play_threshold_percent: 75,
            lastfm_scrobbling: false,
            lastfm_scrobbling_profile: Some(LastFmScrobblingProfile {
                username: "rianjs".into(),
                started_at: 42,
            }),
        };

        assert!(store.load().unwrap().is_none());
        store.save(&settings).unwrap();
        assert_eq!(store.load().unwrap(), Some(settings));
    }

    #[test]
    fn settings_load_preserves_96_kbps_streaming_quality() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSettingsStore::new(dir.path());
        let mut json = serde_json::to_value(Settings::default()).unwrap();
        json["streamingBitrate"] = 96.into();
        fs::write(
            dir.path().join("settings.json"),
            serde_json::to_vec(&json).unwrap(),
        )
        .unwrap();

        assert_eq!(store.load().unwrap().unwrap().streaming_bitrate, 96);
    }

    #[test]
    fn fresh_settings_default_to_local_playback() {
        assert_eq!(Settings::default().playback_backend, PlaybackBackend::Local);
    }

    #[test]
    fn legacy_settings_default_shuffle_off() {
        let mut json = serde_json::to_value(Settings::default()).unwrap();
        json.as_object_mut().unwrap().remove("shuffle");

        let settings: Settings = serde_json::from_value(json).unwrap();

        assert!(!settings.shuffle);
    }

    #[test]
    fn legacy_settings_default_playlists_expanded() {
        let mut json = serde_json::to_value(Settings::default()).unwrap();
        json.as_object_mut().unwrap().remove("plCollapsed");

        let settings: Settings = serde_json::from_value(json).unwrap();

        assert!(!settings.pl_collapsed);
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
    fn legacy_settings_enable_lastfm_scrobbling_by_default() {
        let mut json = serde_json::to_value(Settings::default()).unwrap();
        json.as_object_mut().unwrap().remove("lastfmScrobbling");

        let settings: Settings = serde_json::from_value(json).unwrap();

        assert!(settings.lastfm_scrobbling);
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
    fn cooldown_reads_project_expiration_without_persisting() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("cooldowns.json"),
            br#"{"/albums":{"kind":"quota","deadline":200},"/shows":{"kind":"transient","deadline":50}}"#,
        )
        .unwrap();

        let reloaded = FsCooldownStore::new(dir.path());
        assert_eq!(
            reloaded.cooldowns(50).unwrap(),
            BTreeMap::from([(
                GLOBAL_QUOTA_KEY.into(),
                Cooldown {
                    kind: CooldownKind::Quota,
                    deadline: 200,
                },
            )])
        );
        assert_eq!(
            fs::read(dir.path().join("cooldowns.json")).unwrap(),
            br#"{"/albums":{"kind":"quota","deadline":200},"/shows":{"kind":"transient","deadline":50}}"#
        );
        assert_eq!(reloaded.cooldowns(201).unwrap(), BTreeMap::new());
        assert_eq!(
            fs::read(dir.path().join("cooldowns.json")).unwrap(),
            br#"{"/albums":{"kind":"quota","deadline":200},"/shows":{"kind":"transient","deadline":50}}"#
        );
        assert!(dir.path().join("cooldowns.json").is_file());
        assert!(!dir.path().join("artist-genres.json").exists());
    }

    #[test]
    fn failed_cooldown_save_leaves_live_state_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsCooldownStore::new(dir.path());
        assert!(store.cooldowns(0).unwrap().is_empty());
        fs::create_dir(dir.path().join("cooldowns.json")).unwrap();

        assert!(store
            .update_cooldowns(0, |cooldowns| {
                cooldowns.insert(
                    "/albums".into(),
                    Cooldown {
                        kind: CooldownKind::Quota,
                        deadline: 200,
                    },
                );
            })
            .is_err());

        fs::remove_dir(dir.path().join("cooldowns.json")).unwrap();
        assert!(store.cooldowns(0).unwrap().is_empty());
    }

    #[test]
    fn legacy_quota_entries_coalesce_to_the_latest_global_deadline() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("cooldowns.json"),
            br#"{"/albums":{"kind":"quota","deadline":200},"/tracks":{"kind":"quota","deadline":300},"/me/tracks":{"kind":"transient","deadline":400}}"#,
        )
        .unwrap();

        let store = FsCooldownStore::new(dir.path());
        assert_eq!(
            store.cooldowns(100).unwrap(),
            BTreeMap::from([
                (
                    GLOBAL_QUOTA_KEY.into(),
                    Cooldown {
                        kind: CooldownKind::Quota,
                        deadline: 300,
                    },
                ),
                (
                    "/me/tracks".into(),
                    Cooldown {
                        kind: CooldownKind::Transient,
                        deadline: 400,
                    },
                ),
            ])
        );
        assert_eq!(
            fs::read(dir.path().join("cooldowns.json")).unwrap(),
            br#"{"/albums":{"kind":"quota","deadline":200},"/tracks":{"kind":"quota","deadline":300},"/me/tracks":{"kind":"transient","deadline":400}}"#
        );
    }

    #[test]
    fn cooldown_reads_cache_without_persisting_pruning_or_migration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cooldowns.json");
        let original = br#"{"/albums":{"kind":"quota","deadline":200},"/tracks":{"kind":"quota","deadline":300}}"#;
        fs::write(&path, original).unwrap();
        let store = FsCooldownStore::new(dir.path());

        assert_eq!(
            store.effective_cooldown(100).unwrap(),
            Some(Cooldown {
                kind: CooldownKind::Quota,
                deadline: 300,
            })
        );
        assert_eq!(fs::read(&path).unwrap(), original);
        fs::remove_file(&path).unwrap();
        assert_eq!(
            store.effective_cooldown(100).unwrap(),
            Some(Cooldown {
                kind: CooldownKind::Quota,
                deadline: 300,
            })
        );
        assert_eq!(store.effective_cooldown(301).unwrap(), None);
        assert!(!path.exists());
    }

    #[test]
    fn legacy_quota_can_be_replaced_and_cleared_by_mutations() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("cooldowns.json"),
            br#"{"/albums":{"kind":"quota","deadline":500}}"#,
        )
        .unwrap();
        let store = FsCooldownStore::new(dir.path());

        store
            .record_cooldown("/tracks", CooldownKind::Quota, 250, 200)
            .unwrap();
        assert_eq!(
            store.effective_cooldown(200).unwrap(),
            Some(Cooldown {
                kind: CooldownKind::Quota,
                deadline: 250,
            })
        );
        store.clear_quota(200).unwrap();
        assert_eq!(store.effective_cooldown(200).unwrap(), None);
    }

    #[test]
    fn cooldown_policy_uses_global_quota_and_replaces_or_clears_deadlines() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsCooldownStore::new(dir.path());

        store
            .record_cooldown("/me/tracks", CooldownKind::Transient, 200, 100)
            .unwrap();
        store
            .record_cooldown("/albums", CooldownKind::Quota, 300, 100)
            .unwrap();
        assert_eq!(
            store.cooldown_for("/artists", 100).unwrap(),
            Some(Cooldown {
                kind: CooldownKind::Quota,
                deadline: 300,
            })
        );
        assert_eq!(
            store.cooldown_for("/me/tracks", 100).unwrap(),
            Some(Cooldown {
                kind: CooldownKind::Quota,
                deadline: 300,
            })
        );
        assert_eq!(
            store.effective_cooldown(100).unwrap(),
            Some(Cooldown {
                kind: CooldownKind::Quota,
                deadline: 300,
            })
        );

        store
            .record_cooldown("/me/tracks", CooldownKind::Transient, 250, 100)
            .unwrap();
        assert_eq!(
            store.cooldown_for("/me/tracks", 100).unwrap(),
            Some(Cooldown {
                kind: CooldownKind::Quota,
                deadline: 300,
            })
        );

        store
            .record_cooldown("/me/tracks", CooldownKind::Transient, 100, 100)
            .unwrap();
        assert_eq!(store.cooldowns(100).unwrap().get("/me/tracks"), None);

        store.clear_quota(100).unwrap();
        assert!(store.cooldowns(100).unwrap().is_empty());
    }

    #[test]
    fn concurrent_cooldown_updates_and_cleanup_preserve_live_deadlines() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FsCooldownStore::new(dir.path()));
        store
            .save_cooldowns(&BTreeMap::from([(
                "/expired".into(),
                Cooldown {
                    kind: CooldownKind::Transient,
                    deadline: 50,
                },
            )]))
            .unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let mut threads = Vec::new();
        for family in ["/albums", "/tracks"] {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                store
                    .update_cooldowns(100, |cooldowns| {
                        cooldowns.insert(
                            family.into(),
                            Cooldown {
                                kind: CooldownKind::Quota,
                                deadline: 200,
                            },
                        );
                    })
                    .unwrap();
            }));
        }
        barrier.wait();
        store.cooldowns(100).unwrap();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(
            FsCooldownStore::new(dir.path()).cooldowns(100).unwrap(),
            BTreeMap::from([(
                GLOBAL_QUOTA_KEY.into(),
                Cooldown {
                    kind: CooldownKind::Quota,
                    deadline: 200,
                },
            )])
        );
    }

    #[test]
    fn artist_genres_persist_across_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let raw = br#"{"artist-1":["rock"]}"#;
        fs::write(dir.path().join("artist-genres.json"), raw).unwrap();
        let store = FsArtistGenresStore::new(dir.path());

        assert_eq!(store.artist_genres().unwrap()["artist-1"], ["rock"]);
        store
            .save_artist_genres(&BTreeMap::from([("artist-1".into(), vec!["rock".into()])]))
            .unwrap();
        assert_eq!(
            fs::read(dir.path().join("artist-genres.json")).unwrap(),
            raw
        );
        assert!(dir.path().join("artist-genres.json").is_file());
        assert!(!dir.path().join("cooldowns.json").exists());
    }

    #[test]
    fn failed_artist_genre_flush_remains_dirty_for_retry() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsArtistGenresStore::new(dir.path());
        store
            .cache_artist_genres("artist-1".into(), vec!["rock".into()])
            .unwrap();
        let path = dir.path().join("artist-genres.json");
        fs::create_dir(&path).unwrap();

        assert!(store.flush_artist_genres().is_err());
        fs::remove_dir(&path).unwrap();
        store.flush_artist_genres().unwrap();

        assert_eq!(store.save_count(), 2);
        assert_eq!(
            FsArtistGenresStore::new(dir.path())
                .artist_genres()
                .unwrap()["artist-1"],
            ["rock"]
        );
    }

    #[test]
    fn spotify_catalog_round_trip_is_atomic_and_separate() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSpotifyCatalogStore::new(dir.path());
        let mut catalog = SpotifyCatalog::default();
        catalog.observe_artist(&retune_spotify::client::Artist {
            id: "artist-1".into(),
            name: "Artist".into(),
            genres: vec!["rock".into()],
            followers: None,
            images: vec![],
        });

        store.save(&catalog).unwrap();

        assert_eq!(store.load().unwrap(), catalog);
        assert!(dir.path().join("spotify-catalog.json").is_file());
        assert!(!dir.path().join("library.json").exists());
        assert!(!dir.path().join("spotify-catalog.json.tmp").exists());
    }

    #[test]
    fn corrupt_spotify_catalog_is_quarantined() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("spotify-catalog.json"), b"not-json").unwrap();

        let catalog = FsSpotifyCatalogStore::new(dir.path()).load().unwrap();

        assert_eq!(catalog, SpotifyCatalog::default());
        assert!(!dir.path().join("spotify-catalog.json").exists());
        assert!(fs::read_dir(dir.path()).unwrap().flatten().any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains("spotify-catalog.json.corrupt-")
        }));
    }

    #[test]
    fn unknown_spotify_catalog_version_is_quarantined() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("spotify-catalog.json"),
            serde_json::to_vec(&serde_json::json!({
                "version": 99,
                "artists": {},
                "albums": {},
                "tracks": {}
            }))
            .unwrap(),
        )
        .unwrap();

        assert_eq!(
            FsSpotifyCatalogStore::new(dir.path()).load().unwrap(),
            SpotifyCatalog::default()
        );
        assert!(fs::read_dir(dir.path()).unwrap().flatten().any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains("spotify-catalog.json.corrupt-")
        }));
    }

    #[test]
    fn semantically_invalid_spotify_catalog_is_quarantined() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("spotify-catalog.json"),
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "artists": {"wrong-key": {
                    "id": "artist-1", "name": "Artist", "complete": false
                }},
                "albums": {},
                "tracks": {}
            }))
            .unwrap(),
        )
        .unwrap();

        assert_eq!(
            FsSpotifyCatalogStore::new(dir.path()).load().unwrap(),
            SpotifyCatalog::default()
        );
        assert!(fs::read_dir(dir.path()).unwrap().flatten().any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains("spotify-catalog.json.corrupt-")
        }));
    }

    #[test]
    fn spotify_library_state_is_separate_and_survives_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let state = SpotifyLibraryState {
            account_id: "account".into(),
            complete: true,
            saved_tracks: BTreeMap::from([("spotify:track:one".into(), Some(10))]),
            saved_albums: BTreeMap::from([(
                "spotify:album:one".into(),
                SavedAlbumRecord {
                    uri: "spotify:album:one".into(),
                    name: "Album".into(),
                    artists: vec!["Artist".into()],
                    release_date: Some("2024-01-02".into()),
                    album_type: Some("album".into()),
                    added_at: Some(11),
                    track_uris: vec!["spotify:track:one".into()],
                },
            )]),
        };
        let store = FsSpotifyLibraryStore::new(dir.path());

        store.save(&state).unwrap();

        assert_eq!(
            FsSpotifyLibraryStore::new(dir.path()).load().unwrap(),
            state
        );
        assert!(dir.path().join("spotify-library.json").is_file());
        assert!(!dir.path().join("library.json").exists());
    }

    #[test]
    fn spotify_library_state_merges_earliest_known_membership_times() {
        let mut current = SpotifyLibraryState {
            account_id: "account".into(),
            complete: true,
            saved_tracks: BTreeMap::from([("spotify:track:one".into(), Some(200))]),
            saved_albums: BTreeMap::new(),
        };
        current.add_saved_album(SavedAlbumRecord {
            uri: "spotify:album:one".into(),
            name: "Album".into(),
            artists: vec![],
            release_date: None,
            album_type: None,
            added_at: Some(300),
            track_uris: vec![],
        });
        let incoming = SpotifyLibraryState {
            account_id: "account".into(),
            complete: true,
            saved_tracks: BTreeMap::from([("spotify:track:one".into(), Some(100))]),
            saved_albums: BTreeMap::from([(
                "spotify:album:one".into(),
                SavedAlbumRecord {
                    uri: "spotify:album:one".into(),
                    name: "Album".into(),
                    artists: vec![],
                    release_date: None,
                    album_type: None,
                    added_at: Some(400),
                    track_uris: vec![],
                },
            )]),
        };

        let merged = current.merge_earliest_times(incoming);

        assert_eq!(merged.saved_tracks["spotify:track:one"], Some(100));
        assert_eq!(merged.saved_albums["spotify:album:one"].added_at, Some(300));
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
                track_metadata_version: crate::playlists::TRACK_METADATA_VERSION,
                spotify_tracks: vec![],
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
                "track",
                "name",
                "time",
                "artist",
                "album",
                "genre",
                "rating",
                "plays",
                "disc",
                "kind",
                "bitrate",
                "lastPlayed",
                "added",
                "releaseDate",
            ]
        );
        assert_eq!(
            settings.hidden_columns,
            [
                "disc",
                "kind",
                "bitrate",
                "lastPlayed",
                "added",
                "releaseDate"
            ]
        );
        assert_eq!(settings.playback_backend, PlaybackBackend::Local);
        assert_eq!(settings.repeat, RepeatMode::Off);
        assert_eq!(settings.volume, 62);
        assert_eq!(settings.streaming_bitrate, 320);
        assert!(!settings.normalize_volume);
        assert!(settings.gapless);
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
                "disc",
                "kind",
                "bitrate",
                "lastPlayed",
                "added",
                "releaseDate",
            ]
        );
        assert_eq!(
            settings.hidden_columns,
            [
                "genre",
                "disc",
                "kind",
                "bitrate",
                "lastPlayed",
                "added",
                "releaseDate"
            ]
        );
    }

    #[test]
    fn settings_load_discards_columns_from_newer_builds() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSettingsStore::new(dir.path());
        let mut future = serde_json::to_value(Settings::default()).unwrap();
        future["columnOrder"]
            .as_array_mut()
            .unwrap()
            .push("futureColumn".into());
        future["hiddenColumns"]
            .as_array_mut()
            .unwrap()
            .push("futureColumn".into());
        future["columnWidths"]["futureColumn"] = 80.into();
        future["sortColumn"] = "futureColumn".into();
        fs::write(
            dir.path().join("settings.json"),
            serde_json::to_vec(&future).unwrap(),
        )
        .unwrap();

        let settings = store.load().unwrap().unwrap();
        assert_eq!(settings.column_order, Settings::default().column_order);
        assert_eq!(settings.hidden_columns, Settings::default().hidden_columns);
        assert!(!settings.column_widths.contains_key("futureColumn"));
        assert_eq!(settings.sort_column, None);
    }

    #[test]
    fn settings_load_adds_optional_date_columns_and_hides_them() {
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
        assert!(settings
            .column_order
            .ends_with(&["added".into(), "releaseDate".into()]));
        assert!(settings
            .hidden_columns
            .ends_with(&["added".into(), "releaseDate".into()]));
    }

    #[test]
    fn settings_load_migrates_the_legacy_default_library_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSettingsStore::new(dir.path());
        let legacy = serde_json::json!({
            "theme": "system", "zoom": 1.0, "zebra": true,
            "columnOrder": ["name", "artist", "album", "disc", "track", "time", "rating", "genre", "plays", "kind", "bitrate", "lastPlayed", "added", "releaseDate"],
            "autoAddSpotifyLibrary": true
        });
        fs::write(
            dir.path().join("settings.json"),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();

        assert_eq!(
            store.load().unwrap().unwrap().column_order,
            Settings::default().column_order
        );
    }

    #[test]
    fn playlist_layout_defaults_remove_empty_and_default_overrides() {
        let mut settings = Settings::default();
        settings.playlist_hidden_columns.insert(
            "playlist".into(),
            Settings::PLAYLIST_OPTIONAL_COLUMNS
                .map(String::from)
                .to_vec(),
        );
        settings
            .playlist_hidden_columns
            .insert("all-visible".into(), vec![]);
        settings.playlist_column_orders.insert(
            "playlist".into(),
            Settings::PLAYLIST_COLUMNS.map(String::from).to_vec(),
        );
        settings
            .playlist_column_widths
            .insert("playlist".into(), BTreeMap::new());

        settings.normalize();

        assert!(!settings.playlist_hidden_columns.contains_key("playlist"));
        assert_eq!(
            settings.playlist_hidden_columns["all-visible"],
            Vec::<String>::new()
        );
        assert!(settings.playlist_column_orders.is_empty());
        assert!(settings.playlist_column_widths.is_empty());
    }

    #[test]
    fn playlist_layout_overrides_normalize_and_validate_at_the_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSettingsStore::new(dir.path());
        let json = serde_json::json!({
            "theme": "system", "zoom": 1.0, "zebra": true,
            "columnOrder": Settings::default().column_order,
            "autoAddSpotifyLibrary": true,
            "playlistHiddenColumns": {"a": ["genre", "track", "genre"]},
            "playlistColumnOrders": {"a": ["genre", "name", "artist"]},
            "playlistColumnWidths": {"a": {"name": 220, "future": 80}}
        });
        fs::write(
            dir.path().join("settings.json"),
            serde_json::to_vec(&json).unwrap(),
        )
        .unwrap();

        let settings = store.load().unwrap().unwrap();
        assert_eq!(
            settings.playlist_hidden_columns["a"],
            vec!["genre".to_string(), "track".to_string()]
        );
        assert_eq!(
            settings.playlist_column_orders["a"],
            [
                "genre",
                "name",
                "artist",
                "album",
                "time",
                "rating",
                "plays",
                "disc",
                "kind",
                "bitrate",
                "lastPlayed",
                "added",
                "releaseDate",
                "track"
            ]
            .map(String::from)
            .to_vec()
        );
        assert_eq!(
            settings.playlist_column_widths["a"],
            BTreeMap::from([("name".into(), 220)])
        );
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
        assert!(settings.playlist_hidden_columns.is_empty());
        assert!(settings.playlist_column_orders.is_empty());
        assert!(settings.playlist_column_widths.is_empty());
    }

    #[test]
    fn missing_playback_backend_defaults_to_local() {
        let settings: Settings = serde_json::from_value(serde_json::json!({
            "theme": "system",
            "zoom": 1.0,
            "zebra": true,
            "columnOrder": ["track", "name", "time", "artist", "album", "genre", "rating"],
            "autoAddSpotifyLibrary": true
        }))
        .unwrap();
        assert_eq!(settings.playback_backend, PlaybackBackend::Local);
        assert_eq!(settings.repeat, RepeatMode::Off);
        assert_eq!(settings.streaming_bitrate, 320);
        assert!(!settings.normalize_volume);
        assert!(settings.gapless);
    }

    #[test]
    fn invalid_repeat_is_rejected() {
        let mut settings = serde_json::to_value(Settings::default()).unwrap();
        settings["repeat"] = "sometimes".into();
        assert!(serde_json::from_value::<Settings>(settings).is_err());
        assert!(serde_json::from_str::<RepeatMode>("\"sometimes\"").is_err());
    }

    #[test]
    fn playback_settings_and_view_keep_exact_lowercase_bytes() {
        let settings = Settings {
            playback_backend: PlaybackBackend::Connect,
            repeat: RepeatMode::One,
            ..Settings::default()
        };
        let bytes = serde_json::to_vec(&settings).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["playbackBackend"], "connect");
        assert_eq!(value["repeat"], "one");
        assert_eq!(
            serde_json::from_slice::<Settings>(&bytes).unwrap(),
            settings
        );

        let view = serde_json::to_value(SettingsView::from(&settings)).unwrap();
        assert_eq!(view["playbackBackend"], "connect");
        assert_eq!(view["repeat"], "one");
        assert!(serde_json::from_str::<PlaybackBackend>("\"Connect\"").is_err());
        assert!(serde_json::from_value::<SettingsPatch>(serde_json::json!({
            "playbackBackend": "unknown"
        }))
        .is_err());
    }

    #[test]
    fn lastfm_scrobbling_profile_is_validated_on_load_and_save() {
        for profile in [
            LastFmScrobblingProfile {
                username: "   ".into(),
                started_at: 1,
            },
            LastFmScrobblingProfile {
                username: "user".into(),
                started_at: 0,
            },
        ] {
            let settings = Settings {
                lastfm_scrobbling_profile: Some(profile.clone()),
                ..Settings::default()
            };
            assert!(settings.validate().is_err());

            let dir = tempfile::tempdir().unwrap();
            let store = FsSettingsStore::new(dir.path());
            let mut json = serde_json::to_value(Settings::default()).unwrap();
            json["lastfmScrobblingProfile"] = serde_json::to_value(profile).unwrap();
            fs::write(
                dir.path().join("settings.json"),
                serde_json::to_vec(&json).unwrap(),
            )
            .unwrap();
            assert!(store.load().is_err());
            assert!(store.save(&settings).is_err());
        }
    }

    #[test]
    fn streaming_bitrate_accepts_supported_qualities_only() {
        for streaming_bitrate in [96, 160, 320] {
            let settings = Settings {
                streaming_bitrate,
                ..Settings::default()
            };
            assert!(settings.validate().is_ok(), "{streaming_bitrate}");
        }
        for streaming_bitrate in [0, 95, 97, 256, u16::MAX] {
            let settings = Settings {
                streaming_bitrate,
                ..Settings::default()
            };
            assert_eq!(
                settings.validate().unwrap_err().to_string(),
                "settings streamingBitrate must be 96, 160, or 320",
                "{streaming_bitrate}"
            );
        }
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
    fn column_widths_allow_every_column_at_least_28px() {
        for column in Settings::COLUMNS {
            let settings = Settings {
                column_widths: BTreeMap::from([(column.into(), 28)]),
                ..Settings::default()
            };
            assert!(settings.validate().is_ok(), "{column}");
        }

        let mut settings = Settings::default();
        settings.column_widths.insert("name".into(), u32::MAX);
        assert!(settings.validate().is_ok());
        settings.column_widths.insert("artist".into(), 27);
        assert!(settings.validate().is_err());

        settings.column_widths = BTreeMap::from([("composer".into(), 84)]);
        assert!(settings.validate().is_err(), "composer");
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

        settings.hidden_columns = vec![];
        settings
            .playlist_hidden_columns
            .insert("playlist".into(), vec!["genre".into()]);
        assert!(settings.validate().is_ok());
        settings
            .playlist_hidden_columns
            .insert("playlist".into(), vec!["name".into()]);
        assert!(settings.validate().is_err());
    }

    #[test]
    fn playlist_layout_validation_rejects_invalid_columns_and_widths() {
        let mut settings = Settings::default();
        settings
            .playlist_column_orders
            .insert("playlist".into(), vec!["name".into()]);
        assert!(settings.validate().is_err());

        settings.playlist_column_orders.clear();
        settings
            .playlist_hidden_columns
            .insert("playlist".into(), vec!["track".into()]);
        assert!(settings.validate().is_ok());

        settings.playlist_hidden_columns.clear();
        settings
            .playlist_column_widths
            .insert("playlist".into(), BTreeMap::from([("track".into(), 28)]));
        assert!(settings.validate().is_ok());
        settings
            .playlist_column_widths
            .insert("playlist".into(), BTreeMap::from([("track".into(), 27)]));
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

    #[test]
    fn oversized_user_state_is_rejected_without_removing_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::File::create(&path)
            .unwrap()
            .set_len(MAX_SETTINGS_BYTES + 1)
            .unwrap();

        assert!(matches!(
            FsSettingsStore::new(dir.path()).load(),
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::InvalidData
        ));
        assert!(path.exists());
    }

    #[test]
    fn oversized_reconstructible_catalog_is_quarantined() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spotify-catalog.json");
        fs::File::create(&path)
            .unwrap()
            .set_len(MAX_SPOTIFY_STATE_BYTES + 1)
            .unwrap();

        assert_eq!(
            FsSpotifyCatalogStore::new(dir.path()).load().unwrap(),
            SpotifyCatalog::default()
        );
        assert!(!path.exists());
        assert!(fs::read_dir(dir.path()).unwrap().flatten().any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("spotify-catalog.json.corrupt-")
        }));
    }

    #[test]
    fn oversized_reconstructible_json_is_quarantined() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("playlists.json");
        fs::File::create(&path)
            .unwrap()
            .set_len(MAX_PLAYLIST_BYTES + 1)
            .unwrap();

        assert_eq!(
            FsPlaylistStore::new(dir.path()).load().unwrap(),
            PlaylistCache::default()
        );
        assert!(!path.exists());
        assert!(fs::read_dir(dir.path()).unwrap().flatten().any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("playlists.json.oversized-")
        }));
    }
}

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use retune_core::model::{AlbumKey, Library, Rating, TrackEdit};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use tokio::sync::Mutex;

pub(crate) const SESSION_VERSION: u8 = 1;
pub(crate) const LASTFM_PAGE_LIMIT: u32 = 200;
pub(crate) const MAX_SERIALIZED_SESSION_BYTES: usize = 100 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ImportPhase {
    Downloading,
    Matching,
    Review,
    Done,
    Suspended,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum CountMode {
    Sum,
    Overwrite,
    Zero,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Confidence {
    Exact,
    Likely,
    Low,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum AlbumRelation {
    BestMatch,
    SameSongs,
    Superset,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportDefaults {
    pub import_content: bool,
    pub include_historical_play_counts: bool,
    pub whole_album: bool,
}

impl Default for ImportDefaults {
    fn default() -> Self {
        Self {
            import_content: true,
            include_historical_play_counts: true,
            whole_album: false,
        }
    }
}

impl ImportDefaults {
    fn validate(&self) -> Result<(), String> {
        if !self.import_content && !self.include_historical_play_counts {
            return Err(
                "Select content or historical play counts before starting the import.".into(),
            );
        }
        if self.whole_album && !self.import_content {
            return Err("Whole-album import requires content import to be enabled.".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SourceVariant {
    pub artist: String,
    pub album: String,
    pub track: String,
    pub play_count: u64,
    pub earliest: u64,
    pub latest: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SourceRow {
    pub stable_id: String,
    pub artist: String,
    pub album: String,
    pub track: String,
    pub variants: Vec<SourceVariant>,
    pub play_count: u64,
    pub earliest: u64,
    pub latest: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AlbumCandidate {
    pub uri: String,
    pub name: String,
    pub artist: String,
    pub track_uris: Vec<String>,
    #[serde(default)]
    pub track_names: Vec<String>,
    #[serde(default)]
    pub track_artists: Vec<String>,
    #[serde(default)]
    pub track_albums: Vec<String>,
    pub relation: Option<AlbumRelation>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MatchResult {
    pub source_id: String,
    pub search_term: String,
    pub confidence: Option<Confidence>,
    pub selected_uri: Option<String>,
    pub candidates: Vec<AlbumCandidate>,
    pub track_matches: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum RowStatus {
    Pending,
    Done,
    Skipped,
    IgnoredAlbum,
    IgnoredArtist,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RowDecision {
    pub status: RowStatus,
    pub excluded: bool,
}

impl Default for RowDecision {
    fn default() -> Self {
        Self {
            status: RowStatus::Pending,
            excluded: false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PageOptions {
    pub import_content: bool,
    pub include_historical_play_counts: bool,
    pub whole_album: bool,
    pub genre: Option<String>,
    pub rating: Option<u8>,
    pub selected_track_ids: BTreeSet<String>,
}

impl Default for PageOptions {
    fn default() -> Self {
        Self {
            import_content: true,
            include_historical_play_counts: true,
            whole_album: false,
            genre: None,
            rating: None,
            selected_track_ids: BTreeSet::new(),
        }
    }
}

impl PageOptions {
    fn from_defaults(defaults: &ImportDefaults) -> Self {
        Self {
            import_content: defaults.import_content,
            include_historical_play_counts: defaults.include_historical_play_counts,
            whole_album: defaults.whole_album,
            ..Self::default()
        }
    }

    fn validate(&self) -> Result<(), String> {
        ImportDefaults {
            import_content: self.import_content,
            include_historical_play_counts: self.include_historical_play_counts,
            whole_album: self.whole_album,
        }
        .validate()?;
        if self.rating.is_some_and(|rating| !(1..=5).contains(&rating)) {
            return Err("Rating must be between 1 and 5.".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RetryableError {
    pub message: String,
    pub attempt: u32,
    pub retryable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportBatch {
    pub page: u32,
    pub source_ids: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LastFmImportSessionV1 {
    pub version: u8,
    pub lastfm_username: String,
    pub spotify_account_id: String,
    pub snapshot_to: u64,
    pub next_page: u32,
    pub total_pages: Option<u32>,
    pub total_scrobbles: u64,
    pub included_scrobbles: u64,
    pub skipped_now_playing: u64,
    pub skipped_undated: u64,
    pub phase: ImportPhase,
    pub retryable_error: Option<RetryableError>,
    pub defaults: ImportDefaults,
    pub batches: Vec<ImportBatch>,
    pub rows: Vec<SourceRow>,
    pub matches: BTreeMap<String, MatchResult>,
    pub decisions: BTreeMap<String, RowDecision>,
    pub page_options: BTreeMap<String, PageOptions>,
    pub count_modes: BTreeMap<String, CountMode>,
    pub search_terms: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportStateView {
    pub phase: Option<ImportPhase>,
    pub username: Option<String>,
    pub spotify_account_id: Option<String>,
    pub next_page: u32,
    pub total_pages: Option<u32>,
    pub total_scrobbles: u64,
    pub included_scrobbles: u64,
    pub matched_rows: usize,
    pub match_total: usize,
    pub defaults: ImportDefaults,
    pub remaining: usize,
    pub retryable_error: Option<RetryableError>,
    pub search_terms: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum QueueStatus {
    Done,
    Skipped,
    IgnoredAlbum,
    IgnoredArtist,
    Excluded,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportQueueItem {
    pub artist: String,
    pub album: String,
    pub play_count: u64,
    pub latest: u64,
    pub source_ids: Vec<String>,
    pub remaining: bool,
    pub album_entities: u32,
    pub track_entities: u32,
    pub status: Option<QueueStatus>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportPageItem {
    pub source: SourceRow,
    pub decision: RowDecision,
    pub match_result: Option<MatchResult>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportPageView {
    pub state: ImportStateView,
    pub artist: String,
    pub album: String,
    pub page_number: usize,
    pub page_count: usize,
    pub rows: Vec<ImportPageItem>,
    pub options: PageOptions,
    pub fuzzy_groups: BTreeMap<String, Vec<SourceRow>>,
    pub count_modes: BTreeMap<String, CountMode>,
    pub locked_count_modes: BTreeSet<String>,
}

impl LastFmImportSessionV1 {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn new(
        lastfm_username: String,
        spotify_account_id: String,
        snapshot_to: u64,
    ) -> Self {
        Self::new_with_defaults(
            lastfm_username,
            spotify_account_id,
            snapshot_to,
            ImportDefaults::default(),
        )
    }

    pub(crate) fn new_with_defaults(
        lastfm_username: String,
        spotify_account_id: String,
        snapshot_to: u64,
        defaults: ImportDefaults,
    ) -> Self {
        Self {
            version: SESSION_VERSION,
            lastfm_username,
            spotify_account_id,
            snapshot_to,
            next_page: 1,
            total_pages: None,
            total_scrobbles: 0,
            included_scrobbles: 0,
            skipped_now_playing: 0,
            skipped_undated: 0,
            phase: ImportPhase::Downloading,
            retryable_error: None,
            defaults,
            batches: Vec::new(),
            rows: Vec::new(),
            matches: BTreeMap::new(),
            decisions: BTreeMap::new(),
            page_options: BTreeMap::new(),
            count_modes: BTreeMap::new(),
            search_terms: true,
        }
    }

    pub(crate) fn remaining(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| {
                let decision = self
                    .decisions
                    .get(&row.stable_id)
                    .cloned()
                    .unwrap_or_default();
                matches!(decision.status, RowStatus::Pending | RowStatus::Skipped)
                    && !decision.excluded
            })
            .count()
    }

    fn options_for(&self, artist: &str, album: &str) -> PageOptions {
        self.page_options
            .get(&format!("{artist}\u{1f}{album}"))
            .cloned()
            .unwrap_or_else(|| {
                let selected_track_ids = self
                    .rows
                    .iter()
                    .filter(|row| {
                        row.artist == artist
                            && row.album == album
                            && matches!(
                                default_decision(self, &row.stable_id).status,
                                RowStatus::Pending | RowStatus::Skipped
                            )
                            && !default_decision(self, &row.stable_id).excluded
                    })
                    .map(|row| row.stable_id.clone())
                    .collect();
                PageOptions {
                    selected_track_ids,
                    ..PageOptions::from_defaults(&self.defaults)
                }
            })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ParsedRecentTracksPage {
    pub page: u32,
    pub total_pages: Option<u32>,
    pub total: Option<u64>,
    pub tracks: Vec<ParsedScrobble>,
    pub skipped_now_playing: u64,
    pub skipped_undated: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ParsedScrobble {
    pub artist: String,
    pub album: String,
    pub track: String,
    pub timestamp: u64,
}

pub(crate) fn parse_recent_tracks_page(value: &Value) -> Result<ParsedRecentTracksPage, String> {
    let recent = value
        .get("recenttracks")
        .ok_or_else(|| "Last.fm response did not contain recent tracks.".to_string())?;
    let attributes = recent.get("@attr");
    let page = attributes
        .and_then(|value| value.get("page"))
        .and_then(value_string)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let total_pages = attributes
        .and_then(|value| value.get("totalPages"))
        .and_then(value_string)
        .and_then(|value| value.parse().ok());
    let total = attributes
        .and_then(|value| value.get("total"))
        .and_then(value_string)
        .and_then(|value| value.parse().ok());
    let entries = match recent.get("track") {
        Some(Value::Array(entries)) => entries.iter().collect::<Vec<_>>(),
        Some(Value::Object(_)) => vec![recent.get("track").expect("track was just checked")],
        Some(Value::Null) | None => Vec::new(),
        Some(_) => return Err("Last.fm recent tracks had an invalid track list.".into()),
    };
    let mut parsed = ParsedRecentTracksPage {
        page,
        total_pages,
        total,
        ..ParsedRecentTracksPage::default()
    };
    for entry in entries {
        if is_now_playing(entry) {
            parsed.skipped_now_playing += 1;
            continue;
        }
        let artist = entry.get("artist").and_then(value_text).unwrap_or_default();
        let track = entry.get("name").and_then(value_string).unwrap_or_default();
        let album = entry.get("album").and_then(value_text).unwrap_or_default();
        let timestamp = entry
            .get("date")
            .and_then(|date| date.get("uts"))
            .and_then(value_string)
            .and_then(|value| value.parse().ok())
            .filter(|timestamp| *timestamp > 0);
        let Some(timestamp) = timestamp else {
            parsed.skipped_undated += 1;
            continue;
        };
        if artist.trim().is_empty() || track.trim().is_empty() {
            parsed.skipped_undated += 1;
            continue;
        }
        parsed.tracks.push(ParsedScrobble {
            artist: artist.trim().to_owned(),
            album: album.trim().to_owned(),
            track: track.trim().to_owned(),
            timestamp,
        });
    }
    Ok(parsed)
}

fn value_string(value: &Value) -> Option<&str> {
    value.as_str()
}

fn value_text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.get("#text").and_then(value_string).map(str::to_owned))
        .or_else(|| value.get("text").and_then(value_string).map(str::to_owned))
}

fn is_now_playing(value: &Value) -> bool {
    matches!(
        value
            .get("@attr")
            .and_then(|attributes| attributes.get("nowplaying")),
        Some(Value::String(value)) if value == "1" || value.eq_ignore_ascii_case("true")
    ) || matches!(
        value
            .get("@attr")
            .and_then(|attributes| attributes.get("nowplaying")),
        Some(Value::Number(value)) if value.as_u64() == Some(1)
    ) || matches!(
        value
            .get("@attr")
            .and_then(|attributes| attributes.get("nowplaying")),
        Some(Value::Bool(true))
    )
}

pub(crate) fn normalize_for_match(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn source_id(artist: &str, album: &str, track: &str) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}",
        normalize_for_match(artist),
        normalize_for_match(album),
        normalize_for_match(track)
    )
}

pub(crate) fn aggregate_scrobbles(rows: &mut Vec<SourceRow>, scrobbles: &[ParsedScrobble]) {
    for scrobble in scrobbles {
        let id = source_id(&scrobble.artist, &scrobble.album, &scrobble.track);
        let Some(row) = rows.iter_mut().find(|row| row.stable_id == id) else {
            rows.push(SourceRow {
                stable_id: id,
                artist: scrobble.artist.clone(),
                album: scrobble.album.clone(),
                track: scrobble.track.clone(),
                variants: Vec::new(),
                play_count: 0,
                earliest: scrobble.timestamp,
                latest: scrobble.timestamp,
            });
            let row = rows.last_mut().expect("row was just pushed");
            add_variant(row, scrobble);
            continue;
        };
        add_variant(row, scrobble);
    }
}

fn add_variant(row: &mut SourceRow, scrobble: &ParsedScrobble) {
    row.play_count = row.play_count.saturating_add(1);
    row.earliest = row.earliest.min(scrobble.timestamp);
    row.latest = row.latest.max(scrobble.timestamp);
    if let Some(variant) = row.variants.iter_mut().find(|variant| {
        variant.artist == scrobble.artist
            && variant.album == scrobble.album
            && variant.track == scrobble.track
    }) {
        variant.play_count = variant.play_count.saturating_add(1);
        variant.earliest = variant.earliest.min(scrobble.timestamp);
        variant.latest = variant.latest.max(scrobble.timestamp);
        return;
    }
    row.variants.push(SourceVariant {
        artist: scrobble.artist.clone(),
        album: scrobble.album.clone(),
        track: scrobble.track.clone(),
        play_count: 1,
        earliest: scrobble.timestamp,
        latest: scrobble.timestamp,
    });
}

pub(crate) fn resolved_play_count(rows: &[&SourceRow], mode: CountMode) -> u64 {
    match mode {
        CountMode::Sum => rows
            .iter()
            .map(|row| row.play_count)
            .fold(0, u64::saturating_add),
        CountMode::Overwrite => rows
            .iter()
            .flat_map(|row| row.variants.iter())
            .map(|variant| variant.play_count)
            .max()
            .unwrap_or(0),
        CountMode::Zero => 0,
    }
}

pub(crate) fn resolved_timestamps(rows: &[&SourceRow]) -> Option<(u64, u64)> {
    let earliest = rows.iter().map(|row| row.earliest).min()?;
    let latest = rows.iter().map(|row| row.latest).max()?;
    Some((earliest, latest))
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn classify_album_candidates(
    source_track_uris: &[String],
    candidates: &mut [AlbumCandidate],
) {
    let source = source_track_uris.iter().collect::<BTreeSet<_>>();
    for candidate in candidates.iter_mut() {
        let target = candidate.track_uris.iter().collect::<BTreeSet<_>>();
        let overlap = source.intersection(&target).count();
        candidate.relation = if overlap == source.len() && target.len() == source.len() {
            Some(AlbumRelation::BestMatch)
        } else if overlap == source.len() && target.len() > source.len() {
            Some(AlbumRelation::Superset)
        } else if overlap > 0 && overlap * 2 >= source.len().max(1) {
            Some(AlbumRelation::SameSongs)
        } else {
            None
        };
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HistoryUpdate {
    pub uri: String,
    pub play_count: Option<u64>,
    pub earliest: Option<u64>,
    pub latest: Option<u64>,
}

pub(crate) fn apply_history_updates(library: &mut Library, updates: &[HistoryUpdate]) {
    for update in updates {
        let Some(track) = library
            .tracks_mut()
            .iter_mut()
            .find(|track| track.uri == update.uri)
        else {
            continue;
        };
        if let Some(play_count) = update.play_count {
            track.play_count = track.play_count.max(play_count.min(u32::MAX as u64) as u32);
        }
        if let Some(latest) = update.latest {
            track.last_played_at = Some(track.last_played_at.unwrap_or(0).max(latest));
        }
        if let Some(earliest) = update.earliest {
            track.added_at = Some(track.added_at.unwrap_or(earliest).min(earliest));
        }
    }
}

pub(crate) fn apply_metadata(
    library: &mut Library,
    tracks: &[String],
    whole_album: bool,
    genre: Option<&str>,
    rating: Option<u8>,
) -> Result<(), String> {
    let ids = library
        .tracks()
        .iter()
        .filter(|track| tracks.iter().any(|uri| uri == &track.uri))
        .map(|track| track.id)
        .collect::<Vec<_>>();
    if let Some(genre) = genre.map(str::trim).filter(|genre| !genre.is_empty()) {
        for id in &ids {
            library
                .edit(
                    *id,
                    TrackEdit {
                        cat: Some(genre.to_owned()),
                        ..TrackEdit::default()
                    },
                )
                .map_err(|error| error.to_string())?;
        }
    }
    let Some(stars) = rating else {
        return Ok(());
    };
    let rating = Rating::new(stars).ok_or_else(|| "Rating must be between 1 and 5.".to_string())?;
    if whole_album {
        let Some(first) = ids.first().and_then(|id| library.get(*id)) else {
            return Ok(());
        };
        library.set_album_rating(AlbumKey::of(first), Some(rating));
    } else {
        for id in ids {
            library
                .set_track_rating(id, Some(rating))
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct ImportSessionStore {
    path: PathBuf,
}

impl ImportSessionStore {
    pub(crate) fn new(app_data_dir: impl AsRef<Path>) -> Self {
        Self {
            path: app_data_dir.as_ref().join("lastfm-import.json"),
        }
    }

    pub(crate) fn load(&self) -> Result<Option<LastFmImportSessionV1>, String> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err("Could not read the Last.fm import session.".into()),
        };
        if bytes.len() > MAX_SERIALIZED_SESSION_BYTES {
            self.quarantine()?;
            return Ok(None);
        }
        let parsed = serde_json::from_slice::<LastFmImportSessionV1>(&bytes);
        match parsed {
            Ok(session)
                if session.version == SESSION_VERSION
                    && session.defaults.validate().is_ok()
                    && session
                        .page_options
                        .values()
                        .all(|options| options.validate().is_ok()) =>
            {
                Ok(Some(session))
            }
            Ok(_) | Err(_) => {
                self.quarantine()?;
                Ok(None)
            }
        }
    }

    pub(crate) fn save(&self, session: &LastFmImportSessionV1) -> Result<(), String> {
        let bytes = serde_json::to_vec(session)
            .map_err(|_| "Could not serialize the Last.fm import session.".to_string())?;
        if bytes.len() > MAX_SERIALIZED_SESSION_BYTES {
            return Err("The Last.fm import session exceeds the 100 MB safety limit.".into());
        }
        super::lastfm::atomic_write(&self.path, &bytes, true)
    }

    fn quarantine(&self) -> Result<(), String> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let target = self.path.with_extension(format!("quarantine-{stamp}"));
        fs::rename(&self.path, target)
            .map_err(|_| "Could not quarantine the Last.fm import session.".to_string())
    }
}

pub(crate) struct Service {
    store: ImportSessionStore,
    session: Mutex<Option<LastFmImportSessionV1>>,
    running: AtomicBool,
}

impl Service {
    pub(crate) fn new(app_data_dir: impl AsRef<Path>) -> Arc<Self> {
        let store = ImportSessionStore::new(app_data_dir);
        let session = match store.load() {
            Ok(session) => session,
            Err(error) => {
                log::warn!("Last.fm importer state is unavailable: {error}");
                None
            }
        };
        Arc::new(Self {
            store,
            session: Mutex::new(session),
            running: AtomicBool::new(false),
        })
    }

    pub(crate) async fn state(&self) -> ImportStateView {
        self.state_with_identity(None).await
    }

    pub(crate) async fn state_with_identity(
        &self,
        identity: Option<(String, String)>,
    ) -> ImportStateView {
        let session = self.session.lock().await;
        match session.as_ref() {
            Some(session) if session.phase == ImportPhase::Suspended => suspended_state_view(),
            Some(session) => state_view(Some(session)),
            None => state_view_with_identity(None, identity.as_ref()),
        }
    }

    async fn snapshot(&self) -> Option<LastFmImportSessionV1> {
        self.session.lock().await.clone()
    }

    async fn persist(&self, session: LastFmImportSessionV1) -> Result<(), String> {
        let store = self.store.clone();
        tauri::async_runtime::spawn_blocking(move || store.save(&session))
            .await
            .map_err(|_| "Last.fm import persistence task stopped.".to_string())?
    }

    #[cfg(test)]
    async fn save(&self, session: LastFmImportSessionV1) -> Result<(), String> {
        self.mutate_session(|_| Ok((Some(session), ()))).await
    }

    async fn mutate_session<R, F>(&self, mutation: F) -> Result<R, String>
    where
        F: FnOnce(
            Option<LastFmImportSessionV1>,
        ) -> Result<(Option<LastFmImportSessionV1>, R), String>,
    {
        let mut current = self.session.lock().await;
        let (next, result) = mutation(current.clone())?;
        if let Some(session) = next.as_ref() {
            self.persist(session.clone()).await?;
        }
        *current = next;
        Ok(result)
    }

    async fn mutate_owned_session<R, F>(
        &self,
        username: &str,
        spotify_account_id: &str,
        allowed_phase: fn(ImportPhase) -> bool,
        mutation: F,
    ) -> Result<R, String>
    where
        F: FnOnce(LastFmImportSessionV1) -> Result<(LastFmImportSessionV1, R), String>,
    {
        self.mutate_session(|session| {
            let Some(session) = session else {
                return Err("No Last.fm import session is active.".into());
            };
            if session.lastfm_username != username
                || session.spotify_account_id != spotify_account_id
                || !allowed_phase(session.phase)
            {
                return Err(
                    "The Last.fm import is no longer active for this account or phase.".into(),
                );
            }
            let (session, result) = mutation(session)?;
            Ok((Some(session), result))
        })
        .await
    }

    async fn suspend_for_account_mismatch(&self) -> Result<(), String> {
        self.mutate_session(|session| {
            let Some(mut session) = session else {
                return Ok((None, ()));
            };
            session.phase = ImportPhase::Suspended;
            session.retryable_error = Some(RetryableError {
                message: "This import is suspended because the connected account changed. Reconnect Last.fm and Spotify to resume.".into(),
                attempt: 0,
                retryable: false,
            });
            Ok((Some(session), ()))
        })
        .await
    }

    pub(crate) async fn start_or_resume(
        &self,
        username: &str,
        spotify_account_id: &str,
        snapshot_to: u64,
        defaults: Option<ImportDefaults>,
    ) -> Result<ImportStateView, String> {
        if let Some(defaults) = &defaults {
            defaults.validate()?;
        }
        let result = self
            .mutate_session(|current| {
                let session = match current {
                    Some(mut session) => {
                        if session.lastfm_username != username
                            || session.spotify_account_id != spotify_account_id
                        {
                            session.phase = ImportPhase::Suspended;
                            session.retryable_error = Some(RetryableError {
                                message: "This import is suspended because the connected account changed. Reconnect Last.fm and Spotify to resume.".into(),
                                attempt: 0,
                                retryable: false,
                            });
                            return Ok((
                                Some(session),
                                Err("The saved Last.fm import belongs to a different account; it is suspended for safety.".into()),
                            ));
                        }
                        if session.phase == ImportPhase::Suspended {
                            session.phase = if session
                                .total_pages
                                .is_some_and(|total_pages| session.next_page > total_pages)
                            {
                                ImportPhase::Matching
                            } else {
                                ImportPhase::Downloading
                            };
                            session.retryable_error = None;
                        }
                        session
                    }
                    None => LastFmImportSessionV1::new_with_defaults(
                        username.to_owned(),
                        spotify_account_id.to_owned(),
                        snapshot_to,
                        defaults.unwrap_or_default(),
                    ),
                };
                let view = state_view(Some(&session));
                Ok((Some(session), Ok(view)))
            })
            .await?;
        result
    }

    async fn checkpoint_page(
        &self,
        page: u32,
        parsed: &ParsedRecentTracksPage,
    ) -> Result<ImportStateView, String> {
        let result = self
            .mutate_session(|current| {
                let Some(mut session) = current else {
                    return Err("No Last.fm import session is active.".into());
                };
                if session.phase != ImportPhase::Downloading {
                    return Ok((Some(session.clone()), state_view(Some(&session))));
                }
                if parsed.page != page {
                    return Err(format!(
                        "Last.fm response was for page {}, expected page {page}.",
                        parsed.page
                    ));
                }
                if page < session.next_page {
                    return Ok((Some(session.clone()), state_view(Some(&session))));
                }
                if page > session.next_page {
                    return Err("Last.fm import pages must be checkpointed sequentially.".into());
                }
                aggregate_scrobbles(&mut session.rows, &parsed.tracks);
                session.total_pages = parsed.total_pages.or(session.total_pages);
                session.total_scrobbles = parsed.total.unwrap_or(session.total_scrobbles);
                session.included_scrobbles = session
                    .included_scrobbles
                    .saturating_add(parsed.tracks.len() as u64);
                session.skipped_now_playing = session
                    .skipped_now_playing
                    .saturating_add(parsed.skipped_now_playing);
                session.skipped_undated = session
                    .skipped_undated
                    .saturating_add(parsed.skipped_undated);
                session.batches.push(ImportBatch {
                    page,
                    source_ids: parsed
                        .tracks
                        .iter()
                        .map(|track| source_id(&track.artist, &track.album, &track.track))
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect(),
                });
                session.next_page = page.saturating_add(1);
                if session
                    .total_pages
                    .is_some_and(|total_pages| session.next_page > total_pages)
                    || (parsed.total_pages.is_none()
                        && parsed.tracks.len() < LASTFM_PAGE_LIMIT as usize)
                {
                    session.phase = ImportPhase::Matching;
                }
                session.retryable_error = None;
                Ok((Some(session.clone()), state_view(Some(&session))))
            })
            .await?;
        Ok(result)
    }

    async fn set_retryable_error(&self, error: RetryableError) -> Result<(), String> {
        self.mutate_session(|session| {
            let Some(mut session) = session else {
                return Ok((None, ()));
            };
            if session.phase == ImportPhase::Suspended {
                return Ok((Some(session), ()));
            }
            session.retryable_error = Some(error);
            Ok((Some(session), ()))
        })
        .await
    }

    async fn set_match(
        &self,
        username: &str,
        spotify_account_id: &str,
        result: MatchResult,
    ) -> Result<(), String> {
        self.set_matches(username, spotify_account_id, vec![result])
            .await
    }

    async fn set_matches(
        &self,
        username: &str,
        spotify_account_id: &str,
        results: Vec<MatchResult>,
    ) -> Result<(), String> {
        self.mutate_owned_session(
            username,
            spotify_account_id,
            review_phase_allowed,
            |mut session| {
                for result in results {
                    session.matches.insert(result.source_id.clone(), result);
                }
                Ok((session, ()))
            },
        )
        .await
    }

    async fn set_matches_during_matching(
        &self,
        username: &str,
        spotify_account_id: &str,
        results: Vec<MatchResult>,
    ) -> Result<(), String> {
        self.mutate_session(|session| {
            let Some(mut session) = session else {
                return Err("No Last.fm import session is active.".into());
            };
            if session.phase != ImportPhase::Matching
                || session.lastfm_username != username
                || session.spotify_account_id != spotify_account_id
            {
                return Err("Last.fm matching stopped because the connected account or import phase changed.".into());
            }
            for result in results {
                session.matches.insert(result.source_id.clone(), result);
            }
            Ok((Some(session), ()))
        })
        .await
    }

    async fn set_count_mode(
        &self,
        username: &str,
        spotify_account_id: &str,
        target_uri: &str,
        mode: CountMode,
    ) -> Result<(), String> {
        self.mutate_owned_session(
            username,
            spotify_account_id,
            review_phase_allowed,
            |mut session| {
                let current = session
                    .count_modes
                    .get(target_uri)
                    .copied()
                    .unwrap_or(CountMode::Sum);
                if current != mode && locked_count_modes(&session).contains(target_uri) {
                    return Err(
                        "This Spotify target's play-count strategy is locked after import.".into(),
                    );
                }
                session.count_modes.insert(target_uri.to_owned(), mode);
                Ok((session, ()))
            },
        )
        .await
    }

    async fn set_search_terms(
        &self,
        username: &str,
        spotify_account_id: &str,
        search_terms: bool,
    ) -> Result<(), String> {
        self.mutate_owned_session(
            username,
            spotify_account_id,
            review_phase_allowed,
            |mut session| {
                session.search_terms = search_terms;
                Ok((session, ()))
            },
        )
        .await
    }

    async fn finish_matching_if_current(
        &self,
        username: &str,
        spotify_account_id: &str,
    ) -> Result<(), String> {
        self.mutate_session(|session| {
            let Some(mut session) = session else {
                return Ok((None, ()));
            };
            if session.phase != ImportPhase::Matching
                || session.lastfm_username != username
                || session.spotify_account_id != spotify_account_id
            {
                return Err("Last.fm matching stopped because the connected account or import phase changed.".into());
            }
            session.phase = ImportPhase::Review;
            session.retryable_error = None;
            Ok((Some(session), ()))
        })
        .await
    }

    pub(crate) async fn queue(&self) -> Vec<ImportQueueItem> {
        let Some(session) = self.snapshot().await else {
            return Vec::new();
        };
        if session.phase == ImportPhase::Suspended {
            return Vec::new();
        }
        let mut grouped = BTreeMap::<(String, String), Vec<&SourceRow>>::new();
        for row in &session.rows {
            grouped
                .entry((row.artist.clone(), row.album.clone()))
                .or_default()
                .push(row);
        }
        grouped
            .into_iter()
            .map(|((artist, album), rows)| {
                let options = session.options_for(&artist, &album);
                let remaining = rows.iter().any(|row| {
                    let decision = default_decision(&session, &row.stable_id);
                    matches!(decision.status, RowStatus::Pending | RowStatus::Skipped)
                        && !decision.excluded
                });
                let selected = rows
                    .iter()
                    .filter(|row| {
                        let decision = default_decision(&session, &row.stable_id);
                        options.selected_track_ids.contains(&row.stable_id)
                            && matches!(decision.status, RowStatus::Pending | RowStatus::Skipped)
                            && !decision.excluded
                    })
                    .collect::<Vec<_>>();
                let mut album_entities = 0;
                let mut track_uris = BTreeSet::new();
                if options.import_content {
                    if options.whole_album {
                        album_entities = selected
                            .iter()
                            .filter_map(|row| session.matches.get(&row.stable_id))
                            .filter_map(|result| {
                                result.selected_uri.as_deref().or_else(|| {
                                    best_candidate(result).map(|candidate| candidate.uri.as_str())
                                })
                            })
                            .any(|uri| uri.starts_with("spotify:album:"))
                            as u32;
                    } else {
                        for row in &selected {
                            if let Some(result) = session.matches.get(&row.stable_id) {
                                if let Some(uri) = matched_track_uri_for_row(result, row) {
                                    track_uris.insert(uri);
                                }
                            }
                        }
                    }
                }
                ImportQueueItem {
                    artist,
                    album,
                    play_count: rows
                        .iter()
                        .map(|row| row.play_count)
                        .fold(0, u64::saturating_add),
                    latest: rows.iter().map(|row| row.latest).max().unwrap_or_default(),
                    source_ids: rows.iter().map(|row| row.stable_id.clone()).collect(),
                    remaining,
                    album_entities,
                    track_entities: track_uris.len() as u32,
                    status: queue_status(&session, &rows),
                }
            })
            .collect()
    }

    pub(crate) async fn page(&self, artist: &str, album: &str) -> Option<ImportPageView> {
        let session = self.snapshot().await?;
        if session.phase == ImportPhase::Suspended {
            return None;
        }
        let pages = session
            .rows
            .iter()
            .map(|row| (row.artist.clone(), row.album.clone()))
            .collect::<BTreeSet<_>>();
        let page_number = pages
            .iter()
            .position(|(page_artist, page_album)| page_artist == artist && page_album == album)
            .map(|index| index + 1)
            .unwrap_or(1);
        let rows = session
            .rows
            .iter()
            .filter(|row| row.artist == artist && row.album == album)
            .map(|row| ImportPageItem {
                source: row.clone(),
                decision: default_decision(&session, &row.stable_id),
                match_result: session.matches.get(&row.stable_id).cloned(),
            })
            .collect();
        let options = session.options_for(artist, album);
        let mut fuzzy_groups = BTreeMap::<String, Vec<SourceRow>>::new();
        for row in &session.rows {
            let decision = default_decision(&session, &row.stable_id);
            let participates = !decision.excluded
                && match decision.status {
                    RowStatus::Done => true,
                    RowStatus::Pending | RowStatus::Skipped => session
                        .options_for(&row.artist, &row.album)
                        .selected_track_ids
                        .contains(&row.stable_id),
                    RowStatus::IgnoredAlbum | RowStatus::IgnoredArtist => false,
                };
            if !participates {
                continue;
            }
            let Some(target_uri) = session
                .matches
                .get(&row.stable_id)
                .and_then(|result| matched_track_uri(result, &row.stable_id))
            else {
                continue;
            };
            fuzzy_groups
                .entry(target_uri)
                .or_default()
                .push(row.clone());
        }
        fuzzy_groups
            .retain(|_, rows| rows.len() > 1 || rows.iter().any(|row| row.variants.len() > 1));
        Some(ImportPageView {
            state: state_view(Some(&session)),
            artist: artist.to_owned(),
            album: album.to_owned(),
            page_number,
            page_count: pages.len(),
            rows,
            options,
            fuzzy_groups,
            count_modes: session.count_modes.clone(),
            locked_count_modes: locked_count_modes(&session),
        })
    }

    async fn update_options(
        &self,
        username: &str,
        spotify_account_id: &str,
        artist: &str,
        album: &str,
        options: PageOptions,
    ) -> Result<(), String> {
        options.validate()?;
        self.mutate_owned_session(
            username,
            spotify_account_id,
            review_phase_allowed,
            |mut session| {
                session
                    .page_options
                    .insert(format!("{artist}\u{1f}{album}"), options);
                Ok((session, ()))
            },
        )
        .await
    }

    async fn review_action(
        &self,
        username: &str,
        spotify_account_id: &str,
        id: &str,
        action: &str,
        artist: &str,
        album: &str,
    ) -> Result<(), String> {
        self.mutate_owned_session(
            username,
            spotify_account_id,
            review_phase_allowed,
            |mut session| {
                match action {
                    "exclude" | "undo-exclude" => {
                        exclude_row(&mut session, id, action == "exclude");
                    }
                    "ignore-album" => ignore_album(&mut session, artist, album),
                    "ignore-artist" => ignore_artist(&mut session, artist),
                    "skip-album" => skip_album(&mut session, artist, album),
                    "restore" => {
                        let ids = session
                            .rows
                            .iter()
                            .filter(|row| {
                                row.artist == artist
                                    && row.album == album
                                    && is_actionable(&session, &row.stable_id)
                            })
                            .map(|row| row.stable_id.clone())
                            .collect::<Vec<_>>();
                        for id in ids {
                            session.decisions.insert(id, RowDecision::default());
                        }
                    }
                    _ => return Err("Unknown Last.fm import review action.".into()),
                }
                update_review_phase(&mut session);
                Ok((session, ()))
            },
        )
        .await
    }

    async fn commit_rows(
        &self,
        username: &str,
        spotify_account_id: &str,
        ids: &[String],
        artist: &str,
        album: &str,
        options: PageOptions,
    ) -> Result<ImportStateView, String> {
        self.mutate_owned_session(
            username,
            spotify_account_id,
            review_phase_allowed,
            |mut session| {
                session
                    .page_options
                    .insert(format!("{artist}\u{1f}{album}"), options);
                for id in ids {
                    session.decisions.insert(
                        id.clone(),
                        RowDecision {
                            status: RowStatus::Done,
                            excluded: false,
                        },
                    );
                }
                if session.remaining() == 0 {
                    session.phase = ImportPhase::Done;
                }
                let view = state_view(Some(&session));
                Ok((session, view))
            },
        )
        .await
    }

    async fn select_match(
        &self,
        username: &str,
        spotify_account_id: &str,
        source_id: &str,
        uri: &str,
    ) -> Result<(), String> {
        self.mutate_owned_session(
            username,
            spotify_account_id,
            review_phase_allowed,
            |mut session| {
                let Some((row_artist, row_album)) = session
                    .rows
                    .iter()
                    .find(|row| row.stable_id == source_id)
                    .map(|row| (row.artist.clone(), row.album.clone()))
                else {
                    return Err("Unknown Last.fm import source row.".into());
                };
                let Some(candidate) = session
                    .matches
                    .get(source_id)
                    .and_then(|result| {
                        result
                            .candidates
                            .iter()
                            .find(|candidate| candidate.uri == uri)
                    })
                    .cloned()
                else {
                    return Err("This source row has no Spotify candidates.".into());
                };
                if candidate.relation.is_none() && !candidate.uri.starts_with("spotify:track:") {
                    return Err(
                        "That Spotify match is not supported by the source track set.".into(),
                    );
                }
                if candidate.uri.starts_with("spotify:album:") {
                    let related = session
                        .rows
                        .iter()
                        .filter(|row| row.artist == row_artist && row.album == row_album)
                        .map(|row| (row.stable_id.clone(), row.track.clone()))
                        .collect::<Vec<_>>();
                    for (id, track) in related {
                        let Some(result) = session.matches.get_mut(&id) else {
                            continue;
                        };
                        let Some(candidate) = result
                            .candidates
                            .iter()
                            .find(|candidate| candidate.uri == uri)
                            .cloned()
                        else {
                            continue;
                        };
                        update_selected_match(result, &id, &track, &candidate);
                    }
                } else {
                    let row_track = session
                        .rows
                        .iter()
                        .find(|row| row.stable_id == source_id)
                        .map(|row| row.track.clone())
                        .ok_or_else(|| "Unknown Last.fm import source row.".to_string())?;
                    if let Some(result) = session.matches.get_mut(source_id) {
                        update_selected_match(result, source_id, &row_track, &candidate);
                    }
                }
                Ok((session, ()))
            },
        )
        .await
    }

    fn claim_runner(&self) -> bool {
        !self.running.swap(true, Ordering::AcqRel)
    }

    fn release_runner(&self) {
        self.running.store(false, Ordering::Release);
    }
}

fn state_view(session: Option<&LastFmImportSessionV1>) -> ImportStateView {
    state_view_with_identity(session, None)
}

fn state_view_with_identity(
    session: Option<&LastFmImportSessionV1>,
    identity: Option<&(String, String)>,
) -> ImportStateView {
    ImportStateView {
        phase: session.map(|session| session.phase),
        username: session
            .map(|session| session.lastfm_username.clone())
            .or_else(|| identity.map(|(username, _)| username.clone())),
        spotify_account_id: session
            .map(|session| session.spotify_account_id.clone())
            .or_else(|| identity.map(|(_, account_id)| account_id.clone())),
        next_page: session.map(|session| session.next_page).unwrap_or(1),
        total_pages: session.and_then(|session| session.total_pages),
        total_scrobbles: session
            .map(|session| session.total_scrobbles)
            .unwrap_or_default(),
        included_scrobbles: session
            .map(|session| session.included_scrobbles)
            .unwrap_or_default(),
        matched_rows: session
            .map(|session| session.matches.len())
            .unwrap_or_default(),
        match_total: session
            .map(|session| session.rows.len())
            .unwrap_or_default(),
        defaults: session
            .map(|session| session.defaults.clone())
            .unwrap_or_default(),
        remaining: session
            .filter(|session| matches!(session.phase, ImportPhase::Review | ImportPhase::Done))
            .map(LastFmImportSessionV1::remaining)
            .unwrap_or_default(),
        retryable_error: session.and_then(|session| session.retryable_error.clone()),
        search_terms: session.map(|session| session.search_terms).unwrap_or(true),
    }
}

fn suspended_state_view() -> ImportStateView {
    ImportStateView {
        phase: Some(ImportPhase::Suspended),
        username: None,
        spotify_account_id: None,
        next_page: 1,
        total_pages: None,
        total_scrobbles: 0,
        included_scrobbles: 0,
        matched_rows: 0,
        match_total: 0,
        defaults: ImportDefaults::default(),
        remaining: 0,
        retryable_error: Some(RetryableError {
            message: "This import is suspended because the connected account changed. Reconnect Last.fm and Spotify to resume.".into(),
            attempt: 0,
            retryable: false,
        }),
        search_terms: true,
    }
}

async fn connected_accounts(app: &tauri::AppHandle) -> Result<(String, String), String> {
    let state = app.state::<crate::AppState>();
    let _membership_guard = state.spotify_library_gate.lock().await;
    connected_accounts_locked(&state).await
}

async fn connected_accounts_locked(state: &crate::AppState) -> Result<(String, String), String> {
    if !crate::stored_connection_state(&state.token_store)?.connected {
        return Err("Connect Spotify before importing its library.".into());
    }
    let username = state
        .lastfm
        .state()
        .await
        .username
        .ok_or_else(|| "Connect Last.fm before importing its history.".to_string())?;
    let cached_library = state
        .spotify_library
        .lock()
        .expect("Spotify library mutex poisoned")
        .clone();
    let spotify_account_id = if cached_library.is_exact() {
        cached_library.account_id
    } else {
        let provider = crate::provider_from(state)?;
        provider
            .me()
            .await
            .map_err(|error| format!("Could not identify the connected Spotify account: {error}"))?
            .id
    };
    Ok((username, spotify_account_id))
}

async fn assert_current_account(
    app: &tauri::AppHandle,
    service: &Service,
) -> Result<(String, String), String> {
    let state = app.state::<crate::AppState>();
    let _membership_guard = state.spotify_library_gate.lock().await;
    assert_current_account_locked(&state, service).await
}

async fn assert_current_account_locked(
    state: &crate::AppState,
    service: &Service,
) -> Result<(String, String), String> {
    let (username, spotify_account_id) = connected_accounts_locked(state).await?;
    let Some(session) = service.snapshot().await else {
        return Err("No Last.fm import session is active.".into());
    };
    if session.lastfm_username != username || session.spotify_account_id != spotify_account_id {
        service.suspend_for_account_mismatch().await?;
        return Err(
            "The saved Last.fm import belongs to a different account; it is suspended for safety."
                .into(),
        );
    }
    if session.phase == ImportPhase::Suspended {
        return Err("The Last.fm import is suspended for account safety.".into());
    }
    Ok((username, spotify_account_id))
}

async fn ensure_import_readable(app: &tauri::AppHandle, service: &Service) -> Result<bool, String> {
    let Some(session) = service.snapshot().await else {
        return Ok(true);
    };
    if session.phase == ImportPhase::Suspended {
        return Ok(false);
    }
    let state = app.state::<crate::AppState>();
    let current = {
        let _membership_guard = state.spotify_library_gate.lock().await;
        connected_accounts_locked(&state).await
    };
    match current {
        Ok((username, spotify_account_id))
            if username == session.lastfm_username
                && spotify_account_id == session.spotify_account_id =>
        {
            Ok(true)
        }
        Ok(_) | Err(_) => {
            service.suspend_for_account_mismatch().await?;
            Ok(false)
        }
    }
}

async fn emit_import_changed(
    app: &tauri::AppHandle,
    service: &Service,
) -> Result<ImportStateView, String> {
    let view = service.state().await;
    app.emit("lastfm-import-changed", &view)
        .map_err(|error| error.to_string())?;
    Ok(view)
}

async fn start_import(
    app: tauri::AppHandle,
    defaults: Option<ImportDefaults>,
) -> Result<ImportStateView, String> {
    let (username, spotify_account_id) = connected_accounts(&app).await?;
    let state = app.state::<crate::AppState>();
    let service = Arc::clone(&state.lastfm_import);
    let view = service
        .start_or_resume(&username, &spotify_account_id, crate::unix_now(), defaults)
        .await?;
    app.emit("lastfm-import-changed", &view)
        .map_err(|error| error.to_string())?;
    if service.claim_runner() {
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            run_import(app, service, username, spotify_account_id).await;
        });
    }
    Ok(view)
}

async fn run_import(
    app: tauri::AppHandle,
    service: Arc<Service>,
    username: String,
    spotify_account_id: String,
) {
    let result = async {
        loop {
            let Some(session) = service.snapshot().await else {
                break;
            };
            match session.phase {
                ImportPhase::Downloading => {
                    let lastfm = Arc::clone(&app.state::<crate::AppState>().lastfm);
                    let payload = match lastfm
                        .import_recent_tracks_page(
                            &username,
                            session.next_page,
                            session.snapshot_to,
                        )
                        .await
                    {
                        Ok(payload) => payload,
                        Err(error) => {
                            if error.account_mismatch {
                                service.suspend_for_account_mismatch().await?;
                                return Err(error.message);
                            }
                            let attempt = service
                                .snapshot()
                                .await
                                .and_then(|session| session.retryable_error)
                                .map(|error| error.attempt.saturating_add(1))
                                .unwrap_or(1);
                            service
                                .set_retryable_error(RetryableError {
                                    message: error.message.clone(),
                                    attempt: if error.retryable { attempt } else { 0 },
                                    retryable: error.retryable,
                                })
                                .await?;
                            return Err(error.message);
                        }
                    };
                    let parsed = match parse_recent_tracks_page(&payload) {
                        Ok(parsed) => parsed,
                        Err(message) => {
                            let attempt = service
                                .snapshot()
                                .await
                                .and_then(|session| session.retryable_error)
                                .map(|error| error.attempt.saturating_add(1))
                                .unwrap_or(1);
                            service
                                .set_retryable_error(RetryableError {
                                    message: message.clone(),
                                    attempt,
                                    retryable: false,
                                })
                                .await?;
                            return Err(message);
                        }
                    };
                    service.checkpoint_page(session.next_page, &parsed).await?;
                    let _ = app.emit("lastfm-import-changed", service.state().await);
                    if parsed.tracks.len() == LASTFM_PAGE_LIMIT as usize {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                }
                ImportPhase::Matching => {
                    run_matching(&app, &service, &username, &spotify_account_id).await?;
                }
                ImportPhase::Review | ImportPhase::Done | ImportPhase::Suspended => break,
            }
        }
        Ok::<(), String>(())
    }
    .await;
    if let Err(error) = result {
        let already_recorded = service
            .snapshot()
            .await
            .and_then(|session| session.retryable_error)
            .is_some();
        if !already_recorded {
            let _ = service
                .set_retryable_error(RetryableError {
                    message: error,
                    attempt: 0,
                    retryable: true,
                })
                .await;
        }
    }
    service.release_runner();
    let _ = app.emit("lastfm-import-changed", service.state().await);
}

fn album_search_term(artist: &str, album: &str) -> String {
    let artist = artist.replace('"', " ");
    let album = album.replace('"', " ");
    format!("album:\"{album}\" artist:\"{artist}\"")
}

fn track_search_term(artist: &str, track: &str) -> String {
    let artist = artist.replace('"', " ");
    let track = track.replace('"', " ");
    format!("track:\"{track}\" artist:\"{artist}\"")
}

pub(crate) fn classify_album_candidates_by_name(
    source_track_names: &[String],
    candidates: &mut [AlbumCandidate],
) {
    let source = source_track_names
        .iter()
        .map(|name| normalize_for_match(name))
        .collect::<BTreeSet<_>>();
    for candidate in candidates.iter_mut() {
        let target = candidate
            .track_names
            .iter()
            .map(|name| normalize_for_match(name))
            .collect::<BTreeSet<_>>();
        let overlap = source.intersection(&target).count();
        candidate.relation = if overlap == source.len() && target.len() == source.len() {
            Some(AlbumRelation::BestMatch)
        } else if overlap == source.len() && target.len() > source.len() {
            Some(AlbumRelation::Superset)
        } else if overlap > 0 && overlap * 2 >= source.len().max(1) {
            Some(AlbumRelation::SameSongs)
        } else {
            None
        };
    }
}

fn candidate_rank(relation: Option<AlbumRelation>) -> u8 {
    match relation {
        Some(AlbumRelation::BestMatch) => 0,
        Some(AlbumRelation::SameSongs) => 1,
        Some(AlbumRelation::Superset) => 2,
        None => 3,
    }
}

async fn checkpoint_matching(
    app: &tauri::AppHandle,
    service: &Service,
    username: &str,
    spotify_account_id: &str,
    results: Vec<MatchResult>,
) -> Result<(), String> {
    let (current_username, current_account_id) = assert_current_account(app, service).await?;
    if current_username != username || current_account_id != spotify_account_id {
        return Err("The connected account changed during matching.".into());
    }
    service
        .set_matches_during_matching(username, spotify_account_id, results)
        .await
}

async fn run_matching(
    app: &tauri::AppHandle,
    service: &Service,
    username: &str,
    spotify_account_id: &str,
) -> Result<(), String> {
    let state = app.state::<crate::AppState>();
    let (current_username, current_account_id) = assert_current_account(app, service).await?;
    if current_username != username || current_account_id != spotify_account_id {
        return Err("The connected Spotify account changed during matching.".into());
    }
    let Some(session) = service.snapshot().await else {
        return Ok(());
    };
    if session.spotify_account_id != spotify_account_id || session.phase != ImportPhase::Matching {
        return Ok(());
    }
    let provider = crate::provider_from(&state)?;
    let mut groups = BTreeMap::<(String, String), Vec<SourceRow>>::new();
    for row in session.rows {
        if !session.matches.contains_key(&row.stable_id) {
            groups
                .entry((row.artist.clone(), row.album.clone()))
                .or_default()
                .push(row);
        }
    }
    for ((artist, album), rows) in groups {
        if album.is_empty() {
            for row in rows {
                let search_term = track_search_term(&artist, &row.track);
                let results =
                    crate::provider::search_tracks(provider.as_ref(), &search_term).await?;
                let mut candidates = results
                    .items
                    .into_iter()
                    .map(|track| AlbumCandidate {
                        uri: track.uri.clone(),
                        name: track.name.clone(),
                        artist: track.artist.clone(),
                        track_uris: vec![track.uri.clone()],
                        track_names: vec![track.name.clone()],
                        track_artists: vec![track.artist],
                        track_albums: vec![track.alb],
                        relation: None,
                    })
                    .collect::<Vec<_>>();
                classify_album_candidates_by_name(
                    std::slice::from_ref(&row.track),
                    &mut candidates,
                );
                let selected = candidates
                    .iter()
                    .min_by_key(|candidate| candidate_rank(candidate.relation));
                let confidence = selected.map(|candidate| match candidate.relation {
                    Some(AlbumRelation::BestMatch) => Confidence::Exact,
                    Some(AlbumRelation::SameSongs | AlbumRelation::Superset) => Confidence::Likely,
                    None => Confidence::Low,
                });
                let selected_uri = selected
                    .filter(|candidate| candidate.relation.is_some())
                    .map(|candidate| candidate.uri.clone());
                let mut track_matches = BTreeMap::new();
                if let Some(uri) = selected_uri.clone() {
                    track_matches.insert(row.stable_id.clone(), uri);
                }
                checkpoint_matching(
                    app,
                    service,
                    username,
                    spotify_account_id,
                    vec![MatchResult {
                        source_id: row.stable_id,
                        search_term,
                        confidence,
                        selected_uri,
                        candidates,
                        track_matches,
                    }],
                )
                .await?;
                let _ = app.emit("lastfm-import-changed", service.state().await);
            }
            continue;
        }
        let search_term = album_search_term(&artist, &album);
        let results = crate::provider::search_albums(provider.as_ref(), &search_term).await?;
        let mut candidates = Vec::new();
        for album_result in results.items.into_iter().take(10) {
            let tracks =
                crate::provider::album_tracks(provider.as_ref(), &album_result.uri).await?;
            candidates.push(AlbumCandidate {
                uri: album_result.uri,
                name: album_result.name,
                artist: album_result.artist,
                track_uris: tracks.iter().map(|track| track.uri.clone()).collect(),
                track_names: tracks.iter().map(|track| track.name.clone()).collect(),
                track_artists: tracks.iter().map(|track| track.art.clone()).collect(),
                track_albums: tracks.iter().map(|track| track.alb.clone()).collect(),
                relation: None,
            });
        }
        let source_track_names = rows.iter().map(|row| row.track.clone()).collect::<Vec<_>>();
        classify_album_candidates_by_name(&source_track_names, &mut candidates);
        let selected = candidates
            .iter()
            .min_by_key(|candidate| candidate_rank(candidate.relation));
        let confidence = selected.map(|candidate| match candidate.relation {
            Some(AlbumRelation::BestMatch) => Confidence::Exact,
            Some(AlbumRelation::SameSongs | AlbumRelation::Superset) => Confidence::Likely,
            None => Confidence::Low,
        });
        let selected_uri = selected
            .filter(|candidate| candidate.relation.is_some())
            .map(|candidate| candidate.uri.clone());
        let mut track_matches = BTreeMap::new();
        if let Some(selected) = selected.filter(|candidate| candidate.relation.is_some()) {
            for row in &rows {
                if let Some(index) = selected
                    .track_names
                    .iter()
                    .position(|name| normalize_for_match(name) == normalize_for_match(&row.track))
                {
                    if let Some(uri) = selected.track_uris.get(index) {
                        track_matches.insert(row.stable_id.clone(), uri.clone());
                    }
                }
            }
        }
        let matches = rows
            .into_iter()
            .map(|row| MatchResult {
                source_id: row.stable_id,
                search_term: search_term.clone(),
                confidence,
                selected_uri: selected_uri.clone(),
                candidates: candidates.clone(),
                track_matches: track_matches.clone(),
            })
            .collect();
        checkpoint_matching(app, service, username, spotify_account_id, matches).await?;
        let _ = app.emit("lastfm-import-changed", service.state().await);
    }
    let (final_username, final_account_id) = assert_current_account(app, service).await?;
    if final_username != username || final_account_id != spotify_account_id {
        return Err("The connected account changed before matching completed.".into());
    }
    service
        .finish_matching_if_current(username, spotify_account_id)
        .await
}

fn matched_track_uri(result: &MatchResult, source_id: &str) -> Option<String> {
    result.track_matches.get(source_id).cloned().or_else(|| {
        result
            .selected_uri
            .as_ref()
            .filter(|uri| uri.starts_with("spotify:track:"))
            .cloned()
    })
}

fn best_candidate(result: &MatchResult) -> Option<&AlbumCandidate> {
    result
        .candidates
        .iter()
        .filter(|candidate| {
            candidate.relation.is_some() || candidate.uri.starts_with("spotify:track:")
        })
        .min_by_key(|candidate| candidate_rank(candidate.relation))
}

fn matched_track_uri_for_row(result: &MatchResult, row: &SourceRow) -> Option<String> {
    matched_track_uri(result, &row.stable_id).or_else(|| {
        let candidate = best_candidate(result)?;
        if candidate.uri.starts_with("spotify:track:") {
            return Some(candidate.uri.clone());
        }
        let index = candidate
            .track_names
            .iter()
            .position(|name| normalize_for_match(name) == normalize_for_match(&row.track))?;
        candidate.track_uris.get(index).cloned()
    })
}

fn membership_uris_for_import(
    import_content: bool,
    whole_album: bool,
    album_uri: Option<&str>,
    track_uris: &[String],
) -> Option<Vec<String>> {
    if !import_content {
        return None;
    }
    if whole_album {
        return album_uri
            .filter(|uri| uri.starts_with("spotify:album:"))
            .map(|uri| vec![uri.to_owned()]);
    }
    let mut seen = BTreeSet::new();
    Some(
        track_uris
            .iter()
            .filter(|uri| uri.starts_with("spotify:track:") && seen.insert((*uri).clone()))
            .cloned()
            .collect(),
    )
}

fn committed_source_ids(
    rows: &[SourceRow],
    target_by_source: &BTreeMap<String, String>,
    import_content: bool,
    whole_album: bool,
    include_historical_play_counts: bool,
) -> Vec<String> {
    if import_content && whole_album && !include_historical_play_counts {
        return rows.iter().map(|row| row.stable_id.clone()).collect();
    }
    rows.iter()
        .filter(|row| target_by_source.contains_key(&row.stable_id))
        .map(|row| row.stable_id.clone())
        .collect()
}

fn historical_count_for_target(
    session: &LastFmImportSessionV1,
    target_uri: &str,
    current_rows: &[&SourceRow],
    current_options: &PageOptions,
) -> u64 {
    let current_ids = current_rows
        .iter()
        .map(|row| row.stable_id.as_str())
        .collect::<BTreeSet<_>>();
    let mut relevant = Vec::new();
    for row in &session.rows {
        let decision = default_decision(session, &row.stable_id);
        let current_page = current_ids.contains(row.stable_id.as_str());
        let included = current_page || (decision.status == RowStatus::Done && !decision.excluded);
        let page_options = if current_page {
            current_options.clone()
        } else {
            session.options_for(&row.artist, &row.album)
        };
        if included
            && page_options.include_historical_play_counts
            && session
                .matches
                .get(&row.stable_id)
                .and_then(|result| matched_track_uri(result, &row.stable_id))
                .as_deref()
                == Some(target_uri)
        {
            relevant.push(row);
        }
    }
    resolved_play_count(
        &relevant,
        session
            .count_modes
            .get(target_uri)
            .copied()
            .unwrap_or(CountMode::Sum),
    )
}

fn update_selected_match(
    result: &mut MatchResult,
    source_id: &str,
    source_track: &str,
    candidate: &AlbumCandidate,
) {
    result.selected_uri = Some(candidate.uri.clone());
    result.confidence = Some(match candidate.relation {
        Some(AlbumRelation::BestMatch) => Confidence::Exact,
        Some(AlbumRelation::SameSongs | AlbumRelation::Superset) => Confidence::Likely,
        None => Confidence::Low,
    });
    result.track_matches.remove(source_id);
    if let Some(index) = candidate
        .track_names
        .iter()
        .position(|name| normalize_for_match(name) == normalize_for_match(source_track))
    {
        if let Some(track_uri) = candidate.track_uris.get(index) {
            result
                .track_matches
                .insert(source_id.to_owned(), track_uri.clone());
        }
    } else if candidate.uri.starts_with("spotify:track:") {
        result
            .track_matches
            .insert(source_id.to_owned(), candidate.uri.clone());
    }
}

async fn apply_page(
    app: &tauri::AppHandle,
    service: &Service,
    artist: &str,
    album: &str,
    selected_ids: &[String],
    options: PageOptions,
) -> Result<ImportStateView, String> {
    options.validate()?;
    let state = app.state::<crate::AppState>();
    let _membership_guard = state.spotify_library_gate.lock().await;
    let (username, spotify_account_id) = assert_current_account_locked(&state, service).await?;
    let Some(session) = service.snapshot().await else {
        return Err("No Last.fm import session is active.".into());
    };
    let selected = selected_ids.iter().cloned().collect::<BTreeSet<_>>();
    let rows = session
        .rows
        .iter()
        .filter(|row| row.artist == artist && row.album == album)
        .filter(|row| selected.contains(&row.stable_id))
        .filter(|row| {
            let decision = default_decision(&session, &row.stable_id);
            !decision.excluded && matches!(decision.status, RowStatus::Pending | RowStatus::Skipped)
        })
        .cloned()
        .collect::<Vec<_>>();
    if rows.is_empty() {
        return Ok(state_view(Some(&session)));
    }

    let mut target_by_source = BTreeMap::<String, String>::new();
    for row in &rows {
        if let Some(result) = session.matches.get(&row.stable_id) {
            if let Some(uri) = matched_track_uri(result, &row.stable_id) {
                target_by_source.insert(row.stable_id.clone(), uri);
            }
        }
    }
    let mut metadata_uris = target_by_source.values().cloned().collect::<BTreeSet<_>>();
    if options.import_content && options.whole_album {
        let album_uri = rows
            .iter()
            .filter_map(|row| session.matches.get(&row.stable_id))
            .filter_map(|result| result.selected_uri.as_deref())
            .find(|uri| uri.starts_with("spotify:album:"))
            .ok_or_else(|| {
                "Choose a supported Spotify album match before accepting.".to_string()
            })?;
        let album_uri = membership_uris_for_import(true, true, Some(album_uri), &[])
            .and_then(|uris| uris.into_iter().next())
            .ok_or_else(|| "Expected a Spotify album URI for the import.".to_string())?;
        let provider = crate::provider_from(&state)?;
        let saved = crate::spotify_commands::save_album_operation(
            &state,
            provider.as_ref(),
            &album_uri,
            album,
            artist,
            crate::unix_now(),
        )
        .await?;
        if saved.album_uri != album_uri {
            return Err("Spotify returned a different album than the selected match.".into());
        }
        metadata_uris = saved.track_uris.iter().cloned().collect();
    } else if options.import_content {
        let requested = target_by_source.values().cloned().collect::<BTreeSet<_>>();
        if !requested.is_empty() {
            let requested = membership_uris_for_import(
                true,
                false,
                None,
                &requested.iter().cloned().collect::<Vec<_>>(),
            )
            .unwrap_or_default();
            let provider = crate::provider_from(&state)?;
            crate::spotify_commands::save_tracks_operation(
                &state,
                provider.as_ref(),
                requested,
                crate::unix_now(),
            )
            .await?;
        }
    }

    let mut by_target = BTreeMap::<String, Vec<&SourceRow>>::new();
    for row in &rows {
        if let Some(uri) = target_by_source.get(&row.stable_id) {
            by_target.entry(uri.clone()).or_default().push(row);
        }
    }
    let updates = by_target
        .iter()
        .map(|(uri, rows)| {
            let refs = rows.to_vec();
            let (earliest, latest) = resolved_timestamps(&refs).unwrap_or_default();
            HistoryUpdate {
                uri: uri.clone(),
                play_count: options
                    .include_historical_play_counts
                    .then(|| historical_count_for_target(&session, uri, rows, &options)),
                earliest: (earliest > 0).then_some(earliest),
                latest: options.include_historical_play_counts.then_some(latest),
            }
        })
        .collect::<Vec<_>>();
    let _ = assert_current_account_locked(&state, service).await?;
    if !updates.is_empty() || !metadata_uris.is_empty() {
        crate::mutate_library(&state, |library| {
            apply_history_updates(library, &updates);
            apply_metadata(
                library,
                &metadata_uris.iter().cloned().collect::<Vec<_>>(),
                options.whole_album,
                options.genre.as_deref(),
                options.rating,
            )
        })?;
    }
    let committed = committed_source_ids(
        &rows,
        &target_by_source,
        options.import_content,
        options.whole_album,
        options.include_historical_play_counts,
    );
    service
        .commit_rows(
            &username,
            &spotify_account_id,
            &committed,
            artist,
            album,
            options,
        )
        .await
}

async fn album_candidates<
    T: retune_spotify::client::Transport,
    S: retune_spotify::tokens::TokenStore,
>(
    provider: &retune_spotify::client::SpotifyClient<T, S>,
    query: &str,
    source_track_names: &[String],
) -> Result<Vec<AlbumCandidate>, String> {
    let results = crate::provider::search_albums(provider, query).await?;
    let mut candidates = Vec::new();
    for album in results.items.into_iter().take(10) {
        let tracks = crate::provider::album_tracks(provider, &album.uri).await?;
        candidates.push(AlbumCandidate {
            uri: album.uri,
            name: album.name,
            artist: album.artist,
            track_uris: tracks.iter().map(|track| track.uri.clone()).collect(),
            track_names: tracks.iter().map(|track| track.name.clone()).collect(),
            track_artists: tracks.iter().map(|track| track.art.clone()).collect(),
            track_albums: tracks.iter().map(|track| track.alb.clone()).collect(),
            relation: None,
        });
    }
    classify_album_candidates_by_name(source_track_names, &mut candidates);
    Ok(candidates)
}

fn match_result_for(
    source_id: String,
    search_term: String,
    mut candidates: Vec<AlbumCandidate>,
    source_track: &str,
    auto_select: bool,
) -> MatchResult {
    let selected = auto_select
        .then(|| {
            candidates
                .iter()
                .min_by_key(|candidate| candidate_rank(candidate.relation))
        })
        .flatten();
    let confidence = selected.map(|candidate| match candidate.relation {
        Some(AlbumRelation::BestMatch) => Confidence::Exact,
        Some(AlbumRelation::SameSongs | AlbumRelation::Superset) => Confidence::Likely,
        None => Confidence::Low,
    });
    let selected_uri = selected
        .filter(|candidate| candidate.relation.is_some())
        .map(|candidate| candidate.uri.clone());
    let mut track_matches = BTreeMap::new();
    if let Some(selected) = selected.filter(|candidate| candidate.relation.is_some()) {
        if let Some(index) = selected
            .track_names
            .iter()
            .position(|name| normalize_for_match(name) == normalize_for_match(source_track))
        {
            if let Some(uri) = selected.track_uris.get(index) {
                track_matches.insert(source_id.clone(), uri.clone());
            }
        } else if selected.uri.starts_with("spotify:track:") {
            track_matches.insert(source_id.clone(), selected.uri.clone());
        }
    }
    // Keep the list bounded even if a future provider adapter returns more.
    candidates.truncate(10);
    MatchResult {
        source_id,
        search_term,
        confidence,
        selected_uri,
        candidates,
        track_matches,
    }
}

#[tauri::command]
pub(crate) async fn open_lastfm_importer(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("lastfm-importer") {
        window.show().map_err(|error| error.to_string())?;
        window.set_focus().map_err(|error| error.to_string())?;
        return Ok(());
    }
    WebviewWindowBuilder::new(
        &app,
        "lastfm-importer",
        WebviewUrl::App("index.html".into()),
    )
    .title("Last.fm importer")
    .inner_size(1320.0, 840.0)
    .resizable(true)
    .build()
    .map(|_| ())
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) async fn lastfm_import_state(app: tauri::AppHandle) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let service = &state.lastfm_import;
    let identity = if service.snapshot().await.is_none() {
        let _membership_guard = state.spotify_library_gate.lock().await;
        connected_accounts_locked(&state).await.ok()
    } else {
        let _ = ensure_import_readable(&app, service.as_ref()).await?;
        None
    };
    Ok(service.state_with_identity(identity).await)
}

#[tauri::command]
pub(crate) async fn lastfm_import_queue(
    app: tauri::AppHandle,
) -> Result<Vec<ImportQueueItem>, String> {
    let service = &app.state::<crate::AppState>().lastfm_import;
    if !ensure_import_readable(&app, service.as_ref()).await? {
        return Ok(Vec::new());
    }
    Ok(service.queue().await)
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_page(
    app: tauri::AppHandle,
    artist: String,
    album: String,
) -> Result<Option<ImportPageView>, String> {
    let service = &app.state::<crate::AppState>().lastfm_import;
    if !ensure_import_readable(&app, service.as_ref()).await? {
        return Ok(None);
    }
    Ok(service.page(&artist, &album).await)
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_review(
    app: tauri::AppHandle,
    id: String,
    action: String,
    artist: String,
    album: String,
) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let (username, spotify_account_id) =
        assert_current_account(&app, state.lastfm_import.as_ref()).await?;
    state
        .lastfm_import
        .review_action(
            &username,
            &spotify_account_id,
            &id,
            &action,
            &artist,
            &album,
        )
        .await?;
    emit_import_changed(&app, state.lastfm_import.as_ref()).await
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_options(
    app: tauri::AppHandle,
    artist: String,
    album: String,
    options: PageOptions,
) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let (username, spotify_account_id) =
        assert_current_account(&app, state.lastfm_import.as_ref()).await?;
    state
        .lastfm_import
        .update_options(&username, &spotify_account_id, &artist, &album, options)
        .await?;
    emit_import_changed(&app, state.lastfm_import.as_ref()).await
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_count_mode(
    app: tauri::AppHandle,
    target_uri: String,
    mode: CountMode,
) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let (username, spotify_account_id) =
        assert_current_account(&app, state.lastfm_import.as_ref()).await?;
    state
        .lastfm_import
        .set_count_mode(&username, &spotify_account_id, &target_uri, mode)
        .await?;
    emit_import_changed(&app, state.lastfm_import.as_ref()).await
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_search_terms(
    app: tauri::AppHandle,
    show: bool,
) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let (username, spotify_account_id) =
        assert_current_account(&app, state.lastfm_import.as_ref()).await?;
    state
        .lastfm_import
        .set_search_terms(&username, &spotify_account_id, show)
        .await?;
    emit_import_changed(&app, state.lastfm_import.as_ref()).await
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_select_match(
    app: tauri::AppHandle,
    id: String,
    uri: String,
) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let (username, spotify_account_id) =
        assert_current_account(&app, state.lastfm_import.as_ref()).await?;
    state
        .lastfm_import
        .select_match(&username, &spotify_account_id, &id, &uri)
        .await?;
    emit_import_changed(&app, state.lastfm_import.as_ref()).await
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_change_track(
    app: tauri::AppHandle,
    id: String,
    query: String,
) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let (username, spotify_account_id) =
        assert_current_account(&app, state.lastfm_import.as_ref()).await?;
    let session = state
        .lastfm_import
        .snapshot()
        .await
        .ok_or_else(|| "No Last.fm import session is active.".to_string())?;
    let row = session
        .rows
        .iter()
        .find(|row| row.stable_id == id)
        .ok_or_else(|| "Unknown Last.fm import source row.".to_string())?;
    let search_term = if query.trim().is_empty() {
        track_search_term(&row.artist, &row.track)
    } else {
        query.trim().to_owned()
    };
    let provider = crate::provider_from(&state)?;
    let results = crate::provider::search_tracks(provider.as_ref(), &search_term).await?;
    let candidates = results
        .items
        .into_iter()
        .map(|track| AlbumCandidate {
            uri: track.uri.clone(),
            name: track.name.clone(),
            artist: track.artist.clone(),
            track_uris: vec![track.uri.clone()],
            track_names: vec![track.name.clone()],
            track_artists: vec![track.artist],
            track_albums: vec![track.alb],
            relation: None,
        })
        .collect::<Vec<_>>();
    let mut candidates = candidates;
    classify_album_candidates_by_name(std::slice::from_ref(&row.track), &mut candidates);
    let _ = assert_current_account(&app, state.lastfm_import.as_ref()).await?;
    state
        .lastfm_import
        .set_match(
            &username,
            &spotify_account_id,
            match_result_for(id, search_term, candidates, &row.track, false),
        )
        .await?;
    emit_import_changed(&app, state.lastfm_import.as_ref()).await
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_change_album(
    app: tauri::AppHandle,
    id: String,
    query: String,
) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let (username, spotify_account_id) =
        assert_current_account(&app, state.lastfm_import.as_ref()).await?;
    let session = state
        .lastfm_import
        .snapshot()
        .await
        .ok_or_else(|| "No Last.fm import session is active.".to_string())?;
    let row = session
        .rows
        .iter()
        .find(|row| row.stable_id == id)
        .ok_or_else(|| "Unknown Last.fm import source row.".to_string())?;
    let related = session
        .rows
        .iter()
        .filter(|candidate| candidate.artist == row.artist && candidate.album == row.album)
        .map(|candidate| candidate.track.clone())
        .collect::<Vec<_>>();
    let search_term = if query.trim().is_empty() {
        album_search_term(&row.artist, &row.album)
    } else {
        query.trim().to_owned()
    };
    let provider = crate::provider_from(&state)?;
    let candidates = album_candidates(provider.as_ref(), &search_term, &related).await?;
    let matches = session
        .rows
        .iter()
        .filter(|candidate| candidate.artist == row.artist && candidate.album == row.album)
        .map(|candidate_row| {
            match_result_for(
                candidate_row.stable_id.clone(),
                search_term.clone(),
                candidates.clone(),
                &candidate_row.track,
                false,
            )
        })
        .collect();
    let _ = assert_current_account(&app, state.lastfm_import.as_ref()).await?;
    state
        .lastfm_import
        .set_matches(&username, &spotify_account_id, matches)
        .await?;
    emit_import_changed(&app, state.lastfm_import.as_ref()).await
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_apply(
    app: tauri::AppHandle,
    artist: String,
    album: String,
    selected_ids: Vec<String>,
    options: PageOptions,
) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let view = apply_page(
        &app,
        state.lastfm_import.as_ref(),
        &artist,
        &album,
        &selected_ids,
        options,
    )
    .await?;
    app.emit("lastfm-import-changed", &view)
        .map_err(|error| error.to_string())?;
    Ok(view)
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_accept_all_page(
    app: tauri::AppHandle,
    artist: String,
    album: String,
) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let (username, spotify_account_id) =
        assert_current_account(&app, state.lastfm_import.as_ref()).await?;
    let Some(page) = state.lastfm_import.page(&artist, &album).await else {
        return Ok(state.lastfm_import.state().await);
    };
    let mut selected_album_uris = BTreeSet::new();
    for item in &page.rows {
        if !page
            .options
            .selected_track_ids
            .contains(&item.source.stable_id)
            || item.decision.excluded
            || !matches!(
                item.decision.status,
                RowStatus::Pending | RowStatus::Skipped
            )
        {
            continue;
        }
        let Some(result) = &item.match_result else {
            continue;
        };
        if result.selected_uri.is_some() {
            continue;
        }
        let Some(candidate) = best_candidate(result) else {
            continue;
        };
        if candidate.uri.starts_with("spotify:album:") {
            if selected_album_uris.insert(candidate.uri.clone()) {
                state
                    .lastfm_import
                    .select_match(
                        &username,
                        &spotify_account_id,
                        &item.source.stable_id,
                        &candidate.uri,
                    )
                    .await?;
            }
        } else {
            state
                .lastfm_import
                .select_match(
                    &username,
                    &spotify_account_id,
                    &item.source.stable_id,
                    &candidate.uri,
                )
                .await?;
        }
    }
    let Some(page) = state.lastfm_import.page(&artist, &album).await else {
        return Ok(state.lastfm_import.state().await);
    };
    let selected_ids = page
        .options
        .selected_track_ids
        .iter()
        .filter(|id| {
            page.rows.iter().any(|item| {
                &item.source.stable_id == *id
                    && !item.decision.excluded
                    && matches!(
                        item.decision.status,
                        RowStatus::Pending | RowStatus::Skipped
                    )
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    let view = apply_page(
        &app,
        state.lastfm_import.as_ref(),
        &artist,
        &album,
        &selected_ids,
        page.options,
    )
    .await?;
    app.emit("lastfm-import-changed", &view)
        .map_err(|error| error.to_string())?;
    Ok(view)
}

#[tauri::command]
pub(crate) async fn start_lastfm_import(
    app: tauri::AppHandle,
    defaults: Option<ImportDefaults>,
) -> Result<ImportStateView, String> {
    start_import(app, defaults).await
}

pub(crate) fn default_decision(session: &LastFmImportSessionV1, id: &str) -> RowDecision {
    session.decisions.get(id).cloned().unwrap_or_default()
}

fn locked_count_modes(session: &LastFmImportSessionV1) -> BTreeSet<String> {
    session
        .rows
        .iter()
        .filter(|row| default_decision(session, &row.stable_id).status == RowStatus::Done)
        .filter_map(|row| {
            session
                .matches
                .get(&row.stable_id)
                .and_then(|result| matched_track_uri(result, &row.stable_id))
        })
        .collect()
}

fn queue_status(session: &LastFmImportSessionV1, rows: &[&SourceRow]) -> Option<QueueStatus> {
    if rows
        .iter()
        .all(|row| default_decision(session, &row.stable_id).excluded)
    {
        return Some(QueueStatus::Excluded);
    }
    let first = rows
        .first()
        .map(|row| default_decision(session, &row.stable_id).status)?;
    if first == RowStatus::Pending
        || !rows
            .iter()
            .all(|row| default_decision(session, &row.stable_id).status == first)
    {
        return None;
    }
    Some(match first {
        RowStatus::Done => QueueStatus::Done,
        RowStatus::Skipped => QueueStatus::Skipped,
        RowStatus::IgnoredAlbum => QueueStatus::IgnoredAlbum,
        RowStatus::IgnoredArtist => QueueStatus::IgnoredArtist,
        RowStatus::Pending => return None,
    })
}

fn update_review_phase(session: &mut LastFmImportSessionV1) {
    if session.remaining() == 0 {
        session.phase = ImportPhase::Done;
    } else if session.phase == ImportPhase::Done {
        session.phase = ImportPhase::Review;
    }
}

fn review_phase_allowed(phase: ImportPhase) -> bool {
    matches!(phase, ImportPhase::Review | ImportPhase::Done)
}

fn exclude_row(session: &mut LastFmImportSessionV1, id: &str, excluded: bool) {
    if is_reviewable(session, id) {
        let decision = session.decisions.entry(id.to_owned()).or_default();
        decision.excluded = excluded;
    }
}

fn is_reviewable(session: &LastFmImportSessionV1, id: &str) -> bool {
    matches!(
        default_decision(session, id).status,
        RowStatus::Pending | RowStatus::Skipped
    )
}

fn is_actionable(session: &LastFmImportSessionV1, id: &str) -> bool {
    is_reviewable(session, id) && !default_decision(session, id).excluded
}

pub(crate) fn ignore_album(session: &mut LastFmImportSessionV1, artist: &str, album: &str) {
    let ids = session
        .rows
        .iter()
        .filter(|row| {
            row.artist == artist && row.album == album && is_actionable(session, &row.stable_id)
        })
        .map(|row| row.stable_id.clone())
        .collect::<Vec<_>>();
    for id in ids {
        session.decisions.insert(
            id,
            RowDecision {
                status: RowStatus::IgnoredAlbum,
                excluded: false,
            },
        );
    }
}

pub(crate) fn ignore_artist(session: &mut LastFmImportSessionV1, artist: &str) {
    let ids = session
        .rows
        .iter()
        .filter(|row| row.artist == artist && is_actionable(session, &row.stable_id))
        .map(|row| row.stable_id.clone())
        .collect::<Vec<_>>();
    for id in ids {
        session.decisions.insert(
            id,
            RowDecision {
                status: RowStatus::IgnoredArtist,
                excluded: false,
            },
        );
    }
}

pub(crate) fn skip_album(session: &mut LastFmImportSessionV1, artist: &str, album: &str) {
    let ids = session
        .rows
        .iter()
        .filter(|row| {
            row.artist == artist && row.album == album && is_actionable(session, &row.stable_id)
        })
        .map(|row| row.stable_id.clone())
        .collect::<Vec<_>>();
    for id in ids {
        session.decisions.insert(
            id,
            RowDecision {
                status: RowStatus::Skipped,
                excluded: false,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{SavedAlbumRecord, SpotifyLibraryState};
    use retune_core::model::SourceId;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::{fs, time::Duration};

    fn response(entries: Value) -> Value {
        serde_json::json!({
            "recenttracks": {
                "track": entries,
                "@attr": {"page": "2", "totalPages": "4", "total": "601"}
            }
        })
    }

    fn scrobble(artist: &str, album: &str, track: &str, timestamp: u64) -> ParsedScrobble {
        ParsedScrobble {
            artist: artist.into(),
            album: album.into(),
            track: track.into(),
            timestamp,
        }
    }

    #[test]
    fn parses_nowplaying_and_undated_rows_without_retaining_them() {
        let parsed = parse_recent_tracks_page(&response(serde_json::json!([
            {"artist": {"#text": "Artist"}, "album": {"#text": "Album"}, "name": "Song", "date": {"uts": "20"}},
            {"artist": {"#text": "Live"}, "name": "Now", "@attr": {"nowplaying": "1"}},
            {"artist": {"#text": "Live"}, "name": "Now too", "@attr": {"nowplaying": true}},
            {"artist": {"#text": "Old"}, "name": "Missing date"},
        ]))).unwrap();

        assert_eq!(parsed.page, 2);
        assert_eq!(parsed.total_pages, Some(4));
        assert_eq!(parsed.total, Some(601));
        assert_eq!(parsed.tracks.len(), 1);
        assert_eq!(parsed.skipped_now_playing, 2);
        assert_eq!(parsed.skipped_undated, 1);
        assert_eq!(parsed.tracks[0].timestamp, 20);
    }

    #[test]
    fn parses_a_single_track_object_and_text_fields() {
        let parsed = parse_recent_tracks_page(&response(serde_json::json!({
            "artist": "Artist", "album": "Album", "name": "Song", "date": {"uts": "9"}
        })))
        .unwrap();
        assert_eq!(parsed.tracks, vec![scrobble("Artist", "Album", "Song", 9)]);
    }

    #[test]
    fn aggregation_keeps_compact_raw_variants_and_timestamps() {
        let mut rows = Vec::new();
        aggregate_scrobbles(
            &mut rows,
            &[
                scrobble("Beyoncé", "Lemonade", "Sorry", 300),
                scrobble("Beyoncé", "Lemonade", "Sorry!", 100),
                scrobble("Beyoncé", "Lemonade", "Sorry", 200),
            ],
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].play_count, 3);
        assert_eq!((rows[0].earliest, rows[0].latest), (100, 300));
        assert_eq!(rows[0].variants.len(), 2);
        assert_eq!(resolved_play_count(&[&rows[0]], CountMode::Sum), 3);
        assert_eq!(resolved_play_count(&[&rows[0]], CountMode::Overwrite), 2);
        assert_eq!(resolved_play_count(&[&rows[0]], CountMode::Zero), 0);
    }

    #[test]
    fn setup_state_view_reports_identity_and_review_only_remaining() {
        let identity = ("rianjs".to_owned(), "spotify-user".to_owned());
        let setup = state_view_with_identity(None, Some(&identity));
        assert_eq!(setup.phase, None);
        assert_eq!(setup.username.as_deref(), Some("rianjs"));
        assert_eq!(setup.spotify_account_id.as_deref(), Some("spotify-user"));
        assert_eq!(setup.remaining, 0);

        let mut session = LastFmImportSessionV1::new("rianjs".into(), "spotify-user".into(), 10);
        aggregate_scrobbles(
            &mut session.rows,
            &[scrobble("Artist", "Album", "Track", 10)],
        );
        assert_eq!(state_view(Some(&session)).remaining, 0);
        session.phase = ImportPhase::Review;
        assert_eq!(state_view(Some(&session)).remaining, 1);
    }

    #[test]
    fn fuzzy_arithmetic_combines_rows_mapped_to_one_target() {
        let mut rows = Vec::new();
        aggregate_scrobbles(
            &mut rows,
            &[
                scrobble("Artist", "Album", "Song", 10),
                scrobble("Artist", "Album", "Song", 11),
                scrobble("Artist", "Album", "Song (Live)", 12),
            ],
        );
        assert_eq!(rows.len(), 2);
        let refs = rows.iter().collect::<Vec<_>>();
        assert_eq!(resolved_play_count(&refs, CountMode::Sum), 3);
        assert_eq!(resolved_play_count(&refs, CountMode::Overwrite), 2);
        assert_eq!(resolved_timestamps(&refs), Some((10, 12)));
    }

    #[tokio::test]
    async fn page_fuzzy_groups_only_include_rows_selected_for_import() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        let mut session = LastFmImportSessionV1::new("user".into(), "spotify".into(), 10);
        aggregate_scrobbles(
            &mut session.rows,
            &[
                scrobble("Artist", "Done", "Track", 1),
                scrobble("Artist", "Selected", "Track", 2),
                scrobble("Artist", "Skipped", "Track", 3),
                scrobble("Artist", "Ignored", "Track", 4),
                scrobble("Artist", "Excluded", "Track", 5),
                scrobble("Artist", "Unchecked", "Track", 6),
            ],
        );
        let ids = session
            .rows
            .iter()
            .map(|row| (row.album.clone(), row.stable_id.clone()))
            .collect::<BTreeMap<_, _>>();
        let target = "spotify:track:target".to_owned();
        for row in &session.rows {
            session.matches.insert(
                row.stable_id.clone(),
                MatchResult {
                    source_id: row.stable_id.clone(),
                    search_term: row.track.clone(),
                    confidence: Some(Confidence::Exact),
                    selected_uri: Some(target.clone()),
                    candidates: Vec::new(),
                    track_matches: BTreeMap::from([(row.stable_id.clone(), target.clone())]),
                },
            );
        }
        session.decisions.insert(
            ids["Done"].clone(),
            RowDecision {
                status: RowStatus::Done,
                excluded: false,
            },
        );
        session.decisions.insert(
            ids["Skipped"].clone(),
            RowDecision {
                status: RowStatus::Skipped,
                excluded: false,
            },
        );
        session.decisions.insert(
            ids["Ignored"].clone(),
            RowDecision {
                status: RowStatus::IgnoredAlbum,
                excluded: false,
            },
        );
        session.decisions.insert(
            ids["Excluded"].clone(),
            RowDecision {
                status: RowStatus::Pending,
                excluded: true,
            },
        );
        for (album, selected) in [
            ("Selected", true),
            ("Skipped", true),
            ("Ignored", true),
            ("Excluded", true),
            ("Unchecked", false),
        ] {
            session.page_options.insert(
                format!("Artist\u{1f}{album}"),
                PageOptions {
                    selected_track_ids: if selected {
                        BTreeSet::from([ids[album].clone()])
                    } else {
                        BTreeSet::new()
                    },
                    ..PageOptions::default()
                },
            );
        }
        service.save(session).await.unwrap();

        let page = service.page("Artist", "Selected").await.unwrap();
        let included = page
            .fuzzy_groups
            .get(&target)
            .unwrap()
            .iter()
            .map(|row| row.album.clone())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            included,
            BTreeSet::from([
                "Done".to_owned(),
                "Selected".to_owned(),
                "Skipped".to_owned(),
            ])
        );
        assert!(page.locked_count_modes.contains(&target));
    }

    #[tokio::test]
    async fn count_mode_change_is_rejected_after_target_is_done() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        let mut session = LastFmImportSessionV1::new("user".into(), "spotify".into(), 10);
        aggregate_scrobbles(
            &mut session.rows,
            &[scrobble("Artist", "Album", "Track", 1)],
        );
        let source_id = session.rows[0].stable_id.clone();
        let target = "spotify:track:target";
        session.matches.insert(
            source_id.clone(),
            MatchResult {
                source_id: source_id.clone(),
                search_term: "Track".into(),
                confidence: Some(Confidence::Exact),
                selected_uri: Some(target.into()),
                candidates: Vec::new(),
                track_matches: BTreeMap::from([(source_id.clone(), target.into())]),
            },
        );
        session.decisions.insert(
            source_id,
            RowDecision {
                status: RowStatus::Done,
                excluded: false,
            },
        );
        session
            .count_modes
            .insert(target.into(), CountMode::Overwrite);
        session.phase = ImportPhase::Review;
        service.save(session).await.unwrap();

        assert!(service
            .set_count_mode("user", "spotify", target, CountMode::Overwrite)
            .await
            .is_ok());
        assert!(service
            .set_count_mode("user", "spotify", target, CountMode::Zero)
            .await
            .is_err());
        assert_eq!(
            service.snapshot().await.unwrap().count_modes.get(target),
            Some(&CountMode::Overwrite)
        );
    }

    #[test]
    fn target_count_mode_is_session_scoped_across_pages_and_persisted() {
        let mut session = LastFmImportSessionV1::new("user".into(), "spotify".into(), 10);
        aggregate_scrobbles(
            &mut session.rows,
            &[
                scrobble("Artist", "First", "Song", 10),
                scrobble("Artist", "First", "Song", 11),
                scrobble("Artist", "Second", "Song!", 12),
                scrobble("Artist", "Second", "Song!", 13),
            ],
        );
        let first = session.rows[0].stable_id.clone();
        let second = session.rows[1].stable_id.clone();
        for id in [&first, &second] {
            session.matches.insert(
                (*id).clone(),
                MatchResult {
                    source_id: (*id).clone(),
                    search_term: String::new(),
                    confidence: Some(Confidence::Exact),
                    selected_uri: Some("spotify:track:target".into()),
                    candidates: Vec::new(),
                    track_matches: BTreeMap::from([((*id).clone(), "spotify:track:target".into())]),
                },
            );
        }
        let target = "spotify:track:target";
        session.decisions.insert(
            first.clone(),
            RowDecision {
                status: RowStatus::Done,
                excluded: false,
            },
        );
        let current = vec![&session.rows[1]];
        session.count_modes.insert(target.into(), CountMode::Sum);
        assert_eq!(
            historical_count_for_target(&session, target, &current, &PageOptions::default()),
            4
        );
        session
            .count_modes
            .insert(target.into(), CountMode::Overwrite);
        assert_eq!(
            historical_count_for_target(&session, target, &current, &PageOptions::default()),
            2
        );
        session.count_modes.insert(target.into(), CountMode::Zero);
        assert_eq!(
            historical_count_for_target(&session, target, &current, &PageOptions::default()),
            0
        );

        let dir = tempfile::tempdir().unwrap();
        let store = ImportSessionStore::new(dir.path());
        store.save(&session).unwrap();
        assert_eq!(
            store.load().unwrap().unwrap().count_modes[target],
            CountMode::Zero
        );
    }

    #[test]
    fn album_candidates_are_classified_from_track_set_overlap() {
        let source = vec!["a".into(), "b".into()];
        let mut candidates = vec![
            AlbumCandidate {
                uri: "best".into(),
                name: "Best".into(),
                artist: "A".into(),
                track_uris: vec!["a".into(), "b".into()],
                track_names: vec![],
                track_artists: vec![],
                track_albums: vec![],
                relation: None,
            },
            AlbumCandidate {
                uri: "super".into(),
                name: "Super".into(),
                artist: "A".into(),
                track_uris: vec!["a".into(), "b".into(), "c".into()],
                track_names: vec![],
                track_artists: vec![],
                track_albums: vec![],
                relation: None,
            },
            AlbumCandidate {
                uri: "same".into(),
                name: "Same".into(),
                artist: "A".into(),
                track_uris: vec!["a".into(), "z".into()],
                track_names: vec![],
                track_artists: vec![],
                track_albums: vec![],
                relation: None,
            },
        ];
        classify_album_candidates(&source, &mut candidates);
        assert_eq!(candidates[0].relation, Some(AlbumRelation::BestMatch));
        assert_eq!(candidates[1].relation, Some(AlbumRelation::Superset));
        assert_eq!(candidates[2].relation, Some(AlbumRelation::SameSongs));
    }

    #[test]
    fn fuzzy_strategy_remains_independent_per_target() {
        let mut session = LastFmImportSessionV1::new("user".into(), "spotify".into(), 10);
        aggregate_scrobbles(
            &mut session.rows,
            &[
                scrobble("Artist", "Album", "One", 1),
                scrobble("artist", "album", "one", 2),
                scrobble("Artist", "Album", "Two", 3),
            ],
        );
        let first = session.rows[0].stable_id.clone();
        let second = session.rows[1].stable_id.clone();
        session.matches.insert(
            first.clone(),
            MatchResult {
                source_id: first.clone(),
                search_term: String::new(),
                confidence: Some(Confidence::Exact),
                selected_uri: Some("spotify:track:one".into()),
                candidates: Vec::new(),
                track_matches: BTreeMap::from([(first.clone(), "spotify:track:one".into())]),
            },
        );
        session.matches.insert(
            second.clone(),
            MatchResult {
                source_id: second.clone(),
                search_term: String::new(),
                confidence: Some(Confidence::Exact),
                selected_uri: Some("spotify:track:two".into()),
                candidates: Vec::new(),
                track_matches: BTreeMap::from([(second.clone(), "spotify:track:two".into())]),
            },
        );
        session
            .count_modes
            .insert("spotify:track:one".into(), CountMode::Sum);
        session
            .count_modes
            .insert("spotify:track:two".into(), CountMode::Zero);
        assert_eq!(
            historical_count_for_target(
                &session,
                "spotify:track:one",
                &[&session.rows[0]],
                &PageOptions::default()
            ),
            2
        );
        assert_eq!(
            historical_count_for_target(
                &session,
                "spotify:track:two",
                &[&session.rows[1]],
                &PageOptions::default()
            ),
            0
        );
    }

    #[test]
    fn history_is_max_count_and_max_latest_with_earliest_added_at() {
        let mut library = Library::new();
        let id = library.add(retune_core::model::NewTrack {
            uri: "spotify:track:song".into(),
            source: SourceId::Music,
            art: "Artist".into(),
            alb: "Album".into(),
            name: "Song".into(),
            duration: Duration::from_secs(1),
            added_at: Some(50),
            ..retune_core::model::NewTrack::default()
        });
        library.tracks_mut()[0].play_count = 8;
        library.tracks_mut()[0].last_played_at = Some(90);
        apply_history_updates(
            &mut library,
            &[HistoryUpdate {
                uri: "spotify:track:song".into(),
                play_count: Some(4),
                earliest: Some(10),
                latest: Some(100),
            }],
        );
        let track = library.get(id).unwrap();
        assert_eq!(
            (track.play_count, track.last_played_at, track.added_at),
            (8, Some(100), Some(10))
        );
        apply_history_updates(
            &mut library,
            &[HistoryUpdate {
                uri: "spotify:track:song".into(),
                play_count: Some(0),
                earliest: None,
                latest: None,
            }],
        );
        assert_eq!(library.get(id).unwrap().play_count, 8);
    }

    #[test]
    fn content_and_history_intents_are_independent_but_not_both_empty() {
        let defaults = ImportDefaults::default();
        assert_eq!(
            (
                defaults.import_content,
                defaults.include_historical_play_counts,
                defaults.whole_album
            ),
            (true, true, false)
        );
        assert!(PageOptions {
            import_content: true,
            include_historical_play_counts: false,
            ..PageOptions::default()
        }
        .validate()
        .is_ok());
        assert!(PageOptions {
            import_content: false,
            include_historical_play_counts: true,
            ..PageOptions::default()
        }
        .validate()
        .is_ok());
        assert!(PageOptions {
            import_content: false,
            include_historical_play_counts: false,
            ..PageOptions::default()
        }
        .validate()
        .is_err());
        for rating in [0, 6] {
            assert!(PageOptions {
                rating: Some(rating),
                ..PageOptions::default()
            }
            .validate()
            .is_err());
        }
        assert!(PageOptions {
            import_content: false,
            include_historical_play_counts: true,
            whole_album: true,
            ..PageOptions::default()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn whole_album_history_keeps_unmatched_source_rows_pending() {
        let rows = vec![
            SourceRow {
                stable_id: "matched".into(),
                artist: "Artist".into(),
                album: "Album".into(),
                track: "Matched".into(),
                variants: Vec::new(),
                play_count: 1,
                earliest: 1,
                latest: 1,
            },
            SourceRow {
                stable_id: "unmatched".into(),
                artist: "Artist".into(),
                album: "Album".into(),
                track: "Unmatched".into(),
                variants: Vec::new(),
                play_count: 1,
                earliest: 1,
                latest: 1,
            },
        ];
        let target_by_source = BTreeMap::from([("matched".into(), "spotify:track:one".into())]);
        assert_eq!(
            committed_source_ids(&rows, &target_by_source, true, true, true),
            vec!["matched"]
        );
        assert_eq!(
            committed_source_ids(&rows, &target_by_source, true, true, false),
            vec!["matched", "unmatched"]
        );
    }

    #[test]
    fn content_only_history_update_preserves_counts_and_last_played() {
        let mut library = Library::new();
        let id = library.add(retune_core::model::NewTrack {
            uri: "spotify:track:content".into(),
            added_at: Some(50),
            ..Default::default()
        });
        library.tracks_mut()[0].play_count = 8;
        library.tracks_mut()[0].last_played_at = Some(90);
        apply_history_updates(
            &mut library,
            &[HistoryUpdate {
                uri: "spotify:track:content".into(),
                play_count: None,
                earliest: Some(10),
                latest: None,
            }],
        );
        let track = library.get(id).unwrap();
        assert_eq!(
            (track.play_count, track.last_played_at, track.added_at),
            (8, Some(90), Some(10))
        );
    }

    #[tokio::test]
    async fn fake_spotify_transport_keeps_album_and_track_import_memberships_exact() {
        let album = membership_uris_for_import(
            true,
            true,
            Some("spotify:album:album"),
            &["spotify:track:ignored".into()],
        )
        .unwrap();
        let tracks = membership_uris_for_import(
            true,
            false,
            None,
            &[
                "spotify:track:one".into(),
                "spotify:track:two".into(),
                "spotify:track:one".into(),
            ],
        )
        .unwrap();
        let client = retune_spotify::client::fake_client(
            [
                retune_spotify::client::Response::json(204, Value::Null),
                retune_spotify::client::Response::json(204, Value::Null),
            ],
            "user-library-modify",
        );
        client.save_to_library(&album).await.unwrap();
        client.save_to_library(&tracks).await.unwrap();
        let requests = client.transport().requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            url::Url::parse(&requests[0].url)
                .unwrap()
                .query_pairs()
                .find(|(key, _)| key == "uris")
                .unwrap()
                .1,
            "spotify:album:album"
        );
        assert_eq!(
            url::Url::parse(&requests[1].url)
                .unwrap()
                .query_pairs()
                .find(|(key, _)| key == "uris")
                .unwrap()
                .1,
            "spotify:track:one,spotify:track:two"
        );

        let mut album_membership = SpotifyLibraryState {
            account_id: "spotify-user".into(),
            complete: true,
            ..SpotifyLibraryState::default()
        };
        album_membership.add_saved_album(SavedAlbumRecord {
            uri: "spotify:album:album".into(),
            name: "Album".into(),
            artists: vec!["Artist".into()],
            release_date: None,
            album_type: None,
            added_at: Some(100),
            track_uris: vec!["spotify:track:one".into(), "spotify:track:two".into()],
        });
        assert!(album_membership.saved_tracks.is_empty());
        assert_eq!(album_membership.saved_albums.len(), 1);

        let mut track_membership = SpotifyLibraryState {
            account_id: "spotify-user".into(),
            complete: true,
            ..SpotifyLibraryState::default()
        };
        for uri in ["spotify:track:one", "spotify:track:two"] {
            track_membership.add_saved_track(uri.into(), Some(100));
        }
        assert!(track_membership.saved_albums.is_empty());
        assert_eq!(track_membership.saved_tracks.len(), 2);
    }

    #[test]
    fn metadata_scope_and_blank_values_preserve_existing_data() {
        let mut library = Library::new();
        let first = library.add(retune_core::model::NewTrack {
            uri: "one".into(),
            art: "A".into(),
            alb: "B".into(),
            cat: "Old".into(),
            ..Default::default()
        });
        let second = library.add(retune_core::model::NewTrack {
            uri: "two".into(),
            art: "A".into(),
            alb: "B".into(),
            cat: "Old".into(),
            ..Default::default()
        });
        apply_metadata(
            &mut library,
            &["one".into(), "two".into()],
            false,
            Some(" "),
            Some(5),
        )
        .unwrap();
        assert_eq!(
            library.get(first).unwrap().rating.map(Rating::stars),
            Some(5)
        );
        assert_eq!(
            library.get(second).unwrap().rating.map(Rating::stars),
            Some(5)
        );
        assert_eq!(library.get(first).unwrap().cat, "Old");
        apply_metadata(
            &mut library,
            &["one".into(), "two".into()],
            true,
            Some("Rock"),
            Some(4),
        )
        .unwrap();
        assert_eq!(
            library
                .album_rating(&AlbumKey {
                    source: SourceId::Music,
                    art: "A".into(),
                    alb: "B".into()
                })
                .map(Rating::stars),
            Some(4)
        );
        assert_eq!(
            library.get(first).unwrap().rating.map(Rating::stars),
            Some(5)
        );
    }

    #[test]
    fn review_actions_cascade_and_remaining_count_is_durable() {
        let mut session = LastFmImportSessionV1::new("user".into(), "spotify".into(), 10);
        aggregate_scrobbles(
            &mut session.rows,
            &[
                scrobble("A", "Album", "One", 1),
                scrobble("A", "Other", "Two", 2),
                scrobble("B", "Album", "Three", 3),
            ],
        );
        assert_eq!(session.remaining(), 3);
        skip_album(&mut session, "A", "Album");
        assert_eq!(session.remaining(), 3);
        // Skipped pages remain revisitable; turning the page back to pending is undoable.
        session.decisions.values_mut().for_each(|decision| {
            if decision.status == RowStatus::Skipped {
                decision.status = RowStatus::Pending
            }
        });
        ignore_artist(&mut session, "A");
        assert_eq!(session.remaining(), 1);
        let open_id = session.rows[2].stable_id.clone();
        exclude_row(&mut session, &open_id, true);
        assert!(session.decisions.values().any(|decision| decision.excluded));
        ignore_album(&mut session, "B", "Album");
        assert_eq!(session.remaining(), 0);
    }

    #[tokio::test]
    async fn fully_excluded_review_action_reaches_done_and_has_view_only_queue_status() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        service
            .start_or_resume("lastfm-user", "spotify-user", 500, None)
            .await
            .unwrap();
        service
            .checkpoint_page(
                1,
                &ParsedRecentTracksPage {
                    page: 1,
                    total_pages: Some(1),
                    tracks: vec![
                        scrobble("A", "Album", "One", 1),
                        scrobble("A", "Album", "Two", 2),
                    ],
                    ..ParsedRecentTracksPage::default()
                },
            )
            .await
            .unwrap();
        let mut session = service.snapshot().await.unwrap();
        session.phase = ImportPhase::Review;
        service.save(session.clone()).await.unwrap();
        for row in &session.rows {
            service
                .review_action(
                    "lastfm-user",
                    "spotify-user",
                    &row.stable_id,
                    "exclude",
                    "A",
                    "Album",
                )
                .await
                .unwrap();
        }
        let session = service.snapshot().await.unwrap();
        let refs = session.rows.iter().collect::<Vec<_>>();
        assert_eq!(session.phase, ImportPhase::Done);
        assert_eq!(queue_status(&session, &refs), Some(QueueStatus::Excluded));
        assert_eq!(session.remaining(), 0);
    }

    #[tokio::test]
    async fn owned_review_mutations_reject_mismatch_and_suspension() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        let mut session = LastFmImportSessionV1::new("user".into(), "spotify".into(), 10);
        session.phase = ImportPhase::Review;
        service.save(session).await.unwrap();

        assert!(service
            .set_search_terms("other", "spotify", false)
            .await
            .is_err());
        let mut session = service.snapshot().await.unwrap();
        session.phase = ImportPhase::Suspended;
        service.save(session).await.unwrap();

        assert!(service
            .review_action("user", "spotify", "id", "exclude", "Artist", "Album")
            .await
            .is_err());
        assert!(service
            .update_options("user", "spotify", "Artist", "Album", PageOptions::default(),)
            .await
            .is_err());
        assert!(service
            .set_count_mode("user", "spotify", "spotify:track:target", CountMode::Zero)
            .await
            .is_err());
        assert!(service
            .set_search_terms("user", "spotify", false)
            .await
            .is_err());
        assert!(service
            .set_match(
                "user",
                "spotify",
                MatchResult {
                    source_id: "id".into(),
                    search_term: "track".into(),
                    confidence: None,
                    selected_uri: None,
                    candidates: Vec::new(),
                    track_matches: BTreeMap::new(),
                },
            )
            .await
            .is_err());
        assert!(service
            .select_match("user", "spotify", "id", "spotify:track:target")
            .await
            .is_err());

        let session = service.snapshot().await.unwrap();
        assert_eq!(session.phase, ImportPhase::Suspended);
        assert!(session.page_options.is_empty());
        assert!(session.matches.is_empty());
        assert!(session.search_terms);
    }

    #[test]
    fn persistence_round_trip_quarantines_corrupt_unknown_and_rejects_oversize() {
        let dir = tempfile::tempdir().unwrap();
        let store = ImportSessionStore::new(dir.path());
        let session = LastFmImportSessionV1::new("user".into(), "spotify".into(), 42);
        store.save(&session).unwrap();
        assert_eq!(store.load().unwrap(), Some(session.clone()));
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(dir.path().join("lastfm-import.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let mut invalid_options = session.clone();
        invalid_options.page_options.insert(
            "Artist\u{1f}Album".into(),
            PageOptions {
                rating: Some(6),
                ..PageOptions::default()
            },
        );
        store.save(&invalid_options).unwrap();
        assert_eq!(store.load().unwrap(), None);

        let mut invalid = session.clone();
        invalid.defaults = ImportDefaults {
            import_content: false,
            include_historical_play_counts: false,
            whole_album: false,
        };
        store.save(&invalid).unwrap();
        assert_eq!(store.load().unwrap(), None);

        fs::write(dir.path().join("lastfm-import.json"), br"not json").unwrap();
        assert_eq!(store.load().unwrap(), None);
        assert!(fs::read_dir(dir.path()).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("quarantine")));

        let mut unknown = LastFmImportSessionV1::new("user".into(), "spotify".into(), 42);
        unknown.version = 99;
        store.save(&unknown).unwrap();
        assert_eq!(store.load().unwrap(), None);
        assert!(fs::read_dir(dir.path()).unwrap().count() >= 2);

        let mut too_large = LastFmImportSessionV1::new("user".into(), "spotify".into(), 42);
        too_large.rows.push(SourceRow {
            stable_id: "x".into(),
            artist: "a".into(),
            album: "b".into(),
            track: "c".into(),
            variants: vec![SourceVariant {
                artist: "a".into(),
                album: "b".into(),
                track: "c".into(),
                play_count: 1,
                earliest: 1,
                latest: 1,
            }],
            play_count: 1,
            earliest: 1,
            latest: 1,
        });
        too_large.rows[0].variants[0].track = "x".repeat(MAX_SERIALIZED_SESSION_BYTES);
        assert!(store.save(&too_large).is_err());
    }

    #[tokio::test]
    async fn page_checkpoint_resume_is_idempotent_and_account_mismatch_suspends() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        service
            .start_or_resume("lastfm-user", "spotify-user", 500, None)
            .await
            .unwrap();
        let parsed = ParsedRecentTracksPage {
            page: 1,
            total_pages: Some(2),
            total: Some(2),
            tracks: vec![scrobble("Artist", "Album", "Track", 10)],
            ..ParsedRecentTracksPage::default()
        };
        service.checkpoint_page(1, &parsed).await.unwrap();
        service.checkpoint_page(1, &parsed).await.unwrap();
        let resumed = Service::new(dir.path());
        let session = resumed.snapshot().await.unwrap();
        assert_eq!(session.next_page, 2);
        assert_eq!(session.rows.len(), 1);
        assert_eq!(session.included_scrobbles, 1);

        let mismatch = resumed
            .start_or_resume("other-user", "spotify-user", 600, None)
            .await;
        assert!(mismatch.is_err());
        assert_eq!(
            resumed.snapshot().await.unwrap().phase,
            ImportPhase::Suspended
        );
        let resumed_for_owner = resumed
            .start_or_resume("lastfm-user", "spotify-user", 600, None)
            .await
            .unwrap();
        assert_eq!(resumed_for_owner.phase, Some(ImportPhase::Downloading));
    }

    #[tokio::test]
    async fn search_terms_preference_round_trips_on_resume() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        service
            .start_or_resume("lastfm-user", "spotify-user", 500, None)
            .await
            .unwrap();
        let mut review_session = service.snapshot().await.unwrap();
        review_session.phase = ImportPhase::Review;
        service.save(review_session).await.unwrap();
        service
            .set_search_terms("lastfm-user", "spotify-user", false)
            .await
            .unwrap();
        assert!(!service.state().await.search_terms);
        let resumed = Service::new(dir.path());
        assert!(!resumed.state().await.search_terms);
    }

    #[tokio::test]
    async fn overlapping_mutations_preserve_memory_and_disk_changes() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        service
            .start_or_resume("lastfm-user", "spotify-user", 500, None)
            .await
            .unwrap();
        let mut review_session = service.snapshot().await.unwrap();
        review_session.phase = ImportPhase::Review;
        service.save(review_session).await.unwrap();
        let (search_terms, count_mode) = tokio::join!(
            service.set_search_terms("lastfm-user", "spotify-user", false),
            service.set_count_mode(
                "lastfm-user",
                "spotify-user",
                "spotify:track:target",
                CountMode::Overwrite
            ),
        );
        search_terms.unwrap();
        count_mode.unwrap();
        let current = service.snapshot().await.unwrap();
        assert!(!current.search_terms);
        assert_eq!(
            current.count_modes.get("spotify:track:target"),
            Some(&CountMode::Overwrite)
        );
        let resumed = Service::new(dir.path());
        let persisted = resumed.snapshot().await.unwrap();
        assert!(!persisted.search_terms);
        assert_eq!(
            persisted.count_modes.get("spotify:track:target"),
            Some(&CountMode::Overwrite)
        );
    }

    #[tokio::test]
    async fn failed_blocking_persistence_does_not_commit_live_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let mut service = Service::new(dir.path());
        service
            .start_or_resume("lastfm-user", "spotify-user", 500, None)
            .await
            .unwrap();
        let mut review_session = service.snapshot().await.unwrap();
        review_session.phase = ImportPhase::Review;
        service.save(review_session).await.unwrap();
        Arc::get_mut(&mut service).unwrap().store.path = dir.path().to_path_buf();

        let error = service
            .set_search_terms("lastfm-user", "spotify-user", false)
            .await
            .unwrap_err();
        assert!(error.contains("Could not replace the Last.fm store"));
        assert!(service.snapshot().await.unwrap().search_terms);
        assert!(
            ImportSessionStore::new(dir.path())
                .load()
                .unwrap()
                .unwrap()
                .search_terms
        );
    }

    #[tokio::test]
    async fn matching_checkpoint_and_finalization_cannot_write_through_suspension() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        service
            .start_or_resume("lastfm-user", "spotify-user", 500, None)
            .await
            .unwrap();
        service
            .checkpoint_page(
                1,
                &ParsedRecentTracksPage {
                    page: 1,
                    total_pages: Some(1),
                    tracks: vec![scrobble("Artist", "Album", "Track", 10)],
                    ..ParsedRecentTracksPage::default()
                },
            )
            .await
            .unwrap();
        service.suspend_for_account_mismatch().await.unwrap();
        let result = service
            .set_matches_during_matching(
                "lastfm-user",
                "spotify-user",
                vec![MatchResult {
                    source_id: "artist\u{1f}album\u{1f}track".into(),
                    search_term: "track search".into(),
                    confidence: Some(Confidence::Exact),
                    selected_uri: Some("spotify:track:target".into()),
                    candidates: Vec::new(),
                    track_matches: BTreeMap::new(),
                }],
            )
            .await;
        assert!(result.is_err());
        assert!(service
            .finish_matching_if_current("lastfm-user", "spotify-user")
            .await
            .is_err());
        let session = service.snapshot().await.unwrap();
        assert_eq!(session.phase, ImportPhase::Suspended);
        assert!(session.matches.is_empty());
    }

    #[tokio::test]
    async fn suspended_reads_are_redacted_and_empty() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        service
            .start_or_resume("prior-user", "prior-spotify", 500, None)
            .await
            .unwrap();
        service
            .checkpoint_page(
                1,
                &ParsedRecentTracksPage {
                    page: 1,
                    total_pages: Some(1),
                    tracks: vec![scrobble("Prior Artist", "Prior Album", "Track", 10)],
                    ..ParsedRecentTracksPage::default()
                },
            )
            .await
            .unwrap();
        service.suspend_for_account_mismatch().await.unwrap();
        let state = service.state().await;
        assert_eq!(state.phase, Some(ImportPhase::Suspended));
        assert_eq!(state.username, None);
        assert_eq!(state.spotify_account_id, None);
        assert_eq!(state.remaining, 0);
        assert!(service.queue().await.is_empty());
        assert!(service.page("Prior Artist", "Prior Album").await.is_none());
    }

    #[tokio::test]
    async fn mismatched_pages_do_not_advance_and_page_batches_are_compact() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        service
            .start_or_resume("lastfm-user", "spotify-user", 500, None)
            .await
            .unwrap();
        let mismatched = ParsedRecentTracksPage {
            page: 2,
            tracks: vec![scrobble("Artist", "Album", "Track", 10)],
            ..ParsedRecentTracksPage::default()
        };
        assert!(service.checkpoint_page(1, &mismatched).await.is_err());
        assert_eq!(service.snapshot().await.unwrap().next_page, 1);
        let duplicate_page = ParsedRecentTracksPage {
            page: 1,
            tracks: vec![
                scrobble("Artist", "Album", "Track", 10),
                scrobble("artist", "album", "track", 20),
            ],
            ..ParsedRecentTracksPage::default()
        };
        service.checkpoint_page(1, &duplicate_page).await.unwrap();
        let session = service.snapshot().await.unwrap();
        assert_eq!(session.batches[0].source_ids.len(), 1);
        assert_eq!(session.next_page, 2);
    }

    #[tokio::test]
    async fn queue_reports_exact_entity_counts_for_current_page_choices() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        service
            .start_or_resume("lastfm-user", "spotify-user", 500, None)
            .await
            .unwrap();
        let parsed = ParsedRecentTracksPage {
            page: 1,
            total_pages: Some(1),
            total: Some(2),
            tracks: vec![
                scrobble("Artist", "Album", "One", 10),
                scrobble("Artist", "Album", "Two", 20),
            ],
            ..ParsedRecentTracksPage::default()
        };
        service.checkpoint_page(1, &parsed).await.unwrap();
        let mut review_session = service.snapshot().await.unwrap();
        review_session.phase = ImportPhase::Review;
        service.save(review_session).await.unwrap();
        let rows = service.snapshot().await.unwrap().rows;
        for row in &rows {
            let uri = format!("spotify:track:{}", row.track.to_lowercase());
            let mut track_matches = BTreeMap::new();
            track_matches.insert(row.stable_id.clone(), uri.clone());
            service
                .set_match(
                    "lastfm-user",
                    "spotify-user",
                    MatchResult {
                        source_id: row.stable_id.clone(),
                        search_term: "album search".into(),
                        confidence: Some(Confidence::Exact),
                        selected_uri: Some("spotify:album:album".into()),
                        candidates: vec![AlbumCandidate {
                            uri: "spotify:album:album".into(),
                            name: "Album".into(),
                            artist: "Artist".into(),
                            track_uris: vec![uri],
                            track_names: vec![row.track.clone()],
                            track_artists: vec!["Artist".into()],
                            track_albums: vec!["Album".into()],
                            relation: Some(AlbumRelation::BestMatch),
                        }],
                        track_matches,
                    },
                )
                .await
                .unwrap();
        }
        let queue = service.queue().await;
        assert_eq!((queue[0].album_entities, queue[0].track_entities), (0, 2));

        service
            .update_options(
                "lastfm-user",
                "spotify-user",
                "Artist",
                "Album",
                PageOptions {
                    whole_album: true,
                    selected_track_ids: rows.iter().map(|row| row.stable_id.clone()).collect(),
                    ..PageOptions::default()
                },
            )
            .await
            .unwrap();
        let queue = service.queue().await;
        assert_eq!((queue[0].album_entities, queue[0].track_entities), (1, 0));

        let selected_track_ids = rows
            .iter()
            .map(|row| row.stable_id.clone())
            .collect::<BTreeSet<_>>();
        service
            .update_options(
                "lastfm-user",
                "spotify-user",
                "Artist",
                "Album",
                PageOptions {
                    whole_album: false,
                    selected_track_ids,
                    ..PageOptions::default()
                },
            )
            .await
            .unwrap();
        let queue = service.queue().await;
        assert_eq!((queue[0].album_entities, queue[0].track_entities), (0, 2));

        service
            .update_options(
                "lastfm-user",
                "spotify-user",
                "Artist",
                "Album",
                PageOptions {
                    import_content: false,
                    include_historical_play_counts: true,
                    ..PageOptions::default()
                },
            )
            .await
            .unwrap();
        let queue = service.queue().await;
        assert_eq!((queue[0].album_entities, queue[0].track_entities), (0, 0));
    }

    #[tokio::test]
    async fn selecting_an_album_candidate_remaps_every_related_source_track() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        service
            .start_or_resume("lastfm-user", "spotify-user", 500, None)
            .await
            .unwrap();
        let parsed = ParsedRecentTracksPage {
            page: 1,
            total_pages: Some(1),
            tracks: vec![
                scrobble("Artist", "Album", "One", 10),
                scrobble("Artist", "Album", "Two", 20),
            ],
            ..ParsedRecentTracksPage::default()
        };
        service.checkpoint_page(1, &parsed).await.unwrap();
        let mut review_session = service.snapshot().await.unwrap();
        review_session.phase = ImportPhase::Review;
        service.save(review_session).await.unwrap();
        let rows = service.snapshot().await.unwrap().rows;
        for row in &rows {
            let old_track = format!("spotify:track:old-{}", row.track.to_lowercase());
            let new_track = format!("spotify:track:new-{}", row.track.to_lowercase());
            let mut track_matches = BTreeMap::new();
            track_matches.insert(row.stable_id.clone(), old_track.clone());
            service
                .set_match(
                    "lastfm-user",
                    "spotify-user",
                    MatchResult {
                        source_id: row.stable_id.clone(),
                        search_term: "album search".into(),
                        confidence: Some(Confidence::Exact),
                        selected_uri: Some("spotify:album:old".into()),
                        candidates: vec![
                            AlbumCandidate {
                                uri: "spotify:album:old".into(),
                                name: "Old release".into(),
                                artist: "Artist".into(),
                                track_uris: vec![old_track],
                                track_names: vec![row.track.clone()],
                                track_artists: vec!["Artist".into()],
                                track_albums: vec!["Old release".into()],
                                relation: Some(AlbumRelation::BestMatch),
                            },
                            AlbumCandidate {
                                uri: "spotify:album:new".into(),
                                name: "Alternate release".into(),
                                artist: "Artist".into(),
                                track_uris: vec![new_track],
                                track_names: vec![row.track.clone()],
                                track_artists: vec!["Artist".into()],
                                track_albums: vec!["Alternate release".into()],
                                relation: Some(AlbumRelation::BestMatch),
                            },
                        ],
                        track_matches,
                    },
                )
                .await
                .unwrap();
        }

        service
            .select_match(
                "lastfm-user",
                "spotify-user",
                &rows[0].stable_id,
                "spotify:album:new",
            )
            .await
            .unwrap();
        let session = service.snapshot().await.unwrap();
        for row in rows {
            let result = session.matches.get(&row.stable_id).unwrap();
            assert_eq!(result.selected_uri.as_deref(), Some("spotify:album:new"));
            assert_eq!(
                result.track_matches.get(&row.stable_id),
                Some(&format!("spotify:track:new-{}", row.track.to_lowercase()))
            );
        }
    }
}

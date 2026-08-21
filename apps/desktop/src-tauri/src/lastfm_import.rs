use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    future::Future,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use retune_core::model::{AlbumKey, Library, Rating, TrackEdit};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use tokio::sync::Mutex;

pub(crate) const SESSION_VERSION: u8 = 2;
pub(crate) const LASTFM_PAGE_LIMIT: u32 = 200;
pub(crate) const LASTFM_REVIEW_BATCH_SIZE: usize = 100;
const LASTFM_PAGE_WINDOW_SIZE: u32 = 4;
const LASTFM_QUEUE_PAGE_LIMIT: usize = 1000;
pub(crate) const MAX_SERIALIZED_SESSION_BYTES: usize = 100 * 1024 * 1024;
const MAX_RAW_CACHE_BYTES: u64 = 100 * 1024 * 1024;
const LASTFM_SYNC_VERSION: u8 = 1;
pub(crate) const LASTFM_MAPPINGS_VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ImportPhase {
    Downloading,
    Aggregating,
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
pub(crate) struct LastFmImportSessionV2 {
    pub version: u8,
    pub lastfm_username: String,
    pub spotify_account_id: Option<String>,
    pub history_to: u64,
    #[serde(default)]
    pub downloaded_through: Option<u64>,
    pub cache_id: String,
    pub next_page: u32,
    pub total_pages: Option<u32>,
    pub downloaded_pages: u32,
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
    #[serde(default)]
    pub incremental_source_keys: BTreeMap<String, String>,
    pub search_terms: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportStateView {
    pub phase: Option<ImportPhase>,
    pub username: Option<String>,
    pub spotify_account_id: Option<String>,
    pub history_to: Option<u64>,
    pub downloaded_through: Option<u64>,
    pub next_page: u32,
    pub total_pages: Option<u32>,
    pub downloaded_pages: u32,
    pub total_scrobbles: u64,
    pub included_scrobbles: u64,
    pub processed_scrobbles: u64,
    pub defaults: ImportDefaults,
    pub remaining: usize,
    pub retryable_error: Option<RetryableError>,
    pub search_terms: bool,
    pub syncing: bool,
    pub last_synced_at: Option<u64>,
    pub pending_review: usize,
    pub sync_problem: Option<String>,
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
    pub page: u32,
    pub artist: String,
    pub album: String,
    pub play_count: u64,
    pub latest: u64,
    pub source_count: usize,
    pub remaining: bool,
    pub album_entities: u32,
    pub track_entities: u32,
    pub status: Option<QueueStatus>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportQueuePage {
    pub items: Vec<ImportQueueItem>,
    pub cursor: usize,
    pub next_cursor: Option<usize>,
    pub total: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AcceptAllSummary {
    pub album_entities: u32,
    pub track_entities: u32,
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
    pub batch_id: u32,
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

impl LastFmImportSessionV2 {
    #[cfg(test)]
    pub(crate) fn new(
        lastfm_username: String,
        spotify_account_id: String,
        history_to: u64,
    ) -> Self {
        let mut session =
            Self::new_with_defaults(lastfm_username, history_to, ImportDefaults::default());
        session.spotify_account_id = Some(spotify_account_id);
        session
    }

    pub(crate) fn new_with_defaults(
        lastfm_username: String,
        history_to: u64,
        defaults: ImportDefaults,
    ) -> Self {
        let cache_id = snapshot_cache_id(&lastfm_username, history_to);
        Self {
            version: SESSION_VERSION,
            lastfm_username,
            spotify_account_id: None,
            history_to,
            downloaded_through: None,
            cache_id,
            next_page: 1,
            total_pages: None,
            downloaded_pages: 0,
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
            incremental_source_keys: BTreeMap::new(),
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

    fn options_for_batch(&self, batch_id: u32, artist: &str, album: &str) -> PageOptions {
        let Some(batch) = review_batches(self)
            .into_iter()
            .find(|batch| batch.page == batch_id)
        else {
            let mut options = self.options_for(artist, album);
            options.selected_track_ids.clear();
            return options;
        };
        let batch_ids = batch.source_ids.iter().collect::<BTreeSet<_>>();
        let batch_options = self.page_options.get(&batch_options_key(batch_id)).cloned();
        let legacy_options = self
            .page_options
            .get(&format!("{artist}\u{1f}{album}"))
            .cloned();
        let mut options = batch_options
            .clone()
            .or(legacy_options.clone())
            .unwrap_or_else(|| PageOptions::from_defaults(&self.defaults));
        if batch_options.is_some() || legacy_options.is_some() {
            options
                .selected_track_ids
                .retain(|id| batch_ids.contains(id));
        } else {
            options.selected_track_ids = batch
                .source_ids
                .iter()
                .filter(|id| {
                    let id = (*id).as_str();
                    self.rows.iter().any(|row| {
                        row.stable_id == id
                            && row.artist == artist
                            && row.album == album
                            && is_actionable(self, &row.stable_id)
                    })
                })
                .cloned()
                .collect();
        }
        options
    }

    fn options_for_page_batch(
        &self,
        batch: &ImportBatch,
        artist: &str,
        album: &str,
        rows: &[&SourceRow],
    ) -> PageOptions {
        let batch_ids = batch.source_ids.iter().collect::<BTreeSet<_>>();
        let batch_options = self
            .page_options
            .get(&batch_options_key(batch.page))
            .cloned();
        let legacy_options = self
            .page_options
            .get(&format!("{artist}\u{1f}{album}"))
            .cloned();
        let mut options = batch_options
            .clone()
            .or(legacy_options.clone())
            .unwrap_or_else(|| PageOptions::from_defaults(&self.defaults));
        if batch_options.is_some() || legacy_options.is_some() {
            options
                .selected_track_ids
                .retain(|id| batch_ids.contains(id));
        } else {
            options.selected_track_ids = rows
                .iter()
                .filter(|row| is_actionable(self, &row.stable_id))
                .map(|row| row.stable_id.clone())
                .collect();
        }
        options
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ParsedRecentTracksPage {
    pub page: u32,
    pub total_pages: Option<u32>,
    pub total: Option<u64>,
    pub tracks: Vec<ParsedScrobble>,
    pub skipped_now_playing: u64,
    pub skipped_undated: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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

fn discard_post_cutoff(parsed: &mut ParsedRecentTracksPage, history_to: u64) {
    parsed
        .tracks
        .retain(|scrobble| scrobble.timestamp < history_to);
}

fn sort_scrobbles(scrobbles: &mut [ParsedScrobble]) {
    scrobbles.sort_by(|left, right| {
        left.timestamp
            .cmp(&right.timestamp)
            .then_with(|| {
                normalize_for_match(&left.artist).cmp(&normalize_for_match(&right.artist))
            })
            .then_with(|| normalize_for_match(&left.album).cmp(&normalize_for_match(&right.album)))
            .then_with(|| normalize_for_match(&left.track).cmp(&normalize_for_match(&right.track)))
            .then_with(|| left.artist.cmp(&right.artist))
            .then_with(|| left.album.cmp(&right.album))
            .then_with(|| left.track.cmp(&right.track))
    });
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

fn snapshot_cache_id(username: &str, history_to: u64) -> String {
    format!(
        "{}-{history_to}",
        username
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

pub(crate) fn aggregate_scrobbles(rows: &mut Vec<SourceRow>, scrobbles: &[ParsedScrobble]) {
    let mut row_indices = HashMap::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        row_indices.entry(row.stable_id.clone()).or_insert(index);
    }
    for scrobble in scrobbles {
        let id = source_id(&scrobble.artist, &scrobble.album, &scrobble.track);
        let index = if let Some(index) = row_indices.get(&id).copied() {
            index
        } else {
            let index = rows.len();
            rows.push(SourceRow {
                stable_id: id.clone(),
                artist: scrobble.artist.clone(),
                album: scrobble.album.clone(),
                track: scrobble.track.clone(),
                variants: Vec::new(),
                play_count: 0,
                earliest: scrobble.timestamp,
                latest: scrobble.timestamp,
            });
            row_indices.insert(id, index);
            index
        };
        add_variant(&mut rows[index], scrobble);
    }
}

fn incremental_source_id(source_key: &str) -> String {
    format!("incremental:{source_key}")
}

fn aggregate_incremental_scrobbles(
    rows: &mut Vec<SourceRow>,
    source_keys: &mut BTreeMap<String, String>,
    scrobbles: &[ExternalScrobble],
) {
    let mut row_indices = rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| {
            source_keys
                .get(&row.stable_id)
                .map(|source_key| (source_key.clone(), index))
        })
        .collect::<HashMap<_, _>>();
    for scrobble in scrobbles {
        let source_key = source_id(&scrobble.artist, &scrobble.album, &scrobble.track);
        let stable_id = incremental_source_id(&source_key);
        let index = if let Some(index) = row_indices.get(&source_key).copied() {
            index
        } else {
            let index = rows.len();
            rows.push(SourceRow {
                stable_id: stable_id.clone(),
                artist: scrobble.artist.clone(),
                album: scrobble.album.clone(),
                track: scrobble.track.clone(),
                variants: Vec::new(),
                play_count: 0,
                earliest: scrobble.timestamp,
                latest: scrobble.timestamp,
            });
            source_keys.insert(stable_id, source_key.clone());
            row_indices.insert(source_key, index);
            index
        };
        add_variant(
            &mut rows[index],
            &ParsedScrobble {
                artist: scrobble.artist.clone(),
                album: scrobble.album.clone(),
                track: scrobble.track.clone(),
                timestamp: scrobble.timestamp,
            },
        );
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

fn batch_options_key(batch_id: u32) -> String {
    format!("batch:{batch_id}")
}

fn build_review_batches(rows: &[SourceRow]) -> Vec<ImportBatch> {
    let mut grouped = BTreeMap::<(String, String), Vec<String>>::new();
    for row in rows {
        grouped
            .entry((row.artist.clone(), row.album.clone()))
            .or_default()
            .push(row.stable_id.clone());
    }
    let mut page = 1;
    let mut batches = Vec::new();
    for source_ids in grouped.into_values() {
        for chunk in source_ids.chunks(LASTFM_REVIEW_BATCH_SIZE) {
            batches.push(ImportBatch {
                page,
                source_ids: chunk.to_vec(),
            });
            page += 1;
        }
    }
    batches
}

fn review_batches(session: &LastFmImportSessionV2) -> Vec<ImportBatch> {
    review_batches_for_read(session).into_owned()
}

fn review_batches_for_read(session: &LastFmImportSessionV2) -> Cow<'_, [ImportBatch]> {
    if session.batches.is_empty()
        || session
            .batches
            .iter()
            .any(|batch| batch.page == 0 || batch.source_ids.is_empty())
    {
        Cow::Owned(build_review_batches(&session.rows))
    } else {
        Cow::Borrowed(&session.batches)
    }
}

fn source_row_map(session: &LastFmImportSessionV2) -> HashMap<&str, &SourceRow> {
    session
        .rows
        .iter()
        .map(|row| (row.stable_id.as_str(), row))
        .collect()
}

fn source_batch_map(session: &LastFmImportSessionV2) -> HashMap<String, u32> {
    let mut result = HashMap::new();
    for batch in review_batches(session) {
        for source_id in &batch.source_ids {
            result.insert(source_id.clone(), batch.page);
        }
    }
    result
}

fn batch_rows<'a>(
    batch: &ImportBatch,
    rows: &HashMap<&'a str, &'a SourceRow>,
) -> Vec<&'a SourceRow> {
    batch
        .source_ids
        .iter()
        .filter_map(|id| rows.get(id.as_str()).copied())
        .collect()
}

fn requested_batch(
    session: &LastFmImportSessionV2,
    batch_id: u32,
    artist: &str,
    album: &str,
) -> Option<ImportBatch> {
    let rows = source_row_map(session);
    review_batches(session).into_iter().find(|batch| {
        batch.page == batch_id
            && batch_rows(batch, &rows).len() == batch.source_ids.len()
            && batch_rows(batch, &rows)
                .iter()
                .all(|row| row.artist == artist && row.album == album)
    })
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

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ScrobbleMetadata {
    pub artist: String,
    pub album: String,
    pub track: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExternalScrobble {
    pub artist: String,
    pub album: String,
    pub track: String,
    pub timestamp: u64,
    #[serde(default)]
    pub submitted: Option<ScrobbleMetadata>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AcceptedScrobbleReceipt {
    pub corrected: ScrobbleMetadata,
    pub submitted: ScrobbleMetadata,
    pub timestamp: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LastFmAlbumMapping {
    pub spotify_album_uri: String,
    pub track_uris_by_name: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LastFmMappings {
    pub track_mappings: BTreeMap<String, String>,
    pub album_mappings: BTreeMap<String, LastFmAlbumMapping>,
    pub excluded_tracks: BTreeSet<String>,
    pub ignored_albums: BTreeSet<String>,
    pub ignored_artists: BTreeSet<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ReconciliationResult {
    pub increments: BTreeMap<String, u64>,
    pub latest: BTreeMap<String, u64>,
    pub unresolved: Vec<ExternalScrobble>,
    pub consumed_receipts: Vec<AcceptedScrobbleReceipt>,
}

fn metadata_matches_event(metadata: &ScrobbleMetadata, event: &ExternalScrobble) -> bool {
    let matches = |candidate: &ScrobbleMetadata| {
        normalize_for_match(&metadata.artist) == normalize_for_match(&candidate.artist)
            && normalize_for_match(&metadata.album) == normalize_for_match(&candidate.album)
            && normalize_for_match(&metadata.track) == normalize_for_match(&candidate.track)
    };
    matches(&ScrobbleMetadata {
        artist: event.artist.clone(),
        album: event.album.clone(),
        track: event.track.clone(),
    }) || event.submitted.as_ref().is_some_and(matches)
}

fn source_album_key(artist: &str, album: &str) -> String {
    format!(
        "{}\u{1f}{}",
        normalize_for_match(artist),
        normalize_for_match(album)
    )
}

fn mapped_target(event: &ExternalScrobble, mappings: &LastFmMappings) -> Option<Option<String>> {
    let track_key = source_id(&event.artist, &event.album, &event.track);
    if mappings.excluded_tracks.contains(&track_key)
        || mappings
            .ignored_albums
            .contains(&source_album_key(&event.artist, &event.album))
        || mappings
            .ignored_artists
            .contains(&normalize_for_match(&event.artist))
    {
        return Some(None);
    }
    if let Some(uri) = mappings.track_mappings.get(&track_key) {
        return Some(Some(uri.clone()));
    }
    mappings
        .album_mappings
        .get(&source_album_key(&event.artist, &event.album))
        .and_then(|mapping| {
            mapping
                .track_uris_by_name
                .get(&normalize_for_match(&event.track))
                .cloned()
        })
        .map(Some)
}

pub(crate) fn reconcile_incremental(
    events: &[ExternalScrobble],
    receipts: &[AcceptedScrobbleReceipt],
    mappings: &LastFmMappings,
    available_library_uris: &BTreeSet<String>,
    from: u64,
    to: u64,
) -> ReconciliationResult {
    let mut result = ReconciliationResult::default();
    let mut consumed = vec![false; receipts.len()];
    for event in events
        .iter()
        .filter(|event| event.timestamp >= from && event.timestamp < to)
    {
        if let Some(index) = receipts.iter().enumerate().find_map(|(index, receipt)| {
            (!consumed[index]
                && receipt.timestamp == event.timestamp
                && (metadata_matches_event(&receipt.corrected, event)
                    || metadata_matches_event(&receipt.submitted, event)))
            .then_some(index)
        }) {
            consumed[index] = true;
            result.consumed_receipts.push(receipts[index].clone());
            continue;
        }
        let Some(target) = mapped_target(event, mappings) else {
            result.unresolved.push(event.clone());
            continue;
        };
        let Some(target) = target else {
            continue;
        };
        if !available_library_uris.contains(&target) {
            result.unresolved.push(event.clone());
            continue;
        }
        result
            .increments
            .entry(target.clone())
            .and_modify(|count| *count = count.saturating_add(1))
            .or_insert(1);
        result
            .latest
            .entry(target)
            .and_modify(|latest| *latest = (*latest).max(event.timestamp))
            .or_insert(event.timestamp);
    }
    result
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

pub(crate) fn apply_incremental_updates(
    library: &mut Library,
    increments: &BTreeMap<String, u64>,
    latest: &BTreeMap<String, u64>,
) {
    for track in library.tracks_mut() {
        let Some(increment) = increments.get(&track.uri) else {
            continue;
        };
        track.play_count = track
            .play_count
            .saturating_add((*increment).min(u32::MAX as u64) as u32);
        if let Some(timestamp) = latest.get(&track.uri) {
            track.last_played_at = Some(track.last_played_at.unwrap_or_default().max(*timestamp));
        }
    }
}

pub(crate) fn recover_application_journal(
    library: &mut Library,
    journal: &LastFmApplicationJournal,
) -> Result<JournalRecovery, JournalRecoveryError> {
    if *library == journal.before_library {
        *library = journal.after_library.clone();
        return Ok(JournalRecovery::AppliedBefore);
    }
    if *library == journal.after_library {
        return Ok(JournalRecovery::AlreadyApplied);
    }
    Err(JournalRecoveryError::Conflict)
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
    cache_root: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCacheManifest {
    version: u8,
    cache_id: String,
    lastfm_username: String,
    history_to: u64,
    total_pages: u32,
    pages: BTreeMap<u32, u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CachedRawPage {
    lastfm_username: String,
    history_to: u64,
    total_pages: u32,
    parsed: ParsedRecentTracksPage,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IncrementalRange {
    from: u64,
    to: u64,
    query_from: u64,
    query_to: u64,
    cache_id: String,
    next_page: u32,
    total_pages: Option<u32>,
    downloaded_pages: u32,
    total_scrobbles: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LastFmSyncState {
    version: u8,
    lastfm_username: Option<String>,
    spotify_account_id: Option<String>,
    synced_through: Option<u64>,
    last_synced_at: Option<u64>,
    active: Option<IncrementalRange>,
    backlog: Vec<ExternalScrobble>,
    sync_problem: Option<String>,
    journal: Option<LastFmApplicationJournal>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LastFmApplicationJournal {
    before_library: Library,
    after_library: Library,
    checkpoint_before: Option<u64>,
    checkpoint_after: Option<u64>,
    backlog_before: Vec<ExternalScrobble>,
    backlog_after: Vec<ExternalScrobble>,
    consumed_receipts: Vec<AcceptedScrobbleReceipt>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum JournalRecovery {
    AppliedBefore,
    AlreadyApplied,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum JournalRecoveryError {
    Conflict,
}

impl std::fmt::Display for JournalRecoveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Last.fm application journal conflicts with the current library.")
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PersistedLastFmMappings {
    pub(crate) version: u8,
    pub(crate) lastfm_username: Option<String>,
    pub(crate) spotify_account_id: Option<String>,
    pub(crate) dormant: bool,
    pub(crate) mappings: LastFmMappings,
}

#[derive(Clone)]
struct IncrementalStore {
    path: PathBuf,
}

#[derive(Clone)]
struct MappingsStore {
    path: PathBuf,
}

fn quarantine_file(path: &Path, description: &str) -> Result<PathBuf, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "persistence".into());
    let target = path.with_file_name(format!("{file_name}.quarantine-{stamp}"));
    fs::rename(path, &target)
        .map(|_| target)
        .map_err(|_| format!("Could not quarantine {description}."))
}

impl IncrementalStore {
    fn new(app_data_dir: impl AsRef<Path>) -> Self {
        Self {
            path: app_data_dir.as_ref().join("lastfm-sync.json"),
        }
    }

    fn load(&self) -> Result<LastFmSyncState, String> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(LastFmSyncState {
                    version: LASTFM_SYNC_VERSION,
                    ..LastFmSyncState::default()
                })
            }
            Err(_) => {
                quarantine_file(&self.path, "the Last.fm incremental sync state")?;
                return Err("Last.fm sync state was quarantined; sync starts from now.".into());
            }
        };
        let state = match serde_json::from_slice::<LastFmSyncState>(&bytes) {
            Ok(state) => state,
            Err(_) => {
                quarantine_file(&self.path, "the Last.fm incremental sync state")?;
                return Err("Last.fm sync state was quarantined; sync starts from now.".into());
            }
        };
        if state.version != LASTFM_SYNC_VERSION {
            quarantine_file(&self.path, "the Last.fm incremental sync state")?;
            return Err("Last.fm sync state was quarantined; sync starts from now.".into());
        }
        Ok(state)
    }

    fn save(&self, state: &LastFmSyncState) -> Result<(), String> {
        let bytes = serde_json::to_vec(state)
            .map_err(|_| "Could not serialize Last.fm incremental sync state.".to_string())?;
        super::lastfm::atomic_write(&self.path, &bytes, true)
    }
}

impl MappingsStore {
    fn new(app_data_dir: impl AsRef<Path>) -> Self {
        Self {
            path: app_data_dir.as_ref().join("lastfm-mappings.json"),
        }
    }

    fn load(&self) -> Result<PersistedLastFmMappings, String> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PersistedLastFmMappings {
                    version: LASTFM_MAPPINGS_VERSION,
                    ..PersistedLastFmMappings::default()
                })
            }
            Err(_) => {
                quarantine_file(&self.path, "the Last.fm mappings")?;
                return Err(
                    "Last.fm mappings were quarantined; reusable decisions were reset.".into(),
                );
            }
        };
        let mappings = match serde_json::from_slice::<PersistedLastFmMappings>(&bytes) {
            Ok(mappings) => mappings,
            Err(_) => {
                quarantine_file(&self.path, "the Last.fm mappings")?;
                return Err(
                    "Last.fm mappings were quarantined; reusable decisions were reset.".into(),
                );
            }
        };
        if mappings.version != LASTFM_MAPPINGS_VERSION {
            quarantine_file(&self.path, "the Last.fm mappings")?;
            return Err("Last.fm mappings were quarantined; reusable decisions were reset.".into());
        }
        Ok(mappings)
    }

    fn save(&self, mappings: &PersistedLastFmMappings) -> Result<(), String> {
        let bytes = serde_json::to_vec(mappings)
            .map_err(|_| "Could not serialize Last.fm mappings.".to_string())?;
        super::lastfm::atomic_write(&self.path, &bytes, true)
    }
}

fn incremental_cache_id(username: &str, from: u64, to: u64) -> String {
    format!(
        "incremental-{}-{from}-{to}",
        username
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

fn incremental_cache_session(
    state: &LastFmSyncState,
    username: &str,
) -> Result<LastFmImportSessionV2, String> {
    let range = state
        .active
        .as_ref()
        .ok_or_else(|| "No Last.fm incremental range is active.".to_string())?;
    let total_pages = range
        .total_pages
        .ok_or_else(|| "Last.fm incremental metadata is not available yet.".to_string())?;
    let mut session = LastFmImportSessionV2::new_with_defaults(
        username.to_owned(),
        range.to,
        ImportDefaults::default(),
    );
    session.cache_id = range.cache_id.clone();
    session.total_pages = Some(total_pages);
    session.next_page = range.next_page;
    session.downloaded_pages = range.downloaded_pages;
    Ok(session)
}

impl ImportSessionStore {
    pub(crate) fn new(app_data_dir: impl AsRef<Path>) -> Self {
        let app_data_dir = app_data_dir.as_ref();
        Self {
            path: app_data_dir.join("lastfm-import.json"),
            cache_root: app_data_dir.join("lastfm-import-cache"),
        }
    }

    pub(crate) fn load(&self) -> Result<Option<LastFmImportSessionV2>, String> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err("Could not read the Last.fm import session.".into()),
        };
        if bytes.len() > MAX_SERIALIZED_SESSION_BYTES {
            self.quarantine()?;
            self.quarantine_cache_root()?;
            return Ok(None);
        }
        let parsed = serde_json::from_slice::<LastFmImportSessionV2>(&bytes);
        match parsed {
            Ok(session)
                if session.version == SESSION_VERSION
                    && session.defaults.validate().is_ok()
                    && session
                        .page_options
                        .values()
                        .all(|options| options.validate().is_ok()) =>
            {
                if (matches!(
                    session.phase,
                    ImportPhase::Downloading | ImportPhase::Aggregating
                ) || suspended_source_phase(&session))
                    && self.validate_cache(&session).is_err()
                {
                    self.quarantine_snapshot(&session.cache_id)?;
                    self.quarantine()?;
                    return Ok(None);
                }
                Ok(Some(session))
            }
            Ok(_) | Err(_) => {
                self.quarantine()?;
                self.quarantine_cache_root()?;
                Ok(None)
            }
        }
    }

    pub(crate) fn save(&self, session: &LastFmImportSessionV2) -> Result<(), String> {
        let bytes = serde_json::to_vec(session)
            .map_err(|_| "Could not serialize the Last.fm import session.".to_string())?;
        if bytes.len() > MAX_SERIALIZED_SESSION_BYTES {
            return Err("The Last.fm import session exceeds the 100 MB safety limit.".into());
        }
        super::lastfm::atomic_write(&self.path, &bytes, true)
    }

    fn cache_path(&self, cache_id: &str) -> PathBuf {
        self.cache_root.join(cache_id)
    }

    fn manifest_path(&self, cache_id: &str) -> PathBuf {
        self.cache_path(cache_id).join("manifest.json")
    }

    fn page_path(&self, cache_id: &str, page: u32) -> PathBuf {
        self.cache_path(cache_id).join(format!("page-{page}.json"))
    }

    fn read_manifest(
        &self,
        session: &LastFmImportSessionV2,
    ) -> Result<Option<RawCacheManifest>, String> {
        let path = self.manifest_path(&session.cache_id);
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err("Could not read the Last.fm import cache manifest.".into()),
        };
        if metadata.len() > MAX_RAW_CACHE_BYTES {
            return Err(
                "The Last.fm import cache manifest exceeds the 100 MB safety limit.".into(),
            );
        }
        let bytes = fs::read(path)
            .map_err(|_| "Could not read the Last.fm import cache manifest.".to_string())?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| "The Last.fm import cache manifest is corrupt.".into())
    }

    fn write_page(
        &self,
        session: &LastFmImportSessionV2,
        parsed: &ParsedRecentTracksPage,
    ) -> Result<(), String> {
        let total_pages = session
            .total_pages
            .ok_or_else(|| "Last.fm import metadata is not available yet.".to_string())?;
        if parsed.page == 0 || parsed.page > total_pages {
            return Err("Last.fm import page metadata is out of range.".into());
        }
        let cached = CachedRawPage {
            lastfm_username: session.lastfm_username.clone(),
            history_to: session.history_to,
            total_pages,
            parsed: parsed.clone(),
        };
        let bytes = serde_json::to_vec(&cached)
            .map_err(|_| "Could not serialize a Last.fm import page.".to_string())?;
        if bytes.len() as u64 > MAX_RAW_CACHE_BYTES {
            return Err("The Last.fm import page exceeds the 100 MB safety limit.".into());
        }

        let manifest = self.read_manifest(session)?;
        if let Some(manifest) = &manifest {
            self.validate_manifest_metadata(manifest, session, total_pages)?;
        }
        let mut manifest = manifest.unwrap_or_else(|| RawCacheManifest {
            version: SESSION_VERSION,
            cache_id: session.cache_id.clone(),
            lastfm_username: session.lastfm_username.clone(),
            history_to: session.history_to,
            total_pages,
            pages: BTreeMap::new(),
        });
        let previous = manifest.pages.insert(parsed.page, bytes.len() as u64);
        let current_size = manifest
            .pages
            .values()
            .copied()
            .try_fold(0_u64, u64::checked_add)
            .ok_or_else(|| "The Last.fm import cache size is invalid.".to_string())?;
        if current_size > MAX_RAW_CACHE_BYTES {
            match previous {
                Some(previous) => {
                    manifest.pages.insert(parsed.page, previous);
                }
                None => {
                    manifest.pages.remove(&parsed.page);
                }
            }
            return Err("The Last.fm import cache exceeds the 100 MB safety limit.".into());
        }
        fs::create_dir_all(self.cache_path(&session.cache_id))
            .map_err(|_| "Could not create the Last.fm import cache.".to_string())?;
        super::lastfm::atomic_write(
            &self.page_path(&session.cache_id, parsed.page),
            &bytes,
            true,
        )?;
        let manifest_bytes = serde_json::to_vec(&manifest)
            .map_err(|_| "Could not serialize the Last.fm import cache manifest.".to_string())?;
        super::lastfm::atomic_write(
            &self.manifest_path(&session.cache_id),
            &manifest_bytes,
            true,
        )
    }

    fn validate_cache(&self, session: &LastFmImportSessionV2) -> Result<(), String> {
        validate_session_cursor(session)?;
        let Some(manifest) = self.read_manifest(session)? else {
            return if session.downloaded_pages == 0 {
                Ok(())
            } else {
                Err("The Last.fm import cache manifest is missing.".into())
            };
        };
        let total_pages = session
            .total_pages
            .ok_or_else(|| "The Last.fm import cache has no page total.".to_string())?;
        self.validate_manifest_metadata(&manifest, session, total_pages)?;
        let acknowledged_pages = session.downloaded_pages as usize;
        let max_pages = acknowledged_pages + usize::from(session.next_page > 0);
        if manifest.pages.len() < acknowledged_pages || manifest.pages.len() > max_pages {
            return Err("The Last.fm import cache has a non-contiguous page suffix.".into());
        }
        if let Some((&first_page, _)) = manifest.pages.first_key_value() {
            let expected_last_page = manifest.pages.last_key_value().map(|(&page, _)| page);
            if expected_last_page != Some(total_pages)
                || first_page < session.next_page.max(1)
                || manifest
                    .pages
                    .keys()
                    .copied()
                    .zip(first_page..=total_pages)
                    .any(|(actual, expected)| actual != expected)
            {
                return Err("The Last.fm import cache has a non-contiguous page suffix.".into());
            }
        }
        let total_size = manifest
            .pages
            .values()
            .copied()
            .try_fold(0_u64, u64::checked_add)
            .ok_or_else(|| "The Last.fm import cache size is invalid.".to_string())?;
        if total_size > MAX_RAW_CACHE_BYTES {
            return Err("The Last.fm import cache exceeds the 100 MB safety limit.".into());
        }
        for (&page, &recorded_size) in &manifest.pages {
            if page == 0 || page > total_pages {
                return Err("The Last.fm import cache contains an invalid page.".into());
            }
            let path = self.page_path(&session.cache_id, page);
            let actual_size = fs::metadata(&path)
                .map_err(|_| "An acknowledged Last.fm import page is missing.".to_string())?
                .len();
            if actual_size != recorded_size || recorded_size > MAX_RAW_CACHE_BYTES {
                return Err(
                    "An acknowledged Last.fm import page is oversized or truncated.".into(),
                );
            }
            let bytes = fs::read(&path)
                .map_err(|_| "An acknowledged Last.fm import page is missing.".to_string())?;
            let cached = serde_json::from_slice::<CachedRawPage>(&bytes)
                .map_err(|_| "An acknowledged Last.fm import page is corrupt.".to_string())?;
            if cached.lastfm_username != session.lastfm_username
                || cached.history_to != session.history_to
                || cached.total_pages != total_pages
                || cached.parsed.page != page
                || cached
                    .parsed
                    .total_pages
                    .is_some_and(|value| value != total_pages)
            {
                return Err("An acknowledged Last.fm import page has mismatched metadata.".into());
            }
        }
        Ok(())
    }

    fn validate_manifest_metadata(
        &self,
        manifest: &RawCacheManifest,
        session: &LastFmImportSessionV2,
        total_pages: u32,
    ) -> Result<(), String> {
        if manifest.version != SESSION_VERSION
            || manifest.cache_id != session.cache_id
            || manifest.lastfm_username != session.lastfm_username
            || manifest.history_to != session.history_to
            || manifest.total_pages != total_pages
        {
            return Err("The Last.fm import cache metadata does not match its session.".into());
        }
        Ok(())
    }

    fn read_pages(&self, session: &LastFmImportSessionV2) -> Result<Vec<ParsedScrobble>, String> {
        self.validate_cache(session)?;
        let Some(manifest) = self.read_manifest(session)? else {
            return Ok(Vec::new());
        };
        let mut scrobbles = Vec::new();
        for page in manifest.pages.keys() {
            let bytes = fs::read(self.page_path(&session.cache_id, *page))
                .map_err(|_| "An acknowledged Last.fm import page is missing.".to_string())?;
            let cached = serde_json::from_slice::<CachedRawPage>(&bytes)
                .map_err(|_| "An acknowledged Last.fm import page is corrupt.".to_string())?;
            if cached.lastfm_username != session.lastfm_username {
                return Err(
                    "An acknowledged Last.fm import page belongs to another Last.fm account."
                        .into(),
                );
            }
            scrobbles.extend(
                cached
                    .parsed
                    .tracks
                    .into_iter()
                    .filter(|scrobble| scrobble.timestamp < session.history_to),
            );
        }
        Ok(scrobbles)
    }

    fn remove_snapshot(&self, cache_id: &str) {
        let _ = fs::remove_dir_all(self.cache_path(cache_id));
    }

    fn quarantine_snapshot(&self, cache_id: &str) -> Result<(), String> {
        let path = self.cache_path(cache_id);
        if !path.exists() {
            return Ok(());
        }
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        fs::rename(
            &path,
            path.with_file_name(format!("{cache_id}.quarantine-{stamp}")),
        )
        .map_err(|_| "Could not quarantine the Last.fm import cache.".to_string())
    }

    fn quarantine_cache_root(&self) -> Result<(), String> {
        if !self.cache_root.exists() {
            return Ok(());
        }
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        fs::rename(
            &self.cache_root,
            self.cache_root
                .with_file_name(format!("lastfm-import-cache.quarantine-{stamp}")),
        )
        .map_err(|_| "Could not quarantine the Last.fm import cache.".to_string())
    }

    fn quarantine(&self) -> Result<(), String> {
        quarantine_file(&self.path, "the Last.fm import session").map(|_| ())
    }
}

fn validate_session_cursor(session: &LastFmImportSessionV2) -> Result<(), String> {
    match session.total_pages {
        None if session.next_page == 1 && session.downloaded_pages == 0 => Ok(()),
        None => Err("The Last.fm import cursor has no valid page total.".into()),
        Some(0) if session.next_page == 0 && session.downloaded_pages == 0 => Ok(()),
        Some(total_pages)
            if session.downloaded_pages <= total_pages
                && session.next_page == total_pages.saturating_sub(session.downloaded_pages) =>
        {
            Ok(())
        }
        Some(_) => Err("The Last.fm import cursor is inconsistent with its page total.".into()),
    }
}

fn suspended_source_phase(session: &LastFmImportSessionV2) -> bool {
    session.phase == ImportPhase::Suspended && session.rows.is_empty()
}

fn requires_spotify_ownership(session: &LastFmImportSessionV2) -> bool {
    session.spotify_account_id.is_some()
        && matches!(
            session.phase,
            ImportPhase::Review | ImportPhase::Done | ImportPhase::Suspended
        )
}

pub(crate) struct Service {
    store: ImportSessionStore,
    incremental_store: IncrementalStore,
    mappings_store: MappingsStore,
    session: Mutex<Option<LastFmImportSessionV2>>,
    sync_state: Mutex<LastFmSyncState>,
    mappings: Mutex<PersistedLastFmMappings>,
    reconciliation_lock: Mutex<()>,
    lazy_match_lock: Mutex<()>,
    running: AtomicBool,
    sync_running: AtomicBool,
}

impl Service {
    pub(crate) fn new(app_data_dir: impl AsRef<Path>) -> Arc<Self> {
        let app_data_dir = app_data_dir.as_ref().to_path_buf();
        let store = ImportSessionStore::new(&app_data_dir);
        let incremental_store = IncrementalStore::new(&app_data_dir);
        let mappings_store = MappingsStore::new(&app_data_dir);
        let mut load_problems = Vec::new();
        let session = match store.load() {
            Ok(session) => session,
            Err(error) => {
                log::warn!("Last.fm importer state is unavailable: {error}");
                None
            }
        };
        let mut sync_state = match incremental_store.load() {
            Ok(state) => state,
            Err(error) => {
                log::warn!("Last.fm incremental sync state is unavailable: {error}");
                load_problems.push(error);
                LastFmSyncState {
                    version: LASTFM_SYNC_VERSION,
                    ..LastFmSyncState::default()
                }
            }
        };
        let mappings = match mappings_store.load() {
            Ok(mappings) => mappings,
            Err(error) => {
                log::warn!("Last.fm mappings are unavailable: {error}");
                load_problems.push(error);
                PersistedLastFmMappings {
                    version: LASTFM_MAPPINGS_VERSION,
                    ..PersistedLastFmMappings::default()
                }
            }
        };
        if !load_problems.is_empty() {
            let problem = load_problems.join(" ");
            sync_state.sync_problem = Some(match sync_state.sync_problem.take() {
                Some(existing) => format!("{existing} {problem}"),
                None => problem,
            });
        }
        Arc::new(Self {
            store,
            incremental_store,
            mappings_store,
            session: Mutex::new(session),
            sync_state: Mutex::new(sync_state),
            mappings: Mutex::new(mappings),
            reconciliation_lock: Mutex::new(()),
            lazy_match_lock: Mutex::new(()),
            running: AtomicBool::new(false),
            sync_running: AtomicBool::new(false),
        })
    }

    pub(crate) async fn state(&self) -> ImportStateView {
        let session = self.session.lock().await;
        let mut view = match session.as_ref() {
            Some(session) if session.phase == ImportPhase::Suspended => suspended_state_view(),
            Some(session) => state_view(Some(session)),
            None => state_view(None),
        };
        let sync = self.sync_state.lock().await;
        view.syncing = self.sync_running.load(Ordering::Acquire);
        view.last_synced_at = sync.last_synced_at;
        view.pending_review = sync.backlog.len();
        view.sync_problem = sync.sync_problem.clone();
        view
    }

    async fn snapshot(&self) -> Option<LastFmImportSessionV2> {
        self.session.lock().await.clone()
    }

    async fn sync_snapshot(&self) -> LastFmSyncState {
        self.sync_state.lock().await.clone()
    }

    async fn persist_sync(&self, state: LastFmSyncState) -> Result<(), String> {
        let store = self.incremental_store.clone();
        tauri::async_runtime::spawn_blocking(move || store.save(&state))
            .await
            .map_err(|_| "Last.fm incremental sync persistence task stopped.".to_string())?
    }

    async fn mutate_sync<R, F>(&self, mutation: F) -> Result<R, String>
    where
        F: FnOnce(&mut LastFmSyncState) -> Result<R, String>,
    {
        let mut current = self.sync_state.lock().await;
        let mut next = current.clone();
        let result = mutation(&mut next)?;
        next.version = LASTFM_SYNC_VERSION;
        self.persist_sync(next.clone()).await?;
        *current = next;
        Ok(result)
    }

    async fn mappings_for(
        &self,
        lastfm_username: &str,
        spotify_account_id: Option<&str>,
    ) -> LastFmMappings {
        let mut mappings = self.mappings.lock().await;
        if mappings.lastfm_username.as_deref() != Some(lastfm_username)
            || mappings.spotify_account_id.as_deref() != spotify_account_id
        {
            return LastFmMappings::default();
        }
        if mappings.dormant {
            let mut active = mappings.clone();
            active.dormant = false;
            let store = self.mappings_store.clone();
            let persisted = active.clone();
            if let Err(error) = tauri::async_runtime::spawn_blocking(move || store.save(&persisted))
                .await
                .map_err(|_| "Last.fm mappings activation task stopped.".to_string())
                .and_then(|result| result)
            {
                log::warn!("Last.fm mappings remain dormant: {error}");
                return LastFmMappings::default();
            }
            *mappings = active;
        }
        mappings.mappings.clone()
    }

    async fn save_mappings_for(
        &self,
        lastfm_username: &str,
        spotify_account_id: Option<&str>,
        mappings: LastFmMappings,
    ) -> Result<(), String> {
        let mut current = self.mappings.lock().await;
        if current
            .lastfm_username
            .as_deref()
            .is_some_and(|existing| existing != lastfm_username)
            || current
                .spotify_account_id
                .as_deref()
                .is_some_and(|existing| Some(existing) != spotify_account_id)
        {
            return Err("Last.fm mappings belong to another account and are dormant.".into());
        }
        let next = PersistedLastFmMappings {
            version: LASTFM_MAPPINGS_VERSION,
            lastfm_username: Some(lastfm_username.to_owned()),
            spotify_account_id: spotify_account_id.map(ToOwned::to_owned),
            dormant: false,
            mappings,
        };
        let store = self.mappings_store.clone();
        let persisted = next.clone();
        tauri::async_runtime::spawn_blocking(move || store.save(&persisted))
            .await
            .map_err(|_| "Last.fm mappings persistence task stopped.".to_string())??;
        *current = next;
        Ok(())
    }

    pub(crate) async fn export_mappings(&self) -> PersistedLastFmMappings {
        self.mappings.lock().await.clone()
    }

    pub(crate) async fn restore_mappings(
        &self,
        imported: PersistedLastFmMappings,
    ) -> Result<(), String> {
        if imported.version != LASTFM_MAPPINGS_VERSION {
            return Err("The Last.fm mappings version is unsupported.".into());
        }
        let next = PersistedLastFmMappings {
            version: LASTFM_MAPPINGS_VERSION,
            lastfm_username: imported.lastfm_username,
            spotify_account_id: imported.spotify_account_id,
            dormant: true,
            mappings: imported.mappings,
        };
        let store = self.mappings_store.clone();
        let persisted = next.clone();
        tauri::async_runtime::spawn_blocking(move || store.save(&persisted))
            .await
            .map_err(|_| "Last.fm mappings persistence task stopped.".to_string())??;
        *self.mappings.lock().await = next;
        Ok(())
    }

    pub(crate) async fn backfill_completed_mappings(&self) -> Result<(), String> {
        let Some(session) = self.snapshot().await else {
            return Ok(());
        };
        if !review_phase_allowed(session.phase) || session.spotify_account_id.is_none() {
            return Ok(());
        }
        let username = session.lastfm_username.clone();
        let spotify_account_id = session.spotify_account_id.clone();
        let mut mappings = self
            .mappings_for(&username, spotify_account_id.as_deref())
            .await;
        let before = mappings.clone();
        for row in &session.rows {
            let source_key = session
                .incremental_source_keys
                .get(&row.stable_id)
                .cloned()
                .unwrap_or_else(|| row.stable_id.clone());
            let decision = default_decision(&session, &row.stable_id);
            if decision.excluded {
                mappings.excluded_tracks.insert(source_key.clone());
                continue;
            }
            match decision.status {
                RowStatus::IgnoredAlbum => {
                    mappings
                        .ignored_albums
                        .insert(source_album_key(&row.artist, &row.album));
                    continue;
                }
                RowStatus::IgnoredArtist => {
                    mappings
                        .ignored_artists
                        .insert(normalize_for_match(&row.artist));
                    continue;
                }
                RowStatus::Done => {}
                RowStatus::Pending | RowStatus::Skipped => continue,
            }
            let Some(result) = session.matches.get(&row.stable_id) else {
                continue;
            };
            let Some(track_uri) = matched_track_uri_for_row(result, row) else {
                continue;
            };
            mappings
                .track_mappings
                .insert(source_key, track_uri.clone());
            if let Some(album_uri) = result
                .selected_uri
                .as_deref()
                .filter(|uri| uri.starts_with("spotify:album:"))
            {
                let album = mappings
                    .album_mappings
                    .entry(source_album_key(&row.artist, &row.album))
                    .or_default();
                album.spotify_album_uri = album_uri.to_owned();
                album
                    .track_uris_by_name
                    .insert(normalize_for_match(&row.track), track_uri);
            }
        }
        if mappings != before {
            self.save_mappings_for(&username, spotify_account_id.as_deref(), mappings)
                .await?;
        }
        Ok(())
    }

    async fn sync_backlog_into_review(
        &self,
        username: &str,
        spotify_account_id: Option<&str>,
    ) -> Result<(), String> {
        let backlog = self.sync_snapshot().await.backlog;
        if backlog.is_empty() && self.snapshot().await.is_none() {
            return Ok(());
        }
        self.mutate_session(|current| {
            let mut session = match current {
                Some(session) if session.lastfm_username != username => {
                    return Ok((Some(session), ()))
                }
                Some(session) if !review_phase_allowed(session.phase) => {
                    return Ok((Some(session), ()))
                }
                Some(session) => session,
                None => {
                    let mut session = LastFmImportSessionV2::new_with_defaults(
                        username.to_owned(),
                        crate::unix_now(),
                        ImportDefaults::default(),
                    );
                    session.spotify_account_id = spotify_account_id.map(ToOwned::to_owned);
                    session.phase = ImportPhase::Review;
                    session
                }
            };
            if session.spotify_account_id.is_none() {
                session.spotify_account_id = spotify_account_id.map(ToOwned::to_owned);
            } else if session.spotify_account_id.as_deref() != spotify_account_id {
                return Ok((Some(session), ()));
            }
            let existing_incremental_keys = session.incremental_source_keys.clone();
            session
                .rows
                .retain(|row| !existing_incremental_keys.contains_key(&row.stable_id));
            session.incremental_source_keys.clear();
            aggregate_incremental_scrobbles(
                &mut session.rows,
                &mut session.incremental_source_keys,
                &backlog,
            );
            let row_ids = session
                .rows
                .iter()
                .map(|row| row.stable_id.as_str())
                .collect::<BTreeSet<_>>();
            session
                .decisions
                .retain(|id, _| row_ids.contains(id.as_str()));
            session
                .matches
                .retain(|id, _| row_ids.contains(id.as_str()));
            session.batches = build_review_batches(&session.rows);
            session.phase = if session.rows.is_empty() {
                ImportPhase::Done
            } else {
                ImportPhase::Review
            };
            Ok((Some(session), ()))
        })
        .await
    }

    async fn sweep_backlog_with_mappings(
        &self,
        state: &crate::AppState,
        username: &str,
        spotify_account_id: &str,
    ) -> Result<(), String> {
        let _reconciliation_guard = self.reconciliation_lock.lock().await;
        let before = self.sync_snapshot().await;
        if before.backlog.is_empty() || before.active.is_some() {
            return self
                .sync_backlog_into_review(username, Some(spotify_account_id))
                .await;
        }
        let _library_transaction = crate::begin_library_transaction(state)?;
        let available = state
            .library
            .lock()
            .expect("library mutex poisoned")
            .tracks()
            .iter()
            .map(|track| track.uri.clone())
            .collect::<BTreeSet<_>>();
        let mappings = self.mappings_for(username, Some(spotify_account_id)).await;
        let result =
            reconcile_incremental(&before.backlog, &[], &mappings, &available, 0, u64::MAX);
        let (before_library, after_library) = {
            let library = state.library.lock().expect("library mutex poisoned");
            let before = library.clone();
            let mut after = before.clone();
            apply_incremental_updates(&mut after, &result.increments, &result.latest);
            (before, after)
        };
        let journal = LastFmApplicationJournal {
            before_library,
            after_library,
            checkpoint_before: before.synced_through,
            checkpoint_after: before.synced_through,
            backlog_before: before.backlog.clone(),
            backlog_after: result.unresolved.clone(),
            consumed_receipts: Vec::new(),
        };
        self.mutate_sync(|state| {
            if state.active.is_some() || state.backlog != before.backlog {
                return Err("Last.fm review backlog changed before applying mappings.".into());
            }
            state.journal = Some(journal.clone());
            Ok(())
        })
        .await?;
        if !result.increments.is_empty() {
            crate::mutate_library_in_transaction(state, |library| {
                apply_incremental_updates(library, &result.increments, &result.latest);
                Ok(())
            })?;
        }
        self.mutate_sync(|state| {
            state.backlog = result.unresolved.clone();
            state.journal = None;
            state.sync_problem = None;
            Ok(())
        })
        .await?;
        self.sync_backlog_into_review(username, Some(spotify_account_id))
            .await
    }

    fn claim_sync_runner(&self) -> bool {
        !self.sync_running.swap(true, Ordering::AcqRel)
    }

    fn release_sync_runner(&self) {
        self.sync_running.store(false, Ordering::Release);
    }

    pub(crate) async fn clear_sync_state(&self) -> Result<(), String> {
        let active = self
            .sync_snapshot()
            .await
            .active
            .map(|range| range.cache_id);
        self.mutate_sync(|state| {
            *state = LastFmSyncState {
                version: LASTFM_SYNC_VERSION,
                ..LastFmSyncState::default()
            };
            Ok(())
        })
        .await?;
        if let Some(cache_id) = active {
            self.store.remove_snapshot(&cache_id);
        }
        Ok(())
    }

    async fn persist(&self, session: LastFmImportSessionV2) -> Result<(), String> {
        let store = self.store.clone();
        tauri::async_runtime::spawn_blocking(move || store.save(&session))
            .await
            .map_err(|_| "Last.fm import persistence task stopped.".to_string())?
    }

    #[cfg(test)]
    async fn save(&self, session: LastFmImportSessionV2) -> Result<(), String> {
        self.mutate_session(|_| Ok((Some(session), ()))).await
    }

    async fn mutate_session<R, F>(&self, mutation: F) -> Result<R, String>
    where
        F: FnOnce(
            Option<LastFmImportSessionV2>,
        ) -> Result<(Option<LastFmImportSessionV2>, R), String>,
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
        F: FnOnce(LastFmImportSessionV2) -> Result<(LastFmImportSessionV2, R), String>,
    {
        self.mutate_session(|session| {
            let Some(mut session) = session else {
                return Err("No Last.fm import session is active.".into());
            };
            if session.lastfm_username != username
                || session
                    .spotify_account_id
                    .as_deref()
                    .is_some_and(|bound| bound != spotify_account_id)
                || !allowed_phase(session.phase)
            {
                return Err(
                    "The Last.fm import is no longer active for this account or phase.".into(),
                );
            }
            if session.spotify_account_id.is_none() {
                session.spotify_account_id = Some(spotify_account_id.to_owned());
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
        history_to: u64,
        defaults: Option<ImportDefaults>,
    ) -> Result<ImportStateView, String> {
        if let Some(defaults) = &defaults {
            defaults.validate()?;
        }
        if let Some(session) = self.snapshot().await {
            if suspended_source_phase(&session) {
                let store = self.store.clone();
                let validation_session = session.clone();
                let cache_valid = tauri::async_runtime::spawn_blocking(move || {
                    store.validate_cache(&validation_session).is_ok()
                })
                .await
                .map_err(|_| "Last.fm import cache validation task stopped.".to_string())?;
                if !cache_valid {
                    let mut current = self.session.lock().await;
                    let same_source = current.as_ref().is_some_and(|current| {
                        suspended_source_phase(current)
                            && current.cache_id == session.cache_id
                            && current.lastfm_username == session.lastfm_username
                            && current.history_to == session.history_to
                    });
                    if same_source {
                        let store = self.store.clone();
                        let cache_id = session.cache_id.clone();
                        tauri::async_runtime::spawn_blocking(move || {
                            store.quarantine_snapshot(&cache_id)?;
                            store.quarantine()
                        })
                        .await
                        .map_err(|_| "Last.fm import quarantine task stopped.".to_string())??;
                        *current = None;
                    }
                }
            }
        }
        let result = self
            .mutate_session(|current| {
                let session = match current {
                    Some(mut session) => {
                        if session.lastfm_username != username {
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
                                .is_some_and(|total_pages| {
                                    total_pages == 0 || session.downloaded_pages >= total_pages
                                })
                            {
                                if session.rows.is_empty() {
                                    ImportPhase::Aggregating
                                } else {
                                    ImportPhase::Review
                                }
                            } else {
                                ImportPhase::Downloading
                            };
                            session.retryable_error = None;
                        }
                        session
                    }
                    None => {
                        LastFmImportSessionV2::new_with_defaults(
                            username.to_owned(),
                            history_to,
                            defaults.unwrap_or_default(),
                        )
                    }
                };
                let view = state_view(Some(&session));
                Ok((Some(session), Ok(view)))
            })
            .await?;
        result
    }

    async fn set_metadata(
        &self,
        total_pages: u32,
        total_scrobbles: u64,
    ) -> Result<ImportStateView, String> {
        self.mutate_session(|current| {
            let Some(mut session) = current else {
                return Err("No Last.fm import session is active.".into());
            };
            if session.phase != ImportPhase::Downloading {
                return Ok((Some(session.clone()), state_view(Some(&session))));
            }
            if let Some(existing) = session.total_pages {
                if existing != total_pages {
                    return Err("Last.fm import metadata changed during the snapshot.".into());
                }
                return Ok((Some(session.clone()), state_view(Some(&session))));
            }
            session.total_pages = Some(total_pages);
            session.total_scrobbles = total_scrobbles;
            session.next_page = total_pages;
            session.retryable_error = None;
            if total_pages == 0 {
                session.phase = ImportPhase::Aggregating;
            }
            Ok((Some(session.clone()), state_view(Some(&session))))
        })
        .await
    }

    async fn checkpoint_page(
        &self,
        page: u32,
        parsed: &ParsedRecentTracksPage,
    ) -> Result<ImportStateView, String> {
        let Some(before) = self.snapshot().await else {
            return Err("No Last.fm import session is active.".into());
        };
        if before.phase != ImportPhase::Downloading {
            return Ok(state_view(Some(&before)));
        }
        if parsed.page != page {
            return Err(format!(
                "Last.fm response was for page {}, expected page {page}.",
                parsed.page
            ));
        }
        let total_pages = before
            .total_pages
            .or(parsed.total_pages)
            .ok_or_else(|| "Last.fm import metadata is not available yet.".to_string())?;
        if parsed.total_pages.is_some_and(|value| value != total_pages) {
            return Err("Last.fm page metadata changed during the snapshot.".into());
        }
        let expected_page = if before.next_page == 0 {
            page
        } else {
            before.next_page
        };
        if page != expected_page {
            if page < expected_page {
                return Err("Last.fm import pages must be checkpointed sequentially.".into());
            }
            return Ok(state_view(Some(&before)));
        }
        let mut cache_session = before.clone();
        cache_session.total_pages = Some(total_pages);
        let mut filtered = parsed.clone();
        discard_post_cutoff(&mut filtered, before.history_to);
        let store = self.store.clone();
        let cached_page = filtered.clone();
        tauri::async_runtime::spawn_blocking(move || {
            store.write_page(&cache_session, &cached_page)
        })
        .await
        .map_err(|_| "Last.fm import cache task stopped.".to_string())??;
        let result = self
            .mutate_session(|current| {
                let Some(mut session) = current else {
                    return Err("No Last.fm import session is active.".into());
                };
                if session.phase != ImportPhase::Downloading {
                    return Ok((Some(session.clone()), state_view(Some(&session))));
                }
                if session.next_page != 0 && session.next_page != page {
                    return Err("Last.fm import cursor changed before page acknowledgement.".into());
                }
                session.total_pages = Some(total_pages);
                session.total_scrobbles = filtered.total.unwrap_or(session.total_scrobbles);
                session.included_scrobbles = session
                    .included_scrobbles
                    .saturating_add(filtered.tracks.len() as u64);
                session.skipped_now_playing = session
                    .skipped_now_playing
                    .saturating_add(filtered.skipped_now_playing);
                session.skipped_undated = session
                    .skipped_undated
                    .saturating_add(filtered.skipped_undated);
                if let Some(latest) = filtered
                    .tracks
                    .iter()
                    .map(|scrobble| scrobble.timestamp)
                    .filter(|timestamp| *timestamp > 0)
                    .max()
                {
                    session.downloaded_through = Some(
                        session
                            .downloaded_through
                            .map_or(latest, |current| current.max(latest)),
                    );
                }
                session.downloaded_pages = session.downloaded_pages.saturating_add(1);
                session.next_page = page.saturating_sub(1);
                if session.downloaded_pages >= total_pages {
                    session.next_page = 0;
                    session.phase = ImportPhase::Aggregating;
                }
                session.retryable_error = None;
                Ok((Some(session.clone()), state_view(Some(&session))))
            })
            .await?;
        Ok(result)
    }

    async fn checkpoint_incremental_page(
        &self,
        username: &str,
        page: u32,
        parsed: ParsedRecentTracksPage,
    ) -> Result<(), String> {
        let before = self.sync_snapshot().await;
        let Some(range) = before.active.as_ref() else {
            return Err("No Last.fm incremental range is active.".into());
        };
        if before.lastfm_username.as_deref() != Some(username) {
            return Err("The Last.fm incremental account changed during download.".into());
        }
        let total_pages = range
            .total_pages
            .ok_or_else(|| "Last.fm incremental metadata is not available yet.".to_string())?;
        if parsed.page != page || page == 0 || page > total_pages {
            return Err("Last.fm incremental page metadata is invalid.".into());
        }
        if range.next_page != page {
            return Err("Last.fm incremental pages must be checkpointed sequentially.".into());
        }
        let mut filtered = parsed;
        discard_post_cutoff(&mut filtered, range.to);
        let cache_session = incremental_cache_session(&before, username)?;
        let store = self.store.clone();
        let page_for_cache = filtered.clone();
        tauri::async_runtime::spawn_blocking(move || {
            store.write_page(&cache_session, &page_for_cache)
        })
        .await
        .map_err(|_| "Last.fm incremental cache task stopped.".to_string())??;
        self.mutate_sync(|state| {
            let Some(active) = state.active.as_mut() else {
                return Err("Last.fm incremental range changed during page write.".into());
            };
            if active.cache_id != range.cache_id || active.next_page != page {
                return Err("Last.fm incremental range changed before acknowledgement.".into());
            }
            active.downloaded_pages = active.downloaded_pages.saturating_add(1);
            active.next_page = page.saturating_sub(1);
            if active.downloaded_pages >= total_pages {
                active.next_page = 0;
            }
            Ok(())
        })
        .await
    }

    async fn aggregate_cached(
        &self,
        lastfm: Option<&crate::lastfm::Service>,
    ) -> Result<ImportStateView, String> {
        let Some(session) = self.snapshot().await else {
            return Err("No Last.fm import session is active.".into());
        };
        if session.phase != ImportPhase::Aggregating {
            return Ok(state_view(Some(&session)));
        }
        let store = self.store.clone();
        let blocking_session = session.clone();
        let aggregation = tauri::async_runtime::spawn_blocking(move || {
            let mut scrobbles = store.read_pages(&blocking_session)?;
            sort_scrobbles(&mut scrobbles);
            let mut rows = Vec::new();
            aggregate_scrobbles(&mut rows, &scrobbles);
            let batches = build_review_batches(&rows);
            Ok::<_, String>((rows, batches))
        })
        .await
        .map_err(|_| "Last.fm import aggregation task stopped.".to_string())?;
        let (rows, batches) = match aggregation {
            Ok(result) => result,
            Err(error) => {
                self.invalidate_snapshot().await?;
                return Err(error);
            }
        };
        let commit = || async {
            self.mutate_session(|current| {
                let Some(mut current) = current else {
                    return Err("No Last.fm import session is active.".into());
                };
                if current.cache_id != session.cache_id || current.phase != ImportPhase::Aggregating
                {
                    return Err("Last.fm import changed while aggregation was running.".into());
                }
                current.rows = rows;
                current.batches = batches;
                current.incremental_source_keys.clear();
                current.phase = if current.rows.is_empty() {
                    ImportPhase::Done
                } else {
                    ImportPhase::Review
                };
                current.retryable_error = None;
                Ok((Some(current.clone()), state_view(Some(&current))))
            })
            .await
        };
        let result = match lastfm {
            Some(lastfm) => match lastfm
                .with_import_owner(&session.lastfm_username, commit)
                .await?
            {
                Some(result) => result,
                None => {
                    self.suspend_for_account_mismatch().await?;
                    return Ok(self.state().await);
                }
            },
            None => commit().await?,
        };
        self.store.remove_snapshot(&session.cache_id);
        self.sync_backlog_into_review(
            &session.lastfm_username,
            session.spotify_account_id.as_deref(),
        )
        .await?;
        Ok(result)
    }

    async fn invalidate_snapshot(&self) -> Result<(), String> {
        let mut current = self.session.lock().await;
        if let Some(session) = current.as_ref() {
            self.store.quarantine_snapshot(&session.cache_id)?;
            self.store.quarantine()?;
        }
        *current = None;
        Ok(())
    }

    async fn set_retryable_error(&self, error: Option<RetryableError>) -> Result<(), String> {
        self.mutate_session(|session| {
            let Some(mut session) = session else {
                return Ok((None, ()));
            };
            if session.phase == ImportPhase::Suspended {
                return Ok((Some(session), ()));
            }
            session.retryable_error = error;
            Ok((Some(session), ()))
        })
        .await
    }

    async fn set_match(
        &self,
        username: &str,
        spotify_account_id: &str,
        batch_id: u32,
        result: MatchResult,
    ) -> Result<(), String> {
        self.set_matches(username, spotify_account_id, batch_id, vec![result])
            .await
    }

    async fn set_matches(
        &self,
        username: &str,
        spotify_account_id: &str,
        batch_id: u32,
        results: Vec<MatchResult>,
    ) -> Result<(), String> {
        self.mutate_session(|session| {
            let Some(mut session) = session else {
                return Err("No Last.fm import session is active.".into());
            };
            if session.lastfm_username != username
                || (session.spotify_account_id.is_some()
                    && session.spotify_account_id.as_deref() != Some(spotify_account_id))
                || !review_phase_allowed(session.phase)
            {
                return Err(
                    "The Last.fm import is no longer active for this account or phase.".into(),
                );
            }
            let Some(batch) = review_batches(&session)
                .into_iter()
                .find(|batch| batch.page == batch_id)
            else {
                return Err("Unknown Last.fm import review batch.".into());
            };
            if results
                .iter()
                .any(|result| !batch.source_ids.iter().any(|id| id == &result.source_id))
            {
                return Err("A match does not belong to this review batch.".into());
            }
            session.spotify_account_id = Some(spotify_account_id.to_owned());
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

    pub(crate) async fn queue_page(
        &self,
        cursor: usize,
        limit: usize,
    ) -> Result<ImportQueuePage, String> {
        if limit == 0 || limit > LASTFM_QUEUE_PAGE_LIMIT {
            return Err(format!(
                "Last.fm import queue limit must be between 1 and {LASTFM_QUEUE_PAGE_LIMIT}."
            ));
        }
        let session_guard = self.session.lock().await;
        let Some(session) = session_guard.as_ref() else {
            return Ok(ImportQueuePage {
                items: Vec::new(),
                cursor,
                next_cursor: None,
                total: 0,
            });
        };
        if session.phase == ImportPhase::Suspended {
            return Ok(ImportQueuePage {
                items: Vec::new(),
                cursor,
                next_cursor: None,
                total: 0,
            });
        }
        let batches = review_batches_for_read(session);
        let batches = batches.as_ref();
        let total = batches.len();
        if cursor > total {
            return Err("Last.fm import queue cursor is out of range.".into());
        }
        let end = cursor.saturating_add(limit).min(total);
        let requested_batches = &batches[cursor..end];
        let requested_ids = requested_batches
            .iter()
            .flat_map(|batch| batch.source_ids.iter().map(String::as_str))
            .collect::<HashSet<_>>();
        let rows_by_id = session
            .rows
            .iter()
            .filter(|row| requested_ids.contains(row.stable_id.as_str()))
            .map(|row| (row.stable_id.as_str(), row))
            .collect::<HashMap<_, _>>();
        let items = requested_batches
            .iter()
            .filter_map(|batch| {
                let rows = batch_rows(batch, &rows_by_id);
                queue_item(session, batch, &rows)
            })
            .collect();
        Ok(ImportQueuePage {
            items,
            cursor,
            next_cursor: (end < total).then_some(end),
            total,
        })
    }

    pub(crate) async fn page(
        &self,
        batch_id: u32,
        artist: &str,
        album: &str,
    ) -> Option<ImportPageView> {
        let session_guard = self.session.lock().await;
        let session = session_guard.as_ref()?;
        if session.phase == ImportPhase::Suspended {
            return None;
        }
        let batches = review_batches_for_read(session);
        let batch = batches.iter().find(|batch| batch.page == batch_id)?;
        let requested_ids = batch
            .source_ids
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        let rows_by_id = session
            .rows
            .iter()
            .filter(|row| requested_ids.contains(row.stable_id.as_str()))
            .map(|row| (row.stable_id.as_str(), row))
            .collect::<HashMap<_, _>>();
        let rows = batch_rows(batch, &rows_by_id);
        if rows.len() != batch.source_ids.len()
            || rows
                .iter()
                .any(|row| row.artist != artist || row.album != album)
        {
            return None;
        }
        let page_number = batches
            .iter()
            .position(|candidate| candidate.page == batch_id)?
            + 1;
        let options = session.options_for_page_batch(batch, artist, album, &rows);
        let items = rows
            .iter()
            .map(|row| ImportPageItem {
                source: (*row).clone(),
                decision: default_decision(session, &row.stable_id),
                match_result: session.matches.get(&row.stable_id).cloned(),
            })
            .collect();
        let mut fuzzy_groups = BTreeMap::<String, Vec<SourceRow>>::new();
        for row in &rows {
            let decision = default_decision(session, &row.stable_id);
            let participates = !decision.excluded
                && match decision.status {
                    RowStatus::Done => true,
                    RowStatus::Pending | RowStatus::Skipped => {
                        options.selected_track_ids.contains(&row.stable_id)
                    }
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
                .push((*row).clone());
        }
        fuzzy_groups
            .retain(|_, rows| rows.len() > 1 || rows.iter().any(|row| row.variants.len() > 1));
        let visible_targets = fuzzy_groups.keys().cloned().collect::<BTreeSet<_>>();
        let count_modes = session
            .count_modes
            .iter()
            .filter(|(target, _)| visible_targets.contains(*target))
            .map(|(target, mode)| (target.clone(), *mode))
            .collect();
        let locked_count_modes = locked_count_modes(session)
            .into_iter()
            .filter(|target| visible_targets.contains(target))
            .collect();
        Some(ImportPageView {
            state: state_view(Some(session)),
            batch_id,
            artist: artist.to_owned(),
            album: album.to_owned(),
            page_number,
            page_count: batches.len(),
            rows: items,
            options,
            fuzzy_groups,
            count_modes,
            locked_count_modes,
        })
    }

    async fn update_options(
        &self,
        username: &str,
        spotify_account_id: &str,
        batch_id: u32,
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
                if requested_batch(&session, batch_id, artist, album).is_none() {
                    return Err("Unknown Last.fm import review batch.".into());
                }
                session
                    .page_options
                    .insert(batch_options_key(batch_id), options);
                Ok((session, ()))
            },
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn review_action(
        &self,
        username: &str,
        spotify_account_id: &str,
        batch_id: u32,
        id: Option<&str>,
        action: &str,
        artist: &str,
        album: &str,
    ) -> Result<(), String> {
        self.mutate_owned_session(
            username,
            spotify_account_id,
            review_phase_allowed,
            |mut session| {
                let Some(batch) = requested_batch(&session, batch_id, artist, album) else {
                    return Err("Unknown Last.fm import review batch.".into());
                };
                match action {
                    "exclude" | "undo-exclude" => {
                        let id = id.ok_or_else(|| {
                            "A source row ID is required for this action.".to_string()
                        })?;
                        if !batch.source_ids.iter().any(|source_id| source_id == id) {
                            return Err(
                                "The source row does not belong to this review batch.".into()
                            );
                        }
                        exclude_row(&mut session, id, action == "exclude");
                    }
                    "ignore-album" => {
                        for source_id in album_source_ids(&session, artist, album) {
                            if is_actionable(&session, &source_id) {
                                session.decisions.insert(
                                    source_id,
                                    RowDecision {
                                        status: RowStatus::IgnoredAlbum,
                                        excluded: false,
                                    },
                                );
                            }
                        }
                    }
                    "ignore-artist" => ignore_artist(&mut session, artist),
                    "skip-album" => {
                        for source_id in album_source_ids(&session, artist, album) {
                            if is_actionable(&session, &source_id) {
                                session.decisions.insert(
                                    source_id,
                                    RowDecision {
                                        status: RowStatus::Skipped,
                                        excluded: false,
                                    },
                                );
                            }
                        }
                    }
                    "restore" => {
                        for source_id in album_source_ids(&session, artist, album) {
                            let decision = default_decision(&session, &source_id);
                            if !decision.excluded
                                && matches!(
                                    decision.status,
                                    RowStatus::IgnoredAlbum | RowStatus::Skipped
                                )
                            {
                                session.decisions.insert(source_id, RowDecision::default());
                            }
                        }
                    }
                    _ => return Err("Unknown Last.fm import review action.".into()),
                }
                update_review_phase(&mut session);
                Ok((session, ()))
            },
        )
        .await?;
        let mapping_track_id = if let Some(id) = id {
            self.snapshot()
                .await
                .and_then(|session| session.incremental_source_keys.get(id).cloned())
                .or_else(|| Some(id.to_owned()))
        } else {
            None
        };
        let mut mappings = self.mappings_for(username, Some(spotify_account_id)).await;
        match action {
            "exclude" => {
                if let Some(id) = mapping_track_id.as_deref() {
                    mappings.excluded_tracks.insert(id.to_owned());
                }
            }
            "undo-exclude" => {
                if let Some(id) = mapping_track_id.as_deref() {
                    mappings.excluded_tracks.remove(id);
                }
            }
            "ignore-album" => {
                mappings
                    .ignored_albums
                    .insert(source_album_key(artist, album));
            }
            "restore" => {
                mappings
                    .ignored_albums
                    .remove(&source_album_key(artist, album));
            }
            "ignore-artist" => {
                mappings.ignored_artists.insert(normalize_for_match(artist));
            }
            _ => {}
        }
        if matches!(
            action,
            "exclude" | "undo-exclude" | "ignore-album" | "restore" | "ignore-artist"
        ) {
            self.save_mappings_for(username, Some(spotify_account_id), mappings)
                .await?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn commit_rows(
        &self,
        username: &str,
        spotify_account_id: &str,
        batch_id: u32,
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
                let Some(batch) = requested_batch(&session, batch_id, artist, album) else {
                    return Err("Unknown Last.fm import review batch.".into());
                };
                if ids
                    .iter()
                    .any(|id| !batch.source_ids.iter().any(|source_id| source_id == id))
                {
                    return Err(
                        "A selected source row does not belong to this review batch.".into(),
                    );
                }
                session
                    .page_options
                    .insert(batch_options_key(batch_id), options);
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
        batch_id: u32,
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
                let Some(batch) = requested_batch(&session, batch_id, &row_artist, &row_album)
                else {
                    return Err("The source row does not belong to this review batch.".into());
                };
                let batch_ids = batch.source_ids.iter().cloned().collect::<BTreeSet<_>>();
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
                        .filter(|row| batch_ids.contains(&row.stable_id))
                        .map(|row| (row.stable_id.clone(), row.track.clone()))
                        .collect::<Vec<_>>();
                    let mut group_track_matches = BTreeMap::new();
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
                        if let Some(uri) = result.track_matches.get(&id) {
                            group_track_matches.insert(id, uri.clone());
                        }
                    }
                    for row in session
                        .rows
                        .iter()
                        .filter(|row| batch_ids.contains(&row.stable_id))
                    {
                        if let Some(result) = session.matches.get_mut(&row.stable_id) {
                            result.track_matches = group_track_matches.clone();
                        }
                    }
                } else {
                    let row_track = session
                        .rows
                        .iter()
                        .find(|row| row.stable_id == source_id)
                        .map(|row| row.track.clone())
                        .ok_or_else(|| "Unknown Last.fm import source row.".to_string())?;
                    let album_uri = session
                        .matches
                        .get(source_id)
                        .and_then(|result| result.selected_uri.as_deref())
                        .filter(|uri| uri.starts_with("spotify:album:"))
                        .map(str::to_owned);
                    if let Some(album_uri) = album_uri {
                        let related_ids = session
                            .rows
                            .iter()
                            .filter(|row| batch_ids.contains(&row.stable_id))
                            .map(|row| row.stable_id.clone())
                            .collect::<BTreeSet<_>>();
                        let mut group_track_matches = BTreeMap::new();
                        for id in &related_ids {
                            if let Some(result) = session.matches.get(id) {
                                for (mapped_id, mapped_uri) in &result.track_matches {
                                    if related_ids.contains(mapped_id) {
                                        group_track_matches
                                            .insert(mapped_id.clone(), mapped_uri.clone());
                                    }
                                }
                            }
                        }
                        group_track_matches.insert(source_id.to_owned(), candidate.uri.clone());
                        for id in related_ids {
                            if let Some(result) = session.matches.get_mut(&id) {
                                if result.selected_uri.as_deref() == Some(album_uri.as_str()) {
                                    result.track_matches = group_track_matches.clone();
                                }
                            }
                        }
                    } else if let Some(result) = session.matches.get_mut(source_id) {
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

fn state_view(session: Option<&LastFmImportSessionV2>) -> ImportStateView {
    let processed_scrobbles = session
        .map(|session| {
            session
                .included_scrobbles
                .saturating_add(session.skipped_now_playing)
                .saturating_add(session.skipped_undated)
        })
        .unwrap_or_default();
    ImportStateView {
        phase: session.map(|session| session.phase),
        username: session.map(|session| session.lastfm_username.clone()),
        spotify_account_id: session.and_then(|session| session.spotify_account_id.clone()),
        history_to: session.map(|session| session.history_to),
        downloaded_through: session.and_then(|session| session.downloaded_through),
        next_page: session.map(|session| session.next_page).unwrap_or(1),
        total_pages: session.and_then(|session| session.total_pages),
        downloaded_pages: session
            .map(|session| session.downloaded_pages)
            .unwrap_or_default(),
        total_scrobbles: session
            .map(|session| session.total_scrobbles)
            .unwrap_or_default(),
        included_scrobbles: session
            .map(|session| session.included_scrobbles)
            .unwrap_or_default(),
        processed_scrobbles,
        defaults: session
            .map(|session| session.defaults.clone())
            .unwrap_or_default(),
        remaining: session
            .filter(|session| matches!(session.phase, ImportPhase::Review | ImportPhase::Done))
            .map(LastFmImportSessionV2::remaining)
            .unwrap_or_default(),
        retryable_error: session.and_then(|session| session.retryable_error.clone()),
        search_terms: session.map(|session| session.search_terms).unwrap_or(true),
        syncing: false,
        last_synced_at: None,
        pending_review: 0,
        sync_problem: None,
    }
}

fn suspended_state_view() -> ImportStateView {
    ImportStateView {
        phase: Some(ImportPhase::Suspended),
        username: None,
        spotify_account_id: None,
        history_to: None,
        downloaded_through: None,
        next_page: 1,
        total_pages: None,
        downloaded_pages: 0,
        total_scrobbles: 0,
        included_scrobbles: 0,
        processed_scrobbles: 0,
        defaults: ImportDefaults::default(),
        remaining: 0,
        retryable_error: Some(RetryableError {
            message: "This import is suspended because the connected account changed. Reconnect Last.fm and Spotify to resume.".into(),
            attempt: 0,
            retryable: false,
        }),
        search_terms: true,
        syncing: false,
        last_synced_at: None,
        pending_review: 0,
        sync_problem: None,
    }
}

async fn lastfm_username(app: &tauri::AppHandle) -> Result<String, String> {
    app.state::<crate::AppState>()
        .lastfm
        .state()
        .await
        .username
        .ok_or_else(|| "Connect Last.fm before importing its history.".to_string())
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
    if !session_account_matches(&session, &username, &spotify_account_id, true) {
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
    match lastfm_username(app).await {
        Ok(username) if username == session.lastfm_username => {
            if session.phase == ImportPhase::Suspended {
                if requires_spotify_ownership(&session) {
                    let _ = current_spotify_binding_is_current(app, service, true).await?;
                }
                return Ok(false);
            }
            if requires_spotify_ownership(&session) {
                current_spotify_binding_is_current(app, service, false).await
            } else {
                Ok(true)
            }
        }
        Ok(_) | Err(_) => {
            service.suspend_for_account_mismatch().await?;
            Ok(false)
        }
    }
}

async fn current_spotify_binding_is_current(
    app: &tauri::AppHandle,
    service: &Service,
    allow_suspended: bool,
) -> Result<bool, String> {
    let Some(session) = service.snapshot().await else {
        return Ok(false);
    };
    if session.spotify_account_id.is_none() {
        return Ok(true);
    }
    let state = app.state::<crate::AppState>();
    let _membership_guard = state.spotify_library_gate.lock().await;
    let expected = session.spotify_account_id.as_deref().unwrap_or_default();
    let cached = state
        .spotify_library
        .lock()
        .expect("Spotify library mutex poisoned")
        .clone();
    if cached_spotify_identity_matches(expected, &cached) == Some(false) {
        service.suspend_for_account_mismatch().await?;
        return Ok(false);
    }
    let (username, spotify_account_id) = match connected_accounts_locked(&state).await {
        Ok(accounts) => accounts,
        Err(_) => {
            service.suspend_for_account_mismatch().await?;
            return Ok(false);
        }
    };
    if !session_account_matches(&session, &username, &spotify_account_id, true)
        || (session.phase == ImportPhase::Suspended && !allow_suspended)
    {
        service.suspend_for_account_mismatch().await?;
        return Ok(false);
    }
    Ok(true)
}

async fn emit_import_changed(
    app: &tauri::AppHandle,
    service: &Service,
) -> Result<ImportStateView, String> {
    let Some(session) = service.snapshot().await else {
        let view = service.state().await;
        app.emit("lastfm-import-changed", &view)
            .map_err(|error| error.to_string())?;
        return Ok(view);
    };
    let lastfm = Arc::clone(&app.state::<crate::AppState>().lastfm);
    match lastfm
        .with_import_owner(&session.lastfm_username, || async {
            let view = service.state().await;
            app.emit("lastfm-import-changed", &view)
                .map_err(|error| error.to_string())?;
            Ok(view)
        })
        .await?
    {
        Some(view) => Ok(view),
        None => {
            service.suspend_for_account_mismatch().await?;
            let view = service.state().await;
            app.emit("lastfm-import-changed", &view)
                .map_err(|error| error.to_string())?;
            Ok(view)
        }
    }
}

async fn start_import(
    app: tauri::AppHandle,
    defaults: Option<ImportDefaults>,
) -> Result<ImportStateView, String> {
    let username = lastfm_username(&app).await?;
    let history_to = crate::history_cutoff_for_import(&app, &username).await?;
    let state = app.state::<crate::AppState>();
    let service = Arc::clone(&state.lastfm_import);
    if let Some(session) = service.snapshot().await {
        if session.phase == ImportPhase::Suspended
            && requires_spotify_ownership(&session)
            && !current_spotify_binding_is_current(&app, service.as_ref(), true).await?
        {
            let view = service.state().await;
            app.emit("lastfm-import-changed", &view)
                .map_err(|error| error.to_string())?;
            return Ok(view);
        }
    }
    let view = service
        .start_or_resume(&username, history_to, defaults)
        .await?;
    app.emit("lastfm-import-changed", &view)
        .map_err(|error| error.to_string())?;
    if service.claim_runner() {
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            run_import(app, service, username).await;
        });
    }
    Ok(view)
}

pub(crate) async fn resume_persisted_import(app: tauri::AppHandle) {
    let state = app.state::<crate::AppState>();
    let service = Arc::clone(&state.lastfm_import);
    let Some(session) = service.snapshot().await else {
        return;
    };
    let Some((username, _history_to)) = startup_resume_plan(Some(&session)) else {
        return;
    };
    let live_username = if session.phase == ImportPhase::Aggregating {
        lastfm_username(&app).await.ok()
    } else {
        None
    };
    if !startup_lastfm_identity_matches(&session, live_username.as_deref()) {
        if service.suspend_for_account_mismatch().await.is_ok() {
            let view = service.state().await;
            let _ = app.emit("lastfm-import-changed", &view);
        }
        return;
    }
    if service.claim_runner() {
        run_import(app, service, username).await;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SourceRunnerStep {
    Probe,
    Page(u32),
    Aggregate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SourceWindowOutcome {
    Complete(Vec<u32>),
    Retryable,
    Suspended,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SourcePageFetchResult {
    Success(ParsedRecentTracksPage),
    AccountMismatch(String),
    Retryable(String),
    Permanent(String),
}

fn source_runner_step(session: &LastFmImportSessionV2) -> SourceRunnerStep {
    if session.total_pages.is_none() {
        SourceRunnerStep::Probe
    } else if session.next_page == 0 {
        SourceRunnerStep::Aggregate
    } else {
        SourceRunnerStep::Page(session.next_page)
    }
}

fn source_page_window(next_page: u32, total_pages: u32) -> Vec<u32> {
    if next_page == 0 || next_page > total_pages {
        return Vec::new();
    }
    (next_page.saturating_sub(LASTFM_PAGE_WINDOW_SIZE - 1).max(1)..=next_page)
        .rev()
        .collect()
}

async fn download_page_window<F, Fut>(
    service: Arc<Service>,
    next_page: u32,
    total_pages: u32,
    fetch: F,
) -> Result<SourceWindowOutcome, String>
where
    F: Fn(u32) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = SourcePageFetchResult> + Send + 'static,
{
    let checkpoint_service = Arc::clone(&service);
    download_page_window_with_checkpoint(
        service,
        next_page,
        total_pages,
        fetch,
        move |page, parsed| {
            let service = Arc::clone(&checkpoint_service);
            async move { service.checkpoint_page(page, &parsed).await.map(|_| ()) }
        },
    )
    .await
}

async fn download_page_window_with_checkpoint<F, Fut, C, CFut>(
    service: Arc<Service>,
    next_page: u32,
    total_pages: u32,
    fetch: F,
    checkpoint: C,
) -> Result<SourceWindowOutcome, String>
where
    F: Fn(u32) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = SourcePageFetchResult> + Send + 'static,
    C: Fn(u32, ParsedRecentTracksPage) -> CFut + Clone + Send + Sync + 'static,
    CFut: Future<Output = Result<(), String>> + Send,
{
    let pages = source_page_window(next_page, total_pages);
    if pages.is_empty() {
        return Err("Last.fm import page window is empty.".into());
    }
    let mut requests = pages
        .iter()
        .copied()
        .map(|page| {
            let fetch = fetch.clone();
            tokio::spawn(async move { fetch(page).await })
        })
        .collect::<Vec<_>>();
    let mut checkpointed = Vec::with_capacity(requests.len());

    for page in pages {
        let request = requests.remove(0);
        let parsed = match request.await {
            Ok(SourcePageFetchResult::Success(parsed)) => parsed,
            Ok(failure) => {
                for request in &requests {
                    request.abort();
                }
                return match failure {
                    SourcePageFetchResult::AccountMismatch(_) => {
                        service.suspend_for_account_mismatch().await?;
                        Ok(SourceWindowOutcome::Suspended)
                    }
                    SourcePageFetchResult::Retryable(message) => {
                        let attempt = service
                            .snapshot()
                            .await
                            .and_then(|session| session.retryable_error)
                            .filter(|error| error.retryable)
                            .map(|error| error.attempt.saturating_add(1))
                            .unwrap_or(1);
                        service
                            .set_retryable_error(Some(RetryableError {
                                message,
                                attempt,
                                retryable: true,
                            }))
                            .await?;
                        Ok(SourceWindowOutcome::Retryable)
                    }
                    SourcePageFetchResult::Permanent(message) => {
                        service
                            .set_retryable_error(Some(RetryableError {
                                message: message.clone(),
                                attempt: 0,
                                retryable: false,
                            }))
                            .await?;
                        Err(message)
                    }
                    SourcePageFetchResult::Success(_) => unreachable!(),
                };
            }
            Err(error) => {
                for request in &requests {
                    request.abort();
                }
                let message = format!("Last.fm page fetch task stopped: {error}");
                service
                    .set_retryable_error(Some(RetryableError {
                        message: message.clone(),
                        attempt: 0,
                        retryable: false,
                    }))
                    .await?;
                return Err(message);
            }
        };
        if let Err(error) = checkpoint(page, parsed.clone()).await {
            for request in &requests {
                request.abort();
            }
            return Err(error);
        }
        checkpointed.push(page);
    }

    Ok(SourceWindowOutcome::Complete(checkpointed))
}

async fn fetch_source_page(
    lastfm: &crate::lastfm::Service,
    username: &str,
    page: u32,
    from: u64,
    history_to: u64,
) -> SourcePageFetchResult {
    match lastfm
        .import_recent_tracks_page(username, page, from, history_to)
        .await
    {
        Ok(payload) => match parse_recent_tracks_page(&payload) {
            Ok(parsed) => SourcePageFetchResult::Success(parsed),
            Err(message) => SourcePageFetchResult::Permanent(message),
        },
        Err(error) if error.account_mismatch => {
            SourcePageFetchResult::AccountMismatch(error.message)
        }
        Err(error) if error.retryable => SourcePageFetchResult::Retryable(error.message),
        Err(error) => SourcePageFetchResult::Permanent(error.message),
    }
}

fn startup_resume_plan(session: Option<&LastFmImportSessionV2>) -> Option<(String, u64)> {
    session
        .filter(|session| {
            matches!(
                session.phase,
                ImportPhase::Downloading | ImportPhase::Aggregating
            )
        })
        .map(|session| (session.lastfm_username.clone(), session.history_to))
}

fn startup_lastfm_identity_matches(
    session: &LastFmImportSessionV2,
    live_username: Option<&str>,
) -> bool {
    session.phase != ImportPhase::Aggregating
        || live_username == Some(session.lastfm_username.as_str())
}

async fn run_import(app: tauri::AppHandle, service: Arc<Service>, username: String) {
    let result = async {
        loop {
            let Some(session) = service.snapshot().await else {
                break;
            };
            match session.phase {
                ImportPhase::Downloading => {
                    let lastfm = Arc::clone(&app.state::<crate::AppState>().lastfm);
                    match source_runner_step(&session) {
                        SourceRunnerStep::Probe => {
                            let payload = fetch_import_page_with_retry(
                                &lastfm,
                                &service,
                                &username,
                                1,
                                session.history_to,
                            )
                            .await?;
                            let parsed = match parse_recent_tracks_page(&payload) {
                                Ok(parsed) => parsed,
                                Err(message) => {
                                    service
                                        .set_retryable_error(Some(RetryableError {
                                            message: message.clone(),
                                            attempt: 0,
                                            retryable: false,
                                        }))
                                        .await?;
                                    return Err(message);
                                }
                            };
                            let Some(total_pages) = parsed.total_pages else {
                                let message =
                                    "Last.fm metadata did not include a total page count."
                                        .to_string();
                                service
                                    .set_retryable_error(Some(RetryableError {
                                        message: message.clone(),
                                        attempt: 0,
                                        retryable: false,
                                    }))
                                    .await?;
                                return Err(message);
                            };
                            service
                                .set_metadata(total_pages, parsed.total.unwrap_or_default())
                                .await?;
                        }
                        SourceRunnerStep::Aggregate => {
                            service.aggregate_cached(Some(lastfm.as_ref())).await?;
                        }
                        SourceRunnerStep::Page(page) => {
                            let total_pages = session
                                .total_pages
                                .expect("page downloads require Last.fm metadata");
                            let window_lastfm = Arc::clone(&lastfm);
                            let window_username = username.clone();
                            let history_to = session.history_to;
                            let outcome = download_page_window(
                                Arc::clone(&service),
                                page,
                                total_pages,
                                move |page| {
                                    let lastfm = Arc::clone(&window_lastfm);
                                    let username = window_username.clone();
                                    async move {
                                        fetch_source_page(
                                            lastfm.as_ref(),
                                            &username,
                                            page,
                                            0,
                                            history_to,
                                        )
                                        .await
                                    }
                                },
                            )
                            .await?;
                            match outcome {
                                SourceWindowOutcome::Complete(_) => {}
                                SourceWindowOutcome::Retryable => {
                                    let _ = emit_import_changed(&app, &service).await;
                                    tokio::time::sleep(crate::lastfm::import_retry_delay(
                                        usize::MAX,
                                    ))
                                    .await;
                                    continue;
                                }
                                SourceWindowOutcome::Suspended => break,
                            }
                        }
                    }
                    let _ = emit_import_changed(&app, &service).await;
                }
                ImportPhase::Aggregating => {
                    let lastfm = Arc::clone(&app.state::<crate::AppState>().lastfm);
                    service.aggregate_cached(Some(lastfm.as_ref())).await?;
                }
                ImportPhase::Review | ImportPhase::Done | ImportPhase::Suspended => break,
            }
        }
        Ok::<(), String>(())
    }
    .await;
    if let Err(error) = result {
        let _ = service
            .set_retryable_error(Some(RetryableError {
                message: error,
                attempt: 0,
                retryable: false,
            }))
            .await;
    }
    service.release_runner();
    let _ = emit_import_changed(&app, &service).await;
}

async fn fetch_import_page_with_retry(
    lastfm: &crate::lastfm::Service,
    service: &Service,
    username: &str,
    page: u32,
    history_to: u64,
) -> Result<Value, String> {
    loop {
        match lastfm
            .import_recent_tracks_page(username, page, 0, history_to)
            .await
        {
            Ok(payload) => {
                service.set_retryable_error(None).await?;
                return Ok(payload);
            }
            Err(error) if error.account_mismatch => {
                service.suspend_for_account_mismatch().await?;
                return Err(error.message);
            }
            Err(error) if error.retryable => {
                let attempt = service
                    .snapshot()
                    .await
                    .and_then(|session| session.retryable_error)
                    .map(|error| error.attempt.saturating_add(1))
                    .unwrap_or(1);
                service
                    .set_retryable_error(Some(RetryableError {
                        message: error.message,
                        attempt,
                        retryable: true,
                    }))
                    .await?;
                tokio::time::sleep(crate::lastfm::import_retry_delay(usize::MAX)).await;
            }
            Err(error) => {
                service
                    .set_retryable_error(Some(RetryableError {
                        message: error.message.clone(),
                        attempt: 0,
                        retryable: false,
                    }))
                    .await?;
                return Err(error.message);
            }
        }
    }
}

fn cached_spotify_account(state: &crate::AppState) -> Option<String> {
    let library = state
        .spotify_library
        .lock()
        .expect("Spotify library mutex poisoned")
        .clone();
    library.is_exact().then_some(library.account_id)
}

async fn set_sync_problem(service: &Service, message: Option<String>) -> Result<(), String> {
    service
        .mutate_sync(|state| {
            state.sync_problem = message;
            Ok(())
        })
        .await
}

async fn fetch_incremental_page_with_retry(
    lastfm: &crate::lastfm::Service,
    service: &Service,
    username: &str,
    page: u32,
    from: u64,
    to: u64,
) -> Result<Value, String> {
    loop {
        match lastfm
            .import_recent_tracks_page(username, page, from, to)
            .await
        {
            Ok(payload) => {
                set_sync_problem(service, None).await?;
                return Ok(payload);
            }
            Err(error) if error.account_mismatch => {
                set_sync_problem(service, Some(error.message.clone())).await?;
                return Err(error.message);
            }
            Err(error) if error.retryable => {
                set_sync_problem(service, Some(error.message)).await?;
                tokio::time::sleep(crate::lastfm::import_retry_delay(usize::MAX)).await;
            }
            Err(error) => {
                set_sync_problem(service, Some(error.message.clone())).await?;
                return Err(error.message);
            }
        }
    }
}

async fn read_incremental_events(
    service: &Service,
    username: &str,
) -> Result<Vec<ExternalScrobble>, String> {
    let state = service.sync_snapshot().await;
    let Some(range) = state.active.as_ref() else {
        return Ok(Vec::new());
    };
    let session = incremental_cache_session(&state, username)?;
    let store = service.store.clone();
    let parsed = tauri::async_runtime::spawn_blocking(move || store.read_pages(&session))
        .await
        .map_err(|_| "Last.fm incremental aggregation task stopped.".to_string())??;
    let mut events = parsed
        .into_iter()
        .filter(|scrobble| scrobble.timestamp >= range.from && scrobble.timestamp < range.to)
        .map(|scrobble| ExternalScrobble {
            artist: scrobble.artist,
            album: scrobble.album,
            track: scrobble.track,
            timestamp: scrobble.timestamp,
            submitted: None,
        })
        .collect::<Vec<_>>();
    events.sort_by(|left, right| {
        left.timestamp
            .cmp(&right.timestamp)
            .then_with(|| {
                normalize_for_match(&left.artist).cmp(&normalize_for_match(&right.artist))
            })
            .then_with(|| normalize_for_match(&left.album).cmp(&normalize_for_match(&right.album)))
            .then_with(|| normalize_for_match(&left.track).cmp(&normalize_for_match(&right.track)))
    });
    Ok(events)
}

async fn apply_completed_incremental_range(
    state: &crate::AppState,
    service: &Service,
    username: &str,
) -> Result<(), String> {
    let _reconciliation_guard = service.reconciliation_lock.lock().await;
    let sync_before = service.sync_snapshot().await;
    let Some(range) = sync_before.active.as_ref() else {
        return Ok(());
    };
    if range.next_page != 0 {
        return Err("Last.fm incremental range is not complete.".into());
    }
    let mut events = sync_before.backlog.clone();
    events.extend(read_incremental_events(service, username).await?);
    let lastfm = Arc::clone(&state.lastfm);
    if lastfm.state().await.username.as_deref() != Some(username) {
        return Err("The Last.fm account changed during incremental reconciliation.".into());
    }
    let cached_spotify_account = cached_spotify_account(state);
    if sync_before
        .spotify_account_id
        .as_deref()
        .is_some_and(|expected| cached_spotify_account.as_deref() != Some(expected))
    {
        return Err("The Spotify account changed during incremental reconciliation.".into());
    }
    if sync_before.spotify_account_id.is_none() && cached_spotify_account.is_some() {
        return Err(
            "Spotify account identity became available during incremental download; retry the sync."
                .into(),
        );
    }
    lastfm.settle_before_import().await;
    let _receipt_guard = lastfm.reconciliation_guard().await;
    let receipts = lastfm.accepted_receipts().await;
    let _library_transaction = crate::begin_library_transaction(state)?;
    let available = state
        .library
        .lock()
        .expect("library mutex poisoned")
        .tracks()
        .iter()
        .map(|track| track.uri.clone())
        .collect::<BTreeSet<_>>();
    let spotify_account_id = sync_before.spotify_account_id.as_deref();
    let mappings = service.mappings_for(username, spotify_account_id).await;
    let result = reconcile_incremental(&events, &receipts, &mappings, &available, 0, u64::MAX);
    let (before_library, after_library) = {
        let library = state.library.lock().expect("library mutex poisoned");
        let before = library.clone();
        let mut after = before.clone();
        apply_incremental_updates(&mut after, &result.increments, &result.latest);
        (before, after)
    };
    let journal = LastFmApplicationJournal {
        before_library,
        after_library,
        checkpoint_before: sync_before.synced_through,
        checkpoint_after: Some(range.to),
        backlog_before: sync_before.backlog.clone(),
        backlog_after: result.unresolved.clone(),
        consumed_receipts: result.consumed_receipts.clone(),
    };
    service
        .mutate_sync(|state| {
            if state.active.as_ref().map(|active| &active.cache_id) != Some(&range.cache_id) {
                return Err("Last.fm incremental range changed before applying.".into());
            }
            state.journal = Some(journal.clone());
            Ok(())
        })
        .await?;
    if !result.increments.is_empty() {
        crate::mutate_library_in_transaction(state, |library| {
            apply_incremental_updates(library, &result.increments, &result.latest);
            Ok(())
        })?;
    }
    service
        .mutate_sync(|state| {
            state.lastfm_username = Some(username.to_owned());
            state.synced_through = Some(range.to);
            state.last_synced_at = Some(crate::unix_now());
            state.backlog = result.unresolved.clone();
            state.active = None;
            state.journal = None;
            state.sync_problem = None;
            Ok(())
        })
        .await?;
    lastfm
        .prune_accepted_receipts_locked(&result.consumed_receipts, Some(range.to))
        .await?;
    service.store.remove_snapshot(&range.cache_id);
    service
        .sync_backlog_into_review(username, sync_before.spotify_account_id.as_deref())
        .await?;
    Ok(())
}

async fn recover_pending_incremental_journal(
    state: &crate::AppState,
    service: &Service,
) -> Result<(), String> {
    let _reconciliation_guard = service.reconciliation_lock.lock().await;
    let state_before = service.sync_snapshot().await;
    let Some(journal) = state_before.journal.clone() else {
        return Ok(());
    };
    if state_before.synced_through != journal.checkpoint_before
        || state_before.backlog != journal.backlog_before
    {
        return Err("Last.fm application journal conflicts with the saved sync state.".into());
    }
    let lastfm = Arc::clone(&state.lastfm);
    if lastfm.state().await.username.as_deref() != state_before.lastfm_username.as_deref() {
        return Err("The Last.fm account changed before journal recovery.".into());
    }
    if state_before
        .spotify_account_id
        .as_deref()
        .is_some_and(|expected| cached_spotify_account(state).as_deref() != Some(expected))
    {
        return Err("The Spotify account changed before journal recovery.".into());
    }
    lastfm.settle_before_import().await;
    let _receipt_guard = lastfm.reconciliation_guard().await;
    let _library_transaction = crate::begin_library_transaction(state)?;
    let mut recovered = state
        .library
        .lock()
        .expect("library mutex poisoned")
        .clone();
    let outcome =
        recover_application_journal(&mut recovered, &journal).map_err(|error| error.to_string())?;
    if outcome == JournalRecovery::AppliedBefore {
        crate::mutate_library_in_transaction(state, |library| {
            *library = recovered.clone();
            Ok(())
        })?;
    }
    service
        .mutate_sync(|sync| {
            sync.synced_through = journal.checkpoint_after;
            sync.last_synced_at = Some(crate::unix_now());
            sync.backlog = journal.backlog_after.clone();
            sync.active = None;
            sync.journal = None;
            sync.sync_problem = None;
            Ok(())
        })
        .await?;
    lastfm
        .prune_accepted_receipts_locked(&journal.consumed_receipts, journal.checkpoint_after)
        .await
}

async fn run_incremental_sync(
    state: &crate::AppState,
    service: &Arc<Service>,
    username: &str,
) -> Result<(), String> {
    let now = crate::unix_now();
    let spotify_account_id = cached_spotify_account(state);
    let current = service.sync_snapshot().await;
    if current.lastfm_username.as_deref() == Some(username) {
        recover_pending_incremental_journal(state, service).await?;
    }
    if current.lastfm_username.as_deref() != Some(username) {
        let previous_cache_id = current
            .active
            .as_ref()
            .map(|active| active.cache_id.clone());
        service
            .mutate_sync(|state| {
                state.lastfm_username = Some(username.to_owned());
                state.spotify_account_id = spotify_account_id.clone();
                state.synced_through = None;
                state.last_synced_at = None;
                state.active = None;
                state.backlog.clear();
                state.journal = None;
                state.sync_problem = None;
                Ok(())
            })
            .await?;
        if let Some(cache_id) = previous_cache_id {
            service.store.remove_snapshot(&cache_id);
        }
    }
    let current = service.sync_snapshot().await;
    if current.synced_through.is_none() {
        service
            .mutate_sync(|state| {
                state.synced_through = Some(now);
                state.last_synced_at = Some(now);
                state.spotify_account_id = spotify_account_id.clone();
                Ok(())
            })
            .await?;
        return Ok(());
    }
    if current
        .spotify_account_id
        .as_deref()
        .is_some_and(|expected| spotify_account_id.as_deref() != Some(expected))
    {
        let message = "Last.fm sync is suspended because the connected Spotify account changed.";
        set_sync_problem(service, Some(message.into())).await?;
        return Err(message.into());
    }
    if current.spotify_account_id.is_none() && spotify_account_id.is_some() {
        service
            .mutate_sync(|state| {
                state.spotify_account_id = spotify_account_id.clone();
                Ok(())
            })
            .await?;
    }
    if let Some(spotify_account_id) = spotify_account_id.as_deref() {
        if service.sync_snapshot().await.active.is_some() {
            service
                .sync_backlog_into_review(username, Some(spotify_account_id))
                .await?;
        } else {
            service
                .sweep_backlog_with_mappings(state, username, spotify_account_id)
                .await?;
        }
    } else {
        service.sync_backlog_into_review(username, None).await?;
    }
    let from = current.synced_through.unwrap_or(now);
    if from >= now && current.active.is_none() {
        service
            .mutate_sync(|state| {
                state.last_synced_at = Some(now);
                Ok(())
            })
            .await?;
        return Ok(());
    }
    if current.active.is_none() {
        let query_from = from.saturating_sub(1);
        let query_to = now.saturating_add(1);
        service
            .mutate_sync(|state| {
                state.active = Some(IncrementalRange {
                    from,
                    to: now,
                    query_from,
                    query_to,
                    cache_id: incremental_cache_id(username, from, now),
                    next_page: 1,
                    total_pages: None,
                    downloaded_pages: 0,
                    total_scrobbles: 0,
                });
                Ok(())
            })
            .await?;
    }
    loop {
        let current = service.sync_snapshot().await;
        let range = current
            .active
            .clone()
            .ok_or_else(|| "Last.fm incremental range disappeared.".to_string())?;
        if range.total_pages.is_none() {
            let lastfm = Arc::clone(&state.lastfm);
            let payload = fetch_incremental_page_with_retry(
                &lastfm,
                service,
                username,
                1,
                range.query_from,
                range.query_to,
            )
            .await?;
            let parsed = parse_recent_tracks_page(&payload)?;
            let total_pages = parsed.total_pages.ok_or_else(|| {
                "Last.fm incremental metadata did not include total pages.".to_string()
            })?;
            service
                .mutate_sync(|state| {
                    let active = state
                        .active
                        .as_mut()
                        .ok_or_else(|| "Last.fm incremental range disappeared.".to_string())?;
                    active.total_pages = Some(total_pages);
                    active.total_scrobbles = parsed.total.unwrap_or_default();
                    active.next_page = total_pages;
                    Ok(())
                })
                .await?;
            continue;
        }
        if range.next_page != 0 {
            let total_pages = range.total_pages.expect("checked above");
            let lastfm = Arc::clone(&state.lastfm);
            let fetch_username = username.to_owned();
            let query_from = range.query_from;
            let query_to = range.query_to;
            let checkpoint_service = Arc::clone(service);
            let checkpoint_username = username.to_owned();
            let outcome = download_page_window_with_checkpoint(
                Arc::clone(service),
                range.next_page,
                total_pages,
                move |page| {
                    let lastfm = Arc::clone(&lastfm);
                    let username = fetch_username.clone();
                    async move {
                        fetch_source_page(lastfm.as_ref(), &username, page, query_from, query_to)
                            .await
                    }
                },
                move |page, parsed| {
                    let service = Arc::clone(&checkpoint_service);
                    let username = checkpoint_username.clone();
                    async move {
                        service
                            .checkpoint_incremental_page(&username, page, parsed)
                            .await
                    }
                },
            )
            .await?;
            match outcome {
                SourceWindowOutcome::Complete(_) => continue,
                SourceWindowOutcome::Retryable => {
                    set_sync_problem(
                        service,
                        Some(
                            "Last.fm incremental download will resume after a temporary error."
                                .into(),
                        ),
                    )
                    .await?;
                    return Ok(());
                }
                SourceWindowOutcome::Suspended => {
                    return Err("Last.fm incremental sync was suspended.".into())
                }
            }
        }
        apply_completed_incremental_range(state, service, username).await?;
        return Ok(());
    }
}

#[tauri::command]
pub(crate) async fn sync_lastfm_plays(app: tauri::AppHandle) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let service = Arc::clone(&state.lastfm_import);
    let username = lastfm_username(&app).await?;
    state.lastfm.settle_before_import().await;
    if !service.claim_sync_runner() {
        return Ok(service.state().await);
    }
    let _ = app.emit("lastfm-import-changed", service.state().await);
    let result = run_incremental_sync(&state, &service, &username).await;
    if let Err(error) = &result {
        let _ = set_sync_problem(&service, Some(error.clone())).await;
    }
    service.release_sync_runner();
    let view = service.state().await;
    let _ = app.emit("lastfm-import-changed", &view);
    result.map(|()| view)
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

async fn match_batch<T, S>(
    provider: &retune_spotify::client::SpotifyClient<T, S>,
    artist: &str,
    album: &str,
    rows: &[SourceRow],
) -> Result<Vec<MatchResult>, String>
where
    T: retune_spotify::client::Transport,
    S: retune_spotify::tokens::TokenStore,
{
    if album.is_empty() {
        let mut matches = Vec::new();
        for row in rows {
            let search_term = track_search_term(artist, &row.track);
            let results = crate::provider::search_tracks(provider, &search_term).await?;
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
            classify_album_candidates_by_name(std::slice::from_ref(&row.track), &mut candidates);
            matches.push(match_result_for(
                row.stable_id.clone(),
                search_term,
                candidates,
                &row.track,
                false,
            ));
        }
        return Ok(matches);
    }
    let search_term = album_search_term(artist, album);
    let source_track_names = rows.iter().map(|row| row.track.clone()).collect::<Vec<_>>();
    let candidates = album_candidates(provider, &search_term, &source_track_names).await?;
    Ok(rows
        .iter()
        .map(|row| {
            match_result_for(
                row.stable_id.clone(),
                search_term.clone(),
                candidates.clone(),
                &row.track,
                false,
            )
        })
        .collect())
}

async fn current_matching_account_locked(
    state: &crate::AppState,
    service: &Service,
) -> Result<(String, String), String> {
    let (username, spotify_account_id) = connected_accounts_locked(state).await?;
    let Some(session) = service.snapshot().await else {
        return Err("No Last.fm import session is active.".into());
    };
    if !session_account_matches(&session, &username, &spotify_account_id, false) {
        service.suspend_for_account_mismatch().await?;
        return Err(
            "The saved Last.fm import belongs to a different account; it is suspended for safety."
                .into(),
        );
    }
    if !review_phase_allowed(session.phase) {
        return Err("Last.fm matching is available only after source review begins.".into());
    }
    Ok((username, spotify_account_id))
}

fn session_account_matches(
    session: &LastFmImportSessionV2,
    username: &str,
    spotify_account_id: &str,
    require_spotify_binding: bool,
) -> bool {
    session.lastfm_username == username
        && session
            .spotify_account_id
            .as_deref()
            .map_or(!require_spotify_binding, |bound| {
                bound == spotify_account_id
            })
}

async fn cached_spotify_binding_is_current(
    app: &tauri::AppHandle,
    service: &Service,
) -> Result<bool, String> {
    current_spotify_binding_is_current(app, service, false).await
}

fn cached_spotify_identity_matches(
    expected: &str,
    library: &crate::store::SpotifyLibraryState,
) -> Option<bool> {
    library.is_exact().then_some(library.account_id == expected)
}

async fn lazy_match_page_with_search<A, AFut, F, FFut>(
    service: &Service,
    spotify_library_gate: &tokio::sync::Mutex<()>,
    batch_id: u32,
    artist: &str,
    album: &str,
    current_account: A,
    search: F,
) -> Result<Option<ImportPageView>, String>
where
    A: Fn() -> AFut,
    AFut: Future<Output = Result<(String, String), String>>,
    F: FnOnce(Vec<SourceRow>) -> FFut,
    FFut: Future<Output = Result<Vec<MatchResult>, String>>,
{
    let Some(page) = service.page(batch_id, artist, album).await else {
        return Ok(None);
    };
    let session = service
        .snapshot()
        .await
        .ok_or_else(|| "No Last.fm import session is active.".to_string())?;
    if batch_match_plan(&session, Some((batch_id, artist, album))).is_empty() {
        let _membership_guard = spotify_library_gate.lock().await;
        current_account().await?;
        return Ok(Some(page));
    }

    // ponytail: one importer-wide lock; use per-batch locks only if throughput requires it.
    let _match_guard = service.lazy_match_lock.lock().await;
    let Some(page) = service.page(batch_id, artist, album).await else {
        return Ok(None);
    };
    let session = service
        .snapshot()
        .await
        .ok_or_else(|| "No Last.fm import session is active.".to_string())?;
    if batch_match_plan(&session, Some((batch_id, artist, album))).is_empty() {
        let _membership_guard = spotify_library_gate.lock().await;
        current_account().await?;
        return Ok(Some(page));
    }
    let initial_account = {
        let _membership_guard = spotify_library_gate.lock().await;
        current_account().await?
    };
    let batch = requested_batch(&session, batch_id, artist, album)
        .ok_or_else(|| "Unknown Last.fm import review batch.".to_string())?;
    let rows_by_id = source_row_map(&session);
    let rows = batch_rows(&batch, &rows_by_id)
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let results = search(rows).await?;
    let _membership_guard = spotify_library_gate.lock().await;
    let (username, spotify_account_id) = current_account().await?;
    if (username.as_str(), spotify_account_id.as_str())
        != (initial_account.0.as_str(), initial_account.1.as_str())
    {
        service.suspend_for_account_mismatch().await?;
        return Err(
            "The connected Spotify account changed while matching; the import is suspended for safety."
                .into(),
        );
    }
    service
        .set_matches(&username, &spotify_account_id, batch_id, results)
        .await?;
    Ok(service.page(batch_id, artist, album).await)
}

async fn lazy_match_page(
    app: &tauri::AppHandle,
    service: &Service,
    batch_id: u32,
    artist: &str,
    album: &str,
) -> Result<Option<ImportPageView>, String> {
    let Some(page) = service.page(batch_id, artist, album).await else {
        return Ok(None);
    };
    let session = service
        .snapshot()
        .await
        .ok_or_else(|| "No Last.fm import session is active.".to_string())?;
    if batch_match_plan(&session, Some((batch_id, artist, album))).is_empty() {
        return cached_spotify_binding_is_current(app, service)
            .await
            .map(|current| current.then_some(page));
    }
    let state = app.state::<crate::AppState>();
    let state_ref = &*state;
    let page = lazy_match_page_with_search(
        service,
        &state_ref.spotify_library_gate,
        batch_id,
        artist,
        album,
        || current_matching_account_locked(state_ref, service),
        |rows| async move {
            let provider = crate::provider_from(state_ref)?;
            match_batch(provider.as_ref(), artist, album, &rows).await
        },
    )
    .await?;
    if page.is_some() {
        let _ = app.emit("lastfm-import-changed", service.state().await);
    }
    Ok(page)
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

fn batch_match_plan(
    session: &LastFmImportSessionV2,
    requested: Option<(u32, &str, &str)>,
) -> Vec<(u32, String, String)> {
    let rows_by_id = source_row_map(session);
    review_batches(session)
        .into_iter()
        .filter_map(|batch| {
            let rows = batch_rows(&batch, &rows_by_id);
            let first = rows.first()?;
            let selected = requested.is_some_and(|(requested_page, artist, album)| {
                requested_page == batch.page && artist == first.artist && album == first.album
            });
            let remaining = requested.is_none()
                && rows
                    .iter()
                    .any(|row| is_actionable(session, &row.stable_id));
            if (selected || remaining)
                && rows
                    .iter()
                    .any(|row| !session.matches.contains_key(&row.stable_id))
            {
                Some((batch.page, first.artist.clone(), first.album.clone()))
            } else {
                None
            }
        })
        .collect()
}

fn accept_all_entity_uris(session: &LastFmImportSessionV2) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut album_uris = BTreeSet::new();
    let mut track_uris = BTreeSet::new();
    let rows_by_id = source_row_map(session);
    for batch in review_batches(session) {
        let rows = batch_rows(&batch, &rows_by_id);
        let Some(first) = rows.first() else {
            continue;
        };
        let options = session.options_for_batch(batch.page, &first.artist, &first.album);
        let selected = rows
            .into_iter()
            .filter(|row| {
                options.selected_track_ids.contains(&row.stable_id)
                    && is_actionable(session, &row.stable_id)
            })
            .collect::<Vec<_>>();
        if !options.import_content {
            continue;
        }
        if options.whole_album {
            for row in selected {
                if let Some(uri) = session
                    .matches
                    .get(&row.stable_id)
                    .and_then(|result| {
                        result.selected_uri.as_deref().or_else(|| {
                            best_candidate(result).map(|candidate| candidate.uri.as_str())
                        })
                    })
                    .filter(|uri| uri.starts_with("spotify:album:"))
                {
                    album_uris.insert(uri.to_owned());
                }
            }
        } else {
            for row in selected {
                if let Some(uri) = session
                    .matches
                    .get(&row.stable_id)
                    .and_then(|result| matched_track_uri_for_row(result, row))
                {
                    track_uris.insert(uri);
                }
            }
        }
    }
    (album_uris, track_uris)
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
    session: &LastFmImportSessionV2,
    target_uri: &str,
    current_rows: &[&SourceRow],
    current_options: &PageOptions,
) -> u64 {
    let current_ids = current_rows
        .iter()
        .map(|row| row.stable_id.as_str())
        .collect::<BTreeSet<_>>();
    let source_batches = source_batch_map(session);
    let mut relevant = Vec::new();
    for row in &session.rows {
        let decision = default_decision(session, &row.stable_id);
        let current_page = current_ids.contains(row.stable_id.as_str());
        let included = current_page || (decision.status == RowStatus::Done && !decision.excluded);
        let page_options = if current_page {
            current_options.clone()
        } else {
            source_batches
                .get(row.stable_id.as_str())
                .map(|batch_id| session.options_for_batch(*batch_id, &row.artist, &row.album))
                .unwrap_or_else(|| session.options_for(&row.artist, &row.album))
        };
        if included
            && page_options.include_historical_play_counts
            && session
                .matches
                .get(&row.stable_id)
                .and_then(|result| matched_track_uri_for_row(result, row))
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

fn promote_mapping(
    mappings: &mut LastFmMappings,
    session: &LastFmImportSessionV2,
    row: &SourceRow,
    result: Option<&MatchResult>,
    target_uri: &str,
) {
    let source_key = session
        .incremental_source_keys
        .get(&row.stable_id)
        .cloned()
        .unwrap_or_else(|| row.stable_id.clone());
    mappings
        .track_mappings
        .insert(source_key, target_uri.to_owned());
    let Some(album_uri) = result
        .and_then(|result| result.selected_uri.as_deref())
        .filter(|uri| uri.starts_with("spotify:album:"))
    else {
        return;
    };
    let album = mappings
        .album_mappings
        .entry(source_album_key(&row.artist, &row.album))
        .or_default();
    album.spotify_album_uri = album_uri.to_owned();
    album
        .track_uris_by_name
        .insert(normalize_for_match(&row.track), target_uri.to_owned());
}

async fn apply_page(
    app: &tauri::AppHandle,
    service: &Service,
    batch_id: u32,
    artist: &str,
    album: &str,
    selected_ids: &[String],
    options: PageOptions,
) -> Result<ImportStateView, String> {
    options.validate()?;
    let state = app.state::<crate::AppState>();
    recover_pending_incremental_journal(&state, service).await?;
    let membership_guard = state.spotify_library_gate.lock().await;
    let (username, spotify_account_id) = assert_current_account_locked(&state, service).await?;
    let Some(session) = service.snapshot().await else {
        return Err("No Last.fm import session is active.".into());
    };
    let Some(batch) = requested_batch(&session, batch_id, artist, album) else {
        return Err("Unknown Last.fm import review batch.".into());
    };
    let selected = selected_ids.iter().cloned().collect::<BTreeSet<_>>();
    if selected_ids
        .iter()
        .any(|id| !batch.source_ids.iter().any(|source_id| source_id == id))
    {
        return Err("A selected source row does not belong to this review batch.".into());
    }
    let rows_by_id = source_row_map(&session);
    let rows = batch_rows(&batch, &rows_by_id)
        .into_iter()
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
            if let Some(uri) = matched_track_uri_for_row(result, row) {
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

    let (checked_username, checked_spotify_account_id) =
        assert_current_account_locked(&state, service).await?;
    if checked_username != username || checked_spotify_account_id != spotify_account_id {
        return Err(
            "The connected account changed while saving Spotify content; retry this review batch."
                .into(),
        );
    }
    let reconciliation_guard = service.reconciliation_lock.lock().await;
    let Some(current_session) = service.snapshot().await else {
        return Err("Last.fm import session ended while saving Spotify content.".into());
    };
    if current_session != session {
        return Err(
            "Last.fm review changed while saving Spotify content; retry this review batch.".into(),
        );
    }
    drop(membership_guard);

    let original_mappings = service
        .mappings_for(&username, Some(&spotify_account_id))
        .await;
    let mut mappings = original_mappings.clone();
    for row in &rows {
        if let Some(uri) = target_by_source.get(&row.stable_id) {
            promote_mapping(
                &mut mappings,
                &session,
                row,
                session.matches.get(&row.stable_id),
                uri,
            );
        }
    }
    if mappings != original_mappings {
        service
            .save_mappings_for(&username, Some(&spotify_account_id), mappings)
            .await?;
    }

    let mut by_target = BTreeMap::<String, Vec<&SourceRow>>::new();
    for row in &rows {
        if !session.incremental_source_keys.contains_key(&row.stable_id) {
            if let Some(uri) = target_by_source.get(&row.stable_id) {
                by_target.entry(uri.clone()).or_default().push(row);
            }
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
    let committed = committed_source_ids(
        &rows,
        &target_by_source,
        options.import_content,
        options.whole_album,
        options.include_historical_play_counts,
    );
    let metadata_uris = metadata_uris.into_iter().collect::<Vec<_>>();
    if !updates.is_empty() || !metadata_uris.is_empty() {
        crate::mutate_library(&state, |library| {
            apply_history_updates(library, &updates);
            apply_metadata(
                library,
                &metadata_uris,
                options.whole_album,
                options.genre.as_deref(),
                options.rating,
            )
        })?;
    }
    let view = service
        .commit_rows(
            &username,
            &spotify_account_id,
            batch_id,
            &committed,
            artist,
            album,
            options,
        )
        .await?;
    drop(reconciliation_guard);
    service
        .sweep_backlog_with_mappings(&state, &username, &spotify_account_id)
        .await?;
    Ok(view)
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

fn preserve_match_selection(
    mut result: MatchResult,
    previous: Option<&MatchResult>,
    source_id: &str,
) -> MatchResult {
    let Some(previous) = previous else {
        return result;
    };
    result.selected_uri = previous.selected_uri.clone();
    result.confidence = previous.confidence;
    result.track_matches = previous.track_matches.clone();
    let mut candidates = previous
        .candidates
        .iter()
        .filter(|candidate| {
            previous
                .selected_uri
                .as_deref()
                .is_some_and(|uri| uri == candidate.uri)
                || previous.track_matches.get(source_id).is_some_and(|uri| {
                    candidate
                        .track_uris
                        .iter()
                        .any(|track_uri| track_uri == uri)
                })
        })
        .cloned()
        .collect::<Vec<_>>();
    for candidate in result.candidates {
        if !candidates
            .iter()
            .any(|existing| existing.uri == candidate.uri)
        {
            candidates.push(candidate);
        }
    }
    candidates.truncate(10);
    result.candidates = candidates;
    result
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
    if service.snapshot().await.is_some() {
        let _ = ensure_import_readable(&app, service.as_ref()).await?;
    }
    let mut view = service.state().await;
    if view.phase.is_none() {
        view.username = lastfm_username(&app).await.ok();
    }
    Ok(view)
}

#[tauri::command]
pub(crate) async fn lastfm_import_queue(
    app: tauri::AppHandle,
    cursor: Option<usize>,
    limit: Option<usize>,
) -> Result<ImportQueuePage, String> {
    let service = &app.state::<crate::AppState>().lastfm_import;
    let cursor = cursor.unwrap_or_default();
    let limit = limit.unwrap_or(LASTFM_QUEUE_PAGE_LIMIT);
    if limit == 0 || limit > LASTFM_QUEUE_PAGE_LIMIT {
        return Err(format!(
            "Last.fm import queue limit must be between 1 and {LASTFM_QUEUE_PAGE_LIMIT}."
        ));
    }
    if !ensure_import_readable(&app, service.as_ref()).await? {
        if cursor != 0 {
            return Err("Last.fm import queue cursor is out of range.".into());
        }
        return Ok(ImportQueuePage {
            items: Vec::new(),
            cursor,
            next_cursor: None,
            total: 0,
        });
    }
    service.queue_page(cursor, limit).await
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_page(
    app: tauri::AppHandle,
    batch_id: u32,
    artist: String,
    album: String,
) -> Result<Option<ImportPageView>, String> {
    let service = &app.state::<crate::AppState>().lastfm_import;
    if !ensure_import_readable(&app, service.as_ref()).await? {
        return Ok(None);
    }
    lazy_match_page(&app, service.as_ref(), batch_id, &artist, &album).await
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_review(
    app: tauri::AppHandle,
    batch_id: u32,
    id: Option<String>,
    action: String,
    artist: String,
    album: String,
) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let _membership_guard = state.spotify_library_gate.lock().await;
    let (username, spotify_account_id) =
        assert_current_account_locked(&state, state.lastfm_import.as_ref()).await?;
    state
        .lastfm_import
        .review_action(
            &username,
            &spotify_account_id,
            batch_id,
            id.as_deref(),
            &action,
            &artist,
            &album,
        )
        .await?;
    if matches!(
        action.as_str(),
        "exclude" | "undo-exclude" | "ignore-album" | "restore" | "ignore-artist"
    ) {
        state
            .lastfm_import
            .sweep_backlog_with_mappings(&state, &username, &spotify_account_id)
            .await?;
    }
    drop(_membership_guard);
    emit_import_changed(&app, state.lastfm_import.as_ref()).await
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_options(
    app: tauri::AppHandle,
    batch_id: u32,
    artist: String,
    album: String,
    options: PageOptions,
) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let _membership_guard = state.spotify_library_gate.lock().await;
    let (username, spotify_account_id) =
        assert_current_account_locked(&state, state.lastfm_import.as_ref()).await?;
    state
        .lastfm_import
        .update_options(
            &username,
            &spotify_account_id,
            batch_id,
            &artist,
            &album,
            options,
        )
        .await?;
    drop(_membership_guard);
    emit_import_changed(&app, state.lastfm_import.as_ref()).await
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_count_mode(
    app: tauri::AppHandle,
    target_uri: String,
    mode: CountMode,
) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let _membership_guard = state.spotify_library_gate.lock().await;
    let (username, spotify_account_id) =
        assert_current_account_locked(&state, state.lastfm_import.as_ref()).await?;
    state
        .lastfm_import
        .set_count_mode(&username, &spotify_account_id, &target_uri, mode)
        .await?;
    drop(_membership_guard);
    emit_import_changed(&app, state.lastfm_import.as_ref()).await
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_search_terms(
    app: tauri::AppHandle,
    show: bool,
) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let _membership_guard = state.spotify_library_gate.lock().await;
    let (username, spotify_account_id) =
        assert_current_account_locked(&state, state.lastfm_import.as_ref()).await?;
    state
        .lastfm_import
        .set_search_terms(&username, &spotify_account_id, show)
        .await?;
    drop(_membership_guard);
    emit_import_changed(&app, state.lastfm_import.as_ref()).await
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_select_match(
    app: tauri::AppHandle,
    batch_id: u32,
    id: String,
    uri: String,
) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let _membership_guard = state.spotify_library_gate.lock().await;
    let (username, spotify_account_id) =
        assert_current_account_locked(&state, state.lastfm_import.as_ref()).await?;
    state
        .lastfm_import
        .select_match(&username, &spotify_account_id, batch_id, &id, &uri)
        .await?;
    drop(_membership_guard);
    emit_import_changed(&app, state.lastfm_import.as_ref()).await
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_change_track(
    app: tauri::AppHandle,
    batch_id: u32,
    id: String,
    query: String,
) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let _ = assert_current_account(&app, state.lastfm_import.as_ref()).await?;
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
    if requested_batch(&session, batch_id, &row.artist, &row.album).is_none() {
        return Err("The source row does not belong to this review batch.".into());
    }
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
    let result = preserve_match_selection(
        match_result_for(id.clone(), search_term, candidates, &row.track, false),
        session.matches.get(&id),
        &id,
    );
    let _membership_guard = state.spotify_library_gate.lock().await;
    let (username, spotify_account_id) =
        assert_current_account_locked(&state, state.lastfm_import.as_ref()).await?;
    state
        .lastfm_import
        .set_match(&username, &spotify_account_id, batch_id, result)
        .await?;
    drop(_membership_guard);
    emit_import_changed(&app, state.lastfm_import.as_ref()).await
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_change_album(
    app: tauri::AppHandle,
    batch_id: u32,
    id: String,
    query: String,
) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let _ = assert_current_account(&app, state.lastfm_import.as_ref()).await?;
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
    let batch = requested_batch(&session, batch_id, &row.artist, &row.album)
        .ok_or_else(|| "The source row does not belong to this review batch.".to_string())?;
    let rows_by_id = source_row_map(&session);
    let related = batch_rows(&batch, &rows_by_id)
        .into_iter()
        .map(|candidate| candidate.track.clone())
        .collect::<Vec<_>>();
    let search_term = if query.trim().is_empty() {
        album_search_term(&row.artist, &row.album)
    } else {
        query.trim().to_owned()
    };
    let provider = crate::provider_from(&state)?;
    let candidates = album_candidates(provider.as_ref(), &search_term, &related).await?;
    let matches = batch_rows(&batch, &rows_by_id)
        .into_iter()
        .map(|candidate_row| {
            preserve_match_selection(
                match_result_for(
                    candidate_row.stable_id.clone(),
                    search_term.clone(),
                    candidates.clone(),
                    &candidate_row.track,
                    false,
                ),
                session.matches.get(&candidate_row.stable_id),
                &candidate_row.stable_id,
            )
        })
        .collect();
    let _membership_guard = state.spotify_library_gate.lock().await;
    let (username, spotify_account_id) =
        assert_current_account_locked(&state, state.lastfm_import.as_ref()).await?;
    state
        .lastfm_import
        .set_matches(&username, &spotify_account_id, batch_id, matches)
        .await?;
    drop(_membership_guard);
    emit_import_changed(&app, state.lastfm_import.as_ref()).await
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_apply(
    app: tauri::AppHandle,
    batch_id: u32,
    artist: String,
    album: String,
    selected_ids: Vec<String>,
    options: PageOptions,
) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let view = apply_page(
        &app,
        state.lastfm_import.as_ref(),
        batch_id,
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

#[tauri::command]
pub(crate) async fn lastfm_import_prepare_accept_all(
    app: tauri::AppHandle,
) -> Result<AcceptAllSummary, String> {
    let state = app.state::<crate::AppState>();
    let service = state.lastfm_import.as_ref();
    if !ensure_import_readable(&app, service).await? {
        return Ok(AcceptAllSummary {
            album_entities: 0,
            track_entities: 0,
        });
    }
    let app_for_prepare = app.clone();
    prepare_accept_all_batches(service, |batch_id, artist, album| {
        let app = app_for_prepare.clone();
        async move {
            lazy_match_page(&app, service, batch_id, &artist, &album)
                .await
                .map(|_| ())
        }
    })
    .await
}

async fn prepare_accept_all_batches<F, Fut>(
    service: &Service,
    mut prepare: F,
) -> Result<AcceptAllSummary, String>
where
    F: FnMut(u32, String, String) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let session = service
        .snapshot()
        .await
        .ok_or_else(|| "No Last.fm import session is active.".to_string())?;
    for (batch_id, artist, album) in batch_match_plan(&session, None) {
        prepare(batch_id, artist, album).await?;
    }
    let session = service
        .snapshot()
        .await
        .ok_or_else(|| "No Last.fm import session is active.".to_string())?;
    let (albums, tracks) = accept_all_entity_uris(&session);
    Ok(AcceptAllSummary {
        album_entities: albums.len() as u32,
        track_entities: tracks.len() as u32,
    })
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_accept_all_page(
    app: tauri::AppHandle,
    batch_id: u32,
    artist: String,
    album: String,
) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let _membership_guard = state.spotify_library_gate.lock().await;
    let (username, spotify_account_id) =
        assert_current_account_locked(&state, state.lastfm_import.as_ref()).await?;
    let Some(page) = state.lastfm_import.page(batch_id, &artist, &album).await else {
        return Ok(state.lastfm_import.state().await);
    };
    if page.rows.iter().any(|item| item.match_result.is_none()) {
        return Err("Prepare Accept All before applying its confirmed batches.".into());
    }
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
                        batch_id,
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
                    batch_id,
                    &item.source.stable_id,
                    &candidate.uri,
                )
                .await?;
        }
    }
    drop(_membership_guard);
    let Some(page) = state.lastfm_import.page(batch_id, &artist, &album).await else {
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
        batch_id,
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

pub(crate) fn default_decision(session: &LastFmImportSessionV2, id: &str) -> RowDecision {
    session.decisions.get(id).cloned().unwrap_or_default()
}

fn locked_count_modes(session: &LastFmImportSessionV2) -> BTreeSet<String> {
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

fn queue_item(
    session: &LastFmImportSessionV2,
    batch: &ImportBatch,
    rows: &[&SourceRow],
) -> Option<ImportQueueItem> {
    let first = rows.first()?;
    let artist = first.artist.clone();
    let album = first.album.clone();
    let options = session.options_for_page_batch(batch, &artist, &album, rows);
    let remaining = rows.iter().any(|row| {
        let decision = default_decision(session, &row.stable_id);
        matches!(decision.status, RowStatus::Pending | RowStatus::Skipped) && !decision.excluded
    });
    let selected = rows
        .iter()
        .filter(|row| {
            let decision = default_decision(session, &row.stable_id);
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
                    result
                        .selected_uri
                        .as_deref()
                        .or_else(|| best_candidate(result).map(|candidate| candidate.uri.as_str()))
                })
                .any(|uri| uri.starts_with("spotify:album:")) as u32;
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
    Some(ImportQueueItem {
        page: batch.page,
        artist,
        album,
        play_count: rows
            .iter()
            .map(|row| row.play_count)
            .fold(0, u64::saturating_add),
        latest: rows.iter().map(|row| row.latest).max().unwrap_or_default(),
        source_count: batch.source_ids.len(),
        remaining,
        album_entities,
        track_entities: track_uris.len() as u32,
        status: queue_status(session, rows),
    })
}

fn queue_status(session: &LastFmImportSessionV2, rows: &[&SourceRow]) -> Option<QueueStatus> {
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

fn update_review_phase(session: &mut LastFmImportSessionV2) {
    if session.remaining() == 0 {
        session.phase = ImportPhase::Done;
    } else if session.phase == ImportPhase::Done {
        session.phase = ImportPhase::Review;
    }
}

fn review_phase_allowed(phase: ImportPhase) -> bool {
    matches!(phase, ImportPhase::Review | ImportPhase::Done)
}

fn exclude_row(session: &mut LastFmImportSessionV2, id: &str, excluded: bool) {
    if is_reviewable(session, id) {
        let decision = session.decisions.entry(id.to_owned()).or_default();
        decision.excluded = excluded;
    }
}

fn is_reviewable(session: &LastFmImportSessionV2, id: &str) -> bool {
    matches!(
        default_decision(session, id).status,
        RowStatus::Pending | RowStatus::Skipped
    )
}

fn is_actionable(session: &LastFmImportSessionV2, id: &str) -> bool {
    is_reviewable(session, id) && !default_decision(session, id).excluded
}

fn album_source_ids(session: &LastFmImportSessionV2, artist: &str, album: &str) -> Vec<String> {
    session
        .rows
        .iter()
        .filter(|row| row.artist == artist && row.album == album)
        .map(|row| row.stable_id.clone())
        .collect()
}

#[cfg(test)]
pub(crate) fn ignore_album(session: &mut LastFmImportSessionV2, artist: &str, album: &str) {
    let ids = album_source_ids(session, artist, album);
    for id in ids {
        if is_actionable(session, &id) {
            session.decisions.insert(
                id,
                RowDecision {
                    status: RowStatus::IgnoredAlbum,
                    excluded: false,
                },
            );
        }
    }
}

pub(crate) fn ignore_artist(session: &mut LastFmImportSessionV2, artist: &str) {
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

#[cfg(test)]
pub(crate) fn skip_album(session: &mut LastFmImportSessionV2, artist: &str, album: &str) {
    let ids = album_source_ids(session, artist, album);
    for id in ids {
        if is_actionable(session, &id) {
            session.decisions.insert(
                id,
                RowDecision {
                    status: RowStatus::Skipped,
                    excluded: false,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{SavedAlbumRecord, SpotifyLibraryState};
    use retune_core::model::SourceId;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::{
        fs,
        path::{Path, PathBuf},
        time::Duration,
    };

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
    fn reconciliation_maps_an_external_scrobble_once() {
        let mappings = LastFmMappings {
            track_mappings: BTreeMap::from([(
                source_id("Artist", "Album", "Song"),
                "spotify:track:one".into(),
            )]),
            ..LastFmMappings::default()
        };
        let result = reconcile_incremental(
            &[ExternalScrobble {
                artist: "Artist".into(),
                album: "Album".into(),
                track: "Song".into(),
                timestamp: 150,
                submitted: None,
            }],
            &[],
            &mappings,
            &BTreeSet::from(["spotify:track:one".to_owned()]),
            100,
            200,
        );

        assert_eq!(
            result.increments,
            BTreeMap::from([(String::from("spotify:track:one"), 1)])
        );
        assert!(result.unresolved.is_empty());
    }

    fn event(artist: &str, album: &str, track: &str, timestamp: u64) -> ExternalScrobble {
        ExternalScrobble {
            artist: artist.into(),
            album: album.into(),
            track: track.into(),
            timestamp,
            submitted: None,
        }
    }

    fn quarantined_file(dir: &Path, prefix: &str) -> PathBuf {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(prefix))
            })
            .expect("expected a quarantined persistence file")
    }

    #[tokio::test]
    async fn corrupt_incremental_state_is_quarantined_before_fresh_state_is_saved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lastfm-sync.json");
        fs::write(&path, b"not json").unwrap();

        let service = Service::new(dir.path());
        assert_eq!(
            service.state().await.sync_problem.as_deref(),
            Some("Last.fm sync state was quarantined; sync starts from now.")
        );
        let quarantined = quarantined_file(dir.path(), "lastfm-sync.json.quarantine-");
        assert_eq!(fs::read(quarantined).unwrap(), b"not json");
        assert!(!path.exists());

        service.mutate_sync(|_| Ok(())).await.unwrap();
        assert!(path.is_file());
        assert_ne!(fs::read(path).unwrap(), b"not json");
    }

    #[tokio::test]
    async fn unsupported_incremental_state_is_quarantined_before_fresh_state_is_saved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lastfm-sync.json");
        let unsupported = LastFmSyncState {
            version: LASTFM_SYNC_VERSION.saturating_add(1),
            ..LastFmSyncState::default()
        };
        let bytes = serde_json::to_vec(&unsupported).unwrap();
        fs::write(&path, &bytes).unwrap();

        let service = Service::new(dir.path());
        assert_eq!(
            service.state().await.sync_problem.as_deref(),
            Some("Last.fm sync state was quarantined; sync starts from now.")
        );
        let quarantined = quarantined_file(dir.path(), "lastfm-sync.json.quarantine-");
        assert_eq!(fs::read(quarantined).unwrap(), bytes);
        assert!(!path.exists());

        service.mutate_sync(|_| Ok(())).await.unwrap();
        assert!(path.is_file());
        assert_ne!(fs::read(path).unwrap(), bytes);
    }

    #[tokio::test]
    async fn corrupt_mappings_are_quarantined_before_fresh_mappings_are_saved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lastfm-mappings.json");
        fs::write(&path, b"not json").unwrap();

        let service = Service::new(dir.path());
        assert_eq!(
            service.state().await.sync_problem.as_deref(),
            Some("Last.fm mappings were quarantined; reusable decisions were reset.")
        );
        let quarantined = quarantined_file(dir.path(), "lastfm-mappings.json.quarantine-");
        assert_eq!(fs::read(quarantined).unwrap(), b"not json");
        assert!(!path.exists());

        service
            .save_mappings_for("user", Some("spotify"), LastFmMappings::default())
            .await
            .unwrap();
        assert!(path.is_file());
        assert_ne!(fs::read(path).unwrap(), b"not json");
    }

    #[tokio::test]
    async fn unsupported_mappings_are_quarantined_before_fresh_mappings_are_saved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lastfm-mappings.json");
        let unsupported = PersistedLastFmMappings {
            version: LASTFM_MAPPINGS_VERSION.saturating_add(1),
            ..PersistedLastFmMappings::default()
        };
        let bytes = serde_json::to_vec(&unsupported).unwrap();
        fs::write(&path, &bytes).unwrap();

        let service = Service::new(dir.path());
        assert_eq!(
            service.state().await.sync_problem.as_deref(),
            Some("Last.fm mappings were quarantined; reusable decisions were reset.")
        );
        let quarantined = quarantined_file(dir.path(), "lastfm-mappings.json.quarantine-");
        assert_eq!(fs::read(quarantined).unwrap(), bytes);
        assert!(!path.exists());

        service
            .save_mappings_for("user", Some("spotify"), LastFmMappings::default())
            .await
            .unwrap();
        assert!(path.is_file());
        assert_ne!(fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn mapped_backlog_occurrence_adds_to_existing_count() {
        let mappings = LastFmMappings {
            track_mappings: BTreeMap::from([(
                source_id("Artist", "Album", "Song"),
                "spotify:track:one".into(),
            )]),
            ..LastFmMappings::default()
        };
        let result = reconcile_incremental(
            &[event("Artist", "Album", "Song", 10)],
            &[],
            &mappings,
            &BTreeSet::from(["spotify:track:one".into()]),
            0,
            20,
        );
        let mut library = Library::new();
        let id = library.add(retune_core::model::NewTrack {
            uri: "spotify:track:one".into(),
            ..retune_core::model::NewTrack::default()
        });
        library.tracks_mut()[0].play_count = 100;

        apply_incremental_updates(&mut library, &result.increments, &result.latest);

        assert_eq!(library.get(id).unwrap().play_count, 101);
    }

    #[tokio::test]
    async fn active_incremental_review_preserves_backlog_for_completion() {
        let directory = tempfile::tempdir().unwrap();
        let service = Service::new(directory.path());
        let backlog = vec![event("Artist", "Album", "Song", 10)];
        service
            .mutate_sync(|state| {
                state.lastfm_username = Some("user".into());
                state.spotify_account_id = Some("spotify".into());
                state.active = Some(IncrementalRange {
                    from: 10,
                    to: 20,
                    query_from: 9,
                    query_to: 21,
                    cache_id: "range".into(),
                    next_page: 1,
                    ..IncrementalRange::default()
                });
                state.backlog = backlog.clone();
                Ok(())
            })
            .await
            .unwrap();

        service
            .sync_backlog_into_review("user", Some("spotify"))
            .await
            .unwrap();

        let state = service.sync_snapshot().await;
        assert_eq!(state.backlog, backlog);
        assert!(state.active.is_some());
        assert_eq!(
            service
                .snapshot()
                .await
                .unwrap()
                .incremental_source_keys
                .len(),
            1
        );
    }

    fn receipt(
        corrected: (&str, &str, &str),
        submitted: (&str, &str, &str),
        timestamp: u64,
    ) -> AcceptedScrobbleReceipt {
        AcceptedScrobbleReceipt {
            corrected: ScrobbleMetadata {
                artist: corrected.0.into(),
                album: corrected.1.into(),
                track: corrected.2.into(),
            },
            submitted: ScrobbleMetadata {
                artist: submitted.0.into(),
                album: submitted.1.into(),
                track: submitted.2.into(),
            },
            timestamp,
        }
    }

    fn seed_lastfm_files(directory: &Path, accepted: &[AcceptedScrobbleReceipt]) {
        fs::write(
            directory.join("dev-lastfm-session.json"),
            br#"{"username":"user","key":"session"}"#,
        )
        .unwrap();
        fs::write(
            directory.join("lastfm-scrobbles.json"),
            serde_json::to_vec(&serde_json::json!({
                "version": 2,
                "pending": [],
                "accepted": accepted,
                "owner": "user"
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn library_with_count(play_count: u32) -> Library {
        let mut library = Library::new();
        library.add(retune_core::model::NewTrack {
            uri: "spotify:track:one".into(),
            name: "Song".into(),
            source: SourceId::Music,
            ..retune_core::model::NewTrack::default()
        });
        library.tracks_mut()[0].play_count = play_count;
        library
    }

    fn recent_tracks_response(page: u32, total_pages: u32, entries: Value) -> Value {
        serde_json::json!({
            "recenttracks": {
                "track": entries,
                "@attr": {
                    "page": page.to_string(),
                    "totalPages": total_pages.to_string(),
                    "total": "0"
                }
            }
        })
    }

    fn test_app_state(
        directory: &Path,
        library: Library,
        accepted: &[AcceptedScrobbleReceipt],
    ) -> (Arc<crate::lastfm::Service>, Arc<Service>, crate::AppState) {
        seed_lastfm_files(directory, accepted);
        let lastfm = crate::lastfm::Service::new(directory, true, false);
        let service = Service::new(directory);
        let state = crate::test_app_state(
            directory,
            library,
            SpotifyLibraryState::default(),
            Arc::clone(&lastfm),
            Arc::clone(&service),
        );
        (lastfm, service, state)
    }

    #[tokio::test]
    async fn apply_completed_incremental_range_persists_and_prunes_once() {
        let directory = tempfile::tempdir().unwrap();
        let accepted = receipt(("Artist", "Album", "Song"), ("Artist", "Album", "Song"), 12);
        let (lastfm, service, state) = test_app_state(
            directory.path(),
            library_with_count(100),
            std::slice::from_ref(&accepted),
        );
        service
            .save_mappings_for(
                "user",
                None,
                LastFmMappings {
                    track_mappings: BTreeMap::from([(
                        source_id("Artist", "Album", "Song"),
                        "spotify:track:one".into(),
                    )]),
                    ..LastFmMappings::default()
                },
            )
            .await
            .unwrap();
        service
            .mutate_sync(|sync| {
                sync.lastfm_username = Some("user".into());
                sync.synced_through = Some(10);
                sync.active = Some(IncrementalRange {
                    from: 10,
                    to: 20,
                    query_from: 9,
                    query_to: 21,
                    cache_id: incremental_cache_id("user", 10, 20),
                    next_page: 1,
                    total_pages: Some(1),
                    ..IncrementalRange::default()
                });
                Ok(())
            })
            .await
            .unwrap();
        service
            .checkpoint_incremental_page(
                "user",
                1,
                parsed_page(
                    1,
                    1,
                    vec![
                        scrobble("Artist", "Album", "Song", 11),
                        scrobble("Artist", "Album", "Song", 12),
                    ],
                ),
            )
            .await
            .unwrap();

        apply_completed_incremental_range(&state, &service, "user")
            .await
            .unwrap();

        assert_eq!(state.library.lock().unwrap().tracks()[0].play_count, 101);
        let sync = service.sync_snapshot().await;
        assert_eq!(sync.synced_through, Some(20));
        assert!(sync.active.is_none());
        assert!(sync.journal.is_none());
        assert!(sync.backlog.is_empty());
        assert!(lastfm.accepted_receipts().await.is_empty());
        let ledger: Value = serde_json::from_slice(
            &fs::read(directory.path().join("lastfm-scrobbles.json")).unwrap(),
        )
        .unwrap();
        assert!(ledger["accepted"].as_array().unwrap().is_empty());
    }

    async fn recover_persisted_journal(library: Library) -> (Library, Value) {
        let directory = tempfile::tempdir().unwrap();
        let accepted = receipt(("Artist", "Album", "Song"), ("Artist", "Album", "Song"), 20);
        let (lastfm, service, state) =
            test_app_state(directory.path(), library, std::slice::from_ref(&accepted));
        let before = library_with_count(100);
        let mut after = before.clone();
        apply_incremental_updates(
            &mut after,
            &BTreeMap::from([(String::from("spotify:track:one"), 1)]),
            &BTreeMap::from([(String::from("spotify:track:one"), 19)]),
        );
        let backlog_before = vec![event("Artist", "Album", "Song", 10)];
        service
            .mutate_sync(|sync| {
                sync.lastfm_username = Some("user".into());
                sync.synced_through = Some(10);
                sync.active = Some(IncrementalRange {
                    from: 10,
                    to: 20,
                    next_page: 0,
                    ..IncrementalRange::default()
                });
                sync.backlog = backlog_before.clone();
                sync.journal = Some(LastFmApplicationJournal {
                    before_library: before.clone(),
                    after_library: after.clone(),
                    checkpoint_before: Some(10),
                    checkpoint_after: Some(20),
                    backlog_before,
                    backlog_after: Vec::new(),
                    consumed_receipts: vec![accepted],
                });
                Ok(())
            })
            .await
            .unwrap();

        recover_pending_incremental_journal(&state, &service)
            .await
            .unwrap();
        let sync = service.sync_snapshot().await;
        assert_eq!(sync.synced_through, Some(20));
        assert!(sync.active.is_none());
        assert!(sync.journal.is_none());
        assert!(sync.backlog.is_empty());
        assert!(lastfm.accepted_receipts().await.is_empty());

        recover_pending_incremental_journal(&state, &service)
            .await
            .unwrap();
        let ledger: Value = serde_json::from_slice(
            &fs::read(directory.path().join("lastfm-scrobbles.json")).unwrap(),
        )
        .unwrap();
        let library = state.library.lock().unwrap().clone();
        (library, ledger)
    }

    #[tokio::test]
    async fn recover_pending_incremental_journal_applies_library_before_exactly_once() {
        let (library, ledger) = recover_persisted_journal(library_with_count(100)).await;
        assert_eq!(library.tracks()[0].play_count, 101);
        assert!(ledger["accepted"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn recover_pending_incremental_journal_finalizes_library_after_exactly_once() {
        let mut after = library_with_count(100);
        apply_incremental_updates(
            &mut after,
            &BTreeMap::from([(String::from("spotify:track:one"), 1)]),
            &BTreeMap::from([(String::from("spotify:track:one"), 19)]),
        );
        let (library, ledger) = recover_persisted_journal(after).await;
        assert_eq!(library.tracks()[0].play_count, 101);
        assert!(ledger["accepted"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn incremental_runner_activates_at_now_then_downloads_padded_range_without_spotify() {
        let directory = tempfile::tempdir().unwrap();
        let (lastfm, service, state) = test_app_state(directory.path(), Library::new(), &[]);

        run_incremental_sync(&state, &service, "user")
            .await
            .unwrap();
        let first = service.sync_snapshot().await;
        assert!(first.synced_through.is_some());
        assert_eq!(first.last_synced_at, first.synced_through);
        assert!(lastfm.test_requests().is_empty());

        let from = crate::unix_now().saturating_sub(100);
        service
            .mutate_sync(|sync| {
                sync.synced_through = Some(from);
                sync.last_synced_at = None;
                sync.active = None;
                Ok(())
            })
            .await
            .unwrap();
        let empty_page = recent_tracks_response(1, 1, serde_json::json!([]));
        lastfm.queue_test_response(empty_page.clone());
        lastfm.queue_test_response(empty_page);

        run_incremental_sync(&state, &service, "user")
            .await
            .unwrap();

        let requests = lastfm.test_requests();
        assert_eq!(requests.len(), 2);
        assert!(requests
            .iter()
            .all(|(method, _)| method == "user.getRecentTracks"));
        for (_, params) in &requests {
            assert_eq!(
                params.iter().find(|(key, _)| key == "from").unwrap().1,
                (from - 1).to_string()
            );
        }
        let query_to = requests[0]
            .1
            .iter()
            .find(|(key, _)| key == "to")
            .unwrap()
            .1
            .parse::<u64>()
            .unwrap();
        assert!(requests.iter().all(|(_, params)| {
            params.iter().find(|(key, _)| key == "to").unwrap().1 == query_to.to_string()
        }));
        let final_state = service.sync_snapshot().await;
        assert_eq!(final_state.synced_through, Some(query_to - 1));
        assert!(final_state.active.is_none());
    }

    #[tokio::test]
    async fn restored_mappings_activate_only_for_the_exact_backup_identities() {
        let directory = tempfile::tempdir().unwrap();
        let service = Service::new(directory.path());
        let imported = PersistedLastFmMappings {
            version: LASTFM_MAPPINGS_VERSION,
            lastfm_username: Some("user".into()),
            spotify_account_id: Some("spotify-a".into()),
            dormant: false,
            mappings: LastFmMappings {
                track_mappings: BTreeMap::from([(
                    source_id("Artist", "Album", "Song"),
                    "spotify:track:one".into(),
                )]),
                ..LastFmMappings::default()
            },
        };
        service.restore_mappings(imported).await.unwrap();

        assert!(service
            .mappings_for("other", Some("spotify-a"))
            .await
            .track_mappings
            .is_empty());
        assert!(service
            .mappings_for("user", Some("spotify-b"))
            .await
            .track_mappings
            .is_empty());
        assert!(service.export_mappings().await.dormant);

        let active = service.mappings_for("user", Some("spotify-a")).await;
        let source_key = source_id("Artist", "Album", "Song");
        assert_eq!(active.track_mappings[&source_key], "spotify:track:one");
        assert!(!service.export_mappings().await.dormant);
        assert_eq!(
            service.mappings_for("user", Some("spotify-a")).await,
            active
        );
    }

    #[test]
    fn reconciliation_receipts_are_matched_and_consumed_as_a_multiset() {
        let mappings = LastFmMappings {
            track_mappings: BTreeMap::from([(
                source_id("Artist", "Album", "Song"),
                "spotify:track:one".into(),
            )]),
            ..LastFmMappings::default()
        };
        let events = vec![event("Artist", "Album", "Song", 10); 2];
        let one = reconcile_incremental(
            &events,
            &[receipt(
                ("Corrected", "Album", "Song"),
                ("Artist", "Album", "Song"),
                10,
            )],
            &mappings,
            &BTreeSet::from(["spotify:track:one".into()]),
            0,
            20,
        );
        assert_eq!(one.increments["spotify:track:one"], 1);
        assert_eq!(one.consumed_receipts.len(), 1);

        let two = reconcile_incremental(
            &events,
            &[
                receipt(
                    ("Corrected", "Album", "Song"),
                    ("Artist", "Album", "Song"),
                    10,
                ),
                receipt(
                    ("Corrected", "Album", "Song"),
                    ("Artist", "Album", "Song"),
                    10,
                ),
            ],
            &mappings,
            &BTreeSet::from(["spotify:track:one".into()]),
            0,
            20,
        );
        assert!(two.increments.is_empty());
        assert_eq!(two.consumed_receipts.len(), 2);
    }

    #[test]
    fn reconciliation_matches_corrected_and_submitted_metadata() {
        let mappings = LastFmMappings {
            track_mappings: BTreeMap::from([(
                source_id("Artist", "Album", "Song"),
                "spotify:track:one".into(),
            )]),
            ..LastFmMappings::default()
        };
        let result = reconcile_incremental(
            &[ExternalScrobble {
                artist: "Corrected Artist".into(),
                album: "Corrected Album".into(),
                track: "Corrected Song".into(),
                timestamp: 10,
                submitted: Some(ScrobbleMetadata {
                    artist: "Artist".into(),
                    album: "Album".into(),
                    track: "Song".into(),
                }),
            }],
            &[receipt(
                ("Corrected Artist", "Corrected Album", "Corrected Song"),
                ("Artist", "Album", "Song"),
                10,
            )],
            &mappings,
            &BTreeSet::from(["spotify:track:one".into()]),
            0,
            20,
        );
        assert!(result.increments.is_empty());
        assert_eq!(result.consumed_receipts.len(), 1);
    }

    #[test]
    fn reconciliation_prefers_track_mapping_and_sums_aliases() {
        let mappings = LastFmMappings {
            track_mappings: BTreeMap::from([
                (
                    source_id("Artist", "Album", "Song"),
                    "spotify:track:explicit".into(),
                ),
                (
                    source_id("Artist", "Album", "Song (Live)"),
                    "spotify:track:shared".into(),
                ),
            ]),
            album_mappings: BTreeMap::from([(
                source_album_key("Artist", "Album"),
                LastFmAlbumMapping {
                    spotify_album_uri: "spotify:album:one".into(),
                    track_uris_by_name: BTreeMap::from([(
                        normalize_for_match("Song"),
                        "spotify:track:album".into(),
                    )]),
                },
            )]),
            ..LastFmMappings::default()
        };
        let result = reconcile_incremental(
            &[
                event("Artist", "Album", "Song", 10),
                event("Artist", "Album", "Song (Live)", 20),
                event("Artist", "Album", "Song (Live)", 30),
            ],
            &[],
            &mappings,
            &BTreeSet::from([
                "spotify:track:explicit".into(),
                "spotify:track:shared".into(),
            ]),
            0,
            40,
        );
        assert_eq!(result.increments["spotify:track:explicit"], 1);
        assert_eq!(result.increments["spotify:track:shared"], 2);
        assert_eq!(result.latest["spotify:track:shared"], 30);

        let album_only = LastFmMappings {
            album_mappings: mappings.album_mappings.clone(),
            ..LastFmMappings::default()
        };
        let album_result = reconcile_incremental(
            &[event("Artist", "Album", "Song", 10)],
            &[],
            &album_only,
            &BTreeSet::from(["spotify:track:album".into()]),
            0,
            20,
        );
        assert_eq!(album_result.increments["spotify:track:album"], 1);
    }

    #[test]
    fn reconciliation_ignores_and_unresolved_targets_are_independent() {
        let known = source_id("Known", "Album", "Song");
        let mappings = LastFmMappings {
            track_mappings: BTreeMap::from([
                (known, "spotify:track:known".into()),
                (
                    source_id("Unavailable", "Album", "Song"),
                    "spotify:track:unavailable".into(),
                ),
            ]),
            excluded_tracks: BTreeSet::from([source_id("Ignored", "Album", "Song")]),
            ignored_albums: BTreeSet::from([source_album_key("Album ignored", "Album")]),
            ignored_artists: BTreeSet::from([normalize_for_match("Artist ignored")]),
            ..LastFmMappings::default()
        };
        let result = reconcile_incremental(
            &[
                event("Known", "Album", "Song", 10),
                event("Missing", "Album", "Song", 11),
                event("Unavailable", "Album", "Song", 11),
                event("Ignored", "Album", "Song", 12),
                event("Album ignored", "Album", "Song", 13),
                event("Artist ignored", "Other", "Song", 14),
                event("Known", "Album", "Song", 99),
            ],
            &[],
            &mappings,
            &BTreeSet::from(["spotify:track:known".into()]),
            0,
            50,
        );
        assert_eq!(result.increments["spotify:track:known"], 1);
        assert_eq!(
            result.unresolved,
            vec![
                event("Missing", "Album", "Song", 11),
                event("Unavailable", "Album", "Song", 11),
            ]
        );
        assert!(
            reconcile_incremental(&[], &[], &mappings, &BTreeSet::new(), 0, 100,)
                .increments
                .is_empty()
        );
    }

    #[test]
    fn reconciliation_reprocessing_a_committed_window_is_a_no_op() {
        let mappings = LastFmMappings {
            track_mappings: BTreeMap::from([(
                source_id("Artist", "Album", "Song"),
                "spotify:track:one".into(),
            )]),
            ..LastFmMappings::default()
        };
        let result = reconcile_incremental(
            &[event("Artist", "Album", "Song", 10)],
            &[],
            &mappings,
            &BTreeSet::from(["spotify:track:one".into()]),
            20,
            30,
        );
        assert!(result.increments.is_empty());
        assert!(result.latest.is_empty());
        assert!(result.unresolved.is_empty());
    }

    #[test]
    fn incremental_updates_saturate_and_journal_recovery_is_exactly_once() {
        let mut before = Library::new();
        before.add(retune_core::model::NewTrack {
            uri: "spotify:track:one".into(),
            name: "Song".into(),
            source: SourceId::Music,
            ..retune_core::model::NewTrack::default()
        });
        before.tracks_mut()[0].play_count = u32::MAX - 1;
        let mut after = before.clone();
        apply_incremental_updates(
            &mut after,
            &BTreeMap::from([(String::from("spotify:track:one"), 2)]),
            &BTreeMap::from([(String::from("spotify:track:one"), 40)]),
        );
        assert_eq!(after.tracks()[0].play_count, u32::MAX);
        assert_eq!(after.tracks()[0].last_played_at, Some(40));
        let journal = LastFmApplicationJournal {
            before_library: before.clone(),
            after_library: after.clone(),
            checkpoint_before: Some(1),
            checkpoint_after: Some(2),
            backlog_before: Vec::new(),
            backlog_after: Vec::new(),
            consumed_receipts: Vec::new(),
        };
        assert_eq!(
            recover_application_journal(&mut before, &journal).unwrap(),
            JournalRecovery::AppliedBefore
        );
        assert_eq!(before, after);
        assert_eq!(
            recover_application_journal(&mut before, &journal).unwrap(),
            JournalRecovery::AlreadyApplied
        );
        before.tracks_mut()[0].play_count = 1;
        assert_eq!(
            recover_application_journal(&mut before, &journal),
            Err(JournalRecoveryError::Conflict)
        );
    }

    #[test]
    fn aggregation_input_is_sorted_oldest_first_with_deterministic_ties() {
        let mut scrobbles = vec![
            scrobble("B", "Album", "Track", 20),
            scrobble("A", "Album", "Track", 10),
            scrobble("A", "Album", "Other", 10),
        ];
        sort_scrobbles(&mut scrobbles);
        assert_eq!(
            scrobbles
                .iter()
                .map(|row| (row.artist.clone(), row.track.clone(), row.timestamp))
                .collect::<Vec<_>>(),
            vec![
                ("A".to_owned(), "Other".to_owned(), 10),
                ("A".to_owned(), "Track".to_owned(), 10),
                ("B".to_owned(), "Track".to_owned(), 20),
            ]
        );
    }

    #[test]
    fn aggregation_handles_a_large_unique_input_with_indexed_rows() {
        const UNIQUE_SCROBBLES: usize = 50_000;
        let scrobbles = (0..UNIQUE_SCROBBLES)
            .map(|index| scrobble("Artist", "Album", &format!("Track {index}"), index as u64))
            .collect::<Vec<_>>();
        let mut rows = Vec::new();

        aggregate_scrobbles(&mut rows, &scrobbles);

        assert_eq!(rows.len(), UNIQUE_SCROBBLES);
        assert_eq!(rows[0].track, "Track 0");
        assert_eq!(rows[UNIQUE_SCROBBLES - 1].track, "Track 49999");
    }

    #[test]
    fn source_runner_plans_probe_descending_pages_and_aggregate_without_cursor_advance() {
        let mut session =
            LastFmImportSessionV2::new_with_defaults("user".into(), 100, ImportDefaults::default());
        assert_eq!(source_runner_step(&session), SourceRunnerStep::Probe);

        session.total_pages = Some(3);
        session.next_page = 3;
        session.retryable_error = Some(RetryableError {
            message: "temporary".into(),
            attempt: 1,
            retryable: true,
        });
        assert_eq!(source_runner_step(&session), SourceRunnerStep::Page(3));
        assert_eq!(session.next_page, 3);
        assert_eq!(session.downloaded_pages, 0);

        session.downloaded_pages = 1;
        session.next_page = 2;
        assert_eq!(source_runner_step(&session), SourceRunnerStep::Page(2));
        session.downloaded_pages = 2;
        session.next_page = 1;
        assert_eq!(source_runner_step(&session), SourceRunnerStep::Page(1));
        session.downloaded_pages = 3;
        session.next_page = 0;
        assert_eq!(source_runner_step(&session), SourceRunnerStep::Aggregate);
    }

    #[test]
    fn source_page_windows_cover_full_and_tail_ranges() {
        assert_eq!(source_page_window(12, 12), vec![12, 11, 10, 9]);
        assert_eq!(source_page_window(3, 12), vec![3, 2, 1]);
        assert!(source_page_window(0, 12).is_empty());
    }

    #[tokio::test]
    async fn source_page_window_overlaps_fetches_and_checkpoints_in_descending_order() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        service.start_or_resume("user", 100, None).await.unwrap();
        service.set_metadata(4, 4).await.unwrap();

        let barrier = Arc::new(tokio::sync::Barrier::new(4));
        let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let completed = Arc::new(Mutex::new(Vec::new()));
        let releases = Arc::new(
            (0..5)
                .map(|_| Arc::new(tokio::sync::Notify::new()))
                .collect::<Vec<_>>(),
        );
        let fetch = {
            let barrier = Arc::clone(&barrier);
            let started = Arc::clone(&started);
            let completed = Arc::clone(&completed);
            let releases = Arc::clone(&releases);
            move |page| {
                let barrier = Arc::clone(&barrier);
                let started = Arc::clone(&started);
                let completed = Arc::clone(&completed);
                let release = Arc::clone(&releases[page as usize]);
                async move {
                    started.fetch_add(1, Ordering::SeqCst);
                    barrier.wait().await;
                    release.notified().await;
                    completed.lock().await.push(page);
                    SourcePageFetchResult::Success(parsed_page(
                        page,
                        4,
                        vec![scrobble("Artist", "Album", "Track", page as u64)],
                    ))
                }
            }
        };

        let runner = tokio::spawn(download_page_window(Arc::clone(&service), 4, 4, fetch));
        while started.load(Ordering::SeqCst) < 4 {
            tokio::task::yield_now().await;
        }
        assert_eq!(started.load(Ordering::SeqCst), 4);

        for (index, page) in [1_u32, 2, 3, 4].into_iter().enumerate() {
            releases[page as usize].notify_one();
            loop {
                if completed.lock().await.len() > index {
                    break;
                }
                tokio::task::yield_now().await;
            }
        }

        assert_eq!(completed.lock().await.as_slice(), &[1, 2, 3, 4]);
        assert!(matches!(
            runner.await.unwrap().unwrap(),
            SourceWindowOutcome::Complete(pages) if pages == vec![4, 3, 2, 1]
        ));
        let session = service.snapshot().await.unwrap();
        assert_eq!(session.downloaded_pages, 4);
        assert_eq!(session.next_page, 0);
        assert_eq!(session.phase, ImportPhase::Aggregating);
    }

    #[tokio::test]
    async fn source_page_window_keeps_only_the_contiguous_prefix_and_resumes_at_failure() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        service.start_or_resume("user", 100, None).await.unwrap();
        service.set_metadata(4, 4).await.unwrap();

        let barrier = Arc::new(tokio::sync::Barrier::new(4));
        let requested = Arc::new(Mutex::new(Vec::new()));
        let error = download_page_window(Arc::clone(&service), 4, 4, {
            let barrier = Arc::clone(&barrier);
            let requested = Arc::clone(&requested);
            move |page| {
                let barrier = Arc::clone(&barrier);
                let requested = Arc::clone(&requested);
                async move {
                    requested.lock().await.push(page);
                    barrier.wait().await;
                    if page == 3 {
                        SourcePageFetchResult::Permanent("failed page 3".into())
                    } else {
                        SourcePageFetchResult::Success(parsed_page(
                            page,
                            4,
                            vec![scrobble("Artist", "Album", "Track", page as u64)],
                        ))
                    }
                }
            }
        })
        .await
        .unwrap_err();

        assert_eq!(error, "failed page 3");
        assert_eq!(
            requested
                .lock()
                .await
                .iter()
                .copied()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([1, 2, 3, 4])
        );
        let partial = service.snapshot().await.unwrap();
        assert_eq!(partial.downloaded_pages, 1);
        assert_eq!(partial.next_page, 3);
        assert_eq!(partial.phase, ImportPhase::Downloading);
        assert_eq!(source_runner_step(&partial), SourceRunnerStep::Page(3));
        let manifest = service.store.read_manifest(&partial).unwrap().unwrap();
        assert_eq!(manifest.pages.keys().copied().collect::<Vec<_>>(), vec![4]);

        let resumed = download_page_window(
            Arc::clone(&service),
            partial.next_page,
            4,
            move |page| async move {
                SourcePageFetchResult::Success(parsed_page(
                    page,
                    4,
                    vec![scrobble("Artist", "Album", "Track", page as u64)],
                ))
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            resumed,
            SourceWindowOutcome::Complete(pages) if pages == vec![3, 2, 1]
        ));
    }

    #[tokio::test]
    async fn source_page_window_retryable_failure_preserves_prefix_and_retry_owner() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        service.start_or_resume("user", 100, None).await.unwrap();
        service.set_metadata(4, 4).await.unwrap();

        let barrier = Arc::new(tokio::sync::Barrier::new(4));
        let outcome = download_page_window(Arc::clone(&service), 4, 4, {
            let barrier = Arc::clone(&barrier);
            move |page| {
                let barrier = Arc::clone(&barrier);
                async move {
                    barrier.wait().await;
                    if page == 3 {
                        SourcePageFetchResult::Retryable("rate limited page 3".into())
                    } else {
                        SourcePageFetchResult::Success(parsed_page(
                            page,
                            4,
                            vec![scrobble("Artist", "Album", "Track", page as u64)],
                        ))
                    }
                }
            }
        })
        .await
        .unwrap();

        assert!(matches!(outcome, SourceWindowOutcome::Retryable));
        let partial = service.snapshot().await.unwrap();
        assert_eq!(partial.downloaded_pages, 1);
        assert_eq!(partial.next_page, 3);
        assert_eq!(source_runner_step(&partial), SourceRunnerStep::Page(3));
        assert_eq!(source_page_window(partial.next_page, 4), vec![3, 2, 1]);
        assert_eq!(
            partial.retryable_error,
            Some(RetryableError {
                message: "rate limited page 3".into(),
                attempt: 1,
                retryable: true,
            })
        );
        let manifest = service.store.read_manifest(&partial).unwrap().unwrap();
        assert_eq!(manifest.pages.keys().copied().collect::<Vec<_>>(), vec![4]);
    }

    #[tokio::test]
    async fn incremental_cache_resumes_and_filters_the_padded_query_window() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        service
            .mutate_sync(|state| {
                state.lastfm_username = Some("user".into());
                state.synced_through = Some(10);
                state.active = Some(IncrementalRange {
                    from: 10,
                    to: 20,
                    query_from: 9,
                    query_to: 21,
                    cache_id: incremental_cache_id("user", 10, 20),
                    next_page: 2,
                    total_pages: Some(2),
                    downloaded_pages: 0,
                    total_scrobbles: 6,
                });
                Ok(())
            })
            .await
            .unwrap();

        let page = parsed_page(
            1,
            2,
            vec![
                scrobble("Artist", "Album", "Before", 8),
                scrobble("Artist", "Album", "Inside", 11),
                scrobble("Artist", "Album", "After", 20),
            ],
        );
        assert!(service
            .checkpoint_incremental_page("user", 1, page.clone())
            .await
            .is_err());
        assert!(service
            .store
            .read_manifest(
                &incremental_cache_session(&service.sync_snapshot().await, "user").unwrap()
            )
            .unwrap()
            .is_none());

        service
            .checkpoint_incremental_page(
                "user",
                2,
                parsed_page(
                    2,
                    2,
                    vec![
                        scrobble("Artist", "Album", "Before", 9),
                        scrobble("Artist", "Album", "Inside", 10),
                        scrobble("Artist", "Album", "After", 21),
                    ],
                ),
            )
            .await
            .unwrap();
        assert_eq!(service.sync_snapshot().await.active.unwrap().next_page, 1);
        service
            .checkpoint_incremental_page("user", 1, page)
            .await
            .unwrap();

        let events = read_incremental_events(&service, "user").await.unwrap();
        assert_eq!(
            events
                .iter()
                .map(|event| event.timestamp)
                .collect::<Vec<_>>(),
            vec![10, 11]
        );
        let cache_id = service.sync_snapshot().await.active.unwrap().cache_id;
        service.store.remove_snapshot(&cache_id);
        service
            .mutate_sync(|state| {
                state.active = None;
                state.synced_through = Some(20);
                Ok(())
            })
            .await
            .unwrap();
        assert!(read_incremental_events(&service, "user")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn backlog_rehydration_rebuilds_incremental_rows_without_double_counting() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        let backlog = vec![event("Artist", "Album", "Song", 10); 2];
        service
            .mutate_sync(|state| {
                state.lastfm_username = Some("user".into());
                state.backlog = backlog.clone();
                Ok(())
            })
            .await
            .unwrap();

        service
            .sync_backlog_into_review("user", Some("spotify"))
            .await
            .unwrap();
        let first = service.snapshot().await.unwrap();
        assert_eq!(first.rows[0].play_count, 2);
        service
            .sync_backlog_into_review("user", Some("spotify"))
            .await
            .unwrap();
        let second = service.snapshot().await.unwrap();
        assert_eq!(second.rows[0].play_count, 2);
    }

    #[test]
    fn v2_session_without_downloaded_through_loads_safely() {
        let dir = tempfile::tempdir().unwrap();
        let store = ImportSessionStore::new(dir.path());
        let session =
            LastFmImportSessionV2::new_with_defaults("user".into(), 100, ImportDefaults::default());
        let mut json = serde_json::to_value(&session).unwrap();
        json.as_object_mut().unwrap().remove("downloadedThrough");
        fs::write(&store.path, serde_json::to_vec(&json).unwrap()).unwrap();

        assert_eq!(store.load().unwrap().unwrap().downloaded_through, None);
    }

    #[tokio::test]
    async fn downloaded_through_advances_monotonically_and_survives_reload() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        service.start_or_resume("user", 500, None).await.unwrap();
        service.set_metadata(3, 3).await.unwrap();
        service
            .checkpoint_page(
                3,
                &parsed_page(3, 3, vec![scrobble("Artist", "Album", "Track", 300)]),
            )
            .await
            .unwrap();
        assert_eq!(
            service.snapshot().await.unwrap().downloaded_through,
            Some(300)
        );
        service
            .checkpoint_page(
                2,
                &parsed_page(2, 3, vec![scrobble("Artist", "Album", "Track", 200)]),
            )
            .await
            .unwrap();
        assert_eq!(
            service.snapshot().await.unwrap().downloaded_through,
            Some(300)
        );
        service
            .checkpoint_page(
                1,
                &parsed_page(1, 3, vec![scrobble("Artist", "Album", "Track", 400)]),
            )
            .await
            .unwrap();
        let complete = service.snapshot().await.unwrap();
        assert_eq!(complete.downloaded_through, Some(400));
        assert_eq!(state_view(Some(&complete)).history_to, Some(500));
        assert_eq!(
            Service::new(dir.path())
                .snapshot()
                .await
                .unwrap()
                .downloaded_through,
            Some(400)
        );
    }

    #[test]
    fn startup_resume_plan_uses_the_persisted_source_identity_only() {
        let mut session = LastFmImportSessionV2::new_with_defaults(
            "fixed-user".into(),
            1786804381,
            ImportDefaults::default(),
        );
        assert_eq!(
            startup_resume_plan(Some(&session)),
            Some(("fixed-user".into(), 1786804381))
        );
        assert!(startup_lastfm_identity_matches(
            &session,
            Some("other-user")
        ));
        session.phase = ImportPhase::Aggregating;
        assert_eq!(
            startup_resume_plan(Some(&session)),
            Some(("fixed-user".into(), 1786804381))
        );
        assert!(startup_lastfm_identity_matches(
            &session,
            Some("fixed-user")
        ));
        assert!(!startup_lastfm_identity_matches(
            &session,
            Some("other-user")
        ));
        assert!(!startup_lastfm_identity_matches(&session, None));
        session.phase = ImportPhase::Review;
        assert_eq!(startup_resume_plan(Some(&session)), None);
        assert_eq!(startup_resume_plan(None), None);
    }

    #[test]
    fn cached_spotify_identity_only_trusts_an_exact_matching_cache() {
        let mut library = SpotifyLibraryState {
            account_id: "spotify-a".into(),
            complete: true,
            ..SpotifyLibraryState::default()
        };
        assert_eq!(
            cached_spotify_identity_matches("spotify-a", &library),
            Some(true)
        );
        assert_eq!(
            cached_spotify_identity_matches("spotify-b", &library),
            Some(false)
        );

        library.complete = false;
        assert_eq!(cached_spotify_identity_matches("spotify-a", &library), None);
    }

    #[test]
    fn session_account_matching_requires_bound_identity_for_owned_mutations() {
        let mut session = LastFmImportSessionV2::new("lastfm-user".into(), "spotify-a".into(), 1);
        assert!(session_account_matches(
            &session,
            "lastfm-user",
            "spotify-a",
            true
        ));
        assert!(!session_account_matches(
            &session,
            "lastfm-user",
            "spotify-b",
            true
        ));

        session.spotify_account_id = None;
        assert!(!session_account_matches(
            &session,
            "lastfm-user",
            "spotify-a",
            true
        ));
        assert!(session_account_matches(
            &session,
            "lastfm-user",
            "spotify-a",
            false
        ));
    }

    #[test]
    fn source_and_review_phases_choose_the_correct_account_boundary() {
        let mut session = LastFmImportSessionV2::new_with_defaults(
            "lastfm-user".into(),
            1,
            ImportDefaults::default(),
        );
        assert!(!requires_spotify_ownership(&session));
        session.total_pages = Some(1);
        session.phase = ImportPhase::Aggregating;
        assert!(!requires_spotify_ownership(&session));

        session.phase = ImportPhase::Review;
        assert!(!requires_spotify_ownership(&session));
        session.spotify_account_id = Some("spotify-a".into());
        assert!(requires_spotify_ownership(&session));
        session.phase = ImportPhase::Done;
        assert!(requires_spotify_ownership(&session));
        session.phase = ImportPhase::Suspended;
        assert!(requires_spotify_ownership(&session));
        session.spotify_account_id = None;
        assert!(!requires_spotify_ownership(&session));
    }

    fn parsed_page(
        page: u32,
        total_pages: u32,
        tracks: Vec<ParsedScrobble>,
    ) -> ParsedRecentTracksPage {
        ParsedRecentTracksPage {
            page,
            total_pages: Some(total_pages),
            total: Some(tracks.len() as u64),
            tracks,
            ..ParsedRecentTracksPage::default()
        }
    }

    async fn start_bound(service: &Service, username: &str, spotify: &str, history_to: u64) {
        service
            .start_or_resume(username, history_to, None)
            .await
            .unwrap();
        let mut session = service.snapshot().await.unwrap();
        session.spotify_account_id = Some(spotify.into());
        service.save(session).await.unwrap();
    }

    #[tokio::test]
    async fn completed_v2_sessions_backfill_reusable_mappings_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 10);
        aggregate_scrobbles(&mut session.rows, &[scrobble("Artist", "Album", "Song", 1)]);
        let row = session.rows[0].clone();
        session.phase = ImportPhase::Done;
        session.decisions.insert(
            row.stable_id.clone(),
            RowDecision {
                status: RowStatus::Done,
                excluded: false,
            },
        );
        session.matches.insert(
            row.stable_id.clone(),
            MatchResult {
                source_id: row.stable_id.clone(),
                search_term: "Song".into(),
                confidence: Some(Confidence::Exact),
                selected_uri: Some("spotify:album:album".into()),
                candidates: vec![AlbumCandidate {
                    uri: "spotify:album:album".into(),
                    name: "Album".into(),
                    artist: "Artist".into(),
                    track_uris: vec!["spotify:track:song".into()],
                    track_names: vec!["Song".into()],
                    ..AlbumCandidate::default()
                }],
                track_matches: BTreeMap::from([(
                    row.stable_id.clone(),
                    "spotify:track:song".into(),
                )]),
            },
        );
        service.save(session).await.unwrap();

        service.backfill_completed_mappings().await.unwrap();
        service.backfill_completed_mappings().await.unwrap();
        let mappings = service.mappings_for("user", Some("spotify")).await;
        assert_eq!(
            mappings.track_mappings[&row.stable_id],
            "spotify:track:song"
        );
        assert_eq!(
            mappings.album_mappings[&source_album_key("Artist", "Album")].track_uris_by_name
                [&normalize_for_match("Song")],
            "spotify:track:song"
        );
    }

    #[test]
    fn manifest_is_authoritative_and_acknowledged_page_damage_quarantines_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let store = ImportSessionStore::new(dir.path());
        let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 42);
        session.total_pages = Some(2);
        session.next_page = 2;
        let orphan = store.page_path(&session.cache_id, 2);
        fs::create_dir_all(orphan.parent().unwrap()).unwrap();
        fs::write(&orphan, b"orphan").unwrap();
        assert!(store.validate_cache(&session).is_ok());

        store
            .write_page(
                &session,
                &parsed_page(2, 2, vec![scrobble("Artist", "Album", "Track", 2)]),
            )
            .unwrap();
        assert!(store.validate_cache(&session).is_ok());
        fs::remove_file(&orphan).unwrap();
        session.downloaded_pages = 1;
        session.next_page = 1;
        store.save(&session).unwrap();
        assert!(store.load().unwrap().is_none());
        assert!(fs::read_dir(dir.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("quarantine")
        }));

        let dir = tempfile::tempdir().unwrap();
        let store = ImportSessionStore::new(dir.path());
        let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 42);
        session.total_pages = Some(1);
        session.next_page = 0;
        store
            .write_page(
                &session,
                &parsed_page(1, 1, vec![scrobble("Artist", "Album", "Track", 2)]),
            )
            .unwrap();
        session.downloaded_pages = 1;
        store.save(&session).unwrap();
        let page_path = store.page_path(&session.cache_id, 1);
        let damaged = CachedRawPage {
            lastfm_username: session.lastfm_username.clone(),
            history_to: 43,
            total_pages: 1,
            parsed: parsed_page(1, 1, vec![scrobble("Artist", "Album", "Track", 2)]),
        };
        fs::write(&page_path, serde_json::to_vec(&damaged).unwrap()).unwrap();
        assert!(store.load().unwrap().is_none());

        let dir = tempfile::tempdir().unwrap();
        let store = ImportSessionStore::new(dir.path());
        let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 42);
        session.total_pages = Some(1);
        session.next_page = 0;
        session.downloaded_pages = 1;
        let manifest = RawCacheManifest {
            version: SESSION_VERSION,
            cache_id: session.cache_id.clone(),
            lastfm_username: session.lastfm_username.clone(),
            history_to: session.history_to,
            total_pages: 1,
            pages: BTreeMap::from([(1, MAX_RAW_CACHE_BYTES + 1)]),
        };
        fs::create_dir_all(store.cache_path(&session.cache_id)).unwrap();
        fs::write(
            store.manifest_path(&session.cache_id),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        store.save(&session).unwrap();
        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn cache_validation_rejects_skipped_acknowledged_pages_and_malformed_cursors() {
        let dir = tempfile::tempdir().unwrap();
        let store = ImportSessionStore::new(dir.path());
        let mut skipped = LastFmImportSessionV2::new("user".into(), "spotify".into(), 42);
        skipped.total_pages = Some(3);
        skipped.downloaded_pages = 2;
        skipped.next_page = 1;
        for page in [3, 1] {
            store
                .write_page(
                    &skipped,
                    &parsed_page(
                        page,
                        3,
                        vec![scrobble("Artist", "Album", "Track", page as u64)],
                    ),
                )
                .unwrap();
        }
        assert!(store.validate_cache(&skipped).is_err());
        store.save(&skipped).unwrap();
        assert!(store.load().unwrap().is_none());

        let dir = tempfile::tempdir().unwrap();
        let store = ImportSessionStore::new(dir.path());
        let mut malformed = LastFmImportSessionV2::new("user".into(), "spotify".into(), 42);
        malformed.total_pages = Some(3);
        malformed.downloaded_pages = 2;
        malformed.next_page = 0;
        for page in [3, 2] {
            store
                .write_page(
                    &malformed,
                    &parsed_page(
                        page,
                        3,
                        vec![scrobble("Artist", "Album", "Track", page as u64)],
                    ),
                )
                .unwrap();
        }
        assert!(store.validate_cache(&malformed).is_err());
        store.save(&malformed).unwrap();
        assert!(store.load().unwrap().is_none());
    }

    #[tokio::test]
    async fn suspended_completed_source_revalidates_cache_before_aggregation() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        service.start_or_resume("user", 500, None).await.unwrap();
        service.set_metadata(2, 2).await.unwrap();
        for page in [2, 1] {
            service
                .checkpoint_page(
                    page,
                    &parsed_page(
                        page,
                        2,
                        vec![scrobble("Artist", "Album", "Track", page as u64)],
                    ),
                )
                .await
                .unwrap();
        }
        assert_eq!(
            service.snapshot().await.unwrap().phase,
            ImportPhase::Aggregating
        );
        service.suspend_for_account_mismatch().await.unwrap();
        let suspended = service.snapshot().await.unwrap();
        let store = ImportSessionStore::new(dir.path());
        fs::remove_file(store.page_path(&suspended.cache_id, 1)).unwrap();

        let reloaded = Service::new(dir.path());
        assert!(reloaded.snapshot().await.is_none());
        reloaded.start_or_resume("user", 500, None).await.unwrap();
        let fresh = reloaded.snapshot().await.unwrap();
        assert_eq!(fresh.phase, ImportPhase::Downloading);
        assert_eq!(fresh.downloaded_pages, 0);
        assert_eq!(fresh.next_page, 1);
    }

    #[tokio::test]
    async fn suspended_source_revalidates_cache_but_review_survives_deleted_raw_cache() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        service.start_or_resume("user", 500, None).await.unwrap();
        service.set_metadata(2, 2).await.unwrap();
        service
            .checkpoint_page(
                2,
                &parsed_page(2, 2, vec![scrobble("Artist", "Album", "Track", 2)]),
            )
            .await
            .unwrap();
        service.suspend_for_account_mismatch().await.unwrap();
        let suspended = service.snapshot().await.unwrap();
        let store = ImportSessionStore::new(dir.path());
        fs::remove_file(store.page_path(&suspended.cache_id, 2)).unwrap();

        service.start_or_resume("user", 500, None).await.unwrap();
        let restarted = service.snapshot().await.unwrap();
        assert_eq!(restarted.phase, ImportPhase::Downloading);
        assert_eq!(restarted.downloaded_pages, 0);
        assert_eq!(restarted.next_page, 1);

        let review_dir = tempfile::tempdir().unwrap();
        let review_service = Service::new(review_dir.path());
        review_service
            .start_or_resume("user", 500, None)
            .await
            .unwrap();
        review_service.set_metadata(1, 1).await.unwrap();
        review_service
            .checkpoint_page(
                1,
                &parsed_page(1, 1, vec![scrobble("Artist", "Album", "Track", 2)]),
            )
            .await
            .unwrap();
        review_service.aggregate_cached(None).await.unwrap();
        review_service.suspend_for_account_mismatch().await.unwrap();
        assert_eq!(
            Service::new(review_dir.path())
                .snapshot()
                .await
                .unwrap()
                .phase,
            ImportPhase::Suspended
        );
    }

    #[tokio::test]
    async fn retry_state_round_trips_without_advancing_the_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        service
            .start_or_resume("lastfm-user", 500, None)
            .await
            .unwrap();
        service.set_metadata(3, 600).await.unwrap();
        service
            .set_retryable_error(Some(RetryableError {
                message: "temporary".into(),
                attempt: 4,
                retryable: true,
            }))
            .await
            .unwrap();

        let session = Service::new(dir.path()).snapshot().await.unwrap();
        assert_eq!(session.next_page, 3);
        assert_eq!(session.downloaded_pages, 0);
        assert_eq!(session.retryable_error.unwrap().attempt, 4);
    }

    #[tokio::test]
    async fn spotify_binding_waits_for_the_first_review_match() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        service
            .start_or_resume("lastfm-user", 500, None)
            .await
            .unwrap();
        assert_eq!(service.snapshot().await.unwrap().spotify_account_id, None);
        service.set_metadata(1, 1).await.unwrap();
        service
            .checkpoint_page(
                1,
                &parsed_page(1, 1, vec![scrobble("Artist", "Album", "Track", 10)]),
            )
            .await
            .unwrap();
        service.aggregate_cached(None).await.unwrap();
        let source_id = service.snapshot().await.unwrap().rows[0].stable_id.clone();
        service
            .set_match(
                "lastfm-user",
                "spotify-user",
                1,
                MatchResult {
                    source_id,
                    search_term: "track search".into(),
                    confidence: Some(Confidence::Exact),
                    selected_uri: Some("spotify:track:target".into()),
                    candidates: Vec::new(),
                    track_matches: BTreeMap::new(),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            service
                .snapshot()
                .await
                .unwrap()
                .spotify_account_id
                .as_deref(),
            Some("spotify-user")
        );
    }

    #[tokio::test]
    async fn all_pages_are_present_before_aggregation_and_review() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        service
            .start_or_resume("lastfm-user", 500, None)
            .await
            .unwrap();
        service.set_metadata(2, 2).await.unwrap();
        service
            .checkpoint_page(
                2,
                &parsed_page(2, 2, vec![scrobble("Artist", "Album", "New", 20)]),
            )
            .await
            .unwrap();
        let partial = service.snapshot().await.unwrap();
        assert_eq!(partial.phase, ImportPhase::Downloading);
        assert!(partial.rows.is_empty());
        service
            .checkpoint_page(
                1,
                &parsed_page(1, 2, vec![scrobble("Artist", "Album", "Old", 10)]),
            )
            .await
            .unwrap();
        let complete = service.snapshot().await.unwrap();
        assert_eq!(complete.phase, ImportPhase::Aggregating);
        assert_eq!(complete.downloaded_pages, 2);
        assert!(complete.rows.is_empty());
        service.aggregate_cached(None).await.unwrap();
        let review = service.snapshot().await.unwrap();
        assert_eq!(review.phase, ImportPhase::Review);
        assert_eq!(
            review
                .rows
                .iter()
                .map(|row| row.track.as_str())
                .collect::<Vec<_>>(),
            ["Old", "New"]
        );
    }

    #[tokio::test]
    async fn empty_aggregate_enters_done() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        service
            .start_or_resume("lastfm-user", 500, None)
            .await
            .unwrap();
        service.set_metadata(1, 0).await.unwrap();
        service
            .checkpoint_page(1, &parsed_page(1, 1, Vec::new()))
            .await
            .unwrap();

        service.aggregate_cached(None).await.unwrap();

        let session = service.snapshot().await.unwrap();
        assert_eq!(session.phase, ImportPhase::Done);
        assert!(session.rows.is_empty());
        assert_eq!(session.remaining(), 0);
    }

    #[tokio::test]
    async fn checkpoint_discards_rows_at_or_after_history_cutoff() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        service
            .start_or_resume("lastfm-user", 500, None)
            .await
            .unwrap();
        service.set_metadata(1, 3).await.unwrap();
        service
            .checkpoint_page(
                1,
                &parsed_page(
                    1,
                    1,
                    vec![
                        scrobble("Artist", "Album", "Before", 499),
                        scrobble("Artist", "Album", "At cutoff", 500),
                        scrobble("Artist", "Album", "After", 501),
                    ],
                ),
            )
            .await
            .unwrap();

        let session = service.snapshot().await.unwrap();
        assert_eq!(session.included_scrobbles, 1);
        assert_eq!(session.phase, ImportPhase::Aggregating);
        service.aggregate_cached(None).await.unwrap();
        let session = service.snapshot().await.unwrap();
        assert_eq!(session.rows.len(), 1);
        assert_eq!(session.rows[0].track, "Before");
    }

    #[test]
    fn cache_identity_uses_exact_username_and_rejects_metadata_mismatch() {
        assert_ne!(
            snapshot_cache_id("user.name", 42),
            snapshot_cache_id("username", 42)
        );

        let dir = tempfile::tempdir().unwrap();
        let store = ImportSessionStore::new(dir.path());
        let mut session = LastFmImportSessionV2::new("user.name".into(), "spotify".into(), 42);
        session.total_pages = Some(1);
        session.next_page = 1;
        store
            .write_page(
                &session,
                &parsed_page(1, 1, vec![scrobble("Artist", "Album", "Track", 2)]),
            )
            .unwrap();

        let mut mismatch = session.clone();
        mismatch.lastfm_username = "user-name".into();
        mismatch.cache_id = session.cache_id.clone();
        assert!(store.validate_cache(&mismatch).is_err());
        store.save(&mismatch).unwrap();
        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn visible_batch_matching_is_lazy_and_accept_all_is_the_bulk_plan() {
        let mut session =
            LastFmImportSessionV2::new_with_defaults("user".into(), 100, ImportDefaults::default());
        aggregate_scrobbles(
            &mut session.rows,
            &[
                scrobble("Artist A", "Album A", "One", 1),
                scrobble("Artist B", "Album B", "Two", 2),
                scrobble("Artist C", "Album C", "Three", 3),
            ],
        );
        let first_id = session.rows[0].stable_id.clone();
        let first_key = "Artist A\u{1f}Album A".to_owned();
        session.page_options.insert(
            first_key,
            PageOptions {
                selected_track_ids: BTreeSet::from([first_id.clone()]),
                ..PageOptions::default()
            },
        );

        assert_eq!(
            batch_match_plan(&session, Some((1, "Artist A", "Album A"))),
            vec![(1, "Artist A".into(), "Album A".into())]
        );
        session.matches.insert(
            first_id.clone(),
            MatchResult {
                source_id: first_id,
                search_term: "track".into(),
                confidence: Some(Confidence::Exact),
                selected_uri: Some("spotify:track:first".into()),
                candidates: Vec::new(),
                track_matches: BTreeMap::new(),
            },
        );
        assert!(batch_match_plan(&session, Some((1, "Artist A", "Album A"))).is_empty());
        assert_eq!(
            batch_match_plan(&session, None),
            vec![
                (2, "Artist B".into(), "Album B".into()),
                (3, "Artist C".into(), "Album C".into()),
            ]
        );
    }

    #[test]
    fn review_batches_split_large_single_groups_into_stable_bounded_pages() {
        let rows = (0..205)
            .map(|index| SourceRow {
                stable_id: format!("source-{index}"),
                artist: "Artist".into(),
                album: String::new(),
                track: format!("Track {index}"),
                variants: Vec::new(),
                play_count: 1,
                earliest: index as u64,
                latest: index as u64,
            })
            .collect::<Vec<_>>();
        let batches = build_review_batches(&rows);

        assert_eq!(batches.len(), 3);
        assert_eq!(
            batches.iter().map(|batch| batch.page).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(batches[0].source_ids.len(), LASTFM_REVIEW_BATCH_SIZE);
        assert_eq!(batches[1].source_ids.len(), LASTFM_REVIEW_BATCH_SIZE);
        assert_eq!(batches[2].source_ids.len(), 5);
        assert_eq!(batches[0].source_ids[0], "source-0");
        assert_eq!(batches[1].source_ids[0], "source-100");
        assert_eq!(batches[2].source_ids[0], "source-200");
    }

    #[tokio::test]
    async fn split_batch_default_options_are_local_and_each_batch_can_commit() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 1000);
        aggregate_scrobbles(
            &mut session.rows,
            &(0..205)
                .map(|index| scrobble("Artist", "Album", &format!("Track {index}"), index + 1))
                .collect::<Vec<_>>(),
        );
        session.phase = ImportPhase::Review;
        service.save(session).await.unwrap();

        for batch_id in 1..=3 {
            let page = service.page(batch_id, "Artist", "Album").await.unwrap();
            let source_ids = page
                .rows
                .iter()
                .map(|item| item.source.stable_id.clone())
                .collect::<BTreeSet<_>>();
            assert_eq!(page.options.selected_track_ids, source_ids);
            let selected_ids = page
                .options
                .selected_track_ids
                .iter()
                .cloned()
                .collect::<Vec<_>>();
            service
                .commit_rows(
                    "user",
                    "spotify",
                    batch_id,
                    &selected_ids,
                    "Artist",
                    "Album",
                    page.options,
                )
                .await
                .unwrap();
        }

        assert_eq!(service.snapshot().await.unwrap().phase, ImportPhase::Done);
    }

    #[tokio::test]
    async fn queue_pages_are_bounded_and_validate_cursor_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 1000);
        aggregate_scrobbles(
            &mut session.rows,
            &(0..205)
                .map(|index| scrobble("Artist", "Album", &format!("Track {index}"), index + 1))
                .collect::<Vec<_>>(),
        );
        session.phase = ImportPhase::Review;
        service.save(session).await.unwrap();

        let first = service.queue_page(0, 2).await.unwrap();
        assert_eq!(first.total, 3);
        assert_eq!(first.items.len(), 2);
        assert_eq!(
            first
                .items
                .iter()
                .map(|item| item.source_count)
                .collect::<Vec<_>>(),
            [100, 100]
        );
        assert_eq!(first.next_cursor, Some(2));
        let second = service
            .queue_page(first.next_cursor.unwrap(), 2)
            .await
            .unwrap();
        assert_eq!(
            second
                .items
                .iter()
                .map(|item| item.source_count)
                .collect::<Vec<_>>(),
            [5]
        );
        assert_eq!(second.next_cursor, None);
        assert!(service.queue_page(0, 0).await.is_err());
        assert!(service
            .queue_page(0, LASTFM_QUEUE_PAGE_LIMIT + 1)
            .await
            .is_err());
        assert!(service.queue_page(first.total + 1, 1).await.is_err());
    }

    #[test]
    fn page_projection_does_not_reenter_whole_session_read_helpers() {
        let source = include_str!("lastfm_import.rs");
        let page = source
            .split("    pub(crate) async fn page(")
            .nth(1)
            .unwrap()
            .split("    async fn update_options(")
            .next()
            .unwrap();
        assert!(!page.contains("self.snapshot()"));
        assert!(!page.contains("review_batches("));
        assert!(!page.contains("requested_batch("));
        assert!(!page.contains("source_row_map("));
    }

    #[test]
    fn queue_projection_uses_bounded_page_options() {
        let source = include_str!("lastfm_import.rs");
        let queue_page = source
            .split("    pub(crate) async fn queue_page(")
            .nth(1)
            .unwrap()
            .split("    pub(crate) async fn page(")
            .next()
            .unwrap();
        let queue_item = source
            .split("fn queue_item(")
            .nth(1)
            .unwrap()
            .split("fn queue_status(")
            .next()
            .unwrap();

        for projection in [queue_page, queue_item] {
            assert!(!projection.contains("options_for_batch("));
            assert!(!projection.contains("review_batches("));
            assert!(!projection.contains("source_row_map("));
        }
        assert!(queue_item.contains("options_for_page_batch("));
    }

    #[tokio::test]
    async fn large_queue_follows_every_cursor_in_order_without_materializing_prior_slices() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        let count = 23_132_u32;
        let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 1000);
        session.phase = ImportPhase::Review;
        session.rows = (0..count)
            .map(|index| SourceRow {
                stable_id: format!("source-{index}"),
                artist: format!("Artist {index}"),
                album: format!("Album {index}"),
                track: format!("Track {index}"),
                variants: Vec::new(),
                play_count: 1,
                earliest: index as u64,
                latest: index as u64,
            })
            .collect();
        session.batches = (0..count)
            .map(|index| ImportBatch {
                page: index + 1,
                source_ids: vec![format!("source-{index}")],
            })
            .collect();
        *service.session.lock().await = Some(session);

        let mut cursor = 0;
        let mut seen_pages = Vec::with_capacity(count as usize);
        loop {
            let page = service.queue_page(cursor, 1000).await.unwrap();
            assert_eq!(page.total, count as usize);
            assert!(page.items.len() <= 1000);
            seen_pages.extend(page.items.iter().map(|item| item.page));
            match page.next_cursor {
                Some(next_cursor) => {
                    assert_eq!(next_cursor, cursor + page.items.len());
                    cursor = next_cursor;
                }
                None => break,
            }
        }

        assert_eq!(seen_pages.len(), count as usize);
        assert_eq!(seen_pages, (1..=count).collect::<Vec<_>>());
    }

    #[test]
    fn accept_all_entity_counts_are_unique_across_batches() {
        let mut session =
            LastFmImportSessionV2::new_with_defaults("user".into(), 100, ImportDefaults::default());
        aggregate_scrobbles(
            &mut session.rows,
            &[
                scrobble("Artist A", "Album A", "One", 1),
                scrobble("Artist B", "Album B", "Two", 2),
            ],
        );
        for row in &session.rows {
            session.page_options.insert(
                format!("{}\u{1f}{}", row.artist, row.album),
                PageOptions {
                    whole_album: true,
                    selected_track_ids: BTreeSet::from([row.stable_id.clone()]),
                    ..PageOptions::default()
                },
            );
            session.matches.insert(
                row.stable_id.clone(),
                MatchResult {
                    source_id: row.stable_id.clone(),
                    search_term: "album".into(),
                    confidence: Some(Confidence::Exact),
                    selected_uri: Some("spotify:album:shared".into()),
                    candidates: Vec::new(),
                    track_matches: BTreeMap::new(),
                },
            );
        }
        let (albums, tracks) = accept_all_entity_uris(&session);
        assert_eq!(albums, BTreeSet::from(["spotify:album:shared".into()]));
        assert!(tracks.is_empty());

        for options in session.page_options.values_mut() {
            options.whole_album = false;
        }
        for row in &session.rows {
            session
                .matches
                .get_mut(&row.stable_id)
                .unwrap()
                .track_matches =
                BTreeMap::from([(row.stable_id.clone(), "spotify:track:shared".into())]);
        }
        let (albums, tracks) = accept_all_entity_uris(&session);
        assert!(albums.is_empty());
        assert_eq!(tracks, BTreeSet::from(["spotify:track:shared".into()]));
    }

    #[tokio::test]
    async fn lazy_coordinator_shares_duplicate_opens_and_does_not_prefetch_adjacent_batches() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        let mut session =
            LastFmImportSessionV2::new_with_defaults("user".into(), 100, ImportDefaults::default());
        aggregate_scrobbles(
            &mut session.rows,
            &[
                scrobble("Artist A", "Album A", "One", 1),
                scrobble("Artist B", "Album B", "Two", 2),
            ],
        );
        session.phase = ImportPhase::Review;
        service.save(session).await.unwrap();

        let gate = Arc::new(tokio::sync::Mutex::new(()));
        let searches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let first_service = Arc::clone(&service);
        let first_gate = Arc::clone(&gate);
        let first_searches = Arc::clone(&searches);
        let first = async move {
            lazy_match_page_with_search(
                first_service.as_ref(),
                first_gate.as_ref(),
                1,
                "Artist A",
                "Album A",
                || async { Ok(("user".into(), "spotify".into())) },
                move |rows| {
                    let results = rows
                        .into_iter()
                        .map(|row| MatchResult {
                            source_id: row.stable_id.clone(),
                            search_term: row.track.clone(),
                            confidence: Some(Confidence::Exact),
                            selected_uri: Some("spotify:track:shared".into()),
                            candidates: Vec::new(),
                            track_matches: BTreeMap::from([(
                                row.stable_id,
                                "spotify:track:shared".into(),
                            )]),
                        })
                        .collect();
                    let searches = Arc::clone(&first_searches);
                    async move {
                        searches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        tokio::task::yield_now().await;
                        Ok(results)
                    }
                },
            )
            .await
        };
        let second_service = Arc::clone(&service);
        let second_gate = Arc::clone(&gate);
        let second_searches = Arc::clone(&searches);
        let second = async move {
            lazy_match_page_with_search(
                second_service.as_ref(),
                second_gate.as_ref(),
                1,
                "Artist A",
                "Album A",
                || async { Ok(("user".into(), "spotify".into())) },
                move |rows| {
                    let results = rows
                        .into_iter()
                        .map(|row| MatchResult {
                            source_id: row.stable_id.clone(),
                            search_term: row.track.clone(),
                            confidence: Some(Confidence::Exact),
                            selected_uri: Some("spotify:track:shared".into()),
                            candidates: Vec::new(),
                            track_matches: BTreeMap::from([(
                                row.stable_id,
                                "spotify:track:shared".into(),
                            )]),
                        })
                        .collect();
                    let searches = Arc::clone(&second_searches);
                    async move {
                        searches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        tokio::task::yield_now().await;
                        Ok(results)
                    }
                },
            )
            .await
        };
        let (first, second) = tokio::join!(first, second);
        assert!(first.unwrap().is_some());
        assert!(second.unwrap().is_some());
        assert_eq!(searches.load(std::sync::atomic::Ordering::SeqCst), 1);

        let session = service.snapshot().await.unwrap();
        assert_eq!(session.spotify_account_id.as_deref(), Some("spotify"));
        assert_eq!(session.matches.len(), 1);
        assert!(!session.matches.contains_key(&session.rows[1].stable_id));

        let cached_searches = Arc::clone(&searches);
        let cached = lazy_match_page_with_search(
            &service,
            gate.as_ref(),
            1,
            "Artist A",
            "Album A",
            || async { Ok(("user".into(), "spotify".into())) },
            move |_| async move {
                cached_searches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(Vec::new())
            },
        )
        .await
        .unwrap();
        assert!(cached.is_some());
        assert_eq!(searches.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn lazy_coordinator_suspends_before_persisting_after_account_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        let mut session =
            LastFmImportSessionV2::new_with_defaults("user".into(), 100, ImportDefaults::default());
        aggregate_scrobbles(
            &mut session.rows,
            &[scrobble("Artist", "Album", "Track", 1)],
        );
        session.phase = ImportPhase::Review;
        service.save(session).await.unwrap();

        let gate = tokio::sync::Mutex::new(());
        let account_checks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let searches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let error = lazy_match_page_with_search(
            &service,
            &gate,
            1,
            "Artist",
            "Album",
            {
                let account_checks = Arc::clone(&account_checks);
                move || {
                    let account = account_checks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    async move {
                        Ok((
                            "user".into(),
                            if account == 0 {
                                "spotify-a"
                            } else {
                                "spotify-b"
                            }
                            .into(),
                        ))
                    }
                }
            },
            {
                let searches = Arc::clone(&searches);
                move |rows| {
                    searches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    async move {
                        Ok(rows
                            .into_iter()
                            .map(|row| MatchResult {
                                source_id: row.stable_id,
                                search_term: row.track,
                                confidence: Some(Confidence::Exact),
                                selected_uri: Some("spotify:track:target".into()),
                                candidates: Vec::new(),
                                track_matches: BTreeMap::new(),
                            })
                            .collect())
                    }
                }
            },
        )
        .await
        .unwrap_err();

        assert!(error.contains("changed while matching"));
        assert_eq!(account_checks.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(searches.load(std::sync::atomic::Ordering::SeqCst), 1);
        let session = service.snapshot().await.unwrap();
        assert_eq!(session.phase, ImportPhase::Suspended);
        assert!(session.matches.is_empty());
        assert!(session.spotify_account_id.is_none());
    }

    #[tokio::test]
    async fn accept_all_preparation_is_sequential_and_dedupes_entities() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        let mut session =
            LastFmImportSessionV2::new_with_defaults("user".into(), 100, ImportDefaults::default());
        aggregate_scrobbles(
            &mut session.rows,
            &[
                scrobble("Artist A", "Album A", "One", 1),
                scrobble("Artist B", "Album B", "Two", 2),
                scrobble("Artist C", "Album C", "Three", 3),
            ],
        );
        session.phase = ImportPhase::Review;
        service.save(session).await.unwrap();
        let order = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let summary = prepare_accept_all_batches(&service, {
            let service = Arc::clone(&service);
            let order = Arc::clone(&order);
            move |batch_id, artist, album| {
                let service = Arc::clone(&service);
                let order = Arc::clone(&order);
                async move {
                    order.lock().await.push((artist.clone(), album.clone()));
                    let session = service.snapshot().await.unwrap();
                    let results = session
                        .rows
                        .iter()
                        .filter(|row| row.artist == artist && row.album == album)
                        .map(|row| MatchResult {
                            source_id: row.stable_id.clone(),
                            search_term: row.track.clone(),
                            confidence: Some(Confidence::Exact),
                            selected_uri: Some("spotify:track:shared".into()),
                            candidates: Vec::new(),
                            track_matches: BTreeMap::from([(
                                row.stable_id.clone(),
                                "spotify:track:shared".into(),
                            )]),
                        })
                        .collect();
                    service
                        .set_matches("user", "spotify", batch_id, results)
                        .await
                }
            }
        })
        .await
        .unwrap();

        assert_eq!(summary.album_entities, 0);
        assert_eq!(summary.track_entities, 1);
        assert_eq!(
            *order.lock().await,
            vec![
                ("Artist A".into(), "Album A".into()),
                ("Artist B".into(), "Album B".into()),
                ("Artist C".into(), "Album C".into()),
            ]
        );
    }

    #[tokio::test]
    async fn one_lazy_batch_uses_only_its_spotify_requests() {
        let client = retune_spotify::client::fake_client(
            [retune_spotify::client::Response::json(
                200,
                serde_json::json!({
                    "tracks": {
                        "items": [{
                            "uri": "spotify:track:one",
                            "name": "One",
                            "artists": [{"id": "artist", "name": "Artist"}],
                            "album": {
                                "id": "album",
                                "uri": "spotify:album:album",
                                "name": "Album"
                            }
                        }],
                        "next": null,
                        "total": 1
                    }
                }),
            )],
            "",
        );
        let rows = vec![SourceRow {
            stable_id: "artist\u{1f}\u{1f}one".into(),
            artist: "Artist".into(),
            album: String::new(),
            track: "One".into(),
            variants: Vec::new(),
            play_count: 1,
            earliest: 1,
            latest: 1,
        }];
        let matches = match_batch(&client, "Artist", "", &rows).await.unwrap();
        assert_eq!(matches.len(), 1);
        let requests = client.transport().requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].url.contains("/search?"));
        assert!(requests[0].url.contains("type=track"));
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
    fn setup_state_view_reports_review_only_remaining() {
        let setup = state_view(None);
        assert_eq!(setup.phase, None);
        assert_eq!(setup.username, None);
        assert_eq!(setup.spotify_account_id, None);
        assert_eq!(setup.remaining, 0);

        let mut session = LastFmImportSessionV2::new("rianjs".into(), "spotify-user".into(), 10);
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
    async fn page_fuzzy_groups_stay_inside_the_requested_batch() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 10);
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

        let session = service.snapshot().await.unwrap();
        let selected_page = review_batches(&session)
            .into_iter()
            .find(|batch| {
                batch_rows(batch, &source_row_map(&session))
                    .iter()
                    .any(|row| row.album == "Selected")
            })
            .unwrap()
            .page;
        let page = service
            .page(selected_page, "Artist", "Selected")
            .await
            .unwrap();
        assert_eq!(
            page.rows
                .iter()
                .map(|item| item.source.album.as_str())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["Selected"])
        );
        assert!(!page.fuzzy_groups.contains_key(&target));
        assert!(!page.locked_count_modes.contains(&target));
    }

    #[tokio::test]
    async fn page_projects_count_modes_to_visible_fuzzy_targets() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 10);
        aggregate_scrobbles(
            &mut session.rows,
            &[
                scrobble("Artist", "Hidden", "Track", 1),
                scrobble("Artist", "Visible", "Track", 2),
                scrobble("Artist", "Visible", "Track (Live)", 3),
            ],
        );
        let visible_ids = session
            .rows
            .iter()
            .filter(|row| row.album == "Visible")
            .map(|row| row.stable_id.clone())
            .collect::<Vec<_>>();
        let hidden_id = session
            .rows
            .iter()
            .find(|row| row.album == "Hidden")
            .unwrap()
            .stable_id
            .clone();
        let visible_target = "spotify:track:visible".to_owned();
        let hidden_target = "spotify:track:hidden".to_owned();
        for (source_id, target) in visible_ids
            .iter()
            .map(|id| (id, &visible_target))
            .chain(std::iter::once((&hidden_id, &hidden_target)))
        {
            session.matches.insert(
                source_id.clone(),
                MatchResult {
                    source_id: source_id.clone(),
                    search_term: String::new(),
                    confidence: Some(Confidence::Exact),
                    selected_uri: Some(target.clone()),
                    candidates: Vec::new(),
                    track_matches: BTreeMap::from([(source_id.clone(), target.clone())]),
                },
            );
        }
        session.decisions.insert(
            visible_ids[0].clone(),
            RowDecision {
                status: RowStatus::Done,
                excluded: false,
            },
        );
        session.decisions.insert(
            hidden_id,
            RowDecision {
                status: RowStatus::Done,
                excluded: false,
            },
        );
        session
            .count_modes
            .insert(visible_target.clone(), CountMode::Overwrite);
        session.count_modes.insert(hidden_target, CountMode::Zero);
        session.page_options.insert(
            "Artist\u{1f}Visible".into(),
            PageOptions {
                selected_track_ids: visible_ids.iter().cloned().collect(),
                ..PageOptions::default()
            },
        );
        session.phase = ImportPhase::Review;
        service.save(session).await.unwrap();

        let session = service.snapshot().await.unwrap();
        let visible_page = review_batches(&session)
            .into_iter()
            .find(|batch| {
                batch_rows(batch, &source_row_map(&session))
                    .iter()
                    .any(|row| row.album == "Visible")
            })
            .unwrap();
        let page = service
            .page(visible_page.page, "Artist", "Visible")
            .await
            .unwrap();
        assert_eq!(
            page.fuzzy_groups.keys().cloned().collect::<BTreeSet<_>>(),
            BTreeSet::from([visible_target.clone()])
        );
        assert_eq!(
            page.count_modes,
            BTreeMap::from([(visible_target.clone(), CountMode::Overwrite)])
        );
        assert_eq!(page.locked_count_modes, BTreeSet::from([visible_target]));
    }

    #[tokio::test]
    async fn count_mode_change_is_rejected_after_target_is_done() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 10);
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
        let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 10);
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
        let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 10);
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
        let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 10);
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
        start_bound(&service, "lastfm-user", "spotify-user", 500).await;
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
        service.aggregate_cached(None).await.unwrap();
        let mut session = service.snapshot().await.unwrap();
        session.phase = ImportPhase::Review;
        service.save(session.clone()).await.unwrap();
        for row in &session.rows {
            service
                .review_action(
                    "lastfm-user",
                    "spotify-user",
                    1,
                    Some(row.stable_id.as_str()),
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
    async fn album_review_actions_cascade_across_split_batches_and_restore_from_any_batch() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 1000);
        aggregate_scrobbles(
            &mut session.rows,
            &(0..205)
                .map(|index| scrobble("Artist", "Album", &format!("Track {index}"), index + 1))
                .collect::<Vec<_>>(),
        );
        session.phase = ImportPhase::Review;
        service.save(session.clone()).await.unwrap();

        assert!(service
            .review_action("user", "spotify", 1, None, "exclude", "Artist", "Album",)
            .await
            .is_err());
        assert!(service
            .review_action(
                "user",
                "spotify",
                1,
                Some("not-in-batch"),
                "exclude",
                "Artist",
                "Album",
            )
            .await
            .is_err());

        service
            .review_action(
                "user",
                "spotify",
                1,
                None,
                "ignore-album",
                "Artist",
                "Album",
            )
            .await
            .unwrap();
        let queue = service
            .queue_page(0, LASTFM_QUEUE_PAGE_LIMIT)
            .await
            .unwrap()
            .items;
        assert_eq!(queue.len(), 3);
        assert!(queue
            .iter()
            .all(|item| item.status == Some(QueueStatus::IgnoredAlbum) && !item.remaining));

        service
            .review_action("user", "spotify", 2, None, "restore", "Artist", "Album")
            .await
            .unwrap();
        let queue = service
            .queue_page(0, LASTFM_QUEUE_PAGE_LIMIT)
            .await
            .unwrap()
            .items;
        assert!(queue
            .iter()
            .all(|item| item.status.is_none() && item.remaining));

        service
            .review_action("user", "spotify", 3, None, "skip-album", "Artist", "Album")
            .await
            .unwrap();
        let queue = service
            .queue_page(0, LASTFM_QUEUE_PAGE_LIMIT)
            .await
            .unwrap()
            .items;
        assert!(queue
            .iter()
            .all(|item| item.status == Some(QueueStatus::Skipped) && item.remaining));
    }

    #[tokio::test]
    async fn owned_review_mutations_reject_mismatch_and_suspension() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 10);
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
            .review_action(
                "user",
                "spotify",
                1,
                Some("id"),
                "exclude",
                "Artist",
                "Album"
            )
            .await
            .is_err());
        assert!(service
            .update_options(
                "user",
                "spotify",
                1,
                "Artist",
                "Album",
                PageOptions::default(),
            )
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
                1,
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
            .select_match("user", "spotify", 1, "id", "spotify:track:target")
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
        let session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 42);
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

        let mut unknown = LastFmImportSessionV2::new("user".into(), "spotify".into(), 42);
        unknown.version = 99;
        store.save(&unknown).unwrap();
        assert_eq!(store.load().unwrap(), None);
        assert!(fs::read_dir(dir.path()).unwrap().count() >= 2);

        let mut too_large = LastFmImportSessionV2::new("user".into(), "spotify".into(), 42);
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
        start_bound(&service, "lastfm-user", "spotify-user", 500).await;
        service.set_metadata(2, 2).await.unwrap();
        let parsed = parsed_page(2, 2, vec![scrobble("Artist", "Album", "Track", 10)]);
        service.checkpoint_page(2, &parsed).await.unwrap();
        service.checkpoint_page(2, &parsed).await.unwrap();
        service
            .checkpoint_page(1, &parsed_page(1, 2, Vec::new()))
            .await
            .unwrap();
        service.aggregate_cached(None).await.unwrap();
        let resumed = Service::new(dir.path());
        let session = resumed.snapshot().await.unwrap();
        assert_eq!(session.next_page, 0);
        assert_eq!(session.rows.len(), 1);
        assert_eq!(session.included_scrobbles, 1);

        let mismatch = resumed.start_or_resume("other-user", 600, None).await;
        assert!(mismatch.is_err());
        assert_eq!(
            resumed.snapshot().await.unwrap().phase,
            ImportPhase::Suspended
        );
        let resumed_for_owner = resumed
            .start_or_resume("lastfm-user", 600, None)
            .await
            .unwrap();
        assert_eq!(resumed_for_owner.phase, Some(ImportPhase::Review));
    }

    #[tokio::test]
    async fn search_terms_preference_round_trips_on_resume() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        start_bound(&service, "lastfm-user", "spotify-user", 500).await;
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
        start_bound(&service, "lastfm-user", "spotify-user", 500).await;
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
        start_bound(&service, "lastfm-user", "spotify-user", 500).await;
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
    async fn matching_cannot_write_through_suspension() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        start_bound(&service, "lastfm-user", "spotify-user", 500).await;
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
            .set_matches(
                "lastfm-user",
                "spotify-user",
                1,
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
        let session = service.snapshot().await.unwrap();
        assert_eq!(session.phase, ImportPhase::Suspended);
        assert!(session.matches.is_empty());
    }

    #[tokio::test]
    async fn suspended_reads_are_redacted_and_empty() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        start_bound(&service, "prior-user", "prior-spotify", 500).await;
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
        assert!(service
            .queue_page(0, LASTFM_QUEUE_PAGE_LIMIT)
            .await
            .unwrap()
            .items
            .is_empty());
        assert!(service
            .page(1, "Prior Artist", "Prior Album")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn mismatched_pages_do_not_advance_and_page_batches_are_compact() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        start_bound(&service, "lastfm-user", "spotify-user", 500).await;
        service.set_metadata(2, 1).await.unwrap();
        let mismatched = parsed_page(2, 2, vec![scrobble("Artist", "Album", "Track", 10)]);
        assert!(service.checkpoint_page(1, &mismatched).await.is_err());
        assert_eq!(service.snapshot().await.unwrap().next_page, 2);
        service.checkpoint_page(2, &mismatched).await.unwrap();
        let duplicate_page = parsed_page(
            1,
            2,
            vec![
                scrobble("Artist", "Album", "Track", 10),
                scrobble("artist", "album", "track", 20),
            ],
        );
        service.checkpoint_page(1, &duplicate_page).await.unwrap();
        service.aggregate_cached(None).await.unwrap();
        let session = service.snapshot().await.unwrap();
        assert_eq!(session.batches[0].source_ids.len(), 1);
        assert_eq!(session.next_page, 0);
    }

    #[tokio::test]
    async fn queue_reports_exact_entity_counts_for_current_page_choices() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        start_bound(&service, "lastfm-user", "spotify-user", 500).await;
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
        service.aggregate_cached(None).await.unwrap();
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
                    1,
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
        let queue = service
            .queue_page(0, LASTFM_QUEUE_PAGE_LIMIT)
            .await
            .unwrap()
            .items;
        assert_eq!((queue[0].album_entities, queue[0].track_entities), (0, 2));

        service
            .update_options(
                "lastfm-user",
                "spotify-user",
                1,
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
        let queue = service
            .queue_page(0, LASTFM_QUEUE_PAGE_LIMIT)
            .await
            .unwrap()
            .items;
        assert_eq!((queue[0].album_entities, queue[0].track_entities), (1, 0));

        let selected_track_ids = rows
            .iter()
            .map(|row| row.stable_id.clone())
            .collect::<BTreeSet<_>>();
        service
            .update_options(
                "lastfm-user",
                "spotify-user",
                1,
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
        let queue = service
            .queue_page(0, LASTFM_QUEUE_PAGE_LIMIT)
            .await
            .unwrap()
            .items;
        assert_eq!((queue[0].album_entities, queue[0].track_entities), (0, 2));

        service
            .update_options(
                "lastfm-user",
                "spotify-user",
                1,
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
        let queue = service
            .queue_page(0, LASTFM_QUEUE_PAGE_LIMIT)
            .await
            .unwrap()
            .items;
        assert_eq!((queue[0].album_entities, queue[0].track_entities), (0, 0));
    }

    #[tokio::test]
    async fn selecting_an_album_candidate_remaps_every_related_source_track() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        start_bound(&service, "lastfm-user", "spotify-user", 500).await;
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
        service.aggregate_cached(None).await.unwrap();
        let mut review_session = service.snapshot().await.unwrap();
        review_session.phase = ImportPhase::Review;
        service.save(review_session).await.unwrap();
        let rows = service.snapshot().await.unwrap().rows;
        for row in &rows {
            let old_track = format!("spotify:track:old-{}", row.track.to_lowercase());
            let new_track = format!("spotify:track:new-{}", row.track.to_lowercase());
            let mut track_matches = BTreeMap::new();
            track_matches.insert(row.stable_id.clone(), old_track.clone());
            let mut candidates = vec![
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
                    track_uris: vec![new_track.clone()],
                    track_names: vec![row.track.clone()],
                    track_artists: vec!["Artist".into()],
                    track_albums: vec!["Alternate release".into()],
                    relation: Some(AlbumRelation::BestMatch),
                },
            ];
            if row.stable_id == rows[0].stable_id {
                candidates.push(AlbumCandidate {
                    uri: "spotify:track:rematched".into(),
                    name: row.track.clone(),
                    artist: "Artist".into(),
                    track_uris: vec!["spotify:track:rematched".into()],
                    track_names: vec![row.track.clone()],
                    track_artists: vec!["Artist".into()],
                    track_albums: vec!["The Classics".into()],
                    relation: None,
                });
            }
            service
                .set_match(
                    "lastfm-user",
                    "spotify-user",
                    1,
                    MatchResult {
                        source_id: row.stable_id.clone(),
                        search_term: "album search".into(),
                        confidence: Some(Confidence::Exact),
                        selected_uri: Some("spotify:album:old".into()),
                        candidates,
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
                1,
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

        let rows = service.snapshot().await.unwrap().rows;
        let first_id = rows[0].stable_id.clone();
        let second_id = rows[1].stable_id.clone();
        service
            .select_match(
                "lastfm-user",
                "spotify-user",
                1,
                &first_id,
                "spotify:track:rematched",
            )
            .await
            .unwrap();
        let session = service.snapshot().await.unwrap();
        let first = session.matches.get(&first_id).unwrap();
        assert_eq!(first.selected_uri.as_deref(), Some("spotify:album:new"));
        assert_eq!(first.confidence, Some(Confidence::Exact));
        assert_eq!(first.track_matches.len(), 2);
        assert_eq!(
            first.track_matches.get(&first_id).map(String::as_str),
            Some("spotify:track:rematched")
        );
        assert_eq!(
            first.track_matches.get(&second_id).map(String::as_str),
            Some("spotify:track:new-two")
        );
        assert_eq!(
            first
                .candidates
                .iter()
                .find(|candidate| candidate.uri == "spotify:album:new")
                .map(|candidate| candidate.name.as_str()),
            Some("Alternate release")
        );
        let sibling = session.matches.get(&second_id).unwrap();
        assert_eq!(sibling.selected_uri.as_deref(), Some("spotify:album:new"));
        assert_eq!(sibling.confidence, Some(Confidence::Exact));
        assert_eq!(
            sibling.track_matches.get(&second_id).map(String::as_str),
            Some("spotify:track:new-two")
        );
    }

    #[test]
    fn picker_candidate_refresh_preserves_selection_until_explicit_choice() {
        let old_track = "spotify:track:old".to_owned();
        let old_album = AlbumCandidate {
            uri: "spotify:album:old".into(),
            name: "Old release".into(),
            artist: "Artist".into(),
            track_uris: vec![old_track.clone()],
            track_names: vec!["One".into()],
            track_artists: vec!["Artist".into()],
            track_albums: vec!["Old release".into()],
            relation: Some(AlbumRelation::BestMatch),
        };
        let previous = MatchResult {
            source_id: "id".into(),
            search_term: "album search".into(),
            confidence: Some(Confidence::Exact),
            selected_uri: Some(old_album.uri.clone()),
            candidates: vec![old_album],
            track_matches: BTreeMap::from([(String::from("id"), old_track.clone())]),
        };
        let refreshed = match_result_for(
            "id".into(),
            "track search".into(),
            vec![AlbumCandidate {
                uri: "spotify:track:new".into(),
                name: "New result".into(),
                artist: "Artist".into(),
                track_uris: vec!["spotify:track:new".into()],
                track_names: vec!["One".into()],
                track_artists: vec!["Artist".into()],
                track_albums: vec!["Release".into()],
                relation: Some(AlbumRelation::BestMatch),
            }],
            "One",
            false,
        );
        let preserved = preserve_match_selection(refreshed, Some(&previous), "id");

        assert_eq!(preserved.selected_uri, previous.selected_uri);
        assert_eq!(preserved.track_matches, previous.track_matches);
        assert!(preserved
            .candidates
            .iter()
            .any(|candidate| candidate.uri == "spotify:album:old"));
        assert!(preserved
            .candidates
            .iter()
            .any(|candidate| candidate.uri == "spotify:track:new"));
    }
}

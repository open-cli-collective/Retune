use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
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
pub(crate) const MAX_SERIALIZED_SESSION_BYTES: usize = 100 * 1024 * 1024;
const MAX_RAW_CACHE_BYTES: u64 = 100 * 1024 * 1024;

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
    pub downloaded_pages: u32,
    pub total_scrobbles: u64,
    pub included_scrobbles: u64,
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
        if session.next_page < total_pages {
            for page in (session.next_page + 1)..=total_pages {
                if !manifest.pages.contains_key(&page) {
                    return Err("The Last.fm import cache is missing an acknowledged page.".into());
                }
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
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let target = self.path.with_extension(format!("quarantine-{stamp}"));
        fs::rename(&self.path, target)
            .map_err(|_| "Could not quarantine the Last.fm import session.".to_string())
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
    session.phase == ImportPhase::Suspended
        && session.rows.is_empty()
        && (session.total_pages.is_none()
            || session.total_pages.is_some_and(|total_pages| {
                session.downloaded_pages < total_pages || session.next_page != 0
            }))
}

pub(crate) struct Service {
    store: ImportSessionStore,
    session: Mutex<Option<LastFmImportSessionV2>>,
    lazy_match_lock: Mutex<()>,
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
            lazy_match_lock: Mutex::new(()),
            running: AtomicBool::new(false),
        })
    }

    pub(crate) async fn state(&self) -> ImportStateView {
        let session = self.session.lock().await;
        match session.as_ref() {
            Some(session) if session.phase == ImportPhase::Suspended => suspended_state_view(),
            Some(session) => state_view(Some(session)),
            None => state_view(None),
        }
    }

    async fn snapshot(&self) -> Option<LastFmImportSessionV2> {
        self.session.lock().await.clone()
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
            let Some(session) = session else {
                return Err("No Last.fm import session is active.".into());
            };
            if session.lastfm_username != username
                || session.spotify_account_id.as_deref() != Some(spotify_account_id)
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
        history_to: u64,
        defaults: Option<ImportDefaults>,
    ) -> Result<ImportStateView, String> {
        if let Some(defaults) = &defaults {
            defaults.validate()?;
        }
        if let Some(session) = self.snapshot().await {
            if suspended_source_phase(&session) && self.store.validate_cache(&session).is_err() {
                self.invalidate_snapshot().await?;
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
        self.store.write_page(&cache_session, &filtered)?;
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

    async fn aggregate_cached(&self) -> Result<ImportStateView, String> {
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
            let mut groups = BTreeMap::<(String, String), Vec<String>>::new();
            for row in &rows {
                groups
                    .entry((row.artist.clone(), row.album.clone()))
                    .or_default()
                    .push(row.stable_id.clone());
            }
            let batches = groups
                .into_values()
                .map(|source_ids| ImportBatch {
                    page: 0,
                    source_ids,
                })
                .collect::<Vec<_>>();
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
        let result = self
            .mutate_session(|current| {
                let Some(mut current) = current else {
                    return Err("No Last.fm import session is active.".into());
                };
                if current.cache_id != session.cache_id || current.phase != ImportPhase::Aggregating
                {
                    return Err("Last.fm import changed while aggregation was running.".into());
                }
                current.rows = rows;
                current.batches = batches;
                current.phase = if current.rows.is_empty() {
                    ImportPhase::Done
                } else {
                    ImportPhase::Review
                };
                current.retryable_error = None;
                Ok((Some(current.clone()), state_view(Some(&current))))
            })
            .await?;
        self.store.remove_snapshot(&session.cache_id);
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
                        .filter(|row| row.artist == row_artist && row.album == row_album)
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
                            .filter(|row| row.artist == row_artist && row.album == row_album)
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
    ImportStateView {
        phase: session.map(|session| session.phase),
        username: session.map(|session| session.lastfm_username.clone()),
        spotify_account_id: session.and_then(|session| session.spotify_account_id.clone()),
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
        defaults: session
            .map(|session| session.defaults.clone())
            .unwrap_or_default(),
        remaining: session
            .filter(|session| matches!(session.phase, ImportPhase::Review | ImportPhase::Done))
            .map(LastFmImportSessionV2::remaining)
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
        downloaded_pages: 0,
        total_scrobbles: 0,
        included_scrobbles: 0,
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
    if session.phase == ImportPhase::Suspended {
        return Ok(false);
    }
    match lastfm_username(app).await {
        Ok(username) if username == session.lastfm_username => Ok(true),
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
    let username = lastfm_username(&app).await?;
    let history_to = crate::history_cutoff_for_import(&app, &username).await?;
    let state = app.state::<crate::AppState>();
    let service = Arc::clone(&state.lastfm_import);
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SourceRunnerStep {
    Probe,
    Page(u32),
    Aggregate,
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
                            service.aggregate_cached().await?;
                        }
                        SourceRunnerStep::Page(page) => {
                            let payload = fetch_import_page_with_retry(
                                &lastfm,
                                &service,
                                &username,
                                page,
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
                            service.checkpoint_page(page, &parsed).await?;
                        }
                    }
                    let _ = app.emit("lastfm-import-changed", service.state().await);
                }
                ImportPhase::Aggregating => {
                    service.aggregate_cached().await?;
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
    let _ = app.emit("lastfm-import-changed", service.state().await);
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
            .import_recent_tracks_page(username, page, history_to)
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
    let Some(session) = service.snapshot().await else {
        return Ok(false);
    };
    let Some(expected) = session.spotify_account_id else {
        return Ok(true);
    };
    let state = app.state::<crate::AppState>();
    let _membership_guard = state.spotify_library_gate.lock().await;
    let current = state
        .spotify_library
        .lock()
        .expect("Spotify library mutex poisoned")
        .clone();
    if cached_spotify_identity_matches(&expected, &current) == Some(false) {
        service.suspend_for_account_mismatch().await?;
        return Ok(false);
    }
    assert_current_account_locked(&state, service)
        .await
        .map(|_| true)
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
    let Some(page) = service.page(artist, album).await else {
        return Ok(None);
    };
    let session = service
        .snapshot()
        .await
        .ok_or_else(|| "No Last.fm import session is active.".to_string())?;
    if batch_match_plan(&session, Some((artist, album))).is_empty() {
        let _membership_guard = spotify_library_gate.lock().await;
        current_account().await?;
        return Ok(Some(page));
    }

    // ponytail: one importer-wide lock; use per-batch locks only if throughput requires it.
    let _match_guard = service.lazy_match_lock.lock().await;
    let Some(page) = service.page(artist, album).await else {
        return Ok(None);
    };
    let session = service
        .snapshot()
        .await
        .ok_or_else(|| "No Last.fm import session is active.".to_string())?;
    if batch_match_plan(&session, Some((artist, album))).is_empty() {
        let _membership_guard = spotify_library_gate.lock().await;
        current_account().await?;
        return Ok(Some(page));
    }
    let initial_account = {
        let _membership_guard = spotify_library_gate.lock().await;
        current_account().await?
    };
    let rows = session
        .rows
        .iter()
        .filter(|row| row.artist == artist && row.album == album)
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
        .set_matches(&username, &spotify_account_id, results)
        .await?;
    Ok(service.page(artist, album).await)
}

async fn lazy_match_page(
    app: &tauri::AppHandle,
    service: &Service,
    artist: &str,
    album: &str,
) -> Result<Option<ImportPageView>, String> {
    let Some(page) = service.page(artist, album).await else {
        return Ok(None);
    };
    let session = service
        .snapshot()
        .await
        .ok_or_else(|| "No Last.fm import session is active.".to_string())?;
    if batch_match_plan(&session, Some((artist, album))).is_empty() {
        return cached_spotify_binding_is_current(app, service)
            .await
            .map(|current| current.then_some(page));
    }
    let state = app.state::<crate::AppState>();
    let state_ref = &*state;
    let page = lazy_match_page_with_search(
        service,
        &state_ref.spotify_library_gate,
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
    requested: Option<(&str, &str)>,
) -> Vec<(String, String)> {
    let keys = session
        .rows
        .iter()
        .map(|row| (row.artist.clone(), row.album.clone()))
        .collect::<BTreeSet<_>>();
    keys.into_iter()
        .filter(|(artist, album)| {
            let mut rows = session
                .rows
                .iter()
                .filter(|row| row.artist == *artist && row.album == *album);
            let selected = requested.is_some_and(|(requested_artist, requested_album)| {
                requested_artist == artist && requested_album == album
            });
            let remaining = requested.is_none()
                && rows
                    .clone()
                    .any(|row| is_actionable(session, &row.stable_id));
            (selected || remaining) && rows.any(|row| !session.matches.contains_key(&row.stable_id))
        })
        .collect()
}

fn accept_all_entity_uris(session: &LastFmImportSessionV2) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut album_uris = BTreeSet::new();
    let mut track_uris = BTreeSet::new();
    let mut grouped = BTreeMap::<(String, String), Vec<&SourceRow>>::new();
    for row in &session.rows {
        grouped
            .entry((row.artist.clone(), row.album.clone()))
            .or_default()
            .push(row);
    }
    for ((artist, album), rows) in grouped {
        let options = session.options_for(&artist, &album);
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
    lazy_match_page(&app, service.as_ref(), &artist, &album).await
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
    let _membership_guard = state.spotify_library_gate.lock().await;
    let (username, spotify_account_id) =
        assert_current_account_locked(&state, state.lastfm_import.as_ref()).await?;
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
    drop(_membership_guard);
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
    let _membership_guard = state.spotify_library_gate.lock().await;
    let (username, spotify_account_id) =
        assert_current_account_locked(&state, state.lastfm_import.as_ref()).await?;
    state
        .lastfm_import
        .update_options(&username, &spotify_account_id, &artist, &album, options)
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
    id: String,
    uri: String,
) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let _membership_guard = state.spotify_library_gate.lock().await;
    let (username, spotify_account_id) =
        assert_current_account_locked(&state, state.lastfm_import.as_ref()).await?;
    state
        .lastfm_import
        .select_match(&username, &spotify_account_id, &id, &uri)
        .await?;
    drop(_membership_guard);
    emit_import_changed(&app, state.lastfm_import.as_ref()).await
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_change_track(
    app: tauri::AppHandle,
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
        .set_match(&username, &spotify_account_id, result)
        .await?;
    drop(_membership_guard);
    emit_import_changed(&app, state.lastfm_import.as_ref()).await
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_change_album(
    app: tauri::AppHandle,
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
        .set_matches(&username, &spotify_account_id, matches)
        .await?;
    drop(_membership_guard);
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
    prepare_accept_all_batches(service, |artist, album| {
        let app = app_for_prepare.clone();
        async move {
            lazy_match_page(&app, service, &artist, &album)
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
    F: FnMut(String, String) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let session = service
        .snapshot()
        .await
        .ok_or_else(|| "No Last.fm import session is active.".to_string())?;
    for (artist, album) in batch_match_plan(&session, None) {
        prepare(artist, album).await?;
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
    artist: String,
    album: String,
) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let _membership_guard = state.spotify_library_gate.lock().await;
    let (username, spotify_account_id) =
        assert_current_account_locked(&state, state.lastfm_import.as_ref()).await?;
    let Some(page) = state.lastfm_import.page(&artist, &album).await else {
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
    drop(_membership_guard);
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

pub(crate) fn ignore_album(session: &mut LastFmImportSessionV2, artist: &str, album: &str) {
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

pub(crate) fn skip_album(session: &mut LastFmImportSessionV2, artist: &str, album: &str) {
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
        session.next_page = 1;
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
        session.next_page = 1;
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
        review_service.aggregate_cached().await.unwrap();
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
        service.aggregate_cached().await.unwrap();
        let source_id = service.snapshot().await.unwrap().rows[0].stable_id.clone();
        service
            .set_match(
                "lastfm-user",
                "spotify-user",
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
        service.aggregate_cached().await.unwrap();
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

        service.aggregate_cached().await.unwrap();

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
        service.aggregate_cached().await.unwrap();
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
            batch_match_plan(&session, Some(("Artist A", "Album A"))),
            vec![("Artist A".into(), "Album A".into())]
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
        assert!(batch_match_plan(&session, Some(("Artist A", "Album A"))).is_empty());
        assert_eq!(
            batch_match_plan(&session, None),
            vec![
                ("Artist B".into(), "Album B".into()),
                ("Artist C".into(), "Album C".into()),
            ]
        );
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
            move |artist, album| {
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
                    service.set_matches("user", "spotify", results).await
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
    async fn page_fuzzy_groups_only_include_rows_selected_for_import() {
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
        service.aggregate_cached().await.unwrap();
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
        service.aggregate_cached().await.unwrap();
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
        assert!(service.queue().await.is_empty());
        assert!(service.page("Prior Artist", "Prior Album").await.is_none());
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
        service.aggregate_cached().await.unwrap();
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
        service.aggregate_cached().await.unwrap();
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
        service.aggregate_cached().await.unwrap();
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

use std::collections::{BTreeMap, BTreeSet};

use retune_core::model::Library;
use serde::{Deserialize, Serialize};

use crate::lastfm::{AcceptedScrobbleReceipt, ScrobbleMetadata};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ImportPhase {
    Downloading,
    Aggregating,
    Review,
    Done,
    Suspended,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum CountMode {
    #[default]
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ReviewAction {
    Exclude,
    UndoExclude,
    IgnoreAlbum,
    IgnoreArtist,
    SkipAlbum,
    Restore,
}

impl ReviewAction {
    pub(super) fn requires_ids(self) -> bool {
        match self {
            Self::Exclude | Self::UndoExclude => true,
            Self::IgnoreAlbum | Self::IgnoreArtist | Self::SkipAlbum | Self::Restore => false,
        }
    }

    pub(super) fn sweeps_backlog(self) -> bool {
        match self {
            Self::IgnoreAlbum | Self::IgnoreArtist | Self::Restore => true,
            Self::Exclude | Self::UndoExclude | Self::SkipAlbum => false,
        }
    }
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
    pub(super) fn validate(&self) -> Result<(), String> {
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
    #[serde(default)]
    pub in_library: bool,
    pub track_uris: Vec<String>,
    #[serde(default)]
    pub track_names: Vec<String>,
    #[serde(default)]
    pub track_artists: Vec<String>,
    #[serde(default)]
    pub track_albums: Vec<String>,
    pub relation: Option<AlbumRelation>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CollectionAlbumCandidate {
    #[serde(flatten)]
    pub matching: AlbumCandidate,
    #[serde(default)]
    pub image_url: Option<String>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub album_type: Option<String>,
    #[serde(default)]
    pub total_tracks: u32,
    #[serde(default)]
    pub track_numbers: Vec<Option<u32>>,
    #[serde(default)]
    pub track_durations: Vec<u64>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CollectionAlbumMatchState {
    #[serde(default)]
    pub cached_candidates: Vec<CollectionAlbumCandidate>,
    #[serde(default)]
    pub selected_album_uris: Vec<String>,
    #[serde(default)]
    pub automatic_selection_disabled: bool,
    /// Candidate URIs inserted by the selected-album rerank, keyed by source row.
    /// Baseline search/library candidates are deliberately not recorded here.
    #[serde(default)]
    pub injected_candidate_uris: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum CollectionTrackMatchStatus {
    #[serde(rename = "matched")]
    Matched,
    #[serde(rename = "ambiguous")]
    Ambiguous,
    #[serde(rename = "unmatched")]
    Unmatched,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CollectionTrackStatus {
    pub uri: String,
    pub status: CollectionTrackMatchStatus,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CollectionAlbumCoverage {
    pub uri: String,
    pub matched: usize,
    pub unique_coverage: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CollectionAlbumPreviewCoverage {
    pub uri: String,
    pub selected: bool,
    pub matched: usize,
    pub unique_coverage: usize,
    pub marginal_matches: i32,
    pub ambiguity_changes: i32,
    pub track_statuses: Vec<CollectionTrackStatus>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CollectionCoverage {
    pub matched: usize,
    pub ambiguous: usize,
    pub unresolved: usize,
    pub selected_albums: Vec<CollectionAlbumCoverage>,
    pub previews: Vec<CollectionAlbumPreviewCoverage>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CollectionMatchView {
    pub cached_albums: Vec<CollectionAlbumCandidate>,
    pub selected_album_uris: Vec<String>,
    pub coverage: CollectionCoverage,
    pub whole_album_ready: bool,
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

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportMatchSelection {
    pub id: String,
    pub uri: String,
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
    pub(super) fn from_defaults(defaults: &ImportDefaults) -> Self {
        Self {
            import_content: defaults.import_content,
            include_historical_play_counts: defaults.include_historical_play_counts,
            whole_album: defaults.whole_album,
            ..Self::default()
        }
    }

    pub(super) fn validate(&self) -> Result<(), String> {
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
    /// `None` identifies batches written before source clustering was persisted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection_shaped: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub representative_artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub representative_album: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub album_labels: Vec<String>,
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
    pub collection_album_matches: BTreeMap<u32, CollectionAlbumMatchState>,
    #[serde(default)]
    pub default_count_mode: CountMode,
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
    pub applying_all: bool,
    pub spotify_limit: Option<crate::store::Cooldown>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum QueueStatus {
    Done,
    Skipped,
    IgnoredAlbum,
    IgnoredArtist,
    Excluded,
    Failed,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ApplyFailureCode {
    SpotifyRateLimited,
    SpotifyQuotaExhausted,
    #[default]
    #[serde(other)]
    ApplyFailed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub(crate) enum ImportApplyFinished {
    Succeeded {
        #[serde(rename = "batchId")]
        batch_id: u32,
    },
    Failed {
        #[serde(rename = "batchId")]
        batch_id: u32,
        code: ApplyFailureCode,
        message: String,
        #[serde(rename = "retryAt")]
        retry_at: Option<u64>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ApplyFailure {
    pub(super) code: ApplyFailureCode,
    pub(super) message: String,
    pub(super) endpoint_family: Option<String>,
    pub(super) retry_at: Option<u64>,
    pub(super) ambiguous_outcome: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportQueueItem {
    pub page: u32,
    pub artist: String,
    pub album: String,
    pub collection_shaped: bool,
    pub album_label_count: usize,
    pub play_count: u64,
    pub imported_play_count: u64,
    pub remaining_play_count: u64,
    pub latest: u64,
    pub source_count: usize,
    pub remaining: bool,
    pub album_entities: u32,
    pub track_entities: u32,
    pub status: Option<QueueStatus>,
    pub error: Option<String>,
    pub error_code: Option<ApplyFailureCode>,
    pub retry_at: Option<u64>,
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
    pub collection_shaped: bool,
    pub album_label_count: usize,
    pub page_number: usize,
    pub page_count: usize,
    pub rows: Vec<ImportPageItem>,
    pub options: PageOptions,
    pub fuzzy_groups: BTreeMap<String, Vec<SourceRow>>,
    pub count_modes: BTreeMap<String, CountMode>,
    pub resolved_counts: BTreeMap<String, u64>,
    pub locked_count_modes: BTreeSet<String>,
    pub collection: Option<CollectionMatchView>,
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

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LastFmAlbumMapping {
    pub spotify_album_uri: String,
    pub track_uris_by_name: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LastFmMappings {
    #[serde(default)]
    pub default_count_mode: CountMode,
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HistoryUpdate {
    pub uri: String,
    pub play_count: Option<u64>,
    pub earliest: Option<u64>,
    pub latest: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LastFmApplicationJournal {
    pub(super) before_library: Library,
    pub(super) after_library: Library,
    pub(super) checkpoint_before: Option<u64>,
    pub(super) checkpoint_after: Option<u64>,
    pub(super) backlog_before: Vec<ExternalScrobble>,
    pub(super) backlog_after: Vec<ExternalScrobble>,
    pub(super) consumed_receipts: Vec<AcceptedScrobbleReceipt>,
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RawCacheManifest {
    pub(super) version: u8,
    pub(super) cache_id: String,
    pub(super) lastfm_username: String,
    pub(super) history_to: u64,
    pub(super) total_pages: u32,
    pub(super) pages: BTreeMap<u32, u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CachedRawPage {
    pub(super) lastfm_username: String,
    pub(super) history_to: u64,
    pub(super) total_pages: u32,
    pub(super) parsed: ParsedRecentTracksPage,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct IncrementalRange {
    pub(super) from: u64,
    pub(super) to: u64,
    pub(super) query_from: u64,
    pub(super) query_to: u64,
    pub(super) cache_id: String,
    pub(super) next_page: u32,
    pub(super) total_pages: Option<u32>,
    pub(super) downloaded_pages: u32,
    pub(super) total_scrobbles: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum ApplyJobStatus {
    #[default]
    Queued,
    Running,
    Failed,
}

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum ApplyJobStage {
    #[default]
    Upstream,
    Local,
    Mappings,
    Decision,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum ApplyMembership {
    None,
    Album {
        uri: String,
        name: String,
        artist: String,
    },
    Tracks(Vec<String>),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ApplyMapping {
    pub(super) source_key: String,
    pub(super) artist: String,
    pub(super) album: String,
    pub(super) track: String,
    pub(super) target_uri: String,
    pub(super) album_uri: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ApplyPlan {
    pub(super) session_id: String,
    pub(super) lastfm_username: String,
    pub(super) spotify_account_id: String,
    pub(super) batch_id: u32,
    pub(super) artist: String,
    pub(super) album: String,
    pub(super) committed_ids: Vec<String>,
    #[serde(default)]
    pub(super) archive_batch: bool,
    pub(super) options: PageOptions,
    pub(super) membership: ApplyMembership,
    pub(super) updates: Vec<HistoryUpdate>,
    pub(super) metadata_uris: Vec<String>,
    pub(super) mappings: Vec<ApplyMapping>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ReviewApplyJob {
    pub(super) id: String,
    pub(super) plan: ApplyPlan,
    #[serde(default)]
    pub(super) status: ApplyJobStatus,
    #[serde(default)]
    pub(super) stage: ApplyJobStage,
    #[serde(default)]
    pub(super) attempt: u32,
    #[serde(default)]
    pub(super) error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) error_code: Option<ApplyFailureCode>,
    #[serde(default)]
    pub(super) retry_at: Option<u64>,
    #[serde(default)]
    pub(super) bulk_index: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AcceptAllCursor {
    pub(super) session_id: String,
    pub(super) lastfm_username: String,
    pub(super) spotify_account_id: String,
    pub(super) next_batch_index: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LastFmSyncState {
    pub(super) version: u8,
    pub(super) lastfm_username: Option<String>,
    pub(super) spotify_account_id: Option<String>,
    pub(super) synced_through: Option<u64>,
    pub(super) last_synced_at: Option<u64>,
    pub(super) active: Option<IncrementalRange>,
    pub(super) backlog: Vec<ExternalScrobble>,
    pub(super) sync_problem: Option<String>,
    pub(super) journal: Option<LastFmApplicationJournal>,
    #[serde(default)]
    pub(super) apply_queue: Vec<ReviewApplyJob>,
    #[serde(default)]
    pub(super) accept_all: Option<AcceptAllCursor>,
}

#[cfg(test)]
mod tests {
    use super::ReviewAction;

    #[test]
    fn review_action_wire_values_and_policy_are_closed() {
        for (action, wire, requires_ids, sweeps_backlog) in [
            (ReviewAction::Exclude, "exclude", true, false),
            (ReviewAction::UndoExclude, "undo-exclude", true, false),
            (ReviewAction::IgnoreAlbum, "ignore-album", false, true),
            (ReviewAction::IgnoreArtist, "ignore-artist", false, true),
            (ReviewAction::SkipAlbum, "skip-album", false, false),
            (ReviewAction::Restore, "restore", false, true),
        ] {
            let json = format!("\"{wire}\"");
            assert_eq!(serde_json::from_str::<ReviewAction>(&json).unwrap(), action);
            assert_eq!(serde_json::to_string(&action).unwrap(), json);
            assert_eq!(action.requires_ids(), requires_ids);
            assert_eq!(action.sweeps_backlog(), sweeps_backlog);
        }
        assert!(serde_json::from_str::<ReviewAction>("\"unknown\"").is_err());
    }
}

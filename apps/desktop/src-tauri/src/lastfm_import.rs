use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    future::Future,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

#[cfg(test)]
use retune_core::model::Library;
#[cfg(test)]
use serde_json::Value;
use tokio::sync::Mutex;
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

#[cfg(test)]
use crate::lastfm::{AcceptedScrobbleReceipt, ScrobbleMetadata};

pub(crate) const SESSION_VERSION: u8 = 2;
pub(crate) const LASTFM_REVIEW_BATCH_SIZE: usize = 100;
const LASTFM_PAGE_WINDOW_SIZE: u32 = 4;
const LASTFM_QUEUE_PAGE_LIMIT: usize = 1000;
pub(crate) const MAX_SERIALIZED_SESSION_BYTES: usize = 100 * 1024 * 1024;
const MAX_RAW_CACHE_BYTES: u64 = 100 * 1024 * 1024;
const LASTFM_SYNC_VERSION: u8 = 1;
pub(crate) const LASTFM_MAPPINGS_VERSION: u8 = 1;

pub(super) fn clear_search_quota(
    cooldown_store: &crate::store::FsCooldownStore,
    source: retune_spotify::client::SearchSource,
) -> Result<(), String> {
    cooldown_store
        .clear_quota_after_search(source, crate::unix_now())
        .map_err(|error| error.to_string())
}

mod application;
mod apply;
mod clustering;
mod collection;
pub(crate) mod commands;
mod coordinator;
mod incremental;
mod matching;
mod model;
mod reconciliation;
mod review;
mod service;
mod source;
mod store;
use apply::build_apply_plan;
#[cfg(test)]
use apply::{
    apply_failure_event, apply_work_pending, commit_apply_plan, effective_apply_session,
    effective_apply_session_for_job, execute_apply_job, retry_plan_matches_request,
    run_apply_upstream_effect,
};
use clustering::{build_review_batches, source_artists_compatible};
use collection::*;
pub(crate) use commands::{resume_persisted_apply, resume_persisted_import};
use coordinator::*;
use incremental::*;
pub(crate) use matching::classify_album_candidates_by_name;
use matching::{
    album_search_term, album_track_candidate, album_tracks_complete,
    automatic_album_candidate_for_rows, candidate_rank, classify_album_candidates_for_rows,
    collection_album_candidate, collection_album_candidate_from_release,
    collection_album_search_term, collection_album_summary, collection_best_title_matches,
    is_album_search_term, match_result_for, match_result_for_release, normalized_word_sequences,
    preserve_match_selection, rank_collection_candidates, release_track_match_index,
    significant_title_tokens, supported_album_summaries, titles_share_contained_words,
    track_search_term, update_selected_match, update_selected_release_match,
    without_known_source_suffix, CollectionMembership,
};
#[cfg(test)]
use matching::{
    album_track_match_index, automatic_album_candidate, collection_track_candidates,
    ratify_collection_result, ratify_collection_result_with_selected_albums,
};
#[allow(unused_imports)]
pub(crate) use model::LastFmAlbumMapping;
use model::{
    AcceptAllCursor, ApplyJobStage, ApplyJobStatus, IncrementalRange, LastFmSyncState,
    ReviewAction, ReviewApplyJob,
};
pub(crate) use model::{
    AcceptAllSummary, AlbumCandidate, AlbumRelation, CollectionAlbumCandidate,
    CollectionAlbumMatchState, CountMode, ImportBatch, ImportDefaults, ImportMatchSelection,
    ImportPageItem, ImportPageView, ImportPhase, ImportQueuePage, ImportStateView, JournalRecovery,
    LastFmApplicationJournal, LastFmImportSessionV2, LastFmMappings, MatchResult, PageOptions,
    ParsedRecentTracksPage, PersistedLastFmMappings, RetryableError, RowDecision, RowStatus,
    SourceRow, SourceVariant,
};
#[cfg(test)]
use model::{ApplyFailure, ApplyMembership, ApplyPlan};
#[allow(unused_imports)]
pub(crate) use model::{
    ApplyFailureCode, Confidence, ExternalScrobble, HistoryUpdate, JournalRecoveryError,
    ParsedScrobble, ReconciliationResult,
};
#[cfg(test)]
use model::{CachedRawPage, CollectionTrackMatchStatus, QueueStatus, RawCacheManifest};
use reconciliation::source_album_key;
#[allow(unused_imports)]
pub(crate) use reconciliation::{apply_history_updates, apply_metadata, resolved_timestamps};
pub(crate) use reconciliation::{
    apply_incremental_updates, reconcile_incremental, recover_application_journal,
    resolved_play_count,
};
use review::*;
pub(crate) use service::Service;
use service::{requires_spotify_ownership, RunnerGuard};
use source::{
    aggregate_incremental_scrobbles, discard_post_cutoff, download_page_window_with_checkpoint,
    fetch_incremental_page_with_retry, fetch_source_page, read_incremental_events, run_import,
    snapshot_cache_id, sort_scrobbles, startup_lastfm_identity_matches, startup_resume_plan,
    SourceWindowOutcome,
};
pub(crate) use source::{aggregate_scrobbles, normalize_for_match, parse_recent_tracks_page};
#[cfg(test)]
use source::{
    download_page_window, source_id, source_page_window, source_runner_step, SourcePageFetchResult,
    SourceRunnerStep,
};
use store::{
    incremental_cache_id, incremental_cache_session, suspended_source_phase, ImportSessionStore,
    IncrementalStore, MappingsStore, ReviewTransaction, ReviewTransactionStore,
};
pub(crate) use store::{
    load_mappings_for_recovery, normalize_restored_mappings, save_mappings_for_recovery,
};

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
            collection_album_matches: BTreeMap::new(),
            default_count_mode: CountMode::Sum,
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
        let customized = batch_options.is_some() || legacy_options.is_some();
        let mut options = batch_options
            .clone()
            .or(legacy_options.clone())
            .unwrap_or_else(|| PageOptions::from_defaults(&self.defaults));
        if customized {
            options
                .selected_track_ids
                .retain(|id| batch_ids.contains(id));
        } else {
            options.selected_track_ids = batch
                .source_ids
                .iter()
                .filter(|id| {
                    let id = (*id).as_str();
                    self.rows
                        .iter()
                        .any(|row| row.stable_id == id && is_actionable(self, &row.stable_id))
                })
                .cloned()
                .collect();
            let rows = batch
                .source_ids
                .iter()
                .filter_map(|id| self.rows.iter().find(|row| row.stable_id == *id))
                .collect::<Vec<_>>();
            options.whole_album =
                options.import_content && exact_album_match_for_rows(self, batch_id, &rows);
        }
        let rows = batch
            .source_ids
            .iter()
            .filter_map(|id| self.rows.iter().find(|row| row.stable_id == *id))
            .collect::<Vec<_>>();
        let collection_shaped = batch_is_collection_shaped(self, &batch, &rows);
        let has_collection_match = self.collection_album_matches.contains_key(&batch_id);
        if (collection_shaped && !has_collection_match)
            || (!album.is_empty() && has_collection_match)
        {
            options.whole_album = false;
        } else if album.is_empty() && options.whole_album {
            options.whole_album =
                options.import_content && exact_album_match_for_rows(self, batch_id, &rows);
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
        let customized = batch_options.is_some() || legacy_options.is_some();
        let mut options = batch_options
            .clone()
            .or(legacy_options.clone())
            .unwrap_or_else(|| PageOptions::from_defaults(&self.defaults));
        if customized {
            options
                .selected_track_ids
                .retain(|id| batch_ids.contains(id));
        } else {
            options.selected_track_ids = rows
                .iter()
                .filter(|row| is_actionable(self, &row.stable_id))
                .map(|row| row.stable_id.clone())
                .collect();
            options.whole_album =
                options.import_content && exact_album_match_for_rows(self, batch.page, rows);
        }
        let collection_shaped = batch_is_collection_shaped(self, batch, rows);
        let has_collection_match = self.collection_album_matches.contains_key(&batch.page);
        if (collection_shaped && !has_collection_match)
            || (!album.is_empty() && has_collection_match)
        {
            options.whole_album = false;
        } else if album.is_empty() && options.whole_album {
            options.whole_album =
                options.import_content && exact_album_match_for_rows(self, batch.page, rows);
        }
        options
    }
}

fn normalize_catalog_text(value: &str) -> String {
    value
        .nfd()
        .filter(|character| !is_combining_mark(*character) && character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn batch_options_key(batch_id: u32) -> String {
    format!("batch:{batch_id}")
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct BatchProjection {
    collection_shaped: bool,
    representative_artist: String,
    representative_album: String,
    album_labels: Vec<String>,
}

fn derived_batch_projection(rows: &[&SourceRow]) -> BatchProjection {
    let representative = rows.iter().copied().max_by(|left, right| {
        left.play_count.cmp(&right.play_count).then_with(|| {
            (
                right.artist.as_str(),
                right.album.as_str(),
                right.track.as_str(),
            )
                .cmp(&(
                    left.artist.as_str(),
                    left.album.as_str(),
                    left.track.as_str(),
                ))
        })
    });
    let album_labels = rows
        .iter()
        .map(|row| row.album.clone())
        .filter(|album| !album.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    BatchProjection {
        collection_shaped: album_labels.len() > 1 || rows.iter().any(|row| row.album.is_empty()),
        representative_artist: representative
            .map(|row| row.artist.clone())
            .unwrap_or_default(),
        representative_album: representative
            .map(|row| row.album.clone())
            .unwrap_or_default(),
        album_labels,
    }
}

fn batch_projection(batch: &ImportBatch, rows: &[&SourceRow]) -> BatchProjection {
    let derived = derived_batch_projection(rows);
    BatchProjection {
        collection_shaped: batch.collection_shaped.unwrap_or(derived.collection_shaped),
        representative_artist: batch
            .representative_artist
            .clone()
            .unwrap_or(derived.representative_artist),
        representative_album: batch
            .representative_album
            .clone()
            .unwrap_or(derived.representative_album),
        album_labels: if batch.album_labels.is_empty() {
            derived.album_labels
        } else {
            batch.album_labels.clone()
        },
    }
}

fn batch_is_collection_shaped(
    session: &LastFmImportSessionV2,
    batch: &ImportBatch,
    rows: &[&SourceRow],
) -> bool {
    batch_projection(batch, rows).collection_shaped
        || session.collection_album_matches.contains_key(&batch.page)
}

fn batch_projection_for_session(
    session: &LastFmImportSessionV2,
    batch_id: u32,
) -> Option<BatchProjection> {
    let rows = source_row_map(session);
    let batch = review_batches(session)
        .into_iter()
        .find(|batch| batch.page == batch_id)?;
    Some(batch_projection(&batch, &batch_rows(&batch, &rows)))
}

fn batch_is_collection_shaped_for_id(session: &LastFmImportSessionV2, batch_id: u32) -> bool {
    batch_projection_for_session(session, batch_id)
        .is_some_and(|projection| projection.collection_shaped)
        || session.collection_album_matches.contains_key(&batch_id)
}

fn requested_batch_containing_source(
    session: &LastFmImportSessionV2,
    batch_id: u32,
    source_id: &str,
) -> Option<ImportBatch> {
    review_batches(session)
        .into_iter()
        .find(|batch| batch.page == batch_id && batch.source_ids.iter().any(|id| id == source_id))
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

fn legacy_batch_is_protected(
    session: &LastFmImportSessionV2,
    batch: &ImportBatch,
    apply_queue: &[ReviewApplyJob],
) -> bool {
    session
        .page_options
        .contains_key(&batch_options_key(batch.page))
        || session.collection_album_matches.contains_key(&batch.page)
        || apply_queue.iter().any(|job| {
            job.plan.session_id == session.cache_id
                && job.plan.batch_id == batch.page
                && matches!(
                    job.status,
                    ApplyJobStatus::Queued | ApplyJobStatus::Running | ApplyJobStatus::Failed
                )
        })
}

fn upgrade_legacy_pending_batches(
    session: &mut LastFmImportSessionV2,
    apply_queue: &[ReviewApplyJob],
) -> bool {
    let legacy = session
        .batches
        .iter()
        .any(|batch| batch.collection_shaped.is_none());
    if !legacy || !review_phase_allowed(session.phase) {
        return false;
    }
    let rows_by_id = source_row_map(session);
    let mut protected_batches = Vec::new();
    let mut pending_rows = Vec::new();
    let mut reserved_pages = BTreeSet::new();
    for batch in &session.batches {
        let rows = batch_rows(batch, &rows_by_id);
        if batch.collection_shaped.is_some()
            || legacy_batch_is_protected(session, batch, apply_queue)
        {
            let mut preserved = batch.clone();
            if preserved.collection_shaped.is_none() {
                let projection = batch_projection(&preserved, &rows);
                preserved.collection_shaped = Some(projection.collection_shaped);
                preserved.representative_artist = Some(projection.representative_artist);
                preserved.representative_album = Some(projection.representative_album);
                preserved.album_labels = projection.album_labels;
            }
            reserved_pages.insert(preserved.page);
            protected_batches.push(preserved);
        } else {
            pending_rows.extend(rows.into_iter().cloned());
        }
    }
    if pending_rows.is_empty() {
        let changed = protected_batches != session.batches;
        if changed {
            protected_batches.sort_by_key(|batch| batch.page);
            session.batches = protected_batches;
        }
        return changed;
    }
    let mut next_page = 1;
    let mut upgraded = build_review_batches(&pending_rows);
    for batch in &mut upgraded {
        while reserved_pages.contains(&next_page) {
            next_page += 1;
        }
        batch.page = next_page;
        reserved_pages.insert(next_page);
        next_page += 1;
    }
    protected_batches.extend(upgraded);
    protected_batches.sort_by_key(|batch| batch.page);
    let changed = protected_batches != session.batches;
    if changed {
        session.batches = protected_batches;
    }
    changed
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
        if batch.page != batch_id {
            return false;
        }
        let batch_rows = batch_rows(batch, &rows);
        if batch_rows.len() != batch.source_ids.len() {
            return false;
        }
        let projection = batch_projection(batch, &batch_rows);
        projection.representative_artist == artist && projection.representative_album == album
    })
}

fn requested_collection_batch_with_album(
    session: &LastFmImportSessionV2,
    batch_id: u32,
    artist: &str,
) -> Result<(ImportBatch, String), String> {
    let rows = source_row_map(session);
    let Some(batch) = review_batches(session)
        .into_iter()
        .find(|batch| batch.page == batch_id)
    else {
        return Err("Unknown Last.fm import review batch.".into());
    };
    let batch_rows = batch_rows(&batch, &rows);
    if batch_rows.len() != batch.source_ids.len() {
        return Err("Album matching is available only for collection batches.".into());
    }
    let projection = batch_projection(&batch, &batch_rows);
    if projection.representative_artist != artist
        || (!projection.collection_shaped
            && !session.collection_album_matches.contains_key(&batch_id))
    {
        return Err("Album matching is available only for collection batches.".into());
    }
    Ok((batch, projection.representative_album))
}

fn requested_collection_batch(
    session: &LastFmImportSessionV2,
    batch_id: u32,
    artist: &str,
) -> Result<ImportBatch, String> {
    requested_collection_batch_with_album(session, batch_id, artist).map(|(batch, _)| batch)
}

fn is_converted_collection_batch(
    session: &LastFmImportSessionV2,
    batch_id: u32,
    album: &str,
) -> bool {
    !album.is_empty() && session.collection_album_matches.contains_key(&batch_id)
}

fn select_match_in_session(
    session: &mut LastFmImportSessionV2,
    batch_id: u32,
    source_id: &str,
    uri: &str,
) -> Result<(String, String), String> {
    let Some((row_artist, row_album, row_track)) = session
        .rows
        .iter()
        .find(|row| row.stable_id == source_id)
        .map(|row| (row.artist.clone(), row.album.clone(), row.track.clone()))
    else {
        return Err("Unknown Last.fm import source row.".into());
    };
    let Some(batch) = requested_batch_containing_source(session, batch_id, source_id) else {
        return Err("The source row does not belong to this review batch.".into());
    };
    let projection = batch_projection_for_session(session, batch_id)
        .ok_or_else(|| "The source row does not belong to this review batch.".to_string())?;
    let batch_identity = (
        projection.representative_artist,
        projection.representative_album,
    );
    let collection_shaped = batch_is_collection_shaped_for_id(session, batch_id);
    let batch_ids = batch.source_ids.iter().cloned().collect::<BTreeSet<_>>();
    let explicit_album = session.matches.get(source_id).is_some_and(|result| {
        spotify_share_uri(&result.search_term, "album")
            .ok()
            .flatten()
            .as_deref()
            == Some(uri)
    });
    let candidate = session
        .matches
        .get(source_id)
        .and_then(|result| {
            result
                .candidates
                .iter()
                .find(|candidate| candidate.uri == uri)
                .cloned()
        })
        .or_else(|| selected_album_track_candidate(session, batch_id, source_id, &row_album, uri))
        .or_else(|| {
            uri.starts_with("spotify:track:")
                .then(|| {
                    batch.source_ids.iter().find_map(|id| {
                        session
                            .matches
                            .get(id)?
                            .candidates
                            .iter()
                            .find(|candidate| candidate.uri == uri)
                            .cloned()
                    })
                })
                .flatten()
        });
    let Some(candidate) = candidate else {
        return Err("This source row has no Spotify candidates.".into());
    };
    if candidate.relation.is_none()
        && !candidate.uri.starts_with("spotify:track:")
        && !explicit_album
    {
        return Err("That Spotify match is not supported by the source track set.".into());
    }
    if candidate.uri.starts_with("spotify:album:") {
        let related = session
            .rows
            .iter()
            .filter(|row| batch_ids.contains(&row.stable_id))
            .cloned()
            .collect::<Vec<_>>();
        let mut group_track_matches = BTreeMap::new();
        for row in related {
            let Some(result) = session.matches.get_mut(&row.stable_id) else {
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
            update_selected_release_match(result, &row, &candidate);
            if let Some(uri) = result.track_matches.get(&row.stable_id) {
                group_track_matches.insert(row.stable_id, uri.clone());
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
        if row_album.is_empty() || collection_shaped {
            let result = session
                .matches
                .entry(source_id.to_owned())
                .or_insert_with(|| MatchResult {
                    source_id: source_id.to_owned(),
                    search_term: track_search_term(&row_artist, &row_track),
                    confidence: None,
                    selected_uri: None,
                    candidates: Vec::new(),
                    track_matches: BTreeMap::new(),
                });
            if !result
                .candidates
                .iter()
                .any(|existing| existing.uri == candidate.uri)
            {
                result.candidates.insert(0, candidate.clone());
            }
        }
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
                            group_track_matches.insert(mapped_id.clone(), mapped_uri.clone());
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
    Ok(batch_identity)
}

fn selected_album_track_candidate(
    session: &LastFmImportSessionV2,
    batch_id: u32,
    source_id: &str,
    row_album: &str,
    uri: &str,
) -> Option<AlbumCandidate> {
    if row_album.is_empty() || batch_is_collection_shaped_for_id(session, batch_id) {
        return collection_selected_albums(session, batch_id)
            .into_iter()
            .find_map(|album| album_track_candidate(&album.matching, uri, false));
    }
    let result = session.matches.get(source_id)?;
    let selected_uri = result.selected_uri.as_deref()?;
    let album = result.candidates.iter().find(|candidate| {
        candidate.uri == selected_uri && selected_uri.starts_with("spotify:album:")
    })?;
    album_track_candidate(album, uri, false)
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
        applying_all: false,
        spotify_limit: None,
    }
}

fn remaining_with_apply_queue(
    session: &LastFmImportSessionV2,
    apply_queue: &[ReviewApplyJob],
) -> usize {
    let archived = apply_queue
        .iter()
        .filter(|job| {
            job.plan.session_id == session.cache_id
                && matches!(job.status, ApplyJobStatus::Queued | ApplyJobStatus::Running)
        })
        .flat_map(|job| job.plan.committed_ids.iter())
        .collect::<BTreeSet<_>>();
    session
        .rows
        .iter()
        .filter(|row| !archived.contains(&row.stable_id))
        .filter(|row| {
            let decision = default_decision(session, &row.stable_id);
            matches!(decision.status, RowStatus::Pending | RowStatus::Skipped) && !decision.excluded
        })
        .count()
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
        applying_all: false,
        spotify_limit: None,
    }
}

async fn lastfm_username(lastfm: &crate::lastfm::Service) -> Result<String, String> {
    lastfm
        .state()
        .await
        .username
        .ok_or_else(|| "Connect Last.fm before importing its history.".to_string())
}

async fn ensure_import_readable<T, S>(
    service: &Service,
    lastfm: &crate::lastfm::Service,
    spotify_membership: &crate::spotify_membership::SpotifyMembership,
    provider: &impl Fn() -> Result<Arc<retune_spotify::client::SpotifyClient<T, S>>, String>,
    connection_state: impl Fn() -> Result<bool, String>,
) -> Result<bool, String>
where
    T: retune_spotify::client::Transport,
    S: retune_spotify::tokens::TokenStore,
{
    let Some(session) = service.snapshot().await else {
        return Ok(true);
    };
    match lastfm_username(lastfm).await {
        Ok(username) if username == session.lastfm_username => {
            if session.phase == ImportPhase::Suspended {
                if requires_spotify_ownership(&session) {
                    let _ = current_spotify_binding_is_current(
                        service,
                        lastfm,
                        spotify_membership,
                        provider,
                        connection_state,
                        true,
                    )
                    .await?;
                }
                return Ok(false);
            }
            if requires_spotify_ownership(&session) {
                current_spotify_binding_is_current(
                    service,
                    lastfm,
                    spotify_membership,
                    provider,
                    connection_state,
                    false,
                )
                .await
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

async fn current_spotify_binding_is_current<T, S>(
    service: &Service,
    lastfm: &crate::lastfm::Service,
    spotify_membership: &crate::spotify_membership::SpotifyMembership,
    provider: &impl Fn() -> Result<Arc<retune_spotify::client::SpotifyClient<T, S>>, String>,
    connection_state: impl FnOnce() -> Result<bool, String>,
    allow_suspended: bool,
) -> Result<bool, String>
where
    T: retune_spotify::client::Transport,
    S: retune_spotify::tokens::TokenStore,
{
    let Some(session) = service.snapshot().await else {
        return Ok(false);
    };
    if session.spotify_account_id.is_none() {
        return Ok(true);
    }
    let membership_guard = spotify_membership.lock().await;
    let expected = session.spotify_account_id.as_deref().unwrap_or_default();
    let cached = membership_guard.snapshot();
    if cached_spotify_identity_matches(expected, &cached) == Some(false) {
        service.suspend_for_account_mismatch().await?;
        return Ok(false);
    }
    let binding = match current_account_binding(
        service,
        lastfm,
        &membership_guard,
        provider,
        connection_state,
        false,
        true,
        allow_suspended,
    )
    .await
    {
        Ok((binding, _)) => binding,
        Err(_) => {
            service.suspend_for_account_mismatch().await?;
            return Ok(false);
        }
    };
    debug_assert_eq!(binding.lastfm_username, session.lastfm_username);
    Ok(true)
}

async fn current_import_view(
    service: &Service,
    lastfm: &crate::lastfm::Service,
) -> Result<ImportStateView, String> {
    let Some(session) = service.snapshot().await else {
        return Ok(service.state().await);
    };
    match lastfm
        .with_import_owner(&session.lastfm_username, || async {
            Ok(service.state().await)
        })
        .await?
    {
        Some(view) => Ok(view),
        None => {
            service.suspend_for_account_mismatch().await?;
            Ok(service.state().await)
        }
    }
}

fn cached_spotify_account(
    spotify_membership: &crate::spotify_membership::SpotifyMembership,
) -> Option<String> {
    let library = spotify_membership.snapshot();
    library.is_exact().then_some(library.account_id)
}

async fn recover_before_apply_job(
    library: &crate::library_state::LibraryState,
    lastfm: &Arc<crate::lastfm::Service>,
    spotify_membership: &crate::spotify_membership::SpotifyMembership,
    service: &Service,
) -> Result<(), String> {
    recover_pending_incremental_journal(library, lastfm, spotify_membership, service).await
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

async fn select_best_matches_for_batch(
    service: &Service,
    username: &str,
    spotify_account_id: &str,
    batch_id: u32,
    artist: &str,
    album: &str,
) -> Result<(), String> {
    let Some(page) = service.page(batch_id, artist, album).await else {
        return Err("Unknown Last.fm import review batch.".into());
    };
    let collection_shaped = service
        .snapshot()
        .await
        .is_some_and(|session| batch_is_collection_shaped_for_id(&session, batch_id));
    let mut selected_album_uris = BTreeSet::new();
    for item in page.rows {
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
        let Some(result) = item.match_result else {
            continue;
        };
        if result.selected_uri.is_some() {
            continue;
        }
        if collection_shaped {
            // Collection ratification already records only conservative track
            // matches; unresolved candidates must stay unresolved in Accept All.
            continue;
        }
        let Some(candidate) = best_candidate(&result) else {
            continue;
        };
        if candidate.uri.starts_with("spotify:album:")
            && !selected_album_uris.insert(candidate.uri.clone())
        {
            continue;
        }
        service
            .select_match(
                username,
                spotify_account_id,
                batch_id,
                &item.source.stable_id,
                &candidate.uri,
            )
            .await?;
    }
    Ok(())
}

async fn enqueue_next_accept_all_job(service: &Service) -> Result<bool, String> {
    loop {
        let sync = service.sync_snapshot().await;
        let Some(cursor) = sync.accept_all.clone() else {
            return Ok(false);
        };
        if sync
            .apply_queue
            .iter()
            .any(|job| job.status == ApplyJobStatus::Failed)
        {
            return Ok(false);
        }
        if sync
            .apply_queue
            .iter()
            .any(|job| matches!(job.status, ApplyJobStatus::Queued | ApplyJobStatus::Running))
        {
            return Ok(true);
        }
        let username = cursor.lastfm_username.clone();
        let spotify_account_id = cursor.spotify_account_id.clone();
        let Some(session) = service.snapshot().await else {
            return Err("No Last.fm import session is active.".into());
        };
        if session.cache_id != cursor.session_id
            || session.lastfm_username != cursor.lastfm_username
            || session.spotify_account_id.as_deref() != Some(cursor.spotify_account_id.as_str())
        {
            return Err("The Last.fm review changed while Accept All was queued.".into());
        }
        let batches = review_batches_for_read(&session);
        let Some(batch) = batches.get(cursor.next_batch_index).cloned() else {
            service
                .mutate_sync(|state| {
                    if state.accept_all.as_ref() == Some(&cursor) {
                        state.accept_all = None;
                    }
                    Ok(())
                })
                .await?;
            return Ok(false);
        };
        if sync.apply_queue.iter().any(|job| {
            job.plan.session_id == cursor.session_id
                && job.plan.batch_id == batch.page
                && job.status == ApplyJobStatus::Failed
        }) {
            return Ok(false);
        }
        let rows_by_id = source_row_map(&session);
        let rows = batch_rows(&batch, &rows_by_id);
        let Some(first) = rows.first() else {
            let next = cursor.next_batch_index.saturating_add(1);
            service
                .mutate_sync(|state| {
                    if let Some(cursor) = state.accept_all.as_mut() {
                        cursor.next_batch_index = next;
                    }
                    Ok(())
                })
                .await?;
            continue;
        };
        let artist = first.artist.clone();
        let album = first.album.clone();
        if rows
            .iter()
            .all(|row| !is_actionable(&session, &row.stable_id))
        {
            let next = cursor.next_batch_index.saturating_add(1);
            service
                .mutate_sync(|state| {
                    if let Some(cursor) = state.accept_all.as_mut() {
                        cursor.next_batch_index = next;
                    }
                    Ok(())
                })
                .await?;
            continue;
        }
        select_best_matches_for_batch(
            service,
            &username,
            &spotify_account_id,
            batch.page,
            &artist,
            &album,
        )
        .await?;
        let session = service
            .snapshot()
            .await
            .ok_or_else(|| "No Last.fm import session is active.".to_string())?;
        let options = session.options_for_page_batch(&batch, &artist, &album, &rows);
        let selected_ids = options
            .selected_track_ids
            .iter()
            .filter(|id| is_actionable(&session, id))
            .cloned()
            .collect::<Vec<_>>();
        if selected_ids.is_empty() {
            let next = cursor.next_batch_index.saturating_add(1);
            service
                .mutate_sync(|state| {
                    if let Some(cursor) = state.accept_all.as_mut() {
                        cursor.next_batch_index = next;
                    }
                    Ok(())
                })
                .await?;
            continue;
        }
        let plan = build_apply_plan(
            &session,
            &spotify_account_id,
            batch.page,
            &artist,
            &album,
            &selected_ids,
            true,
            options,
        )?;
        let next = cursor.next_batch_index.saturating_add(1);
        let id = format!("{}:{}", plan.session_id, plan.batch_id);
        service
            .mutate_sync(|state| {
                if state.accept_all.as_ref() != Some(&cursor) {
                    return Err("Accept All changed before its next batch was queued.".into());
                }
                if state.apply_queue.iter().any(|job| {
                    matches!(job.status, ApplyJobStatus::Queued | ApplyJobStatus::Running)
                }) {
                    return Ok(());
                }
                state
                    .accept_all
                    .as_mut()
                    .expect("cursor was checked")
                    .next_batch_index = next;
                state.apply_queue.retain(|job| job.id != id);
                state.apply_queue.push(ReviewApplyJob {
                    id,
                    plan,
                    status: ApplyJobStatus::Queued,
                    stage: ApplyJobStage::Upstream,
                    attempt: 0,
                    error: None,
                    error_code: None,
                    retry_at: None,
                    bulk_index: Some(cursor.next_batch_index),
                });
                Ok(())
            })
            .await?;
        log::info!(target: "lastfm_import", "accept-all queued batch={}", batch.page);
        return Ok(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use retune_spotify::client::{fake_client, Response, SearchSource};

    #[tokio::test]
    async fn importer_network_search_clears_quota_but_cache_hit_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let cooldown_store = crate::store::FsCooldownStore::new(dir.path());
        let now = crate::unix_now();
        let deadline = now.saturating_add(300);
        cooldown_store
            .record_cooldown("/search", crate::store::CooldownKind::Quota, deadline, now)
            .unwrap();
        let client = fake_client(
            [Response::json(
                200,
                serde_json::json!({
                    "albums": {"items": [], "next": null, "total": 0}
                }),
            )],
            "",
        );

        let (_, source) = album_candidates_with_source(&client, "Artist Album", None, None, &[])
            .await
            .unwrap();
        assert_eq!(source, SearchSource::Network);
        clear_search_quota(&cooldown_store, source).unwrap();
        assert!(cooldown_store.effective_cooldown(now).unwrap().is_none());

        cooldown_store
            .record_cooldown("/search", crate::store::CooldownKind::Quota, deadline, now)
            .unwrap();
        let (_, source) = album_candidates_with_source(&client, "Artist Album", None, None, &[])
            .await
            .unwrap();
        assert_eq!(source, SearchSource::Cache);
        clear_search_quota(&cooldown_store, source).unwrap();
        assert_eq!(
            cooldown_store
                .effective_cooldown(now)
                .unwrap()
                .unwrap()
                .deadline,
            deadline
        );
        assert_eq!(client.transport().requests().len(), 1);
    }
}

#[cfg(test)]
mod integration_tests;

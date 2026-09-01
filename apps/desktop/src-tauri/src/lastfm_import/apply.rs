use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
};

use super::{
    batch_is_collection_shaped, batch_options_key, batch_rows, collection_selected_albums,
    exact_album_match_for_rows, historical_counts_for_targets, is_actionable,
    is_converted_collection_batch, matched_track_uri, matched_track_uri_for_row,
    membership_uris_for_import,
    model::{
        AcceptAllCursor, ApplyFailure, ApplyFailureCode, ApplyJobStage, ApplyJobStatus,
        ApplyMapping, ApplyMembership, ApplyPlan, HistoryUpdate, ImportApplyFinished,
        ImportStateView, LastFmImportSessionV2, LastFmMappings, PageOptions, ReviewApplyJob,
        RowDecision, RowStatus, SourceRow,
    },
    reconciliation::{resolved_timestamps, source_album_key},
    requested_batch, review_phase_allowed, selected_collection_album,
    selected_collection_album_for_rows,
    source::normalize_for_match,
    source_row_map, update_review_phase, Service,
};

impl ApplyFailure {
    pub(super) fn apply_failed(message: impl Into<String>) -> Self {
        Self {
            code: ApplyFailureCode::ApplyFailed,
            message: message.into(),
            endpoint_family: None,
            retry_at: None,
            ambiguous_outcome: false,
        }
    }
}

impl From<String> for ApplyFailure {
    fn from(message: String) -> Self {
        Self::apply_failed(message)
    }
}

impl From<&str> for ApplyFailure {
    fn from(message: &str) -> Self {
        Self::apply_failed(message)
    }
}

impl From<crate::spotify_membership::SpotifyActionFailure> for ApplyFailure {
    fn from(failure: crate::spotify_membership::SpotifyActionFailure) -> Self {
        let code = match failure.kind {
            crate::spotify_membership::SpotifyActionFailureKind::RateLimited => {
                ApplyFailureCode::SpotifyRateLimited
            }
            crate::spotify_membership::SpotifyActionFailureKind::QuotaExhausted => {
                ApplyFailureCode::SpotifyQuotaExhausted
            }
            crate::spotify_membership::SpotifyActionFailureKind::Other => {
                ApplyFailureCode::ApplyFailed
            }
        };
        Self {
            code,
            message: failure.message,
            endpoint_family: failure.endpoint_family,
            retry_at: failure.retry_at,
            ambiguous_outcome: failure.ambiguous_outcome,
        }
    }
}

fn apply_plan_to_effective_session(session: &mut LastFmImportSessionV2, plan: &ApplyPlan) {
    if plan.session_id != session.cache_id {
        return;
    }
    session
        .page_options
        .insert(batch_options_key(plan.batch_id), plan.options.clone());
    let default_count_mode = session.default_count_mode;
    for id in &plan.committed_ids {
        session.decisions.insert(
            id.clone(),
            RowDecision {
                status: RowStatus::Done,
                excluded: false,
            },
        );
    }
    for update in &plan.updates {
        if update.play_count.is_some() {
            session
                .count_modes
                .entry(update.uri.clone())
                .or_insert(default_count_mode);
        }
    }
}

pub(super) fn effective_apply_session(
    session: &LastFmImportSessionV2,
    queued_jobs: &[ReviewApplyJob],
) -> LastFmImportSessionV2 {
    let mut effective = session.clone();
    for job in queued_jobs.iter().filter(|job| {
        matches!(job.status, ApplyJobStatus::Queued | ApplyJobStatus::Running)
            && job.plan.session_id == session.cache_id
    }) {
        apply_plan_to_effective_session(&mut effective, &job.plan);
    }
    effective
}

pub(super) fn effective_apply_session_for_job(
    session: &LastFmImportSessionV2,
    queued_jobs: &[ReviewApplyJob],
    job_id: &str,
) -> LastFmImportSessionV2 {
    let prefix = queued_jobs
        .iter()
        .position(|job| job.id == job_id && job.status == ApplyJobStatus::Failed)
        .map(|index| &queued_jobs[..index])
        .unwrap_or(queued_jobs);
    effective_apply_session(session, prefix)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn retry_plan_matches_request(
    plan: &ApplyPlan,
    spotify_account_id: &str,
    batch_id: u32,
    artist: &str,
    album: &str,
    selected_ids: &[String],
    archive_batch: bool,
    options: &PageOptions,
) -> bool {
    let selected = selected_ids.iter().cloned().collect::<BTreeSet<_>>();
    let committed = plan.committed_ids.iter().cloned().collect::<BTreeSet<_>>();
    plan.spotify_account_id == spotify_account_id
        && plan.batch_id == batch_id
        && plan.artist == artist
        && plan.album == album
        && plan.archive_batch == archive_batch
        && plan.options == *options
        && if archive_batch {
            selected.is_subset(&committed)
        } else {
            selected == committed
        }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_apply_plan(
    session: &LastFmImportSessionV2,
    spotify_account_id: &str,
    batch_id: u32,
    artist: &str,
    album: &str,
    selected_ids: &[String],
    archive_batch: bool,
    options: PageOptions,
) -> Result<ApplyPlan, String> {
    options.validate()?;
    if session
        .spotify_account_id
        .as_deref()
        .is_some_and(|bound| bound != spotify_account_id)
    {
        return Err("The Last.fm review belongs to another Spotify account.".into());
    }
    let Some(batch) = requested_batch(session, batch_id, artist, album) else {
        return Err("Unknown Last.fm import review batch.".into());
    };
    let rows_by_id = source_row_map(session);
    let all_batch_rows = batch_rows(&batch, &rows_by_id);
    let collection_shaped = batch_is_collection_shaped(session, &batch, &all_batch_rows);
    let converted_collection = is_converted_collection_batch(session, batch_id, album);
    if converted_collection && options.whole_album {
        return Err("Whole-album import is unavailable after switching to album matches.".into());
    }
    if collection_shaped
        && options.whole_album
        && !session.collection_album_matches.contains_key(&batch_id)
    {
        let row_refs = all_batch_rows.clone();
        if !exact_album_match_for_rows(session, batch_id, &row_refs) {
            return Err(
                "Choose one supported Spotify album match before importing a collection as a whole album."
                    .into(),
            );
        }
    }
    let selected = selected_ids.iter().cloned().collect::<BTreeSet<_>>();
    if selected_ids
        .iter()
        .any(|id| !batch.source_ids.iter().any(|source_id| source_id == id))
    {
        return Err("A selected source row does not belong to this review batch.".into());
    }
    let rows = batch_rows(&batch, &rows_by_id)
        .into_iter()
        .filter(|row| selected.contains(&row.stable_id))
        .filter(|row| is_actionable(session, &row.stable_id))
        .cloned()
        .collect::<Vec<_>>();
    if rows.is_empty() {
        return Err("Select at least one actionable source row before accepting.".into());
    }
    if collection_shaped && options.whole_album {
        let row_refs = rows.iter().collect::<Vec<_>>();
        if !exact_album_match_for_rows(session, batch_id, &row_refs) {
            return Err(
                "Choose one coherent Spotify album before importing a collection as a whole album."
                    .into(),
            );
        }
    }

    let mut target_by_source = BTreeMap::<String, String>::new();
    for row in &rows {
        if let Some(result) = session.matches.get(&row.stable_id) {
            if let Some(uri) = matched_track_uri_for_row(result, row, collection_shaped) {
                target_by_source.insert(row.stable_id.clone(), uri);
            }
        }
    }
    if (options.include_historical_play_counts || !options.whole_album)
        && rows
            .iter()
            .any(|row| !target_by_source.contains_key(&row.stable_id))
    {
        return Err(
            "Every selected source track needs a supported Spotify match before this batch can be accepted. Change its match or uncheck it first."
                .into(),
        );
    }

    let mut metadata_uris = target_by_source.values().cloned().collect::<BTreeSet<_>>();
    let membership = if options.import_content && options.whole_album {
        if collection_shaped {
            let row_refs = rows.iter().collect::<Vec<_>>();
            let candidate = selected_collection_album_for_rows(session, batch_id, &row_refs)
                .ok_or_else(|| {
                    "Choose one supported Spotify album match before accepting.".to_string()
                })?;
            let album_uri =
                membership_uris_for_import(true, true, Some(&candidate.matching.uri), &[])
                    .and_then(|uris| uris.into_iter().next())
                    .ok_or_else(|| "Expected a Spotify album URI for the import.".to_string())?;
            metadata_uris.extend(candidate.matching.track_uris.iter().cloned());
            ApplyMembership::Album {
                uri: album_uri,
                name: candidate.matching.name.clone(),
                artist: candidate.matching.artist.clone(),
            }
        } else {
            let (result, album_uri) = rows
                .iter()
                .filter_map(|row| session.matches.get(&row.stable_id))
                .filter_map(|result| Some((result, result.selected_uri.as_deref()?)))
                .find(|(_, uri)| uri.starts_with("spotify:album:"))
                .ok_or_else(|| {
                    "Choose a supported Spotify album match before accepting.".to_string()
                })?;
            let album_uri = membership_uris_for_import(true, true, Some(album_uri), &[])
                .and_then(|uris| uris.into_iter().next())
                .ok_or_else(|| "Expected a Spotify album URI for the import.".to_string())?;
            if let Some(candidate) = result
                .candidates
                .iter()
                .find(|candidate| candidate.uri == album_uri)
            {
                metadata_uris.extend(candidate.track_uris.iter().cloned());
            }
            ApplyMembership::Album {
                uri: album_uri,
                name: album.to_owned(),
                artist: artist.to_owned(),
            }
        }
    } else if options.import_content {
        let requested = target_by_source.values().cloned().collect::<BTreeSet<_>>();
        ApplyMembership::Tracks(
            membership_uris_for_import(
                true,
                false,
                None,
                &requested.iter().cloned().collect::<Vec<_>>(),
            )
            .unwrap_or_default(),
        )
    } else {
        ApplyMembership::None
    };

    let mut by_target = BTreeMap::<String, Vec<&SourceRow>>::new();
    for row in &rows {
        if !session.incremental_source_keys.contains_key(&row.stable_id) {
            if let Some(uri) = target_by_source.get(&row.stable_id) {
                by_target.entry(uri.clone()).or_default().push(row);
            }
        }
    }
    let historical_counts = options
        .include_historical_play_counts
        .then(|| historical_counts_for_targets(session, &by_target));
    let updates = by_target
        .iter()
        .map(|(uri, rows)| {
            let refs = rows.to_vec();
            let (earliest, latest) = resolved_timestamps(&refs).unwrap_or_default();
            HistoryUpdate {
                uri: uri.clone(),
                play_count: historical_counts
                    .as_ref()
                    .and_then(|counts| counts.get(uri).copied()),
                earliest: (earliest > 0).then_some(earliest),
                latest: options.include_historical_play_counts.then_some(latest),
            }
        })
        .collect::<Vec<_>>();
    let committed_ids = rows.iter().map(|row| row.stable_id.clone()).collect();
    let mappings = rows
        .iter()
        .filter_map(|row| {
            let target_uri = target_by_source.get(&row.stable_id)?.clone();
            let result = session.matches.get(&row.stable_id);
            Some(ApplyMapping {
                source_key: session
                    .incremental_source_keys
                    .get(&row.stable_id)
                    .cloned()
                    .unwrap_or_else(|| row.stable_id.clone()),
                artist: row.artist.clone(),
                album: row.album.clone(),
                track: row.track.clone(),
                target_uri,
                album_uri: if converted_collection
                    || (collection_shaped && !options.whole_album)
                    || (album.is_empty() && !options.whole_album)
                {
                    None
                } else if collection_shaped && options.whole_album {
                    selected_collection_album_for_rows(
                        session,
                        batch_id,
                        &rows.iter().collect::<Vec<_>>(),
                    )
                    .map(|candidate| candidate.matching.uri)
                } else if album.is_empty() && options.whole_album {
                    selected_collection_album(session, batch_id)
                        .map(|candidate| candidate.matching.uri.clone())
                        .or_else(|| {
                            result
                                .and_then(|result| result.selected_uri.as_deref())
                                .filter(|uri| uri.starts_with("spotify:album:"))
                                .map(ToOwned::to_owned)
                        })
                } else {
                    result
                        .and_then(|result| result.selected_uri.as_deref())
                        .filter(|uri| uri.starts_with("spotify:album:"))
                        .map(ToOwned::to_owned)
                },
            })
        })
        .collect();

    Ok(ApplyPlan {
        session_id: session.cache_id.clone(),
        lastfm_username: session.lastfm_username.clone(),
        spotify_account_id: spotify_account_id.to_owned(),
        batch_id,
        artist: artist.to_owned(),
        album: album.to_owned(),
        committed_ids,
        archive_batch,
        options,
        membership,
        updates,
        metadata_uris: metadata_uris.into_iter().collect(),
        mappings,
    })
}

impl Service {
    pub(super) async fn enqueue_apply_plan(
        &self,
        plan: ApplyPlan,
        bulk_index: Option<usize>,
    ) -> Result<ImportStateView, String> {
        let job_id = format!("{}:{}", plan.session_id, plan.batch_id);
        log::info!(
            target: "lastfm_import",
            "apply enqueue job={} batch={} account={}",
            job_id,
            plan.batch_id,
            plan.spotify_account_id
        );
        self.mutate_sync(|state| {
            let failed = state
                .apply_queue
                .iter()
                .find(|job| job.id == job_id && job.status == ApplyJobStatus::Failed)
                .cloned();
            if let Some(failed) = &failed {
                if failed.plan != plan {
                    return Err(
                        "Apply choices are frozen; retry the failed batch with the same choices."
                            .into(),
                    );
                }
            } else if state
                .apply_queue
                .iter()
                .any(|job| job.status == ApplyJobStatus::Failed && job.id != job_id)
            {
                return Err(
                    "Retry the failed Last.fm apply batch before queuing another batch.".into(),
                );
            }
            if state.accept_all.is_some() && bulk_index.is_none() && failed.is_none() {
                return Err("Accept All is already applying this Last.fm review.".into());
            }
            if state.apply_queue.iter().any(|job| {
                job.id == job_id
                    && matches!(job.status, ApplyJobStatus::Queued | ApplyJobStatus::Running)
            }) {
                return Err("This Last.fm review batch is already queued for application.".into());
            }
            if let Some(failed) = &failed {
                if let Some(bulk_index) = failed.bulk_index {
                    state.accept_all.get_or_insert_with(|| AcceptAllCursor {
                        session_id: failed.plan.session_id.clone(),
                        lastfm_username: failed.plan.lastfm_username.clone(),
                        spotify_account_id: failed.plan.spotify_account_id.clone(),
                        next_batch_index: bulk_index,
                    });
                }
            }
            if let Some(job) = state.apply_queue.iter_mut().find(|job| job.id == job_id) {
                let failed = failed.expect("the queued/running duplicate was rejected above");
                job.plan = failed.plan;
                job.status = ApplyJobStatus::Queued;
                job.error = None;
                job.error_code = None;
                job.retry_at = None;
                job.bulk_index = failed.bulk_index.or(bulk_index);
            } else {
                state.apply_queue.push(ReviewApplyJob {
                    id: job_id,
                    plan,
                    status: ApplyJobStatus::Queued,
                    stage: ApplyJobStage::Upstream,
                    attempt: 0,
                    error: None,
                    error_code: None,
                    retry_at: None,
                    bulk_index,
                });
            }
            Ok(())
        })
        .await?;
        Ok(self.state().await)
    }

    pub(super) async fn retry_failed_apply(
        &self,
        session_id: &str,
        batch_id: u32,
        lastfm_username: &str,
        spotify_account_id: &str,
    ) -> Result<ImportStateView, String> {
        let job_id = format!("{session_id}:{batch_id}");
        let plan = self
            .sync_snapshot()
            .await
            .apply_queue
            .into_iter()
            .find(|job| job.id == job_id && job.status == ApplyJobStatus::Failed)
            .ok_or_else(|| "This Last.fm apply batch is not failed and retryable.".to_string())?
            .plan;
        if plan.lastfm_username != lastfm_username || plan.spotify_account_id != spotify_account_id
        {
            return Err("The failed Last.fm apply batch belongs to another account.".into());
        }
        self.enqueue_apply_plan(plan, None).await
    }

    pub(super) async fn next_apply_job(&self) -> Option<ReviewApplyJob> {
        let queue = self.sync_snapshot().await.apply_queue;
        for job in queue {
            if job.status == ApplyJobStatus::Failed {
                return None;
            }
            if matches!(job.status, ApplyJobStatus::Queued | ApplyJobStatus::Running) {
                return Some(job);
            }
        }
        None
    }

    pub(super) async fn claim_apply_job(&self, id: &str) -> Result<Option<ReviewApplyJob>, String> {
        self.mutate_sync(|state| {
            let Some(job) = state.apply_queue.iter_mut().find(|job| job.id == id) else {
                return Ok(None);
            };
            if !matches!(job.status, ApplyJobStatus::Queued | ApplyJobStatus::Running) {
                return Ok(None);
            }
            job.status = ApplyJobStatus::Running;
            job.attempt = job.attempt.saturating_add(1);
            job.error = None;
            job.error_code = None;
            job.retry_at = None;
            log::info!(
                target: "lastfm_import",
                "apply claim job={} attempt={} stage={:?}",
                job.id,
                job.attempt,
                job.stage
            );
            Ok(Some(job.clone()))
        })
        .await
    }

    pub(super) async fn mark_apply_stage(
        &self,
        id: &str,
        stage: ApplyJobStage,
    ) -> Result<(), String> {
        self.mutate_sync(|state| {
            let Some(job) = state.apply_queue.iter_mut().find(|job| job.id == id) else {
                return Err("Last.fm apply job disappeared before its stage checkpoint.".into());
            };
            if job.status != ApplyJobStatus::Running {
                return Err("Last.fm apply job is no longer running.".into());
            }
            job.stage = stage;
            Ok(())
        })
        .await
    }

    #[cfg(test)]
    pub(super) async fn fail_apply_job(&self, id: &str, message: String) -> Result<(), String> {
        self.fail_apply_job_with(id, ApplyFailure::apply_failed(message))
            .await
    }

    pub(super) async fn fail_apply_job_with(
        &self,
        id: &str,
        failure: ApplyFailure,
    ) -> Result<(), String> {
        log::warn!(
            target: "lastfm_import",
            "apply failure job={} code={:?} family={:?} ambiguous={} message={}",
            id,
            failure.code,
            failure.endpoint_family,
            failure.ambiguous_outcome,
            failure.message
        );
        self.mutate_sync(|state| {
            let bulk_job = state
                .apply_queue
                .iter()
                .find(|job| job.id == id)
                .filter(|job| job.bulk_index.is_some())
                .map(|job| job.plan.session_id.clone());
            if let Some(job) = state.apply_queue.iter_mut().find(|job| job.id == id) {
                job.status = ApplyJobStatus::Failed;
                job.error = Some(failure.message.clone());
                job.error_code = Some(failure.code);
                job.retry_at = failure.retry_at;
            }
            if state
                .accept_all
                .as_ref()
                .is_some_and(|cursor| bulk_job.as_deref() == Some(cursor.session_id.as_str()))
            {
                // Park the bulk cursor with the frozen failed job. This makes the
                // failed batch visible and retryable after a restart; a successful
                // retry restores the cursor from its persisted bulk index.
                state.accept_all = None;
            }
            Ok(())
        })
        .await
    }

    pub(super) async fn remove_apply_job(
        &self,
        id: &str,
    ) -> Result<Option<ReviewApplyJob>, String> {
        self.mutate_sync(|state| {
            let index = state.apply_queue.iter().position(|job| job.id == id);
            let Some(index) = index else {
                return Ok(None);
            };
            let job = state.apply_queue.remove(index);
            if let Some(bulk_index) = job.bulk_index {
                if let Some(cursor) = state.accept_all.as_mut() {
                    if cursor.session_id == job.plan.session_id
                        && cursor.next_batch_index <= bulk_index
                    {
                        cursor.next_batch_index = bulk_index.saturating_add(1);
                    }
                }
            }
            Ok(Some(job))
        })
        .await
    }

    pub(super) fn claim_apply_runner(&self) -> Option<super::RunnerGuard> {
        super::RunnerGuard::claim(&self.apply_running)
    }
}

pub(super) async fn apply_page(
    service: &Service,
    batch_id: u32,
    (artist, album): (&str, &str),
    selected_ids: &[String],
    archive_batch: bool,
    options: PageOptions,
) -> Result<ImportStateView, String> {
    let (session, sync) = service.snapshot_with_sync().await;
    let Some(session) = session else {
        return Err("No Last.fm import session is active.".into());
    };
    let job_id = format!("{}:{batch_id}", session.cache_id);
    let effective_session = effective_apply_session_for_job(&session, &sync.apply_queue, &job_id);
    let spotify_account_id = session
        .spotify_account_id
        .clone()
        .ok_or_else(|| "Choose a supported Spotify match before accepting.".to_string())?;
    let plan = if let Some(failed) = sync
        .apply_queue
        .iter()
        .find(|job| job.id == job_id && job.status == ApplyJobStatus::Failed)
    {
        if !retry_plan_matches_request(
            &failed.plan,
            &spotify_account_id,
            batch_id,
            artist,
            album,
            selected_ids,
            archive_batch,
            &options,
        ) {
            return Err(
                "Apply choices are frozen; retry the failed batch with the same choices.".into(),
            );
        }
        failed.plan.clone()
    } else {
        build_apply_plan(
            &effective_session,
            &spotify_account_id,
            batch_id,
            artist,
            album,
            selected_ids,
            archive_batch,
            options,
        )?
    };
    let view = service.enqueue_apply_plan(plan, None).await?;
    log::info!(
        target: "lastfm_import",
        "apply enqueued batch={} user={}",
        batch_id,
        session.lastfm_username
    );
    Ok(view)
}

pub(super) async fn commit_apply_plan(service: &Service, plan: &ApplyPlan) -> Result<(), String> {
    service
        .mutate_session(|current| {
            let Some(mut session) = current else {
                return Err("No Last.fm import session is active.".into());
            };
            if session.cache_id != plan.session_id
                || session.lastfm_username != plan.lastfm_username
                || session
                    .spotify_account_id
                    .as_deref()
                    .is_some_and(|account| account != plan.spotify_account_id)
                || !review_phase_allowed(session.phase)
            {
                return Err("The Last.fm import changed before its apply job completed.".into());
            }
            session.spotify_account_id = Some(plan.spotify_account_id.clone());
            session
                .page_options
                .insert(batch_options_key(plan.batch_id), plan.options.clone());
            let committed_ids = plan.committed_ids.iter().cloned().collect::<BTreeSet<_>>();
            let deferred_ids = if plan.archive_batch {
                let batch = requested_batch(&session, plan.batch_id, &plan.artist, &plan.album)
                    .ok_or_else(|| "Unknown Last.fm import review batch.".to_string())?;
                let rows_by_id = source_row_map(&session);
                batch_rows(&batch, &rows_by_id)
                    .into_iter()
                    .filter(|row| {
                        is_actionable(&session, &row.stable_id)
                            && !committed_ids.contains(&row.stable_id)
                    })
                    .map(|row| row.stable_id.clone())
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            let default_count_mode = session.default_count_mode;
            for id in &plan.committed_ids {
                if plan.options.include_historical_play_counts
                    && plan.options.selected_track_ids.contains(id)
                {
                    if let Some(target) = session
                        .matches
                        .get(id)
                        .and_then(|result| matched_track_uri(result, id))
                    {
                        session
                            .count_modes
                            .entry(target)
                            .or_insert(default_count_mode);
                    }
                }
                session.decisions.insert(
                    id.clone(),
                    RowDecision {
                        status: RowStatus::Done,
                        excluded: false,
                    },
                );
            }
            for id in deferred_ids {
                session.decisions.insert(
                    id,
                    RowDecision {
                        status: RowStatus::Skipped,
                        excluded: false,
                    },
                );
            }
            update_review_phase(&mut session);
            Ok((Some(session), ()))
        })
        .await
}

pub(super) fn apply_frozen_mappings(mappings: &mut LastFmMappings, plan: &ApplyPlan) {
    for mapping in &plan.mappings {
        mappings
            .track_mappings
            .insert(mapping.source_key.clone(), mapping.target_uri.clone());
        if let Some(album_uri) = mapping.album_uri.as_deref() {
            let album = mappings
                .album_mappings
                .entry(source_album_key(&mapping.artist, &mapping.album))
                .or_default();
            album.spotify_album_uri = album_uri.to_owned();
            album.track_uris_by_name.insert(
                normalize_for_match(&mapping.track),
                mapping.target_uri.clone(),
            );
        }
    }
}

fn cached_collection_tracks_for_apply(
    session: &LastFmImportSessionV2,
    plan: &ApplyPlan,
    added_at: u64,
) -> Vec<retune_core::model::NewTrack> {
    let wanted = plan.metadata_uris.iter().collect::<BTreeSet<_>>();
    let category = plan
        .options
        .genre
        .as_deref()
        .map(str::trim)
        .filter(|genre| !genre.is_empty())
        .unwrap_or(retune_core::UNCATEGORIZED)
        .to_owned();
    let mut tracks = BTreeMap::new();
    for album in collection_selected_albums(session, plan.batch_id) {
        for (index, uri) in album.matching.track_uris.iter().enumerate() {
            if !wanted.contains(uri) || tracks.contains_key(uri) {
                continue;
            }
            let Some(name) = album
                .matching
                .track_names
                .get(index)
                .filter(|name| !name.is_empty())
            else {
                continue;
            };
            tracks.insert(
                uri.clone(),
                retune_core::model::NewTrack {
                    uri: uri.clone(),
                    source: retune_core::model::SourceId::Music,
                    cat: category.clone(),
                    art: album
                        .matching
                        .track_artists
                        .get(index)
                        .filter(|artist| !artist.is_empty())
                        .unwrap_or(&album.matching.artist)
                        .clone(),
                    alb: album
                        .matching
                        .track_albums
                        .get(index)
                        .filter(|name| !name.is_empty())
                        .unwrap_or(&album.matching.name)
                        .clone(),
                    name: name.clone(),
                    duration: std::time::Duration::from_secs(
                        album
                            .track_durations
                            .get(index)
                            .copied()
                            .unwrap_or_default(),
                    ),
                    track_no: album.track_numbers.get(index).copied().flatten(),
                    disc_no: None,
                    added_at: Some(added_at),
                    release_date: album.release_date.clone(),
                    kind: Some("Spotify".into()),
                    bitrate_kbps: None,
                },
            );
        }
    }
    tracks.into_values().collect()
}

pub(super) type ApplyEffectFuture = Pin<Box<dyn Future<Output = Result<(), ApplyFailure>> + Send>>;

pub(super) async fn run_apply_upstream_effect<
    T: retune_spotify::client::Transport,
    S: retune_spotify::tokens::TokenStore,
>(
    service: &Service,
    membership: &mut crate::spotify_membership::SpotifyMembershipGuard,
    library_owner: &crate::library_state::LibraryOwner,
    cooldown_store: &crate::store::FsCooldownStore,
    provider: &retune_spotify::client::SpotifyClient<T, S>,
    plan: &ApplyPlan,
    added_at: u64,
) -> Result<(), ApplyFailure> {
    match &plan.membership {
        ApplyMembership::None => {}
        ApplyMembership::Album { uri, name, artist } => {
            let saved = crate::spotify_membership::save_album_locked(
                provider,
                membership,
                library_owner,
                cooldown_store,
                uri,
                name,
                artist,
                added_at,
            )
            .await?;
            if saved.album_uri != *uri {
                return Err("Spotify returned a different album than the selected match.".into());
            }
        }
        ApplyMembership::Tracks(uris) => {
            let cached_tracks = service
                .snapshot()
                .await
                .as_ref()
                .map(|session| cached_collection_tracks_for_apply(session, plan, added_at))
                .unwrap_or_default();
            crate::spotify_membership::save_tracks_locked(
                provider,
                membership,
                library_owner,
                cooldown_store,
                uris.clone(),
                cached_tracks,
                added_at,
            )
            .await?;
        }
    }
    log::info!(target: "lastfm_import", "apply upstream complete batch={}", plan.batch_id);
    Ok(())
}

pub(super) async fn execute_apply_job<F>(
    service: &Service,
    job: &ReviewApplyJob,
    mut effect: F,
) -> Result<(), ApplyFailure>
where
    F: FnMut(ApplyJobStage, ApplyPlan) -> ApplyEffectFuture,
{
    for (stage, next_stage) in [
        (ApplyJobStage::Upstream, ApplyJobStage::Local),
        (ApplyJobStage::Local, ApplyJobStage::Mappings),
        (ApplyJobStage::Mappings, ApplyJobStage::Decision),
        (ApplyJobStage::Decision, ApplyJobStage::Decision),
    ] {
        if job.stage > stage {
            continue;
        }
        effect(stage, job.plan.clone()).await?;
        if stage == ApplyJobStage::Decision {
            service.remove_apply_job(&job.id).await?;
        } else {
            service.mark_apply_stage(&job.id, next_stage).await?;
        }
    }
    Ok(())
}

pub(super) fn apply_failure_event(batch_id: u32, failure: &ApplyFailure) -> ImportApplyFinished {
    ImportApplyFinished::Failed {
        batch_id,
        code: failure.code,
        message: failure.message.clone(),
        retry_at: failure.retry_at,
    }
}

pub(super) async fn apply_work_pending(service: &Service) -> bool {
    let sync = service.sync_snapshot().await;
    service.next_apply_job().await.is_some()
        || (sync
            .apply_queue
            .iter()
            .all(|job| job.status != ApplyJobStatus::Failed)
            && sync.accept_all.is_some())
}

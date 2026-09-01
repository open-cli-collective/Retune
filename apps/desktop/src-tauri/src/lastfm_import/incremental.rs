use std::time::Duration;

use super::*;

pub(super) async fn set_sync_problem(
    service: &Service,
    message: Option<String>,
) -> Result<(), String> {
    service
        .mutate_sync(|state| {
            state.sync_problem = message;
            Ok(())
        })
        .await
}

pub(super) async fn apply_completed_incremental_range(
    library: &crate::library_state::LibraryState,
    lastfm: &Arc<crate::lastfm::Service>,
    spotify_membership: &crate::spotify_membership::SpotifyMembership,
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
    if lastfm.state().await.username.as_deref() != Some(username) {
        return Err("The Last.fm account changed during incremental reconciliation.".into());
    }
    let cached_spotify_account = cached_spotify_account(spotify_membership);
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
    let library_transaction = library.begin_transaction()?;
    let available = library
        .lock()
        .expect("library mutex poisoned")
        .tracks()
        .iter()
        .map(|track| track.uri.clone())
        .collect::<BTreeSet<_>>();
    let spotify_account_id = sync_before.spotify_account_id.as_deref();
    let mappings = service.mappings_for(username, spotify_account_id).await?;
    let result = reconcile_incremental(&events, &receipts, &mappings, &available, 0, u64::MAX);
    let (before_library, after_library) = {
        let current = library.lock().expect("library mutex poisoned");
        let before = current.clone();
        let mut after = before.clone();
        apply_incremental_updates(&mut after, &result.increments, &result.latest);
        (before, after)
    };
    let journal = LastFmApplicationJournal {
        before_library,
        after_library: after_library.clone(),
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
    let library_transaction = if !result.increments.is_empty() {
        let (transaction, ()) = library
            .replace_in_transaction(library_transaction, after_library, ())
            .await?;
        transaction
    } else {
        library_transaction
    };
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
    if let Err(error) = service.remove_snapshot(&range.cache_id).await {
        log::warn!("Could not remove completed Last.fm incremental cache: {error}");
    }
    service
        .sync_backlog_into_review(username, sync_before.spotify_account_id.as_deref())
        .await?;
    drop(library_transaction);
    Ok(())
}

pub(super) async fn recover_pending_incremental_journal(
    library: &crate::library_state::LibraryState,
    lastfm: &Arc<crate::lastfm::Service>,
    spotify_membership: &crate::spotify_membership::SpotifyMembership,
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
    if lastfm.state().await.username.as_deref() != state_before.lastfm_username.as_deref() {
        return Err("The Last.fm account changed before journal recovery.".into());
    }
    if state_before
        .spotify_account_id
        .as_deref()
        .is_some_and(|expected| {
            cached_spotify_account(spotify_membership).as_deref() != Some(expected)
        })
    {
        return Err("The Spotify account changed before journal recovery.".into());
    }
    lastfm.settle_before_import().await;
    let _receipt_guard = lastfm.reconciliation_guard().await;
    let library_transaction = library.begin_transaction()?;
    let mut recovered = library.lock().expect("library mutex poisoned").clone();
    let outcome =
        recover_application_journal(&mut recovered, &journal).map_err(|error| error.to_string())?;
    let library_transaction = if outcome == JournalRecovery::AppliedBefore {
        let (transaction, ()) = library
            .replace_in_transaction(library_transaction, recovered, ())
            .await?;
        transaction
    } else {
        library_transaction
    };
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
        .await?;
    drop(library_transaction);
    Ok(())
}

pub(super) async fn run_incremental_sync(
    library: &crate::library_state::LibraryState,
    spotify_membership: &crate::spotify_membership::SpotifyMembership,
    lastfm: &Arc<crate::lastfm::Service>,
    service: &Arc<Service>,
    username: &str,
) -> Result<(), String> {
    let generation = lastfm.import_generation();
    let now = crate::unix_now();
    let spotify_account_id = cached_spotify_account(spotify_membership);
    let current = service.sync_snapshot().await;
    if current.lastfm_username.as_deref() == Some(username) {
        recover_pending_incremental_journal(library, lastfm, spotify_membership, service).await?;
    }
    if current.lastfm_username.as_deref() != Some(username) {
        let previous_cache_id = current
            .active
            .as_ref()
            .map(|active| active.cache_id.clone());
        if let Some(cache_id) = previous_cache_id {
            service.remove_snapshot(&cache_id).await.map_err(|error| {
                format!("Could not remove the previous Last.fm account cache: {error}")
            })?;
        }
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
                .sweep_backlog_with_mappings(library, username, spotify_account_id)
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
            let lastfm = Arc::clone(lastfm);
            let payload = fetch_incremental_page_with_retry(
                &lastfm,
                service,
                username,
                generation,
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
            let lastfm = Arc::clone(lastfm);
            let fetch_username = username.to_owned();
            let query_from = range.query_from;
            let query_to = range.query_to;
            let fetch_generation = generation;
            let retry_lastfm = Arc::clone(&lastfm);
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
                        fetch_source_page(
                            &lastfm,
                            &username,
                            fetch_generation,
                            page,
                            query_from,
                            query_to,
                        )
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
                    wait_for_incremental_window_retry(
                        retry_lastfm.as_ref(),
                        service,
                        username,
                        generation,
                        crate::lastfm::import_retry_delay(usize::MAX),
                    )
                    .await?;
                    continue;
                }
                SourceWindowOutcome::Suspended => {
                    return Err("Last.fm incremental sync was suspended.".into());
                }
            }
        }
        apply_completed_incremental_range(library, lastfm, spotify_membership, service, username)
            .await?;
        return Ok(());
    }
}

pub(super) async fn wait_for_incremental_window_retry(
    lastfm: &crate::lastfm::Service,
    service: &Service,
    username: &str,
    generation: u64,
    delay: Duration,
) -> Result<(), String> {
    set_sync_problem(
        service,
        Some("Last.fm incremental download will resume after a temporary error.".into()),
    )
    .await?;
    if let Err(error) = lastfm
        .wait_for_import_retry(username, generation, delay)
        .await
    {
        set_sync_problem(service, Some(error.message.clone())).await?;
        return Err(error.message);
    }
    Ok(())
}

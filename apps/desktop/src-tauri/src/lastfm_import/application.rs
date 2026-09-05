use std::sync::Arc;

#[cfg(test)]
use super::model::ImportQueueItem;
use super::model::{ApplyFailure, ApplyFailureCode, ApplyPlan};
use super::{
    apply::{apply_frozen_mappings, apply_page, commit_apply_plan, run_apply_upstream_effect},
    apply_history_updates, apply_metadata, current_account_binding,
    current_spotify_binding_is_current, ensure_import_readable, lastfm_username, lazy_match_page,
    prepare_accept_all_batches, run_incremental_sync, set_sync_problem, AcceptAllCursor,
    AcceptAllSummary, ApplyJobStage, CollectionAlbumCandidate, CountMode, ImportDefaults,
    ImportMatchSelection, ImportPageView, ImportPhase, ImportQueuePage, ImportStateView,
    PageOptions, ReviewAction, ReviewBatchKey, Service,
};

pub(super) struct UseCases<'a, Provider, Connected> {
    service: &'a Arc<Service>,
    lastfm: &'a Arc<crate::lastfm::Service>,
    membership: &'a crate::spotify_membership::SpotifyMembership,
    library: &'a crate::library_state::LibraryState,
    settings: &'a crate::store::SettingsState,
    cooldown_store: &'a crate::store::FsCooldownStore,
    provider: Provider,
    connected: Connected,
}

pub(super) struct Owners<'a> {
    pub(super) service: &'a Arc<Service>,
    pub(super) lastfm: &'a Arc<crate::lastfm::Service>,
    pub(super) membership: &'a crate::spotify_membership::SpotifyMembership,
    pub(super) library: &'a crate::library_state::LibraryState,
    pub(super) settings: &'a crate::store::SettingsState,
    pub(super) cooldown_store: &'a crate::store::FsCooldownStore,
}

impl<'a, Provider, Connected> UseCases<'a, Provider, Connected>
where
    Provider: Fn() -> Result<Arc<crate::SpotifyProvider>, String>,
    Connected: Fn() -> Result<bool, String>,
{
    pub(super) fn new(owners: Owners<'a>, provider: Provider, connected: Connected) -> Self {
        Self {
            service: owners.service,
            lastfm: owners.lastfm,
            membership: owners.membership,
            library: owners.library,
            settings: owners.settings,
            cooldown_store: owners.cooldown_store,
            provider,
            connected,
        }
    }

    async fn readable(&self) -> Result<bool, String> {
        self.service.ensure_hydrated()?;
        ensure_import_readable(
            self.service,
            self.lastfm,
            self.membership,
            &self.provider,
            &self.connected,
        )
        .await
    }

    pub(super) async fn state(&self, now: u64) -> Result<ImportStateView, String> {
        if self.service.has_session().await {
            let _ = self.readable().await?;
        }
        let mut view = self.service.state().await;
        if view.phase.is_none() {
            view.username = lastfm_username(self.lastfm).await.ok();
        }
        view.spotify_limit = self
            .cooldown_store
            .effective_cooldown(now)
            .map_err(|error| error.to_string())?;
        Ok(view)
    }

    pub(super) async fn queue(
        &self,
        cursor: usize,
        limit: usize,
    ) -> Result<ImportQueuePage, String> {
        if !self.readable().await? {
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
        let mut page = self.service.queue_page(cursor, limit).await?;
        let cooldown = self
            .cooldown_store
            .effective_cooldown(crate::unix_now())
            .map_err(|error| error.to_string())?;
        project_authoritative_retry_at(&mut page, cooldown);
        Ok(page)
    }

    pub(super) async fn page(
        &self,
        key: ReviewBatchKey,
    ) -> Result<(Option<ImportPageView>, bool, bool), String> {
        if !self.readable().await? {
            return Ok((None, false, false));
        }
        lazy_match_page(
            self.service,
            self.lastfm,
            self.membership,
            self.library,
            self.cooldown_store,
            &self.provider,
            &self.connected,
            key,
        )
        .await
    }

    pub(super) async fn combine_batches(
        &self,
        batch_ids: &[u32],
    ) -> Result<Option<ImportPageView>, String> {
        if !self.readable().await? {
            return Err("The Last.fm import is not available for this account.".into());
        }
        super::ensure_review_mutable(self.service).await?;
        let owner = self
            .service
            .owner_phase()
            .await
            .ok_or_else(|| "No Last.fm import session is active.".to_string())?;
        let spotify_account_id = owner
            .spotify_account_id
            .as_deref()
            .ok_or_else(|| "Connect Spotify before changing Last.fm batches.".to_string())?;
        let (batch_id, artist, album) = self
            .service
            .combine_batches(&owner.lastfm_username, spotify_account_id, batch_ids)
            .await?;
        Ok(self.service.page(batch_id, &artist, &album).await)
    }

    pub(super) async fn review(
        &self,
        key: ReviewBatchKey,
        ids: Option<&[String]>,
        action: ReviewAction,
    ) -> Result<ImportStateView, String> {
        super::review_import(
            self.service,
            self.lastfm,
            self.membership,
            self.library,
            &self.provider,
            &self.connected,
            key,
            ids,
            action,
        )
        .await
    }

    pub(super) async fn options(
        &self,
        key: ReviewBatchKey,
        options: PageOptions,
    ) -> Result<ImportStateView, String> {
        super::update_import_options(
            self.service,
            self.lastfm,
            self.membership,
            &self.provider,
            &self.connected,
            key,
            options,
        )
        .await
    }

    pub(super) async fn count_mode(
        &self,
        target_uri: &str,
        mode: CountMode,
    ) -> Result<ImportStateView, String> {
        super::update_import_count_mode(
            self.service,
            self.lastfm,
            self.membership,
            &self.provider,
            &self.connected,
            target_uri,
            mode,
        )
        .await
    }

    pub(super) async fn search_terms(&self, show: bool) -> Result<ImportStateView, String> {
        super::update_import_search_terms(
            self.service,
            self.lastfm,
            self.membership,
            &self.provider,
            &self.connected,
            show,
        )
        .await
    }

    pub(super) async fn select_matches(
        &self,
        batch_id: u32,
        selections: Vec<ImportMatchSelection>,
    ) -> Result<Option<ImportPageView>, String> {
        let selections = selections
            .into_iter()
            .map(|selection| (selection.id, selection.uri))
            .collect::<Vec<_>>();
        super::select_import_matches(
            self.service,
            self.lastfm,
            self.membership,
            &self.provider,
            &self.connected,
            batch_id,
            &selections,
        )
        .await
        .map(|(page, _)| page)
    }

    pub(super) async fn search_collection_albums(
        &self,
        batch_id: u32,
        artist: &str,
        query: &str,
    ) -> Result<(Vec<CollectionAlbumCandidate>, bool), String> {
        let (albums, source) = super::search_collection_albums_with_source(
            self.service,
            self.lastfm,
            self.membership,
            self.library,
            &self.provider,
            &self.connected,
            batch_id,
            artist,
            query,
        )
        .await?;
        super::clear_search_quota(self.cooldown_store, source)?;
        Ok((
            albums,
            source == retune_spotify::client::SearchSource::Network,
        ))
    }

    pub(super) async fn preview_or_add_collection_album(
        &self,
        batch_id: u32,
        artist: &str,
        uri: &str,
        add: bool,
    ) -> Result<Option<ImportPageView>, String> {
        super::preview_or_add_collection_album(
            self.service,
            self.lastfm,
            self.membership,
            self.library,
            &self.provider,
            &self.connected,
            batch_id,
            artist,
            uri,
            add,
        )
        .await
        .map(|(page, _)| page)
    }

    pub(super) async fn remove_collection_album(
        &self,
        batch_id: u32,
        artist: &str,
        uri: &str,
    ) -> Result<Option<ImportPageView>, String> {
        super::remove_collection_album(
            self.service,
            self.lastfm,
            self.membership,
            self.library,
            &self.provider,
            &self.connected,
            batch_id,
            artist,
            uri,
        )
        .await
        .map(|(page, _)| page)
    }

    pub(super) async fn set_collection_album_import(
        &self,
        batch_id: u32,
        artist: &str,
        uri: &str,
        enabled: bool,
    ) -> Result<Option<ImportPageView>, String> {
        super::set_collection_album_import(
            self.service,
            self.lastfm,
            self.membership,
            &self.provider,
            &self.connected,
            batch_id,
            artist,
            uri,
            enabled,
        )
        .await
        .map(|(page, _)| page)
    }

    pub(super) async fn change_track(
        &self,
        batch_id: u32,
        id: &str,
        query: &str,
    ) -> Result<(Option<ImportPageView>, bool), String> {
        let ((page, _), source) = super::change_import_track_with_source(
            self.service,
            self.lastfm,
            self.membership,
            self.library,
            &self.provider,
            &self.connected,
            batch_id,
            id,
            query,
        )
        .await?;
        super::clear_search_quota(self.cooldown_store, source)?;
        Ok((
            page,
            source == retune_spotify::client::SearchSource::Network,
        ))
    }

    pub(super) async fn change_album(
        &self,
        batch_id: u32,
        id: &str,
        query: &str,
    ) -> Result<(ImportStateView, bool), String> {
        let (view, source) = super::change_import_album_with_source(
            self.service,
            self.lastfm,
            self.membership,
            &self.provider,
            &self.connected,
            batch_id,
            id,
            query,
        )
        .await?;
        super::clear_search_quota(self.cooldown_store, source)?;
        Ok((
            view,
            source == retune_spotify::client::SearchSource::Network,
        ))
    }

    pub(super) async fn activate_collection(
        &self,
        key: ReviewBatchKey,
    ) -> Result<Option<ImportPageView>, String> {
        super::activate_collection(
            self.service,
            self.lastfm,
            self.membership,
            self.library,
            &self.provider,
            &self.connected,
            key,
        )
        .await
        .map(|(page, _)| page)
    }

    pub(super) async fn start_import<Spawn, Changed>(
        &self,
        defaults: Option<ImportDefaults>,
        spawn: Spawn,
        mut changed: Changed,
    ) -> Result<ImportStateView, String>
    where
        Spawn:
            FnOnce(super::service::RunnerGuard, Arc<crate::lastfm::Service>, Arc<Service>, String),
        Changed: FnMut(),
    {
        let username = lastfm_username(self.lastfm).await?;
        let history_to =
            crate::settings_commands::history_cutoff_for_import(self.settings, &username).await?;
        if let Some(owner) = self.service.owner_phase().await {
            if owner.phase == ImportPhase::Suspended
                && owner.requires_spotify_ownership()
                && !current_spotify_binding_is_current(
                    self.service,
                    self.lastfm,
                    self.membership,
                    &self.provider,
                    &self.connected,
                    true,
                )
                .await?
            {
                let view = self.service.state().await;
                changed();
                return Ok(view);
            }
        }
        let view = self
            .service
            .start_or_resume(&username, history_to, defaults)
            .await?;
        changed();
        if let Some(run) = self.service.claim_runner() {
            spawn(
                run,
                Arc::clone(self.lastfm),
                Arc::clone(self.service),
                username,
            );
        }
        Ok(view)
    }

    pub(super) async fn sync<StartWorker, Changed>(
        &self,
        mut start_worker: StartWorker,
        mut changed: Changed,
    ) -> Result<ImportStateView, String>
    where
        StartWorker: FnMut(),
        Changed: FnMut(),
    {
        let username = lastfm_username(self.lastfm).await?;
        if self.service.next_apply_job().await.is_some()
            || self.service.sync_snapshot().await.accept_all.is_some()
        {
            start_worker();
        }
        self.lastfm.settle_before_import().await;
        let Some(_run) = self.service.claim_sync_runner() else {
            return Ok(self.service.state().await);
        };
        changed();
        let result = run_incremental_sync(
            self.library,
            self.membership,
            self.lastfm,
            self.service,
            &username,
        )
        .await;
        if let Err(error) = &result {
            let _ = set_sync_problem(self.service, Some(error.clone())).await;
        }
        let view = self.service.state().await;
        changed();
        result.map(|()| view)
    }

    pub(super) async fn apply<StartWorker, Changed>(
        &self,
        key: ReviewBatchKey,
        selected_ids: &[String],
        archive_batch: bool,
        options: PageOptions,
        mut start_worker: StartWorker,
        mut changed: Changed,
    ) -> Result<ImportStateView, String>
    where
        StartWorker: FnMut(),
        Changed: FnMut(),
    {
        let membership = self.membership.lock().await;
        current_account_binding(
            self.service,
            self.lastfm,
            &membership,
            &self.provider,
            &self.connected,
            false,
            true,
            false,
        )
        .await?;
        drop(membership);
        let view = apply_page(
            self.service,
            key.batch_id,
            (&key.artist, &key.album),
            selected_ids,
            archive_batch,
            options,
        )
        .await?;
        start_worker();
        changed();
        Ok(view)
    }

    pub(super) async fn retry_apply<StartWorker, Changed>(
        &self,
        batch_id: u32,
        mut start_worker: StartWorker,
        mut changed: Changed,
    ) -> Result<ImportStateView, String>
    where
        StartWorker: FnMut(),
        Changed: FnMut(),
    {
        let membership = self.membership.lock().await;
        let (binding, _) = current_account_binding(
            self.service,
            self.lastfm,
            &membership,
            &self.provider,
            &self.connected,
            false,
            true,
            false,
        )
        .await?;
        drop(membership);
        let session_id = self
            .service
            .snapshot()
            .await
            .ok_or_else(|| "No Last.fm import session is active.".to_string())?
            .cache_id;
        let view = self
            .service
            .retry_failed_apply(
                &session_id,
                batch_id,
                &binding.lastfm_username,
                &binding.spotify_account_id,
            )
            .await?;
        start_worker();
        changed();
        Ok(view)
    }

    pub(super) async fn prepare_accept_all(
        &self,
    ) -> Result<(AcceptAllSummary, bool, bool), String> {
        if !self.readable().await? {
            return Ok((
                AcceptAllSummary {
                    album_entities: 0,
                    track_entities: 0,
                },
                false,
                false,
            ));
        }
        let changed_any = std::sync::atomic::AtomicBool::new(false);
        let network_search_any = std::sync::atomic::AtomicBool::new(false);
        let summary = prepare_accept_all_batches(self.service, |batch_id, artist, album| {
            let changed_any = &changed_any;
            let network_search_any = &network_search_any;
            async move {
                let (_, changed, network_search) = self
                    .page(ReviewBatchKey {
                        batch_id,
                        artist,
                        album,
                    })
                    .await?;
                changed_any.fetch_or(changed, std::sync::atomic::Ordering::Relaxed);
                network_search_any.fetch_or(network_search, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
        })
        .await?;
        Ok((
            summary,
            changed_any.load(std::sync::atomic::Ordering::Relaxed),
            network_search_any.load(std::sync::atomic::Ordering::Relaxed),
        ))
    }

    pub(super) async fn accept_all<StartWorker, Changed>(
        &self,
        mut start_worker: StartWorker,
        mut changed: Changed,
    ) -> Result<ImportStateView, String>
    where
        StartWorker: FnMut(),
        Changed: FnMut(),
    {
        let owner = self
            .service
            .owner_phase()
            .await
            .ok_or_else(|| "No Last.fm import session is active.".to_string())?;
        let username = owner.lastfm_username.clone();
        let spotify_account_id = owner
            .spotify_account_id
            .clone()
            .ok_or_else(|| "Prepare Spotify matches before accepting all imports.".to_string())?;
        if self.service.sync_snapshot().await.accept_all.is_some() {
            return Err("Accept All is already applying this Last.fm review.".into());
        }
        self.service
            .mutate_sync(|sync| {
                sync.accept_all = Some(AcceptAllCursor {
                    session_id: owner.cache_id.clone(),
                    lastfm_username: username.clone(),
                    spotify_account_id: spotify_account_id.clone(),
                    next_batch_index: 0,
                });
                Ok(())
            })
            .await?;
        start_worker();
        let view = self.service.state().await;
        changed();
        Ok(view)
    }

    pub(super) async fn validate_apply_account(
        &self,
        membership: &crate::spotify_membership::SpotifyMembershipGuard,
        plan: &ApplyPlan,
        require_provider: bool,
        changed_message: &str,
    ) -> Result<Option<Arc<crate::SpotifyProvider>>, ApplyFailure> {
        let (binding, provider) = current_account_binding(
            self.service,
            self.lastfm,
            membership,
            &self.provider,
            &self.connected,
            require_provider,
            true,
            false,
        )
        .await?;
        if binding.lastfm_username != plan.lastfm_username
            || binding.spotify_account_id != plan.spotify_account_id
        {
            return Err(changed_message.into());
        }
        Ok(provider)
    }

    pub(super) async fn run_apply_effect<LibraryChanged>(
        &self,
        stage: ApplyJobStage,
        plan: &ApplyPlan,
        mut library_changed: LibraryChanged,
    ) -> Result<(), ApplyFailure>
    where
        LibraryChanged: FnMut() -> Result<(), String>,
    {
        let mut membership = self.membership.lock().await;
        match stage {
            ApplyJobStage::Upstream => {
                let provider = self
                    .validate_apply_account(
                        &membership,
                        plan,
                        true,
                        "The connected account changed before Spotify membership was applied.",
                    )
                    .await?
                    .expect("upstream apply resolves a provider");
                let library_owner = self.library.owner();
                run_apply_upstream_effect(
                    self.service,
                    &mut membership,
                    &library_owner,
                    self.cooldown_store,
                    provider.as_ref(),
                    plan,
                    crate::unix_now(),
                )
                .await?;
            }
            ApplyJobStage::Local => {
                self.validate_apply_account(
                    &membership,
                    plan,
                    false,
                    "The connected account changed while applying this review batch.",
                )
                .await?;
                if !plan.updates.is_empty() || !plan.metadata_uris.is_empty() {
                    let updates = plan.updates.clone();
                    let metadata_uris = plan.metadata_uris.clone();
                    let options = plan.options.clone();
                    self.library
                        .mutate_async(move |library| {
                            apply_history_updates(library, &updates);
                            apply_metadata(
                                library,
                                &metadata_uris,
                                options.whole_album,
                                options.genre.as_deref(),
                                options.rating,
                            )
                        })
                        .await?;
                    library_changed()?;
                }
                log::info!(target: "lastfm_import", "apply local complete batch={}", plan.batch_id);
            }
            ApplyJobStage::Mappings => {
                self.validate_apply_account(
                    &membership,
                    plan,
                    false,
                    "The connected account changed while applying this review batch.",
                )
                .await?;
                let mappings = self
                    .service
                    .mappings_for(&plan.lastfm_username, Some(&plan.spotify_account_id))
                    .await?;
                let before = mappings.clone();
                let mut mappings = mappings;
                apply_frozen_mappings(&mut mappings, plan);
                if mappings != before {
                    self.service
                        .save_mappings_for(
                            &plan.lastfm_username,
                            Some(&plan.spotify_account_id),
                            mappings,
                        )
                        .await?;
                }
                log::info!(target: "lastfm_import", "apply mappings complete batch={}", plan.batch_id);
            }
            ApplyJobStage::Decision => {
                self.validate_apply_account(
                    &membership,
                    plan,
                    false,
                    "The connected account changed while applying this review batch.",
                )
                .await?;
                commit_apply_plan(self.service, plan).await?;
                log::info!(target: "lastfm_import", "apply decision complete batch={}", plan.batch_id);
            }
        }
        Ok(())
    }
}

fn project_authoritative_retry_at(
    page: &mut ImportQueuePage,
    cooldown: Option<crate::store::Cooldown>,
) {
    let retry_at = cooldown.map(|cooldown| cooldown.deadline);
    for item in &mut page.items {
        if matches!(
            item.error_code,
            Some(ApplyFailureCode::SpotifyRateLimited | ApplyFailureCode::SpotifyQuotaExhausted)
        ) {
            item.retry_at = retry_at;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue_item(code: ApplyFailureCode, retry_at: Option<u64>) -> ImportQueueItem {
        ImportQueueItem {
            page: 1,
            artist: "Artist".into(),
            album: "Album".into(),
            custom_batch: false,
            collection_shaped: false,
            album_label_count: 0,
            play_count: 0,
            imported_play_count: 0,
            remaining_play_count: 0,
            latest: 0,
            source_count: 1,
            remaining: true,
            album_entities: 0,
            track_entities: 0,
            status: Some(super::super::model::QueueStatus::Failed),
            error: Some("Spotify rate limited".into()),
            error_code: Some(code),
            retry_at,
        }
    }

    #[test]
    fn queue_retry_at_projects_the_authoritative_effective_deadline() {
        let mut page = ImportQueuePage {
            items: vec![
                queue_item(ApplyFailureCode::SpotifyRateLimited, Some(999)),
                queue_item(ApplyFailureCode::SpotifyQuotaExhausted, Some(888)),
                queue_item(ApplyFailureCode::ApplyFailed, Some(777)),
            ],
            cursor: 0,
            next_cursor: None,
            total: 3,
        };
        project_authoritative_retry_at(
            &mut page,
            Some(crate::store::Cooldown {
                kind: crate::store::CooldownKind::Quota,
                deadline: 1_234,
            }),
        );
        assert_eq!(page.items[0].retry_at, Some(1_234));
        assert_eq!(page.items[1].retry_at, Some(1_234));
        assert_eq!(page.items[2].retry_at, Some(777));

        project_authoritative_retry_at(&mut page, None);
        assert_eq!(page.items[0].retry_at, None);
        assert_eq!(page.items[1].retry_at, None);
    }
}

use super::*;

async fn load_importer_stores<T: Send + 'static>(
    load: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(load)
        .await
        .map_err(|error| error.to_string())
}

pub(crate) struct MappingsRestore {
    current: tokio::sync::OwnedMutexGuard<PersistedLastFmMappings>,
    _persistence_gate: tokio::sync::OwnedMutexGuard<()>,
    store: MappingsStore,
}

impl MappingsRestore {
    pub(crate) fn snapshot(&self) -> PersistedLastFmMappings {
        self.current.clone()
    }

    pub(crate) fn replace(&mut self, next: PersistedLastFmMappings) -> Result<(), String> {
        self.store.save(&next)?;
        *self.current = next;
        Ok(())
    }

    pub(crate) fn install_recovered(&mut self, next: PersistedLastFmMappings) {
        *self.current = next;
    }
}

#[cfg(test)]
pub(super) fn requires_spotify_ownership(session: &LastFmImportSessionV2) -> bool {
    session.spotify_account_id.is_some()
        && matches!(
            session.phase,
            ImportPhase::Review | ImportPhase::Done | ImportPhase::Suspended
        )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ImportOwnerPhase {
    pub(super) cache_id: String,
    pub(super) lastfm_username: String,
    pub(super) spotify_account_id: Option<String>,
    pub(super) phase: ImportPhase,
}

impl ImportOwnerPhase {
    pub(super) fn requires_spotify_ownership(&self) -> bool {
        self.spotify_account_id.is_some()
            && matches!(
                self.phase,
                ImportPhase::Review | ImportPhase::Done | ImportPhase::Suspended
            )
    }
}

pub(crate) struct Service {
    pub(super) store: ImportSessionStore,
    pub(super) incremental_store: IncrementalStore,
    pub(super) mappings_store: MappingsStore,
    pub(super) review_transaction_store: ReviewTransactionStore,
    pub(super) session: Arc<Mutex<Option<LastFmImportSessionV2>>>,
    pub(super) sync_state: Arc<Mutex<LastFmSyncState>>,
    sync_mutation_gate: Arc<Mutex<()>>,
    pub(super) mappings: Arc<Mutex<PersistedLastFmMappings>>,
    persistence_gate: Arc<Mutex<()>>,
    session_writes: Arc<SessionWriteQueue>,
    #[allow(dead_code)]
    review_writes: Arc<ReviewWriteQueue>,
    pub(super) reconciliation_lock: Mutex<()>,
    pub(super) lazy_match_lock: Mutex<()>,
    pub(super) running: Arc<AtomicBool>,
    pub(super) apply_running: Arc<AtomicBool>,
    pub(super) sync_running: Arc<AtomicBool>,
    pub(super) restore_mutations: Arc<crate::restore_latch::RestoreMutationState>,
    hydration: std::sync::atomic::AtomicU8,
}

pub(super) struct RunnerGuard(Arc<AtomicBool>);

// ponytail: one coalescing writer is enough for local importer metadata.
struct SessionWriteQueue {
    pending: Arc<Mutex<Option<LastFmImportSessionV2>>>,
    running: Arc<AtomicBool>,
}

#[allow(dead_code)]
struct ReviewWriteQueue {
    pending: Arc<Mutex<Option<(LastFmImportSessionV2, PersistedLastFmMappings)>>>,
    running: Arc<AtomicBool>,
}

impl ReviewWriteQueue {
    fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(None)),
            running: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl SessionWriteQueue {
    fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(None)),
            running: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl RunnerGuard {
    pub(super) fn claim(running: &Arc<AtomicBool>) -> Option<Self> {
        (!running.swap(true, Ordering::AcqRel)).then(|| Self(Arc::clone(running)))
    }
}

impl Drop for RunnerGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Service {
    pub(super) async fn remove_snapshot(&self, cache_id: &str) -> std::io::Result<()> {
        let store = self.store.clone();
        let cache_id = cache_id.to_owned();
        tauri::async_runtime::spawn_blocking(move || store.remove_snapshot(&cache_id))
            .await
            .map_err(|error| std::io::Error::other(error.to_string()))?
    }

    #[cfg(test)]
    pub(crate) fn new(app_data_dir: impl AsRef<Path>) -> Arc<Self> {
        Self::new_with_restore_state(
            app_data_dir,
            Arc::new(crate::restore_latch::RestoreMutationState::default()),
        )
    }

    #[cfg(test)]
    pub(crate) fn new_with_restore_state(
        app_data_dir: impl AsRef<Path>,
        restore_mutations: Arc<crate::restore_latch::RestoreMutationState>,
    ) -> Arc<Self> {
        let app_data_dir = app_data_dir.as_ref().to_path_buf();
        let store = ImportSessionStore::new(&app_data_dir);
        let incremental_store = IncrementalStore::new(&app_data_dir);
        let mappings_store = MappingsStore::new(&app_data_dir);
        let review_transaction_store = ReviewTransactionStore::new(&app_data_dir);
        review_transaction_store
            .recover(&store, &incremental_store, &mappings_store)
            .map(drop)
            .expect("Last.fm review transaction recovery failed");
        let mut load_problems = Vec::new();
        let mut session = match store.load() {
            Ok(mut session) => {
                if let Some(session) = session.as_mut() {
                    refresh_cached_album_matches(session);
                }
                session
            }
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
        if let Some(session) = session.as_mut() {
            if upgrade_legacy_pending_batches(session, &sync_state.apply_queue) {
                if let Err(error) = store.save(session) {
                    log::warn!("Could not persist upgraded Last.fm review batches: {error}");
                }
            }
        }
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
            review_transaction_store,
            session: Arc::new(Mutex::new(session)),
            sync_state: Arc::new(Mutex::new(sync_state)),
            sync_mutation_gate: Arc::new(Mutex::new(())),
            mappings: Arc::new(Mutex::new(mappings)),
            persistence_gate: Arc::new(Mutex::new(())),
            session_writes: Arc::new(SessionWriteQueue::new()),
            review_writes: Arc::new(ReviewWriteQueue::new()),
            reconciliation_lock: Mutex::new(()),
            lazy_match_lock: Mutex::new(()),
            running: Arc::new(AtomicBool::new(false)),
            apply_running: Arc::new(AtomicBool::new(false)),
            sync_running: Arc::new(AtomicBool::new(false)),
            restore_mutations,
            hydration: std::sync::atomic::AtomicU8::new(1),
        })
    }

    pub(crate) fn new_unhydrated_with_restore_state(
        app_data_dir: impl AsRef<Path>,
        restore_mutations: Arc<crate::restore_latch::RestoreMutationState>,
    ) -> Arc<Self> {
        let app_data_dir = app_data_dir.as_ref();
        Arc::new(Self {
            store: ImportSessionStore::new(app_data_dir),
            incremental_store: IncrementalStore::new(app_data_dir),
            mappings_store: MappingsStore::new(app_data_dir),
            review_transaction_store: ReviewTransactionStore::new(app_data_dir),
            session: Arc::new(Mutex::new(None)),
            sync_state: Arc::new(Mutex::new(LastFmSyncState {
                version: LASTFM_SYNC_VERSION,
                sync_problem: Some("Retune is still loading Last.fm import state.".into()),
                ..LastFmSyncState::default()
            })),
            sync_mutation_gate: Arc::new(Mutex::new(())),
            mappings: Arc::new(Mutex::new(PersistedLastFmMappings {
                version: LASTFM_MAPPINGS_VERSION,
                ..PersistedLastFmMappings::default()
            })),
            persistence_gate: Arc::new(Mutex::new(())),
            session_writes: Arc::new(SessionWriteQueue::new()),
            review_writes: Arc::new(ReviewWriteQueue::new()),
            reconciliation_lock: Mutex::new(()),
            lazy_match_lock: Mutex::new(()),
            running: Arc::new(AtomicBool::new(false)),
            apply_running: Arc::new(AtomicBool::new(false)),
            sync_running: Arc::new(AtomicBool::new(false)),
            restore_mutations,
            hydration: std::sync::atomic::AtomicU8::new(0),
        })
    }

    pub(super) fn ensure_hydrated(&self) -> Result<(), String> {
        (self.hydration.load(Ordering::Acquire) == 1)
            .then_some(())
            .ok_or_else(|| "Retune is still loading Last.fm import state.".to_string())
    }

    pub(crate) async fn hydrate(&self) -> Result<(), String> {
        let store = self.store.clone();
        let incremental_store = self.incremental_store.clone();
        let mappings_store = self.mappings_store.clone();
        let review_transaction_store = self.review_transaction_store.clone();
        let (session, sync_state, mappings) = load_importer_stores(move || {
            review_transaction_store
                .recover(&store, &incremental_store, &mappings_store)
                .map(drop)?;
            let mut load_problems = Vec::new();
            let mut session = match store.load() {
                Ok(mut session) => {
                    if let Some(session) = session.as_mut() {
                        refresh_cached_album_matches(session);
                    }
                    session
                }
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
            if let Some(session) = session.as_mut() {
                if upgrade_legacy_pending_batches(session, &sync_state.apply_queue) {
                    if let Err(error) = store.save(session) {
                        log::warn!("Could not persist upgraded Last.fm review batches: {error}");
                    }
                }
            }
            if !load_problems.is_empty() {
                let problem = load_problems.join(" ");
                sync_state.sync_problem = Some(match sync_state.sync_problem.take() {
                    Some(existing) => format!("{existing} {problem}"),
                    None => problem,
                });
            }
            Ok::<_, String>((session, sync_state, mappings))
        })
        .await??;
        *self.session.lock().await = session;
        *self.sync_state.lock().await = sync_state;
        *self.mappings.lock().await = mappings;
        self.hydration.store(1, Ordering::Release);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn restore_mutations(&self) -> Arc<crate::restore_latch::RestoreMutationState> {
        Arc::clone(&self.restore_mutations)
    }

    pub(crate) async fn state(&self) -> ImportStateView {
        if self.hydration.load(Ordering::Acquire) != 1 {
            let mut view = state_view(None);
            view.sync_problem = Some("Retune is still loading Last.fm import state.".into());
            return view;
        }
        let session = self.session.lock().await;
        let mut view = match session.as_ref() {
            Some(session) if session.phase == ImportPhase::Suspended => suspended_state_view(),
            Some(session) => state_view(Some(session)),
            None => state_view(None),
        };
        let sync = self.sync_state.lock().await;
        if let Some(session) = session.as_ref() {
            if matches!(session.phase, ImportPhase::Review | ImportPhase::Done) {
                view.remaining = remaining_with_apply_queue(session, &sync.apply_queue);
            }
        }
        view.syncing = self.sync_running.load(Ordering::Acquire);
        view.last_synced_at = sync.last_synced_at;
        view.pending_review = sync.backlog.len();
        view.sync_problem = sync.sync_problem.clone();
        view.applying_all = sync.accept_all.is_some();
        view
    }

    pub(super) async fn snapshot(&self) -> Option<LastFmImportSessionV2> {
        self.session.lock().await.clone()
    }

    pub(super) async fn owner_phase(&self) -> Option<ImportOwnerPhase> {
        self.session
            .lock()
            .await
            .as_ref()
            .map(|session| ImportOwnerPhase {
                cache_id: session.cache_id.clone(),
                lastfm_username: session.lastfm_username.clone(),
                spotify_account_id: session.spotify_account_id.clone(),
                phase: session.phase,
            })
    }

    pub(super) async fn has_session(&self) -> bool {
        self.session.lock().await.is_some()
    }

    pub(super) async fn snapshot_with_sync(
        &self,
    ) -> (Option<LastFmImportSessionV2>, LastFmSyncState) {
        let session_guard = self.session.lock().await;
        let sync = self.sync_state.lock().await.clone();
        (session_guard.clone(), sync)
    }

    pub(super) async fn sync_snapshot(&self) -> LastFmSyncState {
        self.sync_state.lock().await.clone()
    }

    pub(super) async fn mutate_sync<R, F>(&self, mutation: F) -> Result<R, String>
    where
        F: FnOnce(&mut LastFmSyncState) -> Result<R, String>,
        R: Send + 'static,
    {
        self.ensure_hydrated()?;
        let persistence_gate = Arc::clone(&self.persistence_gate).lock_owned().await;
        let persistence_gate = self
            .recover_pending_review_transaction(persistence_gate)
            .await?;
        let mutation_gate = Arc::clone(&self.sync_mutation_gate).lock_owned().await;
        let mut next = self.sync_state.lock().await.clone();
        let result = mutation(&mut next)?;
        next.version = LASTFM_SYNC_VERSION;
        let store = self.incremental_store.clone();
        let current = Arc::clone(&self.sync_state);
        tauri::async_runtime::spawn(async move {
            let saved = next.clone();
            tauri::async_runtime::spawn_blocking(move || store.save(&saved))
                .await
                .map_err(|_| "Last.fm incremental sync persistence task stopped.".to_string())??;
            *current.lock().await = next;
            drop(mutation_gate);
            drop(persistence_gate);
            Ok::<_, String>(result)
        })
        .await
        .map_err(|error| error.to_string())?
    }

    pub(super) async fn mappings_for(
        &self,
        lastfm_username: &str,
        spotify_account_id: Option<&str>,
    ) -> Result<LastFmMappings, String> {
        self.ensure_hydrated()?;
        let persistence_gate = Arc::clone(&self.persistence_gate).lock_owned().await;
        let persistence_gate = self
            .recover_pending_review_transaction(persistence_gate)
            .await?;
        let mappings = self.mappings.lock().await.clone();
        if mappings.lastfm_username.as_deref() != Some(lastfm_username)
            || mappings.spotify_account_id.as_deref() != spotify_account_id
        {
            return Ok(LastFmMappings::default());
        }
        if mappings.dormant {
            self.restore_mutations.ensure_allowed()?;
            let mut active = mappings.clone();
            active.dormant = false;
            if let Err(error) = self
                .commit_mappings(persistence_gate, active.clone(), ())
                .await
            {
                log::warn!("Last.fm mappings remain dormant: {error}");
                return Ok(LastFmMappings::default());
            }
            return Ok(active.mappings);
        }
        Ok(mappings.mappings.clone())
    }

    async fn commit_mappings<R: Send + 'static>(
        &self,
        persistence_gate: tokio::sync::OwnedMutexGuard<()>,
        next: PersistedLastFmMappings,
        result: R,
    ) -> Result<R, String> {
        let store = self.mappings_store.clone();
        let current = Arc::clone(&self.mappings);
        let persisted = next.clone();
        tauri::async_runtime::spawn(async move {
            tauri::async_runtime::spawn_blocking(move || store.save(&persisted))
                .await
                .map_err(|_| "Last.fm mappings persistence task stopped.".to_string())??;
            *current.lock().await = next;
            drop(persistence_gate);
            Ok::<_, String>(result)
        })
        .await
        .map_err(|error| error.to_string())?
    }

    async fn recover_pending_review_transaction(
        &self,
        persistence_gate: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, String> {
        let sync_gate = Arc::clone(&self.sync_mutation_gate);
        let transactions = self.review_transaction_store.clone();
        let sessions = self.store.clone();
        let sync = self.incremental_store.clone();
        let mappings = self.mappings_store.clone();
        let current_session = Arc::clone(&self.session);
        let current_sync = Arc::clone(&self.sync_state);
        let current_mappings = Arc::clone(&self.mappings);
        tauri::async_runtime::spawn(async move {
            let _sync_gate = sync_gate.lock_owned().await;
            let recovered = tauri::async_runtime::spawn_blocking(move || {
                transactions.recover(&sessions, &sync, &mappings)
            })
            .await
            .map_err(|_| "Last.fm review transaction recovery task stopped.".to_string())??;
            if let Some(recovered) = recovered {
                let mut session = current_session.lock().await;
                let mut sync = current_sync.lock().await;
                let mut mappings = current_mappings.lock().await;
                *session = recovered.session;
                if let Some(sync_state) = recovered.sync_state {
                    *sync = sync_state;
                }
                *mappings = recovered.mappings;
            }
            Ok::<_, String>(persistence_gate)
        })
        .await
        .map_err(|error| error.to_string())?
    }

    async fn mutate_review_state<R, F>(&self, mutation: F) -> Result<R, String>
    where
        F: FnOnce(
            Option<LastFmImportSessionV2>,
            PersistedLastFmMappings,
        ) -> Result<(LastFmImportSessionV2, PersistedLastFmMappings, R), String>,
        R: Send + 'static,
    {
        self.ensure_hydrated()?;
        let persistence_gate = Arc::clone(&self.persistence_gate).lock_owned().await;
        let persistence_gate = self
            .recover_pending_review_transaction(persistence_gate)
            .await?;
        let previous_session = self.session.lock().await.clone();
        let previous_mappings = self.mappings.lock().await.clone();
        let (next_session, next_mappings, result) =
            mutation(previous_session.clone(), previous_mappings.clone())?;
        if previous_session.as_ref() == Some(&next_session) && previous_mappings == next_mappings {
            return Ok(result);
        }
        self.restore_mutations.ensure_allowed()?;
        let transaction = ReviewTransaction::new(next_session.clone(), next_mappings.clone());
        let transactions = self.review_transaction_store.clone();
        let sessions = self.store.clone();
        let mappings = self.mappings_store.clone();
        let current_session = Arc::clone(&self.session);
        let current_mappings = Arc::clone(&self.mappings);
        tauri::async_runtime::spawn(async move {
            tauri::async_runtime::spawn_blocking(move || {
                transactions.save(&transaction)?;
                sessions.save(transaction.session.as_ref().expect("transaction session"))?;
                mappings.save(&transaction.mappings)?;
                transactions.clear()
            })
            .await
            .map_err(|_| "Last.fm review transaction persistence task stopped.".to_string())??;
            *current_session.lock().await = Some(next_session);
            *current_mappings.lock().await = next_mappings;
            drop(persistence_gate);
            Ok::<_, String>(result)
        })
        .await
        .map_err(|error| error.to_string())?
    }

    #[allow(dead_code)]
    async fn mutate_review_state_queued<R, F>(&self, mutation: F) -> Result<R, String>
    where
        F: FnOnce(
            Option<LastFmImportSessionV2>,
            PersistedLastFmMappings,
        ) -> Result<(LastFmImportSessionV2, PersistedLastFmMappings, R), String>,
        R: Send + 'static,
    {
        self.ensure_hydrated()?;
        let persistence_gate = Arc::clone(&self.persistence_gate).lock_owned().await;
        let persistence_gate = self
            .recover_pending_review_transaction(persistence_gate)
            .await?;
        let previous_session = self.session.lock().await.clone();
        let previous_mappings = self.mappings.lock().await.clone();
        let (next_session, next_mappings, result) =
            mutation(previous_session.clone(), previous_mappings.clone())?;
        if previous_session.as_ref() == Some(&next_session) && previous_mappings == next_mappings {
            drop(persistence_gate);
            return Ok(result);
        }
        self.restore_mutations.ensure_allowed()?;
        *self.session.lock().await = Some(next_session.clone());
        *self.mappings.lock().await = next_mappings.clone();
        let queue = Arc::clone(&self.review_writes);
        *queue.pending.lock().await = Some((next_session, next_mappings));
        if !queue.running.swap(true, Ordering::AcqRel) {
            let transactions = self.review_transaction_store.clone();
            let sessions = self.store.clone();
            let mappings = self.mappings_store.clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    let (session, persisted) = {
                        let mut pending = queue.pending.lock().await;
                        match pending.take() {
                            Some(next) => next,
                            None => {
                                queue.running.store(false, Ordering::Release);
                                return;
                            }
                        }
                    };
                    let transaction = ReviewTransaction::new(session, persisted);
                    let transactions = transactions.clone();
                    let sessions = sessions.clone();
                    let mappings = mappings.clone();
                    let result = tauri::async_runtime::spawn_blocking(move || {
                        transactions.save(&transaction)?;
                        if let Some(session) = transaction.session.as_ref() {
                            sessions.save(session)?;
                        }
                        mappings.save(&transaction.mappings)?;
                        transactions.clear()
                    })
                    .await;
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => log::warn!(
                            target: "lastfm_import",
                            "queued review save failed: {error}"
                        ),
                        Err(error) => log::warn!(
                            target: "lastfm_import",
                            "queued review save task stopped: {error}"
                        ),
                    }
                }
            });
        }
        drop(persistence_gate);
        Ok(result)
    }

    pub(super) async fn save_mappings_for(
        &self,
        lastfm_username: &str,
        spotify_account_id: Option<&str>,
        mappings: LastFmMappings,
    ) -> Result<(), String> {
        self.ensure_hydrated()?;
        let persistence_gate = Arc::clone(&self.persistence_gate).lock_owned().await;
        let persistence_gate = self
            .recover_pending_review_transaction(persistence_gate)
            .await?;
        let current = self.mappings.lock().await.clone();
        self.restore_mutations.ensure_allowed()?;
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
        self.commit_mappings(persistence_gate, next, ()).await
    }

    pub(crate) async fn export_mappings(&self) -> PersistedLastFmMappings {
        self.mappings.lock().await.clone()
    }

    pub(crate) async fn begin_mappings_restore(&self) -> Result<MappingsRestore, String> {
        self.ensure_hydrated()?;
        let persistence_gate = Arc::clone(&self.persistence_gate).lock_owned().await;
        let persistence_gate = self
            .recover_pending_review_transaction(persistence_gate)
            .await?;
        let current = Arc::clone(&self.mappings).lock_owned().await;
        self.restore_mutations.ensure_allowed()?;
        Ok(MappingsRestore {
            current,
            _persistence_gate: persistence_gate,
            store: self.mappings_store.clone(),
        })
    }

    pub(crate) async fn backfill_completed_mappings(&self) -> Result<(), String> {
        self.ensure_hydrated()?;
        let Some(session) = self.snapshot().await else {
            return Ok(());
        };
        if !review_phase_allowed(session.phase) || session.spotify_account_id.is_none() {
            return Ok(());
        }
        let username = session.lastfm_username.clone();
        let spotify_account_id = session.spotify_account_id.clone();
        let selected_ids = session
            .page_options
            .values()
            .flat_map(|options| options.selected_track_ids.iter())
            .collect::<BTreeSet<_>>();
        let mut mappings = self
            .mappings_for(&username, spotify_account_id.as_deref())
            .await?;
        let before = mappings.clone();
        let source_batches = source_batch_map(&session);
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
            if !selected_ids.contains(&row.stable_id) {
                continue;
            }
            let Some(result) = session.matches.get(&row.stable_id) else {
                continue;
            };
            let batch_id = source_batches.get(&row.stable_id).copied();
            let collection_shaped = batch_id
                .is_some_and(|batch_id| batch_is_collection_shaped_for_id(&session, batch_id));
            let Some(track_uri) = matched_track_uri_for_row(result, row, collection_shaped) else {
                continue;
            };
            mappings
                .track_mappings
                .insert(source_key, track_uri.clone());
            if !collection_shaped {
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
        }
        if mappings != before {
            self.save_mappings_for(&username, spotify_account_id.as_deref(), mappings)
                .await?;
        }
        Ok(())
    }

    pub(super) async fn sync_backlog_into_review(
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
                    return Ok((Some(session), ()));
                }
                Some(session) if !review_phase_allowed(session.phase) => {
                    return Ok((Some(session), ()));
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
            let previous_batches = session.batches.clone();
            let mut custom_batches = previous_batches
                .iter()
                .filter(|batch| batch.custom)
                .cloned()
                .collect::<Vec<_>>();
            for batch in &mut custom_batches {
                batch
                    .source_ids
                    .retain(|source_id| row_ids.contains(source_id.as_str()));
            }
            custom_batches.retain(|batch| !batch.source_ids.is_empty());
            let custom_source_ids = custom_batches
                .iter()
                .flat_map(|batch| batch.source_ids.iter().cloned())
                .collect::<BTreeSet<_>>();
            let regular_rows = session
                .rows
                .iter()
                .filter(|row| !custom_source_ids.contains(&row.stable_id))
                .cloned()
                .collect::<Vec<_>>();
            let reserved_pages = custom_batches
                .iter()
                .map(|batch| batch.page)
                .collect::<BTreeSet<_>>();
            let mut next_page = 1;
            let mut next_batches = build_review_batches(&regular_rows);
            for batch in &mut next_batches {
                while reserved_pages.contains(&next_page) {
                    next_page += 1;
                }
                batch.page = next_page;
                next_page += 1;
            }
            next_batches.extend(custom_batches);
            next_batches.sort_by_key(|batch| batch.page);
            session.page_options.retain(|key, _| {
                let Some(batch_id) = key
                    .strip_prefix("batch:")
                    .and_then(|value| value.parse::<u32>().ok())
                else {
                    return true;
                };
                let previous = previous_batches.iter().find(|batch| batch.page == batch_id);
                let next = next_batches.iter().find(|batch| batch.page == batch_id);
                previous
                    .zip(next)
                    .is_some_and(|(previous, next)| previous.source_ids == next.source_ids)
            });
            session.collection_album_matches.retain(|batch_id, _| {
                let Some(previous) = previous_batches
                    .iter()
                    .find(|batch| batch.page == *batch_id)
                else {
                    return false;
                };
                next_batches
                    .iter()
                    .find(|batch| batch.page == *batch_id)
                    .is_some_and(|next| next.source_ids == previous.source_ids)
            });
            session.batches = next_batches;
            session.phase = if session.rows.is_empty() {
                ImportPhase::Done
            } else {
                ImportPhase::Review
            };
            Ok((Some(session), ()))
        })
        .await
    }

    pub(super) async fn sweep_backlog_with_mappings(
        &self,
        library: &crate::library_state::LibraryState,
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
        let library_transaction = library.begin_transaction()?;
        let available = library
            .lock()
            .expect("library mutex poisoned")
            .tracks()
            .iter()
            .map(|track| track.uri.clone())
            .collect::<BTreeSet<_>>();
        let mappings = self
            .mappings_for(username, Some(spotify_account_id))
            .await?;
        let result =
            reconcile_incremental(&before.backlog, &[], &mappings, &available, 0, u64::MAX);
        let (before_library, after_library) = {
            let library = library.lock().expect("library mutex poisoned");
            let before = library.clone();
            let mut after = before.clone();
            apply_incremental_updates(&mut after, &result.increments, &result.latest);
            (before, after)
        };
        let journal = LastFmApplicationJournal {
            before_library,
            after_library: after_library.clone(),
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
        let library_transaction = if !result.increments.is_empty() {
            let (transaction, ()) = library
                .replace_in_transaction(library_transaction, after_library, ())
                .await?;
            transaction
        } else {
            library_transaction
        };
        self.mutate_sync(|state| {
            state.backlog = result.unresolved.clone();
            state.journal = None;
            state.sync_problem = None;
            Ok(())
        })
        .await?;
        let result = self
            .sync_backlog_into_review(username, Some(spotify_account_id))
            .await;
        drop(library_transaction);
        result
    }

    pub(super) fn claim_sync_runner(&self) -> Option<RunnerGuard> {
        RunnerGuard::claim(&self.sync_running)
    }

    #[cfg(test)]
    pub(super) async fn save(&self, session: LastFmImportSessionV2) -> Result<(), String> {
        self.mutate_session(|_| Ok((Some(session), ()))).await
    }

    async fn enqueue_session_save(&self, session: LastFmImportSessionV2) {
        let queue = Arc::clone(&self.session_writes);
        *queue.pending.lock().await = Some(session);
        if queue.running.swap(true, Ordering::AcqRel) {
            return;
        }
        let store = self.store.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                let next = {
                    let mut pending = queue.pending.lock().await;
                    match pending.take() {
                        Some(next) => next,
                        None => {
                            queue.running.store(false, Ordering::Release);
                            return;
                        }
                    }
                };
                let result = tauri::async_runtime::spawn_blocking({
                    let store = store.clone();
                    move || store.save(&next)
                })
                .await;
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => log::warn!(
                        target: "lastfm_import",
                        "queued metadata save failed: {error}"
                    ),
                    Err(error) => log::warn!(
                        target: "lastfm_import",
                        "queued metadata save task stopped: {error}"
                    ),
                }
            }
        });
    }

    pub(super) async fn mutate_session<R, F>(&self, mutation: F) -> Result<R, String>
    where
        F: FnOnce(
            Option<LastFmImportSessionV2>,
        ) -> Result<(Option<LastFmImportSessionV2>, R), String>,
        R: Send + 'static,
    {
        self.ensure_hydrated()?;
        let persistence_gate = Arc::clone(&self.persistence_gate).lock_owned().await;
        let persistence_gate = self
            .recover_pending_review_transaction(persistence_gate)
            .await?;
        let previous = self.session.lock().await.clone();
        let (next, result) = mutation(previous.clone())?;
        if next != previous {
            let store = self.store.clone();
            let current = Arc::clone(&self.session);
            return tauri::async_runtime::spawn(async move {
                let next = if let Some(session) = next {
                    Some(
                        tauri::async_runtime::spawn_blocking(move || {
                            store.save(&session).map(|()| session)
                        })
                        .await
                        .map_err(|_| "Last.fm import persistence task stopped.".to_string())??,
                    )
                } else {
                    None
                };
                *current.lock().await = next;
                drop(persistence_gate);
                Ok::<_, String>(result)
            })
            .await
            .map_err(|error| error.to_string())?;
        }
        Ok(result)
    }

    async fn mutate_session_queued<R, F>(&self, mutation: F) -> Result<R, String>
    where
        F: FnOnce(
            Option<LastFmImportSessionV2>,
        ) -> Result<(Option<LastFmImportSessionV2>, R), String>,
        R: Send + 'static,
    {
        self.ensure_hydrated()?;
        let persistence_gate = Arc::clone(&self.persistence_gate).lock_owned().await;
        let persistence_gate = self
            .recover_pending_review_transaction(persistence_gate)
            .await?;
        let previous = self.session.lock().await.clone();
        let (next, result) = mutation(previous.clone())?;
        if next != previous {
            self.restore_mutations.ensure_allowed()?;
            *self.session.lock().await = next.clone();
            if let Some(session) = next {
                self.enqueue_session_save(session).await;
            }
        }
        drop(persistence_gate);
        Ok(result)
    }

    #[cfg(test)]
    async fn mutate_owned_session_blocking<R, F>(
        &self,
        username: &str,
        spotify_account_id: &str,
        allowed_phase: fn(ImportPhase) -> bool,
        mutation: F,
    ) -> Result<R, String>
    where
        F: FnOnce(LastFmImportSessionV2) -> Result<(LastFmImportSessionV2, R), String>,
        R: Send + 'static,
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

    #[allow(dead_code)]
    pub(super) async fn mutate_owned_session<R, F>(
        &self,
        username: &str,
        spotify_account_id: &str,
        allowed_phase: fn(ImportPhase) -> bool,
        mutation: F,
    ) -> Result<R, String>
    where
        F: FnOnce(LastFmImportSessionV2) -> Result<(LastFmImportSessionV2, R), String>,
        R: Send + 'static,
    {
        #[cfg(test)]
        return self
            .mutate_owned_session_blocking(username, spotify_account_id, allowed_phase, mutation)
            .await;
        #[cfg(not(test))]
        self.mutate_session_queued(|session| {
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

    pub(super) async fn suspend_for_account_mismatch(&self) -> Result<(), String> {
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

    pub(crate) async fn migrate_spotify_account_id(
        &self,
        legacy_id: &str,
        account_id: &str,
    ) -> Result<(), String> {
        self.ensure_hydrated()?;
        if legacy_id == account_id {
            return Ok(());
        }
        let persistence_gate = Arc::clone(&self.persistence_gate).lock_owned().await;
        let persistence_gate = self
            .recover_pending_review_transaction(persistence_gate)
            .await?;
        let sync_gate = Arc::clone(&self.sync_mutation_gate).lock_owned().await;
        let previous_session = self.session.lock().await.clone();
        let previous_sync = self.sync_state.lock().await.clone();
        let previous_mappings = self.mappings.lock().await.clone();
        let mut next_session = previous_session.clone();
        if let Some(current) = next_session.as_mut() {
            if current.spotify_account_id.as_deref() == Some(legacy_id) {
                current.spotify_account_id = Some(account_id.to_owned());
            }
        }
        let mut next_sync = previous_sync.clone();
        if next_sync.spotify_account_id.as_deref() == Some(legacy_id) {
            next_sync.spotify_account_id = Some(account_id.to_owned());
        }
        for job in &mut next_sync.apply_queue {
            if job.plan.spotify_account_id == legacy_id {
                job.plan.spotify_account_id = account_id.to_owned();
            }
        }
        if let Some(cursor) = next_sync.accept_all.as_mut() {
            if cursor.spotify_account_id == legacy_id {
                cursor.spotify_account_id = account_id.to_owned();
            }
        }
        let mut next_mappings = previous_mappings.clone();
        if next_mappings.spotify_account_id.as_deref() == Some(legacy_id) {
            next_mappings.spotify_account_id = Some(account_id.to_owned());
        }
        if next_session == previous_session
            && next_sync == previous_sync
            && next_mappings == previous_mappings
        {
            return Ok(());
        }
        self.restore_mutations.ensure_allowed()?;
        let transaction = ReviewTransaction::migration(
            next_session.clone(),
            next_sync.clone(),
            next_mappings.clone(),
        );
        let transactions = self.review_transaction_store.clone();
        let sessions = self.store.clone();
        let sync = self.incremental_store.clone();
        let mappings = self.mappings_store.clone();
        let current_session = Arc::clone(&self.session);
        let current_sync = Arc::clone(&self.sync_state);
        let current_mappings = Arc::clone(&self.mappings);
        tauri::async_runtime::spawn(async move {
            tauri::async_runtime::spawn_blocking(move || {
                transactions.save(&transaction)?;
                if let Some(session) = transaction.session.as_ref() {
                    sessions.save(session)?;
                }
                if let Some(sync_state) = transaction.sync_state.as_ref() {
                    sync.save(sync_state)?;
                }
                mappings.save(&transaction.mappings)?;
                transactions.clear()
            })
            .await
            .map_err(|_| "Last.fm account migration persistence task stopped.".to_string())??;
            let mut session = current_session.lock().await;
            let mut sync = current_sync.lock().await;
            let mut mappings = current_mappings.lock().await;
            *session = next_session;
            *sync = next_sync;
            *mappings = next_mappings;
            drop(sync_gate);
            drop(persistence_gate);
            Ok::<_, String>(())
        })
        .await
        .map_err(|error| error.to_string())?
    }

    pub(crate) async fn start_or_resume(
        &self,
        username: &str,
        history_to: u64,
        defaults: Option<ImportDefaults>,
    ) -> Result<ImportStateView, String> {
        self.ensure_hydrated()?;
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
                    self.invalidate_snapshot_if_same(&session).await?;
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

    pub(super) async fn set_metadata(
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

    pub(super) async fn checkpoint_page(
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

    pub(super) async fn checkpoint_incremental_page(
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

    pub(super) async fn aggregate_cached(
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
        if let Err(error) = self.remove_snapshot(&session.cache_id).await {
            log::warn!("Could not remove completed Last.fm import cache: {error}");
        }
        self.sync_backlog_into_review(
            &session.lastfm_username,
            session.spotify_account_id.as_deref(),
        )
        .await?;
        Ok(result)
    }

    pub(super) async fn invalidate_snapshot(&self) -> Result<(), String> {
        let Some(session) = self.snapshot().await else {
            return Ok(());
        };
        self.invalidate_snapshot_if_same(&session).await
    }

    async fn invalidate_snapshot_if_same(
        &self,
        expected: &LastFmImportSessionV2,
    ) -> Result<(), String> {
        self.ensure_hydrated()?;
        let persistence_gate = Arc::clone(&self.persistence_gate).lock_owned().await;
        let persistence_gate = self
            .recover_pending_review_transaction(persistence_gate)
            .await?;
        let same_source = self.session.lock().await.as_ref().is_some_and(|current| {
            current.cache_id == expected.cache_id
                && current.lastfm_username == expected.lastfm_username
                && current.history_to == expected.history_to
        });
        if !same_source {
            return Ok(());
        }
        let store = self.store.clone();
        let cache_id = expected.cache_id.clone();
        let username = expected.lastfm_username.clone();
        let history_to = expected.history_to;
        let current = Arc::clone(&self.session);
        tauri::async_runtime::spawn(async move {
            let quarantine_cache_id = cache_id.clone();
            tauri::async_runtime::spawn_blocking(move || {
                store.quarantine_snapshot(&quarantine_cache_id)?;
                store.quarantine()
            })
            .await
            .map_err(|_| "Last.fm import quarantine task stopped.".to_string())??;
            let mut session = current.lock().await;
            if session.as_ref().is_some_and(|current| {
                current.cache_id == cache_id
                    && current.lastfm_username == username
                    && current.history_to == history_to
            }) {
                *session = None;
            }
            drop(persistence_gate);
            Ok::<_, String>(())
        })
        .await
        .map_err(|error| error.to_string())?
    }

    pub(super) async fn set_retryable_error(
        &self,
        error: Option<RetryableError>,
    ) -> Result<(), String> {
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

    pub(super) async fn set_match(
        &self,
        username: &str,
        spotify_account_id: &str,
        batch_id: u32,
        result: MatchResult,
    ) -> Result<(), String> {
        self.set_matches(username, spotify_account_id, batch_id, vec![result], None)
            .await
    }

    pub(super) async fn set_matches(
        &self,
        username: &str,
        spotify_account_id: &str,
        batch_id: u32,
        results: Vec<MatchResult>,
        persisted_default_count_mode: Option<CountMode>,
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
            if session.spotify_account_id.is_none() {
                if let Some(mode) = persisted_default_count_mode {
                    session.default_count_mode = mode;
                }
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
            if session.collection_album_matches.contains_key(&batch_id)
                && results.iter().any(|result| {
                    result
                        .selected_uri
                        .as_deref()
                        .is_some_and(|uri| uri.starts_with("spotify:album:"))
                        || result
                            .candidates
                            .iter()
                            .any(|candidate| candidate.uri.starts_with("spotify:album:"))
                })
            {
                return Err(
                    "Release matching is unavailable after switching to album matches.".into(),
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

    pub(super) async fn rerank_collection_batch(
        &self,
        batch_id: u32,
        membership: &CollectionMembership,
        mappings: &LastFmMappings,
    ) -> Result<(), String> {
        self.mutate_session(|session| {
            let Some(mut session) = session else {
                return Err("No Last.fm import session is active.".into());
            };
            rerank_collection_session(&mut session, batch_id, membership, mappings)?;
            Ok((Some(session), ()))
        })
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn activate_collection_batch(
        &self,
        username: &str,
        spotify_account_id: &str,
        batch_id: u32,
        artist: &str,
        album: &str,
        membership: &CollectionMembership,
        mappings: &LastFmMappings,
    ) -> Result<String, String> {
        self.mutate_owned_session(
            username,
            spotify_account_id,
            review_phase_allowed,
            |mut session| {
                activate_collection_session(
                    &mut session,
                    batch_id,
                    artist,
                    album,
                    membership,
                    mappings,
                )
                .map(|source_album| (session, source_album))
            },
        )
        .await
    }

    pub(super) async fn cache_collection_album(
        &self,
        username: &str,
        spotify_account_id: &str,
        batch_id: u32,
        artist: &str,
        candidate: CollectionAlbumCandidate,
    ) -> Result<(), String> {
        self.mutate_owned_session(
            username,
            spotify_account_id,
            review_phase_allowed,
            |mut session| {
                requested_collection_batch(&session, batch_id, artist)?;
                if !candidate.matching.uri.starts_with("spotify:album:")
                    || candidate.matching.track_uris.is_empty()
                {
                    return Err("Spotify returned an invalid or empty album preview.".into());
                }
                let state = session
                    .collection_album_matches
                    .entry(batch_id)
                    .or_default();
                if let Some(existing) = state
                    .cached_candidates
                    .iter_mut()
                    .find(|existing| existing.matching.uri == candidate.matching.uri)
                {
                    *existing = candidate;
                } else {
                    state.cached_candidates.push(candidate);
                }
                Ok((session, ()))
            },
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn add_collection_album(
        &self,
        username: &str,
        spotify_account_id: &str,
        batch_id: u32,
        artist: &str,
        uri: &str,
        candidate: Option<CollectionAlbumCandidate>,
        membership: &CollectionMembership,
        mappings: &LastFmMappings,
    ) -> Result<(), String> {
        self.mutate_owned_session(
            username,
            spotify_account_id,
            review_phase_allowed,
            |mut session| {
                requested_collection_batch(&session, batch_id, artist)?;
                if let Some(candidate) = candidate {
                    if candidate.matching.uri != uri || candidate.matching.track_uris.is_empty() {
                        return Err("Spotify returned an invalid or empty album preview.".into());
                    }
                    let state = session
                        .collection_album_matches
                        .entry(batch_id)
                        .or_default();
                    if let Some(existing) = state
                        .cached_candidates
                        .iter_mut()
                        .find(|existing| existing.matching.uri == uri)
                    {
                        *existing = candidate;
                    } else {
                        state.cached_candidates.push(candidate);
                    }
                }
                let state = session
                    .collection_album_matches
                    .get_mut(&batch_id)
                    .ok_or_else(|| "Preview the Spotify album before adding it.".to_string())?;
                if !state
                    .cached_candidates
                    .iter()
                    .any(|candidate| candidate.matching.uri == uri)
                {
                    return Err("Preview the Spotify album before adding it.".into());
                }
                if !state
                    .selected_album_uris
                    .iter()
                    .any(|selected| selected == uri)
                {
                    state.selected_album_uris.push(uri.to_owned());
                }
                rerank_collection_session(&mut session, batch_id, membership, mappings)?;
                Ok((session, ()))
            },
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn seed_collection_albums(
        &self,
        username: &str,
        spotify_account_id: &str,
        batch_id: u32,
        artist: &str,
        candidates: Vec<CollectionAlbumCandidate>,
        selected_uri: Option<String>,
        membership: &CollectionMembership,
        mappings: &LastFmMappings,
    ) -> Result<(), String> {
        self.mutate_owned_session(
            username,
            spotify_account_id,
            review_phase_allowed,
            |mut session| {
                requested_collection_batch(&session, batch_id, artist)?;
                if session.collection_album_matches.contains_key(&batch_id) {
                    return Ok((session, ()));
                }
                if candidates.iter().any(|candidate| {
                    !candidate.matching.uri.starts_with("spotify:album:")
                        || candidate.matching.track_uris.is_empty()
                }) {
                    return Err("Spotify returned an invalid or empty album match.".into());
                }
                let selected_album_uris = match selected_uri {
                    Some(uri)
                        if candidates
                            .iter()
                            .any(|candidate| candidate.matching.uri == uri) =>
                    {
                        vec![uri]
                    }
                    Some(_) => return Err("The automatic Spotify album match is invalid.".into()),
                    None => Vec::new(),
                };
                session.collection_album_matches.insert(
                    batch_id,
                    CollectionAlbumMatchState {
                        cached_candidates: candidates,
                        selected_album_uris,
                        ..CollectionAlbumMatchState::default()
                    },
                );
                rerank_collection_session(&mut session, batch_id, membership, mappings)?;
                Ok((session, ()))
            },
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn remove_collection_album(
        &self,
        username: &str,
        spotify_account_id: &str,
        batch_id: u32,
        artist: &str,
        uri: &str,
        membership: &CollectionMembership,
        mappings: &LastFmMappings,
    ) -> Result<(), String> {
        self.mutate_owned_session(
            username,
            spotify_account_id,
            review_phase_allowed,
            |mut session| {
                requested_collection_batch(&session, batch_id, artist)?;
                let state = session
                    .collection_album_matches
                    .get_mut(&batch_id)
                    .ok_or_else(|| "That Spotify album is not in the match set.".to_string())?;
                let before = state.selected_album_uris.len();
                state.selected_album_uris.retain(|selected| selected != uri);
                if state.selected_album_uris.len() == before {
                    return Err("That Spotify album is not in the match set.".into());
                }
                state.full_album_choices.remove(uri);
                state.automatic_selection_disabled = true;
                rerank_collection_session(&mut session, batch_id, membership, mappings)?;
                Ok((session, ()))
            },
        )
        .await
    }

    pub(super) async fn set_collection_album_import(
        &self,
        username: &str,
        spotify_account_id: &str,
        batch_id: u32,
        artist: &str,
        uri: &str,
        enabled: bool,
    ) -> Result<(), String> {
        self.mutate_owned_session(
            username,
            spotify_account_id,
            review_phase_allowed,
            |mut session| {
                requested_collection_batch(&session, batch_id, artist)?;
                let state = session
                    .collection_album_matches
                    .get_mut(&batch_id)
                    .ok_or_else(|| "That Spotify album is not in the match set.".to_string())?;
                if !state
                    .selected_album_uris
                    .iter()
                    .any(|selected| selected == uri)
                {
                    return Err("That Spotify album is not in the match set.".into());
                }
                state.full_album_choices.insert(uri.to_owned(), enabled);
                Ok((session, ()))
            },
        )
        .await
    }

    #[cfg(test)]
    pub(super) async fn set_count_mode(
        &self,
        username: &str,
        spotify_account_id: &str,
        target_uri: &str,
        mode: CountMode,
    ) -> Result<(), String> {
        self.mutate_review_state(|session, persisted| {
            let (session, persisted) = set_count_mode_in_review(
                session,
                persisted,
                username,
                spotify_account_id,
                target_uri,
                mode,
            )?;
            Ok((session, persisted, ()))
        })
        .await
    }

    pub(super) async fn set_count_mode_queued(
        &self,
        username: &str,
        spotify_account_id: &str,
        target_uri: &str,
        mode: CountMode,
    ) -> Result<(), String> {
        #[cfg(test)]
        return self
            .set_count_mode(username, spotify_account_id, target_uri, mode)
            .await;
        #[cfg(not(test))]
        self.mutate_review_state_queued(|session, persisted| {
            let (session, persisted) = set_count_mode_in_review(
                session,
                persisted,
                username,
                spotify_account_id,
                target_uri,
                mode,
            )?;
            Ok((session, persisted, ()))
        })
        .await
    }

    #[cfg(test)]
    pub(super) async fn set_search_terms(
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

    pub(super) async fn set_search_terms_queued(
        &self,
        username: &str,
        spotify_account_id: &str,
        search_terms: bool,
    ) -> Result<(), String> {
        #[cfg(test)]
        return self
            .set_search_terms(username, spotify_account_id, search_terms)
            .await;
        #[cfg(not(test))]
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
        if session_guard
            .as_ref()
            .is_none_or(|session| session.phase == ImportPhase::Suspended)
        {
            return queue_page_view(
                session_guard.as_ref(),
                &LastFmSyncState::default(),
                cursor,
                limit,
            );
        }
        let sync = self.sync_snapshot().await;
        queue_page_view(session_guard.as_ref(), &sync, cursor, limit)
    }

    pub(crate) async fn page(
        &self,
        batch_id: u32,
        artist: &str,
        album: &str,
    ) -> Option<ImportPageView> {
        let session_guard = self.session.lock().await;
        if session_guard
            .as_ref()
            .is_none_or(|session| session.phase == ImportPhase::Suspended)
        {
            return None;
        }
        let sync = self.sync_snapshot().await;
        page_view(session_guard.as_ref(), &sync, batch_id, artist, album)
    }

    #[cfg(test)]
    pub(super) async fn update_options(
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
            |session| {
                update_options_in_session(session, batch_id, artist, album, options)
                    .map(|session| (session, ()))
            },
        )
        .await
    }

    pub(super) async fn update_options_queued(
        &self,
        username: &str,
        spotify_account_id: &str,
        batch_id: u32,
        artist: &str,
        album: &str,
        options: PageOptions,
    ) -> Result<(), String> {
        #[cfg(test)]
        return self
            .update_options(
                username,
                spotify_account_id,
                batch_id,
                artist,
                album,
                options,
            )
            .await;
        #[cfg(not(test))]
        options.validate()?;
        #[cfg(not(test))]
        self.mutate_owned_session(
            username,
            spotify_account_id,
            review_phase_allowed,
            |session| {
                update_options_in_session(session, batch_id, artist, album, options)
                    .map(|session| (session, ()))
            },
        )
        .await
    }

    pub(super) async fn combine_batches(
        &self,
        username: &str,
        spotify_account_id: &str,
        batch_ids: &[u32],
    ) -> Result<(u32, String, String), String> {
        let blocked = self
            .sync_snapshot()
            .await
            .apply_queue
            .into_iter()
            .filter(|job| {
                batch_ids.contains(&job.plan.batch_id)
                    && matches!(
                        job.status,
                        ApplyJobStatus::Queued | ApplyJobStatus::Running | ApplyJobStatus::Failed
                    )
            })
            .count();
        if blocked > 0 {
            return Err("A selected Last.fm batch has pending or failed apply work.".into());
        }
        self.mutate_owned_session(
            username,
            spotify_account_id,
            review_phase_allowed,
            |mut session| {
                let result = combine_review_batches(&mut session, batch_ids)?;
                Ok((session, result))
            },
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn review_action(
        &self,
        username: &str,
        spotify_account_id: &str,
        batch_id: u32,
        ids: Option<&[String]>,
        action: ReviewAction,
        artist: &str,
        album: &str,
    ) -> Result<(), String> {
        validate_review_action_input(action, ids)?;
        self.mutate_review_state(|session, persisted| {
            let (session, persisted) = apply_review_action(
                session,
                persisted,
                username,
                spotify_account_id,
                batch_id,
                ids,
                action,
                artist,
                album,
            )?;
            Ok((session, persisted, ()))
        })
        .await
    }

    #[allow(clippy::too_many_arguments)]
    #[cfg(test)]
    pub(super) async fn commit_rows(
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
                let include_historical_play_counts = options.include_historical_play_counts;
                let selected_track_ids = options.selected_track_ids.clone();
                session
                    .page_options
                    .insert(batch_options_key(batch_id), options);
                let default_count_mode = session.default_count_mode;
                for id in ids {
                    if include_historical_play_counts && selected_track_ids.contains(id) {
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
                if session.remaining() == 0 {
                    session.phase = ImportPhase::Done;
                }
                let view = state_view(Some(&session));
                Ok((session, view))
            },
        )
        .await
    }

    pub(super) async fn select_match(
        &self,
        username: &str,
        spotify_account_id: &str,
        batch_id: u32,
        source_id: &str,
        uri: &str,
    ) -> Result<(String, String), String> {
        self.select_matches(
            username,
            spotify_account_id,
            batch_id,
            &[(source_id.to_owned(), uri.to_owned())],
        )
        .await
    }

    pub(super) async fn select_matches(
        &self,
        username: &str,
        spotify_account_id: &str,
        batch_id: u32,
        selections: &[(String, String)],
    ) -> Result<(String, String), String> {
        if selections.is_empty() {
            return Err("No Spotify matches were selected.".into());
        }
        self.mutate_owned_session(
            username,
            spotify_account_id,
            review_phase_allowed,
            |session| select_matches_in_session(session, batch_id, selections),
        )
        .await
    }

    pub(super) fn claim_runner(&self) -> Option<RunnerGuard> {
        self.ensure_hydrated().ok()?;
        RunnerGuard::claim(&self.running)
    }
}

#[cfg(test)]
mod hydration_tests {
    use std::{sync::Arc, time::Duration};

    use super::{load_importer_stores, Service};

    #[tokio::test(flavor = "current_thread")]
    async fn delayed_importer_store_load_does_not_block_the_async_worker() {
        let started = Arc::new(tokio::sync::Notify::new());
        let (release, released) = std::sync::mpsc::channel();
        let load = {
            let started = Arc::clone(&started);
            tokio::spawn(async move {
                load_importer_stores(move || {
                    started.notify_one();
                    released.recv().unwrap();
                    7
                })
                .await
            })
        };
        started.notified().await;
        tokio::time::timeout(
            Duration::from_millis(100),
            tokio::time::sleep(Duration::from_millis(1)),
        )
        .await
        .expect("delayed importer storage must not block the Tokio worker");
        release.send(()).unwrap();
        assert_eq!(load.await.unwrap().unwrap(), 7);
    }

    #[tokio::test]
    async fn mutation_is_rejected_until_hydration_installs_state() {
        let directory = tempfile::tempdir().unwrap();
        let service = Service::new_unhydrated_with_restore_state(
            directory.path(),
            Arc::new(crate::restore_latch::RestoreMutationState::default()),
        );
        let started = Arc::new(tokio::sync::Notify::new());
        let (release, released) = std::sync::mpsc::channel();
        let hydration = {
            let service = Arc::clone(&service);
            let started = Arc::clone(&started);
            tokio::spawn(async move {
                load_importer_stores(move || {
                    started.notify_one();
                    released.recv().unwrap();
                })
                .await
                .unwrap();
                service.hydrate().await
            })
        };
        started.notified().await;
        assert_eq!(
            service
                .mutate_sync(|state| {
                    state.sync_problem = None;
                    Ok(())
                })
                .await
                .unwrap_err(),
            "Retune is still loading Last.fm import state."
        );
        assert_eq!(
            service.state().await.sync_problem.as_deref(),
            Some("Retune is still loading Last.fm import state.")
        );
        release.send(()).unwrap();
        hydration.await.unwrap().unwrap();
        service
            .mutate_sync(|state| {
                state.sync_problem = None;
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn queued_session_mutation_publishes_memory_immediately() {
        let directory = tempfile::tempdir().unwrap();
        let service = Service::new(directory.path());
        let session = super::LastFmImportSessionV2::new("user".into(), "spotify".into(), 1);
        service
            .mutate_session_queued(|_| Ok((Some(session.clone()), ())))
            .await
            .unwrap();
        assert_eq!(service.snapshot().await, Some(session));
    }
}

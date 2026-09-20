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

enum SessionResidency {
    Unhydrated,
    Resident(Option<LastFmImportSessionV2>),
    Parked {
        owner: ImportOwnerPhase,
        view: ImportStateView,
    },
}

pub(super) struct SessionSnapshot {
    pub(super) session: LastFmImportSessionV2,
    _lease: SessionLease,
}

impl std::ops::Deref for SessionSnapshot {
    type Target = LastFmImportSessionV2;

    fn deref(&self) -> &Self::Target {
        &self.session
    }
}

struct SessionLease {
    service: std::sync::Weak<Service>,
}

pub(crate) struct Service {
    pub(super) self_weak: std::sync::Weak<Service>,
    pub(super) store: ImportSessionStore,
    pub(super) incremental_store: IncrementalStore,
    pub(super) mappings_store: MappingsStore,
    pub(super) review_transaction_store: ReviewTransactionStore,
    session: Arc<Mutex<SessionResidency>>,
    pub(super) sync_state: Arc<Mutex<LastFmSyncState>>,
    sync_mutation_gate: Arc<Mutex<()>>,
    pub(super) mappings: Arc<Mutex<PersistedLastFmMappings>>,
    persistence_gate: Arc<Mutex<()>>,
    session_writes: Arc<DirtyWriteQueue>,
    review_writes: Arc<DirtyWriteQueue>,
    pub(super) reconciliation_lock: Mutex<()>,
    pub(super) lazy_match_lock: Mutex<()>,
    pub(super) running: Arc<AtomicBool>,
    pub(super) apply_running: Arc<AtomicBool>,
    pub(super) sync_running: Arc<AtomicBool>,
    importer_window_open: AtomicBool,
    active_session_leases: std::sync::atomic::AtomicUsize,
    pub(super) restore_mutations: Arc<crate::restore_latch::RestoreMutationState>,
    hydration: std::sync::atomic::AtomicU8,
}

pub(super) struct RunnerGuard {
    running: Arc<AtomicBool>,
    service: std::sync::Weak<Service>,
}

// ponytail: one coalescing writer is enough for local importer metadata.
struct DirtyWriteQueue {
    pending: Arc<Mutex<bool>>,
    running: Arc<AtomicBool>,
}

fn request_park_after_write(service: &std::sync::Weak<Service>) {
    if let Some(service) = service.upgrade() {
        service.schedule_park_if_closed();
    }
}

impl DirtyWriteQueue {
    fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(false)),
            running: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl RunnerGuard {
    pub(super) fn claim(
        running: &Arc<AtomicBool>,
        service: &std::sync::Weak<Service>,
    ) -> Option<Self> {
        (!running.swap(true, Ordering::AcqRel)).then(|| Self {
            running: Arc::clone(running),
            service: service.clone(),
        })
    }
}

impl Drop for RunnerGuard {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(service) = self.service.upgrade() {
            service.schedule_park_if_closed();
        }
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        let Some(service) = self.service.upgrade() else {
            return;
        };
        if service.active_session_leases.fetch_sub(1, Ordering::AcqRel) == 1 {
            service.schedule_park_if_closed();
        }
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
        let service = Self::new_with_restore_state(
            app_data_dir,
            Arc::new(crate::restore_latch::RestoreMutationState::default()),
        );
        service.set_importer_window_open(true);
        service
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
        review_transaction_store
            .sync_parent()
            .expect("Last.fm review transaction directory sync failed");
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
        Arc::new_cyclic(|self_weak| Self {
            self_weak: self_weak.clone(),
            store,
            incremental_store,
            mappings_store,
            review_transaction_store,
            session: Arc::new(Mutex::new(SessionResidency::Resident(session))),
            sync_state: Arc::new(Mutex::new(sync_state)),
            sync_mutation_gate: Arc::new(Mutex::new(())),
            mappings: Arc::new(Mutex::new(mappings)),
            persistence_gate: Arc::new(Mutex::new(())),
            session_writes: Arc::new(DirtyWriteQueue::new()),
            review_writes: Arc::new(DirtyWriteQueue::new()),
            reconciliation_lock: Mutex::new(()),
            lazy_match_lock: Mutex::new(()),
            running: Arc::new(AtomicBool::new(false)),
            apply_running: Arc::new(AtomicBool::new(false)),
            sync_running: Arc::new(AtomicBool::new(false)),
            importer_window_open: AtomicBool::new(false),
            active_session_leases: std::sync::atomic::AtomicUsize::new(0),
            restore_mutations,
            hydration: std::sync::atomic::AtomicU8::new(1),
        })
    }

    pub(crate) fn new_unhydrated_with_restore_state(
        app_data_dir: impl AsRef<Path>,
        restore_mutations: Arc<crate::restore_latch::RestoreMutationState>,
    ) -> Arc<Self> {
        let app_data_dir = app_data_dir.as_ref();
        Arc::new_cyclic(|self_weak| Self {
            self_weak: self_weak.clone(),
            store: ImportSessionStore::new(app_data_dir),
            incremental_store: IncrementalStore::new(app_data_dir),
            mappings_store: MappingsStore::new(app_data_dir),
            review_transaction_store: ReviewTransactionStore::new(app_data_dir),
            session: Arc::new(Mutex::new(SessionResidency::Unhydrated)),
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
            session_writes: Arc::new(DirtyWriteQueue::new()),
            review_writes: Arc::new(DirtyWriteQueue::new()),
            reconciliation_lock: Mutex::new(()),
            lazy_match_lock: Mutex::new(()),
            running: Arc::new(AtomicBool::new(false)),
            apply_running: Arc::new(AtomicBool::new(false)),
            sync_running: Arc::new(AtomicBool::new(false)),
            importer_window_open: AtomicBool::new(false),
            active_session_leases: std::sync::atomic::AtomicUsize::new(0),
            restore_mutations,
            hydration: std::sync::atomic::AtomicU8::new(0),
        })
    }

    pub(super) fn ensure_hydrated(&self) -> Result<(), String> {
        (self.hydration.load(Ordering::Acquire) == 1)
            .then_some(())
            .ok_or_else(|| "Retune is still loading Last.fm import state.".to_string())
    }

    pub(super) fn is_hydrated(&self) -> bool {
        self.hydration.load(Ordering::Acquire) == 1
    }

    pub(crate) fn set_importer_window_open(&self, open: bool) {
        self.importer_window_open.store(open, Ordering::Release);
        if !open {
            self.schedule_park_if_closed();
        }
    }

    #[cfg(test)]
    pub(super) async fn close_importer_window_and_park(&self) -> Result<(), String> {
        self.importer_window_open.store(false, Ordering::Release);
        self.park_if_closed().await
    }

    fn schedule_park_if_closed(&self) {
        if self.importer_window_open.load(Ordering::Acquire) {
            return;
        }
        let Some(service) = self.self_weak.upgrade() else {
            return;
        };
        tauri::async_runtime::spawn(async move {
            if let Err(error) = service.park_if_closed().await {
                log::warn!(target: "lastfm_import", "Could not park idle Last.fm importer state: {error}");
            }
        });
    }

    fn has_active_work(&self) -> bool {
        self.running.load(Ordering::Acquire)
            || self.apply_running.load(Ordering::Acquire)
            || self.sync_running.load(Ordering::Acquire)
            || self.active_session_leases.load(Ordering::Acquire) != 0
    }

    fn has_queued_writer(&self) -> bool {
        self.session_writes.running.load(Ordering::Acquire)
            || self.review_writes.running.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(super) async fn wait_for_queued_writes(&self) {
        loop {
            let session_pending = *self.session_writes.pending.lock().await;
            let review_pending = *self.review_writes.pending.lock().await;
            if !session_pending
                && !review_pending
                && !self.session_writes.running.load(Ordering::Acquire)
                && !self.review_writes.running.load(Ordering::Acquire)
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    pub(crate) async fn park_if_closed(&self) -> Result<(), String> {
        if self.importer_window_open.load(Ordering::Acquire)
            || self.has_active_work()
            || self.has_queued_writer()
        {
            return Ok(());
        }
        let persistence_gate = Arc::clone(&self.persistence_gate).lock_owned().await;
        if self.importer_window_open.load(Ordering::Acquire)
            || self.has_active_work()
            || self.has_queued_writer()
        {
            drop(persistence_gate);
            return Ok(());
        }
        self.restore_mutations.ensure_allowed()?;
        let session_pending = *self.session_writes.pending.lock().await;
        let review_pending = *self.review_writes.pending.lock().await;
        if session_pending
            || review_pending
            || self.session_writes.running.load(Ordering::Acquire)
            || self.review_writes.running.load(Ordering::Acquire)
        {
            drop(persistence_gate);
            return Ok(());
        }
        let payload = {
            let mut slot = self.session.lock().await;
            // Resident readers acquire their leases under this same lock. Recheck after taking
            // it so a read that passed the earlier atomic preflight cannot race this eviction.
            if self.importer_window_open.load(Ordering::Acquire)
                || self.has_active_work()
                || self.has_queued_writer()
            {
                return Ok(());
            }
            let SessionResidency::Resident(Some(session)) = &*slot else {
                return Ok(());
            };
            let owner = ImportOwnerPhase {
                cache_id: session.cache_id.clone(),
                lastfm_username: session.lastfm_username.clone(),
                spotify_account_id: session.spotify_account_id.clone(),
                phase: session.phase,
            };
            let sync = self.sync_state.lock().await;
            let mut view = if session.phase == ImportPhase::Suspended {
                suspended_state_view(session)
            } else {
                state_view(Some(session))
            };
            if matches!(session.phase, ImportPhase::Review | ImportPhase::Done) {
                view.remaining = remaining_with_apply_queue(session, &sync.apply_queue);
            }
            view.syncing = self.sync_running.load(Ordering::Acquire);
            view.last_synced_at = sync.last_synced_at;
            view.pending_review = sync.backlog.len();
            view.sync_problem = sync.sync_problem.clone();
            view.applying_all = sync.accept_all.is_some();
            drop(sync);
            let previous = std::mem::replace(&mut *slot, SessionResidency::Parked { owner, view });
            match previous {
                SessionResidency::Resident(Some(session)) => session,
                _ => unreachable!("resident Last.fm session changed while locked"),
            }
        };
        drop(persistence_gate);
        tauri::async_runtime::spawn_blocking(move || {
            drop(payload);
        })
        .await
        .map_err(|error| format!("Last.fm import session cleanup task stopped: {error}"))?;
        Ok(())
    }

    async fn ensure_session_resident_under_gate(&self) -> Result<(), String> {
        let parked_owner = {
            let slot = self.session.lock().await;
            match &*slot {
                SessionResidency::Unhydrated => {
                    return Err("Retune is still loading Last.fm import state.".into());
                }
                SessionResidency::Resident(_) => return Ok(()),
                SessionResidency::Parked { owner, .. } => owner.clone(),
            }
        };
        let store = self.store.clone();
        let mut session = tauri::async_runtime::spawn_blocking(move || {
            let mut session = store.load()?.ok_or_else(|| {
                "The parked Last.fm import session is missing from disk.".to_string()
            })?;
            refresh_cached_album_matches(&mut session);
            Ok::<_, String>(session)
        })
        .await
        .map_err(|error| format!("Last.fm import reload task stopped: {error}"))??;
        if session.cache_id != parked_owner.cache_id
            || session.lastfm_username != parked_owner.lastfm_username
            || session.spotify_account_id != parked_owner.spotify_account_id
            || session.phase != parked_owner.phase
        {
            return Err("The parked Last.fm import session identity changed on disk.".into());
        }
        let sync = self.sync_state.lock().await.clone();
        if upgrade_legacy_pending_batches(&mut session, &sync.apply_queue) {
            let store = self.store.clone();
            session = tauri::async_runtime::spawn_blocking(move || {
                store.save(&session)?;
                Ok::<_, String>(session)
            })
            .await
            .map_err(|error| format!("Last.fm import upgrade save task stopped: {error}"))??;
        }
        let mut slot = self.session.lock().await;
        match &*slot {
            SessionResidency::Parked { owner, .. } if owner == &parked_owner => {
                *slot = SessionResidency::Resident(Some(session));
                Ok(())
            }
            SessionResidency::Resident(_) => Ok(()),
            _ => Err("Last.fm import residency changed while reloading its session.".into()),
        }
    }

    async fn lock_resident_session(
        &self,
    ) -> Result<tokio::sync::MutexGuard<'_, SessionResidency>, String> {
        self.ensure_hydrated()?;
        loop {
            let slot = self.session.lock().await;
            if matches!(&*slot, SessionResidency::Resident(_)) {
                return Ok(slot);
            }
            if matches!(&*slot, SessionResidency::Unhydrated) {
                return Err("Retune is still loading Last.fm import state.".into());
            }
            drop(slot);

            // Reads of resident state never wait for a persistence queue's disk I/O. Only a
            // parked session needs the gate, so it cannot be reloaded from beneath a writer.
            let persistence_gate = Arc::clone(&self.persistence_gate).lock_owned().await;
            self.ensure_session_resident_under_gate().await?;
            let slot = self.session.lock().await;
            if matches!(&*slot, SessionResidency::Resident(_)) {
                // Keep the slot locked across this handoff. Callers acquire their lease before
                // releasing it, which prevents park_if_closed from evicting the just-reloaded
                // payload in the gap between releasing the gate and acquiring a lease.
                drop(persistence_gate);
                return Ok(slot);
            }
            if matches!(&*slot, SessionResidency::Unhydrated) {
                return Err("Retune is still loading Last.fm import state.".into());
            }
            drop(slot);
            drop(persistence_gate);
        }
    }

    fn session_lease(&self) -> SessionLease {
        self.active_session_leases.fetch_add(1, Ordering::AcqRel);
        SessionLease {
            service: self.self_weak.clone(),
        }
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
            review_transaction_store.sync_parent()?;
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
        *self.session.lock().await = SessionResidency::Resident(session);
        *self.sync_state.lock().await = sync_state;
        *self.mappings.lock().await = mappings;
        self.hydration.store(1, Ordering::Release);
        self.schedule_park_if_closed();
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
        let (mut view, resident_session) = match &*session {
            SessionResidency::Resident(Some(session))
                if session.phase == ImportPhase::Suspended =>
            {
                (suspended_state_view(session), Some(session))
            }
            SessionResidency::Resident(Some(session)) => (state_view(Some(session)), Some(session)),
            SessionResidency::Resident(None) => (state_view(None), None),
            SessionResidency::Parked { view, .. } => (view.clone(), None),
            SessionResidency::Unhydrated => {
                let mut view = state_view(None);
                view.sync_problem = Some("Retune is still loading Last.fm import state.".into());
                (view, None)
            }
        };
        let sync = self.sync_state.lock().await;
        if let Some(session) = resident_session {
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

    pub(super) async fn snapshot_for_work(&self) -> Result<Option<SessionSnapshot>, String> {
        let slot = self.lock_resident_session().await?;
        let snapshot = match &*slot {
            SessionResidency::Resident(Some(session)) => {
                let lease = self.session_lease();
                Some(SessionSnapshot {
                    session: session.clone(),
                    _lease: lease,
                })
            }
            SessionResidency::Resident(None) => None,
            SessionResidency::Parked { .. } => {
                return Err("Last.fm import session remained parked after reload.".into());
            }
            SessionResidency::Unhydrated => {
                return Err("Retune is still loading Last.fm import state.".into());
            }
        };
        drop(slot);
        Ok(snapshot)
    }

    #[cfg(test)]
    pub(super) async fn snapshot(&self) -> Option<LastFmImportSessionV2> {
        self.snapshot_for_work()
            .await
            .expect("Last.fm import snapshot should load in test")
            .map(|snapshot| snapshot.session)
    }

    #[cfg(test)]
    pub(super) async fn install_session_for_test(&self, session: LastFmImportSessionV2) {
        *self.session.lock().await = SessionResidency::Resident(Some(session));
    }

    pub(super) async fn owner_phase(&self) -> Option<ImportOwnerPhase> {
        match &*self.session.lock().await {
            SessionResidency::Resident(Some(session)) => Some(ImportOwnerPhase {
                cache_id: session.cache_id.clone(),
                lastfm_username: session.lastfm_username.clone(),
                spotify_account_id: session.spotify_account_id.clone(),
                phase: session.phase,
            }),
            SessionResidency::Parked { owner, .. } => Some(owner.clone()),
            SessionResidency::Unhydrated | SessionResidency::Resident(None) => None,
        }
    }

    pub(super) async fn has_session(&self) -> bool {
        match &*self.session.lock().await {
            SessionResidency::Resident(Some(_)) | SessionResidency::Parked { .. } => true,
            SessionResidency::Unhydrated | SessionResidency::Resident(None) => false,
        }
    }

    pub(super) async fn snapshot_with_sync_for_work(
        &self,
    ) -> Result<(Option<SessionSnapshot>, LastFmSyncState), String> {
        let slot = self.lock_resident_session().await?;
        let session = match &*slot {
            SessionResidency::Resident(Some(session)) => {
                let lease = self.session_lease();
                Some(SessionSnapshot {
                    session: session.clone(),
                    _lease: lease,
                })
            }
            SessionResidency::Resident(None) => None,
            SessionResidency::Parked { .. } => {
                return Err("Last.fm import session remained parked after reload.".into());
            }
            SessionResidency::Unhydrated => {
                return Err("Retune is still loading Last.fm import state.".into());
            }
        };
        // Preserve the session -> sync lock order to keep the returned pair coherent with the
        // persisted review transaction snapshot.
        let sync = self.sync_state.lock().await.clone();
        drop(slot);
        Ok((session, sync))
    }

    #[cfg(test)]
    pub(super) async fn snapshot_with_sync(
        &self,
    ) -> (Option<LastFmImportSessionV2>, LastFmSyncState) {
        let (session, sync) = self
            .snapshot_with_sync_for_work()
            .await
            .expect("Last.fm import session should load in test");
        (session.map(|snapshot| snapshot.session), sync)
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
        let restore_mutations = Arc::clone(&self.restore_mutations);
        tauri::async_runtime::spawn(async move {
            let saved = next.clone();
            let save = match tauri::async_runtime::spawn_blocking(move || store.save(&saved)).await
            {
                Ok(save) => save,
                Err(error) => {
                    restore_mutations.mark_recovery_required();
                    return Err(format!(
                        "Last.fm incremental sync persistence task stopped: {error}"
                    ));
                }
            };
            if let Err(error) = save {
                restore_mutations.mark_recovery_required();
                return Err(error);
            }
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
        let restore_mutations = Arc::clone(&self.restore_mutations);
        tauri::async_runtime::spawn(async move {
            let save =
                match tauri::async_runtime::spawn_blocking(move || store.save(&persisted)).await {
                    Ok(save) => save,
                    Err(error) => {
                        restore_mutations.mark_recovery_required();
                        return Err(format!(
                            "Last.fm mappings persistence task stopped: {error}"
                        ));
                    }
                };
            if let Err(error) = save {
                restore_mutations.mark_recovery_required();
                return Err(error);
            }
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
        let _session_lease = self.session_lease();
        let sync_gate = Arc::clone(&self.sync_mutation_gate);
        let transactions = self.review_transaction_store.clone();
        let sessions = self.store.clone();
        let sync = self.incremental_store.clone();
        let mappings = self.mappings_store.clone();
        let current_session = Arc::clone(&self.session);
        let current_sync = Arc::clone(&self.sync_state);
        let current_mappings = Arc::clone(&self.mappings);
        let restore_mutations = Arc::clone(&self.restore_mutations);
        restore_mutations.ensure_allowed()?;
        tauri::async_runtime::spawn(async move {
            let _sync_gate = sync_gate.lock_owned().await;
            let recovered = match tauri::async_runtime::spawn_blocking(move || {
                transactions.recover(&sessions, &sync, &mappings)
            })
            .await
            {
                Ok(Ok(recovered)) => recovered,
                Ok(Err(error)) => {
                    restore_mutations.mark_recovery_required();
                    return Err(error);
                }
                Err(error) => {
                    restore_mutations.mark_recovery_required();
                    return Err(format!(
                        "Last.fm review transaction recovery task stopped: {error}"
                    ));
                }
            };
            if let Some(recovered) = recovered {
                let mut session = current_session.lock().await;
                let mut sync = current_sync.lock().await;
                let mut mappings = current_mappings.lock().await;
                *session = SessionResidency::Resident(recovered.session);
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
        self.ensure_session_resident_under_gate().await?;
        let _session_lease = self.session_lease();
        let previous_session = match &*self.session.lock().await {
            SessionResidency::Resident(session) => session.clone(),
            _ => unreachable!("Last.fm session must be resident under persistence gate"),
        };
        let previous_mappings = self.mappings.lock().await.clone();
        let (next_session, next_mappings, result) =
            mutation(previous_session, previous_mappings.clone())?;
        let session_unchanged = matches!(
            &*self.session.lock().await,
            SessionResidency::Resident(Some(current)) if current == &next_session
        );
        if session_unchanged && previous_mappings == next_mappings {
            return Ok(result);
        }
        self.restore_mutations.ensure_allowed()?;
        let transaction = ReviewTransaction::new(Some(next_session), next_mappings);
        let transactions = self.review_transaction_store.clone();
        let sessions = self.store.clone();
        let mappings = self.mappings_store.clone();
        let current_session = Arc::clone(&self.session);
        let current_mappings = Arc::clone(&self.mappings);
        let restore_mutations = Arc::clone(&self.restore_mutations);
        tauri::async_runtime::spawn(async move {
            let saved = tauri::async_runtime::spawn_blocking(move || {
                transactions.save(&transaction)?;
                sessions.save(transaction.session.as_ref().expect("transaction session"))?;
                mappings.save(&transaction.mappings)?;
                transactions.clear()?;
                Ok::<_, String>((
                    transaction.session.expect("transaction session"),
                    transaction.mappings,
                ))
            })
            .await;
            let (next_session, next_mappings) = match saved {
                Ok(Ok(saved)) => saved,
                Ok(Err(error)) => {
                    restore_mutations.mark_recovery_required();
                    return Err(error);
                }
                Err(error) => {
                    restore_mutations.mark_recovery_required();
                    return Err(format!(
                        "Last.fm review transaction persistence task stopped: {error}"
                    ));
                }
            };
            *current_session.lock().await = SessionResidency::Resident(Some(next_session));
            *current_mappings.lock().await = next_mappings;
            drop(persistence_gate);
            Ok::<_, String>(result)
        })
        .await
        .map_err(|error| error.to_string())?
    }

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
        self.ensure_session_resident_under_gate().await?;
        let _session_lease = self.session_lease();
        let previous_session = match &*self.session.lock().await {
            SessionResidency::Resident(session) => session.clone(),
            _ => unreachable!("Last.fm session must be resident under persistence gate"),
        };
        let previous_mappings = self.mappings.lock().await.clone();
        let (next_session, next_mappings, result) =
            mutation(previous_session, previous_mappings.clone())?;
        let session_unchanged = matches!(
            &*self.session.lock().await,
            SessionResidency::Resident(Some(current)) if current == &next_session
        );
        if session_unchanged && previous_mappings == next_mappings {
            drop(persistence_gate);
            return Ok(result);
        }
        self.restore_mutations.ensure_allowed()?;
        *self.session.lock().await = SessionResidency::Resident(Some(next_session));
        *self.mappings.lock().await = next_mappings;
        self.enqueue_review_save().await;
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
        let Some(session) = self.snapshot_for_work().await? else {
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
        if backlog.is_empty() && !self.has_session().await {
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
            session
                .archived_source_ids
                .retain(|id| row_ids.contains(id.as_str()));
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
            let mut next_batches =
                build_review_batches_preserving_names(&regular_rows, &previous_batches);
            for batch in &mut next_batches {
                while reserved_pages.contains(&next_page) {
                    next_page += 1;
                }
                batch.page = next_page;
                next_page += 1;
            }
            next_batches.extend(custom_batches);
            next_batches.sort_by_key(|batch| batch.page);
            for batch in &next_batches {
                if batch
                    .source_ids
                    .iter()
                    .any(|id| !session.archived_source_ids.contains(id))
                {
                    for id in &batch.source_ids {
                        session.archived_source_ids.remove(id);
                    }
                }
            }
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
        RunnerGuard::claim(&self.sync_running, &self.self_weak)
    }

    #[cfg(test)]
    pub(super) async fn save(&self, session: LastFmImportSessionV2) -> Result<(), String> {
        self.mutate_session(|_| Ok((Some(session), ()))).await
    }

    async fn enqueue_session_save(&self) {
        let queue = Arc::clone(&self.session_writes);
        *queue.pending.lock().await = true;
        if queue.running.swap(true, Ordering::AcqRel) {
            return;
        }
        let store = self.store.clone();
        let persistence_gate = Arc::clone(&self.persistence_gate);
        let current = Arc::clone(&self.session);
        let restore_mutations = Arc::clone(&self.restore_mutations);
        let service = self.self_weak.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                let persistence_gate = persistence_gate.clone().lock_owned().await;
                if restore_mutations.ensure_allowed().is_err() {
                    queue.running.store(false, Ordering::Release);
                    drop(persistence_gate);
                    request_park_after_write(&service);
                    return;
                }
                let dirty = {
                    let mut pending = queue.pending.lock().await;
                    std::mem::take(&mut *pending)
                };
                if !dirty {
                    queue.running.store(false, Ordering::Release);
                    drop(persistence_gate);
                    request_park_after_write(&service);
                    return;
                }
                let next = {
                    let current = current.lock().await;
                    match &*current {
                        SessionResidency::Resident(session) => session.clone(),
                        SessionResidency::Parked { .. } | SessionResidency::Unhydrated => {
                            restore_mutations.mark_recovery_required();
                            queue.running.store(false, Ordering::Release);
                            drop(persistence_gate);
                            request_park_after_write(&service);
                            return;
                        }
                    }
                };
                let Some(next) = next else {
                    // A durable invalidation can supersede a queued session while this worker
                    // waits for the gate. The invalidation already removed its persisted state;
                    // never resurrect that stale snapshot.
                    drop(persistence_gate);
                    continue;
                };
                let result = tauri::async_runtime::spawn_blocking({
                    let store = store.clone();
                    move || store.save(&next)
                })
                .await;
                match result {
                    Ok(Ok(())) => drop(persistence_gate),
                    Ok(Err(error)) => {
                        restore_mutations.mark_recovery_required();
                        queue.running.store(false, Ordering::Release);
                        log::error!(
                            target: "lastfm_import",
                            "queued metadata save failed; restart Retune to recover: {error}"
                        );
                        drop(persistence_gate);
                        request_park_after_write(&service);
                        return;
                    }
                    Err(error) => {
                        restore_mutations.mark_recovery_required();
                        queue.running.store(false, Ordering::Release);
                        log::error!(
                            target: "lastfm_import",
                            "queued metadata save task stopped; restart Retune to recover: {error}"
                        );
                        drop(persistence_gate);
                        request_park_after_write(&service);
                        return;
                    }
                }
            }
        });
    }

    async fn enqueue_review_save(&self) {
        let queue = Arc::clone(&self.review_writes);
        *queue.pending.lock().await = true;
        if queue.running.swap(true, Ordering::AcqRel) {
            return;
        }
        let transactions = self.review_transaction_store.clone();
        let sessions = self.store.clone();
        let mappings_store = self.mappings_store.clone();
        let persistence_gate = Arc::clone(&self.persistence_gate);
        let current_session = Arc::clone(&self.session);
        let current_mappings = Arc::clone(&self.mappings);
        let restore_mutations = Arc::clone(&self.restore_mutations);
        let service = self.self_weak.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                let persistence_gate = persistence_gate.clone().lock_owned().await;
                if restore_mutations.ensure_allowed().is_err() {
                    queue.running.store(false, Ordering::Release);
                    drop(persistence_gate);
                    request_park_after_write(&service);
                    return;
                }
                let dirty = {
                    let mut pending = queue.pending.lock().await;
                    std::mem::take(&mut *pending)
                };
                if !dirty {
                    queue.running.store(false, Ordering::Release);
                    drop(persistence_gate);
                    request_park_after_write(&service);
                    return;
                }
                let session = {
                    let current_session = current_session.lock().await;
                    match &*current_session {
                        SessionResidency::Resident(session) => session.clone(),
                        SessionResidency::Parked { .. } | SessionResidency::Unhydrated => {
                            restore_mutations.mark_recovery_required();
                            queue.running.store(false, Ordering::Release);
                            drop(persistence_gate);
                            request_park_after_write(&service);
                            return;
                        }
                    }
                };
                let mappings = current_mappings.lock().await.clone();
                let transaction = ReviewTransaction::new(session, mappings);
                let result = tauri::async_runtime::spawn_blocking({
                    let transactions = transactions.clone();
                    let sessions = sessions.clone();
                    let mappings_store = mappings_store.clone();
                    move || {
                        transactions.save(&transaction)?;
                        if let Some(session) = transaction.session.as_ref() {
                            sessions.save(session)?;
                        }
                        mappings_store.save(&transaction.mappings)?;
                        transactions.clear()
                    }
                })
                .await;
                match result {
                    Ok(Ok(())) => drop(persistence_gate),
                    Ok(Err(error)) => {
                        restore_mutations.mark_recovery_required();
                        queue.running.store(false, Ordering::Release);
                        log::error!(
                            target: "lastfm_import",
                            "queued review save failed; restart Retune to recover: {error}"
                        );
                        drop(persistence_gate);
                        request_park_after_write(&service);
                        return;
                    }
                    Err(error) => {
                        restore_mutations.mark_recovery_required();
                        queue.running.store(false, Ordering::Release);
                        log::error!(
                            target: "lastfm_import",
                            "queued review save task stopped; restart Retune to recover: {error}"
                        );
                        drop(persistence_gate);
                        request_park_after_write(&service);
                        return;
                    }
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
        self.ensure_session_resident_under_gate().await?;
        let _session_lease = self.session_lease();
        let previous = match &*self.session.lock().await {
            SessionResidency::Resident(session) => session.clone(),
            _ => unreachable!("Last.fm session must be resident under persistence gate"),
        };
        let (next, result) = mutation(previous)?;
        let unchanged = matches!(
            &*self.session.lock().await,
            SessionResidency::Resident(current) if current == &next
        );
        if !unchanged {
            let store = self.store.clone();
            let current = Arc::clone(&self.session);
            let restore_mutations = Arc::clone(&self.restore_mutations);
            return tauri::async_runtime::spawn(async move {
                let next = if let Some(session) = next {
                    let saved = match tauri::async_runtime::spawn_blocking(move || {
                        store.save(&session).map(|()| session)
                    })
                    .await
                    {
                        Ok(Ok(saved)) => saved,
                        Ok(Err(error)) => {
                            restore_mutations.mark_recovery_required();
                            return Err(error);
                        }
                        Err(error) => {
                            restore_mutations.mark_recovery_required();
                            return Err(format!(
                                "Last.fm import persistence task stopped: {error}"
                            ));
                        }
                    };
                    Some(saved)
                } else {
                    None
                };
                *current.lock().await = SessionResidency::Resident(next);
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
        self.ensure_session_resident_under_gate().await?;
        let _session_lease = self.session_lease();
        let previous = match &*self.session.lock().await {
            SessionResidency::Resident(session) => session.clone(),
            _ => unreachable!("Last.fm session must be resident under persistence gate"),
        };
        let (next, result) = mutation(previous)?;
        let unchanged = matches!(
            &*self.session.lock().await,
            SessionResidency::Resident(current) if current == &next
        );
        if !unchanged {
            self.restore_mutations.ensure_allowed()?;
            let has_session = next.is_some();
            *self.session.lock().await = SessionResidency::Resident(next);
            if has_session {
                self.enqueue_session_save().await;
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
        self.ensure_session_resident_under_gate().await?;
        let _session_lease = self.session_lease();
        let sync_gate = Arc::clone(&self.sync_mutation_gate).lock_owned().await;
        let previous_session = match &*self.session.lock().await {
            SessionResidency::Resident(session) => session.clone(),
            _ => unreachable!("Last.fm session must be resident under persistence gate"),
        };
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
        let restore_mutations = Arc::clone(&self.restore_mutations);
        tauri::async_runtime::spawn(async move {
            let saved = tauri::async_runtime::spawn_blocking(move || {
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
            .await;
            match saved {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    restore_mutations.mark_recovery_required();
                    return Err(error);
                }
                Err(error) => {
                    restore_mutations.mark_recovery_required();
                    return Err(format!(
                        "Last.fm account migration persistence task stopped: {error}"
                    ));
                }
            }
            let mut session = current_session.lock().await;
            let mut sync = current_sync.lock().await;
            let mut mappings = current_mappings.lock().await;
            *session = SessionResidency::Resident(next_session);
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
        if let Some(session) = self.snapshot_for_work().await? {
            if suspended_source_phase(&session) {
                let store = self.store.clone();
                let cache_id = session.cache_id.clone();
                let lastfm_username = session.lastfm_username.clone();
                let history_to = session.history_to;
                let SessionSnapshot {
                    session: validation_session,
                    _lease,
                } = session;
                let cache_valid = tauri::async_runtime::spawn_blocking(move || {
                    store.validate_cache(&validation_session).is_ok()
                })
                .await
                .map_err(|_| "Last.fm import cache validation task stopped.".to_string())?;
                if !cache_valid {
                    self.invalidate_snapshot_if_identity(&cache_id, &lastfm_username, history_to)
                        .await?;
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
                let view = state_view(Some(&session));
                return Ok((Some(session), view));
            }
            if let Some(existing) = session.total_pages {
                if existing != total_pages {
                    return Err("Last.fm import metadata changed during the snapshot.".into());
                }
                let view = state_view(Some(&session));
                return Ok((Some(session), view));
            }
            session.total_pages = Some(total_pages);
            session.total_scrobbles = total_scrobbles;
            session.next_page = total_pages;
            session.retryable_error = None;
            if total_pages == 0 {
                session.phase = ImportPhase::Aggregating;
            }
            let view = state_view(Some(&session));
            Ok((Some(session), view))
        })
        .await
    }

    pub(super) async fn checkpoint_page(
        &self,
        page: u32,
        parsed: &ParsedRecentTracksPage,
    ) -> Result<ImportStateView, String> {
        let Some(before) = self.snapshot_for_work().await? else {
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
        let history_to = before.history_to;
        let SessionSnapshot {
            session: mut cache_session,
            _lease,
        } = before;
        cache_session.total_pages = Some(total_pages);
        let mut filtered = parsed.clone();
        discard_post_cutoff(&mut filtered, history_to);
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
                    let view = state_view(Some(&session));
                    return Ok((Some(session), view));
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
                let view = state_view(Some(&session));
                Ok((Some(session), view))
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
        let Some(session) = self.snapshot_for_work().await? else {
            return Err("No Last.fm import session is active.".into());
        };
        if session.phase != ImportPhase::Aggregating {
            return Ok(state_view(Some(&session)));
        }
        let store = self.store.clone();
        let cache_id = session.cache_id.clone();
        let lastfm_username = session.lastfm_username.clone();
        let spotify_account_id = session.spotify_account_id.clone();
        let history_to = session.history_to;
        let SessionSnapshot {
            session: blocking_session,
            _lease,
        } = session;
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
                self.invalidate_snapshot_if_identity(&cache_id, &lastfm_username, history_to)
                    .await?;
                return Err(error);
            }
        };
        let commit = || async {
            self.mutate_session(|current| {
                let Some(mut current) = current else {
                    return Err("No Last.fm import session is active.".into());
                };
                if current.cache_id != cache_id || current.phase != ImportPhase::Aggregating {
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
                let view = state_view(Some(&current));
                Ok((Some(current), view))
            })
            .await
        };
        let result = match lastfm {
            Some(lastfm) => match lastfm.with_import_owner(&lastfm_username, commit).await? {
                Some(result) => result,
                None => {
                    self.suspend_for_account_mismatch().await?;
                    return Ok(self.state().await);
                }
            },
            None => commit().await?,
        };
        if let Err(error) = self.remove_snapshot(&cache_id).await {
            log::warn!("Could not remove completed Last.fm import cache: {error}");
        }
        self.sync_backlog_into_review(&lastfm_username, spotify_account_id.as_deref())
            .await?;
        Ok(result)
    }

    #[cfg(test)]
    pub(super) async fn invalidate_snapshot(&self) -> Result<(), String> {
        let Some(session) = self.snapshot_for_work().await? else {
            return Ok(());
        };
        self.invalidate_snapshot_if_identity(
            &session.cache_id,
            &session.lastfm_username,
            session.history_to,
        )
        .await
    }

    async fn invalidate_snapshot_if_identity(
        &self,
        cache_id: &str,
        username: &str,
        history_to: u64,
    ) -> Result<(), String> {
        self.ensure_hydrated()?;
        let persistence_gate = Arc::clone(&self.persistence_gate).lock_owned().await;
        let persistence_gate = self
            .recover_pending_review_transaction(persistence_gate)
            .await?;
        self.ensure_session_resident_under_gate().await?;
        let _session_lease = self.session_lease();
        let same_source = match &*self.session.lock().await {
            SessionResidency::Resident(Some(current)) => {
                current.cache_id == cache_id
                    && current.lastfm_username == username
                    && current.history_to == history_to
            }
            SessionResidency::Resident(None) => false,
            SessionResidency::Parked { .. } => {
                return Err("Last.fm import session remained parked after reload.".into());
            }
            SessionResidency::Unhydrated => {
                return Err("Retune is still loading Last.fm import state.".into());
            }
        };
        if !same_source {
            return Ok(());
        }
        let store = self.store.clone();
        let cache_id = cache_id.to_owned();
        let username = username.to_owned();
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
            let same_source = match &*session {
                SessionResidency::Resident(Some(current)) => {
                    current.cache_id == cache_id
                        && current.lastfm_username == username
                        && current.history_to == history_to
                }
                SessionResidency::Parked { .. } => false,
                SessionResidency::Unhydrated | SessionResidency::Resident(None) => false,
            };
            if same_source {
                *session = SessionResidency::Resident(None);
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
        let session = self.lock_resident_session().await?;
        let session_lease = self.session_lease();
        let result = match &*session {
            SessionResidency::Resident(Some(session))
                if session.phase != ImportPhase::Suspended =>
            {
                let sync = self.sync_state.lock().await;
                queue_page_view(Some(session), &sync, cursor, limit)
            }
            SessionResidency::Resident(Some(_)) | SessionResidency::Resident(None) => {
                queue_page_view(None, &LastFmSyncState::default(), cursor, limit)
            }
            SessionResidency::Parked { .. } => {
                Err("Last.fm import session remained parked after reload.".into())
            }
            SessionResidency::Unhydrated => {
                Err("Retune is still loading Last.fm import state.".into())
            }
        };
        drop(session);
        drop(session_lease);
        result
    }

    pub(crate) async fn page_for_work(
        &self,
        batch_id: u32,
        artist: &str,
        album: &str,
    ) -> Result<Option<ImportPageView>, String> {
        let session = self.lock_resident_session().await?;
        let session_lease = self.session_lease();
        let result = match &*session {
            SessionResidency::Resident(Some(session))
                if session.phase != ImportPhase::Suspended =>
            {
                let sync = self.sync_state.lock().await;
                Ok(page_view(Some(session), &sync, batch_id, artist, album))
            }
            SessionResidency::Resident(Some(_)) | SessionResidency::Resident(None) => Ok(None),
            SessionResidency::Parked { .. } => {
                Err("Last.fm import session remained parked after reload.".into())
            }
            SessionResidency::Unhydrated => {
                Err("Retune is still loading Last.fm import state.".into())
            }
        };
        drop(session);
        drop(session_lease);
        result
    }

    #[cfg(test)]
    pub(crate) async fn page(
        &self,
        batch_id: u32,
        artist: &str,
        album: &str,
    ) -> Option<ImportPageView> {
        self.page_for_work(batch_id, artist, album)
            .await
            .expect("Last.fm import page should load in test")
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

    pub(super) async fn rename_batch(
        &self,
        username: &str,
        spotify_account_id: &str,
        batch_id: u32,
        artist: &str,
        album: &str,
        name: &str,
    ) -> Result<(), String> {
        self.mutate_owned_session(
            username,
            spotify_account_id,
            review_phase_allowed,
            |session| {
                rename_batch_in_session(session, batch_id, artist, album, name)
                    .map(|session| (session, ()))
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
        action: ReviewAction,
        artist: &str,
        album: &str,
    ) -> Result<(), String> {
        self.mutate_review_state(|session, persisted| {
            let (session, persisted) = apply_review_action(
                session,
                persisted,
                username,
                spotify_account_id,
                batch_id,
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
        RunnerGuard::claim(&self.running, &self.self_weak)
    }
}

#[cfg(test)]
mod hydration_tests {
    use std::{sync::Arc, time::Duration};

    use super::{load_importer_stores, model::*, ImportSessionStore, Service, SessionResidency};

    async fn wait_until_parked(service: &Service) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(
                    &*service.session.lock().await,
                    SessionResidency::Parked { .. }
                ) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("closed importer session should be parked");
    }

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
        assert!(!service.is_hydrated());
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
        assert!(service.is_hydrated());
        service
            .mutate_sync(|state| {
                state.sync_problem = None;
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn startup_hydration_parks_an_idle_importer_without_opening_its_window() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = super::LastFmImportSessionV2::new("user".into(), "spotify".into(), 1);
        session.phase = super::ImportPhase::Review;
        ImportSessionStore::new(directory.path())
            .save(&session)
            .unwrap();
        let service = Service::new_unhydrated_with_restore_state(
            directory.path(),
            Arc::new(crate::restore_latch::RestoreMutationState::default()),
        );

        service.hydrate().await.unwrap();
        wait_until_parked(&service).await;

        assert!(service.has_session().await);
        assert_eq!(service.state().await.username.as_deref(), Some("user"));
    }

    #[tokio::test]
    async fn closing_during_a_session_lease_defers_then_retries_parking() {
        let directory = tempfile::tempdir().unwrap();
        let service = Service::new(directory.path());
        let mut expected = super::LastFmImportSessionV2::new("user".into(), "spotify".into(), 1);
        expected.phase = super::ImportPhase::Review;
        service.save(expected.clone()).await.unwrap();
        let snapshot = service.snapshot_for_work().await.unwrap().unwrap();

        service.set_importer_window_open(false);
        tokio::time::timeout(Duration::from_millis(250), service.park_if_closed())
            .await
            .expect("parking should not wait for an active lease")
            .unwrap();
        assert!(matches!(
            &*service.session.lock().await,
            SessionResidency::Resident(Some(_))
        ));

        drop(snapshot);
        wait_until_parked(&service).await;
        let reloaded = service.snapshot_for_work().await.unwrap().unwrap();
        assert_eq!(reloaded.session, expected);
        assert_eq!(service.state().await.username.as_deref(), Some("user"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn queued_save_completes_before_the_tail_retry_parks() {
        let directory = tempfile::tempdir().unwrap();
        let service = Service::new(directory.path());
        let mut expected = super::LastFmImportSessionV2::new("user".into(), "spotify".into(), 1);
        expected.phase = super::ImportPhase::Review;
        let hook = crate::store::SaveHook::new(false);
        service.store.arm_save(Arc::clone(&hook));

        service
            .mutate_session_queued(|_| Ok((Some(expected.clone()), ())))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !hook.is_reached() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("queued save should reach its store hook");
        hook.wait_until_reached();

        service.set_importer_window_open(false);
        tokio::time::timeout(Duration::from_millis(250), service.park_if_closed())
            .await
            .expect("parking should return while a queued writer is active")
            .unwrap();
        assert!(matches!(
            &*service.session.lock().await,
            SessionResidency::Resident(Some(_))
        ));

        hook.release();
        tokio::time::timeout(Duration::from_secs(5), service.wait_for_queued_writes())
            .await
            .expect("queued save should finish");
        wait_until_parked(&service).await;

        let reloaded = Service::new(directory.path());
        assert_eq!(reloaded.snapshot().await, Some(expected));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failed_queued_save_keeps_the_session_resident_for_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let service = Service::new(directory.path());
        let mut expected = super::LastFmImportSessionV2::new("user".into(), "spotify".into(), 1);
        expected.phase = super::ImportPhase::Review;
        let hook = crate::store::SaveHook::new(true);
        service.store.arm_save(Arc::clone(&hook));

        service
            .mutate_session_queued(|_| Ok((Some(expected), ())))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !hook.is_reached() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("queued save should reach its store hook");
        hook.wait_until_reached();
        hook.release();
        tokio::time::timeout(Duration::from_secs(5), service.wait_for_queued_writes())
            .await
            .expect("failed queued save should finish");

        assert!(service.close_importer_window_and_park().await.is_err());
        assert!(matches!(
            &*service.session.lock().await,
            SessionResidency::Resident(Some(_))
        ));
        assert!(service.has_session().await);
    }

    #[tokio::test]
    async fn failed_parked_reload_keeps_the_cached_owner_and_reports_error() {
        let directory = tempfile::tempdir().unwrap();
        let service = Service::new(directory.path());
        let mut session = super::LastFmImportSessionV2::new("user".into(), "spotify".into(), 1);
        session.phase = super::ImportPhase::Review;
        service.save(session).await.unwrap();
        service.set_importer_window_open(false);
        wait_until_parked(&service).await;
        std::fs::remove_file(&service.store.path).unwrap();

        let reload = service.snapshot_for_work().await;
        assert!(matches!(
            reload,
            Err(ref error) if error.contains("parked Last.fm import session is missing from disk")
        ));
        assert!(service.has_session().await);
        assert_eq!(service.state().await.username.as_deref(), Some("user"));
        assert!(matches!(
            &*service.session.lock().await,
            SessionResidency::Parked { .. }
        ));
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

    #[tokio::test(flavor = "current_thread")]
    async fn queued_review_save_keeps_mappings_after_session_reset() {
        let directory = tempfile::tempdir().unwrap();
        let service = Service::new(directory.path());
        let mappings = PersistedLastFmMappings {
            version: super::LASTFM_MAPPINGS_VERSION,
            lastfm_username: Some("user".into()),
            spotify_account_id: Some("spotify".into()),
            mappings: LastFmMappings {
                default_count_mode: CountMode::Overwrite,
                ..LastFmMappings::default()
            },
            ..PersistedLastFmMappings::default()
        };
        *service.session.lock().await = super::SessionResidency::Resident(None);
        *service.mappings.lock().await = mappings.clone();

        service.enqueue_review_save().await;
        tokio::time::timeout(Duration::from_secs(5), service.wait_for_queued_writes())
            .await
            .expect("queued review save should finish");

        let reloaded = Service::new(directory.path());
        assert!(reloaded.snapshot().await.is_none());
        assert_eq!(reloaded.export_mappings().await, mappings);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn queued_review_save_cannot_overwrite_newer_durable_state() {
        let directory = tempfile::tempdir().unwrap();
        let service = Service::new(directory.path());
        let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 1);
        session.phase = ImportPhase::Review;
        service.save(session).await.unwrap();
        service
            .save_mappings_for("user", Some("spotify"), LastFmMappings::default())
            .await
            .unwrap();

        let hook = crate::store::SaveHook::new(false);
        service.mappings_store.arm_save(Arc::clone(&hook));
        let queued = {
            let service = Arc::clone(&service);
            tokio::spawn(async move {
                service
                    .set_count_mode_queued(
                        "user",
                        "spotify",
                        "spotify:track:queued",
                        CountMode::Zero,
                    )
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            while !hook.is_reached() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("queued review save should reach its store hook");
        hook.wait_until_reached();

        let durable = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.set_search_terms("user", "spotify", true).await })
        };
        tokio::task::yield_now().await;
        assert!(!durable.is_finished());

        hook.release();
        queued.await.unwrap().unwrap();
        durable.await.unwrap().unwrap();

        let reloaded = Service::new(directory.path());
        let session = reloaded.snapshot().await.unwrap();
        assert!(session.search_terms);
        assert_eq!(session.default_count_mode, CountMode::Zero);
        assert_eq!(
            reloaded
                .mappings_for("user", Some("spotify"))
                .await
                .unwrap()
                .default_count_mode,
            CountMode::Zero
        );
    }
}

use std::{
    collections::VecDeque,
    future::Future,
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use serde::Serialize;
use serde_json::Value;
use tauri::Manager;
use tauri_plugin_opener::OpenerExt;
use tokio::sync::Mutex;
use url::Url;

mod api;
mod listening;
mod store;

#[cfg(test)]
pub(crate) use api::FakeRequestExecutor;
#[cfg(test)]
use api::{collect_response_body_with_limit, error_code, signature, RESPONSE_BODY_LIMIT};
pub(crate) use api::{credentials_from, Credentials};
use api::{response_text, Failure, HttpRequestExecutor, RequestExecutor};
use listening::*;
pub(crate) use listening::{AcceptedScrobbleReceipt, ScrobbleMetadata};
use store::*;

const AUTH_URL: &str = "https://www.last.fm/api/auth";
pub(crate) const CREDENTIAL_SERVICE: &str = "com.rianjs.retune";
pub(crate) const SESSION_ACCOUNT: &str = "lastfm-session";
const RETRY_DELAYS: &[Duration] = &[
    Duration::from_secs(1),
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(60),
    Duration::from_secs(300),
];
pub(crate) const LASTFM_PAGE_LIMIT: u32 = 200;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LastFmState {
    pub available: bool,
    pub connected: bool,
    pub username: Option<String>,
    pub pending: bool,
    pub reconnect_required: bool,
    pub problem: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImportFetchError {
    pub message: String,
    pub retryable: bool,
    pub account_mismatch: bool,
}

fn import_recent_tracks_params(
    username: &str,
    page: u32,
    from: u64,
    to: u64,
) -> Vec<(String, String)> {
    vec![
        ("user".into(), username.into()),
        ("page".into(), page.to_string()),
        ("limit".into(), LASTFM_PAGE_LIMIT.to_string()),
        ("from".into(), from.to_string()),
        ("to".into(), to.to_string()),
    ]
}

struct Runtime {
    enabled: bool,
    session: Option<LastFmSession>,
    pending: Option<String>,
    queue: VecDeque<Scrobble>,
    queue_owner: Option<String>,
    reconnect_required: bool,
    build_problem: bool,
    storage_problem: bool,
    flushing: bool,
    queue_revision: u64,
}

const HYDRATION_LOADING: u8 = 0;
const HYDRATION_READY: u8 = 1;

pub(crate) struct Service {
    credentials: Option<Credentials>,
    request_executor: Arc<dyn RequestExecutor>,
    session_store: Arc<dyn SessionStore>,
    pending_store: PendingTokenStore,
    queue_store: QueueStore,
    runtime: Mutex<Runtime>,
    accepted_receipts: Mutex<Vec<AcceptedScrobbleReceipt>>,
    reconciliation_io: Mutex<()>,
    queue_io: Mutex<()>,
    credential_io: Mutex<()>,
    flush_changed: tokio::sync::Notify,
    lifecycle_changed: tokio::sync::Notify,
    lifecycle_generation: AtomicU64,
    shutting_down: AtomicBool,
    flush_task: std::sync::Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    listening: std::sync::Mutex<ListeningState>,
    emit: Arc<dyn Fn(LastFmState) + Send + Sync>,
    hydration: std::sync::atomic::AtomicU8,
}

type LoadedLastFm = (
    Option<LastFmSession>,
    Option<String>,
    VecDeque<Scrobble>,
    Vec<AcceptedScrobbleReceipt>,
    Option<String>,
    bool,
);

fn load_persisted_lastfm(
    credentials_available: bool,
    session_store: Arc<dyn SessionStore>,
    pending_store: PendingTokenStore,
    queue_store: QueueStore,
) -> LoadedLastFm {
    let mut storage_problem = false;
    let session = if credentials_available {
        match session_store.load() {
            Ok(session) => session.filter(valid_session),
            Err(error) => {
                storage_problem = true;
                log::error!("Last.fm local persistence failed while loading the session: {error}");
                None
            }
        }
    } else {
        None
    };
    let pending = match pending_store.load() {
        Ok(token) => token.filter(|token| !token.is_empty()),
        Err(error) => {
            log::error!(
                "Last.fm local persistence failed while loading authorization state: {error}"
            );
            None
        }
    };
    let (ledger, migrated) = match queue_store.load_ledger_with_migration() {
        Ok(result) => result,
        Err(error) => {
            log::error!(
                "Last.fm local persistence failed; queued scrobbles may be unavailable: {error}"
            );
            (ScrobbleLedgerV2::empty(), false)
        }
    };
    let mut queue = ledger.pending;
    let mut accepted = ledger.accepted;
    let mut owner = queue_owner(&queue).map(ToOwned::to_owned).or(ledger.owner);
    if let Some(session) = session.as_ref() {
        if (!queue.is_empty() || !accepted.is_empty())
            && owner.as_deref() != Some(session.username.as_str())
        {
            match queue_store.save_ledger(&ScrobbleLedgerV2::empty()) {
                Ok(()) => {
                    queue.clear();
                    accepted.clear();
                    owner = None;
                }
                Err(error) => {
                    storage_problem = true;
                    log::error!("Last.fm local persistence failed while isolating queued account state: {error}");
                }
            }
        }
    }
    if migrated {
        let migrated = ScrobbleLedgerV2 {
            version: SCROBBLE_LEDGER_VERSION,
            pending: queue.clone(),
            accepted: accepted.clone(),
            owner: owner.clone(),
        };
        if let Err(error) = queue_store.save_ledger(&migrated) {
            storage_problem = true;
            log::error!(
                "Last.fm local persistence failed while migrating the scrobble queue: {error}"
            );
        }
    }
    (session, pending, queue, accepted, owner, storage_problem)
}

impl Service {
    pub(crate) fn new_unhydrated(
        app_data_dir: impl AsRef<Path>,
        dev_store: bool,
        enabled: bool,
        credentials: Option<Credentials>,
        emit: Arc<dyn Fn(LastFmState) + Send + Sync>,
    ) -> Arc<Self> {
        Self::new_unhydrated_with_effects(
            app_data_dir,
            dev_store,
            enabled,
            credentials,
            Arc::new(HttpRequestExecutor::new()),
            emit,
        )
    }

    fn new_unhydrated_with_effects(
        app_data_dir: impl AsRef<Path>,
        dev_store: bool,
        enabled: bool,
        credentials: Option<Credentials>,
        request_executor: Arc<dyn RequestExecutor>,
        emit: Arc<dyn Fn(LastFmState) + Send + Sync>,
    ) -> Arc<Self> {
        let (session_store, storage_problem): (Arc<dyn SessionStore>, bool) = if dev_store {
            (Arc::new(FileSessionStore::new(&app_data_dir)), false)
        } else {
            match KeyringSessionStore::new() {
                Ok(store) => (Arc::new(store), false),
                Err(_) => (Arc::new(FailedSessionStore), true),
            }
        };
        Arc::new(Self {
            credentials,
            request_executor,
            session_store,
            pending_store: PendingTokenStore::new(&app_data_dir),
            queue_store: QueueStore::new(&app_data_dir),
            runtime: Mutex::new(Runtime {
                enabled,
                session: None,
                pending: None,
                queue: VecDeque::new(),
                queue_owner: None,
                reconnect_required: false,
                build_problem: false,
                storage_problem,
                flushing: false,
                queue_revision: 0,
            }),
            accepted_receipts: Mutex::new(Vec::new()),
            reconciliation_io: Mutex::new(()),
            queue_io: Mutex::new(()),
            credential_io: Mutex::new(()),
            flush_changed: tokio::sync::Notify::new(),
            lifecycle_changed: tokio::sync::Notify::new(),
            lifecycle_generation: AtomicU64::new(0),
            shutting_down: AtomicBool::new(false),
            flush_task: std::sync::Mutex::new(None),
            listening: std::sync::Mutex::new(ListeningState::default()),
            emit,
            hydration: std::sync::atomic::AtomicU8::new(HYDRATION_LOADING),
        })
    }

    pub(crate) async fn hydrate(&self) -> Result<(), String> {
        let credentials_available = self.credentials.is_some();
        let session_store = Arc::clone(&self.session_store);
        let pending_store = self.pending_store.clone();
        let queue_store = self.queue_store.clone();
        let (session, pending, queue, accepted, owner, storage_problem) =
            tauri::async_runtime::spawn_blocking(move || {
                load_persisted_lastfm(
                    credentials_available,
                    session_store,
                    pending_store,
                    queue_store,
                )
            })
            .await
            .map_err(|error| error.to_string())?;
        let _credential_io = self.credential_io.lock().await;
        let mut runtime = self.runtime.lock().await;
        runtime.session = session;
        runtime.pending = pending;
        runtime.queue = queue;
        runtime.queue_owner = owner;
        runtime.storage_problem |= storage_problem;
        drop(runtime);
        *self.accepted_receipts.lock().await = accepted;
        self.hydration.store(HYDRATION_READY, Ordering::Release);
        self.emit_state().await;
        Ok(())
    }

    #[cfg(test)]
    fn hydrate_for_test(mut service: Arc<Self>) -> Arc<Self> {
        let (session, pending, queue, accepted, owner, storage_problem) = load_persisted_lastfm(
            service.credentials.is_some(),
            Arc::clone(&service.session_store),
            service.pending_store.clone(),
            service.queue_store.clone(),
        );
        let service_mut = Arc::get_mut(&mut service).expect("test service is not shared yet");
        let runtime = service_mut.runtime.get_mut();
        runtime.session = session;
        runtime.pending = pending;
        runtime.queue = queue;
        runtime.queue_owner = owner;
        runtime.storage_problem |= storage_problem;
        *service_mut.accepted_receipts.get_mut() = accepted;
        service_mut
            .hydration
            .store(HYDRATION_READY, Ordering::Release);
        service
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(
        app_data_dir: impl AsRef<Path>,
        dev_store: bool,
        enabled: bool,
    ) -> Arc<Self> {
        Self::hydrate_for_test(Self::new_unhydrated(
            app_data_dir,
            dev_store,
            enabled,
            credentials_from(Some("test-api-key"), Some("test-shared-secret")),
            Arc::new(|_| {}),
        ))
    }

    #[cfg(test)]
    pub(crate) fn new_with_fake_executor(
        app_data_dir: impl AsRef<Path>,
        dev_store: bool,
        enabled: bool,
    ) -> (Arc<Self>, Arc<FakeRequestExecutor>) {
        let executor = Arc::new(FakeRequestExecutor::default());
        let service = Self::hydrate_for_test(Self::new_unhydrated_with_effects(
            app_data_dir,
            dev_store,
            enabled,
            credentials_from(Some("test-api-key"), Some("test-shared-secret")),
            executor.clone(),
            Arc::new(|_| {}),
        ));
        (service, executor)
    }

    async fn persist_queue(&self, queue: VecDeque<Scrobble>) -> Result<(), String> {
        let accepted = self.accepted_receipts.lock().await.clone();
        let owner = self.runtime.lock().await.queue_owner.clone();
        self.persist_ledger(queue, accepted, owner).await
    }

    async fn persist_ledger(
        &self,
        pending: VecDeque<Scrobble>,
        accepted: Vec<AcceptedScrobbleReceipt>,
        owner: Option<String>,
    ) -> Result<(), String> {
        let store = self.queue_store.clone();
        tauri::async_runtime::spawn_blocking(move || {
            store.save_ledger(&ScrobbleLedgerV2 {
                version: SCROBBLE_LEDGER_VERSION,
                pending,
                accepted,
                owner,
            })
        })
        .await
        .map_err(|_| "Last.fm queue persistence task stopped.".to_string())?
    }

    async fn load_pending(&self) -> Result<Option<String>, String> {
        let store = self.pending_store.clone();
        tauri::async_runtime::spawn_blocking(move || store.load())
            .await
            .map_err(|_| "Last.fm pending-token task stopped.".to_string())?
    }

    async fn save_pending(&self, token: String) -> Result<(), String> {
        let store = self.pending_store.clone();
        tauri::async_runtime::spawn_blocking(move || store.save(&token))
            .await
            .map_err(|_| "Last.fm pending-token task stopped.".to_string())?
    }

    async fn clear_pending(&self) -> Result<(), String> {
        let store = self.pending_store.clone();
        tauri::async_runtime::spawn_blocking(move || store.clear())
            .await
            .map_err(|_| "Last.fm pending-token task stopped.".to_string())?
    }

    async fn save_session(&self, session: LastFmSession) -> Result<(), String> {
        let store = Arc::clone(&self.session_store);
        tauri::async_runtime::spawn_blocking(move || store.save(&session))
            .await
            .map_err(|_| "Last.fm session-store task stopped.".to_string())?
    }

    async fn clear_session(&self) -> Result<(), String> {
        let store = Arc::clone(&self.session_store);
        tauri::async_runtime::spawn_blocking(move || store.clear())
            .await
            .map_err(|_| "Last.fm session-store task stopped.".to_string())?
    }

    pub(crate) async fn state(&self) -> LastFmState {
        if self.hydration.load(Ordering::Acquire) != HYDRATION_READY {
            return LastFmState {
                available: false,
                connected: false,
                username: None,
                pending: false,
                reconnect_required: false,
                problem: Some("Retune is still loading Last.fm state.".into()),
            };
        }
        let runtime = self.runtime.lock().await;
        let (available, problem) = if self.credentials.is_none() {
            (
                false,
                Some("Last.fm is unavailable in this build because its app credentials are not configured.".into()),
            )
        } else if runtime.storage_problem {
            (
                false,
                Some(
                    "Last.fm is unavailable because secure credential storage could not be opened."
                        .into(),
                ),
            )
        } else if runtime.build_problem {
            (
                false,
                Some(
                    "This Retune build cannot use Last.fm because its app identity was rejected."
                        .into(),
                ),
            )
        } else if runtime.reconnect_required {
            (
                true,
                Some(
                    "Your Last.fm session expired. Reconnect Last.fm to resume scrobbling.".into(),
                ),
            )
        } else {
            (true, None)
        };
        LastFmState {
            available,
            connected: available && runtime.session.is_some(),
            username: available
                .then(|| {
                    runtime
                        .session
                        .as_ref()
                        .map(|session| session.username.clone())
                })
                .flatten(),
            pending: available && runtime.pending.is_some(),
            reconnect_required: available && runtime.reconnect_required,
            problem,
        }
    }

    pub(crate) async fn accepted_receipts(&self) -> Vec<AcceptedScrobbleReceipt> {
        self.accepted_receipts.lock().await.clone()
    }

    pub(crate) async fn reconciliation_guard(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.reconciliation_io.lock().await
    }

    pub(crate) async fn prune_accepted_receipts_locked(
        &self,
        consumed: &[AcceptedScrobbleReceipt],
        through: Option<u64>,
    ) -> Result<(), String> {
        if consumed.is_empty() && through.is_none() {
            return Ok(());
        }
        let _queue_io = self.queue_io.lock().await;
        let original = self.accepted_receipts.lock().await.clone();
        let mut next = original.clone();
        for receipt in consumed {
            if let Some(index) = next.iter().position(|candidate| candidate == receipt) {
                next.remove(index);
            }
        }
        if let Some(checkpoint) = through {
            next.retain(|receipt| receipt.timestamp >= checkpoint);
        }
        if next == original {
            return Ok(());
        }
        let (pending, owner) = {
            let runtime = self.runtime.lock().await;
            (runtime.queue.clone(), runtime.queue_owner.clone())
        };
        self.persist_ledger(pending, next.clone(), owner).await?;
        *self.accepted_receipts.lock().await = next;
        Ok(())
    }

    #[cfg(test)]
    async fn prune_accepted_receipts(
        &self,
        consumed: &[AcceptedScrobbleReceipt],
        through: Option<u64>,
    ) -> Result<(), String> {
        let _reconciliation_io = self.reconciliation_guard().await;
        self.prune_accepted_receipts_locked(consumed, through).await
    }

    pub(crate) async fn with_import_owner<F, Fut, T>(
        &self,
        username: &str,
        operation: F,
    ) -> Result<Option<T>, String>
    where
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<T, String>> + Send,
        T: Send,
    {
        self.ensure_available().await?;
        let _credential_io = self.credential_io.lock().await;
        let owns_import = self
            .runtime
            .lock()
            .await
            .session
            .as_ref()
            .is_some_and(|session| session.username == username);
        if !owns_import {
            return Ok(None);
        }
        Ok(Some(operation().await?))
    }

    pub(crate) fn import_generation(&self) -> u64 {
        self.lifecycle_generation.load(Ordering::Acquire)
    }

    fn signal_lifecycle_change(&self) {
        self.lifecycle_generation.fetch_add(1, Ordering::AcqRel);
        self.lifecycle_changed.notify_waiters();
        self.flush_changed.notify_waiters();
    }

    async fn import_lifecycle_error(
        &self,
        username: &str,
        generation: u64,
    ) -> Option<ImportFetchError> {
        let changed = self.import_generation() != generation;
        let shutting_down = self.shutting_down.load(Ordering::Acquire);
        let owns_import = self
            .runtime
            .lock()
            .await
            .session
            .as_ref()
            .is_some_and(|session| session.username == username);
        (changed || shutting_down || !owns_import).then(|| ImportFetchError {
            message: if shutting_down {
                "Last.fm stopped while the importer was waiting to retry.".into()
            } else {
                "The connected Last.fm account changed; resume the importer after reconnecting."
                    .into()
            },
            retryable: false,
            account_mismatch: true,
        })
    }

    pub(crate) async fn wait_for_import_retry(
        &self,
        username: &str,
        generation: u64,
        delay: Duration,
    ) -> Result<(), ImportFetchError> {
        let notified = self.lifecycle_changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if let Some(error) = self.import_lifecycle_error(username, generation).await {
            return Err(error);
        }
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = &mut notified => {}
        }
        match self.import_lifecycle_error(username, generation).await {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub(crate) async fn set_enabled(self: &Arc<Self>, enabled: bool) {
        let should_flush = {
            let mut runtime = self.runtime.lock().await;
            runtime.enabled = enabled;
            flush_ready(&runtime)
        };
        self.flush_changed.notify_one();
        if should_flush {
            Arc::clone(self).schedule_flush().await;
        }
    }

    pub(crate) async fn settle_before_import(self: &Arc<Self>) {
        let should_flush = {
            let runtime = self.runtime.lock().await;
            flush_ready(&runtime) && !runtime.flushing
        };
        if should_flush {
            Arc::clone(self).schedule_flush().await;
        }
        loop {
            let notified = self.flush_changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let waiting = {
                let runtime = self.runtime.lock().await;
                runtime.flushing || flush_ready(&runtime)
            };
            if !waiting {
                return;
            }
            notified.await;
        }
    }

    pub(crate) async fn connect(self: &Arc<Self>) -> Result<(Url, LastFmState), String> {
        self.ensure_available().await?;
        let payload = match self.post("auth.getToken", vec![], None).await {
            Ok(payload) => payload,
            Err(error) => return Err(self.handle_failure(error).await),
        };
        let token = response_text(&payload, &["token"])
            .filter(|token| !token.is_empty())
            .ok_or_else(|| "Last.fm did not return an authorization token.".to_string())?;
        let _credential_io = self.credential_io.lock().await;
        self.save_pending(token.clone()).await?;
        {
            let mut runtime = self.runtime.lock().await;
            runtime.pending = Some(token.clone());
            runtime.reconnect_required = false;
        }
        let mut url = Url::parse(AUTH_URL).map_err(|_| "Last.fm authorization URL is invalid.")?;
        url.query_pairs_mut()
            .append_pair(
                "api_key",
                self.credentials
                    .as_ref()
                    .expect("checked above")
                    .api_key
                    .as_str(),
            )
            .append_pair("token", &token);
        Ok((url, self.state().await))
    }

    pub(crate) async fn finish(self: &Arc<Self>) -> Result<bool, String> {
        self.ensure_available().await?;
        let token = {
            let runtime = self.runtime.lock().await;
            runtime.pending.clone()
        }
        .or(self.load_pending().await?)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| "Start Last.fm authorization first.".to_string())?;
        let payload = match self
            .post("auth.getSession", vec![("token".into(), token)], None)
            .await
        {
            Ok(payload) => payload,
            Err(error) => return Err(self.handle_failure(error).await),
        };
        let session = LastFmSession {
            username: response_text(&payload, &["session", "name"])
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "Last.fm did not return a username.".to_string())?,
            key: response_text(&payload, &["session", "key"])
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "Last.fm did not return a session key.".to_string())?,
        };
        self.finish_authentication(session).await
    }

    pub(crate) async fn activate_connection(self: &Arc<Self>) {
        log::info!("Last.fm connected");
        self.emit_state().await;
        Arc::clone(self).schedule_flush().await;
    }

    async fn finish_authentication(
        self: &Arc<Self>,
        session: LastFmSession,
    ) -> Result<bool, String> {
        let service = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            let _credential_io = service.credential_io.lock().await;
            let previous_username = service
                .runtime
                .lock()
                .await
                .session
                .as_ref()
                .map(|session| session.username.clone());
            service.commit_session(session.clone()).await?;
            Ok(previous_username.as_deref() != Some(session.username.as_str()))
        })
        .await
        .map_err(|error| error.to_string())?
    }

    async fn commit_session(&self, session: LastFmSession) -> Result<(), String> {
        let previous_session = self.runtime.lock().await.session.clone();
        self.save_session(session.clone()).await?;
        if let Err(error) = self.reconcile_queue_owner(&session.username).await {
            let rollback = match previous_session {
                Some(previous) => self.save_session(previous).await,
                None => self.clear_session().await,
            };
            if let Err(rollback_error) = rollback {
                self.runtime.lock().await.storage_problem = true;
                return Err(format!("{error} {rollback_error}"));
            }
            return Err(error);
        }
        let mut runtime = self.runtime.lock().await;
        runtime.session = Some(session.clone());
        runtime.pending = None;
        runtime.reconnect_required = false;
        runtime.build_problem = false;
        runtime.queue_owner = Some(session.username);
        drop(runtime);
        self.signal_lifecycle_change();
        if let Err(error) = self.clear_pending().await {
            log::error!(
                "Last.fm local persistence failed while clearing completed authorization: {error}"
            );
        }
        Ok(())
    }

    pub(crate) async fn disconnect(self: &Arc<Self>) -> Result<LastFmState, String> {
        self.ensure_available().await?;
        let service = Arc::clone(self);
        tauri::async_runtime::spawn(async move { service.disconnect_owned().await })
            .await
            .map_err(|error| error.to_string())?
    }

    async fn disconnect_owned(&self) -> Result<LastFmState, String> {
        let empty = VecDeque::new();
        let _credential_io = self.credential_io.lock().await;
        let _reconciliation_io = self.reconciliation_guard().await;
        let _queue_io = self.queue_io.lock().await;
        let (previous_queue, previous_owner, revision) = {
            let mut runtime = self.runtime.lock().await;
            let previous_queue = runtime.queue.clone();
            let previous_owner = runtime.queue_owner.clone();
            runtime.queue.clear();
            runtime.queue_owner = None;
            runtime.queue_revision = runtime.queue_revision.wrapping_add(1);
            (previous_queue, previous_owner, runtime.queue_revision)
        };
        let previous_receipts = self.accepted_receipts.lock().await.clone();
        if let Err(error) = self.persist_ledger(empty.clone(), Vec::new(), None).await {
            let mut runtime = self.runtime.lock().await;
            if runtime.queue_revision == revision {
                runtime.queue = previous_queue;
                runtime.queue_owner = previous_owner;
                runtime.queue_revision = runtime.queue_revision.wrapping_add(1);
            }
            *self.accepted_receipts.lock().await = previous_receipts;
            log::error!("Last.fm local persistence failed while disconnecting; session retained");
            drop(runtime);
            self.emit_state().await;
            return Err(error);
        }
        self.accepted_receipts.lock().await.clear();

        let session_error = self.clear_session().await.err();
        let pending_error = self.clear_pending().await.err();
        let session_cleared = session_error.is_none();
        let rollback_error = if session_cleared {
            None
        } else {
            self.persist_ledger(
                previous_queue.clone(),
                previous_receipts.clone(),
                previous_owner.clone(),
            )
            .await
            .err()
        };
        let mut runtime = self.runtime.lock().await;
        let mut restore_receipts = false;
        if session_cleared {
            runtime.session = None;
            runtime.reconnect_required = false;
            runtime.build_problem = false;
        } else if rollback_error.is_none() && runtime.queue_revision == revision {
            runtime.queue = previous_queue;
            runtime.queue_owner = previous_owner;
            runtime.queue_revision = runtime.queue_revision.wrapping_add(1);
            restore_receipts = true;
        } else {
            runtime.storage_problem = true;
        }
        if pending_error.is_none() {
            runtime.pending = None;
        }
        let errors = session_error
            .into_iter()
            .chain(pending_error)
            .chain(rollback_error)
            .collect::<Vec<_>>();
        drop(runtime);
        if restore_receipts {
            *self.accepted_receipts.lock().await = previous_receipts;
        }
        if session_cleared {
            self.signal_lifecycle_change();
        }
        if !errors.is_empty() {
            log::error!("Last.fm local persistence failed while disconnecting");
            self.emit_state().await;
            return Err(errors.join(" "));
        }
        log::info!("Last.fm disconnected");
        self.emit_state().await;
        Ok(self.state().await)
    }

    pub(crate) async fn handle_listening_fact(
        self: &Arc<Self>,
        fact: super::playback::ListeningFact,
    ) {
        if self.hydration.load(Ordering::Acquire) != HYDRATION_READY {
            return;
        }
        let actions = self
            .listening
            .lock()
            .expect("Last.fm listening mutex poisoned")
            .apply(fact, crate::unix_now());
        for action in actions {
            match action {
                ListeningAction::NowPlaying(scrobble) => {
                    let service = Arc::clone(self);
                    tauri::async_runtime::spawn(async move {
                        service.send_now_playing(scrobble).await;
                    });
                }
                ListeningAction::Enqueue(scrobble) => {
                    log::debug!("Last.fm scrobble eligible");
                    self.enqueue(scrobble).await;
                }
            }
        }
    }

    async fn ensure_available(&self) -> Result<(), String> {
        let state = self.state().await;
        if state.available {
            Ok(())
        } else {
            Err(state
                .problem
                .unwrap_or_else(|| "Last.fm is unavailable in this build.".into()))
        }
    }

    pub(crate) async fn import_recent_tracks_page(
        self: &Arc<Self>,
        username: &str,
        generation: u64,
        page: u32,
        from: u64,
        to: u64,
    ) -> Result<Value, ImportFetchError> {
        if let Err(message) = self.ensure_available().await {
            return Err(ImportFetchError {
                message,
                retryable: false,
                account_mismatch: false,
            });
        }
        if let Some(error) = self.import_lifecycle_error(username, generation).await {
            return Err(error);
        }
        let params = import_recent_tracks_params(username, page, from, to);
        for attempt in 0..=RETRY_DELAYS.len() {
            if let Some(error) = self.import_lifecycle_error(username, generation).await {
                return Err(error);
            }
            match self
                .post("user.getRecentTracks", params.clone(), None)
                .await
            {
                Ok(value) => {
                    if let Some(error) = self.import_lifecycle_error(username, generation).await {
                        return Err(error);
                    }
                    return Ok(value);
                }
                Err(failure) if is_retryable(failure) && attempt < RETRY_DELAYS.len() => {
                    self.wait_for_import_retry(username, generation, retry_delay(attempt))
                        .await?;
                }
                Err(failure) => {
                    let retryable = is_retryable(failure);
                    return Err(ImportFetchError {
                        message: self.handle_failure(failure).await,
                        retryable,
                        account_mismatch: false,
                    });
                }
            }
        }
        unreachable!("the Last.fm import retry loop always returns")
    }

    async fn send_now_playing(self: &Arc<Self>, scrobble: Scrobble) {
        let session = {
            let runtime = self.runtime.lock().await;
            if !runtime.enabled
                || runtime.session.is_none()
                || runtime.build_problem
                || runtime.storage_problem
            {
                return;
            }
            runtime.session.clone().expect("checked above")
        };
        let result = self
            .post(
                "track.updateNowPlaying",
                scrobble.now_playing_params(),
                Some(&session.key),
            )
            .await;
        match result {
            Ok(_) => log::debug!("Last.fm Now Playing accepted"),
            Err(Failure::Api(9)) => {
                if self.invalidate_session(&session).await {
                    log::info!("Last.fm reconnect required");
                    log::warn!("Last.fm session expired; reconnect required");
                    self.emit_state().await;
                }
            }
            Err(error) if is_build_failure(error) => {
                let mut runtime = self.runtime.lock().await;
                if runtime.session.as_ref() != Some(&session) {
                    return;
                }
                runtime.build_problem = true;
                log_build_failure(error);
                drop(runtime);
                self.emit_state().await;
            }
            Err(_) => log::warn!("Last.fm Now Playing request failed; it will not be retried"),
        }
    }

    async fn enqueue(self: &Arc<Self>, mut scrobble: Scrobble) {
        let _queue_io = self.queue_io.lock().await;
        let (queue, previous, revision) = {
            let mut runtime = self.runtime.lock().await;
            if !runtime.enabled
                || runtime.build_problem
                || runtime.storage_problem
                || (runtime.session.is_none() && !runtime.reconnect_required)
            {
                return;
            }
            let Some(owner) = runtime
                .session
                .as_ref()
                .map(|session| session.username.clone())
                .or_else(|| runtime.queue_owner.clone())
            else {
                return;
            };
            if !runtime.queue.is_empty() && queue_owner(&runtime.queue) != Some(owner.as_str()) {
                return;
            }
            scrobble.owner = owner.clone();
            let previous = runtime.queue.clone();
            runtime.queue.push_back(scrobble);
            runtime.queue_owner = Some(owner);
            runtime.queue_revision = runtime.queue_revision.wrapping_add(1);
            (runtime.queue.clone(), previous, runtime.queue_revision)
        };
        if let Err(error) = self.persist_queue(queue.clone()).await {
            let mut runtime = self.runtime.lock().await;
            if runtime.queue_revision == revision {
                runtime.queue = previous;
                runtime.queue_owner = queue_owner(&runtime.queue).map(ToOwned::to_owned);
                runtime.queue_revision = runtime.queue_revision.wrapping_add(1);
            }
            log::error!("Last.fm local persistence failed; queued scrobble may be lost: {error}");
            return;
        }
        drop(_queue_io);
        log::debug!("Last.fm scrobble eligible; queue count={}", queue.len());
        let should_flush = {
            let runtime = self.runtime.lock().await;
            flush_ready(&runtime)
        };
        if should_flush {
            Arc::clone(self).schedule_flush().await;
        }
    }

    async fn schedule_flush(self: Arc<Self>) {
        if self.shutting_down.load(Ordering::Acquire) {
            return;
        }
        let should_spawn = {
            let mut runtime = self.runtime.lock().await;
            if !flush_ready(&runtime) || runtime.flushing {
                false
            } else {
                runtime.flushing = true;
                true
            }
        };
        if !should_spawn {
            return;
        }
        let mut task_slot = self
            .flush_task
            .lock()
            .expect("Last.fm flush-task mutex poisoned");
        if self.shutting_down.load(Ordering::Acquire) {
            drop(task_slot);
            self.runtime.lock().await.flushing = false;
            self.flush_changed.notify_waiters();
            return;
        }
        let service = Arc::clone(&self);
        let task = tauri::async_runtime::spawn(async move { service.flush_loop().await });
        *task_slot = Some(task);
    }

    async fn wait_for_flush_retry(&self, generation: u64, delay: Duration) -> bool {
        let notified = self.flush_changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let ready = {
            let runtime = self.runtime.lock().await;
            flush_ready(&runtime)
        };
        if !ready
            || self.shutting_down.load(Ordering::Acquire)
            || self.import_generation() != generation
        {
            return false;
        }
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = &mut notified => {}
        }
        true
    }

    async fn flush_loop(self: Arc<Self>) {
        let mut attempt = 0;
        loop {
            let mut stopped = false;
            loop {
                if self.shutting_down.load(Ordering::Acquire) {
                    break;
                }
                let generation = self.import_generation();
                match self.flush_once().await {
                    FlushOutcome::Done => break,
                    FlushOutcome::Continue => {
                        attempt = 0;
                        continue;
                    }
                    FlushOutcome::Retry => {
                        let delay = retry_delay(attempt);
                        attempt = attempt.saturating_add(1);
                        log::debug!("Last.fm retry scheduled in {}s", delay.as_secs());
                        if !self.wait_for_flush_retry(generation, delay).await {
                            break;
                        }
                    }
                    FlushOutcome::Stop => {
                        stopped = true;
                        break;
                    }
                }
            }
            let should_restart = {
                let mut runtime = self.runtime.lock().await;
                runtime.flushing = false;
                flush_ready(&runtime) && !self.shutting_down.load(Ordering::Acquire)
            };
            if !should_restart || stopped {
                break;
            }
            let claimed_restart = {
                let mut runtime = self.runtime.lock().await;
                if flush_ready(&runtime)
                    && !runtime.flushing
                    && !self.shutting_down.load(Ordering::Acquire)
                {
                    runtime.flushing = true;
                    true
                } else {
                    false
                }
            };
            if !claimed_restart {
                break;
            }
            attempt = 0;
        }
        self.flush_changed.notify_waiters();
    }

    pub(crate) async fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
        self.signal_lifecycle_change();
        let task = self
            .flush_task
            .lock()
            .expect("Last.fm flush-task mutex poisoned")
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
        self.flush_changed.notify_waiters();
    }

    async fn flush_once(self: &Arc<Self>) -> FlushOutcome {
        let (batch, session) = {
            let runtime = self.runtime.lock().await;
            if !flush_ready(&runtime) {
                return FlushOutcome::Done;
            }
            let batch = next_batch(&runtime.queue);
            let session = runtime.session.clone().expect("checked above");
            (batch, session)
        };
        let count = batch.len();
        log::debug!("Last.fm scrobble batch count={count}");
        let params = batch
            .iter()
            .enumerate()
            .flat_map(|(index, item)| item.scrobble_params(index))
            .collect();
        match self
            .post("track.scrobble", params, Some(&session.key))
            .await
        {
            Ok(payload) => {
                let Some(results) = scrobble_results(&payload, count, Some(&batch)) else {
                    log::warn!(
                        "Last.fm returned an unusable scrobble response; preserving the queue"
                    );
                    return FlushOutcome::Retry;
                };
                let codes = results.iter().map(|result| result.code).collect::<Vec<_>>();
                let new_receipts = results
                    .iter()
                    .filter_map(|result| result.receipt.clone())
                    .collect::<Vec<_>>();
                let _reconciliation_io = self.reconciliation_guard().await;
                let _queue_io = self.queue_io.lock().await;
                let (original, queue, removed, revision, owner) = {
                    let mut runtime = self.runtime.lock().await;
                    if !queue_starts_with(&runtime.queue, &batch)
                        || runtime.session.as_ref() != Some(&session)
                    {
                        return FlushOutcome::Done;
                    }
                    let original = runtime.queue.clone();
                    let removed = apply_scrobble_results(&mut runtime.queue, &batch, &codes);
                    runtime.queue_revision = runtime.queue_revision.wrapping_add(1);
                    (
                        original,
                        runtime.queue.clone(),
                        removed,
                        runtime.queue_revision,
                        runtime.queue_owner.clone(),
                    )
                };
                let original_receipts = self.accepted_receipts.lock().await.clone();
                let mut receipts = original_receipts.clone();
                receipts.extend(new_receipts);
                *self.accepted_receipts.lock().await = receipts.clone();
                if let Err(error) = self.persist_ledger(queue.clone(), receipts, owner).await {
                    let mut runtime = self.runtime.lock().await;
                    if runtime.queue_revision == revision {
                        runtime.queue = original;
                        runtime.queue_revision = runtime.queue_revision.wrapping_add(1);
                    }
                    *self.accepted_receipts.lock().await = original_receipts;
                    log::error!(
                        "Last.fm local persistence failed after scrobble response; queue retained: {error}"
                    );
                    return FlushOutcome::Stop;
                }
                for (item, code) in removed {
                    if code == 0 {
                        log::info!(
                            "Last.fm scrobble accepted: {} - {}",
                            item.artist,
                            item.track
                        );
                    } else {
                        log::info!(
                            "Last.fm scrobble ignored (code {code}): {} - {}",
                            item.artist,
                            item.track
                        );
                    }
                }
                if queue.is_empty() {
                    FlushOutcome::Done
                } else {
                    FlushOutcome::Continue
                }
            }
            Err(Failure::Api(9)) => {
                if self.invalidate_session(&session).await {
                    log::info!("Last.fm reconnect required");
                    log::warn!("Last.fm session expired; reconnect required");
                    self.emit_state().await;
                }
                FlushOutcome::Stop
            }
            Err(error) if is_build_failure(error) => {
                let mut runtime = self.runtime.lock().await;
                if runtime.session.as_ref() != Some(&session) {
                    return FlushOutcome::Done;
                }
                runtime.build_problem = true;
                log_build_failure(error);
                drop(runtime);
                self.emit_state().await;
                FlushOutcome::Stop
            }
            Err(error) if is_retryable(error) => {
                log::warn!("Last.fm service or network request failed; queued scrobbles retained");
                FlushOutcome::Retry
            }
            Err(error) => {
                let _queue_io = self.queue_io.lock().await;
                let (original, queue, removed, revision) = {
                    let mut runtime = self.runtime.lock().await;
                    if !queue_starts_with(&runtime.queue, &batch)
                        || runtime.session.as_ref() != Some(&session)
                    {
                        return FlushOutcome::Done;
                    }
                    let original = runtime.queue.clone();
                    let removed = batch
                        .iter()
                        .filter_map(|item| runtime.queue.pop_front().map(|_| item.clone()))
                        .collect::<Vec<_>>();
                    runtime.queue_revision = runtime.queue_revision.wrapping_add(1);
                    (
                        original,
                        runtime.queue.clone(),
                        removed,
                        runtime.queue_revision,
                    )
                };
                if let Err(save_error) = self.persist_queue(queue.clone()).await {
                    let mut runtime = self.runtime.lock().await;
                    if runtime.queue_revision == revision {
                        runtime.queue = original;
                        runtime.queue_revision = runtime.queue_revision.wrapping_add(1);
                    }
                    log::error!(
                        "Last.fm local persistence failed after permanent rejection; queue retained: {save_error}"
                    );
                    return FlushOutcome::Stop;
                }
                for item in removed {
                    log::warn!(
                        "Last.fm scrobble permanently rejected (code {:?}): {} - {}",
                        error.code(),
                        item.artist,
                        item.track
                    );
                }
                if queue.is_empty() {
                    FlushOutcome::Done
                } else {
                    FlushOutcome::Continue
                }
            }
        }
    }

    async fn invalidate_session(self: &Arc<Self>, expected: &LastFmSession) -> bool {
        let service = Arc::clone(self);
        let expected = expected.clone();
        tauri::async_runtime::spawn(async move { service.invalidate_session_owned(expected).await })
            .await
            .is_ok_and(|invalidated| invalidated)
    }

    async fn invalidate_session_owned(&self, expected: LastFmSession) -> bool {
        let _credential_io = self.credential_io.lock().await;
        let current = self.runtime.lock().await.session.clone();
        if current.as_ref() != Some(&expected) {
            return false;
        }
        let clear_error = self.clear_session().await.err();
        let mut runtime = self.runtime.lock().await;
        if runtime.session.as_ref() != Some(&expected) {
            return false;
        }
        if runtime.queue_owner.is_none() {
            runtime.queue_owner = Some(expected.username.clone());
        }
        runtime.session = None;
        runtime.reconnect_required = true;
        drop(runtime);
        self.signal_lifecycle_change();
        if let Some(error) = clear_error {
            log::error!(
                "Last.fm local persistence failed while clearing an invalid session: {error}"
            );
        }
        true
    }

    async fn handle_failure(self: &Arc<Self>, failure: Failure) -> String {
        match failure {
            Failure::Api(10 | 13 | 26) => {
                let mut runtime = self.runtime.lock().await;
                runtime.build_problem = true;
                drop(runtime);
                log_build_failure(failure);
                self.emit_state().await;
                "This Retune build cannot use Last.fm because its app identity was rejected.".into()
            }
            Failure::Api(11 | 16 | 29) | Failure::Network | Failure::Http(500..=599) => {
                log::warn!("Last.fm temporary service or network error");
                "Last.fm is temporarily unavailable. Try again later.".into()
            }
            Failure::Api(9) => {
                let session = self.runtime.lock().await.session.clone();
                if let Some(session) = session {
                    self.invalidate_session(&session).await;
                }
                log::info!("Last.fm reconnect required");
                log::warn!("Last.fm session expired; reconnect required");
                self.emit_state().await;
                "Your Last.fm session expired. Reconnect Last.fm to resume scrobbling.".into()
            }
            failure @ (Failure::Api(_) | Failure::Http(_) | Failure::Response) => {
                log::warn!(
                    "Last.fm request permanently rejected (code {:?})",
                    failure.code()
                );
                "Last.fm could not complete that request.".into()
            }
        }
    }

    async fn reconcile_queue_owner(&self, username: &str) -> Result<(), String> {
        let _reconciliation_io = self.reconciliation_guard().await;
        let _queue_io = self.queue_io.lock().await;
        let (revision, queue_owned) = {
            let runtime = self.runtime.lock().await;
            (
                runtime.queue_revision,
                runtime.queue_owner.as_deref() == Some(username),
            )
        };
        let has_receipts = !self.accepted_receipts.lock().await.is_empty();
        let has_queue = !self.runtime.lock().await.queue.is_empty();
        if !queue_owned && (has_queue || has_receipts) {
            self.persist_ledger(VecDeque::new(), Vec::new(), None)
                .await?;
            let mut runtime = self.runtime.lock().await;
            if runtime.queue_revision == revision {
                runtime.queue.clear();
                runtime.queue_owner = None;
                runtime.queue_revision = runtime.queue_revision.wrapping_add(1);
            }
            self.accepted_receipts.lock().await.clear();
        }
        self.runtime.lock().await.queue_owner = Some(username.to_owned());
        Ok(())
    }

    async fn emit_state(&self) {
        (self.emit)(self.state().await);
    }
}

enum FlushOutcome {
    Done,
    Continue,
    Retry,
    Stop,
}

fn flush_ready(runtime: &Runtime) -> bool {
    runtime.enabled
        && runtime
            .session
            .as_ref()
            .is_some_and(|session| queue_owner(&runtime.queue) == Some(session.username.as_str()))
        && !runtime.queue.is_empty()
        && !runtime.build_problem
        && !runtime.storage_problem
}

fn valid_session(session: &LastFmSession) -> bool {
    !session.username.trim().is_empty() && !session.key.trim().is_empty()
}

fn is_build_failure(failure: Failure) -> bool {
    matches!(failure, Failure::Api(10 | 13 | 26))
}

fn log_build_failure(failure: Failure) {
    match failure {
        Failure::Api(26) => {
            log::warn!("Last.fm API key is suspended; automatic scrobbling is paused")
        }
        Failure::Api(10 | 13) => {
            log::warn!("Last.fm app identity was rejected; automatic scrobbling is paused")
        }
        _ => {}
    }
}

fn is_retryable(failure: Failure) -> bool {
    matches!(
        failure,
        Failure::Network
            | Failure::Response
            | Failure::Http(500..=599)
            | Failure::Api(11 | 16 | 29)
    )
}

fn retry_delay(attempt: usize) -> Duration {
    RETRY_DELAYS[attempt.min(RETRY_DELAYS.len() - 1)]
}

pub(crate) fn import_retry_delay(attempt: usize) -> Duration {
    retry_delay(attempt)
}

#[tauri::command]
pub(crate) async fn lastfm_state(
    state: tauri::State<'_, crate::AppState>,
) -> Result<LastFmState, String> {
    Ok(state.lastfm.state().await)
}

#[tauri::command]
pub(crate) async fn connect_lastfm(app: tauri::AppHandle) -> Result<LastFmState, String> {
    let (url, state) = app.state::<crate::AppState>().lastfm.connect().await?;
    app.opener()
        .open_url(url.to_string(), None::<String>)
        .map_err(|_| "Could not open Last.fm in the system browser.")?;
    Ok(state)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::thread;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::playback::SnapshotTrack;

    fn serve(response: Vec<u8>) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(&response);
        });
        (format!("http://{address}/"), handle)
    }

    fn track(artist: &str, album: &str) -> SnapshotTrack {
        SnapshotTrack {
            id: 1,
            uri: "file:///tmp/song.mp3".into(),
            name: "Song".into(),
            art: artist.into(),
            alb: album.into(),
            duration_secs: 180,
        }
    }

    fn queued_scrobble(timestamp: u64) -> Scrobble {
        Scrobble {
            artist: "Artist".into(),
            track: "Song".into(),
            album: None,
            duration_secs: 180,
            timestamp,
            owner: "user".into(),
        }
    }

    struct BlockingSessionStore {
        inner: FileSessionStore,
        save_blocker: Option<Arc<SaveBlocker>>,
        clear_blocker: Option<Arc<SaveBlocker>>,
    }

    impl SessionStore for BlockingSessionStore {
        fn load(&self) -> Result<Option<LastFmSession>, String> {
            self.inner.load()
        }

        fn save(&self, session: &LastFmSession) -> Result<(), String> {
            self.inner.save(session)?;
            if let Some(blocker) = &self.save_blocker {
                blocker.pause();
            }
            Ok(())
        }

        fn clear(&self) -> Result<(), String> {
            self.inner.clear()?;
            if let Some(blocker) = &self.clear_blocker {
                blocker.pause();
            }
            Ok(())
        }
    }

    fn save_blocker() -> (
        Arc<SaveBlocker>,
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (entered, entered_rx) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        (
            Arc::new(SaveBlocker {
                entered,
                release: std::sync::Mutex::new(release_rx),
            }),
            entered_rx,
            release,
        )
    }

    async fn wait_for_blocker(entered: std::sync::mpsc::Receiver<()>) {
        tauri::async_runtime::spawn_blocking(move || {
            entered
                .recv_timeout(Duration::from_secs(1))
                .expect("persistence reached its durable-write blocker");
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn connector_projects_initializing_and_rejects_mutation_until_hydrated() {
        let directory = tempfile::tempdir().unwrap();
        let service = Service::new_unhydrated(
            directory.path(),
            true,
            true,
            credentials_from(Some("test-api-key"), Some("test-shared-secret")),
            Arc::new(|_| {}),
        );
        let started = Arc::new(tokio::sync::Notify::new());
        let (release, released) = std::sync::mpsc::channel();
        let hydration = {
            let service = Arc::clone(&service);
            let started = Arc::clone(&started);
            tokio::spawn(async move {
                tauri::async_runtime::spawn_blocking(move || {
                    started.notify_one();
                    released.recv().unwrap();
                })
                .await
                .unwrap();
                service.hydrate().await
            })
        };
        started.notified().await;
        let state = service.state().await;
        assert!(!state.available);
        assert_eq!(
            state.problem.as_deref(),
            Some("Retune is still loading Last.fm state.")
        );
        assert_eq!(
            service.disconnect().await.unwrap_err(),
            "Retune is still loading Last.fm state."
        );

        release.send(()).unwrap();
        hydration.await.unwrap().unwrap();
        assert!(service.state().await.available);
    }

    struct FailingClearSessionStore;

    impl SessionStore for FailingClearSessionStore {
        fn load(&self) -> Result<Option<LastFmSession>, String> {
            Ok(None)
        }

        fn save(&self, _session: &LastFmSession) -> Result<(), String> {
            Ok(())
        }

        fn clear(&self) -> Result<(), String> {
            Err("session clear failed".into())
        }
    }

    struct FailingClearAndRollbackSessionStore {
        ledger_path: PathBuf,
    }

    impl SessionStore for FailingClearAndRollbackSessionStore {
        fn load(&self) -> Result<Option<LastFmSession>, String> {
            Ok(None)
        }

        fn save(&self, _session: &LastFmSession) -> Result<(), String> {
            Ok(())
        }

        fn clear(&self) -> Result<(), String> {
            fs::remove_file(&self.ledger_path).unwrap();
            fs::create_dir(&self.ledger_path).unwrap();
            Err("session clear failed".into())
        }
    }

    struct FailingSaveSessionStore {
        session: LastFmSession,
    }

    impl SessionStore for FailingSaveSessionStore {
        fn load(&self) -> Result<Option<LastFmSession>, String> {
            Ok(Some(self.session.clone()))
        }

        fn save(&self, _session: &LastFmSession) -> Result<(), String> {
            Err("session save failed".into())
        }

        fn clear(&self) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn credentials_require_a_nonempty_pair() {
        assert!(credentials_from(None, Some("secret")).is_none());
        assert!(credentials_from(Some(""), Some("secret")).is_none());
        assert!(credentials_from(Some("key"), Some(" ")).is_none());
        assert!(credentials_from(Some(" key "), Some(" secret ")).is_some());
    }

    #[tokio::test]
    async fn response_body_limit_accepts_exact_json_and_rejects_plus_one() {
        const LIMIT: usize = 64;
        let mut exact = br#"{"ok":true}"#.to_vec();
        exact.resize(LIMIT, b' ');
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            exact.len()
        )
        .into_bytes();
        let (url, server) = serve([headers, exact].concat());
        let response = reqwest::get(url).await.unwrap();
        let body = collect_response_body_with_limit(response, LIMIT)
            .await
            .unwrap();
        serde_json::from_slice::<Value>(&body).unwrap();
        server.join().unwrap();

        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            LIMIT + 1
        )
        .into_bytes();
        let (url, server) = serve([headers, vec![b' '; LIMIT + 1]].concat());
        let response = reqwest::get(url).await.unwrap();
        assert_eq!(
            collect_response_body_with_limit(response, LIMIT).await,
            Err(Failure::Response)
        );
        server.join().unwrap();

        let chunk = vec![b' '; LIMIT + 1];
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:X}\r\n",
            chunk.len()
        )
        .into_bytes();
        response.extend_from_slice(&chunk);
        response.extend_from_slice(b"\r\n0\r\n\r\n");
        let (url, server) = serve(response);
        let response = reqwest::get(url).await.unwrap();
        assert_eq!(
            collect_response_body_with_limit(response, LIMIT).await,
            Err(Failure::Response)
        );
        server.join().unwrap();

        assert_eq!(RESPONSE_BODY_LIMIT, 8 * 1024 * 1024);
    }

    #[tokio::test]
    async fn fake_executor_observes_signed_form_and_shared_response_failures() {
        let directory = tempfile::tempdir().unwrap();
        let (service, executor) = Service::new_with_fake_executor(directory.path(), true, true);
        executor.queue_json(serde_json::json!({"lfm": {"status": "ok"}}));

        service
            .post(
                "track.updateNowPlaying",
                vec![("artist".into(), "Artist".into())],
                Some("session-key"),
            )
            .await
            .unwrap();

        let requests = executor.requests();
        let params = &requests[0];
        let value = |key: &str| {
            params
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(value("api_key"), Some("test-api-key"));
        assert_eq!(value("method"), Some("track.updateNowPlaying"));
        assert_eq!(value("sk"), Some("session-key"));
        assert_eq!(value("format"), Some("json"));
        let expected_signature = signature(params, "test-shared-secret");
        assert_eq!(value("api_sig"), Some(expected_signature.as_str()));

        executor.queue_network_failure();
        assert_eq!(
            service.post("test", Vec::new(), None).await,
            Err(Failure::Network)
        );
        executor.queue_response(503, b"unavailable".as_slice());
        assert_eq!(
            service.post("test", Vec::new(), None).await,
            Err(Failure::Http(503))
        );
        executor.queue_response(200, b"not json".as_slice());
        assert_eq!(
            service.post("test", Vec::new(), None).await,
            Err(Failure::Response)
        );
        executor.queue_json(serde_json::json!({"error": {"code": 9}}));
        assert_eq!(
            service.post("test", Vec::new(), None).await,
            Err(Failure::Api(9))
        );
    }

    #[tokio::test]
    async fn injected_event_callback_receives_current_state() {
        let directory = tempfile::tempdir().unwrap();
        let executor = Arc::new(FakeRequestExecutor::default());
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let service = Service::hydrate_for_test(Service::new_unhydrated_with_effects(
            directory.path(),
            true,
            true,
            credentials_from(Some("test-api-key"), Some("test-shared-secret")),
            executor,
            Arc::new(move |state| {
                captured
                    .lock()
                    .expect("Last.fm captured event mutex poisoned")
                    .push(state);
            }),
        ));

        service.emit_state().await;

        assert_eq!(events.lock().unwrap().len(), 1);
        assert!(events.lock().unwrap()[0].available);
    }

    #[tokio::test]
    async fn import_owner_guard_rejects_a_different_connected_account() {
        let directory = tempfile::tempdir().unwrap();
        let service = Service::new_for_test(directory.path(), true, true);
        service.runtime.lock().await.session = Some(LastFmSession {
            username: "user".into(),
            key: "session".into(),
        });

        assert_eq!(
            service
                .with_import_owner("user", || async { Ok::<_, String>(7) })
                .await
                .unwrap(),
            Some(7)
        );
        assert_eq!(
            service
                .with_import_owner("other", || async { Ok::<_, String>(7) })
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn disconnect_wakes_import_backoff_without_an_old_generation_request() {
        let directory = tempfile::tempdir().unwrap();
        let (service, executor) = Service::new_with_fake_executor(directory.path(), true, true);
        service.runtime.lock().await.session = Some(LastFmSession {
            username: "user".into(),
            key: "session".into(),
        });
        executor.queue_network_failure();
        executor.queue_json(serde_json::json!({"recenttracks": {"track": []}}));
        let generation = service.import_generation();
        let task = {
            let service = Arc::clone(&service);
            tokio::spawn(async move {
                service
                    .import_recent_tracks_page("user", generation, 1, 0, 1)
                    .await
            })
        };
        while executor.requests().is_empty() {
            tokio::task::yield_now().await;
        }

        service.disconnect().await.unwrap();
        let error = tokio::time::timeout(Duration::from_millis(100), task)
            .await
            .expect("disconnect must wake the long retry")
            .unwrap()
            .unwrap_err();

        assert!(error.account_mismatch);
        assert_eq!(executor.requests().len(), 1);
    }

    #[tokio::test]
    async fn cleared_session_wakes_import_backoff_even_if_pending_cleanup_fails() {
        let directory = tempfile::tempdir().unwrap();
        let (service, executor) = Service::new_with_fake_executor(directory.path(), true, true);
        fs::create_dir(&service.pending_store.path).unwrap();
        {
            let mut runtime = service.runtime.lock().await;
            runtime.session = Some(LastFmSession {
                username: "user".into(),
                key: "session".into(),
            });
            runtime.pending = Some("pending".into());
        }
        executor.queue_network_failure();
        let generation = service.import_generation();
        let task = {
            let service = Arc::clone(&service);
            tokio::spawn(async move {
                service
                    .import_recent_tracks_page("user", generation, 1, 0, 1)
                    .await
            })
        };
        while executor.requests().is_empty() {
            tokio::task::yield_now().await;
        }

        assert!(service.disconnect().await.is_err());
        let error = tokio::time::timeout(Duration::from_millis(100), task)
            .await
            .expect("clearing the session must wake the long retry")
            .unwrap()
            .unwrap_err();

        assert!(error.account_mismatch);
        assert_eq!(executor.requests().len(), 1);
    }

    #[tokio::test]
    async fn shutdown_wakes_import_backoff() {
        let directory = tempfile::tempdir().unwrap();
        let (service, executor) = Service::new_with_fake_executor(directory.path(), true, true);
        service.runtime.lock().await.session = Some(LastFmSession {
            username: "user".into(),
            key: "session".into(),
        });
        executor.queue_network_failure();
        let generation = service.import_generation();
        let task = {
            let service = Arc::clone(&service);
            tokio::spawn(async move {
                service
                    .import_recent_tracks_page("user", generation, 1, 0, 1)
                    .await
            })
        };
        while executor.requests().is_empty() {
            tokio::task::yield_now().await;
        }

        service.shutdown().await;
        let error = tokio::time::timeout(Duration::from_millis(100), task)
            .await
            .expect("shutdown must wake the long retry")
            .unwrap()
            .unwrap_err();

        assert!(error.account_mismatch);
        assert_eq!(executor.requests().len(), 1);
    }

    #[tokio::test]
    async fn import_owner_operation_does_not_hold_the_runtime_guard() {
        let directory = tempfile::tempdir().unwrap();
        let service = Service::new_for_test(directory.path(), true, true);
        service.runtime.lock().await.session = Some(LastFmSession {
            username: "user".into(),
            key: "session".into(),
        });
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let task = {
            let service = Arc::clone(&service);
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            tokio::spawn(async move {
                service
                    .with_import_owner("user", || async move {
                        started.notify_one();
                        release.notified().await;
                        Ok::<_, String>(())
                    })
                    .await
            })
        };
        started.notified().await;

        tokio::time::timeout(Duration::from_millis(100), service.state())
            .await
            .expect("import persistence must not block Last.fm state reads");
        release.notify_one();
        assert_eq!(task.await.unwrap().unwrap(), Some(()));
    }

    #[tokio::test]
    async fn finish_reports_account_changes_without_application_state() {
        let directory = tempfile::tempdir().unwrap();
        let (service, executor) = Service::new_with_fake_executor(directory.path(), true, true);
        for expected_change in [true, false] {
            service.runtime.lock().await.pending = Some("token".into());
            executor.queue_json(serde_json::json!({
                "session": {"name": "user", "key": "session"}
            }));

            assert_eq!(service.finish().await.unwrap(), expected_change);
        }
        assert_eq!(service.state().await.username.as_deref(), Some("user"));
    }

    #[tokio::test]
    async fn cancelled_finish_publishes_the_durable_session() {
        let directory = tempfile::tempdir().unwrap();
        let (mut service, executor) = Service::new_with_fake_executor(directory.path(), true, true);
        let (blocker, entered, release) = save_blocker();
        Arc::get_mut(&mut service).unwrap().session_store = Arc::new(BlockingSessionStore {
            inner: FileSessionStore::new(directory.path()),
            save_blocker: Some(blocker),
            clear_blocker: None,
        });
        service.runtime.lock().await.pending = Some("token".into());
        executor.queue_json(serde_json::json!({
            "session": {"name": "user", "key": "session"}
        }));
        let finish = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.finish().await })
        };

        wait_for_blocker(entered).await;
        finish.abort();
        release.send(()).unwrap();
        assert!(finish.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(1), async {
            while service.state().await.username.as_deref() != Some("user") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            FileSessionStore::new(directory.path())
                .load()
                .unwrap()
                .map(|session| session.username),
            Some("user".into())
        );
    }

    #[tokio::test]
    async fn cancelled_disconnect_finishes_ledger_credentials_and_runtime_publication() {
        let directory = tempfile::tempdir().unwrap();
        let mut service = Service::new_for_test(directory.path(), true, true);
        let session = LastFmSession {
            username: "user".into(),
            key: "session".into(),
        };
        FileSessionStore::new(directory.path())
            .save(&session)
            .unwrap();
        let (blocker, entered, release) = save_blocker();
        Arc::get_mut(&mut service).unwrap().queue_store.blocker = Some(blocker);
        {
            let mut runtime = service.runtime.lock().await;
            runtime.session = Some(session);
            runtime.pending = Some("pending".into());
            runtime.queue = VecDeque::from([queued_scrobble(1)]);
            runtime.queue_owner = Some("user".into());
        }
        service.pending_store.save("pending").unwrap();
        let disconnect = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.disconnect().await })
        };

        wait_for_blocker(entered).await;
        disconnect.abort();
        release.send(()).unwrap();
        assert!(disconnect.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(1), async {
            while service.state().await.connected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(FileSessionStore::new(directory.path())
            .load()
            .unwrap()
            .is_none());
        let ledger = service.queue_store.load_ledger_with_migration().unwrap().0;
        assert!(ledger.pending.is_empty());
        assert!(ledger.accepted.is_empty());
        assert!(service.pending_store.load().unwrap().is_none());
    }

    #[tokio::test]
    async fn cancelled_invalidation_publishes_the_durable_clear() {
        let directory = tempfile::tempdir().unwrap();
        let mut service = Service::new_for_test(directory.path(), true, true);
        let session = LastFmSession {
            username: "user".into(),
            key: "session".into(),
        };
        FileSessionStore::new(directory.path())
            .save(&session)
            .unwrap();
        let (blocker, entered, release) = save_blocker();
        Arc::get_mut(&mut service).unwrap().session_store = Arc::new(BlockingSessionStore {
            inner: FileSessionStore::new(directory.path()),
            save_blocker: None,
            clear_blocker: Some(blocker),
        });
        service.runtime.lock().await.session = Some(session.clone());
        let invalidate = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.invalidate_session(&session).await })
        };

        wait_for_blocker(entered).await;
        invalidate.abort();
        release.send(()).unwrap();
        assert!(invalidate.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(1), async {
            while service.runtime.lock().await.session.is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(service.runtime.lock().await.reconnect_required);
        assert!(FileSessionStore::new(directory.path())
            .load()
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn connect_returns_authorization_and_persists_pending_without_tauri() {
        let directory = tempfile::tempdir().unwrap();
        let (service, executor) = Service::new_with_fake_executor(directory.path(), true, true);
        executor.queue_json(serde_json::json!({"token": "pending-token"}));

        let (url, state) = service.connect().await.unwrap();

        assert_eq!(
            url.as_str(),
            "https://www.last.fm/api/auth?api_key=test-api-key&token=pending-token"
        );
        assert!(state.pending);
        assert!(
            Service::new_for_test(directory.path(), true, true)
                .state()
                .await
                .pending
        );
    }

    #[test]
    fn dev_session_store_round_trips_with_owner_only_permissions() {
        let directory = tempfile::tempdir().unwrap();
        let store = FileSessionStore::new(directory.path());
        let session = LastFmSession {
            username: "user".into(),
            key: "session".into(),
        };
        store.save(&session).unwrap();
        let loaded = store.load().unwrap();
        assert!(loaded
            .as_ref()
            .is_some_and(|value| value.username == session.username && value.key == session.key));
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(directory.path().join("dev-lastfm-session.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        store.clear().unwrap();
        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn signature_sorts_parameters_and_excludes_format() {
        let params = vec![
            ("token".into(), "YOUR_REQUESTED_TOKEN".into()),
            ("format".into(), "json".into()),
            ("method".into(), "auth.getSession".into()),
            ("api_key".into(), "YOUR_API_KEY".into()),
        ];
        assert_eq!(
            signature(&params, "YOUR_SECRET"),
            "94539006de89b3c6b3c030bb1e52b9c4"
        );
    }

    #[test]
    fn signature_uses_ascii_order_for_array_parameters() {
        let params = vec![
            ("artist[1]".into(), "one".into()),
            ("artist[10]".into(), "ten".into()),
        ];
        assert_eq!(
            signature(&params, "SECRET"),
            "5529e8d265523c2b48a5183272fcac8b"
        );
    }

    #[test]
    fn recent_tracks_import_params_keep_the_fixed_cutoff_and_page_limit() {
        assert_eq!(
            import_recent_tracks_params("last.fm-user", 7, 0, 1786804381),
            vec![
                ("user".into(), "last.fm-user".into()),
                ("page".into(), "7".into()),
                ("limit".into(), "200".into()),
                ("from".into(), "0".into()),
                ("to".into(), "1786804381".into()),
            ]
        );
    }

    #[test]
    fn metadata_filters_unknown_artist_and_album() {
        assert!(Scrobble::from_track(&track("Unknown Artist", "Album"), 1).is_none());
        let scrobble = Scrobble::from_track(&track("Artist", "Unknown Album"), 1).unwrap();
        assert_eq!(scrobble.album, None);
        assert_eq!(scrobble.artist, "Artist");
        assert_eq!(scrobble.track, "Song");
        assert_eq!(scrobble.duration_secs, 180);
        assert_eq!(
            scrobble
                .scrobble_params(0)
                .into_iter()
                .map(|(key, _)| key)
                .collect::<Vec<_>>(),
            vec![
                "artist[0]".to_string(),
                "track[0]".to_string(),
                "timestamp[0]".to_string(),
                "duration[0]".to_string(),
            ]
        );
        assert!(Scrobble::from_track(&track(" ", "Album"), 1).is_none());
    }

    #[test]
    fn threshold_matches_last_fm_boundaries() {
        assert_eq!(scrobble_threshold_ms(30), None);
        assert_eq!(scrobble_threshold_ms(31), Some(15_500));
        assert_eq!(scrobble_threshold_ms(60), Some(30_000));
        assert_eq!(scrobble_threshold_ms(180), Some(90_000));
        assert_eq!(scrobble_threshold_ms(480), Some(240_000));
        assert_eq!(scrobble_threshold_ms(1200), Some(240_000));
    }

    #[test]
    fn listening_provider_eligibility_keeps_the_30_001ms_boundary() {
        let track = SnapshotTrack {
            duration_secs: 31,
            ..track("Artist", "Album")
        };
        let mut state = ListeningState::default();
        assert!(matches!(
            state
                .apply(
                    crate::playback::ListeningFact::Started {
                        generation: 1,
                        track: track.clone(),
                    },
                    1234,
                )
                .as_slice(),
            [ListeningAction::NowPlaying(scrobble)] if scrobble.timestamp == 0
        ));
        assert!(state
            .apply(
                crate::playback::ListeningFact::Forward {
                    generation: 1,
                    track: track.clone(),
                    played_ms: 15_000,
                },
                0,
            )
            .is_empty());
        assert!(matches!(
            state
                .apply(
                    crate::playback::ListeningFact::Forward {
                        generation: 1,
                        track,
                        played_ms: 16_000,
                    },
                    0,
                )
                .as_slice(),
            [ListeningAction::Enqueue(scrobble)] if scrobble.timestamp == 1234
        ));
    }

    #[test]
    fn listening_provider_rejects_stale_and_discontinuous_progress() {
        let track = track("Artist", "Album");
        let mut state = ListeningState::default();
        state.apply(
            crate::playback::ListeningFact::Started {
                generation: 1,
                track: track.clone(),
            },
            1234,
        );
        assert!(state
            .apply(
                crate::playback::ListeningFact::Forward {
                    generation: 2,
                    track: track.clone(),
                    played_ms: 100_000,
                },
                0,
            )
            .is_empty());
        state.apply(
            crate::playback::ListeningFact::Discontinuity {
                generation: 1,
                track: track.clone(),
            },
            0,
        );
        assert!(state
            .apply(
                crate::playback::ListeningFact::Forward {
                    generation: 1,
                    track: track.clone(),
                    played_ms: 100_000,
                },
                0,
            )
            .is_empty());
        assert!(state
            .apply(
                crate::playback::ListeningFact::Completed {
                    generation: 1,
                    track,
                },
                0,
            )
            .is_empty());
    }

    #[test]
    fn listening_provider_scrobbles_completion_fallback_once_per_repeat_generation() {
        let track = track("Artist", "Album");
        let mut state = ListeningState::default();
        assert_eq!(
            state
                .apply(
                    crate::playback::ListeningFact::Started {
                        generation: 1,
                        track: track.clone(),
                    },
                    1234,
                )
                .len(),
            1
        );
        assert_eq!(
            state
                .apply(
                    crate::playback::ListeningFact::Completed {
                        generation: 1,
                        track: track.clone(),
                    },
                    0,
                )
                .len(),
            1
        );
        assert!(state
            .apply(
                crate::playback::ListeningFact::Completed {
                    generation: 1,
                    track: track.clone(),
                },
                0,
            )
            .is_empty());
        assert_eq!(
            state
                .apply(
                    crate::playback::ListeningFact::Started {
                        generation: 2,
                        track: track.clone(),
                    },
                    5678,
                )
                .len(),
            1
        );
        assert_eq!(
            state
                .apply(
                    crate::playback::ListeningFact::Completed {
                        generation: 2,
                        track,
                    },
                    0,
                )
                .len(),
            1
        );
    }

    #[test]
    fn batch_is_capped_and_response_codes_are_ordered() {
        let mut items = (0..51)
            .map(|index| Scrobble {
                artist: "Artist".into(),
                track: format!("Track {index}"),
                album: None,
                duration_secs: 60,
                timestamp: index,
                owner: "user".into(),
            })
            .collect::<VecDeque<_>>();
        let first_batch = next_batch(&items);
        assert_eq!(first_batch.len(), 50);
        assert_eq!(first_batch.first().map(|item| item.timestamp), Some(0));
        assert_eq!(first_batch.last().map(|item| item.timestamp), Some(49));
        let removed = apply_scrobble_results(&mut items, &first_batch, &vec![0; first_batch.len()]);
        assert_eq!(removed.len(), 50);
        let second_batch = next_batch(&items);
        assert_eq!(second_batch.len(), 1);
        assert_eq!(second_batch[0].timestamp, 50);
        let response = serde_json::json!({
            "lfm": {"status": "ok", "scrobbles": {"scrobble": [
                {"ignoredMessage": {"code": "0"}},
                {"ignoredMessage": {"@code": "3"}}
            ]}}
        });
        assert_eq!(scrobble_codes(&response, 2), Some(vec![0, 3]));
    }

    #[test]
    fn legacy_scrobble_array_migrates_to_v2_ledger() {
        let directory = tempfile::tempdir().unwrap();
        let store = QueueStore::new(directory.path());
        let queued = VecDeque::from([queued_scrobble(12)]);
        fs::write(&store.path, serde_json::to_vec(&queued).unwrap()).unwrap();

        let (ledger, migrated) = store.load_ledger_with_migration().unwrap();

        assert!(migrated);
        assert_eq!(ledger.pending, queued);
        assert!(ledger.accepted.is_empty());
        let _service = Service::new_for_test(directory.path(), true, true);
        let persisted: Value = serde_json::from_slice(&fs::read(&store.path).unwrap()).unwrap();
        assert_eq!(persisted["version"], SCROBBLE_LEDGER_VERSION);
        assert_eq!(persisted["pending"][0]["timestamp"], 12);
    }

    #[tokio::test]
    async fn accepted_receipts_prune_through_a_checkpoint_as_a_multiset() {
        let directory = tempfile::tempdir().unwrap();
        let service = Service::new_for_test(directory.path(), true, true);
        let receipt = |timestamp| AcceptedScrobbleReceipt {
            corrected: ScrobbleMetadata {
                artist: "Artist".into(),
                album: "Album".into(),
                track: "Song".into(),
            },
            submitted: ScrobbleMetadata {
                artist: "Artist".into(),
                album: "Album".into(),
                track: "Song".into(),
            },
            timestamp,
        };
        let retained = receipt(20);
        {
            *service.accepted_receipts.lock().await = vec![receipt(10), retained.clone()];
            service.runtime.lock().await.queue_owner = Some("user".into());
        }

        service
            .prune_accepted_receipts(&[], Some(20))
            .await
            .unwrap();
        assert_eq!(service.accepted_receipts().await, vec![retained.clone()]);
        service
            .prune_accepted_receipts(&[retained], Some(21))
            .await
            .unwrap();
        assert!(service.accepted_receipts().await.is_empty());
    }

    #[test]
    fn accepted_scrobble_results_capture_corrected_and_submitted_metadata() {
        let submitted = queued_scrobble(42);
        let response = serde_json::json!({
            "scrobbles": {"scrobble": [{
                "ignoredMessage": {"code": "0"},
                "artist": {"#text": "Corrected Artist"},
                "album": {"#text": "Corrected Album"},
                "track": "Corrected Song",
                "timestamp": "43"
            }]}
        });
        let results =
            scrobble_results(&response, 1, Some(std::slice::from_ref(&submitted))).unwrap();
        let receipt = results[0].receipt.as_ref().unwrap();
        assert_eq!(receipt.corrected.artist, "Corrected Artist");
        assert_eq!(receipt.corrected.album, "Corrected Album");
        assert_eq!(receipt.corrected.track, "Corrected Song");
        assert_eq!(receipt.submitted.artist, submitted.artist);
        assert_eq!(receipt.timestamp, 43);
    }

    #[tokio::test]
    async fn flush_once_persists_only_accepted_receipts_and_keeps_unflushed_queue() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("dev-lastfm-session.json"),
            br#"{"username":"user","key":"session"}"#,
        )
        .unwrap();
        let (service, executor) = Service::new_with_fake_executor(directory.path(), true, true);
        {
            let mut runtime = service.runtime.lock().await;
            runtime.queue = (0..51).map(queued_scrobble).collect();
            runtime.queue_owner = Some("user".into());
        }
        let mut response_items = vec![
            serde_json::json!({
                "ignoredMessage": {"code": "0"},
                "artist": {"#text": "Corrected Artist"},
                "album": {"#text": "Corrected Album"},
                "track": "Corrected Song",
                "timestamp": "100"
            }),
            serde_json::json!({"ignoredMessage": {"code": "3"}}),
        ];
        response_items
            .extend((2..50).map(|_| serde_json::json!({"ignoredMessage": {"code": "3"}})));
        executor.queue_json(serde_json::json!({
            "lfm": {"status": "ok", "scrobbles": {"scrobble": response_items}}
        }));

        assert!(matches!(service.flush_once().await, FlushOutcome::Continue));

        let (ledger, migrated) = QueueStore::new(directory.path())
            .load_ledger_with_migration()
            .unwrap();
        assert!(!migrated);
        assert_eq!(ledger.pending, VecDeque::from([queued_scrobble(50)]));
        assert_eq!(ledger.accepted.len(), 1);
        assert_eq!(ledger.accepted[0].corrected.artist, "Corrected Artist");
        assert_eq!(ledger.accepted[0].corrected.album, "Corrected Album");
        assert_eq!(ledger.accepted[0].corrected.track, "Corrected Song");
        assert_eq!(ledger.accepted[0].submitted.artist, "Artist");
        assert_eq!(ledger.accepted[0].submitted.album, "");
        assert_eq!(ledger.accepted[0].submitted.track, "Song");
        assert_eq!(service.accepted_receipts().await, ledger.accepted);
    }

    #[test]
    fn ignored_old_records_are_removed_oldest_first() {
        let first = queued_scrobble(1);
        let second = queued_scrobble(2);
        let mut queue = VecDeque::from([first.clone(), second.clone()]);
        let batch = vec![first, second];
        let removed = apply_scrobble_results(&mut queue, &batch, &[3, 0]);

        assert_eq!(removed.len(), 2);
        assert_eq!(removed[0].0.timestamp, 1);
        assert_eq!(removed[0].1, 3);
        assert!(queue.is_empty());
    }

    #[test]
    fn disabling_retains_queue_and_reenable_makes_it_flushable() {
        let mut runtime = Runtime {
            enabled: true,
            session: Some(LastFmSession {
                username: "user".into(),
                key: "session".into(),
            }),
            pending: None,
            queue: VecDeque::from([queued_scrobble(1)]),
            queue_owner: Some("user".into()),
            reconnect_required: false,
            build_problem: false,
            storage_problem: false,
            flushing: false,
            queue_revision: 0,
        };
        assert!(flush_ready(&runtime));
        let queued = runtime.queue.clone();
        runtime.enabled = false;
        assert!(!flush_ready(&runtime));
        assert_eq!(runtime.queue, queued);
        runtime.enabled = true;
        assert!(flush_ready(&runtime));
    }

    #[tokio::test]
    async fn account_change_wakes_a_flush_retry_and_new_session_can_restart_immediately() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("dev-lastfm-session.json"),
            br#"{"username":"user","key":"session"}"#,
        )
        .unwrap();
        let (service, executor) = Service::new_with_fake_executor(directory.path(), true, true);
        {
            let mut runtime = service.runtime.lock().await;
            runtime.queue = VecDeque::from([queued_scrobble(1)]);
            runtime.queue_owner = Some("user".into());
        }
        executor.queue_network_failure();
        Arc::clone(&service).schedule_flush().await;
        tokio::time::timeout(Duration::from_millis(100), async {
            while executor.requests().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("flush entered retry backoff");

        service.runtime.lock().await.session = None;
        service.signal_lifecycle_change();
        tokio::time::timeout(Duration::from_millis(100), async {
            while service.runtime.lock().await.flushing {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("account change must wake retry without waiting for its timer");

        service.runtime.lock().await.session = Some(LastFmSession {
            username: "user".into(),
            key: "next-session".into(),
        });
        executor.queue_json(serde_json::json!({
            "scrobbles": {"scrobble": [{"ignoredMessage": {"code": "0"}}]}
        }));
        Arc::clone(&service).schedule_flush().await;
        tokio::time::timeout(Duration::from_millis(100), async {
            while !service.runtime.lock().await.queue.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the current session must restart the sole flush worker immediately");
        assert_eq!(executor.requests().len(), 2);
    }

    #[tokio::test]
    async fn flush_retry_does_not_sleep_after_a_lifecycle_generation_change() {
        let directory = tempfile::tempdir().unwrap();
        let service = Service::new_for_test(directory.path(), true, true);
        {
            let mut runtime = service.runtime.lock().await;
            runtime.session = Some(LastFmSession {
                username: "user".into(),
                key: "session".into(),
            });
            runtime.queue = VecDeque::from([queued_scrobble(1)]);
            runtime.queue_owner = Some("user".into());
        }
        let generation = service.import_generation();
        service.signal_lifecycle_change();

        assert!(
            !service
                .wait_for_flush_retry(generation, Duration::from_secs(300))
                .await
        );
    }

    #[tokio::test]
    async fn shutdown_prevents_a_flush_worker_from_being_started() {
        let directory = tempfile::tempdir().unwrap();
        let (service, executor) = Service::new_with_fake_executor(directory.path(), true, true);
        {
            let mut runtime = service.runtime.lock().await;
            runtime.session = Some(LastFmSession {
                username: "user".into(),
                key: "session".into(),
            });
            runtime.queue = VecDeque::from([queued_scrobble(1)]);
            runtime.queue_owner = Some("user".into());
        }

        service.shutdown().await;
        Arc::clone(&service).schedule_flush().await;

        assert!(executor.requests().is_empty());
        assert!(!service.runtime.lock().await.flushing);
        assert!(service.flush_task.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn shutdown_waits_for_a_successful_flush_ledger_commit() {
        let directory = tempfile::tempdir().unwrap();
        let (mut service, executor) = Service::new_with_fake_executor(directory.path(), true, true);
        let (blocker, entered, release) = save_blocker();
        Arc::get_mut(&mut service).unwrap().queue_store.blocker = Some(blocker);
        {
            let mut runtime = service.runtime.lock().await;
            runtime.session = Some(LastFmSession {
                username: "user".into(),
                key: "session".into(),
            });
            runtime.queue = VecDeque::from([queued_scrobble(1)]);
            runtime.queue_owner = Some("user".into());
        }
        executor.queue_json(serde_json::json!({
            "scrobbles": {"scrobble": [{"ignoredMessage": {"code": "0"}}]}
        }));
        Arc::clone(&service).schedule_flush().await;
        wait_for_blocker(entered).await;
        let shutdown = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.shutdown().await })
        };
        tokio::task::yield_now().await;
        assert!(!shutdown.is_finished());

        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), shutdown)
            .await
            .unwrap()
            .unwrap();
        let ledger = service.queue_store.load_ledger_with_migration().unwrap().0;
        assert!(ledger.pending.is_empty());
        assert_eq!(ledger.accepted.len(), 1);
        assert!(service.runtime.lock().await.queue.is_empty());
        assert_eq!(service.accepted_receipts().await.len(), 1);
    }

    #[test]
    fn queue_persistence_does_not_hold_runtime_lock() {
        tauri::async_runtime::block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let mut service = Service::new_for_test(directory.path(), true, true);
            let (entered, entered_rx) = std::sync::mpsc::channel();
            let (release, release_rx) = std::sync::mpsc::channel();
            Arc::get_mut(&mut service).unwrap().queue_store.blocker = Some(Arc::new(SaveBlocker {
                entered,
                release: std::sync::Mutex::new(release_rx),
            }));
            {
                let mut runtime = service.runtime.lock().await;
                runtime.queue_owner = Some("user".into());
                runtime.reconnect_required = true;
            }

            let task_service = Arc::clone(&service);
            let enqueue = tauri::async_runtime::spawn(async move {
                task_service.enqueue(queued_scrobble(2)).await;
            });
            tauri::async_runtime::spawn_blocking(move || {
                entered_rx
                    .recv_timeout(Duration::from_secs(1))
                    .expect("queue persistence started");
            })
            .await
            .unwrap();

            let runtime = service.runtime.lock().await;
            assert_eq!(runtime.queue.len(), 1);
            drop(runtime);

            release.send(()).unwrap();
            enqueue.await.unwrap();
        });
    }

    #[test]
    fn invalid_session_requires_reconnect_without_dropping_queue() {
        tauri::async_runtime::block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let service = Service::new_for_test(directory.path(), true, true);
            let session = LastFmSession {
                username: "user".into(),
                key: "session".into(),
            };
            let queued = VecDeque::from([queued_scrobble(1)]);
            {
                let mut runtime = service.runtime.lock().await;
                runtime.session = Some(session.clone());
                runtime.queue = queued.clone();
                runtime.queue_owner = Some("user".into());
            }

            assert!(service.invalidate_session(&session).await);

            let runtime = service.runtime.lock().await;
            assert!(runtime.session.is_none());
            assert!(runtime.reconnect_required);
            assert_eq!(runtime.queue, queued);
        });
    }

    #[test]
    fn same_user_reconnect_preserves_durable_queue() {
        tauri::async_runtime::block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let service = Service::new_for_test(directory.path(), true, true);
            let queued = VecDeque::from([queued_scrobble(1)]);
            service.queue_store.save(&queued).unwrap();
            {
                let mut runtime = service.runtime.lock().await;
                runtime.queue = queued.clone();
                runtime.queue_owner = Some("user".into());
            }

            service.reconcile_queue_owner("user").await.unwrap();

            let runtime = service.runtime.lock().await;
            assert_eq!(runtime.queue, queued);
            drop(runtime);
            assert_eq!(service.queue_store.load().unwrap(), queued);
        });
    }

    #[test]
    fn different_user_reconnect_clears_durable_queue_before_installing() {
        tauri::async_runtime::block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let service = Service::new_for_test(directory.path(), true, true);
            let mut queued = VecDeque::from([queued_scrobble(1)]);
            queued.front_mut().unwrap().owner = "old-user".into();
            service.queue_store.save(&queued).unwrap();
            let old_session = LastFmSession {
                username: "old-user".into(),
                key: "old-session".into(),
            };
            {
                let mut runtime = service.runtime.lock().await;
                runtime.session = Some(old_session.clone());
                runtime.queue = queued.clone();
                runtime.queue_owner = Some("old-user".into());
            }

            service.reconcile_queue_owner("new-user").await.unwrap();

            let runtime = service.runtime.lock().await;
            assert!(runtime.queue.is_empty());
            assert!(runtime
                .session
                .as_ref()
                .is_some_and(|session| session == &old_session));
            drop(runtime);
            assert!(service.queue_store.load().unwrap().is_empty());
        });
    }

    #[test]
    fn different_user_reconnect_keeps_old_runtime_state_when_queue_clear_fails() {
        tauri::async_runtime::block_on(async {
            let directory = tempfile::tempdir().unwrap();
            fs::create_dir(directory.path().join("lastfm-scrobbles.json")).unwrap();
            let service = Service::new_for_test(directory.path(), true, true);
            let mut queued = VecDeque::from([queued_scrobble(1)]);
            queued.front_mut().unwrap().owner = "old-user".into();
            let old_session = LastFmSession {
                username: "old-user".into(),
                key: "old-session".into(),
            };
            {
                let mut runtime = service.runtime.lock().await;
                runtime.session = Some(old_session.clone());
                runtime.queue = queued.clone();
                runtime.queue_owner = Some("old-user".into());
            }

            assert!(service.reconcile_queue_owner("new-user").await.is_err());
            let runtime = service.runtime.lock().await;
            assert_eq!(runtime.queue, queued);
            assert!(runtime
                .session
                .as_ref()
                .is_some_and(|session| session == &old_session));
        });
    }

    #[test]
    fn api_nine_failure_preserves_queue_and_requires_reconnect() {
        tauri::async_runtime::block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let service = Service::new_for_test(directory.path(), true, true);
            let session = LastFmSession {
                username: "user".into(),
                key: "session".into(),
            };
            let queued = VecDeque::from([queued_scrobble(1)]);
            service.session_store.save(&session).unwrap();
            service.queue_store.save(&queued).unwrap();
            {
                let mut runtime = service.runtime.lock().await;
                runtime.session = Some(session.clone());
                runtime.queue = queued.clone();
                runtime.queue_owner = Some("user".into());
            }

            let message = service.handle_failure(Failure::Api(9)).await;

            assert!(message.contains("expired"));
            let runtime = service.runtime.lock().await;
            assert!(runtime.session.is_none());
            assert!(runtime.reconnect_required);
            assert_eq!(runtime.queue, queued);
            assert!(service.session_store.load().unwrap().is_none());
            assert_eq!(service.queue_store.load().unwrap(), queued);
        });
    }

    #[test]
    fn build_failure_stops_flush_without_dropping_session_or_queue() {
        tauri::async_runtime::block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let mut service = Service::new_for_test(directory.path(), true, true);
            Arc::get_mut(&mut service).unwrap().credentials = Some(Credentials {
                api_key: "test-key".into(),
                shared_secret: "test-secret".into(),
            });
            let session = LastFmSession {
                username: "user".into(),
                key: "session".into(),
            };
            let queued = VecDeque::from([queued_scrobble(1)]);
            {
                let mut runtime = service.runtime.lock().await;
                runtime.session = Some(session.clone());
                runtime.queue = queued.clone();
                runtime.queue_owner = Some("user".into());
            }

            let message = service.handle_failure(Failure::Api(10)).await;

            assert!(message.contains("app identity"));
            let runtime = service.runtime.lock().await;
            assert!(runtime.build_problem);
            assert!(runtime
                .session
                .as_ref()
                .is_some_and(|value| value == &session));
            assert_eq!(runtime.queue, queued);
            assert!(!flush_ready(&runtime));
            drop(runtime);
            let state = service.state().await;
            assert!(!state.available);
            assert_eq!(
                state.problem.as_deref(),
                Some("This Retune build cannot use Last.fm because its app identity was rejected.")
            );
        });
    }

    #[test]
    fn disconnect_keeps_session_and_queue_when_durable_clear_fails() {
        tauri::async_runtime::block_on(async {
            let directory = tempfile::tempdir().unwrap();
            fs::create_dir(directory.path().join("lastfm-scrobbles.json")).unwrap();
            let service = Service::new_for_test(directory.path(), true, true);
            let session = LastFmSession {
                username: "old-user".into(),
                key: "old-session".into(),
            };
            service.session_store.save(&session).unwrap();
            let queued = VecDeque::from([queued_scrobble(1)]);
            {
                let mut runtime = service.runtime.lock().await;
                runtime.session = Some(session.clone());
                runtime.pending = Some("pending".into());
                runtime.queue = queued.clone();
            }

            assert!(service.disconnect().await.is_err());
            let runtime = service.runtime.lock().await;
            assert!(runtime
                .session
                .as_ref()
                .is_some_and(|value| value == &session));
            assert_eq!(runtime.pending.as_deref(), Some("pending"));
            assert_eq!(runtime.queue, queued);
            assert!(service
                .session_store
                .load()
                .unwrap()
                .as_ref()
                .is_some_and(|value| value == &session));
        });
    }

    #[test]
    fn disconnect_restores_ledger_when_credential_clear_fails() {
        tauri::async_runtime::block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let mut service = Service::new_for_test(directory.path(), true, true);
            Arc::get_mut(&mut service).unwrap().session_store = Arc::new(FailingClearSessionStore);
            let session = LastFmSession {
                username: "old-user".into(),
                key: "old-session".into(),
            };
            let queued = VecDeque::from([queued_scrobble(1)]);
            let receipt = AcceptedScrobbleReceipt {
                corrected: ScrobbleMetadata {
                    artist: "Artist".into(),
                    album: "Album".into(),
                    track: "Song".into(),
                },
                submitted: ScrobbleMetadata {
                    artist: "Artist".into(),
                    album: "Album".into(),
                    track: "Song".into(),
                },
                timestamp: 1,
            };
            {
                let mut runtime = service.runtime.lock().await;
                runtime.session = Some(session.clone());
                runtime.queue = queued.clone();
                runtime.queue_owner = Some("old-user".into());
            }
            *service.accepted_receipts.lock().await = vec![receipt.clone()];

            assert!(service.disconnect().await.is_err());
            let (ledger, _) = service.queue_store.load_ledger_with_migration().unwrap();
            assert_eq!(ledger.pending, queued);
            assert_eq!(ledger.accepted, vec![receipt.clone()]);
            assert_eq!(ledger.owner.as_deref(), Some("old-user"));
            let runtime = service.runtime.lock().await;
            assert!(runtime
                .session
                .as_ref()
                .is_some_and(|value| value == &session));
            assert_eq!(runtime.queue, queued);
            assert_eq!(runtime.queue_owner.as_deref(), Some("old-user"));
            drop(runtime);
            assert_eq!(service.accepted_receipts().await, vec![receipt]);
        });
    }

    #[test]
    fn disconnect_marks_storage_unavailable_when_ledger_rollback_fails() {
        tauri::async_runtime::block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let mut service = Service::new_for_test(directory.path(), true, true);
            let ledger_path = service.queue_store.path.clone();
            Arc::get_mut(&mut service).unwrap().session_store =
                Arc::new(FailingClearAndRollbackSessionStore { ledger_path });
            let session = LastFmSession {
                username: "old-user".into(),
                key: "old-session".into(),
            };
            {
                let mut runtime = service.runtime.lock().await;
                runtime.session = Some(session.clone());
                runtime.queue = VecDeque::from([queued_scrobble(1)]);
                runtime.queue_owner = Some("old-user".into());
            }

            assert!(service.disconnect().await.is_err());

            let runtime = service.runtime.lock().await;
            assert!(runtime
                .session
                .as_ref()
                .is_some_and(|value| value == &session));
            assert!(runtime.queue.is_empty());
            assert!(runtime.storage_problem);
        });
    }

    #[test]
    fn cross_account_completion_commits_new_session_when_pending_clear_fails() {
        tauri::async_runtime::block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let service = Service::new_for_test(directory.path(), true, true);
            let old_session = LastFmSession {
                username: "old-user".into(),
                key: "old-session".into(),
            };
            let new_session = LastFmSession {
                username: "new-user".into(),
                key: "new-session".into(),
            };
            let mut queued = VecDeque::from([queued_scrobble(1)]);
            queued.front_mut().unwrap().owner = "old-user".into();
            service.session_store.save(&old_session).unwrap();
            service.queue_store.save(&queued).unwrap();
            fs::create_dir(&service.pending_store.path).unwrap();
            {
                let mut runtime = service.runtime.lock().await;
                runtime.session = Some(old_session.clone());
                runtime.pending = Some("pending-token".into());
                runtime.queue = queued.clone();
                runtime.queue_owner = Some("old-user".into());
            }

            service.commit_session(new_session.clone()).await.unwrap();
            assert!(service
                .session_store
                .load()
                .unwrap()
                .as_ref()
                .is_some_and(|session| session == &new_session));
            let runtime = service.runtime.lock().await;
            assert!(runtime
                .session
                .as_ref()
                .is_some_and(|session| session == &new_session));
            assert!(runtime.pending.is_none());
            assert!(runtime.queue.is_empty());
            drop(runtime);
            assert!(service.queue_store.load().unwrap().is_empty());
        });
    }

    #[test]
    fn cross_account_completion_keeps_retry_state_when_session_save_fails() {
        tauri::async_runtime::block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let mut service = Service::new_for_test(directory.path(), true, true);
            let old_session = LastFmSession {
                username: "old-user".into(),
                key: "old-session".into(),
            };
            let new_session = LastFmSession {
                username: "new-user".into(),
                key: "new-session".into(),
            };
            let mut queued = VecDeque::from([queued_scrobble(1)]);
            queued.front_mut().unwrap().owner = "old-user".into();
            Arc::get_mut(&mut service).unwrap().session_store = Arc::new(FailingSaveSessionStore {
                session: old_session.clone(),
            });
            service.pending_store.save("pending-token").unwrap();
            service.queue_store.save(&queued).unwrap();
            {
                let mut runtime = service.runtime.lock().await;
                runtime.session = Some(old_session.clone());
                runtime.pending = Some("pending-token".into());
                runtime.queue = queued.clone();
                runtime.queue_owner = Some("old-user".into());
            }

            assert!(service.commit_session(new_session).await.is_err());
            assert!(service
                .session_store
                .load()
                .unwrap()
                .as_ref()
                .is_some_and(|session| session == &old_session));
            assert_eq!(
                service.pending_store.load().unwrap().as_deref(),
                Some("pending-token")
            );
            assert_eq!(service.queue_store.load().unwrap(), queued);
            let runtime = service.runtime.lock().await;
            assert!(runtime
                .session
                .as_ref()
                .is_some_and(|session| session == &old_session));
            assert_eq!(runtime.pending.as_deref(), Some("pending-token"));
            assert_eq!(runtime.queue, queued);
        });
    }

    #[test]
    fn json_responses_support_last_fm_root_and_legacy_lfm_envelopes() {
        let response = serde_json::json!({
            "session": {"name": "user", "key": "session"}
        });
        assert_eq!(
            response_text(&response, &["session", "name"]).as_deref(),
            Some("user")
        );
        assert_eq!(error_code(&serde_json::json!({"error": 9})), Some(9));
        assert_eq!(
            scrobble_codes(
                &serde_json::json!({
                    "scrobbles": {"scrobble": {"ignoredMessage": {"code": 0}}}
                }),
                1
            ),
            Some(vec![0])
        );
    }

    #[test]
    fn retry_schedule_is_ordered_and_bounded() {
        assert_eq!(retry_delay(0), Duration::from_secs(1));
        assert_eq!(retry_delay(1), Duration::from_secs(5));
        assert_eq!(retry_delay(2), Duration::from_secs(15));
        assert_eq!(retry_delay(3), Duration::from_secs(60));
        assert_eq!(retry_delay(4), Duration::from_secs(300));
        assert!(retry_delay(0) < retry_delay(1));
        assert!(retry_delay(1) < retry_delay(2));
        assert_eq!(retry_delay(100), retry_delay(4));
    }

    #[test]
    fn failure_classification_does_not_return_sensitive_details() {
        assert!(is_build_failure(Failure::Api(10)));
        assert!(is_retryable(Failure::Api(16)));
        assert!(is_retryable(Failure::Response));
        let failure = Failure::Api(13);
        assert_eq!(failure.code(), Some(13));
    }
}

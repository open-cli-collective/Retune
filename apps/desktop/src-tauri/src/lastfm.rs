use std::{
    collections::VecDeque,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use md5::{Digest, Md5};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{Emitter, Manager};
use tauri_plugin_opener::OpenerExt;
use tokio::sync::Mutex;
use url::Url;

const API_URL: &str = "https://ws.audioscrobbler.com/2.0/";
const AUTH_URL: &str = "https://www.last.fm/api/auth";
const USER_AGENT: &str = concat!("Retune/", env!("CARGO_PKG_VERSION"));
#[cfg(not(test))]
pub(crate) const CREDENTIAL_SERVICE: &str = "com.rianjs.retune";
#[cfg(not(test))]
pub(crate) const SESSION_ACCOUNT: &str = "lastfm-session";
const RETRY_DELAYS: &[Duration] = &[
    Duration::from_secs(1),
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(60),
    Duration::from_secs(300),
];

#[derive(Clone)]
struct Credentials {
    api_key: String,
    shared_secret: String,
}

fn credentials_from(api_key: Option<&str>, shared_secret: Option<&str>) -> Option<Credentials> {
    let api_key = api_key?.trim();
    let shared_secret = shared_secret?.trim();
    (!api_key.is_empty() && !shared_secret.is_empty()).then(|| Credentials {
        api_key: api_key.into(),
        shared_secret: shared_secret.into(),
    })
}

fn built_in_credentials() -> Option<Credentials> {
    credentials_from(
        option_env!("RETUNE_LASTFM_API_KEY"),
        option_env!("RETUNE_LASTFM_SHARED_SECRET"),
    )
}

#[derive(Clone, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct LastFmSession {
    username: String,
    key: String,
}

trait SessionStore: Send + Sync {
    fn load(&self) -> Result<Option<LastFmSession>, String>;
    fn save(&self, session: &LastFmSession) -> Result<(), String>;
    fn clear(&self) -> Result<(), String>;
}

struct FileSessionStore {
    path: PathBuf,
}

impl FileSessionStore {
    fn new(app_data_dir: impl AsRef<Path>) -> Self {
        Self {
            path: app_data_dir.as_ref().join("dev-lastfm-session.json"),
        }
    }
}

impl SessionStore for FileSessionStore {
    fn load(&self) -> Result<Option<LastFmSession>, String> {
        read_json(&self.path)
    }

    fn save(&self, session: &LastFmSession) -> Result<(), String> {
        write_secret_json(&self.path, session)
    }

    fn clear(&self) -> Result<(), String> {
        remove_file(&self.path)
    }
}

#[cfg(not(test))]
struct KeyringSessionStore {
    entry: keyring::Entry,
}

#[cfg(not(test))]
impl KeyringSessionStore {
    fn new() -> Result<Self, String> {
        keyring::Entry::new(CREDENTIAL_SERVICE, SESSION_ACCOUNT)
            .map(|entry| Self { entry })
            .map_err(|_error| "Last.fm credential storage is unavailable.".to_string())
    }
}

#[cfg(not(test))]
impl SessionStore for KeyringSessionStore {
    fn load(&self) -> Result<Option<LastFmSession>, String> {
        match self.entry.get_password() {
            Ok(value) => serde_json::from_str(&value)
                .map(Some)
                .map_err(|_| "Stored Last.fm session is invalid.".into()),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err("Last.fm credential storage is unavailable.".into()),
        }
    }

    fn save(&self, session: &LastFmSession) -> Result<(), String> {
        let value = serde_json::to_string(session)
            .map_err(|_| "Could not save the Last.fm session.".to_string())?;
        self.entry
            .set_password(&value)
            .map_err(|_| "Could not save the Last.fm session.".into())
    }

    fn clear(&self) -> Result<(), String> {
        match self.entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err("Could not remove the Last.fm session.".into()),
        }
    }
}

#[derive(Clone)]
struct PendingTokenStore {
    path: PathBuf,
}

impl PendingTokenStore {
    fn new(app_data_dir: impl AsRef<Path>) -> Self {
        Self {
            path: app_data_dir.as_ref().join("lastfm-pending-token.json"),
        }
    }

    fn load(&self) -> Result<Option<String>, String> {
        read_json(&self.path)
    }

    fn save(&self, token: &str) -> Result<(), String> {
        write_secret_json(&self.path, &token)
    }

    fn clear(&self) -> Result<(), String> {
        remove_file(&self.path)
    }
}

#[cfg(test)]
struct SaveBlocker {
    entered: std::sync::mpsc::Sender<()>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

#[derive(Clone)]
struct QueueStore {
    path: PathBuf,
    #[cfg(test)]
    blocker: Option<Arc<SaveBlocker>>,
}

impl QueueStore {
    fn new(app_data_dir: impl AsRef<Path>) -> Self {
        Self {
            path: app_data_dir.as_ref().join("lastfm-scrobbles.json"),
            #[cfg(test)]
            blocker: None,
        }
    }

    fn load(&self) -> Result<VecDeque<Scrobble>, String> {
        Ok(read_json(&self.path)?.unwrap_or_default())
    }

    fn save(&self, queue: &VecDeque<Scrobble>) -> Result<(), String> {
        #[cfg(test)]
        if let Some(blocker) = &self.blocker {
            let _ = blocker.entered.send(());
            blocker
                .release
                .lock()
                .expect("queue persistence blocker mutex is not poisoned")
                .recv()
                .expect("queue persistence blocker release is sent");
        }
        let bytes = serde_json::to_vec(queue)
            .map_err(|_| "Could not serialize the Last.fm scrobble queue.".to_string())?;
        atomic_write(&self.path, &bytes, false)
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, String> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| format!("Could not read {}.", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(format!("Could not read {}.", path.display())),
    }
}

fn write_secret_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let bytes = serde_json::to_vec(value)
        .map_err(|_| "Could not serialize a Last.fm credential.".to_string())?;
    atomic_write(path, &bytes, true)
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8], secret: bool) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Last.fm store path has no parent.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|_| "Could not create the Last.fm store directory.".to_string())?;
    let temporary = path.with_extension(format!("tmp-{}", rand::random::<u64>()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if secret {
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|_| "Could not open the Last.fm temporary store.".to_string())?;
        #[cfg(unix)]
        if secret {
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|_| "Could not protect the Last.fm temporary store.".to_string())?;
        }
        file.write_all(bytes)
            .map_err(|_| "Could not write the Last.fm store.".to_string())?;
        file.sync_all()
            .map_err(|_| "Could not sync the Last.fm store.".to_string())?;
        fs::rename(&temporary, path).map_err(|_| "Could not replace the Last.fm store.".to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    #[cfg(unix)]
    if secret {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|_| "Could not protect the Last.fm store.".to_string())?;
    }
    Ok(())
}

fn remove_file(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("Could not remove a Last.fm local store.".into()),
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Scrobble {
    artist: String,
    track: String,
    album: Option<String>,
    duration_secs: u64,
    timestamp: u64,
    #[serde(default)]
    owner: String,
}

impl Scrobble {
    fn from_track(track: &super::playback::SnapshotTrack, timestamp: u64) -> Option<Self> {
        let artist = track.art.trim();
        let title = track.name.trim();
        if artist.is_empty() || title.is_empty() || artist.eq_ignore_ascii_case("unknown artist") {
            return None;
        }
        let album = (!track.alb.trim().is_empty()
            && !track.alb.trim().eq_ignore_ascii_case("unknown album"))
        .then(|| track.alb.trim().to_owned());
        Some(Self {
            artist: artist.into(),
            track: title.into(),
            album,
            duration_secs: track.duration_secs,
            timestamp,
            owner: String::new(),
        })
    }

    fn now_playing_params(&self) -> Vec<(String, String)> {
        let mut params = vec![
            ("artist".into(), self.artist.clone()),
            ("track".into(), self.track.clone()),
        ];
        if let Some(album) = &self.album {
            params.push(("album".into(), album.clone()));
        }
        if self.duration_secs > 0 {
            params.push(("duration".into(), self.duration_secs.to_string()));
        }
        params
    }

    fn scrobble_params(&self, index: usize) -> Vec<(String, String)> {
        let mut params = vec![
            (format!("artist[{index}]"), self.artist.clone()),
            (format!("track[{index}]"), self.track.clone()),
            (format!("timestamp[{index}]"), self.timestamp.to_string()),
        ];
        if let Some(album) = &self.album {
            params.push((format!("album[{index}]"), album.clone()));
        }
        if self.duration_secs > 0 {
            params.push((format!("duration[{index}]"), self.duration_secs.to_string()));
        }
        params
    }
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    Network,
    Http(u16),
    Api(u32),
    Response,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImportFetchError {
    pub message: String,
    pub retryable: bool,
    pub account_mismatch: bool,
}

impl Failure {
    fn code(self) -> Option<u32> {
        match self {
            Self::Api(code) => Some(code),
            Self::Network | Self::Http(_) | Self::Response => None,
        }
    }
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

#[derive(Default)]
struct ListeningState {
    generation: Option<u64>,
    track: Option<super::playback::SnapshotTrack>,
    started_at: u64,
    played_ms: u64,
    discontinuous: bool,
    scrobbled: bool,
}

enum ListeningAction {
    NowPlaying(Scrobble),
    Enqueue(Scrobble),
}

impl ListeningState {
    fn apply(
        &mut self,
        fact: super::playback::ListeningFact,
        started_at: u64,
    ) -> Vec<ListeningAction> {
        match fact {
            super::playback::ListeningFact::Started { generation, track } => {
                self.generation = Some(generation);
                self.track = Some(track.clone());
                self.started_at = started_at;
                self.played_ms = 0;
                self.discontinuous = false;
                self.scrobbled = false;
                Scrobble::from_track(&track, 0)
                    .map(ListeningAction::NowPlaying)
                    .into_iter()
                    .collect()
            }
            super::playback::ListeningFact::Forward {
                generation,
                track,
                played_ms,
            } => {
                if !self.matches(generation, &track) {
                    return Vec::new();
                }
                self.played_ms = self.played_ms.max(played_ms);
                self.scrobble_if_eligible(&track, false)
            }
            super::playback::ListeningFact::Discontinuity { generation, track } => {
                if self.matches(generation, &track) {
                    self.discontinuous = true;
                }
                Vec::new()
            }
            super::playback::ListeningFact::Completed { generation, track } => {
                if !self.matches(generation, &track) {
                    return Vec::new();
                }
                self.scrobble_if_eligible(&track, true)
            }
        }
    }

    fn matches(&self, generation: u64, track: &super::playback::SnapshotTrack) -> bool {
        self.generation == Some(generation)
            && self
                .track
                .as_ref()
                .is_some_and(|current| current.uri == track.uri)
    }

    fn scrobble_if_eligible(
        &mut self,
        track: &super::playback::SnapshotTrack,
        completed: bool,
    ) -> Vec<ListeningAction> {
        let threshold = self
            .track
            .as_ref()
            .and_then(|track| scrobble_threshold_ms(track.duration_secs));
        let eligible = !self.scrobbled
            && !self.discontinuous
            && (threshold.is_some_and(|threshold| self.played_ms >= threshold)
                || (completed && threshold.is_some()));
        if !eligible {
            return Vec::new();
        }
        self.scrobbled = true;
        Scrobble::from_track(track, self.started_at)
            .map(ListeningAction::Enqueue)
            .into_iter()
            .collect()
    }
}

pub(crate) struct Service {
    credentials: Option<Credentials>,
    client: Client,
    session_store: Arc<dyn SessionStore>,
    pending_store: PendingTokenStore,
    queue_store: QueueStore,
    runtime: Mutex<Runtime>,
    queue_io: Mutex<()>,
    credential_io: Mutex<()>,
    listening: std::sync::Mutex<ListeningState>,
    app: std::sync::Mutex<Option<tauri::AppHandle>>,
}

impl Service {
    pub(crate) fn new(app_data_dir: impl AsRef<Path>, dev_store: bool, enabled: bool) -> Arc<Self> {
        let credentials = built_in_credentials();
        let (session_store, mut storage_problem): (Arc<dyn SessionStore>, bool) = if dev_store {
            (Arc::new(FileSessionStore::new(&app_data_dir)), false)
        } else {
            #[cfg(not(test))]
            {
                match KeyringSessionStore::new() {
                    Ok(store) => (Arc::new(store), false),
                    Err(_) => (Arc::new(FailedSessionStore), true),
                }
            }
            #[cfg(test)]
            {
                (Arc::new(FileSessionStore::new(&app_data_dir)), false)
            }
        };
        let pending_store = PendingTokenStore::new(&app_data_dir);
        let queue_store = QueueStore::new(&app_data_dir);
        let session = if credentials.is_some() {
            match session_store.load() {
                Ok(session) => session.filter(valid_session),
                Err(error) => {
                    storage_problem = true;
                    log::error!(
                        "Last.fm local persistence failed while loading the session: {error}"
                    );
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
        let mut queue = match queue_store.load() {
            Ok(queue) => queue,
            Err(error) => {
                log::error!("Last.fm local persistence failed; queued scrobbles may be unavailable: {error}");
                VecDeque::new()
            }
        };
        let mut queue_owner = queue_owner(&queue).map(ToOwned::to_owned);
        if let Some(session) = session.as_ref() {
            if !queue.is_empty() && queue_owner.as_deref() != Some(session.username.as_str()) {
                match queue_store.save(&VecDeque::new()) {
                    Ok(()) => {
                        queue.clear();
                        queue_owner = None;
                    }
                    Err(error) => {
                        storage_problem = true;
                        log::error!(
                            "Last.fm local persistence failed while isolating queued account state: {error}"
                        );
                    }
                }
            }
        }
        Arc::new(Self {
            credentials,
            client: Client::builder()
                .timeout(Duration::from_secs(20))
                .user_agent(USER_AGENT)
                .build()
                .expect("Last.fm HTTP client configuration is valid"),
            session_store,
            pending_store,
            queue_store,
            runtime: Mutex::new(Runtime {
                enabled,
                session,
                pending,
                queue,
                queue_owner,
                reconnect_required: false,
                build_problem: false,
                storage_problem,
                flushing: false,
                queue_revision: 0,
            }),
            queue_io: Mutex::new(()),
            credential_io: Mutex::new(()),
            listening: std::sync::Mutex::new(ListeningState::default()),
            app: std::sync::Mutex::new(None),
        })
    }

    pub(crate) fn attach_app(&self, app: tauri::AppHandle) {
        *self.app.lock().expect("Last.fm app mutex poisoned") = Some(app);
    }

    async fn persist_queue(&self, queue: VecDeque<Scrobble>) -> Result<(), String> {
        let store = self.queue_store.clone();
        tauri::async_runtime::spawn_blocking(move || store.save(&queue))
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

    pub(crate) async fn set_enabled(self: &Arc<Self>, enabled: bool) {
        let should_flush = {
            let mut runtime = self.runtime.lock().await;
            runtime.enabled = enabled;
            flush_ready(&runtime)
        };
        if should_flush {
            Arc::clone(self).schedule_flush().await;
        }
    }

    pub(crate) async fn connect(
        self: &Arc<Self>,
        app: &tauri::AppHandle,
    ) -> Result<LastFmState, String> {
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
        app.opener()
            .open_url(url.to_string(), None::<String>)
            .map_err(|_| "Could not open Last.fm in the system browser.")?;
        Ok(self.state().await)
    }

    pub(crate) async fn finish(
        self: &Arc<Self>,
        _app: &tauri::AppHandle,
    ) -> Result<LastFmState, String> {
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
        let _credential_io = self.credential_io.lock().await;
        self.commit_session(session).await?;
        log::info!("Last.fm connected");
        self.emit_state().await;
        Arc::clone(self).schedule_flush().await;
        Ok(self.state().await)
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
        if let Err(error) = self.clear_pending().await {
            log::error!(
                "Last.fm local persistence failed while clearing completed authorization: {error}"
            );
        }
        Ok(())
    }

    pub(crate) async fn disconnect(&self) -> Result<LastFmState, String> {
        let empty = VecDeque::new();
        let _credential_io = self.credential_io.lock().await;
        {
            let _queue_io = self.queue_io.lock().await;
            let (previous, revision) = {
                let mut runtime = self.runtime.lock().await;
                let previous = runtime.queue.clone();
                runtime.queue.clear();
                runtime.queue_owner = None;
                runtime.queue_revision = runtime.queue_revision.wrapping_add(1);
                (previous, runtime.queue_revision)
            };
            if let Err(error) = self.persist_queue(empty.clone()).await {
                let mut runtime = self.runtime.lock().await;
                if runtime.queue_revision == revision {
                    runtime.queue = previous;
                    runtime.queue_owner = queue_owner(&runtime.queue).map(ToOwned::to_owned);
                    runtime.queue_revision = runtime.queue_revision.wrapping_add(1);
                }
                log::error!(
                    "Last.fm local persistence failed while disconnecting; session retained"
                );
                drop(runtime);
                self.emit_state().await;
                return Err(error);
            }
        }

        let session_error = self.clear_session().await.err();
        let pending_error = self.clear_pending().await.err();
        let mut runtime = self.runtime.lock().await;
        if session_error.is_none() {
            runtime.session = None;
            runtime.reconnect_required = false;
            runtime.build_problem = false;
        }
        if pending_error.is_none() {
            runtime.pending = None;
        }
        let errors = session_error
            .into_iter()
            .chain(pending_error)
            .collect::<Vec<_>>();
        drop(runtime);
        if !errors.is_empty() {
            log::error!("Last.fm local persistence failed while disconnecting");
            self.emit_state().await;
            return Err(errors.join(" "));
        }
        log::info!("Last.fm disconnected");
        self.emit_state().await;
        Ok(self.state().await)
    }

    pub(crate) fn handle_listening_fact(self: &Arc<Self>, fact: super::playback::ListeningFact) {
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
                    let service = Arc::clone(self);
                    tauri::async_runtime::spawn(async move {
                        log::debug!("Last.fm scrobble eligible");
                        service.enqueue(scrobble).await;
                    });
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
        &self,
        username: &str,
        page: u32,
        to: u64,
    ) -> Result<Value, ImportFetchError> {
        if let Err(message) = self.ensure_available().await {
            return Err(ImportFetchError {
                message,
                retryable: false,
                account_mismatch: false,
            });
        }
        let connected_username = self
            .runtime
            .lock()
            .await
            .session
            .as_ref()
            .map(|session| session.username.clone());
        if connected_username.as_deref() != Some(username) {
            return Err(ImportFetchError {
                message:
                    "The connected Last.fm account changed; resume the importer after reconnecting."
                        .into(),
                retryable: false,
                account_mismatch: true,
            });
        }
        let params = vec![
            ("user".into(), username.into()),
            ("page".into(), page.to_string()),
            (
                "limit".into(),
                crate::lastfm_import::LASTFM_PAGE_LIMIT.to_string(),
            ),
            ("to".into(), to.to_string()),
        ];
        for attempt in 0..=RETRY_DELAYS.len() {
            match self
                .post("user.getRecentTracks", params.clone(), None)
                .await
            {
                Ok(value) => return Ok(value),
                Err(failure) if is_retryable(failure) && attempt < RETRY_DELAYS.len() => {
                    tokio::time::sleep(retry_delay(attempt)).await;
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

    async fn send_now_playing(&self, scrobble: Scrobble) {
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
        let service = Arc::clone(&self);
        tauri::async_runtime::spawn(async move { service.flush_loop().await });
    }

    async fn flush_loop(self: Arc<Self>) {
        let mut attempt = 0;
        loop {
            let mut stopped = false;
            loop {
                match self.flush_once().await {
                    FlushOutcome::Done => break,
                    FlushOutcome::Continue => {
                        attempt = 0;
                        continue;
                    }
                    FlushOutcome::Retry => {
                        let ready = {
                            let runtime = self.runtime.lock().await;
                            flush_ready(&runtime)
                        };
                        if !ready {
                            break;
                        }
                        let delay = retry_delay(attempt);
                        attempt = attempt.saturating_add(1);
                        log::debug!("Last.fm retry scheduled in {}s", delay.as_secs());
                        tokio::time::sleep(delay).await;
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
                flush_ready(&runtime)
            };
            if !should_restart || stopped {
                break;
            }
            let claimed_restart = {
                let mut runtime = self.runtime.lock().await;
                if flush_ready(&runtime) && !runtime.flushing {
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
    }

    async fn flush_once(&self) -> FlushOutcome {
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
                let Some(codes) = scrobble_codes(&payload, count) else {
                    log::warn!(
                        "Last.fm returned an unusable scrobble response; preserving the queue"
                    );
                    return FlushOutcome::Retry;
                };
                let _queue_io = self.queue_io.lock().await;
                let (original, queue, removed, revision) = {
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
                    )
                };
                if let Err(error) = self.persist_queue(queue.clone()).await {
                    let mut runtime = self.runtime.lock().await;
                    if runtime.queue_revision == revision {
                        runtime.queue = original;
                        runtime.queue_revision = runtime.queue_revision.wrapping_add(1);
                    }
                    log::error!("Last.fm local persistence failed after scrobble response; queue retained: {error}");
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
                    log::error!("Last.fm local persistence failed after permanent rejection; queue retained: {save_error}");
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

    async fn invalidate_session(&self, expected: &LastFmSession) -> bool {
        let _credential_io = self.credential_io.lock().await;
        let expected = expected.clone();
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
        if let Some(error) = clear_error {
            log::error!(
                "Last.fm local persistence failed while clearing an invalid session: {error}"
            );
        }
        true
    }

    async fn post(
        &self,
        method: &str,
        mut params: Vec<(String, String)>,
        session_key: Option<&str>,
    ) -> Result<Value, Failure> {
        let credentials = self.credentials.as_ref().ok_or(Failure::Api(10))?;
        params.push(("api_key".into(), credentials.api_key.clone()));
        params.push(("method".into(), method.into()));
        if let Some(session_key) = session_key {
            params.push(("sk".into(), session_key.into()));
        }
        params.push(("format".into(), "json".into()));
        let api_sig = signature(&params, &credentials.shared_secret);
        params.push(("api_sig".into(), api_sig));
        let response = self
            .client
            .post(API_URL)
            .form(&params)
            .send()
            .await
            .map_err(|_| Failure::Network)?;
        let status = response.status().as_u16();
        let body = response.bytes().await.map_err(|_| Failure::Network)?;
        let value: Value = match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(_) if !(200..300).contains(&status) => return Err(Failure::Http(status)),
            Err(_) => return Err(Failure::Response),
        };
        if let Some(code) = error_code(&value) {
            return Err(Failure::Api(code));
        }
        if !(200..300).contains(&status) {
            return Err(Failure::Http(status));
        }
        if let Some(status) = response_text(&value, &["status"])
            .or_else(|| response_text(&value, &["@status"]))
            .or_else(|| response_text(&value, &["@attr", "status"]))
        {
            if status != "ok" {
                return Err(Failure::Response);
            }
        }
        Ok(value)
    }

    async fn handle_failure(&self, failure: Failure) -> String {
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
        let _queue_io = self.queue_io.lock().await;
        let (revision, should_clear) = {
            let runtime = self.runtime.lock().await;
            (
                runtime.queue_revision,
                !runtime.queue.is_empty() && queue_owner(&runtime.queue) != Some(username),
            )
        };
        if should_clear {
            self.persist_queue(VecDeque::new()).await?;
            let mut runtime = self.runtime.lock().await;
            if runtime.queue_revision == revision {
                runtime.queue.clear();
                runtime.queue_owner = None;
                runtime.queue_revision = runtime.queue_revision.wrapping_add(1);
            }
        }
        self.runtime.lock().await.queue_owner = Some(username.to_owned());
        Ok(())
    }

    async fn emit_state(&self) {
        let app = self.app.lock().expect("Last.fm app mutex poisoned").clone();
        if let Some(app) = app {
            let _ = app.emit("lastfm-changed", self.state().await);
        }
    }
}

#[cfg(not(test))]
struct FailedSessionStore;

#[cfg(not(test))]
impl SessionStore for FailedSessionStore {
    fn load(&self) -> Result<Option<LastFmSession>, String> {
        Err("Last.fm credential storage is unavailable.".into())
    }

    fn save(&self, _session: &LastFmSession) -> Result<(), String> {
        Err("Last.fm credential storage is unavailable.".into())
    }

    fn clear(&self) -> Result<(), String> {
        Err("Last.fm credential storage is unavailable.".into())
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

fn queue_owner(queue: &VecDeque<Scrobble>) -> Option<&str> {
    let owner = queue.front()?.owner.as_str();
    (!owner.is_empty() && queue.iter().all(|item| item.owner.as_str() == owner)).then_some(owner)
}

fn next_batch(queue: &VecDeque<Scrobble>) -> Vec<Scrobble> {
    queue.iter().take(50).cloned().collect()
}

fn queue_starts_with(queue: &VecDeque<Scrobble>, batch: &[Scrobble]) -> bool {
    queue.len() >= batch.len() && queue.iter().zip(batch).all(|(queued, item)| queued == item)
}

fn apply_scrobble_results(
    queue: &mut VecDeque<Scrobble>,
    batch: &[Scrobble],
    codes: &[u32],
) -> Vec<(Scrobble, u32)> {
    let mut removed = Vec::with_capacity(batch.len());
    for (item, code) in batch.iter().zip(codes) {
        if queue.pop_front().is_none() {
            break;
        }
        removed.push((item.clone(), *code));
    }
    removed
}

fn valid_session(session: &LastFmSession) -> bool {
    !session.username.trim().is_empty() && !session.key.trim().is_empty()
}

fn signature(params: &[(String, String)], shared_secret: &str) -> String {
    let mut values = params
        .iter()
        .filter(|(key, _)| key != "format" && key != "callback" && key != "api_sig")
        .collect::<Vec<_>>();
    values.sort_by(|left, right| left.0.cmp(&right.0));
    let mut input = String::new();
    for (key, value) in values {
        input.push_str(key);
        input.push_str(value);
    }
    input.push_str(shared_secret);
    let digest = Md5::digest(input.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn response_text(value: &Value, path: &[&str]) -> Option<String> {
    let mut current = value.get("lfm").unwrap_or(value);
    for key in path {
        current = current.get(*key)?;
    }
    current.as_str().map(ToOwned::to_owned).or_else(|| {
        current
            .get("#text")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    })
}

fn error_code(value: &Value) -> Option<u32> {
    let root = value.get("lfm").unwrap_or(value);
    let error = root.get("error")?;
    let code = error
        .get("code")
        .or_else(|| error.get("@code"))
        .unwrap_or(error);
    code.as_u64()
        .or_else(|| code.as_str()?.parse().ok())
        .map(|code| code as u32)
}

fn scrobble_codes(value: &Value, expected: usize) -> Option<Vec<u32>> {
    let root = value.get("lfm").unwrap_or(value);
    let scrobbles = root.get("scrobbles")?;
    let items = scrobbles.get("scrobble")?;
    let items = match items {
        Value::Array(items) => items.clone(),
        Value::Object(_) => vec![items.clone()],
        _ => return None,
    };
    if items.len() != expected {
        return None;
    }
    Some(
        items
            .into_iter()
            .map(|item| {
                item.get("ignoredMessage")
                    .and_then(|message| message.get("code").or_else(|| message.get("@code")))
                    .and_then(|code| code.as_u64().or_else(|| code.as_str()?.parse().ok()))
                    .unwrap_or(0) as u32
            })
            .collect(),
    )
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

pub(crate) fn scrobble_threshold_ms(duration_secs: u64) -> Option<u64> {
    (duration_secs > 30).then(|| (duration_secs.saturating_mul(500)).min(240_000))
}

#[tauri::command]
pub(crate) async fn lastfm_state(
    state: tauri::State<'_, crate::AppState>,
) -> Result<LastFmState, String> {
    Ok(state.lastfm.state().await)
}

#[tauri::command]
pub(crate) async fn connect_lastfm(app: tauri::AppHandle) -> Result<LastFmState, String> {
    app.state::<crate::AppState>().lastfm.connect(&app).await
}

#[tauri::command]
pub(crate) async fn finish_lastfm(app: tauri::AppHandle) -> Result<LastFmState, String> {
    let state = app.state::<crate::AppState>();
    let result = state.lastfm.finish(&app).await?;
    state.lastfm.set_enabled(true).await;
    crate::set_lastfm_scrobbling(&app, true)?;
    Ok(result)
}

#[tauri::command]
pub(crate) async fn disconnect_lastfm(app: tauri::AppHandle) -> Result<LastFmState, String> {
    app.state::<crate::AppState>().lastfm.disconnect().await
}

#[cfg(test)]
mod tests {
    use std::fs;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::playback::SnapshotTrack;

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
    fn empty_or_missing_built_in_credentials_disable_integration() {
        assert!(credentials_from(None, Some("secret")).is_none());
        assert!(credentials_from(Some(""), Some("secret")).is_none());
        assert!(credentials_from(Some("key"), Some(" ")).is_none());
        assert!(credentials_from(Some(" key "), Some(" secret ")).is_some());
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

    #[test]
    fn queue_persistence_does_not_hold_runtime_lock() {
        tauri::async_runtime::block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let mut service = Service::new(directory.path(), true, true);
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
            let service = Service::new(directory.path(), true, true);
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
            let service = Service::new(directory.path(), true, true);
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
            let service = Service::new(directory.path(), true, true);
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
            let service = Service::new(directory.path(), true, true);
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
            let service = Service::new(directory.path(), true, true);
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
            let mut service = Service::new(directory.path(), true, true);
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
            let service = Service::new(directory.path(), true, true);
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
    fn disconnect_keeps_session_when_credential_clear_fails_after_queue_clear() {
        tauri::async_runtime::block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let mut service = Service::new(directory.path(), true, true);
            Arc::get_mut(&mut service).unwrap().session_store = Arc::new(FailingClearSessionStore);
            let session = LastFmSession {
                username: "old-user".into(),
                key: "old-session".into(),
            };
            let queued = VecDeque::from([queued_scrobble(1)]);
            {
                let mut runtime = service.runtime.lock().await;
                runtime.session = Some(session.clone());
                runtime.queue = queued;
            }

            assert!(service.disconnect().await.is_err());
            assert!(service.queue_store.load().unwrap().is_empty());
            let runtime = service.runtime.lock().await;
            assert!(runtime
                .session
                .as_ref()
                .is_some_and(|value| value == &session));
            assert!(runtime.queue.is_empty());
        });
    }

    #[test]
    fn cross_account_completion_commits_new_session_when_pending_clear_fails() {
        tauri::async_runtime::block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let service = Service::new(directory.path(), true, true);
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
            let mut service = Service::new(directory.path(), true, true);
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

    #[test]
    fn log_messages_do_not_include_request_secrets() {
        for line in include_str!("lastfm.rs")
            .lines()
            .filter(|line| line.contains("log::"))
        {
            for secret in [
                "api_key",
                "shared_secret",
                "api_sig",
                "session.key",
                "token",
            ] {
                assert!(
                    !line.contains(secret),
                    "sensitive value in log line: {line}"
                );
            }
        }
    }
}

#[cfg(test)]
use std::sync::Arc;
use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{AcceptedScrobbleReceipt, Scrobble, CREDENTIAL_SERVICE, SESSION_ACCOUNT};

const CREDENTIAL_FILE_LIMIT: u64 = 64 * 1024;
const SCROBBLE_LEDGER_LIMIT: u64 = 64 * 1024 * 1024;
pub(super) const SCROBBLE_LEDGER_VERSION: u8 = 2;

#[derive(Clone, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LastFmSession {
    pub(super) username: String,
    pub(super) key: String,
}

pub(super) trait SessionStore: Send + Sync {
    fn load(&self) -> Result<Option<LastFmSession>, String>;
    fn save(&self, session: &LastFmSession) -> Result<(), String>;
    fn clear(&self) -> Result<(), String>;
}

pub(super) struct FileSessionStore {
    path: PathBuf,
}
impl FileSessionStore {
    pub(super) fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            path: dir.as_ref().join("dev-lastfm-session.json"),
        }
    }
}
impl SessionStore for FileSessionStore {
    fn load(&self) -> Result<Option<LastFmSession>, String> {
        read_json(&self.path, CREDENTIAL_FILE_LIMIT)
    }
    fn save(&self, session: &LastFmSession) -> Result<(), String> {
        write_secret_json(&self.path, session)
    }
    fn clear(&self) -> Result<(), String> {
        remove_file(&self.path)
    }
}

pub(super) struct KeyringSessionStore {
    entry: keyring::Entry,
}
impl KeyringSessionStore {
    pub(super) fn new() -> Result<Self, String> {
        keyring::Entry::new(CREDENTIAL_SERVICE, SESSION_ACCOUNT)
            .map(|entry| Self { entry })
            .map_err(|_| "Last.fm credential storage is unavailable.".to_string())
    }
}
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

pub(super) struct FailedSessionStore;
impl SessionStore for FailedSessionStore {
    fn load(&self) -> Result<Option<LastFmSession>, String> {
        Err("Last.fm credential storage is unavailable.".into())
    }
    fn save(&self, _: &LastFmSession) -> Result<(), String> {
        Err("Last.fm credential storage is unavailable.".into())
    }
    fn clear(&self) -> Result<(), String> {
        Err("Last.fm credential storage is unavailable.".into())
    }
}

#[derive(Clone)]
pub(super) struct PendingTokenStore {
    pub(super) path: PathBuf,
}
impl PendingTokenStore {
    pub(super) fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            path: dir.as_ref().join("lastfm-pending-token.json"),
        }
    }
    pub(super) fn load(&self) -> Result<Option<String>, String> {
        read_json(&self.path, CREDENTIAL_FILE_LIMIT)
    }
    pub(super) fn save(&self, token: &str) -> Result<(), String> {
        write_secret_json(&self.path, &token)
    }
    pub(super) fn clear(&self) -> Result<(), String> {
        remove_file(&self.path)
    }
}

#[cfg(test)]
pub(super) struct SaveBlocker {
    pub(super) entered: std::sync::mpsc::Sender<()>,
    pub(super) release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}
#[cfg(test)]
impl SaveBlocker {
    pub(super) fn pause(&self) {
        let _ = self.entered.send(());
        self.release
            .lock()
            .expect("persistence blocker mutex is not poisoned")
            .recv()
            .expect("persistence blocker release is sent");
    }
}

#[derive(Clone)]
pub(super) struct QueueStore {
    pub(super) path: PathBuf,
    #[cfg(test)]
    pub(super) blocker: Option<Arc<SaveBlocker>>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ScrobbleLedgerV2 {
    #[serde(default = "scrobble_ledger_version")]
    pub(super) version: u8,
    pub(super) pending: VecDeque<Scrobble>,
    #[serde(default)]
    pub(super) accepted: Vec<AcceptedScrobbleReceipt>,
    #[serde(default)]
    pub(super) owner: Option<String>,
}
fn scrobble_ledger_version() -> u8 {
    SCROBBLE_LEDGER_VERSION
}
impl ScrobbleLedgerV2 {
    pub(super) fn empty() -> Self {
        Self {
            version: SCROBBLE_LEDGER_VERSION,
            pending: VecDeque::new(),
            accepted: Vec::new(),
            owner: None,
        }
    }
}

impl QueueStore {
    pub(super) fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            path: dir.as_ref().join("lastfm-scrobbles.json"),
            #[cfg(test)]
            blocker: None,
        }
    }
    pub(super) fn load_ledger_with_migration(&self) -> Result<(ScrobbleLedgerV2, bool), String> {
        let Some(value) = read_json::<Value>(&self.path, SCROBBLE_LEDGER_LIMIT)? else {
            return Ok((ScrobbleLedgerV2::empty(), false));
        };
        let (ledger, migrated) = if value.is_array() {
            (
                ScrobbleLedgerV2 {
                    pending: serde_json::from_value(value)
                        .map_err(|_| "Could not read the Last.fm scrobble queue.".to_string())?,
                    ..ScrobbleLedgerV2::empty()
                },
                true,
            )
        } else if value.is_object() {
            (
                serde_json::from_value(value)
                    .map_err(|_| "Could not read the Last.fm scrobble ledger.".to_string())?,
                false,
            )
        } else {
            return Err("Could not read the Last.fm scrobble ledger.".into());
        };
        if ledger.version != SCROBBLE_LEDGER_VERSION {
            return Err("The Last.fm scrobble ledger version is unsupported.".into());
        }
        Ok((ledger, migrated))
    }
    #[cfg(test)]
    pub(super) fn load(&self) -> Result<VecDeque<Scrobble>, String> {
        Ok(self.load_ledger_with_migration()?.0.pending)
    }
    #[cfg(test)]
    pub(super) fn save(&self, queue: &VecDeque<Scrobble>) -> Result<(), String> {
        let mut ledger = ScrobbleLedgerV2::empty();
        ledger.pending = queue.clone();
        self.save_ledger(&ledger)
    }
    pub(super) fn save_ledger(&self, ledger: &ScrobbleLedgerV2) -> Result<(), String> {
        let bytes = serde_json::to_vec(ledger)
            .map_err(|_| "Could not serialize the Last.fm scrobble ledger.".to_string())?;
        crate::persistence::atomic_write(&self.path, &bytes, None)
            .map_err(|_| "Could not save the Last.fm scrobble ledger.".to_string())?;
        #[cfg(test)]
        if let Some(blocker) = &self.blocker {
            blocker.pause();
        }
        Ok(())
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path, limit: u64) -> Result<Option<T>, String> {
    match crate::persistence::read_limited(path, limit) {
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
    crate::persistence::atomic_write(path, &bytes, Some(0o600))
        .map_err(|_| "Could not save the Last.fm credential.".to_string())
}
fn remove_file(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("Could not remove a Last.fm local store.".into()),
    }
}

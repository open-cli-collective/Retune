use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use super::{
    model::{
        CachedRawPage, ImportDefaults, ImportPhase, LastFmImportSessionV2, LastFmSyncState,
        ParsedRecentTracksPage, ParsedScrobble, PersistedLastFmMappings, RawCacheManifest,
    },
    source::snapshot_cache_id,
    LASTFM_MAPPINGS_VERSION, LASTFM_SYNC_VERSION, MAX_RAW_CACHE_BYTES,
    MAX_SERIALIZED_SESSION_BYTES, SESSION_VERSION,
};

use crate::persistence::read_limited;

const MAX_INCREMENTAL_STATE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_MAPPINGS_BYTES: u64 = 100 * 1024 * 1024;
const MAX_REVIEW_TRANSACTION_BYTES: u64 = 256 * 1024 * 1024;
const REVIEW_TRANSACTION_VERSION: u32 = 1;

#[derive(Clone, serde::Deserialize, serde::Serialize)]
pub(super) struct ReviewTransaction {
    version: u32,
    pub(super) session: Option<LastFmImportSessionV2>,
    #[serde(default)]
    pub(super) sync_state: Option<LastFmSyncState>,
    pub(super) mappings: PersistedLastFmMappings,
}

impl ReviewTransaction {
    pub(super) fn new(session: LastFmImportSessionV2, mappings: PersistedLastFmMappings) -> Self {
        Self {
            version: REVIEW_TRANSACTION_VERSION,
            session: Some(session),
            sync_state: None,
            mappings,
        }
    }

    pub(super) fn migration(
        session: Option<LastFmImportSessionV2>,
        sync_state: LastFmSyncState,
        mappings: PersistedLastFmMappings,
    ) -> Self {
        Self {
            version: REVIEW_TRANSACTION_VERSION,
            session,
            sync_state: Some(sync_state),
            mappings,
        }
    }
}

#[derive(Clone)]
pub(super) struct ReviewTransactionStore {
    path: PathBuf,
    #[cfg(test)]
    recovery_hook: std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<crate::store::SaveHook>>>>,
}

#[derive(Clone)]
pub(super) struct ImportSessionStore {
    pub(super) path: PathBuf,
    pub(super) cache_root: PathBuf,
    #[cfg(test)]
    save_hook: std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<crate::store::SaveHook>>>>,
    #[cfg(test)]
    quarantine_hook:
        std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<crate::store::SaveHook>>>>,
}

#[derive(Clone)]
pub(super) struct IncrementalStore {
    path: PathBuf,
    #[cfg(test)]
    save_hook: std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<crate::store::SaveHook>>>>,
}

#[derive(Clone)]
pub(super) struct MappingsStore {
    path: PathBuf,
    #[cfg(test)]
    save_hook: std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<crate::store::SaveHook>>>>,
}

impl ReviewTransactionStore {
    pub(super) fn new(app_data_dir: impl AsRef<Path>) -> Self {
        Self {
            path: app_data_dir.as_ref().join("lastfm-review-transaction.json"),
            #[cfg(test)]
            recovery_hook: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    #[cfg(test)]
    pub(super) fn arm_recovery(&self, hook: std::sync::Arc<crate::store::SaveHook>) {
        *self.recovery_hook.lock().unwrap() = Some(hook);
    }

    pub(super) fn save(&self, transaction: &ReviewTransaction) -> Result<(), String> {
        let bytes = serde_json::to_vec(transaction)
            .map_err(|_| "Could not serialize the Last.fm review transaction.".to_string())?;
        if bytes.len() as u64 > MAX_REVIEW_TRANSACTION_BYTES {
            return Err("The Last.fm review transaction exceeds its safety limit.".into());
        }
        crate::persistence::atomic_write(&self.path, &bytes, Some(0o600))
            .map_err(|_| "Could not save the Last.fm review transaction.".to_string())
    }

    fn load(&self) -> Result<Option<ReviewTransaction>, String> {
        let bytes = match read_limited(&self.path, MAX_REVIEW_TRANSACTION_BYTES) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err("Could not read the Last.fm review transaction.".into()),
        };
        let transaction: ReviewTransaction = serde_json::from_slice(&bytes)
            .map_err(|_| "Could not read the Last.fm review transaction.".to_string())?;
        if transaction.version != REVIEW_TRANSACTION_VERSION {
            return Err("The Last.fm review transaction version is unsupported.".into());
        }
        Ok(Some(transaction))
    }

    pub(super) fn clear(&self) -> Result<(), String> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err("Could not finish the Last.fm review transaction.".into()),
        }
    }

    pub(super) fn recover(
        &self,
        sessions: &ImportSessionStore,
        sync: &IncrementalStore,
        mappings: &MappingsStore,
    ) -> Result<Option<ReviewTransaction>, String> {
        let Some(transaction) = self.load()? else {
            return Ok(None);
        };
        if let Some(session) = transaction.session.as_ref() {
            sessions.save(session)?;
        }
        if let Some(sync_state) = transaction.sync_state.as_ref() {
            sync.save(sync_state)?;
        }
        mappings.save(&transaction.mappings)?;
        self.clear()?;
        #[cfg(test)]
        if let Some(hook) = self.recovery_hook.lock().unwrap().take() {
            hook.pause().map_err(|error| error.to_string())?;
        }
        Ok(Some(transaction))
    }
}

fn quarantine_file(path: &Path, description: &str) -> Result<PathBuf, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "persistence".into());
    let target = path.with_file_name(format!("{file_name}.quarantine-{stamp}"));
    fs::rename(path, &target)
        .map(|_| target)
        .map_err(|_| format!("Could not quarantine {description}."))
}

fn validate_cache_id(cache_id: &str) -> Result<(), String> {
    let mut components = Path::new(cache_id).components();
    if matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none() {
        Ok(())
    } else {
        Err("The Last.fm import cache ID is invalid.".into())
    }
}

fn reject_symlink(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err("The Last.fm import cache contains an unsafe symbolic link.".into())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("Could not inspect the Last.fm import cache path.".into()),
    }
}

fn validate_snapshot_cache_id(session: &LastFmImportSessionV2) -> Result<(), String> {
    validate_cache_id(&session.cache_id)?;
    if session.cache_id == snapshot_cache_id(&session.lastfm_username, session.history_to) {
        Ok(())
    } else {
        Err("The Last.fm import cache ID does not match its session.".into())
    }
}

fn validate_incremental_cache_id(state: &LastFmSyncState) -> Result<(), String> {
    let Some(range) = state.active.as_ref() else {
        return Ok(());
    };
    let username = state
        .lastfm_username
        .as_deref()
        .ok_or_else(|| "The Last.fm incremental cache has no account owner.".to_string())?;
    validate_cache_id(&range.cache_id)?;
    if range.cache_id == incremental_cache_id(username, range.from, range.to) {
        Ok(())
    } else {
        Err("The Last.fm incremental cache ID does not match its range.".into())
    }
}

impl IncrementalStore {
    pub(super) fn new(app_data_dir: impl AsRef<Path>) -> Self {
        Self {
            path: app_data_dir.as_ref().join("lastfm-sync.json"),
            #[cfg(test)]
            save_hook: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    #[cfg(test)]
    pub(super) fn arm_save(&self, hook: std::sync::Arc<crate::store::SaveHook>) {
        *self.save_hook.lock().unwrap() = Some(hook);
    }

    pub(super) fn load(&self) -> Result<LastFmSyncState, String> {
        let bytes = match read_limited(&self.path, MAX_INCREMENTAL_STATE_BYTES) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(LastFmSyncState {
                    version: LASTFM_SYNC_VERSION,
                    ..LastFmSyncState::default()
                });
            }
            Err(_) => {
                quarantine_file(&self.path, "the Last.fm incremental sync state")?;
                return Err("Last.fm sync state was quarantined; sync starts from now.".into());
            }
        };
        let state = match serde_json::from_slice::<LastFmSyncState>(&bytes) {
            Ok(state) => state,
            Err(_) => {
                quarantine_file(&self.path, "the Last.fm incremental sync state")?;
                return Err("Last.fm sync state was quarantined; sync starts from now.".into());
            }
        };
        if state.version != LASTFM_SYNC_VERSION || validate_incremental_cache_id(&state).is_err() {
            quarantine_file(&self.path, "the Last.fm incremental sync state")?;
            return Err("Last.fm sync state was quarantined; sync starts from now.".into());
        }
        Ok(state)
    }

    pub(super) fn save(&self, state: &LastFmSyncState) -> Result<(), String> {
        #[cfg(test)]
        if let Some(hook) = self.save_hook.lock().unwrap().take() {
            hook.pause().map_err(|error| error.to_string())?;
        }
        validate_incremental_cache_id(state)?;
        let bytes = serde_json::to_vec(state)
            .map_err(|_| "Could not serialize Last.fm incremental sync state.".to_string())?;
        crate::persistence::atomic_write(&self.path, &bytes, Some(0o600))
            .map_err(|_| "Could not save Last.fm incremental sync state.".to_string())
    }
}

impl MappingsStore {
    pub(super) fn new(app_data_dir: impl AsRef<Path>) -> Self {
        Self {
            path: app_data_dir.as_ref().join("lastfm-mappings.json"),
            #[cfg(test)]
            save_hook: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    #[cfg(test)]
    pub(super) fn arm_save(&self, hook: std::sync::Arc<crate::store::SaveHook>) {
        *self.save_hook.lock().unwrap() = Some(hook);
    }

    pub(super) fn load(&self) -> Result<PersistedLastFmMappings, String> {
        let bytes = match read_limited(&self.path, MAX_MAPPINGS_BYTES) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PersistedLastFmMappings {
                    version: LASTFM_MAPPINGS_VERSION,
                    ..PersistedLastFmMappings::default()
                });
            }
            Err(_) => {
                quarantine_file(&self.path, "the Last.fm mappings")?;
                return Err(
                    "Last.fm mappings were quarantined; reusable decisions were reset.".into(),
                );
            }
        };
        let mappings = match serde_json::from_slice::<PersistedLastFmMappings>(&bytes) {
            Ok(mappings) => mappings,
            Err(_) => {
                quarantine_file(&self.path, "the Last.fm mappings")?;
                return Err(
                    "Last.fm mappings were quarantined; reusable decisions were reset.".into(),
                );
            }
        };
        if mappings.version != LASTFM_MAPPINGS_VERSION {
            quarantine_file(&self.path, "the Last.fm mappings")?;
            return Err("Last.fm mappings were quarantined; reusable decisions were reset.".into());
        }
        Ok(mappings)
    }

    pub(super) fn save(&self, mappings: &PersistedLastFmMappings) -> Result<(), String> {
        #[cfg(test)]
        if let Some(hook) = self.save_hook.lock().unwrap().take() {
            hook.pause().map_err(|error| error.to_string())?;
        }
        let bytes = serde_json::to_vec(mappings)
            .map_err(|_| "Could not serialize Last.fm mappings.".to_string())?;
        crate::persistence::atomic_write(&self.path, &bytes, Some(0o600))
            .map_err(|_| "Could not save Last.fm mappings.".to_string())
    }
}

pub(crate) fn normalize_restored_mappings(
    imported: PersistedLastFmMappings,
) -> Result<PersistedLastFmMappings, String> {
    if imported.version != LASTFM_MAPPINGS_VERSION {
        return Err("The Last.fm mappings version is unsupported.".into());
    }
    Ok(PersistedLastFmMappings {
        version: LASTFM_MAPPINGS_VERSION,
        lastfm_username: imported.lastfm_username,
        spotify_account_id: imported.spotify_account_id,
        dormant: true,
        mappings: imported.mappings,
    })
}

pub(crate) fn load_mappings_for_recovery(
    app_data_dir: &Path,
) -> Result<PersistedLastFmMappings, String> {
    let store = MappingsStore::new(app_data_dir);
    let bytes = match read_limited(&store.path, MAX_MAPPINGS_BYTES) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PersistedLastFmMappings {
                version: LASTFM_MAPPINGS_VERSION,
                ..PersistedLastFmMappings::default()
            });
        }
        Err(error) => return Err(format!("Could not read Last.fm mappings: {error}")),
    };
    let mappings: PersistedLastFmMappings = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Could not parse Last.fm mappings: {error}"))?;
    if mappings.version != LASTFM_MAPPINGS_VERSION {
        return Err("The Last.fm mappings version is unsupported.".into());
    }
    Ok(mappings)
}

pub(crate) fn save_mappings_for_recovery(
    app_data_dir: &Path,
    mappings: &PersistedLastFmMappings,
) -> Result<(), String> {
    MappingsStore::new(app_data_dir).save(mappings)
}

pub(super) fn incremental_cache_id(username: &str, from: u64, to: u64) -> String {
    format!(
        "incremental-{}-{from}-{to}",
        username
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

pub(super) fn incremental_cache_session(
    state: &LastFmSyncState,
    username: &str,
) -> Result<LastFmImportSessionV2, String> {
    let range = state
        .active
        .as_ref()
        .ok_or_else(|| "No Last.fm incremental range is active.".to_string())?;
    validate_cache_id(&range.cache_id)?;
    if range.cache_id != incremental_cache_id(username, range.from, range.to) {
        return Err("The Last.fm incremental cache ID does not match its range.".into());
    }
    let total_pages = range
        .total_pages
        .ok_or_else(|| "Last.fm incremental metadata is not available yet.".to_string())?;
    let mut session = LastFmImportSessionV2::new_with_defaults(
        username.to_owned(),
        range.to,
        ImportDefaults::default(),
    );
    session.cache_id = range.cache_id.clone();
    session.total_pages = Some(total_pages);
    session.next_page = range.next_page;
    session.downloaded_pages = range.downloaded_pages;
    Ok(session)
}

impl ImportSessionStore {
    pub(super) fn new(app_data_dir: impl AsRef<Path>) -> Self {
        let app_data_dir = app_data_dir.as_ref();
        Self {
            path: app_data_dir.join("lastfm-import.json"),
            cache_root: app_data_dir.join("lastfm-import-cache"),
            #[cfg(test)]
            save_hook: std::sync::Arc::new(std::sync::Mutex::new(None)),
            #[cfg(test)]
            quarantine_hook: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    #[cfg(test)]
    pub(super) fn arm_save(&self, hook: std::sync::Arc<crate::store::SaveHook>) {
        *self.save_hook.lock().unwrap() = Some(hook);
    }

    #[cfg(test)]
    pub(super) fn arm_quarantine(&self, hook: std::sync::Arc<crate::store::SaveHook>) {
        *self.quarantine_hook.lock().unwrap() = Some(hook);
    }

    pub(super) fn load(&self) -> Result<Option<LastFmImportSessionV2>, String> {
        let bytes = match read_limited(&self.path, MAX_SERIALIZED_SESSION_BYTES as u64) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                self.quarantine()?;
                self.quarantine_cache_root()?;
                return Ok(None);
            }
            Err(_) => return Err("Could not read the Last.fm import session.".into()),
        };
        let session = match serde_json::from_slice::<LastFmImportSessionV2>(&bytes) {
            Ok(session) => session,
            Err(_) => {
                self.quarantine()?;
                self.quarantine_cache_root()?;
                return Ok(None);
            }
        };
        if validate_snapshot_cache_id(&session).is_err() {
            self.quarantine()?;
            return Ok(None);
        }
        if session.version != SESSION_VERSION
            || session.defaults.validate().is_err()
            || session
                .page_options
                .values()
                .any(|options| options.validate().is_err())
        {
            self.quarantine()?;
            self.quarantine_cache_root()?;
            return Ok(None);
        }
        if (matches!(
            session.phase,
            ImportPhase::Downloading | ImportPhase::Aggregating
        ) || suspended_source_phase(&session))
            && self.validate_cache(&session).is_err()
        {
            self.quarantine()?;
            self.quarantine_snapshot(&session.cache_id)?;
            return Ok(None);
        }
        Ok(Some(session))
    }

    pub(super) fn save(&self, session: &LastFmImportSessionV2) -> Result<(), String> {
        #[cfg(test)]
        if let Some(hook) = self.save_hook.lock().unwrap().take() {
            hook.pause().map_err(|error| error.to_string())?;
        }
        validate_snapshot_cache_id(session)?;
        let bytes = serde_json::to_vec(session)
            .map_err(|_| "Could not serialize the Last.fm import session.".to_string())?;
        if bytes.len() > MAX_SERIALIZED_SESSION_BYTES {
            return Err("The Last.fm import session exceeds the 100 MB safety limit.".into());
        }
        crate::persistence::atomic_write(&self.path, &bytes, Some(0o600))
            .map_err(|_| "Could not save the Last.fm import session.".to_string())
    }

    pub(super) fn cache_path(&self, cache_id: &str) -> Result<PathBuf, String> {
        validate_cache_id(cache_id)?;
        reject_symlink(&self.cache_root)?;
        let path = self.cache_root.join(cache_id);
        reject_symlink(&path)?;
        Ok(path)
    }

    pub(super) fn manifest_path(&self, cache_id: &str) -> Result<PathBuf, String> {
        let path = self.cache_path(cache_id)?.join("manifest.json");
        reject_symlink(&path)?;
        Ok(path)
    }

    pub(super) fn page_path(&self, cache_id: &str, page: u32) -> Result<PathBuf, String> {
        let path = self.cache_path(cache_id)?.join(format!("page-{page}.json"));
        reject_symlink(&path)?;
        Ok(path)
    }

    pub(super) fn read_manifest(
        &self,
        session: &LastFmImportSessionV2,
    ) -> Result<Option<RawCacheManifest>, String> {
        let path = self.manifest_path(&session.cache_id)?;
        let bytes = match read_limited(&path, MAX_RAW_CACHE_BYTES) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                return Err(
                    "The Last.fm import cache manifest exceeds the 100 MB safety limit.".into(),
                );
            }
            Err(_) => return Err("Could not read the Last.fm import cache manifest.".into()),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| "The Last.fm import cache manifest is corrupt.".into())
    }

    pub(super) fn write_page(
        &self,
        session: &LastFmImportSessionV2,
        parsed: &ParsedRecentTracksPage,
    ) -> Result<(), String> {
        let total_pages = session
            .total_pages
            .ok_or_else(|| "Last.fm import metadata is not available yet.".to_string())?;
        if parsed.page == 0 || parsed.page > total_pages {
            return Err("Last.fm import page metadata is out of range.".into());
        }
        let cached = CachedRawPage {
            lastfm_username: session.lastfm_username.clone(),
            history_to: session.history_to,
            total_pages,
            parsed: parsed.clone(),
        };
        let bytes = serde_json::to_vec(&cached)
            .map_err(|_| "Could not serialize a Last.fm import page.".to_string())?;
        if bytes.len() as u64 > MAX_RAW_CACHE_BYTES {
            return Err("The Last.fm import page exceeds the 100 MB safety limit.".into());
        }

        let manifest = self.read_manifest(session)?;
        if let Some(manifest) = &manifest {
            self.validate_manifest_metadata(manifest, session, total_pages)?;
        }
        let mut manifest = manifest.unwrap_or_else(|| RawCacheManifest {
            version: SESSION_VERSION,
            cache_id: session.cache_id.clone(),
            lastfm_username: session.lastfm_username.clone(),
            history_to: session.history_to,
            total_pages,
            pages: BTreeMap::new(),
        });
        let previous = manifest.pages.insert(parsed.page, bytes.len() as u64);
        let current_size = manifest
            .pages
            .values()
            .copied()
            .try_fold(0_u64, u64::checked_add)
            .ok_or_else(|| "The Last.fm import cache size is invalid.".to_string())?;
        if current_size > MAX_RAW_CACHE_BYTES {
            match previous {
                Some(previous) => {
                    manifest.pages.insert(parsed.page, previous);
                }
                None => {
                    manifest.pages.remove(&parsed.page);
                }
            }
            return Err("The Last.fm import cache exceeds the 100 MB safety limit.".into());
        }
        fs::create_dir_all(self.cache_path(&session.cache_id)?)
            .map_err(|_| "Could not create the Last.fm import cache.".to_string())?;
        crate::persistence::atomic_write(
            &self.page_path(&session.cache_id, parsed.page)?,
            &bytes,
            Some(0o600),
        )
        .map_err(|_| "Could not save the Last.fm import page.".to_string())?;
        let manifest_bytes = serde_json::to_vec(&manifest)
            .map_err(|_| "Could not serialize the Last.fm import cache manifest.".to_string())?;
        crate::persistence::atomic_write(
            &self.manifest_path(&session.cache_id)?,
            &manifest_bytes,
            Some(0o600),
        )
        .map_err(|_| "Could not save the Last.fm import cache manifest.".to_string())
    }

    pub(super) fn validate_cache(&self, session: &LastFmImportSessionV2) -> Result<(), String> {
        validate_session_cursor(session)?;
        let Some(manifest) = self.read_manifest(session)? else {
            return if session.downloaded_pages == 0 {
                Ok(())
            } else {
                Err("The Last.fm import cache manifest is missing.".into())
            };
        };
        let total_pages = session
            .total_pages
            .ok_or_else(|| "The Last.fm import cache has no page total.".to_string())?;
        self.validate_manifest_metadata(&manifest, session, total_pages)?;
        let acknowledged_pages = session.downloaded_pages as usize;
        let max_pages = acknowledged_pages + usize::from(session.next_page > 0);
        if manifest.pages.len() < acknowledged_pages || manifest.pages.len() > max_pages {
            return Err("The Last.fm import cache has a non-contiguous page suffix.".into());
        }
        if let Some((&first_page, _)) = manifest.pages.first_key_value() {
            let expected_last_page = manifest.pages.last_key_value().map(|(&page, _)| page);
            if expected_last_page != Some(total_pages)
                || first_page < session.next_page.max(1)
                || manifest
                    .pages
                    .keys()
                    .copied()
                    .zip(first_page..=total_pages)
                    .any(|(actual, expected)| actual != expected)
            {
                return Err("The Last.fm import cache has a non-contiguous page suffix.".into());
            }
        }
        let total_size = manifest
            .pages
            .values()
            .copied()
            .try_fold(0_u64, u64::checked_add)
            .ok_or_else(|| "The Last.fm import cache size is invalid.".to_string())?;
        if total_size > MAX_RAW_CACHE_BYTES {
            return Err("The Last.fm import cache exceeds the 100 MB safety limit.".into());
        }
        for (&page, &recorded_size) in &manifest.pages {
            if page == 0 || page > total_pages {
                return Err("The Last.fm import cache contains an invalid page.".into());
            }
            let path = self.page_path(&session.cache_id, page)?;
            let actual_size = fs::metadata(&path)
                .map_err(|_| "An acknowledged Last.fm import page is missing.".to_string())?
                .len();
            if actual_size != recorded_size || recorded_size > MAX_RAW_CACHE_BYTES {
                return Err(
                    "An acknowledged Last.fm import page is oversized or truncated.".into(),
                );
            }
            let bytes = read_limited(&path, recorded_size)
                .map_err(|_| "An acknowledged Last.fm import page is missing.".to_string())?;
            let cached = serde_json::from_slice::<CachedRawPage>(&bytes)
                .map_err(|_| "An acknowledged Last.fm import page is corrupt.".to_string())?;
            if cached.lastfm_username != session.lastfm_username
                || cached.history_to != session.history_to
                || cached.total_pages != total_pages
                || cached.parsed.page != page
                || cached
                    .parsed
                    .total_pages
                    .is_some_and(|value| value != total_pages)
            {
                return Err("An acknowledged Last.fm import page has mismatched metadata.".into());
            }
        }
        Ok(())
    }

    fn validate_manifest_metadata(
        &self,
        manifest: &RawCacheManifest,
        session: &LastFmImportSessionV2,
        total_pages: u32,
    ) -> Result<(), String> {
        if manifest.version != SESSION_VERSION
            || manifest.cache_id != session.cache_id
            || manifest.lastfm_username != session.lastfm_username
            || manifest.history_to != session.history_to
            || manifest.total_pages != total_pages
        {
            return Err("The Last.fm import cache metadata does not match its session.".into());
        }
        Ok(())
    }

    pub(super) fn read_pages(
        &self,
        session: &LastFmImportSessionV2,
    ) -> Result<Vec<ParsedScrobble>, String> {
        self.validate_cache(session)?;
        let Some(manifest) = self.read_manifest(session)? else {
            return Ok(Vec::new());
        };
        let mut scrobbles = Vec::new();
        for (page, size) in &manifest.pages {
            let bytes = read_limited(&self.page_path(&session.cache_id, *page)?, *size)
                .map_err(|_| "An acknowledged Last.fm import page is missing.".to_string())?;
            let cached = serde_json::from_slice::<CachedRawPage>(&bytes)
                .map_err(|_| "An acknowledged Last.fm import page is corrupt.".to_string())?;
            if cached.lastfm_username != session.lastfm_username {
                return Err(
                    "An acknowledged Last.fm import page belongs to another Last.fm account."
                        .into(),
                );
            }
            scrobbles.extend(
                cached
                    .parsed
                    .tracks
                    .into_iter()
                    .filter(|scrobble| scrobble.timestamp < session.history_to),
            );
        }
        Ok(scrobbles)
    }

    pub(super) fn remove_snapshot(&self, cache_id: &str) -> std::io::Result<()> {
        let path = self
            .cache_path(cache_id)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
        match fs::remove_dir_all(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }

    pub(super) fn quarantine_snapshot(&self, cache_id: &str) -> Result<(), String> {
        let path = self.cache_path(cache_id)?;
        if !path.exists() {
            return Ok(());
        }
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        fs::rename(
            &path,
            path.with_file_name(format!("{cache_id}.quarantine-{stamp}")),
        )
        .map_err(|_| "Could not quarantine the Last.fm import cache.".to_string())
    }

    fn quarantine_cache_root(&self) -> Result<(), String> {
        reject_symlink(&self.cache_root)?;
        if !self.cache_root.exists() {
            return Ok(());
        }
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        fs::rename(
            &self.cache_root,
            self.cache_root
                .with_file_name(format!("lastfm-import-cache.quarantine-{stamp}")),
        )
        .map_err(|_| "Could not quarantine the Last.fm import cache.".to_string())
    }

    pub(super) fn quarantine(&self) -> Result<(), String> {
        quarantine_file(&self.path, "the Last.fm import session")?;
        #[cfg(test)]
        if let Some(hook) = self.quarantine_hook.lock().unwrap().take() {
            hook.pause().map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

fn validate_session_cursor(session: &LastFmImportSessionV2) -> Result<(), String> {
    match session.total_pages {
        None if session.next_page == 1 && session.downloaded_pages == 0 => Ok(()),
        None => Err("The Last.fm import cursor has no valid page total.".into()),
        Some(0) if session.next_page == 0 && session.downloaded_pages == 0 => Ok(()),
        Some(total_pages)
            if session.downloaded_pages <= total_pages
                && session.next_page == total_pages.saturating_sub(session.downloaded_pages) =>
        {
            Ok(())
        }
        Some(_) => Err("The Last.fm import cursor is inconsistent with its page total.".into()),
    }
}

pub(super) fn suspended_source_phase(session: &LastFmImportSessionV2) -> bool {
    session.phase == ImportPhase::Suspended && session.rows.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lastfm_import::model::IncrementalRange;

    fn write_json(path: &Path, value: &impl serde::Serialize) {
        fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
    }

    fn incremental_state(username: &str, from: u64, to: u64) -> LastFmSyncState {
        LastFmSyncState {
            version: LASTFM_SYNC_VERSION,
            lastfm_username: Some(username.into()),
            active: Some(IncrementalRange {
                from,
                to,
                query_from: from.saturating_sub(1),
                query_to: to.saturating_add(1),
                cache_id: incremental_cache_id(username, from, to),
                next_page: 1,
                total_pages: None,
                downloaded_pages: 0,
                total_scrobbles: 0,
            }),
            ..LastFmSyncState::default()
        }
    }

    #[test]
    fn invalid_persisted_cache_ids_never_touch_sibling_paths() {
        for invalid in ["", ".", "..", "../sentinel", "nested/cache"] {
            let directory = tempfile::tempdir().unwrap();
            let store = ImportSessionStore::new(directory.path());
            let sentinel = directory.path().join("sentinel");
            fs::create_dir(&sentinel).unwrap();
            fs::write(sentinel.join("keep"), b"safe").unwrap();

            let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 42);
            session.phase = ImportPhase::Review;
            session.cache_id = invalid.into();
            write_json(&store.path, &session);

            assert!(store.load().unwrap().is_none());
            assert_eq!(fs::read(sentinel.join("keep")).unwrap(), b"safe");
            assert!(store.remove_snapshot(invalid).is_err());
            assert!(store.quarantine_snapshot(invalid).is_err());
            assert_eq!(fs::read(sentinel.join("keep")).unwrap(), b"safe");
        }

        let directory = tempfile::tempdir().unwrap();
        let store = ImportSessionStore::new(directory.path());
        let sentinel = directory.path().join("sentinel");
        fs::create_dir(&sentinel).unwrap();
        fs::write(sentinel.join("keep"), b"safe").unwrap();
        let absolute = sentinel.to_string_lossy().into_owned();
        let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 42);
        session.phase = ImportPhase::Review;
        session.cache_id = absolute.clone();
        write_json(&store.path, &session);

        assert!(store.load().unwrap().is_none());
        assert!(store.remove_snapshot(&absolute).is_err());
        assert!(store.quarantine_snapshot(&absolute).is_err());
        assert_eq!(fs::read(sentinel.join("keep")).unwrap(), b"safe");
    }

    #[test]
    fn invalid_incremental_cache_ids_are_quarantined_without_cache_access() {
        for invalid in ["", ".", "..", "../sentinel", "nested/cache"] {
            let directory = tempfile::tempdir().unwrap();
            let store = IncrementalStore::new(directory.path());
            let sentinel = directory.path().join("sentinel");
            fs::create_dir(&sentinel).unwrap();
            fs::write(sentinel.join("keep"), b"safe").unwrap();
            let mut state = incremental_state("user", 10, 20);
            state.active.as_mut().unwrap().cache_id = invalid.into();
            write_json(&store.path, &state);

            assert!(store.load().is_err());
            assert!(!store.path.exists());
            assert_eq!(fs::read(sentinel.join("keep")).unwrap(), b"safe");
            assert!(incremental_cache_session(&state, "user").is_err());
        }

        let directory = tempfile::tempdir().unwrap();
        let store = IncrementalStore::new(directory.path());
        let sentinel = directory.path().join("sentinel");
        fs::create_dir(&sentinel).unwrap();
        fs::write(sentinel.join("keep"), b"safe").unwrap();
        let mut state = incremental_state("user", 10, 20);
        state.active.as_mut().unwrap().cache_id = sentinel.to_string_lossy().into_owned();
        write_json(&store.path, &state);

        assert!(store.load().is_err());
        assert!(!store.path.exists());
        assert_eq!(fs::read(sentinel.join("keep")).unwrap(), b"safe");
        assert!(incremental_cache_session(&state, "user").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_snapshot_is_never_read_written_renamed_or_deleted() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let store = ImportSessionStore::new(directory.path());
        let sentinel = directory.path().join("sentinel");
        fs::create_dir(&sentinel).unwrap();
        fs::write(sentinel.join("keep"), b"safe").unwrap();
        fs::create_dir(&store.cache_root).unwrap();

        let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 42);
        session.total_pages = Some(1);
        let link = store.cache_root.join(&session.cache_id);
        symlink(&sentinel, &link).unwrap();
        write_json(&store.path, &session);
        let page = ParsedRecentTracksPage {
            page: 1,
            total_pages: Some(1),
            ..ParsedRecentTracksPage::default()
        };

        assert!(store.read_manifest(&session).is_err());
        assert!(store.read_pages(&session).is_err());
        assert!(store.write_page(&session, &page).is_err());
        assert!(store.remove_snapshot(&session.cache_id).is_err());
        assert!(store.quarantine_snapshot(&session.cache_id).is_err());
        assert!(store.load().is_err());
        assert!(!store.path.exists());
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(fs::read(sentinel.join("keep")).unwrap(), b"safe");
    }

    #[test]
    fn oversized_sparse_import_session_is_quarantined_before_reading() {
        let directory = tempfile::tempdir().unwrap();
        let store = ImportSessionStore::new(directory.path());
        fs::File::create(&store.path)
            .unwrap()
            .set_len(MAX_SERIALIZED_SESSION_BYTES as u64 + 1)
            .unwrap();

        assert_eq!(store.load().unwrap(), None);
        assert!(!store.path.exists());
        assert!(fs::read_dir(directory.path())
            .unwrap()
            .flatten()
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with("lastfm-import.json.quarantine-")));
    }
}

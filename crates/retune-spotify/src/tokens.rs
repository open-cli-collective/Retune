use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chacha20poly1305::{
    ChaCha20Poly1305, Nonce,
    aead::{Aead, KeyInit},
};
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

const SERVICE: &str = "com.rianjs.retune";
const LEGACY_ACCOUNT: &str = "spotify-oauth";
const KEY_ACCOUNT: &str = "token-file-key";
const NONCE_LEN: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub access: String,
    pub refresh: String,
    /// Unix timestamp in seconds.
    pub expires_at: u64,
    #[serde(default)]
    pub scopes: String,
}

impl Tokens {
    pub fn missing_scopes(&self) -> Vec<&'static str> {
        let granted = self.scopes.split_ascii_whitespace().collect::<Vec<_>>();
        crate::auth::REQUIRED_SCOPES
            .into_iter()
            .filter(|required| !granted.contains(required))
            .collect()
    }
}

pub trait TokenStore: Send + Sync {
    fn load(&self) -> Result<Option<Tokens>>;
    fn save(&self, tokens: &Tokens) -> Result<()>;
    fn clear(&self) -> Result<()>;
}

impl<S: TokenStore + ?Sized> TokenStore for Arc<S> {
    fn load(&self) -> Result<Option<Tokens>> {
        (**self).load()
    }

    fn save(&self, tokens: &Tokens) -> Result<()> {
        (**self).save(tokens)
    }

    fn clear(&self) -> Result<()> {
        (**self).clear()
    }
}

impl<S: TokenStore + ?Sized> TokenStore for Box<S> {
    fn load(&self) -> Result<Option<Tokens>> {
        (**self).load()
    }

    fn save(&self, tokens: &Tokens) -> Result<()> {
        (**self).save(tokens)
    }

    fn clear(&self) -> Result<()> {
        (**self).clear()
    }
}

pub struct CachedTokenStore<S> {
    inner: S,
    cache: Mutex<Option<Option<Tokens>>>,
}

impl<S: TokenStore> CachedTokenStore<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            cache: Mutex::new(None),
        }
    }
}

impl<S: TokenStore> TokenStore for CachedTokenStore<S> {
    fn load(&self) -> Result<Option<Tokens>> {
        let mut cache = self
            .cache
            .lock()
            .map_err(|error| Error::TokenStore(error.to_string()))?;
        if let Some(tokens) = cache.as_ref() {
            return Ok(tokens.clone());
        }
        let tokens = self.inner.load()?;
        *cache = Some(tokens.clone());
        Ok(tokens)
    }

    fn save(&self, tokens: &Tokens) -> Result<()> {
        self.inner.save(tokens)?;
        *self
            .cache
            .lock()
            .map_err(|error| Error::TokenStore(error.to_string()))? = Some(Some(tokens.clone()));
        Ok(())
    }

    fn clear(&self) -> Result<()> {
        self.inner.clear()?;
        *self
            .cache
            .lock()
            .map_err(|error| Error::TokenStore(error.to_string()))? = Some(None);
        Ok(())
    }
}

pub struct KeychainTokenStore {
    entry: keyring::Entry,
}

impl KeychainTokenStore {
    pub fn new() -> Result<Self> {
        keyring::Entry::new(SERVICE, LEGACY_ACCOUNT)
            .map(|entry| Self { entry })
            .map_err(|error| Error::TokenStore(error.to_string()))
    }
}

impl TokenStore for KeychainTokenStore {
    fn load(&self) -> Result<Option<Tokens>> {
        match self.entry.get_password() {
            Ok(value) => serde_json::from_str(&value)
                .map(Some)
                .map_err(|error| Error::TokenStore(error.to_string())),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(Error::TokenStore(error.to_string())),
        }
    }

    fn save(&self, tokens: &Tokens) -> Result<()> {
        let value =
            serde_json::to_string(tokens).map_err(|error| Error::TokenStore(error.to_string()))?;
        self.entry
            .set_password(&value)
            .map_err(|error| Error::TokenStore(error.to_string()))
    }

    fn clear(&self) -> Result<()> {
        match self.entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(Error::TokenStore(error.to_string())),
        }
    }
}

trait KeySource: Send + Sync {
    fn load_or_create(&self) -> Result<[u8; 32]>;
}

struct KeychainKeySource {
    entry: keyring::Entry,
}

impl KeychainKeySource {
    fn new() -> Result<Self> {
        keyring::Entry::new(SERVICE, KEY_ACCOUNT)
            .map(|entry| Self { entry })
            .map_err(token_error)
    }
}

impl KeySource for KeychainKeySource {
    fn load_or_create(&self) -> Result<[u8; 32]> {
        log::debug!("Loading Spotify token file key from Keychain");
        match self.entry.get_password() {
            Ok(value) => BASE64
                .decode(value)
                .map_err(token_error)?
                .try_into()
                .map_err(|_| Error::TokenStore("token file key must be 32 bytes".into())),
            Err(keyring::Error::NoEntry) => {
                let mut key = [0; 32];
                rand::rng().fill_bytes(&mut key);
                self.entry
                    .set_password(&BASE64.encode(key))
                    .map_err(token_error)?;
                Ok(key)
            }
            Err(error) => Err(token_error(error)),
        }
    }
}

pub struct EncryptedFsTokenStore {
    path: PathBuf,
    key_source: Box<dyn KeySource>,
    key: OnceLock<std::result::Result<[u8; 32], String>>,
}

impl EncryptedFsTokenStore {
    pub fn new(app_data_dir: impl AsRef<Path>) -> Result<Self> {
        Ok(Self::with_key_source(
            app_data_dir,
            KeychainKeySource::new()?,
        ))
    }

    fn with_key_source(
        app_data_dir: impl AsRef<Path>,
        key_source: impl KeySource + 'static,
    ) -> Self {
        Self {
            path: app_data_dir.as_ref().join("tokens.enc"),
            key_source: Box::new(key_source),
            key: OnceLock::new(),
        }
    }

    fn key(&self) -> Result<[u8; 32]> {
        self.key
            .get_or_init(|| {
                self.key_source
                    .load_or_create()
                    .map_err(|error| error.to_string())
            })
            .as_ref()
            .copied()
            .map_err(|error| Error::TokenStore(error.clone()))
    }

    fn file_exists(&self) -> Result<bool> {
        match fs::metadata(&self.path) {
            Ok(metadata) if metadata.is_file() => Ok(true),
            Ok(_) => Err(Error::TokenStore(format!(
                "{} is not a regular file",
                self.path.display()
            ))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(token_error(error)),
        }
    }
}

impl TokenStore for EncryptedFsTokenStore {
    fn load(&self) -> Result<Option<Tokens>> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(token_error(error)),
        };
        if bytes.len() < NONCE_LEN {
            log::warn!("Ignoring corrupt encrypted Spotify token file");
            return Ok(None);
        }

        let cipher = ChaCha20Poly1305::new((&self.key()?).into());
        let plaintext =
            match cipher.decrypt(Nonce::from_slice(&bytes[..NONCE_LEN]), &bytes[NONCE_LEN..]) {
                Ok(plaintext) => plaintext,
                Err(_) => {
                    log::warn!("Ignoring undecryptable Spotify token file");
                    return Ok(None);
                }
            };
        match serde_json::from_slice(&plaintext) {
            Ok(tokens) => Ok(Some(tokens)),
            Err(error) => {
                log::warn!("Ignoring corrupt Spotify token file: {error}");
                Ok(None)
            }
        }
    }

    fn save(&self, tokens: &Tokens) -> Result<()> {
        let plaintext = serde_json::to_vec(tokens).map_err(token_error)?;
        let mut nonce = [0; NONCE_LEN];
        rand::rng().fill_bytes(&mut nonce);
        let cipher = ChaCha20Poly1305::new((&self.key()?).into());
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce), plaintext.as_ref())
            .map_err(token_error)?;
        let mut bytes = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        bytes.extend_from_slice(&nonce);
        bytes.extend_from_slice(&ciphertext);
        atomic_write(&self.path, &bytes)
    }

    fn clear(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(token_error(error)),
        }
    }
}

pub fn migrate_token_store(
    legacy: &(impl TokenStore + ?Sized),
    encrypted: &EncryptedFsTokenStore,
) -> Result<()> {
    if encrypted.file_exists()? {
        return Ok(());
    }
    if let Some(tokens) = legacy.load()? {
        encrypted.save(&tokens)?;
        if let Err(error) = legacy.clear() {
            if let Err(cleanup_error) = encrypted.clear() {
                log::warn!(
                    "Could not remove encrypted Spotify tokens after migration failed: {cleanup_error}"
                );
            }
            return Err(error);
        }
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::TokenStore("token file path has no parent".into()))?;
    fs::create_dir_all(parent).map_err(token_error)?;
    let temporary = path.with_extension(format!("tmp-{}", rand::random::<u64>()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(token_error)
}

fn token_error(error: impl std::fmt::Display) -> Error {
    Error::TokenStore(error.to_string())
}

#[derive(Debug, Default)]
pub struct InMemoryTokenStore(Mutex<Option<Tokens>>);

impl InMemoryTokenStore {
    pub fn new(tokens: Option<Tokens>) -> Self {
        Self(Mutex::new(tokens))
    }
}

impl TokenStore for InMemoryTokenStore {
    fn load(&self) -> Result<Option<Tokens>> {
        Ok(self
            .0
            .lock()
            .map_err(|error| Error::TokenStore(error.to_string()))?
            .clone())
    }

    fn save(&self, tokens: &Tokens) -> Result<()> {
        *self
            .0
            .lock()
            .map_err(|error| Error::TokenStore(error.to_string()))? = Some(tokens.clone());
        Ok(())
    }

    fn clear(&self) -> Result<()> {
        *self
            .0
            .lock()
            .map_err(|error| Error::TokenStore(error.to_string()))? = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::{MetadataExt, PermissionsExt},
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use super::*;

    #[derive(Default)]
    struct CountingStore {
        tokens: Mutex<Option<Tokens>>,
        loads: AtomicUsize,
        saves: AtomicUsize,
        clears: AtomicUsize,
        fail_save: AtomicBool,
        fail_clear: AtomicBool,
    }

    impl TokenStore for CountingStore {
        fn load(&self) -> Result<Option<Tokens>> {
            self.loads.fetch_add(1, Ordering::Relaxed);
            Ok(self.tokens.lock().unwrap().clone())
        }

        fn save(&self, tokens: &Tokens) -> Result<()> {
            self.saves.fetch_add(1, Ordering::Relaxed);
            if self.fail_save.load(Ordering::Relaxed) {
                return Err(Error::TokenStore("save failed".into()));
            }
            *self.tokens.lock().unwrap() = Some(tokens.clone());
            Ok(())
        }

        fn clear(&self) -> Result<()> {
            self.clears.fetch_add(1, Ordering::Relaxed);
            if self.fail_clear.load(Ordering::Relaxed) {
                return Err(Error::TokenStore("clear failed".into()));
            }
            *self.tokens.lock().unwrap() = None;
            Ok(())
        }
    }

    fn tokens(access: &str) -> Tokens {
        Tokens {
            access: access.into(),
            refresh: "refresh".into(),
            expires_at: 42,
            scopes: "streaming".into(),
        }
    }

    #[test]
    fn memory_store_round_trip_and_clear() {
        let store = InMemoryTokenStore::default();
        let tokens = Tokens {
            access: "access".into(),
            refresh: "refresh".into(),
            expires_at: 42,
            scopes: "streaming".into(),
        };
        store.save(&tokens).unwrap();
        assert_eq!(store.load().unwrap(), Some(tokens));
        store.clear().unwrap();
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn cached_store_reads_inner_once_and_updates_after_writes() {
        let inner = Arc::new(CountingStore {
            tokens: Mutex::new(Some(tokens("initial"))),
            ..Default::default()
        });
        let store = CachedTokenStore::new(Arc::clone(&inner));

        assert_eq!(store.load().unwrap(), Some(tokens("initial")));
        assert_eq!(store.load().unwrap(), Some(tokens("initial")));
        assert_eq!(inner.loads.load(Ordering::Relaxed), 1);

        store.save(&tokens("saved")).unwrap();
        assert_eq!(store.load().unwrap(), Some(tokens("saved")));
        assert_eq!(inner.loads.load(Ordering::Relaxed), 1);
        assert_eq!(inner.saves.load(Ordering::Relaxed), 1);

        store.clear().unwrap();
        assert_eq!(store.load().unwrap(), None);
        assert_eq!(inner.loads.load(Ordering::Relaxed), 1);
        assert_eq!(inner.clears.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn failed_save_keeps_cached_tokens() {
        let inner = Arc::new(CountingStore {
            tokens: Mutex::new(Some(tokens("initial"))),
            ..Default::default()
        });
        let store = CachedTokenStore::new(Arc::clone(&inner));
        assert_eq!(store.load().unwrap(), Some(tokens("initial")));
        inner.fail_save.store(true, Ordering::Relaxed);

        assert!(store.save(&tokens("failed")).is_err());
        assert_eq!(store.load().unwrap(), Some(tokens("initial")));
        assert_eq!(inner.loads.load(Ordering::Relaxed), 1);
        assert_eq!(inner.saves.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn legacy_record_defaults_scopes_to_empty() {
        let tokens: Tokens =
            serde_json::from_str(r#"{"access":"access","refresh":"refresh","expires_at":42}"#)
                .unwrap();

        assert!(tokens.scopes.is_empty());
    }

    #[test]
    fn legacy_grant_reports_playlist_scopes_missing() {
        let tokens = Tokens {
            scopes: "streaming user-read-private user-library-read user-library-modify user-read-playback-state user-modify-playback-state".into(),
            ..tokens("access")
        };

        assert_eq!(
            tokens.missing_scopes(),
            [
                "playlist-read-private",
                "playlist-read-collaborative",
                "playlist-modify-public",
                "playlist-modify-private",
                "user-follow-read",
                "user-follow-modify",
            ]
        );
    }

    #[test]
    fn scopes_request_string_matches_required_list() {
        assert_eq!(crate::auth::SCOPES, crate::auth::REQUIRED_SCOPES.join(" "));
    }

    #[test]
    fn current_grant_has_no_missing_scopes_regardless_of_order() {
        let tokens = Tokens {
            scopes: crate::auth::REQUIRED_SCOPES
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join(" "),
            ..tokens("access")
        };

        assert!(tokens.missing_scopes().is_empty());
    }

    struct CountingKeySource {
        key: [u8; 32],
        loads: Arc<AtomicUsize>,
    }

    impl KeySource for CountingKeySource {
        fn load_or_create(&self) -> Result<[u8; 32]> {
            self.loads.fetch_add(1, Ordering::Relaxed);
            Ok(self.key)
        }
    }

    fn encrypted_store(
        dir: &std::path::Path,
        key: [u8; 32],
        loads: Arc<AtomicUsize>,
    ) -> EncryptedFsTokenStore {
        EncryptedFsTokenStore::with_key_source(dir, CountingKeySource { key, loads })
    }

    #[test]
    fn encrypted_store_round_trips_clears_and_fetches_key_once() {
        let dir = tempfile::tempdir().unwrap();
        let loads = Arc::new(AtomicUsize::new(0));
        let store = encrypted_store(dir.path(), [7; 32], Arc::clone(&loads));

        store.clear().unwrap();
        assert_eq!(loads.load(Ordering::Relaxed), 0);
        assert_eq!(store.load().unwrap(), None);
        store.save(&tokens("first")).unwrap();
        assert_eq!(store.load().unwrap(), Some(tokens("first")));
        store.save(&tokens("second")).unwrap();
        assert_eq!(store.load().unwrap(), Some(tokens("second")));
        assert_eq!(loads.load(Ordering::Relaxed), 1);
        assert_eq!(
            fs::metadata(dir.path().join("tokens.enc"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        store.clear().unwrap();
        assert_eq!(loads.load(Ordering::Relaxed), 1);
        assert_eq!(store.load().unwrap(), None);
        store.clear().unwrap();
    }

    #[test]
    fn encrypted_store_uses_fresh_nonces_and_atomic_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let store = encrypted_store(dir.path(), [7; 32], Arc::new(AtomicUsize::new(0)));
        let path = dir.path().join("tokens.enc");

        store.save(&tokens("same")).unwrap();
        let first = fs::read(&path).unwrap();
        let first_inode = fs::metadata(&path).unwrap().ino();
        store.save(&tokens("same")).unwrap();
        let second = fs::read(&path).unwrap();

        assert_ne!(first, second);
        assert_ne!(first_inode, fs::metadata(path).unwrap().ino());
    }

    #[test]
    fn encrypted_store_treats_corrupt_or_wrong_key_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens.enc");
        let store = encrypted_store(dir.path(), [7; 32], Arc::new(AtomicUsize::new(0)));

        fs::write(&path, b"corrupt").unwrap();
        assert_eq!(store.load().unwrap(), None);

        store.save(&tokens("secret")).unwrap();
        let wrong_key = encrypted_store(dir.path(), [8; 32], Arc::new(AtomicUsize::new(0)));
        assert_eq!(wrong_key.load().unwrap(), None);

        let nonce = [1; NONCE_LEN];
        let ciphertext = ChaCha20Poly1305::new((&[7; 32]).into())
            .encrypt(Nonce::from_slice(&nonce), b"not json".as_ref())
            .unwrap();
        fs::write(&path, [nonce.as_slice(), ciphertext.as_slice()].concat()).unwrap();
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn migration_moves_legacy_tokens_only_when_encrypted_file_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let encrypted = encrypted_store(dir.path(), [7; 32], Arc::new(AtomicUsize::new(0)));
        let legacy = CountingStore {
            tokens: Mutex::new(Some(tokens("legacy"))),
            ..Default::default()
        };

        migrate_token_store(&legacy, &encrypted).unwrap();
        assert_eq!(encrypted.load().unwrap(), Some(tokens("legacy")));
        assert_eq!(legacy.loads.load(Ordering::Relaxed), 1);
        assert_eq!(legacy.clears.load(Ordering::Relaxed), 1);

        legacy.save(&tokens("new-legacy")).unwrap();
        migrate_token_store(&legacy, &encrypted).unwrap();
        assert_eq!(legacy.loads.load(Ordering::Relaxed), 1);
        assert_eq!(encrypted.load().unwrap(), Some(tokens("legacy")));
    }

    #[test]
    fn migration_leaves_empty_or_failed_legacy_store_untouched() {
        let empty_dir = tempfile::tempdir().unwrap();
        let empty_encrypted =
            encrypted_store(empty_dir.path(), [7; 32], Arc::new(AtomicUsize::new(0)));
        let empty = CountingStore::default();
        migrate_token_store(&empty, &empty_encrypted).unwrap();
        assert_eq!(empty.loads.load(Ordering::Relaxed), 1);
        assert_eq!(empty.clears.load(Ordering::Relaxed), 0);

        let failed_dir = tempfile::tempdir().unwrap();
        fs::create_dir(failed_dir.path().join("tokens.enc")).unwrap();
        let failed_encrypted =
            encrypted_store(failed_dir.path(), [7; 32], Arc::new(AtomicUsize::new(0)));
        let legacy = CountingStore {
            tokens: Mutex::new(Some(tokens("legacy"))),
            ..Default::default()
        };
        assert!(migrate_token_store(&legacy, &failed_encrypted).is_err());
        assert_eq!(legacy.clears.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn migration_removes_encrypted_file_when_legacy_clear_fails() {
        let dir = tempfile::tempdir().unwrap();
        let encrypted = encrypted_store(dir.path(), [7; 32], Arc::new(AtomicUsize::new(0)));
        let legacy = CountingStore {
            tokens: Mutex::new(Some(tokens("legacy"))),
            fail_clear: AtomicBool::new(true),
            ..Default::default()
        };

        assert!(migrate_token_store(&legacy, &encrypted).is_err());
        assert_eq!(legacy.load().unwrap(), Some(tokens("legacy")));
        assert_eq!(encrypted.load().unwrap(), None);
        assert!(!dir.path().join("tokens.enc").exists());
    }
}

use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chacha20poly1305::{
    ChaCha20Poly1305, Nonce,
    aead::{Aead, KeyInit},
};
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

const SERVICE: &str = "com.rianjs.retune";
const KEY_ACCOUNT: &str = "token-file-key";
const NONCE_LEN: usize = 12;
const MAX_TOKEN_FILE_BYTES: u64 = 1024 * 1024;

fn read_token_file(path: &Path) -> std::io::Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    let length = file.metadata()?.len();
    if length > MAX_TOKEN_FILE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "encrypted token file is oversized",
        ));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(length).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "encrypted token file is oversized",
        )
    })?);
    file.take(MAX_TOKEN_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_TOKEN_FILE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "encrypted token file is oversized",
        ));
    }
    Ok(bytes)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybackCredentials {
    pub username: String,
    #[serde(with = "base64_bytes")]
    pub auth_data: Vec<u8>,
}

mod base64_bytes {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        STANDARD
            .decode(String::deserialize(deserializer)?)
            .map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub access: String,
    pub refresh: String,
    /// Unix timestamp in seconds.
    pub expires_at: u64,
    #[serde(default)]
    pub scopes: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playback_credentials: Option<PlaybackCredentials>,
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
    /// Returns `Ok(None)` only when no token record exists.
    fn load(&self) -> Result<Option<Tokens>>;
    fn save(&self, tokens: &Tokens) -> Result<()>;
    fn clear(&self) -> Result<()>;
    /// Replaces the record only when its complete current value matches.
    fn replace_if_current(&self, expected: &Tokens, tokens: &Tokens) -> Result<bool>;
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

    fn replace_if_current(&self, expected: &Tokens, tokens: &Tokens) -> Result<bool> {
        (**self).replace_if_current(expected, tokens)
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

    fn replace_if_current(&self, expected: &Tokens, tokens: &Tokens) -> Result<bool> {
        (**self).replace_if_current(expected, tokens)
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
        let mut cache = self.cache.lock().map_err(token_error)?;
        if let Some(tokens) = cache.as_ref() {
            return Ok(tokens.clone());
        }
        let tokens = self.inner.load()?;
        *cache = Some(tokens.clone());
        Ok(tokens)
    }

    fn save(&self, tokens: &Tokens) -> Result<()> {
        let mut cache = self.cache.lock().map_err(token_error)?;
        self.inner.save(tokens)?;
        *cache = Some(Some(tokens.clone()));
        Ok(())
    }

    fn clear(&self) -> Result<()> {
        let mut cache = self.cache.lock().map_err(token_error)?;
        self.inner.clear()?;
        *cache = Some(None);
        Ok(())
    }

    fn replace_if_current(&self, expected: &Tokens, tokens: &Tokens) -> Result<bool> {
        let mut cache = self.cache.lock().map_err(token_error)?;
        if !self.inner.replace_if_current(expected, tokens)? {
            *cache = None;
            return Ok(false);
        }
        *cache = Some(Some(tokens.clone()));
        Ok(true)
    }
}

trait KeySource: Send + Sync {
    fn load_or_create(&self) -> Result<[u8; 32]>;
}

struct NativeKeySource {
    entry: keyring::Entry,
}

impl NativeKeySource {
    fn new() -> Result<Self> {
        keyring::Entry::new(SERVICE, KEY_ACCOUNT)
            .map(|entry| Self { entry })
            .map_err(token_error)
    }
}

impl KeySource for NativeKeySource {
    fn load_or_create(&self) -> Result<[u8; 32]> {
        log::debug!("Loading Spotify token file key from native credential store");
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
    key: OnceLock<[u8; 32]>,
    lifecycle: Mutex<()>,
}

impl EncryptedFsTokenStore {
    pub fn new(app_data_dir: impl AsRef<Path>) -> Result<Self> {
        Ok(Self::with_key_source(app_data_dir, NativeKeySource::new()?))
    }

    fn with_key_source(
        app_data_dir: impl AsRef<Path>,
        key_source: impl KeySource + 'static,
    ) -> Self {
        Self {
            path: app_data_dir.as_ref().join("tokens.enc"),
            key_source: Box::new(key_source),
            key: OnceLock::new(),
            lifecycle: Mutex::new(()),
        }
    }

    fn key(&self) -> Result<[u8; 32]> {
        if let Some(key) = self.key.get() {
            return Ok(*key);
        }
        let key = self.key_source.load_or_create()?;
        let _ = self.key.set(key);
        Ok(*self.key.get().unwrap_or(&key))
    }

    fn load_file(&self) -> Result<Option<Tokens>> {
        let bytes = match read_token_file(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                return Err(token_corrupt(error));
            }
            Err(error) => return Err(token_error(error)),
        };
        if bytes.len() < NONCE_LEN {
            return Err(token_corrupt("encrypted token file is truncated"));
        }

        let cipher = ChaCha20Poly1305::new((&self.key()?).into());
        let plaintext = cipher
            .decrypt(Nonce::from_slice(&bytes[..NONCE_LEN]), &bytes[NONCE_LEN..])
            .map_err(|_| token_corrupt("encrypted token authentication failed"))?;
        serde_json::from_slice(&plaintext)
            .map(Some)
            .map_err(token_corrupt)
    }

    fn save_file(&self, tokens: &Tokens) -> Result<()> {
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

    fn clear_file(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(token_error(error)),
        }
    }
}

impl TokenStore for EncryptedFsTokenStore {
    fn load(&self) -> Result<Option<Tokens>> {
        let _guard = self.lifecycle.lock().map_err(token_error)?;
        self.load_file()
    }

    fn save(&self, tokens: &Tokens) -> Result<()> {
        let _guard = self.lifecycle.lock().map_err(token_error)?;
        self.save_file(tokens)
    }

    fn clear(&self) -> Result<()> {
        let _guard = self.lifecycle.lock().map_err(token_error)?;
        self.clear_file()
    }

    fn replace_if_current(&self, expected: &Tokens, tokens: &Tokens) -> Result<bool> {
        let _guard = self.lifecycle.lock().map_err(token_error)?;
        if self.load_file()?.as_ref() != Some(expected) {
            return Ok(false);
        }
        self.save_file(tokens)?;
        Ok(true)
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::TokenStore("token file path has no parent".into()))?;
    fs::create_dir_all(parent).map_err(token_error)?;
    let temporary = path.with_extension(format!("tmp-{}", rand::random::<u64>()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temporary)?;
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

fn token_corrupt(error: impl std::fmt::Display) -> Error {
    Error::TokenStoreCorrupt(error.to_string())
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
        Ok(self.0.lock().map_err(token_error)?.clone())
    }

    fn save(&self, tokens: &Tokens) -> Result<()> {
        *self.0.lock().map_err(token_error)? = Some(tokens.clone());
        Ok(())
    }

    fn clear(&self) -> Result<()> {
        *self.0.lock().map_err(token_error)? = None;
        Ok(())
    }

    fn replace_if_current(&self, expected: &Tokens, tokens: &Tokens) -> Result<bool> {
        let mut current = self.0.lock().map_err(token_error)?;
        if current.as_ref() != Some(expected) {
            return Ok(false);
        }
        *current = Some(tokens.clone());
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{
            Barrier,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    use super::*;

    #[derive(Default)]
    struct CountingStore {
        tokens: Mutex<Option<Tokens>>,
        loads: AtomicUsize,
        saves: AtomicUsize,
        clears: AtomicUsize,
        fail_save: AtomicBool,
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
            *self.tokens.lock().unwrap() = None;
            Ok(())
        }

        fn replace_if_current(&self, expected: &Tokens, tokens: &Tokens) -> Result<bool> {
            let mut current = self.tokens.lock().unwrap();
            if current.as_ref() != Some(expected) {
                return Ok(false);
            }
            *current = Some(tokens.clone());
            Ok(true)
        }
    }

    struct BlockingSaveStore {
        tokens: Mutex<Option<Tokens>>,
        entered: Arc<Barrier>,
        release: Arc<Barrier>,
    }

    impl TokenStore for BlockingSaveStore {
        fn load(&self) -> Result<Option<Tokens>> {
            Ok(self.tokens.lock().unwrap().clone())
        }

        fn save(&self, tokens: &Tokens) -> Result<()> {
            *self.tokens.lock().unwrap() = Some(tokens.clone());
            self.entered.wait();
            self.release.wait();
            Ok(())
        }

        fn clear(&self) -> Result<()> {
            *self.tokens.lock().unwrap() = None;
            Ok(())
        }

        fn replace_if_current(&self, expected: &Tokens, tokens: &Tokens) -> Result<bool> {
            let mut current = self.tokens.lock().unwrap();
            if current.as_ref() != Some(expected) {
                return Ok(false);
            }
            *current = Some(tokens.clone());
            Ok(true)
        }
    }

    fn tokens(access: &str) -> Tokens {
        Tokens {
            access: access.into(),
            refresh: "refresh".into(),
            expires_at: 42,
            scopes: "streaming".into(),
            playback_credentials: None,
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
            playback_credentials: None,
        };
        store.save(&tokens).unwrap();
        assert_eq!(store.load().unwrap(), Some(tokens));
        store.clear().unwrap();
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    fn native_credential_builder_matches_platform() {
        let builder = keyring::default::default_credential_builder();

        #[cfg(target_os = "macos")]
        assert!(
            builder
                .as_any()
                .is::<keyring::macos::MacCredentialBuilder>()
        );
        #[cfg(target_os = "windows")]
        assert!(
            builder
                .as_any()
                .is::<keyring::windows::WinCredentialBuilder>()
        );
        #[cfg(target_os = "linux")]
        assert!(
            builder
                .as_any()
                .is::<keyring::secret_service::SsCredentialBuilder>()
        );
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
    fn cached_save_and_clear_commit_backing_and_cache_together() {
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let inner = Arc::new(BlockingSaveStore {
            tokens: Mutex::new(Some(tokens("initial"))),
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        });
        let store = Arc::new(CachedTokenStore::new(Arc::clone(&inner)));
        assert_eq!(store.load().unwrap(), Some(tokens("initial")));

        let saving = {
            let store = Arc::clone(&store);
            std::thread::spawn(move || store.save(&tokens("saved")).unwrap())
        };
        entered.wait();
        assert!(store.cache.try_lock().is_err());
        let clearing = {
            let store = Arc::clone(&store);
            std::thread::spawn(move || store.clear().unwrap())
        };
        release.wait();
        saving.join().unwrap();
        clearing.join().unwrap();

        assert_eq!(store.load().unwrap(), None);
        assert_eq!(inner.load().unwrap(), None);
    }

    #[test]
    fn cached_conditional_replace_checks_backing_not_a_stale_cache() {
        let inner = Arc::new(CountingStore {
            tokens: Mutex::new(Some(tokens("initial"))),
            ..Default::default()
        });
        let store = CachedTokenStore::new(Arc::clone(&inner));
        let initial = tokens("initial");
        assert_eq!(store.load().unwrap(), Some(initial.clone()));
        inner.save(&tokens("replacement")).unwrap();

        assert!(
            !store
                .replace_if_current(&initial, &tokens("stale"))
                .unwrap()
        );
        assert_eq!(store.load().unwrap(), Some(tokens("replacement")));
        assert_eq!(inner.load().unwrap(), Some(tokens("replacement")));
    }

    #[test]
    fn legacy_record_defaults_scopes_to_empty() {
        let tokens: Tokens =
            serde_json::from_str(r#"{"access":"access","refresh":"refresh","expires_at":42}"#)
                .unwrap();

        assert!(tokens.scopes.is_empty());
        assert!(tokens.playback_credentials.is_none());
    }

    #[test]
    fn playback_credentials_round_trip_as_base64() {
        let tokens = Tokens {
            playback_credentials: Some(PlaybackCredentials {
                username: "user".into(),
                auth_data: vec![0, 1, 2, 254, 255],
            }),
            ..tokens("access")
        };

        let serialized = serde_json::to_string(&tokens).unwrap();
        assert!(serialized.contains("AAEC/v8="));
        assert_eq!(serde_json::from_str::<Tokens>(&serialized).unwrap(), tokens);
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
                "user-read-playback-position",
                "playlist-read-private",
                "playlist-read-collaborative",
                "playlist-modify-public",
                "playlist-modify-private",
            ]
        );
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

    struct FailOnceKeySource {
        key: [u8; 32],
        loads: Arc<AtomicUsize>,
    }

    impl KeySource for FailOnceKeySource {
        fn load_or_create(&self) -> Result<[u8; 32]> {
            if self.loads.fetch_add(1, Ordering::Relaxed) == 0 {
                Err(Error::TokenStore("credential store unavailable".into()))
            } else {
                Ok(self.key)
            }
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
        let first = Tokens {
            playback_credentials: Some(PlaybackCredentials {
                username: "user".into(),
                auth_data: vec![1, 2, 3],
            }),
            ..tokens("first")
        };
        store.save(&first).unwrap();
        assert_eq!(store.load().unwrap(), Some(first));
        store.save(&tokens("second")).unwrap();
        assert_eq!(store.load().unwrap(), Some(tokens("second")));
        assert_eq!(loads.load(Ordering::Relaxed), 1);
        #[cfg(unix)]
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
    fn encrypted_store_retries_a_transient_key_failure_and_caches_success() {
        let dir = tempfile::tempdir().unwrap();
        let loads = Arc::new(AtomicUsize::new(0));
        let store = EncryptedFsTokenStore::with_key_source(
            dir.path(),
            FailOnceKeySource {
                key: [7; 32],
                loads: Arc::clone(&loads),
            },
        );

        assert!(store.save(&tokens("first")).is_err());
        store.save(&tokens("second")).unwrap();
        assert_eq!(store.load().unwrap(), Some(tokens("second")));
        assert_eq!(loads.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn encrypted_store_uses_fresh_nonces_and_atomic_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let store = encrypted_store(dir.path(), [7; 32], Arc::new(AtomicUsize::new(0)));
        let path = dir.path().join("tokens.enc");

        store.save(&tokens("same")).unwrap();
        let first = fs::read(&path).unwrap();
        #[cfg(unix)]
        let first_inode = fs::metadata(&path).unwrap().ino();
        store.save(&tokens("same")).unwrap();
        let second = fs::read(&path).unwrap();

        assert_ne!(first, second);
        #[cfg(unix)]
        assert_ne!(first_inode, fs::metadata(path).unwrap().ino());
    }

    #[test]
    fn encrypted_store_reports_corrupt_wrong_key_and_io_distinctly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens.enc");
        let store = encrypted_store(dir.path(), [7; 32], Arc::new(AtomicUsize::new(0)));

        fs::write(&path, b"corrupt").unwrap();
        assert!(matches!(store.load(), Err(Error::TokenStoreCorrupt(_))));

        store.save(&tokens("secret")).unwrap();
        let wrong_key = encrypted_store(dir.path(), [8; 32], Arc::new(AtomicUsize::new(0)));
        assert!(matches!(wrong_key.load(), Err(Error::TokenStoreCorrupt(_))));

        let nonce = [1; NONCE_LEN];
        let ciphertext = ChaCha20Poly1305::new((&[7; 32]).into())
            .encrypt(Nonce::from_slice(&nonce), b"not json".as_ref())
            .unwrap();
        fs::write(&path, [nonce.as_slice(), ciphertext.as_slice()].concat()).unwrap();
        assert!(matches!(store.load(), Err(Error::TokenStoreCorrupt(_))));

        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(matches!(store.load(), Err(Error::TokenStore(_))));

        fs::remove_dir(&path).unwrap();
        fs::File::create(&path)
            .unwrap()
            .set_len(MAX_TOKEN_FILE_BYTES + 1)
            .unwrap();
        assert!(matches!(store.load(), Err(Error::TokenStoreCorrupt(_))));
        assert!(path.exists());
    }
}

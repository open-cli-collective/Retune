use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

const SERVICE: &str = "com.rianjs.retune";
const ACCOUNT: &str = "spotify-oauth";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub access: String,
    pub refresh: String,
    /// Unix timestamp in seconds.
    pub expires_at: u64,
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
        keyring::Entry::new(SERVICE, ACCOUNT)
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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
    }

    fn tokens(access: &str) -> Tokens {
        Tokens {
            access: access.into(),
            refresh: "refresh".into(),
            expires_at: 42,
        }
    }

    #[test]
    fn memory_store_round_trip_and_clear() {
        let store = InMemoryTokenStore::default();
        let tokens = Tokens {
            access: "access".into(),
            refresh: "refresh".into(),
            expires_at: 42,
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
}

use std::sync::Mutex;

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
    use super::*;

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
}

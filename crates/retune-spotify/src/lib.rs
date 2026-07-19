//! Offline-testable Spotify Web API adapter.

pub mod auth;
pub mod client;
pub mod normalize;
pub mod tokens;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("OAuth callback error: {0}")]
    Callback(String),
    #[error("OAuth state mismatch")]
    StateMismatch,
    #[error("OAuth callback timed out")]
    Timeout,
    #[error("Spotify authorization was denied: {0}")]
    AccessDenied(String),
    #[error("HTTP transport error: {0}")]
    Transport(String),
    #[error("Spotify {endpoint} returned HTTP {status}: {body}")]
    Http {
        endpoint: String,
        status: u16,
        body: String,
    },
    #[error("invalid JSON from Spotify {endpoint}: {source}")]
    Json {
        endpoint: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("token store error: {0}")]
    TokenStore(String),
    #[error("no Spotify tokens are stored")]
    MissingToken,
    #[error("Spotify refresh response did not contain a refresh token or a stored fallback")]
    MissingRefreshToken,
    #[error("invalid request: {0}")]
    InvalidRequest(String),
}

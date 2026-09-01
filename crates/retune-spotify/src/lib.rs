//! Offline-testable Spotify Web API adapter.

pub mod auth;
pub mod catalog;
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
    #[error("Spotify token request timed out")]
    TokenRequestTimeout,
    #[error("Spotify authorization was denied: {0}")]
    AccessDenied(String),
    #[error("HTTP transport error: {0}")]
    Transport(String),
    #[error("HTTP transport error: {source}")]
    TransportSource {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("Spotify {endpoint} returned HTTP {status}: {body}")]
    Http {
        endpoint: String,
        status: u16,
        body: String,
    },
    #[error("Spotify rate limited {endpoint}; retry after {retry_after_secs}s")]
    RateLimited {
        endpoint: String,
        retry_after_secs: u64,
    },
    #[error("Spotify Development Mode quota exhausted for {endpoint}")]
    QuotaExceeded {
        endpoint: String,
        retry_after_secs: Option<u64>,
    },
    #[error("Spotify {endpoint} failed with HTTP {status} after retries")]
    ServerError { endpoint: String, status: u16 },
    #[error("Spotify mutation {endpoint} may have succeeded; reconcile before retrying ({detail})")]
    AmbiguousMutation {
        endpoint: String,
        status: Option<u16>,
        detail: String,
        #[source]
        source: Option<Box<Error>>,
    },
    #[error("invalid JSON from Spotify {endpoint}: {source}")]
    Json {
        endpoint: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("token store error: {0}")]
    TokenStore(String),
    #[error("stored Spotify tokens are corrupt or undecryptable: {0}")]
    TokenStoreCorrupt(String),
    #[error("no Spotify tokens are stored")]
    MissingToken,
    #[error("invalid request: {0}")]
    InvalidRequest(String),
}

impl Error {
    pub fn endpoint(&self) -> Option<&str> {
        match self {
            Self::Http { endpoint, .. }
            | Self::RateLimited { endpoint, .. }
            | Self::QuotaExceeded { endpoint, .. }
            | Self::ServerError { endpoint, .. }
            | Self::AmbiguousMutation { endpoint, .. }
            | Self::Json { endpoint, .. } => Some(endpoint),
            _ => None,
        }
    }

    pub fn ambiguous_outcome(&self) -> bool {
        matches!(self, Self::AmbiguousMutation { .. })
    }
}

pub(crate) fn bounded_error_body(body: &[u8]) -> String {
    const LIMIT: usize = 4 * 1024;
    let truncated = body.len() > LIMIT;
    let mut value = String::from_utf8_lossy(&body[..body.len().min(LIMIT)]).into_owned();
    if truncated {
        value.push_str("… [truncated]");
    }
    value
}

#[cfg(test)]
mod tests {
    #[test]
    fn error_body_keeps_useful_prefix_with_a_small_bound() {
        let body = vec![b'x'; 8 * 1024];
        let bounded = super::bounded_error_body(&body);

        assert!(bounded.starts_with("xxxx"));
        assert!(bounded.ends_with("… [truncated]"));
        assert!(bounded.len() < body.len());
    }
}

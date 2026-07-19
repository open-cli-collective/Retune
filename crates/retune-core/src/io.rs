//! Versioned serialization of the overlay library: export/import for
//! backup (`.json`), compressed export (`.json.gz`), restore, and merge.
//! Pure bytes-in/bytes-out — file paths and the filesystem live in the shell.

use crate::model::Library;

/// Current schema version written by [`export_json`].
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("not a Retune library export (missing version envelope)")]
    MissingEnvelope,
    #[error("schema version {0} is newer than this app understands ({SCHEMA_VERSION})")]
    FromTheFuture(u32),
    #[error("gzip: {0}")]
    Gzip(std::io::Error),
}

/// Serializes as `{"version": 1, "library": { … }}`.
pub fn export_json(_library: &Library) -> Vec<u8> {
    todo!()
}

/// [`export_json`], gzip-compressed.
pub fn export_json_gz(_library: &Library) -> Vec<u8> {
    todo!()
}

/// Accepts the output of either export function — sniffs the gzip magic
/// bytes. Runs version migrations so any older schema still loads; a
/// fixture test pins that v1 files load forever.
pub fn import(_bytes: &[u8]) -> Result<Library, ImportError> {
    todo!()
}

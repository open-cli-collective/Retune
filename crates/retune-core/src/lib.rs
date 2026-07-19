//! Retune's pure overlay domain: the user's local metadata layered over
//! provider track records. No I/O, no async, no provider names.
//!
//! Layout-neutral by design: [`Library`] owns records and overlay edits;
//! [`browse`] is one projection of those records (the three-column
//! Genre/Artist/Album hierarchy). Alternate layouts are alternate projections.

pub mod browse;
pub mod io;
pub mod model;

pub use model::{AlbumKey, Library, Rating, SourceId, TrackId, TrackRecord};

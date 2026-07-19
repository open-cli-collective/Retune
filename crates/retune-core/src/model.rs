use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Stable local identifier, assigned when a record enters the library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TrackId(pub u64);

/// Media-type source. Adding a media type is a new variant + a label map in
/// the UI layer — never a new layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceId {
    Music,
    Podcasts,
    Audiobooks,
}

/// A star rating, 1..=5. Construction is validated; storage is infallible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct Rating(u8);

impl Rating {
    pub fn new(stars: u8) -> Option<Self> {
        (1..=5).contains(&stars).then_some(Self(stars))
    }

    pub fn stars(self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for Rating {
    type Error = String;

    fn try_from(v: u8) -> Result<Self, Self::Error> {
        Rating::new(v).ok_or_else(|| format!("rating out of range 1..=5: {v}"))
    }
}

impl From<Rating> for u8 {
    fn from(r: Rating) -> u8 {
        r.0
    }
}

/// One overlay track record. `cat`/`art`/`alb` are the three generic facets
/// ("category / artist-field / album-field"); what they're *called* is a
/// per-source UI concern (Music: Genre/Artist/Album, Podcasts:
/// Category/Podcaster/Show, ...).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackRecord {
    pub id: TrackId,
    /// Provider URI (e.g. `spotify:track:…`). Carries the media kind and is
    /// the dedupe key for merge and all provider-facing operations.
    pub uri: String,
    pub source: SourceId,
    /// Overlay-normalized category (genre for music).
    pub cat: String,
    pub art: String,
    pub alb: String,
    pub name: String,
    pub duration: Duration,
    /// Per-track rating override. `None` means "inherit from album".
    pub rating: Option<Rating>,
    /// The provider's original category, recorded the first time `cat`
    /// diverges from it. The "●" override marker shows iff
    /// `orig_cat.is_some() && cat != orig_cat`.
    pub orig_cat: Option<String>,
}

/// Album identity is deliberately the (source, artist-text, album-text)
/// tuple — iTunes semantics. Editing a track's `art`/`alb` re-parents it;
/// normalizing two editions to one name merges their groups. See docs/PLAN.md.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AlbumKey {
    pub source: SourceId,
    pub art: String,
    pub alb: String,
}

impl AlbumKey {
    pub fn of(track: &TrackRecord) -> Self {
        Self {
            source: track.source,
            art: track.art.clone(),
            alb: track.alb.clone(),
        }
    }
}

/// The user's overlay library: all track records plus album-level ratings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Library {
    tracks: Vec<TrackRecord>,
    album_ratings: BTreeMap<AlbumKey, Rating>,
    next_id: u64,
}

impl Library {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tracks(&self) -> &[TrackRecord] {
        &self.tracks
    }

    pub fn get(&self, _id: TrackId) -> Option<&TrackRecord> {
        todo!()
    }

    /// Adds a record for `uri` if absent, assigning a fresh [`TrackId`].
    /// If a record with the same `uri` exists, it is left untouched (its
    /// overlay edits win) and its id is returned.
    pub fn add(&mut self, _incoming: NewTrack) -> TrackId {
        todo!()
    }

    /// Edits the user-facing fields of one track (Get Info). Setting `cat`
    /// to a new value records `orig_cat` on first divergence; setting it
    /// back to the original leaves `orig_cat` in place (the marker equality
    /// check hides it).
    pub fn edit(&mut self, _id: TrackId, _edit: TrackEdit) -> Result<(), UnknownTrack> {
        todo!()
    }

    /// Star-click semantics on a track: clicking always sets an explicit
    /// override at `stars` — even if it equals the inherited value — except
    /// that clicking the value matching the current *explicit* override
    /// clears it (reverts to inherited).
    pub fn click_track_star(&mut self, _id: TrackId, _stars: Rating) -> Result<(), UnknownTrack> {
        todo!()
    }

    pub fn set_album_rating(&mut self, _key: AlbumKey, _rating: Option<Rating>) {
        todo!()
    }

    pub fn album_rating(&self, _key: &AlbumKey) -> Option<Rating> {
        todo!()
    }

    /// Effective rating = track override ?? album rating ?? unrated.
    pub fn effective_rating(&self, _id: TrackId) -> Option<EffectiveRating> {
        todo!()
    }

    /// Replaces this library wholesale (File → Restore).
    pub fn restore(&mut self, _other: Library) {
        todo!()
    }

    /// Additive import (File → Merge): dedupe by `uri`, existing records win
    /// and keep their overlay edits; album ratings merge the same way
    /// (existing keys win).
    pub fn merge(&mut self, _other: Library) {
        todo!()
    }
}

/// A record as it arrives from a provider sync or an import — everything but
/// the local id, which [`Library::add`] assigns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTrack {
    pub uri: String,
    pub source: SourceId,
    pub cat: String,
    pub art: String,
    pub alb: String,
    pub name: String,
    pub duration: Duration,
}

/// Field edits from Get Info. `None` = leave unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackEdit {
    pub name: Option<String>,
    pub art: Option<String>,
    pub alb: Option<String>,
    pub cat: Option<String>,
}

/// An effective rating plus where it came from (drives gold vs. muted stars).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveRating {
    Explicit(Rating),
    Inherited(Rating),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("unknown track id {0:?}")]
pub struct UnknownTrack(pub TrackId);

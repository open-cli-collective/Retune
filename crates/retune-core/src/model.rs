use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Sentinel genre for tracks whose provider metadata carries no genre.
pub const UNCATEGORIZED: &str = "Uncategorized";

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
    /// Whether this track participates in ordinary sequential playback.
    /// Missing serialized values default on; only exclusions occupy the overlay.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_no: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disc_no: Option<u32>,
    #[serde(default)]
    pub play_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_played_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate_kbps: Option<u32>,
    /// Per-track rating override. `None` means "inherit from album".
    pub rating: Option<Rating>,
    /// The provider's original category, recorded the first time `cat`
    /// diverges from it. The "●" override marker shows iff
    /// `orig_cat.is_some() && cat != orig_cat`.
    pub orig_cat: Option<String>,
}

/// Album identity is deliberately the (source, artist-text, album-text)
/// tuple — iTunes semantics. Editing a track's `art`/`alb` re-parents it;
/// normalizing two editions to one name merges their groups. See
/// docs/architecture/library.md.
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
    #[serde(with = "album_rating_serde")]
    album_ratings: BTreeMap<AlbumKey, Rating>,
    next_id: u64,
}

mod album_rating_serde {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::{AlbumKey, Rating};

    pub fn serialize<S>(
        ratings: &BTreeMap<AlbumKey, Rating>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        ratings.iter().collect::<Vec<_>>().serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<BTreeMap<AlbumKey, Rating>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Vec::<(AlbumKey, Rating)>::deserialize(deserializer)
            .map(|entries| entries.into_iter().collect())
    }
}

impl Library {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tracks(&self) -> &[TrackRecord] {
        &self.tracks
    }

    pub fn tracks_mut(&mut self) -> &mut [TrackRecord] {
        &mut self.tracks
    }

    pub fn get(&self, id: TrackId) -> Option<&TrackRecord> {
        self.tracks.iter().find(|track| track.id == id)
    }

    fn track_mut(&mut self, id: TrackId) -> Result<&mut TrackRecord, UnknownTrack> {
        self.tracks
            .iter_mut()
            .find(|track| track.id == id)
            .ok_or(UnknownTrack(id))
    }

    /// Adds a record for `uri` if absent, assigning a fresh [`TrackId`].
    /// If a record with the same `uri` exists, it is left untouched (its
    /// overlay edits win) and its id is returned.
    pub fn add(&mut self, incoming: NewTrack) -> TrackId {
        if let Some(track) = self.tracks.iter().find(|track| track.uri == incoming.uri) {
            return track.id;
        }

        let id = self.fresh_id();
        self.tracks.push(TrackRecord {
            id,
            uri: incoming.uri,
            source: incoming.source,
            cat: incoming.cat,
            art: incoming.art,
            alb: incoming.alb,
            name: incoming.name,
            duration: incoming.duration,
            enabled: true,
            track_no: incoming.track_no,
            disc_no: incoming.disc_no,
            play_count: 0,
            last_played_at: None,
            added_at: incoming.added_at,
            release_date: incoming.release_date,
            kind: incoming.kind,
            bitrate_kbps: incoming.bitrate_kbps,
            rating: None,
            orig_cat: None,
        });
        id
    }

    /// Adds a new record or refreshes the provider category of an existing one.
    pub fn upsert(&mut self, incoming: NewTrack) -> TrackId {
        if let Some(track) = self
            .tracks
            .iter_mut()
            .find(|track| track.uri == incoming.uri)
        {
            track.track_no = incoming.track_no;
            track.disc_no = incoming.disc_no;
            track.kind = incoming.kind;
            track.bitrate_kbps = incoming.bitrate_kbps;
            track.release_date = incoming.release_date;
            track.added_at = match (track.added_at, incoming.added_at) {
                (Some(existing), Some(discovered)) => Some(existing.min(discovered)),
                (None, discovered) => discovered,
                (existing, None) => existing,
            };
            if track.orig_cat.is_some() {
                track.orig_cat = Some(incoming.cat);
            } else {
                track.cat = incoming.cat;
            }
            return track.id;
        }
        self.add(incoming)
    }

    /// Edits the user-facing fields of one track (Get Info). Setting `cat`
    /// to a new value records `orig_cat` on first divergence; setting it
    /// back to the original leaves `orig_cat` in place (the marker equality
    /// check hides it).
    pub fn edit(&mut self, id: TrackId, edit: TrackEdit) -> Result<(), UnknownTrack> {
        let track = self.track_mut(id)?;
        if let Some(name) = edit.name {
            track.name = name;
        }
        if let Some(art) = edit.art {
            track.art = art;
        }
        if let Some(alb) = edit.alb {
            track.alb = alb;
        }
        if let Some(cat) = edit.cat {
            if track.orig_cat.is_none() && cat != track.cat {
                track.orig_cat = Some(track.cat.clone());
            }
            track.cat = cat;
        }
        Ok(())
    }

    /// Star-click semantics on a track: clicking always sets an explicit
    /// override at `stars` — even if it equals the inherited value — except
    /// that clicking the value matching the current *explicit* override
    /// clears it (reverts to inherited).
    pub fn click_track_star(&mut self, id: TrackId, stars: Rating) -> Result<(), UnknownTrack> {
        let track = self.track_mut(id)?;
        track.rating = (track.rating != Some(stars)).then_some(stars);
        Ok(())
    }

    /// Directly sets or clears a track's explicit override — the Get Info
    /// form semantic, distinct from [`Self::click_track_star`]'s toggle.
    pub fn set_track_rating(
        &mut self,
        id: TrackId,
        rating: Option<Rating>,
    ) -> Result<(), UnknownTrack> {
        let track = self.track_mut(id)?;
        track.rating = rating;
        Ok(())
    }

    pub fn set_track_enabled(&mut self, id: TrackId, enabled: bool) -> Result<(), UnknownTrack> {
        let track = self.track_mut(id)?;
        track.enabled = enabled;
        Ok(())
    }

    pub fn set_album_rating(&mut self, key: AlbumKey, rating: Option<Rating>) {
        match rating {
            Some(rating) => {
                self.album_ratings.insert(key, rating);
            }
            None => {
                self.album_ratings.remove(&key);
            }
        }
    }

    pub fn album_rating(&self, key: &AlbumKey) -> Option<Rating> {
        self.album_ratings.get(key).copied()
    }

    /// Effective rating = track override ?? album rating ?? unrated.
    pub fn effective_rating(&self, id: TrackId) -> Option<EffectiveRating> {
        let track = self.get(id)?;
        track.rating.map(EffectiveRating::Explicit).or_else(|| {
            self.album_rating(&AlbumKey::of(track))
                .map(EffectiveRating::Inherited)
        })
    }

    /// Replaces this library wholesale (File → Restore).
    pub fn restore(&mut self, other: Library) {
        *self = other;
    }

    /// Additive import (File → Merge): dedupe by `uri`, existing records win
    /// and keep their overlay edits; album ratings merge the same way
    /// (existing keys win).
    pub fn merge(&mut self, other: Library) {
        for mut track in other.tracks {
            if self.tracks.iter().any(|existing| existing.uri == track.uri) {
                continue;
            }
            track.id = self.fresh_id();
            self.tracks.push(track);
        }
        for (key, rating) in other.album_ratings {
            self.album_ratings.entry(key).or_insert(rating);
        }
    }

    pub fn remove_uris(&mut self, uris: &[String]) -> usize {
        let uris = uris
            .iter()
            .map(String::as_str)
            .collect::<std::collections::HashSet<_>>();
        let albums = self
            .tracks
            .iter()
            .filter(|track| uris.contains(track.uri.as_str()))
            .map(AlbumKey::of)
            .collect::<std::collections::BTreeSet<_>>();
        let before = self.tracks.len();
        self.tracks
            .retain(|track| !uris.contains(track.uri.as_str()));
        self.album_ratings.retain(|album, _| {
            !albums.contains(album)
                || self
                    .tracks
                    .iter()
                    .any(|track| AlbumKey::of(track) == *album)
        });
        before - self.tracks.len()
    }

    /// Restores invariants on a freshly deserialized library. Duplicate ids
    /// or URIs mean a corrupt or hand-tampered file; `next_id` is recomputed
    /// so a stale stored value can never mint colliding ids.
    pub(crate) fn validate_imported(&mut self) -> Result<(), String> {
        let mut ids = std::collections::HashSet::new();
        let mut uris = std::collections::HashSet::new();
        for track in &self.tracks {
            if !ids.insert(track.id) {
                return Err(format!("duplicate track id {}", track.id.0));
            }
            if !uris.insert(track.uri.as_str()) {
                return Err(format!("duplicate track uri {:?}", track.uri));
            }
        }
        let past_max = match self.tracks.iter().map(|track| track.id.0).max() {
            None => 0,
            Some(max) => max
                .checked_add(1)
                .ok_or_else(|| format!("track id {max} exhausts the id space"))?,
        };
        // Recomputed outright so deleted ids can be skipped safely and a
        // hostile stored value like u64::MAX is neutralized.
        self.next_id = past_max;
        Ok(())
    }

    fn fresh_id(&mut self) -> TrackId {
        let id = TrackId(self.next_id);
        self.next_id = self.next_id.checked_add(1).expect("track ids exhausted");
        id
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
    pub track_no: Option<u32>,
    pub disc_no: Option<u32>,
    pub added_at: Option<u64>,
    pub release_date: Option<String>,
    pub kind: Option<String>,
    pub bitrate_kbps: Option<u32>,
}

// Manual impl: `SourceId` has no meaningful `Default` to derive from.
impl Default for NewTrack {
    fn default() -> Self {
        Self {
            uri: String::new(),
            source: SourceId::Music,
            cat: String::new(),
            art: String::new(),
            alb: String::new(),
            name: String::new(),
            duration: Duration::ZERO,
            track_no: None,
            disc_no: None,
            added_at: None,
            release_date: None,
            kind: None,
            bitrate_kbps: None,
        }
    }
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

fn default_true() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(uri: &str, cat: &str, art: &str, alb: &str) -> NewTrack {
        NewTrack {
            uri: uri.into(),
            cat: cat.into(),
            art: art.into(),
            alb: alb.into(),
            name: uri.into(),
            duration: Duration::from_secs(180),
            ..NewTrack::default()
        }
    }

    fn rating(stars: u8) -> Rating {
        Rating::new(stars).unwrap()
    }

    #[test]
    fn add_deduplicates_uri_and_assigns_stable_unique_ids() {
        let mut library = Library::new();
        let first = library.add(track("one", "Rock", "Artist", "Album"));
        let duplicate = library.add(track("one", "Jazz", "Other", "Changed"));
        let second = library.add(track("two", "Rock", "Artist", "Album"));

        assert_eq!(duplicate, first);
        assert_ne!(second, first);
        assert_eq!(library.tracks().len(), 2);
        assert_eq!(library.get(first).unwrap().cat, "Rock");
    }

    #[test]
    fn upsert_inserts_new_track() {
        let mut library = Library::new();
        let id = library.upsert(track("one", "Rock", "Artist", "Album"));

        assert_eq!(library.tracks().len(), 1);
        assert_eq!(library.get(id).unwrap().cat, "Rock");
    }

    #[test]
    fn upsert_refreshes_unedited_category_without_other_changes() {
        let mut library = Library::new();
        let id = library.add(track("one", "Rock", "Artist", "Album"));
        library.set_track_rating(id, Some(rating(5))).unwrap();
        library.set_track_enabled(id, false).unwrap();

        let mut changed = track("one", "Metal", "Other", "Changed");
        changed.name = "Changed name".into();
        let existing = library.upsert(changed);

        assert_eq!(existing, id);
        let record = library.get(id).unwrap();
        assert_eq!(record.cat, "Metal");
        assert_eq!(record.name, "one");
        assert_eq!(record.rating, Some(rating(5)));
        assert!(!record.enabled);
    }

    #[test]
    fn upsert_refreshes_provider_track_numbers() {
        let mut library = Library::new();
        let id = library.add(track("one", "Rock", "Artist", "Album"));
        let mut changed = track("one", "Rock", "Artist", "Album");
        changed.track_no = Some(4);
        changed.disc_no = Some(2);

        library.upsert(changed);

        let record = library.get(id).unwrap();
        assert_eq!((record.disc_no, record.track_no), (Some(2), Some(4)));
    }

    #[test]
    fn upsert_refreshes_provider_metadata_and_preserves_play_overlay() {
        let mut library = Library::new();
        let mut original = track("one", "Rock", "Artist", "Album");
        original.added_at = Some(100);
        let id = library.upsert(original);
        let existing = &mut library.tracks_mut()[0];
        existing.play_count = 9;
        existing.last_played_at = Some(123);

        let mut changed = track("one", "Rock", "Artist", "Album");
        changed.added_at = Some(200);
        changed.release_date = Some("2024-02-03".into());
        changed.kind = Some("MPEG audio file".into());
        changed.bitrate_kbps = Some(192);
        library.upsert(changed);

        let track = library.get(id).unwrap();
        assert_eq!((track.play_count, track.last_played_at), (9, Some(123)));
        assert_eq!(track.added_at, Some(100));
        assert_eq!(track.release_date.as_deref(), Some("2024-02-03"));
        assert_eq!(track.kind.as_deref(), Some("MPEG audio file"));
        assert_eq!(track.bitrate_kbps, Some(192));
    }

    #[test]
    fn upsert_backfills_added_time_and_keeps_the_earliest_known_value() {
        let mut library = Library::new();
        let id = library.add(track("one", "Rock", "Artist", "Album"));

        let mut discovered = track("one", "Rock", "Artist", "Album");
        discovered.added_at = Some(200);
        library.upsert(discovered);
        assert_eq!(library.get(id).unwrap().added_at, Some(200));

        let mut earlier = track("one", "Rock", "Artist", "Album");
        earlier.added_at = Some(100);
        library.upsert(earlier);
        assert_eq!(library.get(id).unwrap().added_at, Some(100));

        let mut later = track("one", "Rock", "Artist", "Album");
        later.added_at = Some(300);
        library.upsert(later);
        assert_eq!(library.get(id).unwrap().added_at, Some(100));
    }

    #[test]
    fn upsert_refreshes_original_category_without_overwriting_user_edit() {
        let mut library = Library::new();
        let id = library.add(track("one", "Rock", "Artist", "Album"));
        library
            .edit(
                id,
                TrackEdit {
                    cat: Some("Personal".into()),
                    ..TrackEdit::default()
                },
            )
            .unwrap();

        library.upsert(track("one", "Metal", "Other", "Changed"));

        let record = library.get(id).unwrap();
        assert_eq!(record.cat, "Personal");
        assert_eq!(record.orig_cat.as_deref(), Some("Metal"));
    }

    #[test]
    fn edit_records_original_category_only_on_first_divergence() {
        let mut library = Library::new();
        let id = library.add(track("one", "Rock", "Artist", "Album"));

        library
            .edit(
                id,
                TrackEdit {
                    cat: Some("Jazz".into()),
                    ..TrackEdit::default()
                },
            )
            .unwrap();
        library
            .edit(
                id,
                TrackEdit {
                    cat: Some("Rock".into()),
                    ..TrackEdit::default()
                },
            )
            .unwrap();

        let record = library.get(id).unwrap();
        assert_eq!(record.cat, "Rock");
        assert_eq!(record.orig_cat.as_deref(), Some("Rock"));
    }

    #[test]
    fn track_star_click_toggles_explicit_override_and_inheritance() {
        let mut library = Library::new();
        let id = library.add(track("one", "Rock", "Artist", "Album"));
        library.set_album_rating(AlbumKey::of(library.get(id).unwrap()), Some(rating(4)));

        library.click_track_star(id, rating(4)).unwrap();
        assert_eq!(
            library.effective_rating(id),
            Some(EffectiveRating::Explicit(rating(4)))
        );
        library.click_track_star(id, rating(4)).unwrap();
        assert_eq!(
            library.effective_rating(id),
            Some(EffectiveRating::Inherited(rating(4)))
        );
    }

    #[test]
    fn effective_rating_precedence_and_album_reparenting() {
        let mut library = Library::new();
        let id = library.add(track("one", "Rock", "Artist", "Old"));
        let old = AlbumKey::of(library.get(id).unwrap());
        let target = AlbumKey {
            source: SourceId::Music,
            art: "Artist".into(),
            alb: "Target".into(),
        };
        library.set_album_rating(old, Some(rating(2)));
        library.set_album_rating(target, Some(rating(5)));
        assert_eq!(
            library.effective_rating(id),
            Some(EffectiveRating::Inherited(rating(2)))
        );

        library
            .edit(
                id,
                TrackEdit {
                    alb: Some("Unrated".into()),
                    ..TrackEdit::default()
                },
            )
            .unwrap();
        assert_eq!(library.effective_rating(id), None);
        library
            .edit(
                id,
                TrackEdit {
                    alb: Some("Target".into()),
                    ..TrackEdit::default()
                },
            )
            .unwrap();
        assert_eq!(
            library.effective_rating(id),
            Some(EffectiveRating::Inherited(rating(5)))
        );

        library.click_track_star(id, rating(3)).unwrap();
        library
            .edit(
                id,
                TrackEdit {
                    alb: Some("Old".into()),
                    ..TrackEdit::default()
                },
            )
            .unwrap();
        assert_eq!(
            library.effective_rating(id),
            Some(EffectiveRating::Explicit(rating(3)))
        );
    }

    #[test]
    fn restore_replaces_and_merge_preserves_existing_overlays() {
        let mut existing = Library::new();
        let existing_id = existing.add(track("same", "Edited", "Artist", "Album"));
        let existing_key = AlbumKey::of(existing.get(existing_id).unwrap());
        existing.set_album_rating(existing_key.clone(), Some(rating(5)));

        let mut imported = Library::new();
        imported.add(track("same", "Provider", "Artist", "Album"));
        imported.add(track("new", "Fresh", "Artist", "Other"));
        imported.set_album_rating(existing_key.clone(), Some(rating(1)));
        let new_key = AlbumKey {
            source: SourceId::Music,
            art: "Artist".into(),
            alb: "Other".into(),
        };
        imported.set_album_rating(new_key.clone(), Some(rating(3)));

        existing.merge(imported.clone());
        assert_eq!(existing.tracks().len(), 2);
        assert_eq!(existing.get(existing_id).unwrap().cat, "Edited");
        assert_eq!(existing.album_rating(&existing_key), Some(rating(5)));
        assert_eq!(existing.album_rating(&new_key), Some(rating(3)));
        let new_id = existing
            .tracks()
            .iter()
            .find(|track| track.uri == "new")
            .unwrap()
            .id;
        assert_ne!(new_id, existing_id);

        existing.restore(imported.clone());
        assert_eq!(existing, imported);
    }

    #[test]
    fn remove_uris_removes_tracks_and_only_orphaned_album_ratings() {
        let mut library = Library::new();
        let first = library.add(track("one", "Rock", "Artist", "Album"));
        library.add(track("two", "Rock", "Artist", "Album"));
        library.add(track("other", "Rock", "Artist", "Other"));
        let album = AlbumKey::of(library.get(first).unwrap());
        library.set_album_rating(album.clone(), Some(rating(5)));

        assert_eq!(library.remove_uris(&["one".into()]), 1);
        assert_eq!(library.album_rating(&album), Some(rating(5)));
        assert_eq!(library.remove_uris(&["two".into()]), 1);
        assert_eq!(library.album_rating(&album), None);
        assert_eq!(
            library
                .tracks()
                .iter()
                .map(|track| track.uri.as_str())
                .collect::<Vec<_>>(),
            ["other"]
        );
    }
}

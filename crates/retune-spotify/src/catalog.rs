//! A small, deterministic materialized catalog of Spotify music metadata.
//!
//! The catalog only contains facts that are safe to reuse for entity reads. It
//! deliberately does not contain search results, saved membership, or any
//! other query-shaped state.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::client::{Album, AlbumSummary, Artist, Followers, Image, Page, SimplifiedArtist, Track};

pub const CATALOG_VERSION: u8 = 1;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SpotifyCatalogV1 {
    #[serde(default)]
    pub artists: BTreeMap<String, CatalogArtist>,
    #[serde(default)]
    pub albums: BTreeMap<String, CatalogAlbum>,
    #[serde(default)]
    pub tracks: BTreeMap<String, CatalogTrack>,
}

/// Versioned on-disk wrapper. Maps are BTreeMaps so serialization is stable
/// and diffs stay useful when the cache is inspected during development.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SpotifyCatalog {
    pub version: u8,
    #[serde(flatten)]
    pub v1: SpotifyCatalogV1,
    #[serde(skip)]
    generation: u64,
}

impl PartialEq for SpotifyCatalog {
    fn eq(&self, other: &Self) -> bool {
        self.version == other.version && self.v1 == other.v1
    }
}

impl Eq for SpotifyCatalog {}

impl Default for SpotifyCatalog {
    fn default() -> Self {
        Self {
            version: CATALOG_VERSION,
            v1: SpotifyCatalogV1::default(),
            generation: 0,
        }
    }
}

impl SpotifyCatalog {
    pub fn is_supported(&self) -> bool {
        self.version == CATALOG_VERSION
    }

    /// Monotonically increases when the catalog changes. It is intentionally
    /// not persisted; the desktop store uses it to avoid redundant writes.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn counts(&self) -> CatalogCounts {
        CatalogCounts {
            artists: self.v1.artists.len(),
            albums: self.v1.albums.len(),
            tracks: self.v1.tracks.len(),
        }
    }

    pub fn clear(&mut self) {
        if self.v1.artists.is_empty() && self.v1.albums.is_empty() && self.v1.tracks.is_empty() {
            return;
        }
        self.v1 = SpotifyCatalogV1::default();
        self.bump();
    }

    pub fn observe_artist_summary(&mut self, artist: &Artist) {
        let incoming = CatalogArtist::from_summary(artist);
        self.v1
            .artists
            .entry(artist.id.clone())
            .and_modify(|current| current.merge_summary(&incoming))
            .or_insert(incoming);
        self.bump();
    }

    pub fn observe_artist(&mut self, artist: &Artist) {
        let incoming = CatalogArtist::from_full(artist);
        self.v1
            .artists
            .entry(artist.id.clone())
            .and_modify(|current| current.merge_full(&incoming))
            .or_insert(incoming);
        self.bump();
    }

    pub fn observe_album_summary(&mut self, album: &Album) {
        let incoming = CatalogAlbum::from_summary(album);
        self.v1
            .albums
            .entry(album.uri.clone())
            .and_modify(|current| current.merge_summary(&incoming))
            .or_insert(incoming);
        self.bump();
    }

    /// Merge a complete album response. `tracks_complete` is supplied by the
    /// client after it has fetched every album-track page.
    pub fn observe_album(&mut self, album: &Album, tracks_complete: bool) {
        let incoming = CatalogAlbum::from_full(album, tracks_complete);
        self.v1
            .albums
            .entry(album.uri.clone())
            .and_modify(|current| current.merge_full(&incoming))
            .or_insert(incoming);
        if let Some(tracks) = album.tracks.as_ref() {
            if tracks_complete {
                let parent = AlbumSummary {
                    id: album.id.clone(),
                    uri: album.uri.clone(),
                    name: album.name.clone(),
                    release_date: album.release_date.clone(),
                    images: album.images.clone(),
                };
                for track in &tracks.items {
                    let mut track = track.clone();
                    track.album = Some(parent.clone());
                    self.observe_track(&track);
                }
            }
            self.observe_album_track_page(&album.uri, 0, tracks, tracks_complete);
        }
        self.bump();
    }

    /// Merge one page of album tracks. A complete ordered list is committed
    /// only when the caller has observed the whole list.
    pub fn observe_album_track_page(
        &mut self,
        album_uri: &str,
        offset: u32,
        page: &Page<Track>,
        complete: bool,
    ) {
        let entry = self
            .v1
            .albums
            .entry(album_uri.to_owned())
            .or_insert_with(|| CatalogAlbum::stub(album_uri));
        let tracks = entry.tracks.get_or_insert_with(CatalogTrackList::default);
        let incoming_uris = page
            .items
            .iter()
            .map(|track| track.uri.clone())
            .collect::<Vec<_>>();
        for uri in &incoming_uris {
            tracks.observed_uris.insert(uri.clone());
        }
        if complete {
            tracks.uris = incoming_uris;
            tracks.total = page.total;
            tracks.observed_uris = tracks.uris.iter().cloned().collect();
            tracks.complete = true;
            entry.complete = entry.complete && entry.artists.is_some();
        } else {
            if !tracks.complete {
                if offset == 0 {
                    tracks.uris = incoming_uris;
                } else if offset == tracks.uris.len() as u32 {
                    tracks.uris.extend(incoming_uris);
                }
            }
            tracks.total = tracks.total.max(page.total);
            if page.next.is_none() && page.skipped == 0 && tracks.uris.len() as u32 == tracks.total
            {
                tracks.complete = true;
                entry.complete = entry.complete && entry.artists.is_some();
                tracks.observed_uris = tracks.uris.iter().cloned().collect();
            }
        }
        for track in &page.items {
            self.observe_track_summary(track);
        }
        let _ = offset;
        self.bump();
    }

    pub fn observe_track_summary(&mut self, track: &Track) {
        let incoming = CatalogTrack::from_summary(track);
        self.v1
            .tracks
            .entry(track.uri.clone())
            .and_modify(|current| current.merge_summary(&incoming))
            .or_insert(incoming);
        if let Some(album) = track.album.as_ref() {
            self.observe_album_summary(&album_to_stub(album));
        }
        for artist in &track.artists {
            self.observe_artist_summary(&Artist {
                id: artist.id.clone(),
                name: artist.name.clone(),
                genres: Vec::new(),
                followers: None,
                images: Vec::new(),
            });
        }
        self.bump();
    }

    pub fn observe_track(&mut self, track: &Track) {
        let incoming = CatalogTrack::from_full(track);
        self.v1
            .tracks
            .entry(track.uri.clone())
            .and_modify(|current| current.merge_full(&incoming))
            .or_insert(incoming);
        if let Some(album) = track.album.as_ref() {
            self.observe_album_summary(&album_to_stub(album));
        }
        for artist in &track.artists {
            self.observe_artist_summary(&Artist {
                id: artist.id.clone(),
                name: artist.name.clone(),
                genres: Vec::new(),
                followers: None,
                images: Vec::new(),
            });
        }
        self.bump();
    }

    pub fn set_artist_local_hint(&mut self, artist_id: &str, hint: impl Into<String>) {
        let entry = self
            .v1
            .artists
            .entry(artist_id.to_owned())
            .or_insert_with(|| CatalogArtist::stub(artist_id));
        entry.local_hint = Some(hint.into());
        self.bump();
    }

    pub fn set_album_local_hint(&mut self, album_uri: &str, hint: impl Into<String>) {
        let entry = self
            .v1
            .albums
            .entry(album_uri.to_owned())
            .or_insert_with(|| CatalogAlbum::stub(album_uri));
        entry.local_hint = Some(hint.into());
        self.bump();
    }

    pub fn set_track_local_hint(&mut self, track_uri: &str, hint: impl Into<String>) {
        let entry = self
            .v1
            .tracks
            .entry(track_uri.to_owned())
            .or_insert_with(|| CatalogTrack::stub(track_uri));
        entry.local_hint = Some(hint.into());
        self.bump();
    }

    pub fn complete_artist(&self, id: &str) -> Option<Artist> {
        self.v1
            .artists
            .get(id)
            .filter(|artist| artist.complete)
            .map(CatalogArtist::to_api)
    }

    pub fn complete_album(&self, uri: &str) -> Option<Album> {
        let album = self.v1.albums.get(uri)?;
        if !album.complete || !album.tracks.as_ref()?.complete {
            return None;
        }
        album.to_api(self)
    }

    pub fn complete_track(&self, uri: &str) -> Option<Track> {
        self.v1
            .tracks
            .get(uri)
            .filter(|track| track.complete)
            .map(CatalogTrack::to_api)
    }

    pub fn complete_album_tracks(
        &self,
        album_uri: &str,
        offset: u32,
        limit: u32,
    ) -> Option<Page<Track>> {
        let album = self.v1.albums.get(album_uri)?;
        let tracks = album.tracks.as_ref()?;
        if !tracks.complete {
            return None;
        }
        let items = tracks
            .uris
            .iter()
            .skip(offset as usize)
            .take(limit as usize)
            .map(|uri| self.v1.tracks.get(uri).map(CatalogTrack::to_api))
            .collect::<Option<Vec<_>>>()?;
        let next_offset = offset.saturating_add(items.len() as u32);
        Some(Page {
            items,
            next: (next_offset < tracks.total)
                .then(|| format!("{album_uri}?offset={next_offset}&limit={limit}")),
            skipped: 0,
            total: tracks.total,
        })
    }

    fn bump(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CatalogCounts {
    pub artists: usize,
    pub albums: usize,
    pub tracks: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CatalogImage {
    pub url: String,
    #[serde(default)]
    pub width: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CatalogArtistRef {
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CatalogAlbumRef {
    pub id: String,
    pub uri: String,
    pub name: String,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub images: Vec<CatalogImage>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CatalogArtist {
    pub id: String,
    pub name: String,
    /// `None` means Spotify did not provide the collection; `Some([])` is a
    /// known empty collection from a complete artist response.
    #[serde(default)]
    pub genres: Option<Vec<String>>,
    #[serde(default)]
    pub followers: Option<u64>,
    #[serde(default)]
    pub images: Option<Vec<CatalogImage>>,
    pub complete: bool,
    #[serde(default)]
    pub local_hint: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CatalogTrackList {
    #[serde(default)]
    pub uris: Vec<String>,
    #[serde(default)]
    pub total: u32,
    #[serde(default)]
    pub complete: bool,
    #[serde(default)]
    pub observed_uris: BTreeSet<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CatalogAlbum {
    pub id: String,
    pub uri: String,
    pub name: String,
    #[serde(default)]
    pub artists: Option<Vec<CatalogArtistRef>>,
    #[serde(default)]
    pub images: Option<Vec<CatalogImage>>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub album_type: Option<String>,
    #[serde(default)]
    pub total_tracks: Option<u32>,
    #[serde(default)]
    pub tracks: Option<CatalogTrackList>,
    pub complete: bool,
    #[serde(default)]
    pub local_hint: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CatalogTrack {
    pub uri: String,
    pub name: String,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub track_number: Option<u32>,
    #[serde(default)]
    pub disc_number: Option<u32>,
    #[serde(default)]
    pub artists: Option<Vec<CatalogArtistRef>>,
    #[serde(default)]
    pub album: Option<CatalogAlbumRef>,
    pub complete: bool,
    #[serde(default)]
    pub local_hint: Option<String>,
}

// ponytail: serde DTOs collapse omitted and explicit empty; preserve rich data
// until presence-aware DTOs exist for intentional collection replacement.
impl CatalogArtist {
    fn stub(id: &str) -> Self {
        Self {
            id: id.into(),
            name: String::new(),
            genres: None,
            followers: None,
            images: None,
            complete: false,
            local_hint: None,
        }
    }

    fn from_summary(value: &Artist) -> Self {
        Self {
            id: value.id.clone(),
            name: value.name.clone(),
            genres: None,
            followers: None,
            images: (!value.images.is_empty())
                .then(|| value.images.iter().map(Into::into).collect()),
            complete: false,
            local_hint: None,
        }
    }

    fn from_full(value: &Artist) -> Self {
        Self {
            id: value.id.clone(),
            name: value.name.clone(),
            genres: Some(value.genres.clone()),
            followers: value.followers.as_ref().map(|followers| followers.total),
            images: Some(value.images.iter().map(Into::into).collect()),
            complete: true,
            local_hint: None,
        }
    }

    fn merge_summary(&mut self, incoming: &Self) {
        if self.name.is_empty() {
            self.name = incoming.name.clone();
        }
        if self.images.is_none() {
            self.images = incoming.images.clone();
        }
    }

    fn merge_full(&mut self, incoming: &Self) {
        self.name = incoming.name.clone();
        if incoming
            .genres
            .as_ref()
            .is_some_and(|genres| !genres.is_empty())
            || self.genres.is_none()
        {
            self.genres = incoming.genres.clone();
        }
        if incoming.followers.is_some() {
            self.followers = incoming.followers;
        }
        if incoming
            .images
            .as_ref()
            .is_some_and(|images| !images.is_empty())
            || self.images.is_none()
        {
            self.images = incoming.images.clone();
        }
        self.complete = true;
    }

    fn to_api(&self) -> Artist {
        Artist {
            id: self.id.clone(),
            name: self.name.clone(),
            genres: self.genres.clone().unwrap_or_default(),
            followers: self.followers.map(|total| Followers { total }),
            images: self
                .images
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(Into::into)
                .collect(),
        }
    }
}

impl CatalogAlbum {
    fn stub(uri: &str) -> Self {
        let id = spotify_id(uri).unwrap_or_default();
        Self {
            id,
            uri: uri.into(),
            name: String::new(),
            artists: None,
            images: None,
            release_date: None,
            album_type: None,
            total_tracks: None,
            tracks: None,
            complete: false,
            local_hint: None,
        }
    }

    fn from_summary(value: &Album) -> Self {
        Self {
            id: value.id.clone(),
            uri: value.uri.clone(),
            name: value.name.clone(),
            artists: (!value.artists.is_empty())
                .then(|| value.artists.iter().map(Into::into).collect()),
            images: (!value.images.is_empty())
                .then(|| value.images.iter().map(Into::into).collect()),
            release_date: value.release_date.clone(),
            album_type: value.album_type.clone(),
            total_tracks: (value.total_tracks != 0).then_some(value.total_tracks),
            tracks: None,
            complete: false,
            local_hint: None,
        }
    }

    fn from_full(value: &Album, tracks_complete: bool) -> Self {
        Self {
            id: value.id.clone(),
            uri: value.uri.clone(),
            name: value.name.clone(),
            artists: Some(value.artists.iter().map(Into::into).collect()),
            images: Some(value.images.iter().map(Into::into).collect()),
            release_date: value.release_date.clone(),
            album_type: value.album_type.clone(),
            total_tracks: Some(value.total_tracks),
            tracks: value.tracks.as_ref().map(|tracks| CatalogTrackList {
                uris: tracks.items.iter().map(|track| track.uri.clone()).collect(),
                total: tracks.total,
                complete: tracks_complete,
                observed_uris: tracks.items.iter().map(|track| track.uri.clone()).collect(),
            }),
            complete: tracks_complete && value.tracks.is_some(),
            local_hint: None,
        }
    }

    fn merge_summary(&mut self, incoming: &Self) {
        if self.id.is_empty() {
            self.id = incoming.id.clone();
        }
        if self.name.is_empty() {
            self.name = incoming.name.clone();
        }
        if self.artists.is_none() {
            self.artists = incoming.artists.clone();
        }
        if self.images.is_none() {
            self.images = incoming.images.clone();
        }
        if self.release_date.is_none() {
            self.release_date = incoming.release_date.clone();
        }
        if self.album_type.is_none() {
            self.album_type = incoming.album_type.clone();
        }
        if self.total_tracks.is_none() {
            self.total_tracks = incoming.total_tracks;
        }
    }

    fn merge_full(&mut self, incoming: &Self) {
        self.id = incoming.id.clone();
        self.name = incoming.name.clone();
        if incoming
            .artists
            .as_ref()
            .is_some_and(|artists| !artists.is_empty())
            || self.artists.is_none()
        {
            self.artists = incoming.artists.clone();
        }
        if incoming
            .images
            .as_ref()
            .is_some_and(|images| !images.is_empty())
            || self.images.is_none()
        {
            self.images = incoming.images.clone();
        }
        if incoming.release_date.is_some() {
            self.release_date = incoming.release_date.clone();
        }
        if incoming.album_type.is_some() {
            self.album_type = incoming.album_type.clone();
        }
        if incoming.total_tracks.is_some() {
            self.total_tracks = incoming.total_tracks;
        }
        if incoming
            .tracks
            .as_ref()
            .is_some_and(|tracks| tracks.complete)
        {
            self.tracks = incoming.tracks.clone();
        }
        self.complete |= incoming.complete;
    }

    fn to_api(&self, catalog: &SpotifyCatalog) -> Option<Album> {
        let tracks = self.tracks.as_ref()?;
        let items = tracks
            .uris
            .iter()
            .map(|uri| catalog.v1.tracks.get(uri).map(CatalogTrack::to_api))
            .collect::<Option<Vec<_>>>()?;
        Some(Album {
            id: self.id.clone(),
            uri: self.uri.clone(),
            name: self.name.clone(),
            artists: self
                .artists
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(Into::into)
                .collect(),
            images: self
                .images
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(Into::into)
                .collect(),
            release_date: self.release_date.clone(),
            album_type: self.album_type.clone(),
            total_tracks: self.total_tracks.unwrap_or(tracks.total),
            tracks: Some(Page {
                items,
                next: None,
                skipped: 0,
                total: tracks.total,
            }),
        })
    }
}

impl CatalogTrack {
    fn stub(uri: &str) -> Self {
        Self {
            uri: uri.into(),
            name: String::new(),
            duration_ms: None,
            track_number: None,
            disc_number: None,
            artists: None,
            album: None,
            complete: false,
            local_hint: None,
        }
    }

    fn from_summary(value: &Track) -> Self {
        Self {
            uri: value.uri.clone(),
            name: value.name.clone(),
            duration_ms: value.duration_ms,
            track_number: value.track_number,
            disc_number: value.disc_number,
            artists: (!value.artists.is_empty())
                .then(|| value.artists.iter().map(Into::into).collect()),
            album: value.album.as_ref().map(Into::into),
            complete: false,
            local_hint: None,
        }
    }

    fn from_full(value: &Track) -> Self {
        Self {
            uri: value.uri.clone(),
            name: value.name.clone(),
            duration_ms: value.duration_ms,
            track_number: value.track_number,
            disc_number: value.disc_number,
            artists: Some(value.artists.iter().map(Into::into).collect()),
            album: value.album.as_ref().map(Into::into),
            complete: true,
            local_hint: None,
        }
    }

    fn merge_summary(&mut self, incoming: &Self) {
        if self.name.is_empty() {
            self.name = incoming.name.clone();
        }
        if self.duration_ms.is_none() {
            self.duration_ms = incoming.duration_ms;
        }
        if self.track_number.is_none() {
            self.track_number = incoming.track_number;
        }
        if self.disc_number.is_none() {
            self.disc_number = incoming.disc_number;
        }
        if self.artists.is_none() {
            self.artists = incoming.artists.clone();
        }
        if self.album.is_none() {
            self.album = incoming.album.clone();
        }
    }

    fn merge_full(&mut self, incoming: &Self) {
        self.name = incoming.name.clone();
        if incoming.duration_ms.is_some() {
            self.duration_ms = incoming.duration_ms;
        }
        if incoming.track_number.is_some() {
            self.track_number = incoming.track_number;
        }
        if incoming.disc_number.is_some() {
            self.disc_number = incoming.disc_number;
        }
        if incoming
            .artists
            .as_ref()
            .is_some_and(|artists| !artists.is_empty())
            || self.artists.is_none()
        {
            self.artists = incoming.artists.clone();
        }
        if incoming.album.is_some() {
            self.album = incoming.album.clone();
        }
        self.complete = true;
    }

    fn to_api(&self) -> Track {
        Track {
            uri: self.uri.clone(),
            name: self.name.clone(),
            duration_ms: self.duration_ms,
            track_number: self.track_number,
            disc_number: self.disc_number,
            artists: self
                .artists
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(Into::into)
                .collect(),
            album: self.album.clone().map(Into::into),
        }
    }
}

fn album_to_stub(value: &AlbumSummary) -> Album {
    Album {
        id: value.id.clone(),
        uri: value.uri.clone(),
        name: value.name.clone(),
        artists: Vec::new(),
        images: value.images.clone(),
        release_date: value.release_date.clone(),
        album_type: None,
        total_tracks: 0,
        tracks: None,
    }
}

fn spotify_id(uri: &str) -> Option<String> {
    uri.rsplit_once(':')
        .filter(|(kind, id)| *kind == "spotify:album" && !id.is_empty())
        .map(|(_, id)| id.to_owned())
}

impl From<&Image> for CatalogImage {
    fn from(value: &Image) -> Self {
        Self {
            url: value.url.clone(),
            width: value.width,
        }
    }
}

impl From<CatalogImage> for Image {
    fn from(value: CatalogImage) -> Self {
        Self {
            url: value.url,
            width: value.width,
        }
    }
}

impl From<&SimplifiedArtist> for CatalogArtistRef {
    fn from(value: &SimplifiedArtist) -> Self {
        Self {
            id: Some(value.id.clone()),
            name: value.name.clone(),
        }
    }
}

impl From<CatalogArtistRef> for SimplifiedArtist {
    fn from(value: CatalogArtistRef) -> Self {
        Self {
            id: value.id.unwrap_or_default(),
            name: value.name,
        }
    }
}

impl From<&AlbumSummary> for CatalogAlbumRef {
    fn from(value: &AlbumSummary) -> Self {
        Self {
            id: value.id.clone(),
            uri: value.uri.clone(),
            name: value.name.clone(),
            release_date: value.release_date.clone(),
            images: value.images.iter().map(Into::into).collect(),
        }
    }
}

impl From<CatalogAlbumRef> for AlbumSummary {
    fn from(value: CatalogAlbumRef) -> Self {
        Self {
            id: value.id,
            uri: value.uri,
            name: value.name,
            release_date: value.release_date,
            images: value.images.into_iter().map(Into::into).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artist(genres: Vec<&str>) -> Artist {
        Artist {
            id: "artist-1".into(),
            name: "Artist".into(),
            genres: genres.into_iter().map(str::to_owned).collect(),
            followers: Some(Followers { total: 3 }),
            images: vec![],
        }
    }

    fn track(name: &str) -> Track {
        Track {
            uri: format!("spotify:track:{name}"),
            name: name.into(),
            duration_ms: Some(10),
            track_number: Some(1),
            disc_number: Some(1),
            artists: vec![SimplifiedArtist {
                id: "artist-1".into(),
                name: "Artist".into(),
            }],
            album: None,
        }
    }

    #[test]
    fn summary_does_not_erase_full_known_empty_collections() {
        let mut catalog = SpotifyCatalog::default();
        catalog.observe_artist(&artist(vec![]));
        catalog.observe_artist_summary(&artist(vec!["should-not-win"]));
        let value = &catalog.v1.artists["artist-1"];
        assert_eq!(value.genres, Some(vec![]));
        assert!(value.complete);
    }

    #[test]
    fn sparse_full_observations_preserve_rich_collections() {
        let mut catalog = SpotifyCatalog::default();
        let rich_artist = Artist {
            id: "artist-1".into(),
            name: "Artist".into(),
            genres: vec!["rock".into()],
            followers: Some(Followers { total: 3 }),
            images: vec![Image {
                url: "artist-image".into(),
                width: Some(300),
            }],
        };
        let sparse_artist = Artist {
            id: rich_artist.id.clone(),
            name: rich_artist.name.clone(),
            genres: vec![],
            followers: None,
            images: vec![],
        };
        catalog.observe_artist(&rich_artist);
        catalog.observe_artist(&sparse_artist);

        let rich_track = Track {
            uri: "spotify:track:track".into(),
            name: "Track".into(),
            duration_ms: Some(10),
            track_number: Some(1),
            disc_number: Some(1),
            artists: vec![SimplifiedArtist {
                id: rich_artist.id.clone(),
                name: rich_artist.name.clone(),
            }],
            album: None,
        };
        let sparse_track = Track {
            uri: rich_track.uri.clone(),
            name: rich_track.name.clone(),
            duration_ms: None,
            track_number: None,
            disc_number: None,
            artists: vec![],
            album: None,
        };
        catalog.observe_track(&rich_track);
        catalog.observe_track(&sparse_track);

        assert_eq!(
            catalog.v1.artists["artist-1"].genres,
            Some(vec!["rock".into()])
        );
        assert_eq!(
            catalog.v1.artists["artist-1"].images,
            Some(vec![CatalogImage {
                url: "artist-image".into(),
                width: Some(300),
            }])
        );
        assert_eq!(
            catalog.v1.tracks["spotify:track:track"].artists,
            Some(vec![CatalogArtistRef {
                id: Some("artist-1".into()),
                name: "Artist".into(),
            }])
        );
    }

    #[test]
    fn idless_artist_credit_defaults_and_projects_to_an_empty_wire_id() {
        let credit: CatalogArtistRef = serde_json::from_value(serde_json::json!({
            "name": "Named Artist"
        }))
        .unwrap();
        assert_eq!(credit.id, None);
        assert_eq!(
            SimplifiedArtist::from(credit),
            SimplifiedArtist {
                id: String::new(),
                name: "Named Artist".into(),
            }
        );
    }

    #[test]
    fn complete_album_tracks_replace_partial_order_and_round_trip_deterministically() {
        let mut catalog = SpotifyCatalog::default();
        let first = track("one");
        let second = track("two");
        let page = Page {
            items: vec![first.clone()],
            next: Some("next".into()),
            skipped: 0,
            total: 2,
        };
        catalog.observe_album_track_page("spotify:album:a", 0, &page, false);
        let complete = Page {
            items: vec![second.clone(), first.clone()],
            next: None,
            skipped: 0,
            total: 2,
        };
        catalog.observe_album_track_page("spotify:album:a", 0, &complete, true);
        let cached = catalog
            .complete_album_tracks("spotify:album:a", 0, 10)
            .unwrap();
        assert_eq!(
            cached.items.into_iter().map(|t| t.name).collect::<Vec<_>>(),
            ["two", "one"]
        );
        let bytes = serde_json::to_vec(&catalog).unwrap();
        assert_eq!(
            serde_json::to_vec(&serde_json::from_slice::<SpotifyCatalog>(&bytes).unwrap()).unwrap(),
            bytes
        );
    }

    #[test]
    fn sequential_album_track_pages_become_a_complete_ordered_list() {
        let mut catalog = SpotifyCatalog::default();
        let first = track("one");
        let second = track("two");
        catalog.observe_album_track_page(
            "spotify:album:a",
            0,
            &Page {
                items: vec![first.clone()],
                next: Some("next".into()),
                skipped: 0,
                total: 2,
            },
            false,
        );
        catalog.observe_album_track_page(
            "spotify:album:a",
            1,
            &Page {
                items: vec![second.clone()],
                next: None,
                skipped: 0,
                total: 2,
            },
            false,
        );

        let cached = catalog
            .complete_album_tracks("spotify:album:a", 0, 10)
            .unwrap();
        assert_eq!(cached.items, vec![first, second]);
    }

    #[test]
    fn complete_album_tracks_preserve_cached_pagination() {
        let mut catalog = SpotifyCatalog::default();
        let tracks = [track("one"), track("two"), track("three")];
        catalog.observe_album_track_page(
            "spotify:album:a",
            0,
            &Page {
                items: tracks.to_vec(),
                next: None,
                skipped: 0,
                total: tracks.len() as u32,
            },
            true,
        );

        let first = catalog
            .complete_album_tracks("spotify:album:a", 0, 2)
            .unwrap();
        assert_eq!(first.items, tracks[..2]);
        assert!(first.next.is_some());

        let last = catalog
            .complete_album_tracks("spotify:album:a", 2, 2)
            .unwrap();
        assert_eq!(last.items, tracks[2..]);
        assert!(last.next.is_none());
    }

    #[test]
    fn local_hints_are_not_used_as_identity() {
        let mut catalog = SpotifyCatalog::default();
        catalog.set_track_local_hint("spotify:track:missing", "Artist — Album — Song");
        assert!(catalog.complete_track("spotify:track:missing").is_none());
        assert_eq!(catalog.v1.tracks.len(), 1);
    }
}

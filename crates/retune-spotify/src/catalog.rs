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
struct SpotifyCatalogV1 {
    #[serde(default)]
    artists: BTreeMap<String, CatalogArtist>,
    #[serde(default)]
    albums: BTreeMap<String, CatalogAlbum>,
    #[serde(default)]
    tracks: BTreeMap<String, CatalogTrack>,
}

/// Versioned on-disk wrapper. Maps are BTreeMaps so serialization is stable
/// and diffs stay useful when the cache is inspected during development.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SpotifyCatalog {
    version: u8,
    #[serde(flatten)]
    v1: SpotifyCatalogV1,
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
    pub fn version(&self) -> u8 {
        self.version
    }

    pub fn is_supported(&self) -> bool {
        self.version == CATALOG_VERSION
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.is_supported() {
            return Err("unsupported Spotify catalog version");
        }
        for (id, artist) in &self.v1.artists {
            if id != &artist.id {
                return Err("Spotify catalog artist key does not match its ID");
            }
            if artist.complete && (artist.genres.is_none() || artist.images.is_none()) {
                return Err("complete Spotify catalog artist is missing fields");
            }
        }
        for (uri, album) in &self.v1.albums {
            if uri != &album.uri {
                return Err("Spotify catalog album key does not match its URI");
            }
            if album.complete
                && (album.artists.is_none()
                    || album.images.is_none()
                    || album.total_tracks.is_none()
                    || album.tracks.as_ref().is_none_or(|tracks| !tracks.complete))
            {
                return Err("complete Spotify catalog album is missing fields");
            }
            let invalid_complete_tracks = album.tracks.as_ref().is_some_and(|tracks| {
                tracks.complete
                    && (tracks.uris.len() as u32 != tracks.total
                        || album
                            .total_tracks
                            .is_some_and(|total| total != tracks.total)
                        || tracks.uris.iter().collect::<BTreeSet<_>>().len() != tracks.uris.len()
                        || tracks
                            .uris
                            .iter()
                            .any(|uri| !self.v1.tracks.contains_key(uri)))
            });
            if invalid_complete_tracks {
                return Err("complete Spotify catalog album track list is inconsistent");
            }
        }
        for (uri, track) in &self.v1.tracks {
            if uri != &track.uri {
                return Err("Spotify catalog track key does not match its URI");
            }
            if track.complete && track.artists.is_none() {
                return Err("complete Spotify catalog track is missing fields");
            }
        }
        Ok(())
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
        if self.observe_artist_summary_inner(artist) {
            self.bump();
        }
    }

    fn observe_artist_summary_inner(&mut self, artist: &Artist) -> bool {
        let incoming = CatalogArtist::from_summary(artist);
        match self.v1.artists.entry(artist.id.clone()) {
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                entry.get_mut().merge_summary(&incoming)
            }
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(incoming);
                true
            }
        }
    }

    pub fn observe_artist(&mut self, artist: &Artist) {
        let incoming = CatalogArtist::from_full(artist);
        let changed = match self.v1.artists.entry(artist.id.clone()) {
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                entry.get_mut().merge_full(&incoming)
            }
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(incoming);
                true
            }
        };
        if changed {
            self.bump();
        }
    }

    pub fn observe_album_summary(&mut self, album: &Album) {
        if self.observe_album_summary_inner(album) {
            self.bump();
        }
    }

    fn observe_album_summary_inner(&mut self, album: &Album) -> bool {
        let incoming = CatalogAlbum::from_summary(album);
        match self.v1.albums.entry(album.uri.clone()) {
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                entry.get_mut().merge_summary(&incoming)
            }
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(incoming);
                true
            }
        }
    }

    /// Merge a complete album response. `tracks_complete` is supplied by the
    /// client after it has fetched every album-track page.
    pub fn observe_album(&mut self, album: &Album, tracks_complete: bool) {
        let incoming = CatalogAlbum::from_full(album, tracks_complete);
        let mut changed = match self.v1.albums.entry(album.uri.clone()) {
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                entry.get_mut().merge_full(&incoming)
            }
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(incoming);
                true
            }
        };
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
                    changed |= self.observe_track_inner(&track);
                }
            }
            changed |= self.observe_album_track_page_inner(&album.uri, 0, tracks, tracks_complete);
        }
        if changed {
            self.bump();
        }
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
        if self.observe_album_track_page_inner(album_uri, offset, page, complete) {
            self.bump();
        }
    }

    fn observe_album_track_page_inner(
        &mut self,
        album_uri: &str,
        offset: u32,
        page: &Page<Track>,
        complete: bool,
    ) -> bool {
        let mut changed = {
            let entry = self
                .v1
                .albums
                .entry(album_uri.to_owned())
                .or_insert_with(|| CatalogAlbum::stub(album_uri));
            let before = entry.clone();
            let tracks = entry.tracks.get_or_insert_with(CatalogTrackList::default);
            let incoming_uris = page
                .items
                .iter()
                .map(|track| track.uri.clone())
                .collect::<Vec<_>>();
            if complete {
                tracks.uris = incoming_uris;
                tracks.total = page.total;
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
                if page.next.is_none()
                    && page.skipped == 0
                    && tracks.uris.len() as u32 == tracks.total
                {
                    tracks.complete = true;
                    entry.complete = entry.complete && entry.artists.is_some();
                }
            }
            *entry != before
        };
        for track in &page.items {
            changed |= self.observe_track_summary_inner(track);
        }
        changed
    }

    pub fn observe_track_summary(&mut self, track: &Track) {
        if self.observe_track_summary_inner(track) {
            self.bump();
        }
    }

    fn observe_track_summary_inner(&mut self, track: &Track) -> bool {
        let incoming = CatalogTrack::from_summary(track);
        let mut changed = match self.v1.tracks.entry(track.uri.clone()) {
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                entry.get_mut().merge_summary(&incoming)
            }
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(incoming);
                true
            }
        };
        if let Some(album) = track.album.as_ref() {
            changed |= self.observe_album_summary_inner(&album_to_stub(album));
        }
        for artist in &track.artists {
            changed |= self.observe_artist_summary_inner(&Artist {
                id: artist.id.clone(),
                name: artist.name.clone(),
                genres: Vec::new(),
                followers: None,
                images: Vec::new(),
            });
        }
        changed
    }

    pub fn observe_track(&mut self, track: &Track) {
        if self.observe_track_inner(track) {
            self.bump();
        }
    }

    fn observe_track_inner(&mut self, track: &Track) -> bool {
        let incoming = CatalogTrack::from_full(track);
        let mut changed = match self.v1.tracks.entry(track.uri.clone()) {
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                entry.get_mut().merge_full(&incoming)
            }
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(incoming);
                true
            }
        };
        if let Some(album) = track.album.as_ref() {
            changed |= self.observe_album_summary_inner(&album_to_stub(album));
        }
        for artist in &track.artists {
            changed |= self.observe_artist_summary_inner(&Artist {
                id: artist.id.clone(),
                name: artist.name.clone(),
                genres: Vec::new(),
                followers: None,
                images: Vec::new(),
            });
        }
        changed
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
        if limit == 0 {
            return None;
        }
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
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CatalogTrackList {
    #[serde(default)]
    pub uris: Vec<String>,
    #[serde(default)]
    pub total: u32,
    #[serde(default)]
    pub complete: bool,
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
}

// ponytail: serde DTOs collapse omitted and explicit empty; preserve rich data
// until presence-aware DTOs exist for intentional collection replacement.
impl CatalogArtist {
    fn from_summary(value: &Artist) -> Self {
        Self {
            id: value.id.clone(),
            name: value.name.clone(),
            genres: None,
            followers: None,
            images: (!value.images.is_empty())
                .then(|| value.images.iter().map(Into::into).collect()),
            complete: false,
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
        }
    }

    fn merge_summary(&mut self, incoming: &Self) -> bool {
        let before = self.clone();
        if self.name.is_empty() {
            self.name = incoming.name.clone();
        }
        if self.images.is_none() {
            self.images = incoming.images.clone();
        }
        *self != before
    }

    fn merge_full(&mut self, incoming: &Self) -> bool {
        let before = self.clone();
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
        *self != before
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
            }),
            complete: tracks_complete && value.tracks.is_some(),
        }
    }

    fn merge_summary(&mut self, incoming: &Self) -> bool {
        let before = self.clone();
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
        *self != before
    }

    fn merge_full(&mut self, incoming: &Self) -> bool {
        let before = self.clone();
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
        *self != before
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
        }
    }

    fn merge_summary(&mut self, incoming: &Self) -> bool {
        let before = self.clone();
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
        *self != before
    }

    fn merge_full(&mut self, incoming: &Self) -> bool {
        let before = self.clone();
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
        *self != before
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
    fn logical_track_observation_bumps_once_only_when_changed() {
        let mut catalog = SpotifyCatalog::default();
        let original = track("one");

        catalog.observe_track(&original);
        assert_eq!(catalog.generation(), 1);

        catalog.observe_track(&original);
        assert_eq!(catalog.generation(), 1);

        let mut changed = original;
        changed.name = "renamed".into();
        catalog.observe_track(&changed);
        assert_eq!(catalog.generation(), 2);
    }

    #[test]
    fn serialized_v1_shape_stays_flat_and_generation_stays_transient() {
        let mut catalog = SpotifyCatalog::default();
        catalog.observe_track(&track("one"));
        let value = serde_json::to_value(&catalog).unwrap();

        assert_eq!(value["version"], CATALOG_VERSION);
        assert!(value.get("v1").is_none());
        assert!(value["tracks"].get("spotify:track:one").is_some());
        assert!(value.get("generation").is_none());

        let restored: SpotifyCatalog = serde_json::from_value(value).unwrap();
        assert_eq!(restored, catalog);
        assert_eq!(restored.generation(), 0);
    }

    #[test]
    fn validate_rejects_semantically_corrupt_complete_catalogs() {
        let cases = [
            serde_json::json!({
                "version": 1,
                "artists": {"wrong": {"id": "artist", "name": "Artist", "complete": false}},
                "albums": {}, "tracks": {}
            }),
            serde_json::json!({
                "version": 1, "artists": {},
                "albums": {"spotify:album:a": {
                    "id": "a", "uri": "spotify:album:a", "name": "Album", "complete": true
                }},
                "tracks": {}
            }),
            serde_json::json!({
                "version": 1, "artists": {},
                "albums": {"spotify:album:a": {
                    "id": "a", "uri": "spotify:album:a", "name": "Album", "complete": true,
                    "artists": [], "images": [], "total_tracks": 2,
                    "tracks": {"uris": ["spotify:track:missing"], "total": 2, "complete": true}
                }},
                "tracks": {}
            }),
            serde_json::json!({
                "version": 1, "artists": {}, "albums": {},
                "tracks": {"spotify:track:t": {
                    "uri": "spotify:track:t", "name": "Track", "complete": true
                }}
            }),
        ];

        for value in cases {
            let catalog: SpotifyCatalog = serde_json::from_value(value).unwrap();
            assert!(catalog.validate().is_err());
        }
        assert!(SpotifyCatalog::default().validate().is_ok());
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
        assert!(
            catalog
                .complete_album_tracks("spotify:album:a", 0, 0)
                .is_none()
        );
    }

    #[test]
    fn removed_cache_fields_in_old_json_are_ignored() {
        let catalog: SpotifyCatalog = serde_json::from_value(serde_json::json!({
            "version": 1,
            "artists": {},
            "albums": {"spotify:album:a": {
                "id": "a", "uri": "spotify:album:a", "name": "Album", "complete": false,
                "local_hint": "Artist — Album",
                "tracks": {"uris": [], "total": 0, "complete": false,
                    "observed_uris": ["spotify:track:old"]}
            }},
            "tracks": {}
        }))
        .unwrap();

        let value = serde_json::to_value(catalog).unwrap();
        assert!(
            value["albums"]["spotify:album:a"]
                .get("local_hint")
                .is_none()
        );
        assert!(
            value["albums"]["spotify:album:a"]["tracks"]
                .get("observed_uris")
                .is_none()
        );
    }
}

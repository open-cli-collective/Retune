use retune_core::model::NewTrack;
use retune_spotify::{
    client::{endpoint_family, Album, Artist, SpotifyClient, Transport},
    normalize,
    tokens::TokenStore,
};
use std::{
    collections::{BTreeMap, HashSet},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use crate::store::FsSyncStore;

const PAGE_SIZE: u32 = 50;
const SEARCH_PAGE_SIZE: u32 = 10;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LibraryKind {
    Tracks,
    Albums,
    Shows,
    Episodes,
    Audiobooks,
}

impl LibraryKind {
    pub const ALL: [Self; 5] = [
        Self::Tracks,
        Self::Albums,
        Self::Shows,
        Self::Episodes,
        Self::Audiobooks,
    ];

    pub const fn phase(self) -> &'static str {
        match self {
            Self::Tracks => "Syncing saved tracks…",
            Self::Albums => "Syncing saved albums…",
            Self::Shows => "Syncing saved shows…",
            Self::Episodes => "Syncing saved episodes…",
            Self::Audiobooks => "Syncing saved audiobooks…",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchArtist {
    pub name: String,
    pub uri: String,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchAlbum {
    pub name: String,
    pub artist: String,
    pub uri: String,
    pub track_count: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct SearchResults {
    pub artists: Vec<SearchArtist>,
    pub albums: Vec<SearchAlbum>,
}

pub struct Snapshot {
    pub batches: Vec<Vec<NewTrack>>,
    pub genres_degraded: bool,
    pub partial: bool,
}

pub trait MediaProvider: Send + Sync {
    async fn library_snapshot(&self, kind: LibraryKind) -> Result<Snapshot, String>;
    fn earliest_cooldown(&self) -> Option<u64> {
        None
    }
    fn request_counts(&self) -> BTreeMap<String, u64> {
        BTreeMap::new()
    }
    async fn search(&self, query: &str) -> Result<SearchResults, String>;
    async fn artist_albums(&self, artist: &str) -> Result<Vec<SearchAlbum>, String>;
    async fn album_tracks(&self, album: &str) -> Result<Vec<NewTrack>, String>;
    async fn save_to_spotify(&self, uris: &[String]) -> Result<(), String>;
}

pub struct SpotifySyncProvider<'a, T, S> {
    client: &'a SpotifyClient<T, S>,
    run: SyncRun<'a>,
}

impl<'a, T: Transport, S: TokenStore> SpotifySyncProvider<'a, T, S> {
    pub fn new(client: &'a SpotifyClient<T, S>, store: &'a FsSyncStore) -> Result<Self, String> {
        client.reset_request_counts();
        Ok(Self {
            client,
            run: SyncRun {
                store,
                cooldowns: Mutex::new(
                    store
                        .cooldowns(unix_now())
                        .map_err(|error| error.to_string())?,
                ),
                artist_genres: Mutex::new(
                    store.artist_genres().map_err(|error| error.to_string())?,
                ),
                earliest_cooldown: AtomicU64::new(0),
            },
        })
    }
}

struct SyncRun<'a> {
    store: &'a FsSyncStore,
    cooldowns: Mutex<BTreeMap<String, u64>>,
    artist_genres: Mutex<BTreeMap<String, Vec<String>>>,
    earliest_cooldown: AtomicU64,
}

impl SyncRun<'_> {
    fn cooldown(&self, family: &str) -> Option<u64> {
        let deadline = self
            .cooldowns
            .lock()
            .expect("cooldown mutex poisoned")
            .get(family)
            .copied()
            .filter(|deadline| *deadline > unix_now());
        if let Some(deadline) = deadline {
            self.note_deadline(deadline);
            log::warn!("skipping {family} until {}", format_resume_time(deadline));
        }
        deadline
    }

    fn record_rate_limit(&self, endpoint: &str, retry_after_secs: u64) {
        let family = endpoint_family(endpoint);
        let deadline = unix_now().saturating_add(retry_after_secs);
        let mut cooldowns = self.cooldowns.lock().expect("cooldown mutex poisoned");
        cooldowns.insert(family, deadline);
        self.note_deadline(deadline);
        if let Err(error) = self.store.save_cooldowns(&cooldowns) {
            log::warn!("Could not persist Spotify cooldown: {error}");
        }
    }

    fn note_deadline(&self, deadline: u64) {
        let _ =
            self.earliest_cooldown
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                    Some(if current == 0 {
                        deadline
                    } else {
                        current.min(deadline)
                    })
                });
    }

    fn cached_artist(&self, id: &str) -> Option<Artist> {
        self.artist_genres
            .lock()
            .expect("artist genre mutex poisoned")
            .get(id)
            .cloned()
            .map(|genres| Artist {
                id: id.into(),
                name: String::new(),
                genres,
            })
    }

    fn cache_artist(&self, artist: &Artist) {
        let mut cache = self
            .artist_genres
            .lock()
            .expect("artist genre mutex poisoned");
        cache.insert(artist.id.clone(), artist.genres.clone());
        if let Err(error) = self.store.save_artist_genres(&cache) {
            log::warn!("Could not persist Spotify artist genres: {error}");
        }
    }

    fn earliest_cooldown(&self) -> Option<u64> {
        match self.earliest_cooldown.load(Ordering::Relaxed) {
            0 => None,
            deadline => Some(deadline),
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn format_resume_time(deadline: u64) -> String {
    chrono::DateTime::from_timestamp(deadline as i64, 0)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%H:%M")
                .to_string()
        })
        .unwrap_or_else(|| deadline.to_string())
}

struct SyncHealth<'a> {
    genres_degraded: AtomicBool,
    partial: AtomicBool,
    run: Option<&'a SyncRun<'a>>,
}

impl<'a> SyncHealth<'a> {
    fn new(run: Option<&'a SyncRun<'a>>) -> Self {
        Self {
            genres_degraded: AtomicBool::new(false),
            partial: AtomicBool::new(false),
            run,
        }
    }

    fn skip_content_family(&self, family: &str) -> bool {
        if self.run.and_then(|run| run.cooldown(family)).is_some() {
            self.partial.store(true, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    fn snapshot_page<T>(&self, result: retune_spotify::Result<T>) -> Result<Option<T>, String> {
        match result {
            Ok(page) => Ok(Some(page)),
            Err(retune_spotify::Error::RateLimited {
                endpoint,
                retry_after_secs,
            }) => {
                if let Some(run) = self.run {
                    run.record_rate_limit(&endpoint, retry_after_secs);
                }
                if !self.partial.swap(true, Ordering::Relaxed) {
                    log::warn!(
                        "Spotify library snapshot is partial: Spotify rate limited {endpoint}; retry after {retry_after_secs}s"
                    );
                }
                Ok(None)
            }
            // Live payloads stray from the documented shapes; a page we cannot
            // decode should cost its section, not the whole import.
            Err(error @ retune_spotify::Error::Json { .. }) => {
                self.partial.store(true, Ordering::Relaxed);
                log::error!("Skipping rest of section, page failed to decode: {error}");
                Ok(None)
            }
            Err(error) => Err(error.to_string()),
        }
    }
}

struct GenreSource<'c, T: Transport, S: TokenStore> {
    client: &'c SpotifyClient<T, S>,
    health: &'c SyncHealth<'c>,
}

impl<'c, T: Transport, S: TokenStore> GenreSource<'c, T, S> {
    fn new(client: &'c SpotifyClient<T, S>, health: &'c SyncHealth<'c>) -> Self {
        Self { client, health }
    }

    async fn artist(&self, id: &str) -> Result<Option<Artist>, String> {
        if self.health.genres_degraded.load(Ordering::Relaxed) {
            return Ok(None);
        }
        if let Some(run) = self.health.run {
            if let Some(artist) = run.cached_artist(id) {
                return Ok(Some(artist));
            }
            if run.cooldown("/artists").is_some() {
                self.health.genres_degraded.store(true, Ordering::Relaxed);
                return Ok(None);
            }
        }
        match self.client.artist(id).await {
            Ok(artist) => {
                if let Some(run) = self.health.run {
                    run.cache_artist(&artist);
                }
                Ok(Some(artist))
            }
            Err(retune_spotify::Error::RateLimited {
                endpoint,
                retry_after_secs,
            }) => {
                if let Some(run) = self.health.run {
                    run.record_rate_limit(&endpoint, retry_after_secs);
                }
                if !self.health.genres_degraded.swap(true, Ordering::Relaxed) {
                    log::warn!(
                        "Spotify artist genres unavailable for this sync: Spotify rate limited {endpoint}; retry after {retry_after_secs}s"
                    );
                }
                Ok(None)
            }
            Err(error) => Err(error.to_string()),
        }
    }
}

async fn warm_artists<'a, T: Transport, S: TokenStore>(
    genres: &GenreSource<'_, T, S>,
    tracks: impl IntoIterator<Item = &'a retune_spotify::client::Track>,
    album: Option<&Album>,
) -> Result<(), String> {
    let ids = tracks
        .into_iter()
        .filter_map(|track| {
            track
                .artists
                .first()
                .or_else(|| album.and_then(|album| album.artists.first()))
                .map(|artist| artist.id.clone())
        })
        .collect::<HashSet<_>>();
    for id in ids {
        genres.artist(&id).await?;
    }
    Ok(())
}

async fn normalized_track<T: Transport, S: TokenStore>(
    genres: &GenreSource<'_, T, S>,
    track: &retune_spotify::client::Track,
    album: Option<&Album>,
) -> Result<NewTrack, String> {
    let artist = match track
        .artists
        .first()
        .or_else(|| album.and_then(|album| album.artists.first()))
    {
        Some(artist) => genres.artist(&artist.id).await?,
        None => None,
    };
    Ok(normalize::track(track, artist.as_ref(), album))
}

async fn normalized_album_tracks<T: Transport, S: TokenStore>(
    client: &SpotifyClient<T, S>,
    genres: &GenreSource<'_, T, S>,
    health: &SyncHealth<'_>,
    album: &Album,
) -> Result<(Vec<NewTrack>, bool), String> {
    let mut offset = 0;
    let mut tracks = vec![];
    let mut expected_total = None;
    if let Some(page) = album.tracks.clone() {
        let count = (page.items.len() + page.skipped) as u32;
        expected_total = Some(page.total);
        warm_artists(genres, &page.items, Some(album)).await?;
        for track in page.items {
            tracks.push(normalized_track(genres, &track, Some(album)).await?);
        }
        offset = count;
        if page.total <= offset {
            return Ok((tracks, false));
        }
    }
    loop {
        if health.skip_content_family("/albums") {
            return Ok((tracks, true));
        }
        let Some(page) =
            health.snapshot_page(client.album_tracks(&album.id, offset, PAGE_SIZE).await)?
        else {
            return Ok((tracks, true));
        };
        let count = (page.items.len() + page.skipped) as u32;
        warm_artists(genres, &page.items, Some(album)).await?;
        for track in page.items {
            tracks.push(normalized_track(genres, &track, Some(album)).await?);
        }
        offset += count;
        if count == 0
            || expected_total.is_some_and(|total| offset >= total)
            || (expected_total.is_none() && page.next.is_none())
        {
            return Ok((tracks, false));
        }
    }
}

async fn spotify_library_snapshot<'a, T: Transport, S: TokenStore>(
    client: &'a SpotifyClient<T, S>,
    kind: LibraryKind,
    run: Option<&'a SyncRun<'a>>,
) -> Result<Snapshot, String> {
    let health = SyncHealth::new(run);
    let genres = GenreSource::new(client, &health);
    let family = match kind {
        LibraryKind::Tracks => "/me/tracks",
        LibraryKind::Albums => "/me/albums",
        LibraryKind::Shows => "/me/shows",
        LibraryKind::Episodes => "/me/episodes",
        LibraryKind::Audiobooks => "/me/audiobooks",
    };
    if health.skip_content_family(family) {
        return Ok(Snapshot {
            batches: vec![],
            genres_degraded: false,
            partial: true,
        });
    }
    let mut offset = 0;
    let mut batches = vec![];
    'pages: loop {
        let (batch, count, has_next) = match kind {
            LibraryKind::Tracks => {
                let Some(page) =
                    health.snapshot_page(client.saved_tracks(offset, PAGE_SIZE).await)?
                else {
                    break 'pages;
                };
                let count = (page.items.len() + page.skipped) as u32;
                warm_artists(&genres, page.items.iter().map(|saved| &saved.track), None).await?;
                let mut batch = Vec::with_capacity(page.items.len());
                for saved in page.items {
                    batch.push(normalized_track(&genres, &saved.track, None).await?);
                }
                (batch, count, page.next.is_some())
            }
            LibraryKind::Albums => {
                let Some(page) =
                    health.snapshot_page(client.saved_albums(offset, PAGE_SIZE).await)?
                else {
                    break 'pages;
                };
                let count = (page.items.len() + page.skipped) as u32;
                let mut batch = vec![];
                for saved in page.items {
                    let (tracks, partial) =
                        normalized_album_tracks(client, &genres, &health, &saved.album).await?;
                    batch.extend(tracks);
                    if partial {
                        batches.push(batch);
                        break 'pages;
                    }
                }
                (batch, count, page.next.is_some())
            }
            LibraryKind::Shows => {
                let Some(page) =
                    health.snapshot_page(client.saved_shows(offset, PAGE_SIZE).await)?
                else {
                    break 'pages;
                };
                let count = (page.items.len() + page.skipped) as u32;
                let mut batch = vec![];
                for saved in page.items {
                    if health.skip_content_family("/shows") {
                        batches.push(batch);
                        break 'pages;
                    }
                    let Some(episodes) = health
                        .snapshot_page(client.show_episodes(&saved.show.id, 0, PAGE_SIZE).await)?
                    else {
                        batches.push(batch);
                        break 'pages;
                    };
                    batch.extend(
                        episodes
                            .items
                            .iter()
                            .map(|episode| normalize::episode(episode, Some(&saved.show))),
                    );
                }
                (batch, count, page.next.is_some())
            }
            LibraryKind::Episodes => {
                let Some(page) =
                    health.snapshot_page(client.saved_episodes(offset, PAGE_SIZE).await)?
                else {
                    break 'pages;
                };
                let count = (page.items.len() + page.skipped) as u32;
                let batch = page
                    .items
                    .iter()
                    .map(|saved| normalize::episode(&saved.episode, None))
                    .collect();
                (batch, count, page.next.is_some())
            }
            LibraryKind::Audiobooks => {
                let Some(page) =
                    health.snapshot_page(client.saved_audiobooks(offset, PAGE_SIZE).await)?
                else {
                    break 'pages;
                };
                let count = (page.items.len() + page.skipped) as u32;
                let mut batch = vec![];
                for book in page.items {
                    if health.skip_content_family("/audiobooks") {
                        batches.push(batch);
                        break 'pages;
                    }
                    let Some(chapters) = health
                        .snapshot_page(client.audiobook_chapters(&book.id, 0, PAGE_SIZE).await)?
                    else {
                        batches.push(batch);
                        break 'pages;
                    };
                    batch.extend(
                        chapters
                            .items
                            .iter()
                            .map(|chapter| normalize::chapter(chapter, &book)),
                    );
                }
                (batch, count, page.next.is_some())
            }
        };
        batches.push(batch);
        if !has_next || count == 0 {
            break;
        }
        offset += count;
    }
    Ok(Snapshot {
        batches,
        genres_degraded: health.genres_degraded.load(Ordering::Relaxed),
        partial: health.partial.load(Ordering::Relaxed),
    })
}

impl<T: Transport, S: TokenStore> MediaProvider for SpotifyClient<T, S> {
    async fn library_snapshot(&self, kind: LibraryKind) -> Result<Snapshot, String> {
        spotify_library_snapshot(self, kind, None).await
    }

    async fn search(&self, query: &str) -> Result<SearchResults, String> {
        let results = SpotifyClient::search(self, query, 0, SEARCH_PAGE_SIZE)
            .await
            .map_err(|error| error.to_string())?;
        Ok(SearchResults {
            artists: results
                .artists
                .items
                .into_iter()
                .map(|artist| SearchArtist {
                    uri: format!("spotify:artist:{}", artist.id),
                    name: artist.name,
                })
                .collect(),
            albums: results
                .albums
                .items
                .into_iter()
                .map(|album| SearchAlbum {
                    artist: album
                        .artists
                        .first()
                        .map(|artist| artist.name.clone())
                        .unwrap_or_default(),
                    name: album.name,
                    uri: album.uri,
                    track_count: None,
                })
                .collect(),
        })
    }

    async fn artist_albums(&self, artist: &str) -> Result<Vec<SearchAlbum>, String> {
        let id = artist.rsplit(':').next().unwrap_or(artist);
        let mut offset = 0;
        let mut albums = vec![];
        loop {
            let page = SpotifyClient::artist_albums(self, id, offset, PAGE_SIZE)
                .await
                .map_err(|error| error.to_string())?;
            let count = (page.items.len() + page.skipped) as u32;
            albums.extend(page.items.into_iter().map(|album| {
                SearchAlbum {
                    artist: album
                        .artists
                        .first()
                        .map(|artist| artist.name.clone())
                        .unwrap_or_default(),
                    name: album.name,
                    uri: album.uri,
                    track_count: None,
                }
            }));
            if page.next.is_none() || count == 0 {
                return Ok(albums);
            }
            offset += count;
        }
    }

    async fn album_tracks(&self, album: &str) -> Result<Vec<NewTrack>, String> {
        let health = SyncHealth::new(None);
        let genres = GenreSource::new(self, &health);
        let id = album.rsplit(':').next().unwrap_or(album);
        let mut offset = 0;
        let mut normalized = vec![];
        loop {
            let page = SpotifyClient::album_tracks(self, id, offset, PAGE_SIZE)
                .await
                .map_err(|error| error.to_string())?;
            let count = (page.items.len() + page.skipped) as u32;
            warm_artists(&genres, &page.items, None).await?;
            for track in page.items {
                normalized.push(normalized_track(&genres, &track, None).await?);
            }
            if page.next.is_none() || count == 0 {
                return Ok(normalized);
            }
            offset += count;
        }
    }

    async fn save_to_spotify(&self, uris: &[String]) -> Result<(), String> {
        self.save_to_library(uris)
            .await
            .map_err(|error| error.to_string())
    }
}

impl<T: Transport, S: TokenStore> MediaProvider for SpotifySyncProvider<'_, T, S> {
    async fn library_snapshot(&self, kind: LibraryKind) -> Result<Snapshot, String> {
        spotify_library_snapshot(self.client, kind, Some(&self.run)).await
    }

    fn earliest_cooldown(&self) -> Option<u64> {
        self.run.earliest_cooldown()
    }

    fn request_counts(&self) -> BTreeMap<String, u64> {
        self.client.request_counts()
    }

    async fn search(&self, query: &str) -> Result<SearchResults, String> {
        MediaProvider::search(self.client, query).await
    }

    async fn artist_albums(&self, artist: &str) -> Result<Vec<SearchAlbum>, String> {
        MediaProvider::artist_albums(self.client, artist).await
    }

    async fn album_tracks(&self, album: &str) -> Result<Vec<NewTrack>, String> {
        MediaProvider::album_tracks(self.client, album).await
    }

    async fn save_to_spotify(&self, uris: &[String]) -> Result<(), String> {
        MediaProvider::save_to_spotify(self.client, uris).await
    }
}

#[cfg(test)]
#[derive(Default)]
pub struct FakeProvider {
    pub snapshots: std::collections::HashMap<LibraryKind, Vec<Vec<NewTrack>>>,
    pub genres_degraded: bool,
    pub partial: bool,
}

#[cfg(test)]
impl MediaProvider for FakeProvider {
    async fn library_snapshot(&self, kind: LibraryKind) -> Result<Snapshot, String> {
        Ok(Snapshot {
            batches: self.snapshots.get(&kind).cloned().unwrap_or_default(),
            genres_degraded: self.genres_degraded,
            partial: self.partial,
        })
    }

    async fn search(&self, _query: &str) -> Result<SearchResults, String> {
        Ok(SearchResults {
            artists: vec![],
            albums: vec![],
        })
    }

    async fn artist_albums(&self, _artist: &str) -> Result<Vec<SearchAlbum>, String> {
        Ok(vec![])
    }

    async fn album_tracks(&self, _album: &str) -> Result<Vec<NewTrack>, String> {
        Ok(vec![])
    }

    async fn save_to_spotify(&self, _uris: &[String]) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use retune_core::model::SourceId;
    use retune_spotify::{
        client::{FakeTransport, Response},
        tokens::{InMemoryTokenStore, Tokens},
    };

    use super::*;

    fn client(
        responses: impl IntoIterator<Item = Response>,
    ) -> SpotifyClient<FakeTransport, InMemoryTokenStore> {
        SpotifyClient::new(
            "client",
            FakeTransport::new(responses),
            InMemoryTokenStore::new(Some(Tokens {
                access: "access".into(),
                refresh: "refresh".into(),
                expires_at: u64::MAX,
                scopes: String::new(),
            })),
        )
    }

    fn rate_limited() -> Response {
        let mut response = Response::json(429, serde_json::json!({}));
        response.headers.insert("retry-after".into(), "3600".into());
        response
    }

    fn assert_track(
        track: &NewTrack,
        source: SourceId,
        uri: &str,
        cat: &str,
        art: &str,
        alb: &str,
        name: &str,
    ) {
        assert_eq!(track.source, source);
        assert_eq!(track.uri, uri);
        assert_eq!(track.cat, cat);
        assert_eq!(track.art, art);
        assert_eq!(track.alb, alb);
        assert_eq!(track.name, name);
    }

    #[tokio::test]
    async fn spotify_provider_pages_and_normalizes_saved_tracks() {
        let client = client([
            Response::json(
                200,
                serde_json::json!({"items": [{"track": {
                    "uri": "spotify:track:1", "name": "One", "duration_ms": 1000,
                    "artists": [{"id": "artist-1", "name": "Artist"}],
                    "album": {"id": "album-1", "uri": "spotify:album:1", "name": "Album", "images": []}
                }}], "next": "next"}),
            ),
            Response::json(
                200,
                serde_json::json!({"id": "artist-1", "name": "Artist", "genres": ["rock"]}),
            ),
            Response::json(200, serde_json::json!({"items": [], "next": null})),
        ]);
        let snapshot = MediaProvider::library_snapshot(&client, LibraryKind::Tracks)
            .await
            .unwrap();
        let batches = snapshot.batches;

        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0][0].cat, "rock");
        assert_eq!(batches[0][0].alb, "Album");
        assert!(client.transport().requests()[2]
            .url
            .ends_with("offset=1&limit=50"));
    }

    #[tokio::test]
    async fn saved_albums_expand_tracks_and_normalize_album_metadata() {
        let client = client([
            Response::json(
                200,
                serde_json::json!({"items": [{"album": {
                "id": "album-1", "uri": "spotify:album:1", "name": "Record",
                "artists": [{"id": "artist-1", "name": "Artist"}], "images": []
            }}], "next": null}),
            ),
            Response::json(
                200,
                serde_json::json!({"items": [{
                "uri": "spotify:track:1", "name": "Song", "duration_ms": 1234,
                "artists": [], "album": null
            }], "next": null}),
            ),
            Response::json(
                200,
                serde_json::json!({
                    "id": "artist-1", "name": "Artist", "genres": ["rock"]
                }),
            ),
        ]);

        let batches = client
            .library_snapshot(LibraryKind::Albums)
            .await
            .unwrap()
            .batches;

        assert_track(
            &batches[0][0],
            SourceId::Music,
            "spotify:track:1",
            "rock",
            "Artist",
            "Record",
            "Song",
        );
        assert_eq!(batches[0][0].duration.as_millis(), 1234);
    }

    #[tokio::test]
    async fn saved_album_uses_embedded_tracks_without_album_track_request() {
        let client = client([Response::json(
            200,
            serde_json::json!({"items": [{"album": {
                "id": "album-1", "uri": "spotify:album:1", "name": "Record",
                "artists": [], "images": [],
                "tracks": {"items": [{
                    "uri": "spotify:track:1", "name": "Song", "duration_ms": 1234,
                    "artists": [], "album": null
                }], "next": null, "total": 1}
            }}], "next": null}),
        )]);

        let snapshot = client.library_snapshot(LibraryKind::Albums).await.unwrap();

        assert_eq!(snapshot.batches[0].len(), 1);
        assert_eq!(client.transport().requests().len(), 1);
        assert!(!client.transport().requests()[0].url.contains("/albums/"));
    }

    #[tokio::test]
    async fn saved_album_fetches_only_pages_after_the_embedded_page() {
        let tracks = |start: usize, count: usize| {
            (start..start + count)
                .map(|index| {
                    serde_json::json!({
                        "uri": format!("spotify:track:{index}"), "name": format!("Song {index}"),
                        "duration_ms": 1000, "artists": [], "album": null
                    })
                })
                .collect::<Vec<_>>()
        };
        let client = client([
            Response::json(
                200,
                serde_json::json!({"items": [{"album": {
                    "id": "album-1", "uri": "spotify:album:1", "name": "Record",
                    "artists": [], "images": [],
                    "tracks": {"items": tracks(0, 50), "next": "next", "total": 120}
                }}], "next": null}),
            ),
            Response::json(
                200,
                serde_json::json!({"items": tracks(50, 50), "next": "next", "total": 120}),
            ),
            Response::json(
                200,
                serde_json::json!({"items": tracks(100, 20), "next": null, "total": 120}),
            ),
        ]);

        let snapshot = client.library_snapshot(LibraryKind::Albums).await.unwrap();
        let album_requests = client
            .transport()
            .requests()
            .into_iter()
            .filter(|request| request.url.contains("/albums/album-1/tracks"))
            .collect::<Vec<_>>();

        assert_eq!(snapshot.batches[0].len(), 120);
        assert_eq!(album_requests.len(), 2);
        assert!(album_requests[0].url.contains("offset=50&limit=50"));
        assert!(album_requests[1].url.contains("offset=100&limit=50"));
    }

    #[tokio::test]
    async fn album_track_rate_limit_keeps_previously_collected_tracks() {
        let client = client([
            Response::json(
                200,
                serde_json::json!({"items": [
                    {"track": {"uri": "spotify:track:1", "name": "One", "duration_ms": 1000,
                        "artists": [], "album": null}},
                    {"track": {"uri": "spotify:track:2", "name": "Two", "duration_ms": 1000,
                        "artists": [], "album": null}}
                ], "next": null}),
            ),
            Response::json(
                200,
                serde_json::json!({"items": [{"album": {
                    "id": "album-1", "uri": "spotify:album:1", "name": "Record",
                    "artists": [], "images": []
                }}], "next": null}),
            ),
            rate_limited(),
        ]);

        let tracks = client.library_snapshot(LibraryKind::Tracks).await.unwrap();
        let albums = client.library_snapshot(LibraryKind::Albums).await.unwrap();

        assert_eq!(tracks.batches[0].len(), 2);
        assert!(!tracks.partial);
        assert!(albums.partial);
        assert!(albums.batches[0].is_empty());
        assert_eq!(
            client
                .transport()
                .requests()
                .iter()
                .filter(|request| request.url.contains("/albums/album-1/tracks"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn first_saved_tracks_rate_limit_returns_empty_partial_snapshot() {
        let client = client([rate_limited()]);

        let snapshot = client.library_snapshot(LibraryKind::Tracks).await.unwrap();

        assert!(snapshot.batches.is_empty());
        assert!(snapshot.partial);
    }

    #[tokio::test]
    async fn active_cooldown_skips_section_without_a_request() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSyncStore::new(dir.path());
        let deadline = unix_now() + 3_600;
        store
            .save_cooldowns(&BTreeMap::from([("/me/tracks".into(), deadline)]))
            .unwrap();
        let client = client([]);
        let provider = SpotifySyncProvider::new(&client, &store).unwrap();

        let snapshot = provider
            .library_snapshot(LibraryKind::Tracks)
            .await
            .unwrap();

        assert!(snapshot.partial);
        assert!(snapshot.batches.is_empty());
        assert_eq!(provider.earliest_cooldown(), Some(deadline));
        assert!(client.transport().requests().is_empty());
    }

    #[tokio::test]
    async fn expired_cooldown_is_cleared_and_section_runs() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSyncStore::new(dir.path());
        store
            .save_cooldowns(&BTreeMap::from([(
                "/me/tracks".into(),
                unix_now().saturating_sub(1),
            )]))
            .unwrap();
        let client = client([Response::json(
            200,
            serde_json::json!({"items": [], "next": null}),
        )]);
        let provider = SpotifySyncProvider::new(&client, &store).unwrap();

        let snapshot = provider
            .library_snapshot(LibraryKind::Tracks)
            .await
            .unwrap();

        assert!(!snapshot.partial);
        assert_eq!(client.transport().requests().len(), 1);
        assert!(store.cooldowns(unix_now()).unwrap().is_empty());
    }

    #[tokio::test]
    async fn live_rate_limit_persists_its_endpoint_family() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSyncStore::new(dir.path());
        let client = client([rate_limited()]);
        let provider = SpotifySyncProvider::new(&client, &store).unwrap();

        let snapshot = provider
            .library_snapshot(LibraryKind::Tracks)
            .await
            .unwrap();
        let cooldowns = FsSyncStore::new(dir.path()).cooldowns(unix_now()).unwrap();

        assert!(snapshot.partial);
        assert!(cooldowns["/me/tracks"] > unix_now());
    }

    #[tokio::test]
    async fn persistent_artist_cache_hit_skips_artist_request() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSyncStore::new(dir.path());
        store
            .save_artist_genres(&BTreeMap::from([("artist-1".into(), vec!["rock".into()])]))
            .unwrap();
        let client = client([Response::json(
            200,
            serde_json::json!({"items": [{"track": {
                "uri": "spotify:track:1", "name": "One", "duration_ms": 1000,
                "artists": [{"id": "artist-1", "name": "Artist"}], "album": null
            }}], "next": null}),
        )]);
        let provider = SpotifySyncProvider::new(&client, &store).unwrap();

        let snapshot = provider
            .library_snapshot(LibraryKind::Tracks)
            .await
            .unwrap();

        assert_eq!(snapshot.batches[0][0].cat, "rock");
        assert_eq!(client.transport().requests().len(), 1);
        assert_eq!(provider.request_counts()["/me/tracks"], 1);
    }

    #[tokio::test]
    async fn persistent_artist_cache_miss_is_written_through() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSyncStore::new(dir.path());
        let client = client([
            Response::json(
                200,
                serde_json::json!({"items": [{"track": {
                    "uri": "spotify:track:1", "name": "One", "duration_ms": 1000,
                    "artists": [{"id": "artist-1", "name": "Artist"}], "album": null
                }}], "next": null}),
            ),
            Response::json(
                200,
                serde_json::json!({"id": "artist-1", "name": "Artist", "genres": ["rock"]}),
            ),
        ]);
        let provider = SpotifySyncProvider::new(&client, &store).unwrap();

        provider
            .library_snapshot(LibraryKind::Tracks)
            .await
            .unwrap();

        assert_eq!(client.transport().requests().len(), 2);
        assert_eq!(
            FsSyncStore::new(dir.path()).artist_genres().unwrap()["artist-1"],
            ["rock"]
        );
    }

    #[tokio::test]
    async fn saved_shows_expand_episodes() {
        let client = client([
            Response::json(
                200,
                serde_json::json!({"items": [{"show": {
                "id": "show-1", "uri": "spotify:show:1", "name": "Show",
                "publisher": "Publisher", "category": "Technology", "images": []
            }}], "next": null}),
            ),
            Response::json(
                200,
                serde_json::json!({"items": [{
                "uri": "spotify:episode:1", "name": "Episode", "duration_ms": 2000,
                "show": null
                }], "next": null}),
            ),
        ]);

        let batches = client
            .library_snapshot(LibraryKind::Shows)
            .await
            .unwrap()
            .batches;

        assert_track(
            &batches[0][0],
            SourceId::Podcasts,
            "spotify:episode:1",
            "Technology",
            "Publisher",
            "Show",
            "Episode",
        );
        assert_eq!(batches[0][0].duration.as_millis(), 2000);
        assert_eq!(client.transport().requests().len(), 2);
    }

    #[tokio::test]
    async fn saved_episodes_use_embedded_show_metadata() {
        let client = client([Response::json(
            200,
            serde_json::json!({"items": [{"episode": {
            "uri": "spotify:episode:2", "name": "Saved Episode", "duration_ms": 3000,
            "show": {"id": "show-2", "uri": "spotify:show:2", "name": "Saved Show",
                "publisher": "Host", "category": null, "images": []}
        }}], "next": null}),
        )]);

        let batches = client
            .library_snapshot(LibraryKind::Episodes)
            .await
            .unwrap()
            .batches;

        assert_track(
            &batches[0][0],
            SourceId::Podcasts,
            "spotify:episode:2",
            "Uncategorized",
            "Host",
            "Saved Show",
            "Saved Episode",
        );
        assert_eq!(batches[0][0].duration.as_millis(), 3000);
    }

    #[tokio::test]
    async fn saved_audiobooks_expand_chapters() {
        let client = client([
            Response::json(
                200,
                serde_json::json!({"items": [{
                "id": "book-1", "uri": "spotify:audiobook:1", "name": "Book",
                "authors": [{"name": "Author"}], "genres": ["History"], "images": []
                }], "next": null}),
            ),
            Response::json(
                200,
                serde_json::json!({"items": [{
                "uri": "spotify:chapter:1", "name": "Chapter", "duration_ms": 4000
            }], "next": "next"}),
            ),
        ]);

        let batches = client
            .library_snapshot(LibraryKind::Audiobooks)
            .await
            .unwrap()
            .batches;

        assert_track(
            &batches[0][0],
            SourceId::Audiobooks,
            "spotify:chapter:1",
            "History",
            "Author",
            "Book",
            "Chapter",
        );
        assert_eq!(batches[0][0].duration.as_millis(), 4000);
        assert_eq!(client.transport().requests().len(), 2);
    }

    #[tokio::test]
    async fn search_maps_results_and_uses_search_page_limit() {
        let client = client([Response::json(
            200,
            serde_json::json!({
                "artists": {"items": [{"id": "artist-1", "name": "Artist", "genres": []}], "next": null},
                "albums": {"items": [{"id": "album-1", "uri": "spotify:album:1", "name": "Album",
                    "artists": [{"id": "artist-1", "name": "Artist"}], "images": []}], "next": null}
            }),
        )]);

        let results = MediaProvider::search(&client, "artist").await.unwrap();

        assert_eq!(
            results.artists[0],
            SearchArtist {
                name: "Artist".into(),
                uri: "spotify:artist:artist-1".into()
            }
        );
        assert_eq!(results.albums[0].uri, "spotify:album:1");
        assert!(client.transport().requests()[0]
            .url
            .ends_with("offset=0&limit=10"));
    }

    #[tokio::test]
    async fn artist_albums_pages_by_artist_id() {
        let client = client([Response::json(
            200,
            serde_json::json!({"items": [{
            "id": "album-1", "uri": "spotify:album:1", "name": "Album",
            "artists": [{"id": "artist-1", "name": "Artist"}], "images": []
        }], "next": null}),
        )]);

        let albums = MediaProvider::artist_albums(&client, "spotify:artist:artist-1")
            .await
            .unwrap();

        assert_eq!(albums[0].name, "Album");
        assert!(client.transport().requests()[0]
            .url
            .contains("/artists/artist-1/albums?"));
    }

    #[tokio::test]
    async fn undecodable_item_is_skipped_and_the_rest_of_the_page_imports() {
        let client = client([Response::json(
            200,
            serde_json::json!({"items": [
                {"episode": {"uri": "spotify:episode:1", "name": "Ep", "duration_ms": "bogus"}},
                {"episode": {"uri": "spotify:episode:2", "name": "Good", "duration_ms": 2000,
                    "show": {"id": "show-1", "uri": "spotify:show:1", "name": "Show",
                        "publisher": "Host", "category": null, "images": []}}}
            ], "next": null}),
        )]);

        let snapshot = client
            .library_snapshot(LibraryKind::Episodes)
            .await
            .unwrap();

        assert!(!snapshot.partial);
        let tracks: Vec<_> = snapshot.batches.concat();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].uri, "spotify:episode:2");
    }

    #[tokio::test]
    async fn undecodable_page_degrades_to_partial_instead_of_failing() {
        let client = client([Response::json(200, serde_json::json!({"items": "bogus"}))]);

        let snapshot = client
            .library_snapshot(LibraryKind::Episodes)
            .await
            .unwrap();

        assert!(snapshot.partial);
        assert!(snapshot.batches.iter().all(|batch| batch.is_empty()));
    }

    #[tokio::test]
    async fn album_add_uses_query_only_library_request() {
        let client = client([Response::json(204, serde_json::Value::Null)]);

        client
            .save_to_spotify(&["spotify:album:1".into()])
            .await
            .unwrap();

        let request = &client.transport().requests()[0];
        assert!(request.url.contains("/me/library?uris=spotify%3Aalbum%3A1"));
        assert!(request.body.is_empty());
    }

    #[tokio::test]
    async fn distinct_artist_lookups_are_bounded_and_duplicate_ids_stay_cached() {
        let tracks = (0..5)
            .map(|index| serde_json::json!({
                "track": {"uri": format!("spotify:track:{index}"), "name": format!("Track {index}"),
                    "duration_ms": 1000, "artists": [{"id": format!("artist-{}", index % 4), "name": "Artist"}], "album": null}
            }))
            .collect::<Vec<_>>();
        let mut responses = vec![Response::json(
            200,
            serde_json::json!({"items": tracks, "next": null}),
        )];
        responses.extend((0..4).map(|index| {
            Response::json(
                200,
                serde_json::json!({
                    "id": format!("artist-{index}"), "name": "Artist", "genres": ["genre"]
                }),
            )
        }));
        let client = client(responses);

        let batches = client
            .library_snapshot(LibraryKind::Tracks)
            .await
            .unwrap()
            .batches;

        assert_eq!(batches[0].len(), 5);
        assert!(batches[0].iter().all(|track| track.cat == "genre"));
        assert_eq!(
            client
                .transport()
                .requests()
                .iter()
                .filter(|request| request.url.contains("/artists/"))
                .count(),
            4
        );
    }

    #[tokio::test]
    async fn artist_rate_limit_degrades_genres_and_opens_breaker() {
        let client = client([
            Response::json(
                200,
                serde_json::json!({"items": [
                    {"track": {"uri": "spotify:track:1", "name": "One", "duration_ms": 1000,
                        "artists": [{"id": "artist-1", "name": "One"}], "album": null}},
                    {"track": {"uri": "spotify:track:2", "name": "Two", "duration_ms": 1000,
                        "artists": [{"id": "artist-1", "name": "One"}], "album": null}},
                    {"track": {"uri": "spotify:track:3", "name": "Three", "duration_ms": 1000,
                        "artists": [{"id": "artist-2", "name": "Two"}], "album": null}}
                ], "next": null}),
            ),
            rate_limited(),
        ]);

        let snapshot = client.library_snapshot(LibraryKind::Tracks).await.unwrap();

        assert!(snapshot.genres_degraded);
        assert_eq!(snapshot.batches[0].len(), 3);
        assert!(snapshot.batches[0]
            .iter()
            .all(|track| track.cat == normalize::UNCATEGORIZED));
        assert_eq!(
            client
                .transport()
                .requests()
                .iter()
                .filter(|request| request.url.contains("/artists/"))
                .count(),
            1
        );
    }
}

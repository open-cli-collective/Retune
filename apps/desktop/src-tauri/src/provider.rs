use retune_core::model::NewTrack;
use retune_spotify::{
    client::{endpoint_family, Album, Artist, Image, SpotifyClient, Transport},
    normalize,
    tokens::TokenStore,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use crate::store::FsSyncStore;

const PAGE_SIZE: u32 = 50;
const SEARCH_PAGE_SIZE: u32 = 10;
const ARTIST_LOOKUPS_PER_SYNC: usize = 100;
#[cfg(not(test))]
const ARTIST_LOOKUP_PACE_MS: u64 = 250;

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

    pub const fn label(self) -> &'static str {
        match self {
            Self::Tracks => "tracks",
            Self::Albums => "albums",
            Self::Shows => "podcasts",
            Self::Episodes => "episodes",
            Self::Audiobooks => "audiobooks",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchArtist {
    pub id: String,
    pub name: String,
    pub descriptor: String,
    pub image_url: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchAlbum {
    pub uri: String,
    pub name: String,
    pub artist: String,
    pub year: Option<String>,
    pub image_url: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchTrack {
    pub uri: String,
    pub name: String,
    pub artist: String,
    pub alb: String,
    pub duration_secs: u64,
    pub image_url: Option<String>,
    pub album_uri: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct SearchResults {
    pub artists: Vec<SearchArtist>,
    pub albums: Vec<SearchAlbum>,
    pub tracks: Vec<SearchTrack>,
}

pub struct Snapshot {
    pub batches: Vec<Vec<NewTrack>>,
    pub genres_degraded: bool,
    pub partial: bool,
    pub progress: Option<SectionProgress>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SectionProgress {
    pub label: &'static str,
    pub done: u32,
    pub total: Option<u32>,
}

pub struct SyncBatch {
    pub tracks: Vec<NewTrack>,
    pub done: u32,
    pub total: Option<u32>,
    pub section: &'static str,
}

pub trait MediaProvider: Send + Sync {
    async fn library_snapshot(
        &self,
        kind: LibraryKind,
        on_batch: &(dyn Fn(SyncBatch) + Send + Sync),
    ) -> Result<Snapshot, String>;
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
                pending_music: Mutex::new(vec![]),
                artist_lookups: AtomicUsize::new(0),
                earliest_cooldown: AtomicU64::new(0),
            },
        })
    }
}

struct SyncRun<'a> {
    store: &'a FsSyncStore,
    cooldowns: Mutex<BTreeMap<String, u64>>,
    artist_genres: Mutex<BTreeMap<String, Vec<String>>>,
    pending_music: Mutex<Vec<Vec<PendingTrack>>>,
    artist_lookups: AtomicUsize,
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
            log::warn!(
                "skipping {family} until {}",
                format_resume_time(deadline, chrono::Local::now())
            );
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
                followers: None,
                images: vec![],
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

    fn defer_music(&self, batches: Vec<Vec<PendingTrack>>) {
        self.pending_music
            .lock()
            .expect("pending music mutex poisoned")
            .extend(batches);
    }

    fn take_pending_music(&self) -> Vec<Vec<PendingTrack>> {
        std::mem::take(
            &mut *self
                .pending_music
                .lock()
                .expect("pending music mutex poisoned"),
        )
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn compact_count(count: u64) -> String {
    if count >= 1_000_000 {
        let value = count as f64 / 1_000_000.0;
        if count % 1_000_000 < 100_000 {
            format!("{value:.0}M")
        } else {
            format!("{value:.1}M")
        }
    } else if count >= 1_000 {
        format!("{}K", count / 1_000)
    } else {
        count.to_string()
    }
}

fn title_case(value: &str) -> String {
    value
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect())
                .unwrap_or_default()
        })
        .collect::<Vec<String>>()
        .join(" ")
}

fn artist_descriptor(artist: &Artist) -> String {
    let mut parts = vec![];
    if let Some(genre) = artist.genres.first() {
        parts.push(title_case(genre));
    }
    if let Some(followers) = &artist.followers {
        parts.push(format!("{} followers", compact_count(followers.total)));
    }
    if parts.is_empty() {
        "Artist".into()
    } else {
        parts.join(" · ")
    }
}

fn image_url(images: &[Image]) -> Option<String> {
    images
        .iter()
        .filter(|image| image.width.is_some_and(|width| width >= 64))
        .min_by_key(|image| image.width)
        .or_else(|| images.last())
        .map(|image| image.url.clone())
}

pub fn format_resume_time(deadline: u64, now: chrono::DateTime<chrono::Local>) -> String {
    chrono::DateTime::from_timestamp(deadline as i64, 0)
        .map(|time| {
            let deadline = time.with_timezone(&chrono::Local);
            let days = deadline
                .date_naive()
                .signed_duration_since(now.date_naive())
                .num_days();
            match days {
                ..=0 => deadline.format("%H:%M").to_string(),
                1 => deadline.format("tomorrow at %H:%M").to_string(),
                _ => deadline.format("%b %-d at %H:%M").to_string(),
            }
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
            Err(retune_spotify::Error::ServerError { endpoint, status }) => {
                if !self.partial.swap(true, Ordering::Relaxed) {
                    log::warn!(
                        "Spotify library snapshot is partial: Spotify {endpoint} failed with HTTP {status} after retries"
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
        if let Some(run) = self.health.run {
            if let Some(artist) = run.cached_artist(id) {
                return Ok(Some(artist));
            }
        }
        if self.health.genres_degraded.load(Ordering::Relaxed) {
            return Ok(None);
        }
        if let Some(run) = self.health.run {
            if run.cooldown("/artists").is_some() {
                self.health.genres_degraded.store(true, Ordering::Relaxed);
                return Ok(None);
            }
            let lookup =
                run.artist_lookups
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |lookups| {
                        (lookups < ARTIST_LOOKUPS_PER_SYNC).then_some(lookups + 1)
                    });
            let Ok(lookup) = lookup else {
                self.health.genres_degraded.store(true, Ordering::Relaxed);
                return Ok(None);
            };
            pace_artist_lookup(lookup).await;
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
            Err(retune_spotify::Error::ServerError { endpoint, status }) => {
                log::warn!(
                    "Skipping Spotify artist genre lookup: Spotify {endpoint} failed with HTTP {status} after retries"
                );
                Ok(None)
            }
            Err(error) => Err(error.to_string()),
        }
    }
}

#[cfg(not(test))]
async fn pace_artist_lookup(lookup: usize) {
    if lookup > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(ARTIST_LOOKUP_PACE_MS)).await;
    }
}

#[cfg(test)]
async fn pace_artist_lookup(_lookup: usize) {}

#[derive(Clone)]
struct PendingTrack {
    track: NewTrack,
    artist_id: Option<String>,
}

fn normalized_track(track: &retune_spotify::client::Track, album: Option<&Album>) -> PendingTrack {
    let artist_id = track
        .artists
        .first()
        .or_else(|| album.and_then(|album| album.artists.first()))
        .map(|artist| artist.id.clone());
    PendingTrack {
        track: normalize::track(track, None, album),
        artist_id,
    }
}

async fn enrich_music<T: Transport, S: TokenStore>(
    genres: &GenreSource<'_, T, S>,
    mut batches: Vec<Vec<PendingTrack>>,
) -> Result<Vec<Vec<NewTrack>>, String> {
    let ids = batches
        .iter()
        .flatten()
        .filter_map(|pending| pending.artist_id.clone())
        .collect::<BTreeSet<_>>();
    let mut categories = BTreeMap::new();
    for id in ids {
        if let Some(category) = genres
            .artist(&id)
            .await?
            .and_then(|artist| artist.genres.into_iter().next())
        {
            categories.insert(id, category);
        }
    }
    Ok(batches
        .iter_mut()
        .map(|batch| {
            batch
                .drain(..)
                .map(|mut pending| {
                    if let Some(category) =
                        pending.artist_id.as_ref().and_then(|id| categories.get(id))
                    {
                        pending.track.cat.clone_from(category);
                    }
                    pending.track
                })
                .collect()
        })
        .collect())
}

async fn normalized_album_tracks<T: Transport, S: TokenStore>(
    client: &SpotifyClient<T, S>,
    health: &SyncHealth<'_>,
    album: &Album,
) -> Result<(Vec<PendingTrack>, bool), String> {
    let mut offset = 0;
    let mut tracks = vec![];
    let mut expected_total = None;
    if let Some(page) = album.tracks.clone() {
        let count = (page.items.len() + page.skipped) as u32;
        expected_total = Some(page.total);
        for track in page.items {
            tracks.push(normalized_track(&track, Some(album)));
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
        for track in page.items {
            tracks.push(normalized_track(&track, Some(album)));
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
    on_batch: &(dyn Fn(SyncBatch) + Send + Sync),
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
    let label = kind.label();
    if health.skip_content_family(family) {
        let batches = if kind == LibraryKind::Audiobooks {
            match run {
                Some(run) => enrich_music(&genres, run.take_pending_music()).await?,
                None => vec![],
            }
        } else {
            vec![]
        };
        return Ok(Snapshot {
            batches,
            genres_degraded: health.genres_degraded.load(Ordering::Relaxed),
            partial: true,
            progress: Some(SectionProgress {
                label,
                done: 0,
                total: None,
            }),
        });
    }
    let mut offset = 0;
    let mut done = 0;
    let mut total = None;
    let mut batches = vec![];
    let mut music_batches = vec![];
    'pages: loop {
        let (batch, count, has_next) = match kind {
            LibraryKind::Tracks => {
                let Some(page) =
                    health.snapshot_page(client.saved_tracks(offset, PAGE_SIZE).await)?
                else {
                    break 'pages;
                };
                total.get_or_insert(page.total);
                let count = (page.items.len() + page.skipped) as u32;
                let batch = page
                    .items
                    .into_iter()
                    .map(|saved| normalized_track(&saved.track, None))
                    .collect::<Vec<_>>();
                done += count;
                on_batch(SyncBatch {
                    tracks: batch.iter().map(|pending| pending.track.clone()).collect(),
                    done,
                    total,
                    section: label,
                });
                music_batches.push(batch);
                (vec![], count, page.next.is_some())
            }
            LibraryKind::Albums => {
                let Some(page) =
                    health.snapshot_page(client.saved_albums(offset, PAGE_SIZE).await)?
                else {
                    break 'pages;
                };
                total.get_or_insert(page.total);
                let count = (page.items.len() + page.skipped) as u32;
                let mut batch = vec![];
                for saved in page.items {
                    let (tracks, partial) =
                        normalized_album_tracks(client, &health, &saved.album).await?;
                    batch.extend(tracks);
                    if partial {
                        on_batch(SyncBatch {
                            tracks: batch.iter().map(|pending| pending.track.clone()).collect(),
                            done,
                            total,
                            section: label,
                        });
                        music_batches.push(batch);
                        break 'pages;
                    }
                    done += 1;
                }
                on_batch(SyncBatch {
                    tracks: batch.iter().map(|pending| pending.track.clone()).collect(),
                    done,
                    total,
                    section: label,
                });
                music_batches.push(batch);
                (vec![], count, page.next.is_some())
            }
            LibraryKind::Shows => {
                let Some(page) =
                    health.snapshot_page(client.saved_shows(offset, PAGE_SIZE).await)?
                else {
                    break 'pages;
                };
                total.get_or_insert(page.total);
                let count = (page.items.len() + page.skipped) as u32;
                let mut batch = vec![];
                for saved in page.items {
                    if health.skip_content_family("/shows") {
                        on_batch(SyncBatch {
                            tracks: batch.clone(),
                            done,
                            total,
                            section: label,
                        });
                        batches.push(batch);
                        break 'pages;
                    }
                    let Some(episodes) = health
                        .snapshot_page(client.show_episodes(&saved.show.id, 0, PAGE_SIZE).await)?
                    else {
                        on_batch(SyncBatch {
                            tracks: batch.clone(),
                            done,
                            total,
                            section: label,
                        });
                        batches.push(batch);
                        break 'pages;
                    };
                    done += 1;
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
                total.get_or_insert(page.total);
                let count = (page.items.len() + page.skipped) as u32;
                let batch = page
                    .items
                    .iter()
                    .map(|saved| normalize::episode(&saved.episode, None))
                    .collect();
                done += count;
                (batch, count, page.next.is_some())
            }
            LibraryKind::Audiobooks => {
                let Some(page) =
                    health.snapshot_page(client.saved_audiobooks(offset, PAGE_SIZE).await)?
                else {
                    break 'pages;
                };
                total.get_or_insert(page.total);
                let count = (page.items.len() + page.skipped) as u32;
                let mut batch = vec![];
                for book in page.items {
                    if health.skip_content_family("/audiobooks") {
                        on_batch(SyncBatch {
                            tracks: batch.clone(),
                            done,
                            total,
                            section: label,
                        });
                        batches.push(batch);
                        break 'pages;
                    }
                    let Some(chapters) = health
                        .snapshot_page(client.audiobook_chapters(&book.id, 0, PAGE_SIZE).await)?
                    else {
                        on_batch(SyncBatch {
                            tracks: batch.clone(),
                            done,
                            total,
                            section: label,
                        });
                        batches.push(batch);
                        break 'pages;
                    };
                    done += 1;
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
        if !matches!(kind, LibraryKind::Tracks | LibraryKind::Albums) {
            on_batch(SyncBatch {
                tracks: batch.clone(),
                done,
                total,
                section: label,
            });
            batches.push(batch);
        }
        if !has_next || count == 0 {
            break;
        }
        offset += count;
    }
    if let Some(run) = run {
        run.defer_music(music_batches);
        if kind == LibraryKind::Audiobooks {
            let mut music = enrich_music(&genres, run.take_pending_music()).await?;
            music.append(&mut batches);
            batches = music;
        }
    } else {
        let mut music = enrich_music(&genres, music_batches).await?;
        music.append(&mut batches);
        batches = music;
    }
    let partial = health.partial.load(Ordering::Relaxed);
    Ok(Snapshot {
        batches,
        genres_degraded: health.genres_degraded.load(Ordering::Relaxed),
        partial,
        progress: partial.then_some(SectionProgress { label, done, total }),
    })
}

impl<T: Transport, S: TokenStore> MediaProvider for SpotifyClient<T, S> {
    async fn library_snapshot(
        &self,
        kind: LibraryKind,
        on_batch: &(dyn Fn(SyncBatch) + Send + Sync),
    ) -> Result<Snapshot, String> {
        spotify_library_snapshot(self, kind, None, on_batch).await
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
                    id: artist.id.clone(),
                    descriptor: artist_descriptor(&artist),
                    image_url: image_url(&artist.images),
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
                    year: album
                        .release_date
                        .as_deref()
                        .and_then(|date| date.get(..4))
                        .map(str::to_owned),
                    image_url: image_url(&album.images),
                    uri: album.uri,
                    name: album.name,
                })
                .collect(),
            tracks: results
                .tracks
                .items
                .into_iter()
                .map(|track| SearchTrack {
                    uri: track.uri,
                    name: track.name,
                    artist: track
                        .artists
                        .first()
                        .map(|artist| artist.name.clone())
                        .unwrap_or_default(),
                    alb: track
                        .album
                        .as_ref()
                        .map(|album| album.name.clone())
                        .unwrap_or_default(),
                    duration_secs: track.duration_ms.unwrap_or_default() / 1_000,
                    image_url: track
                        .album
                        .as_ref()
                        .and_then(|album| image_url(&album.images)),
                    album_uri: track.album.map(|album| album.uri),
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
                    year: album
                        .release_date
                        .as_deref()
                        .and_then(|date| date.get(..4))
                        .map(str::to_owned),
                    image_url: image_url(&album.images),
                    uri: album.uri,
                    name: album.name,
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
            for track in page.items {
                normalized.push(normalized_track(&track, None));
            }
            if page.next.is_none() || count == 0 {
                return Ok(enrich_music(&genres, vec![normalized])
                    .await?
                    .pop()
                    .unwrap_or_default());
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
    async fn library_snapshot(
        &self,
        kind: LibraryKind,
        on_batch: &(dyn Fn(SyncBatch) + Send + Sync),
    ) -> Result<Snapshot, String> {
        spotify_library_snapshot(self.client, kind, Some(&self.run), on_batch).await
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
    async fn library_snapshot(
        &self,
        kind: LibraryKind,
        on_batch: &(dyn Fn(SyncBatch) + Send + Sync),
    ) -> Result<Snapshot, String> {
        let batches = self.snapshots.get(&kind).cloned().unwrap_or_default();
        let total = batches.len() as u32;
        for (index, tracks) in batches.iter().enumerate() {
            on_batch(SyncBatch {
                tracks: tracks.clone(),
                done: index as u32 + 1,
                total: Some(total),
                section: kind.label(),
            });
        }
        Ok(Snapshot {
            batches,
            genres_degraded: self.genres_degraded,
            partial: self.partial,
            progress: None,
        })
    }

    async fn search(&self, _query: &str) -> Result<SearchResults, String> {
        Ok(SearchResults {
            artists: vec![],
            albums: vec![],
            tracks: vec![],
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
            Response::json(200, serde_json::json!({"items": [], "next": null})),
            Response::json(
                200,
                serde_json::json!({"id": "artist-1", "name": "Artist", "genres": ["rock"]}),
            ),
        ]);
        let tapped = Mutex::new(vec![]);
        let snapshot = MediaProvider::library_snapshot(&client, LibraryKind::Tracks, &|batch| {
            tapped.lock().unwrap().push(batch)
        })
        .await
        .unwrap();
        let batches = snapshot.batches;
        let tapped = tapped.into_inner().unwrap();

        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0][0].cat, "rock");
        assert_eq!(batches[0][0].alb, "Album");
        assert_eq!(tapped.len(), 2);
        assert_eq!(tapped[0].tracks[0].uri, "spotify:track:1");
        assert_eq!(tapped[0].done, 1);
        assert_eq!(tapped[0].section, "tracks");
        assert!(client.transport().requests()[1]
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
            .library_snapshot(LibraryKind::Albums, &|_| {})
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

        let snapshot = client
            .library_snapshot(LibraryKind::Albums, &|_| {})
            .await
            .unwrap();

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

        let snapshot = client
            .library_snapshot(LibraryKind::Albums, &|_| {})
            .await
            .unwrap();
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

        let tracks = client
            .library_snapshot(LibraryKind::Tracks, &|_| {})
            .await
            .unwrap();
        let albums = client
            .library_snapshot(LibraryKind::Albums, &|_| {})
            .await
            .unwrap();

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
    async fn album_progress_counts_only_fully_ingested_albums() {
        let client = client([
            Response::json(
                200,
                serde_json::json!({"items": [
                    {"album": {
                        "id": "album-1", "uri": "spotify:album:1", "name": "One",
                        "artists": [], "images": []
                    }},
                    {"album": {
                        "id": "album-2", "uri": "spotify:album:2", "name": "Two",
                        "artists": [], "images": []
                    }}
                ], "next": null, "total": 2}),
            ),
            Response::json(
                200,
                serde_json::json!({"items": [], "next": null, "total": 0}),
            ),
            rate_limited(),
        ]);

        let snapshot = client
            .library_snapshot(LibraryKind::Albums, &|_| {})
            .await
            .unwrap();

        assert_eq!(
            snapshot.progress,
            Some(SectionProgress {
                label: "albums",
                done: 1,
                total: Some(2),
            })
        );
    }

    #[tokio::test]
    async fn first_saved_tracks_rate_limit_returns_empty_partial_snapshot() {
        let client = client([rate_limited()]);

        let snapshot = client
            .library_snapshot(LibraryKind::Tracks, &|_| {})
            .await
            .unwrap();

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
            .library_snapshot(LibraryKind::Tracks, &|_| {})
            .await
            .unwrap();

        assert!(snapshot.partial);
        assert!(snapshot.batches.is_empty());
        assert_eq!(
            snapshot.progress,
            Some(SectionProgress {
                label: "tracks",
                done: 0,
                total: None,
            })
        );
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
            .library_snapshot(LibraryKind::Tracks, &|_| {})
            .await
            .unwrap();

        assert!(!snapshot.partial);
        assert!(snapshot.progress.is_none());
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
            .library_snapshot(LibraryKind::Tracks, &|_| {})
            .await
            .unwrap();
        let cooldowns = FsSyncStore::new(dir.path()).cooldowns(unix_now()).unwrap();

        assert!(snapshot.partial);
        assert!(cooldowns["/me/tracks"] > unix_now());
    }

    #[test]
    fn server_error_marks_snapshot_partial_without_a_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSyncStore::new(dir.path());
        let client = client([]);
        let provider = SpotifySyncProvider::new(&client, &store).unwrap();
        let health = SyncHealth::new(Some(&provider.run));

        let page = health
            .snapshot_page::<()>(Err(retune_spotify::Error::ServerError {
                endpoint: "/me/albums?offset=50&limit=50".into(),
                status: 502,
            }))
            .unwrap();

        assert!(page.is_none());
        assert!(health.partial.load(Ordering::Relaxed));
        assert_eq!(provider.earliest_cooldown(), None);
        assert!(store.cooldowns(unix_now()).unwrap().is_empty());
    }

    #[tokio::test]
    async fn artist_server_error_skips_only_that_artist() {
        let client = client([
            Response::json(500, serde_json::json!({})),
            Response::json(502, serde_json::json!({})),
            Response::json(504, serde_json::json!({})),
            Response::json(
                200,
                serde_json::json!({"id": "artist-2", "name": "Two", "genres": ["rock"]}),
            ),
        ]);
        let health = SyncHealth::new(None);
        let genres = GenreSource::new(&client, &health);

        assert!(genres.artist("artist-1").await.unwrap().is_none());
        assert_eq!(
            genres.artist("artist-2").await.unwrap().unwrap().id,
            "artist-2"
        );
        assert!(!health.genres_degraded.load(Ordering::Relaxed));
        assert_eq!(client.transport().requests().len(), 4);
    }

    #[tokio::test]
    async fn persistent_artist_cache_hit_skips_artist_request() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSyncStore::new(dir.path());
        store
            .save_artist_genres(&BTreeMap::from([("artist-1".into(), vec!["rock".into()])]))
            .unwrap();
        let client = client([
            Response::json(
                200,
                serde_json::json!({"items": [{"track": {
                "uri": "spotify:track:1", "name": "One", "duration_ms": 1000,
                "artists": [{"id": "artist-1", "name": "Artist"}], "album": null
            }}], "next": null}),
            ),
            Response::json(200, serde_json::json!({"items": [], "next": null})),
            Response::json(200, serde_json::json!({"items": [], "next": null})),
            Response::json(200, serde_json::json!({"items": [], "next": null})),
            Response::json(200, serde_json::json!({"items": [], "next": null})),
        ]);
        let provider = SpotifySyncProvider::new(&client, &store).unwrap();

        let mut snapshot = None;
        for kind in LibraryKind::ALL {
            snapshot = Some(provider.library_snapshot(kind, &|_| {}).await.unwrap());
        }
        let snapshot = snapshot.unwrap();

        assert_eq!(snapshot.batches[0][0].cat, "rock");
        assert_eq!(client.transport().requests().len(), 5);
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
            Response::json(200, serde_json::json!({"items": [], "next": null})),
            Response::json(200, serde_json::json!({"items": [], "next": null})),
            Response::json(200, serde_json::json!({"items": [], "next": null})),
            Response::json(200, serde_json::json!({"items": [], "next": null})),
            Response::json(
                200,
                serde_json::json!({"id": "artist-1", "name": "Artist", "genres": ["rock"]}),
            ),
        ]);
        let provider = SpotifySyncProvider::new(&client, &store).unwrap();

        let mut snapshot = None;
        for kind in LibraryKind::ALL {
            snapshot = Some(provider.library_snapshot(kind, &|_| {}).await.unwrap());
        }

        assert_eq!(snapshot.unwrap().batches[0][0].cat, "rock");
        assert_eq!(client.transport().requests().len(), 6);
        assert!(client.transport().requests()[4]
            .url
            .contains("/me/audiobooks"));
        assert!(client.transport().requests()[5]
            .url
            .contains("/artists/artist-1"));
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
            .library_snapshot(LibraryKind::Shows, &|_| {})
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
            .library_snapshot(LibraryKind::Episodes, &|_| {})
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
            .library_snapshot(LibraryKind::Audiobooks, &|_| {})
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
                "artists": {"items": [{"id": "artist-1", "name": "Artist", "genres": ["modern classical"],
                    "followers": {"total": 1234567},
                    "images": [{"url": "large", "width": 300}, {"url": "small", "width": 64}]}], "next": null},
                "albums": {"items": [{"id": "album-1", "uri": "spotify:album:1", "name": "Album",
                    "artists": [{"id": "artist-1", "name": "Artist"}], "release_date": "2024-03-01",
                    "images": [{"url": "album", "width": 64}]}], "next": null},
                "tracks": {"items": [{"uri": "spotify:track:1", "name": "Track", "duration_ms": 123456,
                    "artists": [{"id": "artist-1", "name": "Artist"}],
                    "album": {"id": "album-1", "uri": "spotify:album:1", "name": "Album",
                        "images": [{"url": "track", "width": 64}]}}], "next": null}
            }),
        )]);

        let results = MediaProvider::search(&client, "artist").await.unwrap();

        assert_eq!(
            results.artists[0],
            SearchArtist {
                id: "artist-1".into(),
                name: "Artist".into(),
                descriptor: "Modern Classical · 1.2M followers".into(),
                image_url: Some("small".into()),
            }
        );
        assert_eq!(results.albums[0].year.as_deref(), Some("2024"));
        assert_eq!(
            results.tracks[0],
            SearchTrack {
                uri: "spotify:track:1".into(),
                name: "Track".into(),
                artist: "Artist".into(),
                alb: "Album".into(),
                duration_secs: 123,
                image_url: Some("track".into()),
                album_uri: Some("spotify:album:1".into()),
            }
        );
        let url = url::Url::parse(&client.transport().requests()[0].url).unwrap();
        assert_eq!(
            url.query_pairs().find(|(key, _)| key == "type").unwrap().1,
            "artist,album,track"
        );
        assert_eq!(
            url.query_pairs().find(|(key, _)| key == "limit").unwrap().1,
            "10"
        );
    }

    #[test]
    fn formats_compact_counts() {
        assert_eq!(compact_count(1_234_567), "1.2M");
        assert_eq!(compact_count(903_400), "903K");
        assert_eq!(compact_count(999), "999");
    }

    #[test]
    fn picks_smallest_image_at_least_64_pixels_or_last() {
        let images = [
            Image {
                url: "large".into(),
                width: Some(300),
            },
            Image {
                url: "small".into(),
                width: Some(64),
            },
        ];
        assert_eq!(image_url(&images).as_deref(), Some("small"));
        assert_eq!(
            image_url(&[Image {
                url: "fallback".into(),
                width: Some(32),
            }])
            .as_deref(),
            Some("fallback")
        );
        assert_eq!(image_url(&[]), None);
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
            .library_snapshot(LibraryKind::Episodes, &|_| {})
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
            .library_snapshot(LibraryKind::Episodes, &|_| {})
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
            .library_snapshot(LibraryKind::Tracks, &|_| {})
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
    async fn sync_artist_budget_stops_at_cap_and_degrades_remaining_genres() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSyncStore::new(dir.path());
        let tracks = (0..ARTIST_LOOKUPS_PER_SYNC + 1)
            .map(|index| {
                serde_json::json!({"track": {
                    "uri": format!("spotify:track:{index}"), "name": format!("Track {index}"),
                    "duration_ms": 1000,
                    "artists": [{"id": format!("artist-{index:03}"), "name": "Artist"}],
                    "album": null
                }})
            })
            .collect::<Vec<_>>();
        let mut responses = vec![Response::json(
            200,
            serde_json::json!({"items": tracks, "next": null}),
        )];
        responses.extend(
            (0..4).map(|_| Response::json(200, serde_json::json!({"items": [], "next": null}))),
        );
        responses.extend((0..ARTIST_LOOKUPS_PER_SYNC).map(|index| {
            Response::json(
                200,
                serde_json::json!({
                    "id": format!("artist-{index:03}"), "name": "Artist", "genres": ["genre"]
                }),
            )
        }));
        let client = client(responses);
        let provider = SpotifySyncProvider::new(&client, &store).unwrap();

        let mut snapshot = None;
        for kind in LibraryKind::ALL {
            snapshot = Some(provider.library_snapshot(kind, &|_| {}).await.unwrap());
        }
        let snapshot = snapshot.unwrap();

        assert!(snapshot.genres_degraded);
        assert_eq!(
            client
                .transport()
                .requests()
                .iter()
                .filter(|request| request.url.contains("/artists/"))
                .count(),
            ARTIST_LOOKUPS_PER_SYNC
        );
        assert_eq!(
            snapshot.batches[0]
                .iter()
                .filter(|track| track.cat == normalize::UNCATEGORIZED)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn cached_artists_do_not_consume_sync_artist_budget() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSyncStore::new(dir.path());
        store
            .save_artist_genres(&BTreeMap::from([(
                "artist-000".into(),
                vec!["cached".into()],
            )]))
            .unwrap();
        let tracks = (0..=ARTIST_LOOKUPS_PER_SYNC)
            .map(|index| {
                serde_json::json!({"track": {
                    "uri": format!("spotify:track:{index}"), "name": format!("Track {index}"),
                    "duration_ms": 1000,
                    "artists": [{"id": format!("artist-{index:03}"), "name": "Artist"}],
                    "album": null
                }})
            })
            .collect::<Vec<_>>();
        let mut responses = vec![Response::json(
            200,
            serde_json::json!({"items": tracks, "next": null}),
        )];
        responses.extend(
            (0..4).map(|_| Response::json(200, serde_json::json!({"items": [], "next": null}))),
        );
        responses.extend((1..=ARTIST_LOOKUPS_PER_SYNC).map(|index| {
            Response::json(
                200,
                serde_json::json!({
                    "id": format!("artist-{index:03}"), "name": "Artist", "genres": ["genre"]
                }),
            )
        }));
        let client = client(responses);
        let provider = SpotifySyncProvider::new(&client, &store).unwrap();

        let mut snapshot = None;
        for kind in LibraryKind::ALL {
            snapshot = Some(provider.library_snapshot(kind, &|_| {}).await.unwrap());
        }
        let snapshot = snapshot.unwrap();

        assert!(!snapshot.genres_degraded);
        assert_eq!(snapshot.batches[0][0].cat, "cached");
        assert!(snapshot.batches[0]
            .iter()
            .all(|track| track.cat != normalize::UNCATEGORIZED));
        assert_eq!(
            client
                .transport()
                .requests()
                .iter()
                .filter(|request| request.url.contains("/artists/"))
                .count(),
            ARTIST_LOOKUPS_PER_SYNC
        );
    }

    #[test]
    fn resume_time_is_day_aware() {
        use chrono::{Days, TimeZone};

        let now = chrono::Local
            .with_ymd_and_hms(2026, 1, 15, 12, 0, 0)
            .single()
            .unwrap();
        let tomorrow = now.checked_add_days(Days::new(1)).unwrap();
        let later = now.checked_add_days(Days::new(3)).unwrap();

        assert_eq!(format_resume_time(now.timestamp() as u64, now), "12:00");
        assert_eq!(
            format_resume_time(tomorrow.timestamp() as u64, now),
            "tomorrow at 12:00"
        );
        assert_eq!(
            format_resume_time(later.timestamp() as u64, now),
            "Jan 18 at 12:00"
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

        let snapshot = client
            .library_snapshot(LibraryKind::Tracks, &|_| {})
            .await
            .unwrap();

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

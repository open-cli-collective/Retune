use retune_core::model::NewTrack;
use retune_spotify::{
    client::{endpoint_family, Album, Artist, Image, Page, SpotifyClient, Transport},
    normalize,
    tokens::TokenStore,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Mutex,
    },
};

use crate::store::{Cooldown, CooldownKind, FsSyncStore, SavedAlbumRecord};
use crate::unix_now;

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
    /// Ordering invariant: Audiobooks must remain the final section.
    /// SpotifySyncProvider parks music batches in `SyncRun::pending_music`
    /// and flushes + genre-enriches them only when `kind == Audiobooks`
    /// (see the flush in `spotify_library_snapshot`), so `sync::snapshot`
    /// must iterate ALL to completion.
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
    pub album_type: Option<String>,
    pub track_count: u32,
    pub in_library: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtistAlbumsPage {
    pub albums: Vec<SearchAlbum>,
    pub next_offset: Option<u32>,
    pub total: u32,
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
    pub in_library: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchGroup<T> {
    pub items: Vec<T>,
    pub total: u32,
    pub next_offset: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResults {
    pub artists: SearchGroup<SearchArtist>,
    pub albums: SearchGroup<SearchAlbum>,
    pub tracks: SearchGroup<SearchTrack>,
}

pub struct Snapshot {
    pub batches: Vec<Vec<NewTrack>>,
    pub genres_degraded: bool,
    pub partial: bool,
    pub quota_exhausted: bool,
    pub progress: Option<SectionProgress>,
    pub account_id: Option<String>,
    pub saved_tracks: Option<BTreeMap<String, Option<u64>>>,
    pub saved_albums: Option<BTreeMap<String, SavedAlbumRecord>>,
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
}

pub struct SpotifySyncProvider<'a, T, S> {
    client: &'a SpotifyClient<T, S>,
    run: SyncRun<'a>,
    account_id: Option<String>,
}

impl<'a, T: Transport, S: TokenStore> SpotifySyncProvider<'a, T, S> {
    #[cfg(test)]
    pub fn new(client: &'a SpotifyClient<T, S>, store: &'a FsSyncStore) -> Result<Self, String> {
        Self::new_with_account(client, store, None)
    }

    pub fn for_account(
        client: &'a SpotifyClient<T, S>,
        store: &'a FsSyncStore,
        account_id: impl Into<String>,
    ) -> Result<Self, String> {
        Self::new_with_account(client, store, Some(account_id.into()))
    }

    fn new_with_account(
        client: &'a SpotifyClient<T, S>,
        store: &'a FsSyncStore,
        account_id: Option<String>,
    ) -> Result<Self, String> {
        client.reset_request_counts();
        Ok(Self {
            client,
            account_id,
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
    cooldowns: Mutex<BTreeMap<String, Cooldown>>,
    artist_genres: Mutex<BTreeMap<String, Vec<String>>>,
    pending_music: Mutex<Vec<Vec<PendingTrack>>>,
    artist_lookups: AtomicUsize,
    earliest_cooldown: AtomicU64,
}

impl SyncRun<'_> {
    fn cooldown(&self, family: &str) -> Option<Cooldown> {
        let cooldown = self
            .cooldowns
            .lock()
            .expect("cooldown mutex poisoned")
            .get(family)
            .copied()
            .filter(|cooldown| cooldown.deadline > unix_now());
        if let Some(cooldown) = cooldown {
            self.note_deadline(cooldown.deadline);
            log::warn!(
                "skipping {family} until {}",
                format_resume_time(cooldown.deadline, chrono::Local::now())
            );
        }
        cooldown
    }

    fn record_cooldown(&self, endpoint: &str, kind: CooldownKind, retry_after_secs: u64) {
        let family = endpoint_family(endpoint);
        let deadline = unix_now().saturating_add(retry_after_secs);
        let mut cooldowns = self.cooldowns.lock().expect("cooldown mutex poisoned");
        cooldowns.insert(family, Cooldown { kind, deadline });
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

pub(crate) fn title_case(value: &str) -> String {
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

pub(crate) fn artist_descriptor(artist: &Artist) -> String {
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

pub(crate) fn image_url(images: &[Image]) -> Option<String> {
    image_url_at_least(images, 64)
}

pub(crate) fn image_url_at_least(images: &[Image], min_width: u32) -> Option<String> {
    images
        .iter()
        .filter(|image| image.width.is_some_and(|width| width >= min_width))
        .min_by_key(|image| image.width)
        .or_else(|| images.iter().max_by_key(|image| image.width))
        .map(|image| image.url.clone())
}

fn album_track_count(album: &Album) -> u32 {
    album.total_tracks.max(
        album
            .tracks
            .as_ref()
            .map(|tracks| tracks.total)
            .unwrap_or_default(),
    )
}

fn search_album(album: Album) -> SearchAlbum {
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
        album_type: album.album_type.as_deref().map(title_case),
        track_count: album_track_count(&album),
        in_library: false,
        uri: album.uri,
        name: album.name,
    }
}

pub(crate) fn spotify_id(value: &str) -> &str {
    value.rsplit(':').next().unwrap_or(value)
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
    quota_exhausted: AtomicBool,
    run: Option<&'a SyncRun<'a>>,
}

impl<'a> SyncHealth<'a> {
    fn new(run: Option<&'a SyncRun<'a>>) -> Self {
        Self {
            genres_degraded: AtomicBool::new(false),
            partial: AtomicBool::new(false),
            quota_exhausted: AtomicBool::new(false),
            run,
        }
    }

    fn skip_content_family(&self, family: &str) -> bool {
        if let Some(cooldown) = self.run.and_then(|run| run.cooldown(family)) {
            if cooldown.kind == CooldownKind::Quota {
                self.quota_exhausted.store(true, Ordering::Relaxed);
            }
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
                    run.record_cooldown(&endpoint, CooldownKind::Transient, retry_after_secs);
                }
                if !self.partial.swap(true, Ordering::Relaxed) {
                    let family = endpoint_family(&endpoint);
                    log::warn!(
                        "Spotify library snapshot is partial: family={family} kind=transient retry_delay={retry_after_secs}"
                    );
                }
                Ok(None)
            }
            Err(retune_spotify::Error::QuotaExceeded {
                endpoint,
                retry_after_secs,
            }) => {
                self.quota_exhausted.store(true, Ordering::Relaxed);
                if let Some(run) = self.run {
                    if let Some(retry_after_secs) = retry_after_secs {
                        run.record_cooldown(&endpoint, CooldownKind::Quota, retry_after_secs);
                    }
                }
                if !self.partial.swap(true, Ordering::Relaxed) {
                    log::warn!(
                        "Spotify library snapshot is partial: Development Mode quota exhausted for {}",
                        endpoint_family(&endpoint)
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

    fn mark_skipped(&self, skipped: usize, section: &str) {
        if skipped > 0 {
            self.partial.store(true, Ordering::Relaxed);
            log::warn!(
                "Spotify library snapshot is partial: skipped {skipped} undecodable {section}"
            );
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
            if let Some(cooldown) = run.cooldown("/artists") {
                if cooldown.kind == CooldownKind::Quota {
                    self.health.quota_exhausted.store(true, Ordering::Relaxed);
                    self.health.partial.store(true, Ordering::Relaxed);
                }
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
                    run.record_cooldown(&endpoint, CooldownKind::Transient, retry_after_secs);
                }
                if !self.health.genres_degraded.swap(true, Ordering::Relaxed) {
                    let family = endpoint_family(&endpoint);
                    log::warn!(
                        "Spotify artist genres unavailable for this sync: family={family} kind=transient retry_delay={retry_after_secs}"
                    );
                }
                Ok(None)
            }
            Err(retune_spotify::Error::QuotaExceeded {
                endpoint,
                retry_after_secs,
            }) => {
                self.health.quota_exhausted.store(true, Ordering::Relaxed);
                self.health.partial.store(true, Ordering::Relaxed);
                if let Some(run) = self.health.run {
                    if let Some(retry_after_secs) = retry_after_secs {
                        run.record_cooldown(&endpoint, CooldownKind::Quota, retry_after_secs);
                    }
                }
                if !self.health.genres_degraded.swap(true, Ordering::Relaxed) {
                    log::warn!(
                        "Spotify artist genres unavailable: Development Mode quota exhausted for {}",
                        endpoint_family(&endpoint)
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

fn normalized_track(
    track: &retune_spotify::client::Track,
    album: Option<&Album>,
    added_at: Option<u64>,
) -> PendingTrack {
    let artist_id = track
        .artists
        .first()
        .or_else(|| album.and_then(|album| album.artists.first()))
        .map(|artist| artist.id.clone());
    PendingTrack {
        track: NewTrack {
            added_at,
            ..normalize::track(track, None, album)
        },
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
    added_at: Option<u64>,
) -> Result<(Vec<PendingTrack>, bool), String> {
    let mut offset = 0;
    let mut tracks = vec![];
    let mut expected_total = None;
    let mut incomplete = false;
    if let Some(page) = album.tracks.clone() {
        incomplete |= page.skipped > 0;
        health.mark_skipped(page.skipped, "album tracks");
        let count = (page.items.len() + page.skipped) as u32;
        expected_total = Some(page.total);
        for track in page.items {
            tracks.push(normalized_track(&track, Some(album), added_at));
        }
        offset = count;
        if page.total <= offset {
            return Ok((tracks, incomplete));
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
        incomplete |= page.skipped > 0;
        health.mark_skipped(page.skipped, "album tracks");
        let count = (page.items.len() + page.skipped) as u32;
        for track in page.items {
            tracks.push(normalized_track(&track, Some(album), added_at));
        }
        offset += count;
        if count == 0
            || expected_total.is_some_and(|total| offset >= total)
            || (expected_total.is_none() && page.next.is_none())
        {
            return Ok((tracks, incomplete));
        }
    }
}

pub(crate) fn saved_album_record(
    album: &Album,
    track_uris: Vec<String>,
    added_at: Option<u64>,
) -> SavedAlbumRecord {
    SavedAlbumRecord {
        uri: album.uri.clone(),
        name: album.name.clone(),
        artists: album
            .artists
            .iter()
            .map(|artist| artist.name.clone())
            .collect(),
        release_date: album.release_date.clone(),
        album_type: album.album_type.clone(),
        added_at,
        track_uris,
    }
}

async fn spotify_library_snapshot<'a, T: Transport, S: TokenStore>(
    client: &'a SpotifyClient<T, S>,
    kind: LibraryKind,
    run: Option<&'a SyncRun<'a>>,
    account_id: Option<&str>,
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
            quota_exhausted: health.quota_exhausted.load(Ordering::Relaxed),
            progress: Some(SectionProgress {
                label,
                done: 0,
                total: None,
            }),
            account_id: account_id.map(str::to_owned),
            saved_tracks: None,
            saved_albums: None,
        });
    }
    let discovery_at = unix_now();
    let mut offset = 0;
    let mut done = 0;
    let mut total = None;
    let mut batches = vec![];
    let mut music_batches = vec![];
    let mut saved_tracks = BTreeMap::new();
    let mut saved_albums = BTreeMap::new();
    'pages: loop {
        let (batch, count, has_next) = match kind {
            LibraryKind::Tracks => {
                let Some(page) =
                    health.snapshot_page(client.saved_tracks(offset, PAGE_SIZE).await)?
                else {
                    break 'pages;
                };
                health.mark_skipped(page.skipped, "saved tracks");
                total.get_or_insert(page.total);
                let count = (page.items.len() + page.skipped) as u32;
                let batch = page
                    .items
                    .into_iter()
                    .map(|saved| {
                        let added_at = saved.added_at.or(Some(discovery_at));
                        saved_tracks.insert(saved.track.uri.clone(), added_at);
                        normalized_track(&saved.track, None, added_at)
                    })
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
                health.mark_skipped(page.skipped, "saved albums");
                total.get_or_insert(page.total);
                let count = (page.items.len() + page.skipped) as u32;
                let mut batch = vec![];
                let mut interrupted = false;
                for saved in page.items {
                    let membership_at = saved.added_at;
                    let (tracks, partial) = normalized_album_tracks(
                        client,
                        &health,
                        &saved.album,
                        membership_at.or(Some(discovery_at)),
                    )
                    .await?;
                    if partial {
                        interrupted = true;
                        break;
                    }
                    let track_uris = tracks.iter().map(|track| track.track.uri.clone()).collect();
                    batch.extend(tracks);
                    saved_albums.insert(
                        saved.album.uri.clone(),
                        saved_album_record(&saved.album, track_uris, membership_at),
                    );
                    done += 1;
                }
                on_batch(SyncBatch {
                    tracks: batch.iter().map(|pending| pending.track.clone()).collect(),
                    done,
                    total,
                    section: label,
                });
                music_batches.push(batch);
                (vec![], count, page.next.is_some() && !interrupted)
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
                let mut interrupted = false;
                for saved in page.items {
                    if health.skip_content_family("/shows") {
                        interrupted = true;
                        break;
                    }
                    let Some(episodes) = health
                        .snapshot_page(client.show_episodes(&saved.show.id, 0, PAGE_SIZE).await)?
                    else {
                        interrupted = true;
                        break;
                    };
                    done += 1;
                    batch.extend(
                        episodes
                            .items
                            .iter()
                            .map(|episode| normalize::episode(episode, Some(&saved.show))),
                    );
                }
                (batch, count, page.next.is_some() && !interrupted)
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
                let mut interrupted = false;
                for book in page.items {
                    if health.skip_content_family("/audiobooks") {
                        interrupted = true;
                        break;
                    }
                    let Some(chapters) = health
                        .snapshot_page(client.audiobook_chapters(&book.id, 0, PAGE_SIZE).await)?
                    else {
                        interrupted = true;
                        break;
                    };
                    done += 1;
                    batch.extend(
                        chapters
                            .items
                            .iter()
                            .map(|chapter| normalize::chapter(chapter, &book)),
                    );
                }
                (batch, count, page.next.is_some() && !interrupted)
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
        // Audiobooks is the final section (see LibraryKind::ALL) — flush here.
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
    let saved_tracks = (kind == LibraryKind::Tracks && !partial).then_some(saved_tracks);
    let saved_albums = (kind == LibraryKind::Albums && !partial).then_some(saved_albums);
    Ok(Snapshot {
        batches,
        genres_degraded: health.genres_degraded.load(Ordering::Relaxed),
        partial,
        quota_exhausted: health.quota_exhausted.load(Ordering::Relaxed),
        progress: partial.then_some(SectionProgress { label, done, total }),
        account_id: account_id.map(str::to_owned),
        saved_tracks,
        saved_albums,
    })
}

impl<T: Transport, S: TokenStore> MediaProvider for SpotifyClient<T, S> {
    async fn library_snapshot(
        &self,
        kind: LibraryKind,
        on_batch: &(dyn Fn(SyncBatch) + Send + Sync),
    ) -> Result<Snapshot, String> {
        spotify_library_snapshot(self, kind, None, None, on_batch).await
    }
}

fn search_group<T, U>(page: Page<T>, offset: u32, map: impl FnMut(T) -> U) -> SearchGroup<U> {
    let count = (page.items.len() + page.skipped) as u32;
    SearchGroup {
        items: page.items.into_iter().map(map).collect(),
        total: page.total,
        next_offset: (page.next.is_some() && count > 0).then_some(offset + count),
    }
}

pub async fn search<T: Transport, S: TokenStore>(
    client: &SpotifyClient<T, S>,
    query: &str,
    offset: u32,
) -> Result<SearchResults, String> {
    let results = SpotifyClient::search(client, query, offset, SEARCH_PAGE_SIZE)
        .await
        .map_err(|error| error.to_string())?;
    Ok(SearchResults {
        artists: search_group(results.artists, offset, |artist| SearchArtist {
            id: artist.id.clone(),
            descriptor: artist_descriptor(&artist),
            image_url: image_url(&artist.images),
            name: artist.name,
        }),
        albums: search_group(results.albums, offset, search_album),
        tracks: search_group(results.tracks, offset, |track| SearchTrack {
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
            in_library: false,
        }),
    })
}

pub async fn search_albums<T: Transport, S: TokenStore>(
    client: &SpotifyClient<T, S>,
    query: &str,
) -> Result<SearchGroup<SearchAlbum>, String> {
    let results = SpotifyClient::search_with_types(client, query, "album", 0, SEARCH_PAGE_SIZE)
        .await
        .map_err(|error| error.to_string())?;
    Ok(search_group(results.albums, 0, search_album))
}

pub async fn search_tracks<T: Transport, S: TokenStore>(
    client: &SpotifyClient<T, S>,
    query: &str,
) -> Result<SearchGroup<SearchTrack>, String> {
    let results = SpotifyClient::search_with_types(client, query, "track", 0, SEARCH_PAGE_SIZE)
        .await
        .map_err(|error| error.to_string())?;
    Ok(search_group(results.tracks, 0, |track| SearchTrack {
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
        in_library: false,
    }))
}

pub async fn album_tracks<T: Transport, S: TokenStore>(
    client: &SpotifyClient<T, S>,
    album: &str,
) -> Result<Vec<NewTrack>, String> {
    album_content(client, album, None)
        .await
        .map(|(_, tracks)| tracks)
}

pub async fn album_content<T: Transport, S: TokenStore>(
    client: &SpotifyClient<T, S>,
    album: &str,
    added_at: Option<u64>,
) -> Result<(Album, Vec<NewTrack>), String> {
    let health = SyncHealth::new(None);
    let genres = GenreSource::new(client, &health);
    let album = SpotifyClient::album(client, spotify_id(album))
        .await
        .map_err(|error| error.to_string())?;
    if album.tracks.as_ref().is_some_and(|page| page.skipped > 0) {
        return Err("Spotify returned incomplete album tracks; try again later.".into());
    }
    let normalized = album
        .tracks
        .as_ref()
        .map(|page| {
            page.items
                .iter()
                .map(|track| normalized_track(track, Some(&album), added_at))
                .collect()
        })
        .unwrap_or_default();
    Ok((
        album,
        enrich_music(&genres, vec![normalized])
            .await?
            .pop()
            .unwrap_or_default(),
    ))
}

pub async fn artist_albums_page<T: Transport, S: TokenStore>(
    client: &SpotifyClient<T, S>,
    artist: &str,
    offset: u32,
) -> retune_spotify::Result<ArtistAlbumsPage> {
    let page = client
        .artist_albums(spotify_id(artist), offset, SEARCH_PAGE_SIZE)
        .await?;
    let count = (page.items.len() + page.skipped) as u32;
    Ok(ArtistAlbumsPage {
        albums: page.items.into_iter().map(search_album).collect(),
        next_offset: (page.next.is_some() && count > 0).then_some(offset + count),
        total: page.total,
    })
}

impl<T: Transport, S: TokenStore> MediaProvider for SpotifySyncProvider<'_, T, S> {
    async fn library_snapshot(
        &self,
        kind: LibraryKind,
        on_batch: &(dyn Fn(SyncBatch) + Send + Sync),
    ) -> Result<Snapshot, String> {
        spotify_library_snapshot(
            self.client,
            kind,
            Some(&self.run),
            self.account_id.as_deref(),
            on_batch,
        )
        .await
    }

    fn earliest_cooldown(&self) -> Option<u64> {
        self.run.earliest_cooldown()
    }

    fn request_counts(&self) -> BTreeMap<String, u64> {
        self.client.request_counts()
    }
}

#[cfg(test)]
#[derive(Default)]
pub struct FakeProvider {
    pub snapshots: std::collections::HashMap<LibraryKind, Vec<Vec<NewTrack>>>,
    pub genres_degraded: bool,
    pub partial: bool,
    pub quota_exhausted: bool,
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
            quota_exhausted: self.quota_exhausted,
            progress: None,
            account_id: None,
            saved_tracks: None,
            saved_albums: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use retune_core::model::SourceId;
    use retune_spotify::{
        catalog::SpotifyCatalog,
        client::{fake_client, Album, FakeTransport, Page, Response, Track},
        tokens::InMemoryTokenStore,
    };

    use super::*;

    fn client(
        responses: impl IntoIterator<Item = Response>,
    ) -> SpotifyClient<FakeTransport, InMemoryTokenStore> {
        fake_client(responses, "")
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
                serde_json::json!({"items": [{"added_at": "2024-01-02T03:04:05Z", "track": {
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
        assert_eq!(batches[0][0].added_at, Some(1_704_164_645));
        assert_eq!(tapped.len(), 2);
        assert_eq!(tapped[0].tracks[0].uri, "spotify:track:1");
        assert_eq!(tapped[0].done, 1);
        assert_eq!(tapped[0].section, "tracks");
        assert!(client.transport().requests()[1]
            .url
            .ends_with("offset=1&limit=50"));
    }

    #[tokio::test]
    async fn skipped_saved_track_withholds_exact_membership() {
        let client = client([Response::json(
            200,
            serde_json::json!({"items": [{"track": {
                "uri": "spotify:track:1", "name": "One", "duration_ms": 1000,
                "artists": [], "album": null
            }}, null], "next": null, "total": 2}),
        )]);

        let snapshot = client
            .library_snapshot(LibraryKind::Tracks, &|_| {})
            .await
            .unwrap();

        assert!(snapshot.partial);
        assert!(snapshot.saved_tracks.is_none());
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

        let snapshot = client
            .library_snapshot(LibraryKind::Albums, &|_| {})
            .await
            .unwrap();
        let batches = snapshot.batches;

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
        assert!(batches[0][0].added_at.is_some());
        assert_eq!(
            snapshot.saved_albums.unwrap()["spotify:album:1"].added_at,
            None
        );
    }

    #[tokio::test]
    async fn saved_album_state_keeps_membership_metadata_and_track_uris() {
        let dir = tempfile::tempdir().unwrap();
        let client = client([Response::json(
            200,
            serde_json::json!({"items": [{"added_at": "2024-01-02T03:04:05Z", "album": {
                "id": "album-1", "uri": "spotify:album:1", "name": "Record",
                "album_type": "album", "release_date": "2024-01-02",
                "artists": [{"id": "artist-1", "name": "Artist"}], "images": [],
                "tracks": {"items": [{
                    "uri": "spotify:track:1", "name": "Song", "duration_ms": 1234,
                    "artists": [], "album": null
                }], "next": null, "total": 1}
            }}], "next": null, "total": 1}),
        )]);
        let store = FsSyncStore::new(dir.path());
        let provider = SpotifySyncProvider::for_account(&client, &store, "account").unwrap();

        let snapshot = provider
            .library_snapshot(LibraryKind::Albums, &|_| {})
            .await
            .unwrap();
        let mut albums = snapshot.saved_albums.unwrap();
        let record = albums.remove("spotify:album:1").unwrap();

        assert_eq!(record.name, "Record");
        assert_eq!(record.artists, ["Artist"]);
        assert_eq!(record.release_date.as_deref(), Some("2024-01-02"));
        assert_eq!(record.album_type.as_deref(), Some("album"));
        assert_eq!(record.added_at, Some(1_704_164_645));
        assert_eq!(record.track_uris, ["spotify:track:1"]);
    }

    #[tokio::test]
    async fn skipped_album_track_withholds_exact_membership() {
        let client = client([Response::json(
            200,
            serde_json::json!({"items": [{"album": {
                "id": "album-1", "uri": "spotify:album:1", "name": "Record",
                "artists": [], "images": [],
                "tracks": {"items": [{
                    "uri": "spotify:track:1", "name": "Song", "duration_ms": 1234,
                    "artists": [], "album": null
                }, null], "next": null, "total": 2}
            }}], "next": null, "total": 1}),
        )]);

        let snapshot = client
            .library_snapshot(LibraryKind::Albums, &|_| {})
            .await
            .unwrap();

        assert!(snapshot.partial);
        assert!(snapshot.saved_albums.is_none());
    }

    #[tokio::test]
    async fn skipped_saved_album_withholds_exact_membership() {
        let client = client([Response::json(
            200,
            serde_json::json!({"items": [{"album": {
                "id": "album-1", "uri": "spotify:album:1", "name": "Record",
                "artists": [], "images": [],
                "tracks": {"items": [], "next": null, "total": 0}
            }}, null], "next": null, "total": 2}),
        )]);

        let snapshot = client
            .library_snapshot(LibraryKind::Albums, &|_| {})
            .await
            .unwrap();

        assert!(snapshot.partial);
        assert!(snapshot.saved_albums.is_none());
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
    async fn explicit_album_content_uses_all_client_pages() {
        let client = client([
            Response::json(
                200,
                serde_json::json!({
                    "id": "album-1", "uri": "spotify:album:1", "name": "Record",
                    "artists": [], "images": [],
                    "tracks": {"items": [{
                        "uri": "spotify:track:1", "name": "One", "duration_ms": 1000,
                        "artists": [], "album": null
                    }], "next": "next", "total": 2}
                }),
            ),
            Response::json(
                200,
                serde_json::json!({"items": [{
                    "uri": "spotify:track:2", "name": "Two", "duration_ms": 1000,
                    "artists": [], "album": null
                }], "next": null, "total": 2}),
            ),
        ]);

        let (_, tracks) = album_content(&client, "spotify:album:1", Some(10))
            .await
            .unwrap();

        assert_eq!(
            tracks
                .iter()
                .map(|track| track.uri.as_str())
                .collect::<Vec<_>>(),
            ["spotify:track:1", "spotify:track:2"]
        );
        assert!(client.transport().requests()[1]
            .url
            .contains("/albums/1/tracks?offset=1&limit=50"));
    }

    #[tokio::test]
    async fn complete_cached_album_content_needs_no_spotify_request() {
        let album = Album {
            id: "album-1".into(),
            uri: "spotify:album:1".into(),
            name: "Record".into(),
            artists: vec![],
            images: vec![],
            release_date: None,
            album_type: Some("album".into()),
            total_tracks: 1,
            tracks: Some(Page {
                items: vec![Track {
                    uri: "spotify:track:1".into(),
                    name: "One".into(),
                    duration_ms: Some(1_000),
                    track_number: Some(1),
                    disc_number: Some(1),
                    artists: vec![],
                    album: None,
                }],
                next: None,
                skipped: 0,
                total: 1,
            }),
        };
        let mut catalog = SpotifyCatalog::default();
        catalog.observe_album(&album, true);
        let client = SpotifyClient::new_with_catalog(
            "client",
            FakeTransport::new([]),
            InMemoryTokenStore::new(None),
            Arc::new(Mutex::new(catalog)),
        );

        let (cached_album, tracks) = album_content(&client, "spotify:album:1", None)
            .await
            .unwrap();

        assert_eq!(cached_album.uri, album.uri);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].uri, "spotify:track:1");
        assert!(client.transport().requests().is_empty());
    }

    #[tokio::test]
    async fn explicit_album_content_rejects_skipped_tracks() {
        let client = client([Response::json(
            200,
            serde_json::json!({
                "id": "album-1", "uri": "spotify:album:1", "name": "Record",
                "artists": [], "images": [],
                "tracks": {"items": [{
                    "uri": "spotify:track:1", "name": "One", "duration_ms": 1000,
                    "artists": [], "album": null
                }, null], "next": null, "total": 2}
            }),
        )]);

        assert!(album_content(&client, "spotify:album:1", None)
            .await
            .unwrap_err()
            .contains("incomplete album tracks"));
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
            Response::rate_limited("3600"),
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
            Response::rate_limited("3600"),
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
        let client = client([Response::rate_limited("3600")]);

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
            .save_cooldowns(&BTreeMap::from([(
                "/me/tracks".into(),
                Cooldown {
                    kind: CooldownKind::Transient,
                    deadline,
                },
            )]))
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
                Cooldown {
                    kind: CooldownKind::Transient,
                    deadline: unix_now().saturating_sub(1),
                },
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
        let client = client([Response::rate_limited("3600")]);
        let provider = SpotifySyncProvider::new(&client, &store).unwrap();

        let snapshot = provider
            .library_snapshot(LibraryKind::Tracks, &|_| {})
            .await
            .unwrap();
        let cooldowns = FsSyncStore::new(dir.path()).cooldowns(unix_now()).unwrap();

        assert!(snapshot.partial);
        assert!(cooldowns["/me/tracks"].deadline > unix_now());
        assert_eq!(cooldowns["/me/tracks"].kind, CooldownKind::Transient);
    }

    #[tokio::test]
    async fn quota_without_retry_after_is_partial_without_auto_resume() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSyncStore::new(dir.path());
        let client = client([Response::quota_exceeded(None)]);
        let provider = SpotifySyncProvider::new(&client, &store).unwrap();

        let snapshot = provider
            .library_snapshot(LibraryKind::Tracks, &|_| {})
            .await
            .unwrap();

        assert!(snapshot.partial);
        assert!(snapshot.quota_exhausted);
        assert_eq!(provider.earliest_cooldown(), None);
        assert!(store.cooldowns(unix_now()).unwrap().is_empty());
    }

    #[test]
    fn quota_without_deadline_preserves_known_transient_resume() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSyncStore::new(dir.path());
        let client = client([]);
        let provider = SpotifySyncProvider::new(&client, &store).unwrap();
        let health = SyncHealth::new(Some(&provider.run));

        health
            .snapshot_page::<()>(Err(retune_spotify::Error::RateLimited {
                endpoint: "/me/tracks".into(),
                retry_after_secs: 3_600,
            }))
            .unwrap();
        let transient_deadline = provider.earliest_cooldown().unwrap();
        health
            .snapshot_page::<()>(Err(retune_spotify::Error::QuotaExceeded {
                endpoint: "/me/albums".into(),
                retry_after_secs: None,
            }))
            .unwrap();

        assert_eq!(provider.earliest_cooldown(), Some(transient_deadline));
        assert_eq!(store.cooldowns(unix_now()).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn artist_quota_marks_the_music_snapshot_partial() {
        let client = client([
            Response::json(
                200,
                serde_json::json!({"items": [{"track": {
                    "uri": "spotify:track:1", "name": "Song", "duration_ms": 1,
                    "artists": [{"id": "artist-1", "name": "Artist"}], "album": null
                }}], "next": null}),
            ),
            Response::quota_exceeded(None),
        ]);

        let snapshot = client
            .library_snapshot(LibraryKind::Tracks, &|_| {})
            .await
            .unwrap();

        assert!(snapshot.partial);
        assert!(snapshot.quota_exhausted);
        assert!(snapshot.genres_degraded);
    }

    #[tokio::test]
    async fn quota_with_retry_after_persists_typed_auto_resume() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSyncStore::new(dir.path());
        let client = client([Response::quota_exceeded(Some(3_600))]);
        let provider = SpotifySyncProvider::new(&client, &store).unwrap();

        let snapshot = provider
            .library_snapshot(LibraryKind::Tracks, &|_| {})
            .await
            .unwrap();
        let cooldown = store.cooldowns(unix_now()).unwrap()["/me/tracks"];

        assert!(snapshot.partial);
        assert!(snapshot.quota_exhausted);
        assert_eq!(cooldown.kind, CooldownKind::Quota);
        assert_eq!(provider.earliest_cooldown(), Some(cooldown.deadline));
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
    async fn show_episode_rate_limit_keeps_previously_collected_episodes() {
        let client = client([
            Response::json(
                200,
                serde_json::json!({"items": [
                    {"show": {"id": "show-1", "uri": "spotify:show:1", "name": "Show One",
                        "publisher": "Publisher", "category": null, "images": []}},
                    {"show": {"id": "show-2", "uri": "spotify:show:2", "name": "Show Two",
                        "publisher": "Publisher", "category": null, "images": []}}
                ], "next": null}),
            ),
            Response::json(
                200,
                serde_json::json!({"items": [{
                    "uri": "spotify:episode:1", "name": "Episode", "duration_ms": 2000,
                    "show": null
                }], "next": null}),
            ),
            Response::rate_limited("3600"),
        ]);

        let snapshot = client
            .library_snapshot(LibraryKind::Shows, &|_| {})
            .await
            .unwrap();

        assert!(snapshot.partial);
        let tracks = snapshot.batches.concat();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].uri, "spotify:episode:1");
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

        let results = search(&client, "artist", 0).await.unwrap();

        assert_eq!(
            results.artists.items[0],
            SearchArtist {
                id: "artist-1".into(),
                name: "Artist".into(),
                descriptor: "Modern Classical · 1.2M followers".into(),
                image_url: Some("small".into()),
            }
        );
        assert_eq!(results.albums.items[0].year.as_deref(), Some("2024"));
        assert_eq!(
            results.tracks.items[0],
            SearchTrack {
                uri: "spotify:track:1".into(),
                name: "Track".into(),
                artist: "Artist".into(),
                alb: "Album".into(),
                duration_secs: 123,
                image_url: Some("track".into()),
                album_uri: Some("spotify:album:1".into()),
                in_library: false,
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

    #[tokio::test]
    async fn search_pages_expose_totals_and_next_offsets_for_empty_final_pages() {
        let first_page = || {
            Response::json(
                200,
                serde_json::json!({
                    "artists": {"items": (0..10).map(|index| serde_json::json!({"id": format!("artist-{index}"), "name": "Artist", "images": []})).collect::<Vec<_>>(), "next": "next", "total": 10},
                    "albums": {"items": [], "next": "next", "total": 10},
                    "tracks": {"items": [], "next": "next", "total": 10}
                }),
            )
        };
        let client = client([
            first_page(),
            Response::json(
                200,
                serde_json::json!({
                    "artists": {"items": [], "next": null, "total": 10},
                    "albums": {"items": [], "next": null, "total": 10},
                    "tracks": {"items": [], "next": null, "total": 10}
                }),
            ),
        ]);

        let first = search(&client, "artist", 0).await.unwrap();
        let final_page = search(&client, "artist", 10).await.unwrap();

        assert_eq!(first.artists.items.len(), 10);
        assert_eq!(first.artists.total, 10);
        assert_eq!(first.artists.next_offset, Some(10));
        assert!(final_page.artists.items.is_empty());
        assert_eq!(final_page.artists.next_offset, None);
        let requests = client.transport().requests();
        assert_eq!(requests.len(), 2);
        for (request, offset) in requests.iter().zip([0, 10]) {
            let url = url::Url::parse(&request.url).unwrap();
            assert_eq!(url.path(), "/v1/search");
            assert_eq!(
                url.query_pairs()
                    .find(|(key, _)| key == "offset")
                    .unwrap()
                    .1,
                offset.to_string()
            );
            assert_eq!(
                url.query_pairs().find(|(key, _)| key == "limit").unwrap().1,
                "10"
            );
        }
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
        assert_eq!(image_url_at_least(&images, 300).as_deref(), Some("large"));
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
    async fn artist_album_page_filters_release_types_and_preserves_pagination() {
        let client = client([Response::json(
            200,
            serde_json::json!({"items": [{
            "id": "album-1", "uri": "spotify:album:1", "name": "Album",
            "artists": [{"id": "artist-1", "name": "Artist"}], "images": []
        }], "next": "next", "total": 12}),
        )]);

        let page = artist_albums_page(&client, "spotify:artist:artist-1", 0)
            .await
            .unwrap();

        assert_eq!(page.albums[0].name, "Album");
        assert_eq!(page.next_offset, Some(1));
        assert_eq!(page.total, 12);
        let url = url::Url::parse(&client.transport().requests()[0].url).unwrap();
        assert_eq!(url.path(), "/v1/artists/artist-1/albums");
        assert!(url
            .query_pairs()
            .any(|pair| pair == ("include_groups".into(), "album,single".into())));
        assert!(url
            .query_pairs()
            .any(|pair| pair == ("limit".into(), "10".into())));
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
            Response::rate_limited("3600"),
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

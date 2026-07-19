use retune_core::model::NewTrack;
use retune_spotify::{
    client::{Album, Artist, SpotifyClient, Transport},
    normalize,
    tokens::TokenStore,
};
use std::{
    collections::HashSet,
    sync::atomic::{AtomicBool, Ordering},
};

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
}

pub trait MediaProvider: Send + Sync {
    async fn library_snapshot(&self, kind: LibraryKind) -> Result<Snapshot, String>;
    async fn search(&self, query: &str) -> Result<SearchResults, String>;
    async fn artist_albums(&self, artist: &str) -> Result<Vec<SearchAlbum>, String>;
    async fn album_tracks(&self, album: &str) -> Result<Vec<NewTrack>, String>;
    async fn save_to_spotify(&self, uris: &[String]) -> Result<(), String>;
}

struct GenreSource<'c, T: Transport, S: TokenStore> {
    client: &'c SpotifyClient<T, S>,
    degraded: AtomicBool,
}

impl<'c, T: Transport, S: TokenStore> GenreSource<'c, T, S> {
    fn new(client: &'c SpotifyClient<T, S>) -> Self {
        Self {
            client,
            degraded: AtomicBool::new(false),
        }
    }

    async fn artist(&self, id: &str) -> Result<Option<Artist>, String> {
        if self.degraded.load(Ordering::Relaxed) {
            return Ok(None);
        }
        match self.client.artist(id).await {
            Ok(artist) => Ok(Some(artist)),
            Err(error @ retune_spotify::Error::RateLimited { .. }) => {
                if !self.degraded.swap(true, Ordering::Relaxed) {
                    log::warn!("Spotify artist genres unavailable for this sync: {error}");
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
    album: &Album,
) -> Result<Vec<NewTrack>, String> {
    let mut offset = 0;
    let mut tracks = vec![];
    loop {
        let page = client
            .album_tracks(&album.id, offset, PAGE_SIZE)
            .await
            .map_err(|error| error.to_string())?;
        let count = page.items.len() as u32;
        warm_artists(genres, &page.items, Some(album)).await?;
        for track in page.items {
            tracks.push(normalized_track(genres, &track, Some(album)).await?);
        }
        if page.next.is_none() || count == 0 {
            return Ok(tracks);
        }
        offset += count;
    }
}

impl<T: Transport, S: TokenStore> MediaProvider for SpotifyClient<T, S> {
    async fn library_snapshot(&self, kind: LibraryKind) -> Result<Snapshot, String> {
        let genres = GenreSource::new(self);
        let mut offset = 0;
        let mut batches = vec![];
        loop {
            let (batch, count, has_next) = match kind {
                LibraryKind::Tracks => {
                    let page = self
                        .saved_tracks(offset, PAGE_SIZE)
                        .await
                        .map_err(|error| error.to_string())?;
                    let count = page.items.len() as u32;
                    warm_artists(&genres, page.items.iter().map(|saved| &saved.track), None)
                        .await?;
                    let mut batch = Vec::with_capacity(page.items.len());
                    for saved in page.items {
                        batch.push(normalized_track(&genres, &saved.track, None).await?);
                    }
                    (batch, count, page.next.is_some())
                }
                LibraryKind::Albums => {
                    let page = self
                        .saved_albums(offset, PAGE_SIZE)
                        .await
                        .map_err(|error| error.to_string())?;
                    let count = page.items.len() as u32;
                    let mut batch = vec![];
                    for saved in page.items {
                        batch.extend(normalized_album_tracks(self, &genres, &saved.album).await?);
                    }
                    (batch, count, page.next.is_some())
                }
                LibraryKind::Shows => {
                    let page = self
                        .saved_shows(offset, PAGE_SIZE)
                        .await
                        .map_err(|error| error.to_string())?;
                    let count = page.items.len() as u32;
                    let mut batch = vec![];
                    for saved in page.items {
                        let mut episode_offset = 0;
                        loop {
                            let episodes = self
                                .show_episodes(&saved.show.id, episode_offset, PAGE_SIZE)
                                .await
                                .map_err(|error| error.to_string())?;
                            let episode_count = episodes.items.len() as u32;
                            batch.extend(
                                episodes
                                    .items
                                    .iter()
                                    .map(|episode| normalize::episode(episode, Some(&saved.show))),
                            );
                            if episodes.next.is_none() || episode_count == 0 {
                                break;
                            }
                            episode_offset += episode_count;
                        }
                    }
                    (batch, count, page.next.is_some())
                }
                LibraryKind::Episodes => {
                    let page = self
                        .saved_episodes(offset, PAGE_SIZE)
                        .await
                        .map_err(|error| error.to_string())?;
                    let count = page.items.len() as u32;
                    let batch = page
                        .items
                        .iter()
                        .map(|saved| normalize::episode(&saved.episode, None))
                        .collect();
                    (batch, count, page.next.is_some())
                }
                LibraryKind::Audiobooks => {
                    let page = self
                        .saved_audiobooks(offset, PAGE_SIZE)
                        .await
                        .map_err(|error| error.to_string())?;
                    let count = page.items.len() as u32;
                    let mut batch = vec![];
                    for book in page.items {
                        let mut chapter_offset = 0;
                        loop {
                            let chapters = self
                                .audiobook_chapters(&book.id, chapter_offset, PAGE_SIZE)
                                .await
                                .map_err(|error| error.to_string())?;
                            let chapter_count = chapters.items.len() as u32;
                            batch.extend(
                                chapters
                                    .items
                                    .iter()
                                    .map(|chapter| normalize::chapter(chapter, &book)),
                            );
                            if chapters.next.is_none() || chapter_count == 0 {
                                break;
                            }
                            chapter_offset += chapter_count;
                        }
                    }
                    (batch, count, page.next.is_some())
                }
            };
            batches.push(batch);
            if !has_next || count == 0 {
                return Ok(Snapshot {
                    batches,
                    genres_degraded: genres.degraded.load(Ordering::Relaxed),
                });
            }
            offset += count;
        }
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
            let count = page.items.len() as u32;
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
        let genres = GenreSource::new(self);
        let id = album.rsplit(':').next().unwrap_or(album);
        let mut offset = 0;
        let mut normalized = vec![];
        loop {
            let page = SpotifyClient::album_tracks(self, id, offset, PAGE_SIZE)
                .await
                .map_err(|error| error.to_string())?;
            let count = page.items.len() as u32;
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

#[cfg(test)]
pub struct FakeProvider {
    pub snapshots: std::collections::HashMap<LibraryKind, Vec<Vec<NewTrack>>>,
    pub genres_degraded: bool,
}

#[cfg(test)]
impl MediaProvider for FakeProvider {
    async fn library_snapshot(&self, kind: LibraryKind) -> Result<Snapshot, String> {
        Ok(Snapshot {
            batches: self.snapshots.get(&kind).cloned().unwrap_or_default(),
            genres_degraded: self.genres_degraded,
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
            })),
        )
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
            }], "next": null}),
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
        let mut limited = Response::json(429, serde_json::json!({}));
        limited.headers.insert("retry-after".into(), "3600".into());
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
            limited,
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

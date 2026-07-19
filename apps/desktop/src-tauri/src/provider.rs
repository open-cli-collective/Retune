use retune_core::model::NewTrack;
use retune_spotify::{
    client::{Album, SpotifyClient, Transport},
    normalize,
    tokens::TokenStore,
};

const PAGE_SIZE: u32 = 50;

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

pub trait MediaProvider: Send + Sync {
    async fn library_snapshot(&self, kind: LibraryKind) -> Result<Vec<Vec<NewTrack>>, String>;
    async fn search(&self, query: &str) -> Result<SearchResults, String>;
    async fn album_tracks(&self, album: &str) -> Result<Vec<NewTrack>, String>;
    async fn save_to_spotify(&self, uris: &[String]) -> Result<(), String>;
}

async fn normalized_track<T: Transport, S: TokenStore>(
    client: &SpotifyClient<T, S>,
    track: &retune_spotify::client::Track,
    album: Option<&Album>,
) -> Result<NewTrack, String> {
    let artist = match track
        .artists
        .first()
        .or_else(|| album.and_then(|album| album.artists.first()))
    {
        Some(artist) => Some(
            client
                .artist(&artist.id)
                .await
                .map_err(|error| error.to_string())?,
        ),
        None => None,
    };
    Ok(normalize::track(track, artist.as_ref(), album))
}

async fn normalized_album_tracks<T: Transport, S: TokenStore>(
    client: &SpotifyClient<T, S>,
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
        for track in page.items {
            tracks.push(normalized_track(client, &track, Some(album)).await?);
        }
        if page.next.is_none() || count == 0 {
            return Ok(tracks);
        }
        offset += count;
    }
}

impl<T: Transport, S: TokenStore> MediaProvider for SpotifyClient<T, S> {
    async fn library_snapshot(&self, kind: LibraryKind) -> Result<Vec<Vec<NewTrack>>, String> {
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
                    let mut batch = Vec::with_capacity(page.items.len());
                    for saved in page.items {
                        batch.push(normalized_track(self, &saved.track, None).await?);
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
                        batch.extend(normalized_album_tracks(self, &saved.album).await?);
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
                return Ok(batches);
            }
            offset += count;
        }
    }

    async fn search(&self, query: &str) -> Result<SearchResults, String> {
        let results = SpotifyClient::search(self, query, 0, PAGE_SIZE)
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

    async fn album_tracks(&self, album: &str) -> Result<Vec<NewTrack>, String> {
        let id = album.rsplit(':').next().unwrap_or(album);
        let mut offset = 0;
        let mut normalized = vec![];
        loop {
            let page = SpotifyClient::album_tracks(self, id, offset, PAGE_SIZE)
                .await
                .map_err(|error| error.to_string())?;
            let count = page.items.len() as u32;
            for track in page.items {
                normalized.push(normalized_track(self, &track, None).await?);
            }
            if page.next.is_none() || count == 0 {
                return Ok(normalized);
            }
            offset += count;
        }
    }

    async fn save_to_spotify(&self, uris: &[String]) -> Result<(), String> {
        for uri in uris {
            self.save_to_library(uri)
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub struct FakeProvider {
    pub snapshots: std::collections::HashMap<LibraryKind, Vec<Vec<NewTrack>>>,
}

#[cfg(test)]
impl MediaProvider for FakeProvider {
    async fn library_snapshot(&self, kind: LibraryKind) -> Result<Vec<Vec<NewTrack>>, String> {
        Ok(self.snapshots.get(&kind).cloned().unwrap_or_default())
    }

    async fn search(&self, _query: &str) -> Result<SearchResults, String> {
        Ok(SearchResults {
            artists: vec![],
            albums: vec![],
        })
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
    use retune_spotify::{
        client::{FakeTransport, Response},
        tokens::{InMemoryTokenStore, Tokens},
    };

    use super::*;

    #[tokio::test]
    async fn spotify_provider_pages_and_normalizes_saved_tracks() {
        let transport = FakeTransport::new([
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
        let client = SpotifyClient::new(
            "client",
            transport,
            InMemoryTokenStore::new(Some(Tokens {
                access: "access".into(),
                refresh: "refresh".into(),
                expires_at: u64::MAX,
            })),
        );

        let batches = MediaProvider::library_snapshot(&client, LibraryKind::Tracks)
            .await
            .unwrap();

        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0][0].cat, "rock");
        assert_eq!(batches[0][0].alb, "Album");
        assert!(client.transport().requests()[2]
            .url
            .ends_with("offset=1&limit=50"));
    }
}

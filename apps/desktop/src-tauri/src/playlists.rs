use std::collections::HashSet;

use retune_core::model::Library;
use retune_spotify::{
    client::{Playlist, SpotifyClient, Track, Transport},
    tokens::{TokenStore, Tokens},
    Error,
};
use serde::{Deserialize, Serialize};

const PLAYLIST_PAGE_SIZE: u32 = 50;
const TRACK_PAGE_SIZE: u32 = 100;
pub const RECONNECT_HINT: &str = "Reconnect to Spotify to enable playlists (File → Account).";
pub const STALE_PLAYLIST: &str = "Playlist changed elsewhere — reloaded.";

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct PlaylistCache {
    pub playlists: Vec<CachedPlaylist>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct CachedPlaylist {
    pub id: String,
    pub name: String,
    pub snapshot_id: String,
    pub owned: bool,
    pub tracks: Vec<String>,
    #[serde(default)]
    pub non_library_tracks: Vec<CachedTrack>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct CachedTrack {
    pub uri: String,
    pub name: String,
    pub art: String,
    pub alb: String,
    pub duration: u64,
}

pub async fn sync<T: Transport, S: TokenStore>(
    client: &SpotifyClient<T, S>,
    current: &PlaylistCache,
    library: &Library,
) -> retune_spotify::Result<PlaylistCache> {
    let user_id = client.me().await?.id;
    let mut playlists = vec![];
    let mut offset = 0;
    loop {
        let page = client
            .playlists(offset, PLAYLIST_PAGE_SIZE, &user_id)
            .await?;
        let count = (page.items.len() + page.skipped) as u32;
        for summary in page.items {
            let cached = current
                .playlists
                .iter()
                .find(|playlist| playlist.id == summary.id);
            playlists.push(
                if cached.is_some_and(|playlist| playlist.snapshot_id == summary.snapshot_id) {
                    let mut cached = cached.expect("checked above").clone();
                    cached.name = summary.name;
                    cached.owned = summary.owned;
                    cached.non_library_tracks.retain(|track| {
                        !library
                            .tracks()
                            .iter()
                            .any(|library_track| library_track.uri == track.uri)
                    });
                    cached
                } else {
                    fetch(client, summary, library).await?
                },
            );
        }
        offset += count;
        if count == 0 || page.next.is_none() {
            break;
        }
    }
    Ok(PlaylistCache { playlists })
}

pub async fn reorder<T: Transport, S: TokenStore>(
    client: &SpotifyClient<T, S>,
    cache: &mut PlaylistCache,
    library: &Library,
    id: &str,
    range_start: u32,
    insert_before: u32,
    range_length: u32,
) -> Result<(), PlaylistReorderError> {
    let playlist = cache
        .playlists
        .iter()
        .find(|playlist| playlist.id == id)
        .ok_or_else(|| PlaylistReorderError::Other(format!("Unknown playlist {id}")))?;
    validate_reorder(
        playlist.tracks.len(),
        range_start,
        insert_before,
        range_length,
    )
    .map_err(PlaylistReorderError::Other)?;
    let snapshot_id = playlist.snapshot_id.clone();
    let new_snapshot = match client
        .reorder_playlist_tracks(id, range_start, insert_before, range_length, &snapshot_id)
        .await
    {
        Ok(snapshot) => snapshot,
        Err(Error::Http {
            status: 400 | 409, ..
        }) => {
            refresh_one(client, cache, library, id)
                .await
                .map_err(PlaylistReorderError::Spotify)?;
            return Err(PlaylistReorderError::Reloaded);
        }
        Err(error) => return Err(PlaylistReorderError::Spotify(error)),
    };
    let playlist = cache
        .playlists
        .iter_mut()
        .find(|playlist| playlist.id == id)
        .expect("playlist still exists");
    reorder_uris(
        &mut playlist.tracks,
        range_start as usize,
        insert_before as usize,
        range_length as usize,
    );
    playlist.snapshot_id = new_snapshot;
    Ok(())
}

#[derive(Debug)]
pub enum PlaylistReorderError {
    Reloaded,
    Spotify(Error),
    Other(String),
}

pub fn map_error(error: Error, tokens: Option<&Tokens>) -> String {
    if matches!(error, Error::Http { status: 403, .. })
        && tokens.is_some_and(|tokens| !tokens.missing_scopes().is_empty())
    {
        RECONNECT_HINT.into()
    } else {
        error.to_string()
    }
}

async fn refresh_one<T: Transport, S: TokenStore>(
    client: &SpotifyClient<T, S>,
    cache: &mut PlaylistCache,
    library: &Library,
    id: &str,
) -> retune_spotify::Result<()> {
    let user_id = client.me().await?.id;
    let mut offset = 0;
    loop {
        let page = client
            .playlists(offset, PLAYLIST_PAGE_SIZE, &user_id)
            .await?;
        let count = (page.items.len() + page.skipped) as u32;
        if let Some(summary) = page.items.into_iter().find(|playlist| playlist.id == id) {
            let refreshed = fetch(client, summary, library).await?;
            if let Some(existing) = cache
                .playlists
                .iter_mut()
                .find(|playlist| playlist.id == id)
            {
                *existing = refreshed;
            } else {
                cache.playlists.push(refreshed);
            }
            return Ok(());
        }
        offset += count;
        if count == 0 || page.next.is_none() {
            return Err(Error::InvalidRequest(format!("unknown playlist {id}")));
        }
    }
}

async fn fetch<T: Transport, S: TokenStore>(
    client: &SpotifyClient<T, S>,
    summary: Playlist,
    library: &Library,
) -> retune_spotify::Result<CachedPlaylist> {
    let library_uris = library
        .tracks()
        .iter()
        .map(|track| track.uri.as_str())
        .collect::<HashSet<_>>();
    let mut tracks = vec![];
    let mut non_library_tracks = vec![];
    let mut offset = 0;
    loop {
        let page = client
            .playlist_tracks(&summary.id, offset, TRACK_PAGE_SIZE)
            .await?;
        let count = (page.items.len() + page.skipped) as u32;
        for track in page.items {
            if !library_uris.contains(track.uri.as_str()) {
                non_library_tracks.push(cached_track(&track));
            }
            tracks.push(track.uri);
        }
        offset += count;
        if count == 0 || page.next.is_none() {
            break;
        }
    }
    Ok(CachedPlaylist {
        id: summary.id,
        name: summary.name,
        snapshot_id: summary.snapshot_id,
        owned: summary.owned,
        tracks,
        non_library_tracks,
    })
}

fn cached_track(track: &Track) -> CachedTrack {
    CachedTrack {
        uri: track.uri.clone(),
        name: track.name.clone(),
        art: track
            .artists
            .first()
            .map(|artist| artist.name.clone())
            .unwrap_or_default(),
        alb: track
            .album
            .as_ref()
            .map(|album| album.name.clone())
            .unwrap_or_default(),
        duration: track.duration_ms.unwrap_or_default(),
    }
}

fn validate_reorder(
    track_count: usize,
    range_start: u32,
    insert_before: u32,
    range_length: u32,
) -> Result<(), String> {
    let start = range_start as usize;
    let length = range_length as usize;
    if length == 0
        || start
            .checked_add(length)
            .is_none_or(|end| end > track_count)
        || insert_before as usize > track_count
    {
        return Err("Playlist reorder range is out of bounds.".into());
    }
    Ok(())
}

fn reorder_uris(uris: &mut Vec<String>, start: usize, insert_before: usize, length: usize) {
    let moved = uris.drain(start..start + length).collect::<Vec<_>>();
    let destination = if insert_before > start {
        insert_before.saturating_sub(length)
    } else {
        insert_before
    };
    uris.splice(destination..destination, moved);
}

#[cfg(test)]
mod tests {
    use retune_spotify::{
        client::{FakeTransport, Response},
        tokens::InMemoryTokenStore,
    };

    use super::*;

    fn tokens(scopes: &str) -> InMemoryTokenStore {
        InMemoryTokenStore::new(Some(Tokens {
            access: "access".into(),
            refresh: "refresh".into(),
            expires_at: u64::MAX,
            scopes: scopes.into(),
        }))
    }

    fn cached() -> PlaylistCache {
        PlaylistCache {
            playlists: vec![CachedPlaylist {
                id: "playlist".into(),
                name: "Old name".into(),
                snapshot_id: "same".into(),
                owned: true,
                tracks: vec!["spotify:track:1".into(), "spotify:track:2".into()],
                non_library_tracks: vec![],
            }],
        }
    }

    #[tokio::test]
    async fn unchanged_snapshot_skips_track_fetch() {
        let client = SpotifyClient::new(
            "client",
            FakeTransport::new([
                Response::json(200, serde_json::json!({"id": "user"})),
                Response::json(
                    200,
                    serde_json::json!({
                        "items": [{
                            "id": "playlist", "name": "New name", "snapshot_id": "same",
                            "owner": {"id": "user"}, "tracks": {"total": 2}
                        }],
                        "next": null
                    }),
                ),
            ]),
            tokens(retune_spotify::auth::SCOPES),
        );

        let synced = sync(&client, &cached(), &Library::new()).await.unwrap();

        assert_eq!(synced.playlists[0].name, "New name");
        assert_eq!(synced.playlists[0].tracks.len(), 2);
        assert_eq!(client.transport().requests().len(), 2);
    }

    #[tokio::test]
    async fn stale_reorder_refreshes_tracks_and_reports_reload() {
        let client = SpotifyClient::new(
            "client",
            FakeTransport::new([
                Response::json(
                    409,
                    serde_json::json!({"error": {"message": "Snapshot mismatch"}}),
                ),
                Response::json(200, serde_json::json!({"id": "user"})),
                Response::json(
                    200,
                    serde_json::json!({
                        "items": [{
                            "id": "playlist", "name": "Fresh", "snapshot_id": "fresh",
                            "owner": {"id": "user"}, "tracks": {"total": 1}
                        }],
                        "next": null
                    }),
                ),
                Response::json(
                    200,
                    serde_json::json!({
                        "items": [{"is_local": false, "track": {
                            "uri": "spotify:track:fresh", "name": "Fresh",
                            "artists": [], "album": null, "duration_ms": 1000
                        }}],
                        "next": null
                    }),
                ),
            ]),
            tokens(retune_spotify::auth::SCOPES),
        );
        let mut cache = cached();

        let error = reorder(&client, &mut cache, &Library::new(), "playlist", 0, 2, 1)
            .await
            .unwrap_err();

        assert!(matches!(error, PlaylistReorderError::Reloaded));
        assert_eq!(cache.playlists[0].snapshot_id, "fresh");
        assert_eq!(cache.playlists[0].tracks, ["spotify:track:fresh"]);
    }

    #[tokio::test]
    async fn successful_reorder_updates_cached_order_and_snapshot() {
        let client = SpotifyClient::new(
            "client",
            FakeTransport::new([Response::json(
                200,
                serde_json::json!({"snapshot_id": "reordered"}),
            )]),
            tokens(retune_spotify::auth::SCOPES),
        );
        let mut cache = cached();

        reorder(&client, &mut cache, &Library::new(), "playlist", 0, 2, 1)
            .await
            .unwrap();

        assert_eq!(cache.playlists[0].snapshot_id, "reordered");
        assert_eq!(
            cache.playlists[0].tracks,
            ["spotify:track:2", "spotify:track:1"]
        );
    }

    #[test]
    fn forbidden_with_legacy_scopes_maps_to_reconnect_hint() {
        let legacy = Tokens {
            access: String::new(),
            refresh: String::new(),
            expires_at: 0,
            scopes: "user-library-read".into(),
        };
        let error = Error::Http {
            endpoint: "/playlists/id/tracks".into(),
            status: 403,
            body: "Insufficient client scope".into(),
        };

        assert_eq!(map_error(error, Some(&legacy)), RECONNECT_HINT);
    }
}

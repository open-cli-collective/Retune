use std::collections::{HashMap, HashSet};

use retune_core::model::Library;
use retune_spotify::{
    client::{Playlist, SpotifyClient, Track, Transport},
    tokens::{TokenStore, Tokens},
    Error,
};
use serde::{Deserialize, Serialize};

const PLAYLIST_PAGE_SIZE: u32 = 50;
const TRACK_PAGE_SIZE: u32 = 50;
pub(crate) const TRACK_METADATA_VERSION: u8 = 1;
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
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub track_count: usize,
    pub tracks: Vec<String>,
    #[serde(default)]
    pub track_metadata_version: u8,
    #[serde(default, alias = "non_library_tracks")]
    pub spotify_tracks: Vec<CachedTrack>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct CachedTrack {
    pub uri: String,
    pub name: String,
    pub art: String,
    pub alb: String,
    pub duration: u64,
    #[serde(default)]
    pub disc_no: Option<u32>,
    #[serde(default)]
    pub track_no: Option<u32>,
    #[serde(default)]
    pub release_date: Option<String>,
}

pub async fn sync<T: Transport, S: TokenStore>(
    client: &SpotifyClient<T, S>,
    current: &PlaylistCache,
) -> retune_spotify::Result<PlaylistCache> {
    let user_id = client.me().await?.id;
    let mut refreshed = HashMap::new();
    let mut spotify_order = vec![];
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
            spotify_order.push(summary.id.clone());
            if !summary.owned && !summary.collaborative {
                let mut cached = cached.cloned().unwrap_or_else(|| CachedPlaylist {
                    id: summary.id.clone(),
                    name: summary.name.clone(),
                    snapshot_id: summary.snapshot_id.clone(),
                    owned: false,
                    owner: None,
                    track_count: 0,
                    tracks: vec![],
                    track_metadata_version: TRACK_METADATA_VERSION,
                    spotify_tracks: vec![],
                });
                cached.name = summary.name;
                cached.snapshot_id = summary.snapshot_id;
                cached.owner = Some(summary.owner.display_name.unwrap_or(summary.owner.id));
                cached.track_count = summary.tracks.total as usize;
                refreshed.insert(summary.id, cached);
                continue;
            }
            refreshed.insert(
                summary.id.clone(),
                if cached.is_some_and(|playlist| {
                    playlist.snapshot_id == summary.snapshot_id
                        && playlist.track_metadata_version == TRACK_METADATA_VERSION
                        && (playlist.track_count == 0 || !playlist.tracks.is_empty())
                }) {
                    let mut cached = cached.expect("checked above").clone();
                    cached.name = summary.name;
                    cached.owned = summary.owned;
                    cached.owner = Some(summary.owner.display_name.unwrap_or(summary.owner.id));
                    cached.track_count = summary.tracks.total as usize;
                    cached
                } else {
                    fetch(client, summary).await?
                },
            );
        }
        offset += count;
        if count == 0 || page.next.is_none() {
            break;
        }
    }
    let mut playlists = current
        .playlists
        .iter()
        .filter_map(|playlist| refreshed.remove(&playlist.id))
        .collect::<Vec<_>>();
    playlists.extend(
        spotify_order
            .into_iter()
            .filter_map(|id| refreshed.remove(&id)),
    );
    Ok(PlaylistCache { playlists })
}

pub fn reorder_playlists(cache: &mut PlaylistCache, ids: &[String]) -> Result<(), String> {
    if ids.len() != cache.playlists.len()
        || ids.iter().collect::<HashSet<_>>().len() != ids.len()
        || ids
            .iter()
            .any(|id| !cache.playlists.iter().any(|playlist| &playlist.id == id))
    {
        return Err("Playlist reorder must contain every playlist exactly once.".into());
    }
    let mut by_id = std::mem::take(&mut cache.playlists)
        .into_iter()
        .map(|playlist| (playlist.id.clone(), playlist))
        .collect::<HashMap<_, _>>();
    cache.playlists = ids
        .iter()
        .map(|id| by_id.remove(id).expect("validated playlist id"))
        .collect();
    Ok(())
}

pub async fn unfollow<T: Transport, S: TokenStore>(
    client: &SpotifyClient<T, S>,
    cache: &mut PlaylistCache,
    id: &str,
) -> retune_spotify::Result<()> {
    if !cache.playlists.iter().any(|playlist| playlist.id == id) {
        return Err(Error::InvalidRequest(format!("unknown playlist {id}")));
    }
    client.unfollow_playlist(id).await?;
    cache.playlists.retain(|playlist| playlist.id != id);
    Ok(())
}

pub async fn create<T: Transport, S: TokenStore>(
    client: &SpotifyClient<T, S>,
    cache: &mut PlaylistCache,
    name: &str,
) -> retune_spotify::Result<()> {
    let created = client.create_playlist(name).await?;
    cache.playlists.push(CachedPlaylist {
        id: created.id,
        name: created.name,
        snapshot_id: created.snapshot_id,
        owned: true,
        owner: None,
        track_count: 0,
        tracks: vec![],
        track_metadata_version: TRACK_METADATA_VERSION,
        spotify_tracks: vec![],
    });
    Ok(())
}

pub async fn add<T: Transport, S: TokenStore>(
    client: &SpotifyClient<T, S>,
    cache: &mut PlaylistCache,
    library: &Library,
    id: &str,
    uris: Vec<String>,
) -> Result<(), PlaylistAddError> {
    reject_local_uris(&uris, |uri| {
        library
            .tracks()
            .iter()
            .find(|track| track.uri == uri)
            .map(|track| track.name.clone())
    })
    .map_err(PlaylistAddError::Local)?;
    let index = cache
        .playlists
        .iter()
        .position(|playlist| playlist.id == id)
        .ok_or_else(|| PlaylistAddError::Unknown(id.into()))?;
    if !cache.playlists[index].owned {
        return Err(PlaylistAddError::ReadOnly);
    }
    let snapshot_id = client.add_playlist_tracks(id, &uris, None).await?;
    let (track_count, tracks, spotify_tracks) = fetch_tracks(client, id).await?;
    let playlist = &mut cache.playlists[index];
    playlist.snapshot_id = snapshot_id;
    playlist.track_count = track_count;
    playlist.tracks = tracks;
    playlist.track_metadata_version = TRACK_METADATA_VERSION;
    playlist.spotify_tracks = spotify_tracks;
    Ok(())
}

#[derive(Debug)]
pub enum PlaylistAddError {
    Local(String),
    Unknown(String),
    ReadOnly,
    Spotify(Error),
}

pub(crate) fn reject_local_uris(
    uris: &[String],
    mut name: impl FnMut(&str) -> Option<String>,
) -> Result<(), String> {
    let local = uris
        .iter()
        .filter(|uri| uri.starts_with("file:"))
        .map(|uri| {
            name(uri).unwrap_or_else(|| {
                crate::localfiles::path_from_file_uri(uri)
                    .ok()
                    .and_then(|path| {
                        path.file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                    })
                    .unwrap_or_else(|| uri.clone())
            })
        })
        .collect::<Vec<_>>();
    if local.is_empty() {
        return Ok(());
    }
    let count = local.len();
    let remaining = count.saturating_sub(3);
    let mut message = format!(
        "{count} local file track{} can't be added to Spotify: {}",
        if count == 1 { "" } else { "s" },
        local.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
    );
    if remaining > 0 {
        message.push_str(&format!(" + {remaining} more"));
    }
    message.push_str(". Local files stay local.");
    Err(message)
}

impl From<Error> for PlaylistAddError {
    fn from(error: Error) -> Self {
        Self::Spotify(error)
    }
}

pub async fn reorder<T: Transport, S: TokenStore>(
    client: &SpotifyClient<T, S>,
    cache: &mut PlaylistCache,
    id: &str,
    range_start: u32,
    insert_before: u32,
    range_length: u32,
) -> Result<(), PlaylistMutationError> {
    let playlist = cache
        .playlists
        .iter()
        .find(|playlist| playlist.id == id)
        .ok_or_else(|| PlaylistMutationError::Other(format!("Unknown playlist {id}")))?;
    if !playlist.owned {
        return Err(PlaylistMutationError::Other(
            "Only your playlists can be reordered.".into(),
        ));
    }
    if playlist.tracks.len() != playlist.track_count {
        return Err(PlaylistMutationError::Other(
            "This playlist contains items Retune cannot safely reorder.".into(),
        ));
    }
    validate_reorder(
        playlist.tracks.len(),
        range_start,
        insert_before,
        range_length,
    )
    .map_err(PlaylistMutationError::Other)?;
    let snapshot_id = playlist.snapshot_id.clone();
    let result = client
        .reorder_playlist_tracks(id, range_start, insert_before, range_length, &snapshot_id)
        .await;
    let new_snapshot = recover_stale_snapshot(client, cache, id, result).await?;
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
pub enum PlaylistMutationError {
    Reloaded,
    Spotify(Error),
    Other(String),
}

impl From<Error> for PlaylistMutationError {
    fn from(error: Error) -> Self {
        Self::Spotify(error)
    }
}

async fn recover_stale_snapshot<T: Transport, S: TokenStore>(
    client: &SpotifyClient<T, S>,
    cache: &mut PlaylistCache,
    id: &str,
    result: retune_spotify::Result<String>,
) -> Result<String, PlaylistMutationError> {
    match result {
        Ok(snapshot) => Ok(snapshot),
        Err(Error::Http {
            status: 400 | 409, ..
        }) => {
            refresh_one(client, cache, id)
                .await
                .map_err(PlaylistMutationError::Spotify)?;
            Err(PlaylistMutationError::Reloaded)
        }
        Err(error) => Err(PlaylistMutationError::Spotify(error)),
    }
}

pub async fn remove<T: Transport, S: TokenStore>(
    client: &SpotifyClient<T, S>,
    cache: &mut PlaylistCache,
    id: &str,
    indices: &[u32],
) -> Result<(), PlaylistMutationError> {
    let playlist = cache
        .playlists
        .iter()
        .find(|playlist| playlist.id == id)
        .ok_or_else(|| PlaylistMutationError::Other(format!("Unknown playlist {id}")))?;
    if !playlist.owned {
        return Err(PlaylistMutationError::Other(
            "Only your playlists can be changed.".into(),
        ));
    }
    if playlist.tracks.len() != playlist.track_count {
        return Err(PlaylistMutationError::Other(
            "This playlist contains items Retune cannot safely remove.".into(),
        ));
    }
    if indices.is_empty() {
        return Err(PlaylistMutationError::Other(
            "Select at least one track to remove.".into(),
        ));
    }
    let selected = indices
        .iter()
        .map(|index| *index as usize)
        .collect::<HashSet<_>>();
    if selected.len() != indices.len()
        || selected.iter().any(|index| *index >= playlist.tracks.len())
    {
        return Err(PlaylistMutationError::Other(
            "Playlist removal selection is invalid.".into(),
        ));
    }
    let selected_uris = selected
        .iter()
        .map(|index| playlist.tracks[*index].as_str())
        .collect::<HashSet<_>>();
    if playlist
        .tracks
        .iter()
        .enumerate()
        .any(|(index, uri)| selected_uris.contains(uri.as_str()) && !selected.contains(&index))
    {
        return Err(PlaylistMutationError::Other(
            "Select every occurrence of a duplicate track before removing it.".into(),
        ));
    }
    let mut seen = HashSet::new();
    let uris = indices
        .iter()
        .map(|index| playlist.tracks[*index as usize].clone())
        .filter(|uri| seen.insert(uri.clone()))
        .collect::<Vec<_>>();
    let snapshot_id = playlist.snapshot_id.clone();
    let result = client.remove_playlist_tracks(id, &uris, &snapshot_id).await;
    let new_snapshot = recover_stale_snapshot(client, cache, id, result).await?;
    let (track_count, tracks, spotify_tracks) = fetch_tracks(client, id).await?;
    let playlist = cache
        .playlists
        .iter_mut()
        .find(|playlist| playlist.id == id)
        .expect("playlist still exists");
    playlist.snapshot_id = new_snapshot;
    playlist.track_count = track_count;
    playlist.tracks = tracks;
    playlist.track_metadata_version = TRACK_METADATA_VERSION;
    playlist.spotify_tracks = spotify_tracks;
    Ok(())
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
            let refreshed = fetch(client, summary).await?;
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
) -> retune_spotify::Result<CachedPlaylist> {
    let (track_count, tracks, spotify_tracks) = fetch_tracks(client, &summary.id).await?;
    Ok(CachedPlaylist {
        id: summary.id,
        name: summary.name,
        snapshot_id: summary.snapshot_id,
        owned: summary.owned,
        owner: Some(summary.owner.display_name.unwrap_or(summary.owner.id)),
        track_count,
        tracks,
        track_metadata_version: TRACK_METADATA_VERSION,
        spotify_tracks,
    })
}

async fn fetch_tracks<T: Transport, S: TokenStore>(
    client: &SpotifyClient<T, S>,
    id: &str,
) -> retune_spotify::Result<(usize, Vec<String>, Vec<CachedTrack>)> {
    let mut tracks = vec![];
    let mut spotify_tracks = vec![];
    let mut track_count = None;
    let mut offset = 0;
    loop {
        let page = client.playlist_tracks(id, offset, TRACK_PAGE_SIZE).await?;
        track_count.get_or_insert(page.total as usize);
        let count = (page.items.len() + page.skipped) as u32;
        for track in page.items {
            spotify_tracks.push(cached_track(&track));
            tracks.push(track.uri);
        }
        offset += count;
        if count == 0 || page.next.is_none() {
            break;
        }
    }
    Ok((track_count.unwrap_or_default(), tracks, spotify_tracks))
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
        disc_no: track.disc_number,
        track_no: track.track_number,
        release_date: track
            .album
            .as_ref()
            .and_then(|album| album.release_date.clone()),
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
    use crate::store::FsPlaylistStore;
    use retune_core::model::NewTrack;
    use retune_spotify::{
        client::{fake_client, FakeTransport, Method, Response},
        tokens::InMemoryTokenStore,
    };

    use super::*;

    fn cached() -> PlaylistCache {
        PlaylistCache {
            playlists: vec![CachedPlaylist {
                id: "playlist".into(),
                name: "Old name".into(),
                snapshot_id: "same".into(),
                owned: true,
                owner: None,
                track_count: 2,
                tracks: vec!["spotify:track:1".into(), "spotify:track:2".into()],
                track_metadata_version: TRACK_METADATA_VERSION,
                spotify_tracks: vec![],
            }],
        }
    }

    fn ordered() -> PlaylistCache {
        let mut cache = cached();
        for id in ["second", "third"] {
            let mut playlist = cache.playlists[0].clone();
            playlist.id = id.into();
            playlist.name = id.into();
            cache.playlists.push(playlist);
        }
        cache
    }

    #[test]
    fn legacy_track_cache_deserializes_for_refresh() {
        let playlist: CachedPlaylist = serde_json::from_value(serde_json::json!({
            "id": "playlist",
            "name": "Playlist",
            "snapshot_id": "snapshot",
            "owned": true,
            "tracks": ["spotify:track:one"],
            "non_library_tracks": [{
                "uri": "spotify:track:one",
                "name": "Song",
                "art": "Artist",
                "alb": "Album",
                "duration": 180000
            }]
        }))
        .unwrap();

        assert_eq!(playlist.track_metadata_version, 0);
        assert_eq!(playlist.spotify_tracks[0].track_no, None);
    }

    #[test]
    fn playlist_reorder_validates_atomically_and_persists() {
        let mut cache = ordered();
        let before = serde_json::to_vec(&cache).unwrap();
        assert!(reorder_playlists(&mut cache, &["playlist".into(), "second".into()]).is_err());
        assert_eq!(serde_json::to_vec(&cache).unwrap(), before);
        assert!(reorder_playlists(
            &mut cache,
            &["playlist".into(), "second".into(), "second".into()]
        )
        .is_err());
        assert_eq!(serde_json::to_vec(&cache).unwrap(), before);

        reorder_playlists(
            &mut cache,
            &["third".into(), "playlist".into(), "second".into()],
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = FsPlaylistStore::new(dir.path());
        store.save(&cache).unwrap();
        assert_eq!(
            store
                .load()
                .unwrap()
                .playlists
                .iter()
                .map(|playlist| playlist.id.as_str())
                .collect::<Vec<_>>(),
            ["third", "playlist", "second"]
        );
    }

    fn local_track(uri: &str, name: &str) -> NewTrack {
        NewTrack {
            uri: uri.into(),
            cat: "Genre".into(),
            art: "Artist".into(),
            alb: "Album".into(),
            name: name.into(),
            duration: std::time::Duration::from_secs(1),
            ..NewTrack::default()
        }
    }

    #[test]
    fn local_uri_guard_accepts_spotify_and_names_local_tracks() {
        assert_eq!(
            reject_local_uris(&["spotify:track:one".into()], |_| None),
            Ok(())
        );
        assert_eq!(
            reject_local_uris(&["file:///tmp/Fallback%20name.mp3".into()], |_| {
                Some("Library name".into())
            }),
            Err(
                "1 local file track can't be added to Spotify: Library name. Local files stay local."
                    .into()
            )
        );
    }

    #[test]
    fn local_uri_guard_bounds_reported_names() {
        let root = std::env::temp_dir();
        let uris = (1..=5)
            .map(|index| crate::localfiles::file_uri(&root.join(format!("Track {index}.mp3"))))
            .collect::<Vec<_>>();

        assert_eq!(
            reject_local_uris(&uris, |_| None),
            Err("5 local file tracks can't be added to Spotify: Track 1.mp3, Track 2.mp3, Track 3.mp3 + 2 more. Local files stay local.".into())
        );
    }

    #[tokio::test]
    async fn add_rejects_mixed_local_uris_before_transport_or_playlist_validation() {
        let client = fake_client([], &retune_spotify::auth::SCOPES);
        let mut library = Library::new();
        library.add(local_track("file:///tmp/local.mp3", "Local song"));

        let error = add(
            &client,
            &mut PlaylistCache::default(),
            &library,
            "missing",
            vec!["spotify:track:one".into(), "file:///tmp/local.mp3".into()],
        )
        .await
        .unwrap_err();

        assert!(
            matches!(error, PlaylistAddError::Local(message) if message.contains("Local song"))
        );
        assert!(client.transport().requests().is_empty());
    }

    #[tokio::test]
    async fn create_inserts_empty_owned_playlist_and_persists() {
        let client = fake_client(
            [Response::json(
                201,
                serde_json::json!({
                    "id": "new", "name": "Road Trip", "snapshot_id": "snapshot"
                }),
            )],
            &retune_spotify::auth::SCOPES,
        );
        let mut cache = PlaylistCache::default();

        create(&client, &mut cache, "Road Trip").await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = FsPlaylistStore::new(dir.path());
        store.save(&cache).unwrap();

        assert_eq!(store.load().unwrap(), cache);
        assert_eq!(cache.playlists[0].tracks, Vec::<String>::new());
        assert!(cache.playlists[0].owned);
    }

    #[tokio::test]
    async fn legacy_playlist_cache_refreshes_spotify_metadata_for_all_tracks() {
        let client = fake_client(
            [
                Response::json(200, serde_json::json!({"id": "user"})),
                Response::json(
                    200,
                    serde_json::json!({
                        "items": [{
                            "id": "playlist", "name": "Collaborative", "snapshot_id": "same", "collaborative": true,
                            "owner": {"id": "other", "display_name": "Other"},
                            "items": {"total": 2}
                        }],
                        "next": null
                    }),
                ),
                Response::json(
                    200,
                    serde_json::json!({
                        "items": [
                            {"is_local": false, "item": {
                                "uri": "spotify:track:1", "name": "Library track",
                                "artists": [],
                                "album": {"id": "one", "uri": "spotify:album:one", "name": "Library Album", "release_date": "1999", "images": []},
                                "duration_ms": 1000, "disc_number": 2, "track_number": 7
                            }},
                            {"is_local": false, "item": {
                                "uri": "spotify:track:2", "name": "Followed track",
                                "artists": [{"id": "artist", "name": "Artist"}],
                                "album": {"id": "album", "uri": "spotify:album:album", "name": "Album", "release_date": "2024-03-01", "images": []},
                                "duration_ms": 2345, "disc_number": 1, "track_number": 3
                            }}
                        ],
                        "next": null, "total": 2
                    }),
                ),
            ],
            &retune_spotify::auth::SCOPES,
        );
        let mut current = cached();
        current.playlists[0].owned = false;
        current.playlists[0].track_metadata_version = 0;

        let synced = sync(&client, &current).await.unwrap();

        let playlist = &synced.playlists[0];
        assert_eq!(playlist.track_count, 2);
        assert!(!playlist.owned);
        assert_eq!(playlist.tracks, ["spotify:track:1", "spotify:track:2"]);
        assert_eq!(
            playlist.spotify_tracks,
            [
                CachedTrack {
                    uri: "spotify:track:1".into(),
                    name: "Library track".into(),
                    art: String::new(),
                    alb: "Library Album".into(),
                    duration: 1000,
                    disc_no: Some(2),
                    track_no: Some(7),
                    release_date: Some("1999".into()),
                },
                CachedTrack {
                    uri: "spotify:track:2".into(),
                    name: "Followed track".into(),
                    art: "Artist".into(),
                    alb: "Album".into(),
                    duration: 2345,
                    disc_no: Some(1),
                    track_no: Some(3),
                    release_date: Some("2024-03-01".into()),
                }
            ]
        );
        let requests = client.transport().requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            url::Url::parse(&requests[2].url).unwrap().path(),
            "/v1/playlists/playlist/items"
        );
    }

    #[tokio::test]
    async fn unchanged_followed_playlist_snapshot_skips_track_fetch() {
        let client = fake_client(
            [
                Response::json(200, serde_json::json!({"id": "user"})),
                Response::json(
                    200,
                    serde_json::json!({
                        "items": [{
                            "id": "playlist", "name": "New name", "snapshot_id": "same",
                            "owner": {"id": "other", "display_name": "Owner Name"},
                            "tracks": {"total": 2}
                        }],
                        "next": null
                    }),
                ),
            ],
            &retune_spotify::auth::SCOPES,
        );
        let mut current = cached();
        current.playlists[0].owned = false;

        let synced = sync(&client, &current).await.unwrap();

        assert_eq!(synced.playlists[0].name, "New name");
        assert_eq!(synced.playlists[0].owner.as_deref(), Some("Owner Name"));
        assert!(!synced.playlists[0].owned);
        assert_eq!(synced.playlists[0].tracks.len(), 2);
        let requests = client.transport().requests();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].url.ends_with("/me"));
        assert!(requests[1].url.contains("/me/playlists?"));
    }

    #[tokio::test]
    async fn unchanged_empty_followed_playlist_snapshot_skips_track_fetch() {
        let client = fake_client(
            [
                Response::json(200, serde_json::json!({"id": "user"})),
                Response::json(
                    200,
                    serde_json::json!({
                        "items": [{
                            "id": "playlist", "name": "Empty", "snapshot_id": "same",
                            "owner": {"id": "other"}, "tracks": {"total": 0}
                        }],
                        "next": null
                    }),
                ),
            ],
            &retune_spotify::auth::SCOPES,
        );
        let mut current = cached();
        current.playlists[0].owned = false;
        current.playlists[0].track_count = 0;
        current.playlists[0].tracks.clear();

        let synced = sync(&client, &current).await.unwrap();

        assert!(synced.playlists[0].tracks.is_empty());
        let requests = client.transport().requests();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].url.ends_with("/me"));
        assert!(requests[1].url.contains("/me/playlists?"));
    }

    #[tokio::test]
    async fn followed_playlist_with_empty_cache_does_not_request_forbidden_items() {
        let summary = || {
            Response::json(
                200,
                serde_json::json!({
                    "items": [{
                        "id": "playlist", "name": "Followed", "snapshot_id": "same",
                        "owner": {"id": "other", "display_name": "Other"},
                        "tracks": {"total": 3}
                    }],
                    "next": null
                }),
            )
        };
        let client = fake_client(
            [
                Response::json(200, serde_json::json!({"id": "user"})),
                summary(),
                Response::json(200, serde_json::json!({"id": "user"})),
                summary(),
            ],
            &retune_spotify::auth::SCOPES,
        );
        let current = PlaylistCache::default();

        let skipped = sync(&client, &current).await.unwrap();
        assert_eq!(skipped.playlists[0].track_count, 3);
        let synced_again = sync(&client, &skipped).await.unwrap();
        assert!(synced_again.playlists[0].tracks.is_empty());
        assert_eq!(
            client
                .transport()
                .requests()
                .iter()
                .filter(|request| {
                    url::Url::parse(&request.url)
                        .is_ok_and(|url| url.path() == "/v1/playlists/playlist/items")
                })
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn sync_keeps_cached_order_drops_missing_and_appends_new_in_spotify_order() {
        let mut current = ordered();
        current.playlists.swap(0, 1);
        let summaries = |items: serde_json::Value| {
            Response::json(
                200,
                serde_json::json!({
                    "items": items, "next": null
                }),
            )
        };
        let summary = |id: &str| {
            serde_json::json!({
                "id": id, "name": id, "snapshot_id": "same",
                "owner": {"id": "user"}, "tracks": {"total": 2}
            })
        };
        let client = fake_client(
            [
                Response::json(200, serde_json::json!({"id": "user"})),
                summaries(serde_json::json!([
                    summary("playlist"),
                    summary("new-a"),
                    summary("second"),
                    summary("new-b")
                ])),
                Response::json(
                    200,
                    serde_json::json!({"items": [], "next": null, "total": 0}),
                ),
                Response::json(
                    200,
                    serde_json::json!({"items": [], "next": null, "total": 0}),
                ),
            ],
            &retune_spotify::auth::SCOPES,
        );

        let synced = sync(&client, &current).await.unwrap();

        assert_eq!(
            synced
                .playlists
                .iter()
                .map(|playlist| playlist.id.as_str())
                .collect::<Vec<_>>(),
            ["second", "playlist", "new-a", "new-b"]
        );
    }

    #[tokio::test]
    async fn unfollow_removes_only_after_success() {
        for owned in [true, false] {
            let mut cache = cached();
            cache.playlists[0].owned = owned;
            let client = fake_client(
                [Response::json(200, serde_json::Value::Null)],
                &retune_spotify::auth::SCOPES,
            );
            unfollow(&client, &mut cache, "playlist").await.unwrap();
            assert!(cache.playlists.is_empty());
            let request = &client.transport().requests()[0];
            assert_eq!(request.method, Method::Delete);
            assert_eq!(
                url::Url::parse(&request.url).unwrap().path(),
                "/v1/playlists/playlist/followers"
            );
        }

        for status in [403, 500] {
            let mut cache = cached();
            let before = serde_json::to_vec(&cache).unwrap();
            let responses = std::iter::repeat_n(
                Response::json(status, serde_json::json!({"error": {"message": "no"}})),
                3,
            );
            let client = fake_client(responses, &retune_spotify::auth::SCOPES);
            assert!(unfollow(&client, &mut cache, "playlist").await.is_err());
            assert_eq!(serde_json::to_vec(&cache).unwrap(), before);
        }
    }

    #[tokio::test]
    async fn signed_out_unfollow_does_not_touch_transport_or_cache() {
        let mut cache = cached();
        let client = SpotifyClient::new(
            "client",
            FakeTransport::default(),
            InMemoryTokenStore::new(None),
        );

        assert!(matches!(
            unfollow(&client, &mut cache, "playlist").await,
            Err(Error::MissingToken)
        ));
        assert_eq!(cache, cached());
        assert!(client.transport().requests().is_empty());
    }

    fn stale_refresh_responses() -> [Response; 4] {
        [
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
                    "next": null, "total": 1
                }),
            ),
        ]
    }

    #[tokio::test]
    async fn stale_reorder_refreshes_tracks_and_reports_reload() {
        let client = fake_client(stale_refresh_responses(), &retune_spotify::auth::SCOPES);
        let mut cache = cached();

        let error = reorder(&client, &mut cache, "playlist", 0, 2, 1)
            .await
            .unwrap_err();

        assert!(matches!(error, PlaylistMutationError::Reloaded));
        assert_eq!(cache.playlists[0].snapshot_id, "fresh");
        assert_eq!(cache.playlists[0].owner.as_deref(), Some("user"));
        assert_eq!(cache.playlists[0].tracks, ["spotify:track:fresh"]);
    }

    #[tokio::test]
    async fn successful_reorder_updates_cached_order_and_snapshot() {
        let client = fake_client(
            [Response::json(
                200,
                serde_json::json!({"snapshot_id": "reordered"}),
            )],
            &retune_spotify::auth::SCOPES,
        );
        let mut cache = cached();

        reorder(&client, &mut cache, "playlist", 0, 2, 1)
            .await
            .unwrap();

        assert_eq!(cache.playlists[0].snapshot_id, "reordered");
        assert_eq!(
            cache.playlists[0].tracks,
            ["spotify:track:2", "spotify:track:1"]
        );
    }

    #[tokio::test]
    async fn remove_refreshes_canonical_tracks_after_success() {
        let client = fake_client(
            [
                Response::json(200, serde_json::json!({"snapshot_id": "removed"})),
                Response::json(
                    200,
                    serde_json::json!({
                        "items": [{"is_local": false, "track": {
                            "uri": "spotify:track:2", "name": "Two",
                            "artists": [], "album": null, "duration_ms": 1000
                        }}],
                        "next": null, "total": 1
                    }),
                ),
            ],
            &retune_spotify::auth::SCOPES,
        );
        let mut cache = cached();

        remove(&client, &mut cache, "playlist", &[0]).await.unwrap();

        assert_eq!(cache.playlists[0].snapshot_id, "removed");
        assert_eq!(cache.playlists[0].track_count, 1);
        assert_eq!(cache.playlists[0].tracks, ["spotify:track:2"]);
        let request = &client.transport().requests()[0];
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&request.body).unwrap()["items"],
            serde_json::json!([{"uri": "spotify:track:1"}])
        );
    }

    #[tokio::test]
    async fn remove_rejects_unsafe_selections_before_writing() {
        let client = fake_client([], &retune_spotify::auth::SCOPES);
        let mut cache = cached();

        assert!(matches!(
            remove(&client, &mut cache, "playlist", &[]).await,
            Err(PlaylistMutationError::Other(_))
        ));
        assert!(matches!(
            remove(&client, &mut cache, "playlist", &[2]).await,
            Err(PlaylistMutationError::Other(_))
        ));
        cache.playlists[0].owned = false;
        assert!(matches!(
            remove(&client, &mut cache, "playlist", &[0]).await,
            Err(PlaylistMutationError::Other(_))
        ));
        cache.playlists[0].owned = true;
        cache.playlists[0].track_count = 3;
        assert!(matches!(
            remove(&client, &mut cache, "playlist", &[0]).await,
            Err(PlaylistMutationError::Other(_))
        ));
        assert!(client.transport().requests().is_empty());
    }

    #[tokio::test]
    async fn remove_rejects_unselected_duplicate_uri() {
        let client = fake_client([], &retune_spotify::auth::SCOPES);
        let mut cache = cached();
        cache.playlists[0].tracks[1] = cache.playlists[0].tracks[0].clone();

        let error = remove(&client, &mut cache, "playlist", &[0])
            .await
            .unwrap_err();

        assert!(matches!(error, PlaylistMutationError::Other(_)));
        assert!(client.transport().requests().is_empty());
    }

    #[tokio::test]
    async fn stale_remove_refreshes_tracks_and_reports_reload() {
        let client = fake_client(stale_refresh_responses(), &retune_spotify::auth::SCOPES);
        let mut cache = cached();

        let error = remove(&client, &mut cache, "playlist", &[0])
            .await
            .unwrap_err();

        assert!(matches!(error, PlaylistMutationError::Reloaded));
        assert_eq!(cache.playlists[0].snapshot_id, "fresh");
        assert_eq!(cache.playlists[0].tracks, ["spotify:track:fresh"]);
    }

    #[tokio::test]
    async fn block_reorder_preserves_internal_order() {
        let client = fake_client(
            [Response::json(
                200,
                serde_json::json!({"snapshot_id": "reordered"}),
            )],
            &retune_spotify::auth::SCOPES,
        );
        let mut cache = cached();
        cache.playlists[0].track_count = 4;
        cache.playlists[0].tracks = ["1", "2", "3", "4"].map(String::from).to_vec();

        reorder(&client, &mut cache, "playlist", 1, 4, 2)
            .await
            .unwrap();

        assert_eq!(cache.playlists[0].tracks, ["1", "4", "2", "3"]);
    }

    #[test]
    fn forbidden_with_legacy_scopes_maps_to_reconnect_hint() {
        let legacy = Tokens {
            access: String::new(),
            refresh: String::new(),
            expires_at: 0,
            scopes: "user-library-read".into(),
            playback_credentials: None,
        };
        let error = Error::Http {
            endpoint: "/playlists/id/tracks".into(),
            status: 403,
            body: "Insufficient client scope".into(),
        };

        assert_eq!(map_error(error, Some(&legacy)), RECONNECT_HINT);
    }
}

use std::{
    collections::HashMap,
    sync::atomic::{AtomicBool, Ordering},
};

use retune_core::model::Library;
use retune_spotify::tokens::{TokenStore, Tokens};
use serde::Serialize;
use tauri::Manager;

use crate::{
    library_commands::{rating_view, RatingView},
    notify_error, playlists, provider, provider_from,
    spotify_commands::{
        spotify_item_link, spotify_track_destination, SpotifyDestination, SpotifyNavigation,
        SpotifyOpenTarget,
    },
    track_id, AppState, SpotifyProvider,
};

#[cfg(test)]
thread_local! {
    static PROJECTION_LOOKUPS: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PlaylistListView {
    id: String,
    name: String,
    owned: bool,
    owner: Option<String>,
    contains: bool,
    track_count: usize,
    items_available: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PlaylistTrackView {
    id: Option<u64>,
    uri: String,
    name: String,
    art: String,
    alb: String,
    cat: String,
    disc_no: Option<u32>,
    track_no: Option<u32>,
    duration_secs: u64,
    enabled: bool,
    play_count: u32,
    last_played_at: Option<u64>,
    added_at: Option<u64>,
    release_date: Option<String>,
    kind: Option<String>,
    bitrate_kbps: Option<u32>,
    overridden: bool,
    is_local: bool,
    rating: Option<RatingView>,
}

fn playlist_list_views(cache: &playlists::PlaylistCache, uris: &[String]) -> Vec<PlaylistListView> {
    cache
        .playlists
        .iter()
        .map(|playlist| PlaylistListView {
            id: playlist.id.clone(),
            name: playlist.name.clone(),
            owned: playlist.owned,
            owner: playlist.owner.clone(),
            contains: !uris.is_empty() && uris.iter().all(|uri| playlist.tracks.contains(uri)),
            track_count: playlist.track_count,
            items_available: playlist.owned
                || playlist.track_count == 0
                || !playlist.tracks.is_empty(),
        })
        .collect()
}

#[tauri::command(rename_all = "camelCase")]
pub(super) fn playlists_list(
    state: tauri::State<'_, AppState>,
    uris: Option<Vec<String>>,
) -> Result<Vec<PlaylistListView>, String> {
    let uris = uris.unwrap_or_default();
    playlists::validate_playlist_uris(&uris)?;
    Ok(playlist_list_views(&state.playlists.snapshot()?, &uris))
}

#[tauri::command(rename_all = "camelCase")]
pub(super) fn open_spotify_playlist(id: String, target: SpotifyOpenTarget) -> Result<(), String> {
    playlists::validate_playlist_id(&id)?;
    tauri_plugin_opener::open_url(spotify_item_link("playlist", &id, target)?, None::<&str>)
        .map_err(|error| match target {
            SpotifyOpenTarget::App => format!("Could not open the Spotify app: {error}"),
            SpotifyOpenTarget::Web => error.to_string(),
        })
}

#[tauri::command]
pub(super) async fn resolve_spotify_track_destination(
    state: tauri::State<'_, AppState>,
    uri: String,
    destination: SpotifyDestination,
) -> Result<SpotifyNavigation, String> {
    let id = track_id(&uri).ok_or("This track is not from Spotify.")?;
    let track = provider_from(&state)?
        .track(id)
        .await
        .map_err(|error| error.to_string())?;
    spotify_track_destination(&track, destination)
}

#[tauri::command]
pub(super) async fn reorder_playlists(
    app: tauri::AppHandle,
    ids: Vec<String>,
) -> Result<(), String> {
    playlists::validate_playlist_ids(&ids)?;
    let state = app.state::<AppState>();
    let (operation, mut cache) = state.playlists.begin_mutation().await?;
    playlists::reorder_playlists(&mut cache, &ids)?;
    save_playlists(&app, operation, cache, false).await
}

#[tauri::command]
pub(super) async fn playlist_unfollow(app: tauri::AppHandle, id: String) -> Result<(), String> {
    playlists::validate_playlist_id(&id)?;
    let state = app.state::<AppState>();
    let client = provider_from(&state)?;
    let (mut operation, mut cache) = state.playlists.begin_mutation().await?;
    operation.remote_started();
    if let Err(error) = playlists::unfollow(client.as_ref(), &mut cache, &id).await {
        return Err(reconcile_playlist_mutation(
            &app,
            &state,
            client.as_ref(),
            operation,
            &cache,
            error,
        )
        .await);
    }
    operation.remote_resolved();
    save_playlists(&app, operation, cache, true).await
}

#[tauri::command]
pub(super) async fn playlist_tracks(
    app: tauri::AppHandle,
    id: String,
) -> Result<Vec<PlaylistTrackView>, String> {
    playlists::validate_playlist_id(&id)?;
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        playlist_track_views_from_state(&state.playlists, &state.library, &id)
    })
    .await
    .map_err(|error| error.to_string())?
}

pub(super) fn playlist_track_views_from_state(
    playlists: &crate::playlist_state::PlaylistState,
    library: &crate::library_state::LibraryState,
    id: &str,
) -> Result<Vec<PlaylistTrackView>, String> {
    let playlists = playlists.snapshot()?;
    crate::library_commands::project_library(library, |library| {
        playlist_track_views(&playlists, library, id)
    })
}

fn playlist_track_views(
    playlists: &playlists::PlaylistCache,
    library: &Library,
    id: &str,
) -> Result<Vec<PlaylistTrackView>, String> {
    let playlist = playlists
        .playlists
        .iter()
        .find(|playlist| playlist.id == id)
        .ok_or_else(|| format!("Unknown playlist {id}"))?;
    let mut cached_by_uri = HashMap::with_capacity(playlist.spotify_tracks.len());
    for track in &playlist.spotify_tracks {
        cached_by_uri.entry(track.uri.as_str()).or_insert(track);
    }
    let library_by_uri = library
        .tracks()
        .iter()
        .map(|track| (track.uri.as_str(), track))
        .collect::<HashMap<_, _>>();
    Ok(playlist
        .tracks
        .iter()
        .map(|uri| {
            #[cfg(test)]
            PROJECTION_LOOKUPS.set(PROJECTION_LOOKUPS.get().map(|lookups| lookups + 2));
            let cached = cached_by_uri.get(uri.as_str()).copied();
            if let Some(track) = library_by_uri.get(uri.as_str()).copied() {
                PlaylistTrackView {
                    id: Some(track.id.0),
                    uri: track.uri.clone(),
                    name: track.name.clone(),
                    art: track.art.clone(),
                    alb: track.alb.clone(),
                    cat: track.cat.clone(),
                    disc_no: track
                        .disc_no
                        .or_else(|| cached.and_then(|track| track.disc_no)),
                    track_no: track
                        .track_no
                        .or_else(|| cached.and_then(|track| track.track_no)),
                    duration_secs: track.duration.as_secs(),
                    enabled: track.enabled,
                    play_count: track.play_count,
                    last_played_at: track.last_played_at,
                    added_at: track.added_at,
                    release_date: track
                        .release_date
                        .clone()
                        .or_else(|| cached.and_then(|track| track.release_date.clone())),
                    kind: track.kind.clone(),
                    bitrate_kbps: track.bitrate_kbps,
                    overridden: track
                        .orig_cat
                        .as_ref()
                        .is_some_and(|original| original != &track.cat),
                    is_local: false,
                    rating: library.effective_rating(track.id).map(rating_view),
                }
            } else {
                PlaylistTrackView {
                    id: None,
                    uri: uri.clone(),
                    name: cached.map(|track| track.name.clone()).unwrap_or_default(),
                    art: cached.map(|track| track.art.clone()).unwrap_or_default(),
                    alb: cached.map(|track| track.alb.clone()).unwrap_or_default(),
                    cat: String::new(),
                    disc_no: cached.and_then(|track| track.disc_no),
                    track_no: cached.and_then(|track| track.track_no),
                    duration_secs: cached.map_or(0, |track| track.duration / 1000),
                    enabled: true,
                    play_count: 0,
                    last_played_at: None,
                    added_at: None,
                    release_date: cached.and_then(|track| track.release_date.clone()),
                    kind: None,
                    bitrate_kbps: None,
                    overridden: false,
                    is_local: false,
                    rating: None,
                }
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use retune_core::model::NewTrack;

    use super::*;

    #[test]
    fn playlist_view_fills_missing_library_fields_from_spotify() {
        let mut library = Library::new();
        let id = library.add(NewTrack {
            uri: "spotify:track:one".into(),
            name: "Song".into(),
            duration: Duration::from_secs(180),
            ..NewTrack::default()
        });
        let playlists = playlists::PlaylistCache {
            playlists: vec![playlists::CachedPlaylist {
                id: "playlist".into(),
                name: "Playlist".into(),
                snapshot_id: "snapshot".into(),
                owned: true,
                owner: None,
                track_count: 1,
                tracks: vec!["spotify:track:one".into()],
                track_metadata_version: playlists::TRACK_METADATA_VERSION,
                spotify_tracks: vec![playlists::CachedTrack {
                    uri: "spotify:track:one".into(),
                    name: "Song".into(),
                    art: "Artist".into(),
                    alb: "Album".into(),
                    duration: 180_000,
                    disc_no: Some(2),
                    track_no: Some(7),
                    release_date: Some("1999".into()),
                }],
            }],
        };

        let tracks = playlist_track_views(&playlists, &library, "playlist").unwrap();

        assert_eq!(tracks[0].id, Some(id.0));
        assert_eq!((tracks[0].disc_no, tracks[0].track_no), (Some(2), Some(7)));
        assert_eq!(tracks[0].release_date.as_deref(), Some("1999"));
    }

    #[test]
    fn playlist_projection_does_two_index_lookups_per_entry() {
        let mut library = Library::new();
        library.add_all((0..50_000).map(|index| NewTrack {
            uri: format!("spotify:track:{index}"),
            ..NewTrack::default()
        }));
        let playlists = playlists::PlaylistCache {
            playlists: vec![playlists::CachedPlaylist {
                id: "playlist".into(),
                name: "Playlist".into(),
                snapshot_id: "snapshot".into(),
                owned: true,
                owner: None,
                track_count: 10_000,
                tracks: vec!["spotify:track:42".into(); 10_000],
                track_metadata_version: playlists::TRACK_METADATA_VERSION,
                spotify_tracks: vec![playlists::CachedTrack {
                    uri: "spotify:track:42".into(),
                    name: "Song".into(),
                    art: "Artist".into(),
                    alb: "Album".into(),
                    duration: 1,
                    disc_no: None,
                    track_no: None,
                    release_date: None,
                }],
            }],
        };

        PROJECTION_LOOKUPS.set(Some(0));
        let tracks = playlist_track_views(&playlists, &library, "playlist").unwrap();
        let lookups = PROJECTION_LOOKUPS.replace(None).unwrap();

        assert_eq!(tracks.len(), 10_000);
        assert_eq!(lookups, 20_000);
    }

    #[test]
    fn playlist_membership_requires_all_nonempty_uris() {
        let mut cache = playlists::PlaylistCache {
            playlists: vec![playlists::CachedPlaylist {
                id: "playlist".into(),
                name: "Playlist".into(),
                snapshot_id: "snapshot".into(),
                owned: true,
                owner: None,
                track_count: 2,
                tracks: vec!["one".into(), "two".into()],
                track_metadata_version: playlists::TRACK_METADATA_VERSION,
                spotify_tracks: vec![],
            }],
        };

        assert!(playlist_list_views(&cache, &["one".into(), "two".into()])[0].contains);
        assert!(!playlist_list_views(&cache, &["one".into(), "missing".into()])[0].contains);
        assert!(!playlist_list_views(&cache, &["missing".into()])[0].contains);
        assert!(!playlist_list_views(&cache, &[])[0].contains);
        assert!(playlist_list_views(&cache, &[])[0].items_available);
        cache.playlists[0].owned = false;
        cache.playlists[0].tracks.clear();
        assert!(!playlist_list_views(&cache, &[])[0].items_available);
    }

    #[test]
    fn playlist_reconnect_hint_is_dispatched_once() {
        let notified = AtomicBool::new(false);
        let mut messages = vec![];
        let legacy = Tokens {
            access: String::new(),
            refresh: String::new(),
            expires_at: 0,
            scopes: "user-library-read".into(),
            playback_credentials: None,
        };
        let forbidden = || retune_spotify::Error::Http {
            endpoint: "/playlists/id/tracks".into(),
            status: 403,
            body: "Insufficient client scope".into(),
        };

        assert_eq!(
            dispatch_playlist_error(&notified, forbidden(), Some(&legacy), |message| {
                messages.push(message)
            }),
            None
        );
        assert_eq!(
            dispatch_playlist_error(&notified, forbidden(), Some(&legacy), |message| {
                messages.push(message)
            }),
            None
        );
        assert_eq!(messages, [playlists::RECONNECT_HINT]);
        let unrelated = retune_spotify::Error::Http {
            endpoint: "/playlists/id/tracks".into(),
            status: 500,
            body: playlists::RECONNECT_HINT.into(),
        };
        let expected = unrelated.to_string();
        assert_eq!(
            dispatch_playlist_error(&notified, unrelated, Some(&legacy), |_| panic!()),
            Some(expected)
        );
    }
}

pub(super) async fn sync_playlists(
    app: &tauri::AppHandle,
    client: &SpotifyProvider,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let session_revision = state.spotify_session.revision();
    let (operation, current) = state.playlists.begin_sync().await?;
    let sync_result = playlists::sync(client, &current).await;
    let _session_commit = state
        .spotify_session
        .commit_revision(session_revision)
        .await?;
    let synced = match sync_result {
        Ok(synced) => synced,
        Err(error) => {
            let tokens = state.token_store.load().ok().flatten();
            if let Some(error) = dispatch_playlist_error(
                &state.playlist_reauth_notified,
                error,
                tokens.as_ref(),
                |error| notify_error(app, error),
            ) {
                return Err(error);
            }
            return Ok(());
        }
    };
    save_playlists(app, operation, synced, false).await
}

fn playlist_error(state: &AppState, error: retune_spotify::Error) -> String {
    let tokens = state.token_store.load().ok().flatten();
    match playlists::classify_error(error, tokens.as_ref()) {
        playlists::PlaylistFailure::ReconnectRequired => playlists::RECONNECT_HINT.into(),
        playlists::PlaylistFailure::Spotify(error) => error.to_string(),
    }
}

async fn reconcile_playlist_mutation<T: retune_spotify::client::Transport, S: TokenStore>(
    app: &tauri::AppHandle,
    state: &AppState,
    client: &retune_spotify::client::SpotifyClient<T, S>,
    mut operation: crate::playlist_state::PlaylistOperation,
    cache: &playlists::PlaylistCache,
    error: retune_spotify::Error,
) -> String {
    let ambiguous = matches!(&error, retune_spotify::Error::AmbiguousMutation { .. });
    let message = playlist_error(state, error);
    if ambiguous {
        match playlists::sync(client, cache).await {
            Ok(reconciled) => {
                if let Err(error) = save_playlists(app, operation, reconciled, true).await {
                    return format!("{message} Reconciliation failed: {error}");
                }
            }
            Err(error) => return format!("{message} Reconciliation failed: {error}"),
        }
    } else {
        operation.remote_resolved();
    }
    message
}

async fn playlist_add_inner(
    app: &tauri::AppHandle,
    id: String,
    uris: Vec<String>,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let (mut operation, mut cache) = state.playlists.begin_mutation().await?;
    let library = state
        .library
        .lock()
        .expect("library mutex poisoned")
        .clone();
    let client = provider_from(&state)?;
    operation.remote_started();
    if let Err(error) = playlists::add(client.as_ref(), &mut cache, &library, &id, uris).await {
        return Err(match error {
            playlists::PlaylistAddError::Local(message) => {
                operation.remote_resolved();
                message
            }
            playlists::PlaylistAddError::Unknown(id) => {
                operation.remote_resolved();
                format!("Unknown playlist {id}")
            }
            playlists::PlaylistAddError::ReadOnly => {
                operation.remote_resolved();
                "Only your playlists can be changed.".into()
            }
            playlists::PlaylistAddError::Spotify(error) => {
                reconcile_playlist_mutation(app, &state, client.as_ref(), operation, &cache, error)
                    .await
            }
        });
    }
    operation.remote_resolved();
    save_playlists(app, operation, cache, true).await
}

async fn save_playlists(
    app: &tauri::AppHandle,
    operation: crate::playlist_state::PlaylistOperation,
    cache: playlists::PlaylistCache,
    invalidate_on_failure: bool,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let _operation = state
        .playlists
        .commit(operation, cache, invalidate_on_failure)
        .await?;
    crate::emit_main(app, "playlists-changed", ()).map_err(|error| error.to_string())
}

fn dispatch_playlist_error(
    notified: &AtomicBool,
    error: retune_spotify::Error,
    tokens: Option<&Tokens>,
    notify: impl FnOnce(String),
) -> Option<String> {
    match playlists::classify_error(error, tokens) {
        playlists::PlaylistFailure::Spotify(error) => Some(error.to_string()),
        playlists::PlaylistFailure::ReconnectRequired => {
            if !notified.swap(true, Ordering::Relaxed) {
                notify(playlists::RECONNECT_HINT.into());
            }
            None
        }
    }
}

#[tauri::command]
pub(super) async fn playlist_create(
    app: tauri::AppHandle,
    name: String,
) -> Result<PlaylistListView, String> {
    playlists::validate_playlist_name(&name)?;
    let state = app.state::<AppState>();
    let name = name.trim();
    if name.is_empty() {
        return Err("Playlist name is required.".into());
    }
    let (mut operation, mut cache) = state.playlists.begin_mutation().await?;
    let client = provider_from(&state)?;
    operation.remote_started();
    if let Err(error) = playlists::create(client.as_ref(), &mut cache, name).await {
        return Err(reconcile_playlist_mutation(
            &app,
            &state,
            client.as_ref(),
            operation,
            &cache,
            error,
        )
        .await);
    }
    operation.remote_resolved();
    let created = playlist_list_views(&cache, &[])
        .pop()
        .expect("create inserted playlist");
    save_playlists(&app, operation, cache, true).await?;
    Ok(created)
}

#[tauri::command]
pub(super) async fn playlist_add(
    app: tauri::AppHandle,
    id: String,
    uris: Vec<String>,
) -> Result<(), String> {
    playlists::validate_playlist_id(&id)?;
    playlists::validate_playlist_uris(&uris)?;
    playlist_add_inner(&app, id, uris).await
}

#[tauri::command(rename_all = "camelCase")]
pub(super) async fn playlist_add_album(
    app: tauri::AppHandle,
    id: String,
    album_uri: String,
    album_label: Option<String>,
) -> Result<(), String> {
    playlists::validate_playlist_id(&id)?;
    playlists::validate_playlist_uris(std::slice::from_ref(&album_uri))?;
    playlists::reject_local_uris(std::slice::from_ref(&album_uri), |_| album_label.clone())?;
    let provider = provider_from(&app.state::<AppState>())?;
    let tracks = provider::album_tracks(provider.as_ref(), &album_uri).await?;
    let uris = tracks.into_iter().map(|track| track.uri).collect();
    playlist_add_inner(&app, id, uris).await
}

#[tauri::command(rename_all = "camelCase")]
pub(super) async fn playlist_reorder(
    app: tauri::AppHandle,
    id: String,
    range_start: u32,
    insert_before: u32,
    range_length: u32,
) -> Result<(), String> {
    playlists::validate_playlist_id(&id)?;
    let state = app.state::<AppState>();
    let client = provider_from(&state)?;
    let (mut operation, mut cache) = state.playlists.begin_mutation().await?;
    operation.remote_started();
    let result = playlists::reorder(
        client.as_ref(),
        &mut cache,
        &id,
        range_start,
        insert_before,
        range_length,
    )
    .await;
    finish_playlist_mutation(&app, &state, operation, cache, result).await
}

async fn finish_playlist_mutation(
    app: &tauri::AppHandle,
    state: &AppState,
    mut operation: crate::playlist_state::PlaylistOperation,
    cache: playlists::PlaylistCache,
    result: Result<(), playlists::PlaylistMutationError>,
) -> Result<(), String> {
    match result {
        Ok(()) => {
            operation.remote_resolved();
            save_playlists(app, operation, cache, true).await
        }
        Err(playlists::PlaylistMutationError::Reloaded) => {
            operation.remote_resolved();
            save_playlists(app, operation, cache, true).await?;
            Err(playlists::STALE_PLAYLIST.into())
        }
        Err(playlists::PlaylistMutationError::Spotify(error)) => {
            operation.remote_resolved();
            Err(playlist_error(state, error))
        }
        Err(playlists::PlaylistMutationError::Other(error)) => {
            operation.remote_resolved();
            Err(error)
        }
    }
}

#[tauri::command]
pub(super) async fn playlist_remove(
    app: tauri::AppHandle,
    id: String,
    indices: Vec<u32>,
) -> Result<(), String> {
    playlists::validate_playlist_id(&id)?;
    playlists::validate_playlist_indices(&indices)?;
    let state = app.state::<AppState>();
    let client = provider_from(&state)?;
    let (mut operation, mut cache) = state.playlists.begin_mutation().await?;
    operation.remote_started();
    let result = playlists::remove(client.as_ref(), &mut cache, &id, &indices).await;
    finish_playlist_mutation(&app, &state, operation, cache, result).await
}

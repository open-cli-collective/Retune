use retune_core::model::{Library, TrackRecord};
use retune_spotify::{catalog::SpotifyCatalog, client::Track as SpotifyTrack};
use serde::Deserialize;

use crate::{playback::SnapshotTrack, playlists::PlaylistCache};

pub(crate) const MAX_PLAYBACK_QUEUE: usize = 100_000;
const MAX_RESOURCE_URI_BYTES: usize = 2_048;

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct PlaybackResource {
    pub id: u64,
    pub uri: String,
}

pub(crate) fn resolve_cached(
    resources: &[PlaybackResource],
    start_index: usize,
    library: &Library,
    playlists: &PlaylistCache,
    catalog: &SpotifyCatalog,
) -> Result<Vec<Option<SnapshotTrack>>, String> {
    validate_request(resources, start_index)?;
    resources
        .iter()
        .map(|resource| resolve_one(resource, library, playlists, catalog))
        .collect()
}

pub(crate) fn finish(
    tracks: Vec<SnapshotTrack>,
    start_index: usize,
    enabled: impl Fn(&SnapshotTrack) -> bool,
) -> Result<(Vec<SnapshotTrack>, usize), String> {
    if tracks.get(start_index).is_none() {
        return Err("Playback start index is out of range.".into());
    }
    let mut resolved_index = 0;
    let tracks = tracks
        .into_iter()
        .enumerate()
        .filter_map(|(index, track)| {
            if index < start_index && enabled(&track) {
                resolved_index += 1;
            }
            (index == start_index || enabled(&track)).then_some(track)
        })
        .collect();
    Ok((tracks, resolved_index))
}

pub(crate) fn from_spotify(resource: &PlaybackResource, track: &SpotifyTrack) -> SnapshotTrack {
    SnapshotTrack {
        id: resource.id,
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
        duration_secs: track.duration_ms.unwrap_or_default() / 1_000,
    }
}

fn validate_request(resources: &[PlaybackResource], start_index: usize) -> Result<(), String> {
    if resources.is_empty() {
        return Err("Playback queue is empty.".into());
    }
    if resources.len() > MAX_PLAYBACK_QUEUE {
        return Err(format!(
            "Playback queue cannot contain more than {MAX_PLAYBACK_QUEUE} tracks."
        ));
    }
    if start_index >= resources.len() {
        return Err("Playback start index is out of range.".into());
    }
    for resource in resources {
        if resource.uri.len() > MAX_RESOURCE_URI_BYTES {
            return Err("Playback resource URI is too long.".into());
        }
        validate_uri(&resource.uri)?;
    }
    Ok(())
}

fn validate_uri(uri: &str) -> Result<(), String> {
    if uri.starts_with("file://") {
        return Ok(());
    }
    let mut parts = uri.split(':');
    let valid_id = |id: &str| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_alphanumeric());
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("spotify"), Some("track" | "episode"), Some(id), None) if valid_id(id) => Ok(()),
        (Some("spotify"), Some("chapter"), Some(id), None) if valid_id(id) => {
            Err("Audiobook playback isn't supported yet.".into())
        }
        _ => Err("Playback resource URI is invalid or unsupported.".into()),
    }
}

fn resolve_one(
    resource: &PlaybackResource,
    library: &Library,
    playlists: &PlaylistCache,
    catalog: &SpotifyCatalog,
) -> Result<Option<SnapshotTrack>, String> {
    if let Some(track) = library
        .tracks()
        .iter()
        .find(|track| track.uri == resource.uri)
    {
        return Ok(Some(from_library(track)));
    }
    if resource.uri.starts_with("file://") {
        return Err("Local playback resource is not in the library.".into());
    }
    if let Some(track) = playlists
        .playlists
        .iter()
        .flat_map(|playlist| &playlist.spotify_tracks)
        .find(|track| track.uri == resource.uri)
    {
        return Ok(Some(SnapshotTrack {
            id: resource.id,
            uri: track.uri.clone(),
            name: track.name.clone(),
            art: track.art.clone(),
            alb: track.alb.clone(),
            duration_secs: track.duration / 1_000,
        }));
    }
    Ok(catalog
        .complete_track(&resource.uri)
        .map(|track| from_spotify(resource, &track)))
}

fn from_library(track: &TrackRecord) -> SnapshotTrack {
    SnapshotTrack {
        id: track.id.0,
        uri: track.uri.clone(),
        name: track.name.clone(),
        art: track.art.clone(),
        alb: track.alb.clone(),
        duration_secs: track.duration.as_secs(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use retune_core::model::{NewTrack, SourceId};

    use super::*;

    fn add(library: &mut Library, uri: &str, name: &str) -> u64 {
        library
            .add(NewTrack {
                uri: uri.into(),
                source: SourceId::Music,
                name: name.into(),
                art: "canonical artist".into(),
                alb: "canonical album".into(),
                duration: Duration::from_secs(42),
                ..NewTrack::default()
            })
            .0
    }

    fn resource(id: u64, uri: &str) -> PlaybackResource {
        PlaybackResource {
            id,
            uri: uri.into(),
        }
    }

    #[test]
    fn local_resources_require_exact_membership_and_use_canonical_metadata() {
        let mut library = Library::new();
        let id = add(&mut library, "file:///music/song.flac", "canonical name");
        let resolved = resolve_cached(
            &[resource(999, "file:///music/song.flac")],
            0,
            &library,
            &PlaylistCache::default(),
            &SpotifyCatalog::default(),
        )
        .unwrap()
        .pop()
        .flatten()
        .unwrap();
        assert_eq!(
            (resolved.id, resolved.name.as_str()),
            (id, "canonical name")
        );
        assert!(resolve_cached(
            &[resource(1, "file:///etc/passwd")],
            0,
            &library,
            &PlaylistCache::default(),
            &SpotifyCatalog::default(),
        )
        .is_err());
    }

    #[test]
    fn queue_validation_preserves_duplicates_and_rejects_bad_kinds_and_bounds() {
        let mut library = Library::new();
        add(&mut library, "spotify:track:abc123", "one");
        let resources = vec![
            resource(1, "spotify:track:abc123"),
            resource(2, "spotify:track:abc123"),
        ];
        assert_eq!(
            resolve_cached(
                &resources,
                1,
                &library,
                &PlaylistCache::default(),
                &SpotifyCatalog::default(),
            )
            .unwrap()
            .len(),
            2
        );
        assert!(resolve_cached(
            &[resource(1, "spotify:chapter:abc")],
            0,
            &library,
            &PlaylistCache::default(),
            &SpotifyCatalog::default(),
        )
        .unwrap_err()
        .contains("Audiobook"));
        assert!(resolve_cached(
            &[resource(1, "spotify:album:abc")],
            0,
            &library,
            &PlaylistCache::default(),
            &SpotifyCatalog::default(),
        )
        .is_err());
        assert_eq!(
            resolve_cached(
                &vec![resource(1, "spotify:track:abc123"); 1_001],
                0,
                &library,
                &PlaylistCache::default(),
                &SpotifyCatalog::default(),
            )
            .unwrap()
            .len(),
            1_001
        );
        assert!(resolve_cached(
            &vec![resource(1, "spotify:track:abc"); MAX_PLAYBACK_QUEUE + 1],
            0,
            &library,
            &PlaylistCache::default(),
            &SpotifyCatalog::default(),
        )
        .is_err());
        assert!(resolve_cached(
            &resources,
            resources.len(),
            &library,
            &PlaylistCache::default(),
            &SpotifyCatalog::default(),
        )
        .is_err());
    }

    #[test]
    fn finish_keeps_selected_disabled_track_and_filters_other_disabled_tracks() {
        let tracks = [1, 2, 3]
            .map(|id| SnapshotTrack {
                id,
                uri: format!("spotify:track:id{id}"),
                name: String::new(),
                art: String::new(),
                alb: String::new(),
                duration_secs: 1,
            })
            .to_vec();
        let (tracks, index) = finish(tracks, 1, |track| track.id == 3).unwrap();
        assert_eq!(
            tracks.iter().map(|track| track.id).collect::<Vec<_>>(),
            [2, 3]
        );
        assert_eq!(index, 0);
    }
}

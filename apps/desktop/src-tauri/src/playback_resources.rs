use std::collections::{HashMap, HashSet};

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
) -> Result<(Vec<Option<SnapshotTrack>>, Vec<bool>), String> {
    validate_request(resources, start_index)?;
    let requested = resources
        .iter()
        .map(|resource| resource.uri.as_str())
        .collect::<HashSet<_>>();
    let mut library_by_uri = HashMap::with_capacity(requested.len());
    for track in library.tracks() {
        if requested.contains(track.uri.as_str()) {
            library_by_uri.entry(track.uri.as_str()).or_insert(track);
            if library_by_uri.len() == requested.len() {
                break;
            }
        }
    }
    let mut unresolved = HashSet::new();
    let mut tracks = Vec::with_capacity(resources.len());
    let mut enabled = Vec::with_capacity(resources.len());
    for resource in resources {
        if let Some(track) = library_by_uri.get(resource.uri.as_str()).copied() {
            tracks.push(Some(from_library(track)));
            enabled.push(track.enabled);
        } else if resource.uri.starts_with("file://") {
            return Err("Local playback resource is not in the library.".into());
        } else {
            unresolved.insert(resource.uri.as_str());
            tracks.push(None);
            enabled.push(true);
        }
    }
    let mut playlist_by_uri = HashMap::new();
    if !unresolved.is_empty() {
        for track in playlists
            .playlists
            .iter()
            .flat_map(|playlist| &playlist.spotify_tracks)
            .filter(|track| unresolved.contains(track.uri.as_str()))
        {
            playlist_by_uri.entry(track.uri.as_str()).or_insert(track);
            if playlist_by_uri.len() == unresolved.len() {
                break;
            }
        }
    }
    for (resource, resolved) in resources.iter().zip(&mut tracks) {
        if resolved.is_some() {
            continue;
        }
        *resolved = playlist_by_uri
            .get(resource.uri.as_str())
            .map(|track| SnapshotTrack {
                id: resource.id,
                uri: track.uri.clone(),
                name: track.name.clone(),
                art: track.art.clone(),
                alb: track.alb.clone(),
                duration_secs: track.duration / 1_000,
            })
            .or_else(|| {
                catalog
                    .complete_track(&resource.uri)
                    .map(|track| from_spotify(resource, &track))
            });
    }
    Ok((tracks, enabled))
}

pub(crate) fn finish(
    tracks: Vec<SnapshotTrack>,
    start_index: usize,
    enabled: &[bool],
) -> Result<(Vec<SnapshotTrack>, usize), String> {
    if tracks.get(start_index).is_none() || enabled.len() != tracks.len() {
        return Err("Playback start index is out of range.".into());
    }
    let mut resolved_index = 0;
    let tracks = tracks
        .into_iter()
        .enumerate()
        .filter_map(|(index, track)| {
            if index < start_index && enabled[index] {
                resolved_index += 1;
            }
            (index == start_index || enabled[index]).then_some(track)
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
    use crate::playlists::{CachedPlaylist, CachedTrack, TRACK_METADATA_VERSION};

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
        .0
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
            .0
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
            .0
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
    fn cached_resolution_uses_the_first_playlist_match() {
        let playlist = |id: &str, name: &str| CachedPlaylist {
            id: id.into(),
            name: id.into(),
            snapshot_id: id.into(),
            owned: true,
            owner: None,
            track_count: 1,
            tracks: vec!["spotify:track:same".into()],
            track_metadata_version: TRACK_METADATA_VERSION,
            spotify_tracks: vec![CachedTrack {
                uri: "spotify:track:same".into(),
                name: name.into(),
                art: String::new(),
                alb: String::new(),
                duration: 1_000,
                disc_no: None,
                track_no: None,
                release_date: None,
            }],
        };
        let playlists = PlaylistCache {
            playlists: vec![playlist("first", "First"), playlist("second", "Second")],
        };

        let resolved = resolve_cached(
            &[resource(1, "spotify:track:same")],
            0,
            &Library::new(),
            &playlists,
            &SpotifyCatalog::default(),
        )
        .unwrap()
        .0;

        assert_eq!(resolved[0].as_ref().unwrap().name, "First");
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
        let (tracks, index) = finish(tracks, 1, &[false, false, true]).unwrap();
        assert_eq!(
            tracks.iter().map(|track| track.id).collect::<Vec<_>>(),
            [2, 3]
        );
        assert_eq!(index, 0);
    }

    #[test]
    #[ignore = "responsiveness benchmark; run with --release --ignored --nocapture"]
    fn audit_playback_resolution_costs() {
        use std::{hint::black_box, time::Instant};

        let mut library = Library::new();
        library.add_all((0..50_000).map(|index| NewTrack {
            uri: format!("spotify:track:library{index}"),
            name: format!("Library {index}"),
            ..NewTrack::default()
        }));
        let playlist_tracks = (0..50_000)
            .map(|index| CachedTrack {
                uri: format!("spotify:track:playlist{index}"),
                name: format!("Playlist {index}"),
                art: String::new(),
                alb: String::new(),
                duration: 1_000,
                disc_no: None,
                track_no: None,
                release_date: None,
            })
            .collect::<Vec<_>>();
        let playlists = PlaylistCache {
            playlists: vec![CachedPlaylist {
                id: "large".into(),
                name: "Large".into(),
                snapshot_id: "large".into(),
                owned: true,
                owner: None,
                track_count: playlist_tracks.len(),
                tracks: playlist_tracks
                    .iter()
                    .map(|track| track.uri.clone())
                    .collect(),
                track_metadata_version: TRACK_METADATA_VERSION,
                spotify_tracks: playlist_tracks,
            }],
        };
        let catalog = SpotifyCatalog::default();
        let cases = [
            (
                "one-library-large-caches",
                vec![resource(0, "spotify:track:library0")],
            ),
            (
                "library-50000",
                (0..50_000)
                    .map(|index| resource(index, &format!("spotify:track:library{index}")))
                    .collect(),
            ),
            (
                "mixed-5000",
                (0..2_500)
                    .flat_map(|index| {
                        [
                            resource(index, &format!("spotify:track:library{index}")),
                            resource(index, &format!("spotify:track:playlist{index}")),
                        ]
                    })
                    .collect(),
            ),
        ];
        for (name, resources) in cases {
            let run = || {
                let (tracks, enabled) =
                    resolve_cached(&resources, 0, &library, &playlists, &catalog).unwrap();
                let tracks = tracks.into_iter().collect::<Option<Vec<_>>>().unwrap();
                finish(tracks, 0, &enabled).unwrap().0
            };
            let expected = resources
                .iter()
                .map(|resource| resource.uri.as_str())
                .collect::<Vec<_>>();
            let actual = run();
            assert_eq!(
                actual
                    .iter()
                    .map(|track| track.uri.as_str())
                    .collect::<Vec<_>>(),
                expected
            );
            let mut samples = Vec::with_capacity(7);
            for _ in 0..7 {
                let start = Instant::now();
                black_box(run());
                samples.push(start.elapsed().as_secs_f64() * 1_000.0);
            }
            samples.sort_by(f64::total_cmp);
            println!(
                "PLAYBACK fixture=responsiveness-v1 case={name} samples=7 median_ms={:.3} min={:.3} max={:.3}",
                samples[3], samples[0], samples[6]
            );
        }
    }
}

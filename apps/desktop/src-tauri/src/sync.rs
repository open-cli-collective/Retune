use std::collections::{HashMap, HashSet};

use retune_core::{
    io::{export_json, import},
    model::Library,
};

use crate::{
    provider::{MediaProvider, SectionProgress, SyncBatch},
    spotify_track_match,
    store::{OverlayStore, SpotifyLibraryState},
};

#[cfg(test)]
pub async fn reconcile<P: MediaProvider, S: OverlayStore>(
    provider: &P,
    library: &mut Library,
    store: &S,
    first_sync: bool,
    mut progress: impl FnMut(&str),
) -> Result<(), String> {
    let outcome = snapshot(provider, &mut progress, &|_| {}).await?;
    apply(library, store, first_sync, outcome.tracks, None)
}

pub struct SnapshotOutcome {
    pub tracks: Vec<retune_core::model::NewTrack>,
    pub genres_degraded: bool,
    pub partial: bool,
    pub quota_exhausted: bool,
    pub progress: Vec<SectionProgress>,
    pub earliest_cooldown: Option<u64>,
    pub request_counts: std::collections::BTreeMap<String, u64>,
    pub spotify_library: Option<SpotifyLibraryState>,
}

pub async fn snapshot<P: MediaProvider>(
    provider: &P,
    mut progress: impl FnMut(&str),
    on_batch: &(dyn Fn(SyncBatch) + Send + Sync),
) -> Result<SnapshotOutcome, String> {
    let mut incoming = vec![];
    let mut genres_degraded = false;
    let mut partial = false;
    let mut quota_exhausted = false;
    let mut section_progress = vec![];
    let mut account_id = None;
    let mut saved_tracks = None;
    let mut saved_albums = None;
    for kind in crate::provider::LibraryKind::ALL {
        progress(kind.phase());
        let snapshot = provider.library_snapshot(kind, on_batch).await?;
        genres_degraded |= snapshot.genres_degraded;
        partial |= snapshot.partial;
        quota_exhausted |= snapshot.quota_exhausted;
        if let Some(progress) = snapshot.progress {
            section_progress.push(progress);
        }
        if account_id.is_none() {
            account_id = snapshot.account_id;
        }
        if snapshot.saved_tracks.is_some() {
            saved_tracks = snapshot.saved_tracks;
        }
        if snapshot.saved_albums.is_some() {
            saved_albums = snapshot.saved_albums;
        }
        for batch in snapshot.batches {
            incoming.extend(batch);
        }
    }
    let spotify_library = if partial {
        None
    } else {
        match (account_id, saved_tracks, saved_albums) {
            (Some(account_id), Some(saved_tracks), Some(saved_albums)) => {
                Some(SpotifyLibraryState {
                    account_id,
                    complete: true,
                    saved_tracks,
                    saved_albums,
                })
            }
            _ => None,
        }
    };
    Ok(SnapshotOutcome {
        tracks: incoming,
        genres_degraded,
        partial,
        quota_exhausted,
        progress: section_progress,
        earliest_cooldown: provider.earliest_cooldown(),
        request_counts: provider.request_counts(),
        spotify_library,
    })
}

pub fn apply<S: OverlayStore>(
    library: &mut Library,
    store: &S,
    first_sync: bool,
    incoming: Vec<retune_core::model::NewTrack>,
    spotify_library: Option<&SpotifyLibraryState>,
) -> Result<(), String> {
    if first_sync {
        *library = without_fixtures(library)?;
    }
    let aliases = apply_in_memory_with_aliases(library, incoming);
    if let Some(spotify_library) = spotify_library {
        prune_unreferenced_spotify_music_with_aliases(library, spotify_library, &aliases);
    }
    store.save(library).map_err(|error| error.to_string())
}

pub fn prune_unreferenced_spotify_music_with_aliases(
    library: &mut Library,
    spotify_library: &SpotifyLibraryState,
    aliases: &HashMap<String, String>,
) -> usize {
    if !spotify_library.is_exact() {
        return 0;
    }
    let referenced = referenced_local_uris(spotify_library, aliases);
    let uris = library
        .tracks()
        .iter()
        .filter(|track| {
            track.source == retune_core::model::SourceId::Music
                && track.uri.starts_with("spotify:track:")
                && !referenced.contains(&track.uri)
        })
        .map(|track| track.uri.clone())
        .collect::<Vec<_>>();
    library.remove_uris(&uris)
}

#[cfg(test)]
pub fn prune_unreferenced_spotify_tracks(
    library: &mut Library,
    spotify_library: &SpotifyLibraryState,
    candidates: &[String],
) -> usize {
    prune_unreferenced_spotify_tracks_with_aliases(
        library,
        spotify_library,
        candidates,
        &HashMap::new(),
    )
}

pub fn prune_unreferenced_spotify_tracks_with_aliases(
    library: &mut Library,
    spotify_library: &SpotifyLibraryState,
    candidates: &[String],
    aliases: &HashMap<String, String>,
) -> usize {
    if !spotify_library.is_exact() {
        return 0;
    }
    let referenced = referenced_local_uris(spotify_library, aliases);
    let mut seen = HashSet::new();
    let uris = candidates
        .iter()
        .map(|uri| aliases.get(uri).unwrap_or(uri))
        .filter(|uri| seen.insert(uri.as_str()))
        .filter(|uri| !referenced.contains(*uri))
        .cloned()
        .collect::<Vec<_>>();
    library.remove_uris(&uris)
}

pub fn apply_in_memory(library: &mut Library, incoming: Vec<retune_core::model::NewTrack>) {
    let _ = apply_in_memory_with_aliases(library, incoming);
}

pub fn spotify_track_aliases(
    library: &Library,
    incoming: &[retune_core::model::NewTrack],
) -> HashMap<String, String> {
    incoming
        .iter()
        .filter_map(|track| {
            spotify_track_match(library, track)
                .map(|existing| (track.uri.clone(), existing.uri.clone()))
        })
        .collect()
}

fn referenced_local_uris(
    spotify_library: &SpotifyLibraryState,
    aliases: &HashMap<String, String>,
) -> HashSet<String> {
    let remote_uris = spotify_library.saved_tracks.keys().cloned().chain(
        spotify_library
            .saved_albums
            .values()
            .flat_map(|album| album.track_uris.iter().cloned()),
    );
    let mut referenced = HashSet::new();
    for remote_uri in remote_uris {
        referenced.insert(remote_uri.clone());
        if let Some(local_uri) = aliases.get(&remote_uri) {
            referenced.insert(local_uri.clone());
        }
    }
    referenced
}

fn apply_in_memory_with_aliases(
    library: &mut Library,
    incoming: Vec<retune_core::model::NewTrack>,
) -> HashMap<String, String> {
    let mut aliases = HashMap::new();
    let provider_genres = library
        .tracks()
        .iter()
        .map(|track| {
            (
                track.uri.clone(),
                track.orig_cat.clone().unwrap_or_else(|| track.cat.clone()),
            )
        })
        .collect::<HashMap<_, _>>();
    for mut track in incoming {
        if track.cat == retune_spotify::normalize::UNCATEGORIZED {
            if let Some(existing) = provider_genres
                .get(&track.uri)
                .filter(|genre| genre.as_str() != retune_spotify::normalize::UNCATEGORIZED)
            {
                track.cat.clone_from(existing);
            }
        }
        let incoming_uri = track.uri.clone();
        let existing_uri =
            spotify_track_match(library, &track).map(|existing| existing.uri.clone());
        aliases.insert(
            incoming_uri.clone(),
            existing_uri.clone().unwrap_or_else(|| incoming_uri.clone()),
        );
        if existing_uri.is_none_or(|uri| uri == incoming_uri) {
            library.upsert(track);
        }
    }
    aliases
}

pub fn without_fixtures(library: &Library) -> Result<Library, String> {
    let mut value: serde_json::Value =
        serde_json::from_slice(&export_json(library)).map_err(|error| error.to_string())?;
    let tracks = value
        .pointer_mut("/library/tracks")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or_else(|| "library export did not contain tracks".to_string())?;
    tracks.retain(|track| {
        !track
            .get("uri")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|uri| uri.starts_with("fixture:"))
    });
    import(&serde_json::to_vec(&value).map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, HashMap},
        sync::Mutex,
        time::Duration,
    };

    use retune_core::model::{NewTrack, Rating, TrackEdit};

    use super::*;
    use crate::{
        fixture,
        provider::{FakeProvider, LibraryKind},
        store::{SavedAlbumRecord, SpotifyLibraryState, StoreResult},
    };

    #[derive(Default)]
    struct RecordingStore(Mutex<Vec<Library>>);

    impl OverlayStore for RecordingStore {
        fn load(&self) -> StoreResult<Option<Library>> {
            Ok(None)
        }

        fn save(&self, library: &Library) -> StoreResult<()> {
            self.0.lock().unwrap().push(library.clone());
            Ok(())
        }
    }

    fn track(uri: &str, name: &str) -> NewTrack {
        NewTrack {
            uri: uri.into(),
            cat: "Rock".into(),
            art: "Artist".into(),
            alb: "Album".into(),
            name: name.into(),
            duration: Duration::from_secs(1),
            kind: Some("Spotify".into()),
            ..NewTrack::default()
        }
    }

    #[test]
    fn sync_collapses_spotify_aliases_for_the_same_album_slot() {
        let mut library = Library::new();
        let mut original = track("spotify:track:original", "Song");
        original.track_no = Some(1);
        original.disc_no = Some(1);
        let original_id = library.add(original);
        let mut alias = track("spotify:track:alias", "Song");
        alias.track_no = Some(1);
        alias.disc_no = Some(1);
        let mut next_track = track("spotify:track:next", "Song");
        next_track.track_no = Some(2);
        next_track.disc_no = Some(1);
        let mut rerelease = track("spotify:track:rerelease", "Song");
        rerelease.track_no = Some(1);
        rerelease.disc_no = Some(1);
        rerelease.release_date = Some("2025-01-01".into());

        apply_in_memory(&mut library, vec![alias, next_track, rerelease]);

        assert_eq!(library.tracks().len(), 3);
        assert_eq!(library.tracks()[0].id, original_id);
        assert_eq!(library.tracks()[0].uri, "spotify:track:original");
        assert_eq!(library.tracks()[1].uri, "spotify:track:next");
        assert_eq!(library.tracks()[2].uri, "spotify:track:rerelease");
    }

    #[test]
    fn complete_sync_and_removal_preserve_alias_overlays_until_last_reference() {
        let mut library = Library::new();
        let mut original = track("spotify:track:original", "Song");
        original.track_no = Some(1);
        original.disc_no = Some(1);
        let original_id = library.add(original);
        {
            let retained = library
                .tracks_mut()
                .iter_mut()
                .find(|track| track.id == original_id)
                .unwrap();
            retained.rating = Rating::new(4);
            retained.play_count = 7;
            retained.last_played_at = Some(99);
        }

        let mut alias = track("spotify:track:alias", "Song");
        alias.track_no = Some(1);
        alias.disc_no = Some(1);
        let state = SpotifyLibraryState {
            account_id: "account".into(),
            complete: true,
            saved_tracks: BTreeMap::from([("spotify:track:alias".into(), Some(1))]),
            saved_albums: BTreeMap::from([(
                "spotify:album:album".into(),
                SavedAlbumRecord {
                    uri: "spotify:album:album".into(),
                    name: "Album".into(),
                    artists: vec!["Artist".into()],
                    release_date: None,
                    album_type: Some("album".into()),
                    added_at: Some(1),
                    track_uris: vec!["spotify:track:alias".into()],
                },
            )]),
        };
        let store = RecordingStore::default();

        apply(
            &mut library,
            &store,
            false,
            vec![alias.clone()],
            Some(&state),
        )
        .unwrap();

        let retained = &library.tracks()[0];
        assert_eq!(library.tracks().len(), 1);
        assert_eq!(retained.id, original_id);
        assert_eq!(retained.uri, "spotify:track:original");
        assert_eq!(retained.rating, Rating::new(4));
        assert_eq!(retained.play_count, 7);
        assert_eq!(retained.last_played_at, Some(99));

        let aliases = spotify_track_aliases(&library, &[alias]);
        let mut without_individual = state.clone();
        without_individual.saved_tracks.clear();
        prune_unreferenced_spotify_tracks_with_aliases(
            &mut library,
            &without_individual,
            &["spotify:track:alias".into()],
            &aliases,
        );
        assert_eq!(library.tracks().len(), 1);

        without_individual.saved_albums.clear();
        prune_unreferenced_spotify_tracks_with_aliases(
            &mut library,
            &without_individual,
            &["spotify:track:alias".into()],
            &aliases,
        );
        assert!(library.tracks().is_empty());
    }

    #[test]
    fn unknown_membership_never_prunes_local_tracks() {
        for state in [
            SpotifyLibraryState::default(),
            SpotifyLibraryState {
                account_id: "account".into(),
                complete: false,
                ..SpotifyLibraryState::default()
            },
        ] {
            let mut library = Library::new();
            library.add(track("spotify:track:unknown", "Unknown"));

            assert_eq!(
                prune_unreferenced_spotify_tracks_with_aliases(
                    &mut library,
                    &state,
                    &["spotify:track:unknown".into()],
                    &HashMap::new(),
                ),
                0
            );
            assert_eq!(library.tracks().len(), 1);
        }
    }

    #[tokio::test]
    async fn first_sync_purges_fixtures_once_dedupes_and_preserves_edits() {
        let mut library = fixture::library();
        let existing = library.add(track("spotify:track:kept", "Provider name"));
        library
            .edit(
                existing,
                TrackEdit {
                    name: Some("Local name".into()),
                    ..TrackEdit::default()
                },
            )
            .unwrap();
        let provider = FakeProvider {
            snapshots: HashMap::from([
                (
                    LibraryKind::Tracks,
                    vec![vec![
                        track("spotify:track:kept", "Changed upstream"),
                        track("spotify:track:new", "New"),
                    ]],
                ),
                (
                    LibraryKind::Albums,
                    vec![vec![track("spotify:track:new", "Duplicate")]],
                ),
            ]),
            genres_degraded: false,
            partial: false,
            quota_exhausted: false,
        };
        let store = RecordingStore::default();
        let mut phases: Vec<String> = vec![];

        reconcile(&provider, &mut library, &store, true, |phase| {
            phases.push(phase.into())
        })
        .await
        .unwrap();
        assert!(library
            .tracks()
            .iter()
            .all(|track| !track.uri.starts_with("fixture:")));
        library.add(track("fixture:added-after-sync", "Debug fixture"));
        reconcile(&provider, &mut library, &store, false, |_| {})
            .await
            .unwrap();

        assert_eq!(store.0.lock().unwrap().len(), 2);
        assert_eq!(phases.len(), LibraryKind::ALL.len());
        assert!(library
            .tracks()
            .iter()
            .any(|track| track.uri == "fixture:added-after-sync"));
        assert_eq!(
            library
                .tracks()
                .iter()
                .filter(|track| track.uri == "spotify:track:new")
                .count(),
            1
        );
        assert_eq!(library.get(existing).unwrap().name, "Local name");
    }

    #[tokio::test]
    async fn partial_snapshot_applies_collected_tracks() {
        let provider = FakeProvider {
            snapshots: HashMap::from([(
                LibraryKind::Tracks,
                vec![vec![track("spotify:track:partial", "Partial")]],
            )]),
            genres_degraded: false,
            partial: true,
            quota_exhausted: false,
        };
        let store = RecordingStore::default();
        let mut library = Library::new();

        let outcome = snapshot(&provider, |_| {}, &|_| {}).await.unwrap();
        assert!(outcome.partial);
        apply(&mut library, &store, false, outcome.tracks, None).unwrap();

        assert!(library
            .tracks()
            .iter()
            .any(|track| track.uri == "spotify:track:partial"));
    }

    #[test]
    fn degraded_sync_preserves_genre_and_healthy_sync_heals_it() {
        let mut library = Library::new();
        let id = library.add(track("spotify:track:one", "One"));
        let store = RecordingStore::default();
        let mut degraded = track("spotify:track:one", "Changed");
        degraded.cat = retune_spotify::normalize::UNCATEGORIZED.into();

        apply(&mut library, &store, false, vec![degraded], None).unwrap();
        assert_eq!(library.get(id).unwrap().cat, "Rock");

        let mut healthy = track("spotify:track:one", "Changed again");
        healthy.cat = "Metal".into();
        apply(&mut library, &store, false, vec![healthy], None).unwrap();
        assert_eq!(library.get(id).unwrap().cat, "Metal");
    }

    #[test]
    fn incremental_batches_then_final_apply_match_single_apply() {
        let mut base = Library::new();
        let id = base.add(track("spotify:track:one", "Provider name"));
        base.edit(
            id,
            TrackEdit {
                name: Some("Local name".into()),
                ..TrackEdit::default()
            },
        )
        .unwrap();
        let mut degraded = track("spotify:track:one", "Changed upstream");
        degraded.cat = retune_spotify::normalize::UNCATEGORIZED.into();
        let incoming = vec![degraded, track("spotify:track:two", "Two")];
        let store = RecordingStore::default();

        let mut incremental = base.clone();
        for batch in incoming.chunks(1) {
            apply_in_memory(&mut incremental, batch.to_vec());
        }
        apply(&mut incremental, &store, false, incoming.clone(), None).unwrap();

        let mut single = base;
        apply(&mut single, &store, false, incoming, None).unwrap();

        assert_eq!(export_json(&incremental), export_json(&single));
        assert_eq!(incremental.get(id).unwrap().name, "Local name");
    }

    #[test]
    fn partial_apply_keeps_existing_tracks_and_complete_apply_prunes_only_unreferenced_music() {
        let mut library = Library::new();
        library.add(track("spotify:track:keep", "Keep"));
        library.add(track("spotify:track:album", "Album"));
        library.add(track("spotify:track:drop", "Drop"));
        let store = RecordingStore::default();
        let state = SpotifyLibraryState {
            account_id: "account".into(),
            complete: true,
            saved_tracks: HashMap::from([("spotify:track:keep".into(), Some(1))])
                .into_iter()
                .collect(),
            saved_albums: [(
                "spotify:album:one".into(),
                SavedAlbumRecord {
                    uri: "spotify:album:one".into(),
                    name: "Album".into(),
                    artists: vec!["Artist".into()],
                    release_date: None,
                    album_type: None,
                    added_at: Some(1),
                    track_uris: vec!["spotify:track:album".into()],
                },
            )]
            .into_iter()
            .collect(),
        };

        apply(
            &mut library,
            &store,
            false,
            vec![track("spotify:track:new", "New")],
            None,
        )
        .unwrap();
        assert!(library
            .tracks()
            .iter()
            .any(|track| track.uri == "spotify:track:drop"));

        apply(&mut library, &store, false, vec![], Some(&state)).unwrap();
        assert!(library
            .tracks()
            .iter()
            .any(|track| track.uri == "spotify:track:keep"));
        assert!(library
            .tracks()
            .iter()
            .any(|track| track.uri == "spotify:track:album"));
        assert!(!library
            .tracks()
            .iter()
            .any(|track| track.uri == "spotify:track:drop"));
    }

    #[test]
    fn overlapping_album_references_retain_tracks_until_the_last_reference_is_removed() {
        let mut library = Library::new();
        library.add(track("spotify:track:shared", "Shared"));
        library.add(track("spotify:track:other", "Other"));
        let mut state = SpotifyLibraryState {
            account_id: "account".into(),
            complete: true,
            ..SpotifyLibraryState::default()
        };
        for (uri, track_uris) in [
            ("spotify:album:left", vec!["spotify:track:shared"]),
            (
                "spotify:album:right",
                vec!["spotify:track:shared", "spotify:track:other"],
            ),
        ] {
            state.saved_albums.insert(
                uri.into(),
                SavedAlbumRecord {
                    uri: uri.into(),
                    name: "Album".into(),
                    artists: vec![],
                    release_date: None,
                    album_type: None,
                    added_at: None,
                    track_uris: track_uris.into_iter().map(String::from).collect(),
                },
            );
        }

        state.saved_albums.remove("spotify:album:left");
        prune_unreferenced_spotify_tracks(
            &mut library,
            &state,
            &["spotify:track:shared".into(), "spotify:track:other".into()],
        );
        assert_eq!(library.tracks().len(), 2);

        state.saved_albums.remove("spotify:album:right");
        prune_unreferenced_spotify_tracks(
            &mut library,
            &state,
            &["spotify:track:shared".into(), "spotify:track:other".into()],
        );
        assert!(library.tracks().is_empty());
    }
}

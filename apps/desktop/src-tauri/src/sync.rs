use std::collections::{BTreeSet, HashMap, HashSet};

use retune_core::model::Library;

use crate::{
    provider::{LibraryKind, MediaProvider, SectionProgress, SyncBatch},
    spotify_membership::{
        spotify_new_track_identity, spotify_track_identity, SpotifyTrackIdentity,
    },
    store::SpotifyLibraryState,
};

#[cfg(test)]
use crate::store::OverlayStore;

#[cfg(test)]
pub async fn reconcile<P: MediaProvider, S: OverlayStore>(
    provider: &P,
    library: &mut Library,
    store: &S,
    first_sync: bool,
    mut progress: impl FnMut(&str) + Send,
) -> Result<(), String> {
    let (candidate, _) =
        working_snapshot(provider, library, first_sync, &mut progress, &|_| {}).await?;
    commit_candidate(library, store, candidate)
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

struct SpotifyTrackIndex {
    uris: HashMap<String, usize>,
    identities: HashMap<SpotifyTrackIdentity, BTreeSet<usize>>,
    by_index: Vec<(String, Option<SpotifyTrackIdentity>)>,
}

impl SpotifyTrackIndex {
    fn new(library: &Library) -> Self {
        let mut index = Self {
            uris: HashMap::new(),
            identities: HashMap::new(),
            by_index: Vec::with_capacity(library.tracks().len()),
        };
        for track in library.tracks() {
            index.insert(track.uri.clone(), spotify_track_identity(track));
        }
        index
    }

    fn matching_index(&self, track: &retune_core::model::NewTrack) -> Option<usize> {
        let uri = self.uris.get(&track.uri).copied();
        let identity = spotify_new_track_identity(track).and_then(|identity| {
            self.identities
                .get(&identity)
                .and_then(|indexes| indexes.first().copied())
        });
        uri.into_iter().chain(identity).min()
    }

    fn uri(&self, index: usize) -> &str {
        &self.by_index[index].0
    }

    fn insert(&mut self, uri: String, identity: Option<SpotifyTrackIdentity>) {
        let index = self.by_index.len();
        self.uris.insert(uri.clone(), index);
        if let Some(identity) = &identity {
            self.identities
                .entry(identity.clone())
                .or_default()
                .insert(index);
        }
        self.by_index.push((uri, identity));
    }

    fn refresh(&mut self, index: usize, track: &retune_core::model::NewTrack) {
        let Some(old) = self.by_index[index].1.clone() else {
            return;
        };
        if let Some(indexes) = self.identities.get_mut(&old) {
            indexes.remove(&index);
            if indexes.is_empty() {
                self.identities.remove(&old);
            }
        }
        let refreshed = old.refreshed(track);
        self.identities
            .entry(refreshed.clone())
            .or_default()
            .insert(index);
        self.by_index[index].1 = Some(refreshed);
    }
}

pub async fn snapshot<P: MediaProvider>(
    provider: &P,
    mut progress: impl FnMut(&str) + Send,
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
    let mut on_section = |kind: LibraryKind| progress(kind.phase());
    for snapshot in provider
        .complete_snapshot(&mut on_section, on_batch)
        .await?
    {
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

#[cfg(test)]
pub async fn working_snapshot<P: MediaProvider>(
    provider: &P,
    current: &Library,
    first_sync: bool,
    progress: impl FnMut(&str) + Send,
    on_batch: &(dyn Fn(&SyncBatch) + Send + Sync),
) -> Result<(Library, SnapshotOutcome), String> {
    let candidate = std::sync::Mutex::new(if first_sync {
        without_fixtures(current)?
    } else {
        current.clone()
    });
    let aliases = std::sync::Mutex::new(HashMap::new());
    let outcome = snapshot(provider, progress, &|batch| {
        on_batch(&batch);
        let batch_aliases = apply_in_memory_with_aliases(
            &mut candidate.lock().expect("sync candidate mutex poisoned"),
            batch.tracks,
        );
        aliases
            .lock()
            .expect("sync aliases mutex poisoned")
            .extend(batch_aliases);
    })
    .await?;
    let mut candidate = candidate
        .into_inner()
        .expect("sync candidate mutex poisoned");
    let aliases = aliases.into_inner().expect("sync aliases mutex poisoned");
    if let Some(spotify_library) = outcome.spotify_library.as_ref() {
        prune_unreferenced_spotify_music_with_aliases(&mut candidate, spotify_library, &aliases);
    }
    Ok((candidate, outcome))
}

pub fn candidate_from_snapshot(
    current: &Library,
    first_sync: bool,
    incoming: Vec<retune_core::model::NewTrack>,
    spotify_library: Option<&SpotifyLibraryState>,
) -> Result<Library, String> {
    let mut candidate = if first_sync {
        without_fixtures(current)?
    } else {
        current.clone()
    };
    let aliases = apply_in_memory_with_aliases(&mut candidate, incoming);
    if let Some(spotify_library) = spotify_library {
        prune_unreferenced_spotify_music_with_aliases(&mut candidate, spotify_library, &aliases);
    }
    Ok(candidate)
}

#[cfg(test)]
pub fn apply<S: OverlayStore>(
    library: &mut Library,
    store: &S,
    first_sync: bool,
    incoming: Vec<retune_core::model::NewTrack>,
    spotify_library: Option<&SpotifyLibraryState>,
) -> Result<(), String> {
    let candidate = candidate_from_snapshot(library, first_sync, incoming, spotify_library)?;
    commit_candidate(library, store, candidate)
}

#[cfg(test)]
fn commit_candidate<S: OverlayStore>(
    library: &mut Library,
    store: &S,
    candidate: Library,
) -> Result<(), String> {
    store.save(&candidate).map_err(|error| error.to_string())?;
    *library = candidate;
    Ok(())
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

#[cfg(test)]
pub fn apply_in_memory(library: &mut Library, incoming: Vec<retune_core::model::NewTrack>) {
    let _ = apply_in_memory_with_aliases(library, incoming);
}

pub fn spotify_track_aliases(
    library: &Library,
    incoming: &[retune_core::model::NewTrack],
) -> HashMap<String, String> {
    let index = SpotifyTrackIndex::new(library);
    incoming
        .iter()
        .filter_map(|track| {
            index
                .matching_index(track)
                .map(|existing| (track.uri.clone(), index.uri(existing).to_owned()))
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
    let mut index = SpotifyTrackIndex::new(library);
    let mut accepted = Vec::with_capacity(incoming.len());
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
        let existing = index.matching_index(&track);
        let existing_uri = existing.map(|existing| index.uri(existing).to_owned());
        aliases.insert(
            incoming_uri.clone(),
            existing_uri.clone().unwrap_or_else(|| incoming_uri.clone()),
        );
        if existing_uri.is_none_or(|uri| uri == incoming_uri) {
            if let Some(existing) = existing {
                index.refresh(existing, &track);
            } else {
                index.insert(track.uri.clone(), spotify_new_track_identity(&track));
            }
            accepted.push(track);
        }
    }
    library.upsert_all(accepted);
    aliases
}

pub fn without_fixtures(library: &Library) -> Result<Library, String> {
    let mut library = library.clone();
    let uris = library
        .tracks()
        .iter()
        .filter(|track| track.uri.starts_with("fixture:"))
        .map(|track| track.uri.clone())
        .collect::<Vec<_>>();
    library.remove_uris(&uris);
    Ok(library)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, HashMap},
        sync::{Arc, Mutex},
        time::Duration,
    };

    use retune_core::{
        io::export_json,
        model::{AlbumKey, NewTrack, Rating, TrackEdit},
    };

    use super::*;
    use crate::{
        fixture,
        provider::{FakeProvider, LibraryKind},
        store::{SavedAlbumRecord, SpotifyLibraryState, StoreError, StoreResult},
    };

    #[test]
    fn fixture_cleanup_removes_only_orphaned_album_ratings() {
        let mut library = Library::new();
        let fixture = library.add(NewTrack {
            uri: "fixture:one".into(),
            art: "Fixture Artist".into(),
            alb: "Fixture Album".into(),
            name: "Fixture".into(),
            ..NewTrack::default()
        });
        let shared_fixture = library.add(NewTrack {
            uri: "fixture:two".into(),
            art: "Artist".into(),
            alb: "Album".into(),
            name: "Fixture".into(),
            ..NewTrack::default()
        });
        library.add(NewTrack {
            uri: "spotify:track:kept".into(),
            art: "Artist".into(),
            alb: "Album".into(),
            name: "Kept".into(),
            ..NewTrack::default()
        });
        let orphaned = AlbumKey::of(library.get(fixture).unwrap());
        let shared = AlbumKey::of(library.get(shared_fixture).unwrap());
        library.set_album_rating(orphaned.clone(), Rating::new(4));
        library.set_album_rating(shared.clone(), Rating::new(5));

        let cleaned = without_fixtures(&library).unwrap();

        assert_eq!(cleaned.tracks().len(), 1);
        assert_eq!(cleaned.album_rating(&orphaned), None);
        assert_eq!(cleaned.album_rating(&shared), Rating::new(5));
        assert_eq!(library.tracks().len(), 3);
    }

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

    #[derive(Default)]
    struct FailingStore(Mutex<usize>);

    impl OverlayStore for FailingStore {
        fn load(&self) -> StoreResult<Option<Library>> {
            Ok(None)
        }

        fn save(&self, _library: &Library) -> StoreResult<()> {
            *self.0.lock().unwrap() += 1;
            Err(StoreError::Io(std::io::Error::other("save failed")))
        }
    }

    struct FailingAfterBatch {
        entered: Arc<tokio::sync::Barrier>,
        release: Arc<tokio::sync::Barrier>,
    }

    impl MediaProvider for FailingAfterBatch {
        async fn complete_snapshot(
            &self,
            on_section: &mut (dyn FnMut(LibraryKind) + Send),
            on_batch: &(dyn Fn(SyncBatch) + Send + Sync),
        ) -> Result<Vec<crate::provider::Snapshot>, String> {
            on_section(LibraryKind::Tracks);
            on_batch(SyncBatch {
                tracks: vec![track("spotify:track:uncommitted", "Uncommitted")],
                done: 1,
                total: Some(2),
                section: LibraryKind::Tracks.label(),
            });
            self.entered.wait().await;
            self.release.wait().await;
            Err("provider failed".into())
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
    fn indexed_sync_preserves_first_match_and_updates_batch_identity() {
        let mut library = Library::new();
        let mut original = track("spotify:track:original", "Song");
        original.track_no = Some(1);
        library.add(original);
        let mut exact_later = track("spotify:track:alias", "Song");
        exact_later.track_no = Some(1);
        library.add(exact_later.clone());

        let aliases = apply_in_memory_with_aliases(&mut library, vec![exact_later]);
        assert_eq!(aliases["spotify:track:alias"], "spotify:track:original");

        let mut first = track("spotify:track:new", "New");
        first.track_no = Some(1);
        let mut changed = first.clone();
        changed.track_no = Some(2);
        let mut changed_alias = changed.clone();
        changed_alias.uri = "spotify:track:new-alias".into();
        apply_in_memory(&mut library, vec![first, changed, changed_alias]);

        assert!(library
            .tracks()
            .iter()
            .any(|track| track.uri == "spotify:track:new" && track.track_no == Some(2)));
        assert!(!library
            .tracks()
            .iter()
            .any(|track| track.uri == "spotify:track:new-alias"));
    }

    #[test]
    fn complete_sync_and_removal_preserve_alias_overlays_until_last_reference() {
        let mut library = Library::new();
        let mut original = track("spotify:track:original", "Song");
        original.track_no = Some(1);
        original.disc_no = Some(1);
        let original_id = library.add(original);
        library
            .set_track_rating(original_id, Rating::new(4))
            .unwrap();
        library.merge_history_absolute("spotify:track:original", Some(7), None, Some(99));

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

    #[test]
    fn buffered_snapshot_rebases_over_an_explicit_mutation() {
        let baseline = Library::new();
        let buffered = vec![track("spotify:track:synced", "Synced")];
        let mut current = baseline.clone();
        current.add(track("spotify:track:explicit", "Explicit"));

        let candidate = candidate_from_snapshot(&current, false, buffered, None).unwrap();

        assert!(candidate
            .tracks()
            .iter()
            .any(|track| track.uri == "spotify:track:explicit"));
        assert!(candidate
            .tracks()
            .iter()
            .any(|track| track.uri == "spotify:track:synced"));
        assert!(baseline.tracks().is_empty());
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
        assert_eq!(store.0.lock().unwrap().len(), 1);
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
    async fn provider_failure_after_a_batch_leaves_live_and_persisted_library_unchanged() {
        let mut live = Library::new();
        live.add(track("spotify:track:existing", "Existing"));
        let before = export_json(&live);
        let store = Arc::new(RecordingStore(Mutex::new(vec![live.clone()])));
        let entered = Arc::new(tokio::sync::Barrier::new(2));
        let release = Arc::new(tokio::sync::Barrier::new(2));
        let provider = FailingAfterBatch {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        };
        let current = live.clone();
        let task = tokio::spawn(async move {
            working_snapshot(&provider, &current, false, |_| {}, &|_| {}).await
        });

        entered.wait().await;
        assert_eq!(export_json(&live), before);
        assert_eq!(store.0.lock().unwrap().len(), 1);
        assert_eq!(export_json(&store.0.lock().unwrap()[0]), before);
        release.wait().await;

        let error = match task.await.unwrap() {
            Err(error) => error,
            Ok(_) => panic!("provider unexpectedly succeeded"),
        };
        assert_eq!(error, "provider failed");
        assert_eq!(export_json(&live), before);
        assert_eq!(store.0.lock().unwrap().len(), 1);
    }

    #[test]
    fn store_failure_leaves_live_library_unchanged() {
        let mut live = Library::new();
        live.add(track("spotify:track:existing", "Existing"));
        let before = export_json(&live);
        let store = FailingStore::default();

        assert!(apply(
            &mut live,
            &store,
            false,
            vec![track("spotify:track:new", "New")],
            None,
        )
        .is_err());

        assert_eq!(export_json(&live), before);
        assert_eq!(*store.0.lock().unwrap(), 1);
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
    fn progressive_batches_then_enriched_final_apply_match_one_final_apply() {
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
        let mut progressive = track("spotify:track:one", "Changed upstream");
        progressive.cat = retune_spotify::normalize::UNCATEGORIZED.into();
        progressive.added_at = Some(20);
        let second = track("spotify:track:two", "Two");
        let mut enriched = progressive.clone();
        enriched.cat = "Metal".into();
        let final_tracks = vec![enriched, second.clone()];
        let store = RecordingStore::default();

        let mut incremental = base.clone();
        apply_in_memory(&mut incremental, vec![progressive, second]);
        apply(&mut incremental, &store, false, final_tracks.clone(), None).unwrap();

        let mut single = base;
        apply(&mut single, &store, false, final_tracks, None).unwrap();

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

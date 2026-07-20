use std::collections::HashMap;

use retune_core::{
    io::{export_json, import},
    model::Library,
};

use crate::{provider::MediaProvider, store::OverlayStore};

#[cfg(test)]
pub async fn reconcile<P: MediaProvider, S: OverlayStore>(
    provider: &P,
    library: &mut Library,
    store: &S,
    first_sync: bool,
    mut progress: impl FnMut(&str),
) -> Result<(), String> {
    let outcome = snapshot(provider, &mut progress).await?;
    apply(library, store, first_sync, outcome.tracks)
}

pub struct SnapshotOutcome {
    pub tracks: Vec<retune_core::model::NewTrack>,
    pub genres_degraded: bool,
    pub partial: bool,
}

pub async fn snapshot<P: MediaProvider>(
    provider: &P,
    mut progress: impl FnMut(&str),
) -> Result<SnapshotOutcome, String> {
    let mut incoming = vec![];
    let mut genres_degraded = false;
    let mut partial = false;
    for kind in crate::provider::LibraryKind::ALL {
        progress(kind.phase());
        let snapshot = provider.library_snapshot(kind).await?;
        genres_degraded |= snapshot.genres_degraded;
        partial |= snapshot.partial;
        for batch in snapshot.batches {
            incoming.extend(batch);
        }
    }
    Ok(SnapshotOutcome {
        tracks: incoming,
        genres_degraded,
        partial,
    })
}

pub fn apply<S: OverlayStore>(
    library: &mut Library,
    store: &S,
    first_sync: bool,
    incoming: Vec<retune_core::model::NewTrack>,
) -> Result<(), String> {
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
    let mut next = library.clone();
    if first_sync {
        next = without_fixtures(&next)?;
    }
    for mut track in incoming {
        if track.cat == retune_spotify::normalize::UNCATEGORIZED {
            if let Some(existing) = provider_genres
                .get(&track.uri)
                .filter(|genre| genre.as_str() != retune_spotify::normalize::UNCATEGORIZED)
            {
                track.cat.clone_from(existing);
            }
        }
        next.upsert(track);
    }
    store.save(&next).map_err(|error| error.to_string())?;
    *library = next;
    Ok(())
}

fn without_fixtures(library: &Library) -> Result<Library, String> {
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
    use std::{collections::HashMap, sync::Mutex, time::Duration};

    use retune_core::model::{NewTrack, SourceId, TrackEdit};

    use super::*;
    use crate::{
        fixture,
        provider::{FakeProvider, LibraryKind},
        store::StoreResult,
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
            source: SourceId::Music,
            cat: "Rock".into(),
            art: "Artist".into(),
            alb: "Album".into(),
            name: name.into(),
            duration: Duration::from_secs(1),
            track_no: None,
            disc_no: None,
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
        };
        let store = RecordingStore::default();
        let mut library = Library::new();

        let outcome = snapshot(&provider, |_| {}).await.unwrap();
        assert!(outcome.partial);
        apply(&mut library, &store, false, outcome.tracks).unwrap();

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

        apply(&mut library, &store, false, vec![degraded]).unwrap();
        assert_eq!(library.get(id).unwrap().cat, "Rock");

        let mut healthy = track("spotify:track:one", "Changed again");
        healthy.cat = "Metal".into();
        apply(&mut library, &store, false, vec![healthy]).unwrap();
        assert_eq!(library.get(id).unwrap().cat, "Metal");
    }
}

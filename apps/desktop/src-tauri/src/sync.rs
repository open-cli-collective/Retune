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
    let incoming = snapshot(provider, &mut progress).await?;
    apply(library, store, first_sync, incoming)
}

pub async fn snapshot<P: MediaProvider>(
    provider: &P,
    mut progress: impl FnMut(&str),
) -> Result<Vec<retune_core::model::NewTrack>, String> {
    let mut incoming = vec![];
    for kind in crate::provider::LibraryKind::ALL {
        progress(kind.phase());
        for batch in provider.library_snapshot(kind).await? {
            incoming.extend(batch);
        }
    }
    Ok(incoming)
}

pub fn apply<S: OverlayStore>(
    library: &mut Library,
    store: &S,
    first_sync: bool,
    incoming: Vec<retune_core::model::NewTrack>,
) -> Result<(), String> {
    let mut next = library.clone();
    if first_sync {
        next = without_fixtures(&next)?;
    }
    for track in incoming {
        next.add(track);
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
}

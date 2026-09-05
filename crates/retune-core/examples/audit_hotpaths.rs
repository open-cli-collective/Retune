//! Reproduce the 2026-09-05 Rust audit's CPU measurements without app data or APIs.
//! Run: cargo run -p retune-core --release --example audit_hotpaths
//! Comparisons are experiments, not installed production fixes or CI thresholds.
use std::{collections::HashMap, hint::black_box, time::Instant};

use retune_core::{
    browse::{self, Facets, Selection},
    io::{self, ImportError, SCHEMA_VERSION},
    model::{AlbumKey, EffectiveRating, Library, NewTrack, Rating, SourceId},
};

fn import_moving(bytes: &[u8]) -> Result<Library, ImportError> {
    let mut envelope: serde_json::Value = serde_json::from_slice(bytes)?;
    let object = envelope
        .as_object_mut()
        .ok_or(ImportError::MissingEnvelope)?;
    let version = object
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or(ImportError::MissingEnvelope)?;
    if version > u64::from(SCHEMA_VERSION) {
        return Err(ImportError::FromTheFuture(
            u32::try_from(version).unwrap_or(u32::MAX),
        ));
    }
    if version != u64::from(SCHEMA_VERSION) {
        return Err(ImportError::MissingEnvelope);
    }
    let library = object
        .remove("library")
        .ok_or(ImportError::MissingEnvelope)?;
    Ok(serde_json::from_value(library)?)
}

fn median_ms<T>(mut run: impl FnMut() -> T) -> f64 {
    drop(black_box(run())); // Warm up before five measured runs.
    let mut times = [0.0; 5];
    for time in &mut times {
        let start = Instant::now();
        drop(black_box(run()));
        *time = start.elapsed().as_secs_f64() * 1_000.0;
    }
    times.sort_by(f64::total_cmp);
    times[2]
}

fn borrowed_facets(library: &Library) -> Facets {
    let unique = |field: fn(&retune_core::model::TrackRecord) -> &str| {
        let mut values = library.tracks().iter().map(field).collect::<Vec<_>>();
        values.sort_unstable();
        values.dedup();
        values.sort_by_cached_key(|value| (value.to_lowercase(), *value));
        values.into_iter().map(str::to_owned).collect::<Vec<_>>()
    };
    let mut cats = unique(|track| &track.cat);
    cats.sort_by_key(|cat| cat != retune_core::UNCATEGORIZED);
    Facets {
        cats,
        arts: unique(|track| &track.art),
        albs: unique(|track| &track.alb),
    }
}

fn main() {
    println!("rows,operation,current_ms,experiment_ms");
    for size in [10_000, 20_000, 50_000] {
        let mut library = Library::new();
        library.add_all((0..size).map(|index| NewTrack {
            uri: format!("spotify:track:{index:08}"),
            name: format!("Track {index}"),
            cat: ["Rock", "Jazz", "rock", retune_core::UNCATEGORIZED][index % 4].into(),
            art: format!("Artist {:04}", (index * 97) % 500),
            alb: format!("Album {:05}", (index * 37) % 5_000),
            kind: Some("Spotify".into()),
            added_at: Some(1),
            ..NewTrack::default()
        }));
        library.set_album_rating(AlbumKey::of(&library.tracks()[0]), Rating::new(3));
        library
            .set_track_rating(library.tracks()[1].id, Rating::new(5))
            .unwrap();
        let old_ratings = || {
            library
                .tracks()
                .iter()
                .map(|track| library.effective_rating(track.id))
                .collect::<Vec<_>>()
        };
        let new_ratings = || {
            library
                .tracks()
                .iter()
                .map(|track| {
                    track.rating.map(EffectiveRating::Explicit).or_else(|| {
                        library
                            .album_rating(&AlbumKey::of(track))
                            .map(EffectiveRating::Inherited)
                    })
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(old_ratings(), new_ratings());
        println!(
            "{size},ratings,{:.3},{:.3}",
            median_ms(old_ratings),
            median_ms(new_ratings)
        );

        let old_facets = || browse::facets(&library, SourceId::Music, &Selection::default());
        let new_facets = || borrowed_facets(&library);
        assert_eq!(old_facets(), new_facets());
        println!(
            "{size},facets,{:.3},{:.3}",
            median_ms(old_facets),
            median_ms(new_facets)
        );

        // The library-hit kernel from playback_resources::resolve_one. Includes
        // transient index construction in the experiment; excludes DTO/IPC work.
        let old_uris = || {
            library
                .tracks()
                .iter()
                .map(|resource| {
                    library
                        .tracks()
                        .iter()
                        .find(|track| track.uri == resource.uri)
                        .unwrap()
                        .id
                })
                .collect::<Vec<_>>()
        };
        let new_uris = || {
            let index = library
                .tracks()
                .iter()
                .map(|track| (track.uri.as_str(), track.id))
                .collect::<HashMap<_, _>>();
            library
                .tracks()
                .iter()
                .map(|track| index[track.uri.as_str()])
                .collect::<Vec<_>>()
        };
        assert_eq!(old_uris(), new_uris());
        println!(
            "{size},queue_uri_lookup,{:.3},{:.3}",
            median_ms(old_uris),
            median_ms(new_uris)
        );

        let ids = library
            .tracks()
            .iter()
            .map(|track| track.id)
            .collect::<Vec<_>>();
        // Existing startup calls this even for fully populated metadata.
        let old_backfill = median_ms(|| {
            for &id in &ids {
                assert!(
                    !library
                        .fill_missing_metadata(id, Some(2), None, None)
                        .unwrap()
                );
            }
        });
        println!("{size},noop_metadata_backfill,{old_backfill:.3},n/a");

        let bytes = io::export_json(&library);
        assert_eq!(io::import(&bytes).unwrap(), import_moving(&bytes).unwrap());
        println!(
            "{size},library_json_load,{:.3},{:.3}",
            median_ms(|| io::import(&bytes).unwrap()),
            median_ms(|| import_moving(&bytes).unwrap())
        );
    }
}

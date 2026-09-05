use std::path::{Path, PathBuf};

use retune_audio::{audio_kind, import_file, scan_paths, ImportedFile, MAX_SCAN_FAILURE_DETAILS};
use retune_core::model::{Library, NewTrack, SourceId};
use retune_spotify::normalize::UNCATEGORIZED;
use serde::Serialize;
use url::Url;

use crate::unix_now;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FailedImport {
    pub path: String,
    pub reason: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(crate) struct ImportSummary {
    pub imported: usize,
    pub duplicates: usize,
    pub failed: Vec<FailedImport>,
    #[serde(rename = "failureCount")]
    pub failure_count: usize,
}

pub(crate) struct PreparedImport {
    tracks: Vec<NewTrack>,
    duplicates: usize,
    failed: Vec<FailedImport>,
    failure_count: usize,
}

pub(crate) fn prepare_paths(paths: impl IntoIterator<Item = impl AsRef<Path>>) -> PreparedImport {
    let scan = scan_paths(paths);
    let mut prepared = PreparedImport {
        tracks: Vec::with_capacity(scan.files.len()),
        duplicates: scan.duplicates,
        failed: scan
            .failures
            .into_iter()
            .map(|failure| FailedImport {
                path: failure.path.display().to_string(),
                reason: failure.error.to_string(),
            })
            .collect(),
        failure_count: scan.failure_count,
    };
    for path in scan.files {
        match import_file(&path)
            .map(map_file)
            .map_err(|error| error.to_string())
        {
            Ok(track) => prepared.tracks.push(track),
            Err(reason) => record_failure(&mut prepared, &path, reason),
        }
    }
    prepared
}

pub(crate) fn commit_prepared(library: &mut Library, prepared: PreparedImport) -> ImportSummary {
    let supplied = prepared.tracks.len();
    let mut summary = ImportSummary {
        duplicates: prepared.duplicates,
        failed: prepared.failed,
        failure_count: prepared.failure_count,
        ..ImportSummary::default()
    };
    summary.imported = library.add_all(prepared.tracks);
    summary.duplicates += supplied - summary.imported;
    summary
}

fn record_failure(prepared: &mut PreparedImport, path: &Path, reason: String) {
    prepared.failure_count += 1;
    if prepared.failed.len() < MAX_SCAN_FAILURE_DETAILS {
        prepared.failed.push(FailedImport {
            path: path.display().to_string(),
            reason,
        });
    }
}

fn map_file(file: ImportedFile) -> NewTrack {
    let kind = audio_kind(Some(file.info.codec), &file.canonical_path).map(str::to_owned);
    let bitrate_kbps = std::fs::metadata(&file.canonical_path)
        .ok()
        .and_then(|metadata| average_bitrate(metadata.len(), file.tags.duration));
    let name = file
        .tags
        .title
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            file.canonical_path
                .file_stem()
                .expect("canonical file path has a file stem")
                .to_string_lossy()
                .into_owned()
        });
    NewTrack {
        uri: file_uri(&file.canonical_path),
        source: SourceId::Music,
        cat: nonempty(file.tags.genre, UNCATEGORIZED),
        art: nonempty(file.tags.artist, "Unknown Artist"),
        alb: nonempty(file.tags.album, "Unknown Album"),
        name,
        duration: file.tags.duration,
        track_no: file.tags.track_no,
        disc_no: file.tags.disc_no,
        added_at: Some(unix_now()),
        release_date: None,
        kind,
        bitrate_kbps,
    }
}

fn average_bitrate(bytes: u64, duration: std::time::Duration) -> Option<u32> {
    (duration.as_secs_f64() > 0.0)
        .then(|| ((bytes as f64 * 8.0 / duration.as_secs_f64() / 1000.0).round()) as u32)
}

pub(crate) fn backfill_metadata(library: &mut Library) -> bool {
    let now = unix_now();
    let updates = library
        .tracks()
        .iter()
        .filter(|track| {
            track.added_at.is_none()
                || (track.uri.starts_with("spotify:") && track.kind.is_none())
                || (track.uri.starts_with("file:")
                    && (track.kind.is_none() || track.bitrate_kbps.is_none()))
        })
        .map(|track| {
            let (kind, bitrate_kbps) = if track.uri.starts_with("spotify:") {
                (track.kind.is_none().then(|| "Spotify".into()), None)
            } else if track.uri.starts_with("file:") {
                path_from_file_uri(&track.uri).map_or((None, None), |path| {
                    let kind = if track.kind.is_none() {
                        audio_kind(None, &path).map(str::to_owned)
                    } else {
                        None
                    };
                    let bitrate_kbps = if track.bitrate_kbps.is_none() {
                        std::fs::metadata(path)
                            .ok()
                            .and_then(|metadata| average_bitrate(metadata.len(), track.duration))
                    } else {
                        None
                    };
                    (kind, bitrate_kbps)
                })
            } else {
                (None, None)
            };
            (track.id, kind, bitrate_kbps)
        })
        .collect::<Vec<_>>();
    library
        .fill_missing_metadata_all(
            updates
                .into_iter()
                .map(|(id, kind, bitrate_kbps)| (id, Some(now), kind, bitrate_kbps)),
        )
        .expect("metadata targets came from this library")
}

pub(crate) fn file_uri(canonical_path: &Path) -> String {
    Url::from_file_path(canonical_path)
        .expect("canonical file paths are absolute")
        .to_string()
}

pub(crate) fn path_from_file_uri(uri: &str) -> Result<PathBuf, String> {
    Url::parse(uri)
        .map_err(|error| error.to_string())?
        .to_file_path()
        .map_err(|()| format!("invalid file URI: {uri}"))
}

fn nonempty(value: Option<String>, fallback: &str) -> String {
    value
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use super::*;
    use crate::store::{FsOverlayStore, OverlayStore};
    use retune_audio::import_file;
    use retune_core::model::{Library, SourceId, TrackEdit};
    use tempfile::tempdir;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../crates/retune-audio/tests/fixtures")
            .join(name)
    }

    fn import_paths(
        library: &mut Library,
        paths: impl IntoIterator<Item = impl AsRef<Path>>,
    ) -> ImportSummary {
        commit_prepared(library, prepare_paths(paths))
    }

    #[test]
    fn file_uri_round_trips_spaces_and_unicode() {
        let path = std::env::temp_dir().join("Rétune song.mp3");
        let uri = file_uri(&path);
        assert_eq!(path_from_file_uri(&uri).unwrap(), path);
        assert!(uri.contains("%20"));
        assert!(uri.contains("%C3%A9"));
    }

    #[test]
    fn average_bitrate_rounds_to_nearest_kbps() {
        assert_eq!(
            average_bitrate(188_125, std::time::Duration::from_secs(10)),
            Some(151)
        );
    }

    #[test]
    fn mapping_carries_tags_and_uses_required_fallbacks() {
        let tagged = import_file(fixture("cc0-audio-tagged.mp3")).unwrap();
        let track = map_file(tagged);
        assert_eq!(track.source, SourceId::Music);
        assert_eq!(track.name, "Fixture Song");
        assert_eq!(track.art, "Fixture Artist");
        assert_eq!(track.alb, "Fixture Album");
        assert_eq!(track.cat, "Fixture Genre");
        assert_eq!(track.track_no, Some(7));
        assert_eq!(track.disc_no, Some(2));
        assert!(track.added_at.is_some());
        assert_eq!(track.kind.as_deref(), Some("MPEG audio file"));
        assert!(track
            .bitrate_kbps
            .is_some_and(|bitrate| (100..=200).contains(&bitrate)));
        assert!(track.duration.as_secs_f64() > 2.0);

        let untagged = import_file(fixture("cc0-audio.wav")).unwrap();
        let track = map_file(untagged);
        assert_eq!(track.name, "cc0-audio");
        assert_eq!(track.art, "Unknown Artist");
        assert_eq!(track.alb, "Unknown Album");
        assert_eq!(track.cat, "Uncategorized");

        let aac = map_file(import_file(fixture("cc0-audio-aac-lc.m4a")).unwrap());
        assert_eq!(aac.kind.as_deref(), Some("AAC audio file"));
        let alac = map_file(import_file(fixture("cc0-audio-alac.m4a")).unwrap());
        assert_eq!(alac.kind.as_deref(), Some("Apple Lossless audio file"));
    }

    #[cfg(unix)]
    #[test]
    fn batch_dedupes_symlinks_and_reimport_reports_duplicates() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let real = dir.path().join("song.mp3");
        fs::copy(fixture("cc0-audio.mp3"), &real).unwrap();
        symlink(&real, dir.path().join("alias.mp3")).unwrap();
        let mut library = Library::new();

        let alias = dir.path().join("alias.mp3");
        let first = import_paths(&mut library, [&real, &alias]);
        assert_eq!(
            (first.imported, first.duplicates, first.failed.len()),
            (1, 1, 0)
        );
        assert_eq!(library.tracks().len(), 1);
        let second = import_paths(&mut library, [&real, &alias]);
        assert_eq!(
            (second.imported, second.duplicates, second.failed.len()),
            (0, 2, 0)
        );
    }

    #[test]
    fn batch_reports_bad_input_path_and_continues() {
        let mut library = Library::new();
        let summary = import_paths(
            &mut library,
            [fixture("cc0-audio.mp3"), fixture("missing.mp3")],
        );

        assert_eq!((summary.imported, summary.failed.len()), (1, 1));
        assert!(summary.failed[0].path.ends_with("missing.mp3"));
    }

    #[cfg(unix)]
    #[test]
    fn directory_import_keeps_valid_file_and_reports_broken_child() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let root = dir.path().join("visible");
        fs::create_dir(&root).unwrap();
        let good = root.join("good.mp3");
        let broken = root.join("broken.mp3");
        fs::copy(fixture("cc0-audio.mp3"), &good).unwrap();
        symlink(root.join("missing.mp3"), &broken).unwrap();
        let mut library = Library::new();

        let summary = import_paths(&mut library, [&root]);

        assert_eq!((summary.imported, summary.failed.len()), (1, 1));
        assert_eq!(summary.failed[0].path, broken.display().to_string());
        assert_eq!(library.tracks().len(), 1);
    }

    #[test]
    fn reimporting_same_directory_counts_every_valid_file_as_duplicate() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("visible");
        fs::create_dir(&root).unwrap();
        fs::copy(fixture("cc0-audio.mp3"), root.join("one.mp3")).unwrap();
        fs::copy(fixture("cc0-audio.flac"), root.join("two.flac")).unwrap();
        let mut library = Library::new();

        let first = import_paths(&mut library, [&root]);
        let second = import_paths(&mut library, [&root]);

        assert_eq!((first.imported, first.duplicates), (2, 0));
        assert_eq!((second.imported, second.duplicates), (0, 2));
    }

    #[test]
    fn detached_preparation_commits_against_latest_library() {
        let mut library = Library::new();
        let prepared = prepare_paths([fixture("cc0-audio.mp3")]);
        let mut concurrent = map_file(import_file(fixture("cc0-audio.mp3")).unwrap());
        concurrent.name = "Concurrent edit wins".into();
        library.add(concurrent);

        let summary = commit_prepared(&mut library, prepared);

        assert_eq!((summary.imported, summary.duplicates), (0, 1));
        assert_eq!(library.tracks()[0].name, "Concurrent edit wins");
    }

    #[test]
    fn backfill_sets_spotify_and_local_metadata_then_is_a_noop() {
        let mut library = Library::new();
        let mut local = map_file(import_file(fixture("cc0-audio.mp3")).unwrap());
        local.kind = None;
        local.bitrate_kbps = None;
        local.added_at = None;
        let local_id = library.add(local.clone());
        local.uri = file_uri(&std::env::temp_dir().join("definitely/missing/song.flac"));
        let missing_id = library.add(local);
        let mut spotify = map_file(import_file(fixture("cc0-audio.mp3")).unwrap());
        spotify.uri = "spotify:track:one".into();
        spotify.kind = None;
        spotify.bitrate_kbps = None;
        spotify.added_at = None;
        let spotify_id = library.add(spotify);

        assert!(backfill_metadata(&mut library));
        let local = library.get(local_id).unwrap();
        assert_eq!(local.kind.as_deref(), Some("MPEG audio file"));
        assert!(local.bitrate_kbps.is_some());
        let missing = library.get(missing_id).unwrap();
        assert_eq!(missing.kind.as_deref(), Some("FLAC audio file"));
        assert_eq!(missing.bitrate_kbps, None);
        assert_eq!(
            library.get(spotify_id).unwrap().kind.as_deref(),
            Some("Spotify")
        );
        let added = library.tracks()[0].added_at;
        assert!(added.is_some());
        assert!(library.tracks().iter().all(|track| track.added_at == added));
        assert!(!backfill_metadata(&mut library));
    }

    #[test]
    #[ignore = "responsiveness benchmark; run with --release --ignored --nocapture"]
    fn audit_backfill_costs() {
        use std::{hint::black_box, time::Instant};

        let library = |complete: bool| {
            let mut library = Library::new();
            library.add_all((0..50_000).map(|index| NewTrack {
                uri: format!("spotify:track:{index}"),
                name: format!("Track {index}"),
                added_at: complete.then_some(1),
                kind: complete.then(|| "Spotify".into()),
                ..NewTrack::default()
            }));
            library
        };
        let mut correctness = library(false);
        assert!(backfill_metadata(&mut correctness));
        assert!(correctness
            .tracks()
            .iter()
            .all(|track| track.added_at.is_some() && track.kind.as_deref() == Some("Spotify")));
        for (name, mut fixtures) in [
            ("complete-50000", vec![library(true); 8]),
            ("missing-50000", vec![library(false); 8]),
        ] {
            black_box(backfill_metadata(fixtures.last_mut().unwrap()));
            fixtures.pop();
            let mut samples = Vec::with_capacity(7);
            for mut fixture in fixtures {
                let start = Instant::now();
                black_box(backfill_metadata(&mut fixture));
                samples.push(start.elapsed().as_secs_f64() * 1_000.0);
            }
            samples.sort_by(f64::total_cmp);
            println!(
                "BACKFILL fixture=responsiveness-v1 case={name} samples=7 median_ms={:.3} min={:.3} max={:.3}",
                samples[3], samples[0], samples[6]
            );
        }
    }

    #[test]
    fn mixed_batch_reports_bad_files_without_mutating_sources_and_persists() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("visible");
        fs::create_dir(&root).unwrap();
        let good = root.join("good.flac");
        let m4a = root.join("other.m4a");
        let wav = root.join("third.wav");
        let bad = root.join("bad.mp3");
        fs::copy(fixture("cc0-audio.flac"), &good).unwrap();
        fs::copy(fixture("cc0-audio-aac-lc.m4a"), &m4a).unwrap();
        fs::copy(fixture("cc0-audio.wav"), &wav).unwrap();
        fs::copy(fixture("not-audio.mp3"), &bad).unwrap();
        let bytes = fs::read(&good).unwrap();
        let modified = fs::metadata(&good).unwrap().modified().unwrap();
        let mut library = Library::new();

        let summary = import_paths(&mut library, [&root]);
        assert_eq!(
            (summary.imported, summary.duplicates, summary.failed.len()),
            (3, 0, 1)
        );
        assert_eq!(fs::read(&good).unwrap(), bytes);
        assert_eq!(fs::metadata(&good).unwrap().modified().unwrap(), modified);

        let edited = library.tracks()[0].id;
        library
            .edit(
                edited,
                TrackEdit {
                    name: Some("My title".into()),
                    ..TrackEdit::default()
                },
            )
            .unwrap();
        let saved = tempdir().unwrap();
        let store = FsOverlayStore::new(saved.path());
        store.save(&library).unwrap();
        let loaded = store.load().unwrap().unwrap();
        assert_eq!(loaded, library);
        assert_eq!(loaded.get(edited).unwrap().name, "My title");
    }
}

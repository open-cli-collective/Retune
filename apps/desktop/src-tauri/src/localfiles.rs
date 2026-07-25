use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use retune_audio::{import_file, scan_paths, ImportedFile};
use retune_core::model::{Library, NewTrack, SourceId};
use retune_spotify::normalize::UNCATEGORIZED;
use serde::Serialize;
use url::Url;

use crate::store::OverlayStore;

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
}

pub(crate) fn import_paths(
    library: &mut Library,
    paths: impl IntoIterator<Item = impl AsRef<Path>>,
) -> ImportSummary {
    let mut uris = library
        .tracks()
        .iter()
        .map(|track| track.uri.clone())
        .collect::<HashSet<_>>();
    let mut summary = ImportSummary::default();
    for supplied in paths {
        let supplied = supplied.as_ref();
        if let Err(error) = std::fs::symlink_metadata(supplied) {
            summary.failed.push(FailedImport {
                path: supplied.display().to_string(),
                reason: error.to_string(),
            });
            continue;
        }
        for path in scan_paths(&[supplied.to_owned()]) {
            if !uris.insert(file_uri(&path)) {
                summary.duplicates += 1;
                continue;
            }
            match import_file(&path)
                .map_err(|error| error.to_string())
                .and_then(map_file)
            {
                Ok(track) => {
                    library.add(track);
                    summary.imported += 1;
                }
                Err(error) => summary.failed.push(FailedImport {
                    path: path.display().to_string(),
                    reason: error,
                }),
            }
        }
    }
    summary
}

pub(crate) fn import_transaction(
    store: &impl OverlayStore,
    library: &mut Library,
    paths: &[PathBuf],
) -> Result<ImportSummary, String> {
    let mut next = library.clone();
    let summary = import_paths(&mut next, paths);
    store.save(&next).map_err(|error| error.to_string())?;
    *library = next;
    Ok(summary)
}

fn map_file(file: ImportedFile) -> Result<NewTrack, String> {
    let name = file
        .tags
        .title
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            file.canonical_path
                .file_stem()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .ok_or_else(|| format!("{} has no filename", file.canonical_path.display()))?;
    Ok(NewTrack {
        uri: file_uri(&file.canonical_path),
        source: SourceId::Music,
        cat: nonempty(file.tags.genre, UNCATEGORIZED),
        art: nonempty(file.tags.artist, "Unknown Artist"),
        alb: nonempty(file.tags.album, "Unknown Album"),
        name,
        duration: file.tags.duration,
        track_no: file.tags.track_no,
        disc_no: file.tags.disc_no,
    })
}

pub(crate) fn file_uri(canonical_path: &Path) -> String {
    Url::from_file_path(canonical_path)
        .expect("canonical file paths are absolute")
        .to_string()
}

fn nonempty(value: Option<String>, fallback: &str) -> String {
    value
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::{fs, path::PathBuf};

    use retune_audio::import_file;
    use retune_core::model::{Library, SourceId, TrackEdit};
    use tempfile::tempdir;
    use url::Url;

    use super::*;
    use crate::store::{FsOverlayStore, OverlayStore, StoreResult};

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../crates/retune-audio/tests/fixtures")
            .join(name)
    }

    #[test]
    fn file_uri_round_trips_spaces_and_unicode() {
        let path = PathBuf::from("/tmp/Rétune song.mp3");
        let uri = Url::parse(&file_uri(&path)).unwrap();
        assert_eq!(uri.to_file_path().unwrap(), path);
        assert!(uri.as_str().contains("%20"));
        assert!(uri.as_str().contains("%C3%A9"));
    }

    #[test]
    fn mapping_carries_tags_and_uses_required_fallbacks() {
        let tagged = import_file(fixture("cc0-audio-tagged.mp3")).unwrap();
        let track = map_file(tagged).unwrap();
        assert_eq!(track.source, SourceId::Music);
        assert_eq!(track.name, "Fixture Song");
        assert_eq!(track.art, "Fixture Artist");
        assert_eq!(track.alb, "Fixture Album");
        assert_eq!(track.cat, "Fixture Genre");
        assert_eq!(track.track_no, Some(7));
        assert_eq!(track.disc_no, Some(2));
        assert!(track.duration.as_secs_f64() > 2.0);

        let untagged = import_file(fixture("cc0-audio.wav")).unwrap();
        let track = map_file(untagged).unwrap();
        assert_eq!(track.name, "cc0-audio");
        assert_eq!(track.art, "Unknown Artist");
        assert_eq!(track.alb, "Unknown Album");
        assert_eq!(track.cat, "Uncategorized");
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

    struct RecordingStore {
        saves: Cell<usize>,
        fail: bool,
    }

    impl OverlayStore for RecordingStore {
        fn load(&self) -> StoreResult<Option<Library>> {
            unreachable!()
        }

        fn save(&self, _library: &Library) -> StoreResult<()> {
            self.saves.set(self.saves.get() + 1);
            if self.fail {
                Err(std::io::Error::other("save failed").into())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn transaction_saves_once_and_does_not_swap_on_save_failure() {
        let successful = RecordingStore {
            saves: Cell::new(0),
            fail: false,
        };
        let mut library = Library::new();
        let summary =
            import_transaction(&successful, &mut library, &[fixture("cc0-audio.mp3")]).unwrap();
        assert_eq!((summary.imported, successful.saves.get()), (1, 1));

        let failing = RecordingStore {
            saves: Cell::new(0),
            fail: true,
        };
        let before = library.clone();
        assert!(import_transaction(&failing, &mut library, &[fixture("cc0-audio.flac")]).is_err());
        assert_eq!(failing.saves.get(), 1);
        assert_eq!(library, before);
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

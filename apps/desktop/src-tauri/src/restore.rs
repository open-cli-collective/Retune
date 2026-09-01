use std::{
    fs,
    path::{Path, PathBuf},
};

use retune_core::model::Library;
use serde::{Deserialize, Serialize};

use crate::{
    lastfm_import::{
        load_mappings_for_recovery, save_mappings_for_recovery, PersistedLastFmMappings,
    },
    persistence::{atomic_write, read_limited},
    playlists::PlaylistCache,
    store::{FsOverlayStore, FsPlaylistStore, FsSettingsStore, OverlayStore, Settings},
};

const RESTORE_JOURNAL_VERSION: u8 = 1;
const RESTORE_JOURNAL_FILE: &str = "restore-journal.json";
const MAX_RESTORE_JOURNAL_BYTES: u64 = 1024 * 1024 * 1024;
#[cfg(test)]
const FAIL_COMPLETE_ONCE_FILE: &str = ".fail-restore-complete-once";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Change<T> {
    pub(crate) before: T,
    pub(crate) after: T,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum Phase {
    Applying,
    Complete,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RestoreJournal {
    version: u8,
    phase: Phase,
    pub(crate) library: Change<Library>,
    pub(crate) settings: Option<Change<Settings>>,
    pub(crate) playlists: Option<Change<PlaylistCache>>,
    pub(crate) lastfm_mappings: Option<Change<PersistedLastFmMappings>>,
}

impl RestoreJournal {
    pub(crate) fn applying(
        library: Change<Library>,
        settings: Option<Change<Settings>>,
        playlists: Option<Change<PlaylistCache>>,
        lastfm_mappings: Option<Change<PersistedLastFmMappings>>,
    ) -> Self {
        Self {
            version: RESTORE_JOURNAL_VERSION,
            phase: Phase::Applying,
            library,
            settings,
            playlists,
            lastfm_mappings,
        }
    }
}

#[derive(Debug)]
pub(crate) enum RestoreError {
    Corrupt(String),
    Conflict(&'static str),
    Persistence(String),
}

impl std::fmt::Display for RestoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Corrupt(error) => write!(formatter, "Restore journal is unreadable: {error}"),
            Self::Conflict(component) => write!(
                formatter,
                "Restore journal conflicts with current {component}; no recovery writes were made."
            ),
            Self::Persistence(error) => formatter.write_str(error),
        }
    }
}

impl std::error::Error for RestoreError {}

pub(crate) struct RestoreStore {
    app_data_dir: PathBuf,
    path: PathBuf,
}

impl RestoreStore {
    pub(crate) fn new(app_data_dir: &Path) -> Self {
        Self {
            app_data_dir: app_data_dir.to_owned(),
            path: app_data_dir.join(RESTORE_JOURNAL_FILE),
        }
    }

    pub(crate) fn begin(&self, journal: &RestoreJournal) -> Result<(), RestoreError> {
        self.save(journal)
    }

    pub(crate) fn complete(&self, journal: &RestoreJournal) -> Result<(), RestoreError> {
        #[cfg(test)]
        if fs::remove_file(self.app_data_dir.join(FAIL_COMPLETE_ONCE_FILE)).is_ok() {
            return Err(RestoreError::Persistence(
                "injected restore completion failure".into(),
            ));
        }
        let mut complete = journal.clone();
        complete.phase = Phase::Complete;
        self.save(&complete)
    }

    #[cfg(test)]
    pub(crate) fn fail_next_complete(&self) {
        fs::write(self.app_data_dir.join(FAIL_COMPLETE_ONCE_FILE), b"fail").unwrap();
    }

    pub(crate) fn cleanup(&self) -> Result<(), RestoreError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(RestoreError::Persistence(format!(
                "Could not remove completed restore journal: {error}"
            ))),
        }
    }

    fn save(&self, journal: &RestoreJournal) -> Result<(), RestoreError> {
        let bytes = encode_journal(journal, MAX_RESTORE_JOURNAL_BYTES)?;
        atomic_write(&self.path, &bytes, Some(0o600)).map_err(|error| {
            RestoreError::Persistence(format!("Could not save restore journal: {error}"))
        })
    }

    fn load(&self) -> Result<Option<RestoreJournal>, RestoreError> {
        let bytes = match read_limited(&self.path, MAX_RESTORE_JOURNAL_BYTES) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(RestoreError::Corrupt(error.to_string())),
        };
        let journal: RestoreJournal = serde_json::from_slice(&bytes)
            .map_err(|error| RestoreError::Corrupt(error.to_string()))?;
        if journal.version != RESTORE_JOURNAL_VERSION {
            return Err(RestoreError::Corrupt(format!(
                "unsupported version {}",
                journal.version
            )));
        }
        Ok(Some(journal))
    }

    pub(crate) fn recover(&self) -> Result<(), RestoreError> {
        let Some(journal) = self.load()? else {
            return Ok(());
        };
        if journal.phase == Phase::Complete {
            if let Err(error) = self.cleanup() {
                log::warn!("{error}");
            }
            return Ok(());
        }

        let library_store = FsOverlayStore::new(&self.app_data_dir);
        let library = library_store
            .load()
            .map_err(|error| RestoreError::Persistence(error.to_string()))?
            .ok_or(RestoreError::Conflict("library"))?;
        require_known(&library, &journal.library, "library")?;

        let settings_store = FsSettingsStore::new(&self.app_data_dir);
        if let Some(change) = &journal.settings {
            let settings = settings_store
                .load()
                .map_err(|error| RestoreError::Persistence(error.to_string()))?
                .unwrap_or_default();
            require_known(&settings, change, "settings")?;
        }

        let playlist_store = FsPlaylistStore::new(&self.app_data_dir);
        if let Some(change) = &journal.playlists {
            let playlists = playlist_store
                .load()
                .map_err(|error| RestoreError::Persistence(error.to_string()))?;
            require_known(&playlists, change, "playlists")?;
        }

        if let Some(change) = &journal.lastfm_mappings {
            let mappings = load_mappings_for_recovery(&self.app_data_dir)
                .map_err(RestoreError::Persistence)?;
            require_known(&mappings, change, "Last.fm mappings")?;
        }

        library_store
            .save(&journal.library.after)
            .map_err(|error| RestoreError::Persistence(error.to_string()))?;
        if let Some(change) = &journal.settings {
            settings_store
                .save(&change.after)
                .map_err(|error| RestoreError::Persistence(error.to_string()))?;
        }
        if let Some(change) = &journal.playlists {
            playlist_store
                .save(&change.after)
                .map_err(|error| RestoreError::Persistence(error.to_string()))?;
        }
        if let Some(change) = &journal.lastfm_mappings {
            save_mappings_for_recovery(&self.app_data_dir, &change.after)
                .map_err(RestoreError::Persistence)?;
        }
        self.complete(&journal)?;
        if let Err(error) = self.cleanup() {
            log::warn!("{error}");
        }
        Ok(())
    }
}

fn encode_journal(journal: &RestoreJournal, limit: u64) -> Result<Vec<u8>, RestoreError> {
    let bytes = serde_json::to_vec(journal)
        .map_err(|error| RestoreError::Persistence(error.to_string()))?;
    if bytes.len() as u64 > limit {
        return Err(RestoreError::Persistence(format!(
            "Restore journal exceeds the {limit}-byte safety limit; no restore writes were made."
        )));
    }
    Ok(bytes)
}

fn require_known<T: PartialEq>(
    current: &T,
    change: &Change<T>,
    component: &'static str,
) -> Result<(), RestoreError> {
    if current == &change.before || current == &change.after {
        Ok(())
    } else {
        Err(RestoreError::Conflict(component))
    }
}

#[cfg(test)]
mod tests {
    use retune_core::model::NewTrack;
    use tempfile::tempdir;

    use super::*;
    use crate::playlists::{CachedPlaylist, TRACK_METADATA_VERSION};

    fn libraries() -> Change<Library> {
        let before = Library::new();
        let mut after = Library::new();
        after.add(NewTrack {
            uri: "file:///restored.mp3".into(),
            name: "Restored".into(),
            ..NewTrack::default()
        });
        Change { before, after }
    }

    fn mappings() -> Change<PersistedLastFmMappings> {
        let before = PersistedLastFmMappings {
            version: crate::lastfm_import::LASTFM_MAPPINGS_VERSION,
            ..PersistedLastFmMappings::default()
        };
        let mut after = before.clone();
        after.dormant = true;
        after.lastfm_username = Some("listener".into());
        Change { before, after }
    }

    fn playlists() -> Change<PlaylistCache> {
        let before = PlaylistCache::default();
        let after = PlaylistCache {
            playlists: vec![CachedPlaylist {
                id: "restored".into(),
                name: "Restored".into(),
                snapshot_id: "snapshot".into(),
                owned: true,
                owner: None,
                track_count: 1,
                tracks: vec!["spotify:track:restored".into()],
                track_metadata_version: TRACK_METADATA_VERSION,
                spotify_tracks: vec![],
            }],
        };
        Change { before, after }
    }

    #[test]
    fn every_real_file_boundary_rolls_all_components_forward() {
        let library = libraries();
        let before_settings = Settings {
            zebra: false,
            ..Settings::default()
        };
        let after_settings = Settings {
            zebra: true,
            ..before_settings.clone()
        };
        let settings = Change {
            before: before_settings,
            after: after_settings,
        };
        let playlists = playlists();
        let mappings = mappings();
        let journal = RestoreJournal::applying(
            library.clone(),
            Some(settings.clone()),
            Some(playlists.clone()),
            Some(mappings.clone()),
        );

        for durable_after_count in 0..=4 {
            let dir = tempdir().unwrap();
            FsOverlayStore::new(dir.path())
                .save(if durable_after_count >= 1 {
                    &library.after
                } else {
                    &library.before
                })
                .unwrap();
            FsSettingsStore::new(dir.path())
                .save(if durable_after_count >= 2 {
                    &settings.after
                } else {
                    &settings.before
                })
                .unwrap();
            FsPlaylistStore::new(dir.path())
                .save(if durable_after_count >= 3 {
                    &playlists.after
                } else {
                    &playlists.before
                })
                .unwrap();
            save_mappings_for_recovery(
                dir.path(),
                if durable_after_count >= 4 {
                    &mappings.after
                } else {
                    &mappings.before
                },
            )
            .unwrap();
            let store = RestoreStore::new(dir.path());
            store.begin(&journal).unwrap();
            #[cfg(unix)]
            assert_eq!(
                std::os::unix::fs::PermissionsExt::mode(
                    &fs::metadata(dir.path().join(RESTORE_JOURNAL_FILE))
                        .unwrap()
                        .permissions()
                ) & 0o777,
                0o600
            );

            store.recover().unwrap();

            assert_eq!(
                FsOverlayStore::new(dir.path()).load().unwrap(),
                Some(library.after.clone()),
                "library at boundary {durable_after_count}"
            );
            assert_eq!(
                FsSettingsStore::new(dir.path()).load().unwrap(),
                Some(settings.after.clone()),
                "settings at boundary {durable_after_count}"
            );
            assert_eq!(
                FsPlaylistStore::new(dir.path()).load().unwrap(),
                playlists.after,
                "playlists at boundary {durable_after_count}"
            );
            assert_eq!(
                load_mappings_for_recovery(dir.path()).unwrap(),
                mappings.after,
                "mappings at boundary {durable_after_count}"
            );
            assert!(
                !dir.path().join(RESTORE_JOURNAL_FILE).exists(),
                "journal at boundary {durable_after_count}"
            );
        }
    }

    #[test]
    fn conflict_preserves_every_file_and_the_journal() {
        let dir = tempdir().unwrap();
        let library = libraries();
        let mut third = Library::new();
        third.add(NewTrack {
            uri: "file:///third.mp3".into(),
            ..NewTrack::default()
        });
        FsOverlayStore::new(dir.path()).save(&third).unwrap();
        let store = RestoreStore::new(dir.path());
        store
            .begin(&RestoreJournal::applying(library, None, None, None))
            .unwrap();

        assert!(matches!(
            store.recover(),
            Err(RestoreError::Conflict("library"))
        ));
        assert_eq!(FsOverlayStore::new(dir.path()).load().unwrap(), Some(third));
        assert!(dir.path().join(RESTORE_JOURNAL_FILE).exists());
    }

    #[test]
    fn journal_writer_and_reader_share_the_same_hard_ceiling() {
        let journal = RestoreJournal::applying(libraries(), None, None, None);
        let encoded = serde_json::to_vec(&journal).unwrap();

        assert_eq!(
            encode_journal(&journal, encoded.len() as u64).unwrap(),
            encoded
        );
        let error = encode_journal(&journal, encoded.len() as u64 - 1).unwrap_err();
        assert!(error.to_string().contains("no restore writes were made"));
    }

    #[test]
    fn complete_marker_never_replays_old_after_state() {
        let dir = tempdir().unwrap();
        let library = libraries();
        let store = RestoreStore::new(dir.path());
        let journal = RestoreJournal::applying(library.clone(), None, None, None);
        store.complete(&journal).unwrap();
        FsOverlayStore::new(dir.path())
            .save(&library.before)
            .unwrap();

        store.recover().unwrap();

        assert_eq!(
            FsOverlayStore::new(dir.path()).load().unwrap(),
            Some(library.before)
        );
        assert!(!dir.path().join(RESTORE_JOURNAL_FILE).exists());
    }

    #[test]
    fn components_absent_from_backup_are_untouched() {
        let dir = tempdir().unwrap();
        let library = libraries();
        let settings = Settings {
            zebra: true,
            ..Settings::default()
        };
        FsOverlayStore::new(dir.path())
            .save(&library.before)
            .unwrap();
        FsSettingsStore::new(dir.path()).save(&settings).unwrap();
        let store = RestoreStore::new(dir.path());
        store
            .begin(&RestoreJournal::applying(library, None, None, None))
            .unwrap();

        store.recover().unwrap();

        assert_eq!(
            FsSettingsStore::new(dir.path()).load().unwrap(),
            Some(settings)
        );
    }
}

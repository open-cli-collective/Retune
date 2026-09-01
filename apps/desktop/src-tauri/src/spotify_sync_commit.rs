use std::{
    fs,
    path::{Path, PathBuf},
};

use retune_core::model::Library;
use serde::{Deserialize, Serialize};

use crate::{
    persistence::{atomic_write, read_limited},
    store::{
        FsOverlayStore, FsSettingsStore, FsSpotifyLibraryStore, OverlayStore, Settings,
        SpotifyLibraryState,
    },
};

const VERSION: u8 = 1;
const FILE: &str = "spotify-sync-journal.json";
const MAX_BYTES: u64 = 1024 * 1024 * 1024;

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
pub(crate) struct Journal {
    version: u8,
    phase: Phase,
    pub(crate) membership: Change<SpotifyLibraryState>,
    pub(crate) library: Change<Library>,
    pub(crate) settings: Change<Settings>,
}

impl Journal {
    pub(crate) fn applying(
        membership: Change<SpotifyLibraryState>,
        library: Change<Library>,
        settings: Change<Settings>,
    ) -> Self {
        Self {
            version: VERSION,
            phase: Phase::Applying,
            membership,
            library,
            settings,
        }
    }
}

pub(crate) struct Store {
    app_data_dir: PathBuf,
    path: PathBuf,
    #[cfg(test)]
    fail_after: Option<usize>,
    #[cfg(test)]
    pause_before: Option<(usize, std::sync::Arc<crate::store::SaveHook>)>,
}

impl Store {
    pub(crate) fn new(app_data_dir: &Path) -> Self {
        Self {
            app_data_dir: app_data_dir.to_owned(),
            path: app_data_dir.join(FILE),
            #[cfg(test)]
            fail_after: None,
            #[cfg(test)]
            pause_before: None,
        }
    }

    #[cfg(test)]
    fn failing_after(app_data_dir: &Path, writes: usize) -> Self {
        let mut store = Self::new(app_data_dir);
        store.fail_after = Some(writes);
        store
    }

    #[cfg(test)]
    pub(crate) fn pausing_before(
        app_data_dir: &Path,
        writes: usize,
        hook: std::sync::Arc<crate::store::SaveHook>,
    ) -> Self {
        let mut store = Self::new(app_data_dir);
        store.pause_before = Some((writes, hook));
        store
    }

    pub(crate) fn commit(&self, journal: &Journal) -> Result<(), String> {
        self.save(journal)?;
        let primary = self
            .roll_forward(journal)
            .and_then(|()| self.mark_complete(journal));
        if let Err(primary_error) = primary {
            self.recover().map_err(|recovery_error| {
                format!(
                    "Spotify sync commit failed ({primary_error}) and immediate recovery failed ({recovery_error}). Restart Retune before making more changes."
                )
            })?;
            log::warn!(
                "Spotify sync write failed but was rolled forward immediately: {primary_error}"
            );
            return Ok(());
        }
        if let Err(error) = self.cleanup() {
            log::warn!("Could not remove completed Spotify sync journal: {error}");
        }
        Ok(())
    }

    pub(crate) fn recover(&self) -> Result<(), String> {
        let Some(journal) = self.load()? else {
            return Ok(());
        };
        if journal.phase == Phase::Complete {
            if let Err(error) = self.cleanup() {
                log::warn!("Could not remove completed Spotify sync journal: {error}");
            }
            return Ok(());
        }
        self.validate_current(&journal)?;
        self.roll_forward_unchecked(&journal)?;
        self.mark_complete(&journal)?;
        if let Err(error) = self.cleanup() {
            log::warn!("Could not remove completed Spotify sync journal: {error}");
        }
        Ok(())
    }

    fn roll_forward(&self, journal: &Journal) -> Result<(), String> {
        self.write_component(0, || {
            FsSpotifyLibraryStore::new(&self.app_data_dir)
                .save(&journal.membership.after)
                .map_err(|e| e.to_string())
        })?;
        self.write_component(1, || {
            FsOverlayStore::new(&self.app_data_dir)
                .save(&journal.library.after)
                .map_err(|e| e.to_string())
        })?;
        self.write_component(2, || {
            FsSettingsStore::new(&self.app_data_dir)
                .save(&journal.settings.after)
                .map_err(|e| e.to_string())
        })?;
        Ok(())
    }

    fn roll_forward_unchecked(&self, journal: &Journal) -> Result<(), String> {
        FsSpotifyLibraryStore::new(&self.app_data_dir)
            .save(&journal.membership.after)
            .map_err(|e| e.to_string())?;
        FsOverlayStore::new(&self.app_data_dir)
            .save(&journal.library.after)
            .map_err(|e| e.to_string())?;
        FsSettingsStore::new(&self.app_data_dir)
            .save(&journal.settings.after)
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn write_component(
        &self,
        _index: usize,
        write: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        #[cfg(test)]
        if self.fail_after == Some(_index) {
            return Err(format!("injected failure before component {_index}"));
        }
        #[cfg(test)]
        if let Some((pause_index, hook)) = &self.pause_before {
            if *pause_index == _index {
                hook.pause().map_err(|error| error.to_string())?;
            }
        }
        write()
    }

    fn validate_current(&self, journal: &Journal) -> Result<(), String> {
        let membership = FsSpotifyLibraryStore::new(&self.app_data_dir)
            .load()
            .map_err(|e| e.to_string())?;
        require_known(&membership, &journal.membership, "Spotify membership")?;
        let library = FsOverlayStore::new(&self.app_data_dir)
            .load()
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "Spotify sync journal conflicts with missing library".to_string())?;
        require_known(&library, &journal.library, "library")?;
        let settings = FsSettingsStore::new(&self.app_data_dir)
            .load()
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
        require_known(&settings, &journal.settings, "settings")
    }

    fn save(&self, journal: &Journal) -> Result<(), String> {
        let bytes = serde_json::to_vec(journal).map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(format!(
                "Spotify sync journal exceeds the {MAX_BYTES}-byte safety limit"
            ));
        }
        atomic_write(&self.path, &bytes, Some(0o600)).map_err(|e| e.to_string())
    }

    fn mark_complete(&self, journal: &Journal) -> Result<(), String> {
        let mut complete = journal.clone();
        complete.phase = Phase::Complete;
        self.save(&complete)
    }

    fn load(&self) -> Result<Option<Journal>, String> {
        let bytes = match read_limited(&self.path, MAX_BYTES) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("Could not read Spotify sync journal: {error}")),
        };
        let journal: Journal = serde_json::from_slice(&bytes)
            .map_err(|e| format!("Spotify sync journal is unreadable: {e}"))?;
        if journal.version != VERSION {
            return Err(format!(
                "Unsupported Spotify sync journal version {}",
                journal.version
            ));
        }
        Ok(Some(journal))
    }

    fn cleanup(&self) -> Result<(), String> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }
}

fn require_known<T: PartialEq>(current: &T, change: &Change<T>, name: &str) -> Result<(), String> {
    if current == &change.before || current == &change.after {
        Ok(())
    } else {
        Err(format!(
            "Spotify sync journal conflicts with current {name}; no recovery writes were made."
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use retune_core::model::NewTrack;
    use tempfile::tempdir;

    fn fixture() -> Journal {
        let membership_before = SpotifyLibraryState::default();
        let membership_after = SpotifyLibraryState {
            account_id: "account".into(),
            complete: true,
            ..SpotifyLibraryState::default()
        };
        let library_before = Library::new();
        let mut library_after = Library::new();
        library_after.add(NewTrack {
            uri: "spotify:track:new".into(),
            ..NewTrack::default()
        });
        let settings_before = Settings::default();
        let mut settings_after = settings_before.clone();
        settings_after.spotify_sync_completed = true;
        Journal::applying(
            Change {
                before: membership_before,
                after: membership_after,
            },
            Change {
                before: library_before,
                after: library_after,
            },
            Change {
                before: settings_before,
                after: settings_after,
            },
        )
    }

    fn seed(dir: &Path, journal: &Journal) {
        FsSpotifyLibraryStore::new(dir)
            .save(&journal.membership.before)
            .unwrap();
        FsOverlayStore::new(dir)
            .save(&journal.library.before)
            .unwrap();
        FsSettingsStore::new(dir)
            .save(&journal.settings.before)
            .unwrap();
    }

    fn assert_after(dir: &Path, journal: &Journal) {
        assert_eq!(
            FsSpotifyLibraryStore::new(dir).load().unwrap(),
            journal.membership.after
        );
        assert_eq!(
            FsOverlayStore::new(dir).load().unwrap(),
            Some(journal.library.after.clone())
        );
        assert_eq!(
            FsSettingsStore::new(dir).load().unwrap(),
            Some(journal.settings.after.clone())
        );
    }

    #[test]
    fn failure_at_each_component_boundary_rolls_forward() {
        for boundary in 0..=2 {
            let dir = tempdir().unwrap();
            let journal = fixture();
            seed(dir.path(), &journal);
            Store::failing_after(dir.path(), boundary)
                .commit(&journal)
                .unwrap();
            assert_after(dir.path(), &journal);
            assert!(!dir.path().join(FILE).exists());
        }
    }

    #[test]
    fn startup_recovery_rolls_every_mixed_boundary_forward() {
        for durable in 0..=3 {
            let dir = tempdir().unwrap();
            let journal = fixture();
            seed(dir.path(), &journal);
            let store = Store::new(dir.path());
            store.save(&journal).unwrap();
            if durable >= 1 {
                FsSpotifyLibraryStore::new(dir.path())
                    .save(&journal.membership.after)
                    .unwrap();
            }
            if durable >= 2 {
                FsOverlayStore::new(dir.path())
                    .save(&journal.library.after)
                    .unwrap();
            }
            if durable >= 3 {
                FsSettingsStore::new(dir.path())
                    .save(&journal.settings.after)
                    .unwrap();
            }
            store.recover().unwrap();
            assert_after(dir.path(), &journal);
        }
    }
}

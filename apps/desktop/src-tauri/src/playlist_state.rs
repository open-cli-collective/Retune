use std::sync::{Arc, Mutex};

use tokio::sync::{Mutex as AsyncMutex, MutexGuard as AsyncMutexGuard, OwnedMutexGuard};

use crate::{
    playlists::PlaylistCache, restore_latch::RestoreMutationState, store::FsPlaylistStore,
};

pub(crate) struct PlaylistState {
    current: Arc<Mutex<CurrentPlaylistCache>>,
    mutation_gate: Arc<AsyncMutex<()>>,
    store: FsPlaylistStore,
    restore_mutations: Arc<RestoreMutationState>,
}

pub(crate) struct PlaylistOperation {
    _guard: OwnedMutexGuard<()>,
    current: Arc<Mutex<CurrentPlaylistCache>>,
    remote_outcome_uncertain: bool,
}

impl PlaylistOperation {
    pub(crate) fn remote_started(&mut self) {
        self.remote_outcome_uncertain = true;
    }

    pub(crate) fn remote_resolved(&mut self) {
        self.remote_outcome_uncertain = false;
    }
}

impl Drop for PlaylistOperation {
    fn drop(&mut self) {
        if self.remote_outcome_uncertain {
            self.current
                .lock()
                .expect("playlist mutex poisoned")
                .authoritative = false;
        }
    }
}

struct CurrentPlaylistCache {
    cache: PlaylistCache,
    authoritative: bool,
}

pub(crate) struct PlaylistRestore<'a> {
    state: &'a PlaylistState,
    _mutation_guard: AsyncMutexGuard<'a, ()>,
}

impl PlaylistState {
    #[cfg(test)]
    pub(crate) fn new(current: PlaylistCache, store: FsPlaylistStore) -> Self {
        Self::new_with_restore_state(current, store, Arc::new(RestoreMutationState::default()))
    }

    pub(crate) fn new_with_restore_state(
        current: PlaylistCache,
        store: FsPlaylistStore,
        restore_mutations: Arc<RestoreMutationState>,
    ) -> Self {
        Self {
            current: Arc::new(Mutex::new(CurrentPlaylistCache {
                cache: current,
                authoritative: true,
            })),
            mutation_gate: Arc::new(AsyncMutex::new(())),
            store,
            restore_mutations,
        }
    }

    pub(crate) fn snapshot(&self) -> Result<PlaylistCache, String> {
        let current = self.current.lock().expect("playlist mutex poisoned");
        current
            .authoritative
            .then(|| current.cache.clone())
            .ok_or_else(|| {
                "Playlist data must be refreshed from Spotify before it can be used again.".into()
            })
    }

    pub(crate) async fn begin_mutation(
        &self,
    ) -> Result<(PlaylistOperation, PlaylistCache), String> {
        let guard = Arc::clone(&self.mutation_gate).lock_owned().await;
        #[cfg(test)]
        self.restore_mutations.after_wait();
        self.restore_mutations.ensure_allowed()?;
        let current = self.current.lock().expect("playlist mutex poisoned");
        if !current.authoritative {
            return Err(
                "Playlist data must be refreshed from Spotify before another change.".into(),
            );
        }
        Ok((
            PlaylistOperation {
                _guard: guard,
                current: Arc::clone(&self.current),
                remote_outcome_uncertain: false,
            },
            current.cache.clone(),
        ))
    }

    pub(crate) async fn begin_sync(&self) -> Result<(PlaylistOperation, PlaylistCache), String> {
        let guard = Arc::clone(&self.mutation_gate).lock_owned().await;
        self.restore_mutations.ensure_allowed()?;
        let current = self.current.lock().expect("playlist mutex poisoned");
        Ok((
            PlaylistOperation {
                _guard: guard,
                current: Arc::clone(&self.current),
                remote_outcome_uncertain: false,
            },
            if current.authoritative {
                current.cache.clone()
            } else {
                PlaylistCache::default()
            },
        ))
    }

    pub(crate) async fn commit(
        &self,
        mut operation: PlaylistOperation,
        next: PlaylistCache,
        invalidate_on_failure: bool,
    ) -> Result<PlaylistOperation, String> {
        self.restore_mutations.ensure_allowed()?;
        let store = self.store.clone();
        let current = Arc::clone(&self.current);
        let completion = tauri::async_runtime::spawn(async move {
            let result =
                tauri::async_runtime::spawn_blocking(move || store.save(&next).map(|()| next))
                    .await
                    .map_err(|error| error.to_string())?
                    .map_err(|error| error.to_string());
            let next = match result {
                Ok(next) => next,
                Err(error) => {
                    if invalidate_on_failure || operation.remote_outcome_uncertain {
                        current
                            .lock()
                            .expect("playlist mutex poisoned")
                            .authoritative = false;
                    }
                    operation.remote_resolved();
                    return Err(error);
                }
            };
            *current.lock().expect("playlist mutex poisoned") = CurrentPlaylistCache {
                cache: next,
                authoritative: true,
            };
            operation.remote_resolved();
            Ok(operation)
        });
        completion.await.map_err(|error| error.to_string())?
    }

    pub(crate) async fn begin_restore(&self) -> Result<PlaylistRestore<'_>, String> {
        let mutation_guard = self.mutation_gate.lock().await;
        self.restore_mutations.ensure_allowed()?;
        Ok(PlaylistRestore {
            state: self,
            _mutation_guard: mutation_guard,
        })
    }
}

impl PlaylistRestore<'_> {
    pub(crate) fn snapshot(&self) -> PlaylistCache {
        self.state
            .current
            .lock()
            .expect("playlist mutex poisoned")
            .cache
            .clone()
    }

    pub(crate) fn replace(&self, next: PlaylistCache) -> Result<(), String> {
        self.state
            .store
            .save(&next)
            .map_err(|error| error.to_string())?;
        *self.state.current.lock().expect("playlist mutex poisoned") = CurrentPlaylistCache {
            cache: next,
            authoritative: true,
        };
        Ok(())
    }

    pub(crate) fn install_recovered(&self, next: PlaylistCache) {
        *self.state.current.lock().expect("playlist mutex poisoned") = CurrentPlaylistCache {
            cache: next,
            authoritative: true,
        };
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use retune_spotify::client::{fake_client, Response};

    use super::*;

    fn playlist(id: &str) -> crate::playlists::CachedPlaylist {
        crate::playlists::CachedPlaylist {
            id: id.into(),
            name: id.into(),
            snapshot_id: "snapshot".into(),
            owned: true,
            owner: None,
            track_count: 0,
            tracks: Vec::new(),
            track_metadata_version: crate::playlists::TRACK_METADATA_VERSION,
            spotify_tracks: Vec::new(),
        }
    }

    #[test]
    fn queued_mutation_checks_restore_latch_after_async_gate_wait() {
        let directory = tempfile::tempdir().unwrap();
        let restore_mutations = Arc::new(RestoreMutationState::default());
        let state = Arc::new(PlaylistState::new_with_restore_state(
            PlaylistCache::default(),
            FsPlaylistStore::new(directory.path()),
            Arc::clone(&restore_mutations),
        ));
        let restore = tauri::async_runtime::block_on(state.begin_restore()).unwrap();
        let hook = restore_mutations.arm_after_wait();
        let worker = {
            let state = Arc::clone(&state);
            std::thread::spawn(move || {
                tauri::async_runtime::block_on(state.begin_mutation()).map(|_| ())
            })
        };

        drop(restore);
        hook.wait_until_reached();
        restore_mutations.mark_recovery_required();
        hook.release();

        assert!(worker.join().unwrap().is_err());
        assert_eq!(
            FsPlaylistStore::new(directory.path()).load().unwrap(),
            PlaylistCache::default()
        );
    }

    #[tokio::test]
    async fn concurrent_remote_mutations_both_survive_in_memory_and_on_disk() {
        let directory = tempfile::tempdir().unwrap();
        let store = FsPlaylistStore::new(directory.path());
        let state = Arc::new(PlaylistState::new(
            PlaylistCache {
                playlists: vec![playlist("first"), playlist("second")],
            },
            store.clone(),
        ));
        let client = Arc::new(fake_client(
            [
                Response::json(200, serde_json::Value::Null),
                Response::json(200, serde_json::Value::Null),
            ],
            &retune_spotify::auth::SCOPES,
        ));
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for id in ["first", "second"] {
            let state = Arc::clone(&state);
            let client = Arc::clone(&client);
            let barrier = Arc::clone(&barrier);
            workers.push(tokio::spawn(async move {
                barrier.wait().await;
                let (mut operation, mut cache) = state.begin_mutation().await.unwrap();
                operation.remote_started();
                crate::playlists::unfollow(client.as_ref(), &mut cache, id)
                    .await
                    .unwrap();
                operation.remote_resolved();
                state.commit(operation, cache, true).await.unwrap();
            }));
        }
        barrier.wait().await;
        for worker in workers {
            worker.await.unwrap();
        }

        let current = state.snapshot().unwrap();
        assert!(current.playlists.is_empty());
        assert_eq!(store.load().unwrap(), current);
        let requests = client.transport().requests();
        assert_eq!(requests.len(), 2);
        assert!(requests
            .iter()
            .any(|request| request.url.ends_with("/playlists/first/followers")));
        assert!(requests
            .iter()
            .any(|request| request.url.ends_with("/playlists/second/followers")));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn aborted_playlist_save_finishes_disk_and_memory_commit() {
        let directory = tempfile::tempdir().unwrap();
        let store = FsPlaylistStore::new(directory.path());
        let control = store.clone();
        let state = Arc::new(PlaylistState::new(
            PlaylistCache {
                playlists: vec![playlist("before")],
            },
            store,
        ));
        let hook = crate::store::SaveHook::new(false);
        control.arm_save(Arc::clone(&hook));

        let commit = {
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                let (operation, mut next) = state.begin_mutation().await.unwrap();
                next.playlists[0].name = "after".into();
                state.commit(operation, next, false).await
            })
        };
        hook.wait_until_reached();
        commit.abort();
        hook.release();
        assert!(matches!(commit.await, Err(error) if error.is_cancelled()));

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if state
                    .snapshot()
                    .is_ok_and(|cache| cache.playlists[0].name == "after")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(control.load().unwrap(), state.snapshot().unwrap());
    }

    #[tokio::test]
    async fn cancellation_after_fake_remote_success_invalidates_the_cache() {
        let directory = tempfile::tempdir().unwrap();
        let state = Arc::new(PlaylistState::new(
            PlaylistCache {
                playlists: vec![playlist("before")],
            },
            FsPlaylistStore::new(directory.path()),
        ));
        let remote_committed = Arc::new(AtomicBool::new(false));
        let reached = Arc::new(tokio::sync::Notify::new());
        let mutation = {
            let state = Arc::clone(&state);
            let remote_committed = Arc::clone(&remote_committed);
            let reached = Arc::clone(&reached);
            tokio::spawn(async move {
                let (mut operation, _cache) = state.begin_mutation().await.unwrap();
                operation.remote_started();
                remote_committed.store(true, Ordering::SeqCst);
                reached.notify_one();
                std::future::pending::<()>().await;
                operation.remote_resolved();
            })
        };
        reached.notified().await;
        assert!(remote_committed.load(Ordering::SeqCst));

        mutation.abort();
        assert!(mutation.await.unwrap_err().is_cancelled());
        assert!(state.snapshot().is_err());
        assert!(state.begin_mutation().await.is_err());
    }
}

use std::sync::{Arc, Condvar, LockResult, Mutex, MutexGuard};

use retune_core::model::Library;

use crate::{
    restore_latch::RestoreMutationState,
    store::{FsOverlayStore, OverlayStore},
};

#[derive(Clone)]
pub(crate) struct LibraryState {
    current: Arc<Mutex<Library>>,
    store: Arc<FsOverlayStore>,
    write_gate: Arc<Mutex<()>>,
    transaction: Arc<LibraryTransactionState>,
    restore_mutations: Arc<RestoreMutationState>,
}

#[derive(Clone)]
pub(crate) struct LibraryOwner {
    library: Arc<Mutex<Library>>,
    store: Arc<FsOverlayStore>,
    write_gate: Arc<Mutex<()>>,
    transaction: Arc<LibraryTransactionState>,
    restore_mutations: Arc<RestoreMutationState>,
}

pub(crate) struct LibraryTransactionState {
    active: Mutex<bool>,
    changed: Condvar,
}

impl Default for LibraryTransactionState {
    fn default() -> Self {
        Self {
            active: Mutex::new(false),
            changed: Condvar::new(),
        }
    }
}

pub(crate) struct LibraryTransactionGuard {
    state: Arc<LibraryTransactionState>,
}

impl Drop for LibraryTransactionGuard {
    fn drop(&mut self) {
        let mut active = self
            .state
            .active
            .lock()
            .expect("library transaction mutex poisoned");
        *active = false;
        self.state.changed.notify_all();
    }
}

pub(crate) struct LibraryRestore<'a> {
    state: &'a LibraryState,
    _write_gate: MutexGuard<'a, ()>,
    _transaction: LibraryTransactionGuard,
}

impl LibraryState {
    #[cfg(test)]
    pub(crate) fn new(current: Library, store: FsOverlayStore) -> Self {
        Self::new_with_restore_state(current, store, Arc::new(RestoreMutationState::default()))
    }

    pub(crate) fn new_with_restore_state(
        current: Library,
        store: FsOverlayStore,
        restore_mutations: Arc<RestoreMutationState>,
    ) -> Self {
        Self {
            current: Arc::new(Mutex::new(current)),
            store: Arc::new(store),
            write_gate: Arc::new(Mutex::new(())),
            transaction: Arc::new(LibraryTransactionState::default()),
            restore_mutations,
        }
    }

    // Temporary compatibility for read-only callers while the shell modules move
    // behind this owner one at a time.
    pub(crate) fn lock(&self) -> LockResult<MutexGuard<'_, Library>> {
        self.current.lock()
    }

    pub(crate) fn snapshot(&self) -> Library {
        self.current.lock().expect("library mutex poisoned").clone()
    }

    #[cfg(test)]
    pub(crate) fn arm_save(&self, hook: Arc<crate::store::SaveHook>) {
        self.store.arm_save(hook);
    }

    pub(crate) fn owner(&self) -> LibraryOwner {
        LibraryOwner {
            library: Arc::clone(&self.current),
            store: Arc::clone(&self.store),
            write_gate: Arc::clone(&self.write_gate),
            transaction: Arc::clone(&self.transaction),
            restore_mutations: Arc::clone(&self.restore_mutations),
        }
    }

    pub(crate) fn begin_transaction(&self) -> Result<LibraryTransactionGuard, String> {
        begin_transaction(&self.transaction, &self.restore_mutations)
    }

    pub(crate) fn mutate<T>(
        &self,
        mutation: impl FnOnce(&mut Library) -> Result<T, String>,
    ) -> Result<T, String> {
        self.owner().mutate(mutation)
    }

    pub(crate) async fn mutate_async<T: Send + 'static>(
        &self,
        mutation: impl FnOnce(&mut Library) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        self.owner().mutate_async(mutation).await
    }

    #[cfg(test)]
    pub(crate) fn mutate_in_transaction<T>(
        &self,
        mutation: impl FnOnce(&mut Library) -> Result<T, String>,
    ) -> Result<T, String> {
        let write_gate = self.write_gate.lock().expect("library write gate poisoned");
        self.restore_mutations.ensure_allowed()?;
        let mut next = self.current.lock().expect("library mutex poisoned").clone();
        let value = mutation(&mut next)?;
        self.store.save(&next).map_err(|error| error.to_string())?;
        *self.current.lock().expect("library mutex poisoned") = next;
        drop(write_gate);
        Ok(value)
    }

    pub(crate) async fn replace_in_transaction<T: Send + 'static>(
        &self,
        transaction: LibraryTransactionGuard,
        next: Library,
        owner: T,
    ) -> Result<(LibraryTransactionGuard, T), String> {
        let current = Arc::clone(&self.current);
        let store = Arc::clone(&self.store);
        let write_gate = Arc::clone(&self.write_gate);
        let restore_mutations = Arc::clone(&self.restore_mutations);
        tauri::async_runtime::spawn(async move {
            tauri::async_runtime::spawn_blocking(move || {
                let _write_gate = write_gate.lock().expect("library write gate poisoned");
                restore_mutations.ensure_allowed()?;
                store.save(&next).map_err(|error| error.to_string())?;
                *current.lock().expect("library mutex poisoned") = next;
                Ok::<_, String>((transaction, owner))
            })
            .await
            .map_err(|error| error.to_string())?
        })
        .await
        .map_err(|error| error.to_string())?
    }

    pub(crate) fn install_in_transaction(
        &self,
        _transaction: &LibraryTransactionGuard,
        next: Library,
    ) {
        *self.current.lock().expect("library mutex poisoned") = next;
    }

    pub(crate) fn record_play(&self, uri: &str, played_at: u64) -> Result<bool, String> {
        record_play_with(
            self.store.as_ref(),
            &self.current,
            &self.write_gate,
            &self.transaction,
            &self.restore_mutations,
            uri,
            played_at,
        )
    }

    pub(crate) fn begin_restore(&self) -> Result<LibraryRestore<'_>, String> {
        let transaction = self.begin_transaction()?;
        let write_gate = self.write_gate.lock().expect("library write gate poisoned");
        self.restore_mutations.ensure_allowed()?;
        Ok(LibraryRestore {
            state: self,
            _write_gate: write_gate,
            _transaction: transaction,
        })
    }
}

impl LibraryOwner {
    pub(crate) fn read<T>(&self, read: impl FnOnce(&Library) -> T) -> Result<T, String> {
        let current = self.library.lock().expect("library mutex poisoned");
        Ok(read(&current))
    }

    pub(crate) fn mutate<T>(
        &self,
        mutation: impl FnOnce(&mut Library) -> Result<T, String>,
    ) -> Result<T, String> {
        let _transaction_state = wait_for_transaction(self.transaction.as_ref())?;
        let write_gate = self.write_gate.lock().expect("library write gate poisoned");
        #[cfg(test)]
        self.restore_mutations.after_wait();
        self.restore_mutations.ensure_allowed()?;
        let mut next = self.library.lock().expect("library mutex poisoned").clone();
        let value = mutation(&mut next)?;
        self.store.save(&next).map_err(|error| error.to_string())?;
        *self.library.lock().expect("library mutex poisoned") = next;
        drop(write_gate);
        Ok(value)
    }

    pub(crate) async fn mutate_async<R: Send + 'static>(
        &self,
        mutation: impl FnOnce(&mut Library) -> Result<R, String> + Send + 'static,
    ) -> Result<R, String> {
        self.mutate_async_owned(mutation, ())
            .await
            .map(|(value, ())| value)
    }

    pub(crate) async fn mutate_async_owned<R: Send + 'static, O: Send + 'static>(
        &self,
        mutation: impl FnOnce(&mut Library) -> Result<R, String> + Send + 'static,
        owner: O,
    ) -> Result<(R, O), String> {
        let library = Arc::clone(&self.library);
        let store = Arc::clone(&self.store);
        let write_gate = Arc::clone(&self.write_gate);
        let transaction = Arc::clone(&self.transaction);
        let restore_mutations = Arc::clone(&self.restore_mutations);
        tauri::async_runtime::spawn(async move {
            tauri::async_runtime::spawn_blocking(move || {
                let _transaction_state = wait_for_transaction(&transaction)?;
                let _write_gate = write_gate.lock().expect("library write gate poisoned");
                restore_mutations.ensure_allowed()?;
                let mut next = library.lock().expect("library mutex poisoned").clone();
                let value = mutation(&mut next)?;
                store.save(&next).map_err(|error| error.to_string())?;
                *library.lock().expect("library mutex poisoned") = next;
                Ok::<_, String>((value, owner))
            })
            .await
            .map_err(|error| error.to_string())?
        })
        .await
        .map_err(|error| error.to_string())?
    }
}

impl LibraryRestore<'_> {
    pub(crate) fn snapshot(&self) -> Library {
        self.state
            .current
            .lock()
            .expect("library mutex poisoned")
            .clone()
    }

    pub(crate) fn replace(&self, next: Library) -> Result<(), String> {
        self.state
            .store
            .save(&next)
            .map_err(|error| error.to_string())?;
        *self.state.current.lock().expect("library mutex poisoned") = next;
        Ok(())
    }

    pub(crate) fn install_recovered(&self, next: Library) {
        *self.state.current.lock().expect("library mutex poisoned") = next;
    }
}

fn begin_transaction(
    state: &Arc<LibraryTransactionState>,
    restore_mutations: &RestoreMutationState,
) -> Result<LibraryTransactionGuard, String> {
    let mut active = state
        .active
        .lock()
        .map_err(|_| "library transaction mutex poisoned".to_string())?;
    if *active {
        return Err("Another library transaction is already applying.".to_string());
    }
    restore_mutations.ensure_allowed()?;
    *active = true;
    drop(active);
    Ok(LibraryTransactionGuard {
        state: Arc::clone(state),
    })
}

fn wait_for_transaction(state: &LibraryTransactionState) -> Result<MutexGuard<'_, bool>, String> {
    let mut active = state
        .active
        .lock()
        .map_err(|_| "library transaction mutex poisoned".to_string())?;
    while *active {
        active = state
            .changed
            .wait(active)
            .map_err(|_| "library transaction mutex poisoned".to_string())?;
    }
    Ok(active)
}

#[cfg(test)]
pub(crate) fn commit_library_candidate(
    store: &impl OverlayStore,
    current: &mut Library,
    next: Library,
) -> Result<(), String> {
    store.save(&next).map_err(|error| error.to_string())?;
    *current = next;
    Ok(())
}

pub(crate) fn record_play_with(
    store: &impl OverlayStore,
    library: &Mutex<Library>,
    write_gate: &Mutex<()>,
    transaction: &LibraryTransactionState,
    restore_mutations: &RestoreMutationState,
    uri: &str,
    played_at: u64,
) -> Result<bool, String> {
    let _transaction_state = wait_for_transaction(transaction)?;
    let _write_gate = write_gate.lock().expect("library write gate poisoned");
    restore_mutations.ensure_allowed()?;
    let mut next = library.lock().expect("library mutex poisoned").clone();
    if !next.record_play(uri, played_at) {
        return Ok(false);
    }
    store.save(&next).map_err(|error| error.to_string())?;
    *library.lock().expect("library mutex poisoned") = next;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar,
    };

    use retune_core::model::NewTrack;

    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn transaction_save_does_not_block_tokio_and_publishes_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let store = FsOverlayStore::new(directory.path());
        let control = store.clone();
        let state = Arc::new(LibraryState::new(Library::new(), store));
        let hook = crate::store::SaveHook::new(false);
        control.arm_save(Arc::clone(&hook));
        let transaction = state.begin_transaction().unwrap();
        let mut next = Library::new();
        next.add(NewTrack {
            uri: "file:///after.mp3".into(),
            ..NewTrack::default()
        });
        let save = {
            let state = Arc::clone(&state);
            tokio::spawn(async move { state.replace_in_transaction(transaction, next, ()).await })
        };
        while !hook.is_reached() {
            tokio::task::yield_now().await;
        }
        hook.wait_until_reached();

        let unrelated = tokio::spawn(async { 7 });
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_millis(50), unrelated)
                .await
                .unwrap()
                .unwrap(),
            7
        );
        assert!(!save.is_finished());
        assert!(state.snapshot().tracks().is_empty());

        hook.release();
        let (transaction, ()) = save.await.unwrap().unwrap();
        assert_eq!(state.snapshot().tracks().len(), 1);
        assert_eq!(control.load().unwrap(), Some(state.snapshot()));
        let mut queued = {
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                state
                    .mutate_async(|library| {
                        library.add(NewTrack {
                            uri: "file:///queued.mp3".into(),
                            ..NewTrack::default()
                        });
                        Ok(())
                    })
                    .await
            })
        };
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut queued)
                .await
                .is_err()
        );
        drop(transaction);
        queued.await.unwrap().unwrap();
        assert_eq!(state.snapshot().tracks().len(), 2);
    }

    #[test]
    fn restore_save_does_not_hold_library_read_mutex() {
        let directory = tempfile::tempdir().unwrap();
        let store = FsOverlayStore::new(directory.path());
        let control = store.clone();
        let state = LibraryState::new(Library::new(), store);
        let restore = state.begin_restore().unwrap();
        let hook = crate::store::SaveHook::new(false);
        control.arm_save(Arc::clone(&hook));
        let mut next = Library::new();
        next.add(NewTrack {
            uri: "file:///after.mp3".into(),
            ..NewTrack::default()
        });

        std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                hook.wait_until_reached();
                assert!(state.snapshot().tracks().is_empty());
                hook.release();
            });
            restore.replace(next).unwrap();
            reader.join().unwrap();
        });
        assert_eq!(state.snapshot().tracks().len(), 1);
    }

    #[test]
    fn queued_mutation_checks_restore_latch_after_transaction_wait() {
        let directory = tempfile::tempdir().unwrap();
        let restore_mutations = Arc::new(RestoreMutationState::default());
        let state = Arc::new(LibraryState::new_with_restore_state(
            Library::new(),
            FsOverlayStore::new(directory.path()),
            Arc::clone(&restore_mutations),
        ));
        let restore = state.begin_restore().unwrap();
        let hook = restore_mutations.arm_after_wait();
        let mutated = Arc::new(AtomicBool::new(false));
        let worker = {
            let state = Arc::clone(&state);
            let mutated = Arc::clone(&mutated);
            std::thread::spawn(move || {
                state.mutate(|_| {
                    mutated.store(true, Ordering::SeqCst);
                    Ok(())
                })
            })
        };

        drop(restore);
        hook.wait_until_reached();
        restore_mutations.mark_recovery_required();
        hook.release();

        assert!(worker.join().unwrap().is_err());
        assert!(!mutated.load(Ordering::SeqCst));
        assert!(FsOverlayStore::new(directory.path())
            .load()
            .unwrap()
            .is_none());
    }

    struct BlockingStore {
        state: Mutex<(bool, bool)>,
        changed: Condvar,
    }

    impl OverlayStore for BlockingStore {
        fn load(&self) -> crate::store::StoreResult<Option<Library>> {
            Ok(None)
        }

        fn save(&self, _: &Library) -> crate::store::StoreResult<()> {
            let mut state = self.state.lock().unwrap();
            state.0 = true;
            self.changed.notify_all();
            while !state.1 {
                state = self.changed.wait(state).unwrap();
            }
            Ok(())
        }
    }

    #[test]
    fn persistence_does_not_hold_the_library_read_guard() {
        let mut current = Library::new();
        current.add(NewTrack {
            uri: "file:///song.mp3".into(),
            ..NewTrack::default()
        });
        let library = Arc::new(Mutex::new(current));
        let store = Arc::new(BlockingStore {
            state: Mutex::new((false, false)),
            changed: Condvar::new(),
        });
        let worker = {
            let library = Arc::clone(&library);
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                record_play_with(
                    store.as_ref(),
                    &library,
                    &Mutex::new(()),
                    &LibraryTransactionState::default(),
                    &RestoreMutationState::default(),
                    "file:///song.mp3",
                    1,
                )
            })
        };
        let mut state = store.state.lock().unwrap();
        while !state.0 {
            state = store.changed.wait(state).unwrap();
        }
        assert!(
            library.try_lock().is_ok(),
            "save must not retain the library guard"
        );
        state.1 = true;
        store.changed.notify_all();
        drop(state);
        assert!(worker.join().unwrap().unwrap());
    }
}

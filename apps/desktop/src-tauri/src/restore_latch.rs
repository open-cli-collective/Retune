use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
use std::sync::{Arc, Barrier, Mutex};

const RECOVERY_REQUIRED: &str =
    "Restore recovery is required before changes can be saved. Restart Retune to recover.";

#[derive(Default)]
pub(crate) struct RestoreMutationState {
    recovery_required: AtomicBool,
    #[cfg(test)]
    after_wait: Mutex<Option<Arc<AfterWaitHook>>>,
}

#[cfg(test)]
pub(crate) struct AfterWaitHook {
    reached: Barrier,
    release: Barrier,
}

impl RestoreMutationState {
    pub(crate) fn ensure_allowed(&self) -> Result<(), String> {
        if self.recovery_required.load(Ordering::Acquire) {
            Err(RECOVERY_REQUIRED.into())
        } else {
            Ok(())
        }
    }

    pub(crate) fn mark_recovery_required(&self) {
        self.recovery_required.store(true, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn arm_after_wait(&self) -> Arc<AfterWaitHook> {
        let hook = Arc::new(AfterWaitHook {
            reached: Barrier::new(2),
            release: Barrier::new(2),
        });
        *self.after_wait.lock().unwrap() = Some(Arc::clone(&hook));
        hook
    }

    #[cfg(test)]
    pub(crate) fn after_wait(&self) {
        if let Some(hook) = self.after_wait.lock().unwrap().take() {
            hook.reached.wait();
            hook.release.wait();
        }
    }
}

#[cfg(test)]
impl AfterWaitHook {
    pub(crate) fn wait_until_reached(&self) {
        self.reached.wait();
    }

    pub(crate) fn release(&self) {
        self.release.wait();
    }
}

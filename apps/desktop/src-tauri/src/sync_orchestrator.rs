use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct SyncOrchestrator {
    gate: Arc<Mutex<Gate>>,
    retry: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[derive(Default)]
struct Gate {
    running: bool,
    rerun_pending: bool,
}

pub struct SyncRun {
    gate: Arc<Mutex<Gate>>,
    finished: bool,
}

impl SyncRun {
    pub fn finish(&mut self) -> bool {
        let mut gate = self.gate.lock().expect("sync gate mutex poisoned");
        if gate.rerun_pending {
            gate.rerun_pending = false;
            true
        } else {
            gate.running = false;
            self.finished = true;
            false
        }
    }
}

impl Drop for SyncRun {
    fn drop(&mut self) {
        if !self.finished {
            let mut gate = self.gate.lock().expect("sync gate mutex poisoned");
            gate.running = false;
            gate.rerun_pending = false;
        }
    }
}

impl SyncOrchestrator {
    pub fn running(&self) -> bool {
        self.gate.lock().expect("sync gate mutex poisoned").running
    }

    pub fn begin(&self) -> Option<SyncRun> {
        let mut gate = self.gate.lock().expect("sync gate mutex poisoned");
        if gate.running {
            gate.rerun_pending = true;
            None
        } else {
            gate.running = true;
            Some(SyncRun {
                gate: Arc::clone(&self.gate),
                finished: false,
            })
        }
    }

    pub fn cancel_retry(&self) {
        if let Some(retry) = self.retry.lock().expect("sync retry mutex poisoned").take() {
            retry.abort();
        }
    }

    pub fn replace_retry(&self, retry: tokio::task::JoinHandle<()>) {
        self.cancel_retry();
        *self.retry.lock().expect("sync retry mutex poisoned") = Some(retry);
    }

    pub fn retry_fired(&self) {
        self.retry.lock().expect("sync retry mutex poisoned").take();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use retune_spotify::client::{fake_client, Response};

    use super::*;

    #[tokio::test]
    async fn concurrent_requests_coalesce_into_one_rerun() {
        let orchestrator = Arc::new(SyncOrchestrator::default());
        let runs = Arc::new(AtomicUsize::new(0));
        let client = Arc::new(fake_client(
            [
                Response::json(200, serde_json::json!({"items": [], "next": null})),
                Response::json(200, serde_json::json!({"items": [], "next": null})),
            ],
            "",
        ));
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());

        let runner = {
            let orchestrator = Arc::clone(&orchestrator);
            let runs = Arc::clone(&runs);
            let client = Arc::clone(&client);
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            tokio::spawn(async move {
                let mut run = orchestrator.begin().unwrap();
                loop {
                    client.saved_tracks(0, 50).await.unwrap();
                    runs.fetch_add(1, Ordering::Relaxed);
                    started.notify_one();
                    if runs.load(Ordering::Relaxed) == 1 {
                        release.notified().await;
                    }
                    if !run.finish() {
                        break;
                    }
                }
            })
        };

        started.notified().await;
        assert!(orchestrator.begin().is_none());
        assert!(orchestrator.begin().is_none());
        release.notify_one();
        runner.await.unwrap();

        assert_eq!(runs.load(Ordering::Relaxed), 2);
        assert_eq!(client.transport().requests().len(), 2);
    }

    #[tokio::test]
    async fn aborted_or_panicked_runs_release_the_gate() {
        let orchestrator = Arc::new(SyncOrchestrator::default());
        for panic in [false, true] {
            let task_orchestrator = Arc::clone(&orchestrator);
            let started = Arc::new(tokio::sync::Notify::new());
            let task_started = Arc::clone(&started);
            let task = tokio::spawn(async move {
                let _run = task_orchestrator.begin().unwrap();
                task_started.notify_one();
                if panic {
                    panic!("runner panic");
                }
                std::future::pending::<()>().await;
            });
            started.notified().await;
            if panic {
                assert!(task.await.unwrap_err().is_panic());
            } else {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            }
            assert!(orchestrator.begin().is_some());
        }
    }

    #[tokio::test]
    async fn cancelling_a_retry_aborts_the_owned_task() {
        struct Dropped(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let orchestrator = SyncOrchestrator::default();
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task_dropped = Arc::clone(&dropped);
        orchestrator.replace_retry(tokio::spawn(async move {
            let _dropped = Dropped(task_dropped);
            std::future::pending::<()>().await;
        }));
        tokio::task::yield_now().await;

        orchestrator.cancel_retry();
        tokio::time::timeout(std::time::Duration::from_millis(100), async {
            while !dropped.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelling a retry must stop its owned task promptly");
    }
}

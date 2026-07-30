use std::sync::Mutex;

#[derive(Default)]
pub struct SyncOrchestrator {
    gate: Mutex<Gate>,
    retry: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[derive(Default)]
struct Gate {
    running: bool,
    rerun_pending: bool,
}

impl SyncOrchestrator {
    pub fn begin(&self) -> bool {
        let mut gate = self.gate.lock().expect("sync gate mutex poisoned");
        if gate.running {
            gate.rerun_pending = true;
            false
        } else {
            gate.running = true;
            true
        }
    }

    pub fn finish(&self) -> bool {
        let mut gate = self.gate.lock().expect("sync gate mutex poisoned");
        if gate.rerun_pending {
            gate.rerun_pending = false;
            true
        } else {
            gate.running = false;
            false
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
                assert!(orchestrator.begin());
                loop {
                    client.saved_tracks(0, 50).await.unwrap();
                    runs.fetch_add(1, Ordering::Relaxed);
                    started.notify_one();
                    if runs.load(Ordering::Relaxed) == 1 {
                        release.notified().await;
                    }
                    if !orchestrator.finish() {
                        break;
                    }
                }
            })
        };

        started.notified().await;
        assert!(!orchestrator.begin());
        assert!(!orchestrator.begin());
        release.notify_one();
        runner.await.unwrap();

        assert_eq!(runs.load(Ordering::Relaxed), 2);
        assert_eq!(client.transport().requests().len(), 2);
    }
}

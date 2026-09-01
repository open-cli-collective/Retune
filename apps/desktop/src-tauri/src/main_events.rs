use std::{collections::VecDeque, sync::Mutex};

use serde::Serialize;
use tauri::ipc::Channel;

use crate::{
    localfiles::ImportSummary,
    playback::{PlaybackAuthorizationPrompt, PlayerStateEvent},
};

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", content = "payload", rename_all = "camelCase")]
pub(crate) enum MainEvent {
    PlayerState(PlayerStateEvent),
    PlaybackAuthorizationRequired(PlaybackAuthorizationPrompt),
    OperationError(String),
    OperationRecovered,
    LocalImportComplete(ImportSummary),
    StartupNotice(String),
}

impl MainEvent {
    fn kind(&self) -> Option<MainEventKind> {
        match self {
            Self::PlayerState(_) => Some(MainEventKind::PlayerState),
            Self::PlaybackAuthorizationRequired(_) => {
                Some(MainEventKind::PlaybackAuthorizationRequired)
            }
            Self::OperationError(_) => Some(MainEventKind::OperationError),
            Self::OperationRecovered => None,
            Self::LocalImportComplete(_) => Some(MainEventKind::LocalImportComplete),
            Self::StartupNotice(_) => Some(MainEventKind::StartupNotice),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MainEventKind {
    PlayerState,
    PlaybackAuthorizationRequired,
    OperationError,
    LocalImportComplete,
    StartupNotice,
}

const MAX_PENDING_MAIN_EVENTS: usize = 5;

#[derive(Default)]
struct Subscription {
    generation: u64,
    channel: Option<(u64, Channel<MainEvent>)>,
    pending: VecDeque<MainEvent>,
}

#[derive(Default)]
pub(crate) struct MainEventSink(Mutex<Subscription>);

impl MainEventSink {
    pub(crate) fn new(startup_notice: Option<String>) -> Self {
        let sink = Self::default();
        if let Some(notice) = startup_notice {
            sink.send(MainEvent::StartupNotice(notice))
                .expect("retaining a startup notice cannot fail");
        }
        sink
    }

    pub(crate) fn subscribe(&self, channel: Channel<MainEvent>) -> Result<u64, String> {
        let mut subscription = self.0.lock().expect("main event sink mutex poisoned");
        subscription.generation = subscription
            .generation
            .checked_add(1)
            .ok_or("main event subscription generation exhausted")?;
        let generation = subscription.generation;
        subscription.channel = Some((generation, channel));
        let mut pending = std::mem::take(&mut subscription.pending);
        while let Some(event) = pending.pop_front() {
            if let Err(error) = Self::send_locked(&mut subscription, event.clone()) {
                Self::retain_locked(&mut subscription, event);
                while let Some(remaining) = pending.pop_front() {
                    Self::retain_locked(&mut subscription, remaining);
                }
                return Err(error.to_string());
            }
        }
        Ok(generation)
    }

    pub(crate) fn unsubscribe(&self, generation: u64) {
        let mut subscription = self.0.lock().expect("main event sink mutex poisoned");
        if subscription
            .channel
            .as_ref()
            .is_some_and(|(active, _)| *active == generation)
        {
            subscription.channel = None;
        }
    }

    pub(crate) fn send(&self, event: MainEvent) -> tauri::Result<()> {
        let mut subscription = self.0.lock().expect("main event sink mutex poisoned");
        Self::send_locked(&mut subscription, event)
    }

    fn send_locked(subscription: &mut Subscription, event: MainEvent) -> tauri::Result<()> {
        let Some((_, channel)) = &subscription.channel else {
            Self::retain_locked(subscription, event);
            return Ok(());
        };
        if let Err(error) = channel.send(event.clone()) {
            subscription.channel = None;
            Self::retain_locked(subscription, event);
            return Err(error);
        }
        Ok(())
    }

    fn retain_locked(subscription: &mut Subscription, event: MainEvent) {
        let Some(kind) = event.kind() else {
            subscription
                .pending
                .retain(|pending| pending.kind() != Some(MainEventKind::OperationError));
            return;
        };
        subscription
            .pending
            .retain(|pending| pending.kind() != Some(kind));
        subscription.pending.push_back(event);
        debug_assert!(subscription.pending.len() <= MAX_PENDING_MAIN_EVENTS);
    }
}

#[tauri::command]
pub(crate) fn subscribe_main_events(
    state: tauri::State<'_, crate::AppState>,
    channel: Channel<MainEvent>,
) -> Result<u64, String> {
    state.main_events.subscribe(channel)
}

#[tauri::command]
pub(crate) fn unsubscribe_main_events(state: tauri::State<'_, crate::AppState>, generation: u64) {
    state.main_events.unsubscribe(generation);
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use super::*;

    fn counting_channel(count: Arc<AtomicUsize>) -> Channel<MainEvent> {
        Channel::new(move |_| {
            count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
    }

    #[test]
    fn main_events_are_externally_tagged() {
        assert_eq!(
            serde_json::to_value(MainEvent::OperationError("nope".into())).unwrap(),
            serde_json::json!({ "type": "operationError", "payload": "nope" })
        );
        assert_eq!(
            serde_json::to_value(MainEvent::OperationRecovered).unwrap(),
            serde_json::json!({ "type": "operationRecovered" })
        );
    }

    #[test]
    fn stale_unsubscribe_does_not_clear_replacement_channel() {
        let sink = MainEventSink::default();
        let first = Arc::new(AtomicUsize::new(0));
        let second = Arc::new(AtomicUsize::new(0));
        let stale_generation = sink
            .subscribe(counting_channel(Arc::clone(&first)))
            .unwrap();
        let active_generation = sink
            .subscribe(counting_channel(Arc::clone(&second)))
            .unwrap();

        sink.unsubscribe(stale_generation);
        sink.send(MainEvent::OperationError("current".into()))
            .unwrap();

        assert_eq!(first.load(Ordering::Relaxed), 0);
        assert_eq!(second.load(Ordering::Relaxed), 1);
        sink.unsubscribe(active_generation);
        sink.send(MainEvent::OperationError("dropped".into()))
            .unwrap();
        assert_eq!(second.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn pending_events_keep_only_the_latest_value_per_closed_variant() {
        let sink = MainEventSink::new(Some("recovery".into()));
        for index in 0..100 {
            sink.send(MainEvent::StartupNotice(format!("notice {index}")))
                .unwrap();
            sink.send(MainEvent::OperationError(format!("error {index}")))
                .unwrap();
        }
        assert_eq!(sink.0.lock().unwrap().pending.len(), 2);
        // Five payload-bearing variants are the only retainable keys;
        // recovery is an action that removes the pending error.
        assert_eq!(MAX_PENDING_MAIN_EVENTS, 5);
        assert!(sink.0.lock().unwrap().pending.len() <= MAX_PENDING_MAIN_EVENTS);
        let count = Arc::new(AtomicUsize::new(0));

        sink.subscribe(counting_channel(Arc::clone(&count)))
            .unwrap();

        assert_eq!(count.load(Ordering::Relaxed), 2);
        assert!(sink.0.lock().unwrap().pending.is_empty());
    }

    #[test]
    fn recovery_clears_an_error_that_was_never_observed() {
        let sink = MainEventSink::default();
        sink.send(MainEvent::OperationError("temporary".into()))
            .unwrap();

        sink.send(MainEvent::OperationRecovered).unwrap();

        assert!(sink.0.lock().unwrap().pending.is_empty());
    }
}

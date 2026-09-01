use std::sync::Arc;

use serde::Serialize;
use tauri::{Emitter, Manager};

use crate::{
    emit_main,
    playback::{AudioSettings, PlaybackBackend},
    provider_from, spotify_provider,
    store::{LastFmScrobblingProfile, Settings, SettingsPatch, SettingsState, SettingsView, Theme},
    unix_now, AppState,
};

#[tauri::command]
pub(super) fn get_settings(state: tauri::State<'_, AppState>) -> SettingsView {
    SettingsView::from(&state.settings.snapshot())
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Appearance {
    theme: Theme,
}

#[tauri::command]
pub(super) fn get_appearance(state: tauri::State<'_, AppState>) -> Appearance {
    Appearance {
        theme: state.settings.snapshot().theme,
    }
}

pub(crate) fn emit_settings_changed(
    app: &tauri::AppHandle,
    previous: &Settings,
    current: &Settings,
) -> Result<(), String> {
    emit_main(app, "settings-changed", ()).map_err(|error| error.to_string())?;
    if previous.theme != current.theme {
        app.emit_to(
            "lastfm-importer",
            "appearance-changed",
            Appearance {
                theme: current.theme,
            },
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

struct SettingsCommit {
    client_id_changed: bool,
    audio_changed: bool,
    settings: Settings,
}

async fn persist_settings_patch(
    settings: &SettingsState,
    patch: SettingsPatch,
    lastfm_username: Option<&str>,
    committed: impl FnOnce(&Settings, &Settings) -> Result<(), String>,
) -> Result<SettingsCommit, String> {
    patch.validate()?;
    let ((client_id_changed, audio_changed), settings) = settings
        .mutate(
            |settings| {
                let previous_client_id = settings.spotify_client_id.clone();
                let previous_audio = (
                    settings.streaming_bitrate,
                    settings.normalize_volume,
                    settings.gapless,
                );
                patch.apply(settings);
                if settings.lastfm_scrobbling {
                    if let Some(username) = lastfm_username {
                        reconcile_lastfm_scrobbling_profile(settings, username, unix_now());
                    }
                }
                Ok((
                    previous_client_id != settings.spotify_client_id,
                    previous_audio
                        != (
                            settings.streaming_bitrate,
                            settings.normalize_volume,
                            settings.gapless,
                        ),
                ))
            },
            committed,
        )
        .await?;
    Ok(SettingsCommit {
        client_id_changed,
        audio_changed,
        settings,
    })
}

#[tauri::command]
pub(super) async fn update_settings(
    app: tauri::AppHandle,
    patch: SettingsPatch,
) -> Result<(), String> {
    patch.validate()?;
    let state = app.state::<AppState>();
    let lastfm_username = if patch.lastfm_scrobbling == Some(true) {
        state.lastfm.state().await.username
    } else {
        None
    };
    let SettingsCommit {
        client_id_changed,
        audio_changed,
        settings,
    } = persist_settings_patch(
        &state.settings,
        patch,
        lastfm_username.as_deref(),
        |previous, current| emit_settings_changed(&app, previous, current),
    )
    .await?;
    state
        .playback
        .set_requested_backend(settings.playback_backend);
    // Local activation is intentionally lazy: playback owns authorization
    // prompts, and unrelated preference saves must remain offline-safe.
    match settings.playback_backend {
        PlaybackBackend::Connect if state.playback.is_local_active().await => {
            state.playback.switch_to_connect().await;
        }
        PlaybackBackend::Connect | PlaybackBackend::Local => {}
    }
    state.lastfm.set_enabled(settings.lastfm_scrobbling).await;
    state
        .playback
        .set_play_threshold_percent(settings.play_threshold_percent)
        .await;
    if let Some(menu_checks) = &state.menu_checks {
        menu_checks.sync(&settings).map_err(|error| {
            format!("Settings were saved, but menu state could not be updated: {error}")
        })?;
    }
    if audio_changed {
        state.playback.set_audio(AudioSettings {
            bitrate: settings.streaming_bitrate,
            normalize: settings.normalize_volume,
            gapless: settings.gapless,
        });
        if state.playback.is_local_active().await {
            state.playback.invalidate_local().await;
            if let Ok(client) = provider_from(&state) {
                if let Err(error) = state.playback.revalidate(client.as_ref()).await {
                    log::warn!("Audio settings applied; session recreation deferred: {error}");
                }
            }
        }
    }
    if client_id_changed {
        state.spotify_session.invalidate().await;
        *state.spotify.lock().expect("spotify mutex poisoned") = spotify_provider(
            &settings.spotify_client_id,
            Arc::clone(&state.token_store),
            Arc::clone(&state.spotify_catalog),
        )
        .map_err(|error| {
            format!("Settings were saved, but Spotify could not be reconfigured: {error}")
        })?;
    }
    Ok(())
}

pub(super) async fn set_lastfm_scrobbling(
    app: &tauri::AppHandle,
    enabled: bool,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let username = if enabled {
        state.lastfm.state().await.username
    } else {
        None
    };
    state
        .settings
        .mutate(
            |settings| {
                settings.lastfm_scrobbling = enabled;
                if let Some(username) = username.as_deref() {
                    reconcile_lastfm_scrobbling_profile(settings, username, unix_now());
                }
                Ok(())
            },
            |previous, current| emit_settings_changed(app, previous, current),
        )
        .await?;
    Ok(())
}

pub(crate) async fn set_auto_connect(
    app: &tauri::AppHandle,
    auto_connect: bool,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    state
        .settings
        .mutate(
            |settings| {
                settings.auto_connect = auto_connect;
                Ok(())
            },
            |previous, current| emit_settings_changed(app, previous, current),
        )
        .await?;
    Ok(())
}

pub(super) fn reconcile_lastfm_scrobbling_profile(
    settings: &mut Settings,
    username: &str,
    now: u64,
) {
    if username.trim().is_empty()
        || settings
            .lastfm_scrobbling_profile
            .as_ref()
            .is_some_and(|profile| profile.username == username)
    {
        return;
    }
    settings.lastfm_scrobbling_profile = Some(LastFmScrobblingProfile {
        username: username.to_owned(),
        started_at: now,
    });
}

pub(crate) async fn history_cutoff_for_import(
    settings: &SettingsState,
    username: &str,
) -> Result<u64, String> {
    let (cutoff, _) = settings
        .mutate(
            |settings| {
                reconcile_lastfm_scrobbling_profile(settings, username, unix_now());
                settings
                    .lastfm_scrobbling_profile
                    .as_ref()
                    .map(|profile| profile.started_at)
                    .ok_or_else(|| "Could not establish the Last.fm history cutoff.".to_string())
            },
            |_, _| Ok(()),
        )
        .await?;
    Ok(cutoff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{FsSettingsStore, SaveHook};
    use std::{
        collections::BTreeMap,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };

    #[test]
    fn history_cutoff_is_persisted_for_the_account() {
        let temp = tempfile::tempdir().unwrap();
        let settings = SettingsState::new(Settings::default(), FsSettingsStore::new(temp.path()));

        let cutoff =
            tauri::async_runtime::block_on(history_cutoff_for_import(&settings, "listener"))
                .unwrap();

        assert!(cutoff > 0);
        assert_eq!(
            tauri::async_runtime::block_on(history_cutoff_for_import(&settings, "listener")),
            Ok(cutoff)
        );
        assert_eq!(
            settings.snapshot().lastfm_scrobbling_profile,
            Some(LastFmScrobblingProfile {
                username: "listener".into(),
                started_at: cutoff,
            })
        );
    }

    #[test]
    fn oversized_settings_patch_is_rejected_before_store_or_memory_changes() {
        fn assert_rejected_without_store(patch: SettingsPatch) {
            let temp = tempfile::tempdir().unwrap();
            let store = FsSettingsStore::new(temp.path());
            let state = SettingsState::new(Settings::default(), store.clone());
            let result = tauri::async_runtime::block_on(state.mutate(
                move |settings| {
                    patch.validate()?;
                    patch.apply(settings);
                    Ok(())
                },
                |_, _| Ok(()),
            ));
            assert!(result.is_err());
            assert_eq!(state.snapshot(), Settings::default());
            assert_eq!(store.load().unwrap(), None);
        }

        assert_rejected_without_store(SettingsPatch {
            spotify_client_id: Some("x".repeat(4 * 1024 + 1)),
            ..SettingsPatch::default()
        });
        assert_rejected_without_store(SettingsPatch {
            hidden_columns: Some(vec![
                String::new();
                crate::store::MAX_SETTINGS_PATCH_COLLECTION_ITEMS + 1
            ]),
            ..SettingsPatch::default()
        });
        assert_rejected_without_store(SettingsPatch {
            playlist_column_orders: Some(BTreeMap::from([(
                "playlist".into(),
                vec![String::new(); crate::store::MAX_SETTINGS_PATCH_COLLECTION_ITEMS + 1],
            )])),
            ..SettingsPatch::default()
        });
        assert_rejected_without_store(SettingsPatch {
            column_widths: Some(BTreeMap::from([("x".repeat(4 * 1024 + 1), 1)])),
            ..SettingsPatch::default()
        });
    }

    #[test]
    fn invalid_or_failed_settings_patch_has_no_memory_event_or_runtime_effects() {
        fn assert_no_effects(patch: SettingsPatch, fail_store: bool) {
            let temp = tempfile::tempdir().unwrap();
            let store = FsSettingsStore::new(temp.path());
            store.save(&Settings::default()).unwrap();
            let state = SettingsState::new(Settings::default(), store.clone());
            let events = Arc::new(AtomicUsize::new(0));
            let runtime = Arc::new(AtomicUsize::new(0));
            let release = fail_store.then(|| {
                let hook = SaveHook::new(true);
                store.arm_save(Arc::clone(&hook));
                std::thread::spawn(move || {
                    hook.wait_until_reached();
                    hook.release();
                })
            });
            let event_count = Arc::clone(&events);
            let runtime_count = Arc::clone(&runtime);

            let result = tauri::async_runtime::block_on(async {
                persist_settings_patch(&state, patch, None, move |_, _| {
                    event_count.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .await?;
                runtime_count.fetch_add(1, Ordering::SeqCst);
                Ok::<_, String>(())
            });
            if let Some(release) = release {
                release.join().unwrap();
            }

            assert!(result.is_err());
            assert_eq!(state.snapshot(), Settings::default());
            assert_eq!(store.load().unwrap(), Some(Settings::default()));
            assert_eq!(events.load(Ordering::SeqCst), 0);
            assert_eq!(runtime.load(Ordering::SeqCst), 0);
        }

        assert_no_effects(
            SettingsPatch {
                spotify_client_id: Some("x".repeat(4 * 1024 + 1)),
                ..SettingsPatch::default()
            },
            false,
        );
        assert_no_effects(
            SettingsPatch {
                theme: Some(Theme::Dark),
                ..SettingsPatch::default()
            },
            true,
        );
    }
}

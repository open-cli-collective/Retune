use serde::Serialize;
use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

use super::application::{Owners, UseCases};
use super::model::{ApplyFailure, ImportApplyFinished};
use super::{
    apply::{apply_failure_event, apply_work_pending, execute_apply_job},
    current_import_view, enqueue_next_accept_all_job, lastfm_username, recover_before_apply_job,
    startup_lastfm_identity_matches, startup_resume_plan, AcceptAllSummary,
    CollectionAlbumCandidate, CountMode, ImportDefaults, ImportMatchSelection, ImportPageView,
    ImportPhase, ImportQueuePage, ImportStateView, PageOptions, ReviewAction, ReviewApplyJob,
    ReviewBatchKey, Service, LASTFM_QUEUE_PAGE_LIMIT,
};

const IMPORT_WINDOWS: [&str; 2] = ["main", "lastfm-importer"];

fn emit_to_import_windows<T: Clone + Serialize>(
    app: &tauri::AppHandle,
    event: &str,
    payload: T,
) -> Result<(), String> {
    for label in IMPORT_WINDOWS {
        if app.get_webview_window(label).is_some() {
            app.emit_to(label, event, payload.clone())
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn emit_import_invalidated(app: &tauri::AppHandle) -> Result<(), String> {
    emit_to_import_windows(app, "lastfm-import-changed", ())
}

fn emit_apply_finished(app: &tauri::AppHandle, payload: ImportApplyFinished) -> Result<(), String> {
    emit_to_import_windows(app, "lastfm-import-apply-finished", payload)
}

async fn emit_import_changed(
    app: &tauri::AppHandle,
    service: &Service,
    lastfm: &crate::lastfm::Service,
) -> Result<ImportStateView, String> {
    let view = current_import_view(service, lastfm).await?;
    emit_import_invalidated(app)?;
    Ok(view)
}

pub(super) fn use_cases(
    state: &crate::AppState,
) -> UseCases<
    '_,
    impl Fn() -> Result<std::sync::Arc<crate::SpotifyProvider>, String> + '_,
    impl Fn() -> Result<bool, String> + '_,
> {
    UseCases::new(
        Owners {
            service: &state.lastfm_import,
            lastfm: &state.lastfm,
            membership: &state.spotify_membership,
            library: &state.library,
            settings: &state.settings,
            cooldown_store: &state.cooldown_store,
        },
        || crate::provider_from(state),
        || crate::stored_connection_state(&state.token_store).map(|state| state.connected),
    )
}

pub(crate) async fn resume_persisted_import(app: tauri::AppHandle) {
    let state = app.state::<crate::AppState>();
    let service = std::sync::Arc::clone(&state.lastfm_import);
    let Some(session) = service.snapshot().await else {
        return;
    };
    let Some((username, _)) = startup_resume_plan(Some(&session)) else {
        return;
    };
    let live_username = if session.phase == ImportPhase::Aggregating {
        lastfm_username(state.lastfm.as_ref()).await.ok()
    } else {
        None
    };
    if !startup_lastfm_identity_matches(&session, live_username.as_deref()) {
        if service.suspend_for_account_mismatch().await.is_ok() {
            let _ = emit_import_invalidated(&app);
        }
        return;
    }
    if let Some(run) = service.claim_runner() {
        let _run = run;
        let lastfm = std::sync::Arc::clone(&state.lastfm);
        let progress_app = app.clone();
        let progress_service = std::sync::Arc::clone(&service);
        let progress_lastfm = std::sync::Arc::clone(&lastfm);
        super::run_import(lastfm, service, username, move || {
            let app = progress_app.clone();
            let service = std::sync::Arc::clone(&progress_service);
            let lastfm = std::sync::Arc::clone(&progress_lastfm);
            async move {
                let _ = emit_import_changed(&app, &service, &lastfm).await;
            }
        })
        .await;
    }
}

async fn run_apply_job(
    app: &tauri::AppHandle,
    service: std::sync::Arc<Service>,
    job: &ReviewApplyJob,
) -> Result<(), ApplyFailure> {
    let state = app.state::<crate::AppState>();
    recover_before_apply_job(
        &state.library,
        &state.lastfm,
        &state.spotify_membership,
        &service,
    )
    .await?;
    if job.plan.session_id.is_empty() {
        return Err("Last.fm apply job has no session identity.".into());
    }
    let worker_app = app.clone();
    execute_apply_job(&service, job, move |stage, plan| {
        let app = worker_app.clone();
        Box::pin(async move {
            let state = app.state::<crate::AppState>();
            let result = use_cases(&state)
                .run_apply_effect(stage, &plan, || {
                    app.emit_to("main", "library-changed", ())
                        .map_err(|error| error.to_string())
                })
                .await;
            result
        })
    })
    .await?;
    log::info!(target: "lastfm_import", "apply complete job={}", job.id);
    if let Err(error) = emit_apply_finished(
        app,
        ImportApplyFinished::Succeeded {
            batch_id: job.plan.batch_id,
        },
    ) {
        log::warn!(
            target: "lastfm_import",
            "apply completion notification failed job={} error={error}",
            job.id
        );
    }
    Ok(())
}

fn start_apply_worker(app: tauri::AppHandle, service: std::sync::Arc<Service>) {
    let Some(run) = service.claim_apply_runner() else {
        return;
    };
    tauri::async_runtime::spawn(async move {
        let mut restart_worker = true;
        loop {
            let next = match service.next_apply_job().await {
                Some(next) => next,
                None => match enqueue_next_accept_all_job(&service).await {
                    Ok(true) => continue,
                    Ok(false) => break,
                    Err(error) => {
                        log::warn!(target: "lastfm_import", "accept-all resume failed: {error}");
                        restart_worker = false;
                        break;
                    }
                },
            };
            let job = match service.claim_apply_job(&next.id).await {
                Ok(Some(job)) => job,
                Ok(None) => continue,
                Err(error) => {
                    log::error!(target: "lastfm_import", "apply claim failed: {error}");
                    restart_worker = false;
                    break;
                }
            };
            if let Err(error) = run_apply_job(&app, std::sync::Arc::clone(&service), &job).await {
                let _ = service.fail_apply_job_with(&job.id, error.clone()).await;
                let _ = emit_apply_finished(&app, apply_failure_event(job.plan.batch_id, &error));
                restart_worker = false;
                break;
            }
        }
        drop(run);
        if restart_worker && apply_work_pending(&service).await {
            start_apply_worker(app.clone(), std::sync::Arc::clone(&service));
        } else {
            let lastfm = std::sync::Arc::clone(&app.state::<crate::AppState>().lastfm);
            let _ = emit_import_changed(&app, &service, &lastfm).await;
        }
    });
}

pub(crate) async fn resume_persisted_apply(app: tauri::AppHandle) {
    let service = std::sync::Arc::clone(&app.state::<crate::AppState>().lastfm_import);
    let next_job = service.next_apply_job().await;
    let accept_all = service.sync_snapshot().await.accept_all;
    if apply_work_pending(&service).await {
        log::info!(
            target: "lastfm_import",
            "apply resume queued_job={} accept_all={}",
            next_job.as_ref().map(|job| job.id.as_str()).unwrap_or("none"),
            accept_all.is_some()
        );
        start_apply_worker(app, service);
    }
}

fn queue_page_request(
    cursor: Option<usize>,
    limit: Option<usize>,
) -> Result<(usize, usize), String> {
    let cursor = cursor.unwrap_or_default();
    let limit = limit.unwrap_or(LASTFM_QUEUE_PAGE_LIMIT);
    if limit == 0 || limit > LASTFM_QUEUE_PAGE_LIMIT {
        return Err(format!(
            "Last.fm import queue limit must be between 1 and {LASTFM_QUEUE_PAGE_LIMIT}."
        ));
    }
    Ok((cursor, limit))
}

#[tauri::command]
pub(crate) async fn sync_lastfm_plays(app: tauri::AppHandle) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let worker_app = app.clone();
    let changed_app = app.clone();
    let result = use_cases(&state)
        .sync(
            || {
                start_apply_worker(
                    worker_app.clone(),
                    std::sync::Arc::clone(&state.lastfm_import),
                )
            },
            || {
                let _ = emit_import_invalidated(&changed_app);
            },
        )
        .await;
    result
}

#[tauri::command]
pub(crate) async fn open_lastfm_importer(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("lastfm-importer") {
        window.show().map_err(|error| error.to_string())?;
        window.set_focus().map_err(|error| error.to_string())?;
        return Ok(());
    }
    WebviewWindowBuilder::new(
        &app,
        "lastfm-importer",
        WebviewUrl::App("index.html".into()),
    )
    .title("Last.fm importer")
    .inner_size(1320.0, 840.0)
    .resizable(true)
    .build()
    .map(|_| ())
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) async fn lastfm_import_state(app: tauri::AppHandle) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let result = use_cases(&state).state(crate::unix_now()).await;
    result
}

#[tauri::command]
pub(crate) async fn lastfm_import_queue(
    app: tauri::AppHandle,
    cursor: Option<usize>,
    limit: Option<usize>,
) -> Result<ImportQueuePage, String> {
    let (cursor, limit) = queue_page_request(cursor, limit)?;
    let state = app.state::<crate::AppState>();
    let result = use_cases(&state).queue(cursor, limit).await;
    result
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_page(
    app: tauri::AppHandle,
    batch_id: u32,
    artist: String,
    album: String,
) -> Result<Option<ImportPageView>, String> {
    let state = app.state::<crate::AppState>();
    let (page, changed, network_search) = use_cases(&state)
        .page(ReviewBatchKey {
            batch_id,
            artist,
            album,
        })
        .await?;
    if changed || network_search {
        let _ = emit_import_invalidated(&app);
    }
    if network_search {
        crate::spotify_commands::emit_spotify_sync_status(&app)?;
    }
    Ok(page)
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_review(
    app: tauri::AppHandle,
    batch_id: u32,
    ids: Option<Vec<String>>,
    action: ReviewAction,
    artist: String,
    album: String,
) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let view = use_cases(&state)
        .review(
            ReviewBatchKey {
                batch_id,
                artist,
                album,
            },
            ids.as_deref(),
            action,
        )
        .await?;
    emit_import_invalidated(&app).map_err(|error| error.to_string())?;
    Ok(view)
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_options(
    app: tauri::AppHandle,
    batch_id: u32,
    artist: String,
    album: String,
    options: PageOptions,
) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let view = use_cases(&state)
        .options(
            ReviewBatchKey {
                batch_id,
                artist,
                album,
            },
            options,
        )
        .await?;
    emit_import_invalidated(&app).map_err(|error| error.to_string())?;
    Ok(view)
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_count_mode(
    app: tauri::AppHandle,
    target_uri: String,
    mode: CountMode,
) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let view = use_cases(&state).count_mode(&target_uri, mode).await?;
    emit_import_invalidated(&app).map_err(|error| error.to_string())?;
    Ok(view)
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_search_terms(
    app: tauri::AppHandle,
    show: bool,
) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let view = use_cases(&state).search_terms(show).await?;
    emit_import_invalidated(&app).map_err(|error| error.to_string())?;
    Ok(view)
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_select_match(
    app: tauri::AppHandle,
    batch_id: u32,
    id: String,
    uri: String,
) -> Result<Option<ImportPageView>, String> {
    let state = app.state::<crate::AppState>();
    let page = use_cases(&state)
        .select_matches(batch_id, vec![ImportMatchSelection { id, uri }])
        .await?;
    emit_import_invalidated(&app).map_err(|error| error.to_string())?;
    Ok(page)
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_select_matches(
    app: tauri::AppHandle,
    batch_id: u32,
    selections: Vec<ImportMatchSelection>,
) -> Result<Option<ImportPageView>, String> {
    let state = app.state::<crate::AppState>();
    let page = use_cases(&state)
        .select_matches(batch_id, selections)
        .await?;
    emit_import_invalidated(&app).map_err(|error| error.to_string())?;
    Ok(page)
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_collection_search_albums(
    app: tauri::AppHandle,
    batch_id: u32,
    artist: String,
    query: String,
) -> Result<Vec<CollectionAlbumCandidate>, String> {
    let state = app.state::<crate::AppState>();
    let (result, network_search) = use_cases(&state)
        .search_collection_albums(batch_id, &artist, &query)
        .await?;
    if network_search {
        emit_import_invalidated(&app).map_err(|error| error.to_string())?;
        crate::spotify_commands::emit_spotify_sync_status(&app)?;
    }
    Ok(result)
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_collection_preview_album(
    app: tauri::AppHandle,
    batch_id: u32,
    artist: String,
    uri: String,
) -> Result<Option<ImportPageView>, String> {
    let state = app.state::<crate::AppState>();
    let page = use_cases(&state)
        .preview_or_add_collection_album(batch_id, &artist, &uri, false)
        .await?;
    let _ = emit_import_invalidated(&app);
    Ok(page)
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_collection_add_album(
    app: tauri::AppHandle,
    batch_id: u32,
    artist: String,
    uri: String,
) -> Result<Option<ImportPageView>, String> {
    let state = app.state::<crate::AppState>();
    let page = use_cases(&state)
        .preview_or_add_collection_album(batch_id, &artist, &uri, true)
        .await?;
    let _ = emit_import_invalidated(&app);
    Ok(page)
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_collection_remove_album(
    app: tauri::AppHandle,
    batch_id: u32,
    artist: String,
    uri: String,
) -> Result<Option<ImportPageView>, String> {
    let state = app.state::<crate::AppState>();
    let page = use_cases(&state)
        .remove_collection_album(batch_id, &artist, &uri)
        .await?;
    let _ = emit_import_invalidated(&app);
    Ok(page)
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_change_track(
    app: tauri::AppHandle,
    batch_id: u32,
    id: String,
    query: String,
) -> Result<Option<ImportPageView>, String> {
    let state = app.state::<crate::AppState>();
    let (page, network_search) = use_cases(&state)
        .change_track(batch_id, &id, &query)
        .await?;
    if network_search {
        crate::spotify_commands::emit_spotify_sync_status(&app)?;
    }
    emit_import_invalidated(&app).map_err(|error| error.to_string())?;
    Ok(page)
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_change_album(
    app: tauri::AppHandle,
    batch_id: u32,
    id: String,
    query: String,
) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let (view, network_search) = use_cases(&state)
        .change_album(batch_id, &id, &query)
        .await?;
    if network_search {
        crate::spotify_commands::emit_spotify_sync_status(&app)?;
    }
    emit_import_invalidated(&app).map_err(|error| error.to_string())?;
    Ok(view)
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_activate_collection(
    app: tauri::AppHandle,
    batch_id: u32,
    artist: String,
    album: String,
) -> Result<Option<ImportPageView>, String> {
    let state = app.state::<crate::AppState>();
    let page = use_cases(&state)
        .activate_collection(ReviewBatchKey {
            batch_id,
            artist,
            album,
        })
        .await?;
    emit_import_invalidated(&app).map_err(|error| error.to_string())?;
    Ok(page)
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_apply(
    app: tauri::AppHandle,
    batch_id: u32,
    artist: String,
    album: String,
    selected_ids: Vec<String>,
    archive_batch: bool,
    options: PageOptions,
) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let worker_app = app.clone();
    let changed_app = app.clone();
    let result = use_cases(&state)
        .apply(
            ReviewBatchKey {
                batch_id,
                artist,
                album,
            },
            &selected_ids,
            archive_batch,
            options,
            || {
                start_apply_worker(
                    worker_app.clone(),
                    std::sync::Arc::clone(&state.lastfm_import),
                )
            },
            || {
                let _ = emit_import_invalidated(&changed_app);
            },
        )
        .await;
    result
}

#[tauri::command(rename_all = "camelCase")]
pub(crate) async fn lastfm_import_retry_apply(
    app: tauri::AppHandle,
    batch_id: u32,
) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let worker_app = app.clone();
    let changed_app = app.clone();
    let result = use_cases(&state)
        .retry_apply(
            batch_id,
            || {
                start_apply_worker(
                    worker_app.clone(),
                    std::sync::Arc::clone(&state.lastfm_import),
                )
            },
            || {
                let _ = emit_import_invalidated(&changed_app);
            },
        )
        .await;
    result
}

#[tauri::command]
pub(crate) async fn lastfm_import_prepare_accept_all(
    app: tauri::AppHandle,
) -> Result<AcceptAllSummary, String> {
    let state = app.state::<crate::AppState>();
    let (summary, changed, network_search) = use_cases(&state).prepare_accept_all().await?;
    if changed || network_search {
        let _ = emit_import_invalidated(&app);
    }
    if network_search {
        crate::spotify_commands::emit_spotify_sync_status(&app)?;
    }
    Ok(summary)
}

#[tauri::command]
pub(crate) async fn lastfm_import_accept_all(
    app: tauri::AppHandle,
) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let worker_app = app.clone();
    let changed_app = app.clone();
    let result = use_cases(&state)
        .accept_all(
            || {
                start_apply_worker(
                    worker_app.clone(),
                    std::sync::Arc::clone(&state.lastfm_import),
                )
            },
            || {
                let _ = emit_import_invalidated(&changed_app);
            },
        )
        .await;
    result
}

#[tauri::command]
pub(crate) async fn start_lastfm_import(
    app: tauri::AppHandle,
    defaults: Option<ImportDefaults>,
) -> Result<ImportStateView, String> {
    let state = app.state::<crate::AppState>();
    let runner_app = app.clone();
    let changed_app = app.clone();
    let result = use_cases(&state)
        .start_import(
            defaults,
            move |run, lastfm, service, username| {
                let runner_app = runner_app.clone();
                tauri::async_runtime::spawn(async move {
                    let _run = run;
                    let progress_app = runner_app.clone();
                    let progress_service = std::sync::Arc::clone(&service);
                    let progress_lastfm = std::sync::Arc::clone(&lastfm);
                    super::run_import(lastfm, service, username, move || {
                        let app = progress_app.clone();
                        let service = std::sync::Arc::clone(&progress_service);
                        let lastfm = std::sync::Arc::clone(&progress_lastfm);
                        async move {
                            let _ = emit_import_changed(&app, &service, &lastfm).await;
                        }
                    })
                    .await;
                });
            },
            || {
                let _ = emit_import_invalidated(&changed_app);
            },
        )
        .await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_page_request_maps_defaults_and_rejects_invalid_limits() {
        assert_eq!(IMPORT_WINDOWS, ["main", "lastfm-importer"]);
        assert_eq!(
            queue_page_request(None, None),
            Ok((0, LASTFM_QUEUE_PAGE_LIMIT))
        );
        assert!(queue_page_request(None, Some(0)).is_err());
        assert!(queue_page_request(None, Some(LASTFM_QUEUE_PAGE_LIMIT + 1)).is_err());
    }
}

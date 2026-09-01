use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{
    album_id, auth, clear_spotify_catalog_async, emit_connection_state_async, emit_main,
    emit_main_event, empty_player_state, image_url,
    library_commands::{rating_view, RatingView},
    main_events, notify_error, playlist_commands,
    provider::{
        self, artist_albums_page, artist_descriptor, title_case, ArtistAlbumsPage, SearchGroup,
        SearchResults, SpotifySyncProvider, SyncBatch,
    },
    provider_from, resolve_track_artwork,
    settings_commands::set_auto_connect,
    spotify_id,
    spotify_membership::{self, SpotifyActionFailure},
    spotify_provider,
    store::{self, FsCooldownStore, Settings, SpotifyLibraryState},
    stored_connection_state, sync, track_id, unix_now, AppState, ConnectionState, LoopbackListener,
    OpenerExt, Pkce,
};
use librespot_core::{authentication::Credentials, config::SessionConfig, session::Session};
use retune_core::model::{AlbumKey, Library, Rating, SourceId};
use retune_spotify::{
    client::{Album, HttpTransport, Profile, SpotifyClient, Track as SpotifyTrack, Transport},
    tokens::{PlaybackCredentials, TokenStore, Tokens},
};
use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager};

#[derive(Default)]
pub(crate) struct SpotifySession {
    state: Mutex<SpotifySessionState>,
    commit_gate: Arc<tokio::sync::Mutex<()>>,
    changed: tokio::sync::Notify,
}

#[derive(Default)]
struct SpotifySessionState {
    revision: u64,
    active_invalidations: u32,
    active_oauth: Option<Arc<AtomicBool>>,
}

struct SpotifyOAuthAttempt<'a> {
    owner: &'a SpotifySession,
    revision: u64,
    identity: String,
    cancelled: Arc<AtomicBool>,
}

impl SpotifySession {
    pub(crate) fn revision(&self) -> u64 {
        self.state
            .lock()
            .expect("Spotify session mutex poisoned")
            .revision
    }

    fn ensure_revision(&self, revision: u64) -> Result<(), String> {
        (self.revision() == revision)
            .then_some(())
            .ok_or_else(|| "The Spotify connection changed. Try again.".into())
    }

    pub(crate) async fn commit_revision(
        &self,
        revision: u64,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, String> {
        let guard = Arc::clone(&self.commit_gate).lock_owned().await;
        self.ensure_revision(revision)?;
        Ok(guard)
    }

    fn begin_oauth(&self, identity: String) -> Result<SpotifyOAuthAttempt<'_>, String> {
        let mut state = self.state.lock().expect("Spotify session mutex poisoned");
        if state.active_invalidations != 0 || state.active_oauth.is_some() {
            return Err("Spotify authorization is already in progress.".into());
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        state.active_oauth = Some(Arc::clone(&cancelled));
        Ok(SpotifyOAuthAttempt {
            owner: self,
            revision: state.revision,
            identity,
            cancelled,
        })
    }

    fn invalidate_state(&self) {
        let mut state = self.state.lock().expect("Spotify session mutex poisoned");
        state.active_invalidations = state.active_invalidations.saturating_add(1);
        if let Some(cancelled) = &state.active_oauth {
            cancelled.store(true, Ordering::Release);
        }
        state.revision = state.revision.wrapping_add(1);
    }

    fn bump_revision(&self) {
        let mut state = self.state.lock().expect("Spotify session mutex poisoned");
        state.revision = state.revision.wrapping_add(1);
    }

    fn oauth_active(&self) -> bool {
        self.state
            .lock()
            .expect("Spotify session mutex poisoned")
            .active_oauth
            .is_some()
    }

    pub(crate) async fn invalidate(&self) {
        let commit = Arc::clone(&self.commit_gate).lock_owned().await;
        self.invalidate_state();
        drop(commit);
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !self.oauth_active() {
                break;
            }
            changed.await;
        }
        let mut state = self.state.lock().expect("Spotify session mutex poisoned");
        state.active_invalidations = state.active_invalidations.saturating_sub(1);
    }
}

impl SpotifyOAuthAttempt<'_> {
    fn ensure_current(&self, identity: &str) -> Result<(), String> {
        if self.cancelled.load(Ordering::Acquire)
            || self.identity != identity
            || self.owner.revision() != self.revision
        {
            return Err("The Spotify connection changed during authorization. Try again.".into());
        }
        Ok(())
    }

    async fn commit(&self, identity: &str) -> Result<tokio::sync::OwnedMutexGuard<()>, String> {
        let guard = Arc::clone(&self.owner.commit_gate).lock_owned().await;
        self.ensure_current(identity)?;
        Ok(guard)
    }

    async fn commit_replacement(
        &self,
        identity: &str,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, String> {
        let guard = self.commit(identity).await?;
        self.owner.bump_revision();
        Ok(guard)
    }
}

impl Drop for SpotifyOAuthAttempt<'_> {
    fn drop(&mut self) {
        let mut state = self
            .owner
            .state
            .lock()
            .expect("Spotify session mutex poisoned");
        if state
            .active_oauth
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, &self.cancelled))
        {
            state.active_oauth = None;
            self.owner.changed.notify_waiters();
        }
    }
}

#[derive(Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AlbumPageTrackView {
    uri: String,
    name: String,
    track_no: Option<u32>,
    duration_secs: u64,
    enabled: bool,
    track_id: Option<u64>,
    pub(super) saved_individually: bool,
    rating: Option<RatingView>,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AlbumPageView {
    uri: String,
    name: String,
    artist: String,
    artist_id: String,
    album_type: String,
    year: Option<String>,
    image_url: Option<String>,
    total_duration_secs: u64,
    pub(super) saved_album: bool,
    pub(super) content_complete: bool,
    added_at: Option<u64>,
    pub(super) album_rating: Option<u8>,
    pub(super) tracks: Vec<AlbumPageTrackView>,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ArtistPageView {
    id: String,
    name: String,
    descriptor: String,
    image_url: Option<String>,
    following: bool,
}

pub(super) async fn sync_spotify(app: &tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    state.sync_orchestrator.cancel_retry();
    let Some(run) = state.sync_orchestrator.begin() else {
        return Ok(());
    };
    run_sync_loop(app, run).await
}

async fn run_sync_loop(
    app: &tauri::AppHandle,
    mut run: crate::sync_orchestrator::SyncRun,
) -> Result<(), String> {
    loop {
        let result = sync_spotify_inner(app).await;
        if !result.as_ref().is_ok_and(|completion| completion.partial) {
            let _ = emit_main(app, "sync-progress", "");
        }
        if run.finish() {
            continue;
        }
        if let Ok(SyncCompletion {
            auto_resume: Some(deadline),
            ..
        }) = &result
        {
            schedule_auto_resume(app, *deadline);
        }
        return result.map(|_| ());
    }
}

fn schedule_auto_resume(app: &tauri::AppHandle, deadline: u64) {
    let now = unix_now();
    let jitter = 30
        + SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as u64
            % 61;
    let delay = Duration::from_secs(deadline.saturating_sub(now).saturating_add(jitter));
    let handle = app.clone();
    let retry = tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let state = handle.state::<AppState>();
        state.sync_orchestrator.retry_fired();
        if let Some(run) = state.sync_orchestrator.begin() {
            if let Err(error) = Box::pin(run_sync_loop(&handle, run)).await {
                notify_error(&handle, error);
            }
        }
    });
    app.state::<AppState>()
        .sync_orchestrator
        .replace_retry(retry);
}

struct SyncCompletion {
    partial: bool,
    auto_resume: Option<u64>,
}

#[derive(Clone, Copy, Serialize)]
pub(super) struct SyncProgressCount {
    pub(super) tracks: u64,
    pub(super) fraction: f64,
}

#[derive(Default)]
pub(super) struct SyncProgressState {
    tracks: u64,
    sections: [(u32, Option<u32>); 5],
    high_water: f64,
}

impl SyncProgressState {
    pub(super) fn update(&mut self, batch: &SyncBatch) -> SyncProgressCount {
        self.tracks += batch.tracks.len() as u64;
        let index = provider::LibraryKind::ALL
            .iter()
            .position(|kind| kind.label() == batch.section)
            .expect("unknown sync section");
        self.sections[index] = (batch.done, batch.total);
        let fraction = self
            .sections
            .iter()
            .enumerate()
            .map(|(section, (done, total))| {
                if section < index {
                    1.0
                } else {
                    total.map_or(0.0, |total| {
                        if total == 0 {
                            1.0
                        } else {
                            f64::from(*done) / f64::from(total)
                        }
                    })
                }
            })
            .sum::<f64>()
            / provider::LibraryKind::ALL.len() as f64;
        self.high_water = self.high_water.max(fraction.clamp(0.0, 1.0));
        SyncProgressCount {
            tracks: self.tracks,
            fraction: self.high_water,
        }
    }
}

const GENRES_DEGRADED_MSG: &str =
    "Imported without genres (Spotify rate limit) — genres will fill in on a later sync.";

pub(super) fn partial_import_message(
    detail: &str,
    quota_exhausted: bool,
    earliest_cooldown: Option<u64>,
    now: chrono::DateTime<chrono::Local>,
) -> String {
    earliest_cooldown.map_or_else(
        || {
            if quota_exhausted {
                if detail.is_empty() {
                    "Partial import — Spotify Development Mode quota is exhausted; sync again after Spotify resets it.".into()
                } else {
                    format!("Partial import ({detail}) — Spotify Development Mode quota is exhausted; sync again after Spotify resets it.")
                }
            } else if detail.is_empty() {
                "Partial import (Spotify rate limit) — run File → Sync later to finish.".into()
            } else {
                format!("Partial import ({detail}) — run File → Sync later to finish.")
            }
        },
        |deadline| {
            let time = provider::format_resume_time(deadline, now);
            if quota_exhausted && detail.is_empty() {
                format!("Partial import (Spotify Development Mode quota) — will finish automatically after {time}.")
            } else if quota_exhausted {
                format!("Partial import (Spotify Development Mode quota) — {detail} — will finish automatically after {time}.")
            } else if detail.is_empty() {
                format!("Partial import — will finish automatically after {time}.")
            } else {
                format!("Partial import — {detail} — will finish automatically after {time}.")
            }
        },
    )
}

async fn sync_spotify_inner(app: &tauri::AppHandle) -> Result<SyncCompletion, String> {
    log::info!("Starting Spotify sync");
    let state = app.state::<AppState>();
    let provider = provider_from(&state)?;
    let sync_progress = Mutex::new(SyncProgressState::default());
    let on_batch = |batch: SyncBatch| {
        let mut counts = sync_progress.lock().expect("sync progress mutex poisoned");
        let payload = counts.update(&batch);
        drop(counts);
        let _ = emit_main(app, "sync-progress-count", payload);
    };
    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?;
    let result = SpotifySyncApplication {
        session: &state.spotify_session,
        membership: &state.spotify_membership,
        library: &state.library,
        settings: &state.settings,
        token_store: &state.token_store,
        cooldown_store: &state.cooldown_store,
        artist_genres_store: &state.artist_genres_store,
        lastfm_import: &state.lastfm_import,
        restore_mutations: &state.restore_mutations,
        app_data_dir,
    }
    .execute(
        provider.as_ref(),
        |phase| {
            log::info!("{phase}");
            let _ = emit_main(app, "sync-progress", phase);
        },
        &on_batch,
        || clear_spotify_catalog_async(app),
        || {
            let tracks = sync_progress
                .lock()
                .expect("sync progress mutex poisoned")
                .tracks;
            emit_main(
                app,
                "sync-progress-count",
                SyncProgressCount {
                    tracks,
                    fraction: 1.0,
                },
            )
            .map_err(|error| error.to_string())?;
            emit_main(app, "sync-progress", "Saving library…").map_err(|error| error.to_string())
        },
    )
    .await?;
    let SpotifySyncResult {
        genres_degraded,
        partial,
        quota_exhausted,
        progress,
        earliest_cooldown,
        request_counts,
        library_changed,
        session_commit,
    } = result;
    {
        let library = state.library.lock().expect("library mutex poisoned");
        log::info!(
            "Spotify sync applied; {} library tracks",
            library.tracks().len()
        );
    }
    if library_changed {
        emit_main(app, "library-changed", ()).map_err(|error| error.to_string())?;
    }
    if partial {
        let detail = progress
            .iter()
            .map(|progress| match progress.total {
                Some(total) => format!("{} of {total} {}", progress.done, progress.label),
                None => format!("{} pending", progress.label),
            })
            .collect::<Vec<_>>()
            .join(", ");
        let message = partial_import_message(
            &detail,
            quota_exhausted,
            earliest_cooldown,
            chrono::Local::now(),
        );
        log::warn!("{message}");
        if genres_degraded {
            log::warn!("{GENRES_DEGRADED_MSG}");
        }
        emit_main(app, "sync-progress", message).map_err(|error| error.to_string())?;
    } else if genres_degraded {
        log::warn!("{GENRES_DEGRADED_MSG}");
        emit_main(app, "sync-progress", GENRES_DEGRADED_MSG).map_err(|error| error.to_string())?;
    }
    log::info!(
        "sync requests:{}",
        request_counts
            .iter()
            .map(|(family, count)| format!(" {family}={count}"))
            .collect::<String>()
    );
    drop(session_commit);
    if let Err(error) = playlist_commands::sync_playlists(app, provider.as_ref()).await {
        log::warn!("Playlist sync failed: {error}");
    }
    Ok(SyncCompletion {
        partial,
        auto_resume: partial.then_some(earliest_cooldown).flatten(),
    })
}

struct SpotifySyncApplication<'a> {
    session: &'a SpotifySession,
    membership: &'a crate::spotify_membership::SpotifyMembership,
    library: &'a crate::library_state::LibraryState,
    settings: &'a crate::store::SettingsState,
    token_store: &'a crate::SharedTokenStore,
    cooldown_store: &'a FsCooldownStore,
    artist_genres_store: &'a crate::store::FsArtistGenresStore,
    lastfm_import: &'a crate::lastfm_import::Service,
    restore_mutations: &'a Arc<crate::restore_latch::RestoreMutationState>,
    app_data_dir: PathBuf,
}

struct SpotifySyncResult {
    genres_degraded: bool,
    partial: bool,
    quota_exhausted: bool,
    progress: Vec<provider::SectionProgress>,
    earliest_cooldown: Option<u64>,
    request_counts: std::collections::BTreeMap<String, u64>,
    library_changed: bool,
    session_commit: tokio::sync::OwnedMutexGuard<()>,
}

impl SpotifySyncApplication<'_> {
    async fn execute<ClearCatalog, ClearCatalogFuture>(
        &self,
        provider: &crate::SpotifyProvider,
        progress: impl FnMut(&str) + Send,
        on_batch: &(dyn Fn(SyncBatch) + Send + Sync),
        clear_catalog: ClearCatalog,
        before_commit: impl FnOnce() -> Result<(), String>,
    ) -> Result<SpotifySyncResult, String>
    where
        ClearCatalog: FnOnce() -> ClearCatalogFuture,
        ClearCatalogFuture: std::future::Future<Output = Result<(), String>>,
    {
        let session_revision = self.session.revision();
        let baseline_membership = self.membership.snapshot();
        if !stored_connection_state(self.token_store)?.connected {
            return Err("Connect to Spotify before syncing.".into());
        }
        let profile = provider
            .me()
            .await
            .map_err(|error| format!("Could not identify the Spotify account: {error}"))?;
        let account_id = immutable_account_id(&profile)?.to_owned();
        let baseline_membership =
            normalize_sync_baseline(baseline_membership, &profile.id, &account_id);
        let sync_provider = SpotifySyncProvider::for_account(
            provider,
            self.cooldown_store,
            self.artist_genres_store,
            account_id.clone(),
        )?;
        let first_sync = !self.settings.snapshot().spotify_sync_completed;
        let outcome = sync::snapshot(&sync_provider, progress, on_batch).await?;
        let session_commit = self.session.commit_revision(session_revision).await?;
        if !stored_connection_state(self.token_store)?.connected {
            return Err("The Spotify connection changed during sync. Try again.".into());
        }
        let membership = self.membership.lock().await;
        self.lastfm_import
            .migrate_spotify_account_id(&profile.id, &account_id)
            .await?;
        let current = membership.snapshot();
        let (reconciled, account_changed) = reconcile_spotify_account(current, &profile)?;
        if account_changed {
            clear_catalog().await?;
        }
        let sync::SnapshotOutcome {
            tracks,
            genres_degraded,
            partial,
            quota_exhausted,
            progress,
            earliest_cooldown,
            request_counts,
            spotify_library,
        } = outcome;
        let spotify_library = spotify_library
            .map(|incoming| rebase_sync_membership(&baseline_membership, &reconciled, incoming));
        let library_transaction = self.library.begin_transaction()?;
        let current_library = self.library.snapshot();
        let aliases = sync::spotify_track_aliases(&current_library, &tracks);
        let candidate = sync::candidate_from_snapshot(
            &current_library,
            first_sync,
            tracks,
            spotify_library.as_ref(),
        )?;
        let spotify_library = spotify_library.map(|mut merged| {
            let added_times = candidate
                .tracks()
                .iter()
                .map(|track| (track.uri.as_str(), track.added_at))
                .collect::<HashMap<_, _>>();
            for album in merged.saved_albums.values_mut() {
                if album.added_at.is_none() {
                    album.added_at = album
                        .track_uris
                        .iter()
                        .filter_map(|uri| {
                            let local_uri = aliases.get(uri).unwrap_or(uri);
                            added_times.get(local_uri.as_str()).copied().flatten()
                        })
                        .min()
                        .or_else(|| Some(unix_now()));
                }
            }
            merged
        });
        before_commit()?;
        let committed = commit_sync_state(
            membership,
            self.library.clone(),
            self.settings.clone(),
            Arc::clone(self.restore_mutations),
            self.app_data_dir.clone(),
            library_transaction,
            spotify_library.unwrap_or(reconciled),
            candidate,
            partial,
            unix_now(),
            session_commit,
            #[cfg(test)]
            None,
        )
        .await?;
        Ok(SpotifySyncResult {
            genres_degraded,
            partial,
            quota_exhausted,
            progress,
            earliest_cooldown,
            request_counts,
            library_changed: committed.library_changed,
            session_commit: committed.session_commit,
        })
    }
}

#[allow(clippy::too_many_arguments)]
async fn commit_sync_state(
    membership: crate::spotify_membership::SpotifyMembershipGuard,
    library: crate::library_state::LibraryState,
    settings: crate::store::SettingsState,
    restore_mutations: Arc<crate::restore_latch::RestoreMutationState>,
    app_data_dir: PathBuf,
    library_transaction: crate::library_state::LibraryTransactionGuard,
    next_membership: SpotifyLibraryState,
    next_library: Library,
    partial: bool,
    committed_at: u64,
    session_commit: tokio::sync::OwnedMutexGuard<()>,
    #[cfg(test)] commit_hook: Option<(usize, Arc<crate::store::SaveHook>)>,
) -> Result<SyncCommitReceipt, String> {
    tauri::async_runtime::spawn(async move {
        let settings_guard = settings.begin_sync_commit().await?;
        let before_membership = membership.snapshot();
        let before_library = library.snapshot();
        let library_changed = before_library != next_library;
        let before_settings = settings_guard.snapshot();
        let mut next_settings = before_settings.clone();
        record_full_sync(&mut next_settings, partial, committed_at);
        let journal = crate::spotify_sync_commit::Journal::applying(
            crate::spotify_sync_commit::Change {
                before: before_membership,
                after: next_membership.clone(),
            },
            crate::spotify_sync_commit::Change {
                before: before_library,
                after: next_library.clone(),
            },
            crate::spotify_sync_commit::Change {
                before: before_settings,
                after: next_settings.clone(),
            },
        );
        let commit_dir = app_data_dir.clone();
        if let Err(error) = tauri::async_runtime::spawn_blocking(move || {
            #[cfg(test)]
            if let Some((boundary, hook)) = commit_hook {
                return crate::spotify_sync_commit::Store::pausing_before(
                    &commit_dir,
                    boundary,
                    hook,
                )
                .commit(&journal);
            }
            crate::spotify_sync_commit::Store::new(&commit_dir).commit(&journal)
        })
        .await
        .map_err(|error| error.to_string())?
        {
            if app_data_dir.join("spotify-sync-journal.json").exists() {
                restore_mutations.mark_recovery_required();
            }
            return Err(error);
        }
        membership.install(next_membership);
        library.install_in_transaction(&library_transaction, next_library);
        settings_guard.install(next_settings);
        drop(settings_guard);
        drop(library_transaction);
        drop(membership);
        Ok(SyncCommitReceipt {
            session_commit,
            library_changed,
        })
    })
    .await
    .map_err(|error| error.to_string())?
}

struct SyncCommitReceipt {
    session_commit: tokio::sync::OwnedMutexGuard<()>,
    library_changed: bool,
}

fn immutable_account_id(profile: &Profile) -> Result<&str, String> {
    profile.account_id().ok_or_else(|| {
        "Spotify did not return an immutable account ID. Reconnect Spotify before continuing."
            .into()
    })
}

pub(super) fn reconcile_spotify_account(
    mut current: SpotifyLibraryState,
    profile: &Profile,
) -> Result<(SpotifyLibraryState, bool), String> {
    let account_id = immutable_account_id(profile)?;
    if current.account_id == profile.id {
        current.account_id = account_id.to_owned();
        Ok((current, false))
    } else if !current.account_id.is_empty() && current.account_id != account_id {
        Ok((
            SpotifyLibraryState {
                account_id: account_id.to_owned(),
                ..SpotifyLibraryState::default()
            },
            true,
        ))
    } else {
        Ok((current, false))
    }
}

fn rebase_sync_membership(
    baseline: &SpotifyLibraryState,
    current: &SpotifyLibraryState,
    mut incoming: SpotifyLibraryState,
) -> SpotifyLibraryState {
    if !baseline.is_exact()
        || !current.is_exact()
        || baseline.account_id != current.account_id
        || current.account_id != incoming.account_id
    {
        return current.clone().merge_earliest_times(incoming);
    }
    for uri in baseline.saved_tracks.keys() {
        if !current.saved_tracks.contains_key(uri) {
            incoming.saved_tracks.remove(uri);
        }
    }
    for (uri, added_at) in &current.saved_tracks {
        if baseline.saved_tracks.get(uri) != Some(added_at) {
            incoming.saved_tracks.insert(uri.clone(), *added_at);
        }
    }
    for uri in baseline.saved_albums.keys() {
        if !current.saved_albums.contains_key(uri) {
            incoming.saved_albums.remove(uri);
        }
    }
    for (uri, album) in &current.saved_albums {
        if baseline.saved_albums.get(uri) != Some(album) {
            incoming.saved_albums.insert(uri.clone(), album.clone());
        }
    }
    current.clone().merge_earliest_times(incoming)
}

fn normalize_sync_baseline(
    mut baseline: SpotifyLibraryState,
    profile_id: &str,
    account_id: &str,
) -> SpotifyLibraryState {
    if baseline.account_id == profile_id {
        baseline.account_id = account_id.to_owned();
    }
    baseline
}

pub(super) fn record_full_sync(settings: &mut Settings, partial: bool, now: u64) -> bool {
    if partial {
        return false;
    }
    settings.spotify_sync_completed = true;
    settings.last_full_sync = Some(now);
    true
}

pub(super) fn mark_album_membership(
    library: &Library,
    spotify_library: &SpotifyLibraryState,
    albums: &mut [provider::SearchAlbum],
) {
    for album in albums {
        if spotify_library.is_exact() {
            album.in_library = spotify_library.saved_albums.contains_key(&album.uri);
            continue;
        }
        // ponytail: local album identity is artist/title; store Spotify album URIs if
        // same-named editions become a real ambiguity.
        album.in_library = album.track_count > 0
            && library
                .tracks()
                .iter()
                .filter(|track| {
                    track.source == SourceId::Music
                        && track.art == album.artist
                        && track.alb == album.name
                })
                .count()
                >= album.track_count as usize;
    }
}

pub(super) fn mark_track_membership(
    library: &Library,
    spotify_library: &SpotifyLibraryState,
    tracks: &mut [provider::SearchTrack],
) {
    for track in tracks {
        track.in_library = if spotify_library.is_exact() {
            spotify_library.saved_tracks.contains_key(&track.uri)
        } else {
            library
                .tracks()
                .iter()
                .any(|candidate| candidate.source == SourceId::Music && candidate.uri == track.uri)
        };
    }
}

pub(super) async fn artist_albums_outcome<T: Transport, S: TokenStore>(
    provider: &SpotifyClient<T, S>,
    cooldown_store: &FsCooldownStore,
    artist_id: &str,
    offset: u32,
    now: u64,
    display_now: chrono::DateTime<chrono::Local>,
) -> Result<ArtistAlbumsPage, String> {
    if let Some(cooldown) = cooldown_store
        .cooldowns(now)
        .map_err(|error| error.to_string())?
        .get("/artists")
        .copied()
    {
        let time = provider::format_resume_time(cooldown.deadline, display_now);
        return Err(match cooldown.kind {
            store::CooldownKind::Transient => {
                format!("Spotify artist albums are rate limited; try again {time}.")
            }
            store::CooldownKind::Quota => format!(
                "Spotify Development Mode quota is still exhausted; try artist albums again {time}."
            ),
        });
    }
    match artist_albums_page(provider, artist_id, offset).await {
        Ok(page) => Ok(page),
        Err(retune_spotify::Error::RateLimited {
            endpoint,
            retry_after_secs,
        }) => {
            let deadline = now.saturating_add(retry_after_secs);
            spotify_membership::record_cooldown(
                cooldown_store,
                &endpoint,
                store::CooldownKind::Transient,
                deadline,
                now,
            )?;
            Err(format!(
                "Spotify artist albums are rate limited; try again {}.",
                provider::format_resume_time(deadline, display_now)
            ))
        }
        Err(retune_spotify::Error::QuotaExceeded {
            endpoint,
            retry_after_secs,
        }) => {
            if let Some(retry_after_secs) = retry_after_secs {
                let deadline = now.saturating_add(retry_after_secs);
                spotify_membership::record_cooldown(
                    cooldown_store,
                    &endpoint,
                    store::CooldownKind::Quota,
                    deadline,
                    now,
                )?;
                Err(format!(
                    "Spotify Development Mode quota is exhausted; try artist albums again {}.",
                    provider::format_resume_time(deadline, display_now)
                ))
            } else {
                Err("Spotify Development Mode quota is exhausted; try artist albums again after Spotify resets it.".into())
            }
        }
        Err(error) => Err(error.to_string()),
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum SpotifyOpenTarget {
    App,
    Web,
}

pub(super) fn spotify_item_link(
    kind: &str,
    id: &str,
    target: SpotifyOpenTarget,
) -> Result<String, String> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
    {
        return Err("Invalid Spotify ID.".into());
    }
    Ok(match target {
        SpotifyOpenTarget::App => format!("spotify:{kind}:{id}"),
        SpotifyOpenTarget::Web => format!("https://open.spotify.com/{kind}/{id}"),
    })
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum SpotifyDestination {
    Album,
    Artist,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub(super) enum SpotifyNavigation {
    Album { uri: String, highlight: String },
    Artist { id: String },
}

pub(super) fn spotify_track_destination(
    track: &SpotifyTrack,
    destination: SpotifyDestination,
) -> Result<SpotifyNavigation, String> {
    match destination {
        SpotifyDestination::Album => track
            .album
            .as_ref()
            .map(|album| SpotifyNavigation::Album {
                uri: album.uri.clone(),
                highlight: track.uri.clone(),
            })
            .ok_or_else(|| "Spotify album is unavailable.".into()),
        SpotifyDestination::Artist => track
            .artists
            .first()
            .map(|artist| SpotifyNavigation::Artist {
                id: artist.id.clone(),
            })
            .ok_or_else(|| "Spotify artist is unavailable.".into()),
    }
}

pub(super) fn album_page_view(
    library: &Library,
    spotify_library: &SpotifyLibraryState,
    album: Album,
) -> AlbumPageView {
    let artist = album.artists.first();
    let artist_name = artist.map(|artist| artist.name.clone()).unwrap_or_default();
    let total_duration_secs = album
        .tracks
        .as_ref()
        .into_iter()
        .flat_map(|page| &page.items)
        .map(|track| track.duration_ms.unwrap_or_default())
        .sum::<u64>()
        / 1_000;
    let local_added_at = album
        .tracks
        .as_ref()
        .into_iter()
        .flat_map(|page| &page.items)
        .filter_map(|track| {
            let normalized = retune_spotify::normalize::track(track, None, Some(&album));
            spotify_membership::spotify_track_match(library, &normalized)
                .and_then(|track| track.added_at)
        })
        .min();
    let tracks = album
        .tracks
        .clone()
        .map(|page| {
            page.items
                .into_iter()
                .map(|track| {
                    let uri = track.uri.clone();
                    let normalized = retune_spotify::normalize::track(&track, None, Some(&album));
                    let local = spotify_membership::spotify_track_match(library, &normalized);
                    AlbumPageTrackView {
                        uri: uri.clone(),
                        name: track.name,
                        track_no: track.track_number,
                        duration_secs: track.duration_ms.unwrap_or_default() / 1_000,
                        enabled: local.is_none_or(|track| track.enabled),
                        track_id: local.map(|track| track.id.0),
                        saved_individually: if spotify_library.is_exact() {
                            spotify_library.saved_tracks.contains_key(&uri)
                        } else {
                            local.is_some()
                        },
                        rating: local
                            .and_then(|track| library.effective_rating(track.id).map(rating_view)),
                    }
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let content_complete =
        !tracks.is_empty() && tracks.iter().all(|track| track.track_id.is_some());
    let saved_album = if spotify_library.is_exact() {
        spotify_library.saved_albums.contains_key(&album.uri)
    } else {
        content_complete
    };
    let added_at = spotify_library
        .is_exact()
        .then(|| {
            spotify_library
                .saved_albums
                .get(&album.uri)
                .and_then(|album| album.added_at)
        })
        .flatten()
        .or(local_added_at);
    let album_rating = content_complete
        .then(|| {
            library.album_rating(&AlbumKey {
                source: SourceId::Music,
                art: artist_name.clone(),
                alb: album.name.clone(),
            })
        })
        .flatten()
        .map(Rating::stars);
    AlbumPageView {
        uri: album.uri,
        name: album.name,
        artist: artist_name,
        artist_id: artist.map(|artist| artist.id.clone()).unwrap_or_default(),
        album_type: title_case(album.album_type.as_deref().unwrap_or("album")),
        year: album
            .release_date
            .as_deref()
            .and_then(|date| date.get(..4))
            .map(str::to_owned),
        image_url: image_url(&album.images),
        total_duration_secs,
        saved_album,
        content_complete,
        added_at,
        album_rating,
        tracks,
    }
}

fn album_library_uris(uri: &str) -> Vec<String> {
    vec![uri.to_owned()]
}

fn web_oauth_tokens(access: String, refresh: String, expires_at: u64, scopes: String) -> Tokens {
    Tokens {
        access,
        refresh,
        expires_at,
        scopes,
        playback_credentials: None,
    }
}

#[tauri::command]
pub(super) async fn connection_state(app: tauri::AppHandle) -> Result<ConnectionState, String> {
    let token_store = Arc::clone(&app.state::<AppState>().token_store);
    tauri::async_runtime::spawn_blocking(move || stored_connection_state(&token_store))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
pub(super) async fn connect_spotify(app: tauri::AppHandle) -> Result<(), String> {
    let client_id = app
        .state::<AppState>()
        .settings
        .snapshot()
        .spotify_client_id
        .trim()
        .to_owned();
    if client_id.is_empty() {
        return Err("Spotify Client ID is missing. Add it in Preferences, then try again.".into());
    }
    let app_state = app.state::<AppState>();
    let attempt = app_state.spotify_session.begin_oauth(client_id.clone())?;

    // Fixed port: Spotify matches redirect URIs exactly, so the dashboard
    // registration must be http://127.0.0.1:8898/callback.
    let listener =
        LoopbackListener::bind_on(auth::OAUTH_LOOPBACK_PORT).map_err(|error| error.to_string())?;
    let redirect_uri = listener.redirect_uri().map_err(|error| error.to_string())?;
    let oauth_state = auth::random_state();
    let pkce = Pkce::generate();
    let url = auth::authorize_url(&client_id, &redirect_uri, &oauth_state, &pkce.challenge)
        .map_err(|error| error.to_string())?;
    app.opener()
        .open_url(url.to_string(), None::<String>)
        .map_err(|error| error.to_string())?;
    let cancelled = Arc::clone(&attempt.cancelled);
    let callback = tauri::async_runtime::spawn_blocking(move || {
        listener.accept_path_cancelled(
            &oauth_state,
            auth::WEB_CALLBACK_PATH,
            Duration::from_secs(180),
            &cancelled,
        )
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())?;
    let token = auth::exchange_code(
        &HttpTransport::new(),
        &client_id,
        &callback.code,
        &redirect_uri,
        &pkce.verifier,
    )
    .await
    .map_err(|error| error.to_string())?;
    let refresh = token
        .refresh_token
        .ok_or_else(|| "Spotify did not return a refresh token".to_string())?;
    let now = unix_now();
    let state = app.state::<AppState>();
    let current_client_id = state
        .settings
        .snapshot()
        .spotify_client_id
        .trim()
        .to_owned();
    let mut commit = attempt.commit_replacement(&current_client_id).await?;
    let mut membership = state.spotify_membership.lock().await;
    let granted_scopes = token.scope.unwrap_or_else(|| auth::SCOPES.clone());
    commit = membership
        .replace_owned(SpotifyLibraryState::default(), commit)
        .await
        .map_err(|error| error.to_string())?;
    clear_spotify_catalog_async(&app).await?;
    let tokens = web_oauth_tokens(
        token.access_token,
        refresh,
        now.saturating_add(token.expires_in),
        granted_scopes,
    );
    let token_store = Arc::clone(&state.token_store);
    tauri::async_runtime::spawn_blocking(move || token_store.save(&tokens))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    *state.spotify.lock().expect("spotify mutex poisoned") = spotify_provider(
        &client_id,
        Arc::clone(&state.token_store),
        Arc::clone(&state.spotify_catalog),
    )?;
    set_auto_connect(&app, true).await?;
    emit_connection_state_async(&app).await?;
    drop(membership);
    drop(commit);
    drop(attempt);
    sync_spotify(&app).await
}

#[tauri::command]
pub(super) async fn authorize_spotify_playback(app: tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let provider = provider_from(&state)?;
    let expected_tokens = state
        .token_store
        .load()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Connect to Spotify before authorizing playback.".to_string())?;
    let attempt = state
        .spotify_session
        .begin_oauth(expected_tokens.access.clone())?;
    let client_id = SessionConfig::default().client_id;
    let listener =
        LoopbackListener::bind_on(auth::OAUTH_LOOPBACK_PORT).map_err(|error| error.to_string())?;
    let redirect_uri = listener
        .redirect_uri_for(auth::PLAYBACK_CALLBACK_PATH)
        .map_err(|error| error.to_string())?;
    let oauth_state = auth::random_state();
    let pkce = Pkce::generate();
    let url = auth::authorize_url_with_scopes(
        &client_id,
        &redirect_uri,
        &oauth_state,
        &pkce.challenge,
        auth::PLAYBACK_SCOPE,
    )
    .map_err(|error| error.to_string())?;
    app.opener()
        .open_url(url.to_string(), None::<String>)
        .map_err(|error| error.to_string())?;
    let cancelled = Arc::clone(&attempt.cancelled);
    let callback = tauri::async_runtime::spawn_blocking(move || {
        listener.accept_path_cancelled(
            &oauth_state,
            auth::PLAYBACK_CALLBACK_PATH,
            Duration::from_secs(180),
            &cancelled,
        )
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())?;
    let token = auth::exchange_code(
        provider.transport(),
        &client_id,
        &callback.code,
        &redirect_uri,
        &pkce.verifier,
    )
    .await
    .map_err(|error| error.to_string())?;

    let session = Session::new(SessionConfig::default(), None);
    session
        .connect(Credentials::with_access_token(token.access_token), false)
        .await
        .map_err(|error| error.to_string())?;
    if let Err(error) = session.login5().auth_token().await {
        session.shutdown();
        return Err(error.to_string());
    }
    let playback_username = session.username();
    let playback_auth_data = session.auth_data();
    session.shutdown();

    let web_account_id = provider
        .me()
        .await
        .map_err(|error| format!("Could not identify the connected Spotify account: {error}"))?
        .id;
    let playback_credentials =
        playback_credentials(&web_account_id, playback_username, playback_auth_data)?;
    let _commit = attempt.commit(&expected_tokens.access).await?;
    let _membership_guard = state.spotify_membership.lock().await;
    let mut tokens = state
        .token_store
        .load()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Connect to Spotify before authorizing playback.".to_string())?;
    let expected = tokens.clone();
    tokens.playback_credentials = Some(playback_credentials);
    if !state
        .token_store
        .replace_if_current(&expected, &tokens)
        .map_err(|error| error.to_string())?
    {
        return Err(
            "The Spotify connection changed during playback authorization. Try again.".into(),
        );
    }
    emit_connection_state_async(&app).await
}

fn playback_credentials(
    web_account_id: &str,
    username: String,
    auth_data: Vec<u8>,
) -> Result<PlaybackCredentials, String> {
    if username.is_empty() || auth_data.is_empty() {
        return Err("Spotify did not return reusable playback credentials.".into());
    }
    if username != web_account_id {
        return Err(
            "Playback authorization used a different Spotify account. Try again with the account connected to Retune."
                .into(),
        );
    }
    Ok(PlaybackCredentials {
        username,
        auth_data,
    })
}

#[tauri::command]
pub(super) async fn disconnect_spotify(app: tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    state.spotify_session.invalidate().await;
    state.sync_orchestrator.cancel_retry();
    let client = state
        .spotify
        .lock()
        .expect("spotify mutex poisoned")
        .clone();
    state.playback.stop(client.as_deref()).await?;
    state.playback.switch_to_connect().await;
    set_auto_connect(&app, false).await?;
    let mut membership = state.spotify_membership.lock().await;
    membership
        .replace(SpotifyLibraryState::default())
        .await
        .map_err(|error| error.to_string())?;
    clear_spotify_catalog_async(&app).await?;
    drop(membership);
    let token_store = Arc::clone(&state.token_store);
    tauri::async_runtime::spawn_blocking(move || token_store.clear())
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    emit_connection_state_async(&app).await?;
    let shuffle = state.settings.snapshot().shuffle;
    emit_main_event(
        &app,
        main_events::MainEvent::PlayerState(empty_player_state(shuffle)),
    )
    .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub(super) async fn track_artwork(
    state: tauri::State<'_, AppState>,
    uri: String,
    min_width: Option<u32>,
) -> Result<Option<String>, String> {
    let provider = provider_from(&state).ok();
    let local_path = crate::authorized_local_artwork_path(
        &state.library.lock().expect("library mutex poisoned"),
        &uri,
    );
    resolve_track_artwork(
        provider.as_deref(),
        &state.artwork_cache,
        local_path,
        &uri,
        min_width.unwrap_or(64).clamp(1, 2048),
    )
    .await
}

#[tauri::command]
pub(super) async fn sync_from_spotify(app: tauri::AppHandle) -> Result<(), String> {
    sync_spotify(&app).await
}

#[tauri::command]
pub(super) async fn spotify_search(
    state: tauri::State<'_, AppState>,
    query: String,
    offset: u32,
) -> Result<SearchResults, String> {
    retune_spotify::client::validate_search_input(&query, offset)
        .map_err(|error| error.to_string())?;
    if query.trim().is_empty() {
        return Ok(SearchResults {
            artists: SearchGroup {
                items: vec![],
                total: 0,
                next_offset: None,
            },
            albums: SearchGroup {
                items: vec![],
                total: 0,
                next_offset: None,
            },
            tracks: SearchGroup {
                items: vec![],
                total: 0,
                next_offset: None,
            },
        });
    }
    if !stored_connection_state(&state.token_store)?.connected {
        return Err("Connect to Spotify to search.".into());
    }
    let provider = provider_from(&state)?;
    let mut results = provider::search(provider.as_ref(), query.trim(), offset).await?;
    let spotify_library = state.spotify_membership.snapshot();
    mark_album_membership(
        &state.library.lock().expect("library mutex poisoned"),
        &spotify_library,
        &mut results.albums.items,
    );
    mark_track_membership(
        &state.library.lock().expect("library mutex poisoned"),
        &spotify_library,
        &mut results.tracks.items,
    );
    Ok(results)
}

#[tauri::command]
pub(super) async fn spotify_album_page(
    state: tauri::State<'_, AppState>,
    uri: String,
) -> Result<AlbumPageView, String> {
    let provider = provider_from(&state)?;
    let album = provider
        .album(spotify_id(&uri, "album")?)
        .await
        .map_err(|error| error.to_string())?;
    let library = state.library.lock().expect("library mutex poisoned");
    let spotify_library = state.spotify_membership.snapshot();
    Ok(album_page_view(&library, &spotify_library, album))
}

#[tauri::command(rename_all = "camelCase")]
pub(super) async fn spotify_artist_page(
    state: tauri::State<'_, AppState>,
    artist_id: String,
) -> Result<ArtistPageView, String> {
    let provider = provider_from(&state)?;
    let id = spotify_id(&artist_id, "artist")?;
    let artist = provider
        .artist(id)
        .await
        .map_err(|error| error.to_string())?;
    let following = required_artist_follow_state(id, provider.is_following_artist(id).await)?;
    Ok(ArtistPageView {
        id: artist.id.clone(),
        name: artist.name.clone(),
        descriptor: artist_descriptor(&artist),
        image_url: image_url(&artist.images),
        following,
    })
}

fn required_artist_follow_state(
    artist_id: &str,
    result: retune_spotify::Result<bool>,
) -> Result<bool, String> {
    result.map_err(|error| {
        format!("Could not read Spotify follow state for artist {artist_id}: {error}")
    })
}

#[tauri::command(rename_all = "camelCase")]
pub(super) async fn spotify_follow_artist(
    state: tauri::State<'_, AppState>,
    artist_id: String,
    follow: bool,
) -> Result<(), String> {
    provider_from(&state)?
        .follow_artist(spotify_id(&artist_id, "artist")?, follow)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub(super) async fn spotify_artist_albums(
    state: tauri::State<'_, AppState>,
    artist_id: String,
    offset: u32,
) -> Result<ArtistAlbumsPage, String> {
    let provider = provider_from(&state)?;
    let mut page = artist_albums_outcome(
        provider.as_ref(),
        &state.cooldown_store,
        &artist_id,
        offset,
        unix_now(),
        chrono::Local::now(),
    )
    .await?;
    mark_album_membership(
        &state.library.lock().expect("library mutex poisoned"),
        &state.spotify_membership.snapshot(),
        &mut page.albums,
    );
    Ok(page)
}

#[tauri::command]
pub(super) async fn add_spotify_album(
    app: tauri::AppHandle,
    uri: String,
    name: String,
    artist: String,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let provider = provider_from(&state)?;
    let library_owner = state.library.owner();
    spotify_membership::save_album(
        provider.as_ref(),
        &state.spotify_membership,
        &library_owner,
        &state.cooldown_store,
        &uri,
        &name,
        &artist,
        unix_now(),
    )
    .await
    .map_err(SpotifyActionFailure::into_message)?;
    emit_main(&app, "library-changed", ()).map_err(|error| error.to_string())
}

#[tauri::command]
pub(super) async fn remove_spotify_album(app: tauri::AppHandle, uri: String) -> Result<(), String> {
    album_id(&uri).ok_or_else(|| "Expected a Spotify album URI".to_string())?;
    let state = app.state::<AppState>();
    let mut membership = state.spotify_membership.lock().await;
    let provider = provider_from(&state)?;
    let current = membership.snapshot();
    if !current.is_exact() {
        spotify_membership::remove_from_library(
            provider.as_ref(),
            &state.cooldown_store,
            &album_library_uris(&uri),
            unix_now(),
        )
        .await
        .map_err(SpotifyActionFailure::into_message)?;
        return app
            .emit_to("main", "library-changed", ())
            .map_err(|error| error.to_string());
    }
    let (album, tracks) = provider::album_content(provider.as_ref(), &uri, None)
        .await
        .map_err(|error| match error {
            provider::AlbumContentError::Spotify(error) => {
                spotify_membership::spotify_action_error(&state.cooldown_store, error)
            }
            provider::AlbumContentError::Other(message) => SpotifyActionFailure::other(message),
        })
        .map_err(SpotifyActionFailure::into_message)?;
    let uris = tracks
        .iter()
        .map(|track| track.uri.clone())
        .collect::<Vec<_>>();
    let aliases = {
        let library = state.library.lock().expect("library mutex poisoned");
        sync::spotify_track_aliases(&library, &tracks)
    };
    let album_uris = album_library_uris(&album.uri);
    spotify_membership::remove_from_library(
        provider.as_ref(),
        &state.cooldown_store,
        &album_uris,
        unix_now(),
    )
    .await
    .map_err(SpotifyActionFailure::into_message)?;
    let mut next = current;
    next.saved_albums.remove(&album.uri);
    membership
        .replace(next.clone())
        .await
        .map_err(SpotifyActionFailure::from)
        .map_err(SpotifyActionFailure::into_message)?;
    let gate = membership.take_gate();
    let ((), gate) = state
        .library
        .owner()
        .mutate_async_owned(
            move |library| {
                sync::prune_unreferenced_spotify_tracks_with_aliases(
                    library, &next, &uris, &aliases,
                );
                Ok(())
            },
            gate,
        )
        .await?;
    membership.restore_gate(gate);
    emit_main(&app, "library-changed", ()).map_err(|error| error.to_string())
}

#[tauri::command]
pub(super) async fn add_spotify_track(app: tauri::AppHandle, uri: String) -> Result<(), String> {
    add_spotify_tracks(app, vec![uri]).await.map(|_| ())
}

#[tauri::command]
pub(super) async fn add_spotify_tracks(
    app: tauri::AppHandle,
    uris: Vec<String>,
) -> Result<Vec<u64>, String> {
    validate_spotify_track_uris(&uris)?;
    let state = app.state::<AppState>();
    let provider = provider_from(&state)?;
    let library_owner = state.library.owner();
    let ids = spotify_membership::save_tracks(
        provider.as_ref(),
        &state.spotify_membership,
        &library_owner,
        &state.cooldown_store,
        uris,
        Vec::new(),
        unix_now(),
    )
    .await
    .map_err(SpotifyActionFailure::into_message)?;
    emit_main(&app, "library-changed", ()).map_err(|error| error.to_string())?;
    Ok(ids)
}

fn validate_spotify_track_uris(uris: &[String]) -> Result<(), String> {
    if uris.len() > retune_spotify::client::MAX_LIBRARY_WRITE_URIS {
        return Err(format!(
            "Cannot add more than {} Spotify tracks at once.",
            retune_spotify::client::MAX_LIBRARY_WRITE_URIS
        ));
    }
    if uris.iter().any(|uri| track_id(uri).is_none()) {
        return Err("Expected Spotify track URIs".into());
    }
    Ok(())
}

#[tauri::command]
pub(super) async fn remove_spotify_track(app: tauri::AppHandle, uri: String) -> Result<(), String> {
    let id = track_id(&uri).ok_or_else(|| "Expected a Spotify track URI".to_string())?;
    let state = app.state::<AppState>();
    let mut membership = state.spotify_membership.lock().await;
    let provider = provider_from(&state)?;
    let current = membership.snapshot();
    let needs_alias = current.is_exact() && {
        let library = state.library.lock().expect("library mutex poisoned");
        !library.tracks().iter().any(|track| track.uri == uri)
            && library.tracks().iter().any(|track| {
                track.source == SourceId::Music && track.uri.starts_with("spotify:track:")
            })
    };
    let aliases = if needs_alias {
        let remote_track = provider
            .track(id)
            .await
            .map_err(|error| spotify_membership::spotify_action_error(&state.cooldown_store, error))
            .map_err(SpotifyActionFailure::into_message)?;
        let candidate = retune_spotify::normalize::track(&remote_track, None, None);
        let library = state.library.lock().expect("library mutex poisoned");
        sync::spotify_track_aliases(&library, std::slice::from_ref(&candidate))
    } else {
        std::collections::HashMap::new()
    };
    let next = if current.is_exact() {
        let mut next = current;
        next.saved_tracks.remove(&uri);
        Some(next)
    } else {
        None
    };
    spotify_membership::remove_from_library(
        provider.as_ref(),
        &state.cooldown_store,
        std::slice::from_ref(&uri),
        unix_now(),
    )
    .await
    .map_err(SpotifyActionFailure::into_message)?;
    if let Some(next) = next {
        membership
            .replace(next.clone())
            .await
            .map_err(SpotifyActionFailure::from)
            .map_err(SpotifyActionFailure::into_message)?;
        let gate = membership.take_gate();
        let ((), gate) = state
            .library
            .owner()
            .mutate_async_owned(
                move |library| {
                    sync::prune_unreferenced_spotify_tracks_with_aliases(
                        library,
                        &next,
                        std::slice::from_ref(&uri),
                        &aliases,
                    );
                    Ok(())
                },
                gate,
            )
            .await?;
        membership.restore_gate(gate);
    }
    emit_main(&app, "library-changed", ()).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{atomic::AtomicUsize, Arc},
        time::Duration,
    };

    use retune_core::model::{AlbumKey, EffectiveRating, Library, NewTrack, Rating, SourceId};
    use retune_spotify::client::{Album, Image, Page, SimplifiedArtist, Track};

    use super::{
        album_page_view, commit_sync_state, normalize_sync_baseline, playback_credentials,
        rating_view, rebase_sync_membership, required_artist_follow_state,
        validate_spotify_track_uris, web_oauth_tokens, SpotifySession,
    };
    use crate::{
        library_state::LibraryState,
        restore_latch::RestoreMutationState,
        spotify_membership::SpotifyMembership,
        store::{
            FsOverlayStore, FsSettingsStore, FsSpotifyLibraryStore, OverlayStore, SaveHook,
            SavedAlbumRecord, Settings, SettingsState, SpotifyLibraryState,
        },
    };

    fn metadata_track(uri: &str, cat: &str, art: &str, alb: &str) -> NewTrack {
        NewTrack {
            uri: uri.into(),
            cat: cat.into(),
            art: art.into(),
            alb: alb.into(),
            name: uri.into(),
            duration: Duration::from_secs(1),
            ..NewTrack::default()
        }
    }

    fn spotify_album() -> Album {
        Album {
            id: "album".into(),
            uri: "spotify:album:album".into(),
            name: "Album".into(),
            artists: vec![SimplifiedArtist {
                id: "artist".into(),
                name: "Artist".into(),
            }],
            images: vec![Image {
                url: "cover".into(),
                width: Some(300),
            }],
            release_date: Some("2024-02-03".into()),
            album_type: Some("compilation".into()),
            total_tracks: 2,
            tracks: Some(Page {
                items: vec![
                    Track {
                        uri: "spotify:track:one".into(),
                        name: "One".into(),
                        duration_ms: Some(1_500),
                        track_number: Some(1),
                        disc_number: Some(1),
                        artists: vec![],
                        album: None,
                    },
                    Track {
                        uri: "spotify:track:two".into(),
                        name: "Two".into(),
                        duration_ms: Some(2_500),
                        track_number: Some(2),
                        disc_number: Some(1),
                        artists: vec![],
                        album: None,
                    },
                ],
                next: None,
                skipped: 0,
                total: 2,
            }),
        }
    }

    #[test]
    fn album_page_resolves_library_ids_ratings_and_completeness() {
        let mut library = Library::new();
        let mut first = metadata_track("spotify:track:one", "Rock", "Artist", "Album");
        first.added_at = Some(42);
        let id = library.add(first);
        library.set_track_rating(id, Rating::new(4)).unwrap();
        library.set_album_rating(
            AlbumKey {
                source: SourceId::Music,
                art: "Artist".into(),
                alb: "Album".into(),
            },
            Rating::new(5),
        );

        let page = album_page_view(&library, &SpotifyLibraryState::default(), spotify_album());

        assert_eq!(page.album_type, "Compilation");
        assert_eq!(page.year.as_deref(), Some("2024"));
        assert_eq!(page.total_duration_secs, 4);
        assert!(!page.saved_album);
        assert!(!page.content_complete);
        assert_eq!(page.added_at, Some(42));
        assert_eq!(page.album_rating, None);
        assert_eq!(page.tracks[0].track_id, Some(id.0));
        assert_eq!(
            page.tracks[0].rating,
            Some(rating_view(EffectiveRating::Explicit(
                Rating::new(4).unwrap()
            )))
        );
        assert_eq!(page.tracks[1].track_id, None);

        library.add(metadata_track(
            "spotify:track:two",
            "Rock",
            "Artist",
            "Album",
        ));
        let page = album_page_view(&library, &SpotifyLibraryState::default(), spotify_album());
        assert!(page.saved_album);
        assert!(page.content_complete);
        assert_eq!(page.album_rating, Some(5));
    }

    #[test]
    fn album_page_resolves_alternate_track_uri_to_retained_overlay() {
        let mut library = Library::new();
        let mut retained = metadata_track("spotify:track:retained", "Rock", "Artist", "Album");
        retained.name = "One".into();
        retained.duration = Duration::from_millis(1_500);
        retained.track_no = Some(1);
        retained.disc_no = Some(1);
        retained.release_date = Some("2024-02-03".into());
        retained.kind = Some("Spotify".into());
        retained.added_at = Some(42);
        let id = library.add(retained);
        library.set_track_rating(id, Rating::new(4)).unwrap();
        library.add(metadata_track(
            "spotify:track:two",
            "Rock",
            "Artist",
            "Album",
        ));
        library.set_album_rating(
            AlbumKey {
                source: SourceId::Music,
                art: "Artist".into(),
                alb: "Album".into(),
            },
            Rating::new(5),
        );

        let mut album = spotify_album();
        album.tracks.as_mut().unwrap().items[0].uri = "spotify:track:alternate".into();
        let page = album_page_view(&library, &SpotifyLibraryState::default(), album);

        assert_eq!(page.tracks[0].track_id, Some(id.0));
        assert_eq!(
            page.tracks[0].rating,
            Some(rating_view(EffectiveRating::Explicit(
                Rating::new(4).unwrap()
            )))
        );
        assert!(page.content_complete);
        assert_eq!(page.album_rating, Some(5));
        assert_eq!(page.added_at, Some(42));
    }

    #[test]
    fn artist_follow_failure_remains_unavailable() {
        let error = required_artist_follow_state(
            "artist",
            Err(retune_spotify::Error::Transport("offline".into())),
        )
        .unwrap_err();

        assert!(error.contains("artist"));
        assert!(error.contains("offline"));
    }

    #[test]
    fn web_oauth_replacement_requires_playback_reauthorization() {
        let tokens = web_oauth_tokens(
            "new-access".into(),
            "new-refresh".into(),
            10,
            "scope".into(),
        );

        assert!(tokens.playback_credentials.is_none());
    }

    #[test]
    fn album_library_actions_use_only_the_album_uri() {
        assert_eq!(
            super::album_library_uris("spotify:album:album"),
            ["spotify:album:album"]
        );
    }

    #[test]
    fn playback_credentials_require_both_session_parts() {
        assert!(playback_credentials("user", String::new(), vec![1]).is_err());
        assert!(playback_credentials("user", "user".into(), vec![]).is_err());
        assert_eq!(
            playback_credentials("user", "user".into(), vec![1, 2, 3])
                .unwrap()
                .auth_data,
            [1, 2, 3]
        );
    }

    #[test]
    fn playback_credentials_reject_a_different_web_account() {
        let error = playback_credentials("web-user", "playback-user".into(), vec![1])
            .err()
            .unwrap();

        assert!(error.contains("different Spotify account"));
    }

    #[test]
    fn bulk_track_input_limit_accepts_exact_and_rejects_one_over() {
        let uri = format!("spotify:track:{}", "x".repeat(64));
        assert!(validate_spotify_track_uris(&vec![
            uri.clone();
            retune_spotify::client::MAX_LIBRARY_WRITE_URIS
        ])
        .is_ok());
        assert!(validate_spotify_track_uris(&vec![
            uri;
            retune_spotify::client::MAX_LIBRARY_WRITE_URIS
                + 1
        ])
        .is_err());
        assert!(
            validate_spotify_track_uris(&[format!("spotify:track:{}", "x".repeat(65))]).is_err()
        );
    }

    fn saved_album(uri: &str) -> SavedAlbumRecord {
        SavedAlbumRecord {
            uri: uri.into(),
            name: uri.into(),
            artists: vec![],
            release_date: None,
            album_type: None,
            added_at: Some(1),
            track_uris: vec![],
        }
    }

    #[test]
    fn delayed_sync_rebases_explicit_membership_adds_and_removes() {
        let baseline = normalize_sync_baseline(
            SpotifyLibraryState {
                account_id: "legacy-profile-id".into(),
                complete: true,
                saved_tracks: BTreeMap::from([
                    ("spotify:track:kept".into(), Some(1)),
                    ("spotify:track:removed".into(), Some(1)),
                ]),
                saved_albums: BTreeMap::from([
                    (
                        "spotify:album:kept".into(),
                        saved_album("spotify:album:kept"),
                    ),
                    (
                        "spotify:album:removed".into(),
                        saved_album("spotify:album:removed"),
                    ),
                ]),
            },
            "legacy-profile-id",
            "account",
        );
        assert_eq!(baseline.account_id, "account");
        let mut current = baseline.clone();
        current.saved_tracks.remove("spotify:track:removed");
        current
            .saved_tracks
            .insert("spotify:track:added".into(), Some(2));
        current.saved_albums.remove("spotify:album:removed");
        current.saved_albums.insert(
            "spotify:album:added".into(),
            saved_album("spotify:album:added"),
        );
        let mut incoming = baseline.clone();
        incoming
            .saved_tracks
            .insert("spotify:track:remote".into(), Some(3));
        incoming.saved_albums.insert(
            "spotify:album:remote".into(),
            saved_album("spotify:album:remote"),
        );

        let rebased = rebase_sync_membership(&baseline, &current, incoming);

        assert_eq!(
            rebased.saved_tracks.keys().cloned().collect::<Vec<_>>(),
            [
                "spotify:track:added",
                "spotify:track:kept",
                "spotify:track:remote"
            ]
        );
        assert_eq!(
            rebased.saved_albums.keys().cloned().collect::<Vec<_>>(),
            [
                "spotify:album:added",
                "spotify:album:kept",
                "spotify:album:remote"
            ]
        );
    }

    #[test]
    fn oauth_is_single_flight() {
        let session = SpotifySession::default();
        let attempt = session.begin_oauth("web-client".into()).unwrap();

        assert!(session.begin_oauth("playback-account".into()).is_err());
        drop(attempt);
        assert!(session.begin_oauth("playback-account".into()).is_ok());
    }

    #[tokio::test]
    async fn invalidation_cancels_waits_and_allows_an_immediate_new_attempt() {
        let session = Arc::new(SpotifySession::default());
        let attempt = session.begin_oauth("old-client".into()).unwrap();
        let invalidating = Arc::clone(&session);
        let invalidation = tokio::spawn(async move { invalidating.invalidate().await });
        while !attempt.cancelled.load(std::sync::atomic::Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        assert!(attempt.ensure_current("old-client").is_err());
        drop(attempt);
        tokio::time::timeout(Duration::from_secs(1), invalidation)
            .await
            .unwrap()
            .unwrap();

        assert!(session.begin_oauth("new-client".into()).is_ok());
    }

    #[tokio::test]
    async fn oauth_cannot_slip_past_invalidation_waiting_for_the_commit_gate() {
        let session = Arc::new(SpotifySession::default());
        let held_commit = session.commit_gate.lock().await;
        let invalidating = Arc::clone(&session);
        let invalidation = tokio::spawn(async move { invalidating.invalidate().await });
        tokio::task::yield_now().await;
        let slipped_attempt = session.begin_oauth("client".into()).unwrap();
        drop(held_commit);
        while !slipped_attempt
            .cancelled
            .load(std::sync::atomic::Ordering::Acquire)
        {
            tokio::task::yield_now().await;
        }
        assert!(session.begin_oauth("other".into()).is_err());
        drop(slipped_attempt);

        tokio::time::timeout(Duration::from_secs(1), invalidation)
            .await
            .unwrap()
            .unwrap();
        assert!(session.begin_oauth("new-client".into()).is_ok());
    }

    #[tokio::test]
    async fn concurrent_invalidations_keep_new_oauth_blocked_until_both_finish() {
        let session = Arc::new(SpotifySession::default());
        let attempt = session.begin_oauth("client".into()).unwrap();
        let first = {
            let session = Arc::clone(&session);
            tokio::spawn(async move { session.invalidate().await })
        };
        let second = {
            let session = Arc::clone(&session);
            tokio::spawn(async move { session.invalidate().await })
        };
        while !attempt.cancelled.load(std::sync::atomic::Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        while session.state.lock().unwrap().active_invalidations != 2 {
            tokio::task::yield_now().await;
        }
        assert!(session.begin_oauth("blocked".into()).is_err());
        drop(attempt);
        first.await.unwrap();
        second.await.unwrap();

        assert!(session.begin_oauth("new-client".into()).is_ok());
    }

    #[tokio::test]
    async fn stale_oauth_and_sync_revisions_make_zero_commits() {
        let session = Arc::new(SpotifySession::default());
        let revision = session.revision();
        let attempt = session.begin_oauth("old-client".into()).unwrap();
        let invalidating = Arc::clone(&session);
        let invalidation = tokio::spawn(async move { invalidating.invalidate().await });
        while !attempt.cancelled.load(std::sync::atomic::Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        let commits = AtomicUsize::new(0);
        if attempt.commit("old-client").await.is_ok() {
            commits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        drop(attempt);
        invalidation.await.unwrap();
        if session.commit_revision(revision).await.is_ok() {
            commits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        assert_eq!(commits.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn client_id_change_during_exchange_makes_zero_commits() {
        let session = SpotifySession::default();
        let attempt = session.begin_oauth("old-client".into()).unwrap();
        let commits = AtomicUsize::new(0);

        if attempt.commit("new-client").await.is_ok() {
            commits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        assert_eq!(commits.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn replacement_oauth_invalidates_an_in_flight_sync() {
        let session = SpotifySession::default();
        let sync_revision = session.revision();
        let attempt = session.begin_oauth("client".into()).unwrap();
        let commit = attempt.commit_replacement("client").await.unwrap();
        drop(commit);

        assert!(session.commit_revision(sync_revision).await.is_err());
        drop(attempt);
        let current_revision = session.revision();
        assert!(tokio::time::timeout(
            Duration::from_millis(100),
            session.commit_revision(current_revision)
        )
        .await
        .unwrap()
        .is_ok());
    }

    #[tokio::test]
    async fn stale_playlist_sync_revision_cannot_enter_its_commit() {
        let session = SpotifySession::default();
        let playlist_revision = session.revision();
        session.invalidate().await;

        assert!(session.commit_revision(playlist_revision).await.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelled_sync_commit_finishes_atomically_and_reports_invalidation_decision() {
        let directory = tempfile::tempdir().unwrap();
        let restore_mutations = Arc::new(RestoreMutationState::default());
        let before_membership = SpotifyLibraryState::default();
        let after_membership = SpotifyLibraryState {
            account_id: "account".into(),
            complete: true,
            ..SpotifyLibraryState::default()
        };
        let before_library = Library::new();
        let mut after_library = Library::new();
        after_library.add(metadata_track(
            "spotify:track:new",
            "Artist",
            "Artist",
            "Album",
        ));
        let before_settings = Settings::default();
        FsSpotifyLibraryStore::new(directory.path())
            .save(&before_membership)
            .unwrap();
        FsOverlayStore::new(directory.path())
            .save(&before_library)
            .unwrap();
        FsSettingsStore::new(directory.path())
            .save(&before_settings)
            .unwrap();
        let membership = SpotifyMembership::new_with_restore_state(
            before_membership,
            FsSpotifyLibraryStore::new(directory.path()),
            Arc::clone(&restore_mutations),
        );
        let library = LibraryState::new_with_restore_state(
            before_library,
            FsOverlayStore::new(directory.path()),
            Arc::clone(&restore_mutations),
        );
        let settings = SettingsState::new_with_restore_state(
            before_settings,
            FsSettingsStore::new(directory.path()),
            Arc::clone(&restore_mutations),
        );
        let session = SpotifySession::default();
        let hook = SaveHook::new(false);
        let task = tokio::spawn(commit_sync_state(
            membership.lock().await,
            library.clone(),
            settings.clone(),
            Arc::clone(&restore_mutations),
            directory.path().to_owned(),
            library.begin_transaction().unwrap(),
            after_membership.clone(),
            after_library.clone(),
            false,
            42,
            session.commit_revision(session.revision()).await.unwrap(),
            Some((1, Arc::clone(&hook))),
        ));
        while !hook.is_reached() {
            tokio::task::yield_now().await;
        }
        hook.wait_until_reached();
        task.abort();
        hook.release();

        tokio::time::timeout(Duration::from_secs(2), async {
            while library.snapshot() != after_library {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(membership.snapshot(), after_membership);
        assert!(settings.snapshot().spotify_sync_completed);
        assert_eq!(
            FsOverlayStore::new(directory.path()).load().unwrap(),
            Some(after_library.clone())
        );
        assert!(!directory.path().join("spotify-sync-journal.json").exists());

        let mut changed_again = after_library;
        changed_again.add(metadata_track(
            "spotify:track:second",
            "Artist",
            "Artist",
            "Album",
        ));
        let receipt = commit_sync_state(
            membership.lock().await,
            library.clone(),
            settings.clone(),
            restore_mutations,
            directory.path().to_owned(),
            library.begin_transaction().unwrap(),
            after_membership,
            changed_again,
            false,
            43,
            session.commit_revision(session.revision()).await.unwrap(),
            None,
        )
        .await
        .unwrap();
        assert!(
            receipt.library_changed,
            "committed library change must invalidate"
        );
    }
}

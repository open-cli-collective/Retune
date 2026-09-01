use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use retune_core::model::{Library, NewTrack, TrackRecord};
use retune_spotify::{
    client::{Album, SpotifyClient, Transport},
    tokens::TokenStore,
};

use crate::{
    library_state::LibraryOwner,
    provider,
    provider::{saved_album_record, AlbumContentError},
    store::{self, FsCooldownStore, FsSpotifyLibraryStore, SpotifyLibraryState},
};

pub(super) fn album_track_uris(album: &Album) -> Vec<String> {
    album
        .tracks
        .as_ref()
        .into_iter()
        .flat_map(|page| &page.items)
        .map(|track| track.uri.clone())
        .collect()
}

pub(crate) fn spotify_track_match<'a>(
    library: &'a Library,
    incoming: &NewTrack,
) -> Option<&'a TrackRecord> {
    let incoming_identity = spotify_new_track_identity(incoming);
    library.tracks().iter().find(|existing| {
        existing.uri == incoming.uri
            || spotify_track_identity(existing)
                .is_some_and(|identity| Some(&identity) == incoming_identity.as_ref())
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SpotifyTrackIdentity {
    source: retune_core::model::SourceId,
    art: String,
    alb: String,
    disc_no: Option<u32>,
    track_no: Option<u32>,
    name: String,
    duration: std::time::Duration,
    release_date: Option<String>,
}

impl SpotifyTrackIdentity {
    pub(crate) fn refreshed(&self, track: &NewTrack) -> Self {
        Self {
            disc_no: track.disc_no,
            track_no: track.track_no,
            release_date: track.release_date.clone(),
            ..self.clone()
        }
    }
}

pub(crate) fn spotify_track_identity(track: &TrackRecord) -> Option<SpotifyTrackIdentity> {
    track
        .uri
        .starts_with("spotify:track:")
        .then(|| SpotifyTrackIdentity {
            source: track.source,
            art: track.art.clone(),
            alb: track.alb.clone(),
            disc_no: track.disc_no,
            track_no: track.track_no,
            name: track.name.clone(),
            duration: track.duration,
            release_date: track.release_date.clone(),
        })
}

pub(crate) fn spotify_new_track_identity(track: &NewTrack) -> Option<SpotifyTrackIdentity> {
    track
        .uri
        .starts_with("spotify:track:")
        .then(|| SpotifyTrackIdentity {
            source: track.source,
            art: track.art.clone(),
            alb: track.alb.clone(),
            disc_no: track.disc_no,
            track_no: track.track_no,
            name: track.name.clone(),
            duration: track.duration,
            release_date: track.release_date.clone(),
        })
}

#[derive(Clone)]
pub(crate) struct SpotifyMembership {
    gate: Arc<tokio::sync::Mutex<()>>,
    current: Arc<Mutex<SpotifyLibraryState>>,
    store: FsSpotifyLibraryStore,
    restore_mutations: Arc<crate::restore_latch::RestoreMutationState>,
}

pub(crate) struct SpotifyMembershipGuard {
    gate: Option<tokio::sync::OwnedMutexGuard<()>>,
    current: Arc<Mutex<SpotifyLibraryState>>,
    store: FsSpotifyLibraryStore,
    restore_mutations: Arc<crate::restore_latch::RestoreMutationState>,
}

impl SpotifyMembership {
    #[cfg(test)]
    pub(crate) fn new(current: SpotifyLibraryState, store: FsSpotifyLibraryStore) -> Self {
        Self::new_with_restore_state(
            current,
            store,
            Arc::new(crate::restore_latch::RestoreMutationState::default()),
        )
    }

    pub(crate) fn new_with_restore_state(
        current: SpotifyLibraryState,
        store: FsSpotifyLibraryStore,
        restore_mutations: Arc<crate::restore_latch::RestoreMutationState>,
    ) -> Self {
        Self {
            gate: Arc::new(tokio::sync::Mutex::new(())),
            current: Arc::new(Mutex::new(current)),
            store,
            restore_mutations,
        }
    }

    pub(crate) fn snapshot(&self) -> SpotifyLibraryState {
        self.current
            .lock()
            .expect("Spotify library mutex poisoned")
            .clone()
    }

    pub(crate) async fn lock(&self) -> SpotifyMembershipGuard {
        SpotifyMembershipGuard {
            gate: Some(Arc::clone(&self.gate).lock_owned().await),
            current: Arc::clone(&self.current),
            store: self.store.clone(),
            restore_mutations: Arc::clone(&self.restore_mutations),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_for_test(&self, current: SpotifyLibraryState) {
        *self.current.lock().expect("Spotify library mutex poisoned") = current;
    }
}

impl SpotifyMembershipGuard {
    pub(crate) fn snapshot(&self) -> SpotifyLibraryState {
        self.current
            .lock()
            .expect("Spotify library mutex poisoned")
            .clone()
    }

    pub(crate) async fn replace(&mut self, next: SpotifyLibraryState) -> store::StoreResult<()> {
        self.replace_owned(next, ()).await
    }

    pub(crate) async fn replace_owned<T: Send + 'static>(
        &mut self,
        next: SpotifyLibraryState,
        owner: T,
    ) -> store::StoreResult<T> {
        self.restore_mutations
            .ensure_allowed()
            .map_err(std::io::Error::other)?;
        let gate = self.gate.take().expect("membership guard is active");
        let store = self.store.clone();
        let saved = next.clone();
        let current = Arc::clone(&self.current);
        let completion = tauri::async_runtime::spawn(async move {
            tauri::async_runtime::spawn_blocking(move || store.save(&saved))
                .await
                .map_err(|error| std::io::Error::other(error.to_string()))??;
            *current.lock().expect("Spotify library mutex poisoned") = next;
            Ok::<_, store::StoreError>((gate, owner))
        });
        let (gate, owner) = completion
            .await
            .map_err(|error| std::io::Error::other(error.to_string()))??;
        self.gate = Some(gate);
        Ok(owner)
    }

    pub(crate) fn install(&self, next: SpotifyLibraryState) {
        *self.current.lock().expect("Spotify library mutex poisoned") = next;
    }

    pub(crate) fn take_gate(&mut self) -> tokio::sync::OwnedMutexGuard<()> {
        self.gate.take().expect("membership guard is active")
    }

    pub(crate) fn restore_gate(&mut self, gate: tokio::sync::OwnedMutexGuard<()>) {
        debug_assert!(self.gate.is_none());
        self.gate = Some(gate);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SpotifyActionFailureKind {
    RateLimited,
    QuotaExhausted,
    Other,
}

#[derive(Debug)]
pub(crate) struct SpotifyActionFailure {
    pub(crate) kind: SpotifyActionFailureKind,
    pub(crate) message: String,
    pub(crate) endpoint_family: Option<String>,
    pub(crate) retry_at: Option<u64>,
    pub(crate) ambiguous_outcome: bool,
    pub(crate) source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

pub(crate) fn record_cooldown(
    cooldown_store: &FsCooldownStore,
    endpoint: &str,
    kind: store::CooldownKind,
    deadline: u64,
    now: u64,
) -> Result<(), String> {
    cooldown_store
        .record_cooldown(endpoint, kind, deadline, now)
        .map_err(|error| error.to_string())
}

impl SpotifyActionFailure {
    pub(crate) fn other(message: impl Into<String>) -> Self {
        Self {
            kind: SpotifyActionFailureKind::Other,
            message: message.into(),
            endpoint_family: None,
            retry_at: None,
            ambiguous_outcome: false,
            source: None,
        }
    }

    pub(crate) fn into_message(self) -> String {
        self.message
    }
}

impl std::fmt::Display for SpotifyActionFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SpotifyActionFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source.as_ref() as &(dyn std::error::Error + 'static))
    }
}

impl From<String> for SpotifyActionFailure {
    fn from(message: String) -> Self {
        Self::other(message)
    }
}

impl From<store::StoreError> for SpotifyActionFailure {
    fn from(error: store::StoreError) -> Self {
        Self {
            kind: SpotifyActionFailureKind::Other,
            message: error.to_string(),
            endpoint_family: None,
            retry_at: None,
            ambiguous_outcome: false,
            source: Some(Box::new(error)),
        }
    }
}

struct SpotifyActionCooldown {
    endpoint: String,
    kind: store::CooldownKind,
    deadline: u64,
}

fn classify_spotify_action_error(
    error: &retune_spotify::Error,
    now: u64,
) -> (
    SpotifyActionFailureKind,
    Option<u64>,
    Option<SpotifyActionCooldown>,
) {
    match error {
        retune_spotify::Error::RateLimited {
            endpoint,
            retry_after_secs,
        } => {
            let deadline = now.saturating_add(*retry_after_secs);
            (
                SpotifyActionFailureKind::RateLimited,
                Some(deadline),
                Some(SpotifyActionCooldown {
                    endpoint: endpoint.clone(),
                    kind: store::CooldownKind::Transient,
                    deadline,
                }),
            )
        }
        retune_spotify::Error::QuotaExceeded {
            endpoint,
            retry_after_secs,
        } => {
            let deadline = retry_after_secs.map(|seconds| now.saturating_add(seconds));
            (
                SpotifyActionFailureKind::QuotaExhausted,
                deadline,
                deadline.map(|deadline| SpotifyActionCooldown {
                    endpoint: endpoint.clone(),
                    kind: store::CooldownKind::Quota,
                    deadline,
                }),
            )
        }
        _ => (SpotifyActionFailureKind::Other, None, None),
    }
}

fn spotify_action_failure(
    cooldown_store: &FsCooldownStore,
    error: retune_spotify::Error,
    now: u64,
) -> SpotifyActionFailure {
    let (kind, retry_at, cooldown) = classify_spotify_action_error(&error, now);
    let endpoint_family = error
        .endpoint()
        .map(retune_spotify::client::endpoint_family);
    let ambiguous_outcome = error.ambiguous_outcome();
    if let Some(cooldown) = cooldown {
        if let Err(persist_error) = record_cooldown(
            cooldown_store,
            &cooldown.endpoint,
            cooldown.kind,
            cooldown.deadline,
            now,
        ) {
            log::warn!("Could not persist Spotify action cooldown: {persist_error}");
        }
    }
    SpotifyActionFailure {
        kind,
        message: error.to_string(),
        endpoint_family,
        retry_at,
        ambiguous_outcome,
        source: Some(Box::new(error)),
    }
}

pub(crate) fn spotify_action_error(
    cooldown_store: &FsCooldownStore,
    error: retune_spotify::Error,
) -> SpotifyActionFailure {
    spotify_action_failure(cooldown_store, error, crate::unix_now())
}

pub(crate) async fn remove_from_library<T: Transport, S: TokenStore>(
    provider: &SpotifyClient<T, S>,
    cooldown_store: &FsCooldownStore,
    uris: &[String],
    now: u64,
) -> Result<(), SpotifyActionFailure> {
    provider
        .remove_from_library(uris)
        .await
        .map_err(|error| spotify_action_failure(cooldown_store, error, now))
}

pub(crate) struct AlbumSaveResult {
    pub(crate) album_uri: String,
}

// This boundary coordinates independent API, membership, library, cooldown, and metadata inputs.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn save_album<T: Transport, S: TokenStore>(
    provider: &SpotifyClient<T, S>,
    membership: &SpotifyMembership,
    library_owner: &LibraryOwner,
    cooldown_store: &FsCooldownStore,
    uri: &str,
    name: &str,
    artist: &str,
    added_at: u64,
) -> Result<AlbumSaveResult, SpotifyActionFailure> {
    let mut membership = membership.lock().await;
    save_album_locked(
        provider,
        &mut membership,
        library_owner,
        cooldown_store,
        uri,
        name,
        artist,
        added_at,
    )
    .await
}

// The locked variant preserves those explicit dependencies for its single caller.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn save_album_locked<T: Transport, S: TokenStore>(
    provider: &SpotifyClient<T, S>,
    membership: &mut SpotifyMembershipGuard,
    library_owner: &LibraryOwner,
    cooldown_store: &FsCooldownStore,
    uri: &str,
    name: &str,
    artist: &str,
    added_at: u64,
) -> Result<AlbumSaveResult, SpotifyActionFailure> {
    crate::album_id(uri)
        .ok_or_else(|| SpotifyActionFailure::other("Expected a Spotify album URI"))?;
    let (album, mut tracks) = provider::album_content(provider, uri, Some(added_at))
        .await
        .map_err(|error| match error {
            AlbumContentError::Spotify(error) => spotify_action_error(cooldown_store, error),
            AlbumContentError::Other(message) => SpotifyActionFailure::other(message),
        })?;
    for track in &mut tracks {
        if track.alb.is_empty() {
            track.alb = name.to_owned();
        }
        if track.art.is_empty() {
            track.art = artist.to_owned();
        }
    }
    let track_uris = album_track_uris(&album);
    let album_record = saved_album_record(&album, track_uris, Some(added_at));
    let album_uris = vec![album.uri.clone()];
    let current = membership.snapshot();
    provider
        .save_to_library(&album_uris)
        .await
        .map_err(|error| spotify_action_error(cooldown_store, error))?;
    if current.is_exact() {
        let mut next = current;
        next.add_saved_album(album_record);
        membership.replace(next).await?;
    }
    let gate = membership.take_gate();
    let ((), gate) = library_owner
        .mutate_async_owned(
            move |library| {
                for track in tracks {
                    if spotify_track_match(library, &track)
                        .is_none_or(|existing| existing.uri == track.uri)
                    {
                        library.upsert(track);
                    }
                }
                Ok(())
            },
            gate,
        )
        .await
        .map_err(SpotifyActionFailure::from)?;
    membership.restore_gate(gate);
    Ok(AlbumSaveResult {
        album_uri: album.uri,
    })
}

pub(crate) async fn save_tracks<T: Transport, S: TokenStore>(
    provider: &SpotifyClient<T, S>,
    membership: &SpotifyMembership,
    library_owner: &LibraryOwner,
    cooldown_store: &FsCooldownStore,
    uris: Vec<String>,
    cached_tracks: Vec<retune_core::model::NewTrack>,
    added_at: u64,
) -> Result<Vec<u64>, SpotifyActionFailure> {
    let mut membership = membership.lock().await;
    save_tracks_locked(
        provider,
        &mut membership,
        library_owner,
        cooldown_store,
        uris,
        cached_tracks,
        added_at,
    )
    .await
}

pub(crate) async fn save_tracks_locked<T: Transport, S: TokenStore>(
    provider: &SpotifyClient<T, S>,
    membership: &mut SpotifyMembershipGuard,
    library_owner: &LibraryOwner,
    cooldown_store: &FsCooldownStore,
    uris: Vec<String>,
    cached_tracks: Vec<retune_core::model::NewTrack>,
    added_at: u64,
) -> Result<Vec<u64>, SpotifyActionFailure> {
    let mut seen = HashSet::new();
    let uris = uris
        .into_iter()
        .filter(|uri| seen.insert(uri.clone()))
        .collect::<Vec<_>>();
    if uris.iter().any(|uri| crate::track_id(uri).is_none()) {
        return Err(SpotifyActionFailure::other("Expected Spotify track URIs"));
    }
    let requested_uris = uris.clone();
    if requested_uris.is_empty() {
        return Ok(vec![]);
    }
    let initially_present = library_owner.read(|library| {
        requested_uris
            .iter()
            .filter(|uri| library.tracks().iter().any(|track| &track.uri == *uri))
            .cloned()
            .collect::<HashSet<_>>()
    })?;
    let missing_uris = uris
        .into_iter()
        .filter(|uri| !initially_present.contains(uri))
        .collect::<Vec<_>>();
    let mut tracks = cached_tracks
        .into_iter()
        .map(|mut track| {
            track.added_at = Some(added_at);
            (track.uri.clone(), track)
        })
        .collect::<HashMap<_, _>>();
    for uri in &missing_uris {
        if tracks.contains_key(uri) {
            continue;
        }
        let track = provider
            .track(crate::track_id(uri).expect("validated above"))
            .await
            .map_err(|error| spotify_action_error(cooldown_store, error))?;
        let artist = match track.artists.first() {
            Some(artist) => match provider.artist(&artist.id).await {
                Ok(artist) => Some(artist),
                Err(error) => {
                    log::warn!(
                        "Spotify track {uri} is missing optional artist enrichment for {}: {error}",
                        artist.id
                    );
                    None
                }
            },
            None => None,
        };
        let mut normalized = retune_spotify::normalize::track(&track, artist.as_ref(), None);
        normalized.added_at = Some(added_at);
        tracks.insert(uri.clone(), normalized);
    }
    let current = membership.snapshot();
    let next = if current.is_exact() {
        let mut next = current;
        for uri in &requested_uris {
            next.add_saved_track(uri.clone(), Some(added_at));
        }
        Some(next)
    } else {
        None
    };
    provider
        .save_to_library(&requested_uris)
        .await
        .map_err(|error| spotify_action_error(cooldown_store, error))?;
    if let Some(next) = next {
        membership.replace(next).await?;
    }
    let gate = membership.take_gate();
    let result = library_owner
        .mutate_async_owned(
            move |library| {
                requested_uris
                    .iter()
                    .map(|uri| {
                        if let Some(track) =
                            library.tracks().iter().find(|track| &track.uri == uri)
                        {
                            return Ok(track.id.0);
                        }
                        let track = tracks.remove(uri).ok_or_else(|| {
                            "The local library changed while Spotify tracks were being saved; retry the action."
                                .to_string()
                        })?;
                        Ok(library.upsert(track).0)
                    })
                    .collect::<Result<Vec<_>, String>>()
            },
            gate,
        )
        .await
        .map_err(SpotifyActionFailure::from)?;
    membership.restore_gate(result.1);
    Ok(result.0)
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{mpsc, Arc, Mutex},
        time::{Duration, Instant},
    };

    use retune_core::model::{Library, NewTrack, SourceId};
    use retune_spotify::{
        client::{fake_client, Request, Response, SendFuture, SpotifyClient, Transport},
        tokens::{InMemoryTokenStore, Tokens},
    };

    use super::{
        classify_spotify_action_error, remove_from_library, save_tracks, spotify_action_failure,
        SpotifyActionFailureKind, SpotifyMembership,
    };
    use crate::store::{FsCooldownStore, FsOverlayStore, OverlayStore, SpotifyLibraryState};

    struct BlockingTransport {
        started: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl Transport for BlockingTransport {
        fn send(&self, _request: Request) -> SendFuture<'_> {
            Box::pin(async move {
                self.started.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
                Ok(Response::json(204, serde_json::Value::Null))
            })
        }
    }

    #[test]
    fn spotify_action_classification_preserves_typed_kind_and_supplied_deadline() {
        let quota = retune_spotify::Error::QuotaExceeded {
            endpoint: "/me/library".into(),
            retry_after_secs: Some(120),
        };
        let (kind, retry_at, cooldown) = classify_spotify_action_error(&quota, 1_000);
        assert_eq!(kind, SpotifyActionFailureKind::QuotaExhausted);
        assert_eq!(retry_at, Some(1_120));
        let cooldown = cooldown.unwrap();
        assert_eq!(cooldown.endpoint, "/me/library");
        assert_eq!(cooldown.kind, crate::store::CooldownKind::Quota);
        assert_eq!(cooldown.deadline, 1_120);
        let unknown = retune_spotify::Error::QuotaExceeded {
            endpoint: "/me/library".into(),
            retry_after_secs: None,
        };
        let (kind, retry_at, cooldown) = classify_spotify_action_error(&unknown, 1_000);
        assert_eq!(kind, SpotifyActionFailureKind::QuotaExhausted);
        assert_eq!(retry_at, None);
        assert!(cooldown.is_none());
    }

    #[test]
    fn cooldown_persistence_failure_does_not_erase_retry_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let blocked = directory.path().join("not-a-directory");
        std::fs::write(&blocked, b"blocked").unwrap();
        let store = FsCooldownStore::new(&blocked);

        let failure = spotify_action_failure(
            &store,
            retune_spotify::Error::RateLimited {
                endpoint: "/me/library".into(),
                retry_after_secs: 17,
            },
            1_000,
        );

        assert_eq!(failure.kind, SpotifyActionFailureKind::RateLimited);
        assert_eq!(failure.retry_at, Some(1_017));
    }

    #[tokio::test]
    async fn rate_limited_removal_persists_typed_deadline_and_source() {
        let directory = tempfile::tempdir().unwrap();
        let store = FsCooldownStore::new(directory.path());
        let client = fake_client([Response::rate_limited("301")], "user-library-modify");

        let failure = remove_from_library(&client, &store, &["spotify:track:one".into()], 1_000)
            .await
            .unwrap_err();

        assert_eq!(failure.kind, SpotifyActionFailureKind::RateLimited);
        assert_eq!(failure.endpoint_family.as_deref(), Some("/me/library"));
        assert_eq!(failure.retry_at, Some(1_301));
        assert!(!failure.ambiguous_outcome);
        assert!(std::error::Error::source(&failure).is_some());
        assert_eq!(
            store.cooldowns(1_000).unwrap()["/me/library"].deadline,
            1_301
        );
    }

    #[test]
    fn ambiguous_mutation_metadata_does_not_depend_on_display_text() {
        let directory = tempfile::tempdir().unwrap();
        let store = FsCooldownStore::new(directory.path());
        let failure = spotify_action_failure(
            &store,
            retune_spotify::Error::AmbiguousMutation {
                endpoint: "/me/tracks".into(),
                status: None,
                detail: "translated transport detail".into(),
                source: None,
            },
            1_000,
        );

        assert_eq!(failure.endpoint_family.as_deref(), Some("/me/tracks"));
        assert!(failure.ambiguous_outcome);
        assert!(std::error::Error::source(&failure).is_some());
    }

    #[test]
    fn store_io_source_remains_discoverable_through_action_failure() {
        let failure = super::SpotifyActionFailure::from(crate::store::StoreError::Io(
            std::io::Error::other("disk unavailable"),
        ));
        let store_error = std::error::Error::source(&failure).unwrap();

        assert!(store_error.source().is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn membership_save_does_not_block_tokio_and_publishes_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::FsSpotifyLibraryStore::new(directory.path());
        let control = store.clone();
        let membership = Arc::new(SpotifyMembership::new(
            SpotifyLibraryState {
                account_id: "before".into(),
                ..SpotifyLibraryState::default()
            },
            store,
        ));
        let hook = crate::store::SaveHook::new(false);
        control.arm_save(Arc::clone(&hook));
        let save = {
            let membership = Arc::clone(&membership);
            tokio::spawn(async move {
                let mut guard = membership.lock().await;
                guard
                    .replace(SpotifyLibraryState {
                        account_id: "after".into(),
                        ..SpotifyLibraryState::default()
                    })
                    .await
            })
        };
        while !hook.is_reached() {
            tokio::task::yield_now().await;
        }
        hook.wait_until_reached();

        let unrelated = tokio::spawn(async { 7 });
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(50), unrelated)
                .await
                .unwrap()
                .unwrap(),
            7
        );
        assert!(!save.is_finished());
        assert_eq!(membership.snapshot().account_id, "before");

        hook.release();
        save.await.unwrap().unwrap();
        assert_eq!(membership.snapshot().account_id, "after");
        assert_eq!(control.load().unwrap(), membership.snapshot());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn saved_track_library_commit_is_nonblocking_and_keeps_membership_owned() {
        let directory = tempfile::tempdir().unwrap();
        let state = Arc::new(crate::test_app_state(
            directory.path(),
            Library::new(),
            SpotifyLibraryState {
                account_id: "account".into(),
                complete: true,
                ..SpotifyLibraryState::default()
            },
            crate::lastfm::Service::new_for_test(directory.path(), true, false),
            crate::lastfm_import::Service::new(directory.path()),
        ));
        let hook = crate::store::SaveHook::new(false);
        state.library.arm_save(Arc::clone(&hook));
        let client = Arc::new(fake_client(
            [Response::json(204, serde_json::Value::Null)],
            "user-library-modify",
        ));
        let track = NewTrack {
            uri: "spotify:track:one".into(),
            name: "One".into(),
            source: SourceId::Music,
            ..NewTrack::default()
        };
        let save = {
            let state = Arc::clone(&state);
            let client = Arc::clone(&client);
            tokio::spawn(async move {
                let library_owner = state.library.owner();
                save_tracks(
                    client.as_ref(),
                    &state.spotify_membership,
                    &library_owner,
                    &state.cooldown_store,
                    vec![track.uri.clone()],
                    vec![track],
                    10,
                )
                .await
            })
        };
        while !hook.is_reached() {
            tokio::task::yield_now().await;
        }
        hook.wait_until_reached();

        assert_eq!(tokio::spawn(async { 7 }).await.unwrap(), 7);
        assert!(state.library.snapshot().tracks().is_empty());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), state.spotify_membership.lock())
                .await
                .is_err()
        );

        hook.release();
        assert_eq!(save.await.unwrap().unwrap().len(), 1);
        assert_eq!(state.library.snapshot().tracks().len(), 1);
    }

    #[tokio::test]
    async fn save_tracks_owns_lock_and_replays_after_overlay_persistence_failure() {
        let directory = tempfile::tempdir().unwrap();
        let lastfm = crate::lastfm::Service::new_for_test(directory.path(), true, false);
        let importer = crate::lastfm_import::Service::new(directory.path());
        let state = Arc::new(crate::test_app_state(
            directory.path(),
            Library::new(),
            SpotifyLibraryState {
                account_id: "account".into(),
                complete: true,
                ..SpotifyLibraryState::default()
            },
            lastfm,
            importer,
        ));
        let client = Arc::new(fake_client(
            [
                Response::json(204, serde_json::Value::Null),
                Response::json(204, serde_json::Value::Null),
            ],
            "user-library-modify",
        ));
        let track = NewTrack {
            uri: "spotify:track:one".into(),
            name: "One".into(),
            source: SourceId::Music,
            ..NewTrack::default()
        };

        let gate = state.spotify_membership.lock().await;
        let library_path = directory.path().join("library.json");
        std::fs::create_dir(&library_path).unwrap();
        let blocked = tokio::spawn({
            let state = Arc::clone(&state);
            let client = Arc::clone(&client);
            let track = track.clone();
            async move {
                let library_owner = state.library.owner();
                save_tracks(
                    client.as_ref(),
                    &state.spotify_membership,
                    &library_owner,
                    &state.cooldown_store,
                    vec![track.uri.clone()],
                    vec![track],
                    10,
                )
                .await
            }
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), async {
                while client.transport().requests().is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_err()
        );
        drop(gate);

        assert!(blocked.await.unwrap().is_err());
        assert!(state
            .spotify_membership
            .snapshot()
            .saved_tracks
            .contains_key("spotify:track:one"));
        assert!(crate::store::FsSpotifyLibraryStore::new(directory.path())
            .load()
            .unwrap()
            .saved_tracks
            .contains_key("spotify:track:one"));
        assert!(state.library.lock().unwrap().tracks().is_empty());
        assert!(!library_path.is_file());
        assert_eq!(client.transport().requests().len(), 1);

        std::fs::remove_dir(&library_path).unwrap();
        let library_owner = state.library.owner();
        let ids = save_tracks(
            client.as_ref(),
            &state.spotify_membership,
            &library_owner,
            &state.cooldown_store,
            vec![track.uri.clone()],
            vec![track],
            10,
        )
        .await
        .unwrap();
        assert_eq!(ids.len(), 1);
        assert!(state
            .spotify_membership
            .snapshot()
            .saved_tracks
            .contains_key("spotify:track:one"));
        assert_eq!(state.library.lock().unwrap().tracks().len(), 1);
        assert_eq!(
            FsOverlayStore::new(directory.path())
                .load()
                .unwrap()
                .unwrap()
                .tracks()
                .len(),
            1
        );
        assert_eq!(client.transport().requests().len(), 2);
    }

    #[test]
    fn save_tracks_rechecks_ids_after_library_transaction() {
        let directory = tempfile::tempdir().unwrap();
        let mut library = Library::new();
        let track = NewTrack {
            uri: "spotify:track:one".into(),
            name: "One".into(),
            source: SourceId::Music,
            ..NewTrack::default()
        };
        let stale_id = library.add(track.clone()).0;
        let state = Arc::new(crate::test_app_state(
            directory.path(),
            library,
            SpotifyLibraryState {
                account_id: "account".into(),
                complete: true,
                ..SpotifyLibraryState::default()
            },
            crate::lastfm::Service::new_for_test(directory.path(), true, false),
            crate::lastfm_import::Service::new(directory.path()),
        ));
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let client = Arc::new(SpotifyClient::new(
            "client",
            BlockingTransport {
                started: started_tx,
                release: Mutex::new(release_rx),
            },
            InMemoryTokenStore::new(Some(Tokens {
                access: "access".into(),
                refresh: "refresh".into(),
                expires_at: u64::MAX,
                scopes: "user-library-modify".into(),
                playback_credentials: None,
            })),
        ));
        let (result_tx, result_rx) = mpsc::channel();
        let worker = std::thread::spawn({
            let state = Arc::clone(&state);
            let client = Arc::clone(&client);
            move || {
                let result = tauri::async_runtime::block_on(async {
                    let library_owner = state.library.owner();
                    save_tracks(
                        client.as_ref(),
                        &state.spotify_membership,
                        &library_owner,
                        &state.cooldown_store,
                        vec![track.uri],
                        vec![],
                        10,
                    )
                    .await
                });
                result_tx.send(result).unwrap();
            }
        });

        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let transaction = state.library.begin_transaction().unwrap();
        state
            .library
            .mutate_in_transaction(|library| {
                *library = Library::new();
                Ok(())
            })
            .unwrap();
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !state
            .spotify_membership
            .snapshot()
            .saved_tracks
            .contains_key("spotify:track:one")
        {
            assert!(Instant::now() < deadline, "membership save did not finish");
            std::thread::yield_now();
        }
        assert!(result_rx.recv_timeout(Duration::from_millis(20)).is_err());
        drop(transaction);

        let failure = result_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap_err();
        worker.join().unwrap();
        assert!(failure.message.contains("retry the action"));
        assert!(state.library.lock().unwrap().tracks().is_empty());
        let durable = FsOverlayStore::new(directory.path())
            .load()
            .unwrap()
            .unwrap();
        assert!(durable.tracks().is_empty());
        assert!(durable.get(retune_core::model::TrackId(stale_id)).is_none());
    }

    #[tokio::test]
    async fn membership_persistence_failure_does_not_advance_local_state() {
        let directory = tempfile::tempdir().unwrap();
        let lastfm = crate::lastfm::Service::new_for_test(directory.path(), true, false);
        let importer = crate::lastfm_import::Service::new(directory.path());
        let state = crate::test_app_state(
            directory.path(),
            Library::new(),
            SpotifyLibraryState {
                account_id: "account".into(),
                complete: true,
                ..SpotifyLibraryState::default()
            },
            lastfm,
            importer,
        );
        let client = fake_client(
            [Response::json(204, serde_json::Value::Null)],
            "user-library-modify",
        );
        let track = NewTrack {
            uri: "spotify:track:one".into(),
            name: "One".into(),
            source: SourceId::Music,
            ..NewTrack::default()
        };
        std::fs::create_dir(directory.path().join("spotify-library.json")).unwrap();

        let library_owner = state.library.owner();
        let result = save_tracks(
            &client,
            &state.spotify_membership,
            &library_owner,
            &state.cooldown_store,
            vec![track.uri.clone()],
            vec![track],
            10,
        )
        .await;

        assert!(result.is_err());
        assert!(state.spotify_membership.snapshot().saved_tracks.is_empty());
        assert!(state.library.lock().unwrap().tracks().is_empty());
        assert_eq!(client.transport().requests().len(), 1);
    }
}

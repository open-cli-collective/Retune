use super::*;

#[cfg(test)]
pub(super) async fn album_candidates<
    T: retune_spotify::client::Transport,
    S: retune_spotify::tokens::TokenStore,
>(
    provider: &retune_spotify::client::SpotifyClient<T, S>,
    query: &str,
    source_album: Option<&str>,
    source_artist: Option<&str>,
    source_track_names: &[String],
) -> Result<Vec<AlbumCandidate>, String> {
    album_candidates_with_source(
        provider,
        query,
        source_album,
        source_artist,
        source_track_names,
    )
    .await
    .map(|(candidates, _)| candidates)
}

pub(super) async fn album_candidates_with_source<
    T: retune_spotify::client::Transport,
    S: retune_spotify::tokens::TokenStore,
>(
    provider: &retune_spotify::client::SpotifyClient<T, S>,
    query: &str,
    source_album: Option<&str>,
    source_artist: Option<&str>,
    source_track_names: &[String],
) -> Result<(Vec<AlbumCandidate>, retune_spotify::client::SearchSource), String> {
    let (results, source) = crate::provider::search_albums_with_source(provider, query).await?;
    let albums = match (source_album, source_artist) {
        (Some(source_album), Some(source_artist)) => supported_album_summaries(
            results.items,
            source_album,
            source_artist,
            source_track_names,
        ),
        _ => results.items.into_iter().take(10).collect(),
    };
    let mut candidates = Vec::new();
    for album in albums {
        let tracks = crate::provider::album_tracks(provider, &album.uri).await?;
        candidates.push(AlbumCandidate {
            uri: album.uri,
            name: album.name,
            artist: album.artist,
            in_library: false,
            track_uris: tracks.iter().map(|track| track.uri.clone()).collect(),
            track_names: tracks.iter().map(|track| track.name.clone()).collect(),
            track_artists: tracks.iter().map(|track| track.art.clone()).collect(),
            track_albums: tracks.iter().map(|track| track.alb.clone()).collect(),
            relation: None,
        });
    }
    classify_album_candidates_by_name(source_track_names, &mut candidates);
    Ok((candidates, source))
}

pub(super) async fn fetch_complete_collection_album<T, S>(
    provider: &retune_spotify::client::SpotifyClient<T, S>,
    uri: &str,
) -> Result<retune_spotify::client::Album, String>
where
    T: retune_spotify::client::Transport,
    S: retune_spotify::tokens::TokenStore,
{
    let album = provider
        .album(crate::provider::spotify_id(uri, "album")?)
        .await
        .map_err(|error| error.to_string())?;
    if !album_tracks_complete(&album) {
        return Err("Spotify returned incomplete album tracks; try again later.".into());
    }
    Ok(album)
}

pub(super) async fn ensure_review_mutable(service: &Service) -> Result<(), String> {
    if service.sync_snapshot().await.accept_all.is_some() {
        return Err("Accept All is applying the confirmed review; wait for it to finish before changing choices.".into());
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct AccountBinding {
    pub(super) lastfm_username: String,
    pub(super) spotify_account_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ReviewBatchKey {
    pub(super) batch_id: u32,
    pub(super) artist: String,
    pub(super) album: String,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn current_account_binding<T, S>(
    service: &Service,
    lastfm: &crate::lastfm::Service,
    membership_guard: &crate::spotify_membership::SpotifyMembershipGuard,
    provider: &impl Fn() -> Result<Arc<retune_spotify::client::SpotifyClient<T, S>>, String>,
    connection_state: impl FnOnce() -> Result<bool, String>,
    require_provider: bool,
    require_spotify_binding: bool,
    allow_suspended: bool,
) -> Result<
    (
        AccountBinding,
        Option<Arc<retune_spotify::client::SpotifyClient<T, S>>>,
    ),
    String,
>
where
    T: retune_spotify::client::Transport,
    S: retune_spotify::tokens::TokenStore,
{
    if !connection_state()? {
        return Err("Connect Spotify before importing its library.".into());
    }
    let lastfm_username = lastfm
        .state()
        .await
        .username
        .ok_or_else(|| "Connect Last.fm before importing its history.".to_string())?;
    let cached_library = membership_guard.snapshot();
    let provider = (!cached_library.is_exact() || require_provider)
        .then(provider)
        .transpose()?;
    let spotify_account_id = if cached_library.is_exact() {
        cached_library.account_id
    } else {
        provider
            .as_ref()
            .expect("non-exact membership resolves a provider")
            .me()
            .await
            .map_err(|error| format!("Could not identify the connected Spotify account: {error}"))?
            .account_id()
            .ok_or_else(|| {
                "Spotify did not return an immutable account ID. Reconnect Spotify before continuing."
                    .to_string()
            })?
            .to_owned()
    };
    let Some(session) = service.snapshot().await else {
        return Err("No Last.fm import session is active.".into());
    };
    if !session_account_matches(
        &session,
        &lastfm_username,
        &spotify_account_id,
        require_spotify_binding,
    ) {
        service.suspend_for_account_mismatch().await?;
        return Err(
            "The saved Last.fm import belongs to a different account; it is suspended for safety."
                .into(),
        );
    }
    if session.phase == ImportPhase::Suspended && !allow_suspended {
        return Err("The Last.fm import is suspended for account safety.".into());
    }
    Ok((
        AccountBinding {
            lastfm_username,
            spotify_account_id,
        },
        provider,
    ))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn review_import<T, S>(
    service: &Service,
    lastfm: &crate::lastfm::Service,
    spotify_membership: &crate::spotify_membership::SpotifyMembership,
    library: &crate::library_state::LibraryState,
    provider: &impl Fn() -> Result<Arc<retune_spotify::client::SpotifyClient<T, S>>, String>,
    connection_state: impl FnOnce() -> Result<bool, String>,
    key: ReviewBatchKey,
    ids: Option<&[String]>,
    action: ReviewAction,
) -> Result<ImportStateView, String>
where
    T: retune_spotify::client::Transport,
    S: retune_spotify::tokens::TokenStore,
{
    ensure_review_mutable(service).await?;
    let membership_guard = spotify_membership.lock().await;
    let binding = current_account_binding(
        service,
        lastfm,
        &membership_guard,
        provider,
        connection_state,
        false,
        true,
        false,
    )
    .await?
    .0;
    service
        .review_action(
            &binding.lastfm_username,
            &binding.spotify_account_id,
            key.batch_id,
            ids,
            action,
            &key.artist,
            &key.album,
        )
        .await?;
    if action.sweeps_backlog() {
        service
            .sweep_backlog_with_mappings(
                library,
                &binding.lastfm_username,
                &binding.spotify_account_id,
            )
            .await?;
    }
    drop(membership_guard);
    current_import_view(service, lastfm).await
}

pub(super) async fn update_import_options<T, S>(
    service: &Service,
    lastfm: &crate::lastfm::Service,
    spotify_membership: &crate::spotify_membership::SpotifyMembership,
    provider: &impl Fn() -> Result<Arc<retune_spotify::client::SpotifyClient<T, S>>, String>,
    connection_state: impl FnOnce() -> Result<bool, String>,
    key: ReviewBatchKey,
    options: PageOptions,
) -> Result<ImportStateView, String>
where
    T: retune_spotify::client::Transport,
    S: retune_spotify::tokens::TokenStore,
{
    ensure_review_mutable(service).await?;
    let membership_guard = spotify_membership.lock().await;
    let binding = current_account_binding(
        service,
        lastfm,
        &membership_guard,
        provider,
        connection_state,
        false,
        true,
        false,
    )
    .await?
    .0;
    service
        .update_options(
            &binding.lastfm_username,
            &binding.spotify_account_id,
            key.batch_id,
            &key.artist,
            &key.album,
            options,
        )
        .await?;
    drop(membership_guard);
    current_import_view(service, lastfm).await
}

pub(super) async fn update_import_count_mode<T, S>(
    service: &Service,
    lastfm: &crate::lastfm::Service,
    spotify_membership: &crate::spotify_membership::SpotifyMembership,
    provider: &impl Fn() -> Result<Arc<retune_spotify::client::SpotifyClient<T, S>>, String>,
    connection_state: impl FnOnce() -> Result<bool, String>,
    target_uri: &str,
    mode: CountMode,
) -> Result<ImportStateView, String>
where
    T: retune_spotify::client::Transport,
    S: retune_spotify::tokens::TokenStore,
{
    ensure_review_mutable(service).await?;
    let membership_guard = spotify_membership.lock().await;
    let binding = current_account_binding(
        service,
        lastfm,
        &membership_guard,
        provider,
        connection_state,
        false,
        true,
        false,
    )
    .await?
    .0;
    service
        .set_count_mode(
            &binding.lastfm_username,
            &binding.spotify_account_id,
            target_uri,
            mode,
        )
        .await?;
    drop(membership_guard);
    current_import_view(service, lastfm).await
}

pub(super) async fn update_import_search_terms<T, S>(
    service: &Service,
    lastfm: &crate::lastfm::Service,
    spotify_membership: &crate::spotify_membership::SpotifyMembership,
    provider: &impl Fn() -> Result<Arc<retune_spotify::client::SpotifyClient<T, S>>, String>,
    connection_state: impl FnOnce() -> Result<bool, String>,
    show: bool,
) -> Result<ImportStateView, String>
where
    T: retune_spotify::client::Transport,
    S: retune_spotify::tokens::TokenStore,
{
    let membership_guard = spotify_membership.lock().await;
    let binding = current_account_binding(
        service,
        lastfm,
        &membership_guard,
        provider,
        connection_state,
        false,
        true,
        false,
    )
    .await?
    .0;
    service
        .set_search_terms(&binding.lastfm_username, &binding.spotify_account_id, show)
        .await?;
    drop(membership_guard);
    current_import_view(service, lastfm).await
}

pub(super) async fn select_import_matches<T, S>(
    service: &Service,
    lastfm: &crate::lastfm::Service,
    spotify_membership: &crate::spotify_membership::SpotifyMembership,
    provider: &impl Fn() -> Result<Arc<retune_spotify::client::SpotifyClient<T, S>>, String>,
    connection_state: impl FnOnce() -> Result<bool, String>,
    batch_id: u32,
    selections: &[(String, String)],
) -> Result<(Option<ImportPageView>, ImportStateView), String>
where
    T: retune_spotify::client::Transport,
    S: retune_spotify::tokens::TokenStore,
{
    ensure_review_mutable(service).await?;
    let membership_guard = spotify_membership.lock().await;
    let binding = current_account_binding(
        service,
        lastfm,
        &membership_guard,
        provider,
        connection_state,
        false,
        true,
        false,
    )
    .await?
    .0;
    let (artist, album) = service
        .select_matches(
            &binding.lastfm_username,
            &binding.spotify_account_id,
            batch_id,
            selections,
        )
        .await?;
    let page = service.page(batch_id, &artist, &album).await;
    drop(membership_guard);
    let view = current_import_view(service, lastfm).await?;
    Ok((page, view))
}

pub(super) fn valid_collection_album_uri(uri: &str) -> bool {
    uri.strip_prefix("spotify:album:")
        .is_some_and(|id| !id.is_empty() && !id.contains(':'))
}

pub(super) fn spotify_share_uri(
    value: &str,
    expected_kind: &str,
) -> Result<Option<String>, String> {
    let value = value.trim();
    let parsed = if let Some(rest) = value.strip_prefix("spotify://") {
        let parts = rest.trim_end_matches('/').split('/').collect::<Vec<_>>();
        (parts.len() == 2).then(|| (parts[0], parts[1]))
    } else if let Some(rest) = value.strip_prefix("spotify:") {
        let parts = rest.split(':').collect::<Vec<_>>();
        (parts.len() == 2).then(|| (parts[0], parts[1]))
    } else if let Some(rest) = value.strip_prefix("https://open.spotify.com/") {
        let path = rest.split(['?', '#']).next().unwrap_or_default();
        let mut parts = path.trim_end_matches('/').split('/').collect::<Vec<_>>();
        if parts.first().is_some_and(|part| part.starts_with("intl-")) {
            parts.remove(0);
        }
        (parts.len() == 2).then(|| (parts[0], parts[1]))
    } else {
        return Ok(None);
    };
    let Some((kind, id)) = parsed else {
        return Err(format!(
            "Paste a valid Spotify {expected_kind} link or URI."
        ));
    };
    if kind != expected_kind {
        return Err(format!(
            "That Spotify link points to a Spotify {kind}, not a Spotify {expected_kind}."
        ));
    }
    if id.is_empty()
        || !id
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
    {
        return Err(format!(
            "Paste a valid Spotify {expected_kind} link or URI."
        ));
    }
    Ok(Some(format!("spotify:{kind}:{id}")))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn load_collection_album_candidate<T, S>(
    service: &Service,
    lastfm: &crate::lastfm::Service,
    spotify_membership: &crate::spotify_membership::SpotifyMembership,
    library: &crate::library_state::LibraryState,
    provider: &impl Fn() -> Result<Arc<retune_spotify::client::SpotifyClient<T, S>>, String>,
    connection_state: &impl Fn() -> Result<bool, String>,
    batch_id: u32,
    artist: &str,
    uri: &str,
) -> Result<(AccountBinding, Option<CollectionAlbumCandidate>), String>
where
    T: retune_spotify::client::Transport,
    S: retune_spotify::tokens::TokenStore,
{
    let (initial, resolved_provider) = {
        let guard = spotify_membership.lock().await;
        let (binding, resolved_provider) = current_account_binding(
            service,
            lastfm,
            &guard,
            provider,
            connection_state,
            true,
            true,
            false,
        )
        .await?;
        let session = service
            .snapshot()
            .await
            .ok_or_else(|| "No Last.fm import session is active.".to_string())?;
        requested_collection_batch(&session, batch_id, artist)?;
        if session
            .collection_album_matches
            .get(&batch_id)
            .is_some_and(|matches| {
                matches
                    .cached_candidates
                    .iter()
                    .any(|candidate| candidate.matching.uri == uri)
            })
        {
            drop(guard);
            return Ok((binding, None));
        }
        drop(guard);
        (
            binding,
            resolved_provider.expect("required provider is resolved"),
        )
    };
    let album = fetch_complete_collection_album(resolved_provider.as_ref(), uri).await?;
    let guard = spotify_membership.lock().await;
    let current = current_account_binding(
        service,
        lastfm,
        &guard,
        provider,
        connection_state,
        false,
        true,
        false,
    )
    .await?
    .0;
    if current != initial {
        service.suspend_for_account_mismatch().await?;
        return Err("The connected Spotify account changed while loading the album; the import is suspended for safety.".into());
    }
    let session = service
        .snapshot()
        .await
        .ok_or_else(|| "No Last.fm import session is active.".to_string())?;
    requested_collection_batch(&session, batch_id, artist)?;
    let candidate = collection_album_candidate(
        &album,
        &collection_membership_from(library, spotify_membership),
    );
    drop(guard);
    Ok((initial, Some(candidate)))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn search_collection_albums_with_source<T, S>(
    service: &Service,
    lastfm: &crate::lastfm::Service,
    spotify_membership: &crate::spotify_membership::SpotifyMembership,
    library: &crate::library_state::LibraryState,
    provider: &impl Fn() -> Result<Arc<retune_spotify::client::SpotifyClient<T, S>>, String>,
    connection_state: &impl Fn() -> Result<bool, String>,
    batch_id: u32,
    artist: &str,
    query: &str,
) -> Result<
    (
        Vec<CollectionAlbumCandidate>,
        retune_spotify::client::SearchSource,
    ),
    String,
>
where
    T: retune_spotify::client::Transport,
    S: retune_spotify::tokens::TokenStore,
{
    ensure_review_mutable(service).await?;
    let query = query.trim();
    if query.is_empty() {
        return Ok((Vec::new(), retune_spotify::client::SearchSource::Cache));
    }
    let direct_uri = spotify_share_uri(query, "album")?;
    let search_term = collection_album_search_term(query);
    let (initial, resolved_provider) = {
        let guard = spotify_membership.lock().await;
        let (binding, resolved_provider) = current_account_binding(
            service,
            lastfm,
            &guard,
            provider,
            connection_state,
            true,
            true,
            false,
        )
        .await?;
        let session = service
            .snapshot()
            .await
            .ok_or_else(|| "No Last.fm import session is active.".to_string())?;
        requested_collection_batch(&session, batch_id, artist)?;
        drop(guard);
        (
            binding,
            resolved_provider.expect("required provider is resolved"),
        )
    };
    let direct_album = if let Some(uri) = direct_uri {
        Some(fetch_complete_collection_album(resolved_provider.as_ref(), &uri).await?)
    } else {
        None
    };
    let (results, source) = if direct_album.is_none() {
        let (results, source) =
            crate::provider::search_albums_with_source(resolved_provider.as_ref(), &search_term)
                .await?;
        (Some(results), source)
    } else {
        (None, retune_spotify::client::SearchSource::Cache)
    };
    let guard = spotify_membership.lock().await;
    let current = current_account_binding(
        service,
        lastfm,
        &guard,
        provider,
        connection_state,
        false,
        true,
        false,
    )
    .await?
    .0;
    if current != initial {
        service.suspend_for_account_mismatch().await?;
        return Err("The connected Spotify account changed while searching; the import is suspended for safety.".into());
    }
    let session = service
        .snapshot()
        .await
        .ok_or_else(|| "No Last.fm import session is active.".to_string())?;
    requested_collection_batch(&session, batch_id, artist)?;
    let membership = collection_membership_from(library, spotify_membership);
    let output = if let Some(album) = direct_album {
        vec![collection_album_candidate(&album, &membership)]
    } else {
        results
            .expect("text search results are present")
            .items
            .into_iter()
            .map(collection_album_summary)
            .collect()
    };
    drop(guard);
    Ok((output, source))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn preview_or_add_collection_album<T, S>(
    service: &Service,
    lastfm: &crate::lastfm::Service,
    spotify_membership: &crate::spotify_membership::SpotifyMembership,
    library: &crate::library_state::LibraryState,
    provider: &impl Fn() -> Result<Arc<retune_spotify::client::SpotifyClient<T, S>>, String>,
    connection_state: &impl Fn() -> Result<bool, String>,
    batch_id: u32,
    artist: &str,
    uri: &str,
    add: bool,
) -> Result<(Option<ImportPageView>, ImportStateView), String>
where
    T: retune_spotify::client::Transport,
    S: retune_spotify::tokens::TokenStore,
{
    if !valid_collection_album_uri(uri) {
        return Err("Choose a valid Spotify album URI.".into());
    }
    ensure_review_mutable(service).await?;
    let (expected, candidate) = load_collection_album_candidate(
        service,
        lastfm,
        spotify_membership,
        library,
        provider,
        connection_state,
        batch_id,
        artist,
        uri,
    )
    .await?;
    let guard = spotify_membership.lock().await;
    let binding = current_account_binding(
        service,
        lastfm,
        &guard,
        provider,
        connection_state,
        false,
        true,
        false,
    )
    .await?
    .0;
    if binding != expected {
        service.suspend_for_account_mismatch().await?;
        return Err("The connected Spotify account changed while loading the album; the import is suspended for safety.".into());
    }
    let membership = collection_membership_from(library, spotify_membership);
    let mappings = service
        .mappings_for(&binding.lastfm_username, Some(&binding.spotify_account_id))
        .await?;
    if add {
        service
            .add_collection_album(
                &binding.lastfm_username,
                &binding.spotify_account_id,
                batch_id,
                artist,
                uri,
                candidate,
                &membership,
                &mappings,
            )
            .await?;
    } else {
        if let Some(candidate) = candidate {
            service
                .cache_collection_album(
                    &binding.lastfm_username,
                    &binding.spotify_account_id,
                    batch_id,
                    artist,
                    candidate,
                )
                .await?;
        }
        service
            .rerank_collection_batch(batch_id, &membership, &mappings)
            .await?;
    }
    let session = service
        .snapshot()
        .await
        .ok_or_else(|| "No Last.fm import session is active.".to_string())?;
    let (_, actual_album) = requested_collection_batch_with_album(&session, batch_id, artist)?;
    let page = service.page(batch_id, artist, &actual_album).await;
    let view = service.state().await;
    drop(guard);
    Ok((page, view))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn remove_collection_album<T, S>(
    service: &Service,
    lastfm: &crate::lastfm::Service,
    spotify_membership: &crate::spotify_membership::SpotifyMembership,
    library: &crate::library_state::LibraryState,
    provider: &impl Fn() -> Result<Arc<retune_spotify::client::SpotifyClient<T, S>>, String>,
    connection_state: impl FnOnce() -> Result<bool, String>,
    batch_id: u32,
    artist: &str,
    uri: &str,
) -> Result<(Option<ImportPageView>, ImportStateView), String>
where
    T: retune_spotify::client::Transport,
    S: retune_spotify::tokens::TokenStore,
{
    if !valid_collection_album_uri(uri) {
        return Err("Choose a valid Spotify album URI.".into());
    }
    ensure_review_mutable(service).await?;
    let guard = spotify_membership.lock().await;
    let binding = current_account_binding(
        service,
        lastfm,
        &guard,
        provider,
        connection_state,
        false,
        true,
        false,
    )
    .await?
    .0;
    let membership = collection_membership_from(library, spotify_membership);
    let mappings = service
        .mappings_for(&binding.lastfm_username, Some(&binding.spotify_account_id))
        .await?;
    service
        .remove_collection_album(
            &binding.lastfm_username,
            &binding.spotify_account_id,
            batch_id,
            artist,
            uri,
            &membership,
            &mappings,
        )
        .await?;
    let session = service
        .snapshot()
        .await
        .ok_or_else(|| "No Last.fm import session is active.".to_string())?;
    let (_, actual_album) = requested_collection_batch_with_album(&session, batch_id, artist)?;
    let page = service.page(batch_id, artist, &actual_album).await;
    let view = service.state().await;
    drop(guard);
    Ok((page, view))
}

pub(super) async fn activate_collection<T, S>(
    service: &Service,
    lastfm: &crate::lastfm::Service,
    spotify_membership: &crate::spotify_membership::SpotifyMembership,
    library: &crate::library_state::LibraryState,
    provider: &impl Fn() -> Result<Arc<retune_spotify::client::SpotifyClient<T, S>>, String>,
    connection_state: impl FnOnce() -> Result<bool, String>,
    key: ReviewBatchKey,
) -> Result<(Option<ImportPageView>, ImportStateView), String>
where
    T: retune_spotify::client::Transport,
    S: retune_spotify::tokens::TokenStore,
{
    ensure_review_mutable(service).await?;
    let guard = spotify_membership.lock().await;
    let binding = current_account_binding(
        service,
        lastfm,
        &guard,
        provider,
        connection_state,
        false,
        true,
        false,
    )
    .await?
    .0;
    let membership = collection_membership_from(library, spotify_membership);
    let mappings = service
        .mappings_for(&binding.lastfm_username, Some(&binding.spotify_account_id))
        .await?;
    let actual_album = service
        .activate_collection_batch(
            &binding.lastfm_username,
            &binding.spotify_account_id,
            key.batch_id,
            &key.artist,
            &key.album,
            &membership,
            &mappings,
        )
        .await?;
    let page = service.page(key.batch_id, &key.artist, &actual_album).await;
    drop(guard);
    let view = current_import_view(service, lastfm).await?;
    Ok((page, view))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn change_import_track_with_source<T, S>(
    service: &Service,
    lastfm: &crate::lastfm::Service,
    spotify_membership: &crate::spotify_membership::SpotifyMembership,
    library: &crate::library_state::LibraryState,
    provider: &impl Fn() -> Result<Arc<retune_spotify::client::SpotifyClient<T, S>>, String>,
    connection_state: &impl Fn() -> Result<bool, String>,
    batch_id: u32,
    id: &str,
    query: &str,
) -> Result<
    (
        (Option<ImportPageView>, ImportStateView),
        retune_spotify::client::SearchSource,
    ),
    String,
>
where
    T: retune_spotify::client::Transport,
    S: retune_spotify::tokens::TokenStore,
{
    ensure_review_mutable(service).await?;
    let (membership, resolved_provider) = {
        let guard = spotify_membership.lock().await;
        let (_, resolved_provider) = current_account_binding(
            service,
            lastfm,
            &guard,
            provider,
            connection_state,
            true,
            true,
            false,
        )
        .await?;
        let membership = collection_membership_from(library, spotify_membership);
        drop(guard);
        (
            membership,
            resolved_provider.expect("required provider is resolved"),
        )
    };
    let session = service
        .snapshot()
        .await
        .ok_or_else(|| "No Last.fm import session is active.".to_string())?;
    let row = session
        .rows
        .iter()
        .find(|row| row.stable_id == id)
        .cloned()
        .ok_or_else(|| "Unknown Last.fm import source row.".to_string())?;
    let batch = requested_batch_containing_source(&session, batch_id, &row.stable_id)
        .ok_or_else(|| "The source row does not belong to this review batch.".to_string())?;
    let rows_by_id = source_row_map(&session);
    let projection = batch_projection(&batch, &batch_rows(&batch, &rows_by_id));
    let collection_shaped = batch_is_collection_shaped_for_id(&session, batch_id);
    let search_term = if query.trim().is_empty() {
        track_search_term(&row.artist, &row.track)
    } else {
        query.trim().to_owned()
    };
    let (candidates, source) = if let Some(uri) = spotify_share_uri(&search_term, "track")? {
        let track = resolved_provider
            .track(crate::provider::spotify_id(&uri, "track")?)
            .await
            .map_err(|error| error.to_string())?;
        let artist = track
            .artists
            .first()
            .map(|artist| artist.name.clone())
            .unwrap_or_default();
        let album = track
            .album
            .as_ref()
            .map(|album| album.name.clone())
            .unwrap_or_default();
        (
            vec![AlbumCandidate {
                uri: track.uri.clone(),
                name: track.name.clone(),
                artist: artist.clone(),
                in_library: membership.contains(&track.uri),
                track_uris: vec![track.uri],
                track_names: vec![track.name],
                track_artists: vec![artist],
                track_albums: vec![album],
                relation: None,
            }],
            retune_spotify::client::SearchSource::Cache,
        )
    } else {
        let (results, source) =
            crate::provider::search_tracks_with_source(resolved_provider.as_ref(), &search_term)
                .await?;
        (
            results
                .items
                .into_iter()
                .map(|track| AlbumCandidate {
                    uri: track.uri.clone(),
                    name: track.name.clone(),
                    artist: track.artist.clone(),
                    in_library: membership.contains(&track.uri),
                    track_uris: vec![track.uri.clone()],
                    track_names: vec![track.name.clone()],
                    track_artists: vec![track.artist],
                    track_albums: vec![track.alb],
                    relation: None,
                })
                .collect(),
            source,
        )
    };
    let mut candidates = candidates;
    if row.album.is_empty() || collection_shaped {
        rank_collection_candidates(&row, &mut candidates, &membership);
    } else {
        classify_album_candidates_for_rows(std::slice::from_ref(&row), &mut candidates);
    }
    let result = if row.album.is_empty() || collection_shaped {
        match_result_for(id.to_owned(), search_term, candidates, &row.track, None)
    } else {
        match_result_for_release(&row, search_term, candidates, None)
    };
    let result = preserve_match_selection(result, session.matches.get(id), id, &row.track);
    let guard = spotify_membership.lock().await;
    let binding = current_account_binding(
        service,
        lastfm,
        &guard,
        provider,
        connection_state,
        false,
        true,
        false,
    )
    .await?
    .0;
    service
        .set_match(
            &binding.lastfm_username,
            &binding.spotify_account_id,
            batch_id,
            result,
        )
        .await?;
    let page = service
        .page(
            batch_id,
            &projection.representative_artist,
            &projection.representative_album,
        )
        .await;
    drop(guard);
    let view = current_import_view(service, lastfm).await?;
    Ok(((page, view), source))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn change_import_album_with_source<T, S>(
    service: &Service,
    lastfm: &crate::lastfm::Service,
    spotify_membership: &crate::spotify_membership::SpotifyMembership,
    provider: &impl Fn() -> Result<Arc<retune_spotify::client::SpotifyClient<T, S>>, String>,
    connection_state: &impl Fn() -> Result<bool, String>,
    batch_id: u32,
    id: &str,
    query: &str,
) -> Result<(ImportStateView, retune_spotify::client::SearchSource), String>
where
    T: retune_spotify::client::Transport,
    S: retune_spotify::tokens::TokenStore,
{
    ensure_review_mutable(service).await?;
    let resolved_provider = {
        let guard = spotify_membership.lock().await;
        let (_, resolved_provider) = current_account_binding(
            service,
            lastfm,
            &guard,
            provider,
            connection_state,
            true,
            true,
            false,
        )
        .await?;
        drop(guard);
        resolved_provider.expect("required provider is resolved")
    };
    let session = service
        .snapshot()
        .await
        .ok_or_else(|| "No Last.fm import session is active.".to_string())?;
    let row = session
        .rows
        .iter()
        .find(|row| row.stable_id == id)
        .cloned()
        .ok_or_else(|| "Unknown Last.fm import source row.".to_string())?;
    let batch = requested_batch_containing_source(&session, batch_id, &row.stable_id)
        .ok_or_else(|| "The source row does not belong to this review batch.".to_string())?;
    let rows_by_id = source_row_map(&session);
    let related_rows = batch_rows(&batch, &rows_by_id)
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let related_refs = related_rows.iter().collect::<Vec<_>>();
    if batch_projection(&batch, &related_refs).collection_shaped
        || is_converted_collection_batch(&session, batch_id, &row.album)
    {
        return Err(
            "Changing the Spotify release is unavailable after switching to album matches.".into(),
        );
    }
    let related = related_rows
        .iter()
        .map(|row| row.track.clone())
        .collect::<Vec<_>>();
    let search_term = if query.trim().is_empty() {
        album_search_term(&row.artist, &row.album)
    } else {
        query.trim().to_owned()
    };
    let (mut candidates, source) = if let Some(uri) = spotify_share_uri(&search_term, "album")? {
        (
            vec![
                collection_album_candidate(
                    &fetch_complete_collection_album(resolved_provider.as_ref(), &uri).await?,
                    &CollectionMembership::default(),
                )
                .matching,
            ],
            retune_spotify::client::SearchSource::Cache,
        )
    } else {
        album_candidates_with_source(
            resolved_provider.as_ref(),
            &search_term,
            None,
            None,
            &related,
        )
        .await?
    };
    classify_album_candidates_for_rows(&related_rows, &mut candidates);
    let selected_uri = automatic_album_candidate_for_rows(&row.album, &related_rows, &candidates)
        .map(|candidate| candidate.uri.clone());
    let matches = related_rows
        .iter()
        .map(|candidate_row| {
            preserve_match_selection(
                match_result_for_release(
                    candidate_row,
                    search_term.clone(),
                    candidates.clone(),
                    selected_uri.as_deref(),
                ),
                session.matches.get(&candidate_row.stable_id),
                &candidate_row.stable_id,
                &candidate_row.track,
            )
        })
        .collect();
    let guard = spotify_membership.lock().await;
    let binding = current_account_binding(
        service,
        lastfm,
        &guard,
        provider,
        connection_state,
        false,
        true,
        false,
    )
    .await?
    .0;
    service
        .set_matches(
            &binding.lastfm_username,
            &binding.spotify_account_id,
            batch_id,
            matches,
            None,
        )
        .await?;
    drop(guard);
    Ok((current_import_view(service, lastfm).await?, source))
}

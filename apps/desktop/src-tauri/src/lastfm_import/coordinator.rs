use super::*;

#[cfg(test)]
pub(super) async fn automatic_collection_album_seed<T, S>(
    provider: &retune_spotify::client::SpotifyClient<T, S>,
    artist: &str,
    album: &str,
    rows: &[SourceRow],
) -> Result<(Vec<CollectionAlbumCandidate>, Option<String>), String>
where
    T: retune_spotify::client::Transport,
    S: retune_spotify::tokens::TokenStore,
{
    automatic_collection_album_seed_with_source(provider, artist, album, rows)
        .await
        .map(|(candidates, selected_uri, _)| (candidates, selected_uri))
}

pub(super) async fn automatic_collection_album_seed_with_source<T, S>(
    provider: &retune_spotify::client::SpotifyClient<T, S>,
    artist: &str,
    album: &str,
    rows: &[SourceRow],
) -> Result<
    (
        Vec<CollectionAlbumCandidate>,
        Option<String>,
        retune_spotify::client::SearchSource,
    ),
    String,
>
where
    T: retune_spotify::client::Transport,
    S: retune_spotify::tokens::TokenStore,
{
    let source_track_names = rows.iter().map(|row| row.track.clone()).collect::<Vec<_>>();
    let (mut candidates, source) = album_candidates_with_source(
        provider,
        &album_search_term(artist, album),
        Some(album),
        Some(artist),
        &source_track_names,
    )
    .await?;
    classify_album_candidates_for_rows(rows, &mut candidates);
    let selected_uri = automatic_album_candidate_for_rows(album, rows, &candidates)
        .map(|candidate| candidate.uri.clone());
    Ok((
        candidates
            .into_iter()
            .map(collection_album_candidate_from_release)
            .collect(),
        selected_uri,
        source,
    ))
}

#[cfg(test)]
pub(super) async fn match_batch<T, S>(
    provider: &retune_spotify::client::SpotifyClient<T, S>,
    artist: &str,
    album: &str,
    collection_shaped: bool,
    rows: &[SourceRow],
) -> Result<Vec<MatchResult>, String>
where
    T: retune_spotify::client::Transport,
    S: retune_spotify::tokens::TokenStore,
{
    match_batch_with_source(provider, artist, album, collection_shaped, rows)
        .await
        .map(|(matches, _)| matches)
}

pub(super) async fn match_batch_with_source<T, S>(
    provider: &retune_spotify::client::SpotifyClient<T, S>,
    artist: &str,
    album: &str,
    collection_shaped: bool,
    rows: &[SourceRow],
) -> Result<(Vec<MatchResult>, retune_spotify::client::SearchSource), String>
where
    T: retune_spotify::client::Transport,
    S: retune_spotify::tokens::TokenStore,
{
    if album.is_empty() || collection_shaped {
        return Ok((Vec::new(), retune_spotify::client::SearchSource::Cache));
    }
    let search_term = album_search_term(artist, album);
    let source_track_names = rows.iter().map(|row| row.track.clone()).collect::<Vec<_>>();
    let (mut candidates, source) = album_candidates_with_source(
        provider,
        &search_term,
        Some(album),
        Some(artist),
        &source_track_names,
    )
    .await?;
    classify_album_candidates_for_rows(rows, &mut candidates);
    let selected_uri = automatic_album_candidate_for_rows(album, rows, &candidates)
        .map(|candidate| candidate.uri.clone());
    Ok((
        rows.iter()
            .map(|row| {
                match_result_for_release(
                    row,
                    search_term.clone(),
                    candidates.clone(),
                    selected_uri.as_deref(),
                )
            })
            .collect(),
        source,
    ))
}

pub(super) async fn current_matching_account<T, S>(
    service: &Service,
    lastfm: &crate::lastfm::Service,
    membership_guard: &crate::spotify_membership::SpotifyMembershipGuard,
    provider: &impl Fn() -> Result<Arc<retune_spotify::client::SpotifyClient<T, S>>, String>,
    connection_state: impl FnOnce() -> Result<bool, String>,
    require_provider: bool,
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
    let current = current_account_binding(
        service,
        lastfm,
        membership_guard,
        provider,
        connection_state,
        require_provider,
        false,
        false,
    )
    .await?;
    let Some(session) = service.snapshot().await else {
        return Err("No Last.fm import session is active.".into());
    };
    if !review_phase_allowed(session.phase) {
        return Err("Last.fm matching is available only after source review begins.".into());
    }
    Ok(current)
}

pub(super) fn session_account_matches(
    session: &LastFmImportSessionV2,
    username: &str,
    spotify_account_id: &str,
    require_spotify_binding: bool,
) -> bool {
    session.lastfm_username == username
        && session
            .spotify_account_id
            .as_deref()
            .map_or(!require_spotify_binding, |bound| {
                bound == spotify_account_id
            })
}

pub(super) async fn cached_spotify_binding_is_current(
    service: &Service,
    lastfm: &crate::lastfm::Service,
    spotify_membership: &crate::spotify_membership::SpotifyMembership,
) -> Result<Option<bool>, String> {
    let Some(session) = service.snapshot().await else {
        return Ok(Some(false));
    };
    let Some(expected) = session.spotify_account_id.as_deref() else {
        return Ok(Some(true));
    };
    let cached = spotify_membership.snapshot();
    match cached_spotify_identity_matches(expected, &cached) {
        Some(true)
            if review_phase_allowed(session.phase)
                && lastfm_username(lastfm).await.as_deref()
                    == Ok(session.lastfm_username.as_str()) =>
        {
            Ok(Some(true))
        }
        Some(_) => {
            service.suspend_for_account_mismatch().await?;
            Ok(Some(false))
        }
        None => Ok(None),
    }
}

pub(super) fn cached_spotify_identity_matches(
    expected: &str,
    library: &crate::store::SpotifyLibraryState,
) -> Option<bool> {
    library.is_exact().then_some(library.account_id == expected)
}

#[cfg(test)]
pub(super) async fn lazy_match_page_with_search<T, S, F, FFut>(
    service: &Service,
    lastfm: &crate::lastfm::Service,
    spotify_membership: &crate::spotify_membership::SpotifyMembership,
    provider: &impl Fn() -> Result<Arc<retune_spotify::client::SpotifyClient<T, S>>, String>,
    connection_state: &impl Fn() -> Result<bool, String>,
    key: &ReviewBatchKey,
    search: F,
) -> Result<Option<ImportPageView>, String>
where
    T: retune_spotify::client::Transport,
    S: retune_spotify::tokens::TokenStore,
    F: FnOnce(Vec<SourceRow>) -> FFut,
    FFut: Future<Output = Result<Vec<MatchResult>, String>>,
{
    lazy_match_page_with_search_source(
        service,
        lastfm,
        spotify_membership,
        provider,
        connection_state,
        key,
        |rows| async move {
            search(rows)
                .await
                .map(|matches| (matches, retune_spotify::client::SearchSource::Cache))
        },
    )
    .await
    .map(|(page, _)| page)
}

pub(super) async fn lazy_match_page_with_search_source<T, S, F, FFut>(
    service: &Service,
    lastfm: &crate::lastfm::Service,
    spotify_membership: &crate::spotify_membership::SpotifyMembership,
    provider: &impl Fn() -> Result<Arc<retune_spotify::client::SpotifyClient<T, S>>, String>,
    connection_state: &impl Fn() -> Result<bool, String>,
    key: &ReviewBatchKey,
    search: F,
) -> Result<(Option<ImportPageView>, retune_spotify::client::SearchSource), String>
where
    T: retune_spotify::client::Transport,
    S: retune_spotify::tokens::TokenStore,
    F: FnOnce(Vec<SourceRow>) -> FFut,
    FFut: Future<Output = Result<(Vec<MatchResult>, retune_spotify::client::SearchSource), String>>,
{
    let batch_id = key.batch_id;
    let artist = key.artist.as_str();
    let album = key.album.as_str();
    let Some(page) = service.page(batch_id, artist, album).await else {
        return Ok((None, retune_spotify::client::SearchSource::Cache));
    };
    let session = service
        .snapshot()
        .await
        .ok_or_else(|| "No Last.fm import session is active.".to_string())?;
    if batch_match_plan(&session, Some((batch_id, artist, album))).is_empty() {
        let membership_guard = spotify_membership.lock().await;
        current_matching_account(
            service,
            lastfm,
            &membership_guard,
            provider,
            connection_state,
            false,
        )
        .await?;
        return Ok((Some(page), retune_spotify::client::SearchSource::Cache));
    }

    // ponytail: one importer-wide lock; use per-batch locks only if throughput requires it.
    let _match_guard = service.lazy_match_lock.lock().await;
    let Some(page) = service.page(batch_id, artist, album).await else {
        return Ok((None, retune_spotify::client::SearchSource::Cache));
    };
    let session = service
        .snapshot()
        .await
        .ok_or_else(|| "No Last.fm import session is active.".to_string())?;
    if batch_match_plan(&session, Some((batch_id, artist, album))).is_empty() {
        let membership_guard = spotify_membership.lock().await;
        current_matching_account(
            service,
            lastfm,
            &membership_guard,
            provider,
            connection_state,
            false,
        )
        .await?;
        return Ok((Some(page), retune_spotify::client::SearchSource::Cache));
    }
    let initial_account = {
        let membership_guard = spotify_membership.lock().await;
        current_matching_account(
            service,
            lastfm,
            &membership_guard,
            provider,
            connection_state,
            false,
        )
        .await?
        .0
    };
    let batch = requested_batch(&session, batch_id, artist, album)
        .ok_or_else(|| "Unknown Last.fm import review batch.".to_string())?;
    let rows_by_id = source_row_map(&session);
    let mut rows = batch_rows(&batch, &rows_by_id)
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    if album.is_empty() {
        rows.retain(|row| {
            session
                .matches
                .get(&row.stable_id)
                .is_none_or(|result| is_album_search_term(&result.search_term))
        });
    }
    let row_tracks = rows
        .iter()
        .map(|row| (row.stable_id.clone(), row.track.clone()))
        .collect::<HashMap<_, _>>();
    let (results, source) = search(rows).await?;
    let results = if album.is_empty() {
        let current_session = service
            .snapshot()
            .await
            .ok_or_else(|| "No Last.fm import session is active.".to_string())?;
        results
            .into_iter()
            .map(|result| {
                let source_id = result.source_id.clone();
                preserve_match_selection(
                    result,
                    current_session.matches.get(&source_id),
                    &source_id,
                    row_tracks.get(&source_id).map(String::as_str).unwrap_or(""),
                )
            })
            .collect()
    } else {
        results
    };
    let membership_guard = spotify_membership.lock().await;
    let current_account = current_matching_account(
        service,
        lastfm,
        &membership_guard,
        provider,
        connection_state,
        false,
    )
    .await?
    .0;
    if current_account != initial_account {
        service.suspend_for_account_mismatch().await?;
        return Err(
            "The connected Spotify account changed while matching; the import is suspended for safety."
                .into(),
        );
    }
    let default_count_mode = service
        .mappings_for(
            &current_account.lastfm_username,
            Some(&current_account.spotify_account_id),
        )
        .await?
        .default_count_mode;
    service
        .set_matches(
            &current_account.lastfm_username,
            &current_account.spotify_account_id,
            batch_id,
            results,
            Some(default_count_mode),
        )
        .await?;
    Ok((service.page(batch_id, artist, album).await, source))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn lazy_seed_collection_page<T, S>(
    service: &Service,
    lastfm: &crate::lastfm::Service,
    spotify_membership: &crate::spotify_membership::SpotifyMembership,
    library: &crate::library_state::LibraryState,
    cooldown_store: &crate::store::FsCooldownStore,
    provider: &impl Fn() -> Result<Arc<retune_spotify::client::SpotifyClient<T, S>>, String>,
    connection_state: &impl Fn() -> Result<bool, String>,
    key: &ReviewBatchKey,
) -> Result<(Option<ImportPageView>, bool), String>
where
    T: retune_spotify::client::Transport,
    S: retune_spotify::tokens::TokenStore,
{
    let batch_id = key.batch_id;
    let artist = key.artist.as_str();
    let album = key.album.as_str();
    // ponytail: one importer-wide lock; use per-batch locks only if throughput requires it.
    let _match_guard = service.lazy_match_lock.lock().await;
    let session = service
        .snapshot()
        .await
        .ok_or_else(|| "No Last.fm import session is active.".to_string())?;
    let Some(rows) = collection_album_seed_rows(&session, batch_id, artist, album) else {
        let membership_guard = spotify_membership.lock().await;
        current_matching_account(
            service,
            lastfm,
            &membership_guard,
            provider,
            connection_state,
            false,
        )
        .await?;
        return Ok((service.page(batch_id, artist, album).await, false));
    };
    let (initial_account, resolved_provider) = {
        let membership_guard = spotify_membership.lock().await;
        current_matching_account(
            service,
            lastfm,
            &membership_guard,
            provider,
            connection_state,
            true,
        )
        .await?
    };
    let (candidates, selected_uri, source) = automatic_collection_album_seed_with_source(
        resolved_provider
            .as_ref()
            .expect("collection seeding requires a provider")
            .as_ref(),
        artist,
        album,
        &rows,
    )
    .await?;
    clear_search_quota(cooldown_store, source)?;
    let membership_guard = spotify_membership.lock().await;
    let current_account = current_matching_account(
        service,
        lastfm,
        &membership_guard,
        provider,
        connection_state,
        false,
    )
    .await?
    .0;
    if current_account != initial_account {
        service.suspend_for_account_mismatch().await?;
        return Err(
            "The connected Spotify account changed while matching; the import is suspended for safety."
                .into(),
        );
    }
    let membership = collection_membership_from(library, spotify_membership);
    let mappings = service
        .mappings_for(
            &current_account.lastfm_username,
            Some(&current_account.spotify_account_id),
        )
        .await?;
    service
        .seed_collection_albums(
            &current_account.lastfm_username,
            &current_account.spotify_account_id,
            batch_id,
            artist,
            candidates,
            selected_uri,
            &membership,
            &mappings,
        )
        .await?;
    Ok((
        service.page(batch_id, artist, album).await,
        source == retune_spotify::client::SearchSource::Network,
    ))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn lazy_match_page<T, S>(
    service: &Service,
    lastfm: &crate::lastfm::Service,
    spotify_membership: &crate::spotify_membership::SpotifyMembership,
    library: &crate::library_state::LibraryState,
    cooldown_store: &crate::store::FsCooldownStore,
    provider: &impl Fn() -> Result<Arc<retune_spotify::client::SpotifyClient<T, S>>, String>,
    connection_state: &impl Fn() -> Result<bool, String>,
    key: ReviewBatchKey,
) -> Result<(Option<ImportPageView>, bool, bool), String>
where
    T: retune_spotify::client::Transport,
    S: retune_spotify::tokens::TokenStore,
{
    let batch_id = key.batch_id;
    let artist = key.artist.as_str();
    let album = key.album.as_str();
    if service.page(batch_id, artist, album).await.is_none() {
        return Ok((None, false, false));
    }
    let initial_session = service
        .snapshot()
        .await
        .ok_or_else(|| "No Last.fm import session is active.".to_string())?;
    if collection_album_seed_rows(&initial_session, batch_id, artist, album).is_some() {
        let (page, network_search) = lazy_seed_collection_page(
            service,
            lastfm,
            spotify_membership,
            library,
            cooldown_store,
            provider,
            connection_state,
            &key,
        )
        .await?;
        let changed = page.is_some();
        return Ok((page, changed, network_search));
    }
    let initial_collection_shaped = batch_is_collection_shaped_for_id(&initial_session, batch_id);
    let initial_needs_match =
        !batch_match_plan(&initial_session, Some((batch_id, artist, album))).is_empty();
    if initial_collection_shaped
        && !initial_needs_match
        && initial_session.spotify_account_id.is_some()
    {
        if let Some(false) =
            cached_spotify_binding_is_current(service, lastfm, spotify_membership).await?
        {
            return Ok((None, false, false));
        }
        let session = service
            .snapshot()
            .await
            .ok_or_else(|| "No Last.fm import session is active.".to_string())?;
        let account_id = session.spotify_account_id.clone();
        let _membership_guard = spotify_membership.lock().await;
        let membership = collection_membership_from(library, spotify_membership);
        let mappings = service
            .mappings_for(&session.lastfm_username, account_id.as_deref())
            .await?;
        service
            .rerank_collection_batch(batch_id, &membership, &mappings)
            .await?;
        return Ok((service.page(batch_id, artist, album).await, false, false));
    }
    let session = service
        .snapshot()
        .await
        .ok_or_else(|| "No Last.fm import session is active.".to_string())?;
    if batch_match_plan(&session, Some((batch_id, artist, album))).is_empty() {
        if cached_spotify_binding_is_current(service, lastfm, spotify_membership).await?
            == Some(false)
        {
            return Ok((None, false, false));
        }
        return Ok((service.page(batch_id, artist, album).await, false, false));
    }
    let (page, source) = lazy_match_page_with_search_source(
        service,
        lastfm,
        spotify_membership,
        provider,
        connection_state,
        &key,
        |rows| async move {
            let provider = provider()?;
            match_batch_with_source(
                provider.as_ref(),
                artist,
                album,
                initial_collection_shaped,
                &rows,
            )
            .await
        },
    )
    .await?;
    clear_search_quota(cooldown_store, source)?;
    let changed = page.is_some();
    Ok((
        page,
        changed,
        source == retune_spotify::client::SearchSource::Network,
    ))
}

pub(super) fn matched_track_uri(result: &MatchResult, source_id: &str) -> Option<String> {
    result
        .track_matches
        .get(source_id)
        .filter(|uri| uri.starts_with("spotify:track:"))
        .cloned()
        .or_else(|| {
            result
                .selected_uri
                .as_ref()
                .filter(|uri| uri.starts_with("spotify:track:"))
                .cloned()
        })
}

pub(super) fn best_candidate(result: &MatchResult) -> Option<&AlbumCandidate> {
    result
        .candidates
        .iter()
        .filter(|candidate| {
            candidate.relation.is_some() || candidate.uri.starts_with("spotify:track:")
        })
        .min_by_key(|candidate| candidate_rank(candidate.relation))
}

pub(super) fn exact_album_match_for_rows(
    session: &LastFmImportSessionV2,
    batch_id: u32,
    rows: &[&SourceRow],
) -> bool {
    let rows = rows
        .iter()
        .copied()
        .filter(|row| {
            let decision = default_decision(session, &row.stable_id);
            is_actionable(session, &row.stable_id) && !decision.excluded
        })
        .collect::<Vec<_>>();
    let Some(first) = rows.first() else {
        return false;
    };
    if session.collection_album_matches.contains_key(&batch_id) {
        let state = &session.collection_album_matches[&batch_id];
        if state.selected_album_uris.iter().any(|uri| {
            !state
                .cached_candidates
                .iter()
                .any(|candidate| candidate.matching.uri == *uri)
        }) {
            return false;
        }
        let selected = collection_selected_albums(session, batch_id);
        let Some(album) = selected.first().copied() else {
            return false;
        };
        if selected.len() != 1 || album.matching.track_uris.len() != rows.len() {
            return false;
        }
        let mut targets = BTreeSet::new();
        return rows.iter().all(|row| {
            let candidates = (0..album.matching.track_uris.len())
                .map(|index| AlbumCandidate {
                    uri: album.matching.track_uris[index].clone(),
                    name: album
                        .matching
                        .track_names
                        .get(index)
                        .cloned()
                        .unwrap_or_default(),
                    artist: album
                        .matching
                        .track_artists
                        .get(index)
                        .cloned()
                        .unwrap_or_else(|| album.matching.artist.clone()),
                    in_library: false,
                    track_uris: vec![album.matching.track_uris[index].clone()],
                    track_names: vec![album
                        .matching
                        .track_names
                        .get(index)
                        .cloned()
                        .unwrap_or_default()],
                    track_artists: vec![album
                        .matching
                        .track_artists
                        .get(index)
                        .cloned()
                        .unwrap_or_else(|| album.matching.artist.clone())],
                    track_albums: vec![album.matching.name.clone()],
                    relation: None,
                })
                .collect::<Vec<_>>();
            let supported = collection_best_title_matches(row, &candidates).len();
            let Some(result) = session.matches.get(&row.stable_id) else {
                return false;
            };
            let Some(target) = matched_track_uri(result, &row.stable_id) else {
                return false;
            };
            supported == 1 && album.matching.track_uris.contains(&target) && targets.insert(target)
        });
    }
    let Some(first_match) = session.matches.get(&first.stable_id) else {
        return false;
    };
    let Some(candidate) = first_match
        .selected_uri
        .as_deref()
        .filter(|uri| uri.starts_with("spotify:album:"))
        .and_then(|uri| {
            first_match
                .candidates
                .iter()
                .find(|candidate| candidate.uri == uri)
        })
        .or_else(|| best_candidate(first_match))
        .filter(|candidate| candidate.uri.starts_with("spotify:album:"))
    else {
        return false;
    };
    if candidate.relation != Some(AlbumRelation::BestMatch)
        || candidate.track_uris.len() != rows.len()
    {
        return false;
    }
    let mut targets = BTreeSet::new();
    rows.into_iter().all(|row| {
        let Some(result) = session.matches.get(&row.stable_id) else {
            return false;
        };
        let selected_album = result
            .selected_uri
            .as_deref()
            .filter(|uri| uri.starts_with("spotify:album:"))
            .or_else(|| best_candidate(result).map(|candidate| candidate.uri.as_str()));
        let Some(target) = matched_track_uri_for_row(result, row, false) else {
            return false;
        };
        selected_album == Some(candidate.uri.as_str())
            && candidate.track_uris.contains(&target)
            && targets.insert(target)
    })
}

pub(super) fn matched_track_uri_for_row(
    result: &MatchResult,
    row: &SourceRow,
    converted_collection: bool,
) -> Option<String> {
    if row.album.is_empty() || converted_collection {
        return matched_track_uri(result, &row.stable_id);
    }
    matched_track_uri(result, &row.stable_id).or_else(|| {
        let candidate = best_candidate(result)?;
        if candidate.uri.starts_with("spotify:track:") {
            return Some(candidate.uri.clone());
        }
        let index = release_track_match_index(row, &candidate.track_names)?;
        candidate.track_uris.get(index).cloned()
    })
}

pub(super) fn row_needs_match(row: &SourceRow, result: Option<&MatchResult>) -> bool {
    result.is_none()
        || (row.album.is_empty()
            && result.is_some_and(|result| is_album_search_term(&result.search_term)))
}

pub(super) fn collection_album_seed_rows(
    session: &LastFmImportSessionV2,
    batch_id: u32,
    artist: &str,
    album: &str,
) -> Option<Vec<SourceRow>> {
    if album.is_empty() || session.collection_album_matches.contains_key(&batch_id) {
        return None;
    }
    let batch = requested_batch(session, batch_id, artist, album)?;
    let rows = batch_rows(&batch, &source_row_map(session));
    let projection = batch_projection(&batch, &rows);
    if !projection.collection_shaped
        || !rows
            .iter()
            .any(|row| is_actionable(session, &row.stable_id))
    {
        return None;
    }
    let seed = rows
        .into_iter()
        .filter(|row| row.artist == artist && row.album == album)
        .cloned()
        .collect::<Vec<_>>();
    (!seed.is_empty()).then_some(seed)
}

pub(super) fn batch_match_plan(
    session: &LastFmImportSessionV2,
    requested: Option<(u32, &str, &str)>,
) -> Vec<(u32, String, String)> {
    let rows_by_id = source_row_map(session);
    review_batches(session)
        .into_iter()
        .filter_map(|batch| {
            let rows = batch_rows(&batch, &rows_by_id);
            rows.first()?;
            let projection = batch_projection(&batch, &rows);
            if batch_is_collection_shaped(session, &batch, &rows) {
                return None;
            }
            let selected = requested.is_some_and(|(requested_page, artist, album)| {
                requested_page == batch.page
                    && artist == projection.representative_artist
                    && album == projection.representative_album
            });
            let remaining = requested.is_none()
                && rows
                    .iter()
                    .any(|row| is_actionable(session, &row.stable_id));
            if (selected || remaining)
                && rows
                    .iter()
                    .any(|row| row_needs_match(row, session.matches.get(&row.stable_id)))
            {
                Some((
                    batch.page,
                    projection.representative_artist,
                    projection.representative_album,
                ))
            } else {
                None
            }
        })
        .collect()
}

pub(super) fn accept_all_entity_uris(
    session: &LastFmImportSessionV2,
) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut album_uris = BTreeSet::new();
    let mut track_uris = BTreeSet::new();
    let rows_by_id = source_row_map(session);
    for batch in review_batches(session) {
        let rows = batch_rows(&batch, &rows_by_id);
        if rows.is_empty() {
            continue;
        }
        let projection = batch_projection(&batch, &rows);
        let collection_shaped = batch_is_collection_shaped(session, &batch, &rows);
        let options = session.options_for_batch(
            batch.page,
            &projection.representative_artist,
            &projection.representative_album,
        );
        let selected = rows
            .iter()
            .filter(|row| {
                options.selected_track_ids.contains(&row.stable_id)
                    && is_actionable(session, &row.stable_id)
            })
            .collect::<Vec<_>>();
        if !options.import_content {
            continue;
        }
        if collection_shaped {
            let full_albums = collection_full_albums(session, batch.page);
            let covered_tracks = full_albums
                .iter()
                .flat_map(|album| album.matching.track_uris.iter())
                .collect::<BTreeSet<_>>();
            album_uris.extend(full_albums.iter().map(|album| album.matching.uri.clone()));
            for row in selected {
                if let Some(uri) = session
                    .matches
                    .get(&row.stable_id)
                    .and_then(|result| matched_track_uri_for_row(result, row, true))
                    .filter(|uri| !covered_tracks.contains(uri))
                {
                    track_uris.insert(uri);
                }
            }
            continue;
        }
        if options.whole_album {
            for row in selected {
                if let Some(uri) = session
                    .matches
                    .get(&row.stable_id)
                    .and_then(|result| {
                        result.selected_uri.as_deref().or_else(|| {
                            best_candidate(result).map(|candidate| candidate.uri.as_str())
                        })
                    })
                    .filter(|uri| uri.starts_with("spotify:album:"))
                {
                    album_uris.insert(uri.to_owned());
                }
            }
        } else {
            for row in selected {
                if let Some(uri) = session
                    .matches
                    .get(&row.stable_id)
                    .and_then(|result| matched_track_uri_for_row(result, row, collection_shaped))
                {
                    track_uris.insert(uri);
                }
            }
        }
    }
    (album_uris, track_uris)
}

pub(super) fn membership_uris_for_import(
    import_content: bool,
    whole_album: bool,
    album_uri: Option<&str>,
    track_uris: &[String],
) -> Option<Vec<String>> {
    if !import_content {
        return None;
    }
    if whole_album {
        return album_uri
            .filter(|uri| uri.starts_with("spotify:album:"))
            .map(|uri| vec![uri.to_owned()]);
    }
    let mut seen = BTreeSet::new();
    Some(
        track_uris
            .iter()
            .filter(|uri| uri.starts_with("spotify:track:") && seen.insert((*uri).clone()))
            .cloned()
            .collect(),
    )
}

pub(super) fn historical_counts_for_targets(
    session: &LastFmImportSessionV2,
    current_by_target: &BTreeMap<String, Vec<&SourceRow>>,
) -> BTreeMap<String, u64> {
    let current_ids = current_by_target
        .values()
        .flat_map(|rows| rows.iter().map(|row| row.stable_id.as_str()))
        .collect::<BTreeSet<_>>();
    let source_batches = source_batch_map(session);
    let mut relevant = BTreeMap::<String, Vec<&SourceRow>>::new();
    for row in &session.rows {
        let decision = default_decision(session, &row.stable_id);
        let included = current_ids.contains(row.stable_id.as_str())
            || (decision.status == RowStatus::Done
                && !decision.excluded
                && source_batches
                    .get(row.stable_id.as_str())
                    .and_then(|batch_id| session.page_options.get(&batch_options_key(*batch_id)))
                    .or_else(|| {
                        session
                            .page_options
                            .get(&format!("{}\u{1f}{}", row.artist, row.album))
                    })
                    .is_some_and(|options| {
                        options.include_historical_play_counts
                            && options.selected_track_ids.contains(&row.stable_id)
                    }));
        if !included {
            continue;
        }
        let Some(target) = session.matches.get(&row.stable_id).and_then(|result| {
            matched_track_uri_for_row(
                result,
                row,
                source_batches
                    .get(&row.stable_id)
                    .is_some_and(|batch_id| batch_is_collection_shaped_for_id(session, *batch_id)),
            )
        }) else {
            continue;
        };
        if current_by_target.contains_key(&target) {
            relevant.entry(target).or_default().push(row);
        }
    }
    current_by_target
        .keys()
        .map(|target| {
            let count = resolved_play_count(
                relevant.get(target).map(Vec::as_slice).unwrap_or_default(),
                session
                    .count_modes
                    .get(target)
                    .copied()
                    .unwrap_or(session.default_count_mode),
            );
            (target.clone(), count)
        })
        .collect()
}

#[cfg(test)]
pub(super) fn historical_count_for_target(
    session: &LastFmImportSessionV2,
    target_uri: &str,
    current_rows: &[&SourceRow],
) -> u64 {
    historical_counts_for_targets(
        session,
        &BTreeMap::from([(target_uri.to_owned(), current_rows.to_vec())]),
    )[target_uri]
}

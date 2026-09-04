use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::matching::{
    automatic_album_candidate_for_rows, classify_album_candidates_for_rows,
    collection_album_candidate_from_release, collection_best_title_matches,
    collection_candidate_matches_title, collection_track_candidates, ratify_collection_result,
    ratify_collection_result_with_selected_albums_and_injected, release_track_match_index,
    remove_injected_collection_candidates, selected_match_confidence, track_search_term,
    update_selected_release_match, without_known_source_suffix, CollectionMembership,
};
use super::model::{
    AlbumCandidate, ApplyJobStatus, CollectionAlbumCandidate, CollectionAlbumCoverage,
    CollectionAlbumMatchState, CollectionAlbumPreviewCoverage, CollectionCoverage,
    CollectionMatchView, CollectionTrackMatchStatus, CollectionTrackStatus, ImportBatch,
    ImportPhase, ImportQueueItem, LastFmImportSessionV2, LastFmMappings, MatchResult, PageOptions,
    QueueStatus, ReviewApplyJob, RowDecision, RowStatus, SourceRow,
};
use super::{
    batch_is_collection_shaped, batch_options_key, batch_projection, batch_rows, best_candidate,
    derived_batch_projection, exact_album_match_for_rows, is_converted_collection_batch,
    matched_track_uri, matched_track_uri_for_row, normalize_catalog_text,
    reconciliation::source_album_key, requested_batch, review_batches, review_batches_for_read,
    source_row_map, LASTFM_REVIEW_BATCH_SIZE,
};

pub(crate) fn default_decision(session: &LastFmImportSessionV2, id: &str) -> RowDecision {
    session.decisions.get(id).cloned().unwrap_or_default()
}

pub(super) fn combine_review_batches(
    session: &mut LastFmImportSessionV2,
    batch_ids: &[u32],
) -> Result<(u32, String, String), String> {
    let requested = batch_ids.iter().copied().collect::<BTreeSet<_>>();
    if requested.len() < 2 || requested.len() != batch_ids.len() {
        return Err("Choose at least two distinct Last.fm batches to combine.".into());
    }
    let batches = review_batches(session);
    let selected = batches
        .iter()
        .filter(|batch| requested.contains(&batch.page))
        .cloned()
        .collect::<Vec<_>>();
    if selected.len() != requested.len() {
        return Err("A selected Last.fm batch is no longer available.".into());
    }
    let rows_by_id = source_row_map(session);
    let rows = selected
        .iter()
        .flat_map(|batch| batch_rows(batch, &rows_by_id))
        .collect::<Vec<_>>();
    let source_ids = rows
        .iter()
        .map(|row| row.stable_id.clone())
        .collect::<BTreeSet<_>>();
    if source_ids.len()
        != selected
            .iter()
            .map(|batch| batch.source_ids.len())
            .sum::<usize>()
    {
        return Err(
            "A selected Last.fm batch contains unavailable or duplicate source rows.".into(),
        );
    }
    if source_ids.len() > LASTFM_REVIEW_BATCH_SIZE {
        return Err(format!(
            "A custom Last.fm batch can contain at most {LASTFM_REVIEW_BATCH_SIZE} source tracks."
        ));
    }
    let projections = selected
        .iter()
        .map(|batch| batch_projection(batch, &batch_rows(batch, &rows_by_id)))
        .collect::<Vec<_>>();
    let artist_keys = rows
        .iter()
        .map(|row| normalize_catalog_text(&row.artist))
        .collect::<BTreeSet<_>>();
    let combined_projection = derived_batch_projection(&rows);
    let representative_artist = if artist_keys.len() == 1 {
        combined_projection.representative_artist
    } else {
        "Various Artists".into()
    };
    let target_page = *requested.first().expect("two requested batches");
    let options = selected
        .iter()
        .zip(&projections)
        .map(|(batch, projection)| {
            session.options_for_page_batch(
                batch,
                &projection.representative_artist,
                &projection.representative_album,
                &batch_rows(batch, &rows_by_id),
            )
        })
        .collect::<Vec<_>>();
    let first_options = &options[0];
    if options.iter().skip(1).any(|option| {
        option.import_content != first_options.import_content
            || option.include_historical_play_counts != first_options.include_historical_play_counts
            || option.genre != first_options.genre
            || option.rating != first_options.rating
    }) {
        return Err(
            "Selected batches have different import options; make them match before combining."
                .into(),
        );
    }
    let mut combined_options = first_options.clone();
    combined_options.whole_album = false;
    combined_options.selected_track_ids = options
        .iter()
        .flat_map(|option| option.selected_track_ids.iter().cloned())
        .collect();
    let album_labels = combined_projection.album_labels;
    let mut combined_collection = CollectionAlbumMatchState {
        automatic_selection_disabled: true,
        ..CollectionAlbumMatchState::default()
    };
    for batch_id in &requested {
        let Some(state) = session.collection_album_matches.get(batch_id) else {
            continue;
        };
        for candidate in &state.cached_candidates {
            if !combined_collection
                .cached_candidates
                .iter()
                .any(|existing| existing.matching.uri == candidate.matching.uri)
            {
                combined_collection
                    .cached_candidates
                    .push(candidate.clone());
            }
        }
        for uri in &state.selected_album_uris {
            if !combined_collection.selected_album_uris.contains(uri) {
                combined_collection.selected_album_uris.push(uri.clone());
            }
        }
        for (uri, enabled) in &state.full_album_choices {
            combined_collection
                .full_album_choices
                .entry(uri.clone())
                .and_modify(|current| *current |= enabled)
                .or_insert(*enabled);
        }
        for (source_id, uris) in &state.injected_candidate_uris {
            combined_collection
                .injected_candidate_uris
                .entry(source_id.clone())
                .or_default()
                .extend(uris.iter().cloned());
        }
    }
    let ordered_source_ids = session
        .rows
        .iter()
        .filter(|row| source_ids.contains(&row.stable_id))
        .map(|row| row.stable_id.clone())
        .collect();
    session.batches = batches
        .into_iter()
        .filter(|batch| !requested.contains(&batch.page))
        .collect();
    session.batches.push(ImportBatch {
        page: target_page,
        source_ids: ordered_source_ids,
        custom: true,
        collection_shaped: Some(true),
        representative_artist: Some(representative_artist.clone()),
        representative_album: Some(String::new()),
        album_labels,
    });
    session.batches.sort_by_key(|batch| batch.page);
    for batch_id in &requested {
        session.page_options.remove(&batch_options_key(*batch_id));
        session.collection_album_matches.remove(batch_id);
    }
    session
        .page_options
        .insert(batch_options_key(target_page), combined_options);
    session
        .collection_album_matches
        .insert(target_page, combined_collection);
    Ok((target_page, representative_artist, String::new()))
}

pub(super) fn refresh_cached_album_matches(session: &mut LastFmImportSessionV2) -> bool {
    let row_indices = session
        .rows
        .iter()
        .enumerate()
        .map(|(index, row)| (row.stable_id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut changed = false;
    for batch in &session.batches {
        if !batch
            .source_ids
            .iter()
            .all(|id| session.matches.contains_key(id))
        {
            continue;
        }
        let selected_albums = batch
            .source_ids
            .iter()
            .filter_map(|id| session.matches[id].selected_uri.as_deref())
            .filter(|uri| uri.starts_with("spotify:album:"))
            .collect::<BTreeSet<_>>();
        let has_track_selection = batch.source_ids.iter().any(|id| {
            session.matches[id]
                .selected_uri
                .as_deref()
                .is_some_and(|uri| !uri.starts_with("spotify:album:"))
        });
        let can_auto_select = selected_albums.is_empty()
            && batch
                .source_ids
                .iter()
                .all(|id| session.matches[id].track_matches.is_empty());
        if has_track_selection
            || selected_albums.len() > 1
            || (!can_auto_select && selected_albums.is_empty())
        {
            continue;
        }
        let rows = batch
            .source_ids
            .iter()
            .filter_map(|id| {
                row_indices
                    .get(id.as_str())
                    .map(|index| &session.rows[*index])
            })
            .collect::<Vec<_>>();
        let Some(first) = rows.first() else {
            continue;
        };
        if first.album.is_empty() || batch_is_collection_shaped(session, batch, &rows) {
            continue;
        }
        let Some(mut candidates) = batch
            .source_ids
            .iter()
            .find_map(|id| session.matches.get(id))
            .map(|result| result.candidates.clone())
        else {
            continue;
        };
        let source_rows = rows.iter().map(|row| (*row).clone()).collect::<Vec<_>>();
        classify_album_candidates_for_rows(&source_rows, &mut candidates);
        let Some(selected_uri) = selected_albums
            .first()
            .map(|uri| (*uri).to_owned())
            .or_else(|| {
                automatic_album_candidate_for_rows(&first.album, &source_rows, &candidates)
                    .map(|candidate| candidate.uri.clone())
            })
        else {
            continue;
        };
        let relations = candidates
            .iter()
            .map(|candidate| (candidate.uri.as_str(), candidate.relation))
            .collect::<HashMap<_, _>>();
        for row in rows {
            let Some(result) = session.matches.get_mut(&row.stable_id) else {
                continue;
            };
            let previous = result.clone();
            for candidate in &mut result.candidates {
                if let Some(relation) = relations.get(candidate.uri.as_str()) {
                    candidate.relation = *relation;
                }
            }
            let Some(candidate) = result
                .candidates
                .iter()
                .find(|candidate| candidate.uri == selected_uri)
                .cloned()
            else {
                continue;
            };
            if result.selected_uri.is_none() {
                update_selected_release_match(result, row, &candidate);
            } else {
                let source_track = without_known_source_suffix(&row.track, &row.artist, &row.album);
                result.confidence = Some(selected_match_confidence(&source_track, &candidate));
                if !result.track_matches.contains_key(&row.stable_id) {
                    if let Some(index) = release_track_match_index(row, &candidate.track_names) {
                        if let Some(uri) = candidate.track_uris.get(index) {
                            result
                                .track_matches
                                .insert(row.stable_id.clone(), uri.clone());
                        }
                    }
                }
            }
            changed |= previous != *result;
        }
    }
    changed
}

pub(super) fn collection_selected_albums(
    session: &LastFmImportSessionV2,
    batch_id: u32,
) -> Vec<&CollectionAlbumCandidate> {
    let Some(state) = session.collection_album_matches.get(&batch_id) else {
        return Vec::new();
    };
    let mut seen = BTreeSet::new();
    state
        .selected_album_uris
        .iter()
        .filter(|uri| seen.insert((*uri).clone()))
        .filter_map(|uri| {
            state
                .cached_candidates
                .iter()
                .find(|candidate| candidate.matching.uri == *uri)
        })
        .collect()
}

pub(super) fn collection_full_albums(
    session: &LastFmImportSessionV2,
    batch_id: u32,
) -> Vec<&CollectionAlbumCandidate> {
    let Some(state) = session.collection_album_matches.get(&batch_id) else {
        return Vec::new();
    };
    collection_selected_albums(session, batch_id)
        .into_iter()
        .filter(|album| {
            state
                .full_album_choices
                .get(&album.matching.uri)
                .copied()
                .unwrap_or(album.matching.in_library)
        })
        .collect()
}

pub(super) fn selected_collection_album(
    session: &LastFmImportSessionV2,
    batch_id: u32,
) -> Option<&CollectionAlbumCandidate> {
    let selected = collection_selected_albums(session, batch_id);
    (selected.len() == 1).then(|| selected[0])
}

pub(super) fn selected_collection_album_for_rows(
    session: &LastFmImportSessionV2,
    batch_id: u32,
    rows: &[&SourceRow],
) -> Option<CollectionAlbumCandidate> {
    if let Some(candidate) = selected_collection_album(session, batch_id) {
        return Some(candidate.clone());
    }
    let selected_uri = rows.iter().find_map(|row| {
        session
            .matches
            .get(&row.stable_id)
            .and_then(|result| result.selected_uri.as_deref())
            .filter(|uri| uri.starts_with("spotify:album:"))
    })?;
    let candidate = rows.iter().find_map(|row| {
        session.matches.get(&row.stable_id).and_then(|result| {
            result
                .candidates
                .iter()
                .find(|candidate| candidate.uri == selected_uri)
        })
    })?;
    Some(collection_album_candidate_from_release(candidate.clone()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CollectionRowStatus {
    Matched,
    Ambiguous,
    Unresolved,
}

pub(super) fn collection_row_status(
    row: &SourceRow,
    result: Option<&MatchResult>,
    selected_tracks: &[AlbumCandidate],
) -> CollectionRowStatus {
    if result
        .and_then(|result| matched_track_uri(result, &row.stable_id))
        .is_some()
    {
        return CollectionRowStatus::Matched;
    }
    let supported = collection_best_title_matches(row, selected_tracks)
        .into_iter()
        .map(|candidate| candidate.uri.as_str())
        .collect::<BTreeSet<_>>();
    if supported.len() > 1 {
        CollectionRowStatus::Ambiguous
    } else if supported.len() == 1 {
        CollectionRowStatus::Matched
    } else if result
        .is_some_and(|result| collection_best_title_matches(row, &result.candidates).len() > 1)
    {
        CollectionRowStatus::Ambiguous
    } else {
        CollectionRowStatus::Unresolved
    }
}

pub(super) fn collection_track_statuses(
    candidate: &CollectionAlbumCandidate,
    eligible: &[&SourceRow],
    selected_tracks: &[AlbumCandidate],
    session: &LastFmImportSessionV2,
) -> Vec<CollectionTrackStatus> {
    candidate
        .matching
        .track_uris
        .iter()
        .map(|uri| {
            let mut matched = false;
            let mut ambiguous = false;
            let mut unique = false;
            for row in eligible {
                if let Some(target) = session
                    .matches
                    .get(&row.stable_id)
                    .and_then(|result| matched_track_uri(result, &row.stable_id))
                {
                    matched |= target == *uri;
                    continue;
                }
                let exact = selected_tracks
                    .iter()
                    .filter(|track| collection_candidate_matches_title(row, track))
                    .map(|track| track.uri.as_str())
                    .collect::<BTreeSet<_>>();
                if exact.contains(uri.as_str()) {
                    ambiguous |= exact.len() > 1;
                    unique |= exact.len() == 1;
                }
            }
            CollectionTrackStatus {
                uri: uri.clone(),
                status: if matched {
                    CollectionTrackMatchStatus::Matched
                } else if ambiguous {
                    CollectionTrackMatchStatus::Ambiguous
                } else if unique {
                    CollectionTrackMatchStatus::Matched
                } else {
                    CollectionTrackMatchStatus::Unmatched
                },
            }
        })
        .collect()
}

pub(super) fn collection_album_preview_coverage(
    candidate: &CollectionAlbumCandidate,
    eligible: &[&SourceRow],
    selected_tracks: &[AlbumCandidate],
    session: &LastFmImportSessionV2,
) -> (usize, usize, usize, usize, Vec<CollectionTrackStatus>) {
    let candidate_tracks =
        collection_track_candidates(&[candidate], &CollectionMembership::default());
    let candidate_uris = candidate_tracks
        .iter()
        .map(|track| track.uri.clone())
        .collect::<BTreeSet<_>>();
    let mut union = selected_tracks.to_vec();
    let existing = union
        .iter()
        .map(|track| track.uri.clone())
        .collect::<BTreeSet<_>>();
    union.extend(
        candidate_tracks
            .into_iter()
            .filter(|track| !existing.contains(&track.uri)),
    );
    let matched = eligible
        .iter()
        .filter(|row| {
            let candidate_target = session
                .matches
                .get(&row.stable_id)
                .and_then(|result| matched_track_uri(result, &row.stable_id));
            if let Some(target) = candidate_target {
                return candidate_uris.contains(&target);
            }
            let exact = union
                .iter()
                .filter(|track| collection_candidate_matches_title(row, track))
                .map(|track| track.uri.as_str())
                .collect::<BTreeSet<_>>();
            exact.len() == 1
                && exact
                    .iter()
                    .next()
                    .is_some_and(|uri| candidate_uris.contains(*uri))
        })
        .count();
    let (projected_matched, ambiguous, _) = count_collection_statuses(
        eligible
            .iter()
            .map(|row| collection_row_status(row, session.matches.get(&row.stable_id), &union)),
    );
    let unique_coverage = eligible
        .iter()
        .filter(|row| {
            let exact = union
                .iter()
                .filter(|track| collection_candidate_matches_title(row, track))
                .map(|track| track.uri.as_str())
                .collect::<BTreeSet<_>>();
            exact.len() == 1
                && exact
                    .iter()
                    .next()
                    .is_some_and(|uri| candidate_uris.contains(*uri))
        })
        .count();
    let statuses = collection_track_statuses(candidate, eligible, &union, session);
    (
        matched,
        projected_matched,
        ambiguous,
        unique_coverage,
        statuses,
    )
}

pub(super) fn count_collection_statuses(
    statuses: impl IntoIterator<Item = CollectionRowStatus>,
) -> (usize, usize, usize) {
    statuses.into_iter().fold(
        (0, 0, 0),
        |(matched, ambiguous, unresolved), status| match status {
            CollectionRowStatus::Matched => (matched + 1, ambiguous, unresolved),
            CollectionRowStatus::Ambiguous => (matched, ambiguous + 1, unresolved),
            CollectionRowStatus::Unresolved => (matched, ambiguous, unresolved + 1),
        },
    )
}

pub(super) fn collection_match_view(
    session: &LastFmImportSessionV2,
    batch_id: u32,
    rows: &[&SourceRow],
) -> CollectionMatchView {
    let state = session
        .collection_album_matches
        .get(&batch_id)
        .cloned()
        .unwrap_or_default();
    let selected = collection_selected_albums(session, batch_id);
    let full_album_uris = collection_full_albums(session, batch_id)
        .into_iter()
        .map(|album| album.matching.uri.clone())
        .collect();
    let membership = CollectionMembership::default();
    let selected_tracks = collection_track_candidates(&selected, &membership);
    let eligible = rows
        .iter()
        .filter(|row| !default_decision(session, &row.stable_id).excluded)
        .copied()
        .collect::<Vec<_>>();
    let base_statuses = eligible.iter().map(|row| {
        collection_row_status(row, session.matches.get(&row.stable_id), &selected_tracks)
    });
    let (matched, ambiguous, unresolved) = count_collection_statuses(base_statuses);
    let selected_albums = selected
        .iter()
        .map(|album| {
            let (matched, _, _, unique_coverage, _) =
                collection_album_preview_coverage(album, &eligible, &selected_tracks, session);
            CollectionAlbumCoverage {
                uri: album.matching.uri.clone(),
                matched,
                unique_coverage,
            }
        })
        .collect::<Vec<_>>();
    let previews = state
        .cached_candidates
        .iter()
        .map(|candidate| {
            let is_selected = state
                .selected_album_uris
                .iter()
                .any(|uri| uri == &candidate.matching.uri);
            let before = eligible.iter().map(|row| {
                collection_row_status(row, session.matches.get(&row.stable_id), &selected_tracks)
            });
            let (before_matched, before_ambiguous, _) = count_collection_statuses(before);
            let (matched, after_matched, after_ambiguous, unique_coverage, track_statuses) =
                collection_album_preview_coverage(candidate, &eligible, &selected_tracks, session);
            CollectionAlbumPreviewCoverage {
                uri: candidate.matching.uri.clone(),
                selected: is_selected,
                matched,
                unique_coverage,
                marginal_matches: if is_selected {
                    0
                } else {
                    after_matched as i32 - before_matched as i32
                },
                ambiguity_changes: if is_selected {
                    0
                } else {
                    after_ambiguous as i32 - before_ambiguous as i32
                },
                track_statuses,
            }
        })
        .collect();
    CollectionMatchView {
        cached_albums: state.cached_candidates,
        selected_album_uris: state.selected_album_uris,
        full_album_uris,
        coverage: CollectionCoverage {
            matched,
            ambiguous,
            unresolved,
            selected_albums,
            previews,
        },
        whole_album_ready: exact_album_match_for_rows(session, batch_id, &eligible),
    }
}

pub(super) fn selected_release_candidate_for_activation(
    session: &LastFmImportSessionV2,
    batch: &ImportBatch,
    rows: &[SourceRow],
) -> Result<AlbumCandidate, String> {
    let mut selected_uri = None;
    for row in rows {
        let Some(result) = session.matches.get(&row.stable_id) else {
            continue;
        };
        let Some(uri) = result
            .selected_uri
            .as_deref()
            .filter(|uri| uri.starts_with("spotify:album:"))
        else {
            continue;
        };
        if selected_uri
            .as_deref()
            .is_some_and(|selected| selected != uri)
        {
            return Err("Choose one Spotify release before adding album matches.".into());
        }
        selected_uri = Some(uri.to_owned());
    }
    let selected_uri = selected_uri
        .ok_or_else(|| "Choose a Spotify release before adding album matches.".to_string())?;
    let candidate = batch
        .source_ids
        .iter()
        .filter_map(|id| session.matches.get(id))
        .find_map(|result| {
            result
                .candidates
                .iter()
                .find(|candidate| candidate.uri == selected_uri)
                .cloned()
        })
        .ok_or_else(|| "The selected Spotify release is no longer cached.".to_string())?;
    if !candidate.uri.starts_with("spotify:album:") || candidate.track_uris.is_empty() {
        return Err("The selected Spotify release has no cached tracks.".into());
    }
    Ok(candidate)
}

pub(super) fn activate_collection_session(
    session: &mut LastFmImportSessionV2,
    batch_id: u32,
    artist: &str,
    album: &str,
    membership: &CollectionMembership,
    mappings: &LastFmMappings,
) -> Result<String, String> {
    let Some(batch) = requested_batch(session, batch_id, artist, album) else {
        return Err("Unknown Last.fm import review batch.".into());
    };
    let rows = batch_rows(&batch, &source_row_map(session))
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let collection_shaped =
        batch_is_collection_shaped(session, &batch, &rows.iter().collect::<Vec<_>>());
    if !collection_shaped && album.is_empty() {
        return Err("Multi-album matching is available only for release batches.".into());
    }
    let was_converted = session.collection_album_matches.contains_key(&batch_id);
    let release = (!was_converted && !collection_shaped)
        .then(|| selected_release_candidate_for_activation(session, &batch, &rows))
        .transpose()?;
    if let Some(release) = release.as_ref() {
        let state = session
            .collection_album_matches
            .entry(batch_id)
            .or_default();
        let candidate = collection_album_candidate_from_release(release.clone());
        if !state
            .cached_candidates
            .iter()
            .any(|existing| existing.matching.uri == candidate.matching.uri)
        {
            state.cached_candidates.push(candidate);
        }
        if !state
            .selected_album_uris
            .iter()
            .any(|uri| uri == &release.uri)
        {
            state.selected_album_uris.push(release.uri.clone());
        }
    }

    if let Some(options) = session.page_options.get_mut(&batch_options_key(batch_id)) {
        options.whole_album = false;
    }
    if let Some(options) = session
        .page_options
        .get_mut(&format!("{artist}\u{1f}{album}"))
    {
        options.whole_album = false;
    }

    if let Some(release) = release.as_ref() {
        let release_uri = release.uri.clone();
        for row in &rows {
            let Some(result) = session.matches.get_mut(&row.stable_id) else {
                continue;
            };
            if result.selected_uri.as_deref() != Some(release_uri.as_str()) {
                continue;
            }
            let expected = release_track_match_index(row, &release.track_names)
                .and_then(|index| release.track_uris.get(index))
                .cloned();
            let explicit = result
                .track_matches
                .get(&row.stable_id)
                .filter(|uri| expected.as_deref() != Some(uri.as_str()))
                .cloned();
            result.selected_uri = explicit;
        }
    }
    // Release-shaped batches become collection sessions as soon as this action
    // succeeds.  Remove the release candidates even on the first conversion;
    // otherwise the old album target remains selectable beside the injected
    // collection tracks.
    if !collection_shaped {
        for row in &rows {
            if let Some(result) = session.matches.get_mut(&row.stable_id) {
                result
                    .candidates
                    .retain(|candidate| !candidate.uri.starts_with("spotify:album:"));
            }
        }
    }
    rerank_collection_session(session, batch_id, membership, mappings)?;
    Ok(album.to_owned())
}

pub(super) fn rerank_collection_session(
    session: &mut LastFmImportSessionV2,
    batch_id: u32,
    membership: &CollectionMembership,
    mappings: &LastFmMappings,
) -> Result<(), String> {
    let rows_by_id = source_row_map(session);
    let Some(batch) = review_batches(session)
        .into_iter()
        .find(|batch| batch.page == batch_id)
    else {
        return Err("Unknown Last.fm import review batch.".into());
    };
    let rows = batch_rows(&batch, &rows_by_id)
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let row_refs = rows.iter().collect::<Vec<_>>();
    let projection = batch_projection(&batch, &row_refs);
    let collection_shaped = batch_is_collection_shaped(session, &batch, &row_refs);
    if !collection_shaped && !session.collection_album_matches.contains_key(&batch_id) {
        return Err("Album matching is available only for collection batches.".into());
    }
    let previous_injected = session
        .collection_album_matches
        .get(&batch_id)
        .map(|state| state.injected_candidate_uris.clone())
        .unwrap_or_default();
    if let Some(state) = session.collection_album_matches.get_mut(&batch_id) {
        for candidate in &mut state.cached_candidates {
            candidate.matching.in_library = membership.contains_album(&candidate.matching.uri);
        }
        if state.selected_album_uris.is_empty()
            && !state.automatic_selection_disabled
            && !projection.representative_album.is_empty()
        {
            let source_rows = rows
                .iter()
                .filter(|row| {
                    row.artist == projection.representative_artist
                        && row.album == projection.representative_album
                })
                .cloned()
                .collect::<Vec<_>>();
            let mut candidates = state
                .cached_candidates
                .iter()
                .map(|candidate| candidate.matching.clone())
                .collect::<Vec<_>>();
            classify_album_candidates_for_rows(&source_rows, &mut candidates);
            if let Some(candidate) = automatic_album_candidate_for_rows(
                &projection.representative_album,
                &source_rows,
                &candidates,
            ) {
                state.selected_album_uris.push(candidate.uri.clone());
            }
        }
        state
            .full_album_choices
            .retain(|uri, _| state.selected_album_uris.contains(uri));
    }
    let selected = collection_selected_albums(session, batch_id)
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let selected_refs = selected.iter().collect::<Vec<_>>();
    let selected_tracks = collection_track_candidates(&selected_refs, membership);
    let selected_track_uris = selected_tracks
        .iter()
        .map(|candidate| candidate.uri.clone())
        .collect::<BTreeSet<_>>();
    let mut next_injected = BTreeMap::new();
    for row in rows {
        let previous = session.matches.get(&row.stable_id).cloned().or_else(|| {
            (!selected.is_empty()).then(|| MatchResult {
                source_id: row.stable_id.clone(),
                search_term: track_search_term(&row.artist, &row.track),
                confidence: None,
                selected_uri: None,
                candidates: Vec::new(),
                track_matches: BTreeMap::new(),
            })
        });
        let Some(previous) = previous else { continue };
        let prior_injected = previous_injected
            .get(&row.stable_id)
            .cloned()
            .unwrap_or_default();
        let baseline = remove_injected_collection_candidates(previous.clone(), &prior_injected);
        let baseline_uris = baseline
            .candidates
            .iter()
            .map(|candidate| candidate.uri.clone())
            .collect::<BTreeSet<_>>();
        let next = if selected.is_empty() {
            ratify_collection_result(&row, baseline, membership, mappings)
        } else {
            ratify_collection_result_with_selected_albums_and_injected(
                &row,
                previous.clone(),
                &selected_refs,
                &prior_injected,
                membership,
                mappings,
            )
        };
        if !selected.is_empty() {
            let injected = selected_track_uris
                .iter()
                .filter(|uri| {
                    !baseline_uris.contains(*uri)
                        && next
                            .candidates
                            .iter()
                            .any(|candidate| candidate.uri == **uri)
                })
                .cloned()
                .collect::<BTreeSet<_>>();
            if !injected.is_empty() {
                next_injected.insert(row.stable_id.clone(), injected);
            }
        }
        if next != previous {
            session.matches.insert(row.stable_id.clone(), next);
        }
    }
    if let Some(state) = session.collection_album_matches.get_mut(&batch_id) {
        state.injected_candidate_uris = next_injected;
    }
    Ok(())
}

pub(super) fn collection_membership_from(
    library: &crate::library_state::LibraryState,
    spotify_membership: &crate::spotify_membership::SpotifyMembership,
) -> CollectionMembership {
    let mut track_uris = library
        .lock()
        .expect("library mutex poisoned")
        .tracks()
        .iter()
        .map(|track| track.uri.clone())
        .collect::<BTreeSet<_>>();
    let spotify_library = spotify_membership.snapshot();
    let mut album_uris = BTreeSet::new();
    if spotify_library.is_exact() {
        album_uris.extend(spotify_library.saved_albums.keys().cloned());
        track_uris.extend(spotify_library.saved_tracks.into_keys());
        track_uris.extend(
            spotify_library
                .saved_albums
                .into_values()
                .flat_map(|album| album.track_uris),
        );
    }
    CollectionMembership {
        track_uris,
        album_uris,
    }
}

pub(super) fn locked_count_modes(session: &LastFmImportSessionV2) -> BTreeSet<String> {
    let selected_ids = session
        .page_options
        .values()
        .filter(|options| options.include_historical_play_counts)
        .flat_map(|options| options.selected_track_ids.iter())
        .collect::<BTreeSet<_>>();
    session
        .rows
        .iter()
        .filter(|row| {
            default_decision(session, &row.stable_id).status == RowStatus::Done
                && selected_ids.contains(&row.stable_id)
        })
        .filter_map(|row| {
            session
                .matches
                .get(&row.stable_id)
                .and_then(|result| matched_track_uri(result, &row.stable_id))
        })
        .collect()
}

pub(super) fn queue_item(
    session: &LastFmImportSessionV2,
    batch: &ImportBatch,
    rows: &[&SourceRow],
    apply_queue: &[ReviewApplyJob],
) -> Option<ImportQueueItem> {
    rows.first()?;
    let projection = batch_projection(batch, rows);
    let artist = projection.representative_artist.clone();
    let album = projection.representative_album.clone();
    let collection_shaped = batch_is_collection_shaped(session, batch, rows);
    let options = session.options_for_page_batch(batch, &artist, &album, rows);
    let remaining = rows.iter().any(|row| {
        let decision = default_decision(session, &row.stable_id);
        matches!(decision.status, RowStatus::Pending | RowStatus::Skipped) && !decision.excluded
    });
    let (imported_play_count, remaining_play_count) =
        rows.iter()
            .fold((0_u64, 0_u64), |(imported, remaining), row| {
                let decision = default_decision(session, &row.stable_id);
                match decision.status {
                    RowStatus::Done => (imported.saturating_add(row.play_count), remaining),
                    RowStatus::Pending | RowStatus::Skipped if !decision.excluded => {
                        (imported, remaining.saturating_add(row.play_count))
                    }
                    _ => (imported, remaining),
                }
            });
    let selected = rows
        .iter()
        .filter(|row| {
            let decision = default_decision(session, &row.stable_id);
            options.selected_track_ids.contains(&row.stable_id)
                && matches!(decision.status, RowStatus::Pending | RowStatus::Skipped)
                && !decision.excluded
        })
        .collect::<Vec<_>>();
    let mut album_entities = 0;
    let mut track_uris = BTreeSet::new();
    if options.import_content {
        if collection_shaped {
            let full_albums = collection_full_albums(session, batch.page);
            let covered_tracks = full_albums
                .iter()
                .flat_map(|album| album.matching.track_uris.iter())
                .collect::<BTreeSet<_>>();
            album_entities = full_albums.len() as u32;
            for row in &selected {
                if let Some(uri) = session
                    .matches
                    .get(&row.stable_id)
                    .and_then(|result| matched_track_uri_for_row(result, row, true))
                    .filter(|uri| !covered_tracks.contains(uri))
                {
                    track_uris.insert(uri);
                }
            }
        } else if options.whole_album {
            if !is_converted_collection_batch(session, batch.page, &album) {
                album_entities = selected
                    .iter()
                    .filter_map(|row| session.matches.get(&row.stable_id))
                    .filter_map(|result| {
                        result.selected_uri.as_deref().or_else(|| {
                            best_candidate(result).map(|candidate| candidate.uri.as_str())
                        })
                    })
                    .any(|uri| uri.starts_with("spotify:album:"))
                    as u32;
            }
        } else {
            for row in &selected {
                if let Some(result) = session.matches.get(&row.stable_id) {
                    if let Some(uri) = matched_track_uri_for_row(result, row, collection_shaped) {
                        track_uris.insert(uri);
                    }
                }
            }
        }
    }
    let failed = apply_queue.iter().find(|job| {
        job.plan.session_id == session.cache_id
            && job.plan.batch_id == batch.page
            && job.status == ApplyJobStatus::Failed
    });
    Some(ImportQueueItem {
        page: batch.page,
        artist,
        album,
        custom_batch: batch.custom,
        collection_shaped,
        album_label_count: projection.album_labels.len(),
        play_count: rows
            .iter()
            .map(|row| row.play_count)
            .fold(0, u64::saturating_add),
        imported_play_count,
        remaining_play_count,
        latest: rows.iter().map(|row| row.latest).max().unwrap_or_default(),
        source_count: batch.source_ids.len(),
        remaining,
        album_entities,
        track_entities: track_uris.len() as u32,
        status: failed
            .map(|_| QueueStatus::Failed)
            .or_else(|| queue_status(session, rows)),
        error: failed.and_then(|job| job.error.clone()),
        error_code: failed.map(|job| job.error_code.unwrap_or_default()),
        retry_at: failed.and_then(|job| job.retry_at),
    })
}

pub(super) fn queue_status(
    session: &LastFmImportSessionV2,
    rows: &[&SourceRow],
) -> Option<QueueStatus> {
    if rows
        .iter()
        .all(|row| default_decision(session, &row.stable_id).excluded)
    {
        return Some(QueueStatus::Excluded);
    }
    let first = rows
        .first()
        .map(|row| default_decision(session, &row.stable_id).status)?;
    if first == RowStatus::Pending
        || !rows
            .iter()
            .all(|row| default_decision(session, &row.stable_id).status == first)
    {
        return None;
    }
    Some(match first {
        RowStatus::Done => QueueStatus::Done,
        RowStatus::Skipped => QueueStatus::Skipped,
        RowStatus::IgnoredAlbum => QueueStatus::IgnoredAlbum,
        RowStatus::IgnoredArtist => QueueStatus::IgnoredArtist,
        RowStatus::Pending => return None,
    })
}

pub(super) fn update_review_phase(session: &mut LastFmImportSessionV2) {
    if session.remaining() == 0 {
        session.phase = ImportPhase::Done;
    } else if session.phase == ImportPhase::Done {
        session.phase = ImportPhase::Review;
    }
}

pub(super) fn review_phase_allowed(phase: ImportPhase) -> bool {
    matches!(phase, ImportPhase::Review | ImportPhase::Done)
}

pub(super) fn exclude_row(session: &mut LastFmImportSessionV2, id: &str, excluded: bool) {
    if is_reviewable(session, id) {
        let decision = session.decisions.entry(id.to_owned()).or_default();
        decision.excluded = excluded;
    }
}

pub(super) fn is_reviewable(session: &LastFmImportSessionV2, id: &str) -> bool {
    matches!(
        default_decision(session, id).status,
        RowStatus::Pending | RowStatus::Skipped
    )
}

pub(super) fn is_actionable(session: &LastFmImportSessionV2, id: &str) -> bool {
    is_reviewable(session, id) && !default_decision(session, id).excluded
}

pub(super) fn required_import_match_ids(
    session: &LastFmImportSessionV2,
    options: &PageOptions,
    rows: &[&SourceRow],
) -> BTreeSet<String> {
    if !options.include_historical_play_counts && options.whole_album {
        return BTreeSet::new();
    }
    rows.iter()
        .filter(|row| {
            options.selected_track_ids.contains(&row.stable_id)
                && is_actionable(session, &row.stable_id)
                && session
                    .matches
                    .get(&row.stable_id)
                    .and_then(|result| matched_track_uri(result, &row.stable_id))
                    .is_none()
        })
        .map(|row| row.stable_id.clone())
        .collect()
}

#[cfg(test)]
pub(super) fn album_source_ids(
    session: &LastFmImportSessionV2,
    artist: &str,
    album: &str,
) -> Vec<String> {
    session
        .rows
        .iter()
        .filter(|row| row.artist == artist && row.album == album)
        .map(|row| row.stable_id.clone())
        .collect()
}

pub(super) fn batch_scope_source_ids(
    session: &LastFmImportSessionV2,
    batch: &ImportBatch,
) -> Vec<String> {
    if batch.custom {
        return batch.source_ids.clone();
    }
    let rows_by_id = source_row_map(session);
    let target = batch_projection(batch, &batch_rows(batch, &rows_by_id));
    review_batches_for_read(session)
        .iter()
        .filter(|candidate| {
            let candidate_projection =
                batch_projection(candidate, &batch_rows(candidate, &rows_by_id));
            candidate_projection.collection_shaped == target.collection_shaped
                && candidate_projection.representative_artist == target.representative_artist
                && candidate_projection.representative_album == target.representative_album
        })
        .flat_map(|candidate| candidate.source_ids.iter().cloned())
        .collect()
}

pub(super) fn source_album_keys_for_ids(
    session: &LastFmImportSessionV2,
    source_ids: &[String],
) -> BTreeSet<String> {
    let rows = source_row_map(session);
    let identities = source_ids
        .iter()
        .filter_map(|id| rows.get(id.as_str()))
        .map(|row| (row.artist.as_str(), row.album.as_str()))
        .collect::<BTreeSet<_>>();
    let has_named_album = identities.iter().any(|(_, album)| !album.is_empty());
    identities
        .into_iter()
        .filter(|(_, album)| !has_named_album || !album.is_empty())
        .map(|(artist, album)| source_album_key(artist, album))
        .collect()
}

#[cfg(test)]
pub(crate) fn ignore_album(session: &mut LastFmImportSessionV2, artist: &str, album: &str) {
    let ids = album_source_ids(session, artist, album);
    for id in ids {
        if is_actionable(session, &id) {
            session.decisions.insert(
                id,
                RowDecision {
                    status: RowStatus::IgnoredAlbum,
                    excluded: false,
                },
            );
        }
    }
}

pub(crate) fn ignore_artist(session: &mut LastFmImportSessionV2, artist: &str) {
    let ids = session
        .rows
        .iter()
        .filter(|row| row.artist == artist && is_actionable(session, &row.stable_id))
        .map(|row| row.stable_id.clone())
        .collect::<Vec<_>>();
    for id in ids {
        session.decisions.insert(
            id,
            RowDecision {
                status: RowStatus::IgnoredArtist,
                excluded: false,
            },
        );
    }
}

#[cfg(test)]
pub(crate) fn skip_album(session: &mut LastFmImportSessionV2, artist: &str, album: &str) {
    let ids = album_source_ids(session, artist, album);
    for id in ids {
        if is_actionable(session, &id) {
            session.decisions.insert(
                id,
                RowDecision {
                    status: RowStatus::Skipped,
                    excluded: false,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, artist: &str, album: &str) -> SourceRow {
        SourceRow {
            stable_id: id.into(),
            artist: artist.into(),
            album: album.into(),
            track: id.into(),
            variants: Vec::new(),
            play_count: 1,
            earliest: 1,
            latest: 1,
        }
    }

    #[test]
    fn review_actions_cascade_and_remaining_count_is_durable() {
        let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 10);
        session.rows = vec![
            row("one", "A", "Album"),
            row("two", "A", "Other"),
            row("three", "B", "Album"),
        ];

        assert_eq!(session.remaining(), 3);
        skip_album(&mut session, "A", "Album");
        assert_eq!(session.remaining(), 3);
        session.decisions.values_mut().for_each(|decision| {
            if decision.status == RowStatus::Skipped {
                decision.status = RowStatus::Pending;
            }
        });
        ignore_artist(&mut session, "A");
        assert_eq!(session.remaining(), 1);
        exclude_row(&mut session, "three", true);
        assert!(session.decisions.values().any(|decision| decision.excluded));
        ignore_album(&mut session, "B", "Album");
        assert_eq!(session.remaining(), 0);
    }

    #[test]
    fn excluded_rows_have_view_only_queue_status() {
        let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 10);
        session.rows = vec![row("one", "A", "Album"), row("two", "A", "Album")];
        for id in ["one", "two"] {
            session.decisions.insert(
                id.into(),
                RowDecision {
                    status: RowStatus::Pending,
                    excluded: true,
                },
            );
        }

        assert_eq!(
            queue_status(&session, &session.rows.iter().collect::<Vec<_>>()),
            Some(QueueStatus::Excluded)
        );
        assert_eq!(session.remaining(), 0);
    }
}

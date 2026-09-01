use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
};

use super::model::{
    AlbumCandidate, AlbumRelation, CollectionAlbumCandidate, Confidence, LastFmMappings,
    MatchResult, SourceRow,
};
use super::reconciliation::source_album_key;
use super::source::{normalize_for_match, source_id};
use super::{normalize_catalog_text, source_artists_compatible};

pub(super) fn album_search_term(artist: &str, album: &str) -> String {
    let artist = artist.replace('"', " ");
    let simplified = without_parenthetical_text(album);
    let album = if simplified.trim().is_empty() {
        album
    } else {
        simplified.trim()
    }
    .replace('"', " ");
    format!("album:\"{album}\" artist:\"{artist}\"")
}

pub(super) fn track_search_term(artist: &str, track: &str) -> String {
    let artist = artist.replace('"', " ");
    let simplified = without_parenthetical_text(track);
    let track = if simplified.trim().is_empty() {
        track
    } else {
        simplified.trim()
    }
    .replace(['/', '"'], " ");
    let track = track.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("track:\"{track}\" artist:\"{artist}\"")
}

pub(super) fn collection_album_search_term(query: &str) -> String {
    query.trim().to_owned()
}

pub(super) fn is_album_search_term(search_term: &str) -> bool {
    search_term
        .trim_start()
        .get(.."album:".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("album:"))
}

pub(crate) fn classify_album_candidates_by_name(
    source_track_names: &[String],
    candidates: &mut [AlbumCandidate],
) {
    classify_album_candidates_with(source_track_names, candidates, |source, targets| {
        album_track_match_index(source, targets)
    });
}

pub(super) fn classify_album_candidates_for_rows(
    rows: &[SourceRow],
    candidates: &mut [AlbumCandidate],
) {
    classify_album_candidates_with(rows, candidates, release_track_match_index);
}

fn classify_album_candidates_with<T>(
    source: &[T],
    candidates: &mut [AlbumCandidate],
    match_index: impl Fn(&T, &[String]) -> Option<usize>,
) {
    for candidate in candidates.iter_mut() {
        let matched = source
            .iter()
            .filter_map(|source| match_index(source, &candidate.track_names))
            .collect::<Vec<_>>();
        let unique_targets = matched.iter().copied().collect::<BTreeSet<_>>().len();
        candidate.relation = if matched.len() == source.len()
            && unique_targets == candidate.track_names.len()
            && candidate.track_names.len() == source.len()
        {
            Some(AlbumRelation::BestMatch)
        } else if matched.len() == source.len() && candidate.track_names.len() > source.len() {
            Some(AlbumRelation::Superset)
        } else if !matched.is_empty() && matched.len() * 2 >= source.len().max(1) {
            Some(AlbumRelation::SameSongs)
        } else {
            None
        };
    }
}

pub(super) fn album_track_match_index(source: &str, targets: &[String]) -> Option<usize> {
    let exact = normalize_catalog_text(source);
    if exact.is_empty() {
        return None;
    }
    if let Some(index) = targets
        .iter()
        .position(|target| normalize_catalog_text(target) == exact)
    {
        return Some(index);
    }
    let source_words = normalized_word_sequences(source);
    let mut equivalent = targets
        .iter()
        .enumerate()
        .filter(|(_, target)| normalized_word_sequences(target) == source_words);
    if let Some((index, _)) = equivalent.next() {
        if equivalent.next().is_none() {
            return Some(index);
        }
    }
    let mut prefixed = targets
        .iter()
        .enumerate()
        .filter(|(_, target)| normalize_catalog_text(target).starts_with(&exact));
    if let Some((index, _)) = prefixed.next() {
        if prefixed.next().is_none() {
            return Some(index);
        }
    }
    let mut compatible = targets
        .iter()
        .enumerate()
        .filter(|(_, target)| titles_share_contained_words(source, target));
    let (index, _) = compatible.next()?;
    compatible.next().is_none().then_some(index)
}

pub(super) fn release_track_match_index(source: &SourceRow, targets: &[String]) -> Option<usize> {
    album_track_match_index(&source.track, targets).or_else(|| {
        let title = without_known_source_suffix(&source.track, &source.artist, &source.album);
        (title != source.track)
            .then(|| album_track_match_index(&title, targets))
            .flatten()
            .or_else(|| unique_significant_token_match(&title, targets))
    })
}

pub(super) fn without_known_source_suffix(track: &str, artist: &str, album: &str) -> String {
    for separator in [" - ", " – ", " — "] {
        let Some((title, suffix)) = track.rsplit_once(separator) else {
            continue;
        };
        let suffix = normalize_catalog_text(suffix);
        if !suffix.is_empty()
            && (suffix == normalize_catalog_text(artist)
                || (!album.is_empty() && suffix == normalize_catalog_text(album)))
        {
            return title.trim().to_owned();
        }
    }
    track.to_owned()
}

pub(super) fn significant_title_tokens(value: &str) -> BTreeSet<String> {
    value
        .split(|character: char| !character.is_alphanumeric())
        .map(normalize_catalog_text)
        .filter(|word| word.chars().count() > 3)
        .collect()
}

fn unique_significant_token_match(source: &str, targets: &[String]) -> Option<usize> {
    let source = significant_title_tokens(source);
    let mut scored = targets.iter().enumerate().filter_map(|(index, target)| {
        let target = significant_title_tokens(target);
        let overlap = source.intersection(&target).count();
        let shorter = source.len().min(target.len());
        (overlap >= 2 && overlap * 2 >= shorter).then_some((index, overlap, shorter))
    });
    let best = scored.next()?;
    let mut tied = false;
    let best = scored.fold(best, |best, candidate| {
        let best_score = (best.1 * 100 / best.2, best.1);
        let candidate_score = (candidate.1 * 100 / candidate.2, candidate.1);
        if candidate_score > best_score {
            tied = false;
            candidate
        } else {
            if candidate_score == best_score {
                tied = true;
            }
            best
        }
    });
    (!tied).then_some(best.0)
}

#[cfg(test)]
pub(super) fn automatic_album_candidate<'a>(
    album: &str,
    source_track_names: &[String],
    candidates: &'a [AlbumCandidate],
) -> Option<&'a AlbumCandidate> {
    automatic_album_candidate_with(album, source_track_names, candidates, |source, targets| {
        album_track_match_index(source, targets)
    })
}

pub(super) fn automatic_album_candidate_for_rows<'a>(
    album: &str,
    rows: &[SourceRow],
    candidates: &'a [AlbumCandidate],
) -> Option<&'a AlbumCandidate> {
    automatic_album_candidate_with(album, rows, candidates, release_track_match_index)
}

fn automatic_album_candidate_with<'a, T>(
    album: &str,
    source: &[T],
    candidates: &'a [AlbumCandidate],
    match_index: impl Fn(&T, &[String]) -> Option<usize>,
) -> Option<&'a AlbumCandidate> {
    let source_count = source.len();
    let mut supported = candidates
        .iter()
        .filter_map(|candidate| {
            if candidate.relation.is_none()
                || !titles_share_contained_words(album, &candidate.name)
                || source_count == 0
                || candidate.track_names.is_empty()
            {
                return None;
            }
            let matched = source
                .iter()
                .filter_map(|source| match_index(source, &candidate.track_names))
                .collect::<Vec<_>>();
            let unique_targets = matched.iter().copied().collect::<BTreeSet<_>>().len();
            ((matched.len() == source_count && unique_targets == source_count)
                || (matched.len() * 5 >= source_count * 4
                    && unique_targets * 5 >= candidate.track_names.len() * 4))
                .then_some((candidate, matched.len(), candidate.track_names.len()))
        })
        .collect::<Vec<_>>();
    supported.sort_by_key(|(_, matched, tracks)| (std::cmp::Reverse(*matched), *tracks));
    let (candidate, matched, tracks) = *supported.first()?;
    supported
        .get(1)
        .is_none_or(|next| (matched, tracks) != (next.1, next.2))
        .then_some(candidate)
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct CollectionMembership {
    pub(super) track_uris: BTreeSet<String>,
}

impl CollectionMembership {
    pub(super) fn contains(&self, uri: &str) -> bool {
        self.track_uris.contains(uri)
    }
}

pub(super) fn collection_track_candidates(
    albums: &[&CollectionAlbumCandidate],
    membership: &CollectionMembership,
) -> Vec<AlbumCandidate> {
    let mut candidates = Vec::new();
    let mut seen = BTreeSet::new();
    for album in albums {
        let matching = &album.matching;
        for uri in &matching.track_uris {
            if !seen.insert(uri.clone()) {
                continue;
            }
            candidates.push(
                album_track_candidate(matching, uri, membership.contains(uri))
                    .expect("track URI came from this album"),
            );
        }
    }
    candidates
}

pub(super) fn album_track_candidate(
    album: &AlbumCandidate,
    uri: &str,
    in_library: bool,
) -> Option<AlbumCandidate> {
    let index = album.track_uris.iter().position(|track| track == uri)?;
    let name = album
        .track_names
        .get(index)
        .cloned()
        .unwrap_or_else(|| album.name.clone());
    let artist = album
        .track_artists
        .get(index)
        .cloned()
        .filter(|artist| !artist.trim().is_empty())
        .unwrap_or_else(|| album.artist.clone());
    let album_name = album
        .track_albums
        .get(index)
        .cloned()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| album.name.clone());
    Some(AlbumCandidate {
        uri: uri.to_owned(),
        name: name.clone(),
        artist: artist.clone(),
        in_library,
        track_uris: vec![uri.to_owned()],
        track_names: vec![name],
        track_artists: vec![artist],
        track_albums: vec![album_name],
        relation: None,
    })
}

pub(super) fn remove_injected_collection_candidates(
    mut result: MatchResult,
    injected_candidate_uris: &BTreeSet<String>,
) -> MatchResult {
    if injected_candidate_uris.is_empty() {
        return result;
    }
    let selected_uri = result.selected_uri.as_deref();
    let explicit_track_uris = result
        .track_matches
        .values()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    result.candidates.retain(|candidate| {
        !injected_candidate_uris.contains(&candidate.uri)
            || selected_uri == Some(candidate.uri.as_str())
            || (selected_uri.is_some() && explicit_track_uris.contains(candidate.uri.as_str()))
    });
    result
        .track_matches
        .retain(|_, uri| !injected_candidate_uris.contains(uri) || selected_uri.is_some());
    result
}

pub(super) fn collection_album_candidate_from_release(
    candidate: AlbumCandidate,
) -> CollectionAlbumCandidate {
    CollectionAlbumCandidate {
        total_tracks: candidate.track_uris.len() as u32,
        matching: candidate,
        ..CollectionAlbumCandidate::default()
    }
}

fn candidate_primary_artist(candidate: &AlbumCandidate) -> &str {
    if !candidate.artist.is_empty() {
        &candidate.artist
    } else {
        candidate
            .track_artists
            .first()
            .map(String::as_str)
            .unwrap_or_default()
    }
}

fn collection_candidate_matches_artist(row: &SourceRow, candidate: &AlbumCandidate) -> bool {
    let candidate_artist = normalize_catalog_text(candidate_primary_artist(candidate));
    !candidate_artist.is_empty()
        && (normalize_catalog_text(&row.artist) == candidate_artist
            || row
                .variants
                .iter()
                .any(|variant| normalize_catalog_text(&variant.artist) == candidate_artist))
}

pub(super) fn collection_candidate_matches_title(
    row: &SourceRow,
    candidate: &AlbumCandidate,
) -> bool {
    let candidate_title = normalize_catalog_text(&candidate.name);
    if candidate_title.is_empty() {
        return false;
    }
    row.variants
        .iter()
        .map(|variant| variant.track.as_str())
        .chain(std::iter::once(row.track.as_str()))
        .any(|track| normalize_catalog_text(track) == candidate_title)
        || candidate.track_names.iter().any(|track| {
            row.variants
                .iter()
                .map(|variant| variant.track.as_str())
                .chain(std::iter::once(row.track.as_str()))
                .any(|source| normalize_catalog_text(source) == normalize_catalog_text(track))
        })
}

fn without_parenthetical_text(value: &str) -> String {
    let mut parenthesis_depth = 0_u32;
    value
        .chars()
        .filter(|character| match character {
            '(' => {
                parenthesis_depth += 1;
                false
            }
            ')' => {
                parenthesis_depth = parenthesis_depth.saturating_sub(1);
                false
            }
            _ => parenthesis_depth == 0,
        })
        .collect::<String>()
}

pub(super) fn normalized_word_sequences(value: &str) -> Vec<String> {
    without_parenthetical_text(value)
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(normalize_catalog_text)
        .filter(|word| !word.is_empty())
        .collect()
}

fn title_token_overlap(left: &str, right: &str) -> usize {
    let left = normalized_word_sequences(left)
        .into_iter()
        .collect::<BTreeSet<_>>();
    let right = normalized_word_sequences(right)
        .into_iter()
        .collect::<BTreeSet<_>>();
    left.intersection(&right).count()
}

fn differs_by_one_inserted_character(left: &str, right: &str) -> bool {
    let (shorter, longer) = if left.chars().count() < right.chars().count() {
        (left, right)
    } else {
        (right, left)
    };
    if shorter.chars().count() + 1 != longer.chars().count() {
        return false;
    }
    let mut shorter = shorter.chars();
    let mut expected = shorter.next();
    let mut skipped = false;
    for character in longer.chars() {
        if expected == Some(character) {
            expected = shorter.next();
        } else if skipped {
            return false;
        } else {
            skipped = true;
        }
    }
    expected.is_none()
}

fn differs_by_one_adjacent_transposition(left: &str, right: &str) -> bool {
    if left.chars().count() != right.chars().count() {
        return false;
    }
    let mut differences = left
        .chars()
        .zip(right.chars())
        .enumerate()
        .filter(|(_, (left, right))| left != right);
    let Some((first_index, (first_left, first_right))) = differences.next() else {
        return false;
    };
    let Some((second_index, (second_left, second_right))) = differences.next() else {
        return false;
    };
    differences.next().is_none()
        && second_index == first_index + 1
        && first_left == second_right
        && first_right == second_left
}

fn titles_differ_by_one_token_typo(left: &BTreeSet<String>, right: &BTreeSet<String>) -> bool {
    let left_only = left.difference(right).collect::<Vec<_>>();
    let right_only = right.difference(left).collect::<Vec<_>>();
    left_only.len() == 1
        && right_only.len() == 1
        && left_only[0]
            .chars()
            .count()
            .min(right_only[0].chars().count())
            >= 5
        && (differs_by_one_inserted_character(left_only[0], right_only[0])
            || differs_by_one_adjacent_transposition(left_only[0], right_only[0]))
}

pub(super) fn titles_share_contained_words(left: &str, right: &str) -> bool {
    let left = normalized_word_sequences(left)
        .into_iter()
        .collect::<BTreeSet<_>>();
    let right = normalized_word_sequences(right)
        .into_iter()
        .collect::<BTreeSet<_>>();
    !left.is_empty()
        && !right.is_empty()
        && (left.is_subset(&right)
            || right.is_subset(&left)
            || titles_differ_by_one_token_typo(&left, &right))
}

fn collection_candidate_title_overlap(row: &SourceRow, candidate: &AlbumCandidate) -> usize {
    std::iter::once(row.track.as_str())
        .chain(row.variants.iter().map(|variant| variant.track.as_str()))
        .flat_map(|source| {
            std::iter::once(candidate.name.as_str())
                .chain(candidate.track_names.iter().map(String::as_str))
                .map(move |target| title_token_overlap(source, target))
        })
        .max()
        .unwrap_or(0)
}

fn collection_candidate_is_same_songs_title(row: &SourceRow, candidate: &AlbumCandidate) -> bool {
    let source_titles = std::iter::once(row.track.as_str())
        .chain(row.variants.iter().map(|variant| variant.track.as_str()));
    let candidate_titles = std::iter::once(candidate.name.as_str())
        .chain(candidate.track_names.iter().map(String::as_str));
    source_titles.clone().any(|source| {
        candidate_titles
            .clone()
            .any(|candidate_title| titles_share_contained_words(source, candidate_title))
    })
}

fn collection_candidate_is_exact(row: &SourceRow, candidate: &AlbumCandidate) -> bool {
    collection_candidate_matches_title(row, candidate)
}

fn collection_candidate_is_same_songs(row: &SourceRow, candidate: &AlbumCandidate) -> bool {
    collection_candidate_is_same_songs_title(row, candidate)
}

pub(super) fn collection_best_title_matches<'a>(
    row: &SourceRow,
    candidates: &'a [AlbumCandidate],
) -> Vec<&'a AlbumCandidate> {
    let exact = candidates
        .iter()
        .filter(|candidate| collection_candidate_is_exact(row, candidate))
        .collect::<Vec<_>>();
    if exact.is_empty() {
        candidates
            .iter()
            .filter(|candidate| collection_candidate_is_same_songs(row, candidate))
            .collect()
    } else {
        exact
    }
}

fn collection_title_confidence(row: &SourceRow, candidate: &AlbumCandidate) -> Confidence {
    if collection_candidate_is_exact(row, candidate) {
        Confidence::Exact
    } else {
        Confidence::Likely
    }
}

fn collection_candidate_rank(row: &SourceRow, candidate: &AlbumCandidate) -> u8 {
    if collection_candidate_is_exact(row, candidate) {
        if candidate.in_library {
            0
        } else {
            1
        }
    } else if collection_candidate_is_same_songs(row, candidate) {
        if candidate.in_library {
            2
        } else {
            3
        }
    } else {
        4
    }
}

fn collection_mapping_uri(row: &SourceRow, mappings: &LastFmMappings) -> Option<String> {
    let key = source_id(&row.artist, &row.album, &row.track);
    mappings
        .track_mappings
        .get(&key)
        .cloned()
        .or_else(|| {
            row.stable_id
                .strip_prefix("incremental:")
                .and_then(|source_key| mappings.track_mappings.get(source_key).cloned())
        })
        .or_else(|| {
            mappings
                .album_mappings
                .get(&source_album_key(&row.artist, &row.album))
                .and_then(|mapping| {
                    mapping
                        .track_uris_by_name
                        .get(&normalize_for_match(&row.track))
                        .cloned()
                })
        })
}

fn mapping_candidate(
    row: &SourceRow,
    uri: &str,
    membership: &CollectionMembership,
) -> AlbumCandidate {
    AlbumCandidate {
        uri: uri.to_owned(),
        name: row.track.clone(),
        artist: row.artist.clone(),
        in_library: membership.contains(uri),
        track_uris: vec![uri.to_owned()],
        track_names: vec![row.track.clone()],
        track_artists: vec![row.artist.clone()],
        track_albums: vec![String::new()],
        relation: Some(AlbumRelation::BestMatch),
    }
}

pub(super) fn rank_collection_candidates(
    row: &SourceRow,
    candidates: &mut Vec<AlbumCandidate>,
    membership: &CollectionMembership,
) {
    for candidate in candidates.iter_mut() {
        candidate.in_library = membership.contains(&candidate.uri);
        candidate.relation = if collection_candidate_is_exact(row, candidate) {
            Some(AlbumRelation::BestMatch)
        } else if collection_candidate_is_same_songs(row, candidate) {
            Some(AlbumRelation::SameSongs)
        } else {
            None
        };
    }
    candidates.sort_by_cached_key(|candidate| {
        (
            collection_candidate_rank(row, candidate),
            !collection_candidate_matches_artist(row, candidate),
            Reverse(collection_candidate_title_overlap(row, candidate)),
            normalize_for_match(&candidate.name),
            candidate.uri.clone(),
        )
    });
    candidates.truncate(10);
}

pub(super) fn ratify_collection_result(
    row: &SourceRow,
    mut result: MatchResult,
    membership: &CollectionMembership,
    mappings: &LastFmMappings,
) -> MatchResult {
    result
        .track_matches
        .retain(|_, uri| uri.starts_with("spotify:track:"));
    if result
        .selected_uri
        .as_deref()
        .is_some_and(|uri| !uri.starts_with("spotify:track:"))
    {
        result.selected_uri = None;
    }
    rank_collection_candidates(row, &mut result.candidates, membership);
    if let Some(uri) =
        collection_mapping_uri(row, mappings).filter(|uri| uri.starts_with("spotify:track:"))
    {
        if !result
            .candidates
            .iter()
            .any(|candidate| candidate.uri == uri)
        {
            result
                .candidates
                .push(mapping_candidate(row, &uri, membership));
        }
        result.selected_uri = None;
        result.confidence = Some(Confidence::Exact);
        result.track_matches = BTreeMap::from([(row.stable_id.clone(), uri)]);
    } else if result.selected_uri.is_some() {
        // An explicit picker choice is durable session state and outranks fresh
        // collection heuristics.
    } else {
        result.selected_uri = None;
        result.track_matches.clear();
        result.confidence = None;
        let supported = collection_best_title_matches(row, &result.candidates);
        let owned = supported
            .iter()
            .copied()
            .filter(|candidate| candidate.in_library)
            .collect::<Vec<_>>();
        let selected = if owned.len() == 1 {
            owned.first().copied()
        } else if owned.is_empty() && supported.len() == 1 {
            supported.first().copied()
        } else {
            None
        };
        if let Some(candidate) = selected {
            result.confidence = Some(collection_title_confidence(row, candidate));
            if candidate.uri.starts_with("spotify:track:") {
                result
                    .track_matches
                    .insert(row.stable_id.clone(), candidate.uri.clone());
            }
        }
    }
    rank_collection_candidates(row, &mut result.candidates, membership);
    if let Some(uri) =
        collection_mapping_uri(row, mappings).filter(|uri| uri.starts_with("spotify:track:"))
    {
        if !result
            .candidates
            .iter()
            .any(|candidate| candidate.uri == uri)
        {
            result
                .candidates
                .insert(0, mapping_candidate(row, &uri, membership));
            result.candidates.truncate(10);
        }
    }
    result
}

#[cfg(test)]
pub(super) fn ratify_collection_result_with_selected_albums(
    row: &SourceRow,
    result: MatchResult,
    albums: &[&CollectionAlbumCandidate],
    membership: &CollectionMembership,
    mappings: &LastFmMappings,
) -> MatchResult {
    ratify_collection_result_with_selected_albums_and_injected(
        row,
        result,
        albums,
        &BTreeSet::new(),
        membership,
        mappings,
    )
}

pub(super) fn ratify_collection_result_with_selected_albums_and_injected(
    row: &SourceRow,
    mut result: MatchResult,
    albums: &[&CollectionAlbumCandidate],
    injected_candidate_uris: &BTreeSet<String>,
    membership: &CollectionMembership,
    mappings: &LastFmMappings,
) -> MatchResult {
    result
        .track_matches
        .retain(|_, uri| uri.starts_with("spotify:track:"));
    if result
        .selected_uri
        .as_deref()
        .is_some_and(|uri| !uri.starts_with("spotify:track:"))
    {
        result.selected_uri = None;
    }
    if albums.is_empty() {
        return ratify_collection_result(
            row,
            remove_injected_collection_candidates(result, injected_candidate_uris),
            membership,
            mappings,
        );
    }
    let previous = remove_injected_collection_candidates(result.clone(), injected_candidate_uris);
    let selected_tracks = collection_track_candidates(albums, membership);
    let mut candidates = previous.candidates.clone();
    for selected in selected_tracks.iter().cloned() {
        if !candidates
            .iter()
            .any(|candidate| candidate.uri == selected.uri)
        {
            candidates.push(selected);
        }
    }
    rank_collection_candidates(row, &mut candidates, membership);
    result.candidates = candidates;

    if let Some(uri) =
        collection_mapping_uri(row, mappings).filter(|uri| uri.starts_with("spotify:track:"))
    {
        if !result
            .candidates
            .iter()
            .any(|candidate| candidate.uri == uri)
        {
            result
                .candidates
                .push(mapping_candidate(row, &uri, membership));
        }
        result.selected_uri = None;
        result.confidence = Some(Confidence::Exact);
        result.track_matches = BTreeMap::from([(row.stable_id.clone(), uri)]);
        return result;
    }
    let supported_selected = collection_best_title_matches(row, &selected_tracks);
    let selected_candidate = (supported_selected.len() == 1)
        .then(|| supported_selected[0])
        .filter(|candidate| candidate.uri.starts_with("spotify:track:"));
    let selected_track_uri = selected_candidate.map(|candidate| candidate.uri.clone());
    if let Some(selected_uri) = result.selected_uri.clone() {
        // A track picker choice is durable session state and outranks a changed set.
        if selected_track_uri.as_deref() == Some(selected_uri.as_str()) {
            if let Some(candidate) = result
                .candidates
                .iter_mut()
                .find(|candidate| candidate.uri == selected_uri)
            {
                candidate.relation = Some(if collection_candidate_is_exact(row, candidate) {
                    AlbumRelation::BestMatch
                } else {
                    AlbumRelation::SameSongs
                });
            }
            result.confidence =
                selected_candidate.map(|candidate| collection_title_confidence(row, candidate));
        }
        return result;
    }

    result.selected_uri = None;
    result.track_matches.clear();
    result.confidence = None;
    if let Some(candidate) = selected_candidate {
        let uri = candidate.uri.clone();
        let confidence = collection_title_confidence(row, candidate);
        if let Some(candidate) = result
            .candidates
            .iter_mut()
            .find(|candidate| candidate.uri == uri)
        {
            candidate.relation = Some(if confidence == Confidence::Exact {
                AlbumRelation::BestMatch
            } else {
                AlbumRelation::SameSongs
            });
        }
        result.confidence = Some(confidence);
        result.track_matches.insert(row.stable_id.clone(), uri);
        return result;
    }
    if supported_selected.len() > 1 {
        // Distinct editions with the same strongest title relation stay ambiguous.
        return result;
    }

    // Keep the existing library-aware fallback for rows not covered by the set.
    let mut fallback = previous;
    rank_collection_candidates(row, &mut fallback.candidates, membership);
    let supported = collection_best_title_matches(row, &fallback.candidates);
    let owned = supported
        .iter()
        .copied()
        .filter(|candidate| candidate.in_library)
        .collect::<Vec<_>>();
    let selected = if owned.len() == 1 {
        owned.first().copied()
    } else if owned.is_empty() && supported.len() == 1 {
        supported.first().copied()
    } else {
        None
    };
    if let Some(candidate) = selected {
        if candidate.uri.starts_with("spotify:track:") {
            result.confidence = Some(collection_title_confidence(row, candidate));
            result
                .track_matches
                .insert(row.stable_id.clone(), candidate.uri.clone());
        }
    }
    result
}

pub(super) fn candidate_rank(relation: Option<AlbumRelation>) -> u8 {
    match relation {
        Some(AlbumRelation::BestMatch) => 0,
        Some(AlbumRelation::SameSongs) => 1,
        Some(AlbumRelation::Superset) => 2,
        None => 3,
    }
}

pub(super) fn update_selected_match(
    result: &mut MatchResult,
    source_id: &str,
    source_track: &str,
    candidate: &AlbumCandidate,
) {
    result.selected_uri = Some(candidate.uri.clone());
    result.confidence = Some(selected_match_confidence(source_track, candidate));
    result.track_matches.remove(source_id);
    if let Some(index) = album_track_match_index(source_track, &candidate.track_names) {
        if let Some(track_uri) = candidate.track_uris.get(index) {
            result
                .track_matches
                .insert(source_id.to_owned(), track_uri.clone());
        }
    } else if candidate.uri.starts_with("spotify:track:") {
        result
            .track_matches
            .insert(source_id.to_owned(), candidate.uri.clone());
    }
}

pub(super) fn update_selected_release_match(
    result: &mut MatchResult,
    source: &SourceRow,
    candidate: &AlbumCandidate,
) {
    let title = without_known_source_suffix(&source.track, &source.artist, &source.album);
    update_selected_match(result, &source.stable_id, &title, candidate);
    if !result.track_matches.contains_key(&source.stable_id) {
        if let Some(index) = release_track_match_index(source, &candidate.track_names) {
            if let Some(uri) = candidate.track_uris.get(index) {
                result
                    .track_matches
                    .insert(source.stable_id.clone(), uri.clone());
            }
        }
    }
}

pub(super) fn selected_match_confidence(
    source_track: &str,
    candidate: &AlbumCandidate,
) -> Confidence {
    match candidate.relation {
        Some(AlbumRelation::BestMatch) => Confidence::Exact,
        Some(AlbumRelation::SameSongs | AlbumRelation::Superset) => Confidence::Likely,
        None if album_track_match_index(source_track, &candidate.track_names).is_some() => {
            Confidence::Likely
        }
        None => Confidence::Low,
    }
}

fn album_summary_title_tier(source: &str, candidate: &str) -> Option<u8> {
    let source_compact = normalize_for_match(source);
    let candidate_compact = normalize_for_match(candidate);
    if source_compact.is_empty() || candidate_compact.is_empty() {
        return None;
    }
    if source_compact == candidate_compact {
        return Some(0);
    }
    if normalized_word_sequences(source) == normalized_word_sequences(candidate) {
        return Some(1);
    }
    if titles_share_contained_words(source, candidate) {
        return Some(2);
    }
    let source_tokens = significant_title_tokens(source);
    let candidate_tokens = significant_title_tokens(candidate);
    let shorter = source_tokens.len().min(candidate_tokens.len());
    (shorter > 0 && source_tokens.intersection(&candidate_tokens).count() * 5 >= shorter * 4)
        .then_some(3)
}

fn album_summary_track_rank(source_count: usize, candidate_count: u32) -> Option<(u8, u32)> {
    if source_count > 0 && candidate_count > 0 && (candidate_count as usize) < source_count {
        return None;
    }
    if candidate_count == 0 {
        Some((1, u32::MAX))
    } else {
        Some((0, candidate_count.saturating_sub(source_count as u32)))
    }
}

pub(super) fn supported_album_summaries(
    summaries: Vec<crate::provider::SearchAlbum>,
    source_album: &str,
    source_artist: &str,
    source_track_names: &[String],
) -> Vec<crate::provider::SearchAlbum> {
    let mut supported = summaries
        .into_iter()
        .enumerate()
        .filter_map(|(spotify_order, album)| {
            let title_tier = album_summary_title_tier(source_album, &album.name)?;
            let track_rank = album_summary_track_rank(source_track_names.len(), album.track_count)?;
            let artist_rank =
                if normalize_catalog_text(source_artist) == normalize_catalog_text(&album.artist) {
                    0
                } else if source_artists_compatible(source_artist, &album.artist) {
                    1
                } else {
                    2
                };
            Some((title_tier, track_rank, artist_rank, spotify_order, album))
        })
        .collect::<Vec<_>>();
    let Some(strongest_title_tier) = supported.iter().map(|item| item.0).min() else {
        return Vec::new();
    };
    supported.retain(|item| item.0 == strongest_title_tier);
    supported.sort_by_key(|(title_tier, track_rank, artist_rank, spotify_order, _)| {
        (*title_tier, *track_rank, *artist_rank, *spotify_order)
    });
    supported
        .into_iter()
        .take(3)
        .map(|(_, _, _, _, album)| album)
        .collect()
}

pub(super) fn collection_album_summary(
    album: crate::provider::SearchAlbum,
) -> CollectionAlbumCandidate {
    CollectionAlbumCandidate {
        matching: AlbumCandidate {
            uri: album.uri,
            name: album.name,
            artist: album.artist,
            in_library: album.in_library,
            track_uris: Vec::new(),
            track_names: Vec::new(),
            track_artists: Vec::new(),
            track_albums: Vec::new(),
            relation: None,
        },
        image_url: album.image_url,
        release_date: album.year.clone(),
        album_type: album.album_type,
        total_tracks: album.track_count,
        track_numbers: Vec::new(),
        track_durations: Vec::new(),
    }
}

pub(super) fn collection_album_candidate(
    album: &retune_spotify::client::Album,
    membership: &CollectionMembership,
) -> CollectionAlbumCandidate {
    let artist = album
        .artists
        .first()
        .map(|artist| artist.name.clone())
        .unwrap_or_default();
    let tracks = album
        .tracks
        .as_ref()
        .map(|page| page.items.as_slice())
        .unwrap_or_default();
    let matching = AlbumCandidate {
        uri: album.uri.clone(),
        name: album.name.clone(),
        artist: artist.clone(),
        in_library: tracks.iter().any(|track| membership.contains(&track.uri)),
        track_uris: tracks.iter().map(|track| track.uri.clone()).collect(),
        track_names: tracks.iter().map(|track| track.name.clone()).collect(),
        track_artists: tracks
            .iter()
            .map(|track| {
                track
                    .artists
                    .first()
                    .map(|artist| artist.name.clone())
                    .unwrap_or_else(|| artist.clone())
            })
            .collect(),
        track_albums: vec![album.name.clone(); tracks.len()],
        relation: None,
    };
    CollectionAlbumCandidate {
        matching,
        image_url: crate::provider::image_url(&album.images),
        release_date: album.release_date.clone(),
        album_type: album.album_type.clone(),
        total_tracks: album.total_tracks.max(tracks.len() as u32),
        track_numbers: tracks.iter().map(|track| track.track_number).collect(),
        track_durations: tracks
            .iter()
            .map(|track| track.duration_ms.unwrap_or_default() / 1_000)
            .collect(),
    }
}

pub(super) fn album_tracks_complete(album: &retune_spotify::client::Album) -> bool {
    let Some(tracks) = album.tracks.as_ref() else {
        return false;
    };
    tracks.skipped == 0
        && tracks.items.len().saturating_add(tracks.skipped)
            >= album.total_tracks.max(tracks.total) as usize
}

pub(super) fn match_result_for(
    source_id: String,
    search_term: String,
    mut candidates: Vec<AlbumCandidate>,
    source_track: &str,
    selected_uri: Option<&str>,
) -> MatchResult {
    let selected =
        selected_uri.and_then(|uri| candidates.iter().find(|candidate| candidate.uri == uri));
    let confidence = selected.map(|candidate| selected_match_confidence(source_track, candidate));
    let selected_uri = selected
        .filter(|candidate| candidate.relation.is_some())
        .map(|candidate| candidate.uri.clone());
    let mut track_matches = BTreeMap::new();
    if let Some(selected) = selected.filter(|candidate| candidate.relation.is_some()) {
        if let Some(index) = album_track_match_index(source_track, &selected.track_names) {
            if let Some(uri) = selected.track_uris.get(index) {
                track_matches.insert(source_id.clone(), uri.clone());
            }
        } else if selected.uri.starts_with("spotify:track:") {
            track_matches.insert(source_id.clone(), selected.uri.clone());
        }
    }
    // Keep the list bounded even if a future provider adapter returns more.
    candidates.truncate(10);
    MatchResult {
        source_id,
        search_term,
        confidence,
        selected_uri,
        candidates,
        track_matches,
    }
}

pub(super) fn match_result_for_release(
    source: &SourceRow,
    search_term: String,
    candidates: Vec<AlbumCandidate>,
    selected_uri: Option<&str>,
) -> MatchResult {
    let title = without_known_source_suffix(&source.track, &source.artist, &source.album);
    let mut result = match_result_for(
        source.stable_id.clone(),
        search_term,
        candidates,
        &title,
        selected_uri,
    );
    let selected = result
        .selected_uri
        .as_deref()
        .and_then(|uri| {
            result
                .candidates
                .iter()
                .find(|candidate| candidate.uri == uri)
        })
        .cloned();
    if let Some(selected) = selected {
        update_selected_release_match(&mut result, source, &selected);
    }
    result
}

pub(super) fn preserve_match_selection(
    mut result: MatchResult,
    previous: Option<&MatchResult>,
    source_id: &str,
    source_track: &str,
) -> MatchResult {
    let Some(previous) = previous else {
        return result;
    };
    if previous.selected_uri.is_none() && previous.track_matches.is_empty() {
        return result;
    }
    result.selected_uri = previous.selected_uri.clone();
    result.confidence = previous.confidence;
    result.track_matches = previous.track_matches.clone();
    let preserved = previous
        .candidates
        .iter()
        .filter(|candidate| {
            previous
                .selected_uri
                .as_deref()
                .is_some_and(|uri| uri == candidate.uri)
                || previous.track_matches.get(source_id).is_some_and(|uri| {
                    candidate
                        .track_uris
                        .iter()
                        .any(|track_uri| track_uri == uri)
                })
        })
        .cloned()
        .collect::<Vec<_>>();
    for candidate in preserved {
        if !result
            .candidates
            .iter()
            .any(|existing| existing.uri == candidate.uri)
        {
            result.candidates.insert(0, candidate);
        }
    }
    for candidate in result
        .candidates
        .iter_mut()
        .filter(|candidate| candidate.uri.starts_with("spotify:track:"))
    {
        classify_album_candidates_by_name(
            &[source_track.to_owned()],
            std::slice::from_mut(candidate),
        );
    }
    result.candidates.truncate(10);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_album_selection_prefers_coverage_then_tightest_release() {
        let candidate = |uri: &str, tracks: &[&str]| AlbumCandidate {
            uri: uri.into(),
            name: "Closer".into(),
            track_names: tracks.iter().map(|track| (*track).into()).collect(),
            ..AlbumCandidate::default()
        };

        let source = ["One", "Two", "Three", "Four", "Five"]
            .map(str::to_owned)
            .to_vec();
        let mut candidates = vec![
            candidate("four-of-five", &["One", "Two", "Three", "Four"]),
            candidate(
                "all-five",
                &["One", "Two", "Three", "Four", "Five", "Bonus"],
            ),
        ];
        classify_album_candidates_by_name(&source, &mut candidates);
        assert_eq!(
            automatic_album_candidate("Closer", &source, &candidates)
                .map(|album| album.uri.as_str()),
            Some("all-five")
        );

        let source = ["Briefly", "Rolling"].map(str::to_owned).to_vec();
        let mut candidates = vec![
            candidate("eleven-track", &["Briefly", "Rolling", "Extra"]),
            candidate("thirteen-track", &["Briefly", "Rolling", "Extra", "Bonus"]),
        ];
        classify_album_candidates_by_name(&source, &mut candidates);
        assert_eq!(
            automatic_album_candidate("Closer", &source, &candidates)
                .map(|album| album.uri.as_str()),
            Some("eleven-track")
        );
    }

    #[test]
    fn release_matching_uses_known_suffixes_and_unique_significant_tokens() {
        let row = |track: &str| SourceRow {
            stable_id: source_id("James Horner", "Back To Titanic", track),
            artist: "James Horner".into(),
            album: "Back To Titanic".into(),
            track: track.into(),
            variants: Vec::new(),
            play_count: 1,
            earliest: 1,
            latest: 1,
        };
        let rows = vec![
            row("The Portrait – James Horner"),
            row("Jack Dawson's Luck – Back To Titanic"),
            row("A Building Panic – Back To Titanic"),
        ];
        let mut candidates = vec![AlbumCandidate {
            uri: "spotify:album:titanic".into(),
            name: "Back To Titanic – More Music from the Motion Picture".into(),
            artist: "James Horner".into(),
            track_uris: vec![
                "spotify:track:portrait".into(),
                "spotify:track:luck".into(),
                "spotify:track:panic".into(),
            ],
            track_names: vec![
                "The Portrait – From 'Titanic' Soundtrack".into(),
                "Jack Dawson's Luck (includes 'John Ryan's Polka') – From 'Titanic' Soundtrack"
                    .into(),
                "A Building Panic (Album Suite) – From 'Titanic' Soundtrack".into(),
            ],
            track_artists: vec!["James Horner".into(); 3],
            track_albums: vec!["Back To Titanic".into(); 3],
            ..AlbumCandidate::default()
        }];

        classify_album_candidates_for_rows(&rows, &mut candidates);
        assert_eq!(candidates[0].relation, Some(AlbumRelation::BestMatch));
        assert_eq!(
            automatic_album_candidate_for_rows("Back To Titanic", &rows, &candidates)
                .map(|candidate| candidate.uri.as_str()),
            Some("spotify:album:titanic")
        );
        for (row, expected) in rows.iter().zip([
            "spotify:track:portrait",
            "spotify:track:luck",
            "spotify:track:panic",
        ]) {
            let result = match_result_for_release(
                row,
                "album search".into(),
                candidates.clone(),
                Some("spotify:album:titanic"),
            );
            assert_eq!(
                result.track_matches.get(&row.stable_id).map(String::as_str),
                Some(expected)
            );
        }

        let classical = SourceRow {
            track: "Un pochettino meno adagio – Vivacissimo – Adagio –".into(),
            ..row("placeholder")
        };
        assert_eq!(
            release_track_match_index(
                &classical,
                &[
                    "Symphony No. 7 in C Major, Op. 105: II. Vivacissimo – Adagio – Largamente molto"
                        .into(),
                    "Symphony No. 5: Adagio".into(),
                ],
            ),
            Some(0)
        );

        let generic = SourceRow {
            artist: "John Williams".into(),
            album: "Greatest Hits".into(),
            track: "Main Theme".into(),
            ..row("placeholder")
        };
        assert_eq!(
            release_track_match_index(
                &generic,
                &[
                    "Main Theme (From 'Jurassic Park')".into(),
                    "Main Theme (From 'Schindler's List')".into(),
                ],
            ),
            None
        );
    }

    #[test]
    fn album_search_ignores_parenthetical_annotations() {
        assert_eq!(
            album_search_term(
                "John Williams",
                "Jurassic Park (Original Motion Picture Soundtrack)",
            ),
            "album:\"Jurassic Park\" artist:\"John Williams\""
        );
        assert_eq!(
            album_track_match_index("Harry & Hermoine", &["Harry & Hermione".into()]),
            Some(0)
        );
        assert_eq!(
            selected_match_confidence(
                "Harry & Hermoine",
                &AlbumCandidate {
                    track_names: vec!["Harry & Hermione".into()],
                    ..AlbumCandidate::default()
                },
            ),
            Confidence::Likely
        );
        assert_eq!(
            album_track_match_index(
                "Raise Your Banner (feat. Anders Fridén) [Single Edit]",
                &[
                    "Raise Your Banner".into(),
                    "Raise Your Banner - Single Edit".into(),
                    "Raise Your Banner - Instrumental".into(),
                ],
            ),
            Some(1)
        );
    }
}

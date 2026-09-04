use super::*;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SourceGroup {
    pub(super) artist: String,
    pub(super) album: String,
    pub(super) row_indices: Vec<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SourceCluster {
    group_indices: Vec<usize>,
    row_indices: Vec<usize>,
    collection_shaped: bool,
    representative_artist: String,
    representative_album: String,
    album_labels: Vec<String>,
}

pub(super) fn source_album_support(left: &str, right: &str) -> bool {
    let left_compact = normalize_for_match(left);
    let right_compact = normalize_for_match(right);
    if left_compact.is_empty() || right_compact.is_empty() {
        return false;
    }
    if left_compact == right_compact {
        return true;
    }
    let left_numbers = numbered_title_tokens(left);
    let right_numbers = numbered_title_tokens(right);
    if !left_numbers.is_empty() && !right_numbers.is_empty() && left_numbers != right_numbers {
        return false;
    }
    if has_series_number(left) != has_series_number(right) {
        let (numbered, unnumbered) = if has_series_number(left) {
            (left, right)
        } else {
            (right, left)
        };
        if !significant_title_tokens(unnumbered).is_subset(&significant_title_tokens(numbered)) {
            return false;
        }
    }
    let left_words = normalized_word_sequences(left);
    let right_words = normalized_word_sequences(right);
    if left_words == right_words || titles_share_contained_words(left, right) {
        return true;
    }
    let left_tokens = significant_title_tokens(left);
    let right_tokens = significant_title_tokens(right);
    let shorter = left_tokens.len().min(right_tokens.len());
    shorter > 0 && left_tokens.intersection(&right_tokens).count() * 5 >= shorter * 4
}

fn numbered_title_tokens(value: &str) -> BTreeSet<String> {
    let words = normalized_word_sequences(value);
    words
        .iter()
        .enumerate()
        .filter_map(|(index, word)| {
            if word.chars().all(|character| character.is_ascii_digit()) {
                return word.parse::<u64>().ok().map(|number| number.to_string());
            }
            index
                .checked_sub(1)
                .and_then(|previous| series_number_marker(&words[previous]).then_some(()))
                .and_then(|_| small_roman_numeral(word))
                .map(|number| number.to_string())
        })
        .collect()
}

fn series_number_marker(word: &str) -> bool {
    matches!(
        word,
        "episode"
            | "part"
            | "volume"
            | "vol"
            | "chapter"
            | "book"
            | "season"
            | "symphony"
            | "disc"
            | "disk"
    )
}

fn has_series_number(value: &str) -> bool {
    normalized_word_sequences(value).windows(2).any(|pair| {
        series_number_marker(&pair[0])
            && (pair[1].chars().all(|character| character.is_ascii_digit())
                || small_roman_numeral(&pair[1]).is_some())
    })
}

fn small_roman_numeral(word: &str) -> Option<u8> {
    [
        "i", "ii", "iii", "iv", "v", "vi", "vii", "viii", "ix", "x", "xi", "xii", "xiii", "xiv",
        "xv", "xvi", "xvii", "xviii", "xix", "xx",
    ]
    .iter()
    .position(|candidate| candidate == &word)
    .map(|index| index as u8 + 1)
}

pub(super) fn source_artists_compatible(left: &str, right: &str) -> bool {
    let left_compact = normalize_catalog_text(left);
    let right_compact = normalize_catalog_text(right);
    !left_compact.is_empty()
        && !right_compact.is_empty()
        && (left_compact == right_compact || titles_share_contained_words(left, right))
}

pub(super) fn source_track_match_count(
    left: &SourceGroup,
    right: &SourceGroup,
    rows: &[SourceRow],
) -> usize {
    let (source, target) = match left.row_indices.len().cmp(&right.row_indices.len()) {
        std::cmp::Ordering::Less => (left, right),
        std::cmp::Ordering::Greater => (right, left),
        std::cmp::Ordering::Equal => {
            if source_group_sort_key(left) <= source_group_sort_key(right) {
                (left, right)
            } else {
                (right, left)
            }
        }
    };
    let targets = target
        .row_indices
        .iter()
        .map(|index| rows[*index].track.clone())
        .collect::<Vec<_>>();
    source
        .row_indices
        .iter()
        .filter_map(|index| release_track_match_index(&rows[*index], &targets))
        .collect::<BTreeSet<_>>()
        .len()
}

fn source_groups_support(left: &SourceGroup, right: &SourceGroup, rows: &[SourceRow]) -> bool {
    let album_support = source_album_support(&left.album, &right.album);
    let artist_support = source_artists_compatible(&left.artist, &right.artist);
    if artist_support && album_support {
        return true;
    }
    if !artist_support && !album_support {
        return false;
    }
    let matches = source_track_match_count(left, right, rows);
    let smaller = left.row_indices.len().min(right.row_indices.len());
    let track_support = matches * 5 >= smaller * 4;
    if artist_support {
        matches >= 2 && track_support
    } else {
        matches >= 3 && track_support
    }
}

fn source_group_plays(group: &SourceGroup, rows: &[SourceRow]) -> u64 {
    group
        .row_indices
        .iter()
        .map(|index| rows[*index].play_count)
        .fold(0, u64::saturating_add)
}

fn source_group_sort_key(group: &SourceGroup) -> (&str, &str) {
    (group.artist.as_str(), group.album.as_str())
}

#[derive(Default)]
struct SourceCandidateIndex {
    artist_buckets: BTreeMap<String, Vec<usize>>,
    track_buckets: BTreeMap<String, Vec<usize>>,
    artist_keys: Vec<Vec<String>>,
    artist_query_keys: Vec<Vec<String>>,
    album_keys: Vec<Vec<String>>,
    album_query_keys: Vec<Vec<String>>,
    track_keys: Vec<Vec<String>>,
    track_query_keys: Vec<Vec<String>>,
    typo_availability: SourceTypoAvailability,
}

#[derive(Default)]
struct SourceTypoAvailability {
    full: HashSet<(String, usize)>,
    deleted: HashSet<(String, usize)>,
}

impl SourceCandidateIndex {
    fn new(groups: &[SourceGroup], named: &[usize], rows: &[SourceRow]) -> Self {
        let typo_availability = source_typo_availability(groups, named, rows);
        let mut index = Self {
            artist_keys: vec![Vec::new(); groups.len()],
            artist_query_keys: vec![Vec::new(); groups.len()],
            album_keys: vec![Vec::new(); groups.len()],
            album_query_keys: vec![Vec::new(); groups.len()],
            track_keys: vec![Vec::new(); groups.len()],
            track_query_keys: vec![Vec::new(); groups.len()],
            ..Self::default()
        };
        for &group_index in named {
            let group = &groups[group_index];
            let (artist_keys, artist_query_keys) =
                source_artist_index_data(&group.artist, &typo_availability);
            let (album_keys, album_query_keys) =
                source_album_index_data(&group.album, &typo_availability);
            let (track_keys, track_query_keys) =
                source_group_track_index_data(group, rows, &typo_availability);
            for key in &artist_keys {
                index
                    .artist_buckets
                    .entry(key.clone())
                    .or_default()
                    .push(group_index);
            }
            if group.row_indices.len() >= 3 {
                for key in &track_keys {
                    index
                        .track_buckets
                        .entry(key.clone())
                        .or_default()
                        .push(group_index);
                }
            }
            index.artist_keys[group_index] = artist_keys;
            index.artist_query_keys[group_index] = artist_query_keys;
            index.album_keys[group_index] = album_keys;
            index.album_query_keys[group_index] = album_query_keys;
            index.track_keys[group_index] = track_keys;
            index.track_query_keys[group_index] = track_query_keys;
        }
        index.typo_availability = typo_availability;
        index
    }

    fn artist_candidates(
        &self,
        artist: &str,
        candidate_bucket_visits: &mut usize,
    ) -> BTreeSet<usize> {
        let mut candidates = BTreeSet::new();
        let (_, query_keys) = source_artist_index_data(artist, &self.typo_availability);
        for key in query_keys {
            if let Some(bucket) = self.artist_buckets.get(&key) {
                *candidate_bucket_visits = candidate_bucket_visits.saturating_add(bucket.len());
                candidates.extend(bucket.iter().copied());
            }
        }
        candidates
    }

    fn group_neighbors(
        &self,
        group_index: usize,
        groups: &[SourceGroup],
        candidate_bucket_visits: &mut usize,
    ) -> BTreeSet<usize> {
        let mut neighbors = self
            .artist_candidates(&groups[group_index].artist, candidate_bucket_visits)
            .into_iter()
            .filter(|candidate| {
                source_artists_compatible(&groups[group_index].artist, &groups[*candidate].artist)
                    && (source_index_keys_intersect(
                        &self.album_query_keys[group_index],
                        &self.album_keys[*candidate],
                    ) || source_index_keys_intersect(
                        &self.track_query_keys[group_index],
                        &self.track_keys[*candidate],
                    ))
            })
            .collect::<BTreeSet<_>>();
        if groups[group_index].row_indices.len() >= 3 {
            for key in &self.track_query_keys[group_index] {
                let Some(bucket) = self.track_buckets.get(key) else {
                    continue;
                };
                *candidate_bucket_visits = candidate_bucket_visits.saturating_add(bucket.len());
                neighbors.extend(bucket.iter().copied().filter(|candidate| {
                    !source_artists_compatible(
                        &groups[group_index].artist,
                        &groups[*candidate].artist,
                    ) && source_index_keys_intersect(
                        &self.album_query_keys[group_index],
                        &self.album_keys[*candidate],
                    )
                }));
            }
        }
        neighbors.remove(&group_index);
        neighbors
    }
}

fn source_index_keys_intersect(query: &[String], indexed: &[String]) -> bool {
    query.iter().any(|key| indexed.binary_search(key).is_ok())
}

fn source_typo_availability(
    groups: &[SourceGroup],
    named: &[usize],
    rows: &[SourceRow],
) -> SourceTypoAvailability {
    let mut availability = SourceTypoAvailability::default();
    for &group_index in named {
        source_add_typo_availability(&mut availability, &groups[group_index].artist);
        source_add_typo_availability(&mut availability, &groups[group_index].album);
        for &row_index in &groups[group_index].row_indices {
            let row = &rows[row_index];
            source_add_typo_availability(&mut availability, &row.track);
            let title = without_known_source_suffix(&row.track, &row.artist, &row.album);
            if title != row.track {
                source_add_typo_availability(&mut availability, &title);
            }
        }
    }
    availability
}

fn source_add_typo_availability(availability: &mut SourceTypoAvailability, title: &str) {
    for token in normalized_word_sequences(title)
        .into_iter()
        .collect::<BTreeSet<_>>()
    {
        let characters = token.chars().collect::<Vec<_>>();
        if characters.len() < 5 {
            continue;
        }
        let length = characters.len();
        availability.full.insert((token.clone(), length));
        for index in 0..characters.len() {
            let mut deletion = String::with_capacity(token.len());
            for (position, character) in characters.iter().enumerate() {
                if position != index {
                    deletion.push(*character);
                }
            }
            availability.deleted.insert((deletion, length));
        }
    }
}

fn source_typo_token_has_neighbor(token: &str, availability: &SourceTypoAvailability) -> bool {
    let characters = token.chars().collect::<Vec<_>>();
    let length = characters.len();
    if length < 5 {
        return false;
    }
    if availability
        .deleted
        .contains(&(token.to_owned(), length + 1))
    {
        return true;
    }
    if length > 5 {
        for index in 0..characters.len() {
            let mut deletion = String::with_capacity(token.len());
            for (position, character) in characters.iter().enumerate() {
                if position != index {
                    deletion.push(*character);
                }
            }
            if availability.full.contains(&(deletion, length - 1)) {
                return true;
            }
        }
    }
    for index in 0..characters.len().saturating_sub(1) {
        let mut swapped = characters.clone();
        swapped.swap(index, index + 1);
        if swapped != characters
            && availability
                .full
                .contains(&(swapped.into_iter().collect(), length))
        {
            return true;
        }
    }
    false
}

fn source_index_pair_keys(prefix: &str, tokens: &BTreeSet<String>) -> BTreeSet<String> {
    let tokens = tokens.iter().collect::<Vec<_>>();
    let mut keys = BTreeSet::new();
    for left in 0..tokens.len() {
        for right in (left + 1)..tokens.len() {
            keys.insert(format!("{prefix}:{}|{}", tokens[left], tokens[right]));
        }
    }
    keys
}

fn source_index_typo_index_keys(
    tokens: &BTreeSet<String>,
    availability: &SourceTypoAvailability,
) -> BTreeSet<String> {
    let tokens = tokens.iter().collect::<Vec<_>>();
    let mut keys = BTreeSet::new();
    for typo_index in 0..tokens.len() {
        let typo = tokens[typo_index];
        let characters = typo.chars().collect::<Vec<_>>();
        if !source_typo_token_has_neighbor(typo, availability) {
            continue;
        }
        let context = tokens
            .iter()
            .enumerate()
            .filter_map(|(index, token)| (index != typo_index).then_some(token.as_str()))
            .collect::<Vec<_>>()
            .join("|");
        let length = characters.len();
        keys.insert(format!("typo-token:{typo}|len:{length}|context:{context}"));
        for index in 0..characters.len() {
            let mut deletion = String::with_capacity(typo.len());
            for (position, character) in characters.iter().enumerate() {
                if position != index {
                    deletion.push(*character);
                }
            }
            keys.insert(format!(
                "typo-delete:{deletion}|len:{length}|context:{context}"
            ));
        }
    }
    keys
}

fn source_index_typo_query_keys(
    tokens: &BTreeSet<String>,
    availability: &SourceTypoAvailability,
) -> BTreeSet<String> {
    let tokens = tokens.iter().collect::<Vec<_>>();
    let mut keys = BTreeSet::new();
    for typo_index in 0..tokens.len() {
        let typo = tokens[typo_index];
        let characters = typo.chars().collect::<Vec<_>>();
        if !source_typo_token_has_neighbor(typo, availability) {
            continue;
        }
        let context = tokens
            .iter()
            .enumerate()
            .filter_map(|(index, token)| (index != typo_index).then_some(token.as_str()))
            .collect::<Vec<_>>()
            .join("|");
        let length = characters.len();
        keys.insert(format!(
            "typo-delete:{typo}|len:{}|context:{context}",
            length + 1
        ));
        if length > 5 {
            for index in 0..characters.len() {
                let mut deletion = String::with_capacity(typo.len());
                for (position, character) in characters.iter().enumerate() {
                    if position != index {
                        deletion.push(*character);
                    }
                }
                keys.insert(format!(
                    "typo-token:{deletion}|len:{}|context:{context}",
                    length - 1
                ));
            }
        }
        for index in 0..characters.len().saturating_sub(1) {
            let mut swapped = characters.clone();
            swapped.swap(index, index + 1);
            if swapped == characters {
                continue;
            }
            let swapped = swapped.into_iter().collect::<String>();
            keys.insert(format!(
                "typo-token:{swapped}|len:{length}|context:{context}"
            ));
        }
    }
    keys
}

fn source_title_index_data(
    title: &str,
    typo_availability: &SourceTypoAvailability,
) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut index_keys = BTreeSet::new();
    let mut query_keys = BTreeSet::new();
    let words = normalized_word_sequences(title)
        .into_iter()
        .collect::<BTreeSet<_>>();
    if words.len() == 1 {
        index_keys.extend(words.iter().map(|word| format!("word-single:{word}")));
        query_keys.extend(words.iter().map(|word| format!("word-single:{word}")));
        query_keys.extend(words.iter().map(|word| format!("word-contains:{word}")));
    } else if words.len() >= 2 {
        index_keys.extend(words.iter().map(|word| format!("word-contains:{word}")));
        index_keys.extend(source_index_pair_keys("word-pair", &words));
        query_keys.extend(source_index_pair_keys("word-pair", &words));
    }

    let significant = significant_title_tokens(title);
    if significant.len() == 1 {
        index_keys.extend(
            significant
                .iter()
                .map(|token| format!("significant-single:{token}")),
        );
        query_keys.extend(
            significant
                .iter()
                .map(|token| format!("significant-single:{token}")),
        );
        query_keys.extend(
            significant
                .iter()
                .map(|token| format!("significant-contains:{token}")),
        );
    } else if significant.len() >= 2 {
        index_keys.extend(
            significant
                .iter()
                .map(|token| format!("significant-contains:{token}")),
        );
        index_keys.extend(source_index_pair_keys("significant-pair", &significant));
        query_keys.extend(source_index_pair_keys("significant-pair", &significant));
    }

    index_keys.extend(source_index_typo_index_keys(&words, typo_availability));
    query_keys.extend(source_index_typo_query_keys(&words, typo_availability));
    (index_keys, query_keys)
}

fn source_artist_index_data(
    artist: &str,
    typo_availability: &SourceTypoAvailability,
) -> (Vec<String>, Vec<String>) {
    let (mut index_keys, mut query_keys) = source_title_index_data(artist, typo_availability);
    let compact = normalize_catalog_text(artist);
    if !compact.is_empty() {
        let key = format!("compact:{compact}");
        index_keys.insert(key.clone());
        query_keys.insert(key);
    }
    (
        index_keys.into_iter().collect(),
        query_keys.into_iter().collect(),
    )
}

fn source_album_index_data(
    album: &str,
    typo_availability: &SourceTypoAvailability,
) -> (Vec<String>, Vec<String>) {
    let (mut index_keys, mut query_keys) = source_title_index_data(album, typo_availability);
    let numbers = numbered_title_tokens(album);
    if !numbers.is_empty() {
        let suffix = numbers.into_iter().collect::<Vec<_>>().join("|");
        index_keys.extend(
            index_keys
                .iter()
                .map(|key| format!("{key}|numbers:{suffix}"))
                .collect::<Vec<_>>(),
        );
        query_keys = query_keys
            .into_iter()
            .map(|key| format!("{key}|numbers:{suffix}"))
            .collect();
    }
    let compact = normalize_for_match(album);
    if !compact.is_empty() {
        let key = format!("compact:{compact}");
        index_keys.insert(key.clone());
        query_keys.insert(key);
    }
    (
        index_keys.into_iter().collect(),
        query_keys.into_iter().collect(),
    )
}

fn source_track_index_data(
    row: &SourceRow,
    typo_availability: &SourceTypoAvailability,
) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut index_keys = BTreeSet::new();
    let mut query_keys = BTreeSet::new();
    let mut titles = vec![row.track.clone()];
    let title = without_known_source_suffix(&row.track, &row.artist, &row.album);
    if title != row.track {
        titles.push(title);
    }
    for title in titles {
        let (title_index_keys, title_query_keys) =
            source_title_index_data(&title, typo_availability);
        index_keys.extend(title_index_keys);
        query_keys.extend(title_query_keys);
        let compact = normalize_catalog_text(&title);
        if !compact.is_empty() {
            let key = format!("compact:{compact}");
            index_keys.insert(key.clone());
            query_keys.insert(key);
        }
    }
    (index_keys, query_keys)
}

fn source_group_track_index_data(
    group: &SourceGroup,
    rows: &[SourceRow],
    typo_availability: &SourceTypoAvailability,
) -> (Vec<String>, Vec<String>) {
    let (index_keys, query_keys) = group
        .row_indices
        .iter()
        .map(|index| source_track_index_data(&rows[*index], typo_availability))
        .fold(
            (BTreeSet::new(), BTreeSet::new()),
            |(mut index_keys, mut query_keys), (row_index_keys, row_query_keys)| {
                index_keys.extend(row_index_keys);
                query_keys.extend(row_query_keys);
                (index_keys, query_keys)
            },
        );
    (
        index_keys.into_iter().collect(),
        query_keys.into_iter().collect(),
    )
}

fn source_cluster_rows(cluster: &SourceCluster, groups: &[SourceGroup]) -> Vec<usize> {
    let mut indices = cluster
        .group_indices
        .iter()
        .flat_map(|index| groups[*index].row_indices.iter().copied())
        .collect::<Vec<_>>();
    indices.sort_unstable();
    indices
}

fn source_row_matches_cluster(
    row: &SourceRow,
    cluster: &SourceCluster,
    groups: &[SourceGroup],
    rows: &[SourceRow],
) -> bool {
    let targets = cluster
        .group_indices
        .iter()
        .flat_map(|index| groups[*index].row_indices.iter())
        .map(|index| rows[*index].track.clone())
        .collect::<Vec<_>>();
    release_track_match_index(row, &targets).is_some()
}

fn source_cluster_for_empty_row(
    row: &SourceRow,
    artist_candidates: &BTreeSet<usize>,
    cluster_for_group: &[usize],
    clusters: &[SourceCluster],
    groups: &[SourceGroup],
    rows: &[SourceRow],
) -> Option<usize> {
    let mut matches = BTreeSet::new();
    for group_index in artist_candidates {
        if !source_artists_compatible(&row.artist, &groups[*group_index].artist) {
            continue;
        }
        let cluster_index = cluster_for_group[*group_index];
        if source_row_matches_cluster(row, &clusters[cluster_index], groups, rows) {
            matches.insert(cluster_index);
        }
    }
    (matches.len() == 1).then(|| *matches.first().expect("one match was inserted"))
}

fn source_clusters(rows: &[SourceRow]) -> Vec<SourceCluster> {
    source_clusters_with_comparison_count(rows).0
}

pub(super) fn source_clusters_with_comparison_count(
    rows: &[SourceRow],
) -> (Vec<SourceCluster>, usize) {
    let (clusters, comparisons, _) = source_clusters_with_stats(rows);
    (clusters, comparisons)
}

pub(super) fn source_clusters_with_stats(rows: &[SourceRow]) -> (Vec<SourceCluster>, usize, usize) {
    let mut grouped = BTreeMap::<(String, String), Vec<usize>>::new();
    for (index, row) in rows.iter().enumerate() {
        grouped
            .entry((row.artist.clone(), row.album.clone()))
            .or_default()
            .push(index);
    }
    let mut groups = grouped
        .into_iter()
        .map(|((artist, album), row_indices)| SourceGroup {
            artist,
            album,
            row_indices,
        })
        .collect::<Vec<_>>();
    let named = groups
        .iter()
        .enumerate()
        .filter_map(|(index, group)| (!group.album.is_empty()).then_some(index))
        .collect::<Vec<_>>();
    let index = SourceCandidateIndex::new(&groups, &named, rows);
    let mut candidate_bucket_visits = 0;
    let mut candidate_neighbors = vec![BTreeSet::new(); groups.len()];
    for &group_index in &named {
        for neighbor in index.group_neighbors(group_index, &groups, &mut candidate_bucket_visits) {
            candidate_neighbors[group_index].insert(neighbor);
            candidate_neighbors[neighbor].insert(group_index);
        }
    }
    let initial_clusters = named
        .iter()
        .copied()
        .map(|index| SourceCluster {
            group_indices: vec![index],
            row_indices: Vec::new(),
            collection_shaped: false,
            representative_artist: String::new(),
            representative_album: String::new(),
            album_labels: Vec::new(),
        })
        .collect::<Vec<_>>();
    let mut active = initial_clusters.into_iter().map(Some).collect::<Vec<_>>();
    let mut cluster_for_group = vec![usize::MAX; groups.len()];
    for (cluster_index, group_index) in groups
        .iter()
        .enumerate()
        .filter_map(|(index, group)| (!group.album.is_empty()).then_some(index))
        .enumerate()
    {
        cluster_for_group[group_index] = cluster_index;
    }

    let mut comparisons = 0;
    let mut support_cache = HashMap::new();
    for left in 0..active.len() {
        while active[left].is_some() {
            let candidate_clusters = active[left]
                .as_ref()
                .expect("left cluster is active")
                .group_indices
                .iter()
                .flat_map(|group_index| candidate_neighbors[*group_index].iter().copied())
                .filter_map(|group_index| {
                    let cluster_index = cluster_for_group[group_index];
                    (cluster_index > left && active[cluster_index].is_some())
                        .then_some(cluster_index)
                })
                .collect::<BTreeSet<_>>();
            let mut merged = false;
            for right in candidate_clusters {
                let can_merge = {
                    let left_cluster = active[left].as_ref().expect("left cluster is active");
                    let right_cluster = active[right].as_ref().expect("right cluster is active");
                    left_cluster.group_indices.iter().all(|left_group| {
                        right_cluster.group_indices.iter().all(|right_group| {
                            let pair = if left_group < right_group {
                                (*left_group, *right_group)
                            } else {
                                (*right_group, *left_group)
                            };
                            *support_cache.entry(pair).or_insert_with(|| {
                                comparisons += 1;
                                source_groups_support(
                                    &groups[*left_group],
                                    &groups[*right_group],
                                    rows,
                                )
                            })
                        })
                    })
                };
                if can_merge {
                    let right_groups = active[right]
                        .take()
                        .expect("right cluster is active")
                        .group_indices;
                    let left_cluster = active[left].as_mut().expect("left cluster is active");
                    left_cluster
                        .group_indices
                        .extend(right_groups.iter().copied());
                    left_cluster.group_indices.sort_unstable();
                    for group_index in right_groups {
                        cluster_for_group[group_index] = left;
                    }
                    merged = true;
                    break;
                }
            }
            if !merged {
                break;
            }
        }
    }

    let mut stable_to_compact = vec![usize::MAX; active.len()];
    let mut clusters = Vec::new();
    for (stable_index, cluster) in active.into_iter().enumerate() {
        let Some(cluster) = cluster else {
            continue;
        };
        stable_to_compact[stable_index] = clusters.len();
        clusters.push(cluster);
    }
    for stable_index in &mut cluster_for_group {
        if *stable_index != usize::MAX {
            *stable_index = stable_to_compact[*stable_index];
        }
    }

    let empty_group_indices = groups
        .iter()
        .enumerate()
        .filter_map(|(index, group)| group.album.is_empty().then_some(index))
        .collect::<Vec<_>>();
    for empty_group_index in empty_group_indices {
        let group = groups[empty_group_index].clone();
        let artist_candidates =
            index.artist_candidates(&group.artist, &mut candidate_bucket_visits);
        let mut residual = Vec::new();
        for row_index in group.row_indices {
            let Some(cluster_index) = source_cluster_for_empty_row(
                &rows[row_index],
                &artist_candidates,
                &cluster_for_group,
                &clusters,
                &groups,
                rows,
            ) else {
                residual.push(row_index);
                continue;
            };
            let attached_group_index = groups.len();
            groups.push(SourceGroup {
                artist: group.artist.clone(),
                album: String::new(),
                row_indices: vec![row_index],
            });
            clusters[cluster_index]
                .group_indices
                .push(attached_group_index);
            clusters[cluster_index].group_indices.sort_unstable();
            clusters[cluster_index].collection_shaped = true;
        }
        if !residual.is_empty() {
            let residual_group_index = groups.len();
            groups.push(SourceGroup {
                artist: group.artist,
                album: String::new(),
                row_indices: residual,
            });
            clusters.push(SourceCluster {
                group_indices: vec![residual_group_index],
                row_indices: Vec::new(),
                collection_shaped: true,
                representative_artist: String::new(),
                representative_album: String::new(),
                album_labels: Vec::new(),
            });
        }
    }
    for cluster in &mut clusters {
        cluster.row_indices = source_cluster_rows(cluster, &groups);
        cluster.collection_shaped |= cluster.group_indices.len() > 1;
        let has_named_group = cluster
            .group_indices
            .iter()
            .any(|index| !groups[*index].album.is_empty());
        let representative = cluster
            .group_indices
            .iter()
            .map(|index| &groups[*index])
            .filter(|group| group.album.is_empty() != has_named_group)
            .max_by(|left, right| {
                source_group_plays(left, rows)
                    .cmp(&source_group_plays(right, rows))
                    .then_with(|| source_group_sort_key(left).cmp(&source_group_sort_key(right)))
            })
            .expect("source clusters always contain a group");
        cluster.representative_artist = representative.artist.clone();
        cluster.representative_album = representative.album.clone();
        cluster.album_labels = cluster
            .group_indices
            .iter()
            .map(|index| groups[*index].album.clone())
            .filter(|album| !album.is_empty())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
    }
    (clusters, comparisons, candidate_bucket_visits)
}

pub(super) fn build_review_batches(rows: &[SourceRow]) -> Vec<ImportBatch> {
    let mut page = 1;
    let mut batches = Vec::new();
    for cluster in source_clusters(rows) {
        let source_ids = cluster
            .row_indices
            .iter()
            .map(|index| rows[*index].stable_id.clone())
            .collect::<Vec<_>>();
        for chunk in source_ids.chunks(LASTFM_REVIEW_BATCH_SIZE) {
            batches.push(ImportBatch {
                page,
                source_ids: chunk.to_vec(),
                custom: false,
                collection_shaped: Some(cluster.collection_shaped),
                representative_artist: Some(cluster.representative_artist.clone()),
                representative_album: Some(cluster.representative_album.clone()),
                album_labels: cluster.album_labels.clone(),
            });
            page += 1;
        }
    }
    batches
}
#[cfg(test)]
mod tests {
    use super::*;

    fn scrobble(artist: &str, album: &str, track: &str, timestamp: u64) -> ParsedScrobble {
        ParsedScrobble {
            artist: artist.into(),
            album: album.into(),
            track: track.into(),
            timestamp,
        }
    }

    fn batches_with_rows(rows: &[SourceRow]) -> Vec<(ImportBatch, Vec<&SourceRow>)> {
        let rows_by_id = rows
            .iter()
            .map(|row| (row.stable_id.as_str(), row))
            .collect::<HashMap<_, _>>();
        build_review_batches(rows)
            .into_iter()
            .map(|batch| {
                let batch_rows = batch_rows(&batch, &rows_by_id);
                (batch, batch_rows)
            })
            .collect()
    }

    #[test]
    fn source_batches_merge_album_aliases_with_the_highest_play_group_label() {
        let mut rows = Vec::new();
        aggregate_scrobbles(
            &mut rows,
            &[
                scrobble("John Williams", "Greatest Hits", "Theme One", 1),
                scrobble("John Williams", "Greatest Hits", "Theme One", 2),
                scrobble(
                    "John Williams",
                    "Greatest Hits: The Best Of John Williams",
                    "Theme One",
                    3,
                ),
                scrobble(
                    "John Williams",
                    "John Williams - Greatest Hits",
                    "Theme One",
                    4,
                ),
            ],
        );

        let batches = batches_with_rows(&rows);

        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].0.collection_shaped, Some(true));
        assert_eq!(
            batches[0].0.representative_artist.as_deref(),
            Some("John Williams")
        );
        assert_eq!(
            batches[0].0.representative_album.as_deref(),
            Some("Greatest Hits")
        );
        assert_eq!(batches[0].0.album_labels.len(), 3);
        assert_eq!(batches[0].1.len(), 3);
    }

    #[test]
    fn source_album_support_keeps_numbered_series_entries_separate() {
        assert!(source_album_support(
            "Star Wars Episode I",
            "Star Wars Episode 1"
        ));
        assert!(!source_album_support(
            "Star Wars Episode IV",
            "Star Wars Episode 1"
        ));
        assert!(source_album_support(
            "Star Wars Episode V: The Empire Strikes Back",
            "Star Wars: The Empire Strikes Back"
        ));
        assert!(!source_album_support(
            "Star Wars Episode I: The Phantom Menace",
            "Star Wars: A New Hope"
        ));
        assert!(!source_album_support(
            "A State of Trance Episode 652",
            "A State of Trance Episode 749"
        ));
        assert!(source_album_support(
            "A State of Trance Episode 652",
            "ASOT 652 - A State of Trance Episode 652"
        ));
        assert!(source_album_support(
            "Greatest Hits 1969-1999",
            "Greatest Hits: 1969–1999"
        ));
    }

    #[test]
    fn source_batches_require_pairwise_support_and_cross_artist_track_thresholds() {
        let mut rows = Vec::new();
        aggregate_scrobbles(
            &mut rows,
            &[
                scrobble("Artist A", "Shared Release", "One", 1),
                scrobble("Artist A", "Shared Release", "Two", 2),
                scrobble("Artist A", "Shared Release", "Three", 3),
                scrobble("Artist B", "Shared Release", "One", 4),
                scrobble("Artist B", "Shared Release", "Two", 5),
                scrobble("Artist B", "Shared Release", "Three", 6),
                scrobble("Artist C", "Shared Release", "One", 7),
                scrobble("Artist C", "Shared Release", "Two", 8),
                scrobble("Artist C", "Shared Release", "Four", 9),
            ],
        );

        let batches = batches_with_rows(&rows);

        assert_eq!(batches.len(), 2);
        assert_eq!(
            batches
                .iter()
                .map(|(batch, _)| batch.collection_shaped)
                .collect::<Vec<_>>(),
            vec![Some(true), Some(false)]
        );
        assert_eq!(
            batches
                .iter()
                .map(|(_, rows)| rows
                    .iter()
                    .map(|row| row.artist.as_str())
                    .collect::<BTreeSet<_>>())
                .collect::<Vec<_>>(),
            vec![
                BTreeSet::from(["Artist A", "Artist B"]),
                BTreeSet::from(["Artist C"]),
            ]
        );
    }

    #[test]
    fn source_artist_compatibility_preserves_word_boundaries() {
        assert!(source_artists_compatible(
            "Sarah Brightman",
            "Sarah Brightman & Steve Barton"
        ));
        assert!(!source_artists_compatible(
            "Sarah Brightman",
            "SarahBrightmanX"
        ));
    }

    #[test]
    fn source_track_support_orients_the_smaller_group_as_source() {
        let rows = vec![
            SourceRow {
                stable_id: source_id("Artist", "Larger", "Live Version"),
                artist: "Artist".into(),
                album: "Larger".into(),
                track: "Live Version".into(),
                variants: Vec::new(),
                play_count: 1,
                earliest: 1,
                latest: 1,
            },
            SourceRow {
                stable_id: source_id("Artist", "Larger", "Live Remix"),
                artist: "Artist".into(),
                album: "Larger".into(),
                track: "Live Remix".into(),
                variants: Vec::new(),
                play_count: 1,
                earliest: 1,
                latest: 1,
            },
            SourceRow {
                stable_id: source_id("Artist", "Smaller", "Live"),
                artist: "Artist".into(),
                album: "Smaller".into(),
                track: "Live".into(),
                variants: Vec::new(),
                play_count: 1,
                earliest: 1,
                latest: 1,
            },
        ];
        let larger = SourceGroup {
            artist: "Artist".into(),
            album: "Larger".into(),
            row_indices: vec![0, 1],
        };
        let smaller = SourceGroup {
            artist: "Artist".into(),
            album: "Smaller".into(),
            row_indices: vec![2],
        };

        assert_eq!(source_track_match_count(&larger, &smaller, &rows), 0);
    }

    #[test]
    fn source_clustering_bounds_comparisons_for_large_unique_input() {
        const GROUPS: usize = 23_166;
        let rows = (0..GROUPS)
            .map(|index| {
                let unique = format!("key{index:05}key{index:05}");
                SourceRow {
                    stable_id: source_id(
                        &format!("Artist {unique}"),
                        &format!("Album {unique}"),
                        &format!("Track {unique}"),
                    ),
                    artist: format!("Artist {unique}"),
                    album: format!("Album {unique}"),
                    track: format!("Track {unique}"),
                    variants: Vec::new(),
                    play_count: 1,
                    earliest: index as u64,
                    latest: index as u64,
                }
            })
            .collect::<Vec<_>>();

        let (clusters, comparisons, candidate_bucket_visits) = source_clusters_with_stats(&rows);

        assert_eq!(clusters.len(), GROUPS);
        assert!(comparisons <= GROUPS);
        assert!(candidate_bucket_visits <= GROUPS * 16);
    }

    #[test]
    fn source_batches_attach_only_uniquely_identifying_empty_album_rows() {
        let mut rows = Vec::new();
        aggregate_scrobbles(
            &mut rows,
            &[
                scrobble("Artist", "Named Album", "Unique Song", 1),
                scrobble("Artist", "Other Album", "Shared Song", 2),
                scrobble("Artist", "Second Album", "Shared Song", 3),
                scrobble("Artist", "", "Unique Song", 4),
                scrobble("Artist", "", "Shared Song", 5),
            ],
        );

        let batches = batches_with_rows(&rows);

        assert_eq!(batches.len(), 4);
        let attached = batches
            .iter()
            .find(|(_, rows)| {
                rows.iter()
                    .any(|row| row.album.is_empty() && row.track == "Unique Song")
            })
            .unwrap();
        assert_eq!(attached.0.collection_shaped, Some(true));
        assert_eq!(
            attached.0.representative_album.as_deref(),
            Some("Named Album")
        );
        assert_eq!(attached.0.album_labels, vec!["Named Album"]);
        let residual = batches
            .iter()
            .find(|(_, rows)| {
                rows.iter()
                    .any(|row| row.album.is_empty() && row.track == "Shared Song")
            })
            .unwrap();
        assert_eq!(residual.0.collection_shaped, Some(true));
        assert_eq!(residual.0.representative_album.as_deref(), Some(""));
        assert!(residual.0.album_labels.is_empty());
    }

    #[test]
    fn source_batches_keep_a_literal_singles_album_release_shaped() {
        let mut rows = Vec::new();
        aggregate_scrobbles(&mut rows, &[scrobble("Artist", "Singles", "Song", 1)]);

        let batches = batches_with_rows(&rows);

        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].0.collection_shaped, Some(false));
        assert_eq!(
            batches[0].0.representative_album.as_deref(),
            Some("Singles")
        );
    }

    #[test]
    fn legacy_upgrade_reclusters_only_untouched_batches() {
        let mut rows = Vec::new();
        aggregate_scrobbles(
            &mut rows,
            &[
                scrobble("Protected Artist", "Protected Release", "Keep", 1),
                scrobble("Artist", "Release", "One", 2),
                scrobble("Artist", "Release: Best", "Two", 3),
            ],
        );
        let protected_id = rows[0].stable_id.clone();
        let pending_ids = rows[1..]
            .iter()
            .map(|row| row.stable_id.clone())
            .collect::<Vec<_>>();
        let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 100);
        session.phase = ImportPhase::Review;
        session.rows = rows;
        session.batches = vec![
            ImportBatch {
                page: 7,
                source_ids: vec![protected_id.clone()],
                custom: false,
                collection_shaped: None,
                representative_artist: None,
                representative_album: None,
                album_labels: Vec::new(),
            },
            ImportBatch {
                page: 9,
                source_ids: pending_ids.clone(),
                custom: false,
                collection_shaped: None,
                representative_artist: None,
                representative_album: None,
                album_labels: Vec::new(),
            },
        ];
        let protected_options = PageOptions {
            selected_track_ids: BTreeSet::from([protected_id.clone()]),
            ..PageOptions::default()
        };
        session
            .page_options
            .insert(batch_options_key(7), protected_options.clone());
        let protected_match = MatchResult {
            source_id: protected_id.clone(),
            search_term: "protected".into(),
            confidence: Some(Confidence::Exact),
            selected_uri: Some("spotify:track:protected".into()),
            candidates: Vec::new(),
            track_matches: BTreeMap::new(),
        };
        session
            .matches
            .insert(protected_id.clone(), protected_match.clone());
        let protected_decision = RowDecision {
            status: RowStatus::Done,
            excluded: false,
        };
        session
            .decisions
            .insert(protected_id.clone(), protected_decision.clone());
        session
            .count_modes
            .insert("spotify:track:protected".into(), CountMode::Overwrite);
        assert!(upgrade_legacy_pending_batches(&mut session, &[]));

        let protected = session
            .batches
            .iter()
            .find(|batch| batch.page == 7)
            .unwrap();
        assert_eq!(protected.source_ids, vec![protected_id.clone()]);
        assert_eq!(protected.collection_shaped, Some(false));
        assert_eq!(
            session.page_options.get(&batch_options_key(7)),
            Some(&protected_options)
        );
        assert_eq!(session.matches.get(&protected_id), Some(&protected_match));
        assert_eq!(
            session.decisions.get(&protected_id),
            Some(&protected_decision)
        );
        assert_eq!(
            session.count_modes.get("spotify:track:protected"),
            Some(&CountMode::Overwrite)
        );

        let upgraded = session
            .batches
            .iter()
            .find(|batch| batch.source_ids.iter().any(|id| id == &pending_ids[0]))
            .unwrap();
        assert_eq!(upgraded.page, 1);
        assert_eq!(
            upgraded.source_ids.iter().collect::<BTreeSet<_>>(),
            pending_ids.iter().collect::<BTreeSet<_>>()
        );
        assert_eq!(upgraded.collection_shaped, Some(true));
        assert_eq!(
            upgraded.representative_album.as_deref(),
            Some("Release: Best")
        );
    }

    #[test]
    fn legacy_upgrade_keeps_row_state_without_freezing_old_batch_boundaries() {
        let mut rows = Vec::new();
        aggregate_scrobbles(
            &mut rows,
            &[
                scrobble("Artist", "Release", "One", 1),
                scrobble("Artist", "Release: Best", "Two", 2),
            ],
        );
        let first_id = rows[0].stable_id.clone();
        let second_id = rows[1].stable_id.clone();
        let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 100);
        session.phase = ImportPhase::Review;
        session.rows = rows;
        session.batches = vec![
            ImportBatch {
                page: 7,
                source_ids: vec![first_id.clone()],
                custom: false,
                collection_shaped: None,
                representative_artist: None,
                representative_album: None,
                album_labels: Vec::new(),
            },
            ImportBatch {
                page: 9,
                source_ids: vec![second_id.clone()],
                custom: false,
                collection_shaped: None,
                representative_artist: None,
                representative_album: None,
                album_labels: Vec::new(),
            },
        ];
        session.matches.insert(
            first_id.clone(),
            MatchResult {
                source_id: first_id.clone(),
                search_term: "cached".into(),
                confidence: None,
                selected_uri: None,
                candidates: Vec::new(),
                track_matches: BTreeMap::new(),
            },
        );

        assert!(upgrade_legacy_pending_batches(&mut session, &[]));

        assert_eq!(session.batches.len(), 1);
        assert_eq!(session.batches[0].page, 1);
        assert_eq!(
            session.batches[0]
                .source_ids
                .iter()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([&first_id, &second_id])
        );
        assert!(session.matches.contains_key(&first_id));
    }
}

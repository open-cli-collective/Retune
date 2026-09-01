use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    sync::Arc,
};

use serde_json::Value;

use super::model::{
    ExternalScrobble, ImportPhase, LastFmImportSessionV2, ParsedRecentTracksPage, ParsedScrobble,
    RetryableError, SourceRow,
};
use super::{
    incremental_cache_session, set_sync_problem, Service, SourceVariant, LASTFM_PAGE_WINDOW_SIZE,
};

pub(crate) fn parse_recent_tracks_page(value: &Value) -> Result<ParsedRecentTracksPage, String> {
    let recent = value
        .get("recenttracks")
        .ok_or_else(|| "Last.fm response did not contain recent tracks.".to_string())?;
    let attributes = recent.get("@attr");
    let page = attributes
        .and_then(|value| value.get("page"))
        .and_then(value_string)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let total_pages = attributes
        .and_then(|value| value.get("totalPages"))
        .and_then(value_string)
        .and_then(|value| value.parse().ok());
    let total = attributes
        .and_then(|value| value.get("total"))
        .and_then(value_string)
        .and_then(|value| value.parse().ok());
    let entries = match recent.get("track") {
        Some(Value::Array(entries)) => entries.iter().collect::<Vec<_>>(),
        Some(Value::Object(_)) => vec![recent.get("track").expect("track was just checked")],
        Some(Value::Null) | None => Vec::new(),
        Some(_) => return Err("Last.fm recent tracks had an invalid track list.".into()),
    };
    let mut parsed = ParsedRecentTracksPage {
        page,
        total_pages,
        total,
        ..ParsedRecentTracksPage::default()
    };
    for entry in entries {
        if is_now_playing(entry) {
            parsed.skipped_now_playing += 1;
            continue;
        }
        let artist = entry.get("artist").and_then(value_text).unwrap_or_default();
        let track = entry.get("name").and_then(value_string).unwrap_or_default();
        let album = entry.get("album").and_then(value_text).unwrap_or_default();
        let timestamp = entry
            .get("date")
            .and_then(|date| date.get("uts"))
            .and_then(value_string)
            .and_then(|value| value.parse().ok())
            .filter(|timestamp| *timestamp > 0);
        let Some(timestamp) = timestamp else {
            parsed.skipped_undated += 1;
            continue;
        };
        if artist.trim().is_empty() || track.trim().is_empty() {
            parsed.skipped_undated += 1;
            continue;
        }
        parsed.tracks.push(ParsedScrobble {
            artist: artist.trim().to_owned(),
            album: album.trim().to_owned(),
            track: track.trim().to_owned(),
            timestamp,
        });
    }
    Ok(parsed)
}

pub(super) fn discard_post_cutoff(parsed: &mut ParsedRecentTracksPage, history_to: u64) {
    parsed
        .tracks
        .retain(|scrobble| scrobble.timestamp < history_to);
}

pub(super) fn sort_scrobbles(scrobbles: &mut [ParsedScrobble]) {
    scrobbles.sort_by(|left, right| {
        left.timestamp
            .cmp(&right.timestamp)
            .then_with(|| {
                normalize_for_match(&left.artist).cmp(&normalize_for_match(&right.artist))
            })
            .then_with(|| normalize_for_match(&left.album).cmp(&normalize_for_match(&right.album)))
            .then_with(|| normalize_for_match(&left.track).cmp(&normalize_for_match(&right.track)))
            .then_with(|| left.artist.cmp(&right.artist))
            .then_with(|| left.album.cmp(&right.album))
            .then_with(|| left.track.cmp(&right.track))
    });
}

fn value_string(value: &Value) -> Option<&str> {
    value.as_str()
}

fn value_text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.get("#text").and_then(value_string).map(str::to_owned))
        .or_else(|| value.get("text").and_then(value_string).map(str::to_owned))
}

fn is_now_playing(value: &Value) -> bool {
    matches!(
        value
            .get("@attr")
            .and_then(|attributes| attributes.get("nowplaying")),
        Some(Value::String(value)) if value == "1" || value.eq_ignore_ascii_case("true")
    ) || matches!(
        value
            .get("@attr")
            .and_then(|attributes| attributes.get("nowplaying")),
        Some(Value::Number(value)) if value.as_u64() == Some(1)
    ) || matches!(
        value
            .get("@attr")
            .and_then(|attributes| attributes.get("nowplaying")),
        Some(Value::Bool(true))
    )
}

pub(crate) fn normalize_for_match(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

pub(super) fn source_id(artist: &str, album: &str, track: &str) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}",
        normalize_for_match(artist),
        normalize_for_match(album),
        normalize_for_match(track)
    )
}

pub(super) fn snapshot_cache_id(username: &str, history_to: u64) -> String {
    format!(
        "{}-{history_to}",
        username
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

pub(crate) fn aggregate_scrobbles(rows: &mut Vec<SourceRow>, scrobbles: &[ParsedScrobble]) {
    let mut row_indices = HashMap::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        row_indices.entry(row.stable_id.clone()).or_insert(index);
    }
    for scrobble in scrobbles {
        let id = source_id(&scrobble.artist, &scrobble.album, &scrobble.track);
        let index = if let Some(index) = row_indices.get(&id).copied() {
            index
        } else {
            let index = rows.len();
            rows.push(SourceRow {
                stable_id: id.clone(),
                artist: scrobble.artist.clone(),
                album: scrobble.album.clone(),
                track: scrobble.track.clone(),
                variants: Vec::new(),
                play_count: 0,
                earliest: scrobble.timestamp,
                latest: scrobble.timestamp,
            });
            row_indices.insert(id, index);
            index
        };
        add_variant(&mut rows[index], scrobble);
    }
}

fn incremental_source_id(source_key: &str) -> String {
    format!("incremental:{source_key}")
}

pub(super) fn aggregate_incremental_scrobbles(
    rows: &mut Vec<SourceRow>,
    source_keys: &mut BTreeMap<String, String>,
    scrobbles: &[ExternalScrobble],
) {
    let mut row_indices = rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| {
            source_keys
                .get(&row.stable_id)
                .map(|source_key| (source_key.clone(), index))
        })
        .collect::<HashMap<_, _>>();
    for scrobble in scrobbles {
        let source_key = source_id(&scrobble.artist, &scrobble.album, &scrobble.track);
        let stable_id = incremental_source_id(&source_key);
        let index = if let Some(index) = row_indices.get(&source_key).copied() {
            index
        } else {
            let index = rows.len();
            rows.push(SourceRow {
                stable_id: stable_id.clone(),
                artist: scrobble.artist.clone(),
                album: scrobble.album.clone(),
                track: scrobble.track.clone(),
                variants: Vec::new(),
                play_count: 0,
                earliest: scrobble.timestamp,
                latest: scrobble.timestamp,
            });
            source_keys.insert(stable_id, source_key.clone());
            row_indices.insert(source_key, index);
            index
        };
        add_variant(
            &mut rows[index],
            &ParsedScrobble {
                artist: scrobble.artist.clone(),
                album: scrobble.album.clone(),
                track: scrobble.track.clone(),
                timestamp: scrobble.timestamp,
            },
        );
    }
}

fn add_variant(row: &mut SourceRow, scrobble: &ParsedScrobble) {
    row.play_count = row.play_count.saturating_add(1);
    row.earliest = row.earliest.min(scrobble.timestamp);
    row.latest = row.latest.max(scrobble.timestamp);
    if let Some(variant) = row.variants.iter_mut().find(|variant| {
        variant.artist == scrobble.artist
            && variant.album == scrobble.album
            && variant.track == scrobble.track
    }) {
        variant.play_count = variant.play_count.saturating_add(1);
        variant.earliest = variant.earliest.min(scrobble.timestamp);
        variant.latest = variant.latest.max(scrobble.timestamp);
        return;
    }
    row.variants.push(SourceVariant {
        artist: scrobble.artist.clone(),
        album: scrobble.album.clone(),
        track: scrobble.track.clone(),
        play_count: 1,
        earliest: scrobble.timestamp,
        latest: scrobble.timestamp,
    });
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SourceRunnerStep {
    Probe,
    Page(u32),
    Aggregate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SourceWindowOutcome {
    Complete(Vec<u32>),
    Retryable,
    Suspended,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SourcePageFetchResult {
    Success(ParsedRecentTracksPage),
    AccountMismatch(String),
    Retryable(String),
    Permanent(String),
}

pub(super) fn source_runner_step(session: &LastFmImportSessionV2) -> SourceRunnerStep {
    if session.total_pages.is_none() {
        SourceRunnerStep::Probe
    } else if session.next_page == 0 {
        SourceRunnerStep::Aggregate
    } else {
        SourceRunnerStep::Page(session.next_page)
    }
}

pub(super) fn source_page_window(next_page: u32, total_pages: u32) -> Vec<u32> {
    if next_page == 0 || next_page > total_pages {
        return Vec::new();
    }
    (next_page.saturating_sub(LASTFM_PAGE_WINDOW_SIZE - 1).max(1)..=next_page)
        .rev()
        .collect()
}

pub(super) async fn download_page_window<F, Fut>(
    service: Arc<Service>,
    next_page: u32,
    total_pages: u32,
    fetch: F,
) -> Result<SourceWindowOutcome, String>
where
    F: Fn(u32) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = SourcePageFetchResult> + Send + 'static,
{
    let checkpoint_service = Arc::clone(&service);
    download_page_window_with_checkpoint(
        service,
        next_page,
        total_pages,
        fetch,
        move |page, parsed| {
            let service = Arc::clone(&checkpoint_service);
            async move { service.checkpoint_page(page, &parsed).await.map(|_| ()) }
        },
    )
    .await
}

pub(super) async fn download_page_window_with_checkpoint<F, Fut, C, CFut>(
    service: Arc<Service>,
    next_page: u32,
    total_pages: u32,
    fetch: F,
    checkpoint: C,
) -> Result<SourceWindowOutcome, String>
where
    F: Fn(u32) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = SourcePageFetchResult> + Send + 'static,
    C: Fn(u32, ParsedRecentTracksPage) -> CFut + Clone + Send + Sync + 'static,
    CFut: Future<Output = Result<(), String>> + Send,
{
    let pages = source_page_window(next_page, total_pages);
    if pages.is_empty() {
        return Err("Last.fm import page window is empty.".into());
    }
    let mut requests = pages
        .iter()
        .copied()
        .map(|page| {
            let fetch = fetch.clone();
            tokio::spawn(async move { fetch(page).await })
        })
        .collect::<Vec<_>>();
    let mut checkpointed = Vec::with_capacity(requests.len());

    for page in pages {
        let request = requests.remove(0);
        let parsed = match request.await {
            Ok(SourcePageFetchResult::Success(parsed)) => parsed,
            Ok(failure) => {
                for request in &requests {
                    request.abort();
                }
                return match failure {
                    SourcePageFetchResult::AccountMismatch(_) => {
                        service.suspend_for_account_mismatch().await?;
                        Ok(SourceWindowOutcome::Suspended)
                    }
                    SourcePageFetchResult::Retryable(message) => {
                        let attempt = service
                            .snapshot()
                            .await
                            .and_then(|session| session.retryable_error)
                            .filter(|error| error.retryable)
                            .map(|error| error.attempt.saturating_add(1))
                            .unwrap_or(1);
                        service
                            .set_retryable_error(Some(RetryableError {
                                message,
                                attempt,
                                retryable: true,
                            }))
                            .await?;
                        Ok(SourceWindowOutcome::Retryable)
                    }
                    SourcePageFetchResult::Permanent(message) => {
                        service
                            .set_retryable_error(Some(RetryableError {
                                message: message.clone(),
                                attempt: 0,
                                retryable: false,
                            }))
                            .await?;
                        Err(message)
                    }
                    SourcePageFetchResult::Success(_) => unreachable!(),
                };
            }
            Err(error) => {
                for request in &requests {
                    request.abort();
                }
                let message = format!("Last.fm page fetch task stopped: {error}");
                service
                    .set_retryable_error(Some(RetryableError {
                        message: message.clone(),
                        attempt: 0,
                        retryable: false,
                    }))
                    .await?;
                return Err(message);
            }
        };
        if let Err(error) = checkpoint(page, parsed.clone()).await {
            for request in &requests {
                request.abort();
            }
            return Err(error);
        }
        checkpointed.push(page);
    }

    Ok(SourceWindowOutcome::Complete(checkpointed))
}

pub(super) async fn fetch_source_page(
    lastfm: &Arc<crate::lastfm::Service>,
    username: &str,
    generation: u64,
    page: u32,
    from: u64,
    history_to: u64,
) -> SourcePageFetchResult {
    match lastfm
        .import_recent_tracks_page(username, generation, page, from, history_to)
        .await
    {
        Ok(payload) => match parse_recent_tracks_page(&payload) {
            Ok(parsed) => SourcePageFetchResult::Success(parsed),
            Err(message) => SourcePageFetchResult::Permanent(message),
        },
        Err(error) if error.account_mismatch => {
            SourcePageFetchResult::AccountMismatch(error.message)
        }
        Err(error) if error.retryable => SourcePageFetchResult::Retryable(error.message),
        Err(error) => SourcePageFetchResult::Permanent(error.message),
    }
}

pub(super) fn startup_resume_plan(
    session: Option<&LastFmImportSessionV2>,
) -> Option<(String, u64)> {
    session
        .filter(|session| {
            matches!(
                session.phase,
                ImportPhase::Downloading | ImportPhase::Aggregating
            )
        })
        .map(|session| (session.lastfm_username.clone(), session.history_to))
}

pub(super) fn startup_lastfm_identity_matches(
    session: &LastFmImportSessionV2,
    live_username: Option<&str>,
) -> bool {
    session.phase != ImportPhase::Aggregating
        || live_username == Some(session.lastfm_username.as_str())
}

pub(super) async fn run_import<F, Fut>(
    lastfm: Arc<crate::lastfm::Service>,
    service: Arc<Service>,
    username: String,
    mut progress: F,
) where
    F: FnMut() -> Fut + Send,
    Fut: Future<Output = ()> + Send,
{
    let generation = lastfm.import_generation();
    let result = async {
        loop {
            let Some(session) = service.snapshot().await else {
                break;
            };
            match session.phase {
                ImportPhase::Downloading => {
                    match source_runner_step(&session) {
                        SourceRunnerStep::Probe => {
                            let payload = fetch_import_page_with_retry(
                                &lastfm,
                                &service,
                                &username,
                                generation,
                                1,
                                session.history_to,
                            )
                            .await?;
                            let parsed = match parse_recent_tracks_page(&payload) {
                                Ok(parsed) => parsed,
                                Err(message) => {
                                    service
                                        .set_retryable_error(Some(RetryableError {
                                            message: message.clone(),
                                            attempt: 0,
                                            retryable: false,
                                        }))
                                        .await?;
                                    return Err(message);
                                }
                            };
                            let Some(total_pages) = parsed.total_pages else {
                                let message =
                                    "Last.fm metadata did not include a total page count."
                                        .to_string();
                                service
                                    .set_retryable_error(Some(RetryableError {
                                        message: message.clone(),
                                        attempt: 0,
                                        retryable: false,
                                    }))
                                    .await?;
                                return Err(message);
                            };
                            service
                                .set_metadata(total_pages, parsed.total.unwrap_or_default())
                                .await?;
                        }
                        SourceRunnerStep::Aggregate => {
                            service.aggregate_cached(Some(lastfm.as_ref())).await?;
                        }
                        SourceRunnerStep::Page(page) => {
                            let total_pages = session
                                .total_pages
                                .expect("page downloads require Last.fm metadata");
                            let window_lastfm = Arc::clone(&lastfm);
                            let window_username = username.clone();
                            let history_to = session.history_to;
                            let outcome = download_page_window(
                                Arc::clone(&service),
                                page,
                                total_pages,
                                move |page| {
                                    let lastfm = Arc::clone(&window_lastfm);
                                    let username = window_username.clone();
                                    async move {
                                        fetch_source_page(
                                            &lastfm, &username, generation, page, 0, history_to,
                                        )
                                        .await
                                    }
                                },
                            )
                            .await?;
                            match outcome {
                                SourceWindowOutcome::Complete(_) => {}
                                SourceWindowOutcome::Retryable => {
                                    progress().await;
                                    if let Err(error) = lastfm
                                        .wait_for_import_retry(
                                            &username,
                                            generation,
                                            crate::lastfm::import_retry_delay(usize::MAX),
                                        )
                                        .await
                                    {
                                        service.suspend_for_account_mismatch().await?;
                                        return Err(error.message);
                                    }
                                    continue;
                                }
                                SourceWindowOutcome::Suspended => break,
                            }
                        }
                    }
                    progress().await;
                }
                ImportPhase::Aggregating => {
                    service.aggregate_cached(Some(lastfm.as_ref())).await?;
                }
                ImportPhase::Review | ImportPhase::Done | ImportPhase::Suspended => break,
            }
        }
        Ok::<(), String>(())
    }
    .await;
    if let Err(error) = result {
        let _ = service
            .set_retryable_error(Some(RetryableError {
                message: error,
                attempt: 0,
                retryable: false,
            }))
            .await;
    }
    progress().await;
}

async fn fetch_import_page_with_retry(
    lastfm: &Arc<crate::lastfm::Service>,
    service: &Service,
    username: &str,
    generation: u64,
    page: u32,
    history_to: u64,
) -> Result<Value, String> {
    loop {
        match lastfm
            .import_recent_tracks_page(username, generation, page, 0, history_to)
            .await
        {
            Ok(payload) => {
                service.set_retryable_error(None).await?;
                return Ok(payload);
            }
            Err(error) if error.account_mismatch => {
                service.suspend_for_account_mismatch().await?;
                return Err(error.message);
            }
            Err(error) if error.retryable => {
                let attempt = service
                    .snapshot()
                    .await
                    .and_then(|session| session.retryable_error)
                    .map(|error| error.attempt.saturating_add(1))
                    .unwrap_or(1);
                service
                    .set_retryable_error(Some(RetryableError {
                        message: error.message,
                        attempt,
                        retryable: true,
                    }))
                    .await?;
                if let Err(error) = lastfm
                    .wait_for_import_retry(
                        username,
                        generation,
                        crate::lastfm::import_retry_delay(usize::MAX),
                    )
                    .await
                {
                    service.suspend_for_account_mismatch().await?;
                    return Err(error.message);
                }
            }
            Err(error) => {
                service
                    .set_retryable_error(Some(RetryableError {
                        message: error.message.clone(),
                        attempt: 0,
                        retryable: false,
                    }))
                    .await?;
                return Err(error.message);
            }
        }
    }
}

pub(super) async fn fetch_incremental_page_with_retry(
    lastfm: &Arc<crate::lastfm::Service>,
    service: &Service,
    username: &str,
    generation: u64,
    page: u32,
    from: u64,
    to: u64,
) -> Result<Value, String> {
    loop {
        match lastfm
            .import_recent_tracks_page(username, generation, page, from, to)
            .await
        {
            Ok(payload) => {
                set_sync_problem(service, None).await?;
                return Ok(payload);
            }
            Err(error) if error.account_mismatch => {
                set_sync_problem(service, Some(error.message.clone())).await?;
                return Err(error.message);
            }
            Err(error) if error.retryable => {
                set_sync_problem(service, Some(error.message)).await?;
                if let Err(error) = lastfm
                    .wait_for_import_retry(
                        username,
                        generation,
                        crate::lastfm::import_retry_delay(usize::MAX),
                    )
                    .await
                {
                    set_sync_problem(service, Some(error.message.clone())).await?;
                    return Err(error.message);
                }
            }
            Err(error) => {
                set_sync_problem(service, Some(error.message.clone())).await?;
                return Err(error.message);
            }
        }
    }
}

pub(super) async fn read_incremental_events(
    service: &Service,
    username: &str,
) -> Result<Vec<ExternalScrobble>, String> {
    let state = service.sync_snapshot().await;
    let Some(range) = state.active.as_ref() else {
        return Ok(Vec::new());
    };
    let session = incremental_cache_session(&state, username)?;
    let store = service.store.clone();
    let parsed = tauri::async_runtime::spawn_blocking(move || store.read_pages(&session))
        .await
        .map_err(|_| "Last.fm incremental aggregation task stopped.".to_string())??;
    let mut events = parsed
        .into_iter()
        .filter(|scrobble| scrobble.timestamp >= range.from && scrobble.timestamp < range.to)
        .map(|scrobble| ExternalScrobble {
            artist: scrobble.artist,
            album: scrobble.album,
            track: scrobble.track,
            timestamp: scrobble.timestamp,
            submitted: None,
        })
        .collect::<Vec<_>>();
    events.sort_by(|left, right| {
        left.timestamp
            .cmp(&right.timestamp)
            .then_with(|| {
                normalize_for_match(&left.artist).cmp(&normalize_for_match(&right.artist))
            })
            .then_with(|| normalize_for_match(&left.album).cmp(&normalize_for_match(&right.album)))
            .then_with(|| normalize_for_match(&left.track).cmp(&normalize_for_match(&right.track)))
    });
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(entries: Value) -> Value {
        serde_json::json!({
            "recenttracks": {
                "track": entries,
                "@attr": {"page": "2", "totalPages": "4", "total": "601"}
            }
        })
    }

    fn scrobble(artist: &str, album: &str, track: &str, timestamp: u64) -> ParsedScrobble {
        ParsedScrobble {
            artist: artist.into(),
            album: album.into(),
            track: track.into(),
            timestamp,
        }
    }

    #[test]
    fn parses_nowplaying_and_undated_rows_without_retaining_them() {
        let parsed = parse_recent_tracks_page(&response(serde_json::json!([
            {"artist": {"#text": "Artist"}, "album": {"#text": "Album"}, "name": "Song", "date": {"uts": "20"}},
            {"artist": {"#text": "Live"}, "name": "Now", "@attr": {"nowplaying": "1"}},
            {"artist": {"#text": "Live"}, "name": "Now too", "@attr": {"nowplaying": true}},
            {"artist": {"#text": "Old"}, "name": "Missing date"},
        ])))
        .unwrap();

        assert_eq!(parsed.page, 2);
        assert_eq!(parsed.total_pages, Some(4));
        assert_eq!(parsed.total, Some(601));
        assert_eq!(parsed.tracks.len(), 1);
        assert_eq!(parsed.skipped_now_playing, 2);
        assert_eq!(parsed.skipped_undated, 1);
        assert_eq!(parsed.tracks[0].timestamp, 20);
    }

    #[test]
    fn parses_a_single_track_object_and_text_fields() {
        let parsed = parse_recent_tracks_page(&response(serde_json::json!({
            "artist": "Artist", "album": "Album", "name": "Song", "date": {"uts": "9"}
        })))
        .unwrap();
        assert_eq!(parsed.tracks, vec![scrobble("Artist", "Album", "Song", 9)]);
    }

    #[test]
    fn aggregation_input_is_sorted_oldest_first_with_deterministic_ties() {
        let mut scrobbles = vec![
            scrobble("B", "Album", "Track", 20),
            scrobble("A", "Album", "Track", 10),
            scrobble("A", "Album", "Other", 10),
        ];
        sort_scrobbles(&mut scrobbles);
        assert_eq!(
            scrobbles
                .iter()
                .map(|row| (row.artist.clone(), row.track.clone(), row.timestamp))
                .collect::<Vec<_>>(),
            vec![
                ("A".to_owned(), "Other".to_owned(), 10),
                ("A".to_owned(), "Track".to_owned(), 10),
                ("B".to_owned(), "Track".to_owned(), 20),
            ]
        );
    }

    #[test]
    fn aggregation_handles_a_large_unique_input_with_indexed_rows() {
        const UNIQUE_SCROBBLES: usize = 50_000;
        let scrobbles = (0..UNIQUE_SCROBBLES)
            .map(|index| scrobble("Artist", "Album", &format!("Track {index}"), index as u64))
            .collect::<Vec<_>>();
        let mut rows = Vec::new();

        aggregate_scrobbles(&mut rows, &scrobbles);

        assert_eq!(rows.len(), UNIQUE_SCROBBLES);
        assert_eq!(rows[0].track, "Track 0");
        assert_eq!(rows[UNIQUE_SCROBBLES - 1].track, "Track 49999");
    }

    #[test]
    fn source_runner_plans_probe_descending_pages_and_aggregate_without_cursor_advance() {
        let mut session = LastFmImportSessionV2::new_with_defaults(
            "user".into(),
            100,
            super::super::ImportDefaults::default(),
        );
        assert_eq!(source_runner_step(&session), SourceRunnerStep::Probe);

        session.total_pages = Some(3);
        session.next_page = 3;
        session.retryable_error = Some(RetryableError {
            message: "temporary".into(),
            attempt: 1,
            retryable: true,
        });
        assert_eq!(source_runner_step(&session), SourceRunnerStep::Page(3));
        assert_eq!(session.next_page, 3);
        assert_eq!(session.downloaded_pages, 0);

        session.downloaded_pages = 1;
        session.next_page = 2;
        assert_eq!(source_runner_step(&session), SourceRunnerStep::Page(2));
        session.downloaded_pages = 2;
        session.next_page = 1;
        assert_eq!(source_runner_step(&session), SourceRunnerStep::Page(1));
        session.downloaded_pages = 3;
        session.next_page = 0;
        assert_eq!(source_runner_step(&session), SourceRunnerStep::Aggregate);
    }

    #[test]
    fn source_page_windows_cover_full_and_tail_ranges() {
        assert_eq!(source_page_window(12, 12), vec![12, 11, 10, 9]);
        assert_eq!(source_page_window(3, 12), vec![3, 2, 1]);
        assert!(source_page_window(0, 12).is_empty());
    }

    #[test]
    fn startup_resume_plan_uses_the_persisted_source_identity_only() {
        let mut session = LastFmImportSessionV2::new_with_defaults(
            "fixed-user".into(),
            1786804381,
            super::super::ImportDefaults::default(),
        );
        assert_eq!(
            startup_resume_plan(Some(&session)),
            Some(("fixed-user".into(), 1786804381))
        );
        assert!(startup_lastfm_identity_matches(
            &session,
            Some("other-user")
        ));
        session.phase = ImportPhase::Aggregating;
        assert_eq!(
            startup_resume_plan(Some(&session)),
            Some(("fixed-user".into(), 1786804381))
        );
        assert!(startup_lastfm_identity_matches(
            &session,
            Some("fixed-user")
        ));
        assert!(!startup_lastfm_identity_matches(
            &session,
            Some("other-user")
        ));
        assert!(!startup_lastfm_identity_matches(&session, None));
        session.phase = ImportPhase::Review;
        assert_eq!(startup_resume_plan(Some(&session)), None);
        assert_eq!(startup_resume_plan(None), None);
    }
}

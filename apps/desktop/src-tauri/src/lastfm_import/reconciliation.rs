use std::collections::{BTreeMap, BTreeSet};

use retune_core::model::{AlbumKey, Library, Rating, TrackEdit};

use crate::lastfm::{AcceptedScrobbleReceipt, ScrobbleMetadata};

use super::model::{
    CountMode, ExternalScrobble, HistoryUpdate, JournalRecovery, JournalRecoveryError,
    LastFmApplicationJournal, LastFmMappings, ReconciliationResult, SourceRow,
};
use super::source::{normalize_for_match, source_id};

pub(crate) fn resolved_play_count(rows: &[&SourceRow], mode: CountMode) -> u64 {
    match mode {
        CountMode::Sum => rows
            .iter()
            .map(|row| row.play_count)
            .fold(0, u64::saturating_add),
        CountMode::Overwrite => rows
            .iter()
            .flat_map(|row| row.variants.iter())
            .map(|variant| variant.play_count)
            .max()
            .unwrap_or(0),
        CountMode::Zero => 0,
    }
}

pub(crate) fn resolved_timestamps(rows: &[&SourceRow]) -> Option<(u64, u64)> {
    let earliest = rows.iter().map(|row| row.earliest).min()?;
    let latest = rows.iter().map(|row| row.latest).max()?;
    Some((earliest, latest))
}

fn metadata_matches_event(metadata: &ScrobbleMetadata, event: &ExternalScrobble) -> bool {
    let matches = |candidate: &ScrobbleMetadata| {
        normalize_for_match(&metadata.artist) == normalize_for_match(&candidate.artist)
            && normalize_for_match(&metadata.album) == normalize_for_match(&candidate.album)
            && normalize_for_match(&metadata.track) == normalize_for_match(&candidate.track)
    };
    matches(&ScrobbleMetadata {
        artist: event.artist.clone(),
        album: event.album.clone(),
        track: event.track.clone(),
    }) || event.submitted.as_ref().is_some_and(matches)
}

pub(super) fn source_album_key(artist: &str, album: &str) -> String {
    format!(
        "{}\u{1f}{}",
        normalize_for_match(artist),
        normalize_for_match(album)
    )
}

fn mapped_target(event: &ExternalScrobble, mappings: &LastFmMappings) -> Option<Option<String>> {
    let track_key = source_id(&event.artist, &event.album, &event.track);
    if mappings.excluded_tracks.contains(&track_key)
        || mappings
            .ignored_albums
            .contains(&source_album_key(&event.artist, &event.album))
        || mappings
            .ignored_artists
            .contains(&normalize_for_match(&event.artist))
    {
        return Some(None);
    }
    if let Some(uri) = mappings.track_mappings.get(&track_key) {
        return Some(Some(uri.clone()));
    }
    mappings
        .album_mappings
        .get(&source_album_key(&event.artist, &event.album))
        .and_then(|mapping| {
            mapping
                .track_uris_by_name
                .get(&normalize_for_match(&event.track))
                .cloned()
        })
        .map(Some)
}

pub(crate) fn reconcile_incremental(
    events: &[ExternalScrobble],
    receipts: &[AcceptedScrobbleReceipt],
    mappings: &LastFmMappings,
    available_library_uris: &BTreeSet<String>,
    from: u64,
    to: u64,
) -> ReconciliationResult {
    let mut result = ReconciliationResult::default();
    let mut consumed = vec![false; receipts.len()];
    for event in events
        .iter()
        .filter(|event| event.timestamp >= from && event.timestamp < to)
    {
        if let Some(index) = receipts.iter().enumerate().find_map(|(index, receipt)| {
            (!consumed[index]
                && receipt.timestamp == event.timestamp
                && (metadata_matches_event(&receipt.corrected, event)
                    || metadata_matches_event(&receipt.submitted, event)))
            .then_some(index)
        }) {
            consumed[index] = true;
            result.consumed_receipts.push(receipts[index].clone());
            continue;
        }
        let Some(target) = mapped_target(event, mappings) else {
            result.unresolved.push(event.clone());
            continue;
        };
        let Some(target) = target else {
            continue;
        };
        if !available_library_uris.contains(&target) {
            result.unresolved.push(event.clone());
            continue;
        }
        result
            .increments
            .entry(target.clone())
            .and_modify(|count| *count = count.saturating_add(1))
            .or_insert(1);
        result
            .latest
            .entry(target)
            .and_modify(|latest| *latest = (*latest).max(event.timestamp))
            .or_insert(event.timestamp);
    }
    result
}

pub(crate) fn apply_incremental_updates(
    library: &mut Library,
    increments: &BTreeMap<String, u64>,
    latest: &BTreeMap<String, u64>,
) {
    for (uri, increment) in increments {
        library.merge_history_additive(uri, *increment, None, latest.get(uri).copied());
    }
}

pub(crate) fn recover_application_journal(
    library: &mut Library,
    journal: &LastFmApplicationJournal,
) -> Result<JournalRecovery, JournalRecoveryError> {
    if *library == journal.before_library {
        *library = journal.after_library.clone();
        return Ok(JournalRecovery::AppliedBefore);
    }
    if *library == journal.after_library {
        return Ok(JournalRecovery::AlreadyApplied);
    }
    Err(JournalRecoveryError::Conflict)
}

pub(crate) fn apply_history_updates(library: &mut Library, updates: &[HistoryUpdate]) {
    for update in updates {
        library.merge_history_absolute(
            &update.uri,
            update.play_count,
            update.earliest,
            update.latest,
        );
    }
}

pub(crate) fn apply_metadata(
    library: &mut Library,
    tracks: &[String],
    whole_album: bool,
    genre: Option<&str>,
    rating: Option<u8>,
) -> Result<(), String> {
    let ids = library
        .tracks()
        .iter()
        .filter(|track| tracks.iter().any(|uri| uri == &track.uri))
        .map(|track| track.id)
        .collect::<Vec<_>>();
    if let Some(genre) = genre.map(str::trim).filter(|genre| !genre.is_empty()) {
        for id in &ids {
            library
                .edit(
                    *id,
                    TrackEdit {
                        cat: Some(genre.to_owned()),
                        ..TrackEdit::default()
                    },
                )
                .map_err(|error| error.to_string())?;
        }
    }
    let Some(stars) = rating else {
        return Ok(());
    };
    let rating = Rating::new(stars).ok_or_else(|| "Rating must be between 1 and 5.".to_string())?;
    if whole_album {
        let Some(first) = ids.first().and_then(|id| library.get(*id)) else {
            return Ok(());
        };
        library.set_album_rating(AlbumKey::of(first), Some(rating));
    } else {
        for id in ids {
            library
                .set_track_rating(id, Some(rating))
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use retune_core::model::{NewTrack, SourceId};

    use super::*;

    fn event(artist: &str, album: &str, track: &str, timestamp: u64) -> ExternalScrobble {
        ExternalScrobble {
            artist: artist.into(),
            album: album.into(),
            track: track.into(),
            timestamp,
            submitted: None,
        }
    }

    #[test]
    fn reconciliation_maps_an_external_scrobble_once() {
        let mappings = LastFmMappings {
            track_mappings: BTreeMap::from([(
                source_id("Artist", "Album", "Song"),
                "spotify:track:one".into(),
            )]),
            ..LastFmMappings::default()
        };
        let result = reconcile_incremental(
            &[event("Artist", "Album", "Song", 150)],
            &[],
            &mappings,
            &BTreeSet::from(["spotify:track:one".to_owned()]),
            100,
            200,
        );

        assert_eq!(
            result.increments,
            BTreeMap::from([(String::from("spotify:track:one"), 1)])
        );
        assert!(result.unresolved.is_empty());
    }

    #[test]
    fn mapped_backlog_occurrence_adds_to_existing_count() {
        let mappings = LastFmMappings {
            track_mappings: BTreeMap::from([(
                source_id("Artist", "Album", "Song"),
                "spotify:track:one".into(),
            )]),
            ..LastFmMappings::default()
        };
        let result = reconcile_incremental(
            &[event("Artist", "Album", "Song", 10)],
            &[],
            &mappings,
            &BTreeSet::from(["spotify:track:one".into()]),
            0,
            20,
        );
        let mut library = Library::new();
        let id = library.add(NewTrack {
            uri: "spotify:track:one".into(),
            ..NewTrack::default()
        });
        library.merge_history_absolute("spotify:track:one", Some(100), None, None);

        apply_incremental_updates(&mut library, &result.increments, &result.latest);

        assert_eq!(library.get(id).unwrap().play_count, 101);
    }

    #[test]
    fn incremental_updates_saturate_and_journal_recovery_is_exactly_once() {
        let mut before = Library::new();
        before.add(NewTrack {
            uri: "spotify:track:one".into(),
            name: "Song".into(),
            source: SourceId::Music,
            ..NewTrack::default()
        });
        before.merge_history_absolute("spotify:track:one", Some((u32::MAX - 1) as u64), None, None);
        let mut after = before.clone();
        apply_incremental_updates(
            &mut after,
            &BTreeMap::from([(String::from("spotify:track:one"), 2)]),
            &BTreeMap::from([(String::from("spotify:track:one"), 40)]),
        );
        assert_eq!(after.tracks()[0].play_count, u32::MAX);
        assert_eq!(after.tracks()[0].last_played_at, Some(40));
        let journal = LastFmApplicationJournal {
            before_library: before.clone(),
            after_library: after.clone(),
            checkpoint_before: Some(1),
            checkpoint_after: Some(2),
            backlog_before: Vec::new(),
            backlog_after: Vec::new(),
            consumed_receipts: Vec::new(),
        };
        assert_eq!(
            recover_application_journal(&mut before, &journal).unwrap(),
            JournalRecovery::AppliedBefore
        );
        assert_eq!(before, after);
        assert_eq!(
            recover_application_journal(&mut before, &journal).unwrap(),
            JournalRecovery::AlreadyApplied
        );
        let mut conflict = Library::new();
        conflict.add(NewTrack {
            uri: "spotify:track:one".into(),
            name: "Song".into(),
            source: SourceId::Music,
            ..NewTrack::default()
        });
        conflict.merge_history_absolute("spotify:track:one", Some(1), None, None);
        assert_eq!(
            recover_application_journal(&mut conflict, &journal),
            Err(JournalRecoveryError::Conflict)
        );
    }

    #[test]
    fn metadata_scope_and_blank_values_preserve_existing_data() {
        let mut library = Library::new();
        let first = library.add(NewTrack {
            uri: "one".into(),
            art: "A".into(),
            alb: "B".into(),
            cat: "Old".into(),
            ..Default::default()
        });
        let second = library.add(NewTrack {
            uri: "two".into(),
            art: "A".into(),
            alb: "B".into(),
            cat: "Old".into(),
            ..Default::default()
        });
        apply_metadata(
            &mut library,
            &["one".into(), "two".into()],
            false,
            Some(" "),
            Some(5),
        )
        .unwrap();
        assert_eq!(
            library.get(first).unwrap().rating.map(Rating::stars),
            Some(5)
        );
        assert_eq!(
            library.get(second).unwrap().rating.map(Rating::stars),
            Some(5)
        );
        assert_eq!(library.get(first).unwrap().cat, "Old");
        apply_metadata(
            &mut library,
            &["one".into(), "two".into()],
            true,
            Some("Rock"),
            Some(4),
        )
        .unwrap();
        assert_eq!(
            library
                .album_rating(&AlbumKey {
                    source: SourceId::Music,
                    art: "A".into(),
                    alb: "B".into(),
                })
                .map(Rating::stars),
            Some(4)
        );
        assert_eq!(
            library.get(first).unwrap().rating.map(Rating::stars),
            Some(5)
        );
    }
}

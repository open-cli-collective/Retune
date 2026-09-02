use super::model::ImportApplyFinished;
use super::*;
use crate::{
    spotify_membership::SpotifyMembership,
    store::{FsSpotifyLibraryStore, SavedAlbumRecord, SpotifyLibraryState},
};
use retune_core::model::SourceId;
use retune_spotify::{
    client::{fake_client, Response},
    tokens::{TokenStore, Tokens},
};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

fn test_spotify_membership(path: &Path) -> Arc<SpotifyMembership> {
    Arc::new(SpotifyMembership::new(
        SpotifyLibraryState::default(),
        FsSpotifyLibraryStore::new(path),
    ))
}

fn album_summary_json(id: &str, name: &str, artist: &str, track_count: u32) -> Value {
    serde_json::json!({
        "id": id,
        "uri": format!("spotify:album:{id}"),
        "name": name,
        "artists": [{"id": "artist", "name": artist}],
        "release_date": "2024",
        "total_tracks": track_count
    })
}

fn album_response(id: &str, name: &str, artist: &str, track_names: &[&str]) -> Response {
    let tracks = track_names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            serde_json::json!({
                "uri": format!("spotify:track:{id}-{index}"),
                "name": name,
                // Keep album hydration focused on album requests.  An artist
                // id here would make provider enrichment issue an unrelated
                // /artists request for every hydrated album.
                "artists": [{"id": "", "name": artist}],
                "track_number": index + 1,
                "duration_ms": 180000
            })
        })
        .collect::<Vec<_>>();
    Response::json(
        200,
        serde_json::json!({
            "id": id,
            "uri": format!("spotify:album:{id}"),
            "name": name,
            "artists": [{"id": "", "name": artist}],
            "release_date": "2024",
            "total_tracks": track_names.len(),
            "tracks": {"items": tracks, "next": null, "total": track_names.len()}
        }),
    )
}

fn album_search_response(items: Vec<Value>) -> Response {
    Response::json(
        200,
        serde_json::json!({
            "albums": {"items": items, "next": null, "total": 0}
        }),
    )
}

fn scrobble(artist: &str, album: &str, track: &str, timestamp: u64) -> ParsedScrobble {
    ParsedScrobble {
        artist: artist.into(),
        album: album.into(),
        track: track.into(),
        timestamp,
    }
}

fn collection_test_row(track: &str) -> SourceRow {
    SourceRow {
        stable_id: source_id("Artist", "", track),
        artist: "Artist".into(),
        album: String::new(),
        track: track.into(),
        variants: Vec::new(),
        play_count: 1,
        earliest: 1,
        latest: 1,
    }
}

fn collection_album(uri: &str, artist: &str, tracks: &[(&str, &str)]) -> CollectionAlbumCandidate {
    CollectionAlbumCandidate {
        matching: AlbumCandidate {
            uri: uri.into(),
            name: uri.rsplit(':').next().unwrap_or(uri).into(),
            artist: artist.into(),
            in_library: false,
            track_uris: tracks.iter().map(|(_, uri)| (*uri).into()).collect(),
            track_names: tracks.iter().map(|(name, _)| (*name).into()).collect(),
            track_artists: tracks.iter().map(|_| artist.into()).collect(),
            track_albums: tracks.iter().map(|_| uri.into()).collect(),
            relation: None,
        },
        total_tracks: tracks.len() as u32,
        track_numbers: (1..=tracks.len())
            .map(|number| Some(number as u32))
            .collect(),
        track_durations: vec![180; tracks.len()],
        ..CollectionAlbumCandidate::default()
    }
}

fn collection_album_for_rows(
    uri: &str,
    rows: &[SourceRow],
    indices: &[usize],
) -> CollectionAlbumCandidate {
    let mut candidate = CollectionAlbumCandidate {
        matching: AlbumCandidate {
            uri: uri.into(),
            name: uri.rsplit(':').next().unwrap_or(uri).into(),
            artist: "Artist".into(),
            ..AlbumCandidate::default()
        },
        ..CollectionAlbumCandidate::default()
    };
    for index in indices {
        let row = &rows[*index];
        candidate.matching.track_uris.push(format!(
            "spotify:track:{}-{}",
            uri.rsplit(':').next().unwrap_or(uri),
            index
        ));
        candidate.matching.track_names.push(row.track.clone());
        candidate.matching.track_artists.push(row.artist.clone());
        candidate
            .matching
            .track_albums
            .push(candidate.matching.name.clone());
    }
    candidate.total_tracks = candidate.matching.track_uris.len() as u32;
    candidate.track_numbers = (1..=candidate.total_tracks).map(Some).collect();
    candidate.track_durations = vec![180; candidate.total_tracks as usize];
    candidate
}

fn exact_collection_track(row: &SourceRow, uri: &str) -> AlbumCandidate {
    AlbumCandidate {
        uri: uri.into(),
        name: row.track.clone(),
        artist: row.artist.clone(),
        track_uris: vec![uri.into()],
        track_names: vec![row.track.clone()],
        track_artists: vec![row.artist.clone()],
        track_albums: vec![String::new()],
        ..AlbumCandidate::default()
    }
}

#[test]
fn collection_album_preview_requires_all_declared_tracks() {
    let mut album: retune_spotify::client::Album = serde_json::from_value(serde_json::json!({
        "id": "album",
        "uri": "spotify:album:album",
        "name": "Album",
        "total_tracks": 2,
        "tracks": {
            "items": [
                {"uri": "spotify:track:one", "name": "One"},
                {"uri": "spotify:track:two", "name": "Two"}
            ],
            "next": null,
            "total": 2
        }
    }))
    .unwrap();
    assert!(album_tracks_complete(&album));
    album.total_tracks = 3;
    assert!(!album_tracks_complete(&album));
    album.tracks = None;
    assert!(!album_tracks_complete(&album));
}

fn collection_match(row: &SourceRow) -> MatchResult {
    MatchResult {
        source_id: row.stable_id.clone(),
        search_term: track_search_term(&row.artist, &row.track),
        confidence: None,
        selected_uri: None,
        candidates: Vec::new(),
        track_matches: BTreeMap::new(),
    }
}

fn collection_session(rows: &[SourceRow]) -> LastFmImportSessionV2 {
    let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 100);
    session.rows = rows.to_vec();
    session.batches = build_review_batches(&session.rows);
    session.phase = ImportPhase::Review;
    for row in rows {
        session
            .matches
            .insert(row.stable_id.clone(), collection_match(row));
    }
    session
}

fn release_test_rows() -> Vec<SourceRow> {
    ["One", "Two", "Three"]
        .into_iter()
        .map(|track| SourceRow {
            stable_id: source_id("Artist", "Release", track),
            artist: "Artist".into(),
            album: "Release".into(),
            track: track.into(),
            variants: Vec::new(),
            play_count: 1,
            earliest: 1,
            latest: 1,
        })
        .collect()
}

fn selected_release_session() -> (LastFmImportSessionV2, Vec<SourceRow>, String) {
    let rows = release_test_rows();
    let release_uri = "spotify:album:seed".to_owned();
    let release = AlbumCandidate {
        uri: release_uri.clone(),
        name: "Seed Release".into(),
        artist: "Artist".into(),
        track_uris: [
            "spotify:track:one",
            "spotify:track:two",
            "spotify:track:three",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        track_names: rows.iter().map(|row| row.track.clone()).collect(),
        track_artists: rows.iter().map(|row| row.artist.clone()).collect(),
        track_albums: vec!["Seed Release".into(); rows.len()],
        relation: Some(AlbumRelation::BestMatch),
        ..AlbumCandidate::default()
    };
    let mut session = collection_session(&rows);
    session.batches = vec![ImportBatch {
        page: 1,
        source_ids: rows.iter().map(|row| row.stable_id.clone()).collect(),
        collection_shaped: Some(false),
        representative_artist: Some("Artist".into()),
        representative_album: Some("Release".into()),
        album_labels: vec!["Release".into()],
    }];
    session.page_options.insert(
        batch_options_key(1),
        PageOptions {
            import_content: true,
            include_historical_play_counts: true,
            whole_album: true,
            ..PageOptions::default()
        },
    );
    for (index, row) in rows.iter().enumerate() {
        let explicit = index == 0;
        session.matches.insert(
            row.stable_id.clone(),
            MatchResult {
                source_id: row.stable_id.clone(),
                search_term: album_search_term(&row.artist, &row.album),
                confidence: Some(Confidence::Exact),
                selected_uri: Some(release_uri.clone()),
                candidates: vec![release.clone()],
                track_matches: BTreeMap::from([(
                    row.stable_id.clone(),
                    if explicit {
                        "spotify:track:manual".into()
                    } else {
                        release.track_uris[index].clone()
                    },
                )]),
            },
        );
    }
    (session, rows, release_uri)
}

#[tokio::test]
async fn activating_release_batch_is_local_idempotent_and_persisted() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    let (session, rows, release_uri) = selected_release_session();
    service.save(session).await.unwrap();
    let mappings = LastFmMappings {
        track_mappings: BTreeMap::from([(
            source_id("Artist", "Release", "Two"),
            "spotify:track:mapped".into(),
        )]),
        ..LastFmMappings::default()
    };

    service
        .activate_collection_batch(
            "user",
            "spotify",
            1,
            "Artist",
            "Release",
            &CollectionMembership::default(),
            &mappings,
        )
        .await
        .unwrap();
    let activated = service.snapshot().await.unwrap();
    let state = &activated.collection_album_matches[&1];
    assert_eq!(state.selected_album_uris, vec![release_uri.clone()]);
    assert_eq!(state.cached_candidates.len(), 1);
    assert!(!activated.page_options[&batch_options_key(1)].whole_album);
    assert_eq!(
        activated.matches[&rows[0].stable_id]
            .selected_uri
            .as_deref(),
        Some("spotify:track:manual")
    );
    assert_eq!(
        activated.matches[&rows[0].stable_id].track_matches[&rows[0].stable_id],
        "spotify:track:manual"
    );
    assert_eq!(
        activated.matches[&rows[1].stable_id].track_matches[&rows[1].stable_id],
        "spotify:track:mapped"
    );
    assert_eq!(
        activated.matches[&rows[2].stable_id].track_matches[&rows[2].stable_id],
        "spotify:track:three"
    );
    let page = service.page(1, "Artist", "Release").await.unwrap();
    assert!(page.collection.is_some());
    assert!(!page.options.whole_album);

    service
        .activate_collection_batch(
            "user",
            "spotify",
            1,
            "Artist",
            "Release",
            &CollectionMembership::default(),
            &mappings,
        )
        .await
        .unwrap();
    let retried = service.snapshot().await.unwrap();
    assert_eq!(
        retried.collection_album_matches[&1].selected_album_uris,
        vec![release_uri]
    );
    assert_eq!(
        retried.collection_album_matches[&1].cached_candidates.len(),
        1
    );
    let reloaded = Service::new(directory.path());
    assert_eq!(reloaded.snapshot().await.unwrap(), retried);
}

#[test]
fn converted_collection_activation_drops_release_candidates_and_album_targets() {
    let (mut session, rows, _) = selected_release_session();
    activate_collection_session(
        &mut session,
        1,
        "Artist",
        "Release",
        &CollectionMembership::default(),
        &LastFmMappings::default(),
    )
    .unwrap();
    assert!(session.matches.values().all(|result| {
        result
            .candidates
            .iter()
            .all(|candidate| !candidate.uri.starts_with("spotify:album:"))
    }));

    let source_id = rows[0].stable_id.clone();
    session
        .rows
        .iter_mut()
        .find(|row| row.stable_id == source_id)
        .unwrap()
        .track = "Seed Release".into();
    session
        .collection_album_matches
        .get_mut(&1)
        .unwrap()
        .selected_album_uris
        .clear();
    let result = session.matches.get_mut(&source_id).unwrap();
    result.selected_uri = None;
    result.track_matches.clear();
    rerank_collection_session(
        &mut session,
        1,
        &CollectionMembership::default(),
        &LastFmMappings::default(),
    )
    .unwrap();

    assert!(session.matches.values().all(|result| {
        result
            .track_matches
            .values()
            .all(|uri| uri.starts_with("spotify:track:"))
    }));
    assert!(session.matches[&source_id].track_matches.is_empty());
    let options = PageOptions {
        import_content: true,
        include_historical_play_counts: true,
        selected_track_ids: BTreeSet::from([source_id.clone()]),
        ..PageOptions::default()
    };
    assert!(build_apply_plan(
        &session,
        "spotify",
        1,
        "Artist",
        "Release",
        std::slice::from_ref(&source_id),
        false,
        options.clone(),
    )
    .unwrap_err()
    .contains("Every selected source track needs a supported Spotify match"));

    session.page_options.insert(batch_options_key(1), options);
    let batch = session.batches[0].clone();
    let rows_by_id = source_row_map(&session);
    let queue_rows = batch_rows(&batch, &rows_by_id);
    let queue = queue_item(&session, &batch, &queue_rows, &[]).unwrap();
    assert_eq!(queue.album_entities, 0);
    assert_eq!(queue.track_entities, 0);
}

#[tokio::test]
async fn converted_collection_rejects_release_match_write_atomically() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    let (session, rows, _) = selected_release_session();
    service.save(session).await.unwrap();
    service
        .activate_collection_batch(
            "user",
            "spotify",
            1,
            "Artist",
            "Release",
            &CollectionMembership::default(),
            &LastFmMappings::default(),
        )
        .await
        .unwrap();
    let before = service.snapshot().await.unwrap();
    let release = before.collection_album_matches[&1].cached_candidates[0]
        .matching
        .clone();
    let result = MatchResult {
        source_id: rows[0].stable_id.clone(),
        search_term: album_search_term("Artist", "Release"),
        confidence: Some(Confidence::Exact),
        selected_uri: Some(release.uri.clone()),
        candidates: vec![release],
        track_matches: BTreeMap::new(),
    };
    let error = service
        .set_matches("user", "spotify", 1, vec![result], None)
        .await
        .unwrap_err();
    assert!(error.contains("Release matching is unavailable"));
    assert_eq!(service.snapshot().await.unwrap(), before);
}

#[tokio::test]
async fn activation_rejects_wrong_account_and_stale_batch_atomically() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    let (session, _, _) = selected_release_session();
    service.save(session).await.unwrap();
    let before = service.snapshot().await.unwrap();
    let empty_membership = CollectionMembership::default();
    let empty_mappings = LastFmMappings::default();

    assert!(service
        .activate_collection_batch(
            "user",
            "other-spotify",
            1,
            "Artist",
            "Release",
            &empty_membership,
            &empty_mappings,
        )
        .await
        .is_err());
    assert_eq!(service.snapshot().await.unwrap(), before);

    assert!(service
        .activate_collection_batch(
            "user",
            "spotify",
            99,
            "Artist",
            "Release",
            &empty_membership,
            &empty_mappings,
        )
        .await
        .is_err());
    assert_eq!(service.snapshot().await.unwrap(), before);

    assert!(service
        .activate_collection_batch(
            "user",
            "spotify",
            1,
            "Artist",
            "Different Release",
            &empty_membership,
            &empty_mappings,
        )
        .await
        .is_err());
    assert_eq!(service.snapshot().await.unwrap(), before);
}

#[test]
fn native_empty_album_collection_keeps_whole_album_acceptance() {
    let rows = [
        collection_test_row("One"),
        collection_test_row("Two"),
        collection_test_row("Three"),
    ];
    let mut session = collection_session(&rows);
    session.batches = vec![ImportBatch {
        page: 1,
        source_ids: rows.iter().map(|row| row.stable_id.clone()).collect(),
        collection_shaped: None,
        representative_artist: None,
        representative_album: None,
        album_labels: Vec::new(),
    }];
    let album = collection_album(
        "spotify:album:native",
        "Artist",
        &[
            ("One", "spotify:track:one"),
            ("Two", "spotify:track:two"),
            ("Three", "spotify:track:three"),
        ],
    );
    session.collection_album_matches.insert(
        1,
        CollectionAlbumMatchState {
            cached_candidates: vec![album.clone()],
            selected_album_uris: vec![album.matching.uri.clone()],
            ..CollectionAlbumMatchState::default()
        },
    );
    session.page_options.insert(
        batch_options_key(1),
        PageOptions {
            import_content: true,
            include_historical_play_counts: true,
            whole_album: true,
            selected_track_ids: rows.iter().map(|row| row.stable_id.clone()).collect(),
            ..PageOptions::default()
        },
    );
    for (row, uri) in rows.iter().zip(album.matching.track_uris.iter()) {
        session
            .matches
            .get_mut(&row.stable_id)
            .unwrap()
            .track_matches = BTreeMap::from([(row.stable_id.clone(), uri.clone())]);
    }

    assert!(session.options_for_batch(1, "Artist", "").whole_album);
    let selected = rows
        .iter()
        .map(|row| row.stable_id.clone())
        .collect::<Vec<_>>();
    let plan = build_apply_plan(
        &session,
        "spotify",
        1,
        "Artist",
        "",
        &selected,
        false,
        session.options_for_batch(1, "Artist", ""),
    )
    .unwrap();
    assert_eq!(
        plan.membership,
        ApplyMembership::Album {
            uri: "spotify:album:native".into(),
            name: "native".into(),
            artist: "Artist".into(),
        }
    );
}

#[test]
fn converted_johnny_mathis_union_reranks_and_restores_ambiguities() {
    let source_album = "For Christmas";
    let artist = "Johnny Mathis";
    let titles = [
        "Seed Song One",
        "Seed Song Two",
        "Seed Song Three",
        "Seed Song Four",
        "Seed Song Five",
        "The Little Drummer Boy",
        "Have Yourself a Merry Little Christmas",
        "Do You Hear What I Hear?",
        "The Lord's Prayer",
        "Jingle Bell Rock",
        "Santa Claus Is Comin' To Town",
        "Calypso Noel",
    ];
    let rows = titles
        .into_iter()
        .map(|track| SourceRow {
            stable_id: source_id(artist, source_album, track),
            artist: artist.into(),
            album: source_album.into(),
            track: track.into(),
            variants: Vec::new(),
            play_count: 1,
            earliest: 1,
            latest: 1,
        })
        .collect::<Vec<_>>();
    let seed_uri = "spotify:album:6fZ4ArAO5YCnY8Zf8PE55G";
    let second_uri = "spotify:album:5BOyiCrvMOMgPFSiFzGQXV";
    let seed_tracks = [
        ("Seed Song One", "spotify:track:sounds-1"),
        ("Seed Song Two", "spotify:track:sounds-2"),
        ("Seed Song Three", "spotify:track:sounds-3"),
        ("Seed Song Four", "spotify:track:sounds-4"),
        ("Seed Song Five", "spotify:track:sounds-5"),
        ("The Little Drummer Boy", "spotify:track:sounds-drummer"),
        (
            "Have Yourself a Merry Little Christmas",
            "spotify:track:sounds-have-yourself",
        ),
    ];
    let second_tracks = [
        ("The Little Drummer Boy", "spotify:track:love-drummer"),
        (
            "Have Yourself a Merry Little Christmas",
            "spotify:track:love-have-yourself",
        ),
        ("Do You Hear What I Hear?", "spotify:track:love-do-you-hear"),
        ("The Lord's Prayer", "spotify:track:love-lords-prayer"),
        ("Jingle Bell Rock", "spotify:track:love-jingle-bell"),
        (
            "Santa Claus Is Comin' To Town",
            "spotify:track:love-santa-claus",
        ),
        ("Calypso Noel", "spotify:track:love-calypso"),
    ];
    let mut seed = collection_album(seed_uri, artist, &seed_tracks);
    seed.matching.name = "Sounds of Christmas".into();
    seed.matching.track_albums = vec![seed.matching.name.clone(); seed_tracks.len()];
    let mut second = collection_album(second_uri, artist, &second_tracks);
    second.matching.name = "Give Me Your Love For Christmas".into();
    second.matching.track_albums = vec![second.matching.name.clone(); second_tracks.len()];

    let mut session = collection_session(&rows);
    session.batches = vec![ImportBatch {
        page: 1,
        source_ids: rows.iter().map(|row| row.stable_id.clone()).collect(),
        collection_shaped: None,
        representative_artist: None,
        representative_album: None,
        album_labels: Vec::new(),
    }];
    session.collection_album_matches.insert(
        1,
        CollectionAlbumMatchState {
            cached_candidates: vec![seed.clone(), second.clone()],
            selected_album_uris: vec![seed_uri.into()],
            ..CollectionAlbumMatchState::default()
        },
    );
    let source_refs = rows.iter().collect::<Vec<_>>();
    let coverage =
        |session: &LastFmImportSessionV2| collection_match_view(session, 1, &source_refs).coverage;

    rerank_collection_session(
        &mut session,
        1,
        &CollectionMembership::default(),
        &LastFmMappings::default(),
    )
    .unwrap();
    let baseline = coverage(&session);
    assert_eq!(
        (baseline.matched, baseline.ambiguous, baseline.unresolved),
        (7, 0, 5)
    );

    session
        .collection_album_matches
        .get_mut(&1)
        .unwrap()
        .selected_album_uris = vec![seed_uri.into(), second_uri.into()];
    rerank_collection_session(
        &mut session,
        1,
        &CollectionMembership::default(),
        &LastFmMappings::default(),
    )
    .unwrap();
    let union = coverage(&session);
    assert_eq!(
        (union.matched, union.ambiguous, union.unresolved),
        (10, 2, 0)
    );
    for title in [
        "The Little Drummer Boy",
        "Have Yourself a Merry Little Christmas",
    ] {
        let result = &session.matches[&source_id(artist, source_album, title)];
        assert!(result.selected_uri.is_none());
        assert!(result.track_matches.is_empty());
    }

    session
        .collection_album_matches
        .get_mut(&1)
        .unwrap()
        .selected_album_uris = vec![seed_uri.into()];
    rerank_collection_session(
        &mut session,
        1,
        &CollectionMembership::default(),
        &LastFmMappings::default(),
    )
    .unwrap();
    let removed = coverage(&session);
    assert_eq!(
        (removed.matched, removed.ambiguous, removed.unresolved),
        (7, 0, 5)
    );

    session
        .collection_album_matches
        .get_mut(&1)
        .unwrap()
        .selected_album_uris = vec![seed_uri.into(), second_uri.into()];
    rerank_collection_session(
        &mut session,
        1,
        &CollectionMembership::default(),
        &LastFmMappings::default(),
    )
    .unwrap();
    let restored = coverage(&session);
    assert_eq!(
        (restored.matched, restored.ambiguous, restored.unresolved),
        (10, 2, 0)
    );

    let ambiguous_id = source_id(artist, source_album, "The Little Drummer Boy");
    let selected_ids = vec![ambiguous_id.clone()];
    let options = PageOptions {
        import_content: true,
        include_historical_play_counts: true,
        selected_track_ids: BTreeSet::from([ambiguous_id.clone()]),
        ..PageOptions::default()
    };
    assert!(build_apply_plan(
        &session,
        "spotify",
        1,
        artist,
        source_album,
        &selected_ids,
        false,
        options.clone(),
    )
    .unwrap_err()
    .contains("Every selected source track needs a supported Spotify match"));

    select_match_in_session(&mut session, 1, &ambiguous_id, "spotify:track:love-drummer").unwrap();
    let plan = build_apply_plan(
        &session,
        "spotify",
        1,
        artist,
        source_album,
        &selected_ids,
        false,
        options,
    )
    .unwrap();
    assert_eq!(plan.mappings[0].target_uri, "spotify:track:love-drummer");
}

#[test]
fn converted_collection_apply_rejects_whole_album_and_does_not_write_album_mapping() {
    let (mut session, rows, release_uri) = selected_release_session();
    activate_collection_session(
        &mut session,
        1,
        "Artist",
        "Release",
        &CollectionMembership::default(),
        &LastFmMappings::default(),
    )
    .unwrap();
    let selected = rows
        .iter()
        .map(|row| row.stable_id.clone())
        .collect::<Vec<_>>();
    let mut options = PageOptions {
        import_content: true,
        include_historical_play_counts: true,
        whole_album: false,
        ..PageOptions::default()
    };
    let plan = build_apply_plan(
        &session,
        "spotify",
        1,
        "Artist",
        "Release",
        &selected,
        false,
        options.clone(),
    )
    .unwrap();
    assert!(plan
        .mappings
        .iter()
        .all(|mapping| mapping.album_uri.is_none()));
    let ApplyMembership::Tracks(track_uris) = &plan.membership else {
        panic!("converted collection should import selected tracks");
    };
    assert_eq!(
        track_uris.iter().cloned().collect::<BTreeSet<_>>(),
        plan.mappings
            .iter()
            .map(|mapping| mapping.target_uri.clone())
            .collect()
    );
    options.whole_album = true;
    assert!(build_apply_plan(
        &session, "spotify", 1, "Artist", "Release", &selected, false, options,
    )
    .unwrap_err()
    .contains("Whole-album import is unavailable"));
    assert_eq!(
        session.collection_album_matches[&1].selected_album_uris,
        vec![release_uri]
    );
}

#[tokio::test]
async fn converted_collection_picker_accepts_cached_track_outside_row_candidates() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    let (session, rows, _) = selected_release_session();
    service.save(session).await.unwrap();
    service
        .activate_collection_batch(
            "user",
            "spotify",
            1,
            "Artist",
            "Release",
            &CollectionMembership::default(),
            &LastFmMappings::default(),
        )
        .await
        .unwrap();

    let source_id = rows[1].stable_id.clone();
    let mut converted = service.snapshot().await.unwrap();
    let result = converted.matches.get_mut(&source_id).unwrap();
    result.candidates.clear();
    result.selected_uri = None;
    result.track_matches.clear();
    service.save(converted).await.unwrap();

    service
        .select_match("user", "spotify", 1, &source_id, "spotify:track:two")
        .await
        .unwrap();
    let selected = service.snapshot().await.unwrap();
    let result = &selected.matches[&source_id];
    assert_eq!(result.selected_uri.as_deref(), Some("spotify:track:two"));
    assert_eq!(
        result.track_matches.get(&source_id).map(String::as_str),
        Some("spotify:track:two")
    );
    assert_eq!(
        Service::new(directory.path()).snapshot().await,
        Some(selected)
    );
}

fn event(artist: &str, album: &str, track: &str, timestamp: u64) -> ExternalScrobble {
    ExternalScrobble {
        artist: artist.into(),
        album: album.into(),
        track: track.into(),
        timestamp,
        submitted: None,
    }
}

fn quarantined_file(dir: &Path, prefix: &str) -> PathBuf {
    fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(prefix))
        })
        .expect("expected a quarantined persistence file")
}

#[tokio::test]
async fn corrupt_incremental_state_is_quarantined_before_fresh_state_is_saved() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lastfm-sync.json");
    fs::write(&path, b"not json").unwrap();

    let service = Service::new(dir.path());
    assert_eq!(
        service.state().await.sync_problem.as_deref(),
        Some("Last.fm sync state was quarantined; sync starts from now.")
    );
    let quarantined = quarantined_file(dir.path(), "lastfm-sync.json.quarantine-");
    assert_eq!(fs::read(quarantined).unwrap(), b"not json");
    assert!(!path.exists());

    service.mutate_sync(|_| Ok(())).await.unwrap();
    assert!(path.is_file());
    assert_ne!(fs::read(path).unwrap(), b"not json");
}

#[tokio::test]
async fn unsupported_incremental_state_is_quarantined_before_fresh_state_is_saved() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lastfm-sync.json");
    let unsupported = LastFmSyncState {
        version: LASTFM_SYNC_VERSION.saturating_add(1),
        ..LastFmSyncState::default()
    };
    let bytes = serde_json::to_vec(&unsupported).unwrap();
    fs::write(&path, &bytes).unwrap();

    let service = Service::new(dir.path());
    assert_eq!(
        service.state().await.sync_problem.as_deref(),
        Some("Last.fm sync state was quarantined; sync starts from now.")
    );
    let quarantined = quarantined_file(dir.path(), "lastfm-sync.json.quarantine-");
    assert_eq!(fs::read(quarantined).unwrap(), bytes);
    assert!(!path.exists());

    service.mutate_sync(|_| Ok(())).await.unwrap();
    assert!(path.is_file());
    assert_ne!(fs::read(path).unwrap(), bytes);
}

#[tokio::test]
async fn corrupt_mappings_are_quarantined_before_fresh_mappings_are_saved() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lastfm-mappings.json");
    fs::write(&path, b"not json").unwrap();

    let service = Service::new(dir.path());
    assert_eq!(
        service.state().await.sync_problem.as_deref(),
        Some("Last.fm mappings were quarantined; reusable decisions were reset.")
    );
    let quarantined = quarantined_file(dir.path(), "lastfm-mappings.json.quarantine-");
    assert_eq!(fs::read(quarantined).unwrap(), b"not json");
    assert!(!path.exists());

    service
        .save_mappings_for("user", Some("spotify"), LastFmMappings::default())
        .await
        .unwrap();
    assert!(path.is_file());
    assert_ne!(fs::read(path).unwrap(), b"not json");
}

#[tokio::test]
async fn unsupported_mappings_are_quarantined_before_fresh_mappings_are_saved() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lastfm-mappings.json");
    let unsupported = PersistedLastFmMappings {
        version: LASTFM_MAPPINGS_VERSION.saturating_add(1),
        ..PersistedLastFmMappings::default()
    };
    let bytes = serde_json::to_vec(&unsupported).unwrap();
    fs::write(&path, &bytes).unwrap();

    let service = Service::new(dir.path());
    assert_eq!(
        service.state().await.sync_problem.as_deref(),
        Some("Last.fm mappings were quarantined; reusable decisions were reset.")
    );
    let quarantined = quarantined_file(dir.path(), "lastfm-mappings.json.quarantine-");
    assert_eq!(fs::read(quarantined).unwrap(), bytes);
    assert!(!path.exists());

    service
        .save_mappings_for("user", Some("spotify"), LastFmMappings::default())
        .await
        .unwrap();
    assert!(path.is_file());
    assert_ne!(fs::read(path).unwrap(), bytes);
}

#[tokio::test]
async fn active_incremental_review_preserves_backlog_for_completion() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    let backlog = vec![event("Artist", "Album", "Song", 10)];
    service
        .mutate_sync(|state| {
            state.lastfm_username = Some("user".into());
            state.spotify_account_id = Some("spotify".into());
            state.active = Some(IncrementalRange {
                from: 10,
                to: 20,
                query_from: 9,
                query_to: 21,
                cache_id: incremental_cache_id("user", 10, 20),
                next_page: 1,
                ..IncrementalRange::default()
            });
            state.backlog = backlog.clone();
            Ok(())
        })
        .await
        .unwrap();

    service
        .sync_backlog_into_review("user", Some("spotify"))
        .await
        .unwrap();

    let state = service.sync_snapshot().await;
    assert_eq!(state.backlog, backlog);
    assert!(state.active.is_some());
    assert_eq!(
        service
            .snapshot()
            .await
            .unwrap()
            .incremental_source_keys
            .len(),
        1
    );
}

#[tokio::test]
async fn bulk_exclusion_is_atomic_and_persists_reusable_mappings() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let rows = [
        collection_test_row("One"),
        collection_test_row("Two"),
        collection_test_row("Three"),
    ];
    let mut session = collection_session(&rows);
    session.batches = vec![
        ImportBatch {
            page: 1,
            source_ids: vec![rows[0].stable_id.clone(), rows[1].stable_id.clone()],
            collection_shaped: None,
            representative_artist: None,
            representative_album: None,
            album_labels: Vec::new(),
        },
        ImportBatch {
            page: 2,
            source_ids: vec![rows[2].stable_id.clone()],
            collection_shaped: None,
            representative_artist: None,
            representative_album: None,
            album_labels: Vec::new(),
        },
    ];
    service.save(session.clone()).await.unwrap();

    let ids = vec![
        rows[0].stable_id.clone(),
        rows[1].stable_id.clone(),
        rows[0].stable_id.clone(),
    ];
    service
        .review_action(
            "user",
            "spotify",
            1,
            Some(ids.as_slice()),
            ReviewAction::Exclude,
            "Artist",
            "",
        )
        .await
        .unwrap();
    let saved = service.snapshot().await.unwrap();
    assert!(saved.decisions[&rows[0].stable_id].excluded);
    assert!(saved.decisions[&rows[1].stable_id].excluded);
    assert!(!default_decision(&saved, &rows[2].stable_id).excluded);
    let expected_exclusions =
        BTreeSet::from([rows[0].stable_id.clone(), rows[1].stable_id.clone()]);
    assert_eq!(
        service.export_mappings().await.mappings.excluded_tracks,
        expected_exclusions
    );
    let queue = service
        .queue_page(0, LASTFM_QUEUE_PAGE_LIMIT)
        .await
        .unwrap();
    assert_eq!(queue.items[0].status, Some(QueueStatus::Excluded));
    assert!(!queue.items[0].remaining);

    let reloaded = Service::new(dir.path());
    let reloaded_session = reloaded.snapshot().await.unwrap();
    assert!(reloaded_session.decisions[&rows[0].stable_id].excluded);
    assert!(reloaded_session.decisions[&rows[1].stable_id].excluded);
    assert_eq!(
        reloaded.export_mappings().await.mappings.excluded_tracks,
        expected_exclusions
    );

    let mut reset = saved.clone();
    reset.decisions.clear();
    service.save(reset.clone()).await.unwrap();
    let oversized = vec![rows[0].stable_id.clone(); LASTFM_REVIEW_BATCH_SIZE + 1];
    let oversized_error = service
        .review_action(
            "user",
            "spotify",
            1,
            Some(oversized.as_slice()),
            ReviewAction::Exclude,
            "Artist",
            "",
        )
        .await
        .unwrap_err();
    assert_eq!(
        oversized_error,
        format!(
            "A Last.fm review action accepts at most {LASTFM_REVIEW_BATCH_SIZE} source row IDs."
        )
    );
    assert!(!service
        .snapshot()
        .await
        .unwrap()
        .decisions
        .values()
        .any(|decision| decision.excluded));
    assert_eq!(
        service.export_mappings().await.mappings.excluded_tracks,
        expected_exclusions
    );
    assert!(service
        .review_action(
            "user",
            "spotify",
            1,
            Some(&[]),
            ReviewAction::Exclude,
            "Artist",
            "",
        )
        .await
        .is_err());
    assert!(!service
        .snapshot()
        .await
        .unwrap()
        .decisions
        .values()
        .any(|decision| decision.excluded));
    assert_eq!(
        service.export_mappings().await.mappings.excluded_tracks,
        expected_exclusions
    );
    assert!(service
        .review_action(
            "user",
            "spotify",
            1,
            Some(&[rows[0].stable_id.clone(), rows[2].stable_id.clone()]),
            ReviewAction::Exclude,
            "Artist",
            "",
        )
        .await
        .is_err());
    assert!(!service
        .snapshot()
        .await
        .unwrap()
        .decisions
        .values()
        .any(|decision| decision.excluded));
    assert_eq!(
        service.export_mappings().await.mappings.excluded_tracks,
        expected_exclusions
    );

    let mut nonreviewable = reset;
    nonreviewable.decisions.insert(
        rows[1].stable_id.clone(),
        RowDecision {
            status: RowStatus::Done,
            excluded: false,
        },
    );
    service.save(nonreviewable).await.unwrap();
    assert!(service
        .review_action(
            "user",
            "spotify",
            1,
            Some(&[rows[0].stable_id.clone(), rows[1].stable_id.clone()]),
            ReviewAction::Exclude,
            "Artist",
            "",
        )
        .await
        .is_err());
    assert!(!service
        .snapshot()
        .await
        .unwrap()
        .decisions
        .values()
        .any(|decision| decision.excluded));
    assert_eq!(
        service.export_mappings().await.mappings.excluded_tracks,
        expected_exclusions
    );

    reloaded
        .review_action(
            "user",
            "spotify",
            1,
            Some(&[rows[0].stable_id.clone(), rows[1].stable_id.clone()]),
            ReviewAction::UndoExclude,
            "Artist",
            "",
        )
        .await
        .unwrap();
    let undone = reloaded.snapshot().await.unwrap();
    assert!(!undone.decisions[&rows[0].stable_id].excluded);
    assert!(!undone.decisions[&rows[1].stable_id].excluded);
    assert!(reloaded
        .export_mappings()
        .await
        .mappings
        .excluded_tracks
        .is_empty());
    let reloaded_after_undo = Service::new(dir.path());
    let undone_after_reload = reloaded_after_undo.snapshot().await.unwrap();
    assert!(!undone_after_reload.decisions[&rows[0].stable_id].excluded);
    assert!(!undone_after_reload.decisions[&rows[1].stable_id].excluded);
    assert!(reloaded_after_undo
        .export_mappings()
        .await
        .mappings
        .excluded_tracks
        .is_empty());
}

fn receipt(
    corrected: (&str, &str, &str),
    submitted: (&str, &str, &str),
    timestamp: u64,
) -> AcceptedScrobbleReceipt {
    AcceptedScrobbleReceipt {
        corrected: ScrobbleMetadata {
            artist: corrected.0.into(),
            album: corrected.1.into(),
            track: corrected.2.into(),
        },
        submitted: ScrobbleMetadata {
            artist: submitted.0.into(),
            album: submitted.1.into(),
            track: submitted.2.into(),
        },
        timestamp,
    }
}

fn seed_lastfm_files(directory: &Path, accepted: &[AcceptedScrobbleReceipt]) {
    fs::write(
        directory.join("dev-lastfm-session.json"),
        br#"{"username":"user","key":"session"}"#,
    )
    .unwrap();
    fs::write(
        directory.join("lastfm-scrobbles.json"),
        serde_json::to_vec(&serde_json::json!({
            "version": 2,
            "pending": [],
            "accepted": accepted,
            "owner": "user"
        }))
        .unwrap(),
    )
    .unwrap();
}

fn library_with_count(play_count: u32) -> Library {
    let mut library = Library::new();
    library.add(retune_core::model::NewTrack {
        uri: "spotify:track:one".into(),
        name: "Song".into(),
        source: SourceId::Music,
        ..retune_core::model::NewTrack::default()
    });
    library.merge_history_absolute("spotify:track:one", Some(play_count as u64), None, None);
    library
}

fn recent_tracks_response(page: u32, total_pages: u32, entries: Value) -> Value {
    serde_json::json!({
        "recenttracks": {
            "track": entries,
            "@attr": {
                "page": page.to_string(),
                "totalPages": total_pages.to_string(),
                "total": "0"
            }
        }
    })
}

fn test_app_state(
    directory: &Path,
    library: Library,
    accepted: &[AcceptedScrobbleReceipt],
) -> (Arc<crate::lastfm::Service>, Arc<Service>, crate::AppState) {
    seed_lastfm_files(directory, accepted);
    let lastfm = crate::lastfm::Service::new_for_test(directory, true, false);
    let service = Service::new(directory);
    let state = crate::test_app_state(
        directory,
        library,
        SpotifyLibraryState::default(),
        Arc::clone(&lastfm),
        Arc::clone(&service),
    );
    (lastfm, service, state)
}

fn test_app_state_with_lastfm_executor(
    directory: &Path,
    library: Library,
) -> (
    Arc<crate::lastfm::Service>,
    Arc<crate::lastfm::FakeRequestExecutor>,
    Arc<Service>,
    crate::AppState,
) {
    seed_lastfm_files(directory, &[]);
    let (lastfm, executor) = crate::lastfm::Service::new_with_fake_executor(directory, true, false);
    let service = Service::new(directory);
    let state = crate::test_app_state(
        directory,
        library,
        SpotifyLibraryState::default(),
        Arc::clone(&lastfm),
        Arc::clone(&service),
    );
    (lastfm, executor, service, state)
}

#[tokio::test]
async fn review_use_case_persists_before_returning_its_view() {
    let directory = tempfile::tempdir().unwrap();
    let (lastfm, service, state) = test_app_state(directory.path(), Library::new(), &[]);
    state.spotify_membership.set_for_test(SpotifyLibraryState {
        account_id: "spotify".into(),
        complete: true,
        ..SpotifyLibraryState::default()
    });
    let mut session =
        LastFmImportSessionV2::new_with_defaults("user".into(), 10, ImportDefaults::default());
    aggregate_scrobbles(&mut session.rows, &[scrobble("Artist", "Album", "Song", 1)]);
    session.phase = ImportPhase::Review;
    session.spotify_account_id = Some("spotify".into());
    session.batches = build_review_batches(&session.rows);
    let id = session.rows[0].stable_id.clone();
    service.save(session).await.unwrap();

    let view = review_import(
        service.as_ref(),
        lastfm.as_ref(),
        &state.spotify_membership,
        &state.library,
        &|| Err::<Arc<crate::SpotifyProvider>, _>("provider should not be needed".into()),
        || Ok(true),
        ReviewBatchKey {
            batch_id: 1,
            artist: "Artist".into(),
            album: "Album".into(),
        },
        Some(std::slice::from_ref(&id)),
        ReviewAction::Exclude,
    )
    .await
    .unwrap();

    assert_eq!(view.pending_review, 0);
    let reloaded = Service::new(directory.path()).snapshot().await.unwrap();
    assert!(reloaded.decisions[&id].excluded);
}

#[tokio::test]
async fn count_mode_use_case_persists_session_and_reusable_mapping_default() {
    let directory = tempfile::tempdir().unwrap();
    let (lastfm, service, state) = test_app_state(directory.path(), Library::new(), &[]);
    state.spotify_membership.set_for_test(SpotifyLibraryState {
        account_id: "spotify".into(),
        complete: true,
        ..SpotifyLibraryState::default()
    });
    let mut session =
        LastFmImportSessionV2::new_with_defaults("user".into(), 10, ImportDefaults::default());
    aggregate_scrobbles(&mut session.rows, &[scrobble("Artist", "Album", "Song", 1)]);
    session.phase = ImportPhase::Review;
    session.spotify_account_id = Some("spotify".into());
    session.batches = build_review_batches(&session.rows);
    service.save(session).await.unwrap();

    let view = update_import_count_mode(
        service.as_ref(),
        lastfm.as_ref(),
        &state.spotify_membership,
        &|| Err::<Arc<crate::SpotifyProvider>, _>("provider should not be needed".into()),
        || Ok(true),
        "spotify:track:one",
        CountMode::Overwrite,
    )
    .await
    .unwrap();

    assert_eq!(view.phase, Some(ImportPhase::Review));
    let reloaded = Service::new(directory.path());
    assert_eq!(
        reloaded.snapshot().await.unwrap().default_count_mode,
        CountMode::Overwrite
    );
    assert_eq!(
        reloaded
            .mappings_for("user", Some("spotify"))
            .await
            .unwrap()
            .default_count_mode,
        CountMode::Overwrite
    );
}

#[tokio::test]
async fn account_binding_resolves_provider_after_membership_gate_acquisition() {
    let directory = tempfile::tempdir().unwrap();
    let (lastfm, service, state) = test_app_state(directory.path(), Library::new(), &[]);
    state.spotify_membership.set_for_test(SpotifyLibraryState {
        account_id: "spotify".into(),
        complete: true,
        ..SpotifyLibraryState::default()
    });
    let mut session =
        LastFmImportSessionV2::new_with_defaults("user".into(), 10, ImportDefaults::default());
    session.phase = ImportPhase::Review;
    session.spotify_account_id = Some("spotify".into());
    service.save(session).await.unwrap();
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let provider = || {
        calls.fetch_add(1, Ordering::SeqCst);
        Err::<Arc<crate::SpotifyProvider>, _>("provider resolved after gate".into())
    };

    let gate = state.spotify_membership.lock().await;
    let mut pending = Box::pin(update_import_search_terms(
        service.as_ref(),
        lastfm.as_ref(),
        &state.spotify_membership,
        &provider,
        || Ok(true),
        false,
    ));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), &mut pending)
            .await
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    state.spotify_membership.set_for_test(SpotifyLibraryState {
        account_id: "spotify".into(),
        complete: false,
        ..SpotifyLibraryState::default()
    });
    drop(gate);

    assert_eq!(pending.await.unwrap_err(), "provider resolved after gate");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn collection_removal_use_case_persists_before_returning_output() {
    let directory = tempfile::tempdir().unwrap();
    let (lastfm, service, state) = test_app_state(directory.path(), Library::new(), &[]);
    state.spotify_membership.set_for_test(SpotifyLibraryState {
        account_id: "spotify".into(),
        complete: true,
        ..SpotifyLibraryState::default()
    });
    let candidate = collection_album(
        "spotify:album:album",
        "Artist",
        &[("One", "spotify:track:one")],
    );
    service
        .save(collection_session(&[collection_test_row("One")]))
        .await
        .unwrap();
    service
        .cache_collection_album("user", "spotify", 1, "Artist", candidate.clone())
        .await
        .unwrap();
    service
        .add_collection_album(
            "user",
            "spotify",
            1,
            "Artist",
            &candidate.matching.uri,
            None,
            &CollectionMembership::default(),
            &LastFmMappings::default(),
        )
        .await
        .unwrap();

    let (page, _) = remove_collection_album(
        service.as_ref(),
        lastfm.as_ref(),
        &state.spotify_membership,
        &state.library,
        &|| Err::<Arc<crate::SpotifyProvider>, _>("provider should not be needed".into()),
        || Ok(true),
        1,
        "Artist",
        &candidate.matching.uri,
    )
    .await
    .unwrap();

    assert!(page
        .unwrap()
        .collection
        .unwrap()
        .selected_album_uris
        .is_empty());
    assert!(Service::new(directory.path())
        .snapshot()
        .await
        .unwrap()
        .collection_album_matches[&1]
        .selected_album_uris
        .is_empty());
}

#[test]
fn remove_snapshot_is_idempotent_and_surfaces_non_directory_targets() {
    let directory = tempfile::tempdir().unwrap();
    let store = ImportSessionStore::new(directory.path());

    store.remove_snapshot("missing").unwrap();

    let snapshot = store.cache_path("complete").unwrap();
    fs::create_dir_all(&snapshot).unwrap();
    fs::write(snapshot.join("page-1.json"), b"cached").unwrap();
    store.remove_snapshot("complete").unwrap();
    assert!(!snapshot.exists());
    store.remove_snapshot("complete").unwrap();

    fs::create_dir_all(&store.cache_root).unwrap();
    let not_a_directory = store.cache_path("not-a-directory").unwrap();
    fs::write(&not_a_directory, b"cached").unwrap();
    let error = store.remove_snapshot("not-a-directory").unwrap_err();
    assert_ne!(error.kind(), std::io::ErrorKind::NotFound);
    assert!(not_a_directory.is_file());
}

#[tokio::test(flavor = "current_thread")]
async fn aborted_sync_state_save_finishes_disk_and_memory_publication() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    let hook = crate::store::SaveHook::new(false);
    service.incremental_store.arm_save(Arc::clone(&hook));
    let mutation = {
        let service = Arc::clone(&service);
        tokio::spawn(async move {
            service
                .mutate_sync(|state| {
                    state.sync_problem = Some("after".into());
                    Ok(())
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
    assert!(!mutation.is_finished());
    assert_ne!(
        service.sync_snapshot().await.sync_problem.as_deref(),
        Some("after")
    );

    mutation.abort();
    hook.release();
    assert!(mutation.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if service.sync_snapshot().await.sync_problem.as_deref() == Some("after") {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        service.incremental_store.load().unwrap(),
        service.sync_snapshot().await
    );
}

#[tokio::test(flavor = "current_thread")]
async fn aborted_import_session_save_finishes_disk_and_memory_publication() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 10);
    session.phase = ImportPhase::Review;
    session.search_terms = false;
    service.save(session).await.unwrap();
    let hook = crate::store::SaveHook::new(false);
    service.store.arm_save(Arc::clone(&hook));
    let mutation = {
        let service = Arc::clone(&service);
        tokio::spawn(async move {
            service
                .mutate_session(|session| {
                    let mut session = session.unwrap();
                    session.search_terms = true;
                    Ok((Some(session), ()))
                })
                .await
        })
    };
    while !hook.is_reached() {
        tokio::task::yield_now().await;
    }
    hook.wait_until_reached();
    assert!(!service.snapshot().await.unwrap().search_terms);

    mutation.abort();
    hook.release();
    assert!(mutation.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(1), async {
        while !service.snapshot().await.unwrap().search_terms {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    service
        .set_search_terms("user", "spotify", true)
        .await
        .unwrap();
    assert!(
        ImportSessionStore::new(directory.path())
            .load()
            .unwrap()
            .unwrap()
            .search_terms
    );
}

#[tokio::test(flavor = "current_thread")]
async fn aborted_mapping_save_finishes_disk_and_memory_publication() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    let hook = crate::store::SaveHook::new(false);
    service.mappings_store.arm_save(Arc::clone(&hook));
    let mutation = {
        let service = Arc::clone(&service);
        tokio::spawn(async move {
            service
                .save_mappings_for(
                    "user",
                    Some("spotify"),
                    LastFmMappings {
                        default_count_mode: CountMode::Zero,
                        ..LastFmMappings::default()
                    },
                )
                .await
        })
    };
    while !hook.is_reached() {
        tokio::task::yield_now().await;
    }
    hook.wait_until_reached();
    assert_eq!(
        service.export_mappings().await.mappings.default_count_mode,
        CountMode::Sum
    );

    mutation.abort();
    hook.release();
    assert!(mutation.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(1), async {
        while service.export_mappings().await.mappings.default_count_mode != CountMode::Zero {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        MappingsStore::new(directory.path())
            .load()
            .unwrap()
            .mappings
            .default_count_mode,
        CountMode::Zero
    );
}

#[tokio::test(flavor = "current_thread")]
async fn aborted_dormant_mapping_activation_finishes_disk_and_memory_publication() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    let dormant = PersistedLastFmMappings {
        version: LASTFM_MAPPINGS_VERSION,
        lastfm_username: Some("user".into()),
        spotify_account_id: Some("spotify".into()),
        dormant: true,
        mappings: LastFmMappings::default(),
    };
    service.mappings_store.save(&dormant).unwrap();
    *service.mappings.lock().await = dormant;
    let hook = crate::store::SaveHook::new(false);
    service.mappings_store.arm_save(Arc::clone(&hook));
    let activation = {
        let service = Arc::clone(&service);
        tokio::spawn(async move { service.mappings_for("user", Some("spotify")).await })
    };
    while !hook.is_reached() {
        tokio::task::yield_now().await;
    }
    hook.wait_until_reached();
    assert!(service.export_mappings().await.dormant);

    activation.abort();
    hook.release();
    assert!(activation.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(1), async {
        while service.export_mappings().await.dormant {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!MappingsStore::new(directory.path()).load().unwrap().dormant);
}

#[tokio::test]
async fn incremental_account_switch_keeps_old_identity_when_cache_cleanup_fails() {
    let directory = tempfile::tempdir().unwrap();
    let (_lastfm, _executor, service, state) =
        test_app_state_with_lastfm_executor(directory.path(), Library::new());
    let cache_id = incremental_cache_id("old-user", 10, 20);
    service
        .mutate_sync(|sync| {
            sync.lastfm_username = Some("old-user".into());
            sync.synced_through = Some(10);
            sync.active = Some(IncrementalRange {
                cache_id: cache_id.clone(),
                from: 10,
                to: 20,
                next_page: 1,
                ..IncrementalRange::default()
            });
            Ok(())
        })
        .await
        .unwrap();
    let before = service.sync_snapshot().await;
    fs::create_dir_all(&service.store.cache_root).unwrap();
    fs::write(
        service.store.cache_path(&cache_id).unwrap(),
        b"not a directory",
    )
    .unwrap();

    let error = run_incremental_sync(
        &state.library,
        &state.spotify_membership,
        &state.lastfm,
        &service,
        "new-user",
    )
    .await
    .unwrap_err();

    assert!(error.contains("Could not remove the previous Last.fm account cache"));
    assert_eq!(service.sync_snapshot().await, before);
    assert_eq!(Service::new(directory.path()).sync_snapshot().await, before);
}

#[tokio::test]
async fn apply_completed_incremental_range_persists_and_prunes_once() {
    let directory = tempfile::tempdir().unwrap();
    let accepted = receipt(("Artist", "Album", "Song"), ("Artist", "Album", "Song"), 12);
    let (lastfm, service, state) = test_app_state(
        directory.path(),
        library_with_count(100),
        std::slice::from_ref(&accepted),
    );
    service
        .save_mappings_for(
            "user",
            None,
            LastFmMappings {
                track_mappings: BTreeMap::from([(
                    source_id("Artist", "Album", "Song"),
                    "spotify:track:one".into(),
                )]),
                ..LastFmMappings::default()
            },
        )
        .await
        .unwrap();
    service
        .mutate_sync(|sync| {
            sync.lastfm_username = Some("user".into());
            sync.synced_through = Some(10);
            sync.active = Some(IncrementalRange {
                from: 10,
                to: 20,
                query_from: 9,
                query_to: 21,
                cache_id: incremental_cache_id("user", 10, 20),
                next_page: 1,
                total_pages: Some(1),
                ..IncrementalRange::default()
            });
            Ok(())
        })
        .await
        .unwrap();
    service
        .checkpoint_incremental_page(
            "user",
            1,
            parsed_page(
                1,
                1,
                vec![
                    scrobble("Artist", "Album", "Song", 11),
                    scrobble("Artist", "Album", "Song", 12),
                ],
            ),
        )
        .await
        .unwrap();

    apply_completed_incremental_range(
        &state.library,
        &state.lastfm,
        &state.spotify_membership,
        &service,
        "user",
    )
    .await
    .unwrap();

    assert_eq!(state.library.lock().unwrap().tracks()[0].play_count, 101);
    let sync = service.sync_snapshot().await;
    assert_eq!(sync.synced_through, Some(20));
    assert!(sync.active.is_none());
    assert!(sync.journal.is_none());
    assert!(sync.backlog.is_empty());
    assert!(lastfm.accepted_receipts().await.is_empty());
    let ledger: Value =
        serde_json::from_slice(&fs::read(directory.path().join("lastfm-scrobbles.json")).unwrap())
            .unwrap();
    assert!(ledger["accepted"].as_array().unwrap().is_empty());
}

async fn recover_persisted_journal(library: Library) -> (Library, Value) {
    let directory = tempfile::tempdir().unwrap();
    let accepted = receipt(("Artist", "Album", "Song"), ("Artist", "Album", "Song"), 20);
    let (lastfm, service, state) =
        test_app_state(directory.path(), library, std::slice::from_ref(&accepted));
    let before = library_with_count(100);
    let mut after = before.clone();
    apply_incremental_updates(
        &mut after,
        &BTreeMap::from([(String::from("spotify:track:one"), 1)]),
        &BTreeMap::from([(String::from("spotify:track:one"), 19)]),
    );
    let backlog_before = vec![event("Artist", "Album", "Song", 10)];
    service
        .mutate_sync(|sync| {
            sync.lastfm_username = Some("user".into());
            sync.synced_through = Some(10);
            sync.active = Some(IncrementalRange {
                from: 10,
                to: 20,
                cache_id: incremental_cache_id("user", 10, 20),
                next_page: 0,
                ..IncrementalRange::default()
            });
            sync.backlog = backlog_before.clone();
            sync.journal = Some(LastFmApplicationJournal {
                before_library: before.clone(),
                after_library: after.clone(),
                checkpoint_before: Some(10),
                checkpoint_after: Some(20),
                backlog_before,
                backlog_after: Vec::new(),
                consumed_receipts: vec![accepted],
            });
            Ok(())
        })
        .await
        .unwrap();

    recover_pending_incremental_journal(
        &state.library,
        &state.lastfm,
        &state.spotify_membership,
        &service,
    )
    .await
    .unwrap();
    let sync = service.sync_snapshot().await;
    assert_eq!(sync.synced_through, Some(20));
    assert!(sync.active.is_none());
    assert!(sync.journal.is_none());
    assert!(sync.backlog.is_empty());
    assert!(lastfm.accepted_receipts().await.is_empty());

    recover_pending_incremental_journal(
        &state.library,
        &state.lastfm,
        &state.spotify_membership,
        &service,
    )
    .await
    .unwrap();
    let ledger: Value =
        serde_json::from_slice(&fs::read(directory.path().join("lastfm-scrobbles.json")).unwrap())
            .unwrap();
    let library = state.library.lock().unwrap().clone();
    (library, ledger)
}

#[tokio::test]
async fn recover_pending_incremental_journal_applies_library_before_exactly_once() {
    let (library, ledger) = recover_persisted_journal(library_with_count(100)).await;
    assert_eq!(library.tracks()[0].play_count, 101);
    assert!(ledger["accepted"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn recover_pending_incremental_journal_finalizes_library_after_exactly_once() {
    let mut after = library_with_count(100);
    apply_incremental_updates(
        &mut after,
        &BTreeMap::from([(String::from("spotify:track:one"), 1)]),
        &BTreeMap::from([(String::from("spotify:track:one"), 19)]),
    );
    let (library, ledger) = recover_persisted_journal(after).await;
    assert_eq!(library.tracks()[0].play_count, 101);
    assert!(ledger["accepted"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn incremental_runner_activates_at_now_then_downloads_padded_range_without_spotify() {
    let directory = tempfile::tempdir().unwrap();
    let (_lastfm, executor, service, state) =
        test_app_state_with_lastfm_executor(directory.path(), Library::new());

    run_incremental_sync(
        &state.library,
        &state.spotify_membership,
        &state.lastfm,
        &service,
        "user",
    )
    .await
    .unwrap();
    let first = service.sync_snapshot().await;
    assert!(first.synced_through.is_some());
    assert_eq!(first.last_synced_at, first.synced_through);
    assert!(executor.requests().is_empty());

    let from = crate::unix_now().saturating_sub(100);
    service
        .mutate_sync(|sync| {
            sync.synced_through = Some(from);
            sync.last_synced_at = None;
            sync.active = None;
            Ok(())
        })
        .await
        .unwrap();
    let empty_page = recent_tracks_response(1, 1, serde_json::json!([]));
    executor.queue_json(empty_page.clone());
    executor.queue_json(empty_page);

    run_incremental_sync(
        &state.library,
        &state.spotify_membership,
        &state.lastfm,
        &service,
        "user",
    )
    .await
    .unwrap();

    let requests = executor.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|params| {
        params
            .iter()
            .any(|(key, value)| key == "method" && value == "user.getRecentTracks")
    }));
    for params in &requests {
        assert_eq!(
            params.iter().find(|(key, _)| key == "from").unwrap().1,
            (from - 1).to_string()
        );
    }
    let query_to = requests[0]
        .iter()
        .find(|(key, _)| key == "to")
        .unwrap()
        .1
        .parse::<u64>()
        .unwrap();
    assert!(requests.iter().all(|params| {
        params.iter().find(|(key, _)| key == "to").unwrap().1 == query_to.to_string()
    }));
    let final_state = service.sync_snapshot().await;
    assert_eq!(final_state.synced_through, Some(query_to - 1));
    assert!(final_state.active.is_none());
}

#[tokio::test]
async fn restored_mappings_activate_only_for_the_exact_backup_identities() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    let imported = PersistedLastFmMappings {
        version: LASTFM_MAPPINGS_VERSION,
        lastfm_username: Some("user".into()),
        spotify_account_id: Some("spotify-a".into()),
        dormant: false,
        mappings: LastFmMappings {
            track_mappings: BTreeMap::from([(
                source_id("Artist", "Album", "Song"),
                "spotify:track:one".into(),
            )]),
            ..LastFmMappings::default()
        },
    };
    service
        .begin_mappings_restore()
        .await
        .unwrap()
        .replace(normalize_restored_mappings(imported).unwrap())
        .unwrap();

    assert!(service
        .mappings_for("other", Some("spotify-a"))
        .await
        .unwrap()
        .track_mappings
        .is_empty());
    assert!(service
        .mappings_for("user", Some("spotify-b"))
        .await
        .unwrap()
        .track_mappings
        .is_empty());
    assert!(service.export_mappings().await.dormant);

    let active = service
        .mappings_for("user", Some("spotify-a"))
        .await
        .unwrap();
    let source_key = source_id("Artist", "Album", "Song");
    assert_eq!(active.track_mappings[&source_key], "spotify:track:one");
    assert!(!service.export_mappings().await.dormant);
    assert_eq!(
        service.mappings_for("user", Some("spotify-a")).await,
        Ok(active)
    );
}

#[test]
fn reconciliation_receipts_are_matched_and_consumed_as_a_multiset() {
    let mappings = LastFmMappings {
        track_mappings: BTreeMap::from([(
            source_id("Artist", "Album", "Song"),
            "spotify:track:one".into(),
        )]),
        ..LastFmMappings::default()
    };
    let events = vec![event("Artist", "Album", "Song", 10); 2];
    let one = reconcile_incremental(
        &events,
        &[receipt(
            ("Corrected", "Album", "Song"),
            ("Artist", "Album", "Song"),
            10,
        )],
        &mappings,
        &BTreeSet::from(["spotify:track:one".into()]),
        0,
        20,
    );
    assert_eq!(one.increments["spotify:track:one"], 1);
    assert_eq!(one.consumed_receipts.len(), 1);

    let two = reconcile_incremental(
        &events,
        &[
            receipt(
                ("Corrected", "Album", "Song"),
                ("Artist", "Album", "Song"),
                10,
            ),
            receipt(
                ("Corrected", "Album", "Song"),
                ("Artist", "Album", "Song"),
                10,
            ),
        ],
        &mappings,
        &BTreeSet::from(["spotify:track:one".into()]),
        0,
        20,
    );
    assert!(two.increments.is_empty());
    assert_eq!(two.consumed_receipts.len(), 2);
}

#[test]
fn reconciliation_matches_corrected_and_submitted_metadata() {
    let mappings = LastFmMappings {
        track_mappings: BTreeMap::from([(
            source_id("Artist", "Album", "Song"),
            "spotify:track:one".into(),
        )]),
        ..LastFmMappings::default()
    };
    let result = reconcile_incremental(
        &[ExternalScrobble {
            artist: "Corrected Artist".into(),
            album: "Corrected Album".into(),
            track: "Corrected Song".into(),
            timestamp: 10,
            submitted: Some(ScrobbleMetadata {
                artist: "Artist".into(),
                album: "Album".into(),
                track: "Song".into(),
            }),
        }],
        &[receipt(
            ("Corrected Artist", "Corrected Album", "Corrected Song"),
            ("Artist", "Album", "Song"),
            10,
        )],
        &mappings,
        &BTreeSet::from(["spotify:track:one".into()]),
        0,
        20,
    );
    assert!(result.increments.is_empty());
    assert_eq!(result.consumed_receipts.len(), 1);
}

#[test]
fn reconciliation_prefers_track_mapping_and_sums_aliases() {
    let mappings = LastFmMappings {
        track_mappings: BTreeMap::from([
            (
                source_id("Artist", "Album", "Song"),
                "spotify:track:explicit".into(),
            ),
            (
                source_id("Artist", "Album", "Song (Live)"),
                "spotify:track:shared".into(),
            ),
        ]),
        album_mappings: BTreeMap::from([(
            source_album_key("Artist", "Album"),
            LastFmAlbumMapping {
                spotify_album_uri: "spotify:album:one".into(),
                track_uris_by_name: BTreeMap::from([(
                    normalize_for_match("Song"),
                    "spotify:track:album".into(),
                )]),
            },
        )]),
        ..LastFmMappings::default()
    };
    let result = reconcile_incremental(
        &[
            event("Artist", "Album", "Song", 10),
            event("Artist", "Album", "Song (Live)", 20),
            event("Artist", "Album", "Song (Live)", 30),
        ],
        &[],
        &mappings,
        &BTreeSet::from([
            "spotify:track:explicit".into(),
            "spotify:track:shared".into(),
        ]),
        0,
        40,
    );
    assert_eq!(result.increments["spotify:track:explicit"], 1);
    assert_eq!(result.increments["spotify:track:shared"], 2);
    assert_eq!(result.latest["spotify:track:shared"], 30);

    let album_only = LastFmMappings {
        album_mappings: mappings.album_mappings.clone(),
        ..LastFmMappings::default()
    };
    let album_result = reconcile_incremental(
        &[event("Artist", "Album", "Song", 10)],
        &[],
        &album_only,
        &BTreeSet::from(["spotify:track:album".into()]),
        0,
        20,
    );
    assert_eq!(album_result.increments["spotify:track:album"], 1);
}

#[test]
fn reconciliation_ignores_and_unresolved_targets_are_independent() {
    let known = source_id("Known", "Album", "Song");
    let mappings = LastFmMappings {
        track_mappings: BTreeMap::from([
            (known, "spotify:track:known".into()),
            (
                source_id("Unavailable", "Album", "Song"),
                "spotify:track:unavailable".into(),
            ),
        ]),
        excluded_tracks: BTreeSet::from([source_id("Ignored", "Album", "Song")]),
        ignored_albums: BTreeSet::from([source_album_key("Album ignored", "Album")]),
        ignored_artists: BTreeSet::from([normalize_for_match("Artist ignored")]),
        ..LastFmMappings::default()
    };
    let result = reconcile_incremental(
        &[
            event("Known", "Album", "Song", 10),
            event("Missing", "Album", "Song", 11),
            event("Unavailable", "Album", "Song", 11),
            event("Ignored", "Album", "Song", 12),
            event("Album ignored", "Album", "Song", 13),
            event("Artist ignored", "Other", "Song", 14),
            event("Known", "Album", "Song", 99),
        ],
        &[],
        &mappings,
        &BTreeSet::from(["spotify:track:known".into()]),
        0,
        50,
    );
    assert_eq!(result.increments["spotify:track:known"], 1);
    assert_eq!(
        result.unresolved,
        vec![
            event("Missing", "Album", "Song", 11),
            event("Unavailable", "Album", "Song", 11),
        ]
    );
    assert!(
        reconcile_incremental(&[], &[], &mappings, &BTreeSet::new(), 0, 100,)
            .increments
            .is_empty()
    );
}

#[test]
fn reconciliation_reprocessing_a_committed_window_is_a_no_op() {
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
        20,
        30,
    );
    assert!(result.increments.is_empty());
    assert!(result.latest.is_empty());
    assert!(result.unresolved.is_empty());
}

#[tokio::test]
async fn source_page_window_overlaps_fetches_and_checkpoints_in_descending_order() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    service.start_or_resume("user", 100, None).await.unwrap();
    service.set_metadata(4, 4).await.unwrap();

    let barrier = Arc::new(tokio::sync::Barrier::new(4));
    let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let completed = Arc::new(Mutex::new(Vec::new()));
    let releases = Arc::new(
        (0..5)
            .map(|_| Arc::new(tokio::sync::Notify::new()))
            .collect::<Vec<_>>(),
    );
    let fetch = {
        let barrier = Arc::clone(&barrier);
        let started = Arc::clone(&started);
        let completed = Arc::clone(&completed);
        let releases = Arc::clone(&releases);
        move |page| {
            let barrier = Arc::clone(&barrier);
            let started = Arc::clone(&started);
            let completed = Arc::clone(&completed);
            let release = Arc::clone(&releases[page as usize]);
            async move {
                started.fetch_add(1, Ordering::SeqCst);
                barrier.wait().await;
                release.notified().await;
                completed.lock().await.push(page);
                SourcePageFetchResult::Success(parsed_page(
                    page,
                    4,
                    vec![scrobble("Artist", "Album", "Track", page as u64)],
                ))
            }
        }
    };

    let runner = tokio::spawn(download_page_window(Arc::clone(&service), 4, 4, fetch));
    while started.load(Ordering::SeqCst) < 4 {
        tokio::task::yield_now().await;
    }
    assert_eq!(started.load(Ordering::SeqCst), 4);

    for (index, page) in [1_u32, 2, 3, 4].into_iter().enumerate() {
        releases[page as usize].notify_one();
        loop {
            if completed.lock().await.len() > index {
                break;
            }
            tokio::task::yield_now().await;
        }
    }

    assert_eq!(completed.lock().await.as_slice(), &[1, 2, 3, 4]);
    assert!(matches!(
        runner.await.unwrap().unwrap(),
        SourceWindowOutcome::Complete(pages) if pages == vec![4, 3, 2, 1]
    ));
    let session = service.snapshot().await.unwrap();
    assert_eq!(session.downloaded_pages, 4);
    assert_eq!(session.next_page, 0);
    assert_eq!(session.phase, ImportPhase::Aggregating);
}

#[tokio::test]
async fn source_page_window_keeps_only_the_contiguous_prefix_and_resumes_at_failure() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    service.start_or_resume("user", 100, None).await.unwrap();
    service.set_metadata(4, 4).await.unwrap();

    let barrier = Arc::new(tokio::sync::Barrier::new(4));
    let requested = Arc::new(Mutex::new(Vec::new()));
    let error = download_page_window(Arc::clone(&service), 4, 4, {
        let barrier = Arc::clone(&barrier);
        let requested = Arc::clone(&requested);
        move |page| {
            let barrier = Arc::clone(&barrier);
            let requested = Arc::clone(&requested);
            async move {
                requested.lock().await.push(page);
                barrier.wait().await;
                if page == 3 {
                    SourcePageFetchResult::Permanent("failed page 3".into())
                } else {
                    SourcePageFetchResult::Success(parsed_page(
                        page,
                        4,
                        vec![scrobble("Artist", "Album", "Track", page as u64)],
                    ))
                }
            }
        }
    })
    .await
    .unwrap_err();

    assert_eq!(error, "failed page 3");
    assert_eq!(
        requested
            .lock()
            .await
            .iter()
            .copied()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([1, 2, 3, 4])
    );
    let partial = service.snapshot().await.unwrap();
    assert_eq!(partial.downloaded_pages, 1);
    assert_eq!(partial.next_page, 3);
    assert_eq!(partial.phase, ImportPhase::Downloading);
    assert_eq!(source_runner_step(&partial), SourceRunnerStep::Page(3));
    let manifest = service.store.read_manifest(&partial).unwrap().unwrap();
    assert_eq!(manifest.pages.keys().copied().collect::<Vec<_>>(), vec![4]);

    let resumed = download_page_window(
        Arc::clone(&service),
        partial.next_page,
        4,
        move |page| async move {
            SourcePageFetchResult::Success(parsed_page(
                page,
                4,
                vec![scrobble("Artist", "Album", "Track", page as u64)],
            ))
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        resumed,
        SourceWindowOutcome::Complete(pages) if pages == vec![3, 2, 1]
    ));
}

#[tokio::test]
async fn source_page_window_retryable_failure_preserves_prefix_and_retry_owner() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    service.start_or_resume("user", 100, None).await.unwrap();
    service.set_metadata(4, 4).await.unwrap();

    let barrier = Arc::new(tokio::sync::Barrier::new(4));
    let outcome = download_page_window(Arc::clone(&service), 4, 4, {
        let barrier = Arc::clone(&barrier);
        move |page| {
            let barrier = Arc::clone(&barrier);
            async move {
                barrier.wait().await;
                if page == 3 {
                    SourcePageFetchResult::Retryable("rate limited page 3".into())
                } else {
                    SourcePageFetchResult::Success(parsed_page(
                        page,
                        4,
                        vec![scrobble("Artist", "Album", "Track", page as u64)],
                    ))
                }
            }
        }
    })
    .await
    .unwrap();

    assert!(matches!(outcome, SourceWindowOutcome::Retryable));
    let partial = service.snapshot().await.unwrap();
    assert_eq!(partial.downloaded_pages, 1);
    assert_eq!(partial.next_page, 3);
    assert_eq!(source_runner_step(&partial), SourceRunnerStep::Page(3));
    assert_eq!(source_page_window(partial.next_page, 4), vec![3, 2, 1]);
    assert_eq!(
        partial.retryable_error,
        Some(RetryableError {
            message: "rate limited page 3".into(),
            attempt: 1,
            retryable: true,
        })
    );
    let manifest = service.store.read_manifest(&partial).unwrap().unwrap();
    assert_eq!(manifest.pages.keys().copied().collect::<Vec<_>>(), vec![4]);
}

#[tokio::test]
async fn incremental_outer_retry_is_woken_by_lastfm_lifecycle_change() {
    let directory = tempfile::tempdir().unwrap();
    let (lastfm, executor) =
        crate::lastfm::Service::new_with_fake_executor(directory.path(), true, true);
    executor.queue_json(serde_json::json!({"token": "pending"}));
    lastfm.connect().await.unwrap();
    executor.queue_json(serde_json::json!({
        "session": {"name": "user", "key": "session"}
    }));
    lastfm.finish().await.unwrap();
    let service = Service::new(directory.path());
    let generation = lastfm.import_generation();
    let retry = {
        let lastfm = Arc::clone(&lastfm);
        let service = Arc::clone(&service);
        tokio::spawn(async move {
            wait_for_incremental_window_retry(
                lastfm.as_ref(),
                service.as_ref(),
                "user",
                generation,
                Duration::from_secs(300),
            )
            .await
        })
    };
    while service.sync_snapshot().await.sync_problem.is_none() {
        tokio::task::yield_now().await;
    }

    lastfm.disconnect().await.unwrap();

    let error = tokio::time::timeout(Duration::from_millis(100), retry)
        .await
        .expect("disconnect must wake the incremental outer retry")
        .unwrap()
        .unwrap_err();
    assert!(error.contains("account changed"));
}

#[tokio::test]
async fn incremental_cache_resumes_and_filters_the_padded_query_window() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    service
        .mutate_sync(|state| {
            state.lastfm_username = Some("user".into());
            state.synced_through = Some(10);
            state.active = Some(IncrementalRange {
                from: 10,
                to: 20,
                query_from: 9,
                query_to: 21,
                cache_id: incremental_cache_id("user", 10, 20),
                next_page: 2,
                total_pages: Some(2),
                downloaded_pages: 0,
                total_scrobbles: 6,
            });
            Ok(())
        })
        .await
        .unwrap();

    let page = parsed_page(
        1,
        2,
        vec![
            scrobble("Artist", "Album", "Before", 8),
            scrobble("Artist", "Album", "Inside", 11),
            scrobble("Artist", "Album", "After", 20),
        ],
    );
    assert!(service
        .checkpoint_incremental_page("user", 1, page.clone())
        .await
        .is_err());
    assert!(service
        .store
        .read_manifest(&incremental_cache_session(&service.sync_snapshot().await, "user").unwrap())
        .unwrap()
        .is_none());

    service
        .checkpoint_incremental_page(
            "user",
            2,
            parsed_page(
                2,
                2,
                vec![
                    scrobble("Artist", "Album", "Before", 9),
                    scrobble("Artist", "Album", "Inside", 10),
                    scrobble("Artist", "Album", "After", 21),
                ],
            ),
        )
        .await
        .unwrap();
    assert_eq!(service.sync_snapshot().await.active.unwrap().next_page, 1);
    service
        .checkpoint_incremental_page("user", 1, page)
        .await
        .unwrap();

    let events = read_incremental_events(&service, "user").await.unwrap();
    assert_eq!(
        events
            .iter()
            .map(|event| event.timestamp)
            .collect::<Vec<_>>(),
        vec![10, 11]
    );
    let cache_id = service.sync_snapshot().await.active.unwrap().cache_id;
    service.store.remove_snapshot(&cache_id).unwrap();
    service
        .mutate_sync(|state| {
            state.active = None;
            state.synced_through = Some(20);
            Ok(())
        })
        .await
        .unwrap();
    assert!(read_incremental_events(&service, "user")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn backlog_rehydration_rebuilds_incremental_rows_without_double_counting() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let backlog = vec![event("Artist", "Album", "Song", 10); 2];
    service
        .mutate_sync(|state| {
            state.lastfm_username = Some("user".into());
            state.backlog = backlog.clone();
            Ok(())
        })
        .await
        .unwrap();

    service
        .sync_backlog_into_review("user", Some("spotify"))
        .await
        .unwrap();
    let first = service.snapshot().await.unwrap();
    assert_eq!(first.rows[0].play_count, 2);
    service
        .sync_backlog_into_review("user", Some("spotify"))
        .await
        .unwrap();
    let second = service.snapshot().await.unwrap();
    assert_eq!(second.rows[0].play_count, 2);
}

#[test]
fn v2_session_without_downloaded_through_loads_safely() {
    let dir = tempfile::tempdir().unwrap();
    let store = ImportSessionStore::new(dir.path());
    let session =
        LastFmImportSessionV2::new_with_defaults("user".into(), 100, ImportDefaults::default());
    let mut json = serde_json::to_value(&session).unwrap();
    json.as_object_mut().unwrap().remove("downloadedThrough");
    fs::write(&store.path, serde_json::to_vec(&json).unwrap()).unwrap();

    assert_eq!(store.load().unwrap().unwrap().downloaded_through, None);
}

#[tokio::test]
async fn downloaded_through_advances_monotonically_and_survives_reload() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    service.start_or_resume("user", 500, None).await.unwrap();
    service.set_metadata(3, 3).await.unwrap();
    service
        .checkpoint_page(
            3,
            &parsed_page(3, 3, vec![scrobble("Artist", "Album", "Track", 300)]),
        )
        .await
        .unwrap();
    assert_eq!(
        service.snapshot().await.unwrap().downloaded_through,
        Some(300)
    );
    service
        .checkpoint_page(
            2,
            &parsed_page(2, 3, vec![scrobble("Artist", "Album", "Track", 200)]),
        )
        .await
        .unwrap();
    assert_eq!(
        service.snapshot().await.unwrap().downloaded_through,
        Some(300)
    );
    service
        .checkpoint_page(
            1,
            &parsed_page(1, 3, vec![scrobble("Artist", "Album", "Track", 400)]),
        )
        .await
        .unwrap();
    let complete = service.snapshot().await.unwrap();
    assert_eq!(complete.downloaded_through, Some(400));
    assert_eq!(state_view(Some(&complete)).history_to, Some(500));
    assert_eq!(
        Service::new(dir.path())
            .snapshot()
            .await
            .unwrap()
            .downloaded_through,
        Some(400)
    );
}

#[test]
fn cached_spotify_identity_only_trusts_an_exact_matching_cache() {
    let mut library = SpotifyLibraryState {
        account_id: "spotify-a".into(),
        complete: true,
        ..SpotifyLibraryState::default()
    };
    assert_eq!(
        cached_spotify_identity_matches("spotify-a", &library),
        Some(true)
    );
    assert_eq!(
        cached_spotify_identity_matches("spotify-b", &library),
        Some(false)
    );

    library.complete = false;
    assert_eq!(cached_spotify_identity_matches("spotify-a", &library), None);
}

#[test]
fn session_account_matching_requires_bound_identity_for_owned_mutations() {
    let mut session = LastFmImportSessionV2::new("lastfm-user".into(), "spotify-a".into(), 1);
    assert!(session_account_matches(
        &session,
        "lastfm-user",
        "spotify-a",
        true
    ));
    assert!(!session_account_matches(
        &session,
        "lastfm-user",
        "spotify-b",
        true
    ));

    session.spotify_account_id = None;
    assert!(!session_account_matches(
        &session,
        "lastfm-user",
        "spotify-a",
        true
    ));
    assert!(session_account_matches(
        &session,
        "lastfm-user",
        "spotify-a",
        false
    ));
}

#[test]
fn source_and_review_phases_choose_the_correct_account_boundary() {
    let mut session = LastFmImportSessionV2::new_with_defaults(
        "lastfm-user".into(),
        1,
        ImportDefaults::default(),
    );
    assert!(!requires_spotify_ownership(&session));
    session.total_pages = Some(1);
    session.phase = ImportPhase::Aggregating;
    assert!(!requires_spotify_ownership(&session));

    session.phase = ImportPhase::Review;
    assert!(!requires_spotify_ownership(&session));
    session.spotify_account_id = Some("spotify-a".into());
    assert!(requires_spotify_ownership(&session));
    session.phase = ImportPhase::Done;
    assert!(requires_spotify_ownership(&session));
    session.phase = ImportPhase::Suspended;
    assert!(requires_spotify_ownership(&session));
    session.spotify_account_id = None;
    assert!(!requires_spotify_ownership(&session));
}

fn parsed_page(page: u32, total_pages: u32, tracks: Vec<ParsedScrobble>) -> ParsedRecentTracksPage {
    ParsedRecentTracksPage {
        page,
        total_pages: Some(total_pages),
        total: Some(tracks.len() as u64),
        tracks,
        ..ParsedRecentTracksPage::default()
    }
}

async fn start_bound(service: &Service, username: &str, spotify: &str, history_to: u64) {
    service
        .start_or_resume(username, history_to, None)
        .await
        .unwrap();
    let mut session = service.snapshot().await.unwrap();
    session.spotify_account_id = Some(spotify.into());
    service.save(session).await.unwrap();
}

#[tokio::test]
async fn completed_v2_sessions_backfill_reusable_mappings_idempotently() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 10);
    aggregate_scrobbles(&mut session.rows, &[scrobble("Artist", "Album", "Song", 1)]);
    let row = session.rows[0].clone();
    session.phase = ImportPhase::Done;
    session.decisions.insert(
        row.stable_id.clone(),
        RowDecision {
            status: RowStatus::Done,
            excluded: false,
        },
    );
    session.matches.insert(
        row.stable_id.clone(),
        MatchResult {
            source_id: row.stable_id.clone(),
            search_term: "Song".into(),
            confidence: Some(Confidence::Exact),
            selected_uri: Some("spotify:album:album".into()),
            candidates: vec![AlbumCandidate {
                uri: "spotify:album:album".into(),
                name: "Album".into(),
                artist: "Artist".into(),
                track_uris: vec!["spotify:track:song".into()],
                track_names: vec!["Song".into()],
                ..AlbumCandidate::default()
            }],
            track_matches: BTreeMap::from([(row.stable_id.clone(), "spotify:track:song".into())]),
        },
    );
    session.page_options.insert(
        "accepted".into(),
        PageOptions {
            selected_track_ids: BTreeSet::from([row.stable_id.clone()]),
            ..PageOptions::default()
        },
    );
    service.save(session).await.unwrap();

    service.backfill_completed_mappings().await.unwrap();
    service.backfill_completed_mappings().await.unwrap();
    let mappings = service.mappings_for("user", Some("spotify")).await.unwrap();
    assert_eq!(
        mappings.track_mappings[&row.stable_id],
        "spotify:track:song"
    );
    assert_eq!(
        mappings.album_mappings[&source_album_key("Artist", "Album")].track_uris_by_name
            [&normalize_for_match("Song")],
        "spotify:track:song"
    );
}

#[test]
fn manifest_is_authoritative_and_acknowledged_page_damage_quarantines_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let store = ImportSessionStore::new(dir.path());
    let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 42);
    session.total_pages = Some(2);
    session.next_page = 2;
    let orphan = store.page_path(&session.cache_id, 2).unwrap();
    fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    fs::write(&orphan, b"orphan").unwrap();
    assert!(store.validate_cache(&session).is_ok());

    store
        .write_page(
            &session,
            &parsed_page(2, 2, vec![scrobble("Artist", "Album", "Track", 2)]),
        )
        .unwrap();
    assert!(store.validate_cache(&session).is_ok());
    fs::remove_file(&orphan).unwrap();
    session.downloaded_pages = 1;
    session.next_page = 1;
    store.save(&session).unwrap();
    assert!(store.load().unwrap().is_none());
    assert!(fs::read_dir(dir.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("quarantine")
    }));

    let dir = tempfile::tempdir().unwrap();
    let store = ImportSessionStore::new(dir.path());
    let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 42);
    session.total_pages = Some(1);
    session.next_page = 0;
    store
        .write_page(
            &session,
            &parsed_page(1, 1, vec![scrobble("Artist", "Album", "Track", 2)]),
        )
        .unwrap();
    session.downloaded_pages = 1;
    store.save(&session).unwrap();
    let page_path = store.page_path(&session.cache_id, 1).unwrap();
    let damaged = CachedRawPage {
        lastfm_username: session.lastfm_username.clone(),
        history_to: 43,
        total_pages: 1,
        parsed: parsed_page(1, 1, vec![scrobble("Artist", "Album", "Track", 2)]),
    };
    fs::write(&page_path, serde_json::to_vec(&damaged).unwrap()).unwrap();
    assert!(store.load().unwrap().is_none());

    let dir = tempfile::tempdir().unwrap();
    let store = ImportSessionStore::new(dir.path());
    let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 42);
    session.total_pages = Some(1);
    session.next_page = 0;
    session.downloaded_pages = 1;
    let manifest = RawCacheManifest {
        version: SESSION_VERSION,
        cache_id: session.cache_id.clone(),
        lastfm_username: session.lastfm_username.clone(),
        history_to: session.history_to,
        total_pages: 1,
        pages: BTreeMap::from([(1, MAX_RAW_CACHE_BYTES + 1)]),
    };
    fs::create_dir_all(store.cache_path(&session.cache_id).unwrap()).unwrap();
    fs::write(
        store.manifest_path(&session.cache_id).unwrap(),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    store.save(&session).unwrap();
    assert!(store.load().unwrap().is_none());
}

#[test]
fn cache_validation_rejects_skipped_acknowledged_pages_and_malformed_cursors() {
    let dir = tempfile::tempdir().unwrap();
    let store = ImportSessionStore::new(dir.path());
    let mut skipped = LastFmImportSessionV2::new("user".into(), "spotify".into(), 42);
    skipped.total_pages = Some(3);
    skipped.downloaded_pages = 2;
    skipped.next_page = 1;
    for page in [3, 1] {
        store
            .write_page(
                &skipped,
                &parsed_page(
                    page,
                    3,
                    vec![scrobble("Artist", "Album", "Track", page as u64)],
                ),
            )
            .unwrap();
    }
    assert!(store.validate_cache(&skipped).is_err());
    store.save(&skipped).unwrap();
    assert!(store.load().unwrap().is_none());

    let dir = tempfile::tempdir().unwrap();
    let store = ImportSessionStore::new(dir.path());
    let mut malformed = LastFmImportSessionV2::new("user".into(), "spotify".into(), 42);
    malformed.total_pages = Some(3);
    malformed.downloaded_pages = 2;
    malformed.next_page = 0;
    for page in [3, 2] {
        store
            .write_page(
                &malformed,
                &parsed_page(
                    page,
                    3,
                    vec![scrobble("Artist", "Album", "Track", page as u64)],
                ),
            )
            .unwrap();
    }
    assert!(store.validate_cache(&malformed).is_err());
    store.save(&malformed).unwrap();
    assert!(store.load().unwrap().is_none());
}

#[tokio::test]
async fn suspended_completed_source_revalidates_cache_before_aggregation() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    service.start_or_resume("user", 500, None).await.unwrap();
    service.set_metadata(2, 2).await.unwrap();
    for page in [2, 1] {
        service
            .checkpoint_page(
                page,
                &parsed_page(
                    page,
                    2,
                    vec![scrobble("Artist", "Album", "Track", page as u64)],
                ),
            )
            .await
            .unwrap();
    }
    assert_eq!(
        service.snapshot().await.unwrap().phase,
        ImportPhase::Aggregating
    );
    service.suspend_for_account_mismatch().await.unwrap();
    let suspended = service.snapshot().await.unwrap();
    let store = ImportSessionStore::new(dir.path());
    fs::remove_file(store.page_path(&suspended.cache_id, 1).unwrap()).unwrap();

    let reloaded = Service::new(dir.path());
    assert!(reloaded.snapshot().await.is_none());
    reloaded.start_or_resume("user", 500, None).await.unwrap();
    let fresh = reloaded.snapshot().await.unwrap();
    assert_eq!(fresh.phase, ImportPhase::Downloading);
    assert_eq!(fresh.downloaded_pages, 0);
    assert_eq!(fresh.next_page, 1);
}

#[tokio::test]
async fn suspended_source_revalidates_cache_but_review_survives_deleted_raw_cache() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    service.start_or_resume("user", 500, None).await.unwrap();
    service.set_metadata(2, 2).await.unwrap();
    service
        .checkpoint_page(
            2,
            &parsed_page(2, 2, vec![scrobble("Artist", "Album", "Track", 2)]),
        )
        .await
        .unwrap();
    service.suspend_for_account_mismatch().await.unwrap();
    let suspended = service.snapshot().await.unwrap();
    let store = ImportSessionStore::new(dir.path());
    fs::remove_file(store.page_path(&suspended.cache_id, 2).unwrap()).unwrap();

    service.start_or_resume("user", 500, None).await.unwrap();
    let restarted = service.snapshot().await.unwrap();
    assert_eq!(restarted.phase, ImportPhase::Downloading);
    assert_eq!(restarted.downloaded_pages, 0);
    assert_eq!(restarted.next_page, 1);

    let review_dir = tempfile::tempdir().unwrap();
    let review_service = Service::new(review_dir.path());
    review_service
        .start_or_resume("user", 500, None)
        .await
        .unwrap();
    review_service.set_metadata(1, 1).await.unwrap();
    review_service
        .checkpoint_page(
            1,
            &parsed_page(1, 1, vec![scrobble("Artist", "Album", "Track", 2)]),
        )
        .await
        .unwrap();
    review_service.aggregate_cached(None).await.unwrap();
    review_service.suspend_for_account_mismatch().await.unwrap();
    assert_eq!(
        Service::new(review_dir.path())
            .snapshot()
            .await
            .unwrap()
            .phase,
        ImportPhase::Suspended
    );
}

#[tokio::test]
async fn retry_state_round_trips_without_advancing_the_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    service
        .start_or_resume("lastfm-user", 500, None)
        .await
        .unwrap();
    service.set_metadata(3, 600).await.unwrap();
    service
        .set_retryable_error(Some(RetryableError {
            message: "temporary".into(),
            attempt: 4,
            retryable: true,
        }))
        .await
        .unwrap();

    let session = Service::new(dir.path()).snapshot().await.unwrap();
    assert_eq!(session.next_page, 3);
    assert_eq!(session.downloaded_pages, 0);
    assert_eq!(session.retryable_error.unwrap().attempt, 4);
}

#[tokio::test]
async fn spotify_binding_waits_for_the_first_review_match() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    service
        .start_or_resume("lastfm-user", 500, None)
        .await
        .unwrap();
    assert_eq!(service.snapshot().await.unwrap().spotify_account_id, None);
    service.set_metadata(1, 1).await.unwrap();
    service
        .checkpoint_page(
            1,
            &parsed_page(1, 1, vec![scrobble("Artist", "Album", "Track", 10)]),
        )
        .await
        .unwrap();
    service.aggregate_cached(None).await.unwrap();
    let source_id = service.snapshot().await.unwrap().rows[0].stable_id.clone();
    service
        .set_match(
            "lastfm-user",
            "spotify-user",
            1,
            MatchResult {
                source_id,
                search_term: "track search".into(),
                confidence: Some(Confidence::Exact),
                selected_uri: Some("spotify:track:target".into()),
                candidates: Vec::new(),
                track_matches: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        service
            .snapshot()
            .await
            .unwrap()
            .spotify_account_id
            .as_deref(),
        Some("spotify-user")
    );
}

#[tokio::test]
async fn legacy_spotify_profile_id_migrates_persisted_import_bindings() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    service
        .save(LastFmImportSessionV2::new(
            "lastfm-user".into(),
            "legacy-profile".into(),
            500,
        ))
        .await
        .unwrap();
    service
        .mutate_sync(|state| {
            state.spotify_account_id = Some("legacy-profile".into());
            Ok(())
        })
        .await
        .unwrap();
    service
        .save_mappings_for(
            "lastfm-user",
            Some("legacy-profile"),
            LastFmMappings::default(),
        )
        .await
        .unwrap();

    service
        .migrate_spotify_account_id("legacy-profile", "immutable-account")
        .await
        .unwrap();

    let reloaded = Service::new(dir.path());
    assert_eq!(
        reloaded
            .snapshot()
            .await
            .unwrap()
            .spotify_account_id
            .as_deref(),
        Some("immutable-account")
    );
    assert_eq!(
        reloaded.sync_snapshot().await.spotify_account_id.as_deref(),
        Some("immutable-account")
    );
    assert_eq!(
        reloaded
            .export_mappings()
            .await
            .spotify_account_id
            .as_deref(),
        Some("immutable-account")
    );
}

#[tokio::test]
async fn account_id_migration_recovers_all_new_after_each_file_boundary() {
    for blocked_store in ["session", "sync", "mappings"] {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        service
            .save(LastFmImportSessionV2::new(
                "lastfm-user".into(),
                "legacy-profile".into(),
                500,
            ))
            .await
            .unwrap();
        service
            .mutate_sync(|state| {
                state.spotify_account_id = Some("legacy-profile".into());
                Ok(())
            })
            .await
            .unwrap();
        service
            .save_mappings_for(
                "lastfm-user",
                Some("legacy-profile"),
                LastFmMappings::default(),
            )
            .await
            .unwrap();
        let hook = crate::store::SaveHook::new(true);
        match blocked_store {
            "session" => service.store.arm_save(Arc::clone(&hook)),
            "sync" => service.incremental_store.arm_save(Arc::clone(&hook)),
            "mappings" => service.mappings_store.arm_save(Arc::clone(&hook)),
            _ => unreachable!(),
        }
        let migration = {
            let service = Arc::clone(&service);
            tokio::spawn(async move {
                service
                    .migrate_spotify_account_id("legacy-profile", "immutable-account")
                    .await
            })
        };
        while !hook.is_reached() {
            tokio::task::yield_now().await;
        }
        hook.wait_until_reached();
        hook.release();
        assert!(migration.await.unwrap().is_err());

        assert_eq!(
            service
                .snapshot()
                .await
                .unwrap()
                .spotify_account_id
                .as_deref(),
            Some("legacy-profile")
        );
        let reloaded = Service::new(dir.path());
        assert_eq!(
            reloaded
                .snapshot()
                .await
                .unwrap()
                .spotify_account_id
                .as_deref(),
            Some("immutable-account")
        );
        assert_eq!(
            reloaded.sync_snapshot().await.spotify_account_id.as_deref(),
            Some("immutable-account")
        );
        assert_eq!(
            reloaded
                .export_mappings()
                .await
                .spotify_account_id
                .as_deref(),
            Some("immutable-account")
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn aborted_account_id_migration_finishes_all_memory_publication() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    service
        .save(LastFmImportSessionV2::new(
            "lastfm-user".into(),
            "legacy-profile".into(),
            500,
        ))
        .await
        .unwrap();
    service
        .mutate_sync(|state| {
            state.spotify_account_id = Some("legacy-profile".into());
            Ok(())
        })
        .await
        .unwrap();
    service
        .save_mappings_for(
            "lastfm-user",
            Some("legacy-profile"),
            LastFmMappings::default(),
        )
        .await
        .unwrap();
    let hook = crate::store::SaveHook::new(false);
    service.mappings_store.arm_save(Arc::clone(&hook));
    let migration = {
        let service = Arc::clone(&service);
        tokio::spawn(async move {
            service
                .migrate_spotify_account_id("legacy-profile", "immutable-account")
                .await
        })
    };
    while !hook.is_reached() {
        tokio::task::yield_now().await;
    }
    hook.wait_until_reached();
    migration.abort();
    hook.release();
    assert!(migration.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(1), async {
        while service
            .export_mappings()
            .await
            .spotify_account_id
            .as_deref()
            != Some("immutable-account")
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        service
            .snapshot()
            .await
            .unwrap()
            .spotify_account_id
            .as_deref(),
        Some("immutable-account")
    );
    assert_eq!(
        service.sync_snapshot().await.spotify_account_id.as_deref(),
        Some("immutable-account")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn aborted_snapshot_invalidation_publishes_none_after_quarantine() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let session = LastFmImportSessionV2::new("lastfm-user".into(), "spotify".into(), 500);
    let cache_path = service.store.cache_path(&session.cache_id).unwrap();
    fs::create_dir_all(&cache_path).unwrap();
    fs::write(cache_path.join("cached"), b"page").unwrap();
    service.save(session).await.unwrap();
    let hook = crate::store::SaveHook::new(false);
    service.store.arm_quarantine(Arc::clone(&hook));
    let invalidation = {
        let service = Arc::clone(&service);
        tokio::spawn(async move { service.invalidate_snapshot().await })
    };
    while !hook.is_reached() {
        tokio::task::yield_now().await;
    }
    hook.wait_until_reached();
    assert!(service.snapshot().await.is_some());

    invalidation.abort();
    hook.release();
    assert!(invalidation.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(1), async {
        while service.snapshot().await.is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(Service::new(dir.path()).snapshot().await.is_none());
}

#[tokio::test]
async fn all_pages_are_present_before_aggregation_and_review() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    service
        .start_or_resume("lastfm-user", 500, None)
        .await
        .unwrap();
    service.set_metadata(2, 2).await.unwrap();
    service
        .checkpoint_page(
            2,
            &parsed_page(2, 2, vec![scrobble("Artist", "Album", "New", 20)]),
        )
        .await
        .unwrap();
    let partial = service.snapshot().await.unwrap();
    assert_eq!(partial.phase, ImportPhase::Downloading);
    assert!(partial.rows.is_empty());
    service
        .checkpoint_page(
            1,
            &parsed_page(1, 2, vec![scrobble("Artist", "Album", "Old", 10)]),
        )
        .await
        .unwrap();
    let complete = service.snapshot().await.unwrap();
    assert_eq!(complete.phase, ImportPhase::Aggregating);
    assert_eq!(complete.downloaded_pages, 2);
    assert!(complete.rows.is_empty());
    service.aggregate_cached(None).await.unwrap();
    let review = service.snapshot().await.unwrap();
    assert_eq!(review.phase, ImportPhase::Review);
    assert_eq!(
        review
            .rows
            .iter()
            .map(|row| row.track.as_str())
            .collect::<Vec<_>>(),
        ["Old", "New"]
    );
}

#[tokio::test]
async fn empty_aggregate_enters_done() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    service
        .start_or_resume("lastfm-user", 500, None)
        .await
        .unwrap();
    service.set_metadata(1, 0).await.unwrap();
    service
        .checkpoint_page(1, &parsed_page(1, 1, Vec::new()))
        .await
        .unwrap();

    service.aggregate_cached(None).await.unwrap();

    let session = service.snapshot().await.unwrap();
    assert_eq!(session.phase, ImportPhase::Done);
    assert!(session.rows.is_empty());
    assert_eq!(session.remaining(), 0);
}

#[tokio::test]
async fn checkpoint_discards_rows_at_or_after_history_cutoff() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    service
        .start_or_resume("lastfm-user", 500, None)
        .await
        .unwrap();
    service.set_metadata(1, 3).await.unwrap();
    service
        .checkpoint_page(
            1,
            &parsed_page(
                1,
                1,
                vec![
                    scrobble("Artist", "Album", "Before", 499),
                    scrobble("Artist", "Album", "At cutoff", 500),
                    scrobble("Artist", "Album", "After", 501),
                ],
            ),
        )
        .await
        .unwrap();

    let session = service.snapshot().await.unwrap();
    assert_eq!(session.included_scrobbles, 1);
    assert_eq!(session.phase, ImportPhase::Aggregating);
    service.aggregate_cached(None).await.unwrap();
    let session = service.snapshot().await.unwrap();
    assert_eq!(session.rows.len(), 1);
    assert_eq!(session.rows[0].track, "Before");
}

#[test]
fn cache_identity_uses_exact_username_and_rejects_metadata_mismatch() {
    assert_ne!(
        snapshot_cache_id("user.name", 42),
        snapshot_cache_id("username", 42)
    );

    let dir = tempfile::tempdir().unwrap();
    let store = ImportSessionStore::new(dir.path());
    let mut session = LastFmImportSessionV2::new("user.name".into(), "spotify".into(), 42);
    session.total_pages = Some(1);
    session.next_page = 1;
    store
        .write_page(
            &session,
            &parsed_page(1, 1, vec![scrobble("Artist", "Album", "Track", 2)]),
        )
        .unwrap();

    let mut mismatch = session.clone();
    mismatch.lastfm_username = "user-name".into();
    mismatch.cache_id = session.cache_id.clone();
    assert!(store.validate_cache(&mismatch).is_err());
    fs::write(&store.path, serde_json::to_vec(&mismatch).unwrap()).unwrap();
    assert!(store.load().unwrap().is_none());
}

#[test]
fn visible_batch_matching_is_lazy_and_accept_all_is_the_bulk_plan() {
    let mut session =
        LastFmImportSessionV2::new_with_defaults("user".into(), 100, ImportDefaults::default());
    aggregate_scrobbles(
        &mut session.rows,
        &[
            scrobble("Artist A", "Album A", "One", 1),
            scrobble("Artist B", "Album B", "Two", 2),
            scrobble("Artist C", "Album C", "Three", 3),
        ],
    );
    let first_id = session.rows[0].stable_id.clone();
    let first_key = "Artist A\u{1f}Album A".to_owned();
    session.page_options.insert(
        first_key,
        PageOptions {
            selected_track_ids: BTreeSet::from([first_id.clone()]),
            ..PageOptions::default()
        },
    );

    assert_eq!(
        batch_match_plan(&session, Some((1, "Artist A", "Album A"))),
        vec![(1, "Artist A".into(), "Album A".into())]
    );
    session.matches.insert(
        first_id.clone(),
        MatchResult {
            source_id: first_id,
            search_term: "track".into(),
            confidence: Some(Confidence::Exact),
            selected_uri: Some("spotify:track:first".into()),
            candidates: Vec::new(),
            track_matches: BTreeMap::new(),
        },
    );
    assert!(batch_match_plan(&session, Some((1, "Artist A", "Album A"))).is_empty());
    assert_eq!(
        batch_match_plan(&session, None),
        vec![
            (2, "Artist B".into(), "Album B".into()),
            (3, "Artist C".into(), "Album C".into()),
        ]
    );
}

#[test]
fn review_batches_split_large_single_groups_into_stable_bounded_pages() {
    let rows = (0..205)
        .map(|index| SourceRow {
            stable_id: format!("source-{index}"),
            artist: "Artist".into(),
            album: String::new(),
            track: format!("Track {index}"),
            variants: Vec::new(),
            play_count: 1,
            earliest: index as u64,
            latest: index as u64,
        })
        .collect::<Vec<_>>();
    let batches = build_review_batches(&rows);

    assert_eq!(batches.len(), 3);
    assert_eq!(
        batches.iter().map(|batch| batch.page).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(batches[0].source_ids.len(), LASTFM_REVIEW_BATCH_SIZE);
    assert_eq!(batches[1].source_ids.len(), LASTFM_REVIEW_BATCH_SIZE);
    assert_eq!(batches[2].source_ids.len(), 5);
    assert_eq!(batches[0].source_ids[0], "source-0");
    assert_eq!(batches[1].source_ids[0], "source-100");
    assert_eq!(batches[2].source_ids[0], "source-200");
}

#[tokio::test]
async fn split_batch_default_options_are_local_and_each_batch_can_commit() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 1000);
    aggregate_scrobbles(
        &mut session.rows,
        &(0..205)
            .map(|index| scrobble("Artist", "Album", &format!("Track {index}"), index + 1))
            .collect::<Vec<_>>(),
    );
    session.phase = ImportPhase::Review;
    service.save(session).await.unwrap();

    for batch_id in 1..=3 {
        let page = service.page(batch_id, "Artist", "Album").await.unwrap();
        let source_ids = page
            .rows
            .iter()
            .map(|item| item.source.stable_id.clone())
            .collect::<BTreeSet<_>>();
        assert_eq!(page.options.selected_track_ids, source_ids);
        let selected_ids = page
            .options
            .selected_track_ids
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        service
            .commit_rows(
                "user",
                "spotify",
                batch_id,
                &selected_ids,
                "Artist",
                "Album",
                page.options,
            )
            .await
            .unwrap();
    }

    assert_eq!(service.snapshot().await.unwrap().phase, ImportPhase::Done);
}

#[tokio::test]
async fn queue_pages_are_bounded_and_validate_cursor_and_limit() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 1000);
    aggregate_scrobbles(
        &mut session.rows,
        &(0..205)
            .map(|index| scrobble("Artist", "Album", &format!("Track {index}"), index + 1))
            .collect::<Vec<_>>(),
    );
    session.phase = ImportPhase::Review;
    service.save(session).await.unwrap();

    let first = service.queue_page(0, 2).await.unwrap();
    assert_eq!(first.total, 3);
    assert_eq!(first.items.len(), 2);
    assert_eq!(
        first
            .items
            .iter()
            .map(|item| item.source_count)
            .collect::<Vec<_>>(),
        [100, 100]
    );
    assert_eq!(first.next_cursor, Some(2));
    let second = service
        .queue_page(first.next_cursor.unwrap(), 2)
        .await
        .unwrap();
    assert_eq!(
        second
            .items
            .iter()
            .map(|item| item.source_count)
            .collect::<Vec<_>>(),
        [5]
    );
    assert_eq!(second.next_cursor, None);
    assert!(service.queue_page(0, 0).await.is_err());
    assert!(service
        .queue_page(0, LASTFM_QUEUE_PAGE_LIMIT + 1)
        .await
        .is_err());
    assert!(service.queue_page(first.total + 1, 1).await.is_err());
}

#[tokio::test]
async fn queue_separates_imported_plays_from_remaining_work() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 1000);
    aggregate_scrobbles(
        &mut session.rows,
        &[
            scrobble("Artist", "Album", "Imported", 1),
            scrobble("Artist", "Album", "Remaining", 2),
        ],
    );
    session.rows.iter_mut().for_each(|row| {
        row.play_count = if row.track == "Imported" { 3_890 } else { 92 };
    });
    let imported_id = session
        .rows
        .iter()
        .find(|row| row.track == "Imported")
        .unwrap()
        .stable_id
        .clone();
    session.decisions.insert(
        imported_id,
        RowDecision {
            status: RowStatus::Done,
            excluded: false,
        },
    );
    session.phase = ImportPhase::Review;
    service.save(session).await.unwrap();

    let item = &service.queue_page(0, 1).await.unwrap().items[0];
    assert_eq!(item.play_count, 3_982);
    assert_eq!(item.imported_play_count, 3_890);
    assert_eq!(item.remaining_play_count, 92);
}

#[tokio::test]
async fn large_queue_follows_every_cursor_in_order_without_materializing_prior_slices() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let count = 23_132_u32;
    let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 1000);
    session.phase = ImportPhase::Review;
    session.rows = (0..count)
        .map(|index| SourceRow {
            stable_id: format!("source-{index}"),
            artist: format!("Artist {index}"),
            album: format!("Album {index}"),
            track: format!("Track {index}"),
            variants: Vec::new(),
            play_count: 1,
            earliest: index as u64,
            latest: index as u64,
        })
        .collect();
    session.batches = (0..count)
        .map(|index| ImportBatch {
            page: index + 1,
            source_ids: vec![format!("source-{index}")],
            collection_shaped: None,
            representative_artist: None,
            representative_album: None,
            album_labels: Vec::new(),
        })
        .collect();
    *service.session.lock().await = Some(session);

    let mut cursor = 0;
    let mut seen_pages = Vec::with_capacity(count as usize);
    loop {
        let page = service.queue_page(cursor, 1000).await.unwrap();
        assert_eq!(page.total, count as usize);
        assert!(page.items.len() <= 1000);
        seen_pages.extend(page.items.iter().map(|item| item.page));
        match page.next_cursor {
            Some(next_cursor) => {
                assert_eq!(next_cursor, cursor + page.items.len());
                cursor = next_cursor;
            }
            None => break,
        }
    }

    assert_eq!(seen_pages.len(), count as usize);
    assert_eq!(seen_pages, (1..=count).collect::<Vec<_>>());
}

#[test]
fn accept_all_entity_counts_are_unique_across_batches() {
    let mut session =
        LastFmImportSessionV2::new_with_defaults("user".into(), 100, ImportDefaults::default());
    aggregate_scrobbles(
        &mut session.rows,
        &[
            scrobble("Artist A", "Album A", "One", 1),
            scrobble("Artist B", "Album B", "Two", 2),
        ],
    );
    for row in &session.rows {
        session.page_options.insert(
            format!("{}\u{1f}{}", row.artist, row.album),
            PageOptions {
                whole_album: true,
                selected_track_ids: BTreeSet::from([row.stable_id.clone()]),
                ..PageOptions::default()
            },
        );
        session.matches.insert(
            row.stable_id.clone(),
            MatchResult {
                source_id: row.stable_id.clone(),
                search_term: "album".into(),
                confidence: Some(Confidence::Exact),
                selected_uri: Some("spotify:album:shared".into()),
                candidates: Vec::new(),
                track_matches: BTreeMap::new(),
            },
        );
    }
    let (albums, tracks) = accept_all_entity_uris(&session);
    assert_eq!(albums, BTreeSet::from(["spotify:album:shared".into()]));
    assert!(tracks.is_empty());

    for options in session.page_options.values_mut() {
        options.whole_album = false;
    }
    for row in &session.rows {
        session
            .matches
            .get_mut(&row.stable_id)
            .unwrap()
            .track_matches =
            BTreeMap::from([(row.stable_id.clone(), "spotify:track:shared".into())]);
    }
    let (albums, tracks) = accept_all_entity_uris(&session);
    assert!(albums.is_empty());
    assert_eq!(tracks, BTreeSet::from(["spotify:track:shared".into()]));
}

#[tokio::test]
async fn lazy_coordinator_shares_duplicate_opens_and_keeps_batch_scope() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let mut session =
        LastFmImportSessionV2::new_with_defaults("user".into(), 100, ImportDefaults::default());
    aggregate_scrobbles(
        &mut session.rows,
        &[
            scrobble("Artist A", "Album A", "One", 1),
            scrobble("Artist B", "Album B", "Two", 2),
        ],
    );
    session.phase = ImportPhase::Review;
    service.save(session).await.unwrap();

    seed_lastfm_files(dir.path(), &[]);
    let lastfm = crate::lastfm::Service::new_for_test(dir.path(), true, false);
    let gate = test_spotify_membership(dir.path());
    gate.set_for_test(SpotifyLibraryState {
        account_id: "spotify".into(),
        complete: true,
        ..SpotifyLibraryState::default()
    });
    let searches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let first_service = Arc::clone(&service);
    let first_lastfm = Arc::clone(&lastfm);
    let first_gate = Arc::clone(&gate);
    let first_searches = Arc::clone(&searches);
    let first = async move {
        lazy_match_page_with_search(
            first_service.as_ref(),
            first_lastfm.as_ref(),
            first_gate.as_ref(),
            &|| Err::<Arc<crate::SpotifyProvider>, _>("unexpected provider".into()),
            &|| Ok(true),
            &ReviewBatchKey {
                batch_id: 1,
                artist: "Artist A".into(),
                album: "Album A".into(),
            },
            move |rows| {
                let results = rows
                    .into_iter()
                    .map(|row| MatchResult {
                        source_id: row.stable_id.clone(),
                        search_term: row.track.clone(),
                        confidence: Some(Confidence::Exact),
                        selected_uri: Some("spotify:track:shared".into()),
                        candidates: Vec::new(),
                        track_matches: BTreeMap::from([(
                            row.stable_id,
                            "spotify:track:shared".into(),
                        )]),
                    })
                    .collect();
                let searches = Arc::clone(&first_searches);
                async move {
                    searches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    Ok(results)
                }
            },
        )
        .await
    };
    let second_service = Arc::clone(&service);
    let second_lastfm = Arc::clone(&lastfm);
    let second_gate = Arc::clone(&gate);
    let second_searches = Arc::clone(&searches);
    let second = async move {
        lazy_match_page_with_search(
            second_service.as_ref(),
            second_lastfm.as_ref(),
            second_gate.as_ref(),
            &|| Err::<Arc<crate::SpotifyProvider>, _>("unexpected provider".into()),
            &|| Ok(true),
            &ReviewBatchKey {
                batch_id: 1,
                artist: "Artist A".into(),
                album: "Album A".into(),
            },
            move |rows| {
                let results = rows
                    .into_iter()
                    .map(|row| MatchResult {
                        source_id: row.stable_id.clone(),
                        search_term: row.track.clone(),
                        confidence: Some(Confidence::Exact),
                        selected_uri: Some("spotify:track:shared".into()),
                        candidates: Vec::new(),
                        track_matches: BTreeMap::from([(
                            row.stable_id,
                            "spotify:track:shared".into(),
                        )]),
                    })
                    .collect();
                let searches = Arc::clone(&second_searches);
                async move {
                    searches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    Ok(results)
                }
            },
        )
        .await
    };
    let (first, second) = tokio::join!(first, second);
    assert!(first.unwrap().is_some());
    assert!(second.unwrap().is_some());
    assert_eq!(searches.load(std::sync::atomic::Ordering::SeqCst), 1);

    let session = service.snapshot().await.unwrap();
    assert_eq!(session.spotify_account_id.as_deref(), Some("spotify"));
    assert_eq!(session.matches.len(), 1);
    assert!(!session.matches.contains_key(&session.rows[1].stable_id));

    let cached_searches = Arc::clone(&searches);
    let cached = lazy_match_page_with_search(
        &service,
        lastfm.as_ref(),
        gate.as_ref(),
        &|| Err::<Arc<crate::SpotifyProvider>, _>("unexpected provider".into()),
        &|| Ok(true),
        &ReviewBatchKey {
            batch_id: 1,
            artist: "Artist A".into(),
            album: "Album A".into(),
        },
        move |_| async move {
            cached_searches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Vec::new())
        },
    )
    .await
    .unwrap();
    assert!(cached.is_some());
    assert_eq!(searches.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn collection_page_open_does_not_refetch_legacy_album_search_rows() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let mut session =
        LastFmImportSessionV2::new_with_defaults("user".into(), 100, ImportDefaults::default());
    aggregate_scrobbles(
        &mut session.rows,
        &[
            scrobble("Artist", "", "Cached", 1),
            scrobble("Artist", "", "Legacy", 2),
        ],
    );
    session.phase = ImportPhase::Review;
    for row in &session.rows {
        let search_term = if row.track == "Legacy" {
            album_search_term(&row.artist, "Singles")
        } else {
            track_search_term(&row.artist, &row.track)
        };
        session.matches.insert(
            row.stable_id.clone(),
            MatchResult {
                source_id: row.stable_id.clone(),
                search_term,
                confidence: None,
                selected_uri: None,
                candidates: Vec::new(),
                track_matches: BTreeMap::new(),
            },
        );
    }
    service.save(session).await.unwrap();

    seed_lastfm_files(dir.path(), &[]);
    let lastfm = crate::lastfm::Service::new_for_test(dir.path(), true, false);
    let gate = test_spotify_membership(dir.path());
    gate.set_for_test(SpotifyLibraryState {
        account_id: "spotify".into(),
        complete: true,
        ..SpotifyLibraryState::default()
    });
    let searched = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let search_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let first_searched = Arc::clone(&searched);
    let first_search_calls = Arc::clone(&search_calls);
    let first = lazy_match_page_with_search(
        &service,
        lastfm.as_ref(),
        gate.as_ref(),
        &|| Err::<Arc<crate::SpotifyProvider>, _>("unexpected provider".into()),
        &|| Ok(true),
        &ReviewBatchKey {
            batch_id: 1,
            artist: "Artist".into(),
            album: String::new(),
        },
        move |rows| {
            first_search_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            first_searched
                .lock()
                .unwrap()
                .extend(rows.iter().map(|row| row.track.clone()));
            async move {
                Ok(rows
                    .into_iter()
                    .map(|row| MatchResult {
                        source_id: row.stable_id.clone(),
                        search_term: track_search_term(&row.artist, &row.track),
                        confidence: None,
                        selected_uri: None,
                        candidates: Vec::new(),
                        track_matches: BTreeMap::new(),
                    })
                    .collect())
            }
        },
    )
    .await
    .unwrap();
    assert!(first.is_some());
    assert_eq!(search_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(searched.lock().unwrap().is_empty());

    let second_searched = Arc::clone(&searched);
    let second_search_calls = Arc::clone(&search_calls);
    let second = lazy_match_page_with_search(
        &service,
        lastfm.as_ref(),
        gate.as_ref(),
        &|| Err::<Arc<crate::SpotifyProvider>, _>("unexpected provider".into()),
        &|| Ok(true),
        &ReviewBatchKey {
            batch_id: 1,
            artist: "Artist".into(),
            album: String::new(),
        },
        move |rows| {
            second_search_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            second_searched
                .lock()
                .unwrap()
                .extend(rows.iter().map(|row| row.track.clone()));
            async move { Ok(Vec::new()) }
        },
    )
    .await
    .unwrap();
    assert!(second.is_some());
    assert_eq!(search_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(searched.lock().unwrap().is_empty());
}

#[tokio::test]
async fn cached_collection_page_reranks_with_unknown_spotify_membership() {
    let directory = tempfile::tempdir().unwrap();
    let (lastfm, service, state) = test_app_state(directory.path(), Library::new(), &[]);
    let mut rows = Vec::new();
    aggregate_scrobbles(
        &mut rows,
        &[
            scrobble("Artist", "", "One", 1),
            scrobble("Artist", "", "Two", 2),
            scrobble("Artist", "Closer", "One", 3),
            scrobble("Artist", "Closer", "Two", 4),
        ],
    );
    let mut session = collection_session(&rows);
    let batch = review_batches(&session).remove(0);
    let mut smaller = collection_album(
        "spotify:album:smaller",
        "Artist",
        &[("One", "spotify:track:one"), ("Two", "spotify:track:two")],
    );
    smaller.matching.name = "Closer".into();
    let mut larger = collection_album(
        "spotify:album:larger",
        "Artist",
        &[
            ("One", "spotify:track:large-one"),
            ("Two", "spotify:track:large-two"),
            ("Bonus", "spotify:track:bonus"),
        ],
    );
    larger.matching.name = "Closer".into();
    session.collection_album_matches.insert(
        batch.page,
        CollectionAlbumMatchState {
            cached_candidates: vec![larger, smaller],
            ..CollectionAlbumMatchState::default()
        },
    );
    service.save(session).await.unwrap();

    lazy_match_page(
        service.as_ref(),
        lastfm.as_ref(),
        &state.spotify_membership,
        &state.library,
        &state.cooldown_store,
        &|| Err::<Arc<crate::SpotifyProvider>, _>("provider should not be needed".into()),
        &|| Ok(false),
        ReviewBatchKey {
            batch_id: batch.page,
            artist: "Artist".into(),
            album: "Closer".into(),
        },
    )
    .await
    .unwrap();

    assert_eq!(
        service.snapshot().await.unwrap().collection_album_matches[&batch.page].selected_album_uris,
        vec!["spotify:album:smaller"]
    );
}

#[tokio::test]
async fn accept_all_skips_collection_batches_without_automatic_searches() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let rows = [collection_test_row("One"), collection_test_row("Two")];
    service.save(collection_session(&rows)).await.unwrap();
    let searches = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let summary = prepare_accept_all_batches(&service, {
        let searches = Arc::clone(&searches);
        move |_, _, _| {
            searches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { Ok(()) }
        }
    })
    .await
    .unwrap();

    assert_eq!(searches.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(summary.album_entities, 0);
    assert_eq!(summary.track_entities, 0);
}

#[tokio::test]
async fn lazy_coordinator_suspends_before_persisting_after_account_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let mut session =
        LastFmImportSessionV2::new_with_defaults("user".into(), 100, ImportDefaults::default());
    aggregate_scrobbles(
        &mut session.rows,
        &[scrobble("Artist", "Album", "Track", 1)],
    );
    session.phase = ImportPhase::Review;
    service.save(session).await.unwrap();

    seed_lastfm_files(dir.path(), &[]);
    let lastfm = crate::lastfm::Service::new_for_test(dir.path(), true, false);
    let gate = test_spotify_membership(dir.path());
    gate.set_for_test(SpotifyLibraryState {
        account_id: "spotify-a".into(),
        complete: true,
        ..SpotifyLibraryState::default()
    });
    let searches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let error = lazy_match_page_with_search(
        &service,
        lastfm.as_ref(),
        gate.as_ref(),
        &|| Err::<Arc<crate::SpotifyProvider>, _>("unexpected provider".into()),
        &|| Ok(true),
        &ReviewBatchKey {
            batch_id: 1,
            artist: "Artist".into(),
            album: "Album".into(),
        },
        {
            let searches = Arc::clone(&searches);
            let gate = Arc::clone(&gate);
            move |rows| {
                searches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                gate.set_for_test(SpotifyLibraryState {
                    account_id: "spotify-b".into(),
                    complete: true,
                    ..SpotifyLibraryState::default()
                });
                async move {
                    Ok(rows
                        .into_iter()
                        .map(|row| MatchResult {
                            source_id: row.stable_id,
                            search_term: row.track,
                            confidence: Some(Confidence::Exact),
                            selected_uri: Some("spotify:track:target".into()),
                            candidates: Vec::new(),
                            track_matches: BTreeMap::new(),
                        })
                        .collect())
                }
            }
        },
    )
    .await
    .unwrap_err();

    assert!(error.contains("changed while matching"));
    assert_eq!(searches.load(std::sync::atomic::Ordering::SeqCst), 1);
    let session = service.snapshot().await.unwrap();
    assert_eq!(session.phase, ImportPhase::Suspended);
    assert!(session.matches.is_empty());
    assert!(session.spotify_account_id.is_none());
}

#[tokio::test]
async fn accept_all_preparation_is_sequential_and_dedupes_entities() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let mut session =
        LastFmImportSessionV2::new_with_defaults("user".into(), 100, ImportDefaults::default());
    aggregate_scrobbles(
        &mut session.rows,
        &[
            scrobble("Artist A", "Album A", "One", 1),
            scrobble("Artist B", "Album B", "Two", 2),
            scrobble("Artist C", "Album C", "Three", 3),
        ],
    );
    session.phase = ImportPhase::Review;
    service.save(session).await.unwrap();
    let order = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let summary = prepare_accept_all_batches(&service, {
        let service = Arc::clone(&service);
        let order = Arc::clone(&order);
        move |batch_id, artist, album| {
            let service = Arc::clone(&service);
            let order = Arc::clone(&order);
            async move {
                order.lock().await.push((artist.clone(), album.clone()));
                let session = service.snapshot().await.unwrap();
                let results = session
                    .rows
                    .iter()
                    .filter(|row| row.artist == artist && row.album == album)
                    .map(|row| MatchResult {
                        source_id: row.stable_id.clone(),
                        search_term: row.track.clone(),
                        confidence: Some(Confidence::Exact),
                        selected_uri: Some("spotify:track:shared".into()),
                        candidates: Vec::new(),
                        track_matches: BTreeMap::from([(
                            row.stable_id.clone(),
                            "spotify:track:shared".into(),
                        )]),
                    })
                    .collect();
                service
                    .set_matches("user", "spotify", batch_id, results, None)
                    .await
            }
        }
    })
    .await
    .unwrap();

    assert_eq!(summary.album_entities, 0);
    assert_eq!(summary.track_entities, 1);
    assert_eq!(
        *order.lock().await,
        vec![
            ("Artist A".into(), "Album A".into()),
            ("Artist B".into(), "Album B".into()),
            ("Artist C".into(), "Album C".into()),
        ]
    );
}

#[tokio::test]
async fn collection_batches_do_not_search_spotify_automatically() {
    let client = retune_spotify::client::fake_client([], "");
    let rows = vec![SourceRow {
        stable_id: "artist\u{1f}\u{1f}one".into(),
        artist: "Various Artists".into(),
        album: String::new(),
        track: "One".into(),
        variants: Vec::new(),
        play_count: 1,
        earliest: 1,
        latest: 1,
    }];
    let matches = match_batch(&client, "Artist", "", true, &rows)
        .await
        .unwrap();
    let session = collection_session(&rows);
    assert!(collection_album_seed_rows(&session, 1, "Various Artists", "").is_none());
    assert!(matches.is_empty());
    assert!(client.transport().requests().is_empty());
}

#[tokio::test]
async fn named_collection_batches_seed_once_from_the_representative_release() {
    let mut rows = Vec::new();
    aggregate_scrobbles(
        &mut rows,
        &[
            scrobble("John Barry", "Out of Africa", "Main Title", 1),
            scrobble("John Barry", "Out of Africa", "Main Title", 2),
            scrobble("John Barry", "Out of Africa", "Safari", 3),
            scrobble("John Barry", "Out of Africa", "Safari", 4),
            scrobble(
                "John Barry",
                "Out of Africa (Soundtrack from the Motion Picture)",
                "Main Title",
                5,
            ),
            scrobble(
                "John Barry",
                "Out of Africa (Soundtrack from the Motion Picture)",
                "Safari",
                6,
            ),
        ],
    );
    let mut session = collection_session(&rows);
    let batch = review_batches(&session).remove(0);
    let projection = batch_projection(&batch, &batch_rows(&batch, &source_row_map(&session)));
    assert!(projection.collection_shaped);
    assert_eq!(projection.representative_album, "Out of Africa");

    let seed_rows = collection_album_seed_rows(
        &session,
        batch.page,
        &projection.representative_artist,
        &projection.representative_album,
    )
    .unwrap();
    assert_eq!(seed_rows.len(), 2);

    let client = fake_client(
        [
            album_search_response(vec![album_summary_json(
                "outofafrica",
                "Out of Africa",
                "John Barry",
                2,
            )]),
            album_response(
                "outofafrica",
                "Out of Africa",
                "John Barry",
                &["Main Title", "Safari"],
            ),
        ],
        "",
    );
    let (candidates, selected_uri) = automatic_collection_album_seed(
        &client,
        &projection.representative_artist,
        &projection.representative_album,
        &seed_rows,
    )
    .await
    .unwrap();
    assert_eq!(selected_uri.as_deref(), Some("spotify:album:outofafrica"));
    assert_eq!(candidates.len(), 1);
    assert_eq!(client.transport().requests().len(), 2);
    assert!(client.transport().requests()[0].url.contains("type=album"));
    assert!(client
        .transport()
        .requests()
        .iter()
        .all(|request| !request.url.contains("type=track")));

    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    session.collection_album_matches.clear();
    service.save(session).await.unwrap();
    service
        .seed_collection_albums(
            "user",
            "spotify",
            batch.page,
            &projection.representative_artist,
            candidates,
            None,
            &CollectionMembership::default(),
            &LastFmMappings::default(),
        )
        .await
        .unwrap();

    let seeded = service.snapshot().await.unwrap();
    assert_eq!(
        seeded.collection_album_matches[&batch.page].selected_album_uris,
        vec!["spotify:album:outofafrica"]
    );
    assert!(seeded.rows.iter().all(|row| {
        seeded
            .matches
            .get(&row.stable_id)
            .and_then(|result| matched_track_uri(result, &row.stable_id))
            .is_some()
    }));
    assert!(collection_album_seed_rows(
        &seeded,
        batch.page,
        &projection.representative_artist,
        &projection.representative_album,
    )
    .is_none());
    service
        .remove_collection_album(
            "user",
            "spotify",
            batch.page,
            &projection.representative_artist,
            "spotify:album:outofafrica",
            &CollectionMembership::default(),
            &LastFmMappings::default(),
        )
        .await
        .unwrap();
    service
        .rerank_collection_batch(
            batch.page,
            &CollectionMembership::default(),
            &LastFmMappings::default(),
        )
        .await
        .unwrap();
    let removed = service.snapshot().await.unwrap();
    assert!(removed.collection_album_matches[&batch.page]
        .selected_album_uris
        .is_empty());
    assert!(removed.collection_album_matches[&batch.page].automatic_selection_disabled);
}

#[tokio::test]
async fn collection_album_search_and_preview_use_bounded_requests_and_cached_edits_are_local() {
    let client = retune_spotify::client::fake_client(
        [
            retune_spotify::client::Response::json(
                200,
                serde_json::json!({
                    "albums": {
                        "items": [{
                            "id": "album",
                            "uri": "spotify:album:album",
                            "name": "Album",
                            "artists": [{"id": "artist", "name": "Artist"}],
                            "album_type": "album",
                            "release_date": "2024",
                            "total_tracks": 1,
                            "tracks": {"items": [], "next": null, "total": 1}
                        }],
                        "next": null,
                        "total": 1
                    }
                }),
            ),
            retune_spotify::client::Response::json(
                200,
                serde_json::json!({
                    "id": "album",
                    "uri": "spotify:album:album",
                    "name": "Album",
                    "artists": [{"id": "artist", "name": "Artist"}],
                    "release_date": "2024",
                    "total_tracks": 1,
                    "tracks": {
                        "items": [{
                            "uri": "spotify:track:one",
                            "name": "One",
                            "artists": [{"id": "artist", "name": "Artist"}],
                            "track_number": 1,
                            "duration_ms": 180000
                        }],
                        "next": null,
                        "total": 1
                    }
                }),
            ),
        ],
        "",
    );
    let summaries = crate::provider::search_albums(&client, "Artist Album")
        .await
        .unwrap();
    assert_eq!(summaries.items.len(), 1);
    assert!(client.transport().requests()[0].url.contains("type=album"));
    let album = client.album("album").await.unwrap();
    let candidate = collection_album_candidate(&album, &CollectionMembership::default());
    assert_eq!(candidate.matching.track_uris, vec!["spotify:track:one"]);

    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    service
        .save(collection_session(&[collection_test_row("One")]))
        .await
        .unwrap();
    service
        .cache_collection_album("user", "spotify", 1, "Artist", candidate.clone())
        .await
        .unwrap();
    let cached_revisit = service.page(1, "Artist", "").await.unwrap();
    assert_eq!(cached_revisit.collection.unwrap().cached_albums.len(), 1);
    service
        .add_collection_album(
            "user",
            "spotify",
            1,
            "Artist",
            &candidate.matching.uri,
            None,
            &CollectionMembership::default(),
            &LastFmMappings::default(),
        )
        .await
        .unwrap();
    assert!(service
        .remove_collection_album(
            "user",
            "spotify",
            1,
            "Artist",
            "spotify:album:missing",
            &CollectionMembership::default(),
            &LastFmMappings::default(),
        )
        .await
        .is_err());
    assert_eq!(
        service.snapshot().await.unwrap().collection_album_matches[&1].selected_album_uris,
        vec![candidate.matching.uri.clone()]
    );
    service
        .remove_collection_album(
            "user",
            "spotify",
            1,
            "Artist",
            &candidate.matching.uri,
            &CollectionMembership::default(),
            &LastFmMappings::default(),
        )
        .await
        .unwrap();
    assert_eq!(client.transport().requests().len(), 2);
    assert!(client
        .transport()
        .requests()
        .iter()
        .all(|request| request.method == retune_spotify::client::Method::Get));
    assert!(service
        .add_collection_album(
            "user",
            "spotify",
            1,
            "Artist",
            "spotify:album:uncached",
            None,
            &CollectionMembership::default(),
            &LastFmMappings::default(),
        )
        .await
        .is_err());
    assert!(
        service.snapshot().await.unwrap().collection_album_matches[&1]
            .selected_album_uris
            .is_empty()
    );
}

#[tokio::test]
async fn direct_collection_add_fetches_once_then_cached_add_is_local() {
    let client = retune_spotify::client::fake_client(
        [retune_spotify::client::Response::json(
            200,
            serde_json::json!({
                "id": "album",
                "uri": "spotify:album:album",
                "name": "Album",
                "artists": [{"id": "artist", "name": "Artist"}],
                "release_date": "2024",
                "total_tracks": 1,
                "tracks": {
                    "items": [{
                        "uri": "spotify:track:one",
                        "name": "One",
                        "artists": [{"id": "artist", "name": "Artist"}],
                        "track_number": 1,
                        "duration_ms": 180000
                    }],
                    "next": null,
                    "total": 1
                }
            }),
        )],
        "",
    );
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    service
        .save(collection_session(&[collection_test_row("One")]))
        .await
        .unwrap();

    let album = fetch_complete_collection_album(&client, "spotify:album:album")
        .await
        .unwrap();
    let candidate = collection_album_candidate(&album, &CollectionMembership::default());
    service
        .add_collection_album(
            "user",
            "spotify",
            1,
            "Artist",
            "spotify:album:album",
            Some(candidate),
            &CollectionMembership::default(),
            &LastFmMappings::default(),
        )
        .await
        .unwrap();
    assert_eq!(client.transport().requests().len(), 1);
    assert!(client
        .transport()
        .requests()
        .iter()
        .all(|request| request.method == retune_spotify::client::Method::Get));
    let snapshot = service.snapshot().await.unwrap();
    assert_eq!(
        snapshot.collection_album_matches[&1].selected_album_uris,
        vec![String::from("spotify:album:album")]
    );
    assert_eq!(
        snapshot
            .matches
            .values()
            .next()
            .unwrap()
            .track_matches
            .values()
            .next(),
        Some(&String::from("spotify:track:one"))
    );

    service
        .remove_collection_album(
            "user",
            "spotify",
            1,
            "Artist",
            "spotify:album:album",
            &CollectionMembership::default(),
            &LastFmMappings::default(),
        )
        .await
        .unwrap();
    service
        .add_collection_album(
            "user",
            "spotify",
            1,
            "Artist",
            "spotify:album:album",
            None,
            &CollectionMembership::default(),
            &LastFmMappings::default(),
        )
        .await
        .unwrap();
    assert_eq!(client.transport().requests().len(), 1);

    let incomplete_client = retune_spotify::client::fake_client(
        [retune_spotify::client::Response::json(
            200,
            serde_json::json!({
                "id": "album",
                "uri": "spotify:album:album",
                "name": "Album",
                "total_tracks": 2,
                "tracks": {"items": [{"uri": "spotify:track:one", "name": "One"}], "next": null, "total": 2}
            }),
        )],
        "",
    );
    assert!(
        fetch_complete_collection_album(&incomplete_client, "spotify:album:album")
            .await
            .is_err()
    );
    assert_eq!(incomplete_client.transport().requests().len(), 1);
    assert!(incomplete_client
        .transport()
        .requests()
        .iter()
        .all(|request| request.method == retune_spotify::client::Method::Get));
    let failed_dir = tempfile::tempdir().unwrap();
    let failed_service = Service::new(failed_dir.path());
    failed_service
        .save(collection_session(&[collection_test_row("One")]))
        .await
        .unwrap();

    let failing_client = retune_spotify::client::fake_client(
        [retune_spotify::client::Response::json(
            404,
            serde_json::json!({}),
        )],
        "",
    );
    assert!(
        fetch_complete_collection_album(&failing_client, "spotify:album:failed")
            .await
            .is_err()
    );
    assert!(failing_client
        .transport()
        .requests()
        .iter()
        .all(|request| request.method == retune_spotify::client::Method::Get));
    let failed_session = failed_service.snapshot().await.unwrap();
    assert!(failed_session
        .collection_album_matches
        .get(&1)
        .map(|state| state.selected_album_uris.is_empty())
        .unwrap_or(true));
}

#[tokio::test]
async fn literal_singles_release_batch_keeps_album_matching() {
    let client = retune_spotify::client::fake_client(
        [
            retune_spotify::client::Response::json(
                200,
                serde_json::json!({
                    "albums": {
                        "items": [{
                            "id": "album",
                            "uri": "spotify:album:album",
                            "name": "Singles",
                            "artists": [{"id": "artist", "name": "Artist"}],
                            "album_type": "album",
                            "release_date": "2024",
                            "total_tracks": 1,
                            "tracks": {"items": [], "next": null, "total": 1}
                        }],
                        "next": null,
                        "total": 1
                    }
                }),
            ),
            retune_spotify::client::Response::json(
                200,
                serde_json::json!({
                    "id": "album",
                    "uri": "spotify:album:album",
                    "name": "Singles",
                    "artists": [],
                    "release_date": "2024",
                    "total_tracks": 1,
                    "tracks": {
                        "items": [{
                            "uri": "spotify:track:one",
                            "name": "One",
                            "artists": []
                        }],
                        "next": null,
                        "total": 1
                    }
                }),
            ),
        ],
        "",
    );
    let rows = vec![SourceRow {
        stable_id: source_id("Artist", "Singles", "One"),
        artist: "Artist".into(),
        album: "Singles".into(),
        track: "One".into(),
        variants: Vec::new(),
        play_count: 1,
        earliest: 1,
        latest: 1,
    }];
    let matches = match_batch(&client, "Artist", "Singles", false, &rows)
        .await
        .unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(
        matches[0].search_term,
        album_search_term("Artist", "Singles")
    );
    assert_eq!(matches[0].candidates.len(), 1);
    assert_eq!(matches[0].candidates[0].uri, "spotify:album:album");
    assert_eq!(
        matches[0].candidates[0].relation,
        Some(AlbumRelation::BestMatch)
    );
    let requests = client.transport().requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].url.contains("type=album"));
    assert!(requests[1].url.contains("/albums/album"));
    assert!(requests
        .iter()
        .all(|request| !request.url.contains("type=track")));
}

#[tokio::test]
async fn unsupported_album_summaries_do_not_hydrate_tracks() {
    let client = fake_client(
        [album_search_response(vec![album_summary_json(
            "unsupported",
            "Unrelated Release",
            "Artist",
            3,
        )])],
        "",
    );
    let source_tracks = vec!["One".to_owned(), "Two".to_owned()];

    let candidates = album_candidates(
        &client,
        "album search",
        Some("Wanted Release"),
        Some("Artist"),
        &source_tracks,
    )
    .await
    .unwrap();

    assert!(candidates.is_empty());
    let requests = client.transport().requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].url.contains("type=album"));
    assert!(!requests[0].url.contains("/albums/"));
}

#[tokio::test]
async fn smaller_release_is_hydrated_and_selected_when_source_rows_collapse() {
    let eleven_tracks = [
        "Briefly",
        "I Do",
        "Juarez",
        "Rolling",
        "A Lifetime",
        "Recognize",
        "Get You In",
        "Sincerely, Me",
        "Extra Ordinary",
        "King of New Orleans",
        "Closer",
    ];
    let mut thirteen_tracks = eleven_tracks.to_vec();
    thirteen_tracks.extend(["Bonus One", "Bonus Two"]);
    let source_tracks = eleven_tracks
        .into_iter()
        .chain([
            "Briefly - Closer",
            "I Do - Closer",
            "Juarez - Closer",
            "Rolling - Closer",
            "A Lifetime - Closer",
        ])
        .collect::<Vec<_>>();
    let rows = source_tracks
        .iter()
        .enumerate()
        .map(|(index, track)| SourceRow {
            stable_id: format!("closer-{index}"),
            artist: "Better Than Ezra".into(),
            album: "Closer".into(),
            track: (*track).into(),
            variants: Vec::new(),
            play_count: 1,
            earliest: 1,
            latest: 1,
        })
        .collect::<Vec<_>>();
    let client = fake_client(
        [
            album_search_response(vec![
                album_summary_json("thirteen", "Closer", "Better Than Ezra", 13),
                album_summary_json("eleven", "Closer", "Better Than Ezra", 11),
            ]),
            album_response("thirteen", "Closer", "Better Than Ezra", &thirteen_tracks),
            album_response("eleven", "Closer", "Better Than Ezra", &eleven_tracks),
        ],
        "",
    );

    let matches = match_batch(&client, "Better Than Ezra", "Closer", false, &rows)
        .await
        .unwrap();

    assert!(matches
        .iter()
        .all(|result| result.selected_uri.as_deref() == Some("spotify:album:eleven")));
    assert_eq!(client.transport().requests().len(), 3);
}

#[tokio::test]
async fn album_summary_gate_uses_strongest_tier_and_hydrates_only_three_in_order() {
    let summaries = vec![
        album_summary_json("wrongartist", "Target", "Other Artist", 3),
        album_summary_json("compatibleexact", "Target", "Artist", 3),
        album_summary_json("compatiblelarger", "Target", "Artist", 4),
        album_summary_json("compatiblelargest", "Target", "Artist", 5),
        album_summary_json("contained", "Target Deluxe", "Artist", 3),
    ];
    let source_tracks = vec!["One".to_owned(), "Two".to_owned(), "Three".to_owned()];
    let client = fake_client(
        [
            album_search_response(summaries),
            album_response(
                "compatibleexact",
                "Target",
                "Artist",
                &["One", "Two", "Three"],
            ),
            album_response(
                "wrongartist",
                "Target",
                "Other Artist",
                &["One", "Two", "Three"],
            ),
            album_response(
                "compatiblelarger",
                "Target",
                "Artist",
                &["One", "Two", "Three"],
            ),
        ],
        "",
    );

    let candidates = album_candidates(
        &client,
        "album search",
        Some("Target"),
        Some("Artist"),
        &source_tracks,
    )
    .await
    .unwrap();

    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.uri.as_str())
            .collect::<Vec<_>>(),
        vec![
            "spotify:album:compatibleexact",
            "spotify:album:wrongartist",
            "spotify:album:compatiblelarger",
        ]
    );
    assert_eq!(client.transport().requests().len(), 4);
    assert!(client
        .transport()
        .requests()
        .iter()
        .skip(1)
        .all(|request| request.url.contains("/albums/")));
}

#[tokio::test]
async fn christmas_album_alias_is_supported_by_summary_title_evidence() {
    let source_tracks = vec!["Song One".to_owned(), "Song Two".to_owned()];
    let client = fake_client(
        [
            album_search_response(vec![album_summary_json(
                "sounds",
                "Sounds of Christmas",
                "Johnny Mathis",
                12,
            )]),
            album_response(
                "sounds",
                "Sounds of Christmas",
                "Johnny Mathis",
                &["Song One", "Song Two"],
            ),
        ],
        "",
    );

    let candidates = album_candidates(
        &client,
        "album search",
        Some("For Christmas"),
        Some("Johnny Mathis"),
        &source_tracks,
    )
    .await
    .unwrap();

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].name, "Sounds of Christmas");
    assert_eq!(client.transport().requests().len(), 2);
}

#[tokio::test]
async fn explicit_album_search_hydrates_an_unsupported_named_summary() {
    let client = fake_client(
        [
            album_search_response(vec![album_summary_json(
                "alternate",
                "Alternate Release",
                "Artist",
                1,
            )]),
            album_response("alternate", "Alternate Release", "Artist", &["One"]),
        ],
        "",
    );

    let candidates = album_candidates(
        &client,
        "alternate release",
        None,
        None,
        &["One".to_owned()],
    )
    .await
    .unwrap();

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].uri, "spotify:album:alternate");
    assert_eq!(client.transport().requests().len(), 2);
}

#[test]
fn collection_match_set_maps_unique_union_and_deduplicates_track_uris() {
    let rows = vec![collection_test_row("One"), collection_test_row("Two")];
    let mut session = collection_session(&rows);
    let first = collection_album(
        "spotify:album:first",
        "Artist",
        &[
            ("One", "spotify:track:one"),
            ("Shared", "spotify:track:shared"),
        ],
    );
    let second = collection_album(
        "spotify:album:second",
        "Artist",
        &[
            ("Shared", "spotify:track:shared"),
            ("Two", "spotify:track:two"),
        ],
    );
    session.collection_album_matches.insert(
        1,
        CollectionAlbumMatchState {
            cached_candidates: vec![first.clone(), second.clone()],
            selected_album_uris: vec![first.matching.uri.clone(), second.matching.uri.clone()],
            ..CollectionAlbumMatchState::default()
        },
    );
    session.batches[0].representative_album = Some("Grouped Release".into());
    let selected = collection_selected_albums(&session, 1);
    let candidates = collection_track_candidates(&selected, &CollectionMembership::default());
    assert_eq!(candidates.len(), 3);

    for row in &rows {
        let result = ratify_collection_result_with_selected_albums(
            row,
            collection_match(row),
            &selected,
            &CollectionMembership::default(),
            &LastFmMappings::default(),
        );
        let expected = match row.track.as_str() {
            "One" => "spotify:track:one",
            "Two" => "spotify:track:two",
            _ => unreachable!(),
        };
        assert_eq!(result.confidence, Some(Confidence::Exact));
        assert_eq!(
            result.track_matches.get(&row.stable_id).map(String::as_str),
            Some(expected)
        );
    }

    session.matches.remove(&rows[0].stable_id);
    let identity =
        select_match_in_session(&mut session, 1, &rows[0].stable_id, "spotify:track:shared")
            .unwrap();
    assert_eq!(identity, ("Artist".into(), "Grouped Release".into()));
    let result = &session.matches[&rows[0].stable_id];
    assert_eq!(result.search_term, "track:\"One\" artist:\"Artist\"");
    assert_eq!(
        result
            .track_matches
            .get(&rows[0].stable_id)
            .map(String::as_str),
        Some("spotify:track:shared")
    );
    assert!(result
        .candidates
        .iter()
        .any(|candidate| candidate.uri == "spotify:track:shared"));
    assert!(select_match_in_session(
        &mut session,
        1,
        &rows[0].stable_id,
        "spotify:track:not-selected",
    )
    .is_err());
}

#[test]
fn collection_match_set_keeps_distinct_editions_ambiguous() {
    let row = collection_test_row("One");
    let first = collection_album(
        "spotify:album:first",
        "Artist",
        &[("One", "spotify:track:one-a")],
    );
    let second = collection_album(
        "spotify:album:second",
        "Artist",
        &[("One", "spotify:track:one-b")],
    );
    let result = ratify_collection_result_with_selected_albums(
        &row,
        collection_match(&row),
        &[&first, &second],
        &CollectionMembership::default(),
        &LastFmMappings::default(),
    );
    assert_eq!(result.confidence, None);
    assert!(result.track_matches.is_empty());
    assert_eq!(
        result
            .candidates
            .iter()
            .map(|candidate| candidate.uri.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["spotify:track:one-a", "spotify:track:one-b"])
    );
}

#[test]
fn collection_match_precedence_preserves_accepted_and_manual_choices() {
    let row = collection_test_row("One");
    let album = collection_album(
        "spotify:album:selected",
        "Artist",
        &[("One", "spotify:track:album")],
    );
    let accepted_uri = "spotify:track:accepted";
    let accepted = LastFmMappings {
        track_mappings: BTreeMap::from([(source_id("Artist", "", "One"), accepted_uri.into())]),
        ..LastFmMappings::default()
    };
    let accepted_result = ratify_collection_result_with_selected_albums(
        &row,
        collection_match(&row),
        &[&album],
        &CollectionMembership::default(),
        &accepted,
    );
    assert_eq!(
        accepted_result.track_matches.get(&row.stable_id),
        Some(&accepted_uri.to_owned())
    );

    let manual_uri = "spotify:track:manual";
    let mut manual = collection_match(&row);
    manual.selected_uri = Some(manual_uri.into());
    manual.candidates.push(AlbumCandidate {
        uri: manual_uri.into(),
        name: row.track.clone(),
        artist: row.artist.clone(),
        track_uris: vec![manual_uri.into()],
        track_names: vec![row.track.clone()],
        track_artists: vec![row.artist.clone()],
        track_albums: vec![String::new()],
        ..AlbumCandidate::default()
    });
    let manual_result = ratify_collection_result_with_selected_albums(
        &row,
        manual,
        &[&album],
        &CollectionMembership::default(),
        &LastFmMappings::default(),
    );
    assert_eq!(manual_result.selected_uri.as_deref(), Some(manual_uri));
    assert!(manual_result.track_matches.is_empty());
}

#[test]
fn collection_match_set_recomputes_automatic_rows_on_add_and_remove() {
    let row = collection_test_row("One");
    let first = collection_album(
        "spotify:album:first",
        "Artist",
        &[("One", "spotify:track:first")],
    );
    let second = collection_album(
        "spotify:album:second",
        "Artist",
        &[("One", "spotify:track:second")],
    );
    let mut session = collection_session(std::slice::from_ref(&row));
    session.collection_album_matches.insert(
        1,
        CollectionAlbumMatchState {
            cached_candidates: vec![first.clone(), second.clone()],
            selected_album_uris: vec![first.matching.uri.clone()],
            ..CollectionAlbumMatchState::default()
        },
    );
    let membership = CollectionMembership::default();
    let mappings = LastFmMappings::default();
    session.matches.remove(&row.stable_id);
    rerank_collection_session(&mut session, 1, &membership, &mappings).unwrap();
    assert_eq!(
        session.matches[&row.stable_id]
            .track_matches
            .get(&row.stable_id)
            .map(String::as_str),
        Some("spotify:track:first")
    );

    session
        .collection_album_matches
        .get_mut(&1)
        .unwrap()
        .selected_album_uris
        .push(second.matching.uri.clone());
    rerank_collection_session(&mut session, 1, &membership, &mappings).unwrap();
    assert!(session.matches[&row.stable_id].track_matches.is_empty());

    session
        .collection_album_matches
        .get_mut(&1)
        .unwrap()
        .selected_album_uris
        .retain(|uri| uri == &second.matching.uri);
    rerank_collection_session(&mut session, 1, &membership, &mappings).unwrap();
    assert_eq!(
        session.matches[&row.stable_id]
            .track_matches
            .get(&row.stable_id)
            .map(String::as_str),
        Some("spotify:track:second")
    );
}

#[test]
fn previewing_unselected_album_preserves_baseline_automatic_match() {
    let row = collection_test_row("One");
    let shared_uri = "spotify:track:shared";
    let preview = collection_album("spotify:album:preview", "Artist", &[("One", shared_uri)]);
    let mut baseline = collection_match(&row);
    baseline.candidates.push(AlbumCandidate {
        uri: shared_uri.into(),
        name: "One".into(),
        artist: "Artist".into(),
        track_uris: vec![shared_uri.into()],
        track_names: vec!["One".into()],
        track_artists: vec!["Artist".into()],
        track_albums: vec!["Baseline search result".into()],
        ..AlbumCandidate::default()
    });
    let membership = CollectionMembership::default();
    let mappings = LastFmMappings::default();
    let mut session = collection_session(std::slice::from_ref(&row));
    session.matches.insert(
        row.stable_id.clone(),
        ratify_collection_result(&row, baseline, &membership, &mappings),
    );
    session.collection_album_matches.insert(
        1,
        CollectionAlbumMatchState {
            cached_candidates: vec![preview],
            selected_album_uris: Vec::new(),
            ..CollectionAlbumMatchState::default()
        },
    );

    rerank_collection_session(&mut session, 1, &membership, &mappings).unwrap();
    let result = &session.matches[&row.stable_id];
    assert_eq!(
        result.track_matches.get(&row.stable_id).map(String::as_str),
        Some(shared_uri)
    );
    assert_eq!(
        result.candidates[0].track_albums,
        vec!["Baseline search result".to_owned()]
    );
    assert!(session.collection_album_matches[&1]
        .injected_candidate_uris
        .is_empty());
}

#[test]
fn selected_album_overlap_with_baseline_candidate_preserves_fallback_after_removal() {
    let row = collection_test_row("One");
    let shared_uri = "spotify:track:shared";
    let selected = collection_album("spotify:album:selected", "Artist", &[("One", shared_uri)]);
    let mut baseline = collection_match(&row);
    baseline.candidates.push(AlbumCandidate {
        uri: shared_uri.into(),
        name: "One".into(),
        artist: "Artist".into(),
        in_library: true,
        track_uris: vec![shared_uri.into()],
        track_names: vec!["One".into()],
        track_artists: vec!["Artist".into()],
        track_albums: vec!["Library fallback".into()],
        ..AlbumCandidate::default()
    });
    let membership = CollectionMembership {
        track_uris: BTreeSet::from([shared_uri.to_owned()]),
    };
    let mappings = LastFmMappings::default();
    let mut session = collection_session(std::slice::from_ref(&row));
    session.matches.insert(
        row.stable_id.clone(),
        ratify_collection_result(&row, baseline, &membership, &mappings),
    );
    session.collection_album_matches.insert(
        1,
        CollectionAlbumMatchState {
            cached_candidates: vec![selected.clone()],
            selected_album_uris: vec![selected.matching.uri.clone()],
            ..CollectionAlbumMatchState::default()
        },
    );

    rerank_collection_session(&mut session, 1, &membership, &mappings).unwrap();
    assert_eq!(
        session.matches[&row.stable_id]
            .track_matches
            .get(&row.stable_id)
            .map(String::as_str),
        Some(shared_uri)
    );
    assert_eq!(
        session.matches[&row.stable_id].candidates[0].track_albums,
        vec!["Library fallback".to_owned()]
    );
    assert!(session.collection_album_matches[&1]
        .injected_candidate_uris
        .is_empty());

    session
        .collection_album_matches
        .get_mut(&1)
        .unwrap()
        .selected_album_uris
        .clear();
    rerank_collection_session(&mut session, 1, &membership, &mappings).unwrap();
    assert_eq!(
        session.matches[&row.stable_id]
            .track_matches
            .get(&row.stable_id)
            .map(String::as_str),
        Some(shared_uri)
    );
    assert_eq!(
        session.matches[&row.stable_id].candidates[0].track_albums,
        vec!["Library fallback".to_owned()]
    );
}

#[tokio::test]
async fn removing_all_collection_albums_drops_old_automatic_matches_but_keeps_durable_choices() {
    let rows = vec![
        collection_test_row("One"),
        collection_test_row("Manual"),
        collection_test_row("Accepted"),
    ];
    let album = collection_album(
        "spotify:album:first",
        "Artist",
        &[("One", "spotify:track:album-one")],
    );
    let manual_uri = "spotify:track:manual";
    let accepted_uri = "spotify:track:accepted";
    let mut session = collection_session(&rows);
    session.collection_album_matches.insert(
        1,
        CollectionAlbumMatchState {
            cached_candidates: vec![album.clone()],
            selected_album_uris: vec![album.matching.uri.clone()],
            ..CollectionAlbumMatchState::default()
        },
    );
    let manual = session.matches.get_mut(&rows[1].stable_id).unwrap();
    manual.selected_uri = Some(manual_uri.into());
    manual.candidates.push(AlbumCandidate {
        uri: manual_uri.into(),
        name: "Manual".into(),
        artist: "Artist".into(),
        track_uris: vec![manual_uri.into()],
        track_names: vec!["Manual".into()],
        track_artists: vec!["Artist".into()],
        track_albums: vec![String::new()],
        ..AlbumCandidate::default()
    });
    let mappings = LastFmMappings {
        track_mappings: BTreeMap::from([(
            source_id("Artist", "", "Accepted"),
            accepted_uri.into(),
        )]),
        ..LastFmMappings::default()
    };
    rerank_collection_session(&mut session, 1, &CollectionMembership::default(), &mappings)
        .unwrap();
    assert_eq!(
        session.matches[&rows[0].stable_id]
            .track_matches
            .get(&rows[0].stable_id)
            .map(String::as_str),
        Some("spotify:track:album-one")
    );
    assert_eq!(
        session.collection_album_matches[&1].injected_candidate_uris[&rows[0].stable_id],
        BTreeSet::from(["spotify:track:album-one".to_owned()])
    );

    session
        .collection_album_matches
        .get_mut(&1)
        .unwrap()
        .selected_album_uris
        .clear();
    rerank_collection_session(&mut session, 1, &CollectionMembership::default(), &mappings)
        .unwrap();
    assert!(!session.matches[&rows[0].stable_id]
        .track_matches
        .contains_key(&rows[0].stable_id));
    assert!(!session.matches[&rows[0].stable_id]
        .candidates
        .iter()
        .any(|candidate| candidate.uri == "spotify:track:album-one"));
    assert!(session.collection_album_matches[&1]
        .injected_candidate_uris
        .is_empty());
    assert_eq!(
        session.matches[&rows[1].stable_id].selected_uri.as_deref(),
        Some(manual_uri)
    );
    assert_eq!(
        session.matches[&rows[2].stable_id]
            .track_matches
            .get(&rows[2].stable_id)
            .map(String::as_str),
        Some(accepted_uri)
    );
}

#[tokio::test]
async fn collection_album_cache_validates_source_artist_not_spotify_album_artist() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    service
        .save(collection_session(&[collection_test_row("One")]))
        .await
        .unwrap();
    let candidate = collection_album(
        "spotify:album:various",
        "Various Artists",
        &[("One", "spotify:track:one")],
    );
    service
        .cache_collection_album("user", "spotify", 1, "Other Artist", candidate.clone())
        .await
        .unwrap_err();
    service
        .cache_collection_album("user", "spotify", 1, "Artist", candidate.clone())
        .await
        .unwrap();
    assert_eq!(
        service.snapshot().await.unwrap().collection_album_matches[&1].cached_candidates,
        vec![candidate.clone()]
    );
    let stale = collection_album(
        "spotify:album:stale",
        "Various Artists",
        &[("One", "spotify:track:stale")],
    );
    assert!(service
        .cache_collection_album("user", "spotify", 1, "Other Artist", stale)
        .await
        .is_err());
    assert_eq!(
        service.snapshot().await.unwrap().collection_album_matches[&1].cached_candidates,
        vec![candidate]
    );
}

#[test]
fn selected_album_exact_title_outweighs_track_artist_credit() {
    let row = collection_test_row("One");
    let wrong_artist = collection_album(
        "spotify:album:other",
        "Other Artist",
        &[("One", "spotify:track:other")],
    );
    let result = ratify_collection_result_with_selected_albums(
        &row,
        collection_match(&row),
        &[&wrong_artist],
        &CollectionMembership::default(),
        &LastFmMappings::default(),
    );
    assert_eq!(result.confidence, Some(Confidence::Exact));
    assert_eq!(
        result.track_matches.get(&row.stable_id).map(String::as_str),
        Some("spotify:track:other")
    );
    assert_eq!(
        result.candidates[0].relation,
        Some(AlbumRelation::BestMatch)
    );

    let mut manually_selected = collection_match(&row);
    manually_selected.selected_uri = Some("spotify:track:other".into());
    manually_selected.confidence = Some(Confidence::Low);
    manually_selected.track_matches =
        BTreeMap::from([(row.stable_id.clone(), "spotify:track:other".into())]);
    manually_selected.candidates =
        collection_track_candidates(&[&wrong_artist], &CollectionMembership::default());
    let result = ratify_collection_result_with_selected_albums(
        &row,
        manually_selected,
        &[&wrong_artist],
        &CollectionMembership::default(),
        &LastFmMappings::default(),
    );
    assert_eq!(result.selected_uri.as_deref(), Some("spotify:track:other"));
    assert_eq!(result.confidence, Some(Confidence::Exact));
    assert_eq!(
        result.candidates[0].relation,
        Some(AlbumRelation::BestMatch)
    );

    let mut ordinary_search = collection_match(&row);
    ordinary_search.candidates =
        collection_track_candidates(&[&wrong_artist], &CollectionMembership::default());
    let result = ratify_collection_result(
        &row,
        ordinary_search,
        &CollectionMembership::default(),
        &LastFmMappings::default(),
    );
    assert_eq!(result.confidence, Some(Confidence::Exact));
    assert_eq!(
        result.track_matches.get(&row.stable_id).map(String::as_str),
        Some("spotify:track:other")
    );
    assert_eq!(
        result.candidates[0].relation,
        Some(AlbumRelation::BestMatch)
    );
}

#[tokio::test]
async fn selecting_match_persists_and_projects_remaining_work_first() {
    let rows = vec![
        collection_test_row("Already"),
        collection_test_row("Needs"),
        collection_test_row("Later"),
    ];
    let mut session = collection_session(&rows);
    session
        .matches
        .get_mut(&rows[0].stable_id)
        .unwrap()
        .track_matches =
        BTreeMap::from([(rows[0].stable_id.clone(), "spotify:track:already".into())]);
    session
        .matches
        .get_mut(&rows[1].stable_id)
        .unwrap()
        .candidates = vec![exact_collection_track(&rows[1], "spotify:track:needs")];
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    service.save(session).await.unwrap();

    let before = service.page(1, "Artist", "").await.unwrap();
    assert_eq!(
        before
            .rows
            .iter()
            .map(|item| item.source.track.as_str())
            .collect::<Vec<_>>(),
        vec!["Needs", "Later", "Already"]
    );

    service
        .select_match(
            "user",
            "spotify",
            1,
            &rows[1].stable_id,
            "spotify:track:needs",
        )
        .await
        .unwrap();
    let after = service.page(1, "Artist", "").await.unwrap();
    assert_eq!(
        after
            .rows
            .iter()
            .map(|item| item.source.track.as_str())
            .collect::<Vec<_>>(),
        vec!["Later", "Already", "Needs"]
    );
    assert_eq!(
        after.rows[2]
            .match_result
            .as_ref()
            .and_then(|result| result.track_matches.get(&rows[1].stable_id))
            .map(String::as_str),
        Some("spotify:track:needs")
    );
    assert_eq!(
        service.snapshot().await.unwrap().matches[&rows[1].stable_id]
            .track_matches
            .get(&rows[1].stable_id)
            .map(String::as_str),
        Some("spotify:track:needs")
    );
}

#[tokio::test]
async fn selecting_suggested_matches_is_atomic_and_projects_the_updated_page() {
    let rows = vec![collection_test_row("One"), collection_test_row("Two")];
    let mut session = collection_session(&rows);
    for (row, uri) in rows.iter().zip(["spotify:track:one", "spotify:track:two"]) {
        session.matches.get_mut(&row.stable_id).unwrap().candidates =
            vec![exact_collection_track(row, uri)];
    }
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    service.save(session).await.unwrap();

    assert!(service
        .select_matches(
            "user",
            "spotify",
            1,
            &[
                (rows[0].stable_id.clone(), "spotify:track:one".into()),
                (rows[1].stable_id.clone(), "spotify:track:missing".into()),
            ],
        )
        .await
        .is_err());
    assert!(
        service.snapshot().await.unwrap().matches[&rows[0].stable_id]
            .track_matches
            .is_empty()
    );

    service
        .select_matches(
            "user",
            "spotify",
            1,
            &[
                (rows[0].stable_id.clone(), "spotify:track:one".into()),
                (rows[1].stable_id.clone(), "spotify:track:two".into()),
            ],
        )
        .await
        .unwrap();
    let page = service.page(1, "Artist", "").await.unwrap();
    assert!(page.rows.iter().all(|item| {
        item.match_result
            .as_ref()
            .and_then(|result| result.track_matches.get(&item.source.stable_id))
            .is_some()
    }));
}

#[test]
fn legacy_v2_session_defaults_collection_match_state() {
    let session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 100);
    let mut value = serde_json::to_value(session).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .remove("collectionAlbumMatches");
    let restored: LastFmImportSessionV2 = serde_json::from_value(value).unwrap();
    assert!(restored.collection_album_matches.is_empty());
}

#[test]
fn legacy_v2_session_defaults_collection_match_metadata() {
    let row = collection_test_row("One");
    let mut session = collection_session(std::slice::from_ref(&row));
    session.collection_album_matches.insert(
        1,
        CollectionAlbumMatchState {
            cached_candidates: Vec::new(),
            selected_album_uris: Vec::new(),
            automatic_selection_disabled: false,
            injected_candidate_uris: BTreeMap::from([(
                row.stable_id.clone(),
                BTreeSet::from(["spotify:track:injected".to_owned()]),
            )]),
        },
    );
    let mut value = serde_json::to_value(session).unwrap();
    let state = value["collectionAlbumMatches"]["1"]
        .as_object_mut()
        .unwrap();
    state.remove("automaticSelectionDisabled");
    state.remove("injectedCandidateUris");
    let restored: LastFmImportSessionV2 = serde_json::from_value(value).unwrap();
    assert!(!restored.collection_album_matches[&1].automatic_selection_disabled);
    assert!(restored.collection_album_matches[&1]
        .injected_candidate_uris
        .is_empty());
}

#[test]
fn collection_apply_plan_requires_one_selected_cached_album_and_uses_its_membership() {
    let rows = vec![collection_test_row("One"), collection_test_row("Two")];
    let first = collection_album(
        "spotify:album:first",
        "Artist",
        &[
            ("One", "spotify:track:first-one"),
            ("Two", "spotify:track:first-two"),
        ],
    );
    let second = collection_album(
        "spotify:album:second",
        "Artist",
        &[
            ("One", "spotify:track:second-one"),
            ("Two", "spotify:track:second-two"),
        ],
    );
    let mut session = collection_session(&rows);
    for (row, uri) in rows
        .iter()
        .zip(["spotify:track:first-one", "spotify:track:first-two"])
    {
        session
            .matches
            .get_mut(&row.stable_id)
            .unwrap()
            .track_matches = BTreeMap::from([(row.stable_id.clone(), uri.into())]);
    }
    session.collection_album_matches.insert(
        1,
        CollectionAlbumMatchState {
            cached_candidates: vec![first.clone(), second],
            selected_album_uris: vec![first.matching.uri.clone(), "spotify:album:second".into()],
            ..CollectionAlbumMatchState::default()
        },
    );
    let selected_ids = rows
        .iter()
        .map(|row| row.stable_id.clone())
        .collect::<Vec<_>>();
    let options = PageOptions {
        whole_album: true,
        ..PageOptions::default()
    };
    let error = build_apply_plan(
        &session,
        "spotify",
        1,
        "Artist",
        "",
        &selected_ids,
        false,
        options.clone(),
    )
    .unwrap_err();
    assert!(error.contains("one coherent Spotify album"));

    session
        .collection_album_matches
        .get_mut(&1)
        .unwrap()
        .selected_album_uris = vec!["spotify:album:first".into()];
    let plan = build_apply_plan(
        &session,
        "spotify",
        1,
        "Artist",
        "",
        &selected_ids,
        false,
        options,
    )
    .unwrap();
    assert_eq!(
        plan.membership,
        ApplyMembership::Album {
            uri: "spotify:album:first".into(),
            name: first.matching.name.clone(),
            artist: "Artist".into(),
        }
    );
    assert_eq!(
        plan.metadata_uris.into_iter().collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "spotify:track:first-one".to_owned(),
            "spotify:track:first-two".to_owned(),
        ])
    );
    assert!(plan
        .mappings
        .iter()
        .all(|mapping| mapping.album_uri.as_deref() == Some("spotify:album:first")));
}

#[test]
fn collection_whole_album_guard_requires_one_complete_coherent_album() {
    let rows = vec![collection_test_row("One"), collection_test_row("Two")];
    let album = collection_album(
        "spotify:album:complete",
        "Artist",
        &[("One", "spotify:track:one"), ("Two", "spotify:track:two")],
    );
    let mut session = collection_session(&rows);
    session.collection_album_matches.insert(
        1,
        CollectionAlbumMatchState {
            cached_candidates: vec![album.clone()],
            selected_album_uris: vec![album.matching.uri.clone()],
            ..CollectionAlbumMatchState::default()
        },
    );
    for row in &rows {
        session
            .matches
            .get_mut(&row.stable_id)
            .unwrap()
            .track_matches = BTreeMap::from([(
            row.stable_id.clone(),
            format!("spotify:track:{}", row.track.to_lowercase()),
        )]);
    }
    let refs = rows.iter().collect::<Vec<_>>();
    assert!(exact_album_match_for_rows(&session, 1, &refs));

    session
        .matches
        .get_mut(&rows[1].stable_id)
        .unwrap()
        .track_matches
        .clear();
    assert!(!exact_album_match_for_rows(&session, 1, &refs));

    session
        .matches
        .get_mut(&rows[1].stable_id)
        .unwrap()
        .track_matches
        .insert(rows[1].stable_id.clone(), "spotify:track:two".into());
    session
        .collection_album_matches
        .get_mut(&1)
        .unwrap()
        .selected_album_uris
        .push("spotify:album:missing".into());
    assert!(!exact_album_match_for_rows(&session, 1, &refs));
}

#[test]
fn collection_projection_does_not_enable_whole_album_for_outside_durable_match() {
    let rows = vec![collection_test_row("One"), collection_test_row("Two")];
    let album = collection_album(
        "spotify:album:one",
        "Artist",
        &[("One", "spotify:track:one")],
    );
    let mut session = collection_session(&rows);
    session.collection_album_matches.insert(
        1,
        CollectionAlbumMatchState {
            cached_candidates: vec![album.clone()],
            selected_album_uris: vec![album.matching.uri.clone()],
            ..CollectionAlbumMatchState::default()
        },
    );
    session
        .matches
        .get_mut(&rows[0].stable_id)
        .unwrap()
        .track_matches = BTreeMap::from([(rows[0].stable_id.clone(), "spotify:track:one".into())]);
    session
        .matches
        .get_mut(&rows[1].stable_id)
        .unwrap()
        .track_matches =
        BTreeMap::from([(rows[1].stable_id.clone(), "spotify:track:outside".into())]);
    let refs = rows.iter().collect::<Vec<_>>();
    let view = collection_match_view(&session, 1, &refs);
    assert_eq!(view.coverage.matched, 2);
    assert_eq!(view.coverage.ambiguous, 0);
    assert_eq!(view.coverage.unresolved, 0);
    assert!(!view.whole_album_ready);
}

#[test]
fn collection_projection_reports_per_track_match_status_and_selected_coverage() {
    let rows = vec![collection_test_row("One"), collection_test_row("Missing")];
    let first = collection_album(
        "spotify:album:first",
        "Artist",
        &[
            ("One", "spotify:track:one-a"),
            ("Missing", "spotify:track:missing"),
            ("Unused", "spotify:track:unused"),
        ],
    );
    let second = collection_album(
        "spotify:album:second",
        "Artist",
        &[("One", "spotify:track:one-b")],
    );
    let mut session = collection_session(&rows);
    session.collection_album_matches.insert(
        1,
        CollectionAlbumMatchState {
            cached_candidates: vec![first.clone(), second],
            selected_album_uris: vec![first.matching.uri.clone(), "spotify:album:second".into()],
            ..CollectionAlbumMatchState::default()
        },
    );
    let refs = rows.iter().collect::<Vec<_>>();
    let view = collection_match_view(&session, 1, &refs);
    let preview = view
        .coverage
        .previews
        .iter()
        .find(|preview| preview.uri == first.matching.uri)
        .unwrap();
    assert!(preview.selected);
    assert_eq!(preview.matched, 1);
    assert_eq!(preview.unique_coverage, 1);
    assert_eq!(
        preview
            .track_statuses
            .iter()
            .map(|track| (&track.uri, &track.status))
            .collect::<Vec<_>>(),
        vec![
            (
                &"spotify:track:one-a".to_owned(),
                &CollectionTrackMatchStatus::Ambiguous
            ),
            (
                &"spotify:track:missing".to_owned(),
                &CollectionTrackMatchStatus::Matched
            ),
            (
                &"spotify:track:unused".to_owned(),
                &CollectionTrackMatchStatus::Unmatched
            ),
        ]
    );
}

#[test]
fn collection_projection_coverage_cases_are_authoritative() {
    struct Expected {
        aggregate: (usize, usize, usize),
        selected: Option<(&'static str, usize, usize)>,
        preview: (&'static str, usize, usize, i32, i32),
    }

    let aggregate_rows = vec![
        collection_test_row("One"),
        collection_test_row("Two"),
        collection_test_row("Missing"),
        collection_test_row("Excluded"),
    ];
    let aggregate_album = collection_album(
        "spotify:album:aggregate",
        "Artist",
        &[
            ("One", "spotify:track:one"),
            ("Two", "spotify:track:two-a"),
            ("Two", "spotify:track:two-b"),
            ("Unused", "spotify:track:unused"),
        ],
    );
    let mut aggregate_session = collection_session(&aggregate_rows);
    aggregate_session.decisions.insert(
        aggregate_rows[3].stable_id.clone(),
        RowDecision {
            status: RowStatus::Pending,
            excluded: true,
        },
    );
    aggregate_session.collection_album_matches.insert(
        1,
        CollectionAlbumMatchState {
            cached_candidates: vec![aggregate_album.clone()],
            selected_album_uris: vec![aggregate_album.matching.uri.clone()],
            ..CollectionAlbumMatchState::default()
        },
    );

    let resolving_rows = vec![collection_test_row("One")];
    let resolving_album = collection_album(
        "spotify:album:resolving",
        "Artist",
        &[("One", "spotify:track:resolving")],
    );
    let mut resolving_session = collection_session(&resolving_rows);
    resolving_session.collection_album_matches.insert(
        1,
        CollectionAlbumMatchState {
            cached_candidates: vec![resolving_album.clone()],
            selected_album_uris: Vec::new(),
            ..CollectionAlbumMatchState::default()
        },
    );

    let ambiguous_rows = vec![collection_test_row("One")];
    let ambiguous_album = collection_album(
        "spotify:album:ambiguous",
        "Artist",
        &[
            ("One", "spotify:track:ambiguous-a"),
            ("One", "spotify:track:ambiguous-b"),
        ],
    );
    let mut ambiguous_session = collection_session(&ambiguous_rows);
    ambiguous_session.collection_album_matches.insert(
        1,
        CollectionAlbumMatchState {
            cached_candidates: vec![ambiguous_album.clone()],
            selected_album_uris: Vec::new(),
            ..CollectionAlbumMatchState::default()
        },
    );

    let cases = vec![
        (
            "aggregate and excluded rows",
            aggregate_session,
            aggregate_rows,
            Expected {
                aggregate: (1, 1, 1),
                selected: Some(("spotify:album:aggregate", 1, 1)),
                preview: ("spotify:album:aggregate", 1, 1, 0, 0),
            },
        ),
        (
            "unselected preview resolves a row",
            resolving_session,
            resolving_rows,
            Expected {
                aggregate: (0, 0, 1),
                selected: None,
                preview: ("spotify:album:resolving", 1, 1, 1, 0),
            },
        ),
        (
            "unselected preview creates ambiguity",
            ambiguous_session,
            ambiguous_rows,
            Expected {
                aggregate: (0, 0, 1),
                selected: None,
                preview: ("spotify:album:ambiguous", 0, 0, 0, 1),
            },
        ),
    ];

    for (name, session, rows, expected) in cases {
        let refs = rows.iter().collect::<Vec<_>>();
        let view = collection_match_view(&session, 1, &refs);
        assert_eq!(
            (
                view.coverage.matched,
                view.coverage.ambiguous,
                view.coverage.unresolved,
            ),
            expected.aggregate,
            "aggregate coverage for {name}"
        );
        match expected.selected {
            Some((uri, matched, unique)) => {
                let selected = view
                    .coverage
                    .selected_albums
                    .iter()
                    .find(|album| album.uri == uri)
                    .unwrap();
                assert_eq!(
                    (selected.matched, selected.unique_coverage),
                    (matched, unique),
                    "selected coverage for {name}"
                );
            }
            None => assert!(
                view.coverage.selected_albums.is_empty(),
                "selected coverage for {name}"
            ),
        }
        let preview = view
            .coverage
            .previews
            .iter()
            .find(|preview| preview.uri == expected.preview.0)
            .unwrap();
        assert_eq!(
            (
                preview.matched,
                preview.unique_coverage,
                preview.marginal_matches,
                preview.ambiguity_changes,
            ),
            (
                expected.preview.1,
                expected.preview.2,
                expected.preview.3,
                expected.preview.4,
            ),
            "preview coverage for {name}"
        );
    }
}

#[test]
fn collection_preview_marginal_matches_preserve_existing_fallback() {
    let rows = (0..100)
        .map(|index| collection_test_row(&format!("Sarah {index}")))
        .collect::<Vec<_>>();
    let mut session = collection_session(&rows);
    for row in &rows[..61] {
        session
            .matches
            .get_mut(&row.stable_id)
            .unwrap()
            .track_matches
            .insert(
                row.stable_id.clone(),
                format!("spotify:track:baseline-{}", row.track),
            );
    }
    for row in &rows[61..70] {
        let result = session.matches.get_mut(&row.stable_id).unwrap();
        result.candidates = vec![
            exact_collection_track(row, &format!("spotify:track:search-a-{}", row.track)),
            exact_collection_track(row, &format!("spotify:track:search-b-{}", row.track)),
        ];
    }
    let candidate = collection_album_for_rows("spotify:album:eden", &rows, &[61, 62, 63]);
    session.collection_album_matches.insert(
        1,
        CollectionAlbumMatchState {
            cached_candidates: vec![candidate],
            selected_album_uris: Vec::new(),
            ..CollectionAlbumMatchState::default()
        },
    );

    let refs = rows.iter().collect::<Vec<_>>();
    let view = collection_match_view(&session, 1, &refs);
    assert_eq!(
        (
            view.coverage.matched,
            view.coverage.ambiguous,
            view.coverage.unresolved,
        ),
        (61, 9, 30)
    );
    let preview = &view.coverage.previews[0];
    assert_eq!(
        (preview.marginal_matches, preview.ambiguity_changes),
        (3, -3)
    );
}

#[test]
fn collection_preview_marginal_matches_count_ambiguity_conversion() {
    let rows = (0..50)
        .map(|index| collection_test_row(&format!("Celtic {index}")))
        .collect::<Vec<_>>();
    let mut session = collection_session(&rows);
    for row in &rows[..3] {
        session
            .matches
            .get_mut(&row.stable_id)
            .unwrap()
            .track_matches
            .insert(
                row.stable_id.clone(),
                format!("spotify:track:baseline-{}", row.track),
            );
    }
    for row in &rows[3..32] {
        let result = session.matches.get_mut(&row.stable_id).unwrap();
        result.candidates = vec![
            exact_collection_track(row, &format!("spotify:track:search-a-{}", row.track)),
            exact_collection_track(row, &format!("spotify:track:search-b-{}", row.track)),
        ];
    }
    let mut candidate =
        collection_album_for_rows("spotify:album:celtic", &rows, &(3..16).collect::<Vec<_>>());
    for index in 0..5 {
        candidate
            .matching
            .track_uris
            .push(format!("spotify:track:celtic-unused-{index}"));
        candidate
            .matching
            .track_names
            .push(format!("Unused {index}"));
        candidate.matching.track_artists.push("Artist".into());
        candidate
            .matching
            .track_albums
            .push(candidate.matching.name.clone());
    }
    candidate.total_tracks = candidate.matching.track_uris.len() as u32;
    candidate.track_numbers = (1..=candidate.total_tracks).map(Some).collect();
    candidate.track_durations = vec![180; candidate.total_tracks as usize];
    session.collection_album_matches.insert(
        1,
        CollectionAlbumMatchState {
            cached_candidates: vec![candidate],
            selected_album_uris: Vec::new(),
            ..CollectionAlbumMatchState::default()
        },
    );

    let refs = rows.iter().collect::<Vec<_>>();
    let view = collection_match_view(&session, 1, &refs);
    assert_eq!(
        (
            view.coverage.matched,
            view.coverage.ambiguous,
            view.coverage.unresolved,
        ),
        (3, 29, 18)
    );
    let preview = &view.coverage.previews[0];
    assert_eq!(
        (preview.marginal_matches, preview.ambiguity_changes),
        (13, -13)
    );
}

#[test]
fn aggregation_keeps_compact_raw_variants_and_timestamps() {
    let mut rows = Vec::new();
    aggregate_scrobbles(
        &mut rows,
        &[
            scrobble("Beyoncé", "Lemonade", "Sorry", 300),
            scrobble("Beyoncé", "Lemonade", "Sorry!", 100),
            scrobble("Beyoncé", "Lemonade", "Sorry", 200),
        ],
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].play_count, 3);
    assert_eq!((rows[0].earliest, rows[0].latest), (100, 300));
    assert_eq!(rows[0].variants.len(), 2);
    assert_eq!(resolved_play_count(&[&rows[0]], CountMode::Sum), 3);
    assert_eq!(resolved_play_count(&[&rows[0]], CountMode::Overwrite), 2);
    assert_eq!(resolved_play_count(&[&rows[0]], CountMode::Zero), 0);
}

#[test]
fn setup_state_view_reports_review_only_remaining() {
    let setup = state_view(None);
    assert_eq!(setup.phase, None);
    assert_eq!(setup.username, None);
    assert_eq!(setup.spotify_account_id, None);
    assert_eq!(setup.remaining, 0);

    let mut session = LastFmImportSessionV2::new("rianjs".into(), "spotify-user".into(), 10);
    aggregate_scrobbles(
        &mut session.rows,
        &[scrobble("Artist", "Album", "Track", 10)],
    );
    assert_eq!(state_view(Some(&session)).remaining, 0);
    session.phase = ImportPhase::Review;
    assert_eq!(state_view(Some(&session)).remaining, 1);
}

#[test]
fn fuzzy_arithmetic_combines_rows_mapped_to_one_target() {
    let mut rows = Vec::new();
    aggregate_scrobbles(
        &mut rows,
        &[
            scrobble("Artist", "Album", "Song", 10),
            scrobble("Artist", "Album", "Song", 11),
            scrobble("Artist", "Album", "Song (Live)", 12),
        ],
    );
    assert_eq!(rows.len(), 2);
    let refs = rows.iter().collect::<Vec<_>>();
    assert_eq!(resolved_play_count(&refs, CountMode::Sum), 3);
    assert_eq!(resolved_play_count(&refs, CountMode::Overwrite), 2);
    assert_eq!(resolved_timestamps(&refs), Some((10, 12)));
}

#[tokio::test]
async fn page_fuzzy_groups_stay_inside_the_requested_batch() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 10);
    aggregate_scrobbles(
        &mut session.rows,
        &[
            scrobble("Artist", "Done", "Track", 1),
            scrobble("Artist", "Selected", "Track", 2),
            scrobble("Artist", "Skipped", "Track", 3),
            scrobble("Artist", "Ignored", "Track", 4),
            scrobble("Artist", "Excluded", "Track", 5),
            scrobble("Artist", "Unchecked", "Track", 6),
        ],
    );
    let ids = session
        .rows
        .iter()
        .map(|row| (row.album.clone(), row.stable_id.clone()))
        .collect::<BTreeMap<_, _>>();
    let target = "spotify:track:target".to_owned();
    for row in &session.rows {
        session.matches.insert(
            row.stable_id.clone(),
            MatchResult {
                source_id: row.stable_id.clone(),
                search_term: row.track.clone(),
                confidence: Some(Confidence::Exact),
                selected_uri: Some(target.clone()),
                candidates: Vec::new(),
                track_matches: BTreeMap::from([(row.stable_id.clone(), target.clone())]),
            },
        );
    }
    session.decisions.insert(
        ids["Done"].clone(),
        RowDecision {
            status: RowStatus::Done,
            excluded: false,
        },
    );
    session.decisions.insert(
        ids["Skipped"].clone(),
        RowDecision {
            status: RowStatus::Skipped,
            excluded: false,
        },
    );
    session.decisions.insert(
        ids["Ignored"].clone(),
        RowDecision {
            status: RowStatus::IgnoredAlbum,
            excluded: false,
        },
    );
    session.decisions.insert(
        ids["Excluded"].clone(),
        RowDecision {
            status: RowStatus::Pending,
            excluded: true,
        },
    );
    for (album, selected) in [
        ("Selected", true),
        ("Skipped", true),
        ("Ignored", true),
        ("Excluded", true),
        ("Unchecked", false),
    ] {
        session.page_options.insert(
            format!("Artist\u{1f}{album}"),
            PageOptions {
                selected_track_ids: if selected {
                    BTreeSet::from([ids[album].clone()])
                } else {
                    BTreeSet::new()
                },
                ..PageOptions::default()
            },
        );
    }
    service.save(session).await.unwrap();

    let session = service.snapshot().await.unwrap();
    let selected_page = review_batches(&session)
        .into_iter()
        .find(|batch| {
            batch_rows(batch, &source_row_map(&session))
                .iter()
                .any(|row| row.album == "Selected")
        })
        .unwrap()
        .page;
    let page = service
        .page(selected_page, "Artist", "Selected")
        .await
        .unwrap();
    assert_eq!(
        page.rows
            .iter()
            .map(|item| item.source.album.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["Selected"])
    );
    assert!(!page.fuzzy_groups.contains_key(&target));
    assert!(!page.locked_count_modes.contains(&target));
}

#[tokio::test]
async fn page_projects_count_modes_to_visible_fuzzy_targets() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 10);
    aggregate_scrobbles(
        &mut session.rows,
        &[
            scrobble("Artist", "Hidden", "Track", 1),
            scrobble("Artist", "Visible", "Track", 2),
            scrobble("Artist", "Visible", "Track (Live)", 3),
        ],
    );
    let visible_ids = session
        .rows
        .iter()
        .filter(|row| row.album == "Visible")
        .map(|row| row.stable_id.clone())
        .collect::<Vec<_>>();
    let hidden_id = session
        .rows
        .iter()
        .find(|row| row.album == "Hidden")
        .unwrap()
        .stable_id
        .clone();
    let visible_target = "spotify:track:visible".to_owned();
    for (source_id, target) in visible_ids
        .iter()
        .map(|id| (id, &visible_target))
        .chain(std::iter::once((&hidden_id, &visible_target)))
    {
        session.matches.insert(
            source_id.clone(),
            MatchResult {
                source_id: source_id.clone(),
                search_term: String::new(),
                confidence: Some(Confidence::Exact),
                selected_uri: Some(target.clone()),
                candidates: Vec::new(),
                track_matches: BTreeMap::from([(source_id.clone(), target.clone())]),
            },
        );
    }
    session.decisions.insert(
        visible_ids[0].clone(),
        RowDecision {
            status: RowStatus::Done,
            excluded: false,
        },
    );
    session.decisions.insert(
        hidden_id.clone(),
        RowDecision {
            status: RowStatus::Done,
            excluded: false,
        },
    );
    session
        .count_modes
        .insert(visible_target.clone(), CountMode::Overwrite);
    session.page_options.insert(
        "Artist\u{1f}Visible".into(),
        PageOptions {
            selected_track_ids: visible_ids.iter().cloned().collect(),
            ..PageOptions::default()
        },
    );
    session.page_options.insert(
        "Artist\u{1f}Hidden".into(),
        PageOptions {
            selected_track_ids: BTreeSet::from([hidden_id]),
            ..PageOptions::default()
        },
    );
    session.phase = ImportPhase::Review;
    service.save(session).await.unwrap();

    let session = service.snapshot().await.unwrap();
    let visible_page = review_batches(&session)
        .into_iter()
        .find(|batch| {
            batch_rows(batch, &source_row_map(&session))
                .iter()
                .any(|row| row.album == "Visible")
        })
        .unwrap();
    for (mode, expected) in [
        (CountMode::Sum, 3),
        (CountMode::Overwrite, 1),
        (CountMode::Zero, 0),
    ] {
        let mut configured = session.clone();
        configured.count_modes.insert(visible_target.clone(), mode);
        service.save(configured).await.unwrap();
        let saved = service.snapshot().await.unwrap();
        let page = service
            .page(visible_page.page, "Artist", "Visible")
            .await
            .unwrap();
        assert_eq!(
            page.fuzzy_groups.keys().cloned().collect::<BTreeSet<_>>(),
            BTreeSet::from([visible_target.clone()])
        );
        assert_eq!(
            page.fuzzy_groups[&visible_target]
                .iter()
                .map(|row| row.stable_id.clone())
                .collect::<BTreeSet<_>>(),
            visible_ids.iter().cloned().collect::<BTreeSet<_>>()
        );
        assert_eq!(page.resolved_counts.get(&visible_target), Some(&expected));
        assert_eq!(
            page.count_modes,
            BTreeMap::from([(visible_target.clone(), mode)])
        );
        assert_eq!(
            page.locked_count_modes,
            BTreeSet::from([visible_target.clone()])
        );

        let options = saved.options_for_batch(visible_page.page, "Artist", "Visible");
        let plan = build_apply_plan(
            &saved,
            "spotify",
            visible_page.page,
            "Artist",
            "Visible",
            &visible_ids,
            false,
            options,
        )
        .unwrap();
        assert_eq!(
            plan.updates
                .iter()
                .find(|update| update.uri == visible_target)
                .and_then(|update| update.play_count),
            Some(expected)
        );
    }
}

#[tokio::test]
async fn count_mode_change_is_rejected_after_target_is_done() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 10);
    aggregate_scrobbles(
        &mut session.rows,
        &[scrobble("Artist", "Album", "Track", 1)],
    );
    let source_id = session.rows[0].stable_id.clone();
    let target = "spotify:track:target";
    session.matches.insert(
        source_id.clone(),
        MatchResult {
            source_id: source_id.clone(),
            search_term: "Track".into(),
            confidence: Some(Confidence::Exact),
            selected_uri: Some(target.into()),
            candidates: Vec::new(),
            track_matches: BTreeMap::from([(source_id.clone(), target.into())]),
        },
    );
    session.decisions.insert(
        source_id.clone(),
        RowDecision {
            status: RowStatus::Done,
            excluded: false,
        },
    );
    session
        .count_modes
        .insert(target.into(), CountMode::Overwrite);
    session.page_options.insert(
        "accepted".into(),
        PageOptions {
            selected_track_ids: BTreeSet::from([source_id]),
            ..PageOptions::default()
        },
    );
    session.phase = ImportPhase::Review;
    service.save(session).await.unwrap();

    assert!(service
        .set_count_mode("user", "spotify", target, CountMode::Overwrite)
        .await
        .is_ok());
    assert!(service
        .set_count_mode("user", "spotify", target, CountMode::Zero)
        .await
        .is_err());
    assert_eq!(
        service.snapshot().await.unwrap().count_modes.get(target),
        Some(&CountMode::Overwrite)
    );
}

#[tokio::test]
async fn count_mode_change_updates_every_unlocked_target_and_survives_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 10);
    session.count_modes = BTreeMap::from([
        ("spotify:track:first".into(), CountMode::Overwrite),
        ("spotify:track:second".into(), CountMode::Sum),
    ]);
    session.phase = ImportPhase::Review;
    service.save(session).await.unwrap();

    service
        .set_count_mode("user", "spotify", "spotify:track:first", CountMode::Zero)
        .await
        .unwrap();
    let session = service.snapshot().await.unwrap();
    assert_eq!(session.default_count_mode, CountMode::Zero);
    assert!(session.count_modes.is_empty());

    service
        .save_mappings_for(
            "user",
            Some("spotify"),
            LastFmMappings {
                default_count_mode: CountMode::Zero,
                ..LastFmMappings::default()
            },
        )
        .await
        .unwrap();
    drop(service);
    assert_eq!(
        Service::new(dir.path())
            .mappings_for("user", Some("spotify"))
            .await
            .unwrap()
            .default_count_mode,
        CountMode::Zero
    );
}

#[tokio::test]
async fn interrupted_count_mode_transaction_rolls_forward_both_files_on_reload() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 10);
    session.phase = ImportPhase::Review;
    service.save(session).await.unwrap();
    service
        .save_mappings_for("user", Some("spotify"), LastFmMappings::default())
        .await
        .unwrap();
    let hook = crate::store::SaveHook::new(true);
    service.mappings_store.arm_save(Arc::clone(&hook));
    let transaction = {
        let service = Arc::clone(&service);
        tokio::spawn(async move {
            service
                .set_count_mode("user", "spotify", "spotify:track:target", CountMode::Zero)
                .await
        })
    };
    while !hook.is_reached() {
        tokio::task::yield_now().await;
    }
    hook.wait_until_reached();
    hook.release();
    assert!(transaction.await.unwrap().is_err());
    assert_eq!(
        service.snapshot().await.unwrap().default_count_mode,
        CountMode::Sum
    );
    assert_eq!(
        service.export_mappings().await.mappings.default_count_mode,
        CountMode::Sum
    );

    let reloaded = Service::new(dir.path());
    assert_eq!(
        reloaded.snapshot().await.unwrap().default_count_mode,
        CountMode::Zero
    );
    assert_eq!(
        reloaded
            .mappings_for("user", Some("spotify"))
            .await
            .unwrap()
            .default_count_mode,
        CountMode::Zero
    );
}

#[tokio::test(flavor = "current_thread")]
async fn aborted_journal_recovery_publishes_before_the_next_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 10);
    session.phase = ImportPhase::Review;
    session.search_terms = false;
    service.save(session).await.unwrap();
    service
        .save_mappings_for("user", Some("spotify"), LastFmMappings::default())
        .await
        .unwrap();
    let failed_save = crate::store::SaveHook::new(true);
    service.mappings_store.arm_save(Arc::clone(&failed_save));
    let transaction = {
        let service = Arc::clone(&service);
        tokio::spawn(async move {
            service
                .set_count_mode("user", "spotify", "spotify:track:target", CountMode::Zero)
                .await
        })
    };
    while !failed_save.is_reached() {
        tokio::task::yield_now().await;
    }
    failed_save.wait_until_reached();
    failed_save.release();
    assert!(transaction.await.unwrap().is_err());

    let recovery_hook = crate::store::SaveHook::new(false);
    service
        .review_transaction_store
        .arm_recovery(Arc::clone(&recovery_hook));
    let cancelled = {
        let service = Arc::clone(&service);
        tokio::spawn(async move { service.set_search_terms("user", "spotify", true).await })
    };
    while !recovery_hook.is_reached() {
        tokio::task::yield_now().await;
    }
    recovery_hook.wait_until_reached();
    cancelled.abort();
    recovery_hook.release();
    assert!(cancelled.await.unwrap_err().is_cancelled());

    service
        .set_search_terms("user", "spotify", true)
        .await
        .unwrap();
    assert_eq!(
        service.snapshot().await.unwrap().default_count_mode,
        CountMode::Zero
    );
    assert_eq!(
        service.export_mappings().await.mappings.default_count_mode,
        CountMode::Zero
    );
    let reloaded = Service::new(dir.path());
    let session = reloaded.snapshot().await.unwrap();
    assert!(session.search_terms);
    assert_eq!(session.default_count_mode, CountMode::Zero);
}

#[tokio::test(flavor = "current_thread")]
async fn delayed_count_mode_transaction_preserves_a_second_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 10);
    session.phase = ImportPhase::Review;
    service.save(session).await.unwrap();
    service
        .save_mappings_for("user", Some("spotify"), LastFmMappings::default())
        .await
        .unwrap();
    let hook = crate::store::SaveHook::new(false);
    service.mappings_store.arm_save(Arc::clone(&hook));
    let first = {
        let service = Arc::clone(&service);
        tokio::spawn(async move {
            service
                .set_count_mode("user", "spotify", "spotify:track:first", CountMode::Zero)
                .await
        })
    };
    while !hook.is_reached() {
        tokio::task::yield_now().await;
    }
    hook.wait_until_reached();
    let second = {
        let service = Arc::clone(&service);
        tokio::spawn(async move {
            service
                .set_count_mode(
                    "user",
                    "spotify",
                    "spotify:track:second",
                    CountMode::Overwrite,
                )
                .await
        })
    };
    tokio::task::yield_now().await;
    assert!(!second.is_finished());

    hook.release();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    let reloaded = Service::new(dir.path());
    assert_eq!(
        reloaded.snapshot().await.unwrap().default_count_mode,
        CountMode::Overwrite
    );
    assert_eq!(
        reloaded
            .mappings_for("user", Some("spotify"))
            .await
            .unwrap()
            .default_count_mode,
        CountMode::Overwrite
    );
}

#[tokio::test]
async fn first_spotify_match_restores_the_reusable_count_mode() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let mut session =
        LastFmImportSessionV2::new_with_defaults("user".into(), 10, ImportDefaults::default());
    aggregate_scrobbles(
        &mut session.rows,
        &[scrobble("Artist", "Album", "Track", 1)],
    );
    session.batches = build_review_batches(&session.rows);
    session.phase = ImportPhase::Review;
    let source_id = session.rows[0].stable_id.clone();
    service.save(session).await.unwrap();

    service
        .set_matches(
            "user",
            "spotify",
            1,
            vec![MatchResult {
                source_id: source_id.clone(),
                search_term: String::new(),
                confidence: Some(Confidence::Exact),
                selected_uri: Some("spotify:track:target".into()),
                candidates: Vec::new(),
                track_matches: BTreeMap::from([(source_id, "spotify:track:target".into())]),
            }],
            Some(CountMode::Overwrite),
        )
        .await
        .unwrap();
    assert_eq!(
        service.snapshot().await.unwrap().default_count_mode,
        CountMode::Overwrite
    );
}

#[test]
fn selected_count_mode_is_session_scoped_across_pages_and_persisted() {
    let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 10);
    aggregate_scrobbles(
        &mut session.rows,
        &[
            scrobble("Artist", "First", "Song", 10),
            scrobble("Artist", "First", "Song", 11),
            scrobble("Artist", "Second", "Song!", 12),
            scrobble("Artist", "Second", "Song!", 13),
        ],
    );
    let first = session.rows[0].stable_id.clone();
    let second = session.rows[1].stable_id.clone();
    for id in [&first, &second] {
        session.matches.insert(
            (*id).clone(),
            MatchResult {
                source_id: (*id).clone(),
                search_term: String::new(),
                confidence: Some(Confidence::Exact),
                selected_uri: Some("spotify:track:target".into()),
                candidates: Vec::new(),
                track_matches: BTreeMap::from([((*id).clone(), "spotify:track:target".into())]),
            },
        );
    }
    let target = "spotify:track:target";
    session.decisions.insert(
        first.clone(),
        RowDecision {
            status: RowStatus::Done,
            excluded: false,
        },
    );
    session.batches = build_review_batches(&session.rows);
    let first_batch = session
        .batches
        .iter()
        .find(|batch| batch.source_ids.contains(&first))
        .unwrap()
        .page;
    session.page_options.insert(
        batch_options_key(first_batch),
        PageOptions {
            selected_track_ids: BTreeSet::from([first.clone()]),
            ..PageOptions::default()
        },
    );
    let current = vec![&session.rows[1]];
    session.count_modes.insert(target.into(), CountMode::Sum);
    assert_eq!(historical_count_for_target(&session, target, &current), 4);
    session
        .count_modes
        .insert(target.into(), CountMode::Overwrite);
    assert_eq!(historical_count_for_target(&session, target, &current), 2);
    session.count_modes.insert(target.into(), CountMode::Zero);
    assert_eq!(historical_count_for_target(&session, target, &current), 0);

    let dir = tempfile::tempdir().unwrap();
    let store = ImportSessionStore::new(dir.path());
    store.save(&session).unwrap();
    assert_eq!(
        store.load().unwrap().unwrap().count_modes[target],
        CountMode::Zero
    );
}

#[test]
fn spotify_share_links_and_selected_album_tracks_remain_reviewable() {
    for value in [
        "spotify:album:Album123",
        "spotify://album/Album123",
        "https://open.spotify.com/album/Album123?si=share",
        "https://open.spotify.com/intl-de/album/Album123#details",
    ] {
        assert_eq!(
            spotify_share_uri(value, "album").unwrap().as_deref(),
            Some("spotify:album:Album123")
        );
    }
    assert_eq!(
        spotify_share_uri("spotify:track:Track123", "track")
            .unwrap()
            .as_deref(),
        Some("spotify:track:Track123")
    );
    assert_eq!(
        spotify_share_uri("Sibelius symphonies", "album").unwrap(),
        None
    );
    assert!(spotify_share_uri("spotify:track:Track123", "album").is_err());
    assert!(spotify_share_uri("https://open.spotify.com/album/not-valid!", "album").is_err());

    let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 10);
    aggregate_scrobbles(
        &mut session.rows,
        &[scrobble("Jean Sibelius", "Symphonies", "Movement I", 1)],
    );
    session.phase = ImportPhase::Review;
    session.batches = build_review_batches(&session.rows);
    let row = session.rows[0].clone();
    session.matches.insert(
        row.stable_id.clone(),
        MatchResult {
            source_id: row.stable_id.clone(),
            search_term: "https://open.spotify.com/album/Album123?si=share".into(),
            confidence: None,
            selected_uri: None,
            candidates: vec![AlbumCandidate {
                uri: "spotify:album:Album123".into(),
                name: "Exact recording".into(),
                artist: "Orchestra".into(),
                in_library: false,
                track_uris: vec!["spotify:track:Different".into()],
                track_names: vec!["Different movement".into()],
                track_artists: vec!["Orchestra".into()],
                track_albums: vec!["Exact recording".into()],
                relation: None,
            }],
            track_matches: BTreeMap::new(),
        },
    );

    select_match_in_session(&mut session, 1, &row.stable_id, "spotify:album:Album123").unwrap();
    let result = &session.matches[&row.stable_id];
    assert_eq!(
        result.selected_uri.as_deref(),
        Some("spotify:album:Album123")
    );
    assert!(result.track_matches.is_empty());

    select_match_in_session(&mut session, 1, &row.stable_id, "spotify:track:Different").unwrap();
    let result = &session.matches[&row.stable_id];
    assert_eq!(
        result.selected_uri.as_deref(),
        Some("spotify:album:Album123")
    );
    assert_eq!(
        result.track_matches.get(&row.stable_id).map(String::as_str),
        Some("spotify:track:Different")
    );
}

#[test]
fn album_title_coverage_selects_freedom_and_tron_style_matches() {
    let candidate = |uri: &str, name: &str, artist: &str, names: Vec<String>| AlbumCandidate {
        uri: uri.into(),
        name: name.into(),
        artist: artist.into(),
        in_library: false,
        track_uris: (0..names.len())
            .map(|index| format!("spotify:track:{uri}:{index}"))
            .collect(),
        track_names: names,
        track_artists: Vec::new(),
        track_albums: Vec::new(),
        relation: None,
    };

    let freedom = vec!["Offering".to_owned(), "Call".to_owned()];
    let mut freedom_candidates = vec![candidate(
        "freedom",
        "Freedom",
        "Michael W. Smith",
        vec!["The Offering".into(), "The Call".into()],
    )];
    classify_album_candidates_by_name(&freedom, &mut freedom_candidates);
    assert_eq!(
        freedom_candidates[0].relation,
        Some(AlbumRelation::BestMatch)
    );
    assert_eq!(
        automatic_album_candidate("Freedom", &freedom, &freedom_candidates)
            .map(|candidate| candidate.uri.as_str()),
        Some("freedom")
    );

    let babel = (1..=15)
        .map(|index| format!("Babel Track {index}"))
        .collect::<Vec<_>>();
    let mut babel_candidates = vec![
        candidate("standard", "Babel", "Mumford & Sons", babel[..12].to_vec()),
        candidate(
            "deluxe",
            "Babel (Deluxe Version)",
            "Mumford & Sons",
            babel.clone(),
        ),
    ];
    classify_album_candidates_by_name(&babel, &mut babel_candidates);
    assert_eq!(
        automatic_album_candidate("Babel (Deluxe Edition)", &babel, &babel_candidates)
            .map(|candidate| candidate.uri.as_str()),
        Some("deluxe")
    );
    babel_candidates.push(candidate(
        "other-deluxe",
        "Babel (Deluxe Edition)",
        "Mumford & Sons",
        babel.clone(),
    ));
    classify_album_candidates_by_name(&babel, &mut babel_candidates);
    assert_eq!(
        automatic_album_candidate("Babel (Deluxe Edition)", &babel, &babel_candidates),
        None
    );
    assert_eq!(
        album_track_match_index(
            "TRON Legacy (End Titles)",
            &[
                "Overture - From TRON Legacy Score".into(),
                "TRON Legacy (End Titles) - From TRON Legacy Score".into(),
            ],
        ),
        Some(1)
    );

    let mut tron = (1..=22)
        .map(|index| format!("Track {index}"))
        .collect::<Vec<_>>();
    tron.extend([
        "Track 1 - From TRON Legacy Score".into(),
        "Track 2 - From TRON Legacy Score".into(),
        "Sea of Simulation".into(),
    ]);
    let tron_album = (1..=22)
        .map(|index| format!("Track {index} - From TRON Legacy Score"))
        .collect::<Vec<_>>();
    let mut complete = tron_album.clone();
    complete.extend((1..=9).map(|index| format!("Bonus {index}")));
    let mut tron_candidates = vec![
        candidate("tron", "TRON: Legacy", "Daft Punk", tron_album),
        candidate(
            "complete",
            "TRON: Legacy - The Complete Edition",
            "Daft Punk",
            complete,
        ),
    ];
    classify_album_candidates_by_name(&tron, &mut tron_candidates);
    assert_eq!(
        automatic_album_candidate("TRON: Legacy", &tron, &tron_candidates)
            .map(|candidate| candidate.uri.as_str()),
        Some("tron")
    );

    let narnia = (1..=13)
        .map(|index| format!("Narnia Track {index}"))
        .collect::<Vec<_>>();
    let mut narnia_album = narnia.clone();
    narnia_album.extend((1..=4).map(|index| format!("Extra Track {index}")));
    let mut narnia_candidates = vec![candidate(
        "narnia",
        "The Chronicles of Narnia: The Lion, the Witch and the Wardrobe (Original Score)",
        "Harry Gregson-Williams",
        narnia_album,
    )];
    classify_album_candidates_by_name(&narnia, &mut narnia_candidates);
    assert_eq!(narnia_candidates[0].relation, Some(AlbumRelation::Superset));
    assert_eq!(
        automatic_album_candidate(
            "The Chronicles of Narnia: The Lion, the Witch and the Wardrobe",
            &narnia,
            &narnia_candidates,
        )
        .map(|candidate| candidate.uri.as_str()),
        Some("narnia")
    );
}

#[test]
fn album_title_and_track_coverage_outweigh_compilation_artist_credit() {
    let source = vec![
        "Dear Clarice (featuring Sir Anthony Hopkins)".into(),
        "Gourmet Vaise Tartare".into(),
        "Avarice".into(),
        "For a Small Stipend".into(),
        "Firenze Di Notte".into(),
        "To Every Captive Soul".into(),
        "Vide Cor Meum".into(),
        "Let My Home Be My Gallows (featuring Sir Anthony Hopkins)".into(),
        "The Burning Heart (featuring Sir Anthony Hopkins)".into(),
        "Aria da Capo (From Goldberg Variations, BWV 988)".into(),
        "The Capponi Library".into(),
        "Virtue".into(),
        "Dear Clarice (feat. Sir Anthony Hopkins)".into(),
        "Let My Home Be My Gallows (feat. Sir Anthony Hopkins)".into(),
        "The Burning Heart (feat. Sir Anthony Hopkins)".into(),
        "Aria da Capo (From the Goldberg Variations, BWV 988)".into(),
    ];
    let tracks = vec![
        "Dear Clarice".into(),
        "Goldberg Variations Bwv 988: Aria - Da Capo".into(),
        "The Capponi Library".into(),
        "Gourmet Valse Tartare".into(),
        "Avarice".into(),
        "For A Small Stipend".into(),
        "Firenze Di Notte".into(),
        "Virtue".into(),
        "Let My Home Be My Gallows".into(),
        "The Burning Heart".into(),
        "To Every Captive Soul".into(),
        "Vide Cor Meum".into(),
    ];
    let mut candidates = vec![AlbumCandidate {
        uri: "hannibal".into(),
        name: "Hannibal - Original Motion Picture Soundtrack".into(),
        artist: "Various Artists".into(),
        in_library: false,
        track_uris: (0..tracks.len())
            .map(|index| format!("spotify:track:hannibal:{index}"))
            .collect(),
        track_names: tracks,
        track_artists: Vec::new(),
        track_albums: Vec::new(),
        relation: None,
    }];

    classify_album_candidates_by_name(&source, &mut candidates);

    assert_eq!(
        automatic_album_candidate("Hannibal", &source, &candidates)
            .map(|candidate| candidate.uri.as_str()),
        Some("hannibal")
    );
    assert_eq!(
        automatic_album_candidate("Unrelated Album", &source, &candidates),
        None
    );
}

#[test]
fn cached_supported_album_is_selected_without_another_spotify_search() {
    let rows = ["Offering", "Call"]
        .into_iter()
        .map(|track| SourceRow {
            stable_id: source_id("Michael W. Smith", "Freedom", track),
            artist: "Michael W. Smith".into(),
            album: "Freedom".into(),
            track: track.into(),
            variants: Vec::new(),
            play_count: 1,
            earliest: 1,
            latest: 1,
        })
        .collect::<Vec<_>>();
    let candidate = AlbumCandidate {
        uri: "spotify:album:freedom".into(),
        name: "Freedom".into(),
        artist: "Michael W. Smith".into(),
        in_library: false,
        track_uris: vec!["spotify:track:offering".into(), "spotify:track:call".into()],
        track_names: vec!["The Offering".into(), "The Call".into()],
        track_artists: Vec::new(),
        track_albums: Vec::new(),
        relation: Some(AlbumRelation::SameSongs),
    };
    let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 1);
    session.rows = rows.clone();
    session.batches = build_review_batches(&session.rows);
    session.phase = ImportPhase::Review;
    for row in &rows {
        session.matches.insert(
            row.stable_id.clone(),
            MatchResult {
                source_id: row.stable_id.clone(),
                search_term: album_search_term(&row.artist, &row.album),
                confidence: None,
                selected_uri: None,
                candidates: vec![candidate.clone()],
                track_matches: BTreeMap::new(),
            },
        );
    }

    assert!(refresh_cached_album_matches(&mut session));
    for row in &rows {
        let result = &session.matches[&row.stable_id];
        assert_eq!(
            result.selected_uri.as_deref(),
            Some("spotify:album:freedom")
        );
        assert!(result.track_matches.contains_key(&row.stable_id));
    }
    session
        .matches
        .get_mut(&rows[0].stable_id)
        .unwrap()
        .track_matches
        .remove(&rows[0].stable_id);
    assert!(refresh_cached_album_matches(&mut session));
    assert!(session.matches[&rows[0].stable_id]
        .track_matches
        .contains_key(&rows[0].stable_id));
}

#[test]
fn legacy_album_candidate_json_defaults_to_not_in_library() {
    let candidate: AlbumCandidate = serde_json::from_value(serde_json::json!({
        "uri": "spotify:track:legacy",
        "name": "Legacy",
        "artist": "Artist",
        "trackUris": ["spotify:track:legacy"],
        "relation": "best-match"
    }))
    .unwrap();
    assert!(!candidate.in_library);
}

fn collection_row(artist: &str, track: &str) -> SourceRow {
    SourceRow {
        stable_id: source_id(artist, "", track),
        artist: artist.into(),
        album: String::new(),
        track: track.into(),
        variants: Vec::new(),
        play_count: 1,
        earliest: 1,
        latest: 1,
    }
}

fn collection_candidate(uri: &str, name: &str, artist: &str, in_library: bool) -> AlbumCandidate {
    AlbumCandidate {
        uri: uri.into(),
        name: name.into(),
        artist: artist.into(),
        in_library,
        track_uris: vec![uri.into()],
        track_names: vec![name.into()],
        track_artists: vec![artist.into()],
        track_albums: vec!["Release".into()],
        relation: None,
    }
}

fn collection_result(row: &SourceRow, candidates: Vec<AlbumCandidate>) -> MatchResult {
    MatchResult {
        source_id: row.stable_id.clone(),
        search_term: track_search_term(&row.artist, &row.track),
        confidence: None,
        selected_uri: None,
        candidates,
        track_matches: BTreeMap::new(),
    }
}

#[test]
fn collection_ratification_uses_local_titles_then_membership_and_artist_ranking() {
    let row = collection_row("Artist", "Song");
    let owned = collection_candidate("spotify:track:owned", "Song", "Artist", true);
    let unowned = collection_candidate("spotify:track:unowned", "Song", "Artist", false);
    let result = ratify_collection_result(
        &row,
        collection_result(&row, vec![unowned, owned]),
        &CollectionMembership {
            track_uris: BTreeSet::from(["spotify:track:owned".into()]),
        },
        &LastFmMappings::default(),
    );
    assert_eq!(
        result.track_matches.get(&row.stable_id),
        Some(&String::from("spotify:track:owned"))
    );

    let result = ratify_collection_result(
        &row,
        collection_result(
            &row,
            vec![collection_candidate(
                "spotify:track:only",
                "Song",
                "Artist",
                false,
            )],
        ),
        &CollectionMembership::default(),
        &LastFmMappings::default(),
    );
    assert_eq!(
        result.track_matches.get(&row.stable_id),
        Some(&String::from("spotify:track:only"))
    );

    let result = ratify_collection_result(
        &row,
        collection_result(
            &row,
            vec![
                collection_candidate("spotify:track:owned", "Song", "Artist", true),
                collection_candidate("spotify:track:other", "Song", "Artist", true),
            ],
        ),
        &CollectionMembership {
            track_uris: BTreeSet::from([
                "spotify:track:owned".into(),
                "spotify:track:other".into(),
            ]),
        },
        &LastFmMappings::default(),
    );
    assert!(result.track_matches.is_empty());

    let result = ratify_collection_result(
        &row,
        collection_result(
            &row,
            vec![collection_candidate(
                "spotify:track:wrong",
                "Song",
                "Wrong Artist",
                true,
            )],
        ),
        &CollectionMembership {
            track_uris: BTreeSet::from(["spotify:track:wrong".into()]),
        },
        &LastFmMappings::default(),
    );
    assert_eq!(
        result.track_matches.get(&row.stable_id).map(String::as_str),
        Some("spotify:track:wrong")
    );
    assert_eq!(
        result.candidates[0].relation,
        Some(AlbumRelation::BestMatch)
    );

    let result = ratify_collection_result(
        &row,
        collection_result(
            &row,
            vec![collection_candidate(
                "spotify:track:near",
                "Song (Live)",
                "Artist",
                true,
            )],
        ),
        &CollectionMembership {
            track_uris: BTreeSet::from(["spotify:track:near".into()]),
        },
        &LastFmMappings::default(),
    );
    assert_eq!(
        result.track_matches.get(&row.stable_id).map(String::as_str),
        Some("spotify:track:near")
    );
    assert_eq!(result.confidence, Some(Confidence::Likely));
    assert!(result.candidates[0].in_library);
    assert_eq!(
        result.candidates[0].relation,
        Some(AlbumRelation::SameSongs)
    );
}

#[test]
fn collection_candidate_relations_require_complete_title_words() {
    assert_eq!(
        track_search_term(
            "Celtic Woman",
            "Last Rose of Summer (intro)/Walking in the Air"
        ),
        "track:\"Last Rose of Summer Walking in the Air\" artist:\"Celtic Woman\""
    );
    for (source_title, candidate_title, candidate_artist, relation) in [
        ("Song", "Song", "Artist", Some(AlbumRelation::BestMatch)),
        (
            "Dulaman",
            "Dúlaman",
            "Artist",
            Some(AlbumRelation::BestMatch),
        ),
        (
            "Song",
            "Song (Live)",
            "Artist",
            Some(AlbumRelation::SameSongs),
        ),
        (
            "Last Rose of Summer (intro)/Walking in the Air",
            "Last Rose of Summer/Walking in the Air – Medley",
            "Artist",
            Some(AlbumRelation::SameSongs),
        ),
        (
            "Siúil A Rún (Walk My Love)",
            "Siulil A Run",
            "Artist",
            Some(AlbumRelation::SameSongs),
        ),
        ("Sailing A Run", "Siulil A Run", "Artist", None),
        (
            "Song of Love",
            "Love Song",
            "Artist",
            Some(AlbumRelation::SameSongs),
        ),
        (
            "El Cid: Fanfare And Entry of The Nobles",
            "Fanfare & Entry Of The Nobles (From \"El Cid\")",
            "Cincinnati Pops Orchestra",
            Some(AlbumRelation::SameSongs),
        ),
        ("La Luna", "Anytime, Anywhere", "Artist", None),
        (
            "Song",
            "Song (Live)",
            "Other Artist",
            Some(AlbumRelation::SameSongs),
        ),
    ] {
        let row = collection_row("Artist", source_title);
        let mut candidates = vec![collection_candidate(
            "spotify:track:candidate",
            candidate_title,
            candidate_artist,
            true,
        )];
        rank_collection_candidates(
            &row,
            &mut candidates,
            &CollectionMembership {
                track_uris: BTreeSet::from(["spotify:track:candidate".into()]),
            },
        );
        assert_eq!(candidates[0].relation, relation);
    }
}

#[test]
fn collection_candidate_relations_accept_source_variant_titles() {
    let mut row = collection_row("Artist", "La Luna");
    row.variants.push(SourceVariant {
        artist: "Artist".into(),
        album: String::new(),
        track: "Song".into(),
        play_count: 1,
        earliest: 1,
        latest: 1,
    });
    let mut candidates = vec![collection_candidate(
        "spotify:track:variant",
        "Song (Live)",
        "Artist",
        true,
    )];
    rank_collection_candidates(
        &row,
        &mut candidates,
        &CollectionMembership {
            track_uris: BTreeSet::from(["spotify:track:variant".into()]),
        },
    );
    assert_eq!(candidates[0].relation, Some(AlbumRelation::SameSongs));
}

#[test]
fn collection_candidates_break_relation_ties_by_title_token_overlap() {
    let row = collection_row("Artist", "Last Rose of Summer Walking in the Air");
    let mut candidates = vec![
        collection_candidate("spotify:track:short", "Walking in the Air", "Artist", false),
        collection_candidate(
            "spotify:track:full",
            "Last Rose of Summer Walking in the Air Medley",
            "Artist",
            false,
        ),
    ];
    rank_collection_candidates(&row, &mut candidates, &CollectionMembership::default());
    assert_eq!(candidates[0].uri, "spotify:track:full");
}

#[test]
fn collection_ratification_reuses_variants_mappings_and_manual_choices() {
    let mut row = collection_row("Artist", "Song");
    row.variants.push(SourceVariant {
        artist: "The Artist".into(),
        album: String::new(),
        track: "Song!".into(),
        play_count: 1,
        earliest: 1,
        latest: 1,
    });
    let variant_candidate =
        collection_candidate("spotify:track:variant", "Song!", "The Artist", false);
    let mut mappings = LastFmMappings::default();
    mappings.track_mappings.insert(
        source_id(&row.artist, &row.album, &row.track),
        "spotify:track:mapped".into(),
    );
    let result = ratify_collection_result(
        &row,
        collection_result(&row, vec![variant_candidate]),
        &CollectionMembership::default(),
        &mappings,
    );
    assert_eq!(
        result.track_matches.get(&row.stable_id).map(String::as_str),
        Some("spotify:track:mapped")
    );
    assert!(result
        .candidates
        .iter()
        .any(|candidate| candidate.uri == "spotify:track:mapped"));

    let mut manual = collection_result(
        &row,
        vec![collection_candidate(
            "spotify:track:manual",
            "Different Song",
            "Wrong Artist",
            false,
        )],
    );
    manual.selected_uri = Some("spotify:track:manual".into());
    manual
        .track_matches
        .insert(row.stable_id.clone(), "spotify:track:manual".into());
    let manual = ratify_collection_result(
        &row,
        manual,
        &CollectionMembership::default(),
        &LastFmMappings::default(),
    );
    assert_eq!(manual.selected_uri.as_deref(), Some("spotify:track:manual"));
    assert_eq!(manual.track_matches[&row.stable_id], "spotify:track:manual");
}

#[tokio::test]
async fn cached_collection_rerank_only_trusts_complete_spotify_membership() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let mut session =
        LastFmImportSessionV2::new_with_defaults("user".into(), 100, ImportDefaults::default());
    aggregate_scrobbles(&mut session.rows, &[scrobble("Artist", "", "Song", 1)]);
    session.phase = ImportPhase::Review;
    session.batches = build_review_batches(&session.rows);
    let row = session.rows[0].clone();
    let uri = "spotify:track:saved";
    session.matches.insert(
        row.stable_id.clone(),
        collection_result(
            &row,
            vec![collection_candidate(uri, "Song", "Artist", false)],
        ),
    );
    service.save(session).await.unwrap();

    let lastfm = crate::lastfm::Service::new_for_test(dir.path(), true, false);
    let state = crate::test_app_state(
        dir.path(),
        Library::new(),
        SpotifyLibraryState {
            account_id: "spotify".into(),
            complete: false,
            saved_tracks: BTreeMap::from([(uri.into(), None)]),
            ..SpotifyLibraryState::default()
        },
        lastfm,
        Arc::clone(&service),
    );
    let incomplete = collection_membership_from(&state.library, &state.spotify_membership);
    assert!(!incomplete.contains(uri));
    service
        .rerank_collection_batch(1, &incomplete, &LastFmMappings::default())
        .await
        .unwrap();
    assert!(!service.snapshot().await.unwrap().matches[&row.stable_id].candidates[0].in_library);

    let mut exact = state.spotify_membership.snapshot();
    exact.complete = true;
    state.spotify_membership.set_for_test(exact);
    let complete = collection_membership_from(&state.library, &state.spotify_membership);
    assert!(complete.contains(uri));
    service
        .rerank_collection_batch(1, &complete, &LastFmMappings::default())
        .await
        .unwrap();
    assert!(service.snapshot().await.unwrap().matches[&row.stable_id].candidates[0].in_library);
}

#[test]
fn collection_cache_requires_track_search_terms_and_literal_singles_is_release_shaped() {
    assert_eq!(
        collection_album_search_term("  sarah brightman eden  "),
        "sarah brightman eden"
    );
    for (query, expected) in [
        (album_search_term("Artist", "Singles"), true),
        (track_search_term("Artist", "Song"), false),
        ("track:\"album:Song\" artist:\"Artist\"".into(), false),
        ("custom album:\"Song\" artist:\"Artist\"".into(), false),
    ] {
        assert_eq!(is_album_search_term(&query), expected, "{query}");
    }
    let collection = collection_row("Artist", "Song");
    let legacy = MatchResult {
        source_id: collection.stable_id.clone(),
        search_term: "album:\"Singles\" artist:\"Artist\"".into(),
        confidence: None,
        selected_uri: None,
        candidates: Vec::new(),
        track_matches: BTreeMap::new(),
    };
    assert!(row_needs_match(&collection, Some(&legacy)));
    let mut release = collection.clone();
    release.album = "Singles".into();
    assert!(!row_needs_match(&release, Some(&legacy)));
}

#[test]
fn fuzzy_strategy_remains_independent_per_target() {
    let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 10);
    aggregate_scrobbles(
        &mut session.rows,
        &[
            scrobble("Artist", "Album", "One", 1),
            scrobble("artist", "album", "one", 2),
            scrobble("Artist", "Album", "Two", 3),
        ],
    );
    let first = session.rows[0].stable_id.clone();
    let second = session.rows[1].stable_id.clone();
    session.matches.insert(
        first.clone(),
        MatchResult {
            source_id: first.clone(),
            search_term: String::new(),
            confidence: Some(Confidence::Exact),
            selected_uri: Some("spotify:track:one".into()),
            candidates: Vec::new(),
            track_matches: BTreeMap::from([(first.clone(), "spotify:track:one".into())]),
        },
    );
    session.matches.insert(
        second.clone(),
        MatchResult {
            source_id: second.clone(),
            search_term: String::new(),
            confidence: Some(Confidence::Exact),
            selected_uri: Some("spotify:track:two".into()),
            candidates: Vec::new(),
            track_matches: BTreeMap::from([(second.clone(), "spotify:track:two".into())]),
        },
    );
    session
        .count_modes
        .insert("spotify:track:one".into(), CountMode::Sum);
    session
        .count_modes
        .insert("spotify:track:two".into(), CountMode::Zero);
    assert_eq!(
        historical_count_for_target(&session, "spotify:track:one", &[&session.rows[0]]),
        2
    );
    assert_eq!(
        historical_count_for_target(&session, "spotify:track:two", &[&session.rows[1]]),
        0
    );
}

#[test]
fn history_is_max_count_and_max_latest_with_earliest_added_at() {
    let mut library = Library::new();
    let id = library.add(retune_core::model::NewTrack {
        uri: "spotify:track:song".into(),
        source: SourceId::Music,
        art: "Artist".into(),
        alb: "Album".into(),
        name: "Song".into(),
        duration: Duration::from_secs(1),
        added_at: Some(50),
        ..retune_core::model::NewTrack::default()
    });
    library.merge_history_absolute("spotify:track:song", Some(8), None, Some(90));
    apply_history_updates(
        &mut library,
        &[HistoryUpdate {
            uri: "spotify:track:song".into(),
            play_count: Some(4),
            earliest: Some(10),
            latest: Some(100),
        }],
    );
    let track = library.get(id).unwrap();
    assert_eq!(
        (track.play_count, track.last_played_at, track.added_at),
        (8, Some(100), Some(10))
    );
    apply_history_updates(
        &mut library,
        &[HistoryUpdate {
            uri: "spotify:track:song".into(),
            play_count: Some(0),
            earliest: None,
            latest: None,
        }],
    );
    assert_eq!(library.get(id).unwrap().play_count, 8);
}

#[test]
fn history_reapply_populates_a_track_materialized_by_content_import() {
    let mut library = Library::new();
    let updates = [HistoryUpdate {
        uri: "spotify:track:new".into(),
        play_count: Some(42),
        earliest: Some(10),
        latest: Some(100),
    }];
    apply_history_updates(&mut library, &updates);
    let id = library.add(retune_core::model::NewTrack {
        uri: "spotify:track:new".into(),
        ..Default::default()
    });
    apply_history_updates(&mut library, &updates);
    apply_history_updates(&mut library, &updates);
    let track = library.get(id).unwrap();
    assert_eq!(
        (track.play_count, track.last_played_at, track.added_at),
        (42, Some(100), Some(10))
    );
}

#[test]
fn content_and_history_intents_are_independent_but_not_both_empty() {
    let defaults = ImportDefaults::default();
    assert_eq!(
        (
            defaults.import_content,
            defaults.include_historical_play_counts,
            defaults.whole_album
        ),
        (true, true, false)
    );
    assert!(PageOptions {
        import_content: true,
        include_historical_play_counts: false,
        ..PageOptions::default()
    }
    .validate()
    .is_ok());
    assert!(PageOptions {
        import_content: false,
        include_historical_play_counts: true,
        ..PageOptions::default()
    }
    .validate()
    .is_ok());
    assert!(PageOptions {
        import_content: false,
        include_historical_play_counts: false,
        ..PageOptions::default()
    }
    .validate()
    .is_err());
    for rating in [0, 6] {
        assert!(PageOptions {
            rating: Some(rating),
            ..PageOptions::default()
        }
        .validate()
        .is_err());
    }
    assert!(PageOptions {
        import_content: false,
        include_historical_play_counts: true,
        whole_album: true,
        ..PageOptions::default()
    }
    .validate()
    .is_err());
}

#[tokio::test]
async fn accept_and_next_marks_unselected_source_rows_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 10);
    session.rows = vec![
        SourceRow {
            stable_id: "matched".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            track: "Matched".into(),
            variants: Vec::new(),
            play_count: 1,
            earliest: 1,
            latest: 1,
        },
        SourceRow {
            stable_id: "unmatched".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            track: "Unmatched".into(),
            variants: Vec::new(),
            play_count: 1,
            earliest: 1,
            latest: 1,
        },
    ];
    session.batches = build_review_batches(&session.rows);
    session.phase = ImportPhase::Review;
    session.matches.insert(
        "matched".into(),
        MatchResult {
            source_id: "matched".into(),
            search_term: "Matched".into(),
            confidence: Some(Confidence::Exact),
            selected_uri: Some("spotify:track:matched".into()),
            candidates: Vec::new(),
            track_matches: BTreeMap::from([("matched".into(), "spotify:track:matched".into())]),
        },
    );
    service.save(session).await.unwrap();
    let session = service.snapshot().await.unwrap();
    let options = PageOptions {
        import_content: false,
        selected_track_ids: BTreeSet::from(["matched".into()]),
        ..PageOptions::default()
    };
    let plan = build_apply_plan(
        &session,
        "spotify",
        1,
        "Artist",
        "Album",
        &["matched".into()],
        true,
        options.clone(),
    )
    .unwrap();
    assert_eq!(plan.committed_ids, vec!["matched"]);
    commit_apply_plan(&service, &plan).await.unwrap();
    let session = service.snapshot().await.unwrap();
    assert_eq!(session.remaining(), 1);
    assert_eq!(
        default_decision(&session, "matched").status,
        RowStatus::Done
    );
    assert_eq!(
        default_decision(&session, "unmatched").status,
        RowStatus::Skipped
    );
    assert_eq!(
        session.page_options[&batch_options_key(1)].selected_track_ids,
        options.selected_track_ids
    );
}

#[tokio::test]
async fn accept_all_plan_marks_unselected_source_rows_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 10);
    session.rows = vec![
        SourceRow {
            stable_id: "matched".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            track: "Matched".into(),
            variants: Vec::new(),
            play_count: 1,
            earliest: 1,
            latest: 1,
        },
        SourceRow {
            stable_id: "unmatched".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            track: "Unmatched".into(),
            variants: Vec::new(),
            play_count: 1,
            earliest: 1,
            latest: 1,
        },
    ];
    session.batches = build_review_batches(&session.rows);
    session.phase = ImportPhase::Review;
    session.page_options.insert(
        batch_options_key(1),
        PageOptions {
            import_content: false,
            selected_track_ids: BTreeSet::from(["matched".into()]),
            ..PageOptions::default()
        },
    );
    session.matches.insert(
        "matched".into(),
        MatchResult {
            source_id: "matched".into(),
            search_term: "Matched".into(),
            confidence: Some(Confidence::Exact),
            selected_uri: Some("spotify:track:matched".into()),
            candidates: Vec::new(),
            track_matches: BTreeMap::from([("matched".into(), "spotify:track:matched".into())]),
        },
    );
    service.save(session.clone()).await.unwrap();
    service
        .mutate_sync(|sync| {
            sync.accept_all = Some(AcceptAllCursor {
                session_id: session.cache_id.clone(),
                lastfm_username: "user".into(),
                spotify_account_id: "spotify".into(),
                next_batch_index: 0,
            });
            Ok(())
        })
        .await
        .unwrap();
    assert!(enqueue_next_accept_all_job(&service).await.unwrap());
    let job = service.next_apply_job().await.unwrap();
    assert_eq!(job.plan.committed_ids, vec!["matched"]);
    service.claim_apply_job(&job.id).await.unwrap().unwrap();
    commit_apply_plan(&service, &job.plan).await.unwrap();
    service.remove_apply_job(&job.id).await.unwrap();
    let session = service.snapshot().await.unwrap();
    assert_eq!(
        default_decision(&session, "matched").status,
        RowStatus::Done
    );
    assert_eq!(
        default_decision(&session, "unmatched").status,
        RowStatus::Skipped
    );
}

#[test]
fn content_only_history_update_preserves_counts_and_last_played() {
    let mut library = Library::new();
    let id = library.add(retune_core::model::NewTrack {
        uri: "spotify:track:content".into(),
        added_at: Some(50),
        ..Default::default()
    });
    library.merge_history_absolute("spotify:track:content", Some(8), None, Some(90));
    apply_history_updates(
        &mut library,
        &[HistoryUpdate {
            uri: "spotify:track:content".into(),
            play_count: None,
            earliest: Some(10),
            latest: None,
        }],
    );
    let track = library.get(id).unwrap();
    assert_eq!(
        (track.play_count, track.last_played_at, track.added_at),
        (8, Some(90), Some(10))
    );
}

#[tokio::test]
async fn fake_spotify_transport_keeps_album_and_track_import_memberships_exact() {
    let album = membership_uris_for_import(
        true,
        true,
        Some("spotify:album:album"),
        &["spotify:track:ignored".into()],
    )
    .unwrap();
    let tracks = membership_uris_for_import(
        true,
        false,
        None,
        &[
            "spotify:track:one".into(),
            "spotify:track:two".into(),
            "spotify:track:one".into(),
        ],
    )
    .unwrap();
    let client = retune_spotify::client::fake_client(
        [
            retune_spotify::client::Response::json(204, Value::Null),
            retune_spotify::client::Response::json(204, Value::Null),
        ],
        "user-library-modify",
    );
    client.save_to_library(&album).await.unwrap();
    client.save_to_library(&tracks).await.unwrap();
    let requests = client.transport().requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        url::Url::parse(&requests[0].url)
            .unwrap()
            .query_pairs()
            .find(|(key, _)| key == "uris")
            .unwrap()
            .1,
        "spotify:album:album"
    );
    assert_eq!(
        url::Url::parse(&requests[1].url)
            .unwrap()
            .query_pairs()
            .find(|(key, _)| key == "uris")
            .unwrap()
            .1,
        "spotify:track:one,spotify:track:two"
    );

    let mut album_membership = SpotifyLibraryState {
        account_id: "spotify-user".into(),
        complete: true,
        ..SpotifyLibraryState::default()
    };
    album_membership.add_saved_album(SavedAlbumRecord {
        uri: "spotify:album:album".into(),
        name: "Album".into(),
        artists: vec!["Artist".into()],
        release_date: None,
        album_type: None,
        added_at: Some(100),
        track_uris: vec!["spotify:track:one".into(), "spotify:track:two".into()],
    });
    assert!(album_membership.saved_tracks.is_empty());
    assert_eq!(album_membership.saved_albums.len(), 1);

    let mut track_membership = SpotifyLibraryState {
        account_id: "spotify-user".into(),
        complete: true,
        ..SpotifyLibraryState::default()
    };
    for uri in ["spotify:track:one", "spotify:track:two"] {
        track_membership.add_saved_track(uri.into(), Some(100));
    }
    assert!(track_membership.saved_albums.is_empty());
    assert_eq!(track_membership.saved_tracks.len(), 2);
}

#[test]
fn track_exclusions_defer_backlog_sweeps() {
    assert!(!ReviewAction::Exclude.sweeps_backlog());
    assert!(!ReviewAction::UndoExclude.sweeps_backlog());
    assert!(ReviewAction::IgnoreAlbum.sweeps_backlog());
    assert!(ReviewAction::IgnoreArtist.sweeps_backlog());
    assert!(ReviewAction::Restore.sweeps_backlog());
}

#[tokio::test]
async fn fully_excluded_review_action_reaches_done_and_has_view_only_queue_status() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    start_bound(&service, "lastfm-user", "spotify-user", 500).await;
    service
        .checkpoint_page(
            1,
            &ParsedRecentTracksPage {
                page: 1,
                total_pages: Some(1),
                tracks: vec![
                    scrobble("A", "Album", "One", 1),
                    scrobble("A", "Album", "Two", 2),
                ],
                ..ParsedRecentTracksPage::default()
            },
        )
        .await
        .unwrap();
    service.aggregate_cached(None).await.unwrap();
    let mut session = service.snapshot().await.unwrap();
    session.phase = ImportPhase::Review;
    service.save(session.clone()).await.unwrap();
    for row in &session.rows {
        let ids = vec![row.stable_id.clone()];
        service
            .review_action(
                "lastfm-user",
                "spotify-user",
                1,
                Some(ids.as_slice()),
                ReviewAction::Exclude,
                "A",
                "Album",
            )
            .await
            .unwrap();
    }
    let session = service.snapshot().await.unwrap();
    let refs = session.rows.iter().collect::<Vec<_>>();
    assert_eq!(session.phase, ImportPhase::Done);
    assert_eq!(queue_status(&session, &refs), Some(QueueStatus::Excluded));
    assert_eq!(session.remaining(), 0);
}

#[tokio::test]
async fn album_review_actions_cascade_across_split_batches_and_restore_from_any_batch() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 1000);
    aggregate_scrobbles(
        &mut session.rows,
        &(0..205)
            .map(|index| scrobble("Artist", "Album", &format!("Track {index}"), index + 1))
            .collect::<Vec<_>>(),
    );
    session.phase = ImportPhase::Review;
    service.save(session.clone()).await.unwrap();

    assert!(service
        .review_action(
            "user",
            "spotify",
            1,
            None,
            ReviewAction::Exclude,
            "Artist",
            "Album",
        )
        .await
        .is_err());
    assert!(service
        .review_action(
            "user",
            "spotify",
            1,
            Some(&["not-in-batch".to_owned()]),
            ReviewAction::Exclude,
            "Artist",
            "Album",
        )
        .await
        .is_err());

    service
        .review_action(
            "user",
            "spotify",
            1,
            None,
            ReviewAction::IgnoreAlbum,
            "Artist",
            "Album",
        )
        .await
        .unwrap();
    let queue = service
        .queue_page(0, LASTFM_QUEUE_PAGE_LIMIT)
        .await
        .unwrap()
        .items;
    assert_eq!(queue.len(), 3);
    assert!(queue
        .iter()
        .all(|item| item.status == Some(QueueStatus::IgnoredAlbum) && !item.remaining));

    service
        .review_action(
            "user",
            "spotify",
            2,
            None,
            ReviewAction::Restore,
            "Artist",
            "Album",
        )
        .await
        .unwrap();
    let queue = service
        .queue_page(0, LASTFM_QUEUE_PAGE_LIMIT)
        .await
        .unwrap()
        .items;
    assert!(queue
        .iter()
        .all(|item| item.status.is_none() && item.remaining));

    service
        .review_action(
            "user",
            "spotify",
            3,
            None,
            ReviewAction::SkipAlbum,
            "Artist",
            "Album",
        )
        .await
        .unwrap();
    let queue = service
        .queue_page(0, LASTFM_QUEUE_PAGE_LIMIT)
        .await
        .unwrap()
        .items;
    assert!(queue
        .iter()
        .all(|item| item.status == Some(QueueStatus::Skipped) && item.remaining));
}

#[tokio::test]
async fn album_review_actions_cover_every_identity_in_a_cluster() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 100);
    aggregate_scrobbles(
        &mut session.rows,
        &[
            scrobble("Artist", "Release", "One", 1),
            scrobble("Artist", "Release: Best", "Two", 2),
        ],
    );
    session.phase = ImportPhase::Review;
    service.save(session).await.unwrap();

    service
        .review_action(
            "user",
            "spotify",
            1,
            None,
            ReviewAction::IgnoreAlbum,
            "Artist",
            "Release: Best",
        )
        .await
        .unwrap();
    let session = service.snapshot().await.unwrap();
    assert!(session.rows.iter().all(|row| {
        default_decision(&session, &row.stable_id).status == RowStatus::IgnoredAlbum
    }));
    let mappings = service.mappings_for("user", Some("spotify")).await.unwrap();
    assert_eq!(
        mappings.ignored_albums,
        BTreeSet::from([
            source_album_key("Artist", "Release"),
            source_album_key("Artist", "Release: Best"),
        ])
    );

    service
        .review_action(
            "user",
            "spotify",
            1,
            None,
            ReviewAction::Restore,
            "Artist",
            "Release: Best",
        )
        .await
        .unwrap();
    let session = service.snapshot().await.unwrap();
    assert!(session
        .rows
        .iter()
        .all(|row| { default_decision(&session, &row.stable_id).status == RowStatus::Pending }));
    assert!(service
        .mappings_for("user", Some("spotify"))
        .await
        .unwrap()
        .ignored_albums
        .is_empty());
}

#[tokio::test]
async fn owned_review_mutations_reject_mismatch_and_suspension() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let mut session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 10);
    session.phase = ImportPhase::Review;
    service.save(session).await.unwrap();

    assert!(service
        .set_search_terms("other", "spotify", false)
        .await
        .is_err());
    let mut session = service.snapshot().await.unwrap();
    session.phase = ImportPhase::Suspended;
    service.save(session).await.unwrap();

    assert!(service
        .review_action(
            "user",
            "spotify",
            1,
            Some(&["id".to_owned()]),
            ReviewAction::Exclude,
            "Artist",
            "Album"
        )
        .await
        .is_err());
    assert!(service
        .update_options(
            "user",
            "spotify",
            1,
            "Artist",
            "Album",
            PageOptions::default(),
        )
        .await
        .is_err());
    assert!(service
        .set_count_mode("user", "spotify", "spotify:track:target", CountMode::Zero)
        .await
        .is_err());
    assert!(service
        .set_search_terms("user", "spotify", false)
        .await
        .is_err());
    assert!(service
        .set_match(
            "user",
            "spotify",
            1,
            MatchResult {
                source_id: "id".into(),
                search_term: "track".into(),
                confidence: None,
                selected_uri: None,
                candidates: Vec::new(),
                track_matches: BTreeMap::new(),
            },
        )
        .await
        .is_err());
    assert!(service
        .select_match("user", "spotify", 1, "id", "spotify:track:target")
        .await
        .is_err());

    let session = service.snapshot().await.unwrap();
    assert_eq!(session.phase, ImportPhase::Suspended);
    assert!(session.page_options.is_empty());
    assert!(session.matches.is_empty());
    assert!(session.search_terms);
}

#[test]
fn persistence_round_trip_quarantines_corrupt_unknown_and_rejects_oversize() {
    let dir = tempfile::tempdir().unwrap();
    let store = ImportSessionStore::new(dir.path());
    let session = LastFmImportSessionV2::new("user".into(), "spotify".into(), 42);
    store.save(&session).unwrap();
    assert_eq!(store.load().unwrap(), Some(session.clone()));
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(dir.path().join("lastfm-import.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    let mut invalid_options = session.clone();
    invalid_options.page_options.insert(
        "Artist\u{1f}Album".into(),
        PageOptions {
            rating: Some(6),
            ..PageOptions::default()
        },
    );
    store.save(&invalid_options).unwrap();
    assert_eq!(store.load().unwrap(), None);

    let mut invalid = session.clone();
    invalid.defaults = ImportDefaults {
        import_content: false,
        include_historical_play_counts: false,
        whole_album: false,
    };
    store.save(&invalid).unwrap();
    assert_eq!(store.load().unwrap(), None);

    fs::write(dir.path().join("lastfm-import.json"), br"not json").unwrap();
    assert_eq!(store.load().unwrap(), None);
    assert!(fs::read_dir(dir.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("quarantine")
    }));

    let mut unknown = LastFmImportSessionV2::new("user".into(), "spotify".into(), 42);
    unknown.version = 99;
    store.save(&unknown).unwrap();
    assert_eq!(store.load().unwrap(), None);
    assert!(fs::read_dir(dir.path()).unwrap().count() >= 2);

    let mut too_large = LastFmImportSessionV2::new("user".into(), "spotify".into(), 42);
    too_large.rows.push(SourceRow {
        stable_id: "x".into(),
        artist: "a".into(),
        album: "b".into(),
        track: "c".into(),
        variants: vec![SourceVariant {
            artist: "a".into(),
            album: "b".into(),
            track: "c".into(),
            play_count: 1,
            earliest: 1,
            latest: 1,
        }],
        play_count: 1,
        earliest: 1,
        latest: 1,
    });
    too_large.rows[0].variants[0].track = "x".repeat(MAX_SERIALIZED_SESSION_BYTES);
    assert!(store.save(&too_large).is_err());
}

#[tokio::test]
async fn page_checkpoint_resume_is_idempotent_and_account_mismatch_suspends() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    start_bound(&service, "lastfm-user", "spotify-user", 500).await;
    service.set_metadata(2, 2).await.unwrap();
    let parsed = parsed_page(2, 2, vec![scrobble("Artist", "Album", "Track", 10)]);
    service.checkpoint_page(2, &parsed).await.unwrap();
    service.checkpoint_page(2, &parsed).await.unwrap();
    service
        .checkpoint_page(1, &parsed_page(1, 2, Vec::new()))
        .await
        .unwrap();
    service.aggregate_cached(None).await.unwrap();
    let resumed = Service::new(dir.path());
    let session = resumed.snapshot().await.unwrap();
    assert_eq!(session.next_page, 0);
    assert_eq!(session.rows.len(), 1);
    assert_eq!(session.included_scrobbles, 1);

    let mismatch = resumed.start_or_resume("other-user", 600, None).await;
    assert!(mismatch.is_err());
    assert_eq!(
        resumed.snapshot().await.unwrap().phase,
        ImportPhase::Suspended
    );
    let resumed_for_owner = resumed
        .start_or_resume("lastfm-user", 600, None)
        .await
        .unwrap();
    assert_eq!(resumed_for_owner.phase, Some(ImportPhase::Review));
}

#[tokio::test]
async fn search_terms_preference_round_trips_on_resume() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    start_bound(&service, "lastfm-user", "spotify-user", 500).await;
    let mut review_session = service.snapshot().await.unwrap();
    review_session.phase = ImportPhase::Review;
    service.save(review_session).await.unwrap();
    service
        .set_search_terms("lastfm-user", "spotify-user", false)
        .await
        .unwrap();
    assert!(!service.state().await.search_terms);
    let resumed = Service::new(dir.path());
    assert!(!resumed.state().await.search_terms);
}

#[tokio::test]
async fn overlapping_mutations_preserve_memory_and_disk_changes() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    start_bound(&service, "lastfm-user", "spotify-user", 500).await;
    let mut review_session = service.snapshot().await.unwrap();
    review_session.phase = ImportPhase::Review;
    service.save(review_session).await.unwrap();
    let (search_terms, count_mode) = tokio::join!(
        service.set_search_terms("lastfm-user", "spotify-user", false),
        service.set_count_mode(
            "lastfm-user",
            "spotify-user",
            "spotify:track:target",
            CountMode::Overwrite
        ),
    );
    search_terms.unwrap();
    count_mode.unwrap();
    let current = service.snapshot().await.unwrap();
    assert!(!current.search_terms);
    assert_eq!(current.default_count_mode, CountMode::Overwrite);
    assert!(current.count_modes.is_empty());
    let resumed = Service::new(dir.path());
    let persisted = resumed.snapshot().await.unwrap();
    assert!(!persisted.search_terms);
    assert_eq!(persisted.default_count_mode, CountMode::Overwrite);
    assert!(persisted.count_modes.is_empty());
}

#[tokio::test]
async fn failed_blocking_persistence_does_not_commit_live_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let mut service = Service::new(dir.path());
    start_bound(&service, "lastfm-user", "spotify-user", 500).await;
    let mut review_session = service.snapshot().await.unwrap();
    review_session.phase = ImportPhase::Review;
    service.save(review_session).await.unwrap();
    Arc::get_mut(&mut service).unwrap().store.path = dir.path().to_path_buf();

    let error = service
        .set_search_terms("lastfm-user", "spotify-user", false)
        .await
        .unwrap_err();
    assert!(error.contains("Could not save the Last.fm import session."));
    assert!(service.snapshot().await.unwrap().search_terms);
    assert!(
        ImportSessionStore::new(dir.path())
            .load()
            .unwrap()
            .unwrap()
            .search_terms
    );
}

#[tokio::test]
async fn matching_cannot_write_through_suspension() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    start_bound(&service, "lastfm-user", "spotify-user", 500).await;
    service
        .checkpoint_page(
            1,
            &ParsedRecentTracksPage {
                page: 1,
                total_pages: Some(1),
                tracks: vec![scrobble("Artist", "Album", "Track", 10)],
                ..ParsedRecentTracksPage::default()
            },
        )
        .await
        .unwrap();
    service.suspend_for_account_mismatch().await.unwrap();
    let result = service
        .set_matches(
            "lastfm-user",
            "spotify-user",
            1,
            vec![MatchResult {
                source_id: "artist\u{1f}album\u{1f}track".into(),
                search_term: "track search".into(),
                confidence: Some(Confidence::Exact),
                selected_uri: Some("spotify:track:target".into()),
                candidates: Vec::new(),
                track_matches: BTreeMap::new(),
            }],
            None,
        )
        .await;
    assert!(result.is_err());
    let session = service.snapshot().await.unwrap();
    assert_eq!(session.phase, ImportPhase::Suspended);
    assert!(session.matches.is_empty());
}

#[tokio::test]
async fn suspended_reads_are_redacted_and_empty() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    start_bound(&service, "prior-user", "prior-spotify", 500).await;
    service
        .checkpoint_page(
            1,
            &ParsedRecentTracksPage {
                page: 1,
                total_pages: Some(1),
                tracks: vec![scrobble("Prior Artist", "Prior Album", "Track", 10)],
                ..ParsedRecentTracksPage::default()
            },
        )
        .await
        .unwrap();
    service.suspend_for_account_mismatch().await.unwrap();
    let state = service.state().await;
    assert_eq!(state.phase, Some(ImportPhase::Suspended));
    assert_eq!(state.username, None);
    assert_eq!(state.spotify_account_id, None);
    assert_eq!(state.remaining, 0);
    assert!(service
        .queue_page(0, LASTFM_QUEUE_PAGE_LIMIT)
        .await
        .unwrap()
        .items
        .is_empty());
    assert!(service
        .page(1, "Prior Artist", "Prior Album")
        .await
        .is_none());
}

#[tokio::test]
async fn mismatched_pages_do_not_advance_and_page_batches_are_compact() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    start_bound(&service, "lastfm-user", "spotify-user", 500).await;
    service.set_metadata(2, 1).await.unwrap();
    let mismatched = parsed_page(2, 2, vec![scrobble("Artist", "Album", "Track", 10)]);
    assert!(service.checkpoint_page(1, &mismatched).await.is_err());
    assert_eq!(service.snapshot().await.unwrap().next_page, 2);
    service.checkpoint_page(2, &mismatched).await.unwrap();
    let duplicate_page = parsed_page(
        1,
        2,
        vec![
            scrobble("Artist", "Album", "Track", 10),
            scrobble("artist", "album", "track", 20),
        ],
    );
    service.checkpoint_page(1, &duplicate_page).await.unwrap();
    service.aggregate_cached(None).await.unwrap();
    let session = service.snapshot().await.unwrap();
    assert_eq!(session.batches[0].source_ids.len(), 1);
    assert_eq!(session.next_page, 0);
}

#[tokio::test]
async fn queue_reports_exact_entity_counts_for_current_page_choices() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    start_bound(&service, "lastfm-user", "spotify-user", 500).await;
    let parsed = ParsedRecentTracksPage {
        page: 1,
        total_pages: Some(1),
        total: Some(2),
        tracks: vec![
            scrobble("Artist", "Album", "One", 10),
            scrobble("Artist", "Album", "Two", 20),
        ],
        ..ParsedRecentTracksPage::default()
    };
    service.checkpoint_page(1, &parsed).await.unwrap();
    service.aggregate_cached(None).await.unwrap();
    let mut review_session = service.snapshot().await.unwrap();
    review_session.phase = ImportPhase::Review;
    service.save(review_session).await.unwrap();
    let rows = service.snapshot().await.unwrap().rows;
    for row in &rows {
        let uri = format!("spotify:track:{}", row.track.to_lowercase());
        let mut track_matches = BTreeMap::new();
        track_matches.insert(row.stable_id.clone(), uri.clone());
        service
            .set_match(
                "lastfm-user",
                "spotify-user",
                1,
                MatchResult {
                    source_id: row.stable_id.clone(),
                    search_term: "album search".into(),
                    confidence: Some(Confidence::Exact),
                    selected_uri: Some("spotify:album:album".into()),
                    candidates: vec![AlbumCandidate {
                        uri: "spotify:album:album".into(),
                        name: "Album".into(),
                        artist: "Artist".into(),
                        in_library: false,
                        track_uris: vec![uri],
                        track_names: vec![row.track.clone()],
                        track_artists: vec!["Artist".into()],
                        track_albums: vec!["Album".into()],
                        relation: Some(AlbumRelation::BestMatch),
                    }],
                    track_matches,
                },
            )
            .await
            .unwrap();
    }
    let queue = service
        .queue_page(0, LASTFM_QUEUE_PAGE_LIMIT)
        .await
        .unwrap()
        .items;
    assert_eq!((queue[0].album_entities, queue[0].track_entities), (0, 2));

    service
        .update_options(
            "lastfm-user",
            "spotify-user",
            1,
            "Artist",
            "Album",
            PageOptions {
                whole_album: true,
                selected_track_ids: rows.iter().map(|row| row.stable_id.clone()).collect(),
                ..PageOptions::default()
            },
        )
        .await
        .unwrap();
    let queue = service
        .queue_page(0, LASTFM_QUEUE_PAGE_LIMIT)
        .await
        .unwrap()
        .items;
    assert_eq!((queue[0].album_entities, queue[0].track_entities), (1, 0));

    let selected_track_ids = rows
        .iter()
        .map(|row| row.stable_id.clone())
        .collect::<BTreeSet<_>>();
    service
        .update_options(
            "lastfm-user",
            "spotify-user",
            1,
            "Artist",
            "Album",
            PageOptions {
                whole_album: false,
                selected_track_ids,
                ..PageOptions::default()
            },
        )
        .await
        .unwrap();
    let queue = service
        .queue_page(0, LASTFM_QUEUE_PAGE_LIMIT)
        .await
        .unwrap()
        .items;
    assert_eq!((queue[0].album_entities, queue[0].track_entities), (0, 2));

    service
        .update_options(
            "lastfm-user",
            "spotify-user",
            1,
            "Artist",
            "Album",
            PageOptions {
                import_content: false,
                include_historical_play_counts: true,
                ..PageOptions::default()
            },
        )
        .await
        .unwrap();
    let queue = service
        .queue_page(0, LASTFM_QUEUE_PAGE_LIMIT)
        .await
        .unwrap()
        .items;
    assert_eq!((queue[0].album_entities, queue[0].track_entities), (0, 0));
}

#[test]
fn exact_album_match_defaults_to_whole_album_until_user_overrides_it() {
    let mut session =
        LastFmImportSessionV2::new_with_defaults("user".into(), 100, ImportDefaults::default());
    aggregate_scrobbles(
        &mut session.rows,
        &[
            scrobble("Artist", "Album", "One", 1),
            scrobble("Artist", "Album", "Two", 2),
        ],
    );
    session.phase = ImportPhase::Review;
    session.batches = vec![ImportBatch {
        page: 1,
        source_ids: session
            .rows
            .iter()
            .map(|row| row.stable_id.clone())
            .collect(),
        collection_shaped: None,
        representative_artist: None,
        representative_album: None,
        album_labels: Vec::new(),
    }];
    let candidate = AlbumCandidate {
        uri: "spotify:album:album".into(),
        name: "Album".into(),
        artist: "Artist".into(),
        in_library: false,
        track_uris: vec!["spotify:track:one".into(), "spotify:track:two".into()],
        track_names: vec!["One".into(), "Two".into()],
        track_artists: vec!["Artist".into(), "Artist".into()],
        track_albums: vec!["Album".into(), "Album".into()],
        relation: Some(AlbumRelation::BestMatch),
    };
    for (row, target) in session
        .rows
        .clone()
        .into_iter()
        .zip(["spotify:track:one", "spotify:track:two"])
    {
        session.matches.insert(
            row.stable_id.clone(),
            MatchResult {
                source_id: row.stable_id.clone(),
                search_term: "album".into(),
                confidence: Some(Confidence::Exact),
                selected_uri: Some(candidate.uri.clone()),
                candidates: vec![candidate.clone()],
                track_matches: BTreeMap::from([(row.stable_id, target.into())]),
            },
        );
    }

    let options = session.options_for_batch(1, "Artist", "Album");
    assert!(options.whole_album);
    let plan = build_apply_plan(
        &session,
        "spotify-user",
        1,
        "Artist",
        "Album",
        &options
            .selected_track_ids
            .iter()
            .cloned()
            .collect::<Vec<_>>(),
        true,
        options.clone(),
    )
    .unwrap();
    assert!(matches!(
        plan.membership,
        ApplyMembership::Album { ref uri, .. } if uri == "spotify:album:album"
    ));
    assert!(matches!(
        plan.metadata_uris.as_slice(),
        [first, second] if first == "spotify:track:one" && second == "spotify:track:two"
    ));

    session.page_options.insert(
        batch_options_key(1),
        PageOptions {
            whole_album: false,
            selected_track_ids: options.selected_track_ids,
            ..PageOptions::default()
        },
    );
    assert!(!session.options_for_batch(1, "Artist", "Album").whole_album);
}

#[tokio::test]
async fn collection_whole_album_guard_covers_persist_and_apply_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let mut session =
        LastFmImportSessionV2::new_with_defaults("user".into(), 100, ImportDefaults::default());
    aggregate_scrobbles(
        &mut session.rows,
        &[
            scrobble("Artist", "", "One", 1),
            scrobble("Artist", "", "Two", 2),
        ],
    );
    session.phase = ImportPhase::Review;
    session.spotify_account_id = Some("spotify".into());
    session.batches = build_review_batches(&session.rows);
    let selected_ids = session
        .rows
        .iter()
        .map(|row| row.stable_id.clone())
        .collect::<Vec<_>>();
    for (row, target) in session
        .rows
        .clone()
        .into_iter()
        .zip(["spotify:track:one", "spotify:track:two"])
    {
        session.matches.insert(
            row.stable_id.clone(),
            MatchResult {
                source_id: row.stable_id.clone(),
                search_term: track_search_term("Artist", &row.track),
                confidence: Some(Confidence::Exact),
                selected_uri: Some(target.into()),
                candidates: vec![AlbumCandidate {
                    uri: target.into(),
                    name: row.track.clone(),
                    artist: "Artist".into(),
                    track_uris: vec![target.into()],
                    track_names: vec![row.track.clone()],
                    track_artists: vec!["Artist".into()],
                    track_albums: vec![String::new()],
                    relation: Some(AlbumRelation::BestMatch),
                    ..AlbumCandidate::default()
                }],
                track_matches: BTreeMap::from([(row.stable_id, target.into())]),
            },
        );
    }
    service.save(session.clone()).await.unwrap();
    let whole_album = PageOptions {
        whole_album: true,
        selected_track_ids: selected_ids.iter().cloned().collect(),
        ..PageOptions::default()
    };
    assert!(service
        .update_options("user", "spotify", 1, "Artist", "", whole_album.clone(),)
        .await
        .is_err());
    assert!(!service
        .snapshot()
        .await
        .unwrap()
        .page_options
        .contains_key(&batch_options_key(1)));
    assert!(build_apply_plan(
        &session,
        "spotify",
        1,
        "Artist",
        "",
        &selected_ids,
        false,
        whole_album.clone(),
    )
    .is_err());

    let mut coherent = service.snapshot().await.unwrap();
    let album_uri = "spotify:album:album";
    for (row, target) in coherent
        .rows
        .clone()
        .into_iter()
        .zip(["spotify:track:one", "spotify:track:two"])
    {
        coherent.matches.insert(
            row.stable_id.clone(),
            MatchResult {
                source_id: row.stable_id.clone(),
                search_term: album_search_term("Artist", "Singles"),
                confidence: Some(Confidence::Exact),
                selected_uri: Some(album_uri.into()),
                candidates: vec![AlbumCandidate {
                    uri: album_uri.into(),
                    name: "Singles".into(),
                    artist: "Artist".into(),
                    track_uris: vec!["spotify:track:one".into(), "spotify:track:two".into()],
                    track_names: vec!["One".into(), "Two".into()],
                    track_artists: vec!["Artist".into(), "Artist".into()],
                    track_albums: vec!["Singles".into(), "Singles".into()],
                    relation: Some(AlbumRelation::BestMatch),
                    ..AlbumCandidate::default()
                }],
                track_matches: BTreeMap::from([(row.stable_id, target.into())]),
            },
        );
    }
    service.save(coherent).await.unwrap();
    service
        .update_options("user", "spotify", 1, "Artist", "", whole_album.clone())
        .await
        .unwrap();
    let persisted = service.snapshot().await.unwrap();
    assert!(persisted.page_options[&batch_options_key(1)].whole_album);
    let plan = build_apply_plan(
        &persisted,
        "spotify",
        1,
        "Artist",
        "",
        &selected_ids,
        false,
        whole_album,
    )
    .unwrap();
    assert!(matches!(
        plan.membership,
        ApplyMembership::Album { ref uri, .. } if uri == album_uri
    ));
}

#[tokio::test]
async fn selecting_an_album_candidate_remaps_every_related_source_track() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    start_bound(&service, "lastfm-user", "spotify-user", 500).await;
    let parsed = ParsedRecentTracksPage {
        page: 1,
        total_pages: Some(1),
        tracks: vec![
            scrobble("Artist", "Album", "One", 10),
            scrobble("Artist", "Album", "Two", 20),
        ],
        ..ParsedRecentTracksPage::default()
    };
    service.checkpoint_page(1, &parsed).await.unwrap();
    service.aggregate_cached(None).await.unwrap();
    let mut review_session = service.snapshot().await.unwrap();
    review_session.phase = ImportPhase::Review;
    service.save(review_session).await.unwrap();
    let rows = service.snapshot().await.unwrap().rows;
    for row in &rows {
        let old_track = format!("spotify:track:old-{}", row.track.to_lowercase());
        let new_track = format!("spotify:track:new-{}", row.track.to_lowercase());
        let mut track_matches = BTreeMap::new();
        track_matches.insert(row.stable_id.clone(), old_track.clone());
        let mut candidates = vec![
            AlbumCandidate {
                uri: "spotify:album:old".into(),
                name: "Old release".into(),
                artist: "Artist".into(),
                in_library: false,
                track_uris: vec![old_track],
                track_names: vec![row.track.clone()],
                track_artists: vec!["Artist".into()],
                track_albums: vec!["Old release".into()],
                relation: Some(AlbumRelation::BestMatch),
            },
            AlbumCandidate {
                uri: "spotify:album:new".into(),
                name: "Alternate release".into(),
                artist: "Artist".into(),
                in_library: false,
                track_uris: vec![new_track.clone()],
                track_names: vec![row.track.clone()],
                track_artists: vec!["Artist".into()],
                track_albums: vec!["Alternate release".into()],
                relation: Some(AlbumRelation::BestMatch),
            },
        ];
        if row.stable_id == rows[0].stable_id {
            candidates.push(AlbumCandidate {
                uri: "spotify:track:rematched".into(),
                name: row.track.clone(),
                artist: "Artist".into(),
                in_library: false,
                track_uris: vec!["spotify:track:rematched".into()],
                track_names: vec![row.track.clone()],
                track_artists: vec!["Artist".into()],
                track_albums: vec!["The Classics".into()],
                relation: None,
            });
        }
        service
            .set_match(
                "lastfm-user",
                "spotify-user",
                1,
                MatchResult {
                    source_id: row.stable_id.clone(),
                    search_term: "album search".into(),
                    confidence: Some(Confidence::Exact),
                    selected_uri: Some("spotify:album:old".into()),
                    candidates,
                    track_matches,
                },
            )
            .await
            .unwrap();
    }

    service
        .select_match(
            "lastfm-user",
            "spotify-user",
            1,
            &rows[0].stable_id,
            "spotify:album:new",
        )
        .await
        .unwrap();
    let session = service.snapshot().await.unwrap();
    for row in rows {
        let result = session.matches.get(&row.stable_id).unwrap();
        assert_eq!(result.selected_uri.as_deref(), Some("spotify:album:new"));
        assert_eq!(
            result.track_matches.get(&row.stable_id),
            Some(&format!("spotify:track:new-{}", row.track.to_lowercase()))
        );
    }

    let rows = service.snapshot().await.unwrap().rows;
    let first_id = rows[0].stable_id.clone();
    let second_id = rows[1].stable_id.clone();
    service
        .select_match(
            "lastfm-user",
            "spotify-user",
            1,
            &first_id,
            "spotify:track:rematched",
        )
        .await
        .unwrap();
    let session = service.snapshot().await.unwrap();
    let first = session.matches.get(&first_id).unwrap();
    assert_eq!(first.selected_uri.as_deref(), Some("spotify:album:new"));
    assert_eq!(first.confidence, Some(Confidence::Exact));
    assert_eq!(first.track_matches.len(), 2);
    assert_eq!(
        first.track_matches.get(&first_id).map(String::as_str),
        Some("spotify:track:rematched")
    );
    assert_eq!(
        first.track_matches.get(&second_id).map(String::as_str),
        Some("spotify:track:new-two")
    );
    assert_eq!(
        first
            .candidates
            .iter()
            .find(|candidate| candidate.uri == "spotify:album:new")
            .map(|candidate| candidate.name.as_str()),
        Some("Alternate release")
    );
    let sibling = session.matches.get(&second_id).unwrap();
    assert_eq!(sibling.selected_uri.as_deref(), Some("spotify:album:new"));
    assert_eq!(sibling.confidence, Some(Confidence::Exact));
    assert_eq!(
        sibling.track_matches.get(&second_id).map(String::as_str),
        Some("spotify:track:new-two")
    );
}

#[test]
fn picker_candidate_refresh_preserves_selection_until_explicit_choice() {
    let old_track = "spotify:track:old".to_owned();
    let old_album = AlbumCandidate {
        uri: "spotify:album:old".into(),
        name: "Old release".into(),
        artist: "Artist".into(),
        in_library: false,
        track_uris: vec![old_track.clone()],
        track_names: vec!["One".into()],
        track_artists: vec!["Artist".into()],
        track_albums: vec!["Old release".into()],
        relation: Some(AlbumRelation::BestMatch),
    };
    let previous = MatchResult {
        source_id: "id".into(),
        search_term: "album search".into(),
        confidence: Some(Confidence::Exact),
        selected_uri: Some(old_album.uri.clone()),
        candidates: vec![old_album],
        track_matches: BTreeMap::from([(String::from("id"), old_track.clone())]),
    };
    let refreshed = match_result_for(
        "id".into(),
        "track search".into(),
        vec![AlbumCandidate {
            uri: "spotify:track:new".into(),
            name: "New result".into(),
            artist: "Artist".into(),
            in_library: false,
            track_uris: vec!["spotify:track:new".into()],
            track_names: vec!["One".into()],
            track_artists: vec!["Artist".into()],
            track_albums: vec!["Release".into()],
            relation: Some(AlbumRelation::BestMatch),
        }],
        "One",
        None,
    );
    let preserved = preserve_match_selection(refreshed, Some(&previous), "id", "One");

    assert_eq!(preserved.selected_uri, previous.selected_uri);
    assert_eq!(preserved.track_matches, previous.track_matches);
    assert!(preserved
        .candidates
        .iter()
        .any(|candidate| candidate.uri == "spotify:album:old"));
    assert!(preserved
        .candidates
        .iter()
        .any(|candidate| candidate.uri == "spotify:track:new"));

    let stale = MatchResult {
        selected_uri: Some("spotify:track:same".into()),
        candidates: vec![AlbumCandidate {
            uri: "spotify:track:same".into(),
            relation: None,
            ..collection_candidate("spotify:track:same", "Song", "Artist", false)
        }],
        track_matches: BTreeMap::from([("id".into(), "spotify:track:same".into())]),
        ..previous.clone()
    };
    let refreshed = match_result_for(
        "id".into(),
        "track search".into(),
        vec![AlbumCandidate {
            uri: "spotify:track:same".into(),
            relation: Some(AlbumRelation::BestMatch),
            ..collection_candidate("spotify:track:same", "Song", "Artist", false)
        }],
        "Song",
        None,
    );
    let preserved = preserve_match_selection(refreshed, Some(&stale), "id", "Song");
    assert_eq!(
        preserved.candidates[0].relation,
        Some(AlbumRelation::BestMatch)
    );

    let unselected = MatchResult {
        selected_uri: None,
        confidence: None,
        track_matches: BTreeMap::new(),
        ..previous
    };
    let automatically_selected = match_result_for(
        "id".into(),
        "album search".into(),
        vec![AlbumCandidate {
            uri: "spotify:album:new".into(),
            name: "New release".into(),
            artist: "Various Artists".into(),
            in_library: false,
            track_uris: vec!["spotify:track:new".into()],
            track_names: vec!["One".into()],
            track_artists: vec!["Artist".into()],
            track_albums: vec!["New release".into()],
            relation: Some(AlbumRelation::BestMatch),
        }],
        "One",
        Some("spotify:album:new"),
    );
    let refreshed =
        preserve_match_selection(automatically_selected, Some(&unselected), "id", "One");
    assert_eq!(refreshed.selected_uri.as_deref(), Some("spotify:album:new"));
}

fn apply_test_plan_for(
    session: &LastFmImportSessionV2,
    batch_id: u32,
    source_id: &str,
    track_uri: &str,
) -> ApplyPlan {
    ApplyPlan {
        session_id: session.cache_id.clone(),
        lastfm_username: session.lastfm_username.clone(),
        spotify_account_id: "spotify-user".into(),
        batch_id,
        artist: "Artist".into(),
        album: "Album".into(),
        committed_ids: vec![source_id.into()],
        archive_batch: false,
        options: PageOptions::default(),
        membership: ApplyMembership::None,
        updates: vec![HistoryUpdate {
            uri: track_uri.into(),
            play_count: Some(3),
            earliest: Some(10),
            latest: Some(20),
        }],
        metadata_uris: Vec::new(),
        mappings: Vec::new(),
    }
}

fn apply_test_plan(session: &LastFmImportSessionV2) -> ApplyPlan {
    apply_test_plan_for(session, 1, "source", "spotify:track:one")
}

#[test]
fn apply_failure_policy_is_independent_of_display_text() {
    let localized: ApplyFailure = crate::spotify_membership::SpotifyActionFailure {
        kind: crate::spotify_membership::SpotifyActionFailureKind::RateLimited,
        message: "Spotify is taking a short break.".into(),
        endpoint_family: Some("/me/tracks".into()),
        retry_at: Some(7_261),
        ambiguous_outcome: false,
        source: None,
    }
    .into();
    assert_eq!(localized.code, ApplyFailureCode::SpotifyRateLimited);
    assert_eq!(localized.endpoint_family.as_deref(), Some("/me/tracks"));
    assert_eq!(localized.retry_at, Some(7_261));
    assert!(!localized.ambiguous_outcome);

    let misleading: ApplyFailure = crate::spotify_membership::SpotifyActionFailure {
        kind: crate::spotify_membership::SpotifyActionFailureKind::Other,
        message: "Spotify rate limited /me/library; this is display copy only".into(),
        endpoint_family: None,
        retry_at: None,
        ambiguous_outcome: false,
        source: None,
    }
    .into();
    assert_eq!(misleading.code, ApplyFailureCode::ApplyFailed);
    assert_eq!(misleading.retry_at, None);

    let ambiguous: ApplyFailure = crate::spotify_membership::SpotifyActionFailure {
        kind: crate::spotify_membership::SpotifyActionFailureKind::Other,
        message: "Outcome is uncertain.".into(),
        endpoint_family: Some("/me/library".into()),
        retry_at: None,
        ambiguous_outcome: true,
        source: None,
    }
    .into();
    assert_eq!(ambiguous.endpoint_family.as_deref(), Some("/me/library"));
    assert!(ambiguous.ambiguous_outcome);
}

#[test]
fn apply_failure_wire_contract_is_closed_and_stable() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../../test/fixtures/ipc-contracts.json")).unwrap();
    assert_eq!(
        serde_json::to_value(ImportApplyFinished::Succeeded { batch_id: 4 }).unwrap(),
        fixture["importApplyFinished"]["succeeded"]
    );
    assert_eq!(
        serde_json::to_value(ApplyFailureCode::SpotifyRateLimited).unwrap(),
        serde_json::json!("spotify-rate-limited")
    );
    assert_eq!(
        serde_json::to_value(ApplyFailureCode::SpotifyQuotaExhausted).unwrap(),
        serde_json::json!("spotify-quota-exhausted")
    );
    assert_eq!(
        serde_json::to_value(ApplyFailureCode::ApplyFailed).unwrap(),
        serde_json::json!("apply-failed")
    );
    assert_eq!(
        serde_json::to_value(apply_failure_event(
            4,
            &ApplyFailure {
                code: ApplyFailureCode::SpotifyRateLimited,
                message: "try later".into(),
                endpoint_family: Some("/me/library".into()),
                retry_at: Some(8_000),
                ambiguous_outcome: false,
            },
        ))
        .unwrap(),
        fixture["importApplyFinished"]["failed"]
    );
}

fn apply_test_session() -> LastFmImportSessionV2 {
    let mut session = LastFmImportSessionV2::new("lastfm-user".into(), "spotify-user".into(), 100);
    session.phase = ImportPhase::Review;
    session.rows = vec![SourceRow {
        stable_id: "source".into(),
        artist: "Artist".into(),
        album: "Album".into(),
        track: "Song".into(),
        variants: vec![SourceVariant {
            artist: "Artist".into(),
            album: "Album".into(),
            track: "Song".into(),
            play_count: 3,
            earliest: 10,
            latest: 20,
        }],
        play_count: 3,
        earliest: 10,
        latest: 20,
    }];
    session.batches = vec![ImportBatch {
        page: 1,
        source_ids: vec!["source".into()],
        collection_shaped: None,
        representative_artist: None,
        representative_album: None,
        album_labels: Vec::new(),
    }];
    session
}

fn apply_test_session_with_two_batches() -> LastFmImportSessionV2 {
    let mut session = apply_test_session();
    session.rows.push(SourceRow {
        stable_id: "source-two".into(),
        artist: "Artist".into(),
        album: "Album".into(),
        track: "Song two".into(),
        variants: vec![SourceVariant {
            artist: "Artist".into(),
            album: "Album".into(),
            track: "Song two".into(),
            play_count: 2,
            earliest: 11,
            latest: 21,
        }],
        play_count: 2,
        earliest: 11,
        latest: 21,
    });
    session.batches.push(ImportBatch {
        page: 2,
        source_ids: vec!["source-two".into()],
        collection_shaped: None,
        representative_artist: None,
        representative_album: None,
        album_labels: Vec::new(),
    });
    session
}

#[tokio::test]
async fn enqueue_persists_frozen_apply_job_before_returning() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    let session = apply_test_session();
    service.save(session.clone()).await.unwrap();
    let plan = apply_test_plan(&session);

    service
        .enqueue_apply_plan(plan.clone(), None)
        .await
        .unwrap();

    let persisted = IncrementalStore::new(directory.path()).load().unwrap();
    assert_eq!(persisted.apply_queue.len(), 1);
    assert_eq!(persisted.apply_queue[0].plan, plan);
    assert_eq!(persisted.apply_queue[0].status, ApplyJobStatus::Queued);
}

#[tokio::test]
async fn enqueue_does_not_recover_or_mutate_an_incremental_journal() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    let session = apply_test_session();
    service.save(session.clone()).await.unwrap();
    service
        .mutate_sync(|sync| {
            sync.journal = Some(LastFmApplicationJournal {
                before_library: Library::new(),
                after_library: Library::new(),
                checkpoint_before: None,
                checkpoint_after: None,
                backlog_before: Vec::new(),
                backlog_after: Vec::new(),
                consumed_receipts: Vec::new(),
            });
            Ok(())
        })
        .await
        .unwrap();
    service
        .enqueue_apply_plan(apply_test_plan(&session), None)
        .await
        .unwrap();
    assert!(service.sync_snapshot().await.journal.is_some());
}

#[tokio::test]
async fn queued_batches_are_hidden_but_failed_batches_reappear_with_choices() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    let session = apply_test_session();
    service.save(session.clone()).await.unwrap();
    let plan = apply_test_plan(&session);
    service
        .enqueue_apply_plan(plan.clone(), None)
        .await
        .unwrap();

    assert_eq!(service.queue_page(0, 10).await.unwrap().total, 0);
    service
        .fail_apply_job_with(
            &format!("{}:1", session.cache_id),
            ApplyFailure {
                code: ApplyFailureCode::SpotifyRateLimited,
                message: "try again".into(),
                endpoint_family: Some("/me/library".into()),
                retry_at: Some(7_261),
                ambiguous_outcome: false,
            },
        )
        .await
        .unwrap();
    let queue = service.queue_page(0, 10).await.unwrap();
    assert_eq!(queue.total, 1);
    assert_eq!(queue.items[0].status, Some(QueueStatus::Failed));
    assert_eq!(queue.items[0].error.as_deref(), Some("try again"));
    assert_eq!(
        queue.items[0].error_code,
        Some(ApplyFailureCode::SpotifyRateLimited)
    );
    assert_eq!(queue.items[0].retry_at, Some(7_261));
    assert_eq!(
        service
            .snapshot()
            .await
            .unwrap()
            .decisions
            .get("source")
            .cloned()
            .unwrap_or_default(),
        RowDecision::default()
    );
    service
        .mutate_sync(|sync| {
            sync.accept_all = Some(AcceptAllCursor {
                session_id: session.cache_id.clone(),
                lastfm_username: session.lastfm_username.clone(),
                spotify_account_id: "spotify-user".into(),
                next_batch_index: 1,
            });
            Ok(())
        })
        .await
        .unwrap();
    service.enqueue_apply_plan(plan, None).await.unwrap();
    let sync = service.sync_snapshot().await;
    let retried = sync.apply_queue.first().unwrap();
    assert_eq!(retried.status, ApplyJobStatus::Queued);
    assert_eq!(retried.error, None);
    assert_eq!(retried.error_code, None);
    assert_eq!(retried.retry_at, None);
    let claimed = service
        .claim_apply_job(&format!("{}:1", session.cache_id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.error, None);
    assert_eq!(claimed.error_code, None);
    assert_eq!(claimed.retry_at, None);
}

#[tokio::test]
async fn queued_committed_rows_leave_remaining_until_they_are_failed_or_removed() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    let session = apply_test_session();
    service.save(session.clone()).await.unwrap();
    let plan = apply_test_plan(&session);
    service.enqueue_apply_plan(plan, None).await.unwrap();

    assert_eq!(service.state().await.remaining, 0);
    service
        .fail_apply_job(&format!("{}:1", session.cache_id), "retry".into())
        .await
        .unwrap();
    assert_eq!(service.state().await.remaining, 1);
}

#[tokio::test]
async fn apply_failures_preserve_an_unrelated_incremental_problem() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    let session = apply_test_session();
    service.save(session.clone()).await.unwrap();
    service
        .mutate_sync(|sync| {
            sync.sync_problem = Some("incremental download is paused".into());
            Ok(())
        })
        .await
        .unwrap();
    service
        .enqueue_apply_plan(apply_test_plan(&session), None)
        .await
        .unwrap();
    service
        .fail_apply_job(&format!("{}:1", session.cache_id), "apply failed".into())
        .await
        .unwrap();
    assert_eq!(
        service.sync_snapshot().await.sync_problem.as_deref(),
        Some("incremental download is paused")
    );
    service
        .remove_apply_job(&format!("{}:1", session.cache_id))
        .await
        .unwrap();
    assert_eq!(
        service.sync_snapshot().await.sync_problem.as_deref(),
        Some("incremental download is paused")
    );
}

#[tokio::test]
async fn accept_all_cursor_checkpoints_survive_restart() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    let session = apply_test_session();
    service.save(session.clone()).await.unwrap();
    let cursor = AcceptAllCursor {
        session_id: session.cache_id.clone(),
        lastfm_username: session.lastfm_username.clone(),
        spotify_account_id: "spotify-user".into(),
        next_batch_index: 4,
    };
    service
        .mutate_sync(|sync| {
            sync.accept_all = Some(cursor.clone());
            Ok(())
        })
        .await
        .unwrap();

    let resumed = Service::new(directory.path());
    assert_eq!(resumed.sync_snapshot().await.accept_all, Some(cursor));
    assert!(resumed.state().await.applying_all);
    assert!(ensure_review_mutable(&resumed).await.is_err());
}

#[tokio::test]
async fn failed_bulk_apply_is_visible_and_retry_restores_its_cursor() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    let session = apply_test_session();
    service.save(session.clone()).await.unwrap();
    let cursor = AcceptAllCursor {
        session_id: session.cache_id.clone(),
        lastfm_username: session.lastfm_username.clone(),
        spotify_account_id: "spotify-user".into(),
        next_batch_index: 3,
    };
    service
        .mutate_sync(|sync| {
            sync.accept_all = Some(cursor);
            Ok(())
        })
        .await
        .unwrap();
    let plan = apply_test_plan(&session);
    service
        .enqueue_apply_plan(plan.clone(), Some(2))
        .await
        .unwrap();
    service
        .claim_apply_job(&format!("{}:1", session.cache_id))
        .await
        .unwrap()
        .unwrap();
    service
        .fail_apply_job(&format!("{}:1", session.cache_id), "retry".into())
        .await
        .unwrap();
    assert!(!apply_work_pending(&service).await);
    assert!(!service.state().await.applying_all);
    assert!(service.sync_snapshot().await.accept_all.is_none());

    let restarted = Service::new(directory.path());
    let failed_queue = restarted.queue_page(0, 10).await.unwrap();
    assert_eq!(failed_queue.items[0].status, Some(QueueStatus::Failed));
    restarted
        .retry_failed_apply(
            &session.cache_id,
            1,
            &session.lastfm_username,
            "spotify-user",
        )
        .await
        .unwrap();
    assert!(restarted.state().await.applying_all);
    let retried = restarted.next_apply_job().await.unwrap();
    assert_eq!(retried.bulk_index, Some(2));
    restarted.remove_apply_job(&retried.id).await.unwrap();
    assert_eq!(
        restarted
            .sync_snapshot()
            .await
            .accept_all
            .unwrap()
            .next_batch_index,
        3
    );
}

#[tokio::test]
async fn failed_apply_retry_keeps_frozen_plan_and_queue_order() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    let session = apply_test_session_with_two_batches();
    service.save(session.clone()).await.unwrap();
    let first = apply_test_plan_for(&session, 1, "source", "spotify:track:one");
    let second = apply_test_plan_for(&session, 2, "source-two", "spotify:track:two");
    service
        .enqueue_apply_plan(first.clone(), None)
        .await
        .unwrap();
    service.enqueue_apply_plan(second, None).await.unwrap();
    service
        .claim_apply_job(&format!("{}:1", session.cache_id))
        .await
        .unwrap();
    service
        .fail_apply_job(&format!("{}:1", session.cache_id), "retry".into())
        .await
        .unwrap();
    assert!(service.next_apply_job().await.is_none());
    assert!(service
        .enqueue_apply_plan(
            apply_test_plan_for(&session, 3, "source-two", "spotify:track:two"),
            None,
        )
        .await
        .is_err());
    let mut changed = first.clone();
    changed.artist = "Changed".into();
    assert!(service.enqueue_apply_plan(changed, None).await.is_err());
    service
        .retry_failed_apply(
            &session.cache_id,
            1,
            &session.lastfm_username,
            "spotify-user",
        )
        .await
        .unwrap();
    let queue = service.sync_snapshot().await.apply_queue;
    assert_eq!(queue.len(), 2);
    assert_eq!(queue[0].plan, first);
    assert_eq!(queue[0].status, ApplyJobStatus::Queued);
    assert_eq!(queue[1].plan.batch_id, 2);
}

#[tokio::test]
async fn a_blocked_worker_does_not_block_enqueue_or_next_batch_projection() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    let session = apply_test_session_with_two_batches();
    service.save(session.clone()).await.unwrap();
    let first = apply_test_plan_for(&session, 1, "source", "spotify:track:one");
    let second = apply_test_plan_for(&session, 2, "source-two", "spotify:track:two");
    service.enqueue_apply_plan(first, None).await.unwrap();
    let first_job_id = format!("{}:1", session.cache_id);

    let (claimed_sender, claimed_receiver) = tokio::sync::oneshot::channel();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let worker_barrier = Arc::clone(&barrier);
    let worker_service = Arc::clone(&service);
    let worker = tokio::spawn(async move {
        let claimed = worker_service
            .claim_apply_job(&first_job_id)
            .await
            .unwrap()
            .is_some();
        claimed_sender.send(claimed).unwrap();
        worker_barrier.wait().await;
    });
    assert!(claimed_receiver.await.unwrap());

    let next_batch = service.queue_page(0, 10).await.unwrap();
    assert_eq!(next_batch.total, 1);
    assert_eq!(next_batch.items[0].page, 2);
    service.enqueue_apply_plan(second, None).await.unwrap();
    assert_eq!(service.sync_snapshot().await.apply_queue.len(), 2);

    barrier.wait().await;
    worker.await.unwrap();
}

#[tokio::test]
async fn apply_effect_executor_runs_serial_stages_and_removes_after_decision() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    let session = apply_test_session();
    service.save(session.clone()).await.unwrap();
    service
        .enqueue_apply_plan(apply_test_plan(&session), None)
        .await
        .unwrap();
    let job = service
        .claim_apply_job(&format!("{}:1", session.cache_id))
        .await
        .unwrap()
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_by_effect = Arc::clone(&seen);
    let service_by_effect = Arc::clone(&service);
    execute_apply_job(&service, &job, move |stage, _plan| {
        let seen = Arc::clone(&seen_by_effect);
        let service = Arc::clone(&service_by_effect);
        Box::pin(async move {
            seen.lock().await.push(stage);
            if stage == ApplyJobStage::Decision {
                assert_eq!(service.sync_snapshot().await.apply_queue.len(), 1);
            }
            Ok(())
        })
    })
    .await
    .unwrap();
    assert_eq!(
        *seen.lock().await,
        vec![
            ApplyJobStage::Upstream,
            ApplyJobStage::Local,
            ApplyJobStage::Mappings,
            ApplyJobStage::Decision,
        ]
    );
    assert!(service.sync_snapshot().await.apply_queue.is_empty());
}

#[tokio::test]
async fn blocked_apply_effect_does_not_block_a_second_enqueue_or_projection() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    let session = apply_test_session_with_two_batches();
    service.save(session.clone()).await.unwrap();
    service
        .enqueue_apply_plan(
            apply_test_plan_for(&session, 1, "source", "spotify:track:one"),
            None,
        )
        .await
        .unwrap();
    let first = service
        .claim_apply_job(&format!("{}:1", session.cache_id))
        .await
        .unwrap()
        .unwrap();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let started = Arc::new(Mutex::new(Some(started_tx)));
    let release = Arc::new(Mutex::new(Some(release_rx)));
    let effect = tokio::spawn({
        let service = Arc::clone(&service);
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        async move {
            execute_apply_job(&service, &first, move |stage, _plan| {
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                Box::pin(async move {
                    if stage == ApplyJobStage::Upstream {
                        if let Some(sender) = started.lock().await.take() {
                            sender.send(()).unwrap();
                        }
                        let receiver = release.lock().await.take();
                        if let Some(receiver) = receiver {
                            receiver.await.unwrap();
                        }
                    }
                    Ok(())
                })
            })
            .await
        }
    });
    started_rx.await.unwrap();
    let second = apply_test_plan_for(&session, 2, "source-two", "spotify:track:two");
    service.enqueue_apply_plan(second, None).await.unwrap();
    assert_eq!(service.state().await.remaining, 0);
    assert_eq!(service.sync_snapshot().await.apply_queue.len(), 2);
    assert_eq!(service.next_apply_job().await.unwrap().plan.batch_id, 1);
    release_tx.send(()).unwrap();
    effect.await.unwrap().unwrap();
    assert_eq!(service.next_apply_job().await.unwrap().plan.batch_id, 2);
}

#[tokio::test]
async fn apply_worker_rechecks_after_release_when_enqueue_races_empty_observation() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    let session = apply_test_session();
    service.save(session.clone()).await.unwrap();
    let run = service.claim_apply_runner().unwrap();
    let (observed_tx, observed_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let worker_service = Arc::clone(&service);
    let worker = tokio::spawn(async move {
        let run = run;
        assert!(!apply_work_pending(&worker_service).await);
        observed_tx.send(()).unwrap();
        release_rx.await.unwrap();
        drop(run);
        assert!(apply_work_pending(&worker_service).await);
        assert!(worker_service.claim_apply_runner().is_some());
    });
    observed_rx.await.unwrap();
    service
        .enqueue_apply_plan(apply_test_plan(&session), None)
        .await
        .unwrap();
    release_tx.send(()).unwrap();
    worker.await.unwrap();
}

#[tokio::test]
async fn cancelled_and_panicked_runner_guards_release_their_claims() {
    for panic in [false, true] {
        let running = Arc::new(AtomicBool::new(false));
        let task_running = Arc::clone(&running);
        let started = Arc::new(tokio::sync::Notify::new());
        let task_started = Arc::clone(&started);
        let task = tokio::spawn(async move {
            let _run = RunnerGuard::claim(&task_running).unwrap();
            task_started.notify_one();
            if panic {
                panic!("runner panic");
            }
            std::future::pending::<()>().await;
        });
        started.notified().await;
        if panic {
            assert!(task.await.unwrap_err().is_panic());
        } else {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        }
        assert!(RunnerGuard::claim(&running).is_some());
    }
}

#[tokio::test]
async fn each_failed_apply_stage_retries_from_its_persisted_checkpoint() {
    for failed_stage in [
        ApplyJobStage::Upstream,
        ApplyJobStage::Local,
        ApplyJobStage::Mappings,
        ApplyJobStage::Decision,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let service = Service::new(directory.path());
        let session = apply_test_session();
        service.save(session.clone()).await.unwrap();
        let plan = apply_test_plan(&session);
        service
            .enqueue_apply_plan(plan.clone(), None)
            .await
            .unwrap();
        let job = service
            .claim_apply_job(&format!("{}:1", session.cache_id))
            .await
            .unwrap()
            .unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_by_effect = Arc::clone(&seen);
        let failure = failed_stage;
        let result = execute_apply_job(&service, &job, move |stage, _plan| {
            let seen = Arc::clone(&seen_by_effect);
            Box::pin(async move {
                seen.lock().await.push(stage);
                if stage == failure {
                    Err(ApplyFailure::apply_failed(format!("failed at {stage:?}")))
                } else {
                    Ok(())
                }
            })
        })
        .await;
        assert!(result.is_err());
        service
            .fail_apply_job(&job.id, result.unwrap_err().message)
            .await
            .unwrap();
        service.enqueue_apply_plan(plan, None).await.unwrap();
        let retried = service.claim_apply_job(&job.id).await.unwrap().unwrap();
        assert_eq!(retried.stage, failed_stage);
        let resumed = Arc::new(Mutex::new(Vec::new()));
        let resumed_by_effect = Arc::clone(&resumed);
        execute_apply_job(&service, &retried, move |stage, _plan| {
            let resumed = Arc::clone(&resumed_by_effect);
            Box::pin(async move {
                resumed.lock().await.push(stage);
                Ok(())
            })
        })
        .await
        .unwrap();
        assert_eq!(
            *resumed.lock().await,
            [
                ApplyJobStage::Upstream,
                ApplyJobStage::Local,
                ApplyJobStage::Mappings,
                ApplyJobStage::Decision,
            ][failed_stage as usize..]
                .to_vec()
        );
    }
}

#[tokio::test]
async fn restarting_a_running_job_resumes_before_a_queued_job() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    let session = apply_test_session_with_two_batches();
    service.save(session.clone()).await.unwrap();
    service
        .enqueue_apply_plan(
            apply_test_plan_for(&session, 1, "source", "spotify:track:one"),
            None,
        )
        .await
        .unwrap();
    service
        .enqueue_apply_plan(
            apply_test_plan_for(&session, 2, "source-two", "spotify:track:two"),
            None,
        )
        .await
        .unwrap();
    let first_id = format!("{}:1", session.cache_id);
    service.claim_apply_job(&first_id).await.unwrap().unwrap();
    service
        .mark_apply_stage(&first_id, ApplyJobStage::Mappings)
        .await
        .unwrap();
    let restarted = Service::new(directory.path());
    let first = restarted.next_apply_job().await.unwrap();
    assert_eq!(first.status, ApplyJobStatus::Running);
    assert_eq!(first.stage, ApplyJobStage::Mappings);
    execute_apply_job(&restarted, &first, |_stage, _plan| {
        Box::pin(async { Ok(()) })
    })
    .await
    .unwrap();
    let second = restarted.next_apply_job().await.unwrap();
    assert_eq!(second.plan.batch_id, 2);
}

#[tokio::test]
async fn production_upstream_effect_saves_one_album_and_checkpoint_replay_skips_it() {
    let directory = tempfile::tempdir().unwrap();
    let (_, service, state) = test_app_state(directory.path(), Library::new(), &[]);
    let mut session = apply_test_session();
    session.lastfm_username = "user".into();
    session.cache_id = snapshot_cache_id("user", session.history_to);
    service.save(session.clone()).await.unwrap();
    state.spotify_membership.set_for_test(SpotifyLibraryState {
        account_id: "spotify-user".into(),
        complete: true,
        ..SpotifyLibraryState::default()
    });
    state
        .token_store
        .save(&Tokens {
            access: "access".into(),
            refresh: "refresh".into(),
            expires_at: u64::MAX,
            scopes: "user-library-modify".into(),
            playback_credentials: None,
        })
        .unwrap();
    let client = fake_client(
        [
            Response::json(
                200,
                serde_json::json!({
                    "id": "one", "uri": "spotify:album:one", "name": "Album",
                    "artists": [], "images": [],
                    "tracks": {"items": [{
                        "uri": "spotify:track:one", "name": "One", "duration_ms": 1000,
                        "artists": [], "album": null
                    }], "next": null, "total": 1}
                }),
            ),
            Response::json(204, Value::Null),
        ],
        "user-library-modify",
    );
    let mut plan = apply_test_plan(&session);
    plan.membership = ApplyMembership::Album {
        uri: "spotify:album:one".into(),
        name: "Album".into(),
        artist: "Artist".into(),
    };
    plan.options.whole_album = true;
    service.enqueue_apply_plan(plan, None).await.unwrap();
    let job = service
        .claim_apply_job(&format!("{}:1", session.cache_id))
        .await
        .unwrap()
        .unwrap();
    {
        let mut membership = state.spotify_membership.lock().await;
        let library_owner = state.library.owner();
        run_apply_upstream_effect(
            &service,
            &mut membership,
            &library_owner,
            &state.cooldown_store,
            &client,
            &job.plan,
            crate::unix_now(),
        )
        .await
        .unwrap();
    }
    let requests = client.transport().requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.contains("/me/library"))
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.contains("/me/tracks"))
            .count(),
        0
    );
    let library_request = requests
        .iter()
        .find(|request| request.url.contains("/me/library"))
        .unwrap();
    assert_eq!(
        url::Url::parse(&library_request.url)
            .unwrap()
            .query_pairs()
            .find(|(key, _)| key == "uris")
            .unwrap()
            .1,
        "spotify:album:one"
    );
    service
        .mark_apply_stage(&job.id, ApplyJobStage::Local)
        .await
        .unwrap();
    let restarted = Service::new(directory.path());
    let resumed = restarted.next_apply_job().await.unwrap();
    assert_eq!(resumed.stage, ApplyJobStage::Local);
    execute_apply_job(&restarted, &resumed, |_stage, _plan| {
        Box::pin(async { Ok(()) })
    })
    .await
    .unwrap();
    assert_eq!(client.transport().requests().len(), 2);
}

#[tokio::test]
async fn production_upstream_effect_reuses_collection_cache_without_track_reads() {
    let directory = tempfile::tempdir().unwrap();
    let (_, service, state) = test_app_state(directory.path(), Library::new(), &[]);
    let row = collection_test_row("One");
    let mut session = collection_session(std::slice::from_ref(&row));
    let mut album = collection_album(
        "spotify:album:album",
        "Artist",
        &[("One", "spotify:track:one")],
    );
    album.matching.name = "Album".into();
    album.matching.track_albums = vec!["Album".into()];
    album.release_date = Some("1994".into());
    session.collection_album_matches.insert(
        1,
        CollectionAlbumMatchState {
            selected_album_uris: vec![album.matching.uri.clone()],
            cached_candidates: vec![album],
            ..CollectionAlbumMatchState::default()
        },
    );
    service.save(session.clone()).await.unwrap();
    state.spotify_membership.set_for_test(SpotifyLibraryState {
        account_id: "spotify".into(),
        complete: true,
        ..SpotifyLibraryState::default()
    });
    state
        .token_store
        .save(&Tokens {
            access: "access".into(),
            refresh: "refresh".into(),
            expires_at: u64::MAX,
            scopes: "user-library-modify".into(),
            playback_credentials: None,
        })
        .unwrap();
    let client = fake_client([Response::json(200, Value::Null)], "user-library-modify");
    let mut plan = apply_test_plan_for(&session, 1, &row.stable_id, "spotify:track:one");
    plan.lastfm_username = "user".into();
    plan.spotify_account_id = "spotify".into();
    plan.album.clear();
    plan.membership = ApplyMembership::Tracks(vec!["spotify:track:one".into()]);
    plan.metadata_uris = vec!["spotify:track:one".into()];

    {
        let mut membership = state.spotify_membership.lock().await;
        let library_owner = state.library.owner();
        run_apply_upstream_effect(
            &service,
            &mut membership,
            &library_owner,
            &state.cooldown_store,
            &client,
            &plan,
            crate::unix_now(),
        )
        .await
        .unwrap();
    }

    let requests = client.transport().requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].url.contains("/me/library"));
    assert!(!requests[0].url.contains("/tracks/"));
    let library = state.library.lock().unwrap();
    let track = library.tracks().first().unwrap();
    assert_eq!(track.uri, "spotify:track:one");
    assert_eq!(track.name, "One");
    assert_eq!(track.art, "Artist");
    assert_eq!(track.alb, "Album");
    assert_eq!(track.duration, Duration::from_secs(180));
    assert_eq!(track.release_date.as_deref(), Some("1994"));
}

#[tokio::test]
async fn application_seam_rejects_account_mismatch_before_provider_resolution() {
    let directory = tempfile::tempdir().unwrap();
    let (_, service, state) = test_app_state(directory.path(), Library::new(), &[]);
    let mut session = apply_test_session();
    session.lastfm_username = "user".into();
    session.cache_id = snapshot_cache_id("user", session.history_to);
    service.save(session.clone()).await.unwrap();
    state.spotify_membership.set_for_test(SpotifyLibraryState {
        account_id: "different-account".into(),
        complete: true,
        ..SpotifyLibraryState::default()
    });
    state
        .token_store
        .save(&Tokens {
            access: "access".into(),
            refresh: "refresh".into(),
            expires_at: u64::MAX,
            scopes: "user-library-modify".into(),
            playback_credentials: None,
        })
        .unwrap();
    let provider_calls = std::sync::atomic::AtomicUsize::new(0);
    let mut plan = apply_test_plan(&session);
    plan.membership = ApplyMembership::Album {
        uri: "spotify:album:one".into(),
        name: "Album".into(),
        artist: "Artist".into(),
    };
    let result = {
        let membership = state.spotify_membership.lock().await;
        application::UseCases::new(
            application::Owners {
                service: &state.lastfm_import,
                lastfm: &state.lastfm,
                membership: &state.spotify_membership,
                library: &state.library,
                settings: &state.settings,
                cooldown_store: &state.cooldown_store,
            },
            || {
                provider_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Err("provider should not be resolved".into())
            },
            || Ok(true),
        )
        .validate_apply_account(
            &membership,
            &plan,
            false,
            "The connected account changed while applying this review batch.",
        )
        .await
    };
    assert!(result.is_err());
    assert_eq!(provider_calls.load(std::sync::atomic::Ordering::Relaxed), 0);
}

#[tokio::test]
async fn later_queued_plan_uses_prior_queued_history_for_sum_and_highest_modes() {
    for (mode, expected) in [(CountMode::Sum, 5), (CountMode::Overwrite, 3)] {
        let directory = tempfile::tempdir().unwrap();
        let service = Service::new(directory.path());
        let mut session = apply_test_session_with_two_batches();
        session.default_count_mode = mode;
        for (id, track) in [("source", "Song"), ("source-two", "Song two")] {
            session.matches.insert(
                id.into(),
                MatchResult {
                    source_id: id.into(),
                    search_term: track.into(),
                    confidence: Some(Confidence::Exact),
                    selected_uri: Some("spotify:track:shared".into()),
                    candidates: Vec::new(),
                    track_matches: BTreeMap::from([(id.into(), "spotify:track:shared".into())]),
                },
            );
        }
        service.save(session.clone()).await.unwrap();
        let first_options = PageOptions {
            selected_track_ids: BTreeSet::from(["source".into()]),
            ..PageOptions::default()
        };
        let first = build_apply_plan(
            &session,
            "spotify-user",
            1,
            "Artist",
            "Album",
            &["source".into()],
            false,
            first_options,
        )
        .unwrap();
        service.enqueue_apply_plan(first, None).await.unwrap();
        let (_, sync) = service.snapshot_with_sync().await;
        let effective = effective_apply_session(&session, &sync.apply_queue);
        let second_options = PageOptions {
            selected_track_ids: BTreeSet::from(["source-two".into()]),
            ..PageOptions::default()
        };
        let second = build_apply_plan(
            &effective,
            "spotify-user",
            2,
            "Artist",
            "Album",
            &["source-two".into()],
            false,
            second_options,
        )
        .unwrap();
        assert_eq!(second.updates[0].play_count, Some(expected));
    }
}

#[tokio::test]
async fn failed_predecessor_retry_ignores_later_queued_shared_target_history() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    let mut session = apply_test_session_with_two_batches();
    for (id, track) in [("source", "Song"), ("source-two", "Song two")] {
        session.matches.insert(
            id.into(),
            MatchResult {
                source_id: id.into(),
                search_term: track.into(),
                confidence: Some(Confidence::Exact),
                selected_uri: Some("spotify:track:shared".into()),
                candidates: Vec::new(),
                track_matches: BTreeMap::from([(id.into(), "spotify:track:shared".into())]),
            },
        );
    }
    service.save(session.clone()).await.unwrap();
    let first_options = PageOptions {
        selected_track_ids: BTreeSet::from(["source".into()]),
        ..PageOptions::default()
    };
    let first = build_apply_plan(
        &session,
        "spotify-user",
        1,
        "Artist",
        "Album",
        &["source".into()],
        false,
        first_options,
    )
    .unwrap();
    service
        .enqueue_apply_plan(first.clone(), None)
        .await
        .unwrap();
    let (_, sync) = service.snapshot_with_sync().await;
    let second_session = effective_apply_session(&session, &sync.apply_queue);
    let second = build_apply_plan(
        &second_session,
        "spotify-user",
        2,
        "Artist",
        "Album",
        &["source-two".into()],
        false,
        PageOptions {
            selected_track_ids: BTreeSet::from(["source-two".into()]),
            ..PageOptions::default()
        },
    )
    .unwrap();
    service.enqueue_apply_plan(second, None).await.unwrap();
    service
        .claim_apply_job(&format!("{}:1", session.cache_id))
        .await
        .unwrap();
    service
        .fail_apply_job(&format!("{}:1", session.cache_id), "retry".into())
        .await
        .unwrap();
    let (_, sync) = service.snapshot_with_sync().await;
    let retry_session = effective_apply_session_for_job(
        &session,
        &sync.apply_queue,
        &format!("{}:1", session.cache_id),
    );
    let retry = build_apply_plan(
        &retry_session,
        "spotify-user",
        1,
        "Artist",
        "Album",
        &["source".into()],
        false,
        PageOptions {
            selected_track_ids: BTreeSet::from(["source".into()]),
            ..PageOptions::default()
        },
    )
    .unwrap();
    assert_eq!(retry, first);
}

#[tokio::test]
async fn apply_jobs_are_claimed_and_drained_in_persisted_queue_order() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    let session = apply_test_session_with_two_batches();
    service.save(session.clone()).await.unwrap();
    let first = apply_test_plan_for(&session, 1, "source", "spotify:track:one");
    let second = apply_test_plan_for(&session, 2, "source-two", "spotify:track:two");
    service.enqueue_apply_plan(first, None).await.unwrap();
    service.enqueue_apply_plan(second, None).await.unwrap();

    assert_eq!(service.next_apply_job().await.unwrap().plan.batch_id, 1);
    let claimed = service
        .claim_apply_job(&format!("{}:1", session.cache_id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.status, ApplyJobStatus::Running);
    assert_eq!(service.next_apply_job().await.unwrap().plan.batch_id, 1);

    service
        .remove_apply_job(&format!("{}:1", session.cache_id))
        .await
        .unwrap();
    assert_eq!(service.next_apply_job().await.unwrap().plan.batch_id, 2);
}

#[tokio::test]
async fn a_running_job_is_restartable_and_failure_at_each_stage_is_retryable() {
    for stage in [
        ApplyJobStage::Upstream,
        ApplyJobStage::Local,
        ApplyJobStage::Mappings,
        ApplyJobStage::Decision,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let service = Service::new(directory.path());
        let session = apply_test_session();
        service.save(session.clone()).await.unwrap();
        let plan = apply_test_plan(&session);
        service
            .enqueue_apply_plan(plan.clone(), None)
            .await
            .unwrap();
        service
            .claim_apply_job(&format!("{}:1", session.cache_id))
            .await
            .unwrap()
            .unwrap();
        service
            .mark_apply_stage(&format!("{}:1", session.cache_id), stage)
            .await
            .unwrap();

        let resumed = Service::new(directory.path());
        let running = resumed.next_apply_job().await.unwrap();
        assert_eq!(running.status, ApplyJobStatus::Running);
        assert_eq!(running.stage, stage);
        let frozen_plan = running.plan.clone();

        resumed
            .fail_apply_job(&running.id, format!("failed at {stage:?}"))
            .await
            .unwrap();
        let queue = resumed.queue_page(0, 10).await.unwrap();
        assert_eq!(queue.items[0].status, Some(QueueStatus::Failed));
        assert_eq!(queue.items[0].error, Some(format!("failed at {stage:?}")));

        resumed.enqueue_apply_plan(plan, None).await.unwrap();
        let retried = resumed
            .sync_snapshot()
            .await
            .apply_queue
            .into_iter()
            .find(|job| job.plan.batch_id == 1)
            .unwrap();
        assert_eq!(retried.status, ApplyJobStatus::Queued);
        assert_eq!(retried.stage, stage);
        assert_eq!(retried.plan, frozen_plan);
    }
}

#[tokio::test]
async fn decision_commit_keeps_the_job_until_the_final_removal() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    let session = apply_test_session();
    service.save(session.clone()).await.unwrap();
    let plan = apply_test_plan(&session);
    service
        .enqueue_apply_plan(plan.clone(), None)
        .await
        .unwrap();
    service
        .claim_apply_job(&format!("{}:1", session.cache_id))
        .await
        .unwrap()
        .unwrap();

    commit_apply_plan(&service, &plan).await.unwrap();
    assert_eq!(
        default_decision(&service.snapshot().await.unwrap(), "source").status,
        RowStatus::Done
    );
    assert_eq!(service.sync_snapshot().await.apply_queue.len(), 1);

    service
        .remove_apply_job(&format!("{}:1", session.cache_id))
        .await
        .unwrap();
    assert!(service.sync_snapshot().await.apply_queue.is_empty());
}

#[tokio::test]
async fn a_failed_decision_retry_reuses_the_frozen_plan_after_session_commit() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    let session = apply_test_session();
    service.save(session.clone()).await.unwrap();
    let plan = apply_test_plan(&session);
    service
        .enqueue_apply_plan(plan.clone(), None)
        .await
        .unwrap();
    let job_id = format!("{}:1", session.cache_id);
    service.claim_apply_job(&job_id).await.unwrap().unwrap();
    commit_apply_plan(&service, &plan).await.unwrap();
    service
        .fail_apply_job(&job_id, "remove failed".into())
        .await
        .unwrap();
    let failed = service
        .sync_snapshot()
        .await
        .apply_queue
        .into_iter()
        .find(|job| job.id == job_id)
        .unwrap();
    assert!(retry_plan_matches_request(
        &failed.plan,
        "spotify-user",
        1,
        "Artist",
        "Album",
        &["source".into()],
        false,
        &PageOptions::default(),
    ));
    service
        .enqueue_apply_plan(failed.plan.clone(), None)
        .await
        .unwrap();
    let retried = service.next_apply_job().await.unwrap();
    assert_eq!(retried.stage, failed.stage);
    let retried = service.claim_apply_job(&retried.id).await.unwrap().unwrap();
    execute_apply_job(&service, &retried, |_stage, _plan| {
        Box::pin(async { Ok(()) })
    })
    .await
    .unwrap();
    assert!(service.sync_snapshot().await.apply_queue.is_empty());
}

#[test]
fn legacy_sync_state_without_apply_fields_loads_an_empty_queue() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("lastfm-sync.json");
    let state = LastFmSyncState {
        version: LASTFM_SYNC_VERSION,
        ..LastFmSyncState::default()
    };
    let mut value = serde_json::to_value(state).unwrap();
    let object = value.as_object_mut().unwrap();
    object.remove("applyQueue");
    object.remove("acceptAll");
    fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();

    let loaded = IncrementalStore::new(directory.path()).load().unwrap();
    assert!(loaded.apply_queue.is_empty());
    assert!(loaded.accept_all.is_none());
}

#[tokio::test]
async fn legacy_failed_job_without_known_code_restarts_as_apply_failed() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(directory.path());
    let session = apply_test_session();
    service.save(session.clone()).await.unwrap();
    let plan = apply_test_plan(&session);
    service
        .enqueue_apply_plan(plan.clone(), None)
        .await
        .unwrap();
    let job_id = format!("{}:1", session.cache_id);
    service.claim_apply_job(&job_id).await.unwrap().unwrap();
    service
        .mark_apply_stage(&job_id, ApplyJobStage::Mappings)
        .await
        .unwrap();
    service
        .fail_apply_job_with(
            &job_id,
            ApplyFailure {
                code: ApplyFailureCode::SpotifyQuotaExhausted,
                message: "legacy display message".into(),
                endpoint_family: Some("/me/library".into()),
                retry_at: Some(9_999),
                ambiguous_outcome: false,
            },
        )
        .await
        .unwrap();

    let path = directory.path().join("lastfm-sync.json");
    let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["applyQueue"][0]
        .as_object_mut()
        .unwrap()
        .remove("errorCode");
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

    let restarted = Service::new(directory.path());
    let persisted = restarted.sync_snapshot().await.apply_queue.remove(0);
    assert_eq!(persisted.plan, plan);
    assert_eq!(persisted.stage, ApplyJobStage::Mappings);
    assert_eq!(persisted.attempt, 1);
    assert_eq!(persisted.error.as_deref(), Some("legacy display message"));
    assert_eq!(persisted.error_code, None);
    assert_eq!(persisted.retry_at, Some(9_999));
    let projected = restarted.queue_page(0, 10).await.unwrap().items.remove(0);
    assert_eq!(projected.error_code, Some(ApplyFailureCode::ApplyFailed));
    assert_eq!(projected.error.as_deref(), Some("legacy display message"));
    assert_eq!(projected.retry_at, Some(9_999));

    value["applyQueue"][0]["errorCode"] = Value::String("future-code".into());
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let future = Service::new(directory.path());
    let projected = future.queue_page(0, 10).await.unwrap().items.remove(0);
    assert_eq!(projected.error_code, Some(ApplyFailureCode::ApplyFailed));
    assert_eq!(projected.retry_at, Some(9_999));
}

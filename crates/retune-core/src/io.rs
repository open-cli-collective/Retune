//! Versioned serialization of the overlay library: export/import for
//! backup (`.json`), compressed export (`.json.gz`), restore, and merge.
//! Pure bytes-in/bytes-out — file paths and the filesystem live in the shell.

use std::io::{Read, Write};

use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use serde::Serialize;

use crate::model::Library;

/// Current schema version written by [`export_json`].
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("not a Retune library export (missing version envelope)")]
    MissingEnvelope,
    #[error("schema version {0} is newer than this app understands ({SCHEMA_VERSION})")]
    FromTheFuture(u32),
    #[error("gzip: {0}")]
    Gzip(std::io::Error),
    #[error("invalid library data: {0}")]
    Invalid(String),
}

/// Serializes as `{"version": 1, "library": { … }}`.
pub fn export_json(library: &Library) -> Vec<u8> {
    #[derive(Serialize)]
    struct Envelope<'a> {
        version: u32,
        library: &'a Library,
    }

    serde_json::to_vec(&Envelope {
        version: SCHEMA_VERSION,
        library,
    })
    .expect("Library is always serializable")
}

/// [`export_json`], gzip-compressed.
pub fn export_json_gz(library: &Library) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(&export_json(library))
        .expect("writing to memory cannot fail");
    encoder.finish().expect("writing to memory cannot fail")
}

/// Accepts the output of either export function — sniffs the gzip magic
/// bytes. Runs version migrations so any older schema still loads; a
/// fixture test pins that v1 files load forever.
pub fn import(bytes: &[u8]) -> Result<Library, ImportError> {
    let json = if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut json = Vec::new();
        GzDecoder::new(bytes)
            .read_to_end(&mut json)
            .map_err(ImportError::Gzip)?;
        json
    } else {
        bytes.to_vec()
    };
    let envelope: serde_json::Value = serde_json::from_slice(&json)?;
    let object = envelope.as_object().ok_or(ImportError::MissingEnvelope)?;
    let version = object
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or(ImportError::MissingEnvelope)?;
    if version > u64::from(SCHEMA_VERSION) {
        return Err(ImportError::FromTheFuture(
            u32::try_from(version).unwrap_or(u32::MAX),
        ));
    }
    if version != u64::from(SCHEMA_VERSION) {
        return Err(ImportError::MissingEnvelope);
    }
    let library = object
        .get("library")
        .cloned()
        .ok_or(ImportError::MissingEnvelope)?;
    let mut library: Library = serde_json::from_value(library)?;
    library.validate_imported().map_err(ImportError::Invalid)?;
    Ok(library)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::model::{AlbumKey, NewTrack, Rating, SourceId};

    fn library() -> Library {
        let mut library = Library::new();
        let id = library.add(NewTrack {
            uri: "spotify:track:one".into(),
            source: SourceId::Music,
            cat: "Rock".into(),
            art: "Artist".into(),
            alb: "Album".into(),
            name: "Track".into(),
            duration: Duration::from_secs(42),
            track_no: None,
            disc_no: None,
            added_at: None,
            release_date: None,
            kind: Some("Spotify".into()),
            bitrate_kbps: None,
        });
        library
            .click_track_star(id, Rating::new(5).unwrap())
            .unwrap();
        let track = &mut library.tracks_mut()[0];
        track.play_count = 3;
        track.last_played_at = Some(1_700_000_000);
        track.added_at = Some(1_600_000_000);
        track.kind = Some("Spotify".into());
        track.bitrate_kbps = Some(320);
        library.set_album_rating(
            AlbumKey::of(library.get(id).unwrap()),
            Some(Rating::new(4).unwrap()),
        );
        library
    }

    #[test]
    fn json_and_gzip_exports_round_trip_everything() {
        let library = library();
        assert_eq!(import(&export_json(&library)).unwrap(), library);
        assert_eq!(import(&export_json_gz(&library)).unwrap(), library);
    }

    #[test]
    fn corrupt_and_missing_envelopes_return_errors() {
        assert!(matches!(
            import(br#"{"version":1,"library":"#),
            Err(ImportError::Json(_))
        ));
        assert!(matches!(
            import(br#"{"library":{}}"#),
            Err(ImportError::MissingEnvelope)
        ));
        assert!(matches!(
            import(&[0x1f, 0x8b, 0]),
            Err(ImportError::Gzip(_))
        ));
    }

    #[test]
    fn imports_with_duplicate_ids_or_uris_are_rejected() {
        let dup = |id: u64, uri: &str| {
            format!(
                r#"{{"id":{id},"uri":"{uri}","source":"music","cat":"Rock","art":"A","alb":"B","name":"N","duration":{{"secs":1,"nanos":0}},"rating":null,"orig_cat":null}}"#
            )
        };
        let envelope = |tracks: &str| {
            format!(
                r#"{{"version":1,"library":{{"tracks":[{tracks}],"album_ratings":[],"next_id":0}}}}"#
            )
        };

        let same_id = envelope(&format!("{},{}", dup(1, "a"), dup(1, "b")));
        assert!(matches!(
            import(same_id.as_bytes()),
            Err(ImportError::Invalid(_))
        ));

        let same_uri = envelope(&format!("{},{}", dup(1, "a"), dup(2, "a")));
        assert!(matches!(
            import(same_uri.as_bytes()),
            Err(ImportError::Invalid(_))
        ));
    }

    #[test]
    fn imported_next_id_is_recomputed_from_tracks() {
        let hostile = format!(
            r#"{{"version":1,"library":{{"tracks":[{{"id":7,"uri":"a","source":"music","cat":"Rock","art":"A","alb":"B","name":"N","duration":{{"secs":1,"nanos":0}},"rating":null,"orig_cat":null}}],"album_ratings":[],"next_id":{}}}}}"#,
            u64::MAX
        );
        let mut library = import(hostile.as_bytes()).unwrap();
        let id = library.add(NewTrack {
            uri: "b".into(),
            source: SourceId::Music,
            cat: "Rock".into(),
            art: "A".into(),
            alb: "B".into(),
            name: "M".into(),
            duration: Duration::from_secs(1),
            track_no: None,
            disc_no: None,
            added_at: None,
            release_date: None,
            kind: None,
            bitrate_kbps: None,
        });
        assert_eq!(id.0, 8);
    }

    #[test]
    fn future_versions_are_rejected() {
        assert!(matches!(
            import(br#"{"version":999,"library":{}}"#),
            Err(ImportError::FromTheFuture(999))
        ));
    }

    #[test]
    fn handwritten_v1_envelope_loads() {
        let fixture = br#"{
            "version": 1,
            "library": {
                "tracks": [{
                    "id": 7,
                    "uri": "fixture:track",
                    "source": "music",
                    "cat": "Rock",
                    "art": "Artist",
                    "alb": "Album",
                    "name": "Fixture",
                    "duration": {"secs": 42, "nanos": 0},
                    "rating": 5,
                    "orig_cat": "Original"
                }],
                "album_ratings": [[
                    {"source": "music", "art": "Artist", "alb": "Album"},
                    4
                ]],
                "next_id": 8
            }
        }"#;
        let library = import(fixture).unwrap();
        let library = import(&export_json(&library)).unwrap();
        let track = &library.tracks()[0];
        assert_eq!(track.play_count, 0);
        assert_eq!(track.last_played_at, None);
        assert_eq!(track.added_at, None);
        assert_eq!(track.kind, None);
        assert_eq!(track.bitrate_kbps, None);

        assert_eq!(library.tracks()[0].id.0, 7);
        assert_eq!(library.tracks()[0].orig_cat.as_deref(), Some("Original"));
        assert_eq!(
            library.album_rating(&AlbumKey::of(&library.tracks()[0])),
            Rating::new(4)
        );
    }
}

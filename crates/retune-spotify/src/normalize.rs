use std::time::Duration;

use retune_core::model::{NewTrack, SourceId};

use crate::client::{Album, Artist, Audiobook, Chapter, Episode, Show, Track};

pub const UNCATEGORIZED: &str = "Uncategorized";

pub fn track(
    value: &Track,
    primary_artist: Option<&Artist>,
    parent_album: Option<&Album>,
) -> NewTrack {
    NewTrack {
        uri: value.uri.clone(),
        source: SourceId::Music,
        cat: primary_artist
            .and_then(|artist| artist.genres.first())
            .cloned()
            .unwrap_or_else(|| UNCATEGORIZED.into()),
        art: value
            .artists
            .first()
            .or_else(|| parent_album.and_then(|album| album.artists.first()))
            .map(|artist| artist.name.clone())
            .unwrap_or_default(),
        alb: value
            .album
            .as_ref()
            .map(|album| album.name.clone())
            .or_else(|| parent_album.map(|album| album.name.clone()))
            .unwrap_or_default(),
        name: value.name.clone(),
        duration: Duration::from_millis(value.duration_ms.unwrap_or_default()),
        track_no: value.track_number,
        disc_no: value.disc_number,
    }
}

pub fn episode(value: &Episode, parent_show: Option<&Show>) -> NewTrack {
    let show = value.show.as_ref().or(parent_show);
    NewTrack {
        uri: value.uri.clone(),
        source: SourceId::Podcasts,
        cat: show
            .and_then(|show| show.category.clone())
            .unwrap_or_else(|| UNCATEGORIZED.into()),
        art: show
            .map(|show| show.publisher.clone())
            .filter(|publisher| !publisher.is_empty())
            .unwrap_or_else(|| "Unknown".into()),
        alb: show.map(|show| show.name.clone()).unwrap_or_default(),
        name: value.name.clone(),
        duration: Duration::from_millis(value.duration_ms.unwrap_or_default()),
        track_no: None,
        disc_no: None,
    }
}

pub fn chapter(value: &Chapter, book: &Audiobook) -> NewTrack {
    NewTrack {
        uri: value.uri.clone(),
        source: SourceId::Audiobooks,
        cat: book
            .genres
            .first()
            .cloned()
            .unwrap_or_else(|| UNCATEGORIZED.into()),
        art: book
            .authors
            .first()
            .map(|author| author.name.clone())
            .unwrap_or_default(),
        alb: book.name.clone(),
        name: value.name.clone(),
        duration: Duration::from_millis(value.duration_ms.unwrap_or_default()),
        track_no: value.chapter_number,
        disc_no: None,
    }
}

#[cfg(test)]
mod tests {
    use crate::client::{Album, AlbumSummary, Author, SimplifiedArtist};

    use super::*;

    fn music() -> Track {
        Track {
            uri: "spotify:track:1".into(),
            name: "Song".into(),
            duration_ms: Some(1234),
            track_number: Some(2),
            disc_number: Some(1),
            artists: vec![SimplifiedArtist {
                id: "artist-1".into(),
                name: "Primary".into(),
            }],
            album: Some(AlbumSummary {
                id: "album-1".into(),
                uri: "spotify:album:1".into(),
                name: "Record".into(),
                images: vec![],
            }),
        }
    }

    #[test]
    fn track_uses_first_primary_artist_genre_and_unknown_fallback() {
        let artist = Artist {
            id: "artist-1".into(),
            name: "Primary".into(),
            genres: vec!["indie".into(), "rock".into()],
        };
        let mapped = track(&music(), Some(&artist), None);
        assert_eq!(mapped.uri, "spotify:track:1");
        assert_eq!(mapped.cat, "indie");
        assert_eq!(mapped.art, "Primary");
        assert_eq!(mapped.alb, "Record");
        assert_eq!((mapped.disc_no, mapped.track_no), (Some(1), Some(2)));
        assert_eq!(track(&music(), None, None).cat, UNCATEGORIZED);
        assert_eq!(
            track(
                &music(),
                Some(&Artist {
                    genres: vec![],
                    ..artist
                }),
                None,
            )
            .cat,
            UNCATEGORIZED
        );
    }

    #[test]
    fn simplified_album_track_uses_parent_album_metadata() {
        let value = Track {
            album: None,
            artists: vec![],
            ..music()
        };
        let album = Album {
            id: "album-1".into(),
            uri: "spotify:album:1".into(),
            name: "Parent Record".into(),
            artists: vec![SimplifiedArtist {
                id: "artist-1".into(),
                name: "Parent Artist".into(),
            }],
            images: vec![],
        };
        let mapped = track(&value, None, Some(&album));
        assert_eq!(mapped.alb, "Parent Record");
        assert_eq!(mapped.art, "Parent Artist");
    }

    #[test]
    fn episode_uses_show_facets_and_uncategorized_fallback() {
        let show = Show {
            id: "show-1".into(),
            uri: "spotify:show:1".into(),
            name: "The Show".into(),
            publisher: "Podcaster".into(),
            category: Some("Technology".into()),
            images: vec![],
        };
        let value = Episode {
            uri: "spotify:episode:1".into(),
            name: "Episode".into(),
            duration_ms: Some(2000),
            show: None,
        };
        let mapped = episode(&value, Some(&show));
        assert_eq!(mapped.cat, "Technology");
        assert_eq!(mapped.art, "Podcaster");
        assert_eq!(mapped.alb, "The Show");
        let uncategorized = episode(
            &value,
            Some(&Show {
                category: None,
                ..show
            }),
        );
        assert_eq!(uncategorized.cat, UNCATEGORIZED);
    }

    #[test]
    fn show_missing_publisher_decodes_and_normalizes_to_unknown() {
        // Live /me/shows payloads can omit publisher despite the documented shape.
        let show: Show = serde_json::from_value(serde_json::json!({
            "id": "show-1", "uri": "spotify:show:1", "name": "The Show", "category": null
        }))
        .unwrap();
        assert_eq!(show.publisher, "");
        let value = Episode {
            uri: "spotify:episode:1".into(),
            name: "Episode".into(),
            duration_ms: Some(2000),
            show: None,
        };
        assert_eq!(episode(&value, Some(&show)).art, "Unknown");
    }

    #[test]
    fn episode_with_null_duration_decodes_to_zero() {
        // Live /me/episodes payloads can carry duration_ms: null.
        let value: Episode = serde_json::from_value(serde_json::json!({
            "uri": "spotify:episode:9", "name": "Ep", "duration_ms": null, "show": null
        }))
        .unwrap();
        assert_eq!(value.duration_ms, None);
        assert_eq!(episode(&value, None).duration.as_millis(), 0);
    }

    #[test]
    fn chapter_uses_first_author_genre_and_fallbacks() {
        let value = Chapter {
            uri: "spotify:chapter:1".into(),
            name: "Chapter One".into(),
            duration_ms: Some(3000),
            chapter_number: Some(1),
        };
        let book = Audiobook {
            id: "book-1".into(),
            uri: "spotify:audiobook:1".into(),
            name: "The Book".into(),
            authors: vec![Author {
                name: "First Author".into(),
            }],
            genres: vec!["History".into()],
            images: vec![],
        };
        let mapped = chapter(&value, &book);
        assert_eq!(mapped.uri, "spotify:chapter:1");
        assert_eq!(mapped.cat, "History");
        assert_eq!(mapped.art, "First Author");
        assert_eq!(mapped.alb, "The Book");
        assert_eq!(mapped.track_no, Some(1));
        assert_eq!(
            chapter(
                &value,
                &Audiobook {
                    genres: vec![],
                    authors: vec![],
                    ..book
                }
            )
            .cat,
            UNCATEGORIZED
        );
    }
}

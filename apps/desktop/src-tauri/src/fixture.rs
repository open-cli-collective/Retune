use std::time::Duration;

use retune_core::model::{AlbumKey, Library, NewTrack, Rating, SourceId, TrackEdit};

pub fn library() -> Library {
    let mut library = Library::new();
    let music = [
        (
            "Rock",
            "The Eagles",
            "Hotel California",
            "Hotel California",
            390,
        ),
        (
            "Rock",
            "The Eagles",
            "Hotel California",
            "New Kid in Town",
            304,
        ),
        (
            "Rock",
            "The Eagles",
            "Hotel California",
            "Life in the Fast Lane",
            286,
        ),
        ("Rock", "The Eagles", "Hotel California", "Wasted Time", 295),
        ("Rock", "Fleetwood Mac", "Rumours", "Dreams", 254),
        ("Rock", "Fleetwood Mac", "Rumours", "Go Your Own Way", 218),
        ("Rock", "Fleetwood Mac", "Rumours", "The Chain", 268),
        (
            "Operatic Rock",
            "Queen",
            "A Night at the Opera",
            "Bohemian Rhapsody",
            355,
        ),
        (
            "Operatic Rock",
            "Queen",
            "A Night at the Opera",
            "You're My Best Friend",
            172,
        ),
        (
            "Operatic Rock",
            "Queen",
            "A Night at the Opera",
            "Love of My Life",
            219,
        ),
        (
            "Alternative",
            "Radiohead",
            "OK Computer",
            "Paranoid Android",
            383,
        ),
        (
            "Alternative",
            "Radiohead",
            "OK Computer",
            "Karma Police",
            261,
        ),
        (
            "Alternative",
            "Radiohead",
            "OK Computer",
            "No Surprises",
            228,
        ),
        ("Alternative", "The Strokes", "Is This It", "Last Nite", 197),
        ("Alternative", "The Strokes", "Is This It", "Someday", 185),
        ("Jazz", "Miles Davis", "Kind of Blue", "So What", 562),
        (
            "Jazz",
            "Miles Davis",
            "Kind of Blue",
            "Freddie Freeloader",
            586,
        ),
        ("Jazz", "Miles Davis", "Kind of Blue", "Blue in Green", 337),
        (
            "Jazz",
            "John Coltrane",
            "A Love Supreme",
            "Acknowledgement",
            463,
        ),
        ("Jazz", "John Coltrane", "A Love Supreme", "Resolution", 440),
        (
            "Classical",
            "Glenn Gould",
            "Goldberg Variations",
            "Aria",
            236,
        ),
        (
            "Classical",
            "Glenn Gould",
            "Goldberg Variations",
            "Variatio 5",
            72,
        ),
        (
            "Electronic",
            "Boards of Canada",
            "Music Has the Right to Children",
            "Roygbiv",
            151,
        ),
        ("Electronic", "Daft Punk", "Discovery", "One More Time", 320),
        ("Electronic", "Daft Punk", "Discovery", "Digital Love", 298),
        (
            "Electronic",
            "Daft Punk",
            "Discovery",
            "Harder Better Faster Stronger",
            225,
        ),
    ];
    for (index, (cat, art, alb, name, seconds)) in music.into_iter().enumerate() {
        let id = add(
            &mut library,
            SourceId::Music,
            index,
            (cat, art, alb, name, seconds),
        );
        if art == "Queen" {
            library
                .edit(
                    id,
                    TrackEdit {
                        cat: Some("Rock".into()),
                        ..TrackEdit::default()
                    },
                )
                .expect("fixture track exists");
        }
        if name == "Life in the Fast Lane" {
            library
                .click_track_star(id, Rating::new(5).unwrap())
                .expect("fixture track exists");
        }
    }

    library.set_album_rating(
        AlbumKey {
            source: SourceId::Music,
            art: "The Eagles".into(),
            alb: "Hotel California".into(),
        },
        Rating::new(4),
    );

    let podcasts = [
        (
            "Technology",
            "Acquired",
            "S14 · Semiconductors",
            "The Foundry Model",
            2_892,
        ),
        (
            "Technology",
            "Acquired",
            "S14 · Semiconductors",
            "ASML & EUV",
            3_160,
        ),
        (
            "Technology",
            "Lex Fridman",
            "AI Frontiers",
            "On Interpretability",
            9_663,
        ),
        (
            "Technology",
            "Lex Fridman",
            "AI Frontiers",
            "On AI Safety",
            8_734,
        ),
    ];
    for (index, (cat, art, alb, name, seconds)) in podcasts.into_iter().enumerate() {
        add(
            &mut library,
            SourceId::Podcasts,
            index,
            (cat, art, alb, name, seconds),
        );
    }
    let audiobooks = [
        (
            "Fiction",
            "Ursula K. Le Guin",
            "The Left Hand of Darkness",
            "Ch. 1 — A Parade in Erhenrang",
            1_694,
        ),
        (
            "Fiction",
            "Ursula K. Le Guin",
            "The Left Hand of Darkness",
            "Ch. 2 — The Place Inside the Blizzard",
            1_910,
        ),
        (
            "Fiction",
            "Ursula K. Le Guin",
            "The Left Hand of Darkness",
            "Ch. 3 — The Mad King",
            1_746,
        ),
    ];
    for (index, (cat, art, alb, name, seconds)) in audiobooks.into_iter().enumerate() {
        add(
            &mut library,
            SourceId::Audiobooks,
            index,
            (cat, art, alb, name, seconds),
        );
    }
    library
}

fn add(
    library: &mut Library,
    source: SourceId,
    index: usize,
    (cat, art, alb, name, seconds): (&str, &str, &str, &str, u64),
) -> retune_core::model::TrackId {
    library.add(NewTrack {
        uri: format!(
            "fixture:{}:{index}",
            match source {
                SourceId::Music => "track",
                SourceId::Podcasts => "episode",
                SourceId::Audiobooks => "chapter",
            }
        ),
        source,
        cat: cat.into(),
        art: art.into(),
        alb: alb.into(),
        name: name.into(),
        duration: Duration::from_secs(seconds),
        track_no: None,
        disc_no: None,
        added_at: None,
        kind: None,
        bitrate_kbps: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_has_expected_counts_and_edits() {
        let library = library();
        assert_eq!(library.tracks().len(), 33);
        assert_eq!(
            library
                .tracks()
                .iter()
                .filter(|track| track.source == SourceId::Music)
                .count(),
            26
        );
        assert_eq!(
            library
                .tracks()
                .iter()
                .filter(|track| track.orig_cat.is_some())
                .count(),
            3
        );
        assert_eq!(
            library
                .tracks()
                .iter()
                .filter(|track| track.rating.is_some())
                .count(),
            1
        );
    }
}

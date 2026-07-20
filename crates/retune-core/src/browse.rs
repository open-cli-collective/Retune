//! The three-column progressive-filter projection (Genre | Artist | Album →
//! track list). One *possible* view of a [`Library`]; alternate layouts are
//! sibling modules consuming the same records.

use crate::model::{Library, SourceId, TrackRecord};

/// The user's current browse position within one source. Invariant, enforced
/// by the transition methods: a facet can only be set when every broader
/// facet's implications are respected — selecting a broader level resets the
/// narrower ones; selecting a narrower level never clears a broader one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    cat: Option<String>,
    art: Option<String>,
    alb: Option<String>,
}

impl Selection {
    pub fn cat(&self) -> Option<&str> {
        self.cat.as_deref()
    }

    pub fn art(&self) -> Option<&str> {
        self.art.as_deref()
    }

    pub fn alb(&self) -> Option<&str> {
        self.alb.as_deref()
    }

    /// Select a category (`None` = the "All" row). Resets artist and album.
    pub fn select_cat(&mut self, cat: Option<String>) {
        self.cat = cat;
        self.art = None;
        self.alb = None;
    }

    /// Select an artist (`None` = "All"). Resets album, keeps category.
    pub fn select_art(&mut self, art: Option<String>) {
        self.art = art;
        self.alb = None;
    }

    /// Select an album (`None` = "All"). Keeps category and artist.
    pub fn select_alb(&mut self, alb: Option<String>) {
        self.alb = alb;
    }
}

/// The three facet columns for the current selection: every column reflects
/// the *broader* selections (artists are limited to the chosen category;
/// albums to the chosen category + artist), with counts for the "All (N …)"
/// header rows. Lists are sorted, case-insensitively, and deduplicated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facets {
    pub cats: Vec<String>,
    pub arts: Vec<String>,
    pub albs: Vec<String>,
}

pub fn facets(library: &Library, source: SourceId, selection: &Selection) -> Facets {
    let records = library
        .tracks()
        .iter()
        .filter(|track| track.source == source);
    let cats = sorted_unique(records.clone().map(|track| track.cat.clone()).collect());
    let arts = sorted_unique(
        records
            .clone()
            .filter(|track| selected(&selection.cat, &track.cat))
            .map(|track| track.art.clone())
            .collect(),
    );
    let albs = sorted_unique(
        records
            .filter(|track| {
                selected(&selection.cat, &track.cat) && selected(&selection.art, &track.art)
            })
            .map(|track| track.alb.clone())
            .collect(),
    );
    Facets { cats, arts, albs }
}

/// The track list for the current intersection of selections, in stable
/// browse order: artist, album, disc, track, then library insertion order.
pub fn tracks<'a>(
    library: &'a Library,
    source: SourceId,
    selection: &Selection,
) -> Vec<&'a TrackRecord> {
    let mut tracks: Vec<_> = library
        .tracks()
        .iter()
        .filter(|track| {
            track.source == source
                && selected(&selection.cat, &track.cat)
                && selected(&selection.art, &track.art)
                && selected(&selection.alb, &track.alb)
        })
        .collect();
    tracks.sort_by(|left, right| {
        left.art
            .to_lowercase()
            .cmp(&right.art.to_lowercase())
            .then_with(|| left.alb.to_lowercase().cmp(&right.alb.to_lowercase()))
            .then_with(|| left.disc_no.is_none().cmp(&right.disc_no.is_none()))
            .then_with(|| left.disc_no.cmp(&right.disc_no))
            .then_with(|| left.track_no.is_none().cmp(&right.track_no.is_none()))
            .then_with(|| left.track_no.cmp(&right.track_no))
    });
    tracks
}

fn selected(selection: &Option<String>, value: &str) -> bool {
    selection
        .as_deref()
        .is_none_or(|selected| selected == value)
}

fn sorted_unique(mut values: Vec<String>) -> Vec<String> {
    values.sort_by(|left, right| {
        left.to_lowercase()
            .cmp(&right.to_lowercase())
            .then_with(|| left.cmp(right))
    });
    values.dedup();
    values
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::model::NewTrack;

    fn add(library: &mut Library, source: SourceId, uri: &str, cat: &str, art: &str, alb: &str) {
        library.add(NewTrack {
            uri: uri.into(),
            source,
            cat: cat.into(),
            art: art.into(),
            alb: alb.into(),
            name: uri.into(),
            duration: Duration::ZERO,
            track_no: None,
            disc_no: None,
        });
    }

    fn library() -> Library {
        let mut library = Library::new();
        add(&mut library, SourceId::Music, "1", "Rock", "beta", "Zoo");
        add(&mut library, SourceId::Music, "2", "Jazz", "Alpha", "First");
        add(
            &mut library,
            SourceId::Music,
            "3",
            "Rock",
            "alpha",
            "Second",
        );
        add(&mut library, SourceId::Music, "4", "Rock", "beta", "Able");
        add(&mut library, SourceId::Music, "5", "Rock", "beta", "Able");
        add(
            &mut library,
            SourceId::Podcasts,
            "podcast",
            "Audio",
            "Network",
            "Show",
        );
        library
    }

    #[test]
    fn selection_transitions_reset_only_narrower_facets() {
        let mut selection = Selection::default();
        selection.select_cat(Some("Rock".into()));
        selection.select_art(Some("Artist".into()));
        selection.select_alb(Some("Album".into()));
        assert_eq!(selection.cat(), Some("Rock"));
        assert_eq!(selection.art(), Some("Artist"));
        assert_eq!(selection.alb(), Some("Album"));

        selection.select_art(None);
        assert_eq!(selection.cat(), Some("Rock"));
        assert_eq!(selection.art(), None);
        assert_eq!(selection.alb(), None);

        selection.select_alb(Some("Album".into()));
        selection.select_cat(None);
        assert_eq!(selection.cat(), None);
        assert_eq!(selection.art(), None);
        assert_eq!(selection.alb(), None);
    }

    #[test]
    fn facets_use_only_broader_selections_and_the_requested_source() {
        let library = library();
        let mut selection = Selection::default();
        selection.select_cat(Some("Rock".into()));
        selection.select_art(Some("beta".into()));
        selection.select_alb(Some("Zoo".into()));

        assert_eq!(
            facets(&library, SourceId::Music, &selection),
            Facets {
                cats: vec!["Jazz".into(), "Rock".into()],
                arts: vec!["alpha".into(), "beta".into()],
                albs: vec!["Able".into(), "Zoo".into()],
            }
        );
    }

    #[test]
    fn tracks_intersect_selections_and_sort_artist_album_then_insertion() {
        let library = library();
        let mut selection = Selection::default();
        selection.select_cat(Some("Rock".into()));

        let uris: Vec<_> = tracks(&library, SourceId::Music, &selection)
            .into_iter()
            .map(|track| track.uri.as_str())
            .collect();
        assert_eq!(uris, ["3", "4", "5", "1"]);

        selection.select_art(Some("beta".into()));
        selection.select_alb(Some("Able".into()));
        let uris: Vec<_> = tracks(&library, SourceId::Music, &selection)
            .into_iter()
            .map(|track| track.uri.as_str())
            .collect();
        assert_eq!(uris, ["4", "5"]);
    }

    #[test]
    fn tracks_sort_by_disc_and_track_with_missing_numbers_last() {
        let mut library = Library::new();
        for (uri, disc_no, track_no) in [
            ("missing", None, None),
            ("d2t1", Some(2), Some(1)),
            ("d1t2", Some(1), Some(2)),
            ("d1t1", Some(1), Some(1)),
        ] {
            library.add(NewTrack {
                uri: uri.into(),
                source: SourceId::Music,
                cat: "Rock".into(),
                art: "Artist".into(),
                alb: "Album".into(),
                name: uri.into(),
                duration: Duration::ZERO,
                track_no,
                disc_no,
            });
        }

        let uris: Vec<_> = tracks(&library, SourceId::Music, &Selection::default())
            .into_iter()
            .map(|track| track.uri.as_str())
            .collect();
        assert_eq!(uris, ["d1t1", "d1t2", "d2t1", "missing"]);
    }
}

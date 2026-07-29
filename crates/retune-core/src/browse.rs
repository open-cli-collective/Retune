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
    cat: Vec<String>,
    art: Vec<String>,
    alb: Vec<String>,
}

impl Selection {
    pub fn cat(&self) -> &[String] {
        &self.cat
    }

    pub fn art(&self) -> &[String] {
        &self.art
    }

    pub fn alb(&self) -> &[String] {
        &self.alb
    }

    /// Select categories (empty = the "All" row). Resets artist and album.
    pub fn select_cat(&mut self, cat: Vec<String>) {
        self.cat = cat;
        self.art.clear();
        self.alb.clear();
    }

    /// Select artists (empty = "All"). Resets album, keeps category.
    pub fn select_art(&mut self, art: Vec<String>) {
        self.art = art;
        self.alb.clear();
    }

    /// Select albums (empty = "All"). Keeps category and artist.
    pub fn select_alb(&mut self, alb: Vec<String>) {
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

/// The track list for the current intersection of selections. The narrowest
/// selected facet keeps selection order; each group then uses stable browse
/// order: artist, album, disc, track, then library insertion order.
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
        selection_rank(selection, left)
            .cmp(&selection_rank(selection, right))
            .then_with(|| left.art.to_lowercase().cmp(&right.art.to_lowercase()))
            .then_with(|| left.alb.to_lowercase().cmp(&right.alb.to_lowercase()))
            .then_with(|| left.disc_no.unwrap_or(1).cmp(&right.disc_no.unwrap_or(1)))
            .then_with(|| left.track_no.is_none().cmp(&right.track_no.is_none()))
            .then_with(|| left.track_no.cmp(&right.track_no))
    });
    tracks
}

fn selection_rank(selection: &Selection, track: &TrackRecord) -> usize {
    let (values, value) = if !selection.alb.is_empty() {
        (&selection.alb, &track.alb)
    } else if !selection.art.is_empty() {
        (&selection.art, &track.art)
    } else {
        (&selection.cat, &track.cat)
    };
    values
        .iter()
        .position(|selected| selected == value)
        .unwrap_or_default()
}

fn selected(selection: &[String], value: &str) -> bool {
    selection.is_empty() || selection.iter().any(|selected| selected == value)
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
            added_at: None,
            release_date: None,
            kind: None,
            bitrate_kbps: None,
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
        selection.select_cat(vec!["Rock".into()]);
        selection.select_art(vec!["Artist".into()]);
        selection.select_alb(vec!["Album".into()]);
        assert_eq!(selection.cat(), ["Rock"]);
        assert_eq!(selection.art(), ["Artist"]);
        assert_eq!(selection.alb(), ["Album"]);

        selection.select_art(vec![]);
        assert_eq!(selection.cat(), ["Rock"]);
        assert!(selection.art().is_empty());
        assert!(selection.alb().is_empty());

        selection.select_alb(vec!["Album".into()]);
        selection.select_cat(vec![]);
        assert!(selection.cat().is_empty());
        assert!(selection.art().is_empty());
        assert!(selection.alb().is_empty());
    }

    #[test]
    fn facets_use_only_broader_selections_and_the_requested_source() {
        let library = library();
        let mut selection = Selection::default();
        selection.select_cat(vec!["Rock".into()]);
        selection.select_art(vec!["beta".into()]);
        selection.select_alb(vec!["Zoo".into()]);

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
        selection.select_cat(vec!["Rock".into()]);

        let uris: Vec<_> = tracks(&library, SourceId::Music, &selection)
            .into_iter()
            .map(|track| track.uri.as_str())
            .collect();
        assert_eq!(uris, ["3", "4", "5", "1"]);

        selection.select_art(vec!["beta".into()]);
        selection.select_alb(vec!["Able".into()]);
        let uris: Vec<_> = tracks(&library, SourceId::Music, &selection)
            .into_iter()
            .map(|track| track.uri.as_str())
            .collect();
        assert_eq!(uris, ["4", "5"]);
    }

    #[test]
    fn multiple_categories_union_artist_facets_and_tracks() {
        let library = library();
        let mut selection = Selection::default();
        selection.select_cat(vec!["Rock".into(), "Jazz".into()]);

        assert_eq!(
            facets(&library, SourceId::Music, &selection).arts,
            ["Alpha", "alpha", "beta"]
        );
        let uris: Vec<_> = tracks(&library, SourceId::Music, &selection)
            .into_iter()
            .map(|track| track.uri.as_str())
            .collect();
        assert_eq!(uris, ["3", "4", "5", "1", "2"]);
    }

    #[test]
    fn most_specific_facet_preserves_selection_order() {
        let library = library();
        let mut selection = Selection::default();
        selection.select_cat(vec!["Rock".into()]);
        selection.select_art(vec!["beta".into(), "alpha".into()]);

        let uris = tracks(&library, SourceId::Music, &selection)
            .into_iter()
            .map(|track| track.uri.as_str())
            .collect::<Vec<_>>();
        assert_eq!(uris, ["4", "5", "1", "3"]);

        selection.select_alb(vec!["Zoo".into(), "Able".into()]);
        let uris = tracks(&library, SourceId::Music, &selection)
            .into_iter()
            .map(|track| track.uri.as_str())
            .collect::<Vec<_>>();
        assert_eq!(uris, ["1", "4", "5"]);
    }

    #[test]
    fn selecting_narrower_facets_keeps_broader_multi_values() {
        let mut selection = Selection::default();
        selection.select_cat(vec!["Jazz".into(), "Rock".into()]);
        selection.select_art(vec!["Alpha".into(), "beta".into()]);
        selection.select_alb(vec!["First".into(), "Zoo".into()]);

        assert_eq!(selection.cat(), ["Jazz", "Rock"]);
        assert_eq!(selection.art(), ["Alpha", "beta"]);
        assert_eq!(selection.alb(), ["First", "Zoo"]);
    }

    #[test]
    fn tracks_sort_by_disc_and_track_with_missing_disc_treated_as_one() {
        let mut library = Library::new();
        for (uri, disc_no, track_no) in [
            ("missing", None, None),
            ("d2t1", Some(2), Some(1)),
            ("d1t2", Some(1), Some(2)),
            ("d1t1", Some(1), Some(1)),
            ("implicit-d1t3", None, Some(3)),
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
                added_at: None,
                release_date: None,
                kind: None,
                bitrate_kbps: None,
            });
        }

        let uris: Vec<_> = tracks(&library, SourceId::Music, &Selection::default())
            .into_iter()
            .map(|track| track.uri.as_str())
            .collect();
        assert_eq!(uris, ["d1t1", "d1t2", "implicit-d1t3", "d2t1", "missing"]);
    }
}

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
    pub fn select_cat(&mut self, _cat: Option<String>) {
        todo!()
    }

    /// Select an artist (`None` = "All"). Resets album, keeps category.
    pub fn select_art(&mut self, _art: Option<String>) {
        todo!()
    }

    /// Select an album (`None` = "All"). Keeps category and artist.
    pub fn select_alb(&mut self, _alb: Option<String>) {
        todo!()
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

pub fn facets(_library: &Library, _source: SourceId, _selection: &Selection) -> Facets {
    todo!()
}

/// The track list for the current intersection of selections, in stable
/// browse order: artist, then album, then library insertion order (proxy for
/// track number until providers supply one).
pub fn tracks<'a>(
    _library: &'a Library,
    _source: SourceId,
    _selection: &Selection,
) -> Vec<&'a TrackRecord> {
    todo!()
}

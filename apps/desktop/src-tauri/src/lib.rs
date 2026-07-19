mod fixture;

use std::{collections::BTreeSet, sync::Mutex};

use retune_core::{
    browse::{self, Selection},
    model::{AlbumKey, EffectiveRating, Library, Rating, SourceId, TrackId},
};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct SelectionDto {
    cat: Option<String>,
    art: Option<String>,
    alb: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowseView {
    facets: FacetsView,
    tracks: Vec<TrackView>,
    album_rating: Option<u8>,
    counts: CountsView,
}

#[derive(Serialize)]
struct FacetsView {
    cats: Vec<String>,
    arts: Vec<String>,
    albs: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TrackView {
    id: u64,
    name: String,
    art: String,
    alb: String,
    cat: String,
    duration_secs: u64,
    overridden: bool,
    rating: Option<RatingView>,
}

#[derive(Serialize)]
struct RatingView {
    stars: u8,
    explicit: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CountsView {
    tracks: usize,
    total_secs: u64,
    overlay_edits: usize,
    per_source: PerSourceView,
}

#[derive(Serialize)]
struct PerSourceView {
    music: usize,
    podcasts: usize,
    audiobooks: usize,
}

#[tauri::command]
fn browse(
    library: tauri::State<'_, Mutex<Library>>,
    source: SourceId,
    sel: SelectionDto,
    query: Option<String>,
) -> BrowseView {
    let library = library.lock().expect("library mutex poisoned");
    let mut selection = Selection::default();
    selection.select_cat(sel.cat);
    selection.select_art(sel.art);
    selection.select_alb(sel.alb);

    let facet_view = browse::facets(&library, source, &selection);
    let query = query.unwrap_or_default().trim().to_lowercase();
    let selected_tracks = browse::tracks(&library, source, &selection);
    let album_rating = selection.alb().and_then(|alb| {
        let art = selection
            .art()
            .or_else(|| selected_tracks.first().map(|track| track.art.as_str()))?;
        library.album_rating(&AlbumKey {
            source,
            art: art.into(),
            alb: alb.into(),
        })
    });
    let tracks = selected_tracks
        .into_iter()
        .filter(|track| {
            query.is_empty()
                || [&track.name, &track.art, &track.alb, &track.cat]
                    .iter()
                    .any(|value| value.to_lowercase().contains(&query))
        })
        .map(|track| TrackView {
            id: track.id.0,
            name: track.name.clone(),
            art: track.art.clone(),
            alb: track.alb.clone(),
            cat: track.cat.clone(),
            duration_secs: track.duration.as_secs(),
            overridden: track
                .orig_cat
                .as_ref()
                .is_some_and(|original| original != &track.cat),
            rating: library
                .effective_rating(track.id)
                .map(|rating| match rating {
                    EffectiveRating::Explicit(rating) => RatingView {
                        stars: rating.stars(),
                        explicit: true,
                    },
                    EffectiveRating::Inherited(rating) => RatingView {
                        stars: rating.stars(),
                        explicit: false,
                    },
                }),
        })
        .collect();

    BrowseView {
        facets: FacetsView {
            cats: facet_view.cats,
            arts: facet_view.arts,
            albs: facet_view.albs,
        },
        tracks,
        album_rating: album_rating.map(Rating::stars),
        counts: counts(&library, source, &selection, &query),
    }
}

fn counts(library: &Library, source: SourceId, selection: &Selection, query: &str) -> CountsView {
    let visible = browse::tracks(library, source, selection)
        .into_iter()
        .filter(|track| {
            query.is_empty()
                || [&track.name, &track.art, &track.alb, &track.cat]
                    .iter()
                    .any(|value| value.to_lowercase().contains(query))
        })
        .collect::<Vec<_>>();
    let source_count = |source| {
        library
            .tracks()
            .iter()
            .filter(|track| track.source == source)
            .count()
    };
    let albums = library
        .tracks()
        .iter()
        .map(AlbumKey::of)
        .collect::<BTreeSet<_>>();
    let overlay_edits = library
        .tracks()
        .iter()
        .filter(|track| track.rating.is_some())
        .count()
        + library
            .tracks()
            .iter()
            .filter(|track| {
                track
                    .orig_cat
                    .as_ref()
                    .is_some_and(|original| original != &track.cat)
            })
            .count()
        + albums
            .iter()
            .filter(|album| library.album_rating(album).is_some())
            .count();

    CountsView {
        tracks: visible.len(),
        total_secs: visible.iter().map(|track| track.duration.as_secs()).sum(),
        overlay_edits,
        per_source: PerSourceView {
            music: source_count(SourceId::Music),
            podcasts: source_count(SourceId::Podcasts),
            audiobooks: source_count(SourceId::Audiobooks),
        },
    }
}

#[tauri::command]
fn click_track_star(
    library: tauri::State<'_, Mutex<Library>>,
    id: u64,
    stars: u8,
) -> Result<(), String> {
    let rating = Rating::new(stars).ok_or_else(|| "rating must be 1 through 5".to_string())?;
    library
        .lock()
        .expect("library mutex poisoned")
        .click_track_star(TrackId(id), rating)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn set_album_rating(
    library: tauri::State<'_, Mutex<Library>>,
    source: SourceId,
    art: String,
    alb: String,
    stars: Option<u8>,
) -> Result<(), String> {
    let rating = stars
        .map(|stars| Rating::new(stars).ok_or_else(|| "rating must be 1 through 5".to_string()))
        .transpose()?;
    library
        .lock()
        .expect("library mutex poisoned")
        .set_album_rating(AlbumKey { source, art, alb }, rating);
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(Mutex::new(fixture::library()))
        .invoke_handler(tauri::generate_handler![
            browse,
            click_track_star,
            set_album_rating
        ])
        .setup(|app| {
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_counts_cover_visible_tracks_and_global_overlay_edits() {
        let library = fixture::library();
        let all = counts(&library, SourceId::Music, &Selection::default(), "");
        assert_eq!(all.tracks, 26);
        assert_eq!(all.per_source.music, 26);
        assert_eq!(all.per_source.podcasts, 4);
        assert_eq!(all.per_source.audiobooks, 3);
        assert_eq!(all.overlay_edits, 5);

        let filtered = counts(&library, SourceId::Music, &Selection::default(), "bohemian");
        assert_eq!(filtered.tracks, 1);
    }
}

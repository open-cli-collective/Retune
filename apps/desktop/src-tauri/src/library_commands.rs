use super::*;

#[tauri::command]
pub(super) fn browse(
    state: tauri::State<'_, AppState>,
    source: SourceId,
    sel: SelectionDto,
    query: Option<String>,
) -> BrowseView {
    let library = state.library.lock().expect("library mutex poisoned");
    let mut selection = Selection::default();
    selection.select_cat(sel.cat);
    selection.select_art(sel.art);
    selection.select_alb(sel.alb);

    let facet_view = browse::facets(&library, source, &selection);
    let query = query.unwrap_or_default().trim().to_lowercase();
    let selected_tracks = browse::tracks(&library, source, &selection)
        .into_iter()
        .filter(|track| {
            query.is_empty()
                || [&track.name, &track.art, &track.alb, &track.cat]
                    .iter()
                    .any(|value| value.to_lowercase().contains(&query))
        })
        .collect::<Vec<_>>();
    let (album_rating, album_rating_artist, album_rating_ambiguous) =
        album_rating_view(&library, &selection, &selected_tracks);
    let tracks = selected_tracks
        .into_iter()
        .map(|track| TrackView::from_track(track, library.effective_rating(track.id)))
        .collect();

    BrowseView {
        facets: FacetsView {
            cats: facet_view.cats,
            arts: facet_view.arts,
            albs: facet_view.albs,
        },
        tracks,
        album_rating,
        album_rating_artist,
        album_rating_ambiguous,
        counts: counts(&library, source, &selection, &query),
    }
}

#[tauri::command]
pub(super) fn metadata_values(state: tauri::State<'_, AppState>) -> MetadataValues {
    let library = state.library.lock().expect("library mutex poisoned");
    collect_metadata_values(&library)
}

#[tauri::command]
pub(super) fn click_track_star(
    state: tauri::State<'_, AppState>,
    id: u64,
    stars: u8,
) -> Result<(), String> {
    let rating = Rating::new(stars).ok_or_else(|| "rating must be 1 through 5".to_string())?;
    mutate_library(&state, |library| {
        library
            .click_track_star(TrackId(id), rating)
            .map_err(|error| error.to_string())
    })
}

#[tauri::command]
pub(super) fn set_track_enabled(
    state: tauri::State<'_, AppState>,
    id: u64,
    enabled: bool,
) -> Result<(), String> {
    mutate_library(&state, |library| {
        library
            .set_track_enabled(TrackId(id), enabled)
            .map_err(|error| error.to_string())
    })
}

#[tauri::command]
pub(super) fn set_album_rating(
    state: tauri::State<'_, AppState>,
    source: SourceId,
    art: String,
    alb: String,
    stars: Option<u8>,
) -> Result<(), String> {
    let rating = stars
        .map(|stars| Rating::new(stars).ok_or_else(|| "rating must be 1 through 5".to_string()))
        .transpose()?;
    mutate_library(&state, |library| {
        library.set_album_rating(AlbumKey { source, art, alb }, rating);
        Ok(())
    })
}

#[tauri::command]
pub(super) fn get_track(
    state: tauri::State<'_, AppState>,
    id: u64,
) -> Result<TrackInfoView, String> {
    let library = state.library.lock().expect("library mutex poisoned");
    let track = library
        .get(TrackId(id))
        .ok_or_else(|| format!("unknown track id {id}"))?;
    Ok(TrackInfoView::from_track(&library, track))
}

#[tauri::command]
pub(super) fn edit_track(
    state: tauri::State<'_, AppState>,
    id: u64,
    edit: TrackEditDto,
) -> Result<(), String> {
    let rating_change = rating_change(&edit)?;
    mutate_library(&state, |library| {
        apply_track_info(library, id, &edit, rating_change)
    })
}

#[tauri::command]
pub(super) fn set_track_infos(
    app: tauri::AppHandle,
    ids: Vec<u64>,
    edit: TrackEditDto,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    mutate_library(&state, |library| apply_track_infos(library, &ids, &edit))?;
    app.emit("library-changed", ())
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub(super) fn import_local(app: tauri::AppHandle, paths: Vec<String>) {
    launch_local_import(app, paths.into_iter().map(PathBuf::from).collect());
}

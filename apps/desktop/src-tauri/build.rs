const APP_COMMANDS: &[&str] = &[
    "browse",
    "metadata_values",
    "genre_values",
    "click_track_star",
    "set_track_enabled",
    "set_album_rating",
    "get_track",
    "edit_track",
    "set_track_infos",
    "subscribe_main_events",
    "unsubscribe_main_events",
    "get_settings",
    "get_appearance",
    "update_settings",
    "connection_state",
    "connect_spotify",
    "authorize_spotify_playback",
    "disconnect_spotify",
    "sync_from_spotify",
    "spotify_search",
    "spotify_album_page",
    "spotify_artist_page",
    "spotify_follow_artist",
    "spotify_artist_albums",
    "add_spotify_album",
    "remove_spotify_album",
    "add_spotify_track",
    "add_spotify_tracks",
    "remove_spotify_track",
    "playlists_list",
    "open_spotify_playlist",
    "resolve_spotify_track_destination",
    "reorder_playlists",
    "playlist_unfollow",
    "playlist_create",
    "playlist_tracks",
    "playlist_add",
    "playlist_add_album",
    "playlist_reorder",
    "playlist_remove",
    "play_tracks",
    "player_toggle",
    "player_next",
    "player_prev",
    "player_seek",
    "player_set_volume",
    "set_repeat",
    "set_shuffle",
    "track_artwork",
    "lastfm_state",
    "connect_lastfm",
    "finish_lastfm",
    "disconnect_lastfm",
    "open_lastfm_importer",
    "lastfm_import_state",
    "lastfm_import_queue",
    "lastfm_import_page",
    "start_lastfm_import",
    "sync_lastfm_plays",
    "lastfm_import_review",
    "lastfm_import_options",
    "lastfm_import_count_mode",
    "lastfm_import_search_terms",
    "lastfm_import_select_match",
    "lastfm_import_select_matches",
    "lastfm_import_collection_search_albums",
    "lastfm_import_collection_preview_album",
    "lastfm_import_collection_add_album",
    "lastfm_import_collection_remove_album",
    "lastfm_import_activate_collection",
    "lastfm_import_change_track",
    "lastfm_import_change_album",
    "lastfm_import_apply",
    "lastfm_import_retry_apply",
    "lastfm_import_prepare_accept_all",
    "lastfm_import_accept_all",
    "load_diagnostics",
    "email_diagnostics",
    "open_external_destination",
];

fn main() {
    let windows_msvc = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    let attributes = if windows_msvc {
        tauri_build::Attributes::new()
            .windows_attributes(tauri_build::WindowsAttributes::new_without_app_manifest())
    } else {
        tauri_build::Attributes::new()
    };
    tauri_build::try_build(
        attributes.app_manifest(tauri_build::AppManifest::new().commands(APP_COMMANDS)),
    )
    .expect("failed to run Tauri build script");

    if windows_msvc {
        let manifest =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("windows-app-manifest.xml");
        println!("cargo:rerun-if-changed={}", manifest.display());
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
    }
}

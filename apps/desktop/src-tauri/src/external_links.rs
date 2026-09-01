use serde::Deserialize;

use crate::spotify_commands::{spotify_item_link, SpotifyOpenTarget};

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(super) enum ExternalDestination {
    LastFm,
    SpotifyAlbum { id: String },
}

fn destination_url(destination: ExternalDestination) -> Result<String, String> {
    match destination {
        ExternalDestination::LastFm => Ok("https://www.last.fm/".into()),
        ExternalDestination::SpotifyAlbum { id } => {
            spotify_item_link("album", &id, SpotifyOpenTarget::Web)
        }
    }
}

#[tauri::command]
pub(super) fn open_external_destination(destination: ExternalDestination) -> Result<(), String> {
    tauri_plugin_opener::open_url(destination_url(destination)?, None::<&str>)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_destinations_are_closed_and_canonical() {
        assert_eq!(
            destination_url(ExternalDestination::LastFm).unwrap(),
            "https://www.last.fm/"
        );
        assert_eq!(
            destination_url(ExternalDestination::SpotifyAlbum {
                id: "AbC123".into()
            })
            .unwrap(),
            "https://open.spotify.com/album/AbC123"
        );
        for id in ["", "../album", "album/id", "album?id"] {
            assert!(destination_url(ExternalDestination::SpotifyAlbum { id: id.into() }).is_err());
        }
        assert!(destination_url(ExternalDestination::SpotifyAlbum { id: "a".repeat(65) }).is_err());
    }
}

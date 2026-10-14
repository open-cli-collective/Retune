use std::{collections::HashMap, time::Duration};

use librespot_core::{authentication::Credentials, config::SessionConfig, Session, SpotifyUri};
use librespot_metadata::{Metadata, Playlist, Track};
use librespot_protocol::{
    authentication::AuthenticationType, playlist4_external::SelectedListContent,
};
use protobuf::Message;
use retune_spotify::tokens::PlaybackCredentials;
use serde::{Deserialize, Serialize};

use crate::playlists::{CachedPlaylist, CachedTrack, TRACK_METADATA_VERSION};

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Shadow {
    pub account: String,
    pub refreshed_at: u64,
    pub attempted_at: u64,
    pub error: Option<String>,
}

impl Shadow {
    pub fn due(&self, now: u64) -> bool {
        now.saturating_sub(self.refreshed_at) >= 6 * 60 * 60
            && now.saturating_sub(self.attempted_at) >= 15 * 60
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Candidate {
    pub id: String,
    pub name: String,
    pub owner: String,
    pub account: String,
}

// A metadata-only session never creates a player or takes over Spotify Connect.
pub(crate) struct Reader(Session);

impl Drop for Reader {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

impl Reader {
    pub async fn connect(saved: &PlaybackCredentials) -> Result<Self, String> {
        if saved.auth_data.is_empty() {
            return Err(
                "Authorize built-in Spotify playback in File → Account to shadow playlists.".into(),
            );
        }
        let reader = Self(Session::new(SessionConfig::default(), None));
        tokio::time::timeout(Duration::from_secs(30), async {
            reader
                .0
                .connect(
                    Credentials {
                        username: Some(saved.username.clone()),
                        auth_type: AuthenticationType::AUTHENTICATION_STORED_SPOTIFY_CREDENTIALS,
                        auth_data: saved.auth_data.clone(),
                    },
                    false,
                )
                .await
                .map_err(|e| e.to_string())?;
            reader
                .0
                .login5()
                .auth_token()
                .await
                .map_err(|e| e.to_string())?;
            if reader.0.username() != saved.username {
                return Err(
                    "Spotify account changed. Reconnect before shadowing playlists.".into(),
                );
            }
            Ok::<_, String>(())
        })
        .await
        .map_err(|_| "Spotify playlist connection timed out.".to_string())??;
        Ok(reader)
    }

    pub async fn discover(&self, account: &str) -> Result<Vec<Candidate>, String> {
        let mut result = Vec::new();
        let mut offset = 0;
        let mut revision = None;
        loop {
            let bytes = self
                .0
                .spclient()
                .get_rootlist(offset, Some(500))
                .await
                .map_err(|e| e.to_string())?;
            let root = SelectedListContent::parse_from_bytes(&bytes).map_err(|e| e.to_string())?;
            if revision
                .as_ref()
                .is_some_and(|r| r != &root.revision().to_vec())
            {
                return Err("Saved playlists changed during discovery. Try again.".into());
            }
            revision = Some(root.revision().to_vec());
            let contents = root
                .contents
                .as_ref()
                .ok_or("Spotify returned no saved playlists.")?;
            if contents.pos() as usize != offset
                || contents.items.len() != contents.meta_items.len()
            {
                return Err(
                    "Spotify returned incomplete playlist discovery metadata. Try again.".into(),
                );
            }
            for (item, meta) in contents.items.iter().zip(&contents.meta_items) {
                let Ok(SpotifyUri::Playlist { id, .. }) = SpotifyUri::from_uri(item.uri()) else {
                    continue;
                };
                if meta.owner_username() == account {
                    continue;
                }
                result.push(Candidate {
                    id: id.to_base62().map_err(|e| e.to_string())?,
                    name: meta
                        .attributes
                        .as_ref()
                        .map_or("Playlist", |a| a.name())
                        .to_owned(),
                    owner: meta.owner_username().to_owned(),
                    account: account.to_owned(),
                });
            }
            offset += contents.items.len();
            if offset >= root.length().max(0) as usize {
                break;
            }
            if contents.items.is_empty() || offset > 10_000 {
                return Err("Spotify returned an incomplete saved-playlist list.".into());
            }
        }
        Ok(result)
    }

    pub async fn fetch(
        &self,
        id: &str,
        account: &str,
        cached: Option<&CachedPlaylist>,
    ) -> Result<CachedPlaylist, String> {
        let uri =
            SpotifyUri::from_uri(&format!("spotify:playlist:{id}")).map_err(|e| e.to_string())?;
        // shortcut: reject truncated snapshots; add revision-checked pagination if a needed playlist exceeds the native response limit.
        let playlist = Playlist::get(&self.0, &uri)
            .await
            .map_err(|e| e.to_string())?;
        validate_complete(
            playlist.length,
            playlist.contents.position,
            playlist.contents.is_truncated,
            playlist.contents.items.len(),
        )?;
        let owner = match &playlist.id {
            SpotifyUri::Playlist { user, .. } => user.clone(),
            _ => return Err("Spotify returned a different playlist identity.".into()),
        };
        if owner.as_deref() == Some(account) {
            return Err("Your own playlists already sync with Retune.".into());
        }
        let mut metadata: HashMap<String, CachedTrack> = cached
            .into_iter()
            .flat_map(|p| p.spotify_tracks.iter())
            .map(|t| (t.uri.clone(), t.clone()))
            .collect();
        let mut tracks = Vec::new();
        let mut spotify_tracks = Vec::new();
        for uri in playlist.tracks() {
            if !matches!(uri, SpotifyUri::Track { .. }) {
                return Err(
                    "This playlist contains unsupported items; its previous shadow has been kept."
                        .into(),
                );
            }
            let key = uri.to_uri().map_err(|e| e.to_string())?;
            if !metadata.contains_key(&key) {
                let track = Track::get(&self.0, uri).await.map_err(|e| e.to_string())?;
                if track.name.is_empty() || track.duration <= 0 {
                    return Err("Spotify returned incomplete track metadata; its previous shadow has been kept.".into());
                }
                metadata.insert(
                    key.clone(),
                    CachedTrack {
                        uri: key.clone(),
                        name: track.name,
                        art: track
                            .artists
                            .iter()
                            .map(|a| a.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                        alb: track.album.name,
                        duration: track.duration as u64,
                        disc_no: u32::try_from(track.disc_number).ok().filter(|n| *n > 0),
                        track_no: u32::try_from(track.number).ok().filter(|n| *n > 0),
                        release_date: Some(track.album.date.date().to_string()),
                    },
                );
            }
            spotify_tracks.push(metadata[&key].clone());
            tracks.push(key);
        }
        let now = crate::unix_now();
        Ok(CachedPlaylist {
            id: id.to_owned(),
            name: playlist.name().to_owned(),
            owned: false,
            owner,
            snapshot_id: playlist
                .revision
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
            track_count: tracks.len(),
            tracks,
            spotify_tracks,
            track_metadata_version: TRACK_METADATA_VERSION,
            shadow: Some(Shadow {
                account: account.to_owned(),
                refreshed_at: now,
                attempted_at: now,
                error: None,
            }),
        })
    }
}

fn validate_complete(
    length: i32,
    position: i32,
    truncated: bool,
    returned: usize,
) -> Result<(), String> {
    if length < 0 || position != 0 || truncated || returned != length as usize || returned > 10_000
    {
        return Err(
            "Spotify returned a partial playlist; its previous shadow has been kept.".into(),
        );
    }
    Ok(())
}

pub(crate) fn apply_refresh(
    previous: &mut CachedPlaylist,
    fetched: Result<CachedPlaylist, String>,
    now: u64,
) -> Result<(), String> {
    match fetched {
        Ok(fetched) => {
            *previous = fetched;
            Ok(())
        }
        Err(error) => {
            let shadow = previous
                .shadow
                .as_mut()
                .ok_or("This playlist is no longer shadowed.")?;
            shadow.attempted_at = now;
            shadow.error = Some(error.clone());
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_snapshots_cannot_replace_a_shadow() {
        assert!(validate_complete(2, 0, false, 2).is_ok());
        assert!(validate_complete(0, 0, false, 0).is_ok());
        for (length, position, truncated, returned) in [
            (2, 0, false, 1),
            (2, 1, false, 2),
            (2, 0, true, 2),
            (-1, 0, false, 0),
        ] {
            assert!(validate_complete(length, position, truncated, returned).is_err());
        }
    }

    #[test]
    fn automatic_refresh_waits_six_hours_and_backs_off_after_failure() {
        let mut shadow = Shadow {
            account: "user".into(),
            refreshed_at: 100,
            attempted_at: 100,
            error: None,
        };
        assert!(!shadow.due(100 + 6 * 3600 - 1));
        assert!(shadow.due(100 + 6 * 3600));
        shadow.attempted_at = 100 + 6 * 3600;
        assert!(!shadow.due(shadow.attempted_at + 899));
        assert!(shadow.due(shadow.attempted_at + 900));
    }
    #[tokio::test]
    #[ignore = "Needs explicit Spotify dev token path and playlist IDs; reads Spotify and writes only a temporary cache"]
    async fn live_shadow_round_trip() {
        let path = std::env::var("RETUNE_SHADOW_PROBE_TOKENS").expect("dev token path");
        let ids = std::env::var("RETUNE_SHADOW_PROBE_IDS").expect("comma-separated playlist IDs");
        let tokens: retune_spotify::tokens::Tokens =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let credentials = tokens.playback_credentials.unwrap();
        let reader = Reader::connect(&credentials).await.unwrap();
        let candidates = reader.discover(&credentials.username).await.unwrap();
        let mut cache = crate::playlists::PlaylistCache::default();
        for id in ids.split(',') {
            assert!(candidates.iter().any(|c| c.id == id));
            let playlist = reader.fetch(id, &credentials.username, None).await.unwrap();
            assert!(!playlist.tracks.is_empty());
            assert_eq!(playlist.tracks.len(), playlist.spotify_tracks.len());
            assert!(playlist
                .spotify_tracks
                .iter()
                .all(|t| !t.name.is_empty() && t.duration > 0));
            let refreshed = reader
                .fetch(id, &credentials.username, Some(&playlist))
                .await
                .unwrap();
            assert_eq!(playlist.tracks, refreshed.tracks);
            println!(
                "Shadowed {}: {} complete tracks; refresh preserved canonical order",
                playlist.name, playlist.track_count
            );
            cache.playlists.push(refreshed);
        }
        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::FsPlaylistStore::new(directory.path());
        store.save(&cache).unwrap();
        assert_eq!(store.load().unwrap(), cache);
    }
}

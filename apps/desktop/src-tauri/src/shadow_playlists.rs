use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use librespot_core::{authentication::Credentials, config::SessionConfig, Session, SpotifyUri};
use librespot_metadata::{Metadata, Playlist, Track};
use librespot_protocol::{
    authentication::AuthenticationType,
    extended_metadata::{
        BatchedEntityRequest, BatchedExtensionResponse, EntityRequest, ExtensionQuery,
    },
    extension_kind::ExtensionKind,
    playlist4_external::SelectedListContent,
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

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Progress {
    pub completed: usize,
    pub total: usize,
}

// A conservative application batch size, not an asserted Spotify API limit.
const METADATA_BATCH_SIZE: usize = 50;

fn missing_tracks(tracks: &[String], metadata: &HashMap<String, CachedTrack>) -> Vec<String> {
    let mut seen = HashSet::new();
    tracks
        .iter()
        .filter(|uri| !metadata.contains_key(*uri) && seen.insert((*uri).clone()))
        .cloned()
        .collect()
}

fn decode_batch(
    response: BatchedExtensionResponse,
    requested: &[String],
) -> Result<HashMap<String, CachedTrack>, String> {
    let mut tracks = HashMap::new();
    for group in response.extended_metadata {
        if group.extension_kind.enum_value().ok() != Some(ExtensionKind::TRACK_V4) {
            continue;
        }
        if group
            .header
            .as_ref()
            .is_some_and(|header| !matches!(header.provider_error_status, 0 | 200))
        {
            return Err(
                "Spotify could not provide track metadata; the previous shadow has been kept."
                    .into(),
            );
        }
        for entry in group.extension_data {
            if entry
                .header
                .as_ref()
                .is_some_and(|header| !matches!(header.status_code, 0 | 200))
            {
                return Err(
                    "Spotify could not provide track metadata; the previous shadow has been kept."
                        .into(),
                );
            }
            if !requested.contains(&entry.entity_uri) || tracks.contains_key(&entry.entity_uri) {
                return Err("Spotify returned unexpected track metadata; the previous shadow has been kept.".into());
            }
            let data = entry.extension_data.as_ref().ok_or(
                "Spotify returned missing track metadata; the previous shadow has been kept.",
            )?;
            let message = librespot_protocol::metadata::Track::parse_from_bytes(&data.value)
                .map_err(|e| e.to_string())?;
            let track = Track::try_from(&message).map_err(|e| e.to_string())?;
            if track.id.to_uri().map_err(|e| e.to_string())? != entry.entity_uri
                || track.name.is_empty()
                || track.duration <= 0
            {
                return Err("Spotify returned incomplete track metadata; the previous shadow has been kept.".into());
            }
            tracks.insert(
                entry.entity_uri.clone(),
                CachedTrack {
                    uri: entry.entity_uri,
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
    }
    if tracks.len() != requested.len() {
        return Err(
            "Spotify returned partial track metadata; the previous shadow has been kept.".into(),
        );
    }
    Ok(tracks)
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
        progress: impl Fn(Progress),
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
        let tracks = playlist.tracks().map(|uri| {
            if !matches!(uri, SpotifyUri::Track { .. }) {
                return Err("This playlist contains unsupported items; its previous shadow has been kept.".into());
            }
            uri.to_uri().map_err(|e| e.to_string())
        }).collect::<Result<Vec<_>, String>>()?;
        let missing = missing_tracks(&tracks, &metadata);
        let report = |metadata: &HashMap<String, CachedTrack>| {
            progress(Progress {
                completed: tracks
                    .iter()
                    .filter(|uri| metadata.contains_key(*uri))
                    .count(),
                total: tracks.len(),
            })
        };
        report(&metadata);
        for batch in missing.chunks(METADATA_BATCH_SIZE) {
            let request = BatchedEntityRequest {
                entity_request: batch
                    .iter()
                    .map(|uri| EntityRequest {
                        entity_uri: uri.clone(),
                        query: vec![ExtensionQuery {
                            extension_kind: ExtensionKind::TRACK_V4.into(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            };
            let response = self.0.spclient().get_extended_metadata(request).await.map_err(|e| {
                format!("Couldn’t fetch track details from Spotify: {e}. The previous shadow has been kept. Try again later.")
            })?;
            metadata.extend(decode_batch(response, batch)?);
            report(&metadata);
        }
        let spotify_tracks = tracks.iter().map(|uri| metadata[uri].clone()).collect();
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

    fn batch_entry(id: u8) -> librespot_protocol::entity_extension_data::EntityExtensionData {
        let uri = SpotifyUri::Track {
            id: librespot_core::SpotifyId::from_raw(&[id; 16]).unwrap(),
        }
        .to_uri()
        .unwrap();
        let message = librespot_protocol::metadata::Track {
            gid: Some(vec![id; 16]),
            name: Some(format!("Track {id}")),
            duration: Some(123000),
            album: protobuf::MessageField::some(librespot_protocol::metadata::Album {
                gid: Some(vec![1; 16]),
                name: Some("Album".into()),
                date: protobuf::MessageField::some(librespot_protocol::metadata::Date {
                    year: Some(2020),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        librespot_protocol::entity_extension_data::EntityExtensionData {
            entity_uri: uri,
            extension_data: protobuf::MessageField::some(protobuf::well_known_types::any::Any {
                value: message.write_to_bytes().unwrap(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn batches_match_by_identity_and_reject_partial_or_duplicate_results() {
        let a = batch_entry(1);
        let b = batch_entry(2);
        let requested = vec![a.entity_uri.clone(), b.entity_uri.clone()];
        let response = |entries| BatchedExtensionResponse {
            extended_metadata: vec![
                librespot_protocol::extended_metadata::EntityExtensionDataArray {
                    extension_kind: ExtensionKind::TRACK_V4.into(),
                    extension_data: entries,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let decoded = decode_batch(response(vec![b.clone(), a.clone()]), &requested).unwrap();
        assert_eq!(decoded[&requested[0]].name, "Track 1");
        assert_eq!(decoded[&requested[1]].name, "Track 2");
        assert!(decode_batch(response(vec![a.clone()]), &requested).is_err());
        assert!(decode_batch(response(vec![a.clone(), a.clone()]), &requested).is_err());
        assert!(decode_batch(response(vec![a, batch_entry(3)]), &requested).is_err());
    }

    #[test]
    fn only_unique_uncached_tracks_need_requests() {
        let a = batch_entry(1);
        let b = batch_entry(2);
        let requested = vec![a.entity_uri.clone()];
        let response = BatchedExtensionResponse {
            extended_metadata: vec![
                librespot_protocol::extended_metadata::EntityExtensionDataArray {
                    extension_kind: ExtensionKind::TRACK_V4.into(),
                    extension_data: vec![a.clone()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let metadata = decode_batch(response, &requested).unwrap();
        let original = vec![
            b.entity_uri.clone(),
            a.entity_uri.clone(),
            b.entity_uri.clone(),
        ];
        assert_eq!(missing_tracks(&original, &metadata), vec![b.entity_uri]);
        assert_eq!(original.len(), 3);
    }

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
            let started = std::time::Instant::now();
            let playlist = reader
                .fetch(id, &credentials.username, None, |p| {
                    println!("Resolved {} / {} entries", p.completed, p.total)
                })
                .await
                .unwrap();
            println!("Initial fetch completed in {:?}", started.elapsed());
            assert!(!playlist.tracks.is_empty());
            assert_eq!(playlist.tracks.len(), playlist.spotify_tracks.len());
            assert!(playlist
                .spotify_tracks
                .iter()
                .all(|t| !t.name.is_empty() && t.duration > 0));
            let refreshed = reader
                .fetch(id, &credentials.username, Some(&playlist), |_| {})
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

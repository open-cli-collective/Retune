use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, de::DeserializeOwned};
use tokio::{sync::Mutex as AsyncMutex, time::Instant};

use crate::catalog::SpotifyCatalog;
use crate::tokens::{InMemoryTokenStore, TokenStore, Tokens};
use crate::{Error, Result};

mod endpoints;
mod models;
mod request;
mod transport;

use models::{
    AddPlaylistTracks, CreatePlaylist, Devices, PlaylistTrackItem, PlaylistTrackUri,
    RemovePlaylistTracks, ReorderPlaylistTracks, SnapshotResponse,
};
use request::{decode, paged, player_path};

#[cfg(test)]
use request::{retry_after_header_at, token_expired};

pub use models::{
    Album, AlbumSummary, Artist, Audiobook, Author, Chapter, CreatedPlaylist, Device, Episode,
    Followers, Image, Page, PlayerState, Playlist, PlaylistOwner, PlaylistTrackCount, Profile,
    SavedAlbum, SavedEpisode, SavedShow, SavedTrack, SearchResults, Show, SimplifiedArtist, Track,
};

pub use request::{SpotifyClient, endpoint_family};

pub use transport::{
    FakeTransport, HttpTransport, Method, Request, Response, SendFuture, Transport,
};

const API_BASE: &str = "https://api.spotify.com/v1";
pub const MAX_LIBRARY_WRITE_URIS: usize = 10_000;
pub const MAX_SEARCH_OFFSET: u32 = 1_000;
pub const MAX_SEARCH_QUERY_BYTES: usize = 4 * 1024;
const MAX_RATE_LIMIT_RETRIES: usize = 3;
const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(300);
const SERVER_RETRY_BACKOFFS: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(3)];

fn reject_local_uris(uris: &[String]) -> Result<()> {
    if uris.iter().any(|uri| uri.starts_with("file:")) {
        return Err(Error::InvalidRequest(
            "local file URIs cannot be sent to Spotify".into(),
        ));
    }
    Ok(())
}

pub fn validate_search_input(query: &str, offset: u32) -> Result<()> {
    if query.len() > MAX_SEARCH_QUERY_BYTES {
        return Err(Error::InvalidRequest(format!(
            "search query exceeds {MAX_SEARCH_QUERY_BYTES} bytes"
        )));
    }
    if offset > MAX_SEARCH_OFFSET {
        return Err(Error::InvalidRequest(format!(
            "search offset exceeds {MAX_SEARCH_OFFSET}"
        )));
    }
    Ok(())
}

fn spotify_path_id<'a>(id: &'a str, kind: &str) -> Result<&'a str> {
    if id.is_empty() || id.len() > 64 || !id.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err(Error::InvalidRequest(format!("invalid Spotify {kind} ID")));
    }
    Ok(id)
}

/// Test double: a client over [`FakeTransport`] with never-expiring tokens
/// granting `scopes`.
pub fn fake_client(
    responses: impl IntoIterator<Item = Response>,
    scopes: &str,
) -> SpotifyClient<FakeTransport, InMemoryTokenStore> {
    SpotifyClient::new(
        "client",
        FakeTransport::new(responses),
        InMemoryTokenStore::new(Some(Tokens {
            access: "access".into(),
            refresh: "refresh".into(),
            expires_at: u64::MAX,
            scopes: scopes.into(),
            playback_credentials: None,
        })),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::auth::TOKEN_URL;
    use crate::tokens::{CachedTokenStore, InMemoryTokenStore, PlaybackCredentials};
    use tokio::sync::Barrier;

    struct OverlapTransport {
        responses: Mutex<VecDeque<Response>>,
        active: AtomicUsize,
        max_active: AtomicUsize,
        send_times: Mutex<Vec<Instant>>,
    }

    impl OverlapTransport {
        fn new(responses: impl IntoIterator<Item = Response>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
                send_times: Mutex::new(vec![]),
            }
        }

        fn send_times(&self) -> Vec<Instant> {
            self.send_times.lock().expect("fake mutex poisoned").clone()
        }
    }

    impl Transport for OverlapTransport {
        fn send(&self, _request: Request) -> SendFuture<'_> {
            Box::pin(async move {
                let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_active.fetch_max(active, Ordering::SeqCst);
                self.send_times
                    .lock()
                    .map_err(|error| Error::Transport(error.to_string()))?
                    .push(Instant::now());
                tokio::task::yield_now().await;
                let response = self
                    .responses
                    .lock()
                    .map_err(|error| Error::Transport(error.to_string()))?
                    .pop_front()
                    .ok_or_else(|| Error::Transport("fake response queue exhausted".into()));
                self.active.fetch_sub(1, Ordering::SeqCst);
                response
            })
        }
    }

    struct DelayedRefreshTransport {
        entered: Arc<Barrier>,
        release: Arc<Barrier>,
    }

    impl Transport for DelayedRefreshTransport {
        fn send(&self, _request: Request) -> SendFuture<'_> {
            Box::pin(async move {
                self.entered.wait().await;
                self.release.wait().await;
                Ok(Response::json(
                    200,
                    serde_json::json!({
                        "access_token": "stale-refresh",
                        "refresh_token": "rotated-refresh",
                        "expires_in": 3600
                    }),
                ))
            })
        }
    }

    fn tokens() -> InMemoryTokenStore {
        InMemoryTokenStore::new(Some(Tokens {
            access: "old".into(),
            refresh: "refresh".into(),
            expires_at: 0,
            scopes: "streaming user-read-private".into(),
            playback_credentials: None,
        }))
    }

    #[tokio::test]
    async fn saved_track_page_sends_bearer_and_decodes() {
        let transport = FakeTransport::new([Response::json(
            200,
            serde_json::json!({"items": [{"added_at": "2024-01-02T03:04:05Z", "track": {
                "uri": "spotify:track:1", "name": "One", "duration_ms": 3,
                "artists": [], "album": null
            }}, {"added_at": "2024-01-02T03:04:05.987Z", "track": {
                "uri": "spotify:track:2", "name": "Two", "duration_ms": 3,
                "artists": [], "album": null
            }}], "next": null}),
        )]);
        let client = SpotifyClient::new("client", transport, tokens());
        let page = client.saved_tracks(20, 10).await.unwrap();
        assert_eq!(page.items[0].track.name, "One");
        assert_eq!(page.items[0].added_at, Some(1_704_164_645));
        assert_eq!(page.items[1].added_at, Some(1_704_164_645));
        assert_eq!(client.track("1").await.unwrap(), page.items[0].track);
        let requests = client.transport().requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].url,
            format!("{API_BASE}/me/tracks?offset=20&limit=10")
        );
        assert_eq!(requests[0].headers["authorization"], "Bearer old");
    }

    #[tokio::test]
    async fn type_restricted_search_accepts_omitted_result_groups() {
        let client = SpotifyClient::new(
            "client",
            FakeTransport::new([
                Response::json(
                    200,
                    serde_json::json!({
                        "albums": {
                            "items": [{
                                "id": "albumid",
                                "uri": "spotify:album:albumid",
                                "name": "Parasite (acoustic)",
                                "artists": [{"id": "artistid", "name": "Dead by April"}]
                            }],
                            "next": null,
                            "total": 1
                        }
                    }),
                ),
                Response::json(
                    200,
                    serde_json::json!({
                        "tracks": {
                            "items": [{
                                "uri": "spotify:track:trackid",
                                "name": "Parasite",
                                "artists": [{"id": "artistid", "name": "Dead by April"}],
                                "album": {
                                    "id": "albumid",
                                    "uri": "spotify:album:albumid",
                                    "name": "Parasite (acoustic)"
                                }
                            }],
                            "next": null,
                            "total": 1
                        }
                    }),
                ),
            ]),
            tokens(),
        );

        let albums = client
            .search_with_types("album query", "album", 0, 10)
            .await
            .unwrap();
        assert_eq!(albums.albums.items[0].uri, "spotify:album:albumid");
        assert_eq!(albums.albums.items[0].artists[0].name, "Dead by April");
        assert!(albums.artists.items.is_empty());
        assert!(albums.tracks.items.is_empty());

        let tracks = client
            .search_with_types("track query", "track", 0, 10)
            .await
            .unwrap();
        assert_eq!(tracks.tracks.items[0].uri, "spotify:track:trackid");
        assert_eq!(tracks.tracks.items[0].name, "Parasite");
        assert_eq!(tracks.tracks.items[0].artists[0].name, "Dead by April");
        assert!(tracks.artists.items.is_empty());
        assert!(tracks.albums.items.is_empty());
        assert_eq!(
            client.track("trackid").await.unwrap(),
            tracks.tracks.items[0]
        );

        let requests = client.transport().requests();
        assert!(requests[0].url.contains("type=album"));
        assert!(requests[1].url.contains("type=track"));
        assert_eq!(requests.len(), 2);
    }

    #[tokio::test]
    async fn search_limits_reject_before_transport_or_catalog_effects() {
        let client = SpotifyClient::new("client", FakeTransport::new([]), tokens());
        let generation = client
            .catalog
            .lock()
            .expect("Spotify catalog mutex poisoned")
            .generation();

        let long_query = "q".repeat(MAX_SEARCH_QUERY_BYTES + 1);
        assert!(
            validate_search_input(&"q".repeat(MAX_SEARCH_QUERY_BYTES), MAX_SEARCH_OFFSET).is_ok()
        );
        assert!(client.search(&long_query, 0, 10).await.is_err());
        assert!(
            client
                .search("query", MAX_SEARCH_OFFSET + 1, 10)
                .await
                .is_err()
        );

        assert!(client.transport().requests().is_empty());
        assert_eq!(
            client
                .catalog
                .lock()
                .expect("Spotify catalog mutex poisoned")
                .generation(),
            generation
        );
    }

    #[tokio::test]
    async fn saved_album_decodes_membership_time() {
        let client = SpotifyClient::new(
            "client",
            FakeTransport::new([Response::json(
                200,
                serde_json::json!({"items": [{"added_at": "2024-01-02T03:04:05Z", "album": {
                    "id": "album-1", "uri": "spotify:album:1", "name": "Album",
                    "artists": [], "images": []
                }}], "next": null}),
            )]),
            tokens(),
        );

        let page = client.saved_albums(0, 1).await.unwrap();

        assert_eq!(page.items[0].added_at, Some(1_704_164_645));
    }

    #[tokio::test]
    async fn refreshes_once_on_unauthorized() {
        let transport = FakeTransport::new([
            Response::json(401, serde_json::json!({"error": "expired"})),
            Response::json(
                200,
                serde_json::json!({"access_token": "new", "expires_in": 3600}),
            ),
            Response::json(200, serde_json::json!({"items": [], "next": null})),
        ]);
        let store = tokens();
        store
            .save(&Tokens {
                playback_credentials: Some(crate::tokens::PlaybackCredentials {
                    username: "user".into(),
                    auth_data: vec![1, 2, 3],
                }),
                ..store.load().unwrap().unwrap()
            })
            .unwrap();
        let client = SpotifyClient::new("client", transport, store);
        client.saved_tracks(0, 50).await.unwrap();
        let requests = client.transport().requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[1].url, TOKEN_URL);
        assert_eq!(requests[2].headers["authorization"], "Bearer new");
        assert_eq!(
            client.tokens.load().unwrap().unwrap().scopes,
            "streaming user-read-private"
        );
        assert_eq!(client.tokens.load().unwrap().unwrap().refresh, "refresh");
        assert_eq!(
            client
                .tokens
                .load()
                .unwrap()
                .unwrap()
                .playback_credentials
                .unwrap()
                .auth_data,
            vec![1, 2, 3]
        );
    }

    #[tokio::test]
    async fn access_token_refreshes_an_expired_grant() {
        let client = SpotifyClient::new(
            "client",
            FakeTransport::new([Response::json(
                200,
                serde_json::json!({"access_token": "new", "expires_in": 3600}),
            )]),
            tokens(),
        );

        let cooldown = Instant::now() + Duration::from_secs(60);
        *client.request_not_before.lock().await = Some(cooldown);

        assert_eq!(client.access_token().await.unwrap(), "new");
        let request = &client.transport().requests()[0];
        assert_eq!(request.url, TOKEN_URL);
        assert_eq!(
            url::form_urlencoded::parse(&request.body)
                .into_owned()
                .collect::<HashMap<_, _>>(),
            HashMap::from([
                ("client_id".into(), "client".into()),
                ("grant_type".into(), "refresh_token".into()),
                ("refresh_token".into(), "refresh".into()),
            ])
        );
        assert!(client.request_counts().is_empty());
        assert_eq!(*client.request_not_before.lock().await, Some(cooldown));
    }

    #[tokio::test]
    async fn malformed_refresh_tokens_do_not_change_storage() {
        for response in [
            serde_json::json!({"access_token": "", "expires_in": 3600}),
            serde_json::json!({
                "access_token": "new",
                "refresh_token": " ",
                "expires_in": 3600
            }),
        ] {
            let store = tokens();
            let before = store.load().unwrap().unwrap();
            let client = SpotifyClient::new(
                "client",
                FakeTransport::new([Response::json(200, response)]),
                store,
            );

            assert!(matches!(
                client.access_token().await,
                Err(Error::Json { .. })
            ));
            assert_eq!(client.token_store().load().unwrap(), Some(before));
        }
    }

    #[tokio::test]
    async fn delayed_refresh_cannot_resurrect_a_cleared_grant() {
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let store = Arc::new(CachedTokenStore::new(tokens()));
        let client = SpotifyClient::new(
            "client",
            DelayedRefreshTransport {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
            },
            Arc::clone(&store),
        );
        let refresh = tokio::spawn(async move { client.access_token().await });

        entered.wait().await;
        store.clear().unwrap();
        release.wait().await;

        assert!(matches!(refresh.await.unwrap(), Err(Error::MissingToken)));
        assert_eq!(store.load().unwrap(), None);
    }

    #[tokio::test]
    async fn delayed_refresh_cannot_overwrite_a_replacement_grant() {
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let initial = Tokens {
            playback_credentials: Some(PlaybackCredentials {
                username: "old-user".into(),
                auth_data: vec![1, 2, 3],
            }),
            ..tokens().load().unwrap().unwrap()
        };
        let store = Arc::new(CachedTokenStore::new(InMemoryTokenStore::new(Some(
            initial,
        ))));
        let client = SpotifyClient::new(
            "client",
            DelayedRefreshTransport {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
            },
            Arc::clone(&store),
        );
        let refresh = tokio::spawn(async move { client.access_token().await });

        entered.wait().await;
        let replacement = Tokens {
            access: "replacement".into(),
            refresh: "replacement-refresh".into(),
            expires_at: u64::MAX,
            scopes: "replacement-scope".into(),
            playback_credentials: None,
        };
        store.save(&replacement).unwrap();
        release.wait().await;

        assert_eq!(refresh.await.unwrap().unwrap(), "replacement");
        assert_eq!(store.load().unwrap(), Some(replacement));
    }

    #[tokio::test]
    async fn delayed_refresh_preserves_new_playback_credentials_on_the_same_grant() {
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let store = Arc::new(CachedTokenStore::new(tokens()));
        let client = SpotifyClient::new(
            "client",
            DelayedRefreshTransport {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
            },
            Arc::clone(&store),
        );
        let refresh = tokio::spawn(async move { client.access_token().await });

        entered.wait().await;
        let expected = store.load().unwrap().unwrap();
        let mut authorized = expected.clone();
        authorized.playback_credentials = Some(PlaybackCredentials {
            username: "user".into(),
            auth_data: vec![1, 2, 3],
        });
        assert!(store.replace_if_current(&expected, &authorized).unwrap());
        release.wait().await;

        assert_eq!(refresh.await.unwrap().unwrap(), "stale-refresh");
        let refreshed = store.load().unwrap().unwrap();
        assert_eq!(refreshed.refresh, "rotated-refresh");
        assert_eq!(
            refreshed.playback_credentials,
            authorized.playback_credentials
        );
    }

    #[tokio::test]
    async fn profile_exposes_immutable_account_id_and_tolerates_removed_product() {
        let client = SpotifyClient::new(
            "client",
            FakeTransport::new([
                Response::json(200, serde_json::json!({"id": "user"})),
                Response::json(
                    200,
                    serde_json::json!({"account_id": "account", "id": "profile"}),
                ),
            ]),
            tokens(),
        );

        assert_eq!(client.me().await.unwrap().account_id(), None);
        let profile = client.me().await.unwrap();
        assert_eq!(profile.id, "profile");
        assert_eq!(profile.account_id(), Some("account"));
    }

    #[tokio::test]
    async fn quota_exhaustion_is_classified_without_retrying() {
        for retry_after_secs in [None, Some(120)] {
            let response = Response::quota_exceeded(retry_after_secs);
            let client = SpotifyClient::new("client", FakeTransport::new([response]), tokens());

            let error = client.saved_tracks(0, 1).await.unwrap_err();

            assert!(matches!(
                error,
                Error::QuotaExceeded {
                    retry_after_secs: actual,
                    ..
                } if actual == retry_after_secs
            ));
            assert_eq!(client.transport().requests().len(), 1);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn malformed_retry_after_is_transient_fallback_but_not_quota_deadline() {
        let transient = Response::rate_limited("not a date");
        let client = SpotifyClient::new(
            "client",
            FakeTransport::new([
                transient,
                Response::json(200, serde_json::json!({"items": [], "next": null})),
            ]),
            tokens(),
        );
        let started = Instant::now();

        client.saved_tracks(0, 1).await.unwrap();

        assert_eq!(Instant::now() - started, Duration::from_secs(1));

        let mut quota = Response::json(
            429,
            serde_json::json!({"error": {"reason": "QUOTA_EXCEEDED"}}),
        );
        quota
            .headers
            .insert("retry-after".into(), "not a date".into());
        let client = SpotifyClient::new("client", FakeTransport::new([quota]), tokens());

        assert!(matches!(
            client.saved_tracks(0, 1).await.unwrap_err(),
            Error::QuotaExceeded {
                retry_after_secs: None,
                ..
            }
        ));
        assert_eq!(client.transport().requests().len(), 1);
    }

    #[tokio::test]
    async fn unknown_and_malformed_rate_limit_bodies_remain_transient() {
        for body in [
            serde_json::to_vec(&serde_json::json!({"error": {"reason": "UNKNOWN"}})).unwrap(),
            b"not json".to_vec(),
        ] {
            let limited = || Response {
                status: 429,
                headers: HashMap::from([("retry-after".into(), "0".into())]),
                body: body.clone(),
            };
            let client = SpotifyClient::new(
                "client",
                FakeTransport::new([limited(), limited(), limited(), limited()]),
                tokens(),
            );

            assert!(matches!(
                client.saved_tracks(0, 1).await.unwrap_err(),
                Error::RateLimited { .. }
            ));
            assert_eq!(client.transport().requests().len(), 4);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_callers_do_not_send_or_retry_together() {
        let transport = OverlapTransport::new([
            Response::rate_limited("1"),
            Response::json(200, serde_json::json!({"items": [], "next": null})),
            Response::json(200, serde_json::json!({"items": [], "next": null})),
        ]);
        let client = SpotifyClient::new("client", transport, tokens());
        let cooldown_deadline = Instant::now() + Duration::from_secs(1);

        let (tracks, albums) = tokio::join!(client.saved_tracks(0, 1), client.saved_albums(0, 1));

        tracks.unwrap();
        albums.unwrap();
        assert_eq!(client.transport().max_active.load(Ordering::SeqCst), 1);
        let send_times = client.transport().send_times();
        assert_eq!(send_times.len(), 3);
        assert!(
            send_times[1..]
                .iter()
                .all(|sent_at| *sent_at >= cooldown_deadline)
        );
    }

    #[tokio::test]
    async fn retries_rate_limit_and_reports_endpoint_for_bad_json() {
        let transport = FakeTransport::new([
            Response::rate_limited("0"),
            Response {
                status: 200,
                headers: HashMap::new(),
                body: b"not json".to_vec(),
            },
        ]);
        let client = SpotifyClient::new("client", transport, tokens());
        let error = client.saved_albums(0, 1).await.unwrap_err();
        assert!(error.to_string().contains("/me/albums?offset=0&limit=1"));
        assert_eq!(client.transport().requests().len(), 2);
    }

    #[tokio::test]
    async fn rate_limit_retries_are_capped() {
        let transport = FakeTransport::new(std::iter::repeat_n(Response::rate_limited("0"), 4));
        let client = SpotifyClient::new("client", transport, tokens());
        let error = client.saved_tracks(0, 1).await.unwrap_err();
        assert!(matches!(error, Error::RateLimited { .. }));
        assert_eq!(client.transport().requests().len(), 4);
    }

    #[tokio::test]
    async fn long_rate_limit_returns_without_retrying() {
        let transport = FakeTransport::new([Response::rate_limited("3600")]);
        let client = SpotifyClient::new("client", transport, tokens());
        let error = client.saved_tracks(0, 1).await.unwrap_err();
        assert!(matches!(
            error,
            Error::RateLimited {
                retry_after_secs: 3600,
                ..
            }
        ));
        assert_eq!(client.transport().requests().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn retries_server_errors_twice_then_succeeds() {
        let transport = FakeTransport::new([
            Response::json(500, serde_json::json!({})),
            Response::json(502, serde_json::json!({})),
            Response::json(200, serde_json::json!({"items": [], "next": null})),
        ]);
        let client = SpotifyClient::new("client", transport, tokens());
        let started = Instant::now();

        assert!(client.saved_albums(50, 50).await.is_ok());
        assert_eq!(Instant::now() - started, Duration::from_secs(4));
        assert_eq!(client.transport().requests().len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_retries_are_capped() {
        let transport = FakeTransport::new([
            Response::json(500, serde_json::json!({})),
            Response::json(502, serde_json::json!({})),
            Response::json(503, serde_json::json!({})),
        ]);
        let client = SpotifyClient::new("client", transport, tokens());

        let error = client.saved_albums(50, 50).await.unwrap_err();
        assert!(matches!(
            error,
            Error::ServerError {
                endpoint,
                status: 503
            } if endpoint == "/me/albums?offset=50&limit=50"
        ));
        assert_eq!(client.transport().requests().len(), 3);
    }

    #[tokio::test]
    async fn mutation_server_and_transport_failures_are_ambiguous_and_not_retried() {
        let server = SpotifyClient::new(
            "client",
            FakeTransport::new([
                Response::json(500, serde_json::json!({})),
                Response::json(
                    201,
                    serde_json::json!({"id": "duplicate", "name": "Road Trip"}),
                ),
            ]),
            tokens(),
        );
        assert!(matches!(
            server.create_playlist("Road Trip").await,
            Err(Error::AmbiguousMutation {
                status: Some(500),
                ..
            })
        ));
        assert_eq!(server.transport().requests().len(), 1);

        let add = SpotifyClient::new(
            "client",
            FakeTransport::new([
                Response::json(503, serde_json::json!({})),
                Response::json(201, serde_json::json!({"snapshot_id": "duplicate"})),
            ]),
            tokens(),
        );
        assert!(matches!(
            add.add_playlist_tracks("playlist", &["spotify:track:one".into()], None)
                .await,
            Err(Error::AmbiguousMutation {
                status: Some(503),
                ..
            })
        ));
        assert_eq!(add.transport().requests().len(), 1);

        let transport = SpotifyClient::new("client", FakeTransport::default(), tokens());
        assert!(matches!(
            transport.create_playlist("Road Trip").await,
            Err(Error::AmbiguousMutation { status: None, .. })
        ));
        assert_eq!(transport.transport().requests().len(), 1);
    }

    #[test]
    fn expiry_and_http_date_boundaries_are_exact() {
        assert!(!token_expired(10, 9));
        assert!(token_expired(10, 10));
        assert!(token_expired(10, 11));

        let deadline = UNIX_EPOCH + Duration::from_secs(10);
        let headers = HashMap::from([("retry-after".into(), httpdate::fmt_http_date(deadline))]);
        assert_eq!(
            retry_after_header_at(&headers, UNIX_EPOCH + Duration::from_secs(9)),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            retry_after_header_at(&headers, deadline),
            Some(Duration::ZERO)
        );
        assert_eq!(
            retry_after_header_at(&headers, UNIX_EPOCH + Duration::from_secs(11)),
            None
        );
    }

    #[tokio::test]
    async fn artist_lookup_is_cached_for_the_run() {
        let transport = FakeTransport::new([Response::json(
            200,
            serde_json::json!({"id": "a", "name": "Artist", "genres": ["rock"]}),
        )]);
        let client = SpotifyClient::new("client", transport, tokens());
        assert_eq!(client.artist("a").await.unwrap().genres, ["rock"]);
        assert_eq!(client.artist("a").await.unwrap().name, "Artist");
        assert_eq!(client.transport().requests().len(), 1);
    }

    #[tokio::test]
    async fn album_pages_inline_tracks_past_the_first_page() {
        let transport = FakeTransport::new([
            Response::json(
                200,
                serde_json::json!({
                    "id": "album", "uri": "spotify:album:album", "name": "Album",
                    "album_type": "album", "release_date": "2024-01-02",
                    "artists": [{"id": "artist", "name": "Artist"}], "images": [],
                    "tracks": {
                        "items": [{
                            "uri": "spotify:track:1", "name": "One",
                            "track_number": 1, "duration_ms": 1000
                        }],
                        "next": "next", "total": 2
                    }
                }),
            ),
            Response::json(
                200,
                serde_json::json!({
                    "items": [{
                        "uri": "spotify:track:2", "name": "Two",
                        "track_number": 2, "duration_ms": 2000
                    }],
                    "next": null, "total": 2
                }),
            ),
        ]);
        let client = SpotifyClient::new("client", transport, tokens());

        let album = client.album("album").await.unwrap();

        assert_eq!(
            album
                .tracks
                .unwrap()
                .items
                .iter()
                .map(|track| track.name.as_str())
                .collect::<Vec<_>>(),
            ["One", "Two"]
        );
        let track = client.track("1").await.unwrap();
        assert_eq!(
            track.album.as_ref().map(|album| album.uri.as_str()),
            Some("spotify:album:album")
        );
        let requests = client.transport().requests();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].url.ends_with("/albums/album"));
        assert!(
            requests[1]
                .url
                .ends_with("/albums/album/tracks?offset=1&limit=50")
        );
    }

    #[tokio::test]
    async fn artist_follow_contains_and_writes_use_library_uris() {
        let transport = FakeTransport::new([
            Response::json(200, serde_json::json!([true])),
            Response::json(204, serde_json::Value::Null),
            Response::json(204, serde_json::Value::Null),
        ]);
        let client = SpotifyClient::new("client", transport, tokens());

        assert!(client.is_following_artist("ab").await.unwrap());
        client.follow_artist("ab", true).await.unwrap();
        client.follow_artist("ab", false).await.unwrap();

        let requests = client.transport().requests();
        assert_eq!(requests[0].method, Method::Get);
        assert_eq!(requests[1].method, Method::Put);
        assert_eq!(requests[2].method, Method::Delete);
        assert_eq!(
            url::Url::parse(&requests[0].url).unwrap().path(),
            "/v1/me/library/contains"
        );
        assert!(
            requests[1..]
                .iter()
                .all(|request| url::Url::parse(&request.url).unwrap().path() == "/v1/me/library")
        );
        assert!(requests.iter().all(|request| {
            let url = url::Url::parse(&request.url).unwrap();
            url.query_pairs()
                .any(|pair| pair == ("uris".into(), "spotify:artist:ab".into()))
        }));
    }

    #[tokio::test]
    async fn artist_follow_contains_requires_exactly_one_boolean() {
        for response in [serde_json::json!([]), serde_json::json!([true, false])] {
            let client = SpotifyClient::new(
                "client",
                FakeTransport::new([Response::json(200, response)]),
                tokens(),
            );

            assert!(matches!(
                client.is_following_artist("artist").await,
                Err(Error::Json { .. })
            ));
        }

        let client = SpotifyClient::new(
            "client",
            FakeTransport::new([Response::json(200, serde_json::json!([false]))]),
            tokens(),
        );
        assert!(!client.is_following_artist("artist").await.unwrap());
    }

    #[tokio::test]
    async fn remove_from_library_sends_track_delete() {
        let client = SpotifyClient::new(
            "client",
            FakeTransport::new([Response::json(204, serde_json::Value::Null)]),
            tokens(),
        );

        client
            .remove_from_library(&["spotify:track:track".into()])
            .await
            .unwrap();

        let request = &client.transport().requests()[0];
        assert_eq!(request.method, Method::Delete);
        let url = url::Url::parse(&request.url).unwrap();
        assert_eq!(url.path(), "/v1/me/library");
        assert_eq!(
            url.query_pairs().collect::<Vec<_>>(),
            [("uris".into(), "spotify:track:track".into())]
        );
    }

    #[tokio::test]
    async fn library_writes_keep_album_and_track_memberships_distinct() {
        let client = SpotifyClient::new(
            "client",
            FakeTransport::new([
                Response::json(204, serde_json::Value::Null),
                Response::json(204, serde_json::Value::Null),
            ]),
            tokens(),
        );

        client
            .save_to_library(&["spotify:album:album".into()])
            .await
            .unwrap();
        client
            .save_to_library(&["spotify:track:track".into()])
            .await
            .unwrap();

        let requests = client.transport().requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            url::Url::parse(&requests[0].url)
                .unwrap()
                .query_pairs()
                .collect::<Vec<_>>(),
            [("uris".into(), "spotify:album:album".into())]
        );
        assert_eq!(
            url::Url::parse(&requests[1].url)
                .unwrap()
                .query_pairs()
                .collect::<Vec<_>>(),
            [("uris".into(), "spotify:track:track".into())]
        );
    }

    #[tokio::test]
    async fn local_uris_are_rejected_before_spotify_writes() {
        let client = SpotifyClient::new("client", FakeTransport::default(), tokens());
        let uris = vec!["spotify:track:track".into(), "file:///tmp/local.mp3".into()];

        assert!(matches!(
            client.play(None, &uris, 0).await,
            Err(Error::InvalidRequest(_))
        ));
        assert!(matches!(
            client.add_playlist_tracks("playlist", &uris, None).await,
            Err(Error::InvalidRequest(_))
        ));
        assert!(matches!(
            client
                .remove_playlist_tracks("playlist", &uris, "snapshot")
                .await,
            Err(Error::InvalidRequest(_))
        ));
        assert!(matches!(
            client.save_to_library(&uris).await,
            Err(Error::InvalidRequest(_))
        ));
        assert!(matches!(
            client.remove_from_library(&uris).await,
            Err(Error::InvalidRequest(_))
        ));
        assert!(client.transport().requests().is_empty());
    }

    #[tokio::test]
    async fn malformed_path_ids_are_rejected_before_transport() {
        #[derive(Clone, Copy, Debug)]
        enum Endpoint {
            UnfollowPlaylist,
            PlaylistTracks,
            AddPlaylistTracks,
            ReorderPlaylistTracks,
            RemovePlaylistTracks,
            ShowEpisodes,
            AudiobookChapters,
            Artist,
            ArtistAlbums,
            IsFollowingArtist,
            FollowArtist,
            Album,
            AlbumTracks,
            Track,
        }

        let cases = [
            (Endpoint::UnfollowPlaylist, "", "playlist"),
            (
                Endpoint::PlaylistTracks,
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "playlist",
            ),
            (
                Endpoint::AddPlaylistTracks,
                "spotify:playlist:abc",
                "playlist",
            ),
            (Endpoint::ReorderPlaylistTracks, "bad/id", "playlist"),
            (Endpoint::RemovePlaylistTracks, "bad?id", "playlist"),
            (Endpoint::ShowEpisodes, "bad#id", "show"),
            (Endpoint::AudiobookChapters, "bad-id", "audiobook"),
            (Endpoint::Artist, "artist:id", "artist"),
            (Endpoint::ArtistAlbums, "artist id", "artist"),
            (Endpoint::IsFollowingArtist, "artist/id", "artist"),
            (Endpoint::FollowArtist, "artist?id", "artist"),
            (Endpoint::Album, "/album", "album"),
            (Endpoint::AlbumTracks, "album?", "album"),
            (Endpoint::Track, "track#", "track"),
        ];
        let client = SpotifyClient::new("client", FakeTransport::default(), tokens());
        let uris = ["spotify:track:track".into()];

        for (endpoint, id, kind) in cases {
            let error = match endpoint {
                Endpoint::UnfollowPlaylist => client.unfollow_playlist(id).await,
                Endpoint::PlaylistTracks => client.playlist_tracks(id, 0, 1).await.map(|_| ()),
                Endpoint::AddPlaylistTracks => client
                    .add_playlist_tracks(id, &uris, None)
                    .await
                    .map(|_| ()),
                Endpoint::ReorderPlaylistTracks => client
                    .reorder_playlist_tracks(id, 0, 1, 1, "snapshot")
                    .await
                    .map(|_| ()),
                Endpoint::RemovePlaylistTracks => client
                    .remove_playlist_tracks(id, &uris, "snapshot")
                    .await
                    .map(|_| ()),
                Endpoint::ShowEpisodes => client.show_episodes(id, 0, 1).await.map(|_| ()),
                Endpoint::AudiobookChapters => {
                    client.audiobook_chapters(id, 0, 1).await.map(|_| ())
                }
                Endpoint::Artist => client.artist(id).await.map(|_| ()),
                Endpoint::ArtistAlbums => client.artist_albums(id, 0, 1).await.map(|_| ()),
                Endpoint::IsFollowingArtist => client.is_following_artist(id).await.map(|_| ()),
                Endpoint::FollowArtist => client.follow_artist(id, true).await,
                Endpoint::Album => client.album(id).await.map(|_| ()),
                Endpoint::AlbumTracks => client.album_tracks(id, 0, 1).await.map(|_| ()),
                Endpoint::Track => client.track(id).await.map(|_| ()),
            }
            .expect_err("malformed path ID must fail");

            assert!(
                matches!(&error, Error::InvalidRequest(message) if message.contains(kind)),
                "{endpoint:?} returned {error}"
            );
            assert!(client.transport().requests().is_empty());
        }
    }

    #[tokio::test]
    async fn player_writes_have_expected_shapes() {
        let transport = FakeTransport::new([
            Response::json(
                200,
                serde_json::json!({"devices": [{
                    "id": "desk", "name": "Desk", "is_restricted": false,
                    "type": "Computer", "is_active": true
                }]}),
            ),
            Response::json(204, serde_json::json!(null)),
            Response::json(204, serde_json::json!(null)),
            Response::json(204, serde_json::json!(null)),
            Response::json(204, serde_json::json!(null)),
            Response::json(204, serde_json::json!(null)),
            Response::json(200, serde_json::json!(null)),
            Response::json(200, serde_json::json!(null)),
        ]);
        let client = SpotifyClient::new("client", transport, tokens());
        let devices = client.devices().await.unwrap();
        assert_eq!(devices[0].device_type, "Computer");
        assert!(devices[0].is_active);
        assert!(!devices[0].is_restricted);
        client
            .play(
                Some("desk & one"),
                &["spotify:track:1".into(), "spotify:track:2".into()],
                1,
            )
            .await
            .unwrap();
        client
            .set_repeat("context", Some("desk & one"))
            .await
            .unwrap();
        client.resume(None).await.unwrap();
        client.pause(None).await.unwrap();
        client.set_volume(42, Some("desk & one")).await.unwrap();
        client.transfer("desk", true).await.unwrap();
        client
            .save_to_library(&["spotify:album:1".into()])
            .await
            .unwrap();
        let requests = client.transport().requests();
        assert!(
            requests[1]
                .url
                .ends_with("/me/player/play?device_id=desk+%26+one")
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&requests[1].body).unwrap(),
            serde_json::json!({
                "uris": ["spotify:track:1", "spotify:track:2"],
                "offset": { "position": 1 }
            })
        );
        assert!(
            requests[2]
                .url
                .ends_with("/me/player/repeat?state=context&device_id=desk+%26+one")
        );
        assert_eq!(requests[3].body, Vec::<u8>::new());
        assert!(requests[4].url.ends_with("/me/player/pause"));
        assert!(
            requests[5]
                .url
                .ends_with("/me/player/volume?volume_percent=42&device_id=desk+%26+one")
        );
        assert_eq!(requests[6].url, format!("{API_BASE}/me/player"));
        let url = url::Url::parse(&requests[7].url).unwrap();
        assert_eq!(url.path(), "/v1/me/library");
        assert_eq!(
            url.query_pairs().collect::<Vec<_>>(),
            [("uris".into(), "spotify:album:1".into())]
        );
        assert!(requests[7].body.is_empty());
    }

    #[tokio::test]
    async fn save_to_library_chunks_forty_uris_without_reordering() {
        let transport = FakeTransport::new(std::iter::repeat_n(
            Response::json(204, serde_json::Value::Null),
            3,
        ));
        let client = SpotifyClient::new("client", transport, tokens());
        let uris = (0..81)
            .map(|index| format!("spotify:track:{index}"))
            .collect::<Vec<_>>();

        client.save_to_library(&uris).await.unwrap();

        let requests = client.transport().requests();
        assert_eq!(requests.len(), 3);
        assert!(requests.iter().all(|request| request.body.is_empty()));
        let sent = requests
            .iter()
            .flat_map(|request| {
                url::Url::parse(&request.url)
                    .unwrap()
                    .query_pairs()
                    .find(|(key, _)| key == "uris")
                    .unwrap()
                    .1
                    .split(',')
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        assert_eq!(sent, uris);
        assert_eq!(
            requests
                .iter()
                .map(|request| url::Url::parse(&request.url)
                    .unwrap()
                    .query_pairs()
                    .find(|(key, _)| key == "uris")
                    .unwrap()
                    .1
                    .split(',')
                    .count())
                .collect::<Vec<_>>(),
            [40, 40, 1]
        );
    }

    #[tokio::test]
    async fn library_write_limit_rejects_before_transport() {
        let client = SpotifyClient::new("client", FakeTransport::new([]), tokens());
        let uris = vec!["spotify:track:one".to_string(); MAX_LIBRARY_WRITE_URIS + 1];

        assert!(client.save_to_library(&uris).await.is_err());

        assert!(client.transport().requests().is_empty());
    }

    #[tokio::test]
    async fn playlist_pages_mark_ownership_and_skip_local_or_null_tracks() {
        let transport = FakeTransport::new([
            Response::json(
                200,
                serde_json::json!({
                    "items": [{
                        "id": "mine", "name": "Mine", "snapshot_id": "s1",
                        "owner": {"id": "user"}, "items": {"total": 3}
                    }],
                    "next": "next", "total": 2
                }),
            ),
            Response::json(
                200,
                serde_json::json!({
                    "items": [{
                        "id": "theirs", "name": "Theirs", "snapshot_id": "s2", "collaborative": true,
                        "owner": {"id": "other"}, "items": {"total": 0}
                    }],
                    "next": null, "total": 2
                }),
            ),
            Response::json(
                200,
                serde_json::json!({
                    "items": [
                        {"is_local": false, "item": {
                            "uri": "spotify:track:1", "name": "One",
                            "artists": [{"id": "artist", "name": "Artist"}],
                            "album": {"id": "album", "uri": "spotify:album:album", "name": "Album", "images": []},
                            "duration_ms": 1234
                        }},
                        {"is_local": true, "item": {
                            "uri": "spotify:local:1", "name": "Local",
                            "artists": [], "album": null, "duration_ms": 1
                        }},
                        {"is_local": false, "item": null}
                    ],
                    "next": null, "total": 3
                }),
            ),
        ]);
        let client = SpotifyClient::new("client", transport, tokens());

        let first = client.playlists(0, 1, "user").await.unwrap();
        let second = client.playlists(1, 1, "user").await.unwrap();
        assert!(first.items[0].owned);
        assert!(!second.items[0].owned);
        assert!(second.items[0].collaborative);
        assert_eq!(first.items[0].tracks.total, 3);

        let tracks = client.playlist_tracks("mine", 0, 100).await.unwrap();
        assert_eq!(tracks.items.len(), 1);
        assert_eq!(tracks.skipped, 2);
        assert_eq!(tracks.items[0].uri, "spotify:track:1");

        let requests = client.transport().requests();
        assert!(requests[0].url.ends_with("/me/playlists?offset=0&limit=1"));
        let tracks_url = url::Url::parse(&requests[2].url).unwrap();
        assert_eq!(tracks_url.path(), "/v1/playlists/mine/items");
        assert_eq!(
            tracks_url
                .query_pairs()
                .find(|(key, _)| key == "limit")
                .unwrap()
                .1,
            "100"
        );
        assert!(
            tracks_url
                .query_pairs()
                .find(|(key, _)| key == "fields")
                .unwrap()
                .1
                .contains("is_local")
        );
    }

    #[tokio::test]
    async fn playlist_track_summary_does_not_hide_a_full_track_lookup() {
        let client = SpotifyClient::new(
            "client",
            FakeTransport::new([
                Response::json(
                    200,
                    serde_json::json!({
                        "items": [{"is_local": false, "item": {
                            "uri": "spotify:track:one", "name": "One",
                            "artists": [], "album": null, "duration_ms": 1234
                        }}],
                        "next": null, "total": 1
                    }),
                ),
                Response::json(
                    200,
                    serde_json::json!({
                        "uri": "spotify:track:one", "name": "One",
                        "artists": [], "album": null, "duration_ms": 1234,
                        "track_number": 7, "disc_number": 2
                    }),
                ),
            ]),
            InMemoryTokenStore::new(Some(Tokens {
                access: "access".into(),
                refresh: "refresh".into(),
                expires_at: u64::MAX,
                scopes: String::new(),
                playback_credentials: None,
            })),
        );

        let page = client.playlist_tracks("playlist", 0, 1).await.unwrap();
        assert_eq!(page.items[0].track_number, None);
        assert_eq!(page.items[0].disc_number, None);

        let track = client.track("one").await.unwrap();
        assert_eq!(track.track_number, Some(7));
        assert_eq!(track.disc_number, Some(2));
        assert_eq!(client.transport().requests().len(), 2);
        assert!(
            client.transport().requests()[1]
                .url
                .ends_with("/tracks/one")
        );
    }

    #[tokio::test]
    async fn playlist_create_posts_private_playlist() {
        let client = SpotifyClient::new(
            "client",
            FakeTransport::new([Response::json(
                201,
                serde_json::json!({
                    "id": "playlist", "name": "Road Trip", "snapshot_id": "snapshot"
                }),
            )]),
            tokens(),
        );

        let created = client.create_playlist("Road Trip").await.unwrap();

        assert_eq!(created.id, "playlist");
        let request = &client.transport().requests()[0];
        assert_eq!(request.method, Method::Post);
        assert!(request.url.ends_with("/me/playlists"));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&request.body).unwrap(),
            serde_json::json!({"name": "Road Trip", "public": false})
        );
    }

    #[tokio::test]
    async fn playlist_unfollow_sends_exact_delete() {
        let client = SpotifyClient::new(
            "client",
            FakeTransport::new([Response::json(200, serde_json::Value::Null)]),
            tokens(),
        );

        client.unfollow_playlist("playlist").await.unwrap();

        let requests = client.transport().requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, Method::Delete);
        assert_eq!(
            requests[0].url,
            format!("{API_BASE}/playlists/playlist/followers")
        );
        assert!(requests[0].body.is_empty());
    }

    #[tokio::test]
    async fn playlist_add_chunks_one_hundred_and_reorder_passes_snapshot() {
        let transport = FakeTransport::new([
            Response::json(201, serde_json::json!({"snapshot_id": "first"})),
            Response::json(201, serde_json::json!({"snapshot_id": "second"})),
            Response::json(200, serde_json::json!({"snapshot_id": "reordered"})),
        ]);
        let client = SpotifyClient::new("client", transport, tokens());
        let uris = (0..101)
            .map(|index| format!("spotify:track:{index}"))
            .collect::<Vec<_>>();

        assert_eq!(
            client
                .add_playlist_tracks("playlist", &uris, Some(5))
                .await
                .unwrap(),
            "second"
        );
        assert_eq!(
            client
                .reorder_playlist_tracks("playlist", 2, 9, 3, "second")
                .await
                .unwrap(),
            "reordered"
        );

        let requests = client.transport().requests();
        assert_eq!(requests.len(), 3);
        assert!(
            requests
                .iter()
                .all(|request| request.url.ends_with("/playlists/playlist/items"))
        );
        assert!(
            requests[..2]
                .iter()
                .all(|request| request.method == Method::Post)
        );
        let first: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let second: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(first["uris"].as_array().unwrap().len(), 100);
        assert_eq!(first["position"], 5);
        assert_eq!(second["uris"].as_array().unwrap().len(), 1);
        assert_eq!(second["position"], 105);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&requests[2].body).unwrap(),
            serde_json::json!({
                "range_start": 2,
                "insert_before": 9,
                "range_length": 3,
                "snapshot_id": "second"
            })
        );
    }

    #[tokio::test]
    async fn playlist_remove_chunks_one_hundred_and_chains_snapshot() {
        let transport = FakeTransport::new([
            Response::json(200, serde_json::json!({"snapshot_id": "first"})),
            Response::json(200, serde_json::json!({"snapshot_id": "second"})),
        ]);
        let client = SpotifyClient::new("client", transport, tokens());
        let uris = (0..101)
            .map(|index| format!("spotify:track:{index}"))
            .collect::<Vec<_>>();

        assert_eq!(
            client
                .remove_playlist_tracks("playlist", &uris, "original")
                .await
                .unwrap(),
            "second"
        );

        let requests = client.transport().requests();
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|request| {
            request.method == Method::Delete && request.url.ends_with("/playlists/playlist/items")
        }));
        let first: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let second: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(first["items"].as_array().unwrap().len(), 100);
        assert_eq!(
            first["items"][0],
            serde_json::json!({"uri": "spotify:track:0"})
        );
        assert_eq!(first["snapshot_id"], "original");
        assert_eq!(second["items"].as_array().unwrap().len(), 1);
        assert_eq!(second["snapshot_id"], "first");
    }

    #[tokio::test]
    async fn playlist_remove_sends_one_exact_request_for_one_chunk() {
        let client = SpotifyClient::new(
            "client",
            FakeTransport::new([Response::json(
                200,
                serde_json::json!({"snapshot_id": "removed"}),
            )]),
            tokens(),
        );
        let uris = vec!["spotify:track:1".into(), "spotify:track:2".into()];

        client
            .remove_playlist_tracks("playlist", &uris, "original")
            .await
            .unwrap();

        let requests = client.transport().requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, Method::Delete);
        assert!(requests[0].url.ends_with("/playlists/playlist/items"));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&requests[0].body).unwrap(),
            serde_json::json!({
                "items": [
                    {"uri": "spotify:track:1"},
                    {"uri": "spotify:track:2"}
                ],
                "snapshot_id": "original"
            })
        );
    }

    #[tokio::test]
    async fn playlist_write_surfaces_forbidden_status_and_message() {
        let transport = FakeTransport::new([Response::json(
            403,
            serde_json::json!({"error": {"status": 403, "message": "Insufficient client scope"}}),
        )]);
        let client = SpotifyClient::new("client", transport, tokens());

        let error = client
            .add_playlist_tracks("playlist", &["spotify:track:1".into()], None)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            Error::Http {
                status: 403,
                ref body,
                ..
            } if body.contains("Insufficient client scope")
        ));
    }

    #[tokio::test]
    async fn concurrent_unauthorized_requests_refresh_once() {
        let transport = FakeTransport::new([
            Response::json(401, serde_json::json!({"error": "expired"})),
            Response::json(
                200,
                serde_json::json!({"access_token": "new", "expires_in": 3600}),
            ),
            Response::json(200, serde_json::json!({"items": [], "next": null})),
            Response::json(200, serde_json::json!({"items": [], "next": null})),
        ]);
        let client = SpotifyClient::new("client", transport, tokens());
        let (left, right) = tokio::join!(client.saved_tracks(0, 1), client.saved_tracks(1, 1));
        left.unwrap();
        right.unwrap();
        assert_eq!(
            client
                .transport()
                .requests()
                .iter()
                .filter(|request| request.url == TOKEN_URL)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn complete_catalog_reads_skip_tokens_and_transport() {
        let catalog = Arc::new(Mutex::new(crate::catalog::SpotifyCatalog::default()));
        let artist = Artist {
            id: "artist1".into(),
            name: "Artist".into(),
            genres: vec!["rock".into()],
            followers: None,
            images: vec![],
        };
        let track = Track {
            uri: "spotify:track:track1".into(),
            name: "Track".into(),
            duration_ms: Some(100),
            track_number: Some(1),
            disc_number: Some(1),
            artists: vec![SimplifiedArtist {
                id: artist.id.clone(),
                name: artist.name.clone(),
            }],
            album: Some(AlbumSummary {
                id: "album1".into(),
                uri: "spotify:album:album1".into(),
                name: "Album".into(),
                release_date: Some("2024".into()),
                images: vec![],
            }),
        };
        let album = Album {
            id: "album1".into(),
            uri: "spotify:album:album1".into(),
            name: "Album".into(),
            artists: track.artists.clone(),
            images: vec![],
            release_date: Some("2024".into()),
            album_type: Some("album".into()),
            total_tracks: 1,
            tracks: Some(Page {
                items: vec![track.clone()],
                next: None,
                skipped: 0,
                total: 1,
            }),
        };
        {
            let mut cached = catalog.lock().unwrap();
            cached.observe_artist(&artist);
            cached.observe_track(&track);
            cached.observe_album(&album, true);
        }
        let client = SpotifyClient::new_with_catalog(
            "client",
            FakeTransport::new([]),
            InMemoryTokenStore::new(None),
            catalog,
        );

        assert_eq!(client.artist("artist1").await.unwrap(), artist);
        assert_eq!(client.track("track1").await.unwrap(), track);
        assert_eq!(client.album("album1").await.unwrap(), album);
        assert_eq!(
            client.album_tracks("album1", 0, 50).await.unwrap().items,
            vec![track]
        );
        assert!(client.transport().requests().is_empty());
    }

    #[tokio::test]
    async fn incomplete_track_fetches_once_then_uses_catalog() {
        let client = SpotifyClient::new(
            "client",
            FakeTransport::new([Response::json(
                200,
                serde_json::json!({
                    "uri": "spotify:track:one",
                    "name": "One",
                    "duration_ms": 10,
                    "artists": [],
                    "album": null
                }),
            )]),
            InMemoryTokenStore::new(Some(Tokens {
                access: "access".into(),
                refresh: "refresh".into(),
                expires_at: u64::MAX,
                scopes: String::new(),
                playback_credentials: None,
            })),
        );

        let first = client.track("one").await.unwrap();
        let second = client.track("one").await.unwrap();
        assert_eq!(first, second);
        assert_eq!(client.transport().requests().len(), 1);
    }

    #[test]
    fn endpoint_families_keep_saved_library_sections_distinct() {
        assert_eq!(endpoint_family("/me/tracks?offset=0"), "/me/tracks");
        assert_eq!(endpoint_family("/albums/abc/tracks"), "/albums");
        assert_eq!(endpoint_family("/artists/abc"), "/artists");
    }
}

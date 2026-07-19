use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, de::DeserializeOwned};
use tokio::sync::Mutex as AsyncMutex;

use crate::auth::TokenResponse;
use crate::tokens::{TokenStore, Tokens};
use crate::{Error, Result};

const API_BASE: &str = "https://api.spotify.com/v1";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const MAX_RATE_LIMIT_RETRIES: usize = 3;

pub type SendFuture<'a> = Pin<Box<dyn Future<Output = Result<Response>> + Send + 'a>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Put,
    Post,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: Method,
    pub url: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(status: u16, value: serde_json::Value) -> Self {
        Self {
            status,
            headers: HashMap::new(),
            body: serde_json::to_vec(&value).expect("JSON value serializes"),
        }
    }
}

pub trait Transport: Send + Sync {
    fn send(&self, request: Request) -> SendFuture<'_>;
}

#[derive(Clone, Default)]
pub struct HttpTransport(reqwest::Client);

impl HttpTransport {
    pub fn new() -> Self {
        Self(reqwest::Client::new())
    }
}

impl Transport for HttpTransport {
    fn send(&self, request: Request) -> SendFuture<'_> {
        Box::pin(async move {
            let method = match request.method {
                Method::Get => reqwest::Method::GET,
                Method::Put => reqwest::Method::PUT,
                Method::Post => reqwest::Method::POST,
            };
            let mut builder = self.0.request(method, request.url);
            for (name, value) in request.headers {
                builder = builder.header(name, value);
            }
            if !request.body.is_empty() {
                builder = builder.body(request.body);
            }
            let response = builder
                .send()
                .await
                .map_err(|error| Error::Transport(error.to_string()))?;
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|value| (name.as_str().to_ascii_lowercase(), value.to_owned()))
                })
                .collect();
            let body = response
                .bytes()
                .await
                .map_err(|error| Error::Transport(error.to_string()))?
                .to_vec();
            Ok(Response {
                status,
                headers,
                body,
            })
        })
    }
}

#[derive(Debug, Default)]
pub struct FakeTransport {
    responses: Mutex<VecDeque<Response>>,
    requests: Mutex<Vec<Request>>,
}

impl FakeTransport {
    pub fn new(responses: impl IntoIterator<Item = Response>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            requests: Mutex::default(),
        }
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().expect("fake mutex poisoned").clone()
    }
}

impl Transport for FakeTransport {
    fn send(&self, request: Request) -> SendFuture<'_> {
        Box::pin(async move {
            self.requests
                .lock()
                .map_err(|error| Error::Transport(error.to_string()))?
                .push(request);
            tokio::task::yield_now().await;
            self.responses
                .lock()
                .map_err(|error| Error::Transport(error.to_string()))?
                .pop_front()
                .ok_or_else(|| Error::Transport("fake response queue exhausted".into()))
        })
    }
}

pub struct SpotifyClient<T, S> {
    client_id: String,
    transport: T,
    tokens: S,
    artist_cache: Mutex<HashMap<String, Artist>>,
    refresh_lock: AsyncMutex<()>,
}

impl<T: Transport, S: TokenStore> SpotifyClient<T, S> {
    pub fn new(client_id: impl Into<String>, transport: T, tokens: S) -> Self {
        Self {
            client_id: client_id.into(),
            transport,
            tokens,
            artist_cache: Mutex::default(),
            refresh_lock: AsyncMutex::new(()),
        }
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    pub async fn saved_tracks(&self, offset: u32, limit: u32) -> Result<Page<SavedTrack>> {
        self.get(&paged("/me/tracks", offset, limit)).await
    }

    pub async fn saved_albums(&self, offset: u32, limit: u32) -> Result<Page<SavedAlbum>> {
        self.get(&paged("/me/albums", offset, limit)).await
    }

    pub async fn saved_shows(&self, offset: u32, limit: u32) -> Result<Page<SavedShow>> {
        self.get(&paged("/me/shows", offset, limit)).await
    }

    pub async fn show_episodes(
        &self,
        show_id: &str,
        offset: u32,
        limit: u32,
    ) -> Result<Page<Episode>> {
        self.get(&paged(&format!("/shows/{show_id}/episodes"), offset, limit))
            .await
    }

    pub async fn saved_episodes(&self, offset: u32, limit: u32) -> Result<Page<SavedEpisode>> {
        self.get(&paged("/me/episodes", offset, limit)).await
    }

    pub async fn saved_audiobooks(&self, offset: u32, limit: u32) -> Result<Page<Audiobook>> {
        self.get(&paged("/me/audiobooks", offset, limit)).await
    }

    pub async fn audiobook_chapters(
        &self,
        audiobook_id: &str,
        offset: u32,
        limit: u32,
    ) -> Result<Page<Chapter>> {
        self.get(&paged(
            &format!("/audiobooks/{audiobook_id}/chapters"),
            offset,
            limit,
        ))
        .await
    }

    pub async fn artist(&self, id: &str) -> Result<Artist> {
        if let Some(artist) = self
            .artist_cache
            .lock()
            .map_err(|error| Error::Transport(error.to_string()))?
            .get(id)
            .cloned()
        {
            return Ok(artist);
        }
        let artist: Artist = self.get(&format!("/artists/{id}")).await?;
        self.artist_cache
            .lock()
            .map_err(|error| Error::Transport(error.to_string()))?
            .insert(id.into(), artist.clone());
        Ok(artist)
    }

    pub async fn search(&self, query: &str, offset: u32, limit: u32) -> Result<SearchResults> {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("q", query)
            .append_pair("type", "artist,album")
            .append_pair("offset", &offset.to_string())
            .append_pair("limit", &limit.to_string())
            .finish();
        self.get(&format!("/search?{query}")).await
    }

    pub async fn artist_albums(
        &self,
        artist_id: &str,
        offset: u32,
        limit: u32,
    ) -> Result<Page<Album>> {
        self.get(&paged(
            &format!("/artists/{artist_id}/albums"),
            offset,
            limit,
        ))
        .await
    }

    pub async fn album_tracks(
        &self,
        album_id: &str,
        offset: u32,
        limit: u32,
    ) -> Result<Page<Track>> {
        self.get(&paged(&format!("/albums/{album_id}/tracks"), offset, limit))
            .await
    }

    pub async fn player(&self) -> Result<Option<PlayerState>> {
        let response = self
            .api_request(Method::Get, "/me/player", Vec::new())
            .await?;
        if response.status == 204 || response.body.is_empty() {
            return Ok(None);
        }
        decode("/me/player", &response.body).map(Some)
    }

    pub async fn devices(&self) -> Result<Vec<Device>> {
        self.get::<Devices>("/me/player/devices")
            .await
            .map(|response| response.devices)
    }

    pub async fn play(&self, device_id: Option<&str>, uris: &[String]) -> Result<()> {
        let path = device_path("/me/player/play", device_id);
        self.empty(
            Method::Put,
            &path,
            serde_json::to_vec(&serde_json::json!({ "uris": uris })).expect("play body serializes"),
        )
        .await
    }

    pub async fn resume(&self, device_id: Option<&str>) -> Result<()> {
        self.empty(
            Method::Put,
            &device_path("/me/player/play", device_id),
            Vec::new(),
        )
        .await
    }

    pub async fn pause(&self, device_id: Option<&str>) -> Result<()> {
        self.empty(
            Method::Put,
            &device_path("/me/player/pause", device_id),
            Vec::new(),
        )
        .await
    }

    pub async fn set_volume(&self, volume_percent: u8, device_id: Option<&str>) -> Result<()> {
        if volume_percent > 100 {
            return Err(Error::InvalidRequest(
                "volume percent must be between 0 and 100".into(),
            ));
        }
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query.append_pair("volume_percent", &volume_percent.to_string());
        if let Some(device_id) = device_id {
            query.append_pair("device_id", device_id);
        }
        self.empty(
            Method::Put,
            &format!("/me/player/volume?{}", query.finish()),
            Vec::new(),
        )
        .await
    }

    pub async fn transfer(&self, device_id: &str, play: bool) -> Result<()> {
        self.empty(
            Method::Put,
            "/me/player",
            serde_json::to_vec(&serde_json::json!({
                "device_ids": [device_id],
                "play": play
            }))
            .expect("transfer body serializes"),
        )
        .await
    }

    /// Must be verified against the February 2026 URI-based `/me/library`
    /// contract during the live feasibility gate.
    pub async fn save_to_library(&self, uri: &str) -> Result<()> {
        const PATH: &str = "/me/library";
        self.empty(
            Method::Put,
            PATH,
            serde_json::to_vec(&serde_json::json!({ "uris": [uri] }))
                .expect("library body serializes"),
        )
        .await
    }

    async fn get<R: DeserializeOwned>(&self, path: &str) -> Result<R> {
        let response = self.api_request(Method::Get, path, Vec::new()).await?;
        decode(path, &response.body)
    }

    async fn empty(&self, method: Method, path: &str, body: Vec<u8>) -> Result<()> {
        self.api_request(method, path, body).await.map(|_| ())
    }

    async fn api_request(&self, method: Method, path: &str, body: Vec<u8>) -> Result<Response> {
        let mut refreshed = false;
        let mut rate_retries = 0;
        loop {
            let access = self.tokens.load()?.ok_or(Error::MissingToken)?.access;
            let response = self
                .transport
                .send(Request {
                    method,
                    url: format!("{API_BASE}{path}"),
                    headers: HashMap::from([
                        ("authorization".into(), format!("Bearer {access}")),
                        ("content-type".into(), "application/json".into()),
                    ]),
                    body: body.clone(),
                })
                .await?;
            if response.status == 401 && !refreshed {
                self.refresh_token(&access).await?;
                refreshed = true;
                continue;
            }
            if response.status == 429 && rate_retries < MAX_RATE_LIMIT_RETRIES {
                rate_retries += 1;
                tokio::time::sleep(retry_after(&response.headers)).await;
                continue;
            }
            if !(200..300).contains(&response.status) {
                return Err(http_error(path, response));
            }
            return Ok(response);
        }
    }

    async fn refresh_token(&self, stale_access: &str) -> Result<()> {
        let _guard = self.refresh_lock.lock().await;
        let stored = self.tokens.load()?.ok_or(Error::MissingToken)?;
        if stored.access != stale_access {
            return Ok(());
        }
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", &self.client_id)
            .append_pair("grant_type", "refresh_token")
            .append_pair("refresh_token", &stored.refresh)
            .finish()
            .into_bytes();
        let response = self
            .transport
            .send(Request {
                method: Method::Post,
                url: TOKEN_URL.into(),
                headers: HashMap::from([(
                    "content-type".into(),
                    "application/x-www-form-urlencoded".into(),
                )]),
                body,
            })
            .await?;
        if !(200..300).contains(&response.status) {
            return Err(http_error(TOKEN_URL, response));
        }
        let token: TokenResponse = decode(TOKEN_URL, &response.body)?;
        self.tokens.save(&Tokens {
            access: token.access_token,
            refresh: token.refresh_token.unwrap_or(stored.refresh),
            expires_at: unix_now().saturating_add(token.expires_in),
        })
    }
}

fn decode<R: DeserializeOwned>(endpoint: &str, body: &[u8]) -> Result<R> {
    serde_json::from_slice(body).map_err(|source| Error::Json {
        endpoint: endpoint.into(),
        source,
    })
}

fn http_error(endpoint: &str, response: Response) -> Error {
    Error::Http {
        endpoint: endpoint.into(),
        status: response.status,
        body: String::from_utf8_lossy(&response.body).into_owned(),
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn retry_after(headers: &HashMap<String, String>) -> Duration {
    let Some(value) = headers.get("retry-after") else {
        return Duration::from_secs(1);
    };
    value
        .parse::<u64>()
        .map(Duration::from_secs)
        .unwrap_or_else(|_| {
            httpdate::parse_http_date(value)
                .ok()
                .and_then(|deadline| deadline.duration_since(SystemTime::now()).ok())
                .unwrap_or_default()
        })
}

fn paged(path: &str, offset: u32, limit: u32) -> String {
    format!("{path}?offset={offset}&limit={limit}")
}

fn device_path(path: &str, device_id: Option<&str>) -> String {
    device_id.map_or_else(
        || path.into(),
        |id| {
            let query = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("device_id", id)
                .finish();
            format!("{path}?{query}")
        },
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Image {
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SimplifiedArtist {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Artist {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub genres: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AlbumSummary {
    pub id: String,
    pub uri: String,
    pub name: String,
    #[serde(default)]
    pub images: Vec<Image>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Track {
    pub uri: String,
    pub name: String,
    pub duration_ms: u64,
    #[serde(default)]
    pub artists: Vec<SimplifiedArtist>,
    pub album: Option<AlbumSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SavedTrack {
    pub track: Track,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Album {
    pub id: String,
    pub uri: String,
    pub name: String,
    #[serde(default)]
    pub artists: Vec<SimplifiedArtist>,
    #[serde(default)]
    pub images: Vec<Image>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SavedAlbum {
    pub album: Album,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Show {
    pub id: String,
    pub uri: String,
    pub name: String,
    pub publisher: String,
    pub category: Option<String>,
    #[serde(default)]
    pub images: Vec<Image>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SavedShow {
    pub show: Show,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Episode {
    pub uri: String,
    pub name: String,
    pub duration_ms: u64,
    pub show: Option<Show>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SavedEpisode {
    pub episode: Episode,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Author {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Audiobook {
    pub id: String,
    pub uri: String,
    pub name: String,
    #[serde(default)]
    pub authors: Vec<Author>,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub images: Vec<Image>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Chapter {
    pub uri: String,
    pub name: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SearchResults {
    pub artists: Page<Artist>,
    pub albums: Page<Album>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Device {
    pub id: Option<String>,
    pub name: String,
    pub is_restricted: bool,
    #[serde(rename = "type")]
    pub device_type: String,
    pub is_active: bool,
}

#[derive(Debug, Deserialize)]
struct Devices {
    devices: Vec<Device>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PlayerState {
    pub is_playing: bool,
    pub progress_ms: Option<u64>,
    pub item: Option<Track>,
    pub device: Device,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::InMemoryTokenStore;

    fn tokens() -> InMemoryTokenStore {
        InMemoryTokenStore::new(Some(Tokens {
            access: "old".into(),
            refresh: "refresh".into(),
            expires_at: 0,
        }))
    }

    #[tokio::test]
    async fn saved_track_page_sends_bearer_and_decodes() {
        let transport = FakeTransport::new([Response::json(
            200,
            serde_json::json!({"items": [{"track": {
                "uri": "spotify:track:1", "name": "One", "duration_ms": 3,
                "artists": [], "album": null
            }}], "next": null}),
        )]);
        let client = SpotifyClient::new("client", transport, tokens());
        let page = client.saved_tracks(20, 10).await.unwrap();
        assert_eq!(page.items[0].track.name, "One");
        let requests = client.transport().requests();
        assert_eq!(
            requests[0].url,
            format!("{API_BASE}/me/tracks?offset=20&limit=10")
        );
        assert_eq!(requests[0].headers["authorization"], "Bearer old");
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
        let client = SpotifyClient::new("client", transport, tokens());
        client.saved_tracks(0, 50).await.unwrap();
        let requests = client.transport().requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[1].url, TOKEN_URL);
        assert_eq!(requests[2].headers["authorization"], "Bearer new");
    }

    #[tokio::test]
    async fn retries_rate_limit_and_reports_endpoint_for_bad_json() {
        let mut limited = Response::json(429, serde_json::json!({}));
        limited.headers.insert("retry-after".into(), "0".into());
        let transport = FakeTransport::new([
            limited,
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
        let limited = || {
            let mut response = Response::json(429, serde_json::json!({}));
            response.headers.insert("retry-after".into(), "0".into());
            response
        };
        let transport = FakeTransport::new([limited(), limited(), limited(), limited()]);
        let client = SpotifyClient::new("client", transport, tokens());
        let error = client.saved_tracks(0, 1).await.unwrap_err();
        assert!(matches!(error, Error::Http { status: 429, .. }));
        assert_eq!(client.transport().requests().len(), 4);
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
        ]);
        let client = SpotifyClient::new("client", transport, tokens());
        let devices = client.devices().await.unwrap();
        assert_eq!(devices[0].device_type, "Computer");
        assert!(devices[0].is_active);
        assert!(!devices[0].is_restricted);
        client
            .play(Some("desk & one"), &["spotify:track:1".into()])
            .await
            .unwrap();
        client.resume(None).await.unwrap();
        client.pause(None).await.unwrap();
        client.set_volume(42, Some("desk & one")).await.unwrap();
        client.transfer("desk", true).await.unwrap();
        client.save_to_library("spotify:album:1").await.unwrap();
        let requests = client.transport().requests();
        assert!(
            requests[1]
                .url
                .ends_with("/me/player/play?device_id=desk+%26+one")
        );
        assert_eq!(requests[2].body, Vec::<u8>::new());
        assert!(requests[3].url.ends_with("/me/player/pause"));
        assert!(
            requests[4]
                .url
                .ends_with("/me/player/volume?volume_percent=42&device_id=desk+%26+one")
        );
        assert_eq!(requests[5].url, format!("{API_BASE}/me/player"));
        assert_eq!(requests[6].url, format!("{API_BASE}/me/library"));
    }

    #[tokio::test]
    async fn concurrent_unauthorized_requests_refresh_once() {
        let transport = FakeTransport::new([
            Response::json(401, serde_json::json!({"error": "expired"})),
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
}

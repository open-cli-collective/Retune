use std::collections::{BTreeMap, HashMap, VecDeque};
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
const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(300);
#[cfg(not(test))]
const SERVER_RETRY_BACKOFFS: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(3)];
#[cfg(test)]
const SERVER_RETRY_BACKOFFS: [Duration; 2] = [Duration::ZERO; 2];

pub type SendFuture<'a> = Pin<Box<dyn Future<Output = Result<Response>> + Send + 'a>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Delete,
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

#[derive(Clone)]
pub struct HttpTransport(reqwest::Client);

impl HttpTransport {
    pub fn new() -> Self {
        Self(
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
        )
    }
}

impl Default for HttpTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl Transport for HttpTransport {
    fn send(&self, request: Request) -> SendFuture<'_> {
        Box::pin(async move {
            let method = match request.method {
                Method::Delete => reqwest::Method::DELETE,
                Method::Get => reqwest::Method::GET,
                Method::Put => reqwest::Method::PUT,
                Method::Post => reqwest::Method::POST,
            };
            let mut builder = self.0.request(method, request.url);
            for (name, value) in request.headers {
                builder = builder.header(name, value);
            }
            // Spotify's edge (Google front end) rejects PUT/POST without a
            // Content-Length header with HTTP 411, and attaching an empty body
            // does not guarantee the header on the wire — set it explicitly.
            if request.body.is_empty() {
                builder = builder.header(reqwest::header::CONTENT_LENGTH, 0);
            }
            builder = builder.body(request.body);
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
    request_counts: Mutex<BTreeMap<String, u64>>,
    refresh_lock: AsyncMutex<()>,
}

impl<T: Transport, S: TokenStore> SpotifyClient<T, S> {
    pub fn new(client_id: impl Into<String>, transport: T, tokens: S) -> Self {
        Self {
            client_id: client_id.into(),
            transport,
            tokens,
            artist_cache: Mutex::default(),
            request_counts: Mutex::default(),
            refresh_lock: AsyncMutex::new(()),
        }
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    pub fn reset_request_counts(&self) {
        self.request_counts
            .lock()
            .expect("request count mutex poisoned")
            .clear();
    }

    pub fn request_counts(&self) -> BTreeMap<String, u64> {
        self.request_counts
            .lock()
            .expect("request count mutex poisoned")
            .clone()
    }

    pub async fn access_token(&self) -> Result<String> {
        let stored = self.tokens.load()?.ok_or(Error::MissingToken)?;
        if stored.expires_at <= unix_now() {
            self.refresh_token(&stored.access).await?;
        }
        Ok(self.tokens.load()?.ok_or(Error::MissingToken)?.access)
    }

    pub async fn saved_tracks(&self, offset: u32, limit: u32) -> Result<Page<SavedTrack>> {
        self.get(&paged("/me/tracks", offset, limit)).await
    }

    pub async fn me(&self) -> Result<Profile> {
        self.get("/me").await
    }

    pub async fn playlists(
        &self,
        offset: u32,
        limit: u32,
        current_user_id: &str,
    ) -> Result<Page<Playlist>> {
        let mut page: Page<Playlist> = self.get(&paged("/me/playlists", offset, limit)).await?;
        for playlist in &mut page.items {
            playlist.owned = playlist.owner.id == current_user_id;
        }
        Ok(page)
    }

    pub async fn create_playlist(&self, name: &str) -> Result<CreatedPlaylist> {
        self.json(
            Method::Post,
            "/me/playlists",
            serde_json::to_vec(&CreatePlaylist {
                name,
                public: false,
            })
            .expect("playlist create body serializes"),
        )
        .await
    }

    pub async fn unfollow_playlist(&self, playlist_id: &str) -> Result<()> {
        self.empty(
            Method::Delete,
            &format!("/playlists/{playlist_id}/followers"),
            Vec::new(),
        )
        .await
    }

    pub async fn playlist_tracks(
        &self,
        playlist_id: &str,
        offset: u32,
        limit: u32,
    ) -> Result<Page<Track>> {
        let fields = "items(is_local,item(uri,name,artists(id,name),album(id,uri,name,images(url)),duration_ms)),next,total";
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("offset", &offset.to_string())
            .append_pair("limit", &limit.to_string())
            .append_pair("fields", fields)
            .finish();
        let page: Page<PlaylistTrackItem> = self
            .get(&format!("/playlists/{playlist_id}/items?{query}"))
            .await?;
        let total = page.total;
        let next = page.next;
        let mut skipped = page.skipped;
        let items = page
            .items
            .into_iter()
            .filter_map(|item| {
                if item.is_local || item.track.is_none() {
                    skipped += 1;
                    None
                } else {
                    item.track
                }
            })
            .collect();
        Ok(Page {
            items,
            next,
            skipped,
            total,
        })
    }

    pub async fn add_playlist_tracks(
        &self,
        playlist_id: &str,
        uris: &[String],
        position: Option<u32>,
    ) -> Result<String> {
        if uris.is_empty() {
            return Err(Error::InvalidRequest(
                "playlist add requires at least one URI".into(),
            ));
        }
        let mut snapshot_id = String::new();
        for (chunk_index, chunk) in uris.chunks(100).enumerate() {
            let position =
                position.map(|position| position.saturating_add((chunk_index * 100) as u32));
            let response: SnapshotResponse = self
                .json(
                    Method::Post,
                    &format!("/playlists/{playlist_id}/items"),
                    serde_json::to_vec(&AddPlaylistTracks {
                        uris: chunk,
                        position,
                    })
                    .expect("playlist add body serializes"),
                )
                .await?;
            snapshot_id = response.snapshot_id;
        }
        Ok(snapshot_id)
    }

    pub async fn reorder_playlist_tracks(
        &self,
        playlist_id: &str,
        range_start: u32,
        insert_before: u32,
        range_length: u32,
        snapshot_id: &str,
    ) -> Result<String> {
        self.json::<SnapshotResponse>(
            Method::Put,
            &format!("/playlists/{playlist_id}/items"),
            serde_json::to_vec(&ReorderPlaylistTracks {
                range_start,
                insert_before,
                range_length,
                snapshot_id,
            })
            .expect("playlist reorder body serializes"),
        )
        .await
        .map(|response| response.snapshot_id)
    }

    pub async fn remove_playlist_tracks(
        &self,
        playlist_id: &str,
        uris: &[String],
        snapshot_id: &str,
    ) -> Result<String> {
        if uris.is_empty() {
            return Err(Error::InvalidRequest(
                "playlist remove requires at least one URI".into(),
            ));
        }
        let mut snapshot_id = snapshot_id.to_owned();
        for chunk in uris.chunks(100) {
            snapshot_id = self
                .json::<SnapshotResponse>(
                    Method::Delete,
                    &format!("/playlists/{playlist_id}/items"),
                    serde_json::to_vec(&RemovePlaylistTracks {
                        items: chunk.iter().map(|uri| PlaylistTrackUri { uri }).collect(),
                        snapshot_id: &snapshot_id,
                    })
                    .expect("playlist remove body serializes"),
                )
                .await?
                .snapshot_id;
        }
        Ok(snapshot_id)
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

    pub async fn album(&self, id: &str) -> Result<Album> {
        let mut album: Album = self.get(&format!("/albums/{id}")).await?;
        let Some(mut tracks) = album.tracks.take() else {
            return Ok(album);
        };
        let mut offset = (tracks.items.len() + tracks.skipped) as u32;
        while tracks.next.is_some() && offset < tracks.total {
            let page = self.album_tracks(id, offset, 50).await?;
            let count = (page.items.len() + page.skipped) as u32;
            tracks.items.extend(page.items);
            tracks.skipped += page.skipped;
            tracks.next = page.next;
            tracks.total = page.total;
            if count == 0 {
                break;
            }
            offset += count;
        }
        album.tracks = Some(tracks);
        Ok(album)
    }

    pub async fn track(&self, id: &str) -> Result<Track> {
        self.get(&format!("/tracks/{id}")).await
    }

    pub async fn artist_top_tracks(&self, id: &str) -> Result<Vec<Track>> {
        self.get::<TopTracks>(&format!("/artists/{id}/top-tracks"))
            .await
            .map(|response| response.tracks)
    }

    pub async fn is_following_artist(&self, id: &str) -> Result<bool> {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("type", "artist")
            .append_pair("ids", id)
            .finish();
        self.get::<Vec<bool>>(&format!("/me/following/contains?{query}"))
            .await
            .map(|following| following.first().copied().unwrap_or(false))
    }

    pub async fn follow_artist(&self, id: &str, follow: bool) -> Result<()> {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("type", "artist")
            .append_pair("ids", id)
            .finish();
        self.empty(
            if follow { Method::Put } else { Method::Delete },
            &format!("/me/following?{query}"),
            Vec::new(),
        )
        .await
    }

    pub async fn remove_saved_album(&self, id: &str) -> Result<()> {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("ids", id)
            .finish();
        self.empty(Method::Delete, &format!("/me/albums?{query}"), Vec::new())
            .await
    }

    pub async fn search(&self, query: &str, offset: u32, limit: u32) -> Result<SearchResults> {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("q", query)
            .append_pair("type", "artist,album,track")
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

    pub async fn play(
        &self,
        device_id: Option<&str>,
        uris: &[String],
        offset: usize,
    ) -> Result<()> {
        if offset >= uris.len() {
            return Err(Error::InvalidRequest(
                "play offset is outside the queue".into(),
            ));
        }
        let path = device_path("/me/player/play", device_id);
        self.empty(
            Method::Put,
            &path,
            serde_json::to_vec(&serde_json::json!({
                "uris": uris,
                "offset": { "position": offset }
            }))
            .expect("play body serializes"),
        )
        .await
    }

    pub async fn set_repeat(&self, state: &str, device_id: Option<&str>) -> Result<()> {
        if !matches!(state, "off" | "context" | "track") {
            return Err(Error::InvalidRequest("invalid repeat state".into()));
        }
        let path = {
            let mut query = url::form_urlencoded::Serializer::new(String::new());
            query.append_pair("state", state);
            if let Some(device_id) = device_id {
                query.append_pair("device_id", device_id);
            }
            format!("/me/player/repeat?{}", query.finish())
        };
        self.empty(Method::Put, &path, Vec::new()).await
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

    pub async fn seek(&self, position_ms: u32, device_id: Option<&str>) -> Result<()> {
        // Scoped: the Serializer holds a non-Sync dyn Fn and must drop
        // before the await so command futures stay Send.
        let query = {
            let mut query = url::form_urlencoded::Serializer::new(String::new());
            query.append_pair("position_ms", &position_ms.to_string());
            if let Some(device_id) = device_id {
                query.append_pair("device_id", device_id);
            }
            query.finish()
        };
        self.empty(Method::Put, &format!("/me/player/seek?{query}"), Vec::new())
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

    pub async fn save_to_library(&self, uris: &[String]) -> Result<()> {
        for chunk in uris.chunks(40) {
            let query = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("uris", &chunk.join(","))
                .finish();
            self.empty(Method::Put, &format!("/me/library?{query}"), Vec::new())
                .await?;
        }
        Ok(())
    }

    async fn get<R: DeserializeOwned>(&self, path: &str) -> Result<R> {
        let response = self.api_request(Method::Get, path, Vec::new()).await?;
        decode(path, &response.body)
    }

    async fn json<R: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Vec<u8>,
    ) -> Result<R> {
        let response = self.api_request(method, path, body).await?;
        decode(path, &response.body)
    }

    async fn empty(&self, method: Method, path: &str, body: Vec<u8>) -> Result<()> {
        self.api_request(method, path, body).await.map(|_| ())
    }

    async fn api_request(&self, method: Method, path: &str, body: Vec<u8>) -> Result<Response> {
        let mut refreshed = false;
        let mut rate_retries = 0;
        let mut server_retries = 0;
        loop {
            let access = self.tokens.load()?.ok_or(Error::MissingToken)?.access;
            *self
                .request_counts
                .lock()
                .map_err(|error| Error::Transport(error.to_string()))?
                .entry(endpoint_family(path))
                .or_default() += 1;
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
            if response.status == 429 {
                let wait = retry_after(&response.headers);
                if wait > MAX_RATE_LIMIT_WAIT || rate_retries >= MAX_RATE_LIMIT_RETRIES {
                    log::warn!(
                        "Spotify rate limited {path}; retry after {}s (not retrying)",
                        wait.as_secs()
                    );
                    return Err(Error::RateLimited {
                        endpoint: path.into(),
                        retry_after_secs: wait.as_secs(),
                    });
                }
                rate_retries += 1;
                log::warn!(
                    "Spotify rate limited {path}; waiting {}s (attempt {}/{})",
                    wait.as_secs(),
                    rate_retries,
                    MAX_RATE_LIMIT_RETRIES
                );
                tokio::time::sleep(wait).await;
                continue;
            }
            if matches!(response.status, 500 | 502 | 503 | 504) {
                if server_retries == SERVER_RETRY_BACKOFFS.len() {
                    return Err(Error::ServerError {
                        endpoint: path.into(),
                        status: response.status,
                    });
                }
                let wait = SERVER_RETRY_BACKOFFS[server_retries];
                server_retries += 1;
                log::warn!(
                    "Spotify {path} returned HTTP {}; retrying in {}s (attempt {}/{})",
                    response.status,
                    wait.as_secs(),
                    server_retries,
                    SERVER_RETRY_BACKOFFS.len()
                );
                tokio::time::sleep(wait).await;
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
        log::info!("Refreshing Spotify access token");
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
            scopes: stored.scopes,
        })?;
        log::info!("Refreshed Spotify access token");
        Ok(())
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

pub fn endpoint_family(endpoint: &str) -> String {
    let mut segments = endpoint.split('?').next().unwrap_or(endpoint).split('/');
    let first = segments
        .find(|segment| !segment.is_empty())
        .unwrap_or_default();
    if first == "me" {
        segments
            .find(|segment| !segment.is_empty())
            .map_or_else(|| "/me".into(), |second| format!("/me/{second}"))
    } else {
        format!("/{first}")
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next: Option<String>,
    /// Items in the payload that failed to decode and were dropped. Live
    /// pages null out fields of removed/unplayable content; one bad item
    /// must not cost the page. Counted so pagination offsets stay correct.
    pub skipped: usize,
    pub total: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Profile {
    pub id: String,
    #[serde(default)]
    pub product: Option<String>,
}

impl Profile {
    pub fn is_premium(&self) -> bool {
        self.product.as_deref() == Some("premium")
    }
}

impl<'de, T: DeserializeOwned> Deserialize<'de> for Page<T> {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct RawPage {
            items: Vec<serde_json::Value>,
            next: Option<String>,
            #[serde(default)]
            total: Option<u32>,
        }
        let raw = RawPage::deserialize(deserializer)?;
        let mut items = Vec::with_capacity(raw.items.len());
        let mut skipped = 0;
        for item in raw.items {
            match serde_json::from_value(item) {
                Ok(item) => items.push(item),
                Err(error) => {
                    skipped += 1;
                    log::warn!("Skipped undecodable item in a Spotify page: {error}");
                }
            }
        }
        let total = raw.total.unwrap_or((items.len() + skipped) as u32);
        Ok(Self {
            items,
            next: raw.next,
            skipped,
            total,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Image {
    pub url: String,
    #[serde(default)]
    pub width: Option<u32>,
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
    #[serde(default)]
    pub followers: Option<Followers>,
    #[serde(default)]
    pub images: Vec<Image>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Followers {
    pub total: u64,
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
    /// Null in some live payloads (e.g. unplayable episodes).
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub track_number: Option<u32>,
    #[serde(default)]
    pub disc_number: Option<u32>,
    #[serde(default)]
    pub artists: Vec<SimplifiedArtist>,
    #[serde(default)]
    pub album: Option<AlbumSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PlaylistOwner {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PlaylistTrackCount {
    pub total: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub snapshot_id: String,
    pub owner: PlaylistOwner,
    #[serde(rename = "items", alias = "tracks")]
    pub tracks: PlaylistTrackCount,
    #[serde(skip)]
    pub owned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CreatedPlaylist {
    pub id: String,
    pub name: String,
    pub snapshot_id: String,
}

#[derive(Debug, Deserialize)]
struct PlaylistTrackItem {
    #[serde(default)]
    is_local: bool,
    #[serde(rename = "item", alias = "track")]
    track: Option<Track>,
}

#[derive(serde::Serialize)]
struct CreatePlaylist<'a> {
    name: &'a str,
    public: bool,
}

#[derive(serde::Serialize)]
struct AddPlaylistTracks<'a> {
    uris: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    position: Option<u32>,
}

#[derive(serde::Serialize)]
struct ReorderPlaylistTracks<'a> {
    range_start: u32,
    insert_before: u32,
    range_length: u32,
    snapshot_id: &'a str,
}

#[derive(serde::Serialize)]
struct RemovePlaylistTracks<'a> {
    items: Vec<PlaylistTrackUri<'a>>,
    snapshot_id: &'a str,
}

#[derive(serde::Serialize)]
struct PlaylistTrackUri<'a> {
    uri: &'a str,
}

#[derive(Deserialize)]
struct SnapshotResponse {
    snapshot_id: String,
}

pub type AlbumTrack = Track;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SavedTrack {
    #[serde(default, deserialize_with = "deserialize_added_at")]
    pub added_at: Option<u64>,
    pub track: Track,
}

fn deserialize_added_at<'de, D>(deserializer: D) -> std::result::Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)?
        .map(|value| {
            chrono::DateTime::parse_from_rfc3339(&value)
                .map_err(serde::de::Error::custom)
                .and_then(|date| u64::try_from(date.timestamp()).map_err(serde::de::Error::custom))
        })
        .transpose()
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
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub album_type: Option<String>,
    #[serde(default)]
    pub tracks: Option<Page<AlbumTrack>>,
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
    /// Absent from some live /me/shows payloads despite the documented shape.
    #[serde(default)]
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
    /// Null in some live payloads (e.g. unplayable episodes).
    #[serde(default)]
    pub duration_ms: Option<u64>,
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
    /// Null in some live payloads (e.g. unplayable episodes).
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub chapter_number: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SearchResults {
    pub artists: Page<Artist>,
    pub albums: Page<Album>,
    pub tracks: Page<Track>,
}

#[derive(Debug, Deserialize)]
struct TopTracks {
    tracks: Vec<Track>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Device {
    pub id: Option<String>,
    pub name: String,
    pub is_restricted: bool,
    #[serde(rename = "type")]
    pub device_type: String,
    pub is_active: bool,
    #[serde(default)]
    pub supports_volume: bool,
    pub volume_percent: Option<u8>,
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
            scopes: "streaming user-read-private".into(),
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
        assert_eq!(
            client.tokens.load().unwrap().unwrap().scopes,
            "streaming user-read-private"
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

        assert_eq!(client.access_token().await.unwrap(), "new");
        assert_eq!(client.transport().requests()[0].url, TOKEN_URL);
    }

    #[tokio::test]
    async fn profile_is_premium_only_for_explicit_premium_product() {
        let client = SpotifyClient::new(
            "client",
            FakeTransport::new([
                Response::json(200, serde_json::json!({"product": "premium", "id": "user"})),
                Response::json(200, serde_json::json!({"id": "user"})),
            ]),
            tokens(),
        );

        assert!(client.me().await.unwrap().is_premium());
        assert!(!client.me().await.unwrap().is_premium());
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
        assert!(matches!(error, Error::RateLimited { .. }));
        assert_eq!(client.transport().requests().len(), 4);
    }

    #[tokio::test]
    async fn long_rate_limit_returns_without_retrying() {
        let mut limited = Response::json(429, serde_json::json!({}));
        limited.headers.insert("retry-after".into(), "3600".into());
        let transport = FakeTransport::new([limited]);
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

    #[tokio::test]
    async fn retries_server_errors_twice_then_succeeds() {
        let transport = FakeTransport::new([
            Response::json(500, serde_json::json!({})),
            Response::json(502, serde_json::json!({})),
            Response::json(200, serde_json::json!({"items": [], "next": null})),
        ]);
        let client = SpotifyClient::new("client", transport, tokens());

        assert!(client.saved_albums(50, 50).await.is_ok());
        assert_eq!(client.transport().requests().len(), 3);
    }

    #[tokio::test]
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
        let requests = client.transport().requests();
        assert!(requests[0].url.ends_with("/albums/album"));
        assert!(
            requests[1]
                .url
                .ends_with("/albums/album/tracks?offset=1&limit=50")
        );
    }

    #[tokio::test]
    async fn artist_follow_contains_and_writes_use_query_parameters() {
        let transport = FakeTransport::new([
            Response::json(200, serde_json::json!([true])),
            Response::json(204, serde_json::Value::Null),
            Response::json(204, serde_json::Value::Null),
        ]);
        let client = SpotifyClient::new("client", transport, tokens());

        assert!(client.is_following_artist("a & b").await.unwrap());
        client.follow_artist("a & b", true).await.unwrap();
        client.follow_artist("a & b", false).await.unwrap();

        let requests = client.transport().requests();
        assert_eq!(requests[0].method, Method::Get);
        assert_eq!(requests[1].method, Method::Put);
        assert_eq!(requests[2].method, Method::Delete);
        assert!(requests.iter().all(|request| {
            let url = url::Url::parse(&request.url).unwrap();
            url.query_pairs()
                .any(|pair| pair == ("type".into(), "artist".into()))
                && url
                    .query_pairs()
                    .any(|pair| pair == ("ids".into(), "a & b".into()))
        }));
    }

    #[tokio::test]
    async fn remove_saved_album_sends_delete() {
        let client = SpotifyClient::new(
            "client",
            FakeTransport::new([Response::json(204, serde_json::Value::Null)]),
            tokens(),
        );

        client.remove_saved_album("album").await.unwrap();

        let request = &client.transport().requests()[0];
        assert_eq!(request.method, Method::Delete);
        let url = url::Url::parse(&request.url).unwrap();
        assert_eq!(url.path(), "/v1/me/albums");
        assert_eq!(
            url.query_pairs().collect::<Vec<_>>(),
            [("ids".into(), "album".into())]
        );
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
    async fn save_to_library_chunks_forty_uris_into_query_only_requests() {
        let transport = FakeTransport::new([
            Response::json(204, serde_json::Value::Null),
            Response::json(204, serde_json::Value::Null),
        ]);
        let client = SpotifyClient::new("client", transport, tokens());
        let uris = (0..41)
            .map(|index| format!("spotify:track:{index}"))
            .collect::<Vec<_>>();

        client.save_to_library(&uris).await.unwrap();

        let requests = client.transport().requests();
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|request| request.body.is_empty()));
        assert_eq!(
            url::Url::parse(&requests[0].url)
                .unwrap()
                .query_pairs()
                .find(|(key, _)| key == "uris")
                .unwrap()
                .1
                .split(',')
                .count(),
            40
        );
        assert_eq!(
            url::Url::parse(&requests[1].url)
                .unwrap()
                .query_pairs()
                .find(|(key, _)| key == "uris")
                .unwrap()
                .1,
            "spotify:track:40"
        );
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
                        "id": "theirs", "name": "Theirs", "snapshot_id": "s2",
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

    #[test]
    fn endpoint_families_keep_saved_library_sections_distinct() {
        assert_eq!(endpoint_family("/me/tracks?offset=0"), "/me/tracks");
        assert_eq!(endpoint_family("/albums/abc/tracks"), "/albums");
        assert_eq!(endpoint_family("/artists/abc"), "/artists");
    }
}

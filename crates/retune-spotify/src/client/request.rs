use super::*;

pub struct SpotifyClient<T, S> {
    pub(super) client_id: String,
    pub(super) transport: T,
    pub(super) tokens: S,
    pub(super) catalog: Arc<Mutex<SpotifyCatalog>>,
    pub(super) request_counts: Mutex<BTreeMap<String, u64>>,
    pub(super) refresh_lock: AsyncMutex<()>,
    pub(super) request_not_before: AsyncMutex<Option<Instant>>,
}

impl<T: Transport, S: TokenStore> SpotifyClient<T, S> {
    pub fn new(client_id: impl Into<String>, transport: T, tokens: S) -> Self {
        Self::new_with_catalog(
            client_id,
            transport,
            tokens,
            Arc::new(Mutex::new(SpotifyCatalog::default())),
        )
    }

    pub fn new_with_catalog(
        client_id: impl Into<String>,
        transport: T,
        tokens: S,
        catalog: Arc<Mutex<SpotifyCatalog>>,
    ) -> Self {
        Self {
            client_id: client_id.into(),
            transport,
            tokens,
            catalog,
            request_counts: Mutex::default(),
            refresh_lock: AsyncMutex::new(()),
            request_not_before: AsyncMutex::new(None),
        }
    }

    pub fn catalog(&self) -> Arc<Mutex<SpotifyCatalog>> {
        Arc::clone(&self.catalog)
    }

    pub fn clear_catalog(&self) {
        self.catalog
            .lock()
            .expect("Spotify catalog mutex poisoned")
            .clear();
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    pub fn token_store(&self) -> &S {
        &self.tokens
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
        if token_expired(stored.expires_at, unix_now()) {
            self.refresh_token(&stored.access).await?;
        }
        Ok(self.tokens.load()?.ok_or(Error::MissingToken)?.access)
    }

    pub(super) async fn get<R: DeserializeOwned>(&self, path: &str) -> Result<R> {
        let response = self.api_request(Method::Get, path, Vec::new()).await?;
        decode(path, &response.body)
    }

    pub(super) async fn json<R: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Vec<u8>,
    ) -> Result<R> {
        let response = self.api_request(method, path, body).await?;
        decode(path, &response.body)
    }

    pub(super) async fn empty(&self, method: Method, path: &str, body: Vec<u8>) -> Result<()> {
        self.api_request(method, path, body).await.map(|_| ())
    }

    pub(super) async fn api_request(
        &self,
        method: Method,
        path: &str,
        body: Vec<u8>,
    ) -> Result<Response> {
        let mut refreshed = false;
        let mut rate_retries = 0;
        let mut server_retries = 0;
        loop {
            let access = self.tokens.load()?.ok_or(Error::MissingToken)?.access;
            let response = match self
                .send_api_request(method, path, body.clone(), &access)
                .await
            {
                Ok(response) => response,
                Err(error)
                    if method != Method::Get
                        && matches!(error, Error::Transport(_) | Error::TransportSource { .. }) =>
                {
                    return Err(Error::AmbiguousMutation {
                        endpoint: path.into(),
                        status: None,
                        detail: error.to_string(),
                        source: Some(Box::new(error)),
                    });
                }
                Err(error) => return Err(error),
            };
            if response.status == 401 && !refreshed {
                self.refresh_token(&access).await?;
                refreshed = true;
                continue;
            }
            if response.status == 429 {
                let family = endpoint_family(path);
                if quota_exceeded(&response.body) {
                    let retry_after_secs =
                        retry_after_header(&response.headers).map(|wait| wait.as_secs());
                    log::warn!(
                        "Spotify request classified: family={family} kind=quota retry_delay={retry_after_secs:?} decision=return"
                    );
                    return Err(Error::QuotaExceeded {
                        endpoint: path.into(),
                        retry_after_secs,
                    });
                }
                let wait = retry_after(&response.headers);
                if wait > MAX_RATE_LIMIT_WAIT || rate_retries >= MAX_RATE_LIMIT_RETRIES {
                    log::warn!(
                        "Spotify request classified: family={family} kind=transient retry_delay={} decision=return",
                        wait.as_secs()
                    );
                    return Err(Error::RateLimited {
                        endpoint: path.into(),
                        retry_after_secs: wait.as_secs(),
                    });
                }
                rate_retries += 1;
                log::warn!(
                    "Spotify request classified: family={family} kind=transient retry_delay={} decision=retry attempt={rate_retries}/{MAX_RATE_LIMIT_RETRIES}",
                    wait.as_secs(),
                );
                continue;
            }
            if matches!(response.status, 500 | 502 | 503 | 504) {
                if method != Method::Get {
                    return Err(Error::AmbiguousMutation {
                        endpoint: path.into(),
                        status: Some(response.status),
                        detail: format!("HTTP {}", response.status),
                        source: None,
                    });
                }
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

    async fn send_api_request(
        &self,
        method: Method,
        path: &str,
        body: Vec<u8>,
        access: &str,
    ) -> Result<Response> {
        let mut not_before = self.request_not_before.lock().await;
        if let Some(deadline) = *not_before {
            tokio::time::sleep_until(deadline).await;
            *not_before = None;
        }
        *self
            .request_counts
            .lock()
            .expect("request count mutex poisoned")
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
                body,
            })
            .await?;
        if response.status == 429 && !quota_exceeded(&response.body) {
            let wait = retry_after(&response.headers);
            if wait <= MAX_RATE_LIMIT_WAIT {
                *not_before = Some(Instant::now() + wait);
            }
        }
        Ok(response)
    }

    async fn refresh_token(&self, stale_access: &str) -> Result<()> {
        let _guard = self.refresh_lock.lock().await;
        let stored = self.tokens.load()?.ok_or(Error::MissingToken)?;
        if stored.access != stale_access {
            return Ok(());
        }
        log::info!("Refreshing Spotify access token");
        let token =
            crate::auth::refresh_access_token(&self.transport, &self.client_id, &stored.refresh)
                .await?;
        let expires_at = unix_now().saturating_add(token.expires_in);
        let mut expected = stored;
        loop {
            let replacement = Tokens {
                access: token.access_token.clone(),
                refresh: token
                    .refresh_token
                    .clone()
                    .unwrap_or_else(|| expected.refresh.clone()),
                expires_at,
                scopes: expected.scopes.clone(),
                playback_credentials: expected.playback_credentials.clone(),
            };
            if self.tokens.replace_if_current(&expected, &replacement)? {
                log::info!("Refreshed Spotify access token");
                break;
            }
            let Some(current) = self.tokens.load()? else {
                log::info!("Discarded stale Spotify token refresh after disconnect");
                break;
            };
            if current.access != stale_access {
                log::info!("Discarded stale Spotify token refresh after grant replacement");
                break;
            }
            expected = current;
        }
        Ok(())
    }
}

pub(super) fn decode<R: DeserializeOwned>(endpoint: &str, body: &[u8]) -> Result<R> {
    serde_json::from_slice(body).map_err(|source| Error::Json {
        endpoint: endpoint.into(),
        source,
    })
}

fn http_error(endpoint: &str, response: Response) -> Error {
    Error::Http {
        endpoint: endpoint.into(),
        status: response.status,
        body: crate::bounded_error_body(&response.body),
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(super) fn token_expired(expires_at: u64, now: u64) -> bool {
    expires_at <= now
}

fn retry_after(headers: &HashMap<String, String>) -> Duration {
    retry_after_header(headers).unwrap_or(Duration::from_secs(1))
}

fn retry_after_header(headers: &HashMap<String, String>) -> Option<Duration> {
    retry_after_header_at(headers, SystemTime::now())
}

pub(super) fn retry_after_header_at(
    headers: &HashMap<String, String>,
    now: SystemTime,
) -> Option<Duration> {
    let value = headers.get("retry-after")?;
    value
        .parse::<u64>()
        .map(Duration::from_secs)
        .ok()
        .or_else(|| {
            httpdate::parse_http_date(value)
                .ok()
                .and_then(|deadline| deadline.duration_since(now).ok())
        })
}

#[derive(Deserialize)]
struct RegularErrorResponse {
    error: RegularError,
}

#[derive(Deserialize)]
struct RegularError {
    reason: Option<String>,
}

fn quota_exceeded(body: &[u8]) -> bool {
    serde_json::from_slice::<RegularErrorResponse>(body)
        .is_ok_and(|response| response.error.reason.as_deref() == Some("QUOTA_EXCEEDED"))
}

pub(super) fn paged(path: &str, offset: u32, limit: u32) -> String {
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

pub(super) fn player_path(path: &str, pairs: &[(&str, &str)], device_id: Option<&str>) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        query.append_pair(key, value);
    }
    if let Some(device_id) = device_id {
        query.append_pair("device_id", device_id);
    }
    let query = query.finish();
    if query.is_empty() {
        path.into()
    } else {
        format!("{path}?{query}")
    }
}

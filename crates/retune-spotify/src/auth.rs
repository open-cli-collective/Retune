use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    LazyLock,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;

use crate::client::{Method, Request, Transport};
use crate::{Error, Result};

pub const REQUIRED_SCOPES: [&str; 10] = [
    "user-library-read",
    "user-library-modify",
    "user-read-playback-state",
    "user-read-playback-position",
    "user-modify-playback-state",
    "user-read-private",
    "playlist-read-private",
    "playlist-read-collaborative",
    "playlist-modify-public",
    "playlist-modify-private",
];
pub static SCOPES: LazyLock<String> = LazyLock::new(|| REQUIRED_SCOPES.join(" "));
pub const PLAYBACK_SCOPE: &str = "streaming";
const AUTHORIZE_URL: &str = "https://accounts.spotify.com/authorize";
pub(crate) const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const TOKEN_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
pub const OAUTH_LOOPBACK_PORT: u16 = 8898;
pub const WEB_CALLBACK_PATH: &str = "/callback";
pub const PLAYBACK_CALLBACK_PATH: &str = "/login";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn generate() -> Self {
        let mut bytes = [0_u8; 64];
        rand::rng().fill_bytes(&mut bytes);
        Self::from_verifier(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
    }

    pub fn from_verifier(verifier: String) -> Self {
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()));
        Self {
            verifier,
            challenge,
        }
    }
}

pub fn random_state() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn authorize_url(
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
) -> Result<Url> {
    authorize_url_with_scopes(client_id, redirect_uri, state, challenge, &SCOPES)
}

pub fn authorize_url_with_scopes(
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
    scopes: &str,
) -> Result<Url> {
    let mut url = Url::parse(AUTHORIZE_URL).expect("Spotify authorize URL is constant");
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", scopes)
        .append_pair("state", state)
        .append_pair("code_challenge_method", "S256")
        .append_pair("code_challenge", challenge);
    Ok(url)
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(try_from = "TokenResponseWire")]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: u64,
    #[serde(default)]
    pub scope: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponseWire {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
    #[serde(default)]
    scope: Option<String>,
}

impl TryFrom<TokenResponseWire> for TokenResponse {
    type Error = &'static str;

    fn try_from(value: TokenResponseWire) -> std::result::Result<Self, Self::Error> {
        if value.access_token.trim().is_empty() {
            return Err("access_token must not be empty");
        }
        if value
            .refresh_token
            .as_deref()
            .is_some_and(|token| token.trim().is_empty())
        {
            return Err("refresh_token must not be empty");
        }
        Ok(Self {
            access_token: value.access_token,
            refresh_token: value.refresh_token,
            expires_in: value.expires_in,
            scope: value.scope,
        })
    }
}

pub async fn exchange_code<T: Transport>(
    transport: &T,
    client_id: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<TokenResponse> {
    token_request(
        transport,
        &[
            ("client_id", client_id),
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
        ],
    )
    .await
}

pub(crate) async fn refresh_access_token<T: Transport>(
    transport: &T,
    client_id: &str,
    refresh_token: &str,
) -> Result<TokenResponse> {
    token_request(
        transport,
        &[
            ("client_id", client_id),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ],
    )
    .await
}

async fn token_request<T: Transport>(
    transport: &T,
    form: &[(&str, &str)],
) -> Result<TokenResponse> {
    let body = {
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        for (key, value) in form {
            serializer.append_pair(key, value);
        }
        serializer.finish().into_bytes()
    };
    let response = tokio::time::timeout(
        TOKEN_REQUEST_TIMEOUT,
        transport.send(Request {
            method: Method::Post,
            url: TOKEN_URL.into(),
            headers: std::collections::HashMap::from([(
                "content-type".into(),
                "application/x-www-form-urlencoded".into(),
            )]),
            body,
        }),
    )
    .await
    .map_err(|_| Error::TokenRequestTimeout)??;
    if !(200..300).contains(&response.status) {
        return Err(Error::Http {
            endpoint: TOKEN_URL.into(),
            status: response.status,
            body: crate::bounded_error_body(&response.body),
        });
    }
    serde_json::from_slice(&response.body).map_err(|source| Error::Json {
        endpoint: TOKEN_URL.into(),
        source,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Callback {
    pub code: String,
    pub state: String,
}

pub struct LoopbackListener {
    listener: TcpListener,
}

impl LoopbackListener {
    /// Binds an OS-assigned ephemeral port — for tests. Real OAuth flows
    /// need [`Self::bind_on`]: Spotify matches redirect URIs exactly, so
    /// the port must be the one registered in the app dashboard.
    pub fn bind() -> Result<Self> {
        Self::bind_on(0)
    }

    pub fn bind_on(port: u16) -> Result<Self> {
        TcpListener::bind(("127.0.0.1", port))
            .map(|listener| Self { listener })
            .map_err(|error| Error::Callback(error.to_string()))
    }

    pub fn redirect_uri(&self) -> Result<String> {
        self.redirect_uri_for(WEB_CALLBACK_PATH)
    }

    pub fn redirect_uri_for(&self, path: &str) -> Result<String> {
        let port = self
            .listener
            .local_addr()
            .map_err(|error| Error::Callback(error.to_string()))?
            .port();
        loopback_redirect_uri(port, path)
    }

    pub fn accept(self, expected_state: &str, timeout: Duration) -> Result<Callback> {
        self.accept_path(expected_state, WEB_CALLBACK_PATH, timeout)
    }

    pub fn accept_path(
        self,
        expected_state: &str,
        expected_path: &str,
        timeout: Duration,
    ) -> Result<Callback> {
        self.accept_path_cancelled(
            expected_state,
            expected_path,
            timeout,
            &AtomicBool::new(false),
        )
    }

    pub fn accept_path_cancelled(
        self,
        expected_state: &str,
        expected_path: &str,
        timeout: Duration,
        cancelled: &AtomicBool,
    ) -> Result<Callback> {
        self.listener
            .set_nonblocking(true)
            .map_err(|error| Error::Callback(error.to_string()))?;
        let deadline = Instant::now() + timeout;
        loop {
            if cancelled.load(Ordering::Acquire) {
                return Err(Error::Callback("authorization cancelled".into()));
            }
            let remaining = deadline.checked_duration_since(Instant::now());
            let Some(remaining) = remaining.filter(|remaining| !remaining.is_zero()) else {
                return Err(Error::Timeout);
            };
            let (mut stream, _) = match self.listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(remaining.min(Duration::from_millis(10)));
                    continue;
                }
                Err(error) => return Err(Error::Callback(error.to_string())),
            };
            // Accepted sockets inherit the listener's non-blocking flag on
            // BSD/macOS; restore blocking so the read timeout governs and a
            // not-yet-arrived request isn't mistaken for a dead connection.
            stream
                .set_nonblocking(false)
                .and_then(|()| {
                    stream.set_read_timeout(Some(remaining.min(Duration::from_millis(10))))
                })
                .map_err(|error| Error::Callback(error.to_string()))?;
            // Read until the header terminator: the request line can arrive
            // split across segments, and a partial read misparses as malformed.
            let mut buffer = Vec::with_capacity(2048);
            let mut chunk = [0_u8; 2048];
            let complete = loop {
                match stream.read(&mut chunk) {
                    Ok(0) => break false,
                    Ok(read) => {
                        buffer.extend_from_slice(&chunk[..read]);
                        if buffer.windows(4).any(|w| w == b"\r\n\r\n") {
                            break true;
                        }
                        if buffer.len() > 16 * 1024 {
                            break false;
                        }
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                            break false;
                        }
                    }
                    Err(_) => break false,
                }
            };
            if !complete {
                continue;
            }
            let Ok(url) = callback_url(&String::from_utf8_lossy(&buffer)) else {
                let _ = respond(
                    &mut stream,
                    "400 Bad Request",
                    "Malformed callback request.",
                );
                continue;
            };
            if url.path() != expected_path {
                let _ = respond(&mut stream, "404 Not Found", "Not found.");
                continue;
            }
            let Some(state) = query_value(&url, "state") else {
                let _ = respond(&mut stream, "400 Bad Request", "Missing callback state.");
                continue;
            };
            if state != expected_state {
                let _ = respond(
                    &mut stream,
                    "400 Bad Request",
                    "Authorization failed: state mismatch.",
                );
                return Err(Error::StateMismatch);
            }
            if let Some(error) = query_value(&url, "error") {
                let description = query_value(&url, "error_description").unwrap_or(error);
                let body = format!("Authorization was denied: {description}");
                let _ = respond(&mut stream, "400 Bad Request", &body);
                return Err(Error::AccessDenied(description));
            }
            let Some(code) = query_value(&url, "code") else {
                let _ = respond(
                    &mut stream,
                    "400 Bad Request",
                    "Missing authorization code.",
                );
                continue;
            };
            let _ = respond(
                &mut stream,
                "200 OK",
                "Authorization complete. You can close this window.",
            );
            return Ok(Callback { code, state });
        }
    }
}

fn loopback_redirect_uri(port: u16, path: &str) -> Result<String> {
    if !path.starts_with('/') {
        return Err(Error::Callback("redirect path must start with '/'".into()));
    }
    Ok(format!("http://127.0.0.1:{port}{path}"))
}

fn callback_url(request: &str) -> Result<Url> {
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| Error::Callback("malformed HTTP request line".into()))?;
    Url::parse(&format!("http://127.0.0.1{target}"))
        .map_err(|error| Error::Callback(error.to_string()))
}

fn query_value(url: &Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find_map(|(key, value)| (key == name).then(|| value.into_owned()))
}

fn respond(stream: &mut TcpStream, status: &str, body: &str) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .and_then(|()| stream.flush())
    .map_err(|error| Error::Callback(error.to_string()))?;
    // Graceful close: shutdown our write side and drain whatever the peer
    // still has in flight. Dropping with unread bytes buffered sends RST,
    // which races the peer's read of our response (flaky on CI runners).
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
    let mut sink = [0_u8; 1024];
    while matches!(stream.read(&mut sink), Ok(n) if n > 0) {}
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::thread;
    use std::time::Duration;

    use crate::client::{FakeTransport, Response, SendFuture};

    use super::*;

    #[test]
    fn rfc_7636_appendix_b_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            Pkce::from_verifier(verifier.into()).challenge,
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn playback_authorize_url_requests_only_streaming() {
        let url = authorize_url_with_scopes(
            "client",
            "http://127.0.0.1:8898/login",
            "state",
            "challenge",
            PLAYBACK_SCOPE,
        )
        .unwrap();

        assert_eq!(url.path(), "/authorize");
        assert_eq!(
            url.query_pairs().find(|(key, _)| key == "scope").unwrap().1,
            PLAYBACK_SCOPE
        );
        assert_eq!(
            url.query_pairs()
                .find(|(key, _)| key == "redirect_uri")
                .unwrap()
                .1,
            "http://127.0.0.1:8898/login"
        );
    }

    #[test]
    fn web_authorize_url_requests_the_current_endpoint_scopes() {
        assert_eq!(
            REQUIRED_SCOPES,
            [
                "user-library-read",
                "user-library-modify",
                "user-read-playback-state",
                "user-read-playback-position",
                "user-modify-playback-state",
                "user-read-private",
                "playlist-read-private",
                "playlist-read-collaborative",
                "playlist-modify-public",
                "playlist-modify-private",
            ]
        );
    }

    #[test]
    fn registered_loopback_redirect_contracts_are_executable() {
        assert_eq!(
            loopback_redirect_uri(OAUTH_LOOPBACK_PORT, WEB_CALLBACK_PATH).unwrap(),
            "http://127.0.0.1:8898/callback"
        );
        assert_eq!(
            loopback_redirect_uri(OAUTH_LOOPBACK_PORT, PLAYBACK_CALLBACK_PATH).unwrap(),
            "http://127.0.0.1:8898/login"
        );
        assert!(loopback_redirect_uri(OAUTH_LOOPBACK_PORT, "login").is_err());
    }

    #[tokio::test]
    async fn code_exchange_encodes_pkce_form_and_decodes_success() {
        let transport = FakeTransport::new([Response::json(
            200,
            serde_json::json!({
                "access_token": "access",
                "refresh_token": "refresh",
                "expires_in": 3600,
                "scope": "streaming"
            }),
        )]);

        let token = exchange_code(
            &transport,
            "client id",
            "code +&",
            "http://127.0.0.1/callback?value=one two",
            "verifier +&",
        )
        .await
        .unwrap();

        assert_eq!(token.access_token, "access");
        assert_eq!(token.refresh_token.as_deref(), Some("refresh"));
        let request = &transport.requests()[0];
        assert_eq!(request.method, Method::Post);
        assert_eq!(request.url, TOKEN_URL);
        assert_eq!(
            request.headers["content-type"],
            "application/x-www-form-urlencoded"
        );
        assert_eq!(
            url::form_urlencoded::parse(&request.body)
                .into_owned()
                .collect::<HashMap<_, _>>(),
            HashMap::from([
                ("client_id".into(), "client id".into()),
                ("grant_type".into(), "authorization_code".into()),
                ("code".into(), "code +&".into()),
                (
                    "redirect_uri".into(),
                    "http://127.0.0.1/callback?value=one two".into()
                ),
                ("code_verifier".into(), "verifier +&".into()),
            ])
        );
    }

    #[tokio::test]
    async fn code_exchange_reports_http_json_and_transport_failures() {
        let http = exchange_code(
            &FakeTransport::new([Response::json(400, serde_json::json!({"error": "bad"}))]),
            "client",
            "code",
            "redirect",
            "verifier",
        )
        .await
        .unwrap_err();
        assert!(matches!(
            http,
            Error::Http {
                status: 400,
                ref endpoint,
                ..
            } if endpoint == TOKEN_URL
        ));

        let malformed = exchange_code(
            &FakeTransport::new([Response {
                status: 200,
                headers: HashMap::new(),
                body: b"not json".to_vec(),
            }]),
            "client",
            "code",
            "redirect",
            "verifier",
        )
        .await
        .unwrap_err();
        assert!(matches!(
            malformed,
            Error::Json { ref endpoint, .. } if endpoint == TOKEN_URL
        ));

        let transport = exchange_code(
            &FakeTransport::new([]),
            "client",
            "code",
            "redirect",
            "verifier",
        )
        .await
        .unwrap_err();
        assert!(matches!(transport, Error::Transport(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn token_exchange_times_out_an_unresponsive_transport() {
        struct NeverTransport;

        impl Transport for NeverTransport {
            fn send(&self, _request: Request) -> SendFuture<'_> {
                Box::pin(std::future::pending())
            }
        }

        let exchange = tokio::spawn(async {
            exchange_code(&NeverTransport, "client", "code", "redirect", "verifier").await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(TOKEN_REQUEST_TIMEOUT).await;
        tokio::task::yield_now().await;

        assert!(exchange.is_finished());
        assert!(matches!(
            exchange.await.unwrap(),
            Err(Error::TokenRequestTimeout)
        ));
    }

    #[tokio::test]
    async fn token_exchange_rejects_empty_secrets() {
        for response in [
            serde_json::json!({"access_token": " ", "expires_in": 3600}),
            serde_json::json!({
                "access_token": "access",
                "refresh_token": "",
                "expires_in": 3600
            }),
        ] {
            let error = exchange_code(
                &FakeTransport::new([Response::json(200, response)]),
                "client",
                "code",
                "redirect",
                "verifier",
            )
            .await
            .unwrap_err();
            assert!(matches!(
                error,
                Error::Json { ref endpoint, .. } if endpoint == TOKEN_URL
            ));
        }
    }

    #[test]
    fn callback_uses_a_real_loopback_request_and_checks_state() {
        let listener = LoopbackListener::bind().unwrap();
        let redirect = listener.redirect_uri().unwrap();
        let handle = thread::spawn(move || listener.accept("right", Duration::from_secs(1)));
        let url = Url::parse(&redirect).unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", url.port().unwrap())).unwrap();
        write!(
            stream,
            "GET /callback?code=a%2Bb&state=right HTTP/1.1\r\nHost: localhost\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert_eq!(
            handle.join().unwrap().unwrap(),
            Callback {
                code: "a+b".into(),
                state: "right".into()
            }
        );
    }

    #[test]
    fn callback_rejects_wrong_state() {
        let listener = LoopbackListener::bind().unwrap();
        let redirect = Url::parse(&listener.redirect_uri().unwrap()).unwrap();
        let handle = thread::spawn(move || listener.accept("right", Duration::from_secs(1)));
        let mut stream = TcpStream::connect(("127.0.0.1", redirect.port().unwrap())).unwrap();
        write!(
            stream,
            "GET /callback?code=x&state=wrong HTTP/1.1\r\nHost: localhost\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(matches!(handle.join().unwrap(), Err(Error::StateMismatch)));
    }

    #[test]
    fn callback_ignores_stray_request_then_accepts_valid_callback() {
        let listener = LoopbackListener::bind().unwrap();
        let redirect = Url::parse(&listener.redirect_uri().unwrap()).unwrap();
        let handle = thread::spawn(move || listener.accept("right", Duration::from_secs(1)));

        let mut stray = TcpStream::connect(("127.0.0.1", redirect.port().unwrap())).unwrap();
        write!(
            stray,
            "GET /favicon.ico HTTP/1.1\r\nHost: localhost\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stray.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 404 Not Found"));

        let mut callback = TcpStream::connect(("127.0.0.1", redirect.port().unwrap())).unwrap();
        write!(
            callback,
            "GET /callback?code=ok&state=right HTTP/1.1\r\nHost: localhost\r\n\r\n"
        )
        .unwrap();
        callback.read_to_string(&mut String::new()).unwrap();
        assert_eq!(handle.join().unwrap().unwrap().code, "ok");
    }

    #[test]
    fn callback_reports_access_denied() {
        let listener = LoopbackListener::bind().unwrap();
        let redirect = Url::parse(&listener.redirect_uri().unwrap()).unwrap();
        let handle = thread::spawn(move || listener.accept("right", Duration::from_secs(1)));
        let mut stream = TcpStream::connect(("127.0.0.1", redirect.port().unwrap())).unwrap();
        write!(
            stream,
            "GET /callback?error=access_denied&error_description=User+declined&state=right HTTP/1.1\r\nHost: localhost\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.contains("Authorization was denied: User declined"));
        assert!(matches!(
            handle.join().unwrap(),
            Err(Error::AccessDenied(_))
        ));
    }

    #[test]
    fn callback_accepts_the_playback_login_path() {
        let listener = LoopbackListener::bind().unwrap();
        let redirect = listener.redirect_uri_for("/login").unwrap();
        let handle =
            thread::spawn(move || listener.accept_path("right", "/login", Duration::from_secs(1)));
        let url = Url::parse(&redirect).unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", url.port().unwrap())).unwrap();
        write!(
            stream,
            "GET /login?code=ok&state=right HTTP/1.1\r\nHost: localhost\r\n\r\n"
        )
        .unwrap();
        stream.read_to_string(&mut String::new()).unwrap();
        assert_eq!(handle.join().unwrap().unwrap().code, "ok");
    }

    #[test]
    fn callback_times_out() {
        let listener = LoopbackListener::bind().unwrap();
        assert!(matches!(
            listener.accept("right", Duration::from_millis(10)),
            Err(Error::Timeout)
        ));
    }

    #[test]
    fn callback_times_out_after_a_client_connects_without_sending_a_request() {
        let listener = LoopbackListener::bind().unwrap();
        let redirect = Url::parse(&listener.redirect_uri().unwrap()).unwrap();
        let handle = thread::spawn(move || listener.accept("right", Duration::from_millis(100)));
        let _silent_client = TcpStream::connect(("127.0.0.1", redirect.port().unwrap())).unwrap();

        assert!(matches!(handle.join().unwrap(), Err(Error::Timeout)));
    }

    #[test]
    fn callback_wait_is_cooperatively_cancelled_with_a_silent_client() {
        let listener = LoopbackListener::bind().unwrap();
        let redirect = Url::parse(&listener.redirect_uri().unwrap()).unwrap();
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let listener_cancelled = std::sync::Arc::clone(&cancelled);
        let handle = thread::spawn(move || {
            listener.accept_path_cancelled(
                "right",
                WEB_CALLBACK_PATH,
                Duration::from_secs(30),
                &listener_cancelled,
            )
        });
        let silent_client = TcpStream::connect(("127.0.0.1", redirect.port().unwrap())).unwrap();
        cancelled.store(true, Ordering::Release);
        drop(silent_client);

        assert!(
            matches!(handle.join().unwrap(), Err(Error::Callback(message)) if message == "authorization cancelled")
        );
        assert!(LoopbackListener::bind_on(redirect.port().unwrap()).is_ok());
    }
}

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;

use crate::{Error, Result};

pub const REQUIRED_SCOPES: [&str; 12] = [
    "user-library-read",
    "user-library-modify",
    "user-read-playback-state",
    "user-modify-playback-state",
    "streaming",
    "user-read-private",
    "playlist-read-private",
    "playlist-read-collaborative",
    "playlist-modify-public",
    "playlist-modify-private",
    "user-follow-read",
    "user-follow-modify",
];
pub const SCOPES: &str = "user-library-read user-library-modify user-read-playback-state user-modify-playback-state streaming user-read-private playlist-read-private playlist-read-collaborative playlist-modify-public playlist-modify-private user-follow-read user-follow-modify";
const AUTHORIZE_URL: &str = "https://accounts.spotify.com/authorize";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";

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
    let mut url = Url::parse(AUTHORIZE_URL).expect("Spotify authorize URL is constant");
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", SCOPES)
        .append_pair("state", state)
        .append_pair("code_challenge_method", "S256")
        .append_pair("code_challenge", challenge);
    Ok(url)
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: u64,
    #[serde(default)]
    pub scope: Option<String>,
}

pub async fn exchange_code(
    client: &reqwest::Client,
    client_id: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<TokenResponse> {
    token_request(
        client,
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

async fn token_request(client: &reqwest::Client, form: &[(&str, &str)]) -> Result<TokenResponse> {
    let response = client
        .post(TOKEN_URL)
        .form(form)
        .send()
        .await
        .map_err(|error| Error::Transport(error.to_string()))?;
    let status = response.status().as_u16();
    let body = response
        .bytes()
        .await
        .map_err(|error| Error::Transport(error.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(Error::Http {
            endpoint: TOKEN_URL.into(),
            status,
            body: String::from_utf8_lossy(&body).into_owned(),
        });
    }
    serde_json::from_slice(&body).map_err(|source| Error::Json {
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
        self.listener
            .local_addr()
            .map(|address| format!("http://127.0.0.1:{}/callback", address.port()))
            .map_err(|error| Error::Callback(error.to_string()))
    }

    pub fn accept(self, expected_state: &str, timeout: Duration) -> Result<Callback> {
        self.listener
            .set_nonblocking(true)
            .map_err(|error| Error::Callback(error.to_string()))?;
        let deadline = Instant::now() + timeout;
        loop {
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
                .and_then(|()| stream.set_read_timeout(Some(remaining)))
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
            if url.path() != "/callback" {
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
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::thread;
    use std::time::Duration;

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
    fn callback_times_out() {
        let listener = LoopbackListener::bind().unwrap();
        assert!(matches!(
            listener.accept("right", Duration::from_millis(10)),
            Err(Error::Timeout)
        ));
    }
}

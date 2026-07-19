use std::io::{Read, Write};
use std::net::TcpListener;

use base64::Engine;
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;

use crate::{Error, Result};

pub const SCOPES: &str =
    "user-library-read user-library-modify user-read-playback-state user-modify-playback-state";
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

pub async fn refresh(
    client: &reqwest::Client,
    client_id: &str,
    refresh_token: &str,
) -> Result<TokenResponse> {
    token_request(
        client,
        &[
            ("client_id", client_id),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
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
    pub fn bind() -> Result<Self> {
        TcpListener::bind(("127.0.0.1", 0))
            .map(|listener| Self { listener })
            .map_err(|error| Error::Callback(error.to_string()))
    }

    pub fn redirect_uri(&self) -> Result<String> {
        self.listener
            .local_addr()
            .map(|address| format!("http://127.0.0.1:{}/callback", address.port()))
            .map_err(|error| Error::Callback(error.to_string()))
    }

    pub fn accept(self, expected_state: &str) -> Result<Callback> {
        let (mut stream, _) = self
            .listener
            .accept()
            .map_err(|error| Error::Callback(error.to_string()))?;
        let mut buffer = [0_u8; 8192];
        let read = stream
            .read(&mut buffer)
            .map_err(|error| Error::Callback(error.to_string()))?;
        let callback = parse_callback_request(&String::from_utf8_lossy(&buffer[..read]))?;
        let (status, body) = if callback.state == expected_state {
            (
                "200 OK",
                "Authorization complete. You can close this window.",
            )
        } else {
            ("400 Bad Request", "Authorization failed: state mismatch.")
        };
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .map_err(|error| Error::Callback(error.to_string()))?;
        if callback.state != expected_state {
            return Err(Error::StateMismatch);
        }
        Ok(callback)
    }
}

fn parse_callback_request(request: &str) -> Result<Callback> {
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| Error::Callback("malformed HTTP request line".into()))?;
    let url = Url::parse(&format!("http://127.0.0.1{target}"))
        .map_err(|error| Error::Callback(error.to_string()))?;
    if url.path() != "/callback" {
        return Err(Error::Callback("unexpected callback path".into()));
    }
    let value = |name: &str| {
        url.query_pairs()
            .find_map(|(key, value)| (key == name).then(|| value.into_owned()))
            .ok_or_else(|| Error::Callback(format!("missing {name}")))
    };
    Ok(Callback {
        code: value("code")?,
        state: value("state")?,
    })
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::thread;

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
        let handle = thread::spawn(move || listener.accept("right"));
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
        let handle = thread::spawn(move || listener.accept("right"));
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
}

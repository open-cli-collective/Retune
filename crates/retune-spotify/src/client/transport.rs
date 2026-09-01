use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;

use crate::{Error, Result};

const API_RESPONSE_BODY_LIMIT: usize = 8 * 1024 * 1024;
const TOKEN_RESPONSE_BODY_LIMIT: usize = 64 * 1024;

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

    /// Test double: a 429 with a `retry-after` header and an empty JSON body.
    pub fn rate_limited(retry_after: &str) -> Self {
        let mut response = Self::json(429, serde_json::json!({}));
        response
            .headers
            .insert("retry-after".into(), retry_after.into());
        response
    }

    /// Test double: a 429 classified as Development Mode quota exhaustion,
    /// with an optional `retry-after` header in seconds.
    pub fn quota_exceeded(retry_after_secs: Option<u64>) -> Self {
        let mut response = Self::json(
            429,
            serde_json::json!({"error": {"reason": "QUOTA_EXCEEDED"}}),
        );
        if let Some(retry_after_secs) = retry_after_secs {
            response
                .headers
                .insert("retry-after".into(), retry_after_secs.to_string());
        }
        response
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
            let response_body_limit = if request.url == crate::auth::TOKEN_URL {
                TOKEN_RESPONSE_BODY_LIMIT
            } else {
                API_RESPONSE_BODY_LIMIT
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
                .map_err(|source| Error::TransportSource {
                    source: Box::new(source),
                })?;
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
            let body = collect_body(response, response_body_limit).await?;
            Ok(Response {
                status,
                headers,
                body,
            })
        })
    }
}

async fn collect_body(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    if response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > limit as u64)
    {
        return Err(Error::Transport(format!(
            "response body exceeds {limit} byte limit"
        )));
    }

    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|source| Error::TransportSource {
            source: Box::new(source),
        })?
    {
        if body
            .len()
            .checked_add(chunk.len())
            .is_none_or(|length| length > limit)
        {
            return Err(Error::Transport(format!(
                "response body exceeds {limit} byte limit"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
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

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    use super::*;

    fn serve(response: Vec<u8>) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(&response);
        });
        (format!("http://{address}/"), handle)
    }

    #[tokio::test]
    async fn response_body_limit_accepts_exact_json_and_rejects_plus_one() {
        const LIMIT: usize = 64;
        let mut exact = br#"{"ok":true}"#.to_vec();
        exact.resize(LIMIT, b' ');
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            exact.len()
        )
        .into_bytes();
        let (url, server) = serve([response, exact].concat());
        let response = reqwest::get(url).await.unwrap();
        let body = collect_body(response, LIMIT).await.unwrap();
        serde_json::from_slice::<serde_json::Value>(&body).unwrap();
        server.join().unwrap();

        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            LIMIT + 1
        )
        .into_bytes();
        let (url, server) = serve([response, vec![b' '; LIMIT + 1]].concat());
        let response = reqwest::get(url).await.unwrap();
        assert!(matches!(
            collect_body(response, LIMIT).await,
            Err(Error::Transport(message)) if message.contains("exceeds")
        ));
        server.join().unwrap();

        let chunk = vec![b' '; LIMIT + 1];
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:X}\r\n",
            chunk.len()
        )
        .into_bytes();
        response.extend_from_slice(&chunk);
        response.extend_from_slice(b"\r\n0\r\n\r\n");
        let (url, server) = serve(response);
        let response = reqwest::get(url).await.unwrap();
        assert!(matches!(
            collect_body(response, LIMIT).await,
            Err(Error::Transport(message)) if message.contains("exceeds")
        ));
        server.join().unwrap();

        assert_eq!(TOKEN_RESPONSE_BODY_LIMIT, 64 * 1024);
        assert_eq!(API_RESPONSE_BODY_LIMIT, 8 * 1024 * 1024);
    }

    #[tokio::test]
    async fn http_transport_preserves_reqwest_as_the_error_source() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);

        let error = HttpTransport::new()
            .send(Request {
                method: Method::Get,
                url: format!("http://{address}/"),
                headers: HashMap::new(),
                body: Vec::new(),
            })
            .await
            .unwrap_err();

        assert!(matches!(error, Error::TransportSource { .. }));
        assert!(std::error::Error::source(&error).is_some());
    }
}

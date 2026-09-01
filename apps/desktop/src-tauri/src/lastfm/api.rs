#[cfg(test)]
use std::collections::VecDeque;
use std::{future::Future, pin::Pin, time::Duration};

use md5::{Digest, Md5};
use reqwest::Client;
use serde_json::Value;

use super::Service;

const API_URL: &str = "https://ws.audioscrobbler.com/2.0/";
pub(super) const RESPONSE_BODY_LIMIT: usize = 8 * 1024 * 1024;
const USER_AGENT: &str = concat!("Retune/", env!("CARGO_PKG_VERSION"));

#[derive(Clone)]
pub(crate) struct Credentials {
    pub(super) api_key: String,
    pub(super) shared_secret: String,
}

pub(crate) fn credentials_from(
    api_key: Option<&str>,
    shared_secret: Option<&str>,
) -> Option<Credentials> {
    let api_key = api_key?.trim();
    let shared_secret = shared_secret?.trim();
    (!api_key.is_empty() && !shared_secret.is_empty()).then(|| Credentials {
        api_key: api_key.into(),
        shared_secret: shared_secret.into(),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Failure {
    Network,
    Http(u16),
    Api(u32),
    Response,
}

impl Failure {
    pub(super) fn code(self) -> Option<u32> {
        match self {
            Self::Api(code) => Some(code),
            Self::Network | Self::Http(_) | Self::Response => None,
        }
    }
}

pub(super) struct RequestResponse {
    status: u16,
    body: Vec<u8>,
}

pub(super) trait RequestExecutor: Send + Sync {
    fn post(
        &self,
        params: Vec<(String, String)>,
    ) -> Pin<Box<dyn Future<Output = Result<RequestResponse, Failure>> + Send + '_>>;
}

pub(super) struct HttpRequestExecutor {
    client: Client,
}

impl HttpRequestExecutor {
    pub(super) fn new() -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(20))
                .user_agent(USER_AGENT)
                .build()
                .expect("Last.fm HTTP client configuration is valid"),
        }
    }
}

impl RequestExecutor for HttpRequestExecutor {
    fn post(
        &self,
        params: Vec<(String, String)>,
    ) -> Pin<Box<dyn Future<Output = Result<RequestResponse, Failure>> + Send + '_>> {
        Box::pin(async move {
            let response = self
                .client
                .post(API_URL)
                .form(&params)
                .send()
                .await
                .map_err(|_| Failure::Network)?;
            let status = response.status().as_u16();
            let body = collect_response_body(response).await?;
            Ok(RequestResponse { status, body })
        })
    }
}

async fn collect_response_body(response: reqwest::Response) -> Result<Vec<u8>, Failure> {
    collect_response_body_with_limit(response, RESPONSE_BODY_LIMIT).await
}

pub(super) async fn collect_response_body_with_limit(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, Failure> {
    if response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > limit as u64)
    {
        return Err(Failure::Response);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Failure::Network)? {
        if body
            .len()
            .checked_add(chunk.len())
            .is_none_or(|length| length > limit)
        {
            return Err(Failure::Response);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct FakeRequestExecutor {
    responses: std::sync::Mutex<VecDeque<Result<RequestResponse, Failure>>>,
    requests: std::sync::Mutex<Vec<Vec<(String, String)>>>,
}

#[cfg(test)]
impl FakeRequestExecutor {
    pub(crate) fn queue_json(&self, value: Value) {
        self.queue_response(200, serde_json::to_vec(&value).unwrap());
    }

    pub(super) fn queue_response(&self, status: u16, body: impl Into<Vec<u8>>) {
        self.responses
            .lock()
            .expect("Last.fm fake response mutex poisoned")
            .push_back(Ok(RequestResponse {
                status,
                body: body.into(),
            }));
    }

    pub(super) fn queue_network_failure(&self) {
        self.responses
            .lock()
            .expect("Last.fm fake response mutex poisoned")
            .push_back(Err(Failure::Network));
    }

    pub(crate) fn requests(&self) -> Vec<Vec<(String, String)>> {
        self.requests
            .lock()
            .expect("Last.fm fake request mutex poisoned")
            .clone()
    }
}

#[cfg(test)]
impl RequestExecutor for FakeRequestExecutor {
    fn post(
        &self,
        params: Vec<(String, String)>,
    ) -> Pin<Box<dyn Future<Output = Result<RequestResponse, Failure>> + Send + '_>> {
        Box::pin(async move {
            self.requests
                .lock()
                .expect("Last.fm fake request mutex poisoned")
                .push(params);
            self.responses
                .lock()
                .expect("Last.fm fake response mutex poisoned")
                .pop_front()
                .unwrap_or(Err(Failure::Response))
        })
    }
}

impl Service {
    pub(super) async fn post(
        &self,
        method: &str,
        mut params: Vec<(String, String)>,
        session_key: Option<&str>,
    ) -> Result<Value, Failure> {
        let credentials = self.credentials.as_ref().ok_or(Failure::Api(10))?;
        params.push(("api_key".into(), credentials.api_key.clone()));
        params.push(("method".into(), method.into()));
        if let Some(session_key) = session_key {
            params.push(("sk".into(), session_key.into()));
        }
        params.push(("format".into(), "json".into()));
        params.push((
            "api_sig".into(),
            signature(&params, &credentials.shared_secret),
        ));
        let response = self.request_executor.post(params).await?;
        let value: Value = match serde_json::from_slice(&response.body) {
            Ok(value) => value,
            Err(_) if !(200..300).contains(&response.status) => {
                return Err(Failure::Http(response.status));
            }
            Err(_) => return Err(Failure::Response),
        };
        if let Some(code) = error_code(&value) {
            return Err(Failure::Api(code));
        }
        if !(200..300).contains(&response.status) {
            return Err(Failure::Http(response.status));
        }
        if response_text(&value, &["status"])
            .or_else(|| response_text(&value, &["@status"]))
            .or_else(|| response_text(&value, &["@attr", "status"]))
            .is_some_and(|status| status != "ok")
        {
            return Err(Failure::Response);
        }
        Ok(value)
    }
}

pub(super) fn signature(params: &[(String, String)], shared_secret: &str) -> String {
    let mut values = params
        .iter()
        .filter(|(key, _)| key != "format" && key != "callback" && key != "api_sig")
        .collect::<Vec<_>>();
    values.sort_by(|left, right| left.0.cmp(&right.0));
    let mut input = String::new();
    for (key, value) in values {
        input.push_str(key);
        input.push_str(value);
    }
    input.push_str(shared_secret);
    Md5::digest(input.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(super) fn response_text(value: &Value, path: &[&str]) -> Option<String> {
    let mut current = value.get("lfm").unwrap_or(value);
    for key in path {
        current = current.get(*key)?;
    }
    current.as_str().map(ToOwned::to_owned).or_else(|| {
        current
            .get("#text")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    })
}

pub(super) fn error_code(value: &Value) -> Option<u32> {
    let root = value.get("lfm").unwrap_or(value);
    let error = root.get("error")?;
    let code = error
        .get("code")
        .or_else(|| error.get("@code"))
        .unwrap_or(error);
    code.as_u64()
        .or_else(|| code.as_str()?.parse().ok())
        .map(|code| code as u32)
}

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde::Serialize;
use tauri::Manager;
use tauri_plugin_opener::OpenerExt;

pub(crate) const LOG_TARGET: &str = "retune::diagnostics";
pub(crate) const SESSION_START_MARKER: &str = "retune.session.start";
const MAX_DIAGNOSTIC_EMAIL_BODY_BYTES: usize = 256 * 1024;
const MAX_DIAGNOSTIC_MAILTO_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum DiagnosticLevel {
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct DiagnosticEntry {
    pub(crate) date: String,
    pub(crate) time: String,
    pub(crate) level: DiagnosticLevel,
    pub(crate) target: String,
    pub(crate) message: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiagnosticReport {
    pub(crate) entries: Vec<DiagnosticEntry>,
    pub(crate) email_available: bool,
}

fn bracketed_field(input: &str) -> Option<(&str, &str)> {
    let input = input.strip_prefix('[')?;
    let end = input.find(']')?;
    Some((&input[..end], &input[end + 1..]))
}

pub(crate) fn parse_line(line: &str) -> Option<DiagnosticEntry> {
    let (date, rest) = bracketed_field(line)?;
    let (time, rest) = bracketed_field(rest)?;
    let (level, rest) = bracketed_field(rest)?;
    let (target, message) = bracketed_field(rest)?;
    if date.is_empty() || time.is_empty() || target.is_empty() {
        return None;
    }
    let message = message.strip_prefix(' ')?;
    let level = match level {
        "INFO" => DiagnosticLevel::Info,
        "WARN" => DiagnosticLevel::Warn,
        "ERROR" => DiagnosticLevel::Error,
        _ => return None,
    };
    Some(DiagnosticEntry {
        date: date.to_owned(),
        time: time.to_owned(),
        level,
        target: target.to_owned(),
        message: redact_message(message),
    })
}

const REDACTED: &str = "[REDACTED]";
const SENSITIVE_KEYS: &[&str] = &[
    "access token",
    "access_token",
    "accesstoken",
    "access-token",
    "refresh token",
    "refresh_token",
    "refreshtoken",
    "refresh-token",
    "playback credentials",
    "playback_credentials",
    "playbackcredentials",
    "playback-credentials",
    "playback credential",
    "playback-credential",
    "authorization",
    "authorization token",
    "authorization_token",
    "authorizationtoken",
    "authorization-token",
    "oauth_token",
    "oauthtoken",
    "oauth-token",
    "auth token",
    "auth_token",
    "authtoken",
    "auth-token",
    "bearer",
    "client secret",
    "client_secret",
    "clientsecret",
    "client-secret",
    "shared secret",
    "shared_secret",
    "sharedsecret",
    "shared-secret",
    "session key",
    "session_key",
    "sessionkey",
    "session-key",
    "lastfm session key",
    "lastfm_session_key",
    "lastfmsessionkey",
    "lastfm-session-key",
    "api key",
    "api_key",
    "apikey",
    "api-key",
    "api sig",
    "api_sig",
    "apisig",
    "api-sig",
    "sk",
    "password",
    "passwd",
    "secret",
    "token",
    "key",
];

fn is_key_boundary(value: Option<char>, key: &str) -> bool {
    !value.is_some_and(|value| {
        value.is_ascii_alphanumeric() || value == '_' || (value == '-' && !key.contains('-'))
    })
}

fn next_sensitive_key(message: &str, from: usize) -> Option<(usize, usize)> {
    let lower = message.to_ascii_lowercase();
    SENSITIVE_KEYS
        .iter()
        .filter_map(|key| {
            let mut search = from;
            while let Some(offset) = lower[search..].find(key) {
                let start = search + offset;
                let end = start + key.len();
                if is_key_boundary(message[..start].chars().next_back(), key)
                    && is_key_boundary(message[end..].chars().next(), key)
                {
                    return Some((start, end));
                }
                search = end;
            }
            None
        })
        .min_by_key(|(start, _)| *start)
}

fn redact_field_value(
    message: &str,
    key_start: usize,
    key_end: usize,
) -> Option<(usize, usize, String)> {
    let key = message[key_start..key_end].to_ascii_lowercase();
    let bytes = message.as_bytes();
    let mut cursor = key_end;
    while bytes
        .get(cursor)
        .is_some_and(|byte| byte.is_ascii_whitespace() || matches!(byte, b'"' | b'\''))
    {
        cursor += 1;
    }
    if !matches!(bytes.get(cursor), Some(b':' | b'=')) {
        return None;
    }
    cursor += 1;
    while bytes
        .get(cursor)
        .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        cursor += 1;
    }
    let quoted = bytes
        .get(cursor)
        .copied()
        .filter(|byte| matches!(byte, b'"' | b'\''));
    if let Some(quote) = quoted {
        let start = cursor + 1;
        let mut end = start;
        while end < message.len() {
            if bytes[end] == quote {
                let mut backslashes = 0;
                let mut previous = end;
                while previous > start && bytes[previous - 1] == b'\\' {
                    backslashes += 1;
                    previous -= 1;
                }
                if backslashes % 2 == 0 {
                    break;
                }
            }
            end += 1;
        }
        return Some((start, end, REDACTED.into()));
    }

    let start = cursor;
    let is_authorization = key.contains("authorization") || key == "bearer";
    let end = message[start..]
        .char_indices()
        .find_map(|(offset, value)| {
            let delimiter = if is_authorization {
                matches!(value, '&' | ',' | '}' | ']' | ';' | '\r' | '\n')
            } else {
                value.is_ascii_whitespace()
                    || matches!(value, '&' | ',' | '}' | ']' | ';' | '\r' | '\n')
            };
            delimiter.then_some(start + offset)
        })
        .unwrap_or(message.len());
    if start == end {
        return None;
    }
    let value = &message[start..end];
    let replacement = value
        .find(char::is_whitespace)
        .map(|offset| &value[..offset])
        .filter(|scheme| {
            scheme.eq_ignore_ascii_case("bearer") || scheme.eq_ignore_ascii_case("basic")
        })
        .map(|scheme| format!("{scheme} {REDACTED}"))
        .unwrap_or_else(|| REDACTED.into());
    Some((start, end, replacement))
}

fn redact_message(message: &str) -> String {
    let mut result = String::with_capacity(message.len());
    let mut cursor = 0;
    while let Some((key_start, key_end)) = next_sensitive_key(message, cursor) {
        let Some((value_start, value_end, replacement)) =
            redact_field_value(message, key_start, key_end)
        else {
            cursor = key_end;
            continue;
        };
        result.push_str(&message[cursor..value_start]);
        result.push_str(&replacement);
        cursor = value_end;
    }
    result.push_str(&message[cursor..]);
    let lower = result.to_ascii_lowercase();
    lower
        .contains(" returned http ")
        .then(|| {
            result
                .split_once(": ")
                .map(|(prefix, _)| format!("{prefix}: [redacted response body]"))
        })
        .flatten()
        .unwrap_or(result)
}

fn redacted_mailto_url(email: &str, body: &str) -> Result<String, String> {
    diagnostic_mailto_url(
        email,
        body,
        MAX_DIAGNOSTIC_EMAIL_BODY_BYTES,
        MAX_DIAGNOSTIC_MAILTO_BYTES,
    )
}

pub(crate) fn current_session_entries(contents: &str) -> Vec<DiagnosticEntry> {
    let mut entries = Vec::new();
    let mut session_start = None;
    for line in contents.lines() {
        let Some(entry) = parse_line(line) else {
            continue;
        };
        if entry.target == LOG_TARGET && entry.message == SESSION_START_MARKER {
            session_start = Some(entries.len());
        } else {
            entries.push(entry);
        }
    }
    let Some(session_start) = session_start else {
        return Vec::new();
    };
    entries.into_iter().skip(session_start).collect()
}

pub(crate) fn log_file_path(log_dir: &Path, app_name: &str) -> PathBuf {
    log_dir.join(app_name).with_extension("log")
}

pub(crate) fn read_current_session(path: &Path) -> io::Result<Vec<DiagnosticEntry>> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    Ok(current_session_entries(&contents))
}

pub(crate) fn support_email_from(value: Option<&str>) -> Option<&str> {
    value.and_then(|value| {
        let value = value.trim();
        let (local, domain) = value.split_once('@')?;
        (!local.is_empty()
            && !domain.is_empty()
            && !domain.contains('@')
            && !value
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || matches!(byte, b'?' | b'#' | b'&')))
        .then_some(value)
    })
}

fn support_email() -> Option<&'static str> {
    support_email_from(option_env!("RETUNE_SUPPORT_EMAIL"))
}

fn mailto_url(email: &str, body: &str) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("subject", "Retune diagnostic report")
        .append_pair("body", body)
        .finish();
    format!("mailto:{email}?{query}")
}

fn diagnostic_mailto_url(
    email: &str,
    body: &str,
    max_body_bytes: usize,
    max_url_bytes: usize,
) -> Result<String, String> {
    if body.len() > max_body_bytes {
        return Err("The diagnostic report is too large to email. Copy Logs instead.".into());
    }
    let url = mailto_url(email, &redact_message(body));
    if url.len() > max_url_bytes {
        return Err("The diagnostic email is too large to open. Copy Logs instead.".into());
    }
    Ok(url)
}

#[tauri::command]
pub(super) async fn load_diagnostics(app: tauri::AppHandle) -> Result<DiagnosticReport, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let log_dir = app
            .path()
            .app_log_dir()
            .map_err(|error| format!("Could not locate the Retune log: {error}"))?;
        let path = log_file_path(&log_dir, &app.package_info().name);
        let entries = read_current_session(&path)
            .map_err(|error| format!("Could not read the Retune log: {error}"))?;
        Ok(DiagnosticReport {
            entries,
            email_available: support_email().is_some(),
        })
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub(super) fn email_diagnostics(app: tauri::AppHandle, body: String) -> Result<(), String> {
    if body.trim().is_empty() {
        return Err("There are no diagnostic problems to report.".into());
    }
    let email = support_email().ok_or_else(|| {
        "Email support is unavailable in this build. Copy Logs and share the report instead."
            .to_string()
    })?;
    let url = redacted_mailto_url(email, &body)?;
    app.opener()
        .open_url(url, None::<String>)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parses_actual_bracketed_log_shape() {
        for (name, expected) in [
            ("INFO", DiagnosticLevel::Info),
            ("WARN", DiagnosticLevel::Warn),
            ("ERROR", DiagnosticLevel::Error),
        ] {
            let entry = parse_line(&format!(
                "[2026-08-16][14:03:02][{name}][retune::sync] retrying"
            ))
            .unwrap();
            assert_eq!(entry.date, "2026-08-16");
            assert_eq!(entry.time, "14:03:02");
            assert_eq!(entry.level, expected);
            assert_eq!(entry.target, "retune::sync");
            assert_eq!(entry.message, "retrying");
        }
    }

    #[test]
    fn preserves_brackets_inside_messages() {
        let entry = parse_line("[2026-08-16][14:03:02][ERROR][retune] failed [retry=2]").unwrap();
        assert_eq!(entry.message, "failed [retry=2]");
    }

    #[test]
    fn redacts_all_credential_forms_while_preserving_context() {
        let message = concat!(
            "GET /play?access_token=access-canary&refresh_token=refresh-canary&access-token=access-hyphen-canary&refresh-token=refresh-hyphen-canary&sk=lastfm-query-canary ",
            "headers={X-Api-Key: api-key-canary X-Client-Secret: client-secret-canary X-Session-Key: session-key-canary} ",
            "headers={Authorization: Bearer authorization-canary} ",
            "body={\"playbackCredentials\":\"playback-credential-canary\",",
            "\"accessToken\":\"playback-canary\",",
            "\"refreshToken\":\"refresh-body-canary\",\"session_key\":\"lastfm-canary\",",
            "\"key\":\"session-key-canary\"}"
        );
        let entry = parse_line(&format!(
            "[2026-08-16][14:03:02][ERROR][retune::http] {message}"
        ))
        .unwrap();
        for secret in [
            "access-canary",
            "refresh-canary",
            "access-hyphen-canary",
            "refresh-hyphen-canary",
            "lastfm-query-canary",
            "api-key-canary",
            "client-secret-canary",
            "session-key-canary",
            "authorization-canary",
            "playback-credential-canary",
            "playback-canary",
            "refresh-body-canary",
            "lastfm-canary",
            "session-key-canary",
        ] {
            assert!(!entry.message.contains(secret), "secret leaked: {secret}");
        }
        assert!(entry.message.contains("GET /play"));
        assert!(entry
            .message
            .contains("headers={Authorization: Bearer [REDACTED]}"));
        assert!(entry.message.contains("body={"));

        let boundary = redact_message("access-tokenized=keep-access-tokenized");
        assert!(boundary.contains("keep-access-tokenized"));

        let url = redacted_mailto_url("support@example.com", message).unwrap();
        assert!(!url.contains("access-canary"));
        assert!(!url.contains("lastfm-canary"));

        let entry = parse_line(
            "[2026-08-16][14:03:02][ERROR][retune::spotify] Spotify /me returned HTTP 401: raw-canary",
        )
        .unwrap();
        assert_eq!(
            entry.message,
            "Spotify /me returned HTTP 401: [redacted response body]"
        );
    }

    #[test]
    fn skips_malformed_and_unwanted_levels() {
        assert!(parse_line("not a log").is_none());
        assert!(parse_line("[date][time][INFO][target]missing-space").is_none());
        assert!(parse_line("[date][time][DEBUG][target] debug").is_none());
        assert!(parse_line("[date][time][INFO][] empty target").is_none());
    }

    #[test]
    fn selects_entries_after_the_latest_session_marker() {
        let contents = format!(
            "[date][time][INFO][retune] old\n[date][time][INFO][{LOG_TARGET}] {SESSION_START_MARKER}\n[date][time][INFO][retune] first\n[date][time][INFO][{LOG_TARGET}] {SESSION_START_MARKER}\n[date][time][WARN][retune] latest"
        );
        let entries = current_session_entries(&contents);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].message, "latest");
    }

    #[test]
    fn missing_log_is_empty_and_unreadable_log_is_an_error() {
        let directory = tempdir().unwrap();
        assert!(read_current_session(&directory.path().join("missing.log"))
            .unwrap()
            .is_empty());
        assert!(read_current_session(directory.path()).is_err());
    }

    #[test]
    fn support_email_helper_handles_configured_and_unconfigured_values() {
        assert_eq!(support_email_from(None), None);
        assert_eq!(support_email_from(Some("  ")), None);
        assert_eq!(support_email_from(Some("not-an-email")), None);
        assert_eq!(
            support_email_from(Some("support@example.com?subject=bad")),
            None
        );
        assert_eq!(
            support_email_from(Some(" support@example.com ")),
            Some("support@example.com")
        );
    }

    #[test]
    fn email_url_encodes_subject_and_report_body() {
        let url = mailto_url("support@example.com", "[ERROR] failed & retry");
        assert!(url.starts_with("mailto:support@example.com?"));
        assert!(url.contains("subject=Retune+diagnostic+report"));
        assert!(url.contains("body=%5BERROR%5D+failed+%26+retry"));
    }

    #[test]
    fn diagnostic_email_limits_body_and_final_url_before_an_opener_effect() {
        let email = "support@example.com";
        let exact_body = "a".repeat(8);
        let exact_url = mailto_url(email, &exact_body);
        let mut opener_calls = 0;
        let mut open = |result: Result<String, String>| {
            if result.is_ok() {
                opener_calls += 1;
            }
            result
        };

        assert_eq!(
            open(diagnostic_mailto_url(
                email,
                &exact_body,
                8,
                exact_url.len()
            ))
            .unwrap(),
            exact_url
        );
        assert!(open(diagnostic_mailto_url(
            email,
            &"a".repeat(9),
            9,
            exact_url.len()
        ))
        .is_err());
        assert!(open(diagnostic_mailto_url(email, &"a".repeat(9), 8, usize::MAX)).is_err());
        assert_eq!(opener_calls, 1);
    }
}

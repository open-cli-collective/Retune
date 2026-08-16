use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde::Serialize;
use tauri::Manager;
use tauri_plugin_opener::OpenerExt;

pub(crate) const LOG_TARGET: &str = "retune::diagnostics";
pub(crate) const SESSION_START_MARKER: &str = "retune.session.start";

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
        message: message.to_owned(),
    })
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
        (!value.is_empty()).then_some(value)
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

#[tauri::command]
pub(super) fn load_diagnostics(app: tauri::AppHandle) -> Result<DiagnosticReport, String> {
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
    app.opener()
        .open_url(mailto_url(email, &body), None::<String>)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parses_actual_bracketed_log_shape() {
        let entry = parse_line("[2026-08-16][14:03:02][WARN][retune::sync] retrying").unwrap();
        assert_eq!(entry.date, "2026-08-16");
        assert_eq!(entry.time, "14:03:02");
        assert_eq!(entry.level, DiagnosticLevel::Warn);
        assert_eq!(entry.target, "retune::sync");
        assert_eq!(entry.message, "retrying");
    }

    #[test]
    fn preserves_brackets_inside_messages() {
        let entry = parse_line("[2026-08-16][14:03:02][ERROR][retune] failed [retry=2]").unwrap();
        assert_eq!(entry.message, "failed [retry=2]");
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
}

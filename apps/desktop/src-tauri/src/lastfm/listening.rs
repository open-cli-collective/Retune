use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::api::response_text;

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ScrobbleMetadata {
    pub artist: String,
    pub album: String,
    pub track: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AcceptedScrobbleReceipt {
    pub corrected: ScrobbleMetadata,
    pub submitted: ScrobbleMetadata,
    pub timestamp: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Scrobble {
    pub(super) artist: String,
    pub(super) track: String,
    pub(super) album: Option<String>,
    pub(super) duration_secs: u64,
    pub(super) timestamp: u64,
    #[serde(default)]
    pub(super) owner: String,
}

impl Scrobble {
    pub(super) fn from_track(
        track: &crate::playback::SnapshotTrack,
        timestamp: u64,
    ) -> Option<Self> {
        let artist = track.art.trim();
        let title = track.name.trim();
        if artist.is_empty() || title.is_empty() || artist.eq_ignore_ascii_case("unknown artist") {
            return None;
        }
        let album = (!track.alb.trim().is_empty()
            && !track.alb.trim().eq_ignore_ascii_case("unknown album"))
        .then(|| track.alb.trim().to_owned());
        Some(Self {
            artist: artist.into(),
            track: title.into(),
            album,
            duration_secs: track.duration_secs,
            timestamp,
            owner: String::new(),
        })
    }

    pub(super) fn now_playing_params(&self) -> Vec<(String, String)> {
        let mut params = vec![
            ("artist".into(), self.artist.clone()),
            ("track".into(), self.track.clone()),
        ];
        if let Some(album) = &self.album {
            params.push(("album".into(), album.clone()));
        }
        if self.duration_secs > 0 {
            params.push(("duration".into(), self.duration_secs.to_string()));
        }
        params
    }

    pub(super) fn scrobble_params(&self, index: usize) -> Vec<(String, String)> {
        let mut params = vec![
            (format!("artist[{index}]"), self.artist.clone()),
            (format!("track[{index}]"), self.track.clone()),
            (format!("timestamp[{index}]"), self.timestamp.to_string()),
        ];
        if let Some(album) = &self.album {
            params.push((format!("album[{index}]"), album.clone()));
        }
        if self.duration_secs > 0 {
            params.push((format!("duration[{index}]"), self.duration_secs.to_string()));
        }
        params
    }
}

#[derive(Default)]
pub(super) struct ListeningState {
    generation: Option<u64>,
    track: Option<crate::playback::SnapshotTrack>,
    started_at: u64,
    played_ms: u64,
    discontinuous: bool,
    scrobbled: bool,
}

pub(super) enum ListeningAction {
    NowPlaying(Scrobble),
    Enqueue(Scrobble),
}

impl ListeningState {
    pub(super) fn apply(
        &mut self,
        fact: crate::playback::ListeningFact,
        started_at: u64,
    ) -> Vec<ListeningAction> {
        match fact {
            crate::playback::ListeningFact::Started { generation, track } => {
                self.generation = Some(generation);
                self.track = Some(track.clone());
                self.started_at = started_at;
                self.played_ms = 0;
                self.discontinuous = false;
                self.scrobbled = false;
                Scrobble::from_track(&track, 0)
                    .map(ListeningAction::NowPlaying)
                    .into_iter()
                    .collect()
            }
            crate::playback::ListeningFact::Forward {
                generation,
                track,
                played_ms,
            } => {
                if !self.matches(generation, &track) {
                    return Vec::new();
                }
                self.played_ms = self.played_ms.max(played_ms);
                self.scrobble_if_eligible(&track, false)
            }
            crate::playback::ListeningFact::Discontinuity { generation, track } => {
                if self.matches(generation, &track) {
                    self.discontinuous = true;
                }
                Vec::new()
            }
            crate::playback::ListeningFact::Completed { generation, track } => {
                if !self.matches(generation, &track) {
                    return Vec::new();
                }
                self.scrobble_if_eligible(&track, true)
            }
        }
    }

    fn matches(&self, generation: u64, track: &crate::playback::SnapshotTrack) -> bool {
        self.generation == Some(generation)
            && self
                .track
                .as_ref()
                .is_some_and(|current| current.uri == track.uri)
    }

    fn scrobble_if_eligible(
        &mut self,
        track: &crate::playback::SnapshotTrack,
        completed: bool,
    ) -> Vec<ListeningAction> {
        let threshold = self
            .track
            .as_ref()
            .and_then(|track| scrobble_threshold_ms(track.duration_secs));
        let eligible = !self.scrobbled
            && !self.discontinuous
            && (threshold.is_some_and(|threshold| self.played_ms >= threshold)
                || (completed && threshold.is_some()));
        if !eligible {
            return Vec::new();
        }
        self.scrobbled = true;
        Scrobble::from_track(track, self.started_at)
            .map(ListeningAction::Enqueue)
            .into_iter()
            .collect()
    }
}

pub(super) fn queue_owner(queue: &VecDeque<Scrobble>) -> Option<&str> {
    let owner = queue.front()?.owner.as_str();
    (!owner.is_empty() && queue.iter().all(|item| item.owner.as_str() == owner)).then_some(owner)
}
pub(super) fn next_batch(queue: &VecDeque<Scrobble>) -> Vec<Scrobble> {
    queue.iter().take(50).cloned().collect()
}
pub(super) fn queue_starts_with(queue: &VecDeque<Scrobble>, batch: &[Scrobble]) -> bool {
    queue.len() >= batch.len() && queue.iter().zip(batch).all(|(queued, item)| queued == item)
}

pub(super) struct ScrobbleResult {
    pub(super) code: u32,
    pub(super) receipt: Option<AcceptedScrobbleReceipt>,
}

pub(super) fn apply_scrobble_results(
    queue: &mut VecDeque<Scrobble>,
    batch: &[Scrobble],
    codes: &[u32],
) -> Vec<(Scrobble, u32)> {
    let mut removed = Vec::with_capacity(batch.len());
    for (item, code) in batch.iter().zip(codes) {
        if queue.pop_front().is_none() {
            break;
        }
        removed.push((item.clone(), *code));
    }
    removed
}

#[cfg(test)]
pub(super) fn scrobble_codes(value: &Value, expected: usize) -> Option<Vec<u32>> {
    scrobble_results(value, expected, None)
        .map(|results| results.into_iter().map(|result| result.code).collect())
}

pub(super) fn scrobble_results(
    value: &Value,
    expected: usize,
    submitted: Option<&[Scrobble]>,
) -> Option<Vec<ScrobbleResult>> {
    let root = value.get("lfm").unwrap_or(value);
    let items = root.get("scrobbles")?.get("scrobble")?;
    let items = match items {
        Value::Array(items) => items.clone(),
        Value::Object(_) => vec![items.clone()],
        _ => return None,
    };
    if items.len() != expected {
        return None;
    }
    Some(
        items
            .into_iter()
            .enumerate()
            .map(|(index, item)| {
                let code = item
                    .get("ignoredMessage")
                    .and_then(|message| message.get("code").or_else(|| message.get("@code")))
                    .and_then(|code| code.as_u64().or_else(|| code.as_str()?.parse().ok()))
                    .unwrap_or(0) as u32;
                let receipt = (code == 0)
                    .then(|| submitted.and_then(|batch| batch.get(index)))
                    .flatten()
                    .map(|submitted| AcceptedScrobbleReceipt {
                        corrected: ScrobbleMetadata {
                            artist: response_text(&item, &["artist"])
                                .unwrap_or_else(|| submitted.artist.clone()),
                            album: response_text(&item, &["album"])
                                .unwrap_or_else(|| submitted.album.clone().unwrap_or_default()),
                            track: response_text(&item, &["track"])
                                .unwrap_or_else(|| submitted.track.clone()),
                        },
                        submitted: ScrobbleMetadata {
                            artist: submitted.artist.clone(),
                            album: submitted.album.clone().unwrap_or_default(),
                            track: submitted.track.clone(),
                        },
                        timestamp: response_text(&item, &["timestamp"])
                            .and_then(|timestamp| timestamp.parse().ok())
                            .unwrap_or(submitted.timestamp),
                    });
                ScrobbleResult { code, receipt }
            })
            .collect(),
    )
}

pub(super) fn scrobble_threshold_ms(duration_secs: u64) -> Option<u64> {
    (duration_secs > 30).then(|| (duration_secs.saturating_mul(500)).min(240_000))
}

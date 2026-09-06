use super::{Library, NewTrack, Rating, TrackEdit, TrackId, TrackRecord};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// User decisions are library data, not provider membership. Archived records
/// preserve overlays and identity while staying outside every normal projection.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct TrackDecisions {
    #[serde(default)]
    pub removed: Vec<TrackRecord>,
    #[serde(default)]
    pub merges: Vec<TrackMerge>,
    #[serde(default)]
    pub retained: BTreeSet<String>,
    #[serde(skip)]
    pub aliases: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct TrackMerge {
    pub before: Vec<TrackRecord>,
    pub after: TrackRecord,
    pub target_was_retained: bool,
    pub merged_at: u64,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", tag = "mode", content = "value")]
pub enum MergePlayCount {
    Highest,
    Sum,
    Custom(u32),
}

pub struct TrackMergeOptions {
    pub edit: TrackEdit,
    pub rating: Option<Rating>,
    pub play_count: MergePlayCount,
    pub merged_at: u64,
}

pub enum TrackMergeTarget {
    Existing(TrackId),
    New(Box<NewTrack>),
}

impl TrackDecisions {
    pub fn is_empty(&self) -> bool {
        self.removed.is_empty() && self.merges.is_empty() && self.retained.is_empty()
    }

    pub fn rebuild_aliases(&mut self) -> Result<(), String> {
        let mut aliases = BTreeMap::new();
        for merge in &self.merges {
            if merge.before.len() < 2 {
                return Err("A track merge must have at least two original entries.".into());
            }
            let mut ids = HashSet::new();
            for track in &merge.before {
                if !ids.insert(track.id) {
                    return Err("Duplicate source in a track merge.".into());
                }
                if track.uri != merge.after.uri {
                    aliases.insert(track.uri.clone(), merge.after.uri.clone());
                }
            }
        }
        let mut resolved = BTreeMap::new();
        for (source, target) in &aliases {
            let mut target = target;
            let mut seen = HashSet::from([source]);
            while let Some(next) = aliases.get(target) {
                if !seen.insert(target) {
                    return Err("Track merge aliases contain a cycle.".into());
                }
                target = next;
            }
            resolved.insert(source.clone(), target.clone());
        }
        self.aliases = resolved;
        Ok(())
    }
}

impl Library {
    /// Existing identities win as a complete merge group; importing only part
    /// of a journal would make a later undo overwrite unrelated local edits.
    pub(super) fn merge_imported(&mut self, other: Library) {
        let existing = self
            .known_tracks()
            .map(|track| track.uri.as_str())
            .collect::<HashSet<_>>();
        let blocked = other
            .known_tracks()
            .filter(|track| existing.contains(track.uri.as_str()))
            .map(|track| other.canonical_uri(&track.uri).to_owned())
            .collect::<HashSet<_>>();
        let accepted = other
            .known_tracks()
            .filter(|track| !blocked.contains(other.canonical_uri(&track.uri)))
            .map(|track| track.uri.clone())
            .collect::<BTreeSet<_>>();
        let ids = accepted
            .iter()
            .map(|uri| (uri.clone(), self.fresh_id()))
            .collect::<HashMap<_, _>>();
        let remap = |track: &mut TrackRecord| track.id = ids[&track.uri];
        for mut track in other.tracks {
            if accepted.contains(&track.uri) {
                remap(&mut track);
                self.tracks.push(track);
            }
        }
        for mut track in other.decisions.removed {
            if accepted.contains(&track.uri) {
                remap(&mut track);
                self.decisions.removed.push(track);
            }
        }
        for mut merge in other.decisions.merges {
            if accepted.contains(&merge.after.uri) {
                merge.before.iter_mut().for_each(&remap);
                remap(&mut merge.after);
                self.decisions.merges.push(merge);
            }
        }
        self.decisions.retained.extend(
            other
                .decisions
                .retained
                .into_iter()
                .filter(|uri| accepted.contains(uri)),
        );
        self.decisions
            .rebuild_aliases()
            .expect("imported merge groups have disjoint identities");
        for (key, rating) in other.album_ratings {
            self.album_ratings.entry(key).or_insert(rating);
        }
    }

    /// Includes archived identities for sync matching and diagnostics only.
    pub fn known_tracks(&self) -> impl Iterator<Item = &TrackRecord> {
        self.tracks
            .iter()
            .chain(&self.decisions.removed)
            .chain(self.decisions.merges.iter().flat_map(|merge| &merge.before))
    }

    pub fn canonical_uri<'a>(&'a self, uri: &'a str) -> &'a str {
        self.decisions
            .aliases
            .get(uri)
            .map(String::as_str)
            .unwrap_or(uri)
    }

    pub fn track_aliases(&self) -> &BTreeMap<String, String> {
        &self.decisions.aliases
    }

    pub fn get_by_uri(&self, uri: &str) -> Option<&TrackRecord> {
        let canonical = self.canonical_uri(uri);
        self.tracks.iter().find(|track| track.uri == canonical)
    }

    /// One borrowed index also resolves merged source URIs to their live target.
    pub fn tracks_by_uri(&self) -> HashMap<&str, &TrackRecord> {
        let mut tracks = self
            .tracks
            .iter()
            .map(|track| (track.uri.as_str(), track))
            .collect::<HashMap<_, _>>();
        for (source, target) in &self.decisions.aliases {
            if let Some(&track) = tracks.get(target.as_str()) {
                tracks.insert(source.as_str(), track);
            }
        }
        tracks
    }

    pub fn removed_tracks(&self) -> &[TrackRecord] {
        &self.decisions.removed
    }

    pub fn is_retained(&self, uri: &str) -> bool {
        self.decisions.retained.contains(uri)
    }

    pub(super) fn inactive_uri_ids(&self) -> HashMap<String, TrackId> {
        let mut ids = self
            .decisions
            .removed
            .iter()
            .map(|track| (track.uri.clone(), track.id))
            .collect::<HashMap<_, _>>();
        let targets = self
            .tracks
            .iter()
            .chain(&self.decisions.removed)
            .map(|track| (track.uri.as_str(), track.id))
            .collect::<HashMap<_, _>>();
        for (source, target) in &self.decisions.aliases {
            if let Some(&id) = targets.get(target.as_str()) {
                ids.insert(source.clone(), id);
            }
        }
        ids
    }

    /// Local removal never writes to the provider or deletes a source file.
    pub fn remove_tracks(&mut self, ids: &[TrackId]) -> Result<(), String> {
        let ids = ids.iter().copied().collect::<HashSet<_>>();
        let active = self
            .tracks
            .iter()
            .map(|track| track.id)
            .collect::<HashSet<_>>();
        if ids.is_empty() || !ids.is_subset(&active) {
            return Err("Choose tracks that are still in the Retune library.".into());
        }
        let mut retained = Vec::with_capacity(self.tracks.len());
        for track in self.tracks.drain(..) {
            if ids.contains(&track.id) {
                self.decisions.removed.push(track);
            } else {
                retained.push(track);
            }
        }
        self.tracks = retained;
        Ok(())
    }

    pub fn restore_track(&mut self, uri: &str) -> Option<TrackId> {
        let uri = self.canonical_uri(uri).to_owned();
        let index = self
            .decisions
            .removed
            .iter()
            .position(|track| track.uri == uri)?;
        let track = self.decisions.removed.remove(index);
        let id = track.id;
        self.decisions.retained.insert(track.uri.clone());
        self.tracks.push(track);
        Some(id)
    }

    /// Adds an explicitly chosen local library entry without saving it upstream.
    pub fn add_retune_track(&mut self, track: NewTrack) -> TrackId {
        let uri = self.canonical_uri(&track.uri).to_owned();
        if let Some(id) = self.restore_track(&uri) {
            return id;
        }
        let id = self.add(track);
        self.decisions.retained.insert(uri);
        id
    }

    pub fn merge_sources(&self, id: TrackId) -> Vec<&TrackRecord> {
        let Some(target) = self.get(id) else {
            return vec![];
        };
        let mut seen = HashSet::new();
        self.decisions
            .merges
            .iter()
            .flat_map(|merge| &merge.before)
            .filter(|track| {
                self.canonical_uri(&track.uri) == target.uri && seen.insert(track.uri.as_str())
            })
            .collect()
    }

    pub fn latest_merge_at(&self, id: TrackId) -> Option<u64> {
        let target = self.get(id)?;
        self.decisions
            .merges
            .iter()
            .rev()
            .find(|merge| merge.after.id == target.id)
            .map(|merge| merge.merged_at)
    }

    pub fn merge_tracks(
        &mut self,
        ids: &[TrackId],
        target: TrackMergeTarget,
        options: TrackMergeOptions,
    ) -> Result<TrackId, String> {
        let unique = ids.iter().copied().collect::<HashSet<_>>();
        if unique.len() < 2 || unique.len() != ids.len() {
            return Err("Select at least two distinct tracks to merge.".into());
        }
        let by_id = self
            .tracks
            .iter()
            .map(|track| (track.id, track))
            .collect::<HashMap<_, _>>();
        let mut before = ids
            .iter()
            .map(|id| {
                by_id
                    .get(id)
                    .copied()
                    .cloned()
                    .ok_or_else(|| "A selected track is no longer in the library.".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let source = before[0].source;
        if before.iter().any(|track| track.source != source) {
            return Err("Tracks from different media types cannot be merged.".into());
        }
        let target_id = match target {
            TrackMergeTarget::Existing(id) => {
                let track = self
                    .tracks
                    .iter()
                    .find(|track| track.id == id)
                    .ok_or("The chosen recording is no longer in the library.")?;
                if track.source != source {
                    return Err("Choose a recording of the same media type.".into());
                }
                if !unique.contains(&id) {
                    before.push(track.clone());
                }
                id
            }
            TrackMergeTarget::New(track) => {
                if track.source != source
                    || track.uri.is_empty()
                    || self.known_tracks().any(|known| known.uri == track.uri)
                {
                    return Err(
                        "The chosen recording must be a new identity of the same media type."
                            .into(),
                    );
                }
                self.push(*track)
            }
        };
        let target = self
            .tracks
            .iter_mut()
            .find(|track| track.id == target_id)
            .expect("validated target");
        Self::apply_edit(target, &options.edit);
        target.rating = options.rating;
        target.play_count = match options.play_count {
            MergePlayCount::Highest => before
                .iter()
                .map(|track| track.play_count)
                .max()
                .unwrap_or(0),
            MergePlayCount::Sum => before
                .iter()
                .fold(0u32, |sum, track| sum.saturating_add(track.play_count)),
            MergePlayCount::Custom(value) => value,
        };
        target.added_at = before.iter().filter_map(|track| track.added_at).min();
        target.last_played_at = before.iter().filter_map(|track| track.last_played_at).max();
        let after = target.clone();
        let target_was_retained = !self.decisions.retained.insert(after.uri.clone());
        self.tracks
            .retain(|track| track.id == target_id || !unique.contains(&track.id));
        self.decisions.merges.push(TrackMerge {
            before,
            after,
            target_was_retained,
            merged_at: options.merged_at,
        });
        self.decisions
            .rebuild_aliases()
            .expect("merging live identities cannot create cycles");
        Ok(target_id)
    }

    /// Undo the latest merge of this live entry. Later overlay edits and added
    /// plays remain on the chosen recording; the other originals are restored.
    pub fn undo_track_merge(&mut self, id: TrackId) -> Result<Vec<TrackId>, String> {
        let current = self
            .tracks
            .iter()
            .find(|track| track.id == id)
            .cloned()
            .ok_or("Restore this track before undoing its merge.")?;
        let index = self
            .decisions
            .merges
            .iter()
            .rposition(|merge| merge.after.id == id)
            .ok_or("This track has no merge to undo.")?;
        let merge = self.decisions.merges.remove(index);
        let previous = merge.before.iter().find(|track| track.id == id);
        let mut restored = merge.before.clone();
        let added_plays = current.play_count.saturating_sub(merge.after.play_count);
        if let Some(previous) = previous {
            let mut target = current.clone();
            macro_rules! restore_unchanged {
                ($($field:ident),+ $(,)?) => { $(if target.$field == merge.after.$field { target.$field = previous.$field.clone(); })+ };
            }
            restore_unchanged!(
                name,
                art,
                alb,
                cat,
                orig_cat,
                rating,
                enabled,
                added_at,
                last_played_at
            );
            target.play_count = previous.play_count.saturating_add(added_plays);
            *restored
                .iter_mut()
                .find(|track| track.id == id)
                .expect("previous target") = target;
        } else if current != merge.after {
            let mut target = current;
            target.play_count = added_plays;
            restored.push(target);
        }
        if !merge.target_was_retained && previous.is_some() {
            self.decisions.retained.remove(&merge.after.uri);
        }
        self.tracks.retain(|track| track.id != id);
        let ids = restored.iter().map(|track| track.id).collect();
        self.tracks.extend(restored);
        if !self.tracks.iter().any(|track| track.uri == merge.after.uri) {
            self.decisions.retained.remove(&merge.after.uri);
        }
        self.decisions
            .rebuild_aliases()
            .expect("undoing a live target preserves the merge graph");
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::{export_json, import};
    use std::time::Duration;

    fn source(uri: &str, name: &str) -> NewTrack {
        NewTrack {
            uri: uri.into(),
            name: name.into(),
            art: "Artist".into(),
            alb: "Album".into(),
            cat: "Pop".into(),
            duration: Duration::from_secs(162),
            ..NewTrack::default()
        }
    }

    fn options(play_count: MergePlayCount) -> TrackMergeOptions {
        TrackMergeOptions {
            edit: TrackEdit {
                cat: Some("Rock".into()),
                ..TrackEdit::default()
            },
            rating: Rating::new(4),
            play_count,
            merged_at: 100,
        }
    }

    #[test]
    fn removed_tracks_survive_refresh_restart_and_explicit_restore_with_original_identity() {
        let mut library = Library::new();
        let id = library.add(source("one", "Original"));
        library
            .edit(
                id,
                TrackEdit {
                    name: Some("My title".into()),
                    ..TrackEdit::default()
                },
            )
            .unwrap();
        library.set_track_rating(id, Rating::new(5)).unwrap();
        library.record_play("one", 42);
        let mut original = library.get(id).unwrap().clone();
        library.remove_tracks(&[id]).unwrap();
        assert_eq!(library.upsert(source("one", "Provider title")), id);
        assert_eq!(library.add_all([source("one", "Provider title")]), 0);
        library.upsert_all([source("one", "Provider title")]);
        assert!(library.tracks().is_empty());
        assert!(library.record_play("one", 50));
        original.play_count += 1;
        original.last_played_at = Some(50);
        let mut reopened = import(&export_json(&library)).unwrap();
        assert_eq!(reopened.restore_track("one"), Some(id));
        assert_eq!(reopened.get(id), Some(&original));
        assert!(reopened.is_retained("one"));
        assert!(reopened.add(source("two", "New")).0 > id.0);
    }

    #[test]
    fn three_way_merge_routes_future_plays_and_undo_preserves_later_edits_and_plays() {
        let mut library = Library::new();
        let ids = ["one", "two", "three"].map(|uri| library.add(source(uri, uri)));
        for (uri, plays) in [("one", 114), ("two", 0), ("three", 18)] {
            library.merge_history_absolute(uri, Some(plays), Some(10), Some(50));
        }
        library
            .merge_tracks(
                &ids,
                TrackMergeTarget::Existing(ids[0]),
                options(MergePlayCount::Highest),
            )
            .unwrap();
        assert_eq!(library.tracks().len(), 1);
        assert_eq!(library.get(ids[1]).unwrap().id, ids[0]);
        assert_eq!(library.tracks_by_uri()["three"].play_count, 114);
        library.upsert_all([
            source("two", "Must not replace target"),
            source("three", "Must not reappear"),
        ]);
        assert_eq!(library.get(ids[0]).unwrap().name, "one");
        assert_eq!(library.tracks().len(), 1);
        let mut library = import(&export_json(&library)).unwrap();
        assert!(library.record_play("two", 200));
        assert!(library.record_play("three", 201));
        library
            .edit(
                ids[0],
                TrackEdit {
                    name: Some("Later edit".into()),
                    ..TrackEdit::default()
                },
            )
            .unwrap();
        library.undo_track_merge(ids[0]).unwrap();
        assert_eq!(library.tracks().len(), 3);
        assert_eq!(library.get(ids[0]).unwrap().play_count, 116);
        assert_eq!(library.get(ids[0]).unwrap().name, "Later edit");
        assert_eq!(library.get(ids[0]).unwrap().cat, "Pop");
        assert_eq!(library.get(ids[0]).unwrap().last_played_at, Some(201));
        assert_eq!(library.get(ids[2]).unwrap().play_count, 18);
        assert!(library.track_aliases().is_empty());
        assert_eq!(import(&export_json(&library)).unwrap(), library);
    }

    #[test]
    fn nested_merges_and_a_new_target_can_be_undone_in_order() {
        let mut library = Library::new();
        let ids = ["one", "two", "three"].map(|uri| library.add(source(uri, uri)));
        library.merge_history_absolute("one", Some(10), None, None);
        library.merge_history_absolute("two", Some(20), None, None);
        library
            .merge_tracks(
                &ids[..2],
                TrackMergeTarget::Existing(ids[0]),
                options(MergePlayCount::Sum),
            )
            .unwrap();
        let new = library
            .merge_tracks(
                &[ids[0], ids[2]],
                TrackMergeTarget::New(Box::new(source("new", "Chosen"))),
                options(MergePlayCount::Custom(25)),
            )
            .unwrap();
        assert_eq!(library.canonical_uri("two"), "new");
        assert_eq!(library.get(new).unwrap().play_count, 25);
        let mut library = import(&export_json(&library)).unwrap();
        library.undo_track_merge(new).unwrap();
        assert!(library.get(new).is_none());
        assert_eq!(library.canonical_uri("two"), "one");
        assert_eq!(library.get(ids[0]).unwrap().play_count, 30);
        library.undo_track_merge(ids[0]).unwrap();
        assert_eq!(library.get(ids[0]).unwrap().play_count, 10);
        assert_eq!(library.get(ids[1]).unwrap().play_count, 20);
        assert_eq!(library.tracks().len(), 3);
    }

    #[test]
    fn additive_backup_preserves_disjoint_decisions_and_existing_conflicting_groups() {
        let mut incoming = Library::new();
        let ids = ["one", "two", "hidden"].map(|uri| incoming.add(source(uri, uri)));
        incoming
            .merge_tracks(
                &ids[..2],
                TrackMergeTarget::Existing(ids[0]),
                options(MergePlayCount::Sum),
            )
            .unwrap();
        incoming.remove_tracks(&[ids[2]]).unwrap();
        let mut target = Library::new();
        let existing = target.add(source("unrelated", "Keep"));
        target.merge(incoming.clone());
        let merged = target.get_by_uri("two").unwrap().id;
        assert_ne!(merged, existing);
        assert_eq!(target.removed_tracks().len(), 1);
        assert_eq!(target.remove_uris(&["one".into()]), 0);
        let mut target = import(&export_json(&target)).unwrap();
        target.undo_track_merge(merged).unwrap();
        assert_eq!(target.tracks().len(), 3);
        assert!(target.restore_track("hidden").is_some());
        assert_eq!(import(&export_json(&target)).unwrap(), target);

        let mut conflicting = Library::new();
        let existing = conflicting.add(source("two", "Local edit"));
        conflicting.merge(incoming);
        assert_eq!(conflicting.get(existing).unwrap().name, "Local edit");
        assert!(conflicting.get_by_uri("one").is_none());
        assert!(conflicting.track_aliases().is_empty());
        assert_eq!(conflicting.removed_tracks().len(), 1);
        assert_eq!(import(&export_json(&conflicting)).unwrap(), conflicting);
    }

    #[test]
    fn invalid_decisions_and_stale_selections_do_not_lose_records() {
        let mut library = Library::new();
        let ids = ["one", "two"].map(|uri| library.add(source(uri, uri)));
        let before = library.clone();
        assert!(
            library
                .merge_tracks(
                    &[ids[0], TrackId(999)],
                    TrackMergeTarget::Existing(ids[0]),
                    options(MergePlayCount::Sum)
                )
                .is_err()
        );
        assert!(library.remove_tracks(&[ids[0], TrackId(999)]).is_err());
        assert_eq!(library, before);
        library
            .merge_tracks(
                &ids,
                TrackMergeTarget::Existing(ids[0]),
                options(MergePlayCount::Sum),
            )
            .unwrap();
        let mut data = serde_json::to_value(&library).unwrap();
        data["tracks"] = serde_json::json!([]);
        assert!(serde_json::from_value::<Library>(data).is_err());
        let mut data = serde_json::to_value(&library).unwrap();
        let duplicate = data["decisions"]["merges"][0].clone();
        data["decisions"]["merges"]
            .as_array_mut()
            .unwrap()
            .push(duplicate);
        assert!(serde_json::from_value::<Library>(data).is_err());
        library.remove_tracks(&[ids[0]]).unwrap();
        let mut library = import(&export_json(&library)).unwrap();
        library.upsert_all([source("two", "Still merged")]);
        assert!(library.tracks().is_empty());
        assert_eq!(library.restore_track("two"), Some(ids[0]));
        assert_eq!(library.tracks().len(), 1);
    }
}

use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
};

use crate::{AudioError, AudioInfo, FileTags, probe, read_basic_tags};

const EXTENSIONS: &[&str] = &[
    "aac", "aif", "aiff", "flac", "m4a", "mp3", "mp4", "oga", "ogg", "opus", "wav", "webm",
];

pub const MAX_SCAN_ROOTS: usize = 128;
pub const MAX_SCAN_FILES: usize = 100_000;
pub const MAX_SCAN_DEPTH: usize = 64;
pub const MAX_SCAN_FAILURE_DETAILS: usize = 100;

/// A decodable local file and its read-only metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedFile {
    pub canonical_path: PathBuf,
    pub info: AudioInfo,
    pub tags: FileTags,
}

/// A filesystem path that could not be inspected while scanning.
#[derive(Debug)]
pub struct ScanFailure {
    pub path: PathBuf,
    pub error: io::Error,
}

/// Supported files and path-specific failures found during one scan.
#[derive(Debug, Default)]
pub struct ScanResult {
    pub files: Vec<PathBuf>,
    pub failures: Vec<ScanFailure>,
    pub failure_count: usize,
    pub duplicates: usize,
}

/// Finds supported files without following directory symlinks or file symlinks
/// that escape their selected directory root.
pub fn scan_paths(paths: impl IntoIterator<Item = impl AsRef<Path>>) -> ScanResult {
    scan_paths_with_limits(paths, ScanLimits::default())
}

fn scan_paths_with_limits(
    paths: impl IntoIterator<Item = impl AsRef<Path>>,
    limits: ScanLimits,
) -> ScanResult {
    let roots = paths
        .into_iter()
        .map(|path| path.as_ref().to_owned())
        .collect::<Vec<_>>();
    let mut scan = Scanner::new(limits);
    for root in roots.iter().take(limits.roots) {
        if scan.file_limit_hit {
            break;
        }
        let canonical_root = match fs::canonicalize(root) {
            Ok(path) => path,
            Err(error) => {
                scan.fail(root, error);
                continue;
            }
        };
        if !scan.roots.insert(canonical_root.clone()) {
            if canonical_root.is_file() {
                scan.duplicates += 1;
            }
            continue;
        }
        scan.visit(root, &canonical_root, 0);
    }
    if roots.len() > limits.roots {
        scan.fail(
            &roots[limits.roots],
            io::Error::other(format!(
                "scan root limit of {} exceeded; {} roots skipped",
                limits.roots,
                roots.len() - limits.roots
            )),
        );
    }
    scan.finish()
}

/// Finds supported files below one selected path.
pub fn scan_path(path: &Path) -> ScanResult {
    scan_paths([path])
}

/// Validates and reads one file without modifying it.
pub fn import_file(path: impl AsRef<Path>) -> Result<ImportedFile, AudioError> {
    let path = fs::canonicalize(path)?;
    let info = probe(&path)?;
    let tags = read_basic_tags(&path).unwrap_or_else(|error| {
        log::debug!("audio tags unavailable for {}: {error}", path.display());
        FileTags {
            duration: info.duration.unwrap_or_default(),
            ..Default::default()
        }
    });
    Ok(ImportedFile {
        info,
        tags,
        canonical_path: path,
    })
}

#[derive(Clone, Copy)]
struct ScanLimits {
    roots: usize,
    files: usize,
    depth: usize,
    failure_details: usize,
}

impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            roots: MAX_SCAN_ROOTS,
            files: MAX_SCAN_FILES,
            depth: MAX_SCAN_DEPTH,
            failure_details: MAX_SCAN_FAILURE_DETAILS,
        }
    }
}

struct Scanner {
    limits: ScanLimits,
    roots: BTreeSet<PathBuf>,
    files: BTreeSet<PathBuf>,
    failures: Vec<ScanFailure>,
    failure_count: usize,
    duplicates: usize,
    file_limit_hit: bool,
}

impl Scanner {
    fn new(limits: ScanLimits) -> Self {
        Self {
            limits,
            roots: BTreeSet::new(),
            files: BTreeSet::new(),
            failures: Vec::new(),
            failure_count: 0,
            duplicates: 0,
            file_limit_hit: false,
        }
    }

    fn visit(&mut self, path: &Path, root: &Path, depth: usize) {
        if self.file_limit_hit || hidden(path) {
            return;
        }
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) => {
                self.fail(path, error);
                return;
            }
        };
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            let target = match fs::canonicalize(path) {
                Ok(target) => target,
                Err(error) => {
                    self.fail(path, error);
                    return;
                }
            };
            let target_metadata = match fs::metadata(path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    self.fail(path, error);
                    return;
                }
            };
            if target_metadata.is_dir() {
                return;
            }
            if !target_metadata.is_file() {
                self.fail(path, io::Error::other("special files are not scanned"));
            } else if !target.starts_with(root) {
                self.fail(path, io::Error::other("file symlink escapes selected root"));
            } else if supported(path) {
                self.add_file(path, target);
            }
        } else if metadata.is_dir() {
            if depth == self.limits.depth {
                self.fail(
                    path,
                    io::Error::other(format!("scan depth limit of {} reached", self.limits.depth)),
                );
                return;
            }
            let entries = match fs::read_dir(path) {
                Ok(entries) => entries,
                Err(error) => {
                    self.fail(path, error);
                    return;
                }
            };
            for entry in entries {
                if self.file_limit_hit {
                    break;
                }
                match entry {
                    Ok(entry) => self.visit(&entry.path(), root, depth + 1),
                    Err(error) => self.fail(path, error),
                }
            }
        } else if metadata.is_file() {
            if supported(path) {
                match fs::canonicalize(path) {
                    Ok(canonical) => self.add_file(path, canonical),
                    Err(error) => self.fail(path, error),
                }
            }
        } else {
            self.fail(path, io::Error::other("special files are not scanned"));
        }
    }

    fn add_file(&mut self, source: &Path, canonical: PathBuf) {
        if self.files.contains(&canonical) {
            self.duplicates += 1;
            return;
        }
        if self.files.len() == self.limits.files {
            self.file_limit_hit = true;
            self.fail(
                source,
                io::Error::other(format!("scan file limit of {} reached", self.limits.files)),
            );
            return;
        }
        self.files.insert(canonical);
    }

    fn fail(&mut self, path: &Path, error: io::Error) {
        self.failure_count += 1;
        if self.failures.len() < self.limits.failure_details {
            self.failures.push(ScanFailure {
                path: path.to_owned(),
                error,
            });
        }
    }

    fn finish(mut self) -> ScanResult {
        self.failures
            .sort_by(|left, right| left.path.cmp(&right.path));
        ScanResult {
            files: self.files.into_iter().collect(),
            failures: self.failures,
            failure_count: self.failure_count,
            duplicates: self.duplicates,
        }
    }
}

fn hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.'))
}

fn supported(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn file_limit_stops_the_scan() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("visible");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("one.mp3"), []).unwrap();
        fs::write(root.join("two.mp3"), []).unwrap();
        let scan = scan_paths_with_limits(
            [&root],
            ScanLimits {
                files: 1,
                ..ScanLimits::default()
            },
        );

        assert_eq!(scan.files.len(), 1);
        assert_eq!(scan.failures.len(), 1);
        assert_eq!(scan.failure_count, 1);
        assert!(
            scan.failures
                .iter()
                .any(|failure| failure.error.to_string().contains("file limit"))
        );
    }

    #[test]
    fn root_limit_reports_skipped_roots() {
        let directory = tempdir().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();

        let scan = scan_paths_with_limits(
            [&first, &second],
            ScanLimits {
                roots: 1,
                ..ScanLimits::default()
            },
        );

        assert_eq!(scan.failure_count, 1);
        assert!(scan.failures[0].error.to_string().contains("root limit"));
    }

    #[test]
    fn failure_details_are_truncated_but_counted() {
        let directory = tempdir().unwrap();
        let missing = (0..3)
            .map(|index| directory.path().join(format!("missing-{index}")))
            .collect::<Vec<_>>();

        let scan = scan_paths_with_limits(
            &missing,
            ScanLimits {
                failure_details: 1,
                ..ScanLimits::default()
            },
        );

        assert_eq!(scan.failure_count, 3);
        assert_eq!(scan.failures.len(), 1);
    }

    #[test]
    fn depth_limit_is_reported_without_descending() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("visible");
        let nested = root.join("nested");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("song.mp3"), []).unwrap();

        let scan = scan_paths_with_limits(
            [&root],
            ScanLimits {
                depth: 0,
                ..ScanLimits::default()
            },
        );

        assert!(scan.files.is_empty());
        assert_eq!(scan.failure_count, 1);
        assert!(scan.failures[0].error.to_string().contains("depth limit"));
    }
}

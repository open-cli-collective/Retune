use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[cfg(windows)]
static ATOMIC_REPLACE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) fn read_limited(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    read_limited_file(File::open(path)?, path, limit)
}

pub(crate) fn read_limited_file(file: File, path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let length = file.metadata()?.len();
    if length > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} exceeds its {limit}-byte limit", path.display()),
        ));
    }
    let mut bytes = Vec::with_capacity(length.try_into().unwrap_or(usize::MAX));
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} exceeds its {limit}-byte limit", path.display()),
        ));
    }
    Ok(bytes)
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8], mode: Option<u32>) -> io::Result<()> {
    atomic_write_inner(
        path,
        bytes,
        mode,
        #[cfg(test)]
        None,
    )
}

fn atomic_write_inner(
    path: &Path,
    bytes: &[u8],
    mode: Option<u32>,
    #[cfg(test)] hooks: Option<&TestHooks<'_>>,
) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "store path has no parent"))?;
    fs::create_dir_all(parent)?;
    #[cfg(test)]
    fail_at(hooks, FailureStage::Create)?;
    let (temporary, mut file) = create_temporary(path, mode)?;
    #[cfg(test)]
    if let Some(created) = hooks.and_then(|hooks| hooks.created) {
        created(&temporary);
    }
    let result = (|| {
        #[cfg(test)]
        fail_at(hooks, FailureStage::Write)?;
        file.write_all(bytes)?;
        #[cfg(test)]
        fail_at(hooks, FailureStage::Sync)?;
        file.sync_all()?;
        drop(file);
        #[cfg(test)]
        fail_at(hooks, FailureStage::Rename)?;
        replace(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureStage {
    Create,
    Write,
    Sync,
    Rename,
}

#[cfg(test)]
pub(crate) fn atomic_write_with_failure(
    path: &Path,
    bytes: &[u8],
    mode: Option<u32>,
    failure: FailureStage,
) -> io::Result<()> {
    atomic_write_inner(
        path,
        bytes,
        mode,
        Some(&TestHooks {
            failure: Some(failure),
            created: None,
        }),
    )
}

#[cfg(test)]
struct TestHooks<'a> {
    failure: Option<FailureStage>,
    created: Option<&'a dyn Fn(&Path)>,
}

#[cfg(test)]
fn fail_at(hooks: Option<&TestHooks<'_>>, stage: FailureStage) -> io::Result<()> {
    if hooks.and_then(|hooks| hooks.failure) == Some(stage) {
        Err(io::Error::other("injected atomic-write failure"))
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    // ponytail: serialize Windows replaces globally; use per-path locks if write throughput matters.
    let _guard = ATOMIC_REPLACE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // SAFETY: Both pointers reference live, nul-terminated UTF-16 buffers for the call.
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn create_temporary(path: &Path, mode: Option<u32>) -> io::Result<(PathBuf, File)> {
    loop {
        let mut name = path
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "store path has no name"))?
            .to_os_string();
        name.push(format!(
            ".tmp-{}-{}",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let temporary = path.with_file_name(name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if let Some(mode) = mode {
            options.mode(mode);
        }
        #[cfg(not(unix))]
        let _ = mode;
        match options.open(&temporary) {
            Ok(file) => {
                #[cfg(unix)]
                if let Some(mode) = mode {
                    if let Err(error) = file.set_permissions(fs::Permissions::from_mode(mode)) {
                        drop(file);
                        let _ = fs::remove_file(&temporary);
                        return Err(error);
                    }
                }
                return Ok((temporary, file));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Seek, SeekFrom};
    use std::sync::{Arc, Barrier};

    use super::*;

    #[test]
    fn concurrent_writes_use_unique_temporaries_and_leave_one_complete_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let barrier = Arc::new(Barrier::new(8));
        let writers = (0_u8..8)
            .map(|byte| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let bytes = vec![byte; 1024 * 1024];
                    barrier.wait();
                    atomic_write(&path, &bytes, None).unwrap();
                    bytes
                })
            })
            .collect::<Vec<_>>();
        let inputs = writers
            .into_iter()
            .map(|writer| writer.join().unwrap())
            .collect::<Vec<_>>();

        assert!(inputs.contains(&fs::read(&path).unwrap()));
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);

        let unwritable_target = directory.path().join("directory-target");
        fs::create_dir(&unwritable_target).unwrap();
        assert!(atomic_write(&unwritable_target, b"failure", None).is_err());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
    }

    #[test]
    fn every_atomic_write_failure_stage_preserves_the_previous_file_and_cleans_up() {
        for stage in [
            FailureStage::Create,
            FailureStage::Write,
            FailureStage::Sync,
            FailureStage::Rename,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("state.json");
            fs::write(&path, b"previous").unwrap();

            assert!(atomic_write_inner(
                &path,
                b"replacement",
                None,
                Some(&TestHooks {
                    failure: Some(stage),
                    created: None,
                }),
            )
            .is_err());

            assert_eq!(fs::read(&path).unwrap(), b"previous");
            assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        }
    }

    #[test]
    fn limited_reads_accept_the_limit_and_reject_larger_and_sparse_files() {
        let directory = tempfile::tempdir().unwrap();
        let exact = directory.path().join("exact");
        fs::write(&exact, b"12345678").unwrap();
        assert_eq!(read_limited(&exact, 8).unwrap(), b"12345678");

        let larger = directory.path().join("larger");
        fs::write(&larger, b"123456789").unwrap();
        assert_eq!(
            read_limited(&larger, 8).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let sparse = directory.path().join("sparse");
        let mut file = File::create(&sparse).unwrap();
        file.seek(SeekFrom::Start(1024 * 1024)).unwrap();
        file.write_all(&[0]).unwrap();
        assert_eq!(
            read_limited(&sparse, 8).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[test]
    fn secret_temporary_and_final_files_are_owner_only() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("secret.json");
        let (temporary, file) = create_temporary(&path, Some(0o600)).unwrap();

        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        drop(file);
        fs::remove_file(temporary).unwrap();

        atomic_write(&path, b"secret", Some(0o600)).unwrap();
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn secret_temporary_is_owner_only_while_write_is_paused_and_after_failure() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("secret.json");
        let inspect = |temporary: &Path| {
            assert_eq!(
                fs::metadata(temporary).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert!(!path.exists());
        };

        assert!(atomic_write_inner(
            &path,
            b"credentials",
            Some(0o600),
            Some(&TestHooks {
                failure: Some(FailureStage::Write),
                created: Some(&inspect),
            }),
        )
        .is_err());

        assert!(!path.exists());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}

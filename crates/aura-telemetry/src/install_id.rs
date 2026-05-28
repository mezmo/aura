//! Anonymous install identifier.
//!
//! A single UUID v4, persisted at a stable path on disk, used as the
//! PostHog `distinct_id` on every event. It is **never** sent as a
//! property of any event (see the structural guarantee in
//! [`crate::properties`]) and **never** linked to a user identity:
//! `TelemetryHandle` exposes no method that accepts a user identifier.
//!
//! Deleting the file resets your install identity — that is the
//! user-facing "reset" path, documented in `docs/telemetry.md`.
//!
//! ## Concurrency
//!
//! Two Aura processes can race to populate the install-id on first
//! launch (e.g. web-server and CLI started simultaneously by a
//! supervisor on a fresh box). The persistence path is built so every
//! caller converges on a single UUID:
//!
//! 1. Each writer creates a uniquely-named tmp file containing its
//!    candidate UUID, then `sync_all`s it.
//! 2. The publication step is [`std::fs::hard_link`], which atomically
//!    attaches the tmp file's inode at the final path **or fails with
//!    `AlreadyExists`** if another writer got there first. Unlike
//!    `O_CREAT|O_EXCL` followed by `write`, a racing reader can never
//!    observe an empty file mid-write.
//! 3. The losers re-read the final path and return whatever the winner
//!    wrote.
//! 4. Tmp files are always cleaned up.
//!
//! Corrupt-file recovery walks the same loop a bounded number of
//! times: read → if invalid, unlink and retry, up to
//! [`MAX_RECOVERY_ATTEMPTS`]. Concurrent recoverers each use a
//! distinct tmp filename and converge through the hard-link gate.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use uuid::Uuid;

const MAX_RECOVERY_ATTEMPTS: usize = 8;

/// Errors that may arise while resolving or persisting the install ID.
#[derive(Debug, thiserror::Error)]
pub enum InstallIdError {
    #[error("install-id i/o: {0}")]
    Io(#[from] io::Error),
    #[error("install-id path has no parent directory: {0}")]
    BadParent(PathBuf),
    #[error("install-id path has no file name: {0}")]
    BadFileName(PathBuf),
    #[error(
        "install-id at {path} could not be created or repaired after {MAX_RECOVERY_ATTEMPTS} attempts"
    )]
    Unrecoverable { path: PathBuf },
}

/// Read the install UUID at `path`, creating it if missing or
/// regenerating it if the file's contents are not a valid UUID.
///
/// Concurrency-safe: many processes (or threads) calling
/// `read_or_create` against the same path always converge on the same
/// UUID, and the on-disk file always contains a parseable UUID at the
/// end of any caller's return.
pub fn read_or_create(path: &Path) -> Result<Uuid, InstallIdError> {
    let parent = path
        .parent()
        .ok_or_else(|| InstallIdError::BadParent(path.to_path_buf()))?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| InstallIdError::BadFileName(path.to_path_buf()))?;
    fs::create_dir_all(parent)?;

    for _ in 0..MAX_RECOVERY_ATTEMPTS {
        if let Some(uuid) = read_existing(path)? {
            return Ok(uuid);
        }
        // Either missing or corrupt. Write a fully-formed tmp file
        // first; then publish via hard_link, which fails atomically if
        // another writer got there before us.
        let candidate = Uuid::new_v4();
        let tmp = parent.join(format!(".{file_name}.{}.tmp", Uuid::new_v4()));
        if let Err(e) = write_private(&tmp, candidate.to_string().as_bytes()) {
            let _ = fs::remove_file(&tmp);
            return Err(e.into());
        }
        let link_result = fs::hard_link(&tmp, path);
        // The tmp file is no longer needed in either branch: on success
        // its inode lives on at `path` via the hard link, and on
        // failure it's an orphan to clean up. Errors from removal are
        // ignored — the file is small and the OS will reap it on
        // next boot at worst.
        let _ = fs::remove_file(&tmp);

        match link_result {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                // Another writer beat us. Re-read; if it's a valid
                // UUID return it. If it's still corrupt (possible if
                // the file was already corrupt before we started, and
                // no one has cleaned it up yet) unlink and loop. The
                // unlink may race with another recoverer's unlink —
                // both ignoring the error is fine because the next
                // iteration's read_existing will either see None and
                // try to create, or see a valid UUID someone wrote
                // since.
                match read_existing(path)? {
                    Some(uuid) => return Ok(uuid),
                    None => {
                        let _ = fs::remove_file(path);
                    }
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err(InstallIdError::Unrecoverable {
        path: path.to_path_buf(),
    })
}

fn read_existing(path: &Path) -> Result<Option<Uuid>, InstallIdError> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(Uuid::parse_str(contents.trim()).ok()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::tempdir;

    #[test]
    fn creates_new_install_id_when_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("install-id");
        let uuid = read_or_create(&path).expect("create new");
        let on_disk = fs::read_to_string(&path).unwrap();
        let on_disk_uuid = Uuid::parse_str(on_disk.trim()).unwrap();
        assert_eq!(uuid, on_disk_uuid);
    }

    #[test]
    fn second_call_returns_same_id() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("install-id");
        let first = read_or_create(&path).unwrap();
        let second = read_or_create(&path).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn corrupt_file_is_regenerated() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("install-id");
        fs::write(&path, "not a uuid").unwrap();

        let uuid = read_or_create(&path).expect("regenerate over corrupt");
        let on_disk = fs::read_to_string(&path).unwrap();
        let on_disk_uuid = Uuid::parse_str(on_disk.trim()).unwrap();
        assert_eq!(uuid, on_disk_uuid);
        assert_ne!(on_disk, "not a uuid");
    }

    #[test]
    fn trailing_whitespace_is_tolerated() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("install-id");
        let expected = Uuid::new_v4();
        fs::write(&path, format!("{expected}\n\n")).unwrap();
        let got = read_or_create(&path).unwrap();
        assert_eq!(got, expected);
    }

    #[test]
    fn creates_parent_directory_if_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("dir").join("install-id");
        let uuid = read_or_create(&path).expect("create nested");
        assert!(path.exists());
        let again = read_or_create(&path).unwrap();
        assert_eq!(uuid, again);
    }

    #[cfg(unix)]
    #[test]
    fn install_id_file_is_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("install-id");
        let _ = read_or_create(&path).unwrap();
        let perms = fs::metadata(&path).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o600);
    }

    #[test]
    fn new_uuid_is_version_4() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("install-id");
        let uuid = read_or_create(&path).unwrap();
        assert_eq!(uuid.get_version_num(), 4, "expected UUID v4, got {uuid}");
    }

    /// Many threads racing to create the install-id at a single path
    /// must all return the same UUID, and that UUID must match the
    /// on-disk file. This catches the historical race where two writers
    /// using a shared `.install-id.tmp` path could stomp each other,
    /// and the subsequent O_EXCL-then-write race where a racing reader
    /// could see an empty file and unlink the winner's in-flight write.
    #[test]
    fn concurrent_first_launch_converges_on_one_uuid() {
        let dir = tempdir().unwrap();
        let path = Arc::new(dir.path().join("install-id"));
        let mut handles = Vec::with_capacity(32);
        for _ in 0..32 {
            let p = path.clone();
            handles.push(std::thread::spawn(move || {
                read_or_create(&p).expect("read_or_create")
            }));
        }
        let results: Vec<Uuid> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let first = results[0];
        for u in &results {
            assert_eq!(*u, first, "all threads must agree on the install UUID");
        }
        let on_disk = fs::read_to_string(&*path).unwrap();
        let on_disk_uuid = Uuid::parse_str(on_disk.trim()).unwrap();
        assert_eq!(
            on_disk_uuid, first,
            "on-disk UUID must match the value returned to callers"
        );
    }

    /// Many threads racing to repair a corrupt file must all converge
    /// on the same UUID, and no stale tmp files may be left behind.
    #[test]
    fn concurrent_corrupt_recovery_converges() {
        let dir = tempdir().unwrap();
        let path = Arc::new(dir.path().join("install-id"));
        fs::write(&*path, "totally garbage").unwrap();

        let mut handles = Vec::with_capacity(16);
        for _ in 0..16 {
            let p = path.clone();
            handles.push(std::thread::spawn(move || {
                read_or_create(&p).expect("read_or_create")
            }));
        }
        let results: Vec<Uuid> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let on_disk_uuid = Uuid::parse_str(fs::read_to_string(&*path).unwrap().trim()).unwrap();
        for u in results {
            assert_eq!(
                u, on_disk_uuid,
                "every caller must converge on the on-disk UUID"
            );
        }
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "no .tmp files should remain after recovery"
        );
    }
}

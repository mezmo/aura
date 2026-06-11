//! Anonymous install identifier.
//!
//! A single UUID v4, persisted at a stable path on disk, used as the
//! PostHog `distinct_id` on every event. It is **never** sent as a
//! property of any event (see the structural guarantee in
//! [`crate::properties`]) and **never** linked to a user identity:
//! `TelemetryHandle` exposes no method that accepts a user identifier.
//!
//! ## Three on-disk states
//!
//! | On-disk state | Behaviour |
//! |---|---|
//! | **Missing** | Publish a fresh UUID v4 atomically via [`std::fs::hard_link`]. Concurrent first-launchers all converge on the winner. |
//! | **Valid UUID** | Return it. |
//! | **Corrupt** | Return a fresh per-run UUID **without** modifying the file. |
//!
//! The corrupt branch is deliberately a no-op on disk. An earlier draft
//! tried to auto-recover by unlinking and republishing in a retry loop;
//! review found a multi-process race where one repairer could
//! `remove_file` after another repairer had already published, leaving
//! a third caller holding a UUID that the file no longer contained.
//! Recovery without a cross-process lock cannot guarantee convergence,
//! and a botched recovery would split a real install's reported
//! identity. The trade we make instead: a corrupt file under-counts
//! that install (every run gets a fresh per-run ID) until the user
//! resets it with `rm <path>` — which is the documented reset
//! procedure in `docs/telemetry.md` anyway. Identity is never split.
//!
//! ## Concurrency for the missing case
//!
//! Each writer first creates a uniquely-named tmp file containing its
//! candidate UUID and `sync_all`s it. The publication step is
//! [`std::fs::hard_link`], which atomically attaches the tmp file's
//! inode at the final path **or fails with `AlreadyExists`** if
//! another writer got there first. Unlike `O_CREAT|O_EXCL` followed by
//! `write`, a racing reader can never observe an empty file mid-write.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use uuid::Uuid;

/// Errors that may arise while resolving the install ID.
#[derive(Debug, thiserror::Error)]
pub enum InstallIdError {
    #[error("install-id i/o: {0}")]
    Io(#[from] io::Error),
    #[error("install-id path has no parent directory: {0}")]
    BadParent(PathBuf),
    #[error("install-id path has no file name: {0}")]
    BadFileName(PathBuf),
}

/// Read the install UUID at `path`, creating it if missing. Returns a
/// fresh per-run UUID **without** modifying the file when the file
/// exists but cannot be parsed.
///
/// Concurrency-safe for the missing-file case: many processes calling
/// `read_or_create` against the same path converge on the same UUID
/// via [`std::fs::hard_link`]. See the module-level docs for why the
/// corrupt-file case is deliberately non-converging.
pub fn read_or_create(path: &Path) -> Result<Uuid, InstallIdError> {
    let parent = path
        .parent()
        .ok_or_else(|| InstallIdError::BadParent(path.to_path_buf()))?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| InstallIdError::BadFileName(path.to_path_buf()))?;
    fs::create_dir_all(parent)?;

    // A single read decides the path forward. Branching on the result
    // rather than retrying keeps the corrupt-file path off any code
    // that could race with a peer publisher.
    match fs::read_to_string(path) {
        Ok(contents) => {
            return Ok(parse_or_per_run(&contents, path));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // Fall through to publication.
        }
        Err(e) => return Err(e.into()),
    }

    // First-launch publication. Write a fully-formed tmp file, then
    // attach it at `path` with `hard_link`. The atomic-or-fail
    // semantic of `hard_link` is exactly what we need: at most one
    // caller wins; losers re-read what the winner wrote.
    let candidate = Uuid::new_v4();
    let tmp = parent.join(format!(".{file_name}.{}.tmp", Uuid::new_v4()));
    if let Err(e) = write_private(&tmp, candidate.to_string().as_bytes()) {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    let link_result = fs::hard_link(&tmp, path);
    // The tmp file is no longer needed in either branch: on success it
    // lives on at `path` via the hard link; on failure it's an orphan.
    // Either way we delete its directory entry now.
    let _ = fs::remove_file(&tmp);

    match link_result {
        Ok(()) => Ok(candidate),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            // Lost the publication race. Read whatever the winner
            // wrote; if even that is unparseable (the winner could
            // have lost their own race to a third party who somehow
            // wrote garbage) fall back to per-run.
            match fs::read_to_string(path) {
                Ok(contents) => Ok(parse_or_per_run(&contents, path)),
                Err(e) => Err(e.into()),
            }
        }
        Err(e) => Err(e.into()),
    }
}

fn parse_or_per_run(contents: &str, path: &Path) -> Uuid {
    match Uuid::parse_str(contents.trim()) {
        Ok(uuid) => uuid,
        Err(_) => {
            tracing::debug!(
                path = %path.display(),
                "install-id file is corrupt; using a per-run UUID. \
                 Reset with `rm {}` per docs/telemetry.md.",
                path.display()
            );
            Uuid::new_v4()
        }
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
    /// on-disk file. This covers the historical race where two writers
    /// using a shared tmp path could stomp each other, and the
    /// follow-on O_EXCL race where a racing reader could observe an
    /// empty file mid-write.
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

    /// Corrupt-file behaviour is **non-converging by design**. Each
    /// caller returns a fresh per-run UUID; the file is left untouched.
    /// This is the trade we make to prevent a multi-process recovery
    /// race from splitting a real install's reported identity. See the
    /// module-level docs.
    #[test]
    fn corrupt_file_returns_per_run_uuid_without_touching_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("install-id");
        fs::write(&path, "definitely-not-a-uuid").unwrap();

        let uuid_a = read_or_create(&path).expect("call a");
        let uuid_b = read_or_create(&path).expect("call b");

        // On-disk content is untouched.
        let on_disk = fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "definitely-not-a-uuid");
        // Each call returned a fresh per-run UUID.
        assert_ne!(uuid_a, uuid_b);
        assert_eq!(uuid_a.get_version_num(), 4);
        assert_eq!(uuid_b.get_version_num(), 4);
    }

    /// Concurrent calls against a corrupt file must not modify the file
    /// (no race-with-publish that could let a later loop produce a
    /// second UUID). No `.tmp` files are left behind.
    #[test]
    fn concurrent_corrupt_calls_do_not_modify_file() {
        let dir = tempdir().unwrap();
        let path = Arc::new(dir.path().join("install-id"));
        let original = "totally garbage";
        fs::write(&*path, original).unwrap();

        let mut handles = Vec::with_capacity(16);
        for _ in 0..16 {
            let p = path.clone();
            handles.push(std::thread::spawn(move || {
                read_or_create(&p).expect("read_or_create")
            }));
        }
        let results: Vec<Uuid> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // File untouched.
        assert_eq!(fs::read_to_string(&*path).unwrap(), original);
        // Every caller got a valid v4 UUID, even if they differ.
        for u in &results {
            assert_eq!(u.get_version_num(), 4);
        }
        // No tmp orphans.
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "no .tmp files should remain after concurrent corrupt-file reads"
        );
    }
}

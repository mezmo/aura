//! Debris garbage collection: holder-run, at claim time, epoch-scoped.
//!
//! What is collected is *debris only* — files no manifest references:
//! zombie writes (a thawed stale holder's post-steal writes into its old
//! epoch dir), aborted-turn debris, commit-unknown orphans, interrupted
//! temp renames. Committed, manifest-referenced artifacts are never
//! collected; compaction of the referenced set is a future product card.
//!
//! The load-bearing rule is structural, not temporal (invariant I3): a
//! sweep built from a claims-row read at epoch `k` may only delete under
//! epoch dirs strictly below `k`. A GC pass whose holder froze mid-sweep
//! and thawed after a steal therefore cannot touch the new holder's
//! files — they live at epochs `>= k+1` — no matter how stale its
//! keep-set is. GC unlinks are plain filesystem ops that Postgres does
//! not fence; this scope rule is what makes that safe.

use std::collections::BTreeSet;
use std::path::Path;

use crate::epoch::{Epoch, epoch_dir};
use crate::manifest::{ArtifactPath, Manifest};

/// What survives a sweep: every manifest-referenced path, plus the
/// sweeping claim's own epoch dir (its forthcoming uncommitted writes).
#[derive(Debug, Clone)]
pub(crate) struct KeepSet {
    referenced: BTreeSet<ArtifactPath>,
    own_epoch: Epoch,
}

impl KeepSet {
    /// Build the keep-set from the granted claim's manifest.
    pub(crate) fn from_manifest(manifest: &Manifest, own_epoch: Epoch) -> Self {
        Self {
            referenced: manifest.paths().cloned().collect(),
            own_epoch,
        }
    }
}

/// The deletion-scope rule as a type (I3): a sweep may only touch epoch
/// dirs strictly below the epoch of the claims-row read its keep-set
/// came from.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GcScope {
    below: Epoch,
}

impl GcScope {
    /// Scope a sweep to the epoch the claim was granted at.
    pub(crate) fn at_read(below: Epoch) -> Self {
        Self { below }
    }

    /// Whether a file under `epoch` is in scope for deletion. This *is*
    /// the rule; it is implemented, not a hole.
    pub(crate) fn permits(&self, epoch: Epoch) -> bool {
        epoch < self.below
    }
}

/// One holder-run sweep, assembled at claim time. Runs inside the claim:
/// the claim already serializes cross-pod access to the session, so no
/// sweep coordination is needed, and unlink-of-already-gone is a no-op
/// (idempotent).
#[derive(Debug)]
pub(crate) struct DebrisSweep {
    keep: KeepSet,
    scope: GcScope,
}

impl DebrisSweep {
    /// Assemble the sweep for a freshly granted claim: keep-set from the
    /// claim's manifest, scope from the claim's epoch.
    #[must_use]
    pub(crate) fn at_claim_time(manifest: &Manifest, claimed_epoch: Epoch) -> Self {
        Self {
            keep: KeepSet::from_manifest(manifest, claimed_epoch),
            scope: GcScope::at_read(claimed_epoch),
        }
    }

    /// Run the sweep: walk epoch dirs, unlink unreferenced files in
    /// scope, leave everything else. Concurrent with the claim's own
    /// first writes by construction (the scope excludes the claim's own
    /// epoch).
    ///
    /// # Errors
    /// `std::io::Error` on filesystem failures. A sweep failure does not
    /// fail the claim; debris accumulates for the next holder.
    pub(crate) async fn sweep(&self, session_root: &Path) -> Result<SweepOutcome, std::io::Error> {
        let mut outcome = SweepOutcome::default();
        let mut epoch = Epoch::initial();
        loop {
            let deletable = self.scope.permits(epoch);
            outcome.scanned_epochs += 1;
            sweep_epoch_dir(session_root, epoch, &self.keep, deletable, &mut outcome).await?;
            if epoch == self.keep.own_epoch {
                break;
            }
            epoch = match epoch.next() {
                Some(next) => next,
                None => break,
            };
        }
        Ok(outcome)
    }
}

/// Unlink debris under one epoch dir, recursing into subdirectories.
/// `deletable` is the scope verdict for the whole epoch (I3); within a
/// deletable epoch, a file is still kept when its session-root-relative
/// path is in the keep-set. Both a missing epoch dir and a
/// remove-raced-away file are no-ops, not errors (unlink is idempotent).
async fn sweep_epoch_dir(
    session_root: &Path,
    epoch: Epoch,
    keep: &KeepSet,
    deletable: bool,
    outcome: &mut SweepOutcome,
) -> Result<(), std::io::Error> {
    let mut pending = vec![epoch_dir(session_root, epoch)];
    while let Some(current) = pending.pop() {
        let mut entries = match tokio::fs::read_dir(&current).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        while let Some(entry) = entries.next_entry().await? {
            if entry.file_type().await?.is_dir() {
                pending.push(entry.path());
                continue;
            }
            let path = entry.path();
            let relative = path.strip_prefix(session_root).unwrap_or(&path);
            let referenced = ArtifactPath::parse(&relative.to_string_lossy())
                .is_ok_and(|candidate| keep.referenced.contains(&candidate));
            if !deletable || referenced {
                outcome.kept += 1;
                continue;
            }
            match tokio::fs::remove_file(&path).await {
                Ok(()) => outcome.removed += 1,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
        }
    }
    Ok(())
}

/// Sweep statistics. Diagnostic-only counters; nothing branches on them.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SweepOutcome {
    /// Epoch dirs walked.
    pub scanned_epochs: u64,
    /// Files unlinked.
    pub removed: u64,
    /// Files kept (referenced or out of scope).
    pub kept: u64,
}

#[cfg(test)]
mod tests {
    use crate::identity::TurnId;
    use crate::manifest::{Digest, ManifestEntry};

    use super::*;

    fn sample_entry(epoch: Epoch, bytes: u64) -> ManifestEntry {
        ManifestEntry {
            digest: Digest::from_bytes([7; 32]),
            turn: TurnId::new(),
            epoch,
            bytes,
        }
    }

    #[test]
    fn gc_keep_set_from_manifest() {
        let epoch = Epoch::initial();
        let referenced = ArtifactPath::parse("e1/keep.txt").expect("valid artifact path");
        let mut manifest = Manifest::empty();
        manifest
            .declare(referenced.clone(), sample_entry(epoch, 3))
            .expect("declare succeeds");

        let keep = KeepSet::from_manifest(&manifest, epoch);
        assert_eq!(keep.referenced, BTreeSet::from([referenced]));
        assert_eq!(keep.own_epoch, epoch);
    }

    #[test]
    fn gc_scope_permits_only_below_read_epoch() {
        let e1 = Epoch::initial();
        let e2 = e1.next().expect("epoch 2");
        let e3 = e2.next().expect("epoch 3");
        let scope = GcScope::at_read(e3);

        assert!(scope.permits(e1));
        assert!(scope.permits(e2));
        assert!(!scope.permits(e3));
    }

    #[test]
    fn debris_sweep_removes_unreferenced_debris() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let root = tempfile::tempdir().expect("temp session root");
            let session_root = root.path();

            let e1 = Epoch::initial();
            let e2 = e1.next().expect("epoch 2");
            let e3 = e2.next().expect("epoch 3 (own epoch)");

            let referenced_e1 = ArtifactPath::parse("e1/keep.txt").expect("path parses");
            let referenced_e2 = ArtifactPath::parse("e2/nested/keep.txt").expect("path parses");

            let referenced_e1_file =
                session_root.join(<ArtifactPath as AsRef<Path>>::as_ref(&referenced_e1));
            let referenced_e2_file =
                session_root.join(<ArtifactPath as AsRef<Path>>::as_ref(&referenced_e2));
            let debris_e1_file = epoch_dir(session_root, e1).join("debris.tmp");
            let debris_e2_file = epoch_dir(session_root, e2)
                .join("nested")
                .join("debris.tmp");
            let own_epoch_file = epoch_dir(session_root, e3).join("in-progress.txt");

            for file in [
                &referenced_e1_file,
                &debris_e1_file,
                &referenced_e2_file,
                &debris_e2_file,
                &own_epoch_file,
            ] {
                let parent = file.parent().expect("file has a parent dir");
                tokio::fs::create_dir_all(parent)
                    .await
                    .expect("create parent dir");
                tokio::fs::write(file, b"contents")
                    .await
                    .expect("write file");
            }

            let mut manifest = Manifest::empty();
            manifest
                .declare(referenced_e1.clone(), sample_entry(e1, 8))
                .expect("declare e1 entry");
            manifest
                .declare(referenced_e2.clone(), sample_entry(e2, 8))
                .expect("declare e2 entry");

            let sweep = DebrisSweep::at_claim_time(&manifest, e3);
            let outcome = sweep.sweep(session_root).await.expect("sweep succeeds");

            assert_eq!(outcome.scanned_epochs, 3);
            assert_eq!(outcome.removed, 2);
            assert_eq!(outcome.kept, 3); // 2 referenced + the untouched own-epoch file

            assert!(tokio::fs::try_exists(&referenced_e1_file).await.unwrap());
            assert!(tokio::fs::try_exists(&referenced_e2_file).await.unwrap());
            assert!(tokio::fs::try_exists(&own_epoch_file).await.unwrap());
            assert!(!tokio::fs::try_exists(&debris_e1_file).await.unwrap());
            assert!(!tokio::fs::try_exists(&debris_e2_file).await.unwrap());

            // Idempotent: sweeping already-cleaned debris is a no-op, not an error.
            let outcome2 = sweep
                .sweep(session_root)
                .await
                .expect("second sweep succeeds");
            assert_eq!(outcome2.removed, 0);
            assert!(tokio::fs::try_exists(&referenced_e1_file).await.unwrap());
            assert!(tokio::fs::try_exists(&referenced_e2_file).await.unwrap());
            assert!(tokio::fs::try_exists(&own_epoch_file).await.unwrap());
        });
    }
}

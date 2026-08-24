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

use crate::epoch::Epoch;
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
    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    pub(crate) async fn sweep(&self, session_root: &Path) -> Result<SweepOutcome, std::io::Error> {
        todo!(
            "fill: walk e-dirs, scope.permits + keep-set predicate, idempotent unlink; aura #421 follow-up"
        )
    }
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

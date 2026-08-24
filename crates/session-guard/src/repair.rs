//! The repair lane: Archil-side cures for the two failure classes the
//! probes measured.
//!
//! - [`RepairLane::refresh_dir`] — read-side staleness. A
//!   manifest-referenced file that is still `ENOENT` past the
//!   propagation window is cured with `archil invalidate-cache <dir>`
//!   (vendor, 2026-08-21: forces a directory up-to-date). Unmeasured on
//!   the rig — latency and scope are open vendor questions — so it is
//!   the *first* escalation tier, and its failure is benign (falls
//!   through to `force_cure`).
//! - [`RepairLane::force_cure`] — the writability wedge (H4: EROFS on a
//!   sibling mkdir under a crashed client's delegation) and the final
//!   read-side escalation: `checkout -f` + immediate checkin, measured
//!   518 ms (H4b).
//!
//! Two implementations are planned — the archil CLI and an S3-API
//! variant; which ships by default waits on whether the CLI exists
//! inside CSI-mounted pods (open vendor question). The trait is
//! crate-internal; the aura seam and the PG adapter consume it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::RepairLaneKind;

/// Why a repair action failed. Diagnostic-only payload; callers act on
/// the *kind* of cure that failed, not the message.
#[derive(Debug, thiserror::Error)]
#[error("repair action failed: {0}")]
pub(crate) struct RepairError(String);

/// The two Archil cures, one trait so the CLI/S3-API choice is a config
/// decision, not a code path.
#[async_trait]
pub(crate) trait RepairLane: Send + Sync {
    /// Force a directory's cache up-to-date (read-side staleness cure).
    async fn refresh_dir(&self, dir: &Path) -> Result<(), RepairError>;

    /// Force-take and immediately release the delegation
    /// (`checkout -f` + checkin): the writability-wedge cure and final
    /// escalation.
    async fn force_cure(&self, dir: &Path) -> Result<(), RepairError>;
}

/// Repair via the `archil` binary.
#[derive(Debug)]
pub(crate) struct CliRepairLane {
    binary: PathBuf,
}

impl CliRepairLane {
    /// A lane driving the given archil binary.
    pub(crate) fn new(binary: PathBuf) -> Self {
        Self { binary }
    }
}

#[async_trait]
impl RepairLane for CliRepairLane {
    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    async fn refresh_dir(&self, dir: &Path) -> Result<(), RepairError> {
        todo!("fill: archil invalidate-cache <dir>; aura #421 follow-up")
    }

    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    async fn force_cure(&self, dir: &Path) -> Result<(), RepairError> {
        todo!("fill: archil checkout -f --yes <dir> && archil checkin <dir>; aura #421 follow-up")
    }
}

/// Repair via Archil's S3-compatible API (the CSI-no-CLI fallback).
#[derive(Debug)]
pub(crate) struct S3ApiRepairLane;

#[async_trait]
impl RepairLane for S3ApiRepairLane {
    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    async fn refresh_dir(&self, dir: &Path) -> Result<(), RepairError> {
        todo!("fill: S3-API refresh; aura #421 follow-up")
    }

    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    async fn force_cure(&self, dir: &Path) -> Result<(), RepairError> {
        todo!("fill: S3-API delegation force-take + release; aura #421 follow-up")
    }
}

/// Build the deployment's repair lane from config. `Auto` prefers the
/// CLI when the binary is discoverable, else the S3-API lane.
#[expect(
    unused_variables,
    reason = "todo!() body; filled by aura #421 follow-up"
)]
pub(crate) fn build_repair_lane(kind: RepairLaneKind) -> Arc<dyn RepairLane> {
    todo!("fill: kind dispatch + CLI discovery for Auto; aura #421 follow-up")
}

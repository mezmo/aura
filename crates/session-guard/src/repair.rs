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
//! The trait is crate-internal; the aura seam and the PG adapter
//! consume it.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::process::Command;

/// The archil CLI's bare command name, meant to be resolved via `$PATH`
/// wherever it is spawned or looked up.
const ARCHIL_BINARY: &str = "archil";

/// Why a repair action failed. Diagnostic-only payload; callers act on
/// the *kind* of cure that failed, not the message.
#[derive(Debug, thiserror::Error)]
#[error("repair action failed: {0}")]
pub(crate) struct RepairError(String);

/// The two Archil cures.
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
    async fn refresh_dir(&self, dir: &Path) -> Result<(), RepairError> {
        run_archil(
            &self.binary,
            [OsStr::new("invalidate-cache"), dir.as_os_str()],
        )
        .await
    }

    async fn force_cure(&self, dir: &Path) -> Result<(), RepairError> {
        run_archil(
            &self.binary,
            [
                OsStr::new("checkout"),
                OsStr::new("-f"),
                OsStr::new("--yes"),
                dir.as_os_str(),
            ],
        )
        .await?;
        run_archil(&self.binary, [OsStr::new("checkin"), dir.as_os_str()]).await
    }
}

/// Run one archil subcommand to completion. A non-zero exit and an exec
/// failure (binary missing, not executable) both map to [`RepairError`]
/// — the two lanes' callers (`refresh_dir`'s benign fallthrough,
/// `force_cure`'s hard failure) decide what a `RepairError` means, not
/// this function.
async fn run_archil(
    binary: &Path,
    args: impl IntoIterator<Item = &OsStr>,
) -> Result<(), RepairError> {
    let output = Command::new(binary)
        .args(args)
        .output()
        .await
        .map_err(|err| RepairError(format!("exec {} failed: {err}", binary.display())))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(RepairError(format!(
            "{} exited {}: {}",
            binary.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim(),
        )))
    }
}

/// Build the deployment's repair lane. The archil CLI is the only
/// cure path; a missing binary surfaces as a RepairError at cure time,
/// never here.
pub(crate) fn build_repair_lane() -> Arc<dyn RepairLane> {
    Arc::new(CliRepairLane::new(PathBuf::from(ARCHIL_BINARY)))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// Write a fake `archil` that appends its argv (one invocation per
    /// line) to `argv.log` beside itself, then exits with `exit_code`.
    fn write_fake_archil(dir: &Path, exit_code: i32) -> PathBuf {
        let script = dir.join("archil");
        fs::write(
            &script,
            format!("#!/bin/sh\necho \"$@\" >> \"$(dirname \"$0\")/argv.log\"\nexit {exit_code}\n"),
        )
        .expect("write fake archil script");
        let mut perms = fs::metadata(&script)
            .expect("script metadata")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).expect("chmod script");
        script
    }

    fn read_argv_log(dir: &Path) -> String {
        fs::read_to_string(dir.join("argv.log")).expect("argv.log written")
    }

    #[test]
    fn cli_repair_lane_refresh_dir() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let tmp = tempfile::tempdir().expect("tempdir");
            let binary = write_fake_archil(tmp.path(), 0);
            let lane = CliRepairLane::new(binary);
            let target = tmp.path().join("e1");

            lane.refresh_dir(&target)
                .await
                .expect("refresh_dir succeeds");

            assert_eq!(
                read_argv_log(tmp.path()).trim(),
                format!("invalidate-cache {}", target.display())
            );
        });
    }

    #[test]
    fn cli_repair_lane_refresh_dir_maps_nonzero_exit_to_repair_error() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let tmp = tempfile::tempdir().expect("tempdir");
            let binary = write_fake_archil(tmp.path(), 1);
            let lane = CliRepairLane::new(binary);
            let target = tmp.path().join("e1");

            let err = lane
                .refresh_dir(&target)
                .await
                .expect_err("non-zero exit maps to RepairError");
            assert!(!err.to_string().is_empty());
        });
    }

    #[test]
    fn cli_repair_lane_force_cure() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let tmp = tempfile::tempdir().expect("tempdir");
            let binary = write_fake_archil(tmp.path(), 0);
            let lane = CliRepairLane::new(binary);
            let target = tmp.path().join("e1");

            lane.force_cure(&target).await.expect("force_cure succeeds");

            let log = read_argv_log(tmp.path());
            let mut lines = log.lines();
            assert_eq!(
                lines.next().expect("checkout line"),
                format!("checkout -f --yes {}", target.display())
            );
            assert_eq!(
                lines.next().expect("checkin line"),
                format!("checkin {}", target.display())
            );
        });
    }

    #[test]
    fn cli_repair_lane_force_cure_maps_nonzero_exit_to_repair_error() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let tmp = tempfile::tempdir().expect("tempdir");
            let binary = write_fake_archil(tmp.path(), 1);
            let lane = CliRepairLane::new(binary);
            let target = tmp.path().join("e1");

            let err = lane
                .force_cure(&target)
                .await
                .expect_err("non-zero exit on the first step maps to RepairError");
            assert!(!err.to_string().is_empty());
        });
    }

    #[test]
    fn build_repair_lane_cli_dispatches_to_the_filled_lane() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let lane = build_repair_lane();
            let handle =
                tokio::spawn(async move { lane.refresh_dir(Path::new("/nonexistent")).await });
            // Whether or not a real `archil` sits on this machine's PATH,
            // the CLI lane's filled body runs to completion (Ok or a
            // mapped RepairError) rather than panicking.
            assert!(
                handle.await.is_ok(),
                "build_repair_lane must reach the filled CliRepairLane, not a todo!() stub"
            );
        });
    }
}

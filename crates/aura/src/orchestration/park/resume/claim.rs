//! The endpoint-owned resume claim table and the run's checkpoint document
//! paths.
//!
//! The claim table is the endpoint's per-run handle, distinct from
//! [`super::super::continuation::ResumingDocumentHandle`], which stays the
//! park-module's append-and-publish surface: the table tracks which endpoint
//! evaluation holds a run, the handle mutates the resuming document.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use crate::config::SessionId;
use crate::orchestration::persistence::is_safe_path_component;
use crate::orchestration::types::RunId;

use super::super::commit::parked_document_dir;
use super::super::document::{PARKED_DOCUMENT_SUFFIX, RESUMING_DOCUMENT_SUFFIX};
use super::evaluate::Diagnostic;

/// Why a raw path segment failed validation. Every variant is
/// diagnostic-only: no caller branches on the reason.
#[derive(Debug, Clone)]
pub enum MalformedId {
    /// The run id did not parse as a UUID.
    NotAUuid(Diagnostic),
    /// The segment was empty, carried a path separator, or carried a parent
    /// reference.
    UnsafePathComponent(Diagnostic),
}

impl std::fmt::Display for MalformedId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAUuid(diagnostic) => write!(f, "{diagnostic}"),
            Self::UnsafePathComponent(diagnostic) => write!(f, "{diagnostic}"),
        }
    }
}

/// A path-validated session id: safe as a single filesystem path component.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResumeSessionId(SessionId);

impl ResumeSessionId {
    /// Parse a raw path segment, rejecting anything unsafe as a path
    /// component before any filesystem access.
    pub fn parse(raw: &str) -> Result<Self, MalformedId> {
        if is_safe_path_component(raw) {
            Ok(Self(SessionId::new(raw)))
        } else {
            Err(MalformedId::UnsafePathComponent(Diagnostic::new(format!(
                "session id {raw:?} is not a safe path component"
            ))))
        }
    }
}

impl AsRef<str> for ResumeSessionId {
    fn as_ref(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Display for ResumeSessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.as_str())
    }
}

/// A path-validated run id: parses as a UUID and is safe as a single
/// filesystem path component.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResumeRunId(RunId);

impl ResumeRunId {
    /// Parse a raw path segment as a UUID and reject anything unsafe as a
    /// path component, before any filesystem access.
    pub fn parse(raw: &str) -> Result<Self, MalformedId> {
        let run = RunId::from_str(raw)
            .map_err(|e| MalformedId::NotAUuid(Diagnostic::new(e.to_string())))?;
        if !is_safe_path_component(raw) {
            return Err(MalformedId::UnsafePathComponent(Diagnostic::new(format!(
                "run id {raw:?} is not a safe path component"
            ))));
        }
        Ok(Self(run))
    }

    /// The inner run id.
    #[must_use]
    pub fn run_id(&self) -> RunId {
        self.0
    }
}

impl std::fmt::Display for ResumeRunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Both validated path segments of a resume request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedResumePath {
    pub session: ResumeSessionId,
    pub run: ResumeRunId,
}

impl ValidatedResumePath {
    /// Parse both segments; the first rejection wins.
    pub fn parse(session: &str, run: &str) -> Result<Self, MalformedId> {
        let session = ResumeSessionId::parse(session)?;
        let run = ResumeRunId::parse(run)?;
        Ok(Self { session, run })
    }
}

/// A run's two checkpoint filenames under its session's parked directory.
#[derive(Debug, Clone)]
pub struct ResumeDocuments {
    parked: PathBuf,
    resuming: PathBuf,
}

impl ResumeDocuments {
    /// Derive the two filenames from a validated path and the memory root.
    #[must_use]
    pub fn for_path(path: &ValidatedResumePath, memory_dir: &str) -> Self {
        let dir = parked_document_dir(memory_dir, Some(path.session.as_ref()));
        Self {
            parked: dir.join(format!("{}{PARKED_DOCUMENT_SUFFIX}", path.run)),
            resuming: dir.join(format!("{}{RESUMING_DOCUMENT_SUFFIX}", path.run)),
        }
    }

    /// The published checkpoint filename.
    pub(crate) fn parked(&self) -> &Path {
        &self.parked
    }

    /// The in-progress checkpoint filename.
    pub(crate) fn resuming(&self) -> &Path {
        &self.resuming
    }
}

/// Why claiming a run for a resume segment failed.
#[derive(Debug, Clone)]
pub(crate) enum ClaimResumeFault {
    /// A live claim already holds the run.
    Live,
    /// The atomic rename failed; the claim was not taken.
    Io(Diagnostic),
}

/// Process-local registry of live resume claims: at most one resume per run
/// inside this process.
#[derive(Debug, Default)]
pub struct ResumeClaimTable {
    live: Arc<Mutex<HashSet<RunId>>>,
}

impl ResumeClaimTable {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a live claim holds the run.
    #[must_use]
    pub(crate) fn is_live(&self, run: &ResumeRunId) -> bool {
        self.live
            .lock()
            .expect("resume claim lock")
            .contains(&run.run_id())
    }

    /// Rename the run's resuming document back to its parked name while
    /// holding the claim lock, so a concurrent evaluation cannot observe the
    /// half-renamed pair.
    #[expect(unused_variables, reason = "todo!() body; filled by P45")]
    pub(crate) async fn rename_back_to_parked(
        &self,
        docs: &ResumeDocuments,
    ) -> Result<(), Diagnostic> {
        todo!()
    }

    /// Insert the claim and rename the parked document to its resuming name
    /// as one step under the claim lock: either both happen or neither does.
    #[expect(unused_variables, reason = "todo!() body; filled by P45")]
    pub(crate) async fn claim_and_resume(
        &self,
        docs: &ResumeDocuments,
    ) -> Result<ResumeLease, ClaimResumeFault> {
        todo!()
    }
}

/// A held resume claim for one run.
#[derive(Debug)]
pub struct ResumeLease {
    live: Arc<Mutex<HashSet<RunId>>>,
    run: ResumeRunId,
}

impl Drop for ResumeLease {
    /// Release the run; a later evaluation may claim it.
    fn drop(&mut self) {
        self.live
            .lock()
            .expect("resume claim lock")
            .remove(&self.run.run_id());
    }
}

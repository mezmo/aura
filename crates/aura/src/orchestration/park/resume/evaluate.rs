//! The ordered pre-continuation evaluation and the segment seam.
//!
//! The evaluation is a pipeline of stages, one per row of the card's table:
//! each stage returns only the refusal rows its own step can produce, so a
//! stage structurally cannot emit another row's verdict. [`ResumeRefusal`]
//! is the sum of those rows, with its variants declared in evaluation order.
//! The first matching row wins; the all-decided row is not a refusal but the
//! [`ResumeGrant`] the segment consumes.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::config::AgentRuntimeConfig;
use crate::hitl::{DecisionId, PendingApprovals};

use super::super::RecordedDecisions;
use super::super::document::ParkedRun;
use super::claim::{
    ResumeClaimTable, ResumeDocuments, ResumeLease, ResumeRunId, ResumeSessionId,
    ValidatedResumePath,
};

/// Free-form human text carried for diagnosis; no caller branches on it.
#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub struct Diagnostic(String);

impl Diagnostic {
    pub(crate) fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }
}

impl AsRef<str> for Diagnostic {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Hex sha256 over one identity header's value; construction is by hashing
/// only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct IdentityHash(String);

impl IdentityHash {
    /// Hash one presented identity-header value.
    #[must_use]
    pub fn hash_value(value: &str) -> Self {
        Self(hex::encode(Sha256::digest(value.as_bytes())))
    }

    /// Parse a stored hash, rejecting anything that is not 64 hex
    /// characters.
    pub(crate) fn from_stored(raw: &str) -> Result<Self, Diagnostic> {
        if raw.len() == 64 && raw.bytes().all(|b| b.is_ascii_hexdigit()) {
            Ok(Self(raw.to_owned()))
        } else {
            Err(Diagnostic::new(
                "stored identity hash is not 64 hex characters",
            ))
        }
    }
}

/// The identity-binding side of one resume request.
#[derive(Debug, Clone)]
pub enum IdentityBindingState {
    /// Identity binding is not configured.
    Unbound,
    /// Binding is configured and the request presented no identity header.
    BoundMissingHeader,
    /// Binding is configured, carrying the presented header's hash.
    Bound(IdentityHash),
}

impl IdentityBindingState {
    /// Resolve the configured binding against the presented header value.
    #[must_use]
    pub fn resolve(bind_identity: bool, presented: Option<&str>) -> Self {
        match (bind_identity, presented) {
            (false, _) => Self::Unbound,
            (true, None) => Self::BoundMissingHeader,
            (true, Some(value)) => Self::Bound(IdentityHash::hash_value(value)),
        }
    }
}

/// The gated tool's name on one blocking entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ParkedToolName(String);

impl ParkedToolName {
    pub(crate) fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }
}

impl AsRef<str> for ParkedToolName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// One outstanding parked call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlockingEntry {
    pub decision_id: DecisionId,
    pub tool: ParkedToolName,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

/// The not-ready codes, declared in evaluation order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictCode {
    Running,
    Interrupted,
    ConfigChanged,
    Mismatch,
    Expired,
    Parked,
}

/// The one not-ready body shape: `{code, detail, blocking}`.
#[derive(Debug, Clone, Serialize)]
pub struct ResumeConflictRow {
    code: ConflictCode,
    detail: Diagnostic,
    blocking: Vec<BlockingEntry>,
}

impl ResumeConflictRow {
    /// The conflict code.
    #[must_use]
    pub fn code(&self) -> ConflictCode {
        self.code
    }

    /// The outstanding calls, empty on rows with no pending set.
    #[must_use]
    pub fn blocking(&self) -> &[BlockingEntry] {
        &self.blocking
    }

    pub(crate) fn running() -> Self {
        Self {
            code: ConflictCode::Running,
            detail: Diagnostic::new("another resume holds this run"),
            blocking: Vec::new(),
        }
    }

    pub(crate) fn interrupted() -> Self {
        Self {
            code: ConflictCode::Interrupted,
            detail: Diagnostic::new(
                "a previous resume died mid-segment; the executed list is non-empty",
            ),
            blocking: Vec::new(),
        }
    }

    pub(crate) fn config_changed() -> Self {
        Self {
            code: ConflictCode::ConfigChanged,
            detail: Diagnostic::new("configuration changed since the run parked"),
            blocking: Vec::new(),
        }
    }

    pub(crate) fn mismatch(detail: Diagnostic) -> Self {
        Self {
            code: ConflictCode::Mismatch,
            detail,
            blocking: Vec::new(),
        }
    }

    pub(crate) fn expired(blocking: Vec<BlockingEntry>) -> Self {
        Self {
            code: ConflictCode::Expired,
            detail: Diagnostic::new(
                "the decision window closed before every pending call was decided",
            ),
            blocking,
        }
    }

    pub(crate) fn parked(blocking: Vec<BlockingEntry>) -> Self {
        Self {
            code: ConflictCode::Parked,
            detail: Diagnostic::new("calls still await a decision"),
            blocking,
        }
    }
}

/// Why the evaluation refused the resume. Variants are declared in
/// evaluation order; [`ResumeRefusal::DocumentAbsent`] and
/// [`ResumeRefusal::IdentityMismatch`] are the detail-less rows, the rest
/// render as the one conflict shape.
#[derive(Debug, Clone)]
pub enum ResumeRefusal {
    /// No checkpoint document under either name.
    DocumentAbsent,
    /// Identity binding is configured and the presented hash differs.
    IdentityMismatch,
    /// A not-ready conflict row.
    Conflict(ResumeConflictRow),
    /// A store, document, or claim fault refused the run before any verdict.
    Fault(Diagnostic),
}

/// Why locating a checkpoint failed.
enum LocateFault {
    Absent,
    Fault(Diagnostic),
}

/// Why the identity check failed.
enum IdentityFault {
    Mismatch,
    Fault(Diagnostic),
}

/// Why the claim check failed.
enum ClaimFault {
    Live,
    Fault(Diagnostic),
}

/// Why admitting the checkpoint failed.
enum AdmitFault {
    Interrupted,
    Fault(Diagnostic),
}

/// Why the fingerprint check failed.
enum FingerprintFault {
    Drift,
    Fault(Diagnostic),
}

/// Why the recorded-decisions consult failed.
enum ConsultFault {
    Mismatch(Diagnostic),
    Expired(Vec<BlockingEntry>),
    Parked(Vec<BlockingEntry>),
    Fault(Diagnostic),
}

impl From<LocateFault> for ResumeRefusal {
    fn from(fault: LocateFault) -> Self {
        match fault {
            LocateFault::Absent => Self::DocumentAbsent,
            LocateFault::Fault(diagnostic) => Self::Fault(diagnostic),
        }
    }
}

impl From<IdentityFault> for ResumeRefusal {
    fn from(fault: IdentityFault) -> Self {
        match fault {
            IdentityFault::Mismatch => Self::IdentityMismatch,
            IdentityFault::Fault(diagnostic) => Self::Fault(diagnostic),
        }
    }
}

impl From<ClaimFault> for ResumeRefusal {
    fn from(fault: ClaimFault) -> Self {
        match fault {
            ClaimFault::Live => Self::Conflict(ResumeConflictRow::running()),
            ClaimFault::Fault(diagnostic) => Self::Fault(diagnostic),
        }
    }
}

impl From<AdmitFault> for ResumeRefusal {
    fn from(fault: AdmitFault) -> Self {
        match fault {
            AdmitFault::Interrupted => Self::Conflict(ResumeConflictRow::interrupted()),
            AdmitFault::Fault(diagnostic) => Self::Fault(diagnostic),
        }
    }
}

impl From<FingerprintFault> for ResumeRefusal {
    fn from(fault: FingerprintFault) -> Self {
        match fault {
            FingerprintFault::Drift => Self::Conflict(ResumeConflictRow::config_changed()),
            FingerprintFault::Fault(diagnostic) => Self::Fault(diagnostic),
        }
    }
}

impl From<ConsultFault> for ResumeRefusal {
    fn from(fault: ConsultFault) -> Self {
        match fault {
            ConsultFault::Mismatch(diagnostic) => {
                Self::Conflict(ResumeConflictRow::mismatch(diagnostic))
            }
            ConsultFault::Expired(blocking) => Self::Conflict(ResumeConflictRow::expired(blocking)),
            ConsultFault::Parked(blocking) => Self::Conflict(ResumeConflictRow::parked(blocking)),
            ConsultFault::Fault(diagnostic) => Self::Fault(diagnostic),
        }
    }
}

/// A checkpoint as located on disk, under either its parked or its resuming
/// name.
enum LocatedCheckpoint {
    Parked { document: ParkedRun },
    Resuming { document: ParkedRun },
}

/// Everything one resume evaluation reads.
pub struct ResumeEvaluation<'a> {
    pub path: ValidatedResumePath,
    pub documents: ResumeDocuments,
    pub config: &'a AgentRuntimeConfig,
    pub store: &'a PendingApprovals,
    pub claims: &'a ResumeClaimTable,
    pub identity: IdentityBindingState,
    pub now: chrono::DateTime<chrono::Utc>,
}

/// Authorization to execute one segment for a granted run.
#[derive(Debug)]
pub struct ResumeGrant {
    lease: ResumeLease,
    documents: ResumeDocuments,
    document: ParkedRun,
    recorded: Arc<RecordedDecisions>,
    session: ResumeSessionId,
    run: ResumeRunId,
}

impl ResumeGrant {
    /// The validated session id of the granted run.
    #[must_use]
    pub fn session_id(&self) -> &ResumeSessionId {
        &self.session
    }

    /// The validated run id of the granted run.
    #[must_use]
    pub fn run_id(&self) -> &ResumeRunId {
        &self.run
    }
}

/// Locate and load whichever checkpoint name exists for the run.
#[expect(unused_variables, reason = "todo!() body; filled by P45")]
async fn locate_checkpoint(docs: &ResumeDocuments) -> Result<LocatedCheckpoint, LocateFault> {
    todo!()
}

/// Compare the checkpoint's stored identity hash against the request's.
#[expect(unused_variables, reason = "todo!() body; filled by P45")]
fn check_identity(
    stored: Option<IdentityHash>,
    presented: &IdentityBindingState,
) -> Result<(), IdentityFault> {
    todo!()
}

/// Reject a run another evaluation already holds.
#[expect(unused_variables, reason = "todo!() body; filled by P45")]
fn check_claim(claims: &ResumeClaimTable, run: &ResumeRunId) -> Result<(), ClaimFault> {
    todo!()
}

/// Apply the interrupted and rename-back rows, yielding the parked document
/// the content rows evaluate.
#[expect(unused_variables, reason = "todo!() body; filled by P45")]
async fn admit(
    located: LocatedCheckpoint,
    docs: &ResumeDocuments,
    claims: &ResumeClaimTable,
) -> Result<ParkedRun, AdmitFault> {
    todo!()
}

/// Compare the checkpoint's fingerprint against the rebuilt configuration.
#[expect(unused_variables, reason = "todo!() body; filled by P45")]
fn check_fingerprint(
    document: &ParkedRun,
    config: &AgentRuntimeConfig,
) -> Result<(), FingerprintFault> {
    todo!()
}

/// Consult the store's recorded decisions for every pending call, applying
/// the mismatch, expired, and parked rows; an expired ticket swept by a
/// remote TTL reads as expired, never as a mismatch.
#[expect(unused_variables, reason = "todo!() body; filled by P45")]
async fn consult_decisions(
    document: &ParkedRun,
    store: &PendingApprovals,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Arc<RecordedDecisions>, ConsultFault> {
    todo!()
}

/// Project the run's outstanding parked calls onto blocking entries.
#[expect(unused_variables, reason = "todo!() body; filled by P45")]
async fn project_blocking(
    document: &ParkedRun,
    store: &PendingApprovals,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<BlockingEntry>, Diagnostic> {
    todo!()
}

/// Take the claim and rename the parked document to its resuming name as
/// one step, then assemble the grant.
#[expect(unused_variables, reason = "todo!() body; filled by P45")]
async fn authorize(
    docs: &ResumeDocuments,
    claims: &ResumeClaimTable,
    evaluation_path: &ValidatedResumePath,
    document: ParkedRun,
    recorded: Arc<RecordedDecisions>,
) -> Result<ResumeGrant, Diagnostic> {
    todo!()
}

/// Evaluate a run against the ordered table and either authorize the
/// claim-and-segment or refuse with the first matching row.
#[expect(unused_variables, reason = "todo!() body; filled by P45")]
pub async fn evaluate_resume(
    evaluation: ResumeEvaluation<'_>,
) -> Result<ResumeGrant, ResumeRefusal> {
    todo!()
}

/// The turns of one executed segment, in order; never empty.
#[derive(Debug, Clone)]
pub struct SegmentTurns(Vec<rig::completion::Message>);

impl SegmentTurns {
    /// Take the segment's turns, rejecting an empty set.
    pub fn try_new(turns: Vec<rig::completion::Message>) -> Result<Self, EmptySegment> {
        if turns.is_empty() {
            Err(EmptySegment)
        } else {
            Ok(Self(turns))
        }
    }

    /// The turns in segment order.
    #[must_use]
    pub fn as_slice(&self) -> &[rig::completion::Message] {
        &self.0
    }
}

/// A segment carried no turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmptySegment;

/// One executed resume segment's terminal data.
#[derive(Debug, Clone)]
pub enum SegmentResult {
    Completed {
        turns: SegmentTurns,
    },
    Parked {
        turns: SegmentTurns,
        blocking: Vec<BlockingEntry>,
    },
}

/// Why a segment ended without terminal data.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SegmentError {
    /// The continuation failed mid-segment.
    Continuation(Diagnostic),
}

/// Execute one segment for a granted run: the decided approvals' next agent
/// turns, until the run completes or a new approval-required call parks. The
/// segment is atomic data — no streaming to the client mid-segment.
#[expect(unused_variables, reason = "todo!() body; filled by P45")]
pub async fn run_segment(
    grant: ResumeGrant,
    config: &AgentRuntimeConfig,
    headers: &HashMap<String, String>,
) -> Result<SegmentResult, SegmentError> {
    todo!()
}

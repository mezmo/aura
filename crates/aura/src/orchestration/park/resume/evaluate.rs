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
use crate::request_cancellation::RequestId;

use super::super::RecordedDecisions;
use super::super::commit::{cancel_run_approvals, config_fingerprint};
use super::super::continuation::{RehydrateError, load_recorded_decisions};
use super::super::document::{ParkedRun, load_parked_run};
use super::claim::{
    ClaimResumeFault, ResumeClaimTable, ResumeDocuments, ResumeLease, ResumeRunId, ResumeSessionId,
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
/// only. Comparison-only: the hash never reaches the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityHash(String);

impl IdentityHash {
    /// Hash one presented identity-header value.
    #[must_use]
    pub fn hash_value(value: &str) -> Self {
        Self(hex::encode(Sha256::digest(value.as_bytes())))
    }

    /// Take the stored form: the 64-char lowercase hex digest.
    pub(crate) fn into_inner(self) -> String {
        self.0
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

/// The identity-binding side of one resume request, resolved inside the
/// evaluation from the configured bind flag and the presented header. The
/// type is private to this module, so a resolved state contradicting its
/// config cannot be constructed, only derived.
#[derive(Debug, Clone)]
enum IdentityBindingState {
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
    fn resolve(bind_identity: bool, presented: Option<&str>) -> Self {
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

/// The blocking entries of a body that must carry at least one outstanding
/// call. Serializes as the plain JSON array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct NonEmptyBlocking(Vec<BlockingEntry>);

/// A mandatory blocking set was built from no entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmptyBlocking;

impl NonEmptyBlocking {
    /// Take the blocking entries, rejecting an empty set.
    pub fn try_new(entries: Vec<BlockingEntry>) -> Result<Self, EmptyBlocking> {
        if entries.is_empty() {
            Err(EmptyBlocking)
        } else {
            Ok(Self(entries))
        }
    }

    /// The entries in row order.
    #[must_use]
    pub fn as_slice(&self) -> &[BlockingEntry] {
        &self.0
    }
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

    pub(crate) fn expired(blocking: NonEmptyBlocking) -> Self {
        Self {
            code: ConflictCode::Expired,
            detail: Diagnostic::new(
                "the decision window closed before every pending call was decided",
            ),
            blocking: blocking.0,
        }
    }

    pub(crate) fn parked(blocking: NonEmptyBlocking) -> Self {
        Self {
            code: ConflictCode::Parked,
            detail: Diagnostic::new("calls still await a decision"),
            blocking: blocking.0,
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
    Expired(NonEmptyBlocking),
    Parked(NonEmptyBlocking),
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

impl From<ClaimResumeFault> for ResumeRefusal {
    fn from(fault: ClaimResumeFault) -> Self {
        match fault {
            // A claim lost to a concurrent evaluation is the running row,
            // never the fault sink: the spec fixes 409 for a live claim.
            ClaimResumeFault::Live => Self::Conflict(ResumeConflictRow::running()),
            ClaimResumeFault::Io(diagnostic) => Self::Fault(diagnostic),
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
    /// The memory root the run's two checkpoint names derive from.
    pub memory_dir: &'a str,
    pub config: &'a AgentRuntimeConfig,
    pub store: &'a PendingApprovals,
    pub claims: &'a ResumeClaimTable,
    /// The `[hitl.park]` bind flag, resolved by the caller from the parsed
    /// config: the runtime `HitlRuntime` does not carry it.
    pub bind_identity: bool,
    /// The presented identity header's raw value, before any hashing.
    pub presented_identity: Option<&'a str>,
    /// The request id the run's sweep events publish under.
    pub request_id: RequestId,
    pub now: chrono::DateTime<chrono::Utc>,
}

/// Authorization to execute one segment for a granted run.
#[derive(Debug)]
pub struct ResumeGrant {
    lease: ResumeLease,
    documents: ResumeDocuments,
    document: ParkedRun,
    recorded: Arc<RecordedDecisions>,
    /// The decided approvals the consult consumed from the store.
    consumed: Vec<DecisionId>,
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

    /// The checkpoint document the grant was evaluated from.
    #[must_use]
    pub(crate) fn checkpoint(&self) -> &ParkedRun {
        &self.document
    }

    /// The run's recorded decisions behind the `Arc` the gate holds.
    #[must_use]
    pub(crate) fn recorded_decisions(&self) -> &Arc<RecordedDecisions> {
        &self.recorded
    }

    /// The decided approvals the consult consumed from the store.
    #[must_use]
    pub(crate) fn consumed_decisions(&self) -> &[DecisionId] {
        &self.consumed
    }

    /// The run's two checkpoint paths under its session's parked directory.
    #[must_use]
    pub(crate) fn documents(&self) -> &ResumeDocuments {
        &self.documents
    }
}

/// Locate and load whichever checkpoint name exists for the run: the parked
/// name first, the resuming name only when the parked name is absent. A
/// present-but-unreadable document faults instead of reading as absent, so a
/// corrupt checkpoint can never answer the not-found row.
async fn locate_checkpoint(docs: &ResumeDocuments) -> Result<LocatedCheckpoint, LocateFault> {
    match load_parked_run(docs.parked()).await {
        Ok(document) => return Ok(LocatedCheckpoint::Parked { document }),
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(LocateFault::Fault(Diagnostic::new(format!(
                "the parked checkpoint {} could not be read: {e}",
                docs.parked().display()
            ))));
        }
        Err(_) => {}
    }
    match load_parked_run(docs.resuming()).await {
        Ok(document) => Ok(LocatedCheckpoint::Resuming { document }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(LocateFault::Absent),
        Err(e) => Err(LocateFault::Fault(Diagnostic::new(format!(
            "the resuming checkpoint {} could not be read: {e}",
            docs.resuming().display()
        )))),
    }
}

/// Compare the checkpoint's stored identity hash against the presented
/// header, resolving the configured binding first. Binding off admits every
/// caller; binding on refuses the detail-less row for a missing header, a
/// stored hash that differs, and — failing closed — a document that carries
/// no stored hash at all.
fn check_identity(
    stored: Option<IdentityHash>,
    bind_identity: bool,
    presented_identity: Option<&str>,
) -> Result<(), IdentityFault> {
    match IdentityBindingState::resolve(bind_identity, presented_identity) {
        IdentityBindingState::Unbound => Ok(()),
        // Binding configured with no presented header fails closed: the
        // caller cannot be attributed, so the run answers the not-found row.
        IdentityBindingState::BoundMissingHeader => Err(IdentityFault::Mismatch),
        IdentityBindingState::Bound(presented) => {
            if stored.as_ref() == Some(&presented) {
                Ok(())
            } else {
                Err(IdentityFault::Mismatch)
            }
        }
    }
}

/// Reject a run another evaluation already holds; a race lost later, at the
/// claim itself, surfaces through `authorize` and maps to the same row.
fn check_claim(claims: &ResumeClaimTable, run: &ResumeRunId) -> Result<(), ClaimResumeFault> {
    if claims.is_live(run) {
        Err(ClaimResumeFault::Live)
    } else {
        Ok(())
    }
}

/// Apply the interrupted and rename-back rows, yielding the parked document
/// the content rows evaluate. Non-empty executed tombstones refuse as
/// interrupted under either name; an empty resuming document is renamed back
/// to its parked name under the claim lock before the content rows run.
async fn admit(
    located: LocatedCheckpoint,
    docs: &ResumeDocuments,
    claims: &ResumeClaimTable,
) -> Result<ParkedRun, AdmitFault> {
    match located {
        LocatedCheckpoint::Parked { document } => {
            if document.executed.is_empty() {
                Ok(document)
            } else {
                Err(AdmitFault::Interrupted)
            }
        }
        LocatedCheckpoint::Resuming { document } => {
            if !document.executed.is_empty() {
                // The dead resume's document stays exactly as found: the
                // interrupted row refuses, and no rename hides the evidence.
                return Err(AdmitFault::Interrupted);
            }
            claims
                .rename_back_to_parked(docs)
                .await
                .map_err(AdmitFault::Fault)?;
            Ok(document)
        }
    }
}

/// Compare the checkpoint's fingerprint against the rebuilt configuration.
/// The check runs before the consult, so a drifted config refuses with zero
/// tool invocations and no consumed-decision cleanup.
fn check_fingerprint(
    document: &ParkedRun,
    config: &AgentRuntimeConfig,
) -> Result<(), FingerprintFault> {
    if document.config_fingerprint == config_fingerprint(config) {
        Ok(())
    } else {
        Err(FingerprintFault::Drift)
    }
}

/// Consult the store's recorded decisions for every pending call, applying
/// the mismatch, expired, and parked rows; an expired ticket swept by a
/// remote TTL reads as expired, never as a mismatch. The rehydrate error's
/// raw payloads are wrapped in `Diagnostic` here, at the consult boundary:
/// the wrapped text is the mismatch row's detail, verbatim.
async fn consult_decisions(
    document: &ParkedRun,
    store: &PendingApprovals,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(Arc<RecordedDecisions>, Vec<DecisionId>), ConsultFault> {
    match load_recorded_decisions(store, document, now).await {
        Ok((recorded, consumed)) => Ok((recorded, consumed)),
        Err(RehydrateError::Mismatch(detail)) => {
            Err(ConsultFault::Mismatch(Diagnostic::new(detail)))
        }
        Err(RehydrateError::Parked { .. }) => {
            let blocking = project_blocking(document, store, now)
                .await
                .map_err(ConsultFault::Fault)?;
            Err(ConsultFault::Parked(blocking))
        }
        Err(RehydrateError::Expired) => {
            // The remote TTL swept the ticket: the expired row outranks the
            // mismatch row and carries the pre-sweep blocking list.
            let blocking = project_blocking(document, store, now)
                .await
                .map_err(ConsultFault::Fault)?;
            Err(ConsultFault::Expired(blocking))
        }
        Err(RehydrateError::Store(detail)) | Err(RehydrateError::Document(detail)) => {
            Err(ConsultFault::Fault(Diagnostic::new(detail)))
        }
        Err(err @ (RehydrateError::NotFound | RehydrateError::ConfigChanged)) => {
            // Structurally unreachable: the document was located and
            // fingerprint-checked before the consult. Refuse loudly rather
            // than invent a row.
            Err(ConsultFault::Fault(Diagnostic::new(format!(
                "the recorded-decisions consult reported an unreachable condition: {err}"
            ))))
        }
    }
}

/// Project the run's outstanding parked calls onto blocking entries. The
/// expired and parked rows are unreachable without at least one outstanding
/// call, so an empty projection is a fault, not an empty body. Outstanding
/// means undecided in the store: a call decided since the park commit is
/// settled and blocks nobody, while a ticket a remote TTL swept is still
/// outstanding — the expired row's pre-sweep list. Every entry carries the
/// document's window stamp, the bound the human decision was held to.
async fn project_blocking(
    document: &ParkedRun,
    store: &PendingApprovals,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<NonEmptyBlocking, Diagnostic> {
    let expires_at = chrono::DateTime::parse_from_rfc3339(&document.expires_at)
        .map_err(|e| Diagnostic::new(format!("bad expiry stamp on the parked document: {e}")))?
        .with_timezone(&chrono::Utc);
    let mut entries = Vec::new();
    for node in &document.plan.tasks {
        let crate::orchestration::types::TaskStatus::AwaitingApproval = node.status else {
            continue;
        };
        let Some(pending) = &node.pending else {
            continue;
        };
        for call in pending {
            if store.recorded_decision(&call.decision_id).await.is_none() {
                entries.push(BlockingEntry {
                    decision_id: call.decision_id,
                    tool: ParkedToolName::new(call.tool_name.as_str()),
                    expires_at,
                });
            }
        }
    }
    NonEmptyBlocking::try_new(entries).map_err(|EmptyBlocking| {
        Diagnostic::new(format!(
            "the blocking projection found no outstanding parked calls as of {now}"
        ))
    })
}

/// Take the claim and rename the parked document to its resuming name as
/// one step, then assemble the grant. A claim lost to a concurrent
/// evaluation between `check_claim` and here maps to the running row.
async fn authorize(
    docs: &ResumeDocuments,
    claims: &ResumeClaimTable,
    evaluation_path: &ValidatedResumePath,
    document: ParkedRun,
    recorded: Arc<RecordedDecisions>,
    consumed: Vec<DecisionId>,
) -> Result<ResumeGrant, ClaimResumeFault> {
    let lease = claims.claim_and_resume(docs).await?;
    Ok(ResumeGrant {
        lease,
        documents: docs.clone(),
        document,
        recorded,
        consumed,
        session: evaluation_path.session.clone(),
        run: evaluation_path.run.clone(),
    })
}

/// Evaluate a run against the ordered table and either authorize the
/// claim-and-segment or refuse with the first matching row. An expired
/// refusal tears the run down before rendering: the checkpoint is unlinked
/// and the run's undecided tickets are swept under the bundle's request id,
/// so the row the client sees matches the state left behind.
pub async fn evaluate_resume(
    evaluation: ResumeEvaluation<'_>,
) -> Result<ResumeGrant, ResumeRefusal> {
    let ResumeEvaluation {
        path,
        memory_dir,
        config,
        store,
        claims,
        bind_identity,
        presented_identity,
        request_id,
        now,
    } = evaluation;
    let docs = ResumeDocuments::for_path(&path, memory_dir);

    let located = locate_checkpoint(&docs).await?;

    // The stored hash is parsed before the comparison, so a malformed stamp
    // faults instead of silently mismatching.
    let stored = match &located {
        LocatedCheckpoint::Parked { document } | LocatedCheckpoint::Resuming { document } => {
            match document.identity_hash.as_deref() {
                Some(raw) => Some(IdentityHash::from_stored(raw).map_err(IdentityFault::Fault)?),
                None => None,
            }
        }
    };
    check_identity(stored, bind_identity, presented_identity)?;

    check_claim(claims, &path.run)?;

    let document = admit(located, &docs, claims).await?;

    check_fingerprint(&document, config)?;

    let (recorded, consumed) = match consult_decisions(&document, store, now).await {
        Ok(recorded) => recorded,
        Err(ConsultFault::Expired(blocking)) => {
            let parked_path = docs.parked().to_path_buf();
            let parked_display = parked_path.display().to_string();
            let removed = tokio::task::spawn_blocking(move || std::fs::remove_file(&parked_path))
                .await
                .map_err(|e| {
                    ResumeRefusal::Fault(Diagnostic::new(format!(
                        "the expired checkpoint's unlink task did not complete: {e}"
                    )))
                })?;
            match removed {
                Ok(()) => {}
                // Already gone: the teardown stays idempotent for a retried
                // resume of the same expired run.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(ResumeRefusal::Fault(Diagnostic::new(format!(
                        "unlinking the expired checkpoint {parked_display} failed: {e}"
                    ))));
                }
            }
            if let Err(e) = cancel_run_approvals(store, &path.run.to_string(), &request_id).await {
                return Err(ResumeRefusal::Fault(Diagnostic::new(format!(
                    "the approval sweep for the expired run did not complete: {e}"
                ))));
            }
            return Err(ResumeRefusal::Conflict(ResumeConflictRow::expired(
                blocking,
            )));
        }
        Err(fault) => return Err(fault.into()),
    };

    let grant = authorize(&docs, claims, &path, document, recorded, consumed).await?;
    Ok(grant)
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
        blocking: NonEmptyBlocking,
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
/// segment is atomic data — no streaming to the client mid-segment. The
/// orchestrator entry is the implementing seam; this wrapper keeps the
/// endpoint's call surface in the resume module.
pub async fn run_segment(
    grant: ResumeGrant,
    config: &AgentRuntimeConfig,
    headers: &HashMap<String, String>,
) -> Result<SegmentResult, SegmentError> {
    crate::orchestration::Orchestrator::run_resume_segment(grant, config, headers).await
}

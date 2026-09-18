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
use std::time::Duration;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::config::AgentRuntimeConfig;
use crate::hitl::{DecisionId, PendingApprovals};
use crate::provider_agent::{StreamError, StreamItem};
use crate::request_cancellation::RequestId;
use crate::streaming_request_hook::UsageState;

use super::super::RecordedDecisions;
use super::super::commit::{cancel_run_approvals, config_fingerprint};
use super::super::continuation::{RehydrateError, load_recorded_decisions};
use super::super::document::{ParkedRun, load_parked_run};
use super::super::lifetime::{ReservationFault, RunExecutionScope, RunReservationLease};
use super::claim::{
    ClaimResumeFault, ResumeClaimTable, ResumeDocuments, ResumeRunId, ResumeSessionId,
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

/// Build a mandatory blocking set from outstanding `(decision_id, tool,
/// deadline)` rows. The two re-park projections (the resume drive loop and
/// the coordinator continuation) iterate different sources but emit the same
/// entry shape, each entry carrying its own call's stored deadline; the
/// empty-set refusal is the caller's error to phrase.
pub(crate) fn blocking_from_calls<E>(
    calls: impl Iterator<Item = (DecisionId, String, chrono::DateTime<chrono::Utc>)>,
    empty: impl FnOnce() -> E,
) -> Result<NonEmptyBlocking, E> {
    let entries: Vec<BlockingEntry> = calls
        .map(|(decision_id, tool_name, expires_at)| BlockingEntry {
            decision_id,
            tool: ParkedToolName::new(tool_name),
            expires_at,
        })
        .collect();
    NonEmptyBlocking::try_new(entries).map_err(|EmptyBlocking| empty())
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

    /// The terminal expired row. `blocking` may be empty: a
    /// retention-expired checkpoint with every approval addressed is valid
    /// production evidence and must still render as `expired`, so the row
    /// never requires an outstanding call.
    pub(crate) fn expired(blocking: Vec<BlockingEntry>) -> Self {
        Self {
            code: ConflictCode::Expired,
            detail: Diagnostic::new(
                "the decision window closed before every pending call was decided",
            ),
            blocking,
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
/// render as the one conflict shape or as one of the typed fault rows.
///
/// The typed fault rows carry the diagnostic for server-side logging only;
/// the endpoint renders their fixed client codes (`reify_failed` /
/// `reify_unavailable`) and never the diagnostic text.
#[derive(Debug, Clone)]
pub enum ResumeRefusal {
    /// No checkpoint document under either name.
    DocumentAbsent,
    /// Identity binding is configured and the presented hash differs.
    IdentityMismatch,
    /// A not-ready conflict row.
    Conflict(ResumeConflictRow),
    /// The stored evidence is invalid — a corrupt or self-contradictory
    /// checkpoint, document, or record set. 500 `reify_failed`.
    InvalidEvidence(Diagnostic),
    /// A known pre-execution I/O availability failure: the evidence could
    /// not be read, not because it is wrong but because the store was not
    /// reachable. 503 `reify_unavailable`.
    Unavailable(Diagnostic),
    /// An internal fault outside the evidence/availability classes.
    /// 500 `reify_failed`.
    Internal(Diagnostic),
    /// The undifferentiated fault sink the ordered evaluation still fills
    /// from; the S2/C fills migrate its sites onto the typed rows above.
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
    /// The terminal expired row: blocking may be empty when every member was
    /// addressed before the run-wide window closed.
    Expired(Vec<BlockingEntry>),
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
            // never a fault: the spec fixes 409 for a live claim.
            ClaimResumeFault::Live => Self::Conflict(ResumeConflictRow::running()),
            // The claim seam's failures classify through the typed rows —
            // a filesystem availability failure is 503 `reify_unavailable`,
            // an internal task failure 500 `reify_failed` — never the
            // undifferentiated fault sink.
            ClaimResumeFault::Unavailable(diagnostic) => Self::Unavailable(diagnostic),
            ClaimResumeFault::Internal(diagnostic) => Self::Internal(diagnostic),
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

/// Authorization to execute one segment for a granted run. Owns the run's
/// reservation lease AND its one execution scope: the grant is single-use
/// and non-cloneable, and the run releases when the grant (and every lease
/// reference it handed out) ends.
#[derive(Debug)]
pub struct ResumeGrant {
    reservation: RunReservationLease,
    /// The run's ONE resumed execution scope, established when this grant
    /// was assembled from its reservation. Every consumer — supervisor,
    /// driver, tool contexts, guard — clones this same `Arc`, so the whole
    /// resumed execution shares one cancellation token and one tracker.
    scope: Arc<RunExecutionScope>,
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

    /// The reservation fencing this grant's run: the supervisor's lease
    /// reference, cloned from the grant's own so both share the one
    /// occupation.
    #[must_use]
    pub(crate) fn reservation(&self) -> &RunReservationLease {
        &self.reservation
    }

    /// The scope the resumed execution runs under: a clone of the grant's
    /// ONE scope `Arc`, established at conversion — never a fresh token or
    /// tracker. Two callers share the same cancellation and drain state, so
    /// a supervisor draining its scope joins every task registered through
    /// any scope handle the run handed out. The supervisor seam (S3/S4):
    /// the grant keeps ownership while the segment borrows the scope.
    #[must_use]
    pub fn execution_scope(&self) -> Arc<RunExecutionScope> {
        Arc::clone(&self.scope)
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

/// One run's reservation with its re-read checkpoint: the internal carrier
/// the ordered resume evaluation threads from reserve (step 2) through
/// re-read/recheck (step 3) and the member consult (step 4) to the
/// release-or-convert decision (steps 5–6). Dropping the carrier before
/// conversion releases the reservation with no execution.
#[derive(Debug)]
pub(crate) struct ReservedEvaluation {
    reservation: RunReservationLease,
    docs: ResumeDocuments,
    document: ParkedRun,
}

impl ReservedEvaluation {
    /// Arm the carrier with the held reservation and the checkpoint as
    /// re-read under it.
    pub(crate) fn new(
        reservation: RunReservationLease,
        docs: ResumeDocuments,
        document: ParkedRun,
    ) -> Self {
        Self {
            reservation,
            docs,
            document,
        }
    }

    /// The checkpoint as re-read under the reservation.
    #[must_use]
    pub(crate) fn document(&self) -> &ParkedRun {
        &self.document
    }

    /// The reservation fencing the run through the evaluation.
    #[must_use]
    pub(crate) fn reservation(&self) -> &RunReservationLease {
        &self.reservation
    }

    /// The run's two checkpoint paths.
    #[must_use]
    pub(crate) fn documents(&self) -> &ResumeDocuments {
        &self.docs
    }
}

/// Ordered-resume step 6: the consuming authorization/rename transition. The
/// reserved evaluation's held reservation becomes the grant's fence — the
/// parked document renames to its resuming name under that same reservation
/// (the blocking rename tail holding a lease reference), with no ownerless
/// gap and no second acquisition — and the grant assembles owning that same
/// reservation, establishing its ONE execution scope at conversion. A
/// refusal upstream drops the carrier, releasing the reservation with no
/// execution.
#[expect(
    unused_variables,
    reason = "the `claims` parameter stays unused: the held reservation is the conversion's whole fence, and the surface keeps the table parameter as declared"
)]
pub(crate) async fn convert_reserved(
    claims: &ResumeClaimTable,
    reserved: ReservedEvaluation,
    session: ResumeSessionId,
    run: ResumeRunId,
    recorded: Arc<RecordedDecisions>,
    consumed: Vec<DecisionId>,
) -> Result<ResumeGrant, ClaimResumeFault> {
    // Destructuring IS the consumption: the carrier is gone from here on, so
    // a rename failure drops the reservation inside this function — the
    // refused-outcome shape with no execution riding it.
    let ReservedEvaluation {
        reservation,
        docs,
        document,
    } = reserved;
    let parked = docs.parked().to_path_buf();
    let resuming = docs.resuming().to_path_buf();
    // The blocking tail MOVES a lease reference in: the fence survives an
    // awaiting request dropping, so the run stays reserved until the rename
    // has actually completed.
    let lease = reservation.clone();
    tokio::task::spawn_blocking(move || -> Result<(), ClaimResumeFault> {
        // The lease binding keeps the fence alive through the rename's
        // completion — the same shape as `rename_back_under_reservation`.
        let _lease = lease;
        // NO ENOENT tolerance on the source: the parked document was read
        // under this reservation moments before, so a missing parked name is
        // an honest failure, never a speculative fallback.
        std::fs::rename(&parked, &resuming).map_err(|e| {
            ClaimResumeFault::Unavailable(Diagnostic::new(format!(
                "renaming the parked checkpoint {} to its resuming name failed: {e}",
                parked.display()
            )))
        })
    })
    .await
    .map_err(|e| {
        ClaimResumeFault::Internal(Diagnostic::new(format!(
            "the conversion rename task did not complete: {e}"
        )))
    })??;

    // The scope is established exactly once, at conversion, over the same
    // reservation the grant owns — every later consumer clones this one Arc.
    // The reservation passes carrier → grant with no ownerless gap.
    let scope = RunExecutionScope::new(reservation.clone());
    Ok(ResumeGrant {
        reservation,
        scope,
        documents: docs,
        document,
        recorded,
        consumed,
        session,
        run,
    })
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
        Err(RehydrateError::Parked { blocking }) => {
            // The consult reports parked only with at least one pending
            // snapshot; an empty set would be an internal contradiction, so
            // refuse loudly rather than invent a row.
            let blocking = NonEmptyBlocking::try_new(blocking).map_err(|EmptyBlocking| {
                ConsultFault::Fault(Diagnostic::new(
                    "the consult answered parked with no outstanding calls",
                ))
            })?;
            Err(ConsultFault::Parked(blocking))
        }
        Err(RehydrateError::Expired { blocking }) => {
            // The terminal expired row, carrying the same pending snapshots
            // the consult collected — possibly empty when every member was
            // addressed before the run-wide window closed.
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

/// Evaluate a run under the ruled ordered entry and either convert the
/// reservation into the segment's grant or refuse with the first matching
/// row. The order is the contract: (1) the presented identity resolves enough
/// to preserve 404 privacy before anything is occupied, (2) the run is
/// reserved, (3) the authoritative checkpoint is re-read and rechecked for
/// identity, interruption, and fingerprint under that reservation, (4) the
/// member consult runs under it, (5) a pending or refused outcome releases
/// the reservation with no execution, and (6) a ready outcome converts the
/// SAME reservation into the grant, renaming parked to resuming with no
/// ownerless gap and no second acquisition. An expired refusal tears the run
/// down before rendering: the checkpoint is unlinked and the run's undecided
/// tickets are swept under the bundle's request id, so the row the client
/// sees matches the state left behind.
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

    // Step 1: resolve the presented identity before anything is occupied. An
    // unattributable caller under a bound config answers the not-found row
    // and never reaches the table.
    if let IdentityBindingState::BoundMissingHeader =
        IdentityBindingState::resolve(bind_identity, presented_identity)
    {
        return Err(IdentityFault::Mismatch.into());
    }

    // Step 2: reserve the run. The lock covers the check-and-insert only; the
    // lease is the fence every later step runs under, and a refusal anywhere
    // below drops it with no execution.
    let reservation = claims
        .reserve(&path.run)
        .map_err(|ReservationFault::Live| ClaimResumeFault::Live)
        .map_err(ResumeRefusal::from)?;

    // Step 3: the ONE authoritative read, now under the reservation, then the
    // identity, interruption, and fingerprint rechecks.
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

    let document = match located {
        LocatedCheckpoint::Parked { document } => {
            if document.executed.is_empty() {
                document
            } else {
                return Err(AdmitFault::Interrupted.into());
            }
        }
        LocatedCheckpoint::Resuming { document } => {
            if !document.executed.is_empty() {
                // The dead resume's document stays exactly as found: the
                // interrupted row refuses, and no rename hides the evidence.
                return Err(AdmitFault::Interrupted.into());
            }
            // The empty-resume recovery: the rename-back runs fenced by THIS
            // reservation, the original approval and retention deadlines
            // unchanged. The typed claim vocabulary classifies any failure —
            // propagated as-is, never flattened into the fault sink.
            claims
                .rename_back_under_reservation(&reservation, &docs)
                .await?;
            document
        }
    };

    check_fingerprint(&document, config)?;

    // Step 4: the member consult, under the reservation. The consult stays
    // the interim recorded-decisions path — E7 owns the production cutover.
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
            if let Err(e) =
                cancel_run_approvals(store, &path.run.to_string(), &request_id, None).await
            {
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

    // Steps 5-6: the ready outcome converts the SAME reservation into the
    // grant — no second acquisition and no ownerless gap. The parked document
    // renames to its resuming name under the lease-fenced tail, and the grant
    // owns that one reservation and its one execution scope.
    let grant = convert_reserved(
        claims,
        ReservedEvaluation::new(reservation, docs, document),
        path.session,
        path.run,
        recorded,
        consumed,
    )
    .await?;
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

/// One streamed resume segment's terminal bookkeeping: how the segment
/// ended for the supervisor driving it through [`run_segment_borrowed`].
/// Internal to the resumed execution's ownership chain, but nameable
/// through the `orchestration` facade because that driver is public.
#[derive(Debug, Clone)]
pub enum ResumeStreamEnd {
    /// The run completed within the segment; `final_answer` hands over to
    /// the normal factory finalization — the driver emits no terminal of
    /// its own.
    Completed {
        /// The run's final answer, for the normal factory finalization.
        final_answer: String,
    },
    /// The segment ended in a fresh park: the publication owner emits
    /// `RunParked` once, and the supervisor still applies the normal
    /// terminal stream policy.
    Reparked,
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

/// Drive one segment for a run the supervisor still owns: the borrowed,
/// stream-shaped form of [`run_segment`].
///
/// Events forward through `event_tx` — the same sender the normal
/// completion path drives — and usage accumulates into the caller's
/// shared `usage_state`, under the configured normal visibility and
/// buffering. The driver emits no duplicate final: a completed segment
/// hands the final answer to the normal factory finalization, and a
/// re-park leaves `RunParked` to the publication owner. `outer_budget`
/// is the resumed coordinator's outer request budget: the factory
/// projects it from its timeout by the normal
/// `(!timeout.is_zero()).then_some(timeout)` convention. Cancellation
/// runs through the grant's one execution scope
/// ([`ResumeGrant::execution_scope`]); it is the sole cancellation path,
/// with no second token parameter.
pub async fn run_segment_borrowed(
    grant: &ResumeGrant,
    config: &AgentRuntimeConfig,
    headers: &HashMap<String, String>,
    event_tx: tokio::sync::mpsc::Sender<Result<StreamItem, StreamError>>,
    usage_state: UsageState,
    outer_budget: Option<Duration>,
) -> Result<ResumeStreamEnd, SegmentError> {
    let mut config = config.clone();
    // The resume segment's HITL gate stamps every wire POST with the
    // config's `request_id`, and the chat path's `req_<uuid>` value is
    // runtime state that never persists. Left unset, a re-park's authorize
    // POST carries an empty `request_id`, which the governance schema
    // rejects (surfacing as an opaque HTTP 500). Stamp the run owner id —
    // the same id the park bridge re-mints parked rows to — so resumed wire
    // POSTs name the checkpointed run (the gov-500 rule; precedent
    // `Orchestrator::run_resume_segment`).
    config.request_id = Some(crate::orchestration::park::run_owner_id(
        &grant.checkpoint().run_id,
    ));
    // Header re-resolution is a deliberate no-op here: S1's
    // `prepare_agent_config` already resolved `headers_from_request`
    // forwarding once against the resume caller into this config, and the
    // supervisor passes the EMPTY headers map. The parameter is retained
    // for the frozen seam's call shape.
    let _ = headers;
    crate::orchestration::Orchestrator::run_resume_segment_borrowed(
        grant,
        &config,
        event_tx,
        usage_state,
        outer_budget,
    )
    .await
}

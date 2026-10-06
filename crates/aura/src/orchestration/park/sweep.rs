//! The park retention sweep: reclaim expired checkpoint evidence and orphaned
//! approval rows on a bounded cadence, fenced by the shared run-reservation
//! table so a live run is never touched.
//!
//! One [`ParkSweep`] engine is built per park-enabled config. The server runs
//! one full pass to completion before accepting requests, then spawns a
//! nonoverlapping loop that sleeps 60 seconds between passes. A missing
//! checkpoint root is not an error — a config may simply have no parked
//! evidence yet — but an unreadable root or document is warned and kept.
//!
//! The approval store is shared by every agent in the process, so each engine
//! reclaims only the rows its own instance parked — the same `instance_id`
//! ownership key the poll reconciler claims by. A shorter-TTL sweep therefore
//! cannot treat another agent's still-valid run as an orphan.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aura_config::ParkTtl;
use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::hitl::{AgentScope, ParkedApproval, PendingApprovals};
use crate::session_store::{ApprovalStore, RetainedApproval, SessionStoreError};

use super::cleanup::{
    CheckpointPresence, CleanupAdmissionFault, CleanupReservation, RunCleanupOutcome,
    delete_expired_run, inspect_checkpoint_presence, scan_checkpoint_root,
};
use super::document::ParkedRun;
use super::resume::claim::{ResumeClaimTable, ResumeRunId, ValidatedResumePath};
use super::resume::evaluate::Diagnostic;

/// Cadence between completed passes.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// One run the sweep is considering, with the session and any retained rows
/// that named it as an orphan.
struct Candidate {
    run_id: String,
    session: Option<String>,
    retained_rows: Vec<ParkedApproval>,
}

/// The retention-sweep engine for one park-enabled config.
pub struct ParkSweep {
    table: Arc<ResumeClaimTable>,
    registry: PendingApprovals,
    store: Arc<dyn ApprovalStore>,
    memory_dir: String,
    park_ttl: ParkTtl,
    label: String,
    instance_id: String,
}

impl std::fmt::Debug for ParkSweep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParkSweep")
            .field("table", &self.table)
            .field("memory_dir", &self.memory_dir)
            .field("park_ttl", &self.park_ttl)
            .field("label", &self.label)
            .finish()
    }
}

impl ParkSweep {
    /// Build one sweep engine for a park-enabled config. `table` must be the
    /// same shared claim table requests fence on, so a live run refuses cleanup
    /// here exactly as it refuses a duplicate claim in the handler.
    /// `instance_id` is the config's effective instance id — the same identity
    /// the gate stamps on the rows it parks — so this engine reclaims only
    /// rows its own agent parked out of the shared store.
    #[must_use]
    pub fn new(
        table: Arc<ResumeClaimTable>,
        registry: PendingApprovals,
        store: Arc<dyn ApprovalStore>,
        memory_dir: String,
        park_ttl: ParkTtl,
        label: String,
        instance_id: String,
    ) -> Self {
        Self {
            table,
            registry,
            store,
            memory_dir,
            park_ttl,
            label,
            instance_id,
        }
    }

    /// One full pass: enumerate checkpoint roots, union with retained rows
    /// that name orphaned runs, and delete runs whose deadline has passed or
    /// whose orphaned rows have aged out. Missing roots are benign; unreadable
    /// evidence is warned and kept.
    pub async fn run_pass(&self) {
        let now = Utc::now();
        let roots = match enumerate_checkpoint_roots(&self.memory_dir) {
            Ok(roots) => roots,
            Err(diagnostic) => {
                warn!(
                    agent = %self.label,
                    error = %diagnostic,
                    "park retention sweep could not enumerate checkpoint roots"
                );
                return;
            }
        };

        // Collect candidates from checkpoint scans.
        let mut candidates: HashMap<String, Candidate> = HashMap::new();
        for (session, root) in roots {
            match scan_checkpoint_root(&root).await {
                Ok(ids) => {
                    for run_id in ids {
                        candidates
                            .entry(run_id.clone())
                            .or_insert_with(|| Candidate {
                                run_id: run_id.clone(),
                                session: session.clone(),
                                retained_rows: Vec::new(),
                            });
                    }
                }
                Err(diagnostic) => {
                    warn!(
                        agent = %self.label,
                        root = %root.display(),
                        error = %diagnostic,
                        "park retention sweep could not scan checkpoint root"
                    );
                }
            }
        }

        // Attach this instance's retained rows to the runs they name: a
        // candidate discovered by checkpoint scan gains its registry
        // evidence (the age anchor a corrupt or absent checkpoint reclaims
        // from), and a run with no scanned checkpoint becomes an orphan
        // candidate in its own right.
        if let Err(err) = self.attach_retained_rows(&mut candidates).await {
            warn!(
                agent = %self.label,
                error = %err,
                "park retention sweep could not read retained rows"
            );
            return;
        }

        for candidate in candidates.into_values() {
            self.process_candidate(candidate, now).await;
        }
    }

    /// Spawn the nonoverlapping cadence loop on the current runtime. The loop
    /// stops when `shutdown` cancels or when the returned handle's
    /// [`SweepHandle::stop`] runs. Dropping the handle detaches the loop.
    pub fn spawn(self, shutdown: &CancellationToken) -> SweepHandle {
        let token = shutdown.child_token();
        let task = tokio::spawn(self.run(token.clone()));
        SweepHandle { token, task }
    }

    async fn run(self, token: CancellationToken) {
        loop {
            self.run_pass().await;
            tokio::select! {
                () = token.cancelled() => break,
                _ = tokio::time::sleep(SWEEP_INTERVAL) => {}
            }
        }
    }

    /// Attach every one of this instance's retained rows to the run it
    /// names. The store is shared by every agent in the process, so another
    /// agent's row is not this sweep's to consider — its checkpoint lives in
    /// that agent's memory dir, and reaping it would strand that agent's
    /// run. Single scopes carry no run id and do not contribute.
    async fn attach_retained_rows(
        &self,
        candidates: &mut HashMap<String, Candidate>,
    ) -> Result<(), SessionStoreError> {
        let rows = self.store.retained_rows().await?;
        for retained in rows {
            let approval = match retained {
                RetainedApproval::Pending(approval) => approval,
                RetainedApproval::Addressed { approval, .. } => approval,
            };
            if approval.request.instance_id != self.instance_id {
                continue;
            }
            let Some(run_id) = run_id_from_scope(&approval.request.scope) else {
                continue;
            };
            match candidates.entry(run_id.clone()) {
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    entry.get_mut().retained_rows.push(approval);
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let session = session_from_scope(&approval.request.scope);
                    entry.insert(Candidate {
                        run_id,
                        session,
                        retained_rows: vec![approval],
                    });
                }
            }
        }
        Ok(())
    }

    async fn process_candidate(&self, candidate: Candidate, now: DateTime<Utc>) {
        let run = match ResumeRunId::parse(&candidate.run_id) {
            Ok(run) => run,
            Err(err) => {
                warn!(
                    agent = %self.label,
                    run_id = %candidate.run_id,
                    error = %err,
                    "park retention sweep skipped a malformed run id"
                );
                return;
            }
        };

        let cleanup = match self.acquire(&run, candidate.session.as_deref()) {
            Ok(cleanup) => cleanup,
            Err(CleanupAdmissionFault::Executing) => return,
            Err(CleanupAdmissionFault::MalformedPath(err)) => {
                warn!(
                    agent = %self.label,
                    run_id = %candidate.run_id,
                    session = %candidate.session.as_deref().unwrap_or("sessionless"),
                    error = %err,
                    "park retention sweep skipped a malformed resume path"
                );
                return;
            }
        };

        match inspect_checkpoint_presence(&cleanup).await {
            CheckpointPresence::Present => {
                match read_retention_deadline(&cleanup).await {
                    Some(deadline) if now > deadline => {
                        self.delete(&cleanup, &candidate.run_id).await;
                    }
                    Some(_) => {
                        // Unexpired: keep.
                    }
                    None => {
                        warn!(
                            agent = %self.label,
                            run_id = %candidate.run_id,
                            "park retention sweep kept a present checkpoint whose retention deadline could not be read"
                        );
                    }
                }
            }
            CheckpointPresence::ConfirmedAbsent => {
                match orphan_deadline(&candidate.retained_rows, self.park_ttl) {
                    Some(deadline) if now > deadline => {
                        self.delete(&cleanup, &candidate.run_id).await;
                    }
                    Some(_) => {
                        // Orphan within its grace period: keep.
                    }
                    None => {
                        warn!(
                            agent = %self.label,
                            run_id = %candidate.run_id,
                            "park retention sweep kept an absent checkpoint with no retained rows to age from"
                        );
                    }
                }
            }
            CheckpointPresence::Inaccessible(ref diagnostic) => {
                warn!(
                    agent = %self.label,
                    run_id = %candidate.run_id,
                    error = %diagnostic,
                    "park retention sweep kept unreadable checkpoint evidence"
                );
            }
            CheckpointPresence::Corrupt(ref diagnostic) => {
                // An undecodable checkpoint carries no readable retention
                // deadline, so its rows' registration age stands in — the
                // same anchor the confirmed-absent orphan path ages from.
                // Without rows there is no anchor, and the corrupt document
                // is kept and warned rather than guessed at. Reclaiming
                // here is what keeps a corrupt checkpoint from answering
                // the resume endpoint with an immortal `reify_failed`:
                // once its rows age out, the evidence goes and the run
                // reads absent.
                match orphan_deadline(&candidate.retained_rows, self.park_ttl) {
                    Some(deadline) if now > deadline => {
                        self.delete(&cleanup, &candidate.run_id).await;
                    }
                    Some(_) => {
                        // Corrupt within its grace period: keep.
                    }
                    None => {
                        warn!(
                            agent = %self.label,
                            run_id = %candidate.run_id,
                            error = %diagnostic,
                            "park retention sweep kept a corrupt checkpoint with no retained rows to age from"
                        );
                    }
                }
            }
        }
    }

    fn acquire(
        &self,
        run: &ResumeRunId,
        session: Option<&str>,
    ) -> Result<CleanupReservation, CleanupAdmissionFault> {
        let table = self.table.reservation_table();
        match session {
            Some(session) => {
                let path = ValidatedResumePath::parse(session, &run.to_string())
                    .map_err(|err| CleanupAdmissionFault::MalformedPath(err.to_string()))?;
                CleanupReservation::acquire(table, &path, &self.memory_dir)
            }
            None => CleanupReservation::acquire_sessionless(table, run, &self.memory_dir),
        }
    }

    async fn delete(&self, cleanup: &CleanupReservation, run_id: &str) {
        match delete_expired_run(cleanup, &self.registry).await {
            RunCleanupOutcome::Removed => {
                info!(
                    agent = %self.label,
                    run_id,
                    "park retention sweep removed expired run evidence"
                );
            }
            RunCleanupOutcome::RetainedForRetry(diagnostic) => {
                warn!(
                    agent = %self.label,
                    run_id,
                    error = %diagnostic,
                    "park retention sweep retained expired run for retry"
                );
            }
        }
    }
}

/// How a spawned sweep loop's task ended, as joined by [`SweepHandle::stop`].
#[derive(Debug)]
pub enum SweepExit {
    /// The loop left through its cancel token and joined cleanly.
    Cancelled,
    /// The loop's task died of a panic; the join carried the panic back
    /// to the handle owner.
    Panicked,
}

/// Stop/join handle for a spawned [`ParkSweep`].
pub struct SweepHandle {
    token: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl SweepHandle {
    /// Cancel the loop and await its exit, reporting how the task ended.
    /// The in-flight pass completes first — a checkpoint read or deletion
    /// already under way is never cut. A panic join is logged here, then
    /// reported as [`SweepExit::Panicked`]; with no abort path, any other
    /// join error is warned and reported as the clean [`SweepExit::Cancelled`].
    #[must_use]
    pub async fn stop(self) -> SweepExit {
        self.token.cancel();
        match self.task.await {
            Ok(()) => SweepExit::Cancelled,
            Err(join) if join.is_panic() => {
                warn!(panic = %join, "park retention sweep loop died of a panic");
                SweepExit::Panicked
            }
            Err(join) => {
                warn!(error = %join, "park retention sweep loop ended without a panic");
                SweepExit::Cancelled
            }
        }
    }
}

/// Enumerate the checkpoint roots owned by this config's `memory_dir`: the
/// sessionless root `{memory_dir}/parked/` plus every direct subdirectory
/// that contains a `parked/` child. Missing roots are omitted — a config with
/// no parked evidence yet contributes no candidates.
fn enumerate_checkpoint_roots(
    memory_dir: &str,
) -> Result<Vec<(Option<String>, PathBuf)>, Diagnostic> {
    let root = Path::new(memory_dir);
    let mut roots = Vec::new();

    let sessionless = root.join("parked");
    if sessionless.is_dir() {
        roots.push((None, sessionless));
    }

    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(roots),
        Err(e) => {
            return Err(Diagnostic::new(format!(
                "memory root {} could not be read: {e}",
                root.display()
            )));
        }
    };
    for entry in entries {
        let entry = entry.map_err(|e| {
            Diagnostic::new(format!(
                "memory root {} entry could not be read: {e}",
                root.display()
            ))
        })?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let parked = path.join("parked");
        if parked.is_dir() {
            let session = path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string);
            roots.push((session, parked));
        }
    }

    Ok(roots)
}

/// Extract the run id from a scope, if any. Single scopes carry no run id.
fn run_id_from_scope(scope: &AgentScope) -> Option<String> {
    match scope {
        AgentScope::Worker { run_id, .. } => Some(run_id.to_string()),
        AgentScope::Coordinator { run_id } => Some(run_id.to_string()),
        AgentScope::Single { .. } => None,
    }
}

/// Extract the session id from a scope, if any.
fn session_from_scope(scope: &AgentScope) -> Option<String> {
    match scope {
        AgentScope::Single { session_id } | AgentScope::Worker { session_id, .. } => {
            session_id.as_ref().map(|id| id.as_str().to_string())
        }
        AgentScope::Coordinator { .. } => None,
    }
}

/// The orphan deadline: the latest `registered_at` of the run's retained rows
/// plus the validated `park_ttl`. A run is deleted only after every retained
/// row has aged out, so a late row extends the grace period.
fn orphan_deadline(rows: &[ParkedApproval], park_ttl: ParkTtl) -> Option<DateTime<Utc>> {
    let latest = rows.iter().map(|row| row.registered_at).max()?;
    let secs = i64::try_from(park_ttl.as_secs()).ok()?;
    let age = chrono::Duration::try_seconds(secs)?;
    latest.checked_add_signed(age)
}

/// Read whichever checkpoint document is present and return its absolute
/// retention deadline. `inspect_checkpoint_presence` already proved at least
/// one healthy document exists; this helper is a best-effort deadline read.
async fn read_retention_deadline(cleanup: &CleanupReservation) -> Option<DateTime<Utc>> {
    let docs = cleanup.documents().clone();
    tokio::task::spawn_blocking(move || {
        for path in [docs.parked().to_path_buf(), docs.resuming().to_path_buf()] {
            match std::fs::read(&path) {
                Ok(bytes) => {
                    if let Ok(document) = serde_json::from_slice::<ParkedRun>(&bytes) {
                        return Some(document.retention_expires_at.as_datetime());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => continue,
            }
        }
        None
    })
    .await
    .unwrap_or(None)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;
    use std::str::FromStr;

    use std::sync::Arc;

    use super::ParkSweep;
    use crate::hitl::{
        AcknowledgmentState, AgentScope, ApprovalAuthority, ApprovalItem, ApprovalOrigin,
        ApprovalRequest, DecisionId, PROTOCOL_VERSION, ParkedApproval, PendingApprovals,
    };
    use crate::orchestration::park::commit::run_owner_id;
    use crate::orchestration::park::document::{
        ParkedPlan, ParkedRun, ParkedTaskNode, SCHEMA_VERSION,
    };
    use crate::orchestration::park::resume::claim::{ResumeClaimTable, ResumeDocuments};
    use crate::orchestration::park::retention::RetentionExpiresAt;
    use crate::orchestration::{PendingCall, RunId, TaskIdentity, TaskStatus};
    use crate::session_store::{ApprovalStore, FileApprovalStore, InMemoryEventBus};

    const SESSION: &str = "sess-sweep";
    const RUN_A: &str = "0199c0de-4545-7000-8000-000000000045";
    const RUN_B: &str = "0199c0de-4545-7000-8000-000000000046";
    const DECISION: &str = "0199c0de-4545-7000-8000-000000000042";
    const INSTANCE: &str = "cleanup-instance";
    const INSTANCE_A: &str = "instance-alpha";
    const INSTANCE_B: &str = "instance-beta";

    fn parse_run_id(raw: &str) -> RunId {
        RunId::from_str(raw).expect("fixture run id parses")
    }

    fn documents(memory_dir: &str, session: Option<&str>, run: &str) -> ResumeDocuments {
        match session {
            Some(s) => {
                let path =
                    super::ValidatedResumePath::parse(s, run).expect("fixture path validates");
                ResumeDocuments::for_path(&path, memory_dir)
            }
            None => ResumeDocuments::for_sessionless(
                &super::ResumeRunId::parse(run).unwrap(),
                memory_dir,
            ),
        }
    }

    fn retention_expires_at(stamp: &str) -> RetentionExpiresAt {
        RetentionExpiresAt::from_datetime(
            chrono::DateTime::parse_from_rfc3339(stamp)
                .expect("fixture stamp parses")
                .with_timezone(&chrono::Utc),
        )
    }

    fn checkpoint_document(run_id: &str, retention: RetentionExpiresAt) -> ParkedRun {
        let decision_id = DecisionId::parse(DECISION).expect("fixture decision id parses");
        ParkedRun {
            schema_version: SCHEMA_VERSION,
            session_id: Some(SESSION.to_string()),
            run_id: run_id.to_string(),
            parked_at: "2026-09-01T00:00:00Z".to_string(),
            retention_expires_at: retention,
            query: "Deploy the service".to_string(),
            chat_history: vec![rig::completion::Message::user("Deploy the service")],
            coordinator_conversation: vec![],
            routing_decision: None,
            iteration: 1,
            planning_ms: 0,
            failure_history: vec![],
            plan: ParkedPlan {
                goal: "Deploy the service".to_string(),
                steps: None,
                tasks: vec![ParkedTaskNode {
                    task_id: 3,
                    description: "Gated apply".to_string(),
                    dependencies: vec![],
                    worker: Some("operations".to_string()),
                    rationale: String::new(),
                    status: TaskStatus::AwaitingApproval,
                    result: None,
                    error: None,
                    failure_category: None,
                    attempt: Some(1),
                    history: Some(vec![rig::completion::Message::user("apply it")]),
                    current_prompt: Some(rig::completion::Message::user("tool results")),
                    pending: Some(vec![PendingCall {
                        decision_id,
                        tool_name: "kubectl_apply".to_string(),
                        arguments: serde_json::json!({ "namespace": "prod" }),
                        call_id: "call_apply_1".to_string(),
                    }]),
                }],
            },
            executed: vec![],
            config_fingerprint: "fingerprint".to_string(),
            identity_hash: None,
            request_egress: HashMap::new(),
        }
    }

    async fn write_document(path: &Path, document: &ParkedRun) {
        let bytes = serde_json::to_vec_pretty(document).expect("document serializes");
        write_checkpoint_bytes(path, bytes).await;
    }

    /// A checkpoint that reads but never decodes: valid bytes, wrong shape.
    async fn write_garbage_checkpoint(path: &Path) {
        write_checkpoint_bytes(path, b"{ not a parked run".to_vec()).await;
    }

    async fn write_checkpoint_bytes(path: &Path, bytes: Vec<u8>) {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, bytes)
        })
        .await
        .expect("blocking write task joins")
        .expect("write document");
    }

    fn stage_directory_at(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent directory");
        }
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(path);
        std::fs::create_dir(path).expect("stage directory at document path");
    }

    fn run_approval(
        instance_id: &str,
        run_id: &str,
        decision_id: DecisionId,
        registered_at: chrono::DateTime<chrono::Utc>,
    ) -> ParkedApproval {
        ParkedApproval {
            request: ApprovalRequest {
                version: PROTOCOL_VERSION,
                instance_id: instance_id.to_string(),
                decision_id,
                request_id: run_owner_id(run_id),
                scope: AgentScope::Worker {
                    run_id: parse_run_id(run_id),
                    task: TaskIdentity::new(3, None),
                    session_id: Some(crate::config::SessionId::new(SESSION)),
                },
                origin: ApprovalOrigin::ConfigGate {
                    matched_pattern: "kubectl_*".to_string(),
                    agent_name: "test-agent".to_string(),
                },
                items: vec![ApprovalItem {
                    tool_name: "kubectl_apply".to_string(),
                    tool_namespace: None,
                    arguments: serde_json::json!({ "namespace": "prod" }),
                    tool_call_intent: None,
                }],
            },
            registered_at,
            expires_at: registered_at + chrono::Duration::seconds(3600),
            authority: ApprovalAuthority::Conversational,
            egress_headers: None,
            acknowledgment: AcknowledgmentState::RequiresNotification,
        }
    }

    async fn stage_run_approval(
        registry: &PendingApprovals,
        run_id: &str,
        registered_at: chrono::DateTime<chrono::Utc>,
    ) -> DecisionId {
        stage_run_approval_for(INSTANCE, registry, run_id, registered_at).await
    }

    async fn stage_run_approval_for(
        instance_id: &str,
        registry: &PendingApprovals,
        run_id: &str,
        registered_at: chrono::DateTime<chrono::Utc>,
    ) -> DecisionId {
        let decision_id = DecisionId::generate();
        registry
            .register_durable(run_approval(
                instance_id,
                run_id,
                decision_id,
                registered_at,
            ))
            .await
            .expect("stage run approval");
        decision_id
    }

    fn make_registry(
        root: &Path,
    ) -> (
        PendingApprovals,
        Arc<dyn crate::session_store::ApprovalStore>,
    ) {
        let store: Arc<dyn ApprovalStore> =
            Arc::new(FileApprovalStore::open(root.join("approvals")).unwrap());
        let registry =
            PendingApprovals::with_backend(store.clone(), Arc::new(InMemoryEventBus::new()));
        (registry, store)
    }

    fn sweep(
        registry: PendingApprovals,
        store: Arc<dyn crate::session_store::ApprovalStore>,
        memory_dir: &str,
        park_ttl: u64,
    ) -> ParkSweep {
        sweep_as(
            INSTANCE,
            "sweep-test",
            registry,
            store,
            memory_dir,
            park_ttl,
        )
    }

    fn sweep_as(
        instance_id: &str,
        label: &str,
        registry: PendingApprovals,
        store: Arc<dyn crate::session_store::ApprovalStore>,
        memory_dir: &str,
        park_ttl: u64,
    ) -> ParkSweep {
        ParkSweep::new(
            Arc::new(ResumeClaimTable::new()),
            registry,
            store,
            memory_dir.to_string(),
            aura_config::ParkTtl::try_new(park_ttl).expect("fixture park ttl validates"),
            label.to_string(),
            instance_id.to_string(),
        )
    }

    #[tokio::test]
    async fn pass_deletes_expired_checkpoint_and_registry_evidence() {
        let dir = tempfile::tempdir().expect("temp memory root");
        let memory_dir = dir.path().to_string_lossy().to_string();
        let (registry, store) = make_registry(dir.path());
        let decision_id = stage_run_approval(&registry, RUN_A, chrono::Utc::now()).await;

        let docs = documents(&memory_dir, Some(SESSION), RUN_A);
        write_document(
            docs.parked(),
            &checkpoint_document(RUN_A, retention_expires_at("2026-09-01T00:00:00Z")),
        )
        .await;

        let sweep = sweep(registry.clone(), store.clone(), &memory_dir, 3600);
        sweep.run_pass().await;

        assert!(!docs.parked().exists(), "expired checkpoint is deleted");
        assert!(
            registry.try_parked(&decision_id).await.unwrap().is_none(),
            "registry evidence is swept"
        );
    }

    #[tokio::test]
    async fn pass_keeps_unexpired_present_checkpoint() {
        let dir = tempfile::tempdir().expect("temp memory root");
        let memory_dir = dir.path().to_string_lossy().to_string();
        let (registry, store) = make_registry(dir.path());
        let decision_id = stage_run_approval(&registry, RUN_A, chrono::Utc::now()).await;

        let docs = documents(&memory_dir, Some(SESSION), RUN_A);
        write_document(
            docs.parked(),
            &checkpoint_document(RUN_A, retention_expires_at("2126-09-01T00:00:00Z")),
        )
        .await;

        let sweep = sweep(registry.clone(), store.clone(), &memory_dir, 3600);
        sweep.run_pass().await;

        assert!(docs.parked().exists(), "unexpired checkpoint is kept");
        assert!(
            registry.try_parked(&decision_id).await.unwrap().is_some(),
            "registry evidence is kept"
        );
    }

    #[tokio::test]
    async fn pass_deletes_orphan_rows_past_registered_at_plus_ttl() {
        let dir = tempfile::tempdir().expect("temp memory root");
        let memory_dir = dir.path().to_string_lossy().to_string();
        let (registry, store) = make_registry(dir.path());
        let registered_at = chrono::Utc::now() - chrono::Duration::seconds(7201);
        let decision_id = stage_run_approval(&registry, RUN_A, registered_at).await;

        let sweep = sweep(registry.clone(), store.clone(), &memory_dir, 3600);
        sweep.run_pass().await;

        assert!(
            registry.try_parked(&decision_id).await.unwrap().is_none(),
            "orphan rows past registered_at + park_ttl are swept"
        );
    }

    #[tokio::test]
    async fn pass_keeps_orphan_rows_within_grace() {
        let dir = tempfile::tempdir().expect("temp memory root");
        let memory_dir = dir.path().to_string_lossy().to_string();
        let (registry, store) = make_registry(dir.path());
        let registered_at = chrono::Utc::now() - chrono::Duration::seconds(60);
        let decision_id = stage_run_approval(&registry, RUN_A, registered_at).await;

        let sweep = sweep(registry.clone(), store.clone(), &memory_dir, 3600);
        sweep.run_pass().await;

        assert!(
            registry.try_parked(&decision_id).await.unwrap().is_some(),
            "orphan rows within registered_at + park_ttl grace are kept"
        );
    }

    #[tokio::test]
    async fn pass_skips_live_occupied_run() {
        let dir = tempfile::tempdir().expect("temp memory root");
        let memory_dir = dir.path().to_string_lossy().to_string();
        let (registry, store) = make_registry(dir.path());
        let decision_id = stage_run_approval(&registry, RUN_A, chrono::Utc::now()).await;

        let docs = documents(&memory_dir, Some(SESSION), RUN_A);
        write_document(
            docs.parked(),
            &checkpoint_document(RUN_A, retention_expires_at("2026-09-01T00:00:00Z")),
        )
        .await;

        let table = Arc::new(ResumeClaimTable::new());
        let run = super::ResumeRunId::parse(RUN_A).unwrap();
        let _lease = table.reserve(&run).expect("admit the executing run");

        let sweep = ParkSweep::new(
            table,
            registry.clone(),
            store.clone(),
            memory_dir,
            aura_config::ParkTtl::try_new(3600).unwrap(),
            "sweep-test".to_string(),
            INSTANCE.to_string(),
        );
        sweep.run_pass().await;

        assert!(
            docs.parked().exists(),
            "live occupied checkpoint is untouched"
        );
        assert!(
            registry.try_parked(&decision_id).await.unwrap().is_some(),
            "live occupied registry evidence is untouched"
        );
    }

    /// A checkpoint the filesystem refuses to read (staged here as a
    /// directory squatting on the document path) is `Inaccessible`, not
    /// corrupt: the bytes were never judged, so the evidence is kept and
    /// warned regardless of age.
    #[tokio::test]
    async fn pass_keeps_unreadable_checkpoint_evidence() {
        let dir = tempfile::tempdir().expect("temp memory root");
        let memory_dir = dir.path().to_string_lossy().to_string();
        let (registry, store) = make_registry(dir.path());
        let decision_id = stage_run_approval(&registry, RUN_A, chrono::Utc::now()).await;

        let docs = documents(&memory_dir, Some(SESSION), RUN_A);
        // Replace the checkpoint with a directory so the read fails structurally.
        stage_directory_at(docs.parked());

        let sweep = sweep(registry.clone(), store.clone(), &memory_dir, 3600);
        sweep.run_pass().await;

        assert!(
            docs.parked().exists(),
            "unreadable checkpoint evidence is kept"
        );
        assert!(
            registry.try_parked(&decision_id).await.unwrap().is_some(),
            "registry evidence for an unreadable checkpoint is kept"
        );
    }

    /// A checkpoint that reads but does not decode is `Corrupt`: within its
    /// rows' grace period it is kept, exactly like any other unexpired run.
    #[tokio::test]
    async fn pass_keeps_corrupt_checkpoint_within_grace() {
        let dir = tempfile::tempdir().expect("temp memory root");
        let memory_dir = dir.path().to_string_lossy().to_string();
        let (registry, store) = make_registry(dir.path());
        let decision_id = stage_run_approval(&registry, RUN_A, chrono::Utc::now()).await;

        let docs = documents(&memory_dir, Some(SESSION), RUN_A);
        write_garbage_checkpoint(docs.parked()).await;

        let sweep = sweep(registry.clone(), store.clone(), &memory_dir, 3600);
        sweep.run_pass().await;

        assert!(
            docs.parked().exists(),
            "corrupt checkpoint within its rows' grace period is kept"
        );
        assert!(
            registry.try_parked(&decision_id).await.unwrap().is_some(),
            "registry evidence for an in-grace corrupt checkpoint is kept"
        );
    }

    /// Once a corrupt checkpoint's rows age past the TTL there is no
    /// retention deadline left to honor — the document cannot be read, let
    /// alone resumed — so the sweep reclaims it instead of answering the
    /// resume endpoint with an immortal `reify_failed`.
    #[tokio::test]
    async fn pass_reclaims_corrupt_checkpoint_past_row_age() {
        let dir = tempfile::tempdir().expect("temp memory root");
        let memory_dir = dir.path().to_string_lossy().to_string();
        let (registry, store) = make_registry(dir.path());
        let registered_at = chrono::Utc::now() - chrono::Duration::seconds(7201);
        let decision_id = stage_run_approval(&registry, RUN_A, registered_at).await;

        let docs = documents(&memory_dir, Some(SESSION), RUN_A);
        write_garbage_checkpoint(docs.parked()).await;

        let sweep = sweep(registry.clone(), store.clone(), &memory_dir, 3600);
        sweep.run_pass().await;

        assert!(
            !docs.parked().exists(),
            "corrupt checkpoint with aged-out rows is reclaimed"
        );
        assert!(
            registry.try_parked(&decision_id).await.unwrap().is_none(),
            "registry evidence for an aged-out corrupt checkpoint is swept"
        );
    }

    /// `enumerate_checkpoint_roots` returns a diagnostic when the memory root
    /// itself is unreadable, so a sweep never treats an unreadable root as
    /// empty and silently fresh.
    #[test]
    fn enumerate_roots_diagnoses_unreadable_memory_root() {
        let dir = tempfile::tempdir().expect("temp memory root");
        let root = dir.path().join("memory-root-file");
        std::fs::write(&root, b"not a directory").expect("stage file at memory root path");
        assert!(root.is_file(), "fixture: memory root is a file");
        let result = super::enumerate_checkpoint_roots(root.to_string_lossy().as_ref());
        assert!(
            result.is_err(),
            "an unreadable memory root returns a diagnostic"
        );
    }

    /// A missing memory root is benign: retained orphan rows still age and
    /// are swept, rather than the pass returning early and leaving them
    /// behind forever.
    #[tokio::test]
    async fn pass_sweeps_aged_orphans_when_memory_root_is_missing() {
        let dir = tempfile::tempdir().expect("temp memory root");
        // Use a separate, non-existent path for checkpoint roots while the
        // approval store lives under the temp directory.
        let missing_memory = dir
            .path()
            .join("no-such-memory")
            .to_string_lossy()
            .to_string();
        let (registry, store) = make_registry(dir.path());
        let registered_at = chrono::Utc::now() - chrono::Duration::seconds(7201);
        let decision_id = stage_run_approval(&registry, RUN_A, registered_at).await;

        assert!(
            !Path::new(&missing_memory).exists(),
            "fixture: memory dir is absent"
        );
        let sweep = sweep(registry.clone(), store.clone(), &missing_memory, 3600);
        sweep.run_pass().await;

        assert!(
            registry.try_parked(&decision_id).await.unwrap().is_none(),
            "aged orphan rows are swept even when the memory root is missing"
        );
    }

    /// Regression (PR #760 review): two park-enabled agents share one
    /// approval store, and each engine scans only its own `memory_dir` for
    /// checkpoints. Agent alpha's sweep must not treat beta's rows as
    /// orphans — beta's checkpoint lives in beta's memory dir and is still
    /// unexpired — even though beta's rows have aged past alpha's shorter
    /// TTL. Cancelling them would strand beta's run: its checkpoint survives
    /// with its decisions gone.
    #[tokio::test]
    async fn pass_leaves_another_agents_parked_run_alone() {
        let dir = tempfile::tempdir().expect("temp memory root");
        let (registry, store) = make_registry(dir.path());

        // Beta parked under its own memory dir with an unexpired checkpoint,
        // but its rows registered long enough ago that alpha's TTL has lapsed.
        let memory_beta = dir.path().join("memory-beta").to_string_lossy().to_string();
        let registered_at = chrono::Utc::now() - chrono::Duration::seconds(7201);
        let decision_id = stage_run_approval_for(INSTANCE_B, &registry, RUN_B, registered_at).await;
        let docs = documents(&memory_beta, Some(SESSION), RUN_B);
        write_document(
            docs.parked(),
            &checkpoint_document(RUN_B, retention_expires_at("2126-09-01T00:00:00Z")),
        )
        .await;

        // Alpha sweeps a different memory dir on a shorter TTL.
        let memory_alpha = dir
            .path()
            .join("memory-alpha")
            .to_string_lossy()
            .to_string();
        let sweep_alpha = sweep_as(
            INSTANCE_A,
            "alpha",
            registry.clone(),
            store.clone(),
            &memory_alpha,
            3600,
        );
        sweep_alpha.run_pass().await;

        assert!(
            docs.parked().exists(),
            "beta's checkpoint is untouched by alpha's sweep"
        );
        assert!(
            registry.try_parked(&decision_id).await.unwrap().is_some(),
            "beta's approvals survive alpha's shorter-TTL sweep"
        );

        // Beta's own sweep keeps its unexpired run intact as well.
        let sweep_beta = sweep_as(
            INSTANCE_B,
            "beta",
            registry.clone(),
            store.clone(),
            &memory_beta,
            3600,
        );
        sweep_beta.run_pass().await;

        assert!(
            docs.parked().exists(),
            "beta's checkpoint is kept by its own sweep"
        );
        assert!(
            registry.try_parked(&decision_id).await.unwrap().is_some(),
            "beta's approvals are kept by its own sweep"
        );
    }

    /// The flip side of instance scoping: it must not strand an agent's own
    /// dead evidence in a shared store. Beta's aged orphans (no checkpoint
    /// anywhere) are invisible to alpha's sweep and still reaped by beta's.
    #[tokio::test]
    async fn pass_sweeps_own_aged_orphans_only() {
        let dir = tempfile::tempdir().expect("temp memory root");
        let (registry, store) = make_registry(dir.path());

        let memory_beta = dir.path().join("memory-beta").to_string_lossy().to_string();
        let registered_at = chrono::Utc::now() - chrono::Duration::seconds(7201);
        let decision_id = stage_run_approval_for(INSTANCE_B, &registry, RUN_B, registered_at).await;

        let memory_alpha = dir
            .path()
            .join("memory-alpha")
            .to_string_lossy()
            .to_string();
        let sweep_alpha = sweep_as(
            INSTANCE_A,
            "alpha",
            registry.clone(),
            store.clone(),
            &memory_alpha,
            3600,
        );
        sweep_alpha.run_pass().await;

        assert!(
            registry.try_parked(&decision_id).await.unwrap().is_some(),
            "alpha's sweep ignores beta's aged orphan rows"
        );

        let sweep_beta = sweep_as(
            INSTANCE_B,
            "beta",
            registry.clone(),
            store.clone(),
            &memory_beta,
            3600,
        );
        sweep_beta.run_pass().await;

        assert!(
            registry.try_parked(&decision_id).await.unwrap().is_none(),
            "beta's own sweep reaps its aged orphan rows"
        );
    }
}

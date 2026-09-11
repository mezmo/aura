//! Backend-agnostic conformance battery for the HITL approval store
//! ([`ApprovalStore`]): the behaviors every configured backend must share,
//! factored out of the Redis integration tests so the Docker-free backends
//! (file, memory) pin the same contract without a live Redis.
//!
//! "Instance A" and "instance B" model two server processes sharing one
//! store. For networked backends they are separate connections to the same
//! server; for single-process backends they are two handles to the same
//! store, which is that backend's deployment shape.
//!
//! Each test binary includes a subset of this module, so unused items here
//! are normal; `dead_code` is allowed for that reason.

#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::process::{Child, Command};

use aura::hitl::{
    AgentScope, ApprovalDecision, ApprovalItem, ApprovalOrigin, ApprovalRequest, DecisionId,
    PROTOCOL_VERSION, ParkedApproval, ResolveError, ResolvedDecision,
};
use aura::session_store::{ApprovalStore, ParkedApprovalRecord};

/// A representative parked approval, expiring in `ttl`.
pub fn make_parked(request_id: &str, ttl: Duration) -> ParkedApproval {
    let now = chrono::Utc::now();
    ParkedApproval {
        request: ApprovalRequest {
            version: PROTOCOL_VERSION,
            instance_id: "test-instance".to_string(),
            decision_id: DecisionId::generate(),
            request_id: request_id.to_string(),
            scope: AgentScope::Single { session_id: None },
            origin: ApprovalOrigin::ConfigGate {
                matched_pattern: "kubectl_*".to_string(),
                agent_name: "test-agent".to_string(),
            },
            items: vec![ApprovalItem {
                tool_name: "kubectl_delete".to_string(),
                arguments: serde_json::json!({"pod": "web-1"}),
                tool_call_intent: Some("restarting to pick up the config change".to_string()),
            }],
        },
        registered_at: now,
        expires_at: now + chrono::Duration::from_std(ttl).unwrap(),
        egress_headers: None,
    }
}

/// A ticket registered through one instance is readable, unchanged, through
/// the other.
pub async fn register_get_roundtrip(
    instance_a: &Arc<dyn ApprovalStore>,
    instance_b: &Arc<dyn ApprovalStore>,
) {
    let parked = make_parked("req-1", Duration::from_secs(60));
    let id = parked.request.decision_id;
    let expected = ParkedApprovalRecord::from(&parked);
    instance_a.register(parked).await.unwrap();

    let restored = instance_b
        .get(&id)
        .await
        .unwrap()
        .expect("instance B sees approval");
    assert_eq!(ParkedApprovalRecord::from(&restored), expected);
}

/// The first resolve wins; a second resolve of the same id is `NotFound`.
pub async fn resolve_is_at_most_once(
    instance_a: &Arc<dyn ApprovalStore>,
    instance_b: &Arc<dyn ApprovalStore>,
) {
    let parked = make_parked("req-2", Duration::from_secs(60));
    let id = parked.request.decision_id;
    instance_a.register(parked).await.unwrap();

    instance_b
        .resolve(&id, ApprovalDecision::Approved.into())
        .await
        .expect("first resolve wins");
    assert_eq!(
        instance_a
            .resolve(&id, ApprovalDecision::Approved.into())
            .await,
        Err(ResolveError::NotFound)
    );
}

/// Concurrent resolves of one id admit exactly one winner.
pub async fn concurrent_resolves_have_exactly_one_winner(
    instance_a: &Arc<dyn ApprovalStore>,
    instance_b: &Arc<dyn ApprovalStore>,
) {
    let parked = make_parked("req-3", Duration::from_secs(60));
    let id = parked.request.decision_id;
    instance_a.register(parked).await.unwrap();

    let (a, b) = tokio::join!(
        instance_a.resolve(&id, ApprovalDecision::Approved.into()),
        instance_b.resolve(&id, ApprovalDecision::Approved.into()),
    );
    let winners = usize::from(a.is_ok()) + usize::from(b.is_ok());
    assert_eq!(winners, 1, "exactly one resolver must win: {a:?} / {b:?}");
}

/// A resolution leaves a durable decision record readable from any instance
/// (issue #474), surviving the rejected second resolve.
pub async fn resolve_records_readable_decision(
    instance_a: &Arc<dyn ApprovalStore>,
    instance_b: &Arc<dyn ApprovalStore>,
) {
    let parked = make_parked("req-durable", Duration::from_secs(60));
    let id = parked.request.decision_id;
    instance_a.register(parked).await.unwrap();

    let denied = ApprovalDecision::Denied {
        reason: Some("not now".to_string()),
    };
    instance_b
        .resolve(&id, denied.clone().into())
        .await
        .unwrap();

    assert_eq!(
        instance_a.decision(&id).await.unwrap(),
        Some(ResolvedDecision::from(denied.clone()))
    );
    assert_eq!(
        instance_a
            .resolve(&id, ApprovalDecision::Approved.into())
            .await,
        Err(ResolveError::NotFound)
    );
    assert_eq!(
        instance_a.decision(&id).await.unwrap(),
        Some(ResolvedDecision::from(denied))
    );
    assert_eq!(
        instance_a.decision(&DecisionId::generate()).await.unwrap(),
        None
    );
}

/// Identity captured at resolve time persists in the SAME decision record:
/// the read-back carries the decision AND the identity together, from any
/// instance.
pub async fn resolve_records_identity_with_the_decision(
    instance_a: &Arc<dyn ApprovalStore>,
    instance_b: &Arc<dyn ApprovalStore>,
) {
    let parked = make_parked("req-identity", Duration::from_secs(60));
    let id = parked.request.decision_id;
    instance_a.register(parked).await.unwrap();

    let identity =
        aura::hitl::ResolvedDecision::approved(Some(unidentity(&[("x-forwarded-user", "alice")])));
    instance_b.resolve(&id, identity).await.unwrap();

    match instance_a.decision(&id).await.unwrap().expect("recorded") {
        aura::hitl::ResolvedDecision::Approved {
            identity: Some(got),
        } => {
            assert_eq!(
                got.captured_names().collect::<Vec<_>>(),
                ["x-forwarded-user"],
                "the identity reads back with the decision, from the other instance",
            );
        }
        other => panic!("expected Approved with identity, got {other:?}"),
    }
}

fn unidentity(pairs: &[(&str, &str)]) -> aura::approver_headers::ApproverHeaders {
    // The carrier's constructor is crate-private; build through the record's
    // storage projection instead, the path any resolver's identity takes.
    let map: std::collections::BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    let record_json = serde_json::json!({
        "approved": true,
        "reason": null,
        "decided_at": chrono::Utc::now(),
        "identity": map,
    });
    let record: aura::session_store::DecisionRecord =
        serde_json::from_value(record_json).expect("a decision record with identity");
    match aura::hitl::ResolvedDecision::try_from(record).expect("the record restores") {
        aura::hitl::ResolvedDecision::Approved { identity } => {
            identity.expect("the restored approval carries the identity")
        }
        other => panic!("expected an approval, got {other:?}"),
    }
}

/// A removed ticket no longer resolves.
pub async fn remove_makes_resolve_not_found(instance: &Arc<dyn ApprovalStore>) {
    let parked = make_parked("req-4", Duration::from_secs(60));
    let id = parked.request.decision_id;
    instance.register(parked).await.unwrap();

    instance.remove(&id).await.unwrap();

    assert_eq!(
        instance
            .resolve(&id, ApprovalDecision::Approved.into())
            .await,
        Err(ResolveError::NotFound)
    );
}

/// Cancelling by owner (request) id removes only that owner's tickets and
/// returns exactly the cleared set.
pub async fn cancel_request_removes_only_matching(instance: &Arc<dyn ApprovalStore>) {
    let cancel = make_parked("req-cancel", Duration::from_secs(60));
    let keep = make_parked("req-keep", Duration::from_secs(60));
    let cancel_id = cancel.request.decision_id;
    let cleared_record = ParkedApprovalRecord::from(&cancel);
    let keep_id = keep.request.decision_id;
    instance.register(cancel).await.unwrap();
    instance.register(keep).await.unwrap();

    let cleared = instance.cancel_request("req-cancel").await.unwrap();

    assert_eq!(cleared.len(), 1, "exactly the matching ticket is cleared");
    assert_eq!(
        ParkedApprovalRecord::from(&cleared[0]),
        cleared_record,
        "the cleared record is returned unchanged"
    );
    assert_eq!(cleared[0].request.decision_id, cancel_id);
    assert!(instance.get(&cancel_id).await.unwrap().is_none());
    assert!(instance.get(&keep_id).await.unwrap().is_some());
}

/// The poll reconciler's scan source: `list_pending` returns exactly the
/// parked, undecided tickets — a resolved sibling is never listed.
pub async fn list_pending_returns_only_live_undecided(
    instance_a: &Arc<dyn ApprovalStore>,
    instance_b: &Arc<dyn ApprovalStore>,
) {
    let resolved = make_parked("req-poll-resolved", Duration::from_secs(60));
    let resolved_id = resolved.request.decision_id;
    let live = make_parked("req-poll-live", Duration::from_secs(60));
    let live_id = live.request.decision_id;
    instance_a.register(resolved).await.unwrap();
    instance_a.register(live).await.unwrap();
    instance_b
        .resolve(&resolved_id, ApprovalDecision::Approved.into())
        .await
        .unwrap();

    let pending = instance_a.list_pending().await.unwrap();

    let ids: Vec<DecisionId> = pending.iter().map(|p| p.request.decision_id).collect();
    assert_eq!(ids, [live_id], "exactly the undecided ticket is listed");
}

pub async fn list_pending_empty_store_returns_empty(instance: &Arc<dyn ApprovalStore>) {
    assert!(instance.list_pending().await.unwrap().is_empty());
}

/// Expired tickets are never listed, even where the backend retains them
/// (the file store keeps them until remove; Redis floors the record TTL).
pub async fn list_pending_excludes_expired(
    instance_a: &Arc<dyn ApprovalStore>,
    instance_b: &Arc<dyn ApprovalStore>,
) {
    let mut expired = make_parked("req-poll-expired", Duration::from_secs(60));
    expired.expires_at = chrono::Utc::now() - chrono::Duration::seconds(1);
    let live = make_parked("req-poll-live", Duration::from_secs(60));
    let live_id = live.request.decision_id;
    instance_a.register(expired).await.unwrap();
    instance_a.register(live).await.unwrap();

    let pending = instance_b.list_pending().await.unwrap();

    let ids: Vec<DecisionId> = pending.iter().map(|p| p.request.decision_id).collect();
    assert_eq!(ids, [live_id], "the expired ticket must not be listed");
}

// ---------------------------------------------------------------------------
// A spawned aura-web-server for integration suites
// ---------------------------------------------------------------------------

/// How long a spawned server has to answer `/health` before the spawn
/// retries once on a fresh port.
pub const HEALTH_TIMEOUT: Duration = Duration::from_secs(30);

/// A freshly spawned `aura-web-server`, bound to its own port and reading a
/// config generated for exactly one test case. Killed and its config file
/// removed on drop.
pub struct AuraServer {
    port: u16,
    child: Child,
    config_path: PathBuf,
    /// Accumulated stderr, drained continuously so the child's pipe never
    /// blocks; read back to explain a health-check timeout.
    stderr_log: Arc<Mutex<String>>,
}

impl AuraServer {
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Spawn `aura-web-server` against `config_toml` (config file named
    /// `{config_prefix}{uuid}.toml`) with `extra_env` applied to the child,
    /// waiting until it answers `/health`. `free_port`'s bind-then-drop
    /// leaves a window for another process to grab the port; one retry on a
    /// fresh port covers that.
    pub async fn start(
        config_toml: &str,
        config_prefix: &str,
        extra_env: &[(&str, String)],
    ) -> Self {
        match Self::try_start(config_toml, config_prefix, extra_env).await {
            Ok(server) => server,
            Err(failed) => {
                let log = failed.stderr_log.lock().expect("stderr log mutex").clone();
                eprintln!(
                    "aura-web-server on port {} never answered /health within {HEALTH_TIMEOUT:?}; \
                     retrying once on a fresh port. stderr:\n{log}",
                    failed.port
                );
                failed.stop().await;
                match Self::try_start(config_toml, config_prefix, extra_env).await {
                    Ok(server) => server,
                    Err(failed) => {
                        let log = failed.stderr_log.lock().expect("stderr log mutex").clone();
                        let port = failed.port;
                        failed.stop().await;
                        panic!(
                            "aura-web-server never answered /health, on a fresh port either; \
                             last tried port {port}; stderr:\n{log}"
                        );
                    }
                }
            }
        }
    }

    /// One spawn-and-wait attempt. `Err` carries the (still-running) server
    /// so the caller can log its stderr and stop it before retrying.
    async fn try_start(
        config_toml: &str,
        config_prefix: &str,
        extra_env: &[(&str, String)],
    ) -> Result<Self, Self> {
        let port = free_port();
        let config_path =
            std::env::temp_dir().join(format!("{config_prefix}{}.toml", uuid::Uuid::new_v4()));
        std::fs::write(&config_path, config_toml).expect("write generated test config");

        let mut child = Command::new(env!("CARGO_BIN_EXE_aura-web-server"))
            .env("CONFIG_PATH", &config_path)
            .env("HOST", "127.0.0.1")
            .env("PORT", port.to_string())
            .env("RUST_LOG", "warn")
            .envs(extra_env.iter().map(|(k, v)| (*k, v.clone())))
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn aura-web-server (did `cargo build -p aura-web-server` succeed?)");

        let stderr_log = Arc::new(Mutex::new(String::new()));
        let stderr = child.stderr.take().expect("stderr was piped");
        let log_sink = Arc::clone(&stderr_log);
        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                use tokio::io::AsyncBufReadExt;
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    break;
                }
                let mut log = log_sink.lock().expect("stderr log mutex");
                log.push_str(line.trim_end_matches('\n'));
                log.push('\n');
            }
        });

        let server = Self {
            port,
            child,
            config_path,
            stderr_log,
        };
        if server.is_healthy_within(HEALTH_TIMEOUT).await {
            Ok(server)
        } else {
            Err(server)
        }
    }

    async fn is_healthy_within(&self, timeout: Duration) -> bool {
        let client = reqwest::Client::new();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Ok(resp) = client
                .get(format!("{}/health", self.base_url()))
                .send()
                .await
                && resp.status().is_success()
            {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Kill the child and await its exit, reaping the process, then remove its generated config file. Call this explicitly at test end; `Drop`'s `start_kill` is only the fallback for a test that panics.
    pub async fn stop(mut self) {
        let _ = self.child.kill().await;
        let _ = std::fs::remove_file(&self.config_path);
    }
}

impl Drop for AuraServer {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        let _ = std::fs::remove_file(&self.config_path);
    }
}

/// An OS-assigned free port, read and released before the caller uses it.
/// The bind-then-drop race is the standard tolerance for test-local ports.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

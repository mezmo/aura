//! Backend-agnostic conformance battery for the HITL approval store
//! ([`ApprovalStore`]): the behaviors every configured backend must share,
//! factored out of the Redis integration tests so the Docker-free backends
//! (file, memory) pin the same contract without a live Redis.
//!
//! "Instance A" and "instance B" model two server processes sharing one
//! store. For networked backends they are separate connections to the same
//! server; for single-process backends they are two handles to the same
//! store, which is that backend's deployment shape.

use std::sync::Arc;
use std::time::Duration;

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

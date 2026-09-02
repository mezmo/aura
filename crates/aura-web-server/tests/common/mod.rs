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
    PROTOCOL_VERSION, ParkedApproval, ResolveError,
};
use aura::session_store::{ApprovalStore, ParkedApprovalRecord};

/// A representative parked approval, expiring in `ttl`.
pub fn make_parked(request_id: &str, ttl: Duration) -> ParkedApproval {
    let now = chrono::Utc::now();
    ParkedApproval {
        request: ApprovalRequest {
            version: PROTOCOL_VERSION,
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
        .resolve(&id, ApprovalDecision::Approved)
        .await
        .expect("first resolve wins");
    assert_eq!(
        instance_a.resolve(&id, ApprovalDecision::Approved).await,
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
        instance_a.resolve(&id, ApprovalDecision::Approved),
        instance_b.resolve(&id, ApprovalDecision::Approved),
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
    instance_b.resolve(&id, denied.clone()).await.unwrap();

    assert_eq!(
        instance_a.decision(&id).await.unwrap(),
        Some(denied.clone())
    );
    assert_eq!(
        instance_a.resolve(&id, ApprovalDecision::Approved).await,
        Err(ResolveError::NotFound)
    );
    assert_eq!(instance_a.decision(&id).await.unwrap(), Some(denied));
    assert_eq!(
        instance_a.decision(&DecisionId::generate()).await.unwrap(),
        None
    );
}

/// A removed ticket no longer resolves.
pub async fn remove_makes_resolve_not_found(instance: &Arc<dyn ApprovalStore>) {
    let parked = make_parked("req-4", Duration::from_secs(60));
    let id = parked.request.decision_id;
    instance.register(parked).await.unwrap();

    instance.remove(&id).await.unwrap();

    assert_eq!(
        instance.resolve(&id, ApprovalDecision::Approved).await,
        Err(ResolveError::NotFound)
    );
}

/// Cancelling by owner (request) id removes only that owner's tickets.
pub async fn cancel_request_removes_only_matching(instance: &Arc<dyn ApprovalStore>) {
    let cancel = make_parked("req-cancel", Duration::from_secs(60));
    let keep = make_parked("req-keep", Duration::from_secs(60));
    let cancel_id = cancel.request.decision_id;
    let keep_id = keep.request.decision_id;
    instance.register(cancel).await.unwrap();
    instance.register(keep).await.unwrap();

    instance.cancel_request("req-cancel").await.unwrap();

    assert!(instance.get(&cancel_id).await.unwrap().is_none());
    assert!(instance.get(&keep_id).await.unwrap().is_some());
}

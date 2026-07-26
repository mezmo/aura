//! The behavioral contract every session-store backend must satisfy, written
//! once and parameterized by the backend under test.
//!
//! [`ApprovalStore`] and [`EventBus`] are the traits a deployment swaps, so
//! their guarantees are a shared contract rather than a per-backend detail:
//! the in-memory, file, and Redis implementations each run this battery. A
//! backend that passes here is substitutable; one that does not is not.
//!
//! Every row runs against a single store instance using freshly generated ids
//! and topics, so a battery neither collides with itself nor needs a
//! backend-specific reset hook. Failures are collected across all rows, so one
//! broken guarantee does not hide the rest.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use bytes::Bytes;
use futures::StreamExt;
use uuid::Uuid;

use crate::config::SessionId;
use crate::hitl::{
    AgentScope, ApprovalDecision, ApprovalItem, ApprovalOrigin, ApprovalRequest, DecisionId,
    PROTOCOL_VERSION, ParkedApproval, ResolveError,
};
use crate::orchestration::TaskIdentity;

use super::{ApprovalStore, EventBus, ParkedApprovalRecord, Subscription};

/// How long a row waits for a published payload before calling it lost.
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);
/// Concurrent resolvers raced in the at-most-once row.
const RESOLVER_COUNT: usize = 8;
/// How many times that race is rerun.
const RACE_ROUNDS: usize = 32;
/// Approvals parked under the one request the cancellation row cancels.
const CANCELLED_APPROVALS: usize = 3;

/// Assert `store` satisfies the [`ApprovalStore`] contract, panicking with
/// every failing row.
pub async fn assert_approval_store_conformance(store: Arc<dyn ApprovalStore>) {
    let mut failures = Vec::new();
    record(
        &mut failures,
        "register_then_get_returns_the_record",
        register_then_get_returns_the_record(store.as_ref()).await,
    );
    record(
        &mut failures,
        "get_of_an_unknown_id_is_none",
        get_of_an_unknown_id_is_none(store.as_ref()).await,
    );
    record(
        &mut failures,
        "every_scope_survives_storage",
        every_scope_survives_storage(store.as_ref()).await,
    );
    record(
        &mut failures,
        "resolve_removes_the_entry",
        resolve_removes_the_entry(store.as_ref()).await,
    );
    record(
        &mut failures,
        "resolve_of_an_unknown_id_is_not_found",
        resolve_of_an_unknown_id_is_not_found(store.as_ref()).await,
    );
    record(
        &mut failures,
        "concurrent_resolve_has_exactly_one_winner",
        concurrent_resolve_has_exactly_one_winner(&store).await,
    );
    record(
        &mut failures,
        "remove_is_idempotent",
        remove_is_idempotent(store.as_ref()).await,
    );
    record(
        &mut failures,
        "cancel_request_removes_only_its_own",
        cancel_request_removes_only_its_own(store.as_ref()).await,
    );
    record(
        &mut failures,
        "cancel_of_an_unknown_request_is_ok",
        cancel_of_an_unknown_request_is_ok(store.as_ref()).await,
    );
    assert_all_passed("ApprovalStore", &failures);
}

/// Assert `bus` satisfies the [`EventBus`] contract, panicking with every
/// failing row.
pub async fn assert_event_bus_conformance(bus: Arc<dyn EventBus>) {
    let mut failures = Vec::new();
    record(
        &mut failures,
        "publish_reaches_a_subscriber",
        publish_reaches_a_subscriber(bus.as_ref()).await,
    );
    record(
        &mut failures,
        "publish_fans_out_to_every_subscriber",
        publish_fans_out_to_every_subscriber(bus.as_ref()).await,
    );
    record(
        &mut failures,
        "publish_without_subscribers_is_ok",
        publish_without_subscribers_is_ok(bus.as_ref()).await,
    );
    record(
        &mut failures,
        "topics_are_isolated",
        topics_are_isolated(bus.as_ref()).await,
    );
    assert_all_passed("EventBus", &failures);
}

/// Keep a row's name with its failure, so the report names every contract that
/// broke rather than only the first.
fn record(failures: &mut Vec<String>, name: &str, outcome: Result<()>) {
    if let Err(err) = outcome {
        failures.push(format!("  [{name}] {err:#}"));
    }
}

fn assert_all_passed(trait_name: &str, failures: &[String]) {
    assert!(
        failures.is_empty(),
        "{trait_name} conformance failed on {} row(s):\n{}",
        failures.len(),
        failures.join("\n"),
    );
}

// --- ApprovalStore rows ---

async fn register_then_get_returns_the_record(store: &dyn ApprovalStore) -> Result<()> {
    let entry = parked_single(&unique("register-get"));
    let id = entry.request.decision_id;
    let expected = ParkedApprovalRecord::from(&entry);

    store.register(entry).await?;
    let stored = store
        .get(&id)
        .await?
        .ok_or_else(|| anyhow!("a registered approval must be readable"))?;

    ensure!(
        ParkedApprovalRecord::from(&stored) == expected,
        "the stored approval differs from the registered one",
    );
    Ok(())
}

async fn get_of_an_unknown_id_is_none(store: &dyn ApprovalStore) -> Result<()> {
    let missing = store.get(&DecisionId::generate()).await?;
    ensure!(missing.is_none(), "an unregistered id must read as absent");
    Ok(())
}

/// Worker and coordinator scopes carry ids the store must not flatten; a
/// backend that loses them makes a parked orchestration approval unroutable.
async fn every_scope_survives_storage(store: &dyn ApprovalStore) -> Result<()> {
    let run_id = "0191e8c0-1111-7000-8000-000000000000"
        .parse()
        .map_err(|e| anyhow!("test run id parses: {e}"))?;
    let scopes = [
        AgentScope::Single {
            session_id: Some(SessionId::new("sess-conformance")),
        },
        AgentScope::Worker {
            run_id,
            task: TaskIdentity::new(3, Some("ops".to_string())),
            session_id: None,
        },
        AgentScope::Coordinator { run_id },
    ];

    for scope in scopes {
        let entry = parked(&unique("scope"), scope);
        let id = entry.request.decision_id;
        let expected = ParkedApprovalRecord::from(&entry);
        store.register(entry).await?;
        let stored = store
            .get(&id)
            .await?
            .ok_or_else(|| anyhow!("a registered approval must be readable"))?;
        ensure!(
            ParkedApprovalRecord::from(&stored) == expected,
            "scope {:?} did not survive storage",
            expected.scope,
        );
    }
    Ok(())
}

async fn resolve_removes_the_entry(store: &dyn ApprovalStore) -> Result<()> {
    let entry = parked_single(&unique("resolve"));
    let id = entry.request.decision_id;
    store.register(entry).await?;

    resolve(store, &id).await?;
    ensure!(
        store.get(&id).await?.is_none(),
        "a resolved approval must no longer be readable",
    );
    Ok(())
}

async fn resolve_of_an_unknown_id_is_not_found(store: &dyn ApprovalStore) -> Result<()> {
    let outcome = store
        .resolve(&DecisionId::generate(), ApprovalDecision::Approved)
        .await;
    ensure!(
        outcome == Err(ResolveError::NotFound),
        "resolving an unknown id must report NotFound, got {outcome:?}",
    );
    Ok(())
}

/// At-most-once consumption is the store's job: whichever caller wins, no
/// second caller may also see success and run the gated call twice.
///
/// A non-atomic take only misbehaves when the calls overlap in time, and how
/// wide that window gets depends on the caller's runtime, so the race is
/// released from a barrier and rerun enough times that a broken backend cannot
/// pass by luck.
async fn concurrent_resolve_has_exactly_one_winner(store: &Arc<dyn ApprovalStore>) -> Result<()> {
    for round in 0..RACE_ROUNDS {
        let entry = parked_single(&unique("one-winner"));
        let id = entry.request.decision_id;
        store.register(entry).await?;

        let start = Arc::new(tokio::sync::Barrier::new(RESOLVER_COUNT));
        let resolvers: Vec<_> = (0..RESOLVER_COUNT)
            .map(|_| {
                let store = Arc::clone(store);
                let start = Arc::clone(&start);
                tokio::spawn(async move {
                    start.wait().await;
                    store.resolve(&id, ApprovalDecision::Approved).await
                })
            })
            .collect();

        let mut winners = 0;
        for resolver in resolvers {
            match resolver
                .await
                .map_err(|e| anyhow!("a resolver task did not complete: {e}"))?
            {
                Ok(()) => winners += 1,
                Err(ResolveError::NotFound) => {}
                Err(err) => bail!("a losing resolve must report NotFound, got {err:?}"),
            }
        }
        ensure!(
            winners == 1,
            "round {round}: exactly one of {RESOLVER_COUNT} concurrent resolves must succeed, \
             {winners} did",
        );
    }
    Ok(())
}

async fn remove_is_idempotent(store: &dyn ApprovalStore) -> Result<()> {
    let entry = parked_single(&unique("remove"));
    let id = entry.request.decision_id;
    store.register(entry).await?;

    store.remove(&id).await?;
    ensure!(
        store.get(&id).await?.is_none(),
        "a removed approval must no longer be readable",
    );
    store.remove(&id).await?;
    Ok(())
}

/// One request can hold several approvals, so cancelling it has to sweep all
/// of them. A backend that stops at the first match must fail this row.
///
/// The survivor is resolved after the read. An index-backed backend can leave
/// an entry readable while it is already unconsumable.
async fn cancel_request_removes_only_its_own(store: &dyn ApprovalStore) -> Result<()> {
    let doomed_request = unique("cancel");
    let kept_request = unique("kept");
    let mut doomed_ids = Vec::with_capacity(CANCELLED_APPROVALS);
    for _ in 0..CANCELLED_APPROVALS {
        let doomed = parked_single(&doomed_request);
        doomed_ids.push(doomed.request.decision_id);
        store.register(doomed).await?;
    }
    let kept = parked_single(&kept_request);
    let kept_id = kept.request.decision_id;
    store.register(kept).await?;

    store.cancel_request(&doomed_request).await?;

    for id in &doomed_ids {
        ensure!(
            store.get(id).await?.is_none(),
            "cancelling a request must remove every one of its {CANCELLED_APPROVALS} approvals; \
             {id} survived",
        );
    }
    ensure!(
        store.get(&kept_id).await?.is_some(),
        "cancelling a request must leave other requests' approvals alone",
    );
    resolve(store, &kept_id)
        .await
        .context("an approval left by cancel_request must still be resolvable")?;
    Ok(())
}

async fn cancel_of_an_unknown_request_is_ok(store: &dyn ApprovalStore) -> Result<()> {
    store.cancel_request(&unique("never-registered")).await?;
    Ok(())
}

// --- EventBus rows ---

async fn publish_reaches_a_subscriber(bus: &dyn EventBus) -> Result<()> {
    let topic = unique("delivery");
    let mut subscriber = bus.subscribe(&topic).await?;

    bus.publish(&topic, Bytes::from_static(b"hello")).await?;

    let payload = next_payload(&mut subscriber).await?;
    ensure!(
        payload == Bytes::from_static(b"hello"),
        "the delivered payload must be the published one, got {payload:?}",
    );
    Ok(())
}

async fn publish_fans_out_to_every_subscriber(bus: &dyn EventBus) -> Result<()> {
    let topic = unique("fan-out");
    let mut first = bus.subscribe(&topic).await?;
    let mut second = bus.subscribe(&topic).await?;

    bus.publish(&topic, Bytes::from_static(b"payload")).await?;

    for subscriber in [&mut first, &mut second] {
        let payload = next_payload(subscriber).await?;
        ensure!(
            payload == Bytes::from_static(b"payload"),
            "every subscriber must receive the payload, one got {payload:?}",
        );
    }
    Ok(())
}

async fn publish_without_subscribers_is_ok(bus: &dyn EventBus) -> Result<()> {
    bus.publish(&unique("nobody-home"), Bytes::from_static(b"x"))
        .await?;
    Ok(())
}

async fn topics_are_isolated(bus: &dyn EventBus) -> Result<()> {
    let first_topic = unique("isolated-a");
    let second_topic = unique("isolated-b");
    let mut first = bus.subscribe(&first_topic).await?;
    let mut second = bus.subscribe(&second_topic).await?;

    bus.publish(&first_topic, Bytes::from_static(b"for-a"))
        .await?;
    bus.publish(&second_topic, Bytes::from_static(b"for-b"))
        .await?;

    ensure!(
        next_payload(&mut first).await? == Bytes::from_static(b"for-a"),
        "a subscriber must only receive its own topic's payloads",
    );
    ensure!(
        next_payload(&mut second).await? == Bytes::from_static(b"for-b"),
        "a subscriber must only receive its own topic's payloads",
    );
    Ok(())
}

// --- Row fixtures ---

/// A name unique to this battery run, so rows are independent even when the
/// backend keeps everything ever written to it.
fn unique(label: &str) -> String {
    format!("conformance-{label}-{}", Uuid::now_v7())
}

fn parked_single(request_id: &str) -> ParkedApproval {
    parked(request_id, AgentScope::Single { session_id: None })
}

fn parked(request_id: &str, scope: AgentScope) -> ParkedApproval {
    let now = chrono::Utc::now();
    ParkedApproval {
        request: ApprovalRequest {
            version: PROTOCOL_VERSION,
            decision_id: DecisionId::generate(),
            request_id: request_id.to_string(),
            scope,
            origin: ApprovalOrigin::ConfigGate {
                matched_pattern: "conformance_*".to_string(),
            },
            items: vec![ApprovalItem {
                tool_name: "conformance_tool".to_string(),
                arguments: serde_json::json!({"nested": {"b": 2, "a": [1, "two"]}}),
            }],
        },
        registered_at: now,
        expires_at: now + chrono::Duration::seconds(300),
    }
}

/// [`ResolveError`] carries no `Display`, so rows surface it through `Debug`.
async fn resolve(store: &dyn ApprovalStore, id: &DecisionId) -> Result<()> {
    store
        .resolve(id, ApprovalDecision::Approved)
        .await
        .map_err(|err| anyhow!("resolve failed: {err:?}"))
}

async fn next_payload(subscriber: &mut Subscription) -> Result<Bytes> {
    tokio::time::timeout(DELIVERY_TIMEOUT, subscriber.next())
        .await
        .map_err(|_| anyhow!("no payload within {DELIVERY_TIMEOUT:?}"))?
        .ok_or_else(|| anyhow!("the subscription ended before a payload arrived"))
}

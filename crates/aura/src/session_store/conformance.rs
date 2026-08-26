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
use crate::orchestration::park::{
    AgentInstanceId, CasError, ChatSessionId, CheckpointEnvelope, FencingGeneration, LeaseTtl,
    NonEmpty, ParkCommit, ParkReason, RunCheckpoint, RunEvent, RunState, Session,
    SessionId as ParkSessionId, SessionRecord, WakeReason,
};

use super::{ApprovalStore, EventBus, ParkedApprovalRecord, RunStore, RunStoreError, Subscription};

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
        "resolve_durable_records_a_readable_decision",
        resolve_durable_records_a_readable_decision(store.as_ref()).await,
    );
    record(
        &mut failures,
        "resolve_durable_keeps_the_parked_entry",
        resolve_durable_keeps_the_parked_entry(store.as_ref()).await,
    );
    record(
        &mut failures,
        "concurrent_resolve_durable_agree_on_one_record",
        concurrent_resolve_durable_agree_on_one_record(&store).await,
    );
    record(
        &mut failures,
        "conflicting_resolve_durable_keeps_the_first",
        conflicting_resolve_durable_keeps_the_first(store.as_ref()).await,
    );
    record(
        &mut failures,
        "remove_discards_the_recorded_decision",
        remove_discards_the_recorded_decision(store.as_ref()).await,
    );
    record(
        &mut failures,
        "cancel_request_discards_the_recorded_decision",
        cancel_request_discards_the_recorded_decision(store.as_ref()).await,
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

/// Assert `store` satisfies the [`RunStore`] contract, panicking with every
/// failing row.
pub async fn assert_run_store_conformance(store: Arc<dyn RunStore>) {
    let mut failures = Vec::new();
    record(
        &mut failures,
        "create_then_load_returns_record",
        create_then_load_returns_record(store.as_ref()).await,
    );
    record(
        &mut failures,
        "load_of_unknown_session_is_none",
        load_of_unknown_session_is_none(store.as_ref()).await,
    );
    record(
        &mut failures,
        "create_of_existing_session_is_session_exists",
        create_of_existing_session_is_session_exists(store.as_ref()).await,
    );
    record(
        &mut failures,
        "create_requires_created_state",
        create_requires_created_state(store.as_ref()).await,
    );
    record(
        &mut failures,
        "acquire_lease_on_created_record_succeeds",
        acquire_lease_on_created_record_succeeds(store.as_ref()).await,
    );
    record(
        &mut failures,
        "acquire_lease_on_unknown_session_fails",
        acquire_lease_on_unknown_session_fails(store.as_ref()).await,
    );
    record(
        &mut failures,
        "acquire_lease_on_live_lease_fails",
        acquire_lease_on_live_lease_fails(store.as_ref()).await,
    );
    record(
        &mut failures,
        "heartbeat_lease_extends_lease",
        heartbeat_lease_extends_lease(store.as_ref()).await,
    );
    record(
        &mut failures,
        "heartbeat_lease_stale_generation_fails",
        heartbeat_lease_stale_generation_fails(store.as_ref()).await,
    );
    record(
        &mut failures,
        "release_lease_makes_record_unleased",
        release_lease_makes_record_unleased(store.as_ref()).await,
    );
    record(
        &mut failures,
        "apply_with_live_lease_advances_state",
        apply_with_live_lease_advances_state(store.as_ref()).await,
    );
    record(
        &mut failures,
        "apply_stale_generation_fails",
        apply_stale_generation_fails(store.as_ref()).await,
    );
    record(
        &mut failures,
        "apply_without_lease_fails",
        apply_without_lease_fails(store.as_ref()).await,
    );
    record(
        &mut failures,
        "park_commits_parked_state",
        park_commits_parked_state(store.as_ref()).await,
    );
    record(
        &mut failures,
        "park_stale_generation_fails",
        park_stale_generation_fails(store.as_ref()).await,
    );
    record(
        &mut failures,
        "park_outside_running_fails",
        park_outside_running_fails(store.as_ref()).await,
    );
    assert_all_passed("RunStore", &failures);
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

/// The decision has to outlive the resolver's own return value: a parking
/// instance that never saw the bus wake recovers the resolution from here.
async fn resolve_durable_records_a_readable_decision(store: &dyn ApprovalStore) -> Result<()> {
    let entry = parked_single(&unique("durable-readback"));
    let id = entry.request.decision_id;
    store.register(entry).await?;

    let denied = ApprovalDecision::Denied {
        reason: Some("unsafe".to_string()),
    };
    resolve_durable(store, &id, denied.clone()).await?;

    ensure!(
        store.decision(&id).await? == Some(denied),
        "a durably resolved approval must read back the decision it was resolved with",
    );
    Ok(())
}

/// Durable resolution is not consumption: the parked entry survives it, so a
/// digest-bound claim is what takes the approval, not whoever decided it.
async fn resolve_durable_keeps_the_parked_entry(store: &dyn ApprovalStore) -> Result<()> {
    let entry = parked_single(&unique("durable-repeat"));
    let id = entry.request.decision_id;
    store.register(entry).await?;

    let first = resolve_durable(store, &id, ApprovalDecision::Approved).await?;
    ensure!(
        store.get(&id).await?.is_some(),
        "a durably resolved approval must still be readable",
    );

    let second = resolve_durable(store, &id, ApprovalDecision::Approved).await?;
    ensure!(
        second == first,
        "a repeated resolution must return the recorded wake reason, got {second:?} after {first:?}",
    );
    ensure!(
        store.get(&id).await?.is_some(),
        "a repeated durable resolution must leave the parked entry in place",
    );
    Ok(())
}

/// Concurrent resolvers all win, but on one record: unlike the destructive
/// path there is nothing to hand out exactly once, and a store that let the
/// last writer through would hand different wake reasons to callers reading
/// the same approval.
async fn concurrent_resolve_durable_agree_on_one_record(
    store: &Arc<dyn ApprovalStore>,
) -> Result<()> {
    for round in 0..RACE_ROUNDS {
        let entry = parked_single(&unique("durable-race"));
        let id = entry.request.decision_id;
        store.register(entry).await?;

        let start = Arc::new(tokio::sync::Barrier::new(RESOLVER_COUNT));
        let resolvers: Vec<_> = (0..RESOLVER_COUNT)
            .map(|_| {
                let store = Arc::clone(store);
                let start = Arc::clone(&start);
                tokio::spawn(async move {
                    start.wait().await;
                    store.resolve_durable(&id, ApprovalDecision::Approved).await
                })
            })
            .collect();

        let mut wakes = Vec::with_capacity(RESOLVER_COUNT);
        for resolver in resolvers {
            let outcome = resolver
                .await
                .map_err(|e| anyhow!("a resolver task did not complete: {e}"))?;
            wakes.push(outcome.map_err(|err| {
                anyhow!(
                    "round {round}: every concurrent durable resolve must succeed, one got {err:?}"
                )
            })?);
        }
        ensure!(
            wakes.windows(2).all(|pair| pair[0] == pair[1]),
            "round {round}: {RESOLVER_COUNT} concurrent durable resolves must agree on one \
             recorded wake reason, got {wakes:?}",
        );
        ensure!(
            store.get(&id).await?.is_some(),
            "round {round}: concurrent durable resolves must leave the parked entry in place",
        );
    }
    Ok(())
}

/// Two resolvers can disagree — a webhook approval racing an operator's
/// denial. Letting the later one through would wake a run on a decision no
/// gate ever released.
async fn conflicting_resolve_durable_keeps_the_first(store: &dyn ApprovalStore) -> Result<()> {
    let entry = parked_single(&unique("durable-conflict"));
    let id = entry.request.decision_id;
    store.register(entry).await?;

    let first = resolve_durable(store, &id, ApprovalDecision::Approved).await?;
    let second = resolve_durable(
        store,
        &id,
        ApprovalDecision::Denied {
            reason: Some("too late".to_string()),
        },
    )
    .await?;

    ensure!(
        second == first,
        "the first decision recorded wins: a conflicting resolution must return the stored \
         wake reason, got {second:?} after {first:?}",
    );
    ensure!(
        store.decision(&id).await? == Some(ApprovalDecision::Approved),
        "the first decision recorded wins: the approval must survive a later denial",
    );
    Ok(())
}

async fn remove_discards_the_recorded_decision(store: &dyn ApprovalStore) -> Result<()> {
    let entry = parked_single(&unique("durable-remove"));
    let id = entry.request.decision_id;
    store.register(entry).await?;
    resolve_durable(store, &id, ApprovalDecision::Approved).await?;
    ensure!(
        store.decision(&id).await?.is_some(),
        "the decision must be readable before the removal, or this row proves nothing",
    );

    store.remove(&id).await?;
    ensure!(
        store.decision(&id).await?.is_none(),
        "removing an approval must discard the decision recorded for it",
    );
    Ok(())
}

async fn cancel_request_discards_the_recorded_decision(store: &dyn ApprovalStore) -> Result<()> {
    let request_id = unique("durable-cancel");
    let entry = parked_single(&request_id);
    let id = entry.request.decision_id;
    store.register(entry).await?;
    resolve_durable(store, &id, ApprovalDecision::Approved).await?;
    ensure!(
        store.decision(&id).await?.is_some(),
        "the decision must be readable before the cancellation, or this row proves nothing",
    );

    store.cancel_request(&request_id).await?;
    ensure!(
        store.decision(&id).await?.is_none(),
        "cancelling a request must discard the decisions recorded for its approvals",
    );
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

// --- RunStore rows ---

async fn create_then_load_returns_record(store: &dyn RunStore) -> Result<()> {
    let record = created_record();
    let session = record.session.id;
    store.create(record.clone()).await?;

    let loaded = store
        .load(session)
        .await?
        .ok_or_else(|| anyhow!("a created record must be loadable"))?;
    ensure!(
        loaded == record,
        "the loaded record differs from the created one"
    );
    Ok(())
}

async fn load_of_unknown_session_is_none(store: &dyn RunStore) -> Result<()> {
    let missing = store.load(ParkSessionId::generate()).await?;
    ensure!(
        missing.is_none(),
        "an uncreated session must load as absent"
    );
    Ok(())
}

async fn create_of_existing_session_is_session_exists(store: &dyn RunStore) -> Result<()> {
    let record = created_record();
    let session = record.session.id;
    store.create(record).await?;

    let duplicate = record_with_session(session);
    let outcome = store.create(duplicate).await;
    ensure!(
        outcome == Err(RunStoreError::SessionExists { session }),
        "creating an existing session must report SessionExists, got {outcome:?}"
    );
    Ok(())
}

async fn create_requires_created_state(store: &dyn RunStore) -> Result<()> {
    let mut record = created_record();
    record.state = RunState::Running;
    let outcome = store.create(record).await;
    ensure!(
        matches!(
            outcome,
            Err(RunStoreError::Cas(CasError::StateMismatch { .. }))
        ),
        "creating a non-Created record must report StateMismatch, got {outcome:?}"
    );
    Ok(())
}

async fn acquire_lease_on_created_record_succeeds(store: &dyn RunStore) -> Result<()> {
    let record = created_record();
    let session = record.session.id;
    store.create(record).await?;

    let lease = store
        .acquire_lease(session, AgentInstanceId::generate(), lease_ttl())
        .await?;
    ensure!(
        lease.generation > FencingGeneration::INITIAL,
        "acquire must advance the fencing generation"
    );

    let loaded = store.load(session).await?.unwrap();
    ensure!(
        loaded.generation == lease.generation,
        "the record generation must match the issued lease generation"
    );
    ensure!(
        loaded.lease.as_ref().map(|l| l.generation) == Some(lease.generation),
        "the stored lease generation must match the returned lease"
    );
    Ok(())
}

async fn acquire_lease_on_unknown_session_fails(store: &dyn RunStore) -> Result<()> {
    let outcome = store
        .acquire_lease(
            ParkSessionId::generate(),
            AgentInstanceId::generate(),
            lease_ttl(),
        )
        .await;
    ensure!(
        matches!(outcome, Err(RunStoreError::UnknownSession { .. })),
        "acquire on an unknown session must report UnknownSession, got {outcome:?}"
    );
    Ok(())
}

async fn acquire_lease_on_live_lease_fails(store: &dyn RunStore) -> Result<()> {
    let record = created_record();
    let session = record.session.id;
    store.create(record).await?;

    let first = store
        .acquire_lease(session, AgentInstanceId::generate(), lease_ttl())
        .await?;
    let second = store
        .acquire_lease(session, AgentInstanceId::generate(), lease_ttl())
        .await;
    ensure!(
        matches!(
            second,
            Err(RunStoreError::LeaseHeld {
                holder,
                ..
            }) if holder == first.holder
        ),
        "acquire on a live lease must report LeaseHeld, got {second:?}"
    );
    Ok(())
}

async fn heartbeat_lease_extends_lease(store: &dyn RunStore) -> Result<()> {
    let record = created_record();
    let session = record.session.id;
    store.create(record).await?;

    let lease = store
        .acquire_lease(session, AgentInstanceId::generate(), lease_ttl())
        .await?;
    tokio::time::sleep(Duration::from_millis(10)).await;
    let renewed = store
        .heartbeat_lease(session, lease.generation, lease_ttl())
        .await?;
    ensure!(
        renewed.heartbeat_at >= lease.heartbeat_at,
        "heartbeat must advance heartbeat_at"
    );
    ensure!(
        renewed.expires_at >= lease.expires_at,
        "heartbeat must not shorten the lease"
    );
    Ok(())
}

async fn heartbeat_lease_stale_generation_fails(store: &dyn RunStore) -> Result<()> {
    let record = created_record();
    let session = record.session.id;
    store.create(record).await?;

    let lease = store
        .acquire_lease(session, AgentInstanceId::generate(), lease_ttl())
        .await?;
    let stale = FencingGeneration::INITIAL;
    let outcome = store.heartbeat_lease(session, stale, lease_ttl()).await;
    ensure!(
        matches!(
            outcome,
            Err(RunStoreError::Cas(CasError::GenerationMismatch { .. }))
        ),
        "heartbeat with a stale generation must report GenerationMismatch, got {outcome:?}"
    );

    store.release_lease(session, lease.generation).await?;
    let outcome = store
        .heartbeat_lease(session, lease.generation, lease_ttl())
        .await;
    ensure!(
        matches!(
            outcome,
            Err(RunStoreError::Cas(CasError::StateMismatch { .. }))
        ),
        "heartbeat after release must report StateMismatch, got {outcome:?}"
    );
    Ok(())
}

async fn release_lease_makes_record_unleased(store: &dyn RunStore) -> Result<()> {
    let record = created_record();
    let session = record.session.id;
    store.create(record).await?;

    let lease = store
        .acquire_lease(session, AgentInstanceId::generate(), lease_ttl())
        .await?;
    store.release_lease(session, lease.generation).await?;

    let loaded = store.load(session).await?.unwrap();
    ensure!(loaded.lease.is_none(), "release must clear the lease");
    ensure!(
        loaded.generation == lease.generation,
        "release must not advance the generation"
    );
    Ok(())
}

async fn apply_with_live_lease_advances_state(store: &dyn RunStore) -> Result<()> {
    let record = created_record();
    let session = record.session.id;
    store.create(record).await?;

    let lease = store
        .acquire_lease(session, AgentInstanceId::generate(), lease_ttl())
        .await?;
    let run_id = run_id();
    let next = store
        .apply(session, lease.generation, RunEvent::Start { run_id })
        .await?;
    ensure!(
        next.state == RunState::Running,
        "Start must advance the state to Running"
    );
    ensure!(next.run_id == Some(run_id), "Start must bind the run id");
    ensure!(
        next.generation > lease.generation,
        "apply must advance the generation"
    );
    Ok(())
}

async fn apply_stale_generation_fails(store: &dyn RunStore) -> Result<()> {
    let record = created_record();
    let session = record.session.id;
    store.create(record).await?;

    let lease = store
        .acquire_lease(session, AgentInstanceId::generate(), lease_ttl())
        .await?;
    let outcome = store
        .apply(
            session,
            FencingGeneration::INITIAL,
            RunEvent::Start { run_id: run_id() },
        )
        .await;
    ensure!(
        matches!(
            outcome,
            Err(RunStoreError::Cas(CasError::GenerationMismatch { .. }))
        ),
        "apply with a stale generation must report GenerationMismatch, got {outcome:?}"
    );

    let next = store
        .apply(
            session,
            lease.generation,
            RunEvent::Start { run_id: run_id() },
        )
        .await?;
    let outcome = store
        .apply(session, lease.generation, RunEvent::Complete)
        .await;
    ensure!(
        matches!(
            outcome,
            Err(RunStoreError::Cas(CasError::GenerationMismatch { .. }))
        ),
        "apply with the pre-advance generation must report GenerationMismatch, got {outcome:?}"
    );

    let outcome = store
        .apply(session, next.generation, RunEvent::Complete)
        .await;
    ensure!(
        outcome.is_ok(),
        "apply with the current generation must succeed, got {outcome:?}"
    );
    Ok(())
}

async fn apply_without_lease_fails(store: &dyn RunStore) -> Result<()> {
    let record = created_record();
    let session = record.session.id;
    store.create(record).await?;

    let outcome = store
        .apply(
            session,
            FencingGeneration::INITIAL,
            RunEvent::Start { run_id: run_id() },
        )
        .await;
    ensure!(
        matches!(
            outcome,
            Err(RunStoreError::Cas(CasError::StateMismatch { .. }))
                | Err(RunStoreError::Cas(CasError::GenerationMismatch { .. }))
        ),
        "apply without a held lease must be rejected, got {outcome:?}"
    );
    Ok(())
}

async fn park_commits_parked_state(store: &dyn RunStore) -> Result<()> {
    let record = created_record();
    let session = record.session.id;
    store.create(record).await?;

    let lease = store
        .acquire_lease(session, AgentInstanceId::generate(), lease_ttl())
        .await?;
    let running = store
        .apply(
            session,
            lease.generation,
            RunEvent::Start { run_id: run_id() },
        )
        .await?;

    let commit = park_commit();
    let next = store
        .park(session, running.generation, commit.clone())
        .await?;
    ensure!(
        matches!(next.state, RunState::Parked { .. }),
        "park must advance the state to Parked"
    );

    let loaded = store.load(session).await?.unwrap();
    ensure!(
        matches!(
            loaded.state,
            RunState::Parked {
                reason: ParkReason::ApprovalsBlocked { .. },
                ..
            }
        ),
        "the stored record must carry the parked state"
    );
    Ok(())
}

async fn park_stale_generation_fails(store: &dyn RunStore) -> Result<()> {
    let record = created_record();
    let session = record.session.id;
    store.create(record).await?;

    let lease = store
        .acquire_lease(session, AgentInstanceId::generate(), lease_ttl())
        .await?;
    let running = store
        .apply(
            session,
            lease.generation,
            RunEvent::Start { run_id: run_id() },
        )
        .await?;

    let outcome = store
        .park(session, FencingGeneration::INITIAL, park_commit())
        .await;
    ensure!(
        matches!(
            outcome,
            Err(RunStoreError::Cas(CasError::GenerationMismatch { .. }))
        ),
        "park with a stale generation must report GenerationMismatch, got {outcome:?}"
    );

    let outcome = store.park(session, running.generation, park_commit()).await;
    ensure!(
        outcome.is_ok(),
        "park with the current generation must succeed, got {outcome:?}"
    );
    Ok(())
}

async fn park_outside_running_fails(store: &dyn RunStore) -> Result<()> {
    let record = created_record();
    let session = record.session.id;
    store.create(record).await?;

    let lease = store
        .acquire_lease(session, AgentInstanceId::generate(), lease_ttl())
        .await?;
    let outcome = store.park(session, lease.generation, park_commit()).await;
    ensure!(
        matches!(
            outcome,
            Err(RunStoreError::Cas(CasError::StateMismatch { .. }))
        ),
        "park outside Running must report StateMismatch, got {outcome:?}"
    );
    Ok(())
}

fn created_record() -> SessionRecord {
    record_with_session(ParkSessionId::generate())
}

fn record_with_session(session: ParkSessionId) -> SessionRecord {
    SessionRecord {
        session: Session {
            id: session,
            chat_session_id: Some(ChatSessionId::new("cs_conformance")),
            created_at: chrono::Utc::now(),
        },
        run_id: None,
        state: RunState::Created,
        lease: None,
        generation: FencingGeneration::INITIAL,
    }
}

fn lease_ttl() -> LeaseTtl {
    LeaseTtl::new(Duration::from_secs(5)).expect("positive ttl")
}

fn run_id() -> crate::RunId {
    "018f9d2e-7c3a-7000-8000-000000000271"
        .parse()
        .expect("valid run id")
}

fn park_commit() -> ParkCommit {
    ParkCommit {
        checkpoint: CheckpointEnvelope::new(RunCheckpoint::test_minimal()),
        reason: ParkReason::ApprovalsBlocked {
            decisions: NonEmpty::new(vec![DecisionId::generate()]).expect("non-empty"),
        },
        parked_at: chrono::Utc::now(),
        expires_at: chrono::Utc::now() + chrono::Duration::seconds(300),
    }
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
                agent_name: "test-agent".to_string(),
            },
            items: vec![ApprovalItem {
                tool_name: "conformance_tool".to_string(),
                arguments: serde_json::json!({"nested": {"b": 2, "a": [1, "two"]}}),
                tool_call_intent: None,
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

async fn resolve_durable(
    store: &dyn ApprovalStore,
    id: &DecisionId,
    decision: ApprovalDecision,
) -> Result<WakeReason> {
    store
        .resolve_durable(id, decision)
        .await
        .map_err(|err| anyhow!("resolve_durable failed: {err:?}"))
}

async fn next_payload(subscriber: &mut Subscription) -> Result<Bytes> {
    tokio::time::timeout(DELIVERY_TIMEOUT, subscriber.next())
        .await
        .map_err(|_| anyhow!("no payload within {DELIVERY_TIMEOUT:?}"))?
        .ok_or_else(|| anyhow!("the subscription ended before a payload arrived"))
}

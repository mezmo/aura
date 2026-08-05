//! Failpoint-style tests for the in-memory run store.
//!
//! These tests prove the checkpoint-commit window is atomic: a crash (or a
//! lost CAS) before the commit leaves the old state untouched, and a
//! successful commit always writes a fully-formed `Parked` record carrying
//! its checkpoint. No reader observes a half-written park.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use crate::hitl::DecisionId;
use crate::orchestration::park::{
    AgentInstanceId, CasError, ChatSessionId, CheckpointEnvelope, FencingGeneration, Lease,
    LeaseTtl, NonEmpty, ParkCommit, ParkReason, RunCheckpoint, RunEvent, RunState, Session,
    SessionId, SessionRecord,
};
use crate::session_store::{InMemoryRunStore, RunStore, RunStoreError};

const RUN_ID: &str = "018f9d2e-7c3a-7000-8000-000000000271";

fn lease_ttl() -> LeaseTtl {
    LeaseTtl::new(Duration::from_secs(60)).expect("positive ttl")
}

fn created_record() -> SessionRecord {
    SessionRecord {
        session: Session {
            id: SessionId::generate(),
            chat_session_id: Some(ChatSessionId::new("cs_failpoint")),
            created_at: Utc::now(),
        },
        run_id: None,
        state: RunState::Created,
        lease: None,
        generation: FencingGeneration::INITIAL,
    }
}

async fn seed_running(store: &InMemoryRunStore) -> (SessionId, FencingGeneration, AgentInstanceId) {
    let record = created_record();
    let session = record.session.id;
    let holder = AgentInstanceId::generate();
    store.create(record).await.expect("create succeeds");
    let lease = store
        .acquire_lease(session, holder, lease_ttl())
        .await
        .expect("acquire succeeds");
    let next = store
        .apply(
            session,
            lease.generation,
            RunEvent::Start {
                run_id: RUN_ID.parse().expect("valid uuid"),
            },
        )
        .await
        .expect("start succeeds");
    (session, next.generation, holder)
}

fn park_commit() -> ParkCommit {
    ParkCommit {
        checkpoint: CheckpointEnvelope::new(RunCheckpoint::test_minimal()),
        reason: ParkReason::ApprovalsBlocked {
            decisions: NonEmpty::new(vec![DecisionId::generate()]).unwrap(),
        },
        parked_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::seconds(300),
    }
}

fn expired_lease(generation: FencingGeneration) -> Lease {
    Lease {
        holder: AgentInstanceId::generate(),
        acquired_at: Utc::now(),
        heartbeat_at: Utc::now(),
        expires_at: Utc::now() - chrono::Duration::seconds(1),
        generation,
    }
}

#[tokio::test]
async fn create_rejects_non_created_state() {
    let store = InMemoryRunStore::new();
    let mut record = created_record();
    record.state = RunState::Running;
    record.run_id = Some(RUN_ID.parse().expect("valid uuid"));
    let session = record.session.id;

    let err = store
        .create(record)
        .await
        .expect_err("non-Created create is rejected");
    assert!(matches!(
        err,
        RunStoreError::Cas(CasError::StateMismatch {
            actual: "non-Created"
        })
    ));
    assert!(store.load(session).await.expect("load succeeds").is_none());
}

#[tokio::test]
async fn rejected_park_leaves_record_untouched() {
    // A stale or otherwise rejected CAS is the in-store equivalent of a crash
    // before the commit window closes: the stored record must remain exactly
    // what it was.
    let store = InMemoryRunStore::new();
    let (session, original_generation, _holder) = seed_running(&store).await;

    let stale = FencingGeneration::INITIAL;
    let err = store
        .park(session, stale, park_commit())
        .await
        .expect_err("stale generation is rejected");
    assert!(matches!(
        err,
        RunStoreError::Cas(CasError::GenerationMismatch { .. })
    ));

    let loaded = store
        .load(session)
        .await
        .expect("load succeeds")
        .expect("record exists");
    assert_eq!(loaded.state, RunState::Running);
    assert_eq!(loaded.generation, original_generation);
    assert!(loaded.lease.is_some());
}

#[tokio::test]
async fn successful_park_records_a_fully_formed_parked_state() {
    let store = InMemoryRunStore::new();
    let (session, before, _holder) = seed_running(&store).await;

    let commit = park_commit();
    let next = store
        .park(session, before, commit.clone())
        .await
        .expect("park succeeds");

    assert_eq!(next.generation, before.next());
    assert_eq!(
        next.state,
        RunState::Parked {
            reason: commit.reason,
            parked_at: commit.parked_at,
            expires_at: commit.expires_at,
            checkpoint: Box::new(commit.checkpoint),
        }
    );

    // A separate reader sees the same fully-formed record, never a partial.
    let loaded = store
        .load(session)
        .await
        .expect("load succeeds")
        .expect("record exists");
    assert_eq!(loaded.state, next.state);
    assert_eq!(loaded.generation, next.generation);
}

#[tokio::test]
async fn concurrent_parks_are_atomic_no_half_written_state() {
    // Two pods race to park the same running run. The store's single
    // critical section makes the operation atomic: one wins, the other loses
    // with a generation mismatch, and the winner's record is either observed
    // in full or not at all.
    let store = Arc::new(InMemoryRunStore::new());
    let (session, generation, _holder) = seed_running(&store).await;

    let attempts: Vec<_> = (0..4)
        .map(|i| {
            let store = Arc::clone(&store);
            let commit = ParkCommit {
                checkpoint: CheckpointEnvelope::new(RunCheckpoint::test_minimal()),
                reason: ParkReason::ApprovalsBlocked {
                    decisions: NonEmpty::new(vec![DecisionId::generate()]).unwrap(),
                },
                parked_at: Utc::now() + chrono::Duration::milliseconds(i),
                expires_at: Utc::now() + chrono::Duration::seconds(300),
            };
            tokio::spawn(async move { store.park(session, generation, commit).await })
        })
        .collect();

    let mut successes = 0;
    let mut failures = 0;
    for attempt in attempts {
        match attempt.await.expect("task completes") {
            Ok(_) => successes += 1,
            Err(RunStoreError::Cas(CasError::GenerationMismatch { .. })) => failures += 1,
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    assert_eq!(successes, 1, "exactly one park CAS may succeed");
    assert_eq!(failures, 3, "the rest lose on generation");

    let loaded = store
        .load(session)
        .await
        .expect("load succeeds")
        .expect("record exists");
    match loaded.state {
        RunState::Parked {
            reason: ParkReason::ApprovalsBlocked { .. },
            checkpoint: _,
            ..
        } => {
            assert_eq!(loaded.generation, generation.next());
        }
        other => panic!("loaded state must be a fully-formed Parked, got {other:?}"),
    }
}

#[tokio::test]
async fn lease_acquire_is_a_fenced_cas() {
    // A live lease held by another instance blocks a second claim; an expired
    // lease transfers ownership and advances the fencing generation.
    let store = InMemoryRunStore::new();
    let (session, _generation, first_holder) = seed_running(&store).await;

    let second_holder = AgentInstanceId::generate();
    let err = store
        .acquire_lease(session, second_holder, lease_ttl())
        .await
        .expect_err("live lease blocks second claim");
    assert!(matches!(
        err,
        RunStoreError::LeaseHeld {
            holder,
            ..
        } if holder == first_holder
    ));

    // A record whose lease is already expired transfers to a new holder and
    // advances the fencing generation.
    let mut expired = created_record();
    let expired_session = expired.session.id;
    let before = FencingGeneration::INITIAL.next();
    expired.generation = before;
    expired.lease = Some(expired_lease(before));
    store.create(expired).await.expect("create succeeds");

    let lease = store
        .acquire_lease(expired_session, second_holder, lease_ttl())
        .await
        .expect("expired lease transfers");
    assert_eq!(lease.holder, second_holder);
    assert_eq!(lease.generation, before.next());

    let after = store.load(expired_session).await.unwrap().unwrap();
    assert_eq!(after.generation, lease.generation);
    assert_eq!(after.lease.as_ref().unwrap().holder, second_holder);
}

#[tokio::test]
async fn stale_generation_write_is_rejected() {
    // After a successful claim, a writer presenting the old generation must
    // be rejected, even if it once held the lease.
    let store = InMemoryRunStore::new();
    let record = created_record();
    let session = record.session.id;
    let old_generation = record.generation;
    store.create(record).await.expect("create succeeds");

    let new_holder = AgentInstanceId::generate();
    store
        .acquire_lease(session, new_holder, lease_ttl())
        .await
        .expect("claim succeeds");

    let err = store
        .apply(
            session,
            old_generation,
            RunEvent::Start {
                run_id: RUN_ID.parse().expect("valid uuid"),
            },
        )
        .await
        .expect_err("stale generation is rejected");
    assert!(matches!(
        err,
        RunStoreError::Cas(CasError::GenerationMismatch { .. })
    ));
}

#[tokio::test]
async fn heartbeat_rejects_expired_lease() {
    let store = InMemoryRunStore::new();
    let mut record = created_record();
    let generation = FencingGeneration::INITIAL.next();
    record.generation = generation;
    record.lease = Some(expired_lease(generation));
    let session = record.session.id;
    store.create(record).await.expect("create succeeds");

    let err = store
        .heartbeat_lease(session, generation, lease_ttl())
        .await
        .expect_err("expired lease heartbeat is rejected");
    assert!(matches!(
        err,
        RunStoreError::Cas(CasError::StateMismatch { actual: "expired" })
    ));
}

#[tokio::test]
async fn apply_rejects_expired_lease() {
    let store = InMemoryRunStore::new();
    let mut record = created_record();
    let generation = FencingGeneration::INITIAL.next();
    record.generation = generation;
    record.lease = Some(expired_lease(generation));
    let session = record.session.id;
    store.create(record).await.expect("create succeeds");

    let err = store
        .apply(session, generation, RunEvent::Complete)
        .await
        .expect_err("expired lease apply is rejected");
    assert!(matches!(
        err,
        RunStoreError::Cas(CasError::StateMismatch { actual: "expired" })
    ));
}

#[tokio::test]
async fn park_rejects_expired_lease() {
    let store = InMemoryRunStore::new();
    let mut record = created_record();
    let generation = FencingGeneration::INITIAL.next();
    record.generation = generation;
    record.lease = Some(expired_lease(generation));
    let session = record.session.id;
    store.create(record).await.expect("create succeeds");

    let err = store
        .park(session, generation, park_commit())
        .await
        .expect_err("expired lease park is rejected");
    assert!(matches!(
        err,
        RunStoreError::Cas(CasError::StateMismatch { actual: "expired" })
    ));
}

#[tokio::test]
async fn mutation_rejects_released_lease() {
    let store = InMemoryRunStore::new();
    let record = created_record();
    let session = record.session.id;
    let holder = AgentInstanceId::generate();
    store.create(record).await.expect("create succeeds");
    let lease = store
        .acquire_lease(session, holder, lease_ttl())
        .await
        .expect("acquire succeeds");
    store
        .release_lease(session, lease.generation)
        .await
        .expect("release succeeds");

    let err = store
        .apply(session, lease.generation, RunEvent::Complete)
        .await
        .expect_err("released lease apply is rejected");
    assert!(matches!(
        err,
        RunStoreError::Cas(CasError::StateMismatch { actual: "unleased" })
    ));

    let err = store
        .park(session, lease.generation, park_commit())
        .await
        .expect_err("released lease park is rejected");
    assert!(matches!(
        err,
        RunStoreError::Cas(CasError::StateMismatch { actual: "unleased" })
    ));

    let err = store
        .heartbeat_lease(session, lease.generation, lease_ttl())
        .await
        .expect_err("released lease heartbeat is rejected");
    assert!(matches!(
        err,
        RunStoreError::Cas(CasError::StateMismatch { actual: "unleased" })
    ));
}

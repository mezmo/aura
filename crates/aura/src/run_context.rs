//! Ambient run identity, scoped to the task doing the work.
//!
//! Rig invokes tools as `Tool::call(args)` with no call-time context, so a tool
//! cannot be handed the run it belongs to. A task-local supplies it without any
//! component storing it: concurrent runs each see their own, and nothing has to
//! be reset between runs.
//!
//! This reaches only code running inside the run's task. MCP progress
//! notifications arrive on the transport's own task and cannot read it — they
//! are correlated by progress token instead, registered at call time from here.

use std::pin::Pin;
use std::task::{Context, Poll};

use futures::Stream;

tokio::task_local! {
    static RUN_ID: String;
}

pub fn current_run_id() -> Option<String> {
    RUN_ID.try_with(Clone::clone).ok()
}

/// Runs `f` with `run_id` in scope. Task-locals do not cross `tokio::spawn`, so
/// a spawned worker needs its own call rather than inheriting its parent's.
pub async fn with_run_id<F: Future>(run_id: String, f: F) -> F::Output {
    RUN_ID.scope(run_id, f).await
}

/// Enters the scope on every poll, so a stream polled by a consumer outside the
/// run still executes its tool calls with the run's identity in scope.
pub fn scope_stream<S: Stream>(run_id: String, inner: S) -> ScopedStream<S> {
    ScopedStream { run_id, inner }
}

pub struct ScopedStream<S> {
    run_id: String,
    inner: S,
}

impl<S: Stream> Stream for ScopedStream<S> {
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // SAFETY: `run_id` is never moved, and `inner` is only projected here.
        let this = unsafe { self.get_unchecked_mut() };
        let inner = unsafe { Pin::new_unchecked(&mut this.inner) };
        let run_id = this.run_id.clone();
        RUN_ID.sync_scope(run_id, || inner.poll_next(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[tokio::test]
    async fn there_is_no_run_id_outside_a_run() {
        assert_eq!(current_run_id(), None);
    }

    #[tokio::test]
    async fn a_scope_supplies_the_run_id() {
        let seen = with_run_id("run_1".to_string(), async { current_run_id() }).await;
        assert_eq!(seen.as_deref(), Some("run_1"));
    }

    #[tokio::test]
    async fn concurrent_runs_do_not_see_each_other() {
        let a = tokio::spawn(with_run_id("run_a".to_string(), async {
            tokio::task::yield_now().await;
            current_run_id()
        }));
        let b = tokio::spawn(with_run_id("run_b".to_string(), async {
            tokio::task::yield_now().await;
            current_run_id()
        }));

        assert_eq!(a.await.unwrap().as_deref(), Some("run_a"));
        assert_eq!(b.await.unwrap().as_deref(), Some("run_b"));
    }

    #[tokio::test]
    async fn a_scoped_stream_carries_identity_into_each_poll() {
        let inner = futures::stream::iter(0..3).map(|_| current_run_id());
        let seen: Vec<_> = scope_stream("run_s".to_string(), inner).collect().await;

        assert_eq!(seen, vec![Some("run_s".to_string()); 3]);
    }

    /// Orchestration drives workers with `FuturesUnordered` inside the run's
    /// task rather than spawning them, which is what lets their tool calls —
    /// and so their progress — resolve to the run.
    #[tokio::test]
    async fn workers_driven_as_futures_keep_the_run_id() {
        use futures::stream::{FuturesUnordered, StreamExt};

        let seen = with_run_id("run_w".to_string(), async {
            let mut workers: FuturesUnordered<_> = (0..3)
                .map(|_| async {
                    tokio::task::yield_now().await;
                    current_run_id()
                })
                .collect();

            let mut ids = Vec::new();
            while let Some(id) = workers.next().await {
                ids.push(id);
            }
            ids
        })
        .await;

        assert_eq!(seen, vec![Some("run_w".to_string()); 3]);
    }

    /// A spawned task does not inherit its parent's scope, which is why every
    /// orchestration worker establishes its own.
    #[tokio::test]
    async fn a_spawned_task_does_not_inherit_the_scope() {
        let seen = with_run_id("run_p".to_string(), async {
            tokio::spawn(async { current_run_id() }).await.unwrap()
        })
        .await;

        assert_eq!(seen, None);
    }
}

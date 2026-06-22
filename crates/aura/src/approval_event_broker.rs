//! Request-scoped HITL approval event broker.
//!
//! This routes approval lifecycle events to the active SSE response for the
//! matching request. It is not the approval parking registry: it stores no
//! decisions and has no resume semantics.

use std::collections::HashMap;
use std::sync::OnceLock;

use aura_events::{ApprovalCompleted, ApprovalPending, ApprovalRequested};
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, warn};

const EVENT_CHANNEL_CAPACITY: usize = 32;

/// HITL approval lifecycle events routed to one request stream.
#[derive(Clone, Debug)]
pub enum ApprovalLifecycleEvent {
    Requested(ApprovalRequested),
    Pending(ApprovalPending),
    Completed(ApprovalCompleted),
}

/// Request-scoped approval event broker.
pub struct ApprovalEventBroker {
    senders: RwLock<HashMap<String, mpsc::Sender<ApprovalLifecycleEvent>>>,
}

impl ApprovalEventBroker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            senders: RwLock::new(HashMap::new()),
        }
    }

    /// Subscribe to approval lifecycle events for one request.
    pub async fn subscribe(&self, request_id: &str) -> mpsc::Receiver<ApprovalLifecycleEvent> {
        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let mut senders = self.senders.write().await;
        senders.insert(request_id.to_owned(), tx);
        debug!(
            "Approval event subscription created for request '{}' (total active: {})",
            request_id,
            senders.len()
        );
        rx
    }

    /// Remove a request subscription.
    pub async fn unsubscribe(&self, request_id: &str) {
        let mut senders = self.senders.write().await;
        if senders.remove(request_id).is_some() {
            debug!(
                "Approval event subscription removed for request '{}' (remaining: {})",
                request_id,
                senders.len()
            );
        }
    }

    /// Publish an approval lifecycle event. Returns false if no stream is active
    /// or the receiver cannot accept the event immediately.
    pub async fn publish(&self, request_id: &str, event: ApprovalLifecycleEvent) -> bool {
        let sender = {
            let senders = self.senders.read().await;
            senders.get(request_id).cloned()
        };

        let Some(sender) = sender else {
            debug!(
                "No approval event subscriber for request '{}' (event dropped)",
                request_id
            );
            return false;
        };

        match sender.try_send(event) {
            Ok(()) => {
                debug!("Approval event sent to request '{}'", request_id);
                true
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.unsubscribe(request_id).await;
                debug!(
                    "Approval event receiver dropped for request '{}' (cleaned up)",
                    request_id
                );
                false
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!(
                    "Approval event channel full for request '{}' (event dropped)",
                    request_id
                );
                false
            }
        }
    }

    #[cfg(test)]
    pub async fn active_subscriptions(&self) -> usize {
        self.senders.read().await.len()
    }
}

impl Default for ApprovalEventBroker {
    fn default() -> Self {
        Self::new()
    }
}

static GLOBAL_BROKER: OnceLock<ApprovalEventBroker> = OnceLock::new();

pub fn global() -> &'static ApprovalEventBroker {
    GLOBAL_BROKER.get_or_init(ApprovalEventBroker::new)
}

pub async fn subscribe(request_id: &str) -> mpsc::Receiver<ApprovalLifecycleEvent> {
    global().subscribe(request_id).await
}

pub async fn unsubscribe(request_id: &str) {
    global().unsubscribe(request_id).await;
}

pub async fn publish(request_id: &str, event: ApprovalLifecycleEvent) -> bool {
    global().publish(request_id, event).await
}

#[cfg(test)]
mod tests {
    use aura_events::{AgentScopeWire, ApprovalOriginWire, ApprovalRequested};

    use super::*;

    fn requested(decision_id: &str) -> ApprovalLifecycleEvent {
        ApprovalLifecycleEvent::Requested(ApprovalRequested {
            decision_id: decision_id.to_owned(),
            tool_name: "dangerous_apply".to_owned(),
            origin: ApprovalOriginWire::ConfigGate {
                matched_pattern: "dangerous_*".to_owned(),
            },
            scope: AgentScopeWire::Single { session_id: None },
        })
    }

    #[tokio::test]
    async fn subscribe_publish_recv_roundtrip() {
        let broker = ApprovalEventBroker::new();
        let mut rx = broker.subscribe("req-1").await;

        assert!(broker.publish("req-1", requested("dec-1")).await);

        match rx.recv().await.expect("event") {
            ApprovalLifecycleEvent::Requested(event) => assert_eq!(event.decision_id, "dec-1"),
            other => panic!("expected requested, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn publish_without_subscriber_returns_false() {
        let broker = ApprovalEventBroker::new();

        assert!(!broker.publish("missing", requested("dec-1")).await);
    }

    #[tokio::test]
    async fn receiver_drop_cleans_stale_subscription() {
        let broker = ApprovalEventBroker::new();
        let rx = broker.subscribe("req-1").await;
        drop(rx);

        assert!(!broker.publish("req-1", requested("dec-1")).await);
        assert_eq!(broker.active_subscriptions().await, 0);
    }

    #[tokio::test]
    async fn events_are_isolated_by_request() {
        let broker = ApprovalEventBroker::new();
        let mut rx_a = broker.subscribe("req-a").await;
        let mut rx_b = broker.subscribe("req-b").await;

        assert!(broker.publish("req-b", requested("dec-b")).await);

        assert!(rx_a.try_recv().is_err());
        match rx_b.recv().await.expect("event") {
            ApprovalLifecycleEvent::Requested(event) => assert_eq!(event.decision_id, "dec-b"),
            other => panic!("expected requested, got {:?}", other),
        }
    }
}

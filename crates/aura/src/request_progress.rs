//! Request-scoped progress broker for MCP progress notifications.
//!
//! Routes progress notifications to specific HTTP requests only (no cross-customer leakage).
//! Channels auto-cleanup when receiver is dropped at request end.

use rmcp::model::ProgressToken;
use std::collections::HashMap;
use std::sync::OnceLock;
use tokio::sync::{RwLock, mpsc};
use tracing::debug;

/// Channel capacity for progress notifications per request
const PROGRESS_CHANNEL_CAPACITY: usize = 1024;

/// Progress notification forwarded from MCP handler to SSE streams
#[derive(Clone, Debug)]
pub struct ProgressNotification {
    /// The progress token (correlates to tool call)
    pub progress_token: ProgressToken,
    /// Current progress value (rmcp uses f64 for JSON-RPC compatibility)
    pub progress: f64,
    /// Total progress value (if known)
    pub total: Option<f64>,
    /// Optional message describing current step
    pub message: Option<String>,
}

impl ProgressNotification {
    /// Calculate percent completion (0-100) if total is known
    pub fn percent(&self) -> Option<u8> {
        self.total.map(|total| {
            if total == 0.0 {
                100
            } else {
                ((self.progress / total) * 100.0).min(100.0) as u8
            }
        })
    }
}

/// Request-scoped progress broker that routes MCP progress notifications
/// to specific HTTP requests only.
pub struct RequestProgressBroker {
    /// Map of request_id -> progress channel sender
    senders: RwLock<HashMap<String, mpsc::Sender<ProgressNotification>>>,
}

impl RequestProgressBroker {
    pub fn new() -> Self {
        Self {
            senders: RwLock::new(HashMap::new()),
        }
    }

    /// Subscribe to progress notifications for a request. Auto-unsubscribes when receiver dropped.
    pub async fn subscribe(&self, request_id: &str) -> mpsc::Receiver<ProgressNotification> {
        let (tx, rx) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);

        let mut senders = self.senders.write().await;
        senders.insert(request_id.to_string(), tx);

        debug!(
            "Progress subscription created for request '{}' (total active: {})",
            request_id,
            senders.len()
        );

        rx
    }

    pub async fn unsubscribe(&self, request_id: &str) {
        let mut senders = self.senders.write().await;
        if senders.remove(request_id).is_some() {
            debug!(
                "Progress subscription removed for request '{}' (remaining: {})",
                request_id,
                senders.len()
            );
        }
    }

    /// Publish a progress notification. Returns true if sent, false if no subscriber.
    /// Automatically cleans up stale entries when receiver has been dropped.
    pub async fn publish(&self, request_id: &str, notification: ProgressNotification) -> bool {
        // First try with read lock
        let send_result = {
            let senders = self.senders.read().await;
            if let Some(sender) = senders.get(request_id) {
                Some(sender.send(notification).await)
            } else {
                None
            }
        };

        match send_result {
            Some(Ok(())) => {
                debug!("Progress notification sent to request '{}'", request_id);
                true
            }
            Some(Err(_)) => {
                // Receiver dropped - clean up stale entry
                self.unsubscribe(request_id).await;
                debug!(
                    "Progress receiver dropped for request '{}' (cleaned up)",
                    request_id
                );
                false
            }
            None => {
                debug!(
                    "No progress subscriber for request '{}' (notification dropped)",
                    request_id
                );
                false
            }
        }
    }

    pub async fn active_subscriptions(&self) -> usize {
        self.senders.read().await.len()
    }
}

impl Default for RequestProgressBroker {
    fn default() -> Self {
        Self::new()
    }
}

static GLOBAL_BROKER: OnceLock<RequestProgressBroker> = OnceLock::new();

pub fn global() -> &'static RequestProgressBroker {
    GLOBAL_BROKER.get_or_init(RequestProgressBroker::new)
}

pub async fn subscribe(request_id: &str) -> mpsc::Receiver<ProgressNotification> {
    global().subscribe(request_id).await
}

pub async fn unsubscribe(request_id: &str) {
    global().unsubscribe(request_id).await
}

pub async fn publish(request_id: &str, notification: ProgressNotification) -> bool {
    global().publish(request_id, notification).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::NumberOrString;
    use std::sync::Arc;

    /// Helper to create a numeric progress token for tests
    fn numeric_token(n: i64) -> ProgressToken {
        ProgressToken(NumberOrString::Number(n))
    }

    /// Helper to create a string progress token for tests
    fn string_token(s: &str) -> ProgressToken {
        ProgressToken(NumberOrString::String(Arc::from(s)))
    }

    #[tokio::test]
    async fn test_broker_creation() {
        let broker = RequestProgressBroker::new();
        assert_eq!(broker.active_subscriptions().await, 0);
    }

    #[tokio::test]
    async fn test_subscribe_creates_channel() {
        let broker = RequestProgressBroker::new();
        let _rx = broker.subscribe("req_123").await;
        assert_eq!(broker.active_subscriptions().await, 1);
    }

    #[tokio::test]
    async fn test_unsubscribe_removes_channel() {
        let broker = RequestProgressBroker::new();
        let _rx = broker.subscribe("req_123").await;
        assert_eq!(broker.active_subscriptions().await, 1);

        broker.unsubscribe("req_123").await;
        assert_eq!(broker.active_subscriptions().await, 0);
    }

    #[tokio::test]
    async fn test_publish_to_subscribed_request() {
        let broker = RequestProgressBroker::new();
        let mut rx = broker.subscribe("req_123").await;

        let notification = ProgressNotification {
            progress_token: numeric_token(1),
            progress: 50.0,
            total: Some(100.0),
            message: Some("Halfway there".to_string()),
        };

        let sent = broker.publish("req_123", notification).await;
        assert!(sent);

        let received = rx.recv().await.unwrap();
        assert_eq!(received.progress, 50.0);
        assert_eq!(received.message, Some("Halfway there".to_string()));
    }

    #[tokio::test]
    async fn test_publish_to_unsubscribed_request_fails() {
        let broker = RequestProgressBroker::new();

        let notification = ProgressNotification {
            progress_token: numeric_token(1),
            progress: 50.0,
            total: Some(100.0),
            message: None,
        };

        // No subscriber - should return false
        let sent = broker.publish("req_nonexistent", notification).await;
        assert!(!sent);
    }

    #[tokio::test]
    async fn test_requests_are_isolated() {
        let broker = RequestProgressBroker::new();
        let mut rx1 = broker.subscribe("req_1").await;
        let mut rx2 = broker.subscribe("req_2").await;

        // Send to req_1 only
        let notification = ProgressNotification {
            progress_token: string_token("token_1"),
            progress: 25.0,
            total: Some(100.0),
            message: Some("Request 1 progress".to_string()),
        };
        broker.publish("req_1", notification).await;

        // req_1 should receive it
        let received = rx1.recv().await.unwrap();
        assert_eq!(received.message, Some("Request 1 progress".to_string()));

        // req_2 should NOT receive anything (use try_recv to avoid blocking)
        assert!(rx2.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_percent_calculation() {
        let notification = ProgressNotification {
            progress_token: numeric_token(1),
            progress: 50.0,
            total: Some(100.0),
            message: None,
        };
        assert_eq!(notification.percent(), Some(50));

        let notification_no_total = ProgressNotification {
            progress_token: numeric_token(2),
            progress: 50.0,
            total: None,
            message: None,
        };
        assert_eq!(notification_no_total.percent(), None);

        let notification_zero_total = ProgressNotification {
            progress_token: numeric_token(3),
            progress: 0.0,
            total: Some(0.0),
            message: None,
        };
        assert_eq!(notification_zero_total.percent(), Some(100));
    }

    #[tokio::test]
    async fn test_multiple_notifications_same_request() {
        let broker = RequestProgressBroker::new();
        let mut rx = broker.subscribe("req_123").await;

        // Send multiple notifications
        for i in 1..=5 {
            let notification = ProgressNotification {
                progress_token: numeric_token(1),
                progress: i as f64 * 20.0,
                total: Some(100.0),
                message: Some(format!("Step {}", i)),
            };
            broker.publish("req_123", notification).await;
        }

        // Should receive all 5
        for i in 1..=5 {
            let received = rx.recv().await.unwrap();
            assert_eq!(received.progress, i as f64 * 20.0);
            assert_eq!(received.message, Some(format!("Step {}", i)));
        }
    }

    #[tokio::test]
    async fn test_lazy_cleanup_on_receiver_drop() {
        let broker = RequestProgressBroker::new();

        // Subscribe and verify entry exists
        {
            let _rx = broker.subscribe("req_cleanup").await;
            assert_eq!(broker.active_subscriptions().await, 1);
        } // Receiver dropped here

        // Entry still exists in HashMap (lazy cleanup hasn't happened yet)
        // Publish should fail because receiver is gone, triggering cleanup
        let notification = ProgressNotification {
            progress_token: numeric_token(1),
            progress: 50.0,
            total: Some(100.0),
            message: Some("Should fail".to_string()),
        };
        let sent = broker.publish("req_cleanup", notification).await;

        // Publish returns false when receiver is dropped
        assert!(!sent);

        // Lazy cleanup should have removed the stale entry
        assert_eq!(broker.active_subscriptions().await, 0);
    }

    #[tokio::test]
    async fn test_publish_succeeds_with_active_receiver() {
        let broker = RequestProgressBroker::new();

        // Subscribe and keep receiver alive
        let mut rx = broker.subscribe("req_active").await;
        assert_eq!(broker.active_subscriptions().await, 1);

        // Publish should succeed
        let notification = ProgressNotification {
            progress_token: numeric_token(1),
            progress: 75.0,
            total: Some(100.0),
            message: Some("Should succeed".to_string()),
        };
        let sent = broker.publish("req_active", notification).await;
        assert!(sent);

        // Receiver should get the message
        let received = rx.recv().await;
        assert!(received.is_some());
        let msg = received.unwrap();
        assert_eq!(msg.progress, 75.0);
        assert_eq!(msg.message, Some("Should succeed".to_string()));

        // Subscription still active
        assert_eq!(broker.active_subscriptions().await, 1);
    }

    // =========================================================================
    // Channel Backpressure Tests
    // =========================================================================

    #[tokio::test]
    async fn test_channel_backpressure_behavior() {
        let broker = RequestProgressBroker::new();
        let _rx = broker.subscribe("req_backpressure").await;
        // Don't read from _rx - simulate slow consumer

        // Send more messages than channel capacity (1024)
        // With bounded channel, this should block when full
        let send_count = PROGRESS_CHANNEL_CAPACITY + 100;

        // Use timeout to avoid hanging if blocking occurs
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), async {
            for i in 0..send_count {
                let notification = ProgressNotification {
                    progress_token: numeric_token(i as i64),
                    progress: i as f64,
                    total: Some(send_count as f64),
                    message: Some(format!("msg {}", i)),
                };
                broker.publish("req_backpressure", notification).await;
            }
        })
        .await;

        // The bounded channel should cause this to timeout (blocks when full)
        // This documents that we have backpressure, not unbounded growth
        assert!(
            result.is_err(),
            "Expected timeout - bounded channel should block when full"
        );
    }

    #[tokio::test]
    async fn test_channel_drains_properly() {
        let broker = RequestProgressBroker::new();
        let mut rx = broker.subscribe("req_drain").await;

        // Send exactly capacity messages
        for i in 0..PROGRESS_CHANNEL_CAPACITY {
            let notification = ProgressNotification {
                progress_token: numeric_token(i as i64),
                progress: i as f64,
                total: Some(PROGRESS_CHANNEL_CAPACITY as f64),
                message: None,
            };
            assert!(broker.publish("req_drain", notification).await);
        }

        // Drain all messages
        let mut received = 0;
        while rx.try_recv().is_ok() {
            received += 1;
        }

        assert_eq!(
            received, PROGRESS_CHANNEL_CAPACITY,
            "Should receive exactly capacity messages"
        );
    }

    #[tokio::test]
    async fn test_concurrent_publish_to_same_request() {
        let broker = std::sync::Arc::new(RequestProgressBroker::new());
        let mut rx = broker.subscribe("req_concurrent").await;

        // Spawn multiple publishers
        let handles: Vec<_> = (0..10)
            .map(|publisher_id| {
                let broker = broker.clone();
                tokio::spawn(async move {
                    for i in 0..10 {
                        let notification = ProgressNotification {
                            progress_token: numeric_token((publisher_id * 10 + i) as i64),
                            progress: i as f64,
                            total: Some(10.0),
                            message: Some(format!("pub {} msg {}", publisher_id, i)),
                        };
                        broker.publish("req_concurrent", notification).await;
                    }
                })
            })
            .collect();

        // Wait for all publishers to complete
        for h in handles {
            h.await.unwrap();
        }

        // Count received messages
        let mut received = 0;
        while rx.try_recv().is_ok() {
            received += 1;
        }

        // Should receive all 100 messages (10 publishers * 10 messages)
        assert_eq!(received, 100, "Should receive all concurrent messages");
    }
}

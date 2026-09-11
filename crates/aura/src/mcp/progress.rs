/*!
 * MCP Progress Handler
 *
 * Provides a ClientHandler implementation that supports progress notifications
 * from MCP servers. This enables forwarding progress updates during long-running
 * tool executions to clients via SSE events.
 *
 * # Architecture
 *
 * When an MCP server sends a `notifications/progress` message:
 * 1. `ProgressEnabledHandler.on_progress()` receives the notification
 * 2. The notification is published to the request-scoped `RequestProgressBroker`
 * 3. Web server SSE handlers receive progress only for their specific request
 *
 * # Security
 *
 * Progress notifications are scoped to the HTTP request that initiated the tool call.
 * This prevents cross-customer data leakage in multi-tenant deployments.
 */

use rmcp::{
    ClientHandler,
    handler::client::progress::ProgressDispatcher,
    model::ProgressNotificationParam,
    service::{NotificationContext, RoleClient},
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::request_progress::{self, ProgressNotification};

/// A custom ClientHandler that routes progress notifications to request-scoped channels.
///
/// This handler is used instead of `()` when creating MCP clients to enable
/// progress notification support. Progress notifications received from the
/// server are routed to the specific HTTP request that initiated the tool call,
/// ensuring no cross-request or cross-customer data leakage.
///
/// # Example
/// ```ignore
/// // Create handler with shared request ID reference
/// let current_request_id = Arc::new(RwLock::new(None));
/// let handler = ProgressEnabledHandler::new(current_request_id.clone());
/// let client = serve_client(handler.clone(), transport).await?;
///
/// // Set request ID before tool execution
/// *current_request_id.write().await = Some("req_123".to_string());
///
/// // Progress notifications will now be routed to req_123's channel
/// ```
#[derive(Clone)]
pub struct ProgressEnabledHandler {
    progress_dispatcher: ProgressDispatcher,
    /// Shared reference to the current HTTP request ID
    /// This is set by the MCP client before each tool execution
    current_request_id: Arc<RwLock<Option<String>>>,
    /// Flag to log orphaned progress only once (prevents log flood from servers ignoring cancellation)
    logged_orphaned_warning: Arc<AtomicBool>,
    /// Counter for orphaned progress notifications (for diagnostics)
    orphaned_count: Arc<AtomicU64>,
}

impl std::fmt::Debug for ProgressEnabledHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgressEnabledHandler")
            .field("progress_dispatcher", &self.progress_dispatcher)
            .field("current_request_id", &"Arc<RwLock<Option<String>>>")
            .finish()
    }
}

impl ProgressEnabledHandler {
    pub fn new(current_request_id: Arc<RwLock<Option<String>>>) -> Self {
        Self {
            progress_dispatcher: ProgressDispatcher::new(),
            current_request_id,
            logged_orphaned_warning: Arc::new(AtomicBool::new(false)),
            orphaned_count: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Reset the orphaned warning flag and counter (call when setting a new request ID)
    pub fn reset_orphaned_tracking(&self) {
        self.logged_orphaned_warning.store(false, Ordering::SeqCst);
        self.orphaned_count.store(0, Ordering::SeqCst);
    }

    /// Get the count of orphaned progress notifications received
    pub fn orphaned_count(&self) -> u64 {
        self.orphaned_count.load(Ordering::SeqCst)
    }

    /// Get a reference to the progress dispatcher for subscribing to notifications
    pub fn progress_dispatcher(&self) -> &ProgressDispatcher {
        &self.progress_dispatcher
    }
}

impl ClientHandler for ProgressEnabledHandler {
    /// Handle progress notifications from the MCP server
    ///
    /// This method is called when the server sends `notifications/progress` messages.
    /// The notification is:
    /// 1. Routed to the request-scoped progress channel (if request ID is set)
    /// 2. Routed to the ProgressDispatcher for `call_tool_with_progress()` subscribers
    ///
    /// # Security
    /// Progress is only delivered to the HTTP request that initiated the tool call.
    /// If no request ID is set (CLI mode), the notification is logged but not forwarded.
    #[allow(clippy::manual_async_fn)]
    fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        async move {
            // Get current request ID (if any)
            let request_id = self.current_request_id.read().await.clone();

            // Build notification for request-scoped broker
            let notification = ProgressNotification {
                progress_token: params.progress_token.clone(),
                progress: params.progress,
                total: params.total,
                message: params.message.clone(),
            };

            if let Some(ref req_id) = request_id {
                // Route to request-specific channel
                let sent = request_progress::publish(req_id, notification).await;
                if sent {
                    debug!(
                        "Progress notification routed to request '{}': progress={}, message={:?}",
                        req_id, params.progress, params.message
                    );
                } else {
                    // Request may have ended/cancelled - log at debug level only
                    debug!(
                        "Progress notification dropped for request '{}' (no subscriber)",
                        req_id
                    );
                }
            } else {
                // No request context - could be CLI mode, test, or cancelled request
                // Increment counter and log at INFO so we can see the flow
                let count = self.orphaned_count.fetch_add(1, Ordering::SeqCst) + 1;

                // First orphaned notification gets a warning
                if !self.logged_orphaned_warning.swap(true, Ordering::SeqCst) {
                    warn!(
                        "MCP server ignoring cancellation - orphaned progress notifications arriving"
                    );
                }

                // Log every orphaned notification at INFO for visibility
                info!(
                    "Orphaned MCP progress #{}: progress={}, message={:?}",
                    count, params.progress, params.message
                );
            }

            // Also route to the dispatcher for call_tool_with_progress() subscribers
            self.progress_dispatcher.handle_notification(params).await;
            debug!("Progress notification routed to dispatcher");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_handler() -> ProgressEnabledHandler {
        let current_request_id = Arc::new(RwLock::new(None));
        ProgressEnabledHandler::new(current_request_id)
    }

    #[test]
    fn test_handler_creation() {
        let handler = create_test_handler();
        // Just verify it can be created and progress_dispatcher is accessible
        let _ = handler.progress_dispatcher();
    }

    #[test]
    fn test_handler_clone() {
        let handler = create_test_handler();
        let cloned = handler.clone();
        // Both should have accessible progress dispatchers
        let _ = cloned.progress_dispatcher();
    }

    #[tokio::test]
    async fn test_handler_with_request_id() {
        let current_request_id = Arc::new(RwLock::new(Some("req_test_123".to_string())));
        let handler = ProgressEnabledHandler::new(current_request_id.clone());

        // Verify request ID is accessible
        let req_id = handler.current_request_id.read().await;
        assert_eq!(*req_id, Some("req_test_123".to_string()));
    }

    #[tokio::test]
    async fn test_handler_request_id_changes() {
        let current_request_id = Arc::new(RwLock::new(None));
        let handler = ProgressEnabledHandler::new(current_request_id.clone());

        // Initially no request ID
        {
            let req_id = handler.current_request_id.read().await;
            assert_eq!(*req_id, None);
        }

        // Set request ID
        {
            let mut req_id = current_request_id.write().await;
            *req_id = Some("req_456".to_string());
        }

        // Handler should see the new value
        {
            let req_id = handler.current_request_id.read().await;
            assert_eq!(*req_id, Some("req_456".to_string()));
        }
    }

    #[test]
    fn test_orphaned_tracking() {
        let handler = create_test_handler();

        // Initially false and zero
        assert!(!handler.logged_orphaned_warning.load(Ordering::SeqCst));
        assert_eq!(handler.orphaned_count(), 0);

        // Simulate orphaned notifications
        handler.orphaned_count.fetch_add(1, Ordering::SeqCst);
        handler.orphaned_count.fetch_add(1, Ordering::SeqCst);
        assert_eq!(handler.orphaned_count(), 2);

        // Warning flag
        let was_logged = handler.logged_orphaned_warning.swap(true, Ordering::SeqCst);
        assert!(!was_logged);
        assert!(handler.logged_orphaned_warning.load(Ordering::SeqCst));

        // Reset works for both
        handler.reset_orphaned_tracking();
        assert!(!handler.logged_orphaned_warning.load(Ordering::SeqCst));
        assert_eq!(handler.orphaned_count(), 0);
    }
}

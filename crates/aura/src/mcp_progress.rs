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
    model::{ProgressNotificationParam, ProgressToken},
    service::{NotificationContext, RoleClient},
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use aura_events::agent::{AgentEvent, AgentEventPayload};

/// A custom ClientHandler that routes progress notifications to request-scoped channels.
///
/// This handler is used instead of `()` when creating MCP clients to enable
/// progress notification support. Progress notifications received from the
/// server are routed to the run that initiated the tool call, by the progress
/// token that call minted, so concurrent runs cannot see each other's progress.
#[derive(Clone)]
pub struct ProgressEnabledHandler {
    progress_dispatcher: ProgressDispatcher,
    /// Which run each in-flight progress token belongs to. Notifications arrive
    /// on the transport's task, which cannot read the run's task-local, so the
    /// token is the only thing tying one back to its run.
    token_owners: Arc<RwLock<HashMap<ProgressToken, String>>>,
    /// Flag to log orphaned progress only once (prevents log flood from servers ignoring cancellation)
    logged_orphaned_warning: Arc<AtomicBool>,
    /// Counter for orphaned progress notifications (for diagnostics)
    orphaned_count: Arc<AtomicU64>,
}

impl std::fmt::Debug for ProgressEnabledHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgressEnabledHandler")
            .field("progress_dispatcher", &self.progress_dispatcher)
            .field("token_owners", &"Arc<RwLock<HashMap<..>>>")
            .finish()
    }
}

impl ProgressEnabledHandler {
    /// The run a progress token belongs to, or `None` once its call has ended.
    pub async fn owner_of(&self, token: &ProgressToken) -> Option<String> {
        self.token_owners.read().await.get(token).cloned()
    }

    pub fn new(token_owners: Arc<RwLock<HashMap<ProgressToken, String>>>) -> Self {
        Self {
            progress_dispatcher: ProgressDispatcher::new(),
            token_owners,
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
            let request_id = self.owner_of(&params.progress_token).await;

            if let Some(req_id) = request_id.as_deref() {
                let routed = crate::agent_events::emit(
                    req_id,
                    AgentEvent::single_agent(AgentEventPayload::ToolProgress {
                        progress_token: params.progress_token.clone(),
                        progress: params.progress,
                        total: params.total,
                        message: params.message.clone(),
                    }),
                )
                .await;
                if routed == crate::agent_events::Routed::Delivered {
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
    use rmcp::model::NumberOrString;

    fn token(n: i64) -> ProgressToken {
        ProgressToken(NumberOrString::Number(n))
    }

    fn handler_owning(pairs: &[(i64, &str)]) -> ProgressEnabledHandler {
        let owners = pairs
            .iter()
            .map(|(t, run)| (token(*t), (*run).to_string()))
            .collect::<HashMap<_, _>>();
        ProgressEnabledHandler::new(Arc::new(RwLock::new(owners)))
    }

    #[test]
    fn test_handler_creation() {
        let handler = handler_owning(&[]);
        let _ = handler.progress_dispatcher();
    }

    #[test]
    fn test_handler_clone() {
        let handler = handler_owning(&[]);
        let cloned = handler.clone();
        let _ = cloned.progress_dispatcher();
    }

    #[tokio::test]
    async fn an_unowned_token_has_no_run() {
        assert_eq!(handler_owning(&[]).owner_of(&token(1)).await, None);
    }

    /// The ambient "current request" this replaced was last-writer-wins, so a
    /// worker's progress could be attributed to whichever run set it last.
    #[tokio::test]
    async fn concurrent_runs_route_by_their_own_token() {
        let handler = handler_owning(&[(1, "run_a"), (2, "run_b")]);

        assert_eq!(handler.owner_of(&token(1)).await.as_deref(), Some("run_a"));
        assert_eq!(handler.owner_of(&token(2)).await.as_deref(), Some("run_b"));
    }

    #[tokio::test]
    async fn a_released_token_stops_routing() {
        let owners = Arc::new(RwLock::new(HashMap::new()));
        let handler = ProgressEnabledHandler::new(owners.clone());
        owners.write().await.insert(token(7), "run_a".to_string());
        assert_eq!(handler.owner_of(&token(7)).await.as_deref(), Some("run_a"));

        owners.write().await.remove(&token(7));
        assert_eq!(handler.owner_of(&token(7)).await, None);
    }
}

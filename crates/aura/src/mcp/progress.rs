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
 * 2. The handler finds the call that owns the progress token and emits the
 *    notification on that call's run
 * 3. The run's observer, such as the web server's SSE loop, forwards it
 *
 * # Security
 *
 * Progress notifications reach only the run that made the tool call.
 * This prevents cross-customer data leakage in multi-tenant deployments.
 */

use rmcp::{
    ClientHandler,
    handler::client::progress::ProgressDispatcher,
    model::{ClientInfo, Implementation, ProgressNotificationParam, ProgressToken},
    service::{NotificationContext, RoleClient},
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{debug, warn};

use aura_events::Progress;
use aura_events::agent::{AgentEvent, AgentEventPayload};

use crate::mcp::client::CallContext;

/// How many unowned progress notifications mean a server is ignoring
/// cancellation rather than trailing one past its call.
const ORPHANED_PROGRESS_ALARM: u64 = 16;

/// A custom ClientHandler that routes progress notifications to request-scoped channels.
///
/// This handler is used instead of `()` when creating MCP clients to enable
/// progress notification support. Progress notifications received from the
/// server are routed to the call that initiated them, by the progress token
/// that call minted, so concurrent runs cannot see each other's progress.
#[derive(Clone)]
pub struct ProgressEnabledHandler {
    progress_dispatcher: ProgressDispatcher,
    /// Which call each in-flight progress token belongs to.
    token_owners: Arc<std::sync::Mutex<HashMap<ProgressToken, CallContext>>>,
    /// The call this handler's client serves, shared with it.
    bound_call: Arc<tokio::sync::RwLock<Option<CallContext>>>,
    /// Counter for orphaned progress notifications (for diagnostics)
    orphaned_count: Arc<AtomicU64>,
    /// This client's MCP `clientInfo`.
    client_info: ClientInfo,
}

impl std::fmt::Debug for ProgressEnabledHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgressEnabledHandler")
            .field("progress_dispatcher", &self.progress_dispatcher)
            .field("token_owners", &"Arc<Mutex<HashMap<..>>>")
            .finish()
    }
}

impl ProgressEnabledHandler {
    /// The call a progress token belongs to, or `None` once that call has ended.
    /// Notifications arrive on the transport's task, which cannot read the
    /// run's task-local, so the token is the only thing tying one back.
    ///
    /// rmcp mints a token inside the send, so a server can answer before the
    /// call has claimed it. The client serves one call, which is whose that
    /// notification is.
    pub async fn owner_of(&self, token: &ProgressToken) -> Option<CallContext> {
        let owned = self
            .token_owners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(token)
            .cloned();
        match owned {
            Some(call) => Some(call),
            None => self.bound_call.read().await.clone(),
        }
    }

    /// `user_agent` is the same `product/version` token sent as the HTTP
    /// `User-Agent` header; the handshake announces it split into
    /// `clientInfo.name` and `clientInfo.version`, the shape MCP servers
    /// expect there.
    pub fn new(
        token_owners: Arc<std::sync::Mutex<HashMap<ProgressToken, CallContext>>>,
        bound_call: Arc<tokio::sync::RwLock<Option<CallContext>>>,
        user_agent: &str,
    ) -> Self {
        Self {
            progress_dispatcher: ProgressDispatcher::new(),
            token_owners,
            bound_call,
            orphaned_count: Arc::new(AtomicU64::new(0)),
            client_info: ClientInfo {
                client_info: implementation_from_user_agent(user_agent),
                ..ClientInfo::default()
            },
        }
    }

    /// Reset the orphaned counter (call when setting a new request ID)
    pub fn reset_orphaned_tracking(&self) {
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

/// The product page announced as `clientInfo.websiteUrl`, which MCP servers
/// that track clients link back to.
const AURA_WEBSITE_URL: &str = "https://www.mezmo.com/aura";

/// Split a `product/version` user-agent token into MCP `clientInfo`. A token
/// with no version part names the product alone and reports the running crate
/// version, so a bare deployment tag still carries a real version.
fn implementation_from_user_agent(user_agent: &str) -> Implementation {
    let (name, version) = user_agent.split_once('/').unwrap_or((user_agent, ""));
    let version = match version.trim() {
        "" => env!("CARGO_PKG_VERSION"),
        version => version,
    };
    Implementation {
        name: name.trim().to_owned(),
        version: version.to_owned(),
        website_url: Some(AURA_WEBSITE_URL.to_owned()),
        ..Implementation::default()
    }
}

impl ClientHandler for ProgressEnabledHandler {
    fn get_info(&self) -> ClientInfo {
        self.client_info.clone()
    }

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
            // One lookup, so the id and the agent describe the same call.
            let call = self.owner_of(&params.progress_token).await;

            if let Some(CallContext { run, agent }) = call {
                let req_id = run.id();

                let routed = run
                    .emit(AgentEvent::new(
                        agent,
                        AgentEventPayload::ToolProgress {
                            progress_token: params.progress_token.clone(),
                            progress: Progress {
                                current: params.progress,
                                total: params.total,
                            },
                            message: params.message.clone(),
                        },
                    ))
                    .await;
                if routed {
                    debug!(
                        "Progress notification routed to request '{}': progress={}, message={:?}",
                        req_id, params.progress, params.message
                    );
                }
            } else {
                // Neither the token nor the client names a call, so the
                // notification has no run to reach. Cancelling a call clears
                // both, and a client that never bound one never had either.
                let count = self.orphaned_count.fetch_add(1, Ordering::SeqCst) + 1;

                // A stream of these means the server kept going after being
                // told to stop.
                if count == ORPHANED_PROGRESS_ALARM {
                    warn!(
                        "{} MCP progress notifications with no live call — the server \
                         may be ignoring notifications/cancelled",
                        count
                    );
                }

                debug!(
                    "Unowned MCP progress #{}: progress={}, message={:?}",
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

    /// The same label always names the same run.
    fn run_id(label: &str) -> crate::domain::RequestId {
        crate::domain::RequestId::for_a2a_task(label)
    }

    fn call(label: &str) -> CallContext {
        CallContext {
            run: crate::run_context::RunContext::detached(run_id(label)),
            agent: aura_events::AgentContext::single_agent(),
        }
    }

    fn handler_owning(pairs: &[(i64, &str)]) -> ProgressEnabledHandler {
        let owners = pairs
            .iter()
            .map(|(t, run)| (token(*t), call(run)))
            .collect::<HashMap<_, _>>();
        ProgressEnabledHandler::new(
            Arc::new(std::sync::Mutex::new(owners)),
            Arc::new(tokio::sync::RwLock::new(None)),
            "test/0",
        )
    }

    fn create_test_handler() -> ProgressEnabledHandler {
        handler_owning(&[])
    }

    /// A `product/version` token lands as separate name and version fields.
    #[test]
    fn handshake_info_splits_the_user_agent_into_name_and_version() {
        let handler = ProgressEnabledHandler::new(
            Arc::new(std::sync::Mutex::new(HashMap::new())),
            Arc::new(tokio::sync::RwLock::new(None)),
            "aura/1.2.3",
        );
        let info = handler.get_info().client_info;
        assert_eq!(info.name, "aura");
        assert_eq!(info.version, "1.2.3");
        assert_eq!(info.website_url.as_deref(), Some(AURA_WEBSITE_URL));
    }

    /// A bare product name still reports the version that is actually running.
    #[test]
    fn handshake_info_falls_back_to_the_crate_version() {
        for token in ["mezmo-aura", "mezmo-aura/", " mezmo-aura / "] {
            let handler = ProgressEnabledHandler::new(
                Arc::new(std::sync::Mutex::new(HashMap::new())),
                Arc::new(tokio::sync::RwLock::new(None)),
                token,
            );
            let info = handler.get_info().client_info;
            assert_eq!(info.name, "mezmo-aura", "token {token:?}");
            assert_eq!(info.version, env!("CARGO_PKG_VERSION"), "token {token:?}");
        }
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
        assert!(handler_owning(&[]).owner_of(&token(1)).await.is_none());
    }

    /// rmcp mints a progress token inside the send, so a server can answer
    /// before the call has claimed it. The client serves one call, so that
    /// notification routes to it rather than being dropped.
    #[tokio::test]
    async fn a_token_claimed_after_its_first_notification_still_routes() {
        let owners = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let bound = Arc::new(tokio::sync::RwLock::new(Some(call("run_a"))));
        let handler = ProgressEnabledHandler::new(owners.clone(), bound, "test/0");

        // Nothing owns the token yet: the send has returned but the claim has not
        // landed.
        assert_eq!(
            handler
                .owner_of(&token(1))
                .await
                .map(|call| call.run.id().to_string()),
            Some(run_id("run_a").to_string()),
            "an unclaimed token belongs to the call this client serves"
        );

        // Once claimed, the token answers for itself.
        owners.lock().unwrap().insert(token(1), call("run_a_tool"));
        assert_eq!(
            handler
                .owner_of(&token(1))
                .await
                .map(|call| call.run.id().to_string()),
            Some(run_id("run_a_tool").to_string()),
            "a claim is more precise than the binding"
        );
    }

    /// Each token keeps its own call, so concurrent runs cannot pick up each
    /// other's progress.
    #[tokio::test]
    async fn concurrent_runs_route_by_their_own_token() {
        let handler = handler_owning(&[(1, "run_a"), (2, "run_b")]);

        assert_eq!(
            handler
                .owner_of(&token(1))
                .await
                .map(|c| c.run.id().to_string()),
            Some(run_id("run_a").to_string())
        );
        assert_eq!(
            handler
                .owner_of(&token(2))
                .await
                .map(|c| c.run.id().to_string()),
            Some(run_id("run_b").to_string())
        );
    }

    /// A call releases its token when it ends, so a notification trailing past
    /// the result falls back to the call the client serves and still reaches
    /// that run. Routing stops only once nothing names a call at all.
    #[tokio::test]
    async fn a_released_token_routes_through_the_binding() {
        let owners = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let bound = Arc::new(tokio::sync::RwLock::new(Some(call("run_a"))));
        let handler = ProgressEnabledHandler::new(owners.clone(), Arc::clone(&bound), "test/0");

        owners.lock().unwrap().insert(token(7), call("run_a_tool"));
        assert_eq!(
            handler
                .owner_of(&token(7))
                .await
                .map(|c| c.run.id().to_string()),
            Some(run_id("run_a_tool").to_string())
        );

        owners.lock().unwrap().remove(&token(7));
        assert_eq!(
            handler
                .owner_of(&token(7))
                .await
                .map(|c| c.run.id().to_string()),
            Some(run_id("run_a").to_string()),
            "the binding outlives the tokens of the calls it serves"
        );

        *bound.write().await = None;
        assert!(handler.owner_of(&token(7)).await.is_none());
    }

    #[test]
    fn test_orphaned_tracking() {
        let handler = create_test_handler();

        // Initially false and zero
        assert_eq!(handler.orphaned_count(), 0);

        // Simulate orphaned notifications
        handler.orphaned_count.fetch_add(1, Ordering::SeqCst);
        handler.orphaned_count.fetch_add(1, Ordering::SeqCst);
        assert_eq!(handler.orphaned_count(), 2);

        // Reset works for both
        handler.reset_orphaned_tracking();
        assert_eq!(handler.orphaned_count(), 0);
    }
}

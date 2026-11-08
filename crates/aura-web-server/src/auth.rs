//! HTTP API-key authentication for the web server.
//!
//! Built from the `[auth]` table of any loaded agent config (see
//! [`aura_config::AuthConfig`]). When no config defines `[auth]` (or every
//! table sets `mode = "off"`), the middleware is a pass-through and the
//! server behaves exactly as before.
//!
//! When enabled, every route except the open set (`/health`,
//! `/.well-known/agent-card.json`, and `/v1/approvals/*`, which carries its
//! own HMAC posture) requires a `Authorization: Bearer <key>` (or
//! `x-api-key`) header whose SHA-256 matches a configured key hash. On
//! success an [`AuthContext`] is attached to the request so handlers can
//! enforce per-key agent allowlists and namespace sessions per principal.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tracing::warn;

use aura_config::{AuthMode, Config};

use crate::types::AppState;

/// The authenticated principal for one request. Inserted into request
/// extensions by [`require_auth`] when authentication is enabled.
#[derive(Debug, Clone)]
pub struct AuthContext {
    /// The `[[auth.keys]]` name that matched.
    pub name: String,
    /// Agent names/aliases this key may invoke; `"*"` allows all.
    pub allowed_agents: Vec<String>,
}

impl AuthContext {
    /// Whether this principal may invoke the agent identified by `agent_id`.
    pub fn allows(&self, agent_id: &str) -> bool {
        self.allowed_agents
            .iter()
            .any(|a| a == "*" || a == agent_id)
    }
}

/// One keyring entry: a principal and the 32-byte SHA-256 digests it accepts
/// (current plus, during rotation, previous).
#[derive(Debug)]
struct KeyEntry {
    name: String,
    hashes: Vec<[u8; 32]>,
    allowed_agents: Vec<String>,
}

/// The compiled keyring, built once at startup from all loaded configs.
#[derive(Debug)]
pub struct KeyRing {
    entries: Vec<KeyEntry>,
}

/// Routes that stay reachable without a key when auth is enabled:
/// - `/health`: container probes must not need credentials.
/// - `/.well-known/agent-card.json`: A2A discovery convention.
/// - `/v1/approvals/*`: approvers are a different principal than chat
///   callers; this route keeps its own HMAC verification in the handler.
fn is_open_path(path: &str) -> bool {
    path == "/health"
        || path == "/.well-known/agent-card.json"
        || path.starts_with("/v1/approvals/")
}

impl KeyRing {
    /// Build the deployment keyring from every loaded config. Keys from
    /// multiple config files are merged into one ring; duplicate principal
    /// names across files are rejected at startup.
    ///
    /// Returns `Ok(None)` when no config enables auth.
    pub fn from_configs(configs: &[Config]) -> Result<Option<Self>, String> {
        let mut entries = Vec::new();
        let mut enabled = false;
        for config in configs {
            let Some(auth) = &config.auth else {
                continue;
            };
            if auth.mode == AuthMode::Off {
                continue;
            }
            enabled = true;
            auth.validate().map_err(|e| e.to_string())?;
            for key in &auth.keys {
                if entries.iter().any(|e: &KeyEntry| e.name == key.name) {
                    return Err(format!(
                        "duplicate [auth.keys] name '{}' across loaded configs",
                        key.name
                    ));
                }
                entries.push(KeyEntry {
                    name: key.name.clone(),
                    hashes: key_hashes(key),
                    allowed_agents: key.agents.clone(),
                });
            }
        }
        if !enabled {
            return Ok(None);
        }
        Ok(Some(Self { entries }))
    }

    /// Number of principals in the ring (for startup logging).
    pub fn principal_count(&self) -> usize {
        self.entries.len()
    }

    /// Hash a raw candidate key and constant-time compare it against every
    /// stored digest. Returns the matching principal on success.
    pub fn authenticate(&self, raw: &str) -> Option<AuthContext> {
        let candidate = sha256(raw.as_bytes());
        for entry in &self.entries {
            for hash in &entry.hashes {
                if ct_eq(hash, &candidate) {
                    return Some(AuthContext {
                        name: entry.name.clone(),
                        allowed_agents: entry.allowed_agents.clone(),
                    });
                }
            }
        }
        None
    }
}

fn key_hashes(key: &aura_config::ApiKeyConfig) -> Vec<[u8; 32]> {
    // Config validation already guarantees both decode; re-decode defensively
    // and skip anything malformed rather than panic at request time.
    let mut hashes = Vec::with_capacity(2);
    if let Ok(h) = aura_config::auth::decode_key_hash(&key.key_hash) {
        hashes.push(h);
    }
    if let Some(prev) = &key.key_hash_previous
        && let Ok(h) = aura_config::auth::decode_key_hash(prev)
    {
        hashes.push(h);
    }
    hashes
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Length-then-XOR constant-time comparison. Both sides are fixed-size
/// digests, so the length branch never leaks.
fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// OpenAI-compatible 401/403 error envelope so OpenAI clients surface the
/// problem instead of choking on an unknown shape.
#[derive(Serialize)]
struct AuthErrorEnvelope {
    error: AuthErrorDetail,
}

#[derive(Serialize)]
struct AuthErrorDetail {
    message: String,
    #[serde(rename = "type")]
    error_type: String,
    param: Option<String>,
    code: String,
}

fn auth_error(status: StatusCode, message: &str, code: &str) -> Response {
    (
        status,
        Json(AuthErrorEnvelope {
            error: AuthErrorDetail {
                message: message.to_string(),
                error_type: "authentication_error".to_string(),
                param: None,
                code: code.to_string(),
            },
        }),
    )
        .into_response()
}

/// Extract the candidate key from `Authorization: Bearer` (the form every
/// OpenAI-compatible client sends) or the `x-api-key` fallback used by some
/// gateways.
fn candidate_key(headers: &HeaderMap) -> Option<&str> {
    if let Some(auth) = headers.get(axum::http::header::AUTHORIZATION) {
        let value = auth.to_str().ok()?;
        let (scheme, rest) = value.split_once(' ')?;
        if scheme.eq_ignore_ascii_case("bearer") && !rest.trim().is_empty() {
            return Some(rest.trim());
        }
    }
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

/// Middleware: reject unauthenticated requests when the deployment has an
/// `[auth]` keyring. Runs inside `shutdown_guard` so draining still answers
/// 503 before auth challenges.
pub async fn require_auth(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(ring) = &state.auth else {
        return next.run(request).await;
    };
    if is_open_path(request.uri().path()) {
        return next.run(request).await;
    }
    let outcome = candidate_key(request.headers()).and_then(|key| ring.authenticate(key));
    match outcome {
        Some(ctx) => {
            request.extensions_mut().insert(ctx);
            next.run(request).await
        }
        None => {
            warn!(
                path = request.uri().path(),
                "rejected unauthenticated request"
            );
            auth_error(
                StatusCode::UNAUTHORIZED,
                "Invalid API key provided. Provide your key via the `Authorization: Bearer` header.",
                "invalid_api_key",
            )
        }
    }
}

/// The OpenAI-compatible 403 body for a key that is valid but not allowed to
/// invoke the requested agent.
pub fn forbidden_response(key_name: &str, agent_id: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(AuthErrorEnvelope {
            error: AuthErrorDetail {
                message: format!(
                    "The API key `{key_name}` does not have access to the model `{agent_id}`."
                ),
                error_type: "permission_error".to_string(),
                param: Some("model".to_string()),
                code: "model_not_found".to_string(),
            },
        }),
    )
        .into_response()
}

/// Compile the `[auth]` tables of all loaded configs into the deployment
/// keyring, warning once about mixed-mode setups.
pub fn build_keyring(configs: &[Config]) -> Result<Option<Arc<KeyRing>>, String> {
    let modes: Vec<AuthMode> = configs
        .iter()
        .filter_map(|c| c.auth.as_ref().map(|a| a.mode))
        .collect();
    if modes.contains(&AuthMode::Off) && modes.contains(&AuthMode::ApiKeys) {
        warn!(
            "some configs set [auth] mode = \"off\" while others enable api-keys; \
             auth is enabled for the whole server"
        );
    }
    KeyRing::from_configs(configs).map(|ring| ring.map(Arc::new))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streaming::ToolResultMode;
    use crate::types::{ActiveRequestTracker, AppState};
    use aura_config::AuthConfig;
    use axum::Router;
    use axum::routing::get;
    use tower::ServiceExt;

    const RAW_KEY: &str = "aura-test-key-0001";

    fn auth_config(mode: AuthMode, keys: Vec<aura_config::ApiKeyConfig>) -> AuthConfig {
        AuthConfig { mode, keys }
    }

    fn key(name: &str, raw: &str, agents: Vec<String>) -> aura_config::ApiKeyConfig {
        aura_config::ApiKeyConfig {
            name: name.to_string(),
            key_hash: aura_config::hash_api_key(raw),
            key_hash_previous: None,
            agents,
        }
    }

    fn config_with_auth(auth: Option<AuthConfig>) -> Config {
        Config {
            auth,
            ..Config::default()
        }
    }

    fn test_state(auth: Option<Arc<KeyRing>>) -> Arc<AppState> {
        Arc::new(AppState {
            configs: Arc::new(vec![]),
            tool_result_mode: ToolResultMode::default(),
            tool_result_max_length: 0,
            streaming_buffer_size: 0,
            aura_custom_events: false,
            aura_emit_reasoning: false,
            debug_provider_errors: false,
            streaming_timeout_secs: 0,
            first_chunk_timeout_secs: 0,
            stream_inactivity_timeout_secs: 0,
            shutdown_token: tokio_util::sync::CancellationToken::new(),
            stream_shutdown_token: tokio_util::sync::CancellationToken::new(),
            active_requests: Arc::new(ActiveRequestTracker::default()),
            default_agent: None,
            additional_tools: Arc::new(Vec::new),
            slack_api: None,
            pending_approvals: aura::hitl::PendingApprovals::new(),
            hitl_webhook_hmac: None,
            session_store: Arc::new(crate::session_store::InMemorySessionStore::new()),
            auth,
        })
    }

    /// Minimal app: middleware + a probe route that echoes the principal.
    fn app_with_auth(state: Arc<AppState>) -> Router {
        async fn probe(ext: Option<axum::Extension<AuthContext>>) -> String {
            ext.map(|e| e.name.clone())
                .unwrap_or_else(|| "anonymous".to_string())
        }
        Router::new()
            .route("/v1/models", get(probe))
            .route("/health", get(probe))
            .route("/v1/approvals/{id}", get(probe))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                require_auth,
            ))
            .with_state(state)
    }

    fn request(path: &str, headers: &[(&str, String)]) -> Request {
        let mut builder = axum::http::Request::builder().method("GET").uri(path);
        for (k, v) in headers {
            builder = builder.header(*k, v);
        }
        builder.body(axum::body::Body::empty()).unwrap()
    }

    #[test]
    fn no_auth_table_disables_the_ring() {
        let configs = vec![config_with_auth(None)];
        assert!(build_keyring(&configs).unwrap().is_none());
        let configs = vec![config_with_auth(Some(auth_config(AuthMode::Off, vec![])))];
        assert!(build_keyring(&configs).unwrap().is_none());
    }

    #[test]
    fn authenticates_current_and_previous_hashes() {
        let mut k = key("ops", RAW_KEY, vec!["*".to_string()]);
        k.key_hash_previous = Some(aura_config::hash_api_key("old-key"));
        let ring = KeyRing {
            entries: vec![KeyEntry {
                name: k.name.clone(),
                hashes: key_hashes(&k),
                allowed_agents: k.agents.clone(),
            }],
        };
        assert_eq!(ring.authenticate(RAW_KEY).unwrap().name, "ops");
        assert_eq!(ring.authenticate("old-key").unwrap().name, "ops");
        assert!(ring.authenticate("wrong").is_none());
        assert!(ring.authenticate("").is_none());
    }

    #[test]
    fn allowlist_matching() {
        let ctx = AuthContext {
            name: "k".into(),
            allowed_agents: vec!["incident-response".into()],
        };
        assert!(ctx.allows("incident-response"));
        assert!(!ctx.allows("other"));
        let all = AuthContext {
            name: "k".into(),
            allowed_agents: vec!["*".into()],
        };
        assert!(all.allows("anything"));
    }

    #[test]
    fn duplicate_names_across_configs_rejected() {
        let configs = vec![
            config_with_auth(Some(auth_config(
                AuthMode::ApiKeys,
                vec![key("dup", RAW_KEY, vec!["*".into()])],
            ))),
            config_with_auth(Some(auth_config(
                AuthMode::ApiKeys,
                vec![key("dup", "other-raw", vec!["*".into()])],
            ))),
        ];
        let err = build_keyring(&configs).unwrap_err();
        assert!(err.contains("duplicate"), "{err}");
    }

    #[tokio::test]
    async fn middleware_disabled_passes_through() {
        let state = test_state(None);
        let resp = app_with_auth(state)
            .oneshot(request("/v1/models", &[]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn middleware_rejects_missing_and_wrong_keys() {
        let ring = build_keyring(&[config_with_auth(Some(auth_config(
            AuthMode::ApiKeys,
            vec![key("ops", RAW_KEY, vec!["*".into()])],
        )))])
        .unwrap()
        .unwrap();
        let state = test_state(Some(ring));
        let app = app_with_auth(state);

        let resp = app
            .clone()
            .oneshot(request("/v1/models", &[]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let resp = app
            .clone()
            .oneshot(request(
                "/v1/models",
                &[("authorization", "Bearer nope".to_string())],
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Non-bearer schemes are not accepted.
        let resp = app
            .oneshot(request(
                "/v1/models",
                &[("authorization", format!("Basic {RAW_KEY}"))],
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn middleware_accepts_bearer_and_x_api_key() {
        let ring = build_keyring(&[config_with_auth(Some(auth_config(
            AuthMode::ApiKeys,
            vec![key("ops", RAW_KEY, vec!["*".into()])],
        )))])
        .unwrap()
        .unwrap();
        let state = test_state(Some(ring));
        let app = app_with_auth(state);

        let resp = app
            .clone()
            .oneshot(request(
                "/v1/models",
                &[("authorization", format!("Bearer {RAW_KEY}"))],
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app
            .oneshot(request("/v1/models", &[("x-api-key", RAW_KEY.to_string())]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn open_paths_stay_reachable() {
        let ring = build_keyring(&[config_with_auth(Some(auth_config(
            AuthMode::ApiKeys,
            vec![key("ops", RAW_KEY, vec!["*".into()])],
        )))])
        .unwrap()
        .unwrap();
        let state = test_state(Some(ring));
        let app = app_with_auth(state);

        let resp = app.clone().oneshot(request("/health", &[])).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = app
            .oneshot(request("/v1/approvals/dec_1", &[]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn auth_context_reaches_handlers() {
        let ring = build_keyring(&[config_with_auth(Some(auth_config(
            AuthMode::ApiKeys,
            vec![key("ops", RAW_KEY, vec!["*".into()])],
        )))])
        .unwrap()
        .unwrap();
        let state = test_state(Some(ring));
        let resp = app_with_auth(state)
            .oneshot(request(
                "/v1/models",
                &[("authorization", format!("Bearer {RAW_KEY}"))],
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"ops");
    }

    #[test]
    fn ct_eq_boundaries() {
        let a = sha256(b"a");
        let mut b = a;
        assert!(ct_eq(&a, &b));
        b[31] ^= 1;
        assert!(!ct_eq(&a, &b));
    }
}

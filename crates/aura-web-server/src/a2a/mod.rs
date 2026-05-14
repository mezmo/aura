mod handler;
pub mod overrides;

pub use handler::AuraMessageHandler;

use a2a_rs_server::AuthContext;
use axum::http::HeaderMap;
use std::env;

/// Build an `AuthContext` from an HTTP request's headers.
///
/// Used by both the `A2aServer` `auth_extractor` closure (for the upstream JSON-RPC and
/// unmodified REST endpoints) and the local override handlers in [`overrides`] (which serve
/// `:stream` and `:subscribe` with the race fix applied).
///
/// `A2A_HEADER_USER_ID` and `A2A_HEADER_AUTHORIZATION` name which headers to pull the user id
/// and access token from. `A2A_HEADER_AUTHORIZATION_STRIP_PREFIX`, if set, strips the prefix
/// (e.g. `"Bearer "`) from the authorization value.
///
/// All request headers are also stashed into `metadata` so downstream tool calls (which read
/// `auth.metadata` in `AuraMessageHandler`) can forward them to MCP servers.
pub fn extract_auth_context(headers: &HeaderMap) -> Option<AuthContext> {
    let header_map: serde_json::Map<String, serde_json::Value> = headers
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|val| (k.to_string(), serde_json::Value::String(val.to_string())))
        })
        .collect();

    let user_id = env::var("A2A_HEADER_USER_ID")
        .ok()
        .as_ref()
        .and_then(|h| headers.get(h))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let access_token = env::var("A2A_HEADER_AUTHORIZATION")
        .ok()
        .as_ref()
        .and_then(|h| headers.get(h))
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            if let Some(prefix) = env::var("A2A_HEADER_AUTHORIZATION_STRIP_PREFIX")
                .ok()
                .as_deref()
            {
                s.strip_prefix(prefix).unwrap_or(s).to_string()
            } else {
                s.to_string()
            }
        })
        .unwrap_or_default();

    Some(AuthContext {
        user_id,
        access_token,
        metadata: Some(serde_json::Value::Object(header_map)),
    })
}

//! Shared rendering of the `aura.mcp_status` SSE event into user-facing
//! notices.
//!
//! Both the REPL (status area, styled) and one-shot mode (stderr) surface MCP
//! connection problems. This module centralises the parsing and message
//! wording so the two stay consistent — callers only decide presentation
//! (color/prefix vs. plain stderr line).

/// Marker the `aura` transport layer prepends to an HTTP status in the failure
/// reason (e.g. `"server returned HTTP 404 Not Found"`). Shared from
/// [`aura_events`] so producer and consumer match on a single source of truth —
/// because it's **our** string and not a Rig/rmcp error format, it's stable
/// across Rig fork bumps. The only upstream-owned fragment we parse is reqwest's
/// `HTTP status ... (<status>)` `Display` output, used as a fallback in
/// [`extract_http_status`].
use aura_events::HTTP_STATUS_MARKER;

/// A single user-facing notice derived from an `aura.mcp_status` event.
///
/// The variant carries the severity; the payload is the human-readable message
/// body, which does **not** include an `error:`/`warning:` prefix — each caller
/// adds its own prefix and styling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpNotice {
    Error(String),
    Warning(String),
}

impl McpNotice {
    /// The message body, regardless of severity.
    pub fn message(&self) -> &str {
        match self {
            McpNotice::Error(message) | McpNotice::Warning(message) => message,
        }
    }
}

/// Human-friendly label for an MCP transport identifier from the event.
fn transport_display(transport: &str) -> &str {
    match transport {
        "http_streamable" => "HTTP",
        "sse" => "SSE",
        "stdio" => "STDIO",
        other => other,
    }
}

/// Extract a concise HTTP status (e.g. "404 Not Found") from a verbose error
/// chain, if one is present.
///
/// The server collapses the transport error chain into the reason string, which
/// for an HTTP failure looks like:
///   "... server returned HTTP 404 Not Found: Failed to establish ...
///    Client error: HTTP status client error (404 Not Found) for url (...)"
///
/// We prefer the `server returned HTTP <status>` fragment (injected by AURA at
/// the transport layer, so it's clean), then fall back to reqwest's
/// `HTTP status ... (<status>)` form.
fn extract_http_status(reason: &str) -> Option<String> {
    if let Some(idx) = reason.find(HTTP_STATUS_MARKER) {
        let rest = &reason[idx + HTTP_STATUS_MARKER.len()..];
        // The status runs up to the next chain separator (": ").
        let end = rest.find(": ").unwrap_or(rest.len());
        let status = rest[..end].trim();
        if !status.is_empty() {
            return Some(status.to_string());
        }
    }
    // Fallback: reqwest's "HTTP status client error (404 Not Found)".
    if let Some(idx) = reason.find("HTTP status ") {
        let rest = &reason[idx..];
        if let (Some(open), Some(close)) = (rest.find('('), rest.find(')'))
            && open < close
        {
            let status = rest[open + 1..close].trim();
            if !status.is_empty() {
                return Some(status.to_string());
            }
        }
    }
    None
}

/// Build a one-line failure summary for a degraded MCP server.
///
/// Prefers signal over completeness: when the (often very verbose) reason chain
/// carries an HTTP status, collapse to just `<transport> MCP server '<name>':
/// <status>` (e.g. "HTTP MCP server 'github': 404 Not Found"). The full chain
/// is still available in the SSE stream panel for debugging.
///
/// Otherwise the reason is usually self-describing (e.g. the 401 auth message),
/// so it's shown verbatim once the redundant "Connection failed: " prefix is
/// stripped; the server name/transport are prepended only when the reason
/// doesn't already name the server.
fn failure_summary(transport_display: &str, name: &str, reason: &str) -> String {
    let reason = reason
        .strip_prefix("Connection failed: ")
        .unwrap_or(reason)
        .trim();
    if let Some(status) = extract_http_status(reason) {
        format!("{transport_display} MCP server '{name}': {status}")
    } else if reason.is_empty() {
        format!("Failed to connect to {transport_display} MCP server '{name}'")
    } else if reason.contains(name) {
        reason.to_string()
    } else {
        format!("{transport_display} MCP server '{name}': {reason}")
    }
}

/// Parse an `aura.mcp_status` event payload into user-facing notices.
///
/// Emits at most one notice per server:
/// - a failed server → an error notice carrying the failure reason;
/// - a connected server exposing zero tools → a warning notice;
/// - everything else (connected with tools, not attempted) → nothing.
///
/// A failed server never also produces a "no tools" warning — the connection
/// failure is the actionable problem and no tools are expected when it fails.
pub fn notices_from_event(val: &serde_json::Value) -> Vec<McpNotice> {
    let Some(servers) = val.get("servers").and_then(|s| s.as_array()) else {
        return Vec::new();
    };

    servers
        .iter()
        .filter_map(|s| {
            let name = s
                .get("server_name")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let transport = s
                .get("transport")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let status = s.get("status").and_then(|v| v.as_str()).unwrap_or_default();
            let tools = s
                .get("tools_count")
                .and_then(|v| v.as_u64())
                .unwrap_or_default();
            let disp = transport_display(transport);

            if status == "failed" {
                let reason = s.get("reason").and_then(|v| v.as_str()).unwrap_or_default();
                Some(McpNotice::Error(failure_summary(disp, name, reason)))
            } else if status == "connected" && tools == 0 {
                Some(McpNotice::Warning(format!(
                    "{disp} MCP server '{name}' connected but reported no tools available"
                )))
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(servers: serde_json::Value) -> serde_json::Value {
        json!({ "servers": servers, "session_id": "s1" })
    }

    #[test]
    fn failed_server_yields_error_with_reason_and_no_tools_warning() {
        let notices = notices_from_event(&event(json!([{
            "server_name": "github",
            "transport": "http_streamable",
            "status": "failed",
            "tools_count": 0,
            "reason": "Connection failed: HTTP MCP server 'github' authentication failed (401 Unauthorized)."
        }])));
        // Exactly one notice: the error. No redundant "no tools" warning.
        assert_eq!(notices.len(), 1);
        assert!(matches!(notices[0], McpNotice::Error(_)));
        // "Connection failed: " prefix stripped; reason shown verbatim (it
        // already names the server, so no duplicate prefix is added).
        assert_eq!(
            notices[0].message(),
            "HTTP MCP server 'github' authentication failed (401 Unauthorized)."
        );
    }

    #[test]
    fn failed_server_without_name_in_reason_gets_prefixed() {
        let notices = notices_from_event(&event(json!([{
            "server_name": "kb",
            "transport": "sse",
            "status": "failed",
            "tools_count": 0,
            "reason": "Connection failed: transport closed"
        }])));
        assert_eq!(
            notices[0].message(),
            "SSE MCP server 'kb': transport closed"
        );
    }

    #[test]
    fn failed_server_without_reason_falls_back_to_generic() {
        let notices = notices_from_event(&event(json!([{
            "server_name": "kb",
            "transport": "stdio",
            "status": "failed",
            "tools_count": 0
        }])));
        assert_eq!(
            notices[0].message(),
            "Failed to connect to STDIO MCP server 'kb'"
        );
    }

    #[test]
    fn connected_empty_server_yields_warning_only() {
        let notices = notices_from_event(&event(json!([{
            "server_name": "mezmo",
            "transport": "http_streamable",
            "status": "connected",
            "tools_count": 0
        }])));
        assert_eq!(notices.len(), 1);
        assert!(matches!(notices[0], McpNotice::Warning(_)));
        assert_eq!(
            notices[0].message(),
            "HTTP MCP server 'mezmo' connected but reported no tools available"
        );
    }

    #[test]
    fn connected_with_tools_and_missing_servers_yield_nothing() {
        let connected = notices_from_event(&event(json!([{
            "server_name": "mezmo",
            "transport": "http_streamable",
            "status": "connected",
            "tools_count": 7
        }])));
        assert!(connected.is_empty());
        assert!(notices_from_event(&json!({ "session_id": "s1" })).is_empty());
    }

    #[test]
    fn verbose_http_status_chain_is_condensed_to_signal() {
        // The exact verbose reason a real 404 produces — should collapse to the
        // status alone, not echo the whole transport chain.
        let reason = "Connection failed: MCP initialization error: Failed to connect to \
             HTTP MCP server 'github': server returned HTTP 404 Not Found: Failed to \
             establish MCP client connection: Send message error Transport [...] error: \
             Client error: HTTP status client error (404 Not Found) for url \
             (https://api.githubcopilot.com/mcpz), when send initialize request";
        let notices = notices_from_event(&event(json!([{
            "server_name": "github",
            "transport": "http_streamable",
            "status": "failed",
            "tools_count": 0,
            "reason": reason,
        }])));
        assert_eq!(notices.len(), 1);
        assert!(matches!(notices[0], McpNotice::Error(_)));
        assert_eq!(
            notices[0].message(),
            "HTTP MCP server 'github': 404 Not Found"
        );
    }

    #[test]
    fn http_status_extracted_via_reqwest_fallback() {
        // No "server returned HTTP" marker (e.g. a discover-tools failure) —
        // fall back to reqwest's parenthesised status.
        let reason = "Connection failed: Failed to discover tools from server 'gh': \
             Client error: HTTP status client error (403 Forbidden) for url (https://x/y)";
        let notices = notices_from_event(&event(json!([{
            "server_name": "gh",
            "transport": "http_streamable",
            "status": "failed",
            "tools_count": 0,
            "reason": reason,
        }])));
        assert_eq!(notices[0].message(), "HTTP MCP server 'gh': 403 Forbidden");
    }

    #[test]
    fn auth_401_message_is_left_verbatim() {
        // The friendly 401 message has no HTTP-status marker, so it should pass
        // through unchanged rather than being collapsed.
        let reason = "Connection failed: HTTP MCP server 'github' authentication failed \
             (401 Unauthorized). Check that your forwarded headers and credentials are correct.";
        let notices = notices_from_event(&event(json!([{
            "server_name": "github",
            "transport": "http_streamable",
            "status": "failed",
            "tools_count": 0,
            "reason": reason,
        }])));
        assert_eq!(
            notices[0].message(),
            "HTTP MCP server 'github' authentication failed (401 Unauthorized). \
             Check that your forwarded headers and credentials are correct."
        );
    }
}

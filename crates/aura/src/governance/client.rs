//! Governance catalog webhook delivery.
//!
//! Handles POSTing the catalog envelope to the configured webhook, including
//! optional HMAC signing. Follows the same patterns as HITL webhook delivery.

use aura_config::CatalogWebhookConfig;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

use super::envelope::CatalogEnvelope;
use crate::hitl::{PrimarySecret, SigningContext, Tolerance, WebhookHmac};

/// Maximum time to wait for a TCP connection before failing.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Errors that can occur during catalog delivery.
#[derive(Debug, thiserror::Error)]
pub enum DeliveryError {
    #[error("governance webhook transport error: {0}")]
    Transport(String),
    #[error("governance webhook returned status {status}: {body}")]
    BadStatus { status: u16, body: String },
    #[error("governance webhook HMAC signing failed: {0}")]
    Signing(String),
    #[error("governance webhook HMAC misconfigured: {0}")]
    Misconfigured(String),
    #[error("governance catalog webhook not configured")]
    NotConfigured,
}

/// Result of a successful catalog delivery.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeliveryReport {
    /// The event ID that was delivered.
    pub event_id: String,
    /// HTTP status code from the webhook.
    pub status_code: u16,
    /// Response body from the webhook (truncated if large).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_body: Option<String>,
    /// Number of agents in the catalog.
    pub agents_count: usize,
    /// Total number of MCP servers across all agents.
    pub servers_count: usize,
    /// Total number of tools across all connected servers.
    pub tools_count: usize,
}

/// Deliver a catalog envelope to the configured governance webhook.
///
/// If HMAC signing is configured, the request is signed with the context
/// `catalog-sync:{event_id}`. The function returns a `DeliveryReport` on
/// success or a `DeliveryError` on failure.
pub async fn deliver(
    config: &CatalogWebhookConfig,
    envelope: &CatalogEnvelope,
    req_headers: Option<&HashMap<String, String>>,
) -> Result<DeliveryReport, DeliveryError> {
    let client = build_client();
    let headers = resolve_headers(&config.headers, &config.headers_from_request, req_headers);
    let timeout = Duration::from_secs(config.timeout_secs);

    // Build HMAC signer if configured
    let hmac = build_hmac(config)?;

    // Serialize the envelope
    let body = serde_json::to_vec(envelope).expect("catalog envelope serializes");

    // Build the request
    let mut request = client
        .post(config.url.as_str())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .timeout(timeout);

    // Apply operator headers
    for (name, value) in headers.iter() {
        request = request.header(name.clone(), value.clone());
    }

    // Sign if configured
    if let Some(hmac) = &hmac {
        let context = SigningContext::new(&format!("catalog-sync:{}", envelope.event_id))
            .expect("event_id renders as dot-free ASCII");
        let signed = hmac
            .sign(&context, &body)
            .map_err(|e| DeliveryError::Signing(e.to_string()))?;
        for (name, value) in signed.into_pairs() {
            request = request.header(name, value);
        }
    }

    // Send the request
    let response = request
        .body(body)
        .send()
        .await
        .map_err(|e| DeliveryError::Transport(e.to_string()))?;

    let status = response.status();
    let status_code = status.as_u16();

    // Read response body
    let response_body = response
        .text()
        .await
        .ok()
        .map(|s| truncate_response(&s, 1024));

    if !status.is_success() {
        return Err(DeliveryError::BadStatus {
            status: status_code,
            body: response_body.unwrap_or_default(),
        });
    }

    // Calculate stats
    let agents_count = envelope.agents.len();
    let servers_count: usize = envelope.agents.iter().map(|a| a.mcp_servers.len()).sum();
    let tools_count: usize = envelope
        .agents
        .iter()
        .flat_map(|a| &a.mcp_servers)
        .filter_map(|s| s.tools.as_ref())
        .map(|t| t.len())
        .sum();

    Ok(DeliveryReport {
        event_id: envelope.event_id.clone(),
        status_code,
        response_body,
        agents_count,
        servers_count,
        tools_count,
    })
}

/// Build the reqwest client for webhook calls.
fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .expect("reqwest client builder only fails on TLS backend init")
}

/// Resolve operator-configured headers into a validated `HeaderMap`.
fn resolve_headers(
    static_headers: &HashMap<String, String>,
    headers_from_request: &HashMap<String, String>,
    req_headers: Option<&HashMap<String, String>>,
) -> HeaderMap {
    let empty = HashMap::new();
    let req_headers = req_headers.unwrap_or(&empty);

    let mut resolved: HashMap<String, String> = static_headers
        .iter()
        .map(|(k, v)| (k.to_lowercase(), v.clone()))
        .collect();

    let resolved_count = crate::rig_builder::apply_request_header_mappings(
        &mut resolved,
        headers_from_request,
        req_headers,
    );
    if resolved_count > 0 {
        tracing::info!(
            "Governance catalog webhook: resolved {resolved_count} header(s) from request"
        );
    }

    let mut header_map = HeaderMap::new();
    for (key, value) in &resolved {
        match (
            HeaderName::from_bytes(key.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            (Ok(name), Ok(val)) => {
                header_map.insert(name, val);
            }
            _ => {
                tracing::warn!(
                    "Skipping invalid governance webhook header '{}' (failed to convert)",
                    key
                );
            }
        }
    }
    header_map
}

/// Build the HMAC signer from config if configured.
fn build_hmac(config: &CatalogWebhookConfig) -> Result<Option<WebhookHmac>, DeliveryError> {
    let Some(hmac_config) = &config.hmac else {
        return Ok(None);
    };

    // Validate plaintext URL with HMAC
    if config.url.as_str().starts_with("http://") {
        return Err(DeliveryError::Misconfigured(format!(
            "governance webhook url {} uses plaintext http:// while an HMAC secret is configured; \
             use https://",
            config.url.as_str()
        )));
    }

    // Build the HMAC configuration
    let primary = PrimarySecret::new(hmac_config.secret.as_bytes());
    let hmac = WebhookHmac::new(primary, None, Tolerance::default())
        .map_err(|e| DeliveryError::Misconfigured(e.to_string()))?;

    Ok(Some(hmac))
}

/// Truncate a response body for logging.
fn truncate_response(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}... (truncated)", &s[..max_len])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_response_leaves_short_strings() {
        assert_eq!(truncate_response("hello", 10), "hello");
    }

    #[test]
    fn truncate_response_truncates_long_strings() {
        let long = "a".repeat(2000);
        let result = truncate_response(&long, 1024);
        assert!(result.ends_with("... (truncated)"));
        assert!(result.len() < 1100);
    }
}

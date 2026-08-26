//! Shared helpers for outbound webhook egress: header resolution from
//! `(static, headers_from_request)` mappings, and HMAC construction from an
//! operator-facing secret string.
//!
//! Both the HITL approval route and the governance catalog sync configure
//! their webhook the same way (static headers overlaid with
//! `headers_from_request` values from the inbound request, plus an optional
//! HMAC secret). These helpers keep the header-conversion policy and the
//! "secret → `WebhookHmac`" construction in one place.

use std::collections::HashMap;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use tracing::{info, warn};

use crate::hitl::{ConfigError, PrimarySecret, Tolerance, WebhookHmac};

/// Resolve operator-configured webhook headers into a validated [`HeaderMap`]:
/// `static_headers` overlaid with `headers_from_request` values pulled from the
/// inbound client request. Invalid header names or values are skipped with a
/// warning that names the offending key (never the value).
///
/// Opt-in only: nothing is forwarded unless the operator configures it.
/// Classification is deliberately absent from this surface — see
/// [`apply_request_header_mappings`](crate::rig_builder::apply_request_header_mappings).
///
/// `info_prefix` labels the caller in the "resolved N header(s)" info line
/// (e.g. `"Webhook route"`, `"Catalog webhook"`). `warn_kind` labels the
/// category in the "Skipping invalid ___ header" warning (e.g. `"webhook"`,
/// `"catalog webhook"`).
pub fn resolve_headers(
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
        info!("resolved {resolved_count} header(s) from request");
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
                warn!("Skipping invalid header '{}' (failed to convert)", key);
            }
        }
    }
    header_map
}

/// Build a [`WebhookHmac`] from an operator-configured secret string.
///
/// Returns `Ok(None)` when the secret is `None` or empty (unsigned egress).
/// Uses the default tolerance and no secondary secret; loaders that need a
/// secondary or env-driven configuration should use
/// [`WebhookHmac::load_from_env`] directly.
pub fn build_hmac_from_secret(secret: Option<&str>) -> Result<Option<WebhookHmac>, ConfigError> {
    let Some(secret) = secret.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let primary = PrimarySecret::new(secret.as_bytes());
    let hmac = WebhookHmac::new(primary, None, Tolerance::default())?;
    Ok(Some(hmac))
}

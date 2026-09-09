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

/// Capture failure for poll-delivery egress at rest: a `headers_from_request`
/// destination with no usable resolved value — its request header was absent
/// and its static fallback is absent or invalid.
///
/// The `names` payload is the audit signal: the mapped destination NAMES,
/// sorted, never a value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("webhook egress capture failed: no usable value for mapped headers {names:?}")]
pub struct EgressCaptureError {
    names: Vec<String>,
}

impl EgressCaptureError {
    pub(crate) fn new(mut names: Vec<String>) -> Self {
        // Sorted so one failing resolution always renders the same audit string.
        names.sort_unstable();
        Self { names }
    }

    /// The mapped destination names with no usable resolved value, sorted.
    #[must_use]
    pub fn missing_names(&self) -> &[String] {
        &self.names
    }
}

/// The strict egress check behind the park arm's registration-closed rule:
/// every `headers_from_request` destination must appear in `resolved` with a
/// valid header value. `resolve_headers` skips invalid entries with a warning,
/// so presence in the resolved map is exactly "a usable resolved value" —
/// from the request itself or from a valid static fallback (whose existing
/// resolution semantics this check preserves). A map with no `headers_from_request`
/// destinations always passes.
pub fn check_egress_capture(
    resolved: &HeaderMap,
    headers_from_request: &HashMap<String, String>,
) -> Result<(), EgressCaptureError> {
    let missing: Vec<String> = headers_from_request
        .keys()
        .map(|destination| destination.to_lowercase())
        .filter(|destination| !resolved.contains_key(destination))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(EgressCaptureError::new(missing))
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn missing_mapped_destination_fails_capture_with_names_only() {
        let static_headers = HashMap::new();
        let mapped = headers(&[
            ("authorization", "x-incoming-auth"),
            ("x-tenant", "x-tenant-id"),
        ]);
        // Only one of the two mapped sources arrives.
        let req = headers(&[("x-tenant-id", "acme")]);

        let err = check_egress_capture(
            &resolve_headers(&static_headers, &mapped, Some(&req)),
            &mapped,
        )
        .expect_err("a mapped destination with no usable value must fail capture");

        assert_eq!(
            err.missing_names(),
            ["authorization".to_string()],
            "the failing destination is named, sorted",
        );
        let message = err.to_string();
        assert!(
            message.contains("authorization"),
            "the audit message must name the destination: {message}"
        );
        // No value anywhere in the message — names are the audit surface.
        assert!(!message.contains("acme"), "message was: {message}");
    }

    /// An explicit static fallback keeps the existing resolution semantics:
    /// an absent request header resolves to the static value, and capture
    /// passes.
    #[test]
    fn static_fallback_satisfies_the_capture() {
        let static_headers = headers(&[("authorization", "static-token")]);
        let mapped = headers(&[("authorization", "x-incoming-auth")]);
        let req = HashMap::new();

        let resolved = resolve_headers(&static_headers, &mapped, Some(&req));
        check_egress_capture(&resolved, &mapped)
            .expect("a valid static fallback keeps resolution semantics");
        assert_eq!(resolved.get("authorization").unwrap(), "static-token");
    }

    #[test]
    fn present_request_header_satisfies_the_capture() {
        let static_headers = HashMap::new();
        let mapped = headers(&[("authorization", "x-incoming-auth")]);
        let req = headers(&[("x-incoming-auth", "Bearer dynamic")]);

        let resolved = resolve_headers(&static_headers, &mapped, Some(&req));
        check_egress_capture(&resolved, &mapped).expect("a present mapped request header captures");
        assert_eq!(resolved.get("authorization").unwrap(), "Bearer dynamic");
    }

    /// An invalid static fallback value is not usable: the mapped
    /// destination ends up missing from the resolved map and capture fails.
    #[test]
    fn invalid_static_fallback_fails_capture() {
        let static_headers = headers(&[("x-bad", "bad\r\nvalue")]);
        let mapped = headers(&[("x-bad", "x-source")]);

        let err = check_egress_capture(&resolve_headers(&static_headers, &mapped, None), &mapped)
            .expect_err("an invalid fallback value is not a usable resolved value");
        assert_eq!(err.missing_names(), ["x-bad".to_string()]);
    }

    #[test]
    fn no_mapped_headers_never_fails() {
        let resolved = resolve_headers(&headers(&[("x-static", "v")]), &HashMap::new(), None);
        check_egress_capture(&resolved, &HashMap::new())
            .expect("with nothing mapped there is nothing to fail");
    }
}

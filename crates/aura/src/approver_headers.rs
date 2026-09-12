//! Approver identity header overrides for gated MCP tool calls.
//!
//! When a HITL-gated tool call is approved over the webhook route, the
//! approval HTTP response may carry identity headers. This module holds the
//! types that carry those headers from capture (gate scope, webhook route
//! only) to application (exactly one outbound MCP request, via the rmcp
//! request-extension side-channel).

use aura_config::ToolHeaderMappings;
use reqwest::header::{HeaderMap, HeaderName};

/// Validated approver identity headers captured from one approved webhook
/// response.
///
/// A value of this type exists only for an approved webhook decision whose
/// mapped response headers were all present and valid; a partial capture
/// is unrepresentable (construction fails closed). Construction is
/// crate-private: the webhook client's gate-scoped path is the only
/// producer.
#[derive(Clone)]
pub struct ApproverHeaders {
    /// Validated override pairs, keys lowercased. Keys serve as the audit surface (names only); no separate name list exists.
    headers: HeaderMap,
}

impl ApproverHeaders {
    /// Capture and validate approver headers from an approval response.
    ///
    /// Response lookup is case-insensitive and takes the first value of a
    /// multi-valued header, matching how the route reads the signature
    /// headers off the same response.
    pub(crate) fn from_captured(
        mapping: &ToolHeaderMappings,
        response_headers: &HeaderMap,
    ) -> Result<Self, CaptureError> {
        let mut headers = HeaderMap::new();
        let mut missing = Vec::new();
        for (outbound, response_name) in mapping.iter() {
            match response_headers.get(response_name) {
                Some(value) => {
                    // The outbound name is a validated lowercase header name
                    // by construction of `ToolHeaderMappings`.
                    let name = HeaderName::from_bytes(outbound.as_bytes())
                        .expect("outbound names validated at config parse");
                    headers.insert(name, value.clone());
                }
                None => missing.push(MissingMapping {
                    response_name: response_name.to_owned(),
                    outbound_name: outbound.to_owned(),
                }),
            }
        }
        if !missing.is_empty() {
            missing.sort_unstable();
            return Err(CaptureError::MissingHeaders { missing });
        }
        Ok(Self { headers })
    }

    /// The captured outbound header names (never values), lowercased.
    pub fn captured_names(&self) -> impl Iterator<Item = &str> {
        self.headers.keys().map(HeaderName::as_str)
    }

    /// Apply the overrides to an outbound request builder as per-request
    /// headers, which override the client's frozen `default_headers` for
    /// that one request only.
    ///
    /// Whole-map application, not pair-by-pair: `RequestBuilder::header`
    /// appends, so a name the builder already carries would end up sent
    /// twice — the approver's identity beside the requester's. Passing the
    /// map replaces per name, which is the discipline the lowercased keys
    /// were established for. Default headers need no such care: `reqwest`
    /// fills them in only where the request left the name vacant.
    pub(crate) fn apply_to(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder.headers(self.headers.clone())
    }
}

impl std::fmt::Debug for ApproverHeaders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApproverHeaders")
            .field("captured_names", &self.captured_names().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

// Manual impls rather than derives: `PreCallOutcome` derives `PartialEq, Eq`
// and must keep doing so; `Eq` is sound here because `HeaderValue` equality
// is total, and `HeaderMap` equality is order-insensitive.
impl PartialEq for ApproverHeaders {
    fn eq(&self, other: &Self) -> bool {
        self.headers == other.headers
    }
}

impl Eq for ApproverHeaders {}

/// One missing pair in a capture failure: the approved webhook response
/// lacked the response header a configured mapping expected, so no value
/// could be captured under the outbound tool header's name.
///
/// Both fields are header names and only header names — the audit surface
/// never carries values. Both are lowercase, normalized at config parse by
/// `ToolHeaderMappings`; response lookup remains case-insensitive.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct MissingMapping {
    /// The configured response header that was absent from the approved response.
    pub response_name: String,
    /// The outbound MCP tool header the value would have been captured under.
    pub outbound_name: String,
}

impl std::fmt::Display for MissingMapping {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "\"{}\" (mapped to tool header \"{}\")",
            self.response_name, self.outbound_name
        )
    }
}

/// Capture-time failures: the approved webhook response could not yield
/// the configured approver headers (fail closed).
///
/// The `missing` payload is diagnostic-only text for the error message and
/// the event-level audit signal: always non-empty by construction (capture
/// fails only when at least one mapping is missing), sorted by response
/// header name then outbound header name (the derived field order of
/// `MissingMapping`) so the audit string is deterministic, both sides of
/// every mapping carried as header names and never values, and no domain
/// logic branches on it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CaptureError {
    /// Mapped response headers absent from the approved response. Invalid
    /// values cannot occur here: capture reads a parsed `HeaderMap`, whose
    /// values are already syntactically valid; invalid outbound names are
    /// rejected earlier, at config parse.
    MissingHeaders { missing: Vec<MissingMapping> },
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::MissingHeaders { missing } => {
                if let [one] = missing.as_slice() {
                    write!(
                        f,
                        "approver identity capture failed: webhook response missing header {one}"
                    )
                } else {
                    let clauses: Vec<String> =
                        missing.iter().map(MissingMapping::to_string).collect();
                    write!(
                        f,
                        "approver identity capture failed: webhook response missing headers [{}]",
                        clauses.join("; ")
                    )
                }
            }
        }
    }
}

/// Application-time failures at the execution seam (double override,
/// transport refusal), kept separate from [`CaptureError`] so a
/// composition layer cannot wrap one as the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OverrideApplicationError {
    /// More than one wrapper produced overrides for one call. Identity is
    /// never chosen by wrapper order; this is an error in release and debug
    /// alike.
    #[error("conflicting approver identity overrides from multiple wrappers")]
    DoubleOverride,
    /// The tool's transport cannot deliver per-call headers while identity
    /// was demanded (stdio fails closed).
    #[error("transport {kind:?} cannot deliver approver identity overrides")]
    TransportUnsupported { kind: McpTransportKind },
}

/// Which MCP transport an adaptor was constructed for. Tagged at
/// construction (the three `add_all_tools` branches) so the override path
/// can fail closed on transports that cannot deliver per-request headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpTransportKind {
    StreamableHttp,
    Sse,
    Stdio,
}

/// Fail closed when `kind` cannot deliver per-call header overrides.
/// Called at the execution seam only when overrides exist, so the check
/// keys off the transport alone.
pub(crate) fn ensure_transport_delivers_overrides(
    kind: McpTransportKind,
) -> Result<(), OverrideApplicationError> {
    match kind {
        // Both HTTP send paths read the extension and apply the overrides.
        McpTransportKind::StreamableHttp | McpTransportKind::Sse => Ok(()),
        McpTransportKind::Stdio => Err(OverrideApplicationError::TransportUnsupported { kind }),
    }
}

/// Extract approver overrides from an outbound client message, if the one
/// request riding in it carries them as an extension. `None` for every
/// non-request message and every request without the extension. Shared by
/// the streamable-HTTP `post_message` and the SSE `Transport::send` read
/// points.
#[must_use]
pub(crate) fn extract_from_client_message(
    message: &rmcp::model::ClientJsonRpcMessage,
) -> Option<ApproverHeaders> {
    use rmcp::model::GetExtensions;
    match message {
        rmcp::model::JsonRpcMessage::Request(request) => request
            .request
            .extensions()
            .get::<ApproverHeaders>()
            .cloned(),
        _ => None,
    }
}

tokio::task_local! {
    /// Approver header overrides for the current gated tool call.
    /// Crate-private so nothing outside the wrapper path can inject
    /// overrides.
    pub(crate) static APPROVER_OVERRIDES: Option<ApproverHeaders>;
}

/// Unscoped-safe read of the current call's approver overrides.
///
/// `None` outside any scope. Unscoped reads are a permanent live path, not
/// an edge case: `McpToolAdaptor` is registered WITHOUT a `WrappedTool`
/// when no wrapper is configured. A `with`-based read would panic there;
/// this helper never does.
#[must_use]
pub(crate) fn current_approver_overrides() -> Option<ApproverHeaders> {
    APPROVER_OVERRIDES
        .try_with(std::clone::Clone::clone)
        .unwrap_or(None)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;

    use reqwest::header::HeaderValue;

    use super::*;

    /// One captured override pair for seam tests. Construction uses the real capture path, so a test cannot hold overrides that production could not produce.
    pub(crate) fn captured_overrides(outbound: &str, value: &str) -> ApproverHeaders {
        const RESPONSE_NAME: &str = "x-approver-source";
        ApproverHeaders::from_captured(
            &mappings(&[(outbound, RESPONSE_NAME)]),
            &response(&[(RESPONSE_NAME, value)]),
        )
        .expect("the mapped header is present")
    }

    /// Captured overrides for several pairs. Construction uses the real capture path, so a test cannot hold overrides that production could not produce.
    #[cfg(feature = "otel")]
    pub(crate) fn captured_overrides_multi(pairs: &[(&str, &str)]) -> ApproverHeaders {
        let response_names: Vec<String> = pairs
            .iter()
            .map(|(outbound, _)| format!("x-response-{outbound}"))
            .collect();
        let mapping: Vec<(&str, &str)> = pairs
            .iter()
            .zip(&response_names)
            .map(|((outbound, _), name)| (*outbound, name.as_str()))
            .collect();
        let response_pairs: Vec<(&str, &str)> = pairs
            .iter()
            .zip(&response_names)
            .map(|((_, value), name)| (name.as_str(), *value))
            .collect();
        ApproverHeaders::from_captured(&mappings(&mapping), &response(&response_pairs))
            .expect("every mapped header is present")
    }

    pub(crate) fn mappings(pairs: &[(&str, &str)]) -> ToolHeaderMappings {
        let raw: HashMap<String, String> = pairs
            .iter()
            .map(|(outbound, response)| ((*outbound).to_owned(), (*response).to_owned()))
            .collect();
        ToolHeaderMappings::try_from(raw).expect("test mappings are valid config")
    }

    fn response(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn captures_response_value_under_outbound_name() {
        let captured = ApproverHeaders::from_captured(
            &mappings(&[("x-forwarded-user", "x-approver-id")]),
            &response(&[("x-approver-id", "alice")]),
        )
        .expect("a present mapped header captures");

        assert_eq!(
            captured.captured_names().collect::<Vec<_>>(),
            vec!["x-forwarded-user"]
        );
        assert_eq!(captured.headers.get("x-forwarded-user").unwrap(), "alice");
        assert!(captured.headers.get("x-approver-id").is_none());
    }

    /// A webhook may spell its response header however it likes, as may the operator configuring the mapping.
    #[test]
    fn response_lookup_is_case_insensitive() {
        let captured = ApproverHeaders::from_captured(
            &mappings(&[("x-forwarded-user", "X-Approver-Id")]),
            &response(&[("X-APPROVER-ID", "alice")]),
        )
        .expect("response header casing must not defeat capture");

        assert_eq!(captured.headers.get("x-forwarded-user").unwrap(), "alice");
    }

    /// A response may repeat a header. Capture takes the first value, as the route does. Exactly one value lands under the outbound name; a second identity cannot ride along.
    #[test]
    fn repeated_response_header_captures_only_the_first_value() {
        let captured = ApproverHeaders::from_captured(
            &mappings(&[("x-forwarded-user", "x-approver-id")]),
            &response(&[("x-approver-id", "alice"), ("x-approver-id", "mallory")]),
        )
        .expect("a repeated mapped header still captures");

        assert_eq!(captured.headers.get("x-forwarded-user").unwrap(), "alice");
        assert_eq!(
            captured.headers.get_all("x-forwarded-user").iter().count(),
            1
        );
    }

    /// Every missing mapping is reported at once, sorted by response header
    /// name, so one failing response always produces the same audit string.
    #[test]
    fn missing_names_are_all_reported_and_sorted() {
        let err = ApproverHeaders::from_captured(
            &mappings(&[
                ("x-forwarded-user", "x-approver-id"),
                ("authorization", "x-approver-token"),
                ("x-tenant", "x-approver-tenant"),
            ]),
            &response(&[("x-approver-tenant", "acme")]),
        )
        .expect_err("a partial capture must fail closed");

        assert_eq!(
            err,
            CaptureError::MissingHeaders {
                missing: vec![
                    MissingMapping {
                        response_name: "x-approver-id".to_owned(),
                        outbound_name: "x-forwarded-user".to_owned(),
                    },
                    MissingMapping {
                        response_name: "x-approver-token".to_owned(),
                        outbound_name: "authorization".to_owned(),
                    },
                ],
            }
        );

        // Event-level audit signal: the Display text names both sides of
        // every missing mapping and carries no value.
        let message = err.to_string();
        assert_eq!(
            message,
            "approver identity capture failed: webhook response missing headers \
             [\"x-approver-id\" (mapped to tool header \"x-forwarded-user\"); \
             \"x-approver-token\" (mapped to tool header \"authorization\")]"
        );
        assert!(!message.contains("acme"), "message was: {message}");
    }

    /// The same logical set of missing mappings renders identically however
    /// the config map enumerates them: the audit string is deterministic.
    #[test]
    fn display_is_deterministic_regardless_of_construction_order() {
        let pairs = [
            ("x-forwarded-user", "x-approver-id"),
            ("authorization", "x-approver-token"),
            ("x-tenant", "x-approver-tenant"),
        ];
        let mut reversed = pairs;
        reversed.reverse();

        let forward = ApproverHeaders::from_captured(&mappings(&pairs), &response(&[]))
            .expect_err("an empty response must fail every mapping");
        let backward = ApproverHeaders::from_captured(&mappings(&reversed), &response(&[]))
            .expect_err("an empty response must fail every mapping");

        assert_eq!(forward.to_string(), backward.to_string());
    }

    /// Mappings that read the same response header tie on response name;
    /// the outbound name breaks the tie, so the sort total-orders every
    /// payload and the audit string is deterministic even under ties.
    #[test]
    fn equal_response_names_tie_break_on_outbound_name() {
        let err = ApproverHeaders::from_captured(
            &mappings(&[
                ("x-second-tool", "x-shared-response"),
                ("x-first-tool", "x-shared-response"),
            ]),
            &response(&[]),
        )
        .expect_err("an empty response must fail every mapping");

        assert_eq!(
            err.to_string(),
            "approver identity capture failed: webhook response missing headers \
             [\"x-shared-response\" (mapped to tool header \"x-first-tool\"); \
             \"x-shared-response\" (mapped to tool header \"x-second-tool\")]"
        );
    }

    /// The failure names the response header as what was missing and the
    /// tool header as what it maps to — never the value the response did carry.
    #[test]
    fn missing_pair_error_names_both_sides_and_never_the_value() {
        let err = ApproverHeaders::from_captured(
            &mappings(&[("x-source-header", "x-dest-header")]),
            &response(&[("x-source-header", "my forwarded value")]),
        )
        .expect_err("a response lacking the mapped response header must fail closed");

        let message = err.to_string();
        assert_eq!(
            message,
            "approver identity capture failed: webhook response missing header \
             \"x-dest-header\" (mapped to tool header \"x-source-header\")"
        );
        assert!(
            !message.contains("my forwarded value"),
            "message was: {message}"
        );
    }

    #[test]
    fn apply_to_sets_every_captured_pair_on_the_request() {
        let captured = ApproverHeaders::from_captured(
            &mappings(&[
                ("x-forwarded-user", "x-approver-id"),
                ("x-tenant", "x-approver-tenant"),
            ]),
            &response(&[("x-approver-id", "alice"), ("x-approver-tenant", "acme")]),
        )
        .expect("both mapped headers capture");

        let request = captured
            .apply_to(reqwest::Client::new().post("http://127.0.0.1:9/"))
            .build()
            .expect("the override headers are valid");

        assert_eq!(request.headers().get("x-forwarded-user").unwrap(), "alice");
        assert_eq!(request.headers().get("x-tenant").unwrap(), "acme");
    }

    #[test]
    fn apply_to_replaces_a_header_already_on_the_builder() {
        let captured = ApproverHeaders::from_captured(
            &mappings(&[("authorization", "x-approver-token")]),
            &response(&[("x-approver-token", "Bearer approver")]),
        )
        .expect("the mapped header captures");

        let request = captured
            .apply_to(
                reqwest::Client::new()
                    .post("http://127.0.0.1:9/")
                    .header("authorization", "Bearer requester"),
            )
            .build()
            .expect("the override headers are valid");

        assert_eq!(
            request.headers().get_all("authorization").iter().count(),
            1,
            "the requester's identity must not ride along beside the approver's",
        );
        assert_eq!(
            request.headers().get("authorization").unwrap(),
            "Bearer approver",
        );
    }

    /// Stdio has no per-call header channel, so a call that demands identity
    /// cannot be delivered and must not proceed under the cached one.
    #[test]
    fn stdio_transport_refuses_overrides() {
        assert_eq!(
            ensure_transport_delivers_overrides(McpTransportKind::Stdio),
            Err(OverrideApplicationError::TransportUnsupported {
                kind: McpTransportKind::Stdio
            }),
        );
    }

    /// Both HTTP transports read the extension on their send path, so both
    /// can deliver.
    #[test]
    fn http_transports_accept_overrides() {
        assert_eq!(
            ensure_transport_delivers_overrides(McpTransportKind::StreamableHttp),
            Ok(()),
        );
        assert_eq!(
            ensure_transport_delivers_overrides(McpTransportKind::Sse),
            Ok(()),
        );
    }

    /// Every adaptor call outside a task-local scope (wrapper-less agents)
    /// must read `None`, never panic.
    #[tokio::test]
    async fn unscoped_read_yields_none() {
        assert_eq!(current_approver_overrides(), None);
    }

    #[tokio::test]
    async fn scoped_none_reads_none() {
        APPROVER_OVERRIDES
            .scope(None, async {
                assert_eq!(current_approver_overrides(), None);
            })
            .await;
    }
}

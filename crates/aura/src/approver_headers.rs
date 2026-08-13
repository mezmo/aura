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
    /// The validated override pairs, keys lowercased so an override replaces
    /// the frozen default header rather than coexisting with it. The keys
    /// are also the audit surface (names only); no separate name list
    /// exists to fall out of sync.
    headers: HeaderMap,
}

impl ApproverHeaders {
    /// Capture and validate approver headers from an approval response.
    ///
    /// Every outbound name in `mapping` must resolve to a present response
    /// header or the whole capture fails closed; nothing is silently
    /// dropped. Response lookup is case-insensitive and takes the first
    /// value of a multi-valued header, matching how the route reads the
    /// signature headers off the same response. The missing names in the
    /// error are sorted so a given failure always reports them in one
    /// order.
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
                None => missing.push(outbound.to_owned()),
            }
        }
        if !missing.is_empty() {
            missing.sort_unstable();
            return Err(CaptureError::MissingHeaders { names: missing });
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

/// Capture-time failures: the approved webhook response could not yield
/// the configured approver headers (fail closed).
///
/// The `names` payload is diagnostic-only text for the error message and
/// the event-level audit signal: always non-empty by construction (capture
/// fails only when at least one name is missing), lowercased outbound
/// header names, never values, and no domain logic branches on it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CaptureError {
    /// Mapped response headers absent from the approved response. Invalid
    /// values cannot occur here: capture reads a parsed `HeaderMap`, whose
    /// values are already syntactically valid; invalid outbound names are
    /// rejected earlier, at config parse.
    #[error("approver identity capture failed: response missing mapped headers {names:?}")]
    MissingHeaders { names: Vec<String> },
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
/// Called at the execution seam only when overrides exist.
///
/// The transport alone decides. An override value carrying no pairs is not
/// "no override": the caller reached here because identity was demanded, and
/// a transport with no per-call header channel cannot honor that demand
/// whatever the configured map turned out to hold.
pub(crate) fn ensure_transport_delivers_overrides(
    kind: McpTransportKind,
    _overrides: &ApproverHeaders,
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
    /// Approver header overrides for the current gated tool call. Scoped
    /// by `WrappedTool::call` inside the inner-call spawn; read by
    /// `McpToolAdaptor::call` via [`current_approver_overrides`].
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

    /// One captured override pair, for the seam tests that need a value of
    /// this type without restating the capture plumbing. Construction goes
    /// through the real capture path so a test can never hold overrides the
    /// production path could not have produced.
    pub(crate) fn captured_overrides(outbound: &str, value: &str) -> ApproverHeaders {
        const RESPONSE_NAME: &str = "x-approver-source";
        ApproverHeaders::from_captured(
            &mappings(&[(outbound, RESPONSE_NAME)]),
            &response(&[(RESPONSE_NAME, value)]),
        )
        .expect("the mapped header is present")
    }

    fn mappings(pairs: &[(&str, &str)]) -> ToolHeaderMappings {
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

    /// Capture rekeys the response value under the configured outbound name,
    /// carrying the value through unchanged.
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

    /// A webhook is free to spell its response header any way, and so is
    /// the operator configuring the mapping; neither spelling has to match
    /// the other's case.
    #[test]
    fn response_lookup_is_case_insensitive() {
        let captured = ApproverHeaders::from_captured(
            &mappings(&[("x-forwarded-user", "X-Approver-Id")]),
            &response(&[("X-APPROVER-ID", "alice")]),
        )
        .expect("response header casing must not defeat capture");

        assert_eq!(captured.headers.get("x-forwarded-user").unwrap(), "alice");
    }

    /// A response may repeat a header. Capture takes the first value, as the
    /// route's own header reads do, and exactly one value lands under the
    /// outbound name — a second approver identity cannot ride along.
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

    /// Every missing name is reported at once, sorted, so one failing
    /// response always produces the same audit string.
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
                names: vec!["authorization".to_owned(), "x-forwarded-user".to_owned()],
            }
        );

        // The event-level audit signal: the Display text names every missing
        // header and carries no value, including the one response value that
        // did arrive (for the header that was present).
        let message = err.to_string();
        assert_eq!(
            message,
            "approver identity capture failed: response missing mapped headers \
             [\"authorization\", \"x-forwarded-user\"]"
        );
        assert!(!message.contains("acme"), "message was: {message}");
    }

    /// Every captured pair lands on the request the builder produces, under
    /// the outbound name.
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

    /// The override REPLACES a same-named header already on the builder rather
    /// than coexisting with it. Two identities on one request is the failure
    /// mode this guards: `reqwest`'s per-header setter appends, so applying
    /// pair-by-pair would leave the original value in place beside the
    /// approver's.
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

    fn any_overrides() -> ApproverHeaders {
        ApproverHeaders::from_captured(
            &mappings(&[("x-forwarded-user", "x-approver-id")]),
            &response(&[("x-approver-id", "alice")]),
        )
        .expect("test overrides capture")
    }

    /// Stdio has no per-call header channel, so a call that demands identity
    /// cannot be delivered and must not proceed under the cached one.
    #[test]
    fn stdio_transport_refuses_overrides() {
        assert_eq!(
            ensure_transport_delivers_overrides(McpTransportKind::Stdio, &any_overrides()),
            Err(OverrideApplicationError::TransportUnsupported {
                kind: McpTransportKind::Stdio
            }),
        );
    }

    /// Both HTTP transports read the extension on their send path, so both
    /// can deliver.
    #[test]
    fn http_transports_accept_overrides() {
        let overrides = any_overrides();
        assert_eq!(
            ensure_transport_delivers_overrides(McpTransportKind::StreamableHttp, &overrides),
            Ok(()),
        );
        assert_eq!(
            ensure_transport_delivers_overrides(McpTransportKind::Sse, &overrides),
            Ok(()),
        );
    }

    /// The check keys off the transport alone. An override value with no pairs
    /// is not "no override": identity was demanded, the configured map decided
    /// what that means, and a transport that cannot carry headers still cannot
    /// honor the demand.
    #[test]
    fn stdio_refuses_even_an_empty_override_set() {
        let empty = ApproverHeaders::from_captured(&mappings(&[]), &HeaderMap::new())
            .expect("an empty mapping captures nothing and succeeds");
        assert_eq!(empty.captured_names().count(), 0);

        assert_eq!(
            ensure_transport_delivers_overrides(McpTransportKind::Stdio, &empty),
            Err(OverrideApplicationError::TransportUnsupported {
                kind: McpTransportKind::Stdio
            }),
        );
    }

    /// Every adaptor call outside a task-local scope (wrapper-less agents)
    /// must read `None`, never panic.
    #[tokio::test]
    async fn unscoped_read_yields_none() {
        assert_eq!(current_approver_overrides(), None);
    }

    /// And the scoped read hands the value through unchanged shape-wise
    /// (a `None` payload scoped explicitly is still `None`).
    #[tokio::test]
    async fn scoped_none_reads_none() {
        APPROVER_OVERRIDES
            .scope(None, async {
                assert_eq!(current_approver_overrides(), None);
            })
            .await;
    }
}

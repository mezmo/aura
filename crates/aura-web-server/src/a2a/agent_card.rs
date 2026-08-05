use std::sync::Arc;

use a2a::{AgentCard, AgentInterface, TRANSPORT_PROTOCOL_JSONRPC};
use a2a_server::WELL_KNOWN_AGENT_CARD_PATH;

use crate::a2a::LEGACY_PROTOCOL_VERSION;
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
};
use serde::Serialize;

/// An [`AgentCard`] carrying the pre-v1.0 top-level `url` and
/// `preferredTransport` fields alongside `supportedInterfaces`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompatAgentCard {
    #[serde(flatten)]
    card: AgentCard,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    preferred_transport: Option<String>,
}

impl CompatAgentCard {
    /// Extend `card` with the endpoint fields A2A v0.x clients read.
    ///
    /// Only v0.x clients read these fields, so they point at the v0.3 JSON-RPC
    /// entry of `supported_interfaces` when the card advertises one — sending a
    /// v0.x client to the v1.0 endpoint would only earn it a `method not found`.
    /// Failing that they fall back to any JSON-RPC interface, then to the first
    /// interface listed. A card with no interfaces at all is served unchanged;
    /// there is nothing to point the fields at.
    pub fn new(card: AgentCard) -> Self {
        let is_jsonrpc = |i: &&AgentInterface| i.protocol_binding == TRANSPORT_PROTOCOL_JSONRPC;
        let preferred = card
            .supported_interfaces
            .iter()
            .find(|i| is_jsonrpc(i) && i.protocol_version == LEGACY_PROTOCOL_VERSION)
            .or_else(|| card.supported_interfaces.iter().find(is_jsonrpc))
            .or_else(|| card.supported_interfaces.first());

        let url = preferred.map(|i| i.url.clone());
        let preferred_transport = preferred.map(|i| i.protocol_binding.clone());

        Self {
            card,
            url,
            preferred_transport,
        }
    }
}

/// Serve the agent card at `/.well-known/agent-card.json` with CORS headers for
/// public discovery.
///
/// Stands in for [`a2a_server::agent_card::agent_card_router`], which serializes
/// [`AgentCard`] verbatim and so can only emit the v1.0 shape.
pub fn agent_card_router(card: AgentCard) -> axum::Router {
    axum::Router::new()
        .route(
            WELL_KNOWN_AGENT_CARD_PATH,
            axum::routing::get(handle_agent_card),
        )
        .with_state(Arc::new(CompatAgentCard::new(card)))
}

/// Echo an explicit `Origin` back with credentials allowed, and fall back to a
/// wildcard for origin-less requests.
async fn handle_agent_card(
    State(card): State<Arc<CompatAgentCard>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let mut resp_headers = HeaderMap::new();

    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("*");

    if origin != "*" {
        resp_headers.insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            origin.parse().unwrap_or_else(|_| "*".parse().unwrap()),
        );
        resp_headers.insert(
            header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            "true".parse().unwrap(),
        );
        resp_headers.insert(header::VARY, "Origin".parse().unwrap());
    } else {
        resp_headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".parse().unwrap());
    }

    (StatusCode::OK, resp_headers, Json(card))
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::{AgentCapabilities, AgentInterface, TRANSPORT_PROTOCOL_HTTP_JSON};
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    fn card_with(interfaces: Vec<AgentInterface>) -> AgentCard {
        AgentCard {
            name: "TestAgent".into(),
            description: "A test agent".into(),
            version: "1.0".into(),
            supported_interfaces: interfaces,
            capabilities: AgentCapabilities::default(),
            default_input_modes: vec!["text/plain".into()],
            default_output_modes: vec!["text/plain".into()],
            skills: vec![],
            provider: None,
            documentation_url: None,
            icon_url: None,
            security_schemes: None,
            security_requirements: None,
            signatures: None,
        }
    }

    fn v1_interfaces() -> Vec<AgentInterface> {
        vec![
            AgentInterface::new(
                "https://aura.example.com/a2a/v1",
                TRANSPORT_PROTOCOL_HTTP_JSON,
            ),
            AgentInterface::new(
                "https://aura.example.com/a2a/v1/rpc",
                TRANSPORT_PROTOCOL_JSONRPC,
            ),
        ]
    }

    /// The interface set `AuraAgentExecutor::build_agent_card` produces.
    fn aura_interfaces() -> Vec<AgentInterface> {
        let mut interfaces = v1_interfaces();
        interfaces.push(AgentInterface {
            url: "https://aura.example.com/".into(),
            protocol_binding: TRANSPORT_PROTOCOL_JSONRPC.into(),
            protocol_version: LEGACY_PROTOCOL_VERSION.into(),
            tenant: None,
        });
        interfaces
    }

    fn serialize(card: AgentCard) -> Value {
        serde_json::to_value(CompatAgentCard::new(card)).expect("card serializes")
    }

    #[test]
    fn top_level_fields_point_at_the_v0_3_binding() {
        let value = serialize(card_with(aura_interfaces()));

        assert_eq!(value["url"], json!("https://aura.example.com/"));
        assert_eq!(value["preferredTransport"], json!("JSONRPC"));
    }

    #[test]
    fn v1_shape_is_preserved_alongside_the_compat_fields() {
        let value = serialize(card_with(aura_interfaces()));

        assert_eq!(value["name"], json!("TestAgent"));
        assert_eq!(
            value["supportedInterfaces"],
            json!([
                {
                    "url": "https://aura.example.com/a2a/v1",
                    "protocolBinding": "HTTP+JSON",
                    "protocolVersion": a2a::VERSION,
                },
                {
                    "url": "https://aura.example.com/a2a/v1/rpc",
                    "protocolBinding": "JSONRPC",
                    "protocolVersion": a2a::VERSION,
                },
                {
                    "url": "https://aura.example.com/",
                    "protocolBinding": "JSONRPC",
                    "protocolVersion": LEGACY_PROTOCOL_VERSION,
                },
            ])
        );
    }

    #[test]
    fn falls_back_to_any_jsonrpc_interface_without_a_v0_3_one() {
        let value = serialize(card_with(v1_interfaces()));

        assert_eq!(value["url"], json!("https://aura.example.com/a2a/v1/rpc"));
        assert_eq!(value["preferredTransport"], json!("JSONRPC"));
    }

    #[test]
    fn falls_back_to_the_first_interface_without_a_jsonrpc_binding() {
        let value = serialize(card_with(vec![AgentInterface::new(
            "https://aura.example.com/a2a/v1",
            TRANSPORT_PROTOCOL_HTTP_JSON,
        )]));

        assert_eq!(value["url"], json!("https://aura.example.com/a2a/v1"));
        assert_eq!(value["preferredTransport"], json!("HTTP+JSON"));
    }

    #[test]
    fn omits_the_compat_fields_when_no_interface_is_advertised() {
        let value = serialize(card_with(vec![]));

        assert!(value.get("url").is_none());
        assert!(value.get("preferredTransport").is_none());
    }

    #[tokio::test]
    async fn well_known_route_serves_the_compat_card() {
        let app = agent_card_router(card_with(aura_interfaces()));

        let req = Request::builder()
            .uri(WELL_KNOWN_AGENT_CARD_PATH)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "*"
        );

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["url"], json!("https://aura.example.com/"));
        assert_eq!(value["preferredTransport"], json!("JSONRPC"));
    }

    #[tokio::test]
    async fn well_known_route_echoes_an_explicit_origin() {
        let app = agent_card_router(card_with(aura_interfaces()));

        let req = Request::builder()
            .uri(WELL_KNOWN_AGENT_CARD_PATH)
            .header(header::ORIGIN, "https://example.com")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "https://example.com"
        );
        assert_eq!(
            resp.headers()
                .get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
                .unwrap(),
            "true"
        );
    }
}

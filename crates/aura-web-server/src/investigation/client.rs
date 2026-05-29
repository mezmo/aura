//! HTTP client for the ai-history-service `/internal/investigation` API.

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Investigation state transitions, mirrors `INVESTIGATION_STATUS` in ai-history-service (`lib/constants.js`).
///
/// Failed investigations are still considered completed `InvestigationStatus::Completed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvestigationState {
    Triggered,
    Investigating,
    Completed,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateInvestigationRequest {
    pub trigger_source: String,
    pub linked_alert_id: String,
    pub evidence_refs: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<InvestigationState>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct UpdateInvestigationRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<InvestigationState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_resolution: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution_status: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InvestigationRecord {
    pub id: String,
    #[serde(default)]
    pub state: Option<InvestigationState>,
    #[serde(flatten)]
    pub rest: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct InvestigationEnvelope {
    data: InvestigationRecord,
}

#[derive(Debug, thiserror::Error)]
pub enum AiHistoryError {
    #[error("invalid auth header name '{}'", .0)]
    BadHeaderName(String),
    #[error("invalid auth header value for '{}'", .0)]
    BadHeaderValue(String),
    #[error("ai-history-service returned HTTP {}: {}", .status, .body)]
    Http { status: u16, body: String },
    #[error("ai-history-service request failed: {}", .0)]
    Transport(#[from] reqwest::Error),
    #[error("ai-history-service returned malformed JSON: {}", .0)]
    Json(serde_json::Error),
}

#[derive(Clone)]
pub struct AiHistoryClient {
    http: reqwest::Client,
    base_url: url::Url,
}

impl AiHistoryClient {
    pub fn new(http: reqwest::Client, base_url: url::Url) -> Self {
        Self { http, base_url }
    }

    pub async fn create(
        &self,
        auth_headers: &HashMap<String, String>,
        body: &CreateInvestigationRequest,
    ) -> Result<InvestigationRecord, AiHistoryError> {
        let mut endpoint = self.base_url.clone();

        {
            let mut path = endpoint
                .path_segments_mut()
                .expect("Endpoint cannot be relative");

            path.push("internal");
            path.push("investigation");
        }

        let headers = build_auth_header_map(auth_headers)?;

        let response = self
            .http
            .post(endpoint)
            .headers(headers)
            .json(body)
            .send()
            .await?;

        decode_envelope(response).await
    }

    pub async fn update(
        &self,
        auth_headers: &HashMap<String, String>,
        investigation_id: &str,
        body: &UpdateInvestigationRequest,
    ) -> Result<InvestigationRecord, AiHistoryError> {
        let mut endpoint = self.base_url.clone();

        {
            let mut path = endpoint
                .path_segments_mut()
                .expect("Endpoint cannot be relative");

            path.push("internal");
            path.push("investigation");
            path.push(investigation_id);
        }

        let headers = build_auth_header_map(auth_headers)?;

        let reponse = self
            .http
            .patch(endpoint)
            .headers(headers)
            .json(body)
            .send()
            .await?;

        decode_envelope(reponse).await
    }
}

/// Build a HeaderMap from the forwarded `x-auth-*` headers plus the JSON content type.
/// Returns a typed error if any header name/value is invalid (callers can render 502).
fn build_auth_header_map(
    auth_headers: &HashMap<String, String>,
) -> Result<HeaderMap, AiHistoryError> {
    let mut headers = HeaderMap::new();

    for (key, value) in auth_headers {
        let name = HeaderName::from_bytes(key.as_bytes())
            .map_err(|_| AiHistoryError::BadHeaderName(key.clone()))?;

        let value = HeaderValue::from_str(value)
            .map_err(|_| AiHistoryError::BadHeaderValue(key.clone()))?;

        headers.insert(name, value);
    }

    headers.insert(
        reqwest::header::ACCEPT,
        HeaderValue::from_static("application/json"),
    );

    Ok(headers)
}

async fn decode_envelope(
    response: reqwest::Response,
) -> Result<InvestigationRecord, AiHistoryError> {
    let status = response.status();
    let body_bytes = response.bytes().await?;

    if !status.is_success() {
        let body = String::from_utf8_lossy(&body_bytes).into_owned();

        return Err(AiHistoryError::Http {
            status: status.as_u16(),
            body,
        });
    }

    let envelope: InvestigationEnvelope =
        serde_json::from_slice(&body_bytes).map_err(AiHistoryError::Json)?;

    Ok(envelope.data)
}

#[cfg(test)]
mod tests {

    use crate::investigation::client::InvestigationState;

    use super::{
        AiHistoryError, CreateInvestigationRequest, HashMap, UpdateInvestigationRequest,
        build_auth_header_map,
    };

    #[test]
    fn create_request_serializes_with_state_omitted_when_none() {
        let request = CreateInvestigationRequest {
            trigger_source: "pipeline".into(),
            linked_alert_id: "alert-1".into(),
            evidence_refs: "stuff happened".into(),
            state: None,
        };

        let json = serde_json::to_string(&request).unwrap();

        assert!(!json.contains("state"));
        assert!(json.contains("\"trigger_source\":\"pipeline\""));
    }

    #[test]
    fn update_request_omits_none_fields() {
        let request = UpdateInvestigationRequest {
            state: Some(InvestigationState::Completed),
            confidence_score: None,
            suggested_resolution: None,
            resolution_status: Some("resolved".into()),
        };

        let json = serde_json::to_string(&request).unwrap();

        assert!(json.contains("\"state\":\"completed\""));
        assert!(json.contains("\"resolution_status\":\"resolved\""));

        assert!(!json.contains("confidence_score"));
        assert!(!json.contains("suggested_resolution"));
    }

    #[test]
    fn build_auth_headers_includes_only_provided() {
        let mut headers = HashMap::new();
        headers.insert("x-auth-subject-id".into(), "abc".into());
        headers.insert("x-auth-account-id".into(), "def".into());

        let map = build_auth_header_map(&headers).unwrap();

        assert_eq!(map.get("x-auth-subject-id").unwrap(), "abc");
        assert_eq!(map.get("x-auth-account-id").unwrap(), "def");
        assert_eq!(map.get("accept").unwrap(), "application/json");
    }

    #[test]
    fn invalid_header_name_rejected() {
        let mut headers = HashMap::new();
        headers.insert("bad header\n".into(), "value".into());

        let error = build_auth_header_map(&headers).unwrap_err();

        assert!(matches!(error, AiHistoryError::BadHeaderName(_)));
    }
}

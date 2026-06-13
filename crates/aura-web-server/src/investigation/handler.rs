//! `POST /v1/launch_investigation`: the webhook entry point.

use std::collections::HashMap;
use std::sync::Arc;

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{error, info};
use uuid::Uuid;

use crate::investigation::client::CreateInvestigationRequest;
use crate::investigation::runner::{InvestigationRunnerContext, run_investigation};
use crate::investigation::{REQUIRED_AUTH_HEADERS, client::InvestigationState};
use crate::types::AppState;

/// Webhook payload. `error` is permissive for now: any string or JSON value.
/// We might narrow that down in the future.
#[derive(Debug, Deserialize)]
pub struct LaunchInvestigationRequest {
    pub source: String,
    pub error: Value,
}

#[derive(Debug, Serialize)]
struct LaunchInvestigationResponse {
    investigation_id: String,
    task_id: String,
    state: InvestigationState,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

/// ai-history-service caps `trigger_source` at 254 and `evidence_refs` at 100k.
const TRIGGER_SOURCE_MAX: usize = 254;
const EVIDENCE_REFS_MAX: usize = 100_000;

#[tracing::instrument(name = "launch_investigation", skip(state, request, headers), fields(otel.kind = "server"))]
pub async fn launch_investigation(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<LaunchInvestigationRequest>,
) -> Response {
    // To validate: which headers do we take in
    // How to differentiate between internal and external callers
    let auth_headers = match extract_auth_headers(&headers) {
        Ok(hashmap) => hashmap,
        Err(missing) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(ErrorBody {
                    error: format!("missing required auth header(s): {}", missing.join(", ")),
                }),
            )
                .into_response();
        }
    };

    // TODO do we want to move this validation to the schema?
    if request.source.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                error: "`source` must not be empty".to_owned(),
            }),
        )
            .into_response();
    }

    // TODO do we want to move this validation to the schema?
    let evidence = match request.error {
        Value::String(s) => s,
        Value::Null => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorBody {
                    error: "`error` must not be null".to_owned(),
                }),
            )
                .into_response();
        }
        other => serde_json::to_string(&other).unwrap_or_default(),
    };

    // TODO do we want to move this validation to the schema?
    if evidence.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                error: "`error` must not be empty".to_owned(),
            }),
        )
            .into_response();
    }

    let create_body = CreateInvestigationRequest {
        trigger_source: truncate(&request.source, TRIGGER_SOURCE_MAX),
        linked_alert_id: Uuid::new_v4().to_string(),
        evidence_refs: truncate(&evidence, EVIDENCE_REFS_MAX),
        state: Some(InvestigationState::Triggered),
    };

    let record = match state
        .ai_history_client
        .create(&auth_headers, &create_body)
        .await
    {
        Ok(record) => record,
        Err(error) => {
            error!(%error, "Failed to create investigation in ai-history-service");

            // TODO what error do we want to give here?
            // exposing the fact that we have an `ai-history-service` really is an implementation detail.
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorBody {
                    error: format!("ai-history-service create failed: {}", error),
                }),
            )
                .into_response();
        }
    };

    let investigation_id = record.id.clone();

    // Reuse the same task store as the A2A executor so investigations surface
    // through GET /a2a/v1/tasks/{investigation_id}.
    let task_store = state.a2a_task_store.clone();

    let runner_ctx = InvestigationRunnerContext {
        investigation_id: investigation_id.clone(),
        source: request.source,
        error_text: evidence,
        auth_headers,
        task_store,
    };

    info!(
        investigation_id = %investigation_id,
        "Investigation created; spawning runner"
    );

    tokio::spawn(run_investigation(state.clone(), runner_ctx));

    (
        StatusCode::ACCEPTED,
        Json(LaunchInvestigationResponse {
            investigation_id: investigation_id.clone(),
            task_id: investigation_id,
            state: InvestigationState::Triggered,
        }),
    )
        .into_response()
}

/// Returns the auth-header map on success, or the list of missing header names on failure.
fn extract_auth_headers(headers: &HeaderMap) -> Result<HashMap<String, String>, Vec<&'static str>> {
    let mut out = HashMap::with_capacity(REQUIRED_AUTH_HEADERS.len());

    let mut missing: Vec<&'static str> = Vec::new();

    for name in REQUIRED_AUTH_HEADERS {
        match headers.get(*name).and_then(|v| v.to_str().ok()) {
            Some(v) if !v.is_empty() => {
                out.insert((*name).to_owned(), v.to_owned());
            }
            _ => missing.push(name),
        }
    }

    if missing.is_empty() {
        Ok(out)
    } else {
        Err(missing)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        // Truncate on a char boundary to keep UTF-8 well-formed.
        let end = s.floor_char_boundary(max);
        s[..end].to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::{HeaderMap, extract_auth_headers, truncate};
    use axum::http::HeaderValue;

    fn make_headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn extract_auth_headers_happy_path() {
        let h = make_headers(&[
            ("x-auth-subject-id", "sub"),
            ("x-auth-subject-email", "e@x"),
            ("x-auth-account-id", "acc"),
            ("x-auth-account-short-id", "short"),
            ("x-auth-account-plan", "pro7"),
        ]);
        let out = extract_auth_headers(&h).unwrap();
        assert_eq!(out.len(), 5);
        assert_eq!(
            out.get("x-auth-subject-id").map(String::as_str),
            Some("sub")
        );
    }

    #[test]
    fn extract_auth_headers_missing_reported() {
        let h = make_headers(&[
            ("x-auth-subject-id", "sub"),
            // missing email
            ("x-auth-account-id", "acc"),
            ("x-auth-account-short-id", "short"),
            ("x-auth-account-plan", "pro7"),
        ]);
        let err = extract_auth_headers(&h).unwrap_err();
        assert_eq!(err, vec!["x-auth-subject-email"]);
    }

    #[test]
    fn extract_auth_headers_empty_value_counted_as_missing() {
        let h = make_headers(&[
            ("x-auth-subject-id", ""),
            ("x-auth-subject-email", "e@x"),
            ("x-auth-account-id", "acc"),
            ("x-auth-account-short-id", "short"),
            ("x-auth-account-plan", "pro7"),
        ]);
        let err = extract_auth_headers(&h).unwrap_err();
        assert_eq!(err, vec!["x-auth-subject-id"]);
    }

    #[test]
    fn truncate_handles_ascii() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hello");
    }

    #[test]
    fn truncate_keeps_utf8_well_formed() {
        // 'é' is 2 bytes in UTF-8.
        let s = "héllo";
        // Cap of 2 lands inside the multi-byte char; truncate must back off to a boundary.
        let out = truncate(s, 2);
        assert!(out.is_char_boundary(out.len()));
        assert_eq!(out, "h");
    }
}

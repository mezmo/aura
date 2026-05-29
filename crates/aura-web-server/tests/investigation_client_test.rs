//! Integration test for `AiHistoryClient` against a stub HTTP server that
//! plays the shape of the ai-history-service `/internal/investigation` API.
//!
//! Verifies:
//! - POST body shape and serialization
//! - PATCH body omits None fields
//! - Auth headers forwarded verbatim
//! - Envelope `{ data: ... }` decoded into `InvestigationRecord`
//! - HTTP error paths surface as `AiHistoryError::Http { status, body }`

use std::collections::HashMap;
use std::sync::Arc;

use aura_web_server::investigation::{
    AiHistoryClient, AiHistoryError, CreateInvestigationRequest, UpdateInvestigationRequest,
    client::InvestigationState,
};
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{patch, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::sync::Mutex;

#[derive(Default)]
struct Recorded {
    create_body: Option<Value>,
    create_headers: HashMap<String, String>,
    update_body: Option<Value>,
    update_headers: HashMap<String, String>,
    fail_create: bool,
}

type SharedRecorded = Arc<Mutex<Recorded>>;

async fn handle_create(
    State(recorded): State<SharedRecorded>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let mut guard = recorded.lock().await;

    guard.create_body = Some(body);
    guard.create_headers = headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_string()))
        .collect();
    if guard.fail_create {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Body::from("downstream exploded"),
        )
            .into_response();
    }
    (
        StatusCode::CREATED,
        Json(json!({
            "meta": { "type": "ai-investigation" },
            "data": {
                "id": "60c1234567890abcdef01234",
                "state": "triggered",
                "trigger_source": "pipeline"
            }
        })),
    )
        .into_response()
}

async fn handle_update(
    State(recorded): State<SharedRecorded>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let mut guard = recorded.lock().await;

    guard.update_body = Some(body);
    guard.update_headers = headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_string()))
        .collect();
    (
        StatusCode::OK,
        Json(json!({
            "meta": { "type": "ai-investigation" },
            "data": {
                "id": id,
                "state": "completed"
            }
        })),
    )
        .into_response()
}

async fn spawn_stub(rec: SharedRecorded) -> url::Url {
    let app = Router::new()
        .route("/internal/investigation", post(handle_create))
        .route(
            "/internal/investigation/{investigation_id}",
            patch(handle_update),
        )
        .with_state(rec);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    format!("http://{}", address).parse().unwrap()
}

fn auth_headers() -> HashMap<String, String> {
    [
        ("x-auth-subject-id".to_string(), "sub-1".to_string()),
        (
            "x-auth-subject-email".to_string(),
            "u@example.com".to_string(),
        ),
        ("x-auth-account-id".to_string(), "acc-1".to_string()),
        ("x-auth-account-short-id".to_string(), "short1".to_string()),
        ("x-auth-account-plan".to_string(), "pro7".to_string()),
    ]
    .into_iter()
    .collect::<HashMap<_, _>>()
}

#[tokio::test]
async fn create_posts_correct_body_and_forwards_headers() {
    let recorded = Arc::new(Mutex::new(Recorded::default()));

    let base = spawn_stub(recorded.clone()).await;

    let client = AiHistoryClient::new(reqwest::Client::new(), base);

    let result = client
        .create(
            &auth_headers(),
            &CreateInvestigationRequest {
                trigger_source: "pipeline".to_string(),
                linked_alert_id: "alert-1".to_string(),
                evidence_refs: "stuff went wrong".to_string(),
                state: Some(InvestigationState::Triggered),
            },
        )
        .await
        .expect("create should succeed");

    assert_eq!(result.id, "60c1234567890abcdef01234");
    assert_eq!(result.state, Some(InvestigationState::Triggered));

    let guard = recorded.lock().await;
    let body = guard.create_body.as_ref().unwrap();
    assert_eq!(body["trigger_source"], "pipeline");
    assert_eq!(body["linked_alert_id"], "alert-1");
    assert_eq!(body["evidence_refs"], "stuff went wrong");
    assert_eq!(body["state"], "triggered");

    // Auth headers forwarded verbatim (the server normalizes names to lowercase).
    assert_eq!(
        guard
            .create_headers
            .get("x-auth-subject-id")
            .map(String::as_str),
        Some("sub-1")
    );
    assert_eq!(
        guard
            .create_headers
            .get("x-auth-account-id")
            .map(String::as_str),
        Some("acc-1")
    );
    assert_eq!(
        guard
            .create_headers
            .get("x-auth-account-short-id")
            .map(String::as_str),
        Some("short1")
    );
}

#[tokio::test]
async fn update_patches_only_provided_fields() {
    let recorded = Arc::new(Mutex::new(Recorded::default()));
    let base = spawn_stub(recorded.clone()).await;
    let client = AiHistoryClient::new(reqwest::Client::new(), base);

    client
        .update(
            &auth_headers(),
            "60c1234567890abcdef01234",
            &UpdateInvestigationRequest {
                state: Some(InvestigationState::Completed),
                confidence_score: Some(0.42),
                suggested_resolution: None,
                resolution_status: Some("failed".to_string()),
            },
        )
        .await
        .expect("update should succeed");

    let guard = recorded.lock().await;
    let body = guard.update_body.as_ref().unwrap();

    assert_eq!(body["state"], "completed");
    assert_eq!(body["confidence_score"], 0.42);
    assert_eq!(body["resolution_status"], "failed");
    assert!(body.get("suggested_resolution").is_none());
}

#[tokio::test]
async fn http_error_surfaces_as_typed_error() {
    let recorded = Arc::new(Mutex::new(Recorded {
        fail_create: true,
        ..Default::default()
    }));
    let base = spawn_stub(recorded.clone()).await;
    let client = AiHistoryClient::new(reqwest::Client::new(), base);

    let error = client
        .create(
            &auth_headers(),
            &CreateInvestigationRequest {
                trigger_source: "x".to_string(),
                linked_alert_id: "y".to_string(),
                evidence_refs: "z".to_string(),
                state: None,
            },
        )
        .await
        .expect_err("create should fail");

    match error {
        AiHistoryError::Http { status, body } => {
            assert_eq!(status, 500);
            assert!(body.contains("downstream exploded"));
        }
        other => panic!("expected AiHistoryError::Http, got: {:?}", other),
    }
}

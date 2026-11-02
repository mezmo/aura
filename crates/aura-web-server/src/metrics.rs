//! The Prometheus scrape endpoint and the HTTP request metric.
//!
//! `/metrics` renders every instrument `aura::metrics` holds. It is mounted
//! on the API port unless `--metrics-listen` names a second address, in
//! which case `listener` serves it there instead. Either way it sits outside
//! the shutdown guard, so a draining server can still be scraped.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::extract::{MatchedPath, Request};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::handlers;
use crate::types::AppState;

/// `GET /metrics`: the Prometheus text exposition.
pub async fn scrape() -> Response {
    match aura::metrics::render_prometheus() {
        Ok(body) => (
            [(header::CONTENT_TYPE, aura::metrics::PROMETHEUS_CONTENT_TYPE)],
            body,
        )
            .into_response(),
        Err(e) => {
            error!("metrics scrape failed: {e}");
            (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response()
        }
    }
}

/// The scrape route alone, for merging into the API router.
pub fn router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new().route("/metrics", get(scrape))
}

/// Records `http.server.request.duration` for every matched route. Applied
/// with `Router::layer` after every route is mounted so it sees the matched
/// path; unmatched requests (404 by path) are not recorded, which keeps
/// arbitrary paths out of the label set. For a streaming response the
/// duration ends when the headers go out, not when the stream closes; the
/// run metric covers the stream.
pub async fn record_request(request: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = request.method().clone();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_owned());
    let response = next.run(request).await;
    if let Some(route) = route {
        aura::metrics::record_http_request(
            &route,
            method.as_str(),
            response.status().as_u16(),
            started.elapsed(),
        );
    }
    response
}

/// Serve `router` on `addr` until `exit` is cancelled. Binding happens
/// before this returns, so a taken port fails startup rather than a scrape.
pub async fn listener(
    addr: SocketAddr,
    state: Arc<AppState>,
    exit: CancellationToken,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("Serving /metrics on {addr}");
    // `/health` too, so the metrics port can answer a liveness probe.
    let app = router::<Arc<AppState>>()
        .route("/health", get(handlers::health))
        .with_state(state)
        .layer(axum::middleware::from_fn(record_request));
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app)
            .with_graceful_shutdown(async move { exit.cancelled().await })
            .await
        {
            error!("metrics listener failed: {e}");
        }
    });
    Ok(())
}

#[cfg(all(test, feature = "otel"))]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn body_text(response: Response) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn scrape_serves_prometheus_text_and_the_request_metric() {
        aura::metrics::init().unwrap();
        let app = router::<()>().layer(axum::middleware::from_fn(record_request));

        let first = app
            .clone()
            .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(
            first.headers()[header::CONTENT_TYPE],
            aura::metrics::PROMETHEUS_CONTENT_TYPE
        );
        assert!(body_text(first).await.contains("target_info{"));

        // The first scrape was itself a matched request, so the second one
        // reports it. An unmatched path is answered but never labelled.
        let missing = app
            .clone()
            .oneshot(Request::get("/nowhere").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);

        let second = app
            .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let text = body_text(second).await;
        assert!(
            text.contains(
                r#"http_server_request_duration_seconds_count{http_request_method="GET",http_response_status_code="200",http_route="/metrics"}"#
            ),
            "{text}"
        );
        assert!(!text.contains("/nowhere"), "{text}");
    }
}

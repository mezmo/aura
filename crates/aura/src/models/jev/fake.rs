//! Test transports over jev-driver's: no network.

use std::sync::Arc;

use async_trait::async_trait;
use jev_driver::JevError;
use jev_driver::client::Transport;
use jev_driver::wire::WireResponse;

/// jev-driver's scripted [`jev_driver::fake::FakeTransport`], scripted with
/// JSON response bodies (`Ok`) and HTTP statuses (`Err`).
pub struct FakeTransport(jev_driver::fake::FakeTransport);

impl FakeTransport {
    pub fn new(script: impl IntoIterator<Item = Result<serde_json::Value, u16>>) -> Arc<Self> {
        let script = script
            .into_iter()
            .map(|step| match step {
                Ok(body) => serde_json::from_value::<WireResponse>(body)
                    .map_err(|e| JevError::Transport(format!("invalid response body: {e}"))),
                Err(status) => Err(status_error(status)),
            })
            .collect();
        Arc::new(Self(jev_driver::fake::FakeTransport::new(script)))
    }

    /// Request bodies received so far.
    pub fn requests(&self) -> Vec<String> {
        self.0.requests()
    }
}

#[async_trait]
impl Transport for FakeTransport {
    async fn post_systemone(&self, body: String) -> Result<WireResponse, JevError> {
        self.0.post_systemone(body).await
    }
}

/// A transport whose requests never complete.
pub struct HangingTransport;

#[async_trait]
impl Transport for HangingTransport {
    async fn post_systemone(&self, _body: String) -> Result<WireResponse, JevError> {
        std::future::pending().await
    }
}

/// The error jev-driver's HTTP transport reports for `status`.
fn status_error(status: u16) -> JevError {
    match status {
        401 => JevError::Unauthorized,
        422 => JevError::InvalidRequest(format!("status {status}")),
        429 => JevError::RateLimited { retries: 0 },
        529 => JevError::Overloaded { retries: 0 },
        _ => JevError::Transport(format!("unexpected status {status}")),
    }
}

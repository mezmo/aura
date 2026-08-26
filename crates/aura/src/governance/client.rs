//! HTTP client for sending catalog snapshots to the governance webhook.

use std::collections::HashMap;
use std::time::Duration;

use aura_config::CatalogWebhookConfig;
use reqwest::header::{CONTENT_TYPE, HeaderMap};
use tracing::info;

use super::envelope::CatalogEnvelope;
use crate::hitl::{SigningContext, WebhookHmac};

/// Maximum time to wait for a TCP connection before failing.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Errors that can occur while sending a catalog to the governance webhook.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("catalog webhook transport error: {0}")]
    Transport(String),
    #[error("catalog webhook returned status {status}: {body}")]
    BadStatus { status: u16, body: String },
    #[error("catalog webhook HMAC signing failed: {0}")]
    Signing(String),
    #[error("catalog serialization failed: {0}")]
    Serialization(String),
}

/// HTTP client for the governance catalog webhook.
pub struct CatalogClient {
    client: reqwest::Client,
    url: String,
    timeout: Duration,
    headers: HeaderMap,
    hmac: Option<WebhookHmac>,
}

impl CatalogClient {
    /// Build a catalog client from config.
    ///
    /// `req_headers` is the inbound request headers for `headers_from_request`
    /// resolution. Pass `None` in CLI standalone mode.
    pub fn from_config(
        config: &CatalogWebhookConfig,
        req_headers: Option<&HashMap<String, String>>,
    ) -> Result<Self, CatalogError> {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .expect("reqwest client builder only fails on TLS backend init");

        let headers = crate::webhook_utils::resolve_headers(
            &config.headers,
            &config.headers_from_request,
            req_headers,
        );
        let hmac = crate::webhook_utils::build_hmac_from_secret(
            config.hmac.as_ref().map(|c| c.secret.as_str()),
        )
        .map_err(|e| CatalogError::Signing(e.to_string()))?;
        let timeout = Duration::from_secs(config.timeout_secs);

        Ok(Self {
            client,
            url: config.url.as_str().to_string(),
            timeout,
            headers,
            hmac,
        })
    }

    /// Send a catalog envelope to the governance webhook.
    pub async fn send(&self, envelope: &CatalogEnvelope) -> Result<(), CatalogError> {
        let body =
            serde_json::to_vec(envelope).map_err(|e| CatalogError::Serialization(e.to_string()))?;

        let mut request = self
            .client
            .post(&self.url)
            .timeout(self.timeout)
            .headers(self.headers.clone())
            .header(CONTENT_TYPE, "application/json");

        // Add HMAC signature if configured
        if let Some(hmac) = &self.hmac {
            let context = SigningContext::new(&format!("catalog-sync:{}", envelope.event_id))
                .map_err(|e| CatalogError::Signing(e.to_string()))?;
            let signed = hmac
                .sign(&context, &body)
                .map_err(|e| CatalogError::Signing(e.to_string()))?;
            for (name, value) in signed.into_pairs() {
                request = request.header(name, value);
            }
        }

        let response = request
            .body(body)
            .send()
            .await
            .map_err(|e| CatalogError::Transport(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<failed to read body>".to_string());
            return Err(CatalogError::BadStatus {
                status: status.as_u16(),
                body,
            });
        }

        info!(url = %self.url, event_id = %envelope.event_id, "Catalog sync successful");
        Ok(())
    }
}

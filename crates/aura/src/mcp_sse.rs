//! Legacy SSE MCP transport.
//!
//! Implements the pre-2025-11-05 MCP SSE protocol:
//! 1. HTTP GET to SSE endpoint
//! 2. Server responds with SSE stream containing an `endpoint` event
//! 3. Client POSTs JSON-RPC messages to the resolved message endpoint
//! 4. Server responses arrive as `event: message` on the SSE stream

use std::collections::HashMap;
use std::pin::Pin;

use futures::{Stream, StreamExt};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::{RoleClient, transport::Transport};
use sse_stream::{Error as SseError, Sse};
use tracing::{debug, warn};

use crate::error::SseTransportError;

/// SSE event type for JSON-RPC messages from server to client.
const SSE_EVENT_MESSAGE: &str = "message";
/// SSE event type for endpoint discovery (first event after connection).
const SSE_EVENT_ENDPOINT: &str = "endpoint";

/// MCP client transport over legacy SSE protocol.
///
/// State: after `connect()` the transport holds a live SSE stream and a resolved
/// message endpoint. `close()` consumes the stream (sets it to `None`).
pub struct SseTransport {
    http_client: reqwest::Client,
    message_endpoint: url::Url,
    #[allow(clippy::type_complexity)]
    stream: Option<Pin<Box<dyn Stream<Item = Result<Sse, SseError>> + Send>>>,
}

impl Transport<RoleClient> for SseTransport {
    type Error = SseTransportError;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        let client = self.http_client.clone();
        let uri = self.message_endpoint.clone();
        async move {
            let response = client
                .post(uri.as_str())
                .json(&item)
                .send()
                .await
                .map_err(SseTransportError::Http)?;
            let _ = response
                .error_for_status()
                .map_err(SseTransportError::Http)?;
            Ok(())
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        let stream = self.stream.as_mut()?;
        loop {
            let sse = match stream.next().await {
                Some(Ok(sse)) => sse,
                Some(Err(e)) => {
                    warn!("SSE stream error: {}", e);
                    return None;
                }
                None => {
                    debug!("SSE stream closed by server");
                    return None;
                }
            };
            if let (Some(SSE_EVENT_MESSAGE), Some(data)) = (sse.event.as_deref(), sse.data) {
                match serde_json::from_str(&data) {
                    Ok(msg) => return Some(msg),
                    Err(e) => warn!("Failed to deserialize SSE message: {}", e),
                }
            }
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.stream.take();
        Ok(())
    }
}

impl SseTransport {
    /// Connect to an SSE MCP server.
    ///
    /// 1. GET the SSE endpoint with `Accept: text/event-stream`
    /// 2. Verify the response Content-Type
    /// 3. Read SSE events until the `endpoint` event is received
    /// 4. Resolve the message endpoint URL
    /// 5. Return a connected `SseTransport`
    pub async fn connect(
        url: &str,
        headers: &HashMap<String, String>,
    ) -> Result<Self, SseTransportError> {
        let sse_endpoint = url::Url::parse(url)?;

        let mut header_map = HeaderMap::new();
        for (key, value) in headers {
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

        let http_client = reqwest::Client::builder()
            .default_headers(header_map)
            .build()
            .map_err(SseTransportError::Http)?;

        let response = http_client
            .get(url.to_string())
            .header(ACCEPT, "text/event-stream")
            .send()
            .await
            .map_err(SseTransportError::Http)?;

        let response = response
            .error_for_status()
            .map_err(SseTransportError::Http)?;

        let content_types: Vec<_> = response
            .headers()
            .get_all(CONTENT_TYPE)
            .into_iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        if content_types.is_empty() {
            return Err(SseTransportError::MissingContentType);
        }
        if !content_types
            .iter()
            .any(|ct| ct.starts_with("text/event-stream"))
        {
            return Err(SseTransportError::UnexpectedContentType(
                content_types[0].to_owned(),
            ));
        }

        let mut stream = sse_stream::SseStream::from_byte_stream(response.bytes_stream()).boxed();

        let message_endpoint = loop {
            let sse = stream
                .next()
                .await
                .transpose()
                .map_err(SseTransportError::SseStream)?
                .ok_or(SseTransportError::MissingEndpointEvent)?;
            if let (Some(SSE_EVENT_ENDPOINT), Some(ep)) = (sse.event.as_deref(), sse.data) {
                break resolve_message_endpoint(sse_endpoint, ep)?;
            }
        };

        Ok(Self {
            http_client,
            message_endpoint,
            stream: Some(stream),
        })
    }
}

/// Resolve a message endpoint URL against the SSE base URL per RFC 3986.
fn resolve_message_endpoint(
    base: url::Url,
    endpoint: String,
) -> Result<url::Url, SseTransportError> {
    base.join(&endpoint).map_err(SseTransportError::UrlParse)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_message_endpoint_query_only() {
        let base = url::Url::parse("https://localhost/sse").unwrap();
        let result = resolve_message_endpoint(base, "?sessionId=x".to_owned()).unwrap();
        assert_eq!(result.as_str(), "https://localhost/sse?sessionId=x");
    }

    #[test]
    fn test_resolve_message_endpoint_relative_path() {
        let base = url::Url::parse("https://localhost/sse").unwrap();
        let result = resolve_message_endpoint(base, "mypath?sessionId=x".to_owned()).unwrap();
        assert_eq!(result.as_str(), "https://localhost/mypath?sessionId=x");
    }

    #[test]
    fn test_resolve_message_endpoint_absolute_path() {
        let base = url::Url::parse("https://localhost/sse").unwrap();
        let result = resolve_message_endpoint(base, "/xxx?sessionId=x".to_owned()).unwrap();
        assert_eq!(result.as_str(), "https://localhost/xxx?sessionId=x");
    }

    #[test]
    fn test_resolve_message_endpoint_full_url() {
        let base = url::Url::parse("https://localhost/sse").unwrap();
        let result =
            resolve_message_endpoint(base, "http://example.com/xxx?sessionId=x".to_owned())
                .unwrap();
        assert_eq!(result.as_str(), "http://example.com/xxx?sessionId=x");
    }

    #[test]
    fn test_resolve_message_endpoint_subpath_relative() {
        let base = url::Url::parse("https://example.com/api/mcp/sse").unwrap();
        let result = resolve_message_endpoint(base, "messages?sessionId=x".to_owned()).unwrap();
        assert_eq!(
            result.as_str(),
            "https://example.com/api/mcp/messages?sessionId=x"
        );
    }

    #[test]
    fn test_resolve_message_endpoint_subpath_query() {
        let base = url::Url::parse("https://example.com/api/mcp/sse").unwrap();
        let result = resolve_message_endpoint(base, "?sessionId=x".to_owned()).unwrap();
        assert_eq!(
            result.as_str(),
            "https://example.com/api/mcp/sse?sessionId=x"
        );
    }
}

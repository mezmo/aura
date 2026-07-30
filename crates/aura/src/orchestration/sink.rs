//! Where a run's orchestration events go.
//!
//! An attended run streams events over its request's SSE channel and dies
//! with it. A reified headless run has no request stream, so the same loop
//! must emit through an abstraction: the channel adapter for attended runs,
//! the session's event bus for headless ones.

use std::sync::Arc;

use async_trait::async_trait;

use crate::provider_agent::{StreamError, StreamItem};
use crate::session_store::EventBus;

use super::events::OrchestratorEvent;
use super::park::SessionId;

/// One emission surface for orchestration events, independent of whether a
/// request stream exists.
#[async_trait]
pub trait RunEventSink: Send + Sync {
    /// Deliver one event. Delivery is best-effort: a gone consumer must not
    /// fail the run.
    async fn emit(&self, event: OrchestratorEvent);
}

/// Adapter over an attended request's stream channel.
pub struct ChannelEventSink {
    tx: tokio::sync::mpsc::Sender<Result<StreamItem, StreamError>>,
}

impl ChannelEventSink {
    #[must_use]
    pub fn new(tx: tokio::sync::mpsc::Sender<Result<StreamItem, StreamError>>) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl RunEventSink for ChannelEventSink {
    async fn emit(&self, event: OrchestratorEvent) {
        let _ = self.tx.send(Ok(StreamItem::OrchestratorEvent(event))).await;
    }
}

/// Sink for headless (reified) runs: events publish to the session's bus
/// topic, where retrieval and cross-instance observers can read them.
pub struct BusEventSink {
    #[expect(dead_code, reason = "staged for #271: read by the headless emit fill")]
    bus: Arc<dyn EventBus>,
    #[expect(dead_code, reason = "staged for #271: read by the headless emit fill")]
    session: SessionId,
}

impl BusEventSink {
    #[must_use]
    pub fn new(bus: Arc<dyn EventBus>, session: SessionId) -> Self {
        Self { bus, session }
    }
}

#[async_trait]
impl RunEventSink for BusEventSink {
    #[expect(unused_variables, reason = "staged for #271: headless event emit")]
    async fn emit(&self, event: OrchestratorEvent) {
        todo!("staged for #271: headless event encoding and topic naming")
    }
}

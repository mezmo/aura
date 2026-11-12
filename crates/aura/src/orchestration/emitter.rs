//! The emitter the orchestrator sends all its events through. Each call
//! sends to two places: the item stream, which the request-bound paths read,
//! and the run's event channel, which the runtime writes to the session's
//! journal.

use std::sync::Arc;

use aura_events::agent::{AgentEvent, AgentEventPayload};
use aura_events::{EmittingAgent, TokenCount, TokenUsage};
use tokio::sync::mpsc;

use crate::provider_agent::{StreamError, StreamItem, StreamedAssistantContent};
use crate::run_context::RunContext;

/// An orchestrated run's sender for its events, to its item stream and its
/// event channel.
#[derive(Clone)]
pub(crate) struct Emitter {
    items: mpsc::Sender<Result<StreamItem, StreamError>>,
    run: Arc<RunContext>,
    /// The turn to send, shared by every clone.
    turn: Arc<tokio::sync::Mutex<()>>,
}

impl Emitter {
    /// Creates an emitter that sends to `items` (the run's item stream) and
    /// to the event channel of `run`.
    pub(crate) fn new(
        items: mpsc::Sender<Result<StreamItem, StreamError>>,
        run: Arc<RunContext>,
    ) -> Self {
        Self {
            items,
            run,
            turn: Arc::default(),
        }
    }

    /// Sends `event` as a `StreamItem::AgentEvent` on the item stream and
    /// emits it on the run's channel.
    pub(crate) async fn emit(&self, event: AgentEvent) {
        self.emit_item(StreamItem::AgentEvent(Box::new(event)))
            .await;
    }

    /// Sends `item` unchanged on the item stream, then sends the matching
    /// event (see `channel_event`) on the run's channel.
    ///
    /// The two sends happen in one turn, which every clone of this emitter
    /// waits for. So when clones send from different tasks, as the
    /// orchestrator and its tool-event forwarder do, both streams still get
    /// the same order. Waiting for the turn cannot deadlock, because the
    /// readers of the two streams read both in one loop.
    ///
    /// Both sends are best effort. If the reader of either stream has gone
    /// away, what is sent to it is dropped and the run carries on.
    pub(crate) async fn emit_item(&self, item: StreamItem) {
        let event = channel_event(&item);
        let _turn = self.turn.lock().await;
        let _ = self.items.send(Ok(item)).await;
        if let Some(event) = event {
            self.run.emit(event).await;
        }
    }
}

/// The event to put on the run's channel for `item`, if it has one. The
/// run's text, reasoning deltas and final response go out as the
/// coordinator's `TextDelta`, `Reasoning` and `Completed` events.
fn channel_event(item: &StreamItem) -> Option<AgentEvent> {
    match item {
        StreamItem::AgentEvent(event) => Some(AgentEvent::clone(event)),
        StreamItem::StreamAssistantItem(StreamedAssistantContent::Text(content)) => {
            Some(AgentEvent::new(
                EmittingAgent::coordinator(),
                AgentEventPayload::TextDelta {
                    content: content.clone(),
                },
            ))
        }
        StreamItem::StreamAssistantItem(StreamedAssistantContent::ReasoningDelta {
            delta, ..
        }) => Some(AgentEvent::new(
            EmittingAgent::coordinator(),
            AgentEventPayload::Reasoning {
                content: delta.clone(),
                task_id: None,
            },
        )),
        StreamItem::Final(response) => Some(AgentEvent::new(
            EmittingAgent::coordinator(),
            AgentEventPayload::Completed {
                content: response.content.clone(),
                usage: TokenUsage {
                    prompt_tokens: TokenCount::new(response.usage.input_tokens),
                    completion_tokens: TokenCount::new(response.usage.output_tokens),
                    total_tokens: TokenCount::new(response.usage.total_tokens),
                },
            },
        )),
        StreamItem::ContextUsage {
            agent_id,
            context_tokens,
            response_tokens,
            context_window,
        } => Some(AgentEvent::new(
            EmittingAgent::orchestrated(agent_id),
            AgentEventPayload::ContextUsage {
                context_tokens: TokenCount::new(*context_tokens),
                response_tokens: TokenCount::new(*response_tokens),
                context_window: context_window.map(TokenCount::new),
            },
        )),
        StreamItem::ScratchpadUsage {
            agent_id,
            tokens_intercepted,
            tokens_extracted,
        } => Some(AgentEvent::new(
            EmittingAgent::orchestrated(agent_id),
            AgentEventPayload::ScratchpadUsage {
                tokens_intercepted: TokenCount::new(*tokens_intercepted as u64),
                tokens_extracted: TokenCount::new(*tokens_extracted as u64),
            },
        )),
        // The coordinator holds the run's MCP connections and the workers
        // share them, so the status is reported as the coordinator's.
        StreamItem::McpStatus(servers) => Some(AgentEvent::new(
            EmittingAgent::coordinator(),
            AgentEventPayload::McpStatus {
                servers: servers.clone(),
            },
        )),
        StreamItem::StreamAssistantItem(
            StreamedAssistantContent::ToolCall(_)
            | StreamedAssistantContent::ToolCallDelta { .. }
            | StreamedAssistantContent::Reasoning(_),
        )
        | StreamItem::StreamUserItem(_)
        | StreamItem::FinalMarker
        | StreamItem::TurnUsage(..) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use aura_events::McpServerStatus;

    type Items = mpsc::Receiver<Result<StreamItem, StreamError>>;
    type Events = mpsc::Receiver<AgentEvent>;

    fn emitter() -> (Emitter, Items, Events) {
        let (items, item_rx) = mpsc::channel(8);
        let (run, events) = RunContext::channel(aura_events::RunId::mint());
        (Emitter::new(items, run), item_rx, events)
    }

    /// Drops the emitter, then returns every item it sent and every event
    /// its run's channel received.
    fn drain(
        emitter: Emitter,
        mut items: Items,
        mut events: Events,
    ) -> (Vec<StreamItem>, Vec<AgentEvent>) {
        drop(emitter);
        let items = std::iter::from_fn(|| items.try_recv().ok())
            .map(|item| item.expect("the emitter sends no errors"))
            .collect();
        let events = std::iter::from_fn(|| events.try_recv().ok()).collect();
        (items, events)
    }

    /// Two clones of one emitter, sending at once from two tasks, put the
    /// same order on both streams. The orchestrator and its tool-event
    /// forwarder send through clones of one emitter this way.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn clones_sending_at_once_keep_both_streams_in_one_order() {
        const EACH: u64 = 2_000;
        let (items, mut item_rx) = mpsc::channel(8);
        let (run, mut events) = RunContext::channel(aura_events::RunId::mint());
        let emitter = Emitter::new(items, run);
        let sender = |emitter: Emitter, from: u64| {
            tokio::spawn(async move {
                for iteration in from..from + EACH {
                    emitter
                        .emit(AgentEvent::new(
                            EmittingAgent::coordinator(),
                            AgentEventPayload::Synthesizing {
                                iteration: iteration as usize,
                            },
                        ))
                        .await;
                }
            })
        };
        let iteration = |payload: &AgentEventPayload| match payload {
            AgentEventPayload::Synthesizing { iteration } => *iteration,
            other => panic!("unexpected payload {other:?}"),
        };
        let item_order = tokio::spawn(async move {
            let mut order = Vec::new();
            while let Some(item) = item_rx.recv().await {
                if let Ok(StreamItem::AgentEvent(event)) = item {
                    order.push(iteration(&event.payload));
                }
            }
            order
        });
        let event_order = tokio::spawn(async move {
            let mut order = Vec::new();
            while let Some(event) = events.recv().await {
                order.push(iteration(&event.payload));
            }
            order
        });

        let a = sender(emitter.clone(), 0);
        let b = sender(emitter, EACH);
        a.await.unwrap();
        b.await.unwrap();

        let item_order = item_order.await.unwrap();
        let event_order = event_order.await.unwrap();
        assert_eq!(item_order.len(), 2 * EACH as usize);
        assert_eq!(item_order, event_order, "both streams carry one order");
    }

    #[tokio::test]
    async fn an_event_reaches_both_streams_once() {
        let (emitter, items, events) = emitter();
        emitter
            .emit(AgentEvent::new(
                EmittingAgent::coordinator(),
                AgentEventPayload::Synthesizing { iteration: 2 },
            ))
            .await;

        let (items, events) = drain(emitter, items, events);
        let [StreamItem::AgentEvent(item)] = items.as_slice() else {
            panic!("expected one agent event item, got {items:?}");
        };
        let [event] = events.as_slice() else {
            panic!("expected one channel event, got {events:?}");
        };
        assert!(matches!(
            item.payload,
            AgentEventPayload::Synthesizing { iteration: 2 }
        ));
        assert!(matches!(
            event.payload,
            AgentEventPayload::Synthesizing { iteration: 2 }
        ));
        assert_eq!(item.agent, event.agent);
    }

    /// A context reading goes on the item stream unchanged, for the
    /// request-bound paths to render, and its channel event names the same
    /// agent as the item. Under orchestration the conversation's reading
    /// (`main`) must not look like a single agent's.
    #[tokio::test]
    async fn a_context_reading_keeps_its_item_and_names_its_agent() {
        let (emitter, items, events) = emitter();
        emitter
            .emit_item(StreamItem::ContextUsage {
                agent_id: aura_events::CONVERSATION_AGENT_ID.to_string(),
                context_tokens: 1_200,
                response_tokens: 80,
                context_window: Some(200_000),
            })
            .await;

        let (items, events) = drain(emitter, items, events);
        assert!(
            matches!(
                items.as_slice(),
                [StreamItem::ContextUsage {
                    agent_id,
                    context_tokens: 1_200,
                    response_tokens: 80,
                    context_window: Some(200_000),
                }] if agent_id == aura_events::CONVERSATION_AGENT_ID
            ),
            "{items:?}"
        );
        let [event] = events.as_slice() else {
            panic!("expected one channel event, got {events:?}");
        };
        assert_eq!(event.agent.agent_id, aura_events::CONVERSATION_AGENT_ID);
        assert!(!event.agent.is_single_agent());
        let AgentEventPayload::ContextUsage {
            context_tokens,
            response_tokens,
            context_window,
        } = &event.payload
        else {
            panic!("expected context usage, got {:?}", event.payload);
        };
        assert_eq!(context_tokens.get(), 1_200);
        assert_eq!(response_tokens.get(), 80);
        assert_eq!(context_window.map(TokenCount::get), Some(200_000));
    }

    #[tokio::test]
    async fn a_scratchpad_reading_keeps_its_item_and_names_its_agent() {
        let (emitter, items, events) = emitter();
        emitter
            .emit_item(StreamItem::ScratchpadUsage {
                agent_id: "log_worker".to_string(),
                tokens_intercepted: 9_000,
                tokens_extracted: 400,
            })
            .await;

        let (items, events) = drain(emitter, items, events);
        assert!(
            matches!(
                items.as_slice(),
                [StreamItem::ScratchpadUsage {
                    agent_id,
                    tokens_intercepted: 9_000,
                    tokens_extracted: 400,
                }] if agent_id == "log_worker"
            ),
            "{items:?}"
        );
        let [event] = events.as_slice() else {
            panic!("expected one channel event, got {events:?}");
        };
        assert_eq!(event.agent, EmittingAgent::orchestrated("log_worker"));
        assert!(matches!(
            &event.payload,
            AgentEventPayload::ScratchpadUsage { tokens_intercepted, tokens_extracted }
                if tokens_intercepted.get() == 9_000 && tokens_extracted.get() == 400
        ));
    }

    #[tokio::test]
    async fn a_status_snapshot_keeps_its_item_and_is_the_coordinators() {
        let (emitter, items, events) = emitter();
        let servers = vec![McpServerStatus {
            server_name: "mezmo".to_string(),
            transport: "http_streamable".to_string(),
            status: "connected".to_string(),
            tools_count: 3,
            reason: None,
        }];
        emitter
            .emit_item(StreamItem::McpStatus(servers.clone()))
            .await;

        let (items, events) = drain(emitter, items, events);
        assert!(
            matches!(items.as_slice(), [StreamItem::McpStatus(sent)] if *sent == servers),
            "{items:?}"
        );
        let [event] = events.as_slice() else {
            panic!("expected one channel event, got {events:?}");
        };
        assert_eq!(event.agent, EmittingAgent::coordinator());
        assert!(matches!(
            &event.payload,
            AgentEventPayload::McpStatus { servers: sent } if *sent == servers
        ));
    }

    /// Content goes on the channel at the moment it is sent, as the
    /// coordinator's. The channel then holds the content, and the events sent
    /// in between, in the same order as the item stream.
    #[tokio::test]
    async fn content_reaches_both_streams_as_the_coordinators() {
        let (emitter, items, events) = emitter();
        emitter
            .emit_item(StreamItem::StreamAssistantItem(
                StreamedAssistantContent::ReasoningDelta {
                    delta: "weighing the plan".to_string(),
                    id: None,
                },
            ))
            .await;
        emitter
            .emit(AgentEvent::new(
                EmittingAgent::coordinator(),
                AgentEventPayload::Synthesizing { iteration: 1 },
            ))
            .await;
        emitter
            .emit_item(StreamItem::StreamAssistantItem(
                StreamedAssistantContent::Text("the answer".to_string()),
            ))
            .await;
        emitter
            .emit_item(StreamItem::Final(
                crate::provider_agent::FinalResponseInfo {
                    content: "the answer".to_string(),
                    usage: rig::completion::Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                        total_tokens: 15,
                    },
                    cache_usage: None,
                },
            ))
            .await;

        let (items, events) = drain(emitter, items, events);
        assert_eq!(items.len(), 4, "{items:?}");
        for event in &events {
            assert_eq!(event.agent, EmittingAgent::coordinator());
        }
        let payloads: Vec<_> = events.into_iter().map(|event| event.payload).collect();
        assert!(
            matches!(
                payloads.as_slice(),
                [
                    AgentEventPayload::Reasoning { content: reasoning, task_id: None },
                    AgentEventPayload::Synthesizing { iteration: 1 },
                    AgentEventPayload::TextDelta { content: text },
                    AgentEventPayload::Completed { content: answer, usage },
                ] if reasoning == "weighing the plan"
                    && text == "the answer"
                    && answer == "the answer"
                    && usage.total_tokens.get() == 15
            ),
            "{payloads:?}"
        );
    }

    /// Some items get no channel event. A complete reasoning block repeats
    /// deltas that were already sent, and tool calls and results reach the
    /// channel as tool events of their own. The emitter does not put any of
    /// them on the channel a second time.
    #[tokio::test]
    async fn items_without_content_of_their_own_stay_off_the_channel() {
        let (emitter, items, events) = emitter();
        emitter
            .emit_item(StreamItem::StreamAssistantItem(
                StreamedAssistantContent::Reasoning("weighing the plan".to_string()),
            ))
            .await;
        emitter.emit_item(StreamItem::FinalMarker).await;

        let (items, events) = drain(emitter, items, events);
        assert_eq!(items.len(), 2, "{items:?}");
        assert!(events.is_empty(), "{events:?}");
    }
}

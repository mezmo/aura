//! The body of a chat completion served from the runtime: the streaming
//! SSE body, and the collected body of a non-streaming completion.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use aura::inactivity::{InactivityDeadline, STALL_MESSAGE};
use aura::journal::JournalItem;
use aura::runtime::{AgentRuntime, RunActivity, Subscription};
use aura_events::RunId;
use aura_events::run::RunCancelReason;
use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use tokio::time::{Interval, MissedTickBehavior, Sleep};

use super::StreamOutcome;
use super::projection::{Projected, Pulse, SseProjector};

/// The time between heartbeat comments on a streaming body in production.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// The SSE comment a heartbeat sends.
const HEARTBEAT_FRAME: &[u8] = b": heartbeat\n\n";

/// The timeout windows a streaming body enforces on its run, and how often
/// it sends a heartbeat.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Timeouts {
    /// How long the run may go before its first chunk.
    pub first_chunk: Option<Duration>,
    /// How long the run may go silent between chunks.
    pub inactivity: Option<Duration>,
    /// How often a heartbeat comment goes out.
    pub heartbeat: Option<Duration>,
}

/// What woke the streaming body.
enum Wake {
    /// The subscription's next item, or its end.
    Item(Option<JournalItem>),
    /// The run showed activity.
    Active,
    /// The first-chunk window elapsed. The value is the window's length.
    FirstChunk(Duration),
    /// The inactivity window elapsed.
    Inactive,
    /// The heartbeat's period elapsed.
    Heartbeat,
}

/// The SSE body of a streaming completion. It reads the projector's run from
/// `subscription`, projects each event onto the wire, and enforces the
/// windows and heartbeat in `timeouts`. The body ends when the run ends.
///
/// The first-chunk window starts when the body is built, because the run's
/// `Started` event has already been read by then. The opening events do not
/// count as the first chunk, since the runtime sends them and not the
/// provider. The inactivity window starts at the first chunk, restarts on
/// every later event of the run, and pauses while a tool runs. The run's
/// [`RunActivity`] counts as a chunk too, and restarts the window the same
/// way, because some of what the provider streams, such as a tool call's
/// argument deltas, never becomes an event. When either window runs out, the
/// body cancels the run and keeps reading until the run's ending, which
/// supplies the last frames. Heartbeats go out the whole time and never count
/// as activity.
///
/// The body drops the subscription before it yields the last frames, so the
/// session's claim is released while a slow client is still reading them.
/// If the subscription ends before the run's ending arrives, the body ends
/// without sending anything more.
pub fn sse_body(
    runtime: Arc<AgentRuntime>,
    subscription: Subscription,
    projector: SseProjector,
    timeouts: Timeouts,
) -> BoxStream<'static, Result<Bytes, std::io::Error>> {
    Box::pin(async_stream::stream! {
        let mut subscription = subscription;
        let mut projector = projector;
        let run = projector.run();
        let mut first_chunk = timeouts
            .first_chunk
            .map(|window| (window, Box::pin(tokio::time::sleep(window))));
        let mut first_chunk_seen = false;
        let mut inactivity =
            InactivityDeadline::new_disarmed(timeouts.inactivity.unwrap_or_default());
        let mut heartbeat = heartbeat(timeouts.heartbeat);
        let mut activity = runtime.activity(&run);
        // True once a window has cancelled the run. After that the body only
        // waits for the run's ending.
        let mut stopping = false;

        let last = loop {
            // Check items first. When an item and a timer or heartbeat are
            // ready at the same time, the item goes out first.
            let wake = tokio::select! {
                biased;
                item = subscription.next() => Wake::Item(item),
                () = active(&mut activity), if !stopping => Wake::Active,
                window = expiry(&mut first_chunk), if !stopping => Wake::FirstChunk(window),
                () = inactivity.expired(), if !stopping => Wake::Inactive,
                () = tick(&mut heartbeat) => Wake::Heartbeat,
            };
            match wake {
                Wake::Item(None) => {
                    tracing::warn!(
                        run_id = %run,
                        "the session's stream ended before the run did"
                    );
                    break Vec::new();
                }
                Wake::Item(Some(item)) => match projector.project(&item) {
                    Projected::Nothing => {}
                    Projected::Frames { frames, pulse } => {
                        if pulse != Pulse::Opening {
                            first_chunk_seen = true;
                            first_chunk = None;
                        }
                        match pulse {
                            Pulse::Opening if !first_chunk_seen => {}
                            Pulse::Opening | Pulse::Activity => inactivity.touch(),
                            Pulse::ToolStarted => inactivity.suspend(),
                            Pulse::ToolFinished => inactivity.resume(),
                        }
                        for frame in frames {
                            yield Ok::<Bytes, std::io::Error>(frame);
                        }
                    }
                    Projected::End { frames, ending } => {
                        tracing::debug!(
                            run_id = %run,
                            ?ending,
                            "the run's stream on the wire ended"
                        );
                        break frames;
                    }
                },
                Wake::Active => {
                    first_chunk_seen = true;
                    first_chunk = None;
                    inactivity.touch();
                }
                Wake::FirstChunk(window) => {
                    tracing::warn!(
                        "First chunk timeout ({window:?}) - no response from LLM provider"
                    );
                    let secs = window.as_secs();
                    stop(
                        &runtime,
                        run,
                        window,
                        format!("first chunk timeout: nothing from the provider in {secs}s"),
                    );
                    stopping = true;
                }
                Wake::Inactive => {
                    let window = inactivity.window();
                    tracing::warn!("Inactivity timeout: no stream progress for {window:?}");
                    stop(
                        &runtime,
                        run,
                        window,
                        format!("inactivity timeout: {STALL_MESSAGE} {}s", window.as_secs()),
                    );
                    stopping = true;
                }
                Wake::Heartbeat => yield Ok(Bytes::from_static(HEARTBEAT_FRAME)),
            }
        };

        drop(subscription);
        for frame in last {
            yield Ok(frame);
        }
    })
}

/// Collects the content and usage of the projector's run for a
/// non-streaming completion. No first-chunk or inactivity window applies.
/// Only the run's own deadline limits how long this waits. The subscription
/// is dropped as soon as the run's ending has been read.
pub async fn collect_body(
    mut subscription: Subscription,
    mut projector: SseProjector,
) -> StreamOutcome {
    let run = projector.run();
    loop {
        match subscription.next().await {
            Some(item) => {
                if let Projected::End { ending, .. } = projector.project(&item) {
                    tracing::debug!(run_id = %run, ?ending, "the run's collected stream ended");
                    break;
                }
            }
            None => {
                tracing::warn!(run_id = %run, "the session's stream ended before the run did");
                break;
            }
        }
    }
    drop(subscription);

    let state = projector.state();
    StreamOutcome {
        content: state.accumulated_content.clone(),
        usage: state.usage_stats.clone(),
    }
}

/// Cancels `run` because `window` ran out. The cancel is a deadline, so the
/// run's span records a timeout error, as the request-bound path records its
/// own timeouts. If the run has already ended, it keeps its own ending, which
/// the body is about to read.
fn stop(runtime: &AgentRuntime, run: RunId, window: Duration, message: String) {
    let reason = RunCancelReason::Deadline { after: window };
    if !runtime.cancel(&run, reason, Some(message)) {
        tracing::debug!(run_id = %run, "the run had already ended when its window elapsed");
    }
}

/// Waits for the run's next activity. With nothing to follow, it never
/// resolves.
async fn active(activity: &mut Option<RunActivity>) {
    match activity {
        Some(activity) => activity.next().await,
        None => std::future::pending().await,
    }
}

/// Waits for the window to run out and returns its length. With no window,
/// it never resolves.
async fn expiry(window: &mut Option<(Duration, Pin<Box<Sleep>>)>) -> Duration {
    match window {
        Some((length, sleep)) => {
            sleep.as_mut().await;
            *length
        }
        None => std::future::pending().await,
    }
}

/// Builds the interval heartbeats go out on. Returns `None` when there is no
/// period or the period is zero, since `tokio::time::interval` panics on a
/// zero period.
fn heartbeat(period: Option<Duration>) -> Option<Interval> {
    period.filter(|period| !period.is_zero()).map(|period| {
        let mut heartbeat = tokio::time::interval(period);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
        heartbeat
    })
}

/// Waits for the heartbeat's next tick. With no heartbeat, it never
/// resolves.
async fn tick(heartbeat: &mut Option<Interval>) {
    match heartbeat {
        Some(heartbeat) => {
            heartbeat.tick().await;
        }
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use aura::journal::JournalItem;
    use aura::run_context::RunContext;
    use aura::runtime::{AgentForRun, AgentRuntime, Attach, RuntimeStores, StartRun, Subscription};
    use aura::streaming::{AgentRun, RunOptions};
    use aura::{Message, StreamError, StreamItem, StreamingAgent, ToolName, UsageState};
    use aura_events::agent::{AgentEvent, AgentEventPayload, ToolOutcome};
    use aura_events::run::{LifecycleEvent, RunCancelReason, SessionEventPayload};
    use aura_events::{EmittingAgent, RunId, SessionId, TokenCount, TokenUsage, ToolCallId};
    use aura_test_utils::sse::parse_sse_stream;
    use futures_util::StreamExt;
    use serde_json::Value;
    use tokio::task::JoinHandle;
    use tokio::time::Instant;

    use super::{Timeouts, collect_body, sse_body};
    use crate::streaming::projection::SseProjector;
    use crate::streaming::types::{StreamConfig, ToolResultMode, TurnContext};

    const AGENT: &str = "scripted";
    const SESSION: &str = "sess_body";
    const HEARTBEAT: &str = ": heartbeat\n\n";
    const WINDOW: Duration = Duration::from_secs(30);

    /// One step of a scripted run.
    enum Step {
        /// Emit the event on the run's channel.
        Emit(Box<AgentEvent>),
        /// Yield the item on the run's stream.
        Item(Box<StreamItem>),
        /// Let this much time pass.
        Wait(Duration),
        /// End the run's stream with this error.
        Fail(&'static str),
        /// Wait to be stopped, then end the stream as a stopped agent does.
        Hang,
    }

    fn emit(payload: AgentEventPayload) -> Step {
        Step::Emit(Box::new(AgentEvent::single_agent(payload)))
    }

    fn text(content: &str) -> Step {
        emit(AgentEventPayload::TextDelta {
            content: content.to_string(),
        })
    }

    /// A piece of a tool call's arguments, which never becomes an event.
    fn argument_delta() -> Step {
        Step::Item(Box::new(StreamItem::StreamAssistantItem(
            aura::StreamedAssistantContent::ToolCallDelta {
                id: "t1".to_string(),
                name: None,
                delta: Some("{\"path\":".to_string()),
            },
        )))
    }

    fn wait(secs: u64) -> Step {
        Step::Wait(Duration::from_secs(secs))
    }

    fn tool_requested() -> Step {
        emit(AgentEventPayload::ToolRequested {
            tool_call_id: ToolCallId::new("t1"),
            tool_name: ToolName::new("slow_tool"),
            arguments: serde_json::json!({}),
            unparsed_arguments: None,
        })
    }

    fn tool_complete() -> Step {
        emit(AgentEventPayload::ToolComplete {
            tool_call_id: ToolCallId::new("t1"),
            tool_name: ToolName::new("slow_tool"),
            duration_ms: 100_000,
            outcome: ToolOutcome::Success {
                result: "ok".to_string(),
                format: aura_events::agent::ResultFormat::Text,
            },
            task_id: None,
        })
    }

    /// An agent whose run emits its script's events on the run's channel and
    /// ends its stream as the script says.
    struct ScriptedAgent {
        run_id: RunId,
        steps: Mutex<Option<Vec<Step>>>,
    }

    #[async_trait]
    impl StreamingAgent for ScriptedAgent {
        fn get_provider_info(&self) -> (&str, &str) {
            ("test", "fake")
        }

        fn run_id(&self) -> RunId {
            self.run_id
        }

        async fn stream(
            &self,
            _query: &str,
            _chat_history: Vec<Message>,
            options: RunOptions,
            _request_id: &str,
        ) -> AgentRun {
            let cancel = options.into_parts().1.unwrap_or_default();
            let (run, events) = RunContext::channel_on(self.run_id, cancel.clone());
            let steps = self
                .steps
                .lock()
                .expect("steps lock")
                .take()
                .unwrap_or_default();
            let stopped = cancel.clone();
            let items = async_stream::stream! {
                for step in steps {
                    match step {
                        Step::Emit(event) => {
                            run.emit(*event).await;
                        }
                        Step::Item(item) => yield Ok(*item),
                        Step::Wait(time) => tokio::time::sleep(time).await,
                        Step::Fail(error) => {
                            yield Err::<StreamItem, StreamError>(error.into());
                            return;
                        }
                        Step::Hang => {
                            stopped.cancelled().await;
                            yield Err("the run was stopped".into());
                            return;
                        }
                    }
                }
            };
            AgentRun::new(Box::pin(items), cancel, UsageState::new()).observed_by(events)
        }

        async fn cancel_and_close_mcp(&self, _request_id: &str, _reason: &str) -> usize {
            0
        }
    }

    /// A runtime serving one agent. The agent's first run follows `steps`.
    fn runtime_over(steps: Vec<Step>, stores: RuntimeStores) -> Arc<AgentRuntime> {
        let config = aura_config::Config {
            agent: aura_config::AgentConfig {
                name: AGENT.to_owned(),
                ..Default::default()
            },
            ..Default::default()
        };
        let script = Mutex::new(Some(steps));
        Arc::new(AgentRuntime::over_agents(
            Arc::new(vec![config]),
            Some(AGENT.to_owned()),
            stores,
            move |_config, run_id| {
                Ok(AgentForRun {
                    agent: Arc::new(ScriptedAgent {
                        run_id,
                        steps: Mutex::new(script.lock().expect("script lock").take()),
                    }),
                    attribution: EmittingAgent::single_agent(),
                })
            },
        ))
    }

    fn session() -> SessionId {
        SessionId::new(SESSION).expect("a session id")
    }

    /// A started run. `subscription` is the one a body reads, already past
    /// the run's `Started` event, as `start_run` leaves it. `ending` resolves
    /// to the run's terminal event, read by a second, collecting observer.
    struct Running {
        run: RunId,
        subscription: Subscription,
        ending: JoinHandle<LifecycleEvent>,
    }

    /// Starts a run on the test's session, with the body's observer
    /// attached as `attach`.
    async fn start(runtime: &AgentRuntime, attach: Attach, timeout: Option<Duration>) -> Running {
        let witness = runtime
            .attach(&session(), Attach::collecting())
            .await
            .expect("the witness attaches");
        let mut subscription = runtime
            .attach(&session(), attach)
            .await
            .expect("the body's observer attaches");
        let run = runtime
            .start(StartRun {
                session: Some(session()),
                timeout,
                ..StartRun::new("q")
            })
            .await
            .expect("the run starts");
        past_started(&mut subscription, run).await;
        Running {
            run,
            subscription,
            ending: tokio::spawn(ending_of(witness, run)),
        }
    }

    async fn past_started(subscription: &mut Subscription, run: RunId) {
        while let Some(item) = subscription.next().await {
            if let JournalItem::Event(event) = &item
                && event.run_id == Some(run)
                && matches!(
                    event.payload,
                    SessionEventPayload::Lifecycle(LifecycleEvent::Started { .. })
                )
            {
                return;
            }
        }
        panic!("the session's stream ended before the run started");
    }

    async fn ending_of(mut witness: Subscription, run: RunId) -> LifecycleEvent {
        while let Some(item) = witness.next().await {
            if let JournalItem::Event(event) = item
                && event.run_id == Some(run)
                && let SessionEventPayload::Lifecycle(lifecycle) = &event.payload
                && lifecycle.ends_run()
            {
                return lifecycle.clone();
            }
        }
        panic!("the session's stream ended before the run did");
    }

    fn projector(run: RunId, custom_events: bool) -> SseProjector {
        SseProjector::new(
            StreamConfig::new(custom_events, false, ToolResultMode::Aura, 0),
            TurnContext::new(
                "chatcmpl-test".to_string(),
                "test/fake".to_string(),
                1_700_000_000,
                None,
                SESSION,
            ),
            run,
        )
    }

    /// What a client read from a body, how long the body took in virtual
    /// time, and how the run ended.
    struct Served {
        body: String,
        elapsed: u64,
        ending: LifecycleEvent,
    }

    async fn read(
        runtime: &Arc<AgentRuntime>,
        running: Running,
        custom_events: bool,
        timeouts: Timeouts,
    ) -> Served {
        let Running {
            run,
            subscription,
            ending,
        } = running;
        let clock = Instant::now();
        let body = sse_body(
            Arc::clone(runtime),
            subscription,
            projector(run, custom_events),
            timeouts,
        )
        .map(|chunk| String::from_utf8(chunk.expect("a body chunk").to_vec()).expect("UTF-8"))
        .collect::<String>()
        .await;
        Served {
            body,
            elapsed: clock.elapsed().as_secs(),
            ending: ending.await.expect("the witness reads the ending"),
        }
    }

    async fn serve_with(steps: Vec<Step>, custom_events: bool, timeouts: Timeouts) -> Served {
        let runtime = runtime_over(steps, RuntimeStores::in_memory());
        let running = start(&runtime, Attach::claiming().present(), None).await;
        read(&runtime, running, custom_events, timeouts).await
    }

    async fn serve(steps: Vec<Step>, timeouts: Timeouts) -> Served {
        serve_with(steps, true, timeouts).await
    }

    fn inactivity(window: Duration) -> Timeouts {
        Timeouts {
            inactivity: Some(window),
            ..Timeouts::default()
        }
    }

    /// Asserts the body closed the way it does for a run that reached its
    /// end: the final chunk, then `[DONE]`.
    fn assert_closed(body: &str) {
        let (events, done) = parse_sse_stream(body);
        assert!(done, "the body must end with [DONE]: {body}");
        assert!(body.ends_with("data: [DONE]\n\n"), "got: {body}");
        assert!(
            events.iter().any(|event| {
                serde_json::from_str::<Value>(&event.data).is_ok_and(|json| {
                    json["object"] == "chat.completion.chunk"
                        && json["choices"][0]["finish_reason"].is_string()
                })
            }),
            "the body must carry the final chunk: {body}"
        );
    }

    fn content(body: &str) -> String {
        let (events, _) = parse_sse_stream(body);
        events
            .iter()
            .filter(|event| event.event_type.is_none())
            .filter_map(|event| serde_json::from_str::<Value>(&event.data).ok())
            .filter_map(|json| {
                json["choices"][0]["delta"]["content"]
                    .as_str()
                    .map(str::to_string)
            })
            .collect()
    }

    fn assert_cancelled_by_window(ending: &LifecycleEvent, window: Duration, message_start: &str) {
        let LifecycleEvent::Cancelled {
            reason: RunCancelReason::Deadline { after },
            message: Some(message),
            ..
        } = ending
        else {
            panic!("expected the body to cancel the run, got {ending:?}");
        };
        assert_eq!(*after, window, "the cancel names the window that ran out");
        assert!(message.starts_with(message_start), "got: {message}");
    }

    fn assert_finished(ending: &LifecycleEvent) {
        assert!(
            matches!(ending, LifecycleEvent::Finished { .. }),
            "expected the run to finish, got {ending:?}"
        );
    }

    /// The request-bound `inactivity` suite's runs, plus tests of the
    /// first-chunk window, run against the body's timers.
    mod windows {
        use super::*;

        #[tokio::test(start_paused = true)]
        async fn mid_stream_hang_fails_at_window() {
            let served = serve(vec![text("hi"), Step::Hang], inactivity(WINDOW)).await;

            assert_eq!(served.elapsed, 30);
            assert_cancelled_by_window(
                &served.ending,
                WINDOW,
                "inactivity timeout: no stream progress for 30s",
            );
            assert_eq!(content(&served.body), "hi");
            assert_closed(&served.body);
        }

        /// Before the first chunk, only the first-chunk window runs. The
        /// opening events neither arm the inactivity window nor close the
        /// first-chunk window.
        #[tokio::test(start_paused = true)]
        async fn disarmed_before_first_chunk() {
            let served = serve(
                vec![Step::Hang],
                Timeouts {
                    first_chunk: Some(Duration::from_secs(90)),
                    inactivity: Some(WINDOW),
                    heartbeat: None,
                },
            )
            .await;

            assert_eq!(served.elapsed, 90);
            assert_cancelled_by_window(
                &served.ending,
                Duration::from_secs(90),
                "first chunk timeout: ",
            );
            assert!(served.body.contains("event: aura.session_info"));
            assert_closed(&served.body);
        }

        #[tokio::test(start_paused = true)]
        async fn the_first_chunk_closes_its_window() {
            let served = serve(
                vec![wait(60), text("late"), wait(60), text(" and later")],
                Timeouts {
                    first_chunk: Some(Duration::from_secs(90)),
                    ..Timeouts::default()
                },
            )
            .await;

            assert_eq!(served.elapsed, 120);
            assert_finished(&served.ending);
            assert_eq!(content(&served.body), "late and later");
        }

        /// A tool call's argument deltas are the provider's first chunk,
        /// although they never become events.
        #[tokio::test(start_paused = true)]
        async fn a_streamed_tool_call_closes_the_first_chunk_window() {
            let served = serve(
                vec![wait(60), argument_delta(), wait(60), text("done")],
                Timeouts {
                    first_chunk: Some(Duration::from_secs(90)),
                    ..Timeouts::default()
                },
            )
            .await;

            assert_eq!(served.elapsed, 120);
            assert_finished(&served.ending);
        }

        /// An orchestrated run puts its MCP status reading on its stream
        /// before its provider answers, so that reading is not the first
        /// chunk.
        #[tokio::test(start_paused = true)]
        async fn an_mcp_status_item_leaves_the_first_chunk_window_open() {
            let runtime = runtime_over(
                vec![
                    Step::Item(Box::new(StreamItem::McpStatus(Vec::new()))),
                    Step::Hang,
                ],
                RuntimeStores::in_memory(),
            );
            // The run's own deadline ends the test if the window never runs
            // out.
            let deadline = Some(Duration::from_secs(300));
            let running = start(&runtime, Attach::claiming().present(), deadline).await;
            let served = read(
                &runtime,
                running,
                true,
                Timeouts {
                    first_chunk: Some(Duration::from_secs(90)),
                    ..Timeouts::default()
                },
            )
            .await;

            assert_eq!(served.elapsed, 90);
            assert_cancelled_by_window(
                &served.ending,
                Duration::from_secs(90),
                "first chunk timeout: ",
            );
        }

        /// Activity the runtime read before the body began to follow the run
        /// still closes the first-chunk window.
        #[tokio::test(start_paused = true)]
        async fn activity_before_the_body_closes_the_first_chunk_window() {
            let runtime = runtime_over(
                vec![argument_delta(), Step::Hang],
                RuntimeStores::in_memory(),
            );
            let deadline = Some(Duration::from_secs(300));
            let running = start(&runtime, Attach::claiming().present(), deadline).await;
            tokio::time::sleep(Duration::from_secs(1)).await;
            let served = read(
                &runtime,
                running,
                true,
                Timeouts {
                    first_chunk: Some(Duration::from_secs(90)),
                    ..Timeouts::default()
                },
            )
            .await;

            assert!(
                matches!(
                    served.ending,
                    LifecycleEvent::Cancelled {
                        reason: RunCancelReason::Deadline { after },
                        message: None,
                        ..
                    } if after == Duration::from_secs(300)
                ),
                "only the run's own deadline ends it: {:?}",
                served.ending
            );
        }

        /// A model writing a long tool call streams only argument deltas
        /// until the call is complete. They keep the run alive, as events do.
        #[tokio::test(start_paused = true)]
        async fn argument_deltas_keep_the_body_alive_past_the_window() {
            // 3 × 20s of deltas after the first text, then a full 30s of
            // silence.
            let served = serve(
                vec![
                    text("Writing the file."),
                    wait(20),
                    argument_delta(),
                    wait(20),
                    argument_delta(),
                    wait(20),
                    argument_delta(),
                    Step::Hang,
                ],
                inactivity(WINDOW),
            )
            .await;

            assert_eq!(served.elapsed, 90);
            assert_cancelled_by_window(&served.ending, WINDOW, "inactivity timeout: ");
        }

        #[tokio::test(start_paused = true)]
        async fn flowing_events_keep_the_body_alive_past_the_window() {
            // 3 × 20s of flow, then a full 30s of silence.
            let served = serve(
                vec![
                    wait(20),
                    text("tick"),
                    wait(20),
                    text("tick"),
                    wait(20),
                    text("tick"),
                    Step::Hang,
                ],
                inactivity(WINDOW),
            )
            .await;

            assert_eq!(served.elapsed, 90);
            assert_cancelled_by_window(&served.ending, WINDOW, "inactivity timeout: ");
        }

        #[tokio::test(start_paused = true)]
        async fn tool_execution_is_exempt() {
            // The request, then 100s of silence while the tool runs, then the
            // result and a clean end. That is far past the 30s window, but
            // the window is paused the whole time.
            let served = serve(
                vec![tool_requested(), wait(100), tool_complete()],
                inactivity(WINDOW),
            )
            .await;

            assert_eq!(served.elapsed, 100);
            assert_finished(&served.ending);
            assert_closed(&served.body);
        }

        #[tokio::test(start_paused = true)]
        async fn silence_after_tool_result_still_stalls() {
            // The result restarts the countdown. Silence after a tool has
            // finished is a stall, not more tool work.
            let served = serve(
                vec![tool_requested(), wait(100), tool_complete(), Step::Hang],
                inactivity(WINDOW),
            )
            .await;

            assert_eq!(served.elapsed, 130);
            assert_cancelled_by_window(&served.ending, WINDOW, "inactivity timeout: ");
        }

        #[tokio::test(start_paused = true)]
        async fn normal_completion_beats_a_short_window() {
            let served = serve(
                vec![text("hi"), text("there")],
                inactivity(Duration::from_secs(5)),
            )
            .await;

            assert_finished(&served.ending);
            assert_eq!(content(&served.body), "hithere");
        }

        /// MCP progress every 20s keeps a 30s window alive until the script
        /// stops at 60s, and the run stalls out at 90s. Turning custom events
        /// off hides the progress from the client but not from the window.
        #[tokio::test(start_paused = true)]
        async fn progress_notifications_carry_liveness() {
            for custom_events in [true, false] {
                let mut steps = vec![text("hi")];
                for n in 0..3i64 {
                    steps.push(wait(20));
                    steps.push(emit(AgentEventPayload::ToolProgress {
                        progress_token: aura_events::ProgressToken(
                            aura_events::NumberOrString::Number(n),
                        ),
                        progress: aura_events::Progress::ratio(n as f64, 3.0),
                        message: Some("working".into()),
                    }));
                }
                steps.push(Step::Hang);
                let served = serve_with(steps, custom_events, inactivity(WINDOW)).await;

                assert_eq!(served.elapsed, 90, "custom_events={custom_events}");
                assert_cancelled_by_window(&served.ending, WINDOW, "inactivity timeout: ");
            }
        }

        #[tokio::test(start_paused = true)]
        async fn approval_events_carry_liveness() {
            let approval = emit(AgentEventPayload::ApprovalPending(
                aura_events::ApprovalPending {
                    decision_id: "d1".into(),
                    tool_name: "dangerous_apply".into(),
                    tool_namespace: None,
                    arguments: serde_json::json!({}),
                    origin: aura_events::ApprovalOriginWire::ConfigGate {
                        matched_pattern: "dangerous_*".into(),
                        agent_name: "test-agent".into(),
                    },
                    scope: aura_events::AgentScopeWire::Single { session_id: None },
                    expires_at: "2026-01-01T00:00:00Z".into(),
                },
            ));
            let served = serve(
                vec![text("hi"), wait(25), approval, Step::Hang],
                inactivity(WINDOW),
            )
            .await;

            assert_eq!(served.elapsed, 55);
            assert_cancelled_by_window(&served.ending, WINDOW, "inactivity timeout: ");
            assert!(served.body.contains("event: aura.approval_pending"));
        }
    }

    mod heartbeat {
        use super::*;

        fn every(period: Duration) -> Timeouts {
            Timeouts {
                heartbeat: Some(period),
                ..Timeouts::default()
            }
        }

        #[tokio::test(start_paused = true)]
        async fn a_heartbeat_goes_out_on_its_period() {
            let served = serve(vec![wait(12), text("hi")], every(Duration::from_secs(5))).await;

            // At 0s, 5s and 10s.
            assert_eq!(
                served.body.matches(HEARTBEAT).count(),
                3,
                "got: {}",
                served.body
            );
            assert_finished(&served.ending);
            assert_closed(&served.body);
        }

        #[tokio::test(start_paused = true)]
        async fn a_zero_heartbeat_is_no_heartbeat() {
            let served = serve(vec![wait(12), text("hi")], every(Duration::ZERO)).await;

            assert!(!served.body.contains(HEARTBEAT), "got: {}", served.body);
            assert_finished(&served.ending);
            assert_closed(&served.body);
        }

        #[tokio::test(start_paused = true)]
        async fn heartbeats_do_not_feed_the_deadline() {
            let served = serve(
                vec![text("hi"), Step::Hang],
                Timeouts {
                    inactivity: Some(WINDOW),
                    heartbeat: Some(Duration::from_secs(5)),
                    ..Timeouts::default()
                },
            )
            .await;

            assert_eq!(served.elapsed, 30);
            assert!(served.body.contains(HEARTBEAT));
            assert_cancelled_by_window(&served.ending, WINDOW, "inactivity timeout: ");
        }
    }

    /// How the body ends for each way the run can end, and for a gap.
    mod endings {
        use super::*;

        #[tokio::test(start_paused = true)]
        async fn a_finished_run_ends_with_the_final_chunk() {
            let served = serve(vec![text("Hello.")], Timeouts::default()).await;

            assert_finished(&served.ending);
            assert_eq!(content(&served.body), "Hello.");
            assert_closed(&served.body);
        }

        #[tokio::test(start_paused = true)]
        async fn a_failed_run_ends_with_its_error_text() {
            let served = serve(
                vec![text("Partial "), Step::Fail("upstream exploded")],
                Timeouts::default(),
            )
            .await;

            assert!(
                matches!(&served.ending, LifecycleEvent::Failed { error, .. } if error == "upstream exploded"),
                "got {:?}",
                served.ending
            );
            assert!(
                content(&served.body).starts_with(
                    "Partial The upstream model provider (test/fake) returned an error"
                ),
                "got: {}",
                served.body
            );
            assert_closed(&served.body);
        }

        #[tokio::test(start_paused = true)]
        async fn a_parked_run_ends_with_the_final_chunk() {
            let parked = Step::Emit(Box::new(AgentEvent::new(
                EmittingAgent::coordinator(),
                AgentEventPayload::RunParked {
                    run_id: "run:parked".to_string(),
                    decision_ids: Vec::new(),
                    expires_at: "2026-01-01T00:00:00Z".to_string(),
                    iteration: 1,
                },
            )));
            let served = serve(vec![parked], Timeouts::default()).await;

            assert!(
                matches!(served.ending, LifecycleEvent::Parked { .. }),
                "got {:?}",
                served.ending
            );
            assert_closed(&served.body);
        }

        #[tokio::test(start_paused = true)]
        async fn a_deadline_ends_with_the_final_chunk() {
            let runtime = runtime_over(vec![text("hi"), Step::Hang], RuntimeStores::in_memory());
            let running = start(&runtime, Attach::claiming().present(), Some(WINDOW)).await;
            let served = read(&runtime, running, true, Timeouts::default()).await;

            assert_eq!(served.elapsed, 30);
            assert!(
                matches!(
                    served.ending,
                    LifecycleEvent::Cancelled {
                        reason: RunCancelReason::Deadline { after },
                        ..
                    } if after == WINDOW
                ),
                "got {:?}",
                served.ending
            );
            assert_closed(&served.body);
        }

        #[tokio::test(start_paused = true)]
        async fn an_external_cancel_ends_with_the_final_chunk() {
            let runtime = runtime_over(vec![text("hi"), Step::Hang], RuntimeStores::in_memory());
            let running = start(&runtime, Attach::claiming().present(), None).await;
            let canceller = Arc::clone(&runtime);
            let run = running.run;
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(10)).await;
                canceller.cancel(&run, RunCancelReason::External, Some("enough".to_string()));
            });
            let served = read(&runtime, running, true, Timeouts::default()).await;

            assert_eq!(served.elapsed, 10);
            assert!(
                matches!(
                    &served.ending,
                    LifecycleEvent::Cancelled {
                        reason: RunCancelReason::External,
                        message: Some(message),
                        ..
                    } if message == "enough"
                ),
                "got {:?}",
                served.ending
            );
            assert_closed(&served.body);
        }

        #[tokio::test(start_paused = true)]
        async fn a_shutdown_ends_with_the_final_chunk() {
            let runtime = runtime_over(vec![text("hi"), Step::Hang], RuntimeStores::in_memory());
            let running = start(&runtime, Attach::claiming().present(), None).await;
            let draining = Arc::clone(&runtime);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(10)).await;
                draining.drain(Duration::from_secs(5)).await;
            });
            let served = read(&runtime, running, true, Timeouts::default()).await;

            assert!(
                matches!(
                    served.ending,
                    LifecycleEvent::Cancelled {
                        reason: RunCancelReason::Shutdown,
                        ..
                    }
                ),
                "got {:?}",
                served.ending
            );
            assert_closed(&served.body);
        }

        /// The run ends because its claimant left. The body here reads as a
        /// collecting observer, and it sends nothing more: no final chunk and
        /// no `[DONE]`.
        #[tokio::test(start_paused = true)]
        async fn an_unclaimed_run_ends_the_body_with_nothing_more() {
            let runtime = runtime_over(vec![text("hi"), Step::Hang], RuntimeStores::in_memory());
            let claimant = runtime
                .attach(&session(), Attach::claiming().present())
                .await
                .expect("the claimant attaches");
            let running = start(&runtime, Attach::collecting(), None).await;
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(10)).await;
                drop(claimant);
            });
            let served = read(&runtime, running, true, Timeouts::default()).await;

            assert!(
                matches!(
                    served.ending,
                    LifecycleEvent::Cancelled {
                        reason: RunCancelReason::Unclaimed,
                        ..
                    }
                ),
                "got {:?}",
                served.ending
            );
            assert_eq!(content(&served.body), "hi");
            let (_, done) = parse_sse_stream(&served.body);
            assert!(!done, "nobody is left to read [DONE]: {}", served.body);
            assert!(
                !served.body.contains("finish_reason"),
                "got: {}",
                served.body
            );
        }

        /// A body that falls behind by more events than both the session's
        /// store and its live buffer hold gets a gap, and tells the client.
        #[tokio::test(start_paused = true)]
        async fn a_gap_ends_with_the_fell_behind_text() {
            let stores = RuntimeStores {
                runs: Arc::new(aura::session_store::InMemoryRunStore::new()),
                journals: Arc::new(aura::session_store::InMemoryJournalStore::with_bounds(
                    8, 16,
                )),
                skills: Arc::new(aura::session_store::InMemorySkillInvocationStore::new()),
            };
            // More events than the live buffer holds (`LIVE_CAPACITY`).
            let flood = aura::journal::LIVE_CAPACITY + 100;
            let mut steps = vec![wait(1)];
            steps.extend((0..flood).map(|_| text("x")));
            let runtime = runtime_over(steps, stores);
            let Running {
                run,
                subscription,
                ending,
            } = start(&runtime, Attach::claiming().present(), None).await;

            // The run floods and ends while nothing reads the body's
            // subscription.
            let ending = ending.await.expect("the witness reads the ending");
            assert_finished(&ending);
            let body = sse_body(
                Arc::clone(&runtime),
                subscription,
                projector(run, true),
                Timeouts::default(),
            )
            .map(|chunk| String::from_utf8(chunk.expect("a body chunk").to_vec()).expect("UTF-8"))
            .collect::<String>()
            .await;

            assert!(
                content(&body).starts_with("This response fell behind the agent"),
                "got: {body}"
            );
            assert_closed(&body);
        }

        /// The session's claim is released before the last frames go out,
        /// so a slow client reading them does not hold up the session's next
        /// run.
        #[tokio::test(start_paused = true)]
        async fn the_claim_is_let_go_before_the_last_frames() {
            let runtime = runtime_over(vec![text("hi")], RuntimeStores::in_memory());
            let running = start(&runtime, Attach::claiming().present(), None).await;
            let claimants = || {
                runtime
                    .observer_counts(&session())
                    .expect("the session is held")
                    .claiming
            };
            let mut body = sse_body(
                Arc::clone(&runtime),
                running.subscription,
                projector(running.run, false),
                Timeouts::default(),
            );

            let first = body.next().await.expect("a chunk").expect("a body chunk");
            assert!(
                std::str::from_utf8(&first)
                    .expect("UTF-8")
                    .contains("\"hi\"")
            );
            assert_eq!(claimants(), 1, "the claim holds while the run goes on");

            let last = body.next().await.expect("a chunk").expect("a body chunk");
            assert!(
                std::str::from_utf8(&last)
                    .expect("UTF-8")
                    .contains("finish_reason")
            );
            assert_eq!(claimants(), 0, "the claim is free before the final chunk");
        }
    }

    /// The non-streaming body.
    mod collect {
        use super::*;

        #[tokio::test(start_paused = true)]
        async fn collect_body_returns_the_runs_answer_and_usage() {
            let completed = emit(AgentEventPayload::Completed {
                content: "Hello.".to_string(),
                usage: TokenUsage {
                    prompt_tokens: TokenCount::new(12),
                    completion_tokens: TokenCount::new(7),
                    total_tokens: TokenCount::new(19),
                },
            });
            let runtime = runtime_over(
                vec![text("Hel"), text("lo"), completed],
                RuntimeStores::in_memory(),
            );
            let running = start(&runtime, Attach::claiming().present(), None).await;

            let outcome = collect_body(running.subscription, projector(running.run, false)).await;

            assert_eq!(outcome.content, "Hello.");
            let usage = outcome.usage.expect("the run's usage");
            assert_eq!(
                (
                    usage.prompt_tokens,
                    usage.completion_tokens,
                    usage.total_tokens
                ),
                (12, 7, 19)
            );
            assert_eq!(
                runtime
                    .observer_counts(&session())
                    .expect("the session is held")
                    .claiming,
                0,
                "the claim is let go once the ending is read"
            );
        }

        #[tokio::test(start_paused = true)]
        async fn a_failed_runs_collected_content_is_its_error_text() {
            let runtime = runtime_over(
                vec![Step::Fail("upstream exploded")],
                RuntimeStores::in_memory(),
            );
            let running = start(&runtime, Attach::claiming().present(), None).await;

            let outcome = collect_body(running.subscription, projector(running.run, false)).await;

            assert!(
                outcome
                    .content
                    .starts_with("The upstream model provider (test/fake) returned an error"),
                "got: {}",
                outcome.content
            );
            assert!(outcome.usage.is_none());
        }
    }
}

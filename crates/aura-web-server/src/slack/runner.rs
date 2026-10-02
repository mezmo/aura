//! Turn an accepted Slack message into an agent run and post the answer.

use std::sync::Arc;
use std::time::Duration;

use aura::{Message, RigBuilder, StreamItem};
use futures_util::StreamExt;
use tokio::sync::{Semaphore, mpsc};
use tracing::{Instrument, error, info, warn};

use super::api::{BotIdentity, SlackApi, SlackApiError, SlackMessage};
use super::events::{EventCallback, HUMAN_SUBTYPES, Inbound, SeenMessages, accept, strip_mentions};
use super::socket_mode;
use crate::types::{ActiveRequestGuard, AppState};

/// Capacity of the socket-to-dispatcher channel.
const EVENT_QUEUE: usize = 64;
/// `(channel, ts)` pairs remembered for duplicate suppression.
const SEEN_CAPACITY: usize = 1024;
/// Reaction added to a message the bot is working on.
const ACK_REACTION: &str = "eyes";
/// Newest thread messages carried into the agent's history.
const HISTORY_LIMIT: usize = 100;
const EMPTY_REPLY: &str = "_(the agent returned no text)_";
const FAILED_REPLY: &str = "Sorry, I hit an error answering that. The server log has the details.";

#[derive(Debug, thiserror::Error)]
pub enum SlackStartError {
    #[error("slack auth.test failed: {0}")]
    Auth(SlackApiError),
    #[error("no agent configuration found for `{0}`")]
    UnknownAgent(String),
    #[error("more than one configuration is loaded; set --slack-agent or --default-agent")]
    AmbiguousAgent,
    #[error("agent `{0}` has a [hitl] block; there is no Slack surface to approve tool calls on")]
    HitlUnsupported(String),
}

struct SlackIngress {
    api: SlackApi,
    identity: BotIdentity,
    config: aura_config::Config,
    state: Arc<AppState>,
    slots: Semaphore,
}

/// Verify the tokens, pick the agent, and spawn the Socket Mode loop plus
/// the dispatcher that answers its events. Returns once both are running.
pub async fn start(
    state: Arc<AppState>,
    api: SlackApi,
    agent: Option<&str>,
    concurrency: usize,
) -> Result<(), SlackStartError> {
    install_crypto_provider();
    let identity = api.auth_test().await.map_err(SlackStartError::Auth)?;
    let config = state.resolve_config(agent).ok_or_else(|| match agent {
        Some(name) => SlackStartError::UnknownAgent(name.to_owned()),
        None => match state.default_agent.as_deref() {
            Some(name) => SlackStartError::UnknownAgent(name.to_owned()),
            None => SlackStartError::AmbiguousAgent,
        },
    })?;
    if config.hitl.is_some() {
        return Err(SlackStartError::HitlUnsupported(config.agent.name));
    }
    info!(
        bot_user = identity.user,
        team = identity.team,
        agent = config.agent.name,
        "slack ingress enabled"
    );

    let ingress = Arc::new(SlackIngress {
        api: api.clone(),
        identity,
        config,
        state: Arc::clone(&state),
        slots: Semaphore::new(concurrency),
    });
    let (tx, rx) = mpsc::channel(EVENT_QUEUE);
    tokio::spawn(socket_mode::run(api, tx, state.shutdown_token.clone()));
    tokio::spawn(dispatch(ingress, rx));
    Ok(())
}

/// Pick `ring` as the process-wide rustls provider. The WebSocket client
/// builds its TLS config from the process default, and the dependency tree
/// carries both `ring` (reqwest) and `aws-lc-rs` (A2A server, AWS SDK), so
/// without an explicit choice rustls panics on the first `wss://` dial.
/// Installing twice is not an error: a provider already in place wins.
fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Hand each accepted, first-seen message to its own task, until shutdown
/// begins. A task counts as an active request from the moment it is
/// spawned, waiting for a slot included, so the drain covers it; its handle
/// is tracked so a straggler can be aborted.
async fn dispatch(ingress: Arc<SlackIngress>, mut events: mpsc::Receiver<EventCallback>) {
    let mut seen = SeenMessages::new(SEEN_CAPACITY);
    let shutdown = ingress.state.shutdown_token.clone();
    loop {
        let callback = tokio::select! {
            biased;
            () = shutdown.cancelled() => return,
            next = events.recv() => match next {
                Some(callback) => callback,
                None => return,
            },
        };
        let Some(inbound) = accept(callback.event, &ingress.identity.user_id) else {
            continue;
        };
        if !seen.insert(&inbound.channel, &inbound.ts) {
            continue;
        }
        let ingress = Arc::clone(&ingress);
        let tracker = Arc::clone(&ingress.state.active_requests);
        let active = ActiveRequestGuard::new(Arc::clone(&tracker));
        let task = async move {
            let _active = active;
            let slot = tokio::select! {
                biased;
                () = ingress.state.shutdown_token.cancelled() => return,
                slot = ingress.slots.acquire() => slot,
            };
            if slot.is_ok() {
                ingress.answer(inbound).await;
            }
        };
        tracker.track_task(tokio::spawn(
            task.instrument(tracing::info_span!(parent: None, "agent.stream")),
        ));
    }
}

impl SlackIngress {
    async fn answer(&self, inbound: Inbound) {
        let request_id = format!("slack_{}_{}", inbound.channel, inbound.ts);
        if let Err(e) = self
            .api
            .add_reaction(&inbound.channel, &inbound.ts, ACK_REACTION)
            .await
        {
            warn!(request_id, error = %e, "could not react to slack message");
        }

        let reply = match self.run_agent(&inbound, &request_id).await {
            Ok(text) if text.trim().is_empty() => EMPTY_REPLY.to_owned(),
            Ok(text) => text,
            // The server is going down; a reply would race the shutdown and
            // tell the user something broke when nothing did.
            Err(RunError::Cancelled) => {
                warn!(
                    request_id,
                    "slack-triggered agent run cancelled by shutdown"
                );
                return;
            }
            Err(e) => {
                error!(request_id, error = %e, "slack-triggered agent run failed");
                FAILED_REPLY.to_owned()
            }
        };
        if let Err(e) = self
            .api
            .post_message(&inbound.channel, inbound.reply_thread(), &reply)
            .await
        {
            error!(request_id, error = %e, "could not post slack reply");
        }
    }

    /// Run the agent over the thread so far and return its final text.
    async fn run_agent(&self, inbound: &Inbound, request_id: &str) -> Result<String, RunError> {
        // A thread is its own conversation wherever it is; a top-level DM
        // message continues the DM; a top-level channel mention starts fresh.
        let earlier = match (&inbound.thread_ts, inbound.is_dm) {
            (Some(thread_ts), _) => {
                self.api
                    .conversations_replies(&inbound.channel, thread_ts)
                    .await?
            }
            (None, true) => {
                self.api
                    .conversations_history(&inbound.channel, &inbound.ts, HISTORY_LIMIT)
                    .await?
            }
            (None, false) => Vec::new(),
        };
        let history = thread_history(&earlier, &self.identity, &inbound.ts);
        let session_id = match inbound.reply_thread() {
            Some(thread) => format!("slack:{}:{thread}", inbound.channel),
            None => format!("slack:{}", inbound.channel),
        };
        let agent = RigBuilder::new(self.config.clone(), self.state.pending_approvals.clone())
            .with_hitl_hmac(self.state.hitl_webhook_hmac.clone())
            .build_streaming_agent_with_headers(
                None,
                Some(session_id),
                None,
                Some(request_id.to_owned()),
            )
            .await
            .map_err(|e| RunError::Build(e.to_string()))?;

        let timeout = (self.state.streaming_timeout_secs > 0)
            .then(|| Duration::from_secs(self.state.streaming_timeout_secs));
        let run = agent
            .stream(
                &inbound.text,
                history,
                aura::streaming::RunOptions::bounded(timeout)
                    .cancelled_by(&self.state.stream_shutdown_token),
                request_id,
            )
            .await;

        // The run's own deadline only fires between hook points, so the
        // consumer bounds the wall clock too and, on timeout or shutdown,
        // sends `notifications/cancelled` to every MCP server in flight.
        let outcome = tokio::select! {
            biased;
            () = self.state.stream_shutdown_token.cancelled() => Err(RunError::Cancelled),
            outcome = bounded(timeout, final_content(run.into_events())) => outcome,
        };
        match &outcome {
            Err(RunError::TimedOut) => {
                agent
                    .cancel_and_close_mcp(request_id, "slack run timed out")
                    .await;
            }
            Err(RunError::Cancelled) => {
                agent
                    .cancel_and_close_mcp(request_id, "server shutting down")
                    .await;
            }
            Ok(_) | Err(_) => {}
        }
        outcome
    }
}

/// Drain the run and return the text of its final response.
async fn final_content<S>(mut events: S) -> Result<String, RunError>
where
    S: futures_util::Stream<Item = Result<StreamItem, aura::StreamError>> + Unpin,
{
    let mut content = None;
    while let Some(item) = events.next().await {
        match item {
            Ok(StreamItem::Final(info)) => content = Some(info.content),
            Ok(_) => {}
            Err(e) => return Err(RunError::Stream(e.to_string())),
        }
    }
    content.ok_or(RunError::NoFinal)
}

async fn bounded<F>(limit: Option<Duration>, work: F) -> Result<String, RunError>
where
    F: Future<Output = Result<String, RunError>>,
{
    match limit {
        Some(limit) => tokio::time::timeout(limit, work)
            .await
            .unwrap_or(Err(RunError::TimedOut)),
        None => work.await,
    }
}

#[derive(Debug, thiserror::Error)]
enum RunError {
    #[error(transparent)]
    Slack(#[from] SlackApiError),
    #[error("building agent: {0}")]
    Build(String),
    #[error("agent stream: {0}")]
    Stream(String),
    #[error("agent stream ended without a final response")]
    NoFinal,
    #[error("run exceeded the streaming timeout")]
    TimedOut,
    #[error("run cancelled by server shutdown")]
    Cancelled,
}

/// Map earlier messages, oldest first, onto chat history: the bot's own messages
/// become assistant turns, everyone else's become user turns with the
/// bot's handle stripped. Only messages older than the one being answered
/// (`current_ts`) count, so replies that landed while it waited for a slot
/// do not precede it; empty messages and system subtypes (joins, edits,
/// deletions) are left out, while the bot's own replies pass whatever
/// subtype Slack stamps on them, `bot_message` included. Only the newest
/// `HISTORY_LIMIT` turns are kept. Slack timestamps are fixed-width
/// `seconds.micros`, so string order is time order.
fn thread_history(replies: &[SlackMessage], bot: &BotIdentity, current_ts: &str) -> Vec<Message> {
    let turns: Vec<Message> = replies
        .iter()
        .filter(|m| m.ts.as_str() < current_ts && !m.text.trim().is_empty())
        .filter_map(|m| {
            let from_bot = m.user.as_deref() == Some(&bot.user_id)
                || (m.bot_id.is_some() && m.bot_id == bot.bot_id);
            let from_person = m
                .subtype
                .as_deref()
                .is_none_or(|subtype| HUMAN_SUBTYPES.contains(&subtype));
            if from_bot {
                Some(Message::assistant(&m.text))
            } else if from_person {
                Some(Message::user(strip_mentions(&m.text, &bot.user_id)))
            } else {
                None
            }
        })
        .collect();
    let skip = turns.len().saturating_sub(HISTORY_LIMIT);
    turns.into_iter().skip(skip).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bot() -> BotIdentity {
        BotIdentity {
            user_id: "UBOT".to_owned(),
            bot_id: Some("BBOT".to_owned()),
            team: "T".to_owned(),
            user: "aura".to_owned(),
        }
    }

    fn msg(ts: &str, user: Option<&str>, bot_id: Option<&str>, text: &str) -> SlackMessage {
        SlackMessage {
            ts: ts.to_owned(),
            user: user.map(str::to_owned),
            bot_id: bot_id.map(str::to_owned),
            text: text.to_owned(),
            subtype: None,
        }
    }

    fn roles(history: &[Message]) -> Vec<&'static str> {
        history
            .iter()
            .map(|m| match m {
                Message::User { .. } => "user",
                Message::Assistant { .. } => "assistant",
            })
            .collect()
    }

    #[test]
    fn thread_maps_roles_and_keeps_only_older_messages() {
        let mut bot_message = msg("3", None, Some("BBOT"), "bot answer by bot id");
        bot_message.subtype = Some("bot_message".to_owned());
        let replies = [
            msg("1", Some("U1"), None, "parent question"),
            msg("2", Some("UBOT"), None, "bot answer by user id"),
            bot_message,
            msg("4", Some("U2"), None, "  "),
            msg(
                "5",
                Some("U1"),
                None,
                "<@UBOT> follow-up being answered now",
            ),
            msg("6", Some("U3"), None, "landed while 5 waited for a slot"),
        ];
        let history = thread_history(&replies, &bot(), "5");
        assert_eq!(roles(&history), ["user", "assistant", "assistant"]);
    }

    #[test]
    fn thread_strips_the_bot_handle_from_user_turns() {
        let replies = [msg("1", Some("U1"), None, "<@UBOT> earlier question")];
        let history = thread_history(&replies, &bot(), "2");
        let Message::User { content } = &history[0] else {
            panic!("expected a user turn");
        };
        let text = format!("{content:?}");
        assert!(
            text.contains("earlier question") && !text.contains("UBOT"),
            "{text}"
        );
    }

    #[test]
    fn thread_skips_system_subtypes_and_keeps_the_newest() {
        let mut replies: Vec<SlackMessage> = (0..HISTORY_LIMIT + 5)
            .map(|i| msg(&format!("{i:04}"), Some("U1"), None, &format!("m{i}")))
            .collect();
        replies[0].subtype = Some("channel_join".to_owned());
        replies[1].subtype = Some("file_share".to_owned());
        let history = thread_history(&replies, &bot(), "9999");
        assert_eq!(history.len(), HISTORY_LIMIT);
        assert!(matches!(history.last(), Some(Message::User { .. })));
    }

    #[test]
    fn thread_keeps_bot_replies_whatever_their_subtype_and_drops_system_ones() {
        let mut bot_reply = msg("2", Some("UBOT"), Some("BBOT"), "earlier bot answer");
        bot_reply.subtype = Some("bot_message".to_owned());
        let mut join = msg("3", Some("U2"), None, "has joined the thread");
        join.subtype = Some("channel_join".to_owned());
        let mut upload = msg("4", Some("U1"), None, "see attached");
        upload.subtype = Some("file_share".to_owned());
        let replies = [
            msg("1", Some("U1"), None, "question"),
            bot_reply,
            join,
            upload,
        ];
        let history = thread_history(&replies, &bot(), "5");
        assert_eq!(roles(&history), ["user", "assistant", "user"]);
    }

    #[test]
    fn crypto_provider_is_installed_and_idempotent() {
        install_crypto_provider();
        install_crypto_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }

    #[tokio::test]
    async fn stream_without_final_is_a_failure() {
        let items: Vec<Result<StreamItem, aura::StreamError>> = vec![Ok(StreamItem::FinalMarker)];
        let err = final_content(futures_util::stream::iter(items))
            .await
            .unwrap_err();
        assert!(matches!(err, RunError::NoFinal), "{err}");
    }

    #[tokio::test]
    async fn bounded_reports_timeout() {
        let err = bounded(Some(Duration::from_millis(10)), std::future::pending())
            .await
            .unwrap_err();
        assert!(matches!(err, RunError::TimedOut), "{err}");
    }
}

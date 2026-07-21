//! Redis-backed cross-instance pub/sub over channels named `{p}:bus:{topic}`.
//!
//! One pub/sub connection per bus (i.e. per server instance), owned by a dispatcher task:
//! Redis multiplexes any number of `SUBSCRIBE`s on a single connection and
//! tags each message with its channel, so per-topic connections would only
//! re-implement demultiplexing Redis already does — and the pub/sub
//! connection's reconnect logic (it is not a `ConnectionManager`) then exists
//! exactly once, replaying the dispatcher's topic registry after a drop.
//! Publishing rides the shared auto-reconnecting `ConnectionManager` and never
//! touches the dispatcher.
//!
//! Redis pub/sub is fire-and-forget: payloads published while a subscriber's
//! instance is reconnecting are lost, and a slow subscriber that overflows its
//! [`TOPIC_CAPACITY`] buffer skips missed payloads. Callers own the backstop —
//! a parked approval that misses its wake fails closed at its timeout.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::time::Duration;

use async_trait::async_trait;
use aura::session_store::{EventBus, SessionStoreError, Subscription};
use bytes::Bytes;
use futures_util::StreamExt;
use redis::Client;
use redis::aio::{ConnectionManager, PubSubSink};
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::warn;

use super::request_err;

/// Buffered payloads per topic before slow subscribers start lagging.
const TOPIC_CAPACITY: usize = 64;
/// Delay before the dispatcher retries a dropped pub/sub connection.
const RECONNECT_DELAY: Duration = Duration::from_millis(500);

pub struct RedisEventBus {
    conn: ConnectionManager,
    key_prefix: String,
    commands: mpsc::UnboundedSender<BusCommand>,
    /// Bound on how long `subscribe` waits for its `SUBSCRIBE` to become
    /// active while the dispatcher is reconnecting.
    subscribe_timeout: Duration,
}

impl RedisEventBus {
    pub fn new(
        client: Client,
        conn: ConnectionManager,
        key_prefix: &str,
        subscribe_timeout: Duration,
    ) -> Self {
        let (commands, command_rx) = mpsc::unbounded_channel();
        tokio::spawn(dispatch(client, command_rx));
        Self {
            conn,
            key_prefix: key_prefix.to_string(),
            commands,
            subscribe_timeout,
        }
    }

    fn channel_name(&self, topic: &str) -> String {
        format!("{}:bus:{topic}", self.key_prefix)
    }
}

#[async_trait]
impl EventBus for RedisEventBus {
    async fn publish(&self, topic: &str, payload: Bytes) -> Result<(), SessionStoreError> {
        let mut conn = self.conn.clone();
        redis::AsyncCommands::publish::<_, _, ()>(
            &mut conn,
            self.channel_name(topic),
            payload.as_ref(),
        )
        .await
        .map_err(request_err)
    }

    async fn subscribe(&self, topic: &str) -> Result<Subscription, SessionStoreError> {
        let channel = self.channel_name(topic);
        let (ack, ready) = oneshot::channel();
        self.commands
            .send(BusCommand::Subscribe {
                channel: channel.clone(),
                ack,
            })
            .map_err(|_| dispatcher_stopped())?;
        // The dispatcher acks only after the redis SUBSCRIBE is active, so
        // every payload published after this returns is received.
        let mut rx = tokio::time::timeout(self.subscribe_timeout, ready)
            .await
            .map_err(|_| SessionStoreError::Request {
                reason: "timed out waiting for pub/sub subscription".to_string(),
            })?
            .map_err(|_| dispatcher_stopped())??;

        let guard = SubscriptionGuard {
            channel,
            commands: self.commands.clone(),
        };
        Ok(Box::pin(async_stream::stream! {
            // Owns the guard so its drop-notification outlives the receiver.
            let _guard = guard;
            loop {
                match rx.recv().await {
                    Ok(payload) => yield payload,
                    // A lagged subscriber skips missed payloads but stays
                    // subscribed.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }))
    }
}

fn dispatcher_stopped() -> SessionStoreError {
    SessionStoreError::Request {
        reason: "event bus dispatcher stopped".to_string(),
    }
}

/// Notifies the dispatcher when the handed-out subscription is dropped, so
/// the last subscriber of a channel triggers a redis `UNSUBSCRIBE`.
struct SubscriptionGuard {
    channel: String,
    commands: mpsc::UnboundedSender<BusCommand>,
}

impl Drop for SubscriptionGuard {
    fn drop(&mut self) {
        let _ = self.commands.send(BusCommand::Unsubscribe {
            channel: std::mem::take(&mut self.channel),
        });
    }
}

enum BusCommand {
    Subscribe {
        channel: String,
        ack: oneshot::Sender<Result<broadcast::Receiver<Bytes>, SessionStoreError>>,
    },
    Unsubscribe {
        channel: String,
    },
}

/// One local fan-out per subscribed channel. `subscribers` is the dispatcher's
/// own count (decremented by `Unsubscribe` messages) rather than
/// `Sender::receiver_count`, so unsubscribe accounting does not race receiver
/// drop timing.
struct TopicEntry {
    tx: broadcast::Sender<Bytes>,
    subscribers: usize,
}

enum AfterCommand {
    Continue,
    Reconnect,
}

/// The dispatcher: owns the pub/sub connection, the topic registry, and the
/// reconnect loop. Ends when the bus and every subscription guard are gone.
async fn dispatch(client: Client, mut commands: mpsc::UnboundedReceiver<BusCommand>) {
    let mut topics: HashMap<String, TopicEntry> = HashMap::new();
    loop {
        let pubsub = match client.get_async_pubsub().await {
            Ok(pubsub) => pubsub,
            Err(err) => {
                warn!(error = %err, "event bus pub/sub connection failed; retrying");
                tokio::time::sleep(RECONNECT_DELAY).await;
                continue;
            }
        };
        let (mut sink, mut stream) = pubsub.split();

        // Replay the registry so subscriptions survive the reconnect. A
        // failure restarts the connection; payloads published while down are
        // lost (fire-and-forget, see module doc).
        let mut replayed = true;
        for channel in topics.keys() {
            if let Err(err) = sink.subscribe(channel).await {
                warn!(error = %err, channel, "event bus resubscribe failed; reconnecting");
                replayed = false;
                break;
            }
        }
        if !replayed {
            tokio::time::sleep(RECONNECT_DELAY).await;
            continue;
        }

        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    None => return,
                    Some(command) => {
                        match handle_command(command, &mut topics, &mut sink).await {
                            AfterCommand::Continue => {}
                            AfterCommand::Reconnect => break,
                        }
                    }
                },
                message = stream.next() => match message {
                    Some(message) => {
                        if let Some(entry) = topics.get(message.get_channel_name()) {
                            // Err = no receiver polling right now; payloads
                            // are fire-and-forget.
                            let _ = entry
                                .tx
                                .send(Bytes::copy_from_slice(message.get_payload_bytes()));
                        }
                    }
                    None => {
                        warn!("event bus pub/sub connection lost; reconnecting");
                        break;
                    }
                },
            }
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn handle_command(
    command: BusCommand,
    topics: &mut HashMap<String, TopicEntry>,
    sink: &mut PubSubSink,
) -> AfterCommand {
    match command {
        BusCommand::Subscribe { channel, ack } => match topics.entry(channel) {
            Entry::Occupied(mut entry) => {
                let entry = entry.get_mut();
                if ack.send(Ok(entry.tx.subscribe())).is_ok() {
                    entry.subscribers += 1;
                }
                AfterCommand::Continue
            }
            Entry::Vacant(entry) => match sink.subscribe(entry.key()).await {
                Ok(()) => {
                    let (tx, rx) = broadcast::channel(TOPIC_CAPACITY);
                    if ack.send(Ok(rx)).is_ok() {
                        entry.insert(TopicEntry { tx, subscribers: 1 });
                    } else {
                        // The subscriber timed out waiting; leave the channel
                        // unsubscribed.
                        let _ = sink.unsubscribe(entry.key()).await;
                    }
                    AfterCommand::Continue
                }
                Err(err) => {
                    let _ = ack.send(Err(SessionStoreError::Request {
                        reason: format!("subscribe failed: {err}"),
                    }));
                    AfterCommand::Reconnect
                }
            },
        },
        BusCommand::Unsubscribe { channel } => {
            if let Entry::Occupied(mut entry) = topics.entry(channel) {
                let topic = entry.get_mut();
                topic.subscribers = topic.subscribers.saturating_sub(1);
                if topic.subscribers == 0 {
                    let (channel, _) = entry.remove_entry();
                    // Best-effort: an orphaned redis-side subscription only
                    // costs discarded deliveries until reconnect.
                    let _ = sink.unsubscribe(&channel).await;
                }
            }
            AfterCommand::Continue
        }
    }
}

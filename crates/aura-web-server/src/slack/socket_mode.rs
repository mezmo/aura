//! The Socket Mode connection: open a WebSocket, ack every envelope, hand
//! event callbacks to the runner, and reconnect when Slack asks or the
//! connection goes quiet.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::api::{SlackApi, SlackApiError};
use super::events::{DisconnectReason, EventCallback, Frame, parse_frame};

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);
/// Slack pings a healthy connection every 20 seconds or so.
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, thiserror::Error)]
pub enum SocketModeError {
    #[error(transparent)]
    Api(#[from] SlackApiError),
    #[error("websocket: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("websocket handshake not finished after {0:?}")]
    ConnectTimedOut(Duration),
    #[error("event consumer is gone")]
    ConsumerGone,
}

/// How a connection ended without a transport error.
#[derive(Debug, PartialEq, Eq)]
pub enum Disconnected {
    Slack(DisconnectReason),
    ClosedByPeer,
    StreamEnded,
    /// No frame arrived from Slack for this long.
    Idle(Duration),
    Shutdown,
}

/// Keep a Socket Mode connection up until `shutdown` is cancelled or the
/// `events` receiver is dropped. A fresh URL is minted for every connection.
/// Slack's own refresh and warning disconnects reconnect at once; any other
/// end backs off exponentially, so a peer that drops the link immediately,
/// or an app with Socket Mode switched off, cannot hammer
/// apps.connections.open.
pub async fn run(api: SlackApi, events: mpsc::Sender<EventCallback>, shutdown: CancellationToken) {
    run_with_idle_timeout(api, events, shutdown, IDLE_TIMEOUT).await;
}

async fn run_with_idle_timeout(
    api: SlackApi,
    events: mpsc::Sender<EventCallback>,
    shutdown: CancellationToken,
    idle_timeout: Duration,
) {
    let mut backoff = INITIAL_BACKOFF;
    loop {
        let attempt = async {
            let url = api.connections_open().await?;
            serve_connection(&url, &events, &shutdown, idle_timeout).await
        };
        let outcome = tokio::select! {
            biased;
            () = shutdown.cancelled() => return,
            outcome = attempt => outcome,
        };
        let pause = match outcome {
            Ok(Disconnected::Shutdown) | Err(SocketModeError::ConsumerGone) => return,
            Ok(Disconnected::Slack(
                reason @ (DisconnectReason::RefreshRequested | DisconnectReason::Warning),
            )) => {
                info!(?reason, "slack asked for a fresh socket mode connection");
                backoff = INITIAL_BACKOFF;
                continue;
            }
            Ok(Disconnected::Idle(silent_for)) => {
                warn!(
                    ?silent_for,
                    retry_in = ?backoff,
                    "slack socket mode connection went silent without closing; reconnecting"
                );
                backoff
            }
            Ok(ended) => {
                warn!(?ended, retry_in = ?backoff, "slack socket mode connection ended");
                backoff
            }
            Err(e) => {
                warn!(error = %e, retry_in = ?backoff, "slack socket mode connection failed");
                backoff
            }
        };
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(pause) => {}
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Serve one WebSocket connection to `url` until Slack closes it, `shutdown`
/// is cancelled, or the receiver behind `events` is gone. Every envelope is
/// acked before its event is forwarded, so a consumer that stalls can delay
/// later acks; the runner only spawns per event, so it never stalls long.
///
/// A connection whose path dies without a FIN or RST stays established and
/// never yields another frame, so the handshake and every read carry
/// `idle_timeout` as a deadline. Any frame restarts it, Slack's pings
/// included.
pub async fn serve_connection(
    url: &str,
    events: &mpsc::Sender<EventCallback>,
    shutdown: &CancellationToken,
    idle_timeout: Duration,
) -> Result<Disconnected, SocketModeError> {
    let (ws, _) = tokio::time::timeout(idle_timeout, tokio_tungstenite::connect_async(url))
        .await
        .map_err(|_| SocketModeError::ConnectTimedOut(idle_timeout))??;
    let (mut sink, mut stream) = ws.split();
    loop {
        let next = tokio::select! {
            biased;
            () = shutdown.cancelled() => {
                let _ = tokio::time::timeout(CLOSE_TIMEOUT, sink.send(Message::Close(None))).await;
                return Ok(Disconnected::Shutdown);
            }
            next = tokio::time::timeout(idle_timeout, stream.next()) => match next {
                Ok(next) => next,
                Err(_) => return Ok(Disconnected::Idle(idle_timeout)),
            },
        };
        let message = match next {
            Some(Ok(message)) => message,
            Some(Err(e)) => return Err(e.into()),
            None => return Ok(Disconnected::StreamEnded),
        };
        match message {
            Message::Text(text) => match parse_frame(text.as_str()) {
                Ok(Frame::Hello) => info!("slack socket mode connected"),
                Ok(Frame::Disconnect { reason }) => return Ok(Disconnected::Slack(reason)),
                Ok(Frame::Envelope { envelope_id, event }) => {
                    let ack = serde_json::json!({ "envelope_id": envelope_id }).to_string();
                    sink.send(Message::text(ack)).await?;
                    let Some(event) = event else { continue };
                    let forwarded = tokio::select! {
                        biased;
                        () = shutdown.cancelled() => return Ok(Disconnected::Shutdown),
                        forwarded = events.send(*event) => forwarded,
                    };
                    if forwarded.is_err() {
                        return Err(SocketModeError::ConsumerGone);
                    }
                }
                Ok(Frame::Other(kind)) => debug!(kind, "ignoring slack socket mode frame"),
                Err(e) => warn!(error = %e, "undecodable slack socket mode frame"),
            },
            Message::Ping(payload) => sink.send(Message::Pong(payload)).await?,
            Message::Close(_) => return Ok(Disconnected::ClosedByPeer),
            Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slack::api::{AppToken, BotToken};
    use crate::slack::events::Event;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::tungstenite::Utf8Bytes;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const MENTION_ENVELOPE: &str = r#"{
        "type": "events_api", "envelope_id": "env-1",
        "payload": {"event": {"type": "app_mention", "user": "U1", "text": "<@UB> hi",
                              "ts": "1.0", "channel": "C1"}}
    }"#;

    /// A fake Slack edge: accepts `connections` clients in turn, running
    /// `script` against each with its 1-based ordinal.
    async fn fake_slack<F, Fut>(connections: usize, script: F) -> String
    where
        F: Fn(usize, WebSocketStream<TcpStream>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for ordinal in 1..=connections {
                let (tcp, _) = listener.accept().await.unwrap();
                let ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
                script(ordinal, ws).await;
            }
        });
        format!("ws://{addr}")
    }

    async fn read_text(ws: &mut WebSocketStream<TcpStream>) -> Utf8Bytes {
        match ws.next().await.unwrap().unwrap() {
            Message::Text(text) => text,
            other => panic!("expected text, got {other:?}"),
        }
    }

    async fn send(ws: &mut WebSocketStream<TcpStream>, text: &str) {
        ws.send(Message::text(text)).await.unwrap();
    }

    #[tokio::test]
    async fn acks_envelopes_and_forwards_events_until_disconnect() {
        let url = fake_slack(1, |_, mut ws| async move {
            send(&mut ws, r#"{"type":"hello"}"#).await;
            send(&mut ws, MENTION_ENVELOPE).await;
            let ack: serde_json::Value =
                serde_json::from_str(read_text(&mut ws).await.as_str()).unwrap();
            assert_eq!(ack["envelope_id"], "env-1");
            send(
                &mut ws,
                r#"{"type":"interactive","envelope_id":"env-2","payload":{}}"#,
            )
            .await;
            let ack: serde_json::Value =
                serde_json::from_str(read_text(&mut ws).await.as_str()).unwrap();
            assert_eq!(ack["envelope_id"], "env-2");
            send(
                &mut ws,
                r#"{"type":"disconnect","reason":"refresh_requested"}"#,
            )
            .await;
        })
        .await;

        let (tx, mut rx) = mpsc::channel(4);
        let ended = serve_connection(&url, &tx, &CancellationToken::new(), IDLE_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(
            ended,
            Disconnected::Slack(DisconnectReason::RefreshRequested)
        );

        let callback = rx.recv().await.unwrap();
        assert!(matches!(callback.event, Event::AppMention(ref m) if m.channel == "C1"));
        assert!(
            rx.try_recv().is_err(),
            "ack-only envelope must not become an event"
        );
    }

    #[tokio::test]
    async fn dropped_consumer_ends_the_connection() {
        let url = fake_slack(1, |_, mut ws| async move {
            send(&mut ws, MENTION_ENVELOPE).await;
            let _ack = read_text(&mut ws).await;
            // Keep the socket open so only the consumer can end the loop.
            let _ = ws.next().await;
        })
        .await;

        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let err = serve_connection(&url, &tx, &CancellationToken::new(), IDLE_TIMEOUT)
            .await
            .unwrap_err();
        assert!(matches!(err, SocketModeError::ConsumerGone), "{err}");
    }

    #[tokio::test]
    async fn shutdown_closes_the_connection_even_with_a_stalled_consumer() {
        let url = fake_slack(1, |_, mut ws| async move {
            send(&mut ws, MENTION_ENVELOPE).await;
            let _ack = read_text(&mut ws).await;
            let _ = ws.next().await;
        })
        .await;

        let shutdown = CancellationToken::new();
        // Capacity 0 is not allowed; a full channel of 1 that nobody drains
        // stalls the forward instead.
        let (tx, _rx) = mpsc::channel(1);
        tx.try_send(EventCallback {
            event: Event::Other,
        })
        .unwrap();
        let stopper = shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            stopper.cancel();
        });
        let ended = serve_connection(&url, &tx, &shutdown, IDLE_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(ended, Disconnected::Shutdown);
    }

    #[tokio::test]
    async fn pings_are_answered() {
        let url = fake_slack(1, |_, mut ws| async move {
            ws.send(Message::Ping("p".into())).await.unwrap();
            match ws.next().await.unwrap().unwrap() {
                Message::Pong(payload) => assert_eq!(payload.as_ref(), b"p"),
                other => panic!("expected pong, got {other:?}"),
            }
            ws.send(Message::Close(None)).await.unwrap();
        })
        .await;

        let (tx, _rx) = mpsc::channel(1);
        let ended = serve_connection(&url, &tx, &CancellationToken::new(), IDLE_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(ended, Disconnected::ClosedByPeer);
    }

    #[tokio::test]
    async fn a_silent_connection_ends_as_idle() {
        let url = fake_slack(1, |_, mut ws| async move {
            send(&mut ws, r#"{"type":"hello"}"#).await;
            // Hold the socket open and say nothing more: no close frame.
            let _ = ws.next().await;
        })
        .await;

        let (tx, _rx) = mpsc::channel(1);
        let idle = Duration::from_millis(100);
        let ended = serve_connection(&url, &tx, &CancellationToken::new(), idle)
            .await
            .unwrap();
        assert_eq!(ended, Disconnected::Idle(idle));
    }

    #[tokio::test]
    async fn pings_keep_a_quiet_connection_alive() {
        let idle = Duration::from_millis(300);
        let url = fake_slack(1, move |_, mut ws| async move {
            // Pings alone for three idle periods, each well inside one.
            for _ in 0..30 {
                ws.send(Message::Ping("p".into())).await.unwrap();
                tokio::time::sleep(idle / 10).await;
            }
            ws.send(Message::Close(None)).await.unwrap();
        })
        .await;

        let (tx, _rx) = mpsc::channel(1);
        let ended = serve_connection(&url, &tx, &CancellationToken::new(), idle)
            .await
            .unwrap();
        assert_eq!(ended, Disconnected::ClosedByPeer);
    }

    #[tokio::test]
    async fn a_handshake_that_never_finishes_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // Accept the TCP connection and never answer the upgrade.
            let (_tcp, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });

        let (tx, _rx) = mpsc::channel(1);
        let err = serve_connection(
            &format!("ws://{addr}"),
            &tx,
            &CancellationToken::new(),
            Duration::from_millis(100),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, SocketModeError::ConnectTimedOut(_)), "{err}");
    }

    #[tokio::test]
    async fn run_reconnects_when_the_connection_goes_silent() {
        let shutdown = CancellationToken::new();
        let stopper = shutdown.clone();
        let hellos = Arc::new(AtomicUsize::new(0));
        let hellos_seen = Arc::clone(&hellos);
        let url = fake_slack(2, move |ordinal, mut ws| {
            let stopper = stopper.clone();
            let hellos = Arc::clone(&hellos_seen);
            async move {
                send(&mut ws, r#"{"type":"hello"}"#).await;
                hellos.fetch_add(1, Ordering::SeqCst);
                if ordinal == 1 {
                    // Go silent with the socket held open, off the accept
                    // loop so the second connection can be served.
                    tokio::spawn(async move {
                        let _ = ws.next().await;
                    });
                } else {
                    stopper.cancel();
                    let _ = ws.next().await;
                }
            }
        })
        .await;

        let slack = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/apps.connections.open"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": true, "url": url})),
            )
            .expect(2)
            .mount(&slack)
            .await;
        let api = SlackApi::new(
            BotToken::new("xoxb-b".to_owned()).unwrap(),
            AppToken::new("xapp-a".to_owned()).unwrap(),
        )
        .with_base_url(slack.uri());

        let (tx, _rx) = mpsc::channel(1);
        tokio::time::timeout(
            Duration::from_secs(5),
            run_with_idle_timeout(api, tx, shutdown, Duration::from_millis(100)),
        )
        .await
        .expect("run must return once shutdown is cancelled");
        assert_eq!(hellos.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn run_reconnects_on_refresh_and_stops_on_shutdown() {
        let shutdown = CancellationToken::new();
        let stopper = shutdown.clone();
        let hellos = Arc::new(AtomicUsize::new(0));
        let hellos_seen = Arc::clone(&hellos);
        let url = fake_slack(2, move |ordinal, mut ws| {
            let stopper = stopper.clone();
            let hellos = Arc::clone(&hellos_seen);
            async move {
                send(&mut ws, r#"{"type":"hello"}"#).await;
                hellos.fetch_add(1, Ordering::SeqCst);
                if ordinal == 1 {
                    send(
                        &mut ws,
                        r#"{"type":"disconnect","reason":"refresh_requested"}"#,
                    )
                    .await;
                } else {
                    stopper.cancel();
                    let _ = ws.next().await;
                }
            }
        })
        .await;

        let slack = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/apps.connections.open"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": true, "url": url})),
            )
            .expect(2)
            .mount(&slack)
            .await;
        let api = SlackApi::new(
            BotToken::new("xoxb-b".to_owned()).unwrap(),
            AppToken::new("xapp-a".to_owned()).unwrap(),
        )
        .with_base_url(slack.uri());

        let (tx, _rx) = mpsc::channel(1);
        tokio::time::timeout(Duration::from_secs(5), run(api, tx, shutdown))
            .await
            .expect("run must return once shutdown is cancelled");
        assert_eq!(hellos.load(Ordering::SeqCst), 2);
    }
}

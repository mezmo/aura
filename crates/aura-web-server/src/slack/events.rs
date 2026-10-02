//! Socket Mode frames and the Slack events the ingress answers.

use serde::Deserialize;
use std::collections::{HashSet, VecDeque};

/// Why Slack is closing a Socket Mode connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisconnectReason {
    RefreshRequested,
    Warning,
    LinkDisabled,
    Other(String),
}

impl From<String> for DisconnectReason {
    fn from(raw: String) -> Self {
        match raw.as_str() {
            "refresh_requested" => Self::RefreshRequested,
            "warning" => Self::Warning,
            "link_disabled" => Self::LinkDisabled,
            _ => Self::Other(raw),
        }
    }
}

/// One frame off the Socket Mode WebSocket.
#[derive(Debug)]
pub enum Frame {
    Hello,
    Disconnect {
        reason: DisconnectReason,
    },
    /// Anything Slack expects an acknowledgement for.
    Envelope {
        envelope_id: String,
        event: Option<EventCallback>,
    },
    Other(String),
}

#[derive(Deserialize)]
struct RawFrame {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    envelope_id: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    payload: Option<serde_json::Value>,
}

/// Decode one text frame. Every frame carrying an `envelope_id` comes back
/// as [`Frame::Envelope`], whether or not its payload is an Events API
/// callback the ingress understands, so the caller can ack it and stop
/// Slack from redelivering it. A payload that fails to decode is logged and
/// treated like an unknown kind for the same reason.
pub fn parse_frame(text: &str) -> Result<Frame, serde_json::Error> {
    let raw: RawFrame = serde_json::from_str(text)?;
    let frame = match (raw.kind.as_str(), raw.envelope_id) {
        ("hello", _) => Frame::Hello,
        ("disconnect", _) => Frame::Disconnect {
            reason: raw.reason.unwrap_or_default().into(),
        },
        ("events_api", Some(envelope_id)) => {
            let event = raw
                .payload
                .map(serde_json::from_value::<EventCallback>)
                .transpose()
                .unwrap_or_else(|e| {
                    tracing::warn!(envelope_id, error = %e, "undecodable slack event payload");
                    None
                });
            Frame::Envelope { envelope_id, event }
        }
        (_, Some(envelope_id)) => Frame::Envelope {
            envelope_id,
            event: None,
        },
        (_, None) => Frame::Other(raw.kind),
    };
    Ok(frame)
}

#[derive(Debug, Deserialize)]
pub struct EventCallback {
    pub event: Event,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    AppMention(MessageEvent),
    Message(MessageEvent),
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub struct MessageEvent {
    pub channel: String,
    pub ts: String,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub thread_ts: Option<String>,
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(default)]
    pub bot_id: Option<String>,
    #[serde(default)]
    pub channel_type: Option<String>,
}

/// A message the agent answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inbound {
    pub channel: String,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub user: String,
    pub text: String,
}

impl Inbound {
    /// The thread the reply goes into: the message's own thread, or the
    /// message itself as a new thread parent.
    pub fn reply_thread(&self) -> &str {
        self.thread_ts.as_deref().unwrap_or(&self.ts)
    }
}

/// Subtypes that are still a person's message: a file upload with a
/// caption, and a thread reply also sent to the channel.
pub(super) const HUMAN_SUBTYPES: [&str; 2] = ["file_share", "thread_broadcast"];

/// Decide whether `event` is something the bot answers. Mentions anywhere
/// and direct messages qualify; the bot's own messages and system subtypes
/// such as edits, deletions, and joins do not.
pub fn accept(event: Event, self_user_id: &str) -> Option<Inbound> {
    let (message, is_dm) = match event {
        Event::AppMention(m) => (m, false),
        Event::Message(m) => {
            let is_dm = m.channel_type.as_deref() == Some("im");
            if !is_dm {
                return None;
            }
            (m, true)
        }
        Event::Other => return None,
    };
    if message.bot_id.is_some()
        || message
            .subtype
            .as_deref()
            .is_some_and(|subtype| !HUMAN_SUBTYPES.contains(&subtype))
    {
        return None;
    }
    let user = message.user?;
    if user == self_user_id {
        return None;
    }
    let text = strip_mentions(&message.text, self_user_id);
    if text.is_empty() && !is_dm {
        return None;
    }
    Some(Inbound {
        channel: message.channel,
        ts: message.ts,
        thread_ts: message.thread_ts,
        user,
        text,
    })
}

/// Remove `<@USER>` and `<@USER|name>` tokens that name the bot itself,
/// along with one space that followed each, and trim the ends. Other
/// whitespace, newlines included, is kept as the user typed it.
pub fn strip_mentions(text: &str, self_user_id: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<@") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('>') {
            Some(end) => {
                let id = after[..end].split('|').next().unwrap_or("");
                rest = &after[end + 1..];
                if id == self_user_id {
                    rest = rest.strip_prefix(' ').unwrap_or(rest);
                } else {
                    out.push_str("<@");
                    out.push_str(&after[..=end]);
                }
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out.trim().to_owned()
}

/// Bounded memory of `(channel, ts)` pairs already answered; the oldest
/// pair is forgotten once `capacity` is reached.
#[derive(Debug)]
pub struct SeenMessages {
    order: VecDeque<(String, String)>,
    set: HashSet<(String, String)>,
    capacity: usize,
}

impl SeenMessages {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            order: VecDeque::with_capacity(capacity),
            set: HashSet::with_capacity(capacity),
            capacity,
        }
    }

    /// Record the message; `true` when it was not seen before.
    pub fn insert(&mut self, channel: &str, ts: &str) -> bool {
        let key = (channel.to_owned(), ts.to_owned());
        if !self.set.insert(key.clone()) {
            return false;
        }
        if self.order.len() >= self.capacity
            && let Some(oldest) = self.order.pop_front()
        {
            self.set.remove(&oldest);
        }
        self.order.push_back(key);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SELF: &str = "UBOT";

    fn mention(text: &str) -> Event {
        Event::AppMention(MessageEvent {
            channel: "C1".to_owned(),
            ts: "1.0".to_owned(),
            user: Some("U1".to_owned()),
            text: text.to_owned(),
            thread_ts: None,
            subtype: None,
            bot_id: None,
            channel_type: None,
        })
    }

    fn dm(user: Option<&str>, subtype: Option<&str>, bot_id: Option<&str>) -> Event {
        Event::Message(MessageEvent {
            channel: "D1".to_owned(),
            ts: "2.0".to_owned(),
            user: user.map(str::to_owned),
            text: "hi".to_owned(),
            thread_ts: None,
            subtype: subtype.map(str::to_owned),
            bot_id: bot_id.map(str::to_owned),
            channel_type: Some("im".to_owned()),
        })
    }

    #[test]
    fn parses_hello_and_disconnect() {
        assert!(matches!(
            parse_frame(r#"{"type":"hello","num_connections":1}"#).unwrap(),
            Frame::Hello
        ));
        match parse_frame(r#"{"type":"disconnect","reason":"refresh_requested"}"#).unwrap() {
            Frame::Disconnect { reason } => assert_eq!(reason, DisconnectReason::RefreshRequested),
            other => panic!("{other:?}"),
        }
        match parse_frame(r#"{"type":"disconnect","reason":"maintenance"}"#).unwrap() {
            Frame::Disconnect { reason } => {
                assert_eq!(reason, DisconnectReason::Other("maintenance".to_owned()));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_events_api_envelope() {
        let text = r#"{
            "type": "events_api",
            "envelope_id": "env-1",
            "accepts_response_payload": false,
            "payload": {
                "type": "event_callback",
                "event_id": "Ev1",
                "event": {"type": "app_mention", "user": "U1", "text": "<@UBOT> hi",
                          "ts": "1.0", "channel": "C1", "event_ts": "1.0"}
            }
        }"#;
        match parse_frame(text).unwrap() {
            Frame::Envelope { envelope_id, event } => {
                assert_eq!(envelope_id, "env-1");
                let Some(EventCallback {
                    event: Event::AppMention(m),
                }) = event
                else {
                    panic!("not a mention");
                };
                assert_eq!(m.channel, "C1");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unknown_envelope_kinds_are_still_acked() {
        match parse_frame(r#"{"type":"interactive","envelope_id":"env-2","payload":{}}"#).unwrap() {
            Frame::Envelope { envelope_id, event } => {
                assert_eq!(envelope_id, "env-2");
                assert!(event.is_none());
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            parse_frame(r#"{"type":"something_new"}"#).unwrap(),
            Frame::Other(kind) if kind == "something_new"
        ));
    }

    #[test]
    fn unknown_event_types_decode_as_other() {
        let text = r#"{"type":"events_api","envelope_id":"e","payload":{"event":{"type":"reaction_added"}}}"#;
        match parse_frame(text).unwrap() {
            Frame::Envelope {
                event: Some(cb), ..
            } => assert!(matches!(cb.event, Event::Other)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn mention_is_accepted_with_the_bot_handle_removed() {
        let inbound = accept(mention("<@UBOT> what's up <@U2|dan>?"), SELF).unwrap();
        assert_eq!(inbound.text, "what's up <@U2|dan>?");
        assert_eq!(inbound.user, "U1");
        assert_eq!(inbound.reply_thread(), "1.0");
    }

    #[test]
    fn empty_mention_is_dropped_but_empty_dm_is_not() {
        assert!(accept(mention("<@UBOT|aura>"), SELF).is_none());
        let mut empty_dm = dm(Some("U1"), None, None);
        if let Event::Message(m) = &mut empty_dm {
            m.text.clear();
        }
        assert!(accept(empty_dm, SELF).is_some());
    }

    #[test]
    fn direct_messages_are_accepted_and_filtered() {
        assert!(accept(dm(Some("U1"), None, None), SELF).is_some());
        assert!(accept(dm(Some(SELF), None, None), SELF).is_none());
        assert!(accept(dm(Some("U1"), Some("message_changed"), None), SELF).is_none());
        assert!(accept(dm(Some("U1"), None, Some("B9")), SELF).is_none());
        assert!(accept(dm(None, None, None), SELF).is_none());
    }

    #[test]
    fn channel_messages_without_a_mention_are_ignored() {
        let channel_message = Event::Message(MessageEvent {
            channel: "C1".to_owned(),
            ts: "3.0".to_owned(),
            user: Some("U1".to_owned()),
            text: "just chatting".to_owned(),
            thread_ts: None,
            subtype: None,
            bot_id: None,
            channel_type: Some("channel".to_owned()),
        });
        assert!(accept(channel_message, SELF).is_none());
    }

    #[test]
    fn thread_replies_keep_their_thread() {
        let mut event = mention("<@UBOT> more");
        if let Event::AppMention(m) = &mut event {
            m.thread_ts = Some("0.5".to_owned());
        }
        assert_eq!(accept(event, SELF).unwrap().reply_thread(), "0.5");
    }

    #[test]
    fn strip_mentions_handles_unterminated_token() {
        assert_eq!(strip_mentions("hey <@UBOT", SELF), "hey <@UBOT");
    }

    #[test]
    fn strip_mentions_keeps_newlines_and_other_mentions() {
        let text = "<@UBOT> look:\n```\nfn  main() {}\n```\ncc <@U2|dan>  thanks";
        assert_eq!(
            strip_mentions(text, SELF),
            "look:\n```\nfn  main() {}\n```\ncc <@U2|dan>  thanks"
        );
        assert_eq!(strip_mentions("hi <@UBOT> there", SELF), "hi there");
        assert_eq!(strip_mentions("hi <@UBOT>", SELF), "hi");
    }

    #[test]
    fn human_subtypes_are_accepted() {
        assert!(accept(dm(Some("U1"), Some("file_share"), None), SELF).is_some());
        assert!(accept(dm(Some("U1"), Some("thread_broadcast"), None), SELF).is_some());
        assert!(accept(dm(Some("U1"), Some("message_deleted"), None), SELF).is_none());
    }

    #[test]
    fn undecodable_payload_still_yields_an_ackable_envelope() {
        let text = r#"{"type":"events_api","envelope_id":"env-9","payload":{"event":{"type":"message","ts":5}}}"#;
        match parse_frame(text).unwrap() {
            Frame::Envelope { envelope_id, event } => {
                assert_eq!(envelope_id, "env-9");
                assert!(event.is_none());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn seen_messages_dedupe_and_evict() {
        let mut seen = SeenMessages::new(2);
        assert!(seen.insert("C1", "1"));
        assert!(!seen.insert("C1", "1"));
        assert!(seen.insert("C1", "2"));
        assert!(seen.insert("C1", "3"));
        assert!(seen.insert("C1", "1"), "oldest entry was evicted");
    }
}

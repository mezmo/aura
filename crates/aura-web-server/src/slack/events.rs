//! Socket Mode frames and the Slack events the ingress answers.

use super::api::ActionToken;
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
        event: Option<Box<EventCallback>>,
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
                })
                .map(Box::new);
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
    #[serde(default)]
    pub action_token: Option<ActionToken>,
    #[serde(default)]
    pub assistant_thread: Option<AssistantThread>,
}

/// The `assistant_thread` object some message events carry.
#[derive(Debug, Deserialize)]
pub struct AssistantThread {
    #[serde(default)]
    pub action_token: Option<ActionToken>,
}

/// A message the agent answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inbound {
    pub channel: String,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub user: String,
    pub text: String,
    /// Direct message to the bot, as opposed to a channel mention.
    pub is_dm: bool,
    /// A thread reply in a channel that did not mention the bot.
    pub unaddressed_reply: bool,
    pub action_token: Option<ActionToken>,
}

impl Inbound {
    /// Where the reply goes: the message's own thread when it has one; in a
    /// channel, a new thread under the message; in a DM at top level, the
    /// conversation itself (`None`), since a DM is already one conversation.
    pub fn reply_thread(&self) -> Option<&str> {
        self.thread_ts
            .as_deref()
            .or((!self.is_dm).then_some(self.ts.as_str()))
    }
}

/// Subtypes that are still a person's message: a file upload with a
/// caption, and a thread reply also sent to the channel.
pub(super) const HUMAN_SUBTYPES: [&str; 2] = ["file_share", "thread_broadcast"];

/// Decide whether `event` is something the bot answers. Mentions anywhere,
/// direct messages, and replies inside channel threads qualify; the bot's
/// own messages, top-level channel chatter, and system subtypes such as
/// edits, deletions, and joins do not. A thread reply that does not mention
/// the bot is marked `unaddressed_reply`: the caller answers it only when
/// the bot already took part in that thread, which the event alone cannot
/// show. A reply that does mention the bot also arrives as `app_mention`,
/// and the `(channel, ts)` dedupe keeps whichever came first.
pub fn accept(event: Event, self_user_id: &str) -> Option<Inbound> {
    let mention = format!("<@{self_user_id}");
    let (message, is_dm, unaddressed_reply) = match event {
        Event::AppMention(m) => (m, false, false),
        Event::Message(m) => match m.channel_type.as_deref() {
            Some("im") => (m, true, false),
            Some("channel" | "group") if m.thread_ts.is_some() => {
                let addressed = m.text.contains(&mention);
                (m, false, !addressed)
            }
            _ => return None,
        },
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
    // Slack's own handlers read the token at the top of the event; older
    // payloads nest it under `assistant_thread`.
    let action_token = message
        .action_token
        .or_else(|| message.assistant_thread.and_then(|t| t.action_token));
    Some(Inbound {
        channel: message.channel,
        ts: message.ts,
        thread_ts: message.thread_ts,
        user,
        text,
        is_dm,
        unaddressed_reply,
        action_token,
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

/// Bounded memory of `(channel, ts)` pairs already answered.
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

    /// Record the message; `true` when it was not seen before. Once the set
    /// holds `capacity` pairs, recording a new one forgets the oldest.
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
            action_token: None,
            assistant_thread: None,
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
            action_token: None,
            assistant_thread: None,
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
                }) = event.map(|cb| *cb)
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
        assert_eq!(inbound.reply_thread(), Some("1.0"));
        assert!(!inbound.is_dm);
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

    fn channel_message(text: &str, thread_ts: Option<&str>, channel_type: &str) -> Event {
        Event::Message(MessageEvent {
            channel: "C1".to_owned(),
            ts: "3.0".to_owned(),
            user: Some("U1".to_owned()),
            text: text.to_owned(),
            thread_ts: thread_ts.map(str::to_owned),
            subtype: None,
            bot_id: None,
            channel_type: Some(channel_type.to_owned()),
            action_token: None,
            assistant_thread: None,
        })
    }

    #[test]
    fn top_level_channel_messages_without_a_mention_are_ignored() {
        assert!(accept(channel_message("just chatting", None, "channel"), SELF).is_none());
    }

    #[test]
    fn thread_replies_in_channels_are_accepted_as_unaddressed() {
        let reply = accept(channel_message("and then?", Some("1.0"), "channel"), SELF).unwrap();
        assert!(reply.unaddressed_reply);
        assert!(!reply.is_dm);
        assert_eq!(reply.reply_thread(), Some("1.0"));

        let private = accept(channel_message("same here", Some("1.0"), "group"), SELF).unwrap();
        assert!(private.unaddressed_reply);

        let addressed = accept(
            channel_message("<@UBOT> and then?", Some("1.0"), "channel"),
            SELF,
        )
        .unwrap();
        assert!(!addressed.unaddressed_reply);
        assert_eq!(addressed.text, "and then?");

        let mention = accept(mention("<@UBOT> hi"), SELF).unwrap();
        assert!(!mention.unaddressed_reply);
    }

    #[test]
    fn thread_replies_keep_their_thread() {
        let mut event = mention("<@UBOT> more");
        if let Event::AppMention(m) = &mut event {
            m.thread_ts = Some("0.5".to_owned());
        }
        assert_eq!(accept(event, SELF).unwrap().reply_thread(), Some("0.5"));
    }

    #[test]
    fn direct_messages_reply_at_top_level_unless_threaded() {
        let top_level = accept(dm(Some("U1"), None, None), SELF).unwrap();
        assert!(top_level.is_dm);
        assert_eq!(top_level.reply_thread(), None);

        let mut threaded = dm(Some("U1"), None, None);
        if let Event::Message(m) = &mut threaded {
            m.thread_ts = Some("1.5".to_owned());
        }
        assert_eq!(accept(threaded, SELF).unwrap().reply_thread(), Some("1.5"));
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

    fn event_json(kind: &str, extra: &str) -> String {
        format!(
            r#"{{"type": "event_callback", "event": {{"type": "{kind}", "user": "U1",
                "text": "<@UBOT> where did we discuss gizmo", "ts": "1.0", "channel": "D1",
                "channel_type": "im"{extra}}}}}"#
        )
    }

    fn accepted(json: &str) -> Inbound {
        let callback: EventCallback = serde_json::from_str(json).unwrap();
        accept(callback.event, SELF).unwrap()
    }

    #[test]
    fn action_token_rides_along_from_either_shape_on_both_kinds() {
        for kind in ["app_mention", "message"] {
            let top = accepted(&event_json(kind, r#", "action_token": "12345.98765.abcd""#));
            assert_eq!(
                top.action_token,
                Some(ActionToken::new("12345.98765.abcd".to_owned())),
                "{kind}"
            );
            let nested = accepted(&event_json(
                kind,
                r#", "assistant_thread": {"action_token": "nested.token"}"#,
            ));
            assert_eq!(
                nested.action_token,
                Some(ActionToken::new("nested.token".to_owned())),
                "{kind}"
            );
            let both = accepted(&event_json(
                kind,
                r#", "action_token": "top", "assistant_thread": {"action_token": "nested"}"#,
            ));
            assert_eq!(both.action_token, Some(ActionToken::new("top".to_owned())));
            assert_eq!(accepted(&event_json(kind, "")).action_token, None, "{kind}");
        }
    }

    #[test]
    fn inbound_debug_never_shows_the_action_token() {
        let inbound = accepted(&event_json(
            "message",
            r#", "action_token": "secret-token""#,
        ));
        assert!(!format!("{inbound:?}").contains("secret"));
    }
}

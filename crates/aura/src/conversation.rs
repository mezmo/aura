//! Conversations and their lanes.
//!
//! A conversation is a chat id and the agent that answers it. It has up to
//! [`LANES_PER_CONVERSATION`] sessions, called lanes, so that overlapping
//! requests on one chat id each run on a session of their own.

use std::fmt;

use aura_events::SessionId;
use serde::{Deserialize, Serialize};

/// The most lanes one conversation has.
pub const LANES_PER_CONVERSATION: usize = 4;

// Every conversation has at least its home lane, and lane numbers are `u8`.
const _: () = assert!(LANES_PER_CONVERSATION >= 1 && LANES_PER_CONVERSATION <= u8::MAX as usize);

/// A chat id and the agent serving it. Two agents under one chat id are two
/// conversations.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Conversation {
    chat: SessionId,
    /// The agent's `Config::agent_id`.
    agent: String,
}

impl Conversation {
    pub fn new(chat: SessionId, agent: impl Into<String>) -> Self {
        Self {
            chat,
            agent: agent.into(),
        }
    }

    /// The chat id the conversation runs under.
    pub fn chat(&self) -> &SessionId {
        &self.chat
    }

    pub fn agent(&self) -> &str {
        &self.agent
    }

    /// Every lane of the conversation, from its home lane up.
    pub fn lanes(&self) -> impl Iterator<Item = Lane> + '_ {
        LaneNumber::all().map(|number| Lane::new(self.clone(), number))
    }
}

impl fmt::Display for Conversation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "chat `{}` with agent `{}`", self.chat, self.agent)
    }
}

/// One of a conversation's sessions.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Lane {
    conversation: Conversation,
    number: LaneNumber,
}

impl Lane {
    pub fn new(conversation: Conversation, number: LaneNumber) -> Self {
        Self {
            conversation,
            number,
        }
    }

    /// The conversation's first lane.
    pub fn home(conversation: Conversation) -> Self {
        Self::new(conversation, LaneNumber::HOME)
    }

    /// The id of the session the lane runs on: `{chat}:{agent}:{n}`. Any `%`
    /// or `:` in the agent id is escaped as `%25` or `%3A`.
    ///
    /// Two lanes never get the same id. The number comes after the last `:`.
    /// The escaped agent id has no `:`, so it sits between that one and the
    /// `:` before it, and everything before that is the chat id. `%` is
    /// escaped first so that an agent id containing `%3A` and one containing
    /// `:` stay different.
    pub fn session_id(&self) -> SessionId {
        let agent = self
            .conversation
            .agent
            .replace('%', "%25")
            .replace(':', "%3A");
        SessionId::new(format!(
            "{}:{agent}:{}",
            self.conversation.chat, self.number
        ))
        .expect("a lane id ends with its number, so it is not blank")
    }

    pub fn conversation(&self) -> &Conversation {
        &self.conversation
    }

    pub fn number(&self) -> LaneNumber {
        self.number
    }
}

/// The session a run landed on: a plain session, or a lane of a
/// conversation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Landing {
    Session(SessionId),
    Lane(Lane),
}

impl Landing {
    pub fn session_id(&self) -> SessionId {
        match self {
            Self::Session(id) => id.clone(),
            Self::Lane(lane) => lane.session_id(),
        }
    }

    pub fn lane(&self) -> Option<&Lane> {
        match self {
            Self::Session(_) => None,
            Self::Lane(lane) => Some(lane),
        }
    }
}

/// A lane's place in its conversation, below [`LANES_PER_CONVERSATION`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct LaneNumber(u8);

impl LaneNumber {
    /// The first lane.
    pub const HOME: Self = Self(0);

    pub fn new(number: u8) -> Result<Self, LaneOutOfRange> {
        if usize::from(number) < LANES_PER_CONVERSATION {
            Ok(Self(number))
        } else {
            Err(LaneOutOfRange(number))
        }
    }

    /// The lane after this one, or `None` for the last.
    pub fn next(self) -> Option<Self> {
        Self::new(self.0 + 1).ok()
    }

    /// Every lane number, from [`HOME`](Self::HOME) up.
    pub fn all() -> impl Iterator<Item = Self> {
        std::iter::successors(Some(Self::HOME), |number| number.next())
    }

    pub fn get(self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for LaneNumber {
    type Error = LaneOutOfRange;

    fn try_from(number: u8) -> Result<Self, Self::Error> {
        Self::new(number)
    }
}

impl From<LaneNumber> for u8 {
    fn from(number: LaneNumber) -> Self {
        number.0
    }
}

impl fmt::Display for LaneNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A lane number at or past [`LANES_PER_CONVERSATION`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("lane {0} is out of range; a conversation has {LANES_PER_CONVERSATION} lanes")]
pub struct LaneOutOfRange(pub u8);

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn conversation(chat: &str, agent: &str) -> Conversation {
        Conversation::new(SessionId::new(chat).unwrap(), agent)
    }

    fn lane(chat: &str, agent: &str, number: u8) -> Lane {
        Lane::new(conversation(chat, agent), LaneNumber::new(number).unwrap())
    }

    #[test]
    fn a_lane_id_is_the_chat_the_agent_and_the_number() {
        assert_eq!(
            lane("chat-1", "math", 0).session_id().as_str(),
            "chat-1:math:0"
        );
        assert_eq!(
            lane("chat-1", "math", 3).session_id().as_str(),
            "chat-1:math:3"
        );
        assert_eq!(
            Lane::home(conversation("chat-1", "math"))
                .session_id()
                .as_str(),
            "chat-1:math:0"
        );
    }

    #[test]
    fn a_percent_or_colon_in_the_agent_id_is_escaped() {
        assert_eq!(lane("c", "a:b", 0).session_id().as_str(), "c:a%3Ab:0");
        assert_eq!(lane("c", "50%", 1).session_id().as_str(), "c:50%25:1");
        assert_eq!(lane("c", "a%3Ab", 0).session_id().as_str(), "c:a%253Ab:0");
    }

    /// Unescaped, (`a:b`, `c`) and (`a`, `b:c`) would both be `a:b:c:0`.
    #[test]
    fn a_colon_moved_between_chat_and_agent_gives_another_id() {
        assert_ne!(
            lane("a:b", "c", 0).session_id(),
            lane("a", "b:c", 0).session_id()
        );
    }

    #[test]
    fn a_chat_id_shaped_like_a_lane_id_gets_lanes_of_its_own() {
        let shaped = conversation("c:math:0", "math");
        let plain = conversation("c", "math");
        let ids: HashSet<SessionId> = shaped
            .lanes()
            .chain(plain.lanes())
            .map(|lane| lane.session_id())
            .collect();
        assert_eq!(ids.len(), 2 * LANES_PER_CONVERSATION);
        assert!(
            shaped
                .lanes()
                .all(|lane| lane.session_id().as_str() != "c:math:0"),
            "no lane of the look-alike chat is lane 0 of `c`"
        );
    }

    /// The chat and agent ids below contain `:`, `%`, or text that looks
    /// like an escape. No two of their lanes get the same id.
    #[test]
    fn no_two_lanes_share_an_id() {
        let chats = ["c", "c:", ":c", "a:b", "a", "c:math:0", "%", "%3A", "a%3Ab"];
        let agents = [
            "math", "a:b", "b:c", "b", "%", "%3A", "%25", ":", "", "a%3Ab",
        ];
        let mut ids = HashSet::new();
        let mut lanes = 0;
        for chat in chats {
            for agent in agents {
                for lane in conversation(chat, agent).lanes() {
                    ids.insert(lane.session_id());
                    lanes += 1;
                }
            }
        }
        assert_eq!(lanes, chats.len() * agents.len() * LANES_PER_CONVERSATION);
        assert_eq!(ids.len(), lanes);
    }

    #[test]
    fn a_lane_number_stops_below_the_cap() {
        let cap = u8::try_from(LANES_PER_CONVERSATION).unwrap();
        assert_eq!(LaneNumber::new(cap), Err(LaneOutOfRange(cap)));
        let last = LaneNumber::new(cap - 1).unwrap();
        assert_eq!(last.next(), None);
        assert_eq!(LaneNumber::HOME.get(), 0);
        assert_eq!(
            LaneNumber::all().map(LaneNumber::get).collect::<Vec<_>>(),
            (0..cap).collect::<Vec<_>>()
        );
        assert!(serde_json::from_str::<LaneNumber>(&cap.to_string()).is_err());
        assert_eq!(serde_json::from_str::<LaneNumber>("2").unwrap().get(), 2);
    }
}

//! Slack ingress: answers @mentions and direct messages over Socket Mode.

mod api;
mod events;
mod runner;
mod search;
mod socket_mode;

pub use api::{
    ActionToken, AppToken, BotIdentity, BotToken, MAX_SEARCH_HITS, SearchHit, SearchPage, SlackApi,
    SlackApiError, SlackMessage, TokenError,
};
pub use events::{
    AssistantThread, DisconnectReason, Event, EventCallback, Frame, Inbound, MessageEvent,
    SeenMessages, accept, parse_frame, strip_mentions,
};
pub use runner::{SlackStartError, start};
pub use socket_mode::{Disconnected, SocketModeError, run as run_socket_mode, serve_connection};

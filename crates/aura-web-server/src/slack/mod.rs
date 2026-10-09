//! Slack ingress: answers @mentions and direct messages over Socket Mode.

mod api;
mod budget;
mod events;
mod post;
mod runner;
mod search;
mod socket_mode;

pub use api::{
    ActionToken, AppToken, BotIdentity, BotToken, MAX_SEARCH_HITS, Posted, SearchHit, SearchPage,
    SlackApi, SlackApiError, SlackMessage, TokenError,
};
pub use events::{
    AssistantThread, DisconnectReason, Event, EventCallback, Frame, Inbound, MessageEvent,
    SeenMessages, accept, parse_frame, strip_mentions,
};
pub use post::{POST_TOOL_NAME, SlackPostTool, run_tools};
pub use runner::{SlackStartError, start};
pub use socket_mode::{Disconnected, SocketModeError, run as run_socket_mode, serve_connection};

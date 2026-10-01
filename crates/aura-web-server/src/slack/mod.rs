//! Slack ingress: answers @mentions and direct messages over Socket Mode.

mod api;
mod events;
mod runner;
mod socket_mode;

pub use api::{AppToken, BotIdentity, BotToken, SlackApi, SlackApiError, SlackMessage, TokenError};
pub use events::{
    DisconnectReason, Event, EventCallback, Frame, Inbound, MessageEvent, SeenMessages, accept,
    parse_frame, strip_mentions,
};
pub use runner::{SlackStartError, start};
pub use socket_mode::{Disconnected, SocketModeError, run as run_socket_mode, serve_connection};

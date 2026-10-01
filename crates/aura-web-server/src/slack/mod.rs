//! Slack ingress: answers @mentions and direct messages over Socket Mode.

mod api;
mod events;

pub use api::{AppToken, BotIdentity, BotToken, SlackApi, SlackApiError, SlackMessage, TokenError};
pub use events::{Event, Frame, Inbound, SeenMessages, accept, parse_frame, strip_mentions};

//! The id of one inbound request, which the run serving it carries.

use std::fmt;
use std::sync::Arc;

use uuid::Uuid;

const COMPLETION_PREFIX: &str = "req_";
const A2A_PREFIX: &str = "a2a_";
const SLACK_PREFIX: &str = "slack_";

/// The identity of one inbound request.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RequestId(Arc<str>);

impl RequestId {
    /// Mint a fresh id for a chat completion request.
    #[must_use]
    pub fn generate() -> Self {
        Self(format!("{COMPLETION_PREFIX}{}", Uuid::new_v4().simple()).into())
    }

    /// The id of the request serving A2A task `task_id`. The same task id
    /// always yields the same request id.
    #[must_use]
    pub fn for_a2a_task(task_id: &str) -> Self {
        Self(format!("{A2A_PREFIX}{task_id}").into())
    }

    /// The id of the request answering the Slack message posted at `ts` in
    /// `channel`.
    #[must_use]
    pub fn for_slack_message(channel: &str, ts: &str) -> Self {
        Self(format!("{SLACK_PREFIX}{channel}_{ts}").into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Accepts only the shapes the minting constructors produce.
    pub(crate) fn parse(s: &str) -> Option<Self> {
        let minted = if let Some(uuid) = s.strip_prefix(COMPLETION_PREFIX) {
            uuid.len() == 32 && uuid.bytes().all(|b| b.is_ascii_hexdigit())
        } else if let Some(message) = s.strip_prefix(SLACK_PREFIX) {
            message.contains('_')
        } else {
            s.starts_with(A2A_PREFIX)
        };
        minted.then(|| Self(s.into()))
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_ids_parse_back() {
        for id in [
            RequestId::generate(),
            RequestId::for_a2a_task("task-7"),
            RequestId::for_a2a_task(""),
            RequestId::for_a2a_task("task_with_underscores"),
            RequestId::for_slack_message("C0123ABCD", "1712345678.123456"),
            RequestId::for_slack_message("", ""),
            RequestId::for_slack_message("C_0123", "1712345678_123456"),
        ] {
            assert_eq!(RequestId::parse(id.as_str()), Some(id));
        }
    }

    #[test]
    fn unminted_strings_do_not_parse() {
        for s in [
            "",
            "req-1",
            "req_",
            "req_not-a-uuid",
            "req_0191e8c0111170008000000000000042ff",
            "a2a",
            "slack_",
            "slack_C0123ABCD",
            "run:0191e8c0-1111-7000-8000-000000000042",
        ] {
            assert_eq!(RequestId::parse(s), None, "{s:?} parsed");
        }
    }
}

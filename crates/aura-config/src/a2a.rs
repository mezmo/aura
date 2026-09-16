//! `[a2a]` — remote AURA agents this agent can call over the A2A protocol.
//!
//! ```toml
//! [a2a]
//! poll_interval_secs = 2
//! timeout_secs = 600
//!
//! [a2a.remote.dev]
//! url = "https://aura.dev.example.com"
//! description = "Verifies deployments in the dev cluster"
//! model = "release-verifier"
//! headers = { Authorization = "Bearer {{ env.DEV_AURA_KEY }}" }
//! headers_from_request = { "x-request-id" = "x-request-id" }
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// The tool name through which the model addresses every configured remote.
pub const ASK_AGENT_TOOL_NAME: &str = "ask_agent";

/// Remote agents reachable over A2A, keyed by the name the model uses to
/// address them.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct A2aConfig {
    #[serde(default)]
    pub remote: HashMap<String, A2aRemoteConfig>,
    /// Seconds between task-status polls.
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
    /// Wall-clock budget in seconds for one remote call.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for A2aConfig {
    fn default() -> Self {
        Self {
            remote: HashMap::new(),
            poll_interval_secs: default_poll_interval_secs(),
            timeout_secs: default_timeout_secs(),
        }
    }
}

/// One remote AURA web server.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct A2aRemoteConfig {
    /// Origin of the remote web server, e.g. `https://aura.dev.example.com`.
    pub url: String,
    /// What the remote agent can do, in the model's words.
    #[serde(default)]
    pub description: Option<String>,
    /// Agent name or alias to select on the remote.
    #[serde(default)]
    pub model: Option<String>,
    /// Static request headers, e.g. an API key.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Outbound header name → inbound request header to copy it from.
    #[serde(default)]
    pub headers_from_request: HashMap<String, String>,
    /// Seconds between task-status polls.
    #[serde(default)]
    pub poll_interval_secs: Option<u64>,
    /// Wall-clock budget in seconds for one call.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

fn default_poll_interval_secs() -> u64 {
    2
}

fn default_timeout_secs() -> u64 {
    600
}

impl A2aConfig {
    /// Poll interval for `remote`: its own value, else the `[a2a]` default.
    pub fn poll_interval_secs(&self, remote: &A2aRemoteConfig) -> u64 {
        remote.poll_interval_secs.unwrap_or(self.poll_interval_secs)
    }

    /// Call budget for `remote`: its own value, else the `[a2a]` default.
    pub fn timeout_secs(&self, remote: &A2aRemoteConfig) -> u64 {
        remote.timeout_secs.unwrap_or(self.timeout_secs)
    }

    /// Reject a config the model or the HTTP client could not act on: a
    /// remote name that cannot appear in a tool schema enum, a URL that is
    /// not an absolute `http(s)` origin, a zero budget, or a poll interval
    /// that never fires inside the budget.
    pub fn validate(&self) -> Result<(), crate::ConfigError> {
        let invalid = |msg: String| Err(crate::ConfigError::Validation(msg));

        for (name, remote) in &self.remote {
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                return invalid(format!(
                    "a2a.remote name {name:?} must be non-empty and use only letters, digits, '_' or '-'"
                ));
            }
            let url = remote.url.trim();
            let Some(rest) = url
                .strip_prefix("http://")
                .or_else(|| url.strip_prefix("https://"))
            else {
                return invalid(format!(
                    "a2a.remote.{name}.url must be an absolute http(s) URL, got {:?}",
                    remote.url
                ));
            };
            if rest.split('/').next().unwrap_or_default().is_empty() {
                return invalid(format!(
                    "a2a.remote.{name}.url has no host: {:?}",
                    remote.url
                ));
            }
            for (outbound, inbound) in &remote.headers_from_request {
                if outbound.trim().is_empty() || inbound.trim().is_empty() {
                    return invalid(format!(
                        "a2a.remote.{name}.headers_from_request entries must name both headers, got {outbound:?} = {inbound:?}"
                    ));
                }
            }
            let poll = self.poll_interval_secs(remote);
            let timeout = self.timeout_secs(remote);
            if poll == 0 {
                return invalid(format!(
                    "a2a.remote.{name}: poll_interval_secs must be at least 1"
                ));
            }
            if timeout == 0 {
                return invalid(format!(
                    "a2a.remote.{name}: timeout_secs must be at least 1"
                ));
            }
            if poll >= timeout {
                return invalid(format!(
                    "a2a.remote.{name}: poll_interval_secs ({poll}) must be below timeout_secs ({timeout})"
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_str: &str) -> A2aConfig {
        toml::from_str(toml_str).expect("valid a2a toml")
    }

    #[test]
    fn remotes_parse_with_defaults_and_overrides() {
        let cfg = parse(
            r#"
            [remote.dev]
            url = "http://dev:8080"
            description = "dev cluster"
            model = "verifier"
            headers = { Authorization = "Bearer x" }
            headers_from_request = { "x-request-id" = "x-request-id" }

            [remote.prod]
            url = "https://prod"
            poll_interval_secs = 5
            timeout_secs = 60
            "#,
        );
        assert_eq!(cfg.poll_interval_secs, 2);
        assert_eq!(cfg.timeout_secs, 600);
        let dev = &cfg.remote["dev"];
        assert_eq!(dev.model.as_deref(), Some("verifier"));
        assert_eq!(dev.headers["Authorization"], "Bearer x");
        assert_eq!(cfg.poll_interval_secs(dev), 2);
        assert_eq!(cfg.timeout_secs(dev), 600);
        let prod = &cfg.remote["prod"];
        assert_eq!(cfg.poll_interval_secs(prod), 5);
        assert_eq!(cfg.timeout_secs(prod), 60);
        cfg.validate().expect("valid config");
    }

    #[test]
    fn empty_section_is_valid() {
        let cfg = parse("");
        assert!(cfg.remote.is_empty());
        cfg.validate().expect("empty [a2a] is valid");
    }

    #[test]
    fn rejects_bad_names_urls_and_budgets() {
        let cases = [
            (
                "[remote.\"has space\"]\nurl = \"http://x\"",
                "letters, digits",
            ),
            ("[remote.dev]\nurl = \"dev:8080\"", "absolute http(s) URL"),
            ("[remote.dev]\nurl = \"https://\"", "no host"),
            (
                "[remote.dev]\nurl = \"http://x\"\npoll_interval_secs = 0",
                "poll_interval_secs must be at least 1",
            ),
            (
                "[remote.dev]\nurl = \"http://x\"\ntimeout_secs = 0",
                "timeout_secs must be at least 1",
            ),
            (
                "[remote.dev]\nurl = \"http://x\"\npoll_interval_secs = 30\ntimeout_secs = 30",
                "must be below timeout_secs",
            ),
            (
                "[remote.dev]\nurl = \"http://x\"\nheaders_from_request = { \"\" = \"x\" }",
                "must name both headers",
            ),
        ];
        for (toml_str, expected) in cases {
            let err = parse(toml_str)
                .validate()
                .expect_err(&format!("{toml_str} should fail"));
            assert!(
                err.to_string().contains(expected),
                "{toml_str}: expected {expected:?} in {err}"
            );
        }
    }
}

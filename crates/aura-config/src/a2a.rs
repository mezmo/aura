//! `[a2a]` — remote AURA agents this agent can call over the A2A protocol.
//!
//! ```toml
//! [a2a]
//! poll_interval_secs = 2
//! timeout_secs = 600
//! max_response_bytes = 65536
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

/// Header the remote reads to pick an agent config.
pub const MODEL_HEADER: &str = "x-aura-model";

/// Header carrying the protocol version on every request.
pub const VERSION_HEADER: &str = "a2a-version";

/// Path of the A2A v1.0 JSON-RPC binding under a remote's origin.
pub const JSONRPC_PATH: &str = "/a2a/v1/rpc";

/// Header names an operator may not set through `headers` or map through
/// `headers_from_request`: the client owns the protocol and framing headers,
/// and the agent-selection header is the `model` setting's job, so a mapping
/// cannot hand that choice to whoever sends the inbound request.
pub const RESERVED_HEADERS: [&str; 5] = [
    MODEL_HEADER,
    VERSION_HEADER,
    "host",
    "content-length",
    "content-type",
];

/// `{origin}/a2a/v1/rpc` for an absolute `http(s)` `base_url`. A trailing
/// slash or an already-present binding path is tolerated; credentials in the
/// URL, a query string, and a fragment are refused because the endpoint is
/// echoed into error text the model sees. Shared by config validation and
/// the client so a URL accepted at load never fails at build.
pub fn jsonrpc_endpoint(base_url: &str) -> Result<String, String> {
    let parsed = url::Url::parse(base_url.trim()).map_err(|e| e.to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!(
            "scheme must be http or https, got {:?}",
            parsed.scheme()
        ));
    }
    if parsed.host_str().is_none() {
        return Err("missing host".to_owned());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("credentials in the URL are not allowed; use headers".to_owned());
    }
    if parsed.query().is_some() {
        return Err("a query string is not allowed".to_owned());
    }
    if parsed.fragment().is_some() {
        return Err("a fragment is not allowed".to_owned());
    }
    let base = parsed.as_str().trim_end_matches('/');
    if base.ends_with(JSONRPC_PATH) {
        Ok(base.to_owned())
    } else {
        Ok(format!("{base}{JSONRPC_PATH}"))
    }
}

/// Parse one header for the wire, lowercasing the name. Shared by config
/// validation and the client so a header accepted at load never fails at
/// build.
pub fn parse_header(
    name: &str,
    value: &str,
) -> Result<(http::HeaderName, http::HeaderValue), String> {
    let header_name = http::HeaderName::from_bytes(name.trim().to_ascii_lowercase().as_bytes())
        .map_err(|e| format!("invalid header name {name:?}: {e}"))?;
    let header_value = http::HeaderValue::from_str(value)
        .map_err(|e| format!("invalid value for header {name:?}: {e}"))?;
    Ok((header_name, header_value))
}

fn is_reserved_header(name: &str) -> bool {
    let name = name.trim().to_ascii_lowercase();
    RESERVED_HEADERS.contains(&name.as_str())
}

/// Remote agents reachable over A2A, keyed by the name the model uses to
/// address them.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct A2aConfig {
    #[serde(default)]
    pub remote: HashMap<String, A2aRemoteConfig>,
    /// Seconds between task-status polls.
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
    /// Wall-clock budget in seconds for one remote call.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Cap in bytes on a remote answer's text.
    #[serde(default = "default_max_response_bytes")]
    pub max_response_bytes: usize,
}

impl Default for A2aConfig {
    fn default() -> Self {
        Self {
            remote: HashMap::new(),
            poll_interval_secs: default_poll_interval_secs(),
            timeout_secs: default_timeout_secs(),
            max_response_bytes: default_max_response_bytes(),
        }
    }
}

/// One remote AURA web server.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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

fn default_max_response_bytes() -> usize {
    64 * 1024
}

/// Smallest `max_response_bytes` that leaves room for an answer and the
/// truncation notice.
const MIN_MAX_RESPONSE_BYTES: usize = 1024;

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
    /// remote name that cannot appear in a tool schema enum, a URL or
    /// header the client's own parsers would refuse, a reserved header
    /// name, a zero budget, or a poll interval that never fires inside the
    /// budget.
    pub fn validate(&self) -> Result<(), crate::ConfigError> {
        let invalid = |msg: String| Err(crate::ConfigError::Validation(msg));

        if self.max_response_bytes < MIN_MAX_RESPONSE_BYTES {
            return invalid(format!(
                "a2a.max_response_bytes must be at least {MIN_MAX_RESPONSE_BYTES}, got {}",
                self.max_response_bytes
            ));
        }

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
            if let Err(reason) = jsonrpc_endpoint(&remote.url) {
                return invalid(format!(
                    "a2a.remote.{name}.url must be an absolute http(s) URL, got {:?}: {reason}",
                    remote.url
                ));
            }
            for (header, value) in &remote.headers {
                if is_reserved_header(header) {
                    return invalid(format!(
                        "a2a.remote.{name}.headers: {header:?} is reserved (use `model` to select an agent; the client sets the protocol headers)"
                    ));
                }
                if let Err(reason) = parse_header(header, value) {
                    return invalid(format!("a2a.remote.{name}.headers: {reason}"));
                }
            }
            if let Some(model) = &remote.model
                && let Err(reason) = parse_header(MODEL_HEADER, model)
            {
                return invalid(format!("a2a.remote.{name}.model: {reason}"));
            }
            for (outbound, inbound) in &remote.headers_from_request {
                if inbound.trim().is_empty() {
                    return invalid(format!(
                        "a2a.remote.{name}.headers_from_request: {outbound:?} names no inbound header"
                    ));
                }
                if is_reserved_header(outbound) {
                    return invalid(format!(
                        "a2a.remote.{name}.headers_from_request: {outbound:?} is reserved and cannot be taken from the request"
                    ));
                }
                if let Err(reason) = http::HeaderName::from_bytes(outbound.trim().as_bytes()) {
                    return invalid(format!(
                        "a2a.remote.{name}.headers_from_request: invalid outbound header name {outbound:?}: {reason}"
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
        assert_eq!(cfg.max_response_bytes, 64 * 1024);
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
    fn unknown_keys_are_a_parse_error() {
        for toml_str in [
            "poll_interval_sec = 5",
            "[remote.dev]\nurl = \"http://x\"\ntimeout_sec = 5",
        ] {
            assert!(
                toml::from_str::<A2aConfig>(toml_str).is_err(),
                "{toml_str} should be rejected"
            );
        }
    }

    #[test]
    fn endpoint_is_derived_from_the_origin() {
        assert_eq!(
            jsonrpc_endpoint("http://spoke:8080").unwrap(),
            "http://spoke:8080/a2a/v1/rpc"
        );
        assert_eq!(
            jsonrpc_endpoint("https://spoke.example.com/").unwrap(),
            "https://spoke.example.com/a2a/v1/rpc"
        );
        assert_eq!(
            jsonrpc_endpoint("http://spoke:8080/a2a/v1/rpc").unwrap(),
            "http://spoke:8080/a2a/v1/rpc"
        );
        for bad in [
            "spoke:8080",
            "ftp://spoke",
            "http://",
            "http://bad host",
            "https://host/?api_key=secret",
            "https://host/#frag",
            "https://user:pass@host",
            "https://user@host",
        ] {
            assert!(jsonrpc_endpoint(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn rejects_bad_names_urls_headers_and_budgets() {
        let cases = [
            (
                "[remote.\"has space\"]\nurl = \"http://x\"",
                "letters, digits",
            ),
            ("[remote.dev]\nurl = \"dev:8080\"", "absolute http(s) URL"),
            ("[remote.dev]\nurl = \"https://\"", "absolute http(s) URL"),
            (
                "[remote.dev]\nurl = \"http://bad host\"",
                "absolute http(s) URL",
            ),
            ("[remote.dev]\nurl = \"http://x/?k=v\"", "query string"),
            (
                "[remote.dev]\nurl = \"http://u:p@x\"",
                "credentials in the URL",
            ),
            (
                "[remote.dev]\nurl = \"http://x\"\nheaders = { \"bad header\" = \"v\" }",
                "invalid header name",
            ),
            (
                "[remote.dev]\nurl = \"http://x\"\nheaders = { Authorization = \"line\\nbreak\" }",
                "invalid value for header",
            ),
            (
                "[remote.dev]\nurl = \"http://x\"\nheaders = { X-Aura-Model = \"admin\" }",
                "is reserved",
            ),
            (
                "[remote.dev]\nurl = \"http://x\"\nheaders = { \"A2A-Version\" = \"0.3\" }",
                "is reserved",
            ),
            (
                "[remote.dev]\nurl = \"http://x\"\nheaders = { Host = \"evil\" }",
                "is reserved",
            ),
            (
                "[remote.dev]\nurl = \"http://x\"\nmodel = \"two\\nlines\"",
                "a2a.remote.dev.model",
            ),
            (
                "[remote.dev]\nurl = \"http://x\"\nheaders_from_request = { \"bad name\" = \"x\" }",
                "invalid outbound header name",
            ),
            (
                "[remote.dev]\nurl = \"http://x\"\nheaders_from_request = { \"x-aura-model\" = \"x-model\" }",
                "reserved and cannot be taken from the request",
            ),
            (
                "[remote.dev]\nurl = \"http://x\"\nheaders_from_request = { \"x-out\" = \"\" }",
                "names no inbound header",
            ),
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
                "max_response_bytes = 10\n[remote.dev]\nurl = \"http://x\"",
                "max_response_bytes must be at least",
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

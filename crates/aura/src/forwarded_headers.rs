//! The request headers a prepared agent forwards.
//!
//! `headers_from_request` mappings copy inbound request headers onto the MCP
//! servers and the HITL webhook route when an agent is prepared, and the MCP
//! connections are opened with them. A prepared agent is therefore bound to
//! the request that prepared it as far as those headers go, and can only serve
//! another request that forwards the same values.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use aura_config::{Config, DecisionRouteConfig, McpServerConfig};

/// The inbound request headers `headers_from_request` mappings read, with the
/// values the preparing request carried.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ForwardedHeaders {
    /// Lowercased header name → the request's value for it.
    values: BTreeMap<String, Option<String>>,
}

impl ForwardedHeaders {
    /// The headers `req_headers` forwards under `config`: every inbound name a
    /// `headers_from_request` mapping reads, on an MCP server or the HITL
    /// webhook route, looked up case-insensitively.
    pub fn resolve(config: &Config, req_headers: Option<&HashMap<String, String>>) -> Self {
        let mut names = BTreeSet::new();
        if let Some(mcp) = &config.mcp {
            for server in mcp.servers.values() {
                match server {
                    McpServerConfig::HttpStreamable {
                        headers_from_request,
                        ..
                    }
                    | McpServerConfig::Sse {
                        headers_from_request,
                        ..
                    } => names.extend(headers_from_request.values().map(|n| n.to_lowercase())),
                    McpServerConfig::Stdio { .. } => {}
                }
            }
        }
        if let Some(DecisionRouteConfig::Webhook {
            headers_from_request,
            ..
        }) = config.hitl.as_ref().map(|hitl| &hitl.route)
        {
            names.extend(headers_from_request.values().map(|n| n.to_lowercase()));
        }
        Self::of(names, req_headers)
    }

    /// `req_headers` seen through the names this forwards.
    pub fn project(&self, req_headers: Option<&HashMap<String, String>>) -> Self {
        Self::of(self.values.keys().cloned(), req_headers)
    }

    /// `req_headers` seen through `names`, which are lowercase: each name with
    /// the value the request carries under it in any letter case, or `None`
    /// when the request does not carry it.
    pub(crate) fn of(
        names: impl IntoIterator<Item = String>,
        req_headers: Option<&HashMap<String, String>>,
    ) -> Self {
        let values = names
            .into_iter()
            .map(|name| {
                let value = req_headers.and_then(|headers| {
                    headers
                        .iter()
                        .find(|(k, _)| k.to_lowercase() == name)
                        .map(|(_, v)| v.clone())
                });
                (name, value)
            })
            .collect();
        Self { values }
    }

    /// The forwarded headers as a request carries them, omitting the absent
    /// ones.
    pub fn as_request(&self) -> HashMap<String, String> {
        self.values
            .iter()
            .filter_map(|(name, value)| value.clone().map(|v| (name.clone(), v)))
            .collect()
    }

    /// The first header, by name, whose value differs between `self` and
    /// `req_headers` seen through the same names.
    pub fn first_difference(&self, req_headers: Option<&HashMap<String, String>>) -> Option<&str> {
        let theirs = self.project(req_headers);
        self.values
            .iter()
            .find(|(name, value)| theirs.values.get(*name) != Some(value))
            .map(|(name, _)| name.as_str())
    }
}

/// Names only: the values are credentials.
impl std::fmt::Debug for ForwardedHeaders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut map = f.debug_map();
        for (name, value) in &self.values {
            map.entry(
                name,
                &value.as_ref().map(|_| "<present>").unwrap_or("<absent>"),
            );
        }
        map.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(mcp_mappings: &[(&str, &str)], hitl_mappings: &[(&str, &str)]) -> Config {
        let mut toml = String::from(
            r#"
[agent]
name = "t"
system_prompt = "t"

[agent.llm]
provider = "openai"
model = "gpt-4o"
api_key = "k"
"#,
        );
        if !mcp_mappings.is_empty() {
            toml.push_str("\n[mcp.servers.s]\ntransport = \"http_streamable\"\nurl = \"https://example.com/mcp\"\n[mcp.servers.s.headers_from_request]\n");
            for (out, inbound) in mcp_mappings {
                toml.push_str(&format!("{out} = \"{inbound}\"\n"));
            }
        }
        if !hitl_mappings.is_empty() {
            toml.push_str(
                "\n[hitl]\nrequire_approval = [\"x_*\"]\n[hitl.route]\nmode = \"webhook\"\nurl = \"https://example.com/hook\"\n[hitl.route.headers_from_request]\n",
            );
            for (out, inbound) in hitl_mappings {
                toml.push_str(&format!("{out} = \"{inbound}\"\n"));
            }
        }
        aura_config::load_config_from_str(&toml).expect("config parses")
    }

    fn headers(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn nothing_is_forwarded_without_mappings() {
        let forwarded =
            ForwardedHeaders::resolve(&config(&[], &[]), Some(&headers(&[("x-user", "a")])));
        assert_eq!(forwarded, ForwardedHeaders::default());
        assert_eq!(
            forwarded.first_difference(Some(&headers(&[("x-user", "b")]))),
            None
        );
    }

    #[test]
    fn every_mapping_source_contributes_its_inbound_names() {
        let config = config(
            &[("authorization", "X-User-Token")],
            &[("x-approver", "x-actor")],
        );
        let forwarded =
            ForwardedHeaders::resolve(&config, Some(&headers(&[("x-user-token", "t1")])));

        assert_eq!(
            forwarded.as_request(),
            headers(&[("x-user-token", "t1")]),
            "the MCP mapping's inbound name is read case-insensitively; the absent HITL one is omitted",
        );
        assert_eq!(
            format!("{forwarded:?}"),
            r#"{"x-actor": "<absent>", "x-user-token": "<present>"}"#,
            "debug output names the headers and never their values",
        );
    }

    #[test]
    fn a_request_forwarding_the_same_values_matches() {
        let config = config(&[("authorization", "x-user-token")], &[]);
        let forwarded =
            ForwardedHeaders::resolve(&config, Some(&headers(&[("X-User-Token", "t1")])));

        assert_eq!(
            forwarded.first_difference(Some(&headers(&[("x-user-token", "t1")]))),
            None
        );
        assert_eq!(
            forwarded.first_difference(Some(&headers(&[
                ("x-user-token", "t1"),
                ("x-other", "ignored")
            ]))),
            None,
            "headers no mapping reads do not matter",
        );
    }

    #[test]
    fn a_request_forwarding_a_different_value_differs() {
        let config = config(&[("authorization", "x-user-token")], &[]);
        let forwarded =
            ForwardedHeaders::resolve(&config, Some(&headers(&[("x-user-token", "t1")])));

        assert_eq!(
            forwarded.first_difference(Some(&headers(&[("x-user-token", "t2")]))),
            Some("x-user-token"),
        );
        assert_eq!(
            forwarded.first_difference(None),
            Some("x-user-token"),
            "a request without the header the agent forwarded differs too",
        );
    }

    #[test]
    fn an_agent_prepared_without_the_header_differs_from_a_request_carrying_it() {
        let config = config(&[("authorization", "x-user-token")], &[]);
        let forwarded = ForwardedHeaders::resolve(&config, None);

        assert_eq!(forwarded.first_difference(None), None);
        assert_eq!(
            forwarded.first_difference(Some(&headers(&[("x-user-token", "t1")]))),
            Some("x-user-token"),
            "the static fallback the agent connected with is not this request's token",
        );
    }
}

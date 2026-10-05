//! Output formatter configuration types.
//!
//! Parsed from `[mcp.servers.<name>.output_format]`. The runtime formatters
//! that use them live in the `aura` crate's `output_format` module.

use serde::{Deserialize, Serialize};

/// A tool's output formatter and its options: one value of
/// `[mcp.servers.<name>.output_format]`, keyed by tool-name glob pattern.
///
/// ```toml
/// [mcp.servers.metrics.output_format]
/// "*query*" = { formatter = "prometheus", step_arg = "step" }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "formatter", rename_all = "snake_case")]
pub enum OutputFormatEntry {
    /// Prometheus HTTP API query results (`vector`, `matrix`, `scalar`, `string`).
    Prometheus(PrometheusFormatOptions),
}

impl OutputFormatEntry {
    /// The formatter name as written in config.
    pub fn name(&self) -> &'static str {
        match self {
            OutputFormatEntry::Prometheus(_) => "prometheus",
        }
    }
}

/// Options for the `prometheus` formatter.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PrometheusFormatOptions {
    /// Name of the tool argument holding the requested query step (seconds
    /// or a Prometheus duration such as `30s`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_arg: Option<String>,
    /// Name of the tool argument holding the range start (RFC 3339 or Unix
    /// seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_arg: Option<String>,
    /// Name of the tool argument holding the range end (RFC 3339 or Unix
    /// seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_arg: Option<String>,
    /// Labels to print on every series.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keep_labels: Vec<String>,
    /// Glob patterns (`*` and `?`) for labels not to print.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drop_labels: Vec<String>,
    /// Maximum number of series printed.
    #[serde(default = "default_max_series")]
    pub max_series: usize,
    /// Maximum number of run-length entries printed per series.
    #[serde(default = "default_max_runs_per_series")]
    pub max_runs_per_series: usize,
}

impl Default for PrometheusFormatOptions {
    fn default() -> Self {
        Self {
            step_arg: None,
            start_arg: None,
            end_arg: None,
            keep_labels: Vec::new(),
            drop_labels: Vec::new(),
            max_series: default_max_series(),
            max_runs_per_series: default_max_runs_per_series(),
        }
    }
}

fn default_max_series() -> usize {
    50
}

fn default_max_runs_per_series() -> usize {
    30
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn parse(toml_src: &str) -> Result<HashMap<String, OutputFormatEntry>, toml::de::Error> {
        toml::from_str(toml_src)
    }

    #[test]
    fn prometheus_entry_uses_defaults() {
        let map = parse(r#""*query*" = { formatter = "prometheus" }"#).unwrap();
        assert_eq!(
            map["*query*"],
            OutputFormatEntry::Prometheus(PrometheusFormatOptions::default())
        );
    }

    #[test]
    fn prometheus_entry_reads_options() {
        let map = parse(
            r#""execute_range_query" = { formatter = "prometheus", step_arg = "step", start_arg = "start", end_arg = "end", keep_labels = ["instance"], drop_labels = ["agent_*"], max_series = 10, max_runs_per_series = 5 }"#,
        )
        .unwrap();
        let OutputFormatEntry::Prometheus(opts) = &map["execute_range_query"];
        assert_eq!(opts.step_arg.as_deref(), Some("step"));
        assert_eq!(opts.start_arg.as_deref(), Some("start"));
        assert_eq!(opts.end_arg.as_deref(), Some("end"));
        assert_eq!(opts.keep_labels, vec!["instance"]);
        assert_eq!(opts.drop_labels, vec!["agent_*"]);
        assert_eq!(opts.max_series, 10);
        assert_eq!(opts.max_runs_per_series, 5);
    }

    #[test]
    fn unknown_formatter_is_rejected() {
        assert!(parse(r#""*" = { formatter = "jq" }"#).is_err());
    }

    #[test]
    fn missing_formatter_is_rejected() {
        assert!(parse(r#""*" = { step_arg = "step" }"#).is_err());
    }

    #[test]
    fn unknown_option_is_rejected() {
        assert!(parse(r#""*" = { formatter = "prometheus", stepp_arg = "step" }"#).is_err());
    }

    #[test]
    fn round_trips_through_toml() {
        let entry = OutputFormatEntry::Prometheus(PrometheusFormatOptions {
            step_arg: Some("step".into()),
            keep_labels: vec!["instance".into()],
            ..Default::default()
        });
        let map = HashMap::from([("*".to_string(), entry.clone())]);
        let text = toml::to_string(&map).unwrap();
        assert_eq!(parse(&text).unwrap()["*"], entry);
    }
}

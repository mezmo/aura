<!-- markdownlint-disable MD033 -->
# Format structured MCP tool output before it reaches the model

- Status: **proposed**
- Deciders: pending review
- Date: 2026-10-05

Technical Story: per-server, per-tool output formatters, starting with the Prometheus
HTTP API query result format.

## Context and Problem Statement

Aura passes MCP tool output to the model unchanged. When the output is at least a tool's
`min_tokens` (`[mcp.servers.<name>.scratchpad]`), `ScratchpadWrapper` stores it and the model
explores it with the scratchpad tools (`head`, `slice`, `grep`, `schema`, `item_schema`,
`get_in`, `iterate_over`, `read`).

Prometheus MCP servers return query results in the
[Prometheus HTTP API format](https://prometheus.io/docs/prometheus/latest/querying/api/#expression-query-result-formats)
(`vector`, `matrix`, `scalar`, `string`). Prometheus, Thanos, Mimir, Cortex and
VictoriaMetrics, and hosted services that expose a Prometheus-compatible API, return it.
Three properties of this format interact badly with an agent:

1. **Labels dominate the size.** Every series repeats its full label set. A backend that
   attaches many target or infrastructure labels returns tens of labels per series, most with
   the same value on every series. A small range query (a few series, a few dozen points)
   exceeds a typical `min_tokens` and is diverted to the scratchpad.
2. **Gaps are absences.** A missed scrape is a missing `[timestamp, value]` pair, not a `0`.
   The scratchpad tools search and slice text; none of them can find a missing element, count
   points per series, or compare timestamps. An agent looking for drops with `grep` finds
   nothing and can report that the series is complete.
3. **Resolution can differ from the request.** Query frontends, downsampled long-term
   storage and hosted backends can return a coarser step than the `step` requested, for
   example 1-hour points for a 30-day range requested at 5 minutes. Averaging inside a coarse
   step hides short drops. Nothing in the response says the resolution changed; it is visible
   only in the timestamp spacing.

The computation needed (labels common to every series, points per series, missing
timestamps, returned resolution) is cheap and deterministic in code, and unreliable for a
model working through text tools under a turn limit.

## Decision Drivers

- The feature MUST be off unless configured; existing configurations MUST behave identically.
- It MUST NOT assume a particular backend, label schema or MCP server implementation. Anything
  server-specific (argument names, labels to keep) MUST be configuration.
- It MUST NOT fail a tool call. Output the formatter does not recognise passes through
  unchanged.
- The model MUST be able to get the unformatted response.
- It SHOULD reuse the existing per-server, glob-on-tool-name configuration model and the
  `ToolWrapper` extension point rather than add a parallel mechanism.
- It SHOULD admit further formats (other query APIs) without changes to the builder.

## Considered Options

| Option | Outcome |
|---|---|
| Built-in typed formatters, configured per MCP server, applied by a `ToolWrapper` | Chosen |
| Prompt or skill guidance only (aggregate in PromQL, use `count_over_time`) | Rejected as the sole fix; depends on model compliance and does not reveal a changed resolution |
| Generic transform expressions (jq / JMESPath) per tool | Rejected; can remove labels but cannot express gap or resolution analysis, and every deployment writes its own expressions |
| A proxy MCP server in front of each Prometheus MCP server | Rejected; an extra service per deployment, reimplemented by each user, with no access to the agent's scratchpad budget |
| Change the Prometheus MCP servers | Rejected; several independent implementations, outside aura's control |

## Decision Outcome

Chosen option: **add per-tool output formatters**, configured on each MCP server, applied
by a `ToolWrapper` that runs on the raw tool output before scratchpad interception. The first
formatter is `prometheus`.

### Configuration

A new `output_format` table on all three `McpServerConfig` variants (`stdio`,
`http_streamable`, `sse`), keyed by tool-name glob like `scratchpad`:

```toml
[mcp.servers.metrics.output_format]
"*query*" = { formatter = "prometheus" }
"execute_range_query" = { formatter = "prometheus", step_arg = "step", keep_labels = ["instance"] }
```

- Each entry is a serde enum tagged by `formatter`, with `deny_unknown_fields`. Options are
  typed and validated per formatter at config load; an unknown formatter or option is a
  config error.
- The longest matching pattern on the server wins, as in `scratchpad_tool_map`.
- The wrapper looks up the entry by `(ToolCallContext::tool_namespace, tool_name)`. The
  builder sets `tool_namespace` to the MCP server name for every MCP tool it wraps, so two
  servers that expose the same tool name are configured independently.
- Orchestration workers inherit server configuration; there is no per-worker override in this
  change.

### Wrapper

`OutputFormatWrapper` implements `ToolWrapper`:

- `wrap_schema` adds an optional boolean `_aura_raw` to tools that have a formatter, following
  the `_aura_reasoning` convention.
- `transform_args` removes `_aura_raw`, and reads the request arguments the formatter needs
  (`step_arg`, `start_arg`, `end_arg`) into the extracted data. The arguments sent to the
  server are otherwise unchanged.
- `transform_output` returns the output unchanged when `_aura_raw` was set, the call
  failed, or the formatter does not recognise the output. Otherwise it returns the formatted
  output.

The builder composes it so that its `transform_output` runs before `ScratchpadWrapper`'s,
for single agents and orchestration workers. Formatted output that still reaches
`min_tokens` is intercepted by the scratchpad as today.

### `prometheus` formatter

Input: the `data` object (`resultType`, `result`), the full `{status, data}` envelope, or
either encoded as a JSON string. A `status` other than `success`, an unknown `resultType`, or
any parse failure passes the output through.

Output: plain text, starting with a header that names the formatter and the `_aura_raw`
argument.

- **Labels.** Labels with the same value on every series are printed once as
  `common labels`. Labels that vary are printed per series. `keep_labels` are always printed
  per series; `drop_labels` (globs) are never printed.
- **`vector`.** One row per series: varying labels, value, timestamp.
- **`matrix`.** One row per series: varying labels, number of points, missing timestamps,
  min, max, last value, and count of `NaN` / `±Inf`. Values are run-length encoded
  (`1×28 0×1 1×2`), truncated at `max_points_per_series` with the remainder summarised.
- **Resolution.** The returned resolution is the most common spacing between consecutive
  timestamps across all series. When `step_arg` is configured and the returned resolution
  is coarser than the requested step, the header states both.
- **Missing points.** The expected grid runs from `start` to `end` (when `start_arg` and
  `end_arg` are configured) or from the earliest to the latest timestamp in the response, at
  the returned resolution. Missing timestamps are listed per series, collapsed into ranges.
- **Size.** At most `max_series` series are printed, ordered by number of missing points,
  then by label values. The header states how many were omitted.
- `scalar` and `string` results are printed as a single line.

### Observability

The wrapper records the formatter name and the input and output token counts on the tool
call's tracing span, so the effect can be measured per tool.

## Consequences

- Good: Prometheus query results reach the model at a fraction of their size, usually below
  the scratchpad threshold, with gaps and resolution changes stated explicitly.
- Good: configured per server and per tool with existing patterns; no change for
  configurations that do not use it.
- Good: a new format is a new enum variant and an implementation of the formatter trait.
- Bad: aura takes on knowledge of a third-party response format and must track changes to it.
- Bad: label values that are printed only under `common labels`, or dropped by `drop_labels`,
  are not shown per series; the model needs `_aura_raw` to see the original structure.
- Bad: one more configuration surface on `McpServerConfig`.

## Implementation

One pull request adds the `output_format` configuration types, `OutputFormatWrapper`, its
builder composition, and the `prometheus` formatter, with unit tests from synthetic
fixtures: each `resultType`, the envelope and JSON-string encodings, a changed resolution,
gaps in one series and in all series, `NaN` / `±Inf`, an empty result, and pass-through of
unrecognised output and of `_aura_raw`. Documentation and an example configuration use a
plain Prometheus server and the `up` metric.

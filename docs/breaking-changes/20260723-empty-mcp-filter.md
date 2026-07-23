# 23 July 2026

# !!BREAKING CHANGES!!

## Summary

An **empty** `mcp_filter` (`mcp_filter = []`) now grants **no MCP tools**
instead of every MCP tool. This applies to orchestration workers
(`[orchestration.worker.<name>].mcp_filter`) and to the single-agent
`[agent].mcp_filter`.

An *omitted* `mcp_filter` still grants every MCP tool, so existing configs
that don't write the field are unaffected.

## Why

Previously `mcp_filter = []` and an omitted filter both meant "all tools",
which made two things impossible or dangerous:

- **"No MCP tools" was unexpressible.** Tool-free workers (like a writer
  that only synthesizes text) had to resort to a non-matching dummy pattern
  such as `["__none__"]`.
- **Removing the last allowlist entry silently widened access.** Editing
  `mcp_filter = ["logs_*"]` down to `mcp_filter = []` expanded the worker
  from one tool family to *every* tool — the opposite of the intent.

See issue [#378](https://github.com/mezmo/aura/issues/378).

## The new rule

| Config                        | Tools granted             |
| ----------------------------- | ------------------------- |
| `mcp_filter` omitted          | every MCP tool            |
| `mcp_filter = []`             | none                      |
| `mcp_filter = ["mezmo_*"]`    | matching tools only       |

## Migration

- If a config has `mcp_filter = []` **and relied on receiving all tools**:
  delete the line (omit the field).
- If a config used a non-matching dummy pattern (e.g. `["__none__"]`) to
  keep a worker tool-free: replace it with `mcp_filter = []`.

`client_tool_filter` is unchanged: an empty client-tool filter still means
all client tools.

# Scratchpad Tools Reference

Scratchpad intercepts large MCP tool outputs and writes them to disk so the LLM can explore them selectively instead of loading the entire payload into context. When a tool output exceeds the configured token threshold, Aura saves it to a scratchpad file and returns a pointer. The agent then navigates the content using eight read-only exploration tools.

## Tool Overview

| Tool | Purpose | Best For |
|------|---------|----------|
| `head` | Read first N lines | Previewing file structure |
| `slice` | Extract line range | Reading specific sections |
| `grep` | Regex search with context | Finding specific content |
| `schema` | Show structure with line ranges | Understanding file organization |
| `item_schema` | Show keys across array items | Discovering array item fields |
| `get_in` | Extract value at JSON path | Accessing specific nested data |
| `iterate_over` | Extract fields from array items | Projecting fields across arrays |
| `read` | Read entire file | Last resort for small files |

## head

Read the first N lines of a scratchpad file.

**Parameters:**

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `file` | string | Yes | — | Scratchpad filename (for example, `call_abc123.json`) |
| `lines` | integer | No | 50 | Number of lines to read |

**Example:**

```json
{
  "file": "call_abc123.json",
  "lines": 100
}
```

Use `head` to preview large outputs before deciding what to extract. Start with a small number of lines and increase if needed.

## slice

Extract a range of lines from a scratchpad file.

**Parameters:**

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `file` | string | Yes | — | Scratchpad filename |
| `start` | integer | Yes | — | Start line number (1-indexed) |
| `end` | integer | Yes | — | End line number (1-indexed, inclusive) |

**Example:**

```json
{
  "file": "call_abc123.json",
  "start": 50,
  "end": 100
}
```

Use `slice` after `grep` or `schema` has identified the line range you need.

## grep

Search a scratchpad file with a regex pattern. Returns matching lines with surrounding context.

**Parameters:**

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `file` | string | Yes | — | Scratchpad filename |
| `pattern` | string | Yes | — | Regex pattern to search for |
| `context` | integer | No | 3 | Number of context lines before and after each match |

**Example:**

```json
{
  "file": "call_abc123.json",
  "pattern": "error|failed",
  "context": 5
}
```

Use `grep` to locate specific content without reading the entire file. Reduce the `context` parameter if matches return too much data.

## schema

Show the structure of a scratchpad file with line ranges. Works on JSON (keys, types, arrays) and Markdown (sections, keys).

**Parameters:**

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `file` | string | Yes | — | Scratchpad filename (`.json` or `.md`) |
| `max_depth` | integer | No | 4 | Maximum depth to show |

**Example:**

```json
{
  "file": "call_abc123.json",
  "max_depth": 3
}
```

Use `schema` as the first step when exploring a new scratchpad file. The output shows line ranges you can pass to `slice` or key paths you can pass to `get_in`.

## item_schema

Show all unique keys across items in a JSON array, with types and presence counts. Supports pagination for large arrays.

**Parameters:**

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `file` | string | Yes | — | Scratchpad filename (must be JSON) |
| `path` | string | Yes | — | Dot-separated path to the array (for example, `results` or `data.items`) |
| `offset` | integer | No | 0 | Index of the first item to scan (0-indexed) |
| `limit` | integer | No | all remaining | Maximum items to scan starting at offset |

**Example without pagination:**

```json
{
  "file": "call_abc123.json",
  "path": "results"
}
```

**Example with pagination:**

```json
{
  "file": "call_abc123.json",
  "path": "results",
  "offset": 0,
  "limit": 100
}
```

### Pagination Behavior

When `offset` or `limit` is provided, the output header shows the window being scanned:

```
Item schema for $.results (window: items [0..100) of 500 total):
```

Presence counts are relative to the window. `50/100 items in window` means 50 of the 100 scanned items contain that key.

If `offset` exceeds the array length, an empty window is returned with the total still reported. This lets the agent correct course deterministically.

### Budget Errors

When output exceeds the context budget, `item_schema` returns a structured error with suggestions:

- Paginate with a smaller window
- Spot-check item shapes with `get_in` (for example, `get_in file="..." path="results.0"`)
- Project specific fields with `iterate_over` once the schema is known

**When to use:** Call `item_schema` before `iterate_over` to discover available fields in array items. For heterogeneous arrays where items have different keys, pagination lets you sample different sections to understand the full schema.

## get_in

Extract a value at a nested key path from a JSON scratchpad file.

**Parameters:**

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `file` | string | Yes | — | Scratchpad filename (must be JSON) |
| `path` | string | Yes | — | Dot-separated key path (for example, `results.0.name`) |
| `offset` | integer | No | — | Line offset (0-indexed) for paginating large string values |
| `limit` | integer | No | 100 | Maximum lines to return when paginating |

**Example:**

```json
{
  "file": "call_abc123.json",
  "path": "data.items.0.metadata"
}
```

**Example with pagination (for large string values):**

```json
{
  "file": "call_abc123.json",
  "path": "content.body",
  "offset": 0,
  "limit": 50
}
```

Call `schema` first to see actual top-level keys. Do not guess paths from domain expectations. For example, an RCA tool may expose a single `kv_markdown` string rather than a `root_cause_analysis` object.

### Companion Files

When a scratchpad file contains a large structured string value (embedded JSON or markdown), Aura extracts it to a companion file. If the interception message mentions a companion file, use that file directly instead of navigating through the parent JSON.

## iterate_over

Iterate over items in a JSON array and extract selected fields from each.

**Parameters:**

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `file` | string | Yes | — | Scratchpad filename (must be JSON) |
| `path` | string | Yes | — | Dot-separated path to the array (for example, `results` or `data.items`) |
| `fields` | string | Yes | — | Comma-separated field names to extract (for example, `id,title,metadata.score`) |

**Example:**

```json
{
  "file": "call_abc123.json",
  "path": "results",
  "fields": "id,name,status"
}
```

Fields can use dot-notation for nested access. The output includes an `_index` field showing each item's position in the array.

Use `item_schema` first to discover available fields, then `iterate_over` to extract the specific fields you need.

## read

Read an entire scratchpad file.

**Parameters:**

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `file` | string | Yes | — | Scratchpad filename |

**Example:**

```json
{
  "file": "call_abc123.json"
}
```

This tool may return a large amount of data. Prefer `head`, `slice`, `grep`, or `get_in` for targeted reading. Use `read` only when other tools cannot meet your needs and the file is small enough.

## Budget Behavior

Each exploration tool checks its output against the context budget before returning. When a result would exceed the budget:

1. The tool returns a structured JSON error (not an exception)
2. The error includes the error code, estimated token cost, and suggestions
3. The agent sees this as a successful tool result and can retry with smaller parameters

Each retry consumes a turn, which is why Aura increases `turn_depth` when scratchpad is active (`turn_depth_bonus` config option).

## Workflow Example

A typical exploration workflow:

1. **Preview the structure:** Call `schema` to see top-level keys and line ranges
2. **For arrays:** Call `item_schema` to discover fields across array items (use pagination for large arrays)
3. **Extract data:** Use `get_in` for specific values, `iterate_over` for field projection, or `slice` for line ranges
4. **Search:** Use `grep` when you know what text to find but not where it is

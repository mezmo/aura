# 21 April 2026

# !!BREAKING CHANGES!!

## Summary

The `[llm]` TOML section has been moved from the top level to `[agent.llm]`. LLM configuration is now a property of the agent, not a sibling of it. This unlocks **per-worker LLM overrides** in orchestration mode: each `[orchestration.worker.<name>]` may declare its own `[orchestration.worker.<name>.llm]` to run a different model (or a different provider) than the coordinator, inheriting `[agent.llm]` when omitted.

**NOTE: CONFIGS THAT HAVE NOT BEEN UPDATED WILL FAIL TO PARSE AND THE APP WILL FAIL TO START.** The loader detects a top-level `[llm]` table and emits a migration error pointing to this document. See [Startup Errors](#startup-errors).

---

## What Moved

| Field                     | Old location      | New location                      |
| ------------------------- | ----------------- | --------------------------------- |
| The entire `[llm]` table  | top-level `[llm]` | `[agent.llm]`                     |
| `[llm.additional_params]` | top-level         | `[agent.llm.additional_params]`   |
| `context_window`          | `[llm]`           | `[agent.llm]` (follows the table) |

No fields were renamed — every provider-specific field inside `[llm]` keeps its name under `[agent.llm]`.

---

## Before / After Examples

### Minimal single-agent config

```toml
# BEFORE
[llm]
provider = "openai"
api_key = "{{ env.OPENAI_API_KEY }}"
model = "gpt-5.1"
context_window = 128000
temperature = 0.3

[agent]
name = "My Agent"
system_prompt = "..."
turn_depth = 5

# AFTER
[agent]
name = "My Agent"
system_prompt = "..."
turn_depth = 5

[agent.llm]
provider = "openai"
api_key = "{{ env.OPENAI_API_KEY }}"
model = "gpt-5.1"
context_window = 128000
temperature = 0.3
```

### `additional_params` nested tables

```toml
# BEFORE
[llm]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
temperature = 1.0

[llm.additional_params.thinking]
type = "enabled"
budget_tokens = 8000

# AFTER
[agent]
name = "Thinking Agent"
system_prompt = "..."

[agent.llm]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
temperature = 1.0

[agent.llm.additional_params.thinking]
type = "enabled"
budget_tokens = 8000
```

### Ollama `additional_params`

```toml
# BEFORE
[llm]
provider = "ollama"
model = "qwen3:30b-a3b"
fallback_tool_parsing = true

[llm.additional_params]
num_ctx = 32000
think = true

# AFTER
[agent]
name = "Local Assistant"
system_prompt = "..."

[agent.llm]
provider = "ollama"
model = "qwen3:30b-a3b"
fallback_tool_parsing = true

[agent.llm.additional_params]
num_ctx = 32000
think = true
```

---

## New Capability: Per-Worker LLM Overrides

Workers now accept an optional `[orchestration.worker.<name>.llm]` table. When omitted, the worker inherits `[agent.llm]` (including `context_window`). When present, the worker uses its own LLM configuration exclusively.

```toml
[agent]
name = "Math Coordinator"
system_prompt = "..."

[agent.llm]
provider = "openai"
api_key = "{{ env.OPENAI_API_KEY }}"
model = "gpt-5.1"
context_window = 128000

[orchestration]
enabled = true

# Inherits [agent.llm] — no override needed for the common case
[orchestration.worker.arithmetic]
description = "Basic arithmetic operations"
preamble = "You are an arithmetic specialist."
mcp_filter = ["add", "subtract", "multiply", "divide"]

# Explicit override — this worker runs a cheaper model with a smaller context
[orchestration.worker.formatting]
description = "Formats numeric output for humans"
preamble = "You format numbers as strings."
mcp_filter = []

[orchestration.worker.formatting.llm]
provider = "anthropic"
api_key = "{{ env.ANTHROPIC_API_KEY }}"
model = "claude-haiku-4-5-20251001"
context_window = 200000
```

The worker's resolved `context_window` is what the runtime reports in `aura.session_info` events for that worker and what downstream context-budget work (LOG-23439) will use to size per-worker scratchpads.

---

## Startup Errors

### Top-level `[llm]` is no longer accepted

The loader performs a pre-parse check before deserialization. If it sees a top-level `[llm]` table it fails with:

```
Configuration uses the legacy top-level [llm] table. Move it under
[agent.llm] (and any [llm.additional_params] under
[agent.llm.additional_params]). Workers may optionally override the
LLM via [orchestration.worker.<name>.llm].
```

### `deny_unknown_fields` remains

`aura_config::config::AgentConfig` and `aura::config::LlmConfig` still carry `#[serde(deny_unknown_fields)]`. Any stray field in `[agent]` or `[agent.llm]` (including the fields that were moved out of `[agent]` in the 10 April 2026 migration) still produces a hard parse error.

### Worker LLM fields

`[orchestration.worker.<name>.llm]` accepts the same fields as `[agent.llm]` (the full `LlmConfig` variant set per provider). The same `deny_unknown_fields` rules apply.

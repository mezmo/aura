# 10 April 2026
# !!BREAKING CHANGES!!

## Summary

Several fields that configure LLM behavior have been moved from the `[agent]` section to the `[llm]` section. This aligns these settings with the provider they configure and reduces duplication. The `[agent]` section now only contains agent behavior settings (system prompt, context, turn depth, tool filters, etc.).

**The `[agent]` section will reject these fields with an unknown-field error.

**NOTE: CONFIGS THAT HAVE NOT BEEN UPDATED WILL FAIL TO PARSE AND THE APP WILL FAIL TO START. See [Startup Errors](#startup-errors).

---

## Migrated Fields

The following fields must move from `[agent]` to `[llm]`:

| Field | Old location | New location |
|---|---|---|
| `temperature` | `[agent]` | `[llm]` |
| `reasoning_effort` | `[agent]` | `[llm]` |
| `max_tokens` | `[agent]` | `[llm]` |
| `context_window` | `[agent]` | `[llm]` |
| `additional_params` | `[agent]` | `[llm]` |

---

## Before / After Examples

### temperature, reasoning_effort, context_window, max_tokens

```toml
# BEFORE
[llm]
provider = "openai"
model = "gpt-5.1"

[agent]
name = "My Agent"
system_prompt = "..."
temperature = 0.3
reasoning_effort = "medium"
context_window = 128000
max_tokens = 1000

# AFTER
[llm]
provider = "openai"
model = "gpt-5.1"
reasoning_effort = "medium"
temperature = 0.3
context_window = 128000
max_tokens = 1000

[agent]
name = "My Agent"
system_prompt = "..."
```

### Anthropic/Bedrock thinking parameters

```toml
# BEFORE
[llm]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"

[agent]
name = "Thinking Agent"
system_prompt = "..."
temperature = 1.0
additional_params = { thinking = { type = "enabled", budget_tokens = 8000 } }

# AFTER
[llm]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
temperature = 1.0
additional_params = { thinking = { type = "enabled", budget_tokens = 8000 } }

[agent]
name = "Thinking Agent"
system_prompt = "..."
```

For the nested table syntax variant:

```toml
# BEFORE
[agent.additional_params.thinking]
type = "enabled"
budget_tokens = 8000

# AFTER
[llm.additional_params.thinking]
type = "enabled"
budget_tokens = 8000
```
---

## Ollama-Specific Changes

Two Ollama-specific fields (`num_ctx`, `num_predict`) have been removed as dedicated top-level `[llm]` fields. They must now be placed under `[llm.additional_params]`.

```toml
# BEFORE
[llm]
provider = "ollama"
model = "llama3.2"
num_ctx = 8192
num_predict = 2048

# AFTER
[llm]
provider = "ollama"
model = "llama3.2"

[llm.additional_params]
num_ctx = 8192
num_predict = 2048
```

---

### Startup Errors:

Affected Configs: `deny_unknown_fields` on `AgentConfig` and `LlmConfig`

Both `aura::config::AgentConfig` and `aura_config::config::AgentConfig` now carry `#[serde(deny_unknown_fields)]`. Any errant or stale field in either section (including the removed fields listed above) will produce a hard parse error instead of being silently ignored, calling attention to the customer.

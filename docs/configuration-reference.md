# Configuration Reference

AURA is configured with a single TOML file. The file path defaults to `config.toml` in the working directory; override it with the `CONFIG_PATH` environment variable.

```bash
CONFIG_PATH=configs/my-agent.toml cargo run --bin aura-web-server
```

To validate a config file, start the web server or CLI against it — both validate immediately and exit with a clear error if parsing fails, before binding to any port or entering the REPL:

```bash
cargo run -p aura-web-server -- --config your-config.toml   # exits on parse error before binding
cargo run -p aura-cli -- --config your-config.toml           # exits on parse error before REPL
```

## Environment Variable Interpolation

Any string value in the config can reference an environment variable using `{{ env.VAR_NAME }}`. An optional `| default: 'value'` fallback prevents a hard error when the variable is unset.

```toml
api_key = "{{ env.OPENAI_API_KEY }}"
Authorization = "Bearer {{ env.GITHUB_PERSONAL_ACCESS_TOKEN | default: '' }}"
```

---

## Root-Level Fields

| Field | Type | Default | Description |
|---|---|---|---|
| `memory_dir` | string | — | Base directory for scratchpad storage and orchestration artifact persistence. Required when scratchpad is enabled. |

```toml
memory_dir = "/tmp/aura-sessions"
```

---

## `[agent]`

Defines the agent identity, system prompt, and behavioral settings.

| Field | Type | Default | Description |
|---|---|---|---|
| `name` | string | `"Assistant"` | Display name. Doubles as the model identifier clients send in requests. |
| `alias` | string | — | Stable identifier for model selection. Clients send this as the `model` field. Useful when the name contains spaces. |
| `system_prompt` | string | `"You are a helpful assistant."` | The agent's system prompt. Multi-line strings use TOML `"""..."""` syntax. |
| `context` | list of strings | `[]` | Additional context strings prepended to the system prompt. |
| `turn_depth` | integer | `5` | Max tool-call rounds per user turn. Set to `0` to disable. Acts as a failsafe to prevent models from spinning out in unbounded tool-call loops. |
| `nudge_last_turn` | bool | `false` | Append a wrap-up warning to tool output on the final turn before `turn_depth` terminates the run (orchestration workers are told to call `submit_result`), rather than silently losing all gathered work. |
| `nudge_turns_remaining` | integer | — | Start wrap-up warnings when N or fewer tool-calling turns remain. Independent of `nudge_last_turn`; both default to off and can be enabled separately. |
| `mcp_filter` | list of strings | — | Glob patterns selecting which MCP tools to expose. When omitted, all tools are included. |
| `enable_client_tools` | bool | `false` | Allow the agent to invoke client-side tools advertised in the request. See CLI docs for security implications. |
| `client_tool_filter` | list of strings | — | Glob patterns narrowing which client tools are allowed (requires `enable_client_tools = true`). |
| `model_owner` | string | _(LLM provider name)_ | Overrides the `owned_by` field in `/v1/models` responses. |
| `created_at` | integer | _(current time)_ | Creation timestamp in milliseconds since epoch. Shown in `/v1/models` responses. |
| `hidden` | bool | `false` | Hides this agent from the `/v1/models` list. Useful for agents meant only for internal callers. |

```toml
[agent]
name = "DevOps Assistant"
alias = "devops"
system_prompt = """
You are a DevOps assistant with access to GitHub.
Help with code review, PR management, and repo exploration.
"""
turn_depth = 10
mcp_filter = ["get_*", "list_*", "search_*"]
model_owner = "acme"
hidden = false
```

---

## Multiple Agents

`CONFIG_PATH` can point to a single TOML file or a directory of `.toml` files. When pointed at a directory, AURA loads every `.toml` file and serves each as a selectable agent:

```
configs/
├── research-assistant.toml
├── devops-agent.toml
└── code-reviewer.toml
```

```bash
CONFIG_PATH=configs/ cargo run --bin aura-web-server
```

Each agent is identified by its `alias` (if set) or `name`. Clients discover available agents via `GET /v1/models` and select one by passing its identifier as the `model` field in chat completion requests — the same field tools like LibreChat, OpenWebUI, and CLI clients use to present a model picker.

Agent selection follows this order:
1. If only one config is loaded, it is always used (the `model` field is ignored).
2. Otherwise, `model` is matched first, then `DEFAULT_AGENT` if `model` is absent.
3. Returns a 400 error if multiple configs are loaded and neither `model` nor `DEFAULT_AGENT` resolves to a match.

```toml
[agent]
name = "DevOps Assistant"
alias = "devops"             # clients send "model": "devops"
system_prompt = "You are a DevOps expert."
model_owner = "mezmo"        # override owned_by in /v1/models (defaults to LLM provider)
```

Aliases must be unique across all loaded configs. If two configs share the same `name` and neither has an alias, loading fails with a validation error.

**Hidden agents** are excluded from `GET /v1/models` and the CLI's `/model` list but remain fully accessible when a caller targets them by exact name or alias. Set `hidden = true` to hide agents that are in development, restricted to known callers, or should not appear in model pickers (LibreChat, OpenWebUI, etc.):

```toml
[agent]
name = "Internal Triage Agent"
hidden = true         # excluded from /v1/models and CLI model list; still callable by name
system_prompt = "..."
```

---

## `[agent.llm]`

Configures the LLM provider. The `provider` field is a discriminant that selects the variant and its required fields.

### Common Fields

These fields are available on all providers except where noted.

| Field | Type | Default | Description |
|---|---|---|---|
| `provider` | string | — | **Required.** One of: `openai`, `anthropic`, `bedrock`, `gemini`, `ollama`, `openrouter`. |
| `model` | string | — | **Required.** Model identifier (e.g. `gpt-4o`, `claude-sonnet-4-20250514`). |
| `max_tokens` | integer | — | Maximum tokens in the response. |
| `context_window` | integer | — | Context window size in tokens. Required when scratchpad is enabled. Used for usage reporting in `aura.session_info` events. |
| `temperature` | float | — | Sampling temperature (0.0–2.0). Higher values increase randomness. |
| `additional_params` | table | — | Provider-specific parameters merged into the API request body. Useful for features like extended thinking. |

### `provider = "openai"`

```toml
[agent.llm]
provider = "openai"
api_key = "{{ env.OPENAI_API_KEY }}"
model = "gpt-4o"
max_tokens = 8192
context_window = 128000
temperature = 0.7
# base_url = "https://api.openai.com/v1"  # custom endpoint
# reasoning_effort = "medium"             # minimal | low | medium | high
```

Extra fields:

| Field | Type | Default | Description |
|---|---|---|---|
| `api_key` | string | — | **Required.** OpenAI API key. |
| `base_url` | string | — | Override the API base URL (useful for compatible proxies). |
| `reasoning_effort` | string | — | `minimal`, `low`, `medium`, or `high`. Controls reasoning depth on supported models. |

### `provider = "anthropic"`

```toml
[agent.llm]
provider = "anthropic"
api_key = "{{ env.ANTHROPIC_API_KEY }}"
model = "claude-sonnet-4-20250514"
context_window = 200000

# Enable extended thinking:
[agent.llm.additional_params]
thinking = { type = "adaptive", budget_tokens = 8000 }
```

Extra fields:

| Field | Type | Default | Description |
|---|---|---|---|
| `api_key` | string | — | **Required.** Anthropic API key. |
| `base_url` | string | — | Override the API base URL. |

### `provider = "bedrock"`

Uses AWS credentials from the environment (AWS profile, IAM role, or environment variables). No API key field.

```toml
[agent.llm]
provider = "bedrock"
model = "us.anthropic.claude-3-5-sonnet-20241022-v2:0"
region = "{{ env.AWS_REGION }}"
profile = "default"  # optional
context_window = 200000
```

Extra fields:

| Field | Type | Default | Description |
|---|---|---|---|
| `region` | string | — | **Required.** AWS region (e.g. `us-east-1`). |
| `profile` | string | — | AWS profile name. Uses the default credential chain if omitted. |

### `provider = "gemini"`

```toml
[agent.llm]
provider = "gemini"
api_key = "{{ env.GOOGLE_API_KEY }}"
model = "gemini-2.0-flash"
context_window = 1000000
```

Extra fields:

| Field | Type | Default | Description |
|---|---|---|---|
| `api_key` | string | — | **Required.** Google API key. |
| `base_url` | string | — | Override the API base URL. |

### `provider = "ollama"`

No API key required. Defaults to `http://localhost:11434`.

```toml
[agent.llm]
provider = "ollama"
model = "qwen3:30b-a3b"
base_url = "http://localhost:11434"
context_window = 32768
fallback_tool_parsing = true

# Pass Ollama-specific parameters:
[agent.llm.additional_params]
num_ctx = 32768
top_k = 40
```

Extra fields:

| Field | Type | Default | Description |
|---|---|---|---|
| `base_url` | string | `"http://localhost:11434"` | Ollama server URL. |
| `fallback_tool_parsing` | bool | `false` | Parse tool calls from streamed text output. Enable for models that emit tool calls as text (e.g. some qwen3 variants) rather than native tool-call structures. |

### `provider = "openrouter"`

Access 300+ models through a single API key. Uses OpenRouter's reasoning wire format (`reasoning` + `reasoning_details`). For other OpenAI-compatible APIs (Fireworks, Together), use `provider = "openai"` with `base_url` instead.

```toml
[agent.llm]
provider = "openrouter"
api_key = "{{ env.OPENROUTER_API_KEY }}"
model = "anthropic/claude-sonnet-4"
# base_url = "https://custom-endpoint/v1"  # OpenRouter-compatible proxies only
```

Extra fields:

| Field | Type | Default | Description |
|---|---|---|---|
| `api_key` | string | — | **Required.** OpenRouter API key. |
| `base_url` | string | — | Override the base URL. |

---

## `[mcp]`

Configures Model Context Protocol (MCP) tool servers.

| Field | Type | Default | Description |
|---|---|---|---|
| `sanitize_schemas` | bool | `true` | Sanitize tool schemas for OpenAI function-calling compatibility. Fixes `anyOf` unions, missing types, and optional parameters that strict mode rejects. |

```toml
[mcp]
sanitize_schemas = true
```

### `[mcp.servers.<name>]`

Each server is a named entry under `[mcp.servers]`. The `transport` field selects the connection type.

Tool names are not namespaced by server. If two servers register a tool with the same name, the first one loaded wins silently ([#186](https://github.com/mezmo/aura/issues/186)).

#### `transport = "http_streamable"` (recommended)

Connects to an MCP server over HTTP using the current MCP streamable transport (post-2025-11-05).

```toml
[mcp.servers.my_tools]
transport = "http_streamable"
url = "http://localhost:8081/mcp"
description = "My tool server"

[mcp.servers.my_tools.headers]
Authorization = "Bearer {{ env.MCP_TOKEN }}"
```

#### `transport = "sse"`

Connects using the legacy SSE-based MCP protocol. Supports the same `headers`, `headers_from_request`, and `scratchpad` options as `http_streamable`.

```toml
[mcp.servers.legacy_server]
transport = "sse"
url = "http://localhost:8082/sse"
```

#### `transport = "stdio"`

Spawns a local child process. Each agent request creates its own process instance.

`cmd` is a list where `cmd[0]` is the executable and `cmd[1..]` are fixed arguments that are part of the command (e.g. a script path). `args` are additional arguments appended after.

```toml
[mcp.servers.everything]
transport = "stdio"
cmd = ["npx"]
args = ["-y", "@modelcontextprotocol/server-everything"]

[mcp.servers.everything.env]
MY_VAR = "value"

# Script-based example:
# cmd = ["python3", "/opt/mcp-servers/weather.py"]
# args = ["--verbose"]
```

#### Static Headers

Add static headers to every request to an HTTP or SSE server:

```toml
[mcp.servers.my_tools.headers]
Authorization = "Bearer {{ env.MCP_TOKEN }}"
X-Tenant-ID = "acme"
```

#### Header Forwarding (`headers_from_request`)

Forward headers from the incoming API request to the MCP server. The table maps outgoing header name → incoming request header name. Useful for per-user auth delegation.

```toml
[mcp.servers.my_tools.headers_from_request]
# Outgoing header = Incoming request header
Authorization = "x-user-token"
X-User-ID = "x-user-id"
```

When `headers_from_request` is set, the forwarded header takes precedence over any matching static header from `headers`.

#### Per-Tool Scratchpad Thresholds

Override when a tool's output gets intercepted by the scratchpad system. Keys are glob patterns matched against tool names; the most specific (longest) pattern wins.

```toml
[mcp.servers.my_tools.scratchpad]
"get_large_*" = { min_tokens = 100 }  # intercept all large-prefixed tools
"get_small_*" = { min_tokens = 99999 } # effectively disable interception
"get_report"  = { min_tokens = 200 }  # custom threshold for one tool
```

---

## `[agent.scratchpad]`

Controls context window management. When enabled, large MCP tool outputs are saved to disk and replaced with a file pointer. The agent then uses eight exploration tools (`head`, `slice`, `grep`, `schema`, `item_schema`, `get_in`, `iterate_over`, `read`) to selectively read the data it needs.

Requires `memory_dir` to be set at the root level and `context_window` to be set on `[agent.llm]`.

| Field | Type | Default | Description |
|---|---|---|---|
| `enabled` | bool | `false` | Activate scratchpad interception. |
| `context_safety_margin` | float | `0.20` | Fraction of the context window (0.0–1.0) reserved for reasoning and output. Scratchpad intercepts output before this budget is exhausted. |
| `max_extraction_tokens` | integer | `10000` | Maximum tokens a single exploration tool call may return, preventing a single read from flooding the context. |
| `turn_depth_bonus` | integer | `6` | Extra tool-call turns added when scratchpad is active, to leave room for exploration calls after interception. |

```toml
memory_dir = "/tmp/aura-sessions"

[agent]
turn_depth = 10

[agent.llm]
provider = "openai"
api_key = "{{ env.OPENAI_API_KEY }}"
model = "gpt-4o"
context_window = 128000

[agent.scratchpad]
enabled = true
context_safety_margin = 0.20
max_extraction_tokens = 10000
turn_depth_bonus = 6
```

---

## `[agent.skills]`

Points the agent at directories of on-demand skills. Skills follow the [Agent Skills specification](https://agentskills.io/specification): each skill is a subdirectory containing a `SKILL.md` file with YAML frontmatter (`name`, `description`). The `name` must match the directory name and consist of lowercase alphanumerics and hyphens only (1–64 characters, no leading/trailing/consecutive hyphens).

```toml
[[agent.skills.local]]
source = "skills/"        # relative to process CWD, or absolute

[[agent.skills.local]]
source = "/opt/shared-skills"
```

A `load_skill` tool is exposed to the agent that loads and executes skills on demand.

---

## `[[vector_stores]]`

Configures RAG (Retrieval-Augmented Generation) stores. Each entry creates a `vector_search_<name>` tool the agent can call. Multiple stores create multiple search tools.

| Field | Type | Description |
|---|---|---|
| `name` | string | Unique identifier. Used in worker `vector_stores` lists. |
| `context_prefix` | string | Optional hint shown to the LLM describing what the store contains. |
| `type` | string | Store type: `in_memory`, `qdrant`, or `bedrock_kb`. |

### `type = "qdrant"`

```toml
[[vector_stores]]
name = "docs"
type = "qdrant"
url = "http://localhost:6334"
collection_name = "documents"
context_prefix = "Technical documentation and API references"

[vector_stores.embedding_model]
provider = "openai"
model = "text-embedding-3-small"
api_key = "{{ env.OPENAI_API_KEY }}"
```

### `type = "in_memory"`

```toml
[[vector_stores]]
name = "knowledge"
type = "in_memory"

[vector_stores.embedding_model]
provider = "openai"
model = "text-embedding-3-small"
api_key = "{{ env.OPENAI_API_KEY }}"
```

### `type = "bedrock_kb"`

Managed RAG — no embedding model needed. Uses AWS credentials.

```toml
[[vector_stores]]
name = "company_docs"
type = "bedrock_kb"
knowledge_base_id = "{{ env.BEDROCK_KB_ID }}"
region = "{{ env.AWS_REGION }}"
# profile = "default"  # optional
context_prefix = "Company documentation"
```

### Embedding Models

Used by `in_memory` and `qdrant` types.

```toml
# OpenAI:
[vector_stores.embedding_model]
provider = "openai"
model = "text-embedding-3-small"
api_key = "{{ env.OPENAI_API_KEY }}"

# AWS Bedrock:
[vector_stores.embedding_model]
provider = "bedrock"
model = "amazon.titan-embed-text-v2:0"
region = "{{ env.AWS_REGION }}"
profile = "default"  # optional
```

---

## `[tools]`

Enables built-in server-side tools.

| Field | Type | Default | Description |
|---|---|---|---|
| `filesystem` | bool | `false` | Expose basic filesystem read/write tools to the agent. |
| `custom_tools` | list of strings | `[]` | Reserved for future use. |

```toml
[tools]
filesystem = true
```

---

## Session Store (Durable and Multi-Pod Deployments)

Unlike every other section on this page, the session store is **not configured in TOML** — it's deployment infrastructure (one instance per server, not per-agent), so it's configured only via environment variables.

By default, cross-request session state (A2A tasks, parked HITL approvals) lives in process memory — correct for a single pod, the CLI, and local dev. Behind a load balancer with multiple replicas, configure a shared Redis/Valkey backend and every cross-request flow works no matter which pod serves each request: A2A `message:send` → poll → `list` → history-by-context, A2A `subscribe`/`cancel` against a task executing on another pod, and conversational HITL approvals resolved by a `POST /v1/approvals/{id}` that lands away from the pod that parked them.

```bash
export AURA_SESSION_STORE=redis                          # memory (default) | redis
export AURA_SESSION_STORE_URL=redis://valkey:6379        # redis:// or rediss:// (Valkey compatible)
export AURA_SESSION_STORE_PREFIX=aura:prod               # optional namespace (default "aura")
export AURA_SESSION_STORE_CONNECT_TIMEOUT_SECS=5         # optional
export AURA_SESSION_STORE_TASK_TTL_SECS=86400            # optional; 0 = no expiry
```

The server pings the backend at startup and fails fast if it is unreachable; `/health` reports the backend and its ping latency.

The Redis backend requires building with the `session-store-redis` cargo feature (`cargo build --release --features aura-web-server/session-store-redis`). The in-memory backend is always available. See [`docs/design/session-storage.md`](design/session-storage.md) for the design and Helm packaging roadmap.

---

## `[orchestration]`

Enables multi-agent orchestration mode. A coordinator agent decomposes user queries into tasks and delegates them to specialized worker agents for parallel execution.

The coordinator's system prompt comes from `[agent].system_prompt`. Workers are defined in `[orchestration.worker.<name>]` sections.

Execution loop:

- `Plan`: coordinator decomposes the request into a task DAG.
- `Execute`: dependency-ready tasks run in parallel waves on worker agents.
- `Continue`: coordinator consolidates worker outputs and routes to a final response, replan, or clarification.

Workers run with isolated task context windows and filtered MCP/vector-store access based on each worker block.

| Field | Type | Default | Description |
|---|---|---|---|
| `enabled` | bool | `false` | Activate orchestration mode. |
| `max_planning_cycles` | integer | `3` | Maximum plan→execute→continue cycles per request. |
| `max_plan_parse_retries` | integer | `3` | Retries on plan parse failure before falling back to single-task execution. |
| `allow_direct_answers` | bool | `true` | Allow the coordinator to answer simple queries directly without delegating. |
| `allow_clarification` | bool | `true` | Allow the coordinator to ask for clarification on ambiguous queries. |
| `tools_in_planning` | string | `"summary"` | How much tool info is shown during planning: `"none"`, `"summary"` (tool names per worker), or `"full"` (names + descriptions). |
| `max_tools_per_worker` | integer | `10` | Truncates the tool list per worker in the planning prompt with a `(+N more)` suffix. |
| `coordinator_vector_stores` | list of strings | `[]` | Names of vector stores the coordinator can access. |
| `worker_system_prompt` | string | — | Custom system prompt injected into generic (non-specialized) workers. |
| `duplicate_call_nudge_threshold` | integer | `3` | Consecutive identical tool calls before appending a guidance annotation. |
| `duplicate_call_block_threshold` | integer | `5` | Consecutive identical tool calls before appending an abort annotation. |

```toml
[orchestration]
enabled = true
max_planning_cycles = 3
allow_direct_answers = true
tools_in_planning = "full"
coordinator_vector_stores = ["runbooks"]
memory_dir = "/tmp/aura-orchestration"   # legacy flat field; prefer top-level memory_dir
```

### `[orchestration.timeouts]`

| Field | Type | Default | Description |
|---|---|---|---|
| `per_call_timeout_secs` | integer | `0` (disabled) | Per-call timeout for coordinator and worker LLM calls. Set to a positive value to enable. |

```toml
[orchestration.timeouts]
per_call_timeout_secs = 120
```

### `[orchestration.artifacts]`

Controls persistence and artifact promotion.

| Field | Type | Default | Description |
|---|---|---|---|
| `memory_dir` | string | — | Base directory for run artifacts. Alias: `memory_path`. (Prefer top-level `memory_dir`.) |
| `result_artifact_threshold` | integer | `4000` | Character threshold for writing worker results to artifact files instead of inlining them. |
| `result_summary_length` | integer | `2000` | Max inline summary length when a result is promoted to an artifact. |
| `session_history_turns` | integer | `3` | Max prior run manifests injected as session context in the coordinator preamble. Set to `0` to disable. |
| `persistence_drain_timeout_ms` | integer | `2000` | Timeout (ms) for draining in-flight persistence writes between execution phases. |
| `tool_output_artifact_threshold` | integer | `500` | Character threshold for promoting tool outputs to artifact files. |
| `tool_output_duration_threshold_ms` | integer | `5000` | Duration threshold (ms) for promoting tool outputs regardless of size. |
| `show_tool_reasoning_in_continuation` | bool | `false` | Include condensed tool reasoning traces in continuation prompts so the coordinator can see why workers called specific tools. |
| `max_session_runs` | integer | `20` | Max run directories retained per session before oldest runs are pruned. Set to `0` to disable pruning. |

```toml
[orchestration.artifacts]
memory_dir = "/tmp/aura-orchestration"
result_artifact_threshold = 4000
result_summary_length = 2000
session_history_turns = 3
max_session_runs = 20
```

### `[orchestration.worker.<name>]`

Defines a specialized worker. Worker names must be unique case-insensitively (to avoid filesystem collisions) and non-empty.

| Field | Type | Default | Description |
|---|---|---|---|
| `description` | string | — | **Required.** One-line description shown to the coordinator during planning. |
| `preamble` | string | — | **Required.** Complete system prompt for this worker (replaces the generic worker template). |
| `mcp_filter` | list of strings | `[]` (all tools) | Glob patterns selecting which MCP tools this worker can access. |
| `vector_stores` | list of strings | `[]` (none) | Names of vector stores this worker can access. Workers have no RAG access by default. |
| `turn_depth` | integer | _(inherits `[agent].turn_depth`)_ | Max tool-call rounds for this worker. |

```toml
[orchestration.worker.operations]
description = "Logs, pipelines, metrics, and system analysis"
preamble = """
You are an Operations Specialist with access to observability tools.
Use tools to investigate incidents — do not fabricate data.
"""
mcp_filter = ["mezmo_*"]
vector_stores = ["runbooks"]
turn_depth = 8
```

#### Per-Worker LLM Override

Workers inherit `[agent.llm]` by default. Provide `[orchestration.worker.<name>.llm]` to use a different model for a specific worker (e.g. a cheaper model for simple tasks).

```toml
[orchestration.worker.summarizer.llm]
provider = "openai"
api_key = "{{ env.OPENAI_API_KEY }}"
model = "gpt-4o-mini"
context_window = 128000
```

#### Per-Worker Scratchpad Override

```toml
[orchestration.worker.analyst.scratchpad]
enabled = true
max_extraction_tokens = 5000
```

#### Per-Worker Skills Override

`None` (field absent) inherits `[agent.skills]`. An explicit empty list disables skills. A non-empty list replaces the agent's skills entirely (no merging).

```toml
[[orchestration.worker.researcher.skills.local]]
source = "skills/research"
```

---

## Complete Examples

### Minimal: OpenAI

```toml
[agent]
name = "Assistant"
system_prompt = "You are a helpful assistant."

[agent.llm]
provider = "openai"
api_key = "{{ env.OPENAI_API_KEY }}"
model = "gpt-4o"
```

### Single Agent with MCP Tools

```toml
[mcp]
sanitize_schemas = true

[mcp.servers.github]
transport = "http_streamable"
url = "https://api.githubcopilot.com/mcp/"
description = "GitHub repository operations"

[mcp.servers.github.headers]
Authorization = "Bearer {{ env.GITHUB_PERSONAL_ACCESS_TOKEN }}"

[agent]
name = "DevOps Assistant"
system_prompt = """
You are a DevOps assistant with access to GitHub.
Help with code review, PR management, and repo exploration.
"""
turn_depth = 10

[agent.llm]
provider = "openai"
api_key = "{{ env.OPENAI_API_KEY }}"
model = "gpt-4o"
```

### Single Agent with Scratchpad

Large tool outputs are intercepted and saved to disk; the agent uses exploration tools to read what it needs.

```toml
memory_dir = "/tmp/aura-sessions"

[agent]
name = "Data Analyst"
system_prompt = """
You are a data analysis assistant. When you see a scratchpad pointer
([scratchpad: file_id=...]), use the exploration tools (head, grep, schema,
get_in, etc.) to selectively read the data you need. Do not re-call the
original tool.
"""
turn_depth = 10

[agent.llm]
provider = "openai"
api_key = "{{ env.OPENAI_API_KEY }}"
model = "gpt-4o"
context_window = 128000

[agent.scratchpad]
enabled = true
context_safety_margin = 0.20
max_extraction_tokens = 10000

[mcp.servers.data_api]
transport = "http_streamable"
url = "http://localhost:9000/mcp"

[mcp.servers.data_api.scratchpad]
"get_dataset_*" = { min_tokens = 100 }
```

### Multi-Agent Orchestration with Per-Worker Models

```toml
memory_dir = "/tmp/aura-orchestration"

[agent]
name = "SRE Coordinator"
system_prompt = """
You are an SRE coordinator. Decompose observability tasks into sub-tasks
and delegate to the appropriate specialist worker.
- Kubernetes cluster state and workloads: delegate to k8s-specialist
- Prometheus metrics and alerts: delegate to metrics-analyst
"""

[agent.llm]
provider = "openai"
api_key = "{{ env.OPENAI_API_KEY }}"
model = "gpt-4o"
context_window = 128000

[mcp.servers.kubernetes]
transport = "http_streamable"
url = "http://localhost:8081/mcp"

[mcp.servers.prometheus]
transport = "http_streamable"
url = "http://localhost:8082/mcp"

[orchestration]
enabled = true
max_planning_cycles = 3
tools_in_planning = "full"
allow_direct_answers = true

[orchestration.timeouts]
per_call_timeout_secs = 120

[orchestration.worker.k8s-specialist]
description = "Kubernetes cluster inspection: namespaces, workloads, pods, events"
turn_depth = 8
mcp_filter = ["namespaces_list", "pods_*", "resources_*", "events_list"]
preamble = """
You are a Kubernetes Specialist. Use tools to inspect the cluster.
Never guess cluster state — always call the relevant tool first.
"""

[orchestration.worker.k8s-specialist.llm]
provider = "openai"
api_key = "{{ env.OPENAI_API_KEY }}"
model = "gpt-4o"
context_window = 128000

[orchestration.worker.metrics-analyst]
description = "Prometheus metrics queries, target health, and alert status"
turn_depth = 8
mcp_filter = ["execute_query", "execute_range_query", "list_metrics", "get_targets"]
preamble = """
You are a Prometheus Analyst. Query metrics directly — do not fabricate values.
Report metric names, labels, and exact values from tool results.
"""

[orchestration.worker.metrics-analyst.llm]
provider = "openai"
api_key = "{{ env.OPENAI_API_KEY }}"
model = "gpt-4o-mini"
context_window = 128000
```

### RAG with Qdrant

```toml
[[vector_stores]]
name = "docs"
type = "qdrant"
url = "http://localhost:6334"
collection_name = "documents"
context_prefix = "Technical documentation and API references"

[vector_stores.embedding_model]
provider = "openai"
model = "text-embedding-3-small"
api_key = "{{ env.OPENAI_API_KEY }}"

[[vector_stores]]
name = "runbooks"
type = "qdrant"
url = "http://localhost:6334"
collection_name = "runbooks"
context_prefix = "Operational runbooks and incident response procedures"

[vector_stores.embedding_model]
provider = "openai"
model = "text-embedding-3-small"
api_key = "{{ env.OPENAI_API_KEY }}"

[agent]
name = "Knowledge Assistant"
system_prompt = """
You are a knowledge assistant. Use the vector_search_docs and
vector_search_runbooks tools to ground your answers in documentation.
"""

[agent.llm]
provider = "openai"
api_key = "{{ env.OPENAI_API_KEY }}"
model = "gpt-4o"
```

### Local Ollama (No API Key)

```toml
[agent]
name = "Local Assistant"
system_prompt = "You are a helpful assistant running locally."

[agent.llm]
provider = "ollama"
model = "qwen3:30b-a3b"
base_url = "http://localhost:11434"
context_window = 32768
fallback_tool_parsing = true

[agent.llm.additional_params]
num_ctx = 32768
```

### AWS Bedrock with Knowledge Base

```toml
[[vector_stores]]
name = "company_kb"
type = "bedrock_kb"
knowledge_base_id = "{{ env.BEDROCK_KB_ID }}"
region = "{{ env.AWS_REGION }}"
context_prefix = "Company internal documentation"

[agent]
name = "Enterprise Assistant"
system_prompt = """
You are an enterprise assistant. Use the company knowledge base
to answer questions grounded in internal documentation.
"""

[agent.llm]
provider = "bedrock"
model = "us.anthropic.claude-3-5-sonnet-20241022-v2:0"
region = "{{ env.AWS_REGION }}"
context_window = 200000
```

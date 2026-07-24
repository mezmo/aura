# Scratchpad (Context Window Management)

MCP tools can return responses far larger than an LLM's context window — a single Kubernetes workload listing or log export can be tens of thousands of tokens. Without intervention, this fills the context and degrades reasoning quality.

Scratchpad solves this by intercepting large tool outputs and storing them on disk. The LLM gets a summary and eight read-only exploration tools (`head`, `slice`, `grep`, `schema`, `item_schema`, `get_in`, `iterate_over`, `read`) to selectively pull in only the data it needs.

Scratchpad works in both single-agent and orchestration modes.

## Configuration

Configure at `[agent.scratchpad]` (applies to the single agent, or provides defaults for orchestration workers) and optionally override per worker at `[orchestration.worker.<name>.scratchpad]`. Set a top-level `memory_dir` for persistence. See [`[agent.scratchpad]`](configuration-reference.md#agentscratchpad) in the configuration reference for the full field table, and [`[mcp.servers.<name>.scratchpad]`](configuration-reference.md#per-tool-scratchpad-thresholds) for per-tool interception thresholds.

```toml
# Top-level — required when scratchpad is enabled. Shared by single-agent
# scratchpad and orchestration persistence.
memory_dir = "/tmp/aura"

[agent.scratchpad]
enabled = true
context_safety_margin = 0.20          # 20% of context reserved for reasoning/output
max_extraction_tokens = 10_000        # cap per extraction tool call
turn_depth_bonus = 6                  # extra ReAct turns when scratchpad is active

[orchestration.worker.data-explorer.scratchpad]
# Override just for this worker
max_extraction_tokens = 5_000
```

## Storage locations

- Single-agent: `{memory_dir}/scratchpad/`
- Orchestration: `{memory_dir}/{run_id}/iteration-{n}/scratchpad/` (legacy `[orchestration.artifacts].memory_dir` still works as a fallback)

## Per-tool interception thresholds

Per-tool interception thresholds are configured at `[mcp.servers.<server>.scratchpad]`. Keys are **glob patterns** (default threshold `5_120` if omitted) that are matched against tool names at interception time:

```toml
[mcp.servers.k8s-sre.scratchpad]
"*_list_*"                  = { min_tokens = 512 }   # broad
"k8s_list_service_monitors" = { min_tokens = 384 }   # specific override
"*"                         = { min_tokens = 4096 }  # catch-all
```

When multiple patterns match the same tool, the **longest (most specific) pattern wins**; on length ties the smallest threshold wins.

## Token counting

Token counting uses real tokenization, not byte/character heuristics, so `min_tokens` and the context budget reflect actual model token cost. Dispatch is provider-aware and lives in `token_counter_for_provider` (`scratchpad/context_budget.rs`):

- **OpenAI**: `tiktoken-rs`, using `o200k_base` for GPT-5/4o/o-series models and `cl100k_base` for older models.
- **Gemini**: the `gemini-tokenizer` crate's embedded Gemma 3 SentencePiece model (exact, fully local).
- **Anthropic / Bedrock-Claude**: a calibrated `cl100k_base × 1.1` approximation, since Claude ships no public tokenizer. Bedrock routes to this counter only for Claude model IDs.
- Everything else falls back to `o200k_base`.

## Per-call extraction limit

`max_extraction_tokens` (default `10_000`): every exploration tool checks the size of its result before returning. If a single call would exceed this cap (or the cumulative `ContextBudget`), the tool returns a structured JSON error like `{"error": "head_too_large", "estimated_tokens": ..., "suggestions": [...]}` instead of the content. The LLM sees this as a successful tool result and retries with smaller params — each retry consumes a turn, which is why `turn_depth_bonus` exists.

## Budget and usage reporting

Each agent (single-agent or orchestration worker) gets a fresh `ContextBudget` scoped to that agent's effective LLM's `context_window`; budgets are created at `create_worker()` time and live on `Agent.scratchpad_budget` — workers never share an "orchestrator-level" budget. LLM-reported per-turn token counts (`input_tokens` + `output_tokens`) feed back into the budget as ground truth, so `remaining()` reflects actual context pressure — orchestration via `StreamItem::TurnUsage`, single-agent via the streaming hook's `on_stream_completion_response_finish`.

A per-agent `aura.scratchpad_usage` SSE event (with `agent_id`, `tokens_intercepted`, `tokens_extracted`) is emitted when the agent finishes — the same event name fires for both single-agent and worker contexts (it lives in the base `aura.*` namespace, not `aura.orchestrator.*`).

## Result artifacts and `read_artifact`

In orchestration, large task results are saved to artifact files under `{memory_dir}/{run_id}/artifacts/`. The scratchpad read tools resolve files anywhere under a per-agent **read root**, not just the scratchpad subdir — for orchestration workers the read root is the session dir, so result artifacts are explorable **in place** without copying (writes stay confined to the scratchpad dir, via `ScratchpadStorage::with_read_root`).

When a worker reads an artifact back with `read_artifact`, the same budget rules apply (it shares `check_and_record_budget` with the read tools): an artifact that fits is inlined and recorded against the budget; one that exceeds the limit comes back as a scratchpad pointer (built via the same `build_file_pointer` helper as intercepted MCP output) that the worker explores in place with the read tools (`head`, `grep`, `slice`, …) — never copied into the scratchpad. The coordinator has no scratchpad, so its `read_artifact` always returns inline content.

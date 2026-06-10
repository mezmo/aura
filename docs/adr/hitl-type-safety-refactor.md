# ADR: HITL Type-Safety Refactor

**Status:** Superseded by `docs/adr/hitl-dual-channel-architecture.md` (2026-06-10)
**Date:** 2026-06-05
**Authors:** Mike Shearer
**Related:** `docs/adr/hitl-architecture.md`, [issue #174](https://github.com/mezmo/aura/issues/174), `docs/hitl-integration-guide.md`

## Context

The HITL feature shipped in 4 commits ending with `refactor(hitl): type-safe approval decision enum and task identity grouping`. A targeted review of `hitl.rs` against `main` identified remaining runtime invariants and stringly-typed primitives that should be replaced with newtypes before V1 lands.

This ADR records the type-safety refactor and the long-term placement of the new types in light of:

1. **Issue #174** — "Refactor to make `aura-config` crate the canonical place for config structs." Today `aura-config` depends on `aura`; the long-term direction is the reverse.
2. **`aura-events` audit** — the crate's stated purpose is "shared SSE event types." Three HITL types (`RequestType`, `ApprovalDecision`) do not fit; `TaskIdentity` does (it's embedded in SSE event variants). Only `aura` consumes the first two; `aura-cli` never touches them.
3. **Glob implementation** — the hand-rolled `glob_match` in `crates/aura/src/config.rs:741-773` is 30 lines of recursive backtracking that re-allocates a `Vec<char>` on every call. The standard library equivalent (`globset`) is already in our transitive dependency graph via `regex`.

## Decisions

### D1. Newtype foundation lives in `aura::hitl` (and `aura::config` for glob), not `aura-events`

- `aura-events` is for SSE event types. The audit found `RequestType` and `ApprovalDecision` don't fit the crate's stated purpose. They're consumed only by `aura`. The foundation PR moves them from `aura-events` to `aura::hitl`. `TaskIdentity` stays in `aura-events` because `AuraStreamEvent::ApprovalRequested/Completed` embeds it.
- New newtypes (`RequestId`, `RunId`, `SessionId`, `WebhookUrl`, `GlobPattern`) are defined in `aura::hitl` (or `aura::config` for `GlobPattern`).
- `AgentScope` and `Timestamp` are SSE event field types and stay in `aura-events`.
- For issue #174: `HitlConfig` and the new config-domain newtypes it depends on (`WebhookUrl`, `GlobPattern`) should ultimately live in `aura-config`. This refactor does not move them — it flags them with `// TODO(#174): move to aura-config` and waits for #174.

### D2. `AgentScope` is a 3-variant sum type

```rust
// crates/aura-events/src/lib.rs
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum AgentScope {
    /// Single-agent deployment.
    Single { session_id: Option<SessionId> },
    /// Orchestration worker executing a tool call.
    Worker { run_id: RunId, task: TaskIdentity, session_id: Option<SessionId> },
    /// Orchestrator coordinator itself making a tool call (V2 surface).
    Coordinator { run_id: RunId },
}
```

- V1 only constructs `Single` and `Worker`. `Coordinator` is V2 (coordinator routing tool — see `hitl-architecture.md` §"Surface 3"). The variant is present in V1 with a doc comment to make the V2 surface explicit.
- Variants hold the newtypes (`SessionId`, `RunId`) from day one — not `String`. This avoids a two-step migration when Phase 3 (cross-cutting `RequestId`/`RunId`/`SessionId` migration) lands.

### D3. `ApprovalItem` has no per-item scope in V1

The `task: Option<TaskIdentity>` field on `ApprovalItem` (and on the parallel SSE event variants) is removed. For V1, every item in a single `ApprovalRequest` originates from the request's `agent`. The `agent: HitlAgentContext { name, scope: AgentScope }` field on the request is the single source of truth. V2 batching (multiple workers, one request) re-introduces per-item scope.

### D4. `ApprovalRequest.request_id` IS the global `RequestId` — one ID across the system

Today there are two distinct IDs:

| ID | Where | Generated | Used for |
|---|---|---|---|
| Global `request_id` | `AgentConfig.request_id`, `HitlContext.request_id`, `RequestSetup.request_id`, `callbacks.request_id`, all `tool_event_broker` `&str` params, all MCP cancel `&str` params | `format!("req_{}", Uuid::new_v4().simple())` in web server; `format!("a2a_{}", task_id)` in A2A | SSE routing, MCP cancellation, broker subscriptions |
| Per-approval `request_id` | `ApprovalRequest.request_id` (hitl.rs:68, 402, 534) | Fresh `Uuid::new_v4().to_string()` at each `pre_call` and `call` | Webhook audit log; never echoed in response |

They're consolidated. The wrapper and `request_approval` tool both set `ApprovalRequest.request_id = HitlContext.request_id.clone()`. The two `uuid::Uuid::new_v4()` calls at `hitl.rs:402` and `hitl.rs:534` are deleted.

The webhook sees the same ID we use for SSE routing. Cross-referencing works because the IDs are equal by construction.

### D5. `HitlConfig.enabled: bool` is removed; missing/empty `webhook_url` is a hard parse error

```rust
pub struct HitlConfig {
    pub webhook_url: WebhookUrl,        // required, parsed at TOML load
    #[serde(default = "default_hitl_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub require_approval: Vec<GlobPattern>,
}
```

The `Option<HitlConfig>` wrapper on `Config` is the enable bit. Empty `webhook_url` is rejected at TOML parse time with a `ConfigError::Validation`. This deletes the silent-disable footgun and the three `&& !webhook_url.is_empty()` checks at `builder.rs:312-315`, `orchestrator.rs:608-609`, and the new `dispatch_and_emit` empty-check branch.

### D6. `GlobPattern` wraps `globset::GlobMatcher` — not our own implementation

`globset = "0.4"` is added to `aura` and `aura-config`. The 30-line hand-rolled recursive matcher at `crates/aura/src/config.rs:741-773` is replaced by:

```rust
// crates/aura/src/config.rs
pub struct GlobPattern(globset::GlobMatcher);
impl GlobPattern {
    pub fn new(s: &str) -> Result<Self, globset::Error> { ... }
    pub fn matches(&self, text: &str) -> bool { self.0.is_match(text) }
}
```

This pre-compiles the pattern once at TOML load (or at agent build for HITL patterns), eliminates the per-call `Vec<char>` allocation, and gives us the `Clone + Send + Sync` matcher that fits an `Arc<[GlobPattern]>` field on `HitlApprovalWrapper`. Five call sites in `aura` and one in `hitl.rs` are updated. `url::Url` (transitive via `reqwest`) is used for `WebhookUrl::parse`.

`globset` is **not** currently a dependency in the workspace (verified). `regex` is in `aura`, `aura-cli`, and `aura-config`. Adding `globset` is incremental — it builds on `regex` + `aho-corasick`, both already in the dep graph transitively.

### D7. Wire-format hygiene

- `ApprovalRequest.version: u32` becomes `const PROTOCOL_VERSION: u32 = 1;` with `#[serde(default = "PROTOCOL_VERSION")]` on a kept field. Producers can never drift from the constant; consumers can override if they need to.
- `ApprovalRequest.timestamp: String` becomes `Timestamp(chrono::DateTime<chrono::Utc>)` with a serde adapter that serializes/deserializes as RFC3339. Format choice is centralized in one place.
- `ApprovalError::HttpError(reqwest::Error)` with `#[from]`. New `ApprovalError::HttpStatus(reqwest::StatusCode)` for the non-2xx branch. `ApprovalError::ParseError(serde_json::Error)` with `#[from]`. The `.to_string()` lossy conversions go away. Both `reqwest::Error` and `serde_json::Error` are `Clone`, so the existing `#[derive(Clone)]` on `ApprovalError` is preserved.

### D8. `HitlContext::new()` constructor + `pub(crate)` field visibility

Three call sites construct `HitlContext { ... }` literally (test in `hitl.rs`, `builder.rs`, `orchestrator.rs`). All collapse to `HitlContext::new(dispatch, agent_name, scope, request_id)`. Fields become `pub(crate)`. This is the largest LOC reduction in `hitl.rs` and the only structural change in `HitlContext`.

`dispatch_and_emit` collapses its 4-param signature to `(request: ApprovalRequest, item: &ApprovalItem)` — the duplicated `event_tool_name`, `event_matched_pattern`, `task` parameters were reconstructing data already in `request.items[0]`.

## Implementation: 3 PRs, 11 commits

### PR 1: Foundation — `aura-events` cleanup + newtypes + `globset` (zero behavior change)

| # | Commit | Files touched | Approx LOC delta |
|---|---|---|---|
| 1 | `refactor(events): move HITL protocol types from aura-events to aura::hitl` | `aura-events/src/lib.rs` (remove `RequestType`, `ApprovalDecision`), `aura/src/hitl.rs` (define them here), `aura/src/lib.rs` (`pub use`), `aura/src/stream_events.rs` (update imports), `aura/src/tool_event_broker.rs` (update imports) | -60 / +60 |
| 2 | `feat(aura): add core newtypes (RequestId, RunId, SessionId, WebhookUrl, Timestamp, AgentScope)` | `aura/src/hitl.rs` (new section), `aura-events/src/lib.rs` (`AgentScope`, `Timestamp`) | +120 |
| 3 | `chore(deps): add globset to aura and aura-config` | `aura/Cargo.toml`, `aura-config/Cargo.toml` | +2 |
| 4 | `refactor(aura): GlobPattern newtype wrapping globset::GlobMatcher` | `aura/src/config.rs` (replace hand-rolled `glob_match`), 6 call sites updated (`aura/src/config.rs:698, 715`, `aura/src/orchestrator.rs:1859`, `aura/src/scratchpad/setup.rs:122`, `aura/src/scratchpad/mod.rs:90, 140`, `aura/src/hitl.rs:381`) | -30 / +25 |

**Verification per commit:** `cargo build --workspace`, `cargo test --workspace`, `cargo clippy --workspace --all-targets`. No behavior change in any commit.

### PR 2: HITL refactor using the foundation (behavior change: TOML schema + wire format)

| # | Commit | Files touched | Approx LOC delta |
|---|---|---|---|
| 5 | `feat(hitl): AgentScope sum type in HitlAgentContext` | `aura-events/src/lib.rs` (replace 3 events' `task: Option<TaskIdentity>` with `scope: AgentScope`), `aura/src/hitl.rs` (replace `HitlAgentContext` fields), `aura/src/tool_event_broker.rs` (replace `task` field), `aura-web-server/src/streaming/handlers.rs` (event matchers), `aura/src/builder.rs` + `aura/src/orchestration/orchestrator.rs` (constructors) | ~0 (re-shape) |
| 6 | `fix(hitl): drop ApprovalItem.task — scope lives on the request only` | `aura/src/hitl.rs` (`ApprovalItem` struct), `aura-events/src/lib.rs` (event variants), `aura/src/tool_event_broker.rs` (event variants), `aura-web-server/src/streaming/handlers.rs` (event matchers) | -8 |
| 7 | `fix(hitl): ApprovalRequest.request_id is the global RequestId` | `aura/src/hitl.rs` (delete 2 `uuid::new_v4()` calls, take from `self.hitl.request_id.clone()`) | -2 |
| 8 | `refactor(hitl): drop HitlConfig.enabled, require non-empty webhook_url (hard parse error)` | `aura/src/config.rs` (`HitlConfig`), `aura/src/builder.rs` (collapse if-condition), `aura/src/orchestration/orchestrator.rs` (same), example TOMLs | -6 |
| 9 | `refactor(hitl): HitlContext::new() + pub(crate) fields + dispatch_and_emit takes &ApprovalItem` | `aura/src/hitl.rs`, `aura/src/builder.rs`, `aura/src/orchestration/orchestrator.rs`, `aura/src/hitl.rs` tests | -12 |
| 10 | `refactor(hitl): wire-format hygiene — const version, typed timestamp, typed ApprovalError` | `aura/src/hitl.rs` | ~0 |
| 11 | `feat(hitl): adopt WebhookUrl and GlobPattern end-to-end` | `aura/src/config.rs`, `aura/src/hitl.rs`, `aura/src/builder.rs`, `aura/src/orchestration/orchestrator.rs` | ~0 |

**Doc updates in PR 2** (same PR):
- `docs/adr/hitl-architecture.md` — update wire-format JSON example to match new `agent.scope` shape
- `docs/hitl-integration-guide.md` — update any code samples
- Sandbox stub `~/workspace/aura-sandbox/hitl-test/webhook-stub.py` is already forward-compatible (verified — it reads only `agent.name`, `items[].tool_name`, `items[].matched_pattern`, `items[].arguments`)

**Verification per commit:** `cargo build --workspace`, `cargo test --workspace`, `cargo clippy --workspace --all-targets`. For commits 5, 6, 8, 11: `make test-integration-orchestration-local`.

| Commit | workspace build | workspace test | clippy | integration |
|---|---|---|---|---|
| 5 (AgentScope) | yes | yes | yes | yes |
| 6 (drop item task) | yes | yes | yes | yes |
| 7 (global request_id) | yes | yes | yes | n/a |
| 8 (drop enabled) | yes | yes | yes | yes (config validation) |
| 9 (HitlContext::new) | yes | yes | yes | n/a |
| 10 (wire hygiene) | yes | yes | yes | n/a |
| 11 (adopt newtypes) | yes | yes | yes | yes (full HITL flow) |

### PR 3: Cross-cutting `RequestId` / `RunId` / `SessionId` migration (future, separate)

Replaces the `pub type RequestId = String;` aliases in `request_cancellation.rs` and `tool_event_broker.rs` with the newtypes from PR 1. Migrates non-HITL call sites in `aura`, `aura-config`, `aura-web-server`, `aura-cli`. Out of scope for this work. Issue #174 (move config structs to `aura-config`) is sequenced independently; the newtypes defined in `aura` in PR 1 will move to `aura-config` as part of #174.

## Wire-format changes (consolidated)

Before:
```json
{
  "version": 1,
  "request_type": "tool_gate",
  "request_id": "<uuid>",
  "timestamp": "...",
  "agent":  { "name": "...", "run_id": null, "session_id": "sess-456" },
  "items":  [ { "tool_name": "...", "arguments": {...}, "matched_pattern": "mezmo_*", "task": { "task_id": 3, "worker_id": "log_worker" } } ]
}
```

After:
```json
{
  "version": 1,
  "request_type": "tool_gate",
  "request_id": "req_<uuid>",          // global request_id (web server) or "a2a_<task_id>" (A2A)
  "timestamp": "...",
  "agent":  { "name": "...", "scope": { "scope": "single", "session_id": "sess-456" } },
  "items":  [ { "tool_name": "...", "arguments": {...}, "matched_pattern": "mezmo_*" } ]
}
```

Worker case:
```json
"agent": { "name": "log_worker", "scope": { "scope": "worker", "run_id": "abc-123", "task": { "task_id": 3, "worker_id": "log_worker" }, "session_id": null } }
```

Coordinator case (V2):
```json
"agent": { "name": "coordinator", "scope": { "scope": "coordinator", "run_id": "abc-123" } }
```

TOML schema break:
```toml
# Before — silently disabled if URL is empty
[hitl]
enabled = false
webhook_url = ""

# After — required-when-present
[hitl]
webhook_url = "https://approvals.example.com/webhook"
require_approval = ["kubectl_*"]
timeout_secs = 30
```

## Net `hitl.rs` LOC change

-25 to -30 lines (commits 6, 7, 8, 9 drive the reduction).

## Consequences

- V1 wire format changes the `agent` field shape (becomes a `scope` discriminator) and drops the `task` field on items. The mock webhook at `~/workspace/aura-sandbox/hitl-test/webhook-stub.py` is already forward-compatible.
- `globset` is a new transitive dep. Audit-recommended; well-maintained; small footprint.
- The two `pub type RequestId = String;` aliases remain in place until PR 3. They continue to be `String`-typed. The new `aura::hitl::RequestId` is a distinct newtype. PR 3 unifies them.
- `HitlConfig` flagged with `// TODO(#174): move to aura-config` for the long-term dep inversion.
- Webhook receivers must be updated to read the new `agent.scope` shape. Documented in `docs/adr/hitl-architecture.md` and `docs/hitl-integration-guide.md`.

## Sequenced follow-up

This refactor is sequenced to land in three reviewable PRs:

1. **PR 1** (foundation, ~5 commits, zero behavior change) — easy review; sets up the newtypes and the `globset` dep.
2. **PR 2** (HITL refactor, ~7 commits, behavior change) — the bulk of the work. Wire-format and TOML schema break here. Coordinate with the sandbox owner; update the docs in this PR.
3. **PR 3** (cross-cutting migration, separate) — replaces the `pub type` aliases and migrates non-HITL call sites.

The V2 HITL work (coordinator routing tool, parallel worker parking, centralized dispatcher) is unblocked by V1 but is not in scope for this refactor.

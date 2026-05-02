# Aura Self-Testing Strategy

Aura testing itself — using the agent platform to validate its own behavior, generate test coverage, and discover edge cases.

## Why Self-Test?

Aura is an agent composition platform. The most compelling proof of its capabilities is using it to solve real problems — starting with its own quality. Self-testing also:

- Dogfoods the TOML config, MCP tool execution, and streaming API
- Surfaces usability issues that only appear when building real agents
- Produces test artifacts that double as working examples for users

---

## Option 1: Aura as API Test Agent

An Aura agent configured with HTTP and assertion MCP tools that tests a running Aura instance's `/v1/chat/completions` endpoint — validating the API surface exactly as external clients experience it.

### How It Works

```
┌─────────────────┐         ┌─────────────────┐         ┌──────────────┐
│  Aura Test Agent │──HTTP──▶│  Aura Under Test │──MCP──▶│  Mock MCP    │
│  (tester)        │◀──SSE──│  (target)         │◀──────│  Server      │
└─────────────────┘         └─────────────────┘         └──────────────┘
```

The test agent:
1. Sends chat completion requests to the target Aura instance
2. Parses SSE streams, validates event ordering and content
3. Verifies tool execution lifecycle: `aura.tool_requested` → `aura.tool_start` → `aura.tool_complete`
4. Tests error paths (malformed input, tool failures, timeouts)
5. Tests cancellation (disconnect mid-stream, verify cleanup)
6. Reports pass/fail results with structured output

### Example Agent Configuration

```toml
# configs/self-test-agent.toml
[llm]
provider = "openai"
api_key = "{{ env.OPENAI_API_KEY }}"
model = "gpt-5.2"

[mcp]
sanitize_schemas = true

# HTTP client MCP server for making requests to the target Aura instance
[mcp.servers.http_client]
transport = "http_streamable"
url = "http://localhost:9100/mcp"
description = "HTTP client for testing Aura API endpoints — send requests, parse SSE, validate responses"

# Filesystem MCP server for reading test fixtures and writing results
[mcp.servers.filesystem]
transport = "http_streamable"
url = "http://localhost:9101/mcp"
description = "Read test fixtures, write test results and reports"

[agent]
name = "Aura API Test Agent"
system_prompt = """
You are a QA agent that tests the Aura API server at http://localhost:8080.

Your job is to execute test scenarios against the /v1/chat/completions endpoint
and report results. For each test:

1. Send the request using the http_client tools
2. Parse the response (JSON or SSE stream)
3. Validate against expected behavior
4. Report PASS/FAIL with details

Test categories:
- Streaming: SSE event format, token-by-token delivery, [DONE] termination
- Tool execution: tool_call → tool_result lifecycle, multi-turn chains
- Custom events: aura.tool_requested, aura.tool_start, aura.tool_complete
- Error handling: malformed requests, tool failures, timeout behavior
- Headers: header forwarding to MCP servers
- Cancellation: client disconnect propagation

Report results in structured JSON:
{"test": "name", "status": "PASS|FAIL", "details": "...", "duration_ms": N}
"""
temperature = 0.0
turn_depth = 10
```

### What It Tests

| Category | Test Scenarios |
|----------|---------------|
| **Streaming** | SSE format compliance, chunk ordering, `[DONE]` termination, backpressure |
| **Tool Lifecycle** | `tool_call` → execution → `tool_result`, multi-step chains, parallel tool calls |
| **Custom Events** | `aura.tool_requested/start/complete` emission, timing, payload structure |
| **Error Handling** | Malformed JSON, unknown model, tool failures (`failing_tool`), timeout expiry |
| **Header Forwarding** | Static headers, `headers_from_request` mapping, override precedence |
| **Cancellation** | Mid-stream disconnect, MCP `notifications/cancelled` delivery, resource cleanup |
| **Concurrency** | Simultaneous requests, session isolation, no cross-request contamination |

### GitHub CI Integration

This is the key advantage of Option 1 — the test agent can run as a **GitHub Actions workflow** triggered on PRs:

```yaml
# .github/workflows/aura-api-tests.yml
name: Aura API Tests (Self-Test)

on:
  pull_request:
    branches: [main]
    paths:
      - 'crates/**'
      - 'Cargo.toml'
      - 'Cargo.lock'

jobs:
  api-test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      # Start target Aura + mock MCP via Docker Compose
      - name: Start test infrastructure
        run: |
          docker compose -f compose/base.yml -f compose/test.yml up -d \
            aura-web-server mock-mcp
          # Wait for healthy
          timeout 90 bash -c 'until curl -sf http://localhost:8080/health; do sleep 2; done'

      # Run the Aura test agent against the target
      - name: Run API test agent
        env:
          OPENAI_API_KEY: ${{ secrets.OPENAI_API_KEY }}
        run: |
          # The test agent runs as a second Aura instance
          CONFIG_PATH=configs/self-test-agent.toml \
            cargo run --bin aura-web-server -- --port 8090 &
          TEST_AGENT_PID=$!

          # Execute test suite by sending prompts to the test agent
          python3 scripts/run-api-tests.py \
            --agent-url http://localhost:8090 \
            --target-url http://localhost:8080 \
            --output results/api-test-results.json

          kill $TEST_AGENT_PID

      # Post results as PR comment via gh CLI
      - name: Report results
        if: always()
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: |
          python3 scripts/format-test-report.py results/api-test-results.json > report.md
          gh pr comment ${{ github.event.pull_request.number }} --body-file report.md

      - name: Cleanup
        if: always()
        run: docker compose -f compose/base.yml -f compose/test.yml down
```

**Alternative: Lightweight gh CLI approach** — skip the second Aura instance entirely and use a script that sends curl requests to the target, parses SSE, and reports via `gh pr comment`:

```bash
# scripts/api-smoke-test.sh — runs as a PR check
#!/bin/bash
set -euo pipefail

TARGET_URL="${1:-http://localhost:8080}"
RESULTS=()

# Test 1: Health check
if curl -sf "$TARGET_URL/health" | jq -e '.status == "healthy"' > /dev/null; then
  RESULTS+=("PASS: Health endpoint")
else
  RESULTS+=("FAIL: Health endpoint")
fi

# Test 2: Non-streaming completion
RESPONSE=$(curl -sf "$TARGET_URL/v1/chat/completions" \
  -H "Content-Type: application/json" \
  -d '{"messages":[{"role":"user","content":"Say hello"}]}')
if echo "$RESPONSE" | jq -e '.choices[0].message.content' > /dev/null; then
  RESULTS+=("PASS: Non-streaming completion")
else
  RESULTS+=("FAIL: Non-streaming completion")
fi

# Test 3: Streaming completion
# ... SSE parsing and validation ...

# Output results
printf '%s\n' "${RESULTS[@]}"
```

### Pros and Cons

| Pros | Cons |
|------|------|
| Tests the real API as clients see it | Requires LLM API key (cost per run) |
| Catches integration issues across layers | Non-deterministic (LLM responses vary) |
| Exercises MCP tool calling as a side effect | Two Aura instances needed for full agent approach |
| Results post directly to PRs via `gh` CLI | Slower than unit tests (~minutes per suite) |
| Doubles as a living example of Aura usage | |

---

## Option 2: Aura as Test Case Generator (Multi-Agent Pipeline)

A multi-agent pipeline where one Aura agent generates Rust test code and four review agents evaluate it through different lenses — with an auto-fix loop that iterates until the tests are solid.

### How It Works

```
make test-generate [FILES="crates/aura/src/mcp.rs"]
  │
  ├─ 1. Identify targets
  │     (git diff vs main, or explicit FILES=)
  │
  ├─ 2. Generator Agent
  │     Reads source + existing tests → writes new tests to source tree
  │
  ├─ 3. cargo test
  │     Compile + run (fail fast if broken)
  │
  ├─ 4. Review Agents (parallel, 4 lenses)
  │     ├─ Correctness Agent
  │     ├─ Coverage Agent
  │     ├─ Robustness Agent
  │     └─ Style Agent
  │
  ├─ 5. Issues found?
  │     ├─ YES → Feed review back to Generator → goto step 3 (max 3 rounds)
  │     └─ NO  → Done
  │
  └─ 6. Terminal summary
        PASS/FAIL per file, review verdicts, iteration count
```

### Agent Configurations

Five Aura agents, each with a focused role. All share filesystem and shell MCP tools.

#### Generator Agent

```toml
# configs/test-agents/generator.toml
[llm]
provider = "anthropic"
api_key = "{{ env.ANTHROPIC_API_KEY }}"
model = "claude-sonnet-4-20250514"

[mcp]
sanitize_schemas = true

[mcp.servers.filesystem]
transport = "http_streamable"
url = "http://localhost:9101/mcp"
description = "Read Rust source files and write generated test code"

[mcp.servers.shell]
transport = "http_streamable"
url = "http://localhost:9102/mcp"
description = "Run cargo commands to compile and test generated code"

[agent]
name = "Test Generator"
system_prompt = """
You are a Rust test engineer for the Aura project. You generate test cases
for source files provided to you.

Workflow:
1. Read the target source file and any existing tests for it
2. Identify untested functions, branches, and edge cases
3. Generate test code following the project's conventions
4. Write tests to the correct location in the source tree
5. Run `cargo test` to verify compilation and correctness
6. If tests fail, fix them and re-run until green

Where to write tests:
- Unit tests: append to existing #[cfg(test)] mod tests { } in the source file
- If no test module exists, create one at the bottom of the source file
- Integration tests: crates/aura-web-server/tests/ (for HTTP/SSE behavior)

Conventions:
- #[tokio::test] for async functions
- Test names: test_<function>_<scenario>
- Test both happy paths and error cases
- Prefer real types over mocks
- One logical assertion per test
- Use aura-test-utils helpers where applicable
- Do NOT modify source code — only write test code

When given review feedback, apply the fixes precisely. Focus on the issues
flagged — do not rewrite tests that weren't flagged.
"""
temperature = 0.0
turn_depth = 20
```

#### Correctness Review Agent

```toml
# configs/test-agents/review-correctness.toml
[llm]
provider = "anthropic"
api_key = "{{ env.ANTHROPIC_API_KEY }}"
model = "claude-sonnet-4-20250514"

[mcp]
sanitize_schemas = true

[mcp.servers.filesystem]
transport = "http_streamable"
url = "http://localhost:9101/mcp"
description = "Read source files and generated test code for review"

[agent]
name = "Correctness Reviewer"
system_prompt = """
You review generated Rust tests for CORRECTNESS.

For each test file you are given, read both the source file and the test code.
Evaluate:

1. Does each test actually validate what its name claims?
2. Are assertions meaningful — not tautological (assert_eq!(x, x)) or vacuous?
3. Does the test exercise the real code path, or does it test a mock/stub?
4. Are setup conditions realistic — would this scenario actually occur?
5. If the function under test returned a wrong value, would the test catch it?
6. Are error cases testing the right error variant, not just "any error"?

Output format (JSON array):
[
  {
    "file": "path/to/test.rs",
    "test_name": "test_foo_returns_bar",
    "verdict": "PASS" | "FAIL",
    "issue": "description of the correctness problem (if FAIL)",
    "fix": "specific suggestion for how to fix it"
  }
]

Only flag genuine correctness issues. A test that is correct but could have
better coverage is NOT a correctness issue — that's the Coverage agent's job.
"""
temperature = 0.0
turn_depth = 5
```

#### Coverage & Edge Cases Review Agent

```toml
# configs/test-agents/review-coverage.toml
[llm]
provider = "anthropic"
api_key = "{{ env.ANTHROPIC_API_KEY }}"
model = "claude-sonnet-4-20250514"

[mcp]
sanitize_schemas = true

[mcp.servers.filesystem]
transport = "http_streamable"
url = "http://localhost:9101/mcp"
description = "Read source files and generated test code for review"

[agent]
name = "Coverage Reviewer"
system_prompt = """
You review generated Rust tests for COVERAGE and EDGE CASES.

For each test file, read the source implementation and evaluate what's missing:

1. Are all public functions tested?
2. Are error/failure paths covered (Result::Err, Option::None, panics)?
3. Boundary values: empty collections, zero, max values, single-element?
4. Are match arms and if/else branches all exercised?
5. For async code: cancellation, timeout, concurrent access?
6. For parsers: malformed input, missing fields, extra fields, wrong types?
7. For state machines: all transitions, invalid transitions, re-entry?

Output format (JSON array):
[
  {
    "file": "path/to/source.rs",
    "function": "function_name",
    "missing_scenario": "description of the untested case",
    "priority": "HIGH" | "MEDIUM" | "LOW",
    "suggested_test": "brief description of what the test should do"
  }
]

Focus on scenarios that would catch real bugs. Don't flag trivial getters
or simple delegations that can't meaningfully fail.
"""
temperature = 0.0
turn_depth = 5
```

#### Robustness & Flakiness Review Agent

```toml
# configs/test-agents/review-robustness.toml
[llm]
provider = "anthropic"
api_key = "{{ env.ANTHROPIC_API_KEY }}"
model = "claude-sonnet-4-20250514"

[mcp]
sanitize_schemas = true

[mcp.servers.filesystem]
transport = "http_streamable"
url = "http://localhost:9101/mcp"
description = "Read generated test code for robustness review"

[agent]
name = "Robustness Reviewer"
system_prompt = """
You review generated Rust tests for ROBUSTNESS and FLAKINESS risk.

For each test file, evaluate whether the tests will be reliable in CI:

1. Timing dependencies: Does the test depend on sleep(), wall-clock time,
   or specific execution ordering that could vary under load?
2. Port/resource conflicts: Does it bind to a hardcoded port that could
   collide with other tests running in parallel?
3. Filesystem side effects: Does it write to a shared location without cleanup?
4. Non-determinism: Does it depend on HashMap ordering, random values,
   or floating-point equality without epsilon?
5. External dependencies: Does it require a network call, running service,
   or environment variable that might not be set?
6. Test isolation: Could this test's state leak into or from another test?
7. Brittle assertions: Does it assert on error message strings, debug output,
   or formatting that could change without a real bug?

Output format (JSON array):
[
  {
    "file": "path/to/test.rs",
    "test_name": "test_foo",
    "risk": "HIGH" | "MEDIUM" | "LOW",
    "issue": "description of the flakiness/robustness concern",
    "fix": "specific suggestion"
  }
]

Only flag real risks. Deterministic unit tests with no I/O are inherently
robust — don't waste time reviewing those.
"""
temperature = 0.0
turn_depth = 5
```

#### Style & Convention Review Agent

```toml
# configs/test-agents/review-style.toml
[llm]
provider = "anthropic"
api_key = "{{ env.ANTHROPIC_API_KEY }}"
model = "claude-sonnet-4-20250514"

[mcp]
sanitize_schemas = true

[mcp.servers.filesystem]
transport = "http_streamable"
url = "http://localhost:9101/mcp"
description = "Read existing and generated test code for style review"

[agent]
name = "Style Reviewer"
system_prompt = """
You review generated Rust tests for STYLE and CONVENTION compliance.

Read existing tests in the project to learn the established patterns, then
evaluate the new tests against those patterns:

1. Naming: Do test names follow test_<function>_<scenario> convention?
2. Organization: Are unit tests in #[cfg(test)] mod tests {}? Integration
   tests in crates/aura-web-server/tests/?
3. Assertions: Does the project prefer assert_eq! vs assert!(matches!(...))?
   Are the generated tests consistent?
4. Async patterns: Is #[tokio::test] used correctly? Are timeouts handled
   the same way as existing tests?
5. Imports: Are they organized consistently with the rest of the codebase?
6. Test helpers: Are aura-test-utils helpers used where the existing tests
   use them, rather than reinventing inline?
7. Feature flags: Do integration tests use the correct #[cfg(feature = "...")]?

Output format (JSON array):
[
  {
    "file": "path/to/test.rs",
    "test_name": "test_foo",
    "issue": "description of the style deviation",
    "convention": "what the existing tests do instead",
    "fix": "specific change needed"
  }
]

Only flag meaningful deviations from established patterns. Minor formatting
differences that rustfmt will fix are not worth flagging.
"""
temperature = 0.0
turn_depth = 5
```

### Orchestration: `make test-generate`

The Makefile target supports two modes:
- **Changed files** (default): `make test-generate` — generates tests for files changed vs `main`
- **Explicit files**: `make test-generate FILES="crates/aura/src/mcp.rs crates/aura/src/config.rs"`

```makefile
# Added to Makefile

# Aura self-testing: multi-agent test generation pipeline
TEST_AGENT_CONFIGS := configs/test-agents
ORCHESTRATOR_SCRIPT := scripts/test-generate-orchestrate.sh
MAX_REVIEW_ROUNDS := 3

.PHONY: test-generate
test-generate::           ## Generate + review tests using Aura agents
	@if [ -n "$(FILES)" ]; then \
		echo "[aura] Generating tests for explicit files: $(FILES)"; \
		$(ORCHESTRATOR_SCRIPT) --files "$(FILES)" --max-rounds $(MAX_REVIEW_ROUNDS); \
	else \
		CHANGED=$$(git diff --name-only main -- 'crates/**/*.rs' | grep -v test); \
		if [ -z "$$CHANGED" ]; then \
			echo "[aura] No changed source files vs main. Use FILES= to target specific files."; \
			exit 0; \
		fi; \
		echo "[aura] Generating tests for changed files vs main:"; \
		echo "$$CHANGED" | sed 's/^/  /'; \
		$(ORCHESTRATOR_SCRIPT) --files "$$CHANGED" --max-rounds $(MAX_REVIEW_ROUNDS); \
	fi
```

### Orchestration Script

```bash
#!/usr/bin/env bash
# scripts/test-generate-orchestrate.sh
#
# Multi-agent test generation pipeline:
#   1. Generator agent writes tests
#   2. cargo test validates them
#   3. Four review agents evaluate in parallel
#   4. If issues found, feed back to generator (up to N rounds)
#   5. Print terminal summary

set -euo pipefail

# ── Parse args ───────────────────────────────────────────────────────
FILES=""
MAX_ROUNDS=3

while [[ $# -gt 0 ]]; do
  case $1 in
    --files)    FILES="$2"; shift 2 ;;
    --max-rounds) MAX_ROUNDS="$2"; shift 2 ;;
    *)          echo "Unknown arg: $1"; exit 1 ;;
  esac
done

if [ -z "$FILES" ]; then
  echo "No files specified." && exit 1
fi

# ── Config ───────────────────────────────────────────────────────────
AGENT_CONFIGS="configs/test-agents"
GENERATOR_PORT=8090
REVIEW_PORTS=(8091 8092 8093 8094)
REVIEW_NAMES=("correctness" "coverage" "robustness" "style")
REVIEW_CONFIGS=("review-correctness" "review-coverage" "review-robustness" "review-style")
PIDS=()

cleanup() {
  echo "[aura] Cleaning up agent processes..."
  for pid in "${PIDS[@]}"; do
    kill "$pid" 2>/dev/null || true
  done
}
trap cleanup EXIT

# ── Start MCP tool servers ───────────────────────────────────────────
echo "[aura] Starting MCP tool servers..."
# (filesystem on :9101, shell on :9102 — assumed running or started here)

# ── Start Generator Agent ────────────────────────────────────────────
echo "[aura] Starting generator agent on :${GENERATOR_PORT}..."
CONFIG_PATH="${AGENT_CONFIGS}/generator.toml" \
  cargo run --bin aura-web-server -- --port $GENERATOR_PORT &
PIDS+=($!)
sleep 3  # Wait for server readiness

# ── Generation + Review Loop ─────────────────────────────────────────
ROUND=0
ALL_CLEAR=false

while [ $ROUND -lt $MAX_ROUNDS ] && [ "$ALL_CLEAR" = false ]; do
  ROUND=$((ROUND + 1))
  echo ""
  echo "═══════════════════════════════════════════"
  echo "  Round $ROUND / $MAX_ROUNDS"
  echo "═══════════════════════════════════════════"

  # ── Step 1: Generate (or fix) tests ──────────────────────────────
  if [ $ROUND -eq 1 ]; then
    echo "[generator] Generating tests for: $FILES"
    PROMPT="Generate comprehensive tests for these source files: $FILES"
  else
    echo "[generator] Applying review feedback (round $ROUND)..."
    PROMPT="Apply the following review feedback to the tests you generated. Fix all issues, then run cargo test to verify.\n\nFeedback:\n$(cat /tmp/aura-review-feedback.json)"
  fi

  # Send prompt to generator agent
  curl -sf "http://localhost:${GENERATOR_PORT}/v1/chat/completions" \
    -H "Content-Type: application/json" \
    -d "{\"messages\":[{\"role\":\"user\",\"content\":\"$PROMPT\"}]}" \
    > /tmp/aura-generator-response.json

  # ── Step 2: cargo test ───────────────────────────────────────────
  echo "[test] Running cargo test..."
  if ! cargo test --workspace --lib 2>&1 | tee /tmp/aura-test-output.txt; then
    echo "[test] FAILED — feeding errors back to generator"
    # On test failure in early rounds, let the generator fix it
    if [ $ROUND -lt $MAX_ROUNDS ]; then
      echo "[{\"agent\":\"cargo-test\",\"issues\":[{\"verdict\":\"FAIL\",\"issue\":\"Compilation or test failure\",\"fix\":\"See test output\",\"output\":\"$(tail -50 /tmp/aura-test-output.txt | jq -Rs .)\"}]}]" \
        > /tmp/aura-review-feedback.json
      continue
    else
      echo "[test] FAILED on final round — manual intervention needed"
      exit 1
    fi
  fi
  echo "[test] All tests pass"

  # ── Step 3: Start review agents in parallel ──────────────────────
  echo "[review] Starting 4 review agents in parallel..."
  REVIEW_PIDS=()
  for i in "${!REVIEW_NAMES[@]}"; do
    PORT=${REVIEW_PORTS[$i]}
    NAME=${REVIEW_NAMES[$i]}
    CONFIG=${REVIEW_CONFIGS[$i]}

    CONFIG_PATH="${AGENT_CONFIGS}/${CONFIG}.toml" \
      cargo run --bin aura-web-server -- --port "$PORT" &
    REVIEW_PIDS+=($!)
    PIDS+=($!)
  done
  sleep 3  # Wait for review agents

  # ── Step 4: Send review requests in parallel ─────────────────────
  echo "[review] Reviewing generated tests..."
  REVIEW_RESULTS=()
  for i in "${!REVIEW_NAMES[@]}"; do
    PORT=${REVIEW_PORTS[$i]}
    NAME=${REVIEW_NAMES[$i]}

    curl -sf "http://localhost:${PORT}/v1/chat/completions" \
      -H "Content-Type: application/json" \
      -d "{\"messages\":[{\"role\":\"user\",\"content\":\"Review the tests in these files: $FILES\"}]}" \
      > "/tmp/aura-review-${NAME}.json" &
  done
  wait  # Wait for all reviews to complete

  # ── Step 5: Consolidate review results ───────────────────────────
  TOTAL_ISSUES=0
  FEEDBACK="["
  for NAME in "${REVIEW_NAMES[@]}"; do
    RESULT=$(cat "/tmp/aura-review-${NAME}.json" | jq -r '.choices[0].message.content // empty')
    ISSUE_COUNT=$(echo "$RESULT" | jq 'if type == "array" then [.[] | select(.verdict == "FAIL" or .risk == "HIGH" or .priority == "HIGH")] | length else 0 end' 2>/dev/null || echo 0)
    TOTAL_ISSUES=$((TOTAL_ISSUES + ISSUE_COUNT))
    FEEDBACK="${FEEDBACK}{\"agent\":\"${NAME}\",\"issues\":${RESULT}},"
  done
  FEEDBACK="${FEEDBACK%,}]"
  echo "$FEEDBACK" > /tmp/aura-review-feedback.json

  # Stop review agents
  for pid in "${REVIEW_PIDS[@]}"; do
    kill "$pid" 2>/dev/null || true
  done

  # ── Step 6: Check if we're clear ─────────────────────────────────
  if [ "$TOTAL_ISSUES" -eq 0 ]; then
    ALL_CLEAR=true
    echo "[review] All clear — no issues found"
  else
    echo "[review] Found $TOTAL_ISSUES issues across all lenses"
    if [ $ROUND -lt $MAX_ROUNDS ]; then
      echo "[review] Feeding back to generator for round $((ROUND + 1))..."
    fi
  fi
done

# ── Terminal Summary ─────────────────────────────────────────────────
echo ""
echo "═══════════════════════════════════════════"
echo "  Test Generation Summary"
echo "═══════════════════════════════════════════"
echo "  Files:       $FILES"
echo "  Rounds:      $ROUND / $MAX_ROUNDS"
echo "  cargo test:  PASS"
if [ "$ALL_CLEAR" = true ]; then
  echo "  Reviews:     ALL CLEAR"
else
  echo "  Reviews:     Issues remain (see details above)"
fi
echo ""

# Per-lens summary
for NAME in "${REVIEW_NAMES[@]}"; do
  RESULT=$(cat "/tmp/aura-review-${NAME}.json" 2>/dev/null | jq -r '.choices[0].message.content // "N/A"')
  PASS_COUNT=$(echo "$RESULT" | jq 'if type == "array" then [.[] | select(.verdict == "PASS")] | length else 0 end' 2>/dev/null || echo "?")
  FAIL_COUNT=$(echo "$RESULT" | jq 'if type == "array" then [.[] | select(.verdict == "FAIL" or .risk == "HIGH" or .priority == "HIGH")] | length else 0 end' 2>/dev/null || echo "?")
  printf "  %-14s  PASS: %s  ISSUES: %s\n" "$NAME" "$PASS_COUNT" "$FAIL_COUNT"
done
echo "═══════════════════════════════════════════"
```

### Example Terminal Output

```
$ make test-generate FILES="crates/aura/src/tool_event_broker.rs"
[aura] Generating tests for explicit files: crates/aura/src/tool_event_broker.rs

═══════════════════════════════════════════
  Round 1 / 3
═══════════════════════════════════════════
[generator] Generating tests for: crates/aura/src/tool_event_broker.rs
[test] Running cargo test...
[test] All tests pass
[review] Starting 4 review agents in parallel...
[review] Reviewing generated tests...
[review] Found 3 issues across all lenses

═══════════════════════════════════════════
  Round 2 / 3
═══════════════════════════════════════════
[generator] Applying review feedback (round 2)...
[test] Running cargo test...
[test] All tests pass
[review] Starting 4 review agents in parallel...
[review] Reviewing generated tests...
[review] All clear — no issues found

═══════════════════════════════════════════
  Test Generation Summary
═══════════════════════════════════════════
  Files:       crates/aura/src/tool_event_broker.rs
  Rounds:      2 / 3
  cargo test:  PASS
  Reviews:     ALL CLEAR

  correctness     PASS: 8  ISSUES: 0
  coverage        PASS: 6  ISSUES: 0
  robustness      PASS: 8  ISSUES: 0
  style           PASS: 8  ISSUES: 0
═══════════════════════════════════════════
```

### What It Generates

| Source Module | Generated Tests |
|---------------|-----------------|
| `provider_agent.rs` | Provider type erasure, streaming dispatch, error propagation |
| `tool_event_broker.rs` | FIFO ordering, concurrent access, edge cases (empty queue, duplicate IDs) |
| `request_cancellation.rs` | Token lifecycle, cleanup on drop, concurrent cancel signals |
| `mcp_response.rs` | Response parsing, malformed JSON, content type variants |
| `schema_sanitize.rs` | anyOf handling, missing types, nested schema fixes |
| `config.rs` | Env interpolation, missing fields, invalid TOML, type coercion |
| `stream_events.rs` | Event serialization, custom event payloads, edge cases |

### Pros and Cons

| Pros | Cons |
|------|------|
| Generated tests are deterministic — no LLM at runtime | LLM cost per generation cycle |
| Multi-lens review catches issues a single pass would miss | 5 agents = 5x LLM cost per round |
| Auto-fix loop means less manual intervention | Max 3 rounds may not resolve all issues |
| Tests persist as regular Rust code in the repo | Requires filesystem + shell MCP servers |
| `make test-generate` fits naturally in dev workflow | First run requires MCP server setup |
| Compounds over time — coverage grows with each session | |

---

## Option 3: Aura as Chaos/Scenario Agent

An Aura agent that generates adversarial and edge-case inputs, fires them at Aura's API, and reports failures — leveraging LLM creativity to find issues humans wouldn't think to test.

### How It Works

```
┌──────────────────────┐         ┌─────────────────┐
│  Aura Chaos Agent    │──HTTP──▶│  Aura Under Test │
│  (adversarial input) │◀──SSE──│  (target)         │
└──────────────────────┘         └─────────────────┘
         │
         ▼
   ┌─────────────────┐
   │  Failure Report  │
   │  (reproducible)  │
   └─────────────────┘
```

The chaos agent:
1. Generates creative adversarial inputs (malformed JSON, boundary values, Unicode edge cases, injection attempts)
2. Sends them to the target Aura instance
3. Monitors for crashes, hangs, unexpected errors, or data leaks
4. Captures reproducible failure cases with exact request/response payloads
5. Generates regression test cases from discovered failures (feeds into Option 2)

### Example Agent Configuration

```toml
# configs/chaos-test-agent.toml
[llm]
provider = "anthropic"
api_key = "{{ env.ANTHROPIC_API_KEY }}"
model = "claude-sonnet-4-20250514"

[mcp]
sanitize_schemas = true

[mcp.servers.http_client]
transport = "http_streamable"
url = "http://localhost:9100/mcp"
description = "HTTP client for sending adversarial requests to the target Aura API"

[mcp.servers.filesystem]
transport = "http_streamable"
url = "http://localhost:9101/mcp"
description = "Write failure reports and reproducible test cases"

[agent]
name = "Aura Chaos Agent"
system_prompt = """
You are a chaos testing agent. Your goal is to find bugs, crashes, and
unexpected behavior in the Aura API server at http://localhost:8080.

Attack categories to explore:
1. Malformed requests: invalid JSON, missing fields, wrong types, extra fields
2. Boundary values: empty strings, huge payloads, zero/negative numbers, max integers
3. Unicode and encoding: null bytes, RTL characters, emoji, surrogate pairs, multi-byte
4. Injection: prompt injection in messages, SQL-like strings, shell metacharacters
5. Protocol abuse: invalid SSE parsing, premature disconnects, rapid reconnects
6. Concurrency: simultaneous requests with same session_id, rapid fire
7. Resource exhaustion: very long messages, deep conversation history, max turn_depth
8. Header manipulation: missing content-type, conflicting headers, oversized headers

For each test:
1. Design the adversarial input with a clear hypothesis of what might break
2. Send the request and capture the full response
3. Classify the result: PASS (handled gracefully), FAIL (crash/hang/unexpected)
4. For failures, write a reproducible test case to /tmp/chaos-results/

Be creative. Think about what a malicious or buggy client might send.
"""
temperature = 0.7  # Higher creativity for diverse attack patterns
turn_depth = 15
```

### Attack Scenarios

| Category | Example Scenarios |
|----------|-------------------|
| **Malformed Requests** | Missing `messages` field, `messages: null`, `messages: "string"`, nested 1000 levels deep |
| **Boundary Values** | `max_tokens: 0`, `max_tokens: 999999999`, `temperature: -1`, empty message content |
| **Unicode Edge Cases** | Null bytes in message content, 10MB emoji string, mixed RTL/LTR, lone surrogates |
| **Protocol Abuse** | Request with `stream: true` then immediately disconnect, 100 concurrent streams to same session |
| **Resource Exhaustion** | Conversation with 10,000 messages in history, single message with 1M characters |
| **Header Attacks** | 100KB `Authorization` header, null bytes in header values, duplicate `Content-Type` |

### Output: Failure Pipeline

Discovered failures automatically feed back into the development process:

```
Chaos Agent finds failure
    ↓
Writes reproducible curl command + expected vs actual behavior
    ↓
Developer reviews failure report
    ↓
Option 2 (Test Generator) creates a regression test from the failure
    ↓
Fix lands with test coverage — failure can never recur
```

### Pros and Cons

| Pros | Cons |
|------|------|
| Finds edge cases humans wouldn't think of | Non-deterministic — different bugs each run |
| LLM creativity generates diverse attack patterns | Harder to reproduce exact failures |
| Discovered failures become regression tests | Requires running Aura instance + LLM API |
| Great for hardening before releases | Can generate false positives (expected errors flagged as failures) |
| No upfront test authoring — agent explores freely | Needs human triage of results |

---

## Comparison Matrix

| | Option 1: API Test Agent | Option 2: Test Generator | Option 3: Chaos Agent |
|---|---|---|---|
| **Purpose** | Validate API behavior | Expand test coverage | Find unknown edge cases |
| **Agents** | 1 (tester) | 5 (1 generator + 4 reviewers) | 1 (chaos) |
| **Runtime LLM** | Yes (every run) | No (generate once, run forever) | Yes (every run) |
| **Trigger** | GitHub Actions PR check | `make test-generate` (on-demand) | Periodic/pre-release |
| **Deterministic** | No | Yes (after generation) | No |
| **Cost per run** | Medium (LLM + compute) | Zero (`cargo test`) | Medium (LLM + compute) |
| **Cost to generate** | N/A | Medium (5 agents x up to 3 rounds) | N/A |
| **Best for** | Regression, API contract | Coverage gaps, new code | Security, robustness |
| **Feeds into** | PR comments, status checks | Committed test code | Option 2 (regression tests) |

## Recommended Rollout

1. **Phase 1** — Option 2 (Test Generator): Highest ROI. `make test-generate` gives developers a non-abrasive, opt-in tool that produces persistent tests. Start with the modules that have the lowest coverage. Once the multi-agent pipeline is proven, the generated tests run in CI for free forever.
2. **Phase 2** — Option 1 (API Test Agent): Add as a GitHub Actions PR gate. Validates the full API contract on every PR. Complements Option 2 by testing the assembled system, not just individual units.
3. **Phase 3** — Option 3 (Chaos Agent): Run periodically (weekly or pre-release) to harden the system. Feed discovered failures back into Option 2's generator to create regression tests automatically.

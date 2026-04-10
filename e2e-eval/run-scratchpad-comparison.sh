#!/usr/bin/env bash
# Scratchpad E2E comparison — tests scratchpad interception and exploration tool usage
#
# Usage:
#   ./e2e-eval/run-scratchpad-comparison.sh <iterations> <config1> [config2 ...]
#
# Starts scratchpad-test-mcp automatically, then starts/stops aura server per config.
# Model name is derived from the config filename (minus scratchpad- prefix and .toml suffix).
#
# Auto-detects cloud vs local configs — local models get smaller prompts.
#
# Examples:
#   # Cloud models (1 iteration)
#   ./e2e-eval/run-scratchpad-comparison.sh 1 configs/scratchpad-gpt5.toml configs/scratchpad-sonnet.toml
#
#   # Local models (3 iterations)
#   ./e2e-eval/run-scratchpad-comparison.sh 3 configs/scratchpad-glm.toml configs/scratchpad-qwen3.toml
#
# Prerequisites: cargo build --release, Docker (for scratchpad-test-mcp)
set -euo pipefail

if [[ $# -lt 2 ]]; then
  echo "Usage: $0 <iterations> <config1.toml> [config2.toml ...]" >&2
  exit 1
fi

ITERATIONS="$1"; shift
CONFIGS=("$@")

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BINARY="$PROJECT_DIR/target/release/aura-web-server"
PARSE_SCRIPT="$PROJECT_DIR/e2e-eval/parse-results.py"
ASSERT_SCRIPT="$PROJECT_DIR/e2e-eval/eval-assertions.py"
RESULTS_DIR="$PROJECT_DIR/e2e-eval/scratchpad-results-$(date +%Y%m%d-%H%M%S)"
PORT=8090

if [[ ! -x "$BINARY" ]]; then
  echo "ERROR: Release binary not found. Run: cargo build --release" >&2
  exit 1
fi

# Derive model name from config path: configs/scratchpad-glm.toml -> glm
model_name_from_config() {
  local base
  base="$(basename "$1" .toml)"
  echo "${base#scratchpad-}"
}

# Detect if config is for a local model (uses localhost:11435)
is_local_config() {
  grep -q 'base_url.*1143[45]' "$1" 2>/dev/null
}

mkdir -p "$RESULTS_DIR"

# ── Prompt Sets ────────────────────────────────────────────────────

CLOUD_PROMPTS=(
  "Retrieve the inventory report (use sp_inventory_report with size=10000) and tell me the exact price of the item with SKU 'SKU-007'"
  "Retrieve the inventory report (use sp_inventory_report with size=10000) and tell me the total out_of_stock_count from the summary"
  "Retrieve the log analysis (use sp_log_analysis with size=8000) and find the error code for the auth-service authentication failure"
  "Retrieve the cluster status (use sp_cluster_status with size=6000) and tell me the CPU percentage of the node with status NotReady"
  "Retrieve the small JSON dataset (use sp_get_small_json) and tell me the count value"
)
CLOUD_LABELS=(
  "sp-item-price"
  "sp-out-of-stock"
  "sp-log-error"
  "sp-node-cpu"
  "sp-passthrough"
)

LOCAL_PROMPTS=(
  "Retrieve the inventory report (use sp_inventory_report with size=5000) and tell me the exact price of the item with SKU 'SKU-007'"
  "Retrieve the log analysis (use sp_log_analysis with size=4000) and find the error code for the auth-service authentication failure"
  "Retrieve the small JSON dataset (use sp_get_small_json) and tell me the count value"
)
LOCAL_LABELS=(
  "sp-item-price"
  "sp-log-error"
  "sp-passthrough"
)

# ── Helpers ─────────────────────────────────────────────────────────
SERVER_PID=""
MCP_CONTAINER=""

cleanup() {
  stop_server
  echo ""
  echo "Stopping scratchpad-test-mcp..."
  if [[ -n "$MCP_CONTAINER" ]]; then
    docker stop "$MCP_CONTAINER" >/dev/null 2>&1 || true
    docker rm "$MCP_CONTAINER" >/dev/null 2>&1 || true
  fi
}

wait_for_server() {
  local max_wait=30 count=0
  while ! curl -sf "http://localhost:${PORT}/health" >/dev/null 2>&1; do
    sleep 1
    count=$((count + 1))
    if [[ $count -ge $max_wait ]]; then
      echo "  ERROR: Server did not start within ${max_wait}s" >&2
      return 1
    fi
  done
}

stop_server() {
  if [[ -n "${SERVER_PID}" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  SERVER_PID=""
}
trap cleanup EXIT

run_prompt() {
  local model_name="$1" prompt="$2" label="$3" iter="$4"
  local outdir="$RESULTS_DIR/$model_name/iter-$iter"
  mkdir -p "$outdir"

  local start_ms end_ms elapsed_ms http_code
  start_ms=$(python3 -c 'import time; print(int(time.time()*1000))')

  http_code=$(curl -s -o "$outdir/${label}.sse" -w '%{http_code}' \
    --max-time 240 \
    -X POST "http://localhost:${PORT}/v1/chat/completions" \
    -H "Content-Type: application/json" \
    -d "$(jq -n --arg p "$prompt" '{
      model: null,
      stream: true,
      messages: [{role: "user", content: $p}]
    }')" 2>"$outdir/${label}.curl-err" || true)

  end_ms=$(python3 -c 'import time; print(int(time.time()*1000))')
  elapsed_ms=$((end_ms - start_ms))

  # Count tool calls from SSE event lines
  local tool_calls=0 timeout_flag=0
  if [[ -f "$outdir/${label}.sse" ]]; then
    tool_calls=$(grep -cE '^event: aura\.orchestrator\.tool_call_completed|^event: aura\.tool_complete' \
      "$outdir/${label}.sse" 2>/dev/null || true)
    tool_calls="${tool_calls:-0}"
  fi

  # Timeout: check curl error and SSE content
  if [[ -f "$outdir/${label}.curl-err" ]] && grep -q "timed out" "$outdir/${label}.curl-err" 2>/dev/null; then
    timeout_flag=1
  fi

  local status="ok"
  [[ "$http_code" != "200" ]] && status="HTTP-$http_code"
  [[ "$timeout_flag" -gt 0 ]] && status="TIMEOUT"

  local completed="no"
  grep -q '"finish_reason":"stop"' "$outdir/${label}.sse" 2>/dev/null && completed="yes"

  # Scratchpad metrics from SSE (macOS-compatible, no -P flag)
  local sp_intercepted=0 sp_extracted=0
  if [[ -f "$outdir/${label}.sse" ]]; then
    sp_intercepted=$(python3 -c "
import json, sys
for line in open('$outdir/${label}.sse'):
    if '\"tokens_intercepted\"' in line and line.startswith('data:'):
        d = json.loads(line[5:])
        print(d.get('tokens_intercepted', 0))
        sys.exit()
print(0)" 2>/dev/null || echo 0)
    sp_extracted=$(python3 -c "
import json, sys
for line in open('$outdir/${label}.sse'):
    if '\"tokens_extracted\"' in line and line.startswith('data:'):
        d = json.loads(line[5:])
        print(d.get('tokens_extracted', 0))
        sys.exit()
print(0)" 2>/dev/null || echo 0)
  fi
  sp_intercepted="${sp_intercepted:-0}"
  sp_extracted="${sp_extracted:-0}"

  echo "$model_name,$label,$iter,$elapsed_ms,$tool_calls,$timeout_flag,$status,$completed,$sp_intercepted,$sp_extracted"
}

# ── Start scratchpad-test-mcp ──────────────────────────────────────
echo "Building and starting scratchpad-test-mcp..."

# Build the Docker image from the test MCP server
docker build -t scratchpad-test-mcp:e2e \
  "$PROJECT_DIR/tests/integration/scratchpad-mcp" -q >/dev/null 2>&1

MCP_CONTAINER="scratchpad-mcp-e2e-$$"
docker run -d --name "$MCP_CONTAINER" -p 8083:8083 scratchpad-test-mcp:e2e >/dev/null 2>&1

# Wait for MCP health
echo "Waiting for scratchpad-test-mcp..."
mcp_wait=30
while ! curl -sf -X POST http://localhost:8083/mcp \
  -H "Content-Type: application/json" \
  -H "Accept: application/json, text/event-stream" \
  -d '{"jsonrpc":"2.0","method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"0.1"}},"id":1}' \
  >/dev/null 2>&1; do
  sleep 1
  mcp_wait=$((mcp_wait - 1))
  if [[ $mcp_wait -le 0 ]]; then
    echo "ERROR: scratchpad-test-mcp failed to start" >&2
    exit 1
  fi
done
echo "scratchpad-test-mcp is running."
echo ""

# ── Validate configs ───────────────────────────────────────────────
MODEL_NAMES=()
for config in "${CONFIGS[@]}"; do
  if [[ ! -f "$config" ]]; then
    echo "ERROR: Config not found: $config" >&2
    exit 1
  fi
  MODEL_NAMES+=("$(model_name_from_config "$config")")
done

# ── CSV header ──────────────────────────────────────────────────────
CSV="$RESULTS_DIR/results.csv"
echo "model,prompt,iteration,elapsed_ms,tool_calls,timeouts,status,completed,sp_intercepted,sp_extracted" > "$CSV"

echo "============================================="
echo "  Scratchpad E2E Comparison"
echo "  Iterations: $ITERATIONS"
echo "  Models: ${MODEL_NAMES[*]}"
echo "  Results: $RESULTS_DIR"
echo "============================================="
echo ""

# ── Run ─────────────────────────────────────────────────────────────
for idx in "${!CONFIGS[@]}"; do
  config="${CONFIGS[$idx]}"
  model_name="${MODEL_NAMES[$idx]}"

  # Select prompt set based on config type
  if is_local_config "$config"; then
    PROMPTS=("${LOCAL_PROMPTS[@]}")
    PROMPT_LABELS=("${LOCAL_LABELS[@]}")
    echo "--- $model_name (local, ${#PROMPTS[@]} prompts) ---"
  else
    PROMPTS=("${CLOUD_PROMPTS[@]}")
    PROMPT_LABELS=("${CLOUD_LABELS[@]}")
    echo "--- $model_name (cloud, ${#PROMPTS[@]} prompts) ---"
  fi

  stop_server
  mkdir -p "$RESULTS_DIR/$model_name"
  CONFIG_PATH="$PROJECT_DIR/$config" PORT=$PORT \
    AURA_CUSTOM_EVENTS=true \
    AURA_EMIT_REASONING=true \
    AURA_PROMPT_JOURNAL=1 \
    RUST_LOG=aura=info,aura_web_server=info \
    "$BINARY" > "$RESULTS_DIR/$model_name/server.log" 2>&1 &
  SERVER_PID=$!

  if ! wait_for_server; then
    echo "  SKIP: server failed to start"
    stop_server
    continue
  fi

  for iter in $(seq 1 "$ITERATIONS"); do
    for i in "${!PROMPTS[@]}"; do
      prompt="${PROMPTS[$i]}"
      label="${PROMPT_LABELS[$i]}"
      result=$(run_prompt "$model_name" "$prompt" "$label" "$iter")
      echo "$result" >> "$CSV"

      # Extract key metrics for display
      elapsed=$(echo "$result" | cut -d',' -f4)
      tools=$(echo "$result" | cut -d',' -f5)
      sp_int=$(echo "$result" | cut -d',' -f9)
      sp_ext=$(echo "$result" | cut -d',' -f10)
      status=$(echo "$result" | cut -d',' -f7)
      echo "  [$iter/$ITERATIONS] $label: ${elapsed}ms, ${tools} tools, sp=${sp_int}→${sp_ext}, $status"
    done
  done

  echo ""
done

stop_server

# ── Parse results ──────────────────────────────────────────────────
echo "--- Parsing results ---"
if [[ -f "$PARSE_SCRIPT" ]]; then
  python3 "$PARSE_SCRIPT" "$RESULTS_DIR"
fi

echo ""
echo "--- Running assertions ---"
if [[ -f "$ASSERT_SCRIPT" ]]; then
  python3 "$ASSERT_SCRIPT" "$RESULTS_DIR" --prompt-set scratchpad || true
fi

echo ""
echo "Results: $RESULTS_DIR"

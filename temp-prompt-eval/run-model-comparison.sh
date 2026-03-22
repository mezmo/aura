#!/usr/bin/env bash
# E2E model comparison — captures timing, tool calls, timeouts, and completion stats
#
# Usage:
#   ./temp-prompt-eval/run-model-comparison.sh <iterations> <config1> [config2 ...]
#
# Starts math-mcp automatically, then starts/stops aura server per config.
# Model name is derived from the config filename (minus math-orchestration- prefix and .toml suffix).
#
# Environment:
#   PROMPT_SET=independent (default) — standalone math prompts
#   PROMPT_SET=dependent — chained prompts where each turn builds on the prior
#
# Examples:
#   # Run 3 iterations on two configs (independent prompts)
#   ./temp-prompt-eval/run-model-comparison.sh 3 configs/math-orchestration-glm.toml configs/math-orchestration-qwen3.toml
#
#   # Run 1 iteration with dependent prompts
#   PROMPT_SET=dependent ./temp-prompt-eval/run-model-comparison.sh 1 configs/math-orchestration-opus-bedrock.toml
#
# Prerequisites: cargo build --release, llama-server reachable on 11435 (for local models)
set -euo pipefail

if [[ $# -lt 2 ]]; then
  echo "Usage: $0 <iterations> <config1.toml> [config2.toml ...]" >&2
  exit 1
fi

ITERATIONS="$1"; shift
CONFIGS=("$@")

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BINARY="$PROJECT_DIR/target/release/aura-web-server"
PARSE_SCRIPT="$PROJECT_DIR/temp-prompt-eval/parse-results.py"
RESULTS_DIR="$PROJECT_DIR/temp-prompt-eval/results-$(date +%Y%m%d-%H%M%S)"
PORT=8090

if [[ ! -x "$BINARY" ]]; then
  echo "ERROR: Release binary not found. Run: cargo build --release" >&2
  exit 1
fi

# Derive model name from config path: configs/math-orchestration-glm.toml -> glm
model_name_from_config() {
  local base
  base="$(basename "$1" .toml)"
  echo "${base#math-orchestration-}"
}

mkdir -p "$RESULTS_DIR"

# ── Test Prompts ────────────────────────────────────────────────────
PROMPT_SET="${PROMPT_SET:-independent}"

if [[ "$PROMPT_SET" == "dependent" ]]; then
  PROMPTS=(
    "Calculate the mean of [12, 24, 36, 48]"
    "Take the result from my previous question (the mean) and multiply it by 4"
    "Subtract 20 from that multiplication result, then compute the sine of that many degrees"
    "Compute the median of these three results from our conversation: the original mean, the multiplication result, and the subtraction result"
    "Add the median you just computed to 50, then find the maximum of that sum and 200"
  )
  PROMPT_LABELS=(
    "t1-mean-baseline"
    "t2-multiply-prior"
    "t3-subtract-sin"
    "t4-median-three"
    "t5-add-max"
  )
else
  PROMPTS=(
    "What is 2 + 2?"
    "Calculate the mean of [10, 20, 30] then multiply the result by 3"
    "What is sin(45 degrees)?"
    "Add 15 and 27, then find the mean of the result and 100"
    "Calculate 7 * 8, then subtract 6, then find the median of [result, 10, 25, 50]"
  )
  PROMPT_LABELS=(
    "direct-add"
    "mean-then-multiply"
    "trig-sin45"
    "add-then-mean"
    "multi-step-median"
  )
fi

# ── Helpers ─────────────────────────────────────────────────────────
SERVER_PID=""

cleanup() {
  stop_server
  echo ""
  echo "Stopping math-mcp..."
  docker compose -f "$PROJECT_DIR/compose/base.yml" -f "$PROJECT_DIR/compose/orchestration.yml" \
    stop math-mcp >/dev/null 2>&1 || true
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
    --max-time 180 \
    -X POST "http://localhost:${PORT}/v1/chat/completions" \
    -H "Content-Type: application/json" \
    -d "$(jq -n --arg p "$prompt" '{
      model: "unused",
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

  echo "$model_name,$label,$iter,$elapsed_ms,$tool_calls,$timeout_flag,$status,$completed"
}

# ── Start math-mcp ─────────────────────────────────────────────────
echo "Starting math-mcp..."
docker compose -f "$PROJECT_DIR/compose/base.yml" -f "$PROJECT_DIR/compose/orchestration.yml" \
  up math-mcp -d --wait 2>&1 | grep -v "^$" || true

# Verify math-mcp is healthy
if ! docker compose -f "$PROJECT_DIR/compose/base.yml" -f "$PROJECT_DIR/compose/orchestration.yml" \
  ps math-mcp 2>/dev/null | grep -q "running\|Up"; then
  echo "ERROR: math-mcp failed to start" >&2
  exit 1
fi
echo "math-mcp is running."
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
echo "model,prompt,iteration,elapsed_ms,tool_calls,timeouts,status,completed" > "$CSV"

echo "============================================="
echo "  Model Comparison E2E"
echo "  Iterations: $ITERATIONS"
echo "  Models: ${MODEL_NAMES[*]}"
echo "  Results: $RESULTS_DIR"
echo "============================================="
echo ""

# ── Run ─────────────────────────────────────────────────────────────
for idx in "${!CONFIGS[@]}"; do
  config="${CONFIGS[$idx]}"
  model_name="${MODEL_NAMES[$idx]}"
  echo "--- $model_name ($config) ---"

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

      elapsed=$(echo "$result" | cut -d, -f4)
      tools=$(echo "$result" | cut -d, -f5)
      status=$(echo "$result" | cut -d, -f7)
      printf "  [iter %d] %-20s %6dms  tools=%-3s %s\n" "$iter" "$label" "$elapsed" "$tools" "$status"
    done
  done

  stop_server
  echo ""
done

# ── Summary (parse from SSE files for accuracy) ────────────────────
echo ""
python3 "$PARSE_SCRIPT" "$RESULTS_DIR" --csv

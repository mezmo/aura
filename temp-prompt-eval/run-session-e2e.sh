#!/usr/bin/env bash
# Multi-turn session E2E — sends Q1..Q5 in a single session to test session history injection
#
# Usage:
#   ./temp-prompt-eval/run-session-e2e.sh <config1> [config2 ...]
#
# Each config gets a fresh server instance. All 5 prompts share a single session ID,
# building up a conversation history that the coordinator should see in later turns.
#
# Environment:
#   PROMPT_SET=independent (default) — standalone math prompts
#   PROMPT_SET=dependent — chained prompts where each turn builds on the prior
#
# Prerequisites: cargo build --release, llama-server reachable on 11435 (for local models)
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "Usage: $0 <config1.toml> [config2.toml ...]" >&2
  exit 1
fi

CONFIGS=("$@")

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BINARY="$PROJECT_DIR/target/release/aura-web-server"
RESULTS_DIR="$PROJECT_DIR/temp-prompt-eval/session-results-$(date +%Y%m%d-%H%M%S)"
PORT=8090

if [[ ! -x "$BINARY" ]]; then
  echo "ERROR: Release binary not found. Run: cargo build --release" >&2
  exit 1
fi

model_name_from_config() {
  local base
  base="$(basename "$1" .toml)"
  echo "${base#math-orchestration-}"
}

mkdir -p "$RESULTS_DIR"

# ── Prompts ────────────────────────────────────────────────────────
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
    "q1-direct-add"
    "q2-mean-then-multiply"
    "q3-trig-sin45"
    "q4-add-then-mean"
    "q5-multi-step-median"
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

# Send a single prompt in a session, building up chat history
run_session_prompt() {
  local model_name="$1" prompt="$2" label="$3" session_id="$4" history_json="$5"
  local outdir="$RESULTS_DIR/$model_name"
  mkdir -p "$outdir"

  local start_ms end_ms elapsed_ms http_code
  start_ms=$(python3 -c 'import time; print(int(time.time()*1000))')

  # Build messages array: history + new user message
  local messages_json
  messages_json=$(python3 -c "
import json, sys
history = json.loads(sys.argv[1])
history.append({'role': 'user', 'content': sys.argv[2]})
print(json.dumps(history))
" "$history_json" "$prompt")

  local body
  body=$(jq -n \
    --arg sid "$session_id" \
    --argjson msgs "$messages_json" \
    '{
      model: null,
      stream: true,
      messages: $msgs,
      metadata: { chat_session_id: $sid }
    }')

  http_code=$(curl -s -o "$outdir/${label}.sse" -w '%{http_code}' \
    --max-time 180 \
    -X POST "http://localhost:${PORT}/v1/chat/completions" \
    -H "Content-Type: application/json" \
    -d "$body" 2>"$outdir/${label}.curl-err" || true)

  end_ms=$(python3 -c 'import time; print(int(time.time()*1000))')
  elapsed_ms=$((end_ms - start_ms))

  # Count tool calls
  local tool_calls=0
  if [[ -f "$outdir/${label}.sse" ]]; then
    tool_calls=$(grep -cE '^event: aura\.orchestrator\.tool_call_completed|^event: aura\.tool_complete' \
      "$outdir/${label}.sse" 2>/dev/null || true)
    tool_calls="${tool_calls:-0}"
  fi

  local status="ok"
  [[ "$http_code" != "200" ]] && status="HTTP-$http_code"

  local completed="no"
  grep -q '"finish_reason":"stop"' "$outdir/${label}.sse" 2>/dev/null && completed="yes"

  # Extract assistant response text from SSE for history
  local assistant_text
  assistant_text=$(python3 -c "
import sys, json
text = ''
for line in open(sys.argv[1]):
    if line.startswith('data: ') and not line.startswith('data: [DONE]'):
        try:
            d = json.loads(line[6:])
            delta = d.get('choices', [{}])[0].get('delta', {})
            t = delta.get('content', '')
            if t:
                text += t
        except: pass
print(text[:500])
" "$outdir/${label}.sse" 2>/dev/null || echo "(parse error)")

  # Check if session history was injected (look for the event or preamble marker)
  local session_ctx="none"
  if grep -q "Session History" "$outdir/${label}.sse" 2>/dev/null; then
    session_ctx="visible-in-sse"
  fi

  # Check persistence directory for planning prompt containing session history
  local memory_dir
  memory_dir=$(grep -o 'memory_dir = "[^"]*"' "$RESULTS_DIR/$model_name/config-info.txt" 2>/dev/null | cut -d'"' -f2 || echo "")

  printf "  %-25s %6dms  tools=%-2s %-4s" "$label" "$elapsed_ms" "$tool_calls" "$status"
  if [[ "$completed" == "yes" ]]; then
    echo " ✓"
  else
    echo " ✗"
  fi

  # Return assistant text for building history (via temp file)
  echo "$assistant_text" > "$outdir/${label}.response.txt"
}

# ── Start math-mcp ─────────────────────────────────────────────────
echo "Starting math-mcp..."
docker compose -f "$PROJECT_DIR/compose/base.yml" -f "$PROJECT_DIR/compose/orchestration.yml" \
  up math-mcp -d --wait 2>&1 | grep -v "^$" || true
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

echo "============================================="
echo "  Multi-Turn Session E2E"
echo "  Models: ${MODEL_NAMES[*]}"
echo "  Prompts: ${#PROMPTS[@]} (sequential, same session)"
echo "  Results: $RESULTS_DIR"
echo "============================================="
echo ""

# ── Run each config ───────────────────────────────────────────────
for idx in "${!CONFIGS[@]}"; do
  config="${CONFIGS[$idx]}"
  model="${MODEL_NAMES[$idx]}"
  outdir="$RESULTS_DIR/$model"
  mkdir -p "$outdir"

  echo "--- $model ($config) ---"

  # Save config info
  grep -E "memory_dir|session_history_turns|model" "$config" > "$outdir/config-info.txt" 2>/dev/null || true

  # Start server
  CONFIG_PATH="$config" AURA_CUSTOM_EVENTS=true AURA_EMIT_REASONING=true AURA_PROMPT_JOURNAL=true \
    PORT="$PORT" "$BINARY" --verbose > "$outdir/server.log" 2>&1 &
  SERVER_PID=$!

  if ! wait_for_server; then
    echo "  SKIPPING $model (server failed to start)"
    stop_server
    continue
  fi

  # Generate a stable session ID for this model run
  SESSION_ID="session_e2e_$(date +%s)_${model}"

  # Build up conversation history across turns
  HISTORY_JSON="[]"

  for i in "${!PROMPTS[@]}"; do
    prompt="${PROMPTS[$i]}"
    label="${PROMPT_LABELS[$i]}"

    run_session_prompt "$model" "$prompt" "$label" "$SESSION_ID" "$HISTORY_JSON"

    # Append user message + assistant response to history
    assistant_text=$(cat "$outdir/${label}.response.txt" 2>/dev/null || echo "")
    HISTORY_JSON=$(python3 -c "
import json, sys
history = json.loads(sys.argv[1])
history.append({'role': 'user', 'content': sys.argv[2]})
history.append({'role': 'assistant', 'content': sys.argv[3]})
print(json.dumps(history))
" "$HISTORY_JSON" "$prompt" "$assistant_text")

    # Brief pause between turns
    sleep 1
  done

  echo ""

  # ── Analyze session artifacts ──────────────────────────────────
  memory_dir=$(grep -o 'memory_dir *= *"[^"]*"' "$config" | head -1 | cut -d'"' -f2 || echo "")
  if [[ -n "$memory_dir" ]]; then
    echo "  Session artifacts ($memory_dir):"

    session_dir="$memory_dir/$SESSION_ID"
    if [[ -d "$session_dir" ]]; then
      manifest_count=$(find "$session_dir" -maxdepth 2 -name "manifest.json" ! -path "*/latest/*" 2>/dev/null | wc -l | tr -d ' ')
      echo "    Session dir: $session_dir"
      echo "    Manifests written: $manifest_count"
    else
      echo "    Session dir not found (session_id=$SESSION_ID)"
    fi

    # Check server log for session history injection evidence
    echo "    Session history injection:"
    inject_count=$(grep -c "Injecting session history" "$outdir/server.log" 2>/dev/null || true)
    inject_count="${inject_count:-0}"
    no_prior_count=$(grep -c "no prior manifests found" "$outdir/server.log" 2>/dev/null || true)
    no_prior_count="${no_prior_count:-0}"
    no_session_count=$(grep -c "no session_id" "$outdir/server.log" 2>/dev/null || true)
    no_session_count="${no_session_count:-0}"
    echo "      Injected: $inject_count turns"
    echo "      No prior manifests (first turn): $no_prior_count"
    if [[ "$no_session_count" -gt 0 ]]; then
      echo "      No session_id: $no_session_count"
    fi

    # Show injection details
    grep "Injecting session history" "$outdir/server.log" 2>/dev/null | sed 's/.*INFO/    ✓/' | head -5
  fi

  echo ""
  stop_server
done

echo "============================================="
echo "  Results saved to: $RESULTS_DIR"
echo "============================================="

#!/usr/bin/env bash
# SRE + Scratchpad E2E test — validates scratchpad interception with realistic SRE workloads
#
# Usage:
#   ./e2e-eval/run-sre-scratchpad-e2e.sh <iterations> <config1> [config2 ...]
#
# Starts k8s-sre-mcp (VERBOSE_MODE=true) automatically, then runs each prompt
# against each config, capturing SSE output for assertion evaluation.
#
# Examples:
#   # Single iteration with default SRE+scratchpad config
#   ./e2e-eval/run-sre-scratchpad-e2e.sh 1 configs/sre-scratchpad-e2e.toml
#
#   # Multiple configs
#   ./e2e-eval/run-sre-scratchpad-e2e.sh 3 configs/sre-scratchpad-e2e.toml configs/sre-orchestration-with-scratchpad.toml
#
# Prerequisites: cargo build --release, Docker (for k8s-sre-mcp)
set -euo pipefail

if [[ $# -lt 2 ]]; then
  echo "Usage: $0 <iterations> <config1.toml> [config2.toml ...]" >&2
  exit 1
fi

ITERATIONS="$1"; shift
CONFIGS=("$@")

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BINARY="$PROJECT_DIR/target/release/aura-web-server"
ASSERT_SCRIPT="$PROJECT_DIR/e2e-eval/eval-assertions.py"
RESULTS_DIR="$PROJECT_DIR/e2e-eval/sre-results-$(date +%Y%m%d-%H%M%S)"
PORT=8090
SRE_MCP_PORT=8082
SRE_CONTAINER_NAME="sre-mcp-e2e-$$"

if [[ ! -x "$BINARY" ]]; then
  echo "ERROR: Release binary not found. Run: cargo build --release" >&2
  exit 1
fi

model_name_from_config() {
  local base
  base="$(basename "$1" .toml)"
  # Strip common prefixes to get a clean model identifier
  base="${base#sre-orchestration-with-}"
  base="${base#sre-orchestration-}"
  base="${base#sre-scratchpad-}"
  echo "$base"
}

cleanup() {
  echo "Cleaning up..."
  # Stop aura server if running
  if [[ -n "${AURA_PID:-}" ]] && kill -0 "$AURA_PID" 2>/dev/null; then
    kill "$AURA_PID" 2>/dev/null || true
    wait "$AURA_PID" 2>/dev/null || true
  fi
  # Stop k8s-sre-mcp container
  docker rm -f "$SRE_CONTAINER_NAME" 2>/dev/null || true
}
trap cleanup EXIT

mkdir -p "$RESULTS_DIR"

# ── Prompt Sets ────────────────────────────────────────────────────

PROMPTS=(
  "What namespaces exist in the cluster?"
  "Discover workloads in production, check Prometheus targets, create ServiceMonitors for unmonitored services"
  "Investigate Prometheus target health and suggest remediations for any broken exporters"
)
LABELS=(
  "direct"
  "orchestrated"
  "multi-task-rich"
)

# ── Start k8s-sre-mcp with verbose mode ──────────────────────────

echo "=== Building k8s-sre-mcp Docker image ==="
docker build -t k8s-sre-mcp:local "$PROJECT_DIR/tests/integration/k8s-sre-mcp" 2>&1 | tail -1

echo "=== Starting k8s-sre-mcp (VERBOSE_MODE=true) on port $SRE_MCP_PORT ==="
docker run -d --name "$SRE_CONTAINER_NAME" \
  -p "$SRE_MCP_PORT:8082" \
  -e VERBOSE_MODE=true \
  k8s-sre-mcp:local >/dev/null

# Wait for MCP server to be ready
echo -n "Waiting for k8s-sre-mcp..."
for i in $(seq 1 30); do
  if curl -sf "http://127.0.0.1:$SRE_MCP_PORT/health" >/dev/null 2>&1 || \
     curl -sf "http://127.0.0.1:$SRE_MCP_PORT/mcp" -X POST \
       -H "Content-Type: application/json" \
       -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"0.1"}}}' \
       >/dev/null 2>&1; then
    echo " ready"
    break
  fi
  sleep 1
  echo -n "."
done

# ── Run E2E ──────────────────────────────────────────────────────

for config in "${CONFIGS[@]}"; do
  MODEL="$(model_name_from_config "$config")"
  echo ""
  echo "============================================================"
  echo "  Model: $MODEL  ($config)"
  echo "============================================================"

  for iter in $(seq 1 "$ITERATIONS"); do
    echo ""
    echo "--- Iteration $iter/$ITERATIONS ---"

    ITER_DIR="$RESULTS_DIR/$MODEL/iter-$iter"
    mkdir -p "$ITER_DIR"

    # Start aura server
    CONFIG_PATH="$config" \
    AURA_CUSTOM_EVENTS=true \
    K8S_SRE_MCP_HOST=127.0.0.1 \
    PORT=$PORT \
    "$BINARY" > "$ITER_DIR/server.log" 2>&1 &
    AURA_PID=$!

    # Wait for aura to be ready
    echo -n "  Starting aura..."
    for i in $(seq 1 30); do
      if curl -sf "http://127.0.0.1:$PORT/health" >/dev/null 2>&1; then
        echo " ready (pid=$AURA_PID)"
        break
      fi
      if ! kill -0 "$AURA_PID" 2>/dev/null; then
        echo " FAILED (server exited)"
        cat "$ITER_DIR/server.log"
        exit 1
      fi
      sleep 1
      echo -n "."
    done

    # Run each prompt
    for idx in "${!PROMPTS[@]}"; do
      prompt="${PROMPTS[$idx]}"
      label="${LABELS[$idx]}"
      sse_file="$ITER_DIR/$label.sse"

      echo -n "  [$label] "
      start_ts=$(date +%s)

      curl -sN --max-time 300 \
        -X POST "http://127.0.0.1:$PORT/v1/chat/completions" \
        -H "Content-Type: application/json" \
        -d "$(jq -n --arg p "$prompt" '{
          model: null,
          stream: true,
          messages: [{role: "user", content: $p}]
        }')" \
        > "$sse_file" 2>/dev/null || true

      end_ts=$(date +%s)
      elapsed=$((end_ts - start_ts))

      # Quick sanity check
      if grep -q '"finish_reason":"stop"' "$sse_file" 2>/dev/null; then
        sp_event=$(grep -c 'scratchpad_usage' "$sse_file" 2>/dev/null || echo 0)
        echo "OK (${elapsed}s, scratchpad_events=${sp_event})"
      else
        echo "INCOMPLETE (${elapsed}s)"
      fi
    done

    # Stop aura server
    kill "$AURA_PID" 2>/dev/null || true
    wait "$AURA_PID" 2>/dev/null || true
    unset AURA_PID
    sleep 1
  done
done

# ── Run assertions ───────────────────────────────────────────────

echo ""
echo "============================================================"
echo "  Running assertions (--prompt-set sre-scratchpad)"
echo "============================================================"
echo ""

python3 "$ASSERT_SCRIPT" "$RESULTS_DIR" --prompt-set sre-scratchpad
ASSERT_EXIT=$?

echo ""
echo "Results directory: $RESULTS_DIR"
exit $ASSERT_EXIT

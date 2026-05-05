#!/usr/bin/env bash
# SRE Hard E2E — needle-in-haystack + category survival tests
#
# Requires VERBOSE_MODE=true on k8s-sre-mcp (returns full workload specs).
# Tests scratchpad exploration, cross-domain reasoning, and category survival
# through orchestration synthesis.
#
# Usage:
#   ./e2e-eval/run-sre-hard-e2e.sh <iterations> <config1> [config2 ...]
#
# Examples:
#   ./e2e-eval/run-sre-hard-e2e.sh 1 configs/sre-scratchpad-e2e.toml
#   ./e2e-eval/run-sre-hard-e2e.sh 1 configs/sre-scratchpad-e2e.toml configs/sre-scratchpad-e2e-sonnet.toml
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
BINARY="${BINARY:-$PROJECT_DIR/target/release/aura-web-server}"
ASSERT_SCRIPT="$PROJECT_DIR/e2e-eval/eval-assertions.py"
RESULTS_DIR="$PROJECT_DIR/e2e-eval/sre-hard-results-$(date +%Y%m%d-%H%M%S)"
PORT=8090
SRE_MCP_PORT=8082
SRE_CONTAINER_NAME="sre-mcp-hard-$$"

if [[ ! -x "$BINARY" ]]; then
  echo "ERROR: Release binary not found. Run: cargo build --release" >&2
  exit 1
fi

model_name_from_config() {
  local base
  base="$(basename "$1" .toml)"
  base="${base#sre-orchestration-with-}"
  base="${base#sre-orchestration-}"
  base="${base#sre-scratchpad-}"
  echo "$base"
}

cleanup() {
  echo "Cleaning up..."
  if [[ -n "${AURA_PID:-}" ]] && kill -0 "$AURA_PID" 2>/dev/null; then
    kill "$AURA_PID" 2>/dev/null || true
    wait "$AURA_PID" 2>/dev/null || true
  fi
  docker rm -f "$SRE_CONTAINER_NAME" 2>/dev/null || true
}
trap cleanup EXIT

mkdir -p "$RESULTS_DIR"

# ── Prompt Sets (needle-in-haystack + category survival) ─────────

PROMPTS=(
  "What are the readiness probe HTTP paths for each production workload that has one configured?"
  "Which pods have restart counts above 5? For each, identify the alert rule that would fire and the pod name."
  "Audit the security posture of production workloads. For each, report whether it runs as non-root, has readOnlyRootFilesystem, and list the exact name of every environment variable that references a Kubernetes secret."
  "Perform a comprehensive health check across the cluster. Report ALL findings organized by category: workload health, certificate status, disk pressure, queue depths, replication lag, and firing alerts. For each finding include the exact alert rule name and affected resource name."
  "List all production workloads that have sidecar containers. For each sidecar, report its name, image, and resource limits."
)
LABELS=(
  "probe-paths"
  "restart-investigation"
  "security-audit"
  "multi-category-findings"
  "sidecar-infrastructure"
)

# ── Start k8s-sre-mcp with verbose mode ──────────────────────────

echo "=== Building k8s-sre-mcp Docker image ==="
docker build -t k8s-sre-mcp:local "$PROJECT_DIR/tests/integration/k8s-sre-mcp" 2>&1 | tail -1

echo "=== Starting k8s-sre-mcp (VERBOSE_MODE=true) on port $SRE_MCP_PORT ==="
docker run -d --name "$SRE_CONTAINER_NAME" \
  -p "$SRE_MCP_PORT:8082" \
  -e VERBOSE_MODE=true \
  k8s-sre-mcp:local >/dev/null

echo -n "Waiting for k8s-sre-mcp..."
for i in $(seq 1 30); do
  if curl -sf "http://127.0.0.1:$SRE_MCP_PORT/mcp" -X POST \
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
  echo "  Prompts: ${#PROMPTS[@]} hard (needle-in-haystack + category survival)"
  echo "============================================================"

  for iter in $(seq 1 "$ITERATIONS"); do
    echo ""
    echo "--- Iteration $iter/$ITERATIONS ---"

    ITER_DIR="$RESULTS_DIR/$MODEL/iter-$iter"
    mkdir -p "$ITER_DIR"

    CONFIG_PATH="$config" \
    AURA_CUSTOM_EVENTS=true \
    K8S_SRE_MCP_HOST=127.0.0.1 \
    RUST_LOG=aura=info,aura_web_server=info \
    PORT=$PORT \
    "$BINARY" > "$ITER_DIR/server.log" 2>&1 &
    AURA_PID=$!

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

    for idx in "${!PROMPTS[@]}"; do
      prompt="${PROMPTS[$idx]}"
      label="${LABELS[$idx]}"
      sse_file="$ITER_DIR/$label.sse"

      echo -n "  [$label] "
      start_ts=$(date +%s)

      curl -sN --max-time 600 \
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

      if grep -q '"finish_reason":"stop"' "$sse_file" 2>/dev/null; then
        sp_event=$(grep -c 'scratchpad_usage' "$sse_file" 2>/dev/null || echo 0)
        echo "OK (${elapsed}s, scratchpad_events=${sp_event})"
      else
        echo "INCOMPLETE (${elapsed}s)"
      fi
    done

    kill "$AURA_PID" 2>/dev/null || true
    wait "$AURA_PID" 2>/dev/null || true
    unset AURA_PID
    sleep 1
  done
done

# ── Run assertions ───────────────────────────────────────────────

echo ""
echo "============================================================"
echo "  Running assertions (--prompt-set sre-hard --skip-scratchpad)"
echo "============================================================"
echo ""

python3 "$ASSERT_SCRIPT" "$RESULTS_DIR" --prompt-set sre-hard --skip-scratchpad
ASSERT_EXIT=$?

echo ""
echo "Results directory: $RESULTS_DIR"
exit $ASSERT_EXIT

#!/usr/bin/env bash
# Run dependent-prompt session E2E across multiple models, then analyze each.
# Designed to run unattended — all output logged to a single file.
#
# Usage:
#   ./temp-prompt-eval/run-multi-model-session-eval.sh [config1.toml ...]
#
# Defaults to claude-thinking, glm, qwen35-thinking if no args given.
set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
LOGFILE="$PROJECT_DIR/temp-prompt-eval/multi-model-eval-$(date +%Y%m%d-%H%M%S).log"

if [[ $# -gt 0 ]]; then
  CONFIGS=("$@")
else
  CONFIGS=(
    configs/math-orchestration-claude-thinking.toml
    configs/math-orchestration-glm.toml
    configs/math-orchestration-qwen35-thinking.toml
  )
fi

exec > >(tee -a "$LOGFILE") 2>&1

echo "============================================="
echo "  Multi-Model Session History Eval"
echo "  $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "  Models: ${#CONFIGS[@]}"
echo "  Log: $LOGFILE"
echo "============================================="
echo ""

# Parallel arrays for post-run analysis
MODELS=()
MEM_DIRS=()
SIDS=()

for config in "${CONFIGS[@]}"; do
  if [[ ! -f "$config" ]]; then
    echo "ERROR: Config not found: $config" >&2
    exit 1
  fi

  model=$(basename "$config" .toml | sed 's/math-orchestration-//')

  # Extract memory_dir from config
  memory_dir=$(grep -oP 'memory_dir\s*=\s*"\K[^"]+' "$config" 2>/dev/null || echo "")
  if [[ -z "$memory_dir" ]]; then
    echo "WARNING: No memory_dir in $config — skipping (session persistence required)" >&2
    continue
  fi

  echo ""
  echo "###############################################"
  echo "  Running: $model"
  echo "###############################################"
  echo ""

  PROMPT_SET=dependent "$PROJECT_DIR/temp-prompt-eval/run-session-e2e.sh" "$config"

  # Find the most recent session ID for this model
  latest_session=$(ls -td "$memory_dir"/session_e2e_* 2>/dev/null | head -1 || echo "")
  if [[ -n "$latest_session" ]]; then
    sid=$(basename "$latest_session")
    echo ""
    echo "  >> Session ID: $sid"
  else
    echo "  >> WARNING: No session directory found in $memory_dir"
    sid="MISSING"
  fi

  MODELS+=("$model")
  MEM_DIRS+=("$memory_dir")
  SIDS+=("$sid")

  echo ""
done

echo ""
echo "###############################################"
echo "  Analysis Phase"
echo "###############################################"
echo ""

for i in "${!MODELS[@]}"; do
  model="${MODELS[$i]}"
  memory_dir="${MEM_DIRS[$i]}"
  sid="${SIDS[$i]}"

  if [[ "$sid" == "MISSING" ]]; then
    echo "=== $model: SKIPPED (no session dir) ==="
    continue
  fi

  echo "=== $model ==="
  python3 "$PROJECT_DIR/temp-prompt-eval/analyze-session-history-eval.py" \
    --memory-dir "$memory_dir" \
    --session-id "$sid" \
    --json-export "$PROJECT_DIR/temp-prompt-eval/eval-${model}.json" \
    2>&1 || echo "  >> Analysis failed for $model"
  echo ""
done

echo "============================================="
echo "  COMPLETE — $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "  Log: $LOGFILE"
echo "============================================="

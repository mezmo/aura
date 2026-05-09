#!/usr/bin/env bash
#
# test-generate-orchestrate.sh
#
# Multi-agent test generation pipeline using Aura agents.
# Generates Rust tests, validates with cargo test, then runs 4 review agents
# in parallel. Issues are fed back to the generator for up to N rounds.
#
# Usage:
#   ./scripts/test-generate-orchestrate.sh --files "crates/aura/src/foo.rs"
#   ./scripts/test-generate-orchestrate.sh  # auto-detect changed files vs main
#
# Requires: cargo, curl, jq, OPENAI_API_KEY
set -euo pipefail

# ── Constants ────────────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CONFIG_DIR="${PROJECT_ROOT}/configs/test-agents"
BINARY_NAME="aura-web-server"

GENERATOR_PORT=18090
REVIEW_PORTS=(18091 18092 18093 18094)
REVIEW_NAMES=("correctness" "coverage" "robustness" "style")
REVIEW_CONFIGS=("review-correctness" "review-coverage" "review-robustness" "review-style")

MAX_ROUNDS="${MAX_ROUNDS:-3}"
HEALTH_TIMEOUT=90
REQUEST_TIMEOUT=300

PIDS=()
REVIEW_OUTPUT_DIR="/tmp/aura-test-generate"

# ── Colors ───────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m'

# ── Argument Parsing ─────────────────────────────────────────────────
FILES=""
while [[ $# -gt 0 ]]; do
  case $1 in
    --files)      FILES="$2"; shift 2 ;;
    --max-rounds) MAX_ROUNDS="$2"; shift 2 ;;
    *)            echo "Unknown argument: $1"; exit 1 ;;
  esac
done

# ── Dependency Checks ────────────────────────────────────────────────
for cmd in curl jq cargo; do
  if ! command -v "$cmd" &>/dev/null; then
    echo -e "${RED}Error: $cmd is required but not found.${NC}"
    exit 1
  fi
done

# Verify LLM credentials are available.
# Default configs use AWS Bedrock (credentials via ~/.aws/credentials or env vars).
# If using OpenAI instead, set OPENAI_API_KEY.
if [ -z "${AWS_ACCESS_KEY_ID:-}" ] && [ ! -f ~/.aws/credentials ] && [ -z "${OPENAI_API_KEY:-}" ]; then
  echo -e "${RED}Error: No LLM credentials found.${NC}"
  echo "  For Bedrock: configure ~/.aws/credentials or set AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY"
  echo "  For OpenAI:  export OPENAI_API_KEY=\"sk-...\""
  exit 1
fi

# ── Cleanup ──────────────────────────────────────────────────────────
cleanup() {
  echo -e "\n${BLUE}[cleanup]${NC} Stopping agent processes..."
  for pid in "${PIDS[@]+"${PIDS[@]}"}"; do
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  done
  PIDS=()
}
trap cleanup EXIT INT TERM

# ── Helper Functions ─────────────────────────────────────────────────

wait_for_health() {
  local port=$1
  local elapsed=0
  while [ $elapsed -lt $HEALTH_TIMEOUT ]; do
    if curl -sf "http://127.0.0.1:${port}/health" >/dev/null 2>&1; then
      return 0
    fi
    sleep 2
    elapsed=$((elapsed + 2))
  done
  echo -e "${RED}Error: Agent on port ${port} did not become healthy within ${HEALTH_TIMEOUT}s${NC}"
  return 1
}

start_agent() {
  local config_name=$1
  local port=$2
  local config_path="${CONFIG_DIR}/${config_name}.toml"

  if [ ! -f "$config_path" ]; then
    echo -e "${RED}Error: Config not found: ${config_path}${NC}"
    return 1
  fi

  local log_file="${REVIEW_OUTPUT_DIR}/agent-${config_name}.log"
  CONFIG_PATH="$config_path" \
  PORT="$port" \
  HOST="127.0.0.1" \
    "${PROJECT_ROOT}/target/debug/${BINARY_NAME}" \
    --streaming-timeout-secs "$REQUEST_TIMEOUT" \
    >"$log_file" 2>&1 &
  local pid=$!
  PIDS+=("$pid")

  if ! wait_for_health "$port"; then
    echo -e "${RED}Failed to start agent ${config_name} on port ${port}${NC}"
    return 1
  fi
  echo -e "  ${GREEN}Started${NC} ${config_name} on :${port} (pid ${pid})"
}

stop_agent_on_port() {
  local port=$1
  # Find and kill the process listening on this port
  local pid
  pid=$(lsof -ti :"$port" 2>/dev/null || true)
  if [ -n "$pid" ]; then
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    # Remove from PIDS array
    local new_pids=()
    for p in "${PIDS[@]+"${PIDS[@]}"}"; do
      if [ "$p" != "$pid" ]; then
        new_pids+=("$p")
      fi
    done
    PIDS=("${new_pids[@]+"${new_pids[@]}"}")
  fi
}

stop_all_review_agents() {
  for port in "${REVIEW_PORTS[@]+"${REVIEW_PORTS[@]}"}"; do
    stop_agent_on_port "$port"
  done
}

send_prompt() {
  local port=$1
  local prompt_text=$2

  local payload
  payload=$(jq -n --arg content "$prompt_text" '{
    messages: [{role: "user", content: $content}],
    stream: false
  }')

  local response
  response=$(curl -sf --max-time "$REQUEST_TIMEOUT" \
    -H "Content-Type: application/json" \
    -d "$payload" \
    "http://127.0.0.1:${port}/v1/chat/completions" 2>&1) || {
    echo -e "${RED}Error: Request to agent on port ${port} failed${NC}"
    echo "$response" | tail -5
    return 1
  }

  echo "$response" | jq -r '.choices[0].message.content // "No response content"'
}

# Derive crate name from file path: crates/<crate>/src/... -> <crate>
get_crate_name() {
  local file=$1
  echo "$file" | sed -n 's|crates/\([^/]*\)/.*|\1|p'
}

# Derive test file path from source path
get_test_file_path() {
  local source_file=$1
  local basename
  basename=$(basename "$source_file" .rs)
  local dir
  dir=$(dirname "$source_file")
  echo "${dir}/${basename}_tests.rs"
}

# Get the module name for the generated test file
get_test_module_name() {
  local source_file=$1
  local basename
  basename=$(basename "$source_file" .rs)
  echo "${basename}_tests"
}

# Get the source module name (for use in imports)
get_source_module_name() {
  local source_file=$1
  basename "$source_file" .rs
}

# Find lib.rs for a crate
get_lib_rs_path() {
  local source_file=$1
  local crate_dir
  crate_dir=$(echo "$source_file" | sed -n 's|\(crates/[^/]*/\).*|\1|p')
  echo "${crate_dir}src/lib.rs"
}

# Register generated test module in lib.rs (idempotent)
register_test_module() {
  local source_file=$1
  local test_module
  test_module=$(get_test_module_name "$source_file")
  local lib_rs
  lib_rs="${PROJECT_ROOT}/$(get_lib_rs_path "$source_file")"

  if [ ! -f "$lib_rs" ]; then
    echo -e "  ${YELLOW}Warning: lib.rs not found at ${lib_rs}${NC}"
    return 0
  fi

  local mod_line="#[cfg(test)] mod ${test_module};"

  if ! grep -qF "$test_module" "$lib_rs"; then
    echo "$mod_line" >> "$lib_rs"
    echo -e "  ${BLUE}[module]${NC} Registered ${test_module} in lib.rs"
  fi
}

# ── Target File Identification ───────────────────────────────────────
if [ -n "$FILES" ]; then
  IFS=' ' read -ra TARGET_FILES <<< "$FILES"
else
  mapfile -t TARGET_FILES < <(
    git -C "$PROJECT_ROOT" diff --name-only main -- 'crates/**/*.rs' 2>/dev/null \
      | grep -v '/tests/' \
      | grep -v '_test\.rs' \
      | grep -v '_generated_tests\.rs' \
      || true
  )
fi

if [ ${#TARGET_FILES[@]} -eq 0 ]; then
  echo -e "${YELLOW}No target files found.${NC}"
  echo "  Use: make test-generate FILES=\"crates/aura/src/foo.rs\""
  echo "  Or have uncommitted .rs changes vs main."
  exit 0
fi

echo -e "${BOLD}"
echo "================================================================"
echo "  Aura Test Generation Pipeline"
echo "================================================================"
echo -e "${NC}"
echo -e "  Target files:  ${#TARGET_FILES[@]}"
for f in "${TARGET_FILES[@]}"; do
  echo -e "    ${BLUE}${f}${NC}"
done
echo -e "  Max rounds:    ${MAX_ROUNDS}"
echo ""

# ── Pre-build ────────────────────────────────────────────────────────
echo -e "${BLUE}[build]${NC} Building aura-web-server..."
if ! cargo build --bin "$BINARY_NAME" 2>&1 | tail -3; then
  echo -e "${RED}Build failed.${NC}"
  exit 1
fi
echo -e "${GREEN}[build]${NC} Build complete."
echo ""

# ── Ensure output directory ──────────────────────────────────────────
mkdir -p "$REVIEW_OUTPUT_DIR"

# ── Process Each Target File ─────────────────────────────────────────
FINAL_RESULTS=()

for source_file in "${TARGET_FILES[@]}"; do
  echo -e "${BOLD}────────────────────────────────────────${NC}"
  echo -e "${BOLD}  Processing: ${source_file}${NC}"
  echo -e "${BOLD}────────────────────────────────────────${NC}"

  crate_name=$(get_crate_name "$source_file")
  test_file=$(get_test_file_path "$source_file")
  abs_source="${PROJECT_ROOT}/${source_file}"
  abs_test="${PROJECT_ROOT}/${test_file}"

  if [ -z "$crate_name" ]; then
    echo -e "${YELLOW}Skipping ${source_file} — not in a crate directory${NC}"
    FINAL_RESULTS+=("SKIP|${source_file}|Not in crates/ directory")
    continue
  fi

  if [ ! -f "$abs_source" ]; then
    echo -e "${YELLOW}Skipping ${source_file} — file not found${NC}"
    FINAL_RESULTS+=("SKIP|${source_file}|File not found")
    continue
  fi

  source_module=$(get_source_module_name "$source_file")
  test_module=$(get_test_module_name "$source_file")

  # Pre-create test file (workaround for validate_path canonicalize)
  mkdir -p "$(dirname "$abs_test")"
  touch "$abs_test"

  # Register test module in lib.rs
  register_test_module "$source_file"

  # Pre-create review output files
  for name in "${REVIEW_NAMES[@]}"; do
    touch "${REVIEW_OUTPUT_DIR}/review-${name}.json"
  done

  all_clear=false
  round=0
  feedback=""

  while [ $round -lt "$MAX_ROUNDS" ] && [ "$all_clear" = false ]; do
    round=$((round + 1))
    echo ""
    echo -e "${BLUE}  === Round ${round} / ${MAX_ROUNDS} ===${NC}"

    # ── Generation ─────────────────────────────────────────────────
    echo -e "  ${BLUE}[generator]${NC} Starting agent..."
    start_agent "generator" "$GENERATOR_PORT"

    if [ "$round" -eq 1 ]; then
      gen_prompt="Read the Rust source file at: ${abs_source}

Then generate comprehensive unit tests and write them to: ${abs_test}

IMPORTANT: This test file is a sibling module registered in lib.rs, NOT a submodule of the source file.
Use this import pattern at the top of the file:
  use crate::${source_module}::*;

Do NOT use 'use super::*;' — that would reference the crate root, not the source module.

The file is in crate '${crate_name}'. Write a complete, self-contained test file."
    else
      gen_prompt="Read the source file at: ${abs_source}
Read the current tests at: ${abs_test}

The following issues were found by reviewers. Fix ALL issues by rewriting the test file.

IMPORTANT: Use 'use crate::${source_module}::*;' for imports (NOT 'use super::*;').

FEEDBACK:
${feedback}

Write the corrected tests to: ${abs_test}"
    fi

    echo -e "  ${BLUE}[generator]${NC} Generating tests..."
    gen_response=$(send_prompt "$GENERATOR_PORT" "$gen_prompt")
    stop_agent_on_port "$GENERATOR_PORT"

    # Check if test file was written
    if [ ! -s "$abs_test" ]; then
      echo -e "  ${RED}[generator]${NC} Test file is empty — agent may not have called write_file"
      feedback="The generator did not write any test file. You MUST use the write_file tool to write tests to ${abs_test}. Do not just describe the tests — actually write them."
      continue
    fi

    echo -e "  ${GREEN}[generator]${NC} Tests written to ${test_file}"

    # ── Cargo Test ─────────────────────────────────────────────────
    echo -e "  ${BLUE}[test]${NC} Running cargo test..."
    test_output=""
    test_exit=0
    test_output=$(cargo test --package "$crate_name" --lib 2>&1) || test_exit=$?

    if [ "$test_exit" -ne 0 ]; then
      echo -e "  ${RED}[test]${NC} FAILED (exit code ${test_exit})"
      # Truncate long output for the feedback prompt
      truncated_output=$(echo "$test_output" | tail -40)
      feedback="cargo test FAILED with exit code ${test_exit}. Fix the compilation/test errors:

${truncated_output}

Read the test file at ${abs_test}, fix all errors, and write the corrected version back."
      if [ "$round" -lt "$MAX_ROUNDS" ]; then
        echo -e "  ${YELLOW}[test]${NC} Will retry in next round..."
        continue
      else
        echo -e "  ${RED}[test]${NC} Failed on final round"
        FINAL_RESULTS+=("FAIL|${source_file}|cargo test failed after ${MAX_ROUNDS} rounds")
        break
      fi
    fi
    echo -e "  ${GREEN}[test]${NC} All tests pass"

    # ── Review Phase ───────────────────────────────────────────────
    echo -e "  ${BLUE}[review]${NC} Starting 4 review agents..."
    for i in "${!REVIEW_NAMES[@]}"; do
      start_agent "${REVIEW_CONFIGS[$i]}" "${REVIEW_PORTS[$i]}"
    done

    echo -e "  ${BLUE}[review]${NC} Sending review requests (parallel)..."
    review_pids=()
    for i in "${!REVIEW_NAMES[@]}"; do
      name="${REVIEW_NAMES[$i]}"
      port="${REVIEW_PORTS[$i]}"
      output_path="${REVIEW_OUTPUT_DIR}/review-${name}.json"

      review_prompt="Review the generated tests for correctness, coverage, robustness, and style.

Source file: ${abs_source}
Test file: ${abs_test}

Write your review as a JSON object to: ${output_path}"

      # Run in background
      (send_prompt "$port" "$review_prompt" >/dev/null 2>&1) &
      review_pids+=($!)
    done

    # Wait for all reviews to complete
    for pid in "${review_pids[@]}"; do
      wait "$pid" 2>/dev/null || true
    done

    stop_all_review_agents
    echo -e "  ${GREEN}[review]${NC} All reviews complete"

    # ── Consolidate Reviews ────────────────────────────────────────
    total_issues=0
    feedback=""
    all_clear=true

    for name in "${REVIEW_NAMES[@]}"; do
      output_path="${REVIEW_OUTPUT_DIR}/review-${name}.json"
      if [ ! -s "$output_path" ]; then
        echo -e "  ${YELLOW}[${name}]${NC} No review output"
        continue
      fi

      # Try to parse as JSON; the file might contain the raw JSON or wrapped text
      review_content=$(cat "$output_path")

      # Try to extract verdict from JSON
      verdict=$(echo "$review_content" | jq -r '.verdict // "unknown"' 2>/dev/null || echo "unknown")

      if [ "$verdict" = "fail" ]; then
        all_clear=false
        issue_count=$(echo "$review_content" | jq '.issues | length' 2>/dev/null || echo "?")
        total_issues=$((total_issues + ${issue_count:-0}))
        issues_text=$(echo "$review_content" | jq -r '.issues[] | "- [\(.severity // .risk // .priority // "issue")] \(.test_name // .function // "unknown"): \(.description // .missing_scenario // .issue // "no details")"' 2>/dev/null || echo "  (could not parse issues)")
        feedback="${feedback}
=== ${name} review (FAIL) ===
${issues_text}
"
        echo -e "  ${RED}[${name}]${NC} FAIL — ${issue_count} issue(s)"
      elif [ "$verdict" = "pass" ]; then
        echo -e "  ${GREEN}[${name}]${NC} PASS"
      else
        # Could not parse — treat as pass to avoid infinite loops
        echo -e "  ${YELLOW}[${name}]${NC} Could not parse review (treating as pass)"
      fi
    done

    if [ "$all_clear" = true ]; then
      echo -e "\n  ${GREEN}All reviews passed!${NC}"
      FINAL_RESULTS+=("PASS|${source_file}|Round ${round}/${MAX_ROUNDS}")
    elif [ "$round" -ge "$MAX_ROUNDS" ]; then
      echo -e "\n  ${YELLOW}Issues remain after ${MAX_ROUNDS} rounds (${total_issues} total)${NC}"
      FINAL_RESULTS+=("WARN|${source_file}|${total_issues} review issues after ${MAX_ROUNDS} rounds")
    else
      echo -e "\n  ${YELLOW}${total_issues} issue(s) found — feeding back to generator${NC}"
    fi
  done
done

# ── Final Summary ────────────────────────────────────────────────────
echo ""
echo -e "${BOLD}"
echo "================================================================"
echo "  Test Generation Summary"
echo "================================================================"
echo -e "${NC}"

pass_count=0
fail_count=0
warn_count=0
skip_count=0

for result in "${FINAL_RESULTS[@]}"; do
  IFS='|' read -r status file detail <<< "$result"
  case "$status" in
    PASS) echo -e "  ${GREEN}PASS${NC}  ${file}  (${detail})"; pass_count=$((pass_count + 1)) ;;
    FAIL) echo -e "  ${RED}FAIL${NC}  ${file}  (${detail})"; fail_count=$((fail_count + 1)) ;;
    WARN) echo -e "  ${YELLOW}WARN${NC}  ${file}  (${detail})"; warn_count=$((warn_count + 1)) ;;
    SKIP) echo -e "  ${YELLOW}SKIP${NC}  ${file}  (${detail})"; skip_count=$((skip_count + 1)) ;;
  esac
done

echo ""
echo -e "  Total: ${#TARGET_FILES[@]}  Pass: ${pass_count}  Warn: ${warn_count}  Fail: ${fail_count}  Skip: ${skip_count}"
echo ""
echo "================================================================"

# Exit with error if any failures
if [ "$fail_count" -gt 0 ]; then
  exit 1
fi

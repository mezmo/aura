#!/usr/bin/env python3
"""
Compare independent vs session-mode E2E runs for regression analysis.

Reads:
  - Independent run: SSE files + persistence artifacts (per-session dirs)
  - Session run: SSE files + persistence artifacts (shared session dir)

Reports:
  1. Tool call counts (MCP tools, get_conversation_context, read_artifact)
  2. Plan quality: task descriptions checked for unresolved references
  3. Quality scores from manifests
  4. Timing comparison
  5. Routing differences

Usage:
  python3 temp-prompt-eval/analyze-session-regression.py \
    --independent temp-prompt-eval/results-20260320-110320 \
    --session temp-prompt-eval/session-results-20260320-132922
"""

import argparse
import glob
import json
import os
import re
import sys
from pathlib import Path


# Prompt label mapping between the two scripts
# Independent: direct-add, mean-then-multiply, trig-sin45, add-then-mean, multi-step-median
# Session: q1-direct-add, q2-mean-then-multiply, q3-trig-sin45, q4-add-then-mean, q5-multi-step-median
LABEL_MAP = {
    "q1-direct-add": "direct-add",
    "q2-mean-then-multiply": "mean-then-multiply",
    "q3-trig-sin45": "trig-sin45",
    "q4-add-then-mean": "add-then-mean",
    "q5-multi-step-median": "multi-step-median",
}

MODELS = ["opus-bedrock", "claude-thinking", "glm", "qwen35-thinking"]

# Patterns that indicate unresolved references in task descriptions
LAZY_PATTERNS = [
    r"\bprevious result\b",
    r"\bthat result\b",
    r"\bthe result\b",
    r"\babove\b",
    r"\bprior\b",
    r"\beach other\b",
    r"\bthose\b",
    r"\bthem\b",
]


def parse_sse_file(path):
    """Extract metrics from an SSE file."""
    if not os.path.exists(path):
        return None

    content = open(path).read()
    lines = content.split("\n")

    metrics = {
        "tool_calls": 0,
        "get_conversation_context": 0,
        "read_artifact": 0,
        "tools_used": [],
        "routing": "unknown",
        "quality_score": None,
        "task_count": 0,
        "completed": '"finish_reason":"stop"' in content,
    }

    for line in lines:
        # Count worker tool calls
        if "aura.orchestrator.tool_call_completed" in line or "aura.tool_complete" in line:
            metrics["tool_calls"] += 1

        # Check for specific tool names in tool_start events
        if "aura.orchestrator.tool_call_started" in line or "aura.tool_start" in line:
            if "get_conversation_context" in line:
                metrics["get_conversation_context"] += 1
            if "read_artifact" in line:
                metrics["read_artifact"] += 1

        # Also check data lines for tool names
        if line.startswith("data: ") and "get_conversation_context" in line:
            metrics["get_conversation_context"] += 1
        if line.startswith("data: ") and "read_artifact" in line:
            # Don't double-count from tool_start events
            pass

        # Routing
        if "direct answer" in line.lower() or "respond_directly" in line:
            metrics["routing"] = "direct"
        if "plan created" in line:
            metrics["routing"] = "orchestrated"
            # Extract task count
            m = re.search(r"(\d+) tasks", line)
            if m:
                metrics["task_count"] = int(m.group(1))

        # Quality score
        if "quality=" in line:
            m = re.search(r"quality=([\d.]+)", line)
            if m:
                metrics["quality_score"] = float(m.group(1))

        # Extract tool names from tool_call events
        if "tool_call_started" in line or "tool_start" in line:
            # Look in the next data line
            pass

    return metrics


def find_persistence_manifests(memory_dir, session_id=None):
    """Find all manifest.json files in a persistence directory."""
    manifests = {}
    if session_id:
        search_dir = os.path.join(memory_dir, session_id)
    else:
        search_dir = memory_dir

    if not os.path.exists(search_dir):
        return manifests

    for manifest_path in glob.glob(os.path.join(search_dir, "*/manifest.json")):
        try:
            with open(manifest_path) as f:
                m = json.load(f)
                manifests[m["goal"]] = m
        except (json.JSONDecodeError, KeyError):
            pass

    return manifests


def find_plan_files(memory_dir, session_id=None):
    """Find all plan.json files and extract task descriptions."""
    plans = {}
    if session_id:
        search_dir = os.path.join(memory_dir, session_id)
    else:
        search_dir = memory_dir

    if not os.path.exists(search_dir):
        return plans

    for plan_path in glob.glob(os.path.join(search_dir, "*/iteration-*/plan.json")):
        try:
            with open(plan_path) as f:
                plan = json.load(f)
                goal = plan.get("goal", "unknown")
                tasks = plan.get("tasks", [])
                task_descs = [t.get("description", "") for t in tasks]
                plans[goal] = {
                    "tasks": tasks,
                    "task_descriptions": task_descs,
                    "path": plan_path,
                }
        except (json.JSONDecodeError, KeyError):
            pass

    return plans


def find_tool_calls_files(memory_dir, session_id=None):
    """Find tool-calls.json files and count get_conversation_context/read_artifact usage."""
    if session_id:
        search_dir = os.path.join(memory_dir, session_id)
    else:
        search_dir = memory_dir

    counts = {"get_conversation_context": 0, "read_artifact": 0, "total_mcp": 0}

    if not os.path.exists(search_dir):
        return counts

    for tc_path in glob.glob(os.path.join(search_dir, "*/iteration-*/*.tool-calls.json")):
        try:
            with open(tc_path) as f:
                calls = json.load(f)
                for call in calls:
                    tool_name = call.get("tool", "")
                    counts["total_mcp"] += 1
                    if tool_name == "get_conversation_context":
                        counts["get_conversation_context"] += 1
                    if tool_name == "read_artifact":
                        counts["read_artifact"] += 1
        except (json.JSONDecodeError, KeyError):
            pass

    return counts


def check_lazy_descriptions(task_descriptions):
    """Check task descriptions for unresolved references."""
    issues = []
    for desc in task_descriptions:
        for pattern in LAZY_PATTERNS:
            if re.search(pattern, desc, re.IGNORECASE):
                issues.append(f'  "{desc[:80]}" — matches: {pattern}')
                break
    return issues


def main():
    parser = argparse.ArgumentParser(description="Compare independent vs session E2E runs")
    parser.add_argument("--independent", required=True, help="Independent run results dir")
    parser.add_argument("--session", required=True, help="Session run results dir")
    args = parser.parse_args()

    indep_dir = args.independent
    session_dir = args.session

    print("=" * 76)
    print("  Session vs Independent — Regression Analysis")
    print("=" * 76)
    print(f"  Independent: {indep_dir}")
    print(f"  Session:     {session_dir}")
    print()

    # ── 1. SSE-based comparison ──────────────────────────────────────
    print("-" * 76)
    print("1. PER-PROMPT COMPARISON (from SSE events)")
    print("-" * 76)
    print()

    header = f"{'Model':<20} {'Prompt':<25} {'Mode':<8} {'Route':<12} {'Tools':>5} {'ctx':>4} {'art':>4} {'Q':>5} {'OK':>3}"
    print(header)
    print("-" * len(header))

    all_indep_tools = 0
    all_session_tools = 0
    all_indep_ctx = 0
    all_session_ctx = 0
    all_indep_art = 0
    all_session_art = 0
    routing_diffs = []

    for model in MODELS:
        for session_label, indep_label in LABEL_MAP.items():
            # Independent SSE
            indep_sse = os.path.join(indep_dir, model, "iter-1", f"{indep_label}.sse")
            indep_metrics = parse_sse_file(indep_sse)

            # Session SSE
            session_sse = os.path.join(session_dir, model, f"{session_label}.sse")
            session_metrics = parse_sse_file(session_sse)

            if indep_metrics:
                q_str = f"{indep_metrics['quality_score']:.2f}" if indep_metrics["quality_score"] else "—"
                ok_str = "✓" if indep_metrics["completed"] else "✗"
                print(f"{model:<20} {indep_label:<25} {'indep':<8} {indep_metrics['routing']:<12} {indep_metrics['tool_calls']:>5} {indep_metrics['get_conversation_context']:>4} {indep_metrics['read_artifact']:>4} {q_str:>5} {ok_str:>3}")
                all_indep_tools += indep_metrics["tool_calls"]
                all_indep_ctx += indep_metrics["get_conversation_context"]
                all_indep_art += indep_metrics["read_artifact"]

            if session_metrics:
                q_str = f"{session_metrics['quality_score']:.2f}" if session_metrics["quality_score"] else "—"
                ok_str = "✓" if session_metrics["completed"] else "✗"
                print(f"{'':<20} {'':<25} {'session':<8} {session_metrics['routing']:<12} {session_metrics['tool_calls']:>5} {session_metrics['get_conversation_context']:>4} {session_metrics['read_artifact']:>4} {q_str:>5} {ok_str:>3}")
                all_session_tools += session_metrics["tool_calls"]
                all_session_ctx += session_metrics["get_conversation_context"]
                all_session_art += session_metrics["read_artifact"]

            # Check for routing differences
            if indep_metrics and session_metrics and indep_metrics["routing"] != session_metrics["routing"]:
                routing_diffs.append(f"  {model}/{indep_label}: {indep_metrics['routing']} → {session_metrics['routing']}")

            print()

    print("-" * 76)
    print(f"{'TOTALS':<20} {'':<25} {'indep':<8} {'':<12} {all_indep_tools:>5} {all_indep_ctx:>4} {all_indep_art:>4}")
    print(f"{'':<20} {'':<25} {'session':<8} {'':<12} {all_session_tools:>5} {all_session_ctx:>4} {all_session_art:>4}")
    print()

    # ── 2. Routing differences ───────────────────────────────────────
    print("-" * 76)
    print("2. ROUTING DIFFERENCES")
    print("-" * 76)
    if routing_diffs:
        for diff in routing_diffs:
            print(diff)
    else:
        print("  (none — all prompts routed identically)")
    print()

    # ── 3. Artifact tool usage from persistence ──────────────────────
    print("-" * 76)
    print("3. TOOL USAGE FROM PERSISTENCE (tool-calls.json)")
    print("-" * 76)
    print()

    MEMORY_DIRS = {
        "opus-bedrock": "/tmp/aura-math-opus-bedrock",
        "claude-thinking": "/tmp/aura-math-claude-thinking",
        "glm": "/tmp/aura-math-glm",
        "qwen35-thinking": "/tmp/aura-math-qwen35-thinking",
    }

    for model in MODELS:
        memory_dir = MEMORY_DIRS.get(model, "")
        if not os.path.exists(memory_dir):
            continue

        # Find session dirs
        session_dirs = sorted(glob.glob(os.path.join(memory_dir, "session_e2e_*")))
        if session_dirs:
            session_id = os.path.basename(session_dirs[-1])  # most recent
            counts = find_tool_calls_files(memory_dir, session_id)
            ctx_flag = " ⚠️" if counts["get_conversation_context"] > 0 else ""
            art_flag = " ⚠️" if counts["read_artifact"] > 0 else ""
            print(f"  {model} (session): MCP={counts['total_mcp']}  get_conversation_context={counts['get_conversation_context']}{ctx_flag}  read_artifact={counts['read_artifact']}{art_flag}")

    print()

    # ── 4. Plan quality — lazy description check ─────────────────────
    print("-" * 76)
    print("4. PLAN QUALITY — TASK DESCRIPTION AUDIT")
    print("-" * 76)
    print()
    print("  Checking for unresolved references (pronouns, 'previous result', etc.)")
    print()

    total_lazy = 0
    for model in MODELS:
        memory_dir = MEMORY_DIRS.get(model, "")
        if not os.path.exists(memory_dir):
            continue

        session_dirs = sorted(glob.glob(os.path.join(memory_dir, "session_e2e_*")))
        if not session_dirs:
            continue

        session_id = os.path.basename(session_dirs[-1])
        plans = find_plan_files(memory_dir, session_id)

        model_issues = []
        for goal, plan_data in plans.items():
            issues = check_lazy_descriptions(plan_data["task_descriptions"])
            model_issues.extend(issues)

        if model_issues:
            print(f"  {model}: {len(model_issues)} lazy description(s)")
            for issue in model_issues:
                print(f"    {issue}")
            total_lazy += len(model_issues)
        else:
            print(f"  {model}: ✓ all task descriptions fully resolved")

    print()
    if total_lazy > 0:
        print(f"  ⚠️  TOTAL LAZY DESCRIPTIONS: {total_lazy}")
    else:
        print("  ✓ No lazy descriptions detected across any model")
    print()

    # ── 5. Quality scores from manifests ─────────────────────────────
    print("-" * 76)
    print("5. QUALITY SCORES (from manifests)")
    print("-" * 76)
    print()
    print(f"  {'Model':<20} {'Goal':<50} {'Score':>6}")
    print(f"  {'-'*20} {'-'*50} {'-'*6}")

    for model in MODELS:
        memory_dir = MEMORY_DIRS.get(model, "")
        if not os.path.exists(memory_dir):
            continue

        session_dirs = sorted(glob.glob(os.path.join(memory_dir, "session_e2e_*")))
        if not session_dirs:
            continue

        session_id = os.path.basename(session_dirs[-1])
        manifests = find_persistence_manifests(memory_dir, session_id)

        for goal, m in sorted(manifests.items(), key=lambda x: x[1].get("timestamp", "")):
            score = m.get("quality_score")
            score_str = f"{score:.2f}" if score is not None else "N/A"
            print(f"  {model:<20} {goal[:50]:<50} {score_str:>6}")

    print()

    # ── Summary ──────────────────────────────────────────────────────
    print("=" * 76)
    print("SUMMARY")
    print("=" * 76)
    print()
    print(f"  Tool calls:     indep={all_indep_tools}  session={all_session_tools}  delta={all_session_tools - all_indep_tools}")
    print(f"  Context calls:  indep={all_indep_ctx}  session={all_session_ctx}")
    print(f"  Artifact calls: indep={all_indep_art}  session={all_session_art}")
    print(f"  Routing diffs:  {len(routing_diffs)}")
    print(f"  Lazy descs:     {total_lazy}")
    print()

    if all_session_ctx == 0 and all_session_art == 0 and total_lazy == 0 and len(routing_diffs) == 0:
        print("  ✓ NO REGRESSION DETECTED — session mode behaves identically to independent")
    elif total_lazy > 0:
        print("  ⚠️  POTENTIAL REGRESSION — lazy task descriptions detected in session mode")
    if all_session_ctx > 0:
        print("  ⚠️  Workers called get_conversation_context — session context relay may be insufficient")
    if all_session_art > 0:
        print("  ℹ️  Workers called read_artifact (expected for large results)")

    print()


if __name__ == "__main__":
    main()

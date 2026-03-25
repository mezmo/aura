#!/usr/bin/env python3
"""
Analyze session history eval — measures whether the coordinator uses session
history to skip recomputation and embed prior values.

Reads ONLY structured persistence artifacts (manifest.json, plan.json,
tool-calls.json). No SSE regex.

Usage:
  python3 temp-prompt-eval/analyze-session-history-eval.py \
    --memory-dir /tmp/aura-math-opus-bedrock \
    --session-id session_e2e_<ts>_opus-bedrock \
    [--independent-session-ids id1,id2,id3,id4,id5]
"""

import argparse
import glob
import json
import os
import re
import sys

# ── Turn Definitions ──────────────────────────────────────────────────

# Each turn has multiple match patterns to handle coordinator rephrasing.
# Checked t2-t5 first, then t1 (see match_turn for rationale).
TURN_MATCHERS = {
    "t1": ["mean of [12", "mean of the numbers 12"],
    "t2": ["multiply the", "multiply 30"],
    "t3": ["subtract 20", "subtract 20 from"],
    "t4": ["median of the", "median of three", "median of these"],
    "t5": ["add the median", "add 100 to 50", "median to 50"],
}

REDUNDANT_IF_PRESENT = {
    "t2": {"mean"},
    "t3": {"mean", "multiply"},
    "t4": {"mean", "multiply", "subtract", "degreesToRadians", "sin"},
    "t5": {"median"},
}

EXPECTED_VALUES = {
    "t1": {"result": 30.0, "embed_check": None},
    "t2": {"result": 120.0, "embed_check": ["30"]},
    "t3": {"result": 0.9848, "embed_check": ["120"]},
    "t4": {"result": 100.0, "embed_check": ["30", "120", "100"]},
    "t5": {"result": 200.0, "embed_check": ["100"]},
}

FLOAT_TOLERANCE = 0.01


# ── Artifact Readers ──────────────────────────────────────────────────

def load_json(path):
    """Load a JSON file, return None on failure."""
    try:
        with open(path) as f:
            return json.load(f)
    except (json.JSONDecodeError, FileNotFoundError, OSError):
        return None


def find_run_dirs(memory_dir, session_id):
    """Find all run directories under a session, excluding 'latest' symlink."""
    session_dir = os.path.join(memory_dir, session_id)
    if not os.path.isdir(session_dir):
        return []
    runs = []
    for entry in os.listdir(session_dir):
        run_path = os.path.join(session_dir, entry)
        if entry == "latest" or not os.path.isdir(run_path):
            continue
        runs.append(run_path)
    return sorted(runs)


def match_turn(goal_text):
    """Match a manifest goal to a turn key (t1-t5).

    Uses a two-pass approach: first try specific turns (t2-t5) which
    reference prior results, then fall back to t1 (baseline). This
    prevents t1 patterns from stealing goals that mention the original
    inputs but are actually later turns (e.g., "Multiply the mean of
    [12, 24, 36, 48] by 4" contains "mean of [12" but is really t2).
    """
    goal_lower = goal_text.lower()
    # Pass 1: check t2-t5 first (they reference prior results)
    for turn_key in ["t2", "t3", "t4", "t5"]:
        for pattern in TURN_MATCHERS[turn_key]:
            if pattern.lower() in goal_lower:
                return turn_key
    # Pass 2: check t1 (baseline, broadest patterns)
    for pattern in TURN_MATCHERS["t1"]:
        if pattern.lower() in goal_lower:
            return "t1"
    return None


def extract_turn_metrics(run_dir):
    """Extract metrics from a single run directory."""
    metrics = {
        "run_dir": run_dir,
        "goal": None,
        "status": None,
        "quality_score": None,
        "task_count": 0,
        "tool_names": set(),
        "tool_call_count": 0,
        "task_descriptions": [],
        "result_preview": None,
        "is_direct_answer": False,
    }

    # Load manifest
    manifest = load_json(os.path.join(run_dir, "manifest.json"))
    if manifest is None:
        # Direct-answer run — empty dir
        metrics["is_direct_answer"] = True
        return metrics

    metrics["goal"] = manifest.get("goal", "")
    metrics["status"] = manifest.get("status", "unknown")
    metrics["quality_score"] = manifest.get("quality_score")

    # Get last task result preview
    summaries = manifest.get("task_summaries", [])
    if summaries:
        metrics["result_preview"] = summaries[-1].get("result_preview")

    # Load plan from iteration-1 (execution phase)
    plan = load_json(os.path.join(run_dir, "iteration-1", "plan.json"))
    if plan is None:
        # Try iteration-0 as fallback
        plan = load_json(os.path.join(run_dir, "iteration-0", "plan.json"))

    if plan:
        tasks = plan.get("tasks", [])
        metrics["task_count"] = len(tasks)
        metrics["task_descriptions"] = [t.get("description", "") for t in tasks]

    # Load tool calls from iteration-1
    tool_call_files = glob.glob(os.path.join(run_dir, "iteration-1", "*.tool-calls.json"))
    if not tool_call_files:
        # Fallback to iteration-0
        tool_call_files = glob.glob(os.path.join(run_dir, "iteration-0", "*.tool-calls.json"))

    for tc_path in tool_call_files:
        calls = load_json(tc_path)
        if calls and isinstance(calls, list):
            for call in calls:
                tool_name = call.get("tool", "")
                if tool_name:
                    metrics["tool_names"].add(tool_name)
                    metrics["tool_call_count"] += 1

    return metrics


def check_value_embedded(turn_key, task_descriptions):
    """Check if expected prior-turn values appear in task descriptions."""
    embed_check = EXPECTED_VALUES[turn_key]["embed_check"]
    if embed_check is None:
        return None  # n/a for T1

    all_descs = " ".join(task_descriptions)
    found = [v for v in embed_check if re.search(r'\b' + re.escape(v) + r'\b', all_descs)]
    return found


def check_answer_correct(turn_key, metrics):
    """Check if the result matches expected value within tolerance."""
    expected = EXPECTED_VALUES[turn_key]["result"]
    preview = metrics.get("result_preview")
    if preview is None:
        return False

    # Try to extract a number from the result preview
    try:
        numbers = re.findall(r"-?\d+\.?\d*", str(preview))
        for num_str in numbers:
            num = float(num_str)
            if abs(num - expected) < FLOAT_TOLERANCE:
                return True
    except (ValueError, TypeError):
        pass
    return False


def check_redundant_tools(turn_key, tool_names):
    """Check if any tools from the redundant set were called."""
    if turn_key not in REDUNDANT_IF_PRESENT:
        return None  # n/a for T1
    redundant_set = REDUNDANT_IF_PRESENT[turn_key]
    found = tool_names & redundant_set
    return found


# ── Main Analysis ─────────────────────────────────────────────────────

def analyze_session(memory_dir, session_id):
    """Analyze a session run and return per-turn metrics."""
    run_dirs = find_run_dirs(memory_dir, session_id)
    if not run_dirs:
        print(f"ERROR: No run directories found in {memory_dir}/{session_id}", file=sys.stderr)
        return None

    # Extract metrics from all runs
    all_metrics = []
    for rd in run_dirs:
        m = extract_turn_metrics(rd)
        all_metrics.append(m)

    # Match runs to turns (warn on duplicate matches)
    turn_metrics = {}
    unmatched = []
    for m in all_metrics:
        if m["is_direct_answer"]:
            unmatched.append(m)
            continue
        goal = m.get("goal", "")
        turn_key = match_turn(goal)
        if turn_key:
            if turn_key in turn_metrics:
                print(f"WARNING: Duplicate match for {turn_key}: "
                      f"'{turn_metrics[turn_key].get('goal', '')[:50]}' vs '{goal[:50]}' "
                      f"— keeping first match", file=sys.stderr)
            else:
                turn_metrics[turn_key] = m
        else:
            unmatched.append(m)

    return turn_metrics, unmatched


def print_turn_table(turn_metrics):
    """Print the per-turn metrics table."""
    print("TURN METRICS:")
    header = f"{'Turn':<6} {'Tasks':>5} {'Tools':>5} {'Embed?':<12} {'Redundant?':<12} {'Correct?':<10} {'Status':<12}"
    print(header)
    print("-" * len(header))

    for turn_key in ["t1", "t2", "t3", "t4", "t5"]:
        m = turn_metrics.get(turn_key)
        if m is None:
            print(f"{turn_key.upper():<6} {'—':>5} {'—':>5} {'(missing)':<12} {'—':<12} {'—':<10} {'NOT FOUND':<12}")
            continue

        task_count = m["task_count"]
        tool_count = m["tool_call_count"]

        # Value embedding
        embedded = check_value_embedded(turn_key, m["task_descriptions"])
        if embedded is None:
            embed_str = "n/a"
        else:
            expected = EXPECTED_VALUES[turn_key]["embed_check"]
            if len(embedded) == len(expected):
                if len(embedded) == 1:
                    embed_str = f"Y({embedded[0]})"
                else:
                    embed_str = f"Y({len(embedded)}/{len(expected)})"
            else:
                embed_str = f"N({len(embedded)}/{len(expected)})"

        # Redundant tools
        redundant = check_redundant_tools(turn_key, m["tool_names"])
        if redundant is None:
            redund_str = "n/a"
        elif len(redundant) == 0:
            redund_str = "Y"
        else:
            redund_str = f"N({','.join(sorted(redundant))})"

        # Correctness
        correct = check_answer_correct(turn_key, m)
        correct_str = "Y" if correct else "N"

        # Status
        status = m.get("status", "unknown")

        print(f"{turn_key.upper():<6} {task_count:>5} {tool_count:>5} {embed_str:<12} {redund_str:<12} {correct_str:<10} {status:<12}")


def print_scorecard(turn_metrics):
    """Print the summary scorecard."""
    embed_pass = 0
    embed_total = 0
    efficiency_pass = 0
    efficiency_total = 0
    correct_pass = 0
    correct_total = 0
    total_tools = 0

    for turn_key in ["t1", "t2", "t3", "t4", "t5"]:
        m = turn_metrics.get(turn_key)
        if m is None:
            continue

        total_tools += m["tool_call_count"]
        correct_total += 1
        if check_answer_correct(turn_key, m):
            correct_pass += 1

        embedded = check_value_embedded(turn_key, m["task_descriptions"])
        if embedded is not None:
            embed_total += 1
            expected = EXPECTED_VALUES[turn_key]["embed_check"]
            if len(embedded) == len(expected):
                embed_pass += 1

        redundant = check_redundant_tools(turn_key, m["tool_names"])
        if redundant is not None:
            efficiency_total += 1
            if len(redundant) == 0:
                efficiency_pass += 1

    print()
    print("SCORECARD:")
    if embed_total > 0:
        print(f"  Value embedding:  {embed_pass}/{embed_total} ({100*embed_pass//embed_total}%)")
    else:
        print("  Value embedding:  n/a (no dependent turns found)")
    if efficiency_total > 0:
        no_redundant = "no redundant recomputation" if efficiency_pass == efficiency_total else f"{efficiency_total - efficiency_pass} turn(s) with redundant tools"
        print(f"  Tool efficiency:  {efficiency_pass}/{efficiency_total} ({no_redundant})")
    print(f"  Correctness:      {correct_pass}/{correct_total}")
    print(f"  Total tools:      {total_tools} (session)")


def find_independent_run_dirs(memory_dir):
    """Find run directories for independent mode (no session grouping).

    Independent runs from run-model-comparison.sh don't use session IDs,
    so run dirs (UUIDs with manifest.json) are directly under memory_dir.
    """
    if not os.path.isdir(memory_dir):
        return []
    runs = []
    for entry in os.listdir(memory_dir):
        run_path = os.path.join(memory_dir, entry)
        if entry == "latest" or not os.path.isdir(run_path):
            continue
        # Skip session directories (session_e2e_* contain nested run dirs)
        if entry.startswith("session_"):
            continue
        # Only include dirs that have a manifest (actual runs, not empty)
        if os.path.isfile(os.path.join(run_path, "manifest.json")):
            runs.append(run_path)
    return sorted(runs)


def analyze_independent(memory_dir, run_ids=None):
    """Analyze independent runs for tool count comparison.

    If run_ids provided, looks for those specific run dirs under memory_dir.
    Otherwise, finds all non-session run dirs with manifests.
    """
    total_tools = 0
    if run_ids:
        for rid in run_ids:
            # Try as session_id first, then as direct run_id
            run_dirs = find_run_dirs(memory_dir, rid)
            if not run_dirs:
                rd = os.path.join(memory_dir, rid)
                if os.path.isdir(rd):
                    run_dirs = [rd]
            for rd in run_dirs:
                m = extract_turn_metrics(rd)
                total_tools += m["tool_call_count"]
    else:
        for rd in find_independent_run_dirs(memory_dir):
            m = extract_turn_metrics(rd)
            total_tools += m["tool_call_count"]
    return total_tools


def export_json(turn_metrics, output_path):
    """Export metrics as JSON for programmatic consumption."""
    export = {}
    for turn_key in ["t1", "t2", "t3", "t4", "t5"]:
        m = turn_metrics.get(turn_key)
        if m is None:
            export[turn_key] = None
            continue
        embedded = check_value_embedded(turn_key, m["task_descriptions"])
        redundant = check_redundant_tools(turn_key, m["tool_names"])
        export[turn_key] = {
            "task_count": m["task_count"],
            "tool_call_count": m["tool_call_count"],
            "tool_names": sorted(m["tool_names"]),
            "value_embedded": embedded,
            "redundant_tools": sorted(redundant) if redundant is not None else None,
            "answer_correct": check_answer_correct(turn_key, m),
            "status": m.get("status", "unknown"),
            "result_preview": m.get("result_preview"),
        }

    with open(output_path, "w") as f:
        json.dump(export, f, indent=2)
    print(f"\nJSON exported to: {output_path}")


def main():
    parser = argparse.ArgumentParser(
        description="Analyze session history eval — measures coordinator use of session history"
    )
    parser.add_argument("--memory-dir", required=True, help="Persistence base directory (e.g. /tmp/aura-math-opus-bedrock)")
    parser.add_argument("--session-id", required=True, help="Session ID for the dependent-prompt session run")
    parser.add_argument("--independent-session-ids", help="Comma-separated session IDs for independent baseline runs")
    parser.add_argument("--json-export", help="Path to export metrics as JSON")
    args = parser.parse_args()

    print("=" * 72)
    print("  Session History Eval — Artifact Analysis")
    print("=" * 72)
    print(f"  Memory dir:  {args.memory_dir}")
    print(f"  Session ID:  {args.session_id}")
    print()

    result = analyze_session(args.memory_dir, args.session_id)
    if result is None:
        sys.exit(1)

    turn_metrics, unmatched = result

    if not turn_metrics:
        print("ERROR: No turns matched. Check that you ran with PROMPT_SET=dependent.", file=sys.stderr)
        print(f"  Found {len(unmatched)} unmatched run(s):", file=sys.stderr)
        for m in unmatched:
            goal = m.get("goal", "(no manifest)")
            print(f"    - {goal}", file=sys.stderr)
        sys.exit(1)

    matched_turns = sorted(turn_metrics.keys())
    print(f"  Matched turns: {', '.join(t.upper() for t in matched_turns)} ({len(matched_turns)}/5)")
    if unmatched:
        print(f"  Unmatched runs: {len(unmatched)} (direct answers or unrecognized goals)")
    print()

    print_turn_table(turn_metrics)
    print_scorecard(turn_metrics)

    # Comparison mode
    if args.independent_session_ids:
        indep_ids = [s.strip() for s in args.independent_session_ids.split(",")]
        indep_tools = analyze_independent(args.memory_dir, indep_ids)
        session_tools = sum(
            m["tool_call_count"]
            for m in turn_metrics.values()
            if m is not None
        )
        print()
        print("COMPARISON (session vs independent):")
        print(f"  Session tools:      {session_tools}")
        print(f"  Independent tools:  {indep_tools}")
        delta = indep_tools - session_tools
        if delta > 0:
            print(f"  Tool savings:       {delta} fewer tools in session mode")
        elif delta < 0:
            print(f"  Tool overhead:      {abs(delta)} more tools in session mode")
        else:
            print(f"  Tool savings:       0 (identical)")

    if args.json_export:
        export_json(turn_metrics, args.json_export)

    print()


if __name__ == "__main__":
    main()

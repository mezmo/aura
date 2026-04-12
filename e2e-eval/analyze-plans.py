#!/usr/bin/env python3
"""Analyze plan execution artifacts across iterations.

Reads persistence artifacts (plan.json, manifest.json, task result files,
summary.json) to report on plan structure, task reuse, iteration patterns,
and execution efficiency.

Usage:
  # Analyze all runs for a model
  python3 e2e-eval/analyze-plans.py /tmp/aura-math-gpt5-thinking

  # Analyze a specific run
  python3 e2e-eval/analyze-plans.py /tmp/aura-math-gpt5-thinking --run-id <uuid>

  # Only show multi-iteration runs (where replans happened)
  python3 e2e-eval/analyze-plans.py /tmp/aura-math-gpt5-thinking --replans-only

  # JSON export for programmatic consumption
  python3 e2e-eval/analyze-plans.py /tmp/aura-math-gpt5-thinking --json
"""

import argparse
import json
import os
import sys
from collections import defaultdict


# ── Artifact Readers ─────────────────────────────────────────────────


def load_json(path):
    """Load a JSON file, return None on failure."""
    try:
        with open(path) as f:
            return json.load(f)
    except (json.JSONDecodeError, FileNotFoundError, OSError):
        return None


def find_run_dirs(memory_dir):
    """Find all run directories (those containing manifest.json)."""
    runs = []
    for session_entry in os.listdir(memory_dir):
        session_path = os.path.join(memory_dir, session_entry)
        if not os.path.isdir(session_path):
            continue
        # Check if this is a run dir directly (has manifest.json)
        if os.path.isfile(os.path.join(session_path, "manifest.json")):
            runs.append(session_path)
            continue
        # Otherwise look one level deeper (session_id/run_id/)
        for run_entry in os.listdir(session_path):
            run_path = os.path.join(session_path, run_entry)
            if run_entry == "latest" or not os.path.isdir(run_path):
                continue
            if os.path.isfile(os.path.join(run_path, "manifest.json")):
                runs.append(run_path)
    return sorted(runs)


def find_iteration_dirs(run_dir):
    """Find iteration-N directories in a run, sorted by iteration number."""
    iters = []
    for entry in os.listdir(run_dir):
        if entry.startswith("iteration-") and os.path.isdir(os.path.join(run_dir, entry)):
            try:
                n = int(entry.split("-")[1])
                iters.append((n, os.path.join(run_dir, entry)))
            except (ValueError, IndexError):
                pass
    return sorted(iters, key=lambda x: x[0])


# ── Per-Iteration Analysis ───────────────────────────────────────────


def analyze_iteration(iter_dir, iter_num):
    """Extract plan execution details from a single iteration directory."""
    result = {
        "iteration": iter_num,
        "task_count": 0,
        "tasks": [],
        "reuse_field_count": 0,
        "reuse_text_count": 0,
        "fresh_count": 0,
        "parallel_groups": 0,
        "sub_chains": 0,
        "workers_used": set(),
        "total_worker_duration_ms": 0,
        "reuse_worker_duration_ms": 0,
        "quality_score": None,
        "will_replan": None,
        "eval_gaps": [],
    }

    # Load plan
    plan = load_json(os.path.join(iter_dir, "plan.json"))
    if plan is None:
        return result

    tasks = plan.get("tasks", [])
    steps = plan.get("steps", [])
    result["task_count"] = len(tasks)

    # Analyze step structure (parallel groups, sub-chains)
    result["parallel_groups"], result["sub_chains"] = count_step_structure(steps)

    # Analyze each task
    for task in tasks:
        tid = task.get("id", 0)
        desc = task.get("description", "")
        status = task.get("status", "unknown")
        worker = task.get("worker")
        reuse_from = task.get("reuse_result_from")
        deps = task.get("dependencies", [])

        if worker:
            result["workers_used"].add(worker)

        # Detect reuse: field-based vs text-based vs fresh
        reuse_type = "fresh"
        if reuse_from is not None:
            reuse_type = "field"
            result["reuse_field_count"] += 1
        elif _is_text_reuse(desc):
            reuse_type = "text"
            result["reuse_text_count"] += 1
        else:
            result["fresh_count"] += 1

        # Load task result for duration, and separate tool-calls file for count
        task_result = _load_task_result(iter_dir, tid)
        duration_ms = task_result.get("duration_ms", 0) if task_result else 0
        tool_call_count = _count_task_tool_calls(iter_dir, tid)

        if duration_ms:
            result["total_worker_duration_ms"] += duration_ms
            if reuse_type in ("field", "text"):
                result["reuse_worker_duration_ms"] += duration_ms

        result["tasks"].append({
            "id": tid,
            "description": _truncate(desc, 80),
            "status": status,
            "worker": worker,
            "reuse_type": reuse_type,
            "reuse_result_from": reuse_from,
            "dependencies": deps,
            "duration_ms": duration_ms,
            "tool_calls": tool_call_count,
        })

    # Load evaluation / summary
    summary = load_json(os.path.join(iter_dir, "summary.json"))
    if summary:
        result["quality_score"] = summary.get("quality_score")
        result["will_replan"] = summary.get("will_replan")

    eval_result = load_json(os.path.join(iter_dir, "evaluation.result.json"))
    if eval_result:
        result["eval_gaps"] = eval_result.get("gaps", [])
        # Prefer eval score if summary didn't have it
        if result["quality_score"] is None:
            result["quality_score"] = eval_result.get("score")

    return result


def count_step_structure(steps):
    """Count parallel groups and sub-chains in step tree."""
    parallel = 0
    sub_chains = 0
    for step in steps:
        if isinstance(step, dict):
            if "parallel" in step:
                parallel += 1
                p, s = count_step_structure(step["parallel"])
                parallel += p
                sub_chains += s
            elif "steps" in step:
                sub_chains += 1
                p, s = count_step_structure(step["steps"])
                parallel += p
                sub_chains += s
    return parallel, sub_chains


def _is_text_reuse(description):
    """Detect text-based reuse encoding in task descriptions."""
    lower = description.lower()
    return (
        "reuse_result_from" in lower
        or "reuse the previously" in lower
        or "carry forward" in lower
        or "(reuse" in lower
    )


def _load_task_result(iter_dir, task_id):
    """Load the task result JSON for a given task ID."""
    # Try attempt-1 first (most common)
    for attempt in range(1, 4):
        path = os.path.join(iter_dir, f"task-{task_id}.attempt-{attempt}.result.json")
        result = load_json(path)
        if result is not None:
            return result
    return None


def _count_task_tool_calls(iter_dir, task_id):
    """Count tool calls from the separate tool-calls.json file."""
    for attempt in range(1, 4):
        path = os.path.join(iter_dir, f"task-{task_id}.attempt-{attempt}.tool-calls.json")
        data = load_json(path)
        if data is not None and isinstance(data, list):
            return len(data)
    return 0


def _truncate(s, max_len):
    """Truncate string with ellipsis."""
    if len(s) <= max_len:
        return s
    return s[:max_len - 3] + "..."


# ── Per-Run Analysis ─────────────────────────────────────────────────


def analyze_run(run_dir):
    """Analyze all iterations in a run directory."""
    manifest = load_json(os.path.join(run_dir, "manifest.json"))
    if manifest is None:
        return None

    iter_dirs = find_iteration_dirs(run_dir)
    if not iter_dirs:
        return None

    iterations = []
    for iter_num, iter_path in iter_dirs:
        iterations.append(analyze_iteration(iter_path, iter_num))

    # Compute cross-iteration diff
    diffs = []
    for i in range(1, len(iterations)):
        diffs.append(compute_iteration_diff(iterations[i - 1], iterations[i]))

    return {
        "run_dir": run_dir,
        "run_id": manifest.get("run_id", os.path.basename(run_dir)),
        "goal": manifest.get("goal", ""),
        "status": manifest.get("status", "unknown"),
        "iteration_count": len(iterations),
        "quality_score": manifest.get("quality_score"),
        "routing_mode": manifest.get("routing_mode"),
        "iterations": iterations,
        "diffs": diffs,
    }


def compute_iteration_diff(prev, curr):
    """Compare two iterations to understand what changed."""
    prev_descs = {t["id"]: t["description"] for t in prev["tasks"]}
    curr_descs = {t["id"]: t["description"] for t in curr["tasks"]}

    # Tasks only in prev (dropped)
    dropped = set(prev_descs.keys()) - set(curr_descs.keys())
    # Tasks only in curr (new)
    added = set(curr_descs.keys()) - set(prev_descs.keys())
    # Tasks in both (potentially modified)
    common = set(prev_descs.keys()) & set(curr_descs.keys())
    modified = {tid for tid in common if prev_descs[tid] != curr_descs[tid]}

    return {
        "from_iteration": prev["iteration"],
        "to_iteration": curr["iteration"],
        "tasks_dropped": len(dropped),
        "tasks_added": len(added),
        "tasks_modified": len(modified),
        "tasks_unchanged": len(common) - len(modified),
        "prev_task_count": prev["task_count"],
        "curr_task_count": curr["task_count"],
        "reuse_field": curr["reuse_field_count"],
        "reuse_text": curr["reuse_text_count"],
        "fresh": curr["fresh_count"],
    }


# ── Display ──────────────────────────────────────────────────────────


def print_run_summary(run):
    """Print a compact summary for a single run."""
    goal = _truncate(run["goal"], 90)
    iters = run["iteration_count"]
    status = run["status"]
    q = run["quality_score"]
    q_str = f"{q:.2f}" if q is not None else "—"

    print(f"  Goal:    {goal}")
    print(f"  Status:  {status}  Quality: {q_str}  Iterations: {iters}")

    for it in run["iterations"]:
        n = it["iteration"]
        tc = it["task_count"]
        reuse_f = it["reuse_field_count"]
        reuse_t = it["reuse_text_count"]
        fresh = it["fresh_count"]
        total_ms = it["total_worker_duration_ms"]
        reuse_ms = it["reuse_worker_duration_ms"]
        q_score = it["quality_score"]
        will_rp = it["will_replan"]
        gaps = it["eval_gaps"]

        q_str = f"{q_score:.2f}" if q_score is not None else "—"
        rp_str = "replan" if will_rp else ("done" if will_rp is not None else "—")

        # Reuse summary
        reuse_parts = []
        if reuse_f > 0:
            reuse_parts.append(f"{reuse_f} field")
        if reuse_t > 0:
            reuse_parts.append(f"{reuse_t} text")
        reuse_str = f"reuse={'+'.join(reuse_parts)}" if reuse_parts else ""
        fresh_str = f"fresh={fresh}" if fresh > 0 else ""
        task_breakdown = ", ".join(filter(None, [fresh_str, reuse_str]))

        print(f"  iter-{n}: {tc} tasks ({task_breakdown})  "
              f"Q={q_str} [{rp_str}]  "
              f"worker_time={total_ms}ms", end="")
        if reuse_ms > 0:
            print(f" (reuse_waste={reuse_ms}ms)", end="")
        print()

        # Warn on text-based reuse in replan iterations (iter > 1)
        if reuse_t > 0 and n > 1:
            print(f"         WARNING: {reuse_t} task(s) using text-based reuse — "
                  f"coordinator not using reuse_result_from field ({reuse_ms}ms wasted)")

        # Show gaps if any
        if gaps:
            for gap in gaps[:3]:
                print(f"         gap: {_truncate(str(gap), 70)}")

    # Show diffs
    for diff in run["diffs"]:
        f_iter = diff["from_iteration"]
        t_iter = diff["to_iteration"]
        print(f"  diff {f_iter}→{t_iter}: "
              f"+{diff['tasks_added']} added, "
              f"-{diff['tasks_dropped']} dropped, "
              f"~{diff['tasks_modified']} modified, "
              f"={diff['tasks_unchanged']} unchanged  "
              f"[field_reuse={diff['reuse_field']}, text_reuse={diff['reuse_text']}, fresh={diff['fresh']}]")


def print_run_detail(run):
    """Print detailed per-task breakdown for a run."""
    print_run_summary(run)
    print()

    for it in run["iterations"]:
        n = it["iteration"]
        print(f"  ── Iteration {n} Tasks ──")
        print(f"  {'ID':>3} {'Status':<10} {'Reuse':<8} {'Worker':<14} {'Dur(ms)':>8} {'Tools':>5} Description")
        print(f"  {'—'*3} {'—'*10} {'—'*8} {'—'*14} {'—'*8} {'—'*5} {'—'*40}")
        for t in it["tasks"]:
            tid = t["id"]
            status = t["status"]
            reuse = t["reuse_type"]
            if reuse == "field":
                reuse = f"←{t['reuse_result_from']}"
            elif reuse == "text":
                reuse = "~text"
            else:
                reuse = "—"
            worker = t["worker"] or "—"
            dur = t["duration_ms"] or 0
            tools = t["tool_calls"]
            desc = t["description"]
            print(f"  {tid:>3} {status:<10} {reuse:<8} {worker:<14} {dur:>8} {tools:>5} {desc}")
        print()


def print_aggregate(runs):
    """Print aggregate statistics across all runs."""
    total_runs = len(runs)
    multi_iter = [r for r in runs if r["iteration_count"] > 1]
    single_iter = [r for r in runs if r["iteration_count"] == 1]

    # Aggregate reuse stats across all iterations
    total_field_reuse = 0
    total_text_reuse = 0
    total_fresh = 0
    total_reuse_waste_ms = 0
    total_worker_ms = 0
    iter_counts = defaultdict(int)

    for run in runs:
        iter_counts[run["iteration_count"]] += 1
        for it in run["iterations"]:
            total_field_reuse += it["reuse_field_count"]
            total_text_reuse += it["reuse_text_count"]
            total_fresh += it["fresh_count"]
            total_reuse_waste_ms += it["reuse_worker_duration_ms"]
            total_worker_ms += it["total_worker_duration_ms"]

    print("=" * 72)
    print("  Plan Execution Analysis — Aggregate")
    print("=" * 72)
    print(f"  Total runs:          {total_runs}")
    print(f"  Single-iteration:    {len(single_iter)}")
    print(f"  Multi-iteration:     {len(multi_iter)}")
    iter_dist = ", ".join(f"{c}x {n}-iter" for n, c in sorted(iter_counts.items()))
    print(f"  Iteration dist:      {iter_dist}")
    print()
    print(f"  Total tasks (all iterations):")
    print(f"    Fresh executions:  {total_fresh}")
    print(f"    Field reuse:       {total_field_reuse}")
    if total_text_reuse > 0:
        print(f"    Text reuse:        {total_text_reuse}  ** WARNING: not using reuse_result_from field **")
    else:
        print(f"    Text reuse:        {total_text_reuse}")
    total_reuse = total_field_reuse + total_text_reuse
    total_tasks = total_fresh + total_reuse
    if total_tasks > 0:
        print(f"    Reuse rate:        {total_reuse}/{total_tasks} ({100*total_reuse/total_tasks:.0f}%)")
    print()
    if total_worker_ms > 0:
        print(f"  Worker time:")
        print(f"    Total:             {total_worker_ms}ms")
        if total_reuse_waste_ms > 0:
            print(f"    Reuse waste:       {total_reuse_waste_ms}ms ({100*total_reuse_waste_ms/total_worker_ms:.0f}% of total)")
            print(f"    Savings potential: {total_reuse_waste_ms}ms if field reuse had been available")
        print()

    # Quality distribution
    q_scores = [r["quality_score"] for r in runs if r["quality_score"] is not None]
    if q_scores:
        q_avg = sum(q_scores) / len(q_scores)
        q_min = min(q_scores)
        q_max = max(q_scores)
        print(f"  Quality scores:      avg={q_avg:.2f}  min={q_min:.2f}  max={q_max:.2f}")

    # Success rate
    success = sum(1 for r in runs if r["status"] == "success")
    print(f"  Success rate:        {success}/{total_runs}")
    print()


def print_replan_detail(runs):
    """Print detailed analysis of multi-iteration runs."""
    multi = [r for r in runs if r["iteration_count"] > 1]
    if not multi:
        print("  No multi-iteration runs found.\n")
        return

    print("=" * 72)
    print(f"  Replan Analysis — {len(multi)} multi-iteration run(s)")
    print("=" * 72)

    for run in multi:
        print()
        print(f"  Run: {run['run_id'][:12]}...")
        print_run_detail(run)
        print("-" * 72)


# ── Main ─────────────────────────────────────────────────────────────


def main():
    parser = argparse.ArgumentParser(
        description="Analyze plan execution artifacts from persistence directories"
    )
    parser.add_argument("memory_dir", help="Persistence base directory (e.g. /tmp/aura-math-gpt5-thinking)")
    parser.add_argument("--run-id", help="Analyze a specific run ID only")
    parser.add_argument("--replans-only", action="store_true", help="Only show multi-iteration runs")
    parser.add_argument("--detail", action="store_true", help="Show per-task detail for all runs")
    parser.add_argument("--json", action="store_true", help="Output JSON instead of tables")
    args = parser.parse_args()

    if not os.path.isdir(args.memory_dir):
        print(f"ERROR: Not a directory: {args.memory_dir}", file=sys.stderr)
        sys.exit(1)

    run_dirs = find_run_dirs(args.memory_dir)
    if not run_dirs:
        print(f"ERROR: No run directories found in {args.memory_dir}", file=sys.stderr)
        sys.exit(1)

    # Filter to specific run if requested
    if args.run_id:
        run_dirs = [d for d in run_dirs if args.run_id in d]
        if not run_dirs:
            print(f"ERROR: Run ID '{args.run_id}' not found", file=sys.stderr)
            sys.exit(1)

    # Analyze all runs
    runs = []
    for rd in run_dirs:
        run = analyze_run(rd)
        if run is not None:
            runs.append(run)

    if not runs:
        print("No analyzable runs found.", file=sys.stderr)
        sys.exit(1)

    # Filter to replans only
    if args.replans_only:
        runs = [r for r in runs if r["iteration_count"] > 1]
        if not runs:
            print("No multi-iteration runs found.", file=sys.stderr)
            sys.exit(0)

    # JSON output
    if args.json:
        # Convert sets to lists for JSON serialization
        for run in runs:
            for it in run["iterations"]:
                it["workers_used"] = sorted(it["workers_used"])
        json.dump(runs, sys.stdout, indent=2)
        print()
        sys.exit(0)

    # Print aggregate
    print_aggregate(runs)

    # Print per-run details
    if args.detail:
        for run in runs:
            print_run_detail(run)
            print()
    elif args.replans_only:
        print_replan_detail(runs)
    else:
        # Default: aggregate + replan detail
        print_replan_detail(runs)

        # Also list single-iteration runs briefly
        singles = [r for r in runs if r["iteration_count"] == 1]
        if singles:
            print(f"  Single-iteration runs: {len(singles)} (use --detail to expand)")
            print()


if __name__ == "__main__":
    main()

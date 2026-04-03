#!/usr/bin/env python3
"""Parse E2E comparison results from SSE capture files.

Re-derives tool calls, timeouts, and completion from raw SSE files,
independent of any CSV the runner may have produced. Includes loop
detection via duplicate tool call errors and cross-model outlier analysis.

Usage:
    python3 e2e-eval/parse-results.py <results-dir>
    python3 e2e-eval/parse-results.py <results-dir> --csv
"""
import argparse
import csv
import json
import sys
from collections import defaultdict
from pathlib import Path
from statistics import median

# Allow importing from same directory when run as script
sys.path.insert(0, str(Path(__file__).resolve().parent))
from sse_parser import effective_routing_mode, parse_sse_file


def collect_results(results_dir: Path) -> list[dict]:
    """Walk the results directory and parse all SSE files."""
    rows = []
    for model_dir in sorted(results_dir.iterdir()):
        if not model_dir.is_dir() or model_dir.name in ("__pycache__", "results.csv"):
            continue
        model = model_dir.name
        for iter_dir in sorted(model_dir.iterdir()):
            if not iter_dir.is_dir() or not iter_dir.name.startswith("iter-"):
                continue
            iteration = int(iter_dir.name.split("-")[1])
            for sse_file in sorted(iter_dir.glob("*.sse")):
                label = sse_file.stem
                stats = parse_sse_file(sse_file)
                rows.append({
                    "model": model,
                    "prompt": label,
                    "iteration": iteration,
                    **stats,
                })
    return rows


def merge_timing(rows: list[dict], results_dir: Path) -> list[dict]:
    """Try to merge elapsed_ms from the runner's CSV (timing is only there)."""
    csv_path = results_dir / "results.csv"
    if not csv_path.exists():
        return rows

    timing = {}
    try:
        text = csv_path.read_text()
        for line in text.splitlines():
            parts = line.split(",")
            if len(parts) >= 4 and parts[0] != "model":
                try:
                    key = (parts[0], parts[1], int(parts[2]))
                    elapsed = int(parts[3])
                    timing[key] = elapsed
                except (ValueError, IndexError):
                    continue
    except Exception:
        return rows

    for row in rows:
        key = (row["model"], row["prompt"], row["iteration"])
        row["elapsed_ms"] = timing.get(key, 0)

    return rows


def detect_loops(rows: list[dict]) -> list[dict]:
    """Detect tool looping via three signals and return flagged entries."""
    flags = []
    prompts = sorted(set(r["prompt"] for r in rows))
    models = sorted(set(r["model"] for r in rows))

    for prompt in prompts:
        prompt_rows = [r for r in rows if r["prompt"] == prompt]

        # Per-model average tool counts for this prompt
        model_avgs = {}
        for model in models:
            mr = [r for r in prompt_rows if r["model"] == model]
            if mr:
                model_avgs[model] = sum(r["tool_calls"] for r in mr) / len(mr)

        if not model_avgs:
            continue

        med_tools = median(model_avgs.values()) if len(model_avgs) > 1 else 0

        for model in models:
            mr = [r for r in prompt_rows if r["model"] == model]
            if not mr:
                continue

            avg_tools = model_avgs[model]
            total_dupes = sum(r["duplicate_tool_calls"] for r in mr)
            total_failed = sum(r["failed_tool_calls"] for r in mr)

            reasons = []

            # Signal 1: duplicate tool call errors detected by aura
            if total_dupes > 0:
                reasons.append(f"{total_dupes} duplicate tool call errors")

            # Signal 2: cross-model outlier (>2x median, min 4 above median)
            if med_tools > 0 and avg_tools > med_tools * 2 and avg_tools - med_tools >= 4:
                reasons.append(
                    f"tool count {avg_tools:.0f} is {avg_tools/med_tools:.1f}x "
                    f"the cross-model median ({med_tools:.0f})"
                )

            # Signal 3: high failed tool ratio
            total_tools = sum(r["tool_calls"] for r in mr)
            if total_tools > 0 and total_failed / total_tools > 0.3:
                reasons.append(
                    f"{total_failed}/{total_tools} tool calls failed "
                    f"({total_failed/total_tools:.0%})"
                )

            if reasons:
                flags.append({
                    "model": model,
                    "prompt": prompt,
                    "avg_tools": avg_tools,
                    "median_tools": med_tools,
                    "duplicate_errors": total_dupes,
                    "failed_calls": total_failed,
                    "reasons": reasons,
                })

    return flags


def print_summary(rows: list[dict]):
    """Print aggregate summary tables."""
    models = sorted(set(r["model"] for r in rows))
    prompts = sorted(set(r["prompt"] for r in rows))

    # ── Model-level summary ─────────────────────────────────────────
    print(f"{'Model':<20} {'Avg ms':>8} {'Med ms':>8} {'P95 ms':>8} "
          f"{'Tools':>6} {'Dupes':>6} {'Tasks':>6} {'TOs':>4} {'Done':>8} {'Route':>10}")
    print("-" * 98)

    for model in models:
        mr = [r for r in rows if r["model"] == model]
        times = sorted(r.get("elapsed_ms", 0) for r in mr)
        n = len(times)
        avg = sum(times) // n if n else 0
        med = times[n // 2] if n else 0
        p95 = times[int(n * 0.95)] if n else 0
        tools = sum(r["tool_calls"] for r in mr)
        dupes = sum(r["duplicate_tool_calls"] for r in mr)
        tasks = sum(r["tasks_completed"] for r in mr)
        tos = sum(1 for r in mr if r["timeout"])
        done = sum(1 for r in mr if r["completed"])
        # Routing mode distribution
        route_counts = defaultdict(int)
        for r in mr:
            rm = effective_routing_mode(r)
            route_counts[rm] += 1
        route_str = "/".join(f"{c}{k[0].upper()}" for k, c in sorted(route_counts.items()))
        print(f"{model:<20} {avg:>8} {med:>8} {p95:>8} "
              f"{tools:>6} {dupes:>6} {tasks:>6} {tos:>4} {done:>5}/{n} {route_str:>10}")

    print()

    # ── Per-prompt breakdown ────────────────────────────────────────
    print(f"{'Model':<20} {'Prompt':<22} {'Route':<12} {'Avg ms':>8} {'Tools':>6} "
          f"{'Dupes':>6} {'Tasks':>6} {'TOs':>4} {'OK':>6}")
    print("-" * 96)
    for model in models:
        for prompt in prompts:
            pr = [r for r in rows if r["model"] == model and r["prompt"] == prompt]
            if not pr:
                continue
            times = [r.get("elapsed_ms", 0) for r in pr]
            avg = sum(times) // len(times)
            tools = sum(r["tool_calls"] for r in pr)
            dupes = sum(r["duplicate_tool_calls"] for r in pr)
            tasks = sum(r["tasks_completed"] for r in pr)
            to = sum(1 for r in pr if r["timeout"])
            ok = sum(1 for r in pr if r["completed"])
            # Most common routing mode for this prompt
            route = effective_routing_mode(pr[0])
            flag = " ⚠ LOOP" if dupes > 0 else ""
            print(f"{model:<20} {prompt:<22} {route:<12} {avg:>8} {tools:>6} "
                  f"{dupes:>6} {tasks:>6} {to:>4} {ok:>4}/{len(pr)}{flag}")
        print()

    # ── Routing rationale breakdown ─────────────────────────────
    has_rationale = any(r.get("routing_rationale") for r in rows)
    if has_rationale:
        print(f"{'Model':<20} {'Prompt':<22} {'Route':<12} Rationale")
        print("-" * 100)
        for model in models:
            for prompt in prompts:
                pr = [r for r in rows if r["model"] == model and r["prompt"] == prompt]
                if not pr:
                    continue
                route = effective_routing_mode(pr[0])
                # Show first iteration's rationale (truncated)
                rationale = pr[0].get("routing_rationale") or ""
                if len(rationale) > 60:
                    rationale = rationale[:57] + "..."
                print(f"{model:<20} {prompt:<22} {route:<12} {rationale}")
            print()

    # ── Routing consistency (flag non-deterministic routing) ──────
    # For each (model, prompt), check if routing_mode is the same across all iterations.
    inconsistencies = []
    for model in models:
        for prompt in prompts:
            pr = [r for r in rows if r["model"] == model and r["prompt"] == prompt]
            if len(pr) < 2:
                continue
            routes = [effective_routing_mode(r) for r in pr]
            unique = set(routes)
            if len(unique) > 1:
                counts = {u: routes.count(u) for u in sorted(unique)}
                inconsistencies.append((model, prompt, counts))

    if inconsistencies:
        print(f"Routing Consistency: {len(inconsistencies)} INCONSISTENCY(S)\n")
        print(f"{'Model':<20} {'Prompt':<22} Distribution")
        print("-" * 70)
        for model, prompt, counts in inconsistencies:
            dist = ", ".join(f"{v}x {k}" for k, v in sorted(counts.items()))
            print(f"{model:<20} {prompt:<22} {dist}")
        print()
    else:
        print("Routing Consistency: STABLE — all models route consistently across iterations\n")

    # ── Per-worker tool call breakdown ────────────────────────────
    has_worker_data = any(r.get("tools_by_worker") for r in rows)
    if has_worker_data:
        # Collect all worker names across all rows
        all_workers = sorted(set(
            w for r in rows
            for w in r.get("tools_by_worker", {}).keys()
        ))
        if all_workers:
            worker_hdrs = "".join(f"{w[:12]:>14}" for w in all_workers)
            print(f"{'Model':<20} {'Prompt':<22} {worker_hdrs}")
            print("-" * (42 + 14 * len(all_workers)))
            for model in models:
                for prompt in prompts:
                    pr = [r for r in rows
                          if r["model"] == model and r["prompt"] == prompt]
                    if not pr:
                        continue
                    # Sum per-worker totals across iterations
                    worker_sums = defaultdict(int)
                    worker_fails = defaultdict(int)
                    for r in pr:
                        for w, stats in r.get("tools_by_worker", {}).items():
                            worker_sums[w] += stats.get("total", 0)
                            worker_fails[w] += stats.get("failed", 0)
                    vals = ""
                    for w in all_workers:
                        t = worker_sums.get(w, 0)
                        f = worker_fails.get(w, 0)
                        cell = f"{t}" if f == 0 else f"{t}({f}f)"
                        vals += f"{cell:>14}"
                    print(f"{model:<20} {prompt:<22} {vals}")
                print()

    # ── Reasoning summary (only if any model emits reasoning) ─────
    has_reasoning = any(r.get("reasoning_total", 0) > 0 for r in rows)
    if has_reasoning:
        # Collect all phase names across all rows
        all_phases = sorted(set(
            phase for r in rows
            for phase in r.get("reasoning_by_phase", {}).keys()
        ))
        phase_hdrs = "".join(f"{p:>10}" for p in all_phases)
        print(f"{'Model':<20} {'Prompt':<22} {'Total':>6} {phase_hdrs}")
        print("-" * (50 + 10 * len(all_phases)))

        for model in models:
            model_total = sum(r.get("reasoning_total", 0)
                              for r in rows if r["model"] == model)
            if model_total == 0:
                print(f"{model:<20} {'(no reasoning)':>22}")
                print()
                continue
            for prompt in prompts:
                pr = [r for r in rows if r["model"] == model and r["prompt"] == prompt]
                if not pr:
                    continue
                total = sum(r.get("reasoning_total", 0) for r in pr)
                # Sum phase counts across iterations
                phase_sums = defaultdict(int)
                for r in pr:
                    for phase, count in r.get("reasoning_by_phase", {}).items():
                        phase_sums[phase] += count
                phase_vals = "".join(f"{phase_sums.get(p, 0):>10}" for p in all_phases)
                print(f"{model:<20} {prompt:<22} {total:>6} {phase_vals}")
            print()


    # ── Replan summary (only if any replans occurred) ──────────────
    has_replans = any(r.get("replan_count", 0) > 0 for r in rows)
    if has_replans:
        print(f"{'Model':<20} {'Prompt':<22} {'Replans':>8} {'Triggers':<30} {'Last Q':>8} {'EvalSkip':>9}")
        print("-" * 102)
        for model in models:
            for prompt in prompts:
                pr = [r for r in rows if r["model"] == model and r["prompt"] == prompt]
                if not pr:
                    continue
                total_replans = sum(r.get("replan_count", 0) for r in pr)
                if total_replans == 0:
                    continue
                # Aggregate triggers across iterations
                triggers = defaultdict(int)
                for r in pr:
                    for t, c in r.get("replan_triggers", {}).items():
                        triggers[t] += c
                trigger_str = ", ".join(f"{c}x {t}" for t, c in sorted(triggers.items()))
                # Last quality score and eval_skipped from iterations
                last_q = ""
                eval_skipped = False
                for r in pr:
                    for it in r.get("iterations", []):
                        if it.get("quality_score") is not None:
                            last_q = f"{it['quality_score']:.2f}"
                        if it.get("evaluation_skipped"):
                            eval_skipped = True
                skip_str = "yes" if eval_skipped else "no"
                print(f"{model:<20} {prompt:<22} {total_replans:>8} {trigger_str:<30} {last_q:>8} {skip_str:>9}")
            print()
    else:
        print("Replans: none detected\n")


def print_loop_report(flags: list[dict]):
    """Print loop detection report."""
    if not flags:
        print("Loop Detection: CLEAN — no looping detected\n")
        return

    print(f"Loop Detection: {len(flags)} WARNING(S)\n")
    for f in flags:
        print(f"  ⚠  {f['model']} / {f['prompt']}")
        print(f"     Avg tools: {f['avg_tools']:.0f}  "
              f"(cross-model median: {f['median_tools']:.0f})  "
              f"Duplicate errors: {f['duplicate_errors']}  "
              f"Failed calls: {f['failed_calls']}")
        for reason in f["reasons"]:
            print(f"     → {reason}")
        print()


def write_csv(rows: list[dict], results_dir: Path):
    """Write a clean CSV with all parsed data."""
    csv_path = results_dir / "results-parsed.csv"
    # Flatten reasoning_by_phase into columns for CSV
    for row in rows:
        for phase, count in row.get("reasoning_by_phase", {}).items():
            row[f"reasoning_{phase}"] = count
    # Collect all reasoning phase columns
    phase_cols = sorted(set(
        k for r in rows for k in r if k.startswith("reasoning_") and k != "reasoning_total"
    ))
    fields = [
        "model", "prompt", "iteration", "elapsed_ms",
        "tool_calls", "orch_tools", "worker_tools",
        "duplicate_tool_calls", "failed_tool_calls",
        "tasks_started", "tasks_completed",
        "reasoning_total", *phase_cols,
        "replan_count",
        "completed", "timeout", "planned", "routing_mode", "routing_rationale",
    ]
    with open(csv_path, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=fields, extrasaction="ignore")
        w.writeheader()
        w.writerows(rows)
    print(f"Wrote: {csv_path}")


def write_loops(flags: list[dict], results_dir: Path):
    """Write detected loops to loops.json for cross-run tracking."""
    from datetime import datetime, timezone

    loops_path = results_dir / "loops.json"
    record = {
        "run_dir": results_dir.name,
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "loop_count": len(flags),
        "loops": [
            {
                "model": f["model"],
                "prompt": f["prompt"],
                "avg_tools": round(f["avg_tools"], 1),
                "cross_model_median": round(f["median_tools"], 1),
                "duplicate_errors": f["duplicate_errors"],
                "failed_calls": f["failed_calls"],
                "signals": f["reasons"],
            }
            for f in flags
        ],
    }
    loops_path.write_text(json.dumps(record, indent=2) + "\n")
    print(f"Wrote: {loops_path}")

    # Append to cumulative loop log (one JSONL entry per run)
    log_path = results_dir.parent / "loop-history.jsonl"
    with open(log_path, "a") as fh:
        fh.write(json.dumps(record) + "\n")
    print(f"Appended: {log_path}")


def main():
    parser = argparse.ArgumentParser(description="Parse E2E comparison results")
    parser.add_argument("results_dir", type=Path, help="Path to results-<timestamp> directory")
    parser.add_argument("--csv", action="store_true", help="Write clean results-parsed.csv")
    args = parser.parse_args()

    if not args.results_dir.is_dir():
        print(f"ERROR: Not a directory: {args.results_dir}", file=sys.stderr)
        sys.exit(1)

    rows = collect_results(args.results_dir)
    rows = merge_timing(rows, args.results_dir)

    if not rows:
        print("No results found.", file=sys.stderr)
        sys.exit(1)

    print(f"Parsed {len(rows)} results from {args.results_dir.name}\n")
    print_summary(rows)

    # Loop detection
    flags = detect_loops(rows)
    print_loop_report(flags)
    write_loops(flags, args.results_dir)

    if args.csv:
        write_csv(rows, args.results_dir)

    # Exit non-zero if loops detected (useful for CI)
    if flags:
        sys.exit(2)


if __name__ == "__main__":
    main()

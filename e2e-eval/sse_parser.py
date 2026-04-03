#!/usr/bin/env python3
"""Shared SSE parsing module for Aura E2E evaluation.

Extracts structured data from SSE capture files produced by
run-model-comparison.sh and run-session-e2e.sh.

Usage as library:
    from sse_parser import parse_sse_file
    result = parse_sse_file(Path("path/to/capture.sse"))

Usage standalone (dump JSON for a single file):
    python3 e2e-eval/sse_parser.py path/to/capture.sse
"""
import json
import re
import sys
from collections import defaultdict
from pathlib import Path


def _parse_event_data_pairs(text: str) -> list[tuple[str, str]]:
    """Pair event lines with their data lines from raw SSE text."""
    lines = text.splitlines()
    pairs = []
    i = 0
    while i < len(lines):
        if lines[i].startswith("event: "):
            event_name = lines[i].split("event: ", 1)[1]
            data = ""
            if i + 1 < len(lines) and lines[i + 1].startswith("data: "):
                data = lines[i + 1][6:]
            pairs.append((event_name, data))
        i += 1
    return pairs


def _extract_answer_text(text: str) -> str:
    """Concatenate delta.content chunks from OpenAI-format SSE lines.

    Only includes content that precedes a finish_reason: "stop" signal,
    matching what the user actually sees.
    """
    chunks = []
    for line in text.splitlines():
        if not line.startswith("data: ") or line.startswith("data: [DONE]"):
            continue
        try:
            d = json.loads(line[6:])
            choices = d.get("choices", [])
            if not choices:
                continue
            choice = choices[0]
            delta = choice.get("delta", {})
            content = delta.get("content", "")
            if content:
                chunks.append(content)
        except (json.JSONDecodeError, TypeError, IndexError, KeyError):
            pass
    return "".join(chunks)


def _extract_tool_names(event_data_pairs: list[tuple[str, str]]) -> list[str]:
    """Collect tool names from orchestrator tool_call_started and
    worker-level aura.tool_start events."""
    names = []
    for event_name, data in event_data_pairs:
        if event_name in ("aura.orchestrator.tool_call_started", "aura.tool_start") and data:
            try:
                payload = json.loads(data)
                name = payload.get("tool_name", "")
                if name:
                    names.append(name)
            except (json.JSONDecodeError, TypeError):
                pass
    return names


def _extract_worker_ids(event_data_pairs: list[tuple[str, str]]) -> list[str]:
    """Collect unique worker IDs from task_started events."""
    workers = []
    seen = set()
    for event_name, data in event_data_pairs:
        if event_name == "aura.orchestrator.task_started" and data:
            try:
                payload = json.loads(data)
                worker = payload.get("worker_id", "")
                if worker and worker not in seen:
                    workers.append(worker)
                    seen.add(worker)
            except (json.JSONDecodeError, TypeError):
                pass
    return workers


def _extract_tasks(event_data_pairs: list[tuple[str, str]]) -> list[dict]:
    """Build per-task records by joining task_started → tool_call_* → task_completed.

    Each task record contains:
    - task_id, description, worker_id
    - success, duration_ms, result (from task_completed)
    - tool_calls: list of {tool_name, tool_call_id, arguments, _aura_reasoning,
                           success, duration_ms, result}
    """
    # Index: task_id → record (in-progress)
    tasks: dict[int, dict] = {}
    # Track which task a tool_call_id belongs to
    call_to_task: dict[str, int] = {}

    for event_name, data in event_data_pairs:
        if not data:
            continue
        try:
            payload = json.loads(data)
        except (json.JSONDecodeError, TypeError):
            continue

        if event_name == "aura.orchestrator.task_started":
            tid = payload.get("task_id")
            if tid is None:
                continue
            tasks[tid] = {
                "task_id": tid,
                "description": payload.get("description", ""),
                "worker_id": payload.get("worker_id", ""),
                "success": None,
                "duration_ms": None,
                "result": None,
                "tool_calls": [],
            }

        elif event_name == "aura.orchestrator.tool_call_started":
            tid = payload.get("task_id")
            cid = payload.get("tool_call_id", "")
            args = payload.get("arguments") or {}
            reasoning = args.pop("_aura_reasoning", None) if isinstance(args, dict) else None
            if cid:
                call_to_task[cid] = tid
            call_rec = {
                "tool_name": payload.get("tool_name", ""),
                "tool_call_id": cid,
                "arguments": args,
                "_aura_reasoning": reasoning,
                "success": None,
                "duration_ms": None,
                "result": None,
            }
            if tid is not None and tid in tasks:
                tasks[tid]["tool_calls"].append(call_rec)

        elif event_name == "aura.orchestrator.tool_call_completed":
            cid = payload.get("tool_call_id", "")
            tid = call_to_task.get(cid)
            if tid is not None and tid in tasks:
                for tc in reversed(tasks[tid]["tool_calls"]):
                    if tc["tool_call_id"] == cid:
                        tc["success"] = payload.get("success", True)
                        tc["duration_ms"] = payload.get("duration_ms")
                        tc["result"] = payload.get("result")
                        break

        elif event_name == "aura.orchestrator.task_completed":
            tid = payload.get("task_id")
            if tid is not None and tid in tasks:
                tasks[tid]["success"] = payload.get("success", False)
                tasks[tid]["duration_ms"] = payload.get("duration_ms")
                tasks[tid]["result"] = payload.get("result")

    # Return in task_id order
    return [tasks[tid] for tid in sorted(tasks.keys())]


def parse_sse_file(path: Path) -> dict:
    """Extract stats from a single SSE capture file.

    Returns a dict with all extracted fields including:
    - tool_calls, orch_tools, worker_tools, duplicate/failed counts
    - completed, timeout, planned, routing_mode, routing_rationale, goal, planning_response
    - tasks_started, tasks_completed
    - reasoning_total, reasoning_by_phase
    - event_counts, tools_by_worker
    - replan_count, replan_triggers, replans, iterations
    - answer_text: concatenated assistant response
    - tool_names: ordered list of tool names invoked
    - worker_ids: ordered list of unique worker IDs assigned
    - tasks: per-task records with tool calls and _aura_reasoning
    """
    text = path.read_text(errors="replace")
    event_data_pairs = _parse_event_data_pairs(text)
    events = [e for e, _ in event_data_pairs]

    # Tool calls: orchestrator-level and worker-level
    orch_tools = sum(1 for e in events if e == "aura.orchestrator.tool_call_completed")
    worker_tools = sum(1 for e in events if e == "aura.tool_complete")
    tool_calls = orch_tools + worker_tools

    # Loop detection + per-worker tool call attribution
    duplicate_tool_calls = 0
    failed_tool_calls = 0
    call_id_to_worker = {}
    tools_by_worker = defaultdict(lambda: {"total": 0, "failed": 0, "dupes": 0})
    for event_name, data in event_data_pairs:
        if event_name == "aura.orchestrator.tool_call_started" and data:
            try:
                payload = json.loads(data)
                cid = payload.get("tool_call_id", "")
                worker = payload.get("tool_initiator_id") or payload.get("worker_id") or ""
                if cid and worker:
                    call_id_to_worker[cid] = worker
            except (json.JSONDecodeError, TypeError):
                pass
        elif event_name in ("aura.orchestrator.tool_call_completed", "aura.tool_complete") and data:
            try:
                payload = json.loads(data)
                cid = payload.get("tool_call_id", "")
                worker = call_id_to_worker.get(cid) or payload.get("tool_initiator_id") or payload.get("worker_id") or "unknown"
                tools_by_worker[worker]["total"] += 1
                if not payload.get("success", True):
                    failed_tool_calls += 1
                    tools_by_worker[worker]["failed"] += 1
                    result = payload.get("result", "")
                    if "DUPLICATE TOOL CALL" in result:
                        duplicate_tool_calls += 1
                        tools_by_worker[worker]["dupes"] += 1
            except (json.JSONDecodeError, TypeError):
                pass

    # Completion
    completed = '"finish_reason":"stop"' in text or '"finish_reason": "stop"' in text

    # Timeout detection
    timeout = bool(re.search(r"timeout|timed out", text, re.IGNORECASE))

    # Event breakdown
    event_counts = defaultdict(int)
    for e in events:
        event_counts[e] += 1

    # Plan created? Extract routing_mode and LLM intent fields from first plan_created event.
    planned = "aura.orchestrator.plan_created" in event_counts
    routing_mode = None
    routing_rationale = None
    goal = None
    planning_response = None
    for event_name, data in event_data_pairs:
        if event_name == "aura.orchestrator.plan_created" and data:
            try:
                payload = json.loads(data)
                routing_mode = payload.get("routing_mode")
                routing_rationale = payload.get("routing_rationale")
                goal = payload.get("goal")
                planning_response = payload.get("planning_response")
            except (json.JSONDecodeError, TypeError):
                pass
            break
        # Also capture rationale from direct_answer / clarification_needed
        if event_name in ("aura.orchestrator.direct_answer",
                          "aura.orchestrator.clarification_needed") and data:
            try:
                payload = json.loads(data)
                routing_rationale = payload.get("routing_rationale")
            except (json.JSONDecodeError, TypeError):
                pass
            break

    # Tasks started/completed
    tasks_started = event_counts.get("aura.orchestrator.task_started", 0)
    tasks_completed = event_counts.get("aura.orchestrator.task_completed", 0)

    # Reasoning chunks per orchestration phase
    reasoning_total = event_counts.get("aura.reasoning", 0)
    reasoning_by_phase = defaultdict(int)
    phase = "routing"
    pending_reasoning = 0
    for event_name, _ in event_data_pairs:
        if event_name == "aura.reasoning":
            pending_reasoning += 1
        else:
            if pending_reasoning > 0:
                reasoning_by_phase[phase] += pending_reasoning
                pending_reasoning = 0
            if event_name == "aura.orchestrator.plan_created":
                phase = "planning" if phase == "routing" else "replan"
            elif event_name == "aura.orchestrator.task_started":
                phase = "workers"
            elif event_name == "aura.orchestrator.synthesizing":
                phase = "synthesis"
            elif event_name == "aura.orchestrator.iteration_complete":
                phase = "evaluation"
    if pending_reasoning > 0:
        reasoning_by_phase[phase] += pending_reasoning

    # Replan detection
    replans = []
    iterations = []
    for event_name, data in event_data_pairs:
        if event_name == "aura.orchestrator.replan_started" and data:
            try:
                payload = json.loads(data)
                replans.append({
                    "iteration": payload.get("iteration"),
                    "trigger": payload.get("trigger", "unknown"),
                })
            except (json.JSONDecodeError, TypeError):
                pass
        elif event_name == "aura.orchestrator.iteration_complete" and data:
            try:
                payload = json.loads(data)
                iterations.append({
                    "iteration": payload.get("iteration"),
                    "quality_score": payload.get("quality_score"),
                    "quality_threshold": payload.get("quality_threshold"),
                    "will_replan": payload.get("will_replan", False),
                    "evaluation_skipped": payload.get("evaluation_skipped", False),
                    "gaps": payload.get("gaps", []),
                })
            except (json.JSONDecodeError, TypeError):
                pass

    replan_count = len(replans)
    replan_triggers = defaultdict(int)
    for r in replans:
        replan_triggers[r["trigger"]] += 1

    # New fields: answer_text, tool_names, worker_ids, tasks
    answer_text = _extract_answer_text(text)
    tool_names = _extract_tool_names(event_data_pairs)
    worker_ids = _extract_worker_ids(event_data_pairs)
    tasks = _extract_tasks(event_data_pairs)

    return {
        "tool_calls": tool_calls,
        "orch_tools": orch_tools,
        "worker_tools": worker_tools,
        "duplicate_tool_calls": duplicate_tool_calls,
        "failed_tool_calls": failed_tool_calls,
        "completed": completed,
        "timeout": timeout,
        "planned": planned,
        "routing_mode": routing_mode,
        "routing_rationale": routing_rationale,
        "goal": goal,
        "planning_response": planning_response,
        "tasks_started": tasks_started,
        "tasks_completed": tasks_completed,
        "reasoning_total": reasoning_total,
        "reasoning_by_phase": dict(reasoning_by_phase),
        "event_counts": dict(event_counts),
        "tools_by_worker": {k: dict(v) for k, v in tools_by_worker.items()},
        "replan_count": replan_count,
        "replan_triggers": dict(replan_triggers),
        "replans": replans,
        "iterations": iterations,
        "answer_text": answer_text,
        "tool_names": tool_names,
        "worker_ids": worker_ids,
        "tasks": tasks,
    }


def effective_routing_mode(record: dict) -> str:
    """Return routing_mode from SSE data, falling back to planned flag for old captures."""
    return record.get("routing_mode") or ("plan" if record.get("planned") else "direct")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(f"Usage: {sys.argv[0]} <sse-file>", file=sys.stderr)
        sys.exit(1)
    path = Path(sys.argv[1])
    if not path.is_file():
        print(f"ERROR: Not a file: {path}", file=sys.stderr)
        sys.exit(1)
    result = parse_sse_file(path)
    print(json.dumps(result, indent=2))

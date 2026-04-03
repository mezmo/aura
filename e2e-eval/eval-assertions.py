#!/usr/bin/env python3
"""Structural assertion script for Aura E2E evaluation.

Runs deterministic assertions against SSE capture files to verify:
- Routing mode matches expected per prompt
- Expected workers were assigned
- Expected tool names were called
- Answer text contains expected substrings

Built-in assertions for math-MCP prompts (independent set). Extensible
via JSON config for custom prompt sets.

Usage:
    # Run against a results directory with built-in math-MCP assertions
    python3 e2e-eval/eval-assertions.py e2e-eval/results-20260402-145323

    # Run with custom assertion config
    python3 e2e-eval/eval-assertions.py results-dir --config assertions.json

    # Only check specific prompts
    python3 e2e-eval/eval-assertions.py results-dir --prompts direct-add,trig-sin45

    # JSON output for programmatic consumption
    python3 e2e-eval/eval-assertions.py results-dir --json

Exit codes:
    0 = all assertions passed
    1 = one or more assertions failed
    2 = no results found or config error
"""
import argparse
import json
import sys
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path

# Allow importing from same directory when run as script
sys.path.insert(0, str(Path(__file__).resolve().parent))
from sse_parser import effective_routing_mode, parse_sse_file


# ── Built-in Assertions for Math-MCP (independent prompt set) ────────

BUILTIN_ASSERTIONS = {
    "direct-add": {
        "routing_mode": "direct",
        "workers": [],
        "tool_names": [],
        "answer_contains": ["4"],
    },
    "mean-then-multiply": {
        "routing_mode": "orchestrated",
        "workers": ["statistics", "arithmetic"],
        "tool_names": ["mean", "multiply"],
        "answer_contains": ["60"],
    },
    "trig-sin45": {
        "routing_mode": "direct",
        "workers": [],
        "tool_names": [],
        "answer_contains": ["0.707"],
    },
    "add-then-mean": {
        "routing_mode": "orchestrated",
        "workers": ["arithmetic", "statistics"],
        "tool_names": ["add", "mean"],
        "answer_contains": ["42"],
    },
    "multi-step-median": {
        "routing_mode": "orchestrated",
        "workers": ["arithmetic", "statistics"],
        "tool_names": ["multiply", "subtract", "median"],
        "answer_contains": ["37.5"],
    },
}

# ── Dependent prompt set assertions ──────────────────────────────────

BUILTIN_ASSERTIONS_DEPENDENT = {
    "t1-mean-baseline": {
        "routing_mode": "orchestrated",
        "workers": ["statistics"],
        "tool_names": ["mean"],
        "answer_contains": ["30"],
    },
    "t2-multiply-prior": {
        "routing_mode": "orchestrated",
        "workers": ["arithmetic"],
        "tool_names": ["multiply"],
        "answer_contains": ["120"],
    },
    "t3-subtract-sin": {
        "routing_mode": "orchestrated",
        "workers": ["arithmetic", "trigonometry"],
        "tool_names": ["subtract"],
        "answer_contains": [],
    },
    "t4-median-three": {
        "routing_mode": "orchestrated",
        "workers": ["statistics"],
        "tool_names": ["median"],
        "answer_contains": ["100"],
    },
    "t5-add-max": {
        "routing_mode": "orchestrated",
        "workers": ["arithmetic"],
        "tool_names": ["add"],
        "answer_contains": ["200"],
    },
}


@dataclass
class AssertionResult:
    """Result of a single assertion check."""

    prompt: str
    check: str
    passed: bool
    detail: str = ""
    model: str = ""
    iteration: int = 0
    sse_path: str = ""

    def to_dict(self) -> dict:
        return {
            "prompt": self.prompt,
            "check": self.check,
            "passed": self.passed,
            "detail": self.detail,
        }


def check_routing_mode(prompt: str, parsed: dict, expected: str) -> AssertionResult:
    """Assert that the routing mode matches expected."""
    actual = effective_routing_mode(parsed)
    passed = actual == expected
    detail = f"expected={expected}, actual={actual}"
    return AssertionResult(prompt, "routing_mode", passed, detail)


def check_workers(prompt: str, parsed: dict, expected: list[str]) -> list[AssertionResult]:
    """Assert that expected workers were assigned (subset check)."""
    results = []
    actual_workers = set(parsed.get("worker_ids", []))

    if not expected:
        # For direct-answer prompts, no workers should be assigned
        if actual_workers:
            results.append(AssertionResult(
                prompt, "workers_empty",
                False,
                f"expected no workers, got {sorted(actual_workers)}",
            ))
        else:
            results.append(AssertionResult(
                prompt, "workers_empty", True, "no workers (correct for direct)",
            ))
        return results

    for worker in expected:
        if worker in actual_workers:
            results.append(AssertionResult(
                prompt, f"worker:{worker}", True, "present",
            ))
        else:
            results.append(AssertionResult(
                prompt, f"worker:{worker}", False,
                f"missing (actual: {sorted(actual_workers)})",
            ))
    return results


def check_tool_names(prompt: str, parsed: dict, expected: list[str]) -> list[AssertionResult]:
    """Assert that expected tool names were called (subset check)."""
    results = []
    actual_tools = set(parsed.get("tool_names", []))

    if not expected:
        # For direct-answer prompts, no tools should be called
        if actual_tools:
            results.append(AssertionResult(
                prompt, "tools_empty",
                False,
                f"expected no tools, got {sorted(actual_tools)}",
            ))
        else:
            results.append(AssertionResult(
                prompt, "tools_empty", True, "no tools (correct for direct)",
            ))
        return results

    for tool in expected:
        if tool in actual_tools:
            results.append(AssertionResult(
                prompt, f"tool:{tool}", True, "called",
            ))
        else:
            results.append(AssertionResult(
                prompt, f"tool:{tool}", False,
                f"not called (actual: {sorted(actual_tools)})",
            ))
    return results


def check_answer_contains(prompt: str, parsed: dict, expected: list[str]) -> list[AssertionResult]:
    """Assert that the answer text contains expected substrings."""
    results = []
    answer = parsed.get("answer_text", "")

    if not expected:
        return results

    for substring in expected:
        if substring in answer:
            results.append(AssertionResult(
                prompt, f"answer_contains:{substring}", True, "found",
            ))
        else:
            # Show a preview of the answer for debugging
            preview = answer[:100] + "..." if len(answer) > 100 else answer
            results.append(AssertionResult(
                prompt, f"answer_contains:{substring}", False,
                f"not found in: {preview!r}",
            ))
    return results


def check_completed(prompt: str, parsed: dict) -> AssertionResult:
    """Assert that the response completed (finish_reason: stop)."""
    passed = parsed.get("completed", False)
    detail = "completed" if passed else "did not complete"
    return AssertionResult(prompt, "completed", passed, detail)


def run_assertions(
    prompt: str,
    parsed: dict,
    assertion_spec: dict,
) -> list[AssertionResult]:
    """Run all assertions for a single prompt against parsed SSE data."""
    results = []

    # Always check completion
    results.append(check_completed(prompt, parsed))

    # Routing mode
    if "routing_mode" in assertion_spec:
        results.append(check_routing_mode(prompt, parsed, assertion_spec["routing_mode"]))

    # Workers
    if "workers" in assertion_spec:
        results.extend(check_workers(prompt, parsed, assertion_spec["workers"]))

    # Tool names
    if "tool_names" in assertion_spec:
        results.extend(check_tool_names(prompt, parsed, assertion_spec["tool_names"]))

    # Answer contains
    if "answer_contains" in assertion_spec:
        results.extend(check_answer_contains(prompt, parsed, assertion_spec["answer_contains"]))

    return results


def load_assertions_config(config_path: Path) -> dict:
    """Load assertion config from a JSON file.

    Expected format:
    {
        "prompt-label": {
            "routing_mode": "orchestrated",
            "workers": ["arithmetic", "statistics"],
            "tool_names": ["add", "mean"],
            "answer_contains": ["42"]
        },
        ...
    }
    """
    try:
        text = config_path.read_text()
        config = json.loads(text)
        if not isinstance(config, dict):
            print(f"ERROR: Config must be a JSON object, got {type(config).__name__}",
                  file=sys.stderr)
            sys.exit(2)
        return config
    except (json.JSONDecodeError, OSError) as e:
        print(f"ERROR: Failed to load config {config_path}: {e}", file=sys.stderr)
        sys.exit(2)


def collect_sse_files(results_dir: Path) -> dict[str, list[tuple[str, int, Path]]]:
    """Collect SSE files grouped by prompt label.

    Returns: {prompt_label: [(model, iteration, path), ...]}
    """
    files_by_prompt = defaultdict(list)

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
                files_by_prompt[label].append((model, iteration, sse_file))

    return dict(files_by_prompt)


def detect_prompt_set(prompt_labels: set[str]) -> str:
    """Detect which prompt set was used based on prompt labels found."""
    independent_labels = set(BUILTIN_ASSERTIONS.keys())
    dependent_labels = set(BUILTIN_ASSERTIONS_DEPENDENT.keys())

    independent_overlap = prompt_labels & independent_labels
    dependent_overlap = prompt_labels & dependent_labels

    if len(dependent_overlap) > len(independent_overlap):
        return "dependent"
    return "independent"


def main():
    parser = argparse.ArgumentParser(
        description="Structural assertions for Aura E2E SSE captures",
    )
    parser.add_argument(
        "results_dir", type=Path,
        help="Path to results-<timestamp> directory",
    )
    parser.add_argument(
        "--config", type=Path, default=None,
        help="JSON assertion config (overrides built-in)",
    )
    parser.add_argument(
        "--prompts", type=str, default=None,
        help="Comma-separated prompt labels to check (default: all)",
    )
    parser.add_argument(
        "--json", action="store_true",
        help="Output results as JSON",
    )
    parser.add_argument(
        "--prompt-set", choices=["independent", "dependent", "auto"],
        default="auto",
        help="Which built-in assertion set to use (default: auto-detect)",
    )
    args = parser.parse_args()

    if not args.results_dir.is_dir():
        print(f"ERROR: Not a directory: {args.results_dir}", file=sys.stderr)
        sys.exit(2)

    # Collect SSE files
    files_by_prompt = collect_sse_files(args.results_dir)
    if not files_by_prompt:
        print("ERROR: No SSE files found", file=sys.stderr)
        sys.exit(2)

    # Load assertion config
    if args.config:
        assertions = load_assertions_config(args.config)
    else:
        # Auto-detect or use specified prompt set
        prompt_set = args.prompt_set
        if prompt_set == "auto":
            prompt_set = detect_prompt_set(set(files_by_prompt.keys()))

        if prompt_set == "dependent":
            assertions = BUILTIN_ASSERTIONS_DEPENDENT
        else:
            assertions = BUILTIN_ASSERTIONS

    # Filter prompts if requested
    if args.prompts:
        prompt_filter = set(args.prompts.split(","))
    else:
        prompt_filter = None

    # Run assertions
    all_results = []
    for prompt_label, sse_entries in sorted(files_by_prompt.items()):
        if prompt_label not in assertions:
            continue
        if prompt_filter and prompt_label not in prompt_filter:
            continue

        spec = assertions[prompt_label]

        for model, iteration, sse_path in sse_entries:
            parsed = parse_sse_file(sse_path)
            results = run_assertions(prompt_label, parsed, spec)
            for r in results:
                r.model = model
                r.iteration = iteration
                r.sse_path = str(sse_path)
                all_results.append(r)

    if not all_results:
        print("ERROR: No matching assertions to run", file=sys.stderr)
        sys.exit(2)

    # Output
    pass_count = sum(1 for r in all_results if r.passed)
    fail_count = sum(1 for r in all_results if not r.passed)
    total = len(all_results)

    if args.json:
        output = {
            "results_dir": str(args.results_dir),
            "total": total,
            "passed": pass_count,
            "failed": fail_count,
            "assertions": [
                {
                    **r.to_dict(),
                    "model": r.model,
                    "iteration": r.iteration,
                }
                for r in all_results
            ],
        }
        print(json.dumps(output, indent=2))
    else:
        # Group results by model for readable output
        by_model = defaultdict(list)
        for r in all_results:
            by_model[r.model].append(r)

        for model in sorted(by_model.keys()):
            print(f"--- {model} ---")
            model_results = by_model[model]

            # Group by prompt within model
            by_prompt = defaultdict(list)
            for r in model_results:
                by_prompt[r.prompt].append(r)

            for prompt in sorted(by_prompt.keys()):
                prompt_results = by_prompt[prompt]
                prompt_pass = all(r.passed for r in prompt_results)
                status = "PASS" if prompt_pass else "FAIL"
                print(f"  {prompt}: {status}")
                for r in prompt_results:
                    marker = "  PASS" if r.passed else "  FAIL"
                    print(f"    {marker}  {r.check}: {r.detail}")
            print()

        # Summary
        print("=" * 60)
        if fail_count == 0:
            print(f"ALL PASSED: {pass_count}/{total} assertions")
        else:
            print(f"FAILED: {fail_count}/{total} assertions failed")

            # List failures grouped for quick scan
            print()
            print("Failures:")
            for r in all_results:
                if not r.passed:
                    print(f"  [{r.model}] {r.prompt} / {r.check}: {r.detail}")

    sys.exit(0 if fail_count == 0 else 1)


if __name__ == "__main__":
    main()

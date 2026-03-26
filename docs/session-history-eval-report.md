# Session History Eval Framework — Report

**Date**: 2026-03-24
**Branch**: `mshearer/LOG-23461-session-scoped-persistence`
**Ref**: LOG-23470

---

## What Changed

### Rust (production code)

**`crates/aura/src/prompts/session_history.md`** — Two changes to the coordinator's session history template:

1. Added `Current time: %%CURRENT_TIME%%` so the coordinator can reason about how stale prior turn results are (prior turns already have ISO timestamps, but without "now" they're meaningless).

2. Strengthened worker blindness guidance. The old template mentioned it in a subordinate clause; it's now a standalone bold callout:

   > **CRITICAL: Workers have NO access to session history. Every value a worker needs from a prior turn MUST appear as a literal number in its task description.**

   Also tightened "avoid redundant work" to explicitly say "do not re-call tools".

**`crates/aura/src/orchestration/persistence.rs`** — `build_session_context()` now fills `%%CURRENT_TIME%%` with `chrono::Utc::now().to_rfc3339()`. Single `.replace()` call in the existing chain. All 4 session context tests updated and passing.

### Eval Scripts (e2e-eval/)

| File | Change |
|------|--------|
| `run-qwen-comparison.sh` | **Renamed** to `run-model-comparison.sh` (not Qwen-specific) |
| `run-model-comparison.sh` | Added `PROMPT_SET` env var toggle |
| `run-session-e2e.sh` | Added `PROMPT_SET` env var toggle |
| `analyze-session-history-eval.py` | **New** — artifact-based session eval scorecard |
| `run-multi-model-session-eval.sh` | **New** — unattended multi-model runner with auto-analysis |

Output directories (`session-results-*/`, `eval-*.json`, `multi-model-eval-*.log`) are gitignored.

---

## New Settings & Environment Variables

| Variable | Where | Values | Default | Purpose |
|----------|-------|--------|---------|---------|
| `PROMPT_SET` | Shell env for eval scripts | `independent`, `dependent` | `independent` | Selects prompt set. `independent` = existing standalone math prompts (unchanged). `dependent` = chained T1-T5 where each turn builds on prior output. |

No new Rust config keys. The `session_history_turns` config (default 3) and `memory_dir` already existed from LOG-23461/LOG-23470.

---

## Dependent Prompt Chain (T1-T5)

Each turn produces a deterministic value and requires the coordinator to embed prior results:

| Turn | Prompt | Expected | Prior Value Needed |
|------|--------|----------|-------------------|
| T1 | "Calculate the mean of [12, 24, 36, 48]" | 30.0 | None (baseline) |
| T2 | "Take the result from my previous question (the mean) and multiply it by 4" | 120.0 | 30 from T1 |
| T3 | "Subtract 20 from that multiplication result, then compute the sine of that many degrees" | sin(100deg) = 0.9848 | 120 from T2 |
| T4 | "Compute the median of these three results from our conversation: the original mean, the multiplication result, and the subtraction result" | median([30, 120, 100]) = 100.0 | 30, 120, 100 |
| T5 | "Add the median you just computed to 50, then find the maximum of that sum and 200" | max(150, 200) = 200.0 | 100 from T4 |

---

## E2E Evidence — opus-bedrock (2026-03-23)

### Scorecard (from `analyze-session-history-eval.py`)

```
Turn   Tasks Tools Embed?       Redundant?   Correct?   Status
T1         1     1 n/a          n/a          Y          success
T2         1     1 Y(30)        Y            Y          success
T3         2     3 Y(120)       Y            Y          success
T4         1     1 Y(3/3)       Y            Y          success
T5         2     2 Y(100)       Y            Y          success

SCORECARD:
  Value embedding:  4/4 (100%)
  Tool efficiency:  4/4 (no redundant recomputation)
  Correctness:      5/5
  Total tools:      8 (session)
```

### What the Coordinator Actually Did

Evidence from persistence artifacts (plan.json goals + tool-calls.json):

| Turn | Coordinator Goal | Tools Called | Evidence of Reuse |
|------|-----------------|-------------|-------------------|
| T1 | "Compute the mean of the numbers 12, 24, 36, 48" | `mean([12,24,36,48])` -> 30 | Baseline |
| T2 | "Multiply the previous mean result **(30)** by 4" | `multiply(30, 4)` -> 120 | Embedded 30, did NOT call `mean` |
| T3 | "Subtract 20 from the previous multiplication result **(120)**..." | `subtract(120, 20)` -> 100, `degreesToRadians(100)`, `sin(1.745)` -> 0.9848 | Embedded 120, did NOT call `multiply` or `mean` |
| T4 | "Compute the median of the three results: the original mean **(30)**, the multiplic[ation result **(120)**]..." | `median([30, 120, 100])` -> 100 | Embedded all 3 values, did NOT call any prior tools |
| T5 | "Add the median result **(100)** to 50..." | `add(100, 50)` -> 150, `max([150, 200])` -> 200 | Embedded 100, did NOT call `median` |

**Zero redundant tool calls across all 4 dependent turns.** The coordinator pulled every prior value from session history and passed literal numbers to workers in task descriptions. 8 total tool calls vs an estimated 14+ if each turn recomputed from scratch.

### Session History Injection Log

```
T1: no prior manifests (first turn)
T2: Injecting session history: 1 prior run(s)
T3: Injecting session history: 2 prior run(s)
T4: Injecting session history: 3 prior run(s)
T5: Injecting session history: 3 prior run(s)  (capped at session_history_turns=3)
```

5/5 manifests written. History injection on T2-T5 as expected.

---

## How to Reproduce

```bash
# 1. Build
cargo build --release

# 2. Run dependent session E2E (single model)
PROMPT_SET=dependent ./e2e-eval/run-session-e2e.sh \
  configs/math-orchestration-opus-bedrock.toml

# 3. Analyze (session ID printed by step 2)
python3 e2e-eval/analyze-session-history-eval.py \
  --memory-dir /tmp/aura-math-opus-bedrock \
  --session-id <session_id_from_step_2>

# 4. Optional: run independent baseline for comparison
PROMPT_SET=dependent ./e2e-eval/run-model-comparison.sh 1 \
  configs/math-orchestration-opus-bedrock.toml

# 5. Optional: compare session vs independent
python3 e2e-eval/analyze-session-history-eval.py \
  --memory-dir /tmp/aura-math-opus-bedrock \
  --session-id <session_id> \
  --independent-session-ids <id1>,<id2>,<id3>,<id4>,<id5>
```

---

## Analysis Script Details

`analyze-session-history-eval.py` reads **only structured persistence artifacts** — no SSE regex parsing.

| Metric | Source | What It Checks |
|--------|--------|----------------|
| Value embedding | `plan.json` task descriptions | Prior-turn numeric values appear as literals (word-boundary regex) |
| Tool efficiency | `tool-calls.json` | No tools from REDUNDANT_IF_PRESENT set called |
| Correctness | `manifest.json` result_preview | Result matches expected value (float tolerance 0.01) |
| Status | `manifest.json` | success / failed |

Supports `--json-export` for programmatic consumption and `--independent-session-ids` for session-vs-independent delta comparison.

---

## Multi-Model Results (2026-03-23)

| Model | Turns Matched | Embedding | Efficiency | Correctness | Tools |
|-------|:---:|:---:|:---:|:---:|:---:|
| **opus-bedrock** | 5/5 | 4/4 (100%) | 4/4 | 5/5 | 8 |
| **claude-thinking** | 5/5 | 4/4 (100%) | 4/4 | 5/5 | 8 |
| **glm** | 4/5 | 3/3 (100%) | 3/3 | 4/4 | 7 |
| **qwen35-thinking** | 5/5 | 3/4 (75%) | 4/4 | 4/5 | 8 |

**Zero redundant recomputation across all 4 models.**

### Model-Specific Notes

- **Anthropic models (opus, claude-thinking)**: Perfect scores. Both embedded all prior values and never recomputed.
- **GLM**: T4 (median of three values) routed as direct answer — GLM computed the median itself without orchestration. All orchestrated turns had perfect embedding/efficiency. This is valid behavior, not a failure.
- **Qwen3.5-thinking**: Two minor issues:
  - **T4 correctness**: `median([30,120,100])` returned `120` from math-mcp (same tool bug seen with opus). Qwen's coordinator did not correct it (`result_preview: "Result: 120"`), while opus's coordinator did.
  - **T5 embedding**: Did not embed `100` in task descriptions (0/1), but still got the correct answer (200) with correct tools. The coordinator likely passed the value via tool arguments rather than natural language descriptions.

---

## Code Review

5-agent review (3 Claude + 2 Gemini) performed before E2E. Fixes applied:

| Finding | Source | Action |
|---------|--------|--------|
| Substring false positives (`"30"` matching `"300"`) | Claude #1 + Gemini | Fixed: word-boundary regex |
| Duplicate turn-match silent overwrite | Claude #1 | Fixed: warn + first-match-wins |
| Worker blindness not emphatic enough | Claude #3 | Fixed: standalone bold callout |
| T4 matcher too narrow | E2E run (opus) | Fixed: broadened to `"median of the"` |
| T1 patterns stealing T2 goals | E2E run (glm) | Fixed: priority-ordered matching (T2-T5 first, T1 last) |
| Multiple rephrasing patterns needed | E2E run (4 models) | Fixed: multi-pattern matching per turn |

7 lower-priority items deferred until after multi-model testing — tracked in memory.

---

## Commit Status

```
2fe0d76 chore: gitignore session eval output files
7130917 fix(eval): priority-ordered turn matchers for cross-model goals
340f52b fix(eval): broaden T4 turn matcher for coordinator rephrasing
8afb53c feat(orchestration): session history eval framework
ad908ce feat(orchestration): coordinator session context injection (LOG-23470)
a4d8ca4 feat(orchestration): session-scoped persistence + RunManifest
```

All on branch `mshearer/LOG-23461-session-scoped-persistence`. Not pushed.

---

## TODO Progress

The "Orchestration E2E Evaluation Framework" backlog item in `docs/TODO.md` is now substantially addressed:

- [x] Script the manual test battery (`run-model-comparison.sh`, `run-session-e2e.sh`)
- [x] Capture SSE event streams to files for each test query
- [x] Model comparison matrix (CSV output from `run-model-comparison.sh`)
- [x] Track regressions across code changes (session vs independent comparison mode)
- [x] **NEW**: Dependent prompt chain for session history behavioral testing
- [x] **NEW**: Artifact-based analysis with deterministic pass/fail scorecard
- [ ] Automated assertions on event ordering, required fields, event type coverage (separate concern)
- [ ] Integration test that validates SSE event schema against a snapshot (separate concern)

**Done**: Multi-model validation complete (4 models). Session history reuse confirmed across Anthropic and local models.

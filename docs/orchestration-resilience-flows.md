# Orchestration Resilience Flows — Phase 3C E2E Evidence

Observed during 5-model SRE hard E2E suite (2026-05-05) with persistent
coordinator conversation and read_artifact enabled.

## Sonnet Bedrock: Depth Exhaustion → Intelligent Replan → read_artifact → Answer

The "comprehensive health check" prompt requires 6+ categories of data across
two worker domains. Sonnet's prometheus-analyst worker exhausted turn_depth=12
on the first attempt. The coordinator replanned with finer-grained tasks, hit
depth again on one sub-task, but used `read_artifact` to inspect completed
results and synthesized a final answer from what was available.

```
                        SONNET BEDROCK — multi-category-findings (237s total)
                        ═══════════════════════════════════════════════════

 ┌─────────────────────────────────────────────────────────────────────┐
 │ ITERATION 1                                      conversation_len=0│
 │                                                                     │
 │  Coordinator: create_plan (20.8s)                                   │
 │  ├─ "multi-domain query requires k8s-discovery + prometheus-analyst"│
 │  └─ Plan: 2 tasks (parallel)                                        │
 │      Task 0: k8s-discovery — list production workloads              │
 │      Task 1: prometheus-analyst — comprehensive Prometheus check    │
 │                                                                     │
 │  Execute (parallel):                                                │
 │  ├─ Task 0: ✅ completed (67s) — workload list retrieved            │
 │  └─ Task 1: ❌ depth_exhausted (78s) — MaxDepthError at limit 12   │
 │              Worker made 12 tool calls but couldn't finish           │
 │              all 6 categories in one pass                            │
 │                                                                     │
 │  Post-execute continuation:                          conversation_len=2│
 │  Coordinator sees: 1 success + 1 depth_exhausted failure            │
 │  Decision: create_plan (21.1s)                                      │
 │  ├─ Rationale: "The previous combined Prometheus task exhausted     │
 │  │   the tool-call depth limit. Split into smaller tasks."          │
 │  └─ New plan: 3 tasks (finer-grained split of failed work)          │
 │      Task 0: alerts and firing rules                                │
 │      Task 1: targets and metric metadata                            │
 │      Task 2: queue depths and replication lag                       │
 └─────────────────────────────────────────────────────────────────────┘
                              │
                              ▼
 ┌─────────────────────────────────────────────────────────────────────┐
 │ ITERATION 2                                                         │
 │                                                                     │
 │  Execute (3 tasks parallel):                                        │
 │  ├─ Task 0: ✅ completed (32s) — alerts + rules retrieved           │
 │  ├─ Task 1: ✅ completed (55s) — targets + metadata retrieved       │
 │  └─ Task 2: ❌ depth_exhausted (52s) — queue/replication still      │
 │              too many metrics for 12 turns                           │
 │                                                                     │
 │  Post-execute continuation:                          conversation_len=4│
 │  Coordinator sees: 2 success + 1 failure (2nd depth exhaust)        │
 │  ┌──────────────────────────────────────────────┐                   │
 │  │ read_artifact: task-0-prometheus-analyst-     │                   │
 │  │   iter-2-result.txt                           │  ◄── Phase 4!    │
 │  │ Coordinator inspects the full artifact to     │  Coordinator     │
 │  │ get concrete data before synthesizing.        │  reads artifact  │
 │  └──────────────────────────────────────────────┘  during ReAct    │
 │  Decision: respond_directly (62.9s)                                 │
 │  ├─ Rationale: "All data is now collected from completed tasks.     │
 │  │   The full artifact for alerts/rules has been read."             │
 │  └─ Synthesized answer from iter-1 k8s data + iter-2 alert/target  │
 │     data, noting queue depth data was incomplete                     │
 └─────────────────────────────────────────────────────────────────────┘
```

Key behaviors demonstrated:
- Coordinator diagnosed depth exhaustion and split the failed task
- Conversation persisted across iterations (len 0 → 2 → 4)
- `read_artifact` called during post-execute ReAct loop (Phase 4 unlocked)
- Coordinator synthesized from available data rather than failing entirely


## GPT 5.2: Parallel DAG + read_artifact for Deep Inspection

GPT 5.2 made 13 `read_artifact` calls across 5 prompts — the most of any model.
Workers and coordinator both inspect artifacts to get full tool output when the
summary is insufficient.

```
                         GPT 5.2 — security-audit (78s total)
                         ════════════════════════════════════

 ┌─────────────────────────────────────────────────────────────────────┐
 │  Coordinator: create_plan (6.3s)                  conversation_len=0│
 │  └─ Plan: 3 sequential tasks (DAG dependencies)                     │
 │      Task 0: list production namespaces                             │
 │      Task 1: list workloads in each namespace (depends on 0)        │
 │      Task 2: get detailed specs for each workload (depends on 1)    │
 │                                                                     │
 │  Execute (sequential waves):                                        │
 │  ├─ Wave 1: Task 0 ✅ (10s) — namespaces listed                    │
 │  │   ┌──────────────────────────────────────────┐                   │
 │  │   │ Worker calls read_artifact to inspect     │                   │
 │  │   │ tool output from k8s_list_workloads      │  ◄── Worker-level│
 │  │   │ (63KB response spilled to artifact)       │  artifact read   │
 │  │   └──────────────────────────────────────────┘                   │
 │  ├─ Wave 2: Task 1 ✅ (10s) — workloads enumerated                 │
 │  └─ Wave 3: Task 2 ✅ (18s) — detailed specs retrieved             │
 │                                                                     │
 │  Post-execute continuation:                       conversation_len=2│
 │  ┌──────────────────────────────────────────────┐                   │
 │  │ Coordinator read_artifact:                    │                   │
 │  │   task-2-k8s-discovery-iter-1-result.txt     │  ◄── Coordinator │
 │  │ Full security posture data from Task 2 was   │  reads artifact  │
 │  │ truncated in continuation prompt — coordinator│  for synthesis   │
 │  │ reads the full version before answering.      │                   │
 │  └──────────────────────────────────────────────┘                   │
 │  Decision: respond_directly (25.6s)                                 │
 │  ├─ "All task results are complete and the artifact has been        │
 │  │   fully read. Sufficient data for answer."                       │
 │  └─ Inlined: runAsNonRoot, readOnlyRootFilesystem, STRIPE_API_KEY, │
 │     DB_PASSWORD per workload                                         │
 └─────────────────────────────────────────────────────────────────────┘
```


## GPT 5.2: Worker Timeout → Coordinator Adapts

The restart-investigation prompt had Task 2 (cross-referencing pods with alert
rules) time out at 180s. The coordinator synthesized from the available data.

```
                    GPT 5.2 — restart-investigation (212s total)
                    ═══════════════════════════════════════════

 ┌─────────────────────────────────────────────────────────────────────┐
 │  Coordinator: create_plan (6.3s)                  conversation_len=0│
 │  └─ Plan: 3 tasks                                                   │
 │      Task 0: prometheus_query for restart counts > 5                │
 │      Task 1: alertmanager_get_rules for restart-related rules       │
 │      Task 2: cross-reference pods with rules (depends on 0, 1)     │
 │                                                                     │
 │  Execute:                                                           │
 │  ├─ Wave 1 (parallel):                                              │
 │  │   ├─ Task 0: ✅ (6s) — found notification-service pod           │
 │  │   │   ┌─────────────────────────────────────┐                    │
 │  │   │   │ read_artifact: prometheus-query      │                    │
 │  │   │   │   output (restart counts)            │                    │
 │  │   │   └─────────────────────────────────────┘                    │
 │  │   └─ Task 1: ✅ (12s) — found PodRestartLoop rule               │
 │  │       ┌─────────────────────────────────────┐                    │
 │  │       │ read_artifact: alertmanager-get-     │                    │
 │  │       │   rules output (rule definitions)    │                    │
 │  │       └─────────────────────────────────────┘                    │
 │  └─ Wave 2:                                                         │
 │      └─ Task 2: ❌ timeout (180s) — GPT API slow on this call      │
 │                                                                     │
 │  Post-execute continuation:                       conversation_len=2│
 │  Coordinator sees: 2 completed + 1 timeout                          │
 │  Decision: respond_directly                                         │
 │  ├─ Has restart data (Task 0) + rule definitions (Task 1)           │
 │  └─ Synthesized answer: notification-service with PodRestartLoop    │
 │     rule, noted cross-reference task timed out                       │
 └─────────────────────────────────────────────────────────────────────┘
```


## read_artifact Usage Summary (this run)

| Model | read_artifact calls | Context |
|-------|-------------------|---------|
| Opus Bedrock | 2 | Coordinator synthesis |
| Sonnet Thinking Bedrock | 5 | Mixed coordinator + worker |
| GPT 5.2 | 13 | Heavy worker + coordinator use |
| Gemini 3.1 | 0 | (provider timeout — never ran) |
| Sonnet Bedrock | 4 | Coordinator synthesis after replan |

`read_artifact` was previously unreachable (single-shot coordinator, no ReAct
loop). Phase 3C's persistent conversation enables it structurally. Models are
using it organically without any prompt changes directing them to do so.

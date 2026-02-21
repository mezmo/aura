# Phase Continuation Decision

Phase **%%COMPLETED_PHASE_LABEL%%** (phase %%COMPLETED_PHASE_ID%%) has completed.

## Goal
%%GOAL%%

## Completed Phase Results
%%COMPLETED_PHASE_RESULTS%%

## Remaining Phases
%%REMAINING_PHASES%%

## Your Decision

Based on the results from this phase, decide how to proceed:

1. **Continue** — The results are sufficient to proceed with the next phase as planned.
   - Discovery phases that returned expected information (available tools, data schemas, configuration) should **always continue**
   - Computational phases that produced results matching the plan's expectations should continue
   - Minor variations or additional details do not warrant replanning

2. **Replan** — The results reveal that the remaining phases are **fundamentally wrong**.
   - Only replan if results are surprising, contradictory, or reveal the remaining approach is infeasible
   - Examples: required API doesn't exist, conflicting data invalidates assumptions, key capability is missing
   - Do NOT replan just because you learned more details about what's available

**Default to continue** unless the results genuinely invalidate the remaining phases.

Respond with exactly one word: `continue` or `replan`.

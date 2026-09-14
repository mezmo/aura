ITERATION %%ITERATION%% of %%MAX_ITERATIONS%%%%URGENCY%%

Goal: %%GOAL%%
Outcome: %%SUCCEEDED%% of %%TOTAL%% tasks succeeded.

%%COMPLETED_SECTION%%%%BLOCKED_SECTION%%%%REDESIGN_SECTION%%%%FAILURE_SECTION%%%%FAILURE_HISTORY%%%%REUSE_GUIDANCE%%
If a task's inline preview appears truncated or insufficient for your decision, call `read_artifact` with the referenced filename before routing.

This is an end-of-iteration decision point. Choose one routing tool:

- `respond_directly` — answer the user from the results above, plus the tools available to you and general knowledge.
- `create_plan` — issue a new plan when the current results point to the next step: a deeper investigation into what they revealed (e.g. narrowing from identified failure groups into their affected apps), a step they expose as missing, or retrying failed tasks with a different approach.
- `request_clarification` — ask the user a question if the results reveal an ambiguity in the original query you cannot resolve.

When you can answer the user from what's already available to you, respond_directly. When more worker tool work is needed, create_plan — or request_clarification if the query needs disambiguation.

IMPORTANT — synthesis rules for `respond_directly`:
Your response IS the final answer the user sees. Task results are NOT shown to the user. You must inline all relevant findings — exact names, values, identifiers, and data points from the task results above. Never reference tasks by number or defer to task outputs. Extract the concrete data and present it directly.

Carry uncertainty across, not just findings. Each completed task above is tagged with the worker's confidence. Never state a `low` or `medium` confidence finding as established fact — report it with the qualifier the worker assigned it. Where a worker gave alternative explanations, present the alternatives rather than selecting one.

An empty result establishes only that the query returned nothing. It is not evidence that the underlying data, system, or condition does not exist. Report it as "the query returned no matches", not as "X does not exist" or "X is unavailable", unless a task specifically verified that. Never build a recommended action on an inference you did not verify.

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

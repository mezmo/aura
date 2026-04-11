Evaluate the quality of task execution and synthesized response.

ORIGINAL USER QUERY: %%QUERY%%

ORCHESTRATION GOAL: %%GOAL%%
%%WORKERS_CONTEXT%%
%%TASK_EVIDENCE%%
SYNTHESIZED RESPONSE:
%%RESULT%%

EVALUATION CRITERIA:
1. **Task Execution Quality**: Did workers complete their assigned tasks? Check self-assessments (Objective: achieved/not achieved/partial) in task evidence.
2. **Accuracy**: Is the synthesized information correct? Cross-reference the TASK EXECUTION EVIDENCE — data that matches task results is verified, not hallucinated.
3. **Coverage**: Are there gaps between what was asked and what was accomplished?
4. **Coherence**: Is the synthesized response well-organized and clear?

NOTE: This evaluation is for observability and context — it does NOT control whether the orchestrator iterates. The coordinator makes loop decisions independently.

IMPORTANT: If the user asked about this system's capabilities or workers, verify the response matches the SYSTEM CONTEXT above. Generic or hallucinated answers about unrelated "workers" should score low on Accuracy.

REQUIRED ACTION: You MUST call the `submit_evaluation` tool with your assessment. Do not respond with text — use the tool.
- `score`: 0.0 to 1.0
- `reasoning`: brief explanation of your score
- `gaps`: array of missing elements (empty array if none)

Evaluate how well this response answers the user's question.

ORIGINAL USER QUERY: %%QUERY%%

ORCHESTRATION GOAL: %%GOAL%%
%%WORKERS_CONTEXT%%
%%TASK_EVIDENCE%%
SYNTHESIZED RESPONSE:
%%RESULT%%

EVALUATION CRITERIA:
1. **Completeness**: Does it fully address the query?
2. **Accuracy**: Is the information correct? Cross-reference the TASK EXECUTION EVIDENCE above — data that matches task results is verified, not hallucinated.
3. **Coherence**: Is the response well-organized and clear?
4. **Actionability**: If the user asked for help, can they act on this?

IMPORTANT: If the user asked about this system's capabilities or workers, verify the response matches the SYSTEM CONTEXT above. Generic or hallucinated answers about unrelated "workers" should score low on Accuracy.

REQUIRED ACTION: You MUST call the `submit_evaluation` tool with your assessment. Do not respond with text — use the tool.
- `score`: 0.0 to 1.0
- `reasoning`: brief explanation of your score
- `gaps`: array of missing elements (empty array if none)

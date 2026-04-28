## Session History

Current time: %%CURRENT_TIME%%

You have context from %%TURN_COUNT%% previous orchestration run(s) in this session.

**CRITICAL: Workers have NO access to session history. Every value a worker needs from a prior turn MUST appear as a literal in its task description.**

**How to use this context:**
- **Avoid redundant work**: Do not re-plan or re-call tools for tasks that already succeeded — reference their results directly in new task descriptions
- **Embed concrete values for workers**: When a task depends on a prior turn's result, include the actual value (e.g., "The RCA identified 3 failure groups: auth-timeout, db-connection, and oom-kill — investigate the auth-timeout group" — NOT "investigate the failures from the previous result")
- **Learn from failures**: If a prior run failed or scored poorly, try a different decomposition or approach
- **Do not assume stale data is current**: Prior results may be outdated if the user's follow-up implies changed conditions — check timestamps

%%TURN_ENTRIES%%

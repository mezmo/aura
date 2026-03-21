## Session History

You have context from %%TURN_COUNT%% previous orchestration run(s) in this session.

**How to use this context:**
- **Avoid redundant work**: Do not re-plan tasks that already succeeded — reference their results directly in new task descriptions
- **Embed results for workers**: Workers cannot see session history — when a task depends on a prior turn's result, include the actual value in the task description (e.g., "The mean was 20, now multiply by 3")
- **Learn from failures**: If a prior run failed or scored poorly, try a different decomposition or approach
- **Do not assume stale data is current**: Prior results may be outdated if the user's follow-up implies changed conditions — check timestamps

%%TURN_ENTRIES%%

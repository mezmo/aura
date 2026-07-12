#!/bin/bash
# S2 slot-coverage proof: every envelope-surface %%SLOT%% / {{slot}} is
# exercised by at least one fixture-rendered snapshot, and no snapshot
# carries an unreplaced placeholder token.
set -u
# Run from the worktree root: bash crates/aura/src/orchestration/context_fixture/slot_coverage.sh
SNAP=crates/aura/src/orchestration/context_fixture/snapshots
TPL=crates/aura/src/prompts
fail=0

echo "== template placeholder census (envelope-surface templates) =="
grep -ohE '%%[A-Z_]+%%|\{\{[a-z_]+\}\}' \
  $TPL/orchestrator_preamble.md $TPL/worker_preamble.md \
  $TPL/worker_task_prompt.md $TPL/continuation_prompt.md \
  $TPL/session_history.md | sort -u

echo
echo "== 1) no unreplaced placeholder token in any snapshot =="
if grep -rnE '%%[A-Z_]+%%|\{\{[a-z_]+\}\}' $SNAP/; then
  echo "FAIL: raw placeholder leaked into a snapshot"; fail=1
else
  echo "OK: no raw %%SLOT%% or {{slot}} token in any snapshot"
fi

echo
echo "== 2) per-slot witness present in at least one snapshot =="
check() { # slot, witness (fixed string)
  if grep -rlqF -- "$2" $SNAP/; then
    printf 'OK   %-36s witness %q in: %s\n' "$1" "$2" \
      "$(grep -rlF -- "$2" $SNAP/ | head -1 | xargs basename | sed 's/aura__orchestration__context_fixture__normalize__//')"
  else
    printf 'FAIL %-36s witness %q not found\n' "$1" "$2"; fail=1
  fi
}
check '{{orchestration_system_prompt}}' 'PHASE BOUNDARY PRINCIPLE'
check '{{tools_section}}'      'Call exactly one routing tool per query.'
check '{{recon_guidance}}/recon'    '## Reconnaissance Guidance'
check '{{recon_guidance}}/nonrecon' '**Worker names vs tool names**'
check '{{worker_system_prompt}}/default' '(No custom instructions provided)'
check '{{worker_system_prompt}}/custom'  'Prefer structured summaries over prose'
check '%%YOUR_TASK%%'          'YOUR TASK: '
check '%%CONTEXT%%/populated'  'READ-ONLY PRIOR WORK'
check '%%ITERATION%%/%%MAX_ITERATIONS%%' 'ITERATION 2 of 4'
check '%%URGENCY%%'            '(FINAL ATTEMPT)'
check '%%SUCCEEDED%%/%%TOTAL%%' 'Outcome: 1 of 4 tasks succeeded.'
check '%%GOAL%%'               'Goal (verbatim from the original request): Investigate the elevated error rates'
check '%%COMPLETED_SECTION%%'  'COMPLETED TASKS:'
check '%%BLOCKED_SECTION%%'    'BLOCKED TASKS (dependencies failed):'
check '%%REDESIGN_SECTION%%'   'FAILED TASKS:'
check '%%FAILURE_SECTION%%'    'FAILURE SUMMARY:'
check '%%FAILURE_HISTORY%%'    'FAILURE HISTORY:'
check '%%REUSE_GUIDANCE%%'     'Workers cannot see prior iteration results'
check '%%TURN_ENTRIES%%'       '### Turn 1 ('
check '%%TURN_COUNT%%'         '2 prior run(s) shown above'

echo
echo "== 3) empty-branch witnesses (slot exercised empty) =="
emptycheck() { # label, file, forbidden string
  f=$SNAP/aura__orchestration__context_fixture__normalize__$2.snap
  if grep -qF -- "$3" "$f"; then
    printf 'FAIL %-42s %q renders in %s\n' "$1" "$3" "$2"; fail=1
  else
    printf 'OK   %-42s %q absent from %s\n' "$1" "$3" "$2"
  fi
}
emptycheck '%%URGENCY%% empty'          coordinator_call2_clean '(FINAL ATTEMPT)'
emptycheck '%%COMPLETED_SECTION%% empty' coordinator_call2_all_failed 'COMPLETED TASKS:'
emptycheck '%%BLOCKED_SECTION%% empty'  coordinator_call2_clean 'BLOCKED TASKS'
emptycheck '%%REDESIGN_SECTION%% empty' coordinator_call2_clean 'FAILED TASKS:'
emptycheck '%%FAILURE_SECTION%% empty'  coordinator_call2_clean 'FAILURE SUMMARY:'
emptycheck '%%FAILURE_HISTORY%% empty'  coordinator_call2_clean 'FAILURE HISTORY:'
emptycheck '%%REUSE_GUIDANCE%% empty'   coordinator_call2_all_failed 'Workers cannot see prior iteration results'
emptycheck '%%CONTEXT%% empty'          worker_first_turn_empty 'These are completed worker outputs relevant to YOUR TASK'

echo
[ $fail -eq 0 ] && echo "SLOT COVERAGE: PASS" || echo "SLOT COVERAGE: FAIL"
exit $fail

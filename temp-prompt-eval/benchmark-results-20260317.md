# Aura Orchestration Benchmark Results — 2026-03-17

## Test Setup

- **Branch**: `mshearer/temp-orch-merge-23401-23402`
- **Rig Fork**: rev `16f80813` (reasoning round-trip fix)
- **Math MCP**: Docker Compose, 21 real math tools
- **llama-server**: notanton via SSH tunnel port 11435, parallel slot profiles
- **Iterations**: 3 per model, 5 prompts each (180 total requests across 12 model configs)
- **Env**: `AURA_CUSTOM_EVENTS=true`, `AURA_EMIT_REASONING=true`, `AURA_PROMPT_JOURNAL=1`

### Test Prompts

| Label | Prompt | Type |
|-------|--------|------|
| direct-add | "What is 2 + 2?" | Direct answer |
| mean-then-multiply | "Calculate the mean of [10, 20, 30] then multiply the result by 3" | 2-step orchestrated |
| trig-sin45 | "What is sin(45 degrees)?" | Trig (1-2 tools) |
| add-then-mean | "Add 15 and 27, then find the mean of the result and 100" | 2-step orchestrated |
| multi-step-median | "Calculate 7 * 8, then subtract 6, then find the median of [result, 10, 25, 50]" | 3-step orchestrated |

---

## All Models Summary

| Rank | Model | Provider | Med ms | Avg ms | P95 ms | Tools | Dupes | Tasks | TOs | Done | Reasoning Chunks |
|------|-------|----------|--------|--------|--------|-------|-------|-------|-----|------|-----------------|
| 1 | GPT-5.1 (reasoning=medium) | OpenAI API | 5,895 | 4,625 | 8,044 | 0 | 0 | 18 | 0 | 15/15 | 0* |
| 2 | Qwen3-30B-A3B (instruct) | llama-server | 7,395 | 6,216 | 12,186 | 21 | 0 | 21 | 0 | 15/15 | 0 |
| 3 | Qwen3-30B-A3B (coder) | llama-server | 7,827 | 10,344 | 65,998 | 19 | 0 | 21 | 0 | 15/15 | 0 |
| 4 | GPT-5.1 | OpenAI API | 9,764 | 9,393 | 20,542 | 21 | 0 | 21 | 0 | 15/15 | 0 |
| 5 | Qwen3.5-35B-A3B (no-think) | llama-server | 15,329 | 15,111 | 41,658 | 41 | 18 | 25 | 0 | 15/15 | 0 |
| 6 | GLM 4.7 Flash | llama-server | 17,517 | 17,888 | 30,875 | 28 | 0 | 24 | 0 | 15/15 | 9,817** |
| 7 | Claude Opus 4.5 (thinking) | Bedrock | 19,476 | 17,604 | 32,820 | 24 | 0 | 24 | 0 | 15/15 | 1,714 |
| 8 | Nemotron 3 Nano | llama-server | 26,043 | 31,001 | 94,515 | 27 | 0 | 26 | 0 | 15/15 | 42,449** |
| 9 | Claude Sonnet 4.5 | Anthropic API | 27,545 | 25,632 | 64,567 | 27 | 0 | 24 | 0 | 15/15 | 0 |
| 10 | Qwen3.5-35B-A3B (thinking) | llama-server | 28,068 | 23,447 | 41,280 | 23 | 0 | 22 | 0 | 15/15 | 6,100 |
| 11 | Qwen3-30B-A3B (thinking) | llama-server | 35,120 | 32,574 | 64,898 | 23 | 0 | 22 | 0 | 15/15 | 31,862 |
| 12 | Claude Sonnet 4.5 (thinking) | Anthropic API | 37,478 | 34,112 | 58,311 | 24 | 0 | 24 | 0 | 15/15 | 4,407 |

\* GPT-5.1 with reasoning solves math internally, skipping tool calls entirely — 0 MCP tool calls.
\*\* Nemotron and GLM emit `reasoning_content` via llama-server automatically (model chat templates include thinking).

---

## Per-Prompt Breakdown (avg ms / reasoning chunks across 3 iterations)

| Model | direct-add | mean-then-multiply | trig-sin45 | add-then-mean | multi-step-median |
|-------|------------|-------------------|------------|---------------|-------------------|
| GPT-5.1 (reasoning) | 1,417 / 0 | 6,204 / 0 | 1,537 / 0 | 6,827 / 0 | 7,139 / 0 |
| Qwen3 (instruct) | 3,332 / 0 | 7,672 / 0 | 1,441 / 0 | 7,318 / 0 | 11,319 / 0 |
| Qwen3 (coder) | 3,793 / 0 | 8,023 / 0 | 1,526 / 0 | 27,113 / 0 | 11,267 / 0 |
| GPT-5.1 | 1,832 / 0 | 13,365 / 0 | 1,943 / 0 | 10,047 / 0 | 19,778 / 0 |
| Qwen3.5 (no-think) | 2,732 / 0 | 15,636 / 0 | 3,513 / 0 | **31,883** / 0 | 21,791 / 0 |
| GLM 4.7 Flash | 12,346 / 1,353 | 19,418 / 1,342 | 11,426 / 1,477 | 18,594 / 1,453 | 27,656 / 2,192 |
| Opus 4.5 Bedrock (thinking) | 4,492 / 103 | 19,477 / 370 | 24,180 / 503 | 19,362 / 343 | 20,507 / 395 |
| Nemotron 3 Nano | 6,995 / 1,368 | 16,042 / 3,306 | 28,183 / 7,993 | 31,529 / 8,838 | 72,255 / 20,944 |
| Claude Sonnet 4.5 | 3,110 / 0 | 41,726 / 0 | 13,228 / 0 | 28,100 / 0 | 41,995 / 0 |
| Qwen3.5 (thinking) | 7,655 / 354 | 28,659 / 1,340 | 11,687 / 1,040 | 28,564 / 1,382 | 40,670 / 1,984 |
| Qwen3 (thinking) | 7,344 / 1,009 | 38,226 / 7,414 | 18,751 / 4,615 | 40,027 / 7,615 | 58,523 / 11,209 |
| Claude Sonnet 4.5 (thinking) | 5,521 / 169 | 38,466 / 832 | 42,808 / 1,280 | 36,459 / 951 | 47,308 / 1,175 |

Bold = detected tool looping (see below). Format: avg ms / reasoning chunks.

---

## Tool Calls Per Prompt (across 3 iterations)

| Model | direct-add | mean-then-multiply | trig-sin45 | add-then-mean | multi-step-median | Total |
|-------|------------|-------------------|------------|---------------|-------------------|-------|
| GPT-5.1 (reasoning) | 0 | 0 | 0 | 0 | 0 | 0 |
| GPT-5.1 | 0 | 6 | 0 | 6 | 9 | 21 |
| Qwen3 (instruct) | 0 | 6 | 0 | 6 | 9 | 21 |
| Qwen3 (coder) | 0 | 4 | 0 | 6 | 9 | 19 |
| Qwen3 (thinking) | 0 | 6 | 2 | 6 | 9 | 23 |
| Qwen3.5 (thinking) | 0 | 6 | 2 | 6 | 9 | 23 |
| Opus 4.5 Bedrock (thinking) | 0 | 6 | 6 | 6 | 6 | 24 |
| Claude (thinking) | 0 | 6 | 6 | 6 | 6 | 24 |
| Claude Sonnet 4.5 | 0 | 6 | 6 | 6 | 9 | 27 |
| Nemotron 3 Nano | 0 | 6 | 6 | 6 | 9 | 27 |
| GLM 4.7 Flash | 0 | 6 | 6 | 6 | 10 | 28 |
| Qwen3.5 (no-think) | 0 | 6 | 0 | **28** | 7 | 41 |

---

## Reasoning Chunks by Phase

Only models that emit reasoning content are shown. Counts are SSE chunk totals across 3 iterations.

| Model | Prompt | Total | Routing | Workers | Synthesis |
|-------|--------|------:|--------:|--------:|----------:|
| **Claude Opus 4.5 Bedrock (thinking)** | direct-add | 103 | 103 | 0 | 0 |
| | mean-then-multiply | 370 | 130 | 240 | 0 |
| | trig-sin45 | 503 | 153 | 350 | 0 |
| | add-then-mean | 343 | 129 | 214 | 0 |
| | multi-step-median | 395 | 146 | 249 | 0 |
| **Claude Sonnet 4.5 (thinking)** | direct-add | 169 | 169 | 0 | 0 |
| | mean-then-multiply | 832 | 297 | 535 | 0 |
| | trig-sin45 | 1,280 | 480 | 800 | 0 |
| | add-then-mean | 951 | 565 | 386 | 0 |
| | multi-step-median | 1,175 | 565 | 610 | 0 |
| **GLM 4.7 Flash** | direct-add | 1,353 | 1,353 | 0 | 0 |
| | mean-then-multiply | 1,342 | 523 | 491 | 328 |
| | trig-sin45 | 1,477 | 1,095 | 382 | 0 |
| | add-then-mean | 1,453 | 551 | 534 | 368 |
| | multi-step-median | 2,192 | 787 | 928 | 477 |
| **Nemotron 3 Nano** | direct-add | 1,368 | 1,368 | 0 | 0 |
| | mean-then-multiply | 3,306 | 1,496 | 1,472 | 338 |
| | trig-sin45 | 7,993 | 5,839 | 1,770 | 384 |
| | add-then-mean | 8,838 | 5,858 | 2,298 | 682 |
| | multi-step-median | 20,944 | 12,260 | 7,185 | 1,499 |
| **Qwen3-30B-A3B (thinking)** | direct-add | 1,009 | 1,009 | 0 | 0 |
| | mean-then-multiply | 7,414 | 1,261 | 5,052 | 1,101 |
| | trig-sin45 | 4,615 | 2,905 | 1,710 | 0 |
| | add-then-mean | 7,615 | 2,626 | 3,565 | 1,424 |
| | multi-step-median | 11,209 | 2,352 | 6,274 | 2,583 |
| **Qwen3.5-35B-A3B (thinking)** | direct-add | 354 | 354 | 0 | 0 |
| | mean-then-multiply | 1,340 | 497 | 510 | 333 |
| | trig-sin45 | 1,040 | 909 | 131 | 0 |
| | add-then-mean | 1,382 | 396 | 506 | 480 |
| | multi-step-median | 1,984 | 463 | 951 | 570 |

**GPT-5.1 (reasoning=medium)**: No `reasoning_content` in OpenAI streaming response — reasoning is internal, not exposed via API.

---

## Detected Loops

| Model | Prompt | Avg Tools | Cross-Model Median | Dupe Errors | Failed/Total | Signals |
|-------|--------|-----------|-------------------|-------------|--------------|---------|
| Qwen3.5-35B-A3B (no-think) | add-then-mean | 9.3 | 2.0 | 18 | 18/28 (64%) | duplicate errors, 4.7x outlier, 64% fail rate |

Only Qwen3.5 without thinking loops, and only on `add-then-mean`. Enabling thinking eliminates the loop. Aura's duplicate-call guardrail catches it.

---

## Key Observations

1. **GPT-5.1 with reasoning is fastest but skips tools** — it solves math internally rather than using MCP tools. Fast (4.6s avg) but defeats the purpose of orchestration testing. OpenAI does not expose `reasoning_content` in the streaming API.

2. **Qwen3-instruct is the best local orchestrator** — 7.4s median, uses tools correctly, no loops, no reasoning overhead. Best balance of speed and correct tool usage.

3. **Opus 4.5 Bedrock is the best cloud thinking model** — 19.5s median with correct tool usage and reasoning emission. 2x faster than Sonnet thinking (37.5s) with identical tool call patterns. Does not skip tools like GPT-5.1.

4. **Bedrock emits reasoning correctly** — Opus via Bedrock streams thinking content through routing and workers phases, same as Anthropic API. Requires cross-region inference profile (`global.anthropic.*`), not base model IDs.

5. **Thinking roughly doubles latency** — Qwen3.5: 15s vs 28s; Claude Sonnet: 28s vs 37s; Qwen3: 6s vs 35s. Exception: Opus thinking (19.5s) is faster than Sonnet non-thinking (27.5s).

6. **Claude models skip synthesis reasoning** — all local thinking models (Qwen, GLM, Nemotron) reason during synthesis, Claude models (Sonnet + Opus) don't. May indicate more decisive final assembly.

7. **Nemotron is a hidden thinker** — emits reasoning via llama-server's auto-populated `reasoning_content`. 2,830 avg chunks/request explains its high latency despite being a small model.

8. **Tool looping is Qwen3.5-specific** — only with thinking disabled, only on `add-then-mean`. The duplicate-call guardrail (18 errors) catches it but wastes ~6 extra turns.

9. **180/180 requests completed** — 0 timeouts across all 12 model configs. Parallel slot profiles working well.

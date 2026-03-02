# Ollama Guide

Aura supports running local models through [Ollama](https://ollama.ai), including fallback parsing for tool-call formats that are emitted as plain text instead of native tool-call structures.

## Basic Configuration

```toml
[llm]
provider = "ollama"
model = "qwen3:30b-a3b"
# base_url = "http://localhost:11434" # optional; this default is used automatically
fallback_tool_parsing = true

[llm.additional_params]
num_ctx = 32000
think   = true
```

`base_url` defaults to `http://localhost:11434` when omitted. Use `http://host.docker.internal:11434` when Aura runs inside a container and Ollama runs on the host.

All Ollama-specific parameters (`num_ctx`, `num_predict`, `think`, `seed`, `top_k`, `top_p`, etc.) go under `[llm.additional_params]`. See [Ollama model parameters](https://github.com/ollama/ollama/blob/main/docs/modelfile.md#valid-parameters-and-values) for the full list.

## Fallback Tool Parsing

When `fallback_tool_parsing = true`, Aura tries to detect and execute tool calls from text output patterns commonly produced by local model families.

Known handled styles include:

- Pythonic-like calls (for example Llama-style patterns)
- XML-ish function wrappers (common in some Qwen outputs)
- JSON objects containing name/parameters payloads

This improves tool reliability with local models that do not consistently emit structured function-calling payloads.

## Practical Guidance

- Prefer instruction-tuned variants (`*-instruct`) when you need reliable tool execution.
- Keep prompts explicit about expected tool-call output format.
- Validate behavior with your exact model build and quantization.

## "Thinking model" Caveat

Thinking model variants have known malformed XML tool-call issues in some builds. Aura's fallback parser handles many of these cases, but reliability still depends on model artifact quality and prompt format constraints.

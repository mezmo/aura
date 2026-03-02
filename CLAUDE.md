# CLAUDE.md

## Overview

Aura is a TOML-based configuration system for composing Rig.rs AI agents with MCP tools and RAG pipelines.

## Architecture Notes

### Rig Fork

This project depends on a fork of rig-core with streaming and observability fixes not yet upstream. See `docs/rig-fork-changes.md` for what changed and why.

### Tool Event Broker: Sequential Execution Assumption

`tool_event_broker.rs` uses a FIFO queue to correlate `tool_call_id` between the streaming hook and MCP execution. This only works because Rig 0.28 streaming mode executes tools sequentially. If upgrading Rig, verify this still holds. See `docs/rig-tool-execution-order.md`.

### Key Modules

- `provider_agent.rs` - Type-erased streaming across LLM providers
- `stream_events.rs` - Custom `aura.*` SSE events
- `request_cancellation.rs` - Per-request lifecycle and disconnect detection
- `tool_event_broker.rs` - Tool call/result correlation via FIFO queue

## Documentation

- `README.md` - Setup, configuration, and usage
- `docs/streaming-api-guide.md` - SSE streaming events and client examples
- `docs/request-lifecycle.md` - Request lifecycle, timeouts, and cancellation
- `docs/rig-tool-execution-order.md` - Tool execution order analysis
- `docs/rig-fork-changes.md` - Rig fork modifications

# CLI Guide

Interactive terminal client for Aura with conversation persistence, streaming responses, and local tool execution.

## Quick Start

### Build the CLI

```bash
# Default build — includes standalone support (builds agents in-process from TOML config)
cargo build -p aura-cli --release

# HTTP-only build (connects to an aura-web-server; no in-process agents)
cargo build -p aura-cli --release --no-default-features
```

The binary is at `target/release/aura`. The `standalone-cli` feature is on by default, so the standard build runs both backends. Use `--no-default-features` only when you want an HTTP-only client.

### Run it

**Standalone mode** (no server needed) is the default when you don't set `--api-url`:

```bash
aura --config path/to/agent.toml
```

If you omit `--config`, the CLI loads `config.toml` from the current directory, so a bare `aura` runs the local config.

On first run, if that config is missing (for example, a bare `aura` in a directory with no `config.toml`), the CLI doesn't dump a raw filesystem error. Instead, it reports the path it looked for and offers three ways forward: run `aura init` to generate a config in the current directory, pass `--config <path>` to point at an existing config file or directory, or set `--api-url <url>` (or `AURA_API_URL`) to connect to a running aura-web-server instead.

**HTTP mode** (connect to an aura-web-server) is selected when you set `--api-url` (or `AURA_API_URL`):

```bash
export AURA_API_URL="https://api.example.com"
export AURA_API_KEY="your-api-key"
aura
```

The default build includes standalone support. You only need the `--standalone` flag when `AURA_API_URL` is set in your environment but you want to run standalone anyway. Passing `--standalone` overrides the env var. The `--standalone` flag is mutually exclusive with the `--api-url` flag, so never pass both. When you omit `--config` in standalone mode, the CLI loads `config.toml` from the current directory.

## Backends

| Backend | Selected by | Use case | Build |
|---------|-------------|----------|-------|
| **Standalone** (default) | No `--api-url` set (loads `--config`, or `config.toml`) | Run agents in-process without a server | Included in the default build |
| **HTTP** | `--api-url` / `AURA_API_URL` is set | Connect to a running aura-web-server | Any build (an HTTP-only build requires `--api-url`) |

Both backends share the same SSE event parser. All CLI features work identically regardless of backend.

## Configuration

### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `AURA_API_URL` | Base API URL. Setting it (or `--api-url`) selects HTTP mode; when unset, the CLI runs standalone | `http://localhost:8080` (in HTTP mode) |
| `AURA_API_KEY` | Bearer token for authentication | None |
| `AURA_MODEL` | Model name (HTTP: starting model; standalone: agent name/alias) | None |
| `AURA_EXTRA_HEADERS` | Additional headers as `key:value` pairs | None |
| `AURA_ENABLE_CLIENT_TOOLS` | Enable local tool execution | `false` |
| `AURA_ENABLE_FINAL_RESPONSE_SUMMARY` | Generate LLM-based titles per turn | `false` |
| `AURA_LOG_FILE` | Path to diagnostic log file (see [Logging](#logging)) | None |

### CLI Arguments

| Flag | Description |
|------|-------------|
| `--api-url <URL>` | Base API URL; setting it selects HTTP mode |
| `--api-key <KEY>` | Bearer token for authentication |
| `--model <MODEL>` | Model name (HTTP: starting model; standalone: selects agent) |
| `--system-prompt <PROMPT>` | System prompt (standalone: append/replace; HTTP: prompts for confirmation) |
| `--query <QUERY>` | One-shot mode: run a single query and exit |
| `--resume <ID>` | Resume a conversation by ID or prefix |
| `--force` | Bypass warnings (useful in one-shot mode) |
| `--enable-client-tools` | Enable local tool execution |
| `--standalone` | Force standalone mode; overrides the `AURA_API_URL` env var. Only needed when `AURA_API_URL` is set, and mutually exclusive with the `--api-url` flag (standalone is otherwise the default) |
| `--config <PATH>` | Path to TOML config file or directory for standalone mode (defaults to `config.toml` in the current directory) |
| `--log-file <PATH>` | Append diagnostic logs to file (see [Logging](#logging)) |

**Precedence:** CLI flags > environment variables > project `cli.toml` > global `cli.toml` > defaults.

### Configuration File

The CLI reads TOML preferences from two locations:

| File | Purpose |
|------|---------|
| `~/.aura/cli.toml` | Global defaults |
| `<project>/.aura/cli.toml` | Project-local override (found by walking up from `$PWD`) |

```toml
# ~/.aura/cli.toml
api_url = "https://api.example.com"
api_key = "your-api-key"
model = "gpt-4o"
system_prompt = "You are a helpful assistant."
enable_client_tools = false
log_file = "/tmp/aura.log"  # append-only; see Logging section
```

Keep secrets in `~/.aura/cli.toml` or environment variables, not in project configs that might be committed to version control.

## One-Shot Mode

`--query <text>` runs a single conversation turn and exits. The output contract is strict: **stdout contains only the raw assistant response** — exactly what the model produced, with no bullet markers, styled headers, tool-execution summaries, or markdown rendering.

Everything else goes to **stderr**:

- Diagnostic logs from `--log-file` / `AURA_LOG_FILE`
- Permission prompts for local tools (interactive, on TTY)
- Errors and warnings (with `error:` / `warning:` prefixes)

Standard pipe usage works without scrubbing:

```bash
aura --query "summarize the README" > summary.md
aura --query "list three ideas as JSON" | jq .
aura --query "what's the version?" 2>/dev/null | tee log.txt
```

Exit code `0` means stdout contains the complete response; non-zero means stderr explains why and stdout is empty.

The REPL retains its rich formatting (bullet markers, markdown rendering, tool-call summaries). The strict-output rules apply only to `--query` mode.

## Logging

The CLI is silent by default. Enable diagnostic logging by specifying a log file path. When set, tracing events are written to that path in both REPL and one-shot mode, keeping stdout untouched.

Three sources can supply the path, in precedence order:

| Source | Form |
|--------|------|
| CLI flag | `--log-file /tmp/aura.log` |
| Environment | `AURA_LOG_FILE=/tmp/aura.log` |
| `cli.toml` | `log_file = "/tmp/aura.log"` |

The file is opened in **append mode** and created if missing. The default filter mirrors `aura-web-server`'s verbose mode (info-level for aura crates and rig request handling); override with `RUST_LOG`.

> **Log rotation, truncation, and pruning are your responsibility.** The CLI appends indefinitely — it never truncates, rotates, or compresses the file. Use `logrotate`, a cron job, or `truncate -s 0` to keep the file from growing unbounded.

### Standalone-Mode OpenTelemetry

When the CLI runs in standalone mode (the default build, with no `--api-url` set), the agent runs in-process. Set `OTEL_EXPORTER_OTLP_ENDPOINT` and the CLI installs an OpenTelemetry layer with the same trace structure as `aura-web-server` — `agent.stream` → `agent.turn` → `mcp.tool_call`, with `orchestration.*` spans between them in orchestration mode.

The CLI omits `chat_completions` / `streaming_completion` spans because standalone mode has no HTTP layer — those live on a separate trace in the server. HTTP-mode CLIs skip OTel entirely; traces come from the server process.

OTel init is independent of `--log-file`. You can run with traces only, logs only, or both.

| Variable | Purpose |
|----------|---------|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | OTLP collector endpoint (gRPC). When unset, no OTel layer is installed. |
| `OTEL_SERVICE_NAME` | Resource attribute (defaults to `aura`). |
| `OTEL_LOG_LEVEL` | Override the OTel layer's filter. |
| `OTEL_RECORD_CONTENT` | When `true`, prompt/completion/tool args/results are recorded as span attributes. |
| `OTEL_CONTENT_MAX_LENGTH` | Max bytes for content attributes (default 1000). |

## Interactive Commands

| Command | Description |
|---------|-------------|
| `/help` | Show available commands and keyboard shortcuts |
| `/clear` | Start a new conversation (saves current first) |
| `/conversations` | List saved conversations |
| `/resume <filter>` | Resume a conversation by ID prefix or name |
| `/rename <name>` | Rename the current conversation |
| `/model <filter>` | Browse and select a model |
| `/expand` | Toggle expanded/compact tool call view |
| `/stream` | Toggle SSE event stream panel |
| `/style [name]` | Switch visual style: `normal`, `high-contrast`, `no-colors` |
| `/quit` or `/exit` | Exit the REPL |

All commands can be executed while the model is streaming. Run `/help` at any time to print this list.

If you type an unrecognized or partial command, the CLI reports `Unknown command` and points you to `/help` instead of sending the text to the model. As you type, the prompt suggests matching commands. Press `Tab` to complete an abbreviation to its full command name before you submit.

You don't need the leading slash for the REPL to recognize a command word. Type a bare command name such as `exit`, `quit`, `clear`, `help`, or `model`, and the CLI prints a one-line hint pointing to the matching slash command rather than sending the word to the model as a billable chat message. A few abbreviations and editor shortcuts work the same way. `q`, `:q`, `:wq`, and `:x` suggest `/quit`; `bye` and `logout` suggest `/exit`; and `?` suggests `/help`. Recognition is case-insensitive and applies only to single-word input, so ordinary chat such as "how do I quit vim" is never intercepted.

## Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `Enter` | Submit input |
| `Ctrl+C` | Cancel streaming request, or exit if idle |
| `Ctrl+L` | Clear input line (or screen if empty) |
| `Tab` / `Shift+Tab` | Cycle through matches in `/model` and `/resume` |
| `Esc` | Cancel tab-completion or exit stream panel focus |
| `Up` / `Down` | Navigate input history |
| `Page Up` / `Page Down` | Jump 10 entries through history |

## Conversations

Conversations are saved to `~/.aura/conversations/` and can be resumed:

```bash
# From CLI
aura --resume abc123

# From REPL
/conversations          # List saved conversations
/resume abc123          # Resume by ID prefix
/rename my chat         # Name the current conversation
```

## Model Selection

Use `/model` to browse and select models:

```bash
/model              # List all available models
/model gpt          # Filter models matching "gpt"
/model gpt-4o       # Select if unique match
```

In HTTP mode, models come from `/v1/models`. In standalone mode, they come from loaded TOML configs (each config's `alias` or `name` becomes a selectable model).

## Client-Side Tools

> **USE AT YOUR OWN RISK**
>
> Enabling client-side tools gives an LLM the ability to execute commands on your machine with the same privileges as the user running `aura`. This includes running shell commands, reading and modifying files, and searching your filesystem.
>
> The risks include prompt injection (anything the model reads can contain instructions that hijack it into running destructive commands), hallucination (the model can call the wrong tool with wrong arguments), and lack of sandboxing (tools run directly on the host).
>
> Only enable when you trust the model, provider, and all inputs the model can read, and when you are running in a workspace where worst-case loss is acceptable or recoverable.

By default, the CLI is a pure chat client with no local tools. Pass `--enable-client-tools` to opt in:

```bash
# Enable local tools
aura --enable-client-tools
AURA_ENABLE_CLIENT_TOOLS=true aura

# Explicitly disable (overrides config file)
aura --enable-client-tools=false
```

### Available Tools

| Tool | Description |
|------|-------------|
| `Read` | Read file contents (supports chunked reading) |
| `ListFiles` | List directory contents |
| `SearchFiles` | Search file contents with regex or literal patterns |
| `FindFiles` | Find files recursively by glob pattern |
| `FileInfo` | Get file or directory metadata |
| `Shell` | Execute shell commands |
| `Update` | Signal intent to modify or create files |
| `CompactContext` | Compact conversation history |

### Server Opt-In Required

Both the CLI and server must opt in for local tools to work:

- **CLI:** Pass `--enable-client-tools`
- **Server:** Set `enable_client_tools = true` in the agent's TOML config

Client-side tools only work with single-agent configurations. Orchestrated (multi-agent) configs drop client tools with a warning.

## Permissions

Configure which local tools the model can execute by creating `.aura/permissions.json` in your project directory:

```json
{
  "permissions": {
    "allow": ["ListFiles(*)", "Read(*)", "FindFiles(*)", "SearchFiles(*)"],
    "deny": ["Shell(*)"]
  }
}
```

- **Allow rules:** Matching tools execute immediately without prompting.
- **Deny rules:** Matching tools are blocked with a guidance message.
- **No match:** Tools with no matching rule prompt for approval.

The CLI walks up from the current directory to find the closest `.aura/permissions.json`. Permissions are project-scoped only (no global `~/.aura/permissions.json`). Running outside any project directory means every local tool call prompts.

## SSE Stream Panel

Toggle with `/stream` to see raw SSE events in real time. Supported event types:

- `aura.tool_requested` / `aura.tool_start` / `aura.tool_complete`
- `aura.usage` / `aura.tool_usage`
- `aura.progress` / `aura.session_info` / `aura.reasoning`
- `aura.orchestrator.*` (multi-agent orchestration events)

Events use shared types from the `aura-events` crate, ensuring identical parsing between HTTP and standalone modes.

## Reasoning Output

Models that support extended thinking (Anthropic Claude with extended thinking, OpenAI o-series) stream their reasoning process in real time.

### Single-Agent Mode

Reasoning appears as a top-level block:

```
● Reasoning
⎿ Let me analyze the request step by step...
```

Content streams in real time, updating in place as chunks arrive.

### Orchestration Mode

In multi-agent orchestration, reasoning appears in two places.

**Coordinator reasoning** displays at the top level:

```
● Reasoning - coordinator
⎿ I need to decompose this into multiple tasks...
```

**Worker reasoning** displays inline within the worker's task tree:

```
● Task 0: Analyze logs [log_worker] - done
├─ ● ReadFile(path="/var/log/app.log")
│  ⎿ completed in 0.12s
└─ ● Reasoning
   ⎿ Looking at the error patterns in these logs...
```

### Viewing Full Reasoning

Reasoning content is truncated to fit the terminal width during streaming. Use `/expand` to toggle expanded view and see the complete text with all wire-level fields (agent_id, content, session_id, trace_id).

### Server Requirements

Reasoning events require both environment variables on the server:

```bash
AURA_CUSTOM_EVENTS=true AURA_EMIT_REASONING=true cargo run --bin aura-web-server
```

Without these flags, the server does not emit `aura.reasoning` events and the CLI shows no reasoning output.

## Compatibility

Aura CLI speaks the standard OpenAI Chat Completions API and works with any compatible backend:

- Aura by Mezmo
- OpenAI API / Azure OpenAI
- Local models via Ollama, LM Studio, vLLM
- Any service implementing `/v1/chat/completions`

## Related Documentation

- [crates/aura-cli/README.md](../crates/aura-cli/README.md): Complete CLI reference with all options
- [Streaming API Guide](streaming-api-guide.md): SSE protocol, event types, and client examples
- [Request Lifecycle](request-lifecycle.md): Request flow, timeouts, and cancellation behavior

# Aura CLI

A fast, interactive terminal client for chat completions with tool execution. Built primarily as the command line interface for [Aura by Mezmo](https://mezmo.com/aura), but works with **any OpenAI-compatible API** — plug in your own models, agents, or LLM endpoints.

---

## Quick Start

### Build from the mono-repo

```bash
# HTTP-only mode (lightweight, connects to an aura-web-server)
cargo build -p aura-cli --release

# Standalone mode (builds agents in-process from TOML config, no server needed)
cargo build -p aura-cli --release --features standalone-cli
```

The binary will be at `target/release/aura-cli`.

### Run it

#### HTTP mode (connect to an aura-web-server)

```bash
# Using environment variables
export AURA_API_URL="https://api.example.com"
export AURA_API_KEY="your-api-key"
aura-cli

# Or pass flags directly
aura-cli --api-url "https://api.example.com" \
         --api-key "your-api-key"
```

#### Standalone mode (no server needed)

When built with `--features standalone-cli`, the CLI can load agent configs directly and run without an HTTP server. Use the `--standalone` flag along with `--config`:

```bash
# Single TOML config file
aura-cli --standalone --config path/to/agent.toml

# Directory of TOML configs (enables /model switching between agents)
aura-cli --standalone --config configs/

# One-shot query in standalone mode
aura-cli --standalone --config agent.toml --query "hello"

# Select a specific agent from a config directory
aura-cli --standalone --config configs/ --model "Math Agent"
```

In standalone mode, the CLI builds agents in-process using the same code paths as `aura-web-server`. Both MCP tools (from the TOML config) and CLI local tools (Shell, Read, etc.) are available. The `/model` command works identically — it lists all loaded configs and lets you switch between them.

---

## Backends

Aura CLI supports two backends, selected explicitly via the `--standalone` flag:

| Backend                 | When                         | Dependencies                             | Tools                                            |
| ----------------------- | ---------------------------- | ---------------------------------------- | ------------------------------------------------ |
| **HTTP** (default)      | No `--standalone` flag       | Lightweight — just HTTP client           | CLI local tools + server-side MCP tools (hybrid) |
| **Direct** (standalone) | `--standalone --config path` | Full aura stack (agents, MCP, providers) | CLI local tools + MCP tools from TOML config     |

Both backends produce identical SSE event streams and share the same stream parser (`process_sse_events`), so all CLI features (stream panel, tool display, orchestration events, etc.) work identically regardless of backend.

### Feature flag: `standalone-cli`

The `--standalone` and `--config` flags require the `standalone-cli` Cargo feature. Without it, the binary is a lightweight HTTP-only client with no agent or MCP dependencies.

```bash
# HTTP-only (default) — small binary, fast compile
cargo build -p aura-cli

# Standalone — includes full agent framework
cargo build -p aura-cli --features standalone-cli
```

---

## Features

- **Interactive REPL** with conversation history, streaming responses, and markdown rendering
- **One-shot mode** for scripting and pipelines (`--query`)
- **Local tool execution** — the model can read files, search code, list directories, run shell commands, and edit files on your behalf
- **Standalone mode** — run agents directly from TOML config without a web server (`--standalone --config`)
- **Conversation persistence** — pick up where you left off with `--resume` or `/resume`
- **Tab completion** — cycle through matching models and conversations with `Tab` / `Shift+Tab`
- **Model selection** — browse and select models from the server (or loaded configs in standalone mode)
- **Permission system** — control which local tools are allowed, denied, or prompted before execution
- **SSE streaming** — real-time token-by-token output with a toggleable event panel
- **Auto-compaction** — automatic context management when conversations grow large
- **Mid-stream commands** — execute slash commands while the model is still streaming
- **Works with any OpenAI-compatible API** — not locked to a single provider

---

## Environment Variables

| Variable             | Description                                                                 | Default                                               |
| -------------------- | --------------------------------------------------------------------------- | ----------------------------------------------------- |
| `AURA_API_URL`       | Base API URL (paths like `/v1/chat/completions` are appended automatically) | `http://localhost:8080`                               |
| `AURA_API_KEY`       | Bearer token for authentication                                             | _(none)_                                              |
| `AURA_MODEL`         | Model name to use (in standalone mode, selects agent by name/alias)         | _(none — omitted from request, server picks default)_ |
| `AURA_EXTRA_HEADERS` | Additional HTTP headers as comma-separated `key:value` pairs                | _(none)_                                              |

---

## Command Line Arguments

```
aura-cli [OPTIONS]
```

| Flag                       | Env Equivalent | Description                                                                                  |
| -------------------------- | -------------- | -------------------------------------------------------------------------------------------- |
| `--api-url <URL>`          | `AURA_API_URL` | Base API URL                                                                                 |
| `--api-key <KEY>`          | `AURA_API_KEY` | Bearer token for authentication                                                              |
| `--model <MODEL>`          | `AURA_MODEL`   | Model name (HTTP: starting model; standalone: selects agent by name/alias)                   |
| `--system-prompt <PROMPT>` | —              | System prompt (HTTP: see note below; standalone: append/replace agent prompt)                |
| `--query <QUERY>`          | —              | Run a single query and exit (one-shot mode)                                                  |
| `--resume <ID>`            | —              | Resume a previous conversation by ID or prefix                                               |
| `--force`                  | —              | Bypass warnings and non-critical errors (useful in one-shot/query mode)                      |
| `--standalone`             | —              | Run in standalone mode (requires `--config`, requires `standalone-cli` feature)              |
| `--config <PATH>`          | —              | Path to TOML agent config file or directory (requires `--standalone`)                        |

**Precedence:** CLI flags > environment variables > config file > defaults.

---

## Configuration File

You can set defaults in a TOML config file at:

```
~/.aura/config.toml
```

```toml
api_url = "https://api.example.com"
api_key = "your-api-key"
model = "gpt-4o"
system_prompt = "You are a helpful assistant."
```

> **Note on system prompts:** In **HTTP mode**, `--system-prompt` is intended for
> OpenAI-compatible backends that support system messages. **Aura's server ignores system
> role messages** — the CLI will prompt you to confirm whether you're connecting to Aura
> or another service. In **standalone mode**, `--system-prompt` can append to or replace
> the agent's TOML-configured system prompt (you'll be asked which). In one-shot mode
> (`--query`), standalone silently appends; HTTP mode requires `--force`.

Any value set here is overridden by environment variables or CLI flags.

---

## Interactive Commands

Once inside the REPL, the slash commands below are available. All slash commands can be executed while output is streaming.

| Command            | Description                                                         |
| ------------------ | ------------------------------------------------------------------- |
| `/help`            | Show available commands and keyboard shortcuts                      |
| `/clear`           | Start a new conversation (saves the current one first)              |
| `/expand`          | Toggle expanded/compact tool call view                              |
| `/stream`          | Toggle SSE event stream panel                                       |
| `/conversations`   | List saved conversations                                            |
| `/resume <filter>` | Resume a saved conversation by ID prefix or name                    |
| `/rename <name>`   | Rename the current conversation                                     |
| `/model <filter>`  | Browse and select a model (see [Model Selection](#model-selection)) |
| `/quit` or `/exit` | Exit the REPL                                                       |

---

## Keyboard Shortcuts

| Key                     | Action                                                               |
| ----------------------- | -------------------------------------------------------------------- |
| `Enter`                 | Submit input                                                         |
| `Ctrl+C`                | Cancel the current streaming request, or exit if idle                |
| `Ctrl+L`                | Clear the current input line; if the line is empty, clear the screen |
| `Tab`                   | Cycle forward through matches (in `/model` and `/resume`)            |
| `Shift+Tab`             | Cycle backward through matches                                       |
| `Esc`                   | Cancel tab-completion selection, or exit stream panel focus          |
| `Up` / `Down`           | Navigate input history                                               |
| `Page Up` / `Page Down` | Jump 10 entries through input history                                |

When the **stream panel** is visible, arrow keys and page keys scroll through SSE events instead. Press `Esc` to exit stream panel focus.

---

## Model Selection

Use `/model` to browse and select which model to use for requests.

```
/model              # list all available models
/model gpt          # filter models matching "gpt"
/model gpt-4o       # select "gpt-4o" if it matches exactly or uniquely
```

In **HTTP mode**, the model list is fetched from the server's `/v1/models` endpoint. In **standalone mode**, it comes directly from the loaded TOML configs (each config's `alias` or `name` becomes a selectable model).

---

## Permissions

Aura CLI includes a permission system that controls which local tools the model is allowed to execute. Configure permissions by creating a `.aura/settings.json` file in your project directory:

```json
{
  "permissions": {
    "allow": ["ListFiles(*)", "Read(*)", "FindFiles(*)", "SearchFiles(*)"],
    "deny": ["Shell(*)"]
  }
}
```

- **Allow rules** — matching tools execute immediately without prompting.
- **Deny rules** — matching tools are blocked with a guidance message.
- **No match** — tools with no matching rule prompt you for approval.

### Available Local Tools

| Tool             | Description                                                         |
| ---------------- | ------------------------------------------------------------------- |
| `Read`           | Read file contents (supports chunked reading with offset and limit) |
| `ListFiles`      | List directory contents                                             |
| `SearchFiles`    | Search file contents with regex or literal patterns                 |
| `FindFiles`      | Find files recursively by glob pattern                              |
| `FileInfo`       | Get file or directory metadata                                      |
| `Shell`          | Execute shell commands (last resort)                                |
| `Update`         | Signal intent to modify or create files                             |
| `CompactContext` | Compact conversation history by discarding older messages           |

---

## Conversations

Conversations are automatically saved to `~/.aura/conversations/` and can be resumed:

```bash
aura-cli --resume abc123           # from the CLI
/conversations                     # list saved conversations
/resume abc123                     # resume by ID prefix
/rename my chat                    # name the current conversation
```

---

## SSE Stream Panel

Toggle with `/stream` to see raw SSE events in real time. Supported event types:

- `aura.tool_requested` / `aura.tool_start` / `aura.tool_complete`
- `aura.usage` / `aura.tool_usage`
- `aura.progress` / `aura.session_info` / `aura.reasoning`
- `aura.orchestrator.*` — multi-agent orchestration events

Events are shared types from the `aura-events` crate, ensuring identical parsing between HTTP and standalone modes.

---

## Compatibility

Aura CLI speaks the standard [OpenAI Chat Completions API](https://platform.openai.com/docs/api-reference/chat) and works with any compatible backend:

- Aura by Mezmo
- OpenAI API / Azure OpenAI
- Local models via Ollama, LM Studio, vLLM, etc.
- Any service implementing `/v1/chat/completions`

### Hybrid Tool Execution

In HTTP mode, some tools run locally and others run on the server. When the server handles a tool call (via `aura.tool_complete` SSE events), the CLI uses the server's result instead of executing locally.

In standalone mode, all tools (both CLI local tools and MCP tools from TOML config) are registered on the agent and executed in-process.

---

## Building & Testing

```bash
# Build (HTTP-only)
cargo build -p aura-cli

# Build (standalone)
cargo build -p aura-cli --features standalone-cli

# Run tests
cargo test -p aura-cli                           # HTTP-only mode
cargo test -p aura-cli --features standalone-cli  # + standalone tests

# Clippy
cargo clippy -p aura-cli --all-targets
cargo clippy -p aura-cli --features standalone-cli --all-targets

# Run directly
cargo run -p aura-cli -- --api-url "http://localhost:8080"
cargo run -p aura-cli --features standalone-cli -- --standalone --config agent.toml
```

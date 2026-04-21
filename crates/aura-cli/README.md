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

In standalone mode, the CLI builds agents in-process using the same code paths as `aura-web-server`. MCP tools from the TOML config are available. CLI local tools (Shell, Read, Update, ...) become available when **both** sides opt in — pass `--enable-client-tools` and set `[agent].enable_client_tools = true` in the loaded TOML config (single-agent configs only; orchestrated configs drop client tools). See [Client-Side Tools](#client-side-tools) for details. The `/model` command works identically — it lists all loaded configs and lets you switch between them.

---

## Backends

Aura CLI supports two backends, selected explicitly via the `--standalone` flag:

| Backend                 | When                         | Dependencies                             | Tools                                                                            |
| ----------------------- | ---------------------------- | ---------------------------------------- | -------------------------------------------------------------------------------- |
| **HTTP** (default)      | No `--standalone` flag       | Lightweight — just HTTP client           | Server-side MCP tools always; CLI local tools when both sides opt in (see below) |
| **Direct** (standalone) | `--standalone --config path` | Full aura stack (agents, MCP, providers) | MCP tools from TOML config; CLI local tools when `--enable-client-tools` is set  |

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

| Variable                              | Description                                                                                                                                                         | Default                                               |
| ------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------- |
| `AURA_API_URL`                        | Base API URL (paths like `/v1/chat/completions` are appended automatically)                                                                                         | `http://localhost:8080`                               |
| `AURA_API_KEY`                        | Bearer token for authentication                                                                                                                                     | _(none)_                                              |
| `AURA_MODEL`                          | Model name to use (in standalone mode, selects agent by name/alias)                                                                                                 | _(none — omitted from request, server picks default)_ |
| `AURA_EXTRA_HEADERS`                  | Additional HTTP headers as comma-separated `key:value` pairs (e.g. `x-chat-session-id:foo,authorization:Bearer …`); overrides the auto-injected `x-chat-session-id` | _(none)_                                              |
| `AURA_ENABLE_FINAL_RESPONSE_SUMMARY`  | Generate a one-line LLM-based title for each final response (adds an extra round-trip per turn). Set to `true` or `1` to enable.                                    | `false`                                               |
| `AURA_ENABLE_CLIENT_TOOLS`            | Advertise CLI local tools to the model and execute them locally (see [Client-Side Tools](#client-side-tools))                                                       | `false`                                               |

---

## Command Line Arguments

```
aura-cli [OPTIONS]
```

| Flag                                       | Env Equivalent                       | Description                                                                                              |
| ------------------------------------------ | ------------------------------------ | -------------------------------------------------------------------------------------------------------- |
| `--api-url <URL>`                          | `AURA_API_URL`                       | Base API URL                                                                                             |
| `--api-key <KEY>`                          | `AURA_API_KEY`                       | Bearer token for authentication                                                                          |
| `--model <MODEL>`                          | `AURA_MODEL`                         | Model name (HTTP: starting model; standalone: selects agent by name/alias)                               |
| `--system-prompt <PROMPT>`                 | —                                    | System prompt (HTTP: see note below; standalone: append/replace agent prompt)                            |
| `--query <QUERY>`                          | —                                    | Run a single query and exit (one-shot mode)                                                              |
| `--resume <ID>`                            | —                                    | Resume a previous conversation by ID or prefix                                                           |
| `--force`                                  | —                                    | Bypass warnings and non-critical errors (useful in one-shot/query mode)                                  |
| `--enable-client-tools[=<bool>]`           | `AURA_ENABLE_CLIENT_TOOLS`           | Advertise CLI local tools to the model (default: disabled — see [Client-Side Tools](#client-side-tools)) |
| `--enable-final-response-summary[=<bool>]` | `AURA_ENABLE_FINAL_RESPONSE_SUMMARY` | Generate a one-line LLM title for each final response (adds an extra round-trip per turn; default: disabled) |
| `--standalone`                             | —                                    | Run in standalone mode (requires `--config`, requires `standalone-cli` feature)                          |
| `--config <PATH>`                          | —                                    | Path to TOML agent config file or directory (requires `--standalone`)                                    |

**Precedence:** CLI flags > environment variables > project `cli.toml` > global `cli.toml` > defaults.

---

## Configuration File

The CLI looks for a TOML preferences file in two places, with the project file
overriding the global one on a per-field basis:

| File                              | Purpose                                                  |
| --------------------------------- | -------------------------------------------------------- |
| `~/.aura/cli.toml`                | Global defaults (across all projects)                    |
| `<project>/.aura/cli.toml`        | Project-local override, found by walking up from `$PWD`  |

The project lookup walks up from the current working directory until it finds a
`.aura/` directory (the same convention used by `.git`, `.editorconfig`, etc.).
Run the CLI from anywhere inside your project tree and the closest `.aura/cli.toml`
wins. `$HOME` is explicitly skipped so the global file is never picked up twice.

> **Renamed from `config.toml`.** Older versions read `~/.aura/config.toml`. The
> file is still read with a one-time deprecation warning — rename it to `cli.toml`
> at your convenience. The old name collided with Aura **agent** config TOMLs and
> will stop being read in a future release.

```toml
# ~/.aura/cli.toml or <project>/.aura/cli.toml
api_url = "https://api.example.com"
api_key = "your-api-key"
model = "gpt-4o"
system_prompt = "You are a helpful assistant."
enable_client_tools = true   # opt in to local tool execution; default is false
```

> **Note on system prompts:** In **HTTP mode**, `--system-prompt` is intended for
> OpenAI-compatible backends that support system messages. **Aura's server ignores system
> role messages** — the CLI will prompt you to confirm whether you're connecting to Aura
> or another service. In **standalone mode**, `--system-prompt` can append to or replace
> the agent's TOML-configured system prompt (you'll be asked which). In one-shot mode
> (`--query`), standalone silently appends; HTTP mode requires `--force`.

> **Don't commit secrets.** A project `cli.toml` checked into source control will
> share `api_key` with everyone who clones the repo. Keep secrets in
> `~/.aura/cli.toml`, in `AURA_API_KEY`, or pass them with `--api-key`.

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

## Client-Side Tools

> ---
> # **USE AT YOUR OWN RISK**
> ---
>
> **Enabling client-side tools gives an LLM the ability to execute commands on your machine.** That includes running shell commands, reading and modifying files, and searching your filesystem — with the same privileges as the user running `aura-cli`. Treat `--enable-client-tools` as functionally equivalent to handing the model a shell prompt.
>
> **The risks are real:**
> - **Prompt injection.** Anything the model reads — a file, a tool output, an MCP response, a webpage retrieved by another tool — can contain instructions that hijack the model into running destructive commands (`rm -rf`, exfiltrating secrets, modifying source code, etc.).
> - **Hallucination.** The model can confidently call the wrong tool with the wrong arguments. There is no undo for a `Shell("rm ...")` call.
> - **No sandbox.** Tools run directly on the host. There is no container, no chroot, no syscall filter. If you can run it from your shell, the model can run it through the CLI.
> - **The permission system reduces blast radius but is not a security boundary.** Globs are easy to over-grant (`Shell(*)` allows anything). Treat allow-rules conservatively and prefer prompt-on-execute for anything sensitive.
>
> **Only enable when:**
> - You trust the model and provider.
> - You trust every input the model can read (configs, MCP servers, vector stores, URLs).
> - You are running in a workspace where worst-case loss (deleted files, leaked credentials, modified source) is acceptable or recoverable from version control / backups.
>
> Disabled by default. Opting in is your decision and your responsibility.

By default, Aura CLI is a **pure chat client** — no local tools are advertised to the model and the REPL never executes anything on the host. Pass `--enable-client-tools` (or set `AURA_ENABLE_CLIENT_TOOLS=true`) to opt in to local tool execution, at which point the model can call tools like `Shell`, `Read`, and `Update` and the REPL runs them locally with permission checks.

```bash
# Disabled (default) — chat only
aura-cli

# Enabled — local tools available, gated by the permission system
aura-cli --enable-client-tools
AURA_ENABLE_CLIENT_TOOLS=true aura-cli

# Explicitly disable (overrides config file)
aura-cli --enable-client-tools=false
```

### How it works

When enabled, the CLI sends a `tools` field on every request describing the local tools (Shell, Read, Update, ListFiles, FindFiles, SearchFiles, FileInfo). The server registers them as **passthrough** tools — the LLM sees them as callable, but instead of executing server-side, the stream terminates with `finish_reason: "tool_calls"`. The CLI then executes the tool locally (after permission checks), appends the result to the conversation as a `role: "tool"` message, and submits a follow-up request. This keeps the permission system, Update grouping, and other interactive UX firmly in the REPL.

### Backend symmetry

> **Single-agent configurations only.** Client-side tools are not supported when the selected config has `[orchestration].enabled = true`. Tools advertised to an orchestrated config are dropped with a warning. Use a single-agent config (no `[orchestration]`, or `enabled = false`) to enable local tools.

Both halves of the system gate this independently — **and both must opt in for local tools to fire**, in HTTP mode and standalone mode alike. Standalone is not a special case: it uses the same handler path as `aura-web-server`, so the agent's `[agent].enable_client_tools = true` is required there too. The CLI side has a single switch (`--enable-client-tools`) that turns advertisement on or off. The server side is **per-agent** in TOML — `[agent].enable_client_tools = true` opts the single-agent config in, with an optional `client_tool_filter = ["..."]` glob list. There is no server-wide `--enable-client-tools` flag.

| Mode                                 | TOML side (`[agent].enable_client_tools`) | CLI side (`--enable-client-tools`) | Effective                                                                                        |
| ------------------------------------ | ----------------------------------------- | ---------------------------------- | ------------------------------------------------------------------------------------------------ |
| Standalone, single-agent, both on    | `true`                                    | required                           | Local tools fully enabled                                                                        |
| Standalone, single-agent, TOML off   | absent / `false`                          | (any)                              | Tools advertised but the in-process server **silently drops** them; CLI prints a startup warning |
| HTTP, single-agent, both on          | `true`                                    | required                           | Local tools fully enabled                                                                        |
| HTTP, single-agent, agent off        | absent / `false`                          | (any)                              | Tools advertised but server **silently drops** them — no local tools                             |
| Standalone or HTTP, **orchestrated** | any orchestrated config                   | (any)                              | Tools advertised but server **drops with warning** — orchestration unsupported                   |
| Both off                             | absent / `false`                          | CLI without flag                   | Pure chat                                                                                        |

Precedence for resolving the CLI flag: `--enable-client-tools` argument or `AURA_ENABLE_CLIENT_TOOLS` env var > `<project>/.aura/cli.toml` > `~/.aura/cli.toml` (`enable_client_tools = true|false`) > default (`false`). The CLI arg and env var sit at the same tier — either one will override values set in `cli.toml`.

If local tools never fire when you expect them to, check that **both** sides are opted in: the agent's TOML has `enable_client_tools = true` (and the config is single-agent), and `--enable-client-tools` is set on the CLI. In standalone mode the CLI prints a startup warning when the flag is set but no loaded config opts in.

---

## Permissions

Aura CLI includes a permission system that controls which local tools the model is allowed to execute. Permission rules only matter when client-side tools are enabled (see [Client-Side Tools](#client-side-tools)). Configure permissions by creating a `.aura/permissions.json` file in your project directory:

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

The CLI walks up from the current working directory to find the closest
`.aura/permissions.json`, the same way it finds `cli.toml`. Permissions are
**project-scoped only** — there is no `~/.aura/permissions.json`. Running the
CLI from outside any project directory means no permissions are loaded, and
every local tool call prompts.

> **Renamed from `settings.json`.** Older versions read `.aura/settings.json`.
> The file is still read with a one-time deprecation warning; new "always
> allow" rules accepted at the prompt are written to `permissions.json`,
> migrating any existing rules forward.

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

When client-side tools are enabled, both backends execute server-side tools (MCP, RAG) within the agent's stream and pause for client-side tools (Shell, Read, ...) to be executed in the REPL with permission checks. The CLI follows up with a `role: "tool"` result and the agent resumes. Server-side tool results arrive via `aura.tool_complete` SSE events; the CLI uses those rather than executing locally.

When `--enable-client-tools` is off, the CLI only sees server-side tool execution. See [Client-Side Tools](#client-side-tools) for the full flow.

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

# Client-Side Tools

> ---
> # **USE AT YOUR OWN RISK**
> ---
>
> **Enabling client-side tools grants the LLM the ability to call tools that execute on the *client's* machine** — with the same privileges as the user running the client. When a server agent opts in and a client (e.g. the [AURA CLI](../crates/aura-cli/README.md#client-side-tools)) advertises tools like `Shell`, `Read`, or `Update`, the LLM can invoke them and the client executes them locally. This is functionally equivalent to giving the model a shell prompt on every connecting client.
>
> **The risks are real:**
> - **Prompt injection.** Anything the model reads — a file, a tool output, an MCP response, a vector-store hit, a webpage retrieved by another tool — can contain instructions that hijack the model into running destructive commands (`rm -rf`, exfiltrating secrets, modifying source code, etc.). The server cannot tell a legitimate request from an injected one.
> - **Hallucination.** The model can confidently call the wrong tool with the wrong arguments. There is no undo for a `Shell("rm -rf ...")` invocation.
> - **No sandbox.** The server only forwards tool calls; execution happens client-side with full host privileges. There is no container, no chroot, no syscall filter — if you can run it from your shell, the model can run it through the client.
> - **Permission filters reduce blast radius but are not a security boundary.** Server-side `client_tool_filter` and the CLI's allow/deny globs control which tools the model *can ask for* or execute without prompting — not what a tool does once invoked. Globs are easy to over-grant (`Shell(*)` allows anything); treat allow-rules conservatively and prefer prompt-on-execute for anything sensitive.
>
> **Only enable on agents (and clients) where:**
> - You trust the model, the provider, and every data source the model can read (configs, MCP servers, vector stores, web fetches).
> - You trust every client that will connect with local tools enabled and the user account it runs under.
> - Worst-case loss (deleted files, leaked credentials, modified source) is acceptable or recoverable (version control, backups).
>
> Disabled by default on both sides. Opting in is your decision and your responsibility — and your users'.

## How it works

The server honors a `tools` array on incoming chat completion requests. Whether those tools are actually attached to the LLM is a **per-agent opt-in** in TOML — there is no server-wide flag. Tools that get attached are registered as **passthrough** tools: the LLM sees them alongside any server-side MCP tools and can call them, but instead of executing server-side, the stream terminates with `finish_reason: "tool_calls"` so the client can run the tool locally and submit the result back as a `role: "tool"` follow-up.

```bash
# 1) Initial request advertising a client-side tool
curl -X POST http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "stream": true,
    "messages": [{"role": "user", "content": "What time is it?"}],
    "tools": [{
      "type": "function",
      "function": {
        "name": "get_current_time",
        "description": "Get the current time",
        "parameters": {"type": "object", "properties": {}}
      }
    }]
  }'

# 2) Stream ends with finish_reason: "tool_calls". The client executes the tool
#    locally and submits the result back in a follow-up request:
curl -X POST http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "stream": true,
    "tools": [ ... same tools array ... ],
    "messages": [
      {"role": "user", "content": "What time is it?"},
      {"role": "assistant", "content": null, "tool_calls": [
        {"id": "call_abc", "type": "function",
         "function": {"name": "get_current_time", "arguments": "{}"}}
      ]},
      {"role": "tool", "tool_call_id": "call_abc", "content": "2026-04-30T14:30:00Z"}
    ]
  }'
```

When the loaded agent doesn't opt in (the default), any `tools` field on the request is silently dropped; the server runs MCP tools as usual but never asks the client to execute anything. Per-agent opt-in is the design — accepting client-supplied tool definitions means trusting the client to execute them, so it should be a deliberate config decision.

## Single-agent configurations only

Client-side tools are not supported in orchestrated (multi-agent) configurations — when `[orchestration].enabled = true`, any `tools` array on the request is dropped with a warning. The reason: the passthrough mechanism requires terminating the user-facing SSE stream with `finish_reason: "tool_calls"`, which doesn't compose with the coordinator/worker pipeline. If you need local tools, use a single-agent config.

## Server-side configuration

`enable_client_tools` and `client_tool_filter` are per-agent `[agent]` fields — see [`[agent]`](configuration-reference.md#agent) in the configuration reference for the full field table.

```toml
[agent]
name = "Assistant"
system_prompt = "..."
enable_client_tools = true
client_tool_filter = ["Read", "ListFiles", "Find*"]   # optional; omitted/empty = all
```

`client_tool_filter` is a list of glob patterns matched against the request's `tools[].function.name`. An empty or omitted filter means all client tools are available. A request that supplies tools never reaches an agent that did not opt in.

## Client-side configuration (AURA CLI)

The [AURA CLI](../crates/aura-cli/README.md#client-side-tools) is the reference client implementation. `--enable-client-tools` (or `AURA_ENABLE_CLIENT_TOOLS=true`) turns on advertisement of its local tools (`Shell`, `Read`, `Update`, `ListFiles`, `FindFiles`, `SearchFiles`, `FileInfo`); the CLI's permission system (`.aura/permissions.json`) then governs which of those actually execute without a prompt. See the CLI README for the flag/env var syntax, precedence rules, and the permission system.

## Backend symmetry

Both halves of the system gate this independently — **and both must opt in for local tools to fire**, in HTTP mode and standalone mode alike. Standalone is not a special case: it uses the same handler path as `aura-web-server`, so the agent's `[agent].enable_client_tools = true` is required there too.

| Mode                                 | TOML side (`[agent].enable_client_tools`) | Client side (`--enable-client-tools`) | Effective                                                                                        |
| ------------------------------------ | ----------------------------------------- | -------------------------------------- | ------------------------------------------------------------------------------------------------ |
| Standalone, single-agent, both on    | `true`                                    | required                               | Local tools fully enabled                                                                        |
| Standalone, single-agent, TOML off   | absent / `false`                          | (any)                                   | Tools advertised but the in-process server **silently drops** them; CLI prints a startup warning |
| HTTP, single-agent, both on          | `true`                                    | required                               | Local tools fully enabled                                                                        |
| HTTP, single-agent, agent off        | absent / `false`                          | (any)                                   | Tools advertised but server **silently drops** them — no local tools                              |
| Standalone or HTTP, **orchestrated** | any orchestrated config                   | (any)                                   | Tools advertised but server **drops with warning** — orchestration unsupported                   |
| Both off                             | absent / `false`                          | client without flag                    | Pure chat                                                                                         |

If local tools never fire when you expect them to, check that **both** sides are opted in: the agent's TOML has `enable_client_tools = true` (and the config is single-agent), and the client has local tools enabled.

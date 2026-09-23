# Multi-agent directory example

One `aura webserver` process serving three agents. Point `CONFIG_PATH` at
this directory and every `.toml` in it is loaded as a separate agent.

```bash
export OPENAI_API_KEY="sk-..."
npx -y kubernetes-mcp-server@latest --port 8081 --read-only   # for sre.toml
CONFIG_PATH=examples/multi-agent/ DEFAULT_AGENT=sre cargo run --bin aura -- webserver
```

| File | `model` id | Notes |
| --- | --- | --- |
| `sre.toml` | `sre` | Kubernetes MCP tools |
| `writer.toml` | `writer` | no tools, cheaper model |
| `internal-eval.toml` | `eval` | `hidden = true` — callable, but absent from `/v1/models` |

Rules the loader enforces across the directory:

- Every agent's id (`alias`, or `name` when no alias) must be unique.
- `DEFAULT_AGENT` (or `--default-agent`) must match one of those ids, or
  the server refuses to start. With more than one agent and no default,
  requests that omit `model` get a 400.
- Files are loaded in sorted filename order; each is validated on its own.

Try it:

```bash
curl -s localhost:8080/v1/models | jq '.data[].id'     # sre, writer — not eval
aura --api-url http://localhost:8080 --model writer
aura --api-url http://localhost:8080 --model eval       # still works
```

# Security Policy and Data-Handling Model

This document defines AURA's security boundary, deployment responsibilities,
data flows, and vulnerability-reporting policy.

## Safety Boundary

AURA orchestrates model requests and tool calls under the identity and operating
system privileges assigned by its operator. Its controls can limit which tools a
model sees, require approval for configured tool-name patterns, and export an
audit trail. They do not make model output trustworthy, sandbox tools, undo
mutations, or enforce the retention policies of an LLM provider or MCP server.

The operator is responsible for authenticating users, isolating the deployment,
granting least-privilege credentials, selecting trusted model and MCP endpoints,
restricting network egress, reviewing approval policy, and backing up systems a
tool can change. Treat data returned by logs, runbooks, vector stores, webpages,
and MCP servers as untrusted model input.

The web server (`aura webserver`) does not provide end-user authentication or
identity-aware RBAC for its OpenAI-compatible API. Bind it to a private
interface or place it behind an authenticated TLS gateway or service mesh.

An air-gapped deployment requires every enabled network dependency to be local
or disabled. This includes model providers, MCP and RAG services, approval
webhooks, Redis-compatible session stores, OTLP collectors, and any AURA server
used by the CLI. Disable CLI product telemetry, use offline initialization when
provider model-list endpoints are unavailable, and restrict or disable
client-side tools and STDIO MCP processes that can open their own network
connections. Enforce the boundary with network policy; AURA does not implement
a general-purpose egress firewall.

## Telemetry Defaults

AURA exposes two independent mechanisms with different recipients and
purposes. CLI product telemetry is usage data sent to Mezmo by default.
OpenTelemetry is operational data exported only to a collector selected by the
deployment operator.

### Mezmo CLI Product Telemetry

This mechanism gives Mezmo bounded usage signals, associated in the event
payload with a random install ID rather than a user identity, for improving the
AURA CLI. The payload disables PostHog IP and geolocation enrichment, but the
network recipient can still observe transport metadata such as the source IP.
It applies only to the interactive CLI; the web server and core library do not
link or emit this product telemetry. The published server container runs the web
server and therefore does not emit it.

- With no recorded preference, telemetry starts in a held state. The first
  interactive session displays a notice; no event is sent until the user sends
  the first chat message. Sending that message enables telemetry. `aura init`
  and non-interactive one-shot queries do not send product telemetry.
- The event schema is allow-listed in code. It includes a random install ID,
  random session ID, AURA version, coarse OS family and deployment method,
  session mode, whether client tools are enabled, chat start/completion and
  success, and session exit reason. It excludes prompts, responses, file paths,
  host identifiers, model identifiers, token counts, latency, and error text.
- Events are sent to Mezmo's PostHog project by default. An operator can
  override the endpoint, in which case that operator-selected destination is
  the recipient instead of Mezmo. Set `DO_NOT_TRACK=1`,
  `AURA_TELEMETRY_DISABLED=1`, `AURA_TELEMETRY_ENABLED=false`, or
  `[telemetry] enabled = false` in CLI preferences to prevent transmission. CI
  and test environments are disabled automatically. `/telemetry status`,
  `/telemetry recent`, and `/telemetry disable` expose the runtime state.
- A local inspection log is written at
  `~/.aura/telemetry/events.jsonl` by default, including events that were held
  or failed to send. This local audit copy is for the operator and is not itself
  transmitted. It rotates at 1,000 lines and retains one rotated file. Set
  `AURA_TELEMETRY_LOG_EVENTS=0` to disable it. The install ID remains at
  `~/.aura/install-id` until the operator removes it.
- Mezmo and PostHog control retention when the default destination is used. The
  destination owner controls retention when the endpoint is overridden. AURA
  does not enforce remote retention. Disable product telemetry if the recipient
  is outside the deployment's approved data boundary.

### Operator OpenTelemetry Export

This mechanism gives the deployment operator request, model, tool, and
orchestration traces. It is off unless `OTEL_EXPORTER_OTLP_ENDPOINT` is set, and
AURA exports only to the collector at that operator-supplied endpoint. Mezmo
does not receive these traces unless the operator deliberately selects a
Mezmo-operated collector.

Once OTLP export is enabled, treat the collector as a recipient of prompt,
response, and tool content under the current implementation. For AURA-owned
spans, `OTEL_RECORD_CONTENT=false` (the default) omits prompt/completion text and
tool arguments/results, and `OTEL_RECORD_CONTENT=true` includes them subject to
`OTEL_CONTENT_MAX_LENGTH`. However, Rig-owned chat, agent-turn, and tool spans
currently record content independently of that flag; AURA translates their
prompt, response, structured-message, tool-argument, and tool-result attributes
for export. Those attributes are subject to `OTEL_ATTRIBUTE_VALUE_LENGTH_LIMIT`
(65,536 bytes by default), not `OTEL_CONTENT_MAX_LENGTH`.

Accordingly, `OTEL_RECORD_CONTENT=false` is not currently a complete content
suppression control. Do not set `OTEL_EXPORTER_OTLP_ENDPOINT` unless the
collector and its downstream systems are approved to receive model and tool
content. The operator controls their access and retention.

## Credentials

Use environment-variable placeholders such as `{{ env.OPENAI_API_KEY }}`
instead of literal credentials in source TOML, and arrange for the deployment's
secret manager to populate those environment variables. The initialization
flow keeps the placeholder in `config.toml`; if a user enters a new key, the CLI
may write it to a local `.env`. That write uses the process's normal filesystem
permissions rather than enforcing a dedicated restrictive mode, so the
operator must restrict the file and exclude it from version control.

Environment substitution is general-purpose: AURA replaces placeholders
throughout the TOML document before parsing it, not only in credential fields.
A secret referenced from `system_prompt`, another prompt-bearing field, or tool
configuration will be placed there. Resolved credentials also exist in AURA's
process memory and runtime configuration.

When a credential is referenced only from a designated provider or transport
authentication field, AURA passes it to that client and does not deliberately
copy it into a model message. This is not a secret-isolation boundary. A broadly
privileged MCP server, STDIO child process, or client-side shell/file tool may
be able to read process environment, mounted secrets, credential files, or
other host data and return them to the model or another destination. Run AURA
and every MCP server with separate least-privilege identities where possible.

`headers_from_request` can deliberately forward selected inbound headers to an
MCP server or approval webhook. Forward only headers the destination is allowed
to receive, and never use a catch-all proxy rule for authentication headers.

## Data Retention and LLM Egress

A model provider receives the system prompt, user input, relevant conversation
history, and tool results placed into model context. An MCP server receives its
selected tool name and arguments; tool arguments can contain user input or
model-derived data. RAG services, approval webhooks, Redis-compatible session
stores, and OTLP collectors are additional destinations only when configured.
The interactive CLI sends its API requests to its configured AURA server and,
unless disabled as described above, sends product telemetry to the configured
telemetry endpoint. `aura init` can contact a selected provider's model-list
endpoint unless offline mode is used.

These are AURA’s direct application-level data flows. MCP servers, enabled
client-side tools, and STDIO processes may independently initiate additional
network traffic under their own operating-system privileges. Their downstream
behavior and egress policies are outside AURA’s control. Evaluate those
components separately and enforce system-level network policy where required.

Enabled client-side tools and STDIO MCP processes execute with the operator's
OS privileges and may initiate network traffic that AURA cannot enumerate or
constrain. Use process isolation and egress allow-lists to enforce the intended
set of destinations. Do not describe an AURA deployment as sending data "only"
to model providers and MCP servers unless network policy independently enforces
that narrower boundary and every other feature above is disabled.

Provider-side and MCP-side logging, training, residency, and retention are
governed by those services. AURA cannot override them. Select provider terms
and endpoints that meet the deployment's requirements, or use local services.

The OpenAI-compatible server does not persist ordinary chat history; clients
resubmit it with later requests. The CLI saves conversations under
`~/.aura/conversations/` until the user removes them. Scratchpad files,
orchestration artifacts, optional diagnostic logs, and configured session-store
records can also contain operational data. Their storage permissions, expiry,
rotation, backup, and deletion are operator responsibilities.

## RBAC and MCP Permission Boundaries

AURA's `mcp_filter` and `client_tool_filter` settings control which named tools
are exposed to an agent or specialist worker. An explicit empty `mcp_filter`
grants no MCP tools; a missing filter can expose all discovered MCP tools. Use
explicit allow-lists for every agent and worker.

These filters are capability-reduction controls, not user-aware RBAC. The MCP
server and the credential used to connect to it define the authoritative data
and action permissions. Enforce user authentication and RBAC at the ingress,
MCP server, cloud IAM, Kubernetes service account, and network layers. Do not
give an AURA process broader credentials than any allowed tool call requires.

## Read-Only and Mutating Tools

AURA does not infer whether an arbitrary MCP tool is read-only or mutating from
its name or schema. Separate read-only and mutating MCP servers or credentials,
expose only required tools, and match all mutating tool names in the approval
policy. Prefer read-only service accounts for investigative agents and a
separate, narrowly scoped remediation agent for changes.

Client-side tools are disabled by default and require opt-in from both the CLI
and agent configuration. When enabled, `Shell`, file-reading, and file-update
tools run on the client host with the user's privileges and without a sandbox.
Project permission rules can allow, deny, or prompt for matching calls, but glob
rules are not a hard security boundary and can be over-broad.

## Approval and Fail-Closed Behavior

The `[hitl]` policy gates only tool names matching its `require_approval` glob
patterns. A specific matching call reaches its underlying tool only after the
configured route returns an `Approved` decision. Denial short-circuits that
call; timeout, client disconnect, shutdown, malformed or rejected approval,
and approval transport, channel, or storage failure return an error before the
underlying tool runs. These outcomes therefore fail closed for that matching
call. They do not prevent the model from proposing another call.

AURA does not establish that an approver is human. A conversational decision
comes from the connected client, while a webhook decision can be produced by a
person or an automated service. The deployment must authenticate users and
enforce approver identity and authorization outside AURA.

Webhook HMAC signing and response verification are optional. Without a signing
secret, AURA sends unsigned requests, accepts unsigned well-formed responses,
and permits a plaintext `http://` webhook URL. Production webhook deployments
should use TLS and configure AURA's HMAC secret; when a secret is configured,
AURA rejects unsigned or invalid responses and rejects plaintext webhook URLs.

Tools omitted from `require_approval` are not protected by this gate. Approval
also does not prove that a call is correct: the model-generated explanation and
arguments may reflect hallucination or prompt injection. Approvers should
inspect the exact tool, arguments, target, and expected effect, and reject calls
whose impact cannot be determined.

## Prompt Injection from Logs and Runbooks

Logs, alerts, traces, tickets, runbooks, retrieved documents, webpages, and MCP
responses can contain text that instructs the model to ignore policy, reveal
data, or call tools. AURA treats this content as context; it does not guarantee
that a model will distinguish instructions from evidence.

Reduce this risk by combining independent controls:

- Give investigative agents read-only tools and minimal data access.
- Use explicit tool allow-lists and approval patterns for every mutation.
- Keep credentials inaccessible to model-directed shell and file tools.
- Restrict outbound network access and MCP destinations.
- Require an approver to verify source data and exact arguments rather than
  trusting the model's summary.
- Test agents with hostile log lines and runbook content before production use.

## Vulnerability Reporting

Report suspected vulnerabilities privately to
[security@mezmo.com](mailto:security@mezmo.com). GitHub private vulnerability
reporting is not currently enabled for this repository, so do not rely on the
repository's private-advisory submission URL. Include the affected version,
deployment mode, reproduction steps, impact, and any proposed mitigation. Do
not include live credentials, customer data, or an unredacted exploit in a
public issue. Use a public issue only for non-sensitive hardening requests.

AURA does not currently publish a multi-version security-support window.
Security fixes are targeted at the latest released version; maintainers make no
support commitment for older releases or unreleased builds from `main` unless a
release announcement states otherwise. Upgrade to the newest release before
reporting a problem that may already be fixed.

## Release Integrity, Signing, and SBOMs

Release binaries and `.deb`/`.rpm` packages are accompanied by SHA-256 checksums.
The install script requires a matching checksum by default; a mismatch is always
fatal. Keep `AURA_REQUIRE_CHECKSUM=1`, or provide a separately obtained trusted
checksum with `AURA_CHECKSUMS`.

Checksums detect corruption but, when downloaded from the same release location
as an artifact, do not protect against compromise of that release account. AURA
does not currently claim cryptographic signatures or provenance attestations for
release packages or container images, and does not currently publish an SBOM as
a release artifact. Where those controls are required, build from a reviewed
source revision in a controlled pipeline, generate and retain an SBOM, scan the
result, sign it with your organization's trusted identity, and pin deployments
to verified package hashes or container digests.

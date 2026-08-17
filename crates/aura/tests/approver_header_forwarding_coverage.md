# Approver header forwarding: coverage manifest

Describes the unit-level tests that cover the path an approver's captured
identity headers take from an approval decision to one outbound MCP request.
Those tests live beside the code they cover, in the `#[cfg(test)]` modules
named in the tables below.

## Identity-claim scope

A green suite proves the delivery semantics at each seam the override crosses:

- The gate's decision mapping hands captured overrides to the tool call it
  released, and hands nothing to any other outcome.
- Wrapper composition carries a single producer's overrides through and
  refuses a second producer rather than choosing between them.
- The tool adaptor consults its transport tag before dispatch, so a transport
  that cannot carry per-call headers stops the call instead of running it
  under the requester's identity.
- Both HTTP send paths apply the override to exactly the request that carries
  it, replacing the client's frozen value for that request only, and leave the
  JSON body untouched.

The boundary runs in both directions:

- Upstream, the tests take a captured `ApproverHeaders` as given. What a real
  approver system returns, and whether an operator's configured map matches
  it, is outside the claim.
- Downstream, the seam is a loopback HTTP server on `127.0.0.1`. What a real
  MCP server does with a forwarded header, and how the header survives
  proxies, TLS termination, or a gateway that rewrites request headers, is a
  named residual risk rather than a covered surface.

A green suite is a necessary condition for the feature working, never a
sufficient one. Nothing about end-to-end behavior through the web server is
claimed here.

## Covered surfaces

Every surface of the override path appears in this manifest: either as a
covered row here or as a row under Exclusions. A surface in neither place is
a defect in this document, not an implicit exclusion.

| Surface or branch | Test | Notes |
|---|---|---|
| Approved decision carrying overrides maps to a proceed that keeps them | `hitl::gate::tests::approval_result_mapping_carries_captured_overrides_into_the_call` | The only path from a decision to the call it released |
| Approved decision without overrides maps to a proceed without them | `hitl::gate::tests::approval_result_mapping_proceeds_only_on_approval` | |
| The agent-callable `request_approval` surface never captures identity | `hitl::route::tests::webhook_signing::route_wide_approval_discards_identity_headers` | An approved, identity-bearing response under a non-empty mapping resolves to a plain decision; the outcome type has no override channel |
| Denial, timeout, cancellation and channel fault never proceed | `hitl::gate::tests::approval_result_mapping_denial_is_feedback_not_error`, `hitl::gate::tests::approval_result_mapping_timeout_cancel_and_channel_fault_are_errors` | Denial maps to feedback, the rest to errors — neither is a proceed, so nothing may be applied |
| Composition keeps the one producer's overrides across passive wrappers | `tool_wrapper::tests::composed_overrides::the_single_producers_identity_survives_its_passive_neighbours` | Composition builds a fresh outcome, so this is a value-loss seam |
| Two producing wrappers fail the call | `tool_wrapper::tests::composed_overrides::two_producers_fail_the_call_rather_than_pick_one` | Wrapper order must never decide identity |
| Overrides scoped by the wrapper reach the adaptor across its inner-call spawn | `builder::tests::transport_tagging::http_and_sse_tools_are_tagged_so_a_gated_call_delivers_identity` | Exercises the wrapper, the spawn, the adaptor read and the wire in one call |
| Streamable-HTTP tools are tagged as able to deliver | `builder::tests::transport_tagging::http_and_sse_tools_are_tagged_so_a_gated_call_delivers_identity` | |
| SSE tools are tagged as able to deliver | `builder::tests::transport_tagging::http_and_sse_tools_are_tagged_so_a_gated_call_delivers_identity` | |
| Stdio tools are tagged as unable to deliver | `builder::tests::transport_tagging::stdio_tools_are_tagged_so_a_gated_call_fails_closed` | A mistag here would run a gated call under the cached identity |
| Stdio adaptor refuses a gated call before anything is dispatched | `builder::tests::transport_tagging::stdio_tools_are_tagged_so_a_gated_call_fails_closed` | Asserts the recording server saw nothing, not only that the call errored |
| Stdio adaptor runs an ungated call untouched | `mcp_dynamic::tests::stdio_adaptor_runs_an_ungated_call` | The fail-open complement: no override means no transport check |
| Adaptor reads the scoped overrides and threads them to the client | `builder::tests::transport_tagging::http_and_sse_tools_are_tagged_so_a_gated_call_delivers_identity` | Exercises the wrapper, the spawn, the adaptor read and the wire in one call |
| Transport check accepts both HTTP transports | `approver_headers::tests::http_transports_accept_overrides` | |
| Transport check refuses stdio | `approver_headers::tests::stdio_transport_refuses_overrides` | |
| Transport check refuses stdio for an override set with no pairs | `approver_headers::tests::stdio_refuses_even_an_empty_override_set` | Existence of an override value, not its size, is what the check keys off |
| Every captured pair lands on the outbound request | `approver_headers::tests::apply_to_sets_every_captured_pair_on_the_request` | |
| An override replaces a same-named header already on the builder | `approver_headers::tests::apply_to_replaces_a_header_already_on_the_builder` | The per-header setter appends, so pair-by-pair application would send both identities |
| An override replaces the client's frozen header on the wire | `mcp_streamable_http::tests::override_replaces_the_clients_frozen_identity_for_that_call_only` | Also asserts the handshake kept the original identity |
| The override rides exactly one call | `mcp_streamable_http::tests::override_rides_one_call_and_no_later_one` | The next call on the same client carries none |
| Concurrent gated calls keep their own identities | `mcp_streamable_http::tests::concurrent_gated_calls_keep_their_own_identity` | Correlated by the tool name in each recorded body |
| An ungated call carries no override | `mcp_streamable_http::tests::override_rides_one_call_and_no_later_one`, `mcp_streamable_http::tests::override_replaces_the_clients_frozen_identity_for_that_call_only` | Asserted as the second, ungated call of each test — the stronger post-gated-call (staleness) context |
| The override never reaches the JSON body | `mcp_streamable_http::tests::override_never_reaches_the_json_body` | The extension is dropped by the serializer, which is what keeps it off the wire as data |
| The untracked call path delivers the override | `mcp_streamable_http::tests::override_rides_one_call_and_no_later_one` | This path builds its request explicitly rather than delegating to the client library |
| The tracked call path delivers the override | `mcp_streamable_http::tests::call_tool_delivers_the_override_on_either_branch` | Reaches `call_tool_tracked` through the public `call_tool` dispatch, the route production takes under the web server |
| Both paths behave alike on one client | `mcp_streamable_http::tests::call_tool_delivers_the_override_on_either_branch` | Which path a call takes must not decide whether identity is delivered |
| The SSE send path applies the override once, keeps it out of the body, and leaves the next ungated message alone | `mcp_sse::tests::approver_overrides::send_applies_the_override_to_that_post_alone` | Exercises the transport's send directly, gated then ungated on one transport |
| Capture rekeys a response header under the configured outbound name | `approver_headers::tests::captures_response_value_under_outbound_name` | |
| Capture reads response headers case-insensitively | `approver_headers::tests::response_lookup_is_case_insensitive` | |
| A repeated response header yields exactly one captured value | `approver_headers::tests::repeated_response_header_captures_only_the_first_value` | |
| A partial capture fails and names every missing header | `approver_headers::tests::missing_names_are_all_reported_and_sorted` | Also pins the exact Display text: every missing name present, no captured value |
| An adaptor call outside any scope reads no overrides | `approver_headers::tests::unscoped_read_yields_none` | Agents with no wrapper are a live path, not an edge case |
| A gated call carrying an override stamps its header names, sorted and joined, on the execution span | `mcp_tool_execution::tests::applied_headers_span::gated_call_stamps_the_applied_header_names_never_values` | Also asserts the captured value never lands on the span |
| An ungated call's execution span carries no `applied_headers` attribute | `mcp_tool_execution::tests::applied_headers_span::ungated_call_records_no_applied_headers` | |

## Exclusions

| Surface excluded | Reason | Owning test |
|---|---|---|
| End-to-end forwarding through the web server, from an approval response to a tool that echoes what it received | Needs a running mock approver and a running MCP fixture, so it cannot be a unit test | The feature-gated suite under `crates/aura-web-server/tests/` |
| The error text a caller sees when a configured header is missing from the approval response | Its shape is an HTTP-boundary concern; the capture failure itself is covered above | The feature-gated suite under `crates/aura-web-server/tests/` |
| An ungated tool call through the full server stack | Nothing in the override path runs, so a unit test would assert the absence of a feature rather than its behavior | The feature-gated suite under `crates/aura-web-server/tests/` |
| Rejection of reserved outbound header names at config parse | Belongs to config validation, not to delivery | `aura_config::config::tests::hitl_webhook_tool_headers_reserved_name_rejected` |
| Rejection of duplicate outbound names after lowercasing | Same | `aura_config::config::tests::hitl_webhook_tool_headers_duplicate_after_lowercase_rejected` |
| Capture from a live approval round trip, including denial, timeout, cancellation and signature failure | Covered where the webhook client is tested rather than where headers are applied | `hitl::route::tests::webhook_signing` |
| Behavior against a real MCP server over a real network | Named residual risk: the harness answers over loopback in one hop, so nothing in between gets a chance to rewrite a header | |
| Behavior after an upgrade of the MCP client library past the pinned version | Named residual risk: the override is read inside a trait method whose signature that library owns, and later versions change it. The read point needs re-verifying at any such upgrade | |

## Harness constraints

Constraints the loopback harness obeys so the tests stay stable.

| Constraint | Where it binds | Notes |
|---|---|---|
| One request per connection, closed by the server | The recording server's reply | Keeps the reader off keep-alive framing; every test reads whole requests |
| The server answers a stream request with 405 | The client library opens a server-to-client stream once it has a session | 405 is the documented "no such stream", which the client absorbs |
| Concurrent calls are correlated by the tool name in the request body | The concurrency test | Arrival order is not asserted, only that each request carries its own identity |
| The adaptor tests tag a stdio adaptor over a reachable HTTP client | The transport-tag tests | The tag, not the wire, is what the check consults, so this is the case that must still refuse |

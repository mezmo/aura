# Amendment: park and resume decrees from the #271 delivery wave

- Status: **accepted**
- Deciders: Mike Shearer
- Date: 2026-09-30
- Amends: [2026-06-16 HITL approval architecture](2026-06-16-hitl-approval-architecture.md)

Technical Story: [#271](https://github.com/mezmo/aura/issues/271)

## Context and Problem Statement

The #271 wave delivered park/resume on top of the 2026-06-16 approval
architecture, under a series of rulings ("decrees") issued during the
N0–N3 sessions. Those rulings lived in board logs, session ledgers, and
commit messages; the base ADR predates all of them and still reads as
current where the rulings changed the ground. This amendment records
the decrees in one place so the next reader has a source of truth. It
is deliberately light: each entry states the ruling and where its
enforcement lives, not the full reasoning, which the linked records
carry.

## Decisions

### A1 — the id-channel split

The authorize POST body's `request_id` derives from `scope.run_id` for
Worker scope on park-armed asks and notify legs only; the fresh
`req_<uuid>` flows end-to-end for channels (SSE, cancellation, MCP
tracking); durable rows (parked rows, checkpoints, the decision store)
MUST NOT key on the fresh id — they key on `run:<id>`. Derivation
MUST recompute `run:<id>` from the run id, never read it from config.
Enforced in `hitl/route.rs` (`build_approval_post`); pinned by the
nine `a1_` frames (route tests and resume goldens). Ruling:
2026-09-25, recorded verbatim in commit `e40468c6`.

### The 207 bridge is the only park entry

Parking enters through the bridge that mints `run:`-owned rows on a
207 ask response. `park_pre_call` (the conversational park path) is
retired; conversational asks hold inline and never park. Enforced in
`hitl/gate.rs`; pinned by the bridge frames and the absence of the
retired path's tests.

### Admission posture

The typed admission matrix (`crate::park::validate_park_admission`)
runs FIRST in `Config::validate`; interval and timeout checks follow
(Poll-only); availability refusals follow that. Availability prose
MUST name the missing surface (`hitl.route.delivery`,
`hitl.park.enabled`), not the route mode. Poll delivery and durable
parking MUST be refused at every candidate tip until the live-test
gate authorizes activation.

### Strict authority decode

D3: the parked row's authority field decodes strictly; a wrong
authority on resolve is indistinguishable from unknown. This
supersedes the N1 transitional decode, which accepted either form.
Enforced in the `ApprovalAuthority` decode (session_store `record.rs`)
and the authority-gated resolve boundaries in `session_store/file.rs`.

### The MCP arm rides the run

`McpManager::set_current_request` does not survive the #690 events
refactor. A resumed segment arms MCP by binding the request's
observable run (`RunContext::channel_on` keyed by the fresh request
id, receiver pumped into the SSE channel); `for_resume_segment` binds
`current_run()`. Cancellation stays keyed by that run; run-scoped MCP
emissions reach the consumer. A synthesized run with a dropped
receiver MUST NOT substitute for the arm (Gate A finding, repaired).

### Production-entry tests refuse

Tests MUST NOT add production entry points to reach a positive path.
When a restack recuts such a test to empty, its content lands in a
restoration ledger row mapping it to the source revision that carries
it. `#[ignore]` and production bypasses are not restoration.

## Consequences

- Positive: one normative record for the wave's rulings; the base ADR
  stays intact and points here for everything after 2026-09-30.
- Negative: the entries are summaries. A reader who needs the full
  argument must follow the links below; if those session artifacts
  are lost, the tests are the remaining enforcement.
- Known gap: live-path behavior (Gate M consumer smoke) is N4b work
  and authorizes nothing here; activation of poll delivery and
  durable parking remains gated on it.

## Links

- Base ADR: [2026-06-16](2026-06-16-hitl-approval-architecture.md)
- Plan of record: `aura-session-docs` board, plan
  `2026-09-27-271-implementation-sessions.md` (N0–N5 DAG, directives)
- Session ledgers: `.review/n3-review/RESOLUTIONS.md` (N3-R/S
  series, Gate A/D rounds) and the N2 equivalents — uncommitted
  working artifacts; the committed evidence is the test suite
- Design notes: `crates/aura/src/orchestration/park/resume/DESIGN.md`
  (A1 priority rule, executed rebase note),
  `crates/aura/src/orchestration/park/SKELETON.md`
- PR: [#707](https://github.com/mezmo/aura/pull/707) at `f5992c40`

# HITL webhook HMAC design - W3 typed-holes skeleton

This document is the design-panel input for card W3 (#399). It describes the
HMAC-SHA256 root-of-trust module for the approval exchange. The accompanying
`signing.rs` is a Layer 1 skeleton: full public type surface, `todo!()`
bodies, zero behavior.

The signed payload is `"{unix_seconds}.{context}.{raw_body}"`. The `{context}`
segment binds each signature to a resource and direction, so a captured
signature cannot be re-aimed at another resource within the skew window. The
context registry the fill and both seams use:

- egress approval-request POST: `approval-request:{decision_id}`
- ingress decision POST and the webhook-response leg:
  `approval-decision:{decision_id}`

**Injectivity.** `UnixTimestamp` is a `u64` re-rendered as canonical decimal
(no `+`, no leading zeros), and `SigningContext` forbids the `.` delimiter, so
the first two `.` characters split the signed string unambiguously and no two
distinct `(timestamp, context, body)` triples collide. This argument depends
on those two constraints; widening `UnixTimestamp` to a string, adding a
dotted field, or relaxing the context charset would break it.

## 1. Type-to-business-rule map

| Public type | Business rule it enforces | Invalid state it makes unrepresentable |
|---|---|---|
| `PrimarySecret` | Only the primary secret may sign egress. | A secondary secret cannot be passed to `WebhookHmac::sign`; signing takes `&self` and uses the primary. |
| `SecondarySecret` | The secondary verifies ingress only during rotation. | There is no `sign` method on `SecondarySecret`; it cannot produce an egress signature. |
| `SigningContext` | Every signature is bound to a resource and direction, and the binding never contains the payload delimiter. | An empty, non-ASCII, or `.`-bearing context cannot be constructed; `SigningContext::new` returns `ContextError`. |
| `SignatureHeader` | The `X-Aura-Signature-256` value is `sha256=` followed by 64 lowercase hex chars. | A malformed or non-hex signature cannot be constructed; parsing returns `VerificationError::MalformedSignature`. |
| `Signature` | A parsed signature tag is exactly 32 bytes. | A tag of any other length cannot be represented; a non-constant-time `==` cannot be written (no `PartialEq`, no byte accessor). |
| `UnixTimestamp` | `X-Aura-Timestamp` is a canonical-decimal non-negative integer of unix seconds. | A signed, leading-zero, or non-numeric timestamp cannot be represented; parsing returns `VerificationError::MalformedTimestamp`. |
| `Tolerance` | Skew tolerance is between 1 and `MAX_TOLERANCE_SECS` (86400s). | Zero or an out-of-range tolerance cannot be constructed; `Tolerance::new` returns `ConfigError`. |
| `SignedHeaders` | Egress headers are a matched pair from one signing operation. | Removing field accessors and consuming the pair via `into_pairs` makes mixing a signature and timestamp from different results deliberate work on raw strings, not a zero-effort default; it is atomic-use hygiene, not an unrepresentable state (a mismatched pair is a self-inflicted 401, no attacker leverage). |
| `WebhookHmac` | Signing and verification exist only when a secret is configured. | The feature-off state is `Option<WebhookHmac>`; `sign`/`verify` are unreachable without one. |
| `VerifiedBody` | Body bytes reach the deserializer only after authorization ran on exactly those bytes. | `VerifiedBody` wraps immutable `Bytes` (not a generic `AsRef<[u8]>`, whose slice could vary between reads). Its only public constructor is `authorize_ingress`, which consumes the body: a caller cannot obtain the verified bytes without the authorization result, and the bytes are frozen at verification time (residual risk 7). |
| `ConfigError` / `ContextError` / `SigningError` / `ClockError` / `VerificationError` | Failures are classified precisely; no catch-all. | Each failure has a named, testable case; a misconfiguration cannot be silently read as feature-off. |

## 2. Visibility and seam table

| Item | Visibility | Seam | Notes |
|---|---|---|---|
| `signing.rs` module | private (`mod signing;`) | Internal to `aura` crate | Re-export deferred until U(design) ratifies the surface (see §5). Rows below describe the post-re-export surface. |
| `WebhookHmac::new` | `pub` | Constructor | Byte-oriented, flow-agnostic. Enforces the `MIN_SECRET_BYTES` (32) floor on primary and secondary; the floor lives here, not on the secret constructors, so the policy has one home. |
| `WebhookHmac::load_from_env` | `pub` | Env loader | Thin HITL-named wrapper over `new`. Reads `AURA_HITL_WEBHOOK_SECRET`, `_SECONDARY`, and `AURA_HITL_WEBHOOK_TOLERANCE_SECS`. `Ok(None)` only when the primary is absent from the environment altogether; a secondary-without-primary, empty/short primary, malformed/out-of-range tolerance, or non-Unicode value returns `Err`. **Startup logging contract**: `warn!` "HITL webhook HMAC verification DISABLED" on `Ok(None)`; `info!` on `Ok(Some)` naming secondary-present and tolerance, never key material. |
| `WebhookHmac::sign` | `pub` | Egress signing | **Seam**: `WebhookClient::request_approval` (`route.rs`) serializes `ApprovalRequestWire` with `serde_json::to_vec` and calls `sign(&context, &body)` with context `approval-request:{decision_id}`; the two headers attach via `SignedHeaders::into_pairs`. |
| `WebhookHmac::verify` | `pub(crate)` | Internal verification | Configured-path step called by `authorize_ingress`; not exposed. Evaluates primary then, if configured, secondary, so timing reveals only "did the primary sign this" to a key holder. |
| `authorize_ingress` | `pub` | Ingress + response verification | **Seam (ingress)**: `resolve_approval` (`handlers.rs`) swaps its extractor to `Bytes`, calls `authorize_ingress` **before** parsing the path UUID, with context `approval-decision:{decision_id}`, then feeds the verified bytes through the stock `Json` extractor. **Seam (Route A response)**: `route.rs` verifies the webhook *response* body+headers with the same call and context before treating it as a decision (see §4). |
| `VerifiedBody` | `pub` | Verified witness | Consumes an immutable `Bytes`; `as_ref`/`into_inner` expose the authorized bytes. Only `authorize_ingress` constructs it. |
| `SignatureHeader::parse` | `pub` | Header parsing | Rejects anything not `sha256=<64 lowercase hex>`. |
| `UnixTimestamp::parse` | `pub` | Header parsing | Rejects `+`, leading zeros, non-numeric. |
| `UnixTimestamp::now` | `pub` | Clock source | Fallible (`ClockError`), never panics on a pre-epoch clock. |
| `SigningContext::new` | `pub` | Context builder | Validates non-empty, ASCII, delimiter-free. |
| `PrimarySecret` / `SecondarySecret` | `pub` | Secret types | Infallible `new`; `Debug` redacts. No public byte access; key material stays inside the module. Length floor enforced in `WebhookHmac::new`. |
| `Signature` | `pub` | Opaque tag | 32 bytes, no `PartialEq`, no byte accessor. |
| `Tolerance` | `pub` | Config value | 1..=86400s; `Default` is 300. |
| `SignedHeaders` | `pub` | Signing result | Matched pair; consumed via `into_pairs`, no field accessors. |
| `ConfigError`/`ContextError`/`SigningError`/`ClockError`/`VerificationError` | `pub` | Error taxonomy | `VerificationError` carries validated newtypes in `SkewedTimestamp`; all variants map to one uniform 401 on the wire, variant detail is log-only (never in a response body). |

## 3. Named residual risks

1. **Secret encoding - DECIDED.** Env var values are raw UTF-8 bytes used
   directly as HMAC key material. Operators generate at least 32 random bytes
   (`openssl rand -hex 32` yields a 64-char ASCII key, used as-is). Documented
   in `docs/hitl.md`; no base64/hex decoding step.
2. **Unset vs empty primary - fail loud on empty.** Only an *unset*
   `AURA_HITL_WEBHOOK_SECRET` yields `Ok(None)` (feature off, intentional). A
   var that is present but empty or whitespace-only is a misconfiguration, not
   a disable: `load_from_env` returns `Err(ConfigError::PrimaryTooShort {
   len: 0 })`. This diverges deliberately from the session-store trim-empty
   precedent (`aura-config/src/session_store.rs:136`), because a silently
   disabled security control is the failure mode the vet flagged. The
   startup-log contract (§2) makes the disabled state greppable regardless.
3. **Constant-time verification across both secrets - RESOLVED.** The
   primary-then-secondary fallback distinguishes only "primary matched" (one
   HMAC) from "everything else" (both HMACs); "secondary matched" and "no
   match" share a timing class, so the observable requires already holding the
   primary key. The fill computes both when a secondary is configured and
   never varies the error variant by which secret failed (one `Mismatch`).
4. **Clock skew source.** `UnixTimestamp::now` uses `SystemTime::now` and is
   fallible on a pre-epoch clock. Container drift or host jumps can reject
   legitimate requests; 300s is standard but not a guarantee. Skew arithmetic
   uses `abs_diff` so an attacker-supplied future timestamp cannot underflow.
5. **Raw-body extraction in axum.** The `Json` → `Bytes` swap is a visible
   signature change; any middleware that also consumes the body stream
   conflicts (single-consumption). The fill also re-creates the `Json`
   extractor's 415 (wrong content-type) and 422 (bad/unknown-field) responses
   by hand and keeps `DefaultBodyLimit` so HMAC never runs over an unbounded
   body; golden-frame tests for 415/422 land *before* the swap.
6. **Secondary-secret lifecycle.** No API rotates or expires a secondary; it
   stays until the env var is unset and the process restarts. See the rotation
   runbook in §6.
7. **Seam bypass by handler defect.** `authorize_ingress` consumes the body
   and returns the only handle to the verified bytes, so the ordinary path
   cannot skip it. A handler that captured the raw `Bytes` separately before
   calling could still parse them; this is now deliberate perversity visible
   in a diff rather than a zero-keystroke default. Controls: Gate A seam
   review, the unsigned-⇒-401 e2e acceptance, and auth-before-path-parse
   ordering.
8. **Header hygiene at the seam.** `authorize_ingress` takes `Option<&str>`
   header values, so the `aura-web-server` seam owns two conversions:
   `HeaderValue::to_str` failure maps to the missing-header 401 (never
   `unwrap`), and a duplicated `X-Aura-*` header uses `HeaderMap::get` (first
   value). Header *names* are matched case-insensitively via `HeaderMap`;
   values stay strict (exact `sha256=`, lowercase hex).

## 4. Out-of-scope seams and the Route A decision

1. **Asymmetric or per-target identity.** Single shared symmetric secret this
   wave; the 271 T1-D identity ADR owns asymmetric identity. Documented seam.
2. **Replay beyond timestamp skew (held-request flow only).** The 300s
   tolerance bounds the window; exactly-once is P7's durable decision FSM
   (#398, DECISIONS ruling 1). This deferral is sound here **only because**
   `store.resolve` destructively takes the record, so an in-window duplicate
   of the *same* decision gets 404. That justification is flow-specific: a
   request-and-approvals resource model where a POST *creates* a record has no
   such protection and must re-derive replay handling per resource.
3. **Route A response leg - IN SCOPE (Opus N2).** On the webhook route the
   decision arrives as the HTTP *response* to AURA's signed POST. It is
   verified with the same `authorize_ingress` over the response body and its
   `X-Aura-*` headers, context `approval-decision:{decision_id}`, before being
   treated as a decision. `WebhookUrl` also rejects `http://` when a
   secret is configured (a plaintext response channel would defeat the point).
   Without this the card cannot claim "root of trust on both legs" for Route
   A.

## 5. `#![allow(dead_code)]` removal plan and hole-inventory baseline

The module starts with `#![allow(dead_code)]` because every behavior body is a
`todo!()`. The allow is removed in the fill PR when the last `todo!()` is
replaced.

The module is deliberately private (`mod signing;`) until U(design) ratifies
the surface, so `aura-web-server` cannot reach the seam yet. Fill step 1,
post-ratification: add to `hitl/mod.rs`

```rust
pub use signing::{
    authorize_ingress, SignedHeaders, SigningContext, VerifiedBody,
    VerificationError, WebhookHmac, SIGNATURE_HEADER, TIMESTAMP_HEADER,
};
```

The cross-crate `pub` surface (what `aura-web-server` may name) gets its first
real review at Gate A, since the panel reviewed it only within `aura`.

Baseline hole inventory (from `signing.rs`):

| Function | Line | Hole |
|---|---|---|
| `SigningContext::new` | 105 | Validate non-empty, ASCII, delimiter-free. |
| `SignatureHeader::parse` | 128 | Parse `sha256=<hex>`, validate lowercase/length. |
| `UnixTimestamp::parse` | 162 | Parse canonical decimal; reject `+`/leading zeros. |
| `UnixTimestamp::now` | 168 | `SystemTime::now` to unix seconds, fallible. |
| `Tolerance::new` | 187 | Enforce 1..=`MAX_TOLERANCE_SECS`. |
| `SignedHeaders::into_pairs` | 220 | Emit the two `(name, value)` header entries. |
| `WebhookHmac::new` | 242 | Enforce secret-length floor; build config. |
| `WebhookHmac::load_from_env` | 258 | Read env, validate, log, delegate to `new`. |
| `WebhookHmac::sign` | 269 | Build `{ts}.{context}.{body}`, HMAC, hex, headers. |
| `WebhookHmac::verify` | 283 | Skew via `abs_diff`; primary then secondary; constant-time. |
| `authorize_ingress` | 408 | Feature-off passthrough; parse headers; skew; verify. |

Removal condition: delete `#![allow(dead_code)]` and run the three cargo gates
plus the W3 unit-test suite (known-answer vector, skew both directions incl.
`u64::MAX`/`0`/`now+tolerance+1`, secondary rotation accept, tampered body
reject, cross-context reject, missing-header reject when secret set, all-pass
when secret absent).

## 6. Rotation runbook

Secrets are read once at startup, so rotation is a three-restart procedure:

1. Set `AURA_HITL_WEBHOOK_SECRET_SECONDARY` to the new key on every AURA
   instance and restart. Ingress now accepts old (primary) or new (secondary);
   egress still signs with the old primary.
2. Update the receiver(s) to the new key. Update the peer that posts decisions
   to sign with the new key.
3. Promote: set `AURA_HITL_WEBHOOK_SECRET` to the new key, clear
   `_SECONDARY`, restart. The old key no longer verifies.

Each step is a rolling restart; at no point is verification disabled.

## 7. Test construction policy (Gate A B3)

`std::env::set_var` is process-global, so tests must not mutate the
environment while any other test in the same binary can read it.

- Inside `aura`, tests build `WebhookHmac` from parts via the public
  `WebhookHmac::new` (no environment involved). Only the `load_from_env`
  tests in `signing.rs` touch the env (they exist to exercise exactly that
  path), and they serialize behind the module's single `ENV_LOCK`.
- `WebhookClient::new` deliberately does not read the environment; the
  binary entrypoints load signing once at startup and thread it into
  `HitlRuntime::from_config` (the production path). This keeps
  `WebhookClient::new` tests env-free.
- `aura-web-server` cannot name the secret part types (the facade exports
  only the DESIGN §5 set), so its ingress tests obtain one `WebhookHmac`
  through `load_from_env` inside a `OnceLock` initializer: the env is
  mutated exactly once per test binary, serialized by the `OnceLock`, and
  the value is cached. That initializer must remain the only env mutation
  in the aura-web-server test binary.

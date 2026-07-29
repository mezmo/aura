# HITL webhook HMAC design — W3 typed-holes skeleton

This document is the design-panel input for card W3 (#399). It describes the
HMAC-SHA256 root-of-trust module proposed for both legs of the approval
webhook exchange. The accompanying `signing.rs` is a Layer 1 skeleton: full
public type surface, `todo!()` bodies, zero behavior.

## 1. Type-to-business-rule map

| Public type | Business rule it enforces | Invalid state it makes unrepresentable |
|---|---|---|
| `PrimarySecret` | Only the primary secret may sign egress. | A secondary secret cannot be passed to `WebhookHmac::sign`; the signing path requires `PrimarySecret`. |
| `SecondarySecret` | The secondary secret verifies ingress only during rotation. | There is no `sign` method on `SecondarySecret`; it cannot be used to produce egress signatures. |
| `SignatureHeader` | The `X-Aura-Signature-256` value is a `sha256=` prefix followed by 64 lowercase hex chars. | A malformed or non-hex signature cannot be constructed; parsing returns `VerificationError::MalformedSignature`. |
| `Signature` | A verified signature is exactly 32 bytes. | A signature of any other length cannot be represented; it is rejected at parse time. |
| `UnixTimestamp` | `X-Aura-Timestamp` is a non-negative integer of unix seconds. | A non-numeric or negative timestamp cannot be represented; parsing returns `VerificationError::MalformedTimestamp`. |
| `Tolerance` | Skew tolerance is a positive number of seconds. | Negative tolerance is unrepresentable (`u64`); zero is allowed but must be explicitly requested. |
| `SignedHeaders` | Egress headers are produced as a matched pair from one signing operation. | A signature header and timestamp header generated independently cannot be represented; the type binds them. |
| `WebhookHmac` | Signing and verification are only available when a secret is configured. | The "feature off" state is `Option<WebhookHmac>`; `sign`/`verify` are unreachable when the feature is off. |
| `VerificationError` | Verification failures are classified precisely for observability and testing. | A catch-all error variant does not exist; every failure has a named, testable case. |

## 2. Visibility and seam table

| Item | Visibility | Seam | Notes |
|---|---|---|---|
| `signing.rs` module | `pub(crate)` via `hitl/mod.rs` | Internal to `aura` crate | Re-exports intentionally deferred until the panel ratifies the public surface. |
| `WebhookHmac::load_from_env` | `pub` | Env loader | Reads `AURA_HITL_WEBHOOK_SECRET` and `AURA_HITL_WEBHOOK_SECRET_SECONDARY`; returns `None` when the primary is absent/empty. |
| `WebhookHmac::sign` | `pub` | Egress signing | **Untouched integration seam**: `route.rs:195-230` (`WebhookClient::request_approval`) serializes `ApprovalRequestWire` to bytes with `serde_json::to_vec`, then calls `sign` and attaches `SIGNATURE_HEADER`/`TIMESTAMP_HEADER`. |
| `WebhookHmac::verify` | `pub` | Ingress verification | **Untouched integration seam**: `handlers.rs:1032-1051` (`resolve_approval`) switches its axum extractor to `Bytes`. After verifying the two `X-Aura-*` headers it deserializes with `serde_json::from_slice::<ApprovalDecisionWire>(&bytes)`. |
| `SignatureHeader::parse` | `pub` | Header parsing | Called by the ingress seam; rejects anything that is not `sha256=<64 hex chars>`. |
| `UnixTimestamp::parse` | `pub` | Header parsing | Called by the ingress seam; rejects non-numeric timestamps. |
| `PrimarySecret` / `SecondarySecret` | `pub` | Secret types | Construction via `new` only; `Debug` redacts material. |
| `SignedHeaders` | `pub` | Signing result | Public fields because both `signature` and `timestamp` are validated by construction. |
| `VerificationError` | `pub` | Error taxonomy | Carries validated newtypes in `SkewedTimestamp`, never bare domain values. |

## 3. Named residual risks

1. **Secret encoding.** The skeleton treats the env var value as raw HMAC key bytes. If operators expect base64 (`whsec_...`) or hex, the first deployment will silently use the wrong key material. The design panel must ratify the encoding.
2. **Env-var precedence.** `load_from_env` reads directly from `std::env`. It does not follow the `env_var` helper's trim-empty rule from `aura-config/src/session_store.rs:136` yet; the filled implementation must reuse that helper or match it exactly.
3. **Constant-time verification across both secrets.** `verify` tries the primary secret and falls back to the secondary. A naive implementation may leak which secret matched through short-circuit timing, so the panel must confirm the constant-time strategy (single comparison path or merged verification).
4. **Clock skew source.** `UnixTimestamp::now` uses `SystemTime::now`. Container clock drift or host time jumps can cause legitimate requests to be rejected. The 300s default is industry standard but not a guarantee.
5. **Raw-body extraction in axum.** Changing the handler extractor to `Bytes` is a visible signature change. Any middleware that also consumes the body stream will conflict because axum bodies are single-consumption.
6. **Secondary-secret lifecycle.** There is no API to rotate or expire a secondary secret; it stays configured until the env var is unset. A long rotation window increases exposure if the old secret is compromised.

## 4. Out-of-scope seams

1. **Asymmetric or per-target identity.** AURA does not yet have a target cryptographic identity for webhooks. The 271 T1-D identity ADR owns this; W3 uses a single shared symmetric secret and documents the seam.
2. **Replay protection beyond timestamp skew.** The 300s tolerance bounds the replay window but does not provide exactly-once delivery. P7's durable decision FSM (#398, deferred per DECISIONS-2026-07-28 ruling 1) owns exactly-once semantics.

## 5. `#![allow(dead_code)]` removal plan and hole-inventory baseline

The module starts with `#![allow(dead_code)]` because every behavior body is a
`todo!()`. The allow is removed in the fill PR when the last `todo!()` is
replaced.

Baseline hole inventory (from `signing.rs`):

| Line | Function | Hole |
|---|---|---|
| `SignatureHeader::parse` | ~l60 | Parse `sha256=<hex>` and validate length/hex. |
| `UnixTimestamp::parse` | ~l95 | Parse decimal unix seconds into `UnixTimestamp`. |
| `UnixTimestamp::now` | ~l100 | Convert `SystemTime::now` to unix seconds. |
| `WebhookHmac::load_from_env` | ~l125 | Read env vars, build `PrimarySecret` and optional `SecondarySecret`. |
| `WebhookHmac::sign` | ~l140 | Build signed payload, compute HMAC-SHA256, hex-encode, return headers. |
| `WebhookHmac::verify` | ~l147 | Check skew, recompute HMAC with primary then secondary, constant-time compare. |

Removal condition: delete `#![allow(dead_code)]` and run the three cargo gates
plus the W3 unit-test suite (known-answer vector, skew both ways, secondary
rotation, tampered body, missing header when secret set, all-pass when secret
absent).

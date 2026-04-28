# Implementation Plan: AURA-RM-008 Structured Error Taxonomy

## Summary

Create `ErrorCategory` enum and `AuraError` struct in the `aura` crate. Add `code` field to `ErrorDetail` in `aura-web-server`. Map existing error sources. Update all 6 error construction sites with sanitization. Unit tests inline.

## Implementation Order

1. New module `error_taxonomy.rs` in `aura` crate with enum + struct + From impls
2. Expose from `lib.rs`
3. Add `code: Option<String>` to `ErrorDetail` in `aura-web-server/src/types.rs`
4. Add `From<&StreamTermination>` in `aura-web-server`
5. Update all 6 `ErrorDetail` construction sites in handlers.rs and main.rs
6. Unit tests (inline in both crates)

## Estimated Scope

- 2 new files (error_taxonomy.rs, tests inline)
- 3 modified files (lib.rs, types.rs, handlers.rs, main.rs)
- ~200 lines of new code + ~150 lines of tests

# Architecture Spec: [AURA-RM-NNN] [Title]

**Status:** Draft | In Review | Approved | Implemented
**Roadmap Item:** AURA-RM-NNN
**Product Spec:** [link to product-spec.md]
**Author:** [name]
**Created:** [date]
**Last Updated:** [date]

---

## Summary

[One paragraph: what is being built and the core technical approach.]

## Constitution Compliance Check

- [ ] Article I: Does this change preserve OpenAI API compatibility?
- [ ] Article II: Does this respect crate boundary separation?
- [ ] Article V: Are new integration tests behind feature flags?
- [ ] Article VI: Are secrets handled via env var templates?
- [ ] Article VII: Are new config fields backward-compatible with defaults?

## Technical Context

- **Language/Version:** Rust (edition 2024, stable toolchain)
- **Affected Crates:** [list crates modified]
- **New Dependencies:** [list any new Cargo.toml deps with justification]
- **Performance Objectives:** [latency, throughput, memory targets if applicable]

## Design

### Crate Changes

#### `aura` (core)
- [New modules, types, traits]
- [Modified modules with summary of changes]

#### `aura-config`
- [New config structs/fields]
- [Migration/backward compat notes]

#### `aura-web-server`
- [New routes, middleware, handlers]
- [Request/response changes]

### Data Flow

[Describe the request path through the system. Use ASCII diagrams where helpful.]

```
Request → [middleware] → Handler → Agent → [new component] → Response
```

### Key Types / Interfaces

```rust
// Show the key new types, traits, or function signatures
```

### Error Handling

[How errors from this feature propagate. Which error types are used. How they surface to the API consumer.]

### Configuration

```toml
# Full TOML config example with all new fields and their defaults
```

## Migration / Backward Compatibility

[What happens to existing deployments? Do existing configs need changes? Is there a migration path?]

## Alternatives Considered

| Approach | Pros | Cons | Why Not |
|----------|------|------|---------|
| [Alternative 1] | | | |
| [Alternative 2] | | | |

## Risks

- [Risk 1: description and mitigation]
- [Risk 2: description and mitigation]

## Implementation Order

[Ordered list of implementation steps, referencing which acceptance criteria each satisfies.]

1. [Step 1] — Satisfies: AC-NNN.N.N
2. [Step 2] — Satisfies: AC-NNN.N.N

# Quality & Testing Spec: [AURA-RM-NNN] [Title]

**Status:** Draft | In Review | Approved | Implemented
**Roadmap Item:** AURA-RM-NNN
**Product Spec:** [link to product-spec.md]
**Architecture Spec:** [link to architecture-spec.md]
**Author:** [name]
**Created:** [date]
**Last Updated:** [date]

---

## Test Strategy

- **Unit Tests:** [scope and approach]
- **Integration Tests:** [scope, infrastructure needed]
- **End-to-End Tests:** [scope, tools used (Playwright, curl, etc.)]
- **Performance Tests:** [if applicable — load, latency, concurrency]

## Acceptance Criteria Traceability

Every acceptance criterion from the product spec must map to at least one test case.

| Acceptance Criterion | Test Type | Test Case ID | Test Location | Status |
|---------------------|-----------|-------------|---------------|--------|
| AC-NNN.1.1 | Unit | TC-NNN.1.1.1 | `crates/aura/src/module_tests.rs::test_name` | Pending |
| AC-NNN.1.2 | Integration | TC-NNN.1.2.1 | `crates/aura-web-server/tests/test_file.rs::test_name` | Pending |
| AC-NNN.2.1 | E2E | TC-NNN.2.1.1 | `tests/e2e/test_file.rs` | Pending |

## Test Cases

### TC-NNN.1.1.1: [Test Case Title]

**Satisfies:** AC-NNN.1.1
**Type:** Unit | Integration | E2E
**Location:** `crate/path/to/test_file.rs::test_function_name`

**Setup:**
- [Preconditions, fixtures, config]

**Steps:**
1. [Action]
2. [Action]

**Expected Result:**
- [Assertion 1]
- [Assertion 2]

**Edge Cases Covered:**
- [Edge case 1]
- [Edge case 2]

### TC-NNN.1.2.1: [Test Case Title]
[Repeat structure above]

## Test Infrastructure

### Required Fixtures
- [Config files, mock servers, test data]

### Required Services
- [Docker containers, MCP servers, databases]

### Environment Variables
- [Test-specific env vars needed]

## Quality Gates

All must pass before the feature is considered complete:

- [ ] All unit tests pass (`cargo test --workspace --lib`)
- [ ] All integration tests pass (`make test-integration`)
- [ ] Zero compiler warnings
- [ ] Clippy clean (`cargo clippy --all-targets -- -D warnings`)
- [ ] Code formatted (`cargo fmt --check`)
- [ ] No regressions in existing test suite
- [ ] Acceptance criteria traceability: every AC has at least one passing test
- [ ] Performance targets met (if applicable)

## Coverage Notes

[What is explicitly NOT tested and why. E.g., "LLM provider actual API calls are not tested in unit tests — covered by integration tests with real credentials."]

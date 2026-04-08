# Contributing to Aura

Thank you for your interest in contributing to Aura! This guide will help you get started, whether you're fixing a bug, adding a feature, improving documentation, or proposing an idea.

Aura is in **Open Alpha** — contributions of all kinds are welcome and appreciated.

## Table of Contents

- [Contributing to Aura](#contributing-to-aura)
  - [Table of Contents](#table-of-contents)
  - [Code of Conduct](#code-of-conduct)
  - [Contributor License Agreement](#contributor-license-agreement)
  - [Getting Help](#getting-help)
  - [Ways to Contribute](#ways-to-contribute)
  - [Development Setup](#development-setup)
    - [Prerequisites](#prerequisites)
    - [Getting Started](#getting-started)
  - [Project Structure](#project-structure)
  - [Development Workflow](#development-workflow)
    - [Branch Strategy](#branch-strategy)
    - [Typical Workflow](#typical-workflow)
    - [Useful Make Targets](#useful-make-targets)
  - [Code Quality Standards](#code-quality-standards)
    - [Formatting](#formatting)
    - [Linting](#linting)
    - [General Guidelines](#general-guidelines)
  - [Testing](#testing)
    - [Unit Tests (Required)](#unit-tests-required)
    - [Integration Tests (Encouraged)](#integration-tests-encouraged)
    - [Writing Tests](#writing-tests)
  - [Commit Message Convention](#commit-message-convention)
    - [Format](#format)
    - [Types](#types)
    - [Examples](#examples)
    - [Breaking Changes](#breaking-changes)
    - [Important Notes](#important-notes)
  - [Submitting a Pull Request](#submitting-a-pull-request)
    - [Before You Submit](#before-you-submit)
    - [After You Submit](#after-you-submit)
    - [PR Guidelines](#pr-guidelines)
    - [PR Template](#pr-template)
  - [Review Process](#review-process)
  - [Documentation](#documentation)
  - [Reporting Issues](#reporting-issues)
  - [License](#license)

## Code of Conduct

This project follows the [Contributor Covenant Code of Conduct](CODE_OF_CONDUCT.md). By participating, you are expected to uphold this code. Please report any concerns to the maintainers.

## Contributor License Agreement

All contributors — both individual and corporate — must agree to the [Mezmo Contributor License Agreement (CLA)](CLA.md) before their contributions can be accepted. The CLA ensures that you grant the necessary rights for your contributions to be included in the project while you retain ownership of your work.

By submitting a pull request, commit, or other contribution to this repository after being presented with the CLA, you accept and agree to its terms. If you are contributing on behalf of your employer, please ensure you have authorization to accept the agreement on their behalf.

Organizations with ten (10) or more contributors may contact [cla@mezmo.com](mailto:cla@mezmo.com) to execute a separate Corporate Contributor License Agreement covering all authorized contributors within the organization.

For any questions about the CLA or licensing, please reach out to [cla@mezmo.com](mailto:cla@mezmo.com).

## Getting Help

- **GitHub Discussions** — Ask questions, share ideas, or discuss approaches before opening a PR: [Discussions](https://github.com/mezmo/aura/discussions)
- **GitHub Issues** — Report bugs or request features: [Issues](https://github.com/mezmo/aura/issues)

## Ways to Contribute

There are many ways to contribute beyond writing code:

- **Report bugs** — Found something broken? [Open an issue](#reporting-issues).
- **Suggest features** — Have an idea? Start a [Discussion](https://github.com/mezmo/aura/discussions) or open an issue.
- **Improve documentation** — Typo fixes, clarifications, new guides, and examples are always welcome.
- **Add example configurations** — Share useful agent configurations in `examples/`.
- **Write tests** — Help increase test coverage with unit or integration tests.
- **Review pull requests** — Thoughtful reviews help maintain quality and are a great way to learn the codebase.
- **Triage issues** — Help reproduce bugs, clarify reports, or identify duplicates.

## Development Setup

### Prerequisites

- **Rust (nightly)** — Aura requires the nightly toolchain. The repo includes a `rust-toolchain.toml` that handles this automatically.
- **Docker and Docker Compose** — Required for integration tests and containerized builds.
- **Git** — For version control and contributing via pull requests.

### Getting Started

1. **Fork the repository** on GitHub (external contributors) or clone directly (maintainers).

2. **Clone your fork:**

   ```bash
   git clone https://github.com/<your-username>/aura.git
   cd aura
   ```

3. **Install Rust** (if you don't have it):

   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```

   The nightly toolchain will be installed automatically when you first build, thanks to `rust-toolchain.toml`.

4. **Set up your configuration:**

   ```bash
   cp examples/reference.toml config.toml
   cp .env.example .env
   ```

   Edit `.env` with your API keys. At minimum, you'll need an `OPENAI_API_KEY` if you plan to run the server or integration tests.

5. **Build the project:**

   ```bash
   cargo build --workspace
   ```

6. **Verify everything works:**

   ```bash
   cargo test --workspace
   ```

   This runs formatting checks, unit tests, and clippy in one command.

## Project Structure

Understanding the crate layout will help you navigate the codebase:

```text
aura/
├── crates/
│   ├── aura/                # Core agent builder library
│   ├── aura-config/         # TOML parser and config loader
│   ├── aura-web-server/     # OpenAI-compatible HTTP/SSE server
│   └── aura-test-utils/     # Shared testing utilities
├── compose/                 # Docker Compose files for testing
├── docs/                    # Architecture and protocol documentation
├── examples/                # Example TOML configurations
│   ├── reference.toml       # Complete annotated configuration
│   ├── minimal/             # Bare minimum per-provider configs
│   └── complete/            # Full agent composition examples
├── scripts/                 # CI and utility scripts
└── development/             # LibreChat and OpenWebUI integration
```

**Key architectural docs** to read before diving into the code:

- [docs/streaming-api-guide.md](docs/streaming-api-guide.md) — SSE protocol, event types, and client handling
- [docs/request-lifecycle.md](docs/request-lifecycle.md) — Request flow, timeouts, and cancellation
- [docs/rig-fork-changes.md](docs/rig-fork-changes.md) — Why we use a Rig.rs fork and what changed
- [docs/rig-tool-execution-order.md](docs/rig-tool-execution-order.md) — Tool execution ordering (important for `tool_event_broker.rs`)

## Development Workflow

### Branch Strategy

- **External contributors**: Fork the repo and create a feature branch in your fork.
- **Maintainers**: Create branches directly in the repository.

All changes are merged to `main` via **rebase merging** to maintain a linear commit history.

### Typical Workflow

1. **Create a branch** from the latest `main`:

   ```bash
   git checkout main
   git pull origin main
   git checkout -b feat/your-feature-name
   ```

2. **Make your changes**, keeping commits focused and atomic.

3. **Run quality checks** before pushing:

   ```bash
   make fmt-check
   cargo test --workspace
   ```

4. **Push your branch** and open a pull request.

### Useful Make Targets

| Command              | Description                           |
| -------------------- | ------------------------------------- |
| `make build`         | Build all workspace crates            |
| `make build-release` | Build in release mode                 |
| `make fmt`           | Format code with rustfmt              |
| `make fmt-check`     | Check formatting (CI mode)            |
| `make lint`          | Run clippy with warnings as errors    |
| `make test`          | Run cargo tests + integration tests   |
| `make ci`            | Run all checks: fmt-check, test, lint |
| `make clean`         | Clean build artifacts                 |

You can also simulate the full CI pipeline locally:

```bash
./scripts/test-ci.sh
```

## Code Quality Standards

### Formatting

All code must be formatted with `rustfmt`:

```bash
cargo fmt --all
```

CI will reject unformatted code. Run `make fmt` before committing.

### Linting

All code must pass `clippy` with warnings treated as errors:

```bash
cargo clippy --all-targets --all-features -- -D warnings
```

### General Guidelines

- Follow existing patterns and conventions in the codebase.
- Keep changes focused — one logical change per PR.
- Avoid unnecessary refactoring alongside feature work. If refactoring is needed, submit it as a separate PR.
- Write clear, descriptive variable and function names. Prefer readability over cleverness.
- Add comments where the "why" isn't obvious from the code. Don't comment on the "what" — the code should speak for itself.
- Ensure your changes don't introduce compiler warnings.

## Testing

### Unit Tests (Required)

All contributors must run tests before submitting a PR:

```bash
cargo test --workspace
```

Unit tests don't require any external services or API keys.

### Integration Tests (Encouraged)

Integration tests verify end-to-end behavior through the web server, including LLM interaction and MCP tool execution. They **require a real `OPENAI_API_KEY`** because they make actual API calls to OpenAI. MCP tool execution is handled by mock servers — no external tool APIs are called.

If you have an API key and want to run integration tests locally:

```bash
# Start local test infrastructure (mock MCP servers + Aura)
make test-integration-local-up

# Run the integration test suite
cargo test --package aura-web-server --features integration --no-fail-fast -- --test-threads=1

# Tear down when done
make test-integration-local-down

# Or do it all in one command:
make test-integration-local
```

Integration tests run single-threaded (`--test-threads=1`) due to LLM API rate limits.

**Feature flags** allow running specific test suites:

| Flag                            | Suite                                         |
| ------------------------------- | --------------------------------------------- |
| `integration`                   | All integration tests                         |
| `integration-streaming`         | Streaming functionality                       |
| `integration-header-forwarding` | MCP header forwarding                         |
| `integration-mcp`               | MCP tool execution                            |
| `integration-events`            | Custom `aura.*` events                        |
| `integration-cancellation`      | Request cancellation                          |
| `integration-progress`          | MCP progress notifications                    |
| `integration-vector`            | Vector store / RAG (requires external Qdrant) |

Example — run only streaming tests:

```bash
cargo test --package aura-web-server --features integration-streaming --no-fail-fast -- --test-threads=1
```

### Writing Tests

- **Unit tests**: Place `#[cfg(test)]` modules in the same file as the code they test.
- **Integration tests**: Add to `crates/aura-web-server/tests/`. Use the `aura-test-utils` crate for shared test helpers.
- If your change is user-facing, consider adding or updating integration tests.
- If your change is internal/algorithmic, unit tests are preferred.

## Commit Message Convention

This project uses [Conventional Commits](https://www.conventionalcommits.org/) and enforces them via CI. **Every commit** on `main` must follow this format because we use rebase merging to maintain a linear history.

### Format

The first line, which includes the type and description, must be entirely lowercase. The body
and optional footer can use lower and upper casing.

```
<type>(<optional scope>): <description>

[body]

[optional footer(s)]
```

### Types

| Type       | When to Use                                             |
| ---------- | ------------------------------------------------------- |
| `feat`     | A new feature                                           |
| `fix`      | A bug fix                                               |
| `doc`      | Documentation only changes                              |
| `style`    | Formatting, missing semicolons, etc. (no code change)   |
| `refactor` | Code change that neither fixes a bug nor adds a feature |
| `perf`     | Performance improvement                                 |
| `test`     | Adding or updating tests                                |
| `chore`    | Build process, tooling, or dependency updates           |
| `ci`       | CI/CD configuration changes                             |

### Examples

```
feat(config): add support for gemini provider

fix(streaming): correct sse event ordering on disconnect

doc: add ollama troubleshooting guide

test(mcp): add header forwarding integration tests

refactor(provider-agent): simplify type-erased streaming dispatch
```

### Breaking Changes

For breaking changes, add `!` after the type/scope and include a `BREAKING CHANGE` footer:

```
feat(config)!: rename [tools] section to [mcp.tools]

BREAKING CHANGE: The [tools] config section has been renamed to [mcp.tools].
Update your config.toml files accordingly.
```

### Important Notes

- The commit message subject should be lowercase and not end with a period.
- Keep the subject line under 72 characters.
- Use the body to explain **what** and **why**, not **how**.
- CI will reject commits that don't follow this convention.

## Submitting a Pull Request

### Before You Submit

1. **Rebase on latest `main`** to ensure a clean history:

   ```bash
   git fetch origin
   git rebase origin/main
   ```

2. **Run tests locally:**

   ```bash
   cargo test --workspace
   ```

3. **Ensure all commits follow** the [Conventional Commits](#commit-message-convention) format.

4. **Update documentation** if your change affects user-facing behavior, configuration, or APIs.

### After You Submit

- Check the comments in the PR for:
  - The status of automated checks
  - Automated request to sign contributors agreement
  - Feedback from other contributors

### PR Guidelines

- **Title**: Use a clear, descriptive title that summarizes the change.
- **Description**: Explain what the PR does and why. Include:
  - Context and motivation for the change
  - Summary of the approach taken
  - Any trade-offs or alternatives considered
  - Testing performed (unit, integration, manual)
- **Size**: Keep PRs focused and reasonably sized. Large changes should be broken into a series of smaller, reviewable PRs.
- **Draft PRs**: If you want early feedback on a work-in-progress, open a draft PR.

### PR Template

When opening a PR, consider including:

```markdown
## What

Brief description of what this PR does.

## Why

Motivation and context for the change.

## How

Summary of the approach and any notable implementation details.

## Testing

How this was tested (unit tests, integration tests, manual testing).

## Checklist

- [ ] Code follows existing patterns and conventions
- [ ] `cargo test --workspace` passes locally
- [ ] Commits follow Conventional Commits format
- [ ] Documentation updated (if applicable)
- [ ] Tests added or updated (if applicable)
```

## Review Process

- All PRs require at least one maintainer review before merging.
- Reviewers may request changes — this is normal and part of maintaining quality.
- PRs are merged via **rebase merge** to maintain a linear commit history.
- Be responsive to review feedback. If you disagree with a suggestion, explain your reasoning — constructive discussion improves outcomes.
- After approval, a maintainer will merge your PR.

## Documentation

Good documentation is as valuable as good code. If your change affects any of the following, please update the relevant docs:

- **Configuration options** — Update `examples/reference.toml` and the relevant `examples/` files.
- **API behavior** — Update `docs/streaming-api-guide.md` or `docs/request-lifecycle.md`.
- **New features** — Add usage examples to the README or create a new guide in `docs/`.
- **Architecture changes** — Update the relevant doc in `docs/` and note any impacts in `CLAUDE.md`.

## Reporting Issues

When reporting a bug, please include:

- **Aura version** (`cargo metadata --format-version=1 | jq -r '.packages[] | select(.name=="aura") | .version'` or check `Cargo.toml`)
- **Rust toolchain version** (`rustc --version`)
- **Operating system and version**
- **Steps to reproduce** the issue
- **Expected behavior** vs. **actual behavior**
- **Relevant logs** (set `RUST_LOG=debug` or `RUST_LOG=aura=trace` for verbose output)
- **Configuration** (sanitized — remove API keys and secrets)

For feature requests, describe the use case and the problem you're trying to solve. Starting a [Discussion](https://github.com/mezmo/aura/discussions) first is a great way to refine ideas before opening a formal issue.

## License

By contributing to Aura, you agree that your contributions will be licensed under the [Apache License, Version 2.0](LICENSE), the same license that covers the project. All contributions are also subject to the [Contributor License Agreement](CLA.md).

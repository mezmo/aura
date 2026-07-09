# Contributing to AURA

Thank you for contributing! Bug reports, features, documentation, example configurations, tests, reviews, and issue triage are all welcome.

Setup, build, test, and architecture documentation lives in [DEVELOPMENT.md](DEVELOPMENT.md). This guide covers the contribution process itself.

## The Short Version

1. Fork the repo (external contributors) or branch from `main` (maintainers).
2. Make a focused change: one logical change per PR, with tests.
3. Run `make fmt-check`, `cargo test --workspace`, and `make lint` before pushing.
4. Write [Conventional Commits](#commit-messages); verify with `make lint-commits`.
5. Open a PR explaining what and why. A maintainer reviews, then rebase-merges.

Contributions require agreement to the [CLA](#contributor-license-agreement), and this project follows the [Contributor Covenant Code of Conduct](CODE_OF_CONDUCT.md).

## Getting Help

- **[Community Slack](https://mezmo.com/r/slack-aura)**: ask questions, share ideas, or discuss approaches before opening a PR.
- **[GitHub Issues](https://github.com/mezmo/aura/issues)**: report bugs or request features.

## Contributor License Agreement

All contributors, both individual and corporate, must agree to the [Mezmo Contributor License Agreement (CLA)](CLA.md) before their contributions can be accepted. The CLA ensures that you grant the necessary rights for your contributions to be included in the project while you retain ownership of your work.

By submitting a pull request, commit, or other contribution to this repository after being presented with the CLA, you accept and agree to its terms. If you are contributing on behalf of your employer, please ensure you have authorization to accept the agreement on their behalf.

Organizations with ten (10) or more contributors may contact [cla@mezmo.com](mailto:cla@mezmo.com) to execute a separate Corporate Contributor License Agreement covering all authorized contributors within the organization. For any questions about the CLA or licensing, reach out to [cla@mezmo.com](mailto:cla@mezmo.com).

## Development Workflow

Environment setup, build instructions, project structure, and Make targets are documented in [DEVELOPMENT.md](DEVELOPMENT.md).

1. Create a branch from the latest `main`:

   ```bash
   git checkout main
   git pull origin main
   git checkout -b feat/your-feature-name
   ```

2. Make your changes, keeping commits focused and atomic.

3. Run quality checks before pushing:

   ```bash
   make fmt-check
   cargo test --workspace
   make lint
   ```

   (`make ci` bundles the fmt-check and lint hooks, but its `test` hook is currently empty, so always run `cargo test --workspace` yourself.)

4. Push your branch and open a pull request.

All changes are merged to `main` via **rebase merging** to maintain a linear commit history, so every commit must follow the [commit message convention](#commit-messages).

## Code Quality

- Format with rustfmt (`make fmt`); CI rejects unformatted code.
- Pass clippy with warnings as errors (`make lint`).
- Follow existing patterns and conventions in the codebase.
- Avoid refactoring alongside feature work; submit refactors as separate PRs.
- Prefer clear, descriptive names over cleverness. Comment the "why", not the "what".
- Don't introduce compiler warnings.

## Testing

- **Unit tests are required.** `cargo test --workspace` must pass before you submit; it needs no external services or API keys. Prefer unit tests for internal or algorithmic changes.
- **Integration tests are encouraged** for user-facing changes. They call real LLM APIs and need an `OPENAI_API_KEY` plus Docker; see [DEVELOPMENT.md](DEVELOPMENT.md#testing) for suites, feature flags, and commands.

## Commit Messages

This project uses [Conventional Commits](https://www.conventionalcommits.org/), enforced by CI (`make lint-commits`):

```
<type>(<optional scope>): <description>

[body]

[optional footer(s)]
```

- The subject line must be entirely lowercase, under 72 characters, and not end with a period.
- Use the body to explain **what** and **why**, not **how**.
- If the change relates to a tracked ticket, include a `Ref: LOG-XXXXXX` footer; otherwise omit it (commitlint does not require it). Use `Fixes: #<issue number>` when the change closes a GitHub issue.

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

Examples:

```
feat(config): add support for gemini provider

fix(streaming): correct sse event ordering on disconnect

doc: add ollama troubleshooting guide
```

For breaking changes, add `!` after the type/scope and include a `BREAKING CHANGE` footer:

```
feat(config)!: rename [tools] section to [mcp.tools]

BREAKING CHANGE: The [tools] config section has been renamed to [mcp.tools].
Update your config.toml files accordingly.
```

## Pull Requests

Before you submit:

1. Rebase on the latest `main` (`git fetch origin && git rebase origin/main`).
2. Run the quality checks above, plus `make lint-commits`.
3. Update documentation if your change affects user-facing behavior, configuration, or APIs (see [Updating Documentation](#updating-documentation)).

Guidelines:

- **Title**: clear and descriptive.
- **Description**: explain what the PR does and why: context, approach, trade-offs considered, and testing performed.
- **Size**: keep PRs focused and reasonably sized; break large changes into a series of smaller, reviewable PRs.
- **Draft PRs**: open one if you want early feedback on work in progress.

A suggested PR template:

```markdown
## What

Brief description of what this PR does.

## Why

Motivation and context for the change.

## How

Summary of the approach and any notable implementation details.

## Testing

How this was tested (unit tests, integration tests, manual testing).
```

After you submit, watch the PR for automated check results, the CLA bot's request to sign the contributor agreement, and reviewer feedback.

## Review Process

- All PRs require at least one maintainer review before merging.
- Reviewers may request changes; this is normal and part of maintaining quality. If you disagree with a suggestion, explain your reasoning.
- After approval, a maintainer rebase-merges your PR.

## Updating Documentation

If your change affects any of the following, update the relevant docs (see the documentation map in [CLAUDE.md](CLAUDE.md)):

- **Configuration options**: update `examples/reference.toml` and the relevant `examples/` files.
- **API behavior**: update `docs/streaming-api-guide.md` or `docs/request-lifecycle.md`.
- **New features**: add usage examples to the README or create a new guide in `docs/`.
- **Build, testing, or architecture changes**: update `DEVELOPMENT.md` and the relevant doc in `docs/`, and note any impacts in `CLAUDE.md`.

## Reporting Issues

When reporting a bug, please include:

- **AURA version** (check `Cargo.toml`) and **Rust toolchain version** (`rustc --version`)
- **Operating system and version**
- **Steps to reproduce**, plus **expected** vs. **actual** behavior
- **Relevant logs** (set `RUST_LOG=debug` or `RUST_LOG=aura=trace` for verbose output)
- **Configuration** (sanitized: remove API keys and secrets)

For feature requests, describe the use case and the problem you're trying to solve. Raising the idea in the [community Slack](https://mezmo.com/r/slack-aura) first is a great way to refine it before opening a formal issue.

## License

By contributing to AURA, you agree that your contributions will be licensed under the [Apache License, Version 2.0](LICENSE), the same license that covers the project. All contributions are also subject to the [Contributor License Agreement](CLA.md).

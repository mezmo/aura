<!-- markdownlint-disable MD033 -->
# Adopt branch-per-channel nightly / beta / stable promotion

- Status: **accepted**
- Deciders: Jacob Hull
- Date: 2026-07-29

Technical Story: release-cadence redesign — a nightly channel on `nightly`, iterated
betas, and promotion to a stable channel on `main`.

## Context and Problem Statement

Aura releases continuously off `main`: `release.config.js` pins
`branches: ['main']`, and the Jenkinsfile `Release` stage runs `semantic-release`
on every releasable merge, cutting a full stable version each time — a GitHub
release, a moved `latest` Docker tag, and a Homebrew tap bump. There is exactly one
channel, so every releasable merge a human lands is immediately what `latest` and
`brew upgrade` serve, with no intermediate validation.

The redesign introduces three channels at distinct risk levels:

- **nightly** — the bleeding edge, carried on `nightly`; every releasable merge is
  consumable as a prerelease but is not promoted to the stable channel.
- **beta** — a candidate that is cut from `nightly`, then iterated
  (`beta.1`, `beta.2`, …) as fixes land, until it is judged ready.
- **stable** — what `latest` and the Homebrew tap follow, carried on `main`; produced
  by promoting a blessed beta.

`semantic-release` represents release channels as configured branches: release
branches publish stable versions, while prerelease branches publish prerelease
versions. Reusing it therefore requires one branch per channel.

## Decision Drivers

- `main` MUST represent the **stable** channel — what `latest` and the Homebrew tap
  follow — keeping the conventional meaning of the default branch as production.
- Day-to-day development MUST land on a living edge (`nightly`) that is consumable as
  a prerelease without being promoted to stable.
- The flow MUST support cutting a candidate and iterating it until it is promoted,
  distinct from both nightly and stable.
- `semantic-release` SHOULD be reused end to end — version computation, changelog,
  GitHub releases, Docker tags, and the Homebrew bump — rather than reimplemented
  in pipeline scripts.
- Each channel publishes an immutable version-tagged image; `nightly` and `main`
  also move a channel pointer (`nightly`, `latest`). `latest` and the Homebrew tap
  advance on stable only.

## Considered Options

| Option | Outcome |
|---|---|
| Branch per channel | Chosen; satisfies all drivers using native `semantic-release` progression |
| Single mainline | Rejected; requires custom channel publication |
| `main` nightly, separate `stable` branch | Rejected; makes `latest`/Homebrew track a non-default branch |
| No release branch | Rejected; unsupported by `semantic-release` |

## Decision Outcome

Chosen option: **adopt a `semantic-release` branch-per-channel workflow** — one
branch per channel, separating day-to-day development (nightly), release
stabilization (beta), and production (stable). It replaces the current single-mainline,
release-on-every-merge model with an explicit promotion step: each channel is a
`semantic-release` branch, so promotion is a merge of the candidate into `main`,
after which `semantic-release` computes and publishes the stable release.

### Branch topology

| Branch | `semantic-release` role | Emits | Primary Docker alias | `latest` + Homebrew |
|---|---|---|---|---|
| `nightly` | prerelease `nightly` | `X.Y.Z-nightly.N` on every releasable merge | `nightly` | no |
| `beta` | prerelease `beta` | `X.Y.Z-beta.N` as the candidate iterates | — | no |
| `main` | release branch | bare `X.Y.Z` | `latest` | yes |

`main` is the release branch that `semantic-release` requires; `nightly` and `beta`
are prerelease branches whose versions derive from the last release on `main`.
`nightly` and `main` are permanent; `beta` is temporary — branched from the chosen
`nightly` commit at Cut and torn down after Sync, so at most one beta line is ever active.

The selected Cut commit must descend from the latest `main` release. Promotion and
synchronization preserve published history so `main` remains traceable to the
selected beta.

### Branch workflow

| Phase | Decision-level behavior |
|---|---|
| Cut | Create `beta` from an eligible `nightly` commit |
| Stabilize | Fix on `beta` and merge fixes back to `nightly` |
| Promote | Merge the blessed candidate into `main` |
| Sync | Merge `main` into `nightly` and delete `beta` |

Stable rebuilds from the promoted source; it does not reuse the beta artifacts.

### Positive Consequences

- `main` keeps its conventional meaning as the stable, production channel.
- `nightly` is the living edge where development lands, consumable as a prerelease.
- The candidate lives on its own branch (`beta`), iterated independently of nightly
  and stable.
- Promotion is a Git merge; `semantic-release` performs the stable version
  calculation and publication (changelog, version commit-back, GitHub release).
- `latest` and the Homebrew tap become stable-only pointers instead of tracking every
  releasable merge.

### Negative Consequences

- The single mainline becomes permanent `nightly` and `main` plus a per-cycle `beta`,
  introducing standing overhead: the `main` → `nightly` sync after every release, and
  the discipline of landing release fixes on `beta` rather than `nightly`.
- Candidate fixes must land on `beta` and be merged back to `nightly`; fixes made only on
  `nightly` are excluded from the active candidate. Branch conventions and protections
  are needed to steer fixes to `beta`.
- Nightly builds run on every releasable merge to `nightly`; multi-arch builds add CI
  cost, so nightly may warrant a lighter build.
- Promotion and sync must preserve published history — merge, never cherry-pick,
  squash, or rebase — so releases stay traceable to their beta.

## Links

- Supersedes the current wiring: `release.config.js`, `Jenkinsfile` (`Release`
  stage), `scripts/bump-homebrew-tap.sh`.
- Design and implementation note:
  [docs/design/release-channels.md](../design/release-channels.md) — the release
  configuration, per-channel release behavior, CI restructuring, branch workflow,
  validation, recovery, and operations.
- `semantic-release` branch configuration and prerelease workflow:
  <https://semantic-release.gitbook.io/semantic-release/usage/configuration#branches>

<!-- markdownlint-disable MD033 -->
# Release channels (branch-per-channel): design and implementation note

Companion to the ADR
[2026-07-29-release-channels](../adr/2026-07-29-release-channels.md). This document
specifies the release configuration, CI pipeline, branch workflow, validation,
recovery, and operational requirements.

**Status:** proposed as of 2026-07-29; not yet implemented.

## TL;DR

- Three `semantic-release` branches: permanent `main` (release → `X.Y.Z`) and
  `nightly` (prerelease `nightly`), plus a per-cycle `beta` (prerelease `beta`).
  `nightly` and `beta` derive their base version from the last release on `main`.
- One `release.config.js` selects **plugins and Docker tags by channel**, derived
  from `BRANCH_NAME`. `semantic-release` computes the version and publishes; no
  versioning logic is hand-rolled.
- Channel publishing differs: nightly's only external publication plugin is
  **Docker**; beta adds a **GitHub prerelease**; stable adds **changelog, version
  commit-back, GitHub release, and Homebrew**. `latest` and Homebrew move on stable
  only.
- Promotion merges `beta` into `main`; cross-branch changes are merged rather than
  cherry-picked so `main` retains the exact beta history.

## 1. Branch topology and channel mapping

| Branch | `semantic-release` role | `channel` | Emits | Moving Docker tags |
|---|---|---|---|---|
| `main` | release | default (no prerelease channel) | `X.Y.Z` | `latest`, version-family, `automated-security-scan` |
| `nightly` | prerelease `nightly` | `nightly` | `X.Y.Z-nightly.N` | `nightly` |
| `beta` | prerelease `beta` | `beta` | `X.Y.Z-beta.N` | — (immutable only) |

`nightly` and `main` are permanent and always on the remote; `beta` exists only while
a cycle is active (`semantic-release` ignores a configured prerelease branch that is
absent).

Branches may diverge. The workflow requires:

- the Cut commit to descend from the latest `main` tag (so a fresh `beta` inherits it);
- the promoted `beta` tip to become an ancestor of `main`;
- stabilization fixes to reach `nightly` before `beta` is deleted; and
- published history never to be rewritten.

Tags use the existing `v${version}` format (`v0.3.0`, `v0.3.0-beta.1`,
`v0.3.0-nightly.4`). Prerelease tags do not become stable release baselines. The
`beta` moving tag is intentionally omitted: a beta is validated as a specific
artifact, so consumers reference the immutable `X.Y.Z-beta.N`.

## 2. Release configuration

The current file `extends: '@mezmoinc/release-config-docker'` and pins
`branches: ['main']`. Under this design it **composes** the shared config
programmatically — `require` it, then spread and override `branches`, `plugins`, and
`dockerTags` — rather than relying on semantic-release's `extends`. Every invocation
(CI and local `release:dry`) runs with `BRANCH_NAME` set to the target branch.

```js
'use strict'
const base = require('@mezmoinc/release-config-docker')

const channelByBranch = { nightly: 'nightly', beta: 'beta', main: 'stable' }
const branch = process.env.BRANCH_NAME
const channel = channelByBranch[branch]
if (!channel) throw new Error(`unsupported release branch: ${branch}`)
if (process.env.RELEASE_CHANNEL && process.env.RELEASE_CHANNEL !== channel) {
  throw new Error(`RELEASE_CHANNEL=${process.env.RELEASE_CHANNEL} does not match ${branch}`)
}

const branches = [
  'main',
  { name: 'beta', prerelease: 'beta', channel: 'beta' },
  { name: 'nightly', prerelease: 'nightly', channel: 'nightly' },
]

const drop = {
  stable:  new Set(),
  beta:    new Set(['@semantic-release/changelog', '@semantic-release/git']),
  nightly: new Set(['@semantic-release/changelog', '@semantic-release/git',
                    '@semantic-release/github']),
}[channel]

const name = (p) => (Array.isArray(p) ? p[0] : p)
const ORDER = [
  '@semantic-release/commit-analyzer',
  '@semantic-release/release-notes-generator',
  '@semantic-release/changelog',
  '@semantic-release/exec',
  '@codedependant/semantic-release-docker',
  '@semantic-release/github',
  '@semantic-release/git',
]
const byName = new Map()
for (const p of base.plugins) {
  const n = name(p)
  if (byName.has(n)) throw new Error(`duplicate plugin instance: ${n}`)
  if (!drop.has(n) && !ORDER.includes(n)) throw new Error(`plugin not covered by ORDER: ${n}`)
  byName.set(n, p)
}

const preview = process.env.RELEASE_PREVIEW === '1'
const verifyCmds = ['scripts/verify-release-version.sh "${nextRelease.version}"']
if (channel === 'stable') {
  verifyCmds.push('scripts/bump-homebrew-tap.sh --dry-run "${nextRelease.version}"')
}
const verifyReleaseCmd = preview
  ? 'printf "%s\\n" "${nextRelease.version}" > .next-release-version'
  : verifyCmds.join(' && ')
const withExecVerify = (p) => {
  const [plugin, options = {}] = Array.isArray(p) ? p : [p, {}]
  return plugin === '@semantic-release/exec'
    ? [plugin, { ...options, verifyReleaseCmd }]
    : p
}

const plugins = ORDER
  .filter((n) => byName.has(n) && !drop.has(n))
  .map((n) => withExecVerify(byName.get(n)))

const dockerTags = {
  nightly: ['{{version}}', 'nightly'],
  beta:    ['{{version}}'],
  stable:  ['{{version}}', 'latest', '{{major}}-latest',
            '{{major}}.{{minor}}-latest', 'automated-security-scan'],
}[channel]

module.exports = { ...base, branches, plugins, dockerTags }
```

`base.releaseRules` (the `0.x.x` major→minor / minor→patch downgrade) and the
existing `@semantic-release/github` asset override carry over unchanged.

## 3. Per-channel release behavior

| Plugin | nightly | beta | stable | Purpose |
|---|:---:|:---:|:---:|---|
| `commit-analyzer` | ✅ | ✅ | ✅ | compute version |
| `release-notes-generator` | ✅ | ✅ | ✅ | notes body |
| `@semantic-release/exec` (`set-version.sh`) | ✅ | ✅ | ✅ | stamp `Cargo.*` for the build |
| `@codedependant/…/docker` | ✅ | ✅ | ✅ | prepare + publish image |
| `@semantic-release/github` | — | ✅ | ✅ | release/prerelease + assets |
| `@semantic-release/changelog` | — | — | ✅ | `CHANGELOG.md` |
| `@semantic-release/git` | — | — | ✅ | `[skip ci]` version commit-back |
| Homebrew (`exec` `successCmd`) | — | — | ✅ | tap bump |

- The `github` plugin auto-marks prereleases from the branch, so beta publishes a
  prerelease with no extra config.
- **Homebrew** shares the `@semantic-release/exec` instance used for version
  stamping, so its `successCmd` runs on every channel. `bump-homebrew-tap.sh` must
  exit for prerelease versions as its **first action — before any credential or
  network access** — so nightly and beta (which are not provisioned Homebrew
  credentials) pass. The Homebrew dry-run is gated to stable in the config (§2).
  Stable tap failures are recovered by rerunning that script directly (§8).

### Published Docker tags

Selected in JavaScript by channel (array in §2):

- nightly → immutable `X.Y.Z-nightly.N` + `nightly`
- beta → immutable `X.Y.Z-beta.N`
- stable → immutable `X.Y.Z` + `latest` + version-family (`{{major}}-latest`,
  `{{major}}.{{minor}}-latest`) + `automated-security-scan`

## 4. CI restructuring (Jenkinsfile)

Replace the single `Release` stage (gated on `main`, full release every merge) with a
per-branch release, each running only the stages its channel needs (`BRANCH_NAME`
selects the channel; `RELEASE_CHANNEL`, if exported, is validated against it — §2):

| Trigger | channel | Binaries (linux+darwin) | Docker | `npm run release` |
|---|---|:---:|:---:|:---:|
| merge to `nightly` | `nightly` | — | ✅ | ✅ (Docker only) |
| merge to `beta` | `beta` | ✅ | ✅ | ✅ (+ GitHub prerelease) |
| merge to `main` | `stable` | ✅ | ✅ | ✅ (full chain) |

- **Binaries:** `make build-binaries-linux` / `-darwin` + `make verify-binaries`
  (for `checksums.txt`, consumed by Homebrew) run for beta and stable only.
- **Build order and version equality:**
  1. Preview the version into `.next-release-version` (command below).
  2. Stamp and build the beta/stable binaries from that version.
  3. Start the real `semantic-release` run.
  4. `verifyRelease` checks `nextRelease.version` equals the preview, **before any
     `prepare` or publish side effect**.
  5. `prepareCmd` stamps the same version; the Docker plugin builds the image; publish.

  `scripts/verify-release-version.sh` performs the comparison via `verifyReleaseCmd`
  (on stable, composed ahead of the Homebrew dry-run); a mismatch aborts before side
  effects.
- **`prepare` order (stable):** `changelog` → `exec` (`set-version.sh`) → Docker
  build → `git` commit-back. Stable images do **not** ship `CHANGELOG.md` (exclude it
  from the Docker build context), so the changelog step only needs to precede the
  `git` commit-back.
- **`[skip ci]` handling:** only `main` commits back, so the existing "don't abort on
  the release branch" concurrency logic stays on `main`; `nightly` and `beta` no
  longer produce version commits.

Preview a branch's next version with:

```sh
rm -f .next-release-version
BRANCH_NAME=<branch> RELEASE_PREVIEW=1 npm run release:dry -- --no-ci
```

## 5. Branch workflow

In production, Cut / Stabilize / Promote / Sync run through protected branches and
reviewed PRs; `semantic-release` activates a branch only when its remote branch
exists, so Cut creates `beta` on the remote and Sync deletes it there.

| Phase | Operation | Result |
|---|---|---|
| Cut | Create remote `beta` from an eligible `nightly` commit | first beta |
| Stabilize | Merge fixes into `beta`; back-merge them to `nightly` | new betas; nightly keeps the fix |
| Promote | Merge the blessed `beta` commit into `main` | stable release |
| Sync | Merge `main` into `nightly`; delete `beta` | new stable baseline |

The Cut commit must descend from the latest `main` tag; do not re-merge
`nightly → beta` during a cycle (it would pull in features landed after the cut).

**Promotion invariants.** Channel branches are merged, not cherry-picked, so `main`
retains the exact candidate history; published history is never squashed or rebased.
Promotion freezes `beta` at the blessed commit. The merge into `main` must
produce the same source tree as the blessed beta; any conflict resolution or source
change returns to `beta` and requires a new beta. `beta` is deleted only after the beta
and stable release jobs complete. The `main → nightly` sync uses `--no-ff` so its
`[skip ci]` commit does not suppress CI. These properties are required; the
enforcement mechanism is implementation-specific.

## 6. Validation / spike plan

Before wiring CI, dry-run from each actual remote branch using the normal configured
branch list. Run in preview mode (§4):

```sh
git switch main
BRANCH_NAME=main    RELEASE_PREVIEW=1 npm run release:dry -- --no-ci   # base X.Y.Z
git switch nightly
BRANCH_NAME=nightly RELEASE_PREVIEW=1 npm run release:dry -- --no-ci   # X.Y.Z-nightly.N
git switch beta
BRANCH_NAME=beta    RELEASE_PREVIEW=1 npm run release:dry -- --no-ci   # X.Y.Z-beta.N
```

Validating `beta` needs a temporary remote `beta` branch. Accept when: each branch
yields the right version form, the config validates (no `ERELEASEBRANCHES` /
`EPRERELEASEBRANCHES`), and nightly and beta versions progress coherently from the same
stable base.

## 7. Rollout prerequisites and deferred enhancements

**Required before rollout:**

- **Migration.** `main` already carries the release line, so it stays the stable
  branch seeded from the current release (`0.1.x`). Create `nightly` from `main`,
  retarget day-to-day development there, and make the first Cut without disrupting the
  in-flight release.
- **Hotfix path.** Release the fix from `main`, then merge it into `nightly`. During an
  active beta cycle, also merge it into `beta` and validate a new beta before promotion.
- **Changelog scope.** Confirm stable-only `CHANGELOG.md` is acceptable given
  `CLAUDE.md` treats it as auto-generated.

**Deferred enhancements:**

- **Artifact provenance.** Stable artifacts are rebuilt from the promoted source
  rather than reused from the beta — an accepted limitation; exact artifact promotion
  (retag by digest) is deferred.
- **Nightly architecture set.** Restrict nightly to `linux/amd64` to cut CI cost,
  keeping multi-arch for beta/stable.

## 8. Failure recovery

Because Homebrew runs after the Git tag and other publications, a Homebrew failure may
leave an otherwise complete stable release. Recovery rules:

- Homebrew updates are idempotent; a failed tap bump is retried directly for the
  published version, not by rerunning semantic-release.
- Any other post-tag partial failure: repair the specific output; never blindly rerun
  the full release.
- Moving tags (`latest`, `nightly`) are never rolled backward during recovery.
- Operators must be able to determine which outputs succeeded (tag, image, GitHub
  release, tap).

## 9. Operational requirements

- **Branch lifecycle and protection.** Protect `nightly`, `main`, and the name `beta`
  through persistent repository rules. Only one `beta` may exist; its creation and
  promotion require authorized actors.
- **Job serialization.** Serialize each channel's release job so an older nightly
  build cannot move the `nightly` tag after a newer one.
- **Credential scope.** Every channel receives narrowly scoped repository credentials
  for pushing release tags and notes, plus Docker-registry credentials. Beta and stable
  additionally receive GitHub publication credentials; stable also receives Homebrew
  credentials.
- **Registry immutability.** Protect versioned image tags (`X.Y.Z`, `X.Y.Z-beta.N`,
  `X.Y.Z-nightly.N`) from overwrite.

## 10. Touch points (for implementers)

- `release.config.js` — branches, per-channel plugins, Docker tags (§2, §3).
- `Jenkinsfile` — per-branch release stages, `BRANCH_NAME`, binary/Docker gating,
  `prepare` order and version-equality check (§4).
- `package.json` — `release` / `release:dry` scripts, run with `BRANCH_NAME` set.
- `scripts/bump-homebrew-tap.sh` — exit for prerelease versions as its first action,
  before any credential/network use (§3).
- `scripts/verify-release-version.sh` (new) — `exec` `verifyReleaseCmd`; assert
  `nextRelease.version` equals `.next-release-version` (§4).
- `scripts/set-version.sh`, `.makefiles/rust.mk` (`build-binaries-*`,
  `verify-binaries`) — unchanged, reused per channel.
- `.dockerignore` — exclude `CHANGELOG.md` from the stable image build context (§4).
- Repository ruleset — persistent name-matched protection for `nightly`, `beta`,
  `main` (§9).

## 11. Related docs

- ADR: [2026-07-29-release-channels](../adr/2026-07-29-release-channels.md)
- `semantic-release` branches / prerelease workflow:
  <https://semantic-release.gitbook.io/semantic-release/usage/configuration#branches>

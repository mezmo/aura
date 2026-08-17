<!-- markdownlint-disable MD033 -->
# Release channels (branch-per-channel): design and implementation note

Companion to the ADR
[2026-07-29-release-channels](../adr/2026-07-29-release-channels.md). This document
specifies the release configuration, CI pipeline, branch workflow, validation,
recovery, and operational requirements.

**Status:** implemented as of 2026-08-17. The repository-side rollout steps in §7
(creating and protecting `nightly`, retargeting development, the first Cut) are
administrative and remain outstanding.

## TL;DR

- Three `semantic-release` branches: permanent `main` (release → `X.Y.Z`) and
  `nightly` (prerelease `nightly`), plus a per-cycle `beta` (prerelease `beta`).
  `nightly` and `beta` derive their base version from the last release on `main`.
- One `release.config.js` selects **plugins, exec commands, and Docker tags by
  channel**, derived from `BRANCH_NAME`. `semantic-release` computes the version and
  publishes; no versioning logic is hand-rolled.
- Channel publishing differs: nightly's only external publication plugin is
  **Docker**; beta adds a **GitHub prerelease**; stable adds **changelog, version
  commit-back, GitHub release, Homebrew, and Cloudsmith packages**. `latest`,
  Homebrew, and the package repository move on stable only.
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

`release.config.js` keeps `extends: '@mezmoinc/release-config-docker'` and overrides
`branches`, `plugins`, and the `exec` commands. Every invocation (CI and local
`release:dry`) runs with `BRANCH_NAME` set to the target branch. The `0.x.x`
major→minor / minor→patch `releaseRules` downgrade and the
`@semantic-release/github` asset override carry over unchanged.

Most of the channel machinery already exists and is not reimplemented:

- **Docker tags** need no per-channel list. The plugin drops a tag that renders
  empty, and `channel` is empty only on stable, so one static array covers all three
  rows of the table above. The shared config already uses this idiom to keep the
  version-family tags off beta; the guards widen from "not beta" to `{{#unless
  channel}}` so nightly is covered too.
- **Plugin order** is the shared config's, filtered by a per-channel `DROPPED` set.
  Its `remap` step already guarantees the docker plugin follows `exec`.
- **`beta` is a `semantic-release` default branch**; `nightly` is not, so all three
  are declared. Declaring them also keeps the other defaults (`master`, `next`,
  `next-major`, `alpha`, the maintenance glob) from going live if such a branch
  appears.

What is computed: the plugin drops and the `exec` commands, both keyed off
`BRANCH_NAME`.

- `semantic-release` fixes the plugin list in `getConfig`, before `getBranches`
  resolves the branch, so a per-channel plugin set cannot read `nextRelease.channel`
  and must come from the environment. Anything evaluated later — the Docker tags,
  the `exec` command templates — uses `semantic-release`'s own channel instead.
- An unrecognised branch keeps every plugin and the full command chain. That is what
  the change-request dry run wants: it rehearses stable from a feature branch. The
  `branches` list, not this lookup, is what decides whether a branch may release.
- The `exec` commands are **top-level** keys, not `exec` plugin options: the dry run
  names the plugin on the command line (`--plugins … @semantic-release/exec`), and
  such a plugin is given only the top-level keys.
- `bump-homebrew-tap.sh` exits on a prerelease version regardless (§3), so the tap is
  guarded at the point of action as well as by the branch lookup.

## 3. Per-channel release behavior

| Plugin | nightly | beta | stable | Purpose |
|---|:---:|:---:|:---:|---|
| `commit-analyzer` | ✅ | ✅ | ✅ | compute version |
| `release-notes-generator` | ✅ | ✅ | ✅ | notes body |
| `@semantic-release/npm` | ✅ | ✅ | ✅ | stamp `package.json` (`npmPublish: false`) |
| `@semantic-release/exec` (`set-version.sh`) | ✅ | ✅ | ✅ | stamp `Cargo.*` for the build |
| `@codedependant/…/docker` | ✅ | ✅ | ✅ | prepare + publish image |
| `@semantic-release/github` | — | ✅ | ✅ | release/prerelease + assets |
| `@semantic-release/changelog` | — | — | ✅ | `CHANGELOG.md` |
| `@semantic-release/git` | — | — | ✅ | `[skip ci]` version commit-back |
| Homebrew + Cloudsmith (`exec` `successCmd`) | — | — | ✅ | tap bump, `.deb`/`.rpm` publish |

- The `github` plugin auto-marks prereleases from the branch, so beta publishes a
  prerelease with no extra config.
- **Homebrew and the package repository serve the stable channel**, so their
  `verifyReleaseCmd` dry runs and their `successCmd` publishes are configured for
  stable alone. As defense in depth, `bump-homebrew-tap.sh` also exits for a
  prerelease version as its **first action — before any credential or network
  access**, so a prerelease version can never reach the tap. Stable tap failures are
  recovered by rerunning that script directly (§8).

### Published Docker tags

One templated list, resolved per channel (§2):

- nightly → immutable `X.Y.Z-nightly.N` + `nightly`
- beta → immutable `X.Y.Z-beta.N`
- stable → immutable `X.Y.Z` + `latest` + version-family (`{{major}}-latest`,
  `{{major}}.{{minor}}-latest`) + `automated-security-scan`

## 4. CI restructuring (Jenkinsfile)

The `Release` stage runs on any of the three release branches instead of `main`
alone, each running only the stages its channel needs (`BRANCH_NAME` selects the
channel):

| Trigger | channel | Binaries (linux+darwin) | Docker | `npm run release` |
|---|---|:---:|:---:|:---:|
| merge to `nightly` | `nightly` | — | ✅ | ✅ (Docker only) |
| merge to `beta` | `beta` | ✅ | ✅ | ✅ (+ GitHub prerelease) |
| merge to `main` | `stable` | ✅ | ✅ | ✅ (full chain) |

- **Binaries:** the binary, package, and checksum stages (`make build-binaries-linux`
  / `-darwin`, `make release-artifacts`) run for beta and stable only. Nightly
  publishes the image alone.
- **Build order:**
  1. `npm run release:version` (`scripts/next-version.mjs`) previews the version into
     `NEXT_RELEASE_VERSION`; an empty result skips the release.
  2. Stamp and build the beta/stable binaries from that version.
  3. Start the real `semantic-release` run, which recomputes the version itself.
  4. `prepareCmd` stamps the same version; the Docker plugin builds the image; publish.

  The artifacts built at step 2 carry the previewed version, so the recomputation at
  step 3 has to land on it. It does because both derive the version from the tags
  reachable from the branch under release (`git tag --merged`), over a checkout whose
  history is fixed for the build. A branch that advanced on the remote meanwhile
  fails semantic-release's up-to-date check, which skips the release rather than
  publishing a different version.
- **Channel branch refs.** `release:version` reads the workspace as a `file://`
  remote, and semantic-release keeps only the configured branches the repository
  actually has, so the release stage materializes a local ref for each channel branch
  first — otherwise a prerelease branch cannot derive its base version from `main`.
- **`prepare` order (stable):** `changelog` → `exec` (`set-version.sh`) → Docker
  build → `git` commit-back. Stable images do **not** ship `CHANGELOG.md` (excluded
  from the Docker build context), so the changelog step only needs to precede the
  `git` commit-back.
- **`[skip ci]` handling:** only `main` commits back, so the "don't abort on the
  release branch" concurrency logic covers all three release branches (an aborted
  nightly could strand its moving tag) while only `main` produces version commits.
- **Change-request dry run.** The dry run rehearses the *stable* chain — the widest
  set of commands — from a feature branch, which the config gives it by default (§2).
  It passes `--branches=<branch>` to make the branch under test releasable.

Preview a branch's next version with:

```sh
BRANCH_NAME=<branch> npm run --silent release:version
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
branch list — `release:dry` no longer overrides `--branches`, so the checked-out
branch is resolved against the configured channels:

```sh
git switch main
BRANCH_NAME=main    npm run release:dry
git switch nightly
BRANCH_NAME=nightly npm run release:dry
git switch beta
BRANCH_NAME=beta    npm run release:dry
```

Validating `beta` needs a temporary remote `beta` branch. Accept when: each branch
yields the right version form (`X.Y.Z`, `X.Y.Z-nightly.N`, `X.Y.Z-beta.N`) on the
right channel, the config validates (no `ERELEASEBRANCHES` / `EPRERELEASEBRANCHES`),
and nightly and beta versions progress coherently from the same stable base.

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
- **Prerelease package channel.** Nightly and beta publish no `.deb`/`.rpm`, since a
  prerelease in the stable Cloudsmith repository would reach `apt upgrade`. A separate
  prerelease repository would lift that restriction.

## 8. Failure recovery

Because Homebrew and the package publish run after the Git tag and other
publications, a failure there may leave an otherwise complete stable release.
Recovery rules:

- Homebrew updates are idempotent; a failed tap bump is retried directly for the
  published version, not by rerunning semantic-release.
- Any other post-tag partial failure: repair the specific output; never blindly rerun
  the full release.
- Moving tags (`latest`, `nightly`) are never rolled backward during recovery.
- Operators must be able to determine which outputs succeeded (tag, image, GitHub
  release, tap, packages).

## 9. Operational requirements

- **Branch lifecycle and protection.** Protect `nightly`, `main`, and the name `beta`
  through persistent repository rules. Only one `beta` may exist; its creation and
  promotion require authorized actors.
- **Job serialization.** Serialize each channel's release job so an older nightly
  build cannot move the `nightly` tag after a newer one. CI serializes rather than
  aborting on the release branches (§4).
- **Credential scope.** Every channel receives narrowly scoped repository credentials
  for pushing release tags and notes, plus Docker-registry credentials. Beta and stable
  additionally receive GitHub publication credentials; stable also receives Homebrew
  and package-repository credentials.
- **Registry immutability.** Protect versioned image tags (`X.Y.Z`, `X.Y.Z-beta.N`,
  `X.Y.Z-nightly.N`) from overwrite.

## 10. Touch points (for implementers)

- `release.config.js` — channel selection, branches, per-channel plugins, exec
  commands, and Docker tags (§2, §3).
- `Jenkinsfile` — per-branch release stages, `BRANCH_NAME`, binary/Docker gating,
  and channel branch refs (§4).
- `package.json` — `release` / `release:dry` / `release:version` scripts, run with
  `BRANCH_NAME` set.
- `scripts/next-version.mjs` — previews the version; analyses a channel branch
  against the full channel branch list so a prerelease computes a prerelease version.
- `scripts/bump-homebrew-tap.sh` — exits for prerelease versions as its first action,
  before any credential/network use (§3).
- `scripts/set-version.sh`, `.makefiles/rust.mk` (`build-binaries-*`,
  `release-artifacts`) — unchanged, reused per channel.
- `.makefiles/commitlint.mk` — lints a branch against its base (`nightly` by default,
  the change request's target branch in CI).
- `.dockerignore` — excludes `CHANGELOG.md` from the image build context (§4).
- Repository ruleset — persistent name-matched protection for `nightly`, `beta`,
  `main` (§9).

## 11. Related docs

- ADR: [2026-07-29-release-channels](../adr/2026-07-29-release-channels.md)
- `semantic-release` branches / prerelease workflow:
  <https://semantic-release.gitbook.io/semantic-release/usage/configuration#branches>

# scripts/

Release and install helpers.

| Script | Purpose |
| --- | --- |
| [`install.sh`](install.sh) | Install AURA from the Cloudsmith package repository or GitHub Releases |
| [`build-packages.sh`](build-packages.sh) | Build `.deb`/`.rpm` packages from the Linux release binaries |
| [`publish-packages.sh`](publish-packages.sh) | Publish the built `.deb`/`.rpm` packages to Cloudsmith |
| [`bump-homebrew-tap.sh`](bump-homebrew-tap.sh) | Bump `mezmo/homebrew-tap` formulae to a released version |
| [`set-version.sh`](set-version.sh) | Set the workspace and crate versions in `Cargo.toml` |
| [`next-version.mjs`](next-version.mjs) | Print the version semantic-release would release next |
| [`sync-release-downloads.sh`](sync-release-downloads.sh) | Snapshot cumulative release-asset download totals into PostHog |

`BRANCH_NAME` selects the release channel; see
[the release channels design note](../docs/design/release-channels.md).

## `install.sh`

```bash
curl -fsSL https://raw.githubusercontent.com/mezmo/aura/main/scripts/install.sh | bash
```

Installs `aura` for `linux`/`darwin` on `amd64`/`arm64`. One binary: the
interactive CLI, and the web server via `aura webserver`.

The script takes no command-line arguments. Every switch is an environment
variable, so it works unchanged when piped into `bash`:

```bash
curl -fsSL .../install.sh | AURA_COMPONENT=cli AURA_VERSION=0.1.3 bash
```

### Switches

| Variable | Default | Effect |
| --- | --- | --- |
| `AURA_VERSION` | `latest` | Version to install. A leading `v` is optional (`0.1.3` and `v0.1.3` both work). For `direct`, `latest` follows the `releases/latest` redirect; for `deb`/`rpm` it pins the package version, and `latest` lets the package manager pick. |
| `AURA_INSTALL_METHOD` | `auto` | How to install: `auto`, `homebrew`, `direct`, `deb`, or `rpm`. See below. Any other value is an error. |
| `AURA_INSTALL_PATH` | `~/.local/bin` | Install directory for the `direct` method. Created if missing. The `homebrew`, `deb`, and `rpm` methods install to their own prefixes and ignore it. |
| `AURA_COMPONENT` | `all` | Accepts `all`, `server`, or `cli`; all three install the `aura` binary. Any other value is an error. |
| `AURA_REQUIRE_CHECKSUM` | `1` | `direct` only. `0` downgrades a missing `checksums.txt`, or a missing entry for an asset, to a warning instead of a fatal error. A checksum *mismatch* is always fatal. |
| `AURA_CHECKSUMS` | unset | `direct` only. Path to a local `checksums.txt` to verify against, instead of downloading one from the release. |

### Install methods

`AURA_INSTALL_METHOD` selects how AURA is installed.

- `auto` (default) uses the first method whose requirements are met and whose
  options don't conflict, in order: a native `deb` then `rpm` package (Linux,
  matching package manager present, and able to become root without prompting —
  running as root or passwordless `sudo`), then `homebrew` (when `brew` is on
  `PATH`), then a `direct` binary download. When an option rules a method out
  (e.g. `AURA_INSTALL_PATH` with a package or Homebrew), `auto` notes it and
  moves on.
- `homebrew` installs from the `mezmo/tap` tap. It cannot pin `AURA_VERSION` or
  honor `AURA_INSTALL_PATH`.
- `direct` downloads the release binaries into `AURA_INSTALL_PATH` and verifies
  them against the release `checksums.txt`.
- `deb` / `rpm` register the [Cloudsmith](https://cloudsmith.com) package
  repository and install through the system package manager, escalating with
  `sudo` if not already root, so later upgrades arrive with `apt upgrade` /
  `dnf update`. Packages are verified by the repository's GPG signatures. Linux
  only; they cannot honor `AURA_INSTALL_PATH`.

Requesting an explicit method whose requirements are unmet (e.g. `deb` off
Linux, `homebrew` without `brew`) or that conflicts with a set option (e.g.
`homebrew` with `AURA_VERSION`, or `deb` with `AURA_INSTALL_PATH`) is an error —
only `auto` falls back.

### Repository layout

Packages are published to `any-distro/any-version`. The RPM side serves that
path directly; the Debian side indexes per distribution, so an apt source names
a concrete distro and codename.

### Requirements

- `curl` or `wget` for downloads
- `sha256sum`, `shasum`, or `openssl` for `direct` checksum verification. If
  none is installed, verification is skipped with a warning, or fails when
  `AURA_REQUIRE_CHECKSUM=1`.
- For `deb`: `apt-get`, plus root or `sudo`. `gpg` dearmors the signing key
  when present; otherwise the armored key is installed, which `apt` also
  accepts. `ca-certificates`, and `apt-transport-https` on apt older than 1.5,
  are installed first when missing.
- For `rpm`: `dnf`, `microdnf`, `yum`, or `zypper`, plus root or `sudo`.

## `publish-packages.sh`

```
publish-packages.sh [--dry-run]
```

Uploads every `.deb` and `.rpm` in `dist/` to a Cloudsmith repository with the
`cloudsmith` CLI, one package at a time. Uploads wait for server-side
synchronisation, so a package Cloudsmith rejects fails the script.

`--dry-run` sends the same push but uploads nothing. The server still
authenticates it, so it verifies the API key and the target repository.

| Variable | Default | Effect |
| --- | --- | --- |
| `DRY_RUN` | `0` | `1` is the same as `--dry-run`. |
| `CLOUDSMITH_API_KEY` | unset | API key with write access to the repository. Required. |
| `CLOUDSMITH_REPO` | `mezmo/aura` | `owner/repository` to publish to. |
| `CLOUDSMITH_DISTRO` | `any-distro/any-version` | Distribution/version coordinates the packages are filed under. |
| `DIST_DIR` | `dist` | Directory holding the packages. |
| `PACKAGERS` | `deb rpm` | Space-separated formats to publish. |
| `CLOUDSMITH` | `cloudsmith` | `cloudsmith` executable to use. |


## `sync-release-downloads.sh`

```
sync-release-downloads.sh [--dry-run] [--date YYYY-MM-DD] [--selftest]
```

Sends one PostHog event per GitHub release asset, carrying that asset's
cumulative download count as of a snapshot date. Run daily at 01:17 UTC by
[the `Release download metrics` workflow](../.github/workflows/release-download-metrics.yml),
which snapshots the previous UTC day.

The count is approximate for the day it names. GitHub publishes only a live
cumulative counter, so a run reads it at execution time and attributes it to
the previous UTC day — the 01:17 run folds that day's first 77 minutes into a
value labelled `23:59:59Z` the day before. The offset is the same on every
snapshot, so day-over-day differences still cover a true 24 hours. Naming an
older date does not reconstruct it: a retry days later stamps today's counters
with that date.

Retries are safe. The event UUID is derived from `(repository, asset ID,
snapshot date)` and the timestamp is pinned to `23:59:59Z` on the snapshot
date, so re-running a date re-sends byte-identical events that PostHog
deduplicates. Deduplication is eventual, so reporting should aggregate with
`max(download_count)` per asset and snapshot date — cumulative counts only
rise, which makes `max` correct while duplicates are still visible.

After sending, the script reads the snapshot back and fails if PostHog cannot
account for every event. This is not belt-and-braces: PostHog answers
`200 {"status":"Ok"}` to a batch sent with an invalid project token, so an
unverified send cannot tell success from silent discard. The read-back looks
for this run's own event UUIDs at this run's timestamp, rather than counting a
whole date: counting by date would also match UUIDs left by an earlier run
whose asset set differed, and those can cover for an event that never arrived.

A run also reports when the previous day holds no snapshot. That is advisory:
GitHub only exposes current cumulative counts, so a missed day cannot be
reconstructed by retrying.

Draft releases are skipped; their assets are not publicly downloadable.
GitHub-generated source archives are not release assets and never appear.

| Switch | Default | Effect |
| --- | --- | --- |
| `--date` / `SNAPSHOT_DATE` | yesterday, UTC | Date to snapshot. Re-running a past date re-sends that date's events. |
| `--dry-run` / `DRY_RUN=1` | off | Collect from GitHub and build the payload, print the first event, send nothing. Needs no PostHog token. |
| `--selftest` | off | Run the built-in assertions (UUID vectors, payload shape) and exit. Reaches no network. |
| `POSTHOG_PROJECT_API_KEY` | unset | PostHog project write token. Required unless `--dry-run`. |
| `POSTHOG_API_READ_KEY` | unset | Personal API key used to read the snapshot back. Required unless `--dry-run` or `SKIP_VERIFY=1`. |
| `POSTHOG_PROJECT_ID` | `443794` | Numeric project id the read-back queries. Must be the project the write token belongs to. |
| `POSTHOG_API_HOST` | `https://us.posthog.com` | PostHog query host. Distinct from the ingest host. |
| `VERIFY_TIMEOUT` | `300` | Seconds to wait for ingestion before failing the read-back. |
| `SKIP_VERIFY` | `0` | `1` sends without reading the snapshot back. |
| `POSTHOG_HOST` | `https://us.i.posthog.com` | PostHog ingest host. |
| `GITHUB_REPOS` | `mezmo/aura` | Space-separated `owner/repo` list to snapshot. |
| `BATCH_SIZE` | `1000` | Events per PostHog `/batch` request. |
| `GH_TOKEN` / `GITHUB_TOKEN` | unset | Token `gh` authenticates with. |

## `bump-homebrew-tap.sh`

```
bump-homebrew-tap.sh [--dry-run] <version>
```

Rewrites the version tag in each `url` and the matching `sha256` in every
`Formula/*.rb` of `mezmo/homebrew-tap`, and pushes to `main`.

A prerelease version (`0.2.0-beta.1`) exits 0 without doing anything, before
any token or network use — the tap follows stable only.

| Switch | Default | Effect |
| --- | --- | --- |
| `--dry-run` / `DRY_RUN=1` | off | Print the proposed commit and test the push with `git push --dry-run` without updating any refs. Tolerates a missing or incomplete checksums file, leaving any hash it cannot resolve untouched. |
| `CHECKSUMS_FILE` | `dist/checksums.txt` | Release checksums to source each `sha256` from. |
| `GH_TOKEN` / `GITHUB_TOKEN` | unset | Token used to clone and push the tap. Required unless `--dry-run`. |

Exits 0 without committing when the formulae already sit at the target version.

## `next-version.mjs`

```
npm run --silent release:version [repository-url]
```

Prints the version semantic-release would release next, or nothing when no
change is releasable. Loads only `commit-analyzer`, so no release lifecycle
command runs; semantic-release's logging goes to stderr, leaving stdout as the
version alone.

`BRANCH_NAME` selects the branch to analyse. A channel branch is analysed
against the whole channel branch list, so a prerelease derives its version from
the last release on `main` (`0.2.0-nightly.1`); any other branch on its own,
which is what makes a feature branch under test releasable.

## `set-version.sh`

```
set-version.sh <version>
```

Sets `version` in the workspace `Cargo.toml` and in each `crates/*/Cargo.toml`
that carries its own version, then runs `make update-lockfile`. Stands in for
`cargo set-version`, which does not build against this workspace's edition 2024
requirements.

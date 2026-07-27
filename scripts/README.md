# scripts/

Release and install helpers.

| Script | Purpose |
| --- | --- |
| [`install.sh`](install.sh) | Install AURA binaries from GitHub Releases |
| [`bump-homebrew-tap.sh`](bump-homebrew-tap.sh) | Bump `mezmo/homebrew-tap` formulae to a released version |
| [`set-version.sh`](set-version.sh) | Set the workspace and crate versions in `Cargo.toml` |

## `install.sh`

```bash
curl -fsSL https://raw.githubusercontent.com/mezmo/aura/main/scripts/install.sh | bash
```

Installs `aura` (CLI) and `aura-web-server` for `linux`/`darwin` on `amd64`/`arm64`.

The script takes no command-line arguments. Every switch is an environment
variable, so it works unchanged when piped into `bash`:

```bash
curl -fsSL .../install.sh | AURA_COMPONENT=cli AURA_VERSION=0.1.3 bash
```

### Switches

| Variable | Default | Effect |
| --- | --- | --- |
| `AURA_VERSION` | `latest` | Release tag to install. A leading `v` is optional (`0.1.3` and `v0.1.3` both work). `latest` is resolved by following the `releases/latest` redirect. |
| `AURA_INSTALL_METHOD` | `auto` | How to install: `auto`, `homebrew`, `direct`, `deb`, or `rpm`. See below. Any other value is an error. |
| `AURA_INSTALL_PATH` | `~/.local/bin` | Install directory for the `direct` method. Created if missing. The `homebrew`, `deb`, and `rpm` methods install to their own prefixes and ignore it. |
| `AURA_COMPONENT` | `all` | Which binaries to install: `all`, `server` (`aura-web-server` only), or `cli` (`aura` only). Any other value is an error. |
| `AURA_REQUIRE_CHECKSUM` | `1` | `0` downgrades a missing `checksums.txt`, or a missing entry for an asset, to a warning instead of a fatal error. A checksum *mismatch* is always fatal. |
| `AURA_CHECKSUMS` | unset | Path to a local `checksums.txt` to verify against, instead of downloading one from the release. |

### Install methods

`AURA_INSTALL_METHOD` selects how AURA is installed. Each `AURA_COMPONENT` maps
to its own tap formula and system package, so component-scoped installs work on
every method.

- `auto` (default) uses the first method whose requirements are met and whose
  options don't conflict, in order: a native `deb` then `rpm` package (Linux,
  matching package manager present, and able to become root without prompting —
  running as root or passwordless `sudo`), then `homebrew` (when `brew` is on
  `PATH`), then a `direct` binary download. When an option rules a method out
  (e.g. `AURA_INSTALL_PATH` with a package or Homebrew), `auto` notes it and
  moves on.
- `homebrew` installs from the `mezmo/tap` tap. It cannot pin `AURA_VERSION` or
  honor `AURA_INSTALL_PATH`.
- `direct` downloads the release binaries into `AURA_INSTALL_PATH`.
- `deb` / `rpm` download the release's system packages and install them to
  `/usr/bin`, escalating with `sudo` if not already root. Linux only; they
  cannot honor `AURA_INSTALL_PATH`.

Requesting an explicit method whose requirements are unmet (e.g. `deb` off
Linux, `homebrew` without `brew`) or that conflicts with a set option (e.g.
`homebrew` with `AURA_VERSION`, or `deb` with `AURA_INSTALL_PATH`) is an error —
only `auto` falls back.

### Requirements

- `curl` or `wget` for downloads
- `sha256sum`, `shasum`, or `openssl` for checksum verification. If none is
  installed, verification is skipped with a warning, or fails when
  `AURA_REQUIRE_CHECKSUM=1`.
- For `deb`/`rpm`: `dpkg` (deb) or `dnf`/`yum`/`rpm` (rpm), plus root or `sudo`.

## `bump-homebrew-tap.sh`

```
bump-homebrew-tap.sh [--dry-run] <version>
```

Rewrites the `version` and `sha256` fields in each `Formula/*.rb` of
`mezmo/homebrew-tap` and pushes to `main`.

| Switch | Default | Effect |
| --- | --- | --- |
| `--dry-run` / `DRY_RUN=1` | off | Print the proposed commit and test the push with `git push --dry-run` without updating any refs. Tolerates a missing checksums file, validating the version bump only. |
| `CHECKSUMS_FILE` | `dist/checksums.txt` | Release checksums to source each `sha256` from. |
| `GH_TOKEN` / `GITHUB_TOKEN` | unset | Token used to clone and push the tap. Required unless `--dry-run`. |

Exits 0 without committing when the formulae already sit at the target version.

## `set-version.sh`

```
set-version.sh <version>
```

Sets `version` in the workspace `Cargo.toml` and in each `crates/*/Cargo.toml`
that carries its own version, then runs `make update-lockfile`. Stands in for
`cargo set-version`, which does not build against this workspace's edition 2024
requirements.

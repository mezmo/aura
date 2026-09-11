#!/usr/bin/env bash
# Tests for check-release-baseline.sh, run against throwaway git repositories.
#
# Usage: check-release-baseline.test.sh

set -uo pipefail

SCRIPT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/check-release-baseline.sh"
workdir="$(mktemp -d)"
trap 'rm -rf "${workdir}"' EXIT

passed=0
failed=0

# A repository with main, one channel branch, and whatever tags a case needs.
# Commits are empty, so only the shape of the history matters.
new_repo() {
  local dir="${workdir}/$1"
  git init --quiet --initial-branch=main "${dir}"
  git -C "${dir}" -c user.email=t@t -c user.name=t commit --quiet --allow-empty -m "root"
  printf '%s' "${dir}"
}

commit() { git -C "$1" -c user.email=t@t -c user.name=t commit --quiet --allow-empty -m "$2"; }

expect() {
  local name="$1" dir="$2" want="$3"; shift 3
  local out; out="$(cd "${dir}" && "${SCRIPT}" "$@" 2>&1)"
  local got=$?
  if [ "${got}" = "${want}" ]; then
    printf '  ok    %s\n' "${name}"
    passed=$((passed + 1))
  else
    printf '  FAIL  %s (want exit %s, got %s)\n        %s\n' "${name}" "${want}" "${got}" "${out}"
    failed=$((failed + 1))
  fi
}

echo "check-release-baseline.sh"

# main releases v0.2.17; the channel branched before that tag.
r="$(new_repo stale)"
git -C "$r" branch nightly
commit "$r" "version bump"
git -C "$r" tag v0.2.17
git -C "$r" checkout --quiet nightly
commit "$r" "channel work"
expect "channel missing main's release tag" "$r" 10

# The same repository once the back-merge has landed.
git -C "$r" -c user.email=t@t -c user.name=t merge --quiet --no-ff --no-edit main
expect "channel that took the back-merge" "$r" 0

# A prerelease tag newer than the stable one must not become the baseline.
r="$(new_repo prerelease)"
git -C "$r" tag v0.2.17
git -C "$r" branch nightly
commit "$r" "next cycle"
git -C "$r" tag v0.3.0-beta.1
git -C "$r" checkout --quiet nightly
expect "prerelease tags are not the baseline" "$r" 0

# sort -V, not lexical: v0.2.10 is newer than v0.2.9.
r="$(new_repo numeric)"
git -C "$r" tag v0.2.9
commit "$r" "ten"
git -C "$r" tag v0.2.10
git -C "$r" branch -f nightly HEAD~1
git -C "$r" checkout --quiet nightly
expect "versions compare numerically" "$r" 10

# Tags that are not vX.Y.Z are ignored entirely.
r="$(new_repo othertags)"
git -C "$r" tag v0.2.17
git -C "$r" branch nightly
commit "$r" "later"
git -C "$r" tag nightly-build
git -C "$r" tag v0.4.0-rc.1
git -C "$r" checkout --quiet nightly
expect "non-release tags are ignored" "$r" 0

# Nothing to be behind.
r="$(new_repo untagged)"
git -C "$r" branch nightly
git -C "$r" checkout --quiet nightly
expect "no stable tag on main" "$r" 0

# Refs other than a local branch resolve rather than silently passing.
r="$(new_repo refs)"
git -C "$r" branch nightly
commit "$r" "version bump"
git -C "$r" tag v0.2.17
git -C "$r" update-ref refs/remotes/origin/main HEAD
sha="$(git -C "$r" rev-parse HEAD)"
git -C "$r" checkout --quiet nightly
expect "a remote-tracking ref" "$r" 10 origin/main
expect "a raw commit sha"      "$r" 10 "${sha}"

# A ref that does not resolve is an error, never a pass.
expect "unresolvable ref is not a pass" "$r" 1 refs/heads/nope

printf '\n%d passed, %d failed\n' "${passed}" "${failed}"
[ "${failed}" -eq 0 ]

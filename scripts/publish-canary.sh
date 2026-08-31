#!/usr/bin/env bash
# Publish a local host-platform build to the canary channel — ZERO CI minutes.
#
# Builds the current tree (this box's <arch>-<os> only) and uploads it as a
# -canary.N *prerelease* on this repo's Releases. The release is created directly
# with `gh`, so the all-platform release.yml never fires. Pull it back with
#   grove up --channel canary      (or GROVE_CHANNEL=canary / a canary-installed box)
#
# Usage:
#   scripts/publish-canary.sh                  # auto-bump N off the highest canary
#   scripts/publish-canary.sh 0.2.0-canary.1   # pin an explicit version
#
# Run from the repo root on `main` (NOT a .claude/worktrees checkout). Host-only:
# `grove up --channel canary` on a different OS/arch will 404 — for a full
# multi-platform release push a `v*` tag and let release.yml build the matrix.
set -euo pipefail

cd "$(CDPATH= cd "$(dirname "$0")/.." && pwd)" # repo root
repo="jgeschwendt/grove"

# 1. Resolve the version: explicit arg wins; else bump N on the highest existing
#    canary. Numeric-aware sort, matching install.sh + update::layout::version_key.
if [ "${1:-}" != "" ]; then
  vsn="${1#v}"
else
  latest=$(gh release list --repo "$repo" --limit 100 --json tagName -q '.[].tagName' |
    grep -E '^v.+-canary\.[0-9]+$' |
    sort -t. -k1,1V -k2,2V -k3,3V -k4,4n | tail -1 || true)
  [ -n "$latest" ] || {
    echo "publish-canary: no existing canary to bump — pass an explicit version" >&2
    exit 1
  }
  base="${latest%-canary.*}"        # v0.1.1
  n="${latest##*-canary.}"          # 1
  vsn="${base#v}-canary.$((n + 1))" # 0.1.1-canary.2
fi
tag="v$vsn"

if gh release view "$tag" --repo "$repo" >/dev/null 2>&1; then
  echo "publish-canary: $tag already exists on $repo" >&2
  exit 1
fi

# Host target = <arch>-<os>, matching release.sh / update::source::host_target().
arch=$(uname -m)
case "$arch" in arm64 | aarch64) arch=aarch64 ;; x86_64 | amd64) arch=x86_64 ;; esac
os=$(uname -s)
case "$os" in
Darwin) os=darwin ;;
Linux) os=linux ;;
*)
  echo "publish-canary: unsupported OS: $os" >&2
  exit 1
  ;;
esac
target="${arch}-${os}"

echo "publish-canary: $tag ($target) → $repo"

# Always revert the version stamp and drop the local dist, even if the build or the
# upload fails — a stamped-but-unpublished tree must never be left behind for the
# next commit to pick up.
cleanup() {
  git checkout -- Cargo.toml Cargo.lock 2>/dev/null || true
  rm -rf "dist/$vsn" dist/latest
}
trap cleanup EXIT

# 2. Stamp the version — [workspace.package] is the single source every crate reads,
#    and CARGO_PKG_VERSION is what the daemon reports to the self-update health gate.
#    The build below rewrites Cargo.lock to match; the trap reverts both.
sed -i.bak -E "s/^version = \"[^\"]+\"/version = \"$vsn\"/" Cargo.toml && rm -f Cargo.toml.bak

# 3. Build the bundle. release.sh fails loudly if the stamp didn't reach the binary
#    — a mislabelled bundle would break the health gate on every box that pulls it.
DIST="$PWD/dist" sh scripts/release.sh

# 4. Publish the host bundle + sidecar + bootstrap CLI as a prerelease.
gh release create "$tag" --repo "$repo" --prerelease --title "$tag" \
  --notes "grove $tag — local $target build (canary channel)" \
  "dist/$vsn/$target.tar.gz" \
  "dist/$vsn/$target.tar.gz.sha256" \
  "dist/$vsn/grove-$target"

echo "publish-canary: published $tag ($target)"
echo "publish-canary: pull it with  grove up --channel canary"

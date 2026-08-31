#!/bin/sh
# Hermetic smoke for the real installer: a clean HOME + GROVE_HOME, a local fixture
# "release" (no network), and scripts/install.sh itself — not a parallel copy of it,
# which is how v1 ended up shipping an installer no test ever ran.
#
# The whole install lifecycle, in order: clean install → swap up to a second
# version → roll back → uninstall. Everything mutates inside one mktemp directory a
# trap reclaims, and nothing touches the developer's ~/.grove.
#
# GROVE_BIND is pinned to a port nothing can hold, and that is not hygiene: every
# `grove up` bounces the server at GROVE_BIND, and on the default 127.0.0.1:7777
# that is the developer's OWN running daemon — the first run of this script stopped
# it. A refused connect reads Offline, which is both hermetic and the honest shape
# of a clean install: nothing running, so nothing to bounce or health-gate.
#
# Usage: sh test/install_smoke.sh   (builds the grove binary first)
#        mise run smoke             (the gate; CI runs the same task)
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd) # repo root
cd "$ROOT"

echo "smoke: building grove"
cargo build -p grove --quiet
GROVE_BIN="$ROOT/target/debug/grove"

# Compute the target the same way grove + install.sh do.
arch=$(uname -m)
os=$(uname -s)
case "$arch" in arm64 | aarch64) arch=aarch64 ;; x86_64 | amd64) arch=x86_64 ;; esac
case "$os" in Darwin) os=darwin ;; Linux) os=linux ;; esac
TARGET="${arch}-${os}"

# Port 1 is privileged and unbindable, so every probe is refused rather than racing
# whatever else on this machine holds an ephemeral port between a pick and a bind.
BIND="127.0.0.1:1"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
FIX="$WORK/release"
HOME_DIR="$WORK/home"
GH="$WORK/grovehome"
mkdir -p "$FIX" "$HOME_DIR"

# The one step of install.sh that is not contained by HOME/GROVE_HOME: its PATH-link
# search takes the first WRITABLE candidate, and the first candidate is a system-wide
# directory whose writability is a property of the host rather than of the redirected
# home. On any box where the invoking user can write it (a Homebrew prefix, most
# single-user Linux boxes, root in a container) this smoke would `ln -sf` over the
# operator's real `grove` link and then have uninstall.sh delete it. Pinned, not
# hoped for — and asserted on exactly, since accepting the host path as a pass is
# what ratified the escape.
LINK_DIR="$HOME_DIR/.local/bin"

# Stage one fixture version: the bundle tarball (bin/grove — the single binary), its
# checksum sidecar, the standalone bootstrap CLI, and the bootstrap's own sidecar —
# install.sh verifies that one before it runs the binary. Exactly what
# scripts/release.sh writes, and exactly what a GitHub release carries.
stage_version() {
  v="$1"
  mkdir -p "$FIX/$v"
  cp "$GROVE_BIN" "$FIX/$v/grove-$TARGET"
  if command -v sha256sum >/dev/null 2>&1; then
    (cd "$FIX/$v" && sha256sum "grove-$TARGET" | awk '{print $1}' >"grove-$TARGET.sha256")
  else
    (cd "$FIX/$v" && shasum -a 256 "grove-$TARGET" | awk '{print $1}' >"grove-$TARGET.sha256")
  fi
  s="$WORK/stage-$v"
  mkdir -p "$s/bin"
  cp "$GROVE_BIN" "$s/bin/grove"
  tar -czf "$FIX/$v/$TARGET.tar.gz" -C "$s" bin
  if command -v sha256sum >/dev/null 2>&1; then
    (cd "$FIX/$v" && sha256sum "$TARGET.tar.gz" | awk '{print $1}' >"$TARGET.tar.gz.sha256")
  else
    (cd "$FIX/$v" && shasum -a 256 "$TARGET.tar.gz" | awk '{print $1}' >"$TARGET.tar.gz.sha256")
  fi
}

fail() {
  echo "FAIL: $1" >&2
  exit 1
}

assert_current() {
  want="$1"
  got=$(readlink "$GH/current")
  [ "$got" = "versions/$want" ] || fail "current → $got, want versions/$want"
}

V1="0.1.0-smoke"
V2="0.2.0-smoke"
echo "smoke: staging fixture release ($TARGET) at $FIX"
stage_version "$V1"
printf '%s' "$V1" >"$FIX/latest"

echo "smoke: install.sh (clean machine, local release base)"
HOME="$HOME_DIR" GROVE_HOME="$GH" GROVE_BIND="$BIND" GROVE_INSTALL_BASE_URL="$FIX" \
  GROVE_LINK_DIR="$LINK_DIR" bash "$ROOT/scripts/install.sh"

echo "smoke: asserting clean install"
[ -L "$GH/current" ] || fail "$GH/current is not a symlink"
assert_current "$V1"
[ -x "$GH/current/bin/grove" ] || fail "current/bin/grove missing"
[ -L "$LINK_DIR/grove" ] || fail "grove not linked into $LINK_DIR"
"$GH/current/bin/grove" version >/dev/null || fail "installed grove does not run"
# A -smoke suffix is not a channel (no trailing .N), so the box follows stable.
[ "$(cat "$GH/channel")" = "stable" ] || fail "channel → $(cat "$GH/channel"), want stable"
# The gate answered on the way in, so nothing may be left owed on disk.
[ ! -e "$GH/pending" ] || fail "a settled install left a pending marker"

GROVE="$GH/current/bin/grove"
run_grove() {
  HOME="$HOME_DIR" GROVE_HOME="$GH" GROVE_BIND="$BIND" GROVE_INSTALL_BASE_URL="$FIX" "$GROVE" "$@"
}

echo "smoke: grove up --version $V2 (swap)"
stage_version "$V2"
run_grove up --version "$V2"
assert_current "$V2"
[ "$(readlink "$GH/previous")" = "versions/$V1" ] || fail "previous → $(readlink "$GH/previous"), want versions/$V1"
[ ! -e "$GH/pending" ] || fail "a settled update left a pending marker"

echo "smoke: grove up --rollback"
run_grove up --rollback
assert_current "$V1"

echo "smoke: uninstall.sh"
HOME="$HOME_DIR" GROVE_HOME="$GH" GROVE_BIND="$BIND" GROVE_LINK_DIR="$LINK_DIR" sh "$ROOT/scripts/uninstall.sh"
[ ! -e "$GH" ] || fail "uninstall left $GH behind"
[ ! -L "$LINK_DIR/grove" ] || fail "uninstall left the PATH symlink behind"

echo "smoke: PASS — install $V1, swap to $V2, rollback to $V1, uninstall"

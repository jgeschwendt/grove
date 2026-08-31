#!/bin/sh
# Assemble a grove release BUNDLE for THIS box's platform.
#
# v2 ships one executable, so the bundle is one file in a `bin/` directory —
# `grove serve` is the daemon, and the launcher contract is `current/bin/grove
# serve`. No embedded runtime, no second release to stage into.
#
# Under $DIST/<version>/:
#   <target>.tar.gz         the bundle: bin/grove
#   <target>.tar.gz.sha256  the checksum sidecar `grove up` verifies
#   grove-<target>          the standalone bootstrap CLI install.sh fetches
# and $DIST/latest names the version (what the flat LocalDir/BaseUrl sources read).
#
# Consumed by `grove up` (crates/grove/src/update/), test/install_smoke.sh, the
# release workflow, and scripts/publish-canary.sh.
#
# Shell completions and a man page are NOT in the bundle: clap can generate both,
# but they need `clap_complete`/`clap_mangen` in the shipped binary's dependency
# graph, and nothing installs them yet. Add them here when something does.
set -eu

cd "$(CDPATH= cd "$(dirname "$0")/.." && pwd)" # repo root
DIST="${DIST:-$PWD/dist}"

# Target = <arch>-<os>, matching update::source::host_target() and install.sh.
arch=$(uname -m)
case "$arch" in arm64 | aarch64) arch=aarch64 ;; x86_64 | amd64) arch=x86_64 ;; esac
os=$(uname -s)
case "$os" in
Darwin) os=darwin ;;
Linux) os=linux ;;
*)
  echo "release: unsupported OS: $os" >&2
  exit 1
  ;;
esac
TARGET="${arch}-${os}"

# Version: Cargo.toml's [workspace.package] version is the single source. Every
# crate takes `version.workspace = true`, and the binary bakes it in through
# CARGO_PKG_VERSION — the value the daemon reports on /api/health and the value the
# self-update gate compares against what it flipped to.
VERSION=$(grep -m1 '^version = ' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')

echo "release: building grove ($VERSION, $TARGET)"
cargo build --release -p grove

BIN="target/release/grove"

# Guard: the binary must actually REPORT the version we are about to name the
# bundle after. v1's analogue caught a stale `_build` whose app dir kept the old
# vsn; here it catches a `--release` build cargo decided was already fresh after a
# version bump it didn't notice, or a $DIST assembled from another checkout. Either
# way the shipped daemon would answer the health gate with the wrong version and
# every update of it would auto-roll-back — silently, and only on a real install.
REPORTED=$("$BIN" version)
[ "$REPORTED" = "grove $VERSION" ] || {
  echo "release: version stamp did not propagate — $BIN reports '$REPORTED', expected 'grove $VERSION'" >&2
  echo "release: stale build? re-run after 'cargo clean -p grove'." >&2
  exit 1
}

OUT="$DIST/$VERSION"
STAGE="$DIST/.stage-$VERSION-$TARGET"
rm -rf "$STAGE"
mkdir -p "$OUT" "$STAGE/bin"
cp "$BIN" "$STAGE/bin/grove"
chmod +x "$STAGE/bin/grove"

tar -czf "$OUT/$TARGET.tar.gz" -C "$STAGE" bin
rm -rf "$STAGE"

if command -v sha256sum >/dev/null 2>&1; then
  (cd "$OUT" && sha256sum "$TARGET.tar.gz" | awk '{print $1}' >"$TARGET.tar.gz.sha256")
else
  (cd "$OUT" && shasum -a 256 "$TARGET.tar.gz" | awk '{print $1}' >"$TARGET.tar.gz.sha256")
fi

cp "$BIN" "$OUT/grove-$TARGET"
chmod +x "$OUT/grove-$TARGET"

# The bootstrap gets a sidecar of its own. install.sh EXECUTES this asset, so it is
# the one that most needs a checksum — and it is the one that had none, leaving the
# tarball's checksum to be verified by an unverified binary.
if command -v sha256sum >/dev/null 2>&1; then
  (cd "$OUT" && sha256sum "grove-$TARGET" | awk '{print $1}' >"grove-$TARGET.sha256")
else
  (cd "$OUT" && shasum -a 256 "grove-$TARGET" | awk '{print $1}' >"grove-$TARGET.sha256")
fi
echo "$VERSION" >"$DIST/latest"

echo "release: bundle    → $OUT/$TARGET.tar.gz"
echo "release: bootstrap → $OUT/grove-$TARGET"
echo "release: channel   → $DIST/latest ($VERSION)"

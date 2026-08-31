#!/bin/sh
# grove uninstaller. Stops the server, removes the `grove` symlink from PATH, and
# deletes the data dir.
#
# Usage:
#   sh uninstall.sh
#   GROVE_HOME=/custom sh uninstall.sh
#
# Environment:
#   GROVE_HOME       data dir (default: ~/.grove)
#   GROVE_LINK_DIR   where install.sh put the PATH symlink; searched in addition to
#                    the two default locations.
set -eu

GROVE_HOME="${GROVE_HOME:-$HOME/.grove}"

# Stop a running server FIRST, through the installed binary: the data dir about to
# be deleted holds the pid file and the lock the daemon runs under, and a server
# left running over a removed home is one nothing can find to stop. Best-effort —
# an already-stopped server exits nonzero on some paths and must not abort the rest.
if [ -x "$GROVE_HOME/current/bin/grove" ]; then
  GROVE_HOME="$GROVE_HOME" "$GROVE_HOME/current/bin/grove" off >/dev/null 2>&1 || true
fi

# Remove the PATH symlink wherever install.sh may have placed it. Only unlink a
# symlink that actually points into GROVE_HOME — never a user's unrelated binary.
unlink_from() { # $1=dir
  link="$1/grove"
  if [ -L "$link" ]; then
    target=$(readlink "$link")
    case "$target" in
    "$GROVE_HOME"/*) rm -f "$link" && echo "grove: unlinked $link" ;;
    esac
  fi
}
unlink_from /usr/local/bin
unlink_from "$HOME/.local/bin"
if [ -n "${GROVE_LINK_DIR:-}" ]; then
  unlink_from "$GROVE_LINK_DIR"
fi

# Remove the data dir (versions, current/previous symlinks, manifest, worktrees).
rm -rf "$GROVE_HOME"
echo "grove: removed $GROVE_HOME"
echo "grove: done."

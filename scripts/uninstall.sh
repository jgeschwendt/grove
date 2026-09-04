#!/bin/sh
# grove uninstaller. Stops the server, removes the `grove` symlink from PATH, and
# deletes the install root. The workspace is kept unless you ask for it: the install
# is regenerable by a single `grove up`, the checkouts under $GROVE_HOME are not, and
# the two used to share a root — one `rm -rf` took every worktree with the binary.
#
# Usage:
#   sh uninstall.sh                     # remove the install, keep the workspace
#   sh uninstall.sh --purge             # …and delete $GROVE_HOME with it
#   GROVE_INSTALL=/opt/grove sh uninstall.sh
#
# Environment:
#   GROVE_INSTALL    install root — versions/, current, channel (default:
#                    $XDG_DATA_HOME/grove, i.e. ~/.local/share/grove)
#   GROVE_HOME       workspace dir — manifest.toml, code/ (default: ~/.grove).
#                    Removed only under --purge.
#   GROVE_LINK_DIR   where install.sh put the PATH symlink; searched in addition to
#                    the two default locations.
set -eu

GROVE_HOME="${GROVE_HOME:-$HOME/.grove}"
GROVE_INSTALL="${GROVE_INSTALL:-${XDG_DATA_HOME:-$HOME/.local/share}/grove}"

purge=""
for arg in "$@"; do
  case "$arg" in
  --purge) purge=1 ;;
  *)
    echo "grove: unknown argument: $arg" >&2
    echo "usage: sh uninstall.sh [--purge]" >&2
    exit 2
    ;;
  esac
done

# Stop a running server FIRST, through the installed binary: the roots about to be
# deleted hold the binary itself and — in the workspace — the pid file and the lock
# the daemon runs under, and a server left running over a removed install is one
# nothing can find to stop. Both roots are passed through, so the binary resolves the
# same pair this script did. Best-effort — an already-stopped server exits nonzero on
# some paths and must not abort the rest.
if [ -x "$GROVE_INSTALL/current/bin/grove" ]; then
  GROVE_HOME="$GROVE_HOME" GROVE_INSTALL="$GROVE_INSTALL" \
    "$GROVE_INSTALL/current/bin/grove" off >/dev/null 2>&1 || true
fi

# Remove the PATH symlink wherever install.sh may have placed it. Only unlink a
# symlink that actually points into GROVE_INSTALL — never a user's unrelated binary.
unlink_from() { # $1=dir
  link="$1/grove"
  if [ -L "$link" ]; then
    target=$(readlink "$link")
    case "$target" in
    "$GROVE_INSTALL"/*) rm -f "$link" && echo "grove: unlinked $link" ;;
    esac
  fi
}
unlink_from /usr/local/bin
unlink_from "$HOME/.local/bin"
if [ -n "${GROVE_LINK_DIR:-}" ]; then
  unlink_from "$GROVE_LINK_DIR"
fi

# The install root (versions, the current/previous symlinks, channel, update.lock) —
# always, and safely: every byte of it comes back from `grove up`.
rm -rf "$GROVE_INSTALL"
echo "grove: removed $GROVE_INSTALL"

# The workspace (manifest, worktrees, every checkout under code/) — only when asked.
if [ -n "$purge" ]; then
  rm -rf "$GROVE_HOME"
  echo "grove: purged workspace $GROVE_HOME"
else
  echo "grove: kept workspace $GROVE_HOME — 'sh uninstall.sh --purge' takes it too"
fi
echo "grove: done."

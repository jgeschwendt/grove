#!/bin/sh
# Render every Claude Design artboard under docs/figures/project/ to its PNG at 2×,
# the same pixels the canvas exports — agent-browser, headless. Run from anywhere: mise run figures.
# Each board renders twice: <name>.png, then <name>-dark.png from a temp copy whose light token
# colours darken.py swaps for their dark values (tokens: project/ds/gigantic/tokens.json).
set -e
# An isolated session: the default one is shared, and a concurrent user resizes it mid-shot.
export AGENT_BROWSER_SESSION="${AGENT_BROWSER_SESSION:-grove-figures}"
P=$(cd "$(dirname "$0")" && pwd)
S=$P/project
D=$(mktemp -d "${TMPDIR:-/tmp}/grove-figures.XXXXXX")
trap 'rm -rf "$D"' EXIT
cp "$S/support.js" "$D/"
render() { # <dir> <board .dc.html> <out.png> <viewport h>
  agent-browser set viewport 1600 "$4" 2 >/dev/null
  agent-browser open "file://$1/$2" >/dev/null
  agent-browser wait 1500 >/dev/null
  agent-browser screenshot "$P/$3" >/dev/null
  printf '%s %s\n' "$3" "$(file "$P/$3" | sed 's/.*PNG image data, //; s/,.*//')"
}
shoot() { # <board .dc.html> <name>
  h=$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["boards"][sys.argv[2]]["h"])' "$S/canvas.json" "$1")
  render "$S" "$1" "$2.png" "$h"
  cp "$S/$1" "$D/$1"
  python3 -I "$P/darken.py" "$S/ds/gigantic/tokens.json" "$D/$1"
  render "$D" "$1" "$2-dark.png" "$h"
}
shoot RootLayout.dc.html  root-layout
shoot EngineLanes.dc.html engine-lanes
agent-browser close >/dev/null

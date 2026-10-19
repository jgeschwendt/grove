"""Rewrite a board's light token colours to their dark values: darken.py <tokens.json> <board.dc.html>.

Maps every 6-digit hex (`#rrggbb` and data-URI `%23rrggbb`) whose value is a light colour token to
that token's dark value, in place. Shared light values resolve to the first token declaring them, so
#ffffff -> ui-bg (even as ink on the accent fill), #476349 -> ui-accent. Prints each hex left unmapped.
"""

import json
import re
import sys
from urllib.parse import quote

HEX = re.compile(r"(#|%23)([0-9a-fA-F]{6})(?![0-9a-fA-F])")
# Non-token hexes the boards use, mapped by hand to the nearest dark token. None today.
EXTRA = {}

tokens, board = sys.argv[1], sys.argv[2]
dark = {}
for t in json.load(open(tokens))["color"]["tokens"]:
    v = t["value"]
    if isinstance(v, dict) and re.fullmatch(r"#[0-9a-fA-F]{6}", v.get("light", "")):
        dark.setdefault(v["light"].lower(), v["dark"])
dark.update(EXTRA)

unmapped = set()


def sub(m):
    light = "#" + m.group(2).lower()
    if light not in dark:
        unmapped.add(light)
        return m.group(0)
    d = dark[light]
    return d if m.group(1) == "#" else quote(d, safe="")


src = open(board).read()
open(board, "w").write(HEX.sub(sub, src))
print(f"{board.rsplit('/', 1)[-1]}: unmapped {' '.join(sorted(unmapped)) or 'none'}")

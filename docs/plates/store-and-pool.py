#!/usr/bin/env python3
"""Visual plate for docs/plans/store-and-pool.md.

Run:  uv run --with opencv-python-headless --with numpy python store-and-pool.py
Out:  docs/plates/store-and-pool.png
"""

from __future__ import annotations

import os

import cv2
import numpy as np

# ── canvas ───────────────────────────────────────────────────────────────────
W, H = 2000, 1100
OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "store-and-pool.png")

# ── palette (BGR, muted) ─────────────────────────────────────────────────────
WHITE = (255, 255, 255)
PAPER = (250, 250, 248)
INK = (58, 56, 52)  # dark grey text
MUTE = (140, 138, 134)  # secondary text
HAIR = (228, 228, 224)  # hairlines
GREY = (150, 142, 136)  # grove-owned (the dotted entries)
BLUE = (168, 124, 74)  # checkouts
AMBER = (72, 138, 190)  # the bare repo
GREEN = (110, 158, 108)  # pool slots
PLUM = (160, 106, 136)  # migration line

SIMPLEX = cv2.FONT_HERSHEY_SIMPLEX
DUPLEX = cv2.FONT_HERSHEY_DUPLEX
AA = cv2.LINE_AA

ARROW, DOT, DASH = "→", "·", "—"
GLYPHS = (ARROW, DOT, DASH)


# ── primitives ───────────────────────────────────────────────────────────────
def tint(color, amount=0.90):
    """Blend a line colour toward white for a soft fill."""
    return tuple(int(c + (255 - c) * amount) for c in color)


def rounded_rect(img, p0, p1, r, color, thickness=-1):
    x0, y0 = p0
    x1, y1 = p1
    r = max(0, min(r, (x1 - x0) // 2, (y1 - y0) // 2))
    if thickness < 0:
        cv2.rectangle(img, (x0 + r, y0), (x1 - r, y1), color, -1, AA)
        cv2.rectangle(img, (x0, y0 + r), (x1, y1 - r), color, -1, AA)
        for cx, cy, a in (
            (x0 + r, y0 + r, 180),
            (x1 - r, y0 + r, 270),
            (x1 - r, y1 - r, 0),
            (x0 + r, y1 - r, 90),
        ):
            cv2.ellipse(img, (cx, cy), (r, r), a, 0, 90, color, -1, AA)
        return
    cv2.line(img, (x0 + r, y0), (x1 - r, y0), color, thickness, AA)
    cv2.line(img, (x0 + r, y1), (x1 - r, y1), color, thickness, AA)
    cv2.line(img, (x0, y0 + r), (x0, y1 - r), color, thickness, AA)
    cv2.line(img, (x1, y0 + r), (x1, y1 - r), color, thickness, AA)
    for cx, cy, a in (
        (x0 + r, y0 + r, 180),
        (x1 - r, y0 + r, 270),
        (x1 - r, y1 - r, 0),
        (x0 + r, y1 - r, 90),
    ):
        cv2.ellipse(img, (cx, cy), (r, r), a, 0, 90, color, thickness, AA)


def _tokens(text):
    out, buf = [], ""
    for ch in text:
        if ch in GLYPHS:
            if buf:
                out.append(("t", buf))
                buf = ""
            out.append(("g", ch))
        else:
            buf += ch
    if buf:
        out.append(("t", buf))
    return out


def _glyph_w(g, scale):
    if g == ARROW:
        return int(34 * scale) + 12
    if g == DASH:
        return int(34 * scale) + 10
    return int(12 * scale) + 12


def measure(text, font=SIMPLEX, scale=0.55, thickness=1):
    w = 0
    for kind, val in _tokens(text):
        w += (
            cv2.getTextSize(val, font, scale, thickness)[0][0]
            if kind == "t"
            else _glyph_w(val, scale)
        )
    return w


def text(img, s, org, scale=0.55, color=INK, font=SIMPLEX, thickness=1, align="left"):
    """Draw text; renders the unicode glyphs → · — as vector marks (Hershey has no glyph)."""
    x, y = org
    total = measure(s, font, scale, thickness)
    if align == "center":
        x -= total // 2
    elif align == "right":
        x -= total
    cap = cv2.getTextSize("A", font, scale, thickness)[0][1]
    cy = y - cap // 3
    for kind, val in _tokens(s):
        if kind == "t":
            cv2.putText(img, val, (x, y), font, scale, color, thickness, AA)
            x += cv2.getTextSize(val, font, scale, thickness)[0][0]
            continue
        gw = _glyph_w(val, scale)
        t = max(1, int(round(3.2 * scale)))
        if val == ARROW:
            cv2.arrowedLine(
                img, (x + 5, cy), (x + gw - 7, cy), color, t, AA, tipLength=0.42
            )
        elif val == DASH:
            cv2.line(img, (x + 5, cy), (x + gw - 5, cy), color, t, AA)
        else:
            cv2.circle(img, (x + gw // 2, cy), max(2, int(2.6 * scale) + 1), color, -1, AA)
        x += gw
    return total


def arrow(img, a, b, color, thickness=2, head_px=15):
    """Arrow whose head is a fixed pixel size, not a fraction of the segment."""
    length = max(1.0, float(np.hypot(b[0] - a[0], b[1] - a[1])))
    cv2.arrowedLine(img, a, b, color, thickness, AA, tipLength=head_px / length)


def elbow(img, pts, color, thickness=2, head=True):
    """Right-angle polyline; arrowhead on the final segment."""
    for i in range(len(pts) - 1):
        a, b = pts[i], pts[i + 1]
        last = i == len(pts) - 2
        if last and head:
            arrow(img, a, b, color, thickness)
        else:
            cv2.line(img, a, b, color, thickness, AA)


def row(img, x0, x1, y0, y1, label, color, tag):
    rounded_rect(img, (x0, y0), (x1, y1), 10, tint(color, 0.90), -1)
    rounded_rect(img, (x0, y0), (x1, y1), 10, color, 2)
    base = y0 + (y1 - y0) // 2 + 8
    text(img, label, (x0 + 22, base), 0.64, INK, SIMPLEX, 1)
    text(img, tag, (x1 - 20, base), 0.46, color, SIMPLEX, 1, align="right")


# ── canvas ───────────────────────────────────────────────────────────────────
img = np.full((H, W, 3), 255, dtype=np.uint8)

# title
text(img, "roots and code", (70, 74), 0.98, INK, DUPLEX, 1)
text(img, "grove " + ARROW + " v0.3.0", (1930, 74), 0.6, MUTE, SIMPLEX, 1, align="right")
cv2.line(img, (70, 96), (1930, 96), HAIR, 1, AA)

# panel shells
PL = (60, 120, 960, 740)
PR = (1040, 120, 1940, 740)
for x0, y0, x1, y1 in (PL, PR):
    rounded_rect(img, (x0, y0), (x1, y1), 16, PAPER, -1)
    rounded_rect(img, (x0, y0), (x1, y1), 16, HAIR, 2)

# ── LEFT · today (v0.2) ──────────────────────────────────────────────────────
text(img, "today (v0.2)", (100, 172), 0.72, INK, DUPLEX, 1)

bx0, bx1, by0, by1 = 130, 800, 210, 596
rounded_rect(img, (bx0, by0), (bx1, by1), 14, WHITE, -1)
rounded_rect(img, (bx0, by0), (bx1, by1), 14, (214, 214, 210), 2)
rounded_rect(img, (bx0, by0), (bx1, by0 + 52), 14, (243, 243, 240), -1)
cv2.rectangle(img, (bx0, by0 + 38), (bx1, by0 + 52), (243, 243, 240), -1, AA)
cv2.line(img, (bx0, by0 + 52), (bx1, by0 + 52), (222, 222, 218), 1, AA)
text(img, "code/<owner>/<repo>/", (bx0 + 22, by0 + 36), 0.62, INK, DUPLEX, 1)

rx0, rx1 = bx0 + 30, bx1 - 30
ry = by0 + 74
for label, color, tag in (
    (".bare", GREY, "grove-owned"),
    (".pool/slot-N", GREY, "grove-owned"),
    ("main/", BLUE, "checkout"),
    ("blog/", BLUE, "checkout"),
):
    row(img, rx0, rx1, ry, ry + 58, label, color, tag)
    ry += 70

text(
    img,
    "VS Code hides only .git " + DASH + " .bare and .pool show",
    (130, 646),
    0.54,
    MUTE,
    SIMPLEX,
    1,
)

# ── RIGHT · target (v0.3) ────────────────────────────────────────────────────
text(img, "target (v0.3)", (1080, 172), 0.72, INK, DUPLEX, 1)
text(img, "$GROVE_HOME", (1080, 226), 0.58, MUTE, DUPLEX, 1)
cv2.line(img, (1080, 240), (1080, 690), HAIR, 2, AA)

# code box (checkouts, and only checkouts)
kx0, kx1, ky0, ky1 = 1330, 1905, 216, 416
rounded_rect(img, (kx0, ky0), (kx1, ky1), 14, WHITE, -1)
rounded_rect(img, (kx0, ky0), (kx1, ky1), 14, (214, 214, 210), 2)
rounded_rect(img, (kx0, ky0), (kx1, ky0 + 50), 14, (243, 243, 240), -1)
cv2.rectangle(img, (kx0, ky0 + 36), (kx1, ky0 + 50), (243, 243, 240), -1, AA)
cv2.line(img, (kx0, ky0 + 50), (kx1, ky0 + 50), (222, 222, 218), 1, AA)
text(img, "code/<owner>/<repo>/", (kx0 + 22, ky0 + 34), 0.62, INK, DUPLEX, 1)

MAIN_Y, BLOG_Y = 300, 372
row(img, kx0 + 28, kx1 - 28, MAIN_Y - 28, MAIN_Y + 28, "main/", BLUE, "checkout")
row(img, kx0 + 28, kx1 - 28, BLOG_Y - 28, BLOG_Y + 28, "blog/", BLUE, "checkout")

# roots box (one directory per root — everything grove owns)
gx0, gx1, gy0, gy1 = 1240, 1780, 462, 718
rounded_rect(img, (gx0, gy0), (gx1, gy1), 14, WHITE, -1)
rounded_rect(img, (gx0, gy0), (gx1, gy1), 14, (214, 214, 210), 2)
rounded_rect(img, (gx0, gy0), (gx1, gy0 + 48), 14, (243, 243, 240), -1)
cv2.rectangle(img, (gx0, gy0 + 34), (gx1, gy0 + 48), (243, 243, 240), -1, AA)
cv2.line(img, (gx0, gy0 + 48), (gx1, gy0 + 48), (222, 222, 218), 1, AA)
text(img, "roots/<owner>/<repo>/", (gx0 + 22, gy0 + 33), 0.62, INK, DUPLEX, 1)
text(img, "everything grove owns for that root", (gx0 + 22, gy0 + 76), 0.5, MUTE, SIMPLEX, 1)

BARE_Y, POOL_Y, CLONE_Y = 568, 624, 680
for cy, label, color, tag in (
    (BARE_Y, "bare/", AMBER, "the bare repo"),
    (POOL_Y, "pool/slot-N", GREEN, "warm slots"),
    (CLONE_Y, "cloning", GREY, "in-flight marker"),
):
    row(img, gx0 + 28, gx1 - 28, cy - 24, cy + 24, label, color, tag)

# bare  →  each checkout
TRUNK = 1150
elbow(img, [(gx0, BARE_Y), (TRUNK, BARE_Y), (TRUNK, MAIN_Y), (kx0 - 4, MAIN_Y)], AMBER, 2)
elbow(img, [(TRUNK, BLOG_Y), (kx0 - 4, BLOG_Y)], AMBER, 2)
cv2.circle(img, (TRUNK, BLOG_Y), 4, AMBER, -1, AA)
text(img, "worktree pointer (absolute)", (1200, 444), 0.5, AMBER, SIMPLEX, 1)

# pool slot  →  the code dir
PROMO = 1840
elbow(img, [(gx1, POOL_Y), (PROMO, POOL_Y), (PROMO, ky1 + 6)], GREEN, 2)
text(img, "promote = git worktree move", (1820, 444), 0.5, GREEN, SIMPLEX, 1, align="right")

# ── migration line ───────────────────────────────────────────────────────────
cv2.line(img, (70, 790), (1930, 790), HAIR, 1, AA)
text(img, "migration", (70, 848), 0.6, INK, DUPLEX, 1)

LY = 902
cv2.line(img, (260, LY), (1850, LY), PLUM, 5, AA)
cv2.arrowedLine(img, (1800, LY), (1858, LY), PLUM, 5, AA, tipLength=0.55)

STATIONS = (
    (430, "grove up v0.3.0", ARROW + " reconcile refuses (legacy v2)"),
    (1055, "grove doctor --fix", ARROW + " roots/<slug>/{bare,pool}, git worktree repair"),
    (1680, "status re-derives", ARROW + " ready"),
)
for sx, top, bottom in STATIONS:
    cv2.circle(img, (sx, LY), 14, PLUM, -1, AA)
    cv2.circle(img, (sx, LY), 7, WHITE, -1, AA)
    text(img, top, (sx, LY + 56), 0.58, INK, DUPLEX, 1, align="center")
    text(img, bottom, (sx, LY + 88), 0.52, MUTE, SIMPLEX, 1, align="center")

# ── footer ───────────────────────────────────────────────────────────────────
cv2.line(img, (70, 1018), (1930, 1018), HAIR, 1, AA)
text(
    img,
    "Rule: a code dir holds checkouts and only checkouts; one root, one directory.",
    (70, 1058),
    0.56,
    INK,
    SIMPLEX,
    1,
)
text(img, "docs/plans/store-and-pool.md", (1930, 1058), 0.5, MUTE, SIMPLEX, 1, align="right")

cv2.imwrite(OUT, img, [cv2.IMWRITE_PNG_COMPRESSION, 9])

# ── verify ───────────────────────────────────────────────────────────────────
check = cv2.imread(OUT)
size = os.path.getsize(OUT)
print("wrote", OUT)
print("shape", check.shape)
print("bytes", size, "=", round(size / 1024, 1), "KiB", "| under 1 MB:", size < 1_000_000)

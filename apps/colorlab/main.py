#!/usr/bin/env python3
"""Read-only color panel: normalize a color and compute WCAG luminance + contrast ratios."""
import os
import sys

# Shared host-link plumbing (socket loop, token stamping, frame bound, the
# agent-tool id echo) from apps/_sdk — fs_read-granted. The path is resolved
# relative to THIS file (apps/<app>/main.py -> ../_sdk), so it works both when
# darwind launches the app (cwd = project root) and when the tests run from the
# app dir. Bytecode writes are disabled since apps/_sdk is read-only in the
# sandbox. Re-importing drain_lines/MAX_FRAME_BYTES/TOKEN keeps them resolvable
# off `main` for the framing/contract tests.
sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "_sdk"))
from harness import (  # noqa: E402 — must follow the sys.path insert above
    MAX_FRAME_BYTES,
    TOKEN,
    drain_lines,
    reply_result,
    run,
    send,
)


def _parse_color(color):
    """Return an (r, g, b) tuple of 0-255 ints, or None on any parse failure."""
    if not isinstance(color, str):
        return None
    s = color.strip()
    if not s:
        return None
    if s.startswith("#"):
        h = s[1:].strip()
        if len(h) == 3:
            h = h[0] * 2 + h[1] * 2 + h[2] * 2
        if len(h) != 6:
            return None
        try:
            r = int(h[0:2], 16)
            g = int(h[2:4], 16)
            b = int(h[4:6], 16)
        except ValueError:
            return None
        return (r, g, b)
    if "," in s:
        parts = s.split(",")
        if len(parts) != 3:
            return None
        vals = []
        for p in parts:
            p = p.strip()
            if p == "" or not (p.isdigit() or (p[0] in "+-" and p[1:].isdigit())):
                return None
            try:
                n = int(p)
            except ValueError:
                return None
            if n < 0 or n > 255:
                return None
            vals.append(n)
        return (vals[0], vals[1], vals[2])
    return None


def _rgb_to_hsl(r, g, b):
    """Return (h 0-360 int, s 0-100 int, l 0-100 int)."""
    rf, gf, bf = r / 255.0, g / 255.0, b / 255.0
    mx = max(rf, gf, bf)
    mn = min(rf, gf, bf)
    lightness = (mx + mn) / 2.0
    delta = mx - mn
    if delta == 0:
        h = 0.0
        s = 0.0
    else:
        s = delta / (1.0 - abs(2.0 * lightness - 1.0))
        if mx == rf:
            h = 60.0 * (((gf - bf) / delta) % 6.0)
        elif mx == gf:
            h = 60.0 * (((bf - rf) / delta) + 2.0)
        else:
            h = 60.0 * (((rf - gf) / delta) + 4.0)
    h = h % 360.0
    return (int(round(h)) % 360, int(round(s * 100)), int(round(lightness * 100)))


def _channel_lin(c8):
    """Linearize an sRGB channel per the WCAG relative-luminance formula."""
    cs = c8 / 255.0
    if cs <= 0.03928:
        return cs / 12.92
    return ((cs + 0.055) / 1.055) ** 2.4


def _relative_luminance(r, g, b):
    return 0.2126 * _channel_lin(r) + 0.7152 * _channel_lin(g) + 0.0722 * _channel_lin(b)


def _contrast(l1, l2):
    """WCAG contrast ratio: (lighter + 0.05) / (darker + 0.05)."""
    lighter = max(l1, l2)
    darker = min(l1, l2)
    return (lighter + 0.05) / (darker + 0.05)


def compute(payload):
    """PURE, offline, no I/O, never raises.

    Reads payload["color"] (a #rgb/#rrggbb hex or "r,g,b" string). On parse
    failure returns {"error": ...}. Else returns hex, rgb, hsl, WCAG relative
    luminance, and contrast ratios vs white/black with AA/AAA pass flags.
    """
    try:
        color = payload.get("color") if isinstance(payload, dict) else None
    except Exception:  # noqa: BLE001 — never raise on hostile input
        color = None
    rgb = _parse_color(color)
    if rgb is None:
        return {"error": "invalid color: expected #rgb, #rrggbb, or 'r,g,b' (0-255)"}
    r, g, b = rgb
    lum = _relative_luminance(r, g, b)
    lum_white = _relative_luminance(255, 255, 255)  # 1.0
    lum_black = _relative_luminance(0, 0, 0)  # 0.0
    contrast_white = _contrast(lum, lum_white)
    contrast_black = _contrast(lum, lum_black)
    # WCAG pass flags are derived from the UNROUNDED ratio — a true 4.4986 must
    # fail AA. Rounding to 2 decimals first would inflate it to 4.50 and report a
    # sub-threshold color as passing (and likewise 6.998 -> 7.00 for AAA). Round
    # only the reported ratios, below.
    aa_on_white = contrast_white >= 4.5
    aaa_on_white = contrast_white >= 7
    return {
        "hex": "#{:02x}{:02x}{:02x}".format(r, g, b),
        "rgb": [r, g, b],
        "hsl": list(_rgb_to_hsl(r, g, b)),
        "relative_luminance": round(lum, 4),
        "contrast_white": round(contrast_white, 2),
        "contrast_black": round(contrast_black, 2),
        "aa_on_white": aa_on_white,
        "aaa_on_white": aaa_on_white,
    }


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "colorlab.analyze", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "colorlab.analyze":
        reply_result(conn, msg, compute(msg))
    elif op == "stop":
        raise SystemExit(0)


if __name__ == "__main__":
    sys.exit(run(handle))

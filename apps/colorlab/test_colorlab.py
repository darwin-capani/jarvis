#!/usr/bin/env python3
"""Tests for colorlab.compute — WCAG color science, pure and offline."""
import json

from main import compute


def _approx(a, b, eps=1e-9):
    return abs(a - b) <= eps


def test_white_hex():
    r = compute({"color": "#ffffff"})
    assert "error" not in r, r
    assert r["hex"] == "#ffffff", r
    assert r["rgb"] == [255, 255, 255], r
    assert r["hsl"] == [0, 0, 100], r
    # White has relative luminance 1.0.
    assert _approx(r["relative_luminance"], 1.0), r
    # Contrast white-on-white = 1.0; white-on-black = 21.0.
    assert r["contrast_white"] == 1.0, r
    assert r["contrast_black"] == 21.0, r
    assert r["aa_on_white"] is False, r
    assert r["aaa_on_white"] is False, r


def test_black_hex():
    r = compute({"color": "#000000"})
    assert "error" not in r, r
    assert r["hex"] == "#000000", r
    assert r["rgb"] == [0, 0, 0], r
    assert r["hsl"] == [0, 0, 0], r
    assert _approx(r["relative_luminance"], 0.0), r
    # Black text on white background -> max contrast 21.0, passes AA and AAA.
    assert r["contrast_white"] == 21.0, r
    assert r["contrast_black"] == 1.0, r
    assert r["aa_on_white"] is True, r
    assert r["aaa_on_white"] is True, r


def test_short_hex_expands():
    # "#f00" expands to "#ff0000" (pure red).
    r = compute({"color": "#f00"})
    assert "error" not in r, r
    assert r["hex"] == "#ff0000", r
    assert r["rgb"] == [255, 0, 0], r
    assert r["hsl"][0] == 0, r  # hue red
    assert r["hsl"][1] == 100, r  # full saturation
    assert r["hsl"][2] == 50, r  # 50% lightness
    # Pure-red luminance per WCAG = 0.2126.
    assert _approx(r["relative_luminance"], 0.2126), r


def test_rgb_triplet_and_hue():
    # Pure green via comma triplet.
    r = compute({"color": "0,128,0"})
    assert "error" not in r, r
    assert r["hex"] == "#008000", r
    assert r["rgb"] == [0, 128, 0], r
    assert r["hsl"][0] == 120, r  # green hue
    # Contrast values are rounded to 2 decimal places.
    assert round(r["contrast_white"], 2) == r["contrast_white"], r
    assert round(r["contrast_black"], 2) == r["contrast_black"], r


def test_contrast_formula_gray():
    # Mid gray 119,119,119 (#777777): known WCAG contrast vs white ~4.48 (fails AA).
    r = compute({"color": "#777777"})
    assert "error" not in r, r
    assert r["aa_on_white"] is False, r
    # 128,128,128 (#808080) is lighter; contrast vs white should be lower still.
    r2 = compute({"color": "128,128,128"})
    assert r2["contrast_white"] < r["contrast_white"], (r, r2)


def test_wcag_flags_use_unrounded_contrast():
    # 0,126,183 has a true WCAG contrast vs white of 4.4986 — just below the 4.5
    # AA threshold. The flag must be False even though the DISPLAYED ratio rounds
    # to 4.5 (the flag must not be computed from the pre-rounded value).
    r = compute({"color": "0,126,183"})
    assert "error" not in r, r
    assert r["contrast_white"] == 4.5, r  # display value rounds up...
    assert r["aa_on_white"] is False, r   # ...but the flag uses the raw 4.4986
    # 0,60,248 has a true contrast of ~6.996 vs white — just below the 7.0 AAA
    # threshold; the displayed value rounds to 7.0 but AAA must still fail.
    r2 = compute({"color": "0,60,248"})
    assert "error" not in r2, r2
    assert r2["aaa_on_white"] is False, r2


def test_hostile_and_empty_inputs_never_raise():
    # None payload, missing key, wrong types, malformed strings, out-of-range —
    # every one must return an {"error": ...} dict and never raise.
    cases = [
        None,
        {},
        {"color": None},
        {"color": ""},
        {"color": "   "},
        {"color": 12345},
        {"color": [255, 0, 0]},
        {"color": "#12"},
        {"color": "#gggggg"},
        {"color": "#1234567"},
        {"color": "300,0,0"},
        {"color": "-1,0,0"},
        {"color": "1,2"},
        {"color": "1,2,3,4"},
        {"color": "a,b,c"},
        {"color": "1.5,2,3"},
        {"color": "not a color"},
        "totally-not-a-dict",
        42,
    ]
    for c in cases:
        out = compute(c)
        assert isinstance(out, dict), (c, out)
        assert "error" in out, (c, out)


# --- input-frame bounding (defense in depth) ---------------------------------
# main()'s socket read loop routes every recv() chunk through main.drain_lines,
# which DROPS a partial frame once it passes MAX_FRAME_BYTES with no newline, so a
# peer streaming bytes without a newline cannot grow the read buffer without bound
# (OOM). These assert that real helper — the daemon side is already bounded
# (apps.rs read_line_bounded / genproxy MAX_PROXY_LINE_BYTES).
import main as _frame_mod  # noqa: E402 — deliberately mid-file, after the app's own imports


def test_max_frame_bytes_is_8_mib():
    assert _frame_mod.MAX_FRAME_BYTES == 8 * 1024 * 1024


def test_oversized_frame_is_dropped_not_accumulated():
    # A newline-less frame past the cap is DISCARDED, not retained -> memory bounded.
    cap = _frame_mod.MAX_FRAME_BYTES
    lines, buf, overflowed = _frame_mod.drain_lines(b"x" * (cap + 1))
    assert overflowed is True
    assert buf == b""
    assert lines == []


def test_complete_lines_drain_and_partial_is_preserved():
    # Newline framing is intact: whole lines come out in order; a small partial stays.
    lines, buf, overflowed = _frame_mod.drain_lines(b'{"a":1}\n{"b":2}\n{"c":3')
    assert lines == [b'{"a":1}', b'{"b":2}']
    assert buf == b'{"c":3'
    assert overflowed is False


# -- the agent-tool request/response contract (SHARED shape; copy per app) ----
# A colorlab.analyze op carrying a request `id` is answered with a type:"result"
# line echoing that id; an op without one keeps the legacy uncorrelated
# type:"items" line (voice/refresh paths depend on the byte-identical wire).


class FakeConn:
    """Captures sendall payloads so handle() can be driven without a socket."""

    def __init__(self):
        self.lines = []

    def sendall(self, raw):
        self.lines.append(json.loads(raw.decode("utf-8").strip()))


def test_tool_op_with_id_answers_a_correlated_result():
    conn = FakeConn()
    _frame_mod.handle(conn, {"type": "colorlab.analyze", "id": "req-7", "color": "#ffffff"})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "result", reply
    assert reply["id"] == "req-7", "the request id is echoed verbatim"
    assert reply["data"]["hex"] == "#ffffff"
    assert reply["token"] == _frame_mod.TOKEN


def test_tool_op_without_id_keeps_the_legacy_items_line():
    conn = FakeConn()
    _frame_mod.handle(conn, {"type": "colorlab.analyze", "color": "#ffffff"})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "items", "no id -> uncorrelated legacy line"
    assert "id" not in reply
    assert reply["data"]["hex"] == "#ffffff"


def test_non_string_or_empty_id_is_treated_as_absent():
    for bad_id in (7, "", None, ["x"]):
        conn = FakeConn()
        _frame_mod.handle(conn, {"type": "colorlab.analyze", "id": bad_id, "color": "#ffffff"})
        assert conn.lines[0]["type"] == "items", f"id={bad_id!r} must not correlate"


if __name__ == "__main__":
    # Script-style runs exercise the framing tests too — they are plain
    # functions the runner below would otherwise never call.
    test_max_frame_bytes_is_8_mib()
    test_oversized_frame_is_dropped_not_accumulated()
    test_complete_lines_drain_and_partial_is_preserved()
    print("framing: 3 checks ok")
    test_tool_op_with_id_answers_a_correlated_result()
    test_tool_op_without_id_keeps_the_legacy_items_line()
    test_non_string_or_empty_id_is_treated_as_absent()
    print("agent-tool contract: 3 checks ok")
    test_white_hex()
    test_black_hex()
    test_short_hex_expands()
    test_rgb_triplet_and_hue()
    test_contrast_formula_gray()
    test_wcag_flags_use_unrounded_contrast()
    test_hostile_and_empty_inputs_never_raise()
    print("all tests passed")

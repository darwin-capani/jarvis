#!/usr/bin/env python3
"""Tests for colorlab.compute — WCAG color science, pure and offline."""
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


if __name__ == "__main__":
    test_white_hex()
    test_black_hex()
    test_short_hex_expands()
    test_rgb_triplet_and_hue()
    test_contrast_formula_gray()
    test_hostile_and_empty_inputs_never_raise()
    print("all tests passed")

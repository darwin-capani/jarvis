#!/usr/bin/env python3
"""Tests for diffscope.compute — pure unified line-diff. Exit 0 on pass."""
from main import compute


def test_basic_change():
    r = compute({"a": "line1\nline2\nline3", "b": "line1\nCHANGED\nline3"})
    assert "error" not in r, r
    # One line replaced -> one removed, one added.
    assert r["added"] == 1, r
    assert r["removed"] == 1, r
    # Header lines and the changed content should appear in the diff text.
    assert "-line2" in r["diff"], r
    assert "+CHANGED" in r["diff"], r
    # +++/--- header lines are excluded from the counts.
    assert "+++" in r["diff"] and "---" in r["diff"], r


def test_pure_additions():
    r = compute({"a": "a\nb", "b": "a\nb\nc\nd"})
    assert "error" not in r, r
    assert r["added"] == 2, r
    assert r["removed"] == 0, r


def test_identical_no_diff():
    r = compute({"a": "same\ntext", "b": "same\ntext"})
    assert "error" not in r, r
    assert r["diff"] == "", r
    assert r["added"] == 0 and r["removed"] == 0, r


def test_missing_fields_default_empty():
    # Missing both a and b -> both default to "" -> empty diff, no raise.
    r = compute({})
    assert "error" not in r, r
    assert r["diff"] == "" and r["added"] == 0 and r["removed"] == 0, r


def test_cap_200_lines():
    # Wholesale replacement of many lines forces a large diff; must cap at 200.
    a = "\n".join("old%d" % i for i in range(300))
    b = "\n".join("new%d" % i for i in range(300))
    r = compute({"a": a, "b": b})
    assert "error" not in r, r
    n = len(r["diff"].split("\n"))
    assert n <= 200, n


def test_content_lines_starting_with_plusplus_are_counted():
    # A content line whose data begins with "++"/"--" (C's "++i", a "--flag",
    # a YAML "---") is emitted by unified_diff as "+++i"/"---x". These must be
    # counted as real insertions/deletions, not skipped as file headers.
    r = compute({"a": "a\nb", "b": "a\n++i\nb"})
    assert "error" not in r, r
    assert r["added"] == 1, r
    assert r["removed"] == 0, r
    # Symmetric: a deleted line starting with "--".
    r2 = compute({"a": "a\n--flag\nb", "b": "a\nb"})
    assert "error" not in r2, r2
    assert r2["removed"] == 1, r2
    assert r2["added"] == 0, r2


def test_hostile_inputs_never_raise():
    # None payload.
    assert "error" in compute(None)
    # Non-dict payload.
    assert "error" in compute("not a dict")
    assert "error" in compute(42)
    # Container values are rejected, not crashed on.
    assert "error" in compute({"a": ["x"], "b": "y"})
    assert "error" in compute({"a": "x", "b": {"k": "v"}})
    # None values coerce to empty strings -> valid empty diff.
    r = compute({"a": None, "b": None})
    assert "error" not in r and r["diff"] == "", r
    # Non-string scalar coerces to str without raising.
    r2 = compute({"a": 1, "b": 2})
    assert "error" not in r2, r2


if __name__ == "__main__":
    test_basic_change()
    test_pure_additions()
    test_identical_no_diff()
    test_missing_fields_default_empty()
    test_cap_200_lines()
    test_content_lines_starting_with_plusplus_are_counted()
    test_hostile_inputs_never_raise()
    print("ok")

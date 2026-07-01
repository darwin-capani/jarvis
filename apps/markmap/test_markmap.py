#!/usr/bin/env python3
"""Tests for markmap.compute — ATX heading outline extraction."""
from main import compute


def test_basic_outline():
    md = "# Title\n\nsome text\n\n## Section A\ncontent\n### Sub A1\n## Section B\n"
    out = compute({"markdown": md})
    assert "error" not in out, out
    assert out["count"] == 4, out
    assert out["outline"] == [
        {"level": 1, "text": "Title"},
        {"level": 2, "text": "Section A"},
        {"level": 3, "text": "Sub A1"},
        {"level": 2, "text": "Section B"},
    ], out["outline"]


def test_ignores_fenced_code_and_non_headings():
    md = (
        "# Real\n"
        "```\n"
        "# not a heading, inside fence\n"
        "## also ignored\n"
        "```\n"
        "#nospace is not a heading\n"
        "####### seven hashes is not a heading\n"
        "## Closed heading ##\n"
    )
    out = compute({"markdown": md})
    assert "error" not in out, out
    assert out["count"] == 2, out
    assert out["outline"] == [
        {"level": 1, "text": "Real"},
        {"level": 2, "text": "Closed heading"},
    ], out["outline"]


def test_cap_at_50():
    md = "\n".join("# H%d" % i for i in range(120))
    out = compute({"markdown": md})
    assert "error" not in out, out
    assert out["count"] == 120, out
    assert len(out["outline"]) == 50, len(out["outline"])
    assert out["outline"][0] == {"level": 1, "text": "H0"}, out["outline"][0]
    assert out["outline"][49] == {"level": 1, "text": "H49"}, out["outline"][49]


def test_hostile_and_empty_inputs_do_not_raise():
    # Empty string -> empty outline, no error.
    empty = compute({"markdown": ""})
    assert empty == {"outline": [], "count": 0}, empty
    # Missing key -> treated as empty.
    assert compute({}) == {"outline": [], "count": 0}
    # Wrong payload type -> error dict, no raise.
    assert "error" in compute(None)
    assert "error" in compute([1, 2, 3])
    assert "error" in compute("just a string")
    # Wrong markdown type (list) -> error dict, no raise.
    assert "error" in compute({"markdown": ["a", "b"]})
    # Bool markdown -> error dict, no raise.
    assert "error" in compute({"markdown": True})
    # Unterminated fence must not raise and must swallow trailing lines.
    unterminated = compute({"markdown": "# Keep\n```\n# swallowed\n"})
    assert unterminated == {"outline": [{"level": 1, "text": "Keep"}], "count": 1}, unterminated
    # CRLF line endings handled.
    crlf = compute({"markdown": "# A\r\n## B\r\n"})
    assert crlf["count"] == 2, crlf


def test_heading_of_only_hashes():
    # "## ##" -> text becomes empty string, still a level-2 heading, no raise.
    out = compute({"markdown": "## ##\n"})
    assert out["count"] == 1, out
    assert out["outline"] == [{"level": 2, "text": ""}], out["outline"]


if __name__ == "__main__":
    test_basic_outline()
    test_ignores_fenced_code_and_non_headings()
    test_cap_at_50()
    test_hostile_and_empty_inputs_do_not_raise()
    test_heading_of_only_hashes()
    print("all tests passed")

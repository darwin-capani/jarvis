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


if __name__ == "__main__":
    # Script-style runs exercise the framing tests too — they are plain
    # functions the runner below would otherwise never call.
    test_max_frame_bytes_is_8_mib()
    test_oversized_frame_is_dropped_not_accumulated()
    test_complete_lines_drain_and_partial_is_preserved()
    print("framing: 3 checks ok")
    test_basic_outline()
    test_ignores_fenced_code_and_non_headings()
    test_cap_at_50()
    test_hostile_and_empty_inputs_do_not_raise()
    test_heading_of_only_hashes()
    print("all tests passed")

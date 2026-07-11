#!/usr/bin/env python3
"""Unit tests for codeglass.compute — pure, offline code-metrics."""
import unittest

from main import compute


class TestCompute(unittest.TestCase):
    def test_mixed_snippet(self):
        code = (
            "# header comment\n"
            "\n"
            "def f(x):\n"
            "    return x + 1  # inline is code, not a comment line\n"
            "// c-style comment\n"
            "/* block open\n"
            " * continuation star line\n"
        )
        r = compute({"code": code})
        # 7 physical lines.
        self.assertEqual(r["lines"], 7)
        # One truly blank line.
        self.assertEqual(r["blank_lines"], 1)
        # Comment lines: '# header', '// c-style', '/* block', ' * continuation'.
        self.assertEqual(r["comment_lines"], 4)
        # code = 7 - 1 - 4 = 2 ('def f(x):' and the return line).
        self.assertEqual(r["code_lines"], 2)
        # Longest is the return line.
        self.assertEqual(
            r["longest_line_len"],
            len("    return x + 1  # inline is code, not a comment line"),
        )
        # No TODO/FIXME/XXX markers present.
        self.assertEqual(r["todo_count"], 0)

    def test_todo_density(self):
        code = "TODO: wire it up\nx = 1\n# FIXME later\nok = XXX\nplain\n"
        r = compute({"code": code})
        self.assertEqual(r["lines"], 5)
        # Three lines carry a marker (TODO, FIXME, XXX).
        self.assertEqual(r["todo_count"], 3)
        # '# FIXME later' is the only comment line.
        self.assertEqual(r["comment_lines"], 1)
        self.assertEqual(r["blank_lines"], 0)
        self.assertEqual(r["code_lines"], 4)

    def test_empty_string(self):
        r = compute({"code": ""})
        self.assertEqual(
            r,
            {
                "lines": 0,
                "blank_lines": 0,
                "comment_lines": 0,
                "code_lines": 0,
                "longest_line_len": 0,
                "todo_count": 0,
            },
        )

    def test_hostile_and_missing_input_never_raises(self):
        # None payload, missing key, and non-string 'code' must all be safe.
        for bad in (None, {}, {"code": None}, {"code": 123}, {"code": ["a", "b"]}, "notadict", 42, []):
            r = compute(bad)
            self.assertEqual(r["lines"], 0)
            self.assertEqual(r["code_lines"], 0)
            self.assertEqual(r["todo_count"], 0)
            self.assertEqual(r["longest_line_len"], 0)

    def test_whitespace_only_lines_are_blank(self):
        code = "   \n\t\n  code_here = 1\n"
        r = compute({"code": code})
        self.assertEqual(r["lines"], 3)
        self.assertEqual(r["blank_lines"], 2)
        self.assertEqual(r["comment_lines"], 0)
        self.assertEqual(r["code_lines"], 1)


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
    unittest.main()

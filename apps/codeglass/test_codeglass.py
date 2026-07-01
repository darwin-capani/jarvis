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


if __name__ == "__main__":
    unittest.main()

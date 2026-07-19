#!/usr/bin/env python3
"""Unit tests for codeglass.compute — pure, offline code-metrics."""
import json
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


# -- the agent-tool request/response contract (SHARED shape; copy per app) ----
# codeglass.metrics is offered to the agent loop as an app__ tool. A request
# carrying a string `id` is answered with a type:"result" line echoing that id;
# a request without one keeps the legacy uncorrelated type:"items" line.


class FakeConn:
    """Captures sendall payloads so handle() can be driven without a socket."""

    def __init__(self):
        self.lines = []

    def sendall(self, raw):
        self.lines.append(json.loads(raw.decode("utf-8").strip()))


def test_tool_op_with_id_answers_a_correlated_result():
    conn = FakeConn()
    _frame_mod.handle(conn, {"type": "codeglass.metrics", "id": "req-7", "code": "x = 1\n"})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "result", reply
    assert reply["id"] == "req-7", "the request id is echoed verbatim"
    assert reply["data"]["lines"] == 1
    assert reply["token"] == _frame_mod.TOKEN


def test_tool_op_without_id_keeps_the_legacy_items_line():
    conn = FakeConn()
    _frame_mod.handle(conn, {"type": "codeglass.metrics", "code": "x = 1\n"})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "items", "no id -> uncorrelated legacy line"
    assert "id" not in reply
    assert reply["data"]["lines"] == 1


def test_non_string_or_empty_id_is_treated_as_absent():
    for bad_id in (7, "", None, ["x"]):
        conn = FakeConn()
        _frame_mod.handle(conn, {"type": "codeglass.metrics", "id": bad_id, "code": "x = 1\n"})
        assert conn.lines[0]["type"] == "items", f"id={bad_id!r} must not correlate"


if __name__ == "__main__":
    # Script-style runs exercise the framing + contract tests too — they are
    # plain functions the runner below (unittest.main) would otherwise never call.
    test_max_frame_bytes_is_8_mib()
    test_oversized_frame_is_dropped_not_accumulated()
    test_complete_lines_drain_and_partial_is_preserved()
    print("framing: 3 checks ok")
    test_tool_op_with_id_answers_a_correlated_result()
    test_tool_op_without_id_keeps_the_legacy_items_line()
    test_non_string_or_empty_id_is_treated_as_absent()
    print("contract: 3 checks ok")
    unittest.main()

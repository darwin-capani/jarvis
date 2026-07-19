#!/usr/bin/env python3
"""Tests for datalint.compute — pure JSON inspector. Run: python3 test_datalint.py"""
import json
import unittest

from main import compute


class TestCompute(unittest.TestCase):
    def test_valid_object(self):
        r = compute({"json": '{"a": 1, "b": {"c": 2, "d": [3, 4]}}'})
        self.assertTrue(r["valid"])
        self.assertEqual(r["root_type"], "object")
        self.assertEqual(r["top_level_keys"], 2)
        # nodes: root, a=1, b={}, c=2, d=[], 3, 4 => 7
        self.assertEqual(r["total_nodes"], 7)
        # root(1) -> b(2) -> d(3) -> 3/4(4)
        self.assertEqual(r["max_depth"], 4)

    def test_valid_array(self):
        r = compute({"json": "[1, 2, 3]"})
        self.assertTrue(r["valid"])
        self.assertEqual(r["root_type"], "array")
        self.assertEqual(r["top_level_keys"], 3)
        self.assertEqual(r["total_nodes"], 4)  # root list + 3 ints
        self.assertEqual(r["max_depth"], 2)

    def test_scalar_types(self):
        # bool must NOT be reported as number (bool subclasses int in Python)
        rb = compute({"json": "true"})
        self.assertEqual(rb["root_type"], "bool")
        self.assertEqual(rb["top_level_keys"], 0)
        self.assertEqual(rb["total_nodes"], 1)
        self.assertEqual(rb["max_depth"], 1)

        rn = compute({"json": "42"})
        self.assertEqual(rn["root_type"], "number")
        self.assertEqual(rn["total_nodes"], 1)

        rnull = compute({"json": "null"})
        self.assertEqual(rnull["root_type"], "null")

        rs = compute({"json": '"hello"'})
        self.assertEqual(rs["root_type"], "string")

    def test_invalid_json_reports_error(self):
        r = compute({"json": "{not valid}"})
        self.assertFalse(r["valid"])
        self.assertIn("error", r)
        self.assertIsInstance(r["error"], str)

    def test_hostile_and_empty_inputs_do_not_raise(self):
        # empty string -> invalid, but must not raise
        self.assertFalse(compute({"json": ""})["valid"])
        # missing key
        self.assertFalse(compute({})["valid"])
        # non-string json field -> coerced to "" -> invalid
        self.assertFalse(compute({"json": 12345})["valid"])
        self.assertFalse(compute({"json": None})["valid"])
        self.assertFalse(compute({"json": [1, 2]})["valid"])
        # payload not a dict at all
        self.assertFalse(compute(None)["valid"])
        self.assertFalse(compute("nope")["valid"])
        self.assertFalse(compute(42)["valid"])

    def test_deeply_nested_traversal(self):
        # 500 levels deep parses fine; verifies the explicit-stack traversal
        # (not Python recursion) walks every level without crashing.
        depth = 500
        s = "[" * depth + "]" * depth
        r = compute({"json": s})
        self.assertTrue(r["valid"])
        self.assertEqual(r["root_type"], "array")
        self.assertEqual(r["max_depth"], depth)
        self.assertEqual(r["total_nodes"], depth)

    def test_over_deep_input_returns_clean_error(self):
        # Input too deep for json.loads' own scanner: compute must NOT raise,
        # it returns a well-formed {valid: False, error: str} dict.
        s = "[" * 20000 + "]" * 20000
        r = compute({"json": s})
        self.assertFalse(r["valid"])
        self.assertIn("error", r)
        self.assertIsInstance(r["error"], str)

    def test_empty_containers(self):
        ro = compute({"json": "{}"})
        self.assertEqual(ro["root_type"], "object")
        self.assertEqual(ro["top_level_keys"], 0)
        self.assertEqual(ro["total_nodes"], 1)
        self.assertEqual(ro["max_depth"], 1)


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
# datalint.inspect is offered to the agent loop as an invocable tool: a request
# carrying a non-empty string `id` is answered with a type:"result" line echoing
# that id; a request without one keeps the legacy uncorrelated type:"items" line.


class FakeConn:
    """Captures sendall payloads so handle() can be driven without a socket."""

    def __init__(self):
        self.lines = []

    def sendall(self, raw):
        self.lines.append(json.loads(raw.decode("utf-8").strip()))


def test_tool_op_with_id_answers_a_correlated_result():
    conn = FakeConn()
    _frame_mod.handle(conn, {"type": "datalint.inspect", "id": "req-7", "json": "42"})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "result", reply
    assert reply["id"] == "req-7", "the request id is echoed verbatim"
    assert reply["data"]["valid"] is True
    assert reply["token"] == _frame_mod.TOKEN


def test_tool_op_without_id_keeps_the_legacy_items_line():
    conn = FakeConn()
    _frame_mod.handle(conn, {"type": "datalint.inspect", "json": "42"})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "items", "no id -> uncorrelated legacy line"
    assert "id" not in reply
    assert reply["data"]["valid"] is True


def test_non_string_or_empty_id_is_treated_as_absent():
    for bad_id in (7, "", None, ["x"]):
        conn = FakeConn()
        _frame_mod.handle(conn, {"type": "datalint.inspect", "id": bad_id, "json": "42"})
        assert conn.lines[0]["type"] == "items", f"id={bad_id!r} must not correlate"


if __name__ == "__main__":
    # Script-style runs exercise the framing + contract tests too — they are plain
    # functions the runner below would otherwise never call.
    test_max_frame_bytes_is_8_mib()
    test_oversized_frame_is_dropped_not_accumulated()
    test_complete_lines_drain_and_partial_is_preserved()
    print("framing: 3 checks ok")
    test_tool_op_with_id_answers_a_correlated_result()
    test_tool_op_without_id_keeps_the_legacy_items_line()
    test_non_string_or_empty_id_is_treated_as_absent()
    print("agent-tool contract: 3 checks ok")
    unittest.main()

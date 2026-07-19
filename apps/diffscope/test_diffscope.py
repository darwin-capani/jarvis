#!/usr/bin/env python3
"""Tests for diffscope.compute — pure unified line-diff. Exit 0 on pass."""
import json

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
# A domain op carrying a request `id` is answered with a type:"result" line
# echoing that id; an op without one keeps the legacy uncorrelated type:"items"
# line — byte-identical to the pre-contract wire (see apps/example-plugin).


class FakeConn:
    """Captures sendall payloads so handle() can be driven without a socket."""

    def __init__(self):
        self.lines = []

    def sendall(self, raw):
        self.lines.append(json.loads(raw.decode("utf-8").strip()))


def test_tool_op_with_id_answers_a_correlated_result():
    conn = FakeConn()
    _frame_mod.handle(conn, {"type": "diffscope.unified", "id": "req-7",
                             "a": "line1\nline2", "b": "line1\nCHANGED"})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "result", reply
    assert reply["id"] == "req-7", "the request id is echoed verbatim"
    assert "error" not in reply["data"], reply
    assert reply["data"]["added"] == 1 and reply["data"]["removed"] == 1, reply
    assert reply["token"] == _frame_mod.TOKEN


def test_tool_op_without_id_keeps_the_legacy_items_line():
    conn = FakeConn()
    _frame_mod.handle(conn, {"type": "diffscope.unified", "a": "x", "b": "y"})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "items", "no id -> uncorrelated legacy line"
    assert "id" not in reply
    assert "error" not in reply["data"], reply


def test_non_string_or_empty_id_is_treated_as_absent():
    for bad_id in (7, "", None, ["x"]):
        conn = FakeConn()
        _frame_mod.handle(conn, {"type": "diffscope.unified", "id": bad_id, "a": "x", "b": "y"})
        assert conn.lines[0]["type"] == "items", f"id={bad_id!r} must not correlate"


if __name__ == "__main__":
    # Script-style runs exercise the framing tests too — they are plain
    # functions the runner below would otherwise never call.
    test_max_frame_bytes_is_8_mib()
    test_oversized_frame_is_dropped_not_accumulated()
    test_complete_lines_drain_and_partial_is_preserved()
    print("framing: 3 checks ok")
    test_basic_change()
    test_pure_additions()
    test_identical_no_diff()
    test_missing_fields_default_empty()
    test_cap_200_lines()
    test_content_lines_starting_with_plusplus_are_counted()
    test_hostile_inputs_never_raise()
    test_tool_op_with_id_answers_a_correlated_result()
    test_tool_op_without_id_keeps_the_legacy_items_line()
    test_non_string_or_empty_id_is_treated_as_absent()
    print("contract: 3 checks ok")
    print("ok")

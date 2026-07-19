#!/usr/bin/env python3
"""Tests for example-plugin's input-frame bounding + the agent-tool contract.

example-plugin is the PLUGIN SDK reference handler. This covers the behavior it
shares with every micro-app — main()'s socket read loop caps a single
un-newlined frame via main.drain_lines / MAX_FRAME_BYTES — plus the CANONICAL
agent-tool request/response contract: a domain op carrying a request `id` is
answered with a type:"result" line echoing that id; an op without one keeps the
legacy uncorrelated type:"items" line. Run from this dir:
`python3 test_example_plugin.py` or `pytest`.
"""
import json

import main


class FakeConn:
    """Captures sendall payloads so handle() can be driven without a socket."""

    def __init__(self):
        self.lines = []

    def sendall(self, raw):
        self.lines.append(json.loads(raw.decode("utf-8").strip()))


def test_max_frame_bytes_is_8_mib():
    assert main.MAX_FRAME_BYTES == 8 * 1024 * 1024


def test_oversized_frame_is_dropped_not_accumulated():
    cap = main.MAX_FRAME_BYTES
    lines, buf, overflowed = main.drain_lines(b"x" * (cap + 1))
    assert overflowed is True
    assert buf == b""
    assert lines == []


def test_complete_lines_drain_and_partial_is_preserved():
    lines, buf, overflowed = main.drain_lines(b'{"a":1}\n{"b":2}\n{"c":3')
    assert lines == [b'{"a":1}', b'{"b":2}']
    assert buf == b'{"c":3'
    assert overflowed is False


# -- the agent-tool request/response contract (SHARED shape; copy per app) ----


def test_tool_op_with_id_answers_a_correlated_result():
    conn = FakeConn()
    main.handle(conn, {"type": "example.read_status", "id": "req-7"})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "result", reply
    assert reply["id"] == "req-7", "the request id is echoed verbatim"
    assert reply["data"]["status"] == "ok"
    assert reply["token"] == main.TOKEN


def test_tool_op_without_id_keeps_the_legacy_items_line():
    conn = FakeConn()
    main.handle(conn, {"type": "example.read_status"})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "items", "no id -> uncorrelated legacy line"
    assert "id" not in reply
    assert reply["data"]["status"] == "ok"


def test_non_string_or_empty_id_is_treated_as_absent():
    for bad_id in (7, "", None, ["x"]):
        conn = FakeConn()
        main.handle(conn, {"type": "example.read_status", "id": bad_id})
        assert conn.lines[0]["type"] == "items", f"id={bad_id!r} must not correlate"


if __name__ == "__main__":
    for t in [
        test_max_frame_bytes_is_8_mib,
        test_oversized_frame_is_dropped_not_accumulated,
        test_complete_lines_drain_and_partial_is_preserved,
        test_tool_op_with_id_answers_a_correlated_result,
        test_tool_op_without_id_keeps_the_legacy_items_line,
        test_non_string_or_empty_id_is_treated_as_absent,
    ]:
        t()
        print("ok:", t.__name__)
    print("ALL PASSED")

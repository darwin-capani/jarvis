#!/usr/bin/env python3
"""Tests for the shared micro-app harness (apps/_sdk/harness.py).

Covers the plumbing every migrated app now inherits from ONE place: the frame
bound (drain_lines / MAX_FRAME_BYTES), token stamping (send), and the agent-tool
request/response id echo (reply_result). Run: `python3 test_harness.py`.
"""
import json

import harness


class FakeConn:
    """Captures sendall payloads so send/reply_result can be driven without a socket."""

    def __init__(self):
        self.lines = []

    def sendall(self, raw):
        self.lines.append(json.loads(raw.decode("utf-8").strip()))


def check(name, cond):
    if not cond:
        print("FAIL:", name)
        raise SystemExit(1)
    print("ok:", name)


def test_max_frame_bytes_is_8_mib():
    check("MAX_FRAME_BYTES == 8 MiB", harness.MAX_FRAME_BYTES == 8 * 1024 * 1024)


def test_oversized_frame_is_dropped():
    lines, buf, overflowed = harness.drain_lines(b"x" * (harness.MAX_FRAME_BYTES + 1))
    check("oversized frame overflowed", overflowed is True)
    check("oversized frame dropped", buf == b"")
    check("oversized frame yields no lines", lines == [])


def test_complete_lines_drain_partial_preserved():
    lines, buf, overflowed = harness.drain_lines(b'{"a":1}\n{"b":2}\n{"c":3')
    check("complete lines drained", lines == [b'{"a":1}', b'{"b":2}'])
    check("partial preserved", buf == b'{"c":3')
    check("no overflow", overflowed is False)


def test_send_stamps_token():
    conn = FakeConn()
    harness.send(conn, {"type": "items", "data": {"x": 1}})
    check("send stamps token", conn.lines[0].get("token") == harness.TOKEN)
    check("send preserves type", conn.lines[0]["type"] == "items")


def test_reply_result_correlates_with_id():
    conn = FakeConn()
    harness.reply_result(conn, {"type": "x.op", "id": "req-9"}, {"answer": 42})
    r = conn.lines[0]
    check("id -> result line", r["type"] == "result")
    check("id echoed", r["id"] == "req-9")
    check("data carried", r["data"]["answer"] == 42)


def test_reply_result_without_id_is_legacy_items():
    conn = FakeConn()
    harness.reply_result(conn, {"type": "x.op"}, {"answer": 42})
    check("no id -> items line", conn.lines[0]["type"] == "items")
    check("no id key", "id" not in conn.lines[0])


def test_reply_result_non_string_or_empty_id_is_legacy():
    for bad in (7, "", None, ["x"]):
        conn = FakeConn()
        harness.reply_result(conn, {"type": "x.op", "id": bad}, {"ok": True})
        check(f"id={bad!r} treated as absent", conn.lines[0]["type"] == "items")


class FakeRunConn:
    """A fake socket for driving harness.run(): recv() replays a queued script of
    byte chunks (then EOF), sendall() captures outbound lines."""

    def __init__(self, chunks):
        self._chunks = list(chunks)
        self.lines = []

    def recv(self, _n):
        return self._chunks.pop(0) if self._chunks else b""

    def sendall(self, raw):
        self.lines.append(json.loads(raw.decode("utf-8").strip()))


def _run_with(chunks, handle, monkey_token="t", monkey_socket="/tmp/x"):
    """Drive harness.run(handle) against a scripted FakeRunConn, bypassing the
    real socket connect. Returns (exit_code, captured_lines)."""
    fake = FakeRunConn(chunks)
    orig_token, orig_socket, orig_socket_mod = harness.TOKEN, harness.SOCKET_PATH, harness.socket
    try:
        harness.TOKEN = monkey_token
        harness.SOCKET_PATH = monkey_socket

        class _SockFactory:
            def socket(self, *_a, **_k):
                class _S:
                    def connect(self, _p):
                        pass
                    def recv(self, n):
                        return fake.recv(n)
                    def sendall(self, raw):
                        fake.sendall(raw)
                return _S()
        harness.socket = _SockFactory()
        harness.socket.AF_UNIX = 0
        harness.socket.SOCK_STREAM = 0
        code = harness.run(handle)
        return code, fake.lines
    finally:
        harness.TOKEN, harness.SOCKET_PATH, harness.socket = orig_token, orig_socket, orig_socket_mod


def test_run_returns_1_when_not_launched_by_darwind():
    # No token/socket -> the honest not-launched exit, no connect attempt.
    orig_t = harness.TOKEN
    try:
        harness.TOKEN = ""
        check("run() -> 1 with no token", harness.run(lambda c, m: None) == 1)
    finally:
        harness.TOKEN = orig_t


def test_run_dispatches_ops_relays_errors_and_stops():
    def handle(conn, msg):
        op = msg.get("type")
        if op == "echo":
            harness.reply_result(conn, msg, {"got": msg.get("v")})
        elif op == "boom":
            raise ValueError("kaboom")
        elif op == "stop":
            raise SystemExit(0)

    # One framed op per line across two recv chunks; a blank line is skipped; a
    # handler exception is relayed as a log; `stop` returns 0.
    chunks = [
        b'{"type":"echo","id":"req-1","v":5}\n\n',
        b'{"type":"boom"}\n{"type":"stop"}\n',
    ]
    code, lines = _run_with(chunks, handle)
    check("run() returns 0 on stop", code == 0)
    check("echo answered as correlated result", lines[0]["type"] == "result" and lines[0]["id"] == "req-1")
    check("echo carried data", lines[0]["data"]["got"] == 5)
    check("handler error relayed as log", lines[1]["type"] == "log" and "kaboom" in lines[1]["data"]["line"])
    # Every outbound line carried the token active during the run ("t").
    check("every outbound line token-stamped", all(l.get("token") == "t" for l in lines))


def test_run_logs_an_overflow_then_stops():
    def handle(conn, msg):
        if msg.get("type") == "stop":
            raise SystemExit(0)

    # An un-newlined chunk over the cap -> dropped + an overflow log; then stop.
    chunks = [b"x" * (harness.MAX_FRAME_BYTES + 1), b'{"type":"stop"}\n']
    code, lines = _run_with(chunks, handle)
    check("run() returns 0 after overflow+stop", code == 0)
    check("overflow relayed as a log", any(l["type"] == "log" and "exceeded" in l["data"]["line"] for l in lines))


if __name__ == "__main__":
    for t in [
        test_max_frame_bytes_is_8_mib,
        test_oversized_frame_is_dropped,
        test_complete_lines_drain_partial_preserved,
        test_send_stamps_token,
        test_reply_result_correlates_with_id,
        test_reply_result_without_id_is_legacy_items,
        test_reply_result_non_string_or_empty_id_is_legacy,
        test_run_returns_1_when_not_launched_by_darwind,
        test_run_dispatches_ops_relays_errors_and_stops,
        test_run_logs_an_overflow_then_stops,
    ]:
        t()
    print("ALL PASSED")

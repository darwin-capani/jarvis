#!/usr/bin/env python3
"""Tests for classify: pure prompt-building + a MOCK generate-proxy end-to-end run."""
import json
import os
import socket
import tempfile
import threading
import main


class MockProxy:
    """A fake generate proxy: binds a unix socket, accepts ONE connection, reads
    the request line (recorded in .request), and replies with the canned JSON."""
    def __init__(self, reply):
        self.reply = reply
        self.request = None
        self.dir = tempfile.mkdtemp()
        self.path = os.path.join(self.dir, "generate.sock")

    def __enter__(self):
        self.srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.srv.bind(self.path)
        self.srv.listen(1)
        threading.Thread(target=self._serve, daemon=True).start()
        return self

    def _serve(self):
        conn, _ = self.srv.accept()
        buf = b""
        while b"\n" not in buf:
            c = conn.recv(4096)
            if not c:
                break
            buf += c
        self.request = json.loads(buf.split(b"\n", 1)[0].decode())
        conn.sendall((json.dumps(self.reply) + "\n").encode())
        conn.close()

    def __exit__(self, *a):
        try:
            self.srv.close()
        except Exception:
            pass


def test_build_prompt_pure():
    # A valid payload yields a non-empty prompt that mentions the text and both labels.
    p = main.build_prompt({"text": "I love this!", "labels": ["positive", "negative"]})
    assert isinstance(p, str), p
    assert p.strip(), p
    assert "I love this!" in p, p
    assert "positive" in p and "negative" in p, p
    # The prompt must instruct the model to emit only a single label.
    assert "ONLY" in p or "only" in p, p

    # Hostile / missing input returns an {"error": ...} dict and never raises.
    assert "error" in main.build_prompt({"text": "", "labels": ["a"]})
    assert "error" in main.build_prompt({"text": "hi", "labels": []})
    assert "error" in main.build_prompt({"text": "hi"})
    assert "error" in main.build_prompt({"labels": ["a"]})
    assert "error" in main.build_prompt({"text": "hi", "labels": [""]})
    assert "error" in main.build_prompt({"text": "hi", "labels": [1, 2]})
    assert "error" in main.build_prompt({"text": 123, "labels": ["a"]})
    assert "error" in main.build_prompt("not a dict")


def test_compute_via_mock_proxy():
    with MockProxy({"ok": True, "text": "  canned answer  "}) as mp:
        r = main.compute({"text": "I love this!", "labels": ["positive", "negative"]}, sock_path=mp.path)
    assert r == {"result": "canned answer"}, r
    assert mp.request["op"] == "generate", mp.request
    assert mp.request.get("text"), mp.request  # a non-empty prompt was sent


def test_compute_proxy_error_never_raises():
    with MockProxy({"ok": False, "error": "rate_limited"}) as mp:
        r = main.compute({"text": "I love this!", "labels": ["positive", "negative"]}, sock_path=mp.path)
    assert "error" in r, r


def test_hostile_inputs_never_raise():
    assert "error" in main.compute(None)
    assert "error" in main.compute([1, 2, 3])


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
# A classify.run op carrying a request `id` is answered with a type:"result" line
# echoing that id; an op without one keeps the legacy uncorrelated type:"items"
# line. handle() calls compute() with no sock_path override, so ContractProxy
# patches main.GENERATE_SOCK to a canned proxy for the op's lifetime -> compute()
# returns a non-error {"result": ...} and the reply's SHAPE is what's asserted.


class FakeConn:
    """Captures sendall payloads so handle() can be driven without a socket."""

    def __init__(self):
        self.lines = []

    def sendall(self, raw):
        self.lines.append(json.loads(raw.decode("utf-8").strip()))


class ContractProxy:
    """A generate proxy for the agent-tool contract tests: it accepts MANY
    connections (handle() calls compute() once per op, with no sock_path
    override) and patches main.GENERATE_SOCK to itself for its lifetime, so
    compute() returns a non-error {"result": ...}. Each connection is answered
    with the same canned reply."""

    def __init__(self, reply):
        self.reply = reply
        self.dir = tempfile.mkdtemp()
        self.path = os.path.join(self.dir, "generate.sock")

    def __enter__(self):
        self.srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.srv.bind(self.path)
        self.srv.listen(8)
        self._stop = False
        threading.Thread(target=self._serve, daemon=True).start()
        self._saved = main.GENERATE_SOCK
        main.GENERATE_SOCK = self.path
        return self

    def _serve(self):
        while not self._stop:
            try:
                conn, _ = self.srv.accept()
            except OSError:
                return
            buf = b""
            while b"\n" not in buf:
                c = conn.recv(4096)
                if not c:
                    break
                buf += c
            try:
                conn.sendall((json.dumps(self.reply) + "\n").encode())
            except OSError:
                pass
            conn.close()

    def __exit__(self, *a):
        self._stop = True
        main.GENERATE_SOCK = self._saved
        try:
            self.srv.close()
        except Exception:
            pass


def test_tool_op_with_id_answers_a_correlated_result():
    conn = FakeConn()
    with ContractProxy({"ok": True, "text": "positive"}):
        main.handle(conn, {"type": "classify.run", "id": "req-7",
                           "text": "I love this!", "labels": ["positive", "negative"]})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "result", reply
    assert reply["id"] == "req-7", "the request id is echoed verbatim"
    assert reply["data"]["result"] == "positive", reply
    assert reply["token"] == main.TOKEN


def test_tool_op_without_id_keeps_the_legacy_items_line():
    conn = FakeConn()
    with ContractProxy({"ok": True, "text": "positive"}):
        main.handle(conn, {"type": "classify.run",
                           "text": "I love this!", "labels": ["positive", "negative"]})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "items", "no id -> uncorrelated legacy line"
    assert "id" not in reply
    assert reply["data"]["result"] == "positive", reply


def test_non_string_or_empty_id_is_treated_as_absent():
    with ContractProxy({"ok": True, "text": "positive"}):
        for bad_id in (7, "", None, ["x"]):
            conn = FakeConn()
            main.handle(conn, {"type": "classify.run", "id": bad_id,
                               "text": "I love this!", "labels": ["positive", "negative"]})
            assert conn.lines[0]["type"] == "items", f"id={bad_id!r} must not correlate"


if __name__ == "__main__":
    # Script-style runs exercise the framing tests too — they are plain
    # functions the runner below would otherwise never call.
    test_max_frame_bytes_is_8_mib()
    test_oversized_frame_is_dropped_not_accumulated()
    test_complete_lines_drain_and_partial_is_preserved()
    print("framing: 3 checks ok")
    for t in [test_build_prompt_pure, test_compute_via_mock_proxy, test_compute_proxy_error_never_raises, test_hostile_inputs_never_raise,
              test_tool_op_with_id_answers_a_correlated_result, test_tool_op_without_id_keeps_the_legacy_items_line, test_non_string_or_empty_id_is_treated_as_absent]:
        t()
        print("ok:", t.__name__)
    print("ALL PASSED")

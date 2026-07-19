#!/usr/bin/env python3
"""Tests for summarize: pure prompt-building + a MOCK generate-proxy end-to-end run."""
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
    # A valid payload yields a non-empty prompt string that mentions the source
    # text and the requested sentence count.
    payload = {"text": "The cat sat on the mat. It was warm.", "sentences": 1}
    prompt = main.build_prompt(payload)
    assert isinstance(prompt, str), prompt
    assert prompt, prompt
    assert "The cat sat on the mat." in prompt, prompt
    assert "1 sentence" in prompt, prompt  # singular unit for N == 1

    # Default sentence count (3) is applied when omitted.
    p3 = main.build_prompt({"text": "Hello world."})
    assert isinstance(p3, str) and "3 sentences" in p3, p3

    # Hostile / missing inputs return an {"error": ...} dict, never raise.
    assert "error" in main.build_prompt({}), "missing text"
    assert "error" in main.build_prompt({"text": "   "}), "blank text"
    assert "error" in main.build_prompt({"text": 123}), "non-str text"
    assert "error" in main.build_prompt({"text": "hi", "sentences": 0}), "sentences < 1"
    assert "error" in main.build_prompt({"text": "hi", "sentences": "two"}), "non-int sentences"
    assert "error" in main.build_prompt(None), "non-dict payload"


def test_compute_via_mock_proxy():
    with MockProxy({"ok": True, "text": "  canned answer  "}) as mp:
        r = main.compute({"text": "The cat sat on the mat. It was warm.", "sentences": 1}, sock_path=mp.path)
    assert r == {"result": "canned answer"}, r
    assert mp.request["op"] == "generate", mp.request
    assert mp.request.get("text"), mp.request  # a non-empty prompt was sent


def test_compute_proxy_error_never_raises():
    with MockProxy({"ok": False, "error": "rate_limited"}) as mp:
        r = main.compute({"text": "The cat sat on the mat. It was warm.", "sentences": 1}, sock_path=mp.path)
    assert "error" in r, r


def test_hostile_inputs_never_raise():
    assert "error" in main.compute(None)
    assert "error" in main.compute([1, 2, 3])


# -- the agent-tool request/response contract (SHARED shape; copy per app) ----
# handle() routes summarize.run through reply_result: a domain op carrying a
# request `id` is answered with a type:"result" line echoing that id; an op
# without one keeps the legacy uncorrelated type:"items" line. handle() runs
# compute(), which reaches the on-device LLM through the generate proxy, so each
# test points main.GENERATE_SOCK at a MockProxy to get a real {"result": ...}
# back, then asserts ONLY the correlation envelope reply_result adds.
_VALID = {"type": "summarize.run", "text": "The cat sat on the mat. It was warm.", "sentences": 1}


class FakeConn:
    """Captures sendall payloads so handle() can be driven without a socket."""

    def __init__(self):
        self.lines = []

    def sendall(self, raw):
        self.lines.append(json.loads(raw.decode("utf-8").strip()))


def _drive(msg, reply):
    """Run handle(msg) with GENERATE_SOCK pointed at a one-shot MockProxy."""
    with MockProxy(reply) as mp:
        saved, main.GENERATE_SOCK = main.GENERATE_SOCK, mp.path
        try:
            conn = FakeConn()
            main.handle(conn, msg)
        finally:
            main.GENERATE_SOCK = saved
    return conn


def test_tool_op_with_id_answers_a_correlated_result():
    conn = _drive(dict(_VALID, id="req-7"), {"ok": True, "text": "  canned answer  "})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "result", reply
    assert reply["id"] == "req-7", "the request id is echoed verbatim"
    assert reply["data"]["result"] == "canned answer"
    assert reply["token"] == main.TOKEN


def test_tool_op_without_id_keeps_the_legacy_items_line():
    conn = _drive(dict(_VALID), {"ok": True, "text": "canned answer"})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "items", "no id -> uncorrelated legacy line"
    assert "id" not in reply
    assert reply["data"]["result"] == "canned answer"


def test_non_string_or_empty_id_is_treated_as_absent():
    for bad_id in (7, "", None, ["x"]):
        conn = _drive(dict(_VALID, id=bad_id), {"ok": True, "text": "canned answer"})
        assert conn.lines[0]["type"] == "items", f"id={bad_id!r} must not correlate"


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
    for t in [
        test_build_prompt_pure,
        test_compute_via_mock_proxy,
        test_compute_proxy_error_never_raises,
        test_hostile_inputs_never_raise,
        test_tool_op_with_id_answers_a_correlated_result,
        test_tool_op_without_id_keeps_the_legacy_items_line,
        test_non_string_or_empty_id_is_treated_as_absent,
    ]:
        t()
        print("ok:", t.__name__)
    print("ALL PASSED")

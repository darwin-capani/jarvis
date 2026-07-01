#!/usr/bin/env python3
"""Tests for rewrite: pure prompt-building + a MOCK generate-proxy end-to-end run."""
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
    # A valid payload yields a non-empty prompt string that mentions both the
    # requested tone and the input text.
    p = main.build_prompt({"text": "hey can u send the thing", "tone": "formal"})
    assert isinstance(p, str) and p, p
    assert "formal" in p, p
    assert "hey can u send the thing" in p, p

    # Default tone kicks in when tone is missing/blank.
    p2 = main.build_prompt({"text": "ship it"})
    assert isinstance(p2, str) and "clear and professional" in p2, p2
    p3 = main.build_prompt({"text": "ship it", "tone": "   "})
    assert isinstance(p3, str) and "clear and professional" in p3, p3

    # Hostile / missing / wrong-type inputs return an {"error": ...}, never raise.
    assert "error" in main.build_prompt({"text": ""})
    assert "error" in main.build_prompt({"text": "   "})
    assert "error" in main.build_prompt({"text": 123})
    assert "error" in main.build_prompt({})
    assert "error" in main.build_prompt("not a dict")


def test_compute_via_mock_proxy():
    with MockProxy({"ok": True, "text": "  canned answer  "}) as mp:
        r = main.compute({"text": "hey can u send the thing", "tone": "formal"}, sock_path=mp.path)
    assert r == {"result": "canned answer"}, r
    assert mp.request["op"] == "generate", mp.request
    assert mp.request.get("text"), mp.request  # a non-empty prompt was sent


def test_compute_proxy_error_never_raises():
    with MockProxy({"ok": False, "error": "rate_limited"}) as mp:
        r = main.compute({"text": "hey can u send the thing", "tone": "formal"}, sock_path=mp.path)
    assert "error" in r, r


def test_hostile_inputs_never_raise():
    assert "error" in main.compute(None)
    assert "error" in main.compute([1, 2, 3])


if __name__ == "__main__":
    for t in [test_build_prompt_pure, test_compute_via_mock_proxy, test_compute_proxy_error_never_raises, test_hostile_inputs_never_raise]:
        t()
        print("ok:", t.__name__)
    print("ALL PASSED")

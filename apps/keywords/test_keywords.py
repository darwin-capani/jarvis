#!/usr/bin/env python3
"""Tests for keywords: pure prompt-building + a MOCK generate-proxy end-to-end run."""
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
    # A valid payload yields a non-empty prompt string that embeds the input text
    # and the requested count.
    p = main.build_prompt(
        {"text": "Rust is a systems language focused on safety and speed.", "count": 3}
    )
    assert isinstance(p, str) and p, p
    assert "Rust is a systems language focused on safety and speed." in p, p
    assert "3" in p, p  # the requested keyword count is stated in the prompt

    # Count defaults to 5 when omitted.
    d = main.build_prompt({"text": "hello world"})
    assert isinstance(d, str) and "5" in d, d

    # Hostile / empty inputs return an {"error": ...} dict, never raise.
    assert "error" in main.build_prompt({"text": ""})
    assert "error" in main.build_prompt({"text": "   "})
    assert "error" in main.build_prompt({"text": 123})
    assert "error" in main.build_prompt({})
    assert "error" in main.build_prompt({"text": "ok", "count": "many"})
    assert "error" in main.build_prompt({"text": "ok", "count": 0})
    assert "error" in main.build_prompt("not a dict")


def test_compute_via_mock_proxy():
    with MockProxy({"ok": True, "text": "  canned answer  "}) as mp:
        r = main.compute(
            {"text": "Rust is a systems language focused on safety and speed.", "count": 3},
            sock_path=mp.path,
        )
    assert r == {"result": "canned answer"}, r
    assert mp.request["op"] == "generate", mp.request
    assert mp.request.get("text"), mp.request  # a non-empty prompt was sent


def test_compute_proxy_error_never_raises():
    with MockProxy({"ok": False, "error": "rate_limited"}) as mp:
        r = main.compute(
            {"text": "Rust is a systems language focused on safety and speed.", "count": 3},
            sock_path=mp.path,
        )
    assert "error" in r, r


def test_hostile_inputs_never_raise():
    assert "error" in main.compute(None)
    assert "error" in main.compute([1, 2, 3])


if __name__ == "__main__":
    for t in [test_build_prompt_pure, test_compute_via_mock_proxy, test_compute_proxy_error_never_raises, test_hostile_inputs_never_raise]:
        t()
        print("ok:", t.__name__)
    print("ALL PASSED")

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


if __name__ == "__main__":
    for t in [test_build_prompt_pure, test_compute_via_mock_proxy, test_compute_proxy_error_never_raises, test_hostile_inputs_never_raise]:
        t()
        print("ok:", t.__name__)
    print("ALL PASSED")

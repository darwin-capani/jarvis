#!/usr/bin/env python3
"""On-device AI keyword extractor: pull the top keywords/tags from text via the local LLM."""
import json
import os
import socket
import sys

# Shared host-link plumbing (socket loop, token stamping, frame bound, the
# agent-tool id echo) from apps/_sdk — fs_read-granted. The path is resolved
# relative to THIS file (apps/<app>/main.py -> ../_sdk), so it works both when
# darwind launches the app (cwd = project root) and when the tests run from the
# app dir. Bytecode writes are disabled since apps/_sdk is read-only in the
# sandbox. Re-importing drain_lines/MAX_FRAME_BYTES/TOKEN keeps them resolvable
# off `main` for the framing/contract tests.
sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "_sdk"))
from harness import (  # noqa: E402 — must follow the sys.path insert above
    MAX_FRAME_BYTES,
    SOCKET_PATH,
    TOKEN,
    drain_lines,
    reply_result,
    run,
    send,
)

APP_NAME = os.environ.get("DARWIN_APP_NAME", "keywords")
# The daemon-mediated generate proxy lives beside our own relay socket.
GENERATE_SOCK = (
    os.path.join(os.path.dirname(SOCKET_PATH), "generate.sock") if SOCKET_PATH else ""
)
_MAX_TOKENS = 256


def build_prompt(payload):
    """PURE: build the on-device-LLM prompt string from the payload, or return an
    {"error": ...} dict on bad/missing input. Never raises. Reads payload["text"]
    (non-empty str) and optional payload["count"] (int, default 5) and instructs the
    model to emit exactly N keywords as one comma-separated line, no extra prose."""
    if not isinstance(payload, dict):
        return {"error": "payload must be an object"}
    text = payload.get("text")
    if not isinstance(text, str) or not text.strip():
        return {"error": "text must be a non-empty string"}

    count = payload.get("count", 5)
    if isinstance(count, bool) or not isinstance(count, int):
        return {"error": "count must be an integer"}
    if count < 1:
        return {"error": "count must be >= 1"}
    if count > 50:
        count = 50

    text = text.strip()
    return (
        "You are a keyword extraction engine. Read the TEXT below and identify the "
        f"{count} most important keywords or tags that capture its main topics.\n"
        "Rules:\n"
        f"- Output EXACTLY {count} keywords, ranked most important first.\n"
        "- Output ONE line only: the keywords separated by commas (e.g. alpha, beta, gamma).\n"
        "- Keep each keyword to 1-3 words; prefer nouns and noun phrases.\n"
        "- Do NOT add explanations, numbering, quotes, or any other prose.\n\n"
        f"TEXT:\n{text}\n\n"
        "Keywords:"
    )


def generate(prompt, max_tokens=_MAX_TOKENS, sock_path=None):
    """Ask the on-device LLM through the daemon generate proxy (op=generate ONLY).
    Returns the generated text; raises RuntimeError on any proxy error. sock_path
    is injectable so tests can point it at a mock proxy."""
    path = sock_path or GENERATE_SOCK
    if not path:
        raise RuntimeError("generate proxy socket unavailable")
    req = {"name": APP_NAME, "token": TOKEN, "op": "generate", "text": prompt, "max_tokens": int(max_tokens)}
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as gc:
        gc.settimeout(30)
        gc.connect(path)
        gc.sendall((json.dumps(req) + "\n").encode("utf-8"))
        buf = b""
        while b"\n" not in buf:
            chunk = gc.recv(4096)
            if not chunk:
                break
            buf += chunk
    reply = json.loads(buf.split(b"\n", 1)[0].decode("utf-8"))
    if not reply.get("ok"):
        raise RuntimeError(str(reply.get("error", "generate failed")))
    return reply.get("text", "")


def compute(payload, sock_path=None):
    """The AI op: build the prompt, call the on-device LLM, shape the result.
    NEVER raises — returns {"error": ...} on bad input or a proxy error."""
    if not isinstance(payload, dict):
        return {"error": "payload must be an object"}
    prompt = build_prompt(payload)
    if isinstance(prompt, dict):
        return prompt  # build_prompt returned an {"error": ...}
    try:
        text = generate(prompt, sock_path=sock_path)
    except Exception as e:  # noqa: BLE001 — compute never raises
        return {"error": f"generate failed: {e}"}
    return {"result": text.strip()}


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "keywords.run", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "keywords.run":
        reply_result(conn, msg, compute(msg))
    elif op == "stop":
        raise SystemExit(0)


if __name__ == "__main__":
    sys.exit(run(handle))

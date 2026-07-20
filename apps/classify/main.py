#!/usr/bin/env python3
"""On-device AI text classifier: assign text to exactly one caller-provided label via the local LLM."""
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
# off `main` for the framing/contract tests. SOCKET_PATH is re-imported too: this
# app derives its generate-proxy path from it.
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

APP_NAME = os.environ.get("DARWIN_APP_NAME", "classify")
# The daemon-mediated generate proxy lives beside our own relay socket.
GENERATE_SOCK = (
    os.path.join(os.path.dirname(SOCKET_PATH), "generate.sock") if SOCKET_PATH else ""
)
_MAX_TOKENS = 256


def build_prompt(payload):
    """PURE: build the on-device-LLM prompt string from the payload, or return an
    {"error": ...} dict on bad/missing input. Never raises. Reads payload["text"]
    (non-empty str) and payload["labels"] (non-empty list of non-empty strings)
    and instructs the model to reply with ONLY the single best-matching label."""
    if not isinstance(payload, dict):
        return {"error": "payload must be an object"}
    text = payload.get("text")
    if not isinstance(text, str) or not text.strip():
        return {"error": "text must be a non-empty string"}
    labels = payload.get("labels")
    if not isinstance(labels, list) or not labels:
        return {"error": "labels must be a non-empty list of strings"}
    clean_labels = []
    for label in labels:
        if not isinstance(label, str) or not label.strip():
            return {"error": "each label must be a non-empty string"}
        clean_labels.append(label.strip())
    label_lines = "\n".join(f"- {label}" for label in clean_labels)
    joined = ", ".join(clean_labels)
    return (
        "You are a precise text classification engine.\n"
        "Classify the text below into exactly ONE of the allowed labels.\n"
        "Reply with ONLY that single label, copied verbatim from the list — no "
        "punctuation, quotes, explanation, or extra words.\n"
        f"If unsure, pick the closest label; you MUST choose one of: {joined}.\n\n"
        "Allowed labels:\n"
        f"{label_lines}\n\n"
        "Text:\n"
        f"{text.strip()}\n\n"
        "Label:"
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
        send(conn, {"type": "status", "data": {"tool": "classify.run", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "classify.run":
        reply_result(conn, msg, compute(msg))
    elif op == "stop":
        raise SystemExit(0)


if __name__ == "__main__":
    sys.exit(run(handle))

#!/usr/bin/env python3
"""Read-only text-statistics panel: counts, word metrics, and a readability proxy. Pure, offline."""
import json
import os
import socket
import sys

TOKEN = os.environ.get("JARVIS_APP_TOKEN", "")
SOCKET_PATH = os.environ.get("JARVIS_APP_SOCKET", "")


def send(conn, obj):
    obj["token"] = TOKEN
    conn.sendall((json.dumps(obj) + "\n").encode("utf-8"))


def compute(payload):
    """PURE, offline, no I/O, never raises. Return text statistics for payload["text"]."""
    if not isinstance(payload, dict):
        payload = {}
    text = payload.get("text", "")
    if not isinstance(text, str):
        text = ""

    chars = len(text)

    # Words: whitespace-delimited tokens, stripped of surrounding punctuation for
    # length/uniqueness metrics. Keep the raw token count as "words".
    raw_tokens = text.split()
    words = len(raw_tokens)

    # Clean tokens for length/longest/unique metrics: drop leading/trailing
    # non-alphanumeric chars so "hello," and "hello" are the same word.
    cleaned = []
    for tok in raw_tokens:
        w = tok.strip(".,!?;:\"'()[]{}<>-–—…“”‘’`")
        if w:
            cleaned.append(w)

    # Sentence terminators: . ! ?  (min 1 if there is any non-space text).
    terminators = sum(1 for ch in text if ch in ".!?")
    has_text = len(text.strip()) > 0
    if terminators == 0 and has_text:
        sentences = 1
    else:
        sentences = terminators

    if cleaned:
        total_len = sum(len(w) for w in cleaned)
        avg_word_len = round(total_len / len(cleaned), 1)
        longest_word = max(cleaned, key=len)
        unique_words = len({w.lower() for w in cleaned})
    else:
        avg_word_len = 0.0
        longest_word = ""
        unique_words = 0

    if sentences > 0:
        words_per_sentence = round(words / sentences, 1)
    else:
        words_per_sentence = 0.0

    return {
        "chars": chars,
        "words": words,
        "sentences": sentences,
        "avg_word_len": avg_word_len,
        "longest_word": longest_word,
        "unique_words": unique_words,
        "words_per_sentence": words_per_sentence,
    }


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "textkit.stats", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "textkit.stats":
        send(conn, {"type": "items", "data": compute(msg)})
    elif op == "stop":
        raise SystemExit(0)


def main():
    if not TOKEN or not SOCKET_PATH:
        print("missing JARVIS_APP_TOKEN / JARVIS_APP_SOCKET; not launched by jarvisd", file=sys.stderr)
        return 1
    conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    conn.connect(SOCKET_PATH)
    buf = b""
    while True:
        chunk = conn.recv(4096)
        if not chunk:
            break
        buf += chunk
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            if not line.strip():
                continue
            try:
                handle(conn, json.loads(line))
            except SystemExit:
                return 0
            except Exception as e:  # noqa: BLE001 — a plugin never crashes the host
                send(conn, {"type": "log", "data": {"line": f"handler error: {e}"}})
    return 0


if __name__ == "__main__":
    sys.exit(main())

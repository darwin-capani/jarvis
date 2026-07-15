#!/usr/bin/env python3
"""Read-only text-statistics panel: counts, word metrics, and a readability proxy. Pure, offline."""
import json
import os
import socket
import sys

TOKEN = os.environ.get("DARWIN_APP_TOKEN", "")
SOCKET_PATH = os.environ.get("DARWIN_APP_SOCKET", "")


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


MAX_FRAME_BYTES = 8 * 1024 * 1024  # cap on one un-newlined frame from the daemon


def drain_lines(buf, max_frame=MAX_FRAME_BYTES):
    """PURE framing: split every complete newline-terminated line out of buf.

    Returns (lines, remaining, overflowed): the complete lines with their trailing
    newline stripped in arrival order, the leftover partial buffer, and whether
    that leftover grew past max_frame WITHOUT a newline. When it has, the leftover
    is DROPPED (returned as b"") so a peer streaming an unframed, unbounded blob
    can't grow the read buffer without bound (OOM) — the daemon side is already
    bounded (apps.rs read_line_bounded / genproxy MAX_PROXY_LINE_BYTES). Newline
    framing is otherwise identical to buf.split(b"\\n", 1). Never raises."""
    lines = []
    while b"\n" in buf:
        line, buf = buf.split(b"\n", 1)
        lines.append(line)
    overflowed = len(buf) > max_frame
    if overflowed:
        buf = b""
    return lines, buf, overflowed


def main():
    if not TOKEN or not SOCKET_PATH:
        print("missing DARWIN_APP_TOKEN / DARWIN_APP_SOCKET; not launched by darwind", file=sys.stderr)
        return 1
    conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    conn.connect(SOCKET_PATH)
    buf = b""
    while True:
        chunk = conn.recv(4096)
        if not chunk:
            break
        buf += chunk
        lines, buf, overflowed = drain_lines(buf)
        for line in lines:
            if not line.strip():
                continue
            try:
                handle(conn, json.loads(line))
            except SystemExit:
                return 0
            except Exception as e:  # noqa: BLE001 — a plugin never crashes the host
                send(conn, {"type": "log", "data": {"line": f"handler error: {e}"}})
        if overflowed:
            send(conn, {"type": "log", "data": {"line": f"input frame exceeded {MAX_FRAME_BYTES} bytes; dropped"}})
    return 0


if __name__ == "__main__":
    sys.exit(main())

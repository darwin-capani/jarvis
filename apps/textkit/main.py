#!/usr/bin/env python3
"""Read-only text-statistics panel: counts, word metrics, and a readability proxy. Pure, offline."""
import os
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
    TOKEN,
    drain_lines,
    reply_result,
    run,
    send,
)


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
        reply_result(conn, msg, compute(msg))
    elif op == "stop":
        raise SystemExit(0)


if __name__ == "__main__":
    sys.exit(run(handle))

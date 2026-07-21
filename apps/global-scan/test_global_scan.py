#!/usr/bin/env python3
"""Tests for global-scan's daemon-mediated FETCH-proxy migration.

The app no longer has direct network egress: it fetches every feed THROUGH the
daemon fetch proxy over state/ipc/apps/fetch.sock (op=fetch), exactly like it
already talks to the generate proxy. These tests exercise the new
`FeedFetchClient` wire client with a MOCK socket (no real network, no daemon),
plus the fetch_feed -> parse path and a full run_cycle over a stubbed proxy.

Dual-style (repo convention): each `test_*` is pytest-discoverable AND driven by
the __main__ runner, which prints "ALL PASSED" on success. Run either:
    python3.11 test_global_scan.py        # script style
    pytest test_global_scan.py            # pytest style
"""
import io
import json
import os
import sys

import main


def check(name, cond):
    if not cond:
        print("FAIL:", name)
        sys.exit(1)
    print("ok:", name)


# --------------------------------------------------------------------------- #
# A mock AF_UNIX socket: captures the bytes the client sends and replays a
# canned one-line JSON reply. No real socket, no daemon.
# --------------------------------------------------------------------------- #
class _FakeSocket:
    def __init__(self, reply_text, capture):
        self._reply = reply_text
        self._cap = capture

    def settimeout(self, t):
        self._cap.setdefault("timeouts", []).append(t)

    def connect(self, path):
        self._cap["connected"] = path

    def sendall(self, payload):
        self._cap["sent"] = payload

    def makefile(self, *a, **k):
        return io.StringIO(self._reply)

    def close(self):
        self._cap["closed"] = True


def _with_socket(reply_text, capture):
    """Return a socket.socket replacement that hands out the fake."""
    def factory(*a, **k):
        return _FakeSocket(reply_text, capture)
    return factory


def _run_fetch(reply_obj_or_text):
    """Drive FeedFetchClient.fetch against a mock socket; return (result_or_exc,
    capture). result is the returned body or the raised exception."""
    if isinstance(reply_obj_or_text, str):
        reply = reply_obj_or_text
    else:
        reply = json.dumps(reply_obj_or_text) + "\n"
    cap = {}
    orig = main.socket.socket
    main.socket.socket = _with_socket(reply, cap)
    try:
        client = main.FeedFetchClient(main.FETCH_SOCKET, "global-scan", "tok-123")
        try:
            return client.fetch("https://feeds.npr.org/1001/rss.xml"), cap
        except Exception as exc:  # noqa: BLE001 - the tests classify it
            return exc, cap
    finally:
        main.socket.socket = orig


# A minimal but valid RSS feed with one dated item.
_SAMPLE_RSS = (
    '<?xml version="1.0" encoding="UTF-8"?>'
    '<rss version="2.0"><channel><title>NPR</title>'
    "<item><title>Test headline</title>"
    "<link>https://example.com/a</link>"
    "<pubDate>Mon, 21 Jul 2026 00:00:00 GMT</pubDate>"
    "<description>Some body text.</description></item>"
    "</channel></rss>"
)

_SAMPLE_ATOM = (
    '<?xml version="1.0" encoding="UTF-8"?>'
    '<feed xmlns="http://www.w3.org/2005/Atom"><title>Ars Technica</title>'
    "<entry><title>Atom item</title>"
    '<link rel="alternate" href="https://example.com/b"/>'
    "<published>2026-07-21T00:00:00Z</published>"
    "<summary>Atom summary.</summary></entry></feed>"
)


# --------------------------------------------------------------------------- #
# Tests.
# --------------------------------------------------------------------------- #
def test_drain_lines_framing():
    # Complete lines drain; a partial tail is preserved; an un-newlined blob
    # past the cap is dropped (no unbounded growth).
    lines, rem, over = main._drain_lines(b"a\nb\nc")
    check("drain complete lines", lines == [b"a", b"b"])
    check("drain keeps partial", rem == b"c")
    check("drain no overflow", over is False)
    big = b"x" * (main.MAX_FRAME_BYTES + 10)
    _, rem2, over2 = main._drain_lines(big)
    check("drain drops oversized", rem2 == b"" and over2 is True)


def test_fetch_client_ok_returns_body_and_wire_shape():
    body, cap = _run_fetch({"ok": True, "status": 200, "body": _SAMPLE_RSS})
    check("fetch returns the body", body == _SAMPLE_RSS)
    check("fetch connected to fetch.sock", str(cap["connected"]).endswith("fetch.sock"))
    sent = json.loads(cap["sent"].decode("utf-8"))
    check("wire op is fetch", sent["op"] == "fetch")
    check("wire carries name", sent["name"] == "global-scan")
    check("wire carries token", sent["token"] == "tok-123")
    check("wire carries url", sent["url"] == "https://feeds.npr.org/1001/rss.xml")
    check("wire has exactly these keys", set(sent.keys()) == {"name", "token", "op", "url"})
    check("socket closed", cap.get("closed") is True)


def test_fetch_client_ok_false_raises():
    for kind in (
        "op_not_permitted",
        "unauthorized",
        "rate_limited",
        "url_not_permitted",
        "host_resolves_private",
        "redirect_denied",
        "too_many_redirects",
        "body_too_large",
        "fetch_failed",
    ):
        res, _ = _run_fetch({"ok": False, "error": kind})
        check(f"ok=false ({kind}) raises OSError", isinstance(res, OSError))


def test_fetch_client_missing_ok_raises():
    # A reply with no ok field is treated as a failure (fail-closed).
    res, _ = _run_fetch({"status": 200, "body": "x"})
    check("absent ok raises", isinstance(res, OSError))


def test_fetch_client_empty_reply_raises():
    res, _ = _run_fetch("")  # empty line -> proxy gave nothing
    check("empty reply raises OSError", isinstance(res, OSError))


class _StubClient:
    """A FeedFetchClient stand-in: returns a fixed body for any URL."""

    def __init__(self, body, sock_path=None, name=None, token=None):
        self._body = body

    def fetch(self, url):
        return self._body


def test_fetch_feed_parses_body_from_the_client():
    items = main.fetch_feed("https://feeds.npr.org/1001/rss.xml", "world", _StubClient(_SAMPLE_RSS))
    check("one item parsed", len(items) == 1)
    it = items[0]
    check("title parsed", it["title"] == "Test headline")
    check("url parsed", it["url"] == "https://example.com/a")
    check("category threaded", it["category"] == "world")


def test_fetch_feed_empty_body_raises():
    try:
        main.fetch_feed("https://feeds.npr.org/x", "world", _StubClient(""))
    except ValueError:
        check("empty body -> ValueError (feed failed)", True)
        return
    check("empty body must raise ValueError", False)


def test_parse_feed_atom_regression():
    items = main.parse_feed("https://feeds.arstechnica.com/x", _SAMPLE_ATOM.encode("utf-8"), "tech")
    check("atom item parsed", len(items) == 1 and items[0]["title"] == "Atom item")
    check("atom link parsed", items[0]["url"] == "https://example.com/b")


def test_run_cycle_uses_the_fetch_proxy_and_falls_back_to_extractive():
    # Swap FeedFetchClient for a stub that returns the sample RSS for every feed,
    # so run_cycle resolves all feeds THROUGH the (stubbed) proxy with no network.
    orig = main.FeedFetchClient

    def _factory(sock_path, name, token):
        return _StubClient(_SAMPLE_RSS)

    main.FeedFetchClient = _factory
    # Ensure no launch token in env, so the LLM enhancement is treated as
    # unavailable and we exercise the extractive-summary fallback.
    saved_token = os.environ.pop("DARWIN_APP_TOKEN", None)
    try:
        feeds = main.load_feeds()
        total = sum(len(v) for v in feeds.values())
        result = main.run_cycle(feeds, link=None)
    finally:
        main.FeedFetchClient = orig
        if saved_token is not None:
            os.environ["DARWIN_APP_TOKEN"] = saved_token

    check("every feed resolved through the proxy", result["feeds_ok"] == total)
    check("no feed failed", result["feeds_failed"] == 0)
    check("items produced", len(result["items"]) >= 1)
    check("extractive fallback (no LLM)", result["used_llm"] is False)
    # Every emitted item carries a non-empty summary (extractive fallback).
    check("all items summarized", all(it["summary"] for it in result["items"]))


def main_run():
    test_drain_lines_framing()
    test_fetch_client_ok_returns_body_and_wire_shape()
    test_fetch_client_ok_false_raises()
    test_fetch_client_missing_ok_raises()
    test_fetch_client_empty_reply_raises()
    print("fetch client: ok")
    test_fetch_feed_parses_body_from_the_client()
    test_fetch_feed_empty_body_raises()
    test_parse_feed_atom_regression()
    print("fetch_feed / parse: ok")
    test_run_cycle_uses_the_fetch_proxy_and_falls_back_to_extractive()
    print("run_cycle: ok")
    print("ALL PASSED")


if __name__ == "__main__":
    main_run()

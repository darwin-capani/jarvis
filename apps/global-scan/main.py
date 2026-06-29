#!/usr/bin/env python3.11
"""Global-Scan — JARVIS micro-app: world intel feed aggregator.

First app on the micro-app runtime substrate (docs/SANDBOX.md). It runs as a
separate, seatbelt-sandboxed process launched by jarvisd, talks to the daemon
over a per-app Unix socket using newline-delimited JSON, and authenticates every
line it sends with the capability token jarvisd minted for this launch.

What it does, honestly framed: it polls OPEN, reputable, non-paywalled RSS/Atom
feeds (apps/global-scan/feeds.toml), dedupes and ranks the latest items
newest-first, and renders an intel digest in the HUD. If the local inference
server is reachable it adds a NEUTRAL one-line summary per top item plus a short
overall brief; if not, it falls back to extractive summaries from the feed text.
It predicts nothing and surveils no one — it reads syndication feeds publishers
offer openly.

Protocol (must match the daemon's app host, CONTRACT part B):
  env JARVIS_APP_SOCKET  abs path to state/ipc/apps/global-scan.sock
  env JARVIS_APP_TOKEN   capability token to stamp on every outbound line
  env JARVIS_APP_NAME    this app's name (defaults to "global-scan")

  app -> host (one JSON object per line):
    {"token": <env>, "type": "items",  "data": {"brief", "items":[...], "fetched_at"}}
    {"token": <env>, "type": "status", "data": {"feeds_ok", "feeds_failed"}}
    {"token": <env>, "type": "log",    "data": {"line"}}

  host -> app (one JSON object per line):
    {"type": "start"}    begin / resume polling
    {"type": "refresh"}  fetch now, ignoring the interval timer
    {"type": "stop"}     stop polling and exit cleanly

LLM summaries go through the daemon-mediated generate PROXY, not the raw
inference socket (security finding #4, docs/SANDBOX.md). The proxy lives at
state/ipc/apps/generate.sock, accepts ONLY op=generate, is token-gated, caps
max_tokens at 256, and rate-limits per app. The request stamps name + the
JARVIS_APP_TOKEN capability token; on any ok=false reply (op_not_permitted,
unauthorized, rate_limited, inference_unavailable) OR an unreachable proxy the
app falls back to extractive summaries exactly as before. The app is granted
ONLY this proxy socket — it can no longer reach the multiplexed inference.sock.

Stdlib only — no feedparser, no heavy deps, no torch. urllib for HTTPS and
xml.etree for parsing. The poll loop runs in a worker thread so the socket
reader can deliver host commands without blocking on the network.
"""

from __future__ import annotations

import datetime as _dt
import html
import json
import os
import re
import socket
import sys
import threading
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET
from email.utils import parsedate_to_datetime
from pathlib import Path

try:  # py3.11 stdlib
    import tomllib
except ModuleNotFoundError:  # pragma: no cover - py<3.11 fallback path
    tomllib = None  # type: ignore[assignment]

APP_NAME = "global-scan"
APP_DIR = Path(__file__).resolve().parent
PROJECT_ROOT = APP_DIR.parents[1]
FEEDS_PATH = APP_DIR / "feeds.toml"

# Tunables.
REFRESH_INTERVAL_S = 10 * 60       # ~10 min between cycles
TOP_N = 20                         # items kept and emitted per cycle
FETCH_TIMEOUT_S = 12               # per-feed HTTP timeout
USER_AGENT = "JARVIS-GlobalScan/0.1 (+micro-app; RSS reader)"
# Daemon-mediated generate PROXY (NOT the raw inference.sock): op-restricted,
# token-gated, 256-token-capped, rate-limited. This is the ONLY socket the app
# is granted (manifest fs_read), closing security finding #4.
LLM_SOCKET = PROJECT_ROOT / "state" / "ipc" / "apps" / "generate.sock"
LLM_CONNECT_TIMEOUT_S = 1.5
LLM_READ_TIMEOUT_S = 12.0
LLM_SUMMARY_MAX_ITEMS = 8          # only summarize the top few (keeps it cheap)
LLM_SUMMARY_MAX_TOKENS = 40
LLM_BRIEF_MAX_TOKENS = 90

ATOM = "{http://www.w3.org/2005/Atom}"

# Built-in default feeds: used only if feeds.toml is missing/unreadable. These
# mirror feeds.toml exactly and every host is in the manifest's net_hosts.
DEFAULT_FEEDS = {
    "world": [
        "https://feeds.npr.org/1001/rss.xml",
        "https://feeds.bbci.co.uk/news/world/rss.xml",
    ],
    "tech": [
        "https://feeds.arstechnica.com/arstechnica/index",
        "https://hnrss.org/frontpage",
        "https://www.theverge.com/rss/index.xml",
    ],
    "science": [
        "https://www.sciencedaily.com/rss/top/science.xml",
        "https://www.nasa.gov/feed/",
    ],
    "markets": [
        "https://feeds.a.dj.com/rss/RSSMarketsMain.xml",
        "https://feeds.content.dowjones.io/public/rss/mw_topstories",
    ],
}

# Per-source display name (host -> label) for clean HUD chips.
SOURCE_LABELS = {
    "feeds.npr.org": "NPR",
    "feeds.bbci.co.uk": "BBC",
    "www.theguardian.com": "Guardian",
    "feeds.arstechnica.com": "Ars Technica",
    "hnrss.org": "Hacker News",
    "www.theverge.com": "The Verge",
    "www.sciencedaily.com": "ScienceDaily",
    "www.nasa.gov": "NASA",
    "feeds.a.dj.com": "WSJ Markets",
    "feeds.content.dowjones.io": "MarketWatch",
    "search.cnbc.com": "CNBC",
}

# Words that flag an item as breaking/alert (drives the HUD's red accent).
_ALERT_RE = re.compile(
    r"\b(breaking|urgent|alert|just in|developing|live updates?|"
    r"emergency|evacuat\w*|earthquake|tsunami|wildfire|airstrike|"
    r"explosion|attack|outage|recall|war\b)\b",
    re.IGNORECASE,
)

_TAG_RE = re.compile(r"<[^>]+>")
_WS_RE = re.compile(r"\s+")


# --------------------------------------------------------------------------- #
# Host IPC: a thin authenticated writer + a command reader.
# --------------------------------------------------------------------------- #
class HostLink:
    """Newline-delimited JSON link to the daemon's app host over a Unix socket.

    Every outbound line carries the capability token from the environment. The
    daemon verifies it and drops anything that does not match, so we simply
    stamp it and write. Writes are serialized with a lock because the poll
    thread and the main thread can both emit (e.g. logs)."""

    def __init__(self, sock_path: str, token: str) -> None:
        self._token = token
        self._sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._sock.connect(sock_path)
        self._wlock = threading.Lock()
        self._rfile = self._sock.makefile("r", encoding="utf-8", newline="\n")

    def send(self, msg_type: str, data: dict) -> None:
        line = json.dumps(
            {"token": self._token, "type": msg_type, "data": data},
            ensure_ascii=False,
            separators=(",", ":"),
        )
        payload = (line + "\n").encode("utf-8")
        with self._wlock:
            try:
                self._sock.sendall(payload)
            except (BrokenPipeError, OSError):
                # Host went away; nothing useful we can do — let the read loop
                # observe EOF and shut us down.
                pass

    def log(self, line: str) -> None:
        self.send("log", {"line": line})

    def commands(self):
        """Yield host->app command dicts until the connection closes."""
        for raw in self._rfile:
            raw = raw.strip()
            if not raw:
                continue
            try:
                yield json.loads(raw)
            except json.JSONDecodeError:
                continue

    def close(self) -> None:
        try:
            self._rfile.close()
        except OSError:
            pass
        try:
            self._sock.close()
        except OSError:
            pass


# --------------------------------------------------------------------------- #
# Feed config.
# --------------------------------------------------------------------------- #
def load_feeds() -> dict[str, list[str]]:
    """Load category -> [urls] from feeds.toml; fall back to DEFAULT_FEEDS."""
    if tomllib is None or not FEEDS_PATH.exists():
        return {k: list(v) for k, v in DEFAULT_FEEDS.items()}
    try:
        with FEEDS_PATH.open("rb") as fh:
            doc = tomllib.load(fh)
    except (OSError, tomllib.TOMLDecodeError):
        return {k: list(v) for k, v in DEFAULT_FEEDS.items()}

    out: dict[str, list[str]] = {}
    for category, section in doc.items():
        if isinstance(section, dict):
            feeds = section.get("feeds")
            if isinstance(feeds, list):
                urls = [u for u in feeds if isinstance(u, str) and u.strip()]
                if urls:
                    out[category] = urls
    return out or {k: list(v) for k, v in DEFAULT_FEEDS.items()}


# --------------------------------------------------------------------------- #
# Parsing helpers.
# --------------------------------------------------------------------------- #
def _host_of(url: str) -> str:
    m = re.match(r"https?://([^/]+)/?", url or "")
    return m.group(1).lower() if m else ""


def _source_label(feed_url: str, channel_title: str | None) -> str:
    host = _host_of(feed_url)
    if host in SOURCE_LABELS:
        return SOURCE_LABELS[host]
    if channel_title:
        # Trim noise like "NPR Topics: Business" -> "NPR".
        return channel_title.split(":")[0].split(" - ")[0].strip()[:32]
    return host or "unknown"


def _clean_text(value: str | None) -> str:
    if not value:
        return ""
    text = html.unescape(value)
    text = _TAG_RE.sub(" ", text)
    text = _WS_RE.sub(" ", text)
    return text.strip()


def _first_sentence(value: str | None, limit: int = 220) -> str:
    text = _clean_text(value)
    if not text:
        return ""
    m = re.search(r"(.+?[.!?])(\s|$)", text)
    sentence = m.group(1) if m else text
    if len(sentence) > limit:
        sentence = sentence[: limit - 1].rstrip() + "…"
    return sentence


def _parse_published(raw: str | None) -> _dt.datetime | None:
    if not raw:
        return None
    raw = raw.strip()
    # RFC 822 (RSS pubDate), incl. odd minute-only forms.
    try:
        dt = parsedate_to_datetime(raw)
        if dt is not None:
            return dt if dt.tzinfo else dt.replace(tzinfo=_dt.timezone.utc)
    except (TypeError, ValueError, IndexError):
        pass
    # ISO 8601 (Atom published/updated, dc:date).
    iso = raw.replace("Z", "+00:00")
    try:
        dt = _dt.datetime.fromisoformat(iso)
        return dt if dt.tzinfo else dt.replace(tzinfo=_dt.timezone.utc)
    except ValueError:
        return None


def _strip_ns(tag: str) -> str:
    return tag.split("}", 1)[-1] if "}" in tag else tag


def _atom_link(entry: ET.Element) -> str:
    """Pick the alternate (article) link from an Atom entry."""
    alt = ""
    first = ""
    for link in entry.findall(f"{ATOM}link"):
        href = link.get("href", "")
        if not href:
            continue
        rel = link.get("rel", "alternate")
        if rel == "alternate":
            alt = href
            break
        if not first:
            first = href
    return alt or first


def parse_feed(feed_url: str, body: bytes, category: str) -> list[dict]:
    """Parse one feed body into normalized item dicts. Tolerant of RSS & Atom."""
    root = ET.fromstring(body)
    tag = _strip_ns(root.tag)
    items: list[dict] = []

    if tag == "feed":  # Atom
        channel_title = None
        ct = root.find(f"{ATOM}title")
        if ct is not None:
            channel_title = _clean_text(ct.text)
        source = _source_label(feed_url, channel_title)
        for entry in root.findall(f"{ATOM}entry"):
            title_el = entry.find(f"{ATOM}title")
            title = _clean_text(title_el.text if title_el is not None else "")
            link = _atom_link(entry)
            if not title or not link:
                continue
            pub_el = entry.find(f"{ATOM}published")
            if pub_el is None:
                pub_el = entry.find(f"{ATOM}updated")
            published_raw = pub_el.text if pub_el is not None else None
            summ_el = entry.find(f"{ATOM}summary")
            if summ_el is None:
                summ_el = entry.find(f"{ATOM}content")
            descr = summ_el.text if summ_el is not None else ""
            items.append(_mk_item(title, link, published_raw, source, category, descr))
        return items

    # RSS: root is <rss>; items live under <channel>.
    channel = root.find("channel")
    scope = channel if channel is not None else root
    channel_title = None
    ct = scope.find("title")
    if ct is not None:
        channel_title = _clean_text(ct.text)
    source = _source_label(feed_url, channel_title)
    for item in scope.findall("item"):
        title_el = item.find("title")
        title = _clean_text(title_el.text if title_el is not None else "")
        link_el = item.find("link")
        link = (link_el.text or "").strip() if link_el is not None else ""
        if not title or not link:
            continue
        pub_el = item.find("pubDate")
        published_raw = pub_el.text if pub_el is not None else None
        if not published_raw:
            # dc:date fallback (namespaced).
            for child in item:
                if _strip_ns(child.tag) == "date":
                    published_raw = child.text
                    break
        descr_el = item.find("description")
        descr = descr_el.text if descr_el is not None else ""
        items.append(_mk_item(title, link, published_raw, source, category, descr))
    return items


def _mk_item(title, link, published_raw, source, category, descr) -> dict:
    dt = _parse_published(published_raw)
    flag = "alert" if _ALERT_RE.search(title) else "normal"
    return {
        "title": title,
        "url": link.strip(),
        "source": source,
        "category": category,
        "published": dt.astimezone(_dt.timezone.utc).isoformat() if dt else "",
        "_sort_ts": dt.timestamp() if dt else 0.0,
        "_descr": _first_sentence(descr),
        "flag": flag,
        "summary": "",  # filled by enhancement / extractive fallback below
    }


def fetch_feed(feed_url: str, category: str, link: "HostLink | None") -> list[dict]:
    req = urllib.request.Request(feed_url, headers={"User-Agent": USER_AGENT})
    with urllib.request.urlopen(req, timeout=FETCH_TIMEOUT_S) as resp:
        body = resp.read()
    return parse_feed(feed_url, body, category)


# --------------------------------------------------------------------------- #
# Optional local-LLM enhancement (graceful; never required).
# --------------------------------------------------------------------------- #
class InferenceClient:
    """Minimal JSONL client for the daemon-mediated generate PROXY (op=generate
    ONLY), at state/ipc/apps/generate.sock.

    Every request stamps this app's name + the JARVIS_APP_TOKEN capability token
    the daemon minted for this launch; the proxy verifies the token, caps
    max_tokens, rate-limits, and forwards to the inference server. Best-effort:
    any connect/read/parse failure — or an ok=false reply (op_not_permitted,
    unauthorized, rate_limited, inference_unavailable) — raises so the caller
    falls back to extractive summaries. Never imports torch or any heavy dep."""

    def __init__(self, sock_path: Path, name: str, token: str) -> None:
        self._sock_path = str(sock_path)
        self._name = name
        self._token = token

    def available(self) -> bool:
        # No token means we are not running under the daemon (e.g. --selftest):
        # treat the proxy as unavailable so we go straight to extractive.
        if not self._token:
            return False
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.settimeout(LLM_CONNECT_TIMEOUT_S)
            s.connect(self._sock_path)
            s.close()
            return True
        except OSError:
            return False

    def generate(self, text: str, max_tokens: int) -> str:
        # Proxy request shape (CONTRACT part B): name + token + op=generate +
        # text + max_tokens. The proxy clamps max_tokens to its own cap (256).
        req = {
            "name": self._name,
            "token": self._token,
            "op": "generate",
            "text": text,
            "max_tokens": int(max_tokens),
        }
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.settimeout(LLM_CONNECT_TIMEOUT_S)
        try:
            s.connect(self._sock_path)
            s.settimeout(LLM_READ_TIMEOUT_S)
            s.sendall((json.dumps(req) + "\n").encode("utf-8"))
            rfile = s.makefile("r", encoding="utf-8", newline="\n")
            line = rfile.readline()
            if not line:
                raise OSError("empty response from generate proxy")
            resp = json.loads(line)
            # ok=false (any error) -> raise so the caller falls back.
            if not resp.get("ok"):
                raise OSError(resp.get("error", "generate proxy rejected the request"))
            return (resp.get("text") or "").strip()
        finally:
            s.close()


def _summary_prompt(item: dict) -> str:
    base = item["title"]
    if item.get("_descr"):
        base += " — " + item["_descr"]
    return (
        "Summarize this news headline in ONE neutral, factual sentence "
        "(max 22 words). No opinion, no speculation, no preamble. "
        "Output only the sentence.\n\n" + base
    )


def _brief_prompt(items: list[dict]) -> str:
    headlines = "\n".join(f"- [{it['source']}] {it['title']}" for it in items[:8])
    return (
        "These are the top current headlines from public news feeds. Write a "
        "neutral 2-sentence situational brief of what is happening right now. "
        "Factual and concise, no opinion, no prediction. Output only the brief.\n\n"
        + headlines
    )


def enhance(items: list[dict], link: "HostLink | None") -> tuple[str, bool]:
    """Add neutral summaries + an overall brief. Returns (brief, used_llm).

    Falls back to extractive (headline + first feed sentence) on any failure.
    ALWAYS leaves every item with a non-empty real-content summary."""
    used_llm = False
    # Name + capability token from the launch env. Under --selftest neither is
    # set, so the token is "" and the proxy is treated as unavailable (straight
    # to extractive). Under the daemon both are present and stamp every request.
    app_name = os.environ.get("JARVIS_APP_NAME", APP_NAME)
    app_token = os.environ.get("JARVIS_APP_TOKEN", "")
    client = InferenceClient(LLM_SOCKET, app_name, app_token)
    llm_ok = client.available()

    if llm_ok:
        for it in items[:LLM_SUMMARY_MAX_ITEMS]:
            try:
                s = client.generate(_summary_prompt(it), LLM_SUMMARY_MAX_TOKENS)
                s = _clean_text(s)
                if s:
                    it["summary"] = s[:240]
                    used_llm = True
            except (OSError, ValueError, json.JSONDecodeError):
                llm_ok = False
                if link is not None:
                    link.log("inference enhancement unavailable; using extractive summaries")
                break

    # Extractive fallback for anything still without a summary.
    for it in items:
        if not it["summary"]:
            it["summary"] = it["_descr"] or it["title"]

    # Overall brief.
    brief = ""
    if used_llm and llm_ok:
        try:
            brief = _clean_text(client.generate(_brief_prompt(items), LLM_BRIEF_MAX_TOKENS))
        except (OSError, ValueError, json.JSONDecodeError):
            brief = ""
    if not brief:
        cats = sorted({it["category"] for it in items})
        brief = (
            f"Aggregating {len(items)} latest items across "
            f"{', '.join(cats)} from public feeds. Summaries are neutral; "
            "no prediction, no surveillance."
        )
    return brief, used_llm


# --------------------------------------------------------------------------- #
# One poll cycle.
# --------------------------------------------------------------------------- #
def run_cycle(feeds: dict[str, list[str]], link: "HostLink | None") -> dict:
    """Fetch all feeds, dedupe, rank, enhance, and (if linked) emit to host.

    Returns a result dict (also returned for the in-process self-test)."""
    collected: list[dict] = []
    feeds_ok = 0
    feeds_failed = 0
    errors: list[str] = []

    for category, urls in feeds.items():
        for url in urls:
            try:
                items = fetch_feed(url, category, link)
                collected.extend(items)
                feeds_ok += 1
            except (urllib.error.URLError, ET.ParseError, OSError, ValueError) as exc:
                feeds_failed += 1
                errors.append(f"{_host_of(url)}: {exc}")
                if link is not None:
                    link.log(f"feed failed {_host_of(url)}: {exc}")

    # Dedupe by URL, keeping the first (and richer) occurrence.
    seen: set[str] = set()
    deduped: list[dict] = []
    for it in collected:
        key = it["url"]
        if key in seen:
            continue
        seen.add(key)
        deduped.append(it)

    # Newest-first; undated items sink to the bottom.
    deduped.sort(key=lambda it: it["_sort_ts"], reverse=True)
    top = deduped[:TOP_N]

    brief, used_llm = enhance(top, link)

    fetched_at = _dt.datetime.now(_dt.timezone.utc).isoformat()
    public_items = []
    for it in top:
        public_items.append(
            {
                "title": it["title"],
                "source": it["source"],
                "url": it["url"],
                "published": it["published"],
                "category": it["category"],
                "summary": it["summary"],
                "flag": it["flag"],
            }
        )

    if link is not None:
        link.send(
            "items",
            {"brief": brief, "items": public_items, "fetched_at": fetched_at},
        )
        link.send("status", {"feeds_ok": feeds_ok, "feeds_failed": feeds_failed})

    return {
        "brief": brief,
        "items": public_items,
        "fetched_at": fetched_at,
        "feeds_ok": feeds_ok,
        "feeds_failed": feeds_failed,
        "used_llm": used_llm,
        "errors": errors,
    }


# --------------------------------------------------------------------------- #
# Main run loop (sandboxed mode, driven by the host).
# --------------------------------------------------------------------------- #
def main() -> int:
    sock_path = os.environ.get("JARVIS_APP_SOCKET")
    token = os.environ.get("JARVIS_APP_TOKEN")
    if not sock_path or not token:
        sys.stderr.write(
            "global-scan: JARVIS_APP_SOCKET and JARVIS_APP_TOKEN must be set "
            "(this app runs under jarvisd, not standalone)\n"
        )
        return 2

    try:
        link = HostLink(sock_path, token)
    except OSError as exc:
        sys.stderr.write(f"global-scan: cannot connect to host socket: {exc}\n")
        return 1

    feeds = load_feeds()
    n_feeds = sum(len(v) for v in feeds.values())
    link.log(f"global-scan online: {n_feeds} feeds across {len(feeds)} categories")

    stop_event = threading.Event()
    refresh_event = threading.Event()
    running = threading.Event()
    running.set()  # autostart polling on launch

    def poll_loop() -> None:
        # Immediate first cycle, then interval-or-refresh driven.
        while not stop_event.is_set():
            if running.is_set():
                try:
                    run_cycle(feeds, link)
                except Exception as exc:  # never let the loop die on one bad cycle
                    link.log(f"cycle error: {exc}")
            # Wait for the interval, an explicit refresh, or stop.
            refresh_event.wait(timeout=REFRESH_INTERVAL_S)
            refresh_event.clear()

    worker = threading.Thread(target=poll_loop, name="global-scan-poll", daemon=True)
    worker.start()

    # Command reader (blocks on the socket until the host closes it).
    try:
        for cmd in link.commands():
            ctype = cmd.get("type")
            if ctype == "stop":
                break
            if ctype == "refresh":
                running.set()
                refresh_event.set()
            elif ctype == "start":
                running.set()
                refresh_event.set()
    except OSError:
        pass
    finally:
        stop_event.set()
        refresh_event.set()
        worker.join(timeout=2.0)
        link.log("global-scan stopping")
        link.close()
    return 0


# --------------------------------------------------------------------------- #
# In-process self-test (no sandbox, no host socket): real read-only fetch.
# --------------------------------------------------------------------------- #
def selftest() -> int:
    feeds = load_feeds()
    result = run_cycle(feeds, link=None)
    total = result["feeds_ok"] + result["feeds_failed"]
    print(f"feeds resolved: {result['feeds_ok']}/{total}")
    if result["errors"]:
        print("failures:")
        for e in result["errors"]:
            print(f"  - {e}")
    print(f"items after dedupe/rank: {len(result['items'])}")
    print(f"llm enhancement used: {result['used_llm']}")
    print(f"brief: {result['brief']}")
    if result["items"]:
        s = result["items"][0]
        print("sample item:")
        print(f"  [{s['category']}/{s['source']}] {s['title']}")
        print(f"  published: {s['published'] or '(undated)'}  flag: {s['flag']}")
        print(f"  url: {s['url']}")
        print(f"  summary: {s['summary']}")
    return 0 if result["feeds_ok"] > 0 else 1


if __name__ == "__main__":
    if "--selftest" in sys.argv:
        raise SystemExit(selftest())
    raise SystemExit(main())

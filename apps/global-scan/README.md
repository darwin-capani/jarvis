# Global-Scan

First micro-app on the JARVIS micro-app runtime substrate (`docs/SANDBOX.md`).
A world **intel feed aggregator**: it polls open, reputable, non-paywalled
RSS/Atom feeds, dedupes and ranks the latest items newest-first, optionally adds
a neutral one-line summary per top item plus a short overall brief from the
local LLM, and renders an intel digest panel in the HUD.

Honest framing: it **aggregates and summarizes public syndication feeds**. It
predicts nothing and surveils no one.

## Files

| File | Purpose |
|---|---|
| `manifest.toml` | Sandbox manifest (SANDBOX.md schema). `net_hosts` lists exactly the feed hostnames in `feeds.toml`. |
| `feeds.toml` | Category → list of RSS/Atom feed URLs. Every URL verified to return a parseable feed over HTTPS on 2026-06-13. |
| `main.py` | The app. Runs under `jarvisd` + `sandbox-exec`; reads `JARVIS_APP_SOCKET` / `JARVIS_APP_TOKEN` from env. |

## How it runs

`jarvisd` launches `main.py` under a generated seatbelt profile and hands it a
per-launch capability token. The app:

1. Connects to its per-app Unix socket (`state/ipc/apps/global-scan.sock`).
2. Loads `feeds.toml` (falls back to a built-in default set if missing).
3. Polls every feed over HTTPS (stdlib `urllib` + `xml.etree`, no heavy deps),
   parses title/link/published/source, dedupes by URL, sorts newest-first,
   keeps the top 20.
4. If the daemon-mediated generate proxy at `state/ipc/apps/generate.sock` is
   reachable, asks the local LLM through it (`op=generate` only, token-gated,
   256-token cap, rate-limited — the raw `inference.sock` is not reachable) with
   a neutral instruction and low `max_tokens` for a one-line summary per top
   item and a 2-sentence brief; on any failure falls back to extractive summaries
   (headline + first feed sentence). Real headline/source/time always shown.
5. Emits to the host socket, every line stamped with the token:
   - `{"type":"items","data":{"brief","items":[…],"fetched_at"}}`
   - `{"type":"status","data":{"feeds_ok","feeds_failed"}}`
6. Re-polls every ~10 min or on a host `refresh` command; stops on `stop`.

Each item: `{title, source, url, published, category, summary, flag}` where
`flag` is `"alert"` for breaking/urgent headlines (drives the HUD red accent),
else `"normal"`.

## Self-test (no sandbox, real read-only fetch)

```sh
.venv/bin/python3.11 apps/global-scan/main.py --selftest
```

Reports how many feeds resolved, item count after dedupe/rank, whether LLM
enhancement was used, the brief, and a sample parsed item. Read-only network.

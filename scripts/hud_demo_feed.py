#!/usr/bin/env python3
"""Fake darwind telemetry feed for HUD development.

Serves ws://127.0.0.1:7177 (the daemon's telemetry port — do NOT run this
while darwind is running) and streams a realistic event loop: system.load,
audio.level at ~15Hz, and a full spoken-exchange sequence every ~12s. Lets
the HUD be developed and visually verified without the daemon, mic, or
models.

Usage: .venv/bin/python scripts/hud_demo_feed.py
"""

import asyncio
import json
import math
import os
import time

import websockets

PORT = 7177
START = time.time()

# Optional headless-verification freeze: when DARWIN_DEMO_HOLD=<agent> is set,
# the feed runs the roll-call once, then holds that ONE agent active (and stops
# the per-exchange delegation rotation) so a screenshot deterministically shows
# the CONSTELLATION // AGENTS panel highlighting it and the core in its hue.
# Unset (the default) -> the normal cycling demo loop. No behavior change to the
# shipped demo; this is a verification convenience only.
HOLD_AGENT = (os.environ.get("DARWIN_DEMO_HOLD") or "").strip().lower() or None


def env(source, event, data):
    return json.dumps(
        {
            "ts": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
            "source": source,
            "event": event,
            "data": data,
        }
    )


# Sample global-scan micro-app feed (build contract part D). The daemon's app
# host relays each app's items as ("system","app.data",{name,topic,payload}); a
# real feed is fetched/parsed/summarized by apps/global-scan/main.py. One item
# is flagged "breaking" to exercise the panel's alert-red accent (red is
# reserved for alerts only). Published times are made recent at send time so
# the relative-time chips ("3M", "1H") read naturally.
GLOBAL_SCAN_BRIEF = (
    "Aggregating public, non-paywalled feeds (NPR, BBC, Ars Technica, HN). "
    "Summaries only — no prediction, no surveillance."
)
GLOBAL_SCAN_ITEMS = [
    {
        "title": "Senate advances long-debated infrastructure package in late-night vote",
        "source": "NPR",
        "category": "breaking",
        "age_min": 4,
        "summary": "Procedural vote clears the way for a final floor vote later this week.",
    },
    {
        "title": "New on-device transformer runtime claims 3x token throughput on Apple silicon",
        "source": "Ars Technica",
        "category": "tech",
        "age_min": 38,
        "summary": "Benchmarks credit a reworked KV-cache layout and ANE offload path.",
    },
    {
        "title": "Markets close mixed as energy gains offset a tech pullback",
        "source": "BBC",
        "category": "markets",
        "age_min": 95,
        "summary": "Major indices finished little changed in light pre-holiday trading.",
    },
    {
        "title": "Show HN: a tiny seatbelt-profile generator for sandboxing CLI tools",
        "source": "Hacker News",
        "category": "tech",
        "age_min": 142,
        "summary": "Derives a default-deny SBPL profile from a declared permission manifest.",
    },
]


# Agent constellation roll-call (CONTRACT part C). Mirrors config/agents.toml /
# hud/src/core/agents.ts: name | role | hue (0-360 for the HUD core). The demo
# walks agent.active through several agents so the CONSTELLATION // AGENTS panel
# highlights each in turn, the status-bar ACTIVE chip recolors, and the R3F core
# hue lerps per agent. NOTE: ultron's identity hue is deep-orange 15 — NOT the
# reserved alert-red — so the roll-call never lights the core red.
ROLL_CALL = [
    ("darwin", "Prime Orchestrator", 190),
    ("friday", "Daily Intel", 35),
    ("veronica", "Content + Comms", 320),
    ("vision", "Research + OSINT", 265),
    ("ultron", "Security + Automation", 15),
    ("stark", "Business Intel", 205),
    ("steve", "CTO + Builds", 150),
    ("gecko", "Markets + Capital", 120),
    ("hercules", "Fitness + Nutrition", 90),
    ("jerome", "Leisure + DJ", 340),
]


# Per-exchange delegation samples (CONTRACT part C). Rotated across exchanges
# so the active agent — and thus the core hue + ACTIVE chip — varies turn to
# turn after the opening roll-call. (name, role, hue) drawn from ROLL_CALL.
DELEGATIONS = [
    ("darwin", "Prime Orchestrator", 190),
    ("vision", "Research + OSINT", 265),
    ("steve", "CTO + Builds", 150),
    ("friday", "Daily Intel", 35),
    ("gecko", "Markets + Capital", 120),
]


async def roll_call(ws, hold=1.4):
    """Emit agent.active for each agent in turn (the reel centerpiece).

    Each step lights one agent so the constellation panel + core color cycle.
    `hold` paces the sweep so the lerped core-hue transition is visible. No
    audio is played and no spoken intro is synthesized here — this is the HUD
    verification path only; the daemon owns the real per-voice introductions.
    """
    for name, role, hue in ROLL_CALL:
        await ws.send(env("system", "agent.active", {"name": name, "role": role, "hue": hue}))
        await asyncio.sleep(hold)


def global_scan_payload():
    """Build a fresh app.data payload with recent published timestamps."""
    now = time.time()
    items = []
    for it in GLOBAL_SCAN_ITEMS:
        published = time.strftime(
            "%Y-%m-%dT%H:%M:%S%z", time.localtime(now - it["age_min"] * 60)
        )
        items.append(
            {
                "title": it["title"],
                "source": it["source"],
                "url": f"https://example.invalid/{it['source'].lower().replace(' ', '')}",
                "published": published,
                "category": it["category"],
                "summary": it["summary"],
            }
        )
    return {
        "brief": GLOBAL_SCAN_BRIEF,
        "items": items,
        "fetched_at": time.strftime("%Y-%m-%dT%H:%M:%S%z", time.localtime(now)),
    }


async def feed(ws):
    print("HUD connected")
    await ws.send(env("system", "daemon.started", {"root": "/demo", "cloud_key_present": False}))
    # Bring the Global-Scan panel online and push an initial intel feed.
    await ws.send(env("system", "app.started", {"name": "global-scan"}))
    await ws.send(
        env("system", "app.data", {"name": "global-scan", "topic": "feed", "payload": global_scan_payload()})
    )
    await ws.send(
        env("system", "app.data", {"name": "global-scan", "topic": "feed", "payload": {"feeds_ok": 4, "feeds_failed": 0}})
    )
    # Agent constellation (CONTRACT part C): introduce the team once on connect
    # so the CONSTELLATION // AGENTS panel, the ACTIVE status chip, and the
    # per-agent core hue are immediately verifiable headless. Released back to
    # the idle cyan afterward by the pipeline.completed at the end of the first
    # exchange (and re-driven per-exchange below via DELEGATIONS).
    await roll_call(ws)
    # Headless-verification freeze: hold one agent active and stop here so a
    # screenshot is deterministic (the normal cycling loop below is skipped).
    if HOLD_AGENT is not None:
        held = next((e for e in ROLL_CALL if e[0] == HOLD_AGENT), None)
        if held is None:
            held = next((d for d in DELEGATIONS if d[0] == HOLD_AGENT), ROLL_CALL[0])
        name, role, hue = held
        print(f"HOLD mode: pinning agent.active -> {name} (hue {hue})")
        while True:
            await ws.send(env("system", "agent.active", {"name": name, "role": role, "hue": hue}))
            # Keep a touch of audio life so the core renders animated, but never
            # re-emit pipeline.completed/idle (that would clear the active agent).
            await ws.send(env("system", "audio.level", {"rms": 0.18, "peak": 0.3}))
            await asyncio.sleep(0.5)
    # Self-heal v2 review panel (HUD build B). Walk the warn-amber surface
    # through DIAGNOSING -> PROPOSAL once, a few seconds in, so the
    # SELF-REPAIR // PROPOSALS panel is verifiable headless. These are demo
    # values only — no patch is staged or applied; the panel is display-and-
    # guide (it surfaces `scripts/apply_heal.sh <ts>` for the human to run).
    heal_phase = 0  # 0=not yet, 1=diagnosing emitted, 2=proposal emitted
    start_wall = time.time()
    heal_ts = int(start_wall)  # the staging <ts> for the apply command

    seq = 0
    last_load = 0.0
    last_exchange = 0.0
    last_scan = time.time()
    speaking_until = 0.0
    try:
        while True:
            now = time.time()
            t = now - START
            speaking = now < speaking_until

            # Self-heal v2: emit DIAGNOSING ~5s in, then the validated PROPOSAL
            # ~3s later, so the warn-amber SELF-REPAIR // PROPOSALS panel is
            # exercised end-to-end. confidence drives the segmented gauge.
            since_start = now - start_wall
            if heal_phase == 0 and since_start > 5.0:
                heal_phase = 1
                await ws.send(
                    env(
                        "system",
                        "heal.diagnosing",
                        {
                            "signature": "thread 'audio-capture' panicked: device disconnected (cpal BuildStreamError)",
                            "subsystem": "audio",
                            "files": ["src/audio.rs:212", "src/speech.rs:88"],
                        },
                    )
                )
            elif heal_phase == 1 and since_start > 8.0:
                heal_phase = 2
                await ws.send(
                    env(
                        "system",
                        "heal.proposal",
                        {
                            "ts": heal_ts,
                            "files": ["src/audio.rs"],
                            "validated": True,
                            "confidence": 0.82,
                            "subsystem": "audio",
                            "signature": "device disconnected (cpal BuildStreamError)",
                        },
                    )
                )

            # ~15Hz mic level: quiet floor with speech bursts during exchanges
            burst = 0.05 * max(0.0, math.sin(t * 1.8)) if (t % 12.0) < 2.2 else 0.0
            rms = round(0.004 + 0.003 * math.sin(t * 7.0) + burst, 4)
            await ws.send(env("audio", "audio.level", {"rms": abs(rms), "speaking": speaking}))

            if now - last_load > 2.0:
                last_load = now
                await ws.send(
                    env(
                        "system",
                        "system.load",
                        {
                            "cpu_percent": 9.0 + 6.0 * abs(math.sin(t * 0.2)),
                            "mem_used_bytes": int(11.2e9),
                            "mem_total_bytes": int(17.18e9),
                            "disk_free_bytes": int(344e9),
                            "uptime_secs": int(t) + 86400 * 5,
                        },
                    )
                )

            # Refresh the intel feed periodically (the real app polls ~10 min;
            # the demo refreshes faster so the relative-time chips keep moving).
            if now - last_scan > 30.0:
                last_scan = now
                await ws.send(
                    env("system", "app.data", {"name": "global-scan", "topic": "feed", "payload": global_scan_payload()})
                )

            if now - last_exchange > 12.0:
                last_exchange = now
                seq += 1
                await ws.send(env("audio", "utterance.captured", {"path": f"/demo/u{seq}.wav"}))
                await asyncio.sleep(0.35)
                await ws.send(env("local", "stt.transcript", {"text": "What's my system status?"}))
                await ws.send(
                    env(
                        "local",
                        "intent.classified",
                        {"intent": "system.query", "confidence": 0.97, "complexity": "light"},
                    )
                )
                # Darwin-Prime delegates this turn to the next agent in the
                # rotation; the HUD lights it in the constellation, recolors the
                # ACTIVE chip, and lerps the core to its hue. Released back to
                # cyan by pipeline.completed at the end of the exchange.
                ag_name, ag_role, ag_hue = DELEGATIONS[(seq - 1) % len(DELEGATIONS)]
                await ws.send(
                    env("system", "agent.active", {"name": ag_name, "role": ag_role, "hue": ag_hue})
                )
                await ws.send(
                    env("local", "route.local", {"intent": "system.query", "confidence": 0.97})
                )
                await asyncio.sleep(0.8)
                response = "Right away, sir. CPU at 11 percent. Memory, 11.2 of 16 gigabytes. All systems are green."
                await ws.send(env("local", "response.speaking", {"text": response}))
                speaking_until = now + 6.0
                await ws.send(env("local", "route.completed", {"routed_to": "local", "response": response}))
                if seq % 2 == 0:
                    await ws.send(
                        env("system", "memory.learned", {"key": "user.habit.status_check", "value": "asks for status often"})
                    )
                if seq % 3 == 0:
                    await ws.send(
                        env("system", "action.executed", {"tool": "open_url", "outcome": "Opened apple.com in Comet (the default browser)."})
                    )
                await ws.send(
                    env(
                        "system",
                        "pipeline.completed",
                        {"stt_ms": 340, "classify_ms": 780, "route_ms": 1230, "speak_ms": 5200, "first_audio_ms": 410, "total_ms": 7600},
                    )
                )
            await asyncio.sleep(0.066)
    except websockets.ConnectionClosed:
        print("HUD disconnected")


async def main():
    async with websockets.serve(feed, "127.0.0.1", PORT):
        print(f"demo feed on ws://127.0.0.1:{PORT} — Ctrl-C to stop")
        await asyncio.Future()


if __name__ == "__main__":
    asyncio.run(main())

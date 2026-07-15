import { describe, expect, it } from "vitest";
import type { TelemetryEnvelope } from "../core/events";
import { CONFIDENCE_SEGMENTS, confidencePct, litSegments } from "../core/heal";
import {
  APP_FEED_ITEM_CAP,
  ENTER_FRAMES_TO_LISTEN,
  EVIDENCE_REFRESH_MS,
  HudState,
  LISTEN_ENTER_RMS,
  LISTEN_EXIT_RMS,
  OP_FORWARD_OUTCOME_CAP,
  QUIET_FRAMES_TO_IDLE,
  STALE_STATE_MS,
  TICKER_CAP,
  TOAST_CAP,
  TOAST_EXIT_MS,
  TOAST_TTL_MS,
  TRANSCRIPT_CAP,
  initialState,
  reduce,
} from "../core/state";

/* helpers ------------------------------------------------------------------ */

let counter = 0;
function env(
  event: string,
  data: Record<string, unknown> = {},
  source = "system",
): TelemetryEnvelope {
  counter += 1;
  return { ts: `2026-06-12T12:00:${String(counter % 60).padStart(2, "0")}Z`, source, event, data };
}

function tel(state: HudState, e: TelemetryEnvelope, at = 1000): HudState {
  return reduce(state, { type: "telemetry", envelope: e, at });
}

function connected(at = 0): HudState {
  return reduce(initialState(), { type: "ws.connected", at });
}

function audioLevel(rms: number, speaking = false, source = "audio"): TelemetryEnvelope {
  return env("audio.level", { rms, speaking }, source);
}

const LOUD = 0.05; // comfortably above LISTEN_ENTER_RMS
const QUIET = 0.001; // comfortably below LISTEN_EXIT_RMS

/** Feed loud frames until the dwell promotes idle -> listening. */
function listenAfterDwell(s: HudState, at = 1000): HudState {
  for (let i = 0; i < ENTER_FRAMES_TO_LISTEN; i++) {
    s = tel(s, audioLevel(LOUD, false), at);
  }
  return s;
}

/* connection --------------------------------------------------------------- */

describe("connection state", () => {
  it("starts offline", () => {
    expect(initialState().coreState).toBe("offline");
    expect(initialState().connected).toBe(false);
  });

  it("ws.connected: offline -> idle", () => {
    const s = connected();
    expect(s.connected).toBe(true);
    expect(s.coreState).toBe("idle");
  });

  it("ws.disconnected: any state -> offline", () => {
    let s = connected();
    s = tel(s, env("utterance.captured", { path: "/tmp/u.wav" }, "audio"));
    expect(s.coreState).toBe("processing");
    s = reduce(s, { type: "ws.disconnected", at: 2000 });
    expect(s.connected).toBe(false);
    expect(s.coreState).toBe("offline");
  });

  it("ws.disconnected while already offline returns the SAME reference (no churn)", () => {
    let s = connected();
    s = reduce(s, { type: "ws.disconnected", at: 1000 });
    expect(s.coreState).toBe("offline");
    // every failed backoff attempt re-fires onClose — must not re-render
    expect(reduce(s, { type: "ws.disconnected", at: 2000 })).toBe(s);
    const init = initialState();
    expect(reduce(init, { type: "ws.disconnected", at: 0 })).toBe(init);
  });

  it("ws.disconnected clears the active agent (no phantom ACTIVE chip after reconnect)", () => {
    let s = connected();
    // An agent lights up mid-turn.
    s = tel(s, env("agent.active", { name: "vision" }));
    expect(s.activeAgent).not.toBeNull();
    // The link drops BEFORE the turn's terminal pipeline.completed/route.failed.
    s = reduce(s, { type: "ws.disconnected", at: 2000 });
    expect(s.activeAgent).toBeNull();
    // Reconnect lands in idle; idle is not TRANSIENT, so the stale-decay never
    // runs — activeAgent must stay cleared (else the "ACTIVE: <agent>" chip + agent
    // core hue would persist forever).
    s = reduce(s, { type: "ws.connected", at: 3000 });
    expect(s.activeAgent).toBeNull();
  });

  it("reconnect: offline -> idle again", () => {
    let s = connected();
    s = reduce(s, { type: "ws.disconnected", at: 1 });
    s = reduce(s, { type: "ws.connected", at: 2 });
    expect(s.coreState).toBe("idle");
  });
});

/* core transitions: hysteresis + dwell (anti-flash directive #5) ------------- */

describe("listening hysteresis and dwell", () => {
  it("a SINGLE loud frame does not promote idle -> listening (enter dwell)", () => {
    const s = tel(connected(), audioLevel(LOUD, false));
    expect(s.coreState).toBe("idle");
    expect(s.loudStreak).toBe(1);
  });

  it("promotes after ENTER_FRAMES_TO_LISTEN consecutive loud frames", () => {
    const s = listenAfterDwell(connected());
    expect(s.coreState).toBe("listening");
  });

  it("a sub-enter frame resets the loud streak", () => {
    let s = tel(connected(), audioLevel(LOUD, false));
    s = tel(s, audioLevel(0.005, false)); // dips before the dwell completes
    expect(s.loudStreak).toBe(0);
    s = tel(s, audioLevel(LOUD, false));
    expect(s.coreState).toBe("idle"); // streak restarted, still 1 frame in
  });

  it("rms at exactly LISTEN_ENTER_RMS does not count toward entering", () => {
    let s = connected();
    for (let i = 0; i < 10; i++) s = tel(s, audioLevel(LISTEN_ENTER_RMS, false));
    expect(s.coreState).toBe("idle");
  });

  it("rms hovering at the OLD 0.015 threshold never oscillates states", () => {
    // The original bug: enter and exit shared 0.015, so ambient noise
    // alternating 0.0149/0.0151 cycled listening<->idle at ~1Hz.
    let s = connected();
    for (let i = 0; i < 200; i++) {
      s = tel(s, audioLevel(i % 2 === 0 ? 0.0151 : 0.0149, false));
      expect(s.coreState).toBe("idle"); // below enter threshold: never promotes
    }
  });

  it("inside the hysteresis band, listening HOLDS (no exit, no thrash)", () => {
    let s = listenAfterDwell(connected());
    // 0.014 is below enter (0.018) but above exit (0.012): hold listening.
    for (let i = 0; i < 100; i++) {
      s = tel(s, audioLevel(0.014, false));
      expect(s.coreState).toBe("listening");
    }
  });

  it("exits to idle only after QUIET_FRAMES_TO_IDLE frames below the EXIT threshold (~600ms)", () => {
    let s = listenAfterDwell(connected());
    for (let i = 0; i < QUIET_FRAMES_TO_IDLE - 1; i++) {
      s = tel(s, audioLevel(QUIET, false));
      expect(s.coreState).toBe("listening");
    }
    s = tel(s, audioLevel(QUIET, false));
    expect(s.coreState).toBe("idle");
  });

  it("rms between exit and enter does not count as a quiet frame", () => {
    let s = listenAfterDwell(connected());
    for (let i = 0; i < QUIET_FRAMES_TO_IDLE - 1; i++) s = tel(s, audioLevel(QUIET, false));
    s = tel(s, audioLevel((LISTEN_EXIT_RMS + LISTEN_ENTER_RMS) / 2, false)); // band: resets streak
    for (let i = 0; i < QUIET_FRAMES_TO_IDLE - 1; i++) {
      s = tel(s, audioLevel(QUIET, false));
    }
    expect(s.coreState).toBe("listening");
  });

  it("a loud frame resets the quiet streak", () => {
    let s = listenAfterDwell(connected());
    for (let i = 0; i < QUIET_FRAMES_TO_IDLE - 1; i++) s = tel(s, audioLevel(QUIET, false));
    s = tel(s, audioLevel(LOUD, false)); // speech resumes
    for (let i = 0; i < QUIET_FRAMES_TO_IDLE - 1; i++) {
      s = tel(s, audioLevel(QUIET, false));
    }
    expect(s.coreState).toBe("listening");
  });

  it("audio.level with speaking=true (mic muted) never enters listening", () => {
    let s = connected();
    for (let i = 0; i < 10; i++) s = tel(s, audioLevel(0.5, true));
    expect(s.coreState).toBe("idle");
    expect(s.micMuted).toBe(true);
  });

  it("audio.level does not demote processing/thinking states", () => {
    let s = tel(connected(), env("utterance.captured", { path: "/p" }, "audio"));
    s = tel(s, audioLevel(0.4, false));
    expect(s.coreState).toBe("processing");
    s = tel(s, env("route.cloud", { intent: "x", confidence: 0.9, model: "m", deep_reasoning: false }, "cloud"));
    s = tel(s, audioLevel(0.4, false));
    expect(s.coreState).toBe("thinking-cloud");
  });

  it("returns the SAME reference for state-machine-invisible frames (15Hz storm fix)", () => {
    // idle + quiet frame: nothing visible changes — React must bail out.
    let s = connected();
    s = tel(s, audioLevel(QUIET, false)); // settle micMuted=false (already false)
    expect(tel(s, audioLevel(QUIET, false))).toBe(s);
    expect(tel(s, audioLevel(0.005, false))).toBe(s);
    // listening + steady loud frame inside the refresh window: same reference.
    let l = listenAfterDwell(connected(), 1000);
    const held = tel(l, audioLevel(LOUD, false), 1100);
    expect(held).toBe(l);
  });
});

/* event-driven transitions ---------------------------------------------------- */

describe("core state transitions", () => {
  it("utterance.captured -> processing", () => {
    const s = tel(connected(), env("utterance.captured", { path: "/p.wav" }, "audio"));
    expect(s.coreState).toBe("processing");
  });

  it("route.local -> thinking-local", () => {
    const s = tel(connected(), env("route.local", { intent: "system.query", confidence: 0.97 }, "local"));
    expect(s.coreState).toBe("thinking-local");
  });

  it("route.cloud -> thinking-cloud and records the model id", () => {
    const s = tel(
      connected(),
      env("route.cloud", { intent: "conversation", confidence: 0.4, model: "claude-x", deep_reasoning: true }, "cloud"),
    );
    expect(s.coreState).toBe("thinking-cloud");
    expect(s.cloudModel).toBe("claude-x");
  });

  it("route.cloud_failed falls back to thinking-local (daemon degrades locally)", () => {
    let s = tel(connected(), env("route.cloud", { intent: "i", confidence: 0.4, model: "m", deep_reasoning: false }, "cloud"));
    s = tel(s, env("route.cloud_failed", { intent: "i", error: "api down" }, "cloud"));
    expect(s.coreState).toBe("thinking-local");
    expect(s.lastError?.event).toBe("route.cloud_failed");
    expect(s.lastError?.detail).toBe("api down");
  });

  it("response.speaking -> speaking", () => {
    const s = tel(connected(), env("response.speaking", { text: "Certainly, sir." }, "local"));
    expect(s.coreState).toBe("speaking");
  });

  it("pipeline.completed -> idle with timings captured", () => {
    let s = tel(connected(), env("response.speaking", { text: "x" }, "local"));
    s = tel(
      s,
      env("pipeline.completed", {
        stt_ms: 640,
        classify_ms: 210,
        route_ms: 980,
        first_audio_ms: 310,
        speak_ms: 2150,
        total_ms: 3980,
      }),
    );
    expect(s.coreState).toBe("idle");
    expect(s.lastTimings).toEqual({
      sttMs: 640,
      classifyMs: 210,
      routeMs: 980,
      speakMs: 2150,
      firstAudioMs: 310,
      totalMs: 3980,
    });
  });

  it("pipeline.completed tolerates null first_audio_ms (Option<u64>)", () => {
    const s = tel(
      connected(),
      env("pipeline.completed", { stt_ms: 1, classify_ms: 2, route_ms: 3, first_audio_ms: null, speak_ms: 4, total_ms: 10 }),
    );
    expect(s.lastTimings?.firstAudioMs).toBeNull();
  });

  it("stt.empty -> idle", () => {
    let s = tel(connected(), env("utterance.captured", { path: "/p" }, "audio"));
    s = tel(s, env("stt.empty", { path: "/p" }, "local"));
    expect(s.coreState).toBe("idle");
  });

  it("route.failed -> idle with the error recorded", () => {
    let s = tel(connected(), env("route.local", { intent: "i", confidence: 1 }, "local"));
    s = tel(s, env("route.failed", { intent: "i", error: "boom" }));
    expect(s.coreState).toBe("idle");
    expect(s.lastError?.detail).toBe("boom");
  });

  it("full happy-path pipeline walks every state", () => {
    let s = connected();
    const seen: string[] = [s.coreState];
    const walk = (e: TelemetryEnvelope) => {
      s = tel(s, e);
      if (seen[seen.length - 1] !== s.coreState) seen.push(s.coreState);
    };
    walk(audioLevel(0.08, false));
    walk(audioLevel(0.08, false)); // enter dwell: 2 consecutive loud frames
    walk(env("utterance.captured", { path: "/p" }, "audio"));
    walk(env("stt.transcript", { text: "what's my cpu at" }, "local"));
    walk(env("intent.classified", { intent: "system.query", confidence: 0.98, complexity: "simple" }, "local"));
    walk(env("route.local", { intent: "system.query", confidence: 0.98 }, "local"));
    walk(env("route.completed", { routed_to: "local", response: "CPU is at 12 percent." }, "local"));
    walk(env("response.speaking", { text: "CPU is at 12 percent." }, "local"));
    walk(env("pipeline.completed", { stt_ms: 1, classify_ms: 1, route_ms: 1, first_audio_ms: 1, speak_ms: 1, total_ms: 5 }));
    expect(seen).toEqual([
      "idle",
      "listening",
      "processing",
      "thinking-local",
      "speaking",
      "idle",
    ]);
  });
});

/* 12s decay + evidence keepalive ---------------------------------------------- */

describe("stale-state decay", () => {
  it("a transient state decays to idle after 12s without events", () => {
    let s = tel(connected(), env("utterance.captured", { path: "/p" }, "audio"), 1000);
    expect(s.coreState).toBe("processing");
    s = reduce(s, { type: "tick", at: 1000 + STALE_STATE_MS - 1 });
    expect(s.coreState).toBe("processing");
    s = reduce(s, { type: "tick", at: 1000 + STALE_STATE_MS });
    expect(s.coreState).toBe("idle");
  });

  it("idle and offline never decay", () => {
    let s = connected();
    s = reduce(s, { type: "tick", at: 10 * STALE_STATE_MS });
    expect(s.coreState).toBe("idle");
    s = reduce(s, { type: "ws.disconnected", at: 0 });
    s = reduce(s, { type: "tick", at: 20 * STALE_STATE_MS });
    expect(s.coreState).toBe("offline");
  });

  it("15s of loud frames keeps listening alive (no mid-dictation blink)", () => {
    let s = listenAfterDwell(connected(0), 1000);
    expect(s.coreState).toBe("listening");
    // 15s of speech at the 66ms frame cadence, ticks every 250ms.
    for (let at = 1066; at <= 16000; at += 66) {
      s = tel(s, audioLevel(LOUD, false), at);
      if (at % 250 < 66) s = reduce(s, { type: "tick", at });
      expect(s.coreState).toBe("listening");
    }
  });

  it("15s of speaking=true frames keeps speaking alive (long TTS reply)", () => {
    let s = tel(connected(0), env("response.speaking", { text: "long reply" }, "local"), 1000);
    expect(s.coreState).toBe("speaking");
    for (let at = 1066; at <= 16000; at += 66) {
      s = tel(s, audioLevel(0.001, true), at);
      if (at % 250 < 66) s = reduce(s, { type: "tick", at });
      expect(s.coreState).toBe("speaking");
    }
  });

  it("listening WITHOUT loud evidence still decays after 12s (band-level ambient)", () => {
    let s = listenAfterDwell(connected(0), 1000);
    // ambient inside the hysteresis band holds listening but is not loud
    // evidence — the stale decay must still reclaim it eventually.
    for (let at = 1066; at <= 1000 + STALE_STATE_MS; at += 66) {
      s = tel(s, audioLevel(0.014, false), at);
    }
    s = reduce(s, { type: "tick", at: 1000 + STALE_STATE_MS });
    expect(s.coreState).toBe("idle");
  });

  it("evidence refresh is rate-limited (no per-frame state churn)", () => {
    let s = listenAfterDwell(connected(0), 1000);
    const since = s.stateSince;
    s = tel(s, audioLevel(LOUD, false), 1000 + EVIDENCE_REFRESH_MS - 1);
    expect(s.stateSince).toBe(since); // inside the window: untouched
    s = tel(s, audioLevel(LOUD, false), 1000 + EVIDENCE_REFRESH_MS);
    expect(s.stateSince).toBe(1000 + EVIDENCE_REFRESH_MS); // refreshed
  });
});

/* transcript ------------------------------------------------------------------ */

describe("transcript ring buffer", () => {
  it("records user and darwin lines in order", () => {
    let s = connected();
    s = tel(s, env("stt.transcript", { text: "hello" }, "local"));
    s = tel(s, env("route.completed", { routed_to: "cloud", response: "Good evening." }, "cloud"));
    expect(s.transcript.map((l) => [l.who, l.text])).toEqual([
      ["user", "hello"],
      ["darwin", "Good evening."],
    ]);
    expect(s.transcript[1].routedTo).toBe("cloud");
  });

  it("caps at TRANSCRIPT_CAP, dropping the oldest", () => {
    let s = connected();
    for (let i = 0; i < TRANSCRIPT_CAP + 7; i++) {
      s = tel(s, env("stt.transcript", { text: `line ${i}` }, "local"));
    }
    expect(s.transcript).toHaveLength(TRANSCRIPT_CAP);
    expect(s.transcript[0].text).toBe("line 7");
    expect(s.transcript[TRANSCRIPT_CAP - 1].text).toBe(`line ${TRANSCRIPT_CAP + 6}`);
  });

  it("line seqs stay unique and increasing past the cap (autoscroll key)", () => {
    let s = connected();
    for (let i = 0; i < TRANSCRIPT_CAP + 5; i++) {
      s = tel(s, env("stt.transcript", { text: `line ${i}` }, "local"));
    }
    const seqs = s.transcript.map((l) => l.seq);
    for (let i = 1; i < seqs.length; i++) expect(seqs[i]).toBeGreaterThan(seqs[i - 1]);
  });
});

/* gauges, intent, daemon metadata ---------------------------------------------- */

describe("system gauges and metadata", () => {
  it("system.load fills the gauges (disk may be null)", () => {
    const s = tel(
      connected(),
      env("system.load", {
        cpu_percent: 23.5,
        mem_used_bytes: 8e9,
        mem_total_bytes: 16e9,
        disk_free_bytes: null,
        uptime_secs: 12345,
      }),
    );
    expect(s.gauges).toEqual({
      cpuPercent: 23.5,
      memUsedBytes: 8e9,
      memTotalBytes: 16e9,
      diskFreeBytes: null,
      uptimeSecs: 12345,
    });
  });

  it("daemon.started records root and cloud_key_present, resets to idle", () => {
    let s = tel(connected(), env("route.local", { intent: "i", confidence: 1 }, "local"));
    s = tel(s, env("daemon.started", { root: "/Users/x/darwin", cloud_key_present: true }));
    expect(s.daemonRoot).toBe("/Users/x/darwin");
    expect(s.cloudKeyPresent).toBe(true);
    expect(s.coreState).toBe("idle");
  });

  it("daemon.started without cloud_key_present leaves it unknown (older daemon)", () => {
    const s = tel(connected(), env("daemon.started", { root: "/r" }));
    expect(s.cloudKeyPresent).toBeNull();
  });

  it("intent.classified updates the intent chip", () => {
    const s = tel(
      connected(),
      env("intent.classified", { intent: "web.search", confidence: 0.55, complexity: "simple" }, "local"),
    );
    expect(s.lastIntent).toEqual({ intent: "web.search", confidence: 0.55, complexity: "simple" });
  });
});

/* inference banner --------------------------------------------------------------- */

describe("inference.unavailable", () => {
  it("raises the banner and aborts to idle for transcribe/classify", () => {
    let s = tel(connected(), env("utterance.captured", { path: "/p" }, "audio"));
    s = tel(s, env("inference.unavailable", { op: "transcribe", error: "conn refused" }));
    expect(s.inferenceOffline).toBe(true);
    expect(s.coreState).toBe("idle");
  });

  it("keeps the pipeline alive for converse (daemon falls back in-persona)", () => {
    let s = tel(connected(), env("route.local", { intent: "conversation", confidence: 1 }, "local"));
    s = tel(s, env("inference.unavailable", { op: "converse", error: "down" }));
    expect(s.inferenceOffline).toBe(true);
    expect(s.coreState).toBe("thinking-local");
  });

  it("clears on events that PROVE the server responded", () => {
    let s = tel(connected(), env("inference.unavailable", { op: "extract_facts", error: "down" }));
    expect(s.inferenceOffline).toBe(true);
    s = tel(s, env("stt.transcript", { text: "hi" }, "local"));
    expect(s.inferenceOffline).toBe(false);
  });

  it("does NOT clear on opener.played (source local, but pre-STT) — banner blink fix", () => {
    // speech.rs emits opener.played with source "local" BEFORE STT ever
    // contacts the inference server; clearing on it made the banner blink
    // once per exchange while the server was down.
    let s = tel(connected(), env("inference.unavailable", { op: "transcribe", error: "down" }));
    s = tel(s, env("opener.played", { index: 2, text: "Sir?" }, "local"));
    expect(s.inferenceOffline).toBe(true);
    s = tel(s, env("utterance.captured", { path: "/p" }, "audio"));
    expect(s.inferenceOffline).toBe(true);
    s = tel(s, env("route.completed", { routed_to: "local", response: "x" }, "local"));
    expect(s.inferenceOffline).toBe(true);
  });

  it("clears on stt.empty and memory.learned (server round-trips)", () => {
    let s = tel(connected(), env("inference.unavailable", { op: "transcribe", error: "down" }));
    expect(tel(s, env("stt.empty", { path: "/p" }, "local")).inferenceOffline).toBe(false);
    expect(tel(s, env("memory.learned", { key: "k", value: "v" })).inferenceOffline).toBe(false);
    expect(
      tel(s, env("intent.classified", { intent: "i", confidence: 1, complexity: "simple" }, "local"))
        .inferenceOffline,
    ).toBe(false);
  });
});

/* tickers + toasts --------------------------------------------------------------- */

describe("facts, actions, toasts", () => {
  it("memory.learned feeds the facts ticker and a LEARNED toast", () => {
    const s = tel(connected(), env("memory.learned", { key: "user.name", value: "Darwin" }), 5000);
    expect(s.facts[0]).toMatchObject({ key: "user.name", value: "Darwin" });
    expect(s.toasts).toHaveLength(1);
    expect(s.toasts[0].text).toBe("LEARNED: user.name = Darwin");
    expect(s.toasts[0].expiresAt).toBe(5000 + TOAST_TTL_MS);
    expect(s.toasts[0].exiting).toBe(false);
  });

  it("action.executed feeds the actions ticker and an ACTION toast", () => {
    const s = tel(connected(), env("action.executed", { tool: "open_app", outcome: "Opened Safari" }));
    expect(s.actions[0]).toMatchObject({ tool: "open_app", outcome: "Opened Safari" });
    expect(s.toasts[0].text).toBe("ACTION: open_app — Opened Safari");
  });

  it("app.op_forwarded feeds the actions ticker and an OP toast", () => {
    const s = tel(
      connected(),
      env("app.op_forwarded", { name: "silicon-canvas", op: '{"op":"select.net","name":"3V3"}' }),
    );
    expect(s.actions[0]).toMatchObject({
      tool: "silicon-canvas",
      outcome: '{"op":"select.net","name":"3V3"}',
    });
    expect(s.toasts[0].kind).toBe("action");
    expect(s.toasts[0].text).toBe('OP → silicon-canvas: {"op":"select.net","name":"3V3"}');
  });

  it("app.op_forwarded without a name is ignored (same reference)", () => {
    const s = connected();
    expect(tel(s, env("app.op_forwarded", { op: '{"op":"erc.run"}' }))).toBe(s);
  });

  it("app.op_forwarded caps an overlong op line at OP_FORWARD_OUTCOME_CAP", () => {
    const longOp = "x".repeat(OP_FORWARD_OUTCOME_CAP + 50);
    const s = tel(connected(), env("app.op_forwarded", { name: "silicon-canvas", op: longOp }));
    expect(s.actions[0].outcome).toHaveLength(OP_FORWARD_OUTCOME_CAP);
    expect(s.actions[0].outcome.endsWith("…")).toBe(true);
  });

  it("tickers cap at TICKER_CAP newest-first", () => {
    let s = connected();
    for (let i = 0; i < TICKER_CAP + 5; i++) {
      s = tel(s, env("memory.learned", { key: `k${i}`, value: "v" }));
    }
    expect(s.facts).toHaveLength(TICKER_CAP);
    expect(s.facts[0].key).toBe(`k${TICKER_CAP + 4}`);
  });

  it("toast ids are stable and unique (React keys)", () => {
    let s = connected();
    for (let i = 0; i < 3; i++) {
      s = tel(s, env("action.executed", { tool: `t${i}`, outcome: "ok" }), 1000);
    }
    const ids = s.toasts.map((t) => t.id);
    expect(new Set(ids).size).toBe(ids.length);
  });

  it("expiry is two-phase: exiting at TTL, removed TOAST_EXIT_MS later (fade-out)", () => {
    let s = tel(connected(), env("action.executed", { tool: "t", outcome: "ok" }), 1000);
    s = reduce(s, { type: "tick", at: 1000 + TOAST_TTL_MS - 1 });
    expect(s.toasts[0].exiting).toBe(false);
    s = reduce(s, { type: "tick", at: 1000 + TOAST_TTL_MS });
    expect(s.toasts).toHaveLength(1); // still mounted for the fade
    expect(s.toasts[0].exiting).toBe(true);
    s = reduce(s, { type: "tick", at: 1000 + TOAST_TTL_MS + TOAST_EXIT_MS });
    expect(s.toasts).toHaveLength(0);
  });

  it("cap overflow marks the oldest toast exiting instead of deleting it", () => {
    let s = connected();
    for (let i = 0; i < TOAST_CAP; i++) {
      s = tel(s, env("action.executed", { tool: `t${i}`, outcome: "ok" }), 1000);
    }
    expect(s.toasts).toHaveLength(TOAST_CAP);
    const oldestId = s.toasts[0].id;
    s = tel(s, env("action.executed", { tool: "overflow", outcome: "ok" }), 2000);
    // The evicted toast is still present (fading), capped actives hold.
    expect(s.toasts.filter((t) => !t.exiting)).toHaveLength(TOAST_CAP);
    const evicted = s.toasts.find((t) => t.id === oldestId);
    expect(evicted?.exiting).toBe(true);
    // ...and it is removed after the fade window.
    s = reduce(s, { type: "tick", at: 2000 + TOAST_EXIT_MS });
    expect(s.toasts.find((t) => t.id === oldestId)).toBeUndefined();
  });

  it("memory.consolidated emits a faithful summary toast", () => {
    const s = tel(connected(), env("memory.consolidated", { upserts: 4, deletes: 1 }));
    expect(s.toasts[0].text).toBe("MEMORY CONSOLIDATED: 4 upserts, 1 deletes");
  });

  it("proactive.brief emits a cyan info toast with the brief data", () => {
    const s = tel(connected(), env("proactive.brief", { gap_hours: 9, habits_matched: 2 }));
    expect(s.toasts[0].kind).toBe("info");
    expect(s.toasts[0].text).toBe("PROACTIVE BRIEF · GAP 9H · 2 HABITS MATCHED");
  });

  it("proactive.brief tolerates missing fields", () => {
    const s = tel(connected(), env("proactive.brief", { gap_hours: 5 }));
    expect(s.toasts[0].text).toBe("PROACTIVE BRIEF · GAP 5H");
    const s2 = tel(connected(), env("proactive.brief", { habits_matched: 1 }));
    expect(s2.toasts[0].text).toBe("PROACTIVE BRIEF · 1 HABIT MATCHED");
  });

  it("proactive.surface surfaces EDITH's card as an info toast", () => {
    const s = tel(connected(), env("proactive.surface", { trigger: "calendar", text: "Heads up: \"Standup\" starts in 10 minutes." }, "agent.edith"));
    expect(s.toasts[0].kind).toBe("info");
    expect(s.toasts[0].text).toContain("Standup");
    expect(s.toasts[0].text).toContain("calendar");
  });

  it("proactive.surface with no text is a no-op", () => {
    const base = connected();
    const s = tel(base, env("proactive.surface", { trigger: "mail" }, "agent.edith"));
    expect(s.toasts).toHaveLength(0);
  });
});

/* heal events ------------------------------------------------------------------ */

describe("self-heal events", () => {
  it("heal.suppressed records the burst", () => {
    const s = tel(connected(), env("heal.suppressed", { errors_last_60s: 7, reason: "self_heal.enabled = false" }));
    expect(s.heal).toEqual({ event: "heal.suppressed", errorsLast60s: 7 });
  });

  /* v2: root-cause diagnosis (warn-amber, transient) ---------------------- */

  it("heal.diagnosing records the live root-cause diagnosis (warn-amber)", () => {
    const s = tel(
      connected(),
      env("heal.diagnosing", {
        signature: "panicked at 'called Result::unwrap() on an Err value'",
        subsystem: "audio",
        files: ["src/audio.rs:212", "src/speech.rs:88"],
      }),
    );
    expect(s.healDiagnosing).toEqual({
      signature: "panicked at 'called Result::unwrap() on an Err value'",
      subsystem: "audio",
      files: ["src/audio.rs:212", "src/speech.rs:88"],
      ts: s.healDiagnosing?.ts,
    });
    // diagnosis is NOT an error — it must not raise the red banner
    expect(s.healAlert).toBeNull();
  });

  it("heal.diagnosing tolerates missing fields (empty signature/subsystem)", () => {
    const s = tel(connected(), env("heal.diagnosing", { files: ["src/router.rs"] }));
    expect(s.healDiagnosing).toMatchObject({ signature: "", subsystem: "", files: ["src/router.rs"] });
  });

  /* v2: pending proposal (warn-amber, persistent) ------------------------- */

  it("heal.proposal raises a persistent pending proposal with confidence + apply ts", () => {
    const s = tel(
      connected(),
      env("heal.proposal", {
        ts: 1765432100,
        files: ["src/router.rs"],
        validated: true,
        confidence: 0.82,
        subsystem: "router",
        signature: "route timeout",
      }),
    );
    expect(s.healProposal).toMatchObject({
      refTs: 1765432100,
      files: ["src/router.rs"],
      validated: true,
      confidence: 0.82,
      subsystem: "router",
      signature: "route timeout",
    });
    // a proposal is NOT an error — the red banner stays clear
    expect(s.healAlert).toBeNull();
    // survives ticks (persistent until acknowledged)
    const later = reduce(s, { type: "tick", at: 10_000_000 });
    expect(later.healProposal).toBe(s.healProposal);
  });

  it("heal.proposal resolves the diagnosis and inherits its subsystem/signature when not echoed", () => {
    let s = tel(
      connected(),
      env("heal.diagnosing", { signature: "deadlock in mixer", subsystem: "audio", files: ["src/audio.rs:50"] }),
    );
    expect(s.healDiagnosing).not.toBeNull();
    s = tel(s, env("heal.proposal", { ts: 1765432200, files: ["src/audio.rs"], validated: true, confidence: 0.6 }));
    // diagnosing is retired once the proposal lands
    expect(s.healDiagnosing).toBeNull();
    // subsystem/signature carried forward from the diagnosis
    expect(s.healProposal).toMatchObject({ subsystem: "audio", signature: "deadlock in mixer", confidence: 0.6 });
  });

  it("heal.proposal without confidence leaves it null (older daemon)", () => {
    const s = tel(connected(), env("heal.proposal", { ts: 1765432100, files: ["src/x.rs"], validated: true }));
    expect(s.healProposal?.confidence).toBeNull();
  });

  it("a fresh heal.diagnosing supersedes a stale pending proposal", () => {
    let s = tel(connected(), env("heal.proposal", { ts: 1, files: ["src/a.rs"], validated: true, confidence: 0.5 }));
    expect(s.healProposal).not.toBeNull();
    s = tel(s, env("heal.diagnosing", { signature: "new burst", subsystem: "inference", files: ["src/inference.rs"] }));
    expect(s.healProposal).toBeNull();
    expect(s.healDiagnosing?.subsystem).toBe("inference");
  });

  /* v2: error states (alert-red banner, clear the pending surfaces) ------- */

  it("heal.rejected raises a red alert with the failing stage and clears pending surfaces", () => {
    let s = tel(connected(), env("heal.diagnosing", { signature: "x", subsystem: "router", files: [] }));
    s = tel(s, env("heal.rejected", { ts: 1765432100, stage: "cargo_check" }));
    expect(s.healAlert?.kind).toBe("rejected");
    expect(s.healAlert?.detail).toBe("STAGE: cargo_check");
    expect(s.healDiagnosing).toBeNull();
    expect(s.healProposal).toBeNull();
  });

  it("heal.blocked raises a red alert with the reason and clears diagnosing", () => {
    let s = tel(connected(), env("heal.diagnosing", { signature: "x", subsystem: "router", files: [] }));
    s = tel(s, env("heal.blocked", { reason: "no_api_key" }));
    expect(s.healAlert?.kind).toBe("blocked");
    expect(s.healAlert?.detail).toBe("no_api_key");
    expect(s.healDiagnosing).toBeNull();
  });

  it("heal.applied (opt-in auto mode) raises an applied alert and consumes the proposal", () => {
    let s = tel(connected(), env("heal.proposal", { ts: 1765432100, files: ["src/x.rs"], validated: true, confidence: 0.9 }));
    s = tel(s, env("heal.applied", { ts: 1765432100 }));
    expect(s.healAlert?.kind).toBe("applied");
    expect(s.healAlert?.refTs).toBe(1765432100);
    expect(s.healProposal).toBeNull();
  });

  it("alert.dismiss clears the red banner, the proposal, and the diagnosis (no-op when clear)", () => {
    // red banner
    let s = tel(connected(), env("heal.blocked", { reason: "no_api_key" }));
    s = reduce(s, { type: "alert.dismiss" });
    expect(s.healAlert).toBeNull();
    expect(reduce(s, { type: "alert.dismiss" })).toBe(s);
    // pending proposal
    let p = tel(connected(), env("heal.proposal", { ts: 9, files: ["src/x.rs"], validated: true, confidence: 0.7 }));
    p = reduce(p, { type: "alert.dismiss" });
    expect(p.healProposal).toBeNull();
    // live diagnosis
    let d = tel(connected(), env("heal.diagnosing", { signature: "x", subsystem: "audio", files: [] }));
    d = reduce(d, { type: "alert.dismiss" });
    expect(d.healDiagnosing).toBeNull();
  });
});

/* self-heal confidence gauge (pure presentation math) ------------------------ */

describe("self-heal confidence gauge math", () => {
  it("maps 0..1 confidence to lit segments out of CONFIDENCE_SEGMENTS", () => {
    expect(litSegments(0)).toBe(0);
    expect(litSegments(1)).toBe(CONFIDENCE_SEGMENTS);
    expect(litSegments(0.5)).toBe(5);
    expect(litSegments(0.82)).toBe(8);
  });

  it("clamps out-of-range / non-finite confidence (no overrun)", () => {
    expect(litSegments(1.7)).toBe(CONFIDENCE_SEGMENTS);
    expect(litSegments(-0.3)).toBe(0);
    expect(litSegments(Number.NaN)).toBe(0);
  });

  it("confidencePct rounds and clamps to 0..100", () => {
    expect(confidencePct(0.826)).toBe(83);
    expect(confidencePct(0)).toBe(0);
    expect(confidencePct(2)).toBe(100);
    expect(confidencePct(Number.NaN)).toBe(0);
  });
});

/* misc ------------------------------------------------------------------------- */

describe("misc events", () => {
  it("unknown events are ignored without churn (same reference)", () => {
    const s = connected();
    expect(tel(s, env("warp.core.breach", { level: 9000 }))).toBe(s);
  });

  it("known non-state-bearing events are ignored without churn", () => {
    const s = connected();
    expect(tel(s, env("opener.played", { index: 2, text: "Sir?" }, "local"))).toBe(s);
    expect(tel(s, env("vad.segment_capped", { samples: 480000, cap_secs: 30 }, "audio"))).toBe(s);
    expect(tel(s, env("intent.handled", { intent: "web.open", text: "open apple.com" }, "local"))).toBe(s);
  });

  it("events with missing required fields are ignored", () => {
    const s = connected();
    expect(tel(s, env("stt.transcript", {}, "local"))).toBe(s);
    expect(tel(s, env("memory.learned", { value: "no key" }))).toBe(s);
    expect(tel(s, env("action.executed", { outcome: "no tool" }))).toBe(s);
    expect(tel(s, env("audio.level", { speaking: false }, "audio"))).toBe(s);
  });
});

/* agent constellation / team layer (CONTRACT part C) ------------------------- */

describe("agent.active delegation + roster highlight", () => {
  it("sets the active agent from a full event (name/role/hue)", () => {
    const s = tel(
      connected(),
      env("agent.active", { name: "vision", role: "Research + OSINT", hue: 265 }),
    );
    expect(s.activeAgent).toEqual({ name: "vision", role: "Research + OSINT", hue: 265 });
  });

  it("resolves role + hue from the static roster when the event omits them", () => {
    const s = tel(connected(), env("agent.active", { name: "vision" }));
    // CONTRACT C.1: a minimal {name} event still lights a known agent fully.
    expect(s.activeAgent).toEqual({ name: "vision", role: "Research + OSINT", hue: 265 });
  });

  it("the event's own role/hue win over the static mirror (daemon is truth)", () => {
    const s = tel(
      connected(),
      env("agent.active", { name: "steve", role: "Build Engineer", hue: 151 }),
    );
    expect(s.activeAgent).toEqual({ name: "steve", role: "Build Engineer", hue: 151 });
  });

  it("normalizes an out-of-range hue into [0,360)", () => {
    const s = tel(connected(), env("agent.active", { name: "gecko", hue: 480 }));
    expect(s.activeAgent?.hue).toBe(120); // 480 -> 120
  });

  it("lights an UNKNOWN agent too (honesty: the daemon roster is truth)", () => {
    const s = tel(connected(), env("agent.active", { name: "loki", role: "Trickster", hue: 280 }));
    expect(s.activeAgent).toEqual({ name: "loki", role: "Trickster", hue: 280 });
  });

  it("an unknown agent with no hue falls back to the default cyan", () => {
    const s = tel(connected(), env("agent.active", { name: "loki" }));
    expect(s.activeAgent).toEqual({ name: "loki", role: "", hue: 190 });
  });

  it("agent.active with no name is ignored (same reference)", () => {
    const s = connected();
    expect(tel(s, env("agent.active", { role: "x", hue: 100 }))).toBe(s);
  });

  it("a re-emitted identical agent.active returns the SAME reference (no churn)", () => {
    let s = tel(connected(), env("agent.active", { name: "friday", hue: 35 }));
    const before = s;
    expect(tel(s, env("agent.active", { name: "friday", hue: 35, role: "Daily Intel" }))).toBe(
      before,
    );
  });

  it("switching agents replaces the active agent (core hue follows)", () => {
    let s = tel(connected(), env("agent.active", { name: "friday" }));
    expect(s.activeAgent?.name).toBe("friday");
    s = tel(s, env("agent.active", { name: "gecko" }));
    expect(s.activeAgent?.name).toBe("gecko");
    expect(s.activeAgent?.hue).toBe(120);
  });

  it("pipeline.completed releases the active agent (core damps back to cyan)", () => {
    let s = tel(connected(), env("agent.active", { name: "vision" }));
    expect(s.activeAgent).not.toBeNull();
    s = tel(
      s,
      env("pipeline.completed", {
        stt_ms: 1, classify_ms: 1, route_ms: 1, first_audio_ms: 1, speak_ms: 1, total_ms: 5,
      }),
    );
    expect(s.activeAgent).toBeNull();
  });

  it("route.failed releases the active agent", () => {
    let s = tel(connected(), env("agent.active", { name: "steve" }));
    s = tel(s, env("route.failed", { intent: "i", error: "boom" }));
    expect(s.activeAgent).toBeNull();
  });

  it("the 12s stale decay also strands-releases the active agent", () => {
    let s = tel(connected(), env("utterance.captured", { path: "/p" }, "audio"), 1000);
    s = tel(s, env("agent.active", { name: "ultron", hue: 15 }), 1000);
    expect(s.coreState).toBe("processing");
    expect(s.activeAgent?.name).toBe("ultron");
    s = reduce(s, { type: "tick", at: 1000 + STALE_STATE_MS });
    expect(s.coreState).toBe("idle");
    expect(s.activeAgent).toBeNull();
  });

  it("agent.active does NOT touch the core state machine (orthogonal)", () => {
    let s = tel(connected(), env("route.local", { intent: "i", confidence: 1 }, "local"), 1000);
    expect(s.coreState).toBe("thinking-local");
    s = tel(s, env("agent.active", { name: "steve" }), 1100);
    expect(s.coreState).toBe("thinking-local"); // unchanged: only activeAgent moves
  });

  it("roll-call: cycling agents lights each in turn with its hue", () => {
    // CONTRACT C.3 centerpiece — the HUD highlights each agent in order and
    // the core color cycles. The reducer simply tracks the latest active one.
    let s = connected();
    const order = [
      ["darwin", 190],
      ["friday", 35],
      ["vision", 265],
      ["ultron", 15],
      ["gecko", 120],
    ] as const;
    const seen: Array<[string, number]> = [];
    for (const [name] of order) {
      s = tel(s, env("agent.active", { name }));
      seen.push([s.activeAgent!.name, s.activeAgent!.hue]);
    }
    expect(seen).toEqual(order.map(([n, h]) => [n, h]));
  });
});

/* micro-app feed surfaces (build contract part D) ---------------------------- */

const GS = "global-scan";

function gsItem(over: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    title: "Headline",
    source: "NPR",
    url: `https://npr.org/${counter}-${Math.random()}`,
    published: "2026-06-13T11:00:00Z",
    category: "world",
    summary: "One-line summary.",
    ...over,
  };
}

function appData(
  name: string,
  payload: Record<string, unknown>,
): TelemetryEnvelope {
  return env("app.data", { name, topic: "feed", payload });
}

describe("micro-app: running registry (app.started / app.stopped)", () => {
  it("app.started tracks the app as running and creates an empty feed slice", () => {
    const s = tel(connected(), env("app.started", { name: GS }));
    expect(s.runningApps.has(GS)).toBe(true);
    expect(s.appFeeds[GS]).toMatchObject({ running: true, items: [], brief: "" });
  });

  it("app.stopped untracks the app but keeps the last items (dimmed surface)", () => {
    let s = tel(connected(), env("app.started", { name: GS }));
    s = tel(s, appData(GS, { brief: "B", items: [gsItem()], fetched_at: "2026-06-13T11:00:00Z" }));
    expect(s.appFeeds[GS].items).toHaveLength(1);
    s = tel(s, env("app.stopped", { name: GS }));
    expect(s.runningApps.has(GS)).toBe(false);
    expect(s.appFeeds[GS].running).toBe(false);
    expect(s.appFeeds[GS].items).toHaveLength(1); // retained for the placeholder fallback
  });

  it("a restart (started -> stopped -> started) preserves prior items", () => {
    let s = tel(connected(), env("app.started", { name: GS }));
    s = tel(s, appData(GS, { items: [gsItem(), gsItem()] }));
    s = tel(s, env("app.stopped", { name: GS }));
    s = tel(s, env("app.started", { name: GS }));
    expect(s.runningApps.has(GS)).toBe(true);
    expect(s.appFeeds[GS].running).toBe(true);
    expect(s.appFeeds[GS].items).toHaveLength(2);
  });

  it("a re-announced app.started does not blank a populated panel (same items)", () => {
    let s = tel(connected(), env("app.started", { name: GS }));
    s = tel(s, appData(GS, { brief: "live", items: [gsItem()] }));
    const before = s.appFeeds[GS];
    s = tel(s, env("app.started", { name: GS }));
    expect(s.appFeeds[GS]).toBe(before); // no churn, items intact
  });

  it("app.started with no name is ignored (same reference)", () => {
    const s = connected();
    expect(tel(s, env("app.started", {}))).toBe(s);
    expect(tel(s, env("app.stopped", {}))).toBe(s);
  });

  it("app.stopped for an unknown/never-started app is a no-op", () => {
    const s = connected();
    expect(tel(s, env("app.stopped", { name: "ghost" }))).toBe(s);
  });
});

describe("micro-app: app.data feed relay", () => {
  it("populates brief, items, fetched_at and marks the surface running", () => {
    const s = tel(
      connected(),
      appData(GS, {
        brief: "Aggregated public feeds. No prediction.",
        items: [
          gsItem({ title: "A", source: "BBC", category: "world" }),
          gsItem({ title: "B", source: "Ars Technica", category: "tech" }),
        ],
        fetched_at: "2026-06-13T11:30:00Z",
      }),
      4242,
    );
    const f = s.appFeeds[GS];
    expect(f.running).toBe(true);
    expect(f.brief).toBe("Aggregated public feeds. No prediction.");
    expect(f.items.map((i) => i.title)).toEqual(["A", "B"]);
    expect(f.items[0].source).toBe("BBC");
    expect(f.fetchedAt).toBe("2026-06-13T11:30:00Z");
    expect(f.updatedAt).toBe(4242);
    // app.data implies running even without a prior app.started
    expect(s.runningApps.has(GS)).toBe(true);
  });

  it("status relay sets feeds_ok / feeds_failed without disturbing items", () => {
    let s = tel(connected(), appData(GS, { items: [gsItem()] }));
    s = tel(s, appData(GS, { feeds_ok: 7, feeds_failed: 1 }));
    expect(s.appFeeds[GS].feedsOk).toBe(7);
    expect(s.appFeeds[GS].feedsFailed).toBe(1);
    expect(s.appFeeds[GS].items).toHaveLength(1); // items survive a status-only relay
  });

  it("a later items relay replaces the prior items (newest cycle wins)", () => {
    let s = tel(connected(), appData(GS, { items: [gsItem({ title: "old" })] }));
    s = tel(s, appData(GS, { items: [gsItem({ title: "new1" }), gsItem({ title: "new2" })] }));
    expect(s.appFeeds[GS].items.map((i) => i.title)).toEqual(["new1", "new2"]);
  });

  it("caps stored items at APP_FEED_ITEM_CAP", () => {
    const many = Array.from({ length: APP_FEED_ITEM_CAP + 12 }, (_, i) =>
      gsItem({ title: `t${i}` }),
    );
    const s = tel(connected(), appData(GS, { items: many }));
    expect(s.appFeeds[GS].items).toHaveLength(APP_FEED_ITEM_CAP);
    expect(s.appFeeds[GS].items[0].title).toBe("t0"); // newest-first order preserved, tail dropped
  });

  it("coerces malformed item fields to empty strings (never throws)", () => {
    const s = tel(
      connected(),
      appData(GS, {
        items: [
          { title: 123, source: null, url: ["x"], published: {}, category: 5, summary: undefined },
          "not-an-object",
          gsItem({ title: "ok" }),
        ],
      }),
    );
    const items = s.appFeeds[GS].items;
    // the non-object entry is dropped; the malformed object is coerced
    expect(items).toHaveLength(2);
    expect(items[0]).toEqual({ title: "", source: "", url: "", published: "", category: "", summary: "" });
    expect(items[1].title).toBe("ok");
  });

  it("app.data with no name or a non-object payload is ignored (same reference)", () => {
    const s = connected();
    expect(tel(s, env("app.data", { topic: "feed", payload: { items: [] } }))).toBe(s);
    expect(tel(s, env("app.data", { name: GS, topic: "feed", payload: "nope" }))).toBe(s);
    expect(tel(s, env("app.data", { name: GS, topic: "feed" }))).toBe(s);
  });

  it("multiple apps keep independent feed slices (keyed by name)", () => {
    let s = tel(connected(), appData(GS, { items: [gsItem({ title: "gs" })] }));
    s = tel(s, appData("algo-core", { items: [gsItem({ title: "ac" })] }));
    expect(s.appFeeds[GS].items[0].title).toBe("gs");
    expect(s.appFeeds["algo-core"].items[0].title).toBe("ac");
    expect(s.runningApps.has(GS)).toBe(true);
    expect(s.runningApps.has("algo-core")).toBe(true);
  });

  it("app.log / app.auth_failed / app.crashed are not panel-state-bearing (same reference)", () => {
    const s = tel(connected(), appData(GS, { items: [gsItem()] }));
    expect(tel(s, env("app.log", { name: GS, line: "polling" }))).toBe(s);
    expect(tel(s, env("app.auth_failed", { name: GS }))).toBe(s);
    expect(tel(s, env("app.crashed", { name: GS, restarts: 3 }))).toBe(s);
  });

  it("app feed events never decay the core state (orthogonal to the voice pipeline)", () => {
    let s = tel(connected(), env("utterance.captured", { path: "/p" }, "audio"), 1000);
    expect(s.coreState).toBe("processing");
    s = tel(s, appData(GS, { items: [gsItem()] }), 1100);
    expect(s.coreState).toBe("processing"); // app data must not touch the state machine
  });
});

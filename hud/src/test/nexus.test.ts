import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import NexusPanel from "../components/NexusPanel";
import {
  NEXUS_MATRIX_DIM_CAP,
  NEXUS_SPECTRUM_BANDS,
  NEXUS_TOPIC_CLIPPING,
  NEXUS_TOPIC_GAIN,
  NEXUS_TOPIC_LEVELS,
  NEXUS_TOPIC_ROUTES,
  NEXUS_TOPIC_SPECTRUM,
  parseNexusClipping,
  parseNexusGain,
  parseNexusLevels,
  parseNexusRoutes,
  parseNexusSpectrum,
  type TelemetryEnvelope,
} from "../core/events";
import { initialState, reduce, type AppFeed, type HudState } from "../core/state";

/* ------------------------------------------------------------------------ *
 * Nexus micro-app payload parsers (events.ts). DEVICE-GATED, ON-DEVICE ONLY:  *
 * the CoreAudio IOProc, aggregate device, the sub-10ms monitor RTT and AUv3   *
 * hosting run on real audio hardware and are NOT exercised here. These tests   *
 * cover ONLY the telemetry the HUD-side panel renders, against SYNTHESIZED     *
 * in-memory payloads — never a real device. A malformed/partial payload must   *
 * yield null (or drop the offending sub-item), never throw. The panel must     *
 * never fabricate a measurement: measured_rtt_ms / LUFS stay null when the     *
 * on-device core has not reported them.                                        *
 * ------------------------------------------------------------------------ */

/** Build a synthetic 96-band spectrum frame (no device involved). */
function bands96(value = -40): number[] {
  return Array.from({ length: NEXUS_SPECTRUM_BANDS }, () => value);
}

describe("parseNexusLevels (audio.levels — DEFAULT topic)", () => {
  it("parses a full per-channel meter frame + the three BS.1770-4 LUFS readouts", () => {
    const l = parseNexusLevels({
      ch: [
        { peak_dbfs: -6.2, rms_dbfs: -18.4 },
        { peak_dbfs: -3.1, rms_dbfs: -14.0 },
      ],
      lufs_m: -23.1,
      lufs_s: -22.4,
      lufs_i: -23.0,
    });
    expect(l).toEqual({
      ch: [
        { peakDbfs: -6.2, rmsDbfs: -18.4 },
        { peakDbfs: -3.1, rmsDbfs: -14.0 },
      ],
      lufsM: -23.1,
      lufsS: -22.4,
      lufsI: -23.0,
    });
  });

  it("returns null when `ch` is absent or non-array (no meter frame to render)", () => {
    expect(parseNexusLevels({ lufs_i: -23 })).toBeNull();
    expect(parseNexusLevels({ ch: "loud" })).toBeNull();
    expect(parseNexusLevels({})).toBeNull();
  });

  it("accepts an empty channel list (device open, no inputs) with null LUFS", () => {
    const l = parseNexusLevels({ ch: [] });
    expect(l).not.toBeNull();
    expect(l!.ch).toEqual([]);
    // HONEST: an unreported loudness reads null, never a fabricated value.
    expect(l!.lufsM).toBeNull();
    expect(l!.lufsS).toBeNull();
    expect(l!.lufsI).toBeNull();
  });

  it("drops channels with no usable peak; defaults rms to the peak on partials", () => {
    const l = parseNexusLevels({
      ch: [
        { rms_dbfs: -20 }, // no peak -> dropped
        "junk", // not an object -> dropped
        { peak_dbfs: -9 }, // peak only -> rms defaults to peak
        { peak_dbfs: -4, rms_dbfs: -12 },
      ],
    });
    expect(l!.ch).toEqual([
      { peakDbfs: -9, rmsDbfs: -9 },
      { peakDbfs: -4, rmsDbfs: -12 },
    ]);
  });

  it("rejects a non-finite peak (NaN/Infinity) on a channel rather than rendering it", () => {
    const l = parseNexusLevels({ ch: [{ peak_dbfs: NaN }, { peak_dbfs: -Infinity }] });
    expect(l!.ch).toEqual([]);
  });
});

describe("parseNexusRoutes (audio.routes)", () => {
  it("parses a matrix snapshot + a measured loopback RTT", () => {
    const r = parseNexusRoutes({
      inputs: 4,
      outputs: 2,
      matrix: [
        { in: 0, out: 0, gain_db: 0 },
        { in: 1, out: 1, gain_db: -3.5 },
      ],
      measured_rtt_ms: 7.4,
    });
    expect(r).toEqual({
      inputs: 4,
      outputs: 2,
      matrix: [
        { in: 0, out: 0, gainDb: 0 },
        { in: 1, out: 1, gainDb: -3.5 },
      ],
      measuredRttMs: 7.4,
    });
  });

  it("returns null unless inputs AND outputs are finite (the grid dimensions)", () => {
    expect(parseNexusRoutes({ outputs: 2, matrix: [] })).toBeNull();
    expect(parseNexusRoutes({ inputs: 4, matrix: [] })).toBeNull();
    expect(parseNexusRoutes({ inputs: "4", outputs: 2 })).toBeNull();
    expect(parseNexusRoutes({})).toBeNull();
  });

  it("leaves measured_rtt_ms null until the on-device core measures it — NEVER fabricated", () => {
    const r = parseNexusRoutes({ inputs: 2, outputs: 2, matrix: [] });
    expect(r).not.toBeNull();
    expect(r!.measuredRttMs).toBeNull();
    expect(r!.matrix).toEqual([]);
  });

  it("drops crosspoints missing in/out/gain; keeps the well-formed ones", () => {
    const r = parseNexusRoutes({
      inputs: 3,
      outputs: 3,
      matrix: [
        { in: 0, out: 1, gain_db: 2 },
        { in: 1, out: 2 }, // no gain -> dropped
        { out: 0, gain_db: 0 }, // no in -> dropped
        "junk", // not an object -> dropped
        { in: 2, out: 2, gain_db: -6 },
      ],
    });
    expect(r!.matrix).toEqual([
      { in: 0, out: 1, gainDb: 2 },
      { in: 2, out: 2, gainDb: -6 },
    ]);
  });

  it("defaults matrix to [] when absent (a cleared matrix)", () => {
    expect(parseNexusRoutes({ inputs: 1, outputs: 1 })!.matrix).toEqual([]);
  });

  it("floors + clamps the grid dimensions so a malformed/spoofed frame can never crash the panel render", () => {
    // A huge dimension would otherwise feed Array.from({length}) in the panel —
    // a freeze/OOM, or past 2^32 a RangeError that takes down the HUD. The parser
    // is the fail-safe boundary: dimensions are floored to ints and clamped.
    const huge = parseNexusRoutes({ inputs: 4294967296, outputs: 1e9, matrix: [] });
    expect(huge).not.toBeNull();
    expect(huge!.inputs).toBe(NEXUS_MATRIX_DIM_CAP);
    expect(huge!.outputs).toBe(NEXUS_MATRIX_DIM_CAP);
    // Renderability invariant the panel relies on: each dimension fits a real array.
    expect(() => Array.from({ length: huge!.inputs })).not.toThrow();
    expect(() => Array.from({ length: huge!.outputs })).not.toThrow();
    // Negative / fractional dimensions floor to a safe non-negative integer.
    const odd = parseNexusRoutes({ inputs: -3, outputs: 2.9, matrix: [] });
    expect(odd!.inputs).toBe(0);
    expect(odd!.outputs).toBe(2);
    // Real small interfaces are untouched (DEFAULT 4x4 well under the cap).
    const real = parseNexusRoutes({ inputs: 4, outputs: 4, matrix: [] });
    expect(real!.inputs).toBe(4);
    expect(real!.outputs).toBe(4);
  });
});

describe("parseNexusGain (audio.gain)", () => {
  it("parses an input/output trim change with its gain-staging stage", () => {
    expect(parseNexusGain({ channel: 0, gain_db: -2.5, stage: "interface" })).toEqual({
      channel: 0,
      gainDb: -2.5,
      muted: null,
      stage: "interface",
    });
  });

  it("parses the DISTINCT mute/unmute payload ({muted} instead of {gain_db})", () => {
    // "mute the mic" — the frame the app emits for a gain.set {mute:true} op.
    expect(parseNexusGain({ channel: 0, muted: true, stage: "input" })).toEqual({
      channel: 0,
      gainDb: null,
      muted: true,
      stage: "input",
    });
    expect(parseNexusGain({ channel: 2, muted: false, stage: "output" })).toEqual({
      channel: 2,
      gainDb: null,
      muted: false,
      stage: "output",
    });
  });

  it("returns null unless channel is finite AND the frame carries gain_db or muted", () => {
    expect(parseNexusGain({ gain_db: -2 })).toBeNull();
    expect(parseNexusGain({ channel: 0 })).toBeNull();
    expect(parseNexusGain({ channel: 0, gain_db: Infinity })).toBeNull();
    expect(parseNexusGain({ muted: true })).toBeNull(); // no channel
    expect(parseNexusGain({ channel: 0, muted: "yes" })).toBeNull(); // non-bool mute
    expect(parseNexusGain({})).toBeNull();
  });

  it("still rejects the legacy gain_db=null mute frame (never a fake 0 dB)", () => {
    // The pre-fix app emitted {gain_db: null} on mute; with no muted flag that
    // frame stays rejected rather than being misread as a trim change.
    expect(parseNexusGain({ channel: 0, gain_db: null, stage: "input" })).toBeNull();
  });

  it("defaults stage to '' when absent", () => {
    expect(parseNexusGain({ channel: 1, gain_db: 6 })).toEqual({
      channel: 1,
      gainDb: 6,
      muted: null,
      stage: "",
    });
  });
});

describe("parseNexusClipping (audio.clipping)", () => {
  it("parses a true-peak clip event (SPEC §3, -1 dBFS, 4x oversampled)", () => {
    expect(parseNexusClipping({ channel: 2, true_peak_dbfs: -0.3 })).toEqual({
      channel: 2,
      truePeakDbfs: -0.3,
    });
  });

  it("returns null unless channel AND true_peak_dbfs are finite", () => {
    expect(parseNexusClipping({ channel: 2 })).toBeNull();
    expect(parseNexusClipping({ true_peak_dbfs: -0.3 })).toBeNull();
    expect(parseNexusClipping({ channel: 2, true_peak_dbfs: "loud" })).toBeNull();
    expect(parseNexusClipping({})).toBeNull();
  });
});

describe("parseNexusSpectrum (audio.spectrum)", () => {
  it("parses a 96-band log spectrum frame", () => {
    const s = parseNexusSpectrum({ bands: bands96(-30) });
    expect(s).not.toBeNull();
    expect(s!.bands).toHaveLength(NEXUS_SPECTRUM_BANDS);
    expect(s!.bands[0]).toBe(-30);
  });

  it("rejects a frame with the wrong band count (a partial FFT is not rendered)", () => {
    expect(parseNexusSpectrum({ bands: bands96().slice(0, 95) })).toBeNull(); // too few
    expect(parseNexusSpectrum({ bands: [...bands96(), -40] })).toBeNull(); // too many
    expect(parseNexusSpectrum({ bands: [] })).toBeNull();
    expect(parseNexusSpectrum({})).toBeNull();
  });

  it("rejects a frame containing a non-finite band rather than rendering a hole", () => {
    const withNaN = bands96();
    withNaN[40] = NaN;
    expect(parseNexusSpectrum({ bands: withNaN })).toBeNull();
    const withStr = bands96() as unknown[];
    withStr[10] = "loud";
    expect(parseNexusSpectrum({ bands: withStr as number[] })).toBeNull();
  });
});

/* ------------------------------------------------------------------------ *
 * Reducer: app.data stashes each nexus topic under feed.topics, keyed by the  *
 * relay topic — additive, and must not disturb other app surfaces. The panel  *
 * reads + narrows its own slice with the parsers above (mirrors vision /      *
 * silicon-canvas). Nexus ships no UI; the HUD renders from telemetry only.    *
 * ------------------------------------------------------------------------ */

const N = "nexus";

function env(event: string, data: Record<string, unknown>): TelemetryEnvelope {
  return { ts: "2026-06-13T12:00:00.000Z", source: "system", event, data };
}

function tel(state: HudState, e: TelemetryEnvelope, at = 1000): HudState {
  return reduce(state, { type: "telemetry", envelope: e, at });
}

function connected(): HudState {
  return reduce(initialState(), { type: "ws.connected", at: 0 });
}

describe("reducer: app.data nexus topic storage", () => {
  it("stores each nexus topic payload verbatim under feed.topics[topic]", () => {
    let s = connected();
    s = tel(
      s,
      env("app.data", {
        name: N,
        topic: NEXUS_TOPIC_LEVELS,
        payload: {
          ch: [{ peak_dbfs: -6, rms_dbfs: -18 }],
          lufs_m: -23,
          lufs_s: -22,
          lufs_i: -23,
        },
      }),
    );
    s = tel(
      s,
      env("app.data", {
        name: N,
        topic: NEXUS_TOPIC_ROUTES,
        payload: {
          inputs: 2,
          outputs: 2,
          matrix: [{ in: 0, out: 0, gain_db: 0 }],
          measured_rtt_ms: 7.1,
        },
      }),
    );
    s = tel(
      s,
      env("app.data", {
        name: N,
        topic: NEXUS_TOPIC_GAIN,
        payload: { channel: 0, gain_db: -2, stage: "interface" },
      }),
    );
    s = tel(
      s,
      env("app.data", {
        name: N,
        topic: NEXUS_TOPIC_CLIPPING,
        payload: { channel: 0, true_peak_dbfs: -0.2 },
      }),
    );
    s = tel(
      s,
      env("app.data", {
        name: N,
        topic: NEXUS_TOPIC_SPECTRUM,
        payload: { bands: bands96(-35) },
      }),
    );

    const feed = s.appFeeds[N];
    expect(feed.running).toBe(true);
    expect(s.runningApps.has(N)).toBe(true);

    // Each topic slice round-trips through the matching parser.
    expect(parseNexusLevels(feed.topics[NEXUS_TOPIC_LEVELS])!.ch).toHaveLength(1);
    expect(parseNexusLevels(feed.topics[NEXUS_TOPIC_LEVELS])!.lufsI).toBe(-23);
    expect(parseNexusRoutes(feed.topics[NEXUS_TOPIC_ROUTES])!.measuredRttMs).toBe(7.1);
    expect(parseNexusGain(feed.topics[NEXUS_TOPIC_GAIN])!.stage).toBe("interface");
    expect(parseNexusClipping(feed.topics[NEXUS_TOPIC_CLIPPING])!.truePeakDbfs).toBe(-0.2);
    expect(parseNexusSpectrum(feed.topics[NEXUS_TOPIC_SPECTRUM])!.bands).toHaveLength(
      NEXUS_SPECTRUM_BANDS,
    );
  });

  it("a newer payload on the SAME topic replaces it; other topics are retained", () => {
    let s = connected();
    s = tel(
      s,
      env("app.data", {
        name: N,
        topic: NEXUS_TOPIC_LEVELS,
        payload: { ch: [{ peak_dbfs: -10 }] },
      }),
    );
    s = tel(
      s,
      env("app.data", {
        name: N,
        topic: NEXUS_TOPIC_ROUTES,
        payload: { inputs: 2, outputs: 2, matrix: [] },
      }),
    );
    // Newer levels frame on the same topic.
    s = tel(
      s,
      env("app.data", {
        name: N,
        topic: NEXUS_TOPIC_LEVELS,
        payload: { ch: [{ peak_dbfs: -2 }] },
      }),
    );

    const feed = s.appFeeds[N];
    expect(parseNexusLevels(feed.topics[NEXUS_TOPIC_LEVELS])!.ch[0].peakDbfs).toBe(-2);
    // The routes topic stored earlier survives the levels update.
    expect(parseNexusRoutes(feed.topics[NEXUS_TOPIC_ROUTES])!.inputs).toBe(2);
  });

  it("does not mutate the prior topics map in place (immutable update)", () => {
    let s = connected();
    s = tel(
      s,
      env("app.data", {
        name: N,
        topic: NEXUS_TOPIC_LEVELS,
        payload: { ch: [] },
      }),
    );
    const beforeTopics = s.appFeeds[N].topics;
    s = tel(
      s,
      env("app.data", {
        name: N,
        topic: NEXUS_TOPIC_ROUTES,
        payload: { inputs: 1, outputs: 1, matrix: [] },
      }),
    );
    expect(NEXUS_TOPIC_ROUTES in beforeTopics).toBe(false);
    expect(NEXUS_TOPIC_ROUTES in s.appFeeds[N].topics).toBe(true);
  });

  it("an app.stopped marks the nexus surface offline but keeps the last telemetry", () => {
    let s = connected();
    s = tel(
      s,
      env("app.data", {
        name: N,
        topic: NEXUS_TOPIC_ROUTES,
        payload: { inputs: 4, outputs: 2, matrix: [{ in: 0, out: 0, gain_db: 0 }], measured_rtt_ms: 6.8 },
      }),
    );
    s = tel(s, env("app.stopped", { name: N }));
    const feed = s.appFeeds[N];
    expect(feed.running).toBe(false);
    expect(s.runningApps.has(N)).toBe(false);
    // Last telemetry retained so the panel can show it dimmed, not blanked.
    expect(parseNexusRoutes(feed.topics[NEXUS_TOPIC_ROUTES])!.measuredRttMs).toBe(6.8);
  });

  it("ignores a nexus app.data line with no payload object (no churn)", () => {
    let s = connected();
    const before = s;
    s = tel(s, env("app.data", { name: N, topic: NEXUS_TOPIC_LEVELS }));
    // No payload -> reducer returns the same reference (existing contract).
    expect(s).toBe(before);
  });

  it("does not disturb a co-resident app surface (additive per-app storage)", () => {
    let s = connected();
    // A different app reports first.
    s = tel(
      s,
      env("app.data", {
        name: "vision",
        topic: "vision.status",
        payload: { state: "watching" },
      }),
    );
    s = tel(
      s,
      env("app.data", {
        name: N,
        topic: NEXUS_TOPIC_LEVELS,
        payload: { ch: [{ peak_dbfs: -5 }] },
      }),
    );
    // Both surfaces coexist; nexus storage did not touch vision's slice.
    expect(s.appFeeds["vision"].topics["vision.status"]).toEqual({ state: "watching" });
    expect(parseNexusLevels(s.appFeeds[N].topics[NEXUS_TOPIC_LEVELS])!.ch).toHaveLength(1);
  });
});

/* ------------------------------------------------------------------------ *
 * The panel itself (rendered headlessly via renderToStaticMarkup — node env,  *
 * no jsdom, same pattern as forge/mark-forge tests): the END-TO-END wire      *
 * check for the two nexus contract fixes. The APP emits crosspoints under     *
 * "matrix" (emit_routes) and mutes as {muted} (gain.set) — these feeds prove   *
 * the panel actually LIGHTS from those exact payloads.                        *
 * ------------------------------------------------------------------------ */

function nexusFeed(topics: AppFeed["topics"]): AppFeed {
  return {
    running: true,
    brief: "",
    items: [],
    fetchedAt: null,
    feedsOk: null,
    feedsFailed: null,
    updatedAt: 1000,
    topics,
  };
}

describe("NexusPanel (headless render of the fixed wire shapes)", () => {
  it("lights matrix crosspoints from the app's `matrix` wire key", () => {
    const feed = nexusFeed({
      [NEXUS_TOPIC_ROUTES]: {
        inputs: 2,
        outputs: 2,
        matrix: [{ in: 0, out: 1, gain_db: -6 }],
        measured_rtt_ms: null,
      },
    });
    const html = renderToStaticMarkup(createElement(NexusPanel, { feed, running: true }));
    expect(html).toContain("2×2");
    // The live crosspoint cell renders lit, with its gain in the tooltip
    // (toFixed produces an ASCII minus in the title attribute).
    expect(html).toContain("nx-cell on");
    expect(html).toContain("I0→O1 -6.0 dB");
  });

  it("renders MUTED (never a fabricated 0.0 dB) from the {muted} gain payload", () => {
    const feed = nexusFeed({
      [NEXUS_TOPIC_GAIN]: { channel: 0, muted: true, stage: "input" },
    });
    const html = renderToStaticMarkup(createElement(NexusPanel, { feed, running: true }));
    expect(html).toContain("MUTED");
    expect(html).toContain("CH 0");
    expect(html).not.toContain("0.0 dB");
  });

  it("renders UNMUTED for a muted:false frame and the trim dB for a gain_db frame", () => {
    const unmuted = nexusFeed({
      [NEXUS_TOPIC_GAIN]: { channel: 1, muted: false, stage: "input" },
    });
    expect(
      renderToStaticMarkup(createElement(NexusPanel, { feed: unmuted, running: true })),
    ).toContain("UNMUTED");
    const trim = nexusFeed({
      [NEXUS_TOPIC_GAIN]: { channel: 1, gain_db: -2.5, stage: "output" },
    });
    const html = renderToStaticMarkup(createElement(NexusPanel, { feed: trim, running: true }));
    expect(html).toContain("2.5 dB");
    expect(html).toContain("OUTPUT");
  });
});

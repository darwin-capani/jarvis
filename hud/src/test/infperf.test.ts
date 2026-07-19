import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import InferencePerfPanel from "../components/InferencePerfPanel";
import {
  applyInferencePerf,
  inferencePerfInitial,
  applyInferenceDecode,
  quantHonest,
  quantIsKnown,
  quantLabel,
  speculativeHonest,
  speculativeLabel,
  speculativeTone,
  throttleHonest,
  throttleReasonLabel,
  throttleTierPrefLabel,
  throttleTone,
  type InferencePerfStatus,
  type TelemetryEnvelope,
} from "../core/events";
import { HudState, initialState, reduce } from "../core/state";

/* helpers ------------------------------------------------------------------ */

let counter = 0;
function env(
  event: string,
  data: Record<string, unknown> = {},
  source = "system",
): TelemetryEnvelope {
  counter += 1;
  return {
    ts: `2026-06-17T12:00:${String(counter % 60).padStart(2, "0")}Z`,
    source,
    event,
    data,
  };
}

function tel(state: HudState, e: TelemetryEnvelope, at = 1000): HudState {
  return reduce(state, { type: "telemetry", envelope: e, at });
}

function connected(at = 0): HudState {
  return reduce(initialState(), { type: "ws.connected", at });
}

function renderPanel(perf: InferencePerfStatus): string {
  return renderToStaticMarkup(createElement(InferencePerfPanel, { perf }));
}

/* ----------------------------------------------------------------- seeding */

describe("inference-perf folding (events.ts)", () => {
  it("seeds the honest awaiting / no-throttle resting state", () => {
    expect(inferencePerfInitial()).toEqual({
      speculative: null,
      quant: null,
      throttle: null,
      tps: null,
      peakMemGib: null,
    });
  });

  /* ----------------------------------------------------- #37 speculative */

  it("folds speculative=true (the path that ACTUALLY ran)", () => {
    const p = applyInferencePerf(inferencePerfInitial(), { speculative: true });
    expect(p.speculative).toBe(true);
    expect(speculativeLabel(p)).toBe("ON");
    expect(speculativeTone(p)).toBe("good");
  });

  it("folds speculative=false (normal generation — the honest fallback)", () => {
    const p = applyInferencePerf(inferencePerfInitial(), { speculative: false });
    expect(p.speculative).toBe(false);
    expect(speculativeLabel(p)).toBe("OFF");
    expect(speculativeTone(p)).toBe("idle");
    // The honest copy names the fallback, never fakes speculative.
    expect(speculativeHonest(p)).toMatch(/normal generation/i);
    expect(speculativeHonest(p)).toMatch(/never fakes speculative/i);
  });

  it("awaiting speculative reads AWAITING, idle, never asserts a path", () => {
    const p = inferencePerfInitial();
    expect(speculativeLabel(p)).toBe("AWAITING");
    expect(speculativeTone(p)).toBe("idle");
    expect(speculativeHonest(p)).toMatch(/awaiting/i);
  });

  it("a missing speculative field keeps the prior value (no blank)", () => {
    const on = applyInferencePerf(inferencePerfInitial(), { speculative: true });
    const after = applyInferencePerf(on, { quant: "int4" }); // no speculative key
    expect(after.speculative).toBe(true);
  });

  it("a non-bool speculative is ignored (keeps prior)", () => {
    const on = applyInferencePerf(inferencePerfInitial(), { speculative: true });
    const after = applyInferencePerf(on, { speculative: "yes" });
    expect(after.speculative).toBe(true);
  });

  it("speculative honest ON states it is device-gated, never a number", () => {
    const p = applyInferencePerf(inferencePerfInitial(), { speculative: true });
    expect(speculativeHonest(p)).toMatch(/device\/model-dependent|device-dependent/i);
    expect(speculativeHonest(p)).toMatch(/not measured/i);
  });

  /* ----------------------------------------------------------- #39 quant */

  it("folds the quant that ACTUALLY loaded (verbatim)", () => {
    const p = applyInferencePerf(inferencePerfInitial(), { quant: "int4" });
    expect(p.quant).toBe("int4");
    expect(quantLabel(p)).toBe("INT4");
    expect(quantIsKnown(p)).toBe(true);
  });

  it("auto quant reads AUTO + the as-configured honest note", () => {
    const p = applyInferencePerf(inferencePerfInitial(), { quant: "auto" });
    expect(quantLabel(p)).toBe("AUTO");
    expect(quantIsKnown(p)).toBe(true);
    expect(quantHonest(p)).toMatch(/as configured|today's behavior/i);
  });

  it("an UNKNOWN quant is still shown verbatim (honest) but reads unknown", () => {
    // The server reports the quant that ACTUALLY loaded — even a string the HUD
    // doesn't recognize is shown, never hidden, never rewritten.
    const p = applyInferencePerf(inferencePerfInitial(), { quant: "int3" });
    expect(p.quant).toBe("int3");
    expect(quantLabel(p)).toBe("INT3");
    expect(quantIsKnown(p)).toBe(false);
  });

  it("quant honest for an explicit quant names the fallback truth", () => {
    const p = applyInferencePerf(inferencePerfInitial(), { quant: "int8" });
    expect(quantHonest(p)).toMatch(/ACTUALLY loaded|actually loaded/i);
    expect(quantHonest(p)).toMatch(/fell back|fall back|fallback/i);
    expect(quantHonest(p)).toMatch(/not measured|device-gated/i);
  });

  it("awaiting quant reads AWAITING, unknown, never asserts a quant", () => {
    const p = inferencePerfInitial();
    expect(quantLabel(p)).toBe("AWAITING");
    expect(quantIsKnown(p)).toBe(false);
    expect(quantHonest(p)).toMatch(/awaiting/i);
  });

  it("a non-string quant is ignored (keeps prior)", () => {
    const seeded = applyInferencePerf(inferencePerfInitial(), { quant: "fp16" });
    const after = applyInferencePerf(seeded, { quant: 4 });
    expect(after.quant).toBe("fp16");
  });

  /* -------------------------------------------------------- #38 throttle */

  it("folds an active throttle plan (low_battery)", () => {
    const p = applyInferencePerf(inferencePerfInitial(), {
      throttle: { reason: "low_battery", tier_pref: "fast", defer_heavy: true },
    });
    expect(p.throttle).toEqual({
      reason: "low_battery",
      tierPref: "fast",
      deferHeavy: true,
    });
    expect(throttleTone(p.throttle)).toBe("warn");
    expect(throttleReasonLabel(p.throttle)).toBe("LOW BATTERY");
    expect(throttleTierPrefLabel(p.throttle)).toBe("FAST");
  });

  it("folds an active throttle plan (thermal)", () => {
    const p = applyInferencePerf(inferencePerfInitial(), {
      throttle: { reason: "thermal", tier_pref: "fast", defer_heavy: false },
    });
    expect(p.throttle?.reason).toBe("thermal");
    expect(throttleReasonLabel(p.throttle)).toBe("THERMAL");
    expect(p.throttle?.deferHeavy).toBe(false);
  });

  it("OFF DEFAULT: an absent throttle field => null (no phantom indicator)", () => {
    // Under [power].adaptive off the daemon emits NO throttle field. The HUD must
    // show no throttle — never a phantom one.
    const p = applyInferencePerf(inferencePerfInitial(), { speculative: false, quant: "auto" });
    expect(p.throttle).toBeNull();
    expect(throttleTone(p.throttle)).toBe("idle");
    expect(throttleReasonLabel(p.throttle)).toBe("");
    expect(throttleTierPrefLabel(p.throttle)).toBe("");
    expect(throttleHonest(p.throttle)).toMatch(/no throttle/i);
    expect(throttleHonest(p.throttle)).toMatch(/device-gated/i);
  });

  it("throttle is REPLACED every turn (a stale throttle never lingers)", () => {
    const throttled = applyInferencePerf(inferencePerfInitial(), {
      throttle: { reason: "thermal", tier_pref: "fast", defer_heavy: true },
    });
    expect(throttled.throttle).not.toBeNull();
    // Next turn carries no throttle (e.g. cooled down / on AC) -> cleared.
    const cleared = applyInferencePerf(throttled, { speculative: false });
    expect(cleared.throttle).toBeNull();
  });

  it("a throttle with a neutral/disabled reason is DROPPED (never named)", () => {
    // The daemon only emits `throttle` when it actually throttled, so a
    // disabled/nominal reason should never arrive — but if it does, drop it
    // rather than render a throttle we can't honestly name.
    for (const reason of ["disabled", "nominal", "bogus"]) {
      const p = applyInferencePerf(inferencePerfInitial(), {
        throttle: { reason, tier_pref: "fast", defer_heavy: true },
      });
      expect(p.throttle).toBeNull();
    }
  });

  it("a malformed throttle object => null (honest no-throttle, never throws)", () => {
    const p = applyInferencePerf(inferencePerfInitial(), { throttle: "throttled!" });
    expect(p.throttle).toBeNull();
  });

  it("an active throttle with a missing tier_pref defaults to AUTO pref", () => {
    const p = applyInferencePerf(inferencePerfInitial(), {
      throttle: { reason: "low_battery" },
    });
    expect(p.throttle?.tierPref).toBe("auto");
    expect(p.throttle?.deferHeavy).toBe(false);
  });

  it("throttle honest names the reason + that the live power read is device-gated", () => {
    const p = applyInferencePerf(inferencePerfInitial(), {
      throttle: { reason: "thermal", tier_pref: "fast", defer_heavy: true },
    });
    expect(throttleHonest(p.throttle)).toMatch(/thermal/i);
    expect(throttleHonest(p.throttle)).toMatch(/device-gated/i);
    expect(throttleHonest(p.throttle)).toMatch(/defers heavy/i);
  });
});

/* ---------------------------------------------------- reducer model.tier */

describe("reducer model.tier inference-perf folding", () => {
  it("threads speculative/quant/throttle from a model.tier turn", () => {
    const s = tel(
      connected(),
      env("model.tier", {
        tier: "local",
        reason: "auto",
        speculative: true,
        quant: "int4",
        throttle: { reason: "low_battery", tier_pref: "fast", defer_heavy: true },
      }),
    );
    expect(s.inferencePerf.speculative).toBe(true);
    expect(s.inferencePerf.quant).toBe("int4");
    expect(s.inferencePerf.throttle?.reason).toBe("low_battery");
  });

  it("OFF DEFAULT: a model.tier turn with no perf fields stays neutral", () => {
    // Today's runtime: speculative off + auto quant + no throttle. A turn that
    // reports only speculative=false + quant=auto must leave throttle null.
    const s = tel(
      connected(),
      env("model.tier", {
        tier: "local",
        reason: "auto",
        speculative: false,
        quant: "auto",
      }),
    );
    expect(s.inferencePerf.speculative).toBe(false);
    expect(s.inferencePerf.quant).toBe("auto");
    expect(s.inferencePerf.throttle).toBeNull();
  });

  it("a malformed model.tier never throws and never blanks a known readout", () => {
    let s = tel(
      connected(),
      env("model.tier", { tier: "local", speculative: true, quant: "int8" }),
    );
    s = tel(s, env("model.tier", { tier: 99, speculative: [], quant: {} }));
    expect(s.inferencePerf.speculative).toBe(true);
    expect(s.inferencePerf.quant).toBe("int8");
  });

  it("a cleared throttle next turn drops the prior throttle (no stale)", () => {
    let s = tel(
      connected(),
      env("model.tier", {
        tier: "local",
        throttle: { reason: "thermal", tier_pref: "fast", defer_heavy: true },
      }),
    );
    expect(s.inferencePerf.throttle).not.toBeNull();
    s = tel(s, env("model.tier", { tier: "local", speculative: false }));
    expect(s.inferencePerf.throttle).toBeNull();
  });

  it("the perf surface starts at the honest resting state on a fresh connect", () => {
    const s = connected();
    expect(s.inferencePerf).toEqual({ speculative: null, quant: null, throttle: null, tps: null, peakMemGib: null });
  });

  /* ---------------------------------------------- Wave A: decode metrics */

  it("folds mlx_lm-measured decode metrics (tok/s + peak mem) without touching throttle", () => {
    // A throttle set by model.tier must survive a following inference.decode.
    const throttled = applyInferencePerf(inferencePerfInitial(), {
      throttle: { reason: "thermal", tier_pref: "fast", defer_heavy: true },
    });
    const after = applyInferenceDecode(throttled, {
      generation_tps: 61.3,
      peak_memory_gb: 2.44,
      speculative: false,
      quant: "int4",
    });
    expect(after.tps).toBe(61.3);
    expect(after.peakMemGib).toBe(2.44);
    expect(after.speculative).toBe(false);
    expect(after.quant).toBe("int4");
    expect(after.throttle).toEqual({ reason: "thermal", tierPref: "fast", deferHeavy: true });
  });

  it("keeps prior decode numbers when a turn reports none (never blanks a known value)", () => {
    const first = applyInferenceDecode(inferencePerfInitial(), { generation_tps: 60, peak_memory_gb: 2.4 });
    const second = applyInferenceDecode(first, { speculative: true }); // no numbers this turn
    expect(second.tps).toBe(60);
    expect(second.peakMemGib).toBe(2.4);
    expect(second.speculative).toBe(true);
  });

  it("ignores a non-finite / non-number decode figure (honest: never a fabricated readout)", () => {
    const p1 = applyInferenceDecode(inferencePerfInitial(), { generation_tps: "fast", peak_memory_gb: Infinity });
    expect(p1.tps).toBeNull();
    expect(p1.peakMemGib).toBeNull();
  });

});

/* ------------------------------------------------------------- the panel */

describe("InferencePerfPanel", () => {
  it("renders nothing at the awaiting/no-readout resting state", () => {
    expect(renderPanel(inferencePerfInitial())).toBe("");
  });

  it("renders the DECODE row with the mlx_lm-measured tok/s + peak memory", () => {
    const perf = applyInferenceDecode(inferencePerfInitial(), {
      generation_tps: 60.9,
      peak_memory_gb: 2.44,
    });
    const html = renderPanel(perf);
    expect(html).toContain("DECODE");
    expect(html).toContain("60.9 tok/s");
    expect(html).toContain("2.44 GiB peak");
    // The measured number is honestly labelled, not a device-gated estimate.
    expect(html).toMatch(/Measured by mlx_lm/i);
  });

  it("renders the three rows once a turn reports a readout", () => {
    const perf = applyInferencePerf(inferencePerfInitial(), {
      speculative: true,
      quant: "int4",
      throttle: { reason: "low_battery", tier_pref: "fast", defer_heavy: true },
    });
    const html = renderPanel(perf);
    expect(html).toContain("INFERENCE // PERF");
    expect(html).toContain("SPECULATIVE DECODING");
    expect(html).toContain("THROTTLE");
    expect(html).toContain("QUANTIZATION");
    // The path that actually ran.
    expect(html).toContain("ON"); // speculative ran
    expect(html).toContain("INT4"); // quant that loaded
    expect(html).toContain("LOW BATTERY"); // throttle reason
    expect(html).toContain("DEFER HEAVY");
  });

  it("renders the honest OFF/neutral readout (normal gen, auto quant, no throttle)", () => {
    const perf = applyInferencePerf(inferencePerfInitial(), {
      speculative: false,
      quant: "auto",
    });
    const html = renderPanel(perf);
    expect(html).toContain("OFF"); // normal generation
    expect(html).toContain("AUTO"); // loaded as configured
    expect(html).toContain("NONE"); // no throttle
    // No phantom throttle reason.
    expect(html).not.toContain("LOW BATTERY");
    expect(html).not.toContain("THERMAL");
    // Honest device-gated copy is present.
    expect(html).toMatch(/not measured here|device-gated/i);
  });

  it("shows an unknown loaded quant verbatim (never hides the real path)", () => {
    const perf = applyInferencePerf(inferencePerfInitial(), { quant: "int3" });
    const html = renderPanel(perf);
    expect(html).toContain("INT3");
  });

  it("has no action button (read-only)", () => {
    const perf = applyInferencePerf(inferencePerfInitial(), { speculative: true });
    const html = renderPanel(perf);
    expect(html).not.toContain("<button");
  });
});

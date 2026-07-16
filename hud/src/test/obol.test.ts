import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import SpendMeter from "../components/SpendMeter";
import { parseObolSpend, type TelemetryEnvelope } from "../core/events";
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
    ts: `2026-06-16T12:00:${String(counter % 60).padStart(2, "0")}Z`,
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

/** A realistic `obol.spend` payload — modelled EXACTLY on
 *  daemon/src/obol.rs::build_spend_report: a cap is set, today's spend is at the
 *  ~80% ease shoulder, the recent rows carry only NON-SECRET fields. */
const easeReport: Record<string, unknown> = {
  day_spend_usd: 8.4,
  daily_cap_usd: 10.0,
  cap_configured: true,
  headroom_usd: 1.6,
  fraction: 0.84,
  pressure: "ease",
  will_step_down: true,
  reduce_only: true,
  calls_today: 12,
  cost_is_estimate: true,
  recent: [
    {
      ts: 1_700_000_100,
      model: "claude-opus-4-8",
      input_tokens: 1200,
      output_tokens: 340,
      cache_read_tokens: 8000,
      cost_usd: 0.0462,
      agent: "darwin",
    },
    {
      ts: 1_700_000_050,
      model: "claude-haiku-4-5",
      input_tokens: 800,
      output_tokens: 120,
      cache_read_tokens: 0,
      cost_usd: 0.0014,
      agent: "gecko",
    },
  ],
};

/* parser ------------------------------------------------------------------- */

describe("parseObolSpend", () => {
  it("parses a measured EASE report with a cap + recent rows", () => {
    const s = parseObolSpend(easeReport);
    expect(s.capConfigured).toBe(true);
    expect(s.daySpendUsd).toBeCloseTo(8.4, 6);
    expect(s.dailyCapUsd).toBeCloseTo(10.0, 6);
    expect(s.headroomUsd).toBeCloseTo(1.6, 6);
    expect(s.fraction).toBeCloseTo(0.84, 6);
    expect(s.pressure).toBe("ease");
    expect(s.willStepDown).toBe(true);
    expect(s.reduceOnly).toBe(true);
    expect(s.callsToday).toBe(12);
    expect(s.costIsEstimate).toBe(true);
    expect(s.recent).toHaveLength(2);
    expect(s.recent[0]).toEqual({
      ts: 1_700_000_100,
      model: "claude-opus-4-8",
      inputTokens: 1200,
      outputTokens: 340,
      cacheReadTokens: 8000,
      costUsd: 0.0462,
      agent: "darwin",
    });
  });

  it("maps FLOOR (at/over cap) and NONE (no cap) honestly", () => {
    const floor = parseObolSpend({
      day_spend_usd: 12.0,
      daily_cap_usd: 10.0,
      cap_configured: true,
      headroom_usd: 0.0,
      fraction: 1.2,
      pressure: "floor",
      will_step_down: true,
    });
    expect(floor.pressure).toBe("floor");
    expect(floor.willStepDown).toBe(true);
    expect(floor.headroomUsd).toBe(0);

    // The shipped no-cap default: pure accounting, never a step-down.
    const noCap = parseObolSpend({
      day_spend_usd: 42.5,
      daily_cap_usd: 0.0,
      cap_configured: false,
      pressure: "none",
      will_step_down: false,
    });
    expect(noCap.capConfigured).toBe(false);
    expect(noCap.pressure).toBe("none");
    expect(noCap.willStepDown).toBe(false);
    expect(noCap.daySpendUsd).toBeCloseTo(42.5, 6);
  });

  it("never returns null and is fail-safe on a garbled payload", () => {
    // A junk payload yields an honest all-zero/"none" snapshot, never throws.
    const s = parseObolSpend({
      day_spend_usd: "nonsense",
      daily_cap_usd: -3,
      pressure: "explode",
      will_step_down: "maybe",
      calls_today: -7,
      recent: "not-an-array",
    } as unknown as Record<string, unknown>);
    expect(s.daySpendUsd).toBe(0);
    expect(s.dailyCapUsd).toBe(0);
    // An unknown pressure narrows to the safe "none" (no step-down).
    expect(s.pressure).toBe("none");
    expect(s.willStepDown).toBe(false);
    expect(s.callsToday).toBe(0);
    expect(s.recent).toEqual([]);
    // Fail-safe: dollars are always shown as an estimate, budget always reduce-only.
    expect(s.costIsEstimate).toBe(true);
    expect(s.reduceOnly).toBe(true);
  });

  it("is SECRET-FREE — a parsed report carries no utterance text", () => {
    // Even if the wire smuggled an extra field, the parser reads ONLY the known
    // aggregate keys; a leak-canary in an unexpected place never survives.
    const s = parseObolSpend({
      ...easeReport,
      utterance: "SECRET CANARY do-not-leak",
      recent: [
        {
          ts: 1,
          model: "claude-opus-4-8",
          input_tokens: 10,
          output_tokens: 1,
          cache_read_tokens: 0,
          cost_usd: 0.001,
          agent: "darwin",
          prompt: "SECRET CANARY do-not-leak",
        },
      ],
    });
    const json = JSON.stringify(s);
    expect(json).not.toContain("CANARY");
    expect(json).not.toContain("do-not-leak");
    // The row surfaced ONLY its non-secret fields.
    expect(Object.keys(s.recent[0]).sort()).toEqual([
      "agent",
      "cacheReadTokens",
      "costUsd",
      "inputTokens",
      "model",
      "outputTokens",
      "ts",
    ]);
  });
});

/* reducer ------------------------------------------------------------------ */

describe("obol.spend reducer", () => {
  it("folds an obol.spend envelope into state.obolSpend", () => {
    const s0 = connected();
    expect(s0.obolSpend).toBeNull(); // nothing until the first meter emit
    const s1 = tel(s0, env("obol.spend", easeReport));
    expect(s1.obolSpend).not.toBeNull();
    expect(s1.obolSpend?.pressure).toBe("ease");
    expect(s1.obolSpend?.daySpendUsd).toBeCloseTo(8.4, 6);
    expect(s1.obolSpend?.callsToday).toBe(12);
  });

  it("replaces the prior snapshot on the next meter tick (never stacks)", () => {
    let s = tel(connected(), env("obol.spend", easeReport));
    expect(s.obolSpend?.pressure).toBe("ease");
    // A later tick over the cap replaces it wholesale.
    s = tel(
      s,
      env("obol.spend", {
        day_spend_usd: 11.0,
        daily_cap_usd: 10.0,
        cap_configured: true,
        pressure: "floor",
        will_step_down: true,
        calls_today: 15,
      }),
    );
    expect(s.obolSpend?.pressure).toBe("floor");
    expect(s.obolSpend?.callsToday).toBe(15);
  });
});

/* gauge component ---------------------------------------------------------- */

describe("SpendMeter gauge", () => {
  it("renders nothing before the first meter (null spend)", () => {
    const html = renderToStaticMarkup(createElement(SpendMeter, { spend: null }));
    expect(html).toBe("");
  });

  it("renders the day spend, cap, and the EASE budget posture", () => {
    const spend = parseObolSpend(easeReport);
    const html = renderToStaticMarkup(createElement(SpendMeter, { spend }));
    expect(html).toContain("SPEND // CLOUD METER");
    expect(html).toContain("REDUCE-ONLY");
    // The measured dollars are shown as an estimate.
    expect(html).toContain("8.40");
    expect(html).toContain("10.00");
    // The budget pressure pill surfaces EASE.
    expect(html).toContain("EASE");
    // A recent row shows the agent + model (secret-free).
    expect(html).toContain("darwin");
    expect(html).toContain("claude-opus-4-8");
    // No canary can appear (the parser stripped it above).
    expect(html).not.toContain("CANARY");
  });

  it("shows NO CAP / pure-accounting under the shipped no-cap default", () => {
    const spend = parseObolSpend({
      day_spend_usd: 3.25,
      daily_cap_usd: 0.0,
      cap_configured: false,
      pressure: "none",
      will_step_down: false,
      calls_today: 4,
    });
    const html = renderToStaticMarkup(createElement(SpendMeter, { spend }));
    expect(html).toContain("NO CAP");
    // No gauge bar is rendered without a cap (nothing to meter against).
    expect(html).not.toContain("spend-gauge-fill");
  });
});

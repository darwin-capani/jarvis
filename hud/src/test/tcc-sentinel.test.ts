import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import TccSentinelPanel from "../components/TccSentinelPanel";
import { parseTccSnapshot, parseTccAnomalies, TCC_ANOMALY_CAP } from "../core/events";
import type { TccSentinel, TelemetryEnvelope } from "../core/events";
import { initialState, reduce } from "../core/state";
import type { HudState } from "../core/state";

/* helpers ------------------------------------------------------------------ */
let counter = 0;
function env(
  event: string,
  data: Record<string, unknown> = {},
  source = "system",
): TelemetryEnvelope {
  counter += 1;
  return {
    ts: `2026-06-30T12:00:${String(counter % 60).padStart(2, "0")}Z`,
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

/* the defensive parsers ---------------------------------------------------- */
describe("parseTccSnapshot (defensive)", () => {
  it("reads an available snapshot with its counts", () => {
    const s = parseTccSnapshot({ available: true, grants: 12, high_risk_allowed: 3 });
    expect(s).toEqual({ available: true, grants: 12, highRiskAllowed: 3 });
  });

  it("defaults to an honest UNAVAILABLE snapshot when fields are absent", () => {
    const s = parseTccSnapshot({});
    expect(s.available).toBe(false);
    // Counts are meaningless when unavailable — zeroed, never invented.
    expect(s.grants).toBe(0);
    expect(s.highRiskAllowed).toBe(0);
  });

  it("zeroes counts when unavailable even if the payload carries stale numbers", () => {
    const s = parseTccSnapshot({ available: false, grants: 99, high_risk_allowed: 9 });
    expect(s).toEqual({ available: false, grants: 0, highRiskAllowed: 0 });
  });

  it("never throws on junk", () => {
    expect(() => parseTccSnapshot({ grants: "nope" })).not.toThrow();
  });
});

describe("parseTccAnomalies (defensive)", () => {
  it("keeps only non-empty strings", () => {
    const items = parseTccAnomalies({ items: ["NEW grant: a", 7, "", "ESCALATION: b", null] });
    expect(items).toEqual(["NEW grant: a", "ESCALATION: b"]);
  });

  it("returns [] when items is missing or not an array", () => {
    expect(parseTccAnomalies({})).toEqual([]);
    expect(parseTccAnomalies({ items: "nope" })).toEqual([]);
  });

  it("caps the list", () => {
    const many = Array.from({ length: TCC_ANOMALY_CAP + 10 }, (_, i) => `NEW grant: ${i}`);
    expect(parseTccAnomalies({ items: many }).length).toBe(TCC_ANOMALY_CAP);
  });
});

/* the reducer arms --------------------------------------------------------- */
describe("tcc reducer", () => {
  it("sets the sentinel snapshot from tcc.snapshot", () => {
    const s = tel(connected(), env("tcc.snapshot", { available: true, grants: 5, high_risk_allowed: 1 }));
    expect(s.tccSentinel).not.toBeNull();
    expect(s.tccSentinel!.grants).toBe(5);
    expect(s.tccSentinel!.highRiskAllowed).toBe(1);
  });

  it("accumulates + dedupes tcc.anomaly batches, newest-first", () => {
    let s = tel(connected(), env("tcc.anomaly", { items: ["NEW grant: a"] }));
    s = tel(s, env("tcc.anomaly", { items: ["ESCALATION: b [HIGH-RISK]", "NEW grant: a"] }));
    // "NEW grant: a" is not duplicated; the newest batch leads.
    expect(s.tccAnomalies).toEqual(["ESCALATION: b [HIGH-RISK]", "NEW grant: a"]);
  });

  it("ignores an empty anomaly batch (no state churn)", () => {
    const before = tel(connected(), env("tcc.anomaly", { items: ["NEW grant: a"] }));
    const after = tel(before, env("tcc.anomaly", { items: [] }));
    expect(after.tccAnomalies).toEqual(before.tccAnomalies);
  });
});

/* the panel (headless) ----------------------------------------------------- */
describe("TccSentinelPanel (review-only)", () => {
  const render = (sentinel: TccSentinel | null, anomalies: string[] = []) =>
    renderToStaticMarkup(createElement(TccSentinelPanel, { sentinel, anomalies }));

  it("renders nothing before any scan", () => {
    expect(render(null)).toBe("");
  });

  it("shows an honest Full Disk Access hint when the store is unreadable", () => {
    const html = render({ available: false, grants: 0, highRiskAllowed: 0 });
    expect(html).toContain("Full Disk Access");
    expect(html).toContain("REVIEW ONLY");
  });

  it("shows the grant count + high-risk count + anomalies when available", () => {
    const html = render(
      { available: true, grants: 12, highRiskAllowed: 2 },
      ["ESCALATION: com.evil.spy → Screen Recording [HIGH-RISK]"],
    );
    expect(html).toContain("12");
    expect(html).toContain("2 HIGH-RISK ALLOWED");
    expect(html).toContain("com.evil.spy");
    expect(html).toContain("RECENT CHANGES");
  });

  it("is review-only — renders no action button", () => {
    const html = render({ available: true, grants: 1, highRiskAllowed: 0 });
    expect(html).not.toContain("<button");
  });
});

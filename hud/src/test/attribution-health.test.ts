import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import AttributionHealthPanel from "../components/AttributionHealthPanel";
import { parseAttributionHealth, ATTRIBUTION_FLAG_CAP } from "../core/events";
import type { AttributionHealth, TelemetryEnvelope } from "../core/events";
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

const payload: Record<string, unknown> = {
  turns: 42,
  reliable: 3,
  failing: 1,
  flags: [{ kind: "agent", name: "karen", turns: 6, rate: 17 }],
  promote: [{ kind: "skill", name: "base64_encode", turns: 10, rate: 95 }],
};

/* the defensive parser ----------------------------------------------------- */
describe("parseAttributionHealth (defensive)", () => {
  it("parses a well-formed payload", () => {
    const h = parseAttributionHealth(payload);
    expect(h).toEqual({
      turns: 42,
      reliable: 3,
      failing: 1,
      flags: [{ kind: "agent", name: "karen", turns: 6, rate: 17 }],
      promote: [{ kind: "skill", name: "base64_encode", turns: 10, rate: 95 }],
    });
  });

  it("defaults to an all-zero snapshot when fields are absent", () => {
    const h = parseAttributionHealth({});
    expect(h).toEqual({ turns: 0, reliable: 0, failing: 0, flags: [], promote: [] });
  });

  it("drops flags with no name and caps the list", () => {
    const many = Array.from({ length: ATTRIBUTION_FLAG_CAP + 5 }, (_, i) => ({
      kind: "tool",
      name: `t${i}`,
      turns: 6,
      rate: 10,
    }));
    const h = parseAttributionHealth({ flags: [{ kind: "tool" }, 7, ...many] });
    expect(h.flags.length).toBe(ATTRIBUTION_FLAG_CAP);
    expect(h.flags.every((f) => f.name.length > 0)).toBe(true);
  });

  it("never throws on junk", () => {
    expect(() => parseAttributionHealth({ flags: "nope" })).not.toThrow();
    expect(parseAttributionHealth({ flags: "nope" }).flags).toEqual([]);
  });
});

/* the reducer arm ---------------------------------------------------------- */
describe("attribution.health reducer", () => {
  it("sets the snapshot and REPLACES it on the next tick", () => {
    let s = tel(connected(), env("attribution.health", payload));
    expect(s.attributionHealth).not.toBeNull();
    expect(s.attributionHealth!.failing).toBe(1);
    // A later snapshot replaces (current health, not an accumulating log).
    s = tel(s, env("attribution.health", { turns: 50, reliable: 4, failing: 0, flags: [] }));
    expect(s.attributionHealth!.failing).toBe(0);
    expect(s.attributionHealth!.flags).toEqual([]);
  });
});

/* the panel (headless) ----------------------------------------------------- */
describe("AttributionHealthPanel (review-only)", () => {
  const render = (health: AttributionHealth | null) =>
    renderToStaticMarkup(createElement(AttributionHealthPanel, { health }));

  it("renders nothing before any snapshot", () => {
    expect(render(null)).toBe("");
  });

  it("shows reliable/failing counts, the failing capability, and promotion candidates", () => {
    const html = render(parseAttributionHealth(payload));
    expect(html).toContain("REVIEW ONLY");
    expect(html).toContain("RELIABLE");
    expect(html).toContain("FAILING");
    expect(html).toContain("NEEDS ATTENTION");
    expect(html).toContain("karen");
    expect(html).toContain("17% success");
    // The promote section names the eval-verified, live-proven skill.
    expect(html).toContain("READY TO PROMOTE");
    expect(html).toContain("base64_encode");
    expect(html).toContain("95% success");
  });

  it("shows an all-healthy note when nothing is failing, and no promote section when empty", () => {
    const html = render({ turns: 30, reliable: 5, failing: 0, flags: [], promote: [] });
    expect(html).toContain("No well-sampled capability is failing");
    expect(html).not.toContain("NEEDS ATTENTION");
    expect(html).not.toContain("READY TO PROMOTE");
  });

  it("is review-only — renders no action button", () => {
    const html = render(parseAttributionHealth(payload));
    expect(html).not.toContain("<button");
  });
});

import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import CapabilityAtlasPanel from "../components/CapabilityAtlasPanel";
import { parseCapabilityAtlas } from "../core/events";
import type { CapabilityAtlas, TelemetryEnvelope } from "../core/events";
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
  enabled: true,
  armed: 2,
  total: 3,
  capabilities: [
    { name: "GitHub", kind: "integration", armed: true, detail: "connected" },
    {
      name: "Slack",
      kind: "integration",
      armed: false,
      detail: "inert — add in Settings: slack_bot_token",
    },
    { name: "darwin", kind: "agent", armed: true, detail: "Prime Orchestrator" },
  ],
};

/* the defensive parser ----------------------------------------------------- */
describe("parseCapabilityAtlas (defensive)", () => {
  it("parses a well-formed payload", () => {
    const a = parseCapabilityAtlas(payload);
    expect(a.enabled).toBe(true);
    expect(a.armed).toBe(2);
    expect(a.total).toBe(3);
    expect(a.capabilities.length).toBe(3);
    expect(a.capabilities[0]).toEqual({
      name: "GitHub",
      kind: "integration",
      armed: true,
      detail: "connected",
    });
  });

  it("defaults to an empty, OFF snapshot when fields are absent", () => {
    const a = parseCapabilityAtlas({});
    expect(a.enabled).toBe(false);
    expect(a.capabilities).toEqual([]);
    expect(a.armed).toBe(0);
    expect(a.total).toBe(0);
  });

  it("derives counts when absent and drops malformed entries", () => {
    const a = parseCapabilityAtlas({
      enabled: true,
      capabilities: [
        { name: "x", armed: true }, // kept (kind -> "unknown", detail -> "")
        7, // dropped (not an object)
        { kind: "skill" }, // dropped (no name)
        { name: "y", kind: "skill", armed: false, detail: "d" }, // kept
      ],
    });
    expect(a.capabilities.map((c) => c.name)).toEqual(["x", "y"]);
    expect(a.total).toBe(2); // derived from kept entries
    expect(a.armed).toBe(1); // derived (only x is armed)
    expect(a.capabilities[0].kind).toBe("unknown");
    expect(a.capabilities[0].detail).toBe("");
  });

  it("never throws on junk", () => {
    expect(() => parseCapabilityAtlas({ capabilities: "nope" })).not.toThrow();
    expect(parseCapabilityAtlas({ capabilities: "nope" }).capabilities).toEqual([]);
  });

  it("caps a hostile oversized capabilities array", () => {
    const many = Array.from({ length: 1000 }, (_, i) => ({
      name: `c${i}`,
      kind: "skill",
      armed: true,
      detail: "",
    }));
    const a = parseCapabilityAtlas({ capabilities: many });
    expect(a.capabilities.length).toBe(500);
  });
});

/* the reducer arm ---------------------------------------------------------- */
describe("capability.atlas reducer", () => {
  it("sets the snapshot from a well-formed event", () => {
    const s = tel(connected(), env("capability.atlas", payload));
    expect(s.capabilityAtlas).not.toBeNull();
    expect(s.capabilityAtlas!.armed).toBe(2);
    expect(s.capabilityAtlas!.capabilities.length).toBe(3);
  });
});

/* the panel (headless) ----------------------------------------------------- */
describe("CapabilityAtlasPanel (review-only)", () => {
  const render = (atlas: CapabilityAtlas | null) =>
    renderToStaticMarkup(createElement(CapabilityAtlasPanel, { atlas }));

  it("renders nothing before any snapshot", () => {
    expect(render(null)).toBe("");
  });

  it("shows the armed/total summary, the entries, and ARMED + INERT pills", () => {
    const html = render(parseCapabilityAtlas(payload));
    expect(html).toContain("REVIEW ONLY");
    expect(html).toContain("ARMED");
    expect(html).toContain("INERT");
    expect(html).toContain("GitHub");
    expect(html).toContain("Slack");
    expect(html).toContain("connected");
  });

  it("is review-only — renders no action button", () => {
    const html = render(parseCapabilityAtlas(payload));
    expect(html).not.toContain("<button");
  });
});

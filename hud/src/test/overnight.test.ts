import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import OvernightPanel from "../components/OvernightPanel";
import { parseOvernightStatus, type OvernightStatus, type TelemetryEnvelope } from "../core/events";
import { initialState, reduce, type HudState } from "../core/state";

let counter = 0;
function env(event: string, data: Record<string, unknown>, source = "system"): TelemetryEnvelope {
  counter += 1;
  return { ts: `2026-07-13T00:00:${String(counter % 60).padStart(2, "0")}Z`, source, event, data };
}
function connected() {
  return reduce(initialState(), { type: "ws.connected", at: 0 });
}
function tel(state: HudState, e: TelemetryEnvelope) {
  return reduce(state, { type: "telemetry", envelope: e, at: 1000 });
}

/** Mirrors daemon/src/overnight.rs::status_payload. */
const offWire = {
  enabled: false,
  cloud_key_present: false,
  dep_verified: false,
  dependency: "an Anthropic API key (in the Keychain)",
  runs_tools: false,
  queued: 0,
  done: 0,
  failed: 0,
  items: [],
};

describe("parseOvernightStatus (pins the honest invariants)", () => {
  it("parses the off state", () => {
    const s = parseOvernightStatus(offWire);
    expect(s.enabled).toBe(false);
    expect(s.runsTools).toBe(false);
    expect(s.cloudKeyPresent).toBe(false);
    expect(s.queued).toBe(0);
    expect(s.items).toEqual([]);
  });

  it("never lets a payload claim it runs tools", () => {
    const spoofed = parseOvernightStatus({ ...offWire, runs_tools: true, enabled: true });
    expect(spoofed.runsTools).toBe(false); // pinned — overnight work is tool-less
  });

  it("bounds strings, caps items, and drops promptless rows", () => {
    const s = parseOvernightStatus({
      ...offWire,
      enabled: true,
      items: [
        { prompt: "look into A", result: "x".repeat(9999), status: "done" },
        { result: "no prompt", status: "done" }, // dropped
        { prompt: "b", status: "weird" }, // status coerced to failed
        ...Array.from({ length: 50 }, (_, i) => ({ prompt: `p${i}`, result: "r", status: "done" })),
      ],
    });
    expect(s.items.length).toBeLessThanOrEqual(8);
    expect(s.items[0].result.length).toBeLessThanOrEqual(400);
    expect(s.items.find((i) => i.prompt === "b")?.status).toBe("failed");
  });

  it("degrades a malformed frame to the honest off state", () => {
    const d = parseOvernightStatus({});
    expect(d.enabled).toBe(false);
    expect(d.runsTools).toBe(false);
    expect(d.items).toEqual([]);
  });
});

describe("overnight.status reducer", () => {
  it("is null until the first frame, then set", () => {
    let s = connected();
    expect(s.overnight).toBeNull();
    s = tel(s, env("overnight.status", { ...offWire, enabled: true, queued: 3 }));
    expect(s.overnight?.enabled).toBe(true);
    expect(s.overnight?.queued).toBe(3);
  });
});

describe("OvernightPanel", () => {
  const render = (o: OvernightStatus | null) =>
    renderToStaticMarkup(createElement(OvernightPanel, { overnight: o }));

  it("renders nothing before the first frame", () => {
    expect(render(null)).toBe("");
  });

  it("shows OFF and the tool-less footnote", () => {
    const html = render(parseOvernightStatus(offWire));
    expect(html).toContain("OVERNIGHT // ASYNC AGENTS");
    expect(html).toContain("OFF");
    expect(html).toContain("tool-less");
    expect(html).toContain("waits for");
  });

  it("shows ARMED · NEEDS KEY until a cloud key is present, then READY", () => {
    expect(render(parseOvernightStatus({ ...offWire, enabled: true }))).toContain("ARMED · NEEDS KEY");
    expect(render(parseOvernightStatus({ ...offWire, enabled: true, cloud_key_present: true }))).toContain("READY");
  });

  it("lists finished work from the morning brief", () => {
    const html = render(
      parseOvernightStatus({
        ...offWire,
        enabled: true,
        cloud_key_present: true,
        done: 1,
        items: [{ prompt: "research quarterly options", result: "drafted a summary", status: "done" }],
      }),
    );
    expect(html).toContain("research quarterly options");
    expect(html).toContain("drafted a summary");
  });
});

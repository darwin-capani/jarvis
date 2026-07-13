import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import PostureDashboardPanel from "../components/PostureDashboardPanel";
import { parsePostureSnapshot, type TelemetryEnvelope } from "../core/events";
import { initialState, reduce } from "../core/state";

let counter = 0;
function env(event: string, data: Record<string, unknown>, source = "system"): TelemetryEnvelope {
  counter += 1;
  return { ts: `2026-07-13T00:00:${String(counter % 60).padStart(2, "0")}Z`, source, event, data };
}
function connected() {
  return reduce(initialState(), { type: "ws.connected", at: 0 });
}

const protectedWire = {
  filevault: "on",
  firewall: "on",
  sip: "on",
  updates: "up_to_date",
  updates_pending: 0,
  checked_ts: "2026-07-13T10:30:00Z",
};

describe("parsePostureSnapshot (never fabricates protection)", () => {
  it("parses the daemon's exact wire tokens", () => {
    expect(parsePostureSnapshot(protectedWire)).toEqual({
      filevault: "on",
      firewall: "on",
      sip: "on",
      updates: "up_to_date",
      updatesPending: 0,
      checkedTs: "2026-07-13T10:30:00Z",
    });
    expect(
      parsePostureSnapshot({
        filevault: "off",
        firewall: "unreadable",
        sip: "unclear",
        updates: "pending",
        updates_pending: 3,
      }),
    ).toEqual({
      filevault: "off",
      firewall: "unreadable",
      sip: "unclear",
      updates: "pending",
      updatesPending: 3,
      checkedTs: "",
    });
  });

  it("coerces anything unknown to the honest can't-confirm — never 'on'", () => {
    const p = parsePostureSnapshot({
      filevault: "ON", // wrong case is not a match
      firewall: 1,
      sip: "enabled", // not a wire token
      updates: "fine",
      updates_pending: -4,
    });
    expect(p).toEqual({
      filevault: "unclear",
      firewall: "unclear",
      sip: "unclear",
      updates: "unclear",
      updatesPending: 0,
      checkedTs: "",
    });
    // Empty frame: everything unclear, nothing green.
    expect(parsePostureSnapshot({})).toEqual({
      filevault: "unclear",
      firewall: "unclear",
      sip: "unclear",
      updates: "unclear",
      updatesPending: 0,
      checkedTs: "",
    });
  });
});

describe("posture.snapshot reducer", () => {
  it("is null until the first frame, then populated", () => {
    const s0 = connected();
    expect(s0.posture).toBeNull();
    const s1 = reduce(s0, {
      type: "telemetry",
      envelope: env("posture.snapshot", protectedWire),
      at: 1000,
    });
    expect(s1.posture).not.toBeNull();
    expect(s1.posture?.filevault).toBe("on");
    expect(s1.posture?.updates).toBe("up_to_date");
  });
});

describe("PostureDashboardPanel", () => {
  const render = (posture: Parameters<typeof PostureDashboardPanel>[0]["posture"]) =>
    renderToStaticMarkup(createElement(PostureDashboardPanel, { posture }));

  it("renders nothing before the first frame", () => {
    expect(render(null)).toBe("");
  });

  it("shows a fully protected board with green pills", () => {
    const html = render({
      filevault: "on",
      firewall: "on",
      sip: "on",
      updates: "up_to_date",
      updatesPending: 0,
      checkedTs: "2026-07-13T10:30:00Z",
    });
    expect(html).toContain("FileVault");
    expect(html).toContain("Application firewall");
    expect(html).toContain("System Integrity Protection");
    expect(html).toContain("UP TO DATE");
    expect(html).toContain("protected");
    // The read-only honesty note is always present, with the honest data-age
    // stamp (the daemon re-broadcasts a cached snapshot between scans).
    expect(html).toContain("yours to do in System Settings");
    expect(html).toContain("Checked ");
  });

  it("shows exposure and honest can't-confirm distinctly", () => {
    const html = render({
      filevault: "off",
      firewall: "unreadable",
      sip: "unclear",
      updates: "pending",
      updatesPending: 3,
      checkedTs: "",
    });
    expect(html).toContain("OFF");
    expect(html).toContain("exposed");
    expect(html).toContain("UNREADABLE");
    expect(html).toContain("UNCLEAR");
    expect(html).toContain("3 PENDING");
    // Nothing on this board is green.
    expect(html).not.toContain("protected");
  });
});

import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import HandoffPanel from "../components/HandoffPanel";
import { parseHandoffStatus, type HandoffStatus, type TelemetryEnvelope } from "../core/events";
import { initialState, reduce, type HudState } from "../core/state";

let counter = 0;
function env(event: string, data: Record<string, unknown>, source = "system"): TelemetryEnvelope {
  counter += 1;
  return { ts: `2026-07-15T00:00:${String(counter % 60).padStart(2, "0")}Z`, source, event, data };
}
function connected() {
  return reduce(initialState(), { type: "ws.connected", at: 0 });
}
function tel(state: HudState, e: TelemetryEnvelope) {
  return reduce(state, { type: "telemetry", envelope: e, at: 1000 });
}

/** Mirrors daemon/src/handoff.rs::status_payload. */
const offWire = {
  enabled: false,
  key_present: false,
  peer_configured: false,
  transport_inert: true,
  carries_credentials: false,
  restore_parks: true,
  pending_capsule: false,
  device: "",
};

describe("parseHandoffStatus (pins the honest invariants)", () => {
  it("parses the off state", () => {
    expect(parseHandoffStatus(offWire)).toEqual({
      enabled: false,
      keyPresent: false,
      peerConfigured: false,
      transportInert: true,
      carriesCredentials: false,
      restoreParks: true,
      pendingCapsule: false,
      device: "",
    });
  });

  it("never lets a payload claim a live transport, riding credentials, or a non-parking restore", () => {
    const spoofed = parseHandoffStatus({
      ...offWire,
      enabled: true,
      key_present: true,
      peer_configured: true,
      transport_inert: false, // ignored — pinned true
      carries_credentials: true, // ignored — pinned false
      restore_parks: false, // ignored — pinned true
      pending_capsule: true,
      device: "dev-b",
    });
    expect(spoofed.transportInert).toBe(true);
    expect(spoofed.carriesCredentials).toBe(false);
    expect(spoofed.restoreParks).toBe(true);
    expect(spoofed.keyPresent).toBe(true);
    expect(spoofed.peerConfigured).toBe(true);
    expect(spoofed.pendingCapsule).toBe(true);
    expect(spoofed.device).toBe("dev-b");
  });

  it("degrades a malformed frame to the honest off state", () => {
    const d = parseHandoffStatus({});
    expect(d.enabled).toBe(false);
    expect(d.keyPresent).toBe(false);
    expect(d.transportInert).toBe(true);
    expect(d.carriesCredentials).toBe(false);
    expect(d.restoreParks).toBe(true);
    expect(d.pendingCapsule).toBe(false);
    expect(d.device).toBe("");
  });

  it("bounds an over-long device label defensively", () => {
    const d = parseHandoffStatus({ ...offWire, device: "x".repeat(500) });
    expect(d.device.length).toBe(64);
  });
});

describe("handoff.status reducer", () => {
  it("is null until the first frame, then set", () => {
    let s = connected();
    expect(s.handoff).toBeNull();
    s = tel(s, env("handoff.status", { ...offWire, enabled: true, key_present: true, device: "studio" }));
    expect(s.handoff?.enabled).toBe(true);
    expect(s.handoff?.keyPresent).toBe(true);
    expect(s.handoff?.device).toBe("studio");
    // Pinned honest even through the reducer.
    expect(s.handoff?.carriesCredentials).toBe(false);
    expect(s.handoff?.restoreParks).toBe(true);
  });
});

describe("HandoffPanel", () => {
  const render = (handoff: HandoffStatus | null) =>
    renderToStaticMarkup(createElement(HandoffPanel, { handoff }));

  it("renders nothing before the first frame", () => {
    expect(render(null)).toBe("");
  });

  it("shows OFF and the no-credentials / restore-parks / inert-transport footnotes", () => {
    const html = render(parseHandoffStatus(offWire));
    expect(html).toContain("HANDOFF // CONTINUITY");
    expect(html).toContain("OFF");
    expect(html).toContain("end-to-end encrypted");
    expect(html).toContain("carries NO credentials");
    expect(html).toContain("never restores permission");
    expect(html).toContain("armed but inert");
  });

  it("shows ARMED · NEEDS PAIRING until a key exists, then PAIRED", () => {
    expect(render(parseHandoffStatus({ ...offWire, enabled: true }))).toContain("ARMED · NEEDS PAIRING");
    expect(render(parseHandoffStatus({ ...offWire, enabled: true, key_present: true }))).toContain(
      "ARMED · PAIRED",
    );
  });

  it("surfaces 'Resume on <device>' only when armed + paired", () => {
    // Off / needs-pairing: no resume affordance.
    expect(render(parseHandoffStatus({ ...offWire, enabled: true }))).not.toContain("Resume on");
    // Paired with a named device.
    const paired = render(parseHandoffStatus({ ...offWire, enabled: true, key_present: true, device: "mac-studio" }));
    expect(paired).toContain("Resume on mac-studio");
    // Paired without a name falls back to a friendly label, never blank.
    const unnamed = render(parseHandoffStatus({ ...offWire, enabled: true, key_present: true, device: "" }));
    expect(unnamed).toContain("Resume on your other Mac");
  });

  it("surfaces a staged inbound capsule as context-only, parked", () => {
    const html = render(
      parseHandoffStatus({ ...offWire, enabled: true, key_present: true, pending_capsule: true, device: "laptop" }),
    );
    expect(html).toContain("staged");
    expect(html).toContain("authority did not transfer");
  });
});

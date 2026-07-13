import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import SyncPanel from "../components/SyncPanel";
import { parseSyncStatus, type SyncStatus, type TelemetryEnvelope } from "../core/events";
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

/** Mirrors daemon/src/sync.rs::status_payload. */
const offWire = {
  enabled: false,
  key_present: false,
  peer_configured: false,
  transport_inert: true,
  syncable_facts: 0,
  pending_conflicts: 0,
  deletes_propagate: false,
};

describe("parseSyncStatus (pins the honest invariants)", () => {
  it("parses the off state", () => {
    expect(parseSyncStatus(offWire)).toEqual({
      enabled: false,
      keyPresent: false,
      peerConfigured: false,
      transportInert: true,
      syncableFacts: 0,
      pendingConflicts: 0,
      deletesPropagate: false,
    });
  });

  it("never lets a payload claim a live transport or propagating deletes", () => {
    const spoofed = parseSyncStatus({
      ...offWire,
      enabled: true,
      key_present: true,
      transport_inert: false, // ignored — pinned true
      deletes_propagate: true, // ignored — pinned false
      syncable_facts: 120,
      pending_conflicts: 3,
    });
    expect(spoofed.transportInert).toBe(true);
    expect(spoofed.deletesPropagate).toBe(false);
    expect(spoofed.keyPresent).toBe(true);
    expect(spoofed.syncableFacts).toBe(120);
    expect(spoofed.pendingConflicts).toBe(3);
  });

  it("degrades a malformed frame to the honest off state", () => {
    const d = parseSyncStatus({});
    expect(d.enabled).toBe(false);
    expect(d.keyPresent).toBe(false);
    expect(d.transportInert).toBe(true);
    expect(d.deletesPropagate).toBe(false);
  });
});

describe("sync.status reducer", () => {
  it("is null until the first frame, then set", () => {
    let s = connected();
    expect(s.federatedSync).toBeNull();
    s = tel(s, env("sync.status", { ...offWire, enabled: true, key_present: true, syncable_facts: 40 }));
    expect(s.federatedSync?.enabled).toBe(true);
    expect(s.federatedSync?.keyPresent).toBe(true);
  });
});

describe("SyncPanel", () => {
  const render = (sync: SyncStatus | null) =>
    renderToStaticMarkup(createElement(SyncPanel, { sync }));

  it("renders nothing before the first frame", () => {
    expect(render(null)).toBe("");
  });

  it("shows OFF and the E2E / inert-transport / no-delete-propagation footnotes", () => {
    const html = render(parseSyncStatus(offWire));
    expect(html).toContain("SYNC // FEDERATED MEMORY");
    expect(html).toContain("OFF");
    expect(html).toContain("0 facts syncable");
    expect(html).toContain("end-to-end encrypted");
    expect(html).toContain("armed");
    expect(html).toContain("Deletions");
  });

  it("shows ARMED · NEEDS PAIRING until a key exists, then PAIRED", () => {
    expect(render(parseSyncStatus({ ...offWire, enabled: true }))).toContain("ARMED · NEEDS PAIRING");
    expect(render(parseSyncStatus({ ...offWire, enabled: true, key_present: true }))).toContain(
      "ARMED · PAIRED",
    );
  });

  it("surfaces pending conflicts as never-silently-overwritten", () => {
    const html = render(parseSyncStatus({ ...offWire, enabled: true, key_present: true, pending_conflicts: 2 }));
    expect(html).toContain("2 conflicts to");
    expect(html).toContain("never overwrote yours silently");
  });
});

import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import PasteboardPanel from "../components/PasteboardPanel";
import {
  parsePasteboardStatus,
  PASTEBOARD_PREVIEW_CAP,
  type PasteboardStatus,
  type TelemetryEnvelope,
} from "../core/events";
import { initialState, reduce } from "../core/state";

let counter = 0;
function env(event: string, data: Record<string, unknown>, source = "pasteboard"): TelemetryEnvelope {
  counter += 1;
  return { ts: `2026-07-15T00:00:${String(counter % 60).padStart(2, "0")}Z`, source, event, data };
}
function connected() {
  return reduce(initialState(), { type: "ws.connected", at: 0 });
}

describe("parsePasteboardStatus (never invents a captured state)", () => {
  it("parses a well-formed enabled payload with redacted previews", () => {
    const p = parsePasteboardStatus({
      enabled: true,
      count: 3,
      cap: 50,
      poll_interval_secs: 3,
      recent: ["the office lease renews in March", "email [redacted] about the invoice"],
    });
    expect(p).toEqual({
      enabled: true,
      count: 3,
      cap: 50,
      pollIntervalSecs: 3,
      recent: ["the office lease renews in March", "email [redacted] about the invoice"],
    });
  });

  it("coerces an absent/garbled payload to the honest OFF, empty snapshot", () => {
    expect(parsePasteboardStatus({})).toEqual({
      enabled: false,
      count: 0,
      cap: 0,
      pollIntervalSecs: 0,
      recent: [],
    });
    // A garbled recent (not an array) degrades to no previews, never throws.
    expect(parsePasteboardStatus({ enabled: true, recent: 5 }).recent).toEqual([]);
    // Non-string entries are dropped rather than failing the whole field.
    expect(
      parsePasteboardStatus({ enabled: true, recent: ["ok", 7, null, "two"] }).recent,
    ).toEqual(["ok", "two"]);
  });

  it("drops previews when OFF (a disabled pasteboard shows nothing captured)", () => {
    const p = parsePasteboardStatus({
      enabled: false,
      count: 2,
      cap: 10,
      poll_interval_secs: 3,
      recent: ["a leaked preview", "another"],
    });
    expect(p.enabled).toBe(false);
    expect(p.recent).toEqual([]);
  });

  it("clamps negative / fractional counts and caps the preview list", () => {
    const many = Array.from({ length: PASTEBOARD_PREVIEW_CAP + 5 }, (_, i) => `clip ${i}`);
    const p = parsePasteboardStatus({
      enabled: true,
      count: -4,
      cap: 12.9,
      poll_interval_secs: -1,
      recent: many,
    });
    expect(p.count).toBe(0);
    expect(p.cap).toBe(12);
    expect(p.pollIntervalSecs).toBe(0);
    expect(p.recent).toHaveLength(PASTEBOARD_PREVIEW_CAP);
  });
});

describe("reduce (pasteboard.status)", () => {
  it("is null until the first status frame", () => {
    expect(connected().pasteboard).toBeNull();
  });

  it("stores the parsed status on a pasteboard.status frame", () => {
    const s = reduce(
      connected(),
      {
        type: "telemetry",
        envelope: env("pasteboard.status", {
          enabled: true,
          count: 1,
          cap: 50,
          poll_interval_secs: 3,
          recent: ["the lease renews in March"],
        }),
        at: 1,
      },
    );
    expect(s.pasteboard).not.toBeNull();
    expect(s.pasteboard?.enabled).toBe(true);
    expect(s.pasteboard?.count).toBe(1);
    expect(s.pasteboard?.recent).toEqual(["the lease renews in March"]);
  });

  it("reflects a later OFF frame (disable wipes the previews)", () => {
    let s = reduce(connected(), {
      type: "telemetry",
      envelope: env("pasteboard.status", { enabled: true, count: 2, cap: 10, recent: ["x", "y"] }),
      at: 1,
    });
    expect(s.pasteboard?.recent).toEqual(["x", "y"]);
    s = reduce(s, {
      type: "telemetry",
      envelope: env("pasteboard.status", { enabled: false, count: 0, cap: 10, recent: [] }),
      at: 2,
    });
    expect(s.pasteboard?.enabled).toBe(false);
    expect(s.pasteboard?.recent).toEqual([]);
  });
});

describe("PasteboardPanel", () => {
  function render(pasteboard: PasteboardStatus | null): string {
    return renderToStaticMarkup(createElement(PasteboardPanel, { pasteboard }));
  }

  it("renders nothing until a status arrives", () => {
    expect(render(null)).toBe("");
  });

  it("renders the OFF state with an opt-in note and no clips", () => {
    const html = render({ enabled: false, count: 0, cap: 50, pollIntervalSecs: 3, recent: [] });
    expect(html).toContain("OFF");
    expect(html.toLowerCase()).toContain("opt-in");
    expect(html).not.toContain("<li");
  });

  it("renders the redacted clips when capturing", () => {
    const html = render({
      enabled: true,
      count: 2,
      cap: 50,
      pollIntervalSecs: 3,
      recent: ["the office lease renews in March", "email [redacted] about the invoice"],
    });
    expect(html).toContain("CAPTURING");
    expect(html).toContain("the office lease renews in March");
    expect(html).toContain("[redacted]");
    expect(html).toContain("2 / 50 clips");
  });

  it("shows an honest empty note when enabled but nothing copied yet", () => {
    const html = render({ enabled: true, count: 0, cap: 50, pollIntervalSecs: 3, recent: [] });
    expect(html).toContain("CAPTURING");
    expect(html.toLowerCase()).toContain("nothing copied yet");
  });
});

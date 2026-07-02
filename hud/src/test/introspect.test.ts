import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import IntrospectPanel from "../components/IntrospectPanel";
import {
  parseIntrospectSnapshot,
  introspectDriftLine,
  introspectAnomalyLine,
  introspectModuleViolationLine,
  introspectSecurityLine,
  parseIntrospectCapabilities,
  mergeIntrospectAlert,
  INTROSPECT_ALERT_CAP,
} from "../core/events";
import type { IntrospectStatus, TelemetryEnvelope } from "../core/events";
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
    ts: `2026-07-02T12:00:${String(counter % 60).padStart(2, "0")}Z`,
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
describe("parseIntrospectSnapshot (defensive)", () => {
  it("reads a snapshot with its counts", () => {
    const s = parseIntrospectSnapshot({ apps: 3, drift: 1, anomalies: 2 });
    expect(s).toEqual({ apps: 3, drift: 1, anomalies: 2 });
  });
  it("defaults to an honest all-zero snapshot when fields are absent", () => {
    expect(parseIntrospectSnapshot({})).toEqual({ apps: 0, drift: 0, anomalies: 0 });
  });
  it("never throws on junk", () => {
    expect(() => parseIntrospectSnapshot({ apps: "nope" })).not.toThrow();
  });
});

describe("introspect finding formatters (defensive)", () => {
  it("formats profile drift and the missing-file case", () => {
    expect(introspectDriftLine({ app: "global-scan" })).toContain("PROFILE DRIFT: global-scan");
    expect(introspectDriftLine({ app: "global-scan", missing: true })).toContain("PROFILE MISSING");
  });
  it("formats an anomaly with and without detail", () => {
    expect(introspectAnomalyLine({ app: "x", kind: "cpu_spike", detail: "99% > 95%" })).toBe(
      "ANOMALY [cpu_spike]: x — 99% > 95%",
    );
    expect(introspectAnomalyLine({ app: "x", kind: "rss_growth" })).toBe("ANOMALY [rss_growth]: x");
  });
  it("formats a module violation", () => {
    expect(introspectModuleViolationLine({ app: "x", path: "/tmp/evil.dylib" })).toBe(
      "MODULE: x loaded unexpected /tmp/evil.dylib",
    );
  });
  it("formats a security event, tagging high events SECURITY", () => {
    expect(
      introspectSecurityLine({ app: "gs", kind: "wx_violation", high: true, detail: "jit=false" }),
    ).toBe("SECURITY [wx_violation]: gs — jit=false");
    expect(introspectSecurityLine({ app: "gs", kind: "signal", high: false })).toBe(
      "notice [signal]: gs",
    );
    expect(introspectSecurityLine({ kind: "wx_violation" })).toBeNull(); // no app
  });
  it("returns null when the structural anchor is missing", () => {
    expect(introspectDriftLine({})).toBeNull();
    expect(introspectAnomalyLine({ app: "x" })).toBeNull(); // no kind
    expect(introspectModuleViolationLine({ app: "x" })).toBeNull(); // no path
  });
  it("mergeIntrospectAlert dedupes newest-first and caps", () => {
    let alerts: string[] = [];
    alerts = mergeIntrospectAlert("A", alerts);
    alerts = mergeIntrospectAlert("B", alerts);
    alerts = mergeIntrospectAlert("A", alerts); // dup collapses, moves to front
    expect(alerts).toEqual(["A", "B"]);
    for (let i = 0; i < INTROSPECT_ALERT_CAP + 10; i++) alerts = mergeIntrospectAlert(`L${i}`, alerts);
    expect(alerts.length).toBe(INTROSPECT_ALERT_CAP);
  });
});

describe("parseIntrospectCapabilities (defensive)", () => {
  it("sorts by name, dedupes, and drops nameless entries", () => {
    const caps = parseIntrospectCapabilities({
      apps: [
        { name: "vision", caps: "camera, screen" },
        { name: "global-scan", caps: "net(2)" },
        { name: "global-scan", caps: "net(2)" }, // dup -> collapsed
        { caps: "orphan" }, // no name -> dropped
        "junk",
      ],
    });
    expect(caps).toEqual([
      { name: "global-scan", caps: "net(2)" },
      { name: "vision", caps: "camera, screen" },
    ]);
  });
  it("returns [] for a missing/non-array payload and never throws", () => {
    expect(parseIntrospectCapabilities({})).toEqual([]);
    expect(() => parseIntrospectCapabilities({ apps: "nope" })).not.toThrow();
  });
});

/* the reducer arms --------------------------------------------------------- */
describe("introspect reducer", () => {
  it("sets the snapshot from introspect.snapshot", () => {
    const s = tel(connected(), env("introspect.snapshot", { apps: 4, drift: 0, anomalies: 1 }));
    expect(s.introspect).not.toBeNull();
    expect(s.introspect!.apps).toBe(4);
    expect(s.introspect!.anomalies).toBe(1);
  });

  it("accumulates + dedupes findings across event types, newest-first", () => {
    let s = tel(connected(), env("introspect.profile_drift", { app: "a" }));
    s = tel(s, env("introspect.anomaly", { app: "b", kind: "cpu_spike", detail: "hot" }));
    s = tel(s, env("introspect.module_violation", { app: "c", path: "/x.dylib" }));
    s = tel(s, env("introspect.security_event", { app: "d", kind: "wx_violation", high: true, detail: "jit=false" }));
    // repeat the drift — deduped, not doubled.
    s = tel(s, env("introspect.profile_drift", { app: "a" }));
    expect(s.introspectAlerts).toEqual([
      "PROFILE DRIFT: a — on-disk seatbelt profile changed since launch",
      "SECURITY [wx_violation]: d — jit=false",
      "MODULE: c loaded unexpected /x.dylib",
      "ANOMALY [cpu_spike]: b — hot",
    ]);
  });

  it("ignores an unusable finding (no state churn)", () => {
    const before = tel(connected(), env("introspect.profile_drift", { app: "a" }));
    const after = tel(before, env("introspect.module_violation", { app: "c" })); // no path
    expect(after.introspectAlerts).toEqual(before.introspectAlerts);
  });

  it("replaces the capability inventory wholesale each tick", () => {
    let s = tel(connected(), env("introspect.capabilities", { apps: [{ name: "a", caps: "net(1)" }] }));
    expect(s.introspectCapabilities).toEqual([{ name: "a", caps: "net(1)" }]);
    // A later tick fully replaces (not merges) the inventory.
    s = tel(s, env("introspect.capabilities", { apps: [{ name: "b", caps: "gpu" }] }));
    expect(s.introspectCapabilities).toEqual([{ name: "b", caps: "gpu" }]);
  });
});

/* the panel (headless) ----------------------------------------------------- */
describe("IntrospectPanel (review-only)", () => {
  const render = (
    status: IntrospectStatus | null,
    alerts: string[] = [],
    capabilities: { name: string; caps: string }[] = [],
  ) => renderToStaticMarkup(createElement(IntrospectPanel, { status, alerts, capabilities }));

  it("renders nothing before any tick", () => {
    expect(render(null)).toBe("");
  });

  it("shows the observed count + drift/anomaly summary + findings", () => {
    const html = render({ apps: 5, drift: 1, anomalies: 2 }, [
      "MODULE: vision loaded unexpected /tmp/inject.dylib",
    ]);
    expect(html).toContain("5");
    expect(html).toContain("1 DRIFT · 2 ANOMALIES");
    expect(html).toContain("/tmp/inject.dylib");
    expect(html).toContain("RECENT FINDINGS");
    expect(html).toContain("REVIEW ONLY");
  });

  it("shows the declared-capability inventory when present", () => {
    const html = render({ apps: 2, drift: 0, anomalies: 0 }, [], [
      { name: "global-scan", caps: "net(2), fs_read(1)" },
      { name: "vision", caps: "camera, screen" },
    ]);
    expect(html).toContain("DECLARED CAPABILITIES");
    expect(html).toContain("global-scan");
    expect(html).toContain("net(2), fs_read(1)");
    expect(html).toContain("camera, screen");
  });

  it("is review-only — renders no action button", () => {
    const html = render({ apps: 1, drift: 0, anomalies: 0 });
    expect(html).not.toContain("<button");
  });
});

import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import VitalsPanel from "../components/VitalsPanel";
import {
  parseVitals,
  memUsedPercent,
  volumeUsedPercent,
  cpuAverage,
  VITALS_MAX_VOLUMES,
  VITALS_MAX_CORES,
  type HardwareVitals,
} from "../core/vitals";
import { initialState, reduce } from "../core/state";
import type { HudState } from "../core/state";
import type { TelemetryEnvelope } from "../core/events";

/* helpers ------------------------------------------------------------------ */
let counter = 0;
function env(event: string, data: Record<string, unknown> = {}): TelemetryEnvelope {
  counter += 1;
  return {
    ts: `2026-07-16T12:00:${String(counter % 60).padStart(2, "0")}Z`,
    source: "system",
    event,
    data,
  };
}
function tel(state: HudState, e: TelemetryEnvelope, at = 1000): HudState {
  return reduce(state, { type: "telemetry", envelope: e, at });
}

const FULL = {
  battery: { percent: 73, on_ac: false, charge_state: "discharging" },
  thermal: "serious",
  memory: { used_bytes: 12_000_000_000, total_bytes: 16_000_000_000, pressure: "warn" },
  cpu: { per_core: [10, 20, 90, 5], load_avg: [1.2, 1.1, 0.9] },
  volumes: [
    { label: "Macintosh HD", mount: "/", free_bytes: 100_000_000_000, total_bytes: 500_000_000_000 },
    { label: "Data", mount: "/System/Volumes/Data", free_bytes: 50, total_bytes: 200 },
  ],
  uptime_secs: 90_061,
};

/* ------------------------------------------------------------------------- */
describe("parseVitals", () => {
  it("parses a full, honest snapshot", () => {
    const v = parseVitals(FULL);
    expect(v.battery).toEqual({ percent: 73, onAc: false, chargeState: "discharging" });
    expect(v.thermal).toBe("serious");
    expect(v.memory).toEqual({
      usedBytes: 12_000_000_000,
      totalBytes: 16_000_000_000,
      pressure: "warn",
    });
    expect(v.cpu.perCore).toEqual([10, 20, 90, 5]);
    expect(v.cpu.loadAvg).toEqual([1.2, 1.1, 0.9]);
    expect(v.volumes).toHaveLength(2);
    expect(v.volumes[0]).toEqual({
      label: "Macintosh HD",
      mount: "/",
      freeBytes: 100_000_000_000,
      totalBytes: 500_000_000_000,
    });
    expect(v.uptimeSecs).toBe(90_061);
  });

  it("degrades a fully-malformed frame to honest unknowns, never throwing", () => {
    const v = parseVitals({} as Record<string, unknown>);
    expect(v.battery).toEqual({ percent: null, onAc: false, chargeState: "unknown" });
    expect(v.thermal).toBe("unknown");
    expect(v.memory).toEqual({ usedBytes: null, totalBytes: null, pressure: "unknown" });
    expect(v.cpu.perCore).toEqual([]);
    expect(v.cpu.loadAvg).toBeNull();
    expect(v.volumes).toEqual([]);
    expect(v.uptimeSecs).toBeNull();
  });

  it("a desktop Mac (no battery) reads null percent, never a fabricated low", () => {
    const v = parseVitals({ ...FULL, battery: { percent: null, on_ac: true, charge_state: "unknown" } });
    expect(v.battery.percent).toBeNull();
    expect(v.battery.onAc).toBe(true);
    expect(v.battery.chargeState).toBe("unknown");
  });

  it("coerces unknown enum values to 'unknown'", () => {
    const v = parseVitals({
      ...FULL,
      thermal: "meltdown",
      battery: { percent: 50, on_ac: true, charge_state: "sideways" },
      memory: { used_bytes: 1, total_bytes: 2, pressure: "panic" },
    });
    expect(v.thermal).toBe("unknown");
    expect(v.battery.chargeState).toBe("unknown");
    expect(v.memory.pressure).toBe("unknown");
  });

  it("clamps battery percent and per-core CPU into 0..100 and drops NaN cores", () => {
    const v = parseVitals({
      ...FULL,
      battery: { percent: 150, on_ac: true, charge_state: "charged" },
      cpu: { per_core: [-5, 200, 42.7, Number.NaN, "x"], load_avg: [1, 2, 3] },
    });
    expect(v.battery.percent).toBe(100);
    // -5 -> 0, 200 -> 100, 42.7 kept, NaN + "x" dropped.
    expect(v.cpu.perCore).toEqual([0, 100, 42.7]);
  });

  it("drops zero-capacity / malformed volumes and clamps free to total", () => {
    const v = parseVitals({
      ...FULL,
      volumes: [
        { label: "ok", mount: "/ok", free_bytes: 10, total_bytes: 100 },
        { label: "zero", mount: "/z", free_bytes: 0, total_bytes: 0 }, // dropped
        { label: "neg", mount: "/n", free_bytes: -1, total_bytes: 100 }, // dropped
        { label: "overfull", mount: "/o", free_bytes: 999, total_bytes: 100 }, // free clamped
        "not an object",
      ],
    });
    expect(v.volumes.map((x) => x.mount)).toEqual(["/ok", "/o"]);
    expect(v.volumes[1].freeBytes).toBe(100); // clamped to total
  });

  it("bounds hostile per-core and volume arrays", () => {
    const cores = Array.from({ length: VITALS_MAX_CORES + 50 }, () => 1);
    const vols = Array.from({ length: VITALS_MAX_VOLUMES + 50 }, (_, i) => ({
      label: `v${i}`,
      mount: `/v${i}`,
      free_bytes: 1,
      total_bytes: 2,
    }));
    const v = parseVitals({ ...FULL, cpu: { per_core: cores, load_avg: [0, 0, 0] }, volumes: vols });
    expect(v.cpu.perCore).toHaveLength(VITALS_MAX_CORES);
    expect(v.volumes).toHaveLength(VITALS_MAX_VOLUMES);
  });

  it("rejects a load average that isn't three finite numbers", () => {
    expect(parseVitals({ ...FULL, cpu: { per_core: [1], load_avg: [1, 2] } }).cpu.loadAvg).toBeNull();
    expect(
      parseVitals({ ...FULL, cpu: { per_core: [1], load_avg: [1, Number.NaN, 3] } }).cpu.loadAvg,
    ).toBeNull();
  });
});

describe("presentation helpers", () => {
  it("memUsedPercent handles unknown/zero total", () => {
    expect(memUsedPercent({ usedBytes: 12, totalBytes: 16, pressure: "warn" })).toBe(75);
    expect(memUsedPercent({ usedBytes: null, totalBytes: 16, pressure: "unknown" })).toBeNull();
    expect(memUsedPercent({ usedBytes: 5, totalBytes: 0, pressure: "unknown" })).toBeNull();
  });

  it("volumeUsedPercent computes used share and clamps", () => {
    expect(volumeUsedPercent({ label: "", mount: "/", freeBytes: 100, totalBytes: 500 })).toBe(80);
    expect(volumeUsedPercent({ label: "", mount: "/", freeBytes: 0, totalBytes: 0 })).toBe(0);
  });

  it("cpuAverage means the cores, null when empty", () => {
    expect(cpuAverage([10, 20, 30])).toBe(20);
    expect(cpuAverage([])).toBeNull();
  });
});

describe("reducer: hardware.vitals", () => {
  it("folds a hardware.vitals frame into state.vitals (read-only surface)", () => {
    const s0 = initialState();
    expect(s0.vitals).toBeNull();
    const s1 = tel(s0, env("hardware.vitals", FULL));
    expect(s1.vitals?.thermal).toBe("serious");
    expect(s1.vitals?.battery.percent).toBe(73);
    expect(s1.vitals?.volumes).toHaveLength(2);
  });

  it("a malformed frame still yields an honest snapshot (never throws / never stale-fabricates)", () => {
    const s = tel(initialState(), env("hardware.vitals", { thermal: 5, volumes: "nope" }));
    expect(s.vitals?.thermal).toBe("unknown");
    expect(s.vitals?.volumes).toEqual([]);
  });
});

describe("VitalsPanel", () => {
  const full: HardwareVitals = parseVitals(FULL);

  it("renders battery, thermal, memory and volumes honestly", () => {
    const html = renderToStaticMarkup(createElement(VitalsPanel, { vitals: full }));
    expect(html).toContain("SYS // VITALS");
    expect(html).toContain("hardware.vitals");
    expect(html).toContain("73%");
    expect(html).toContain("SERIOUS");
    expect(html).toContain("Macintosh HD");
    expect(html).toContain("free");
  });

  it("renders an honest empty state when no vitals have been read", () => {
    const html = renderToStaticMarkup(createElement(VitalsPanel, { vitals: null }));
    expect(html).toContain("no hardware vitals read yet");
  });

  it("shows AC (never a fabricated charge) for a desktop Mac with no battery", () => {
    const desktop = parseVitals({
      ...FULL,
      battery: { percent: null, on_ac: true, charge_state: "unknown" },
    });
    const html = renderToStaticMarkup(createElement(VitalsPanel, { vitals: desktop }));
    expect(html).toContain("AC");
    expect(html).not.toContain("73%");
  });
});

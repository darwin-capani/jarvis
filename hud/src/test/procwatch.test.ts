import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import ProcPanel from "../components/ProcPanel";
import {
  parseProcesses,
  maxCpu,
  maxMem,
  sharePct,
  PROC_MAX_TOP,
  PROC_MAX_NAME,
  type ProcessesFrame,
} from "../core/procwatch";
import { initialState, reduce } from "../core/state";
import type { HudState } from "../core/state";
import type { TelemetryEnvelope } from "../core/events";

/* helpers ------------------------------------------------------------------ */
let counter = 0;
function env(event: string, data: Record<string, unknown> = {}): TelemetryEnvelope {
  counter += 1;
  return {
    ts: `2026-07-17T12:00:${String(counter % 60).padStart(2, "0")}Z`,
    source: "system",
    event,
    data,
  };
}
function tel(state: HudState, e: TelemetryEnvelope, at = 1000): HudState {
  return reduce(state, { type: "telemetry", envelope: e, at });
}

const FULL = {
  total: 423,
  new_since_poll: 3,
  top_cpu: [
    { name: "darwind", pid: 501, ppid: 1, uid: 501, cpu_pct: 142.5, mem_bytes: 512_000_000 },
    { name: "WindowServer", pid: 402, ppid: 1, uid: 88, cpu_pct: 31.2, mem_bytes: 900_000_000 },
  ],
  top_mem: [
    { name: "inferenced", pid: 777, ppid: 501, uid: 501, cpu_pct: 8.0, mem_bytes: 4_200_000_000 },
    { name: "WindowServer", pid: 402, ppid: 1, uid: 88, cpu_pct: 31.2, mem_bytes: 900_000_000 },
  ],
  load_avg: [2.31, 1.9, 1.4],
};

/** The daemon's FIRST-poll frame: cpu is a two-sample delta, so top_cpu is
 *  honestly EMPTY, every entry cpu_pct is null, and new_since_poll is null. */
const FIRST_POLL = {
  total: 421,
  new_since_poll: null,
  top_cpu: [],
  top_mem: [
    { name: "inferenced", pid: 777, ppid: 501, uid: 501, cpu_pct: null, mem_bytes: 4_200_000_000 },
    { name: "WindowServer", pid: 402, ppid: 1, uid: 88, cpu_pct: null, mem_bytes: 900_000_000 },
  ],
  load_avg: [2.31, 1.9, 1.4],
};

/* ------------------------------------------------------------------------- */
describe("parseProcesses", () => {
  it("parses a full, honest frame", () => {
    const p = parseProcesses(FULL);
    expect(p.total).toBe(423);
    expect(p.newSincePoll).toBe(3);
    expect(p.topCpu).toHaveLength(2);
    expect(p.topCpu[0]).toEqual({
      name: "darwind",
      pid: 501,
      ppid: 1,
      uid: 501,
      cpuPct: 142.5, // >100 is HONEST on multi-core — not clamped to 100
      memBytes: 512_000_000,
    });
    expect(p.topMem[0].name).toBe("inferenced");
    expect(p.loadAvg).toEqual([2.31, 1.9, 1.4]);
  });

  it("degrades a fully-malformed frame to honest unknowns, never throwing", () => {
    const p = parseProcesses({} as Record<string, unknown>);
    expect(p.total).toBeNull();
    expect(p.newSincePoll).toBeNull();
    expect(p.topCpu).toEqual([]);
    expect(p.topMem).toEqual([]);
    expect(p.loadAvg).toBeNull();
  });

  it("preserves the first-poll null new_since_poll, never a fabricated 0", () => {
    // The daemon has no baseline on its first poll and says null. Coercing
    // that to 0 would claim "nothing new" that was never measured.
    const p = parseProcesses({ ...FULL, new_since_poll: null });
    expect(p.newSincePoll).toBeNull();
  });

  it("preserves the whole first-poll warm-up shape honestly", () => {
    // cpu% is a two-sample delta: the first frame carries an EMPTY top_cpu
    // and null cpu_pct on every top_mem entry. The parser must keep those
    // nulls — coercing any to 0 would fabricate a "0.0% cpu" never measured.
    const p = parseProcesses(FIRST_POLL);
    expect(p.topCpu).toEqual([]);
    expect(p.newSincePoll).toBeNull();
    expect(p.topMem).toHaveLength(2);
    expect(p.topMem[0].cpuPct).toBeNull();
    expect(p.topMem[0].memBytes).toBe(4_200_000_000);
  });

  it("drops rows without a valid pid and preserves unreadable fields as null", () => {
    const p = parseProcesses({
      ...FULL,
      top_cpu: [
        { name: "no-pid", cpu_pct: 99 }, // dropped: unkeyable
        { name: "frac-pid", pid: 1.5, cpu_pct: 99 }, // dropped: not an integer
        { name: "neg-pid", pid: -4, cpu_pct: 99 }, // dropped: negative
        { name: "bare", pid: 7 }, // kept: cpu/mem honestly unknown
        "not an object",
      ],
    });
    expect(p.topCpu).toHaveLength(1);
    expect(p.topCpu[0]).toEqual({
      name: "bare",
      pid: 7,
      ppid: null,
      uid: null,
      cpuPct: null,
      memBytes: null,
    });
  });

  it("clamps negative cpu to 0 and drops negative memory to null", () => {
    const p = parseProcesses({
      ...FULL,
      top_cpu: [{ name: "x", pid: 1, cpu_pct: -5, mem_bytes: -1 }],
    });
    expect(p.topCpu[0].cpuPct).toBe(0);
    expect(p.topCpu[0].memBytes).toBeNull();
  });

  it("bounds hostile arrays and giant names", () => {
    const rows = Array.from({ length: PROC_MAX_TOP + 50 }, (_, i) => ({
      name: "x".repeat(10_000),
      pid: i,
      cpu_pct: 1,
      mem_bytes: 1,
    }));
    const p = parseProcesses({ ...FULL, top_cpu: rows, top_mem: rows });
    expect(p.topCpu).toHaveLength(PROC_MAX_TOP);
    expect(p.topMem).toHaveLength(PROC_MAX_TOP);
    expect(p.topCpu[0].name).toHaveLength(PROC_MAX_NAME);
  });

  it("rejects counts and load that aren't honest numbers", () => {
    const p = parseProcesses({
      ...FULL,
      total: -1,
      new_since_poll: 2.5,
      load_avg: [1, Number.NaN, 3],
    });
    expect(p.total).toBeNull();
    expect(p.newSincePoll).toBeNull();
    expect(p.loadAvg).toBeNull();
    expect(parseProcesses({ ...FULL, load_avg: [1, 2] }).loadAvg).toBeNull();
  });
});

describe("presentation helpers", () => {
  const full: ProcessesFrame = parseProcesses(FULL);

  it("maxCpu / maxMem find the list peak, null when unreadable", () => {
    expect(maxCpu(full.topCpu)).toBe(142.5);
    expect(maxMem(full.topMem)).toBe(4_200_000_000);
    expect(maxCpu([])).toBeNull();
    expect(
      maxCpu([{ name: "x", pid: 1, ppid: null, uid: null, cpuPct: null, memBytes: null }]),
    ).toBeNull();
  });

  it("sharePct scales against the peak and degrades to an honest empty bar", () => {
    expect(sharePct(50, 100)).toBe(50);
    expect(sharePct(200, 100)).toBe(100); // clamped
    expect(sharePct(null, 100)).toBe(0); // unreadable value -> empty, not faked
    expect(sharePct(50, null)).toBe(0); // no peak -> empty
    expect(sharePct(50, 0)).toBe(0); // zero peak -> empty, no division
  });
});

describe("reducer: system.processes", () => {
  it("folds a system.processes frame into state.processes (read-only surface)", () => {
    const s0 = initialState();
    expect(s0.processes).toBeNull();
    const s1 = tel(s0, env("system.processes", FULL));
    expect(s1.processes?.total).toBe(423);
    expect(s1.processes?.topCpu[0].name).toBe("darwind");
    expect(s1.processes?.newSincePoll).toBe(3);
  });

  it("a malformed frame still yields an honest snapshot (never throws / never stale-fabricates)", () => {
    const s = tel(initialState(), env("system.processes", { total: "many", top_cpu: "nope" }));
    expect(s.processes?.total).toBeNull();
    expect(s.processes?.topCpu).toEqual([]);
  });
});

describe("ProcPanel", () => {
  const full: ProcessesFrame = parseProcesses(FULL);

  it("renders counts and both top lists honestly", () => {
    const html = renderToStaticMarkup(createElement(ProcPanel, { proc: full }));
    expect(html).toContain("SYS // PROCESSES");
    expect(html).toContain("system.processes");
    expect(html).toContain("423");
    expect(html).toContain("darwind");
    expect(html).toContain("inferenced");
    expect(html).toContain("142.5%");
    expect(html).toContain("4.2 GB");
  });

  it("renders an honest empty state when no frame has been read", () => {
    const html = renderToStaticMarkup(createElement(ProcPanel, { proc: null }));
    expect(html).toContain("no process snapshot read yet");
  });

  it("shows — for the first-poll unknown new-count, never a fabricated 0", () => {
    const first = parseProcesses({ ...FULL, new_since_poll: null });
    const html = renderToStaticMarkup(createElement(ProcPanel, { proc: first }));
    expect(html).toContain("new since poll");
    expect(html).toContain("—");
  });

  it("renders the first-poll frame as an honest warm-up, never 0.0%", () => {
    // First poll: processes ARE visible (total > 0) but cpu has no baseline
    // yet — the TOP CPU list must read as warming up, and every entry's cpu
    // column as "—". A fabricated "0.0%" would claim a measurement that was
    // never taken (the vitals on_ac precedent).
    const first = parseProcesses(FIRST_POLL);
    const html = renderToStaticMarkup(createElement(ProcPanel, { proc: first }));
    expect(html).toContain("cpu warming up — deltas need two polls");
    expect(html).not.toContain("0.0%");
    expect(html).toContain("—"); // the unknown new-since-poll count
  });

  it("renders an honest empty-list line for an empty table", () => {
    const empty = parseProcesses({ total: 0, new_since_poll: null, top_cpu: [], top_mem: [], load_avg: [0, 0, 0] });
    const html = renderToStaticMarkup(createElement(ProcPanel, { proc: empty }));
    expect(html).toContain("no process readings visible");
  });
});

/**
 * PROCESS OBSERVATORY — pure parser + presentation helpers for the
 * `system.processes` telemetry frame (daemon/src/procwatch.rs). No DOM/React
 * imports, so the parse + the derived numbers are verifiable headlessly under
 * vitest, exactly like core/vitals.ts. The ProcPanel component imports these
 * types + helpers.
 *
 * The daemon emits `system.processes` every [procwatch].poll_secs carrying a
 * STRICTLY READ-ONLY, SECRET-FREE reduction of the LIVE process table: total
 * process count, top-N by CPU and by memory (process NAME + pid + ppid/uid +
 * cpu/mem ONLY — never argv/env/open files; the daemon reads only fixed-size
 * libproc structs and never issues the argv/env sysctl), how many processes
 * are new since the last poll (null on the very first poll — no baseline
 * yet), and the load average as context. CPU % is a TWO-SAMPLE delta: on the
 * first poll every cpu_pct is null and top_cpu is honestly EMPTY (warm-up,
 * never a fabricated 0.0%); real deltas arrive from the second poll on.
 *
 * This module parses that shape DEFENSIVELY: a missing/malformed field
 * degrades to an honest "unknown"/null (never a fabricated value), a hostile
 * array is bounded, and NaN/out-of-range numbers are dropped or clamped.
 * Nothing here ever throws.
 */
import { num, str } from "./events";

/** How many rows each top list will render (matches the daemon's own cap). */
export const PROC_MAX_TOP = 32;
/** Upper bound on a rendered process-name length (matches the daemon's cap),
 *  so a hostile frame can't flood the DOM with a giant name. */
export const PROC_MAX_NAME = 64;

export interface ProcEntry {
  /** The process's short NAME (never its command line) — SECRET-FREE. */
  name: string;
  pid: number;
  /** Parent pid, or null when the daemon couldn't read it (honest absent). */
  ppid: number | null;
  /** Owning uid, or null where unavailable (honest absent). */
  uid: number | null;
  /** CPU percent — a two-sample delta that can honestly exceed 100 on
   *  multi-core (the daemon sums across cores). Null when no baseline exists
   *  yet (first poll / brand-new process) or unreadable — NEVER a fabricated
   *  0. */
  cpuPct: number | null;
  /** Resident memory bytes, or null when unreadable. */
  memBytes: number | null;
}

export interface ProcessesFrame {
  /** The KERNEL'S live pid count (includes pids the unprivileged daemon can't
   *  inspect — those can't appear in the lists), or null when unreadable. */
  total: number | null;
  /** Processes started since the previous poll, or null on the FIRST poll
   *  (no baseline yet — the daemon says null, never a fabricated 0, and we
   *  preserve that honestly). */
  newSincePoll: number | null;
  topCpu: ProcEntry[];
  topMem: ProcEntry[];
  /** Load average (1 / 5 / 15 min), or null when unreadable. */
  loadAvg: [number, number, number] | null;
}

function isObj(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

/** A non-negative integer, or null — pids/uids/counts are never fractional
 *  and never negative; anything else is an honest unknown. */
function nonNegInt(v: number | null): number | null {
  return v !== null && Number.isInteger(v) && v >= 0 ? v : null;
}

function parseEntries(v: unknown): ProcEntry[] {
  if (!Array.isArray(v)) return [];
  const out: ProcEntry[] = [];
  for (const item of v) {
    if (!isObj(item)) continue;
    // A row without a valid pid is unkeyable and dropped (never fabricated).
    const pid = nonNegInt(num(item, "pid"));
    if (pid === null) continue;
    const cpu = num(item, "cpu_pct");
    const mem = num(item, "mem_bytes");
    out.push({
      name: (str(item, "name") ?? "").slice(0, PROC_MAX_NAME),
      pid,
      ppid: nonNegInt(num(item, "ppid")),
      uid: nonNegInt(num(item, "uid")),
      // Negative CPU clamps to 0; an unreadable one stays null (never faked).
      cpuPct: cpu === null ? null : Math.max(0, cpu),
      memBytes: mem === null || mem < 0 ? null : mem,
    });
    if (out.length >= PROC_MAX_TOP) break;
  }
  return out;
}

function parseLoad(v: unknown): [number, number, number] | null {
  const raw = Array.isArray(v) ? (v as unknown[]) : null;
  return raw !== null &&
    raw.length >= 3 &&
    raw.slice(0, 3).every((x) => typeof x === "number" && Number.isFinite(x))
    ? ([raw[0], raw[1], raw[2]] as [number, number, number])
    : null;
}

/** Parse a `system.processes` payload. NEVER throws / never returns null — a
 *  malformed frame degrades every field to an honest unknown/empty, so the
 *  panel shows "can't read" rather than a fabricated reading. */
export function parseProcesses(data: Record<string, unknown>): ProcessesFrame {
  return {
    total: nonNegInt(num(data, "total")),
    newSincePoll: nonNegInt(num(data, "new_since_poll")),
    topCpu: parseEntries(data["top_cpu"]),
    topMem: parseEntries(data["top_mem"]),
    loadAvg: parseLoad(data["load_avg"]),
  };
}

/* Presentation helpers (pure, tested) ------------------------------------- */

/** The largest readable CPU percent in a list, or null when none is readable
 *  — the panel scales its bars against this. */
export function maxCpu(entries: ProcEntry[]): number | null {
  let max: number | null = null;
  for (const e of entries) {
    if (e.cpuPct !== null && (max === null || e.cpuPct > max)) max = e.cpuPct;
  }
  return max;
}

/** The largest readable memory reading in a list, or null when none. */
export function maxMem(entries: ProcEntry[]): number | null {
  let max: number | null = null;
  for (const e of entries) {
    if (e.memBytes !== null && (max === null || e.memBytes > max)) max = e.memBytes;
  }
  return max;
}

/** A value's share of the list peak as 0..100 (for a row bar). Null value or
 *  an empty/unreadable peak -> 0 (an honest empty bar, never a fabricated
 *  fill). */
export function sharePct(value: number | null, peak: number | null): number {
  if (value === null || peak === null || peak <= 0) return 0;
  return Math.min(100, Math.max(0, (value / peak) * 100));
}

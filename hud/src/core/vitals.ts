/**
 * HARDWARE VITALS — pure parser + presentation helpers for the
 * `hardware.vitals` telemetry frame (daemon/src/vitals.rs). No DOM/React
 * imports, so the parse + the derived percentages are verifiable headlessly
 * under vitest, exactly like core/changeq.ts. The VitalsPanel component imports
 * these types + helpers.
 *
 * The daemon emits `hardware.vitals` every [vitals].poll_secs carrying a
 * STRICTLY READ-ONLY, SECRET-FREE snapshot of the machine: battery %, AC/charge
 * state, the LIVE macOS thermal-pressure level (ProcessInfo.thermalState),
 * memory pressure (a used-fraction level), per-core CPU utilization + load
 * average, and every mounted volume's LABEL + free/total bytes (no file data).
 *
 * This module parses that shape DEFENSIVELY: a missing/malformed field degrades
 * to an honest "unknown"/null (never a fabricated value), a hostile array is
 * bounded, and NaN/out-of-range numbers are dropped or clamped. Nothing here
 * ever throws.
 */
import { num, str, bool } from "./events";

/** How many volumes the panel will render (matches the daemon's own cap). */
export const VITALS_MAX_VOLUMES = 24;
/** Upper bound on per-core CPU entries rendered, so a hostile frame can't flood
 *  the DOM (a real machine has far fewer logical cores). */
export const VITALS_MAX_CORES = 256;

/** The macOS thermal-pressure ladder (ProcessInfo.thermalState) + an honest
 *  "unknown" for an unreadable/malformed frame. */
export type ThermalLevel = "nominal" | "fair" | "serious" | "critical" | "unknown";
/** The coarse battery charge state, or "unknown" when unreadable. */
export type ChargeState = "discharging" | "charging" | "charged" | "unknown";
/** The used-fraction memory-pressure level, or "unknown" when unreadable. */
export type MemPressureLevel = "normal" | "warn" | "critical" | "unknown";

export interface VitalsBattery {
  /** Charge percent 0..100, or null on a desktop Mac / read failure (NEVER a
   *  fabricated low). */
  percent: number | null;
  /** Whether the machine is on AC power. */
  onAc: boolean;
  chargeState: ChargeState;
}

export interface VitalsMemory {
  usedBytes: number | null;
  totalBytes: number | null;
  pressure: MemPressureLevel;
}

export interface VitalsCpu {
  /** Per logical core utilization percent (0..100), bounded. */
  perCore: number[];
  /** Load average (1 / 5 / 15 min), or null when unreadable. */
  loadAvg: [number, number, number] | null;
}

export interface VitalsVolume {
  /** The volume label (already shown in Finder) — SECRET-FREE. */
  label: string;
  /** The mount path. */
  mount: string;
  freeBytes: number;
  totalBytes: number;
}

export interface HardwareVitals {
  battery: VitalsBattery;
  thermal: ThermalLevel;
  memory: VitalsMemory;
  cpu: VitalsCpu;
  volumes: VitalsVolume[];
  uptimeSecs: number | null;
}

function isObj(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

/** Clamp a number into 0..100. */
function clampPct(v: number): number {
  return Math.min(100, Math.max(0, v));
}

function coerceThermal(v: string | null): ThermalLevel {
  return v === "nominal" || v === "fair" || v === "serious" || v === "critical"
    ? v
    : "unknown";
}

function coerceCharge(v: string | null): ChargeState {
  return v === "discharging" || v === "charging" || v === "charged" ? v : "unknown";
}

function coercePressure(v: string | null): MemPressureLevel {
  return v === "normal" || v === "warn" || v === "critical" ? v : "unknown";
}

function parseBattery(v: unknown): VitalsBattery {
  const o = isObj(v) ? v : {};
  const pct = num(o, "percent");
  return {
    // Round + clamp; a null (no battery) is preserved honestly, never faked.
    percent: pct === null ? null : Math.round(clampPct(pct)),
    onAc: bool(o, "on_ac") ?? false,
    chargeState: coerceCharge(str(o, "charge_state")),
  };
}

function parseMemory(v: unknown): VitalsMemory {
  const o = isObj(v) ? v : {};
  return {
    usedBytes: num(o, "used_bytes"),
    totalBytes: num(o, "total_bytes"),
    pressure: coercePressure(str(o, "pressure")),
  };
}

function parseCpu(v: unknown): VitalsCpu {
  const o = isObj(v) ? v : {};
  const rawCores = Array.isArray(o["per_core"]) ? (o["per_core"] as unknown[]) : [];
  const perCore = rawCores
    .filter((x): x is number => typeof x === "number" && Number.isFinite(x))
    .slice(0, VITALS_MAX_CORES)
    .map(clampPct);
  const rawLoad = Array.isArray(o["load_avg"]) ? (o["load_avg"] as unknown[]) : null;
  const loadAvg =
    rawLoad !== null &&
    rawLoad.length >= 3 &&
    rawLoad.slice(0, 3).every((x) => typeof x === "number" && Number.isFinite(x))
      ? ([rawLoad[0], rawLoad[1], rawLoad[2]] as [number, number, number])
      : null;
  return { perCore, loadAvg };
}

function parseVolumes(v: unknown): VitalsVolume[] {
  if (!Array.isArray(v)) return [];
  const out: VitalsVolume[] = [];
  for (const item of v) {
    if (!isObj(item)) continue;
    const total = num(item, "total_bytes");
    const free = num(item, "free_bytes");
    // A volume with no usable capacity, or negative bytes, is dropped (never a
    // fabricated row). Free is clamped to [0, total].
    if (total === null || total <= 0 || free === null || free < 0) continue;
    out.push({
      label: str(item, "label") ?? "",
      mount: str(item, "mount") ?? "",
      freeBytes: Math.min(free, total),
      totalBytes: total,
    });
    if (out.length >= VITALS_MAX_VOLUMES) break;
  }
  return out;
}

/** Parse a `hardware.vitals` payload. NEVER throws / never returns null — a
 *  malformed frame degrades every field to an honest unknown/empty, so the
 *  panel shows "can't read" rather than a fabricated reading. */
export function parseVitals(data: Record<string, unknown>): HardwareVitals {
  return {
    battery: parseBattery(data["battery"]),
    thermal: coerceThermal(str(data, "thermal")),
    memory: parseMemory(data["memory"]),
    cpu: parseCpu(data["cpu"]),
    volumes: parseVolumes(data["volumes"]),
    uptimeSecs: num(data, "uptime_secs"),
  };
}

/* Presentation helpers (pure, tested) ------------------------------------- */

/** Memory used as a percent of total, or null when unreadable. */
export function memUsedPercent(m: VitalsMemory): number | null {
  if (m.usedBytes === null || m.totalBytes === null || m.totalBytes <= 0) return null;
  return clampPct((m.usedBytes / m.totalBytes) * 100);
}

/** A volume's USED space as a percent of its capacity (0..100). */
export function volumeUsedPercent(v: VitalsVolume): number {
  if (v.totalBytes <= 0) return 0;
  const used = Math.max(0, v.totalBytes - v.freeBytes);
  return clampPct((used / v.totalBytes) * 100);
}

/** The mean per-core CPU utilization, or null when no cores were reported. */
export function cpuAverage(perCore: number[]): number | null {
  if (perCore.length === 0) return null;
  return perCore.reduce((a, b) => a + b, 0) / perCore.length;
}

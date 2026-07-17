import type {
  HardwareVitals,
  ThermalLevel,
  MemPressureLevel,
  ChargeState,
} from "../core/vitals";
import { memUsedPercent, volumeUsedPercent, cpuAverage } from "../core/vitals";
import Frame from "./Frame";

/** Bytes -> a compact human string. `—` when unknown. */
function fmtBytes(n: number | null): string {
  if (n === null) return "—";
  if (n >= 1e12) return `${(n / 1e12).toFixed(2)} TB`;
  if (n >= 1e9) return `${(n / 1e9).toFixed(1)} GB`;
  if (n >= 1e6) return `${(n / 1e6).toFixed(0)} MB`;
  return `${n} B`;
}

function fmtUptime(secs: number | null): string {
  if (secs === null) return "—";
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  return d > 0 ? `${d}d ${h}h ${m}m` : h > 0 ? `${h}h ${m}m` : `${m}m`;
}

const GAUGE_SEGMENTS = 22;

/** A segmented block gauge, matching the DIAGNOSTICS panel's visual language. */
function Gauge({
  label,
  value,
  pct,
  hot,
}: {
  label: string;
  value: string;
  pct: number | null;
  hot?: boolean;
}) {
  const lit =
    pct === null
      ? 0
      : Math.round((Math.min(100, Math.max(0, pct)) / 100) * GAUGE_SEGMENTS);
  return (
    <div className={`gauge ${hot ? "hot" : ""}`}>
      <div className="row">
        <span>{label}</span>
        <span className="val">{value}</span>
      </div>
      <div className="seg-bar">
        {Array.from({ length: GAUGE_SEGMENTS }, (_, i) => (
          <i key={i} className={i < lit ? "on" : ""} />
        ))}
      </div>
    </div>
  );
}

/** Thermal is a throttle trigger at serious/critical; memory at critical. */
function thermalHot(t: ThermalLevel): boolean {
  return t === "serious" || t === "critical";
}
function pressureHot(p: MemPressureLevel): boolean {
  return p === "critical";
}

const THERMAL_LABEL: Record<ThermalLevel, string> = {
  nominal: "NOMINAL",
  fair: "FAIR",
  serious: "SERIOUS",
  critical: "CRITICAL",
  unknown: "—",
};

const CHARGE_GLYPH: Record<ChargeState, string> = {
  charging: "▲ charging",
  discharging: "▼ discharging",
  charged: "■ charged",
  unknown: "battery",
};

export default function VitalsPanel({ vitals }: { vitals: HardwareVitals | null }) {
  if (vitals === null) {
    return (
      <Frame className="diagnostics vitals" title="SYS // VITALS" tag="hardware.vitals">
        <div className="tickers sub">
          <div className="tick-entry v">no hardware vitals read yet</div>
        </div>
      </Frame>
    );
  }

  const { battery, thermal, memory, cpu, volumes } = vitals;
  const memPct = memUsedPercent(memory);
  const cpuAvg = cpuAverage(cpu.perCore);
  const batteryValue =
    battery.percent === null
      ? battery.onAc
        ? "AC"
        : "—"
      : `${battery.percent}% · ${battery.onAc ? "AC" : "batt"}`;
  const loadText =
    cpu.loadAvg === null
      ? "—"
      : cpu.loadAvg.map((x) => x.toFixed(2)).join(" · ");

  return (
    <Frame className="diagnostics vitals" title="SYS // VITALS" tag="hardware.vitals">
      <div className="gauges sub">
        <Gauge
          label={`BATT · ${CHARGE_GLYPH[battery.chargeState]}`}
          value={batteryValue}
          pct={battery.percent}
          hot={
            battery.percent !== null &&
            !battery.onAc &&
            battery.chargeState === "discharging" &&
            battery.percent <= 20
          }
        />
        <Gauge
          label="THERMAL"
          value={THERMAL_LABEL[thermal]}
          pct={null}
          hot={thermalHot(thermal)}
        />
        <Gauge
          label={`MEM · ${memory.pressure}`}
          value={
            memPct === null
              ? "—"
              : `${fmtBytes(memory.usedBytes)} / ${fmtBytes(memory.totalBytes)}`
          }
          pct={memPct}
          hot={pressureHot(memory.pressure) || (memPct !== null && memPct > 90)}
        />
        <Gauge
          label={`CPU · ${cpu.perCore.length} cores`}
          value={cpuAvg === null ? "—" : `${cpuAvg.toFixed(1)}% · load ${loadText}`}
          pct={cpuAvg}
          hot={cpuAvg !== null && cpuAvg > 85}
        />
      </div>

      {/* Per-core utilization strip: one vertical bar per logical core, height
          proportional to its % (inline-styled so no new CSS is required). */}
      {cpu.perCore.length > 0 && (
        <div
          className="core-strip"
          aria-label="per-core CPU utilization"
          style={{ display: "flex", alignItems: "flex-end", gap: "2px", height: "22px" }}
        >
          {cpu.perCore.map((c, i) => (
            <i
              key={i}
              title={`core ${i}: ${c.toFixed(0)}%`}
              className={c > 85 ? "on hot" : "on"}
              style={{
                display: "inline-block",
                flex: "1 1 0",
                minWidth: "2px",
                height: `${Math.max(4, Math.min(100, c))}%`,
                background: "currentColor",
                opacity: c > 85 ? 1 : 0.55,
              }}
            />
          ))}
        </div>
      )}

      <div className="sub-title">
        VOLUMES <span className="tag">{volumes.length}</span>
        <span className="tag">up {fmtUptime(vitals.uptimeSecs)}</span>
      </div>
      <div className="tickers sub">
        {volumes.length === 0 && (
          <div className="tick-entry v">no mounted volumes visible</div>
        )}
        {volumes.map((v) => {
          const usedPct = volumeUsedPercent(v);
          return (
            <div key={v.mount || v.label} className="tick-entry">
              <span className="k">◈ {v.label || v.mount || "volume"}</span>{" "}
              <span className="v">
                {fmtBytes(v.freeBytes)} free / {fmtBytes(v.totalBytes)} ·{" "}
                {usedPct.toFixed(0)}% used
              </span>
            </div>
          );
        })}
      </div>
    </Frame>
  );
}

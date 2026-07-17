import type { ProcEntry, ProcessesFrame } from "../core/procwatch";
import { maxCpu, maxMem, sharePct } from "../core/procwatch";
import Frame from "./Frame";

/** Bytes -> a compact human string. `—` when unknown. */
function fmtBytes(n: number | null): string {
  if (n === null) return "—";
  if (n >= 1e12) return `${(n / 1e12).toFixed(2)} TB`;
  if (n >= 1e9) return `${(n / 1e9).toFixed(1)} GB`;
  if (n >= 1e6) return `${(n / 1e6).toFixed(0)} MB`;
  return `${n} B`;
}

/** CPU percent -> display. `—` when unreadable (never a fabricated 0). */
function fmtCpu(c: number | null): string {
  return c === null ? "—" : `${c.toFixed(1)}%`;
}

function fmtCount(n: number | null): string {
  return n === null ? "—" : String(n);
}

/** One top-list row: name + pid on the left, cpu/mem + a peak-relative bar on
 *  the right (inline-styled so no new CSS is required, like VitalsPanel's
 *  core strip). */
function ProcRow({ entry, pct, value }: { entry: ProcEntry; pct: number; value: string }) {
  return (
    <div className="tick-entry">
      <span className="k">
        ▸ {entry.name || `pid ${entry.pid}`}{" "}
        <span className="tag">{entry.pid}</span>
      </span>{" "}
      <span className="v">
        {value}{" "}
        <i
          aria-hidden
          style={{
            display: "inline-block",
            width: "48px",
            height: "6px",
            verticalAlign: "middle",
            background: `linear-gradient(to right, currentColor ${pct}%, transparent ${pct}%)`,
            opacity: 0.55,
          }}
        />
      </span>
    </div>
  );
}

export default function ProcPanel({ proc }: { proc: ProcessesFrame | null }) {
  if (proc === null) {
    return (
      <Frame className="diagnostics procwatch" title="SYS // PROCESSES" tag="system.processes">
        <div className="tickers sub">
          <div className="tick-entry v">no process snapshot read yet</div>
        </div>
      </Frame>
    );
  }

  const { topCpu, topMem } = proc;
  const cpuPeak = maxCpu(topCpu);
  const memPeak = maxMem(topMem);
  const loadText =
    proc.loadAvg === null ? "—" : proc.loadAvg.map((x) => x.toFixed(2)).join(" · ");

  return (
    <Frame className="diagnostics procwatch" title="SYS // PROCESSES" tag="system.processes">
      {/* Counts strip: total table size, new since the last poll ("—" on the
          first poll — no baseline yet, honestly unknown), load context. */}
      <div className="tickers sub">
        <div className="tick-entry">
          <span className="k">total</span>{" "}
          <span className="v">{fmtCount(proc.total)}</span>
        </div>
        <div className="tick-entry">
          <span className="k">new since poll</span>{" "}
          <span className="v">{fmtCount(proc.newSincePoll)}</span>
        </div>
        <div className="tick-entry">
          <span className="k">load</span> <span className="v">{loadText}</span>
        </div>
      </div>

      <div className="sub-title">
        TOP CPU <span className="tag">{topCpu.length}</span>
      </div>
      <div className="tickers sub">
        {/* CPU % is a two-sample delta: the daemon's FIRST poll has no
            baseline, so top_cpu arrives honestly EMPTY (never a fabricated
            0.0% list). With processes visible, an empty list means warm-up. */}
        {topCpu.length === 0 && (
          <div className="tick-entry v">
            {proc.total !== null && proc.total > 0
              ? "cpu warming up — deltas need two polls"
              : "no process readings visible"}
          </div>
        )}
        {topCpu.map((e) => (
          <ProcRow
            key={`c${e.pid}`}
            entry={e}
            pct={sharePct(e.cpuPct, cpuPeak)}
            value={fmtCpu(e.cpuPct)}
          />
        ))}
      </div>

      <div className="sub-title">
        TOP MEM <span className="tag">{topMem.length}</span>
      </div>
      <div className="tickers sub">
        {topMem.length === 0 && (
          <div className="tick-entry v">no process readings visible</div>
        )}
        {topMem.map((e) => (
          <ProcRow
            key={`m${e.pid}`}
            entry={e}
            pct={sharePct(e.memBytes, memPeak)}
            value={fmtBytes(e.memBytes)}
          />
        ))}
      </div>
    </Frame>
  );
}

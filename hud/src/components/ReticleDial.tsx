import type { CoreState } from "../core/state";

/**
 * Top-left circular reticle (restyle contract item 4): concentric tick
 * rings, a slow rotating sweep, and a needle — repurposed as the live CPU /
 * core-state dial. Data bindings are truthful: needle + readout come from
 * system.load cpu_percent, the state label mirrors the reducer's coreState.
 *
 * The needle moves via a CSS transform transition (600ms) — system.load
 * arrives every 2s, so updates glide instead of stepping.
 */

const NEEDLE_MIN_DEG = -120;
const NEEDLE_SPAN_DEG = 240;

/** 48 outer tick marks. */
function OuterTicks() {
  const ticks = [];
  for (let i = 0; i < 48; i++) {
    const a = (i / 48) * Math.PI * 2;
    const major = i % 4 === 0;
    const r0 = major ? 66 : 69;
    ticks.push(
      <line
        key={i}
        x1={80 + Math.cos(a) * r0}
        y1={80 + Math.sin(a) * r0}
        x2={80 + Math.cos(a) * 73}
        y2={80 + Math.sin(a) * 73}
        className={major ? "rt-tick major" : "rt-tick"}
      />,
    );
  }
  return <>{ticks}</>;
}

export default function ReticleDial({
  cpuPercent,
  coreState,
}: {
  cpuPercent: number | null;
  coreState: CoreState;
}) {
  const pct = cpuPercent === null ? 0 : Math.min(100, Math.max(0, cpuPercent));
  const needleDeg = NEEDLE_MIN_DEG + (pct / 100) * NEEDLE_SPAN_DEG;
  const hot = cpuPercent !== null && cpuPercent > 85;

  return (
    <div
      className={`reticle ${coreState === "offline" ? "offline" : ""}`}
      // a11y: a labeled div carries no role — role="img" makes the label land,
      // and the label carries the REAL reading (or the honest absence of one).
      role="img"
      aria-label={cpuPercent === null ? "CPU dial: no reading" : `CPU dial: ${Math.round(pct)} percent`}
    >
      <svg viewBox="0 0 160 160" className="reticle-svg" aria-hidden="true">
        {/* concentric rings */}
        <circle cx="80" cy="80" r="74" className="rt-ring faint" />
        <circle cx="80" cy="80" r="62" className="rt-ring" />
        <circle cx="80" cy="80" r="44" className="rt-ring dashed" />
        <circle cx="80" cy="80" r="30" className="rt-ring faint" />
        <OuterTicks />
        {/* cardinal cross hairs */}
        <line x1="80" y1="4" x2="80" y2="14" className="rt-cross" />
        <line x1="80" y1="146" x2="80" y2="156" className="rt-cross" />
        <line x1="4" y1="80" x2="14" y2="80" className="rt-cross" />
        <line x1="146" y1="80" x2="156" y2="80" className="rt-cross" />
        {/* rotating sweep (pure decoration, CSS animated) */}
        <g className="rt-sweep">
          <line x1="80" y1="58" x2="80" y2="20" />
          <path d="M 80 20 A 60 60 0 0 1 110 28" fill="none" />
        </g>
        {/* needle — CPU percent, CSS transition glides between samples */}
        <g
          className={`rt-needle ${hot ? "hot" : ""}`}
          style={{ transform: `rotate(${needleDeg}deg)` }}
        >
          <line x1="80" y1="56" x2="80" y2="26" />
          <circle cx="80" cy="24" r="2.2" />
        </g>
      </svg>
      <div className="reticle-readout">
        <span className="pct">{cpuPercent === null ? "—" : `${pct.toFixed(0)}%`}</span>
        <span className="lbl">CPU</span>
        <span className="state">{coreState.toUpperCase()}</span>
      </div>
    </div>
  );
}

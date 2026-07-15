import { memo, useEffect, useMemo, useState } from "react";
import type { CoreState } from "../core/state";

/**
 * "Neural conduits": faint energy channels curving from the core out to the
 * left/right panel columns, each carrying a travelling light pulse — the core
 * visibly feeding the readouts. Ambient and slow at rest; brighter + faster
 * whenever DARWIN is doing something (listening → speaking), violet on cloud
 * compute. Pure atmosphere: pointer-events:none, z:2 (above the vignette, below
 * the panels, which cover the conduits' far ends so they read as "entering" the
 * panels).
 *
 * Crisp at any size: the bezier paths are computed in real pixels from the live
 * viewport (resize-tracked) rather than a stretched viewBox, so dashes/strokes
 * never distort. The travelling pulse is a pure CSS stroke-dashoffset animation
 * (no per-frame React work) and is frozen under prefers-reduced-motion.
 */

function viewport() {
  return {
    w: typeof window === "undefined" ? 1440 : window.innerWidth,
    h: typeof window === "undefined" ? 900 : window.innerHeight,
  };
}

interface Conduit {
  d: string;
  /** -ve animation-delay so the pulses are staggered, not marching in lockstep. */
  delay: number;
}

function buildConduits(w: number, h: number): Conduit[] {
  const cx = w / 2;
  const cy = h * 0.44; // matches the lifted core center
  // Inner edges of the 24vw side columns; pull the anchors a touch past the edge
  // so each conduit clearly arrives AT the panel column.
  const leftX = w * 0.245;
  const rightX = w * 0.755;
  const ys = [0.3, 0.5, 0.7];
  const out: Conduit[] = [];
  let i = 0;
  for (const side of [leftX, rightX]) {
    for (const yf of ys) {
      const ax = side;
      const ay = h * yf;
      const dx = ax - cx;
      const dy = ay - cy;
      const len = Math.hypot(dx, dy) || 1;
      // bow the channel perpendicular to the core→anchor line, alternating sign
      const sign = i % 2 === 0 ? 1 : -1;
      const bow = len * 0.17 * sign;
      const mx = (cx + ax) / 2 + (-dy / len) * bow;
      const my = (cy + ay) / 2 + (dx / len) * bow;
      out.push({
        d: `M ${cx.toFixed(1)} ${cy.toFixed(1)} Q ${mx.toFixed(1)} ${my.toFixed(1)} ${ax.toFixed(1)} ${ay.toFixed(1)}`,
        delay: -(i * 0.42),
      });
      i++;
    }
  }
  return out;
}

const ACTIVE: ReadonlySet<CoreState> = new Set<CoreState>([
  "listening",
  "processing",
  "thinking-local",
  "thinking-cloud",
  "speaking",
]);

function ConduitField({ coreState }: { coreState: CoreState }) {
  const [dim, setDim] = useState(viewport);
  useEffect(() => {
    let raf = 0;
    const onResize = () => {
      cancelAnimationFrame(raf);
      // Functional updater that returns the SAME reference when w/h are
      // unchanged, so React bails out (a fresh {w,h} would always re-render,
      // even on a no-op / DPR-only resize event).
      raf = requestAnimationFrame(() =>
        setDim((prev) => {
          const v = viewport();
          return prev.w === v.w && prev.h === v.h ? prev : v;
        }),
      );
    };
    window.addEventListener("resize", onResize);
    return () => {
      window.removeEventListener("resize", onResize);
      cancelAnimationFrame(raf);
    };
  }, []);

  const conduits = useMemo(() => buildConduits(dim.w, dim.h), [dim.w, dim.h]);
  const cls =
    "conduits" +
    (ACTIVE.has(coreState) ? " active" : "") +
    (coreState === "thinking-cloud" ? " cloud" : "");

  return (
    <svg
      className={cls}
      viewBox={`0 0 ${dim.w} ${dim.h}`}
      preserveAspectRatio="none"
      aria-hidden="true"
    >
      {conduits.map((c, i) => (
        <g key={i}>
          <path className="conduit-base" d={c.d} pathLength={100} />
          <path
            className="conduit-flow"
            d={c.d}
            pathLength={100}
            style={{ animationDelay: `${c.delay}s` }}
          />
        </g>
      ))}
    </svg>
  );
}

export default memo(ConduitField);

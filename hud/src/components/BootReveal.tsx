import { useEffect, useRef } from "react";

/**
 * Cinematic multi-stage power-on shown the first time DARWIN connects (the
 * "wake"): a scan sweep + corner brackets draw the frame, the reticle rings
 * ignite and the D.A.R.W.I.N. mark resolves, a subsystem checklist ticks ONLINE
 * one by one, then "ALL SYSTEMS ONLINE" lands and the whole overlay fades to
 * reveal the live HUD (whose panels then materialize in).
 *
 * Honest + accessible:
 *  - Self-dismisses on a timer AND on click/Esc (escape-routes); the timer is
 *    armed once (stable onDone from App) so it can't be reset by re-renders.
 *  - Under prefers-reduced-motion the choreography collapses to a static stamp
 *    (CSS zeroes the animations; the timer just shortens).
 *  - Purely presentational + aria-hidden: it gates nothing and renders above
 *    everything, so a missed/!mounted reveal never affects the HUD beneath it.
 *  - The subsystem names are real DARWIN subsystems (no fabricated metrics).
 */

const SUBSYSTEMS = [
  "MLX RUNTIME",
  "DARWIND DAEMON",
  "27-AGENT CONSTELLATION",
  "TELEMETRY LINK",
  "MEMORY STORE",
  "VOICE PIPELINE",
];

export default function BootReveal({
  onReveal,
  onDone,
}: {
  /** Fires ~0.6s before unmount so the panels deal in UNDERNEATH the fading
   *  overlay (no empty-HUD gap during the dissolve). */
  onReveal: () => void;
  onDone: () => void;
}) {
  // Each parent callback must fire at most once: the dismiss timer, the reveal
  // timer, the click handler and the Esc key can race, and onReveal/onDone are
  // one-shot transitions in App. These guards make completion idempotent.
  const revealed = useRef(false);
  const done = useRef(false);
  const fireReveal = () => {
    if (revealed.current) return;
    revealed.current = true;
    onReveal();
  };
  const fireDone = () => {
    if (done.current) return;
    done.current = true;
    onDone();
  };

  useEffect(() => {
    const reduce =
      typeof window !== "undefined" &&
      typeof window.matchMedia === "function" &&
      window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    const total = reduce ? 1100 : 3500;
    // Start the panel reveal before the overlay finishes fading so the deal-in
    // overlaps the dissolve; under reduced-motion there's no fade, so reveal at
    // unmount.
    const revealTimer = setTimeout(fireReveal, reduce ? total : total - 600);
    const timer = setTimeout(fireDone, total);
    const skip = () => {
      fireReveal();
      fireDone();
    };
    const onKey = (ev: KeyboardEvent) => {
      if (ev.key === "Escape") skip();
    };
    window.addEventListener("keydown", onKey);
    return () => {
      clearTimeout(revealTimer);
      clearTimeout(timer);
      window.removeEventListener("keydown", onKey);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [onReveal, onDone]);

  return (
    <div
      className="boot-reveal"
      aria-hidden="true"
      onClick={() => {
        fireReveal();
        fireDone();
      }}
    >
      <div className="boot-grid" />
      <div className="boot-scan" />
      <i className="boot-corner tl" />
      <i className="boot-corner tr" />
      <i className="boot-corner bl" />
      <i className="boot-corner br" />
      <div className="boot-center">
        <div className="boot-ring" />
        <div className="boot-ring boot-ring-2" />
        <div className="boot-mark">D.A.R.W.I.N.</div>
        <div className="boot-sub">DIGITAL ASSISTANT RUNNING WHOLLY IN-HOUSE, NATIVELY</div>
        <ul className="boot-checklist">
          {SUBSYSTEMS.map((name, i) => (
            <li key={name} style={{ animationDelay: `${(1.2 + i * 0.2).toFixed(2)}s` }}>
              <span className="bc-name">{name}</span>
              <span className="bc-dots" />
              <span className="bc-ok">ONLINE</span>
            </li>
          ))}
        </ul>
        <div className="boot-status">◇ ALL SYSTEMS ONLINE</div>
      </div>
    </div>
  );
}

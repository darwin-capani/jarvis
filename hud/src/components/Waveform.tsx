import { useEffect, useRef } from "react";
import { audioStore, RMS_HISTORY } from "../core/audioStore";
import { damp, waveSilentTarget } from "../core/visuals";
import Frame from "./Frame";

const BARS = 64;

export interface WaveformProps {
  connected: boolean;
}

/**
 * Bottom full-width equalizer: 64 bars folded from the 128-sample rms
 * history, drawn on canvas at rAF cadence. The history/mute state is read
 * from core/audioStore.ts inside the loop — 15Hz audio.level frames never
 * re-render this component (the only prop is `connected`).
 *
 * Anti-flash: the idle shimmer and the live bars are CROSSFADED through a
 * damped silentMix with a hysteresis band (visuals.waveSilentTarget), and
 * bar color is a continuous ramp over level — no frame-to-frame branch can
 * swap the whole field or strobe a single bar.
 */
export default function Waveform({ connected }: WaveformProps) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const connectedRef = useRef(connected);
  connectedRef.current = connected;

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    let raf = 0;
    let prevT = 0;
    let silentMix = 1; // 1 = idle shimmer, 0 = live data
    let silentTarget = 1;

    const draw = (tMs: number) => {
      raf = requestAnimationFrame(draw);
      const dt = prevT === 0 ? 1 / 60 : Math.min((tMs - prevT) / 1000, 0.1);
      prevT = tMs;
      const online = connectedRef.current;
      const muted = audioStore.micMuted;

      const dpr = Math.min(window.devicePixelRatio || 1, 2);
      const w = canvas.clientWidth;
      const h = canvas.clientHeight;
      if (w === 0 || h === 0) return;
      if (canvas.width !== Math.round(w * dpr) || canvas.height !== Math.round(h * dpr)) {
        canvas.width = Math.round(w * dpr);
        canvas.height = Math.round(h * dpr);
      }
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      ctx.clearRect(0, 0, w, h);

      const t = tMs / 1000;
      const slot = w / BARS;
      const barW = Math.max(2, slot * 0.55);
      const mid = h / 2;

      // Hysteretic shimmer/data crossfade (pure target fn + damped mix).
      silentTarget = waveSilentTarget(silentTarget, audioStore.mean());
      silentMix = damp(silentMix, silentTarget, 6, dt);

      for (let i = 0; i < BARS; i++) {
        // fold 128 samples -> 64 bars (pairwise average)
        const a = audioStore.at(i * 2);
        const b = audioStore.at(Math.min(i * 2 + 1, RMS_HISTORY - 1));
        const dataLevel = Math.min(1, ((a + b) / 2) * 14);

        // idle shimmer: slow travelling interference, very dim
        const ph = i / BARS;
        let shimmer = 0.04 + 0.035 * Math.sin(t * 1.4 + ph * 9) * Math.sin(t * 0.53 + ph * 4.2);
        shimmer = Math.max(0.012, shimmer);

        const level = dataLevel * (1 - silentMix) + shimmer * silentMix;
        const bh = Math.max(1.5, level * (h * 0.82));
        const x = i * slot + (slot - barW) / 2;

        if (!online) {
          ctx.fillStyle = "rgba(73, 104, 122, 0.35)";
        } else if (muted) {
          ctx.fillStyle = `rgba(157, 125, 255, ${0.25 + level * 0.6})`; // DARWIN talking
        } else {
          // continuous color ramp cyan -> bright ice over level (no branch)
          const g = Math.min(1, level * 1.6);
          const r = Math.round(26 + (125 - 26) * g);
          const gr = Math.round(198 + (243 - 198) * g);
          const bl = Math.round(227 + (255 - 227) * g);
          ctx.fillStyle = `rgba(${r}, ${gr}, ${bl}, ${0.3 + level * 0.62})`;
        }
        ctx.fillRect(x, mid - bh / 2, barW, bh);
      }

      // center hairline
      ctx.fillStyle = "rgba(26, 214, 255, 0.18)";
      ctx.fillRect(0, mid - 0.5, w, 1);
    };
    raf = requestAnimationFrame(draw);
    return () => cancelAnimationFrame(raf);
  }, []);

  return (
    <Frame className="waveform" title="AUDIO // INPUT FIELD">
      <canvas ref={canvasRef} aria-label="Audio waveform" />
    </Frame>
  );
}

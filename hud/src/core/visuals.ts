/**
 * Core-state -> visual signature mapping. Pure (no three.js imports) so the
 * state signatures are unit-testable; the R3F layer lerps toward these
 * targets every frame (no hard cuts).
 */
import type { CoreState } from "./state";

export interface CoreVisualTarget {
  /** Hue in degrees. Local/cyan = 190, cloud/violet = 268. */
  hue: number;
  /** Emissive/bloom intensity multiplier. */
  intensity: number;
  /** Core rotation speed, rad/s. */
  spin: number;
  /** Breathing/pulse rate in Hz. */
  pulseHz: number;
  /** Pulse depth 0..1 (how much the pulse modulates scale/brightness). */
  pulseDepth: number;
  /** 0..1 — particles stream upward (cloud routing made visible). */
  upward: number;
  /** 0..1 — particles converge toward the core (thinking). */
  converge: number;
}

export const HUE_CYAN = 190;
export const HUE_VIOLET = 268;

const clamp01 = (v: number) => Math.min(1, Math.max(0, v));

/**
 * Visual signature for a core state, with an optional active-agent hue
 * override (CONTRACT part C.3). When an agent is handling the request the core
 * adopts that agent's identity hue across every state, so the centerpiece
 * cycles color per active agent during a roll call; the renderer still LERPS
 * toward this target every frame (dampHue), so the change is a sweep, never a
 * hard cut. Passing `null`/`undefined` (idle, no active agent) restores the
 * default cyan / thinking-cloud-violet behavior unchanged.
 *
 * Only the hue is overridden — intensity/spin/pulse/particle motion keep
 * tracking the pipeline state, so the upward cloud-routing stream and the rms
 * pulse still read correctly under any agent color.
 */
export function coreVisualTarget(
  state: CoreState,
  rms: number,
  agentHue?: number | null,
): CoreVisualTarget {
  const base = baseVisualTarget(state, rms);
  if (agentHue == null || !Number.isFinite(agentHue)) return base;
  const hue = ((agentHue % 360) + 360) % 360;
  return base.hue === hue ? base : { ...base, hue };
}

function baseVisualTarget(state: CoreState, rms: number): CoreVisualTarget {
  switch (state) {
    case "offline":
      return { hue: HUE_CYAN, intensity: 0.12, spin: 0.02, pulseHz: 0.05, pulseDepth: 0.04, upward: 0, converge: 0 };
    case "idle":
      // Slow rotation, gentle breathing — present and visible at rest.
      return { hue: HUE_CYAN, intensity: 0.55, spin: 0.06, pulseHz: 0.1, pulseDepth: 0.07, upward: 0, converge: 0 };
    case "listening":
      // Gentle swell synced to the live rms — bounded so a loud syllable
      // brightens and breathes the core, never detonates it. (Was rms*8 /
      // rms*10 with depth up to 0.4: that drove the per-syllable strobing.)
      return {
        hue: HUE_CYAN,
        intensity: 0.7 + clamp01(rms * 5) * 0.35,
        spin: 0.1,
        pulseHz: 0.6,
        pulseDepth: 0.05,
        upward: 0,
        converge: 0.08,
      };
    case "processing":
      // Accelerated spin.
      return { hue: HUE_CYAN, intensity: 0.85, spin: 0.9, pulseHz: 0.6, pulseDepth: 0.06, upward: 0, converge: 0.25 };
    case "thinking-local":
      // Cyan intensity surge.
      return { hue: HUE_CYAN, intensity: 1.35, spin: 0.7, pulseHz: 0.8, pulseDepth: 0.1, upward: 0, converge: 0.35 };
    case "thinking-cloud":
      // Hue shift to violet + upward particle stream.
      return { hue: HUE_VIOLET, intensity: 1.2, spin: 0.7, pulseHz: 0.8, pulseDepth: 0.1, upward: 1, converge: 0.3 };
    case "speaking":
      // Calm amplitude breathing; the renderer mixes a (now gentle) synthetic
      // envelope with rms through a smoothed follower. Slower pulse + shallow
      // depth so DARWIN "breathes" while talking instead of vibrating.
      return {
        hue: HUE_CYAN,
        intensity: 0.85 + clamp01(rms * 5) * 0.3,
        spin: 0.14,
        pulseHz: 0.6,
        pulseDepth: 0.05,
        upward: 0,
        converge: 0,
      };
  }
}

/**
 * Synthetic speech envelope for coreState === "speaking" when no live rms is
 * available (the daemon mutes its own mic while talking, so audio.level rms
 * goes quiet). Deterministic in t — testable.
 */
export function syntheticSpeechEnvelope(tSeconds: number): number {
  // Slower (3.0 vs 6.1) and shallower (0.45..0.85 vs 0.25..1.0) so the speaking
  // core reads as calm breathing, not a strobe. The renderer further smooths
  // this through ampFollow, so even this gentle oscillation never jitters.
  const syllable = Math.abs(Math.sin(tSeconds * 3.0));
  const phrase = 0.6 + 0.4 * Math.abs(Math.sin(tSeconds * 0.7 + 1.2));
  return clamp01(0.45 + 0.4 * syllable * phrase);
}

/** Frame-rate-independent exponential approach (critically damped feel). */
export function damp(current: number, target: number, lambda: number, dt: number): number {
  return target + (current - target) * Math.exp(-lambda * dt);
}

/**
 * Audio-amplitude envelope follower: rms arrives noisy at ~15Hz, so feeding it
 * straight into scale/brightness strobes the core per syllable. This eases the
 * smoothed value toward the target with an ASYMMETRIC time constant — a gentle
 * attack and a slower release — so the orb swells with the voice's loudness
 * CONTOUR and settles softly, never tracking individual spikes. Pure + tested.
 */
export const AMP_ATTACK = 6;
export const AMP_RELEASE = 2.5;
export function ampFollow(current: number, target: number, dt: number): number {
  const lambda = target > current ? AMP_ATTACK : AMP_RELEASE;
  return damp(current, target, lambda, dt);
}

/* ----------------------------------------------------------- waveform mix */

/** Waveform silent-mode hysteresis band: enter the idle shimmer only below
 *  ENTER, leave it only above EXIT. A quiet room drifting across one value
 *  can no longer flip the whole bar field frame-to-frame. */
export const WAVE_SILENT_ENTER = 0.0015;
export const WAVE_SILENT_EXIT = 0.003;

/**
 * Target for the shimmer/data crossfade (1 = idle shimmer, 0 = live bars).
 * The renderer damps toward this target, so the swap is a blend, never a
 * single-frame branch. Pure + hysteretic — unit-tested.
 */
export function waveSilentTarget(prevTarget: number, meanRms: number): number {
  if (meanRms < WAVE_SILENT_ENTER) return 1;
  if (meanRms > WAVE_SILENT_EXIT) return 0;
  return prevTarget; // inside the band: hold (hysteresis)
}

/** Shortest-path hue interpolation in degrees (190 -> 268 must not wrap). */
export function dampHue(current: number, target: number, lambda: number, dt: number): number {
  let delta = ((target - current + 540) % 360) - 180;
  const moved = current + delta * (1 - Math.exp(-lambda * dt));
  return ((moved % 360) + 360) % 360;
}

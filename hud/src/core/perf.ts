/**
 * Adaptive performance governor — BIDIRECTIONAL: it drops particle/bloom tiers
 * when frame time stays above 20ms AND recovers them when frame time stays
 * comfortably below 15ms with headroom to spare. Pure — unit-tested headlessly.
 *
 * Without the recovery leg (the original), a single transient load spike (a
 * background compile, a thermal blip) dropped a tier that NEVER came back — the
 * scene stayed permanently degraded long after the machine recovered. Now it
 * ratchets both ways, with HYSTERESIS so it can't oscillate at the boundary:
 *   * DROP responsively — sustained >20ms for SUSTAIN_FRAMES.
 *   * RECOVER conservatively — sustained <15ms (a 5ms guard band below the drop
 *     threshold) for RECOVERY_FRAMES (longer than SUSTAIN_FRAMES), so a tier only
 *     comes back when there is real, durable headroom, never right at the edge.
 * A change either way starts the same wall-clock COOLDOWN so drops and recoveries
 * cannot chatter.
 *
 * Tier changes must never read as a visual cut: the render layer keeps the
 * EffectComposer permanently mounted and LERPS bloom intensity to the tier
 * target, and sheds/restores particles via geometry.setDrawRange on a
 * once-allocated buffer (no regenerated positions). The governor's job is only to
 * decide WHEN, with a wall-clock cooldown so consecutive changes can't land
 * back-to-back even at degraded frame rates (frame-counted cooldowns shrink
 * in real terms exactly when the scene is struggling).
 */

export interface PerfTier {
  particles: number;
  bloom: boolean;
}

/** Tier 0 is the full scene; higher tiers shed load. */
export const PERF_TIERS: readonly PerfTier[] = [
  { particles: 6000, bloom: true },
  { particles: 3000, bloom: true },
  { particles: 3000, bloom: false },
  { particles: 1500, bloom: false },
];

export const FRAME_BUDGET_MS = 20;
/** Consecutive over-budget frames (EMA) before a tier drop: ~1.5s @60fps. */
export const SUSTAIN_FRAMES = 90;
/** The RECOVERY threshold — a tier is only restored when the EMA stays below
 *  this. The 5ms guard band under FRAME_BUDGET_MS is the HYSTERESIS that stops
 *  the governor oscillating: recovering right at 20ms would immediately re-drop. */
export const RECOVERY_BUDGET_MS = 15;
/** Consecutive UNDER-recovery-budget frames before a tier is RESTORED: ~3s
 *  @60fps — deliberately longer than SUSTAIN_FRAMES, so the scene recovers
 *  fidelity only on durable headroom (drop fast, recover slow). */
export const RECOVERY_FRAMES = 180;
/** Wall-clock pause after ANY tier change so the new tier can settle and the
 *  crossfade can finish — independent of the (degraded) frame rate. */
export const COOLDOWN_MS = 5000;

export class PerfGovernor {
  private ema = 1000 / 60;
  private overCount = 0;
  private underCount = 0;
  private cooldownMsLeft = 0;
  tier = 0;

  /**
   * Feed one frame time (ms). Returns the current tier index into PERF_TIERS,
   * which may have just DROPPED (sustained over budget) or RECOVERED (sustained
   * headroom below the recovery guard band).
   */
  sample(frameMs: number): number {
    if (!Number.isFinite(frameMs) || frameMs <= 0) return this.tier;
    this.ema = this.ema * 0.9 + frameMs * 0.1;

    if (this.cooldownMsLeft > 0) {
      this.cooldownMsLeft = Math.max(0, this.cooldownMsLeft - frameMs);
      return this.tier;
    }
    if (this.ema > FRAME_BUDGET_MS) {
      // Over budget: build toward a DROP; any recovery progress is abandoned.
      this.overCount += 1;
      this.underCount = 0;
      if (this.overCount >= SUSTAIN_FRAMES && this.tier < PERF_TIERS.length - 1) {
        this.tier += 1;
        this.overCount = 0;
        this.cooldownMsLeft = COOLDOWN_MS;
      }
    } else if (this.ema < RECOVERY_BUDGET_MS) {
      // Comfortable headroom: build toward a RECOVERY; abandon any drop progress.
      this.underCount += 1;
      this.overCount = 0;
      if (this.underCount >= RECOVERY_FRAMES && this.tier > 0) {
        this.tier -= 1;
        this.underCount = 0;
        this.cooldownMsLeft = COOLDOWN_MS;
      }
    } else {
      // In the hysteresis band (15..20ms): neither a strong drop nor recovery
      // signal — let brief excursions in either direction decay toward 0 so only
      // SUSTAINED load drops and only SUSTAINED headroom recovers.
      if (this.overCount > 0) this.overCount -= 1;
      if (this.underCount > 0) this.underCount -= 1;
    }
    return this.tier;
  }

  current(): PerfTier {
    return PERF_TIERS[this.tier];
  }
}

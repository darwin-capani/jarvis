import { describe, expect, it } from "vitest";
import {
  COOLDOWN_MS,
  PERF_TIERS,
  PerfGovernor,
  RECOVERY_BUDGET_MS,
  RECOVERY_FRAMES,
  SUSTAIN_FRAMES,
} from "../core/perf";

describe("PerfGovernor", () => {
  it("starts at the full-scene tier", () => {
    const g = new PerfGovernor();
    expect(g.tier).toBe(0);
    expect(g.current()).toEqual(PERF_TIERS[0]);
  });

  it("holds tier 0 under sustained 60fps", () => {
    const g = new PerfGovernor();
    for (let i = 0; i < 1000; i++) g.sample(16.7);
    expect(g.tier).toBe(0);
  });

  it("drops a tier under sustained over-budget frames", () => {
    const g = new PerfGovernor();
    for (let i = 0; i < SUSTAIN_FRAMES + 60; i++) g.sample(30);
    expect(g.tier).toBe(1);
  });

  it("brief spikes do not drop a tier", () => {
    const g = new PerfGovernor();
    for (let cycle = 0; cycle < 50; cycle++) {
      for (let i = 0; i < 5; i++) g.sample(40); // 5-frame hitch
      for (let i = 0; i < 120; i++) g.sample(14); // 2s recovery
    }
    expect(g.tier).toBe(0);
  });

  it("keeps degrading down the ladder but never past the last tier", () => {
    const g = new PerfGovernor();
    // 50ms frames: plenty of wall-clock to burn through every cooldown.
    const frames =
      (SUSTAIN_FRAMES + Math.ceil(COOLDOWN_MS / 50) + 100) * PERF_TIERS.length * 2;
    for (let i = 0; i < frames; i++) {
      g.sample(50);
    }
    expect(g.tier).toBe(PERF_TIERS.length - 1);
    expect(g.current().bloom).toBe(false);
  });

  it("cooldown is WALL-CLOCK: consecutive drops are >= COOLDOWN_MS apart", () => {
    // Frame-counted cooldowns shrink in real terms exactly when frames are
    // slow — the visible back-to-back tier cuts. Verify the gap holds in ms
    // at a degraded ~33fps (30ms frames).
    const g = new PerfGovernor();
    let elapsedMs = 0;
    while (g.tier === 0) {
      g.sample(30);
      elapsedMs += 30;
      expect(elapsedMs).toBeLessThan(300_000);
    }
    const msAtFirstDrop = elapsedMs;
    while (g.tier === 1) {
      g.sample(30);
      elapsedMs += 30;
      if (elapsedMs > 600_000) break;
    }
    expect(elapsedMs - msAtFirstDrop).toBeGreaterThanOrEqual(COOLDOWN_MS);
  });

  it("cooldown holds even longer in frame terms at very slow frame rates", () => {
    // At 100ms frames (10fps) the cooldown still spans COOLDOWN_MS of wall
    // clock — i.e. at least COOLDOWN_MS/100 samples with no drop.
    const g = new PerfGovernor();
    while (g.tier === 0) g.sample(100);
    let samples = 0;
    while (g.tier === 1 && samples < 10_000) {
      g.sample(100);
      samples += 1;
    }
    expect(samples * 100).toBeGreaterThanOrEqual(COOLDOWN_MS);
  });

  it("ignores nonsense frame times", () => {
    const g = new PerfGovernor();
    g.sample(NaN);
    g.sample(-5);
    g.sample(Infinity);
    expect(g.tier).toBe(0);
  });

  it("tier table sheds load monotonically", () => {
    for (let i = 1; i < PERF_TIERS.length; i++) {
      const prev = PERF_TIERS[i - 1];
      const cur = PERF_TIERS[i];
      const prevLoad = prev.particles + (prev.bloom ? 1 : 0);
      const curLoad = cur.particles + (cur.bloom ? 1 : 0);
      expect(curLoad).toBeLessThan(prevLoad);
    }
  });

  it("tier 0 has the maximum particle count (single-buffer drawRange invariant)", () => {
    // CoreScene allocates the particle buffer ONCE at PERF_TIERS[0].particles
    // and sheds via setDrawRange — every tier must fit inside that buffer.
    for (const tier of PERF_TIERS) {
      expect(tier.particles).toBeLessThanOrEqual(PERF_TIERS[0].particles);
    }
  });

  // -- BIDIRECTIONAL RECOVERY (the fix: a spike no longer degrades forever) --

  it("RECOVERS a dropped tier under sustained headroom (was: degraded forever)", () => {
    const g = new PerfGovernor();
    // Drop one tier under load.
    for (let i = 0; i < SUSTAIN_FRAMES + 60; i++) g.sample(30);
    expect(g.tier).toBe(1);
    // Now feed sustained comfortable headroom (<15ms) long enough to burn the
    // cooldown AND accumulate RECOVERY_FRAMES — the tier must come BACK.
    for (let i = 0; i < RECOVERY_FRAMES + Math.ceil(COOLDOWN_MS / 10) + 60; i++) g.sample(10);
    expect(g.tier).toBe(0);
  });

  it("recovers only ONE tier per cooldown (no back-to-back restore chatter)", () => {
    const g = new PerfGovernor();
    // Drop to tier 2 (feed over-budget until it gets there).
    let f = 0;
    while (g.tier < 2 && f < 5000) { g.sample(30); f += 1; }
    expect(g.tier).toBe(2);
    // Recover exactly ONE tier (burns any residual cooldown, then RECOVERY_FRAMES).
    let r = 0;
    while (g.tier > 1 && r < 5000) { g.sample(10); r += 1; }
    expect(g.tier).toBe(1);
    // Immediately after a recovery a fresh COOLDOWN is running, so the NEXT frames
    // cannot restore a second tier back-to-back — it must stay at 1 for a while.
    for (let i = 0; i < 50; i++) g.sample(10);
    expect(g.tier).toBe(1);
    // Given enough further headroom it does eventually recover fully.
    let r2 = 0;
    while (g.tier > 0 && r2 < 20000) { g.sample(10); r2 += 1; }
    expect(g.tier).toBe(0);
  });

  it("HYSTERESIS: frames in the 15..20ms band neither drop nor recover (no oscillation)", () => {
    const g = new PerfGovernor();
    for (let i = 0; i < SUSTAIN_FRAMES + 60; i++) g.sample(30); // drop to tier 1
    expect(g.tier).toBe(1);
    // Sit right in the guard band (17ms) for a long time: under FRAME_BUDGET (no
    // further drop) but over RECOVERY_BUDGET (no recovery) -> tier stays put.
    for (let i = 0; i < RECOVERY_FRAMES * 3; i++) g.sample(17);
    expect(g.tier).toBe(1);
  });

  it("recovery never goes below tier 0", () => {
    const g = new PerfGovernor();
    for (let i = 0; i < RECOVERY_FRAMES * 3; i++) g.sample(8); // buttery smooth from the start
    expect(g.tier).toBe(0);
  });

  it("drops FAST but recovers SLOW (recover conservatively)", () => {
    // The recovery dwell must be longer than the drop dwell — a tier comes back
    // only on durable headroom, never on a brief lull.
    expect(RECOVERY_FRAMES).toBeGreaterThan(SUSTAIN_FRAMES);
    expect(RECOVERY_BUDGET_MS).toBeLessThan(20); // the guard band exists
  });

  it("a load spike that recovers leaves the scene at FULL fidelity (end to end)", () => {
    const g = new PerfGovernor();
    // Transient spike drops a tier...
    for (let i = 0; i < SUSTAIN_FRAMES + 30; i++) g.sample(35);
    expect(g.tier).toBeGreaterThan(0);
    const bloomLostDuringSpike = !g.current().bloom || g.current().particles < PERF_TIERS[0].particles;
    expect(bloomLostDuringSpike).toBe(true);
    // ...then the machine recovers and the full scene comes back.
    for (let i = 0; i < RECOVERY_FRAMES * 2 + Math.ceil(COOLDOWN_MS / 10); i++) g.sample(10);
    expect(g.current()).toEqual(PERF_TIERS[0]);
  });
});

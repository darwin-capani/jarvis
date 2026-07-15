import { describe, expect, it } from "vitest";
import {
  applyReduce,
  confidencePct,
  confirmReady,
  initialApplyState,
  litSegments,
  REARM_MS,
  stageLabel,
  type ApplyState,
} from "../core/heal";

/* The confidence-gauge math (pre-existing helpers) — kept covered here. */
describe("confidence gauge math", () => {
  it("maps confidence 0..1 to lit segments, clamped", () => {
    expect(litSegments(0)).toBe(0);
    expect(litSegments(1)).toBe(10);
    expect(litSegments(0.55)).toBe(6); // round
    expect(litSegments(-3)).toBe(0); // clamp low
    expect(litSegments(9)).toBe(10); // clamp high
    expect(litSegments(NaN)).toBe(0); // defensive
  });

  it("confidencePct clamps to 0..100", () => {
    expect(confidencePct(0)).toBe(0);
    expect(confidencePct(1)).toBe(100);
    expect(confidencePct(0.5)).toBe(50);
    expect(confidencePct(2)).toBe(100);
    expect(confidencePct(NaN)).toBe(0);
  });
});

/* ------------------------------------------------------------------------ *
 * The two-step-confirm gate. The whole safety point is that the apply spawn
 * is reachable ONLY via accept -> (wait >= REARM_MS) -> confirm, and a fast
 * double-click cannot blow through the confirm.
 * ------------------------------------------------------------------------ */
describe("Accept two-step confirm gate", () => {
  it("starts idle", () => {
    const s = initialApplyState();
    expect(s.phase).toBe("idle");
    expect(s.armedAt).toBeNull();
  });

  it("accept arms the confirm step (idle -> confirming)", () => {
    const s = applyReduce(initialApplyState(), { type: "accept", at: 1000 });
    expect(s.phase).toBe("confirming");
    expect(s.armedAt).toBe(1000);
  });

  it("a confirm fired within the re-arm window is IGNORED (no skip)", () => {
    let s = applyReduce(initialApplyState(), { type: "accept", at: 1000 });
    // a double-click: confirm only 50ms after the accept (< REARM_MS)
    s = applyReduce(s, { type: "confirm", at: 1000 + 50 });
    expect(s.phase).toBe("confirming"); // still waiting — NOT applying
  });

  it("confirm exactly at the re-arm boundary is honored (confirming -> applying)", () => {
    let s = applyReduce(initialApplyState(), { type: "accept", at: 1000 });
    s = applyReduce(s, { type: "confirm", at: 1000 + REARM_MS });
    expect(s.phase).toBe("applying");
    expect(s.armedAt).toBeNull();
  });

  it("confirmReady mirrors the reducer guard", () => {
    const armed: ApplyState = {
      ...initialApplyState(),
      phase: "confirming",
      armedAt: 1000,
    };
    expect(confirmReady(armed, 1000 + REARM_MS - 1)).toBe(false);
    expect(confirmReady(armed, 1000 + REARM_MS)).toBe(true);
    // not confirming -> never ready
    expect(confirmReady(initialApplyState(), 99999)).toBe(false);
    // confirming but unarmed (defensive) -> never ready
    expect(confirmReady({ ...armed, armedAt: null }, 99999)).toBe(false);
  });

  it("confirm from idle (no prior accept) does nothing", () => {
    const s = applyReduce(initialApplyState(), { type: "confirm", at: 5000 });
    expect(s.phase).toBe("idle");
  });

  it("a stray accept while applying is ignored (cannot re-arm mid-apply)", () => {
    let s = applyReduce(initialApplyState(), { type: "accept", at: 1000 });
    s = applyReduce(s, { type: "confirm", at: 1000 + REARM_MS });
    expect(s.phase).toBe("applying");
    const s2 = applyReduce(s, { type: "accept", at: 9000 });
    expect(s2).toBe(s); // unchanged
  });
});

/* ------------------------------------------------------------------------ *
 * The apply lifecycle: idle -> confirming -> applying -> applied | failed.
 * ------------------------------------------------------------------------ */
describe("apply lifecycle", () => {
  function toApplying(): ApplyState {
    let s = applyReduce(initialApplyState(), { type: "accept", at: 0 });
    s = applyReduce(s, { type: "confirm", at: REARM_MS });
    return s;
  }

  it("stage updates only land while applying", () => {
    let s = toApplying();
    s = applyReduce(s, { type: "applyStage", stage: "revalidating" });
    expect(s.stage).toBe("revalidating");
    s = applyReduce(s, { type: "applyStage", stage: "rebuilding" });
    expect(s.stage).toBe("rebuilding");

    // a stage update arriving in idle is ignored
    const idle = applyReduce(initialApplyState(), {
      type: "applyStage",
      stage: "applying",
    });
    expect(idle.phase).toBe("idle");
    expect(idle.stage).toBe("");
  });

  it("applyOk -> applied with restart-aware message", () => {
    const s = applyReduce(toApplying(), {
      type: "applyOk",
      restarted: true,
      message: "Healed. DARWIN restarted on the new build.",
    });
    expect(s.phase).toBe("applied");
    expect(s.restarted).toBe(true);
    expect(s.message).toMatch(/restarted/i);
  });

  it("applyFail -> failed and carries the reason", () => {
    const s = applyReduce(toApplying(), {
      type: "applyFail",
      message: "Validation/apply failed (revalidating). Patch NOT applied.",
    });
    expect(s.phase).toBe("failed");
    expect(s.message).toMatch(/NOT applied/i);
  });

  it("a terminal action cannot fire unless applying (no spurious success)", () => {
    const idleOk = applyReduce(initialApplyState(), {
      type: "applyOk",
      restarted: false,
      message: "x",
    });
    expect(idleOk.phase).toBe("idle");
    const confirming = applyReduce(initialApplyState(), { type: "accept", at: 0 });
    const okFromConfirming = applyReduce(confirming, {
      type: "applyOk",
      restarted: false,
      message: "x",
    });
    expect(okFromConfirming.phase).toBe("confirming"); // not jumped to applied
  });

  it("reset backs out of confirming and clears terminal states, but NOT mid-apply", () => {
    // confirming -> reset -> idle
    const confirming = applyReduce(initialApplyState(), { type: "accept", at: 0 });
    expect(applyReduce(confirming, { type: "reset" }).phase).toBe("idle");

    // applied -> reset -> idle
    const applied = applyReduce(
      applyReduce(initialApplyState(), { type: "accept", at: 0 }),
      { type: "confirm", at: REARM_MS },
    );
    const ok = applyReduce(applied, {
      type: "applyOk",
      restarted: false,
      message: "done",
    });
    expect(applyReduce(ok, { type: "reset" }).phase).toBe("idle");

    // applying -> reset is REFUSED (spawn in flight)
    const applying = applyReduce(
      applyReduce(initialApplyState(), { type: "accept", at: 0 }),
      { type: "confirm", at: REARM_MS },
    );
    expect(applyReduce(applying, { type: "reset" }).phase).toBe("applying");
  });
});

describe("stage labels", () => {
  it("maps script stage tokens to human spinner text", () => {
    expect(stageLabel("revalidating")).toMatch(/cargo check \+ full test/i);
    expect(stageLabel("applying")).toBe("Applying…");
    expect(stageLabel("rebuilding")).toBe("Rebuilding…");
    expect(stageLabel("")).toBe("Starting…");
    expect(stageLabel("starting…")).toBe("Starting…");
    // unknown token still renders something sensible
    expect(stageLabel("whatever")).toBe("whatever…");
  });
});

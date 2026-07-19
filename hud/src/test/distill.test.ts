import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import DistillPanel from "../components/DistillPanel";
import { parseDistillStatus, type DistillStatus, type TelemetryEnvelope } from "../core/events";
import { initialState, reduce, type HudState } from "../core/state";

let counter = 0;
function env(event: string, data: Record<string, unknown>, source = "system"): TelemetryEnvelope {
  counter += 1;
  return { ts: `2026-07-13T00:00:${String(counter % 60).padStart(2, "0")}Z`, source, event, data };
}
function connected() {
  return reduce(initialState(), { type: "ws.connected", at: 0 });
}
function tel(state: HudState, e: TelemetryEnvelope) {
  return reduce(state, { type: "telemetry", envelope: e, at: 1000 });
}

/** Mirrors daemon/src/distill.rs::status_payload. */
const offWire = {
  enabled: false,
  dep_verified: false,
  dependency: "Apple Silicon + mlx-lm (verified only on-device)",
  examples_ready: 0,
  min_examples: 32,
  ready_to_train: false,
  gated_promotion: true,
  adapter_live: false,
  adapter_pointer: "none",
  last_run: null,
  promoted: null,
};

const liveRunWire = {
  created: "2026-07-13T10:00:00Z",
  base_model: "mlx-community/Qwen3-4B-Instruct-2507-4bit",
  example_count: 80,
  status: "trained",
  promoted: true,
  held_out_base_loss: 2.5,
  held_out_adapter_loss: 2.2,
};

describe("parseDistillStatus (never fabricates readiness or liveness)", () => {
  it("parses the off state", () => {
    expect(parseDistillStatus(offWire)).toEqual({
      enabled: false,
      depVerified: false,
      dependency: "Apple Silicon + mlx-lm (verified only on-device)",
      examplesReady: 0,
      minExamples: 32,
      readyToTrain: false,
      gatedPromotion: true,
      adapterLive: false,
      adapterPointer: "none",
      lastRun: null,
      promoted: null,
    });
  });

  it("never lets a payload claim dep-verified or un-say the measured gate", () => {
    const spoofed = parseDistillStatus({
      ...offWire,
      enabled: true,
      dep_verified: "yes", // non-boolean -> false
      gated_promotion: false, // pinned true regardless
      examples_ready: 50,
      ready_to_train: true,
    });
    expect(spoofed.depVerified).toBe(false);
    expect(spoofed.gatedPromotion).toBe(true);
    expect(spoofed.readyToTrain).toBe(true);
  });

  it("adapter_live requires BOTH the literal true AND a live pointer state", () => {
    // Live claim with a non-live pointer -> refused (conservative).
    const inconsistent = parseDistillStatus({
      ...offWire,
      adapter_live: true,
      adapter_pointer: "installed-mismatch",
    });
    expect(inconsistent.adapterLive).toBe(false);
    expect(inconsistent.adapterPointer).toBe("installed-mismatch");
    // An unknown pointer token degrades to "none" and kills the live claim.
    const unknownPtr = parseDistillStatus({
      ...offWire,
      adapter_live: true,
      adapter_pointer: "hacked",
    });
    expect(unknownPtr.adapterLive).toBe(false);
    expect(unknownPtr.adapterPointer).toBe("none");
    // A consistent live frame parses live, WITH the measured losses.
    const live = parseDistillStatus({
      ...offWire,
      enabled: true,
      adapter_live: true,
      adapter_pointer: "live",
      promoted: liveRunWire,
    });
    expect(live.adapterLive).toBe(true);
    expect(live.promoted?.heldOutBaseLoss).toBe(2.5);
    expect(live.promoted?.heldOutAdapterLoss).toBe(2.2);
    expect(live.promoted?.promoted).toBe(true);
  });

  it("coerces an unknown last-run status to failed and reads promoted strictly", () => {
    const run = parseDistillStatus({
      ...offWire,
      last_run: { created: "t", base_model: "b", example_count: 40, status: "hacked", promoted: 1 },
    }).lastRun;
    expect(run?.status).toBe("failed");
    expect(run?.promoted).toBe(false);
    expect(run?.heldOutBaseLoss).toBeNull();
    // A genuine trained/promoted:false round-trips.
    const ok = parseDistillStatus({
      ...offWire,
      last_run: { created: "t", base_model: "b", example_count: 40, status: "trained", promoted: false },
    }).lastRun;
    expect(ok?.status).toBe("trained");
    expect(ok?.promoted).toBe(false);
  });

  it("degrades a malformed frame to the honest off/inert state", () => {
    const d = parseDistillStatus({});
    expect(d.enabled).toBe(false);
    expect(d.readyToTrain).toBe(false);
    expect(d.gatedPromotion).toBe(true);
    expect(d.adapterLive).toBe(false);
    expect(d.adapterPointer).toBe("none");
    expect(d.lastRun).toBeNull();
    expect(d.promoted).toBeNull();
  });
});

describe("distill.status reducer", () => {
  it("is null until the first frame, then set", () => {
    let s = connected();
    expect(s.distill).toBeNull();
    s = tel(s, env("distill.status", { ...offWire, enabled: true, examples_ready: 40, ready_to_train: true }));
    expect(s.distill?.enabled).toBe(true);
    expect(s.distill?.readyToTrain).toBe(true);
  });
});

describe("DistillPanel", () => {
  const render = (distill: DistillStatus | null) =>
    renderToStaticMarkup(createElement(DistillPanel, { distill }));

  it("renders nothing before the first frame", () => {
    expect(render(null)).toBe("");
  });

  it("shows OFF and the measured-gate footnote (no stale never-promoted claim)", () => {
    const html = render(parseDistillStatus(offWire));
    expect(html).toContain("SELF-DISTILL // LoRA");
    expect(html).toContain("OFF");
    expect(html).toContain("0/32 graded examples ready");
    expect(html).toContain("PROMOTION · MEASURED-GATED");
    expect(html).toContain("measurably beats the base model");
    // The pre-promotion-feature claims must be gone: with [distill].auto_promote
    // an adapter CAN go live without a further operator act (still measured).
    expect(html).not.toContain("NEVER AUTO-PROMOTED");
    expect(html).not.toContain("never swapped into");
  });

  it("shows ARMED · NEEDS DEVICE when the dataset is ready but the device gate isn't verified", () => {
    const html = render(
      parseDistillStatus({ ...offWire, enabled: true, examples_ready: 40, ready_to_train: true }),
    );
    expect(html).toContain("ARMED · NEEDS DEVICE");
  });

  it("shows a staged (not promoted) last run and no live line", () => {
    const html = render(
      parseDistillStatus({
        ...offWire,
        enabled: true,
        last_run: { created: "t", base_model: "b", example_count: 80, status: "trained", promoted: false },
      }),
    );
    expect(html).toContain("last run: trained");
    expect(html).toContain("80 examples");
    expect(html).toContain("· staged (not live)");
    expect(html).not.toContain("· PROMOTED");
    expect(html).not.toContain("live adapter");
  });

  it("shows the LIVE adapter with its measured held-out win", () => {
    const html = render(
      parseDistillStatus({
        ...offWire,
        enabled: true,
        adapter_live: true,
        adapter_pointer: "live",
        promoted: liveRunWire,
        last_run: liveRunWire,
      }),
    );
    expect(html).toContain("ADAPTER LIVE · MEASURED WIN");
    expect(html).toContain("beat base 2.200 vs 2.500 held-out");
    expect(html).toContain("reversible");
    expect(html).toContain("· PROMOTED");
  });

  it("warns on a mismatched or quant-undecided pointer instead of claiming live", () => {
    const mism = render(parseDistillStatus({ ...offWire, enabled: true, adapter_pointer: "installed-mismatch" }));
    expect(mism).toContain("match the resident model");
    expect(mism).not.toContain("ADAPTER LIVE");
    const quant = render(
      parseDistillStatus({ ...offWire, enabled: true, adapter_pointer: "installed-quant-undecided" }),
    );
    expect(quant).toContain("explicit quant decides");
    expect(quant).not.toContain("ADAPTER LIVE");
  });
});

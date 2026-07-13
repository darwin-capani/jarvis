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
  never_promotes: true,
  last_run: null,
};

describe("parseDistillStatus (never fabricates device readiness or promotion)", () => {
  it("parses the off state", () => {
    expect(parseDistillStatus(offWire)).toEqual({
      enabled: false,
      depVerified: false,
      dependency: "Apple Silicon + mlx-lm (verified only on-device)",
      examplesReady: 0,
      minExamples: 32,
      readyToTrain: false,
      neverPromotes: true,
      lastRun: null,
    });
  });

  it("never lets a payload claim dep-verified or un-say never-promotes", () => {
    const spoofed = parseDistillStatus({
      ...offWire,
      enabled: true,
      dep_verified: "yes", // non-boolean -> false
      never_promotes: false, // pinned true regardless
      examples_ready: 50,
      ready_to_train: true,
    });
    expect(spoofed.depVerified).toBe(false);
    expect(spoofed.neverPromotes).toBe(true);
    expect(spoofed.readyToTrain).toBe(true);
  });

  it("coerces an unknown last-run status to failed and reads promoted strictly", () => {
    const run = parseDistillStatus({
      ...offWire,
      last_run: { created: "t", base_model: "b", example_count: 40, status: "hacked", promoted: 1 },
    }).lastRun;
    expect(run?.status).toBe("failed");
    expect(run?.promoted).toBe(false);
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
    expect(d.neverPromotes).toBe(true);
    expect(d.lastRun).toBeNull();
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

  it("shows OFF and the never-auto-promoted footnote", () => {
    const html = render(parseDistillStatus(offWire));
    expect(html).toContain("SELF-DISTILL // LoRA");
    expect(html).toContain("OFF");
    expect(html).toContain("0/32 graded examples ready");
    expect(html).toContain("never swapped into");
    expect(html).toContain("promotion is a deliberate step");
  });

  it("shows ARMED · NEEDS DEVICE when the dataset is ready but the device gate isn't verified", () => {
    const html = render(
      parseDistillStatus({ ...offWire, enabled: true, examples_ready: 40, ready_to_train: true }),
    );
    expect(html).toContain("ARMED · NEEDS DEVICE");
  });

  it("shows a staged (not promoted) last run", () => {
    const html = render(
      parseDistillStatus({
        ...offWire,
        enabled: true,
        last_run: { created: "t", base_model: "b", example_count: 80, status: "trained", promoted: false },
      }),
    );
    expect(html).toContain("last run: trained");
    expect(html).toContain("80 examples");
    expect(html).toContain("staged (not live)");
    // The run line shows "staged (not live)", not "PROMOTED" (the frame tag's
    // standing "NEVER AUTO-PROMOTED" is a separate, honest label).
    expect(html).toContain("· staged (not live)");
    expect(html).not.toContain("· PROMOTED");
  });
});

import type { DistillStatus } from "../core/events";
import Frame from "./Frame";

/**
 * SELF-DISTILL // LoRA — the honest state of the armed-but-inert on-device
 * self-distillation pipeline (daemon distill.rs).
 *
 * HONESTY CONTRACT (do not regress):
 *   - SHIPS OFF: OFF unless the operator enables [distill]. The pill says so.
 *   - The device dependency (Apple Silicon + mlx-lm) is UNVERIFIABLE from the
 *     daemon, so an armed pipeline reads ARMED · NEEDS DEVICE (verified=false)
 *     — never a fabricated "ready to train".
 *   - A trained adapter is STAGED, NEVER auto-promoted into the live model.
 *     The standing footnote says so, and any last run's `promoted` is shown
 *     (always false — promotion is a separate, deliberate act).
 */
export default function DistillPanel({ distill }: { distill: DistillStatus | null }) {
  if (distill === null) return null;

  const state = pipelineState(distill);
  return (
    <div className="distill-panel">
      <Frame title="SELF-DISTILL // LoRA" tag="STAGED · NEVER AUTO-PROMOTED">
        <div className="distill-body">
          <div className="distill-head">
            <span className={`distill-pill ${state.cls}`}>{state.label}</span>
            <span className="distill-examples dim-note">
              {distill.examplesReady}/{distill.minExamples} graded examples ready
            </span>
          </div>
          {distill.lastRun !== null && (
            <div className="distill-run dim-note">
              last run: {distill.lastRun.status}
              {" · "}
              {distill.lastRun.exampleCount} examples
              {" · "}
              {distill.lastRun.promoted ? "PROMOTED" : "staged (not live)"}
            </div>
          )}
          <div className="distill-foot dim-note">
            Learns a personal adapter from your own redacted, un-redirected turns.
            A trained adapter is staged under state/lora and never swapped into
            the live model on its own — promotion is a deliberate step.
          </div>
        </div>
      </Frame>
    </div>
  );
}

function pipelineState(d: DistillStatus): { label: string; cls: string } {
  if (!d.enabled) return { label: "OFF", cls: "off" };
  if (d.readyToTrain) {
    // Dataset is ready; the device gate is still separate + unverified.
    return { label: "ARMED · NEEDS DEVICE", cls: "armed" };
  }
  return { label: "ARMED · GATHERING", cls: "armed" };
}

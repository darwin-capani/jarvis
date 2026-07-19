import type { DistillStatus } from "../core/events";
import Frame from "./Frame";

/**
 * SELF-DISTILL // LoRA — the honest state of the on-device self-distillation
 * pipeline (daemon distill.rs).
 *
 * HONESTY CONTRACT (do not regress):
 *   - SHIPS OFF: OFF unless the operator enables [distill]. The pill says so.
 *   - The device dependency (Apple Silicon + mlx-lm) is UNVERIFIABLE from the
 *     daemon, so an armed pipeline reads ARMED · NEEDS DEVICE (verified=false)
 *     — never a fabricated "ready to train".
 *   - PROMOTION IS MEASURED-GATED: a trained adapter goes live ONLY on a strict
 *     held-out win over the base model (deliberate promote, or the ships-OFF
 *     auto_promote chain), and it is reversible. The frame tag + footnote say
 *     exactly that; the LIVE line appears only for a daemon-VERIFIED live
 *     adapter (adapterLive), with its measured losses when present.
 *   - A pointer the server would refuse (base mismatch) or whose fusing an
 *     explicit quant decides at load time is surfaced as its own warning line —
 *     never rendered as "live".
 */
export default function DistillPanel({ distill }: { distill: DistillStatus | null }) {
  if (distill === null) return null;

  const state = pipelineState(distill);
  const live = distill.adapterLive ? distill.promoted : null;
  return (
    <div className="distill-panel">
      <Frame
        title="SELF-DISTILL // LoRA"
        tag={distill.adapterLive ? "ADAPTER LIVE · MEASURED WIN" : "PROMOTION · MEASURED-GATED"}
      >
        <div className="distill-body">
          <div className="distill-head">
            <span className={`distill-pill ${state.cls}`}>{state.label}</span>
            <span className="distill-examples dim-note">
              {distill.examplesReady}/{distill.minExamples} graded examples ready
            </span>
          </div>
          {live !== null && (
            <div className="distill-live dim-note">
              live adapter
              {live.heldOutAdapterLoss !== null && live.heldOutBaseLoss !== null
                ? `: beat base ${live.heldOutAdapterLoss.toFixed(3)} vs ${live.heldOutBaseLoss.toFixed(3)} held-out`
                : ""}
              {" · reversible"}
            </div>
          )}
          {distill.adapterPointer === "installed-mismatch" && (
            <div className="distill-live dim-note">
              installed adapter doesn&apos;t match the resident model — base serves
            </div>
          )}
          {distill.adapterPointer === "installed-quant-undecided" && (
            <div className="distill-live dim-note">
              installed adapter — an explicit quant decides at the server&apos;s model load
            </div>
          )}
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
            It goes live only if it measurably beats the base model on your
            held-out turns — a tie or a loss keeps the current model — and
            promotion is reversible.
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

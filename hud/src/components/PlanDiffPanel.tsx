import type { PlanDiff } from "../core/events";
import Frame from "./Frame";

/**
 * PLAN // DIFF — the structured, STATE-BOUND diff for the currently PARKED
 * consequential action (daemon plan.rs -> `plan.diff`). It upgrades the
 * confirmation preview from prose to a field-level before -> after diff bound to a
 * hash of the current state.
 *
 * HONESTY CONTRACT (do not regress):
 *   - ADVISORY ONLY on the HUD. Nothing here approves, denies, or executes
 *     anything — the daemon owns the gate. The state-hash the daemon binds can
 *     only make ITS gate STRICTER (re-park on drift), never approve a drifted
 *     action; this panel just SHOWS the diff, and says so in its footnote.
 *   - Zero fabrication: every change is a real before/after the daemon computed
 *     (bounded + parsed again here); an empty/malformed diff renders NOTHING.
 *   - A `drift` re-park (phase="confirm") is flagged loudly — the state changed
 *     since the plan was first shown, so the daemon re-parked a FRESH plan.
 *   - The panel clears when the pending resolves or is superseded — it never
 *     describes an action that is no longer awaiting the user.
 */
export default function PlanDiffPanel({ plan }: { plan: PlanDiff | null }) {
  if (plan === null) return null;

  return (
    <div className="plandiff-panel">
      <Frame title="PLAN // DIFF" tag={plan.drift ? "STATE DRIFTED" : "PRE-CONFIRM"}>
        <div className="plandiff-body">
          <div className="plandiff-head">
            <span className={`plandiff-pill${plan.drift ? " drift" : ""}`}>
              {plan.drift ? "RE-PARKED · STATE CHANGED" : "AWAITING YOUR CONFIRM"}
            </span>
            <span className="plandiff-tool">{plan.tool}</span>
          </div>
          {plan.summary !== "" && <div className="plandiff-summary">{plan.summary}</div>}
          <ul className="plandiff-changes">
            {plan.changes.map((c, i) => (
              <li key={`${c.resource.slice(0, 40)}-${i}`} className="plandiff-change">
                <div className="plandiff-resource">{c.resource}</div>
                <div className="plandiff-delta">
                  <span className="plandiff-before">{c.before}</span>
                  <span className="plandiff-arrow" aria-hidden="true"> → </span>
                  <span className="plandiff-after">{c.after}</span>
                </div>
              </li>
            ))}
          </ul>
          {plan.drift && (
            <div className="plandiff-drift dim-note">
              The state changed since I showed you — this is the current plan. Say
              confirm to apply it, or cancel to drop it.
            </div>
          )}
          <div className="plandiff-foot dim-note">
            Advisory only — the gate is unchanged. Nothing runs without your spoken
            confirm, and the plan re-checks the state before it applies.
          </div>
        </div>
      </Frame>
    </div>
  );
}

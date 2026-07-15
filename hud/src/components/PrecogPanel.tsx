import type { PrecogPlan } from "../core/events";
import Frame from "./Frame";

/**
 * PRECOG // WHAT-IF — the read-only counterfactual command surface, fed by the
 * daemon's `precog.plan` (daemon/src/simulate.rs PlannedOutcome::telemetry()),
 * emitted by the router when the owner asks "what would you do if I said X".
 *
 * It answers one question honestly: if the user actually said `X`, what WOULD
 * DARWIN do? The daemon runs the SAME pipeline a live turn would — classify ->
 * capability selector -> agent delegation -> model tier -> confirmation-gate
 * PROJECTION -> reversibility — UP TO but NEVER THROUGH the gate, and hands the
 * plan here. It NEVER executes and NEVER satisfies a gate: the simulate path holds
 * no actuator / memory-write / inference handle by construction, so a simulated
 * turn cannot fire an action even a benign one.
 *
 * HONESTY CONTRACT (do not regress):
 *   - DESCRIPTION, NEVER EXECUTION. The panel SHOWS a plan the daemon projected —
 *     there is NO button here, nothing to run. The frame's `executed` /
 *     `satisfiedAGate` are PINNED false by the parser, and this panel states that
 *     plainly, so a hostile payload can never make the surface claim a simulation
 *     ran or cleared a gate.
 *   - HONEST ABOUT THE GATE. When a real run WOULD park at the confirmation gate,
 *     the panel says so (a "WOULD PARK" pill + the projected tool + whether it is
 *     reversible). PRECOG only ever REPORTS the park — it never satisfies it. When
 *     the mode is "clarify" the panel reports that a real run would ASK first and
 *     act on nothing.
 *   - SECRET-FREE. Only the pipeline DECISIONS + the (already user-spoken)
 *     hypothetical are shown — no fact value, no memory, no tool output (nothing
 *     ran, so there is nothing to leak).
 *
 * The reducer holds `precogPlan` at null until the owner asks a PRECOG query, so
 * this component renders nothing until there is a real plan to show — mirroring the
 * other event-fed panels (CustomsPanel, BriefFocusPanel).
 */
export default function PrecogPanel({ plan }: { plan: PrecogPlan | null }) {
  // Nothing to show until a PRECOG query produced a plan. Mirrors the other
  // event-fed panels: the reducer holds this null and we render nothing.
  if (plan === null) return null;

  const clarify = plan.mode === "clarify";

  return (
    <div className="precog-panel">
      <Frame title="PRECOG // WHAT-IF" tag="SIMULATION · NEVER RUNS">
        <div className="precog-body">
          <div className="precog-head">
            <span className="precog-label">IF YOU SAID</span>
            <span className="precog-utterance" title="the hypothetical utterance PRECOG simulated">
              &ldquo;{plan.utterance}&rdquo;
            </span>
          </div>

          <div className="precog-grid">
            <PrecogCell k="INTENT" v={plan.intent || "—"} title="the classifier intent for the hypothetical" />
            <PrecogCell k="AGENT" v={plan.agent || "—"} title="the agent Darwin-Prime would delegate to" />
            <PrecogCell k="MODE" v={plan.mode || "—"} title="the capability mode a real run would route to" />
            <PrecogCell k="TIER" v={plan.tier || "—"} title="the model tier a real run would resolve to" />
          </div>

          <PrecogGate plan={plan} clarify={clarify} />

          {plan.why.length > 0 && <div className="precog-why">{plan.why}</div>}

          <div className="precog-foot dim-note">
            This is a SIMULATION — nothing ran and no gate was satisfied. PRECOG runs
            the same pipeline a real turn would, but stops before the gate: it can
            report that a real run would PARK for your spoken yes, but it never fires
            an action itself — not even a benign one.
          </div>
        </div>
      </Frame>
    </div>
  );
}

/** One decision cell in the plan grid: a label + its value. Purely presentational. */
function PrecogCell({ k, v, title }: { k: string; v: string; title: string }) {
  return (
    <div className="precog-cell" title={title}>
      <span className="precog-k">{k}</span>
      <span className="precog-v">{v}</span>
    </div>
  );
}

/** The gate verdict — the panel's headline. Three honest states:
 *   - CLARIFY: a real run would ASK a one-line question first and act on nothing.
 *   - WOULD PARK: a real run would park a consequential tool at the confirmation
 *     gate for a spoken yes (with the projected tool + reversibility), which PRECOG
 *     never satisfies itself.
 *   - NO GATE: no consequential action — a plain turn, nothing to confirm. */
function PrecogGate({ plan, clarify }: { plan: PrecogPlan; clarify: boolean }) {
  if (clarify) {
    return (
      <div className="precog-gate clarify">
        <span className="precog-pill clarify" title="a real run would ask a clarifying question first">
          WOULD CLARIFY
        </span>
        <span className="precog-gate-note">
          A real run would ask one clarifying question first — it would act on nothing.
        </span>
      </div>
    );
  }
  if (plan.wouldPark) {
    return (
      <div className="precog-gate park">
        <span className="precog-pill park" title="a real run would park this action at the confirmation gate for a spoken yes">
          WOULD PARK
        </span>
        {plan.tool !== null && (
          <span className="precog-tool" title="the projected consequential tool a real run would engage">
            {plan.tool}
          </span>
        )}
        <span
          className={`precog-pill ${plan.reversible ? "reversible" : "irreversible"}`}
          title={
            plan.reversible
              ? "the planned action has a safe mechanical inverse (undoable)"
              : "the planned action has no safe mechanical inverse (not undoable)"
          }
        >
          {plan.reversible ? "REVERSIBLE" : "IRREVERSIBLE"}
        </span>
        <span className="precog-gate-note">
          A real run would PARK for your spoken yes — PRECOG never satisfies that gate.
        </span>
      </div>
    );
  }
  return (
    <div className="precog-gate clear">
      <span className="precog-pill clear" title="no consequential action — a plain turn, nothing to confirm">
        NO GATE
      </span>
      <span className="precog-gate-note">No consequential action — nothing to confirm.</span>
    </div>
  );
}

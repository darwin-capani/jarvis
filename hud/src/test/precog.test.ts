import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import PrecogPanel from "../components/PrecogPanel";
import {
  parsePrecogPlan,
  type PrecogPlan,
  type TelemetryEnvelope,
} from "../core/events";
import { HudState, initialState, reduce } from "../core/state";

/* helpers ------------------------------------------------------------------ */

let counter = 0;
function env(
  event: string,
  data: Record<string, unknown> = {},
  source = "local",
): TelemetryEnvelope {
  counter += 1;
  return {
    ts: `2026-07-15T12:00:${String(counter % 60).padStart(2, "0")}Z`,
    source,
    event,
    data,
  };
}

function tel(state: HudState, e: TelemetryEnvelope, at = 1000): HudState {
  return reduce(state, { type: "telemetry", envelope: e, at });
}

function connected(at = 0): HudState {
  return reduce(initialState(), { type: "ws.connected", at });
}

function renderPanel(plan: PrecogPlan | null): string {
  return renderToStaticMarkup(createElement(PrecogPanel, { plan }));
}

/** The daemon's PlannedOutcome::telemetry() shape for a CONSEQUENTIAL, IRREVERSIBLE
 *  action ("send an email ...") — projects gmail_send, would_park, not reversible.
 *  Mirrors simulate.rs's consequential_email_plans_a_park_and_is_irreversible. */
const planEmail: Record<string, unknown> = {
  utterance: "send an email to the team about the launch",
  intent: "conversation",
  agent: "darwin",
  mode: "one_shot",
  tier: "heavy",
  tool: "gmail_send",
  would_park: true,
  reversible: false,
  confidence: 0.8,
  why: "A real run would delegate to darwin and PARK 'gmail_send' at the confirmation gate for a spoken yes (irreversible); PRECOG never satisfies that gate.",
  executed: false,
  satisfied_a_gate: false,
};

/** A benign, NON-gated turn ("open safari") — no tool, no park, reversible. */
const planBenign: Record<string, unknown> = {
  utterance: "open safari",
  intent: "app.launch",
  agent: "oracle",
  mode: "one_shot",
  tier: "local",
  tool: null,
  would_park: false,
  reversible: true,
  confidence: 0.95,
  why: "A real run would handle this as a one_shot turn on the local tier via oracle, with no consequential action to confirm.",
  executed: false,
  satisfied_a_gate: false,
};

/* parser ------------------------------------------------------------------- */

describe("parsePrecogPlan", () => {
  it("parses a consequential plan and preserves the park + reversibility verdict", () => {
    const p = parsePrecogPlan(planEmail);
    expect(p).not.toBeNull();
    expect(p!.tool).toBe("gmail_send");
    expect(p!.wouldPark).toBe(true);
    expect(p!.reversible).toBe(false);
    expect(p!.mode).toBe("one_shot");
    expect(p!.confidence).toBeCloseTo(0.8, 5);
  });

  it("parses a benign plan with no gated tool (tool null, no park)", () => {
    const p = parsePrecogPlan(planBenign);
    expect(p).not.toBeNull();
    expect(p!.tool).toBeNull();
    expect(p!.wouldPark).toBe(false);
    expect(p!.reversible).toBe(true);
  });

  it("PINS the never-executes contract even when a hostile payload claims otherwise", () => {
    // A hostile/garbled frame that tries to claim the simulation RAN and SATISFIED
    // a gate must NEVER be honored — the parser pins both to false HUD-side, so the
    // panel can never claim a simulation executed or cleared a gate.
    const hostile = { ...planEmail, executed: true, satisfied_a_gate: true };
    const p = parsePrecogPlan(hostile);
    expect(p).not.toBeNull();
    expect(p!.executed).toBe(false);
    expect(p!.satisfiedAGate).toBe(false);
  });

  it("clamps a junk confidence into [0,1] and drifts nothing else", () => {
    expect(parsePrecogPlan({ ...planEmail, confidence: 5 })!.confidence).toBe(1);
    expect(parsePrecogPlan({ ...planEmail, confidence: -3 })!.confidence).toBe(0);
    expect(parsePrecogPlan({ ...planEmail, confidence: "x" })!.confidence).toBe(0);
  });

  it("treats an empty string tool as no gated tool (null)", () => {
    expect(parsePrecogPlan({ ...planEmail, tool: "" })!.tool).toBeNull();
  });

  it("returns null for a truly empty/garbled frame (no utterance and no mode)", () => {
    expect(parsePrecogPlan({})).toBeNull();
    expect(parsePrecogPlan({ intent: "x" })).toBeNull();
  });

  it("defaults defensively for a partial frame (missing booleans)", () => {
    const p = parsePrecogPlan({ utterance: "do a thing", mode: "one_shot" });
    expect(p).not.toBeNull();
    expect(p!.wouldPark).toBe(false); // absent -> false
    expect(p!.reversible).toBe(true); // absent -> true (nothing to reverse)
    expect(p!.tool).toBeNull();
  });
});

/* reducer ------------------------------------------------------------------ */

describe("precog.plan reducer", () => {
  it("holds precogPlan null until a PRECOG query arrives", () => {
    expect(initialState().precogPlan).toBeNull();
  });

  it("stores the latest plan and replaces it in place on the next query", () => {
    let s = connected();
    s = tel(s, env("precog.plan", planEmail));
    expect(s.precogPlan).not.toBeNull();
    expect(s.precogPlan!.tool).toBe("gmail_send");
    // A second query replaces the surface in place (latest plan wins).
    s = tel(s, env("precog.plan", planBenign));
    expect(s.precogPlan!.utterance).toBe("open safari");
    expect(s.precogPlan!.wouldPark).toBe(false);
  });

  it("drops a junk frame rather than churning a hollow card", () => {
    let s = connected();
    s = tel(s, env("precog.plan", planEmail));
    const before = s.precogPlan;
    // A garbled frame (no utterance, no mode) parses to null and is ignored.
    s = tel(s, env("precog.plan", {}));
    expect(s.precogPlan).toBe(before);
  });
});

/* panel -------------------------------------------------------------------- */

describe("PrecogPanel", () => {
  it("renders nothing until there is a plan", () => {
    expect(renderPanel(null)).toBe("");
  });

  it("shows the hypothetical, the projected tool, and the WOULD PARK verdict", () => {
    const html = renderPanel(parsePrecogPlan(planEmail));
    expect(html).toContain("PRECOG // WHAT-IF");
    expect(html).toContain("send an email to the team about the launch");
    expect(html).toContain("gmail_send");
    expect(html).toContain("WOULD PARK");
    expect(html).toContain("IRREVERSIBLE");
    // The honest gate contract is stated.
    expect(html.toLowerCase()).toContain("never satisfies that gate");
    // No execution is ever claimed.
    expect(html.toLowerCase()).toContain("nothing ran");
  });

  it("shows the NO GATE verdict for a benign turn (no consequential action)", () => {
    const html = renderPanel(parsePrecogPlan(planBenign));
    expect(html).toContain("NO GATE");
    expect(html).not.toContain("WOULD PARK");
    expect(html).toContain("open safari");
  });

  it("shows a WOULD CLARIFY verdict for a clarify plan (acts on nothing)", () => {
    const clarifyPlan = parsePrecogPlan({
      utterance: "look after my deadlines",
      intent: "conversation",
      agent: "darwin",
      mode: "clarify",
      tier: "heavy",
      tool: null,
      would_park: false,
      reversible: true,
      confidence: 0.5,
      why: "A real run would ask one clarifying question first (recurring vs. one-off) and act on nothing.",
      executed: false,
      satisfied_a_gate: false,
    });
    const html = renderPanel(clarifyPlan);
    expect(html).toContain("WOULD CLARIFY");
    expect(html).not.toContain("WOULD PARK");
    expect(html.toLowerCase()).toContain("act on nothing");
  });

  it("shows REVERSIBLE for a consequential-but-undoable action", () => {
    const homePlan = parsePrecogPlan({
      utterance: "turn on the living room lights",
      intent: "conversation",
      agent: "dume",
      mode: "one_shot",
      tier: "heavy",
      tool: "dume_control",
      would_park: true,
      reversible: true,
      confidence: 0.7,
      why: "reversible park",
      executed: false,
      satisfied_a_gate: false,
    });
    const html = renderPanel(homePlan);
    expect(html).toContain("WOULD PARK");
    expect(html).toContain("REVERSIBLE");
    expect(html).not.toContain("IRREVERSIBLE");
    expect(html).toContain("dume_control");
  });
});

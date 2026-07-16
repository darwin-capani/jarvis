import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import PlanDiffPanel from "../components/PlanDiffPanel";
import { parsePlanDiff, type TelemetryEnvelope } from "../core/events";
import {
  activePlanDiff,
  initialState,
  PLAN_DIFF_TTL_MS,
  reduce,
  type HudState,
} from "../core/state";

let counter = 0;
function env(event: string, data: Record<string, unknown>, source = "system"): TelemetryEnvelope {
  counter += 1;
  return { ts: `2026-07-13T00:00:${String(counter % 60).padStart(2, "0")}Z`, source, event, data };
}
function connected() {
  return reduce(initialState(), { type: "ws.connected", at: 0 });
}
function tel(state: HudState, e: TelemetryEnvelope, at = 1000) {
  return reduce(state, { type: "telemetry", envelope: e, at });
}

const wire = {
  tool: "connector_add",
  agent: "agent.pepper",
  summary: "Add MCP connector 'files' (http) — added inert; no capability granted",
  changes: [
    {
      resource: "config/darwin.toml [[mcp.servers]] 'files'",
      before: "(absent)",
      after: "https endpoint https://mcp.example.com/sse — INERT (agents=[], every tool gated)",
    },
  ],
  state_hash: "abc1234500000000",
  phase: "park",
  drift: false,
};

describe("parsePlanDiff (never fabricates a diff)", () => {
  it("parses a well-formed park-phase plan diff", () => {
    const p = parsePlanDiff(wire);
    expect(p).not.toBeNull();
    expect(p?.tool).toBe("connector_add");
    expect(p?.agent).toBe("agent.pepper");
    expect(p?.stateHash).toBe("abc1234500000000");
    expect(p?.phase).toBe("park");
    expect(p?.drift).toBe(false);
    expect(p?.changes).toHaveLength(1);
    expect(p?.changes[0].before).toBe("(absent)");
    expect(p?.changes[0].resource).toContain("files");
  });

  it("returns null with no tool or no well-formed change", () => {
    expect(parsePlanDiff({})).toBeNull();
    expect(parsePlanDiff({ tool: "connector_add", changes: [] })).toBeNull();
    expect(parsePlanDiff({ tool: "", changes: wire.changes })).toBeNull();
    // A change missing its resource is dropped; an all-invalid list -> null.
    expect(parsePlanDiff({ tool: "connector_add", changes: [{ before: "x", after: "y" }] })).toBeNull();
    expect(parsePlanDiff({ tool: "connector_add", changes: [42, "nope"] })).toBeNull();
  });

  it("narrows phase to the closed set and defaults drift", () => {
    const bogus = parsePlanDiff({ ...wire, phase: "banana", drift: "yes" });
    expect(bogus?.phase).toBe("park"); // unknown phase -> park
    expect(bogus?.drift).toBe(false); // non-boolean drift -> false
    const drifted = parsePlanDiff({ ...wire, phase: "confirm", drift: true });
    expect(drifted?.phase).toBe("confirm");
    expect(drifted?.drift).toBe(true);
  });

  it("caps and bounds the changes + fields — the wire is never trusted", () => {
    const big = parsePlanDiff({
      tool: "connector_add",
      changes: Array.from({ length: 20 }, () => ({
        resource: "r".repeat(5000),
        before: "b".repeat(5000),
        after: "a".repeat(5000),
      })),
      summary: "s".repeat(5000),
    });
    expect(big?.changes.length).toBeLessThanOrEqual(8);
    expect(big?.summary.length).toBeLessThanOrEqual(240);
    for (const c of big?.changes ?? []) {
      expect(c.resource.length).toBeLessThanOrEqual(240);
      expect(c.before.length).toBeLessThanOrEqual(240);
      expect(c.after.length).toBeLessThanOrEqual(240);
    }
  });
});

describe("plan.diff reducer lifecycle", () => {
  it("sets on plan.diff, clears on every resolution event", () => {
    for (const resolve of ["confirm.affirmed", "confirm.denied", "confirm.dropped_unrelated", "confirm.replayed"]) {
      let s = connected();
      s = tel(s, env("plan.diff", wire));
      expect(s.planDiff?.tool).toBe("connector_add");
      s = tel(s, env(resolve, { tool: "connector_add" }));
      expect(s.planDiff, `cleared on ${resolve}`).toBeNull();
    }
  });

  it("a new park supersedes a stale diff (single slot)", () => {
    let s = connected();
    s = tel(s, env("plan.diff", wire));
    // A different action parks -> the old diff goes stale even before a fresh one.
    s = tel(s, env("confirm.parked", { tool: "standing_create", agent: "agent.fury" }));
    expect(s.planDiff).toBeNull();
    // And the follow-up diff for the NEW pending lands normally.
    s = tel(s, env("plan.diff", {
      tool: "standing_create",
      agent: "agent.fury",
      summary: "Establish a standing mission: brief me",
      changes: [{ resource: "standing missions", before: "0 mission(s)", after: "+1 'brief me'" }],
      state_hash: "def0",
      phase: "park",
      drift: false,
    }));
    expect(s.planDiff?.tool).toBe("standing_create");
  });

  it("a park EMITTED BEFORE its own diff does not wipe that diff", () => {
    // The daemon emits confirm.parked FIRST, then plan.diff — verify that order
    // leaves the fresh diff shown (the supersede clears only a PRIOR diff).
    let s = connected();
    s = tel(s, env("plan.diff", wire)); // an earlier action's diff
    s = tel(s, env("confirm.parked", { tool: "standing_create", agent: "agent.fury" })); // clears it
    expect(s.planDiff).toBeNull();
    s = tel(s, env("plan.diff", { ...wire, tool: "standing_create" })); // the new one lands
    expect(s.planDiff?.tool).toBe("standing_create");
  });

  it("a drift re-park replaces the shown diff with the fresh one", () => {
    let s = connected();
    s = tel(s, env("plan.diff", wire));
    expect(s.planDiff?.drift).toBe(false);
    // Drift re-park: daemon emits confirm.parked (via=plan_drift) then a confirm-phase diff.
    s = tel(s, env("confirm.parked", { tool: "connector_add", agent: "agent.pepper", via: "plan_drift" }));
    s = tel(s, env("plan.diff", { ...wire, phase: "confirm", drift: true, state_hash: "newhash000" }));
    expect(s.planDiff?.drift).toBe(true);
    expect(s.planDiff?.phase).toBe("confirm");
    expect(s.planDiff?.stateHash).toBe("newhash000");
  });

  it("a link drop / daemon restart drops the orphaned diff", () => {
    let s = connected();
    s = tel(s, env("plan.diff", wire));
    s = reduce(s, { type: "ws.disconnected", at: 2000 });
    expect(s.planDiff).toBeNull();
  });

  it("drops an empty diff rather than clearing or fabricating", () => {
    let s = connected();
    s = tel(s, env("plan.diff", wire));
    s = tel(s, env("plan.diff", { tool: "x_post", changes: [] }));
    expect(s.planDiff?.tool).toBe("connector_add");
  });

  it("retires a stale diff whose pending died silently (client TTL)", () => {
    let s = connected();
    s = tel(s, env("plan.diff", wire), 10_000);
    expect(activePlanDiff(s, 10_500)?.tool).toBe("connector_add");
    expect(activePlanDiff(s, 10_000 + PLAN_DIFF_TTL_MS + 1)).toBeNull();
    // The underlying state is untouched (the selector is pure) until a real event.
    expect(s.planDiff?.tool).toBe("connector_add");
  });
});

describe("PlanDiffPanel", () => {
  const render = (plan: Parameters<typeof PlanDiffPanel>[0]["plan"]) =>
    renderToStaticMarkup(createElement(PlanDiffPanel, { plan }));

  it("renders nothing with no plan", () => {
    expect(render(null)).toBe("");
  });

  it("shows the summary + before/after with the advisory footnote", () => {
    const html = render(parsePlanDiff(wire));
    expect(html).toContain("PLAN // DIFF");
    expect(html).toContain("connector_add");
    expect(html).toContain("AWAITING YOUR CONFIRM");
    expect(html).toContain("(absent)");
    expect(html).toContain("INERT");
    // The standing honesty note: advisory only, gate unchanged.
    expect(html).toContain("the gate is unchanged");
    expect(html).toContain("Nothing runs without your");
  });

  it("flags a drift re-park loudly", () => {
    const drifted = parsePlanDiff({ ...wire, phase: "confirm", drift: true });
    const html = render(drifted);
    expect(html).toContain("STATE DRIFTED");
    expect(html).toContain("RE-PARKED");
    expect(html).toContain("The state changed since I showed you");
  });
});

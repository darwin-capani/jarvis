import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import SecondLookPanel from "../components/SecondLookPanel";
import { parseConsensusAdvisory, type TelemetryEnvelope } from "../core/events";
import {
  activeConsensusAdvisory,
  CONSENSUS_ADVISORY_TTL_MS,
  initialState,
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
  tool: "gmail_send",
  agent: "agent.pepper",
  notes: ["once done, I can't undo this — sent mail can't be unsent"],
};

describe("parseConsensusAdvisory (never fabricates an advisory)", () => {
  it("parses a well-formed advisory", () => {
    expect(parseConsensusAdvisory(wire)).toEqual(wire);
  });

  it("returns null with no tool or no notes", () => {
    expect(parseConsensusAdvisory({})).toBeNull();
    expect(parseConsensusAdvisory({ tool: "gmail_send", notes: [] })).toBeNull();
    expect(parseConsensusAdvisory({ tool: "", notes: ["n"] })).toBeNull();
    expect(parseConsensusAdvisory({ tool: "gmail_send", notes: [42, ""] })).toBeNull();
  });

  it("caps and bounds notes — the wire is never trusted", () => {
    const run = parseConsensusAdvisory({
      tool: "gmail_send",
      notes: Array.from({ length: 10 }, () => "n".repeat(5000)),
    });
    expect(run?.notes).toHaveLength(4);
    for (const n of run?.notes ?? []) expect(n.length).toBeLessThanOrEqual(240);
  });
});

describe("consensus.advisory reducer lifecycle", () => {
  it("sets on advisory, clears on every resolution event", () => {
    for (const resolve of ["confirm.affirmed", "confirm.denied", "confirm.dropped_unrelated", "confirm.replayed"]) {
      let s = connected();
      s = tel(s, env("consensus.advisory", wire));
      expect(s.consensusAdvisory?.tool).toBe("gmail_send");
      // Resolution clears — the advisory described a pending that is gone.
      // confirm.replayed covers the HUD/command-channel confirm that actually
      // executes the action.
      s = tel(s, env(resolve, { tool: "gmail_send" }));
      expect(s.consensusAdvisory, `cleared on ${resolve}`).toBeNull();
    }
  });

  it("a new park supersedes a stale advisory", () => {
    let s = connected();
    s = tel(s, env("consensus.advisory", wire));
    // A different action parks (single slot) — the old advisory goes stale
    // even before (or without) a fresh advisory event.
    s = tel(s, env("confirm.parked", { tool: "slack_post_message", agent: "agent.a" }));
    expect(s.consensusAdvisory).toBeNull();
    // And the follow-up advisory for the NEW pending lands normally.
    s = tel(s, env("consensus.advisory", { tool: "slack_post_message", notes: ["'#new-chan' isn't in my recent record"] }));
    expect(s.consensusAdvisory?.tool).toBe("slack_post_message");
  });

  it("a link drop / daemon restart drops the orphaned advisory", () => {
    let s = connected();
    s = tel(s, env("consensus.advisory", wire));
    s = reduce(s, { type: "ws.disconnected", at: 2000 });
    expect(s.consensusAdvisory).toBeNull();
  });

  it("drops an empty advisory rather than clearing or fabricating", () => {
    let s = connected();
    s = tel(s, env("consensus.advisory", wire));
    s = tel(s, env("consensus.advisory", { tool: "x_post", notes: [] }));
    expect(s.consensusAdvisory?.tool).toBe("gmail_send");
  });

  it("retires a stale advisory whose pending died silently (client TTL)", () => {
    // Silent resolutions — barge-in, lockdown, TTL expiry, command-channel
    // deny — emit no clearing event; the client TTL is the catch-all.
    let s = connected();
    s = tel(s, env("consensus.advisory", wire), 10_000);
    // Fresh: still shown.
    expect(activeConsensusAdvisory(s, 10_500)?.tool).toBe("gmail_send");
    // Past the pending's TTL: retired even with no clearing event.
    expect(activeConsensusAdvisory(s, 10_000 + CONSENSUS_ADVISORY_TTL_MS + 1)).toBeNull();
    // The underlying state is untouched (the selector is pure) until a real
    // event clears it — the panel simply stops showing it.
    expect(s.consensusAdvisory?.tool).toBe("gmail_send");
  });
});

describe("SecondLookPanel", () => {
  const render = (advisory: Parameters<typeof SecondLookPanel>[0]["advisory"]) =>
    renderToStaticMarkup(createElement(SecondLookPanel, { advisory }));

  it("renders nothing with no pending advisory", () => {
    expect(render(null)).toBe("");
  });

  it("shows the notes verbatim with the advisory-only footnote", () => {
    const html = render(wire);
    expect(html).toContain("SECOND LOOK // PRE-CONFIRM");
    expect(html).toContain("gmail_send");
    expect(html).toContain("sent mail can&#x27;t be unsent");
    expect(html).toContain("AWAITING YOUR CONFIRM");
    // The standing honesty note: advisory only, gate unchanged.
    expect(html).toContain("the gate is unchanged");
    expect(html).toContain("Nothing runs without your");
  });
});

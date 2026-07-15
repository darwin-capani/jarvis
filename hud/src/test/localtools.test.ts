import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import StatusBar from "../components/StatusBar";
import {
  LOCAL_TOOLS_TRACE_MAX,
  applyLocalToolsEngaged,
  applyLocalToolsExecuted,
  applyLocalToolsOutOfSubset,
  localToolsHonest,
  localToolsInitial,
  localToolsLabel,
  localToolsTone,
  modelTierInitial,
  sttTierInitial,
  voiceIdInitial,
  voiceTierInitial,
  voiceModeInitial,
  type LocalToolsStatus,
  type ModelTierStatus,
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
    ts: `2026-06-16T12:00:${String(counter % 60).padStart(2, "0")}Z`,
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

/* ----------------------------------------------------------------- folding */

describe("offline tool-loop folding helpers (events.ts)", () => {
  it("seeds the honest CHATTING resting state (not engaged, nothing gated)", () => {
    expect(localToolsInitial()).toEqual({
      engaged: false,
      toolsUsed: 0,
      tools: [],
      gated: false,
      intent: null,
      refusedOutOfSubset: false,
      recent: [],
    });
  });

  it("local_tools.engaged folds the per-turn ACTING-OFFLINE verdict", () => {
    const lt = applyLocalToolsEngaged(localToolsInitial(), {
      tools_used: 2,
      tools: ["recall_facts", "doc_search"],
      gated: false,
      intent: "recall",
    });
    expect(lt.engaged).toBe(true);
    expect(lt.toolsUsed).toBe(2);
    expect(lt.tools).toEqual(["recall_facts", "doc_search"]);
    expect(lt.gated).toBe(false);
    expect(lt.intent).toBe("recall");
  });

  it("derives toolsUsed from the tools array length when the count is absent", () => {
    const lt = applyLocalToolsEngaged(localToolsInitial(), {
      tools: ["recall_facts"],
    });
    expect(lt.toolsUsed).toBe(1);
  });

  it("gated defaults to false (an unknown gate posture is NOT a fake gate fire)", () => {
    const lt = applyLocalToolsEngaged(localToolsInitial(), { tools_used: 1, tools: ["x"] });
    expect(lt.gated).toBe(false);
  });

  it("an engaged turn carries gated=true (a safety gate fired offline)", () => {
    const lt = applyLocalToolsEngaged(localToolsInitial(), {
      tools_used: 1,
      tools: ["remember_fact"],
      gated: true,
      intent: "store",
    });
    expect(lt.engaged).toBe(true);
    expect(lt.gated).toBe(true);
  });

  it("drops non-string entries from the tools array, never throwing", () => {
    const lt = applyLocalToolsEngaged(localToolsInitial(), {
      tools_used: 2,
      tools: ["recall_facts", 42, null, "doc_search"],
    });
    expect(lt.tools).toEqual(["recall_facts", "doc_search"]);
  });

  it("local_tools.executed pushes onto the bounded activity trace (newest last)", () => {
    let lt = applyLocalToolsEngaged(localToolsInitial(), { tools_used: 0, tools: [] });
    lt = applyLocalToolsExecuted(lt, {
      tool: "recall_facts",
      agent: "darwin",
      is_error: false,
      outcome: "found 3 facts",
    });
    expect(lt.recent).toHaveLength(1);
    expect(lt.recent[0]).toEqual({
      tool: "recall_facts",
      agent: "darwin",
      isError: false,
      outcome: "found 3 facts",
      outOfSubset: false,
    });
  });

  it("executed with no tool name is a no-op (nothing to trace)", () => {
    const seeded = applyLocalToolsEngaged(localToolsInitial(), { tools_used: 0, tools: [] });
    const lt = applyLocalToolsExecuted(seeded, { agent: "darwin", is_error: false });
    expect(lt).toBe(seeded);
  });

  it("the activity trace ring is bounded to LOCAL_TOOLS_TRACE_MAX", () => {
    let lt = localToolsInitial();
    for (let i = 0; i < LOCAL_TOOLS_TRACE_MAX + 5; i += 1) {
      lt = applyLocalToolsExecuted(lt, { tool: `tool_${i}`, is_error: false });
    }
    expect(lt.recent).toHaveLength(LOCAL_TOOLS_TRACE_MAX);
    // newest last: the final pushed tool is retained, the oldest dropped
    expect(lt.recent[lt.recent.length - 1].tool).toBe(
      `tool_${LOCAL_TOOLS_TRACE_MAX + 4}`,
    );
    expect(lt.recent[0].tool).toBe("tool_5");
  });

  it("local_tools.out_of_subset raises refusedOutOfSubset and traces the refusal", () => {
    const lt = applyLocalToolsOutOfSubset(localToolsInitial(), {
      tool: "send_email",
      agent: "darwin",
    });
    expect(lt.refusedOutOfSubset).toBe(true);
    const last = lt.recent[lt.recent.length - 1];
    expect(last.tool).toBe("send_email");
    expect(last.outOfSubset).toBe(true);
    expect(last.isError).toBe(true);
  });

  it("a fresh engaged turn clears a prior out-of-subset refusal flag", () => {
    let lt = applyLocalToolsOutOfSubset(localToolsInitial(), { tool: "send_email" });
    expect(lt.refusedOutOfSubset).toBe(true);
    lt = applyLocalToolsEngaged(lt, { tools_used: 1, tools: ["recall_facts"] });
    expect(lt.refusedOutOfSubset).toBe(false);
  });

  it("never throws on a malformed payload and never surfaces a stray field", () => {
    const lt = applyLocalToolsEngaged(localToolsInitial(), {
      tools_used: "lots",
      tools: "not-an-array",
      gated: "yes",
      secret: "leak",
    } as unknown as Record<string, unknown>);
    expect(Object.keys(lt)).not.toContain("secret");
    expect(lt.toolsUsed).toBe(0); // no count, no array length -> 0
    expect(lt.gated).toBe(false);
  });
});

/* ---------------------------------------------------------- derivation */

describe("offline tool-loop derivation (pure)", () => {
  it("labels ACTING OFFLINE when engaged, CHATTING when resting", () => {
    expect(localToolsLabel(localToolsInitial())).toBe("CHATTING");
    const engaged = applyLocalToolsEngaged(localToolsInitial(), {
      tools_used: 1,
      tools: ["recall_facts"],
    });
    expect(localToolsLabel(engaged)).toBe("ACTING OFFLINE");
  });

  it("tone: chatting=idle, acting clean=ok, gated/refused=warn", () => {
    expect(localToolsTone(localToolsInitial())).toBe("idle");
    const clean = applyLocalToolsEngaged(localToolsInitial(), {
      tools_used: 1,
      tools: ["recall_facts"],
      gated: false,
    });
    expect(localToolsTone(clean)).toBe("ok");
    const gated = applyLocalToolsEngaged(localToolsInitial(), {
      tools_used: 1,
      tools: ["remember_fact"],
      gated: true,
    });
    expect(localToolsTone(gated)).toBe("warn");
    const refused = applyLocalToolsOutOfSubset(
      applyLocalToolsEngaged(localToolsInitial(), { tools_used: 1, tools: ["recall_facts"] }),
      { tool: "send_email" },
    );
    expect(localToolsTone(refused)).toBe("warn");
  });

  it("the honest copy states on-device, less-reliable, and same-gates-apply", () => {
    const lt = applyLocalToolsEngaged(localToolsInitial(), {
      tools_used: 1,
      tools: ["recall_facts"],
      gated: false,
    });
    const copy = localToolsHonest(lt).toLowerCase();
    expect(copy).toContain("on-device");
    expect(copy).toContain("less");
    expect(copy).toContain("reliable");
    expect(copy).toContain("bounded");
    // names every gate that still applies offline
    expect(copy).toContain("confirmation");
    expect(copy).toContain("voice-id");
    expect(copy).toContain("lockdown");
    expect(copy).toContain("policy");
    // never claims it bypasses a gate
    expect(copy).toMatch(/bypass(es)? nothing|never bypass|does not bypass/);
  });

  it("the honest copy names the SAFE LOCAL subset and excludes outward/cloud tools", () => {
    const lt = applyLocalToolsEngaged(localToolsInitial(), {
      tools_used: 1,
      tools: ["recall_facts"],
    });
    const copy = localToolsHonest(lt).toLowerCase();
    expect(copy).toContain("local");
    expect(copy).toContain("read/compute");
    expect(copy).toMatch(/no outward|never outward|not.{0,12}outward/);
  });

  it("the honest copy flags a gate that fired (the proof the gates hold offline)", () => {
    const gated = applyLocalToolsEngaged(localToolsInitial(), {
      tools_used: 1,
      tools: ["remember_fact"],
      gated: true,
    });
    expect(localToolsHonest(gated).toLowerCase()).toMatch(/gate fired|gate.{0,12}fired/);
  });

  it("the honest copy flags a refused out-of-subset tool", () => {
    const refused = applyLocalToolsOutOfSubset(
      applyLocalToolsEngaged(localToolsInitial(), { tools_used: 1, tools: ["recall_facts"] }),
      { tool: "send_email" },
    );
    expect(localToolsHonest(refused).toLowerCase()).toContain("outside the safe subset");
  });

  it("the resting copy names CHATTING and does NOT claim a tool ran", () => {
    const copy = localToolsHonest(localToolsInitial()).toLowerCase();
    expect(copy).toContain("chatting");
    expect(copy).toContain("no local tools ran");
  });

  it("never frames the on-device 4B as as-good-as the cloud model", () => {
    const lt = applyLocalToolsEngaged(localToolsInitial(), {
      tools_used: 1,
      tools: ["recall_facts"],
    });
    const copy = localToolsHonest(lt).toLowerCase();
    expect(copy).not.toMatch(/as good as|as reliable as|equals|same as/);
  });
});

/* --------------------------------------------------------------- reducer */

describe("reducer local_tools.* events", () => {
  it("seeds localTools as the honest CHATTING resting state", () => {
    const lt = initialState().localTools;
    expect(localToolsLabel(lt)).toBe("CHATTING");
    expect(lt.engaged).toBe(false);
  });

  it("threads a local_tools.engaged verdict into state (ACTING OFFLINE)", () => {
    const s = tel(
      connected(),
      env("local_tools.engaged", {
        tools_used: 2,
        tools: ["recall_facts", "doc_search"],
        gated: false,
        intent: "recall",
      }),
    );
    expect(s.localTools.engaged).toBe(true);
    expect(s.localTools.toolsUsed).toBe(2);
    expect(s.localTools.tools).toEqual(["recall_facts", "doc_search"]);
  });

  it("a gated engaged turn surfaces gated=true (a safety gate fired offline)", () => {
    const s = tel(
      connected(),
      env("local_tools.engaged", {
        tools_used: 1,
        tools: ["remember_fact"],
        gated: true,
        intent: "store",
      }),
    );
    expect(s.localTools.gated).toBe(true);
  });

  it("threads per-tool local_tools.executed activity into the trace", () => {
    let s = tel(connected(), env("local_tools.engaged", { tools_used: 0, tools: [] }));
    s = tel(
      s,
      env("local_tools.executed", {
        tool: "doc_search",
        agent: "darwin",
        is_error: false,
        outcome: "3 hits",
      }),
    );
    expect(s.localTools.recent).toHaveLength(1);
    expect(s.localTools.recent[0].tool).toBe("doc_search");
  });

  it("threads local_tools.out_of_subset (a refused hallucinated tool)", () => {
    const s = tel(
      connected(),
      env("local_tools.out_of_subset", { tool: "open_url", agent: "darwin" }),
    );
    expect(s.localTools.refusedOutOfSubset).toBe(true);
    expect(s.localTools.recent[0].outOfSubset).toBe(true);
  });

  it("a malformed local_tools.engaged never throws and never blanks the indicator", () => {
    let s = tel(
      connected(),
      env("local_tools.engaged", { tools_used: 1, tools: ["recall_facts"] }),
    );
    s = tel(s, env("local_tools.engaged", { tools_used: [], tools: 99 }));
    // engaged stays true; tools coerces to [] but does not crash
    expect(s.localTools.engaged).toBe(true);
  });
});

/* ------------------------------------------------------------ render: chip */

const noop = () => {};

function renderStatusBar(
  localTools: LocalToolsStatus | null,
  modelTier: ModelTierStatus = modelTierInitial(),
): string {
  return renderToStaticMarkup(
    createElement(StatusBar, {
      connected: true,
      coreState: "idle" as const,
      cloudKeyPresent: false,
      inferenceOffline: false,
      heal: null,
      cloudModel: null,
      activeAgent: null,
      voiceId: voiceIdInitial(),
      modelTier,
      localTools,
      voiceTier: voiceTierInitial(),
      sttTier: sttTierInitial(),
      voiceMode: voiceModeInitial(),
      onOpenSettings: noop,
      onOpenDeck: noop,
    }),
  );
}

describe("StatusBar offline-agency chip", () => {
  it("renders NOTHING in the resting CHATTING state (bar stays uncluttered)", () => {
    const html = renderStatusBar(localToolsInitial());
    expect(html).not.toContain("ACTING OFFLINE");
    expect(html).not.toContain("localtools-chip");
  });

  it("renders NOTHING when localTools is absent (backward-compatible)", () => {
    const html = renderStatusBar(null);
    expect(html).not.toContain("localtools-chip");
  });

  it("renders ACTING OFFLINE with the tool count when the loop engaged", () => {
    const lt = applyLocalToolsEngaged(localToolsInitial(), {
      tools_used: 2,
      tools: ["recall_facts", "doc_search"],
      gated: false,
    });
    const html = renderStatusBar(lt);
    expect(html).toContain("ACTING OFFLINE");
    expect(html).toContain("localtools-chip");
    expect(html).toContain("ok"); // clean tone
    expect(html).toContain("2");
  });

  it("renders the honest hover copy (on-device, less reliable, same gates apply)", () => {
    const lt = applyLocalToolsEngaged(localToolsInitial(), {
      tools_used: 1,
      tools: ["recall_facts"],
    });
    const html = renderStatusBar(lt).toLowerCase();
    expect(html).toContain("on-device");
    expect(html).toContain("less");
    expect(html).toContain("reliable");
    expect(html).toContain("confirmation");
    expect(html).toContain("voice-id");
    expect(html).toContain("lockdown");
    expect(html).toContain("policy");
  });

  it("flags GATED inline in the warn tone when a safety gate fired offline", () => {
    const lt = applyLocalToolsEngaged(localToolsInitial(), {
      tools_used: 1,
      tools: ["remember_fact"],
      gated: true,
    });
    const html = renderStatusBar(lt);
    expect(html).toContain("ACTING OFFLINE");
    expect(html).toContain("GATED");
    expect(html).toContain("warn");
  });

  it("flags REFUSED inline when the 4B reached outside the safe subset", () => {
    const lt = applyLocalToolsOutOfSubset(
      applyLocalToolsEngaged(localToolsInitial(), { tools_used: 1, tools: ["recall_facts"] }),
      { tool: "send_email" },
    );
    const html = renderStatusBar(lt);
    expect(html).toContain("REFUSED");
    expect(html).toContain("warn");
  });

  it("never claims the on-device path equals the cloud model's quality", () => {
    const lt = applyLocalToolsEngaged(localToolsInitial(), {
      tools_used: 1,
      tools: ["recall_facts"],
    });
    const html = renderStatusBar(lt).toLowerCase();
    expect(html).not.toMatch(/as good as|as reliable as the cloud|same as opus/);
  });
});

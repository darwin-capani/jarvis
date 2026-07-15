import { describe, expect, it } from "vitest";
import {
  parseEpisodicRecorded,
  parseMemoryRetention,
  parseUserModelConsolidated,
  type TelemetryEnvelope,
} from "../core/events";
import {
  EPISODE_TIMELINE_CAP,
  initialState,
  reduce,
  type HudState,
} from "../core/state";

/* ------------------------------------------------------------------------ *
 * MEMORY surface — the episodic store (Core-A) + user model (Core-B) HUD     *
 * view, fed by ACTIVITY telemetry ONLY. The daemon emits episodic.recorded   *
 * (per turn), user_model.consolidated[_failed], and memory.retention — never *
 * the episode bodies or profile entries. These tests pin the parsers + the   *
 * reducer arms, and the HONESTY invariants: observed-not-clairvoyant (gated  *
 * turns are surfaced as "not kept", never dropped or faked), bounded         *
 * (timeline capped; eviction proof carried), and that nothing fabricates a   *
 * memory the daemon did not report.                                          *
 * ------------------------------------------------------------------------ */

let counter = 0;
function env(event: string, data: Record<string, unknown> = {}): TelemetryEnvelope {
  counter += 1;
  return {
    ts: `2026-06-16T09:00:${String(counter % 60).padStart(2, "0")}Z`,
    source: "system",
    event,
    data,
  };
}

function tel(state: HudState, e: TelemetryEnvelope, at = 1000): HudState {
  return reduce(state, { type: "telemetry", envelope: e, at });
}

/* parsers ------------------------------------------------------------------ */

describe("parseEpisodicRecorded (episodic.recorded)", () => {
  it("parses a recorded turn with its agent scope", () => {
    expect(parseEpisodicRecorded({ recorded: true, agent: "agent.darwin" })).toEqual({
      recorded: true,
      agent: "agent.darwin",
    });
  });

  it("parses a gated-out turn (recorded=false) honestly", () => {
    expect(parseEpisodicRecorded({ recorded: false, agent: "agent.mnemosyne" })).toEqual({
      recorded: false,
      agent: "agent.mnemosyne",
    });
  });

  it("defaults a missing agent to the shared scope ''", () => {
    expect(parseEpisodicRecorded({ recorded: true })).toEqual({ recorded: true, agent: "" });
  });

  it("returns null without a real `recorded` boolean (no fabricated outcome)", () => {
    expect(parseEpisodicRecorded({ agent: "agent.darwin" })).toBeNull();
    expect(parseEpisodicRecorded({ recorded: "yes" })).toBeNull();
    expect(parseEpisodicRecorded({})).toBeNull();
  });

  it("never carries content — only the bit + agent are on the parsed shape", () => {
    const r = parseEpisodicRecorded({
      recorded: true,
      agent: "agent.darwin",
      utterance: "secret thing",
      summary: "should never appear",
    });
    expect(r).toEqual({ recorded: true, agent: "agent.darwin" });
    expect(Object.keys(r ?? {})).toEqual(["recorded", "agent"]);
  });
});

describe("parseUserModelConsolidated (user_model.consolidated)", () => {
  it("parses the entry-written count", () => {
    expect(parseUserModelConsolidated({ entries_written: 4 })).toEqual({ entriesWritten: 4 });
  });

  it("accepts a zero-entry pass (observed nothing new this round)", () => {
    expect(parseUserModelConsolidated({ entries_written: 0 })).toEqual({ entriesWritten: 0 });
  });

  it("returns null without a finite count", () => {
    expect(parseUserModelConsolidated({})).toBeNull();
    expect(parseUserModelConsolidated({ entries_written: "lots" })).toBeNull();
  });
});

describe("parseMemoryRetention (memory.retention)", () => {
  it("parses all three eviction counters", () => {
    expect(
      parseMemoryRetention({
        events_deleted: 10,
        transcripts_deleted: 2,
        episodes_deleted: 5,
      }),
    ).toEqual({ eventsDeleted: 10, transcriptsDeleted: 2, episodesDeleted: 5 });
  });

  it("defaults absent counters to 0 when at least one is present", () => {
    expect(parseMemoryRetention({ episodes_deleted: 3 })).toEqual({
      eventsDeleted: 0,
      transcriptsDeleted: 0,
      episodesDeleted: 3,
    });
  });

  it("returns null when no counter is present (not a real pass)", () => {
    expect(parseMemoryRetention({})).toBeNull();
    expect(parseMemoryRetention({ foo: 1 })).toBeNull();
  });
});

/* reducer ------------------------------------------------------------------ */

describe("memory reducer — episodic timeline", () => {
  it("starts empty + observed-only", () => {
    const m = initialState().memory;
    expect(m.timeline).toEqual([]);
    expect(m.recordedCount).toBe(0);
    expect(m.gatedCount).toBe(0);
    expect(m.userModelEntries).toBeNull();
    expect(m.lastEvictedEpisodes).toBeNull();
  });

  it("prepends recorded turns newest-first with agent + ts", () => {
    let s = initialState();
    s = tel(s, env("episodic.recorded", { recorded: true, agent: "agent.darwin" }));
    s = tel(s, env("episodic.recorded", { recorded: true, agent: "agent.edith" }));
    expect(s.memory.timeline).toHaveLength(2);
    expect(s.memory.timeline[0].agent).toBe("agent.edith"); // newest first
    expect(s.memory.timeline[1].agent).toBe("agent.darwin");
    expect(s.memory.timeline[0].recorded).toBe(true);
    expect(s.memory.recordedCount).toBe(2);
    expect(s.memory.gatedCount).toBe(0);
  });

  it("surfaces a gated-out turn honestly (kept vs gated counts)", () => {
    let s = initialState();
    s = tel(s, env("episodic.recorded", { recorded: false, agent: "agent.darwin" }));
    s = tel(s, env("episodic.recorded", { recorded: true, agent: "agent.darwin" }));
    expect(s.memory.recordedCount).toBe(1);
    expect(s.memory.gatedCount).toBe(1);
    expect(s.memory.timeline[1].recorded).toBe(false); // the gated turn is still shown
  });

  it("bounds the timeline ring to the cap (oldest evicted from view)", () => {
    let s = initialState();
    for (let i = 0; i < EPISODE_TIMELINE_CAP + 8; i++) {
      s = tel(s, env("episodic.recorded", { recorded: true, agent: `agent.a${i}` }));
    }
    expect(s.memory.timeline).toHaveLength(EPISODE_TIMELINE_CAP);
    // The most recent is at the head; counts still reflect ALL observed turns.
    expect(s.memory.timeline[0].agent).toBe(`agent.a${EPISODE_TIMELINE_CAP + 7}`);
    expect(s.memory.recordedCount).toBe(EPISODE_TIMELINE_CAP + 8);
  });

  it("drops a malformed episodic.recorded without churning state", () => {
    const s = initialState();
    const next = tel(s, env("episodic.recorded", { agent: "agent.darwin" }));
    expect(next).toBe(s); // same reference — no fabricated row
  });
});

describe("memory reducer — user model + retention", () => {
  it("records the user-model consolidation count + clears stale", () => {
    let s = initialState();
    s = tel(s, env("user_model.consolidation_failed", { error: "db busy" }));
    expect(s.memory.userModelStale).toBe(true);
    s = tel(s, env("user_model.consolidated", { entries_written: 3 }));
    expect(s.memory.userModelEntries).toBe(3);
    expect(s.memory.userModelStale).toBe(false);
    expect(s.memory.userModelConsolidatedAt).not.toBeNull();
  });

  it("flags a failed pass as stale while keeping the prior count visible", () => {
    let s = initialState();
    s = tel(s, env("user_model.consolidated", { entries_written: 5 }));
    s = tel(s, env("user_model.consolidation_failed", { error: "locked" }));
    expect(s.memory.userModelEntries).toBe(5); // prior count preserved
    expect(s.memory.userModelStale).toBe(true);
  });

  it("carries the episode-eviction proof from a retention pass", () => {
    let s = initialState();
    s = tel(
      s,
      env("memory.retention", {
        events_deleted: 12,
        transcripts_deleted: 1,
        episodes_deleted: 7,
      }),
    );
    expect(s.memory.lastEvictedEpisodes).toBe(7);
    expect(s.memory.lastRetentionAt).not.toBeNull();
  });

  it("does not let a malformed user_model / retention event churn state", () => {
    const s = initialState();
    expect(tel(s, env("user_model.consolidated", { entries_written: "x" }))).toBe(s);
    expect(tel(s, env("memory.retention", {}))).toBe(s);
  });
});

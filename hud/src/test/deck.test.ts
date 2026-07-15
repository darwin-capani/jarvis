import { describe, expect, it } from "vitest";
import {
  AUTO_ROUTE,
  DECK_LOG_CAP,
  agentForAsk,
  deckReduce,
  forgeApplyCommand,
  hasPending,
  initialDeckState,
  parseForgePendingTs,
  parsePendingConfirmation,
  parsePendingSnapshot,
  replyToActions,
  type DeckState,
} from "../core/deck";
import type { CommandReply } from "../tauri/command";

/* ------------------------------------------------------------------------ *
 * Agent targeting — auto-route vs. addressing a specific agent.             *
 * ------------------------------------------------------------------------ */
describe("agentForAsk", () => {
  it("returns undefined for the auto-route sentinel (omit agent -> orchestrator)", () => {
    expect(agentForAsk(AUTO_ROUTE)).toBeUndefined();
    expect(agentForAsk("")).toBeUndefined();
    expect(agentForAsk("   ")).toBeUndefined();
  });
  it("returns the trimmed agent name when one is addressed", () => {
    expect(agentForAsk("edith")).toBe("edith");
    expect(agentForAsk("  fury  ")).toBe("fury");
  });
});

/* ------------------------------------------------------------------------ *
 * The deck reducer — log ring + tray state, PURE (no DOM).                  *
 * ------------------------------------------------------------------------ */
describe("deckReduce", () => {
  it("starts empty with no pending", () => {
    const s = initialDeckState();
    expect(s.log).toEqual([]);
    expect(s.busy).toBe(false);
    expect(hasPending(s.pending)).toBe(false);
  });

  it("appends command / reply / error / system lines with monotonic ids", () => {
    let s = initialDeckState();
    s = deckReduce(s, { type: "command", agent: "edith", text: "status" });
    s = deckReduce(s, { type: "reply", agent: "edith", text: "All nominal." });
    s = deckReduce(s, { type: "error", text: "rate_limited" });
    s = deckReduce(s, { type: "system", text: "working" });
    expect(s.log.map((l) => l.kind)).toEqual(["command", "reply", "error", "system"]);
    expect(s.log.map((l) => l.id)).toEqual([1, 2, 3, 4]);
    expect(s.log[1].agent).toBe("edith");
    // error/system lines are not attributed to an agent.
    expect(s.log[2].agent).toBeNull();
  });

  it("caps the log ring at DECK_LOG_CAP (drops oldest)", () => {
    let s = initialDeckState();
    for (let i = 0; i < DECK_LOG_CAP + 25; i++) {
      s = deckReduce(s, { type: "reply", agent: null, text: `line ${i}` });
    }
    expect(s.log.length).toBe(DECK_LOG_CAP);
    // The newest line survives; the very first was dropped.
    expect(s.log[s.log.length - 1].text).toBe(`line ${DECK_LOG_CAP + 24}`);
    expect(s.log[0].text).toBe(`line 25`);
  });

  it("busy toggles the in-flight flag", () => {
    let s = initialDeckState();
    s = deckReduce(s, { type: "busy", busy: true });
    expect(s.busy).toBe(true);
    s = deckReduce(s, { type: "busy", busy: false });
    expect(s.busy).toBe(false);
  });

  it("pending replaces the snapshot; clearConfirmation/clearForge clear in place", () => {
    let s: DeckState = initialDeckState();
    s = deckReduce(s, {
      type: "pending",
      snapshot: {
        confirmation: { id: "abc", agent: "agent.pepper", tool: "gmail_send", preview: "Would email" },
        forge_pending_ts: "1770000000",
      },
    });
    expect(hasPending(s.pending)).toBe(true);
    s = deckReduce(s, { type: "clearConfirmation" });
    expect(s.pending.confirmation).toBeNull();
    // The forge marker survives a confirmation clear.
    expect(s.pending.forge_pending_ts).toBe("1770000000");
    s = deckReduce(s, { type: "clearForge" });
    expect(s.pending.forge_pending_ts).toBeNull();
    expect(hasPending(s.pending)).toBe(false);
  });
});

/* ------------------------------------------------------------------------ *
 * Defensive pending-snapshot parsing — junk yields an EMPTY tray, no throw. *
 * ------------------------------------------------------------------------ */
describe("parsePendingConfirmation (defensive)", () => {
  it("parses a well-formed confirmation", () => {
    expect(
      parsePendingConfirmation({ id: "deadbeef", agent: "agent.pepper", tool: "gmail_send", preview: "Would email Alice" }),
    ).toEqual({ id: "deadbeef", agent: "agent.pepper", tool: "gmail_send", preview: "Would email Alice" });
  });
  it("returns null without a usable id (nothing to confirm/deny)", () => {
    expect(parsePendingConfirmation({ tool: "gmail_send" })).toBeNull();
    expect(parsePendingConfirmation({ id: "" })).toBeNull();
    expect(parsePendingConfirmation({ id: 42 })).toBeNull();
    expect(parsePendingConfirmation(null)).toBeNull();
    expect(parsePendingConfirmation("nope")).toBeNull();
  });
  it("defaults agent/tool/preview to empty for a partial-but-identified action", () => {
    expect(parsePendingConfirmation({ id: "x" })).toEqual({
      id: "x",
      agent: "",
      tool: "",
      preview: "",
    });
  });
  it("never throws on junk", () => {
    expect(() => parsePendingConfirmation({})).not.toThrow();
    expect(() => parsePendingConfirmation(undefined)).not.toThrow();
  });
});

describe("parseForgePendingTs (defensive)", () => {
  it("accepts a non-empty string or a finite number, coercing to string", () => {
    expect(parseForgePendingTs("1770000000")).toBe("1770000000");
    expect(parseForgePendingTs(1770000000)).toBe("1770000000");
  });
  it("rejects empty / non-finite / wrong type", () => {
    expect(parseForgePendingTs("")).toBeNull();
    expect(parseForgePendingTs(Number.NaN)).toBeNull();
    expect(parseForgePendingTs(null)).toBeNull();
    expect(parseForgePendingTs({})).toBeNull();
  });
});

describe("parsePendingSnapshot (defensive)", () => {
  it("narrows a full snapshot", () => {
    const s = parsePendingSnapshot({
      confirmation: { id: "abc", agent: "a", tool: "t", preview: "p" },
      forge_pending_ts: 99,
    });
    expect(s.confirmation?.id).toBe("abc");
    expect(s.forge_pending_ts).toBe("99");
  });
  it("yields an empty tray for junk (never throws, never fabricates a card)", () => {
    for (const junk of [null, undefined, "x", 42, [], { confirmation: "bad", forge_pending_ts: {} }]) {
      const s = parsePendingSnapshot(junk);
      expect(s.confirmation).toBeNull();
      expect(s.forge_pending_ts).toBeNull();
      expect(hasPending(s)).toBe(false);
    }
  });
});

/* ------------------------------------------------------------------------ *
 * The forge manual command — review-only, no apply button anywhere.        *
 * ------------------------------------------------------------------------ */
describe("forgeApplyCommand", () => {
  it("is the exact manual deploy command (the only install route)", () => {
    expect(forgeApplyCommand("1770000000")).toBe("scripts/apply_forge.sh 1770000000");
  });
});

/* ------------------------------------------------------------------------ *
 * Reply -> deck-action mapping.                                             *
 * ------------------------------------------------------------------------ */
describe("replyToActions", () => {
  it("maps an ok prose reply to a single attributed reply line", () => {
    const reply: CommandReply = { ok: true, reply: "Roll call complete." };
    const actions = replyToActions(reply, { expectPending: false, replyAgent: "darwin" });
    expect(actions).toEqual([{ type: "reply", agent: "darwin", text: "Roll call complete." }]);
  });

  it("maps a failed reply to a single error line carrying the daemon's reason", () => {
    const reply: CommandReply = { ok: false, error: "rate_limited" };
    expect(replyToActions(reply, { expectPending: false, replyAgent: null })).toEqual([
      { type: "error", text: "rate_limited" },
    ]);
  });

  it("falls back to 'command failed' when a failed reply has no error string", () => {
    expect(replyToActions({ ok: false } as CommandReply, { expectPending: false, replyAgent: null })).toEqual([
      { type: "error", text: "command failed" },
    ]);
  });

  it("routes a pending snapshot into the tray when expected (no prose spam)", () => {
    const reply: CommandReply = {
      ok: true,
      pending: { confirmation: { id: "x", agent: "a", tool: "t", preview: "p" }, forge_pending_ts: null },
    };
    const actions = replyToActions(reply, { expectPending: true, replyAgent: null });
    expect(actions).toHaveLength(1);
    expect(actions[0].type).toBe("pending");
  });

  it("treats a null/garbled reply as a clean error, never a throw", () => {
    expect(() =>
      replyToActions(null as unknown as CommandReply, { expectPending: false, replyAgent: null }),
    ).not.toThrow();
    expect(
      replyToActions(null as unknown as CommandReply, { expectPending: false, replyAgent: null })[0].type,
    ).toBe("error");
  });
});

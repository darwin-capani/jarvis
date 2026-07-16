import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import ChangeQueuePanel from "../components/ChangeQueuePanel";
import {
  type ChangeqState,
  type PendingChange,
  changeqReduce,
  hasPending,
  isChangeqKind,
  kindLabel,
  MAX_DISPLAY,
  mirroredCount,
  parseChangeqList,
  parsePendingChange,
} from "../core/changeq";
import { type TelemetryEnvelope } from "../core/events";
import { HudState, initialState, reduce } from "../core/state";

/* helpers ------------------------------------------------------------------ */

/** A well-formed changeq.list `pending[]` item as the daemon emits it. */
function item(over: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    seq: 1,
    kind: "code",
    ts: 1700,
    artifact: "state/code/proposals/1700",
    summary: "diff: +3/-1 lines, 2 hunks",
    apply_command: "scripts/apply_code_diff.sh 1700",
    committed: true,
    commit: "COMMITSHA",
    provenance: { agent: "steve", model: "claude-opus-4-8", run: "1700", state_hash: "a1b2c3d4" },
    ...over,
  };
}

function frame(pending: Record<string, unknown>[], branch = "darwin/changeq"): Record<string, unknown> {
  return { branch, count: pending.length, pending };
}

let counter = 0;
function env(event: string, data: Record<string, unknown>): TelemetryEnvelope {
  counter += 1;
  return {
    ts: `2026-07-15T12:00:${String(counter % 60).padStart(2, "0")}Z`,
    source: "local",
    event,
    data,
  };
}

/* ------------------------------------------------------------------------- *
 * PARSER — defensive, never fabricates
 * ------------------------------------------------------------------------- */
describe("parseChangeqList", () => {
  it("parses a well-formed frame into typed pending changes", () => {
    const s = parseChangeqList(frame([item()]));
    expect(s).not.toBeNull();
    expect(s!.branch).toBe("darwin/changeq");
    expect(s!.pending).toHaveLength(1);
    const c = s!.pending[0];
    expect(c.kind).toBe("code");
    expect(c.ts).toBe(1700);
    expect(c.artifact).toBe("state/code/proposals/1700");
    expect(c.applyCommand).toBe("scripts/apply_code_diff.sh 1700");
    expect(c.committed).toBe(true);
    expect(c.commit).toBe("COMMITSHA");
    expect(c.provenance).toEqual({
      agent: "steve",
      model: "claude-opus-4-8",
      run: "1700",
      stateHash: "a1b2c3d4",
    });
  });

  it("DROPS a malformed item (bad kind / missing ts / missing apply command) — never guessed", () => {
    const s = parseChangeqList(
      frame([
        item(),
        item({ kind: "bogus", ts: 2 }), // unknown kind -> dropped
        item({ ts: undefined, seq: 3 }), // missing ts -> dropped
        item({ apply_command: undefined, ts: 4, seq: 4 }), // missing apply cmd -> dropped
      ]),
    );
    expect(s!.pending).toHaveLength(1);
    expect(s!.pending[0].ts).toBe(1700);
  });

  it("fills a garbled/absent provenance field with the honest 'unknown', never a fabrication", () => {
    const s = parseChangeqList(frame([item({ provenance: { agent: "self-heal" } })]));
    expect(s!.pending[0].provenance).toEqual({
      agent: "self-heal",
      model: "unknown",
      run: "unknown",
      stateHash: "unknown",
    });
  });

  it("returns null for a non-record or a pending that is not an array", () => {
    expect(parseChangeqList(null)).toBeNull();
    expect(parseChangeqList(42)).toBeNull();
    expect(parseChangeqList([])).toBeNull();
    expect(parseChangeqList({ branch: "x" })).toBeNull(); // no pending array
    expect(parseChangeqList({ pending: "nope" })).toBeNull();
  });

  it("defaults the branch name when absent", () => {
    const s = parseChangeqList({ pending: [item()] });
    expect(s!.branch).toBe("darwin/changeq");
  });

  it("parsePendingChange rejects a non-object and accepts a valid one", () => {
    expect(parsePendingChange(null)).toBeNull();
    expect(parsePendingChange("x")).toBeNull();
    expect(parsePendingChange(item())).not.toBeNull();
  });
});

/* ------------------------------------------------------------------------- *
 * REDUCER — dedup, newest-first, bounded, malformed-safe
 * ------------------------------------------------------------------------- */
describe("changeqReduce", () => {
  it("replaces with the authoritative frame, deduped by (kind, ts), newest-first", () => {
    const next = parseChangeqList(
      frame([
        item({ seq: 1, kind: "code", ts: 100 }),
        item({ seq: 2, kind: "heal", ts: 300 }),
        item({ seq: 3, kind: "code", ts: 200 }),
        item({ seq: 4, kind: "code", ts: 100, summary: "newer dup" }), // dup of (code,100)
      ]),
    );
    const s = changeqReduce(null, next);
    expect(s!.pending.map((c) => `${c.kind}:${c.ts}`)).toEqual(["heal:300", "code:200", "code:100"]);
    // The later (code,100) won the dedup.
    expect(s!.pending.find((c) => c.kind === "code" && c.ts === 100)!.summary).toBe("newer dup");
  });

  it("IGNORES a malformed frame (null) and keeps the last good state", () => {
    const prev = changeqReduce(null, parseChangeqList(frame([item()])));
    expect(prev).not.toBeNull();
    const after = changeqReduce(prev, null);
    expect(after).toBe(prev); // same reference — a garbled broadcast never blanks the panel
  });

  it("returns null when nothing is pending (panel then renders nothing)", () => {
    const s = changeqReduce(changeqReduce(null, parseChangeqList(frame([item()]))), parseChangeqList(frame([])));
    expect(s).toBeNull();
  });

  it("caps the display list to MAX_DISPLAY", () => {
    const many = Array.from({ length: MAX_DISPLAY + 20 }, (_, i) =>
      item({ seq: i + 1, kind: "code", ts: 1000 + i }),
    );
    const s = changeqReduce(null, parseChangeqList(frame(many)));
    expect(s!.pending).toHaveLength(MAX_DISPLAY);
    // Newest kept (highest ts first).
    expect(s!.pending[0].ts).toBe(1000 + MAX_DISPLAY + 19);
  });
});

/* ------------------------------------------------------------------------- *
 * PURE HELPERS
 * ------------------------------------------------------------------------- */
describe("pure helpers", () => {
  it("isChangeqKind accepts only the four known kinds", () => {
    for (const k of ["heal", "code", "forge", "optimize"]) expect(isChangeqKind(k)).toBe(true);
    expect(isChangeqKind("bogus")).toBe(false);
    expect(isChangeqKind(3)).toBe(false);
  });

  it("kindLabel maps every kind to a human label", () => {
    expect(kindLabel("heal")).toBe("SELF-HEAL PATCH");
    expect(kindLabel("code")).toBe("CODE DIFF");
    expect(kindLabel("forge")).toBe("FORGED APP");
    expect(kindLabel("optimize")).toBe("ROUTING TUNING");
  });

  it("mirroredCount + hasPending reflect the state honestly", () => {
    expect(mirroredCount(null)).toBe(0);
    expect(hasPending(null)).toBe(false);
    const s = changeqReduce(
      null,
      parseChangeqList(frame([item({ ts: 1, committed: true }), item({ ts: 2, seq: 2, committed: false })])),
    );
    expect(hasPending(s)).toBe(true);
    expect(mirroredCount(s)).toBe(1);
    expect(hasPending({ branch: "b", pending: [] } as ChangeqState)).toBe(false);
  });
});

/* ------------------------------------------------------------------------- *
 * PANEL — renders review-only, NO one-click apply (renderToStaticMarkup, node)
 * ------------------------------------------------------------------------- */
describe("ChangeQueuePanel", () => {
  function render(state: ChangeqState | null): string {
    return renderToStaticMarkup(createElement(ChangeQueuePanel, { changeq: state }));
  }

  it("renders nothing when null or empty", () => {
    expect(render(null)).toBe("");
    expect(render({ branch: "darwin/changeq", pending: [] })).toBe("");
  });

  it("lists each pending proposal with its kind, artifact, provenance, and EXISTING apply command", () => {
    const state = changeqReduce(
      null,
      parseChangeqList(frame([item({ kind: "heal", ts: 42, apply_command: "scripts/apply_heal.sh 42" })])),
    );
    const html = render(state);
    expect(html).toContain("CHANGE QUEUE");
    expect(html).toContain("SELF-HEAL PATCH");
    expect(html).toContain("state/code/proposals/1700"); // artifact locator present
    expect(html).toContain("scripts/apply_heal.sh 42"); // the EXISTING gated apply command
    expect(html).toContain("darwin/changeq"); // the review branch
    expect(html).toContain("steve"); // provenance agent
  });

  it("has NO one-click apply control — the only apply route is the shown manual command", () => {
    const state = changeqReduce(null, parseChangeqList(frame([item()])));
    const html = render(state);
    // The propose-only contract: no button / clickable apply affordance in the panel.
    expect(html).not.toContain("<button");
    expect(html.toLowerCase()).toContain("propose-only");
  });
});

/* ------------------------------------------------------------------------- *
 * STATE INTEGRATION — the changeq.list envelope populates state.changeq
 * ------------------------------------------------------------------------- */
describe("state.ts changeq.list reducer wiring", () => {
  function tel(state: HudState, e: TelemetryEnvelope): HudState {
    return reduce(state, { type: "telemetry", envelope: e, at: 1000 });
  }

  it("folds a changeq.list frame into state.changeq (deduped, newest-first)", () => {
    let s = initialState();
    expect(s.changeq).toBeNull();
    s = tel(
      s,
      env("changeq.list", frame([item({ kind: "code", ts: 100 }), item({ kind: "forge", ts: 300, seq: 2 })])),
    );
    expect(s.changeq).not.toBeNull();
    expect(s.changeq!.pending.map((c: PendingChange) => c.kind)).toEqual(["forge", "code"]);
  });

  it("a garbled changeq.list frame never blanks an existing queue", () => {
    let s = initialState();
    s = tel(s, env("changeq.list", frame([item()])));
    const populated = s.changeq;
    s = tel(s, env("changeq.list", { pending: "not-an-array" }));
    expect(s.changeq).toBe(populated); // preserved
  });
});

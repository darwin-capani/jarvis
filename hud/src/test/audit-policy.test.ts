import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import AuditPanel from "../components/AuditPanel";
import SettingsModal, { POLICY_PHRASES } from "../components/SettingsModal";
import {
  coercePolicyDecision,
  liveGateEventFrom,
  parseAuditSnapshot,
  parsePolicySnapshot,
  voiceIdInitial,
  modelTierInitial,
  sttTierInitial,
  type AuditSnapshot,
  type LiveGateEvent,
  type PolicySnapshot,
  type TelemetryEnvelope,
} from "../core/events";
import { HudState, initialState, reduce, LIVE_GATE_CAP } from "../core/state";

/* helpers ------------------------------------------------------------------ */

let counter = 0;
function env(
  event: string,
  data: Record<string, unknown> = {},
  source = "system",
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

const noop = () => {};

/** A realistic audit.snapshot payload: an OK chain, a mix of decisions/outcomes,
 *  a redacted target — and a hostile token-shaped field the daemon would never
 *  send, to pin the secret-free contract. */
const mockAudit: Record<string, unknown> = {
  enabled: true,
  total: 3,
  truncated: false,
  chain: { ok: true, count: 3 },
  entries: [
    {
      seq: 3,
      ts: "2026-06-16T12:00:03Z",
      agent: "agent.pepper",
      tool: "gmail_send",
      target_redacted: "to a@example.com (subj redacted)",
      decision: "always",
      outcome: "executed",
      prev_hash: "abc",
      entry_hash: "def",
    },
    {
      seq: 2,
      ts: "2026-06-16T12:00:02Z",
      agent: "agent.friday",
      tool: "x_post",
      target_redacted: "post (140 chars)",
      decision: "never",
      outcome: "blocked_by_policy",
      prev_hash: "xyz",
      entry_hash: "abc",
    },
    {
      seq: 1,
      ts: "2026-06-16T12:00:01Z",
      agent: "darwin",
      tool: "slack_post_message",
      target_redacted: "#ops",
      decision: "ask",
      outcome: "parked",
      prev_hash: "GENESIS",
      entry_hash: "xyz",
    },
  ],
};

const mockPolicy: Record<string, unknown> = {
  enabled: true,
  rules: [
    { scope: { tool: "gmail_send" }, decision: "always" },
    { scope: { tool: "x_post" }, decision: "never" },
    { scope: { tool: "slack_post_message", recipient: "#ops" }, decision: "always" },
  ],
};

/* ======================================================================== *
 * parseAuditSnapshot (defensive, SECRET-FREE)                                *
 * ======================================================================== */
describe("parseAuditSnapshot (defensive, secret-free)", () => {
  it("parses a well-formed audit.snapshot", () => {
    const s = parseAuditSnapshot(mockAudit);
    expect(s.enabled).toBe(true);
    expect(s.total).toBe(3);
    expect(s.truncated).toBe(false);
    expect(s.chain.ok).toBe(true);
    expect(s.chain.count).toBe(3);
    expect(s.entries.length).toBe(3);
    const top = s.entries[0];
    expect(top.seq).toBe(3);
    expect(top.tool).toBe("gmail_send");
    expect(top.target).toBe("to a@example.com (subj redacted)");
    expect(top.decision).toBe("always");
    expect(top.outcome).toBe("executed");
  });

  it("NEVER surfaces the chain bytes (prev_hash/entry_hash) — only the verdict", () => {
    const s = parseAuditSnapshot(mockAudit);
    const blob = JSON.stringify(s);
    expect(blob).not.toContain("prev_hash");
    expect(blob).not.toContain("entry_hash");
    // the actual hash values are gone too
    expect(blob).not.toContain("GENESIS");
    // but the SECRET-FREE decision/outcome/target survive
    expect(blob).toContain("gmail_send");
    expect(blob).toContain("executed");
  });

  it("NEVER surfaces a hostile token/secret field", () => {
    const s = parseAuditSnapshot({
      enabled: true,
      chain: { ok: true, count: 1 },
      entries: [
        {
          seq: 1,
          tool: "gmail_send",
          target_redacted: "to a@example.com",
          decision: "always",
          outcome: "executed",
          // hostile extras a malformed/compromised payload might carry
          token: "sk-SECRET",
          bearer: "leak",
          input: { password: "hunter2", body: "the real secret email body" },
          raw: "https://user:pw@host",
        },
      ],
    });
    const blob = JSON.stringify(s);
    expect(blob).not.toContain("SECRET");
    expect(blob).not.toContain("leak");
    expect(blob).not.toContain("hunter2");
    expect(blob).not.toContain("the real secret email body");
    expect(blob).not.toContain("user:pw");
    expect(blob).not.toContain("token");
  });

  it("surfaces a BROKEN chain verdict with where + why", () => {
    const s = parseAuditSnapshot({
      enabled: true,
      chain: { ok: false, count: 5, broken_seq: 4, reason: "entry_hash mismatch (a field was altered)" },
      entries: [],
    });
    expect(s.chain.ok).toBe(false);
    expect(s.chain.brokenSeq).toBe(4);
    expect(s.chain.reason).toContain("entry_hash mismatch");
  });

  it("fails toward NOT-OK when the chain status is absent/garbled (never a false green)", () => {
    expect(parseAuditSnapshot({}).chain.ok).toBe(false);
    expect(parseAuditSnapshot({ chain: "nope" }).chain.ok).toBe(false);
    expect(parseAuditSnapshot({ chain: { count: 2 } }).chain.ok).toBe(false);
  });

  it("defaults to the shipped posture + drops malformed entries, never throws", () => {
    const s = parseAuditSnapshot({
      enabled: "yes", // non-bool -> false
      entries: [
        { tool: "x" }, // no seq -> dropped
        { seq: 2 }, // no tool -> dropped
        42, // non-object -> dropped
        { seq: 1, tool: "ok", decision: "garbage", outcome: "weird_future_token" },
      ],
    });
    expect(s.enabled).toBe(false);
    expect(s.entries.length).toBe(1);
    // a junk decision reads as the SAFE "ask", never a loosening
    expect(s.entries[0].decision).toBe("ask");
    // an unknown outcome is carried verbatim (forward-tolerant)
    expect(s.entries[0].outcome).toBe("weird_future_token");
  });

  it("never throws on junk", () => {
    expect(() => parseAuditSnapshot({ entries: "nope" })).not.toThrow();
    expect(parseAuditSnapshot({ entries: "nope" }).entries).toEqual([]);
  });
});

/* ======================================================================== *
 * parsePolicySnapshot (defensive)                                            *
 * ======================================================================== */
describe("parsePolicySnapshot (defensive)", () => {
  it("parses a well-formed policy.snapshot (scope-nested)", () => {
    const s = parsePolicySnapshot(mockPolicy);
    expect(s.enabled).toBe(true);
    expect(s.rules.length).toBe(3);
    expect(s.rules[0]).toEqual({
      tool: "gmail_send",
      agent: null,
      recipient: null,
      decision: "always",
    });
    const scoped = s.rules.find((r) => r.recipient === "#ops")!;
    expect(scoped.tool).toBe("slack_post_message");
    expect(scoped.decision).toBe("always");
  });

  it("SHIPPED-EMPTY default: enabled=false, rules=[] (ASK everywhere)", () => {
    const s = parsePolicySnapshot({});
    expect(s.enabled).toBe(false);
    expect(s.rules).toEqual([]);
  });

  it("a junk decision reads as the SAFE ask, never an always loosening", () => {
    const s = parsePolicySnapshot({
      enabled: true,
      rules: [{ scope: { tool: "gmail_send" }, decision: "definitely allow it" }],
    });
    expect(s.rules[0].decision).toBe("ask");
  });

  it("drops a rule with no tool anchor, never throws", () => {
    const s = parsePolicySnapshot({
      rules: [{ scope: {}, decision: "always" }, "junk", { decision: "never" }],
    });
    expect(s.rules).toEqual([]);
  });
});

describe("coercePolicyDecision", () => {
  it("passes through known tokens", () => {
    expect(coercePolicyDecision("always")).toBe("always");
    expect(coercePolicyDecision("never")).toBe("never");
    expect(coercePolicyDecision("ask")).toBe("ask");
  });
  it("defaults anything else to the SAFE ask (never always)", () => {
    expect(coercePolicyDecision("Always")).toBe("ask"); // case-sensitive
    expect(coercePolicyDecision("allow")).toBe("ask");
    expect(coercePolicyDecision(1)).toBe("ask");
    expect(coercePolicyDecision(null)).toBe("ask");
    expect(coercePolicyDecision(undefined)).toBe("ask");
  });
});

/* ======================================================================== *
 * liveGateEventFrom (chokepoint events, secret-free)                         *
 * ======================================================================== */
describe("liveGateEventFrom (chokepoint events)", () => {
  it("maps policy.blocked / policy.auto_approved / confirm.parked to kinds", () => {
    expect(liveGateEventFrom("policy.blocked", { tool: "x", agent: "a" }, "t", 1)!.kind).toBe(
      "blocked",
    );
    expect(
      liveGateEventFrom("policy.auto_approved", { tool: "x", agent: "a" }, "t", 1)!.kind,
    ).toBe("auto_approved");
    expect(liveGateEventFrom("confirm.parked", { tool: "x", agent: "a" }, "t", 1)!.kind).toBe(
      "parked",
    );
  });

  it("carries the mcp / via routing marker (secret-free)", () => {
    expect(
      liveGateEventFrom("policy.blocked", { tool: "t", agent: "a", mcp: true }, "t", 1)!.via,
    ).toBe("mcp");
    expect(
      liveGateEventFrom("policy.auto_approved", { tool: "t", via: "selector" }, "t", 1)!.via,
    ).toBe("selector");
    expect(liveGateEventFrom("policy.blocked", { tool: "t" }, "t", 1)!.via).toBe(null);
  });

  it("returns null for an unrelated event", () => {
    expect(liveGateEventFrom("audio.level", {}, "t", 1)).toBeNull();
    expect(liveGateEventFrom("answer.verified", {}, "t", 1)).toBeNull();
  });

  it("never carries a target/input (chokepoint events are tool/agent only)", () => {
    const ev = liveGateEventFrom(
      "policy.auto_approved",
      { tool: "gmail_send", agent: "a", body: "secret", token: "sk-X" },
      "t",
      1,
    );
    const blob = JSON.stringify(ev);
    expect(blob).not.toContain("secret");
    expect(blob).not.toContain("sk-X");
  });
});

/* ======================================================================== *
 * Reducer arms                                                               *
 * ======================================================================== */
describe("audit.snapshot / policy.snapshot reducer", () => {
  it("sets the audit snapshot from a well-formed event (secret-free)", () => {
    const s = tel(connected(), env("audit.snapshot", mockAudit));
    expect(s.audit).not.toBeNull();
    expect(s.audit!.chain.ok).toBe(true);
    expect(s.audit!.entries.length).toBe(3);
    expect(JSON.stringify(s.audit)).not.toContain("entry_hash");
  });

  it("sets the policy snapshot; an empty store is the honest ASK-everywhere state", () => {
    const s = tel(connected(), env("policy.snapshot", { enabled: true, rules: [] }));
    expect(s.policy).not.toBeNull();
    expect(s.policy!.enabled).toBe(true);
    expect(s.policy!.rules).toEqual([]);
  });

  it("folds the live chokepoint events newest-first into a bounded ring", () => {
    let s = connected();
    s = tel(s, env("policy.blocked", { tool: "x_post", agent: "agent.friday" }));
    s = tel(s, env("confirm.parked", { tool: "gmail_send", agent: "agent.pepper" }));
    s = tel(s, env("policy.auto_approved", { tool: "slack_post_message", agent: "darwin" }));
    expect(s.liveGate.length).toBe(3);
    // newest-first
    expect(s.liveGate[0].kind).toBe("auto_approved");
    expect(s.liveGate[0].tool).toBe("slack_post_message");
    expect(s.liveGate[2].kind).toBe("blocked");
  });

  it("bounds the live ring at LIVE_GATE_CAP", () => {
    let s = connected();
    for (let i = 0; i < LIVE_GATE_CAP + 10; i++) {
      s = tel(s, env("policy.blocked", { tool: `tool_${i}`, agent: "a" }));
    }
    expect(s.liveGate.length).toBe(LIVE_GATE_CAP);
  });

  it("a live chokepoint event never stores a secret", () => {
    const s = tel(
      connected(),
      env("policy.auto_approved", { tool: "gmail_send", agent: "a", body: "secret-body", token: "sk-X" }),
    );
    const blob = JSON.stringify(s.liveGate);
    expect(blob).not.toContain("secret-body");
    expect(blob).not.toContain("sk-X");
  });

  it("audit.truncated flips the truncated flag on the loaded snapshot", () => {
    let s = tel(connected(), env("audit.snapshot", mockAudit));
    expect(s.audit!.truncated).toBe(false);
    s = tel(s, env("audit.truncated", { removed: 100, kept: 9900 }));
    expect(s.audit!.truncated).toBe(true);
  });

  it("audit.truncated is a no-op (same ref) when no snapshot is loaded", () => {
    const before = connected();
    const after = tel(before, env("audit.truncated", { removed: 1, kept: 1 }));
    expect(after.audit).toBeNull();
  });
});

/* ======================================================================== *
 * AuditPanel (review-only, honest, secret-free)                              *
 * ======================================================================== */
describe("AuditPanel (review-only, honest)", () => {
  const render = (audit: AuditSnapshot | null, liveGate: LiveGateEvent[] = []) =>
    renderToStaticMarkup(createElement(AuditPanel, { audit, liveGate }));

  it("renders nothing before any snapshot or live event", () => {
    expect(render(null, [])).toBe("");
  });

  it("shows the chain-OK indicator and the recent decisions", () => {
    const html = render(parseAuditSnapshot(mockAudit));
    expect(html).toContain("REVIEW ONLY");
    expect(html).toContain("CHAIN OK");
    expect(html).toContain("3 entries verified");
    // the decisions + outcomes
    expect(html).toContain("gmail_send");
    expect(html).toContain("EXECUTED");
    expect(html).toContain("x_post");
    expect(html).toContain("BLOCKED");
    expect(html).toContain("PARKED");
    // the redacted target is shown
    expect(html).toContain("#ops");
  });

  it("shows a TAMPER verdict with where it broke", () => {
    const html = render(
      parseAuditSnapshot({
        enabled: true,
        chain: { ok: false, count: 5, broken_seq: 4, reason: "entry_hash mismatch (a field was altered)" },
        entries: [],
      }),
    );
    expect(html).toContain("CHAIN TAMPER DETECTED");
    expect(html).toContain("#4");
    expect(html).toContain("entry_hash mismatch");
  });

  it("surfaces the HONEST copy: tamper-EVIDENT not tamper-PROOF; backstops; NEVER wins", () => {
    const html = render(parseAuditSnapshot(mockAudit));
    expect(html).toContain("tamper-EVIDENT");
    expect(html).toContain("not tamper-PROOF");
    expect(html.toLowerCase()).toContain("rewrites the whole on-disk chain");
    expect(html).toContain("master switch");
    expect(html).toContain("voice-id");
    expect(html.toLowerCase()).toContain("never always wins");
  });

  it("has NO action button — it is review-only", () => {
    const html = render(parseAuditSnapshot(mockAudit));
    expect(html).not.toContain("<button");
  });

  it("never renders a secret / chain byte even from a hostile snapshot", () => {
    const html = render(
      parseAuditSnapshot({
        enabled: true,
        chain: { ok: true, count: 1 },
        entries: [
          {
            seq: 1,
            tool: "gmail_send",
            target_redacted: "to a@example.com",
            decision: "always",
            outcome: "executed",
            token: "sk-SECRET",
            input: { body: "the secret body" },
            entry_hash: "deadbeef",
          },
        ],
      }),
    );
    expect(html).not.toContain("SECRET");
    expect(html).not.toContain("the secret body");
    expect(html).not.toContain("deadbeef");
  });

  it("folds in the LIVE chokepoint events before the snapshot entries", () => {
    const live: LiveGateEvent[] = [
      { kind: "auto_approved", tool: "gmail_send", agent: "a", via: "mcp", ts: "2026-06-16T12:00:09Z", seq: 9 },
    ];
    const html = render(parseAuditSnapshot(mockAudit), live);
    expect(html).toContain("LIVE");
    expect(html).toContain("AUTO-APPROVED");
    expect(html).toContain("mcp");
  });

  it("shows the honest empty state when nothing has been recorded", () => {
    const html = render(parseAuditSnapshot({ enabled: true, chain: { ok: true, count: 0 }, entries: [] }));
    expect(html.toLowerCase()).toContain("no consequential decision recorded yet");
    expect(html.toLowerCase()).toContain("ask");
  });

  it("shows the truncation note when a prune re-rooted the chain", () => {
    const html = render(parseAuditSnapshot({ ...mockAudit, truncated: true }));
    expect(html.toLowerCase()).toContain("re-rooted");
    expect(html.toLowerCase()).toContain("still verifies");
  });

  it("shows the honest OFF state when audit is disabled", () => {
    const html = render(parseAuditSnapshot({ enabled: false, entries: [] }));
    expect(html.toLowerCase()).toContain("audit log is off");
  });
});

/* ======================================================================== *
 * SettingsModal — POLICY editor (user-set only, honest)                      *
 * ======================================================================== */
function renderSettings(policy: PolicySnapshot | null): string {
  return renderToStaticMarkup(
    createElement(SettingsModal, {
      mcp: null,
      voiceId: voiceIdInitial(),
      modelTier: modelTierInitial(),
      sttTier: sttTierInitial(),
      policy,
      onClose: noop,
    }),
  );
}

describe("SettingsModal policy editor (user-set only, honest)", () => {
  it("shows the section with the ALWAYS / NEVER / ASK controls", () => {
    const html = renderSettings(parsePolicySnapshot({ enabled: true, rules: [] }));
    expect(html).toContain("CONSEQUENTIAL POLICY");
    expect(html).toContain("ALWAYS ALLOW");
    expect(html).toContain("NEVER");
    expect(html).toContain("ASK (DEFAULT)");
  });

  it("renders the honest empty / ASK-everywhere state", () => {
    const html = renderSettings(parsePolicySnapshot({ enabled: true, rules: [] }));
    expect(html).toContain("EMPTY · ASK EVERYWHERE");
  });

  it("lists the user-set rules with their decisions + scope", () => {
    const html = renderSettings(parsePolicySnapshot(mockPolicy));
    expect(html).toContain("gmail_send");
    expect(html).toContain("x_post");
    expect(html).toContain("slack_post_message");
    // the scoped recipient is shown
    expect(html).toContain("#ops");
    // both ALWAYS and NEVER decisions render
    expect(html).toContain(">ALWAYS<");
    expect(html).toContain(">NEVER<");
  });

  it("surfaces the HONEST invariants: master ceiling, NEVER wins, user-set only, inert ALWAYS", () => {
    const html = renderSettings(parsePolicySnapshot({ enabled: true, rules: [] }));
    expect(html).toContain("allow_consequential");
    expect(html.toLowerCase()).toContain("master switch");
    expect(html.toLowerCase()).toContain("inert"); // ALWAYS inert when master off
    expect(html).toContain("USER-SET ONLY");
    expect(html.toLowerCase()).toContain("cannot take effect"); // injected set-policy cannot fire
    // "NEVER always wins" — the words bracket a <b> tag in the rendered markup,
    // so assert the surrounding hard-block clause rather than a contiguous string.
    expect(html.toLowerCase()).toContain("always wins");
    expect(html.toLowerCase()).toContain("hard-block");
    expect(html.toLowerCase()).toContain("ask everywhere");
  });

  it("the policy phrases are explicit, tool-named, user-only writes", () => {
    // The HUD half of the round-trip a daemon classifier test will lock. These
    // are USER-SET writes over the command channel — there is no other write path.
    expect(POLICY_PHRASES.always("gmail_send")).toBe("always allow the gmail_send action");
    expect(POLICY_PHRASES.never("x_post")).toBe("never allow the x_post action");
    expect(POLICY_PHRASES.ask("slack_post_message")).toBe(
      "always ask before the slack_post_message action",
    );
    // each names the verb AND the specific tool — never a blanket all-tools rule
    expect(POLICY_PHRASES.always("t")).toContain("t");
    expect(POLICY_PHRASES.never("t")).toContain("never");
  });

  it("renders the AWAITING state when no policy snapshot has arrived", () => {
    const html = renderSettings(null);
    expect(html).toContain("AWAITING");
  });
});

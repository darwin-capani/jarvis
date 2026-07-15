import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import SuggestionsPanel from "../components/SuggestionsPanel";
import {
  parseSuggestion,
  suggestionAcceptText,
  type Suggestion,
  type TelemetryEnvelope,
} from "../core/events";
import { HudState, initialState, reduce, SUGGESTION_CAP } from "../core/state";

/* helpers ------------------------------------------------------------------ */

let counter = 0;
function env(
  event: string,
  data: Record<string, unknown> = {},
  source = "system",
): TelemetryEnvelope {
  counter += 1;
  return {
    ts: `2026-06-16T09:00:${String(counter % 60).padStart(2, "0")}Z`,
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

/** The daemon's habit-automation telemetry card (Suggestion::telemetry(),
 *  habit kind). Mirrors the exact wire fields the daemon emits + the test
 *  habit_offer_telemetry_carries_the_feed_card_fields asserts. */
function habitCard(over: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    id: "habit:agent.darwin:budget.review",
    agent: "agent.darwin",
    text: "You review the budget every weekday morning — want me to make that a standing mission?",
    kind: "habit_automation",
    topic: "budget.review",
    occurrences: 3,
    proposed_goal: "review the budget.review",
    proposed_schedule: "daily at 09:00",
    accept_routes_through: "standing_create",
    auto_acts: false,
    ...over,
  };
}

/** The daemon's predictive telemetry card (predictive kind) — NO proposed_goal
 *  (a prediction has no action to accept). */
function predictiveCard(over: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    id: "pred:agent.darwin:news.brief:morning",
    agent: "agent.darwin",
    text: "You usually catch the news brief around morning.",
    kind: "predictive",
    topic: "news.brief",
    time_of_day: "morning",
    occurrences: 4,
    auto_acts: false,
    ...over,
  };
}

/* ------------------------------------------------------------------------ *
 * The defensive parser. A card the panel cannot render+address must NEVER be
 * surfaced: no id, an unknown kind, or a habit offer with no proposed goal an
 * Accept could route through the gated standing path. Never fabricates.
 * ------------------------------------------------------------------------ */
describe("parseSuggestion (defensive, never-fabricate)", () => {
  it("parses a well-formed habit-automation card (the feed-card fields)", () => {
    const sg = parseSuggestion(habitCard());
    expect(sg).not.toBeNull();
    expect(sg!.kind).toBe("habit_automation");
    expect(sg!.id).toBe("habit:agent.darwin:budget.review");
    expect(sg!.agent).toBe("agent.darwin");
    expect(sg!.topic).toBe("budget.review");
    expect(sg!.occurrences).toBe(3);
    expect(sg!.proposedGoal).toBe("review the budget.review");
    expect(sg!.proposedSchedule).toBe("daily at 09:00");
    expect(sg!.acceptRoutesThrough).toBe("standing_create");
    // A suggestion NEVER auto-acts.
    expect(sg!.autoActs).toBe(false);
  });

  it("parses a predictive card and carries NO proposed goal (no action)", () => {
    const sg = parseSuggestion(predictiveCard());
    expect(sg).not.toBeNull();
    expect(sg!.kind).toBe("predictive");
    expect(sg!.timeOfDay).toBe("morning");
    expect(sg!.occurrences).toBe(4);
    // A prediction has nothing to accept.
    expect(sg!.proposedGoal).toBeNull();
    expect(sg!.proposedSchedule).toBeNull();
    expect(sg!.acceptRoutesThrough).toBeNull();
    expect(sg!.autoActs).toBe(false);
  });

  it("returns null without a usable id (cannot Accept/Dismiss it)", () => {
    expect(parseSuggestion(habitCard({ id: undefined }))).toBeNull();
    expect(parseSuggestion(habitCard({ id: "" }))).toBeNull();
    expect(parseSuggestion(habitCard({ id: 42 }))).toBeNull();
  });

  it("returns null for an unknown/missing kind (panel renders only known kinds)", () => {
    expect(parseSuggestion(habitCard({ kind: undefined }))).toBeNull();
    expect(parseSuggestion(habitCard({ kind: "spooky_action" }))).toBeNull();
  });

  it("returns null for a habit offer with NO proposed goal (no gated route)", () => {
    // An Accept with no goal could not route through the gated standing path, so
    // the card is not renderable — never surfaced.
    expect(parseSuggestion(habitCard({ proposed_goal: undefined }))).toBeNull();
    expect(parseSuggestion(habitCard({ proposed_goal: "" }))).toBeNull();
  });

  it("forces auto_acts to false even if a hostile payload claimed true", () => {
    // A card claiming auto_acts:true is a contract violation — never honored.
    const sg = parseSuggestion(habitCard({ auto_acts: true }));
    expect(sg!.autoActs).toBe(false);
    const p = parseSuggestion(predictiveCard({ auto_acts: true }));
    expect(p!.autoActs).toBe(false);
  });

  it("forces a predictive card's proposed goal to null even if smuggled", () => {
    // A prediction must carry no action even if a payload sneaks a goal in.
    const sg = parseSuggestion(
      predictiveCard({ proposed_goal: "sneaky", accept_routes_through: "standing_create" }),
    );
    expect(sg!.proposedGoal).toBeNull();
    expect(sg!.acceptRoutesThrough).toBeNull();
  });

  it("never throws on junk", () => {
    expect(() => parseSuggestion({})).not.toThrow();
    expect(parseSuggestion({})).toBeNull();
  });
});

/* ------------------------------------------------------------------------ *
 * The Accept-text builder. ACCEPT is the gated standing-creation route — it
 * phrases a standing-mission SETUP request the daemon routes to standing_create
 * (parked behind the gate). A predictive card has no action -> null.
 * ------------------------------------------------------------------------ */
describe("suggestionAcceptText (gated route, never ungated create)", () => {
  it("builds a standing-mission SETUP request from a habit offer", () => {
    const sg = parseSuggestion(habitCard())!;
    const text = suggestionAcceptText(sg);
    expect(text).not.toBeNull();
    // Explicit standing-setup phrasing + the schedule's hard recurring cue both
    // point the daemon selector at the gated Standing route (not a one-shot).
    expect(text).toMatch(/standing mission/i);
    expect(text).toContain("review the budget.review");
    expect(text).toContain("daily at 09:00");
  });

  it("omits the schedule clause when there is none", () => {
    const sg = parseSuggestion(habitCard({ proposed_schedule: undefined }))!;
    const text = suggestionAcceptText(sg);
    expect(text).toMatch(/standing mission to review the budget\.review\.?$/i);
  });

  it("yields null for a predictive suggestion (nothing to accept)", () => {
    const sg = parseSuggestion(predictiveCard())!;
    expect(suggestionAcceptText(sg)).toBeNull();
  });
});

/* ------------------------------------------------------------------------ *
 * The reducer. proactive.suggestion surfaces a card on the suggestions feed;
 * it NEVER acts. dedup updates in place; a dismissed id stays suppressed; the
 * feature OFF (no event) means an empty feed; the card is secret-free; the
 * agent scope is preserved verbatim.
 * ------------------------------------------------------------------------ */
describe("proactive.suggestion reducer", () => {
  it("surfaces a habit-automation offer on the suggestions feed", () => {
    const s = tel(connected(), env("proactive.suggestion", habitCard()));
    expect(s.suggestions.length).toBe(1);
    expect(s.suggestions[0].kind).toBe("habit_automation");
    expect(s.suggestions[0].proposedGoal).toBe("review the budget.review");
  });

  it("surfaces a predictive suggestion (intel only, no action)", () => {
    const s = tel(connected(), env("proactive.suggestion", predictiveCard()));
    expect(s.suggestions.length).toBe(1);
    expect(s.suggestions[0].kind).toBe("predictive");
    expect(s.suggestions[0].proposedGoal).toBeNull();
  });

  it("[proactive] OFF (no event emitted) => no suggestions", () => {
    // The daemon only emits proactive.suggestion with the feature ON. With it OFF
    // no event arrives, so the feed stays empty (the panel renders nothing).
    const s = connected();
    expect(s.suggestions.length).toBe(0);
  });

  it("never fabricates a card from a malformed payload", () => {
    const s = tel(connected(), env("proactive.suggestion", { kind: "habit_automation" }));
    expect(s.suggestions.length).toBe(0);
  });

  it("dedups a re-emit of the same id (updates in place, no duplicate)", () => {
    let s = tel(connected(), env("proactive.suggestion", habitCard({ occurrences: 3 })));
    s = tel(s, env("proactive.suggestion", habitCard({ occurrences: 5 })));
    expect(s.suggestions.length).toBe(1);
    // Newest evidence wins.
    expect(s.suggestions[0].occurrences).toBe(5);
  });

  it("suppresses a re-offer of a DISMISSED id (the dismiss ledger)", () => {
    let s = tel(connected(), env("proactive.suggestion", habitCard()));
    expect(s.suggestions.length).toBe(1);
    const id = s.suggestions[0].id;
    s = reduce(s, { type: "suggestion.dismiss", id });
    expect(s.suggestions.length).toBe(0);
    expect(s.dismissedSuggestions.has(id)).toBe(true);
    // The daemon re-mines the same recurring pattern -> same id -> suppressed.
    s = tel(s, env("proactive.suggestion", habitCard()));
    expect(s.suggestions.length).toBe(0);
  });

  it("a dismiss is idempotent / no-churn when nothing matches", () => {
    const s = connected();
    const s2 = reduce(s, { type: "suggestion.dismiss", id: "nope" });
    // First dismiss of an unseen id records it (suppresses a future re-offer).
    expect(s2.dismissedSuggestions.has("nope")).toBe(true);
    // A second dismiss of the same id is a no-op (same reference).
    const s3 = reduce(s2, { type: "suggestion.dismiss", id: "nope" });
    expect(s3).toBe(s2);
  });

  it("preserves the agent scope verbatim (a suggestion stays in its scope)", () => {
    // A card mined under agent A keeps agent A — the HUD never re-scopes it.
    const s = tel(
      connected(),
      env("proactive.suggestion", habitCard({ id: "habit:agent.edith:x", agent: "agent.edith" })),
    );
    expect(s.suggestions[0].agent).toBe("agent.edith");
  });

  it("two distinct ids (same topic, different agents) are two cards", () => {
    let s = tel(
      connected(),
      env("proactive.suggestion", habitCard({ id: "habit:agent.darwin:t", agent: "agent.darwin" })),
    );
    s = tel(
      s,
      env("proactive.suggestion", habitCard({ id: "habit:agent.edith:t", agent: "agent.edith" })),
    );
    expect(s.suggestions.length).toBe(2);
    const agents = s.suggestions.map((x) => x.agent).sort();
    expect(agents).toEqual(["agent.darwin", "agent.edith"]);
  });

  it("bounds the feed at SUGGESTION_CAP", () => {
    let s = connected();
    for (let i = 0; i < SUGGESTION_CAP + 5; i += 1) {
      s = tel(s, env("proactive.suggestion", habitCard({ id: `habit:n${i}` })));
    }
    expect(s.suggestions.length).toBe(SUGGESTION_CAP);
  });

  it("never carries a secret — only contracted fields survive into state", () => {
    const s = tel(
      connected(),
      env("proactive.suggestion", habitCard({ api_key: "sk-SECRET", token: "leak" })),
    );
    const serialized = JSON.stringify(s.suggestions);
    expect(serialized).not.toContain("SECRET");
    expect(serialized).not.toContain("leak");
    expect(serialized).not.toContain("api_key");
  });
});

/* ------------------------------------------------------------------------ *
 * The panel. Honest, propose-only copy; Accept ONLY on a habit offer (routes
 * through the gated standing creation), no Accept on a prediction; Dismiss on
 * every card; never an auto-act / one-click create affordance.
 * ------------------------------------------------------------------------ */
describe("SuggestionsPanel (propose-only, gated accept)", () => {
  const habit = parseSuggestion(habitCard())!;
  const prediction = parseSuggestion(predictiveCard())!;

  const render = (suggestions: Suggestion[]) =>
    renderToStaticMarkup(
      createElement(SuggestionsPanel, {
        suggestions,
        onAccept: () => {},
        onDismiss: () => {},
      }),
    );

  it("renders nothing when the feed is empty (event-fed panel)", () => {
    expect(render([])).toBe("");
  });

  it("renders a habit offer: preview + ACCEPT + DISMISS, honest gate copy", () => {
    const html = render([habit]);
    expect(html).toContain("HABIT OFFER");
    expect(html).toContain("review the budget.review");
    expect(html).toContain("daily at 09:00");
    expect(html).toContain("ACCEPT");
    expect(html).toContain("DISMISS");
    // The gated-accept posture is stated, naming the standing_create route.
    expect(html).toContain("standing_create");
    expect(html).toMatch(/parks for your confirmation/i);
  });

  it("renders a predictive suggestion with DISMISS but NO ACCEPT (no action)", () => {
    const html = render([prediction]);
    expect(html).toContain("PREDICTION");
    expect(html).toContain("DISMISS");
    // A prediction has no action — no Accept verb.
    expect(html).not.toContain("ACCEPT");
  });

  it("states the never-auto-act + observed-pattern honesty", () => {
    const html = render([habit, prediction]);
    expect(html).toMatch(/never auto-acts/i);
    expect(html).toMatch(/suggestions, not actions/i);
    expect(html).toMatch(/can be wrong/i);
  });

  it("has NO auto-act / one-click create affordance — only ACCEPT + DISMISS verbs", () => {
    const html = render([habit]);
    // No button label implying a direct/ungated create or auto-run.
    expect(html).not.toMatch(/<button[^>]*>[^<]*(CREATE|RUN NOW|DO IT|AUTOMATE NOW)/i);
  });

  it("surfaces the evidence (occurrences) so the user sees WHY it was offered", () => {
    const html = render([habit]);
    expect(html).toMatch(/observed\s*3/i);
  });
});

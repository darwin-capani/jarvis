import type { Suggestion } from "../core/events";
import { suggestionAcceptText } from "../core/events";
import Frame from "./Frame";

/**
 * SUGGESTIONS // PROACTIVE INTEL — the propose-only feed for the daemon's
 * proactive-intelligence module (#13 habit-automation offers + #14 predictive
 * suggestions, daemon/src/proactive_intel.rs Suggestion::telemetry() ->
 * `proactive.suggestion`). Each card is an OBSERVED-pattern suggestion mined
 * from the redacted, agent-scoped episodic store — never an action.
 *
 * SAFETY / HONESTY CONTRACT (do not regress):
 *   - These are SUGGESTIONS, not actions. DARWIN NEVER auto-acts on them (every
 *     card carries auto_acts=false; the panel renders no "do it now" button).
 *     They are OBSERVED-pattern based (threshold-gated), so they CAN be wrong and
 *     are always DISMISSIBLE.
 *   - ACCEPT (habit offers only) routes through the EXISTING gated standing-
 *     mission creation — it sends a standing-mission SETUP request that the
 *     daemon's selector routes to `standing_create`, which PARKS behind the
 *     cross-turn confirmation gate. So accepting STILL goes through the normal
 *     confirmation gate; this panel never directly/ungated creates a mission.
 *     A predictive suggestion carries NO action (no proposed goal) — it shows no
 *     Accept, only Dismiss.
 *   - DISMISS drops the card and suppresses the same id on a re-offer (the
 *     dismiss ledger), so a dismissed suggestion is not nagged repeatedly.
 *   - The detection is a HEURISTIC — it does NOT "know what you want". The copy
 *     says so. It SHIPS OFF (mirrors proactive.speak): with [proactive] off the
 *     daemon emits no cards and this panel renders nothing.
 *   - SECRET-FREE: every field traces to redacted episodic data (parseSuggestion
 *     drops anything malformed/unaddressable). The proposed goal/schedule are the
 *     PROPOSED (not created) mission, shown for preview only.
 *
 * `onAccept(id, text)` is handed the offer id + the natural-language standing-
 * setup request to send via the gated command channel (App.tsx wires it to
 * sendCommand({cmd:"ask"}), the same gated path a spoken "set up a standing
 * mission to ..." takes, and dismisses the offer locally so it is not re-shown).
 * `onDismiss(id)` dispatches the dismiss action.
 */
export default function SuggestionsPanel({
  suggestions,
  onAccept,
  onDismiss,
}: {
  suggestions: Suggestion[];
  onAccept: (id: string, text: string) => void;
  onDismiss: (id: string) => void;
}) {
  // Event-fed panel: nothing to show until the daemon emits a card (it only does
  // so with [proactive] on AND a real recurring pattern over threshold). Render
  // nothing rather than a placeholder, mirroring the other telemetry panels.
  if (suggestions.length === 0) return null;

  return (
    <div className="suggestions-panel">
      <Frame title="SUGGESTIONS // PROACTIVE INTEL" tag="OBSERVED · DISMISSIBLE">
        <div className="sg-body">
          {suggestions.map((s) => (
            <SuggestionCard
              key={s.id}
              suggestion={s}
              onAccept={onAccept}
              onDismiss={onDismiss}
            />
          ))}

          <div className="sg-foot dim-note">
            These are SUGGESTIONS, not actions — DARWIN never acts on them by
            itself. They come from patterns observed in your own past turns, so
            they can be wrong; dismiss any that miss. Accepting a habit offer does
            not create anything directly: it proposes a standing mission that still
            parks behind the normal confirmation gate before it is established.
          </div>
        </div>
      </Frame>
    </div>
  );
}

/** One suggestion card. A habit offer previews the PROPOSED (not created)
 *  mission and shows Accept (routes through the gated standing creation) +
 *  Dismiss; a predictive suggestion is intel only and shows just Dismiss. */
function SuggestionCard({
  suggestion,
  onAccept,
  onDismiss,
}: {
  suggestion: Suggestion;
  onAccept: (id: string, text: string) => void;
  onDismiss: (id: string) => void;
}) {
  const isHabit = suggestion.kind === "habit_automation";
  // Non-null only for a habit offer with a proposed goal (a predictive card has
  // nothing to accept) — so Accept appears exactly when there is a gated route.
  const acceptText = suggestionAcceptText(suggestion);

  return (
    <div className={`sg-card ${isHabit ? "habit" : "predictive"}`}>
      <div className="sg-head">
        <span className={`sg-kind ${isHabit ? "habit" : "predictive"}`}>
          {isHabit ? "HABIT OFFER" : "PREDICTION"}
        </span>
        <span className="sg-agent">{suggestion.agent}</span>
        {/* The honest never-auto-act marker, grounded in the wire field. */}
        <span className="sg-noact" title="DARWIN never acts on a suggestion by itself">
          NEVER AUTO-ACTS
        </span>
      </div>

      {/* The daemon-authored, dismissible human line. */}
      <div className="sg-text">{suggestion.text}</div>

      {/* The evidence — surfaced honestly so the user sees WHY it was offered. */}
      <div className="sg-evidence dim-note">
        Observed {suggestion.occurrences}×
        {suggestion.topic ? <> · {suggestion.topic}</> : null}
        {!isHabit && suggestion.timeOfDay ? <> · {suggestion.timeOfDay}</> : null}
      </div>

      {/* Habit offer: preview the PROPOSED standing mission (NOT created). */}
      {isHabit && suggestion.proposedGoal !== null && (
        <div className="sg-proposal">
          <div className="sg-proposal-label">PROPOSED STANDING MISSION (PREVIEW)</div>
          <div className="sg-row">
            <span className="sg-k">GOAL</span>
            <span className="sg-v">{suggestion.proposedGoal}</span>
          </div>
          {suggestion.proposedSchedule ? (
            <div className="sg-row">
              <span className="sg-k">SCHEDULE</span>
              <span className="sg-v">{suggestion.proposedSchedule}</span>
            </div>
          ) : null}
          <div className="sg-gate dim-note">
            Accept routes through{" "}
            <code>{suggestion.acceptRoutesThrough ?? "standing_create"}</code> — it
            proposes this mission and parks for your confirmation. Nothing is
            created until you confirm at the gate.
          </div>
        </div>
      )}

      <div className="sg-actions">
        {/* Accept ONLY when there is a gated action to route (habit offer with a
            proposed goal). It is the dedicated gated-creation route — never an
            ungated create. A predictive card has no Accept. */}
        {acceptText !== null && (
          <button
            type="button"
            className="sg-accept"
            onClick={() => onAccept(suggestion.id, acceptText)}
            title="Propose this as a standing mission (parks for confirmation at the gate)"
          >
            ACCEPT
          </button>
        )}
        <button
          type="button"
          className="sg-dismiss"
          onClick={() => onDismiss(suggestion.id)}
          title="Dismiss this suggestion (it will not be offered again)"
        >
          DISMISS
        </button>
      </div>
    </div>
  );
}

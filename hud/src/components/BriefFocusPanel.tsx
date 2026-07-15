import type { BriefItem, FocusActive, ProactiveDigest } from "../core/events";
import { briefPriorityLabel, focusIsDefault } from "../core/events";
import Frame from "./Frame";

/**
 * BRIEF // FOCUS — the read-only attention surface for #23 (the smarter brief)
 * and #24 (focus profiles), fed by the daemon's `proactive.digest`
 * (daemon/src/brief.rs Brief::telemetry) and `focus.active`
 * (daemon/src/focus.rs TunedBehavior::telemetry), both emitted by agent.edith.
 *
 * It surfaces two honest readouts:
 *
 *   BRIEF — the PRIORITIZED brief items, each with its REAL source citation
 *   (the signal's origin: a calendar event id / a message id / a news source).
 *   When there is no digest (or it is honestly empty) it shows the honest-empty
 *   state — "nothing to brief; no signals; DARWIN won't pad it" — rather than a
 *   fabricated item. An UNCONNECTED source contributes no signal (honestly
 *   absent), so an empty radar reads as quiet, never invented.
 *
 *   FOCUS PROFILE — the ACTIVE profile (default / work / sleep / deep-focus /
 *   a named custom) and what it is QUIETING (the categories that still surface,
 *   the brief verbosity, whether suggestions are quieted). The default profile
 *   is the identity — today's behavior, nothing quieted.
 *
 * HONESTY CONTRACT (do not regress):
 *   - CITES REAL SIGNALS, NEVER FABRICATES. Every brief row carries the real
 *     rendered source the daemon attached; the parser dropped any row without
 *     one. There is nothing here to invent a citation from.
 *   - HONESTLY EMPTY. With no signal the brief says so plainly — it never pads
 *     the readout with a phantom item.
 *   - FOCUS ONLY QUIETS, NEVER LOOSENS. A focus profile is PERMISSION-NEUTRAL by
 *     construction: it only adjusts which categories surface, the brief
 *     verbosity, and whether suggestions are quieted. It NEVER loosens a gate,
 *     enables an action, or raises autonomy. The card states that posture on the
 *     wire (permission_neutral / raises_autonomy=false / loosens_gate=false),
 *     pinned HUD-side, and this panel's copy says it out loud.
 *   - SHIPPED OFF / NEUTRAL. [focus].profile ships "default" (the identity) and
 *     the daemon only emits a NON-empty digest, so until a profile is selected /
 *     a signal surfaces, the focus readout shows "today's behavior" and the brief
 *     shows nothing.
 *   - READ-ONLY. There is NO button here. This panel only SHOWS the digest +
 *     posture the daemon already produced.
 *
 * The reducer holds `proactiveDigest` at null unless a real NON-empty digest
 * arrived (an empty/garbled one clears it to null), and holds `focusProfile` at
 * null until the daemon emits the startup focus.active card — so this component
 * renders nothing until there is something honest to show.
 */
export default function BriefFocusPanel({
  digest,
  focus,
}: {
  digest: ProactiveDigest | null;
  focus: FocusActive | null;
}) {
  // Nothing to show until a real digest OR a focus posture arrives. With the
  // shipped defaults (no signal surfaced + focus = "default") the reducer holds
  // both at null, so render nothing rather than a placeholder — mirroring the
  // other event-fed panels (AnswerSourcesPanel, DocSearchPanel).
  if (digest === null && focus === null) return null;

  return (
    <div className="brief-panel">
      <Frame title="BRIEF // FOCUS" tag="HONEST · READ ONLY">
        <div className="brief-body">
          <BriefSection digest={digest} />
          <FocusSection focus={focus} />
        </div>
      </Frame>
    </div>
  );
}

/** The BRIEF readout: the prioritized, cited items — or the honest-empty state
 *  when there is no digest (or it is empty). Never pads a fabricated item. */
function BriefSection({ digest }: { digest: ProactiveDigest | null }) {
  // No digest, or an honest-empty one => the honest "nothing to brief" copy.
  const empty = digest === null || digest.empty || digest.items.length === 0;

  return (
    <div className="brief-section">
      <div className="brief-section-head">
        <span className="brief-section-title">BRIEF</span>
        {empty ? (
          <span
            className="brief-pill empty"
            title="no signals surfaced — DARWIN does not pad the brief with a fabricated item"
          >
            NOTHING TO BRIEF
          </span>
        ) : (
          <span
            className="brief-pill cited"
            title="prioritized brief items, each cited to its real source"
          >
            {digest.items.length} CITED
          </span>
        )}
      </div>

      {empty ? (
        <div className="brief-empty dim-note">
          Nothing to brief — no signals on the radar. DARWIN won&rsquo;t pad it
          with an invented item; an unconnected source contributes nothing
          (honestly absent, never fabricated).
        </div>
      ) : (
        <div className="brief-item-list">
          {digest.items.map((it, i) => (
            <BriefRow key={`${it.source}:${i}`} item={it} />
          ))}
        </div>
      )}
    </div>
  );
}

/** One prioritized brief item: its priority chip, the honest line, and the REAL
 *  source citation that makes it verifiable (never fabricated). */
function BriefRow({ item }: { item: BriefItem }) {
  return (
    <div className="brief-item">
      <div className="brief-item-head">
        <span
          className={`brief-pill prio-${item.priority}`}
          title="the relevance priority the daemon ranked this signal at"
        >
          {briefPriorityLabel(item.priority)}
        </span>
        <span className="brief-item-text">{item.text}</span>
      </div>
      <span
        className="brief-item-source"
        title="the REAL source this item is cited to (calendar event id / message id / news source) — never fabricated"
      >
        {item.source}
      </span>
    </div>
  );
}

/** The FOCUS PROFILE readout: the active profile + what it is quieting, with the
 *  permission-neutral posture stated honestly. The default profile is the
 *  identity (today's behavior, nothing quieted). */
function FocusSection({ focus }: { focus: FocusActive | null }) {
  if (focus === null) {
    // No focus.active seen yet — state the shipped default honestly rather than
    // a blank. (The daemon emits the card at the loop start; until then the
    // shipped default IS "default".)
    return (
      <div className="brief-section">
        <div className="brief-section-head">
          <span className="brief-section-title">FOCUS PROFILE</span>
          <span className="brief-pill profile-default" title="the shipped default profile — today's behavior">
            DEFAULT
          </span>
        </div>
        <div className="focus-default dim-note">
          Default focus — today&rsquo;s behavior. Nothing is quieted.{" "}
          <FocusContractNote />
        </div>
      </div>
    );
  }

  const isDefault = focusIsDefault(focus);
  const profileLabel = focus.profile.toUpperCase();

  return (
    <div className="brief-section">
      <div className="brief-section-head">
        <span className="brief-section-title">FOCUS PROFILE</span>
        <span
          className={`brief-pill profile-${focus.profile}`}
          title="the active focus profile — a permission-neutral lens that only quiets what surfaces"
        >
          {profileLabel}
        </span>
      </div>

      {isDefault ? (
        <div className="focus-default dim-note">
          Default focus — today&rsquo;s behavior. Nothing is quieted.{" "}
          <FocusContractNote />
        </div>
      ) : (
        <div className="focus-body">
          <FocusQuieting focus={focus} />
          <div className="focus-foot dim-note">
            <FocusContractNote />
          </div>
        </div>
      )}
    </div>
  );
}

/** What the active (non-default) profile is quieting: the categories that still
 *  surface (others are silenced), the brief verbosity, and whether suggestions
 *  are quieted — all read straight from the wire. */
function FocusQuieting({ focus }: { focus: FocusActive }) {
  return (
    <div className="focus-quieting">
      <div className="focus-knob">
        <span className="focus-knob-label">SURFACING</span>
        {focus.surfacing.length === 0 ? (
          <span
            className="focus-knob-value muted"
            title="this profile surfaces nothing but a genuinely critical signal"
          >
            critical only — everything else quieted
          </span>
        ) : (
          <span className="focus-knob-value">
            {focus.surfacing.map((c) => (
              <span key={c} className="focus-cat">
                {c}
              </span>
            ))}
          </span>
        )}
      </div>
      <div className="focus-knob">
        <span className="focus-knob-label">VERBOSITY</span>
        <span className={`focus-knob-value verb-${focus.verbosity}`}>{focus.verbosity}</span>
      </div>
      <div className="focus-knob">
        <span className="focus-knob-label">SUGGESTIONS</span>
        <span className={`focus-knob-value ${focus.suggestionsQuieted ? "muted" : ""}`}>
          {focus.suggestionsQuieted ? "quieted" : "as normal"}
        </span>
      </div>
    </div>
  );
}

/** The shared honesty line — the permission-neutral contract, stated out loud so
 *  the operator knows a focus profile can only ever make DARWIN quieter. */
function FocusContractNote() {
  return (
    <span>
      A focus profile only quiets / focuses what surfaces — it never loosens a
      gate or enables an action.
    </span>
  );
}

import type { ActionSurface } from "../core/state";
import {
  draftKindLabel,
  missionStatusLabel,
  type DurableMission,
  type MacroEntry,
  type MissionStatus,
  type PendingDraft,
} from "../core/events";
import Frame from "./Frame";

/**
 * ACTION // DRAFTS · MISSIONS · MACROS — the read-only HUD surface for the three
 * OFF-default, gated, wired-live action features (daemon #25 auto-draft, #26
 * durable missions, #27 macro record/replay).
 *
 * SAFETY + HONESTY CONTRACT (do not regress — the panel must SAY these, not just
 * imply them):
 *   - REVIEW-ONLY. There is NO button here that sends a draft, runs/resumes a
 *     mission, or replays a macro. Those are gated actions taken by voice through
 *     the daemon's existing consequential gate — never from this status surface.
 *   - SECRET-FREE. Every field shown is the secret-free subset the daemon emits:
 *     a draft's subject + a BOUNDED preview (NEVER the full body, never a
 *     recipient secret/token); a mission's id/goal/status/progress; a macro's
 *     name + step count + last-replay outcome. The full draft body, a token, a
 *     resolved credential, or a macro's literal secret never reach this panel —
 *     there is no such field on the wire to render.
 *   - #25 drafts: "review & send — DARWIN never auto-sends." A draft is a
 *     suggestion; the draft module has no send path; an actual send is a
 *     SEPARATE explicit action that rides the existing gate.
 *   - #26 missions: "loads paused on restart; steps re-gated." A persisted
 *     mission does NOT auto-run — it loads PAUSED and the user must explicitly
 *     resume; a resumed mission re-gates each consequential step (no pre-approval).
 *   - #27 macros: "replays through the gate each time; stores no secrets." A
 *     macro stores only the recorded intents/utterances; a replay re-runs each
 *     command through the normal router + the gate fresh (no batch bypass).
 *
 * All three SHIP OFF behind their own flags, so the resting state is the honest
 * empty surface (which renders nothing — mirroring the other event-fed panels).
 * The parsers in core/events.ts already drop malformed fields, so this component
 * never has to defend against junk shapes — it renders the bounded, clean data.
 */
export default function ActionPanel({ action }: { action: ActionSurface }) {
  const { drafts, missions, macros } = action;

  // Nothing on any of the three sub-surfaces yet (the shipped-OFF resting state)
  // — render nothing rather than an empty placeholder, like the other event-fed
  // panels. A feature only populates its section once the operator enables it.
  if (drafts.length === 0 && missions.length === 0 && macros.length === 0) {
    return null;
  }

  return (
    <div className="action-panel">
      <Frame title="ACTION // DRAFTS · MISSIONS · MACROS" tag="REVIEW ONLY">
        <div className="act-body">
          {drafts.length > 0 && <DraftsSection drafts={drafts} />}
          {missions.length > 0 && <MissionsSection missions={missions} />}
          {macros.length > 0 && <MacrosSection macros={macros} />}
        </div>
      </Frame>
    </div>
  );
}

/* ---- #25 PENDING DRAFTS --------------------------------------------------- */

function DraftsSection({ drafts }: { drafts: PendingDraft[] }) {
  return (
    <section className="act-section">
      <div className="act-section-head">
        <span className="act-section-title">PENDING DRAFTS</span>
        <span className="act-count">{drafts.length}</span>
      </div>
      <div className="act-list">
        {drafts.map((d) => (
          <DraftRow key={d.id} draft={d} />
        ))}
      </div>
      <div className="act-note dim-note">
        Review &amp; send — DARWIN never auto-sends. A draft is a suggestion you
        review; sending it is a separate, gated action. The full body never
        leaves the device on this surface.
      </div>
    </section>
  );
}

function DraftRow({ draft }: { draft: PendingDraft }) {
  return (
    <div className="act-row act-draft">
      <div className="act-row-head">
        <span className="act-kind">{draftKindLabel(draft.kind)}</span>
        {/* status is hard-pinned to "draft" by the parser — surfaced explicitly
            so a reader can SEE it was never sent. */}
        <span className="act-status draft">DRAFT</span>
        <span className="act-subject">
          {draft.subject || <span className="dim-note">(no subject)</span>}
        </span>
      </div>
      {draft.preview ? (
        <div className="act-preview dim-note" title="bounded preview — never the full body">
          {draft.preview}
        </div>
      ) : null}
    </div>
  );
}

/* ---- #26 DURABLE MISSIONS ------------------------------------------------- */

function MissionsSection({ missions }: { missions: DurableMission[] }) {
  return (
    <section className="act-section">
      <div className="act-section-head">
        <span className="act-section-title">DURABLE MISSIONS</span>
        <span className="act-count">{missions.length}</span>
      </div>
      <div className="act-list">
        {missions.map((m) => (
          <MissionRow key={m.id} mission={m} />
        ))}
      </div>
      <div className="act-note dim-note">
        Loads paused on restart; steps re-gated. A persisted mission never
        auto-runs — it loads PAUSED and you must explicitly resume it, and each
        consequential step re-passes the confirmation gate (the save carries no
        pre-approval).
      </div>
    </section>
  );
}

function MissionRow({ mission }: { mission: DurableMission }) {
  const pct =
    mission.total > 0
      ? Math.min(100, Math.round((mission.done / mission.total) * 100))
      : 0;
  return (
    <div className="act-row act-mission">
      <div className="act-row-head">
        <MissionPill status={mission.status} />
        <span className="act-mission-goal">
          {mission.goal || <span className="dim-note">(no goal)</span>}
        </span>
        <span className="act-mission-id dim-note" title="durable mission id">
          {mission.id}
        </span>
      </div>
      {mission.total > 0 ? (
        <div className="act-progress" aria-label={`${mission.done} of ${mission.total} sub-tasks`}>
          <span className="act-progress-bar" aria-hidden="true">
            <i style={{ width: `${pct}%` }} />
          </span>
          <span className="act-progress-text dim-note">
            {mission.done}/{mission.total} sub-tasks
          </span>
        </div>
      ) : (
        <div className="act-progress-text dim-note">no sub-task breakdown yet</div>
      )}
    </div>
  );
}

/** ALWAYS-honest status pill. PAUSED is the safe default a saved mission loads
 *  as; ACTIVE means the user explicitly resumed it; DONE/CANCELLED are terminal.
 *  Tone vocabulary mirrors the audit panel (amber = attention/paused, green =
 *  ok/active-or-done, ice = inert/cancelled). */
function MissionPill({ status }: { status: MissionStatus }) {
  const tone =
    status === "active"
      ? "active"
      : status === "paused"
        ? "paused"
        : status === "done"
          ? "done"
          : "cancelled";
  return <span className={`act-status ${tone}`}>{missionStatusLabel(status)}</span>;
}

/* ---- #27 MACROS ----------------------------------------------------------- */

function MacrosSection({ macros }: { macros: MacroEntry[] }) {
  return (
    <section className="act-section">
      <div className="act-section-head">
        <span className="act-section-title">MACROS</span>
        <span className="act-count">{macros.length}</span>
      </div>
      <div className="act-list">
        {macros.map((m) => (
          <MacroRow key={m.name} macro={m} />
        ))}
      </div>
      <div className="act-note dim-note">
        Replays through the gate each time; stores no secrets. A macro records
        only the intents/utterances — never a token or credential — and a replay
        re-runs each command through the gate fresh (no batch bypass).
      </div>
    </section>
  );
}

function MacroRow({ macro }: { macro: MacroEntry }) {
  return (
    <div className="act-row act-macro">
      <div className="act-row-head">
        <span className="act-macro-name">{macro.name}</span>
        <span className="act-macro-steps dim-note">
          {macro.steps} step{macro.steps === 1 ? "" : "s"}
        </span>
        <span className={`act-status ${macro.replayPhase}`}>
          {macro.replayPhase === "running"
            ? "REPLAYING"
            : macro.replayPhase === "done"
              ? "REPLAY DONE"
              : "READY"}
        </span>
      </div>
      {macro.replayPhase === "running" && macro.lastStep ? (
        <div className="act-macro-step dim-note" title="recorded intent + utterance — re-gated this step">
          → {macro.lastStep.intent || "(intent)"}
          {macro.lastStep.utterance ? `: ${macro.lastStep.utterance}` : ""}
        </div>
      ) : null}
    </div>
  );
}

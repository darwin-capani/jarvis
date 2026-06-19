import { useCallback, useEffect, useReducer, useState } from "react";
import {
  AUTONOMY_OPTIONS,
  CatalogEntry,
  GROUP_ORDER,
  GroupName,
  dangerousPending,
  entriesForGroup,
  isDangerousChange,
  pendingChanges,
  sameValue,
  valueMapFromStates,
} from "../core/systemSettings";
import {
  Change,
  SettingState,
  SettingValue,
  configGet,
  configSet,
  daemonRestart,
  pickFolder,
} from "../tauri/configSettings";
import type { VoiceIdStatus } from "../core/events";

/**
 * SYSTEM SETTINGS — the dedicated config surface for config/jarvis.toml. On open
 * it reads every whitelisted setting (config_get) and renders the catalog
 * GROUPED, one control per setting bound to its live value. Edits are BATCHED
 * locally (marked "pending") — nothing is written until the explicit
 * "Apply changes — restarts JARVIS" action, which confirms, writes the batch
 * (config_set), then restarts the daemon (daemon_restart) so the new config
 * takes effect. There is no hot-reload; this is the honest model.
 *
 * SAFETY: this surface only EDITS values that ride the backend's strict
 * whitelist; it never touches the runtime gate enforcement. Dangerous changes
 * (master switch OFF, self-heal Full-auto, encrypt-memory ON, shell/ui/mcp ON,
 * cloud tiers ON, screen-context ON) require an explicit in-UI confirm with the
 * warning copy before they can be applied.
 */

/* ------------------------------------------------------------------ state */

interface PanelState {
  /** "loading" until config_get resolves; "ready" once values are in; "error"
   *  on a failed read (plain browser / file missing). */
  phase: "loading" | "ready" | "error";
  /** The honest error message when phase === "error". */
  error: string | null;
  /** The live (on-file) values keyed by id — the baseline pending is diffed
   *  against. Updated to the draft after a successful apply. */
  live: Record<string, SettingValue>;
  /** The locally-drafted values keyed by id — edited as the user changes
   *  controls; never written until Apply. */
  draft: Record<string, SettingValue>;
  /** The confirm overlay phase of the Apply flow, or null when no overlay. */
  confirm: ConfirmState | null;
  /** A transient status line under the footer (apply result / restart detail). */
  status: { kind: "ok" | "warn" | "err"; text: string } | null;
  /** True while config_set + daemon_restart are in flight (buttons disabled). */
  busy: boolean;
}

interface ConfirmState {
  /** The pending change batch this confirm will write. */
  changes: Change[];
  /** The dangerous subset (each shown with its warning copy). */
  dangerous: { entry: CatalogEntry; value: SettingValue }[];
}

type Action =
  | { type: "loaded"; states: SettingState[] }
  | { type: "loadError"; error: string }
  | { type: "edit"; id: string; value: SettingValue }
  | { type: "discard" }
  | { type: "openConfirm" }
  | { type: "closeConfirm" }
  | { type: "applyStart" }
  | { type: "applyDone"; live: Record<string, SettingValue>; status: PanelState["status"] }
  | { type: "applyFail"; status: PanelState["status"] };

function initial(): PanelState {
  return {
    phase: "loading",
    error: null,
    live: {},
    draft: {},
    confirm: null,
    status: null,
    busy: false,
  };
}

function reduce(state: PanelState, action: Action): PanelState {
  switch (action.type) {
    case "loaded": {
      const map = valueMapFromStates(action.states);
      return { ...state, phase: "ready", error: null, live: map, draft: { ...map } };
    }
    case "loadError":
      return { ...state, phase: "error", error: action.error };
    case "edit":
      return { ...state, draft: { ...state.draft, [action.id]: action.value }, status: null };
    case "discard":
      return { ...state, draft: { ...state.live }, confirm: null, status: null };
    case "openConfirm": {
      const changes = pendingChanges(state.live, state.draft);
      if (changes.length === 0) return state;
      return {
        ...state,
        confirm: { changes, dangerous: dangerousPending(changes) },
      };
    }
    case "closeConfirm":
      return { ...state, confirm: null };
    case "applyStart":
      return { ...state, busy: true, status: null };
    case "applyDone":
      return { ...state, busy: false, confirm: null, live: action.live, draft: { ...action.live }, status: action.status };
    case "applyFail":
      return { ...state, busy: false, status: action.status };
    default:
      return state;
  }
}

/* ------------------------------------------------------------------ panel */

export interface SystemSettingsPanelProps {
  /** The live voice-id telemetry the HUD already receives (App.tsx state.voiceId,
   *  threaded through SettingsModal). Used ONLY to render an enrollment BADGE next
   *  to the voice_id.enabled toggle — never to change config. Optional so the
   *  existing render test can mount the panel without telemetry; null = the honest
   *  "telemetry not yet seen" state (the badge reflects enabled + a note). */
  voiceId?: VoiceIdStatus | null;
}

export default function SystemSettingsPanel({
  voiceId = null,
}: SystemSettingsPanelProps) {
  const [state, dispatch] = useReducer(reduce, undefined, initial);

  // On mount, read the live config values into the controls.
  useEffect(() => {
    let cancelled = false;
    configGet()
      .then((states) => {
        if (!cancelled) dispatch({ type: "loaded", states });
      })
      .catch((e: unknown) => {
        if (!cancelled) {
          dispatch({
            type: "loadError",
            error: typeof e === "string" ? e : e instanceof Error ? e.message : "could not read config",
          });
        }
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const onEdit = useCallback((id: string, value: SettingValue) => {
    dispatch({ type: "edit", id, value });
  }, []);

  // Apply the confirmed batch: write the TOML, then restart the daemon. Honest
  // about the restart outcome (the daemon may not be loaded).
  const onApply = useCallback(async () => {
    if (!state.confirm) return;
    const { changes } = state.confirm;
    dispatch({ type: "applyStart" });
    try {
      const nLines = await configSet(changes);
      const restart = await daemonRestart();
      // The new on-file values become the live baseline (the draft already
      // equals them); a fresh config_get is not needed — the batch is exactly
      // what we wrote.
      const nextLive: Record<string, SettingValue> = { ...state.live };
      for (const c of changes) nextLive[c.id] = c.value;
      const wrote = `Wrote ${nLines} line${nLines === 1 ? "" : "s"} to config/jarvis.toml.`;
      dispatch({
        type: "applyDone",
        live: nextLive,
        status: {
          kind: restart.ok ? "ok" : "warn",
          text: `${wrote} ${restart.detail}`,
        },
      });
    } catch (e: unknown) {
      // config_set rejected the whole batch (whitelist/validation) — nothing was
      // written. Surface the backend's honest message.
      dispatch({
        type: "applyFail",
        status: {
          kind: "err",
          text:
            "No changes written. " +
            (typeof e === "string" ? e : e instanceof Error ? e.message : "config_set rejected the batch"),
        },
      });
    }
  }, [state.confirm, state.live]);

  if (state.phase === "loading") {
    return <div className="syscfg-note">Reading config/jarvis.toml…</div>;
  }
  if (state.phase === "error") {
    return (
      <div className="syscfg-note err">
        {state.error}
        <div className="kv-note" style={{ marginTop: 6 }}>
          System Settings edit config/jarvis.toml at the resolved JARVIS root and require the
          JARVIS desktop app. The values shown here load from that file.
        </div>
      </div>
    );
  }

  const changes = pendingChanges(state.live, state.draft);
  const pendingIds = new Set(changes.map((c) => c.id));
  const dangerCount = dangerousPending(changes).length;

  return (
    <div className="syscfg">
      <div className="kv-note syscfg-intro">
        These edit <strong>config/jarvis.toml</strong> only. JARVIS caches its config at startup —
        there is no hot-reload, so edits are <strong>batched</strong> and take effect when you
        <strong> Apply</strong>, which writes the file and <strong>restarts the daemon</strong>.
        The runtime gates (master switch, per-action confirm, voice-id, lockdown, policy) are
        enforced live and are not weakened by anything here.
      </div>

      {GROUP_ORDER.map((group) => (
        <SettingsGroup
          key={group}
          group={group}
          draft={state.draft}
          live={state.live}
          pendingIds={pendingIds}
          onEdit={onEdit}
          voiceId={voiceId}
        />
      ))}

      <div className="syscfg-footer">
        <div className="syscfg-pending-summary" aria-live="polite">
          {changes.length === 0 ? (
            <span className="syscfg-clean">No pending changes</span>
          ) : (
            <span className="syscfg-pending-count">
              {changes.length} pending change{changes.length === 1 ? "" : "s"}
              {dangerCount > 0 && (
                <span className="syscfg-danger-count"> · {dangerCount} need confirmation</span>
              )}
            </span>
          )}
        </div>
        <div className="syscfg-footer-btns">
          <button
            type="button"
            className="icon-btn"
            disabled={changes.length === 0 || state.busy}
            onClick={() => dispatch({ type: "discard" })}
          >
            Discard
          </button>
          <button
            type="button"
            className="icon-btn syscfg-apply"
            disabled={changes.length === 0 || state.busy}
            onClick={() => dispatch({ type: "openConfirm" })}
          >
            Apply changes — restarts JARVIS
          </button>
        </div>
      </div>

      {state.status && (
        <div className={`syscfg-status ${state.status.kind}`} role="status">
          {state.status.text}
        </div>
      )}

      {state.confirm && (
        <ApplyConfirm
          confirm={state.confirm}
          busy={state.busy}
          onCancel={() => dispatch({ type: "closeConfirm" })}
          onConfirm={onApply}
        />
      )}
    </div>
  );
}

/* ------------------------------------------------------------- group block */

function SettingsGroup({
  group,
  draft,
  live,
  pendingIds,
  onEdit,
  voiceId,
}: {
  group: GroupName;
  draft: Record<string, SettingValue>;
  live: Record<string, SettingValue>;
  pendingIds: Set<string>;
  onEdit: (id: string, value: SettingValue) => void;
  voiceId: VoiceIdStatus | null;
}) {
  const entries = entriesForGroup(group);
  return (
    <section className="syscfg-group" aria-label={group}>
      <div className="cred-section-title">{group.toUpperCase()}</div>
      {entries.map((entry) => (
        <SettingRow
          key={entry.id}
          entry={entry}
          draftValue={draft[entry.id]}
          liveValue={live[entry.id]}
          pending={pendingIds.has(entry.id)}
          onEdit={onEdit}
          voiceId={voiceId}
        />
      ))}
    </section>
  );
}

/* --------------------------------------------------------------- one row */

function SettingRow({
  entry,
  draftValue,
  liveValue,
  pending,
  onEdit,
  voiceId,
}: {
  entry: CatalogEntry;
  draftValue: SettingValue | undefined;
  liveValue: SettingValue | undefined;
  pending: boolean;
  onEdit: (id: string, value: SettingValue) => void;
  voiceId: VoiceIdStatus | null;
}) {
  const dangerousNow =
    draftValue !== undefined &&
    isDangerousChange(entry, draftValue) &&
    !sameValue(draftValue, liveValue ?? draftValue);

  return (
    <div className={`syscfg-row${pending ? " pending" : ""}${dangerousNow ? " danger" : ""}`}>
      <div className="syscfg-row-head">
        <span className="syscfg-label">{entry.label}</span>
        {/* The voice-id enrollment BADGE sits next to the voice_id.enabled toggle.
            It is UI-ONLY (reflects telemetry; gates nothing) — see VoiceIdBadge. */}
        {entry.id === "voice_id.enabled" && <VoiceIdBadge voiceId={voiceId} />}
        <span className="syscfg-control">
          <SettingControl entry={entry} value={draftValue} onEdit={onEdit} />
        </span>
        {pending && (
          <span className="syscfg-pending-badge" title="Pending — applied on restart">
            PENDING
          </span>
        )}
      </div>
      <div className="syscfg-hint">{entry.hint}</div>
      {dangerousNow && entry.danger && <div className="syscfg-warn">⚠ {entry.danger}</div>}
    </div>
  );
}

/** The VOICE-ID ENROLLMENT BADGE (GAP 3 — UI-only). It REFLECTS the voice-id
 *  telemetry the HUD already receives; it adds NO authority and offers NO enroll
 *  button (enrollment is the spoken "enroll my voice" flow). Honest copy:
 *    - ENROLLED (green) when a profile is on file.
 *    - NOT ENROLLED (amber) otherwise, naming the spoken enroll phrase.
 *    - When telemetry has not arrived yet (null), an AWAITING note (we say so).
 *  Always reinforces that voice-id gates NOTHING until enrolled, even when On. */
export function VoiceIdBadge({ voiceId }: { voiceId: VoiceIdStatus | null }) {
  if (voiceId === null) {
    return (
      <span
        className="syscfg-vid-badge awaiting"
        title="voice-id telemetry not seen yet — voice-id gates nothing until you enroll, even when On"
      >
        ENROLLMENT — AWAITING TELEMETRY
      </span>
    );
  }
  if (voiceId.enrolled) {
    return (
      <span
        className="syscfg-vid-badge enrolled"
        title="A voice profile is on file. Voice-id gates nothing until you enroll, even when On."
      >
        ENROLLED
      </span>
    );
  }
  return (
    <span
      className="syscfg-vid-badge not-enrolled"
      title="No profile on file. Voice-id gates nothing until you enroll, even when On."
    >
      NOT ENROLLED — say &quot;enroll my voice&quot;
    </span>
  );
}

/* -------------------------------------------------------------- controls */

function SettingControl({
  entry,
  value,
  onEdit,
}: {
  entry: CatalogEntry;
  value: SettingValue | undefined;
  onEdit: (id: string, value: SettingValue) => void;
}) {
  switch (entry.control) {
    case "toggle":
      return <ToggleControl id={entry.id} value={value === true} onEdit={onEdit} />;
    case "autonomy":
      return <AutonomyControl id={entry.id} value={typeof value === "string" ? value : "off"} onEdit={onEdit} />;
    case "select":
      return <SelectControl entry={entry} value={typeof value === "string" ? value : ""} onEdit={onEdit} />;
    case "number":
      return <NumberControl entry={entry} value={typeof value === "number" ? value : NaN} onEdit={onEdit} />;
    case "string":
      return <StringControl entry={entry} value={typeof value === "string" ? value : ""} onEdit={onEdit} />;
    case "strlist":
    case "pathlist":
      return (
        <ListControl
          entry={entry}
          value={Array.isArray(value) ? (value as string[]) : []}
          onEdit={onEdit}
        />
      );
    default:
      return null;
  }
}

/** A freeform single-line text field (a HuggingFace model id). The backend
 *  re-validates (trim, length cap, control-char reject, TOML-escape on write);
 *  empty is the HONEST "feature inert / disabled" value, so we keep it editable
 *  to empty. Trimmed on edit so trailing spaces never read as a pending change. */
function StringControl({
  entry,
  value,
  onEdit,
}: {
  entry: CatalogEntry;
  value: string;
  onEdit: (id: string, value: SettingValue) => void;
}) {
  return (
    <input
      type="text"
      className="syscfg-text-input"
      value={value}
      placeholder={entry.placeholder ?? ""}
      aria-label={entry.label}
      autoComplete="off"
      autoCorrect="off"
      autoCapitalize="off"
      spellCheck={false}
      onChange={(e) => onEdit(entry.id, e.target.value)}
    />
  );
}

/** A string-ARRAY list editor: each current entry on its own row with a Remove
 *  (✕) button, plus an "Add" affordance. For a "pathlist" the add affordance is a
 *  native folder PICKER ("Add folder…") AND a validated manual absolute-path
 *  text-add (the always-works baseline); for a "strlist" it is a manual repo-id
 *  text-add only. All validation is mirrored from the backend (absolute-path
 *  shape, non-empty, no dup) so a bad add is rejected in-UI before it is ever
 *  drafted — and the backend re-validates + TOML-escapes on write regardless. */
function ListControl({
  entry,
  value,
  onEdit,
}: {
  entry: CatalogEntry;
  value: string[];
  onEdit: (id: string, value: SettingValue) => void;
}) {
  const isPaths = entry.control === "pathlist";
  const [text, setText] = useState("");
  const [err, setErr] = useState<string | null>(null);

  // Validate a candidate against the backend's element rules (kept in lockstep
  // with config_settings.rs validate_element): trim, non-empty, no control char,
  // length cap, absolute path for a pathlist, and no duplicate.
  const validate = useCallback(
    (raw: string): { ok: true; value: string } | { ok: false; error: string } => {
      const v = raw.trim();
      if (!v) return { ok: false, error: "enter a value" };
      if (v.length > 200) return { ok: false, error: "too long (max 200 chars)" };
      // Reject any control character (U+0000..U+001F or U+007F) — mirrors the
      // backend's validate_element. Computed by code point, never an embedded raw
      // control byte in the source.
      const hasControl = [...v].some((c) => {
        const code = c.charCodeAt(0);
        return code < 0x20 || code === 0x7f;
      });
      if (hasControl) return { ok: false, error: "contains a control character" };
      if (isPaths && !v.startsWith("/")) return { ok: false, error: "path must be absolute (start with /)" };
      if (value.includes(v)) return { ok: false, error: "already added" };
      return { ok: true, value: v };
    },
    [isPaths, value],
  );

  const add = useCallback(
    (raw: string) => {
      const r = validate(raw);
      if (!r.ok) {
        setErr(r.error);
        return;
      }
      onEdit(entry.id, [...value, r.value]);
      setText("");
      setErr(null);
    },
    [validate, onEdit, entry.id, value],
  );

  const removeAt = useCallback(
    (idx: number) => {
      onEdit(
        entry.id,
        value.filter((_, i) => i !== idx),
      );
      setErr(null);
    },
    [onEdit, entry.id, value],
  );

  // Open the native folder picker (pathlist only). On a real selection the
  // returned absolute path is validated + appended; cancel / no-picker is a
  // silent no-op (the manual input below is the baseline that always works).
  const browse = useCallback(async () => {
    const picked = await pickFolder();
    if (picked) add(picked);
  }, [add]);

  return (
    <div className="syscfg-list">
      {value.length === 0 ? (
        <div className="syscfg-list-empty">None — nothing is indexed until you add one.</div>
      ) : (
        <ul className="syscfg-list-items">
          {value.map((item, idx) => (
            <li key={`${item}-${idx}`} className="syscfg-list-item">
              <code className="syscfg-list-path">{item}</code>
              <button
                type="button"
                className="icon-btn syscfg-list-remove"
                aria-label={`remove ${item}`}
                title="remove"
                onClick={() => removeAt(idx)}
              >
                ✕
              </button>
            </li>
          ))}
        </ul>
      )}

      <div className="syscfg-list-add">
        {isPaths && (
          <button
            type="button"
            className="icon-btn syscfg-list-browse"
            onClick={() => void browse()}
            title="open the native folder picker"
          >
            Add folder…
          </button>
        )}
        <input
          type="text"
          className="syscfg-text-input syscfg-list-input"
          value={text}
          placeholder={entry.placeholder ?? (isPaths ? "/absolute/path" : "model/repo-id")}
          aria-label={`${entry.label} — add entry`}
          autoComplete="off"
          autoCorrect="off"
          autoCapitalize="off"
          spellCheck={false}
          onChange={(e) => {
            setText(e.target.value);
            if (err) setErr(null);
          }}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              add(text);
            }
          }}
        />
        <button
          type="button"
          className="icon-btn syscfg-list-add-btn"
          disabled={text.trim().length === 0}
          onClick={() => add(text)}
        >
          Add
        </button>
      </div>
      {err && <div className="syscfg-list-err">{err}</div>}
    </div>
  );
}

/** A two-state On/Off toggle (the bulk of the catalog). */
function ToggleControl({
  id,
  value,
  onEdit,
}: {
  id: string;
  value: boolean;
  onEdit: (id: string, value: SettingValue) => void;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={value}
      className={`syscfg-toggle ${value ? "on" : "off"}`}
      onClick={() => onEdit(id, !value)}
    >
      <span className="syscfg-toggle-track" aria-hidden="true">
        <span className="syscfg-toggle-knob" />
      </span>
      <span className="syscfg-toggle-text">{value ? "On" : "Off"}</span>
    </button>
  );
}

/** The single 3-way segmented control for self_heal / forge / optimize. */
function AutonomyControl({
  id,
  value,
  onEdit,
}: {
  id: string;
  value: string;
  onEdit: (id: string, value: SettingValue) => void;
}) {
  return (
    <div className="syscfg-seg" role="radiogroup" aria-label={`${id} autonomy mode`}>
      {AUTONOMY_OPTIONS.map((opt) => {
        const active = value === opt.value;
        return (
          <button
            key={opt.value}
            type="button"
            role="radio"
            aria-checked={active}
            className={`syscfg-seg-opt${active ? " active" : ""}${opt.danger ? " danger" : ""}`}
            onClick={() => onEdit(id, opt.value)}
          >
            {opt.label}
          </button>
        );
      })}
    </div>
  );
}

/** An enum select bound to the entry's options. */
function SelectControl({
  entry,
  value,
  onEdit,
}: {
  entry: CatalogEntry;
  value: string;
  onEdit: (id: string, value: SettingValue) => void;
}) {
  // The allowed options come from the live SettingState (passed through the
  // catalog entry's known set); we render the union so an out-of-list live value
  // is still shown honestly rather than silently dropped.
  const options = optionsFor(entry, value);
  return (
    <select
      className="syscfg-select"
      value={value}
      aria-label={entry.label}
      onChange={(e) => onEdit(entry.id, e.target.value)}
    >
      {options.map((opt) => (
        <option key={opt} value={opt}>
          {entry.optionLabels?.[opt] ?? opt}
        </option>
      ))}
    </select>
  );
}

/** The option list for a select: the catalog's known option-label keys, plus the
 *  current value if it is somehow outside that set (shown verbatim, never lost). */
function optionsFor(entry: CatalogEntry, current: string): string[] {
  const known = entry.optionLabels ? Object.keys(entry.optionLabels) : [];
  if (current && !known.includes(current)) {
    return [current, ...known];
  }
  return known;
}

/** A bounded number input. The backend re-validates the range; the UI clamps the
 *  step + shows the unit. Invalid (NaN) keeps the previous value rather than
 *  writing garbage. */
function NumberControl({
  entry,
  value,
  onEdit,
}: {
  entry: CatalogEntry;
  value: number;
  onEdit: (id: string, value: SettingValue) => void;
}) {
  return (
    <span className="syscfg-number">
      <input
        type="number"
        className="syscfg-number-input"
        value={Number.isNaN(value) ? "" : value}
        step={entry.step ?? 1}
        aria-label={entry.label}
        onChange={(e) => {
          const n = entry.step && entry.step < 1 ? parseFloat(e.target.value) : parseInt(e.target.value, 10);
          if (!Number.isNaN(n)) onEdit(entry.id, n);
        }}
      />
      {entry.unit && <span className="syscfg-number-unit">{entry.unit}</span>}
    </span>
  );
}

/* ------------------------------------------------------- apply confirm */

function ApplyConfirm({
  confirm,
  busy,
  onCancel,
  onConfirm,
}: {
  confirm: ConfirmState;
  busy: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  const hasDanger = confirm.dangerous.length > 0;
  return (
    <div className="syscfg-confirm-backdrop" onClick={busy ? undefined : onCancel}>
      <div className="syscfg-confirm" onClick={(e) => e.stopPropagation()} role="dialog" aria-modal="true" aria-label="Confirm apply">
        <div className="syscfg-confirm-title">APPLY {confirm.changes.length} CHANGE{confirm.changes.length === 1 ? "" : "S"} // RESTART JARVIS</div>
        <div className="kv-note">
          This writes config/jarvis.toml in place (comments + structure preserved) and then runs
          <strong> launchctl kickstart com.jarvis.daemon</strong> so the new config takes effect.
          If the daemon is not loaded, the write still lands and the restart reports honestly.
        </div>

        <ul className="syscfg-confirm-list">
          {confirm.changes.map((c) => (
            <li key={c.id} className="syscfg-confirm-item">
              <code>{c.id}</code> → <code>{String(c.value)}</code>
            </li>
          ))}
        </ul>

        {hasDanger && (
          <div className="syscfg-confirm-danger">
            <div className="syscfg-confirm-danger-title">⚠ DANGEROUS CHANGES — READ BEFORE APPLYING</div>
            {confirm.dangerous.map((d) => (
              <div key={d.entry.id} className="syscfg-confirm-danger-item">
                <strong>{d.entry.label}</strong>: {d.entry.danger}
              </div>
            ))}
          </div>
        )}

        <div className="syscfg-confirm-btns">
          <button type="button" className="icon-btn" disabled={busy} onClick={onCancel}>
            Cancel
          </button>
          <button
            type="button"
            className={`icon-btn syscfg-apply${hasDanger ? " danger" : ""}`}
            disabled={busy}
            onClick={onConfirm}
          >
            {busy ? "Applying…" : hasDanger ? "I understand — apply & restart" : "Apply & restart"}
          </button>
        </div>
      </div>
    </div>
  );
}

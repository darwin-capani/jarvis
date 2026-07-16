/**
 * CHANGE QUEUE — pure parser + reducer for the `changeq.list` telemetry frame
 * (daemon/src/changeq.rs). No DOM/React imports, so the frame parsing + the
 * dedup/sort/cap presentation logic are verifiable headlessly under vitest,
 * exactly like core/heal.ts. The ChangeQueuePanel component imports these.
 *
 * The daemon emits `changeq.list` carrying the FULL current pending set of
 * PROPOSE-ONLY proposals (self-heal patches, code diffs, forged apps, routing
 * optimizations) unified into one git-native review lane. The frame is
 * SECRET-FREE by construction: only the kind, the proposal ts, the confined
 * artifact locator, a short summary, the EXISTING apply command, a committed
 * flag + commit sha, and sanitized provenance (agent / model / run / state-hash
 * — never a secret/token) ride the wire. This module parses that shape
 * DEFENSIVELY (a malformed item is dropped, never guessed) and folds it into a
 * stable, deduped, newest-first display list.
 *
 * HONESTY: the panel these feed shows the EXISTING, human-gated apply command
 * per proposal — there is deliberately NO one-click apply (mirroring the
 * heal/forge/code panels). Applying is the human running that re-validating
 * command; nothing here applies anything.
 */

/** The closed vocabulary of propose-only writers the lane unifies. */
export type ChangeqKind = "heal" | "code" | "forge" | "optimize";

/** Every valid kind, in a stable order. */
export const CHANGEQ_KINDS: readonly ChangeqKind[] = ["heal", "code", "forge", "optimize"];

/** How many proposals the panel will display (newest first) — a bounded review
 *  window, matching the daemon's bounded queue posture. */
export const MAX_DISPLAY = 50;

/** The honest, secret-free provenance a proposal carries (never a token). */
export interface ChangeqProvenance {
  /** The agent/component that produced the artifact. */
  agent: string;
  /** The model that authored it (or a component label for a deterministic writer). */
  model: string;
  /** The run id the artifact belongs to. */
  run: string;
  /** A short NON-crypto content fingerprint of the change (never a secret). */
  stateHash: string;
}

/** One pending proposal in the review lane. */
export interface PendingChange {
  /** The queue-assigned monotonic id. */
  seq: number;
  /** Which propose-only writer produced it. */
  kind: ChangeqKind;
  /** The proposal <ts>. */
  ts: number;
  /** The CONFINED artifact locator (repo-root-relative under state/). */
  artifact: string;
  /** A compact, secret-free one-line summary. */
  summary: string;
  /** The EXISTING, re-validating apply command a human runs (never a new authority). */
  applyCommand: string;
  /** Whether it has been mirrored onto the review branch. */
  committed: boolean;
  /** The review-branch commit sha once mirrored (drives git-revert rollback), else null. */
  commit: string | null;
  /** The honest, secret-free provenance. */
  provenance: ChangeqProvenance;
}

/** The parsed change-queue state the panel renders. */
export interface ChangeqState {
  /** The dedicated LOCAL review branch (e.g. "darwin/changeq"). */
  branch: string;
  /** The pending proposals, deduped + newest-first + capped. */
  pending: PendingChange[];
}

/* ----------------------------------------------------------------------- *
 * Defensive field readers — no assumptions about the wire shape.
 * ----------------------------------------------------------------------- */

function asRecord(v: unknown): Record<string, unknown> | null {
  return typeof v === "object" && v !== null && !Array.isArray(v)
    ? (v as Record<string, unknown>)
    : null;
}

function readStr(o: Record<string, unknown>, k: string): string | null {
  const v = o[k];
  return typeof v === "string" ? v : null;
}

function readNum(o: Record<string, unknown>, k: string): number | null {
  const v = o[k];
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

/** True only for one of the four known kinds — an unknown kind is never guessed. */
export function isChangeqKind(v: unknown): v is ChangeqKind {
  return typeof v === "string" && (CHANGEQ_KINDS as readonly string[]).includes(v);
}

/** Parse a provenance object defensively; an absent/garbled field reads as the
 *  honest "unknown" (never fabricated). */
function parseProvenance(v: unknown): ChangeqProvenance {
  const o = asRecord(v) ?? {};
  return {
    agent: readStr(o, "agent") ?? "unknown",
    model: readStr(o, "model") ?? "unknown",
    run: readStr(o, "run") ?? "unknown",
    stateHash: readStr(o, "state_hash") ?? "unknown",
  };
}

/** Parse ONE pending-change item, or `null` when it is malformed (missing a
 *  kind / ts / artifact / apply command) — a bad item is DROPPED, never guessed
 *  into a proposal. */
export function parsePendingChange(v: unknown): PendingChange | null {
  const o = asRecord(v);
  if (o === null) return null;
  const kind = o["kind"];
  const ts = readNum(o, "ts");
  const seq = readNum(o, "seq");
  const artifact = readStr(o, "artifact");
  const applyCommand = readStr(o, "apply_command");
  if (!isChangeqKind(kind) || ts === null || seq === null || artifact === null || applyCommand === null) {
    return null;
  }
  const commit = readStr(o, "commit");
  return {
    seq,
    kind,
    ts,
    artifact,
    summary: readStr(o, "summary") ?? "",
    applyCommand,
    committed: o["committed"] === true,
    commit,
    provenance: parseProvenance(o["provenance"]),
  };
}

/** Parse a `changeq.list` frame's `data` into a ChangeqState, or `null` when the
 *  frame is not a well-formed list (so the reducer keeps the last good state).
 *  Malformed items are dropped; the branch falls back to the default name. */
export function parseChangeqList(data: unknown): ChangeqState | null {
  const o = asRecord(data);
  if (o === null) return null;
  const raw = o["pending"];
  if (!Array.isArray(raw)) return null;
  const pending: PendingChange[] = [];
  for (const item of raw) {
    const parsed = parsePendingChange(item);
    if (parsed !== null) pending.push(parsed);
  }
  return {
    branch: readStr(o, "branch") ?? "darwin/changeq",
    pending,
  };
}

/* ----------------------------------------------------------------------- *
 * Reducer — fold a parsed frame into the presentation state.
 * ----------------------------------------------------------------------- */

/** PURE reducer. A malformed frame (`next === null`) is IGNORED — the last good
 *  state is preserved (a garbled broadcast never blanks the panel). Otherwise
 *  the frame is authoritative: dedup by (kind, ts) keeping the latest, sort
 *  NEWEST-FIRST (by ts, then seq), and cap to MAX_DISPLAY. Returns `null` when
 *  nothing is pending, so the panel renders nothing (mirrors the code/forge/heal
 *  panels' empty posture). */
export function changeqReduce(
  prev: ChangeqState | null,
  next: ChangeqState | null,
): ChangeqState | null {
  if (next === null) return prev;
  const byKey = new Map<string, PendingChange>();
  for (const c of next.pending) {
    byKey.set(`${c.kind}:${c.ts}`, c); // last write wins (dedup)
  }
  const pending = [...byKey.values()]
    .sort((a, b) => b.ts - a.ts || b.seq - a.seq)
    .slice(0, MAX_DISPLAY);
  if (pending.length === 0) return null;
  return { branch: next.branch, pending };
}

/* ----------------------------------------------------------------------- *
 * Small presentation helpers (pure).
 * ----------------------------------------------------------------------- */

/** Human label for a kind. PURE. */
export function kindLabel(kind: ChangeqKind): string {
  switch (kind) {
    case "heal":
      return "SELF-HEAL PATCH";
    case "code":
      return "CODE DIFF";
    case "forge":
      return "FORGED APP";
    case "optimize":
      return "ROUTING TUNING";
    default:
      return String(kind).toUpperCase();
  }
}

/** The count of currently-mirrored (on-branch) proposals in a state. PURE. */
export function mirroredCount(state: ChangeqState | null): number {
  if (state === null) return 0;
  return state.pending.filter((c) => c.committed).length;
}

/** The panel's render gate: it shows the Change Queue ONLY when there is at least
 *  one pending proposal (otherwise it renders nothing, mirroring the code/forge
 *  panels' empty posture). A TYPE GUARD, so a passing check also narrows the state
 *  to non-null for the caller. PURE — the exact predicate the component consults. */
export function hasPending(state: ChangeqState | null): state is ChangeqState {
  return state !== null && state.pending.length > 0;
}

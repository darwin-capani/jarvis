/**
 * Pure state + logic for the COMMAND DECK — the Iron-Man holotable command
 * surface. No DOM/React/three/Tauri imports here so the deck's reducer, the
 * pending-snapshot parser, and the agent-targeting logic are verifiable
 * headlessly under vitest (node env), exactly like state.ts / events.ts.
 *
 * SAFETY POSTURE mirrored from the channel contract:
 *   - The deck adds NO authority. It assembles bounded requests and renders
 *     bounded replies; every consequential action still parks daemon-side, and
 *     the deck surfaces the park prompt — it never fires.
 *   - The pending tray offers Approve/Deny for confirmations (confirm/deny by
 *     id) and Review/Dismiss for a forge proposal. There is DELIBERATELY no
 *     apply/deploy path: the deck shows the manual `scripts/apply_forge.sh <ts>`
 *     command, never a button that runs it.
 *   - Defensive parsing of any reply payload; a malformed snapshot yields an
 *     empty tray, never a throw and never a fabricated card.
 *   - NEVER render or store a secret/token — the bridge strips the token
 *     backend-side, and nothing here reintroduces one.
 */

import type {
  CommandReply,
  PendingConfirmation,
  PendingSnapshot,
} from "../tauri/command";
import { str } from "./events";

/* ------------------------------------------------------------------------ *
 * Agent targeting — the deck lets the operator address a specific agent or  *
 * let Darwin-Prime route. "" (the sentinel) means auto-route.               *
 * ------------------------------------------------------------------------ */

/** The sentinel agent target meaning "let Darwin-Prime route" (no agent ref on
 *  the wire — the daemon resolves to the orchestrator). */
export const AUTO_ROUTE = "";

/** Resolve the optional `agent` field for an `ask` from a deck selection. The
 *  AUTO_ROUTE sentinel (and any blank) yields undefined so the request omits
 *  `agent` and the daemon routes to the orchestrator. PURE. */
export function agentForAsk(selected: string): string | undefined {
  const a = selected.trim();
  return a.length === 0 ? undefined : a;
}

/* ------------------------------------------------------------------------ *
 * Deck transcript — the rolling log of operator commands + agent replies.   *
 * A small ring buffer (newest last), like state.ts's transcript.            *
 * ------------------------------------------------------------------------ */

/** Max deck-log entries kept (ring buffer). */
export const DECK_LOG_CAP = 80;

/** One deck-log line. `kind` styles the row; `text` is the prose (never a
 *  secret — replies are the same prose the user would hear). */
export interface DeckLogEntry {
  /** Monotonic id for React keys (assigned by the reducer). */
  id: number;
  /** "command" = operator input echo; "reply" = agent prose; "error" = a
   *  rejection/failure line; "system" = a deck status note. */
  kind: "command" | "reply" | "error" | "system";
  /** The agent this line is attributed to (for replies/commands), or null. */
  agent: string | null;
  text: string;
}

/** The deck's pure state: the log ring + the latest pending snapshot + a
 *  pending-request flag (so the UI can disable inputs while a command is in
 *  flight) + a monotonic id counter. */
export interface DeckState {
  log: DeckLogEntry[];
  pending: PendingSnapshot;
  /** True while a command round-trip is in flight (UI disables send). */
  busy: boolean;
  nextId: number;
}

export type DeckAction =
  /** Append an operator command echo. */
  | { type: "command"; agent: string | null; text: string }
  /** Append an agent reply. */
  | { type: "reply"; agent: string | null; text: string }
  /** Append an error/rejection line. */
  | { type: "error"; text: string }
  /** Append a deck system note. */
  | { type: "system"; text: string }
  /** Mark a round-trip in flight / done. */
  | { type: "busy"; busy: boolean }
  /** Replace the pending snapshot (from a `pending` reply). */
  | { type: "pending"; snapshot: PendingSnapshot }
  /** Clear the pending confirmation locally (after confirm/deny ack). */
  | { type: "clearConfirmation" }
  /** Clear the pending forge marker locally (after dismiss ack). */
  | { type: "clearForge" };

export function initialDeckState(): DeckState {
  return {
    log: [],
    pending: { confirmation: null, forge_pending_ts: null },
    busy: false,
    nextId: 1,
  };
}

/** Append an entry to the log ring, capping at DECK_LOG_CAP (drop oldest). */
function pushLog(state: DeckState, entry: Omit<DeckLogEntry, "id">): DeckState {
  const withId: DeckLogEntry = { ...entry, id: state.nextId };
  const log = [...state.log, withId];
  const trimmed = log.length > DECK_LOG_CAP ? log.slice(log.length - DECK_LOG_CAP) : log;
  return { ...state, log: trimmed, nextId: state.nextId + 1 };
}

/** PURE deck reducer. Mirrors state.ts's reducer purity so the log ring + the
 *  pending tray state are unit-testable without a DOM. */
export function deckReduce(state: DeckState, action: DeckAction): DeckState {
  switch (action.type) {
    case "command":
      return pushLog(state, { kind: "command", agent: action.agent, text: action.text });
    case "reply":
      return pushLog(state, { kind: "reply", agent: action.agent, text: action.text });
    case "error":
      return pushLog(state, { kind: "error", agent: null, text: action.text });
    case "system":
      return pushLog(state, { kind: "system", agent: null, text: action.text });
    case "busy":
      return { ...state, busy: action.busy };
    case "pending":
      return { ...state, pending: action.snapshot };
    case "clearConfirmation":
      return { ...state, pending: { ...state.pending, confirmation: null } };
    case "clearForge":
      return { ...state, pending: { ...state.pending, forge_pending_ts: null } };
    default:
      return state;
  }
}

/* ------------------------------------------------------------------------ *
 * Defensive parsing of a pending snapshot. The bridge already narrows the   *
 * daemon reply backend-side, but the deck re-narrows what it stores so a     *
 * malformed/partial payload yields an EMPTY tray (no fabricated card),       *
 * never a throw — and so no stray field can ever reach a render.             *
 * ------------------------------------------------------------------------ */

function isPlainObject(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

/** Narrow one untrusted confirmation object, or null unless it has a usable id
 *  (the structural anchor — without an id there is nothing to confirm/deny).
 *  agent/tool/preview default to "" so a partial-but-identified confirmation
 *  still lists. NEVER reads input args (the daemon never sends them here).
 *  Never throws. */
export function parsePendingConfirmation(v: unknown): PendingConfirmation | null {
  if (!isPlainObject(v)) return null;
  const id = str(v, "id");
  if (id === null || id.length === 0) return null;
  return {
    id,
    agent: str(v, "agent") ?? "",
    tool: str(v, "tool") ?? "",
    preview: str(v, "preview") ?? "",
  };
}

/** Coerce a forge ts (string OR number on the wire) to a non-empty string, or
 *  null. The ts is the argument to the manual apply command. Never throws. */
export function parseForgePendingTs(v: unknown): string | null {
  if (typeof v === "string") return v.length > 0 ? v : null;
  if (typeof v === "number" && Number.isFinite(v)) return String(v);
  return null;
}

/** Narrow an untrusted pending payload into a PendingSnapshot. A missing/
 *  malformed confirmation -> null (no card); a missing/malformed forge ts ->
 *  null (no forge row). Never throws on junk; always returns a usable snapshot
 *  (an empty tray for junk). */
export function parsePendingSnapshot(v: unknown): PendingSnapshot {
  if (!isPlainObject(v)) {
    return { confirmation: null, forge_pending_ts: null };
  }
  return {
    confirmation: parsePendingConfirmation(v["confirmation"]),
    forge_pending_ts: parseForgePendingTs(v["forge_pending_ts"]),
  };
}

/** The EXACT manual deploy command for a forge proposal ts — the ONLY install
 *  route. The deck shows this string; it NEVER offers a button that runs it.
 *  PURE. */
export function forgeApplyCommand(ts: string): string {
  return `scripts/apply_forge.sh ${ts}`;
}

/** Does the snapshot have anything to show in the tray? (a parked confirmation
 *  OR a forge proposal). The tray hides entirely when there is nothing pending. */
export function hasPending(snapshot: PendingSnapshot): boolean {
  return Boolean(snapshot.confirmation) || Boolean(snapshot.forge_pending_ts);
}

/* ------------------------------------------------------------------------ *
 * Reply -> deck action mapping. Centralizes how a CommandReply becomes log   *
 * entries + a snapshot update, so the component stays thin and the mapping   *
 * is testable. A failed reply maps to a single error line; a pending reply   *
 * updates the snapshot silently (no log spam from polling).                  *
 * ------------------------------------------------------------------------ */

/** Map a CommandReply to the deck actions it implies. `expectPending` is true
 *  when the originating command was `pending`/`state` (so a snapshot reply
 *  updates the tray rather than logging prose). The replyAgent attributes a
 *  prose reply. NEVER produces a secret-bearing entry — replies are prose only.
 *  PURE. */
export function replyToActions(
  reply: CommandReply,
  opts: { expectPending: boolean; replyAgent: string | null },
): DeckAction[] {
  if (!reply || reply.ok !== true) {
    const error = (reply && typeof reply.error === "string" && reply.error) || "command failed";
    return [{ type: "error", text: error }];
  }
  const actions: DeckAction[] = [];
  if (opts.expectPending && reply.pending) {
    actions.push({ type: "pending", snapshot: parsePendingSnapshot(reply.pending) });
  }
  if (typeof reply.reply === "string" && reply.reply.length > 0) {
    actions.push({ type: "reply", agent: opts.replyAgent, text: reply.reply });
  }
  // An ok reply with neither prose nor a snapshot (e.g. an empty pending result)
  // produces nothing — the caller decides whether to note "nothing pending".
  return actions;
}

/**
 * HUD command-channel bridge — the React-facing wrapper around the Tauri
 * backend's `send_command`. This is the ONLY way the deck talks to the daemon,
 * and it deliberately carries NO token: the capability token lives entirely in
 * the Tauri backend (src-tauri/src/command.rs), which reads it from the daemon's
 * confined handoff file and injects it on the wire. The JS layer never sees it.
 *
 * Every call degrades gracefully when the frontend runs in a plain browser
 * (vite dev) where no Tauri runtime exists — it returns a clean
 * `{ ok:false, error:"no shell" }` rather than throwing, so the deck shows an
 * honest "available in the desktop app" state instead of crashing the HUD.
 */
import { invoke } from "@tauri-apps/api/core";
import { inTauri } from "./bridge";

/** The bounded command verbs the deck may send — the SAME structural allowlist
 *  the Tauri backend and the daemon enforce. There is no apply/deploy verb. */
export type CommandVerb =
  | "ask"
  | "brief"
  | "mission"
  | "roster"
  | "state"
  | "pending"
  | "confirm"
  | "deny"
  | "dismiss_forge"
  // USER-SET-ONLY consequential-policy write: the deck sends the anchored phrase
  // text; the daemon classifies + applies it (NOT via the model tool loop).
  | "policy"
  // Task #12 — the panic/lockdown emergency stop. DEDICATED bare verbs (no fields):
  // `panic` engages the lockdown (the HUD PANIC button), `unlock` lifts it (the
  // deliberate USER-only resume control). Both call lockdown::panic()/unlock()
  // DIRECTLY daemon-side — NOT routed through the model. The PANIC button sends
  // `{cmd:"panic"}`, never `{cmd:"ask"}`, so a panic can never leak to the answer
  // path; the reply carries `locked` so the HUD flips its indicator immediately.
  | "panic"
  | "unlock";

/** The typed request the deck hands the backend. One shape serves all verbs;
 *  per-verb requirements are enforced backend-side (and mirrored in the deck UI
 *  so a disabled button never sends an ill-formed request). NO token field. */
export interface CommandRequest {
  cmd: CommandVerb;
  text?: string;
  agent?: string;
  goal?: string;
  id?: string;
  ts?: number;
}

/** One genuinely-parked confirmation (the `pending` command's confirmation).
 *  Ids + a faithful preview only — NEVER the input args (those stay daemon-side
 *  until an explicit confirm). */
export interface PendingConfirmation {
  id: string;
  agent: string;
  tool: string;
  preview: string;
}

/** The replay-FREE pending snapshot the `pending` command returns. */
export interface PendingSnapshot {
  confirmation?: PendingConfirmation | null;
  /** The forge proposal ts (string), or absent. Surfaced for Review/Dismiss
   *  only — the deck shows the manual apply command, never an apply button. */
  forge_pending_ts?: string | null;
}

/** The reply surfaced to the UI. `ok` mirrors the daemon's `{ok}`; `reply` is
 *  the prose line; `pending` is the snapshot (pending command only); `error` is
 *  the daemon's rejection vocabulary or a backend-local error. NO token/secret
 *  is ever present (the backend strips everything but these fields). */
export interface CommandReply {
  ok: boolean;
  reply?: string | null;
  pending?: PendingSnapshot | null;
  error?: string | null;
  /** Task #12 — the lockdown verdict the daemon attaches to the panic/unlock
   *  replies (`{"locked": is_locked_down()}`), so the HUD flips its LOCKED DOWN /
   *  NORMAL indicator IMMEDIATELY on the button press without waiting for the next
   *  startup snapshot. Absent on every other verb (the backend omits it). */
  locked?: boolean | null;
}

/**
 * Send one bounded command to the daemon via the Tauri backend. In a plain
 * browser (no shell) this never invokes — it returns an honest not-available
 * reply so the deck degrades instead of throwing. The backend is the trust
 * boundary: it validates, injects the token, does the local socket round-trip,
 * and returns the defensively-parsed reply.
 */
export async function sendCommand(request: CommandRequest): Promise<CommandReply> {
  if (!inTauri()) {
    return { ok: false, error: "the command deck requires the DARWIN desktop app" };
  }
  try {
    return await invoke<CommandReply>("send_command", { request });
  } catch (e) {
    // A thrown Tauri error (e.g. a not-well-formed request) becomes a clean
    // UI-facing failure — never an unhandled rejection that blanks the HUD. The
    // message is backend-authored guidance, never a secret.
    return { ok: false, error: typeof e === "string" ? e : "command failed" };
  }
}

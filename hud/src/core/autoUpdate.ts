/* ======================================================================== *
 * AUTO-UPDATE — the pure, DOM-free core for the launch update flow.          *
 *                                                                            *
 * This module owns three honesty-critical, unit-testable pieces with NO      *
 * React / DOM / Tauri imports (so they run in the node vitest env exactly    *
 * like the reducer/parsers/onboarding flag):                                 *
 *                                                                            *
 *  1. The persisted AUTO-UPDATE preference (localStorage key                 *
 *     "jarvis.autoUpdate") — mirrors onboarding.ts's once-only flag pattern  *
 *     (versioned key, fixed literal, fail-safe reads/writes).                *
 *                                                                            *
 *  2. `decideLaunchUpdateAction(check, autoOn)` — the LAUNCH BRANCH. THE      *
 *     CARDINAL HONESTY RULE LIVES HERE: a dialog or a silent install may be  *
 *     produced ONLY when the backend reports status === "available" (a REAL  *
 *     newer signed version). EVERY other status (not_configured, up_to_date, *
 *     error, installed, unavailable, or anything unrecognised) maps to       *
 *     `{ kind: "none" }` — no dialog, no nag, exactly today's quiet launch.  *
 *     It can never fabricate an update: the only inputs are the backend's    *
 *     own status + the local pref.                                           *
 *                                                                            *
 *  3. `updateDialogReduce` — the dialog's install state machine (idle ->     *
 *     installing -> installed | error). Pure, so the three-button wiring +   *
 *     the disabled/Installing…/honest-error states can be asserted without a *
 *     DOM.                                                                    *
 *                                                                            *
 * The install authority is UNCHANGED: nothing here downloads or installs.    *
 * The component calls the EXISTING signed backend command                    *
 * (`checkForUpdates(true)`), which verifies the minisign signature against   *
 * the owner pubkey. This module only decides WHEN the dialog/silent path is  *
 * reached and tracks the resulting status.                                   *
 * ======================================================================== */

import type { UpdateCheck } from "../tauri/bridge";

/* --- the persisted auto-update preference (DOM-free, fail-safe) ----------- */

/** localStorage key for the "auto-install updates on launch" preference.
 *  Versioned so a future change of meaning can ship by bumping the suffix
 *  without colliding with an already-set v1 (same discipline as
 *  ONBOARDING_SEEN_KEY). */
export const AUTO_UPDATE_KEY = "jarvis.autoUpdate.v1";

/** The value written when auto-update is ON. A presence-equality check is
 *  enough; the literal is fixed so a test can assert it exactly. Anything else
 *  (absent, "0", garbage) reads as OFF. */
export const AUTO_UPDATE_ON_VALUE = "1";

/** Minimal storage shape so the pref helpers can be unit-tested with an
 *  in-memory stub and degrade safely when localStorage is unavailable
 *  (no-DOM/SSR/vitest-node). Mirrors OnboardingStorage but ALSO needs
 *  removeItem so turning the pref OFF clears the key (rather than leaving a
 *  stale "0" around). */
export interface AutoUpdateStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

/** The real browser localStorage when present, else null. Never throws (a
 *  privacy-mode / sandboxed context can throw on access). */
function defaultStorage(): AutoUpdateStorage | null {
  try {
    if (typeof localStorage === "undefined") return null;
    return localStorage;
  } catch {
    return null;
  }
}

/** Is auto-install-on-launch turned ON?
 *
 *  FAIL-SAFE TOWARD OFF (the default + the safer posture): if storage is
 *  unavailable OR the read throws, we return FALSE — so a context that cannot
 *  remember the pref shows the dialog (a deliberate per-launch choice) rather
 *  than silently auto-installing. OFF is also the shipped default, so a fresh
 *  install reads false. */
export function isAutoUpdateOn(storage: AutoUpdateStorage | null = defaultStorage()): boolean {
  if (storage === null) return false;
  try {
    return storage.getItem(AUTO_UPDATE_KEY) === AUTO_UPDATE_ON_VALUE;
  } catch {
    return false;
  }
}

/** Persist the auto-update preference. ON writes the fixed literal; OFF removes
 *  the key entirely (so "don't ask again" is fully undoable and never leaves a
 *  stale value). Idempotent; never throws (a failed write just means the pref
 *  may not stick — the launch path then shows the dialog, which is safe, never
 *  a surprise install). */
export function setAutoUpdateOn(
  on: boolean,
  storage: AutoUpdateStorage | null = defaultStorage(),
): void {
  if (storage === null) return;
  try {
    if (on) storage.setItem(AUTO_UPDATE_KEY, AUTO_UPDATE_ON_VALUE);
    else storage.removeItem(AUTO_UPDATE_KEY);
  } catch {
    /* a failed persist is non-fatal — see isAutoUpdateOn's fail-safe note */
  }
}

/* --- the launch branch (THE honesty gate) --------------------------------- */

/** What the launch auto-check should do with a check result.
 *   - "none"   : do NOTHING visible (no dialog, no nag). The ONLY outcome for
 *                every status that is not "available".
 *   - "dialog" : open the UpdateDialog for `version` (pref OFF + available).
 *   - "silent" : auto-install `version` with a brief honest notice, no dialog
 *                (pref ON + available). */
export type LaunchUpdateAction =
  | { kind: "none" }
  | { kind: "dialog"; version: string }
  | { kind: "silent"; version: string };

/**
 * THE CARDINAL HONESTY RULE, in one pure function.
 *
 * A dialog or a silent install is produced ONLY when `check.status` is exactly
 * "available" — a REAL newer signed version the updater reported (and whose
 * signature the install path verifies). For ANY other status —
 * "not_configured", "up_to_date", "error", "installed", "unavailable", or an
 * unrecognised value — this returns `{ kind: "none" }`. There is no input that
 * can fabricate an update: the only signals are the backend's own status and
 * the local pref. When available, the pref decides silent (ON) vs dialog (OFF).
 *
 * Defensive: an "available" status with no usable version string also maps to
 * "none" — we never open a dialog that cannot honestly name the version.
 */
export function decideLaunchUpdateAction(
  check: UpdateCheck,
  autoOn: boolean,
): LaunchUpdateAction {
  if (check.status !== "available") return { kind: "none" };
  const version = (check.version ?? "").trim();
  if (version === "") return { kind: "none" };
  return autoOn ? { kind: "silent", version } : { kind: "dialog", version };
}

/* --- the dialog install state machine (pure) ------------------------------ */

/** The dialog's install phase.
 *   - "idle"       : the three buttons are live; nothing in flight.
 *   - "installing" : checkForUpdates(true) is running — buttons disabled,
 *                    "Installing…" shown. NEVER claims success.
 *   - "installed"  : the backend reported status "installed" (downloaded,
 *                    signature-verified, installed) — offer relaunch / restart.
 *   - "error"      : an install attempt failed — show the honest detail and let
 *                    the user retry or cancel. NEVER claims success. */
export type UpdateDialogPhase = "idle" | "installing" | "installed" | "error";

export interface UpdateDialogState {
  phase: UpdateDialogPhase;
  /** Honest detail line for the "error" phase (the backend's message), or for
   *  "installed" (the success/relaunch line). Empty in idle/installing. */
  detail: string;
}

export type UpdateDialogEvent =
  | { type: "installStart" }
  /** The resolved result of checkForUpdates(true). Only status "installed" is
   *  treated as success; ANYTHING else is surfaced as an honest error (we never
   *  claim success on a non-"installed" result). */
  | { type: "installResult"; result: UpdateCheck };

export function updateDialogInitial(): UpdateDialogState {
  return { phase: "idle", detail: "" };
}

/** Pure transition for the install flow. Honest by construction: only a backend
 *  result with status === "installed" reaches the "installed" phase; every
 *  other resolved status (error/up_to_date/not_configured/unavailable/…) lands
 *  in "error" with the backend's own detail, so the dialog can never claim a
 *  success that did not happen. */
export function updateDialogReduce(
  state: UpdateDialogState,
  event: UpdateDialogEvent,
): UpdateDialogState {
  switch (event.type) {
    case "installStart":
      return { phase: "installing", detail: "" };
    case "installResult": {
      const r = event.result;
      if (r.status === "installed") {
        return {
          phase: "installed",
          detail:
            r.detail || "Update installed and signature-verified — restart to finish updating.",
        };
      }
      return {
        phase: "error",
        detail: r.detail || "The update could not be installed.",
      };
    }
    default:
      return state;
  }
}

/** The honest, non-blocking launch notice shown when the pref is ON and a
 *  silent install begins — so an auto-install is never a silent surprise. */
export function silentUpdateNotice(version: string): string {
  return `Updating JARVIS to ${version}…`;
}

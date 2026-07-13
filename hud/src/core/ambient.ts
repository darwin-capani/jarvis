/* ------------------------------------------------------------------------ *
 * AT-REST / AMBIENT MODE — when you step away, the dense HUD calms into a      *
 * glanceable "mirror" (big clock, presence, a few key stats). Driven by the    *
 * daemon-fused presence signal (F5): the daemon already decides Away/Present/  *
 * Focused, so the HUD doesn't re-implement fragile idle heuristics. Any LOCAL  *
 * pointer/key activity instantly wins and keeps the full HUD awake — so the    *
 * mirror never appears while you're actually touching the machine.            *
 * ------------------------------------------------------------------------ */

import type { PresenceState } from "./events";

/** Default grace period after the last local pointer/key activity before the
 *  HUD is allowed to go at-rest, even once the daemon reports "away". Keeps the
 *  mirror from flashing in during a brief pause. */
export const AT_REST_IDLE_MS = 20_000;

/** Whether the HUD should be in the calm at-rest/ambient mirror.
 *
 *  PURE. At-rest requires BOTH: the daemon fused presence is "away" (the
 *  authoritative "not at the machine" signal), AND no local pointer/key activity
 *  within `idleMs`. Local activity always wins — a present/focused user, or one
 *  who just moved the mouse, sees the full HUD. A null presence (no frame yet)
 *  is never at-rest (the HUD claims nothing until the daemon speaks). */
export function isAtRest(
  presence: PresenceState | null,
  lastLocalActivityMs: number,
  nowMs: number,
  idleMs: number = AT_REST_IDLE_MS,
): boolean {
  if (presence !== "away") return false;
  return nowMs - lastLocalActivityMs >= idleMs;
}

/**
 * FOCUS-TRAP core (a11y): the PURE next-focus arithmetic for modal dialogs,
 * plus the shared focusable-element selector. The DOM side lives in the
 * [`useModalFocus`] hook; this module stays node-testable (no DOM).
 *
 * Contract (mirrors the Cmd-K palette's behavior, generalized to dialogs with
 * MANY focusables): Tab cycles forward through the dialog's focusable elements
 * and WRAPS at the end; Shift+Tab cycles backward and wraps at the start; focus
 * can never tab out into the obscured HUD behind the modal.
 */

/**
 * The elements a modal trap cycles through. Matches the interactive controls
 * the HUD actually renders (buttons, inputs, selects, textareas, links, and
 * explicitly-tabbable elements) while excluding disabled controls and anything
 * opted out with tabindex="-1".
 */
export const FOCUSABLE_SELECTOR = [
  "a[href]",
  "button:not([disabled])",
  "input:not([disabled])",
  "select:not([disabled])",
  "textarea:not([disabled])",
  '[tabindex]:not([tabindex="-1"])',
].join(", ");

/**
 * PURE: the index a Tab / Shift+Tab press should focus next, over `count`
 * focusable elements with focus currently at `current` (`-1` = focus is
 * nowhere inside the trap, e.g. it landed on the container itself).
 *
 *   - No focusables (`count <= 0`) -> -1 (the caller keeps focus where it is).
 *   - From outside (`current === -1`): Tab enters at the FIRST element,
 *     Shift+Tab enters at the LAST (the same order a browser would enter).
 *   - Otherwise: one step forward/backward, WRAPPING at both ends.
 */
export function nextTrapIndex(count: number, current: number, shiftKey: boolean): number {
  if (count <= 0) return -1;
  if (current < 0 || current >= count) return shiftKey ? count - 1 : 0;
  return (current + (shiftKey ? -1 : 1) + count) % count;
}

import { useEffect, useRef, type RefObject } from "react";
import { FOCUSABLE_SELECTOR, nextTrapIndex } from "../core/focusTrap";

/**
 * MODAL FOCUS MANAGEMENT (a11y): the one hook every HUD dialog uses so that
 * `aria-modal="true"` is TRUE and not cosmetic. On mount it (1) remembers the
 * element that had focus (the opener), (2) moves focus INTO the dialog (first
 * focusable control, else the container itself), (3) TRAPS Tab / Shift+Tab in a
 * wrapping cycle via the pure [`nextTrapIndex`], and (4) on unmount RESTORES
 * focus to the opener — so a keyboard/screen-reader user is never left focused
 * on the obscured HUD behind a modal, and never stranded after it closes.
 *
 * `onEscape` (optional): close the dialog on Escape. Pass it ONLY when the
 * dialog has no OTHER Escape path (some dialogs already close via a
 * window-level listener; wiring both would double-fire — the #109 palette
 * lesson). Pass `undefined` while a dialog is busy to make Escape inert,
 * mirroring the busy-aware backdrop-click convention.
 *
 * SSR-safe: effects never run under renderToStaticMarkup, so the static a11y
 * tests exercise markup only and this hook is inert there.
 */
export default function useModalFocus(
  ref: RefObject<HTMLElement | null>,
  onEscape?: (() => void) | undefined,
  /** For dialogs that stay MOUNTED while closed (they render `null` on an
   *  `open` prop, e.g. the Command Deck): pass `open` so the trap engages when
   *  the dialog actually appears. Conditionally-mounted dialogs omit it. */
  active: boolean = true,
) {
  // The latest onEscape without re-running the trap effect (re-running would
  // re-autofocus mid-session every time a busy flag flips the callback).
  const onEscapeRef = useRef(onEscape);
  onEscapeRef.current = onEscape;

  useEffect(() => {
    if (!active) return;
    const container = ref.current;
    if (!container) return;

    const opener = document.activeElement;

    // Focusables inside the dialog, in DOM order, skipping invisible ones
    // (collapsed tabs/sections render nothing focusable-by-eye).
    const focusables = (): HTMLElement[] =>
      Array.from(container.querySelectorAll<HTMLElement>(FOCUSABLE_SELECTOR)).filter(
        (el) => el.getClientRects().length > 0,
      );

    // (2) Autofocus: first control, else the container itself (made
    // programmatically focusable so focus genuinely ENTERS the dialog).
    const first = focusables()[0];
    if (first) {
      first.focus();
    } else {
      container.tabIndex = -1;
      container.focus();
    }

    // (3) Trap Tab + handle Escape while focus is inside the dialog (the trap
    // itself guarantees it stays inside).
    const onKeyDown = (ev: KeyboardEvent) => {
      if (ev.key === "Escape") {
        const close = onEscapeRef.current;
        if (close) {
          ev.preventDefault();
          close();
        }
        return;
      }
      if (ev.key !== "Tab") return;
      ev.preventDefault();
      const els = focusables();
      const current = els.indexOf(document.activeElement as HTMLElement);
      const next = nextTrapIndex(els.length, current, ev.shiftKey);
      if (next >= 0) els[next].focus();
    };
    container.addEventListener("keydown", onKeyDown);

    // (4) Restore the opener's focus when the dialog goes away.
    return () => {
      container.removeEventListener("keydown", onKeyDown);
      if (opener instanceof HTMLElement && opener.isConnected) opener.focus();
    };
    // Runs for the dialog's VISIBLE lifetime: conditionally-mounted dialogs
    // mount/unmount (active stays true), always-mounted ones flip `active`.
  }, [active, ref]);
}

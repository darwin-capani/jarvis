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
  /** CSS selector for the control that should receive the INITIAL focus when
   *  the dialog's natural first focusable is the wrong landing spot (e.g. a ✕
   *  close button ahead of the primary input — Space/Enter would instantly
   *  close). Falls back to the first focusable when absent/not found. */
  initialFocus?: string,
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

    // (2) Autofocus: the designated initial control when given, else the first
    // control, else the container itself (made programmatically focusable so
    // focus genuinely ENTERS the dialog).
    const designated = initialFocus
      ? container.querySelector<HTMLElement>(initialFocus)
      : null;
    const first = (designated && designated.getClientRects().length > 0 ? designated : null) ?? focusables()[0];
    if (first) {
      first.focus();
    } else {
      container.tabIndex = -1;
      container.focus();
    }

    // (3) Trap Tab + handle Escape while focus is inside the dialog (the trap
    // itself guarantees it stays inside). A handled key is STOPPED as well as
    // prevented: with NESTED dialogs (ApplyConfirm inside SettingsModal) both
    // containers hold this listener on the same bubble path — without
    // stopPropagation the OUTER trap re-fires on the same keydown and yanks
    // focus out of the inner dialog (and a handled Escape would bubble on to a
    // window-level close listener and tear down the whole outer modal). The
    // innermost dialog under the event owns the key; an UNHANDLED Escape (no
    // onEscape) still bubbles so an outer/window listener can close.
    const onKeyDown = (ev: KeyboardEvent) => {
      if (ev.key === "Escape") {
        const close = onEscapeRef.current;
        if (close) {
          ev.preventDefault();
          ev.stopPropagation();
          close();
        }
        return;
      }
      if (ev.key !== "Tab") return;
      ev.preventDefault();
      ev.stopPropagation();
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
  }, [active, ref, initialFocus]);
}

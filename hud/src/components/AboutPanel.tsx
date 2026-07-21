import { useCallback, useRef, useState } from "react";
import { checkForUpdates } from "../tauri/bridge";
import { aboutCheckView, type AboutCheckView } from "../core/about";
import useModalFocus from "./useModalFocus";

/* ======================================================================== *
 * ABOUT PANEL — the custom "About D.A.R.W.I.N." panel.                       *
 *                                                                            *
 * WHY CUSTOM: the macOS standard about panel can only show static text, so   *
 * it cannot host a working "Check for Updates" button. The Rust shell        *
 * replaces the system "About" menu item with one that emits `menu://about`;  *
 * App listens for it and mounts this panel. Layout, top to bottom:           *
 *   D.A.R.W.I.N.  ·  Version <x>  ·  [ Check for Updates ]  ·  status line    *
 *   ·  made by darwin capani                                                  *
 * (the user's ask: the Check-for-Updates button directly under the version,  *
 *  and the "made by darwin capani" credit directly under that button).       *
 *                                                                            *
 * HONESTY: the button runs the SAME backend check the launch auto-check uses *
 * (checkForUpdates(false) — a CHECK, never a silent install). On "up to      *
 * date" it shows the reassuring latest line; on a REAL available version it  *
 * routes to the EXISTING signed UpdateDialog via onUpdateAvailable (download *
 * + minisign verify + install live there) — this panel adds NO install       *
 * authority. Any other status surfaces the backend's honest detail.          *
 * ======================================================================== */

export interface AboutPanelProps {
  /** The app version (from the native menu event payload, e.g. "1.0.0"). */
  version: string;
  /** Close the panel (backdrop click / Close). */
  onClose: () => void;
  /** A REAL newer signed version was found — hand off to the signed
   *  UpdateDialog (App closes this panel and opens it). Never called unless the
   *  backend returned status "available" WITH a version. */
  onUpdateAvailable: (version: string) => void;
}

export default function AboutPanel({ version, onClose, onUpdateAvailable }: AboutPanelProps) {
  const [busy, setBusy] = useState(false);
  const [view, setView] = useState<AboutCheckView | null>(null);

  // "Check for Updates" — the honest backend CHECK (no install). On a real
  // available version we route to the signed UpdateDialog; otherwise we show
  // the honest line inline (latest / error / not-configured).
  const onCheck = useCallback(async () => {
    if (busy) return;
    setBusy(true);
    setView(null);
    const result = await checkForUpdates(false);
    const v = aboutCheckView(result);
    setBusy(false);
    if (v.kind === "available" && v.version) {
      onUpdateAvailable(v.version);
      return;
    }
    setView(v);
  }, [busy, onUpdateAvailable]);

  // a11y: trap + autofocus + focus-restore; Escape closes, busy-aware like
  // the backdrop click (inert mid-check so a check is never orphaned).
  const modalRef = useRef<HTMLDivElement>(null);
  useModalFocus(modalRef, busy ? undefined : onClose);

  return (
    <div className="syscfg-confirm-backdrop about-backdrop" onClick={busy ? undefined : onClose}>
      <div
        className="syscfg-confirm about-panel"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-label="About D.A.R.W.I.N."
        ref={modalRef}
      >
        {/* A compact, self-contained reactor glyph (no CoreScene dependency). */}
        <div className="about-orb" aria-hidden="true">
          <span className="about-orb-ring" />
          <span className="about-orb-core" />
        </div>

        <div className="about-name">D.A.R.W.I.N.</div>
        <div className="about-version">Version {version}</div>

        {/* Right under the version — the working Check-for-Updates button. */}
        <button
          type="button"
          className="icon-btn syscfg-apply about-check-btn"
          disabled={busy}
          onClick={() => void onCheck()}
          title="Check GitHub for a newer signature-verified release"
        >
          {busy ? "Checking…" : "Check for Updates"}
        </button>

        {view && (
          <div
            className={`about-status ${view.kind === "error" ? "err" : "ok"}`}
            role="status"
          >
            {view.text}
          </div>
        )}

        {/* Directly under the button — the credit. */}
        <div className="about-credit">made by darwin capani</div>
      </div>
    </div>
  );
}

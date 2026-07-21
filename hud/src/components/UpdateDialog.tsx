import { useCallback, useReducer, useRef, useState } from "react";
import { checkForUpdates, relaunchApp } from "../tauri/bridge";
import {
  setAutoUpdateOn,
  updateDialogInitial,
  updateDialogReduce,
} from "../core/autoUpdate";
import useModalFocus from "./useModalFocus";

/* ======================================================================== *
 * UPDATE DIALOG — the launch "Update available" modal.                       *
 *                                                                            *
 * HONESTY CONTRACT (do not regress): this dialog is mounted by App ONLY when *
 * the launch auto-check returned status === "available" (a REAL newer signed *
 * version) AND the auto-update pref is OFF. App owns that gate                *
 * (decideLaunchUpdateAction). The dialog itself NEVER decides an update       *
 * exists and NEVER invents a version — it is handed the version string the    *
 * backend reported.                                                          *
 *                                                                            *
 * The install path goes through the EXISTING signed backend command          *
 * `checkForUpdates(true)`, which downloads, VERIFIES the minisign signature   *
 * against the owner's public key, and installs. This component adds NO new    *
 * install authority and never claims success unless the backend returns       *
 * status "installed" (see updateDialogReduce). On success it offers a         *
 * relaunch via the built-in restart (relaunchApp); if relaunch is             *
 * unavailable (no shell) it falls back to an honest "Restart to finish        *
 * updating" line rather than pretending.                                      *
 *                                                                            *
 * THREE buttons:                                                             *
 *   - "Update"                 -> install + relaunch.                         *
 *   - "Update & don't ask again" -> persist pref = ON, THEN install +         *
 *                                  relaunch (future launches auto-install).   *
 *   - "Cancel"                 -> close; pref UNCHANGED (re-checks next        *
 *                                  launch).                                   *
 * ======================================================================== */

export interface UpdateDialogProps {
  /** The version the backend reported available (App passes result.version). */
  version: string;
  /** Close the dialog (Cancel, or after a relaunch dispatch). Does NOT change
   *  the preference — that is only changed by the "don't ask again" button. */
  onClose: () => void;
}

export default function UpdateDialog({ version, onClose }: UpdateDialogProps) {
  const [state, dispatch] = useReducer(updateDialogReduce, undefined, updateDialogInitial);
  // True once a successful install has been relaunched-attempted but the shell
  // could not relaunch (browser/no-shell) — we then ask the user to restart.
  const [needsManualRestart, setNeedsManualRestart] = useState(false);

  const busy = state.phase === "installing";

  // The shared install+relaunch path used by BOTH "Update" and
  // "Update & don't ask again". It calls the EXISTING signed backend command
  // (download + minisign verify + install). On the honest "installed" result it
  // attempts the built-in relaunch; if that is unavailable it surfaces the
  // honest "restart to finish" state. Any other result is an honest error
  // (the reducer never treats a non-"installed" status as success).
  const installAndRelaunch = useCallback(async () => {
    if (busy) return;
    dispatch({ type: "installStart" });
    const result = await checkForUpdates(true);
    dispatch({ type: "installResult", result });
    if (result.status === "installed") {
      const relaunched = await relaunchApp();
      if (relaunched) {
        // restart() replaces the process; if we are still here, close cleanly.
        onClose();
      } else {
        // No shell to relaunch (or relaunch failed) — be honest, ask the user
        // to restart. We do NOT claim the update is "finished".
        setNeedsManualRestart(true);
      }
    }
    // On a non-"installed" result the reducer is already in "error" with the
    // backend's honest detail; the buttons re-enable for retry/cancel.
  }, [busy, onClose]);

  // "Update" — install + relaunch. Pref UNCHANGED.
  const onUpdate = useCallback(() => {
    void installAndRelaunch();
  }, [installAndRelaunch]);

  // "Update & don't ask again" — FIRST persist the pref = ON (so future
  // launches auto-install without this dialog), THEN the same install+relaunch.
  const onUpdateDontAsk = useCallback(() => {
    setAutoUpdateOn(true);
    void installAndRelaunch();
  }, [installAndRelaunch]);

  // "Cancel" — just close. Pref is NOT touched, so the next launch re-checks
  // and (if still available) shows this dialog again.
  const onCancel = useCallback(() => {
    if (busy) return;
    onClose();
  }, [busy, onClose]);

  const installed = state.phase === "installed";
  const errored = state.phase === "error";

  // a11y: trap + autofocus + focus-restore; Escape = Cancel (onCancel is
  // already busy-aware, so Escape is inert mid-install like the backdrop).
  const modalRef = useRef<HTMLDivElement>(null);
  useModalFocus(modalRef, onCancel);

  return (
    <div
      className="syscfg-confirm-backdrop"
      onClick={busy ? undefined : onCancel}
    >
      <div
        className="syscfg-confirm update-dialog"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-label="Update available"
        ref={modalRef}
      >
        <div className="syscfg-confirm-title">UPDATE AVAILABLE</div>

        <div className="kv-note">
          <strong>DARWIN {version} is available.</strong>
          <div style={{ marginTop: 6 }}>
            Downloaded updates are signature-verified before installing.
          </div>
        </div>

        {/* Honest status line — only shown for installing / installed / error.
            It NEVER claims success unless the backend returned "installed". */}
        {busy && (
          <div className="syscfg-status" role="status">
            Installing…
          </div>
        )}
        {installed && (
          <div className="syscfg-status ok" role="status">
            {needsManualRestart
              ? "Update installed and signature-verified. Restart DARWIN to finish updating."
              : state.detail}
          </div>
        )}
        {errored && (
          <div className="syscfg-status err" role="status">
            {state.detail}
          </div>
        )}

        <div className="syscfg-confirm-btns">
          {installed && needsManualRestart ? (
            // Honest terminal state when we could not relaunch for the user.
            <button type="button" className="icon-btn" onClick={onClose}>
              Close
            </button>
          ) : (
            <>
              <button
                type="button"
                className="icon-btn"
                disabled={busy}
                onClick={onCancel}
              >
                Cancel
              </button>
              <button
                type="button"
                className="icon-btn"
                disabled={busy}
                onClick={onUpdateDontAsk}
                title="Install this update and automatically install signature-verified updates on future launches"
              >
                {busy ? "Installing…" : "Update & don't ask again"}
              </button>
              <button
                type="button"
                className="icon-btn syscfg-apply"
                disabled={busy}
                onClick={onUpdate}
                title="Download, verify the signature, install, and relaunch"
              >
                {busy ? "Installing…" : errored ? "Retry update" : "Update"}
              </button>
            </>
          )}
        </div>
      </div>
    </div>
  );
}

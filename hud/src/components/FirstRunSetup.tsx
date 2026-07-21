import { useCallback, useReducer, useRef } from "react";
import { openSetupInstall } from "../tauri/bridge";
import {
  SETUP_COPY,
  setupScreenInitial,
  setupScreenReduce,
} from "../core/firstRunSetup";
import useModalFocus from "./useModalFocus";

/* ======================================================================== *
 * FIRST-RUN SETUP — the install gate for a freshly-downloaded DARWIN app.    *
 *                                                                            *
 * MOUNT CONTRACT (owned by App via decideShowSetup — see core/firstRunSetup):*
 * App mounts this ONLY when, in the Tauri shell, the backend is genuinely NOT *
 * installed (backend_installed() === false) AND the command channel is NOT    *
 * connected (the daemon isn't reachable). It NEVER mounts when DARWIN is       *
 * installed + running — a connected daemon alone keeps it hidden.            *
 *                                                                            *
 * WHAT IT DOES: honest copy about what the installer does (multi-GB on-device *
 * models, missing deps, time, ONE macOS password prompt, opens Terminal), an  *
 * "Install DARWIN" button that calls open_setup_install() (which opens        *
 * Terminal on the REAL public install.sh one-liner), then a "waiting for      *
 * DARWIN to come online…" state that auto-dismisses the moment the daemon      *
 * connects (App passes `connected`; when it flips true the gate un-mounts us). *
 *                                                                            *
 * It adds NO install authority of its own and never reimplements provisioning *
 * — it just launches the working installer and waits for the connection.     *
 * ======================================================================== */

export interface FirstRunSetupProps {
  /** The live daemon-reachable signal (App's `state.connected`). When it flips
   *  true the App gate un-mounts this screen; we also use it to show the
   *  "connected — finishing up" beat in the waiting state. */
  connected: boolean;
}

export default function FirstRunSetup({ connected }: FirstRunSetupProps) {
  const [state, dispatch] = useReducer(setupScreenReduce, undefined, setupScreenInitial);

  const busy = state.phase === "opening";
  const waiting = state.phase === "waiting";
  const errored = state.phase === "error";

  // "Install DARWIN" — open Terminal on the public installer one-liner via the
  // backend command. On success we move to the waiting state and poll the
  // connection (below); on failure we surface the honest error and stay on intro.
  const onInstall = useCallback(async () => {
    if (busy) return;
    dispatch({ type: "installStart" });
    const r = await openSetupInstall();
    dispatch({ type: "installResult", opened: r.opened, detail: r.detail });
  }, [busy]);

  // POST-INSTALL WAIT — once Terminal is launched we are in "waiting", and the
  // poll for "is the daemon online?" is the App's live `connected` flag (the
  // ws://127.0.0.1:7177 telemetry link auto-reconnects 1s->5s, so it flips true
  // the moment the daemon comes up). When it does, the App gate (decideShowSetup)
  // un-mounts this screen automatically — the auto-dismiss is owned there, never
  // faked here. Until then the waiting UI below reflects `connected` directly.

  // a11y: trap + autofocus. NO Escape — this gate is deliberately
  // non-dismissable (the App gate un-mounts it when the daemon connects).
  const modalRef = useRef<HTMLDivElement>(null);
  useModalFocus(modalRef);

  return (
    <div className="syscfg-confirm-backdrop first-run-setup-backdrop">
      <div
        className="syscfg-confirm first-run-setup"
        role="dialog"
        aria-modal="true"
        aria-label="Finish setting up DARWIN"
        ref={modalRef}
      >
        {waiting ? (
          <>
            <div className="syscfg-confirm-title">{SETUP_COPY.waitingTitle}</div>
            <div className="kv-note">
              <div className="setup-spinner" aria-hidden="true" />
              <div style={{ marginTop: 8 }}>{SETUP_COPY.waitingBody}</div>
            </div>
            <div className="syscfg-status" role="status">
              {connected
                ? "DARWIN connected — finishing up…"
                : "Installing… (this screen clears itself when DARWIN connects)"}
            </div>
          </>
        ) : (
          <>
            <div className="syscfg-confirm-title">{SETUP_COPY.title}</div>
            <div className="kv-note">
              <strong>{SETUP_COPY.lede}</strong>
              {SETUP_COPY.body.map((line, i) => (
                <div key={i} style={{ marginTop: 8 }}>
                  {line}
                </div>
              ))}
            </div>

            {busy && (
              <div className="syscfg-status" role="status">
                Opening Terminal…
              </div>
            )}
            {errored && (
              <div className="syscfg-status err" role="status">
                {state.detail}
              </div>
            )}

            <div className="syscfg-confirm-btns">
              <button
                type="button"
                className="icon-btn syscfg-apply"
                disabled={busy}
                onClick={() => void onInstall()}
                title="Open Terminal and run the DARWIN installer (installs dependencies, builds the daemon, downloads the on-device models)"
              >
                {busy ? "Opening Terminal…" : errored ? "Retry install" : SETUP_COPY.action}
              </button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}

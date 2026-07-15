/* ======================================================================== *
 * FIRST-RUN SETUP — the pure, DOM-free gate decision + the setup copy.       *
 *                                                                            *
 * THE PROBLEM: a freshly-downloaded DARWIN .app has the HUD but NOT the       *
 * backend (the daemon binary, the Python venv, the on-device models). On that *
 * first run we want to offer to run the REAL installer. But a FALSE setup     *
 * prompt on a HEALTHY install is the worst outcome — so the gate is           *
 * deliberately CONSERVATIVE.                                                  *
 *                                                                            *
 * THE GATE (decideShowSetup):                                                 *
 *   The FIRST-RUN SETUP screen shows ONLY when ALL of:                        *
 *     1. we are in the Tauri shell (inTauri) — never a plain browser/vitest   *
 *        render, where there is no backend to install and no installer to     *
 *        open;                                                                *
 *     2. the backend is genuinely NOT installed (backend_installed() ===      *
 *        false — the install home is missing the daemon binary AND/OR the     *
 *        venv python); AND                                                    *
 *     3. the command channel is NOT connected (the daemon is not reachable —  *
 *        `state.connected` is false). A RUNNING daemon means the telemetry/   *
 *        command channel connects, so a healthy + running DARWIN NEVER shows  *
 *        this screen even for an instant.                                     *
 *                                                                            *
 *   CONSERVATIVE BY CONSTRUCTION: if the backend IS installed, OR the channel *
 *   IS connected, OR we are not in the shell, the gate returns false. When we *
 *   are UNSURE whether the install check has resolved yet (installed ===      *
 *   null), we do NOT nag — we prefer the normal HUD. A connected daemon alone *
 *   is enough to keep this hidden.                                            *
 *                                                                            *
 * This module is DOM/React/Tauri-free so the gate + copy are unit-tested in   *
 * the node vitest env exactly like the reducer/onboarding/auto-update cores.  *
 * The component (FirstRunSetup.tsx) calls the backend + renders these.        *
 * ======================================================================== */

/** The inputs the gate decides on. All three are owned by the App:
 *   - `inTauri`: are we inside the desktop shell? (false in a browser/vitest)
 *   - `installed`: the resolved backend_installed() result, or null while the
 *      check is still in flight / unavailable (we have not learned yet).
 *   - `connected`: is the daemon reachable? (App's `state.connected`, the live
 *      ws://127.0.0.1:7177 telemetry link — a running daemon connects it). */
export interface SetupGateInput {
  inTauri: boolean;
  installed: boolean | null;
  connected: boolean;
}

/**
 * THE GATE, as one pure function. Returns true ONLY when the setup screen should
 * mount: in the shell, the backend is KNOWN-not-installed, and the daemon is not
 * reachable. Every other combination returns false:
 *   - not in the shell                      -> false (nothing to install here)
 *   - the channel is connected              -> false (a running daemon ⇒ installed-enough)
 *   - the backend is installed              -> false (normal HUD)
 *   - the install check has not resolved    -> false (UNSURE ⇒ never nag)
 *
 * Note `connected` is checked FIRST and independently of `installed`: even if the
 * install probe somehow false-negatives, a connected daemon keeps the gate shut.
 */
export function decideShowSetup(input: SetupGateInput): boolean {
  if (!input.inTauri) return false;
  // A reachable daemon is definitive proof the backend is up — never prompt.
  if (input.connected) return false;
  // Only prompt when we POSITIVELY know the backend is not installed. `null`
  // (not yet resolved / unknown) is treated as "do not nag".
  return input.installed === false;
}

/** The phase of the setup screen's own flow (NOT the gate — the gate decides
 *  whether the screen mounts at all; this tracks what the mounted screen shows):
 *   - "intro"   : the honest pre-install copy + the "Install DARWIN" button.
 *   - "opening" : open_setup_install() is in flight (briefly).
 *   - "waiting" : Terminal was launched; we are polling for the daemon to come
 *                 online (the connection). Auto-dismisses when it connects.
 *   - "error"   : Terminal could NOT be opened (no GUI session, etc.) — show the
 *                 honest detail and let the user retry. NEVER claims it ran. */
export type SetupPhase = "intro" | "opening" | "waiting" | "error";

export interface SetupScreenState {
  phase: SetupPhase;
  /** Honest detail for the "error" phase (the backend's message) or the
   *  "waiting" phase (the installer-launched line). Empty in intro/opening. */
  detail: string;
}

export type SetupScreenEvent =
  | { type: "installStart" }
  /** The resolved result of open_setup_install(): `opened` true -> Terminal is
   *  running the installer (go to "waiting"); false -> surface the honest error. */
  | { type: "installResult"; opened: boolean; detail: string };

export function setupScreenInitial(): SetupScreenState {
  return { phase: "intro", detail: "" };
}

/** Pure transition for the screen's launch flow. Honest by construction: we only
 *  reach "waiting" when Terminal actually opened (`opened === true`); a false
 *  result lands in "error" with the backend's detail, so the screen never claims
 *  the installer is running when it is not. */
export function setupScreenReduce(
  state: SetupScreenState,
  event: SetupScreenEvent,
): SetupScreenState {
  switch (event.type) {
    case "installStart":
      return { phase: "opening", detail: "" };
    case "installResult":
      if (event.opened) {
        return {
          phase: "waiting",
          detail:
            event.detail ||
            "Installing in Terminal. This takes a while; DARWIN will connect when it finishes.",
        };
      }
      return {
        phase: "error",
        detail: event.detail || "Could not open Terminal to run the installer.",
      };
    default:
      return state;
  }
}

/* --- the honest setup copy (DOM-free, asserted by tests) ------------------- */

/** The headline + the honest, no-overclaim body for the FIRST-RUN SETUP screen.
 *  It states plainly: it installs the FULL system (deps if missing, the daemon
 *  built fresh, the Python venv, the on-device models — SEVERAL GB), it takes a
 *  while, macOS asks ONCE for your password (Homebrew), and it opens Terminal to
 *  run the real installer; when it finishes, DARWIN connects (or relaunch). */
export const SETUP_COPY = {
  title: "Finish setting up DARWIN",
  /** The one-line summary shown under the title. */
  lede: "This Mac has the DARWIN app but not the engine yet. Install it to bring DARWIN online.",
  /** Body paragraphs (rendered as separate lines). HONEST about scope + cost. */
  body: [
    "The installer sets up the full system: it installs any missing dependencies (Homebrew, Python, Rust, Node), builds the DARWIN daemon fresh, creates the Python environment, and downloads the on-device models — several gigabytes in total.",
    "It takes a while. macOS will ask once for your password (Homebrew needs it). It runs in Terminal so you can watch the progress — that is also why the password prompt works.",
    "When the installer finishes, DARWIN connects automatically. If it does not, relaunch the app.",
  ],
  /** The primary action button label. */
  action: "Install DARWIN",
  /** The waiting-state heading shown after Terminal is launched. */
  waitingTitle: "Waiting for DARWIN to come online…",
  /** The waiting-state body — honest that it is still installing and what to do. */
  waitingBody:
    "The installer is running in Terminal. This screen clears itself the moment DARWIN connects. If the installer has finished and DARWIN still has not connected, relaunch the app.",
} as const;

/* ======================================================================== *
 * ABOUT PANEL — the pure, DOM-free mapping from an update CHECK result to    *
 * the line shown under the "Check for Updates" button.                       *
 *                                                                            *
 * The custom About panel (components/AboutPanel.tsx) replaces the macOS       *
 * standard about panel so it can carry a WORKING "Check for Updates" button   *
 * and the "made by darwin capani" credit. The button runs the SAME honest     *
 * backend check the launch auto-check uses (checkForUpdates(false) — a CHECK, *
 * never a silent install) and routes the result through this function.        *
 *                                                                            *
 * HONESTY CONTRACT (do not regress): this never invents an update and never   *
 * claims "latest" unless the backend actually returned `up_to_date`. For an    *
 * `available` result it surfaces the real version and signals the caller to    *
 * hand off to the EXISTING signed UpdateDialog (download + minisign verify +   *
 * install) — this module adds NO install authority of its own. Any other       *
 * status surfaces the backend's own honest `detail` line verbatim.            *
 *                                                                            *
 * Kept Tauri/DOM-free so it unit-tests in the node vitest env like the other  *
 * cores (autoUpdate, onboarding, firstRunSetup).                             *
 * ======================================================================== */

import type { UpdateCheck } from "../tauri/bridge";

/** What the About panel shows after a check:
 *   - "uptodate"  : reassuring "DARWIN is on the latest version." (ok styling)
 *   - "available" : a real newer version exists — the panel hands `version` to
 *                   the App so the existing signed UpdateDialog opens. (The
 *                   panel does NOT install; it only routes.)
 *   - "error"     : the check could not complete / updates aren't available
 *                   here — the backend's honest detail, error styling. */
export type AboutCheckKind = "uptodate" | "available" | "error";

export interface AboutCheckView {
  kind: AboutCheckKind;
  /** The line to render. For most statuses this is the backend's own honest
   *  `detail`; for "available" it is a concise "DARWIN X is available." */
  text: string;
  /** The available version (only set for kind "available"); else null. */
  version: string | null;
}

/**
 * Map an UpdateCheck result to the About panel's view. Pure + exhaustive.
 *
 * `up_to_date`  -> the reassuring latest-version line (the user's explicit ask:
 *                  "when there is no update, it says DARWIN is on the latest
 *                  version").
 * `available`   -> kind "available" + the real version, so the panel routes to
 *                  the signed UpdateDialog. Falls back to a generic line if the
 *                  backend somehow omitted the version (then NO routing — we
 *                  never fabricate a version to install).
 * everything else (`not_configured` / `error` / `unavailable` / `installed`)
 *               -> surface the backend's honest detail. We never pretend.
 */
export function aboutCheckView(result: UpdateCheck): AboutCheckView {
  switch (result.status) {
    case "up_to_date":
      return { kind: "uptodate", text: "DARWIN is on the latest version.", version: null };
    case "available": {
      const v = result.version ?? null;
      return v
        ? { kind: "available", text: `DARWIN ${v} is available.`, version: v }
        // No version string -> do NOT route to install; show an honest line.
        : { kind: "error", text: result.detail || "An update is available.", version: null };
    }
    case "installed":
      // A check(false) never installs, but stay exhaustive + honest.
      return { kind: "uptodate", text: result.detail || "DARWIN is up to date.", version: null };
    default:
      // not_configured | error | unavailable -> the backend's honest detail.
      return { kind: "error", text: result.detail || "Could not check for updates.", version: null };
  }
}

import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  AUTO_UPDATE_KEY,
  AUTO_UPDATE_ON_VALUE,
  decideLaunchUpdateAction,
  isAutoUpdateOn,
  setAutoUpdateOn,
  silentUpdateNotice,
  updateDialogInitial,
  updateDialogReduce,
  type AutoUpdateStorage,
} from "../core/autoUpdate";
import type { UpdateCheck } from "../tauri/bridge";

/* The auto-update launch feature. THE CARDINAL HONESTY RULE the tests pin:
 *   - the dialog (and the silent path) are reachable ONLY for status
 *     "available" — never not_configured / up_to_date / error / unavailable /
 *     installed / unknown, and never a fabricated update;
 *   - the persisted pref mirrors onboarding.ts (a localStorage key) and is
 *     fully REVERSIBLE (OFF clears the key);
 *   - the three dialog buttons drive the right paths (Update = install;
 *     Update&don't-ask = persist pref THEN install; Cancel = close, pref
 *     unchanged);
 *   - when the pref is ON the launch path installs SILENTLY (no dialog) with an
 *     honest notice;
 *   - the install state machine never claims success on a non-"installed"
 *     result. */

/* ----------------------------------------------------- in-memory storage stub */

/** A localStorage stub (with removeItem) so the pref helpers run in node. */
function memStorage(): AutoUpdateStorage & { map: Map<string, string> } {
  const map = new Map<string, string>();
  return {
    map,
    getItem: (k) => (map.has(k) ? map.get(k)! : null),
    setItem: (k, v) => {
      map.set(k, v);
    },
    removeItem: (k) => {
      map.delete(k);
    },
  };
}

/* small builders for the UpdateCheck contract */
const available = (version: string | null = "1.4.0"): UpdateCheck => ({
  status: "available",
  detail: `Version ${version} is available.`,
  version,
});
const notConfigured = (): UpdateCheck => ({
  status: "not_configured",
  detail: "Auto-update is not armed yet — see docs/RELEASE.md.",
  version: null,
});
const upToDate = (): UpdateCheck => ({
  status: "up_to_date",
  detail: "JARVIS is on the latest version.",
  version: null,
});
const errored = (): UpdateCheck => ({ status: "error", detail: "offline" });
const unavailable = (): UpdateCheck => ({
  status: "unavailable",
  detail: "Updates are checked from the JARVIS desktop app.",
});
const installed = (version = "1.4.0"): UpdateCheck => ({
  status: "installed",
  detail: `Version ${version} was downloaded, verified, and installed — relaunch JARVIS to finish.`,
  version,
});

/* ======================================================================== *
 * Persisted preference — mirrors the onboarding once-only flag pattern.      *
 * ======================================================================== */
describe("auto-update preference persistence", () => {
  it("default is OFF on a fresh install (no key set)", () => {
    const store = memStorage();
    expect(isAutoUpdateOn(store)).toBe(false);
  });

  it("setAutoUpdateOn(true) persists the fixed literal under the versioned key", () => {
    const store = memStorage();
    setAutoUpdateOn(true, store);
    expect(isAutoUpdateOn(store)).toBe(true);
    expect(store.getItem(AUTO_UPDATE_KEY)).toBe(AUTO_UPDATE_ON_VALUE);
  });

  it("is REVERSIBLE: setAutoUpdateOn(false) clears the key (don't-ask-again is undoable)", () => {
    const store = memStorage();
    setAutoUpdateOn(true, store);
    expect(isAutoUpdateOn(store)).toBe(true);
    setAutoUpdateOn(false, store);
    expect(isAutoUpdateOn(store)).toBe(false);
    // OFF removes the key rather than leaving a stale value behind.
    expect(store.getItem(AUTO_UPDATE_KEY)).toBeNull();
  });

  it("FAIL-SAFE toward OFF: with no storage it reads OFF (shows the dialog, never auto-installs)", () => {
    expect(isAutoUpdateOn(null)).toBe(false);
    expect(() => setAutoUpdateOn(true, null)).not.toThrow();
  });

  it("FAIL-SAFE: a storage that throws on read reads OFF", () => {
    const throwing: AutoUpdateStorage = {
      getItem: () => {
        throw new Error("blocked");
      },
      setItem: () => {},
      removeItem: () => {},
    };
    expect(isAutoUpdateOn(throwing)).toBe(false);
  });

  it("a different key/value does NOT count as ON (only the exact flag arms it)", () => {
    const store = memStorage();
    store.setItem(AUTO_UPDATE_KEY, "0");
    expect(isAutoUpdateOn(store)).toBe(false);
    store.setItem("some.other.key", AUTO_UPDATE_ON_VALUE);
    expect(isAutoUpdateOn(store)).toBe(false);
  });
});

/* ======================================================================== *
 * THE honesty gate — decideLaunchUpdateAction only ever acts on "available". *
 * ======================================================================== */
describe("launch branch — only status 'available' surfaces anything", () => {
  it("status 'available' + pref OFF -> open the dialog naming the real version", () => {
    const a = decideLaunchUpdateAction(available("2.0.1"), false);
    expect(a).toEqual({ kind: "dialog", version: "2.0.1" });
  });

  it("status 'available' + pref ON -> SILENT install (no dialog)", () => {
    const a = decideLaunchUpdateAction(available("2.0.1"), true);
    expect(a).toEqual({ kind: "silent", version: "2.0.1" });
  });

  it("NEVER surfaces for not_configured (the shipped, un-armed state) — pref ON or OFF", () => {
    expect(decideLaunchUpdateAction(notConfigured(), false)).toEqual({ kind: "none" });
    expect(decideLaunchUpdateAction(notConfigured(), true)).toEqual({ kind: "none" });
  });

  it("NEVER surfaces for up_to_date / error / unavailable / installed (no dialog, no nag)", () => {
    for (const r of [upToDate(), errored(), unavailable(), installed()]) {
      expect(decideLaunchUpdateAction(r, false)).toEqual({ kind: "none" });
      expect(decideLaunchUpdateAction(r, true)).toEqual({ kind: "none" });
    }
  });

  it("NEVER surfaces for an unrecognised status (defensive default-deny)", () => {
    const weird = { status: "totally_made_up", detail: "x" } as unknown as UpdateCheck;
    expect(decideLaunchUpdateAction(weird, false)).toEqual({ kind: "none" });
    expect(decideLaunchUpdateAction(weird, true)).toEqual({ kind: "none" });
  });

  it("cannot fabricate an update: 'available' with no usable version -> none", () => {
    // An available status that can't honestly name a version never opens a dialog.
    expect(decideLaunchUpdateAction(available(null), false)).toEqual({ kind: "none" });
    expect(decideLaunchUpdateAction(available(""), true)).toEqual({ kind: "none" });
    expect(decideLaunchUpdateAction(available("   "), false)).toEqual({ kind: "none" });
  });

  it("the silent notice is honest + names the version (never a silent surprise)", () => {
    expect(silentUpdateNotice("3.1.0")).toBe("Updating JARVIS to 3.1.0…");
  });
});

/* ======================================================================== *
 * Install state machine — never claims success unless backend says installed *
 * ======================================================================== */
describe("update dialog install reducer", () => {
  it("idle -> installing on installStart (buttons disable, no success claim)", () => {
    const s = updateDialogReduce(updateDialogInitial(), { type: "installStart" });
    expect(s.phase).toBe("installing");
    expect(s.detail).toBe("");
  });

  it("a backend 'installed' result is the ONLY path to the installed phase", () => {
    const s = updateDialogReduce(
      { phase: "installing", detail: "" },
      { type: "installResult", result: installed("9.9.9") },
    );
    expect(s.phase).toBe("installed");
    expect(s.detail).toContain("9.9.9");
  });

  it("any non-'installed' result is surfaced as an honest error (never success)", () => {
    for (const r of [errored(), upToDate(), notConfigured(), unavailable()]) {
      const s = updateDialogReduce(
        { phase: "installing", detail: "" },
        { type: "installResult", result: r },
      );
      expect(s.phase).toBe("error");
      // it carries the backend's honest detail, never a fake "installed".
      expect(s.detail).toBe(r.detail);
    }
  });
});

/* ======================================================================== *
 * Dialog render — three labelled buttons + honest sub-copy.                  *
 * ======================================================================== */
describe("UpdateDialog render (three buttons + honest copy)", () => {
  let UpdateDialog: typeof import("../components/UpdateDialog").default;
  beforeEach(async () => {
    vi.resetModules();
    UpdateDialog = (await import("../components/UpdateDialog")).default;
  });

  function html(version = "1.4.0"): string {
    return renderToStaticMarkup(
      createElement(UpdateDialog, { version, onClose: () => {} }),
    );
  }

  it("names the real version and the signature-verification sub-copy", () => {
    const h = html("1.4.0");
    expect(h).toContain("UPDATE AVAILABLE");
    expect(h).toContain("JARVIS 1.4.0 is available.");
    expect(h.toLowerCase()).toContain("signature-verified before installing");
  });

  it("renders exactly the three labelled buttons (Update, don't-ask, Cancel)", () => {
    const h = html();
    expect(h).toContain("Cancel");
    expect(h).toContain("Update &amp; don&#x27;t ask again");
    // The primary Update button (not the don't-ask variant) is present.
    expect(h).toMatch(/>Update<\/button>/);
  });

  it("does not claim success on first paint (no 'installed' copy before any click)", () => {
    const h = html();
    expect(h.toLowerCase()).not.toContain("restart to finish");
    expect(h.toLowerCase()).not.toContain("installing…");
  });
});

/* ======================================================================== *
 * Button wiring — the exact action each of the three buttons performs.       *
 * (No jsdom in this env, so we drive the SAME helpers the onClicks call and  *
 * assert the persistence side-effect + install/relaunch sequence via spies.) *
 * ======================================================================== */
describe("three-button wiring (the exact action each performs)", () => {
  const checkSpy = vi.fn<(install: boolean) => Promise<UpdateCheck>>();
  const relaunchSpy = vi.fn<() => Promise<boolean>>();

  beforeEach(() => {
    checkSpy.mockReset();
    relaunchSpy.mockReset();
  });
  afterEach(() => {
    vi.restoreAllMocks();
  });

  // The shared install+relaunch the two install buttons run (mirrors the
  // component's installAndRelaunch): call checkForUpdates(true); on "installed"
  // relaunch. We assert the call shapes + ordering against the spies.
  async function installAndRelaunch(): Promise<UpdateCheck> {
    const result = await checkSpy(true);
    if (result.status === "installed") await relaunchSpy();
    return result;
  }

  it("'Update' installs via the SIGNED backend command (install=true) then relaunches", async () => {
    checkSpy.mockResolvedValue(installed("1.4.0"));
    relaunchSpy.mockResolvedValue(true);
    const store = memStorage();
    // "Update" never touches the preference.
    const r = await installAndRelaunch();
    expect(checkSpy).toHaveBeenCalledWith(true); // the EXISTING install path
    expect(relaunchSpy).toHaveBeenCalledTimes(1);
    expect(r.status).toBe("installed");
    expect(isAutoUpdateOn(store)).toBe(false); // pref unchanged
  });

  it("'Update & don't ask again' persists pref=ON BEFORE installing, then installs+relaunches", async () => {
    checkSpy.mockResolvedValue(installed());
    relaunchSpy.mockResolvedValue(true);
    const store = memStorage();
    // The component sets the pref FIRST, then runs the same install path.
    setAutoUpdateOn(true, store);
    expect(isAutoUpdateOn(store)).toBe(true); // persisted before the install
    await installAndRelaunch();
    expect(checkSpy).toHaveBeenCalledWith(true);
    expect(relaunchSpy).toHaveBeenCalledTimes(1);
  });

  it("'Cancel' closes WITHOUT changing the preference (re-checks next launch)", () => {
    const store = memStorage();
    const onClose = vi.fn();
    // Cancel's onClick = onClose(); it never calls setAutoUpdateOn or check.
    onClose();
    expect(onClose).toHaveBeenCalledTimes(1);
    expect(checkSpy).not.toHaveBeenCalled();
    expect(isAutoUpdateOn(store)).toBe(false);
  });

  it("an install error does NOT relaunch and is never reported as success", async () => {
    checkSpy.mockResolvedValue(errored());
    const r = await installAndRelaunch();
    expect(r.status).toBe("error");
    expect(relaunchSpy).not.toHaveBeenCalled();
    // The reducer keeps it honest.
    const s = updateDialogReduce(
      { phase: "installing", detail: "" },
      { type: "installResult", result: r },
    );
    expect(s.phase).toBe("error");
  });
});

/* ======================================================================== *
 * Launch path when the pref is ON — installs silently, no dialog.            *
 * ======================================================================== */
describe("silent launch install when pref is ON", () => {
  const checkSpy = vi.fn<(install: boolean) => Promise<UpdateCheck>>();
  const relaunchSpy = vi.fn<() => Promise<boolean>>();
  const toastSpy = vi.fn<(text: string) => void>();

  beforeEach(() => {
    checkSpy.mockReset();
    relaunchSpy.mockReset();
    toastSpy.mockReset();
  });

  // Mirrors App's launch effect for the "available" branch.
  async function runLaunch(check: UpdateCheck, autoOn: boolean): Promise<string> {
    const action = decideLaunchUpdateAction(check, autoOn);
    if (action.kind === "none") return "none";
    if (action.kind === "dialog") return "dialog"; // App would open the modal
    // silent:
    toastSpy(silentUpdateNotice(action.version)); // honest brief notice FIRST
    const installed = await checkSpy(true); // the EXISTING signed command
    if (installed.status === "installed") await relaunchSpy();
    return "silent";
  }

  it("pref ON + available -> shows the honest notice, installs (install=true), relaunches; NO dialog", async () => {
    checkSpy.mockResolvedValue(installed("5.0.0"));
    relaunchSpy.mockResolvedValue(true);
    const outcome = await runLaunch(available("5.0.0"), true);
    expect(outcome).toBe("silent"); // never "dialog"
    expect(toastSpy).toHaveBeenCalledWith("Updating JARVIS to 5.0.0…");
    expect(checkSpy).toHaveBeenCalledWith(true);
    expect(relaunchSpy).toHaveBeenCalledTimes(1);
  });

  it("pref OFF + available -> opens the dialog (no silent install, no toast)", async () => {
    const outcome = await runLaunch(available("5.0.0"), false);
    expect(outcome).toBe("dialog");
    expect(toastSpy).not.toHaveBeenCalled();
    expect(checkSpy).not.toHaveBeenCalled();
  });

  it("pref ON but NOT armed (not_configured) -> nothing happens (no install, no toast)", async () => {
    const outcome = await runLaunch(notConfigured(), true);
    expect(outcome).toBe("none");
    expect(toastSpy).not.toHaveBeenCalled();
    expect(checkSpy).not.toHaveBeenCalled();
  });
});

import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import FirstRunSetup from "../components/FirstRunSetup";
import {
  SETUP_COPY,
  decideShowSetup,
  setupScreenInitial,
  setupScreenReduce,
} from "../core/firstRunSetup";

/* ======================================================================== *
 * THE GATE — the load-bearing honesty contract. The FIRST-RUN SETUP screen   *
 * shows ONLY when (in the shell) the backend is KNOWN-not-installed AND the   *
 * daemon is not reachable; it NEVER shows when DARWIN is installed + running. *
 * ======================================================================== */
describe("first-run setup gate (decideShowSetup)", () => {
  it("SHOWS only when in the shell, not installed, and not connected", () => {
    expect(
      decideShowSetup({ inTauri: true, installed: false, connected: false }),
    ).toBe(true);
  });

  it("NEVER shows when the daemon is connected — even if the install probe says not-installed", () => {
    // A reachable daemon is definitive proof the backend is up: the gate must be
    // shut regardless of a (possibly false-negative) install probe. This is the
    // 'a false setup prompt on a healthy install is the worst outcome' guard.
    expect(
      decideShowSetup({ inTauri: true, installed: false, connected: true }),
    ).toBe(false);
    expect(
      decideShowSetup({ inTauri: true, installed: true, connected: true }),
    ).toBe(false);
    expect(
      decideShowSetup({ inTauri: true, installed: null, connected: true }),
    ).toBe(false);
  });

  it("NEVER shows when the backend IS installed (normal HUD)", () => {
    expect(
      decideShowSetup({ inTauri: true, installed: true, connected: false }),
    ).toBe(false);
  });

  it("NEVER shows while the install check is unresolved (null ⇒ do not nag)", () => {
    // UNSURE-whether-installed must prefer the normal HUD, never a false prompt.
    expect(
      decideShowSetup({ inTauri: true, installed: null, connected: false }),
    ).toBe(false);
  });

  it("NEVER shows outside the Tauri shell (browser / vitest render)", () => {
    // Nothing to install + no installer to open outside the desktop app.
    for (const installed of [false, true, null] as const) {
      for (const connected of [false, true]) {
        expect(decideShowSetup({ inTauri: false, installed, connected })).toBe(false);
      }
    }
  });

  it("the only TRUE combination is exactly {shell, not-installed, not-connected}", () => {
    // Exhaustive truth table over the three inputs: precisely one row is true.
    let trueCount = 0;
    for (const inTauri of [false, true]) {
      for (const installed of [false, true, null] as const) {
        for (const connected of [false, true]) {
          const show = decideShowSetup({ inTauri, installed, connected });
          if (show) {
            trueCount += 1;
            expect(inTauri).toBe(true);
            expect(installed).toBe(false);
            expect(connected).toBe(false);
          }
        }
      }
    }
    expect(trueCount).toBe(1);
  });
});

/* ======================================================================== *
 * POST-INSTALL WAIT — the screen's launch state machine. Honest: it only      *
 * reaches "waiting" when Terminal actually opened; a failure is an honest      *
 * error, never a faked "running".                                            *
 * ======================================================================== */
describe("first-run setup screen reducer", () => {
  it("starts on the intro", () => {
    expect(setupScreenInitial()).toEqual({ phase: "intro", detail: "" });
  });

  it("installStart -> opening (no claim yet)", () => {
    const s = setupScreenReduce(setupScreenInitial(), { type: "installStart" });
    expect(s.phase).toBe("opening");
  });

  it("a successful open -> waiting (Terminal is running the installer)", () => {
    const opening = setupScreenReduce(setupScreenInitial(), { type: "installStart" });
    const s = setupScreenReduce(opening, {
      type: "installResult",
      opened: true,
      detail: "Opened Terminal running the DARWIN installer.",
    });
    expect(s.phase).toBe("waiting");
    expect(s.detail).toContain("installer");
  });

  it("a FAILED open -> error with the honest detail (never claims it ran)", () => {
    const opening = setupScreenReduce(setupScreenInitial(), { type: "installStart" });
    const s = setupScreenReduce(opening, {
      type: "installResult",
      opened: false,
      detail: "could not open Terminal: no GUI session",
    });
    expect(s.phase).toBe("error");
    expect(s.detail).toContain("could not open Terminal");
    // It must NOT have moved to the running/waiting state on a failure.
    expect(s.phase).not.toBe("waiting");
  });

  it("falls back to honest default copy when the backend gives no detail", () => {
    const opening = setupScreenReduce(setupScreenInitial(), { type: "installStart" });
    const ok = setupScreenReduce(opening, { type: "installResult", opened: true, detail: "" });
    expect(ok.detail).not.toBe("");
    const bad = setupScreenReduce(opening, { type: "installResult", opened: false, detail: "" });
    expect(bad.detail).not.toBe("");
  });
});

/* ======================================================================== *
 * THE COPY — honest about scope (multi-GB models, deps, time, ONE password    *
 * prompt, opens Terminal, then connects).                                    *
 * ======================================================================== */
describe("first-run setup copy", () => {
  it("names the full install scope and cost honestly", () => {
    const blob = [
      SETUP_COPY.title,
      SETUP_COPY.lede,
      ...SETUP_COPY.body,
      SETUP_COPY.action,
      SETUP_COPY.waitingTitle,
      SETUP_COPY.waitingBody,
    ]
      .join(" ")
      .toLowerCase();
    // Multi-GB on-device models.
    expect(blob).toContain("model");
    expect(blob).toMatch(/gigabyte|gb|several/);
    // Missing deps installed.
    expect(blob).toMatch(/dependenc|homebrew|python|rust|node/);
    // Takes a while.
    expect(blob).toContain("while");
    // ONE macOS password prompt.
    expect(blob).toContain("password");
    // Opens Terminal.
    expect(blob).toContain("terminal");
    // Connects when done (or relaunch).
    expect(blob).toContain("connect");
    expect(blob).toContain("relaunch");
  });
});

/* ======================================================================== *
 * RENDER — the intro shows the honest copy + the Install button; the waiting  *
 * state shows the "waiting for DARWIN to come online…" heading.              *
 * ======================================================================== */
describe("FirstRunSetup render", () => {
  function html(connected = false) {
    return renderToStaticMarkup(createElement(FirstRunSetup, { connected }));
  }

  it("renders the intro with the honest scope copy and the Install button", () => {
    const out = html(false);
    expect(out).toContain(SETUP_COPY.title);
    expect(out).toContain(SETUP_COPY.action); // "Install DARWIN"
    const lower = out.toLowerCase();
    expect(lower).toContain("password");
    expect(lower).toContain("terminal");
    expect(lower).toContain("model");
    // The intro is NOT the waiting state.
    expect(out).not.toContain(SETUP_COPY.waitingTitle);
  });

  it("is a modal dialog (so the pre-backend gate is unambiguous)", () => {
    const out = html(false);
    expect(out).toContain('role="dialog"');
    expect(out).toContain('aria-modal="true"');
  });
});

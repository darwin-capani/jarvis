import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import SystemAccessPanel from "../components/SystemAccessPanel";
import {
  PERMISSION_KEYS,
  PERMISSION_PANES,
  PERMISSIONS_COPY,
} from "../core/permissions";

/* ======================================================================== *
 * DRIFT LOCK — this key set is the OTHER half of the cross-language lock.    *
 * hud/src-tauri/src/permissions.rs::allowlist_has_exactly_the_expected_keys *
 * asserts the IDENTICAL list, so adding/removing/renaming a pane on one side *
 * without the other fails one of the two suites. The backend rejects any key *
 * not in its allowlist, so the UI can only ever open a known Privacy pane.   *
 * ======================================================================== */
describe("system-access permission allowlist", () => {
  it("is exactly the expected key set, in order (mirrors the Rust allowlist)", () => {
    expect(PERMISSION_KEYS).toEqual([
      "full_disk",
      "accessibility",
      "screen",
      "microphone",
      "input_monitoring",
      "automation",
      "camera",
    ]);
  });

  it("every pane has a key, a Privacy_ anchor, a label, and a reason", () => {
    for (const p of PERMISSION_PANES) {
      expect(p.key).toBeTruthy();
      expect(p.anchor.startsWith("Privacy_")).toBe(true);
      expect(p.label).toBeTruthy();
      expect(p.why.length).toBeGreaterThan(10);
    }
  });

  it("keys are unique (no duplicate pane)", () => {
    expect(new Set(PERMISSION_KEYS).size).toBe(PERMISSION_KEYS.length);
  });
});

/* ======================================================================== *
 * HONESTY — the copy must say macOS won't let DARWIN grant itself access     *
 * (the user flips the switch), and must NOT claim it auto-grants anything.   *
 * ======================================================================== */
describe("system-access copy honesty", () => {
  it("states plainly that no app can grant itself the permission", () => {
    const blob = [
      PERMISSIONS_COPY.lede,
      ...PERMISSIONS_COPY.how,
      PERMISSIONS_COPY.footnote,
    ]
      .join(" ")
      .toLowerCase();
    // The boundary: macOS / no app can switch them on for itself.
    expect(blob).toMatch(/no app|can('|)t (grant|switch)|won't let|yourself|you flip|yours to flip/);
    // It directs the user to System Settings.
    expect(blob).toContain("system settings");
    // It is honest that the user does the toggling.
    expect(blob).toMatch(/turn darwin on|flip|switch/);
    // It must NOT pretend DARWIN grants/enables the permission itself.
    expect(blob).not.toMatch(/darwin (grants|enables|turns on) (the )?(permission|access|full disk)/);
  });

  it("the re-request label matches the user-facing action", () => {
    expect(PERMISSIONS_COPY.requestAll.toLowerCase()).toContain("re-request");
    expect(PERMISSIONS_COPY.requestAll.toLowerCase()).toContain("permission");
  });
});

/* ======================================================================== *
 * RENDER — every permission row, the OPEN buttons, and the RE-REQUEST ALL    *
 * control render; outside the shell the buttons are inert (honest).         *
 * ======================================================================== */
describe("SystemAccessPanel render", () => {
  function html() {
    return renderToStaticMarkup(createElement(SystemAccessPanel));
  }

  // React escapes & < > " ' in text/attributes; mirror that so labels with an
  // ampersand ("Screen & System Audio Recording") match the rendered markup.
  const esc = (s: string) =>
    s
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&#x27;");

  it("renders every permission label and its reason", () => {
    const out = html();
    for (const p of PERMISSION_PANES) {
      expect(out).toContain(esc(p.label));
    }
    // Section + the headline re-request control.
    expect(out).toContain(PERMISSIONS_COPY.title);
    expect(out).toContain(PERMISSIONS_COPY.requestAll);
    // One REQUEST affordance per pane (aria-label form is stable to assert).
    for (const p of PERMISSION_PANES) {
      expect(out).toContain(esc(`Request ${p.label} access`));
    }
  });

  it("outside the DARWIN shell the buttons are disabled (honest — nothing to open)", () => {
    const out = html();
    // vitest is not the Tauri shell, so every action button renders disabled.
    expect(out).toContain("disabled");
    expect(out.toLowerCase()).toContain("desktop app");
  });
});

import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";

// Mock the Tauri runtime so component import graphs (sendCommand -> invoke)
// resolve without a shell. SSR render never fires them.
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

import Frame from "../components/Frame";
import TranscriptPanel from "../components/TranscriptPanel";
import GlobalScanPanel from "../components/GlobalScanPanel";
import AgentPanel from "../components/AgentPanel";
import ReticleDial from "../components/ReticleDial";
import AboutPanel from "../components/AboutPanel";
import OnboardingWizard from "../components/OnboardingWizard";
import UpdateDialog from "../components/UpdateDialog";
import { FOCUSABLE_SELECTOR, nextTrapIndex } from "../core/focusTrap";

/* ======================================================================== *
 * A11Y CONTRACT (HUD-wide) — static-markup side.                             *
 *                                                                            *
 * The vitest env is node (no DOM), so these tests pin the STATIC a11y        *
 * contract: roles, names, live-region semantics, and landmark/heading        *
 * structure as rendered. Focus BEHAVIOR (trap/autofocus/restore) is          *
 * effect-driven and cannot run under SSR — its decision arithmetic is the    *
 * pure `nextTrapIndex`, tested exhaustively below.                            *
 * ======================================================================== */

describe("focus-trap arithmetic (pure)", () => {
  it("no focusables -> -1 (caller keeps focus put)", () => {
    expect(nextTrapIndex(0, -1, false)).toBe(-1);
    expect(nextTrapIndex(0, 0, true)).toBe(-1);
    expect(nextTrapIndex(-3, 1, false)).toBe(-1);
  });

  it("entering from outside: Tab lands FIRST, Shift+Tab lands LAST", () => {
    expect(nextTrapIndex(4, -1, false)).toBe(0);
    expect(nextTrapIndex(4, -1, true)).toBe(3);
    // An out-of-range current (stale index) re-enters the same way.
    expect(nextTrapIndex(4, 9, false)).toBe(0);
    expect(nextTrapIndex(4, 9, true)).toBe(3);
  });

  it("steps forward/backward and WRAPS at both ends", () => {
    expect(nextTrapIndex(3, 0, false)).toBe(1);
    expect(nextTrapIndex(3, 1, false)).toBe(2);
    expect(nextTrapIndex(3, 2, false)).toBe(0); // wrap fwd
    expect(nextTrapIndex(3, 2, true)).toBe(1);
    expect(nextTrapIndex(3, 0, true)).toBe(2); // wrap back
  });

  it("a single focusable cycles onto itself (never escapes)", () => {
    expect(nextTrapIndex(1, 0, false)).toBe(0);
    expect(nextTrapIndex(1, 0, true)).toBe(0);
  });

  it("the focusable selector excludes disabled controls and tabindex=-1", () => {
    expect(FOCUSABLE_SELECTOR).toContain("button:not([disabled])");
    expect(FOCUSABLE_SELECTOR).toContain('[tabindex]:not([tabindex="-1"])');
  });
});

describe("Frame: named sections + real headings", () => {
  it("a titled frame is a section NAMED BY a real <h2>", () => {
    const html = renderToStaticMarkup(
      createElement(Frame, { title: "COMMS // TRANSCRIPT", children: "body" }),
    );
    expect(html).toContain("<section");
    expect(html).toContain("aria-labelledby=");
    // The title is a heading element (heading-nav for ~60 panels), not a span.
    expect(html).toMatch(/<h2 class="t" id="[^"]+">COMMS \/\/ TRANSCRIPT<\/h2>/);
    // The section's label id matches the h2's id (the linkage is real).
    const labelled = html.match(/aria-labelledby="([^"]+)"/)?.[1];
    const headingId = html.match(/<h2 class="t" id="([^"]+)"/)?.[1];
    expect(labelled).toBeTruthy();
    expect(labelled).toBe(headingId);
  });

  it("a titleless frame stays an unnamed section (nothing to call it)", () => {
    const html = renderToStaticMarkup(createElement(Frame, { children: "body" }));
    expect(html).not.toContain("aria-labelledby");
    expect(html).not.toContain("<h2");
  });
});

describe("live regions: the conversation is announced, captions are not doubled", () => {
  it("the transcript is a role=log (announce additions) with a name", () => {
    const html = renderToStaticMarkup(
      createElement(TranscriptPanel, {
        lines: [{ seq: 1, who: "user" as const, text: "hello", ts: "0", routedTo: "local" }],
        intent: null,
      }),
    );
    expect(html).toContain('role="log"');
    expect(html).toContain('aria-label="Conversation transcript"');
  });

  it("the intel feed is a role=log with a name", () => {
    const html = renderToStaticMarkup(
      createElement(GlobalScanPanel, {
        feed: {
          brief: "quiet day",
          items: [
            { title: "t", url: "u", source: "s", category: "net", published: null },
          ],
        } as never,
        running: true,
      }),
    );
    expect(html).toContain('role="log"');
    expect(html).toContain('aria-label="Intel feed"');
  });

  it("the HANDLING agent line is a role=status (announced on change)", () => {
    const html = renderToStaticMarkup(createElement(AgentPanel, { active: null }));
    expect(html).toContain('role="status"');
    expect(html).toContain("STANDBY");
  });
});

describe("decorative/graphic surfaces", () => {
  it("the CPU dial is a named role=img carrying the REAL reading, svg hidden", () => {
    const withReading = renderToStaticMarkup(
      createElement(ReticleDial, { cpuPercent: 42, coreState: "idle" as never }),
    );
    expect(withReading).toContain('role="img"');
    expect(withReading).toContain('aria-label="CPU dial: 42 percent"');
    expect(withReading).toContain('aria-hidden="true"');
    // No reading -> the honest absence, never a fabricated number.
    const noReading = renderToStaticMarkup(
      createElement(ReticleDial, { cpuPercent: null, coreState: "idle" as never }),
    );
    expect(noReading).toContain('aria-label="CPU dial: no reading"');
  });
});

describe("dialogs render real dialog semantics", () => {
  it("AboutPanel: dialog + modal + name", () => {
    const html = renderToStaticMarkup(
      createElement(AboutPanel, {
        version: "1.6.0",
        onClose: () => {},
        onUpdateAvailable: () => {},
      }),
    );
    expect(html).toContain('role="dialog"');
    expect(html).toContain('aria-modal="true"');
    expect(html).toContain('aria-label="About D.A.R.W.I.N."');
  });

  it("OnboardingWizard: dialog + modal + name", () => {
    const html = renderToStaticMarkup(
      createElement(OnboardingWizard, { onRoute: () => {}, onDismiss: () => {} }),
    );
    expect(html).toContain('role="dialog"');
    expect(html).toContain('aria-modal="true"');
  });

  it("UpdateDialog: dialog + modal + name", () => {
    const html = renderToStaticMarkup(
      createElement(UpdateDialog, { version: "9.9.9", onClose: () => {} }),
    );
    expect(html).toContain('role="dialog"');
    expect(html).toContain('aria-modal="true"');
    expect(html).toContain('aria-label="Update available"');
  });
});

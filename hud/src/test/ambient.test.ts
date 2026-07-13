import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import AmbientMode from "../components/AmbientMode";
import { isAtRest, AT_REST_IDLE_MS } from "../core/ambient";
import { parsePresence, type Presence } from "../core/events";

describe("isAtRest (driven by presence + local activity)", () => {
  const now = 1_000_000;

  it("is never at-rest unless the daemon reports 'away'", () => {
    expect(isAtRest("present", 0, now)).toBe(false);
    expect(isAtRest("focused", 0, now)).toBe(false);
    expect(isAtRest(null, 0, now)).toBe(false); // no frame yet -> claims nothing
  });

  it("at-rest when away AND local activity is older than the idle window", () => {
    expect(isAtRest("away", now - AT_REST_IDLE_MS, now)).toBe(true);
    expect(isAtRest("away", now - AT_REST_IDLE_MS - 5000, now)).toBe(true);
  });

  it("recent local activity keeps the full HUD awake even when away", () => {
    // Moved the mouse 1s ago -> not at-rest despite 'away'.
    expect(isAtRest("away", now - 1000, now)).toBe(false);
  });
});

describe("AmbientMode (calm glanceable mirror)", () => {
  const render = (presence: Presence | null, briefCount = 0, feedCount = 0) =>
    renderToStaticMarkup(
      createElement(AmbientMode, {
        now: new Date(2026, 6, 12, 9, 5), // Sun Jul 12 2026, 09:05
        presence,
        briefCount,
        feedCount,
      }),
    );

  it("renders a padded 24h clock and a full date", () => {
    const html = render(parsePresence({ state: "away", at_machine: false }));
    expect(html).toContain("09:05");
    expect(html).toContain("Sunday, July 12");
  });

  it("shows the presence label and only non-zero counts", () => {
    const html = render(parsePresence({ state: "away" }), 3, 0);
    expect(html).toContain("Away");
    expect(html).toContain("3 in your brief");
    expect(html).not.toContain("live feeds"); // feedCount 0 -> hidden
  });

  it("labels a null presence as 'Standing by', never fabricating a state", () => {
    expect(render(null)).toContain("Standing by");
  });

  it("invites the user to wake the console", () => {
    expect(render(parsePresence({ state: "away" }))).toContain("wake the console");
  });
});

import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import AppDeckPanel from "../components/AppDeckPanel";
import type { AppFeed } from "../core/state";

/* helpers ------------------------------------------------------------------ */
const render = (
  runningApps: ReadonlySet<string>,
  appFeeds: Record<string, AppFeed> = {},
) => renderToStaticMarkup(createElement(AppDeckPanel, { runningApps, appFeeds }));

function feed(running: boolean): AppFeed {
  return {
    running,
    brief: "",
    items: [],
    fetchedAt: null,
    feedsOk: null,
    feedsFailed: null,
    updatedAt: 0,
    topics: {},
  } as AppFeed;
}

describe("AppDeckPanel", () => {
  it("always renders the curated fleet grouped by category (all IDLE with no live apps)", () => {
    const html = render(new Set());
    expect(html).toContain("APP // DECK");
    expect(html).toContain("REVIEW ONLY");
    // Every fleet member is present (23-app toolkit incl. the on-device-AI apps).
    for (const name of [
      "Summarize", "Classify", "Extract", "Rewrite", "Explain", "Keywords", "Titlegen", "Sentiment",
      "Codeglass", "JSONPath", "RegexPad", "Diffscope", "Datalint", "CSVLens", "Numbase",
      "Hashkit", "JWTPeek", "Entropy", "Textkit", "Markmap", "Cronwise", "Timewarp", "Colorlab",
    ]) {
      expect(html).toContain(name);
    }
    // Their exposed tools are shown.
    expect(html).toContain("summarize.run");
    expect(html).toContain("codeglass.metrics");
    expect(html).toContain("timewarp.convert");
    // Category group headers are present (AI leads).
    for (const cat of ["AI", "DEV", "DATA", "SECURITY", "TEXT", "TIME", "DESIGN"]) {
      expect(html).toContain(`>${cat}<`);
    }
    // 0 of 23 live.
    expect(html).toContain(">0<");
    expect(html).toContain(">23<");
    // No app card is in the LIVE state (the state pill carries `deck-state live`).
    expect(html).not.toContain("deck-state live");
  });

  it("marks an app LIVE when it is in runningApps", () => {
    const html = render(new Set(["hashkit"]));
    expect(html).toContain(">1<"); // live count
    expect(html).toContain("deck-state live");
  });

  it("also treats appFeeds[name].running as live (even if not in the set)", () => {
    const html = render(new Set(), { textkit: feed(true) });
    expect(html).toContain(">1<");
    expect(html).toContain("deck-state live");
  });

  it("is review-only — renders no launch/action button", () => {
    const html = render(new Set(["codeglass"]));
    expect(html).not.toContain("<button");
  });
});

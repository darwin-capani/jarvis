import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import AppDeckPanel from "../components/AppDeckPanel";
import { appManifestIssueLine, APP_MANIFEST_ISSUE_CAP, type TelemetryEnvelope } from "../core/events";
import { initialState, reduce, type AppFeed, type HudState } from "../core/state";

/* helpers ------------------------------------------------------------------ */
const render = (
  runningApps: ReadonlySet<string>,
  appFeeds: Record<string, AppFeed> = {},
  manifestIssues: string[] = [],
) =>
  renderToStaticMarkup(
    createElement(AppDeckPanel, { runningApps, appFeeds, manifestIssues }),
  );

let counter = 0;
function env(event: string, data: Record<string, unknown> = {}): TelemetryEnvelope {
  counter += 1;
  return {
    ts: `2026-07-11T12:00:${String(counter % 60).padStart(2, "0")}Z`,
    source: "system",
    event,
    data,
  };
}
function tel(state: HudState, e: TelemetryEnvelope, at = 1000): HudState {
  return reduce(state, { type: "telemetry", envelope: e, at });
}
function connected(at = 0): HudState {
  return reduce(initialState(), { type: "ws.connected", at });
}

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

  it("renders manifest install errors when present; hides the block when empty", () => {
    const html = render(new Set(), {}, ["broken-app: missing [app] name"]);
    expect(html).toContain("MANIFEST ERRORS");
    expect(html).toContain("broken-app: missing [app] name");
    // No issues -> the block does not render at all.
    expect(render(new Set())).not.toContain("MANIFEST ERRORS");
  });
});

/* ------------------------------------------------------------------------ *
 * app.manifest_invalid — AppRegistry::discover SKIPPED an apps/<dir>/ whose  *
 * manifest.toml failed to parse/validate. The daemon emits {name, error}     *
 * ONCE at startup (after telemetry::init — the ordering fix); the reducer    *
 * accumulates deduped, capped "dir: error" lines the App Deck renders as     *
 * install errors instead of a silently absent app.                          *
 * ------------------------------------------------------------------------ */
describe("appManifestIssueLine (defensive)", () => {
  it("formats the daemon's {name, error} payload", () => {
    expect(appManifestIssueLine({ name: "bad-app", error: "entry escapes app dir" })).toBe(
      "bad-app: entry escapes app dir",
    );
  });
  it("falls back to a generic reason when error is absent/empty", () => {
    expect(appManifestIssueLine({ name: "bad-app" })).toBe("bad-app: invalid manifest");
    expect(appManifestIssueLine({ name: "bad-app", error: "" })).toBe("bad-app: invalid manifest");
  });
  it("returns null with no usable name (nothing to point the user at)", () => {
    expect(appManifestIssueLine({})).toBeNull();
    expect(appManifestIssueLine({ name: "", error: "x" })).toBeNull();
    expect(appManifestIssueLine({ name: 42, error: "x" })).toBeNull();
  });
});

describe("app.manifest_invalid reducer", () => {
  it("accumulates newest-first, dedupes, and caps", () => {
    let s = tel(connected(), env("app.manifest_invalid", { name: "a", error: "bad toml" }));
    s = tel(s, env("app.manifest_invalid", { name: "b", error: "no entry" }));
    // The same broken manifest re-reported (e.g. a daemon restart) collapses.
    s = tel(s, env("app.manifest_invalid", { name: "a", error: "bad toml" }));
    expect(s.appManifestIssues).toEqual(["a: bad toml", "b: no entry"]);
    for (let i = 0; i < APP_MANIFEST_ISSUE_CAP + 5; i++) {
      s = tel(s, env("app.manifest_invalid", { name: `app-${i}`, error: "x" }));
    }
    expect(s.appManifestIssues.length).toBe(APP_MANIFEST_ISSUE_CAP);
  });

  it("ignores a frame with no usable name (no churn)", () => {
    const before = tel(connected(), env("app.manifest_invalid", { name: "a", error: "x" }));
    const after = tel(before, env("app.manifest_invalid", { error: "orphan" }));
    expect(after).toBe(before);
  });

  it("does not disturb the running-app tracking (a skipped app never registers)", () => {
    const s = tel(connected(), env("app.manifest_invalid", { name: "bad-app", error: "x" }));
    expect(s.runningApps.has("bad-app")).toBe(false);
    expect("bad-app" in s.appFeeds).toBe(false);
  });
});

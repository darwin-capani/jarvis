import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import ResearchNotebooksPanel from "../components/ResearchNotebooksPanel";
import {
  notebookVerbLabel,
  parseNotebookActivity,
  type NotebookActivity,
  type TelemetryEnvelope,
} from "../core/events";
import { HudState, initialState, reduce } from "../core/state";

/* helpers ------------------------------------------------------------------ */

let counter = 0;
function env(
  event: string,
  data: Record<string, unknown> = {},
  source = "system",
): TelemetryEnvelope {
  counter += 1;
  return {
    ts: `2026-06-16T12:00:${String(counter % 60).padStart(2, "0")}Z`,
    source,
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

function render(activity: NotebookActivity | null): string {
  return renderToStaticMarkup(createElement(ResearchNotebooksPanel, { activity }));
}

/* fixtures — mirror daemon/src/notebook.rs NotebookCard wire shape ----------- */

/** A REVISIT that surfaces a real saved run, grounded in two REAL fetched
 *  sources (the run-local id + the page title + the real URL the run fetched).
 *  This mirrors the persisted, grounded citations — never a fabricated source. */
const revisitCited: Record<string, unknown> = {
  verb: "revisit",
  card: {
    verb: "revisit",
    topic: "the JWST",
    snippet: "The JWST sees in the infrared, peering through cosmic dust.",
    run_count: 2,
    citations: [
      { source_id: 1, title: "NASA — JWST Overview", url: "https://nasa.gov/jwst" },
      { source_id: 2, title: "ESA — Webb Facts", url: "https://esa.int/webb" },
    ],
  },
};

/** A SAVE of the most-recent run. */
const savedCard: Record<string, unknown> = {
  verb: "saved",
  card: {
    verb: "saved",
    topic: "black holes",
    snippet: "Saved the latest run on black holes.",
    run_count: 1,
    citations: [
      { source_id: 5, title: "Event Horizon Telescope", url: "https://eventhorizontelescope.org" },
    ],
  },
};

/** A no-op SAVE_NONE — nothing real to surface, so card is null. */
const saveNone: Record<string, unknown> = { verb: "save_none", card: null };

/** An honest-empty REVISIT — a topic with no saved runs yet. */
const revisitEmpty: Record<string, unknown> = {
  verb: "revisit",
  card: { verb: "revisit", topic: "fusion", snippet: "", run_count: 0, citations: [] },
};

/* ------------------------------------------------------------------------- */

describe("parseNotebookActivity", () => {
  it("records the verb + the real fetched-source citations of a revisited run", () => {
    const a = parseNotebookActivity(revisitCited);
    expect(a.verb).toBe("revisit");
    expect(a.card).not.toBeNull();
    expect(a.card?.topic).toBe("the JWST");
    expect(a.card?.runCount).toBe(2);
    expect(a.card?.citations).toHaveLength(2);
    expect(a.card?.citations[0]).toEqual({
      sourceId: 1,
      title: "NASA — JWST Overview",
      url: "https://nasa.gov/jwst",
    });
    expect(a.card?.citations[1].url).toBe("https://esa.int/webb");
  });

  it("a save_none/forget_none/error carries NO card (nothing to surface)", () => {
    expect(parseNotebookActivity(saveNone).card).toBeNull();
    expect(parseNotebookActivity({ verb: "forget_none", card: null }).card).toBeNull();
    expect(parseNotebookActivity({ verb: "error", card: null }).card).toBeNull();
  });

  it("an honest-empty revisit yields a card with 0 runs + no citations + no snippet", () => {
    const a = parseNotebookActivity(revisitEmpty);
    expect(a.card?.runCount).toBe(0);
    expect(a.card?.citations).toHaveLength(0);
    expect(a.card?.snippet).toBe("");
  });

  it("drops a citation with no url AND no title (never fabricates a source)", () => {
    const a = parseNotebookActivity({
      verb: "revisit",
      card: {
        verb: "revisit",
        topic: "x",
        snippet: "s",
        run_count: 1,
        citations: [
          { source_id: 1, title: "Real", url: "https://real.test" }, // kept
          { source_id: 2, title: "", url: "" }, // dropped (nothing to point at)
          { source_id: 3 }, // dropped
          { source_id: 4, title: "Title only", url: "" }, // kept (has a title)
        ],
      },
    });
    expect(a.card?.citations).toHaveLength(2);
    expect(a.card?.citations[0].url).toBe("https://real.test");
    expect(a.card?.citations[1].title).toBe("Title only");
  });

  it("drops an unknown verb rather than rendering a bad badge", () => {
    const a = parseNotebookActivity({ verb: "obliterate", card: { topic: "x" } });
    expect(a.verb).toBe("error");
    expect(a.card).toBeNull();
  });

  it("never throws on junk and yields an honest no-card record", () => {
    const a = parseNotebookActivity({ verb: "revisit", card: "not-an-object" });
    expect(a.verb).toBe("revisit");
    expect(a.card).toBeNull();
  });
});

describe("notebookVerbLabel", () => {
  it("maps each verb to an honest past-tense activity label", () => {
    expect(notebookVerbLabel("saved")).toBe("SAVED");
    expect(notebookVerbLabel("revisit")).toBe("REVISITED");
    expect(notebookVerbLabel("list")).toBe("SHELF");
    expect(notebookVerbLabel("forget")).toBe("FORGOTTEN");
  });
});

describe("notebook.card reducer", () => {
  it("folds a revisited card with its real citations onto state", () => {
    const s = tel(connected(), env("notebook.card", revisitCited));
    expect(s.notebook?.card).not.toBeNull();
    expect(s.notebook?.card?.citations).toHaveLength(2);
    expect(s.notebook?.card?.citations[0].url).toBe("https://nasa.gov/jwst");
  });

  it("a fresh card REPLACES the prior activity", () => {
    let s = tel(connected(), env("notebook.card", revisitCited));
    expect(s.notebook?.card?.topic).toBe("the JWST");
    s = tel(s, env("notebook.card", savedCard));
    expect(s.notebook?.card?.topic).toBe("black holes");
    expect(s.notebook?.card?.verb).toBe("saved");
  });

  it("a save_none no-op KEEPS the prior real card (never blanks the panel)", () => {
    let s = tel(connected(), env("notebook.card", revisitCited));
    expect(s.notebook?.card?.topic).toBe("the JWST");
    const before = s;
    // A bare "save this research" with no recent run: nothing to surface.
    s = tel(s, env("notebook.card", saveNone));
    expect(s.notebook?.card?.topic).toBe("the JWST"); // prior card preserved
    // ...AND the SAME state reference so React bails the re-render — a bare `{ ...s }`
    // clone would churn a full-tree re-render (incl. the WebGL core) on every no-op.
    expect(s).toBe(before);
  });

  it("a no-op before any card leaves nothing to render (stays null)", () => {
    const base = connected();
    const s = tel(base, env("notebook.card", saveNone));
    expect(s.notebook).toBeNull();
  });
});

describe("ResearchNotebooksPanel", () => {
  it("renders nothing before any notebook command", () => {
    expect(render(null)).toBe("");
  });

  it("renders nothing for a no-card no-op", () => {
    expect(render(parseNotebookActivity(saveNone))).toBe("");
  });

  it("surfaces the verb, topic, snippet, and the REAL fetched citations", () => {
    const html = render(parseNotebookActivity(revisitCited));
    expect(html).toContain("RESEARCH // NOTEBOOKS");
    expect(html).toContain("REVISITED");
    expect(html).toContain("the JWST");
    expect(html).toContain("The JWST sees in the infrared");
    // Every real citation locator is surfaced.
    expect(html).toContain("NASA — JWST Overview");
    expect(html).toContain("https://nasa.gov/jwst");
    expect(html).toContain("ESA — Webb Facts");
    expect(html).toContain("https://esa.int/webb");
    expect(html).toContain("2 CITED");
    // Honest framing.
    expect(html.toLowerCase()).toContain("never invented");
  });

  it("shows the honest-empty state for a topic with no saved runs", () => {
    const html = render(parseNotebookActivity(revisitEmpty));
    expect(html.toLowerCase()).toContain("nothing saved on this topic yet");
    expect(html).not.toContain("CITED");
  });

  it("shows the honest no-grounded-sources label when a run cited none", () => {
    const html = render(
      parseNotebookActivity({
        verb: "saved",
        card: { verb: "saved", topic: "t", snippet: "some synthesis", run_count: 1, citations: [] },
      }),
    );
    expect(html).toContain("NO GROUNDED SOURCES");
    expect(html.toLowerCase()).toContain("no citation is invented");
  });

  it("is SECRET-FREE: only the honest fields render, never a leaked secret", () => {
    const a = parseNotebookActivity({
      verb: "revisit",
      card: {
        verb: "revisit",
        topic: "the JWST",
        snippet: "real snippet",
        run_count: 1,
        // A daemon that (incorrectly) tried to ride a secret/embedding alongside
        // the honest locators: the parser reads ONLY the three honest fields.
        citations: [
          {
            source_id: 1,
            title: "Real",
            url: "https://real.test",
            embedding: [0.123456, 0.654321],
            raw_content: "RAW_UNREDACTED_BODY",
            secret: "API_KEY_LEAK",
          },
        ],
      },
    });
    expect(Object.keys(a.card!.citations[0]).sort()).toEqual(["sourceId", "title", "url"]);
    const html = render(a);
    expect(html).toContain("real snippet");
    expect(html).toContain("https://real.test");
    expect(html).not.toContain("0.123456");
    expect(html).not.toContain("RAW_UNREDACTED_BODY");
    expect(html).not.toContain("API_KEY_LEAK");
    expect(html).not.toContain("embedding");
  });
});

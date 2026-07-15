import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import QuickLookOverlay from "../components/QuickLookOverlay";
import {
  parseArtifactPeek,
  isKnownArtifactKind,
  ARTIFACT_CITATIONS_CAP,
  ARTIFACT_PEEK_EVENT,
  type ArtifactPeek,
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

function render(artifact: ArtifactPeek | null): string {
  return renderToStaticMarkup(createElement(QuickLookOverlay, { artifact }));
}

/* fixtures — mirror daemon/src/artifact.rs ArtifactRef::to_frame wire shape --- */

/** A CITED report artifact, exactly as artifact.rs emits it. */
const citedReport: Record<string, unknown> = {
  id: 7,
  kind: "report",
  title: "JWST deep field",
  ts: "2026-06-16T12:00:00Z",
  preview: "3 sections, 2 citations",
  agent: "darwin",
  uncited: false,
  citation_count: 2,
  citations: [
    { title: "JWST overview", url: "https://nasa.gov/jwst" },
    { title: "Deep field", url: "https://nasa.gov/deepfield" },
  ],
};

/** An UNCITED chart artifact (live system metrics genuinely cite nothing). */
const uncitedChart: Record<string, unknown> = {
  id: 8,
  kind: "chart",
  title: "System load",
  ts: "2026-06-16T12:00:01Z",
  preview: "1 series, 2 points",
  agent: "darwin",
  uncited: true,
  citation_count: 0,
  citations: [],
};

/** A code_diff artifact grounded in real file locators. */
const codeDiff: Record<string, unknown> = {
  id: 9,
  kind: "code_diff",
  title: "proposal 1750000000",
  ts: "2026-06-16T12:00:02Z",
  preview: "diff: +5/-2 lines, 1 hunk",
  agent: "steve",
  uncited: false,
  citation_count: 1,
  citations: [{ title: "src/config.rs", url: "src/config.rs:120" }],
};

/* -------------------------------------------------------------------------- */
/* PARSER — parseArtifactPeek                                                  */
/* -------------------------------------------------------------------------- */

describe("parseArtifactPeek", () => {
  it("parses a cited artifact with its real citations verbatim", () => {
    const a = parseArtifactPeek(citedReport)!;
    expect(a).not.toBeNull();
    expect(a.id).toBe(7);
    expect(a.kind).toBe("report");
    expect(a.title).toBe("JWST deep field");
    expect(a.preview).toBe("3 sections, 2 citations");
    expect(a.agent).toBe("darwin");
    expect(a.uncited).toBe(false);
    expect(a.citationCount).toBe(2);
    expect(a.citations).toHaveLength(2);
    expect(a.citations[0]).toEqual({ title: "JWST overview", url: "https://nasa.gov/jwst" });
    expect(a.citations[1].url).toBe("https://nasa.gov/deepfield");
  });

  it("parses an uncited artifact as uncited, never fabricating a source", () => {
    const a = parseArtifactPeek(uncitedChart)!;
    expect(a.uncited).toBe(true);
    expect(a.citationCount).toBe(0);
    expect(a.citations).toHaveLength(0);
  });

  it("re-derives uncited from the surviving citations, never trusting the wire flag", () => {
    // Spoofed uncited:false over an EMPTY citation list -> honest UNCITED.
    const spoofedCited = parseArtifactPeek({
      ...uncitedChart,
      uncited: false,
      citation_count: 3, // lying total
      citations: [],
    })!;
    expect(spoofedCited.uncited).toBe(true);
    expect(spoofedCited.citations).toHaveLength(0);

    // Spoofed uncited:true over REAL citations -> still surfaces them.
    const spoofedUncited = parseArtifactPeek({
      ...citedReport,
      uncited: true,
    })!;
    expect(spoofedUncited.uncited).toBe(false);
    expect(spoofedUncited.citations).toHaveLength(2);
  });

  it("drops a citation with no usable locator, never fabricating one", () => {
    const a = parseArtifactPeek({
      ...codeDiff,
      citation_count: 2,
      citations: [
        { title: "src/config.rs", url: "src/config.rs:120" },
        { title: "   ", url: "" }, // both blank -> dropped
      ],
    })!;
    expect(a.citations).toHaveLength(1);
    expect(a.citations[0].title).toBe("src/config.rs");
    // citationCount is the daemon's honest total, floored at the surviving count.
    expect(a.citationCount).toBe(2);
  });

  it("floors citationCount at the surviving citation count", () => {
    const a = parseArtifactPeek({
      ...citedReport,
      citation_count: 0, // understated total
    })!;
    expect(a.citationCount).toBe(2);
  });

  it("returns null for a junk frame with no kind (dropped, never rendered)", () => {
    expect(parseArtifactPeek({ id: 1, title: "x" })).toBeNull();
    expect(parseArtifactPeek({ kind: "   " })).toBeNull();
    expect(parseArtifactPeek({})).toBeNull();
  });

  it("bounds the citations to the view cap", () => {
    const many = Array.from({ length: ARTIFACT_CITATIONS_CAP + 20 }, (_, i) => ({
      title: `s${i}`,
      url: `https://x/${i}`,
    }));
    const a = parseArtifactPeek({ ...citedReport, citations: many, citation_count: many.length })!;
    expect(a.citations.length).toBeLessThanOrEqual(ARTIFACT_CITATIONS_CAP);
  });

  it("carries an unknown kind as a generic (never guessed) kind string", () => {
    const a = parseArtifactPeek({ ...uncitedChart, kind: "forecast_v2" })!;
    expect(a.kind).toBe("forecast_v2");
    expect(isKnownArtifactKind(a.kind)).toBe(false);
    expect(isKnownArtifactKind("report")).toBe(true);
  });
});

/* -------------------------------------------------------------------------- */
/* REDUCER — artifact.peek                                                     */
/* -------------------------------------------------------------------------- */

describe("reducer: artifact.peek", () => {
  it("sets artifactPeek from a valid frame", () => {
    const s0 = connected();
    expect(s0.artifactPeek).toBeNull();
    const s1 = tel(s0, env(ARTIFACT_PEEK_EVENT, citedReport));
    expect(s1.artifactPeek).not.toBeNull();
    expect(s1.artifactPeek!.title).toBe("JWST deep field");
    expect(s1.artifactPeek!.uncited).toBe(false);
  });

  it("a fresh peek replaces the prior one (shows the last thing peeked)", () => {
    let s = connected();
    s = tel(s, env(ARTIFACT_PEEK_EVENT, citedReport));
    s = tel(s, env(ARTIFACT_PEEK_EVENT, uncitedChart));
    expect(s.artifactPeek!.id).toBe(8);
    expect(s.artifactPeek!.kind).toBe("chart");
    expect(s.artifactPeek!.uncited).toBe(true);
  });

  it("drops a junk frame without churning the tree (same reference)", () => {
    const s0 = tel(connected(), env(ARTIFACT_PEEK_EVENT, citedReport));
    const s1 = tel(s0, env(ARTIFACT_PEEK_EVENT, { id: 1 })); // no kind -> parser null
    expect(s1).toBe(s0);
    expect(s1.artifactPeek!.title).toBe("JWST deep field"); // prior peek preserved
  });
});

/* -------------------------------------------------------------------------- */
/* OVERLAY — renders any kind with a provenance footer                        */
/* -------------------------------------------------------------------------- */

describe("QuickLookOverlay render", () => {
  it("renders nothing when there is no artifact", () => {
    expect(render(null)).toBe("");
  });

  it("renders a cited report with its real citations in the provenance footer", () => {
    const html = render(parseArtifactPeek(citedReport));
    expect(html).toContain("QUICKLOOK // REPORT");
    expect(html).toContain("JWST deep field");
    expect(html).toContain("3 sections, 2 citations");
    expect(html).toContain("PROVENANCE");
    expect(html).toContain("by darwin");
    expect(html).toContain("2 CITED");
    // The REAL citation locators are shown, never fabricated.
    expect(html).toContain("JWST overview");
    expect(html).toContain("https://nasa.gov/jwst");
    // No UNCITED pill (the foot copy mentions the word, so target the pill class).
    expect(html).not.toContain("quicklook-pill uncited");
  });

  it("renders an uncited artifact as UNCITED, never a fabricated source", () => {
    const html = render(parseArtifactPeek(uncitedChart));
    expect(html).toContain("QUICKLOOK // CHART");
    expect(html).toContain("System load");
    expect(html).toContain("UNCITED");
    expect(html).toContain("by darwin");
    // No fabricated citation host anywhere.
    expect(html).not.toContain("http");
    // No "N CITED" count pill (the cited pill; " CITED" with a leading space is not
    // a substring of "UNCITED").
    expect(html).not.toContain(" CITED");
  });

  it("renders a code_diff with its real file-locator citations", () => {
    const html = render(parseArtifactPeek(codeDiff));
    expect(html).toContain("QUICKLOOK // CODE DIFF");
    expect(html).toContain("diff: +5/-2 lines, 1 hunk");
    expect(html).toContain("by steve");
    expect(html).toContain("src/config.rs");
    expect(html).toContain("src/config.rs:120");
  });

  it("renders an unknown kind generically (uppercased, never guessed)", () => {
    const html = render(parseArtifactPeek({ ...uncitedChart, kind: "mystery" }));
    expect(html).toContain("QUICKLOOK // MYSTERY");
  });

  it("shows the honest total when the citation preview is bounded shorter", () => {
    // citationCount 5 but only 1 citation survives -> honest "5 sources" note.
    const a = parseArtifactPeek({
      ...codeDiff,
      citation_count: 5,
      citations: [{ title: "src/config.rs", url: "src/config.rs:120" }],
    })!;
    const html = render(a);
    expect(html).toContain("5 CITED");
    expect(html).toContain("+ 4 more");
  });
});

import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import ProvenanceLedgerPanel from "../components/ProvenanceLedgerPanel";
import {
  parseResearchProvenance,
  PROVENANCE_LEDGER_CAP,
  PROVENANCE_ROWS_CAP,
  type ResearchProvenance,
  type TelemetryEnvelope,
} from "../core/events";
import { initialState, reduce, type HudState } from "../core/state";

let counter = 0;
function env(event: string, data: Record<string, unknown>, source = "system"): TelemetryEnvelope {
  counter += 1;
  return { ts: `2026-07-13T00:00:${String(counter % 60).padStart(2, "0")}Z`, source, event, data };
}
function connected() {
  return reduce(initialState(), { type: "ws.connected", at: 0 });
}
function tel(state: HudState, e: TelemetryEnvelope) {
  return reduce(state, { type: "telemetry", envelope: e, at: 1000 });
}

const wireRun = {
  question: "What causes inflation?",
  sources_fetched: 2,
  claims_total: 3,
  claims_grounded: 2,
  claims_ungrounded: 1,
  claims_omitted: 0,
  truncated: false,
  claims: [
    { text: "An unsourced assertion", grounded: false, source_id: 0, source_title: "", source_url: "" },
    {
      text: "Monetary expansion raises prices",
      grounded: true,
      source_id: 1,
      source_title: "Money and prices",
      source_url: "https://example.com/a",
    },
  ],
};

describe("parseResearchProvenance (never pads a ledger)", () => {
  it("parses the daemon's wire shape", () => {
    const run = parseResearchProvenance(wireRun);
    expect(run).not.toBeNull();
    expect(run?.question).toBe("What causes inflation?");
    expect(run?.sourcesFetched).toBe(2);
    expect(run?.claimsTotal).toBe(3);
    expect(run?.claimsGrounded).toBe(2);
    expect(run?.claimsUngrounded).toBe(1);
    expect(run?.claimsOmitted).toBe(0);
    expect(run?.truncated).toBe(false);
    expect(run?.claims).toEqual([
      { text: "An unsourced assertion", grounded: false, sourceId: 0, sourceTitle: "", sourceUrl: "" },
      {
        text: "Monetary expansion raises prices",
        grounded: true,
        sourceId: 1,
        sourceTitle: "Money and prices",
        sourceUrl: "https://example.com/a",
      },
    ]);
  });

  it("returns null for a frame without the claim counts", () => {
    expect(parseResearchProvenance({})).toBeNull();
    expect(parseResearchProvenance({ question: "q", claims: [] })).toBeNull();
    expect(parseResearchProvenance({ claims_total: "three", claims_grounded: 1, claims_ungrounded: 0 })).toBeNull();
  });

  it("drops malformed rows and never invents grounding", () => {
    const run = parseResearchProvenance({
      claims_total: 2,
      claims_grounded: -5,
      claims_ungrounded: 1,
      claims: [
        { text: "ok", grounded: "yes", source_id: -2 }, // non-boolean grounded -> false
        { grounded: true }, // no text -> dropped
        "junk",
      ],
    });
    expect(run).not.toBeNull();
    expect(run?.claimsGrounded).toBe(0); // negative clamps
    expect(run?.claims).toHaveLength(1);
    expect(run?.claims[0].grounded).toBe(false);
    expect(run?.claims[0].sourceId).toBe(0);
  });

  it("caps rows and bounds strings — the persistent ring never trusts the wire", () => {
    const bloated = {
      claims_total: 100,
      claims_grounded: 0,
      claims_ungrounded: 100,
      claims: Array.from({ length: 100 }, (_, i) => ({
        text: `claim ${i} ${"z".repeat(5000)}`,
        grounded: false,
        source_id: 0,
        source_title: "t".repeat(5000),
        source_url: "https://example.com/?q=" + "u".repeat(5000),
      })),
      question: "q".repeat(5000),
    };
    const run = parseResearchProvenance(bloated);
    expect(run).not.toBeNull();
    expect(run?.claims).toHaveLength(PROVENANCE_ROWS_CAP);
    expect(run?.question.length).toBeLessThanOrEqual(240);
    for (const c of run?.claims ?? []) {
      expect(c.text.length).toBeLessThanOrEqual(240);
      expect(c.sourceTitle.length).toBeLessThanOrEqual(240);
      expect(c.sourceUrl.length).toBeLessThanOrEqual(240);
    }
  });
});

describe("research.provenance reducer (bounded newest-first ring)", () => {
  it("accumulates runs newest-first and holds the cap", () => {
    let s = connected();
    expect(s.researchProvenance).toEqual([]);
    for (let i = 0; i < PROVENANCE_LEDGER_CAP + 3; i++) {
      s = tel(s, env("research.provenance", { ...wireRun, question: `q${i}` }));
    }
    expect(s.researchProvenance).toHaveLength(PROVENANCE_LEDGER_CAP);
    expect(s.researchProvenance[0].question).toBe(`q${PROVENANCE_LEDGER_CAP + 2}`);
  });

  it("drops a malformed frame instead of padding the ledger", () => {
    let s = connected();
    s = tel(s, env("research.provenance", { junk: true }));
    expect(s.researchProvenance).toEqual([]);
  });
});

describe("ProvenanceLedgerPanel", () => {
  const render = (runs: ResearchProvenance[]) =>
    renderToStaticMarkup(createElement(ProvenanceLedgerPanel, { runs }));

  it("renders nothing before the first run", () => {
    expect(render([])).toBe("");
  });

  it("flags set-aside claims and shows the honest fraction", () => {
    const run = parseResearchProvenance(wireRun);
    const html = render([run as ResearchProvenance]);
    expect(html).toContain("2/3 GROUNDED");
    expect(html).toContain("SET ASIDE · UNSOURCED");
    expect(html).toContain("An unsourced assertion");
    expect(html).toContain("Money and prices");
    expect(html).toContain("2 sources fetched");
    expect(html).toContain("never presented as fact");
  });

  it("shows ALL GROUNDED only when nothing was set aside, and discloses truncation", () => {
    const clean = parseResearchProvenance({
      ...wireRun,
      claims_ungrounded: 0,
      claims_grounded: 3,
      truncated: true,
      claims: [wireRun.claims[1]],
    }) as ResearchProvenance;
    const html = render([clean]);
    expect(html).toContain("ALL GROUNDED");
    expect(html).not.toContain("SET ASIDE");
    expect(html).toContain("truncated by its budget");
  });

  it("discloses rows the daemon cap dropped", () => {
    const run = parseResearchProvenance({
      ...wireRun,
      claims_total: 20,
      claims_ungrounded: 18,
      claims_grounded: 2,
      claims_omitted: 8,
    }) as ResearchProvenance;
    const html = render([run]);
    expect(html).toContain("8 more claims not shown");
  });
});

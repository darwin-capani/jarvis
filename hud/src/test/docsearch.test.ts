import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import DocSearchPanel from "../components/DocSearchPanel";
import {
  parseDocIndexStatus,
  parseDocSearchEmbedder,
  parseDocSearchReindexNeeded,
  parseDocSearchResult,
  parsePdfJailAvailable,
  parseSpotlightAvailable,
  type DocIndexStatus,
  type DocSearchResult,
  type TelemetryEnvelope,
} from "../core/events";
import { HudState, initialState, reduce } from "../core/state";

/* helpers ------------------------------------------------------------------ */

let counter = 0;
function env(
  event: string,
  data: Record<string, unknown> = {},
  source = "local",
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

/** A realistic docsearch.indexed payload: a fully-embedded (neural) index. */
const indexedNeural: Record<string, unknown> = {
  files: 12,
  chunks: 240,
  embedded_chunks: 240,
};

/** A docsearch.indexed payload where the embedder was down at index time, so
 *  only some chunks carry a vector — search over the rest falls back to BM25. */
const indexedPartial: Record<string, unknown> = {
  files: 12,
  chunks: 240,
  embedded_chunks: 0,
};

/** A realistic docsearch.searched payload: two CITED hits + the method that ran.
 *  The hits cite real (test) file paths + offsets + snippets + scores. */
const searchedNeural: Record<string, unknown> = {
  query: "renewal clause",
  method: "neural-embedding",
  hits: [
    {
      file_path: "/Users/me/notes/lease.md",
      root: "/Users/me/notes",
      byte_offset: 1840,
      snippet: "The renewal clause auto-extends the term by twelve months.",
      score: 0.91,
    },
    {
      file_path: "/Users/me/notes/addendum.txt",
      root: "/Users/me/notes",
      byte_offset: 60,
      snippet: "Renewal requires written notice thirty days prior.",
      score: 0.74,
    },
  ],
};

/* ------------------------------------------------------------------------ *
 * parseDocIndexStatus — counts only, never null, embedded clamped to chunks. *
 * ------------------------------------------------------------------------ */
describe("parseDocIndexStatus (defensive, counts-only)", () => {
  it("parses a well-formed neural index", () => {
    const s = parseDocIndexStatus(indexedNeural);
    expect(s).toEqual({ files: 12, chunks: 240, embeddedChunks: 240 });
  });

  it("defaults all counts to 0 (honest empty index) when absent", () => {
    expect(parseDocIndexStatus({})).toEqual({ files: 0, chunks: 0, embeddedChunks: 0 });
  });

  it("clamps embeddedChunks to <= chunks (never over-claims)", () => {
    // A hostile/buggy payload claiming more embedded than total must not let the
    // panel say search is "fully neural" when it isn't.
    const s = parseDocIndexStatus({ files: 1, chunks: 10, embedded_chunks: 9999 });
    expect(s.embeddedChunks).toBe(10);
  });

  it("floors/clamps negative and fractional counts to a sane non-negative int", () => {
    const s = parseDocIndexStatus({ files: -5, chunks: 4.9, embedded_chunks: -1 });
    expect(s.files).toBe(0);
    expect(s.chunks).toBe(4);
    expect(s.embeddedChunks).toBe(0);
  });

  it("never throws on junk", () => {
    expect(() => parseDocIndexStatus({ files: "lots", chunks: null })).not.toThrow();
    expect(parseDocIndexStatus({ files: "lots" })).toEqual({
      files: 0,
      chunks: 0,
      embeddedChunks: 0,
    });
  });
});

/* ------------------------------------------------------------------------ *
 * parseDocSearchResult — only REAL hits, honest method, never fabricates.    *
 * ------------------------------------------------------------------------ */
describe("parseDocSearchResult (defensive, cite-only)", () => {
  it("parses a well-formed neural search result with cited hits", () => {
    const r = parseDocSearchResult(searchedNeural);
    expect(r.query).toBe("renewal clause");
    expect(r.method).toBe("neural-embedding");
    expect(r.hits.length).toBe(2);
    expect(r.hits[0]).toEqual({
      filePath: "/Users/me/notes/lease.md",
      root: "/Users/me/notes",
      byteOffset: 1840,
      snippet: "The renewal clause auto-extends the term by twelve months.",
      score: 0.91,
    });
  });

  it("defaults method to lexical-bm25 (never OVER-states as neural) when absent", () => {
    const r = parseDocSearchResult({ query: "x", hits: [] });
    expect(r.method).toBe("lexical-bm25");
  });

  it("drops a hit with no file_path (not a real citation) — never fabricates", () => {
    const r = parseDocSearchResult({
      query: "q",
      method: "lexical-bm25",
      hits: [
        { snippet: "orphan snippet with no file", score: 0.5 }, // no file_path -> dropped
        42, // non-object -> dropped
        { file_path: "/a/b.md", byte_offset: 3, snippet: "real", score: 0.4 },
      ],
    });
    expect(r.hits.length).toBe(1);
    expect(r.hits[0].filePath).toBe("/a/b.md");
    expect(r.hits[0].root).toBe(""); // missing root defaults to empty, not undefined
  });

  it("yields an honest empty result (nothing found), never null", () => {
    const r = parseDocSearchResult({ query: "no match", method: "neural-embedding", hits: [] });
    expect(r.hits).toEqual([]);
    expect(r.query).toBe("no match");
  });

  it("preserves an unknown future method verbatim", () => {
    const r = parseDocSearchResult({ query: "q", method: "hybrid-rerank", hits: [] });
    expect(r.method).toBe("hybrid-rerank");
  });

  it("never throws on junk", () => {
    expect(() => parseDocSearchResult({ hits: "nope" })).not.toThrow();
    expect(parseDocSearchResult({ hits: "nope" }).hits).toEqual([]);
  });
});

/* ------------------------------------------------------------------------ *
 * The reducer arms. docsearch.indexed sets the status; docsearch.searched     *
 * sets the cited result. Both NEVER null after a frame.                       *
 * ------------------------------------------------------------------------ */
describe("parsePdfJailAvailable (strict, never overclaims the jail)", () => {
  it("reports armed only on a literal JSON true", () => {
    expect(parsePdfJailAvailable({ pdfjail_available: true })).toBe(true);
    expect(parsePdfJailAvailable({ pdfjail_available: false })).toBe(false);
  });

  it("coerces absent/malformed/truthy-but-not-boolean to false (the safe direction)", () => {
    // Claiming the WEAKER guard when actually jailed is merely conservative;
    // claiming the jail is armed when it is not would hide a degraded install.
    expect(parsePdfJailAvailable({})).toBe(false);
    expect(parsePdfJailAvailable({ pdfjail_available: "true" })).toBe(false);
    expect(parsePdfJailAvailable({ pdfjail_available: 1 })).toBe(false);
    expect(parsePdfJailAvailable({ pdfjail_available: null })).toBe(false);
  });
});

describe("parseSpotlightAvailable (strict, never overclaims the bridge)", () => {
  it("reports the bridge answering only on a literal JSON true", () => {
    expect(parseSpotlightAvailable({ spotlight_available: true })).toBe(true);
    expect(parseSpotlightAvailable({ spotlight_available: false })).toBe(false);
  });

  it("coerces absent/malformed/truthy-but-not-boolean to false (the safe direction)", () => {
    // An OLDER daemon's status frame has no spotlight leg at all — that must
    // read as "not answering", never as a working integration.
    expect(parseSpotlightAvailable({ pdfjail_available: true })).toBe(false);
    expect(parseSpotlightAvailable({})).toBe(false);
    expect(parseSpotlightAvailable({ spotlight_available: "true" })).toBe(false);
    expect(parseSpotlightAvailable({ spotlight_available: 1 })).toBe(false);
    expect(parseSpotlightAvailable({ spotlight_available: null })).toBe(false);
  });
});

describe("parseDocSearchEmbedder (defensive, claims nothing when absent)", () => {
  it("parses a present vector-space stamp verbatim", () => {
    expect(parseDocSearchEmbedder({ embedder: "coreml-bge-small-en-v1.5" })).toBe(
      "coreml-bge-small-en-v1.5",
    );
    expect(parseDocSearchEmbedder({ embedder: "llm-meanpool:qwen3-4b" })).toBe(
      "llm-meanpool:qwen3-4b",
    );
  });

  it("coerces absent/null/empty/malformed to null (an older daemon / no observation / no vectors)", () => {
    expect(parseDocSearchEmbedder({})).toBeNull();
    expect(parseDocSearchEmbedder({ embedder: null })).toBeNull();
    expect(parseDocSearchEmbedder({ embedder: "" })).toBeNull();
    expect(parseDocSearchEmbedder({ embedder: 42 })).toBeNull();
  });
});

describe("parseDocSearchReindexNeeded (strict, never fabricates a warning)", () => {
  it("raises the warning only on a literal JSON true", () => {
    expect(parseDocSearchReindexNeeded({ reindex_needed: true })).toBe(true);
    expect(parseDocSearchReindexNeeded({ reindex_needed: false })).toBe(false);
  });

  it("coerces absent/malformed/truthy-but-not-boolean to false", () => {
    // An OLDER daemon predates the second embedder entirely, so its status
    // frame has no reindex_needed leg AND no mismatch is possible — false is
    // the CORRECT reading, not merely the conservative one.
    expect(parseDocSearchReindexNeeded({})).toBe(false);
    expect(parseDocSearchReindexNeeded({ reindex_needed: "true" })).toBe(false);
    expect(parseDocSearchReindexNeeded({ reindex_needed: 1 })).toBe(false);
    expect(parseDocSearchReindexNeeded({ reindex_needed: null })).toBe(false);
  });
});

describe("docsearch reducer", () => {
  it("starts with no index, no search, and unknown (null) pdf-jail + spotlight statuses", () => {
    const s = connected();
    expect(s.docIndex).toBeNull();
    expect(s.docSearch).toBeNull();
    expect(s.pdfJailAvailable).toBeNull();
    expect(s.spotlightAvailable).toBeNull();
    // The vector-space legs start unclaimed: no stamp known, no warning raised.
    expect(s.docSearchEmbedder).toBeNull();
    expect(s.docSearchReindexNeeded).toBe(false);
  });

  it("sets the vector-space legs from docsearch.status (latest-wins, both directions)", () => {
    let s = tel(
      connected(),
      env(
        "docsearch.status",
        {
          pdfjail_available: true,
          spotlight_available: false,
          embedder: "llm-meanpool:qwen3-4b",
          reindex_needed: true,
        },
        "system",
      ),
    );
    expect(s.docSearchEmbedder).toBe("llm-meanpool:qwen3-4b");
    expect(s.docSearchReindexNeeded).toBe(true);
    // A reindex under the active embedder clears the mismatch on the next
    // frame — never a sticky stale warning.
    s = tel(
      s,
      env(
        "docsearch.status",
        {
          pdfjail_available: true,
          spotlight_available: false,
          embedder: "coreml-bge-small-en-v1.5",
          reindex_needed: false,
        },
        "system",
      ),
    );
    expect(s.docSearchEmbedder).toBe("coreml-bge-small-en-v1.5");
    expect(s.docSearchReindexNeeded).toBe(false);
  });

  it("an OLDER daemon's status frame (no vector-space legs) claims no stamp and raises no warning", () => {
    const s = tel(connected(), env("docsearch.status", { pdfjail_available: true }, "system"));
    expect(s.docSearchEmbedder).toBeNull();
    expect(s.docSearchReindexNeeded).toBe(false);
  });

  it("sets the pdf-jail guard status from docsearch.status (system channel)", () => {
    let s = tel(connected(), env("docsearch.status", { pdfjail_available: true }, "system"));
    expect(s.pdfJailAvailable).toBe(true);
    // The fallback state arrives the same way (latest-wins) …
    s = tel(s, env("docsearch.status", { pdfjail_available: false }, "system"));
    expect(s.pdfJailAvailable).toBe(false);
  });

  it("sets the spotlight bridge status from the SAME docsearch.status frame", () => {
    let s = tel(
      connected(),
      env("docsearch.status", { pdfjail_available: true, spotlight_available: true }, "system"),
    );
    expect(s.spotlightAvailable).toBe(true);
    // Latest-wins, independently of the jail leg.
    s = tel(s, env("docsearch.status", { pdfjail_available: true, spotlight_available: false }, "system"));
    expect(s.spotlightAvailable).toBe(false);
    expect(s.pdfJailAvailable).toBe(true);
  });

  it("an OLDER daemon's status frame (no spotlight leg) reads as not-answering", () => {
    const s = tel(connected(), env("docsearch.status", { pdfjail_available: true }, "system"));
    expect(s.spotlightAvailable).toBe(false);
  });

  it("a malformed docsearch.status frame reads as the in-process fallback, not armed", () => {
    const s = tel(
      connected(),
      env("docsearch.status", { pdfjail_available: "yes", spotlight_available: "yes" }, "system"),
    );
    expect(s.pdfJailAvailable).toBe(false);
    expect(s.spotlightAvailable).toBe(false);
  });

  it("sets the index status from docsearch.indexed", () => {
    const s = tel(connected(), env("docsearch.indexed", indexedNeural));
    expect(s.docIndex).toEqual({ files: 12, chunks: 240, embeddedChunks: 240 });
  });

  it("sets the cited search result from docsearch.searched", () => {
    const s = tel(connected(), env("docsearch.searched", searchedNeural));
    expect(s.docSearch).not.toBeNull();
    expect(s.docSearch!.hits.length).toBe(2);
    expect(s.docSearch!.method).toBe("neural-embedding");
  });

  it("a later index/search replaces the prior one (latest-wins)", () => {
    let s = tel(connected(), env("docsearch.indexed", indexedNeural));
    s = tel(s, env("docsearch.indexed", indexedPartial));
    expect(s.docIndex!.embeddedChunks).toBe(0);
  });
});

/* ------------------------------------------------------------------------ *
 * The panel (rendered headlessly). PRIVATE + REVIEW-ONLY: cites real files,   *
 * honest method, no action button, honest off/empty states.                  *
 * ------------------------------------------------------------------------ */
describe("DocSearchPanel (cited, honest, review-only)", () => {
  const render = (
    index: DocIndexStatus | null,
    search: DocSearchResult | null,
    pdfJail: boolean | null = null,
    spotlight: boolean | null = null,
    reindexNeeded = false,
    storeEmbedder: string | null = null,
  ) =>
    renderToStaticMarkup(
      createElement(DocSearchPanel, {
        index,
        search,
        pdfJail,
        spotlight,
        reindexNeeded,
        storeEmbedder,
      }),
    );

  it("shows the green ARMED guard pill when the daemon reports the pdf jail present", () => {
    const html = render(parseDocIndexStatus(indexedNeural), null, true);
    expect(html).toContain("PDF JAIL ARMED");
    expect(html).not.toContain("PDF JAIL MISSING");
    expect(html).toContain("docsearch-pill jailed");
  });

  it("shows the amber MISSING guard pill when the daemon is on the in-process fallback", () => {
    const html = render(parseDocIndexStatus(indexedNeural), null, false);
    expect(html).toContain("PDF JAIL MISSING");
    expect(html).not.toContain("PDF JAIL ARMED");
    expect(html).toContain("docsearch-pill unjailed");
  });

  it("claims nothing about the guard before a status frame arrives (older daemons)", () => {
    const html = render(parseDocIndexStatus(indexedNeural), null, null);
    expect(html).not.toContain("PDF JAIL");
  });

  it("the guard pill rides the indexed head only — the NOT-INDEXED state stays as-is", () => {
    // Before any index exists no extraction has run, so the guard has nothing to
    // qualify; the honest empty state is unchanged even when the helper is absent.
    const html = render(parseDocIndexStatus({ files: 0, chunks: 0, embedded_chunks: 0 }), null, false);
    expect(html).toMatch(/NOT INDEXED/i);
    expect(html).not.toContain("PDF JAIL");
  });

  it("shows the green SPOTLIGHT ON pill when the daemon reports the bridge answering", () => {
    const html = render(parseDocIndexStatus(indexedNeural), null, null, true);
    expect(html).toContain("SPOTLIGHT ON");
    expect(html).not.toContain("SPOTLIGHT IDLE");
    expect(html).toContain("docsearch-pill spotlight-on");
  });

  it("shows the dim SPOTLIGHT IDLE pill when the bridge has not answered (honest false)", () => {
    // False covers "no search has queried Spotlight yet" AND "mdfind absent /
    // Spotlight indexing disabled" — idle is normal, so the pill is dim, not amber.
    const html = render(parseDocIndexStatus(indexedNeural), null, null, false);
    expect(html).toContain("SPOTLIGHT IDLE");
    expect(html).not.toContain("SPOTLIGHT ON");
    expect(html).toContain("docsearch-pill spotlight-idle");
  });

  it("claims nothing about spotlight before a status frame arrives (older daemons)", () => {
    const html = render(parseDocIndexStatus(indexedNeural), null, null, null);
    expect(html).not.toContain("SPOTLIGHT");
  });

  it("shows the amber OLD-EMBEDDER pill + reindex note when the daemon reports a space mismatch", () => {
    const html = render(
      parseDocIndexStatus(indexedNeural),
      null,
      null,
      null,
      true,
      "llm-meanpool:qwen3-4b",
    );
    expect(html).toContain("INDEX: OLD EMBEDDER");
    expect(html).toContain("docsearch-pill stale-space");
    // The tooltip + note name the stamp, the honest degradation (BM25, never a
    // cross-space cosine), and the exact spoken recovery command.
    expect(html).toContain("llm-meanpool:qwen3-4b");
    expect(html).toMatch(/keyword \(BM25\) ranking/);
    expect(html).toContain("index my documents");
  });

  it("the OLD-EMBEDDER pill renders without a stamp too (the mismatch alone is the warning)", () => {
    const html = render(parseDocIndexStatus(indexedNeural), null, null, null, true, null);
    expect(html).toContain("INDEX: OLD EMBEDDER");
    expect(html).not.toContain("(null)");
  });

  it("shows the OLD-EMBEDDER pill for a PRE-STAMP index (unknown-space sentinel stamp)", () => {
    // A pre-stamp index of unverifiable origin: the daemon stamps it with the
    // reserved sentinel and reports reindex_needed. The panel treats the stamp
    // as an opaque string — the pill fires and the sentinel shows verbatim, and
    // the copy honestly says "different or unverifiable" embedder.
    const html = render(
      parseDocIndexStatus(indexedNeural),
      null,
      null,
      null,
      true,
      "unknown-pre-tag",
    );
    expect(html).toContain("INDEX: OLD EMBEDDER");
    expect(html).toContain("unknown-pre-tag");
    expect(html).toMatch(/different or unverifiable on-device embedder/);
  });

  it("raises no OLD-EMBEDDER warning when the daemon reports none (older daemons / matching space)", () => {
    const html = render(parseDocIndexStatus(indexedNeural), null, null, null, false);
    expect(html).not.toContain("OLD EMBEDDER");
    expect(html).not.toContain("stale-space");
  });

  it("renders nothing before any index or search", () => {
    expect(render(null, null)).toBe("");
  });

  it("shows the honest NOT-INDEXED state with the enable + allowlist steps", () => {
    // An index event with zero chunks reads as "not indexed yet", never a fake.
    const html = render(parseDocIndexStatus({ files: 0, chunks: 0, embedded_chunks: 0 }), null);
    expect(html).toMatch(/NOT INDEXED/i);
    expect(html).toContain("[docsearch].enabled");
    expect(html).toContain("[docsearch].roots");
    expect(html).toContain("REVIEW ONLY");
  });

  it("shows counts and the NEURAL pill for a fully-embedded index", () => {
    const html = render(parseDocIndexStatus(indexedNeural), null);
    expect(html).toContain("12"); // files
    expect(html).toContain("240"); // chunks
    expect(html).toContain("NEURAL");
    expect(html).not.toContain("BM25 FALLBACK");
  });

  it("shows the BM25-FALLBACK pill + honest note when not all chunks are embedded", () => {
    const html = render(parseDocIndexStatus(indexedPartial), null);
    expect(html).toContain("BM25 FALLBACK");
    // The honest "N of M chunks embedded, rest fall back to BM25" note.
    expect(html).toMatch(/0 of 240 chunks are embedded/);
  });

  it("renders cited hits: real file path, offset, score, and snippet", () => {
    const html = render(null, parseDocSearchResult(searchedNeural));
    expect(html).toContain("/Users/me/notes/lease.md");
    expect(html).toContain("@1840");
    expect(html).toContain("0.910"); // score, fixed(3)
    expect(html).toContain("The renewal clause auto-extends the term");
    // The query and the method that actually ran are reported.
    expect(html).toContain("renewal clause");
    expect(html).toContain("NEURAL (on-device embeddings)");
  });

  it("reports the BM25 method honestly when the search fell back", () => {
    const html = render(
      null,
      parseDocSearchResult({ ...searchedNeural, method: "lexical-bm25" }),
    );
    expect(html).toContain("LEXICAL (BM25 keyword)");
    expect(html).not.toContain("NEURAL (on-device embeddings)");
  });

  it("shows the honest NOTHING-FOUND state for an empty result (never a fake hit)", () => {
    const html = render(
      null,
      parseDocSearchResult({ query: "no match", method: "neural-embedding", hits: [] }),
    );
    expect(html).toMatch(/Nothing matched/i);
    expect(html).toContain("honest result");
  });

  it("has NO action button — indexing/searching/forgetting are spoken", () => {
    const html = render(parseDocIndexStatus(indexedNeural), parseDocSearchResult(searchedNeural));
    expect(html).not.toContain("<button");
    // The spoken commands are surfaced in the footer.
    expect(html).toContain("index my documents");
    expect(html).toContain("forget my file index");
  });

  it("states the on-device / private / honest-extraction contract in the footer", () => {
    const html = render(parseDocIndexStatus(indexedNeural), null);
    expect(html).toMatch(/on-device/i);
    expect(html).toMatch(/never leave|never scans|never silently/i);
    // The extractor claim is honest about what IS handled (text, born-digital
    // PDFs, Office docs) and what is skipped (scanned/encrypted/corrupt files) —
    // the old "PDFs are skipped" copy predates the docsearch extractors.
    expect(html).toMatch(/born-digital PDFs and Office docs are extracted/i);
    expect(html).toMatch(/skipped honestly/i);
  });
});

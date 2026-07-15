import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import KnowledgeGraphPanel from "../components/KnowledgeGraphPanel";
import {
  KG_ENTITY_TYPES,
  kgEntityTypeLabel,
  parseKnowledgeGraphResult,
  type KnowledgeGraphResult,
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

/** A realistic `knowledge_graph.built` payload mirroring the daemon's
 *  router.rs::handle_build_knowledge_graph emission: the build stats, the honest
 *  extractor method token, and a bounded SHARED world-model snapshot — entities
 *  (each provenance-tagged with a real source file:offset(+char span)) grouped by
 *  type, and relationships carrying the source detail on the co-occurrence edge. */
const builtRich: Record<string, unknown> = {
  chunks_scanned: 42,
  entities_written: 4,
  relationships_written: 2,
  skipped_at_cap: 0,
  extractor: "deterministic-heuristic",
  graph: {
    entities: [
      {
        type: "project",
        id: "project_darwin",
        name: "Project DARWIN",
        source: "/Users/me/notes/darwin.md:0 (chars 12-25)",
      },
      {
        type: "person",
        id: "darwin_capani",
        name: "Darwin Capani",
        source: "/Users/me/notes/darwin.md:0 (chars 40-53)",
      },
      {
        type: "deadline",
        id: "2026_06_30",
        name: "2026-06-30",
        source: "/Users/me/notes/roadmap.md:512 (chars 4-14)",
      },
      {
        type: "topic",
        id: "knowledge_graph",
        name: "Knowledge Graph",
        source: "/Users/me/notes/darwin.md:1024 (chars 8-23)",
      },
    ],
    relationships: [
      {
        from: "darwin_capani",
        relation: "mentions",
        to: "project_darwin",
        source: "source /Users/me/notes/darwin.md:0",
      },
      {
        from: "project_darwin",
        relation: "mentions",
        to: "knowledge_graph",
        source: "source /Users/me/notes/darwin.md:1024",
      },
    ],
  },
};

/** A build that extracted NOTHING from entity-less prose: stats present, graph
 *  empty. The honest "extracted nothing" — never fabricated. */
const builtEmpty: Record<string, unknown> = {
  chunks_scanned: 7,
  entities_written: 0,
  relationships_written: 0,
  skipped_at_cap: 0,
  extractor: "deterministic-heuristic",
  graph: { entities: [], relationships: [] },
};

/* ------------------------------------------------------------------------ *
 * The canonical kind order + labels.                                         *
 * ------------------------------------------------------------------------ */
describe("kg entity-type order + labels", () => {
  it("groups in the world model's canonical kind order", () => {
    expect(KG_ENTITY_TYPES).toEqual([
      "project",
      "person",
      "deadline",
      "task",
      "topic",
      "thread",
    ]);
  });

  it("labels every known kind, falling back to UPPER for an unknown one", () => {
    expect(kgEntityTypeLabel("project")).toBe("Projects");
    expect(kgEntityTypeLabel("person")).toBe("People");
    expect(kgEntityTypeLabel("deadline")).toBe("Deadlines");
    expect(kgEntityTypeLabel("task")).toBe("Tasks");
    expect(kgEntityTypeLabel("topic")).toBe("Topics");
    expect(kgEntityTypeLabel("thread")).toBe("Threads");
    expect(kgEntityTypeLabel("organization")).toBe("ORGANIZATION");
    expect(kgEntityTypeLabel("")).toBe("OTHER");
  });
});

/* ------------------------------------------------------------------------ *
 * parseKnowledgeGraphResult — only REAL grounded nodes/edges, honest stats,  *
 * never fabricates, never null, never throws.                                *
 * ------------------------------------------------------------------------ */
describe("parseKnowledgeGraphResult (defensive, grounded-only)", () => {
  it("parses the stats, method, and provenance-tagged graph", () => {
    const r = parseKnowledgeGraphResult(builtRich);
    expect(r.chunksScanned).toBe(42);
    expect(r.entitiesWritten).toBe(4);
    expect(r.relationshipsWritten).toBe(2);
    expect(r.skippedAtCap).toBe(0);
    expect(r.extractor).toBe("deterministic-heuristic");
    expect(r.entities.length).toBe(4);
    expect(r.relationships.length).toBe(2);
    // Provenance is carried verbatim from the daemon.
    const proj = r.entities.find((e) => e.id === "project_darwin");
    expect(proj?.source).toBe("/Users/me/notes/darwin.md:0 (chars 12-25)");
    expect(r.relationships[0].source).toBe("source /Users/me/notes/darwin.md:0");
  });

  it("an entity-less build is the honest empty graph (never fabricates)", () => {
    const r = parseKnowledgeGraphResult(builtEmpty);
    expect(r.entities).toEqual([]);
    expect(r.relationships).toEqual([]);
    expect(r.chunksScanned).toBe(7);
  });

  it("drops an entity with no id (cannot be cited or grouped)", () => {
    const r = parseKnowledgeGraphResult({
      graph: {
        entities: [
          { type: "person", id: "", name: "Nameless", source: "f:0" },
          { type: "person", id: "ok", name: "Ok", source: "f:1" },
        ],
        relationships: [],
      },
    });
    expect(r.entities.length).toBe(1);
    expect(r.entities[0].id).toBe("ok");
  });

  it("drops a relationship missing an endpoint (not a real edge)", () => {
    const r = parseKnowledgeGraphResult({
      graph: {
        entities: [],
        relationships: [
          { from: "a", relation: "mentions", to: "", source: "f:0" },
          { from: "", relation: "mentions", to: "b", source: "f:0" },
          { from: "a", relation: "mentions", to: "b", source: "f:0" },
        ],
      },
    });
    expect(r.relationships.length).toBe(1);
    expect(r.relationships[0].from).toBe("a");
    expect(r.relationships[0].to).toBe("b");
  });

  it("an entity with no source keeps a null citation (honest, not faked)", () => {
    const r = parseKnowledgeGraphResult({
      graph: { entities: [{ type: "topic", id: "x", name: "X" }], relationships: [] },
    });
    expect(r.entities[0].source).toBeNull();
  });

  it("defaults the extractor token to the conservative heuristic when absent", () => {
    const r = parseKnowledgeGraphResult({ chunks_scanned: 1 });
    expect(r.extractor).toBe("deterministic-heuristic");
  });

  it("never returns null and never throws on junk", () => {
    expect(() =>
      parseKnowledgeGraphResult({ graph: "nope", chunks_scanned: "x" }),
    ).not.toThrow();
    const r = parseKnowledgeGraphResult({ graph: "nope", chunks_scanned: "x" });
    expect(r).not.toBeNull();
    expect(r.entities).toEqual([]);
    expect(r.chunksScanned).toBe(0);
  });
});

/* ------------------------------------------------------------------------ *
 * The reducer arm. knowledge_graph.built sets the result; NEVER null after a *
 * frame; latest-wins.                                                        *
 * ------------------------------------------------------------------------ */
describe("knowledge_graph.built reducer", () => {
  it("starts with no knowledge graph", () => {
    expect(connected().knowledgeGraph).toBeNull();
  });

  it("sets the result from knowledge_graph.built", () => {
    const s = tel(connected(), env("knowledge_graph.built", builtRich));
    expect(s.knowledgeGraph).not.toBeNull();
    expect(s.knowledgeGraph!.entities.length).toBe(4);
    expect(s.knowledgeGraph!.relationships.length).toBe(2);
    expect(s.knowledgeGraph!.chunksScanned).toBe(42);
  });

  it("a later build replaces the prior one (latest-wins)", () => {
    let s = tel(connected(), env("knowledge_graph.built", builtRich));
    s = tel(s, env("knowledge_graph.built", builtEmpty));
    expect(s.knowledgeGraph!.entities).toEqual([]);
    expect(s.knowledgeGraph!.chunksScanned).toBe(7);
  });
});

/* ------------------------------------------------------------------------ *
 * The panel (rendered headlessly). PRIVATE + REVIEW-ONLY: entities grouped   *
 * BY TYPE, each with its source provenance, relationships as edges, honest    *
 * heuristic copy, no action button, honest empty state.                      *
 * ------------------------------------------------------------------------ */
describe("KnowledgeGraphPanel (grouped, provenance-tagged, honest, review-only)", () => {
  const render = (graph: KnowledgeGraphResult | null) =>
    renderToStaticMarkup(createElement(KnowledgeGraphPanel, { graph }));

  it("renders nothing before any build", () => {
    expect(render(null)).toBe("");
  });

  it("groups entities BY TYPE with the canonical labels", () => {
    const html = render(parseKnowledgeGraphResult(builtRich));
    expect(html).toContain("Projects");
    expect(html).toContain("People");
    expect(html).toContain("Deadlines");
    expect(html).toContain("Topics");
    expect(html).toContain("REVIEW ONLY");
  });

  it("renders each entity's name + its REAL source provenance citation", () => {
    const html = render(parseKnowledgeGraphResult(builtRich));
    expect(html).toContain("Project DARWIN");
    expect(html).toContain("/Users/me/notes/darwin.md:0 (chars 12-25)");
    expect(html).toContain("Darwin Capani");
    expect(html).toContain("2026-06-30");
  });

  it("renders relationships as from-relation-to edges with their source", () => {
    const html = render(parseKnowledgeGraphResult(builtRich));
    expect(html).toContain("darwin_capani");
    expect(html).toContain("mentions");
    expect(html).toContain("project_darwin");
    expect(html).toContain("source /Users/me/notes/darwin.md:0");
  });

  it("surfaces the build stats + the honest extractor method token", () => {
    const html = render(parseKnowledgeGraphResult(builtRich));
    expect(html).toContain("deterministic-heuristic");
    expect(html).toContain("CHUNKS SCANNED");
    expect(html).toContain("42");
  });

  it("carries the honest copy: heuristic, conservative, text-grounded, bounded, off", () => {
    const html = render(parseKnowledgeGraphResult(builtRich));
    expect(html).toContain("heuristic");
    expect(html).toContain("source file");
    expect(html.toLowerCase()).toContain("bounded");
    expect(html).toContain("map my documents");
  });

  it("has NO action button (building is a spoken intent)", () => {
    const html = render(parseKnowledgeGraphResult(builtRich));
    expect(html).not.toContain("<button");
  });

  it("shows the honest empty state for an entity-less build (never faked)", () => {
    const html = render(parseKnowledgeGraphResult(builtEmpty));
    expect(html).toContain("No entities were extracted");
    expect(html).not.toContain("<button");
  });

  it("surfaces the at-cap skip count as the honest bound proof", () => {
    const html = render(
      parseKnowledgeGraphResult({ ...builtRich, skipped_at_cap: 3 }),
    );
    expect(html).toContain("SKIPPED (AT CAP)");
    expect(html).toContain("at its bound");
  });

  it("renders an unknown future kind under its own group rather than dropping it", () => {
    const html = render(
      parseKnowledgeGraphResult({
        graph: {
          entities: [{ type: "organization", id: "acme", name: "Acme", source: "f:0" }],
          relationships: [],
        },
      }),
    );
    expect(html).toContain("ORGANIZATION");
    expect(html).toContain("Acme");
  });

  it("shows 'no citation' honestly for an entity with no source", () => {
    const html = render(
      parseKnowledgeGraphResult({
        graph: { entities: [{ type: "topic", id: "x", name: "X" }], relationships: [] },
      }),
    );
    expect(html).toContain("no citation");
  });
});

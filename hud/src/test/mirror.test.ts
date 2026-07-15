import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import MirrorPanel, { facetLabel } from "../components/MirrorPanel";
import {
  parseMirrorFrame,
  MIRROR_BELIEFS_CAP,
  type MirrorBelief,
  type MirrorFrame,
  type TelemetryEnvelope,
} from "../core/events";
import { initialState, reduce, type HudState } from "../core/state";

let counter = 0;
function env(event: string, data: Record<string, unknown>, source = "system"): TelemetryEnvelope {
  counter += 1;
  return { ts: `2026-07-15T00:00:${String(counter % 60).padStart(2, "0")}Z`, source, event, data };
}
function connected() {
  return reduce(initialState(), { type: "ws.connected", at: 0 });
}
function tel(state: HudState, e: TelemetryEnvelope) {
  return reduce(state, { type: "telemetry", envelope: e, at: 1000 });
}

/** Mirrors daemon/src/user_model.rs::mirror_frame_json's wire shape (strongest-first). */
const wire = {
  action: "snapshot",
  subject: "",
  found: false,
  beliefs: [
    {
      key: "user.model.topic.rust",
      facet: "topic",
      subject: "rust",
      observation: "keeps coming back to rust",
      observed_count: 5,
      provenance: ["ep:1", "ep:2"],
    },
    {
      key: "user.model.preference.editor",
      facet: "preference",
      subject: "editor",
      observation: "editor = neovim",
      observed_count: 2,
      provenance: ["fact:user.preference.editor"],
    },
  ],
  suppressed: ["style_tone"],
};

/** The explain-turn frame: same belief list, but action context + found. */
const explainWire = { ...wire, action: "explain", subject: "rust", found: true };

describe("parseMirrorFrame (never fabricates a belief)", () => {
  it("parses the daemon's wire shape", () => {
    const f = parseMirrorFrame(wire);
    expect(f).not.toBeNull();
    expect(f?.action).toBe("snapshot");
    expect(f?.beliefs).toHaveLength(2);
    expect(f?.beliefs[0]).toEqual({
      key: "user.model.topic.rust",
      facet: "topic",
      subject: "rust",
      observation: "keeps coming back to rust",
      observedCount: 5,
      provenance: ["ep:1", "ep:2"],
    });
    expect(f?.suppressed).toEqual(["style_tone"]);
  });

  it("parses an explain-turn frame (action + subject + found)", () => {
    const f = parseMirrorFrame(explainWire);
    expect(f?.action).toBe("explain");
    expect(f?.subject).toBe("rust");
    expect(f?.found).toBe(true);
  });

  it("drops a frame with no action", () => {
    expect(parseMirrorFrame({})).toBeNull();
    expect(parseMirrorFrame({ beliefs: [] })).toBeNull();
  });

  it("caps beliefs, bounds strings, and drops malformed rows", () => {
    const bloated = {
      action: "snapshot",
      beliefs: Array.from({ length: 200 }, (_, i) => ({
        key: `user.model.topic.t${i}`,
        facet: "topic",
        subject: `t${i}`,
        observation: "z".repeat(5000),
        observed_count: i,
        provenance: Array.from({ length: 50 }, (_, j) => `ep:${j}`),
      })),
    };
    const f = parseMirrorFrame(bloated);
    expect(f?.beliefs).toHaveLength(MIRROR_BELIEFS_CAP);
    for (const b of f?.beliefs ?? []) {
      expect(b.observation.length).toBeLessThanOrEqual(240);
      expect(b.provenance.length).toBeLessThanOrEqual(8);
    }
    // A belief without a key or subject is meaningless and dropped, not fatal.
    const partial = parseMirrorFrame({
      action: "snapshot",
      beliefs: [
        { facet: "topic", subject: "no-key" },
        "junk",
        { key: "user.model.topic.rust", subject: "rust", observation: "ok" },
      ],
    });
    expect(partial?.beliefs).toHaveLength(1);
    expect(partial?.beliefs[0].subject).toBe("rust");
  });
});

describe("mirror.belief reducer", () => {
  it("is null until the first frame, then replaces wholesale", () => {
    let s = connected();
    expect(s.mirror).toBeNull();
    s = tel(s, env("mirror.belief", wire));
    expect(s.mirror?.beliefs).toHaveLength(2);
    // A later explain frame replaces it wholesale (new action context).
    s = tel(s, env("mirror.belief", explainWire));
    expect(s.mirror?.action).toBe("explain");
    expect(s.mirror?.found).toBe(true);
  });

  it("drops a malformed frame (same reference, prior list kept)", () => {
    let s = connected();
    s = tel(s, env("mirror.belief", wire));
    const before = s.mirror;
    s = tel(s, env("mirror.belief", { junk: true }));
    expect(s.mirror).toBe(before);
  });
});

describe("MirrorPanel", () => {
  const render = (mirror: MirrorFrame | null, onContest: (b: MirrorBelief) => void = () => {}) =>
    renderToStaticMarkup(createElement(MirrorPanel, { mirror, onContest }));

  it("renders nothing before the first frame", () => {
    expect(render(null)).toBe("");
  });

  it("shows each belief with its observation, evidence chip, and contest control", () => {
    const html = render(parseMirrorFrame(wire) as MirrorFrame);
    expect(html).toContain("MIRROR // SELF-MODEL");
    expect(html).toContain("keeps coming back to rust");
    expect(html).toContain("Recurring topic"); // facet label for "topic"
    expect(html).toContain("observed 5"); // the evidence chip's observed-count
    expect(html).toContain("2 sources"); // provenance source count
    expect(html).toContain("that"); // the "that's wrong" contest control
    expect(html.toLowerCase()).toContain("wrong");
    expect(html).toContain("1 contested"); // the suppressed count surfaced
    expect(html).toContain("Review only");
  });

  it("shows the honest-empty state when there are no beliefs", () => {
    const empty = parseMirrorFrame({ action: "snapshot", beliefs: [], suppressed: [] }) as MirrorFrame;
    const html = render(empty);
    expect(html).toContain("have not built up an observed picture");
    expect(html).not.toContain("that’s wrong");
  });

  it("maps facet tokens to labels and shows unknown tokens verbatim", () => {
    expect(facetLabel("preference")).toBe("Preference");
    expect(facetLabel("pattern")).toBe("Pattern");
    expect(facetLabel("topic")).toBe("Recurring topic");
    expect(facetLabel("style")).toBe("Communication style");
    expect(facetLabel("mystery")).toBe("mystery");
  });
});

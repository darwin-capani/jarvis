import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import CaptionBand from "../components/CaptionBand";
import {
  parseCaptionLine,
  CAPTION_UNKNOWN_SPEAKER,
  type TelemetryEnvelope,
} from "../core/events";
import {
  CAPTIONS_CAP,
  HudState,
  initialState,
  reduce,
  type CaptionEntry,
} from "../core/state";

/* helpers ------------------------------------------------------------------ */

let counter = 0;
function env(
  event: string,
  data: Record<string, unknown> = {},
  source = "local",
): TelemetryEnvelope {
  counter += 1;
  return {
    ts: `2026-07-15T12:00:${String(counter % 60).padStart(2, "0")}Z`,
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

/** Render the band to static markup (node env, no DOM) — mirrors chart.test.ts. */
function render(captions: CaptionEntry[]): string {
  return renderToStaticMarkup(createElement(CaptionBand, { captions }));
}

function row(over: Partial<CaptionEntry> = {}): CaptionEntry {
  return { text: "hello", speaker: "speaker_0", translation: null, ts: 0, seq: 1, ...over };
}

/* PARSER — parseCaptionLine (wire -> CaptionLine) --------------------------- */

describe("parseCaptionLine", () => {
  it("parses a full caption line verbatim", () => {
    const line = parseCaptionLine({
      text: "  good morning  ",
      speaker: "speaker_1",
      translation: "  buenos días  ",
      ts: 1720000000000,
    });
    expect(line).toEqual({
      text: "good morning",
      speaker: "speaker_1",
      translation: "buenos días",
      ts: 1720000000000,
    });
  });

  it("drops a frame with no usable text (never an empty caption)", () => {
    expect(parseCaptionLine({ text: "", speaker: "speaker_0" })).toBeNull();
    expect(parseCaptionLine({ text: "   ", speaker: "speaker_0" })).toBeNull();
    expect(parseCaptionLine({ speaker: "speaker_0" })).toBeNull();
  });

  it("reads a missing/blank speaker honestly as 'unknown', never fabricated", () => {
    expect(parseCaptionLine({ text: "hi" })?.speaker).toBe(CAPTION_UNKNOWN_SPEAKER);
    expect(parseCaptionLine({ text: "hi", speaker: "   " })?.speaker).toBe(
      CAPTION_UNKNOWN_SPEAKER,
    );
    // A real reported label is carried verbatim.
    expect(parseCaptionLine({ text: "hi", speaker: "speaker_2" })?.speaker).toBe("speaker_2");
  });

  it("carries translation only when non-blank (passthrough => null)", () => {
    expect(parseCaptionLine({ text: "hi", translation: "hola" })?.translation).toBe("hola");
    // Absent / blank / non-string translation => null (honest passthrough).
    expect(parseCaptionLine({ text: "hi" })?.translation).toBeNull();
    expect(parseCaptionLine({ text: "hi", translation: "   " })?.translation).toBeNull();
    expect(parseCaptionLine({ text: "hi", translation: null })?.translation).toBeNull();
    expect(parseCaptionLine({ text: "hi", translation: 42 })?.translation).toBeNull();
  });

  it("defaults a missing/garbled ts to 0 (never throws)", () => {
    expect(parseCaptionLine({ text: "hi" })?.ts).toBe(0);
    expect(parseCaptionLine({ text: "hi", ts: "nope" as unknown })?.ts).toBe(0);
  });
});

/* REDUCER — captions.line case --------------------------------------------- */

describe("captions.line reducer", () => {
  it("appends a valid caption row to the band (newest last, stamped seq)", () => {
    let s = connected();
    s = tel(s, env("captions.line", { text: "hello", speaker: "speaker_0", ts: 1 }));
    s = tel(s, env("captions.line", { text: "hi there", speaker: "speaker_1", ts: 2 }));
    expect(s.captions.map((c) => c.text)).toEqual(["hello", "hi there"]);
    expect(s.captions.map((c) => c.speaker)).toEqual(["speaker_0", "speaker_1"]);
    // Monotonic seq for a stable render key.
    expect(s.captions[0].seq).toBeLessThan(s.captions[1].seq);
  });

  it("keeps a passthrough row with translation null, never fabricated", () => {
    let s = connected();
    s = tel(s, env("captions.line", { text: "the meeting starts", speaker: "unknown", ts: 5 }));
    expect(s.captions).toHaveLength(1);
    expect(s.captions[0].translation).toBeNull();
    expect(s.captions[0].speaker).toBe(CAPTION_UNKNOWN_SPEAKER);
  });

  it("keeps a translated row when the daemon produced one", () => {
    let s = connected();
    s = tel(
      s,
      env("captions.line", { text: "hello", speaker: "speaker_0", translation: "hola", ts: 6 }),
    );
    expect(s.captions[0].translation).toBe("hola");
  });

  it("drops a malformed/empty frame without churning state (same reference)", () => {
    const s = connected();
    const after = tel(s, env("captions.line", { text: "   ", speaker: "speaker_0" }));
    expect(after).toBe(s); // no usable text => reducer returns the same state
    expect(after.captions).toHaveLength(0);
  });

  it("bounds the ring at CAPTIONS_CAP (oldest evicted)", () => {
    let s = connected();
    for (let i = 0; i < CAPTIONS_CAP + 25; i += 1) {
      s = tel(s, env("captions.line", { text: `line ${i}`, speaker: "unknown", ts: i }));
    }
    expect(s.captions.length).toBe(CAPTIONS_CAP);
    // The oldest rows were evicted; the newest survives at the tail.
    expect(s.captions[s.captions.length - 1].text).toBe(`line ${CAPTIONS_CAP + 24}`);
  });
});

/* BAND RENDER — CaptionBand (renderToStaticMarkup) -------------------------- */

describe("CaptionBand render", () => {
  it("renders nothing when the band is empty (ships OFF => no rows)", () => {
    expect(render([])).toBe("");
  });

  it("renders the speaker label and heard text for a row", () => {
    const html = render([row({ text: "good morning", speaker: "speaker_0", seq: 1 })]);
    expect(html).toContain("SPEAKER_0");
    expect(html).toContain("good morning");
    expect(html).toContain("LIVE CAPTIONS");
  });

  it("shows an unseparated stream honestly as UNKNOWN, never a fabricated speaker", () => {
    const html = render([row({ text: "everyone at once", speaker: "unknown", seq: 1 })]);
    expect(html).toContain("UNKNOWN");
    expect(html).toContain("everyone at once");
  });

  it("renders a translation only when present", () => {
    const withTr = render([row({ text: "hello", translation: "hola", seq: 1 })]);
    expect(withTr).toContain("hola");
    // A passthrough row (translation null) shows only the heard text — no translation node.
    const passthrough = render([row({ text: "hello", translation: null, seq: 1 })]);
    expect(passthrough).toContain("hello");
    expect(passthrough).not.toContain("caption-translation");
  });
});

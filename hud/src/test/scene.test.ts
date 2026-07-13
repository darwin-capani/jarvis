import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import ScenePanel from "../components/ScenePanel";
import { parseSceneStatus, type SceneStatus, type TelemetryEnvelope } from "../core/events";
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

/** Mirrors daemon/src/scene.rs::status_payload. */
const offWire = {
  enabled: false,
  classifier_present: false,
  capture_wired: false,
  dep_verified: false,
  dependency: "a bundled sound-event classifier model + a wired capture tap",
  listening: false,
  retains_audio: false,
  vocabulary: ["doorbell", "knock", "smoke_alarm", "dog_bark"],
  recent_events: [],
  recent_count: 0,
};

describe("parseSceneStatus (pins the honest invariants)", () => {
  it("parses the off state", () => {
    const s = parseSceneStatus(offWire);
    expect(s.enabled).toBe(false);
    expect(s.listening).toBe(false);
    expect(s.retainsAudio).toBe(false);
    expect(s.classifierPresent).toBe(false);
    expect(s.vocabulary).toContain("doorbell");
    expect(s.recentEvents).toEqual([]);
  });

  it("never lets a payload claim retained audio", () => {
    const spoofed = parseSceneStatus({ ...offWire, retains_audio: true, enabled: true, classifier_present: true, capture_wired: true });
    expect(spoofed.retainsAudio).toBe(false); // pinned — a frame can't claim audio is kept
  });

  it("re-derives listening — a model without a wired capture tap is NOT listening", () => {
    // A frame that claims listening:true but has no capture tap must not be believed.
    const spoofed = parseSceneStatus({ ...offWire, enabled: true, classifier_present: true, capture_wired: false, listening: true });
    expect(spoofed.listening).toBe(false); // re-derived, not trusted from the wire
    // All three preconditions -> genuinely listening.
    const live = parseSceneStatus({ ...offWire, enabled: true, classifier_present: true, capture_wired: true });
    expect(live.listening).toBe(true);
  });

  it("clamps event confidence, bounds strings, and caps the arrays", () => {
    const s = parseSceneStatus({
      ...offWire,
      enabled: true,
      recent_events: [
        { label: "doorbell", confidence: 5.0, ts: "2026-07-13T10:00:00Z" }, // clamped to 1
        { label: "", confidence: 0.9, ts: "x" }, // dropped (no label)
        { confidence: 0.9 }, // dropped (no label)
      ],
      vocabulary: Array.from({ length: 100 }, (_, i) => `label_${i}`),
    });
    expect(s.recentEvents.length).toBe(1);
    expect(s.recentEvents[0].confidence).toBe(1);
    expect(s.vocabulary.length).toBeLessThanOrEqual(32);
  });

  it("degrades a malformed frame to the honest off/inert state", () => {
    const d = parseSceneStatus({});
    expect(d.enabled).toBe(false);
    expect(d.listening).toBe(false);
    expect(d.retainsAudio).toBe(false);
    expect(d.vocabulary).toEqual([]);
    expect(d.recentEvents).toEqual([]);
  });
});

describe("scene.status reducer", () => {
  it("is null until the first frame, then set", () => {
    let s = connected();
    expect(s.scene).toBeNull();
    s = tel(s, env("scene.status", { ...offWire, enabled: true }));
    expect(s.scene?.enabled).toBe(true);
    expect(s.scene?.listening).toBe(false);
  });
});

describe("ScenePanel", () => {
  const render = (scene: SceneStatus | null) =>
    renderToStaticMarkup(createElement(ScenePanel, { scene }));

  it("renders nothing before the first frame", () => {
    expect(render(null)).toBe("");
  });

  it("shows OFF, the vocabulary, and the never-retains-audio footnote", () => {
    const html = render(parseSceneStatus(offWire));
    expect(html).toContain("SCENE // ACOUSTIC AWARENESS");
    expect(html).toContain("OFF");
    expect(html).toContain("NEVER RETAINS AUDIO");
    expect(html).toContain("audio is never retained");
    expect(html).toContain("Listens for");
  });

  it("walks OFF -> NEEDS MODEL -> NO CAPTURE -> LISTENING as dependencies come up", () => {
    expect(render(parseSceneStatus({ ...offWire, enabled: true }))).toContain("ARMED · NEEDS MODEL");
    expect(render(parseSceneStatus({ ...offWire, enabled: true, classifier_present: true }))).toContain(
      "ARMED · NO CAPTURE",
    );
    expect(
      render(parseSceneStatus({ ...offWire, enabled: true, classifier_present: true, capture_wired: true })),
    ).toContain("LISTENING");
  });

  it("lists live events with their confidence", () => {
    const html = render(
      parseSceneStatus({
        ...offWire,
        enabled: true,
        classifier_present: true,
        capture_wired: true,
        recent_events: [{ label: "doorbell", confidence: 0.92, ts: "2026-07-13T10:00:00Z" }],
        recent_count: 1,
      }),
    );
    expect(html).toContain("doorbell");
    expect(html).toContain("92%");
  });
});

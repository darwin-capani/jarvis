import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import StatusBar from "../components/StatusBar";
import {
  applyVoiceTier,
  modelTierInitial,
  sttTierInitial,
  voiceIdInitial,
  voiceTierDetail,
  voiceTierInitial,
  voiceTierLabel,
  voiceTierTone,
  voiceModeInitial,
  type TelemetryEnvelope,
  type VoiceTierStatus,
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

/* ----------------------------------------------------------------- folding */

describe("voice-tier folding helpers (events.ts)", () => {
  it("seeds the honest awaiting resting state", () => {
    expect(voiceTierInitial()).toEqual({ backend: null, agent: null });
  });

  it("folds a kokoro (on-device) verdict", () => {
    const v = applyVoiceTier(voiceTierInitial(), {
      backend: "kokoro",
      agent: "darwin",
    });
    expect(v).toEqual({ backend: "kokoro", agent: "darwin" });
  });

  it("folds an elevenlabs (cloud) verdict", () => {
    const v = applyVoiceTier(voiceTierInitial(), {
      backend: "elevenlabs",
      agent: "friday",
    });
    expect(v).toEqual({ backend: "elevenlabs", agent: "friday" });
  });

  it("ignores an unknown backend (keeps the prior value)", () => {
    const seeded: VoiceTierStatus = { backend: "kokoro", agent: "darwin" };
    const v = applyVoiceTier(seeded, { backend: "totally-bogus" });
    expect(v.backend).toBe("kokoro"); // unchanged, not blanked
  });

  it("never reads a key/voice id even if a frame smuggled one in", () => {
    // The contract payload is {backend, agent} only. Prove the reducer reads
    // neither a key nor a voice id field — they have no effect on the surface.
    const v = applyVoiceTier(voiceTierInitial(), {
      backend: "elevenlabs",
      agent: "darwin",
      el_key: "sk-should-be-ignored",
      voice_id: "EL_SECRET_VOICE",
    });
    expect(v).toEqual({ backend: "elevenlabs", agent: "darwin" });
    expect(JSON.stringify(v)).not.toContain("sk-should-be-ignored");
    expect(JSON.stringify(v)).not.toContain("EL_SECRET_VOICE");
  });

  it("labels and describes the tiers honestly", () => {
    expect(voiceTierLabel(null)).toBe("AWAITING");
    expect(voiceTierLabel("kokoro")).toBe("ON-DEVICE");
    expect(voiceTierLabel("elevenlabs")).toBe("CLOUD VOICE");
    // On-device names the private/offline default; cloud names that text leaves
    // the device.
    expect(voiceTierDetail("kokoro").toLowerCase()).toContain("on-device");
    expect(voiceTierDetail("kokoro").toLowerCase()).toContain("fallback");
    expect(voiceTierDetail("elevenlabs").toLowerCase()).toContain("leaves the device");
    // Tone: cloud is an accent (amber), on-device is the calm default (green).
    expect(voiceTierTone("elevenlabs")).toBe("warn");
    expect(voiceTierTone("kokoro")).toBe("good");
    expect(voiceTierTone(null)).toBe("idle");
  });
});

/* ----------------------------------------------------------- state folding */

describe("voice.tier in the HUD reducer", () => {
  it("starts in the seeded awaiting state", () => {
    expect(initialState().voiceTier).toEqual({ backend: null, agent: null });
  });

  it("folds a voice.tier telemetry frame", () => {
    let s = connected();
    s = tel(s, env("voice.tier", { backend: "elevenlabs", agent: "darwin" }));
    expect(s.voiceTier.backend).toBe("elevenlabs");
    expect(s.voiceTier.agent).toBe("darwin");
    // A later on-device reply flips it back honestly.
    s = tel(s, env("voice.tier", { backend: "kokoro", agent: "vision" }));
    expect(s.voiceTier.backend).toBe("kokoro");
    expect(s.voiceTier.agent).toBe("vision");
  });

  it("a garbled frame never blanks the indicator", () => {
    let s = connected();
    s = tel(s, env("voice.tier", { backend: "kokoro", agent: "darwin" }));
    s = tel(s, env("voice.tier", { backend: "noise" }));
    expect(s.voiceTier.backend).toBe("kokoro"); // kept
  });
});

/* -------------------------------------------------------- StatusBar render */

const noop = () => {};

function renderStatusBar(voiceTier: VoiceTierStatus): string {
  return renderToStaticMarkup(
    createElement(StatusBar, {
      connected: true,
      coreState: "idle" as const,
      cloudKeyPresent: true,
      inferenceOffline: false,
      heal: null,
      cloudModel: null,
      activeAgent: null,
      voiceId: voiceIdInitial(),
      modelTier: modelTierInitial(),
      voiceTier,
      sttTier: sttTierInitial(),
      voiceMode: voiceModeInitial(),
      onOpenSettings: noop,
      onOpenDeck: noop,
    }),
  );
}

describe("StatusBar voice-tier chip", () => {
  it("renders AWAITING in the seeded resting state", () => {
    const html = renderStatusBar(voiceTierInitial());
    expect(html).toContain("TTS AWAITING");
  });

  it("renders ON-DEVICE in the calm tone for Kokoro", () => {
    const html = renderStatusBar({ backend: "kokoro", agent: "darwin" });
    expect(html).toContain("TTS ON-DEVICE");
    expect(html).toContain("good");
  });

  it("renders CLOUD VOICE in the amber accent for ElevenLabs, with honest copy", () => {
    const html = renderStatusBar({ backend: "elevenlabs", agent: "friday" });
    expect(html).toContain("TTS CLOUD VOICE");
    expect(html).toContain("warn");
    // The hover copy states the privacy fact: text leaves the device.
    expect(html.toLowerCase()).toContain("leaves the device");
  });

  it("never renders a key/voice id in the chip", () => {
    const html = renderStatusBar({ backend: "elevenlabs", agent: "darwin" });
    expect(html).not.toContain("xi-api-key");
    expect(html).not.toContain("sk-");
  });
});

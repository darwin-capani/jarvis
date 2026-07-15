import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import StatusBar from "../components/StatusBar";
import SettingsModal, {
  VOICE_CLONE_PHRASES,
} from "../components/SettingsModal";
import {
  applySttTier,
  modelTierInitial,
  sttTierDetail,
  sttTierInitial,
  sttTierLabel,
  sttTierTone,
  voiceIdInitial,
  voiceTierInitial,
  voiceModeInitial,
  type SttTierStatus,
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

/* ----------------------------------------------------------------- folding */

describe("stt-tier folding helpers (events.ts)", () => {
  it("seeds the honest awaiting resting state", () => {
    expect(sttTierInitial()).toEqual({ backend: null });
  });

  it("folds a whisper (on-device) verdict", () => {
    const s = applySttTier(sttTierInitial(), { backend: "whisper" });
    expect(s).toEqual({ backend: "whisper" });
  });

  it("folds an elevenlabs_scribe (cloud) verdict", () => {
    const s = applySttTier(sttTierInitial(), { backend: "elevenlabs_scribe" });
    expect(s).toEqual({ backend: "elevenlabs_scribe" });
  });

  it("ignores an unknown backend (keeps the prior value)", () => {
    const seeded: SttTierStatus = { backend: "whisper" };
    const s = applySttTier(seeded, { backend: "totally-bogus" });
    expect(s.backend).toBe("whisper"); // unchanged, not blanked
  });

  it("never reads a key/transcript even if a frame smuggled one in", () => {
    // The contract payload is {backend} only. Prove the reducer reads neither a
    // key nor the transcript text — they have no effect on the surface.
    const s = applySttTier(sttTierInitial(), {
      backend: "elevenlabs_scribe",
      el_key: "sk-should-be-ignored",
      transcript: "MY PRIVATE WORDS",
    });
    expect(s).toEqual({ backend: "elevenlabs_scribe" });
    expect(JSON.stringify(s)).not.toContain("sk-should-be-ignored");
    expect(JSON.stringify(s)).not.toContain("MY PRIVATE WORDS");
  });

  it("labels and describes the tiers honestly", () => {
    expect(sttTierLabel(null)).toBe("AWAITING");
    expect(sttTierLabel("whisper")).toBe("ON-DEVICE STT");
    expect(sttTierLabel("elevenlabs_scribe")).toBe("CLOUD STT");
    // On-device names the private/offline default + fallback; cloud names that the
    // VOICE AUDIO leaves the device (more sensitive than TTS text).
    expect(sttTierDetail("whisper").toLowerCase()).toContain("on-device");
    expect(sttTierDetail("whisper").toLowerCase()).toContain("fallback");
    expect(sttTierDetail("elevenlabs_scribe").toLowerCase()).toContain(
      "voice audio leaves the device",
    );
    // Tone: cloud is an accent (amber), on-device is the calm default (green).
    expect(sttTierTone("elevenlabs_scribe")).toBe("warn");
    expect(sttTierTone("whisper")).toBe("good");
    expect(sttTierTone(null)).toBe("idle");
  });
});

/* ----------------------------------------------------------- state folding */

describe("stt.tier in the HUD reducer", () => {
  it("starts in the seeded awaiting state", () => {
    expect(initialState().sttTier).toEqual({ backend: null });
  });

  it("folds an stt.tier telemetry frame", () => {
    let s = connected();
    s = tel(s, env("stt.tier", { backend: "elevenlabs_scribe" }));
    expect(s.sttTier.backend).toBe("elevenlabs_scribe");
    // A later on-device transcription flips it back honestly.
    s = tel(s, env("stt.tier", { backend: "whisper" }));
    expect(s.sttTier.backend).toBe("whisper");
  });

  it("a garbled frame never blanks the indicator", () => {
    let s = connected();
    s = tel(s, env("stt.tier", { backend: "whisper" }));
    s = tel(s, env("stt.tier", { backend: "noise" }));
    expect(s.sttTier.backend).toBe("whisper"); // kept
  });
});

/* -------------------------------------------------------- StatusBar render */

const noop = () => {};

function renderStatusBar(sttTier: SttTierStatus): string {
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
      voiceTier: voiceTierInitial(),
      sttTier,
      voiceMode: voiceModeInitial(),
      onOpenSettings: noop,
      onOpenDeck: noop,
    }),
  );
}

describe("StatusBar stt-tier chip", () => {
  it("renders AWAITING in the seeded resting state", () => {
    const html = renderStatusBar(sttTierInitial());
    expect(html).toContain("STT AWAITING");
  });

  it("renders ON-DEVICE STT in the calm tone for whisper", () => {
    const html = renderStatusBar({ backend: "whisper" });
    expect(html).toContain("STT ON-DEVICE STT");
    expect(html).toContain("good");
  });

  it("renders CLOUD STT in the amber accent for Scribe, with honest copy", () => {
    const html = renderStatusBar({ backend: "elevenlabs_scribe" });
    expect(html).toContain("STT CLOUD STT");
    expect(html).toContain("warn");
    // The hover copy states the privacy fact: the VOICE AUDIO leaves the device,
    // and that STT is more sensitive than TTS text.
    expect(html.toLowerCase()).toContain("voice audio leaves the device");
    expect(html.toLowerCase()).toContain("more sensitive");
  });

  it("never renders a key/transcript in the chip", () => {
    const html = renderStatusBar({ backend: "elevenlabs_scribe" });
    expect(html).not.toContain("xi-api-key");
    expect(html).not.toContain("sk-");
  });
});

/* ------------------------------------------------- SettingsModal STT section */

function renderSettings(sttTier: SttTierStatus): string {
  return renderToStaticMarkup(
    createElement(SettingsModal, {
      mcp: null,
      voiceId: voiceIdInitial(),
      modelTier: modelTierInitial(),
      sttTier,
      onClose: noop,
    }),
  );
}

describe("SettingsModal cloud-STT section", () => {
  it("shows the CLOUD STT vs ON-DEVICE STT indicator driven by stt.tier", () => {
    const off = renderSettings(sttTierInitial());
    expect(off).toContain("CLOUD STT // TRANSCRIPTION");
    // The on-device whisper verdict reads as ON-DEVICE STT.
    const local = renderSettings({ backend: "whisper" });
    expect(local).toContain("ON-DEVICE STT");
    const cloud = renderSettings({ backend: "elevenlabs_scribe" });
    expect(cloud).toContain("CLOUD STT");
  });

  it("is honest that VOICE AUDIO leaves the device + is more sensitive than TTS text", () => {
    const html = renderSettings(sttTierInitial()).toLowerCase();
    expect(html).toContain("voice audio");
    expect(html).toContain("leaves the device");
    expect(html).toContain("more sensitive than the tts text");
    // On-device whisper is named as the private/offline default + fallback.
    expect(html).toContain("private/offline default");
    expect(html).toContain("fallback");
  });

  it("documents the pinned OFF-by-default cloud_stt key (HUD never writes config)", () => {
    const html = renderSettings(sttTierInitial());
    expect(html).toContain("cloud_stt");
    expect(html).toContain("darwin.toml");
    const lower = html.toLowerCase();
    expect(lower).toContain("ships off");
    expect(lower).toContain("does not write daemon config");
  });
});

/* ----------------------------------------------- SettingsModal clone control */

describe("SettingsModal voice-clone section (consent-gated)", () => {
  it("shows the consent-gated clone control with the propose action first", () => {
    const html = renderSettings(sttTierInitial());
    expect(html).toContain("VOICE CLONE // YOUR OWN VOICE (CONSENT-GATED)");
    // The resting state is the propose step — the upload-confirm is NOT present
    // until the user takes the first explicit step.
    expect(html).toContain("CLONE MY VOICE");
    expect(html).not.toContain("CONFIRM CLONE (UPLOADS SAMPLE)");
    // The resting pill names that the flow is consent-gated.
    expect(html).toContain("CONSENT-GATED");
  });

  it("is honest that the audio SAMPLE leaves the device + is authorization-bound", () => {
    const html = renderSettings(sttTierInitial()).toLowerCase();
    expect(html).toContain("leaves this device");
    expect(html).toContain("authorized to use");
    // No impersonating others; your own voice only.
    expect(html).toContain("no impersonating others");
    // Honest about the no-key path: nothing uploaded, keep on-device voice.
    expect(html).toContain("with no key nothing is uploaded");
    // Two explicit steps; nothing uploaded until you confirm.
    expect(html).toContain("two explicit steps");
    expect(html).toContain("nothing is uploaded until you confirm");
  });

  it("documents the confined in-tree owner sample + the forget control", () => {
    const html = renderSettings(sttTierInitial());
    expect(html).toContain("state/voiceid/");
    expect(html).toContain("state/voice-samples/");
    expect(html).toContain("FORGET CLONE");
    // Authorization-bound: an escaping path is rejected.
    expect(html.toLowerCase()).toContain("escapes the darwin root is rejected");
  });

  it("clone phrases are anchored to the daemon's classify_intent / is_confirmation", () => {
    // PROPOSE must be classified Clone: mentions the voice + "clone", no forget verb.
    expect(VOICE_CLONE_PHRASES.propose.toLowerCase()).toContain("my voice");
    expect(VOICE_CLONE_PHRASES.propose.toLowerCase()).toContain("clone");
    expect(VOICE_CLONE_PHRASES.propose.toLowerCase()).not.toMatch(
      /forget|delete|remove|clear|unclone|erase/,
    );
    // CONFIRM must read as a clear yes (is_confirmation matches on "yes").
    expect(VOICE_CLONE_PHRASES.confirm.toLowerCase()).toContain("yes");
    // FORGET must carry a forget verb + the voice-clone subject (classify -> Forget).
    expect(VOICE_CLONE_PHRASES.forget.toLowerCase()).toContain("forget");
    expect(VOICE_CLONE_PHRASES.forget.toLowerCase()).toContain("voice clone");
  });
});

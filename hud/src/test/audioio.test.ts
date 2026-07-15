import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import AudioIoPanel from "../components/AudioIoPanel";
import {
  audioIoInitial,
  applyInterpretSegmentFed,
  applyInterpretSegment,
  applyTranscriptDiarized,
  applyUtteranceNoWake,
  interpretLabel,
  interpretDirection,
  interpretTone,
  diarizationLabel,
  diarizationTone,
  diarizationDetail,
  type AudioIoStatus,
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
    ts: `2026-06-17T12:00:${String(counter % 60).padStart(2, "0")}Z`,
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

describe("audio-i/o folding helpers (events.ts)", () => {
  it("seeds the honest OFF/neutral resting state (all three features OFF)", () => {
    const s = audioIoInitial();
    expect(s.interpret).toEqual({
      active: false,
      source: null,
      target: null,
      spoke: false,
      translations: 0,
    });
    expect(s.diarization).toEqual({
      seen: false,
      backendCanDiarize: false,
      multiSpeaker: false,
      turns: 0,
    });
    // Default phrase preserves today's behavior; nothing dropped yet.
    expect(s.wake).toEqual({ phrase: "darwin", lastDropped: false });
  });

  /* ---- #30 live interpretation ---- */

  it("#30 segment_fed marks interpret ACTIVE + records direction/voicing (device-gated)", () => {
    const s = applyInterpretSegmentFed(audioIoInitial(), {
      target: "Spanish",
      speak: true,
    });
    expect(s.interpret.active).toBe(true);
    expect(s.interpret.target).toBe("Spanish");
    expect(s.interpret.spoke).toBe(true);
    // No real translation has rendered yet — the count stays honestly 0.
    expect(s.interpret.translations).toBe(0);
  });

  it("#30 segment with translated:true bumps the REAL-translation count", () => {
    let s = applyInterpretSegmentFed(audioIoInitial(), { target: "French", speak: false });
    s = applyInterpretSegment(s, { to: "French", translated: true, spoke: false });
    expect(s.interpret.translations).toBe(1);
    s = applyInterpretSegment(s, { to: "French", translated: true, spoke: false });
    expect(s.interpret.translations).toBe(2);
  });

  it("#30 a frame WITHOUT translated:true never counts as a translation (honest degrade)", () => {
    // A degrade emits no segment at all; but even a malformed frame missing
    // translated:true must NEVER be counted as a real translation.
    let s = applyInterpretSegment(audioIoInitial(), { to: "German" });
    expect(s.interpret.translations).toBe(0);
    s = applyInterpretSegment(s, { to: "German", translated: false });
    expect(s.interpret.translations).toBe(0);
  });

  it("#30 a blank target never blanks the prior direction", () => {
    let s = applyInterpretSegmentFed(audioIoInitial(), { target: "Japanese", speak: false });
    s = applyInterpretSegmentFed(s, { target: "", speak: false });
    expect(s.interpret.target).toBe("Japanese"); // kept, not blanked
  });

  it("#30 never reads/renders a transcript or translation even if a frame smuggled one", () => {
    const s = applyInterpretSegment(audioIoInitial(), {
      to: "Spanish",
      translated: true,
      spoke: false,
      text: "MY PRIVATE WORDS",
      translation: "HOLA SECRETO",
    });
    expect(JSON.stringify(s)).not.toContain("MY PRIVATE WORDS");
    expect(JSON.stringify(s)).not.toContain("HOLA SECRETO");
  });

  it("#30 labels/direction/tone are honest", () => {
    const off = audioIoInitial().interpret;
    expect(interpretLabel(off)).toBe("INTERPRET OFF");
    expect(interpretTone(off)).toBe("idle");
    // auto-detect source when unknown, "—" target when unset.
    expect(interpretDirection(off)).toBe("auto-detect → —");
    const on = applyInterpretSegmentFed(audioIoInitial(), { target: "Spanish", speak: true }).interpret;
    expect(interpretLabel(on)).toBe("LIVE INTERPRET");
    expect(interpretTone(on)).toBe("warn"); // active is an accent worth noticing
    expect(interpretDirection(on)).toBe("auto-detect → Spanish");
  });

  /* ---- #31 diarization (EL-Scribe-only honesty) ---- */

  it("#31 on-device whisper reads as a single honest stream (NO fabricated speaker)", () => {
    const s = applyTranscriptDiarized(audioIoInitial(), {
      transcript: "speaker: unknown\nhello there",
      turns: 1,
      multi_speaker: false,
      backend_can_diarize: false,
    });
    expect(s.diarization.seen).toBe(true);
    expect(s.diarization.backendCanDiarize).toBe(false);
    expect(s.diarization.multiSpeaker).toBe(false);
    expect(diarizationLabel(s.diarization)).toBe("ON-DEVICE: NO DIARIZATION");
    expect(diarizationTone(s.diarization)).toBe("good");
    // Never carries the transcript text.
    expect(JSON.stringify(s)).not.toContain("hello there");
  });

  it("#31 a non-diarizing backend can NEVER over-claim multi-speaker", () => {
    // Even if a hostile frame says multi_speaker:true while backend_can_diarize:false,
    // we never surface a fabricated multi-speaker the backend could not have produced.
    const s = applyTranscriptDiarized(audioIoInitial(), {
      turns: 3,
      multi_speaker: true,
      backend_can_diarize: false,
    });
    expect(s.diarization.multiSpeaker).toBe(false); // pinned honest
    expect(diarizationLabel(s.diarization)).toBe("ON-DEVICE: NO DIARIZATION");
  });

  it("#31 EL Scribe MULTI-SPEAKER surfaces the backend's labels honestly", () => {
    const s = applyTranscriptDiarized(audioIoInitial(), {
      turns: 4,
      multi_speaker: true,
      backend_can_diarize: true,
    });
    expect(s.diarization.backendCanDiarize).toBe(true);
    expect(s.diarization.multiSpeaker).toBe(true);
    expect(s.diarization.turns).toBe(4);
    expect(diarizationLabel(s.diarization)).toBe("MULTI-SPEAKER");
    expect(diarizationTone(s.diarization)).toBe("warn");
  });

  it("#31 EL Scribe single speaker reads as SINGLE STREAM", () => {
    const s = applyTranscriptDiarized(audioIoInitial(), {
      turns: 1,
      multi_speaker: false,
      backend_can_diarize: true,
    });
    expect(diarizationLabel(s.diarization)).toBe("SINGLE STREAM");
    expect(diarizationTone(s.diarization)).toBe("good");
  });

  it("#31 not-seen reads NOT SEEN with the honest EL-Scribe-only copy", () => {
    const d = audioIoInitial().diarization;
    expect(diarizationLabel(d)).toBe("NOT SEEN");
    expect(diarizationTone(d)).toBe("idle");
    expect(diarizationDetail(d).toLowerCase()).toContain("elevenlabs-scribe-only");
    // on-device detail is honest about the single stream + no fabricated speaker.
    const onDevice = applyTranscriptDiarized(audioIoInitial(), {
      turns: 1,
      backend_can_diarize: false,
    }).diarization;
    expect(diarizationDetail(onDevice).toLowerCase()).toContain("single honest stream");
    expect(diarizationDetail(onDevice).toLowerCase()).toContain("never a fabricated speaker");
  });

  /* ---- #32 custom wake word ---- */

  it("#32 utterance.no_wake records the ACTIVE phrase + that the gate dropped one", () => {
    const s = applyUtteranceNoWake(audioIoInitial(), {
      phrase: "computer",
      path: "/tmp/state/audio/utt-123.wav",
    });
    expect(s.wake.phrase).toBe("computer");
    expect(s.wake.lastDropped).toBe(true);
    // The wav path is NEVER carried onto the surface.
    expect(JSON.stringify(s)).not.toContain("utt-123.wav");
  });

  it("#32 a blank phrase never blanks the active wake word", () => {
    let s = applyUtteranceNoWake(audioIoInitial(), { phrase: "athena" });
    s = applyUtteranceNoWake(s, { phrase: "   " });
    expect(s.wake.phrase).toBe("athena"); // kept, not blanked
  });
});

/* ----------------------------------------------------------- state folding */

describe("audio-i/o in the HUD reducer", () => {
  it("starts in the seeded OFF/neutral state", () => {
    const s = initialState().audioIo;
    expect(s.interpret.active).toBe(false);
    expect(s.diarization.seen).toBe(false);
    expect(s.wake.phrase).toBe("darwin");
  });

  it("folds the #30 interpret.segment_fed + interpret.segment events", () => {
    let s = connected();
    s = tel(s, env("interpret.segment_fed", { target: "Spanish", speak: false }, "audio"));
    expect(s.audioIo.interpret.active).toBe(true);
    expect(s.audioIo.interpret.target).toBe("Spanish");
    s = tel(s, env("interpret.segment", { to: "Spanish", translated: true, spoke: false }));
    expect(s.audioIo.interpret.translations).toBe(1);
  });

  it("folds the #31 transcript.diarized event honestly (on-device single stream)", () => {
    let s = connected();
    s = tel(s, env("transcript.diarized", {
      transcript: "speaker: unknown\nhi",
      turns: 1,
      multi_speaker: false,
      backend_can_diarize: false,
    }));
    expect(s.audioIo.diarization.seen).toBe(true);
    expect(s.audioIo.diarization.backendCanDiarize).toBe(false);
    expect(s.audioIo.diarization.multiSpeaker).toBe(false);
  });

  it("folds the #32 utterance.no_wake event (active phrase, dropped flag)", () => {
    let s = connected();
    s = tel(s, env("utterance.no_wake", { phrase: "computer", path: "/x.wav" }, "audio"));
    expect(s.audioIo.wake.phrase).toBe("computer");
    expect(s.audioIo.wake.lastDropped).toBe(true);
  });

  it("a garbled frame never blanks the surface", () => {
    let s = connected();
    s = tel(s, env("interpret.segment_fed", { target: "French", speak: true }, "audio"));
    s = tel(s, env("interpret.segment_fed", { target: "" }, "audio"));
    expect(s.audioIo.interpret.target).toBe("French"); // kept
  });
});

/* -------------------------------------------------------- panel render */

function renderPanel(audio: AudioIoStatus): string {
  return renderToStaticMarkup(createElement(AudioIoPanel, { audio }));
}

describe("AudioIoPanel render", () => {
  it("renders the honest OFF/neutral resting posture", () => {
    const html = renderPanel(audioIoInitial());
    expect(html).toContain("AUDIO // I/O");
    expect(html).toContain("INTERPRET OFF");
    expect(html).toContain("NOT SEEN");
    // The configured wake word is named (default darwin).
    expect(html).toContain("darwin");
    // Honest device-gated + EL-Scribe-only + never-fabricated copy.
    const lower = html.toLowerCase();
    expect(lower).toContain("device-gated");
    expect(lower).toContain("elevenlabs-scribe-only");
    expect(lower).toContain("never a fabricated speaker");
  });

  it("renders the LIVE INTERPRET direction + real-translation count when active", () => {
    let a = applyInterpretSegmentFed(audioIoInitial(), { target: "Spanish", speak: true });
    a = applyInterpretSegment(a, { to: "Spanish", translated: true, spoke: true });
    const html = renderPanel(a);
    expect(html).toContain("LIVE INTERPRET");
    expect(html).toContain("auto-detect → Spanish");
    expect(html).toContain("spoken");
    expect(html).toContain("1 real translation");
  });

  it("renders MULTI-SPEAKER only for an EL-Scribe diarized frame", () => {
    const a = applyTranscriptDiarized(audioIoInitial(), {
      turns: 3,
      multi_speaker: true,
      backend_can_diarize: true,
    });
    const html = renderPanel(a);
    expect(html).toContain("MULTI-SPEAKER");
    expect(html).toContain("3 turns");
  });

  it("renders the on-device single-stream honesty (no fabricated speaker)", () => {
    const a = applyTranscriptDiarized(audioIoInitial(), {
      transcript: "speaker: unknown\nsecret words",
      turns: 1,
      backend_can_diarize: false,
    });
    const html = renderPanel(a);
    expect(html).toContain("ON-DEVICE: NO DIARIZATION");
    expect(html).toContain("single honest stream");
    // The transcript text never reaches the rendered surface.
    expect(html).not.toContain("secret words");
  });

  it("never renders a key/transcript/translation/wav path", () => {
    let a = applyInterpretSegment(audioIoInitial(), {
      to: "Spanish",
      translated: true,
      text: "PRIVATE",
      translation: "SECRETO",
    });
    a = applyUtteranceNoWake(a, { phrase: "darwin", path: "/state/audio/x.wav" });
    const html = renderPanel(a);
    expect(html).not.toContain("PRIVATE");
    expect(html).not.toContain("SECRETO");
    expect(html).not.toContain("x.wav");
    expect(html).not.toContain("sk-");
  });

  /* ---- SFX cue trigger affordance (read-only catalog + honest gate) ---- */

  it("renders the read-only SFX cue catalog with a Play control per cue", () => {
    const html = renderPanel(audioIoInitial());
    expect(html).toContain("SFX CUES");
    // Every built-in cue label is listed (read-only catalog).
    for (const label of ["Confirm", "Alert", "Error", "Success", "Notify", "Wake"]) {
      expect(html).toContain(label);
    }
    // A Play button per cue (six rows) — the static markup has one per cue.
    expect(html.match(/audioio-cue-play/g)?.length).toBe(6);
  });

  it("in the no-shell (node) render the cues are OFF and the buttons are disabled", () => {
    // The server render has no Tauri shell, so the gate is closed: the pill reads
    // OFF and every Play button is disabled — never a fabricated "ready" state.
    const html = renderPanel(audioIoInitial());
    expect(html).toContain(">OFF<");
    // React serializes a disabled button with the `disabled` attribute.
    expect(html).toContain('class="icon-btn audioio-cue-play"');
    expect(html).toContain("disabled");
    // Honest copy: cues require the key + cloud SFX on, else nothing plays.
    const lower = html.toLowerCase();
    expect(lower).toContain("desktop app");
    expect(lower).toContain("else nothing plays");
  });
});

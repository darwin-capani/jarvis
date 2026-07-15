import { describe, expect, it } from "vitest";
import {
  MIN_VOICE_DESCRIPTION_CHARS,
  buildDesignVoiceRequest,
  buildPronunciationRequest,
  classifyVoiceLabReply,
  designVoiceErrorCopy,
  designVoiceOutcomeCopy,
  pronunciationErrorCopy,
  pronunciationOutcomeCopy,
  voiceLabDisabledReason,
  voiceLabGateCopy,
  voiceLabGateOpen,
  type VoiceLabDisabledReason,
  type VoiceLabOutcome,
} from "../core/voiceLab";

/* ------------------------------------------------------------------- gate */

describe("voice-lab gate (mirrors the daemon key + non-Local-tier gate)", () => {
  it("is open ONLY when the cloud tier is on AND a key is present", () => {
    expect(voiceLabGateOpen({ cloudTierOn: true, keyPresent: true })).toBe(true);
    expect(voiceLabGateOpen({ cloudTierOn: true, keyPresent: false })).toBe(false);
    expect(voiceLabGateOpen({ cloudTierOn: false, keyPresent: true })).toBe(false);
    expect(voiceLabGateOpen({ cloudTierOn: false, keyPresent: false })).toBe(false);
  });

  it("voiceLabDisabledReason reports exactly what's missing (no_shell wins)", () => {
    // No shell beats everything — nothing is created without the daemon.
    expect(
      voiceLabDisabledReason(false, { cloudTierOn: true, keyPresent: true }),
    ).toBe("no_shell");
    // In the shell, the precise missing piece is named.
    expect(
      voiceLabDisabledReason(true, { cloudTierOn: false, keyPresent: false }),
    ).toBe("tier_off_and_no_key");
    expect(
      voiceLabDisabledReason(true, { cloudTierOn: false, keyPresent: true }),
    ).toBe("tier_off");
    expect(
      voiceLabDisabledReason(true, { cloudTierOn: true, keyPresent: false }),
    ).toBe("no_key");
    // Both present in the shell → enabled (null reason).
    expect(
      voiceLabDisabledReason(true, { cloudTierOn: true, keyPresent: true }),
    ).toBe(null);
  });

  it("gate copy is honest for every state and never claims a creation", () => {
    const states: (VoiceLabDisabledReason | null)[] = [
      null,
      "no_shell",
      "tier_off",
      "no_key",
      "tier_off_and_no_key",
    ];
    for (const r of states) {
      const copy = voiceLabGateCopy(r);
      expect(copy.trim().length).toBeGreaterThan(0);
      // Never asserts a successful creation in the gate copy.
      expect(copy.toLowerCase()).not.toContain("designed and saved");
      expect(copy.toLowerCase()).not.toContain("added the pronunciation");
    }
    // The enabled copy is honest about the offline no-op (never a fabricated voice).
    expect(voiceLabGateCopy(null).toLowerCase()).toContain("never fabricate");
    // The disabled copies name the concrete fix.
    expect(voiceLabGateCopy("tier_off").toLowerCase()).toContain("cloud_tier");
    expect(voiceLabGateCopy("no_key").toLowerCase()).toContain("elevenlabs");
    expect(voiceLabGateCopy("no_shell").toLowerCase()).toContain("desktop app");
  });
});

/* -------------------------------------------------- design-voice shaping */

describe("buildDesignVoiceRequest (mirrors the daemon decide floors)", () => {
  it("shapes a request for a valid agent + a long-enough description", () => {
    const r = buildDesignVoiceRequest(
      "  friday  ",
      "  a calm, warm British concierge voice  ",
      "",
    );
    expect(r.ok).toBe(true);
    if (r.ok) {
      // Trimmed; the blank name is OMITTED (the daemon defaults it to the agent).
      expect(r.request).toEqual({
        agent: "friday",
        description: "a calm, warm British concierge voice",
      });
      expect(Object.keys(r.request)).toEqual(["agent", "description"]);
    }
  });

  it("carries the display name when one is given", () => {
    const r = buildDesignVoiceRequest(
      "friday",
      "a calm, warm British concierge voice",
      "  Concierge  ",
    );
    expect(r.ok).toBe(true);
    if (r.ok) expect(r.request.name).toBe("Concierge");
  });

  it("rejects an empty agent and a too-short description (the EL floor)", () => {
    const noAgent = buildDesignVoiceRequest("   ", "a calm, warm British concierge voice", "");
    expect(noAgent).toEqual({ ok: false, error: "no_agent" });

    // Exactly one char below the floor is rejected; at the floor it passes.
    const below = "x".repeat(MIN_VOICE_DESCRIPTION_CHARS - 1);
    expect(buildDesignVoiceRequest("friday", below, "")).toEqual({
      ok: false,
      error: "description_too_short",
    });
    const atFloor = "x".repeat(MIN_VOICE_DESCRIPTION_CHARS);
    expect(buildDesignVoiceRequest("friday", atFloor, "").ok).toBe(true);
  });

  it("error copy names the concrete fix", () => {
    expect(designVoiceErrorCopy("no_agent").toLowerCase()).toContain("agent");
    expect(designVoiceErrorCopy("description_too_short")).toContain(
      String(MIN_VOICE_DESCRIPTION_CHARS),
    );
  });
});

/* ------------------------------------------------ pronunciation shaping */

describe("buildPronunciationRequest (mirrors the daemon decide floors)", () => {
  it("shapes a single alias rule for a valid word + say", () => {
    const r = buildPronunciationRequest("  DARWIN  ", "  dar win  ", "");
    expect(r.ok).toBe(true);
    if (r.ok) {
      expect(r.request).toEqual({ word: "DARWIN", say: "dar win" });
      // The blank name is OMITTED (the daemon defaults it to a fixed label).
      expect(Object.keys(r.request)).toEqual(["word", "say"]);
    }
  });

  it("carries the dictionary name when one is given", () => {
    const r = buildPronunciationRequest("DARWIN", "dar win", "My dictionary");
    expect(r.ok).toBe(true);
    if (r.ok) expect(r.request.name).toBe("My dictionary");
  });

  it("rejects an empty word or an empty say", () => {
    expect(buildPronunciationRequest("   ", "dar win", "")).toEqual({
      ok: false,
      error: "no_word",
    });
    expect(buildPronunciationRequest("DARWIN", "   ", "")).toEqual({
      ok: false,
      error: "no_say",
    });
  });

  it("error copy names the concrete fix", () => {
    expect(pronunciationErrorCopy("no_word").toLowerCase()).toContain("word");
    expect(pronunciationErrorCopy("no_say").toLowerCase()).toContain("said");
  });
});

/* ------------------------------------------------- outcome → prose */

describe("classifyVoiceLabReply", () => {
  it("an ok reply is a genuine creation", () => {
    expect(
      classifyVoiceLabReply(true, "Designed and saved the Concierge voice for friday."),
    ).toBe("created");
    expect(
      classifyVoiceLabReply(true, 'Created the pronunciation rule: say "DARWIN" as "dar win".'),
    ).toBe("created");
  });

  it("a gate-closed / offline / no-key Err is an honest unavailable no-op", () => {
    expect(
      classifyVoiceLabReply(
        false,
        "Designing a voice needs the cloud tier, but you're working offline — nothing was created.",
      ),
    ).toBe("unavailable");
    expect(
      classifyVoiceLabReply(
        false,
        "I can't design a voice without an ElevenLabs key — add one in Settings. No voice was created.",
      ),
    ).toBe("unavailable");
    expect(
      classifyVoiceLabReply(
        false,
        "Creating a pronunciation dictionary needs the cloud tier, but you're working offline — nothing was created.",
      ),
    ).toBe("unavailable");
  });

  it("any other Err is a generic failure (never a fabricated success)", () => {
    expect(
      classifyVoiceLabReply(
        false,
        "I couldn't design that voice just now — the cloud request didn't go through. Nothing was created.",
      ),
    ).toBe("failed");
    // A protocol-level relay failure also reads as failed, never created.
    expect(classifyVoiceLabReply(false, "unknown_command")).toBe("failed");
  });
});

describe("outcome copy is honest + secret-free", () => {
  it("design-voice copy maps each outcome and never claims a phantom creation", () => {
    expect(designVoiceOutcomeCopy("created", "friday").toLowerCase()).toContain("saved");
    const unavailable = designVoiceOutcomeCopy("unavailable", "friday").toLowerCase();
    expect(unavailable).toContain("nothing was created");
    const failed = designVoiceOutcomeCopy("failed", "friday").toLowerCase();
    expect(failed).toContain("nothing was created");
    expect(designVoiceOutcomeCopy("no_shell", "friday").toLowerCase()).toContain(
      "desktop app",
    );
  });

  it("pronunciation copy maps each outcome and never claims a phantom creation", () => {
    const created = pronunciationOutcomeCopy("created", "DARWIN", "dar win");
    expect(created).toContain("DARWIN");
    expect(created).toContain("dar win");
    const unavailable = pronunciationOutcomeCopy("unavailable", "DARWIN", "dar win").toLowerCase();
    expect(unavailable).toContain("nothing was created");
    const failed = pronunciationOutcomeCopy("failed", "DARWIN", "dar win").toLowerCase();
    expect(failed).toContain("nothing was created");
    expect(pronunciationOutcomeCopy("no_shell", "DARWIN", "dar win").toLowerCase()).toContain(
      "desktop app",
    );
  });

  it("never leaks a secret-shaped marker and falls back for blank fields", () => {
    const outcomes: VoiceLabOutcome[] = ["created", "unavailable", "failed", "no_shell"];
    for (const o of outcomes) {
      const dv = designVoiceOutcomeCopy(o, "");
      const pr = pronunciationOutcomeCopy(o, "", "");
      for (const copy of [dv, pr]) {
        expect(copy).not.toMatch(/sk-/);
        expect(copy).not.toMatch(/\.wav/);
        expect(copy.trim().length).toBeGreaterThan(0);
      }
    }
  });
});

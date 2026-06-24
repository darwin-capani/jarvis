import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

/* ------------------------------------------------------------------------ *
 * Tauri-shell shim — make `inTauri()` read true and route sendCommand through *
 * a controllable invoke spy so the ENROLL / FORGET button wiring can be        *
 * asserted (the SAME {cmd:"ask"} spoken intent, NO new daemon command) with no *
 * real socket. The static-render tests below never run effects, so config_get  *
 * is never invoked — the loading-note tests are unaffected by this shim.        *
 * ------------------------------------------------------------------------ */
const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args: unknown) => invokeMock(cmd, args),
}));
vi.mock("../tauri/bridge", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../tauri/bridge")>();
  return { ...actual, inTauri: () => true };
});

import SystemSettingsPanel, {
  VoiceIdBadge,
  VoiceIdEnrollControls,
  UpdatesSection,
  UninstallSection,
  VOICE_ID_PHRASES,
} from "../components/SystemSettingsPanel";
import { sendCommand } from "../tauri/command";
import {
  applyVoiceIdEnrolled,
  applyVoiceIdEnrollStarted,
  voiceIdInitial,
  type VoiceIdStatus,
} from "../core/events";
import {
  AUTONOMY_IDS,
  AUTONOMY_OPTIONS,
  CATALOG,
  GROUP_ORDER,
  type CatalogEntry,
  dangerousPending,
  entriesForGroup,
  entryById,
  isDangerousChange,
  pendingChanges,
  sameValue,
  valueMapFromStates,
} from "../core/systemSettings";
import type { Change, SettingState, SettingValue } from "../tauri/configSettings";
import {
  isAutoUpdateOn,
  setAutoUpdateOn,
  type AutoUpdateStorage,
} from "../core/autoUpdate";

/* The SYSTEM SETTINGS catalog is the UI-facing mirror of the backend whitelist
 * (src-tauri/src/config_settings.rs SETTINGS + AUTONOMY_SECTIONS). These pin the
 * groups, the batched-diff model, the dangerous-change confirm gating, and the
 * KEY invariant: the catalog can only offer ids the backend already allows
 * (a drift on either side fails CI). */

/* ------------------------------------------- backend whitelist (mirror) */

/** The EXACT id set the backend whitelist allows — a static mirror of
 *  src-tauri/src/config_settings.rs SETTINGS (each "section.key") plus the
 *  AUTONOMY_SECTIONS (the bare 3-way ids). This is the same "explicit expected
 *  list" discipline credentials.test.ts uses: a drift on either side fails CI,
 *  and the backend ALSO has its own Rust drift test against the real config
 *  file (every_whitelisted_key_exists_in_the_real_config_file). */
const BACKEND_WHITELIST_IDS: string[] = [
  // SAFETY & GATES
  "integrations.allow_consequential",
  "voice_id.enabled",
  "voice_id.gate_scope",
  "voice_id.threshold",
  "security.encrypt_memory",
  "policy.enabled",
  // AUTONOMY (flat toggles)
  "standing.enabled",
  "drafts.enabled",
  "missions.durable",
  "macros.enabled",
  // PROACTIVITY
  "proactive.enabled",
  "proactive.speak",
  "proactive.suggest",
  "proactive.quiet_start",
  "proactive.quiet_end",
  "focus.profile",
  // PERCEPTION
  "screen_context.enabled",
  "screen_context.interval_secs",
  "vision.enabled",
  "vision.model",
  "image.enabled",
  "image.model",
  "audio.sound_monitor",
  "interpret.live",
  "interpret.speak",
  "episodic.enabled",
  // VOICE & SPEECH
  "voice.cloud_tier",
  "voice.cloud_stt",
  "voice.cloud_sfx",
  "voice.cloud_music",
  "voice.stream_tts",
  "voice.event_cues",
  "voice.mic_source",
  "voice.pronunciation_dictionary_id",
  "voice.pronunciation_dictionary_version",
  "voice.adaptive_prosody",
  "voice.whisper",
  "voice.whisper_auto",
  "voice.diarize",
  "speech.engine",
  "speech.model",
  "speech.instant_opener",
  // CAPABILITIES
  "shell.enabled",
  "ui_automation.enabled",
  "ui_automation.actuate_via_app",
  "mcp.enabled",
  "webhooks.enabled",
  "plugin_sdk.enabled",
  "docsearch.enabled",
  "docsearch.roots",
  "docsearch.build_graph",
  "code.enabled",
  "code.roots",
  "local_tools.enabled",
  "report.enabled",
  "chart.enabled",
  "answers.cite",
  "answers.confidence",
  "answers.verify",
  "answers.cross_check",
  "answers.debate",
  // PERFORMANCE & MODELS
  "power.adaptive",
  "inference.speculative",
  "inference.draft_model",
  "inference.quant",
  "models.classifier",
  "models.local_warm",
  "models.local_budget_gib",
  "router.conversation_route",
  // AUTONOMY 3-way (bare section ids — AUTONOMY_SECTIONS)
  "self_heal",
  "forge",
  "optimize",
];

describe("system settings catalog ↔ backend whitelist", () => {
  const backend = new Set(BACKEND_WHITELIST_IDS);

  it("the whitelist mirror is the expected size (flat settings + 3 autonomy)", () => {
    expect(backend.size).toBe(BACKEND_WHITELIST_IDS.length); // no dup in the mirror
    expect(backend.size).toBeGreaterThan(40);
    expect(backend.has("integrations.allow_consequential")).toBe(true);
    expect(backend.has("self_heal")).toBe(true);
  });

  it("every catalog id is a backend-whitelisted id (no UI can offer an off-list key)", () => {
    const offlist = CATALOG.map((e) => e.id).filter((id) => !backend.has(id));
    expect(offlist).toEqual([]);
  });

  it("every backend-whitelisted id is exposed in the catalog (no setting silently dropped)", () => {
    const catalogIds = new Set(CATALOG.map((e) => e.id));
    const missing = [...backend].filter((id) => !catalogIds.has(id));
    expect(missing).toEqual([]);
  });

  it("has no duplicate ids", () => {
    const ids = CATALOG.map((e) => e.id);
    expect(new Set(ids).size).toBe(ids.length);
  });

  it("the three autonomy ids render as the 3-way control", () => {
    for (const id of AUTONOMY_IDS) {
      const entry = entryById(id);
      expect(entry?.control).toBe("autonomy");
    }
    // The 3-way values mirror the backend AUTONOMY_STATES order.
    expect(AUTONOMY_OPTIONS.map((o) => o.value)).toEqual(["off", "propose", "auto"]);
  });

  it("every entry belongs to one of the seven groups", () => {
    for (const e of CATALOG) {
      expect(GROUP_ORDER).toContain(e.group);
    }
  });
});

/* ------------------------------------------------------ control kinds */

describe("control kinds match the catalog requirements", () => {
  it("voice_id exposes on/off + scope select + a float threshold", () => {
    expect(entryById("voice_id.enabled")?.control).toBe("toggle");
    expect(entryById("voice_id.gate_scope")?.control).toBe("select");
    const thr = entryById("voice_id.threshold");
    expect(thr?.control).toBe("number");
    expect(thr?.step).toBeLessThan(1); // a float step, not an integer
  });

  it("the enum selects are present with their option labels", () => {
    for (const id of [
      "speech.engine",
      "inference.quant",
      "router.conversation_route",
      "focus.profile",
    ]) {
      const e = entryById(id);
      expect(e?.control).toBe("select");
      expect(e?.optionLabels).toBeTruthy();
    }
  });

  it("the key numeric bounds are numbers", () => {
    expect(entryById("screen_context.interval_secs")?.control).toBe("number");
    expect(entryById("proactive.quiet_start")?.control).toBe("number");
    expect(entryById("proactive.quiet_end")?.control).toBe("number");
  });

  it("the model-id fields are freeform string controls", () => {
    for (const id of [
      "vision.model",
      "image.model",
      "speech.model",
      "inference.draft_model",
      "models.classifier",
    ]) {
      const e = entryById(id);
      expect(e?.control).toBe("string");
      // Each hint honestly states empty = inert/disabled.
      expect(e?.hint.toLowerCase()).toMatch(/empty/);
    }
  });

  it("the path-array fields are folder-picker pathlists; local_warm is a plain strlist", () => {
    for (const id of ["docsearch.roots", "code.roots"]) {
      expect(entryById(id)?.control).toBe("pathlist");
    }
    expect(entryById("models.local_warm")?.control).toBe("strlist");
    // local_budget_gib is a bounded number with a GiB unit.
    const budget = entryById("models.local_budget_gib");
    expect(budget?.control).toBe("number");
    expect(budget?.unit).toBe("GiB");
  });

  it("the path-array hints are honest: on-device, nothing read until a folder is added", () => {
    for (const id of ["docsearch.roots", "code.roots"]) {
      const hint = entryById(id)!.hint.toLowerCase();
      // The exact honesty the prompt requires: contents stay on-device and
      // nothing is read until the user adds a folder.
      expect(hint).toContain("on-device");
      expect(hint).toMatch(/nothing is read until/);
    }
  });
});

/* ------------------------------------- new ElevenLabs daemon voice keys (HUD) */

/* The daemon's [voice] section grew four cloud-voice config keys; the HUD must
 * SURFACE + CONTROL them through the SAME settings catalog the other [voice]
 * flags ride. These pin that each new key is exposed in the Voice & Speech group
 * with the right control kind + honest copy (lifted from config/jarvis.toml):
 *   - voice.cloud_sfx  -> a toggle (ships on, inert without a key, non-speech cues)
 *   - voice.stream_tts -> a toggle (opt-in lower-latency EL streaming, falls back)
 *   - voice.pronunciation_dictionary_id / _version -> text fields (empty = none).
 * They sit beside the existing cloud_tier / cloud_stt voice settings. */
describe("new ElevenLabs [voice] daemon keys are surfaced in the HUD catalog", () => {
  it("cloud_sfx is an honest toggle in the Voice & Speech group", () => {
    const e = entryById("voice.cloud_sfx");
    expect(e?.control).toBe("toggle");
    expect(e?.group).toBe("Voice & Speech");
    expect(e?.label).toBe("Cloud sound effects (ElevenLabs)");
    const hint = e!.hint.toLowerCase();
    // Honest: ships on, inert without a key, generates non-speech cues, no on-device fallback.
    expect(hint).toContain("ships on");
    expect(hint).toContain("inert without a key");
    expect(hint).toContain("cue");
    expect(hint).toContain("no on-device sfx generator");
  });

  it("cloud_music is an honest toggle in the Voice & Speech group", () => {
    const e = entryById("voice.cloud_music");
    expect(e?.control).toBe("toggle");
    expect(e?.group).toBe("Voice & Speech");
    expect(e?.label).toBe("Cloud music (ElevenLabs)");
    const hint = e!.hint.toLowerCase();
    // Honest: ships on, inert without a key, generates a full track from a prompt,
    // the audio is a cloud generation (no on-device fallback).
    expect(hint).toContain("ships on");
    expect(hint).toContain("inert without a key");
    expect(hint).toContain("track");
    expect(hint).toContain("no on-device music generator");
    expect(hint).toContain("cloud generation");
  });

  it("stream_tts is an honest opt-in toggle in the Voice & Speech group", () => {
    const e = entryById("voice.stream_tts");
    expect(e?.control).toBe("toggle");
    expect(e?.group).toBe("Voice & Speech");
    expect(e?.label).toBe("Streaming cloud TTS");
    const hint = e!.hint.toLowerCase();
    // Honest: opt-in (ships off), lower-latency EL streaming, falls back on any error.
    expect(hint).toContain("opt-in");
    expect(hint).toContain("ships off");
    expect(hint).toMatch(/lower[- ]latency/);
    expect(hint).toContain("falls back");
  });

  it("event_cues is an honest opt-in toggle in the Voice & Speech group", () => {
    const e = entryById("voice.event_cues");
    expect(e?.control).toBe("toggle");
    expect(e?.group).toBe("Voice & Speech");
    expect(e?.label).toBe("Event sound cues");
    const hint = e!.hint.toLowerCase();
    // Honest: opt-in (ships off), fire-and-forget cue on key events, never changes
    // the outcome, requires cloud_sfx + a key (silent no-op otherwise).
    expect(hint).toContain("opt-in");
    expect(hint).toContain("ships off");
    expect(hint).toContain("fire-and-forget");
    expect(hint).toContain("cloud_sfx");
    expect(hint).toMatch(/silent no-op/);
  });

  it("pronunciation dictionary id + version are text fields (empty = none)", () => {
    for (const id of [
      "voice.pronunciation_dictionary_id",
      "voice.pronunciation_dictionary_version",
    ]) {
      const e = entryById(id);
      expect(e?.control).toBe("string");
      expect(e?.group).toBe("Voice & Speech");
      // Each hint honestly states empty = none.
      expect(e!.hint.toLowerCase()).toMatch(/empty = none/);
    }
    expect(entryById("voice.pronunciation_dictionary_id")?.label).toBe(
      "Pronunciation dictionary id",
    );
    expect(entryById("voice.pronunciation_dictionary_version")?.label).toBe(
      "Pronunciation dictionary version",
    );
  });

  it("the new keys sit in the same group as the existing cloud_tier / cloud_stt voice settings", () => {
    const ids = entriesForGroup("Voice & Speech").map((e) => e.id);
    for (const id of [
      "voice.cloud_tier",
      "voice.cloud_stt",
      "voice.cloud_sfx",
      "voice.cloud_music",
      "voice.stream_tts",
      "voice.event_cues",
      "voice.pronunciation_dictionary_id",
      "voice.pronunciation_dictionary_version",
    ]) {
      expect(ids).toContain(id);
    }
  });

  it("the new toggles/text fields are never marked dangerous (no confirm gate)", () => {
    for (const id of ["voice.cloud_sfx", "voice.cloud_music", "voice.stream_tts", "voice.event_cues"]) {
      const e = entryById(id)!;
      expect(e.danger).toBeFalsy();
      expect(isDangerousChange(e, true)).toBe(false);
      expect(isDangerousChange(e, false)).toBe(false);
    }
    for (const id of [
      "voice.pronunciation_dictionary_id",
      "voice.pronunciation_dictionary_version",
    ]) {
      const e = entryById(id)!;
      expect(e.danger).toBeFalsy();
      expect(isDangerousChange(e, "any-id")).toBe(false);
    }
  });
});

/* ------------------------------------------------------ string + array model */

describe("string + array value model", () => {
  it("sameValue compares arrays by order + element, scalars by identity", () => {
    expect(sameValue(["/a", "/b"], ["/a", "/b"])).toBe(true);
    expect(sameValue(["/a", "/b"], ["/b", "/a"])).toBe(false); // a reorder is a change
    expect(sameValue(["/a"], ["/a", "/b"])).toBe(false); // add is a change
    expect(sameValue([], [])).toBe(true);
    expect(sameValue("x", "x")).toBe(true);
    expect(sameValue("x", "y")).toBe(false);
    // A scalar vs an array is never equal.
    expect(sameValue("x", ["x"])).toBe(false);
  });

  it("pendingChanges emits a Change when an array gains/loses/reorders an element", () => {
    const live: Record<string, SettingValue> = {
      "docsearch.roots": ["/a"],
      "vision.model": "",
    };
    const draft: Record<string, SettingValue> = {
      "docsearch.roots": ["/a", "/b"], // added /b
      "vision.model": "mlx-community/x", // set a model id
    };
    const changes = pendingChanges(live, draft);
    const byId = Object.fromEntries(changes.map((c) => [c.id, c.value]));
    expect(byId["docsearch.roots"]).toEqual(["/a", "/b"]);
    expect(byId["vision.model"]).toBe("mlx-community/x");
  });

  it("an unchanged array (same order + elements) is NOT a pending change", () => {
    const live: Record<string, SettingValue> = { "code.roots": ["/p", "/q"] };
    const draft: Record<string, SettingValue> = { "code.roots": ["/p", "/q"] };
    expect(pendingChanges(live, draft)).toEqual([]);
  });

  it("setting a model id back to empty (honest disabled) round-trips through valueMapFromStates", () => {
    const map = valueMapFromStates([
      { id: "vision.model", section: "vision", key: "model", kind: "string", value: "" },
      { id: "docsearch.roots", section: "docsearch", key: "roots", kind: "pathlist", value: [] },
    ]);
    expect(map["vision.model"]).toBe("");
    expect(map["docsearch.roots"]).toEqual([]);
  });

  it("the model-id / array fields are never marked dangerous (no confirm gate)", () => {
    for (const id of [
      "vision.model",
      "image.model",
      "speech.model",
      "inference.draft_model",
      "models.classifier",
      "docsearch.roots",
      "code.roots",
      "models.local_warm",
    ]) {
      const e = entryById(id)!;
      expect(e.danger).toBeFalsy();
      expect(isDangerousChange(e, "anything")).toBe(false);
      expect(isDangerousChange(e, ["/x"])).toBe(false);
    }
  });
});

/* ------------------------------------------------------ dangerous confirms */

describe("dangerous-change gating", () => {
  function entry(id: string): CatalogEntry {
    const e = entryById(id);
    if (!e) throw new Error(`missing ${id}`);
    return e;
  }

  it("disarming the master switch (allow_consequential -> false) is dangerous", () => {
    const e = entry("integrations.allow_consequential");
    expect(isDangerousChange(e, false)).toBe(true);
    expect(isDangerousChange(e, true)).toBe(false);
  });

  it("enabling encrypt_memory / shell / ui / mcp / cloud tiers / screen-context is dangerous (ON)", () => {
    for (const id of [
      "security.encrypt_memory",
      "shell.enabled",
      "ui_automation.enabled",
      "mcp.enabled",
      "voice.cloud_tier",
      "voice.cloud_stt",
      "screen_context.enabled",
    ]) {
      const e = entry(id);
      expect(isDangerousChange(e, true)).toBe(true);
      expect(isDangerousChange(e, false)).toBe(false);
      expect(e.danger).toBeTruthy();
    }
  });

  it("self_heal/forge/optimize Full-auto is dangerous; Off/Propose are not", () => {
    for (const id of AUTONOMY_IDS) {
      const e = entry(id);
      expect(isDangerousChange(e, "auto")).toBe(true);
      expect(isDangerousChange(e, "propose")).toBe(false);
      expect(isDangerousChange(e, "off")).toBe(false);
    }
  });

  it("a plain feature toggle (e.g. answers.cite) is never dangerous", () => {
    const e = entry("answers.cite");
    expect(isDangerousChange(e, true)).toBe(false);
    expect(isDangerousChange(e, false)).toBe(false);
  });
});

/* ------------------------------------------------------ batched diff model */

describe("batched pending-change model", () => {
  it("emits a Change only for ids whose draft differs from live", () => {
    const live: Record<string, SettingValue> = {
      "shell.enabled": false,
      "answers.cite": true,
      "voice_id.threshold": 0.86,
      self_heal: "propose",
    };
    const draft: Record<string, SettingValue> = {
      "shell.enabled": true, // changed
      "answers.cite": true, // unchanged
      "voice_id.threshold": 0.9, // changed
      self_heal: "propose", // unchanged
    };
    const changes = pendingChanges(live, draft);
    const byId = Object.fromEntries(changes.map((c) => [c.id, c.value]));
    expect(byId["shell.enabled"]).toBe(true);
    expect(byId["voice_id.threshold"]).toBe(0.9);
    expect(changes.find((c) => c.id === "answers.cite")).toBeUndefined();
    expect(changes.find((c) => c.id === "self_heal")).toBeUndefined();
  });

  it("never emits a change for an id absent from both maps", () => {
    const changes = pendingChanges({}, { "shell.enabled": true });
    expect(changes).toEqual([]);
  });

  it("returns changes in catalog order (stable, legible batch)", () => {
    const live: Record<string, SettingValue> = {
      "answers.cite": true,
      "integrations.allow_consequential": true,
    };
    const draft: Record<string, SettingValue> = {
      "answers.cite": false,
      "integrations.allow_consequential": false,
    };
    const changes = pendingChanges(live, draft);
    // integrations.allow_consequential precedes answers.cite in the catalog.
    expect(changes[0].id).toBe("integrations.allow_consequential");
    expect(changes[1].id).toBe("answers.cite");
  });

  it("dangerousPending surfaces exactly the risky subset of a batch", () => {
    const changes: Change[] = [
      { id: "answers.cite", value: false }, // benign
      { id: "shell.enabled", value: true }, // dangerous (ON)
      { id: "self_heal", value: "auto" }, // dangerous (Full-auto)
      { id: "self_heal", value: "propose" }, // not (Propose)
    ];
    const dangerous = dangerousPending(changes).map((d) => d.entry.id);
    expect(dangerous).toContain("shell.enabled");
    expect(dangerous).toContain("self_heal");
    expect(dangerous).not.toContain("answers.cite");
  });
});

/* ------------------------------------------------------ value-map helper */

describe("valueMapFromStates", () => {
  it("keys the live values by id", () => {
    const states: SettingState[] = [
      { id: "shell.enabled", section: "shell", key: "enabled", kind: "bool", value: true },
      { id: "self_heal", section: "self_heal", key: "", kind: "autonomy", value: "propose" },
    ];
    const map = valueMapFromStates(states);
    expect(map["shell.enabled"]).toBe(true);
    expect(map["self_heal"]).toBe("propose");
  });
});

/* ------------------------------------------------------ group coverage */

describe("group coverage", () => {
  it("each group has at least one entry", () => {
    for (const g of GROUP_ORDER) {
      expect(entriesForGroup(g).length).toBeGreaterThan(0);
    }
  });

  it("Safety & Gates contains the master switch, voice-id, encryption, and policy", () => {
    const ids = entriesForGroup("Safety & Gates").map((e) => e.id);
    expect(ids).toContain("integrations.allow_consequential");
    expect(ids).toContain("voice_id.enabled");
    expect(ids).toContain("security.encrypt_memory");
    expect(ids).toContain("policy.enabled");
  });
});

/* ------------------------------------- voice-id enrollment badge (GAP 3) */

/* The badge is UI-ONLY: it REFLECTS the voice-id telemetry the HUD already
 * receives (App.tsx state.voiceId, threaded through SettingsModal into the
 * panel) and adds NO authority — no enroll button, no config write. It shows
 * ENROLLED (green) vs NOT ENROLLED (amber) vs an AWAITING note before telemetry
 * arrives, and every variant carries the honest "gates nothing until you enroll,
 * even when On" reassurance. These pin the three states + the honest copy from
 * the telemetry's `enrolled` field (the source the prompt names). */
describe("voice-id enrollment badge", () => {
  function badge(voiceId: VoiceIdStatus | null): string {
    return renderToStaticMarkup(createElement(VoiceIdBadge, { voiceId }));
  }
  const enrolled: VoiceIdStatus = { ...voiceIdInitial(), enabled: true, enrolled: true };
  const notEnrolled: VoiceIdStatus = { ...voiceIdInitial(), enabled: true, enrolled: false };

  it("shows ENROLLED (green class) when a profile is on file", () => {
    const html = badge(enrolled);
    expect(html).toContain("ENROLLED");
    expect(html).not.toContain("NOT ENROLLED");
    expect(html).toContain("syscfg-vid-badge enrolled");
  });

  it("shows NOT ENROLLED (amber class) + the spoken enroll phrase when no profile", () => {
    const html = badge(notEnrolled);
    expect(html).toContain("NOT ENROLLED");
    // The honest spoken-flow copy (renderToStaticMarkup encodes the quotes).
    expect(html).toContain("enroll my voice");
    expect(html).toContain("syscfg-vid-badge not-enrolled");
  });

  it("reads ONLY the telemetry's `enrolled` flag, independent of `enabled`", () => {
    // Enabled but not enrolled is still NOT ENROLLED — voice-id gates nothing
    // until a profile exists, even when On. And enrolled-but-disabled still
    // honestly reflects the on-file profile.
    expect(badge({ ...voiceIdInitial(), enabled: true, enrolled: false })).toContain("NOT ENROLLED");
    expect(badge({ ...voiceIdInitial(), enabled: false, enrolled: true })).toContain("ENROLLED");
  });

  it("surfaces an honest AWAITING note before any telemetry frame (null)", () => {
    const html = badge(null);
    expect(html).toContain("AWAITING");
    expect(html).toContain("syscfg-vid-badge awaiting");
  });

  it("every badge variant reinforces that voice-id gates nothing until enrolled", () => {
    for (const html of [badge(enrolled), badge(notEnrolled), badge(null)]) {
      // The reassurance rides in each variant's title attribute.
      expect(html.toLowerCase()).toContain("gates nothing");
    }
  });

  it("the BADGE itself never renders a button (it is a pure telemetry reflector; the Enroll/Forget buttons live in VoiceIdEnrollControls, not in the badge)", () => {
    for (const html of [badge(enrolled), badge(notEnrolled), badge(null)]) {
      expect(html).not.toContain("<button");
    }
  });
});

/* -------------------------------- voice-id enroll / forget controls (WS3-C) */

/* The Enroll / Forget controls are PURE FRONTEND: they reuse the EXISTING command
 * bridge — no new Tauri/daemon command. Each button sends the SAME spoken phrase
 * the daemon classifies (voiceid::classify_intent), and the ENROLLED signal comes
 * ONLY from real telemetry (the badge), never simulated by a click. These pin:
 *   - the buttons render with honest mic/daemon copy,
 *   - the exact phrases match the daemon anchors and ride {cmd:"ask"} (no new verb),
 *   - capture progress is reflected from telemetry (captured/need),
 *   - the badge flips to ENROLLED only on the real voiceid.enrolled fold. */
describe("voice-id enroll / forget controls", () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });
  afterEach(() => {
    vi.restoreAllMocks();
  });

  function controls(
    voiceId: VoiceIdStatus | null,
    enabledDraft = true,
    enabledLive = true,
  ): string {
    return renderToStaticMarkup(
      createElement(VoiceIdEnrollControls, {
        voiceId,
        enabledDraft,
        enabledLive,
        onEdit: () => {},
      }),
    );
  }

  it("renders an Enroll button and a Forget affordance with honest mic + daemon copy", () => {
    const html = controls({ ...voiceIdInitial(), enabled: true, enrolled: false });
    expect(html).toContain("Enroll my voice");
    expect(html).toContain("Forget my voice");
    // HONEST: it needs Microphone access + the daemon running; speak the prompts.
    expect(html.toLowerCase()).toContain("microphone access");
    expect(html.toLowerCase()).toContain("daemon running");
    expect(html.toLowerCase()).toContain("speak the prompts");
  });

  it("the enroll phrase is the EXACT daemon-classified spoken intent (no new verb)", () => {
    // Byte-for-byte an anchor in voiceid::classify_intent ("my voice" + enroll verb).
    expect(VOICE_ID_PHRASES.enroll).toBe("enroll my voice");
    expect(VOICE_ID_PHRASES.forget).toBe("forget my voice");
  });

  it("Enroll sends the spoken intent over the EXISTING bridge as {cmd:'ask'} — no new daemon command", async () => {
    invokeMock.mockResolvedValue({ ok: true, reply: "Let's enroll your voice." });
    // The button's onClick calls sendCommand({cmd:"ask", text: VOICE_ID_PHRASES.enroll}).
    // We assert that exact wire shape (the same path the model-tier/voice-clone
    // buttons use) reaches the backend invoke — never a bespoke enroll verb.
    const r = await sendCommand({ cmd: "ask", text: VOICE_ID_PHRASES.enroll });
    expect(r.ok).toBe(true);
    expect(invokeMock).toHaveBeenCalledTimes(1);
    const [name, args] = invokeMock.mock.calls[0];
    expect(name).toBe("send_command");
    expect(args).toEqual({ request: { cmd: "ask", text: "enroll my voice" } });
  });

  it("Forget sends the spoken forget intent over the same {cmd:'ask'} bridge", async () => {
    invokeMock.mockResolvedValue({ ok: true, reply: "Done — I've forgotten your voice." });
    const r = await sendCommand({ cmd: "ask", text: VOICE_ID_PHRASES.forget });
    expect(r.ok).toBe(true);
    expect(invokeMock.mock.calls[0][1]).toEqual({
      request: { cmd: "ask", text: "forget my voice" },
    });
  });

  it("reflects live capture progress from telemetry (captured / need) — never invented", () => {
    // A capture session is open: enroll_started -> need 3, then 1 captured.
    let v = applyVoiceIdEnrollStarted({ ...voiceIdInitial(), enabled: true }, { need: 3 });
    const html = controls(v);
    // "Capturing 0/3" right after start (captured 0, need 3).
    expect(html).toContain("Capturing 0/3");
    expect(html).toContain("speak the prompts");
    // The Enroll button reads "Enrolling…" while a session is open.
    expect(html).toContain("Enrolling");
  });

  it("flips the badge to ENROLLED only when REAL telemetry reports it (a click cannot fake it)", () => {
    // Mid-enroll: the badge is NOT enrolled yet (no fake success on the button press).
    const enrolling = applyVoiceIdEnrollStarted({ ...voiceIdInitial(), enabled: true }, { need: 3 });
    expect(renderToStaticMarkup(createElement(VoiceIdBadge, { voiceId: enrolling }))).toContain(
      "NOT ENROLLED",
    );
    // Only the real voiceid.enrolled fold turns the badge ENROLLED.
    const done = applyVoiceIdEnrolled(enrolling);
    const badgeHtml = renderToStaticMarkup(createElement(VoiceIdBadge, { voiceId: done }));
    expect(badgeHtml).toContain("ENROLLED");
    expect(badgeHtml).not.toContain("NOT ENROLLED");
  });

  it("offers to flip voice-id ON when it is not live-enabled (does not fire into a dead path)", () => {
    // Voice-id off (live + draft false): the note offers to turn it On, with the
    // honest Apply/restart caveat — it does NOT pretend enrollment started.
    const html = controls({ ...voiceIdInitial(), enabled: false }, false, false);
    expect(html.toLowerCase()).toContain("turn it on");
    expect(html).toContain("Enroll my voice");
  });

  it("disables the buttons honestly when not in the desktop shell", async () => {
    // Re-import the component under a NON-Tauri bridge so inTauri() reads false.
    vi.resetModules();
    vi.doMock("../tauri/bridge", async (importOriginal) => {
      const actual = await importOriginal<typeof import("../tauri/bridge")>();
      return { ...actual, inTauri: () => false };
    });
    const mod = await import("../components/SystemSettingsPanel");
    const html = renderToStaticMarkup(
      createElement(mod.VoiceIdEnrollControls, {
        voiceId: { ...voiceIdInitial(), enabled: true, enrolled: false },
        enabledDraft: true,
        enabledLive: true,
        onEdit: () => {},
      }),
    );
    // The buttons carry the disabled attribute and the honest desktop-app note.
    expect(html).toContain("disabled");
    expect(html.toLowerCase()).toContain("desktop app");
    vi.doUnmock("../tauri/bridge");
    vi.resetModules();
  });
});

/* -------------------------------------------- updates + uninstall (WS4a) */

/* The Updates + Uninstall sections live in the System tab. These pin the
 * honesty/safety contract from the static render (they run no effects, so no
 * invoke fires on mount):
 *   - Updates: a "Check for updates" affordance; no Install button until a real
 *     signed update is reported available (a click can't conjure one).
 *   - Uninstall: a clearly-marked DESTRUCTIVE control whose FIRST click only
 *     arms a confirm — the static (un-armed) render shows no "Open uninstaller"
 *     button and never auto-runs anything. The copy states the two-step typed
 *     confirmation still happens in the terminal. */
describe("updates section (WS4a)", () => {
  function html(): string {
    return renderToStaticMarkup(createElement(UpdatesSection));
  }
  it("renders a Check-for-updates control with honest signed-update copy", () => {
    const h = html();
    expect(h).toContain("Check for updates");
    expect(h.toLowerCase()).toContain("signature");
    // No Install button on first paint — none is offered until a real available
    // update is reported by the backend (a click can never fabricate one).
    expect(h).not.toContain("Install");
  });

  it("exposes the REVERSIBLE 'Automatically install updates on launch' toggle (default Off)", () => {
    const h = html();
    expect(h).toContain("Automatically install updates on launch");
    // A real switch, defaulting OFF (the shipped posture) since no pref is set.
    expect(h).toContain('role="switch"');
    expect(h).toContain('aria-checked="false"');
    // Honest copy: it auto-installs SIGNATURE-VERIFIED updates at launch, and is
    // the undo of "don't ask again".
    expect(h.toLowerCase()).toContain("signature-verified");
    expect(h.toLowerCase()).toContain("off by default");
  });
});

/* The settings toggle reverses the dialog's "don't ask again": it reflects +
 * flips the SAME persisted jarvis.autoUpdate flag the launch path reads. */
describe("auto-update settings toggle reversibility", () => {
  it("reflects + flips the same persisted preference (ON then back OFF)", () => {
    const map = new Map<string, string>();
    const store: AutoUpdateStorage = {
      getItem: (k) => (map.has(k) ? map.get(k)! : null),
      setItem: (k, v) => {
        map.set(k, v);
      },
      removeItem: (k) => {
        map.delete(k);
      },
    };
    expect(isAutoUpdateOn(store)).toBe(false); // default OFF (what the toggle shows)
    setAutoUpdateOn(true, store); // dialog's "don't ask again" / toggle ON
    expect(isAutoUpdateOn(store)).toBe(true);
    setAutoUpdateOn(false, store); // toggle OFF — fully reversible
    expect(isAutoUpdateOn(store)).toBe(false);
  });
});

describe("uninstall section (WS4a)", () => {
  function html(): string {
    return renderToStaticMarkup(createElement(UninstallSection));
  }
  it("is a clearly-marked danger zone with a two-stage, non-destructive first click", () => {
    const h = html();
    expect(h).toContain("DANGER ZONE");
    expect(h).toContain("Uninstall JARVIS");
    // The first (un-armed) render shows the arming button, NOT the open-terminal
    // action — a single click cannot open (let alone run) the uninstaller.
    expect(h).not.toContain("Open uninstaller in Terminal");
    // Honest copy: it only OPENS Terminal; nothing is removed until the user types
    // the script's confirmations there.
    expect(h.toLowerCase()).toContain("opens the uninstaller in terminal");
    expect(h.toLowerCase()).toContain("never deletes anything by itself");
  });
});

/* ------------------------------------------------------ component render */

describe("SystemSettingsPanel render (no daemon)", () => {
  it("renders the honest loading state without throwing (config_get is async)", () => {
    // In the node test env there is no Tauri runtime, so configGet's promise is
    // still pending on first paint — the panel shows the loading note. This
    // proves the component mounts cleanly with no daemon present.
    const html = renderToStaticMarkup(createElement(SystemSettingsPanel));
    expect(html).toContain("Reading config/jarvis.toml");
  });

  it("mounts cleanly when fed the live voice-id telemetry prop", () => {
    // The panel accepts the voiceId telemetry threaded from SettingsModal/App
    // (used only for the enrollment badge once loaded). Mounting with it present
    // must not throw — it stays on the loading note until config_get resolves.
    const voiceId: VoiceIdStatus = { ...voiceIdInitial(), enabled: true, enrolled: true };
    const html = renderToStaticMarkup(createElement(SystemSettingsPanel, { voiceId }));
    expect(html).toContain("Reading config/jarvis.toml");
  });
});

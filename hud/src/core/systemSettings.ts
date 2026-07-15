/**
 * The SYSTEM SETTINGS catalog — the PURE, no-IO descriptor layer the
 * SystemSettingsPanel renders from. It mirrors the backend whitelist
 * (src-tauri/src/config_settings.rs SETTINGS + AUTONOMY_SECTIONS) and pairs each
 * editable key with the HONEST hint/warning copy LIFTED from the comments in
 * config/darwin.toml. The backend remains the trust boundary: this catalog only
 * decides what the UI offers + how it warns; every value still rides config_set's
 * strict whitelist validation, and a change takes effect only on daemon RESTART.
 *
 * KEPT IN SYNC: a unit test asserts every id here is a backend-whitelisted id
 * (and vice-versa), so a drift on either side fails CI. Nothing here can write a
 * key the backend does not already allow.
 */
import type { Change, SettingState, SettingValue } from "../tauri/configSettings";

/** The seven catalog groups, rendered in this order. */
export const GROUP_ORDER = [
  "Safety & Gates",
  "Autonomy",
  "Proactivity",
  "Perception",
  "Voice & Speech",
  "Capabilities",
  "Performance & Models",
] as const;

export type GroupName = (typeof GROUP_ORDER)[number];

/** The 3-way autonomy ids (each spans enabled+mode behind one control). Mirrors
 *  the backend AUTONOMY_SECTIONS. */
export const AUTONOMY_IDS = ["self_heal", "forge", "optimize"] as const;

/** The labels for the 3-way segmented control, in order. The stored values are
 *  "off" | "propose" | "auto" (backend AUTONOMY_STATES). */
export const AUTONOMY_OPTIONS: { value: string; label: string; danger?: boolean }[] = [
  { value: "off", label: "Off" },
  { value: "propose", label: "Propose" },
  { value: "auto", label: "Full-auto", danger: true },
];

/**
 * One catalog entry — the UI-facing descriptor for a single setting id. `kind`
 * is independent of the backend's coercion (the backend still re-validates); it
 * tells the panel which control to render. `danger` marks a setting whose
 * RISKY direction needs an explicit in-UI confirm; `dangerWhen` picks which
 * value direction is the risky one (so e.g. turning a gate OFF warns, turning it
 * back ON does not, and vice-versa).
 */
export interface CatalogEntry {
  id: string;
  group: GroupName;
  /** Short control label (the human name). */
  label: string;
  /** Control kind: a plain toggle, a 3-way autonomy segment, an enum select, a
   *  bounded number, a freeform text field (a model id), or a string-array list
   *  editor — "pathlist" adds a native folder picker (absolute paths), "strlist"
   *  is manual-add only (repo ids). */
  control: "toggle" | "autonomy" | "select" | "number" | "string" | "strlist" | "pathlist";
  /** For "string" / list controls — the placeholder shown in the (empty) input. */
  placeholder?: string;
  /** Honest one-to-three-line hint, lifted/condensed from the toml comment. */
  hint: string;
  /** When set, this setting's risky direction requires an explicit confirm; the
   *  copy is the warning shown in the confirm step. */
  danger?: string;
  /** Which value direction is the risky one that triggers the confirm. For a
   *  toggle: "on" (enabling is risky) or "off" (disabling is risky). For an
   *  autonomy control: "auto" (Full-auto is risky). Absent => never confirms. */
  dangerWhen?: "on" | "off" | "auto";
  /** For "select" — overrides the human labels for the enum option values. */
  optionLabels?: Record<string, string>;
  /** For "number" — a unit suffix shown beside the field (e.g. "secs"). */
  unit?: string;
  /** For "number" with a float kind — step granularity (defaults to 1). */
  step?: number;
}

/**
 * THE CATALOG. Order within a group is the render order. Hints are condensed
 * from config/darwin.toml's comments — kept honest (no overstated guarantees).
 */
export const CATALOG: CatalogEntry[] = [
  /* ----------------------------------------------------- SAFETY & GATES */
  {
    id: "integrations.allow_consequential",
    group: "Safety & Gates",
    label: "Allow consequential actions",
    control: "toggle",
    hint:
      "Master switch for outward/side-effecting actions (post, send, create). " +
      "OFF = every consequential action returns a DRY-RUN PREVIEW only and performs no side effect. " +
      "Even ON, each action still needs a fresh per-action confirm + voice-id (if enrolled) + !lockdown + policy.",
    danger:
      "Disarming the master switch puts every outward action back to DRY-RUN-PREVIEW only — confirmed sends/posts/spends will NOT happen until you re-arm and restart.",
    dangerWhen: "off",
  },
  {
    id: "voice_id.enabled",
    group: "Safety & Gates",
    label: "Voice-id gate",
    control: "toggle",
    hint:
      "On-device speaker verification (an added layer, NOT a high-assurance biometric — spoofable by replay). " +
      "Ships OFF: when off, OR with no enrolled profile, NOTHING is gated by voice. " +
      "Enrollment is always explicit (say \"enroll my voice\"); enabling without enrolling still gates nothing.",
  },
  {
    id: "voice_id.gate_scope",
    group: "Safety & Gates",
    label: "Voice-id scope",
    control: "select",
    optionLabels: { consequential: "Consequential only", all: "All commands (strict)" },
    hint:
      "\"consequential\": gate only outward actions + the confirmation replay. " +
      "\"all\": ALSO block non-consequential commands from an unrecognized speaker (stricter). Unknown falls back to consequential.",
  },
  {
    id: "voice_id.threshold",
    group: "Safety & Gates",
    label: "Voice-id threshold",
    control: "number",
    step: 0.01,
    hint:
      "Cosine ACCEPT point on the acoustic embedding (the operating point — tune on the real mic). " +
      "Higher = stricter (fewer false accepts, more false rejects). NOT a measured accuracy.",
  },
  {
    id: "security.encrypt_memory",
    group: "Safety & Gates",
    label: "Encrypt memory at rest",
    control: "toggle",
    hint:
      "SQLCipher AES-256 over the sensitive local stores. Ships OFF. " +
      "Protects AT REST ON DISK ONLY — while the daemon runs the key + decrypted pages are in RAM (not protected from a live/root attacker).",
    danger:
      "Enabling is an IRREVERSIBLE on-disk migration: a fresh 256-bit key is generated and written to the Keychain (account memory_encryption_key) and the plaintext stores are re-keyed. LOSE THAT KEYCHAIN ITEM AND THE DATA IS UNRECOVERABLE.",
    dangerWhen: "on",
  },
  {
    id: "policy.enabled",
    group: "Safety & Gates",
    label: "Per-action policy layer",
    control: "toggle",
    hint:
      "The layer master switch for the per-action ALLOW/NEVER/ASK rules (the rules themselves live in the Policy editor, not here). " +
      "Ships ON but INERT while the store is empty (Ask everywhere). false = bypass the layer entirely (every action is Ask).",
  },

  /* ----------------------------------------------------- AUTONOMY */
  {
    id: "self_heal",
    group: "Autonomy",
    label: "Self-heal",
    control: "autonomy",
    hint:
      "On an error burst: draft a unified-diff patch (needs the cloud key), build + test it staged. " +
      "Propose: write the validated patch + report and wait for a human to apply it. " +
      "Full-auto: the daemon APPLIES the patch to itself, rebuilds, and restarts — dangerous.",
    danger:
      "Self-heal Full-auto lets the daemon APPLY its own validated patch, rebuild --release, and EXIT so its supervisor restarts it. It edits and restarts DARWIN without you in the loop.",
    dangerWhen: "auto",
  },
  {
    id: "forge",
    group: "Autonomy",
    label: "Self-forge",
    control: "autonomy",
    hint:
      "Authors a new sandboxed micro-app from a goal (needs the cloud key), staged + validated + built. " +
      "Propose: write the validated app + report and wait for a human to apply. " +
      "Full-auto is RESERVED — it only governs the staged artifact and NEVER deploys (deploy is always a separate human step); it behaves as Propose for now.",
    danger:
      "Forge Full-auto only governs the staged artifact — it NEVER deploys a forged app into apps/ (that is always a separate human step). It behaves as Propose for now.",
    dangerWhen: "auto",
  },
  {
    id: "optimize",
    group: "Autonomy",
    label: "Optimize-from-usage",
    control: "autonomy",
    hint:
      "Records PII-redacted interaction traces and PROPOSES a measured tuning, adopted only if it beats baseline on held-out traces. " +
      "Propose: writes a diff a human reviews + applies. " +
      "Full-auto is RESERVED and behaves as Propose — there is no auto-apply-to-live path.",
    danger:
      "Optimize Full-auto is reserved: there is no auto-apply-to-live path. It behaves as Propose for now (writes a diff a human applies).",
    dangerWhen: "auto",
  },
  {
    id: "standing.enabled",
    group: "Autonomy",
    label: "Standing missions",
    control: "toggle",
    hint:
      "Durable scheduled goals. Even on: establishing a mission is itself confirmation-gated, and every consequential step still parks behind the confirm gate + master switch. Bounded to <=8 active missions.",
  },
  {
    id: "drafts.enabled",
    group: "Autonomy",
    label: "Auto-draft",
    control: "toggle",
    hint:
      "Composes a REVIEWABLE pending draft (email reply / message) — the drafts module has NO send path. An actual send is a separate explicit action that rides the existing gate. Enabling never enables an autonomous send.",
  },
  {
    id: "missions.durable",
    group: "Autonomy",
    label: "Durable missions",
    control: "toggle",
    hint:
      "Persist mission state across a restart. A persisted mission does NOT auto-run — it loads PAUSED and you must resume it; each consequential step re-runs through the same gate. Adds persistence, never autonomy.",
  },
  {
    id: "macros.enabled",
    group: "Autonomy",
    label: "Macros (record/replay)",
    control: "toggle",
    hint:
      "Record/replay a named sequence of commands (utterances + intent names only — never secrets/tokens). Replay re-runs each step through the normal gate FRESH — no pre-approval, no batching past the gate.",
  },

  /* ----------------------------------------------------- PROACTIVITY */
  {
    id: "proactive.enabled",
    group: "Proactivity",
    label: "First-contact brief",
    control: "toggle",
    hint:
      "After an away gap the next reply opens with verified context (time of day, system status, matching habits, pending heal proposal, facts learned while away).",
  },
  {
    id: "proactive.speak",
    group: "Proactivity",
    label: "Speak proactively",
    control: "toggle",
    hint:
      "Master gate for SPOKEN proactivity — EDITH also voices the brief (echo-safe speech path, never while already speaking), on top of the HUD proactive card. Off = HUD-card-only.",
  },
  {
    id: "proactive.suggest",
    group: "Proactivity",
    label: "Suggestions feed",
    control: "toggle",
    hint:
      "Proactive-intelligence suggester (habit detector + predictive suggester) — its OWN switch. Surfaces observed-pattern suggestion cards; accepting a habit offer still routes through the gated confirmation. DARWIN never auto-acts.",
  },
  {
    id: "proactive.quiet_start",
    group: "Proactivity",
    label: "Quiet hours start",
    control: "number",
    unit: "h",
    hint:
      "Quiet-hours band start (local hour, 0-23). Within [start,end) EDITH stays fully silent — no card either. Wraps midnight when start > end.",
  },
  {
    id: "proactive.quiet_end",
    group: "Proactivity",
    label: "Quiet hours end",
    control: "number",
    unit: "h",
    hint:
      "Quiet-hours band end (local hour, exclusive). quiet_start == quiet_end disables quiet hours.",
  },
  {
    id: "focus.profile",
    group: "Proactivity",
    label: "Focus profile",
    control: "select",
    optionLabels: {
      default: "Default (today's behavior)",
      work: "Work",
      sleep: "Sleep",
      deep_focus: "Deep focus",
    },
    hint:
      "A permission-NEUTRAL lens over the proactive surfaces — it can ONLY make DARWIN quieter, never loosen a gate. \"work\" silences news/routine/market; \"sleep\"/\"deep_focus\" surface only critical. Critical signals always survive.",
  },

  /* ----------------------------------------------------- PERCEPTION */
  {
    id: "screen_context.enabled",
    group: "Perception",
    label: "Continuous screen context",
    control: "toggle",
    hint:
      "The MOST privacy-sensitive read feature: a bounded/redacted/transient in-RAM ring of recent on-screen OCR snapshots, recallable + forgettable. Glyph-only, never a face; pixels never leave the device; READ-ONLY.",
    danger:
      "Screen context continuously captures and OCRs your screen (glyph-only, transient, redacted, never leaves the device). It is INERT without macOS Screen-Recording consent — the flag cannot grant it — but enabling arms the capture loop the moment consent exists.",
    dangerWhen: "on",
  },
  {
    id: "screen_context.interval_secs",
    group: "Perception",
    label: "Screen capture interval",
    control: "number",
    unit: "secs",
    hint:
      "Cadence the device-gated loop grabs ONE frame (floored to >= 1). A calmer (larger) or faster (smaller) watch.",
  },
  {
    id: "vision.enabled",
    group: "Perception",
    label: "Vision (VLM describe)",
    control: "toggle",
    hint:
      "On-device vision-language describe path. INERT WITHOUT A MODEL — with the vision model empty it returns honest vlm_unavailable (never fabricates). Needs an on-device Qwen2-VL-class checkpoint + RAM; pixels stay on-device.",
  },
  {
    id: "vision.model",
    group: "Perception",
    label: "Vision model id",
    control: "string",
    placeholder: "mlx-community/Qwen2-VL-… (empty = disabled)",
    hint:
      "HuggingFace repo id for the on-device VLM (e.g. a Qwen2-VL-class mlx-vlm model); empty = feature inert / disabled (honest vlm_unavailable, never fabricates). Needs the model downloaded + RAM; pixels stay on-device.",
  },
  {
    id: "image.enabled",
    group: "Perception",
    label: "Image generation",
    control: "toggle",
    hint:
      "On-device text->image (MLX diffusion, 100% on-device; no cloud image API). INERT WITHOUT A MODEL — with the image model empty it returns honest image_model_unavailable. Needs a FLUX.1-schnell-class checkpoint + RAM.",
  },
  {
    id: "image.model",
    group: "Perception",
    label: "Image model id",
    control: "string",
    placeholder: "mlx-community/… diffusion repo (empty = disabled)",
    hint:
      "HuggingFace repo id for the on-device diffusion model (e.g. a FLUX.1-schnell-class checkpoint); empty = feature inert / disabled (honest image_model_unavailable). Needs the model downloaded + RAM.",
  },
  {
    id: "audio.sound_monitor",
    group: "Perception",
    label: "Ambient sound monitor",
    control: "toggle",
    hint:
      "Periodically classifies a short ambient clip on-device (doorbell/alarm/glass-break/…). Only LABELS leave the op — the AUDIO never leaves the device. INERT WITHOUT MIC/TCC consent. Distinct from STT.",
  },
  {
    id: "interpret.live",
    group: "Perception",
    label: "Live interpretation",
    control: "toggle",
    hint:
      "Continuous live translation from the device-gated mic loop. INERT WITHOUT TCC/MIC consent. Quality is bounded by the local ~4B model; offline degrades honestly (never a fabricated translation).",
  },
  {
    id: "interpret.speak",
    group: "Perception",
    label: "Speak the translation",
    control: "toggle",
    hint:
      "Render-only by default — voicing the translation is its OWN opt-in. On = also speak each rendered translation through the echo-safe speech path.",
  },
  {
    id: "episodic.enabled",
    group: "Perception",
    label: "Episodic memory",
    control: "toggle",
    hint:
      "Durable, redacted, agent-scoped, BOUNDED memory of completed turns powering read-only recall. Recording is gated at the source (transient/voice-id-unverified turns are never recorded); inspectable + forgettable. Off = record nothing.",
  },

  /* ----------------------------------------------------- VOICE & SPEECH */
  {
    id: "voice.cloud_tier",
    group: "Voice & Speech",
    label: "Cloud TTS tier",
    control: "toggle",
    hint:
      "Optional ElevenLabs cloud voice layer on top of on-device Kokoro. INERT WITHOUT A KEY (needs elevenlabs_api_key + a non-Local tier). When active the TTS TEXT leaves the device. On-device Kokoro stays the fallback.",
    danger:
      "With the cloud TTS tier active (and a key present, non-Local tier) the TEXT DARWIN is about to speak LEAVES the device on a round trip to ElevenLabs. On-device Kokoro stays the private fallback.",
    dangerWhen: "on",
  },
  {
    id: "voice.cloud_stt",
    group: "Voice & Speech",
    label: "Cloud STT tier",
    control: "toggle",
    hint:
      "Separate switch for the ElevenLabs Scribe cloud-STT tier (transcription). Gated independently of cloud TTS. INERT WITHOUT A KEY. On-device whisper is the private default + fallback on any error.",
    danger:
      "With cloud STT active (and a key present, non-Local tier) your VOICE AUDIO leaves the device for transcription — MORE sensitive than the TTS text leg. On-device whisper stays the private fallback.",
    dangerWhen: "on",
  },
  {
    id: "voice.cloud_sfx",
    group: "Voice & Speech",
    label: "Cloud sound effects (ElevenLabs)",
    control: "toggle",
    hint:
      "Gates the ElevenLabs sound-effect CUE tier (a short text prompt -> a generated non-speech cue). " +
      "Ships ON but INERT WITHOUT A KEY — needs the EL key + a non-Local tier; there is no on-device SFX generator, so without a key (or offline) it is a silent no-op (never faked). " +
      "An added cue layer reached only through its explicit gate; it never changes the default speech path. When active the SFX text prompt leaves the device.",
  },
  {
    id: "voice.cloud_music",
    group: "Voice & Speech",
    label: "Cloud music (ElevenLabs)",
    control: "toggle",
    hint:
      "Gates the ElevenLabs MUSIC-generation tier (a text prompt -> a generated full track). " +
      "Ships ON but INERT WITHOUT A KEY — needs the EL key + a non-Local tier; there is no on-device music generator, so without a key (or offline) it is honestly unavailable (never faked). " +
      "An added music layer reached only through its explicit gate; it never changes the default speech path. When active the music text prompt leaves the device and it generates a full track (a cloud generation).",
  },
  {
    id: "voice.stream_tts",
    group: "Voice & Speech",
    label: "Streaming cloud TTS",
    control: "toggle",
    hint:
      "Opt-in LOWER-LATENCY ElevenLabs streaming TTS for faster first audio (ships OFF — default is today's blocking synthesis). " +
      "Active only when the resolved backend is ElevenLabs; the server FALLS BACK to the standard blocking path on ANY streaming error (a turn is never failed by the streaming leg). Inert on on-device Kokoro. Delivery timing only — never a gate.",
  },
  {
    id: "voice.event_cues",
    group: "Voice & Speech",
    label: "Event sound cues",
    control: "toggle",
    hint:
      "Opt-in (ships OFF): plays a short fire-and-forget ElevenLabs cue on a couple of key events (a confirmed action plays \"success\", a denied one plays \"notify\"). " +
      "The cue is spawned AFTER the action already completed, so it NEVER blocks, delays, or changes the outcome/reply. " +
      "Requires cloud_sfx + an EL key (rides the same gate); without that it is a silent no-op. Delivery only — never a gate.",
  },
  {
    id: "voice.mic_source",
    group: "Voice & Speech",
    label: "Microphone owner",
    control: "select",
    optionLabels: {
      device: "Background service (default)",
      app: "DARWIN app — clean prompt",
    },
    hint:
      "WHO opens the microphone. \"Background service\" (default) keeps today's behavior — the daemon opens the mic itself. " +
      "\"DARWIN app\" makes the app capture the mic, so macOS shows a clean \"DARWIN\" permission prompt, and the app streams the audio to the service over a local socket. " +
      "After switching to \"DARWIN app\", RESTART the daemon; the app then asks for the microphone the next time it launches.",
  },
  {
    id: "voice.pronunciation_dictionary_id",
    group: "Voice & Speech",
    label: "Pronunciation dictionary id",
    control: "string",
    placeholder: "empty = none (today's pronunciation)",
    hint:
      "Pins HOW THE CLOUD VOICE SAYS names/acronyms: the active ElevenLabs pronunciation-dictionary id threaded into speak as a locator. " +
      "Empty = NONE — no locator is sent, so speech is byte-for-byte today's. Paste the dictionary_id the create_pronunciation op returns. EL-pronunciation-gated; inert on Kokoro.",
  },
  {
    id: "voice.pronunciation_dictionary_version",
    group: "Voice & Speech",
    label: "Pronunciation dictionary version",
    control: "string",
    placeholder: "empty = latest version",
    hint:
      "OPTIONAL version pin for the pronunciation dictionary above. " +
      "Empty = NONE (latest version). Paste the version_id the create_pronunciation op returns to pin it. Inert when the id is empty.",
  },
  {
    id: "voice.adaptive_prosody",
    group: "Voice & Speech",
    label: "Adaptive prosody",
    control: "toggle",
    hint:
      "Expressiveness-only: picks a prosody profile and emits EL-v3 audio-tags ONLY on EL-v3 backends (on Kokoro the mapping is coarse/neutral). Changes delivery, never a gate.",
  },
  {
    id: "voice.whisper",
    group: "Voice & Speech",
    label: "Whisper / discreet mode",
    control: "toggle",
    hint:
      "Explicit-command terse + soft delivery. Changes DELIVERY ONLY — never suppresses a required safety confirmation (a required confirm still speaks, softly).",
  },
  {
    id: "voice.whisper_auto",
    group: "Voice & Speech",
    label: "Auto-engage whisper",
    control: "toggle",
    hint:
      "Auto-engage whisper by sustained low-amplitude input. Pure energy-series heuristic; does NOT open the mic itself. Delivery-only.",
  },
  {
    id: "voice.diarize",
    group: "Voice & Speech",
    label: "Diarization",
    control: "toggle",
    hint:
      "Multi-speaker diarization — consumes speaker labels the EL Scribe STT backend reports. INERT ON-DEVICE: on-device whisper has no diarization model => honest single-stream \"speaker: unknown\" (never fabricated speakers).",
  },
  {
    id: "speech.engine",
    group: "Voice & Speech",
    label: "TTS engine",
    control: "select",
    optionLabels: { kokoro: "Kokoro (on-device default)", csm: "CSM (Sesame)", orpheus: "Orpheus" },
    hint:
      "On-device TTS engine. Adoption gate: warm RTF <= 0.5 + clean load. Per the shipped eval, kokoro PASSES; csm and orpheus FAIL the RTF gate on this hardware (and share the GPU with the 4B LLM), so kokoro is the default.",
  },
  {
    id: "speech.model",
    group: "Voice & Speech",
    label: "TTS model id",
    control: "string",
    placeholder: "empty = engine default (e.g. Kokoro-82M-bf16)",
    hint:
      "HuggingFace repo id overriding the chosen engine's default checkpoint; empty = engine default (kokoro: mlx-community/Kokoro-82M-bf16, csm: mlx-community/csm-1b-8bit, orpheus: mlx-community/orpheus-3b-0.1-ft-4bit).",
  },
  {
    id: "speech.instant_opener",
    group: "Voice & Speech",
    label: "Instant opener",
    control: "toggle",
    hint:
      "Master gate for the canned instant acknowledgment (\"Right away, sir.\") that plays the instant an utterance ends while STT runs concurrently. Pure UX, no safety surface. Off = the converse stream is the whole reply.",
  },

  /* ----------------------------------------------------- CAPABILITIES */
  {
    id: "shell.enabled",
    group: "Capabilities",
    label: "Sandboxed shell",
    control: "toggle",
    hint:
      "The HIGHEST-RISK capability (arbitrary command execution). Even ON it NEVER auto-runs: shell_run parks per-action even under an \"Always\" policy, clears a destructive denylist, and execs only under master switch + confirm + voice-id + !lockdown in a deny-default sandbox.",
    danger:
      "Shell enables ARBITRARY COMMAND EXECUTION. Even ON it parks per-action (never auto-approved, even under an \"Always\" policy) and execs only under the full gate stack in a deny-default sandbox-exec profile.",
    dangerWhen: "on",
  },
  {
    id: "ui_automation.enabled",
    group: "Capabilities",
    label: "UI automation",
    control: "toggle",
    hint:
      "The capstone — physically actuating the macOS UI. Even ON it NEVER auto-runs: ui_actuate parks PER ACTION (one confirm = one actuation), fires only under the full gate stack, never batched. INERT WITHOUT Accessibility consent.",
    danger:
      "UI automation physically ACTUATES the macOS UI (clicks, keystrokes). Even ON it parks PER ACTION (one confirm = one actuation, a second re-parks) and fires only under the full gate stack. INERT without Accessibility consent.",
    dangerWhen: "on",
  },
  {
    id: "ui_automation.actuate_via_app",
    group: "Capabilities",
    label: "Actuate via the DARWIN app",
    control: "toggle",
    hint:
      "WHERE an already-approved click/keystroke is posted. OFF (default): the background service posts it (grant Accessibility to the service). " +
      "ON: the DARWIN app posts it, so macOS shows a clean \"DARWIN would like to control this computer\" prompt and the grant is the app's. " +
      "EVERY safety gate (per-action confirm, bounds, the full gate stack) still runs in the service BEFORE the post — this only changes who posts the final event. Restart the daemon after changing.",
  },
  {
    id: "mcp.enabled",
    group: "Capabilities",
    label: "MCP client",
    control: "toggle",
    hint:
      "Connect to external MCP tool servers — the MOST DANGEROUS external surface (an MCP server runs code on your machine). INERT WITHOUT SERVERS: the servers list ships empty, so nothing connects until you add a [[mcp.servers]] entry. Every consequential MCP tool still rides the full gate.",
    danger:
      "MCP connects to external tool servers — code that runs on YOUR machine and exposes tools an agent can call. It is INERT until you add a [[mcp.servers]] entry to darwin.toml; even then every consequential MCP tool parks behind the full gate.",
    dangerWhen: "on",
  },
  {
    id: "webhooks.enabled",
    group: "Capabilities",
    label: "Inbound webhooks",
    control: "toggle",
    hint:
      "The most security-sensitive inbound surface. INERT WITHOUT MAPPINGS + SECRET (mappings ship empty; the HMAC secret is in the Keychain, never this file). Loopback-only bind. A mapped consequential intent still PARKS for a spoken confirm.",
  },
  {
    id: "plugin_sdk.enabled",
    group: "Capabilities",
    label: "Plugin SDK",
    control: "toggle",
    hint:
      "The micro-app capability-module validator + register-on-launch handshake. A plugin can't request a capability outside its allowed set (over-privileged manifests are rejected), can't escape the default-deny SBPL, and any consequential tool it exposes still rides the gate.",
  },
  {
    id: "docsearch.enabled",
    group: "Capabilities",
    label: "On-device file search",
    control: "toggle",
    hint:
      "On-device file RAG — contents + embeddings NEVER leave the device. INERT WITHOUT ROOTS: the folder allowlist ships empty, so even enabled it indexes nothing until you allowlist a folder (set roots in darwin.toml). Text-like files only.",
  },
  {
    id: "docsearch.roots",
    group: "Capabilities",
    label: "File-search folders",
    control: "pathlist",
    placeholder: "/Users/you/Documents",
    hint:
      "Folders DARWIN may index for file/code search; contents stay on-device; nothing is read until a folder is added. Each entry is an ABSOLUTE path. Use Add folder… (native picker) or type an absolute path.",
  },
  {
    id: "docsearch.build_graph",
    group: "Capabilities",
    label: "Knowledge-graph build",
    control: "toggle",
    hint:
      "Deterministic knowledge-graph build over already-indexed chunks (never re-walks disk), writing the shared provenance-tagged user.world.* tier. INERT WITHOUT INDEXED DOCS (needs docsearch roots + an index).",
  },
  {
    id: "code.enabled",
    group: "Capabilities",
    label: "Code intelligence",
    control: "toggle",
    hint:
      "code_explain (grounded, CITED answers over the on-device code index) + code_propose_diff (PROPOSE-ONLY reviewable unified diff — never edits your code). INERT WITHOUT ROOTS. The only path that touches your code is the human-reviewed apply script, confined to the allowlisted root.",
  },
  {
    id: "code.roots",
    group: "Capabilities",
    label: "Code-intelligence roots",
    control: "pathlist",
    placeholder: "/Users/you/project",
    hint:
      "Folders DARWIN may index for file/code search; contents stay on-device; nothing is read until a folder is added. The apply script writes ONLY under a root here. Also add the same root under file-search folders so the code is retrievable.",
  },
  {
    id: "local_tools.enabled",
    group: "Capabilities",
    label: "Offline tool-loop",
    control: "toggle",
    hint:
      "Gives the on-device 4B BOUNDED agency over a curated SAFE local-tool subset (local read/compute only — never outward/cloud). Every tool runs through the same gated execute_tool, so the confirm gate + voice-id + lockdown + policy all still apply offline.",
  },
  {
    id: "report.enabled",
    group: "Capabilities",
    label: "Report generation",
    control: "toggle",
    hint:
      "Read-only: folds already-cited notebook/research material into one bounded markdown report under cite discipline. Never fabricates a source/body (honest-empty when none). Speaks/displays only — reaches nothing.",
  },
  {
    id: "chart.enabled",
    group: "Capabilities",
    label: "Data -> chart",
    control: "toggle",
    hint:
      "Neutral presentation: serializes the EXACT data points as a chart envelope the HUD plots exactly — no interpolation, no invented point, honest-empty. Changes no gate, takes no action, reaches no network.",
  },
  {
    id: "answers.cite",
    group: "Capabilities",
    label: "Always cite sources",
    control: "toggle",
    hint:
      "An answer is followed by a \"Sources:\" line naming the REAL tool-result sources (or \"from my own knowledge\" when no retrieval ran) — never a fabricated citation.",
  },
  {
    id: "answers.confidence",
    group: "Capabilities",
    label: "Self-reported confidence",
    control: "toggle",
    hint:
      "The model self-reports grounded/inferred/uncertain + a one-line why. The plumbing is what ships tested; calibration QUALITY is model-behavior-gated and is never claimed measured.",
  },
  {
    id: "answers.verify",
    group: "Capabilities",
    label: "Self-verification pass",
    control: "toggle",
    hint:
      "On an IMPORTANT turn: one bounded self-critique of the draft against the real sources + at most one revise (skips trivial turns; needs the cloud tier). REDUCES hallucination; not a correctness guarantee; costs one extra model call.",
  },
  {
    id: "answers.cross_check",
    group: "Capabilities",
    label: "Tool-result cross-check",
    control: "toggle",
    hint:
      "Bounded deterministic plausibility cross-check of a tool result before it is surfaced/built into an action. A tripped check only DOWNGRADES confidence + flags a caveat — it NEVER removes a confirmation gate.",
  },
  {
    id: "answers.debate",
    group: "Capabilities",
    label: "Multi-model debate",
    control: "toggle",
    hint:
      "High-stakes-only two-model debate (conservative predicate; ordinary turns never debate). Agreement raises confidence; disagreement surfaces BOTH (never a fake consensus). Bounded to <=2 model calls; needs the cloud tier.",
  },

  /* ----------------------------------------------------- PERFORMANCE & MODELS */
  {
    id: "power.adaptive",
    group: "Performance & Models",
    label: "Battery/thermal adaptive",
    control: "toggle",
    hint:
      "PERF-ONLY: on low battery (discharging) or serious thermal pressure the policy may prefer the cheaper local Fast sub-tier + defer heavy work. NEVER loosens a gate, never makes a cloud call.",
  },
  {
    id: "inference.speculative",
    group: "Performance & Models",
    label: "Speculative decoding",
    control: "toggle",
    hint:
      "Master gate for speculative/draft decoding. INERT WITHOUT A DRAFT MODEL — also needs a loadable draft_model (ships empty); absent that, generate falls back to normal gen and honestly reports speculative=false (never faked).",
  },
  {
    id: "inference.draft_model",
    group: "Performance & Models",
    label: "Draft model id",
    control: "string",
    placeholder: "mlx-community/Qwen3-0.6B-… (empty = inert)",
    hint:
      "HuggingFace repo id of the small DRAFT checkpoint mlx_lm uses to propose tokens; empty = feature inert / disabled (speculative reports false even when its gate is on). Set a real downloadable checkpoint to engage; on-device only.",
  },
  {
    id: "models.classifier",
    group: "Performance & Models",
    label: "Classifier model id",
    control: "string",
    placeholder: "empty = reuse the main llm",
    hint:
      "HuggingFace repo id of a dedicated small resident model for op=classify; empty = reuse the main llm. Every shipped small candidate failed the 7-utterance accuracy gate (main 4B = 7/7) — only set this after a candidate passes >=6/7.",
  },
  {
    id: "models.local_warm",
    group: "Performance & Models",
    label: "Warm-set model ids",
    control: "strlist",
    placeholder: "mlx-community/Qwen3-0.6B-Instruct-4bit",
    hint:
      "Extra local model ids kept warm beside the base llm (multi-resident) so the LOCAL tier can swap instantly with no reload. Empty = single-resident (today's behavior). RAM-bounded by the budget below; ~2x RAM per warm model — the swap-speed benefit is device-gated, not a measured claim.",
  },
  {
    id: "models.local_budget_gib",
    group: "Performance & Models",
    label: "Warm-set RAM budget",
    control: "number",
    unit: "GiB",
    step: 0.5,
    hint:
      "RAM budget (GiB) the warm-set may occupy. 0 (default) or any non-positive value = single-resident (only the base llm warm). A positive budget admits extras up to that estimated total; if the base llm alone exceeds it, single-resident is kept. Set conservatively for the Mac's actual free unified memory.",
  },
  {
    id: "inference.quant",
    group: "Performance & Models",
    label: "Local model quantization",
    control: "select",
    optionLabels: { auto: "Auto (as configured)", fp16: "fp16", int8: "int8", int4: "int4" },
    hint:
      "Selectable quantization for the local model load. \"auto\" = today's behavior. An explicit value selects a matching variant; the loader falls back + reports the quant that ACTUALLY loaded (never claims int4 when fp16 loaded). Real tradeoff is device-gated.",
  },
  {
    id: "router.conversation_route",
    group: "Performance & Models",
    label: "Conversation route",
    control: "select",
    optionLabels: {
      cloud_heavy: "Cloud heavy (Opus)",
      cloud_fast: "Cloud fast (Haiku)",
      local: "Local (offline / on-device)",
    },
    hint:
      "Where casual chat/greetings/opinions are answered. \"cloud_heavy\": cloud Opus (varied personality); \"cloud_fast\": cloud Haiku; \"local\": the resident 4B (offline). Cloud variants need the cloud key; with no key chat degrades to the local 4B. Actions/queries are unaffected.",
  },
];

/** The catalog entry for an id, or undefined for an unknown id. */
export function entryById(id: string): CatalogEntry | undefined {
  return CATALOG.find((e) => e.id === id);
}

/** The catalog entries belonging to a group, in catalog (render) order. */
export function entriesForGroup(group: GroupName): CatalogEntry[] {
  return CATALOG.filter((e) => e.group === group);
}

/* --------------------------------------------------------------- diff model */

/** Compare two setting values for equality across the bool/number/string/array
 *  union. Arrays compare by ORDER + element (a reorder or an add/remove counts as
 *  a change); scalars compare by identity. */
export function sameValue(a: SettingValue, b: SettingValue): boolean {
  if (Array.isArray(a) || Array.isArray(b)) {
    if (!Array.isArray(a) || !Array.isArray(b)) return false;
    if (a.length !== b.length) return false;
    return a.every((v, i) => v === b[i]);
  }
  return a === b;
}

/**
 * Build the pending-change batch: for every id whose drafted value differs from
 * the live (server) value, emit a Change. Ids not present in the live map (or
 * the catalog) are ignored — nothing off the known set is ever written. Returns
 * the changes in catalog order so the batch is stable + legible.
 */
export function pendingChanges(
  live: Record<string, SettingValue>,
  draft: Record<string, SettingValue>,
): Change[] {
  const changes: Change[] = [];
  for (const entry of CATALOG) {
    const id = entry.id;
    if (!(id in live) || !(id in draft)) continue;
    if (!sameValue(live[id], draft[id])) {
      changes.push({ id, value: draft[id] });
    }
  }
  return changes;
}

/** True iff this drafted value, vs its live value, is the RISKY direction for
 *  the entry — the direction that requires an explicit in-UI confirm. */
export function isDangerousChange(
  entry: CatalogEntry,
  draftValue: SettingValue,
): boolean {
  if (!entry.danger || !entry.dangerWhen) return false;
  switch (entry.dangerWhen) {
    case "on":
      return draftValue === true;
    case "off":
      return draftValue === false;
    case "auto":
      return draftValue === "auto";
    default:
      return false;
  }
}

/**
 * The dangerous entries within a pending batch — the subset whose drafted value
 * is the risky direction. The Apply flow shows EACH of these (with its warning
 * copy) in the confirm step so a risky change can never slip through on a single
 * click.
 */
export function dangerousPending(
  changes: Change[],
): { entry: CatalogEntry; value: SettingValue }[] {
  const out: { entry: CatalogEntry; value: SettingValue }[] = [];
  for (const change of changes) {
    const entry = entryById(change.id);
    if (entry && isDangerousChange(entry, change.value)) {
      out.push({ entry, value: change.value });
    }
  }
  return out;
}

/** Build the live + draft value maps from the backend's SettingState list. Both
 *  maps start identical; the draft is mutated locally as the user edits. */
export function valueMapFromStates(
  states: SettingState[],
): Record<string, SettingValue> {
  const map: Record<string, SettingValue> = {};
  for (const s of states) {
    map[s.id] = s.value;
  }
  return map;
}

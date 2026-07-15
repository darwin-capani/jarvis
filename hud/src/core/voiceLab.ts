/**
 * VOICE LAB — the PURE request-shaping + gate + outcome logic behind the HUD's
 * two ElevenLabs provisioning controls (Phase-2):
 *
 *   - DESIGN A VOICE   — design an ElevenLabs voice for an agent from a text
 *     DESCRIPTION (the daemon `design_voice` verb). Only the text description
 *     leaves the device; the returned voice id is stored daemon-side.
 *   - ADD A PRONUNCIATION — mint a single-alias pronunciation rule (word -> say,
 *     the daemon `create_pronunciation` verb). Text rules only; no audio leaves
 *     the device.
 *
 * No DOM/React/Tauri imports here so the request shaping, the gate predicate, and
 * the outcome->prose mapping are verifiable headlessly under vitest (node env),
 * exactly like sfxCue.ts.
 *
 * HONESTY CONTRACT (do not regress):
 *   - These controls add NO authority and NO new tier. A request only ever does
 *     anything when the daemon's already-shipped gate is open: a non-Local
 *     (cloud) model tier AND an ElevenLabs key on file. This module's
 *     `voiceLabGateOpen` mirrors the HUD-knowable half of that predicate
 *     (`voice.cloud_tier` switch ON + a key present) for the COPY + the
 *     enabled/disabled state only — the REAL gate still lives daemon-side
 *     (`model_tier::active_tier` != Local + the Keychain key); the HUD never
 *     fabricates a voice or a dictionary.
 *   - With the switch off / no key, the affordance is an honest, disabled control
 *     with copy that says nothing will be created — never a faked "designed"
 *     state. The daemon ALSO re-checks the gate and returns an honest `Err` when
 *     it is closed (e.g. offline); we surface that verbatim, never a fake success.
 *   - SECRET-FREE. This module sees only booleans (switch on, key present) and
 *     the free-text request fields (agent name, description, word, say) — never
 *     the key value, never the returned voice/dictionary id.
 *
 * The validation floors mirror daemon/src/command.rs `decide`:
 *   - a non-empty agent,
 *   - a description of at least `MIN_VOICE_DESCRIPTION_CHARS` (the EL design
 *     floor),
 *   - a non-empty word + a non-empty say for a pronunciation rule.
 */

/** The EL voice-design prompt floor — mirrors daemon
 *  `command.rs::MIN_VOICE_DESCRIPTION_CHARS`. A description below this is rejected
 *  HERE (the daemon also rejects it) so a too-thin request never rides the wire. */
export const MIN_VOICE_DESCRIPTION_CHARS = 20;

/** The free-text clamp — mirrors the daemon `MAX_TEXT_CHARS`. The backend relay
 *  also clamps, but trimming here keeps an oversized field from being shaped into
 *  a request at all. */
export const MAX_TEXT_CHARS = 4 * 1024;

/* ------------------------------------------------------------------------ *
 * Gate mirror — the HONEST copy + enabled/disabled half. The REAL gate is    *
 * daemon-side (a non-Local model tier AND a Keychain ElevenLabs key). This   *
 * predicate decides only whether the HUD's controls are ENABLED and what the *
 * explanatory copy says; it never itself creates anything.                   *
 * ------------------------------------------------------------------------ */

/** The two secret-free inputs the HUD knows locally: whether the
 *  `voice.cloud_tier` switch is ON, and whether an ElevenLabs key is on file
 *  (presence only — never the value). Mirrors the switch+key half of the daemon
 *  gate (`model_tier::active_tier != Local` + the resolved key). */
export interface VoiceLabGateInputs {
  /** `[voice].cloud_tier` — the cloud TTS tier switch (ships OFF). */
  cloudTierOn: boolean;
  /** An ElevenLabs key is on file in the Keychain (presence only). */
  keyPresent: boolean;
}

/**
 * The switch+key half of the daemon gate. True only when the cloud tier is ON
 * **and** a key is present. The daemon ALSO requires a non-Local (online) tier at
 * call time; the HUD can't always know that locally, so a "gate open" here means
 * "the HUD won't block the attempt" — the daemon still makes the final, honest
 * call and returns an `Err` if it's offline. PURE.
 */
export function voiceLabGateOpen(g: VoiceLabGateInputs): boolean {
  return g.cloudTierOn && g.keyPresent;
}

/** Why the Voice Lab controls are disabled (or null when enabled). Drives the
 *  honest copy: the operator is told EXACTLY what's missing so the control is
 *  never a mystery and never pretends it will create a voice/dictionary. PURE. */
export type VoiceLabDisabledReason =
  | "no_shell" // running in a plain browser, not the desktop app
  | "tier_off" // [voice].cloud_tier is OFF
  | "no_key" // no ElevenLabs key on file
  | "tier_off_and_no_key"; // both missing

/** Resolve the disabled reason (or null = enabled) from the shell + gate inputs.
 *  `no_shell` takes precedence (nothing can be created without the daemon);
 *  otherwise the missing gate pieces are reported precisely. PURE. */
export function voiceLabDisabledReason(
  inTauri: boolean,
  g: VoiceLabGateInputs,
): VoiceLabDisabledReason | null {
  if (!inTauri) return "no_shell";
  if (!g.cloudTierOn && !g.keyPresent) return "tier_off_and_no_key";
  if (!g.cloudTierOn) return "tier_off";
  if (!g.keyPresent) return "no_key";
  return null;
}

/** Honest, secret-free explanatory copy for the current gate state. When enabled
 *  it states the on-attempt caveat (an offline/Local tier still fails honestly,
 *  creating nothing); when disabled it names exactly what to turn on. NEVER claims
 *  anything was created. PURE. */
export function voiceLabGateCopy(reason: VoiceLabDisabledReason | null): string {
  switch (reason) {
    case null:
      return (
        "Ready. Designing a voice or adding a pronunciation calls ElevenLabs once " +
        "and stores the result on this device. If you're offline (Local tier) " +
        "nothing is created — it never fabricates a voice or a dictionary."
      );
    case "no_shell":
      return "The Voice Lab requires the DARWIN desktop app.";
    case "tier_off":
      return "The cloud tier is off. Turn on cloud TTS ([voice].cloud_tier) to use the Voice Lab.";
    case "no_key":
      return "Add an ElevenLabs key (elevenlabs_api_key) to use the Voice Lab.";
    case "tier_off_and_no_key":
      return (
        "The Voice Lab needs cloud TTS ([voice].cloud_tier) ON and an ElevenLabs " +
        "key on file. With either missing, nothing is created."
      );
  }
}

/* ------------------------------------------------------------------------ *
 * Design-a-voice request shaping. PURE: it validates the agent + description *
 * against the daemon's floors and packages them; it injects no token and     *
 * performs no I/O.                                                            *
 * ------------------------------------------------------------------------ */

/** The bounded request to design a voice. Carries ONLY the agent, the text
 *  description, and an optional display name — never a key, never an id. The
 *  `name` is omitted entirely when blank (the daemon defaults it to the agent
 *  name), so the wire shape stays minimal. */
export interface DesignVoiceRequest {
  agent: string;
  description: string;
  name?: string;
}

/** Why a design request could not be shaped (or null when it's valid). Mirrors
 *  the daemon `decide` rejections so the HUD can show a precise inline error
 *  BEFORE any round-trip. PURE. */
export type DesignVoiceError = "no_agent" | "description_too_short";

/** Clamp a free-text field to [`MAX_TEXT_CHARS`] (char-boundary safe), the same
 *  clamp the daemon + the backend relay apply. PURE. */
function clampText(s: string): string {
  if (s.length <= MAX_TEXT_CHARS) return s;
  return Array.from(s).slice(0, MAX_TEXT_CHARS).join("");
}

/**
 * Shape a design-voice request from the raw form fields. Returns a structured
 * error (never throws) when the agent is empty or the description is below the EL
 * design floor — the SAME checks the daemon's `decide` makes, so a too-thin
 * request never spends a cloud op. The `name` is trimmed; when blank it is OMITTED
 * (the daemon defaults it to the agent name). PURE.
 */
export function buildDesignVoiceRequest(
  agentRaw: string,
  descriptionRaw: string,
  nameRaw: string,
): { ok: true; request: DesignVoiceRequest } | { ok: false; error: DesignVoiceError } {
  const agent = clampText(agentRaw.trim());
  if (agent.length === 0) {
    return { ok: false, error: "no_agent" };
  }
  const description = clampText(descriptionRaw.trim());
  // Count Unicode code points (matches the daemon's `chars().count()`).
  if (Array.from(description).length < MIN_VOICE_DESCRIPTION_CHARS) {
    return { ok: false, error: "description_too_short" };
  }
  const name = clampText(nameRaw.trim());
  const request: DesignVoiceRequest = name.length > 0
    ? { agent, description, name }
    : { agent, description };
  return { ok: true, request };
}

/** Inline copy for a design-request validation error. PURE. */
export function designVoiceErrorCopy(error: DesignVoiceError): string {
  switch (error) {
    case "no_agent":
      return "Pick an agent for the new voice.";
    case "description_too_short":
      return `Describe the voice in a bit more detail (at least ${MIN_VOICE_DESCRIPTION_CHARS} characters).`;
  }
}

/* ------------------------------------------------------------------------ *
 * Add-a-pronunciation request shaping. PURE: it validates the word + say     *
 * and packages a single alias rule; it injects no token and performs no I/O. *
 * ------------------------------------------------------------------------ */

/** The bounded request to add ONE pronunciation rule (word -> say). Carries only
 *  the two text fields + an optional dictionary name — never a key, never an id. */
export interface PronunciationRequest {
  word: string;
  say: string;
  name?: string;
}

/** Why a pronunciation request could not be shaped (or null when valid). Mirrors
 *  the daemon `decide` rejections. PURE. */
export type PronunciationError = "no_word" | "no_say";

/**
 * Shape a create-pronunciation request from the raw form fields. Returns a
 * structured error (never throws) when either the word to replace or the alias is
 * empty — the SAME checks the daemon's `decide` makes. The `name` is trimmed; when
 * blank it is OMITTED (the daemon defaults it to a fixed label). PURE.
 */
export function buildPronunciationRequest(
  wordRaw: string,
  sayRaw: string,
  nameRaw: string,
): { ok: true; request: PronunciationRequest } | { ok: false; error: PronunciationError } {
  const word = clampText(wordRaw.trim());
  if (word.length === 0) {
    return { ok: false, error: "no_word" };
  }
  const say = clampText(sayRaw.trim());
  if (say.length === 0) {
    return { ok: false, error: "no_say" };
  }
  const name = clampText(nameRaw.trim());
  const request: PronunciationRequest = name.length > 0
    ? { word, say, name }
    : { word, say };
  return { ok: true, request };
}

/** Inline copy for a pronunciation-request validation error. PURE. */
export function pronunciationErrorCopy(error: PronunciationError): string {
  switch (error) {
    case "no_word":
      return "Enter the word or phrase to respell.";
    case "no_say":
      return "Enter how it should be said.";
  }
}

/* ------------------------------------------------------------------------ *
 * Outcome -> prose. Map the bounded daemon outcome into one honest line for   *
 * the HUD status. Mirrors the daemon trigger vocabulary: an honest success,   *
 * a closed-gate / offline / no-key no-op (`unavailable`), and a generic       *
 * failure. NEVER claims a creation that didn't happen.                        *
 * ------------------------------------------------------------------------ */

/** The honest result vocabulary surfaced after a Voice-Lab attempt. The daemon
 *  returns prose, so the bridge classifies it into this small set: a genuine
 *  creation (`created`), a closed-gate/offline/no-key no-op (`unavailable`), a
 *  generic failure (`failed`), and a shell-unavailable case (`no_shell`). The HUD
 *  shows the matching prose — it NEVER claims a creation when the daemon reported
 *  a no-op. */
export type VoiceLabOutcome = "created" | "unavailable" | "failed" | "no_shell";

/**
 * Classify the daemon's prose reply (already secret-free) into a [`VoiceLabOutcome`].
 * The trigger returns an honest line either way: on a closed gate / no key /
 * offline it begins with the "needs the cloud tier" / "without an ElevenLabs key"
 * shape; a generation/persist failure says "didn't go through"; otherwise (an
 * `ok:true` reply) it is a genuine creation. PURE — unit-tested without any socket.
 *
 * `ok` is the daemon `{ok}` bool; `line` is the prose reply (success) OR the error
 * line (failure). NO path or secret is ever read — only the contracted vocabulary.
 */
export function classifyVoiceLabReply(ok: boolean, line: string): VoiceLabOutcome {
  if (ok) return "created";
  const lower = line.toLowerCase();
  // The trigger's gate-closed / offline / no-key copy. These are honest no-ops:
  // nothing was created. Mirrors the daemon's `trigger_design_voice` /
  // `trigger_create_pronunciation` Err prose.
  if (
    lower.includes("needs the cloud tier") ||
    lower.includes("working offline") ||
    lower.includes("without an elevenlabs key") ||
    lower.includes("add one in settings")
  ) {
    return "unavailable";
  }
  return "failed";
}

/** Human prose for a design-voice outcome, scoped to the agent (never an id/secret).
 *  PURE — the panel may render the daemon's own line, falling back to this. */
export function designVoiceOutcomeCopy(outcome: VoiceLabOutcome, agent: string): string {
  const a = agent.trim() || "the agent";
  switch (outcome) {
    case "created":
      return `Designed and saved a voice for ${a}.`;
    case "unavailable":
      return `Couldn't design the voice — the cloud tier is off, there's no key, or you're offline. Nothing was created.`;
    case "failed":
      return `Couldn't design the voice for ${a} — the cloud request didn't go through. Nothing was created.`;
    case "no_shell":
      return "Designing a voice requires the DARWIN desktop app.";
  }
}

/** Human prose for a create-pronunciation outcome, scoped to the rule (never an
 *  id/secret). PURE. */
export function pronunciationOutcomeCopy(
  outcome: VoiceLabOutcome,
  word: string,
  say: string,
): string {
  const w = word.trim() || "the word";
  const s = say.trim() || "the alias";
  switch (outcome) {
    case "created":
      return `Added the pronunciation: say “${w}” as “${s}”.`;
    case "unavailable":
      return `Couldn't add the pronunciation — the cloud tier is off, there's no key, or you're offline. Nothing was created.`;
    case "failed":
      return `Couldn't add the pronunciation for “${w}” — the cloud request didn't go through. Nothing was created.`;
    case "no_shell":
      return "Adding a pronunciation requires the DARWIN desktop app.";
  }
}

/**
 * COMPOSE MUSIC — the PURE request-shaping + gate + outcome logic behind the
 * HUD's ElevenLabs Music control (Phase-3):
 *
 *   - COMPOSE MUSIC — generate a FULL music track from a text PROMPT (the daemon
 *     `compose_music` verb). Only the text prompt (+ an optional length) leaves
 *     the device; the generated track is a cloud generation handled daemon-side.
 *
 * No DOM/React/Tauri imports here so the request shaping, the gate predicate, and
 * the outcome->prose mapping are verifiable headlessly under vitest (node env),
 * exactly like voiceLab.ts / sfxCue.ts.
 *
 * HONESTY CONTRACT (do not regress):
 *   - This control adds NO authority and NO new tier. A request only ever does
 *     anything when the daemon's already-shipped gate is open: a non-Local
 *     (cloud) model tier AND an ElevenLabs key on file, with `[voice].cloud_music`
 *     ON. This module's `musicGateOpen` mirrors the HUD-knowable half of that
 *     predicate (`voice.cloud_music` switch ON + a key present) for the COPY + the
 *     enabled/disabled state only — the REAL gate still lives daemon-side (the
 *     non-Local tier + the Keychain key); the HUD never fabricates a track.
 *   - With the switch off / no key, the affordance is an honest, disabled control
 *     with copy that says nothing will be created — never a faked "composed"
 *     state. The daemon ALSO re-checks the gate and returns honest "unavailable" /
 *     "didn't go through, nothing was created" prose when it is closed (e.g.
 *     offline); we surface that verbatim, never a fake success.
 *   - SECRET-FREE. This module sees only booleans (switch on, key present) and the
 *     free-text request fields (the prompt, an optional length) — never the key
 *     value, never the produced audio path.
 *
 * The validation floor mirrors daemon/src/command.rs: a non-empty prompt.
 */

/** The free-text clamp — mirrors the daemon `MAX_TEXT_CHARS`. The backend relay
 *  also clamps, but trimming here keeps an oversized field from being shaped into
 *  a request at all. */
export const MAX_TEXT_CHARS = 4 * 1024;

/** Track-length bounds (seconds). The HUD offers an OPTIONAL length control; when
 *  given it is clamped into this band and converted to ms for the wire. The
 *  default of `DEFAULT_LENGTH_SECONDS` is what the control pre-fills; a BLANK
 *  length omits the field entirely (the daemon picks a sensible default). These
 *  mirror the daemon's bounded `length_ms`. */
export const MIN_LENGTH_SECONDS = 3;
export const MAX_LENGTH_SECONDS = 600;
export const DEFAULT_LENGTH_SECONDS = 30;

/* ------------------------------------------------------------------------ *
 * Gate mirror — the HONEST copy + enabled/disabled half. The REAL gate is    *
 * daemon-side (a non-Local model tier AND a Keychain ElevenLabs key AND      *
 * `[voice].cloud_music`). This predicate decides only whether the HUD's      *
 * control is ENABLED and what the explanatory copy says; it never itself     *
 * creates anything.                                                          *
 * ------------------------------------------------------------------------ */

/** The two secret-free inputs the HUD knows locally: whether the
 *  `voice.cloud_music` switch is ON, and whether an ElevenLabs key is on file
 *  (presence only — never the value). Mirrors the switch+key half of the daemon
 *  gate (a non-Local tier + the resolved key). */
export interface MusicGateInputs {
  /** `[voice].cloud_music` — the cloud music-generation switch (ships ON). */
  cloudMusicOn: boolean;
  /** An ElevenLabs key is on file in the Keychain (presence only). */
  keyPresent: boolean;
}

/**
 * The switch+key half of the daemon gate. True only when the cloud-music switch
 * is ON **and** a key is present. The daemon ALSO requires a non-Local (online)
 * tier at call time; the HUD can't always know that locally, so a "gate open"
 * here means "the HUD won't block the attempt" — the daemon still makes the
 * final, honest call and returns an unavailable/failed prose if it's offline.
 * PURE.
 */
export function musicGateOpen(g: MusicGateInputs): boolean {
  return g.cloudMusicOn && g.keyPresent;
}

/** Why the Compose-music control is disabled (or null when enabled). Drives the
 *  honest copy: the operator is told EXACTLY what's missing so the control is
 *  never a mystery and never pretends it will create a track. PURE. */
export type MusicDisabledReason =
  | "no_shell" // running in a plain browser, not the desktop app
  | "music_off" // [voice].cloud_music is OFF
  | "no_key" // no ElevenLabs key on file
  | "music_off_and_no_key"; // both missing

/** Resolve the disabled reason (or null = enabled) from the shell + gate inputs.
 *  `no_shell` takes precedence (nothing can be created without the daemon);
 *  otherwise the missing gate pieces are reported precisely. PURE. */
export function musicDisabledReason(
  inTauri: boolean,
  g: MusicGateInputs,
): MusicDisabledReason | null {
  if (!inTauri) return "no_shell";
  if (!g.cloudMusicOn && !g.keyPresent) return "music_off_and_no_key";
  if (!g.cloudMusicOn) return "music_off";
  if (!g.keyPresent) return "no_key";
  return null;
}

/** Honest, secret-free explanatory copy for the current gate state. When enabled
 *  it states the on-attempt caveat (an offline/Local tier still fails honestly,
 *  creating nothing); when disabled it names exactly what to turn on. NEVER claims
 *  anything was created. PURE. */
export function musicGateCopy(reason: MusicDisabledReason | null): string {
  switch (reason) {
    case null:
      return (
        "Ready. Composing music calls ElevenLabs once and generates a full track " +
        "from your prompt. If you're offline (Local tier) nothing is created — it " +
        "never fabricates a track."
      );
    case "no_shell":
      return "Composing music requires the DARWIN desktop app.";
    case "music_off":
      return "Cloud music is off. Turn on cloud music ([voice].cloud_music) to compose a track.";
    case "no_key":
      return "Add an ElevenLabs key (elevenlabs_api_key) to compose music.";
    case "music_off_and_no_key":
      return (
        "Composing music needs cloud music ([voice].cloud_music) ON and an " +
        "ElevenLabs key on file. With either missing, nothing is created."
      );
  }
}

/* ------------------------------------------------------------------------ *
 * Compose-music request shaping. PURE: it validates the prompt against the   *
 * daemon's floor, clamps the optional length into the bounded ms band, and   *
 * packages them; it injects no token and performs no I/O.                    *
 * ------------------------------------------------------------------------ */

/** The bounded request to compose music. Carries ONLY the text prompt and an
 *  OPTIONAL track length in milliseconds — never a key, never a path. `lengthMs`
 *  is omitted entirely when the length field is blank (the daemon defaults it),
 *  so the wire shape stays minimal. */
export interface ComposeMusicRequest {
  prompt: string;
  lengthMs?: number;
}

/** Why a compose request could not be shaped (or null when it's valid). Mirrors
 *  the daemon rejection so the HUD can show a precise inline error BEFORE any
 *  round-trip. PURE. */
export type ComposeMusicError = "no_prompt";

/** Clamp a free-text field to [`MAX_TEXT_CHARS`] (char-boundary safe), the same
 *  clamp the daemon + the backend relay apply. PURE. */
function clampText(s: string): string {
  if (s.length <= MAX_TEXT_CHARS) return s;
  return Array.from(s).slice(0, MAX_TEXT_CHARS).join("");
}

/** Clamp a raw seconds value into the bounded [MIN, MAX] band and convert to ms.
 *  A non-finite/<=0 value is treated as out of band and clamped to the floor.
 *  PURE. */
function clampLengthMs(seconds: number): number {
  let s = seconds;
  if (!Number.isFinite(s)) s = MIN_LENGTH_SECONDS;
  s = Math.round(s);
  if (s < MIN_LENGTH_SECONDS) s = MIN_LENGTH_SECONDS;
  if (s > MAX_LENGTH_SECONDS) s = MAX_LENGTH_SECONDS;
  return s * 1000;
}

/**
 * Shape a compose-music request from the raw form fields. Returns a structured
 * error (never throws) when the prompt is empty — the SAME check the daemon
 * makes, so a blank request never spends a cloud op. `lengthRaw` is the OPTIONAL
 * length control's value: a blank/undefined value OMITS `lengthMs` (the daemon
 * defaults it); a numeric value is clamped into [MIN, MAX] seconds and converted
 * to ms. PURE.
 */
export function buildComposeMusicRequest(
  promptRaw: string,
  lengthRaw?: string | number | null,
):
  | { ok: true; request: ComposeMusicRequest }
  | { ok: false; error: ComposeMusicError } {
  const prompt = clampText(promptRaw.trim());
  if (prompt.length === 0) {
    return { ok: false, error: "no_prompt" };
  }
  // The length field is OPTIONAL: a blank string / null / undefined omits it.
  const trimmed =
    typeof lengthRaw === "string" ? lengthRaw.trim() : lengthRaw;
  const hasLength =
    trimmed !== undefined &&
    trimmed !== null &&
    !(typeof trimmed === "string" && trimmed === "");
  if (!hasLength) {
    return { ok: true, request: { prompt } };
  }
  const seconds = typeof trimmed === "number" ? trimmed : Number(trimmed);
  if (!Number.isFinite(seconds)) {
    // A non-numeric length is treated as "unset" rather than an error — the
    // prompt is the only required field; the daemon defaults the length.
    return { ok: true, request: { prompt } };
  }
  return { ok: true, request: { prompt, lengthMs: clampLengthMs(seconds) } };
}

/** Inline copy for a compose-request validation error. PURE. */
export function composeMusicErrorCopy(error: ComposeMusicError): string {
  switch (error) {
    case "no_prompt":
      return "Describe the music to compose (e.g. an 8-bit happy birthday).";
  }
}

/* ------------------------------------------------------------------------ *
 * Outcome -> prose. Map the bounded daemon outcome into one honest line for   *
 * the HUD status. Mirrors the daemon trigger vocabulary: an honest creation,  *
 * a closed-gate / offline / no-key no-op (`unavailable`), and a generic       *
 * failure. NEVER claims a track that didn't happen.                           *
 * ------------------------------------------------------------------------ */

/** The honest result vocabulary surfaced after a Compose-music attempt. The
 *  daemon returns prose, so the bridge classifies it into this small set: a
 *  genuine creation (`created`), a closed-gate/offline/no-key no-op
 *  (`unavailable`), a generic failure (`failed`), and a shell-unavailable case
 *  (`no_shell`). The HUD shows the matching prose — it NEVER claims a track when
 *  the daemon reported a no-op. */
export type MusicOutcome = "created" | "unavailable" | "failed" | "no_shell";

/**
 * Classify the daemon's prose reply (already secret-free) into a [`MusicOutcome`].
 * FAIL-SAFE form, exactly like classifyVoiceLabReply: the command channel returns
 * `ok:true` for any DISPATCHED verb and carries the trigger's honest outcome PROSE
 * in the reply — a closed gate / failure is reported in that TEXT, NOT via the
 * bare `ok` flag. So we classify on the secret-free line and call it "created"
 * ONLY when the prose carries no gate / failure marker: an honest no-op or failure
 * can NEVER read as "created", even on an `ok:true` dispatch. PURE — unit-tested
 * without any socket.
 *
 * `ok` is the daemon `{ok}` bool; `line` is the prose reply (success) OR the error
 * line (failure). NO path or secret is ever read — only the contracted vocabulary.
 */
export function classifyMusicReply(ok: boolean, line: string): MusicOutcome {
  const lower = line.toLowerCase();
  // A closed-gate no-op: cloud music off / offline / no key. The daemon's own
  // honest copy names these; mirror its markers so a no-op never reads "created".
  if (
    lower.includes("needs the cloud tier") ||
    lower.includes("needs cloud music") ||
    lower.includes("working offline") ||
    lower.includes("without an elevenlabs key") ||
    lower.includes("add one in settings") ||
    lower.includes("unavailable")
  ) {
    return "unavailable";
  }
  // Any honest failure marker -> failed, even on an ok:true dispatch.
  if (
    !ok ||
    lower.includes("couldn't") ||
    lower.includes("could not") ||
    lower.includes("can't") ||
    lower.includes("didn't go through") ||
    lower.includes("nothing was created")
  ) {
    return "failed";
  }
  // A genuine creation: dispatched ok with clean success prose.
  return "created";
}

/** Human prose for a compose-music outcome, scoped to the prompt (never a path/
 *  secret). PURE — the panel may render the daemon's own line, falling back to
 *  this. */
export function composeMusicOutcomeCopy(outcome: MusicOutcome, prompt: string): string {
  const p = prompt.trim() || "your prompt";
  switch (outcome) {
    case "created":
      return `Composed a track from “${p}”.`;
    case "unavailable":
      return `Couldn't compose the track — cloud music is off, there's no key, or you're offline. Nothing was created.`;
    case "failed":
      return `Couldn't compose the track from “${p}” — the cloud request didn't go through. Nothing was created.`;
    case "no_shell":
      return "Composing music requires the DARWIN desktop app.";
  }
}

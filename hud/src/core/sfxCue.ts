/**
 * NAMED SFX CUE CATALOG — the HUD-side mirror of the daemon's code-level cue
 * palette (daemon/src/sfx_cue.rs `CATALOG`) plus the PURE logic the trigger
 * affordance needs: the read-only cue list, the play-request shaping, and the
 * HONEST gate/outcome copy. No DOM/React/Tauri imports here so the catalog, the
 * gate predicate, and the outcome→prose mapping are verifiable headlessly under
 * vitest (node env), exactly like deck.ts / events.ts.
 *
 * HONESTY CONTRACT (do not regress):
 *   - The cue layer adds NO authority and NO new tier. A cue only ever plays when
 *     the daemon's already-shipped gate is open: `[voice].cloud_sfx` ON **and** an
 *     ElevenLabs key on file (and the daemon online). This module's `cueGateOpen`
 *     mirrors that predicate (`voice_tier::sfx_enabled`) for the COPY only — the
 *     real gate still lives daemon-side; the HUD never fabricates a cue.
 *   - With the switch off / no key, the affordance is an honest, disabled control
 *     with copy that says nothing will play — never a faked "playing" state.
 *   - SECRET-FREE. This module sees only booleans (switch on, key present) and the
 *     cue NAME — never the key value, never the produced WAV path.
 *
 * The names + order MUST stay in lockstep with daemon/src/sfx_cue.rs CATALOG;
 * `CUE_CATALOG` is the single source the panel and the tests both read.
 */

/** One catalog entry: the stable lookup NAME (the daemon plays by this) plus a
 *  short, human label/blurb for the read-only HUD list. The blurb is descriptive
 *  copy only — the actual EL sound-generation prompt lives daemon-side and never
 *  reaches the HUD. */
export interface CueEntry {
  /** The stable lookup name (lowercase, no spaces) — matches the daemon catalog. */
  name: string;
  /** A short human label for the row (Title-cased name is fine, but explicit). */
  label: string;
  /** A one-line description of the cue's character (no secret, no prompt). */
  blurb: string;
}

/**
 * The BUILT-IN cue catalog — names + order mirror daemon/src/sfx_cue.rs `CATALOG`
 * (confirm / alert / error / success / notify / wake). These are code-level
 * product defaults, not user config; the HUD shows them READ-ONLY (you can play a
 * built-in cue, you cannot add/rename one here). The blurbs paraphrase the daemon
 * prompts so the operator knows what each cue sounds like — they are NOT the
 * prompts and carry no secret.
 */
export const CUE_CATALOG: readonly CueEntry[] = [
  {
    name: "confirm",
    label: "Confirm",
    blurb: "Soft single confirmation chime — clean, reassuring, quick decay.",
  },
  {
    name: "alert",
    label: "Alert",
    blurb: "Urgent two-tone alert — two quick high-low beeps, attention-grabbing.",
  },
  {
    name: "error",
    label: "Error",
    blurb: "Short low error buzz — flat, negative, quick stop.",
  },
  {
    name: "success",
    label: "Success",
    blurb: "Bright ascending success tone — a clean rising three-note arpeggio.",
  },
  {
    name: "notify",
    label: "Notify",
    blurb: "Gentle notification ping — soft, rounded, unobtrusive.",
  },
  {
    name: "wake",
    label: "Wake",
    blurb: "Subtle power-up swell — a rising whoosh into a warm pad, HUD coming online.",
  },
] as const;

/** The catalog's cue names, in catalog order — used to enumerate what can be
 *  played and to pin the palette in the tests. PURE. */
export function cueNames(): string[] {
  return CUE_CATALOG.map((c) => c.name);
}

/** Whether `name` is a known built-in cue (case-insensitive, trimmed). An unknown
 *  name is NEVER fabricated — the play control only offers catalog names, and a
 *  request for an unknown name is rejected before any shaping. PURE. */
export function isKnownCue(name: string): boolean {
  const want = name.trim().toLowerCase();
  return CUE_CATALOG.some((c) => c.name === want);
}

/* ------------------------------------------------------------------------ *
 * Gate mirror — the HONEST copy half. The REAL gate is daemon-side          *
 * (voice_tier::sfx_enabled = cloud_sfx && key, AND a non-Local tier). This  *
 * predicate decides only whether the HUD's play control is ENABLED and what *
 * the explanatory copy says; it never itself plays or fabricates a cue.     *
 * ------------------------------------------------------------------------ */

/** The two secret-free inputs the HUD knows locally: whether the `voice.cloud_sfx`
 *  switch is ON, and whether an ElevenLabs key is on file (presence only — never
 *  the value). Mirrors the switch+key half of `voice_tier::sfx_enabled`. */
export interface CueGateInputs {
  /** `[voice].cloud_sfx` — the SFX cue tier switch (ships OFF). */
  cloudSfxOn: boolean;
  /** An ElevenLabs key is on file in the Keychain (presence only). */
  keyPresent: boolean;
}

/**
 * The switch+key half of the daemon gate (`voice_tier::sfx_enabled`). True only
 * when the cue tier is ON **and** a key is present. The daemon ALSO requires a
 * non-Local (online) tier at play time; the HUD can't always know that locally,
 * so a "gate open" here means "the HUD won't block the attempt" — the daemon
 * still makes the final, honest call and returns a silent no-op if it's offline.
 * PURE.
 */
export function cueGateOpen(g: CueGateInputs): boolean {
  return g.cloudSfxOn && g.keyPresent;
}

/** Why the play control is disabled (or null when it's enabled). Drives the
 *  honest copy: the operator is told EXACTLY what's missing so the control is
 *  never a mystery and never pretends a cue will play. PURE. */
export type CueDisabledReason =
  | "no_shell" // running in a plain browser, not the desktop app
  | "switch_off" // [voice].cloud_sfx is OFF
  | "no_key" // no ElevenLabs key on file
  | "switch_off_and_no_key"; // both missing

/** Resolve the disabled reason (or null = enabled) from the shell + gate inputs.
 *  `no_shell` takes precedence (nothing can play without the daemon); otherwise
 *  the missing gate pieces are reported precisely. PURE. */
export function cueDisabledReason(
  inTauri: boolean,
  g: CueGateInputs,
): CueDisabledReason | null {
  if (!inTauri) return "no_shell";
  if (!g.cloudSfxOn && !g.keyPresent) return "switch_off_and_no_key";
  if (!g.cloudSfxOn) return "switch_off";
  if (!g.keyPresent) return "no_key";
  return null;
}

/** Honest, secret-free explanatory copy for the current gate state. When enabled
 *  it states the on-attempt caveat (a Local/offline tier still no-ops silently);
 *  when disabled it names exactly what to turn on. NEVER claims a cue played.
 *  PURE. */
export function cueGateCopy(reason: CueDisabledReason | null): string {
  switch (reason) {
    case null:
      return (
        "Ready. A cue is generated by ElevenLabs the first time, then cached — " +
        "playing the same cue again costs nothing. If the daemon is offline " +
        "(Local tier) nothing plays; it never fabricates a cue."
      );
    case "no_shell":
      return "Playing cues requires the DARWIN desktop app.";
    case "switch_off":
      return "Cues are off. Turn on cloud SFX ([voice].cloud_sfx) to play them.";
    case "no_key":
      return "Add an ElevenLabs key (elevenlabs_api_key) to play cues.";
    case "switch_off_and_no_key":
      return (
        "Cues need cloud SFX ([voice].cloud_sfx) ON and an ElevenLabs key on " +
        "file. With either missing, nothing plays."
      );
  }
}

/* ------------------------------------------------------------------------ *
 * Play-request shaping — the bounded request the HUD hands the bridge to    *
 * trigger a cue. PURE: it only validates the cue name against the catalog   *
 * and packages the name; it injects no token and performs no I/O.           *
 * ------------------------------------------------------------------------ */

/** The bounded request to play one named cue. Carries ONLY the cue name (a
 *  catalog atom) — never a key, never a prompt, never a path. */
export interface PlayCueRequest {
  cue: string;
}

/** Shape a play request for `name`. Returns null for an unknown name (the play
 *  control only offers catalog names, so this is defense-in-depth — an unknown
 *  name is never fabricated into a request). The name is normalized to the
 *  catalog atom (trimmed, lowercased) so the daemon lookup is exact. PURE. */
export function buildPlayCueRequest(name: string): PlayCueRequest | null {
  const want = name.trim().toLowerCase();
  if (!isKnownCue(want)) return null;
  return { cue: want };
}

/* ------------------------------------------------------------------------ *
 * Outcome → prose — map the daemon's play_cue reply into one honest line     *
 * for the HUD log/toast. Mirrors the daemon PlayOutcome vocabulary.          *
 * ------------------------------------------------------------------------ */

/** The honest result vocabulary surfaced after a play attempt. Mirrors the
 *  daemon's `PlayOutcome` (played / cached / disabled / unknown / failed) plus a
 *  shell-unavailable case. The HUD shows the matching prose — it NEVER claims a
 *  cue played when the daemon reported a no-op. */
export type CuePlayOutcome =
  | "played"
  | "cached"
  | "disabled"
  | "unknown"
  | "failed"
  | "no_shell";

/** Human prose for a play outcome, scoped to the cue NAME (never a path/secret).
 *  PURE — the panel renders the returned line verbatim. */
export function cueOutcomeCopy(outcome: CuePlayOutcome, cue: string): string {
  const c = cue.trim() || "cue";
  switch (outcome) {
    case "played":
      return `Played the “${c}” cue (generated and cached).`;
    case "cached":
      return `Played the “${c}” cue (from cache — no cloud call).`;
    case "disabled":
      return `“${c}” did not play — cues are off or no key/offline. Nothing was produced.`;
    case "unknown":
      return `There's no built-in cue called “${c}”.`;
    case "failed":
      return `Couldn't play “${c}” — the cloud sound didn't go through. Nothing was produced.`;
    case "no_shell":
      return "Playing cues requires the DARWIN desktop app.";
  }
}

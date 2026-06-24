import { useCallback, useEffect, useState } from "react";
import type { AudioIoStatus } from "../core/events";
import {
  interpretLabel,
  interpretDirection,
  interpretTone,
  diarizationLabel,
  diarizationTone,
  diarizationDetail,
} from "../core/events";
import {
  CUE_CATALOG,
  buildPlayCueRequest,
  cueDisabledReason,
  cueGateCopy,
  cueOutcomeCopy,
  type CuePlayOutcome,
} from "../core/sfxCue";
import {
  buildDesignVoiceRequest,
  buildPronunciationRequest,
  designVoiceErrorCopy,
  designVoiceOutcomeCopy,
  pronunciationErrorCopy,
  pronunciationOutcomeCopy,
  voiceLabDisabledReason,
  voiceLabGateCopy,
} from "../core/voiceLab";
import {
  DEFAULT_LENGTH_SECONDS,
  MAX_LENGTH_SECONDS,
  MIN_LENGTH_SECONDS,
  buildComposeMusicRequest,
  composeMusicErrorCopy,
  composeMusicOutcomeCopy,
  musicDisabledReason,
  musicGateCopy,
} from "../core/music";
import { ROSTER } from "../core/agents";
import {
  composeMusic,
  createPronunciation,
  designVoice,
  inTauri,
  keychainStatus,
  playSfxCue,
} from "../tauri/bridge";
import { configGet } from "../tauri/configSettings";
import Frame from "./Frame";

/**
 * AUDIO // I/O — the read-only indicator for the three audio-INPUT features, all
 * shipping OFF / neutral and surfaced HONESTLY here. It reads only the secret-free
 * `audioIo` status the reducer folds from daemon telemetry — never the transcript
 * text, a translation, a fabricated speaker, or the captured wav path.
 *
 *   #30 LIVE INTERPRET — source→target language + render-only vs spoken + the
 *       count of REAL translations rendered. ACTIVE only once the DEVICE-GATED mic
 *       loop has fed a segment (`interpret.segment_fed`); a real translation
 *       (`interpret.segment` with translated:true) bumps the count. The copy is
 *       explicit that the continuous always-listening live loop is DEVICE-GATED
 *       (mic) and that an offline/unavailable segment DEGRADES honestly — never a
 *       fabricated translation.
 *   #31 DIARIZATION — speaker labelling is ElevenLabs-Scribe-ONLY. The badge reads
 *       the GROUND-TRUTH `backendCanDiarize` bit: ON-DEVICE: NO DIARIZATION (whisper
 *       has no diarization model — a single honest stream, never a fabricated
 *       speaker), SINGLE STREAM (EL Scribe, one speaker), or MULTI-SPEAKER (EL
 *       Scribe reported >1 distinct speaker — the backend's labels, never invented).
 *   #32 WAKE WORD — the ACTIVE configured wake phrase (default "jarvis"). The copy
 *       is honest that the always-listening loop that consults the matcher is
 *       DEVICE-GATED (mic); the matcher itself is conservative + pure.
 *
 * HONESTY CONTRACT (do not regress):
 *   - DEVICE-GATED. Live interpretation + the always-listening / wake loop run on
 *     the MIC and are device-gated; this panel only reflects what the daemon
 *     reported it did, never claims a live mic capture happened headlessly.
 *   - NEVER FABRICATED. No fabricated speaker, no fabricated translation, no
 *     fabricated wake trigger reaches this surface — diarization honesty is the
 *     backend's own `backend_can_diarize`, a translation count is only the real
 *     translated:true frames, and the wake phrase is the configured one.
 *   - SHIPS OFF / NEUTRAL. All three flags ship OFF; before any telemetry the
 *     surface rests in the honest INTERPRET OFF / NOT SEEN / default-"jarvis"
 *     state and the panel still renders that resting posture (it is informative —
 *     it names the configured wake word and the diarization posture).
 *   - SECRET-FREE. Only languages / booleans / counts / the wake phrase — never
 *     the transcript text, the translation, or the wav path.
 */
export default function AudioIoPanel({
  audio,
}: {
  audio: AudioIoStatus;
}) {
  const i = audio.interpret;
  const d = audio.diarization;
  const w = audio.wake;

  const iTone = interpretTone(i);
  const dTone = diarizationTone(d);

  return (
    <div className="audioio-panel">
      <Frame title="AUDIO // I/O" tag="HONEST · READ ONLY">
        <div className="verify-body">
          {/* #30 LIVE INTERPRET ----------------------------------------- */}
          <div className="verify-row">
            <span className="verify-head">INTERPRET</span>
            <span
              className={`verify-pill audioio-${iTone}`}
              title={
                "Continuous live interpretation (#30). The always-listening live loop " +
                "is DEVICE-GATED (mic) — this only reflects what the daemon reported. " +
                "An offline/unavailable segment DEGRADES honestly; a fabricated " +
                "translation is never produced. Ships OFF ([interpret].live)."
              }
            >
              <span className={`dot ${iTone}`} />
              {interpretLabel(i)}
            </span>
            <span className="verify-meaning">
              {i.active
                ? `${interpretDirection(i)} · ${i.spoke ? "spoken" : "render-only"}`
                : "off — device-gated mic loop not running"}
            </span>
          </div>
          {i.active && (
            <div className="audioio-sub dim-note">
              {i.translations > 0
                ? `${i.translations} real translation${i.translations === 1 ? "" : "s"} rendered`
                : "0 real translations yet (a degrade renders nothing — never a fake translation)"}
            </div>
          )}

          {/* #31 DIARIZATION -------------------------------------------- */}
          <div className="verify-row">
            <span className="verify-head">DIARIZATION</span>
            <span
              className={`verify-pill audioio-${dTone}`}
              title={diarizationDetail(d)}
            >
              <span className={`dot ${dTone}`} />
              {diarizationLabel(d)}
            </span>
            <span className="verify-meaning">
              {!d.seen
                ? "ElevenLabs-Scribe-only ([voice].diarize ships OFF)"
                : d.backendCanDiarize
                  ? `${d.turns} turn${d.turns === 1 ? "" : "s"}`
                  : "single honest stream — no on-device diarization model"}
            </span>
          </div>

          {/* #32 WAKE WORD ---------------------------------------------- */}
          <div className="verify-row">
            <span className="verify-head">WAKE WORD</span>
            <span
              className="verify-pill audioio-good"
              title={
                'The configured wake phrase that gates "is this for JARVIS" (#32). ' +
                'Defaults to "jarvis", preserving today\'s behavior. The matcher is ' +
                "conservative + pure; the always-listening loop that consults it is " +
                "DEVICE-GATED (mic). Ships OFF ([wake].enabled)."
              }
            >
              <span className="dot good" />“{w.phrase}”
            </span>
            <span className="verify-meaning">
              {w.lastDropped
                ? "active — an utterance was dropped for lacking the wake word"
                : "active wake phrase (default preserves today's behavior)"}
            </span>
          </div>

          {/* SFX CUES — read-only catalog + honest per-cue test triggers ---- */}
          <SfxCueTrigger />

          {/* VOICE LAB — design a voice + add a pronunciation (key + tier gated) */}
          <VoiceLab />

          {/* COMPOSE MUSIC — generate a full track from a prompt (key + music gated) */}
          <ComposeMusic />

          <div className="verify-foot dim-note">
            All three audio-input features ship <b>OFF / neutral</b>. Live
            interpretation and the always-listening wake loop are{" "}
            <b>DEVICE-GATED (mic)</b> — this panel reflects only what the daemon
            reported, never a fabricated translation or wake trigger. Diarization is{" "}
            <b>ElevenLabs-Scribe-only</b>: on-device whisper has no diarization model
            and reads as a single honest stream — <b>never a fabricated speaker</b>.
            Secret-free — only languages, counts, and the wake phrase; never the
            transcript, the translation, or the audio.
          </div>
        </div>
      </Frame>
    </div>
  );
}

/**
 * SFX CUES (Phase-2) — a small, HONEST way to TEST the built-in sound-effect
 * cues from the HUD. It surfaces the READ-ONLY cue catalog (the daemon's
 * code-level palette) and a per-cue "Play" button that sends the `play_sfx_cue`
 * command through the existing token-injecting command seam.
 *
 * HONESTY CONTRACT (do not regress):
 *   - Cues require the ElevenLabs key + cloud SFX ON. The buttons are DISABLED
 *     (with copy that names exactly what's missing) until the gate is open; with
 *     the switch off / no key / offline, NOTHING plays — never a fabricated cue.
 *   - The control adds NO authority and NO new tier. The daemon makes the real,
 *     gated call (`voice_tier::sfx_enabled`) and returns an honest silent no-op
 *     when closed; this surface only reflects that outcome, never claims a cue
 *     played when it didn't.
 *   - SECRET-FREE. It reads only two booleans (switch on, key present) and sends
 *     the cue NAME — never the key value, never the produced WAV path.
 *
 * The gate inputs are probed ONCE on mount (config `voice.cloud_sfx` + key
 * presence); the pure catalog/gate/outcome logic lives in core/sfxCue.ts and is
 * unit-tested headlessly.
 */
function SfxCueTrigger() {
  const shell = inTauri();
  const [cloudSfxOn, setCloudSfxOn] = useState(false);
  const [keyPresent, setKeyPresent] = useState(false);
  const [playing, setPlaying] = useState<string | null>(null);
  const [note, setNote] = useState("");

  // Probe the secret-free gate inputs once: the cloud-SFX switch (from config)
  // and whether an ElevenLabs key is on file (presence only — never the value).
  // In a plain browser both stay false and the controls render disabled.
  useEffect(() => {
    if (!shell) return;
    let live = true;
    void (async () => {
      try {
        const settings = await configGet();
        const sfx = settings.find((s) => s.key === "voice.cloud_sfx");
        if (live) setCloudSfxOn(sfx?.value === true);
      } catch {
        if (live) setCloudSfxOn(false);
      }
    })();
    void keychainStatus("elevenlabs_api_key")
      .then((present) => {
        if (live) setKeyPresent(present);
      })
      .catch(() => {
        if (live) setKeyPresent(false);
      });
    return () => {
      live = false;
    };
  }, [shell]);

  const reason = cueDisabledReason(shell, { cloudSfxOn, keyPresent });
  const gateOpen = reason === null;

  const play = useCallback(
    async (name: string) => {
      if (playing) return;
      const req = buildPlayCueRequest(name);
      if (!req) return; // unknown name — never fabricated (defense-in-depth)
      setPlaying(name);
      setNote("");
      try {
        const r = await playSfxCue(req.cue);
        // Map the bounded daemon outcome to honest prose (never claims a play
        // that didn't happen). `detail` is the daemon's own secret-free line; we
        // prefer it, falling back to the contract copy.
        setNote(r.detail || cueOutcomeCopy(r.outcome as CuePlayOutcome, req.cue));
      } catch {
        setNote(cueOutcomeCopy("failed", req.cue));
      } finally {
        setPlaying(null);
      }
    },
    [playing],
  );

  return (
    <div className="audioio-sfx" aria-label="Sound-effect cues">
      <div className="verify-row">
        <span className="verify-head">SFX CUES</span>
        <span
          className={`verify-pill audioio-${gateOpen ? "good" : "idle"}`}
          title={
            "Built-in sound-effect cues you can test from here. They require the " +
            "ElevenLabs key + cloud SFX ([voice].cloud_sfx) ON; with either off (or " +
            "offline) nothing plays — never a fabricated cue. The cue NAME is the " +
            "only thing sent; no key value or audio path crosses this surface."
          }
        >
          <span className={`dot ${gateOpen ? "good" : "idle"}`} />
          {gateOpen ? "READY" : "OFF"}
        </span>
        <span className="verify-meaning">{cueGateCopy(reason)}</span>
      </div>

      <ul className="audioio-cue-list">
        {CUE_CATALOG.map((cue) => (
          <li className="audioio-cue-row" key={cue.name}>
            <button
              type="button"
              className="icon-btn audioio-cue-play"
              onClick={() => void play(cue.name)}
              disabled={!gateOpen || playing !== null}
              title={
                gateOpen
                  ? `Play the “${cue.label}” cue — ${cue.blurb}`
                  : `“${cue.label}” — ${cueGateCopy(reason)}`
              }
            >
              {playing === cue.name ? "Playing…" : "Play"}
            </button>
            <span className="audioio-cue-name">{cue.label}</span>
            <span className="audioio-cue-blurb dim-note">{cue.blurb}</span>
          </li>
        ))}
      </ul>

      <div className="audioio-sub dim-note" role="status">
        {note ||
          "Built-in cues (read-only): " +
            CUE_CATALOG.map((c) => c.name).join(", ") +
            ". They generate once via ElevenLabs, then play from cache. " +
            "Cues require the key + cloud SFX on — else nothing plays."}
      </div>
    </div>
  );
}

/**
 * VOICE LAB (Phase-2) — two HONEST ElevenLabs provisioning controls from the HUD:
 *
 *   - DESIGN A VOICE — design a voice for an agent from a text DESCRIPTION
 *     (the `design_voice` command). Only the text description leaves the device;
 *     the returned voice id is stored daemon-side.
 *   - ADD A PRONUNCIATION — mint a single alias rule (word -> say, the
 *     `create_pronunciation` command). Text rules only; no audio leaves the device.
 *
 * Both ride the SAME token-injecting command seam every verb uses — they add NO
 * authority and NO new tier.
 *
 * HONESTY CONTRACT (do not regress):
 *   - The Voice Lab requires the ElevenLabs key + the cloud tier. The forms are
 *     DISABLED (with copy that names exactly what's missing) until the gate is
 *     open; with the tier off / no key / offline, NOTHING is created — never a
 *     fabricated voice or dictionary.
 *   - The daemon makes the real, gated call and returns an honest `Err` when the
 *     gate is closed; this surface only reflects that outcome, never claims a
 *     creation when it didn't happen.
 *   - SECRET-FREE. It reads only two booleans (cloud-tier switch on, key present)
 *     and sends the free-text request fields — never the key value, never the
 *     returned voice/dictionary id.
 *
 * The gate inputs are probed ONCE on mount (config `voice.cloud_tier` + key
 * presence); the pure request-shaping / gate / outcome logic lives in
 * core/voiceLab.ts and is unit-tested headlessly.
 */
function VoiceLab() {
  const shell = inTauri();
  const [cloudTierOn, setCloudTierOn] = useState(false);
  const [keyPresent, setKeyPresent] = useState(false);

  // Probe the secret-free gate inputs once: the cloud-tier switch (from config)
  // and whether an ElevenLabs key is on file (presence only — never the value).
  // In a plain browser both stay false and the forms render disabled.
  useEffect(() => {
    if (!shell) return;
    let live = true;
    void (async () => {
      try {
        const settings = await configGet();
        const tier = settings.find((s) => s.key === "voice.cloud_tier");
        if (live) setCloudTierOn(tier?.value === true);
      } catch {
        if (live) setCloudTierOn(false);
      }
    })();
    void keychainStatus("elevenlabs_api_key")
      .then((present) => {
        if (live) setKeyPresent(present);
      })
      .catch(() => {
        if (live) setKeyPresent(false);
      });
    return () => {
      live = false;
    };
  }, [shell]);

  const reason = voiceLabDisabledReason(shell, { cloudTierOn, keyPresent });
  const gateOpen = reason === null;

  return (
    <div className="audioio-voicelab" aria-label="Voice Lab">
      <div className="verify-row">
        <span className="verify-head">VOICE LAB</span>
        <span
          className={`verify-pill audioio-${gateOpen ? "good" : "idle"}`}
          title={
            "Design an ElevenLabs voice for an agent, or add a pronunciation rule. " +
            "Both require the ElevenLabs key + cloud TTS ([voice].cloud_tier) ON; " +
            "with either off (or offline) nothing is created — never a fabricated " +
            "voice or dictionary. Only the text you enter leaves the device; no key " +
            "value or returned id crosses this surface."
          }
        >
          <span className={`dot ${gateOpen ? "good" : "idle"}`} />
          {gateOpen ? "READY" : "OFF"}
        </span>
        <span className="verify-meaning">{voiceLabGateCopy(reason)}</span>
      </div>

      <DesignVoiceForm gateOpen={gateOpen} reason={reason} />
      <AddPronunciationForm gateOpen={gateOpen} reason={reason} />

      <div className="audioio-sub dim-note">
        Designing a voice or adding a pronunciation calls ElevenLabs once and
        stores the result on this device. Requires the key + cloud tier — else
        nothing is created.
      </div>
    </div>
  );
}

/** DESIGN A VOICE — pick an agent + describe the voice, then Design. The shaping +
 *  validation floors live in core/voiceLab.ts (mirroring the daemon `decide`); a
 *  thin request never leaves the form. The outcome is the daemon's own honest line,
 *  falling back to the contract copy — never a fabricated "designed" state. */
function DesignVoiceForm({
  gateOpen,
  reason,
}: {
  gateOpen: boolean;
  reason: ReturnType<typeof voiceLabDisabledReason>;
}) {
  const [agent, setAgent] = useState(ROSTER[0]?.name ?? "");
  const [description, setDescription] = useState("");
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState("");

  const submit = useCallback(async () => {
    if (busy) return;
    const shaped = buildDesignVoiceRequest(agent, description, "");
    if (!shaped.ok) {
      setNote(designVoiceErrorCopy(shaped.error));
      return;
    }
    setBusy(true);
    setNote("");
    try {
      const r = await designVoice(shaped.request.agent, shaped.request.description);
      // Prefer the daemon's own secret-free line; fall back to the contract copy.
      setNote(r.detail || designVoiceOutcomeCopy(r.outcome, shaped.request.agent));
    } catch {
      setNote(designVoiceOutcomeCopy("failed", shaped.request.agent));
    } finally {
      setBusy(false);
    }
  }, [agent, description, busy]);

  return (
    <div className="audioio-vl-form" aria-label="Design a voice">
      <div className="audioio-vl-title">Design a voice</div>
      <div className="audioio-vl-fields">
        <label className="audioio-vl-label">
          Agent
          <select
            className="audioio-vl-select"
            value={agent}
            onChange={(e) => setAgent(e.target.value)}
            disabled={!gateOpen || busy}
          >
            {ROSTER.map((a) => (
              <option key={a.name} value={a.name}>
                {a.name} — {a.role}
              </option>
            ))}
          </select>
        </label>
        <label className="audioio-vl-label">
          Describe the voice
          <textarea
            className="audioio-vl-textarea"
            value={description}
            onChange={(e) => setDescription(e.target.value)}
            disabled={!gateOpen || busy}
            rows={2}
            placeholder="e.g. a calm, warm British concierge voice, mid-30s, measured pace"
            title={gateOpen ? undefined : voiceLabGateCopy(reason)}
          />
        </label>
        <button
          type="button"
          className="icon-btn audioio-vl-submit"
          onClick={() => void submit()}
          disabled={!gateOpen || busy}
          title={gateOpen ? "Design this voice (calls ElevenLabs once)" : voiceLabGateCopy(reason)}
        >
          {busy ? "Designing…" : "Design"}
        </button>
      </div>
      {note && (
        <div className="audioio-sub dim-note" role="status">
          {note}
        </div>
      )}
    </div>
  );
}

/**
 * COMPOSE MUSIC (Phase-3) — an HONEST way to generate a FULL music track from a
 * text prompt via ElevenLabs, from the HUD. It sends the `compose_music` command
 * through the existing token-injecting command seam.
 *
 * HONESTY CONTRACT (do not regress):
 *   - Composing music requires the ElevenLabs key + cloud music ON. The form is
 *     DISABLED (with copy that names exactly what's missing) until the gate is
 *     open; with the switch off / no key / offline, NOTHING is created — never a
 *     fabricated track.
 *   - The control adds NO authority and NO new tier. The daemon makes the real,
 *     gated call (`[voice].cloud_music` + an EL key + a non-Local tier) and
 *     returns honest "unavailable" / "didn't go through, nothing was created"
 *     prose when closed; this surface only reflects that outcome, never claims a
 *     track was made on a no-op.
 *   - SECRET-FREE. It reads only two booleans (switch on, key present) and sends
 *     the PROMPT (+ an optional length) — never the key value, never the audio path.
 *
 * The gate inputs are probed ONCE on mount (config `voice.cloud_music` + key
 * presence); the pure request-shaping / gate / outcome logic lives in
 * core/music.ts and is unit-tested headlessly.
 */
function ComposeMusic() {
  const shell = inTauri();
  const [cloudMusicOn, setCloudMusicOn] = useState(false);
  const [keyPresent, setKeyPresent] = useState(false);
  const [prompt, setPrompt] = useState("");
  const [length, setLength] = useState(String(DEFAULT_LENGTH_SECONDS));
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState("");

  // Probe the secret-free gate inputs once: the cloud-music switch (from config)
  // and whether an ElevenLabs key is on file (presence only — never the value).
  // In a plain browser both stay false and the form renders disabled.
  useEffect(() => {
    if (!shell) return;
    let live = true;
    void (async () => {
      try {
        const settings = await configGet();
        const music = settings.find((s) => s.key === "voice.cloud_music");
        if (live) setCloudMusicOn(music?.value === true);
      } catch {
        if (live) setCloudMusicOn(false);
      }
    })();
    void keychainStatus("elevenlabs_api_key")
      .then((present) => {
        if (live) setKeyPresent(present);
      })
      .catch(() => {
        if (live) setKeyPresent(false);
      });
    return () => {
      live = false;
    };
  }, [shell]);

  const reason = musicDisabledReason(shell, { cloudMusicOn, keyPresent });
  const gateOpen = reason === null;

  const submit = useCallback(async () => {
    if (busy) return;
    const shaped = buildComposeMusicRequest(prompt, length);
    if (!shaped.ok) {
      setNote(composeMusicErrorCopy(shaped.error));
      return;
    }
    setBusy(true);
    setNote("");
    try {
      const r = await composeMusic(shaped.request.prompt, shaped.request.lengthMs);
      // Prefer the daemon's own secret-free line; fall back to the contract copy.
      setNote(r.detail || composeMusicOutcomeCopy(r.outcome, shaped.request.prompt));
    } catch {
      setNote(composeMusicOutcomeCopy("failed", shaped.request.prompt));
    } finally {
      setBusy(false);
    }
  }, [prompt, length, busy]);

  return (
    <div className="audioio-music" aria-label="Compose music">
      <div className="verify-row">
        <span className="verify-head">COMPOSE MUSIC</span>
        <span
          className={`verify-pill audioio-${gateOpen ? "good" : "idle"}`}
          title={
            "Generate a full music track from a text prompt via ElevenLabs. It " +
            "requires the ElevenLabs key + cloud music ([voice].cloud_music) ON; " +
            "with either off (or offline) nothing is created — never a fabricated " +
            "track. Only the prompt you enter leaves the device; no key value or " +
            "audio path crosses this surface."
          }
        >
          <span className={`dot ${gateOpen ? "good" : "idle"}`} />
          {gateOpen ? "READY" : "OFF"}
        </span>
        <span className="verify-meaning">{musicGateCopy(reason)}</span>
      </div>

      <div className="audioio-vl-form" aria-label="Compose a track">
        <div className="audioio-vl-fields">
          <label className="audioio-vl-label">
            Prompt
            <textarea
              className="audioio-vl-textarea"
              value={prompt}
              onChange={(e) => setPrompt(e.target.value)}
              disabled={!gateOpen || busy}
              rows={2}
              placeholder="e.g. an 8-bit happy birthday"
              title={gateOpen ? undefined : musicGateCopy(reason)}
            />
          </label>
          <label className="audioio-vl-label">
            Length (seconds, optional)
            <input
              className="audioio-vl-input"
              type="number"
              inputMode="numeric"
              min={MIN_LENGTH_SECONDS}
              max={MAX_LENGTH_SECONDS}
              value={length}
              onChange={(e) => setLength(e.target.value)}
              disabled={!gateOpen || busy}
              placeholder={`${DEFAULT_LENGTH_SECONDS} (blank = default)`}
              title={
                gateOpen
                  ? `Optional track length, ${MIN_LENGTH_SECONDS}–${MAX_LENGTH_SECONDS}s (blank = daemon default)`
                  : musicGateCopy(reason)
              }
            />
          </label>
          <button
            type="button"
            className="icon-btn audioio-vl-submit"
            onClick={() => void submit()}
            disabled={!gateOpen || busy}
            title={gateOpen ? "Compose this track (calls ElevenLabs once)" : musicGateCopy(reason)}
          >
            {busy ? "Composing…" : "Compose"}
          </button>
        </div>
        {note && (
          <div className="audioio-sub dim-note" role="status">
            {note}
          </div>
        )}
      </div>

      <div className="audioio-sub dim-note">
        Composing music calls ElevenLabs once and generates a full track from your
        prompt (a cloud generation). Requires the key + cloud music — else nothing
        is created.
      </div>
    </div>
  );
}

/** ADD A PRONUNCIATION — a word + how to say it, then Add. Shaping/validation in
 *  core/voiceLab.ts; the daemon mints ONE alias rule. The outcome is honest — never
 *  a fabricated dictionary. */
function AddPronunciationForm({
  gateOpen,
  reason,
}: {
  gateOpen: boolean;
  reason: ReturnType<typeof voiceLabDisabledReason>;
}) {
  const [word, setWord] = useState("");
  const [say, setSay] = useState("");
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState("");

  const submit = useCallback(async () => {
    if (busy) return;
    const shaped = buildPronunciationRequest(word, say, "");
    if (!shaped.ok) {
      setNote(pronunciationErrorCopy(shaped.error));
      return;
    }
    setBusy(true);
    setNote("");
    try {
      const r = await createPronunciation(shaped.request.word, shaped.request.say);
      setNote(
        r.detail ||
          pronunciationOutcomeCopy(r.outcome, shaped.request.word, shaped.request.say),
      );
    } catch {
      setNote(pronunciationOutcomeCopy("failed", shaped.request.word, shaped.request.say));
    } finally {
      setBusy(false);
    }
  }, [word, say, busy]);

  return (
    <div className="audioio-vl-form" aria-label="Add a pronunciation">
      <div className="audioio-vl-title">Add a pronunciation</div>
      <div className="audioio-vl-fields">
        <label className="audioio-vl-label">
          Word
          <input
            className="audioio-vl-input"
            value={word}
            onChange={(e) => setWord(e.target.value)}
            disabled={!gateOpen || busy}
            placeholder="e.g. JARVIS"
            title={gateOpen ? undefined : voiceLabGateCopy(reason)}
          />
        </label>
        <label className="audioio-vl-label">
          Say it like
          <input
            className="audioio-vl-input"
            value={say}
            onChange={(e) => setSay(e.target.value)}
            disabled={!gateOpen || busy}
            placeholder="e.g. jar viss"
            title={gateOpen ? undefined : voiceLabGateCopy(reason)}
          />
        </label>
        <button
          type="button"
          className="icon-btn audioio-vl-submit"
          onClick={() => void submit()}
          disabled={!gateOpen || busy}
          title={gateOpen ? "Add this pronunciation (calls ElevenLabs once)" : voiceLabGateCopy(reason)}
        >
          {busy ? "Adding…" : "Add"}
        </button>
      </div>
      {note && (
        <div className="audioio-sub dim-note" role="status">
          {note}
        </div>
      )}
    </div>
  );
}

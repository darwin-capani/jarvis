import { useEffect, useState } from "react";
import type { CSSProperties } from "react";
import type { ActiveAgent, CoreState, HealStatus } from "../core/state";
import type {
  LocalToolsStatus,
  LocalWarmStatus,
  LockdownStatus,
  ModelTierStatus,
  SecurityStatus,
  SttTierStatus,
  VoiceIdStatus,
  VoiceTierStatus,
  VoiceModeStatus,
} from "../core/events";
import {
  localToolsHonest,
  localToolsLabel,
  localToolsTone,
  localWarmExtraCount,
  localWarmHonest,
  localWarmLabel,
  localWarmTone,
  localSubLabel,
  lockdownLabel,
  lockdownTone,
  securityLabel,
  securityTone,
  modelTierHonest,
  modelTierLabel,
  modelTierModeLabel,
  modelTierReasonHonest,
  modelTierReasonLabel,
  modelTierTone,
  sttTierDetail,
  sttTierLabel,
  sttTierTone,
  voiceIdDisplay,
  voiceIdLabel,
  voiceIdSimilarityPct,
  voiceIdTone,
  voiceTierDetail,
  voiceTierLabel,
  voiceTierTone,
  prosodyProfileLabel,
  voiceModeTone,
  voiceModeRichDetail,
  voiceModeWhisperDetail,
} from "../core/events";
import { toggleFullscreen } from "../tauri/bridge";

/** The voice-id chip. Shows the on-device speaker-verification state:
 *  OFF / NOT ENROLLED / ENROLLING / ENROLLED / this-turn VERIFIED✓ /
 *  UNRECOGNIZED. HONEST by construction — the title spells out that this is a
 *  lightweight acoustic match that RAISES the bar, NOT a biometric, and the
 *  score is a SIMILARITY (never a guarantee). The chip renders nothing when
 *  voice-id is OFF except a dim "VOICE OFF" marker, so the bar is not cluttered
 *  in the shipped-OFF default; an enabled subsystem gets the live verdict. */
function VoiceIdChip({ v }: { v: VoiceIdStatus }) {
  const display = voiceIdDisplay(v);
  const tone = voiceIdTone(display);
  const label = voiceIdLabel(display);
  const sim = voiceIdSimilarityPct(v);

  // Honest hover copy: what voice-id is and is not.
  const baseHonesty =
    "On-device voice match — a lightweight acoustic check that raises the bar, " +
    "NOT a biometric. Spoofable by a recording or a good impression. The hard " +
    "backstop for outward actions is still the confirmation gate.";
  let title = baseHonesty;
  if (display === "enrolling") {
    const cap = v.captured ?? 0;
    const need = v.need;
    title = `Enrolling your voice${need !== null ? ` (${cap}/${cap + need} captured)` : ""}. ` + baseHonesty;
  } else if (display === "verified" && sim !== null) {
    title = `This turn matched the enrolled voice — similarity ${sim}% (a similarity, NOT a guarantee). ${baseHonesty}`;
  } else if (display === "unrecognized" && sim !== null) {
    title = `This turn did NOT match the enrolled voice — similarity ${sim}% (below the threshold). ${baseHonesty}`;
  }

  // Suffix: a check on verified, a similarity readout when there is a verdict,
  // or the capture progress while enrolling.
  let suffix = "";
  if (display === "verified") {
    suffix = sim !== null ? ` ✓ ${sim}%` : " ✓";
  } else if (display === "unrecognized" && sim !== null) {
    suffix = ` ${sim}%`;
  } else if (display === "enrolling" && v.need !== null) {
    suffix = ` ${v.captured ?? 0}/${(v.captured ?? 0) + v.need}`;
  }

  return (
    <span className={`status-item voiceid-chip ${tone}`} title={title}>
      <span className={`dot ${tone}`} />
      VOICE {label}
      {suffix}
    </span>
  );
}

/** The MODEL tier chip. Shows WHICH model answers — LOCAL / FAST / HEAVY — plus
 *  the MANUAL-vs-AUTO mode and the REASON (override/auto/fallback). HONEST by
 *  construction: the hover copy spells out that LOCAL is the on-device/private
 *  path but capability-limited (NOT Opus-grade), that AUTO is a per-turn difficulty
 *  HEURISTIC (overridable), and that FALLBACK is the honest cloud-unreachable
 *  degrade to on-device. It is MODEL-ONLY — it reflects which model answers and
 *  changes NO safety gate. Before any model.tier/model.swap telemetry the chip
 *  rests on a dim AWAITING/AUTO marker so the bar is not cluttered. */
function ModelTierChip({ m }: { m: ModelTierStatus }) {
  const tone = modelTierTone(m.tier, m.reason);
  const label = modelTierLabel(m.tier);
  const mode = modelTierModeLabel(m.manual);
  const reasonLabel = modelTierReasonLabel(m.reason);

  // Honest hover copy: which model, the mode, and why — with the ceilings stated.
  const baseHonesty =
    "MODEL-ONLY — picks which model answers; changes NO safety gate (the " +
    "consequential confirmation gate, the master switch, the owner voice-id gate, " +
    "and per-agent allowlists are identical at every tier).";
  const title =
    `${label} (${mode}) — ${modelTierHonest(m.tier)}. ` +
    (reasonLabel ? `${reasonLabel}: ${modelTierReasonHonest(m.reason)}. ` : "") +
    baseHonesty;

  // Suffix: the mode, plus a FALLBACK flag when the cloud degraded to on-device
  // (the one reason worth surfacing inline — override/auto are implied by the
  // mode word).
  const suffix =
    m.reason === "fallback" ? ` ${mode} · FALLBACK` : ` ${mode}`;

  return (
    <span className={`status-item modeltier-chip ${tone}`} title={title}>
      <span className={`dot ${tone}`} />
      MODEL {label}
      {suffix}
    </span>
  );
}

/** The RESIDENT-MODELS chip (task #17, item 3). Shows the Local tier's resident-
 *  models PLAN — SINGLE (one local model kept warm: the safe single-resident
 *  low-RAM default) vs MULTI (>1 local model kept warm for an INSTANT local swap
 *  WHEN RAM allows) — plus, when multi-resident, the count of EXTRA warm models and
 *  the ACTIVE per-turn sub-choice (FAST/CAPABLE/AUTO). HONEST by construction: the
 *  hover copy spells out that multi-resident keeps >1 local model warm for instant
 *  swap ONLY when RAM allows (~2x RAM), that single-resident is the safe default on
 *  a low-RAM Mac, and that this is the PLANNED warm-set, NOT a measured speed
 *  benefit (the swap benefit is device/RAM-dependent and not measured); it changes
 *  NO safety gate and does NOT change which tier is chosen — it only refines which
 *  warm local model answers an on-device turn. It rests on a dim AWAITING marker
 *  until the daemon emits the config-derived `model.local_warm` snapshot so the bar
 *  is not cluttered before it reports. */
function ResidentModelsChip({ lw }: { lw: LocalWarmStatus }) {
  const tone = localWarmTone(lw);
  const label = localWarmLabel(lw);
  const title = localWarmHonest(lw);

  // Suffix: in MULTI, the count of extra warm models (+N) and the active per-turn
  // sub-choice when one has been reported. SINGLE/AWAITING add no suffix — the bar
  // stays calm in the safe default.
  const extra = localWarmExtraCount(lw);
  const sub = localSubLabel(lw.activeSub);
  const suffix = lw.multiResident
    ? ` +${extra}${sub ? ` · ${sub}` : ""}`
    : "";

  return (
    <span className={`status-item residentmodels-chip ${tone}`} title={title}>
      <span className={`dot ${tone}`} />
      LOCAL {label}
      {suffix}
    </span>
  );
}

/** The OFFLINE-AGENCY chip (task #3, item 2). Shows whether the on-device path is
 *  USING SAFE LOCAL TOOLS this turn (ACTING OFFLINE) vs answering conversationally
 *  (CHATTING — the resting state). HONEST by construction: the hover copy spells out
 *  that the on-device ~4B is using a CURATED SAFE set of LOCAL read/compute tools
 *  while offline (memory recall, doc-search, skills, confined file-read — never
 *  outward/cloud tools), that it is LESS RELIABLE at tool-calling than the cloud
 *  model (a real ceiling — the loop is BOUNDED and falls back to a plain answer),
 *  and that the SAME safety gates (confirmation, voice-id, lockdown, policy) ALL
 *  still apply offline — offline tool-use bypasses NOTHING. When a gate fired this
 *  turn (`gated`) or the 4B reached outside the safe subset and was REFUSED
 *  (`refusedOutOfSubset`), the chip flags it inline (amber) and the copy names it as
 *  the proof the gates hold offline. It is ACTIVITY-ONLY — it reflects what the
 *  on-device path did and changes NO gate; the model.tier chip already marks the
 *  Local tier. Renders nothing in the resting CHATTING state so the bar is not
 *  cluttered when the offline loop is not running. */
function LocalToolsChip({ lt }: { lt: LocalToolsStatus }) {
  // ACTIVITY-ONLY: render only while actually acting offline. The resting
  // "chatting" state adds no chip (the MODEL LOCAL tier chip already says we are
  // on-device) — the bar stays uncluttered until a local tool runs.
  if (!lt.engaged) return null;

  const tone = localToolsTone(lt);
  const label = localToolsLabel(lt);
  const title = localToolsHonest(lt);

  // Suffix: the tool count, plus an inline GATED / REFUSED flag when a safety gate
  // fired or the 4B reached outside the safe subset (the honest "gates apply
  // offline" markers worth surfacing inline).
  const count = lt.toolsUsed > 0 ? ` ${lt.toolsUsed}` : "";
  const flag = lt.gated
    ? " · GATED"
    : lt.refusedOutOfSubset
      ? " · REFUSED"
      : "";

  return (
    <span className={`status-item localtools-chip ${tone}`} title={title}>
      <span className={`dot ${tone}`} />
      {label}
      {count}
      {flag}
    </span>
  );
}

/** The VOICE tier chip. Shows which TTS backend voiced the last reply — ON-DEVICE
 *  (Kokoro, the private/offline default + fallback) vs CLOUD VOICE (the optional
 *  ElevenLabs premium voices). HONEST by construction: the hover copy states that
 *  on the CLOUD path the spoken text LEAVES the device to be synthesized, that
 *  on-device Kokoro is the private default and the fallback on any cloud error, and
 *  that this is VOICE-ONLY — it changes how DARWIN sounds, no safety gate, and the
 *  telemetry carries no key. Before any voice.tier telemetry the chip rests on a dim
 *  AWAITING marker. */
function VoiceTierChip({ v }: { v: VoiceTierStatus }) {
  const tone = voiceTierTone(v.backend);
  const label = voiceTierLabel(v.backend);
  const title =
    `Voice: ${label}. ${voiceTierDetail(v.backend)} ` +
    "VOICE-ONLY — changes how DARWIN sounds, not any safety gate. On-device Kokoro " +
    "is the private/offline default + the fallback; the cloud voice tier ships OFF " +
    "and needs an ElevenLabs key + online to engage. (No key or voice id is ever " +
    "shown here — only which backend spoke.)" +
    (v.agent ? ` Last voice: ${v.agent}.` : "");

  return (
    <span className={`status-item voicetier-chip ${tone}`} title={title}>
      <span className={`dot ${tone}`} />
      TTS {label}
    </span>
  );
}

/** The STT tier chip. Shows which STT backend transcribed the last captured audio
 *  — ON-DEVICE STT (whisper/mlx_whisper, the private/offline default + the fallback
 *  on any cloud error) vs CLOUD STT (the optional gated ElevenLabs Scribe tier).
 *  HONEST by construction: the hover copy states that on the CLOUD path the user's
 *  VOICE AUDIO leaves the device to be transcribed — MORE sensitive than the TTS
 *  text leg (which sent synthesized text); that on-device whisper is the private
 *  default and the fallback; and that the telemetry carries no key/transcript/audio.
 *  Before any stt.tier telemetry the chip rests on a dim AWAITING marker. */
function SttTierChip({ s }: { s: SttTierStatus }) {
  const tone = sttTierTone(s.backend);
  const label = sttTierLabel(s.backend);
  const title =
    `Transcription: ${label}. ${sttTierDetail(s.backend)} ` +
    "STT is MORE sensitive than TTS text — the cloud path uploads your actual voice " +
    "recording, not synthesized output. On-device whisper is the private/offline " +
    "default + the fallback on any cloud error; the cloud-STT tier ships OFF and " +
    "needs an ElevenLabs key + online to engage. (No key or transcript is ever shown " +
    "here — only which backend transcribed.)";

  return (
    <span className={`status-item stttier-chip ${tone}`} title={title}>
      <span className={`dot ${tone}`} />
      STT {label}
    </span>
  );
}

/** The VOICE-MODE chip — the read-only #33 (adaptive tone) + #34 (whisper)
 *  indicator. Shows the current TONE profile (NEUTRAL/CALM/URGENT/WARM), whether
 *  RICH prosody was honestly applied (EL-v3 only), and the WHISPER state. HONEST
 *  by construction: the label reads "RICH" only when the daemon's ground-truth
 *  `rich` bit says EL-v3 audio-tags/stability/style were ACTUALLY applied —
 *  otherwise "coarse", with the hover copy spelling out that rich prosody is
 *  ElevenLabs-v3-gated and the on-device default gets a coarse rate-only mapping,
 *  never faked. A "WHISPER" marker is appended only while discreet mode is
 *  engaged, and the copy states whisper changes DELIVERY only — it never
 *  suppresses a required confirmation. Both features ship OFF/neutral, so before
 *  any telemetry (and on a malformed frame) the chip rests on the honest
 *  NEUTRAL · whisper-off default. Renders nothing until the first voice.prosody
 *  frame so the bar is not cluttered before the daemon has spoken. */
function VoiceModeChip({ v }: { v: VoiceModeStatus }) {
  if (!v.seen) return null;
  const tone = voiceModeTone(v.profile);
  const profile = prosodyProfileLabel(v.profile);
  const richLabel = v.rich ? "RICH" : "coarse";
  const title =
    `Tone: ${profile} · ${v.rich ? "rich prosody" : "coarse/neutral prosody"}` +
    `${v.whisper ? " · whisper ON" : ""}. ` +
    `${voiceModeRichDetail(v)} ${voiceModeWhisperDetail(v)} ` +
    "EXPRESSIVENESS-ONLY — changes how DARWIN sounds (tone + soft/terse delivery), " +
    "not any safety gate. Rich prosody is ElevenLabs-v3-gated (the on-device default " +
    "gets a coarse rate-only mapping, never faked); whisper changes delivery only and " +
    "never suppresses a required confirmation. (No key, voice id, or text is ever " +
    "shown here — only the delivery facts.)";

  return (
    <span className={`status-item voicemode-chip ${tone}`} title={title}>
      <span className={`dot ${tone}`} />
      TONE {profile} · {richLabel}
      {v.whisper && " · WHISPER"}
    </span>
  );
}

/** The AT-REST ENCRYPTION chip. Shows ENCRYPTED AT REST (the master key actually
 *  resolved this run and the four sensitive SQLite stores + the voiceid owner blob
 *  open via SQLCipher) vs NOT ENCRYPTED (the shipped-OFF default, or config-on but
 *  the key failed to resolve). HONEST by construction: the label is driven by the
 *  GROUND-TRUTH `active`, never `config` alone, so a config-on-but-key-failed run
 *  reads honestly as NOT ENCRYPTED — never a false green. The hover copy spells out
 *  that this protects data AT REST ON DISK only (NOT against a live-process/root
 *  attacker — the key + decrypted pages are in RAM while darwind runs), names the
 *  cipher + key location, and states the scope is partial (the config TOML + the
 *  in-RAM working set are NOT covered). Renders nothing until the snapshot arrives
 *  so the bar is not cluttered before the daemon reports. */
function EncryptionChip({ sec }: { sec: SecurityStatus }) {
  const tone = securityTone(sec);
  const label = securityLabel(sec);
  // Honest hover copy: what at-rest encryption is and is NOT. Prefer the daemon's
  // verbatim honesty string (single source of truth); fall back to a built-in line
  // for an older daemon that has not got the field.
  const honesty =
    sec.honesty ||
    "Protects data AT REST ON DISK only — NOT against a live-process/root attacker " +
      "(the key + decrypted data are in RAM while darwind runs). Partial scope: the " +
      "config TOML and the in-RAM working set are NOT encrypted.";
  const cipher = sec.cipher ? `${sec.cipher}. ` : "";
  const keyLoc = sec.keyLocation ? `Key: ${sec.keyLocation}. ` : "";
  // When config is ON but the key did not resolve, say so honestly in the title —
  // the indicator is NOT ENCRYPTED and the reason is a key/Keychain failure.
  const mismatch =
    sec.config && !sec.active
      ? "Config requests encryption but the master key did NOT resolve this run, so the stores are PLAINTEXT — check the Keychain item. "
      : "";
  const title = `${label}. ${mismatch}${cipher}${keyLoc}${honesty}`;

  return (
    <span className={`status-item encryption-chip ${tone}`} title={title}>
      <span className={`dot ${tone}`} />
      {sec.active ? "ENCRYPTED" : "NOT ENCRYPTED"}
    </span>
  );
}

/** The PANIC / LOCKDOWN status chip (task #12). Shows the emergency-stop posture:
 *  LOCKED DOWN (the alarm state — every consequential / outward / autonomy / mic
 *  surface is forced off and it persists across a restart) vs NORMAL (the shipped
 *  default — every gate exactly as configured). HONEST by construction: the hover
 *  copy spells out that the stop is FUTURE-only (it cannot undo an action already
 *  done) and that, when LOCKED DOWN, it survives a restart until a deliberate
 *  unlock; when the snapshot says it was RESTORED from the marker, the chip notes
 *  the stop persisted across a reboot. Renders nothing until the daemon emits the
 *  startup snapshot so the bar is not cluttered before it reports; once present it
 *  always shows the honest current posture (never stale). */
function LockdownChip({ l }: { l: LockdownStatus }) {
  const tone = lockdownTone(l);
  const label = lockdownLabel(l);
  const title = l.locked
    ? "EMERGENCY STOP ENGAGED — all future outward actions, all autonomy, and the " +
      "microphone are forced OFF, and this persists across a restart until you " +
      "deliberately unlock. It does NOT undo anything already done (a sent message " +
      "stays sent). Unlock from Settings or by saying 'unlock'." +
      (l.restoredFromMarker
        ? " This lockdown was RESTORED from the persisted marker on startup — it survived a restart."
        : "")
    : "NORMAL — no emergency stop; every gate is exactly as you configured it. The " +
      "PANIC control stops all FUTURE outward actions + autonomy + the mic " +
      "immediately and persists, but cannot undo an action already executed.";

  return (
    <span className={`status-item lockdown-chip ${tone}`} title={title}>
      <span className={`dot ${tone}`} />
      {l.locked ? "🔒 " : ""}
      {label}
      {l.locked && l.restoredFromMarker ? " · RESTORED" : ""}
    </span>
  );
}

function WarnTri() {
  return (
    <svg viewBox="0 0 20 18" className="tri sm" aria-hidden="true">
      <path d="M10 1.5 19 16.5 H1 Z" fill="none" strokeWidth="1.6" />
      <line x1="10" y1="6.5" x2="10" y2="11.4" strokeWidth="1.8" />
      <circle cx="10" cy="14" r="1" stroke="none" />
    </svg>
  );
}

export default function StatusBar({
  connected,
  coreState,
  cloudKeyPresent,
  inferenceOffline,
  heal,
  cloudModel,
  activeAgent,
  voiceId,
  modelTier,
  localWarm = null,
  localTools = null,
  voiceTier,
  sttTier,
  voiceMode,
  security = null,
  lockdown = null,
  onPanic,
  onOpenSettings,
  onOpenDeck,
}: {
  connected: boolean;
  coreState: CoreState;
  cloudKeyPresent: boolean | null;
  inferenceOffline: boolean;
  heal: HealStatus | null;
  cloudModel: string | null;
  activeAgent: ActiveAgent | null;
  voiceId: VoiceIdStatus;
  modelTier: ModelTierStatus;
  /** The RESIDENT-MODELS plan (model.local_warm + the per-turn local_sub), or null
   *  before the daemon emits the config-derived snapshot. Optional (defaults to
   *  null) so the existing StatusBar tests render without it; the indicator renders
   *  only once the snapshot is present. */
  localWarm?: LocalWarmStatus | null;
  /** The OFFLINE TOOL-LOOP surface (local_tools.*), or null before the daemon emits
   *  any local_tools event. Optional (defaults to null) so the existing StatusBar
   *  tests render without it; the ACTING-OFFLINE chip renders only once the loop has
   *  engaged (and renders nothing in the resting "chatting" state). */
  localTools?: LocalToolsStatus | null;
  voiceTier: VoiceTierStatus;
  sttTier: SttTierStatus;
  /** The VOICE-MODE (#33 adaptive tone + #34 whisper) surface — the read-only
   *  expressiveness indicator. Always present (seeded with the honest OFF/neutral
   *  default); the chip itself renders nothing until the first voice.prosody frame. */
  voiceMode: VoiceModeStatus;
  /** The at-rest-encryption posture (security.status), or null before the daemon
   *  emits the startup snapshot. Optional (defaults to null) so the existing
   *  StatusBar tests render without it; the chip renders only once it is present. */
  security?: SecurityStatus | null;
  /** The PANIC / LOCKDOWN emergency-stop posture (lockdown.status), or null before
   *  the daemon emits the startup snapshot. Optional (defaults to null) so the
   *  existing StatusBar tests render without it; the indicator renders only once it
   *  is present. */
  lockdown?: LockdownStatus | null;
  /** Fire the DEDICATED panic verb (NOT {cmd:"ask"}) — engage the emergency stop.
   *  Optional so the existing StatusBar tests render without it; when absent the
   *  prominent PANIC button is hidden. App wires this to sendCommand({cmd:"panic"})
   *  and folds the reply's `locked` into the indicator immediately. */
  onPanic?: () => void;
  onOpenSettings: () => void;
  onOpenDeck: () => void;
}) {
  const [clock, setClock] = useState(() => new Date());
  useEffect(() => {
    const id = setInterval(() => setClock(new Date()), 1000);
    return () => clearInterval(id);
  }, []);

  return (
    <header className="statusbar">
      <span className="status-ident">
        D.A.R.W.I.N <span className="ident-sub">MAIN // GENERAL</span>
      </span>
      <span className="status-item">
        <span className={`dot ${connected ? "ok" : "off"}`} />
        LINK {connected ? "ONLINE" : "OFFLINE"}
      </span>
      <span className="status-item">
        <span className={`dot ${!connected ? "off" : inferenceOffline ? "bad" : "ok"}`} />
        INFERENCE
      </span>
      <span className="status-item">
        <span
          className={`dot ${
            cloudKeyPresent === null ? "off" : cloudKeyPresent ? "good" : "warn"
          }`}
        />
        CLOUD KEY{" "}
        {cloudKeyPresent === null ? "UNKNOWN" : cloudKeyPresent ? "PRESENT" : "ABSENT"}
      </span>
      <VoiceIdChip v={voiceId} />
      <ModelTierChip m={modelTier} />
      {localWarm && localWarm.base !== null && <ResidentModelsChip lw={localWarm} />}
      {localTools && <LocalToolsChip lt={localTools} />}
      <VoiceTierChip v={voiceTier} />
      <SttTierChip s={sttTier} />
      <VoiceModeChip v={voiceMode} />
      {security && <EncryptionChip sec={security} />}
      {lockdown && <LockdownChip l={lockdown} />}
      {heal && (
        <span className="status-item warned" title={heal.event}>
          <WarnTri />
          HEAL {heal.event === "heal.suppressed" ? "SUPPRESSED" : "TRIGGERED"} ·{" "}
          {heal.errorsLast60s}/60s
        </span>
      )}
      {cloudModel && coreState === "thinking-cloud" && (
        <span className="status-item cloud-chip">▲ {cloudModel}</span>
      )}
      {activeAgent && (
        // Active-agent chip (CONTRACT part C.4) in the agent's identity hue.
        // The hue is a runtime value (0..360 from agent.active), so it is set
        // inline rather than via a fixed token.
        <span
          className="status-item agent-chip"
          style={
            { ["--agent-hue" as string]: String(activeAgent.hue) } as CSSProperties
          }
          title={activeAgent.role || undefined}
        >
          ◇ ACTIVE: {activeAgent.name.toUpperCase()}
        </span>
      )}
      <span className="grow" />
      <span className="status-item state-word">{coreState.toUpperCase()}</span>
      <span className="status-item">
        {clock.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" })}
      </span>
      {/* The prominent PANIC control (task #12) — the always-visible emergency
          stop. It sends the DEDICATED panic verb (NOT {cmd:"ask"}), so a panic can
          never leak to the answer path; App folds the reply's `locked` into the
          indicator immediately. When already LOCKED DOWN it reads ENGAGED and is
          inert (unlock is deliberate, from Settings). Honest title: stops all
          FUTURE outward actions + autonomy + the mic and persists; cannot undo
          anything already done. */}
      {onPanic && (
        <button
          type="button"
          className={`panic-btn${lockdown?.locked ? " engaged" : ""}`}
          onClick={onPanic}
          disabled={lockdown?.locked === true}
          aria-label="Panic — engage the emergency lockdown"
          title={
            lockdown?.locked
              ? "Lockdown is ENGAGED — all future outward actions, autonomy, and the mic are off and persist across a restart. Unlock deliberately from Settings."
              : "PANIC — immediately stop ALL future outward actions, ALL autonomy, and the microphone, and persist it across a restart until you unlock. It does NOT undo anything already done (a sent message stays sent)."
          }
        >
          {lockdown?.locked ? "🔒 LOCKED" : "⛔ PANIC"}
        </button>
      )}
      <button className="icon-btn" onClick={onOpenDeck} title="Command Deck (C)">
        ◎
      </button>
      <button className="icon-btn" onClick={() => void toggleFullscreen()} title="Fullscreen (F11)">
        ⛶
      </button>
      <button className="icon-btn" onClick={onOpenSettings} title="Settings">
        ⚙
      </button>
    </header>
  );
}

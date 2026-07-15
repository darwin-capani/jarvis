//! EXPRESSIVENESS LAYER — #33 adaptive tone/prosody + #34 whisper/discreet mode.
//!
//! This module is the PURE expressiveness brain. It answers two questions for a
//! reply that is about to be spoken, WITHOUT touching audio, ElevenLabs, the mic,
//! a port, or any I/O:
//!
//!   * #33 — given the reply's CONTEXT (what kind of turn is this: an urgent
//!     alert/heal, a routine confirmation, a wellness/biometric reply, a greeting/
//!     roll-call, …), which [`ProsodyProfile`] should colour the delivery, and what
//!     speak params does that profile produce ON THE RESOLVED BACKEND?
//!   * #34 — is DARWIN in WHISPER (discreet) mode, and if so how does that make the
//!     delivery terser + softer — WITHOUT ever silencing a safety confirmation?
//!
//! ## Honesty (the whole point)
//!
//! Rich prosody is EL-v3-GATED. ElevenLabs v3 (`eleven_v3`) supports inline
//! audio-tags (`[calm]`, `[urgently]`, …) and stability/style voice-settings;
//! `eleven_flash_v2_5` / `eleven_multilingual_v2` and the on-device Kokoro engine do
//! NOT. So [`shape_speak_request`] emits the rich v3 surface ONLY when the resolved
//! backend is ElevenLabs AND its model is v3-capable. On Kokoro — and on a non-v3 EL
//! model — it returns a COARSE / neutral shaping (at most a leading rate hint Kokoro
//! actually honours, never fabricated audio-tags). We NEVER fake rich prosody on a
//! backend that cannot do it, and the caller can read [`SpeakShape::rich`] to know
//! whether the rich surface was actually applied.
//!
//! ## Posture
//!
//! Both features SHIP ON (full-power default) behind their own config flags
//! (`[voice].adaptive_prosody`, `[voice].whisper`, `[voice].whisper_auto`); they are
//! EXPRESSIVENESS-ONLY (delivery, never a gate). With them
//! off, the [`SpeakShape`] this module produces is the IDENTITY shaping — the speak
//! request is byte-for-byte today's neutral request on every backend. Whisper changes
//! DELIVERY only: it never changes WHETHER a gate speaks, never suppresses a required
//! confirmation, never weakens the master switch / confirm gate / voice-id / lockdown
//! / policy. It also opens NO mic: the auto-engage path is a PURE function over an
//! energy series the audio layer already computes.
//!
//! Everything here is HERMETIC: pure functions of their inputs, no globals, no
//! network, no audio. The shaped params are the contract the speak path threads to
//! the inference server (which already accepts `backend`/`model`; the v3 voice
//! settings ride the same request body the daemon builds).

// This module is the PURE, verified expressiveness CONTRACT (#33 + #34): a
// deterministic classifier, the per-backend speak-request shaper, the whisper state
// machine + auto-engage heuristic, and the thin process-global binding the live sites
// toggle/read. It is now WIRED LIVE behind its ON-by-default (expressiveness-only) flags:
//   * speech.rs (+ router.rs roll-call) call `classify_prosody` + `shape_speak_request`
//     + `apply_whisper` and thread the `SpeakShape` onto the inference speak request;
//   * router.rs routes `parse_whisper_command` -> `apply_command_global` into the
//     process-global whisper state (mirroring `model_tier::OVERRIDE`);
//   * audio.rs calls `apply_auto_engage_global` over the VAD energy series behind both
//     `[voice].whisper` && `[voice].whisper_auto` (inert-by-flag; never opens the mic);
//   * inference/server.py CONSUMES the shaped params (EL-v3 audio-tag inline +
//     stability/style in voice_settings; coarse rate/gain on every backend).
// A few PURE contract predicates retain `#[allow(dead_code)]` rather than a warning —
// each is covered by a hermetic test below and kept as the auditable contract surface.
#![allow(dead_code)]

use crate::config::Config;
use crate::voice_tier::Backend;

/// The ElevenLabs model id that supports inline audio-tags + stability/style
/// voice-settings (the only backend on which rich adaptive prosody is real). The
/// daemon mirrors the server's model ids here so the v3-capability check is a pure,
/// local decision; `eleven_flash_v2_5` / `eleven_multilingual_v2` are NOT v3.
pub const ELEVENLABS_V3_MODEL: &str = "eleven_v3";

/// The tone a reply should be delivered in. Conservative + deterministic: the
/// classifier only leaves [`ProsodyProfile::Neutral`] when the context CLEARLY
/// matches one of the other profiles. `Neutral` is the safe default (and the only
/// profile produced when the feature is off).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProsodyProfile {
    /// The default — a plain, routine reply (a confirmation, a status line, an
    /// ordinary answer). No prosody colouring; the byte-for-byte neutral request.
    Neutral,
    /// A wellness / biometric / reassurance reply — slower, gentler delivery.
    Calm,
    /// An urgent alert / failure / heal / security event — quicker, firmer delivery.
    Urgent,
    /// A greeting / roll-call / welcome — friendly, warm delivery.
    Warm,
}

impl ProsodyProfile {
    /// Stable identifier for `voice.prosody` telemetry / the HUD indicator.
    pub fn as_str(&self) -> &'static str {
        match self {
            ProsodyProfile::Neutral => "neutral",
            ProsodyProfile::Calm => "calm",
            ProsodyProfile::Urgent => "urgent",
            ProsodyProfile::Warm => "warm",
        }
    }
}

/// What KIND of turn is being spoken — the conservative, structured signal the
/// classifier maps to a [`ProsodyProfile`]. The router/agent layer knows this from
/// the route it took (a heal/alert vs a confirmation vs a wellness reply vs a
/// roll-call), so the classifier need NOT guess from free text — it reads a tag the
/// caller already has. This keeps the classifier deterministic and auditable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyKind {
    /// An ordinary answer / status / routine confirmation. -> Neutral.
    Routine,
    /// An urgent alert, failure, self-heal event, or security/lockdown notice.
    /// -> Urgent.
    Alert,
    /// A wellness / biometric / health reply (WHOOP, recovery, reassurance).
    /// -> Calm.
    Wellness,
    /// A greeting / roll-call / welcome / good-morning. -> Warm.
    Greeting,
}

/// PURE, deterministic, conservative context->profile classifier. Maps the caller's
/// structured [`ReplyKind`] to a [`ProsodyProfile`]. There is no free-text guessing
/// and no I/O: the same input always yields the same profile, and anything that is
/// not unambiguously alert/wellness/greeting stays [`ProsodyProfile::Neutral`].
///
/// `required_confirm` is honoured here too: a turn that carries a REQUIRED safety
/// confirmation is delivered NEUTRALLY regardless of kind, so the gate's words are
/// never coloured urgent/soft/warm in a way that could downplay or dramatise them.
/// This is the classifier-side companion to whisper's never-silence guarantee.
pub fn classify_prosody(kind: ReplyKind, required_confirm: bool) -> ProsodyProfile {
    if required_confirm {
        // A safety confirmation is always delivered plainly — its tone is not the
        // expressiveness layer's to colour.
        return ProsodyProfile::Neutral;
    }
    match kind {
        ReplyKind::Alert => ProsodyProfile::Urgent,
        ReplyKind::Wellness => ProsodyProfile::Calm,
        ReplyKind::Greeting => ProsodyProfile::Warm,
        ReplyKind::Routine => ProsodyProfile::Neutral,
    }
}

/// The shaped speak parameters [`shape_speak_request`] produces — the contract the
/// speak path threads to the inference server. This is ADDITIVE to the existing
/// `backend`/`voice_id`/`model` wire: when nothing is set (the OFF/neutral default)
/// the request is byte-for-byte today's.
///
/// SECURITY: carries NO key (like [`Backend`]); only non-secret delivery hints.
#[derive(Debug, Clone, PartialEq)]
pub struct SpeakShape {
    /// ElevenLabs v3 inline audio-tag prefixed to the text (e.g. `[calm]`), or None.
    /// Set ONLY on the EL-v3-capable rich path; NEVER on Kokoro or a non-v3 EL model
    /// (we never fake an audio-tag a backend would speak literally).
    pub audio_tag: Option<&'static str>,
    /// ElevenLabs v3 voice-settings `stability` in [0,1], or None. Lower = more
    /// expressive/variable; higher = steadier. Set ONLY on the rich v3 path.
    pub stability: Option<f32>,
    /// ElevenLabs v3 voice-settings `style` in [0,1], or None. Higher = more
    /// stylistic exaggeration. Set ONLY on the rich v3 path.
    pub style: Option<f32>,
    /// A COARSE delivery rate multiplier honoured on EVERY backend (Kokoro can vary
    /// its speaking rate; the server clamps it). 1.0 = today's neutral rate. This is
    /// the ONLY prosody signal Kokoro gets — an honest coarse mapping, no fake tags.
    pub rate: f32,
    /// A COARSE output volume/gain multiplier in (0,1]. 1.0 = today's level; whisper
    /// mode lowers it for a soft delivery. Honoured on every backend (a gain applied
    /// to the produced WAV), so "speak softly" is real on Kokoro too.
    pub volume: f32,
    /// True when replies should be made TERSE (whisper mode trims them to a short
    /// form before synthesis). The text-shortening itself happens in the reply
    /// builder; this flag tells it to.
    pub terse: bool,
    /// True when the RICH EL-v3 surface (audio_tag/stability/style) was actually
    /// applied. False on Kokoro / non-v3 EL / when prosody is off — the honest
    /// signal that rich prosody was NOT faked on a backend that can't do it.
    pub rich: bool,
}

impl SpeakShape {
    /// The IDENTITY shaping — byte-for-byte today's neutral request: no audio-tag, no
    /// v3 settings, rate/volume 1.0, not terse, not rich. This is exactly what the
    /// caller gets when both features are OFF, and what the speak path treats as "set
    /// nothing extra on the wire".
    pub const fn neutral() -> Self {
        Self {
            audio_tag: None,
            stability: None,
            style: None,
            rate: 1.0,
            volume: 1.0,
            terse: false,
            rich: false,
        }
    }

    /// Whether this shape is the identity (neutral) shaping — i.e. the speak path
    /// should set NOTHING extra on the wire and send today's exact request. Used by
    /// the speak path to keep the default wire untouched, and asserted by tests.
    pub fn is_neutral(&self) -> bool {
        *self == Self::neutral()
    }
}

/// Whether `model` is the ElevenLabs v3 model (the only one with audio-tags +
/// stability/style). PURE string compare against [`ELEVENLABS_V3_MODEL`]; everything
/// else (flash v2.5, multilingual v2, an unknown id) is treated as NON-v3, so the
/// rich surface is withheld rather than faked.
fn is_v3_capable(model: &str) -> bool {
    model.trim() == ELEVENLABS_V3_MODEL
}

/// PURE: shape the speak params for `profile` on the RESOLVED `backend`, honouring
/// the `adaptive_prosody` switch. Returns the EL-v3 rich surface ONLY when the
/// backend is ElevenLabs with a v3-capable model; otherwise a COARSE/neutral shaping.
///
///   * `[voice].adaptive_prosody` OFF (the default) -> [`SpeakShape::neutral`] on
///     EVERY backend: byte-for-byte today's request. The classifier output is
///     ignored.
///   * ON + backend == ElevenLabs(v3) -> the RICH surface: an inline audio-tag plus
///     stability/style voice-settings tuned per profile (`rich = true`). Neutral on
///     this path is still the no-op shape (no tag, no settings).
///   * ON + backend == ElevenLabs(non-v3) OR Kokoro -> a COARSE shaping: at most a
///     rate nudge the backend actually honours (`rich = false`). NO audio-tags,
///     NO v3 settings — rich prosody is EL-v3-gated and we never fake it.
///
/// HONESTY: the `rich` flag tells the caller (and telemetry) whether the rich path
/// was real. On Kokoro it is always false even when a coarse rate is applied.
pub fn shape_speak_request(cfg: &Config, profile: ProsodyProfile, backend: &Backend) -> SpeakShape {
    // (1) Feature OFF -> the identity shaping on every backend (today's exact wire).
    if !cfg.voice.adaptive_prosody {
        return SpeakShape::neutral();
    }

    match backend {
        // (2) The rich path is reachable ONLY on ElevenLabs with a v3-capable model.
        Backend::ElevenLabs { model, .. } if is_v3_capable(model) => shape_rich_v3(profile),
        // (3) ElevenLabs on a NON-v3 model -> coarse (no tags/settings; EL-v3-gated).
        Backend::ElevenLabs { .. } => shape_coarse(profile),
        // (4) Kokoro -> coarse rate-only mapping; rich prosody is never faked here.
        Backend::Kokoro { .. } => shape_coarse(profile),
    }
}

/// The RICH EL-v3 surface for a profile: an inline audio-tag + stability/style voice
/// settings. Neutral is the no-op shape (nothing set). Values are conservative and
/// deterministic.
fn shape_rich_v3(profile: ProsodyProfile) -> SpeakShape {
    let mut s = SpeakShape::neutral();
    match profile {
        ProsodyProfile::Neutral => return s, // no colouring even on the rich path
        ProsodyProfile::Calm => {
            s.audio_tag = Some("[calm]");
            s.stability = Some(0.75); // steadier, gentler
            s.style = Some(0.30);
            s.rate = 0.95; // slightly slower
        }
        ProsodyProfile::Urgent => {
            s.audio_tag = Some("[urgently]");
            s.stability = Some(0.35); // more dynamic / firmer
            s.style = Some(0.60);
            s.rate = 1.08; // slightly quicker
        }
        ProsodyProfile::Warm => {
            s.audio_tag = Some("[warmly]");
            s.stability = Some(0.55);
            s.style = Some(0.45);
            s.rate = 1.0;
        }
    }
    s.rich = true;
    s
}

/// The COARSE mapping for Kokoro (and non-v3 EL models): at most a rate nudge the
/// backend actually honours, and NEVER an audio-tag / v3 setting. `rich = false`
/// always — this is the honest "the backend can't do rich prosody" path.
fn shape_coarse(profile: ProsodyProfile) -> SpeakShape {
    let mut s = SpeakShape::neutral();
    s.rate = match profile {
        ProsodyProfile::Neutral => 1.0,
        ProsodyProfile::Calm => 0.95,
        ProsodyProfile::Urgent => 1.08,
        ProsodyProfile::Warm => 1.0,
    };
    // rich stays false; audio_tag/stability/style stay None — never faked.
    s
}

// ===========================================================================
// #34 WHISPER / DISCREET MODE — the state machine + auto-engage heuristic.
// ===========================================================================

/// The whisper-mode state machine. INERT until the operator turns the feature on
/// ([voice].whisper). Holds whether discreet delivery is currently engaged; mutated
/// ONLY by an explicit command ([`WhisperCommand`]) or — when [voice].whisper_auto is
/// on — by the PURE auto-engage heuristic. PURE: no I/O, no audio, no mic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WhisperState {
    on: bool,
}

/// The explicit toggle a command maps to. [`parse_whisper_command`] turns an operator
/// utterance into one of these (or None when the utterance is not a whisper command).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhisperCommand {
    /// "whisper mode" / "speak quietly" / "be discreet" -> engage whisper.
    On,
    /// "back to normal" / "speak normally" / "out loud" -> disengage whisper.
    Off,
}

impl WhisperState {
    /// A fresh, OFF state — the only state reachable while the feature ships off.
    pub const fn new() -> Self {
        Self { on: false }
    }

    /// Whether discreet (whisper) delivery is currently engaged.
    pub fn is_on(&self) -> bool {
        self.on
    }

    /// Apply an explicit [`WhisperCommand`], honouring the `[voice].whisper` master
    /// switch: with the feature OFF this is a no-op (stays off) so a stray command
    /// can never engage whisper while the operator hasn't enabled it. Returns the new
    /// state for convenience.
    pub fn apply_command(&mut self, cfg: &Config, cmd: WhisperCommand) -> bool {
        if !cfg.voice.whisper {
            self.on = false; // feature off -> always off, regardless of the command
            return self.on;
        }
        self.on = matches!(cmd, WhisperCommand::On);
        self.on
    }

    /// Apply the PURE low-amplitude AUTO-ENGAGE heuristic over an energy series,
    /// honouring BOTH gates: `[voice].whisper` (the feature) AND `[voice].whisper_auto`
    /// (the separate auto-engage opt-in). With EITHER off this is a no-op. When both
    /// are on, a SUSTAINED-quiet series engages whisper; otherwise it is left
    /// unchanged (it never auto-DISENGAGES — only the explicit "back to normal"
    /// turns it off, so a brief loud moment doesn't yank discreet mode away).
    ///
    /// `energies` is a series of recent input RMS energies (0.0..=1.0) the audio
    /// layer already computes — this function NEVER reads the mic. Returns the new
    /// state.
    pub fn apply_auto_engage(&mut self, cfg: &Config, energies: &[f32]) -> bool {
        if !cfg.voice.whisper || !cfg.voice.whisper_auto {
            return self.on; // feature or auto-engage off -> no change
        }
        if is_sustained_quiet(energies) {
            self.on = true;
        }
        self.on
    }
}

/// The quiet threshold + minimum sustained sample count for [`is_sustained_quiet`].
/// Conservative: a SINGLE quiet sample (or a short dip) never trips it — the operator
/// must be consistently speaking softly. Tuned against a normalized RMS in 0.0..=1.0.
const WHISPER_QUIET_RMS: f32 = 0.08;
const WHISPER_SUSTAINED_SAMPLES: usize = 6;

/// PURE heuristic: is this energy series SUSTAINED-quiet — i.e. at least
/// [`WHISPER_SUSTAINED_SAMPLES`] samples AND every sample below [`WHISPER_QUIET_RMS`]?
/// Conservative by design: any sample at/above the threshold (a normal-volume
/// moment), or too-few samples, returns false. No I/O, no mic — a pure fold over the
/// slice the audio layer hands in.
pub fn is_sustained_quiet(energies: &[f32]) -> bool {
    energies.len() >= WHISPER_SUSTAINED_SAMPLES && energies.iter().all(|&e| e < WHISPER_QUIET_RMS)
}

/// PURE command parser: map an operator utterance to a [`WhisperCommand`], or None
/// when it is not a whisper toggle. Case-insensitive substring match on a small,
/// conservative phrase set. "back to normal" / "speak normally" / "out loud" win over
/// the on-phrases so a "normal" utterance is never misread as "engage".
pub fn parse_whisper_command(utterance: &str) -> Option<WhisperCommand> {
    let u = utterance.to_lowercase();
    // OFF phrases first (precedence): a "back to normal" must never read as "on".
    const OFF: &[&str] = &["back to normal", "speak normally", "speak up", "out loud", "normal voice"];
    if OFF.iter().any(|p| u.contains(p)) {
        return Some(WhisperCommand::Off);
    }
    const ON: &[&str] = &["whisper mode", "whisper", "speak quietly", "be discreet", "discreet mode", "keep it down"];
    if ON.iter().any(|p| u.contains(p)) {
        return Some(WhisperCommand::On);
    }
    None
}

// ===========================================================================
// PROCESS-GLOBAL WHISPER STATE — the ONE live mutable slot the router toggles and
// the speak path reads. Mirrors the established `model_tier::OVERRIDE` /
// `voiceid` TURN_GATE / `response_voice::RESPONSE_VOICE_LANG` pattern: a
// poison-tolerant `Mutex` global with a `#[cfg(test)]` thread-local seam so
// parallel tests never race the shared slot. The `WhisperState` struct above stays
// the PURE state machine (no I/O); this is the thin process-lifetime binding.
// ===========================================================================

use std::sync::Mutex;

/// The process-global whisper STATE (distinct from the `[voice].whisper` feature
/// flag, which ships ON). Default OFF — whisper delivery is off until the user
/// explicitly engages it (every mutation goes through `WhisperState::apply_*`, which
/// honour the feature flag). Process-local: resets to OFF on restart, exactly
/// like `model_tier::OVERRIDE` and the voice-id turn gate.
static WHISPER_GLOBAL: Mutex<WhisperState> = Mutex::new(WhisperState::new());

// Test-only thread-local override of the whisper read, mirroring
// `model_tier::OVERRIDE_TL` / `voiceid`'s `GATE_OVERRIDE`: a test forces the read
// on its OWN thread without touching the process-global slot other parallel tests
// share. Compiled out of release.
#[cfg(test)]
thread_local! {
    static WHISPER_TL: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Apply an explicit [`WhisperCommand`] to the PROCESS-GLOBAL whisper state, honouring
/// the `[voice].whisper` master switch (a stray command is inert while the feature is
/// off — `WhisperState::apply_command` enforces it). Returns the new on/off state.
/// This is the live entry the router's whisper-command handler calls. Poison-tolerant.
pub fn apply_command_global(cfg: &Config, cmd: WhisperCommand) -> bool {
    #[cfg(test)]
    {
        // If a test installed a thread-local seam, mutate THERE so the shared
        // process-global slot stays untouched for other parallel tests (mirrors
        // `model_tier::set_override`). The pure `WhisperState::apply_command` still
        // decides the new value (honouring the master switch); we just route the
        // write through the per-thread seam the test is reading.
        if WHISPER_TL.with(|c| c.get().is_some()) {
            let mut st = WhisperState { on: WHISPER_TL.with(|c| c.get()).unwrap_or(false) };
            let now = st.apply_command(cfg, cmd);
            WHISPER_TL.with(|c| c.set(Some(now)));
            return now;
        }
    }
    let mut st = WHISPER_GLOBAL.lock().unwrap_or_else(|p| p.into_inner());
    st.apply_command(cfg, cmd)
}

/// Apply the PURE low-amplitude AUTO-ENGAGE heuristic to the PROCESS-GLOBAL whisper
/// state behind BOTH gates (`[voice].whisper` && `[voice].whisper_auto`). No-op with
/// either off. The energy series is one the audio layer already computed — this NEVER
/// reads the mic. Returns the new on/off state. Poison-tolerant.
pub fn apply_auto_engage_global(cfg: &Config, energies: &[f32]) -> bool {
    #[cfg(test)]
    {
        // Thread-local seam (mirrors `apply_command_global` / `model_tier`): when a
        // test forced the read, route the auto-engage write into that per-thread slot
        // so parallel tests never race the shared global.
        if WHISPER_TL.with(|c| c.get().is_some()) {
            let mut st = WhisperState { on: WHISPER_TL.with(|c| c.get()).unwrap_or(false) };
            let now = st.apply_auto_engage(cfg, energies);
            WHISPER_TL.with(|c| c.set(Some(now)));
            return now;
        }
    }
    let mut st = WHISPER_GLOBAL.lock().unwrap_or_else(|p| p.into_inner());
    st.apply_auto_engage(cfg, energies)
}

/// Whether discreet (whisper) delivery is currently engaged process-wide. The read
/// the speak path consults to fold whisper delivery into the [`SpeakShape`]. With the
/// feature off the global can only ever be OFF (every mutation honours the switch), so
/// this reads false and the speak path is byte-for-byte today's. Poison-tolerant.
pub fn whisper_state_is_on() -> bool {
    #[cfg(test)]
    {
        if let Some(forced) = WHISPER_TL.with(|c| c.get()) {
            return forced;
        }
    }
    WHISPER_GLOBAL.lock().unwrap_or_else(|p| p.into_inner()).is_on()
}

/// `#[cfg(test)]`-only RAII guard forcing `whisper_state_is_on()` on the current
/// thread, restoring the prior thread-local state on drop (so a forced value never
/// leaks into another parallel test). The whole seam is `cfg(test)`. Mirrors
/// `model_tier::OverrideGuard`.
#[cfg(test)]
pub(crate) struct WhisperGuard {
    prev: Option<bool>,
}

#[cfg(test)]
impl WhisperGuard {
    pub(crate) fn force(on: bool) -> Self {
        let prev = WHISPER_TL.with(|c| c.replace(Some(on)));
        Self { prev }
    }
}

#[cfg(test)]
impl Drop for WhisperGuard {
    fn drop(&mut self) {
        WHISPER_TL.with(|c| c.set(self.prev.take()));
    }
}

/// The soft/terse delivery whisper applies to a [`SpeakShape`]. Conservative: lowers
/// the volume to a soft level and marks the reply terse, but does NOT add or change
/// any EL-v3 audio-tag (whisper is a volume/length change, not a tone colour).
const WHISPER_VOLUME: f32 = 0.45;

/// PURE: fold whisper-mode delivery into a [`SpeakShape`], honouring the
/// never-silence guarantee. This is the SINGLE chokepoint that combines #33's
/// profile shaping with #34's discreet delivery.
///
///   * `whisper_on == false` (or the feature off, which keeps the state off) ->
///     `shape` is returned UNCHANGED.
///   * `whisper_on == true` && `required_confirm == false` -> soft + terse: volume is
///     lowered to [`WHISPER_VOLUME`] and `terse` is set, so the reply is delivered
///     quietly and briefly.
///   * `whisper_on == true` && `required_confirm == true` -> the NEVER-SILENCE path:
///     a required safety confirmation is delivered at FULL volume and NOT made terse,
///     so whisper can never soften/shorten a gate's words below audibility. Whisper
///     changes delivery of ordinary replies, never whether (or how clearly) a
///     required confirmation speaks.
///
/// PURE: no I/O. The returned shape is the contract the speak path threads onward.
pub fn apply_whisper(shape: SpeakShape, whisper_on: bool, required_confirm: bool) -> SpeakShape {
    if !whisper_on {
        return shape;
    }
    if required_confirm {
        // NEVER-SILENCE: a required confirmation always speaks fully + clearly. We do
        // NOT lower its volume and do NOT make it terse — whisper changes delivery of
        // ordinary replies only, never a gate's words.
        return shape;
    }
    let mut s = shape;
    s.volume = WHISPER_VOLUME;
    s.terse = true;
    s
}

/// Emit the SECRET-FREE expressiveness telemetry the HUD reads to render the
/// prosody/whisper indicator. Carries ONLY non-secret delivery facts — the profile
/// name, the backend kind, whether the RICH (EL-v3) surface was actually applied
/// (the honest "is this real prosody or a coarse mapping" bit), whether whisper is
/// engaged, and the coarse rate/volume. NEVER the key, the voice id, or the text.
/// Fire-and-forget like every other `telemetry::emit` (dropped when no HUD).
pub fn emit_telemetry(profile: ProsodyProfile, backend: &Backend, shape: &SpeakShape, whisper_on: bool) {
    crate::telemetry::emit(
        "voice",
        "voice.prosody",
        serde_json::json!({
            "profile": profile.as_str(),
            "backend": backend.as_str(),
            "rich": shape.rich,
            "whisper": whisper_on,
            "terse": shape.terse,
            "rate": shape.rate,
            "volume": shape.volume,
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::voice_tier::Backend;

    fn kokoro() -> Backend {
        Backend::Kokoro { voice: "bm_george".to_string() }
    }
    fn el_v3() -> Backend {
        Backend::ElevenLabs { voice_id: "EL_DARWIN".to_string(), model: ELEVENLABS_V3_MODEL.to_string() }
    }
    fn el_flash() -> Backend {
        Backend::ElevenLabs { voice_id: "EL_DARWIN".to_string(), model: "eleven_flash_v2_5".to_string() }
    }

    fn cfg_prosody_on() -> Config {
        let mut c = Config::default();
        c.voice.adaptive_prosody = true;
        c
    }
    fn cfg_whisper_on() -> Config {
        let mut c = Config::default();
        c.voice.whisper = true;
        c
    }
    /// A config with the prosody EXPRESSIVENESS features explicitly OFF — used by the
    /// off-path tests, since the shipped DEFAULT is now ON (full-power).
    fn cfg_prosody_off() -> Config {
        let mut c = Config::default();
        c.voice.adaptive_prosody = false;
        c.voice.whisper = false;
        c.voice.whisper_auto = false;
        c
    }

    // === Defaults: everything ON (full-power) ==============================

    #[test]
    fn features_ship_on_by_default() {
        let cfg = Config::default();
        assert!(cfg.voice.adaptive_prosody, "#33 ships ON (full-power default)");
        assert!(cfg.voice.whisper, "#34 ships ON (full-power default)");
        assert!(cfg.voice.whisper_auto, "#34 auto-engage ships ON (full-power default)");
    }

    // === #33 classifier: deterministic + conservative =====================

    #[test]
    fn classifier_is_deterministic_and_conservative() {
        // Each kind maps to exactly one profile; Routine (and anything ambiguous)
        // stays Neutral. Running twice yields the identical result (no randomness).
        for _ in 0..2 {
            assert_eq!(classify_prosody(ReplyKind::Routine, false), ProsodyProfile::Neutral);
            assert_eq!(classify_prosody(ReplyKind::Alert, false), ProsodyProfile::Urgent);
            assert_eq!(classify_prosody(ReplyKind::Wellness, false), ProsodyProfile::Calm);
            assert_eq!(classify_prosody(ReplyKind::Greeting, false), ProsodyProfile::Warm);
        }
    }

    #[test]
    fn required_confirmation_is_always_classified_neutral() {
        // No matter the kind, a required confirmation is delivered plainly — its tone
        // is not the expressiveness layer's to colour.
        for kind in [ReplyKind::Routine, ReplyKind::Alert, ReplyKind::Wellness, ReplyKind::Greeting] {
            assert_eq!(
                classify_prosody(kind, true),
                ProsodyProfile::Neutral,
                "required confirm must stay Neutral for {kind:?}"
            );
        }
    }

    // === #33 shape: OFF default is byte-for-byte today's neutral request ===

    #[test]
    fn prosody_off_is_neutral_on_every_backend() {
        let cfg = cfg_prosody_off(); // adaptive_prosody = false (explicit off-path)
        for profile in [ProsodyProfile::Neutral, ProsodyProfile::Calm, ProsodyProfile::Urgent, ProsodyProfile::Warm] {
            for backend in [kokoro(), el_v3(), el_flash()] {
                let s = shape_speak_request(&cfg, profile, &backend);
                assert!(s.is_neutral(), "off -> neutral shape for {profile:?} on {backend:?}");
                assert!(!s.rich);
            }
        }
    }

    // === #33 shape: EL-v3 gets the RICH surface ============================

    #[test]
    fn el_v3_gets_rich_audio_tags_and_settings() {
        let cfg = cfg_prosody_on();
        let s = shape_speak_request(&cfg, ProsodyProfile::Urgent, &el_v3());
        assert!(s.rich, "EL v3 is the rich path");
        assert_eq!(s.audio_tag, Some("[urgently]"));
        assert!(s.stability.is_some() && s.style.is_some(), "v3 carries stability+style");
        let calm = shape_speak_request(&cfg, ProsodyProfile::Calm, &el_v3());
        assert_eq!(calm.audio_tag, Some("[calm]"));
        let warm = shape_speak_request(&cfg, ProsodyProfile::Warm, &el_v3());
        assert_eq!(warm.audio_tag, Some("[warmly]"));
        // Neutral even on the rich path is the no-op shape.
        let neutral = shape_speak_request(&cfg, ProsodyProfile::Neutral, &el_v3());
        assert!(neutral.is_neutral(), "Neutral profile = no colouring even on v3");
    }

    // === #33 HONESTY: Kokoro + non-v3 EL never get faked rich prosody ======

    #[test]
    fn kokoro_and_non_v3_el_get_coarse_never_faked_rich() {
        let cfg = cfg_prosody_on();
        for backend in [kokoro(), el_flash()] {
            for profile in [ProsodyProfile::Calm, ProsodyProfile::Urgent, ProsodyProfile::Warm] {
                let s = shape_speak_request(&cfg, profile, &backend);
                assert!(!s.rich, "rich prosody is EL-v3-gated; {backend:?} must be coarse");
                assert_eq!(s.audio_tag, None, "no faked audio-tag on {backend:?}");
                assert_eq!(s.stability, None, "no faked stability on {backend:?}");
                assert_eq!(s.style, None, "no faked style on {backend:?}");
            }
            // Urgent nudges the coarse rate up; Calm down — the only signal these get.
            assert!(shape_speak_request(&cfg, ProsodyProfile::Urgent, &backend).rate > 1.0);
            assert!(shape_speak_request(&cfg, ProsodyProfile::Calm, &backend).rate < 1.0);
        }
    }

    // === #34 whisper: OFF default is inert =================================

    #[test]
    fn whisper_command_is_inert_while_feature_off() {
        let cfg = cfg_prosody_off(); // whisper = false (explicit off-path)
        let mut st = WhisperState::new();
        assert!(!st.apply_command(&cfg, WhisperCommand::On), "feature off -> stays off");
        assert!(!st.is_on());
    }

    #[test]
    fn whisper_off_returns_request_byte_for_byte() {
        // whisper_on=false must return the input shape unchanged (today's request).
        let base = shape_speak_request(&cfg_prosody_on(), ProsodyProfile::Calm, &el_v3());
        let out = apply_whisper(base.clone(), false, false);
        assert_eq!(out, base, "whisper off must not touch the shape");
        // And on the neutral default shape, off whisper keeps it neutral.
        let neutral = SpeakShape::neutral();
        assert!(apply_whisper(neutral, false, false).is_neutral());
    }

    // === #34 whisper: explicit toggle on/off ==============================

    #[test]
    fn explicit_command_toggles_whisper_when_enabled() {
        let cfg = cfg_whisper_on();
        let mut st = WhisperState::new();
        assert!(st.apply_command(&cfg, WhisperCommand::On));
        assert!(st.is_on());
        assert!(!st.apply_command(&cfg, WhisperCommand::Off));
        assert!(!st.is_on());
    }

    #[test]
    fn command_parser_maps_phrases_conservatively() {
        assert_eq!(parse_whisper_command("whisper mode"), Some(WhisperCommand::On));
        assert_eq!(parse_whisper_command("can you speak quietly please"), Some(WhisperCommand::On));
        assert_eq!(parse_whisper_command("be discreet"), Some(WhisperCommand::On));
        assert_eq!(parse_whisper_command("back to normal"), Some(WhisperCommand::Off));
        assert_eq!(parse_whisper_command("you can speak normally now"), Some(WhisperCommand::Off));
        // OFF precedence: a "normal" utterance never reads as "on".
        assert_eq!(parse_whisper_command("ok back to normal voice"), Some(WhisperCommand::Off));
        // Not a whisper command at all.
        assert_eq!(parse_whisper_command("what's the weather"), None);
    }

    // === #34 whisper: terse + soft when on ================================

    #[test]
    fn whisper_on_yields_terse_and_soft() {
        let base = SpeakShape::neutral();
        let out = apply_whisper(base, true, false);
        assert!(out.terse, "whisper makes the reply terse");
        assert!(out.volume < 1.0, "whisper lowers the volume (soft delivery)");
        assert_eq!(out.volume, WHISPER_VOLUME);
        // Whisper does NOT invent an audio-tag (it's a volume/length change).
        assert_eq!(out.audio_tag, None);
    }

    // === #34 NEVER-SILENCE: a required confirmation is not softened ========

    #[test]
    fn whisper_never_silences_a_required_confirmation() {
        // Even with whisper ON, a required confirmation speaks at full volume and is
        // NOT made terse — whisper changes delivery of ordinary replies, never a
        // gate's words.
        let base = SpeakShape::neutral();
        let out = apply_whisper(base.clone(), true, true);
        assert_eq!(out.volume, 1.0, "a required confirm must stay full-volume");
        assert!(!out.terse, "a required confirm must not be trimmed");
        assert_eq!(out, base, "required confirm under whisper == today's request");
    }

    // === #34 auto-engage heuristic: pure, sustained-quiet only, OFF default =

    #[test]
    fn auto_engage_when_disabled_never_trips() {
        // Feature on but whisper_auto explicitly OFF -> a sustained-quiet series does
        // NOT engage. (The shipped DEFAULT is now ON, full-power; this proves the
        // off path still exists when an operator disables auto-engage.)
        let mut cfg = cfg_whisper_on(); // whisper=true
        cfg.voice.whisper_auto = false; // explicit off-path
        let mut st = WhisperState::new();
        let quiet = [0.02_f32; 10];
        assert!(!st.apply_auto_engage(&cfg, &quiet), "auto-engage off -> no change");
        assert!(!st.is_on());
    }

    #[test]
    fn auto_engage_trips_only_on_sustained_quiet_when_enabled() {
        let mut cfg = cfg_whisper_on();
        cfg.voice.whisper_auto = true;
        // A SUSTAINED-quiet series engages whisper.
        let quiet = [0.02_f32; WHISPER_SUSTAINED_SAMPLES + 2];
        let mut st = WhisperState::new();
        assert!(st.apply_auto_engage(&cfg, &quiet), "sustained quiet -> engage");
        // A series with even ONE normal-volume sample does NOT trip.
        let mut noisy = vec![0.02_f32; WHISPER_SUSTAINED_SAMPLES + 2];
        noisy[3] = 0.5; // one loud moment
        let mut st2 = WhisperState::new();
        assert!(!st2.apply_auto_engage(&cfg, &noisy), "any loud sample -> no engage");
        // Too-few samples (a brief dip) does NOT trip.
        let short = [0.02_f32; WHISPER_SUSTAINED_SAMPLES - 1];
        let mut st3 = WhisperState::new();
        assert!(!st3.apply_auto_engage(&cfg, &short), "too-short quiet -> no engage");
    }

    #[test]
    fn auto_engage_never_disengages() {
        // Once on, a loud series does NOT auto-turn-it-off (only the explicit
        // command does) — a brief loud moment must not yank discreet mode away.
        let mut cfg = cfg_whisper_on();
        cfg.voice.whisper_auto = true;
        let mut st = WhisperState { on: true } ;
        let loud = [0.9_f32; 10];
        assert!(st.apply_auto_engage(&cfg, &loud), "auto-engage never disengages");
        assert!(st.is_on());
    }

    #[test]
    fn is_sustained_quiet_is_pure_and_conservative() {
        assert!(is_sustained_quiet(&[0.01; WHISPER_SUSTAINED_SAMPLES]));
        assert!(!is_sustained_quiet(&[0.01; WHISPER_SUSTAINED_SAMPLES - 1]), "too short");
        assert!(!is_sustained_quiet(&[]), "empty is never quiet");
        let mut s = vec![0.01_f32; WHISPER_SUSTAINED_SAMPLES];
        s[0] = WHISPER_QUIET_RMS; // exactly at threshold counts as NOT quiet
        assert!(!is_sustained_quiet(&s), "at-threshold sample breaks the quiet run");
    }

    // === #34 PROCESS-GLOBAL: parse -> apply_command_global -> read (router path) =

    #[test]
    fn parse_command_toggles_the_process_global_through_the_router_path() {
        // (f) The exact flow the router's whisper-command handler runs:
        // parse_whisper_command(utterance) -> apply_command_global(cfg, cmd) mutating
        // the PROCESS-GLOBAL, and whisper_state_is_on() reading it back. Feature must
        // be ON for the command to engage (the master switch is honoured inside
        // apply_command). We reset to OFF at the end so the shared global never leaks
        // into another test.
        // Isolate to this thread's seam so the parse->apply->read flow never races the
        // shared process-global another parallel test mutates (the writes route through
        // the seam while it is installed). Starts OFF, matching a fresh global.
        let _g = WhisperGuard::force(false);
        let cfg = cfg_whisper_on();
        // "whisper mode" parses to On and engages the global.
        let on_cmd = parse_whisper_command("please switch to whisper mode").expect("parses On");
        assert_eq!(on_cmd, WhisperCommand::On);
        assert!(apply_command_global(&cfg, on_cmd), "apply engages the global");
        assert!(whisper_state_is_on(), "the global now reads on");
        // "back to normal" parses to Off and disengages it.
        let off_cmd = parse_whisper_command("ok back to normal").expect("parses Off");
        assert_eq!(off_cmd, WhisperCommand::Off);
        assert!(!apply_command_global(&cfg, off_cmd), "apply disengages the global");
        assert!(!whisper_state_is_on(), "the global now reads off");
    }

    #[test]
    fn apply_command_global_is_inert_while_feature_off() {
        // With [voice].whisper OFF (the shipped default) a parsed command can NEVER
        // engage the process-global — apply_command_global honours the master switch.
        // Isolate to this thread's seam (starts OFF) so we read back exactly the write
        // this test made, never a value another parallel global-mutating test set.
        let _g = WhisperGuard::force(false);
        let cfg = cfg_prosody_off(); // whisper = false (explicit off-path)
        let cmd = parse_whisper_command("whisper mode").expect("parses On");
        assert!(!apply_command_global(&cfg, cmd), "feature off -> global stays off");
        assert!(!whisper_state_is_on(), "a stray command is inert while the feature is off");
    }

    #[test]
    fn whisper_guard_forces_the_read_without_touching_the_global() {
        // The test seam mirrors model_tier::OverrideGuard: it forces the READ on this
        // thread (so the inference/speech tests can assert the whisper-on wire) without
        // racing the shared global other tests use.
        {
            let _g = WhisperGuard::force(true);
            assert!(whisper_state_is_on(), "guard forces on");
        }
        // Dropped -> back to the real global (off by default in a fresh test).
        let _g2 = WhisperGuard::force(false);
        assert!(!whisper_state_is_on(), "guard forces off");
    }

    // === Integration: #33 shape + #34 whisper compose, OFF stays neutral ===

    #[test]
    fn full_off_path_is_byte_for_byte_neutral() {
        // Both features off + whisper state off -> the composed shape is the identity.
        // (The shipped DEFAULT is now ON, full-power; this proves the off path still
        // produces today's exact request when an operator disables both features.)
        let cfg = cfg_prosody_off();
        let profile = classify_prosody(ReplyKind::Alert, false); // Urgent
        let shaped = shape_speak_request(&cfg, profile, &el_v3());
        let final_shape = apply_whisper(shaped, /*whisper_on=*/ false, /*required=*/ false);
        assert!(final_shape.is_neutral(), "all-off path must be today's exact request");
    }
}

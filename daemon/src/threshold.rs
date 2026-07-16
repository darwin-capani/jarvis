//! THRESHOLD — a voice-scoped GUEST / restricted-speaker mode.
//!
//! ## What this is
//! When voice-id reports an UNRECOGNIZED speaker (or the owner explicitly turns
//! "guest mode" on), THRESHOLD projects a GUEST SCOPE over the turn:
//!   * a restricted, strictly READ-ONLY tool allowlist (no consequential/outward
//!     tools, no writes) — [`GUEST_READ_ONLY_TOOLS`], intersected with the owner's
//!     own allowlist so it can never NAME a tool the owner lacks;
//!   * recall routed to the SHARED-only namespace — never the owner's private
//!     `agent.*` facts — by REUSING the existing namespace-isolation guard
//!     ([`crate::memory::Memory::agent_scoped_facts`]) with a reserved sentinel
//!     namespace no agent ever writes under, so the guard returns exactly the
//!     shared tier;
//!   * a quieter focus profile (a [`crate::focus::FocusProfile`]), COMPOSED on top
//!     of the owner's active profile through the SAME restrict-only
//!     [`crate::focus::apply_profile`] path, so it can only ever quiet further.
//!
//! ## The sacred invariant: guest scope can ONLY NARROW — it LAYERS ON TOP
//! A guest scope is derived from the owner scope and is provably NO BROADER than
//! it on every axis ([`Scope::is_no_broader_than`], asserted by the property
//! test): its tools are a SUBSET of the owner's, its recall is at least as
//! restricted (shared-only), and its focus profile is at least as quiet. There is
//! NO axis on which a guest scope can loosen anything — [`Scope`] carries only the
//! three restrict-only knobs (tools / shared-recall / profile) and NOTHING that
//! could express "loosen a gate", "raise autonomy", or "enable a consequential
//! action" (the type literally cannot — see `scope_has_only_restrict_only_knobs`).
//!
//! ## HONESTY — guest mode is a COURTESY boundary, never a security backstop
//! Voice-id is a bar-RAISER, not a high-assurance biometric: it rejects an
//! obviously different voice but is REPLAY-/impersonation-SPOOFABLE (see
//! [`crate::voiceid`]). Guest mode is therefore a COURTESY layer stacked ON TOP of
//! — NEVER a replacement for — the real backstops:
//!   * the `[integrations].allow_consequential` MASTER SWITCH,
//!   * the per-action SPOKEN CONFIRM gate ([`crate::confirm`]),
//!   * the voice-id owner gate ([`crate::voiceid::OwnerGate`]),
//!   * and the per-action [`crate::policy`] layer.
//!
//! Those are UNCHANGED whether or not guest mode is on. THRESHOLD holds no handle
//! to any of them and can only ever REMOVE things from the owner scope; a guest
//! turn is thus gated AT LEAST as strictly as an owner turn, never more loosely.
//! The value of guest mode is quieting DARWIN and withholding the owner's private
//! surfaces from a bystander — not gating outward actions (the master switch +
//! confirm gate already do that, spoof-proofed by a fresh human "yes").
//!
//! ## Ships ARMED-but-inert
//! `[threshold].enabled` defaults TRUE (armed): an unrecognized speaker is
//! auto-scoped. But the "unrecognized" signal only exists when voice-id is
//! ENFORCING (enabled + a profile enrolled); with voice-id off — the shipped
//! default — there is no speaker signal, so armed-by-default THRESHOLD is INERT
//! until the owner enrolls a voice OR explicitly toggles guest mode on. With
//! `[threshold].enabled = false` the feature never scopes anything (owner behavior
//! byte-for-byte).
//!
//! This module is a PURE decision seam whose LIVE wiring (the router installing the
//! per-turn guest scope, the recall path reading `shared_recall_only`, the tool
//! loop consulting `read_only_tools`, and the `emit_guest` telemetry call) lands at
//! integration. Until then its public API is unused in the live build, so — exactly
//! like `policy.rs`'s "a shared contract another component reads" rationale — the
//! unused-item lint is allowed module-wide. The invariant lives next to the type it
//! guards; the tests exercise every item.
#![allow(dead_code)]

use std::sync::Mutex;

use serde_json::json;

use crate::focus::{apply_profile, BaseBehavior, FocusProfile, TunedBehavior};

/// The reserved sentinel memory namespace a GUEST recalls under. It is a valid
/// `agent.*`-shaped string that NO enrolled agent (and not the owner) ever writes
/// facts under, chosen deliberately free of SQL `LIKE` metacharacters (`_`, `%`)
/// so it can never wildcard-match a real namespace. Feeding it to the EXISTING
/// own+shared guard [`crate::memory::Memory::agent_scoped_facts`] therefore yields
/// ONLY the shared tier (the `agent.<sentinel>.` own-prefix matches nothing, so
/// just the `NOT LIKE 'agent.%'` shared rows survive) — the honest reuse of the
/// isolation guard, not a second recall path.
pub const GUEST_NAMESPACE: &str = "agent.guest-scope";

/// The tools wildcard the orchestrator (`darwin`) holds. Mirrors
/// `agents::TOOLS_WILDCARD` / [`crate::agents::Agent::may_use`].
const TOOLS_WILDCARD: &str = "*";

/// The CURATED, strictly-READ-ONLY local tool allowlist a guest may use. Every
/// entry runs entirely on-device and is read-only: it stores nothing, sends
/// nothing to the cloud, and takes NO consequential/outward action. This is a
/// STRICT subset of `anthropic::SAFE_LOCAL_TOOLS` with the two non-read entries
/// deliberately DROPPED: `remember_fact` (a durable WRITE) and `skill_invoke` (can
/// dispatch a CONSEQUENTIAL skill). A guest gets to ASK and RETRIEVE, never to
/// change state or reach outward. The guest recall tools additionally read only
/// the SHARED tier (see [`GUEST_NAMESPACE`]).
pub const GUEST_READ_ONLY_TOOLS: &[&str] = &[
    // Memory / semantic recall — read-only retrieval (shared-tier only for a guest).
    "recall_facts",
    "mnemosyne_recall",
    "episodic_recall",
    "user_model_query",
    "world_query",
    // On-device retrieval / search — read-only, on-device. NOTE: `unified_search`
    // is deliberately NOT here — it fans the query out to the OWNER's connected
    // Gmail / Calendar / Slack and returns their private data, so it is neither
    // on-device nor safe to expose to a bystander (it is not in SAFE_LOCAL_TOOLS).
    "doc_search",
    "search_files",
    // Read-only status + the skill CATALOG (listing, not invocation).
    "system_status",
    "skill_list",
];

/// The per-turn SPEAKER signal THRESHOLD reasons over, derived from the voice-id
/// owner gate. Honest about the three distinct states so auto-guest triggers ONLY
/// on a real "unrecognized" reading, never on the mere ABSENCE of voice-id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeakerState {
    /// Voice-id is ENFORCING and this turn verified as the enrolled owner.
    OwnerVerified,
    /// Voice-id is ENFORCING but this turn did NOT verify — an unrecognized
    /// speaker (also the fail-closed "no usable audio while enforcing" case). This
    /// is the reading that AUTO-scopes to guest.
    Unrecognized,
    /// Voice-id is NOT enforcing (disabled, or no profile enrolled) — there is no
    /// speaker signal at all. Auto-guest does NOT trigger from this; only an
    /// explicit guest toggle does.
    Unenforced,
}

impl SpeakerState {
    /// Derive the speaker state from the voice-id [`crate::voiceid::OwnerGate`] —
    /// the SAME gate the consequential chokepoints read. `enforcing && verified`
    /// is the owner; `enforcing && !verified` is an unrecognized speaker (the
    /// fail-closed reading); `!enforcing` is no signal. This is the seam that ties
    /// THRESHOLD to the existing fail-closed voice-id gate WITHOUT duplicating its
    /// logic.
    pub fn from_owner_gate(gate: &crate::voiceid::OwnerGate) -> Self {
        if !gate.enforcing {
            SpeakerState::Unenforced
        } else if gate.verified {
            SpeakerState::OwnerVerified
        } else {
            SpeakerState::Unrecognized
        }
    }

    /// Whether this reading, on its own, auto-scopes to guest (an unrecognized
    /// enforcing turn). An explicit guest toggle scopes regardless of this.
    fn auto_scopes(self) -> bool {
        matches!(self, SpeakerState::Unrecognized)
    }
}

/// Why guest scope is (in)active this turn — a stable, secret-free telemetry
/// token. Never carries audio, a score, or any speaker identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestReason {
    /// The owner path: guest scope is NOT active (recognized owner, or no signal
    /// and no explicit toggle). The owner scope is used unchanged.
    Owner,
    /// Guest scope active because voice-id reported an UNRECOGNIZED speaker.
    Unrecognized,
    /// Guest scope active because the owner EXPLICITLY toggled guest mode on.
    Explicit,
    /// `[threshold].enabled = false` — the feature is off; owner scope is used
    /// unchanged (even an explicit toggle is ignored while disabled).
    Disabled,
}

impl GuestReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            GuestReason::Owner => "owner",
            GuestReason::Unrecognized => "unrecognized_speaker",
            GuestReason::Explicit => "explicit_guest",
            GuestReason::Disabled => "disabled",
        }
    }
}

/// The effective SCOPE a turn runs under. The WHOLE surface THRESHOLD projects —
/// and notice what is NOT here: no gate, no confirm, no master switch, no voice-id
/// verdict, no policy, no autonomy level, no consequential-action flag. A `Scope`
/// literally cannot express "loosen a gate" or "enable a side effect", so a guest
/// `Scope` derived from an owner `Scope` can only ever be a RESTRICTION of it.
///
/// The three restrict-only knobs:
///   * `tools` — the tool allowlist (`["*"]` = the orchestrator wildcard).
///   * `shared_recall_only` — when true, recall is confined to the SHARED tier
///     (via [`GUEST_NAMESPACE`]); when false, the owner's normal own+shared recall.
///   * `profile` — the focus lens applied to (composed onto) the base behavior.
#[derive(Debug, Clone, PartialEq)]
pub struct Scope {
    pub tools: Vec<String>,
    pub shared_recall_only: bool,
    pub profile: FocusProfile,
}

impl Scope {
    /// The OWNER scope: the given tool allowlist + focus profile, with FULL
    /// (own+shared) recall. This is the base every guest scope narrows FROM.
    pub fn owner(tools: Vec<String>, profile: FocusProfile) -> Self {
        Scope {
            tools,
            shared_recall_only: false,
            profile,
        }
    }

    /// Whether `tool` is admitted by this scope's allowlist — the wildcard admits
    /// everything, else exact membership. Mirrors [`crate::agents::Agent::may_use`]
    /// so guest tool admission uses the IDENTICAL rule as the live allowlist gate.
    pub fn admits(&self, tool: &str) -> bool {
        admits(&self.tools, tool)
    }

    /// The memory namespace this scope recalls under, GIVEN the owner's active
    /// namespace. A shared-only (guest) scope recalls under the reserved
    /// [`GUEST_NAMESPACE`] sentinel — so the EXISTING own+shared guard returns only
    /// the shared tier; an owner scope recalls under the owner's own namespace
    /// (own + shared, exactly as today).
    pub fn recall_namespace<'a>(&self, owner_namespace: &'a str) -> &'a str {
        if self.shared_recall_only {
            GUEST_NAMESPACE
        } else {
            owner_namespace
        }
    }

    /// The focus behavior this scope yields when COMPOSED on top of `base`. For an
    /// owner scope `base` is the raw [`BaseBehavior`]; for a guest scope `base`
    /// should be the OWNER's tuned behavior (`owner_tuned.as_base()`), so the guest
    /// profile composes ON TOP and can only narrow further — the same composition
    /// Auto-Focus uses.
    pub fn behavior(&self, base: &BaseBehavior) -> TunedBehavior {
        apply_profile(&self.profile, base)
    }

    /// THE machine-checkable RESTRICT-ONLY predicate: is this scope NO BROADER than
    /// `owner` on every axis, evaluated against `base`? True iff
    ///   * every tool it admits is also admitted by the owner (tools ⊆ owner);
    ///   * its recall is at least as confined (`shared_recall_only` cannot go from
    ///     the owner's true to a looser false);
    ///   * its focus profile, COMPOSED on top of the owner's, is no broader than
    ///     the owner's tuned behavior (always holds by [`apply_profile`]'s
    ///     construction — asserted here so a regression would be caught).
    ///
    /// Because [`Scope`] has no gate/permission/autonomy field, "no broader" on
    /// these three axes is the COMPLETE statement of "this scope loosened nothing".
    /// The property test asserts it for every derived guest scope.
    pub fn is_no_broader_than(&self, owner: &Scope, base: &BaseBehavior) -> bool {
        let tools_subset = self.tools.iter().all(|t| owner.admits(t));
        // shared_recall_only may only tighten (false->true) or hold, never loosen
        // (true->false): the owner's confinement, if any, must be preserved.
        let recall_no_broader = !owner.shared_recall_only || self.shared_recall_only;
        let owner_tuned = apply_profile(&owner.profile, base);
        let self_tuned = apply_profile(&self.profile, &owner_tuned.as_base());
        let profile_no_broader = self_tuned.is_no_broader_than(&owner_tuned.as_base());
        tools_subset && recall_no_broader && profile_no_broader
    }
}

/// The decision THRESHOLD renders for one turn: whether guest scope is active, WHY
/// (secret-free), and the effective [`Scope`] to run under (the owner scope
/// unchanged, or the narrowed guest scope).
#[derive(Debug, Clone, PartialEq)]
pub struct GuestDecision {
    pub active: bool,
    pub reason: GuestReason,
    pub scope: Scope,
}

/// The PURE seam: given the speaker signal, an explicit guest toggle, the
/// `[threshold]` config, and the OWNER scope this turn would run under, decide the
/// effective scope. Total and side-effect-free (no I/O, no gate, no clock), so the
/// whole decision is unit-testable.
///
/// Rules:
///   * `[threshold].enabled = false` -> owner scope unchanged (reason `Disabled`).
///   * else guest is active iff `guest_flag` OR the speaker is `Unrecognized`;
///     an explicit flag names the reason `Explicit`, else `Unrecognized`.
///   * an active guest scope is [`Scope::guest_from`] the owner (tools narrowed to
///     the read-only intersection, recall shared-only, the configured quiet
///     profile) — provably NO BROADER than the owner.
///   * otherwise the OWNER scope is returned BYTE-FOR-BYTE (reason `Owner`), so a
///     recognized owner's turn is never altered by this feature.
pub fn decide(
    speaker: SpeakerState,
    guest_flag: bool,
    cfg: &ThresholdConfigView,
    owner: &Scope,
) -> GuestDecision {
    if !cfg.enabled {
        return GuestDecision {
            active: false,
            reason: GuestReason::Disabled,
            scope: owner.clone(),
        };
    }
    let reason = if guest_flag {
        GuestReason::Explicit
    } else if speaker.auto_scopes() {
        GuestReason::Unrecognized
    } else {
        GuestReason::Owner
    };
    match reason {
        GuestReason::Owner | GuestReason::Disabled => GuestDecision {
            active: false,
            reason: GuestReason::Owner,
            scope: owner.clone(),
        },
        GuestReason::Unrecognized | GuestReason::Explicit => GuestDecision {
            active: true,
            reason,
            scope: guest_from(owner, &cfg.guest_profile),
        },
    }
}

/// Derive the GUEST scope from the owner scope + the configured quiet profile:
/// tools narrowed to the READ-ONLY intersection, recall shared-only, and the
/// (quiet) guest focus profile. The result is provably NO BROADER than `owner`
/// (see [`Scope::is_no_broader_than`]).
pub fn guest_from(owner: &Scope, guest_profile: &FocusProfile) -> Scope {
    Scope {
        tools: guest_read_only_tools(&owner.tools),
        shared_recall_only: true,
        profile: guest_profile.clone(),
    }
}

/// The guest tool allowlist: the curated [`GUEST_READ_ONLY_TOOLS`] INTERSECTED
/// with what the owner may already use. Intersecting (never unioning) is the
/// safety property — the result can only ever NARROW the owner's tools: a tool the
/// owner lacks is dropped, and a tool outside the read-only set is dropped, so a
/// guest can never gain a capability the owner lacks nor a non-read-only one.
/// Order follows the curated list for a stable telemetry frame.
pub fn guest_read_only_tools(owner_tools: &[String]) -> Vec<String> {
    GUEST_READ_ONLY_TOOLS
        .iter()
        .filter(|t| admits(owner_tools, t))
        .map(|t| (*t).to_string())
        .collect()
}

/// Whether `allowed` admits `tool` — the wildcard admits everything, else exact
/// membership. Mirrors [`crate::agents::Agent::may_use`] / `anthropic::agent_may_use`
/// so THRESHOLD narrows tools by the IDENTICAL rule the live allowlist gate uses.
fn admits(allowed: &[String], tool: &str) -> bool {
    allowed.iter().any(|t| t == TOOLS_WILDCARD || t == tool)
}

/// A resolved, borrow-free view of `[threshold]` config the pure [`decide`] seam
/// reasons over (so the decision never depends on the on-disk `String` shape). The
/// caller builds it once per turn from [`crate::config::ThresholdConfig`].
#[derive(Debug, Clone, PartialEq)]
pub struct ThresholdConfigView {
    /// Master switch (armed-by-default). False => guest scope never applies.
    pub enabled: bool,
    /// The (quiet) focus profile a guest turn uses, already parsed. Restrict-only
    /// by construction ([`crate::focus::apply_profile`]), so ANY value can only
    /// quiet — never broaden.
    pub guest_profile: FocusProfile,
}

impl ThresholdConfigView {
    /// Resolve the runtime view from the on-disk `[threshold]` config: parse the
    /// `guest_profile` string via [`crate::focus::FocusProfile::from_config_str`]
    /// (which is restrict-only for ANY string, so a typo can only ever quiet).
    pub fn from_config(cfg: &crate::config::ThresholdConfig) -> Self {
        ThresholdConfigView {
            enabled: cfg.enabled,
            guest_profile: FocusProfile::from_config_str(&cfg.guest_profile),
        }
    }
}

/// The secret-free `threshold.guest` telemetry frame: whether guest scope is
/// active, WHY, the read-only tool set, the shared-recall flag, the quiet profile,
/// and — made EXPLICIT on the wire so the HUD copy is grounded — the restrict-only
/// posture (guest mode loosens no gate, raises no autonomy). Carries NO audio, NO
/// score, NO speaker identity, NO private fact.
pub fn guest_telemetry(decision: &GuestDecision) -> serde_json::Value {
    json!({
        "guest_active": decision.active,
        "reason": decision.reason.as_str(),
        "read_only_tools": decision.scope.tools,
        "shared_recall_only": decision.scope.shared_recall_only,
        "profile": decision.scope.profile.as_str(),
        // The contract, stated on the wire (mirrors focus's permission-neutral
        // posture): guest scope can only RESTRICT — it is a courtesy layer on top
        // of the unchanged master switch + confirm + voice-id + policy gates.
        "restrict_only": true,
        "loosens_gate": false,
        "raises_autonomy": false,
    })
}

/// Emit the `threshold.guest` frame for the HUD. Thin live-side wrapper over
/// [`guest_telemetry`]; the pure builder is what the tests pin. This is the live
/// emit seam the router calls once per turn after [`decide`].
pub fn emit_guest(decision: &GuestDecision) {
    crate::telemetry::emit("threshold", "threshold.guest", guest_telemetry(decision));
}

// ---------------------------------------------------------------------------
// The per-turn GUEST SCOPE — how the installed [`Scope`] threads into the deep
// recall dispatch, the tool loop, and the anticipation tick WITHOUT parameter
// threading, EXACTLY mirroring `voiceid`'s per-turn `TURN_GATE` global.
// ---------------------------------------------------------------------------

/// Process-global current-turn GUEST SCOPE. `None` = no guest scope installed
/// this turn, which reads as the OWNER PATH (no restriction): the recall
/// dispatch uses the owner namespace, the tool loop offers the full agent
/// allowlist, and the anticipation tick uses the owner's tuned behavior — all
/// byte-for-byte unchanged. `Some(scope)` = a guest turn is scoped by `scope`.
///
/// Set once per turn near the top of `run_pipeline` (after voice-id, via
/// [`set_turn_scope`] on an active guest decision, else [`clear_turn_scope`]),
/// read at the deep recall/tool-loop sites + the anticipation tick
/// ([`current_turn_scope`]), and REPLACED every full turn so an owner turn never
/// inherits a stale guest scope. Mirrors `voiceid::TURN_GATE` (a `Scope` is not
/// `Copy`, so this is a `Mutex<Option<Scope>>` cloned on read rather than a
/// `Cell`).
static TURN_SCOPE: Mutex<Option<Scope>> = Mutex::new(None);

// Test-only thread-local override, mirroring `voiceid`'s `GATE_OVERRIDE`: a test
// forces the current-turn scope on its OWN thread without touching the
// process-global slot other (parallel) tests read. The outer `Option` is "is an
// override installed", the inner `Option<Scope>` is the forced value (Some =
// guest scope, None = owner path). Compiled out in release.
#[cfg(test)]
thread_local! {
    static SCOPE_OVERRIDE: std::cell::RefCell<Option<Option<Scope>>> =
        const { std::cell::RefCell::new(None) };
}

/// Install THIS turn's guest scope (called once near the top of `run_pipeline`
/// when the decision is active). Poison-tolerant.
pub fn set_turn_scope(scope: Scope) {
    *TURN_SCOPE.lock().unwrap_or_else(|p| p.into_inner()) = Some(scope);
}

/// Clear the per-turn guest scope (called on an OWNER-path turn, and any time no
/// guest scope should be in force) so a later turn never inherits a stale guest
/// scope. Poison-tolerant.
pub fn clear_turn_scope() {
    *TURN_SCOPE.lock().unwrap_or_else(|p| p.into_inner()) = None;
}

/// The current turn's installed guest scope — `None` (the OWNER path) when none
/// is installed. This is the deep read consulted by the recall dispatch, the
/// tool loop, and the anticipation tick.
pub fn current_turn_scope() -> Option<Scope> {
    #[cfg(test)]
    {
        if let Some(over) = SCOPE_OVERRIDE.with(|c| c.borrow().clone()) {
            return over;
        }
    }
    TURN_SCOPE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
}

/// The memory namespace to recall under THIS turn, honoring an installed guest
/// scope: when a guest scope is active the reserved shared-only [`GUEST_NAMESPACE`]
/// sentinel (so the EXISTING own+shared guard returns only the SHARED tier), else
/// the owner namespace UNCHANGED. This is the single seam the live recall
/// dispatch (`grounded_facts`, the `recall_facts` / `mnemosyne_recall` /
/// `episodic_recall` tools, `router::agent_facts`) calls so a guest NEVER reads
/// the owner's private `agent.*` facts — while the owner path is byte-for-byte
/// today's (returns `owner_namespace` verbatim).
pub fn recall_namespace_for_turn(owner_namespace: &str) -> String {
    match current_turn_scope() {
        Some(scope) => scope.recall_namespace(owner_namespace).to_string(),
        None => owner_namespace.to_string(),
    }
}

/// `#[cfg(test)]`-only RAII guard forcing [`current_turn_scope`] to a value on the
/// current thread, restoring the prior state on drop (so the override never leaks
/// into another test). Mirrors `voiceid::GateOverride`; the whole seam is
/// `cfg(test)`. Tests use THIS (never [`set_turn_scope`]) so the process-global
/// slot the live path uses stays untouched across parallel tests.
#[cfg(test)]
pub(crate) struct ScopeOverride {
    prev: Option<Option<Scope>>,
}

#[cfg(test)]
impl ScopeOverride {
    /// Force a GUEST scope in force on this thread.
    pub(crate) fn guest(scope: Scope) -> Self {
        let prev = SCOPE_OVERRIDE.with(|c| c.replace(Some(Some(scope))));
        Self { prev }
    }

    /// Force the OWNER path (no guest scope) on this thread.
    pub(crate) fn owner() -> Self {
        let prev = SCOPE_OVERRIDE.with(|c| c.replace(Some(None)));
        Self { prev }
    }
}

#[cfg(test)]
impl Drop for ScopeOverride {
    fn drop(&mut self) {
        SCOPE_OVERRIDE.with(|c| *c.borrow_mut() = self.prev.take());
    }
}

// ---------------------------------------------------------------------------
// The explicit "guest mode on/off" toggle — a CONSERVATIVE, anchored-imperative
// spoken classifier (mirrors `vault::classify_vault_command`). An ordinary
// sentence that merely MENTIONS "guest mode" never toggles it.
// ---------------------------------------------------------------------------

/// An explicit guest-mode toggle parsed from a spoken utterance. Only these
/// anchored imperative phrasings ever flip guest mode; a bare mention never does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestToggle {
    /// "guest mode on" / "enable guest mode" / … — hand the mic to a guest.
    On,
    /// "guest mode off" / "disable guest mode" / … — the owner takes the mic back.
    Off,
}

/// Normalize an utterance for anchored matching: lowercase, strip surrounding
/// whitespace + trailing sentence punctuation, and collapse internal whitespace
/// runs to single spaces. Pure. Mirrors `vault::normalize`.
fn normalize(text: &str) -> String {
    let lowered = text.trim().trim_end_matches(['.', '!', '?', ',']).to_lowercase();
    lowered.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether the normalized utterance IS one of `phrases` — the whole thing or its
/// leading imperative (so "guest mode on please" matches, but a sentence that
/// merely mentions guest mode does not). Mirrors `vault::matches_phrase`.
fn matches_phrase(norm: &str, phrases: &[&str]) -> bool {
    phrases.iter().any(|p| {
        norm == *p
            || norm
                .strip_prefix(p)
                .is_some_and(|rest| rest.starts_with(' '))
    })
}

/// The OFF anchor phrases — checked FIRST so a "guest mode off" utterance (which
/// contains "guest mode") never reads as ON. Mirrors vault's off-precedence.
const GUEST_OFF_PHRASES: &[&str] = &[
    "guest mode off",
    "turn off guest mode",
    "disable guest mode",
    "exit guest mode",
    "leave guest mode",
    "end guest mode",
];

/// The ON anchor phrases. NOTE: the BARE "guest mode" is deliberately absent —
/// `matches_phrase` treats a phrase as a leading imperative, so "guest mode, what
/// is that?" would otherwise engage guest mode when the user is merely ASKING. An
/// intentional toggle uses an explicit verb / on-off form.
const GUEST_ON_PHRASES: &[&str] = &[
    "guest mode on",
    "turn on guest mode",
    "enable guest mode",
    "enter guest mode",
    "start guest mode",
];

/// CONSERVATIVELY classify a spoken guest-mode toggle. Anchored on the imperative
/// phrase set (an ordinary sentence that merely mentions "guest" never triggers),
/// with OFF taking precedence over ON. `None` for anything that is not a clear
/// toggle. PURE — the boundary is unit-tested. Handled BEFORE normal routing, the
/// exact discipline `vault::classify_vault_command` / `voiceid::classify_intent`
/// use.
pub fn classify_guest_toggle(text: &str) -> Option<GuestToggle> {
    let norm = normalize(text);
    if norm.is_empty() {
        return None;
    }
    if matches_phrase(&norm, GUEST_OFF_PHRASES) {
        return Some(GuestToggle::Off);
    }
    if matches_phrase(&norm, GUEST_ON_PHRASES) {
        return Some(GuestToggle::On);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::focus::SignalCategory;
    use crate::voiceid::{GateScope, OwnerGate};

    /// A representative OWNER scope: the orchestrator wildcard + the identity
    /// (Default) focus profile. Its guest projection is the widest possible (the
    /// whole read-only set), which is the sharpest test of the subset invariant.
    fn orchestrator_owner() -> Scope {
        Scope::owner(vec!["*".to_string()], FocusProfile::Default)
    }

    /// A representative SPECIALIST owner scope: a finite allowlist mixing read-only
    /// tools with consequential/outward ones. Its guest projection must keep ONLY
    /// the read-only ones it already holds.
    fn specialist_owner() -> Scope {
        Scope::owner(
            vec![
                "recall_facts".to_string(),
                "doc_search".to_string(),
                "gmail_send".to_string(), // consequential/outward — dropped for guest
                "remember_fact".to_string(), // a write — dropped for guest
                "shell_run".to_string(),  // maximally dangerous — dropped for guest
            ],
            FocusProfile::Work,
        )
    }

    fn armed_cfg() -> ThresholdConfigView {
        ThresholdConfigView {
            enabled: true,
            guest_profile: FocusProfile::DeepFocus,
        }
    }

    // =====================================================================
    // THE SCOPE DECISION
    // =====================================================================

    #[test]
    fn recognized_owner_gets_the_full_owner_scope_unchanged() {
        // A recognized owner turn is returned BYTE-FOR-BYTE — guest mode never
        // alters the owner's path.
        let owner = orchestrator_owner();
        let d = decide(SpeakerState::OwnerVerified, false, &armed_cfg(), &owner);
        assert!(!d.active, "recognized owner is not scoped to guest");
        assert_eq!(d.reason, GuestReason::Owner);
        assert_eq!(d.scope, owner, "owner scope returned unchanged");
    }

    #[test]
    fn unrecognized_speaker_is_auto_scoped_to_read_only_shared_recall_quiet() {
        let owner = orchestrator_owner();
        let d = decide(SpeakerState::Unrecognized, false, &armed_cfg(), &owner);
        assert!(d.active, "an unrecognized speaker is auto-scoped to guest");
        assert_eq!(d.reason, GuestReason::Unrecognized);
        // READ-ONLY: the guest tools are exactly the curated read-only set (the
        // orchestrator holds the wildcard, so the intersection is the whole set).
        assert_eq!(
            d.scope.tools,
            GUEST_READ_ONLY_TOOLS.iter().map(|t| t.to_string()).collect::<Vec<_>>(),
            "guest gets the full read-only set under a wildcard owner"
        );
        // SHARED-RECALL-ONLY.
        assert!(d.scope.shared_recall_only, "guest recall is shared-only");
        // QUIET profile (the configured DeepFocus).
        assert_eq!(d.scope.profile, FocusProfile::DeepFocus);
    }

    #[test]
    fn explicit_guest_toggle_scopes_even_for_a_recognized_owner() {
        // The owner can hand the mic to a guest explicitly, even while recognized.
        let owner = orchestrator_owner();
        let d = decide(SpeakerState::OwnerVerified, true, &armed_cfg(), &owner);
        assert!(d.active, "an explicit guest toggle scopes regardless of speaker");
        assert_eq!(d.reason, GuestReason::Explicit);
        assert!(d.scope.shared_recall_only);
        assert_eq!(d.scope.profile, FocusProfile::DeepFocus);
    }

    #[test]
    fn unenforced_voiceid_without_a_flag_stays_owner() {
        // With voice-id NOT enforcing (off / unenrolled) there is NO "unrecognized"
        // signal, so armed-by-default THRESHOLD is INERT unless the owner toggles.
        let owner = orchestrator_owner();
        let d = decide(SpeakerState::Unenforced, false, &armed_cfg(), &owner);
        assert!(!d.active, "no speaker signal + no toggle -> owner scope");
        assert_eq!(d.reason, GuestReason::Owner);
        assert_eq!(d.scope, owner);
        // ...but an explicit toggle still works with voice-id off.
        let d2 = decide(SpeakerState::Unenforced, true, &armed_cfg(), &owner);
        assert!(d2.active, "an explicit toggle works even with voice-id off");
        assert_eq!(d2.reason, GuestReason::Explicit);
    }

    #[test]
    fn disabled_threshold_never_scopes_even_an_unrecognized_speaker() {
        let owner = orchestrator_owner();
        let cfg = ThresholdConfigView { enabled: false, guest_profile: FocusProfile::DeepFocus };
        for (speaker, flag) in [
            (SpeakerState::Unrecognized, false),
            (SpeakerState::Unrecognized, true),
            (SpeakerState::OwnerVerified, true),
        ] {
            let d = decide(speaker, flag, &cfg, &owner);
            assert!(!d.active, "disabled threshold never scopes ({speaker:?}, flag={flag})");
            assert_eq!(d.reason, GuestReason::Disabled);
            assert_eq!(d.scope, owner, "disabled -> owner scope byte-for-byte");
        }
    }

    #[test]
    fn specialist_guest_keeps_only_its_own_read_only_tools() {
        // A specialist owner's guest projection is the intersection: it keeps only
        // the read-only tools the owner ALREADY held (recall_facts, doc_search) and
        // drops the consequential/write ones (gmail_send, remember_fact, shell_run).
        let owner = specialist_owner();
        let d = decide(SpeakerState::Unrecognized, false, &armed_cfg(), &owner);
        assert!(d.active);
        assert_eq!(
            d.scope.tools,
            vec!["recall_facts".to_string(), "doc_search".to_string()],
            "guest keeps only the read-only tools the owner held"
        );
    }

    // =====================================================================
    // THE RESTRICT-ONLY INVARIANT: guest ⊆ owner
    // =====================================================================

    /// A spread of owner scopes for the property sweep: the orchestrator, a
    /// specialist, and an owner already on a quiet profile.
    fn owner_scopes() -> Vec<Scope> {
        vec![
            orchestrator_owner(),
            specialist_owner(),
            Scope::owner(
                vec!["recall_facts".to_string(), "system_status".to_string()],
                FocusProfile::Sleep,
            ),
            // An owner that itself already recalls shared-only (an edge case the
            // guest must still never loosen back to full recall).
            Scope {
                tools: vec!["*".to_string()],
                shared_recall_only: true,
                profile: FocusProfile::DeepFocus,
            },
        ]
    }

    fn guest_profiles() -> Vec<FocusProfile> {
        vec![
            FocusProfile::DeepFocus,
            FocusProfile::Sleep,
            FocusProfile::Work,
            FocusProfile::Default, // even the IDENTITY guest profile must not broaden
            FocusProfile::from_config_str("study"), // a named custom profile
        ]
    }

    #[test]
    fn property_a_guest_scope_is_never_broader_than_its_owner() {
        // THE restrict-only gate, machine-checked: for EVERY owner scope, EVERY
        // guest profile, and a spread of bases, the derived guest scope is NO
        // BROADER than the owner on every axis (tools ⊆ owner, recall at least as
        // confined, focus at least as quiet).
        let bases = [
            BaseBehavior::default(),
            // An already-narrowed base (composition must still only narrow).
            apply_profile(&FocusProfile::Work, &BaseBehavior::default()).as_base(),
        ];
        for owner in owner_scopes() {
            for gp in guest_profiles() {
                let guest = guest_from(&owner, &gp);
                for base in &bases {
                    assert!(
                        guest.is_no_broader_than(&owner, base),
                        "guest {guest:?} (profile {gp:?}) broadened owner {owner:?} over base {base:?}"
                    );
                    // Tools are strictly a SUBSET (every guest tool is owner-admitted).
                    for t in &guest.tools {
                        assert!(owner.admits(t), "guest tool {t} not admitted by owner {owner:?}");
                    }
                    // Recall never loosens: a shared-only owner stays shared-only.
                    if owner.shared_recall_only {
                        assert!(guest.shared_recall_only, "guest un-confined a shared-only owner");
                    }
                }
            }
        }
    }

    #[test]
    fn guest_read_only_tools_intersects_never_unions() {
        // Wildcard owner -> the whole read-only set.
        let full = guest_read_only_tools(&["*".to_string()]);
        assert_eq!(full.len(), GUEST_READ_ONLY_TOOLS.len());
        // A narrow owner -> only the overlap, and NEVER a tool the owner lacks.
        let narrow = guest_read_only_tools(&[
            "doc_search".to_string(),
            "gmail_send".to_string(), // not read-only -> not admitted into the guest set
        ]);
        assert_eq!(narrow, vec!["doc_search".to_string()]);
        // An owner with NO read-only tools -> an empty guest set (nothing to grant).
        assert!(guest_read_only_tools(&["gmail_send".to_string()]).is_empty());
    }

    // =====================================================================
    // THE OWNER'S CONSEQUENTIAL GATE IS UNCHANGED BY GUEST MODE
    // =====================================================================

    #[test]
    fn the_guest_read_only_set_contains_no_consequential_or_write_tool() {
        // Guest mode can only ever REMOVE tools — it never grants a consequential,
        // outward, or write tool. The curated read-only set is DISJOINT from every
        // known consequential/outward/write tool (incl. the two maximally-dangerous
        // ones and the two SAFE_LOCAL_TOOLS entries deliberately dropped).
        const FORBIDDEN: &[&str] = &[
            "gmail_send", "slack_post_message", "x_post", "dume_control",
            "ui_actuate", "shell_run", // policy::NEVER_AUTO_APPROVE_TOOLS
            "remember_fact", "skill_invoke", // the two SAFE_LOCAL_TOOLS non-reads, dropped
            "world_record", "user_model_correct", "standing_create", "open_url",
        ];
        for f in FORBIDDEN {
            assert!(
                !GUEST_READ_ONLY_TOOLS.contains(f),
                "the guest read-only set must NOT contain the consequential/write tool {f}"
            );
        }
        // And the maximally-dangerous tools are absent by construction.
        for t in crate::policy::NEVER_AUTO_APPROVE_TOOLS {
            assert!(!GUEST_READ_ONLY_TOOLS.contains(t), "{t} must never be a guest tool");
        }
    }

    #[test]
    fn guest_mode_holds_no_gate_and_cannot_touch_the_consequential_path() {
        // STRUCTURAL: a `Scope` — all guest mode can produce — carries ONLY the
        // three restrict-only knobs. There is no `.gate`, `.confirm`,
        // `.allow_consequential`, `.master`, `.voice_id`, `.policy`, `.autonomy`
        // field, so a guest scope CANNOT express "loosen a gate" or "enable a
        // consequential action". If a future edit added such a field, this
        // exhaustive destructuring would fail to compile, forcing a re-review.
        let d = decide(SpeakerState::Unrecognized, false, &armed_cfg(), &orchestrator_owner());
        let Scope { tools: _, shared_recall_only: _, profile: _ } = d.scope;

        // And the OWNER's voice-id consequential gate is evaluated by voiceid.rs,
        // untouched by THRESHOLD: an OwnerGate's verdict is identical no matter what
        // guest mode decided. A verified owner may still fire a consequential action
        // exactly as before; an unrecognized speaker is already denied there — guest
        // mode neither adds nor removes that.
        let verified = OwnerGate { enforcing: true, verified: true, scope: GateScope::Consequential };
        assert!(verified.allow_consequential(), "owner consequential gate is unchanged by guest mode");
        let unrecognized = OwnerGate { enforcing: true, verified: false, scope: GateScope::Consequential };
        assert!(!unrecognized.allow_consequential(), "the voice-id gate still denies an unrecognized speaker");
        // The two are the SAME whether or not a guest scope was projected — the
        // decision above installed a guest scope yet the gate verdicts are unmoved.
    }

    #[test]
    fn scope_has_only_restrict_only_knobs() {
        // A standing assertion (read with the struct def): the ONLY things readable
        // off a Scope are the three NON-consequential knobs. The exhaustive pattern
        // IS the proof — there is no permission/gate/autonomy axis on which a guest
        // scope could loosen anything.
        let s = guest_from(&orchestrator_owner(), &FocusProfile::DeepFocus);
        let Scope { tools, shared_recall_only, profile } = s;
        assert!(!tools.is_empty());
        assert!(shared_recall_only);
        assert_eq!(profile, FocusProfile::DeepFocus);
    }

    // =====================================================================
    // SPEAKER-STATE mapping from the voice-id gate
    // =====================================================================

    #[test]
    fn speaker_state_maps_from_the_owner_gate() {
        // enforcing + verified -> owner; enforcing + !verified -> unrecognized;
        // !enforcing -> unenforced (no signal). Ties THRESHOLD to the SAME gate the
        // consequential chokepoints read.
        let owner = OwnerGate { enforcing: true, verified: true, scope: GateScope::Consequential };
        assert_eq!(SpeakerState::from_owner_gate(&owner), SpeakerState::OwnerVerified);
        let unk = OwnerGate { enforcing: true, verified: false, scope: GateScope::Consequential };
        assert_eq!(SpeakerState::from_owner_gate(&unk), SpeakerState::Unrecognized);
        assert_eq!(SpeakerState::from_owner_gate(&OwnerGate::OFF), SpeakerState::Unenforced);
        // The OFF gate (voice-id disabled/unenrolled) yields NO signal -> Unenforced,
        // so armed-by-default THRESHOLD stays inert until a voice is enrolled.
    }

    // =====================================================================
    // RECALL ROUTING: reuse of the existing namespace-isolation guard
    // =====================================================================

    #[test]
    fn guest_recall_namespace_is_the_sentinel_owner_is_the_owner_namespace() {
        let owner = orchestrator_owner();
        assert_eq!(owner.recall_namespace("agent.darwin"), "agent.darwin", "owner recalls under its own ns");
        let guest = guest_from(&owner, &FocusProfile::DeepFocus);
        assert_eq!(guest.recall_namespace("agent.darwin"), GUEST_NAMESPACE, "guest recalls under the sentinel");
        // The sentinel is free of SQL LIKE metacharacters so it can never wildcard.
        assert!(!GUEST_NAMESPACE.contains('_'), "sentinel must avoid the LIKE '_' wildcard");
        assert!(!GUEST_NAMESPACE.contains('%'), "sentinel must avoid the LIKE '%' wildcard");
    }

    #[tokio::test]
    async fn guest_recall_sees_only_shared_facts_via_the_existing_guard() {
        // END-TO-END proof that routing a guest through the reserved sentinel
        // namespace and the EXISTING own+shared guard yields SHARED-ONLY recall —
        // never the owner's private `agent.*` facts.
        let path = std::env::temp_dir().join(format!("darwin-threshold-recall-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mem = crate::memory::Memory::open(&path).expect("open temp memory");

        // A shared fact (common knowledge) + the OWNER's private namespaced fact.
        mem.upsert_fact("user.name", "Darwin").await.unwrap();
        mem.upsert_fact("agent.darwin.secret_note", "the owner's private note").await.unwrap();

        // The OWNER (recalling under its own namespace) sees BOTH.
        let owner_ns = orchestrator_owner();
        let owner_view = mem
            .agent_scoped_facts(owner_ns.recall_namespace("agent.darwin"), 50)
            .await
            .unwrap();
        let owner_keys: Vec<&str> = owner_view.iter().map(|(k, _)| k.as_str()).collect();
        assert!(owner_keys.contains(&"user.name"));
        assert!(owner_keys.contains(&"agent.darwin.secret_note"), "owner sees its private fact");

        // The GUEST (recalling under the sentinel) sees ONLY the shared fact — the
        // owner's private `agent.darwin.*` note is invisible.
        let guest = guest_from(&owner_ns, &FocusProfile::DeepFocus);
        let guest_view = mem
            .agent_scoped_facts(guest.recall_namespace("agent.darwin"), 50)
            .await
            .unwrap();
        let guest_keys: Vec<&str> = guest_view.iter().map(|(k, _)| k.as_str()).collect();
        assert!(guest_keys.contains(&"user.name"), "guest sees shared knowledge");
        assert!(
            !guest_keys.contains(&"agent.darwin.secret_note"),
            "guest MUST NOT see the owner's private fact: {guest_keys:?}"
        );

        let _ = std::fs::remove_file(&path);
    }

    // =====================================================================
    // TELEMETRY shape
    // =====================================================================

    #[test]
    fn telemetry_states_the_restrict_only_posture_and_is_secret_free() {
        let d = decide(SpeakerState::Unrecognized, false, &armed_cfg(), &orchestrator_owner());
        let v = guest_telemetry(&d);
        assert_eq!(v["guest_active"], true);
        assert_eq!(v["reason"], "unrecognized_speaker");
        assert_eq!(v["shared_recall_only"], true);
        assert_eq!(v["profile"], "deep_focus");
        // The contract on the wire so the HUD copy is grounded, not hardcoded.
        assert_eq!(v["restrict_only"], true);
        assert_eq!(v["loosens_gate"], false);
        assert_eq!(v["raises_autonomy"], false);
        // The read-only tools are listed (a HUD chip can show them).
        assert!(v["read_only_tools"].as_array().is_some_and(|a| !a.is_empty()));
        // NOTHING secret leaks: no audio/score/embedding/private-fact field.
        for leak in ["audio", "score", "embedding", "samples", "facts", "private"] {
            assert!(v.get(leak).is_none(), "telemetry leaked {leak}");
        }
    }

    #[test]
    fn owner_path_telemetry_reports_inactive_with_the_owner_reason() {
        let d = decide(SpeakerState::OwnerVerified, false, &armed_cfg(), &orchestrator_owner());
        let v = guest_telemetry(&d);
        assert_eq!(v["guest_active"], false);
        assert_eq!(v["reason"], "owner");
        // Even the inactive frame states the restrict-only posture (never loosens).
        assert_eq!(v["loosens_gate"], false);
    }

    #[test]
    fn the_guest_tool_set_is_strictly_on_device_read_only() {
        // REGRESSION: unified_search fans out to the OWNER's Gmail / Calendar / Slack —
        // it is NOT on-device and NOT safe to hand a bystander; it must never be a
        // guest tool (the exact thing guest mode exists to withhold).
        assert!(
            !GUEST_READ_ONLY_TOOLS.contains(&"unified_search"),
            "unified_search reads the owner's connected cloud accounts — never a guest tool"
        );
        // The module's stated contract: every guest tool is a STRICT subset of the
        // author's on-device read-only curation SAFE_LOCAL_TOOLS.
        for t in GUEST_READ_ONLY_TOOLS {
            assert!(
                crate::anthropic::SAFE_LOCAL_TOOLS.contains(t),
                "guest tool {t:?} must be in SAFE_LOCAL_TOOLS (proven on-device + read-only)"
            );
        }
        // No write / outward / consequential tool slipped in.
        for banned in ["remember_fact", "open_url", "web_search", "gmail_send", "ui_actuate", "shell_run"] {
            assert!(!GUEST_READ_ONLY_TOOLS.contains(&banned), "{banned:?} must not be a guest tool");
        }
    }

    // =====================================================================
    // CONFIG view resolution
    // =====================================================================

    #[test]
    fn config_view_resolves_armed_by_default_with_a_quiet_profile() {
        let cfg = crate::config::ThresholdConfig::default();
        assert!(cfg.enabled, "[threshold] ships ARMED by default");
        let view = ThresholdConfigView::from_config(&cfg);
        assert!(view.enabled);
        // The shipped guest profile is a genuinely quiet one (only Critical surfaces).
        let tuned = apply_profile(&view.guest_profile, &BaseBehavior::default());
        assert!(tuned.surfaces(SignalCategory::Critical), "critical floor still holds for a guest");
        assert!(!tuned.surfaces(SignalCategory::News), "the guest profile quiets ordinary intel");
        // An unknown guest_profile string is a restrict-only CUSTOM profile, never broadening.
        let weird = ThresholdConfigView::from_config(&crate::config::ThresholdConfig {
            enabled: true,
            guest_profile: "whatever-typo".to_string(),
        });
        let wt = apply_profile(&weird.guest_profile, &BaseBehavior::default());
        assert!(wt.is_no_broader_than(&BaseBehavior::default()), "a typo'd guest profile can only quiet");
    }

    // =====================================================================
    // LIVE WIRING: the per-turn scope global + the recall seam
    // =====================================================================

    #[test]
    fn current_turn_scope_defaults_to_the_owner_path_and_the_override_restores() {
        // Default (no install / no override) reads as the OWNER path (None) — so the
        // recall dispatch, tool loop, and anticipation tick are all byte-for-byte
        // today's until a guest scope is installed.
        assert!(current_turn_scope().is_none(), "no scope installed -> owner path");
        {
            let guest = guest_from(&orchestrator_owner(), &FocusProfile::DeepFocus);
            let _o = ScopeOverride::guest(guest.clone());
            assert_eq!(current_turn_scope(), Some(guest), "override installs a guest scope on this thread");
        }
        // Restored on drop — the override never leaks into the next test.
        assert!(current_turn_scope().is_none(), "override restored the owner path on drop");
        // An explicit OWNER override also reads as the owner path.
        {
            let _o = ScopeOverride::owner();
            assert!(current_turn_scope().is_none(), "owner override -> owner path");
        }
        assert!(current_turn_scope().is_none());
    }

    #[test]
    fn recall_namespace_for_turn_is_the_owner_ns_on_the_owner_path_and_the_sentinel_for_a_guest() {
        // OWNER PATH (no guest scope): recall_namespace_for_turn returns the owner
        // namespace VERBATIM — the recall dispatch is byte-for-byte unchanged.
        {
            let _o = ScopeOverride::owner();
            assert_eq!(recall_namespace_for_turn("agent.darwin"), "agent.darwin");
            assert_eq!(recall_namespace_for_turn("agent.friday"), "agent.friday");
        }
        // GUEST PATH: the shared-only sentinel, so the EXISTING own+shared guard
        // yields only the shared tier — never the owner's private agent.* facts.
        {
            let guest = guest_from(&orchestrator_owner(), &FocusProfile::DeepFocus);
            let _o = ScopeOverride::guest(guest);
            assert_eq!(recall_namespace_for_turn("agent.darwin"), GUEST_NAMESPACE);
            assert_eq!(recall_namespace_for_turn("agent.friday"), GUEST_NAMESPACE);
        }
    }

    #[tokio::test]
    async fn live_recall_seam_hides_the_owners_private_fact_from_a_guest_end_to_end() {
        // END-TO-END proof of the WIRING: the LIVE recall dispatch feeds
        // `recall_namespace_for_turn(owner_ns)` to the EXISTING own+shared guard.
        // With a guest scope installed that routes to the sentinel, so the owner's
        // private agent.* fact is invisible; with no scope (owner path) both facts
        // are visible — byte-for-byte today's recall.
        let path = std::env::temp_dir().join(format!("darwin-threshold-wire-recall-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mem = crate::memory::Memory::open(&path).expect("open temp memory");
        mem.upsert_fact("user.name", "Darwin").await.unwrap();
        mem.upsert_fact("agent.darwin.secret_note", "the owner's private note").await.unwrap();

        // OWNER PATH: recall_namespace_for_turn("agent.darwin") == "agent.darwin",
        // so the guard returns own + shared — both facts visible (unchanged).
        {
            let _o = ScopeOverride::owner();
            let ns = recall_namespace_for_turn("agent.darwin");
            let view = mem.agent_scoped_facts(&ns, 50).await.unwrap();
            let keys: Vec<&str> = view.iter().map(|(k, _)| k.as_str()).collect();
            assert!(keys.contains(&"user.name"), "owner sees shared knowledge");
            assert!(keys.contains(&"agent.darwin.secret_note"), "owner path sees the private fact (unchanged)");
        }

        // GUEST PATH: the live seam routes to the sentinel, so the guard returns
        // SHARED-only — the owner's private note is invisible to the guest.
        {
            let guest = guest_from(&orchestrator_owner(), &FocusProfile::DeepFocus);
            let _o = ScopeOverride::guest(guest);
            let ns = recall_namespace_for_turn("agent.darwin");
            let view = mem.agent_scoped_facts(&ns, 50).await.unwrap();
            let keys: Vec<&str> = view.iter().map(|(k, _)| k.as_str()).collect();
            assert!(keys.contains(&"user.name"), "guest still sees shared knowledge");
            assert!(
                !keys.contains(&"agent.darwin.secret_note"),
                "guest MUST NOT see the owner's private fact via the LIVE recall seam: {keys:?}"
            );
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn install_site_invariant_holds_the_guest_scope_is_never_broader_than_the_owner() {
        // The install-site SAFETY RAIL, exactly as `run_pipeline` evaluates it: the
        // decided guest scope is asserted NO BROADER than the owner scope it was
        // derived from, over the base the anticipation tick composes on. Proven here
        // for every guest reason (auto + explicit) and a spread of guest profiles.
        let owner_scope = Scope::owner(vec!["*".to_string()], FocusProfile::Default);
        let base = BaseBehavior::default();
        for gp in guest_profiles() {
            let cfg = ThresholdConfigView { enabled: true, guest_profile: gp.clone() };
            for (speaker, flag) in [
                (SpeakerState::Unrecognized, false),
                (SpeakerState::OwnerVerified, true),
                (SpeakerState::Unenforced, true),
            ] {
                let d = decide(speaker, flag, &cfg, &owner_scope);
                assert!(d.active, "guest should be active for ({speaker:?}, flag={flag})");
                assert!(
                    d.scope.is_no_broader_than(&owner_scope, &base),
                    "install-site rail: guest scope broadened the owner ({gp:?}, {speaker:?}, flag={flag})"
                );
            }
        }
    }

    // =====================================================================
    // The explicit "guest mode on/off" toggle classifier
    // =====================================================================

    #[test]
    fn guest_toggle_is_anchored_and_does_not_over_trigger() {
        use GuestToggle::*;
        // ON phrasings.
        for u in [
            "guest mode on",
            "turn on guest mode",
            "enable guest mode",
            "enter guest mode please",
            "start guest mode",
            "Guest mode on.",
        ] {
            assert_eq!(classify_guest_toggle(u), Some(On), "{u:?} should turn guest mode ON");
        }
        // OFF phrasings (off wins even though they contain "guest mode").
        for u in [
            "guest mode off",
            "turn off guest mode",
            "disable guest mode",
            "exit guest mode",
            "leave guest mode",
            "end guest mode.",
        ] {
            assert_eq!(classify_guest_toggle(u), Some(Off), "{u:?} should turn guest mode OFF");
        }
        // Ordinary sentences — including ones that MENTION guest mode — must NOT trip
        // it. A bystander must never be able to widen anything, and a mere QUESTION
        // about guest mode must not silently scope the turn.
        for u in [
            "what is guest mode",
            "guest mode, what does that do?",
            "tell me about guest mode",
            "is guest mode on right now",
            "a guest is coming over later",
            "send an email to my guest",
            "",
            "   ",
        ] {
            assert_eq!(classify_guest_toggle(u), None, "{u:?} must not toggle guest mode");
        }
    }

    // =====================================================================
    // ANTICIPATION TICK composition (WIRING POINT 4)
    // =====================================================================

    #[test]
    fn anticipation_guest_composition_can_only_quiet_the_owners_tuned_behavior() {
        use crate::focus::SignalCategory;
        // The EXACT composition the anticipation tick performs when a guest scope is
        // installed: apply_profile(&scope.profile, &owner_tuned.as_base()) — the guest
        // focus profile layered ON TOP of the owner's tuned behavior. It must be NO
        // BROADER than the owner's tuned behavior (it can only quiet further, never
        // surface more) for EVERY owner behavior a tick could be running under.
        let owner_bases = [
            BaseBehavior::default(),
            apply_profile(&FocusProfile::Work, &BaseBehavior::default()).as_base(),
            apply_profile(&FocusProfile::Sleep, &BaseBehavior::default()).as_base(),
        ];
        for owner_base in owner_bases {
            let owner_tuned = apply_profile(&FocusProfile::Default, &owner_base);
            let scope = guest_from(&orchestrator_owner(), &FocusProfile::DeepFocus);
            // This is precisely `main.rs`'s WIRING POINT 4 expression.
            let guest_tuned = apply_profile(&scope.profile, &owner_tuned.as_base());
            assert!(
                guest_tuned.is_no_broader_than(&owner_tuned.as_base()),
                "the guest anticipation composition must never surface more than the owner's tuned behavior"
            );
            // The Critical floor still holds (a genuinely critical signal is never
            // withheld even from the guest-quieted tick); ordinary intel is quieted.
            assert!(guest_tuned.surfaces(SignalCategory::Critical), "critical floor holds for a guest tick");
            assert!(!guest_tuned.surfaces(SignalCategory::News), "the guest tick quiets ordinary intel");
        }
    }
}

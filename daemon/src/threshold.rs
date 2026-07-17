//! THRESHOLD — a voice-scoped GUEST / restricted-speaker mode.
//!
//! ## What this is
//! When voice-id reports an UNRECOGNIZED speaker (or the owner explicitly turns
//! "guest mode" on), THRESHOLD projects a GUEST SCOPE over the turn:
//!   * a restricted, strictly NON-PERSONAL tool allowlist — [`GUEST_READ_ONLY_TOOLS`]
//!     (only `system_status`, `skill_list`, `babel_translate`), intersected with the
//!     owner's own allowlist so it can never NAME a tool the owner lacks. NO tool
//!     that reads or writes the owner's personal data is offered;
//!   * NO owner memory at all. The whole fact store is the owner's personal data —
//!     the "shared across agents" (`not agent.*`) tier still holds the owner's
//!     `user.*` / `user.model.*` / `user.world.*` rows, so it is NOT safe for a
//!     bystander. The live recall dispatch consults [`is_guest_turn`] and feeds a
//!     guest turn an EMPTY fact + history feed (fail-closed) — there is no "safe
//!     subset" of the owner's memory to hand a bystander;
//!   * a quieter focus profile (a [`crate::focus::FocusProfile`]) carried as a
//!     restrict-only knob — provably no-broader than the owner's via
//!     [`crate::focus::apply_profile`]. NB: this scope is per-TURN, so it governs
//!     the guest's own spoken turn, NOT the ambient anticipation/mission loops.
//!     "Ambient guest-presence quieting" of proactive briefs is a SEPARATE future
//!     feature (it needs a PERSISTENT guest-presence signal, not a per-turn one).
//!
//! ## The sacred invariant: guest scope can ONLY NARROW — it LAYERS ON TOP
//! A guest scope is derived from the owner scope and is provably NO BROADER than
//! it on every axis ([`Scope::is_no_broader_than`], asserted by the property
//! test): its tools are a SUBSET of the owner's, its recall is at least as
//! restricted (owner memory withheld), and its focus profile is at least as quiet.
//! There is
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
//! This module is a PURE decision seam. Its LIVE wiring is installed by the daemon:
//! `run_pipeline` decides + installs the per-turn guest scope (cleared at turn end
//! by `TurnScopeGuard`); the recall dispatch consults [`is_guest_turn`] to withhold
//! owner memory; the tool loop intersects the offered tools with the guest scope and
//! `execute_tool` refuses any tool outside it; `route()` refuses every owner-data /
//! consequential fast path for a guest; and `emit_guest` publishes the frame. A few
//! pure helpers exist for the invariant proofs (`is_no_broader_than`, `behavior`)
//! and are not all live-called, so — exactly like `policy.rs`'s "a shared contract
//! another component reads" rationale — the unused-item lint is allowed module-wide.
//! The invariant lives next to the type it guards; the tests exercise every item.
#![allow(dead_code)]

use serde_json::json;

use crate::focus::{apply_profile, BaseBehavior, FocusProfile, TunedBehavior};

/// The tools wildcard the orchestrator (`darwin`) holds. Mirrors
/// `agents::TOOLS_WILDCARD` / [`crate::agents::Agent::may_use`].
const TOOLS_WILDCARD: &str = "*";

/// The CURATED tool allowlist a guest may use — narrowed to ONLY genuinely
/// NON-PERSONAL tools, ones whose dispatch touches NO owner-stored personal data
/// and takes NO consequential/outward/write action:
///   * `system_status` — machine health (RAM + disk-free pct); no owner data.
///   * `skill_list` — the skill CATALOG (capability names/categories); no owner data.
///   * `babel_translate` — transforms the guest's own text; stores nothing, reads
///     nothing, sends nothing.
///
/// A guest can TALK, TRANSLATE, and see non-personal STATUS — nothing personal.
///
/// DELIBERATELY EXCLUDED (each reads or writes the OWNER's private data, so the
/// "shared across agents" tier is NOT safe for a bystander — that tier still holds
/// the owner's personal `user.*` / `user.model.*` / `user.world.*` rows):
///   * `recall_facts` / `mnemosyne_recall` / `episodic_recall` — read the owner's
///     memory store (routing to a "shared" namespace does NOT protect the owner,
///     because that tier IS the owner's personal facts);
///   * `user_model_query` — the owner's personal profile (`user.model.*`);
///   * `world_query` — the owner's personal world graph (`user.world.*`);
///   * `doc_search` — the owner's indexed documents;
///   * `search_files` — the owner's `$HOME` filesystem;
///   * `remember_fact` / `skill_invoke` — a durable WRITE / a consequential dispatch.
///
/// A bystander gets NO owner-memory access at all.
pub const GUEST_READ_ONLY_TOOLS: &[&str] = &[
    // Read-only machine status + the skill CATALOG (listing, not invocation).
    "system_status",
    "skill_list",
    // On-device translation of the guest's OWN text — no owner data touched.
    "babel_translate",
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
///   * `shared_recall_only` — when true, the owner's stored memory is WITHHELD from
///     this turn ENTIRELY (a guest reads none of it — see [`is_guest_turn`], which
///     the live recall dispatch consults to return an empty feed); when false, the
///     owner's normal own+shared recall. It only ever TIGHTENS recall.
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
// recall dispatch and the tool loop of the GUEST'S OWN TURN, WITHOUT parameter
// threading. It is a TASK-LOCAL confined to the run_pipeline turn task, so a
// CONCURRENT background task (the anticipation loop, a durable/standing mission)
// runs OUTSIDE any turn scope and can NEVER read — nor be governed by — a guest
// turn's scope. A per-turn signal governs a TURN, never ambient background work.
// ---------------------------------------------------------------------------

tokio::task_local! {
    /// The current TURN's guest scope. Established fresh for EACH turn by
    /// [`with_turn_scope`] (the event loop wraps every `run_pipeline` call), so:
    ///   * only the turn's OWN task sees it — a concurrent mission/anticipation
    ///     task reads `None` (see [`current_turn_scope`]);
    ///   * it resets to `None` for the next turn BY CONSTRUCTION — a guest turn's
    ///     scope can never leak into the owner's next turn.
    /// `RefCell` gives interior mutability so the decision (known only mid-turn,
    /// after STT) can be installed via [`set_turn_scope`] within the same scope.
    static TURN_SCOPE: std::cell::RefCell<Option<Scope>>;
}

// Test-only thread-local override, mirroring `voiceid`'s `GATE_OVERRIDE`: a test
// forces the current-turn scope on its OWN thread without establishing a task
// scope. The outer `Option` is "is an override installed", the inner
// `Option<Scope>` is the forced value (Some = guest scope, None = owner path).
// Compiled out in release.
#[cfg(test)]
thread_local! {
    static SCOPE_OVERRIDE: std::cell::RefCell<Option<Option<Scope>>> =
        const { std::cell::RefCell::new(None) };
}

/// Run one turn's pipeline `fut` inside a FRESH per-turn guest-scope slot. The
/// event loop wraps EVERY `run_pipeline` call in this, so the guest scope is
/// confined to that turn's task and reset for the next turn. A background task
/// (anticipation / mission) is NOT wrapped, so its [`current_turn_scope`] reads
/// `None` and it is never governed by a guest turn.
pub async fn with_turn_scope<F>(fut: F) -> F::Output
where
    F: std::future::Future,
{
    TURN_SCOPE.scope(std::cell::RefCell::new(None), fut).await
}

/// Install THIS turn's guest scope (called once near the top of `run_pipeline`
/// when the decision is active). A no-op if somehow called outside a turn scope
/// (a background task), so it can never affect ambient work.
pub fn set_turn_scope(scope: Scope) {
    let _ = TURN_SCOPE.try_with(|c| *c.borrow_mut() = Some(scope));
}

/// Clear the per-turn guest scope (the OWNER-path branch). A no-op outside a turn
/// scope. Belt-and-suspenders on top of the per-turn reset [`with_turn_scope`]
/// already guarantees.
pub fn clear_turn_scope() {
    let _ = TURN_SCOPE.try_with(|c| *c.borrow_mut() = None);
}

/// The current TURN's installed guest scope — `None` (the OWNER path) when none is
/// installed OR when called from a BACKGROUND task that is not a turn (a mission,
/// the anticipation loop). This is the deep read consulted by the recall dispatch
/// and the tool loop.
///
/// In TEST builds a thread-local override takes precedence (so a test can force a
/// scope on its own thread); absent an override it falls through to the task-local
/// (so a test exercising [`with_turn_scope`] observes the real mechanism).
pub fn current_turn_scope() -> Option<Scope> {
    #[cfg(test)]
    if let Some(over) = SCOPE_OVERRIDE.with(|c| c.borrow().clone()) {
        return over;
    }
    TURN_SCOPE.try_with(|c| c.borrow().clone()).unwrap_or(None)
}

/// Whether THIS turn is a GUEST turn — i.e. a guest scope is installed. The live
/// recall dispatch (`grounded_facts` / `router::agent_facts`) consults this to
/// WITHHOLD the owner's stored memory from a guest ENTIRELY: a guest turn feeds NO
/// owner facts to the prompt and is offered NO owner-memory recall tool. The whole
/// `user.*` / `user.model.*` / `user.world.*` / `agent.*` store is the OWNER's
/// personal data — the "shared across agents" (`not agent.*`) tier is NOT safe for
/// a bystander, since it still holds the owner's `user.*` rows — so a guest reads
/// NONE of it. On the owner path this is false and the feed is byte-for-byte
/// today's. This is the honest, fail-closed replacement for any namespace routing:
/// there is no "safe subset" of the owner's memory to hand a bystander.
pub fn is_guest_turn() -> bool {
    // PRESENCE-ONLY — never clones the Scope (this runs at ~15 read sites per turn;
    // the value is never inspected here, only Option::is_some()).
    #[cfg(test)]
    if let Some(over) = SCOPE_OVERRIDE.with(|c| c.borrow().clone()) {
        return over.is_some();
    }
    TURN_SCOPE.try_with(|c| c.borrow().is_some()).unwrap_or(false)
}

/// THE WRITE-INTEGRITY CHOKEPOINT PREDICATE — the WRITE-side twin of
/// [`is_guest_turn`]'s READ-side withholding.
///
/// ## The invariant it enforces
/// **A guest turn leaves NO durable trace in the owner's state.** Rather than
/// gate the UNBOUNDED, scattered durable-write CALL SITES (transcript, the passive
/// learner, episodic, the CAUSA decision-trace, the optimizer trace, macro-capture,
/// `record_event`, `record_interaction`, … — each of which a review keeps
/// re-finding, whack-a-mole), the invariant is enforced ONCE at the BOUNDED,
/// enumerable PERSISTENCE BOUNDARY: the finite set of durable-write PRIMITIVES.
/// Each such primitive calls this and NO-OPs (writes nothing) when it returns true,
/// so a guest turn STRUCTURALLY cannot durably write — no matter which caller
/// reaches the primitive, now or in the future.
///
/// The gated primitives (the enumeration the tests pin) are:
///   * every `Memory` INSERT/UPDATE write: `record_event`, `upsert_fact_at` (the
///     single fact-write leaf that `upsert_fact` / `upsert_user_fact` — and thus
///     the cloud `remember_fact` tool, `user_model`, `standing`, `macros::save`,
///     `proactive::record_interaction` — all funnel through), `record_transcript`,
///     `record_episode`, `save_notebook_entry`;
///   * the optimizer [`crate::optimize::TraceStore`] (`record_returning_id` /
///     `record_trace` / `label_outcome`);
///   * `crate::macros::capture` (the recording buffer whose contents become durable
///     at "stop recording");
///   * `crate::episodic::record_episode`;
///   * `crate::explain::record` (the decision-trace ring the owner reads back via
///     "why did you do that");
///   * `crate::calibrate::record` / `relabel` (the calibration window that shifts
///     the owner's clarify threshold).
///
/// ## Background tasks are UNAFFECTED (by construction)
/// The guest scope is a per-turn TASK-LOCAL ([`with_turn_scope`]). A concurrent
/// BACKGROUND task — a mission, the anticipation / retention / reflection loop —
/// runs OUTSIDE any turn scope and reads FALSE here, so its writes proceed
/// normally. The one durable write that ESCAPES this chokepoint is the passive
/// fact-learner: it is `tokio::spawn`ed (`spawn_learning_task`), so it runs in a
/// DETACHED task that does NOT inherit the turn's task-local and would read FALSE
/// here. It therefore keeps its OWN call-site gate (`should_learn_turn`, which
/// refuses to SPAWN on a guest turn) — that gate is load-bearing and is NOT
/// subsumed by this chokepoint.
///
/// ## HONESTY
/// Only DURABLE owner-durable / owner-influencing state is gated. Telemetry emits,
/// per-turn in-RAM scratch (the source accumulator, `take_turn_tool`, the verify
/// outcome), secret-free latency aggregates, and the guest's OWN spoken reply are
/// NOT durable owner state and flow normally.
///
/// ONE honest caveat about ordering: a single CONTENT-FREE capture MARKER —
/// `record_event("audio", "utterance.captured", <wav_path>)` — is emitted DURING STT
/// (overlapped with transcription), which is BEFORE the guest scope can be decided
/// (the decision needs the TRANSCRIPT, for the explicit "guest mode" toggle, so it
/// cannot be known pre-STT). At that instant `is_guest_turn()` is still false, so the
/// chokepoint does not gate that one row. It carries NO transcript / NO utterance
/// content / NO owner data (just a wav path + timestamp; the wav is discarded after
/// the turn), the `events` table has NO owner-facing ROW reader, and it is
/// retention-pruned — so no bystander's WORDS ever enter the durable log. The
/// utterance CONTENT, which rides the router's `route.cloud` / `route.local` event
/// payloads (recorded AFTER the scope is installed), IS gated here.
pub fn guest_write_blocked() -> bool {
    is_guest_turn()
}

/// GUEST = LOCAL-ONLY. The cloud-routing twin of [`crate::vault::deny_cloud`]:
/// fold a guest turn into THIS turn's cloud-vs-local decision so a bystander's turn
/// NEVER reaches the owner's PAID cloud brain. A guest conversation therefore stays
/// on the on-device model, which closes a durable-write path that no per-sink gate
/// should: with no cloud call there is NO obol spend row and NO bump of the owner's
/// daily budget total (a durable, owner-readable, budget-influencing trace), NO
/// egress of the guest's turn under the owner's API key — and it is MORE private for
/// the guest too. This is a principled SCOPE EXTENSION (guest = local-only), not
/// another per-sink write gate.
///
/// RESTRICT-ONLY + COMPOSABLE with the vault gate — `guest OR vault -> local`. It can
/// only ever turn a would-be cloud turn LOCAL, never the reverse. With no guest scope
/// installed (the owner path, and EVERY background task — it reads the task-local, so
/// a mission/anticipation tick is unaffected) this is byte-for-byte `would_go_cloud`,
/// so the owner still uses the cloud by default. Applied at the SAME two router seams
/// vault uses: the `cloud_reachable` entry seam (which the conversation brain, the
/// roster answer, capability routing, and agent selection all consult) and the
/// actuating tool-loop `to_cloud` seam.
pub fn deny_cloud(would_go_cloud: bool) -> bool {
    would_go_cloud && !is_guest_turn()
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

    /// A representative SPECIALIST owner scope: a finite allowlist mixing the ONE
    /// non-personal guest tool it holds (`system_status`) with owner-data readers and
    /// consequential/write tools. Its guest projection must keep ONLY the non-personal
    /// tool it already holds, dropping every owner-data / consequential one.
    fn specialist_owner() -> Scope {
        Scope::owner(
            vec![
                "system_status".to_string(), // non-personal — kept for a guest
                "recall_facts".to_string(),  // owner memory — dropped for guest
                "doc_search".to_string(),    // owner documents — dropped for guest
                "gmail_send".to_string(),     // consequential/outward — dropped for guest
                "remember_fact".to_string(),  // a write — dropped for guest
                "shell_run".to_string(),      // maximally dangerous — dropped for guest
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
    fn an_unrecognized_speaker_cannot_un_scope_themselves() {
        // SECURITY: the explicit "guest mode off" toggle only clears the PERSISTENT
        // flag (guest_flag=false); it does NOT clear the per-turn voice signal. So an
        // UNRECOGNIZED speaker who says "guest mode off" (guest_flag=false) is STILL
        // auto-scoped to guest — the voice-gate auto-scope wins. A bystander can never
        // talk their way out of the guest scope while voice-id is enforcing.
        let owner = orchestrator_owner();
        let d = decide(SpeakerState::Unrecognized, false, &armed_cfg(), &owner);
        assert!(d.active, "an unrecognized speaker stays scoped even with the explicit flag OFF");
        assert_eq!(d.reason, GuestReason::Unrecognized);
        // Only a VERIFIED owner turning the flag off returns to the full owner scope.
        let d2 = decide(SpeakerState::OwnerVerified, false, &armed_cfg(), &owner);
        assert!(!d2.active, "the verified owner with the flag off gets the full owner scope");
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
    fn specialist_guest_keeps_only_its_own_non_personal_tools() {
        // A specialist owner's guest projection is the intersection: it keeps ONLY
        // the non-personal tool the owner ALREADY held (system_status) and drops the
        // owner-data readers (recall_facts, doc_search) and the consequential/write
        // ones (gmail_send, remember_fact, shell_run).
        let owner = specialist_owner();
        let d = decide(SpeakerState::Unrecognized, false, &armed_cfg(), &owner);
        assert!(d.active);
        assert_eq!(
            d.scope.tools,
            vec!["system_status".to_string()],
            "guest keeps only the non-personal tools the owner held"
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
        // Wildcard owner -> the whole non-personal guest set.
        let full = guest_read_only_tools(&["*".to_string()]);
        assert_eq!(full.len(), GUEST_READ_ONLY_TOOLS.len());
        // A narrow owner -> only the overlap, and NEVER a tool the owner lacks. Here
        // the owner holds one guest tool (system_status) and one it doesn't grant a
        // guest (gmail_send), so the guest keeps only system_status.
        let narrow = guest_read_only_tools(&[
            "system_status".to_string(),
            "gmail_send".to_string(), // not a guest tool -> not admitted into the guest set
        ]);
        assert_eq!(narrow, vec!["system_status".to_string()]);
        // An owner with NO non-personal guest tools -> an empty guest set. Note an
        // owner-DATA reader (doc_search) does NOT grant a guest anything.
        assert!(guest_read_only_tools(&["gmail_send".to_string()]).is_empty());
        assert!(guest_read_only_tools(&["doc_search".to_string()]).is_empty(),
            "an owner-data reader is never a guest tool");
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
    fn the_guest_tool_set_is_exactly_the_three_non_personal_tools() {
        // The guest allowlist is narrowed to ONLY genuinely non-personal tools —
        // ones whose dispatch touches NO owner-stored personal data and takes no
        // consequential/write action.
        assert_eq!(
            GUEST_READ_ONLY_TOOLS,
            &["system_status", "skill_list", "babel_translate"],
            "the guest set is exactly the three non-personal tools"
        );
        // REGRESSION: NONE of the owner-data readers or write/outward tools may ever
        // be a guest tool. `unified_search` fans out to the owner's connected cloud
        // accounts; the memory-recall tools read the owner's fact store (the "shared"
        // tier still holds the owner's user.* rows); user_model_query / world_query
        // read the owner's profile / world graph; doc_search / search_files read the
        // owner's documents / $HOME; remember_fact / skill_invoke write / dispatch.
        for banned in [
            "unified_search",
            "recall_facts", "mnemosyne_recall", "episodic_recall",
            "user_model_query", "world_query",
            "doc_search", "search_files",
            "remember_fact", "skill_invoke",
            "open_url", "web_search", "gmail_send", "ui_actuate", "shell_run",
        ] {
            assert!(
                !GUEST_READ_ONLY_TOOLS.contains(&banned),
                "{banned:?} reads or writes the owner's data — must NEVER be a guest tool"
            );
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
        // recall dispatch and tool loop are byte-for-byte today's until a guest scope
        // is installed.
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

    #[tokio::test]
    async fn the_guest_scope_is_confined_to_its_own_turn_and_never_leaks_or_touches_background_tasks() {
        // FINDING 2 + 4: the per-turn scope is a TASK-LOCAL established by
        // `with_turn_scope`. Prove (a) a BACKGROUND task (no wrapper — a mission /
        // the anticipation loop) reads None, so it is NEVER governed by a guest turn;
        // (b) the scope is visible WITHIN its own turn; (c) it resets for the next
        // turn by construction — no cross-turn leak.
        //
        // NB: in test builds `current_turn_scope` prefers a thread-local override
        // (used by the other guest tests); with NO override installed it falls through
        // to the task-local, which is what this test exercises.

        // (a) Outside any turn scope -> None (a concurrent mission/anticipation task).
        assert!(current_turn_scope().is_none(), "a background task reads no scope");

        // (b) A turn wraps its work in with_turn_scope; the installed scope is visible.
        with_turn_scope(async {
            assert!(current_turn_scope().is_none(), "a turn starts with no scope");
            set_turn_scope(guest_from(&orchestrator_owner(), &FocusProfile::DeepFocus));
            assert!(current_turn_scope().is_some(), "the scope is visible within its own turn");
            // A clear within the turn takes effect immediately.
            clear_turn_scope();
            assert!(current_turn_scope().is_none(), "clear within the turn empties the slot");
            set_turn_scope(guest_from(&orchestrator_owner(), &FocusProfile::DeepFocus));
        })
        .await;

        // (c) The NEXT turn is a FRESH scope — the previous turn's scope did not leak.
        with_turn_scope(async {
            assert!(current_turn_scope().is_none(), "the guest scope did NOT leak into the next turn");
        })
        .await;

        // And after all turns, a background task still reads None.
        assert!(current_turn_scope().is_none(), "background tasks remain unaffected");
    }

    #[test]
    fn is_guest_turn_tracks_the_installed_scope() {
        // OWNER PATH (no guest scope): is_guest_turn() is false — the recall dispatch
        // is byte-for-byte unchanged and feeds the owner's memory as today.
        {
            let _o = ScopeOverride::owner();
            assert!(!is_guest_turn(), "owner path is not a guest turn");
        }
        // GUEST PATH: is_guest_turn() is true — the live recall dispatch consults
        // this to WITHHOLD the owner's memory ENTIRELY (empty feed).
        {
            let guest = guest_from(&orchestrator_owner(), &FocusProfile::DeepFocus);
            let _o = ScopeOverride::guest(guest);
            assert!(is_guest_turn(), "an installed guest scope makes it a guest turn");
        }
        // Restored on drop — the override never leaks into the next test.
        assert!(!is_guest_turn(), "override restored the owner path");
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
    // A guest scope's focus profile is a restrict-only knob (NOT wired to any
    // ambient tick — that is a separate future feature; see the module doc).
    // =====================================================================

    #[test]
    fn a_guest_scopes_focus_profile_is_provably_no_broader_than_the_owners() {
        use crate::focus::SignalCategory;
        // The guest scope carries a focus profile as a restrict-only knob. Composed
        // on top of the owner's tuned behavior it is provably NO BROADER (it can only
        // quiet further), which is the profile axis of `is_no_broader_than`. This is a
        // PURE property of the scope; it is NOT read by the anticipation/mission loops
        // (a per-turn scope must not govern ambient background tasks).
        let owner_bases = [
            BaseBehavior::default(),
            apply_profile(&FocusProfile::Work, &BaseBehavior::default()).as_base(),
            apply_profile(&FocusProfile::Sleep, &BaseBehavior::default()).as_base(),
        ];
        for owner_base in owner_bases {
            let owner_tuned = apply_profile(&FocusProfile::Default, &owner_base);
            let scope = guest_from(&orchestrator_owner(), &FocusProfile::DeepFocus);
            let guest_tuned = scope.behavior(&owner_tuned.as_base());
            assert!(
                guest_tuned.is_no_broader_than(&owner_tuned.as_base()),
                "the guest scope's profile must never surface more than the owner's tuned behavior"
            );
            assert!(guest_tuned.surfaces(SignalCategory::Critical), "critical floor holds");
            assert!(!guest_tuned.surfaces(SignalCategory::News), "the guest profile quiets ordinary intel");
        }
    }

    // =====================================================================
    // WRITE-INTEGRITY CHOKEPOINT — the persistence-boundary enumeration
    //
    // THE invariant: a guest turn leaves NO durable trace in the owner's state.
    // This test ENUMERATES every DURABLE-STORE write primitive (the finite
    // persistence layer) and proves each is a NO-OP under a guest turn
    // (with_turn_scope + set_turn_scope, the REAL task-local mechanism) and
    // UNCHANGED for an owner turn. The three process-global RING primitives
    // (macros::capture, explain::record, calibrate::record/relabel) are proven
    // in their home modules (natural serialization); together they cover the
    // full set guarded by threshold::guest_write_blocked.
    // =====================================================================

    /// Remove a temp DB and its WAL/SHM sidecars.
    fn rm_db(path: &std::path::Path) {
        for suffix in ["", "-wal", "-shm"] {
            let mut s = path.to_path_buf().into_os_string();
            s.push(suffix);
            let _ = std::fs::remove_file(std::path::PathBuf::from(s));
        }
    }

    #[tokio::test]
    async fn write_integrity_every_durable_store_primitive_is_a_noop_on_a_guest_turn() {
        use crate::config::Config;
        use crate::episodic;
        use crate::memory::{Episode, Memory, NotebookCitation, NotebookEntry};
        use crate::optimize::{self, Outcome, Trace, TraceStore};
        use crate::voiceid::OwnerGate;

        // Isolated on-disk stores (unique per test) so every assertion is EXACT and
        // immune to any other test.
        let base = std::env::temp_dir().join(format!("darwin-threshold-writeint-{}", std::process::id()));
        let mem_path = std::path::PathBuf::from(format!("{}-mem.db", base.display()));
        let trace_path = std::path::PathBuf::from(format!("{}-trace.db", base.display()));
        rm_db(&mem_path);
        rm_db(&trace_path);
        let mem = Memory::open(&mem_path).unwrap();
        let store = TraceStore::open(&trace_path).unwrap();

        let mut cfg = Config::default();
        cfg.episodic.enabled = true;
        cfg.optimize.enabled = true;
        // OwnerGate::OFF => voice-id not enforcing => the VoiceGate permits recording,
        // so an OWNER turn WOULD record an episode (isolating the guest guard as the
        // ONLY reason a guest turn does not).
        let voice = episodic::VoiceGate::from_owner_gate(OwnerGate::OFF);

        let episode = || Episode {
            id: 0,
            ts: String::new(),
            agent_namespace: "agent.darwin".to_string(),
            utterance_redacted: "hello there".to_string(),
            topic: "conversation".to_string(),
            salient_entities: vec![],
            outcome: "ok".to_string(),
            summary: "a greeting".to_string(),
        };
        let notebook = || NotebookEntry {
            id: 0,
            ts: String::new(),
            agent_namespace: "agent.darwin".to_string(),
            topic_key: "topic".to_string(),
            topic: "Topic".to_string(),
            synthesized: "a synthesized body".to_string(),
            citations: vec![NotebookCitation {
                source_id: 1,
                title: "t".to_string(),
                url: "https://example.com".to_string(),
            }],
        };
        let a_trace = || Trace::new("hello there", "conversation", "agent.darwin", "clarify", "", Outcome::Success, 1, 1);

        let guest = guest_from(&orchestrator_owner(), &FocusProfile::DeepFocus);

        // ---- PHASE 1: GUEST TURN — every durable-write primitive must NO-OP ----
        with_turn_scope(async {
            set_turn_scope(guest.clone());
            assert!(is_guest_turn(), "the guest scope is installed for this turn");

            // Memory INSERT/UPDATE primitives.
            mem.record_event("cloud", "route.cloud", "guest utterance").await.unwrap();
            mem.upsert_fact("user.enum.fact", "leak").await.unwrap();
            mem.upsert_user_fact("user.enum.userfact", "leak").await.unwrap();
            mem.record_transcript(Some("/tmp/g.wav"), "guest utterance", "conversation", "cloud", Some("guest reply"))
                .await
                .unwrap();
            mem.record_episode(&episode()).await.unwrap();
            let nb_id = mem.save_notebook_entry(&notebook()).await.unwrap();
            assert_eq!(nb_id, 0, "a guest notebook write returns the no-row sentinel and inserts nothing");

            // The episodic recorder honestly reports it recorded NOTHING.
            let recorded = episodic::record_episode(&cfg, &mem, "agent.darwin", "guest utterance", "guest reply", "conversation", false, voice)
                .await
                .unwrap();
            assert!(!recorded, "episodic::record_episode records nothing for a guest (and says so)");

            // Optimizer TraceStore primitives.
            let direct = store.record_returning_id(&a_trace()).await.unwrap();
            assert_eq!(direct, 0, "a guest trace INSERT returns the no-row sentinel and inserts nothing");
            let rec = optimize::record_trace(&cfg, &store, "guest utterance", "conversation", "agent.darwin", "clarify", "", Outcome::Success, 1, 1)
                .await
                .unwrap();
            assert!(rec.is_none(), "record_trace seeds no optimizer trace for a guest");
        })
        .await;

        // Every durable store is EMPTY after the guest turn.
        assert_eq!(mem.events_count().await.unwrap(), 0, "a guest turn wrote a durable event");
        assert!(mem.get_fact("user.enum.fact").await.unwrap().is_none(), "a guest turn wrote a fact");
        assert!(mem.get_fact("user.enum.userfact").await.unwrap().is_none(), "a guest turn wrote a user fact");
        assert_eq!(mem.recent_exchanges(10).await.unwrap().len(), 0, "a guest turn wrote a transcript");
        assert_eq!(mem.episodes_count().await.unwrap(), 0, "a guest turn wrote an episode");
        assert_eq!(mem.notebook_entries_count().await.unwrap(), 0, "a guest turn wrote a notebook entry");
        assert_eq!(store.count().await.unwrap(), 0, "a guest turn wrote an optimizer trace");

        // ---- PHASE 2: OWNER TURN — every primitive writes exactly as today ----
        // No turn scope installed => is_guest_turn() == false (the owner path).
        assert!(!is_guest_turn(), "no scope installed -> owner path");
        mem.record_event("cloud", "route.cloud", "owner utterance").await.unwrap();
        mem.upsert_fact("user.enum.fact", "kept").await.unwrap();
        mem.upsert_user_fact("user.enum.userfact", "kept").await.unwrap();
        mem.record_transcript(Some("/tmp/o.wav"), "owner utterance", "conversation", "cloud", Some("owner reply"))
            .await
            .unwrap();
        mem.record_episode(&episode()).await.unwrap();
        let nb_id = mem.save_notebook_entry(&notebook()).await.unwrap();
        assert!(nb_id > 0, "an owner notebook write returns a real row id");
        let recorded = episodic::record_episode(&cfg, &mem, "agent.darwin", "owner utterance", "owner reply", "conversation", false, voice)
            .await
            .unwrap();
        assert!(recorded, "an owner episodic write records");
        let direct = store.record_returning_id(&a_trace()).await.unwrap();
        assert!(direct > 0, "an owner trace INSERT returns a real row id");
        let owner_trace_id = optimize::record_trace(&cfg, &store, "owner utterance", "conversation", "agent.darwin", "clarify", "", Outcome::Success, 1, 2)
            .await
            .unwrap()
            .expect("an owner record_trace returns a row id");

        // Every durable store now reflects the OWNER writes.
        assert_eq!(mem.events_count().await.unwrap(), 1, "the owner event was not recorded");
        assert_eq!(mem.get_fact("user.enum.fact").await.unwrap().as_deref(), Some("kept"));
        assert_eq!(mem.get_fact("user.enum.userfact").await.unwrap().as_deref(), Some("kept"));
        assert_eq!(mem.recent_exchanges(10).await.unwrap().len(), 1, "the owner transcript was not recorded");
        // record_episode (direct) + episodic::record_episode == 2 episodes.
        assert_eq!(mem.episodes_count().await.unwrap(), 2, "the owner episodes were not recorded");
        assert_eq!(mem.notebook_entries_count().await.unwrap(), 1, "the owner notebook was not recorded");
        // record_returning_id + record_trace == 2 traces.
        assert_eq!(store.count().await.unwrap(), 2, "the owner traces were not recorded");

        // ---- label_outcome: a guest turn relabels 0 rows; the owner path relabels 1.
        let guest_relabel = with_turn_scope(async {
            set_turn_scope(guest.clone());
            store.label_outcome(owner_trace_id, Outcome::CorrectedNextTurn).await.unwrap()
        })
        .await;
        assert_eq!(guest_relabel, 0, "a guest turn relabeled the owner's optimizer trace");
        let owner_relabel = store.label_outcome(owner_trace_id, Outcome::CorrectedNextTurn).await.unwrap();
        assert_eq!(owner_relabel, 1, "the owner path did not relabel the row");

        drop(mem);
        drop(store);
        rm_db(&mem_path);
        rm_db(&trace_path);
    }
}

//! FOCUS PROFILES (#24) — a PERMISSION-NEUTRAL lens over DARWIN's proactive
//! surfaces. A focus profile answers ONE question: of the things DARWIN could
//! proactively show or say, which should it stay quiet about right now? It can
//! make DARWIN do LESS — surface fewer signal categories, render a terser brief,
//! hold back suggestions — and it can NEVER make DARWIN do MORE.
//!
//! ## The sacred invariant: a profile can only QUIET, never LOOSEN
//! This module is the #24 gate's enforcement, and the enforcement is at the
//! TYPE LEVEL, not by convention:
//!
//!   * [`TunedBehavior`] — what [`apply_profile`] returns — carries ONLY
//!     non-consequential knobs: a SET of signal categories that may surface, a
//!     brief verbosity, and a "suggestions quieted" bool. There is NO field for
//!     a permission, a gate, a confirm, a voice-id, a lockdown, an autonomy
//!     level, or a consequential action. The type literally cannot express
//!     "enable a side effect" or "loosen a gate" — so `apply_profile` cannot
//!     return one. (See the `tuned_behavior_has_no_permission_field` doc-level
//!     reasoning + the property tests.)
//!
//!   * Every knob a profile touches is RESTRICT-ONLY relative to the base:
//!       - the surfacing set is always a SUBSET of the base's set (a profile may
//!         REMOVE a category from surfacing, never ADD one the base suppressed);
//!       - verbosity may only step DOWN or hold (Full -> Brief -> Silent), never up;
//!       - `suggestions_quieted` may only flip false -> true (quiet more), never
//!         true -> false (un-quiet).
//!         [`TunedBehavior::is_no_broader_than`] is the machine-checkable predicate
//!         the property test asserts for EVERY profile against its base.
//!
//!   * The DEFAULT profile is the IDENTITY: `apply_profile(Default, base) == base`
//!     for every base. With `[focus].profile = "default"` (the shipped default)
//!     today's behavior is reproduced byte-for-byte — the feature ships NEUTRAL.
//!
//! ## What a profile does NOT touch (by construction, not by promise)
//! `apply_profile` takes a [`BaseBehavior`] and returns a [`TunedBehavior`].
//! Neither type references `integrations::gate`, `consequential_allowed`, the
//! confirm path, the master switch, voice-id, lockdown, or policy. The brief
//! still makes NO outward call; accepting a suggestion still routes through the
//! EXISTING gated path. A profile narrows WHICH non-consequential intel reaches
//! the user — full stop. It never enables an action, never raises autonomy,
//! never confirms anything.
//!
//! ## Wiring (live, not dead)
//! The active profile is read from `[focus].profile` (config.rs). The live
//! anticipation tick applies it to the base behavior and uses the tuned result
//! to (a) drop a surfaced brief whose category the profile silences and (b)
//! quiet the proactive-suggestion feed. The on-demand `edith_brief` path applies
//! it too. With the default profile every gate is identity, so the live paths are
//! byte-for-byte today's behavior until the operator names a quieter profile.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Signal categories — the NON-CONSEQUENTIAL axis a profile filters on
// ---------------------------------------------------------------------------

/// The coarse CATEGORY of a proactive signal, for focus filtering. This is the
/// ONLY axis a profile reasons over: which KINDS of intel are allowed to surface
/// under the active focus. Deliberately coarse + closed — a profile decides
/// "show me critical things only" or "no news right now", never anything about
/// permissions or actions.
///
/// `Critical` is the never-silenced floor: a profile may quiet everything else
/// (news, routine, calendar, mail), but DeepFocus/Sleep still let a genuinely
/// critical signal through, so DARWIN does not go silent on something urgent
/// just because the user asked for fewer interruptions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalCategory {
    /// A genuinely urgent/critical signal (an imminent calendar conflict, a
    /// critical system-health reading). The floor: NO profile silences this.
    Critical,
    /// A calendar signal that is upcoming but not urgent.
    Calendar,
    /// Unread/important mail.
    Mail,
    /// System-health intel below the critical bar (e.g. a notable but not dire
    /// reading).
    Health,
    /// A market move.
    Market,
    /// News / world-model intel (Global-Scan). The lowest-priority, first-quieted
    /// category.
    News,
    /// Routine intel that recurs (a predictive "you usually do X now" heads-up).
    Routine,
}

impl SignalCategory {
    /// A stable short string for telemetry.
    pub fn as_str(&self) -> &'static str {
        match self {
            SignalCategory::Critical => "critical",
            SignalCategory::Calendar => "calendar",
            SignalCategory::Mail => "mail",
            SignalCategory::Health => "health",
            SignalCategory::Market => "market",
            SignalCategory::News => "news",
            SignalCategory::Routine => "routine",
        }
    }

    /// Every category, in priority order (Critical first). The base behavior's
    /// "surface everything" set.
    pub fn all() -> [SignalCategory; 7] {
        [
            SignalCategory::Critical,
            SignalCategory::Calendar,
            SignalCategory::Mail,
            SignalCategory::Health,
            SignalCategory::Market,
            SignalCategory::News,
            SignalCategory::Routine,
        ]
    }
}

// ---------------------------------------------------------------------------
// Verbosity — how much a surfaced brief says (a NON-CONSEQUENTIAL knob)
// ---------------------------------------------------------------------------

/// How verbose a surfaced brief should be. Ordered from most to least: `Full`
/// (every ranked item), `Brief` (top item(s) only — a glance), `Silent` (no
/// brief at all). A profile may only step DOWN or hold (never up), so a profile
/// can make the digest terser, never chattier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verbosity {
    /// Render the full ranked digest (capped as the builder caps it).
    Full,
    /// Render only the single highest-priority item — a one-line glance.
    Brief,
    /// Render no brief at all (the surface goes dark — but a Critical category
    /// still surfaces independently via the surfacing set; verbosity governs the
    /// DIGEST, not the critical floor).
    Silent,
}

impl Verbosity {
    /// A rank where SMALLER == terser, so "no broader than" is `self >= base` in
    /// terseness (i.e. `self.rank() >= base.rank()` means self is at least as
    /// quiet). Full=0 (loudest), Brief=1, Silent=2 (quietest).
    fn rank(&self) -> u8 {
        match self {
            Verbosity::Full => 0,
            Verbosity::Brief => 1,
            Verbosity::Silent => 2,
        }
    }

    /// How many ranked items this verbosity admits (the builder caps further).
    /// `Full` => the builder's own cap; `Brief` => 1; `Silent` => 0.
    pub fn max_items(&self, full_cap: usize) -> usize {
        match self {
            Verbosity::Full => full_cap,
            Verbosity::Brief => 1,
            Verbosity::Silent => 0,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Verbosity::Full => "full",
            Verbosity::Brief => "brief",
            Verbosity::Silent => "silent",
        }
    }
}

// ---------------------------------------------------------------------------
// The behavior types — NON-CONSEQUENTIAL by construction
// ---------------------------------------------------------------------------

/// The proactive behavior knobs a focus profile may tune. This is the WHOLE
/// surface a profile touches — and notice what is NOT here: no permission, no
/// gate, no confirm, no master switch, no voice-id, no lockdown, no autonomy
/// level, no consequential-action flag. Those live in `integrations`,
/// `confirm`, `lockdown`, `policy` — and `apply_profile` neither takes nor
/// returns any of them. A profile literally cannot reach them through this type.
///
/// The three knobs:
///   * `surfacing` — the set of signal CATEGORIES allowed to surface.
///   * `verbosity` — how much a surfaced brief says.
///   * `suggestions_quieted` — whether the proactive-suggestion feed is held back.
///
/// All three are NON-CONSEQUENTIAL: they govern which already-permitted,
/// outward-call-free intel the user sees, never whether an action may run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseBehavior {
    /// Categories allowed to surface. The base "show everything" is
    /// [`SignalCategory::all`].
    pub surfacing: Vec<SignalCategory>,
    /// Brief verbosity. Base is [`Verbosity::Full`].
    pub verbosity: Verbosity,
    /// Whether the proactive-suggestion feed is quieted. Base is `false`
    /// (suggestions surface as today, still behind their own `[proactive].suggest`
    /// gate — focus does not open that gate, it can only further quiet).
    pub suggestions_quieted: bool,
}

impl Default for BaseBehavior {
    /// Today's behavior: every category surfaces, full verbosity, suggestions not
    /// quieted by focus. This is the base every profile tunes DOWN from.
    fn default() -> Self {
        BaseBehavior {
            surfacing: SignalCategory::all().to_vec(),
            verbosity: Verbosity::Full,
            suggestions_quieted: false,
        }
    }
}

/// The tuned behavior `apply_profile` returns. SAME knobs as [`BaseBehavior`],
/// and — crucially — SAME absence of any permission/gate/autonomy field. There
/// is no constructor here that can add a consequential capability; `apply_profile`
/// only ever produces a value whose every knob is restrict-only relative to the
/// base it was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunedBehavior {
    pub surfacing: Vec<SignalCategory>,
    pub verbosity: Verbosity,
    pub suggestions_quieted: bool,
}

impl TunedBehavior {
    /// Whether `category` is allowed to surface under this tuned behavior.
    pub fn surfaces(&self, category: SignalCategory) -> bool {
        self.surfacing.contains(&category)
    }

    /// THE machine-checkable PERMISSION-NEUTRALITY predicate: is this tuned
    /// behavior NO BROADER than `base` on every axis? True iff:
    ///   * its surfacing set is a SUBSET of the base's (it added no category the
    ///     base suppressed);
    ///   * its verbosity is at least as terse as the base's (stepped down or held);
    ///   * its `suggestions_quieted` is at least as quiet (false->true or held,
    ///     never true->false).
    ///
    /// Because the type carries NO permission/gate/autonomy field, "no broader"
    /// on these three NON-CONSEQUENTIAL axes is the COMPLETE statement of "this
    /// profile loosened nothing" — there is no other axis on which it COULD
    /// loosen. The property test asserts this for every profile.
    ///
    /// `#[allow(dead_code)]`: this is the #24 GATE's machine-checkable predicate,
    /// exercised by the `property_no_profile_broadens_the_permission_surface`
    /// property test (a `#[cfg(test)]` consumer). It is kept as a first-class
    /// method (not test-local) so the invariant lives next to the type it guards.
    #[allow(dead_code)]
    pub fn is_no_broader_than(&self, base: &BaseBehavior) -> bool {
        let surfacing_subset = self
            .surfacing
            .iter()
            .all(|c| base.surfacing.contains(c));
        let verbosity_no_louder = self.verbosity.rank() >= base.verbosity.rank();
        // suggestions: base.quieted == true must stay true (can't un-quiet);
        // base.quieted == false may go either way (quieting more is fine).
        let suggestions_no_louder = !base.suggestions_quieted || self.suggestions_quieted;
        surfacing_subset && verbosity_no_louder && suggestions_no_louder
    }

    /// The HUD telemetry for the active focus (the `focus.active` card): which
    /// categories surface, the verbosity, whether suggestions are quieted, and
    /// the explicit permission-neutral posture. Secret-free; no permission/gate
    /// field exists to leak.
    pub fn telemetry(&self, profile: FocusProfile) -> serde_json::Value {
        let cats: Vec<&str> = self.surfacing.iter().map(|c| c.as_str()).collect();
        serde_json::json!({
            "profile": profile.as_str(),
            "surfacing": cats,
            "verbosity": self.verbosity.as_str(),
            "suggestions_quieted": self.suggestions_quieted,
            // Make the contract explicit on the wire so the HUD can state it.
            "permission_neutral": true,
            "raises_autonomy": false,
            "loosens_gate": false,
        })
    }
}

// ---------------------------------------------------------------------------
// The profiles
// ---------------------------------------------------------------------------

/// A focus profile: the named lens the operator selects. `Default` is the
/// identity (today's behavior). The others quiet progressively more:
///   * `Work` — silences News + Routine (stay heads-down on work intel: calendar,
///     mail, health still surface; market quiets).
///   * `Sleep` — silences everything EXCEPT Critical, brief verbosity, quiets
///     suggestions (the user is asleep; only a genuinely critical thing surfaces).
///   * `DeepFocus` — surfaces NOTHING but Critical, Silent digest, quiets
///     suggestions (the most restrictive: a true do-not-disturb that still lets a
///     critical signal through).
///   * `Custom(name)` — a named custom profile. It carries no extra power: a
///     custom profile is built by the SAME restrict-only construction (it can
///     only narrow the base), so an operator-named profile can never be broader
///     than the base either. The name is cosmetic (telemetry/copy).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FocusProfile {
    Default,
    Work,
    Sleep,
    DeepFocus,
    /// A named custom profile. The name is cosmetic; the behavior is the
    /// restrict-only `Custom` table below.
    Custom(String),
}

impl FocusProfile {
    /// A stable short string for telemetry/copy.
    pub fn as_str(&self) -> &'static str {
        match self {
            FocusProfile::Default => "default",
            FocusProfile::Work => "work",
            FocusProfile::Sleep => "sleep",
            FocusProfile::DeepFocus => "deep_focus",
            FocusProfile::Custom(_) => "custom",
        }
    }

    /// Parse a `[focus].profile` config string into a profile. Empty/whitespace/
    /// "default" => `Default` (the identity — today's behavior). The recognized
    /// names map to their profiles. Any OTHER non-blank string is a NAMED CUSTOM
    /// profile (the name is cosmetic; the behavior is the restrict-only `Custom`
    /// table) — so even a typo can only ever QUIET, never broaden (fail SAFE by
    /// CONSTRUCTION, not by degrading to Default).
    pub fn from_config_str(s: &str) -> FocusProfile {
        match s.trim().to_lowercase().as_str() {
            "" | "default" => FocusProfile::Default,
            "work" => FocusProfile::Work,
            "sleep" => FocusProfile::Sleep,
            "deep_focus" | "deepfocus" | "deep-focus" => FocusProfile::DeepFocus,
            // A custom profile name. Preserve the original (trimmed) string for
            // telemetry copy, but the BEHAVIOR is the restrict-only Custom table.
            other => FocusProfile::Custom(other.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Trigger -> category (the EDITH single-card surface's focus axis)
// ---------------------------------------------------------------------------

/// Map an EDITH anticipation [`crate::anticipate::TriggerKind`] to the focus
/// [`SignalCategory`] used to decide whether the active profile silences its
/// single-card surface. A low-disk reading is CRITICAL (never silenced — a full
/// disk can break the machine); calendar/mail/mem-high map to their own
/// categories; a market move is News-adjacent low priority. PURE; no clock, no
/// state. Keeps the live-tick wiring honest: the SAME critical-floor rule the
/// snapshot->brief conversion uses governs the single-card surface too.
pub fn category_for_trigger(kind: crate::anticipate::TriggerKind) -> SignalCategory {
    use crate::anticipate::TriggerKind;
    match kind {
        // A low disk is critical — it survives even DeepFocus.
        TriggerKind::DiskLow => SignalCategory::Critical,
        TriggerKind::Calendar => SignalCategory::Calendar,
        TriggerKind::Mail => SignalCategory::Mail,
        TriggerKind::MemHigh => SignalCategory::Health,
        TriggerKind::Market => SignalCategory::Market,
    }
}

// ---------------------------------------------------------------------------
// apply_profile — PURE, restrict-only
// ---------------------------------------------------------------------------

/// Apply a focus profile to a base behavior, returning the tuned behavior.
///
/// PURE and restrict-only BY CONSTRUCTION. Every branch builds its result by
/// REMOVING categories from `base.surfacing` (never adding), stepping verbosity
/// DOWN or holding (never up), and only ever flipping `suggestions_quieted`
/// false->true (never true->false). The `Default` branch returns the base
/// unchanged (the identity). Because [`TunedBehavior`] has no permission/gate/
/// autonomy field, there is no branch that COULD return a broader posture — the
/// strongest thing any branch does is silence more.
///
/// The invariant `apply_profile(p, base).is_no_broader_than(base)` holds for
/// EVERY `p` and EVERY `base` — proven by the property test.
pub fn apply_profile(profile: &FocusProfile, base: &BaseBehavior) -> TunedBehavior {
    // Helper: keep only the base categories NOT in `silence` (subset by
    // construction — we can only drop, never add).
    let keep_except = |silence: &[SignalCategory]| -> Vec<SignalCategory> {
        base.surfacing
            .iter()
            .copied()
            .filter(|c| !silence.contains(c))
            .collect()
    };
    // Helper: the terser of (base, requested) verbosity — never louder than base.
    let step_down = |requested: Verbosity| -> Verbosity {
        if requested.rank() >= base.verbosity.rank() {
            requested
        } else {
            base.verbosity
        }
    };
    // Helper: quiet suggestions at least as much as the base (OR with base).
    let quiet = |q: bool| -> bool { q || base.suggestions_quieted };

    match profile {
        // IDENTITY: today's behavior, byte-for-byte. The shipped default.
        FocusProfile::Default => TunedBehavior {
            surfacing: base.surfacing.clone(),
            verbosity: base.verbosity,
            suggestions_quieted: base.suggestions_quieted,
        },
        // WORK: heads-down — silence News + Routine (and Market, a non-work
        // distraction); keep calendar/mail/health/critical. Full verbosity (work
        // intel is wanted in full), suggestions not additionally quieted.
        FocusProfile::Work => TunedBehavior {
            surfacing: keep_except(&[
                SignalCategory::News,
                SignalCategory::Routine,
                SignalCategory::Market,
            ]),
            verbosity: step_down(Verbosity::Full),
            suggestions_quieted: quiet(false),
        },
        // SLEEP: only Critical surfaces; everything else is silenced. Brief
        // verbosity for the rare critical thing; suggestions quieted.
        FocusProfile::Sleep => TunedBehavior {
            surfacing: keep_except(&[
                SignalCategory::Calendar,
                SignalCategory::Mail,
                SignalCategory::Health,
                SignalCategory::Market,
                SignalCategory::News,
                SignalCategory::Routine,
            ]),
            verbosity: step_down(Verbosity::Brief),
            suggestions_quieted: quiet(true),
        },
        // DEEP FOCUS: the most restrictive. Only Critical surfaces, the digest is
        // Silent (no brief at all), suggestions quieted. A true do-not-disturb —
        // but a genuinely critical signal still gets through (the floor).
        FocusProfile::DeepFocus => TunedBehavior {
            surfacing: keep_except(&[
                SignalCategory::Calendar,
                SignalCategory::Mail,
                SignalCategory::Health,
                SignalCategory::Market,
                SignalCategory::News,
                SignalCategory::Routine,
            ]),
            verbosity: step_down(Verbosity::Silent),
            suggestions_quieted: quiet(true),
        },
        // CUSTOM: a named profile. It carries no special power — it is built by
        // the SAME restrict-only construction. The shipped custom behavior quiets
        // News + Routine + Market and steps to Brief (a sensible "fewer
        // interruptions" lens). An operator who wants a different custom mix edits
        // this table; whatever they pick, `keep_except`/`step_down`/`quiet` make
        // it IMPOSSIBLE to broaden the base. The name is cosmetic.
        FocusProfile::Custom(_) => TunedBehavior {
            surfacing: keep_except(&[
                SignalCategory::News,
                SignalCategory::Routine,
                SignalCategory::Market,
            ]),
            verbosity: step_down(Verbosity::Brief),
            suggestions_quieted: quiet(true),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every profile we ship, including a named custom, for the table + property
    /// tests. Exhaustive over the profile space (Custom stands in for the named
    /// family — every Custom shares one restrict-only table).
    fn all_profiles() -> Vec<FocusProfile> {
        vec![
            FocusProfile::Default,
            FocusProfile::Work,
            FocusProfile::Sleep,
            FocusProfile::DeepFocus,
            FocusProfile::Custom("study".to_string()),
        ]
    }

    // =====================================================================
    // DEFAULT == TODAY: the identity (the shipped neutral default)
    // =====================================================================

    #[test]
    fn default_profile_is_the_identity_todays_behavior() {
        let base = BaseBehavior::default();
        let tuned = apply_profile(&FocusProfile::Default, &base);
        assert_eq!(tuned.surfacing, base.surfacing, "default surfaces everything the base does");
        assert_eq!(tuned.verbosity, base.verbosity, "default keeps base verbosity");
        assert_eq!(
            tuned.suggestions_quieted, base.suggestions_quieted,
            "default does not quiet suggestions"
        );
        // The identity over ANY base, not just the canonical one.
        let custom_base = BaseBehavior {
            surfacing: vec![SignalCategory::Critical, SignalCategory::Mail],
            verbosity: Verbosity::Brief,
            suggestions_quieted: true,
        };
        let tuned = apply_profile(&FocusProfile::Default, &custom_base);
        assert_eq!(
            TunedBehavior {
                surfacing: custom_base.surfacing.clone(),
                verbosity: custom_base.verbosity,
                suggestions_quieted: custom_base.suggestions_quieted,
            },
            tuned,
            "Default is the identity over any base"
        );
    }

    #[test]
    fn shipped_default_config_profile_is_default() {
        // The ships-NEUTRAL contract: an empty/"default" config string parses to
        // the identity profile, so the shipped config reproduces today's behavior.
        assert_eq!(FocusProfile::from_config_str(""), FocusProfile::Default);
        assert_eq!(FocusProfile::from_config_str("default"), FocusProfile::Default);
        // A blank-but-whitespace value also degrades to the identity.
        assert_eq!(FocusProfile::from_config_str("   "), FocusProfile::Default);
        // An unrecognized non-blank value is a NAMED CUSTOM profile — which is
        // ITSELF restrict-only (it can only quiet, never broaden), so an operator
        // typo can never accidentally LOOSEN anything. Safety here is the
        // restrict-only construction, not a degrade-to-default.
        let unknown = FocusProfile::from_config_str("nonsense");
        assert_eq!(unknown, FocusProfile::Custom("nonsense".to_string()));
        assert!(
            apply_profile(&unknown, &BaseBehavior::default()).is_no_broader_than(&BaseBehavior::default()),
            "an unknown profile name is a restrict-only custom profile, never broader"
        );
        // The config default (FocusConfig::default) is "default".
        let cfg = crate::config::FocusConfig::default();
        assert_eq!(cfg.profile, "default", "[focus].profile ships \"default\"");
        assert_eq!(
            FocusProfile::from_config_str(&cfg.profile),
            FocusProfile::Default,
            "the shipped config profile is the identity"
        );
    }

    // =====================================================================
    // THE PROFILE TABLE: each non-default profile only RESTRICTS/QUIETS
    // =====================================================================

    #[test]
    fn work_silences_news_and_routine_keeps_work_intel() {
        let base = BaseBehavior::default();
        let t = apply_profile(&FocusProfile::Work, &base);
        assert!(!t.surfaces(SignalCategory::News), "work silences news");
        assert!(!t.surfaces(SignalCategory::Routine), "work silences routine");
        assert!(!t.surfaces(SignalCategory::Market), "work silences market");
        // Work intel still surfaces.
        assert!(t.surfaces(SignalCategory::Calendar));
        assert!(t.surfaces(SignalCategory::Mail));
        assert!(t.surfaces(SignalCategory::Critical), "critical never silenced");
    }

    #[test]
    fn sleep_surfaces_only_critical() {
        let base = BaseBehavior::default();
        let t = apply_profile(&FocusProfile::Sleep, &base);
        assert!(t.surfaces(SignalCategory::Critical), "critical still gets through asleep");
        for c in [
            SignalCategory::Calendar,
            SignalCategory::Mail,
            SignalCategory::Health,
            SignalCategory::Market,
            SignalCategory::News,
            SignalCategory::Routine,
        ] {
            assert!(!t.surfaces(c), "sleep silences {c:?}");
        }
        assert!(t.suggestions_quieted, "sleep quiets suggestions");
        assert_eq!(t.verbosity, Verbosity::Brief);
    }

    #[test]
    fn deep_focus_is_the_most_restrictive_but_still_lets_critical_through() {
        let base = BaseBehavior::default();
        let t = apply_profile(&FocusProfile::DeepFocus, &base);
        // Only critical surfaces.
        assert_eq!(t.surfacing, vec![SignalCategory::Critical]);
        assert!(t.surfaces(SignalCategory::Critical), "even deep focus passes critical");
        assert_eq!(t.verbosity, Verbosity::Silent, "deep focus renders no digest");
        assert!(t.suggestions_quieted, "deep focus quiets suggestions");
    }

    #[test]
    fn custom_profile_is_restrict_only_and_named() {
        let base = BaseBehavior::default();
        let p = FocusProfile::from_config_str("study");
        assert_eq!(p, FocusProfile::Custom("study".to_string()), "an unknown name is a custom profile");
        let t = apply_profile(&p, &base);
        // Restrict-only: it dropped categories, never added; it cannot exceed base.
        assert!(t.is_no_broader_than(&base), "a custom profile can never broaden the base");
        assert!(t.surfaces(SignalCategory::Critical), "critical floor holds for custom too");
    }

    // =====================================================================
    // PERMISSION-NEUTRALITY: the property test — NO profile broadens
    // =====================================================================

    /// A small spread of bases to run the property over: the full base, an
    /// already-narrowed base, an already-quieted base, and a critical-only base.
    /// The invariant must hold against EVERY base, not just the canonical one —
    /// applying a profile to an already-restricted behavior must still only
    /// restrict further (composition stays restrict-only).
    fn bases() -> Vec<BaseBehavior> {
        vec![
            BaseBehavior::default(),
            BaseBehavior {
                surfacing: vec![SignalCategory::Critical, SignalCategory::Calendar, SignalCategory::Mail],
                verbosity: Verbosity::Brief,
                suggestions_quieted: false,
            },
            BaseBehavior {
                surfacing: vec![SignalCategory::Critical, SignalCategory::News],
                verbosity: Verbosity::Full,
                suggestions_quieted: true,
            },
            BaseBehavior {
                surfacing: vec![SignalCategory::Critical],
                verbosity: Verbosity::Silent,
                suggestions_quieted: true,
            },
        ]
    }

    #[test]
    fn property_no_profile_broadens_the_permission_surface() {
        // THE #24 GATE, machine-checked: for EVERY profile and EVERY base, the
        // tuned behavior is NO BROADER than the base on every axis. A profile can
        // only ever make DARWIN quieter — never surface a category the base
        // suppressed, never get louder, never un-quiet suggestions.
        for base in bases() {
            for profile in all_profiles() {
                let tuned = apply_profile(&profile, &base);
                assert!(
                    tuned.is_no_broader_than(&base),
                    "profile {:?} broadened base {:?} -> {:?}",
                    profile,
                    base,
                    tuned
                );
                // Surfacing is strictly a SUBSET (no category appears that the
                // base didn't already allow).
                for c in &tuned.surfacing {
                    assert!(
                        base.surfacing.contains(c),
                        "profile {:?} surfaced {:?} which base {:?} suppressed",
                        profile,
                        c,
                        base
                    );
                }
                // Verbosity never louder than base.
                assert!(
                    tuned.verbosity.rank() >= base.verbosity.rank(),
                    "profile {:?} made the digest louder than base {:?}",
                    profile,
                    base
                );
                // Suggestions: a base that quieted them stays quieted.
                if base.suggestions_quieted {
                    assert!(
                        tuned.suggestions_quieted,
                        "profile {:?} un-quieted suggestions the base had quieted",
                        profile
                    );
                }
            }
        }
    }

    #[test]
    fn applying_a_profile_twice_never_re_broadens_idempotent_restriction() {
        // Composing a profile onto its own output must not re-broaden: feed the
        // tuned result back as a base and re-apply — the second pass is still no
        // broader than the first. (Restriction composes monotonically.)
        for profile in all_profiles() {
            let base = BaseBehavior::default();
            let once = apply_profile(&profile, &base);
            let twice_base = BaseBehavior {
                surfacing: once.surfacing.clone(),
                verbosity: once.verbosity,
                suggestions_quieted: once.suggestions_quieted,
            };
            let twice = apply_profile(&profile, &twice_base);
            assert!(
                twice.is_no_broader_than(&twice_base),
                "re-applying {profile:?} re-broadened its own output"
            );
        }
    }

    // =====================================================================
    // TYPE-LEVEL ARGUMENT: TunedBehavior carries no permission/gate field
    // =====================================================================

    #[test]
    fn tuned_behavior_has_only_non_consequential_knobs() {
        // This test is a STANDING ASSERTION (read with the struct def): the only
        // way to read anything off a TunedBehavior is the three NON-CONSEQUENTIAL
        // knobs below. There is no `.gate`, `.confirm`, `.allow_consequential`,
        // `.autonomy`, `.voice_id`, `.lockdown`, `.permission` — the type does not
        // have them, so `apply_profile` provably cannot return one. If a future
        // edit added a permission field to TunedBehavior, this test's exhaustive
        // destructuring would FAIL TO COMPILE, forcing a re-review of the #24 gate.
        let t = apply_profile(&FocusProfile::DeepFocus, &BaseBehavior::default());
        let TunedBehavior {
            surfacing: _,
            verbosity: _,
            suggestions_quieted: _,
        } = t;
        // (No further assertions needed — the exhaustive pattern IS the proof.)
    }

    // =====================================================================
    // TELEMETRY shape — the HUD focus.active card
    // =====================================================================

    #[test]
    fn telemetry_states_the_permission_neutral_posture() {
        let t = apply_profile(&FocusProfile::Sleep, &BaseBehavior::default());
        let v = t.telemetry(FocusProfile::Sleep);
        assert_eq!(v["profile"], "sleep");
        assert_eq!(v["verbosity"], "brief");
        assert_eq!(v["suggestions_quieted"], true);
        // The contract is on the wire so the HUD copy is grounded, not hardcoded.
        assert_eq!(v["permission_neutral"], true);
        assert_eq!(v["raises_autonomy"], false);
        assert_eq!(v["loosens_gate"], false);
        // Surfacing carries only the critical floor under sleep.
        assert_eq!(v["surfacing"], serde_json::json!(["critical"]));
    }
}

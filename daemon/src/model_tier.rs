//! MODEL TIER + RUNTIME OVERRIDE — the honest, swap-only "which brain answers"
//! layer. This module decides WHICH model tier (Local / Fast / Heavy) answers a
//! turn, refining the existing binary cloud-vs-local contract in
//! [`crate::router`] WITHOUT touching any safety posture.
//!
//! ## What a tier means (honestly)
//!
//!   * `Heavy`  — the cloud heavy model ([cloud].heavy_model, Opus): the most
//!     capable, used for genuinely complex turns. Needs a cloud key + reachability.
//!   * `Fast`   — the cloud fast model ([cloud].fast_model, Haiku): quick + cheap,
//!     for trivial/low-complexity turns. Needs a cloud key + reachability.
//!   * `Local`  — the resident on-device 4B (no cloud call AT ALL). The utterance
//!     and its content stay on this machine — a REAL privacy benefit. But the
//!     local 4B has a genuine CAPABILITY CEILING (near-deterministic on some
//!     tasks); `Local` is NOT Opus quality, and nothing here pretends it is.
//!
//! ## Precedence (resolve_tier): Override > Auto > Fallback
//!
//!   1. **Override** — an explicit voice command ("use the powerful model", "go
//!      offline") set a process-global override. It WINS over the auto heuristic.
//!      A `Local` override forces local: NO cloud call is made (the privacy path).
//!   2. **Auto** — no override: the configured [router].conversation_route is the
//!      durable default tier, refined by THIS turn's difficulty (a trivial turn
//!      can step DOWN to Fast/Local to save cost+latency; a heavy turn steps UP to
//!      Heavy). This is a HEURISTIC — it can be wrong, which is exactly why it is
//!      overridable and surfaced (the `model.tier` telemetry carries the reason).
//!   3. **Fallback** — if the resolved tier is a cloud tier but the cloud is NOT
//!      reachable (no key / offline), OR the cloud call later errors, fall back to
//!      `Local` (Reason::Fallback) — the existing degrade path, now named. A
//!      `Local` resolution (override OR fallback OR offline) means the router makes
//!      NO cloud call.
//!
//! ## Swap-only — safety is UNCHANGED at every tier
//!
//! This module changes only WHICH model string (or the local path) the router
//! passes to the completion call. It does NOT touch the consequential-confirmation
//! gate, the `[integrations].allow_consequential` master switch, the owner
//! voice-id gate, or the per-agent tool allowlist — those behave identically at
//! Local, Fast, and Heavy. There is no tier that loosens a gate.
//!
//! ## Persistence — PROCESS-LOCAL (documented choice)
//!
//! The override lives in a process-global [`Mutex`] (mirroring the voice-id
//! `TURN_GATE` / eval `USAGE_SINK` runtime-state pattern). It is intentionally
//! PROCESS-LOCAL: it resets to the config default ([router].conversation_route) on
//! restart. The durable default is therefore always the config — a voice swap is a
//! deliberate, in-session steering action, not a config edit, and a fresh process
//! starts from the owner's written intent. (If a future "make offline stick across
//! restarts" requirement lands, the seam is a single state fact `meta.model_override`
//! read at startup into `set_override`; this module's API would not change.)
//!
//! Everything here is HERMETIC: [`resolve_tier`] and [`classify_model_swap`] are
//! pure functions; the override get/set is a process-global with a `#[cfg(test)]`
//! thread-local seam so tests never leak state into each other.

use std::sync::Mutex;

use crate::config::Config;

/// The model tier answering a turn. Ordered Local < Fast < Heavy by capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// The resident on-device 4B. No cloud call — the utterance stays on-device.
    Local,
    /// The cloud fast model (Haiku): quick + cheap.
    Fast,
    /// The cloud heavy model (Opus): the most capable.
    Heavy,
}

impl Tier {
    /// Stable identifier for telemetry / the HUD tier indicator.
    pub fn as_str(&self) -> &'static str {
        match self {
            Tier::Local => "local",
            Tier::Fast => "fast",
            Tier::Heavy => "heavy",
        }
    }

    /// An HONEST one-line label for the HUD / a spoken note. Local is named as the
    /// on-device privacy path with its capability ceiling implied ("on-device"),
    /// never dressed up as cloud-grade. Part of the tier API the HUD tier
    /// indicator reads; surfaced now so the honest copy lives with the enum.
    #[allow(dead_code)] // HUD-facing tier label; consumed by the indicator card
    pub fn honest_label(&self) -> &'static str {
        match self {
            Tier::Local => "on-device (private, capability-limited)",
            Tier::Fast => "cloud fast",
            Tier::Heavy => "cloud heavy (most capable)",
        }
    }

    /// Whether this tier makes a cloud call. `Local` does NOT (the privacy path);
    /// `Fast`/`Heavy` do (and therefore need a cloud key + reachability).
    pub fn is_cloud(&self) -> bool {
        matches!(self, Tier::Fast | Tier::Heavy)
    }

    /// Map a [router].conversation_route config string to its default tier.
    /// "cloud_heavy" -> Heavy, "cloud_fast" -> Fast, anything else (incl. "local"
    /// and any unknown value) -> Local (the safe, always-available default).
    pub fn from_route(route: &str) -> Tier {
        match route {
            "cloud_heavy" => Tier::Heavy,
            "cloud_fast" => Tier::Fast,
            _ => Tier::Local,
        }
    }
}

/// WHY a tier was chosen — surfaced in `model.tier` telemetry so the HUD can show
/// MANUAL vs AUTO vs a degrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// An explicit voice override is in force (MANUAL).
    Override,
    /// No override; the auto difficulty heuristic picked it (AUTO).
    Auto,
    /// A cloud tier was wanted but the cloud was unreachable, or the cloud call
    /// errored — degraded to Local. The existing degrade path, now named.
    Fallback,
}

impl Reason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Reason::Override => "override",
            Reason::Auto => "auto",
            Reason::Fallback => "fallback",
        }
    }
}

/// What the router should actually run: either the on-device path, or a cloud
/// completion with this exact model string. Returned by [`tier_to_model`] so the
/// "tier -> model string" mapping is one verified place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelChoice {
    /// The on-device 4B path — no cloud call. The router degrades/answers locally.
    Local,
    /// A cloud completion using this model string ([cloud].heavy_model or
    /// [cloud].fast_model).
    Cloud(String),
}

// ---------------------------------------------------------------------------
// PROCESS-GLOBAL RUNTIME OVERRIDE (mirrors voiceid::TURN_GATE / eval::USAGE_SINK)
// ---------------------------------------------------------------------------

/// The process-global model-tier override. `None` = no manual override is in
/// force, which reads as "use the config default + auto heuristic" — the safe
/// default, exactly like `TURN_GATE` defaulting to OFF. Process-local: resets to
/// `None` (config default) on restart (documented above).
static OVERRIDE: Mutex<Option<Tier>> = Mutex::new(None);

// Test-only thread-local override mirroring voiceid's `GATE_OVERRIDE`: a test
// forces an override on its OWN thread without touching the process-global slot
// other parallel tests rely on. Compiled out of release. (Plain comment: rustdoc
// can't attach a doc comment to a macro invocation — it would warn.)
#[cfg(test)]
thread_local! {
    static OVERRIDE_TL: std::cell::Cell<Option<Option<Tier>>> = const { std::cell::Cell::new(None) };
}

/// Install (or clear, with `None`) the manual tier override. Called by the
/// voice-command handler: a Heavy/Fast/Local intent sets `Some(tier)`; the Auto
/// intent clears it (`None`) back to the config default. Poison-tolerant.
pub fn set_override(tier: Option<Tier>) {
    #[cfg(test)]
    {
        // If a test has installed a thread-local seam, write there so the global
        // slot stays untouched for other parallel tests.
        if OVERRIDE_TL.with(|c| c.get().is_some()) {
            OVERRIDE_TL.with(|c| c.set(Some(tier)));
            return;
        }
    }
    *OVERRIDE.lock().unwrap_or_else(|p| p.into_inner()) = tier;
}

/// The current manual override (`None` = none in force; use config default).
/// Poison-tolerant.
pub fn current_override() -> Option<Tier> {
    #[cfg(test)]
    {
        if let Some(seam) = OVERRIDE_TL.with(|c| c.get()) {
            return seam;
        }
    }
    *OVERRIDE.lock().unwrap_or_else(|p| p.into_inner())
}

/// Clear the override back to the config default. Convenience for the Auto intent
/// and for turn cleanup if a future requirement scopes the override per-turn. The
/// live Auto intent path uses `set_override(None)` via `intent.to_override()`;
/// this named alias is kept for callers (and tests) that clear explicitly.
#[allow(dead_code)] // public override-API alias; exercised by tests, kept for callers
pub fn clear_override() {
    set_override(None);
}

/// `#[cfg(test)]`-only RAII guard forcing `current_override()` on the current
/// thread, restoring the prior thread-local state on drop (so an override never
/// leaks into another parallel test). The whole seam is `cfg(test)`.
#[cfg(test)]
pub(crate) struct OverrideGuard {
    prev: Option<Option<Tier>>,
}

#[cfg(test)]
impl OverrideGuard {
    /// Begin a thread-local override seam, initialized to `start` (so the global
    /// slot is never read for the lifetime of the guard on this thread).
    pub(crate) fn force(start: Option<Tier>) -> Self {
        let prev = OVERRIDE_TL.with(|c| c.replace(Some(start)));
        Self { prev }
    }
}

#[cfg(test)]
impl Drop for OverrideGuard {
    fn drop(&mut self) {
        OVERRIDE_TL.with(|c| c.set(self.prev));
    }
}

// ---------------------------------------------------------------------------
// RESOLVE: Override > Auto-difficulty > Fallback
// ---------------------------------------------------------------------------

/// Resolve the tier for a turn, applying the precedence Override > Auto > Fallback.
///
/// * `cfg`             — for the [router].conversation_route default + the
///   [cloud] model strings (read by [`tier_to_model`]).
/// * `override_tier`   — the manual override (usually [`current_override`]); when
///   `Some`, it WINS (Reason::Override) over the auto heuristic.
/// * `complexity`      — the classifier's `complexity` ("heavy" => a complex turn;
///   anything else => low/trivial) used by the AUTO heuristic to step up/down.
/// * `confidence`      — the classifier confidence; below `low_conf_threshold` a
///   turn is treated as needing the more capable tier (mirrors the existing
///   low-confidence-to-cloud contract).
/// * `low_conf_threshold` — [router].cloud_confidence_threshold.
/// * `cloud_reachable` — whether a cloud call can be made at all this turn (key +
///   reachability). If a cloud tier is resolved but this is false, it degrades to
///   Local with Reason::Fallback — NO cloud call.
///
/// Returns `(Tier, Reason)`. A `Local` result (override / offline / fallback) is
/// the signal to the router that NO cloud completion is attempted this turn.
pub fn resolve_tier(
    cfg: &Config,
    override_tier: Option<Tier>,
    complexity: &str,
    confidence: f64,
    low_conf_threshold: f64,
    cloud_reachable: bool,
) -> (Tier, Reason) {
    // 1. OVERRIDE wins. An explicit Local override is the privacy path: force
    //    Local, NO cloud call, regardless of cloud reachability.
    if let Some(forced) = override_tier {
        if forced.is_cloud() && !cloud_reachable {
            // The user asked for a cloud tier but there is no cloud this turn —
            // honor the intent's spirit honestly: fall back to Local rather than
            // pretend. The HUD shows Fallback so it is never silently wrong.
            return (Tier::Local, Reason::Fallback);
        }
        return (forced, Reason::Override);
    }

    // 2. AUTO — no override. Start from the config default tier, then refine by
    //    this turn's difficulty (the heuristic). This REFINES today's binary
    //    cloud-vs-local contract; the default tier preserves current behavior.
    let default_tier = Tier::from_route(&cfg.router.conversation_route);
    let auto = auto_tier(default_tier, complexity, confidence, low_conf_threshold);

    // 3. FALLBACK — a cloud tier with no cloud this turn degrades to Local.
    if auto.is_cloud() && !cloud_reachable {
        return (Tier::Local, Reason::Fallback);
    }
    (auto, Reason::Auto)
}

/// The AUTO difficulty heuristic: refine the configured default tier by this
/// turn's difficulty. A HEURISTIC (can be wrong) — overridable + surfaced.
///
///   * A HEAVY (or low-confidence) turn wants the MOST capable available tier:
///     step UP to Heavy when the default is a cloud tier (a cloud-fast default
///     still escalates a genuinely hard turn to Heavy). A `Local` default never
///     silently becomes cloud (offline/private intent is preserved).
///   * A trivial/low-complexity, confident turn can step DOWN one notch from
///     Heavy to Fast to save cost+latency (Heavy is overkill for a greeting). It
///     never steps a cloud default down to Local on its own — going on-device is a
///     deliberate (override) choice, not an auto cost optimization.
fn auto_tier(default_tier: Tier, complexity: &str, confidence: f64, low_conf_threshold: f64) -> Tier {
    let hard = complexity == "heavy" || confidence < low_conf_threshold;
    match default_tier {
        // Offline/private default: AUTO never silently goes to cloud. The user
        // chose local; only an explicit override (or the config) leaves local.
        Tier::Local => Tier::Local,
        // Cloud-fast default: a genuinely hard turn escalates to Heavy; otherwise
        // stay Fast (already the cheap tier).
        Tier::Fast => {
            if hard {
                Tier::Heavy
            } else {
                Tier::Fast
            }
        }
        // Cloud-heavy default (the shipped default): a hard turn stays Heavy; a
        // trivial, confident turn steps down to Fast to save cost+latency.
        Tier::Heavy => {
            if hard {
                Tier::Heavy
            } else {
                Tier::Fast
            }
        }
    }
}

/// The EFFECTIVE tier for ancillary "is the operator offline?" decisions (e.g. the
/// voice tier deciding whether cloud TTS is allowed) — NOT the per-turn routing
/// decision (`resolve_tier` owns that, with this turn's difficulty/confidence).
///
/// It is the manual override when one is in force, otherwise the config default
/// ([router].conversation_route). The ONE thing callers care about is whether this
/// is [`Tier::Local`]: a `Local` override ("work offline / go offline / stay on
/// device") OR a local default means the operator wants ON-DEVICE, so the voice
/// tier must keep speech on-device too — exactly tying voice to the model-swap
/// "work offline" intent. Process-global override read; poison-tolerant via
/// [`current_override`]. Pure given `cfg` + the override.
pub fn active_tier(cfg: &Config, override_tier: Option<Tier>) -> Tier {
    override_tier.unwrap_or_else(|| Tier::from_route(&cfg.router.conversation_route))
}

/// Map a resolved [`Tier`] to the concrete [`ModelChoice`] the router runs:
/// Heavy -> [cloud].heavy_model, Fast -> [cloud].fast_model, Local -> the
/// on-device path (no cloud call). One verified place for the tier->model string.
pub fn tier_to_model(tier: Tier, cfg: &Config) -> ModelChoice {
    match tier {
        Tier::Heavy => ModelChoice::Cloud(cfg.cloud.heavy_model.clone()),
        Tier::Fast => ModelChoice::Cloud(cfg.cloud.fast_model.clone()),
        Tier::Local => ModelChoice::Local,
    }
}

// ---------------------------------------------------------------------------
// MULTI-RESIDENT LOCAL SUB-TIER (task #17) — pick WHICH warm local model the
// (already-chosen) Local tier answers with. HONESTY FIRST.
//
// This is a refinement WITHIN the Local tier ONLY. It does NOT change which TIER
// is chosen (resolve_tier owns that), does NOT touch the consequential gate, and
// makes NO cloud call — Local is still the on-device, no-cloud path at every
// sub-choice. It only lets the local tier pick between models that are ALREADY
// warm: a small "local-fast" model for trivial offline turns vs the capable base
// for harder ones. The benefit (an instant swap, no reload) exists ONLY when the
// server actually kept >1 model warm, which is RAM-bounded and OFF by default
// (single-resident). When the warm-set is single-resident the sub-tier collapses
// to the base — exactly today's behavior — so a low-RAM Mac is unaffected.
//
// The policy here MIRRORS the Python keep-warm policy in inference/server.py
// (estimate_local_model_gib / plan_warm_set) so the daemon's HUD telemetry plan
// agrees with the server's actual warm-set. It is PURE arithmetic over the
// configured sizes/heuristic — no model, no load, no MLX — so the budget / admit
// / single-fallback decisions are unit-tested with synthetic sizes.
// ---------------------------------------------------------------------------

/// Conservative APPROX footprint (GiB of unified memory) the budgeting policy
/// assumes for a local model whose size is unknown (not in [models].local_sizes
/// and no heuristic match). Deliberately generous so an unknown model is treated
/// as COSTLY and must EARN a warm slot against the budget. MUST match server.py's
/// `DEFAULT_LOCAL_MODEL_GIB`.
pub const DEFAULT_LOCAL_MODEL_GIB: f64 = 3.0;

/// Best-effort APPROX resident footprint (GiB) for a local MLX model id, used
/// ONLY by the budgeting POLICY (never an allocation). Resolution order, mirroring
/// server.py's `estimate_local_model_gib`:
///   1. an explicit override in `sizes` ([models].local_sizes, id -> GiB),
///   2. a coarse heuristic on the id (param count x a per-bit-width factor),
///   3. [`DEFAULT_LOCAL_MODEL_GIB`] when nothing matches.
///      This is an ESTIMATE for keep-warm bookkeeping, NOT a measurement and NOT a
///      guarantee; the real resident size is device/quant/runtime dependent. PURE.
pub fn estimate_local_model_gib(
    model_id: &str,
    sizes: &std::collections::BTreeMap<String, f64>,
) -> f64 {
    if let Some(&g) = sizes.get(model_id) {
        if g > 0.0 {
            return g;
        }
        // A non-positive override is a misconfiguration; fall through to the
        // heuristic (the honest default) rather than admit a free model.
    }
    let name = model_id.to_lowercase();
    // Param count from a "<n>b" token in the id (e.g. "qwen3-4b" -> 4.0). The
    // 'b' must be followed by a non-alphanumeric, end of string, or "it"
    // (…-4bit) so a stray letter ("…-base") never reads as a size.
    let params_b = parse_params_b(&name);
    let params_b = match params_b {
        Some(p) => p,
        None => return DEFAULT_LOCAL_MODEL_GIB,
    };
    // Bytes-per-parameter by quantization. Factors include MLX overhead + a small
    // KV margin; intentionally a touch high so the budget is respected, not blown.
    // MUST match the per-bit factors in server.py's estimate_local_model_gib.
    let per_b = if name.contains("4bit") || name.contains("4-bit") || name.contains("q4") {
        0.6
    } else if name.contains("8bit") || name.contains("8-bit") || name.contains("q8") {
        1.15
    } else {
        2.2
    };
    // Round to 3 decimals to match the Python estimate exactly.
    ((params_b * per_b) * 1000.0).round() / 1000.0
}

/// Parse a `<n>b` parameter-count token out of a lowercased model id, matching
/// server.py's regex `(\d+(?:\.\d+)?)\s*b(?:[-_./]|$|it)`. Returns the param
/// count in billions (e.g. 4.0 for "qwen3-4b", 0.6 for "qwen3-0.6b-4bit"). PURE.
fn parse_params_b(name: &str) -> Option<f64> {
    let bytes = name.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    while i < n {
        // Scan a number: digits with an optional single '.' fractional part.
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < n && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i < n && bytes[i] == b'.' {
            let dot = i;
            i += 1;
            let frac_start = i;
            while i < n && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i == frac_start {
                // A trailing dot with no fraction ("4.b") — not a valid number;
                // keep only the integer part.
                i = dot;
            }
        }
        let num = &name[start..i];
        // Optional whitespace between the number and the 'b' (the regex's \s*).
        // Match ANY ASCII whitespace (space/tab/newline/CR/FF), not just a literal
        // space, for exact parity with server.py's \s* — no real MLX model id has
        // whitespace here, but this keeps the daemon's plan byte-identical to the
        // server's actual warm-set on any pathological id.
        let mut j = i;
        while j < n && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        // Require a 'b' followed by a boundary: a separator, end, or "it".
        if j < n && bytes[j] == b'b' {
            let after = j + 1;
            let boundary = after >= n
                || matches!(bytes[after], b'-' | b'_' | b'.' | b'/')
                || name[after..].starts_with("it");
            if boundary {
                if let Ok(p) = num.parse::<f64>() {
                    return Some(p);
                }
            }
        }
        // Not a size token; resume scanning AFTER this number.
    }
    None
}

/// PURE keep-warm POLICY (mirrors server.py's `plan_warm_set`). Decide which
/// local models may be kept resident at once under a RAM `budget_gib`, given the
/// always-resident `base_id` and the operator's `configured` extra warm ids
/// ([models].local_warm). Returns the ORDERED list of model ids the server is
/// ALLOWED to keep warm (base first, then the configured extras that fit, in
/// config order).
///
/// Rules (RAM-bounded, single-resident-safe):
///   * `base_id` is ALWAYS first in the result — it is the single-resident
///     fallback and the persona-cache/embedding model.
///   * A non-positive budget, OR a base whose estimate alone exceeds the budget,
///     => SINGLE-RESIDENT (base only, no extras): the safe low-RAM default.
///   * Each subsequent unique configured id is admitted ONLY while adding its
///     estimate keeps the running total <= budget; otherwise it is SKIPPED.
///     Scanning continues (a later, smaller model may still fit).
///
/// PURE arithmetic over `sizes`/heuristic estimates — no model, no load — so the
/// budget/admit/single-fallback decisions are unit-testable with synthetic sizes.
pub fn plan_warm_set(
    base_id: &str,
    configured: &[String],
    budget_gib: f64,
    sizes: &std::collections::BTreeMap<String, f64>,
) -> Vec<String> {
    let mut plan = vec![base_id.to_string()];
    let mut used = estimate_local_model_gib(base_id, sizes);
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    seen.insert(base_id);
    // Non-positive budget OR base over budget => single-resident (base only).
    if budget_gib <= 0.0 || used > budget_gib {
        return plan;
    }
    for mid in configured {
        let mid = mid.as_str();
        if mid.is_empty() || seen.contains(mid) {
            continue;
        }
        let cost = estimate_local_model_gib(mid, sizes);
        if used + cost <= budget_gib {
            plan.push(mid.to_string());
            seen.insert(mid);
            used += cost;
        }
        // else: does not fit -> skipped (never warm). Keep scanning.
    }
    plan
}

/// The Local-tier SUB-CHOICE: which warm local model answers this on-device turn.
/// Refines the Local tier ONLY — every variant is still the no-cloud path. When
/// the warm-set is single-resident every variant collapses to the base (today's
/// behavior), so this only ever matters when the server actually kept >1 warm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalSubTier {
    /// The small "local-fast" model (the first NON-base member of the warm plan):
    /// chosen for trivial/confident offline turns to answer faster. Falls back to
    /// the base when no extra is warm. LIVE as a telemetry label: emitted as the
    /// `local_sub` HUD indicator when AUTO answered on the fast model this turn.
    /// (Also an explicit-control variant reserved for a future voice/HUD toggle.)
    Fast,
    /// The capable base ([models].llm): the single-resident model + the safe
    /// default. Chosen for harder/low-confidence offline turns. LIVE as a telemetry
    /// label: emitted as the `local_sub` HUD indicator when the base answered a
    /// multi-resident turn. (Also an explicit-control variant for a future toggle.)
    Capable,
    /// AUTO by THIS turn's difficulty: a hard or low-confidence turn -> Capable;
    /// a trivial, confident turn -> Fast (if a fast model is warm, else Capable).
    /// This is what the live router threads (`local_model_for_turn`).
    Auto,
}

impl LocalSubTier {
    /// Stable identifier for `model.local_sub` telemetry / the HUD indicator.
    /// LIVE on the local path: `router::local_sub_for_turn` emits this in the
    /// per-turn `model.tier` payload and the HUD folds it into the resident-models
    /// FAST/CAPABLE indicator (`applyLocalSub` -> `localSubLabel`).
    pub fn as_str(&self) -> &'static str {
        match self {
            LocalSubTier::Fast => "fast",
            LocalSubTier::Capable => "capable",
            LocalSubTier::Auto => "auto",
        }
    }
}

/// Pick the warm LOCAL model id this on-device turn answers with, given the
/// server's allowed warm `plan` (base first, from [`plan_warm_set`]), the
/// requested `sub` choice, and this turn's difficulty. PURE — no load.
///
///   * SINGLE-RESIDENT (`plan` has only the base) => ALWAYS the base, whatever
///     `sub` asks. This is the default + low-RAM path: it is exactly today's
///     behavior (the one resident model answers every local turn).
///   * MULTI-RESIDENT: `Capable` -> the base; `Fast` -> the first non-base warm
///     model (the local-fast); `Auto` -> Fast for a trivial, confident turn, else
///     the base. A hard or low-confidence offline turn never silently downgrades
///     to the weaker fast model.
///
/// Returns the chosen model id (a member of `plan`). The daemon threads this id to
/// the generate/converse op as `local_model`; an unknown/empty id the server falls
/// back to the base, so this can never crash a turn.
pub fn select_local_model<'a>(
    plan: &'a [String],
    sub: LocalSubTier,
    complexity: &str,
    confidence: f64,
    low_conf_threshold: f64,
) -> &'a str {
    // Defensive: an empty plan should never happen (the base is always present),
    // but never index past it.
    let base = plan.first().map(String::as_str).unwrap_or("");
    let fast = plan.get(1).map(String::as_str); // the first non-base warm model
    // Single-resident (or a degenerate plan): only the base can answer.
    let fast = match fast {
        Some(f) => f,
        None => return base,
    };
    let hard = complexity == "heavy" || confidence < low_conf_threshold;
    match sub {
        LocalSubTier::Capable => base,
        LocalSubTier::Fast => fast,
        // AUTO: a hard/low-confidence turn keeps the capable base; an easy,
        // confident turn takes the faster model.
        LocalSubTier::Auto => {
            if hard {
                base
            } else {
                fast
            }
        }
    }
}

// ---------------------------------------------------------------------------
// BATTERY/THERMAL ADAPTIVE THROTTLING (#38) — a PURE policy over a synthetic
// (battery, on_ac, thermal) reading. HONESTY + PERF/RUNTIME ONLY.
//
// This influences ONLY the LOCAL sub-tier preference (prefer the cheaper Fast
// sub-tier) and a "defer heavy work" hint. It does NOT change WHICH tier is
// chosen (resolve_tier owns that), does NOT loosen a gate, and makes NO cloud
// call. The LIVE power reader (pmset / thermal pressure / IOKit) is DEVICE-GATED
// behind [power].adaptive: with the flag OFF the daemon feeds a NEUTRAL reading
// (None battery, on_ac=true, Nominal thermal) so the plan is always neutral and
// routing is byte-for-byte today's. The real battery/thermal benefit is only
// observable on-device and is NEVER measured headlessly.
// ---------------------------------------------------------------------------

/// The machine's THERMAL pressure level, mirroring macOS's
/// `ProcessInfo.thermalState` ladder (Nominal/Fair/Serious/Critical). Read live
/// ONLY when [power].adaptive is on (device-gated); under the OFF default the
/// policy is fed `Nominal` so it never throttles. PURE value — no I/O.
///
/// Fair/Serious/Critical are constructed by the DEVICE-GATED live thermal read
/// (`ProcessInfo.thermalState`, not wired in this headless build) and by the
/// hermetic policy tests; the headless live reader reports `Nominal` so the
/// policy never throttles on a guess — hence the allow (the device read + the
/// throttle_decision tests are the real constructors).
#[allow(dead_code)] // Fair/Serious/Critical: device-gated live thermal read + tests
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThermalState {
    /// No thermal pressure — the normal state.
    Nominal,
    /// Mild thermal pressure (fans up) — not yet a throttle trigger on its own.
    Fair,
    /// Serious thermal pressure — the OS is actively throttling; prefer Fast +
    /// defer heavy work.
    Serious,
    /// Critical thermal pressure — prefer Fast + defer heavy work (same action as
    /// Serious; the OS is aggressively throttling).
    Critical,
}

impl ThermalState {
    /// Stable identifier for `model.throttle` telemetry / the HUD indicator.
    /// Used by the device-gated thermal indicator + tests.
    #[allow(dead_code)] // HUD thermal indicator (device-gated) + tests
    pub fn as_str(&self) -> &'static str {
        match self {
            ThermalState::Nominal => "nominal",
            ThermalState::Fair => "fair",
            ThermalState::Serious => "serious",
            ThermalState::Critical => "critical",
        }
    }

    /// Whether this level is a throttle trigger (Serious/Critical — the OS is
    /// actively throttling). Fair/Nominal are NOT, on their own.
    fn is_pressured(&self) -> bool {
        matches!(self, ThermalState::Serious | ThermalState::Critical)
    }
}

/// The PURE output of [`throttle_decision`]: how the battery/thermal state should
/// (conservatively) nudge the LOCAL sub-tier this turn. Influences ONLY the local
/// sub-choice + a defer-heavy hint; it NEVER changes the resolved tier, loosens a
/// gate, or makes a cloud call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThrottlePlan {
    /// The preferred LOCAL sub-tier. `Auto` (the neutral default + the OFF state)
    /// leaves the existing AUTO-by-difficulty sub-choice untouched; `Fast` biases
    /// the on-device turn toward the cheaper warm model to save battery/heat.
    pub tier_pref: LocalSubTier,
    /// Whether to DEFER heavy/optional background work (e.g. multi-model warming,
    /// speculative decoding's extra draft pass) this turn. A hint the caller may
    /// honor; never a hard block.
    pub defer_heavy: bool,
    /// A stable, HONEST reason code for the plan, for `model.throttle` telemetry.
    pub reason: ThrottleReason,
}

/// Why [`throttle_decision`] produced its plan — a stable code for telemetry and
/// for asserting the policy table in tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThrottleReason {
    /// Adaptive throttling is OFF ([power].adaptive=false): neutral plan, the
    /// live reader is never consulted. This is the shipped default.
    Disabled,
    /// On AC + nominal/fair thermal: no throttle (the machine has power + headroom).
    Nominal,
    /// On battery below the low-battery threshold: prefer Fast + defer heavy.
    LowBattery,
    /// Serious/critical thermal pressure: prefer Fast + defer heavy (even on AC).
    ThermalPressure,
}

impl ThrottleReason {
    /// Stable identifier for `model.throttle` telemetry / the HUD indicator.
    pub fn as_str(&self) -> &'static str {
        match self {
            ThrottleReason::Disabled => "disabled",
            ThrottleReason::Nominal => "nominal",
            ThrottleReason::LowBattery => "low_battery",
            ThrottleReason::ThermalPressure => "thermal",
        }
    }
}

impl ThrottlePlan {
    /// The NEUTRAL plan: leave the AUTO sub-choice untouched, defer nothing. This
    /// is what the OFF default produces and what every "no throttle" branch
    /// returns, so the OFF state is byte-for-byte today's routing.
    pub fn neutral(reason: ThrottleReason) -> Self {
        ThrottlePlan {
            tier_pref: LocalSubTier::Auto,
            defer_heavy: false,
            reason,
        }
    }

    /// Whether this plan actually throttles (prefers Fast or defers heavy work).
    /// `false` for every neutral plan (incl. the OFF default).
    pub fn is_throttled(&self) -> bool {
        self.tier_pref == LocalSubTier::Fast || self.defer_heavy
    }
}

/// CONSERVATIVE battery/thermal throttle policy (#38). PURE — a function of the
/// synthetic reading + config, no I/O, no model, no power read. The LIVE reader
/// that supplies `(battery_pct, on_ac, thermal)` is device-gated behind
/// [power].adaptive in [`crate::power`]; this decision is unit-tested over the
/// whole synthetic input space.
///
/// Policy (deliberately conservative — it only ever PREFERS the cheaper local
/// path, never the reverse, and never touches a gate):
///   * `[power].adaptive` OFF  -> ALWAYS neutral (`ThrottleReason::Disabled`).
///     The reader is never consulted; routing is byte-for-byte today's.
///   * Serious/Critical THERMAL -> prefer Fast + defer heavy
///     (`ThermalPressure`), regardless of AC — heat is heat. Highest priority.
///   * ON BATTERY below `low_battery_pct` -> prefer Fast + defer heavy
///     (`LowBattery`).
///   * Otherwise (on AC, OR on battery above the threshold, with nominal/fair
///     thermal) -> neutral (`Nominal`): the machine has power/headroom, so leave
///     the AUTO sub-choice alone.
///
/// `battery_pct` is `None` when the live reader could not read a battery (e.g. a
/// desktop Mac, or a read failure) — treated as "no battery concern" (the
/// thermal branch still applies), NEVER fabricated as a low battery.
pub fn throttle_decision(
    battery_pct: Option<u8>,
    on_ac: bool,
    thermal: ThermalState,
    cfg: &Config,
) -> ThrottlePlan {
    // OFF default: never consult the reading; neutral plan == today's routing.
    if !cfg.power.adaptive {
        return ThrottlePlan::neutral(ThrottleReason::Disabled);
    }
    // Thermal pressure is the highest-priority trigger — heat throttles even on
    // AC (a hot machine on the charger still benefits from the lighter model).
    if thermal.is_pressured() {
        return ThrottlePlan {
            tier_pref: LocalSubTier::Fast,
            defer_heavy: true,
            reason: ThrottleReason::ThermalPressure,
        };
    }
    // Low battery while DISCHARGING -> prefer the cheaper warm model + defer
    // heavy work. On AC the battery never triggers (it is charging / topped).
    if !on_ac {
        if let Some(pct) = battery_pct {
            if pct < cfg.power.low_battery_pct {
                return ThrottlePlan {
                    tier_pref: LocalSubTier::Fast,
                    defer_heavy: true,
                    reason: ThrottleReason::LowBattery,
                };
            }
        }
    }
    // On AC, or on battery above the threshold, with nominal/fair thermal: the
    // machine has power + headroom — leave the AUTO sub-choice untouched.
    ThrottlePlan::neutral(ThrottleReason::Nominal)
}

/// Apply a [`ThrottlePlan`] to the AUTO sub-tier choice for a LOCAL turn. PURE.
/// A throttled plan that prefers `Fast` OVERRIDES the AUTO-by-difficulty default
/// to the explicit `Fast` sub-tier (so a low-battery / hot machine answers an
/// easy turn on the cheaper warm model); a neutral plan leaves it `Auto` so
/// `select_local_model` keeps today's difficulty-based behavior. It never picks a
/// MORE expensive tier than AUTO would, and a hard turn still resolves to the
/// capable base inside `select_local_model` (Fast collapses to base when no extra
/// is warm), so a throttle can never degrade a genuinely hard offline turn below
/// the single-resident base.
pub fn throttled_sub_tier(plan: &ThrottlePlan) -> LocalSubTier {
    plan.tier_pref
}

/// HONEST telemetry shape for the HUD resident-models indicator (mirrors
/// server.py's `InferenceEngine.local_warm_status`). Built from config alone —
/// PURE, no model, no load. It reports the PLANNED warm-set (what the policy
/// ALLOWS under the budget), the base/active id, whether multi-resident is in
/// effect, and the budget. It does NOT claim what is actually resident (only the
/// server knows that at runtime) and does NOT claim a measured speed benefit —
/// the swap benefit is device/RAM-gated. `multi_resident == false` is the safe
/// single-resident low-RAM default.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalWarmTelemetry {
    /// The always-resident base/primary local model ([models].llm).
    pub base: String,
    /// The ordered warm-set the policy admits under the budget (base first).
    pub planned: Vec<String>,
    /// True iff the policy admitted >1 model (an instant local swap is possible
    /// WHEN RAM allows). False => single-resident (the safe low-RAM default).
    pub multi_resident: bool,
    /// The configured RAM budget (GiB); 0 => single-resident.
    pub budget_gib: f64,
}

/// Build the HUD resident-models telemetry from config (the warm-set plan under
/// the budget). PURE; the single place the daemon assembles the indicator shape,
/// so it stays in lockstep with the server's actual policy. The base is
/// [models].llm; the extras + budget + sizes are the [models].local_* keys.
pub fn local_warm_telemetry(cfg: &Config) -> LocalWarmTelemetry {
    let base = cfg.models.llm.clone();
    let planned = plan_warm_set(
        &base,
        &cfg.models.local_warm,
        cfg.models.local_budget_gib,
        &cfg.models.local_sizes,
    );
    LocalWarmTelemetry {
        multi_resident: planned.len() > 1,
        budget_gib: cfg.models.local_budget_gib,
        base,
        planned,
    }
}

// ---------------------------------------------------------------------------
// VOICE INTENTS: classify_model_swap — CONSERVATIVE model-control detection
// ---------------------------------------------------------------------------

/// A detected model-control voice command. Maps to a tier override (or Auto =
/// clear). Detected BEFORE normal routing, like a command — never falls through
/// to a normal answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSwapIntent {
    /// Use the most capable cloud model (Opus). -> override Some(Heavy).
    Heavy,
    /// Use the fast/light cloud model (Haiku). -> override Some(Fast).
    Fast,
    /// Work offline / on-device / privately — NO cloud call. -> override Some(Local).
    Local,
    /// Back to automatic — clear the override, let DARWIN pick per turn.
    Auto,
}

impl ModelSwapIntent {
    /// The override this intent installs: a tier (`Some`) for Heavy/Fast/Local, or
    /// `None` for Auto (which CLEARS the override back to the config default).
    pub fn to_override(self) -> Option<Tier> {
        match self {
            ModelSwapIntent::Heavy => Some(Tier::Heavy),
            ModelSwapIntent::Fast => Some(Tier::Fast),
            ModelSwapIntent::Local => Some(Tier::Local),
            ModelSwapIntent::Auto => None,
        }
    }

    /// A short, HONEST spoken acknowledgment. Local is named as the on-device
    /// privacy path; Auto admits it is a heuristic pick; nothing claims local ==
    /// Opus quality.
    pub fn ack(&self) -> &'static str {
        match self {
            ModelSwapIntent::Heavy => "Switching to the powerful model.",
            ModelSwapIntent::Fast => "Fast mode.",
            ModelSwapIntent::Local => "Going offline - staying on device.",
            ModelSwapIntent::Auto => "Auto - I'll pick the best model per request.",
        }
    }

    /// Stable identifier for `model.swap` telemetry.
    pub fn as_str(&self) -> &'static str {
        match self {
            ModelSwapIntent::Heavy => "heavy",
            ModelSwapIntent::Fast => "fast",
            ModelSwapIntent::Local => "local",
            ModelSwapIntent::Auto => "auto",
        }
    }
}

/// CONSERVATIVELY detect a model-control command in `utterance`. Returns
/// `Some(intent)` ONLY for imperative, model-control phrasing — a normal sentence
/// that merely MENTIONS "fast" / "offline" / "powerful" must NOT trigger.
///
/// How false-triggers are avoided:
///   * Detection anchors on full MODEL-CONTROL PHRASES ("use the powerful model",
///     "go offline", "switch to opus", "speed mode"), not bare adjectives. A
///     sentence like "the offline backup ran fast" contains "offline" and "fast"
///     but none of the anchored control phrases, so it returns `None`.
///   * The model-naming family ("use the ... model", "switch to opus/haiku") is a
///     two-part match: a control lead-in AND a model word. "I read a fast book"
///     has neither lead-in nor model word.
///   * Each family's phrases are deliberately specific imperatives, so a passing
///     mention can't satisfy them.
///
/// Pure + deterministic — a function of the utterance only.
pub fn classify_model_swap(utterance: &str) -> Option<ModelSwapIntent> {
    let text = utterance.trim().to_lowercase();
    if text.is_empty() {
        return None;
    }

    // AUTO checked first: "back to normal / auto / let you decide / default mode".
    // (An auto request is unambiguous and should clear an override even if a stray
    // capability word appears.)
    const AUTO_PHRASES: &[&str] = &[
        "auto mode",
        "automatic mode",
        "go auto",
        "switch to auto",
        "set to auto",
        "back to auto",
        "back to normal",
        "default mode",
        "back to default",
        "let darwin decide",
        "let darwin pick",
        "let you decide",
        "you decide which model",
        "pick for me",
        "pick the model for me",
        "choose the model for me",
        "decide the model for me",
        "automatic model",
        "automatically pick",
        "automatically choose",
    ];
    if AUTO_PHRASES.iter().any(|p| text.contains(p)) {
        return Some(ModelSwapIntent::Auto);
    }

    // LOCAL / OFFLINE / PRIVATE — the on-device privacy path. Anchored on
    // imperative control phrasing, never a bare "offline" mention.
    const LOCAL_PHRASES: &[&str] = &[
        "work offline",
        "go offline",
        "offline mode",
        "switch to offline",
        "private mode",
        "go private",
        "stay on device",
        "stay on-device",
        "on device only",
        "on-device only",
        "keep it on device",
        "keep it on-device",
        "use the local model",
        "use local model",
        "switch to local",
        "use the on-device model",
        "off the grid",
        "local model only",
        "local only",
    ];
    if LOCAL_PHRASES.iter().any(|p| text.contains(p)) {
        return Some(ModelSwapIntent::Local);
    }

    // HEAVY — the most capable model. Anchored phrases + the model-naming family.
    const HEAVY_PHRASES: &[&str] = &[
        "use the powerful model",
        "use the most powerful model",
        "use the heavy model",
        "use the best model",
        "use the strongest model",
        "use the smartest model",
        "use the big model",
        "powerful model",
        "heavy model",
        "strongest model",
        "smartest model",
        "best model",
        "switch to opus",
        "use opus",
        "switch to the powerful model",
        "switch to the heavy model",
        "max power",
        "maximum power",
        "full power",
        "power mode",
        "heavy mode",
        "go heavy",
    ];
    if HEAVY_PHRASES.iter().any(|p| text.contains(p)) {
        return Some(ModelSwapIntent::Heavy);
    }

    // FAST — the quick/light model. Anchored phrases + the model-naming family.
    // NOTE: bare "fast" never triggers; only model-control phrasing does.
    const FAST_PHRASES: &[&str] = &[
        "use the fast model",
        "use the quick model",
        "use the light model",
        "use the lite model",
        "fast model",
        "quick model",
        "light model",
        "go fast",
        "speed mode",
        "fast mode",
        "switch to haiku",
        "use haiku",
        "switch to fast",
        "switch to the fast model",
        "lightweight model",
        "light mode model",
    ];
    if FAST_PHRASES.iter().any(|p| text.contains(p)) {
        return Some(ModelSwapIntent::Fast);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_route(route: &str) -> Config {
        let mut c = Config::default();
        c.router.conversation_route = route.to_string();
        c
    }

    // --- Tier basics --------------------------------------------------------

    #[test]
    fn tier_from_route_maps_known_and_unknown() {
        assert_eq!(Tier::from_route("cloud_heavy"), Tier::Heavy);
        assert_eq!(Tier::from_route("cloud_fast"), Tier::Fast);
        assert_eq!(Tier::from_route("local"), Tier::Local);
        // Unknown -> Local (safe).
        assert_eq!(Tier::from_route("wat"), Tier::Local);
        assert_eq!(Tier::from_route(""), Tier::Local);
    }

    #[test]
    fn local_tier_is_not_cloud_fast_and_heavy_are() {
        assert!(!Tier::Local.is_cloud());
        assert!(Tier::Fast.is_cloud());
        assert!(Tier::Heavy.is_cloud());
    }

    // --- tier_to_model: NO cloud string for Local ---------------------------

    #[test]
    fn tier_to_model_maps_to_config_strings_and_local_path() {
        let cfg = Config::default(); // heavy=opus, fast=haiku
        assert_eq!(
            tier_to_model(Tier::Heavy, &cfg),
            ModelChoice::Cloud("claude-opus-4-8".to_string())
        );
        assert_eq!(
            tier_to_model(Tier::Fast, &cfg),
            ModelChoice::Cloud("claude-haiku-4-5".to_string())
        );
        // Local NEVER yields a cloud model string — it is the on-device path.
        assert_eq!(tier_to_model(Tier::Local, &cfg), ModelChoice::Local);
    }

    // --- PRECEDENCE: Override beats Auto ------------------------------------

    #[test]
    fn explicit_override_beats_auto() {
        // Default route cloud_heavy; a trivial turn AUTO would pick Fast — but an
        // explicit Heavy override wins, with Reason::Override.
        let cfg = cfg_with_route("cloud_heavy");
        let (tier, reason) =
            resolve_tier(&cfg, Some(Tier::Heavy), "light", 0.95, 0.6, true);
        assert_eq!(tier, Tier::Heavy);
        assert_eq!(reason, Reason::Override);

        // A Fast override wins even on a HEAVY turn (auto would pick Heavy).
        let (tier, reason) = resolve_tier(&cfg, Some(Tier::Fast), "heavy", 0.95, 0.6, true);
        assert_eq!(tier, Tier::Fast);
        assert_eq!(reason, Reason::Override);
    }

    // --- AUTO maps complexity -> tier ---------------------------------------

    #[test]
    fn auto_maps_complexity_to_tier_from_cloud_heavy_default() {
        let cfg = cfg_with_route("cloud_heavy");
        // Heavy turn -> Heavy (auto).
        let (tier, reason) = resolve_tier(&cfg, None, "heavy", 0.95, 0.6, true);
        assert_eq!((tier, reason), (Tier::Heavy, Reason::Auto));
        // Trivial confident turn steps DOWN to Fast to save cost.
        let (tier, reason) = resolve_tier(&cfg, None, "light", 0.95, 0.6, true);
        assert_eq!((tier, reason), (Tier::Fast, Reason::Auto));
        // Low-confidence light turn is treated as hard -> Heavy.
        let (tier, reason) = resolve_tier(&cfg, None, "light", 0.3, 0.6, true);
        assert_eq!((tier, reason), (Tier::Heavy, Reason::Auto));
    }

    #[test]
    fn auto_from_cloud_fast_default_escalates_hard_turns() {
        let cfg = cfg_with_route("cloud_fast");
        // Trivial -> stays Fast.
        let (tier, reason) = resolve_tier(&cfg, None, "light", 0.95, 0.6, true);
        assert_eq!((tier, reason), (Tier::Fast, Reason::Auto));
        // Heavy -> escalates to Heavy even from a fast default.
        let (tier, reason) = resolve_tier(&cfg, None, "heavy", 0.95, 0.6, true);
        assert_eq!((tier, reason), (Tier::Heavy, Reason::Auto));
    }

    #[test]
    fn auto_from_local_default_never_goes_to_cloud() {
        let cfg = cfg_with_route("local");
        // Even a HEAVY turn stays Local under a local default (offline intent
        // preserved) — auto never silently reaches cloud.
        let (tier, reason) = resolve_tier(&cfg, None, "heavy", 0.95, 0.6, true);
        assert_eq!((tier, reason), (Tier::Local, Reason::Auto));
    }

    // --- cloud-unreachable / local-override -> Local, NO cloud --------------

    #[test]
    fn cloud_unreachable_auto_falls_back_to_local() {
        let cfg = cfg_with_route("cloud_heavy");
        // Heavy turn would be Heavy, but cloud is unreachable -> Local + Fallback.
        let (tier, reason) = resolve_tier(&cfg, None, "heavy", 0.95, 0.6, false);
        assert_eq!((tier, reason), (Tier::Local, Reason::Fallback));
        // And tier_to_model gives NO cloud string.
        assert_eq!(tier_to_model(tier, &cfg), ModelChoice::Local);
    }

    #[test]
    fn local_override_forces_local_even_when_cloud_reachable() {
        let cfg = cfg_with_route("cloud_heavy");
        // Offline override on a heavy turn with cloud UP: still Local (privacy
        // wins), Reason::Override, NO cloud model.
        let (tier, reason) = resolve_tier(&cfg, Some(Tier::Local), "heavy", 0.95, 0.6, true);
        assert_eq!((tier, reason), (Tier::Local, Reason::Override));
        assert_eq!(tier_to_model(tier, &cfg), ModelChoice::Local);
    }

    #[test]
    fn cloud_override_with_no_cloud_degrades_to_local() {
        let cfg = cfg_with_route("local");
        // The user asked for Heavy but there is no cloud -> honest Local fallback,
        // not a pretend cloud call.
        let (tier, reason) = resolve_tier(&cfg, Some(Tier::Heavy), "light", 0.95, 0.6, false);
        assert_eq!((tier, reason), (Tier::Local, Reason::Fallback));
        assert_eq!(tier_to_model(tier, &cfg), ModelChoice::Local);
    }

    // --- override set / clear / Auto clears ---------------------------------

    #[test]
    fn override_sets_and_clears_and_auto_clears() {
        let _guard = OverrideGuard::force(None);
        assert_eq!(current_override(), None);

        set_override(Some(Tier::Heavy));
        assert_eq!(current_override(), Some(Tier::Heavy));

        // The Auto intent maps to a None override -> clears.
        set_override(ModelSwapIntent::Auto.to_override());
        assert_eq!(current_override(), None);

        set_override(Some(Tier::Local));
        assert_eq!(current_override(), Some(Tier::Local));
        clear_override();
        assert_eq!(current_override(), None);
    }

    #[test]
    fn active_tier_is_override_else_route_default() {
        // No override -> the config default route decides.
        assert_eq!(active_tier(&cfg_with_route("cloud_heavy"), None), Tier::Heavy);
        assert_eq!(active_tier(&cfg_with_route("cloud_fast"), None), Tier::Fast);
        assert_eq!(active_tier(&cfg_with_route("local"), None), Tier::Local);
        // An override WINS over the route — including "work offline" forcing Local
        // even when the default route is cloud (the signal the voice tier reads to
        // keep speech on-device).
        let cfg = cfg_with_route("cloud_heavy");
        assert_eq!(active_tier(&cfg, Some(Tier::Local)), Tier::Local);
        assert_eq!(active_tier(&cfg, Some(Tier::Fast)), Tier::Fast);
    }

    #[test]
    fn intent_to_override_mapping() {
        assert_eq!(ModelSwapIntent::Heavy.to_override(), Some(Tier::Heavy));
        assert_eq!(ModelSwapIntent::Fast.to_override(), Some(Tier::Fast));
        assert_eq!(ModelSwapIntent::Local.to_override(), Some(Tier::Local));
        assert_eq!(ModelSwapIntent::Auto.to_override(), None);
    }

    // --- offline forces Local (no cloud path) via the full set->resolve chain -

    #[test]
    fn offline_intent_forces_local_no_cloud_call() {
        let _guard = OverrideGuard::force(None);
        let cfg = cfg_with_route("cloud_heavy");
        // Speak "go offline" -> Local override installed.
        let intent = classify_model_swap("go offline").unwrap();
        assert_eq!(intent, ModelSwapIntent::Local);
        set_override(intent.to_override());
        // Now even a heavy, confident turn with cloud reachable resolves Local.
        let (tier, reason) =
            resolve_tier(&cfg, current_override(), "heavy", 0.95, 0.6, true);
        assert_eq!((tier, reason), (Tier::Local, Reason::Override));
        assert_eq!(tier_to_model(tier, &cfg), ModelChoice::Local);
    }

    // --- classify_model_swap: each intent detected -------------------------

    #[test]
    fn detects_heavy_intents() {
        for u in [
            "use the powerful model",
            "use the best model please",
            "switch to opus",
            "max power",
            "go heavy",
            "use the strongest model",
        ] {
            assert_eq!(
                classify_model_swap(u),
                Some(ModelSwapIntent::Heavy),
                "should detect Heavy in {u:?}"
            );
        }
    }

    #[test]
    fn detects_fast_intents() {
        for u in [
            "use the fast model",
            "go fast",
            "speed mode",
            "use haiku",
            "switch to the fast model",
            "use the light model",
        ] {
            assert_eq!(
                classify_model_swap(u),
                Some(ModelSwapIntent::Fast),
                "should detect Fast in {u:?}"
            );
        }
    }

    #[test]
    fn detects_local_intents() {
        for u in [
            "work offline",
            "go offline",
            "offline mode",
            "private mode",
            "stay on device",
            "use the local model",
            "off the grid",
        ] {
            assert_eq!(
                classify_model_swap(u),
                Some(ModelSwapIntent::Local),
                "should detect Local in {u:?}"
            );
        }
    }

    #[test]
    fn detects_auto_intents() {
        for u in [
            "auto mode",
            "automatic mode",
            "let darwin decide",
            "back to normal",
            "default mode",
            "pick for me",
        ] {
            assert_eq!(
                classify_model_swap(u),
                Some(ModelSwapIntent::Auto),
                "should detect Auto in {u:?}"
            );
        }
    }

    // --- classify_model_swap: CONSERVATIVE — no false triggers --------------

    #[test]
    fn does_not_false_trigger_on_normal_sentences() {
        // Each of these merely MENTIONS a capability word but is not a model
        // command. None may trigger.
        for u in [
            "the offline backup finished",                 // "offline"
            "my internet is fast today",                   // "fast"
            "that was a powerful speech",                  // "powerful"
            "she ran fast to catch the bus",               // "fast"
            "what's the weather like, darwin",             // plain chat
            "i need a quick answer about taxes",           // "quick" but not a model cmd
            "the local cafe is on device street",          // "local" + "device"
            "tell me something best for breakfast",        // "best" but no "model"
            "opus is a great album name",                  // "opus" without a control lead-in
            "let me decide what to eat",                   // "let me decide" != model decide
            "private thoughts are private",                // "private" without "mode"
            "we should automatically save the file",       // "automatically" but not model pick
        ] {
            assert_eq!(
                classify_model_swap(u),
                None,
                "must NOT trigger on normal sentence {u:?}"
            );
        }
    }

    // --- INTEGRATION SEAM: the HUD Settings buttons send phrases the classifier
    //     MUST recognize. These four literals are the exact `swap(..)` phrases in
    //     hud/src/components/SettingsModal.tsx; if either side drifts (e.g. the AUTO
    //     button regresses to "auto, you pick the model", which classifies as None
    //     and would leak to the normal answer path instead of clearing the
    //     override), this test fails. Locks the round-trip neither suite covered.
    #[test]
    fn settings_button_phrases_round_trip_to_their_intent() {
        // (button label, exact phrase the SettingsModal click handler sends, intent)
        let cases: &[(&str, &str, ModelSwapIntent)] = &[
            ("HEAVY", "use the most powerful model", ModelSwapIntent::Heavy),
            ("FAST", "use the fast model", ModelSwapIntent::Fast),
            ("LOCAL", "work offline, stay on device", ModelSwapIntent::Local),
            // AUTO must classify (clearing the override), NOT fall through to a
            // normal answer. "auto, you pick the model" was the broken phrase.
            ("AUTO", "auto mode", ModelSwapIntent::Auto),
        ];
        for (button, phrase, want) in cases {
            assert_eq!(
                classify_model_swap(phrase),
                Some(*want),
                "Settings {button} button phrase {phrase:?} must classify as {want:?} \
                 (a None here means the click leaks to the normal answer path and the \
                 override is never set/cleared)"
            );
        }
        // The AUTO intent specifically must CLEAR the override (None), not pin a tier.
        assert_eq!(
            classify_model_swap("auto mode").unwrap().to_override(),
            None,
            "the AUTO button must clear the override back to the config default"
        );
    }

    #[test]
    fn ack_strings_are_honest_and_present() {
        // Local ack names the on-device/privacy benefit; Auto admits it's a pick.
        assert!(ModelSwapIntent::Local.ack().to_lowercase().contains("device"));
        assert!(ModelSwapIntent::Auto.ack().to_lowercase().contains("auto"));
        assert!(!ModelSwapIntent::Heavy.ack().is_empty());
        assert!(!ModelSwapIntent::Fast.ack().is_empty());
        // The honest_label never claims local == cloud quality.
        assert!(Tier::Local.honest_label().contains("device"));
        assert!(Tier::Local.honest_label().contains("limited"));
    }

    // =======================================================================
    // MULTI-RESIDENT LOCAL SUB-TIER (task #17) — PURE policy, synthetic sizes,
    // NO real model / NO MLX / NO load. Every test below is hermetic.
    // =======================================================================

    use std::collections::BTreeMap;

    fn sizes(pairs: &[(&str, f64)]) -> BTreeMap<String, f64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn ids(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| s.to_string()).collect()
    }

    // --- estimate_local_model_gib: override > heuristic > default -----------

    #[test]
    fn estimate_uses_explicit_size_override_first() {
        let s = sizes(&[("foo/bar-7b-4bit", 1.25)]);
        // The override wins over the heuristic (which would say ~4.2 for a 7b-4bit).
        assert_eq!(estimate_local_model_gib("foo/bar-7b-4bit", &s), 1.25);
    }

    #[test]
    fn estimate_heuristic_by_param_count_and_quant() {
        let empty = BTreeMap::new();
        // 4B-4bit -> 4.0 * 0.6 = 2.4
        assert_eq!(estimate_local_model_gib("qwen3-4b-4bit", &empty), 2.4);
        // 0.6B-4bit -> 0.6 * 0.6 = 0.36
        assert_eq!(estimate_local_model_gib("qwen3-0.6b-4bit", &empty), 0.36);
        // 8b-8bit -> 8.0 * 1.15 = 9.2
        assert_eq!(estimate_local_model_gib("model-8b-8bit", &empty), 9.2);
        // 7b with no quant token -> bf16 factor 2.2 -> 15.4
        assert_eq!(estimate_local_model_gib("plain-7b", &empty), 15.4);
    }

    #[test]
    fn estimate_unknown_id_uses_costly_default() {
        let empty = BTreeMap::new();
        // No "<n>b" token at all -> the deliberately-generous default.
        assert_eq!(
            estimate_local_model_gib("some/whisper-model", &empty),
            DEFAULT_LOCAL_MODEL_GIB
        );
        // A non-positive override is ignored (falls through to the heuristic).
        let bad = sizes(&[("plain-7b", 0.0)]);
        assert_eq!(estimate_local_model_gib("plain-7b", &bad), 15.4);
    }

    // --- plan_warm_set: single-resident default, admit, skip, fallback ------

    #[test]
    fn plan_single_resident_by_default_zero_budget() {
        // CONSERVATIVE default: a 0 budget => base-only, regardless of configured
        // extras. This is today's behavior and the safe low-RAM state.
        let s = sizes(&[("base", 2.4), ("fast", 0.4)]);
        let plan = plan_warm_set("base", &ids(&["fast"]), 0.0, &s);
        assert_eq!(plan, ids(&["base"]));
    }

    #[test]
    fn plan_admits_extras_within_budget_in_order() {
        // base 2.4 + fast 0.4 + tiny 0.3 = 3.1 <= 4.0 -> all three warm, base first.
        let s = sizes(&[("base", 2.4), ("fast", 0.4), ("tiny", 0.3)]);
        let plan = plan_warm_set("base", &ids(&["fast", "tiny"]), 4.0, &s);
        assert_eq!(plan, ids(&["base", "fast", "tiny"]));
    }

    #[test]
    fn plan_skips_an_extra_that_would_exceed_budget_but_keeps_scanning() {
        // base 2.4 + big 2.0 = 4.4 > 4.0 -> big SKIPPED; tiny 0.3 still fits
        // (2.4 + 0.3 = 2.7) -> admitted. Smaller-later models are not lost.
        let s = sizes(&[("base", 2.4), ("big", 2.0), ("tiny", 0.3)]);
        let plan = plan_warm_set("base", &ids(&["big", "tiny"]), 4.0, &s);
        assert_eq!(plan, ids(&["base", "tiny"]));
    }

    #[test]
    fn plan_falls_back_to_single_resident_when_base_alone_exceeds_budget() {
        // base 6.0 > budget 4.0 -> SINGLE-RESIDENT (base must stay warm; no extras).
        let s = sizes(&[("base", 6.0), ("fast", 0.4)]);
        let plan = plan_warm_set("base", &ids(&["fast"]), 4.0, &s);
        assert_eq!(plan, ids(&["base"]));
    }

    #[test]
    fn plan_dedups_and_ignores_empty_and_base_repeats() {
        let s = sizes(&[("base", 2.0), ("fast", 0.4)]);
        let plan = plan_warm_set("base", &ids(&["", "base", "fast", "fast"]), 4.0, &s);
        assert_eq!(plan, ids(&["base", "fast"]));
    }

    // --- select_local_model: single collapse + AUTO by difficulty ----------

    #[test]
    fn select_single_resident_always_returns_base() {
        // A single-resident plan ignores the sub-choice entirely — base answers
        // every local turn (today's behavior, the low-RAM-safe path).
        let plan = ids(&["base"]);
        for sub in [LocalSubTier::Fast, LocalSubTier::Capable, LocalSubTier::Auto] {
            assert_eq!(select_local_model(&plan, sub, "heavy", 0.9, 0.6), "base");
            assert_eq!(select_local_model(&plan, sub, "light", 0.9, 0.6), "base");
        }
    }

    #[test]
    fn select_explicit_fast_and_capable() {
        let plan = ids(&["base", "fast"]);
        // Explicit Fast -> the non-base warm model; Capable -> the base.
        assert_eq!(
            select_local_model(&plan, LocalSubTier::Fast, "heavy", 0.9, 0.6),
            "fast"
        );
        assert_eq!(
            select_local_model(&plan, LocalSubTier::Capable, "light", 0.9, 0.6),
            "base"
        );
    }

    #[test]
    fn select_auto_keeps_capable_base_on_hard_turns() {
        let plan = ids(&["base", "fast"]);
        // A trivial, confident turn takes the faster warm model.
        assert_eq!(
            select_local_model(&plan, LocalSubTier::Auto, "light", 0.9, 0.6),
            "fast"
        );
        // A heavy turn keeps the capable base (no silent downgrade).
        assert_eq!(
            select_local_model(&plan, LocalSubTier::Auto, "heavy", 0.9, 0.6),
            "base"
        );
        // A low-confidence (below threshold) light turn is treated as hard -> base.
        assert_eq!(
            select_local_model(&plan, LocalSubTier::Auto, "light", 0.3, 0.6),
            "base"
        );
    }

    // --- config default is CONSERVATIVE single-resident (pinned) -----------

    #[test]
    fn config_default_is_single_resident_pinned() {
        // The shipped default: empty warm-set + 0 budget == single-resident. This
        // PINS the conservative default so a low-RAM Mac is never silently flipped
        // to multi-resident.
        let cfg = Config::default();
        assert!(cfg.models.local_warm.is_empty());
        assert_eq!(cfg.models.local_budget_gib, 0.0);
        assert!(cfg.models.local_sizes.is_empty());
        let tel = local_warm_telemetry(&cfg);
        assert_eq!(tel.base, cfg.models.llm);
        assert_eq!(tel.planned, ids(&[cfg.models.llm.as_str()]));
        assert!(!tel.multi_resident, "default MUST be single-resident");
        assert_eq!(tel.budget_gib, 0.0);
    }

    #[test]
    fn telemetry_reports_multi_resident_only_when_budget_admits() {
        // Configure a fast extra + a budget that admits it -> multi-resident.
        let mut cfg = Config::default();
        cfg.models.llm = "base-4b-4bit".to_string(); // estimate 2.4
        cfg.models.local_warm = ids(&["fast-0.6b-4bit"]); // estimate 0.36
        cfg.models.local_budget_gib = 3.0; // 2.4 + 0.36 = 2.76 <= 3.0
        let tel = local_warm_telemetry(&cfg);
        assert_eq!(tel.planned, ids(&["base-4b-4bit", "fast-0.6b-4bit"]));
        assert!(tel.multi_resident);
        assert_eq!(tel.budget_gib, 3.0);

        // Same config but a budget too small for the extra -> single-resident.
        cfg.models.local_budget_gib = 2.5; // 2.4 + 0.36 = 2.76 > 2.5
        let tel = local_warm_telemetry(&cfg);
        assert_eq!(tel.planned, ids(&["base-4b-4bit"]));
        assert!(!tel.multi_resident);
    }

    #[test]
    fn low_ram_budget_forces_single_resident_even_with_warm_set() {
        // A low-RAM Mac: a tiny budget the base alone exceeds -> single-resident,
        // the configured warm-set notwithstanding. The honest low-RAM fallback.
        let mut cfg = Config::default();
        cfg.models.llm = "base-4b-4bit".to_string(); // ~2.4 GiB estimate
        cfg.models.local_warm = ids(&["fast-0.6b-4bit"]);
        cfg.models.local_budget_gib = 1.0; // < base estimate
        let tel = local_warm_telemetry(&cfg);
        assert_eq!(tel.planned, ids(&["base-4b-4bit"]));
        assert!(!tel.multi_resident);
    }

    #[test]
    fn local_sub_tier_labels_are_stable() {
        assert_eq!(LocalSubTier::Fast.as_str(), "fast");
        assert_eq!(LocalSubTier::Capable.as_str(), "capable");
        assert_eq!(LocalSubTier::Auto.as_str(), "auto");
    }

    // --- #38 BATTERY/THERMAL THROTTLE policy (PURE, synthetic inputs) -------

    /// A config with [power].adaptive set to `on`, everything else default
    /// (low_battery_pct = 20). Helper for the policy table.
    fn power_cfg(on: bool) -> Config {
        let mut cfg = Config::default();
        cfg.power.adaptive = on;
        cfg
    }

    /// EXPLICITLY OFF ([power].adaptive=false): the policy is ALWAYS neutral
    /// regardless of the reading — nothing reads power, routing is today's. This
    /// PINS the off-path contract: even a 1%-battery, critical-thermal reading
    /// produces a neutral plan when the flag is off. (The shipped DEFAULT is now ON,
    /// full-power; this proves the off path still exists when an operator disables it.)
    #[test]
    fn throttle_when_disabled_never_throttles() {
        let cfg = power_cfg(false);
        assert!(!cfg.power.adaptive, "explicitly-disabled [power].adaptive");
        // The worst possible reading still yields a neutral plan when OFF.
        let plan = throttle_decision(Some(1), false, ThermalState::Critical, &cfg);
        assert_eq!(plan.reason, ThrottleReason::Disabled);
        assert_eq!(plan.tier_pref, LocalSubTier::Auto);
        assert!(!plan.defer_heavy);
        assert!(!plan.is_throttled(), "disabled adaptive must never throttle");
    }

    /// The shipped DEFAULT is ON (full-power) — a low-battery discharging reading
    /// under the default config throttles (proves the default actually engages the
    /// perf-only policy; it never loosens a gate or makes a cloud call).
    #[test]
    fn throttle_default_is_on_and_engages_on_low_battery() {
        let cfg = Config::default();
        assert!(cfg.power.adaptive, "[power].adaptive ships ON (full-power default)");
        let plan = throttle_decision(Some(5), false, ThermalState::Nominal, &cfg);
        assert!(plan.is_throttled(), "low battery while discharging must throttle under the ON default");
        assert_eq!(plan.tier_pref, LocalSubTier::Fast);
    }

    /// ON + on AC + nominal thermal + healthy battery -> no throttle (the machine
    /// has power + headroom; leave the AUTO sub-choice alone).
    #[test]
    fn throttle_on_ac_nominal_is_neutral() {
        let cfg = power_cfg(true);
        let plan = throttle_decision(Some(90), true, ThermalState::Nominal, &cfg);
        assert_eq!(plan.reason, ThrottleReason::Nominal);
        assert_eq!(plan.tier_pref, LocalSubTier::Auto);
        assert!(!plan.is_throttled());
        // Fair thermal on AC is still not a trigger on its own.
        let plan = throttle_decision(Some(90), true, ThermalState::Fair, &cfg);
        assert_eq!(plan.reason, ThrottleReason::Nominal);
        assert!(!plan.is_throttled());
    }

    /// ON + on battery below the threshold + nominal thermal -> prefer Fast +
    /// defer heavy (LowBattery). A healthy battery on battery is still neutral.
    #[test]
    fn throttle_low_battery_on_battery_prefers_fast() {
        let cfg = power_cfg(true); // threshold 20
        // 10% discharging -> throttle.
        let plan = throttle_decision(Some(10), false, ThermalState::Nominal, &cfg);
        assert_eq!(plan.reason, ThrottleReason::LowBattery);
        assert_eq!(plan.tier_pref, LocalSubTier::Fast);
        assert!(plan.defer_heavy);
        assert!(plan.is_throttled());
        // Exactly at the threshold is NOT below it -> neutral (boundary check).
        let plan = throttle_decision(Some(20), false, ThermalState::Nominal, &cfg);
        assert_eq!(plan.reason, ThrottleReason::Nominal);
        assert!(!plan.is_throttled());
        // 50% on battery -> neutral (plenty of charge).
        let plan = throttle_decision(Some(50), false, ThermalState::Nominal, &cfg);
        assert_eq!(plan.reason, ThrottleReason::Nominal);
        assert!(!plan.is_throttled());
    }

    /// ON + low battery but ON AC -> no throttle (it is charging / topped). The
    /// battery branch only fires while DISCHARGING.
    #[test]
    fn throttle_low_battery_on_ac_does_not_throttle() {
        let cfg = power_cfg(true);
        let plan = throttle_decision(Some(5), true, ThermalState::Nominal, &cfg);
        assert_eq!(plan.reason, ThrottleReason::Nominal);
        assert!(!plan.is_throttled(), "on AC the battery never triggers");
    }

    /// ON + serious/critical thermal -> prefer Fast + defer heavy, EVEN on AC with
    /// a full battery (heat is heat; highest-priority trigger).
    #[test]
    fn throttle_thermal_pressure_triggers_even_on_ac() {
        let cfg = power_cfg(true);
        for thermal in [ThermalState::Serious, ThermalState::Critical] {
            let plan = throttle_decision(Some(100), true, thermal, &cfg);
            assert_eq!(plan.reason, ThrottleReason::ThermalPressure, "{thermal:?}");
            assert_eq!(plan.tier_pref, LocalSubTier::Fast);
            assert!(plan.defer_heavy);
            assert!(plan.is_throttled());
        }
    }

    /// ON + no battery readable (None, e.g. a desktop Mac or a read failure) ->
    /// the battery branch is skipped (NEVER fabricated as low); only thermal can
    /// throttle. Honest: a missing battery is not a low battery.
    #[test]
    fn throttle_none_battery_is_not_treated_as_low() {
        let cfg = power_cfg(true);
        // No battery + discharging-claimed + nominal -> neutral (no fake low).
        let plan = throttle_decision(None, false, ThermalState::Nominal, &cfg);
        assert_eq!(plan.reason, ThrottleReason::Nominal);
        assert!(!plan.is_throttled());
        // But thermal pressure still throttles regardless of the battery read.
        let plan = throttle_decision(None, false, ThermalState::Critical, &cfg);
        assert_eq!(plan.reason, ThrottleReason::ThermalPressure);
        assert!(plan.is_throttled());
    }

    /// `throttled_sub_tier` maps the plan to the sub-tier the router applies, and
    /// a throttled Fast plan biasing select_local_model picks the faster warm
    /// model on an EASY turn but a HARD turn still collapses to the capable base
    /// (a throttle never degrades a genuinely hard offline turn).
    #[test]
    fn throttled_sub_tier_biases_easy_turns_only() {
        let cfg = power_cfg(true);
        let throttled = throttle_decision(Some(5), false, ThermalState::Nominal, &cfg);
        assert_eq!(throttled_sub_tier(&throttled), LocalSubTier::Fast);
        let plan = ids(&["base", "fast"]);
        // Throttled + easy turn -> the faster warm model.
        assert_eq!(
            select_local_model(&plan, throttled_sub_tier(&throttled), "light", 0.9, 0.6),
            "fast"
        );
        // Even throttled, an explicit Fast sub-tier with no warm extra collapses to
        // the base (single-resident) — never a crash, never below the base.
        let single = ids(&["base"]);
        assert_eq!(
            select_local_model(&single, throttled_sub_tier(&throttled), "light", 0.9, 0.6),
            "base"
        );
        // A neutral plan leaves AUTO -> a hard turn keeps the capable base.
        let neutral = ThrottlePlan::neutral(ThrottleReason::Nominal);
        assert_eq!(
            select_local_model(&plan, throttled_sub_tier(&neutral), "heavy", 0.9, 0.6),
            "base"
        );
    }

    /// The throttle reason + thermal labels are stable identifiers (telemetry).
    #[test]
    fn throttle_labels_are_stable() {
        assert_eq!(ThrottleReason::Disabled.as_str(), "disabled");
        assert_eq!(ThrottleReason::Nominal.as_str(), "nominal");
        assert_eq!(ThrottleReason::LowBattery.as_str(), "low_battery");
        assert_eq!(ThrottleReason::ThermalPressure.as_str(), "thermal");
        assert_eq!(ThermalState::Nominal.as_str(), "nominal");
        assert_eq!(ThermalState::Fair.as_str(), "fair");
        assert_eq!(ThermalState::Serious.as_str(), "serious");
        assert_eq!(ThermalState::Critical.as_str(), "critical");
    }
}

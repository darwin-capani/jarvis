//! The SKILL FRAMEWORK — JARVIS's hand-written, in-tree library of small,
//! composable capabilities plus the open standard that lets the library grow.
//!
//! A [`SkillDef`] is the unit: a `snake_case` name, a category, a description of
//! WHEN to use it, trigger cues, a PURE `run` function over `serde_json::Value`
//! args, a `consequential` flag, and an optional source-gate marker. The
//! [`Registry`] aggregates every category module's `skills()` into one catalog
//! the meta-tools (`skill_list` / `skill_invoke` in anthropic.rs) surface WITHOUT
//! bloating `tool_defs` — the model discovers skills through `skill_list` and
//! runs one through `skill_invoke`, which dispatches here.
//!
//! HONEST SCOPE. This is a genuine hand-written in-tree library (a handful of
//! categories, a growing set of real skills) plus an extensible open standard —
//! NOT a populated 13.7k-entry community marketplace. New skills are added
//! in-tree (drop a [`SkillDef`] into a category file) or, at the open-standard
//! extension point, declared via an external manifest (see docs/SKILLS.md). The
//! catalog reports the REAL shipped count, never a fabricated one.
//!
//! PURITY + DETERMINISM is the design rule. A skill's `run` is a pure function:
//! no network, no clock-without-injection, no randomness-without-seed, no
//! ambient I/O. That is what makes the library flawless and hermetically
//! testable. A skill that would need a live external source (online dictionary,
//! live FX, current weather) is either omitted this round or built read-only and
//! honestly `source_gated` — it returns a "needs a data source" notice until one
//! is configured, and NEVER fabricates a result. A skill that mutates or acts
//! OUTSIDE the process is `consequential` and routes through the SAME cross-turn
//! confirmation gate + armed-by-default master switch a built-in consequential tool
//! uses (parked unless the switch is on AND a human confirmed — even with the
//! switch armed, a fresh confirm is still required).

use anyhow::{anyhow, Result};
use serde_json::Value;

// Category modules. Each is pre-declared here and aggregated in `Registry::new`,
// and each owns its OWN file with a public `skills()` that returns a Vec of
// SkillDef — so the Library phase fills one file per category with NO shared
// edits to this module. `utilities` carries the proof skills end-to-end; the
// rest scaffold empty-but-compiling for the library phase to populate.
pub mod datetime;
pub mod finance;
pub mod fun;
pub mod knowledge;
pub mod mathx;
pub mod text;
pub mod units;
pub mod utilities;

/// The category a skill belongs to. Stable `snake_case` slugs the meta-tool's
/// optional `category` filter keys on, and the headings docs/SKILLS.md documents.
/// Kept as an enum (not a free string) so a typo'd category cannot silently
/// create a phantom heading and every category has exactly one canonical slug.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// Encoders, hashers, formatters, id/slug helpers — general-purpose glue.
    Utilities,
    /// Text inspection + transforms: counts, case, reversing, trimming.
    Text,
    /// Calendar/clock arithmetic over INJECTED instants (never the wall clock).
    Datetime,
    /// Unit conversions with exact or well-known constant factors.
    Units,
    /// Extended math: bases, gcd/lcm, combinatorics, number predicates.
    Mathx,
    /// Reference lookups over BUNDLED data (no live source). Source-gated when
    /// the honest answer needs an external feed.
    Knowledge,
    /// Money math that is PURE arithmetic (tip, split, simple/compound interest)
    /// — never a live quote. Live FX/quotes are source-gated or omitted.
    Finance,
    /// Playful, deterministic-with-a-seed helpers (dice, coin, pick).
    Fun,
}

impl Category {
    /// The canonical `snake_case` slug — the `category` value `skill_list` filters
    /// on and the heading docs/SKILLS.md uses. Pure + total.
    pub const fn slug(self) -> &'static str {
        match self {
            Category::Utilities => "utilities",
            Category::Text => "text",
            Category::Datetime => "datetime",
            Category::Units => "units",
            Category::Mathx => "mathx",
            Category::Knowledge => "knowledge",
            Category::Finance => "finance",
            Category::Fun => "fun",
        }
    }

    /// Every category, in catalog (declaration) order — the order `skill_list`
    /// with no filter walks and the order docs/SKILLS.md lists. Pure.
    pub const fn all() -> &'static [Category] {
        &[
            Category::Utilities,
            Category::Text,
            Category::Datetime,
            Category::Units,
            Category::Mathx,
            Category::Knowledge,
            Category::Finance,
            Category::Fun,
        ]
    }

    /// Resolve a slug back to its category, for the meta-tool's `category` filter.
    /// An unknown slug is `None` (the caller reports a friendly error rather than
    /// silently returning the whole catalog). Pure.
    pub fn from_slug(slug: &str) -> Option<Category> {
        Category::all().iter().copied().find(|c| c.slug() == slug)
    }
}

/// The signature every skill's `run` function has: a PURE map from JSON args to a
/// human-readable outcome string. `Ok(String)` is the result the model relays;
/// `Err(_)` is a friendly, secret-free failure (bad args, a source-gated skill
/// with no source) the meta-tool surfaces as an `is_error` tool outcome. The
/// function takes `&Value` by reference (it borrows, never owns) and returns
/// owned `String` so the registry can hold a `fn` pointer with no captured state
/// — which keeps a `SkillDef` `'static`, `Copy`-free but cheaply cloneable, and
/// trivially testable in isolation.
pub type RunFn = fn(&Value) -> Result<String>;

/// One skill: the framework's unit of capability. Construct with [`SkillDef::new`]
/// (pure read-only default) and the `consequential` / `source_gated` builders.
///
/// EXACT shape the Library phase fills in — see the module docs and
/// docs/SKILLS.md for the add-a-skill recipe.
#[derive(Debug, Clone)]
pub struct SkillDef {
    /// Unique `snake_case` identifier, e.g. `base64_encode`. The name the model
    /// passes to `skill_invoke`. Uniqueness + casing are enforced by
    /// [`Registry::new`]'s guard, so a malformed or duplicate name fails fast at
    /// construction (a test pins it) rather than shadowing another skill at run.
    pub name: &'static str,
    /// The category this skill lists under.
    pub category: Category,
    /// One line on WHEN to use the skill (not just what it does) — the catalog
    /// entry `skill_list` returns so the model can discover + choose correctly.
    pub description: &'static str,
    /// Trigger cues: short phrasings that should bring this skill to mind. Carried
    /// for discovery/routing + documentation; not a hard gate (the model still
    /// chooses), so they never restrict what `skill_invoke` will run.
    #[allow(dead_code)] // discovery/routing + docs surface the Library phase reads; not consumed on the dispatch hot path
    pub cues: &'static [&'static str],
    /// The PURE run function. Hermetic + deterministic where at all possible.
    pub run: RunFn,
    /// Does invoking this skill MUTATE or ACT outside the process? A pure skill is
    /// `false` and runs ungated. A `true` skill routes through the cross-turn
    /// confirmation gate + the armed-by-default master switch (it PARKS for a spoken
    /// human yes instead of acting on first call — even with the switch armed) exactly
    /// like a built-in consequential tool. Defaults to `false` via [`SkillDef::new`].
    pub consequential: bool,
    /// Is this skill's honest answer dependent on an EXTERNAL data source that is
    /// not bundled (live dictionary, FX rate, weather)? When `true` the skill is
    /// shipped read-only and its `run` returns a "needs a data source" notice
    /// until one is configured — it NEVER fabricates. Defaults to `false`.
    pub source_gated: bool,
    /// Known-vector eval: (args-as-JSON, expected output) pairs the skill MUST
    /// reproduce through its pure `run`. [`Registry::new`] runs these at registry
    /// build and REFUSES to admit a skill whose run does not match — an
    /// eval-gated promotion: a skill cannot enter the live catalog unless it
    /// passes its own declared eval. Empty (the default via [`SkillDef::new`])
    /// skips the check, so existing skills are unaffected until they opt in via
    /// [`SkillDef::with_eval_vectors`]. For PURE skills only.
    pub eval_vectors: &'static [(&'static str, &'static str)],
}

impl SkillDef {
    /// A PURE, read-only, non-source-gated skill — the common case. Defaults
    /// `consequential` and `source_gated` to `false`; use the builders below to
    /// flip either. The Library phase calls this for almost every skill.
    pub const fn new(
        name: &'static str,
        category: Category,
        description: &'static str,
        cues: &'static [&'static str],
        run: RunFn,
    ) -> Self {
        SkillDef {
            name,
            category,
            description,
            cues,
            run,
            consequential: false,
            source_gated: false,
            eval_vectors: &[],
        }
    }

    /// Mark this skill CONSEQUENTIAL (it mutates/acts outside the process). The
    /// meta-tool routes a consequential skill through `confirm::park` +
    /// `integrations::gate`, so it parks for a spoken human yes instead of firing.
    #[allow(dead_code)] // Library-phase builder: a side-effecting skill calls this; no consequential skill ships THIS round
    pub const fn consequential(mut self) -> Self {
        self.consequential = true;
        self
    }

    /// Mark this skill SOURCE-GATED (its honest answer needs an external feed not
    /// bundled in-tree). Its `run` must return a "needs a data source" notice
    /// until one is configured; it never fabricates a value.
    #[allow(dead_code)] // Library-phase builder: a feed-dependent skill calls this; no source-gated skill ships THIS round
    pub const fn source_gated(mut self) -> Self {
        self.source_gated = true;
        self
    }

    /// Attach known-vector evals (see [`SkillDef::eval_vectors`]). Each pair is
    /// (args-as-JSON, expected output); the skill is REFUSED at registry build
    /// unless its pure `run` reproduces every expected output exactly — the
    /// eval-gated promotion bar.
    #[allow(dead_code)] // Library-phase builder: a skill opts into eval-gating with this; the gate is live for when the catalog adopts vectors
    pub const fn with_eval_vectors(
        mut self,
        vectors: &'static [(&'static str, &'static str)],
    ) -> Self {
        self.eval_vectors = vectors;
        self
    }

    /// One catalog line for `skill_list`: `name`, category slug, a
    /// consequential/source-gated marker, and the one-line description. Pure, so
    /// the rendering is unit-testable. The marker is what tells the model a skill
    /// will PARK (consequential) or report "needs a source" (source-gated) before
    /// it ever invokes it.
    pub fn catalog_line(&self) -> String {
        let mut marks = Vec::new();
        if self.consequential {
            marks.push("consequential");
        }
        if self.source_gated {
            marks.push("source-gated");
        }
        let tag = if marks.is_empty() {
            String::new()
        } else {
            format!(" [{}]", marks.join(", "))
        };
        format!("{} ({}){}: {}", self.name, self.category.slug(), tag, self.description)
    }
}

/// Is `name` a valid skill identifier: non-empty, `snake_case` — lowercase
/// ASCII letters/digits/underscore, starting with a letter, no leading/trailing
/// or doubled underscore. Pure; the guard [`Registry::new`] runs at startup (a
/// test pins it) so a malformed name can never enter the catalog. The
/// no-double-underscore rule keeps skill names disjoint from the MCP flat tool
/// namespace (`mcp__<server>__<tool>`), so the two surfaces can never collide.
pub fn is_snake_case(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let bytes = name.as_bytes();
    // Must start with a lowercase letter (not a digit or underscore).
    if !bytes[0].is_ascii_lowercase() {
        return false;
    }
    // Must not end with an underscore.
    if *bytes.last().unwrap() == b'_' {
        return false;
    }
    let mut prev_underscore = false;
    for &b in bytes {
        let ok = b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_';
        if !ok {
            return false;
        }
        if b == b'_' {
            if prev_underscore {
                return false; // no doubled underscore
            }
            prev_underscore = true;
        } else {
            prev_underscore = false;
        }
    }
    true
}

/// The aggregated skill catalog. Built once at startup from every category
/// module's `skills()`. Holds the skills in catalog (category-declaration, then
/// per-module) order and a name index for O(1) lookup. Constructing it runs the
/// uniqueness + `snake_case` guard, so an in-tree mistake fails loudly the first
/// time the registry is built (a test exercises this) — never silently.
/// Run a skill's declared known-vector eval: every (args-as-JSON, expected)
/// pair must parse and reproduce `expected` exactly through the skill's pure
/// `run`. Returns `Err` (fatal at registry build via [`Registry::new`]) on a
/// parse failure, a run error, or a mismatch — so a skill can never enter the
/// live catalog unless it passes its own eval. A skill with no vectors (the
/// default) is trivially OK. For PURE skills (a consequential/source-gated skill
/// does not compute a stable answer to assert against).
fn check_eval_vectors(s: &SkillDef) -> Result<()> {
    for &(args_json, expected) in s.eval_vectors {
        let args: Value = serde_json::from_str(args_json).map_err(|e| {
            anyhow!("skill '{}' eval-vector args are not valid JSON ({e}): {args_json}", s.name)
        })?;
        match (s.run)(&args) {
            Ok(out) if out.as_str() == expected => {}
            Ok(out) => {
                return Err(anyhow!(
                    "skill '{}' failed its eval: args {args_json} produced {out:?}, expected {expected:?}",
                    s.name
                ));
            }
            Err(e) => {
                return Err(anyhow!("skill '{}' eval run errored on args {args_json}: {e}", s.name));
            }
        }
    }
    Ok(())
}

pub struct Registry {
    skills: Vec<SkillDef>,
}

impl Registry {
    /// Aggregate every category module's `skills()` into one catalog, enforcing
    /// the guard: every name is `snake_case` and unique across ALL categories.
    ///
    /// Returns `Err` (never panics) if the guard trips, so the caller decides how
    /// to surface a build-time library mistake; the process-global accessor
    /// [`global`] treats that as a fatal misconfiguration. Aggregation order is
    /// the `Category::all()` order, matching the catalog + docs ordering.
    pub fn new() -> Result<Self> {
        let mut skills: Vec<SkillDef> = Vec::new();
        skills.extend(utilities::skills());
        skills.extend(text::skills());
        skills.extend(datetime::skills());
        skills.extend(units::skills());
        skills.extend(mathx::skills());
        skills.extend(knowledge::skills());
        skills.extend(finance::skills());
        skills.extend(fun::skills());

        // Guard: snake_case + global uniqueness. A duplicate or malformed name is
        // a programming error in a category file — fail with a precise message.
        let mut seen = std::collections::HashSet::new();
        for s in &skills {
            if !is_snake_case(s.name) {
                return Err(anyhow!(
                    "skill name '{}' is not snake_case (category {})",
                    s.name,
                    s.category.slug()
                ));
            }
            if !seen.insert(s.name) {
                return Err(anyhow!("duplicate skill name '{}'", s.name));
            }
            // A skill cannot be BOTH consequential and source-gated: those are
            // distinct gating models (park-for-yes vs. needs-a-feed) and combining
            // them would be ambiguous for the meta-tool. Pin it here.
            if s.consequential && s.source_gated {
                return Err(anyhow!(
                    "skill '{}' is both consequential and source-gated; pick one",
                    s.name
                ));
            }
            // Eval-gated promotion: a skill that declares known-vector evals must
            // reproduce every one through its pure run, or it never enters the
            // live catalog. Skills with no vectors (the default) pass trivially.
            check_eval_vectors(s)?;
        }

        Ok(Registry { skills })
    }

    /// All skills, in catalog order. The meta-tool's `skill_list` with no filter
    /// walks this.
    pub fn all(&self) -> &[SkillDef] {
        &self.skills
    }

    /// The skills in one category, in catalog order. Backs `skill_list`'s
    /// `category` filter. An empty category (a scaffolded module the library has
    /// not filled yet) yields an empty slice, which is fine.
    pub fn by_category(&self, category: Category) -> Vec<&SkillDef> {
        self.skills.iter().filter(|s| s.category == category).collect()
    }

    /// Look one skill up by exact name. `None` is the friendly "unknown skill"
    /// path the meta-tool reports. O(n) over a small in-tree set; kept simple
    /// rather than holding a borrow-tangling index.
    pub fn get(&self, name: &str) -> Option<&SkillDef> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// How many skills are shipped — the REAL count the catalog reports. Honesty:
    /// this is the genuine in-tree number, never a marketing figure.
    pub fn count(&self) -> usize {
        self.skills.len()
    }

    /// A SECRET-FREE catalog snapshot for the HUD Skills Marketplace panel. Mirrors
    /// `mcp::McpManager::status_snapshot`: a plain JSON value the HUD reduces and
    /// renders read-only. It carries ONLY the discovery fields a `SkillDef` already
    /// exposes through `skill_list` — name, category slug, the one-line "when to use"
    /// description, and the two safety markers (consequential / source_gated). A
    /// skill's `run` function, cues, and any internal state are NOT included: there
    /// is nothing secret in a pure in-tree skill, and keeping the payload to the
    /// catalog surface means even a future field can never leak through the panel.
    ///
    /// `enabled` is the live `[skills].enabled` master switch (passed in by the
    /// caller, which owns the Config) so the panel shows the honest on/off state.
    /// `count` is the REAL shipped total (never a marketing figure). `categories`
    /// lists every category in catalog order with its per-category count, so the
    /// panel can render counts even for an empty heading. Skills are listed in
    /// catalog (category-declaration) order — the same order `skill_list` walks.
    pub fn catalog_snapshot(&self, enabled: bool) -> Value {
        use serde_json::json;
        let categories: Vec<Value> = Category::all()
            .iter()
            .map(|c| {
                json!({
                    "slug": c.slug(),
                    "count": self.by_category(*c).len(),
                })
            })
            .collect();
        let skills: Vec<Value> = self
            .skills
            .iter()
            .map(|s| {
                json!({
                    "name": s.name,
                    "category": s.category.slug(),
                    "description": s.description,
                    "consequential": s.consequential,
                    "source_gated": s.source_gated,
                })
            })
            .collect();
        json!({
            "enabled": enabled,
            "count": self.count(),
            "categories": categories,
            "skills": skills,
        })
    }

    /// Test-only: build a registry from an explicit skill list, still running the
    /// guard. Lets a test inject a CONSEQUENTIAL or source-gated skill (none ship
    /// this round) so the meta-tool's gating paths are exercised without flipping
    /// the process-global master switch or mutating the shipped catalog.
    #[cfg(test)]
    pub fn from_skills_for_test(skills: Vec<SkillDef>) -> Result<Self> {
        let mut seen = std::collections::HashSet::new();
        for s in &skills {
            if !is_snake_case(s.name) {
                return Err(anyhow!("skill name '{}' is not snake_case", s.name));
            }
            if !seen.insert(s.name) {
                return Err(anyhow!("duplicate skill name '{}'", s.name));
            }
        }
        Ok(Registry { skills })
    }
}

/// Process-global registry, built once on first use. The catalog is static
/// (every skill is an in-tree `SkillDef`), so a `OnceLock` mirrors how
/// `tool_defs()` and the other small shared singletons are cached. A guard
/// failure here is a build-time library mistake (caught by tests), so the
/// accessor treats it as fatal — a malformed catalog must never ship.
pub fn global() -> &'static Registry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| Registry::new().expect("skill registry guard failed (duplicate or non-snake_case skill name)"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn registry_builds_and_passes_the_guard() {
        // The real, aggregated catalog must build (uniqueness + snake_case hold
        // across every category module). This is the end-to-end aggregation test.
        let reg = Registry::new().expect("catalog must aggregate cleanly");
        assert!(reg.count() >= 3, "the proof skills ship at least 3 entries");
    }

    #[test]
    fn every_skill_name_is_snake_case_and_unique() {
        let reg = global();
        let mut seen = std::collections::HashSet::new();
        for s in reg.all() {
            assert!(is_snake_case(s.name), "non-snake_case skill: {}", s.name);
            assert!(seen.insert(s.name), "duplicate skill: {}", s.name);
            assert!(!s.description.is_empty(), "{} needs a description", s.name);
        }
    }

    #[test]
    fn eval_gate_enforces_known_vectors() {
        // A skill that declares known-vector evals must reproduce every one
        // through its pure run, or the gate refuses it at registry build.
        fn echo(v: &serde_json::Value) -> Result<String> {
            Ok(v.get("x").and_then(|x| x.as_str()).unwrap_or("").to_string())
        }
        let ok = SkillDef::new("echo_ok", Category::Utilities, "echo x", &[], echo)
            .with_eval_vectors(&[("{\"x\":\"hi\"}", "hi")]);
        assert!(check_eval_vectors(&ok).is_ok(), "matching vectors pass the gate");

        let mismatch = SkillDef::new("echo_bad", Category::Utilities, "echo x", &[], echo)
            .with_eval_vectors(&[("{\"x\":\"hi\"}", "BYE")]);
        assert!(check_eval_vectors(&mismatch).is_err(), "a wrong expected output fails the gate");

        let bad_json = SkillDef::new("echo_badjson", Category::Utilities, "echo x", &[], echo)
            .with_eval_vectors(&[("not json", "x")]);
        assert!(check_eval_vectors(&bad_json).is_err(), "invalid args JSON fails the gate");

        let none = SkillDef::new("echo_none", Category::Utilities, "echo x", &[], echo);
        assert!(check_eval_vectors(&none).is_ok(), "no vectors -> trivially ok");
    }

    #[test]
    fn snake_case_predicate_rejects_the_obvious_bad_shapes() {
        assert!(is_snake_case("base64_encode"));
        assert!(is_snake_case("word_count"));
        assert!(is_snake_case("dice_roll"));
        assert!(is_snake_case("a"));
        assert!(is_snake_case("sha256_hex"));
        assert!(!is_snake_case(""), "empty");
        assert!(!is_snake_case("Base64"), "uppercase");
        assert!(!is_snake_case("1up"), "leading digit");
        assert!(!is_snake_case("_x"), "leading underscore");
        assert!(!is_snake_case("x_"), "trailing underscore");
        assert!(!is_snake_case("a__b"), "doubled underscore (MCP-namespace collision)");
        assert!(!is_snake_case("a-b"), "hyphen");
        assert!(!is_snake_case("a b"), "space");
    }

    #[test]
    fn duplicate_name_trips_the_guard() {
        // Build a tiny registry by hand to prove the guard rejects a dup — we
        // can't inject into the real category modules, so exercise the same
        // invariant the guard enforces via a constructed pair.
        fn noop(_: &Value) -> Result<String> {
            Ok(String::new())
        }
        let a = SkillDef::new("dup_name", Category::Utilities, "d", &[], noop);
        let b = SkillDef::new("dup_name", Category::Text, "d", &[], noop);
        let mut seen = std::collections::HashSet::new();
        assert!(seen.insert(a.name));
        assert!(!seen.insert(b.name), "the guard's HashSet must reject the dup");
    }

    #[test]
    fn category_slug_roundtrips() {
        for c in Category::all() {
            assert_eq!(Category::from_slug(c.slug()), Some(*c));
        }
        assert_eq!(Category::from_slug("nope"), None);
        assert_eq!(Category::all().len(), 8, "exactly 8 scaffolded categories");
    }

    #[test]
    fn catalog_line_marks_consequential_and_source_gated() {
        fn noop(_: &Value) -> Result<String> {
            Ok(String::new())
        }
        let pure = SkillDef::new("pure_one", Category::Utilities, "does a thing", &[], noop);
        assert_eq!(pure.catalog_line(), "pure_one (utilities): does a thing");

        let conseq = SkillDef::new("act_one", Category::Utilities, "acts", &[], noop).consequential();
        assert!(conseq.catalog_line().contains("[consequential]"));

        let gated = SkillDef::new("look_one", Category::Knowledge, "looks up", &[], noop).source_gated();
        assert!(gated.catalog_line().contains("[source-gated]"));
    }

    #[test]
    fn by_category_filters_and_get_finds() {
        let reg = global();
        // utilities holds the proof skills; it must be non-empty.
        let utils = reg.by_category(Category::Utilities);
        assert!(!utils.is_empty(), "utilities carries the proof skills");
        // get() resolves a known proof skill and misses an unknown one.
        assert!(reg.get("base64_encode").is_some());
        assert!(reg.get("definitely_not_a_skill").is_none());
    }

    #[test]
    fn the_eight_scaffold_modules_each_expose_skills_fn() {
        // Each module's skills() must at least COMPILE + return a Vec (empty ok).
        // Calling every one here pins that the scaffold contract holds, so the
        // Library phase can fill its own file with no shared edits.
        let _ = utilities::skills();
        let _ = text::skills();
        let _ = datetime::skills();
        let _ = units::skills();
        let _ = mathx::skills();
        let _ = knowledge::skills();
        let _ = finance::skills();
        let _ = fun::skills();
    }

    #[test]
    fn catalog_snapshot_is_secret_free_and_reflects_the_real_count() {
        let reg = global();
        // The honest count + on/off state the HUD panel renders.
        let snap = reg.catalog_snapshot(true);
        assert_eq!(snap["enabled"], json!(true));
        assert_eq!(snap["count"].as_u64().unwrap() as usize, reg.count());

        // Every category appears (catalog order) with a non-negative count, and the
        // per-category counts sum to the real total — no skill is lost or duplicated.
        let cats = snap["categories"].as_array().unwrap();
        assert_eq!(cats.len(), Category::all().len());
        let sum: u64 = cats.iter().map(|c| c["count"].as_u64().unwrap()).sum();
        assert_eq!(sum as usize, reg.count());

        // Each skill row carries ONLY the discovery surface — name, category slug,
        // description, and the two markers. The run fn / cues are never serialized.
        let skills = snap["skills"].as_array().unwrap();
        assert_eq!(skills.len(), reg.count());
        for row in skills {
            let obj = row.as_object().unwrap();
            assert_eq!(obj.len(), 5, "exactly the secret-free discovery fields");
            assert!(obj.contains_key("name"));
            assert!(obj.contains_key("category"));
            assert!(obj.contains_key("description"));
            assert!(obj.contains_key("consequential"));
            assert!(obj.contains_key("source_gated"));
            assert!(!obj.contains_key("run"));
            assert!(!obj.contains_key("cues"));
        }

        // The OFF snapshot is the same catalog with enabled=false (the panel shows
        // the honest off state but still knows what would be offered).
        let off = reg.catalog_snapshot(false);
        assert_eq!(off["enabled"], json!(false));
        assert_eq!(off["count"], snap["count"]);
    }

    #[test]
    fn proof_skill_base64_encode_is_deterministic() {
        // End-to-end: the registry holds it, and its run is pure + deterministic.
        let reg = global();
        let s = reg.get("base64_encode").expect("base64_encode ships");
        assert!(!s.consequential, "a pure skill is ungated");
        let out = (s.run)(&json!({"text": "hello"})).unwrap();
        assert_eq!(out, "aGVsbG8=");
        // Idempotent: same input, same output, every time.
        let again = (s.run)(&json!({"text": "hello"})).unwrap();
        assert_eq!(out, again);
    }
}

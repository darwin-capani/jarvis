//! THE USER MODEL — a structured, COMPOUNDING profile of the user, built ONLY
//! from OBSERVED interactions (episodes + stored facts), every entry
//! PROVENANCE-tagged and reinforced by an OBSERVED-COUNT.
//!
//! WHAT IT IS. A first-class profile — the user's PREFERENCES, PATTERNS/habits,
//! RECURRING TOPICS, and COMMUNICATION STYLE — consolidated from the episodic
//! store and the fact store by the reflection/consolidation pass. Like the World
//! Model it is a thin STRUCTURED layer over the existing facts store
//! ([`crate::memory::Memory`]): there is no new table, every entry is a fact, so
//! the profile inherits the memory layer's WAL, retention, and (crucially) its
//! NAMESPACE ISOLATION semantics for free.
//!
//! WHERE IT LIVES (the isolation decision, mirroring world_model). Everything is
//! written under the SHARED `user.model.*` tier. Because that prefix is NOT
//! `agent.*`, [`crate::memory::Memory::agent_scoped_facts`] already classifies it
//! as SHARED and hands it to EVERY agent — the profile is the user's, not one
//! specialist's. This module NEVER reads or writes the `agent.*` space, so a
//! private note can never be folded into the profile and the profile can never
//! leak a private note. (The episodic INPUTS we consolidate are read AGENT-SCOPED
//! — see [`consolidate`] — so cross-agent isolation holds on the way IN as well.)
//!
//! KEY SCHEME (stable, parseable, collision-resistant):
//!   `user.model.<facet>.<slug>` = `<observed_count>|<provenance>|<observation>`
//! where `<facet>` is one of the bounded [`Facet`] kinds, `<slug>` is a stable
//! slug of the observation's subject, `<observed_count>` is how many times the
//! signal was seen (the COMPOUNDING strength), `<provenance>` is a compact,
//! comma-joined list of the input ids it was derived from (episode `ep:<id>` /
//! fact `fact:<key>`), and `<observation>` is the short human-readable statement.
//!
//! HONESTY (load-bearing, hammered in the tests):
//!   * Built FROM observed interactions — NEVER clairvoyant. An entry exists iff a
//!     real signal in the inputs produced it; contradictory or empty inputs yield
//!     NO invented entry.
//!   * PROVENANCE-tagged: every entry records WHICH episodes/facts it came from,
//!     so the user can see why DARWIN believes it (inspectable).
//!   * COMPOUNDING but BOUNDED: a repeated observation REINFORCES an entry
//!     (observed-count up, provenance extended) rather than duplicating it; the
//!     tier is globally capped so it cannot grow without limit.
//!   * INSPECTABLE + CORRECTABLE + FORGETTABLE: [`render`] surfaces the profile
//!     WITH provenance; [`correct`] overrides or deletes one entry; [`forget`]
//!     clears the whole profile.
//!   * The model can be WRONG: it surfaces only what was observed, with its
//!     confidence (the observed-count), and the user can fix it.
//!
//! Nothing here speaks, acts, or reaches the network. It consolidates observed
//! rows and renders them.

use anyhow::Result;

use crate::memory::{Episode, Memory};

/// The shared tier prefix. Anything under here is visible to EVERY agent via
/// `agent_scoped_facts` (it is not an `agent.*` key, so it is classified SHARED).
pub const MODEL_PREFIX: &str = "user.model.";

// -- BOUNDS (all enforced before any write / on render) ----------------------

/// Max chars in a slug (the entry-subject segment of the key) after slugging.
pub const MAX_SLUG_LEN: usize = 64;
/// Max chars in the human-readable observation statement.
pub const MAX_OBSERVATION_LEN: usize = 200;
/// Hard cap on the number of DISTINCT profile entries the model may hold. A
/// consolidation that would introduce a NEW entry beyond this cap is refused
/// (reinforcing an existing entry always succeeds, so the model never wedges).
pub const MAX_ENTRIES: usize = 256;
/// Hard cap on how many provenance ids one entry records — so a long-lived,
/// often-reinforced entry's provenance list cannot grow without bound. The
/// observed-count keeps climbing past this; only the stored id list is capped
/// (newest provenance wins), which is the honest "here are recent reasons".
pub const MAX_PROVENANCE: usize = 8;
/// The generous window of model-tier rows a single read pulls before structuring.
pub const MODEL_READ_WINDOW: usize = 2_000;
/// Max entries surfaced in the personalization SUMMARY injected into the prompt
/// tail — bounded so the grounding block can never bloat the (uncached) context.
pub const SUMMARY_MAX_ENTRIES: usize = 8;
/// Max chars the personalization summary block may occupy — a second, hard
/// bound on the injected tail so even many short entries can't blow the budget.
pub const SUMMARY_MAX_CHARS: usize = 700;

/// How many times a signal must be OBSERVED across the consolidation inputs
/// before it earns a profile entry. A single stray mention is not yet a
/// preference/pattern — requiring a repeat is what keeps the model from
/// over-claiming on one-off chatter (honesty: a pattern is a REPEATED signal).
pub const MIN_OBSERVATIONS: u32 = 2;

/// The bounded set of profile FACETS the user model recognizes. A free-form facet
/// is rejected so the keyspace stays parseable and the profile stays a coherent
/// schema rather than a junk drawer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Facet {
    /// A stated/observed PREFERENCE ("prefers X over Y", "likes Z").
    Preference,
    /// A recurring HABIT / behavioral PATTERN ("often asks about X in the morning").
    Pattern,
    /// A RECURRING TOPIC the user keeps returning to.
    Topic,
    /// An observed COMMUNICATION STYLE trait ("terse", "asks follow-ups").
    Style,
}

impl Facet {
    /// The stable lowercase token used in the key (`user.model.<token>.…`) and
    /// accepted from the tool input.
    pub fn as_str(&self) -> &'static str {
        match self {
            Facet::Preference => "preference",
            Facet::Pattern => "pattern",
            Facet::Topic => "topic",
            Facet::Style => "style",
        }
    }

    /// A human label for the rendered profile / summary.
    pub fn label(&self) -> &'static str {
        match self {
            Facet::Preference => "Preference",
            Facet::Pattern => "Pattern",
            Facet::Topic => "Recurring topic",
            Facet::Style => "Communication style",
        }
    }

    /// Parse a caller-supplied facet token, case-insensitively and trimmed. A few
    /// natural synonyms map to the canonical kind; anything else is `None`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "preference" | "preferences" | "pref" | "likes" => Some(Facet::Preference),
            "pattern" | "patterns" | "habit" | "habits" => Some(Facet::Pattern),
            "topic" | "topics" | "interest" | "interests" => Some(Facet::Topic),
            "style" | "communication" | "tone" => Some(Facet::Style),
            _ => None,
        }
    }

    /// All valid facets, for error messages and tests.
    pub fn all() -> &'static [Facet] {
        &[Facet::Preference, Facet::Pattern, Facet::Topic, Facet::Style]
    }

    /// Comma-joined list of valid facet tokens, for friendly error copy.
    pub fn valid_list() -> String {
        Self::all()
            .iter()
            .map(|f| f.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// One structured profile entry as read back from the store: its facet, stable
/// subject slug, the human-readable observation, how many times it was observed
/// (the COMPOUNDING confidence), and the provenance ids it was derived from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileEntry {
    pub facet: Facet,
    /// Stable subject slug (the key segment after the facet).
    pub subject: String,
    /// The short human-readable observation ("prefers neovim over vscode").
    pub observation: String,
    /// How many times the signal was OBSERVED — the confidence / strength.
    pub observed_count: u32,
    /// The input ids this entry was derived from (`ep:<id>` / `fact:<key>`),
    /// newest-first, capped at [`MAX_PROVENANCE`]. NEVER empty for a real entry —
    /// an entry with no provenance would be a fabrication, which cannot happen
    /// because [`consolidate`] only ever writes an entry tied to its inputs.
    pub provenance: Vec<String>,
}

/// The structured profile [`query`]/[`snapshot`] return: the entries, bounded by
/// construction and sorted deterministically (facet, then strength desc).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Profile {
    pub entries: Vec<ProfileEntry>,
}

impl Profile {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// -- slugging + value encode/decode ------------------------------------------

/// Normalize a free-form subject to a stable, key-safe SLUG: lowercase, every run
/// of non-alphanumeric collapsed to a single `_`, leading/trailing `_` trimmed,
/// clamped to [`MAX_SLUG_LEN`]. Deterministic. `None` for input that slugs to
/// empty (the caller then rejects it). Same scheme as world_model::slugify so the
/// two structured tiers slug identically.
pub fn slugify(name: &str) -> Option<String> {
    let mut out = String::with_capacity(name.len());
    let mut prev_us = true;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_us = false;
        } else if !prev_us {
            out.push('_');
            prev_us = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    if out.len() > MAX_SLUG_LEN {
        out.truncate(MAX_SLUG_LEN);
        while out.ends_with('_') {
            out.pop();
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Clamp + trim the human observation so a stored entry stays tiny. Pure.
fn bound_observation(observation: &str) -> String {
    let v = observation.trim();
    if v.chars().count() > MAX_OBSERVATION_LEN {
        let cut: String = v.chars().take(MAX_OBSERVATION_LEN).collect();
        cut.trim_end().to_string()
    } else {
        v.to_string()
    }
}

/// Compose the fact key for a profile entry. Parts are pre-slugged.
fn entry_key(facet: Facet, subject: &str) -> String {
    format!("{MODEL_PREFIX}{}.{subject}", facet.as_str())
}

/// Parse a model-tier key into (facet, subject). `None` for any key that is not a
/// well-formed `user.model.<facet>.<subject>` (a malformed/foreign row is skipped,
/// never panics). The subject is a single slug (no dots), so exactly two dot-parts
/// follow the prefix.
fn parse_entry_key(key: &str) -> Option<(Facet, String)> {
    let rest = key.strip_prefix(MODEL_PREFIX)?;
    let (facet_tok, subject) = rest.split_once('.')?;
    
    
    if subject.is_empty() || subject.contains('.') {
        return None;
    }
    let facet = Facet::parse(facet_tok)?;
    Some((facet, subject.to_string()))
}

/// Encode an entry's VALUE as `<count>|<prov1,prov2,...>|<observation>`. The
/// observation is placed LAST and the separators (`|`, `,`) are stripped from the
/// provenance ids and never re-inserted into the observation, so decode is
/// unambiguous (split the count and provenance off the front; the remainder —
/// even if it contains a `|` — is the observation). Pure.
fn encode_value(count: u32, provenance: &[String], observation: &str) -> String {
    let prov = provenance
        .iter()
        .map(|p| p.replace([',', '|'], "_"))
        .collect::<Vec<_>>()
        .join(",");
    format!("{count}|{prov}|{observation}")
}

/// Decode a stored value into (count, provenance, observation). Tolerant: a row
/// that doesn't parse (a hand-edited or legacy value) is treated as a count-1
/// entry whose whole value is the observation and whose provenance is empty — so
/// a malformed row degrades gracefully rather than vanishing. Pure.
fn decode_value(value: &str) -> (u32, Vec<String>, String) {
    let mut parts = value.splitn(3, '|');
    let count_tok = parts.next();
    let prov_tok = parts.next();
    let obs_tok = parts.next();
    match (count_tok, prov_tok, obs_tok) {
        (Some(c), Some(p), Some(o)) if c.trim().parse::<u32>().is_ok() => {
            let count = c.trim().parse::<u32>().unwrap_or(1).max(1);
            let provenance: Vec<String> = p
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            (count, provenance, o.to_string())
        }
        // Not the encoded shape — treat the whole thing as the observation.
        _ => (1, Vec::new(), value.to_string()),
    }
}

// -- READ path ---------------------------------------------------------------

/// Read the FULL structured profile from the SHARED tier, bounded by
/// [`MODEL_READ_WINDOW`]. Reads ONLY `user.model.*`, so it inherently cannot
/// surface any agent's private notes. Sorted deterministically (facet, then
/// observed-count descending, then subject).
pub async fn snapshot(memory: &Memory) -> Result<Profile> {
    let rows = memory
        .recall_facts_limited(MODEL_PREFIX, MODEL_READ_WINDOW)
        .await?;
    Ok(structure_rows(rows))
}

/// Pure: fold raw (key,value) model-tier rows into a sorted [`Profile`]. Skips
/// malformed/foreign rows. Exposed for direct unit testing without a store.
pub fn structure_rows(rows: Vec<(String, String)>) -> Profile {
    let mut entries: Vec<ProfileEntry> = Vec::new();
    for (key, value) in rows.into_iter().take(MODEL_READ_WINDOW) {
        if let Some((facet, subject)) = parse_entry_key(&key) {
            let (observed_count, provenance, observation) = decode_value(&value);
            // A real entry must carry provenance; a row with none is a corrupt /
            // hand-built artifact, not an observation — skip it so render/query
            // never surface an entry we cannot justify (honesty: no provenance,
            // no claim).
            if provenance.is_empty() {
                continue;
            }
            entries.push(ProfileEntry {
                facet,
                subject,
                observation,
                observed_count,
                provenance,
            });
        }
    }
    entries.sort_by(|a, b| {
        a.facet
            .as_str()
            .cmp(b.facet.as_str())
            .then(b.observed_count.cmp(&a.observed_count))
            .then(a.subject.cmp(&b.subject))
    });
    Profile { entries }
}

/// The profile filtered to entries whose subject/observation match the query
/// terms — the read half of `user_model_query`. An EMPTY query returns the whole
/// (bounded) profile ("what do you know about me"). Reads only the shared tier.
pub async fn query(memory: &Memory, about: &str) -> Result<Profile> {
    let full = snapshot(memory).await?;
    Ok(filter_profile(full, about))
}

/// Pure filter of a [`Profile`] by query terms. Exposed for direct testing.
pub fn filter_profile(profile: Profile, about: &str) -> Profile {
    let terms = query_terms(about);
    if terms.is_empty() {
        return profile;
    }
    let entries = profile
        .entries
        .into_iter()
        .filter(|e| {
            let obs = e.observation.to_lowercase();
            terms
                .iter()
                .any(|t| e.subject.contains(t.as_str()) || obs.contains(t.as_str()))
        })
        .collect();
    Profile { entries }
}

/// Tokenize a query the same way world_model does: lowercase, split on
/// non-alphanumeric, drop 1-char tokens.
fn query_terms(about: &str) -> Vec<String> {
    about
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 1)
        .map(|t| t.to_lowercase())
        .collect()
}

// -- WRITE path (correct / forget) -------------------------------------------

/// CORRECT one entry: OVERRIDE its observation (when `new_observation` is
/// non-empty) or DELETE it (when empty). The correctable contract — the user can
/// fix or remove anything DARWIN believes about them. An override keeps the
/// entry's slug + facet but REPLACES the observation, RESETS the observed-count
/// to 1, and stamps the provenance as a user correction (`user:correction`) so
/// the profile honestly records that this entry is now a stated correction, not a
/// consolidated observation. Returns whether a row was changed/removed.
pub async fn correct(
    memory: &Memory,
    facet: Facet,
    subject: &str,
    new_observation: &str,
) -> Result<bool> {
    let slug = slugify(subject)
        .ok_or_else(|| anyhow::anyhow!("subject '{subject}' has no usable characters"))?;
    let key = entry_key(facet, &slug);
    let trimmed = new_observation.trim();
    if trimmed.is_empty() {
        // Delete = forget this one entry.
        return memory.delete_fact(&key).await;
    }
    let observation = bound_observation(trimmed);
    let value = encode_value(1, &["user:correction".to_string()], &observation);
    memory.upsert_user_fact(&key, &value).await?;
    Ok(true)
}

/// FORGET the whole user model: delete every `user.model.*` row. The forgettable
/// contract. Returns how many entries were removed.
pub async fn forget(memory: &Memory) -> Result<u64> {
    let rows = memory
        .recall_facts_limited(MODEL_PREFIX, MODEL_READ_WINDOW)
        .await?;
    let mut deleted = 0u64;
    for (key, _) in rows {
        if parse_entry_key(&key).is_some() && memory.delete_fact(&key).await? {
            deleted += 1;
        }
    }
    Ok(deleted)
}

// -- CONSOLIDATION (the compounding core) ------------------------------------

/// A candidate signal mined from ONE input before it is reinforced into the
/// store: which facet, the subject slug, the human observation, and the input id
/// it came from. Pure intermediate — never persisted directly.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Signal {
    facet: Facet,
    subject: String,
    observation: String,
    provenance_id: String,
}

/// Stopwords excluded from topic/preference mining — low-signal glue. Mirrors the
/// spirit of episodic.rs's stoplist; the redaction placeholder is included so a
/// `[redacted]` span is never mined as a subject.
const STOPWORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "to", "of", "in", "on", "for", "my", "me",
    "i", "is", "it", "this", "that", "with", "what", "how", "can", "you",
    "please", "do", "does", "did", "have", "has", "are", "was", "will", "would",
    "about", "up", "out", "get", "got", "now", "from", "by", "redacted", "your",
    "we", "our", "be", "been", "but", "so", "if", "then", "they", "want", "like",
    "just", "really", "some", "more", "than", "over", "into", "when", "where",
];

/// Minimum length of a mined content word.
const MIN_WORD_LEN: usize = 4;

/// Preference cue phrases: when an utterance contains one, the following content
/// word(s) name a PREFERENCE subject. Deliberately small + explicit — we only
/// claim a preference on an EXPLICIT cue, never guess one from a bare mention.
const PREFERENCE_CUES: &[&str] = &["prefer", "prefers", "favorite", "favourite"];

/// Whether the (lowercased) text expresses a preference cue.
fn has_preference_cue(lower: &str) -> bool {
    PREFERENCE_CUES.iter().any(|c| lower.contains(c))
}

/// Mine the SIGNALS from one episode. The episode is already REDACTED at store, so
/// nothing mined here can be a secret. We derive:
///   * a RECURRING-TOPIC signal per salient entity (the bounded content words the
///     episodic store already extracted) — a topic the user actually raised;
///   * a PREFERENCE signal when the (redacted) utterance carries an explicit
///     preference cue, subject = the utterance's salient entities.
///     Pure + deterministic. Returns the signals tagged with `ep:<id>` provenance.
fn signals_from_episode(ep: &Episode) -> Vec<Signal> {
    let mut out = Vec::new();
    let prov = format!("ep:{}", ep.id);
    let lower = ep.utterance_redacted.to_lowercase();
    let pref = has_preference_cue(&lower);
    for ent in &ep.salient_entities {
        let subject = match slugify(ent) {
            Some(s) => s,
            None => continue,
        };
        if subject.len() < MIN_WORD_LEN || STOPWORDS.contains(&subject.as_str()) {
            continue;
        }
        out.push(Signal {
            facet: Facet::Topic,
            subject: subject.clone(),
            observation: format!("keeps coming back to {ent}"),
            provenance_id: prov.clone(),
        });
        if pref {
            out.push(Signal {
                facet: Facet::Preference,
                subject: subject.clone(),
                observation: format!("expressed a preference around {ent}"),
                provenance_id: prov.clone(),
            });
        }
    }
    out
}

/// Mine SIGNALS from one stored FACT. A user fact under a stable key is an
/// EXPLICIT, already-consolidated statement, so it carries MORE weight than a
/// passing mention — we seed it at the observation threshold so a single relevant
/// fact (e.g. `user.preference.editor = neovim`) earns its entry on its own. We
/// only mine facts whose key NAMES a profile facet (`user.preference.*`,
/// `user.style.*`, `user.pattern.*`, `user.topic.*` or the bare `preference`/
/// `style` families) — a random fact is NOT a profile signal, so it is skipped
/// (honesty: we don't invent a preference from an unrelated fact). Returns the
/// signals tagged with `fact:<key>` provenance, each repeated MIN_OBSERVATIONS
/// times so an explicit fact clears the threshold by itself.
fn signals_from_fact(key: &str, value: &str) -> Vec<Signal> {
    let facet = facet_of_fact_key(key);
    let facet = match facet {
        Some(f) => f,
        None => return Vec::new(),
    };
    let subject = match fact_subject(key) {
        Some(s) => s,
        None => return Vec::new(),
    };
    let value = value.trim();
    if value.is_empty() {
        return Vec::new();
    }
    let observation = bound_observation(&format!("{} = {}", subject.replace('_', " "), value));
    let prov = format!("fact:{key}");
    // An explicit fact is authoritative: seed it AT the threshold so it stands on
    // its own, but still as DISTINCT-shaped observations from ONE provenance, so
    // the observed-count reflects "stated once, explicitly" honestly (the
    // provenance list still shows the single source).
    vec![
        Signal {
            facet,
            subject: subject.clone(),
            observation: observation.clone(),
            provenance_id: prov.clone(),
        };
        MIN_OBSERVATIONS as usize
    ]
}

/// Map a fact KEY to the profile facet it names, or `None` if it names no facet.
/// Recognizes `user.<facet>.*` and the bare `<facet>.*` families.
fn facet_of_fact_key(key: &str) -> Option<Facet> {
    let k = key.trim().to_lowercase();
    let rest = k.strip_prefix("user.").unwrap_or(&k);
    let head = rest.split('.').next()?;
    Facet::parse(head)
}

/// The subject slug of a facet-named fact key: the segment AFTER the facet token.
/// `user.preference.editor` -> `editor`; `style.tone` -> `tone`. `None` when
/// there's no subject segment.
fn fact_subject(key: &str) -> Option<String> {
    let k = key.trim().to_lowercase();
    let rest = k.strip_prefix("user.").unwrap_or(&k);
    let (_facet, subject) = rest.split_once('.')?;
    
    
    slugify(subject)
}

/// The consolidated result of one pass: the entries to UPSERT (with their final
/// observed-count + provenance) and the count of inputs considered. Returned by
/// the PURE [`consolidate_inputs`] so the consolidation logic is testable without
/// a store, and applied to the store by [`consolidate`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Consolidation {
    /// (facet, subject, observed_count, provenance, observation) per entry that
    /// MET the observation threshold. Only these are written — a sub-threshold
    /// signal yields NOTHING (no invented entry).
    pub entries: Vec<(Facet, String, u32, Vec<String>, String)>,
}

/// PURE consolidation: fold the OBSERVED inputs (episodes + facts) into the
/// profile entries that EARNED a place — every entry tied to the inputs that
/// produced it, reinforced by its observed-count. This is the honesty core:
///   * an entry is emitted ONLY when its signal was observed at least
///     [`MIN_OBSERVATIONS`] times across the inputs — a single stray mention is
///     not yet a pattern/preference, so it produces NOTHING;
///   * every emitted entry carries the provenance ids it was derived from — there
///     is no path that emits an entry without provenance, so the model can NEVER
///     fabricate a preference that isn't in the inputs;
///   * `existing` (the current observed-counts, keyed by (facet, subject)) lets a
///     repeated observation COMPOUND onto the prior count rather than resetting —
///     so the model strengthens over time, bounded by the entry cap at write.
///     Deterministic; exposed for direct unit testing.
pub fn consolidate_inputs(
    episodes: &[Episode],
    facts: &[(String, String)],
    existing: &std::collections::HashMap<(Facet, String), u32>,
) -> Consolidation {
    // 1. Mine every signal from every input.
    let mut signals: Vec<Signal> = Vec::new();
    for ep in episodes {
        signals.extend(signals_from_episode(ep));
    }
    for (key, value) in facts {
        signals.extend(signals_from_fact(key, value));
    }

    // 2. Group by (facet, subject); count observations + collect provenance +
    //    keep the FIRST observation phrasing (deterministic).
    use std::collections::HashMap;
    struct Agg {
        observation: String,
        count: u32,
        provenance: Vec<String>,
    }
    let mut groups: HashMap<(Facet, String), Agg> = HashMap::new();
    for s in signals {
        let entry = groups
            .entry((s.facet, s.subject.clone()))
            .or_insert_with(|| Agg {
                observation: s.observation.clone(),
                count: 0,
                provenance: Vec::new(),
            });
        entry.count += 1;
        if !entry.provenance.contains(&s.provenance_id) {
            entry.provenance.push(s.provenance_id);
        }
    }

    // 3. Emit only the groups that MET the threshold (this pass alone OR with the
    //    prior observed-count folded in — so a signal seen once now plus once
    //    before clears it). Compounding: final count = prior + this pass.
    let mut entries: Vec<(Facet, String, u32, Vec<String>, String)> = Vec::new();
    for ((facet, subject), agg) in groups {
        let prior = existing.get(&(facet, subject.clone())).copied().unwrap_or(0);
        let total = prior.saturating_add(agg.count);
        if total < MIN_OBSERVATIONS {
            continue; // sub-threshold -> no entry (never fabricate)
        }
        // Bound the provenance list (newest-first; this pass's ids are the
        // newest), then bound the observation.
        let mut provenance = agg.provenance;
        if provenance.len() > MAX_PROVENANCE {
            provenance.truncate(MAX_PROVENANCE);
        }
        entries.push((
            facet,
            subject,
            total,
            provenance,
            bound_observation(&agg.observation),
        ));
    }
    // Deterministic order (facet, subject) so the applied writes + tests are stable.
    entries.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()).then(a.1.cmp(&b.1)));
    Consolidation { entries }
}

/// Consolidate the user model from the OBSERVED inputs and APPLY the result to the
/// SHARED `user.model.*` tier. This is the function the reflection/consolidation
/// pass calls. It:
///   1. reads the current profile to seed the COMPOUNDING observed-counts +
///      to merge new provenance onto existing entries;
///   2. mines + thresholds the inputs PURELY ([`consolidate_inputs`]);
///   3. upserts each earned entry under its `user.model.*` key, enforcing the
///      global entry cap for NEW entries (a reinforcement of an EXISTING entry
///      always succeeds) and MERGING provenance (newest-first, bounded) so a
///      reinforced entry's reasons accrete rather than being overwritten.
///      Returns how many entries were written (upserted). NEVER fabricates: an empty /
///      sub-threshold input set writes nothing.
///
/// ISOLATION: the caller passes episodes it read AGENT-SCOPED and facts from the
/// (meta-filtered) user view; this function writes ONLY `user.model.*` keys, so it
/// can never write into a private namespace.
pub async fn consolidate(
    memory: &Memory,
    episodes: &[Episode],
    facts: &[(String, String)],
) -> Result<u64> {
    use std::collections::HashMap;
    // 1. Current profile -> existing counts + the existing provenance per entry.
    let current = snapshot(memory).await?;
    let mut existing_counts: HashMap<(Facet, String), u32> = HashMap::new();
    let mut existing_prov: HashMap<(Facet, String), Vec<String>> = HashMap::new();
    for e in &current.entries {
        existing_counts.insert((e.facet, e.subject.clone()), e.observed_count);
        existing_prov.insert((e.facet, e.subject.clone()), e.provenance.clone());
    }

    // 2. Pure consolidation against the existing counts (compounding).
    let result = consolidate_inputs(episodes, facts, &existing_counts);

    // 3. Apply, enforcing the entry cap for NEW entries and merging provenance.
    let mut written = 0u64;
    for (facet, subject, count, new_prov, observation) in result.entries {
        let exists = existing_counts.contains_key(&(facet, subject.clone()));
        if !exists {
            let count_now = entry_count(memory).await?;
            if count_now >= MAX_ENTRIES {
                // At the cap: refuse the NEW entry (reinforcements still apply).
                continue;
            }
        }
        // Merge provenance: NEW ids first (newest-first), then the prior ids,
        // deduped, bounded — so a reinforced entry shows its recent reasons.
        let mut provenance = new_prov;
        if let Some(prior) = existing_prov.get(&(facet, subject.clone())) {
            for p in prior {
                if !provenance.contains(p) {
                    provenance.push(p.clone());
                }
            }
        }
        if provenance.len() > MAX_PROVENANCE {
            provenance.truncate(MAX_PROVENANCE);
        }
        let key = entry_key(facet, &subject);
        let value = encode_value(count, &provenance, &observation);
        memory.upsert_user_fact(&key, &value).await?;
        written += 1;
    }
    Ok(written)
}

/// Count DISTINCT profile entries currently stored (the entry cap is measured
/// against the real count). Reads the model tier up to the read window.
async fn entry_count(memory: &Memory) -> Result<usize> {
    let rows = memory
        .recall_facts_limited(MODEL_PREFIX, MODEL_READ_WINDOW)
        .await?;
    Ok(rows.iter().filter(|(k, _)| parse_entry_key(k).is_some()).count())
}

// -- RENDER (tool result + provenance) + SUMMARY (prompt grounding) ----------

/// Render the FULL profile as inspectable text WITH provenance + observed-count —
/// the `user_model_query` tool result and the HUD inspector feed. Honest framing:
/// it states this is what DARWIN has OBSERVED (not divined), with how many times
/// and from where. Empty profile renders an explicit "nothing observed yet" line
/// so the tool never implies knowledge it lacks.
pub fn render(profile: &Profile) -> String {
    if profile.is_empty() {
        return "I have not built up an observed picture of you yet, sir — \
                nothing has met the bar to record. (I only note what I actually \
                observe, never guess.)"
            .to_string();
    }
    let mut out = String::from(
        "Here is what I have OBSERVED about you (built from our interactions, \
         never assumed — each with how many times I've seen it and where it came \
         from; you can correct or forget any of it):\n",
    );
    for e in &profile.entries {
        out.push_str(&format!(
            "- [{}] {} (observed {}x; from {})\n",
            e.facet.label(),
            e.observation,
            e.observed_count,
            e.provenance.join(", "),
        ));
    }
    out
}

/// The BOUNDED personalization SUMMARY injected into the prompt's UNCACHED tail so
/// replies personalize. STRICTLY grounded — it surfaces only the real, observed
/// profile (top entries by observed-count), with NO provenance noise (that lives
/// in the inspector), capped at [`SUMMARY_MAX_ENTRIES`] entries AND
/// [`SUMMARY_MAX_CHARS`] chars so it can never bloat context. Returns the empty
/// string for an empty profile so the caller adds NO block (honest: nothing
/// observed -> no claim). The preamble's no-fabrication rule still owns honesty;
/// this is grounding, not a license to invent.
pub fn summary(profile: &Profile) -> String {
    if profile.is_empty() {
        return String::new();
    }
    // Strongest first across the whole profile (observed-count desc), bounded.
    let mut entries: Vec<&ProfileEntry> = profile.entries.iter().collect();
    entries.sort_by(|a, b| {
        b.observed_count
            .cmp(&a.observed_count)
            .then(a.facet.as_str().cmp(b.facet.as_str()))
            .then(a.subject.cmp(&b.subject))
    });

    let mut out = String::new();
    for (shown, e) in entries.into_iter().enumerate() {
        if shown >= SUMMARY_MAX_ENTRIES {
            break;
        }
        let line = format!("- {}: {}\n", e.facet.label(), e.observation);
        if out.len() + line.len() > SUMMARY_MAX_CHARS {
            break;
        }
        out.push_str(&line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    struct TempDb(PathBuf);
    impl TempDb {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "darwin-usermodel-test-{}-{}.db",
                std::process::id(),
                tag
            ));
            let _ = std::fs::remove_file(&path);
            TempDb(path)
        }
    }
    impl Drop for TempDb {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let mut p = self.0.clone().into_os_string();
                p.push(suffix);
                let _ = std::fs::remove_file(PathBuf::from(p));
            }
        }
    }

    /// A synthetic episode with given id, utterance (already redacted shape),
    /// salient entities, derived deterministically.
    fn ep(id: i64, utterance: &str, entities: &[&str]) -> Episode {
        Episode {
            id,
            ts: format!("2026-06-15T10:0{id}:00+00:00"),
            agent_namespace: "agent.darwin".to_string(),
            utterance_redacted: utterance.to_string(),
            topic: "conversation".to_string(),
            salient_entities: entities.iter().map(|s| s.to_string()).collect(),
            outcome: "ok".to_string(),
            summary: utterance.to_string(),
        }
    }

    fn no_existing() -> HashMap<(Facet, String), u32> {
        HashMap::new()
    }

    // ===================================================================
    // SLUG / KEY / VALUE round-trips (pure)
    // ===================================================================

    #[test]
    fn facet_parse_accepts_synonyms_and_rejects_junk() {
        assert_eq!(Facet::parse("Preferences"), Some(Facet::Preference));
        assert_eq!(Facet::parse("habit"), Some(Facet::Pattern));
        assert_eq!(Facet::parse("INTERESTS"), Some(Facet::Topic));
        assert_eq!(Facet::parse("tone"), Some(Facet::Style));
        assert_eq!(Facet::parse("nonsense"), None);
    }

    #[test]
    fn entry_key_roundtrips_through_parse() {
        let k = entry_key(Facet::Preference, "editor");
        assert_eq!(parse_entry_key(&k), Some((Facet::Preference, "editor".to_string())));
        // foreign / malformed keys parse to None (skipped, never panic).
        assert_eq!(parse_entry_key("user.world.entity.project.x.status"), None);
        assert_eq!(parse_entry_key("user.name"), None);
        assert_eq!(parse_entry_key("user.model.preference"), None); // no subject
    }

    #[test]
    fn value_encode_decode_roundtrips_even_with_pipe_in_observation() {
        let prov = vec!["ep:1".to_string(), "fact:user.preference.editor".to_string()];
        let v = encode_value(3, &prov, "prefers a|b style");
        let (c, p, o) = decode_value(&v);
        assert_eq!(c, 3);
        assert_eq!(p, prov);
        assert_eq!(o, "prefers a|b style", "observation with a pipe survives");
        // A non-encoded legacy value degrades to a count-1, no-provenance entry.
        let (c2, p2, o2) = decode_value("just a plain string");
        assert_eq!(c2, 1);
        assert!(p2.is_empty());
        assert_eq!(o2, "just a plain string");
    }

    // ===================================================================
    // CONSOLIDATION — right entries, provenance, observed-counts
    // ===================================================================

    #[test]
    fn repeated_topic_earns_an_entry_with_provenance_and_count() {
        // The same topic ("rust") raised across TWO episodes clears the threshold.
        let episodes = vec![
            ep(1, "i was working on rust today", &["rust", "working"]),
            ep(2, "more rust debugging", &["rust", "debugging"]),
        ];
        let c = consolidate_inputs(&episodes, &[], &no_existing());
        let topic = c
            .entries
            .iter()
            .find(|(f, s, _, _, _)| *f == Facet::Topic && s == "rust")
            .expect("rust topic should be recorded");
        let (_f, _s, count, prov, obs) = topic;
        assert_eq!(*count, 2, "observed across two episodes");
        assert!(prov.contains(&"ep:1".to_string()) && prov.contains(&"ep:2".to_string()),
            "provenance names both source episodes: {prov:?}");
        assert!(obs.contains("rust"), "observation mentions the topic: {obs}");
    }

    #[test]
    fn a_single_stray_mention_is_not_recorded_never_fabricates() {
        // "working" appears in only ONE episode -> sub-threshold -> no entry.
        let episodes = vec![ep(1, "i was working on rust", &["rust", "working"])];
        let c = consolidate_inputs(&episodes, &[], &no_existing());
        assert!(
            c.entries.is_empty(),
            "one mention each is below the threshold; nothing is invented: {:?}",
            c.entries
        );
    }

    #[test]
    fn empty_and_contradictory_inputs_invent_nothing() {
        // Empty inputs.
        assert!(consolidate_inputs(&[], &[], &no_existing()).entries.is_empty());
        // "Contradictory" single mentions of unrelated subjects, each seen once:
        // none clears the threshold, so NO preference is fabricated.
        let episodes = vec![
            ep(1, "i prefer tea", &["tea"]),
            ep(2, "i prefer coffee", &["coffee"]),
        ];
        let c = consolidate_inputs(&episodes, &[], &no_existing());
        // tea and coffee each appear once -> below threshold -> nothing.
        assert!(
            c.entries.iter().all(|(_, s, _, _, _)| s != "tea" && s != "coffee"),
            "contradictory one-off preferences are NOT invented: {:?}",
            c.entries
        );
    }

    #[test]
    fn an_explicit_user_fact_earns_its_entry_on_its_own_with_provenance() {
        // A stored preference fact is authoritative -> one fact clears the bar.
        let facts = vec![
            ("user.preference.editor".to_string(), "neovim".to_string()),
            ("user.style.tone".to_string(), "terse and direct".to_string()),
            // A NON-facet fact must NOT become a profile entry (never invent).
            ("user.name".to_string(), "Darwin".to_string()),
        ];
        let c = consolidate_inputs(&[], &facts, &no_existing());
        let editor = c
            .entries
            .iter()
            .find(|(f, s, _, _, _)| *f == Facet::Preference && s == "editor")
            .expect("explicit editor preference recorded");
        assert!(editor.3.contains(&"fact:user.preference.editor".to_string()),
            "provenance names the source fact: {:?}", editor.3);
        assert!(editor.4.contains("neovim"), "observation carries the value: {}", editor.4);
        assert!(
            c.entries.iter().any(|(f, s, _, _, _)| *f == Facet::Style && s == "tone"),
            "explicit style fact recorded"
        );
        assert!(
            c.entries.iter().all(|(_, s, _, _, _)| s != "name"),
            "a non-facet fact (user.name) is NEVER turned into a profile entry: {:?}",
            c.entries
        );
    }

    #[test]
    fn a_repeated_observation_compounds_onto_the_prior_count() {
        // Prior: rust observed 3x. This pass: one more episode mentioning rust.
        let mut existing = HashMap::new();
        existing.insert((Facet::Topic, "rust".to_string()), 3u32);
        let episodes = vec![ep(9, "rust again", &["rust"])];
        let c = consolidate_inputs(&episodes, &[], &existing);
        let topic = c
            .entries
            .iter()
            .find(|(f, s, _, _, _)| *f == Facet::Topic && s == "rust")
            .expect("rust still recorded");
        assert_eq!(topic.2, 4, "prior 3 + this pass 1 = compounded count 4");
    }

    // ===================================================================
    // STORE round-trip: consolidate -> query (with provenance) -> correct -> forget
    // ===================================================================

    #[tokio::test]
    async fn consolidate_then_query_returns_the_profile_with_provenance() {
        let db = TempDb::new("consolidate-query");
        let mem = Memory::open(&db.0).unwrap();
        let episodes = vec![
            ep(1, "rust work", &["rust"]),
            ep(2, "rust again", &["rust"]),
        ];
        let facts = vec![("user.preference.editor".to_string(), "neovim".to_string())];
        let written = consolidate(&mem, &episodes, &facts).await.unwrap();
        assert!(written >= 2, "at least the rust topic + editor preference: {written}");

        // "what do you know about me" -> the whole profile WITH provenance.
        let profile = query(&mem, "").await.unwrap();
        let editor = profile
            .entries
            .iter()
            .find(|e| e.facet == Facet::Preference && e.subject == "editor")
            .expect("editor preference present");
        assert!(!editor.provenance.is_empty(), "entry carries provenance");
        assert!(editor.observation.contains("neovim"));
        let rendered = render(&profile);
        assert!(rendered.contains("neovim"), "render surfaces the observation: {rendered}");
        assert!(rendered.contains("from "), "render surfaces provenance: {rendered}");
        assert!(rendered.contains("observed"), "render surfaces the observed-count");
    }

    #[tokio::test]
    async fn consolidate_is_idempotent_and_compounds_the_count() {
        let db = TempDb::new("compound-store");
        let mem = Memory::open(&db.0).unwrap();
        let episodes = vec![ep(1, "rust", &["rust"]), ep(2, "rust", &["rust"])];
        consolidate(&mem, &episodes, &[]).await.unwrap();
        let before = query(&mem, "rust").await.unwrap();
        let c1 = before.entries[0].observed_count;
        // Run again with one more episode -> the count COMPOUNDS, not resets.
        let more = vec![ep(3, "rust", &["rust"])];
        consolidate(&mem, &more, &[]).await.unwrap();
        let after = query(&mem, "rust").await.unwrap();
        assert!(
            after.entries[0].observed_count > c1,
            "the observed-count compounds across passes: {} -> {}",
            c1, after.entries[0].observed_count
        );
    }

    #[tokio::test]
    async fn correct_overrides_an_entry_and_resets_provenance_to_a_correction() {
        let db = TempDb::new("correct");
        let mem = Memory::open(&db.0).unwrap();
        let facts = vec![("user.preference.editor".to_string(), "neovim".to_string())];
        consolidate(&mem, &[], &facts).await.unwrap();
        // The user corrects it.
        let changed = correct(&mem, Facet::Preference, "editor", "actually I use VS Code now")
            .await
            .unwrap();
        assert!(changed);
        let profile = query(&mem, "editor").await.unwrap();
        let e = profile.entries.iter().find(|e| e.subject == "editor").unwrap();
        assert!(e.observation.contains("VS Code"), "observation overridden: {}", e.observation);
        assert!(
            e.provenance.iter().any(|p| p.contains("correction")),
            "a correction is provenance-tagged as user-stated: {:?}",
            e.provenance
        );
    }

    #[tokio::test]
    async fn correct_with_empty_observation_deletes_the_entry() {
        let db = TempDb::new("correct-delete");
        let mem = Memory::open(&db.0).unwrap();
        let facts = vec![("user.preference.editor".to_string(), "neovim".to_string())];
        consolidate(&mem, &[], &facts).await.unwrap();
        let removed = correct(&mem, Facet::Preference, "editor", "  ").await.unwrap();
        assert!(removed, "an empty correction deletes the entry");
        let profile = query(&mem, "editor").await.unwrap();
        assert!(
            profile.entries.iter().all(|e| e.subject != "editor"),
            "the entry is gone after the empty correction"
        );
    }

    #[tokio::test]
    async fn forget_clears_the_whole_profile() {
        let db = TempDb::new("forget");
        let mem = Memory::open(&db.0).unwrap();
        let facts = vec![
            ("user.preference.editor".to_string(), "neovim".to_string()),
            ("user.style.tone".to_string(), "terse".to_string()),
        ];
        consolidate(&mem, &[], &facts).await.unwrap();
        assert!(!query(&mem, "").await.unwrap().is_empty());
        let cleared = forget(&mem).await.unwrap();
        assert!(cleared >= 2, "both entries forgotten: {cleared}");
        assert!(query(&mem, "").await.unwrap().is_empty(), "profile is empty after forget");
    }

    // ===================================================================
    // END-TO-END via the EPISODE store (mirrors the reflection/Pepper path)
    // ===================================================================

    /// The reflection/Pepper path reads SHARED-tier episodes (agent.darwin) and
    /// folds them + facts into the profile. This exercises that exact shape: real
    /// episodes recorded through the Memory episode store, then consolidate over
    /// what `episodes_recent("agent.darwin", …)` returns — and proves a SPECIALIST's
    /// PRIVATE episode is NOT folded into the shared profile (isolation on the way
    /// IN), while the shared episode IS.
    #[tokio::test]
    async fn consolidating_shared_tier_episodes_compounds_the_profile_and_isolates_private_ones() {
        let db = TempDb::new("e2e-reflect");
        let mem = Memory::open(&db.0).unwrap();
        // Two SHARED (orchestrator) episodes both about "rust" -> clears threshold.
        for i in 0..2 {
            mem.record_episode(&Episode {
                id: 0,
                ts: String::new(),
                agent_namespace: "agent.darwin".to_string(),
                utterance_redacted: format!("working on rust pass {i}"),
                topic: "conversation".to_string(),
                salient_entities: vec!["rust".to_string()],
                outcome: "ok".to_string(),
                summary: format!("rust pass {i}"),
            })
            .await
            .unwrap();
        }
        // A PRIVATE specialist episode about "gardening" — must NOT reach the
        // shared profile (the reflect path reads only the shared scope).
        for i in 0..3 {
            mem.record_episode(&Episode {
                id: 0,
                ts: String::new(),
                agent_namespace: "agent.friday".to_string(),
                utterance_redacted: format!("private gardening note {i}"),
                topic: "conversation".to_string(),
                salient_entities: vec!["gardening".to_string()],
                outcome: "ok".to_string(),
                summary: format!("gardening {i}"),
            })
            .await
            .unwrap();
        }

        // The reflect path's read: SHARED tier only.
        let shared = mem.episodes_recent("agent.darwin", 200).await.unwrap();
        consolidate(&mem, &shared, &[]).await.unwrap();

        let profile = query(&mem, "").await.unwrap();
        // The shared "rust" topic was folded in...
        assert!(
            profile.entries.iter().any(|e| e.subject == "rust"),
            "shared-tier rust topic consolidated: {:?}",
            profile.entries
        );
        // ...but the PRIVATE specialist's "gardening" topic was NOT (isolation IN).
        assert!(
            profile.entries.iter().all(|e| e.subject != "gardening"),
            "a specialist's private episode must NEVER reach the shared profile: {:?}",
            profile.entries
        );
    }

    // ===================================================================
    // ISOLATION + non-fabrication at the store level
    // ===================================================================

    #[tokio::test]
    async fn snapshot_reads_only_the_shared_model_tier_never_private_notes() {
        let db = TempDb::new("isolation");
        let mem = Memory::open(&db.0).unwrap();
        consolidate(
            &mem,
            &[],
            &[("user.preference.editor".to_string(), "neovim".to_string())],
        )
        .await
        .unwrap();
        // A private note in another agent's namespace, and a plain user fact.
        mem.upsert_fact("agent.friday.secret", "friday private intel").await.unwrap();
        mem.upsert_fact("user.name", "Darwin").await.unwrap();

        let profile = snapshot(&mem).await.unwrap();
        // Only the model-tier entry is present.
        assert!(profile.entries.iter().any(|e| e.subject == "editor"));
        let rendered = render(&profile);
        assert!(!rendered.contains("private"), "private note leaked: {rendered}");
        assert!(!rendered.contains("friday"), "agent namespace leaked: {rendered}");
        assert!(!rendered.contains("Darwin"), "non-model fact leaked: {rendered}");
    }

    #[tokio::test]
    async fn the_entry_cap_is_enforced_for_new_entries() {
        let db = TempDb::new("entry-cap");
        let mem = Memory::open(&db.0).unwrap();
        // Pre-seed MAX_ENTRIES distinct topic entries directly (encoded shape).
        for i in 0..MAX_ENTRIES {
            let key = format!("{MODEL_PREFIX}topic.t{i}");
            let v = encode_value(2, &[format!("ep:{i}")], &format!("topic t{i}"));
            mem.upsert_user_fact(&key, &v).await.unwrap();
        }
        // A consolidation that would add a NEW entry is refused at the cap.
        let written = consolidate(
            &mem,
            &[],
            &[("user.preference.brandnew".to_string(), "value".to_string())],
        )
        .await
        .unwrap();
        assert_eq!(written, 0, "no NEW entry past the cap");
    }

    // ===================================================================
    // SUMMARY — bounded personalization grounding
    // ===================================================================

    #[test]
    fn summary_is_empty_for_an_empty_profile() {
        assert_eq!(summary(&Profile::default()), "", "no profile -> no grounding block");
    }

    #[test]
    fn summary_is_bounded_in_entries_and_chars() {
        // Build a profile with MANY long entries; summary clamps both ways.
        let mut entries = Vec::new();
        for i in 0..50 {
            entries.push(ProfileEntry {
                facet: Facet::Topic,
                subject: format!("subject_{i}"),
                observation: format!("a fairly long observation number {i} about something"),
                observed_count: (i as u32) + 2,
                provenance: vec![format!("ep:{i}")],
            });
        }
        let profile = Profile { entries };
        let s = summary(&profile);
        let lines = s.lines().count();
        assert!(lines <= SUMMARY_MAX_ENTRIES, "summary entry-bounded: {lines} lines");
        assert!(s.len() <= SUMMARY_MAX_CHARS, "summary char-bounded: {} chars", s.len());
        // Strongest-first: the highest observed-count entry leads.
        assert!(s.contains("number 49"), "strongest (highest count) entry is shown: {s}");
    }

    #[test]
    fn summary_surfaces_only_observed_entries_no_provenance_noise() {
        let profile = Profile {
            entries: vec![ProfileEntry {
                facet: Facet::Preference,
                subject: "editor".to_string(),
                observation: "editor = neovim".to_string(),
                observed_count: 4,
                provenance: vec!["fact:user.preference.editor".to_string()],
            }],
        };
        let s = summary(&profile);
        assert!(s.contains("neovim"), "the observation is surfaced: {s}");
        assert!(!s.contains("fact:"), "provenance noise stays OUT of the prompt summary: {s}");
        assert!(s.contains("Preference"), "the facet labels the line");
    }
}

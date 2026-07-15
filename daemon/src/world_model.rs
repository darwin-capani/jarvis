//! THE WORLD MODEL — a shared, structured, live picture of the user's world that
//! ALL agents read and (the orchestrator + knowledge agents) update, so the
//! constellation reasons over ONE coherent model instead of flat isolated facts.
//!
//! WHAT IT IS. A thin STRUCTURED layer over the existing facts store
//! ([`crate::memory::Memory`]): ENTITIES (projects, people, deadlines, tasks,
//! topics, threads), the RELATIONSHIPS between them, and per-entity
//! attributes/state. There is no new table — every row is a fact, so the World
//! Model inherits the memory layer's WAL, retention, and (crucially) its
//! NAMESPACE ISOLATION semantics for free.
//!
//! WHERE IT LIVES (the load-bearing isolation decision). Everything is written
//! under the SHARED `user.world.*` tier. Because that prefix is NOT `agent.*`,
//! [`crate::memory::Memory::agent_scoped_facts`] already classifies it as SHARED
//! and hands it to EVERY agent. Conversely, an agent's PRIVATE `agent.<ns>.*`
//! notes are excluded from `agent_scoped_facts` for every OTHER agent BY
//! CONSTRUCTION, and this module NEVER reads or writes the `agent.*` space — so a
//! private note can never be folded into the shared model, and the world model can
//! never leak one agent's private notes into another agent's context. The
//! round-B/RAG isolation property holds unchanged.
//!
//! KEY SCHEME (stable, parseable, collision-resistant):
//!   - entity attribute:  `user.world.entity.<type>.<id>.<attribute>` = value
//!   - relationship:      `user.world.rel.<from_id>.<relation>.<to_id>` = value
//!     where `<type>` is one of the bounded [`EntityType`] kinds and `<id>` /
//!     `<from_id>` / `<to_id>` are SLUGS (lowercased, non-alphanumeric collapsed to
//!     `_`) so a name like "Project DARWIN" round-trips to a stable `project_darwin`.
//!     The human-readable display name is itself stored as the `name` attribute, so
//!     nothing is lost to slugging.
//!
//! BOUNDS. Every input is validated and clamped BEFORE it touches the store
//! (slug/attribute/value length, entity-type whitelist) and the store is bounded
//! globally (a hard cap on the number of distinct world entities + relations, so
//! a runaway writer cannot grow it without limit). Reads cap the rows pulled and
//! the structure returned.
//!
//! SAFETY. `world_update` writes SHARED USER-KNOWLEDGE — it is not a consequential
//! external action (it sends nothing, launches nothing, moves nothing), so it does
//! NOT go through `integrations::gate`. But it is still defended: it validates and
//! bounds every field, refuses reserved `meta.*` keys (via `upsert_user_fact`),
//! and — by only ever composing `user.world.*` keys — can NEVER write into another
//! agent's private `agent.<ns>.*` namespace.

use crate::memory::Memory;
use anyhow::{bail, Result};

/// The shared tier prefix. Anything under here is visible to EVERY agent via
/// `agent_scoped_facts` (it is not an `agent.*` key, so it is classified SHARED).
pub const WORLD_PREFIX: &str = "user.world.";
/// Entity-attribute key prefix: `user.world.entity.<type>.<id>.<attribute>`.
const ENTITY_PREFIX: &str = "user.world.entity.";
/// Relationship key prefix: `user.world.rel.<from_id>.<relation>.<to_id>`.
const REL_PREFIX: &str = "user.world.rel.";

// -- BOUNDS (all enforced before any write) ----------------------------------

/// Max chars in a slug (entity id / relation name segment) after slugging.
pub const MAX_SLUG_LEN: usize = 64;
/// Max chars in an attribute name.
pub const MAX_ATTR_LEN: usize = 48;
/// Max chars in a stored value (attribute value / relationship value).
pub const MAX_VALUE_LEN: usize = 1_024;
/// Hard cap on the number of DISTINCT entities the world model may hold. A write
/// that would introduce a NEW entity beyond this cap is refused (updates to an
/// existing entity always succeed, so the model never wedges).
pub const MAX_ENTITIES: usize = 512;
/// Hard cap on the number of DISTINCT relationships the world model may hold.
pub const MAX_RELATIONS: usize = 1_024;
/// The generous window of world-tier rows a single query/snapshot pulls from the
/// store before structuring — bounds the read so a large store can't blow memory.
pub const WORLD_READ_WINDOW: usize = 4_000;
/// Max entities returned in a single structured `query` result.
pub const MAX_QUERY_ENTITIES: usize = 24;
/// Max relationships returned in a single structured `query` result.
pub const MAX_QUERY_RELATIONS: usize = 48;

/// The bounded set of entity KINDS the world model recognizes. A free-form type
/// is rejected so the keyspace stays parseable and the model stays a coherent
/// schema rather than a junk drawer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityType {
    Project,
    Person,
    Deadline,
    Task,
    Topic,
    Thread,
}

impl EntityType {
    /// The stable lowercase token used in the key (`user.world.entity.<token>.…`)
    /// and accepted from the tool input.
    pub fn as_str(&self) -> &'static str {
        match self {
            EntityType::Project => "project",
            EntityType::Person => "person",
            EntityType::Deadline => "deadline",
            EntityType::Task => "task",
            EntityType::Topic => "topic",
            EntityType::Thread => "thread",
        }
    }

    /// Parse a caller-supplied type token, case-insensitively and trimmed. A few
    /// natural synonyms map to the canonical kind; anything else is `None` (the
    /// caller then rejects it with a helpful message listing the valid kinds).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "project" | "projects" => Some(EntityType::Project),
            "person" | "people" | "contact" => Some(EntityType::Person),
            "deadline" | "deadlines" | "due" => Some(EntityType::Deadline),
            "task" | "tasks" | "todo" | "to-do" => Some(EntityType::Task),
            "topic" | "topics" | "subject" => Some(EntityType::Topic),
            "thread" | "threads" | "conversation" => Some(EntityType::Thread),
            _ => None,
        }
    }

    /// All valid kinds, for error messages and tests.
    pub fn all() -> &'static [EntityType] {
        &[
            EntityType::Project,
            EntityType::Person,
            EntityType::Deadline,
            EntityType::Task,
            EntityType::Topic,
            EntityType::Thread,
        ]
    }

    /// Comma-joined list of valid kind tokens, for friendly error copy.
    pub fn valid_list() -> String {
        Self::all()
            .iter()
            .map(|t| t.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// A structured entity as returned by a query: its type, stable id (slug), the
/// human display name (the `name` attribute, falling back to the id), and its
/// other attributes as (name, value) pairs in stable (alphabetical) order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entity {
    pub entity_type: EntityType,
    pub id: String,
    pub name: String,
    /// Attributes EXCLUDING the synthetic `name` attribute (which is surfaced as
    /// `name`), sorted by attribute name for deterministic output.
    pub attributes: Vec<(String, String)>,
}

/// A structured relationship: the two endpoint ids (slugs), the relation token,
/// and the optional value/detail recorded on the edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relationship {
    pub from: String,
    pub relation: String,
    pub to: String,
    pub value: String,
}

/// The structured state the `world_query` tool returns: the entities relevant to
/// the query plus the relationships among/with them. Bounded by construction.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorldState {
    pub entities: Vec<Entity>,
    pub relationships: Vec<Relationship>,
}

impl WorldState {
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty() && self.relationships.is_empty()
    }
}

// -- slugging + validation ---------------------------------------------------

/// Normalize a free-form name to a stable, key-safe SLUG: lowercase, every run of
/// non-alphanumeric characters collapsed to a single `_`, leading/trailing `_`
/// trimmed, then clamped to [`MAX_SLUG_LEN`]. Deterministic. Returns `None` for
/// input that slugs to empty (all punctuation/whitespace) — the caller rejects it.
pub fn slugify(name: &str) -> Option<String> {
    let mut out = String::with_capacity(name.len());
    let mut prev_us = true; // start true so a leading separator is dropped
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
    if out.is_empty() {
        return None;
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

/// Validate + normalize an attribute name to a key-safe token, same slugging as
/// [`slugify`] but bounded to [`MAX_ATTR_LEN`]. `status`, `due date`, "Due Date"
/// all converge to a stable token.
fn slug_attr(attr: &str) -> Option<String> {
    let s = slugify(attr)?;
    if s.len() > MAX_ATTR_LEN {
        let mut s = s;
        s.truncate(MAX_ATTR_LEN);
        while s.ends_with('_') {
            s.pop();
        }
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    } else {
        Some(s)
    }
}

/// Bound + trim a stored VALUE. Empty (after trim) is rejected by callers; here we
/// only clamp the length so an oversized value can never bloat the store or a
/// prompt. Pure.
fn bound_value(value: &str) -> String {
    let v = value.trim();
    if v.len() > MAX_VALUE_LEN {
        // Truncate on a char boundary so we never split a multibyte sequence.
        let mut end = MAX_VALUE_LEN;
        while end > 0 && !v.is_char_boundary(end) {
            end -= 1;
        }
        v[..end].to_string()
    } else {
        v.to_string()
    }
}

// -- key construction + parsing ----------------------------------------------

/// Compose the fact key for an entity attribute. All parts are pre-slugged.
fn entity_key(etype: EntityType, id: &str, attr: &str) -> String {
    format!("{ENTITY_PREFIX}{}.{id}.{attr}", etype.as_str())
}

/// Compose the fact key for a relationship. All parts are pre-slugged.
fn rel_key(from: &str, relation: &str, to: &str) -> String {
    format!("{REL_PREFIX}{from}.{relation}.{to}")
}

/// Parse an entity-attribute key back into (type, id, attribute). Returns `None`
/// for any key that is not a well-formed `user.world.entity.<type>.<id>.<attr>`
/// (so a malformed or foreign row is simply skipped, never panics).
fn parse_entity_key(key: &str) -> Option<(EntityType, String, String)> {
    let rest = key.strip_prefix(ENTITY_PREFIX)?;
    // <type>.<id>.<attr> — type and attr are single tokens, id is a single slug
    // token too (slugs never contain '.'), so exactly three dot-parts.
    let mut parts = rest.splitn(3, '.');
    let type_tok = parts.next()?;
    let id = parts.next()?;
    let attr = parts.next()?;
    if id.is_empty() || attr.is_empty() || attr.contains('.') {
        return None;
    }
    let etype = EntityType::parse(type_tok)?;
    Some((etype, id.to_string(), attr.to_string()))
}

/// Parse a relationship key back into (from, relation, to). `None` for malformed.
fn parse_rel_key(key: &str) -> Option<(String, String, String)> {
    let rest = key.strip_prefix(REL_PREFIX)?;
    let mut parts = rest.splitn(3, '.');
    let from = parts.next()?;
    let relation = parts.next()?;
    let to = parts.next()?;
    if from.is_empty() || relation.is_empty() || to.is_empty() || to.contains('.') {
        return None;
    }
    Some((from.to_string(), relation.to_string(), to.to_string()))
}

// -- WRITE path --------------------------------------------------------------

/// Record a structured ATTRIBUTE on an entity into the SHARED world tier. This is
/// the write half of `world_update` for the attribute case. It validates and
/// bounds every field, enforces the global entity cap (a write introducing a NEW
/// entity beyond [`MAX_ENTITIES`] is refused; updating an existing one always
/// succeeds), and writes ONLY a `user.world.*` key — so it can never touch a
/// private `agent.<ns>.*` namespace or a reserved `meta.*` key.
///
/// Returns the canonical (type, id, attribute) actually written, so the caller can
/// echo back exactly what was recorded.
pub async fn set_attribute(
    memory: &Memory,
    etype: EntityType,
    name: &str,
    attribute: &str,
    value: &str,
) -> Result<(EntityType, String, String)> {
    let id = slugify(name).ok_or_else(|| {
        anyhow::anyhow!("entity name '{name}' has no usable characters (letters or digits)")
    })?;
    let attr = slug_attr(attribute).ok_or_else(|| {
        anyhow::anyhow!("attribute '{attribute}' has no usable characters")
    })?;
    let value = bound_value(value);
    if value.is_empty() {
        bail!("attribute value is empty");
    }
    // 'name' is the reserved display attribute; everything else is free.
    // Enforce the entity cap for a NEW entity only.
    let key = entity_key(etype, &id, &attr);
    let exists = entity_exists(memory, etype, &id).await?;
    if !exists {
        let count = entity_count(memory).await?;
        if count >= MAX_ENTITIES {
            bail!(
                "world model is at its entity cap ({MAX_ENTITIES}); cannot add a new entity"
            );
        }
        // A brand-new entity must always carry a display name so a query can show
        // it; if the caller is setting some OTHER attribute first, seed `name`
        // from the provided display name too (idempotent for the name case).
        if attr != "name" {
            let name_key = entity_key(etype, &id, "name");
            memory
                .upsert_user_fact(&name_key, &bound_value(name))
                .await?;
        }
    }
    memory.upsert_user_fact(&key, &value).await?;
    Ok((etype, id, attr))
}

/// Record a structured RELATIONSHIP into the SHARED world tier. The write half of
/// `world_update` for the relationship case. Same validation/bounds discipline as
/// [`set_attribute`]; enforces the global relation cap; writes only `user.world.*`.
///
/// `value` may be empty — a relationship can be a bare edge ("relates to") — in
/// which case a stable marker ("true") is stored so the edge exists and round-trips.
/// Returns the canonical (from_id, relation, to_id) written.
pub async fn set_relationship(
    memory: &Memory,
    from: &str,
    relation: &str,
    to: &str,
    value: &str,
) -> Result<(String, String, String)> {
    let from_id = slugify(from)
        .ok_or_else(|| anyhow::anyhow!("relationship 'from' name '{from}' has no usable characters"))?;
    let to_id = slugify(to)
        .ok_or_else(|| anyhow::anyhow!("relationship 'to' name '{to}' has no usable characters"))?;
    let rel = slug_attr(relation)
        .ok_or_else(|| anyhow::anyhow!("relation '{relation}' has no usable characters"))?;
    let stored = {
        let v = bound_value(value);
        if v.is_empty() {
            "true".to_string()
        } else {
            v
        }
    };
    let key = rel_key(&from_id, &rel, &to_id);
    let exists = memory.get_fact(&key).await?.is_some();
    if !exists {
        let count = relation_count(memory).await?;
        if count >= MAX_RELATIONS {
            bail!("world model is at its relationship cap ({MAX_RELATIONS}); cannot add a new relationship");
        }
    }
    memory.upsert_user_fact(&key, &stored).await?;
    Ok((from_id, rel, to_id))
}

/// True if any attribute row already exists for this entity (so a write to it is
/// an UPDATE, exempt from the new-entity cap). One attribute row is enough to
/// prove existence, so a tiny limit suffices.
async fn entity_exists(memory: &Memory, etype: EntityType, id: &str) -> Result<bool> {
    let prefix = format!("{ENTITY_PREFIX}{}.{id}.", etype.as_str());
    let rows = memory.recall_facts_limited(&prefix, 1).await?;
    Ok(!rows.is_empty())
}

/// Count DISTINCT entities currently in the world model (one per type+id), so the
/// new-entity cap is measured against the real entity count, not row count. Reads
/// the entity tier up to the read window (well above [`MAX_ENTITIES`]) so the cap
/// is measured accurately even when the store is full.
async fn entity_count(memory: &Memory) -> Result<usize> {
    let rows = memory
        .recall_facts_limited(ENTITY_PREFIX, WORLD_READ_WINDOW)
        .await?;
    let mut seen: Vec<(EntityType, String)> = Vec::new();
    for (key, _) in rows {
        if let Some((etype, id, _)) = parse_entity_key(&key) {
            if !seen.iter().any(|(t, i)| *t == etype && i == &id) {
                seen.push((etype, id));
            }
        }
    }
    Ok(seen.len())
}

/// Count DISTINCT relationships currently in the world model.
async fn relation_count(memory: &Memory) -> Result<usize> {
    let rows = memory
        .recall_facts_limited(REL_PREFIX, WORLD_READ_WINDOW)
        .await?;
    Ok(rows.iter().filter(|(k, _)| parse_rel_key(k).is_some()).count())
}

// -- READ path ---------------------------------------------------------------

/// Read the FULL structured world model from the SHARED tier: every entity (with
/// its attributes) and every relationship, bounded by [`WORLD_READ_WINDOW`]. This
/// is the structuring core that [`query`] filters. It reads ONLY `user.world.*`,
/// so it inherently cannot surface any agent's private notes.
pub async fn snapshot(memory: &Memory) -> Result<WorldState> {
    // Pull the whole shared world tier in one bounded prefix read. The limit is
    // the structuring window, so a large store is capped at the read source AND
    // again inside structure_rows.
    let rows = memory
        .recall_facts_limited(WORLD_PREFIX, WORLD_READ_WINDOW)
        .await?;
    Ok(structure_rows(rows))
}

/// A grouped entity under construction: its type, id, and the accumulated
/// (attribute, value) pairs, before it is folded into an [`Entity`].
type EntityGroup = (EntityType, String, Vec<(String, String)>);

/// Pure: fold raw (key,value) world-tier rows into a structured [`WorldState`].
/// Skips malformed/foreign rows. Sorted deterministically. Bounded to the read
/// window. Exposed for direct unit testing without a store.
pub fn structure_rows(rows: Vec<(String, String)>) -> WorldState {
    // Group entity attributes by (type, id).
    let mut entities: Vec<EntityGroup> = Vec::new();
    let mut relationships: Vec<Relationship> = Vec::new();

    for (key, value) in rows.into_iter().take(WORLD_READ_WINDOW) {
        if let Some((etype, id, attr)) = parse_entity_key(&key) {
            match entities
                .iter_mut()
                .find(|(t, i, _)| *t == etype && i == &id)
            {
                Some((_, _, attrs)) => attrs.push((attr, value)),
                None => entities.push((etype, id, vec![(attr, value)])),
            }
        } else if let Some((from, relation, to)) = parse_rel_key(&key) {
            relationships.push(Relationship {
                from,
                relation,
                to,
                value,
            });
        }
        // anything else (foreign / malformed) is silently skipped
    }

    // Finalize entities: split out the display name, sort attributes + entities.
    let mut out_entities: Vec<Entity> = entities
        .into_iter()
        .map(|(etype, id, mut attrs)| {
            attrs.sort_by(|a, b| a.0.cmp(&b.0));
            let name = attrs
                .iter()
                .find(|(a, _)| a == "name")
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| id.clone());
            let attributes: Vec<(String, String)> =
                attrs.into_iter().filter(|(a, _)| a != "name").collect();
            Entity {
                entity_type: etype,
                id,
                name,
                attributes,
            }
        })
        .collect();
    out_entities.sort_by(|a, b| {
        a.entity_type
            .as_str()
            .cmp(b.entity_type.as_str())
            .then(a.id.cmp(&b.id))
    });
    relationships.sort_by(|a, b| {
        a.from
            .cmp(&b.from)
            .then(a.relation.cmp(&b.relation))
            .then(a.to.cmp(&b.to))
    });

    WorldState {
        entities: out_entities,
        relationships,
    }
}

/// The structured state ABOUT a query: the entities whose id/name/attributes match
/// the query terms, plus the relationships touching those entities. This is the
/// read half of `world_query`. Bounded ([`MAX_QUERY_ENTITIES`] /
/// [`MAX_QUERY_RELATIONS`]) and read-only. Reads ONLY the shared tier.
///
/// Matching is lexical-token overlap (the same tokenization spirit as recall):
/// an entity matches when any query token appears in its id, name, or any
/// attribute value/name. An EMPTY query returns the whole (bounded) model — "tell
/// me about my world".
pub async fn query(memory: &Memory, about: &str) -> Result<WorldState> {
    let full = snapshot(memory).await?;
    Ok(filter_state(full, about))
}

/// Pure filter of a [`WorldState`] by the query terms. Exposed for direct testing.
pub fn filter_state(state: WorldState, about: &str) -> WorldState {
    let terms = query_terms(about);

    let matched: Vec<Entity> = if terms.is_empty() {
        state.entities.iter().take(MAX_QUERY_ENTITIES).cloned().collect()
    } else {
        state
            .entities
            .iter()
            .filter(|e| entity_matches(e, &terms))
            .take(MAX_QUERY_ENTITIES)
            .cloned()
            .collect()
    };

    // The set of entity ids we surfaced, so we can pull relationships touching them.
    let ids: Vec<&str> = matched.iter().map(|e| e.id.as_str()).collect();
    let relationships: Vec<Relationship> = if terms.is_empty() {
        state
            .relationships
            .iter()
            .take(MAX_QUERY_RELATIONS)
            .cloned()
            .collect()
    } else {
        state
            .relationships
            .iter()
            .filter(|r| {
                ids.iter().any(|id| *id == r.from || *id == r.to)
                    || terms.iter().any(|t| {
                        r.from.contains(t.as_str())
                            || r.to.contains(t.as_str())
                            || r.relation.contains(t.as_str())
                            || r.value.to_lowercase().contains(t.as_str())
                    })
            })
            .take(MAX_QUERY_RELATIONS)
            .cloned()
            .collect()
    };

    WorldState {
        entities: matched,
        relationships,
    }
}

/// True if any query term appears in the entity's id, display name, or any
/// attribute name/value (case-insensitive substring, since ids/attrs are slugs).
fn entity_matches(e: &Entity, terms: &[String]) -> bool {
    let name_l = e.name.to_lowercase();
    terms.iter().any(|t| {
        e.id.contains(t.as_str())
            || name_l.contains(t.as_str())
            || e.attributes.iter().any(|(a, v)| {
                a.contains(t.as_str()) || v.to_lowercase().contains(t.as_str())
            })
    })
}

/// Tokenize the query the same way the recall ranker does (lowercase, split on
/// non-alphanumeric, drop empties) — but keep it dependency-free and local so the
/// world model owns its own bounded matching. Short (1-char) tokens are dropped so
/// a stray letter doesn't match everything.
fn query_terms(about: &str) -> Vec<String> {
    about
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 1)
        .map(|t| t.to_lowercase())
        .collect()
}

// -- RENDER (for the tool result + the RAG/context block) --------------------

/// Render a [`WorldState`] as compact human/agent-readable text. Used both as the
/// `world_query` tool result and (via [`crate::anthropic`]) as the injected
/// world-context block. Returns an empty string for an empty state so callers can
/// omit the block entirely (honest: nothing known -> nothing shown). Bounded by
/// the state it is given (which is already capped).
pub fn render(state: &WorldState) -> String {
    if state.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    if !state.entities.is_empty() {
        out.push_str("Entities:\n");
        for e in &state.entities {
            out.push_str(&format!("- [{}] {}", e.entity_type.as_str(), e.name));
            if !e.attributes.is_empty() {
                let attrs: Vec<String> = e
                    .attributes
                    .iter()
                    .map(|(a, v)| format!("{a}={v}"))
                    .collect();
                out.push_str(&format!(" ({})", attrs.join(", ")));
            }
            out.push('\n');
        }
    }
    if !state.relationships.is_empty() {
        out.push_str("Relationships:\n");
        for r in &state.relationships {
            if r.value == "true" {
                out.push_str(&format!("- {} {} {}\n", r.from, r.relation, r.to));
            } else {
                out.push_str(&format!("- {} {} {} ({})\n", r.from, r.relation, r.to, r.value));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct TempDb(PathBuf);
    impl TempDb {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "darwin-world-test-{}-{}.db",
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

    // -- slugging + parsing (pure) -------------------------------------------

    #[test]
    fn slugify_normalizes_and_is_stable() {
        assert_eq!(slugify("Project DARWIN").as_deref(), Some("project_darwin"));
        assert_eq!(slugify("  Darwin  Capani  ").as_deref(), Some("darwin_capani"));
        assert_eq!(slugify("Q3-2026!!!").as_deref(), Some("q3_2026"));
        // round-trips: slugging a slug is a fixpoint
        let s = slugify("Project DARWIN").unwrap();
        assert_eq!(slugify(&s).as_deref(), Some(s.as_str()));
        // all-punctuation slugs to nothing
        assert_eq!(slugify("!!! --- ???"), None);
        assert_eq!(slugify(""), None);
    }

    #[test]
    fn slug_is_length_bounded() {
        let long = "a".repeat(MAX_SLUG_LEN + 50);
        let s = slugify(&long).unwrap();
        assert!(s.len() <= MAX_SLUG_LEN, "slug exceeded cap: {}", s.len());
    }

    #[test]
    fn entity_type_parse_accepts_synonyms_and_rejects_junk() {
        assert_eq!(EntityType::parse("Person"), Some(EntityType::Person));
        assert_eq!(EntityType::parse("people"), Some(EntityType::Person));
        assert_eq!(EntityType::parse("TODO"), Some(EntityType::Task));
        assert_eq!(EntityType::parse("nonsense"), None);
    }

    #[test]
    fn entity_and_rel_keys_roundtrip_through_parse() {
        let k = entity_key(EntityType::Project, "project_darwin", "status");
        assert_eq!(
            parse_entity_key(&k),
            Some((EntityType::Project, "project_darwin".to_string(), "status".to_string()))
        );
        let rk = rel_key("project_darwin", "owned_by", "darwin");
        assert_eq!(
            parse_rel_key(&rk),
            Some(("project_darwin".to_string(), "owned_by".to_string(), "darwin".to_string()))
        );
        // foreign / malformed keys parse to None (skipped, never panic)
        assert_eq!(parse_entity_key("user.name"), None);
        assert_eq!(parse_rel_key("user.world.entity.project.x.status"), None);
    }

    // -- store / query / update round-trips ----------------------------------

    #[tokio::test]
    async fn entity_attribute_roundtrip() {
        let db = TempDb::new("entity-roundtrip");
        let mem = Memory::open(&db.0).unwrap();

        set_attribute(&mem, EntityType::Project, "Project DARWIN", "status", "active")
            .await
            .unwrap();
        set_attribute(&mem, EntityType::Project, "Project DARWIN", "phase", "3")
            .await
            .unwrap();

        let state = query(&mem, "darwin").await.unwrap();
        assert_eq!(state.entities.len(), 1, "one entity: {state:?}");
        let e = &state.entities[0];
        assert_eq!(e.entity_type, EntityType::Project);
        assert_eq!(e.id, "project_darwin");
        assert_eq!(e.name, "Project DARWIN", "display name preserved verbatim");
        assert_eq!(
            e.attributes,
            vec![
                ("phase".to_string(), "3".to_string()),
                ("status".to_string(), "active".to_string())
            ],
            "attributes sorted, name excluded"
        );
    }

    #[tokio::test]
    async fn update_overwrites_attribute_in_place() {
        let db = TempDb::new("update-inplace");
        let mem = Memory::open(&db.0).unwrap();
        set_attribute(&mem, EntityType::Task, "ship world model", "status", "in_progress")
            .await
            .unwrap();
        set_attribute(&mem, EntityType::Task, "ship world model", "status", "done")
            .await
            .unwrap();
        let state = query(&mem, "world").await.unwrap();
        let e = &state.entities[0];
        assert_eq!(e.attributes, vec![("status".to_string(), "done".to_string())]);
    }

    #[tokio::test]
    async fn relationship_roundtrip_and_links_entities_in_query() {
        let db = TempDb::new("rel-roundtrip");
        let mem = Memory::open(&db.0).unwrap();
        set_attribute(&mem, EntityType::Project, "DARWIN", "status", "active")
            .await
            .unwrap();
        set_attribute(&mem, EntityType::Person, "Darwin", "role", "owner")
            .await
            .unwrap();
        set_relationship(&mem, "DARWIN", "owned by", "Darwin", "since 2026")
            .await
            .unwrap();

        // Querying the project surfaces the relationship that touches it.
        let state = query(&mem, "darwin").await.unwrap();
        assert_eq!(state.relationships.len(), 1);
        let r = &state.relationships[0];
        assert_eq!(r.from, "darwin");
        assert_eq!(r.relation, "owned_by");
        assert_eq!(r.to, "darwin");
        assert_eq!(r.value, "since 2026");
    }

    #[tokio::test]
    async fn empty_query_returns_whole_bounded_model() {
        let db = TempDb::new("empty-query");
        let mem = Memory::open(&db.0).unwrap();
        set_attribute(&mem, EntityType::Topic, "rust", "interest", "high")
            .await
            .unwrap();
        set_attribute(&mem, EntityType::Person, "alice", "team", "eng")
            .await
            .unwrap();
        let state = query(&mem, "").await.unwrap();
        assert_eq!(state.entities.len(), 2, "empty query returns everything");
    }

    #[tokio::test]
    async fn query_returns_nothing_for_unknown_topic_never_fabricates() {
        let db = TempDb::new("no-match");
        let mem = Memory::open(&db.0).unwrap();
        set_attribute(&mem, EntityType::Project, "darwin", "status", "active")
            .await
            .unwrap();
        let state = query(&mem, "quantum chromodynamics").await.unwrap();
        assert!(state.is_empty(), "no match -> empty state, got {state:?}");
        assert_eq!(render(&state), "", "empty state renders nothing");
    }

    // -- BOUNDS --------------------------------------------------------------

    #[tokio::test]
    async fn value_is_length_bounded_on_write() {
        let db = TempDb::new("value-bound");
        let mem = Memory::open(&db.0).unwrap();
        let huge = "x".repeat(MAX_VALUE_LEN + 500);
        set_attribute(&mem, EntityType::Topic, "big", "note", &huge)
            .await
            .unwrap();
        let state = query(&mem, "big").await.unwrap();
        let (_, v) = &state.entities[0].attributes[0];
        assert!(v.len() <= MAX_VALUE_LEN, "value not bounded: {}", v.len());
    }

    #[tokio::test]
    async fn empty_value_and_unusable_name_are_rejected() {
        let db = TempDb::new("reject");
        let mem = Memory::open(&db.0).unwrap();
        assert!(set_attribute(&mem, EntityType::Topic, "x", "note", "   ")
            .await
            .is_err());
        assert!(set_attribute(&mem, EntityType::Topic, "!!!", "note", "v")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn new_entity_cap_is_enforced_but_updates_still_succeed() {
        let db = TempDb::new("entity-cap");
        let mem = Memory::open(&db.0).unwrap();
        // Pre-seed MAX_ENTITIES distinct topics directly (fast path).
        for i in 0..MAX_ENTITIES {
            let key = format!("{ENTITY_PREFIX}topic.t{i}.name");
            mem.upsert_user_fact(&key, &format!("t{i}")).await.unwrap();
        }
        // A NEW entity is refused.
        let err = set_attribute(&mem, EntityType::Topic, "overflow", "note", "v")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cap"), "wrong error: {err}");
        // But UPDATING an existing entity still works.
        set_attribute(&mem, EntityType::Topic, "t0", "note", "still ok")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn relationship_cap_is_enforced() {
        let db = TempDb::new("rel-cap");
        let mem = Memory::open(&db.0).unwrap();
        for i in 0..MAX_RELATIONS {
            let key = format!("{REL_PREFIX}a{i}.rel.b{i}");
            mem.upsert_user_fact(&key, "true").await.unwrap();
        }
        let err = set_relationship(&mem, "x", "rel", "y", "")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cap"), "wrong error: {err}");
    }

    // -- ISOLATION: shared tier visible to all, private notes never folded in --

    #[tokio::test]
    async fn world_model_only_reads_shared_tier_never_private_notes() {
        let db = TempDb::new("isolation");
        let mem = Memory::open(&db.0).unwrap();
        // A real world entity in the shared tier.
        set_attribute(&mem, EntityType::Project, "Project DARWIN", "status", "active")
            .await
            .unwrap();
        // A PRIVATE note in another agent's namespace, AND a private note that
        // even MENTIONS the same topic word — neither may ever appear in the model.
        mem.upsert_fact("agent.friday.secret", "friday private intel about darwin")
            .await
            .unwrap();
        mem.upsert_fact("agent.pepper.note", "pepper private reminder")
            .await
            .unwrap();
        // Also a non-world shared fact must not be mistaken for a world entity.
        mem.upsert_fact("user.name", "Darwin").await.unwrap();

        let state = snapshot(&mem).await.unwrap();
        // Only the world entity is present; no private rows, no plain user.* facts.
        assert_eq!(state.entities.len(), 1);
        assert_eq!(state.entities[0].id, "project_darwin");
        let rendered = render(&state);
        assert!(!rendered.contains("private"), "private note leaked: {rendered}");
        assert!(!rendered.contains("friday"), "agent namespace leaked: {rendered}");
        assert!(!rendered.contains("Darwin"), "non-world fact leaked: {rendered}");

        // Even a query whose term matches the private note's text returns nothing
        // from the private space (the model never reads agent.* at all).
        let q = query(&mem, "intel reminder").await.unwrap();
        assert!(
            !render(&q).contains("private"),
            "private content surfaced via query: {q:?}"
        );
    }

    #[tokio::test]
    async fn structure_rows_skips_foreign_and_malformed() {
        // Pure structuring: only well-formed world rows survive.
        let rows = vec![
            ("user.world.entity.project.darwin.status".to_string(), "active".to_string()),
            ("user.world.entity.project.darwin.name".to_string(), "DARWIN".to_string()),
            ("user.world.rel.darwin.owned_by.darwin".to_string(), "true".to_string()),
            // foreign rows that happen to share the prefix family but aren't valid
            ("user.world.garbage".to_string(), "x".to_string()),
            ("user.name".to_string(), "Darwin".to_string()),
            ("agent.friday.note".to_string(), "private".to_string()),
        ];
        let state = structure_rows(rows);
        assert_eq!(state.entities.len(), 1);
        assert_eq!(state.entities[0].name, "DARWIN");
        assert_eq!(state.relationships.len(), 1);
        let r = render(&state);
        assert!(!r.contains("private"));
        assert!(!r.contains("garbage"));
    }
}

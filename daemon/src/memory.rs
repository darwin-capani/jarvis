use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use tokio::sync::Mutex;

/// True for internal bookkeeping keys ("meta." prefix: meta.last_reflection,
/// meta.heal_pending, meta.heal_last_attempt, meta.last_interaction, ...)
/// that model-driven writes must never touch. Trimmed and ASCII
/// case-insensitive, matching SQLite LIKE's case-insensitivity, so a model
/// cannot smuggle "Meta.last_reflection" past the guard while the prompt
/// filter (`NOT LIKE 'meta.%'`) would still hide the poisoned row.
pub fn is_reserved_key(key: &str) -> bool {
    let k = key.trim().as_bytes();
    k.len() >= 5 && k[..5].eq_ignore_ascii_case(b"meta.")
}

/// rusqlite::Connection is Send but not Sync; the async Mutex serializes
/// access so &Memory can be shared across tasks. Statements here are short
/// enough that holding the lock across them is fine for Phase 1.
pub struct Memory {
    conn: Mutex<Connection>,
}

/// One EPISODE: a single, durable, redacted record of a completed interaction —
/// the unit the episodic store remembers and recalls. Built ONLY from OBSERVED
/// interactions (never fabricated), REDACTED before store (the utterance and the
/// derived fields never carry PII/secrets), AGENT-SCOPED (`agent_namespace` keeps
/// one agent's episodes out of another agent's recall, mirroring
/// `agent_scoped_facts`), and BOUNDED (evict-oldest past the cap in
/// `retention_pass`).
///
/// HONESTY: every field is derived from what was actually said/done this turn —
/// `topic`/`salient_entities`/`outcome`/`summary` are extracted from the real
/// utterance + routing, not invented; recall returns only episodes that were
/// really recorded. The store remembers the recent, bounded past — NOT
/// "everything forever".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Episode {
    /// Stable row id (0 before insert; set by SQLite on store).
    pub id: i64,
    /// RFC3339 timestamp — the turn's completion time, the temporal sort key.
    pub ts: String,
    /// The agent namespace that handled the turn ("agent.<name>"). The recall
    /// scope key: an episode recorded under one agent stays in that agent's
    /// recall (plus the shared orchestrator), never another specialist's.
    pub agent_namespace: String,
    /// The user's utterance with all PII/secret spans stripped (via
    /// optimize::redact) BEFORE store — the ONLY form of the utterance the table
    /// ever holds.
    pub utterance_redacted: String,
    /// The intent/topic the router inferred (e.g. "conversation", "memory.recall").
    pub topic: String,
    /// A few salient entities derived from the (redacted) utterance — short
    /// content words that anchor what the episode was about. Comma-joined in the
    /// row; bounded to a handful.
    pub salient_entities: Vec<String>,
    /// How the turn went, as a short token ("ok" by default; "abandoned"/"failed"
    /// when the turn carried no usable response — those are NOT recorded here, so
    /// in practice this is the completed-turn outcome).
    pub outcome: String,
    /// A short redacted summary of the turn (utterance shape -> response gist),
    /// the human-readable line a timeline/recall surface shows.
    pub summary: String,
}

/// One CITED source backing a notebook entry: the source id the synthesis cited
/// (1-based, the id within its run), its title, and the real fetched URL. A
/// `NotebookCitation` exists ONLY because the run actually fetched and cited it —
/// it mirrors a [`crate::research::Source`] that a grounded claim referenced, so
/// the cite-discipline of research.rs carries through to the persisted notebook:
/// a notebook NEVER holds a citation that was not in its run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotebookCitation {
    /// The 1-based source id within the run (what a grounded claim cited).
    pub source_id: i64,
    /// The cited source's title.
    pub title: String,
    /// The real fetched URL.
    pub url: String,
}

/// One persisted SAGE research run, saved as a CITED notebook entry. A notebook
/// is the set of entries sharing a `topic_key`; an APPEND adds another entry
/// under the same key, so a notebook accrues source memory over time. Built ONLY
/// from a real research run — `synthesized` is the rendered cited answer and
/// `citations` are the run's real fetched sources, never invented (see
/// [`crate::notebook`]). REDACTED (the synthesized text is re-redacted at store),
/// AGENT-SCOPED (`agent_namespace`), and BOUNDED (evict-oldest in
/// `notebook_retention_pass`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotebookEntry {
    /// Stable row id (0 before insert; set by SQLite on store).
    pub id: i64,
    /// RFC3339 timestamp — when the run was saved.
    pub ts: String,
    /// The agent namespace that ran the research ("agent.<name>"). The scope key.
    pub agent_namespace: String,
    /// The NORMALIZED topic key (lowercased/trimmed) used to revisit + append —
    /// two runs on "the James Webb telescope" and "James Webb Telescope" land in
    /// the same notebook.
    pub topic_key: String,
    /// The human-readable topic as the user phrased it.
    pub topic: String,
    /// The rendered, CITED synthesis text (redacted before store).
    pub synthesized: String,
    /// The run's real fetched citations — the bibliography. Empty only when the
    /// run produced no grounded sources (an honest empty run is still recorded so
    /// the notebook reflects what actually happened).
    pub citations: Vec<NotebookCitation>,
}

impl Memory {
    /// Open the main Db PLAINTEXT (today's behavior, byte-for-byte). Reached when
    /// `[security].encrypt_memory` is OFF — the shipped default — so no `PRAGMA
    /// key` is ever applied and the on-disk file is an ordinary SQLite database.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::init_conn(conn)
    }

    /// Open the main Db ENCRYPTED (transparent whole-file SQLCipher AES-256).
    /// The `key` is applied via `PRAGMA key` IMMEDIATELY after `Connection::open`
    /// and BEFORE any other pragma/statement (SQLCipher requires the key before
    /// the first header read). Reached only when `[security].encrypt_memory` is
    /// ON; tests pass an explicit in-test key (the injectable seam — no Keychain).
    pub fn open_encrypted(path: &Path, key: &crate::crypto::SecretKey) -> Result<Self> {
        let conn = Connection::open(path)?;
        crate::crypto::apply_key(&conn, key)?;
        Self::init_conn(conn)
    }

    /// Shared connection setup (pragmas + schema). Runs AFTER any `PRAGMA key`
    /// so the keyed and plaintext paths build the identical schema.
    fn init_conn(conn: Connection) -> Result<Self> {
        // External readers (sqlite3 CLI, init_memory.py, the HUD) must not
        // make daemon statements fail instantly with SQLITE_BUSY: wait
        // briefly instead, and use WAL so readers don't block the writer.
        conn.busy_timeout(Duration::from_millis(250))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS events(
                id INTEGER PRIMARY KEY,
                ts TEXT NOT NULL,
                source TEXT NOT NULL,
                kind TEXT NOT NULL,
                payload TEXT
            );
            CREATE TABLE IF NOT EXISTS facts(
                id INTEGER PRIMARY KEY,
                ts TEXT NOT NULL,
                key TEXT NOT NULL,
                value TEXT NOT NULL,
                confidence REAL DEFAULT 1.0
            );
            -- PERF: index the fact KEY. `facts` is one shared, never-pruned store
            -- (world/model/agent tiers + every remembered fact), so without this every
            -- exact-key access is a full-table scan. The hottest path is WRITE
            -- amplification: upsert_fact/get_fact/delete_fact are `WHERE key = ?`, and
            -- the world-model + reflection loops upsert many rows per turn — each was a
            -- full scan, now an index lookup (EXPLAIN QUERY PLAN: SEARCH ... USING INDEX
            -- idx_facts_key (key=?)). Purely additive: results/ordering are unchanged.
            -- (The parameterized `key LIKE ?||'%'` prefix reads still scan — SQLite's
            -- LIKE optimization doesn't apply to a bound pattern — that rewrite is a
            -- separate, behavior-touching change and is left as a follow-up.)
            CREATE INDEX IF NOT EXISTS idx_facts_key ON facts(key);
            CREATE TABLE IF NOT EXISTS transcripts(
                id INTEGER PRIMARY KEY,
                ts TEXT NOT NULL,
                wav_path TEXT,
                text TEXT NOT NULL,
                intent TEXT,
                routed_to TEXT,
                response TEXT
            );
            CREATE TABLE IF NOT EXISTS episodes(
                id INTEGER PRIMARY KEY,
                ts TEXT NOT NULL,
                agent_namespace TEXT NOT NULL,
                utterance_redacted TEXT NOT NULL,
                topic TEXT NOT NULL,
                salient_entities TEXT NOT NULL,
                outcome TEXT NOT NULL,
                summary TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_episodes_ns_ts
                ON episodes(agent_namespace, ts);
            -- RESEARCH NOTEBOOKS (notebook.rs): a persisted SAGE research run.
            -- One row per run (an APPEND adds another row under the same
            -- topic_key); its citations live in notebook_citations, one row per
            -- cited source. agent_namespace keeps a notebook in its agent's
            -- scope; topic_key is the normalized topic used to revisit/append.
            CREATE TABLE IF NOT EXISTS notebook_entries(
                id INTEGER PRIMARY KEY,
                ts TEXT NOT NULL,
                agent_namespace TEXT NOT NULL,
                topic_key TEXT NOT NULL,
                topic TEXT NOT NULL,
                synthesized TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_notebook_ns_topic
                ON notebook_entries(agent_namespace, topic_key, id);
            -- One CITED source for a notebook entry. The cite-discipline anchor:
            -- a citation row exists ONLY because the run actually fetched it, so
            -- a notebook can never hold a citation that was not in its run.
            CREATE TABLE IF NOT EXISTS notebook_citations(
                id INTEGER PRIMARY KEY,
                entry_id INTEGER NOT NULL,
                source_id INTEGER NOT NULL,
                title TEXT NOT NULL,
                url TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_notebook_cit_entry
                ON notebook_citations(entry_id);",
        )?;
        // Idempotent migration for databases created before the learning
        // loop: add transcripts.response, ignoring "duplicate column name"
        // (the CREATE TABLE above already includes it on fresh installs).
        if let Err(e) = conn.execute("ALTER TABLE transcripts ADD COLUMN response TEXT", []) {
            if !e.to_string().contains("duplicate column name") {
                return Err(e.into());
            }
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub async fn record_event(&self, source: &str, kind: &str, payload: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO events(ts, source, kind, payload) VALUES (?1, ?2, ?3, ?4)",
            params![Utc::now().to_rfc3339(), source, kind, payload],
        )?;
        Ok(())
    }

    /// Update the fact stored under `key` if it exists, otherwise insert it.
    /// This is what the learning loop calls, so a fact like user.name
    /// converges to its latest value instead of accumulating duplicates.
    pub async fn upsert_fact(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        let updated = conn.execute(
            "UPDATE facts SET value = ?2, ts = ?3, confidence = 1.0 WHERE key = ?1",
            params![key, value, Utc::now().to_rfc3339()],
        )?;
        if updated == 0 {
            conn.execute(
                "INSERT INTO facts(ts, key, value, confidence) VALUES (?1, ?2, ?3, 1.0)",
                params![Utc::now().to_rfc3339(), key, value],
            )?;
        }
        Ok(())
    }

    /// The write path for every MODEL-DRIVEN fact (cloud remember_fact tool,
    /// extract_facts learning loop, reflection upserts): identical to
    /// upsert_fact except that reserved "meta." keys are rejected, so a model
    /// output can never overwrite internal bookkeeping rows in place
    /// (audit fix: a forged meta.last_reflection silently disabled
    /// consolidation, invisibly — meta.* rows are filtered from every prompt
    /// feed and from recall display). Trusted internal writes keep using
    /// upsert_fact directly.
    pub async fn upsert_user_fact(&self, key: &str, value: &str) -> Result<()> {
        if is_reserved_key(key) {
            anyhow::bail!("meta.* keys are reserved for internal bookkeeping");
        }
        self.upsert_fact(key, value).await
    }

    /// Remove the fact stored under `key`. Returns whether a row existed —
    /// the reflection loop logs deletes of already-gone keys as no-ops.
    pub async fn delete_fact(&self, key: &str) -> Result<bool> {
        let conn = self.conn.lock().await;
        let deleted = conn.execute("DELETE FROM facts WHERE key = ?1", params![key])?;
        Ok(deleted > 0)
    }

    /// The value stored under exactly `key`, if any — used for internal
    /// bookkeeping keys like meta.last_reflection.
    pub async fn get_fact(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare("SELECT value FROM facts WHERE key = ?1 LIMIT 1")?;
        let mut rows = stmt.query_map(params![key], |row| row.get::<_, String>(0))?;
        Ok(rows.next().transpose()?)
    }

    pub async fn recall_facts(&self, key_prefix: &str) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT key, value FROM facts WHERE key LIKE ?1 || '%' ORDER BY ts DESC LIMIT 50",
        )?;
        let rows = stmt
            .query_map(params![key_prefix], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every fact whose key starts with `key_prefix`, newest first, capped at
    /// `limit` rows. Unlike [`Self::recall_facts`] (a fixed LIMIT 50 display
    /// window) this lets a structured reader pull a LARGER, caller-bounded slice
    /// of one prefix family — the World Model uses it to read the whole shared
    /// `user.world.*` tier (bounded by its own read window + entity/relation caps)
    /// rather than being silently truncated at 50. Like recall_facts it is a plain
    /// prefix scan over the SAME `facts` table, so it inherits namespace semantics:
    /// callers pass a `user.world.*` prefix, which is a SHARED key family — it
    /// never reaches any agent's private `agent.<ns>.*` rows.
    pub async fn recall_facts_limited(
        &self,
        key_prefix: &str,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT key, value FROM facts WHERE key LIKE ?1 || '%'
             ORDER BY ts DESC, id DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![key_prefix, limit as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The most recently touched facts, newest first, INCLUDING internal
    /// "meta." bookkeeping keys. Prompt feeds must use all_user_facts
    /// instead; this unfiltered view is kept for tests and inspection.
    #[allow(dead_code)] // exercised by unit tests; prompt feeds use all_user_facts
    pub async fn all_facts(&self, limit: usize) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().await;
        let mut stmt =
            conn.prepare("SELECT key, value FROM facts ORDER BY ts DESC, id DESC LIMIT ?1")?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// all_facts minus internal bookkeeping: keys starting "meta." (e.g.
    /// meta.last_reflection) never reach a prompt. This is the UNSCOPED view
    /// (every user-visible fact across all agent namespaces); it is for the
    /// system-internal reflection/consolidation pass ONLY. Per-agent PROMPT and
    /// RECALL feeds — generate/converse/cloud, the recall_facts + mnemosyne_recall
    /// tools, and the memory.recall intent — must use [`Self::agent_scoped_facts`]
    /// instead, so one agent never sees another's private agent.<other>.*
    /// namespace (constellation isolation).
    pub async fn all_user_facts(&self, limit: usize) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT key, value FROM facts WHERE key NOT LIKE 'meta.%'
             ORDER BY ts DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The SYNCABLE facts as (key, value, ts) — every non-`meta.*` fact with its
    /// last-touch RFC3339 timestamp, the newest-wins ordering key the F18
    /// federated-sync merge needs (`all_user_facts` drops ts). Bounded, ordered
    /// by key for a stable bundle. `meta.*` bookkeeping is excluded (it must
    /// never sync to a peer). READ-ONLY.
    pub async fn syncable_facts(&self, limit: usize) -> Result<Vec<(String, String, String)>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT key, value, ts FROM facts WHERE key NOT LIKE 'meta.%'
             ORDER BY key ASC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Facts visible to one agent: its OWN namespace ("agent.<name>." prefix)
    /// plus SHARED facts (no "agent." prefix at all), newest first, internal
    /// "meta." bookkeeping always excluded. This is what the active agent's
    /// converse/cloud reply is fed — an agent sees what it learned and what is
    /// common knowledge, never another agent's private namespace
    /// (constellation isolation at the recall layer). `namespace` is the
    /// agent's full namespace string ("agent.friday"); the per-fact prefix is
    /// that plus a dot.
    pub async fn agent_scoped_facts(
        &self,
        namespace: &str,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        let own_prefix = format!("{namespace}.");
        let conn = self.conn.lock().await;
        // Own-namespace rows (LIKE 'agent.friday.%') OR shared rows (NOT LIKE
        // 'agent.%'); meta.* is filtered on both sides. Other agents'
        // namespaces (agent.<other>.*) are excluded by construction.
        let mut stmt = conn.prepare(
            "SELECT key, value FROM facts
             WHERE key NOT LIKE 'meta.%'
               AND (key LIKE ?1 || '%' OR key NOT LIKE 'agent.%')
             ORDER BY ts DESC, id DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![own_prefix, limit as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// How many user-visible facts (internal "meta." bookkeeping excluded)
    /// were created or updated after `ts_rfc3339`. Feeds the proactive
    /// first-contact brief ("N new facts learned since we last spoke") —
    /// fact rows carry their last-touch timestamp, so an upsert counts as
    /// learned, which is exactly the brief's meaning of "new".
    pub async fn facts_learned_since(&self, ts_rfc3339: &str) -> Result<u64> {
        let conn = self.conn.lock().await;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM facts WHERE ts > ?1 AND key NOT LIKE 'meta.%'",
            params![ts_rfc3339],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as u64)
    }

    /// Retention pass (audit fix: events and transcripts grew without bound
    /// on the always-on appliance — record_event fires on every utterance
    /// and local intent, record_transcript on every completed turn, and no
    /// DELETE existed for either). Removes events older than
    /// `events_max_age_days`, caps transcripts to the newest
    /// `transcripts_keep` rows, caps EPISODES to the newest `episodes_keep`
    /// rows (evict-oldest, the bounded-memory contract — the store remembers
    /// the recent past, never "everything forever"), and VACUUMs when anything
    /// was removed so the file actually shrinks. Facts are NOT touched: they
    /// are the consolidated memory, bounded by the reflection pass. Returns
    /// (events_deleted, transcripts_deleted, episodes_deleted).
    pub async fn retention_pass(
        &self,
        events_max_age_days: i64,
        transcripts_keep: usize,
        episodes_keep: usize,
    ) -> Result<(u64, u64, u64)> {
        let conn = self.conn.lock().await;
        let cutoff = (Utc::now() - chrono::Duration::days(events_max_age_days)).to_rfc3339();
        let events_deleted = conn.execute("DELETE FROM events WHERE ts < ?1", params![cutoff])?;
        let transcripts_deleted = conn.execute(
            "DELETE FROM transcripts WHERE id NOT IN
             (SELECT id FROM transcripts ORDER BY id DESC LIMIT ?1)",
            params![transcripts_keep as i64],
        )?;
        // Episodes cap is GLOBAL (newest `episodes_keep` rows by id, the
        // monotonic insert order), not per-agent: the bound is on the on-disk
        // store as a whole, the same shape as the transcripts cap. Recall is
        // still agent-scoped at read time, so a global cap never lets one agent
        // read another's surviving rows.
        let episodes_deleted = conn.execute(
            "DELETE FROM episodes WHERE id NOT IN
             (SELECT id FROM episodes ORDER BY id DESC LIMIT ?1)",
            params![episodes_keep as i64],
        )?;
        if events_deleted + transcripts_deleted + episodes_deleted > 0 {
            // Autocommit context, so VACUUM is legal here; the tables were
            // just capped, so the rewrite is cheap at retention cadence.
            conn.execute_batch("VACUUM")?;
        }
        Ok((
            events_deleted as u64,
            transcripts_deleted as u64,
            episodes_deleted as u64,
        ))
    }

    /// The last `n` (user utterance, JARVIS response) exchanges, oldest
    /// first, ready to drop into a chat history. Transcripts without a
    /// recorded response (older rows, failed turns) are skipped.
    pub async fn recent_exchanges(&self, n: usize) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT text, response FROM transcripts
             WHERE response IS NOT NULL ORDER BY id DESC LIMIT ?1",
        )?;
        let mut rows = stmt
            .query_map(params![n as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.reverse(); // query returns newest first; callers want oldest first
        Ok(rows)
    }

    /// Does `needle` appear anywhere in the RECENT raw conversational record
    /// (transcripts: utterance + response)? Used by the consensus second look
    /// for its first-time-recipient advisory — transcripts are the ONE store
    /// that retains raw recipients (episodes/audit redact them away), so this
    /// is the honest "as far as I can recall" check. ASCII case-insensitive.
    /// BOUNDED by construction: transcripts are retention-capped at the newest
    /// ~2000 rows, so the full scan is trivially cheap. `instr()` rather than
    /// LIKE — the needle is model-supplied and must not carry pattern
    /// metacharacters. A needle too short to be meaningful reports `true`
    /// (seen), the fail-open direction: no advisory is ever fabricated from a
    /// degenerate lookup.
    pub async fn transcript_mentions(&self, needle: &str) -> Result<bool> {
        // ASCII-only lowercase to MATCH SQLite's built-in `lower()` (this build
        // has no ICU), so both sides fold identically. A Unicode `to_lowercase`
        // here would fold non-ASCII letters the SQL side leaves alone, missing a
        // byte-identical recipient and fabricating a false "first-time" note.
        let n = needle.trim().to_ascii_lowercase();
        if n.len() < 3 {
            return Ok(true);
        }
        let conn = self.conn.lock().await;
        let found: i64 = conn.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM transcripts
                WHERE instr(lower(text), ?1) > 0
                   OR instr(lower(coalesce(response, '')), ?1) > 0
             )",
            params![n],
            |row| row.get(0),
        )?;
        Ok(found != 0)
    }

    pub async fn record_transcript(
        &self,
        wav_path: Option<&str>,
        text: &str,
        intent: &str,
        routed_to: &str,
        response: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO transcripts(ts, wav_path, text, intent, routed_to, response)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                Utc::now().to_rfc3339(),
                wav_path,
                text,
                intent,
                routed_to,
                response
            ],
        )?;
        Ok(())
    }

    // -- EPISODIC STORE -----------------------------------------------------
    // A durable, redacted, agent-scoped, bounded record of completed turns.
    // record_episode is the WRITE path (already-redacted fields in; the table
    // re-redacts the utterance + summary defensively so a hand-built Episode can
    // never persist PII). The temporal readers below (recent/since/around) are
    // the raw Db primitives; the topical BM25 + combined ranker live in
    // crate::episodic, which reads through episodes_scoped.

    /// Store one episode. The caller (crate::episodic::record_episode) has
    /// already redacted every field, but record_episode re-redacts the free-text
    /// utterance + summary HERE (defense in depth, mirroring TraceStore::record)
    /// so the table is GUARANTEED to hold only redacted text even if a future
    /// caller builds an Episode by hand. `ts`/`id` on the input are ignored for
    /// the write — the row's ts is stamped now and the id is assigned by SQLite.
    pub async fn record_episode(&self, ep: &Episode) -> Result<()> {
        let conn = self.conn.lock().await;
        // Defensive re-redaction: NEVER trust that the caller already redacted
        // the free-text fields.
        let utterance = crate::optimize::redact(&ep.utterance_redacted);
        let summary = crate::optimize::redact(&ep.summary);
        let entities = ep.salient_entities.join(",");
        conn.execute(
            "INSERT INTO episodes(ts, agent_namespace, utterance_redacted, topic,
                salient_entities, outcome, summary)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                Utc::now().to_rfc3339(),
                ep.agent_namespace,
                utterance,
                ep.topic,
                entities,
                ep.outcome,
                summary,
            ],
        )?;
        Ok(())
    }

    /// Decode one episodes row into an [`Episode`]. Shared by every reader.
    fn row_to_episode(row: &rusqlite::Row) -> rusqlite::Result<Episode> {
        let entities: String = row.get(5)?;
        Ok(Episode {
            id: row.get(0)?,
            ts: row.get(1)?,
            agent_namespace: row.get(2)?,
            utterance_redacted: row.get(3)?,
            topic: row.get(4)?,
            salient_entities: entities
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            outcome: row.get(6)?,
            summary: row.get(7)?,
        })
    }

    /// The SQL fragment scoping episodes to one agent's recall view: its OWN
    /// namespace ("agent.<name>") plus SHARED episodes recorded under the
    /// orchestrator ("agent.jarvis"), NEVER another specialist's private
    /// namespace. This mirrors `agent_scoped_facts`'s own+shared rule: the
    /// orchestrator is the common knowledge tier, so anything it recorded is
    /// visible to all, while a specialist's episodes stay private to it. A query
    /// scoped to "agent.jarvis" itself sees only jarvis rows (own == shared),
    /// which is correct — the orchestrator's view IS the shared tier.
    const SCOPE_CLAUSE: &'static str =
        "(agent_namespace = ?1 OR agent_namespace = 'agent.jarvis')";

    /// The `n` most recent episodes visible to `namespace`, NEWEST first. The
    /// temporal recall primitive. Agent-scoped (own + shared orchestrator rows).
    pub async fn episodes_recent(&self, namespace: &str, n: usize) -> Result<Vec<Episode>> {
        let conn = self.conn.lock().await;
        let sql = format!(
            "SELECT id, ts, agent_namespace, utterance_redacted, topic,
                    salient_entities, outcome, summary
             FROM episodes WHERE {} ORDER BY id DESC LIMIT ?2",
            Self::SCOPE_CLAUSE
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![namespace, n as i64], Self::row_to_episode)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Episodes recorded STRICTLY AFTER `since_rfc3339`, visible to `namespace`,
    /// NEWEST first, capped at `limit`. RFC3339 timestamps compare
    /// lexicographically, so the string bound is exact. Agent-scoped.
    pub async fn episodes_since(
        &self,
        namespace: &str,
        since_rfc3339: &str,
        limit: usize,
    ) -> Result<Vec<Episode>> {
        let conn = self.conn.lock().await;
        let sql = format!(
            "SELECT id, ts, agent_namespace, utterance_redacted, topic,
                    salient_entities, outcome, summary
             FROM episodes WHERE {} AND ts > ?2 ORDER BY id DESC LIMIT ?3",
            Self::SCOPE_CLAUSE
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![namespace, since_rfc3339, limit as i64], Self::row_to_episode)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Episodes whose timestamp falls in the inclusive window
    /// [`from_rfc3339`, `to_rfc3339`], visible to `namespace`, NEWEST first,
    /// capped at `limit` — the "around a time" temporal primitive. Agent-scoped.
    pub async fn episodes_around(
        &self,
        namespace: &str,
        from_rfc3339: &str,
        to_rfc3339: &str,
        limit: usize,
    ) -> Result<Vec<Episode>> {
        let conn = self.conn.lock().await;
        let sql = format!(
            "SELECT id, ts, agent_namespace, utterance_redacted, topic,
                    salient_entities, outcome, summary
             FROM episodes WHERE {} AND ts >= ?2 AND ts <= ?3
             ORDER BY id DESC LIMIT ?4",
            Self::SCOPE_CLAUSE
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(
                params![namespace, from_rfc3339, to_rfc3339, limit as i64],
                Self::row_to_episode,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// A generous agent-scoped WINDOW of recent episodes for the TOPICAL ranker
    /// (crate::episodic) to rank over — own namespace + shared orchestrator rows,
    /// newest first, capped at `window`. Separate from `episodes_recent` only in
    /// intent (this feeds the BM25 ranker; that is a temporal display read);
    /// both share the scope rule so neither can leak another agent's episodes.
    pub async fn episodes_scoped(&self, namespace: &str, window: usize) -> Result<Vec<Episode>> {
        self.episodes_recent(namespace, window).await
    }

    /// FORGET: delete every episode recorded under EXACTLY `namespace` (the
    /// agent's own scope), returning how many rows were removed. The
    /// inspectable+forgettable contract — a user (or the agent's forget path)
    /// can clear that agent's episodic memory. Passing "agent.jarvis" clears the
    /// shared/orchestrator episodes. This deletes only the named namespace's
    /// rows; it never touches another agent's private episodes.
    // Part of the episodic public API (the forgettable contract); consumed by
    // the user-model + HUD stages and exercised by the unit tests. The forget
    // path is not yet wired to a daemon caller, so allow dead_code for now.
    #[allow(dead_code)]
    pub async fn forget_episodes(&self, namespace: &str) -> Result<u64> {
        let conn = self.conn.lock().await;
        let deleted = conn.execute(
            "DELETE FROM episodes WHERE agent_namespace = ?1",
            params![namespace],
        )?;
        Ok(deleted as u64)
    }

    /// Total stored episodes (for tests / telemetry / retention assertions),
    /// across ALL namespaces. UNSCOPED — inspection/bookkeeping only.
    // Inspection/telemetry surface for the HUD timeline + retention assertions;
    // exercised by the unit tests. Allow dead_code until the HUD telemetry feed
    // is wired.
    #[allow(dead_code)]
    pub async fn episodes_count(&self) -> Result<u64> {
        let conn = self.conn.lock().await;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM episodes", [], |r| r.get(0))?;
        Ok(n.max(0) as u64)
    }

    // -- RESEARCH NOTEBOOKS -------------------------------------------------
    // A persistent, redacted, agent-scoped, bounded store of SAGE research runs.
    // save_notebook_entry is the WRITE path (a run -> a cited entry); the entry's
    // synthesized text is re-redacted HERE (defense in depth) and its citations
    // are stored verbatim (they are already public URLs + titles the run
    // fetched). The cite-discipline is enforced by the caller (crate::notebook),
    // which derives the citations ONLY from the run's grounded sources; the Db
    // simply persists exactly what it is handed, so a citation row can never
    // appear that the run did not produce.

    /// The SQL fragment scoping notebooks to one agent: its OWN namespace plus
    /// the SHARED orchestrator tier ("agent.jarvis") — mirroring the episodic
    /// `SCOPE_CLAUSE`, so a specialist's private notebooks never leak to another.
    const NOTEBOOK_SCOPE_CLAUSE: &'static str =
        "(agent_namespace = ?1 OR agent_namespace = 'agent.jarvis')";

    /// Save one SAGE run as a notebook entry under `agent_namespace`/`topic_key`,
    /// returning the new entry's row id. The synthesized text is re-redacted at
    /// the store (NEVER trust the caller already did). Each citation is one row in
    /// `notebook_citations` keyed to this entry — so the notebook holds EXACTLY the
    /// run's cited sources, never a fabricated one. `ts`/`id` on the input are
    /// ignored: ts is stamped now and id is assigned by SQLite.
    pub async fn save_notebook_entry(&self, entry: &NotebookEntry) -> Result<i64> {
        let mut conn = self.conn.lock().await;
        let synthesized = crate::optimize::redact(&entry.synthesized);
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO notebook_entries(ts, agent_namespace, topic_key, topic, synthesized)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                Utc::now().to_rfc3339(),
                entry.agent_namespace,
                entry.topic_key,
                entry.topic,
                synthesized,
            ],
        )?;
        let entry_id = tx.last_insert_rowid();
        for c in &entry.citations {
            tx.execute(
                "INSERT INTO notebook_citations(entry_id, source_id, title, url)
                 VALUES (?1, ?2, ?3, ?4)",
                params![entry_id, c.source_id, c.title, c.url],
            )?;
        }
        tx.commit()?;
        Ok(entry_id)
    }

    /// Load the citations for one entry id, ordered by source id (the run order).
    async fn notebook_citations(&self, entry_id: i64) -> Result<Vec<NotebookCitation>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT source_id, title, url FROM notebook_citations
             WHERE entry_id = ?1 ORDER BY source_id ASC, id ASC",
        )?;
        let rows = stmt
            .query_map(params![entry_id], |row| {
                Ok(NotebookCitation {
                    source_id: row.get(0)?,
                    title: row.get(1)?,
                    url: row.get(2)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every entry of the notebook on EXACTLY `topic_key`, visible to `namespace`
    /// (own + shared), OLDEST first (the order the source memory accrued — a
    /// revisit reads the run history in time order). Each entry carries its
    /// citations. An empty Vec means no such notebook (honest empty — the caller
    /// never fabricates one). Agent-scoped: another specialist's notebook on the
    /// same topic is invisible.
    pub async fn notebook_entries_for(
        &self,
        namespace: &str,
        topic_key: &str,
    ) -> Result<Vec<NotebookEntry>> {
        let ids_and_rows: Vec<(i64, String, String, String, String)> = {
            let conn = self.conn.lock().await;
            let sql = format!(
                "SELECT id, ts, agent_namespace, topic, synthesized
                 FROM notebook_entries WHERE {} AND topic_key = ?2 ORDER BY id ASC",
                Self::NOTEBOOK_SCOPE_CLAUSE
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(params![namespace, topic_key], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        };
        let mut out = Vec::with_capacity(ids_and_rows.len());
        for (id, ts, agent_namespace, topic, synthesized) in ids_and_rows {
            let citations = self.notebook_citations(id).await?;
            out.push(NotebookEntry {
                id,
                ts,
                agent_namespace,
                topic_key: topic_key.to_string(),
                topic,
                synthesized,
                citations,
            });
        }
        Ok(out)
    }

    /// The distinct notebooks visible to `namespace` (own + shared), as
    /// (topic_key, topic, entry_count, last_ts), NEWEST-touched first, capped at
    /// `limit`. The browse list for the HUD panel + the "what have I researched"
    /// intent. Topic + last_ts are taken from the most recent entry of each key.
    // Browse surface for the HUD panel + the list intent; wired live via
    // notebook::dispatch (the LIST notebook intent) and exercised by the notebook
    // tests.
    pub async fn notebook_list(
        &self,
        namespace: &str,
        limit: usize,
    ) -> Result<Vec<(String, String, u64, String)>> {
        let conn = self.conn.lock().await;
        // Group by topic_key; carry the topic + ts of the MAX(id) entry (the most
        // recent run) for that key, ordered newest-touched first.
        let sql = format!(
            "SELECT e.topic_key, e.topic, cnt.n, e.ts
             FROM notebook_entries e
             JOIN (
               SELECT topic_key, COUNT(*) AS n, MAX(id) AS max_id
               FROM notebook_entries WHERE {clause}
               GROUP BY topic_key
             ) cnt ON e.id = cnt.max_id
             ORDER BY e.id DESC LIMIT ?2",
            clause = Self::NOTEBOOK_SCOPE_CLAUSE
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![namespace, limit as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?.max(0) as u64,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// FORGET one notebook: delete every entry (and its citations) on exactly
    /// `topic_key` under exactly `namespace`, returning how many ENTRIES were
    /// removed. The forgettable contract for a single notebook. Scoped to the
    /// named namespace's own rows; never touches another agent's notebooks.
    // Forget surface (the forgettable contract); wired live via notebook::dispatch
    // (the FORGET notebook intent) and exercised by the notebook tests.
    pub async fn forget_notebook(&self, namespace: &str, topic_key: &str) -> Result<u64> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM notebook_citations WHERE entry_id IN
             (SELECT id FROM notebook_entries WHERE agent_namespace = ?1 AND topic_key = ?2)",
            params![namespace, topic_key],
        )?;
        let deleted = tx.execute(
            "DELETE FROM notebook_entries WHERE agent_namespace = ?1 AND topic_key = ?2",
            params![namespace, topic_key],
        )?;
        tx.commit()?;
        Ok(deleted as u64)
    }

    /// FORGET every notebook under exactly `namespace` (the agent's own scope),
    /// returning how many entries were removed. The agent-level forget path.
    #[allow(dead_code)]
    pub async fn forget_notebooks(&self, namespace: &str) -> Result<u64> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM notebook_citations WHERE entry_id IN
             (SELECT id FROM notebook_entries WHERE agent_namespace = ?1)",
            params![namespace],
        )?;
        let deleted = tx.execute(
            "DELETE FROM notebook_entries WHERE agent_namespace = ?1",
            params![namespace],
        )?;
        tx.commit()?;
        Ok(deleted as u64)
    }

    /// BOUNDED retention for notebooks: cap the store to the newest
    /// `entries_keep` ENTRIES (evict-oldest by id, the monotonic insert order),
    /// deleting the orphaned citations too, VACUUMing when anything was removed.
    /// The bounded-memory contract — a notebook store remembers the recent runs,
    /// not "everything forever". The cap is GLOBAL (across namespaces), the same
    /// shape as the episodes/transcripts caps; revisit/browse stay agent-scoped at
    /// read time, so a global cap never lets one agent read another's rows.
    /// Returns the number of entries deleted.
    // Bounded-retention surface; exercised by the notebook tests. Its periodic
    // retention-task caller is the next wiring step (alongside the jarvis.db
    // retention pass), so allow dead_code until then.
    #[allow(dead_code)]
    pub async fn notebook_retention_pass(&self, entries_keep: usize) -> Result<u64> {
        // One guard, held for the whole pass — never re-locked across an await
        // (a second `self.conn.lock().await` would self-deadlock the tokio Mutex
        // AND make this future non-`Send`, breaking the spawned retention task).
        let mut conn = self.conn.lock().await;
        let deleted = {
            let tx = conn.transaction()?;
            // Orphan-safe: delete citations of the about-to-evict entries first.
            tx.execute(
                "DELETE FROM notebook_citations WHERE entry_id IN
                 (SELECT id FROM notebook_entries WHERE id NOT IN
                   (SELECT id FROM notebook_entries ORDER BY id DESC LIMIT ?1))",
                params![entries_keep as i64],
            )?;
            let deleted = tx.execute(
                "DELETE FROM notebook_entries WHERE id NOT IN
                 (SELECT id FROM notebook_entries ORDER BY id DESC LIMIT ?1)",
                params![entries_keep as i64],
            )?;
            tx.commit()?; // drops the transaction's borrow of `conn`
            deleted
        };
        if deleted > 0 {
            // Autocommit context now (the tx committed + dropped), so VACUUM is
            // legal under the SAME guard — no re-lock, mirroring retention_pass.
            conn.execute_batch("VACUUM")?;
        }
        Ok(deleted as u64)
    }

    /// Total stored notebook entries across ALL namespaces (tests / telemetry /
    /// retention assertions). UNSCOPED — inspection/bookkeeping only.
    #[allow(dead_code)]
    pub async fn notebook_entries_count(&self) -> Result<u64> {
        let conn = self.conn.lock().await;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM notebook_entries", [], |r| r.get(0))?;
        Ok(n.max(0) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::{is_reserved_key, Episode, Memory};
    use std::path::PathBuf;

    /// Unique temp DB per test; tests run concurrently in one process.
    struct TempDb(PathBuf);

    impl TempDb {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "jarvis-memory-test-{}-{}.db",
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

    /// The injectable test key seam: an EXPLICIT in-test key, NEVER the Keychain.
    fn test_key() -> crate::crypto::SecretKey {
        crate::crypto::SecretKey::from_bytes([4u8; crate::crypto::KEY_BYTES])
    }

    /// PERF REGRESSION: the exact-key access path (upsert_fact/get_fact/delete_fact
    /// `WHERE key = ?`) must use idx_facts_key, not a full-table scan of the shared,
    /// never-pruned facts store. Proven via EXPLAIN QUERY PLAN so a future schema
    /// edit that drops the index is caught.
    #[tokio::test]
    async fn facts_key_lookups_use_the_index_not_a_full_scan() {
        let db = TempDb::new("facts-index");
        let mem = Memory::open(&db.0).unwrap();
        let conn = mem.conn.lock().await;
        // The index exists.
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_facts_key'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "idx_facts_key must exist");
        // The exact-key lookup plan uses the index (write-amplification win). Use the
        // PRODUCTION predicate form — a bound `key = ?1` — so the plan reflects what
        // upsert_fact/get_fact/delete_fact actually run, not an inlined literal.
        let mut stmt = conn
            .prepare("EXPLAIN QUERY PLAN SELECT value FROM facts WHERE key = ?1")
            .unwrap();
        let details: Vec<String> = stmt
            .query_map(rusqlite::params!["user.world.x"], |r| r.get::<_, String>(3))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(
            details.iter().any(|d| d.contains("idx_facts_key")),
            "exact-key lookup must SEARCH via idx_facts_key, got plan: {details:?}"
        );
    }

    /// transcript_mentions (the F9 first-time-recipient check): ASCII
    /// case-insensitive substring over the raw transcript record, the
    /// short-needle fail-open guard, and honest emptiness on a fresh store.
    #[tokio::test]
    async fn transcript_mentions_is_ascii_case_insensitive_bounded_and_fail_open() {
        let db = TempDb::new("transcript-mentions");
        let mem = Memory::open(&db.0).unwrap();
        // Empty store: nothing has been seen -> false (every recipient is new).
        assert!(!mem.transcript_mentions("bob@x.io").await.unwrap());
        // A short needle reports "seen" (fail-open — never fabricate an advisory
        // from a degenerate lookup).
        assert!(mem.transcript_mentions("hi").await.unwrap());

        mem.record_transcript(None, "email Bob@X.io about the launch", "intent", "local", Some("Done."))
            .await
            .unwrap();
        // Case-insensitive match against the stored utterance, either column.
        assert!(mem.transcript_mentions("bob@x.io").await.unwrap());
        assert!(mem.transcript_mentions("BOB@X.IO").await.unwrap());
        // The RESPONSE column is scanned too (the park preview names recipients).
        mem.record_transcript(None, "do the thing", "intent", "local", Some("Sent to carol@y.z."))
            .await
            .unwrap();
        assert!(mem.transcript_mentions("carol@y.z").await.unwrap());
        // A genuinely-unseen recipient is not found.
        assert!(!mem.transcript_mentions("stranger@new.tld").await.unwrap());
    }

    #[tokio::test]
    async fn open_encrypted_round_trips_and_is_ciphertext_at_rest() {
        let db = TempDb::new("enc-roundtrip");
        let key = test_key();
        // Write a fact through the ENCRYPTED open (the [security].encrypt_memory ON
        // path) with an explicit test key — no Keychain, no network.
        {
            let mem = Memory::open_encrypted(&db.0, &key).unwrap();
            mem.upsert_fact("user.name", "Darwin-secret-canary").await.unwrap();
        }
        // The on-disk file is CIPHERTEXT: the value is not in the clear and the
        // SQLite magic header is absent (it's a SQLCipher file).
        let raw = std::fs::read(&db.0).unwrap();
        assert!(
            !raw.windows(b"Darwin-secret-canary".len())
                .any(|w| w == b"Darwin-secret-canary"),
            "fact value must not appear in plaintext on disk"
        );
        assert!(!raw.starts_with(b"SQLite format 3\0"), "must be SQLCipher-encrypted");
        // Reopen WITH the key: the data reads back.
        {
            let mem = Memory::open_encrypted(&db.0, &key).unwrap();
            let facts = mem.all_facts(10).await.unwrap();
            assert_eq!(facts.len(), 1);
            assert_eq!(facts[0].1, "Darwin-secret-canary");
        }
        // Reopen with the WRONG key fails (cannot read a keyed DB without the key).
        let wrong = crate::crypto::SecretKey::from_bytes([0u8; crate::crypto::KEY_BYTES]);
        assert!(Memory::open_encrypted(&db.0, &wrong).is_err(), "wrong key must fail");
    }

    #[tokio::test]
    async fn off_path_open_is_plaintext_unchanged() {
        // With [security].encrypt_memory OFF the daemon uses Memory::open (no key):
        // the file is an ordinary SQLite DB (the magic header is present) — proving
        // OFF is byte-for-byte today's plaintext behavior.
        let db = TempDb::new("plaintext-unchanged");
        {
            let mem = Memory::open(&db.0).unwrap();
            mem.upsert_fact("user.name", "Darwin").await.unwrap();
        }
        let raw = std::fs::read(&db.0).unwrap();
        assert!(
            raw.starts_with(b"SQLite format 3\0"),
            "OFF path must be a plaintext SQLite file (no encryption)"
        );
    }

    #[tokio::test]
    async fn upsert_fact_inserts_then_updates_in_place() {
        let db = TempDb::new("upsert");
        let mem = Memory::open(&db.0).unwrap();

        mem.upsert_fact("user.name", "Darwin").await.unwrap();
        mem.upsert_fact("user.preference.voice", "British").await.unwrap();
        mem.upsert_fact("user.name", "Darwin Capani").await.unwrap();

        let facts = mem.all_facts(10).await.unwrap();
        assert_eq!(facts.len(), 2, "update must not add a row: {facts:?}");
        let name = facts.iter().find(|(k, _)| k == "user.name").unwrap();
        assert_eq!(name.1, "Darwin Capani");
    }

    #[tokio::test]
    async fn recent_exchanges_returns_oldest_first_and_skips_responseless_rows() {
        let db = TempDb::new("exchanges");
        let mem = Memory::open(&db.0).unwrap();

        mem.record_transcript(None, "first", "conversation", "local", Some("reply one"))
            .await
            .unwrap();
        mem.record_transcript(None, "no reply recorded", "conversation", "local", None)
            .await
            .unwrap();
        mem.record_transcript(None, "second", "conversation", "local", Some("reply two"))
            .await
            .unwrap();
        mem.record_transcript(None, "third", "system.query", "local", Some("reply three"))
            .await
            .unwrap();

        let exchanges = mem.recent_exchanges(2).await.unwrap();
        assert_eq!(
            exchanges,
            vec![
                ("second".to_string(), "reply two".to_string()),
                ("third".to_string(), "reply three".to_string()),
            ],
            "expected the 2 newest rows with responses, oldest first"
        );
    }

    #[tokio::test]
    async fn meta_keys_are_filtered_from_user_facts_but_not_all_facts() {
        let db = TempDb::new("meta-filter");
        let mem = Memory::open(&db.0).unwrap();

        mem.upsert_fact("user.name", "Darwin").await.unwrap();
        mem.upsert_fact("meta.last_reflection", "1760000000").await.unwrap();
        mem.upsert_fact("user.preference.voice", "British").await.unwrap();

        let user_facts = mem.all_user_facts(10).await.unwrap();
        assert_eq!(user_facts.len(), 2, "meta key leaked: {user_facts:?}");
        assert!(user_facts.iter().all(|(k, _)| !k.starts_with("meta.")));

        // all_facts (reflection bookkeeping reads) still sees everything.
        assert_eq!(mem.all_facts(10).await.unwrap().len(), 3);
        assert_eq!(
            mem.get_fact("meta.last_reflection").await.unwrap(),
            Some("1760000000".to_string())
        );
        assert_eq!(mem.get_fact("meta.missing").await.unwrap(), None);
    }

    #[test]
    fn reserved_keys_match_the_meta_prefix_case_insensitively() {
        // SQLite LIKE is ASCII case-insensitive; the guard must be too, or a
        // model writes "Meta.x" that the prompt filter then hides.
        assert!(is_reserved_key("meta.last_reflection"));
        assert!(is_reserved_key("META.heal_pending"));
        assert!(is_reserved_key("Meta.last_interaction"));
        assert!(is_reserved_key("  meta.heal_last_attempt  ")); // trimmed
        assert!(is_reserved_key("meta."));
        // Not reserved: meta-ish but not the dotted namespace.
        assert!(!is_reserved_key("metadata.format"));
        assert!(!is_reserved_key("user.metadata"));
        assert!(!is_reserved_key("user.name"));
        assert!(!is_reserved_key("meta")); // no dot
        assert!(!is_reserved_key(""));
    }

    #[tokio::test]
    async fn model_driven_upserts_cannot_touch_meta_keys() {
        let db = TempDb::new("meta-guard");
        let mem = Memory::open(&db.0).unwrap();

        // Trusted internal write (reflection stamp, heal bookkeeping).
        mem.upsert_fact("meta.last_reflection", "1760000000").await.unwrap();

        // Model-driven write paths must be rejected — including the
        // case-variant a LIKE-filtered display would hide.
        for key in ["meta.last_reflection", "META.last_reflection", " meta.heal_pending"] {
            let err = mem.upsert_user_fact(key, "9999999999").await.unwrap_err();
            assert!(err.to_string().contains("reserved"), "wrong error: {err}");
        }
        // The bookkeeping row is untouched.
        assert_eq!(
            mem.get_fact("meta.last_reflection").await.unwrap(),
            Some("1760000000".to_string())
        );
        // Ordinary user facts still flow through.
        mem.upsert_user_fact("user.name", "Darwin").await.unwrap();
        assert_eq!(mem.get_fact("user.name").await.unwrap(), Some("Darwin".to_string()));
    }

    #[tokio::test]
    async fn agent_scoped_facts_see_own_namespace_plus_shared_only() {
        let db = TempDb::new("agent-scope");
        let mem = Memory::open(&db.0).unwrap();

        // Shared (non-namespaced) facts: every agent sees these.
        mem.upsert_fact("user.name", "Darwin").await.unwrap();
        // friday's private namespace.
        mem.upsert_fact("agent.friday.last_brief", "markets up").await.unwrap();
        // jerome's private namespace — friday must NOT see this.
        mem.upsert_fact("agent.jerome.last_track", "some song").await.unwrap();
        // meta bookkeeping is never visible to any agent.
        mem.upsert_fact("meta.last_interaction", "1760000000").await.unwrap();

        let friday = mem.agent_scoped_facts("agent.friday", 20).await.unwrap();
        let keys: Vec<&str> = friday.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"user.name"), "shared fact missing: {keys:?}");
        assert!(keys.contains(&"agent.friday.last_brief"), "own namespace missing: {keys:?}");
        assert!(
            !keys.contains(&"agent.jerome.last_track"),
            "leaked another agent's namespace: {keys:?}"
        );
        assert!(!keys.iter().any(|k| k.starts_with("meta.")), "meta leaked: {keys:?}");

        // jerome sees its own + shared, but not friday's.
        let jerome = mem.agent_scoped_facts("agent.jerome", 20).await.unwrap();
        let jkeys: Vec<&str> = jerome.iter().map(|(k, _)| k.as_str()).collect();
        assert!(jkeys.contains(&"agent.jerome.last_track"));
        assert!(jkeys.contains(&"user.name"));
        assert!(!jkeys.contains(&"agent.friday.last_brief"));
    }

    #[tokio::test]
    async fn delete_fact_removes_the_row_and_reports_existence() {
        let db = TempDb::new("delete");
        let mem = Memory::open(&db.0).unwrap();

        mem.upsert_fact("user.pet", "a corgi named Watson").await.unwrap();
        assert!(mem.delete_fact("user.pet").await.unwrap());
        assert!(!mem.delete_fact("user.pet").await.unwrap(), "second delete is a no-op");
        assert!(mem.all_facts(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn facts_learned_since_counts_only_newer_non_meta_rows() {
        let db = TempDb::new("learned-since");
        let mem = Memory::open(&db.0).unwrap();

        mem.upsert_fact("user.old", "before the cutoff").await.unwrap();
        // A cutoff strictly after the first row, before the rest. RFC3339
        // rows compare lexicographically, so a far-future / far-past marker
        // around real inserts is exact.
        let cutoff = chrono::Utc::now().to_rfc3339();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        mem.upsert_fact("user.new", "after the cutoff").await.unwrap();
        mem.upsert_fact("meta.last_interaction", "1760000000").await.unwrap();
        mem.upsert_fact("user.old", "updated after the cutoff").await.unwrap();

        // user.new + the updated user.old count; the meta row never does.
        assert_eq!(mem.facts_learned_since(&cutoff).await.unwrap(), 2);
        // Nothing is newer than a far-future cutoff.
        assert_eq!(mem.facts_learned_since("9999-01-01T00:00:00+00:00").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn retention_pass_prunes_old_events_and_caps_transcripts() {
        let db = TempDb::new("retention");
        let mem = Memory::open(&db.0).unwrap();

        // Two ancient events (synthetic timestamps), one fresh.
        {
            let conn = mem.conn.lock().await;
            for ts in ["2020-01-01T00:00:00+00:00", "2021-06-01T00:00:00+00:00"] {
                conn.execute(
                    "INSERT INTO events(ts, source, kind, payload) VALUES (?1, 'x', 'k', 'p')",
                    rusqlite::params![ts],
                )
                .unwrap();
            }
        }
        mem.record_event("audio", "utterance.captured", "now").await.unwrap();

        // Four transcripts; keep the newest two.
        for text in ["t1", "t2", "t3", "t4"] {
            mem.record_transcript(None, text, "conversation", "local", Some("r"))
                .await
                .unwrap();
        }

        let (events_deleted, transcripts_deleted, episodes_deleted) =
            mem.retention_pass(30, 2, 5000).await.unwrap();
        assert_eq!(events_deleted, 2, "both ancient events pruned");
        assert_eq!(transcripts_deleted, 2, "transcripts capped to the newest 2");
        assert_eq!(episodes_deleted, 0, "no episodes were recorded in this test");
        let kept = mem.recent_exchanges(10).await.unwrap();
        assert_eq!(
            kept.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(),
            vec!["t3", "t4"],
            "the NEWEST transcripts survive"
        );

        // Idempotent: a second pass with nothing to do deletes nothing.
        assert_eq!(mem.retention_pass(30, 2, 5000).await.unwrap(), (0, 0, 0));
    }

    fn sample_episode(ns: &str, utterance: &str) -> Episode {
        Episode {
            id: 0,
            ts: String::new(),
            agent_namespace: ns.to_string(),
            utterance_redacted: utterance.to_string(),
            topic: "conversation".to_string(),
            salient_entities: vec!["one".to_string(), "two".to_string()],
            outcome: "ok".to_string(),
            summary: format!("{utterance} -> ok"),
        }
    }

    #[tokio::test]
    async fn record_episode_re_redacts_free_text_defensively_at_the_db() {
        // Even a hand-built Episode carrying a raw secret must NOT persist it:
        // record_episode re-redacts the utterance + summary at the store (defense
        // in depth, mirroring TraceStore::record).
        let db = TempDb::new("ep-redact");
        let mem = Memory::open(&db.0).unwrap();
        let mut ep = sample_episode("agent.jarvis", "leak sk-ABCDEF0123456789ABCD here");
        ep.summary = "summary with sk-ABCDEF0123456789ABCD inside".to_string();
        mem.record_episode(&ep).await.unwrap();

        let got = mem.episodes_recent("agent.jarvis", 5).await.unwrap();
        assert_eq!(got.len(), 1);
        assert!(
            !got[0].utterance_redacted.contains("sk-ABCDEF0123456789ABCD"),
            "secret leaked into the stored utterance: {}",
            got[0].utterance_redacted
        );
        assert!(
            !got[0].summary.contains("sk-ABCDEF0123456789ABCD"),
            "secret leaked into the stored summary: {}",
            got[0].summary
        );
        // The structured fields round-trip intact.
        assert_eq!(got[0].topic, "conversation");
        assert_eq!(got[0].salient_entities, vec!["one".to_string(), "two".to_string()]);
        assert_eq!(got[0].outcome, "ok");
    }

    #[tokio::test]
    async fn episodes_are_agent_scoped_own_plus_shared_orchestrator_only() {
        let db = TempDb::new("ep-scope");
        let mem = Memory::open(&db.0).unwrap();
        mem.record_episode(&sample_episode("agent.friday", "friday market note")).await.unwrap();
        mem.record_episode(&sample_episode("agent.jerome", "jerome song note")).await.unwrap();
        mem.record_episode(&sample_episode("agent.jarvis", "shared weather note")).await.unwrap();

        let friday = mem.episodes_recent("agent.friday", 10).await.unwrap();
        let texts: Vec<&str> = friday.iter().map(|e| e.utterance_redacted.as_str()).collect();
        assert!(texts.contains(&"friday market note"), "own missing: {texts:?}");
        assert!(texts.contains(&"shared weather note"), "shared orchestrator missing: {texts:?}");
        assert!(!texts.contains(&"jerome song note"), "cross-agent leak: {texts:?}");
        // The orchestrator's own view IS the shared tier (own == shared).
        let jarvis = mem.episodes_recent("agent.jarvis", 10).await.unwrap();
        assert_eq!(jarvis.len(), 1, "jarvis sees only the shared row");
    }

    #[tokio::test]
    async fn forget_episodes_clears_only_the_named_namespace() {
        let db = TempDb::new("ep-forget");
        let mem = Memory::open(&db.0).unwrap();
        mem.record_episode(&sample_episode("agent.friday", "a")).await.unwrap();
        mem.record_episode(&sample_episode("agent.friday", "b")).await.unwrap();
        mem.record_episode(&sample_episode("agent.jerome", "c")).await.unwrap();
        assert_eq!(mem.forget_episodes("agent.friday").await.unwrap(), 2);
        assert_eq!(mem.episodes_recent("agent.friday", 10).await.unwrap().len(), 0);
        assert_eq!(mem.episodes_recent("agent.jerome", 10).await.unwrap().len(), 1);
        assert_eq!(mem.episodes_count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn open_is_idempotent_across_reopens() {
        let db = TempDb::new("migration");
        {
            let mem = Memory::open(&db.0).unwrap();
            mem.upsert_fact("k", "v").await.unwrap();
        }
        // Second open re-runs the schema batch and the response-column
        // migration against an existing DB; both must be no-ops.
        let mem = Memory::open(&db.0).unwrap();
        mem.record_transcript(Some("/tmp/x.wav"), "hello", "conversation", "local", Some("hi"))
            .await
            .unwrap();
        assert_eq!(mem.recent_exchanges(5).await.unwrap().len(), 1);
        assert_eq!(mem.all_facts(5).await.unwrap().len(), 1);
    }
}

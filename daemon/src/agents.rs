//! The agent constellation: 27 named profiles on the one local engine.
//!
//! Each [`Agent`] is a PROFILE — a persona prefix, a Kokoro voice, a HUD core
//! hue, a tool allowlist, and a memory namespace — NOT a separate model.
//! Darwin-Prime (`select`) hears every request and delegates to the right
//! agent via a deterministic rule map (intent + keywords -> agent); the
//! selected agent then runs the EXISTING converse/cloud pipeline with its own
//! persona, voice, namespace, and allowlist (router.rs). Honesty is the
//! cardinal rule: agents never imply capability the one engine lacks.
//!
//! The registry is parsed from config/agents.toml with `deny_unknown_fields`,
//! so a typo'd key is a hard parse error rather than a silently dropped
//! customization — the daemon falls back to a hardcoded canonical roster if
//! the file is missing or malformed, so it always runs with the full team.
//!
//! ## Lockstep with config/agents.toml
//! [`Agent`]'s fields mirror each `[[agent]]` entry exactly (name, role,
//! voice, hue, persona_file, tools, namespace). [`canonical`] returns the
//! same 27 agents the shipped file carries; the
//! `shipped_agents_file_matches_canonical` test fails if either side drifts.

use std::collections::HashSet;
use std::path::Path;

use serde::Deserialize;
use tracing::warn;

/// The wildcard tools entry that marks the orchestrator: darwin alone may
/// invoke any tool/intent in the system.
const TOOLS_WILDCARD: &str = "*";

/// One agent profile, mirroring a `[[agent]]` entry in config/agents.toml.
/// `deny_unknown_fields`: a mistyped key is a parse error, never a silently
/// ignored customization (the registry then falls back to canonical).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Agent {
    /// Lowercase id; also the memory-namespace suffix and persona filename
    /// stem (inference/personas/<name>.txt).
    pub name: String,
    /// One-line role description (HUD roster panel + roll-call context).
    pub role: String,
    /// Kokoro voice id, verified to load from the Kokoro-82M voice set.
    pub voice: String,
    /// HUD core hue in integer degrees, 0-360.
    pub hue: u16,
    /// Persona prompt file (read by the daemon; owned by part B).
    pub persona_file: String,
    /// Action/intent allowlist this agent may invoke. `["*"]` = all (darwin).
    pub tools: Vec<String>,
    /// Memory namespace, "agent.<name>".
    pub namespace: String,
}

impl Agent {
    /// True when this agent may invoke `tool` — either it holds the wildcard
    /// (darwin, the orchestrator) or `tool` is in its allowlist. The check is
    /// what enforces isolation: an agent attempting a tool outside its list is
    /// refused and the request routed to the owning agent (router.rs).
    pub fn may_use(&self, tool: &str) -> bool {
        self.tools.iter().any(|t| t == TOOLS_WILDCARD || t == tool)
    }

    /// True when this agent is the orchestrator (holds the tools wildcard).
    pub fn is_orchestrator(&self) -> bool {
        self.tools.iter().any(|t| t == TOOLS_WILDCARD)
    }

    /// The persona name the converse op maps to inference/personas/<name>.txt.
    /// This is the agent's `name` by contract (the persona filename stem); the
    /// daemon passes it (or the persona file text) so the reply is voiced in
    /// the agent's persona without a per-engine model.
    pub fn persona_name(&self) -> &str {
        &self.name
    }

    /// The agent's one-line self-introduction for roll-call, read from the
    /// first "INTRO: <sentence>" line of its persona file (resolved relative to
    /// `root`). Falls back to a grounded sentence built from name + role when
    /// the file or the INTRO line is missing — roll-call must never go silent
    /// on one agent, and the fallback invents nothing beyond the roster.
    pub fn intro(&self, root: &Path) -> String {
        let path = root.join(&self.persona_file);
        if let Ok(contents) = std::fs::read_to_string(&path) {
            if let Some(line) = parse_intro(&contents) {
                return line;
            }
            warn!(agent = %self.name, path = %path.display(), "persona file has no INTRO line; using fallback");
        } else {
            warn!(agent = %self.name, path = %path.display(), "persona file unreadable; using fallback intro");
        }
        format!("{}. {}.", capitalize(&self.name), self.role)
    }
}

/// Extract the roll-call self-introduction from persona file text: the content
/// after the first line beginning "INTRO:" (case-insensitive), trimmed. None
/// when no such line exists or it is blank.
fn parse_intro(contents: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        let trimmed = line.trim_start();
        let rest = trimmed.strip_prefix("INTRO:").or_else(|| trimmed.strip_prefix("intro:"))?;
        let intro = rest.trim();
        (!intro.is_empty()).then(|| intro.to_string())
    })
}

/// Capitalize the first ASCII letter (agent names are lowercase ids).
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Roll-call trigger phrases: an utterance asking the team to introduce
/// itself. Word-boundary matched against the lowercased utterance so
/// "assemble" does not fire on "assembled the report".
const ROLL_CALL_CUES: &[&str] = &[
    "roll call", "rollcall", "introduce the team", "introduce yourselves",
    "introduce yourself", "assemble", "assemble the team", "meet the team",
    "who is on the team", "who's on the team", "team roll call",
];

/// Whether `text` asks for the constellation roll-call (item 3, the reel
/// centerpiece). Deterministic and unit-testable; checked before normal
/// routing in the daemon.
pub fn is_roll_call(text: &str) -> bool {
    let lower = text.to_lowercase();
    ROLL_CALL_CUES.iter().any(|cue| {
        if cue.contains(' ') {
            // Multi-word cues: plain substring is enough (the phrase itself is
            // already specific) — a contained space cannot bleed into another
            // word the way a bare token can.
            lower.contains(cue)
        } else {
            contains_word(&lower, cue)
        }
    })
}

/// Cues for an agent-ROSTER query — the user asking to enumerate / know about
/// their agents/team/constellation ("list my agents", "who are my agents",
/// "what's the constellation"). Distinct from a roll-call (which has the team
/// SPEAK their intros); this wants a spoken LIST. High-precision phrases so it
/// does not fire on ordinary chat that merely mentions an agent.
const AGENT_QUERY_CUES: &[&str] = &[
    "my agents", "list of agents", "list the agents", "list agents",
    "which agents", "name the agents", "name my agents", "how many agents",
    "show me the agents", "who are the agents", "what agents do",
    "the constellation", "my constellation", "agent roster", "the roster",
    "list my team", "name my team", "who are my team",
];

/// Whether `text` asks DARWIN to LIST/NAME its agents (the roster), as opposed
/// to running the spoken roll-call ([`is_roll_call`], checked first). Routed
/// DETERMINISTICALLY by the daemon so the answer always comes from the live
/// registry — never the classifier+local model, which (lacking the roster)
/// hallucinates agents that do not exist.
pub fn is_agent_query(text: &str) -> bool {
    let lower = text.to_lowercase();
    AGENT_QUERY_CUES.iter().any(|cue| lower.contains(cue))
}

/// EDITH (Proactive Sentinel) delegation cues: phrases that ask DARWIN to
/// ANTICIPATE — to surface what is coming or what the user should know — as
/// opposed to friday's intel cues (brief/morning/news/schedule), which EDITH
/// must NOT steal. High-precision multi-word phrases so a plain mention of
/// "watch" (e.g. "play a watch video") does not misroute; the routing check
/// uses plain substring because each phrase is already specific. Order is
/// irrelevant here (any match routes to edith).
const EDITH_CUES: &[&str] = &[
    "heads up",
    "anticipate",
    "proactive",
    "watch for",
    "keep an eye",
    "alert me",
    "what's coming",
    "whats coming",
    "what is coming",
    "what should i know",
    "anything i should know",
];

/// Whether `lower` (already lowercased) is an EDITH anticipation query. Pure and
/// unit-testable; consulted by [`AgentRegistry::select`] before the broad
/// keyword chain so anticipation phrases route to the Proactive Sentinel.
fn is_anticipation_query(lower: &str) -> bool {
    EDITH_CUES.iter().any(|cue| lower.contains(cue))
}

/// FURY (Mission Orchestrator) delegation cues: phrases that ask DARWIN to run a
/// MULTI-STEP MISSION — decompose a goal, assemble the right specialists, drive
/// it to done — as opposed to a single delegated task. These are HIGH-PRECISION
/// multi-word phrases (plain substring is enough; each phrase is already
/// specific) so an ordinary one-shot request ("draft a post", "open safari")
/// does NOT misroute into a mission. They deliberately AVOID the roll-call
/// trigger phrases ([`ROLL_CALL_CUES`], checked earlier in the daemon: "meet the
/// team", "assemble the team", "introduce the team") — those want the team to
/// SPEAK its intros, not to do work — so "get the team" / "the whole team on"
/// here mean "put the specialists to work," not "say hello." Order is irrelevant
/// (any match routes to fury).
const FURY_CUES: &[&str] = &[
    "mission",
    "orchestrate",
    "coordinate",
    "run point",
    "campaign",
    "multi-step",
    "multistep",
    "end to end",
    "end-to-end",
    "handle all of",
    "take care of everything",
    "take care of all of",
    "get the team on",
    "get the whole team",
    "put the team on",
    "the whole team on",
];

/// Whether `lower` (already lowercased) is a FURY mission query. Pure and
/// unit-testable; consulted by [`AgentRegistry::select`] before the broad
/// keyword chain (and just after EDITH's anticipation check) so a multi-step
/// "assemble the team for X" routes to the Mission Orchestrator rather than to
/// whichever specialist happens to match a stray domain keyword inside the goal.
fn is_mission_query(lower: &str) -> bool {
    FURY_CUES.iter().any(|cue| lower.contains(cue))
}

/// CASSANDRA (Forecast & Simulation) delegation cues: phrases that ask DARWIN to
/// MODEL what could happen — run a simulation, forecast a distribution, weigh the
/// odds of a what-if — as opposed to gecko's MARKET watch (market/trade/stock/
/// crypto/portfolio/ticker), which Cassandra must NOT steal. These are precise:
/// "simulate"/"forecast"/"scenario"/"monte carlo"/"what if"/"odds"/"probability"
/// are modeling verbs, distinct from "what's the market doing" (live watch ->
/// gecko). Word-boundary matched for single tokens (so "model" does not fire
/// inside "modeling agency" via a substring quirk — `contains_word` handles it),
/// plain substring for the already-specific multi-word phrases. A request that
/// mentions a stock AND asks to simulate it ("simulate this stock over a year")
/// routes here because the MODELING verb is the more specific intent — Cassandra
/// runs the numbers, she does not quote the live tape. Order is irrelevant (any
/// match routes to cassandra).
const CASSANDRA_MULTI_CUES: &[&str] = &[
    "monte carlo",
    "what if",
    "what-if",
    "model this",
];
const CASSANDRA_WORD_CUES: &[&str] = &[
    "simulate",
    "simulation",
    "forecast",
    "scenario",
    "scenarios",
    "project",
    "projection",
    "projections",
    "odds",
    "probability",
    "likelihood",
];

/// Whether `lower` (already lowercased) is a CASSANDRA forecast/simulation query.
/// Pure and unit-testable; consulted by [`AgentRegistry::select`] after EDITH and
/// FURY but BEFORE the broad single-domain keyword chain, so a "simulate this
/// stock" or "what are the odds" routes to the modeler rather than to gecko's
/// market watch on a stray ticker word. Multi-word phrases use plain substring
/// (already specific); single tokens use the whole-word check so a cue cannot
/// fire as a substring of a larger word.
fn is_forecast_query(lower: &str) -> bool {
    CASSANDRA_MULTI_CUES.iter().any(|cue| lower.contains(cue))
        || CASSANDRA_WORD_CUES.iter().any(|cue| contains_word(lower, cue))
}

/// MNEMOSYNE (Semantic Memory) delegation cues: phrases that ask DARWIN to
/// RETRIEVE the stored past — "what did I say about X", "what do you remember
/// about Y", "dig up that note", "recall everything", "when did I", "have we
/// discussed". This is the RECALL half of memory, deliberately distinct from
/// pepper's STORE/reminder cues ("remember to", "remind me", "set a reminder"),
/// which Mnemosyne must NOT poach: pepper WRITES a fact or schedules a nudge;
/// Mnemosyne READS the existing memory and ranks the relevant facts. The cues
/// are RETRIEVAL phrasings — "what did i say / what do you remember about / dig
/// up / recall everything / find that note / when did i / have we discussed /
/// surface what you know about" — multi-word and already specific, so plain
/// substring is enough and a bare "remember" (pepper's store cue) never reaches
/// here. Order is irrelevant (any match routes to mnemosyne).
const MNEMOSYNE_CUES: &[&str] = &[
    "what did i say",
    "what did i tell you",
    "what do you remember about",
    "what do you remember regarding",
    "dig up",
    "recall everything",
    "find that note",
    "when did i",
    "have we discussed",
    "did we discuss",
    "surface what you know about",
    "what have i told you about",
    "remind me what",
    "pull up what i said",
    // UNIFIED "search everything" cues -> Mnemosyne owns the unified_search tool.
    // Multi-word + specific (so a bare "search" never reaches here), they route a
    // cross-source personal search to the retrieval specialist.
    "search everything",
    "search across everything",
    "search all my",
    "search my stuff",
    "find it across everything",
    "across all my sources",
    "search all of my",
];

/// Whether `lower` (already lowercased) is a MNEMOSYNE RETRIEVAL query. Pure and
/// unit-testable; consulted by [`AgentRegistry::select`] after the other
/// high-precision specialist checks but BEFORE the broad keyword chain (and
/// crucially before pepper's store/reminder keyword cues), so "what did I say
/// about the budget" routes to the recall specialist rather than to pepper's
/// store side on a stray "budget" or "remind" token. These are RETRIEVAL
/// phrasings only — they never include pepper's bare store cues ("remember to",
/// "set a reminder"), so the two halves of memory stay cleanly split.
fn is_recall_query(lower: &str) -> bool {
    MNEMOSYNE_CUES.iter().any(|cue| lower.contains(cue))
}

/// SAGE (Deep Research) delegation cues: phrases that ask DARWIN for a
/// THOROUGH, CITED, multi-source investigation — "deep dive", "deep research",
/// "research report", "look into X thoroughly", "cite your sources", "with
/// citations", "comprehensive write-up" — as opposed to vision's QUICK lookup
/// (vision owns the bare "research / osint / lookup / investigate / footprint"
/// keywords). The boundary the contract pins: vision = a fast pass on
/// authorized targets; SAGE = a bounded MULTI-SOURCE report whose every claim is
/// cited. So SAGE deliberately does NOT poach vision's plain "research"/
/// "investigate"/"lookup" tokens — it owns only the DEEP/CITED variant. These
/// are HIGH-PRECISION multi-word phrases (plain substring is enough; each is
/// already specific) checked BEFORE the broad keyword chain so a "deep dive into
/// the literature, with sources" routes to SAGE rather than to vision on the
/// stray "research" inside it. Single-token cues ("citations", "literature")
/// that are themselves specific to a cited report are matched whole-word in the
/// select() chain. Order is irrelevant (any match routes to sage).
const SAGE_MULTI_CUES: &[&str] = &[
    "deep dive",
    "deep research",
    "deep-dive",
    "thorough research",
    "thorough investigation",
    "research report",
    "research the literature",
    "cite your sources",
    "cite sources",
    "with citations",
    "with sources",
    "comprehensive overview",
    "comprehensive report",
    "comprehensive write-up",
    "comprehensive writeup",
    "everything about",
    "literature review",
];
/// Single-token SAGE cues, matched whole-word: each is itself specific to a
/// CITED, multi-source report (NOT vision's quick-lookup vocabulary).
/// "citations"/"cited"/"sourced"/"literature" only ever appear in a request for
/// a sourced write-up, so they route to SAGE on their own.
const SAGE_WORD_CUES: &[&str] = &[
    "citations",
    "cited",
    "sourced",
    "literature",
];

/// Depth ADVERBS ("thoroughly", "comprehensively") only mean DEEP RESEARCH when
/// they sit alongside an investigation verb — "look into X thoroughly",
/// "research it comprehensively". On their own ("clean it thoroughly") they are
/// not a research cue, so SAGE requires BOTH the adverb AND an
/// investigation/look-into context before claiming the turn — that keeps it from
/// poaching unrelated uses of "thoroughly" and from stealing vision's bare
/// "research" token (vision still wins "research the competitors").
const SAGE_DEPTH_ADVERBS: &[&str] = &["thoroughly", "comprehensively"];
const SAGE_INVESTIGATION_CONTEXT: &[&str] = &[
    "look into", "research", "investigate", "dig into", "read up", "study",
];

/// Whether `lower` (already lowercased) is a SAGE DEEP-RESEARCH query. Pure and
/// unit-testable; consulted by [`AgentRegistry::select`] BEFORE the broad
/// keyword chain (where vision owns the quick "research/osint/lookup" tokens),
/// so a thorough cited investigation routes to the deep-research specialist
/// rather than to vision's quick lookup. The boundary is honest: SAGE matches
/// only the DEEP/CITED phrasings; a bare "research the competitors" (no depth/
/// citation cue) still reaches vision. Multi-word phrases use plain substring
/// (already specific); single tokens use the whole-word check so a cue cannot
/// fire as a substring of a larger word; the depth adverbs additionally require
/// an investigation context so they never fire on an unrelated "thoroughly".
fn is_deep_research_query(lower: &str) -> bool {
    if SAGE_MULTI_CUES.iter().any(|cue| lower.contains(cue)) {
        return true;
    }
    if SAGE_WORD_CUES.iter().any(|cue| contains_word(lower, cue)) {
        return true;
    }
    // A depth adverb counts only with an investigation/look-into context present.
    let has_depth_adverb = SAGE_DEPTH_ADVERBS.iter().any(|cue| contains_word(lower, cue));
    let has_context = SAGE_INVESTIGATION_CONTEXT.iter().any(|cue| lower.contains(cue));
    has_depth_adverb && has_context
}

/// VITALIS (Health & Biometrics) delegation cues: phrases that ask DARWIN to read
/// the BODY's signals off WHOOP — recovery, strain, HRV, sleep score, readiness,
/// resting heart rate, "how recovered am I", "how did I sleep", "how's my body".
/// These are BIOMETRICS, deliberately distinct from hercules' COACHING vocabulary
/// (workout/exercise/training/nutrition/diet/macros/fitness/lift/run), which
/// Vitalis must NOT poach: hercules programs the training and the diet; Vitalis
/// reads what the band measured and tells the user what their body is saying. The
/// boundary is the data, not the topic — "what should I train today" stays with
/// hercules; "how recovered am I / what's my HRV / how did I sleep" is Vitalis.
/// Multi-word phrases are already specific (plain substring is enough); the
/// single-token cues ("recovery", "strain", "hrv", "readiness", "whoop") are
/// matched whole-word in the select() chain so a cue cannot fire as a substring
/// of a larger word. "sleep" is deliberately NOT a bare single-token cue (it is
/// too broad — "set a sleep timer" is jerome/pepper), so the sleep route is
/// claimed only by the specific "sleep score" / "how did i sleep" phrasings.
/// Order is irrelevant (any match routes to vitalis).
const VITALIS_MULTI_CUES: &[&str] = &[
    "sleep score",
    "how recovered",
    "how did i sleep",
    "how's my body",
    "hows my body",
    "how is my body",
    "resting heart rate",
    "resting hr",
];
const VITALIS_WORD_CUES: &[&str] = &[
    "recovery",
    "strain",
    "hrv",
    "readiness",
    "whoop",
    "biometrics",
];

/// Whether `lower` (already lowercased) is a VITALIS biometrics query. Pure and
/// unit-testable; consulted by [`AgentRegistry::select`] BEFORE the broad
/// single-domain keyword chain (where hercules owns the workout/nutrition
/// COACHING tokens), so "how recovered am I" / "what's my HRV" / "whoop sleep
/// score" routes to the biometrics specialist rather than to hercules on a stray
/// fitness word. The boundary is honest: Vitalis matches only the BIOMETRIC
/// READING phrasings; a bare "plan my workout" or "what should I eat" still
/// reaches hercules. Multi-word phrases use plain substring (already specific);
/// single tokens use the whole-word check.
fn is_biometric_query(lower: &str) -> bool {
    VITALIS_MULTI_CUES.iter().any(|cue| lower.contains(cue))
        || VITALIS_WORD_CUES.iter().any(|cue| contains_word(lower, cue))
}

/// KAREN (Comms Autopilot) delegation cues: phrases that ask DARWIN to TRIAGE the
/// inbox and channels — "triage / inbox / catch me up on messages / what needs a
/// reply / clear my inbox / who needs me / my email / my messages / draft a
/// reply". This is the COMMS-TRIAGE half of communications, deliberately distinct
/// from veronica's CONTENT/COMPOSE vocabulary (the broad content/post/caption/
/// draft/write/copy/message/reply/tweet/email keyword cues veronica keeps below),
/// which Karen must NOT poach: veronica COMPOSES original content (a caption, a
/// post, a fresh message); Karen TRIAGES what came IN and drafts a reply to a
/// SPECIFIC inbound message. So Karen owns only the triage/inbox phrasings — the
/// bare "draft"/"message"/"reply" tokens (no inbox/triage framing) still reach
/// veronica. These are HIGH-PRECISION phrasings: the multi-word ones use plain
/// substring (each is already specific — "what needs a reply" / "catch me up on
/// messages" / "draft a reply" carry the inbound framing veronica's bare cues
/// lack), and the single tokens "triage"/"inbox"/"unread" are matched whole-word
/// in the select() chain because each is itself specific to comms triage. Order is
/// irrelevant (any match routes to karen).
const KAREN_MULTI_CUES: &[&str] = &[
    "catch me up on messages",
    "catch me up on my messages",
    "what needs a reply",
    "what needs my reply",
    "what needs replying",
    "who needs me",
    "who needs a reply",
    "clear my inbox",
    "clear out my inbox",
    "draft a reply",
    "draft a response",
    "my email",
    "my emails",
    "my messages",
];
const KAREN_WORD_CUES: &[&str] = &["triage", "inbox", "unread"];

/// Whether `lower` (already lowercased) is a KAREN comms-triage query. Pure and
/// unit-testable; consulted by [`AgentRegistry::select`] BEFORE the broad keyword
/// chain (where veronica owns the bare content/post/caption/draft/message/reply
/// tokens), so a triage/inbox request routes to the comms autopilot rather than to
/// veronica's compose side on a stray "draft" or "message" token. The boundary is
/// honest: Karen matches only the TRIAGE/INBOX phrasings (the inbound framing) — a
/// plain "draft a caption for this post" or "write a message" with no inbox/triage
/// cue still reaches veronica. Multi-word phrases use plain substring (already
/// specific); single tokens use the whole-word check so a cue cannot fire as a
/// substring of a larger word.
fn is_triage_query(lower: &str) -> bool {
    KAREN_MULTI_CUES.iter().any(|cue| lower.contains(cue))
        || KAREN_WORD_CUES.iter().any(|cue| contains_word(lower, cue))
}

/// DUM-E (Home & Environment) delegation cues: phrases that ask DARWIN to read or
/// control the smart home through the user's hub — "lights / thermostat / lock /
/// unlock / smart home / set the / scene / living room / bedroom / home
/// assistant", plus "turn on/off" SCOPED to a home-device context. This is the
/// DEVICE-CONTROL domain, deliberately distinct from jerome's media control
/// (music/play/volume) and oracle's app control (open/quit app), which dume must
/// NOT poach.
///
/// The hard part is "turn on/off": it is far too broad on its own ("turn on
/// do-not-disturb", "turn off notifications") to route to the home agent, so it is
/// claimed ONLY when a home-device noun is also present in the utterance ("turn on
/// the LIGHTS", "turn off the bedroom HEATER"). The unambiguous nouns/phrases
/// ("lights", "thermostat", "lock"/"unlock", "smart home", "home assistant",
/// "scene", "living room", "bedroom") route on their own; "set the" only routes
/// when a home noun is also present (so "set the timer" — jerome/pepper — does not
/// misroute). High-precision: single tokens use the whole-word check so a cue
/// cannot fire as a substring of a larger word. Order is irrelevant.
const DUME_MULTI_CUES: &[&str] = &[
    "smart home",
    "smart-home",
    "home assistant",
    "living room",
    "dining room",
];
/// Single-token home cues that route on their own (each is specific to a home
/// device/area; none collides with jerome's media or oracle's app vocabulary).
const DUME_WORD_CUES: &[&str] = &[
    "lights",
    "thermostat",
    "lock",
    "unlock",
    "scene",
    "bedroom",
    "hvac",
    "dimmer",
];
/// Home-DEVICE nouns that, when present, license the otherwise-too-broad verbs
/// ("turn on", "turn off", "set the"). Includes the singular/area words that are
/// NOT standalone cues above (e.g. bare "light" is too broad alone, but "turn on
/// the light" is clearly the home agent).
const DUME_DEVICE_CONTEXT: &[&str] = &[
    "light", "lights", "lamp", "thermostat", "heater", "heating", "ac",
    "air conditioner", "fan", "lock", "door", "blinds", "shades", "outlet",
    "plug", "switch", "bedroom", "living room", "kitchen", "garage", "scene",
    "thermostat", "hvac",
];
/// The broad verbs that only mean home control alongside a device-context noun.
const DUME_BROAD_VERBS: &[&str] = &["turn on", "turn off", "set the"];

/// Whether `lower` (already lowercased) is a DUM-E home-control/read query. Pure
/// and unit-testable; consulted by [`AgentRegistry::select`] before the broad
/// single-domain keyword chain so a "turn on the living room lights" routes to the
/// home agent rather than to jerome (media) or oracle (apps) on a stray token. The
/// boundary is honest and scoped: the unambiguous home nouns route on their own,
/// but the broad verbs ("turn on/off", "set the") route ONLY when a home-device
/// noun is also present — so "turn on do-not-disturb" or "set the timer" never
/// misroute here.
fn is_home_query(lower: &str) -> bool {
    if DUME_MULTI_CUES.iter().any(|cue| lower.contains(cue)) {
        return true;
    }
    if DUME_WORD_CUES.iter().any(|cue| contains_word(lower, cue)) {
        return true;
    }
    // A broad verb counts only with a home-device noun also present.
    let has_broad_verb = DUME_BROAD_VERBS.iter().any(|cue| lower.contains(cue));
    let has_device = DUME_DEVICE_CONTEXT.iter().any(|cue| contains_word(lower, cue));
    has_broad_verb && has_device
}

/// MIDAS (Personal Treasury) delegation cues: phrases that ask DARWIN to read the
/// user's OWN money — balances, spending, transactions, cash flow, net worth,
/// "where's my money", "how much did I spend", "my accounts", "my budget". This is
/// PERSONAL FINANCE, deliberately distinct from gecko's MARKET watch
/// (market/trade/stock/crypto/portfolio/ticker), which Midas must NOT poach: gecko
/// quotes the live tape and researches trades; Midas reads the user's bank balances
/// and tells them where their cash went. The boundary is whose money: Midas owns the
/// user's accounts/spending; gecko owns the markets. Multi-word phrases are already
/// specific (plain substring is enough); the single-token cues ("balance",
/// "balances", "spending", "transactions") are matched whole-word in the select()
/// chain so a cue cannot fire as a substring of a larger word. "portfolio" is
/// deliberately NOT a Midas cue — it stays gecko's, so an investment-portfolio
/// question is not stolen by personal finance. HARD RULE: Midas READS only — it can
/// never move money, and the persona/copy say so. Order is irrelevant (any match
/// routes to midas).
const MIDAS_MULTI_CUES: &[&str] = &[
    "how much did i spend",
    "how much have i spent",
    "where's my money",
    "wheres my money",
    "where is my money",
    "where did my money go",
    "where does my money go",
    "my accounts",
    "my bank",
    "my checking",
    "my savings",
    "cash flow",
    "net worth",
    "how much money do i have",
    "my balance",
    "account balance",
];
const MIDAS_WORD_CUES: &[&str] = &[
    "balance",
    "balances",
    "spending",
    "transactions",
    "budget",
];

/// Whether `lower` (already lowercased) is a MIDAS personal-finance query. Pure and
/// unit-testable; consulted by [`AgentRegistry::select`] BEFORE the broad
/// single-domain keyword chain (where gecko owns the market/trade tokens), so a
/// "what's my balance" / "how much did I spend" routes to the treasury reader rather
/// than to gecko's market watch. The boundary is honest: Midas matches only the
/// PERSONAL-finance phrasings — a plain "what's the market doing" or "pull up the
/// stock price" still reaches gecko (Midas claims no market/trade/stock/crypto/
/// portfolio/ticker token). Multi-word phrases use plain substring (already
/// specific); single tokens use the whole-word check so a cue cannot fire as a
/// substring of a larger word.
fn is_personal_finance_query(lower: &str) -> bool {
    MIDAS_MULTI_CUES.iter().any(|cue| lower.contains(cue))
        || MIDAS_WORD_CUES.iter().any(|cue| contains_word(lower, cue))
}

/// VOYAGER (Travel & Logistics) delegation cues: phrases that ask DARWIN to find
/// the WAY — driving/walking directions, a route, how long a trip takes (ETA /
/// travel time), how far somewhere is, where the nearest thing is ("coffee near
/// me", "restaurant near the office", "find a pharmacy nearby"). This is the
/// READ-ONLY maps/routes/places domain, deliberately distinct from gecko's
/// markets, friday's schedule, and oracle's apps — Voyager reads a maps provider
/// and tells the user the route, the place, and the time on the road. HARD SCOPE:
/// Voyager does NOT book or pay for ANYTHING (no flights, no hotels, no rides) —
/// it has no reservation or payment tool, so a "book me a flight" request is NOT
/// claimed here; only the routes/places/ETA reads are. The cues are the navigation
/// vocabulary: the multi-word phrases ("how long to get", "travel time", "coffee
/// near", "restaurant near", "find a") are already specific (plain substring), and
/// the single tokens ("directions", "route", "navigate", "nearby", "eta",
/// "map"/"maps") are matched whole-word in the select() chain so a cue cannot fire
/// as a substring of a larger word. "how far" is a multi-word phrase. Order is
/// irrelevant (any match routes to voyager).
const VOYAGER_MULTI_CUES: &[&str] = &[
    "how long to get",
    "how long does it take to get",
    "how far",
    "travel time",
    "coffee near",
    "restaurant near",
    "find a",
];
const VOYAGER_WORD_CUES: &[&str] = &[
    "directions",
    "route",
    "navigate",
    "nearby",
    "eta",
    "map",
    "maps",
];

/// Whether `lower` (already lowercased) is a VOYAGER travel/logistics query. Pure
/// and unit-testable; consulted by [`AgentRegistry::select`] after the other
/// high-precision specialists and BEFORE the broad single-domain keyword chain so a
/// "directions to the airport" / "coffee near me" / "how long to get downtown"
/// routes to the navigator rather than to a stray domain keyword. The boundary is
/// honest and READ-ONLY: Voyager owns routes, places, and travel times — it does
/// NOT book or pay for anything, so a booking/payment request is never claimed
/// here. Multi-word phrases use plain substring (already specific); single tokens
/// use the whole-word check so a cue cannot fire as a substring of a larger word.
fn is_travel_query(lower: &str) -> bool {
    VOYAGER_MULTI_CUES.iter().any(|cue| lower.contains(cue))
        || VOYAGER_WORD_CUES.iter().any(|cue| contains_word(lower, cue))
}

/// AEGIS (Defense & Privacy) delegation cues: phrases that ask DARWIN whether the
/// user is EXPOSED — "have I been pwned", "breach"/"breached", "am I exposed",
/// "data leak", "my passwords leaked", "security posture", "am I protected",
/// "privacy check", "filevault". This is the DEFENSIVE EXPOSURE/PRIVACY domain —
/// checking the user's OWN breach exposure and the LOCAL machine's read-only
/// posture — deliberately distinct from ultron's SECURITY MONITORING vocabulary
/// (monitor/monitoring/threat/intrusion/firewall/defend/defensive/lockdown), which
/// Aegis must NOT poach. The boundary the contract pins: ultron watches the Mac/LAN
/// for ongoing threats (live monitoring); Aegis answers "where am I exposed" — a
/// breach check on the user's own email and a posture read of this machine. So Aegis
/// owns ONLY the exposure/privacy phrasings and deliberately leaves ultron's
/// "firewall"/"threat"/"monitor" tokens alone. These are HIGH-PRECISION: the
/// multi-word phrases ("have i been pwned", "am i exposed", "data leak", "security
/// posture", "am i protected", "privacy check", "passwords leaked") use plain
/// substring (each is already specific), and the single tokens ("breach",
/// "breached", "pwned", "filevault") are matched whole-word in the select() chain so
/// a cue cannot fire as a substring of a larger word. DEFENSIVE-ONLY: Aegis checks
/// the user's OWN assets — it never scans another host. Order is irrelevant (any
/// match routes to aegis).
const AEGIS_MULTI_CUES: &[&str] = &[
    "have i been pwned",
    "am i exposed",
    "am i protected",
    "data leak",
    "data breach",
    "security posture",
    "privacy check",
    "passwords leaked",
    "password leaked",
    "passwords been leaked",
    "my passwords leaked",
];
const AEGIS_WORD_CUES: &[&str] = &[
    "breach",
    "breached",
    "breaches",
    "pwned",
    "filevault",
];

/// Whether `lower` (already lowercased) is an AEGIS exposure/privacy query. Pure and
/// unit-testable; consulted by [`AgentRegistry::select`] BEFORE the broad
/// single-domain keyword chain (where ultron owns the security/monitor/threat/
/// firewall MONITORING tokens), so a "have I been pwned" / "am I exposed" / "check
/// my security posture" routes to the defense-and-privacy specialist rather than to
/// ultron's live monitoring. The boundary is honest: Aegis matches only the
/// EXPOSURE/PRIVACY phrasings — a plain "monitor for threats" or "is the firewall
/// up" still reaches ultron (Aegis claims no monitor/threat/firewall/intrusion
/// token). DEFENSIVE-ONLY: Aegis checks the user's own email + own machine; it never
/// scans another host. Multi-word phrases use plain substring (already specific);
/// single tokens use the whole-word check so a cue cannot fire as a substring of a
/// larger word.
fn is_exposure_query(lower: &str) -> bool {
    AEGIS_MULTI_CUES.iter().any(|cue| lower.contains(cue))
        || AEGIS_WORD_CUES.iter().any(|cue| contains_word(lower, cue))
}

/// BABEL (Translation & Interpretation) delegation cues: phrases that ask DARWIN
/// to RENDER text between languages — "translate this", "in spanish"/"in french"/
/// "in <language>", "how do you say", "what does X mean in", "say this in",
/// "interpret". This is the TRANSLATION domain — turning words from one tongue
/// into another — and it does NOT poach any sibling: veronica COMPOSES original
/// content (the bare draft/write/message tokens) and karen TRIAGES the inbox, but
/// neither owns "translate"; mnemosyne "interprets" nothing (it recalls). The
/// hard part is the bare "in <language>" pattern: "in spanish" alone is too thin
/// to claim a turn on its own (it can trail an unrelated sentence), so the
/// language names are recognized as cues only when a translation/rendering VERB is
/// also present ("say this IN spanish", "how do you say hello IN french",
/// "translate it INTO german") — the explicit "translate"/"translation"/"how do
/// you say"/"what does ... mean in"/"say ... in"/"interpret" phrasings route on
/// their own. High-precision: the multi-word phrases use plain substring (each is
/// already specific) and the single tokens ("translate"/"translation"/"interpret"
/// /"interpreter") are matched whole-word so a cue cannot fire as a substring of a
/// larger word ("interpretation of the data" is gated by the whole-word check on
/// "interpret" — it fires there, which is acceptable: a translation agent reading
/// "interpret this" is the intended owner; the language-gated path keeps the
/// broad "in <lang>" from over-claiming). Order is irrelevant (any match routes to
/// babel).
const BABEL_MULTI_CUES: &[&str] = &[
    "translate",
    "translation",
    "how do you say",
    "how would you say",
    "what does",          // paired with "mean in" below for the "what does X mean in Y" form
    "say this in",
    "say that in",
    "put this in",
];
/// Single-token BABEL cues, matched whole-word: each is itself specific to
/// interpretation between languages.
const BABEL_WORD_CUES: &[&str] = &[
    "interpret",
    "interpreter",
];
/// Translation/rendering VERBS that license the otherwise-too-broad "in <language>"
/// pattern: a bare language name ("spanish") claims the turn only when one of these
/// is also present, so "I lived in Spain and learned spanish" does not misroute,
/// but "say hello in spanish" / "render this into french" does.
const BABEL_RENDER_VERBS: &[&str] = &[
    "say", "translate", "render", "write", "put", "how do you say",
    "how would you say",
];
/// Language names that, alongside a render verb, mark a translation request. Common
/// languages only — the explicit verbs above already carry most requests; this just
/// catches the "<verb> ... in <language>" shape. Matched whole-word.
const BABEL_LANGUAGES: &[&str] = &[
    "spanish", "french", "german", "italian", "portuguese", "dutch", "russian",
    "chinese", "mandarin", "cantonese", "japanese", "korean", "arabic", "hindi",
    "hebrew", "greek", "latin", "polish", "turkish", "swedish", "norwegian",
    "danish", "finnish", "czech", "vietnamese", "thai", "indonesian", "tagalog",
    "swahili", "ukrainian", "romanian", "hungarian",
];

/// Whether `lower` (already lowercased) is a BABEL translation/interpretation
/// query. Pure and unit-testable; consulted by [`AgentRegistry::select`] BEFORE the
/// broad single-domain keyword chain so a "translate this into spanish" / "how do
/// you say hello in french" routes to the translation specialist rather than to
/// veronica on a stray "write" token. The explicit translate/interpret phrasings
/// match on their own; the bare "in <language>" pattern matches only alongside a
/// rendering verb so it never over-claims. The "what does X mean in Y" form needs
/// both "what does" and "mean in" present (a meaning-in-another-language ask).
fn is_translation_query(lower: &str) -> bool {
    // Explicit translate/translation always wins; the other multi-cues do too,
    // except "what does", which requires the "mean in" partner to be a translation.
    for cue in BABEL_MULTI_CUES {
        if *cue == "what does" {
            if lower.contains("what does") && lower.contains("mean in") {
                return true;
            }
            continue;
        }
        if lower.contains(cue) {
            return true;
        }
    }
    if BABEL_WORD_CUES.iter().any(|cue| contains_word(lower, cue)) {
        return true;
    }
    // The "<render verb> ... in/into <language>" shape: a language name plus a
    // rendering verb. "in"/"into" is implied by the verb+language pairing.
    let has_language = BABEL_LANGUAGES.iter().any(|cue| contains_word(lower, cue));
    if has_language {
        let has_verb = BABEL_RENDER_VERBS.iter().any(|cue| {
            if cue.contains(' ') {
                lower.contains(cue)
            } else {
                contains_word(lower, cue)
            }
        });
        if has_verb {
            return true;
        }
    }
    false
}

/// The parsed registry: the 27 agents plus the index of the orchestrator
/// (darwin), which is the fallback for any unmatched request and the owner of
/// every tool. Construction guarantees the orchestrator exists and that the
/// roster is internally consistent (validated by [`AgentRegistry::validate`]).
#[derive(Debug, Clone)]
pub struct AgentRegistry {
    agents: Vec<Agent>,
    /// Index into `agents` of the orchestrator (darwin). Always valid.
    orchestrator: usize,
}

/// The TOML document shape: `[[agent]]` array of tables.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentsDoc {
    #[serde(rename = "agent")]
    agents: Vec<Agent>,
}

impl AgentRegistry {
    /// Load config/agents.toml, falling back to the canonical roster when the
    /// file is missing, unreadable, malformed, or fails validation — so the
    /// daemon always runs with the full team. Returns the registry plus a list
    /// of human-readable issues (re-emitted as telemetry by the caller once
    /// the hub exists, mirroring Config::load).
    pub fn load(path: &Path) -> (AgentRegistry, Vec<String>) {
        match std::fs::read_to_string(path) {
            Ok(raw) => Self::parse(&raw),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                warn!(path = %path.display(), "agents.toml missing; using canonical roster");
                (Self::canonical(), Vec::new())
            }
            Err(e) => {
                let issue = format!("agents.toml unreadable ({e}); using canonical roster");
                warn!(path = %path.display(), "{issue}");
                (Self::canonical(), vec![issue])
            }
        }
    }

    /// Parse a TOML document. A syntax error, an invalid `[[agent]]` table
    /// (unknown key, missing field, wrong type), or a validation failure all
    /// fall back whole to the canonical roster with a reported issue — a
    /// partial team is never half-loaded.
    fn parse(raw: &str) -> (AgentRegistry, Vec<String>) {
        let doc: AgentsDoc = match toml::from_str(raw) {
            Ok(doc) => doc,
            Err(e) => {
                let issue = format!("agents.toml invalid ({e}); using canonical roster");
                warn!("{issue}");
                return (Self::canonical(), vec![issue]);
            }
        };
        match Self::from_agents(doc.agents) {
            Ok(registry) => (registry, Vec::new()),
            Err(e) => {
                let issue = format!("agents.toml failed validation ({e}); using canonical roster");
                warn!("{issue}");
                (Self::canonical(), vec![issue])
            }
        }
    }

    /// Build a validated registry from a list of agents. Errors when the
    /// roster is empty, lacks the orchestrator, has a duplicate name, an
    /// out-of-range hue, a malformed namespace, or an empty tools list.
    fn from_agents(agents: Vec<Agent>) -> Result<AgentRegistry, String> {
        Self::validate(&agents)?;
        let orchestrator = agents
            .iter()
            .position(Agent::is_orchestrator)
            .ok_or("no orchestrator agent (none holds the tools wildcard)")?;
        Ok(AgentRegistry { agents, orchestrator })
    }

    /// Roster invariants every registry must satisfy, whether parsed or
    /// canonical. Catching these at load keeps the rest of the daemon free of
    /// defensive checks.
    fn validate(agents: &[Agent]) -> Result<(), String> {
        if agents.is_empty() {
            return Err("roster is empty".to_string());
        }
        let mut seen = HashSet::new();
        let mut orchestrators = 0;
        for a in agents {
            if a.name.trim().is_empty() {
                return Err("an agent has an empty name".to_string());
            }
            if !seen.insert(a.name.as_str()) {
                return Err(format!("duplicate agent name '{}'", a.name));
            }
            if a.hue > 360 {
                return Err(format!("agent '{}' hue {} out of range 0-360", a.name, a.hue));
            }
            if a.voice.trim().is_empty() {
                return Err(format!("agent '{}' has an empty voice", a.name));
            }
            if a.tools.is_empty() {
                return Err(format!("agent '{}' has an empty tools list", a.name));
            }
            let expected_ns = format!("agent.{}", a.name);
            if a.namespace != expected_ns {
                return Err(format!(
                    "agent '{}' namespace '{}' must be '{expected_ns}'",
                    a.name, a.namespace
                ));
            }
            if a.is_orchestrator() {
                orchestrators += 1;
            }
        }
        if orchestrators == 0 {
            return Err("no orchestrator (none holds the tools wildcard)".to_string());
        }
        Ok(())
    }

    /// All agents, in declaration order — which is roll-call order (darwin
    /// first, then the team) by contract.
    pub fn all(&self) -> &[Agent] {
        &self.agents
    }

    /// The orchestrator (darwin): the delegation fallback and tool owner.
    pub fn orchestrator(&self) -> &Agent {
        &self.agents[self.orchestrator]
    }

    /// A grounded one-block summary of the live constellation — every agent and
    /// its role, the orchestrator marked "(you)" — for the cloud conversation
    /// prompt. The cloud persona has no static roster, so without this the cloud
    /// brain (correctly, per its no-fabrication grounding) DENIES having a team
    /// when asked. Feeding it the REAL roster lets DARWIN accurately name/list/
    /// describe the constellation it orchestrates. Profiles on one engine — the
    /// persona prompt frames the "not separate minds" honesty.
    pub fn roster_brief(&self) -> String {
        let mut s = String::from(
            "Your constellation — the agents you orchestrate (each a distinct \
             persona, voice, and tool scope; all profiles on ONE engine, not \
             separate minds — never imply more capability than that):",
        );
        for a in &self.agents {
            s.push_str("\n- ");
            s.push_str(&a.name);
            if a.is_orchestrator() {
                s.push_str(" (you, Prime Orchestrator)");
            }
            if !a.role.trim().is_empty() {
                s.push_str(" — ");
                s.push_str(a.role.trim());
            }
        }
        s
    }

    /// A plain, GROUNDED spoken roster for the offline fallback: when the cloud
    /// brain is unreachable, DARWIN still names the real team (every agent + its
    /// short role) deterministically — accurate, never hallucinated. The cloud
    /// path phrases the same roster with more wit; this is the safety floor so an
    /// agent-roster query is NEVER answered by the local model guessing.
    pub fn roster_spoken(&self) -> String {
        let parts: Vec<String> = self
            .agents
            .iter()
            .filter(|a| !a.is_orchestrator())
            .map(|a| {
                // Roles read "Short label: detail" — the label before the colon
                // is the speakable summary.
                let short = a.role.split(':').next().unwrap_or("").trim();
                if short.is_empty() {
                    a.name.clone()
                } else {
                    format!("{} for {}", a.name, short)
                }
            })
            .collect();
        format!(
            "You have {} agents in the constellation, sir. {}. And myself, keeping the lot of them in line.",
            self.agents.len(),
            parts.join("; ")
        )
    }

    /// Look up an agent by exact (lowercase) name.
    pub fn get(&self, name: &str) -> Option<&Agent> {
        self.agents.iter().find(|a| a.name == name)
    }

    /// The first agent (other than the orchestrator) whose allowlist contains
    /// `tool` — used to name where a denied tool actually belongs, so an
    /// out-of-domain attempt can be routed to its owner. None when only the
    /// orchestrator holds it.
    pub fn owner_of(&self, tool: &str) -> Option<&Agent> {
        self.agents
            .iter()
            .find(|a| !a.is_orchestrator() && a.tools.iter().any(|t| t == tool))
    }

    /// Darwin-Prime delegation: pick the agent that should handle this
    /// request. Deterministic and unit-testable — the classifier intent plus
    /// keyword cues map to a specialist; anything unmatched falls to the
    /// orchestrator (darwin). The rule map is intentionally ordered: the first
    /// matching specialist wins, so more specific cues (code/bug -> steve)
    /// are checked before broader ones.
    ///
    /// Resolution order:
    ///   1. Offline survival: if the cloud is unreachable, hulk owns the turn.
    ///   2. Intent-driven: app/web/file/system/memory intents map to the
    ///      specialist that owns those tools.
    ///   3. Keyword-driven: domain cues in the utterance pick a specialist.
    ///   4. Fallback: the orchestrator (darwin).
    ///      The chosen agent must still hold the tool it will invoke; the router
    ///      enforces that and re-routes on a violation (isolation).
    pub fn select(&self, intent: &str, text: &str, cloud_reachable: bool) -> &Agent {
        // 1. Cloud down -> the all-local survival profile, but only for the
        //    conversational/heavy cases that would otherwise need the cloud;
        //    concrete local-action intents still go to their owners below so
        //    "open safari" keeps working offline.
        if !cloud_reachable && matches!(intent, "conversation") {
            if let Some(a) = self.get("hulk") {
                return a;
            }
        }

        // MNEMOSYNE, Semantic Memory, owns RETRIEVAL phrasings — "what did I say
        // about X", "what do you remember about Y", "dig up that note", "have we
        // discussed". Checked BEFORE the intent-driven map because a retrieval
        // utterance the classifier labels `memory.recall` would otherwise be
        // claimed by pepper (the STORE/EA owner of the memory.recall intent);
        // retrieval is the more specific memory intent, so the recall specialist
        // wins it. These cues are RETRIEVAL phrasings only and deliberately do
        // NOT include pepper's bare STORE cues ("remember to", "set a reminder"),
        // so the two halves of memory stay cleanly split: pepper writes,
        // Mnemosyne reads. The lowercased text is needed early for this check.
        let lower = text.to_lowercase();
        if is_recall_query(&lower) {
            if let Some(a) = self.get("mnemosyne") {
                return a;
            }
        }

        // 2. Intent-driven mapping to the owning specialist.
        let by_intent = match intent {
            "app.launch" | "app.control" => Some("oracle"),
            "web.open" | "file.op" => Some("vision"),
            "system.query" => Some("ultron"),
            "memory.store" | "memory.recall" => Some("pepper"),
            // ON-DEVICE FILE RAG: (re)building and forgetting the user's OWN file
            // index is MNEMOSYNE's recollective remit (she also owns doc_search),
            // so the index/forget triggers route to her — keeping the HUD agent
            // attribution honest and the file-RAG surface in one agent.
            "docsearch.index" | "docsearch.forget" => Some("mnemosyne"),
            // KNOWLEDGE GRAPH: building/mapping the user's indexed documents into
            // the shared world model is MNEMOSYNE's knowledge-keeping remit (she
            // owns the file-RAG surface AND the world-model write). Ships ON
            // ([docsearch].build_graph) but INERT WITHOUT indexed docs; routes to her so the turn is
            // attributed to the knowledge agent.
            "docsearch.build_graph" | "knowledge.build" => Some("mnemosyne"),
            _ => None,
        };
        if let Some(name) = by_intent {
            if let Some(a) = self.get(name) {
                return a;
            }
        }

        // 3. Keyword cues (lowercased word-boundary contains). Order matters:
        //    earlier rules are more specific. Each maps to an agent that owns
        //    the relevant tools, so the router never has to re-route a
        //    keyword match.
        let has = |needles: &[&str]| needles.iter().any(|n| contains_word(&lower, n));

        // EDITH, the Proactive Sentinel, owns anticipation cues — what's coming,
        // what to watch, what the user should know. These are HIGH-PRECISION
        // multi-word phrases (substring is enough; the phrase is already
        // specific) checked BEFORE the broad keyword chain so they win, and they
        // deliberately AVOID friday's brief/morning/news intel cues. Edith is
        // the FIRST specialist consulted because "anticipate" is the most
        // specific intent in the team.
        if is_anticipation_query(&lower) {
            if let Some(a) = self.get("edith") {
                return a;
            }
        }

        // FURY, the Mission Orchestrator, owns MULTI-STEP mission cues — assemble
        // the team, run point, handle all of X end to end. Checked right after
        // EDITH and BEFORE the single-domain keyword chain so a multi-step goal
        // that mentions, say, "the campaign news and a draft post" routes to FURY
        // (which will decompose and dispatch the pieces) rather than to whichever
        // lone domain keyword (news -> friday, post -> veronica) happens to match
        // first. These are high-precision phrases that do not poach the roll-call
        // greeting cues.
        if is_mission_query(&lower) {
            if let Some(a) = self.get("fury") {
                return a;
            }
        }

        // CASSANDRA, Forecast & Simulation, owns MODELING cues — simulate,
        // forecast, scenario, monte carlo, what-if, odds, probability. Checked
        // after EDITH/FURY and BEFORE the single-domain keyword chain so a
        // "simulate this stock over a year" or "what are the odds" routes to the
        // modeler rather than to gecko's live MARKET watch on a stray ticker
        // word. These are modeling verbs (what COULD happen under assumptions),
        // deliberately distinct from gecko's market/trade/stock cues (the live
        // tape) — Cassandra runs the numbers, she does not quote the market.
        if is_forecast_query(&lower) {
            if let Some(a) = self.get("cassandra") {
                return a;
            }
        }

        // SAGE, Deep Research, owns THOROUGH, CITED, multi-source investigation —
        // "deep dive", "research report", "look into X thoroughly", "with
        // citations", "everything about". Checked after the other high-precision
        // specialists and BEFORE the broad keyword chain so a cited deep dive
        // routes to the deep-research specialist rather than to vision's QUICK
        // lookup on the stray "research" inside it. The boundary the contract
        // pins: vision = a fast lookup on authorized targets (it keeps the bare
        // "research/osint/lookup/investigate/footprint" tokens below); SAGE = a
        // bounded multi-source report whose every claim is cited. SAGE matches
        // only the DEEP/CITED variant, so a plain "research the competitors" with
        // no depth/citation cue still reaches vision.
        if is_deep_research_query(&lower) {
            if let Some(a) = self.get("sage") {
                return a;
            }
        }

        // VITALIS, Health & Biometrics, owns the BODY-SIGNAL reads — recovery,
        // strain, HRV, sleep score, readiness, resting heart rate, "how recovered
        // am I", "how did I sleep", "whoop". Checked after the other
        // high-precision specialists and BEFORE the broad keyword chain so a
        // biometrics question routes to the biometrics specialist rather than to
        // hercules on a stray fitness word. The boundary the contract pins:
        // hercules COACHES (programs the workout + the nutrition — it keeps the
        // workout/exercise/training/nutrition/diet/macros/fitness/lift/run
        // tokens below); Vitalis READS what the band measured. So Vitalis matches
        // only the BIOMETRIC-reading phrasings — a plain "plan my workout" or
        // "what should I eat" still reaches hercules.
        if is_biometric_query(&lower) {
            if let Some(a) = self.get("vitalis") {
                return a;
            }
        }

        // KAREN, Comms Autopilot, owns the TRIAGE/INBOX reads — triage, inbox,
        // unread, "catch me up on messages", "what needs a reply", "clear my
        // inbox", "who needs me", "my email", "draft a reply". Checked after the
        // other high-precision specialists and BEFORE the broad keyword chain so a
        // triage request routes to the comms autopilot rather than to veronica on
        // the stray "message"/"reply"/"email"/"draft" tokens she keeps below. The
        // boundary the contract pins: veronica COMPOSES original content (caption/
        // post/fresh message — she keeps the content/post/caption/draft/write/copy/
        // message/reply/tweet/email tokens); Karen TRIAGES the INBOUND and drafts a
        // reply to a specific message. So Karen matches only the triage/inbox
        // phrasings — a plain "draft a caption for this post" still reaches veronica.
        //
        // AEGIS, Defense & Privacy, is checked JUST BEFORE Karen so an exposure
        // phrasing that happens to mention email ("has my email been breached", "was
        // my email in a breach") routes to the defense-and-privacy specialist rather
        // than being swept up by Karen's "my email" triage cue — the breach/pwned/
        // exposed framing is the more specific intent. Aegis owns the EXPOSURE/PRIVACY
        // reads — "have I been pwned", breach/breached/pwned, "am I exposed", "data
        // leak", "security posture", "am I protected", "privacy check", filevault —
        // and is also checked BEFORE the broad keyword chain so an exposure question
        // routes here rather than to ultron on the security/firewall MONITORING tokens
        // ultron keeps below. The boundary the contract pins: ultron MONITORS the Mac/
        // LAN for live threats (it keeps the monitor/monitoring/threat/intrusion/
        // firewall/defend/defensive/lockdown tokens); Aegis answers "where am I
        // exposed" — a breach check on the user's OWN email and a read-only posture
        // report of THIS machine. So Aegis matches only the exposure/privacy phrasings
        // — a plain "monitor for threats" or "is the firewall up" still reaches
        // ultron, and Karen's pure triage cues ("triage my inbox", "what needs a
        // reply") carry no exposure token, so they stay with Karen. DEFENSIVE-ONLY:
        // the user's own assets, never another host.
        if is_exposure_query(&lower) {
            if let Some(a) = self.get("aegis") {
                return a;
            }
        }

        if is_triage_query(&lower) {
            if let Some(a) = self.get("karen") {
                return a;
            }
        }

        // BABEL, Translation & Interpretation, owns the RENDER-between-languages
        // requests — "translate this", "in <language>", "how do you say", "what
        // does X mean in Y", "interpret". Checked after the other high-precision
        // specialists and BEFORE the broad keyword chain so a "translate this into
        // spanish" / "say hello in french" routes to the translator rather than to
        // veronica on the stray "write"/"say"/"message" tokens she keeps below. The
        // boundary the contract pins: veronica COMPOSES original content and karen
        // TRIAGES the inbox — neither translates; babel turns words from one tongue
        // into another by calling the on-device LLM. The bare "in <language>"
        // pattern is gated on a rendering verb so an unrelated "I learned spanish in
        // Spain" never misroutes, while "say it in spanish" does. HONESTY: text
        // translation is bounded by the local ~4B model (competent, not a dedicated
        // MT system), and the LIVE speech-interpreter loop is device/audio-gated.
        if is_translation_query(&lower) {
            if let Some(a) = self.get("babel") {
                return a;
            }
        }

        // DUM-E, Home & Environment, owns the SMART-HOME reads + controls — lights,
        // thermostat, lock/unlock, scene, "smart home", "home assistant", area
        // names, and "turn on/off"/"set the" SCOPED to a home device. Checked after
        // the other high-precision specialists and BEFORE the broad keyword chain
        // so a home request routes to the home agent rather than to jerome (media:
        // music/play/volume) or oracle (apps: open/quit) on a stray "turn on" /
        // "set the". The boundary the contract pins: jerome controls MEDIA and
        // oracle controls APPS; dume controls the user's smart-home DEVICES through
        // their hub. The broad verbs only route here alongside a home-device noun,
        // so "turn off the music" stays with jerome and "set a timer" stays
        // elsewhere. HONESTY: control goes through the user's OWN Home Assistant
        // hub, not HomeKit directly.
        if is_home_query(&lower) {
            if let Some(a) = self.get("dume") {
                return a;
            }
        }

        // MIDAS, Personal Treasury, owns the PERSONAL-finance reads — balance/
        // balances/spending/transactions/budget, "how much did I spend", "my
        // accounts", "cash flow", "net worth", "where's my money". Checked after the
        // other high-precision specialists and BEFORE the broad keyword chain so a
        // personal-money question routes to the treasury reader rather than to gecko
        // on the market/trade tokens gecko keeps below. The boundary the contract
        // pins: gecko watches the MARKETS (market/trade/stock/crypto/portfolio/
        // ticker — the live tape); Midas reads the USER's own bank balances + spending
        // and says where the cash went. So Midas matches only the personal-finance
        // phrasings — a plain "what's the market doing" or "pull up the stock price"
        // still reaches gecko. HARD RULE: Midas READS only — it never moves money, and
        // it holds no transfer/payment/trade tool, not even a gated one.
        if is_personal_finance_query(&lower) {
            if let Some(a) = self.get("midas") {
                return a;
            }
        }

        // VOYAGER, Travel & Logistics, owns the READ-ONLY maps/routes/places reads —
        // directions/route/navigate, "how long to get"/"travel time"/"eta", "how
        // far", "nearby"/"coffee near"/"restaurant near"/"find a", "map"/"maps".
        // Checked after the other high-precision specialists and BEFORE the broad
        // keyword chain so a "directions to the airport" / "coffee near me" / "how
        // long to get downtown" routes to the navigator rather than to a stray domain
        // keyword. The boundary the contract pins: Voyager reads ROUTES, PLACES, and
        // TRAVEL TIMES off a maps provider — it does NOT book or pay for anything (no
        // flights, no hotels, no rides), so a "book me a flight" request is never
        // claimed here. HONESTY: read-only; booking/payment is out of scope.
        if is_travel_query(&lower) {
            if let Some(a) = self.get("voyager") {
                return a;
            }
        }

        let by_keyword = if has(&["bug", "build", "compile", "code", "pr", "repo", "refactor", "deploy", "patch"]) {
            Some("steve")
        } else if has(&["research", "competitor", "competitors", "trend", "trends", "osint", "ad", "ads", "investigate", "lookup", "footprint"]) {
            Some("vision")
        } else if has(&["news", "brief", "briefing", "morning", "overnight", "headline", "headlines", "schedule", "agenda"]) {
            Some("friday")
        } else if has(&["content", "post", "caption", "draft", "write", "copy", "message", "reply", "tweet", "email"]) {
            Some("veronica")
        } else if has(&["security", "monitor", "monitoring", "threat", "intrusion", "firewall", "defend", "defensive", "lockdown"]) {
            Some("ultron")
        } else if has(&["market", "markets", "trade", "trading", "stock", "stocks", "crypto", "portfolio", "ticker"]) {
            Some("gecko")
        } else if has(&["workout", "exercise", "training", "train", "nutrition", "diet", "macros", "fitness", "lift", "run"]) {
            Some("hercules")
        } else if has(&["meeting", "meetings", "notes", "minutes", "standup", "sync"]) {
            Some("herald")
        } else if has(&["music", "play", "song", "playlist", "track", "dj", "volume", "tune"]) {
            Some("jerome")
        } else if has(&["remind", "reminder", "remember", "schedule", "calendar", "reflect", "consolidate"]) {
            Some("pepper")
        } else if has(&["workflow", "workflows", "automate", "automation", "chain", "routine"]) {
            Some("oracle")
        } else if has(&["chapter", "rush", "fraternity", "sorority", "greek", "pledge", "social"]) {
            // Greek-life cues are more specific than stark's broad "strategy".
            Some("athena")
        } else if has(&["business", "metrics", "kpi", "revenue", "decision", "strategy"]) {
            Some("stark")
        } else {
            None
        };
        if let Some(name) = by_keyword {
            if let Some(a) = self.get(name) {
                return a;
            }
        }

        // 4. Anything unmatched is the orchestrator's: darwin handles it.
        self.orchestrator()
    }

    /// SMARTER delegation: run the fast, authoritative [`select`] first, then —
    /// ONLY when it fell through to the orchestrator default for a non-trivial
    /// CONVERSATION request (no deterministic cue matched) — try a SEMANTIC
    /// fallback that picks the best-matching specialist by similarity of the
    /// utterance to each agent's role/cues text via the injected `scorer`.
    ///
    /// This is purely additive and never a worse outcome than [`select`] alone:
    ///   - The deterministic intent map + keyword cues stay the FIRST PASS and
    ///     remain authoritative: if `select` already chose a specialist (intent,
    ///     keyword, recall, anticipation, offline-hulk, …), that pick is returned
    ///     UNCHANGED — the fallback never overrides a cue match.
    ///   - The fallback engages ONLY for `intent == "conversation"` that landed
    ///     on the orchestrator. A concrete-action intent (`app.launch`,
    ///     `system.query`, …) is already owned deterministically and is left
    ///     alone, so the fallback can never re-aim an action at a specialist that
    ///     does not hold its tool.
    ///   - A blank/trivial utterance, a weak/tied semantic signal, an
    ///     unavailable backend (the scorer returns no confident pick), or a pick
    ///     that resolves to the orchestrator all DEGRADE to the orchestrator —
    ///     the same safe default as today.
    ///
    /// SAFETY: this only changes WHICH agent is delegated to (the persona/voice/
    /// namespace the turn runs under). It does NOT touch tool allowlists, the
    /// confirmation gate, or memory-namespace isolation: the router still resolves
    /// the tool this turn will invoke and enforces the chosen agent's allowlist
    /// (`enforce_tool`), and a consequential action still parks at the gate. The
    /// fallback can only pick a non-orchestrator SPECIALIST for a conversational
    /// turn; it can never hand a consequential tool to an agent that lacks it
    /// (the allowlist re-route would catch that anyway), so no guard is bypassed.
    pub fn select_with_fallback<S: AgentScorer>(
        &self,
        intent: &str,
        text: &str,
        cloud_reachable: bool,
        scorer: &S,
    ) -> &Agent {
        // FIRST PASS, authoritative: the deterministic intent map + keyword cues.
        let chosen = self.select(intent, text, cloud_reachable);

        // The fallback engages ONLY when the deterministic pass produced no
        // specialist — i.e. it fell through to the orchestrator default — AND the
        // turn is a plain CONVERSATION (a concrete-action intent is already owned
        // by its tool's specialist and must not be re-aimed by similarity, which
        // could land it on an agent without that tool).
        if !chosen.is_orchestrator() || intent != "conversation" {
            return chosen;
        }

        // Trivial / empty utterances carry no semantic signal: stay on the safe
        // default rather than letting a blank string score noise.
        if text.trim().is_empty() {
            return chosen;
        }

        // SEMANTIC PASS: ask the injected scorer for the best specialist. The
        // scorer is pure/deterministic and degrades to None on a weak or tied
        // signal or an unavailable backend — in which case we keep the
        // orchestrator (never a worse outcome than `select` alone).
        match self.semantic_pick(text, scorer) {
            // A confident specialist pick that still exists in the roster and is
            // NOT the orchestrator: delegate to it.
            Some(agent) if !agent.is_orchestrator() => agent,
            // No confident pick, a tie, or it resolved to the orchestrator: the
            // safe default.
            _ => self.orchestrator(),
        }
    }

    /// The semantic fallback's core: score the utterance against every
    /// NON-orchestrator agent's role/cues text and return the single best
    /// specialist, or `None` when the signal is absent, weak, or TIED. PURE and
    /// DETERMINISTIC (no clock, no I/O) given a deterministic `scorer`, so the
    /// pick is unit-testable under a fixed mock. The orchestrator is excluded as
    /// a candidate (it is the *fallback*, picked by [`select_with_fallback`] when
    /// this returns None), so the scorer is never asked to rank "no specialist."
    ///
    /// "Confident" means: a strictly-positive top score that is strictly greater
    /// than the runner-up (a tie is treated as no signal -> orchestrator) AND at
    /// or above [`SEMANTIC_MIN_SCORE`]. These thresholds are what keep a weak or
    /// ambiguous match from ever beating the safe default.
    fn semantic_pick<S: AgentScorer>(&self, text: &str, scorer: &S) -> Option<&Agent> {
        // Candidates: every specialist (the orchestrator is the fallback, not a
        // candidate). Build the parallel role/cues corpus the scorer ranks over.
        let candidates: Vec<&Agent> =
            self.agents.iter().filter(|a| !a.is_orchestrator()).collect();
        if candidates.is_empty() {
            return None;
        }
        // The corpus is each agent's CURATED routing text (its domain label +
        // keyword cues), NOT its full prose role. The prose role carries filler
        // connectors ("gets you there", "before you ask") that would otherwise let
        // an incidental conversational word ("there", "ask") spuriously out-score
        // a genuine domain match — exactly the kind of misroute the safe-default
        // contract forbids. `cue_text` keeps only the high-signal vocabulary, so
        // the ranking reflects domain fit, and a chit-chat utterance with no
        // domain overlap scores all-zero and degrades to the orchestrator. Owned
        // strings held here so the borrowed `&str` slice the scorer takes stays
        // valid for the call.
        let corpus_owned: Vec<String> = candidates.iter().map(|a| cue_text(a)).collect();
        let corpus: Vec<&str> = corpus_owned.iter().map(|s| s.as_str()).collect();

        let scores = scorer.score(text, &corpus);
        // A defensive contract check: a misbehaving scorer that returns the wrong
        // count gives no signal rather than an out-of-bounds pick.
        if scores.len() != candidates.len() {
            return None;
        }

        // Find the best and the runner-up in one pass, tracking the best index by
        // the deterministic (score desc, index asc) order so ties never depend on
        // iteration nondeterminism.
        let mut best: Option<(usize, f64)> = None;
        let mut second: Option<f64> = None;
        for (i, &s) in scores.iter().enumerate() {
            if !s.is_finite() {
                continue;
            }
            match best {
                Some((_, bs)) if s > bs => {
                    second = Some(bs);
                    best = Some((i, s));
                }
                Some((_, bs)) => {
                    // Track the highest runner-up (for the strict-tie check).
                    if second.is_none_or(|sec| s > sec) {
                        second = Some(s);
                    }
                    let _ = bs;
                }
                None => best = Some((i, s)),
            }
        }

        let (idx, top) = best?;
        // Confidence gates: a real positive signal, strictly above the runner-up
        // (no tie), at or above the floor. Any failure -> None -> orchestrator.
        if top < SEMANTIC_MIN_SCORE {
            return None;
        }
        if let Some(sec) = second {
            if top <= sec {
                return None; // tie / not strictly best -> safe default
            }
        }
        Some(candidates[idx])
    }

    /// The canonical 27-agent roster — the hardcoded fallback, kept in
    /// lockstep with config/agents.toml (the
    /// `shipped_agents_file_matches_canonical` test enforces it). Voices are
    /// verified Kokoro ids; hues follow the cyan-navy FUI palette with red
    /// reserved for alerts (ultron uses deep-orange 15).
    pub fn canonical() -> AgentRegistry {
        let agents = CANONICAL_ROSTER
            .iter()
            .map(|&(name, role, voice, hue, tools)| Agent {
                name: name.to_string(),
                role: role.to_string(),
                voice: voice.to_string(),
                hue,
                persona_file: format!("inference/personas/{name}.txt"),
                tools: tools.iter().map(|&t| t.to_string()).collect(),
                namespace: format!("agent.{name}"),
            })
            .collect::<Vec<_>>();
        // The canonical roster is correct by construction; from_agents only
        // fails on a programmer edit that breaks an invariant, which the
        // canonical_roster_is_valid test catches before shipping.
        Self::from_agents(agents).expect("canonical roster must be valid")
    }
}

/// (name, role, voice, hue, tools) for the 27 canonical agents, in roll-call
/// order. The single source of truth behind [`AgentRegistry::canonical`].
#[allow(clippy::type_complexity)]
const CANONICAL_ROSTER: &[(&str, &str, &str, u16, &[&str])] = &[
    (
        "darwin",
        "Prime Orchestrator: hears every request, delegates to the right agent, keeps you updated, ensures nothing falls through",
        "bm_george",
        190,
        &["*"],
    ),
    (
        "friday",
        "Daily Intel: morning briefs, news, schedule, what-changed-overnight",
        "bf_emma",
        35,
        &[
            "conversation", "system.query", "memory.recall", "system_status", "recall_facts",
            "gcal_list_events", "gmail_list_recent", "gmail_read_message",
            "gdrive_list_files", "gdrive_search",
            // WORLD MODEL: Friday is the Daily-Intel knowledge agent — it both
            // READS the shared world model to ground its briefs (world_query) and
            // WRITES what it learns overnight (deadlines, project state, who's
            // involved) into the shared user.world.* tier (world_update). Shared
            // user-knowledge, not a consequential action — no gate.
            "world_query", "world_update",
            // SKILL LIBRARY: the meta-tools that DISCOVER (skill_list) and RUN
            // (skill_invoke) DARWIN's hand-written in-tree skills. Friday is a
            // general utility/knowledge agent, so it holds them; a consequential
            // skill still parks behind the confirmation gate when invoked.
            "skill_list", "skill_invoke",
        ],
    ),
    (
        "veronica",
        "Content + Comms: drafts content and messages",
        "af_bella",
        320,
        &[
            "conversation", "memory.store", "memory.recall", "remember_fact", "recall_facts",
            "slack_list_channels", "slack_read_channel", "slack_post_message",
            "gdrive_upload_text",
            "connect_x", "connect_linkedin",
            "x_recent_tweets", "x_mentions", "x_post",
            "linkedin_me", "linkedin_post",
         "world_query",],
    ),
    (
        "vision",
        "Research + OSINT: competitors, trends, ads, deep research on authorized targets",
        "bf_isabella",
        265,
        &[
            "conversation", "web.open", "web.search", "file.op", "memory.recall",
            "web_search", "open_url", "search_files", "recall_facts",
         "world_query",],
    ),
    (
        "ultron",
        "Security + Automation: defensive monitoring of your Mac and LAN; automation, no offensive tooling",
        "am_onyx",
        15,
        &["conversation", "system.query", "memory.recall", "system_status", "recall_facts", "world_query"],
    ),
    (
        "athena",
        "Greek-Life Strategy: chapter, rush, and social strategy advisor",
        "af_nova",
        50,
        &["conversation", "memory.store", "memory.recall", "remember_fact", "recall_facts", "world_query"],
    ),
    (
        "stark",
        "Business Intel: metrics, competitors, business decisions",
        "am_adam",
        205,
        // stark and gecko are the two ads agents (both manage capital), so BOTH hold
        // the ads read tools and BOTH hold the consequential spend tools
        // (pause/enable/resume/budget). The consequential ones still route through the
        // foundation gate, so they preview unless allow_consequential is on AND
        // confirm=true.
        &[
            "conversation", "web.search", "memory.recall", "web_search", "recall_facts",
            "connect_linkedin", "linkedin_me", "linkedin_post",
            "connect_google_ads", "connect_meta_ads",
            "gads_report", "gads_pause_campaign", "gads_enable_campaign", "gads_set_budget",
            "meta_report", "meta_pause_campaign", "meta_resume_campaign", "meta_set_budget",
         "world_query",],
    ),
    (
        "steve",
        "CTO + Builds: investigates bugs, makes code changes, opens GitHub PRs (future)",
        "am_michael",
        150,
        &[
            "conversation", "file.op", "system.query", "memory.recall",
            "search_files", "open_path", "system_status", "recall_facts",
            "github_list_prs", "github_get_pr", "github_list_issues",
            "github_comment_issue", "github_open_pr",
            // Self-Forge (PROPOSE-ONLY): steve, the CTO/builds agent, may DRAFT a
            // new sandboxed micro-app for human review. The tool never deploys or
            // runs anything — it stages, validates, and proposes; a human runs
            // scripts/apply_forge.sh to install. Ships ON in [forge] but PROPOSE-ONLY
            // and inert without a cloud key.
            "forge_app",
            // CODE INTELLIGENCE (task #16): steve, the CTO/Builds agent, owns the
            // read-only + propose-only code surface — code_explain (a grounded,
            // CITED answer over the on-device code index, never fabricating code not
            // indexed) and code_propose_diff (a PROPOSE-ONLY reviewable diff to
            // state/code/proposals/<ts>/ — it NEVER edits the tree; the human runs
            // scripts/apply_code_diff.sh, confined-by-construction to the
            // allowlisted [code].roots root). Both ship ON in [code] but are INERT
            // until a codebase root is allowlisted.
            "code_explain", "code_propose_diff",
            // CHANGE QUEUE (changeq.rs): steve, the CTO/Builds agent, owns the
            // unified git-native review lane over every propose-only artifact —
            // changeq_list (READ-ONLY: list pending heal/code/forge/optimize
            // proposals + provenance) and changeq_apply (PROPOSE-ONLY + human-gated:
            // surface THAT type's EXISTING gated apply command + the git-revert
            // rollback — it invents NO new authority and applies nothing itself).
            "changeq_list", "changeq_apply",
            // SANDBOXED SHELL / TERMINAL (task #43): steve, the CTO/Builds agent,
            // owns the HIGHEST-RISK tool — arbitrary command execution. It ships
            // ON in [shell] but NEVER auto-runs; every command is CONSEQUENTIAL
            // (parks for a spoken yes, never auto-runs), denylist-screened PRE-exec,
            // and only ever runs under the master switch + confirm + voice-id +
            // !lockdown inside a deny-default sandbox-exec profile (no net, write-
            // confined to a scratch dir, the Keychain/~/.claude/daemon state denied).
            "shell_run",
            // GATED UI AUTOMATION (task #44, the CAPSTONE): steve, the CTO/Builds
            // agent, owns the single most DANGEROUS tool — physically actuating the
            // macOS UI (click/type/key). It ships ON in [ui_automation] but NEVER
            // auto-runs (inert without Accessibility TCC + a display); EVERY actuation
            // is CONSEQUENTIAL (it parks PER ACTION for a
            // spoken yes — ONE confirm = ONE actuation; a second re-parks; it never
            // auto-runs, never batches, never loops), is planned by the pure single-
            // action planner, and only ever actuates under the master switch +
            // confirm + voice-id + !lockdown, AND the device Accessibility-TCC
            // consent. The Vision app stays read-only (it locates a control); this
            // actuate op is a separate, maximally-gated surface.
            "ui_actuate",
            "world_query",
        ],
    ),
    (
        "oracle",
        "Workflows: chains actions into repeatable workflows",
        "bm_lewis",
        280,
        &[
            "conversation", "app.launch", "app.control", "system.query", "memory.recall",
            "open_app", "quit_app", "system_status", "recall_facts",
            // Self-Forge (PROPOSE-ONLY): oracle, the workflows agent, may DRAFT a
            // new sandboxed micro-app for human review (forging a tool into the
            // app surface fits its automation remit). Never deploys/runs — it
            // proposes; a human installs via scripts/apply_forge.sh. Ships ON but PROPOSE-ONLY (inert without a cloud key).
            "forge_app",
            // STANDING MISSIONS: a standing mission IS a repeatable scheduled
            // workflow, squarely oracle's (Workflows) remit. It may ESTABLISH one
            // (standing_create, confirmation-gated — parks for a spoken yes),
            // LIST, and CANCEL. The subsystem ships on; establishing is still
            // confirmation-gated and a run still gates every consequential step.
            "standing_create", "standing_list", "standing_cancel",
            "world_query",
        ],
    ),
    (
        "gecko",
        "Markets + Capital: market watch, trading research (future Algo-Core)",
        "bm_daniel",
        120,
        // gecko, the other ads agent (Markets + Capital), holds the same ads read +
        // consequential spend tools as stark — both manage capital. The consequential
        // ones still route through the foundation gate (preview unless
        // allow_consequential is on AND confirm=true).
        &[
            "conversation", "web.search", "memory.recall", "web_search", "recall_facts",
            "connect_google_ads", "connect_meta_ads",
            "gads_report", "gads_pause_campaign", "gads_enable_campaign", "gads_set_budget",
            "meta_report", "meta_pause_campaign", "meta_resume_campaign", "meta_set_budget",
         "world_query",],
    ),
    (
        "hercules",
        "Fitness + Nutrition: training and nutrition coaching",
        "am_fenrir",
        90,
        &["conversation", "memory.store", "memory.recall", "remember_fact", "recall_facts", "world_query"],
    ),
    (
        "pepper",
        "Personal EA + Reflection: scheduling, reminders, reflective consolidation",
        "bf_alice",
        300,
        &[
            "conversation", "memory.store", "memory.recall", "remember_fact", "recall_facts",
            "gcal_list_events", "gcal_create_event",
            "gmail_list_recent", "gmail_read_message", "gmail_send",
            "gdrive_list_files", "gdrive_search", "gdrive_upload_text",
            // SEMANTIC PASTEBOARD: Pepper (the EA who acts on the user's behalf)
            // may recall the clipboard history (pasteboard_recall, read-only) and
            // SET the clipboard for the user (pasteboard_put). pasteboard_put is
            // CONSEQUENTIAL but BENIGN (a pasteboard set only — never a keystroke/
            // file/network); it parks for a spoken yes like her other consequential
            // tools (it never auto-copies).
            "pasteboard_recall", "pasteboard_put",
            // WORLD MODEL: Pepper is the EA + Reflection/consolidation agent — the
            // natural curator of the shared world picture. She READS it
            // (world_query) and WRITES structured entities/relationships/state into
            // the shared user.world.* tier (world_update) as part of consolidation.
            // Shared user-knowledge, not a consequential action — no gate.
            "world_query", "world_update",
        ],
    ),
    (
        "hulk",
        "Offline Survival: the all-local mode when the cloud is unreachable",
        "am_echo",
        110,
        &[
            "conversation", "system.query", "app.launch", "app.control", "file.op",
            "memory.store", "memory.recall",
         "world_query",],
    ),
    (
        "herald",
        "Meetings: capture, notes, scheduling",
        "bm_fable",
        220,
        &[
            "conversation", "memory.store", "memory.recall", "remember_fact", "recall_facts",
            "gcal_list_events", "gcal_create_event",
         "world_query",],
    ),
    (
        "jerome",
        "Leisure + DJ: music and entertainment control",
        "af_river",
        340,
        &["conversation", "app.launch", "app.control", "memory.recall", "open_app", "quit_app", "recall_facts", "world_query"],
    ),
    (
        "edith",
        "Proactive Sentinel: watches your signals and surfaces what matters before you ask",
        // af_sky: a real American-English female Kokoro voice (one of the
        // original Kokoro-82M set), distinct from every other roster voice.
        "af_sky",
        // Teal 170: distinct, calm, on the cyan-navy FUI palette; never the
        // reserved alert red. Sits between gecko (120) and darwin (190).
        170,
        // Read-only / no-consequential surface. EDITH READS the signals it
        // watches (calendar, mail metadata, system health, stored facts) and
        // composes its own brief on demand; it never ACTS. The two edith_*
        // tools are read-only. A consequential follow-up the user approves
        // routes through the foundation gate at the owning agent, not here.
        &[
            "conversation", "system.query", "memory.recall",
            "system_status", "recall_facts",
            "gcal_list_events", "gmail_list_recent",
            "edith_brief", "edith_watch",
         "world_query",],
    ),
    (
        "fury",
        "Mission Orchestrator: decomposes a goal, dispatches the right agents, drives it to done",
        // am_eric: a real American-English male Kokoro voice (one of the
        // original Kokoro-82M set), deep and authoritative, distinct from every
        // other roster voice — fitting the one who assembles the team.
        "am_eric",
        // Deep navy-blue 235: distinct, commanding, on the cyan-navy FUI
        // palette; never the reserved alert red. Sits between stark (205) and
        // vision (265).
        235,
        // FURY is NOT an orchestrator-wildcard like darwin. It holds an EXPLICIT
        // mission tool (fury_mission) plus the read tools it needs to plan and
        // ground a mission — the actual ACTING happens inside each sub-task as
        // the OWNING specialist, under that specialist's own allowlist (the
        // mission engine re-runs the cloud tool loop per sub-task). FURY itself
        // never holds another agent's consequential tools; it coordinates.
        &[
            "conversation", "system.query", "memory.recall",
            "system_status", "recall_facts",
            "fury_mission",
            // STANDING MISSIONS: FURY, the mission orchestrator, is the natural
            // owner of recurring missions — a standing mission RUNS via FURY's
            // bounded engine. It may ESTABLISH one (standing_create, which is
            // confirmation-gated so it parks for a spoken yes — never silent
            // recurring autonomy), LIST them, and CANCEL them. The mission a
            // standing job runs still gates every consequential step.
            "standing_create", "standing_list", "standing_cancel",
            // DURABLE MISSIONS (#26): FURY owns the durable mission store too — it
            // may SAVE a mission to persist a campaign across a restart, LIST saved
            // missions, RESUME one (which re-runs FURY's bounded engine and re-gates
            // every consequential step — the persistence carries no pre-approval),
            // and CANCEL one. A saved mission loads PAUSED; resume is an explicit
            // user-driven step, never an auto-run. ON by default ([missions].durable; persistence only).
            "mission_save", "mission_list", "mission_resume", "mission_cancel",
            "world_query",
        ],
    ),
    (
        "cassandra",
        "Forecast & Simulation: runs the numbers, models scenarios, reports the odds",
        // af_aoede: a real American-English female Kokoro voice from the
        // Kokoro-82M set, distinct from every other roster voice — fitting the
        // probabilistic oracle archetype (Aoede, a muse). Note the honesty: the
        // archetype is a NAME, not a claim of foresight — Cassandra models, she
        // does not prophesy.
        "af_aoede",
        // Steel-blue 250: distinct, cool and analytical, on the cyan-navy FUI
        // palette; never the reserved alert red. Sits between fury (235) and
        // vision (265).
        250,
        // READ-ONLY surface. Both cassandra_* tools are PURE simulations over the
        // user's (or default) assumptions — no side effects, nothing sent or
        // changed, so neither touches integrations::gate(). Cassandra models what
        // COULD happen under the inputs; she never acts and never promises an
        // outcome will occur.
        &[
            "conversation", "memory.recall", "recall_facts",
            "cassandra_forecast", "cassandra_simulate",
         "world_query",],
    ),
    (
        "mnemosyne",
        "Semantic Memory: recalls what you've told DARWIN and surfaces the relevant past",
        // af_kore: a real American-English female Kokoro voice from the
        // Kokoro-82M set (Kore, the maiden — like Mnemosyne, a figure of Greek
        // myth used here only as a NAME, not a claim), distinct from every other
        // roster voice — fitting the precise, recollective archetype.
        "af_kore",
        // Teal-cyan 130: distinct, calm and recollective, on the cyan-navy FUI
        // palette; never the reserved alert red. Sits between gecko (120) and
        // steve (150).
        130,
        // READ-ONLY surface. mnemosyne_recall RANKS the EXISTING stored memories
        // and returns the relevant ones; it stores nothing, sends nothing, and
        // changes nothing, so it never touches integrations::gate(). The store
        // half is pepper's; Mnemosyne is retrieval only. She also holds the
        // generic recall_facts read tool. Ranking is RUNTIME-SELECTED: neural
        // on-device embeddings (cosine over the inference server's embed op) when
        // that server is up, else lexical BM25 — the persona/tool copy names
        // whichever actually ran and never claims neural on fallback.
        &[
            "conversation", "memory.recall", "recall_facts",
            "mnemosyne_recall",
            // EPISODIC RECALL: Mnemosyne also surfaces the EPISODE store (the
            // redacted, agent-scoped, bounded record of completed interactions) —
            // "what did we talk about", "what happened recently", topical recall
            // over past turns. READ-ONLY: it ranks/returns only REAL recorded
            // episodes (agent-scoped, never cross-agent), stores nothing, sends
            // nothing, so it never touches integrations::gate().
            "episodic_recall",
            // WORLD MODEL: Mnemosyne is a KNOWLEDGE agent, so she both READS and
            // WRITES the shared structured world model (world_query + world_update)
            // — recording structured entities/relationships/state into the shared
            // user.world.* tier is squarely her recollective remit. The write is
            // shared user-knowledge, not a consequential action (no gate).
            "world_query", "world_update",
            // USER MODEL: Mnemosyne owns the structured, compounding profile of the
            // user (preferences/patterns/recurring-topics/style, each provenance-
            // tagged + observed-counted). user_model_query READS the profile WITH
            // provenance ("what do you know about me"); user_model_correct and
            // user_model_forget are the CORRECTABLE + FORGETTABLE controls — they
            // edit DARWIN's BELIEF about the user only (no external action, no
            // gate), writing only the shared user.model.* tier and never inventing
            // an entry. Squarely her recollective, knowledge-keeping remit.
            "user_model_query", "user_model_correct", "user_model_forget",
            // SKILL LIBRARY: Mnemosyne is a knowledge agent, so she may DISCOVER
            // (skill_list) and RUN (skill_invoke) the in-tree skill library — pure
            // skills run ungated; a consequential one parks behind the gate.
            "skill_list", "skill_invoke",
            // ON-DEVICE FILE RAG: doc_search is READ-ONLY semantic/lexical search
            // over the user's OWN indexed files (the explicitly-allowlisted folders).
            // It CITES real indexed chunks (file path + snippet), stores nothing,
            // sends nothing — file contents + embeddings never leave the device — so
            // it never touches integrations::gate(). Squarely Mnemosyne's
            // recollective, knowledge-keeping remit: "find where I wrote about X".
            // Ranking is RUNTIME-SELECTED (neural on-device embeddings else lexical
            // BM25) and the report names whichever ran. Ships behind the OFF-by-
            // default [docsearch] gate; with it off / no folder allowlisted, a
            // search honestly returns nothing.
            "doc_search",
            // UNIFIED PERSONAL SEARCH: the READ-ONLY "search everything" surface —
            // one query fanned out across EVERY available source (on-device always:
            // files/episodes/facts/world, agent-scoped; cloud only-if-connected:
            // gmail/calendar/slack via the existing gated read-only reads), merged
            // into one ranked, attributed, CITED list with an HONEST coverage
            // summary (searched vs skipped-with-reason). It stores nothing, sends
            // nothing, and takes NO consequential action (the only cloud calls are
            // the existing gated READS), so it never touches integrations::gate().
            // Squarely Mnemosyne's recollective remit — she owns retrieval across
            // the whole personal corpus. Scoping is preserved (own + shared only,
            // never another agent's private items); every hit cites a real item.
            "unified_search",
            // ARTIFACT PEEK (artifact.rs): READ-ONLY recall over the in-memory,
            // on-device Artifact Registry — "what did you just do" / "peek" reads the
            // most recent (or an id'd) produced artifact back out with HONEST
            // provenance (real agent + real citations, or UNCITED). Squarely
            // Mnemosyne's recollective remit; the orchestrator also holds it via the
            // tools wildcard. Read-only, opens no surface.
            "artifact_peek",
            // SHARE GUARD SCRUB (artifact.rs -> share-guard micro-app): the sibling
            // of artifact_peek over the SAME registry — "scrub this before I share
            // it" resolves the addressed (or most recent) artifact and FORWARDS it to
            // the offline, default-deny sandboxed share-guard app for on-device PII
            // redaction. DAEMON SIDE READ-ONLY: it reads the artifact + forwards to
            // the app's own socket, opens no network, and SENDS NOTHING outward; the
            // app (net_hosts=[]) cannot upload and writes only a redacted COPY in its
            // sandbox — the user shares that copy. Squarely Mnemosyne's recollective
            // remit (she owns the artifact registry surface); the orchestrator also
            // holds it via the wildcard. Not consequential — it never parks.
            "share_guard_scrub",
            // SEMANTIC PASTEBOARD: pasteboard_recall is READ-ONLY recall over the
            // user's PII-redacted, bounded, transient clipboard history ("the thing
            // I copied about the lease"). It ranks the stored clips by meaning via
            // the SAME recall.rs path her other recall tools use, stores nothing,
            // sends nothing — so it never touches integrations::gate(). Squarely her
            // recollective remit. Ships OFF ([pasteboard].enabled=false); with it off
            // / nothing copied, recall honestly returns an empty history.
            "pasteboard_recall",
            // APERTURE: aperture_recall is READ-ONLY recall over the owner's private,
            // PII-redacted, bounded, transient on-device activity timeline ("what was
            // I working on around 3pm"). It summarizes app + window title + duration
            // (never screen pixels), stores nothing, sends nothing — so it never
            // touches integrations::gate(). Squarely her recollective remit. Ships OFF
            // ([aperture].enabled=false); with it off / nothing recorded, recall
            // honestly returns no activity.
            "aperture_recall",
            // The file-RAG WRITE/FORGET triggers (the local intents the classifier
            // emits for "index my documents"/"reindex" and "forget my file index").
            // Both are CONFINED to the user's OWN allowlisted folders + the local
            // state/docsearch.db; the index path is config-gated (ON by default but
            // INERT with an empty allowlist => no whole-disk scan) and forget only clears the
            // local store. Listed here so the router attributes the turn to
            // Mnemosyne (she owns the whole file-RAG surface) rather than rerouting.
            "docsearch.index", "docsearch.forget",
            // KNOWLEDGE GRAPH: the local intent the classifier emits for "build /
            // map a knowledge graph from my documents". It mines the user's OWN
            // already-indexed chunks (confined to allowlisted folders) for grounded,
            // provenance-tagged entities/relationships and upserts them into the
            // SHARED world model (world_update's structured cousin) — never an
            // agent's private namespace, never a fabricated node, bounded by the
            // world-model caps. Both [docsearch].enabled AND [docsearch].build_graph
            // ship ON, but the build is INERT WITHOUT indexed docs (needs an allowlisted
            // root + an index). Both the canonical token and the
            // `knowledge.build` alias are listed (select() routes both to her) so the
            // router attributes EITHER intent to Mnemosyne instead of rerouting the
            // alias to the orchestrator — she owns the whole document-knowledge surface.
            "docsearch.build_graph",
            "knowledge.build",
        ],
    ),
    (
        "sage",
        "Deep Research: multi-source investigation with cited synthesis",
        // am_puck: a real American-English male Kokoro voice from the Kokoro-82M
        // set, distinct from every other roster voice — a measured, scholarly
        // reader for the one who "reads widely and shows its sources."
        "am_puck",
        // Indigo 245: distinct, deep and studious, on the cyan-navy FUI palette;
        // never the reserved alert red. Sits between fury (235) and cassandra
        // (250), 10 degrees off each.
        245,
        // SAGE's surface: the deep-research tool (sage_research, the bounded
        // plan -> search -> fetch -> cited-synthesize core in crate::research)
        // plus the web read tools it leans on to ground an investigation
        // (web.search/web_search to find sources, web.open/open_url to surface
        // one). All READ surface: a real run needs the web + the cloud and spends
        // tokens, but it fetches and cites — it never ACTS. recall_facts lets it
        // ground a question against what the user already told DARWIN.
        &[
            "conversation", "web.search", "web.open", "memory.recall",
            "web_search", "open_url", "recall_facts",
            "sage_research",
         "world_query",
            // SKILL LIBRARY: Sage is the deep-research/knowledge agent, so it may
            // DISCOVER (skill_list) and RUN (skill_invoke) the in-tree skills to
            // support an investigation — pure skills run ungated; a consequential
            // one parks behind the confirmation gate.
            "skill_list", "skill_invoke",
        ],
    ),
    (
        "vitalis",
        "Health & Biometrics: tracks recovery, strain, and sleep, and tells you what your body is saying",
        // af_heart: a real American-English female Kokoro voice — the flagship
        // voice of the original Kokoro-82M set, warm and steady — distinct from
        // every other roster voice; fitting a calm performance physiologist who
        // reads the signals and says it straight.
        "af_heart",
        // Teal 160: distinct, calm and clinical, on the cyan-navy FUI palette;
        // never the reserved alert red. Sits between mnemosyne (130) and edith
        // (170).
        160,
        // WHOOP-based biometrics. The three vitalis_* tools are READ-ONLY
        // (recovery, sleep, strain) over the WHOOP API on the generic oauth2
        // provider — they fetch and report, they never act, so none touches
        // integrations::gate(). connect_whoop runs the one-time browser OAuth
        // consent (it stores the daemon-written refresh token; it posts nothing
        // and changes no WHOOP data). HONESTY: there is NO Apple Health /
        // HealthKit here — that data is iOS/watchOS only and is not reachable
        // from the Mac; Vitalis reads WHOOP, and only after the user connects it.
        &[
            "conversation", "memory.recall", "recall_facts",
            "connect_whoop",
            "vitalis_recovery", "vitalis_sleep", "vitalis_strain",
         "world_query",],
    ),
    (
        "karen",
        "Comms Autopilot: triages your inbox and channels, drafts replies, sends only on your say-so",
        // af_sarah: a real American-English female Kokoro voice from the original
        // Kokoro-82M set — steady and professional, distinct from every other
        // roster voice; fitting a sharp, unflappable chief-of-staff.
        "af_sarah",
        // Cyan 200: distinct, calm and businesslike, on the cyan-navy FUI palette;
        // never the reserved alert red. Sits between darwin (190) and stark (205).
        200,
        // Comms autopilot over the EXISTING Gmail/Slack/X tools — no new
        // integration of her own. The TWO karen_* tools are READ-ONLY: karen_triage
        // aggregates the recent unread/mentions across the CONNECTED comms surfaces
        // (it calls the existing read clients; an unconnected surface is skipped
        // honestly) into one prioritized summary, and karen_draft composes a reply
        // DRAFT and returns it as a PREVIEW — neither sends, so neither touches
        // integrations::gate(). She ALSO holds the existing read tools she triages
        // over (gmail_list_recent/gmail_read_message, slack_read_channel/
        // slack_list_channels, x_mentions/x_recent_tweets) AND the existing
        // consequential SEND tools (gmail_send, slack_post_message, x_post) — those
        // stay behind integrations::gate(confirm) exactly as today: Karen drafts and
        // triages freely, but a send needs the gate ON and confirm=true.
        &[
            "conversation", "memory.recall", "recall_facts",
            "karen_triage", "karen_draft",
            // AUTO-DRAFT (#25): Karen may PERSIST a reviewable pending draft
            // (draft_compose) the user reads and sends THEMSELVES. The draft module
            // has NO send path — a persisted draft is always a suggestion, never
            // auto-sent. draft_list/draft_forget are read-only/reversible. An actual
            // send still rides the existing gated gmail_send/slack_post_message/x_post
            // exactly as today (gate ON + confirm). ON by default ([drafts].enabled; a draft has no send path).
            "draft_compose", "draft_list", "draft_forget",
            "gmail_list_recent", "gmail_read_message", "gmail_send",
            "slack_list_channels", "slack_read_channel", "slack_post_message",
            "x_mentions", "x_recent_tweets", "x_post",
         "world_query",],
    ),
    (
        "dume",
        "Home & Environment: reads and controls your smart-home devices through your hub",
        // am_liam: a real American-English male Kokoro voice from the original
        // Kokoro-82M set, distinct from every other roster voice — an eager,
        // ready-to-help register that fits a competent lab-bot ("Point me at the
        // lights, sir").
        "am_liam",
        // Spring-teal 140: distinct, fresh and utilitarian, on the cyan-navy FUI
        // palette; never the reserved alert red. Sits between mnemosyne (130) and
        // steve (150), 10 degrees off each.
        140,
        // Home Assistant bridge. dume_devices is READ-ONLY (it lists the hub's
        // entities + states over the local REST API) and never touches the
        // foundation gate. dume_control is CONSEQUENTIAL: it routes through
        // integrations::gate(confirm), so it previews the exact service call
        // unless allow_consequential is ON and confirm=true — no device moves
        // otherwise. HONESTY: control goes through the user's OWN Home Assistant
        // hub; DARWIN does not talk HomeKit directly (raw HomeKit is not cleanly
        // reachable from a macOS daemon).
        &[
            "conversation", "memory.recall", "recall_facts",
            "dume_devices", "dume_control",
         "world_query",],
    ),
    (
        "midas",
        "Personal Treasury: reads your balances and spending and tells you where the money goes — never moves it",
        // am_santa: a real American-English male voice from the original Kokoro-82M
        // set, distinct from every other roster voice — a calm, measured register
        // fitting a discreet private banker who watches the books.
        "am_santa",
        // Amber-gold 100: distinct, a treasury/gold note on the cyan-navy FUI
        // palette; never the reserved alert red. Sits between hercules (90) and
        // hulk (110), 10 degrees off each.
        100,
        // READ + INSIGHT ONLY. The three midas_* tools are READ-ONLY over the Plaid
        // API (balances, transactions, by-category spending) — they fetch and
        // report, they never act, so NONE touches integrations::gate(). HARD RULE:
        // MIDAS NEVER MOVES MONEY. There is deliberately NO transfer/payment/trade
        // tool here — not even a gated one — because no money action exists to gate.
        // HONESTY: Plaid needs the user's own app (client id + secret) AND a linked-
        // institution access token from Plaid Link (a frontend step DARWIN does not
        // perform); until configured, midas says "no linked accounts — connect via
        // Plaid in Settings". Midas watches the books; it never touches the money.
        &[
            "conversation", "memory.recall", "recall_facts",
            "midas_balances", "midas_transactions", "midas_spending",
         "world_query",],
    ),
    (
        "voyager",
        "Travel & Logistics: routes, places, and travel times — gets you there",
        // bf_lily: a real British-English female voice from the original Kokoro-82M
        // set, distinct from every other roster voice — a calm, well-travelled
        // register that fits a seasoned navigator/concierge ("Tell me where; I'll
        // find the way").
        "bf_lily",
        // Cyan 180: distinct, calm and wayfinding, on the cyan-navy FUI palette;
        // never the reserved alert red. Sits between edith (170) and darwin (190),
        // 10 degrees off each.
        180,
        // READ-ONLY maps surface. The three voyager_* tools are READ-ONLY over a
        // maps provider (Google Maps Platform: Directions, Places, Distance Matrix):
        // voyager_directions (a route), voyager_places (a places search), and
        // voyager_eta (distance + duration). They fetch and report; NONE touches
        // integrations::gate() because there is nothing to gate — Voyager does NOT
        // book or pay for anything. There is deliberately NO reservation/payment
        // tool here, not even a gated one: booking a flight/hotel/ride would need
        // many provider APIs plus payment, which is out of scope. HONESTY: needs the
        // user's own Maps Platform API key (maps_api_key) in Settings; until then
        // Voyager says "maps isn't configured — add your Maps Platform API key in
        // Settings". The key rides only the request at call time and is never logged.
        &[
            "conversation", "memory.recall", "recall_facts",
            "voyager_directions", "voyager_places", "voyager_eta",
         "world_query",],
    ),
    (
        "aegis",
        "Defense & Privacy: checks your exposure and the machine's security posture",
        // af_nicole: a real American-English female voice from the original
        // Kokoro-82M set, distinct from every other roster voice — a calm, measured
        // register that fits a security chief who reports the walls plainly.
        "af_nicole",
        // Steel-blue 210: distinct, calm and watchful, on the cyan-navy FUI palette;
        // never the reserved alert red (red stays for alerts; ultron's identity hue
        // is deep-orange 15). Sits between stark (205) and herald (220).
        210,
        // DEFENSIVE-ONLY, READ-ONLY surface. Two reads, both safe — NEITHER touches
        // integrations::gate() because neither changes anything:
        //   * aegis_breach_check reads the Have I Been Pwned catalog for the USER'S
        //     OWN email (defaulting to the user's stored address) — it reports
        //     exposure, it cracks nothing and scans no other host.
        //   * aegis_posture reads the LOCAL machine's security posture (FileVault,
        //     firewall, SIP, pending updates) with the same read-only system-command
        //     pattern the daemon already uses — it REPORTS; turning a protection on
        //     is the user's own action in System Settings.
        // NO offensive capability: the user's own email + own machine only. Aegis
        // holds NO consequential/remediation tool, not even a gated one.
        &[
            "conversation", "memory.recall", "recall_facts",
            "aegis_breach_check", "aegis_posture", "aegis_introspect", "aegis_report", "aegis_triage",
         "world_query",],
    ),
    (
        "babel",
        "Translation & Interpretation: renders text between languages and interprets speech turn-by-turn (transcribe -> translate -> speak); continuous real-time live-mic interpretation is device-gated",
        // af_jessica: a real American-English female voice from the original
        // Kokoro-82M set, distinct from every other roster voice — a clear,
        // even-handed register that fits a faithful interpreter who renders
        // meaning straight ("Any tongue, rendered true").
        "af_jessica",
        // Cyan-teal 155: distinct, calm and lucid, on the cyan-navy FUI palette;
        // never the reserved alert red. Sits between steve (150) and vitalis
        // (160), 5 degrees off each.
        155,
        // READ-ONLY surface. babel_translate renders `text` from one language to
        // another by calling the ON-DEVICE LLM (the existing generate/brain path)
        // with a faithful-translation prompt — it transforms text and reports the
        // result; it stores nothing, sends nothing, and changes nothing, so it
        // never touches integrations::gate(). babel_interpret chains the SAME
        // on-device translate with the daemon's echo-safe speech path: it renders one
        // already-transcribed utterance into the target language and speaks the bare
        // translation aloud (turn-based interpretation). HONESTY: translation quality
        // is bounded by the local ~4B model — competent, not a dedicated machine-
        // translation system — and CONTINUOUS, always-on real-time live-mic
        // interpretation (always listening, bidirectional) is DEVICE/AUDIO-gated and
        // not wired here; babel ships the text core + the TURN-BASED interpreter and
        // documents the continuous live-mic mode as the device-gated next step.
        &[
            "conversation", "memory.recall", "recall_facts",
            "babel_translate", "babel_interpret",
         "world_query",],
    ),
];

/// Whole-word substring check: `needle` appears in `haystack` bounded by
/// non-alphanumeric edges, so "ad" does not match "read" and "pr" does not
/// match "spring". Both inputs are already lowercase on the select path.
///
/// `pub(crate)` so the optimizer (optimize.rs) can REPLAY a routing decision
/// through the exact same word-boundary matcher the live router uses — an
/// honest replay must score with the real matching rule, not a re-implemented
/// one that could subtly disagree.
pub(crate) fn contains_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let hbytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(needle) {
        let start = from + rel;
        let end = start + needle.len();
        let left_ok = start == 0 || !hbytes[start - 1].is_ascii_alphanumeric();
        let right_ok = end == hbytes.len() || !hbytes[end].is_ascii_alphanumeric();
        if left_ok && right_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

// ---------------------------------------------------------------------------
// Semantic delegation fallback (smarter intent routing)
// ---------------------------------------------------------------------------

/// The CURATED routing vocabulary per agent for the semantic fallback corpus —
/// the high-signal domain words the deterministic [`AgentRegistry::select`] cue
/// chain already keys on, gathered here so the fallback ranks an utterance
/// against an agent's CUES, not its filler-laden prose role. Keyed by agent
/// name; an agent absent here contributes only its role LABEL (the text before
/// the first colon, e.g. "Fitness + Nutrition") to its corpus entry. Keeping
/// this in lockstep with the cue chain is what makes the fallback agree with the
/// deterministic router on domain vocabulary while staying honest (it is the
/// same keyword-semantic signal, never a claimed neural match).
///
/// `pub(crate)` so the optimizer (optimize.rs) can seed its REPLAY router and
/// baseline cue-weight map from the SAME shipped vocabulary the live fallback
/// ranks against — the optimizer tunes a layer over this, it never forks it.
pub(crate) const CUE_VOCAB: &[(&str, &str)] = &[
    ("steve", "bug build compile code pr repo refactor deploy patch"),
    ("vision", "research competitor competitors trend trends osint ad ads investigate lookup footprint"),
    ("friday", "news brief briefing morning overnight headline headlines schedule agenda daily intel"),
    ("veronica", "content post caption draft write copy message reply tweet email compose"),
    ("ultron", "security monitor monitoring threat intrusion firewall defend defensive lockdown"),
    ("gecko", "market markets trade trading stock stocks crypto portfolio ticker capital"),
    ("hercules", "workout exercise training train nutrition diet macros fitness lift run coaching"),
    ("herald", "meeting meetings notes minutes standup sync capture"),
    ("jerome", "music play song playlist track dj volume tune leisure entertainment"),
    ("pepper", "remind reminder remember schedule calendar reflect consolidate scheduling assistant"),
    ("oracle", "workflow workflows automate automation chain routine app apps launch"),
    ("athena", "chapter rush fraternity sorority greek pledge social"),
    ("stark", "business metrics kpi revenue decision strategy"),
    ("edith", "anticipate proactive heads up watch alert coming sentinel surface"),
    ("fury", "mission orchestrate coordinate dispatch end to end multi-step"),
    ("cassandra", "simulate forecast scenario monte carlo odds probability model what-if"),
    ("mnemosyne", "recall remember discussed mentioned note retrieval semantic memory"),
    ("sage", "deep dive research report citations comprehensive thorough investigation"),
    ("vitalis", "recovery strain hrv sleep readiness whoop biometrics body resting heart"),
    ("karen", "triage inbox unread messages reply comms autopilot"),
    ("dume", "lights thermostat lock unlock scene smart home device hub environment"),
    ("midas", "balance balances spending transactions budget accounts cash flow net worth treasury money"),
    ("voyager", "directions route navigate travel time eta nearby map maps places logistics"),
    ("aegis", "breach pwned exposed exposure posture privacy filevault defense protected"),
    ("babel", "translate translation interpret language render speech"),
];

/// The semantic-fallback corpus text for one agent: its role LABEL (the curated
/// domain name before the first colon) plus its [`CUE_VOCAB`] keyword line. This
/// is the "role/cues" the fallback scores against — high-signal only, so the
/// ranking tracks domain fit and an incidental conversational word in the prose
/// role can no longer misroute. Pure; no I/O.
fn cue_text(agent: &Agent) -> String {
    let label = agent.role.split(':').next().unwrap_or("").trim();
    let cues = CUE_VOCAB
        .iter()
        .find(|(name, _)| *name == agent.name)
        .map(|(_, v)| *v)
        .unwrap_or("");
    if cues.is_empty() {
        label.to_string()
    } else {
        format!("{label} {cues}")
    }
}

/// The minimum top score the semantic fallback will act on. Below this floor
/// the signal is too weak to override the safe orchestrator default, so the
/// turn stays with darwin. With the shipped BM25 scorer (`LexicalAgentScorer`)
/// a score arises only when an utterance term overlaps an agent's role text, so
/// any strictly-positive top is already meaningful; the floor is a small
/// positive epsilon that rejects only the degenerate near-zero case while
/// letting a genuine single-term match through. A mock scorer in tests sets its
/// own magnitudes, so this floor is the live-path guard, not a test artifact.
const SEMANTIC_MIN_SCORE: f64 = 1e-6;

/// A pluggable, PURE scorer for the semantic delegation fallback: given the
/// utterance and a parallel list of each candidate specialist's role/cues text,
/// return a relevance score per candidate (same order, higher = better fit).
///
/// This mirrors [`crate::recall::EmbeddingProvider`] in spirit (the same
/// inject-the-ranking-mechanism discipline RAG uses) so the routing decision is
/// deterministic and unit-testable WITHOUT any live backend: the shipped
/// implementation ([`LexicalAgentScorer`]) reuses the BM25 ranker; a future
/// neural variant could embed the utterance + the role texts; tests inject a
/// fixed mock. The contract that keeps the fallback safe lives in the caller
/// ([`AgentRegistry::semantic_pick`]): a non-strict-best, sub-floor, or absent
/// signal degrades to the orchestrator. An implementation that cannot produce a
/// confident ranking (backend down) should return all-equal / all-zero scores
/// (or a wrong-length vector), which the caller reads as "no signal."
pub trait AgentScorer {
    /// Score the utterance against each candidate's role/cues text, returning
    /// one score per candidate in the SAME ORDER as `roles`. Higher = better
    /// fit; a candidate with no relevance scores 0.0. Pure and deterministic.
    fn score(&self, utterance: &str, roles: &[&str]) -> Vec<f64>;
}

/// The SHIPPED semantic scorer: honest LEXICAL (BM25) similarity of the
/// utterance to each agent's role text, computed by reusing MNEMOSYNE's recall
/// ranker ([`crate::recall`]). It is keyword-semantic, NOT a neural embedding —
/// the same honest fallback recall uses when the on-device embedder is down — so
/// it never claims a meaning-level match it did not make. PURE and DETERMINISTIC
/// (no clock, no I/O, no network): each role becomes a `recall::Fact`, the
/// utterance is the query, and `LexicalProvider::score` ranks them. This is the
/// "lightest honest mechanism that fits" — it adds no model call to the hot
/// path and degrades to "no signal" (all-zero) on a non-overlapping utterance,
/// which the caller maps to the orchestrator default.
#[derive(Debug, Clone, Copy, Default)]
pub struct LexicalAgentScorer;

impl AgentScorer for LexicalAgentScorer {
    fn score(&self, utterance: &str, roles: &[&str]) -> Vec<f64> {
        if roles.is_empty() {
            return Vec::new();
        }
        // Each candidate's role text becomes a "fact" the BM25 ranker scores
        // against the utterance. We score in place (not via rank()) so the
        // result stays parallel to `roles` — the caller needs per-candidate
        // scores in order, not a dedup'd/truncated top-k.
        let facts: Vec<crate::recall::Fact> = roles
            .iter()
            .map(|r| crate::recall::Fact::new("", *r))
            .collect();
        let provider = crate::recall::LexicalProvider::default();
        use crate::recall::EmbeddingProvider;
        provider.score(utterance, &facts)
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentRegistry, CANONICAL_ROSTER};

    /// The canonical roster must satisfy every registry invariant — this is
    /// the guard that lets canonical() unwrap from_agents.
    #[test]
    fn canonical_roster_is_valid() {
        let reg = AgentRegistry::canonical();
        assert_eq!(reg.all().len(), 27, "the roster is 27 agents");
        assert_eq!(reg.orchestrator().name, "darwin");
    }

    #[test]
    fn roster_brief_names_every_agent_marks_the_orchestrator_and_states_the_honesty() {
        let reg = AgentRegistry::canonical();
        let brief = reg.roster_brief();
        // The orchestrator is marked "(you, ...)" so the cloud brain knows it IS darwin.
        assert!(
            brief.contains("darwin (you, Prime Orchestrator)"),
            "orchestrator unmarked: {brief}"
        );
        // Every agent in the live roster appears (grounded — the brain lists only these).
        for a in reg.all() {
            assert!(brief.contains(&a.name), "agent {} missing from roster brief", a.name);
        }
        // A couple of named specialists, so the test fails if the roster goes empty/wrong.
        assert!(brief.contains("vision"), "vision missing: {brief}");
        assert!(brief.contains("friday"), "friday missing: {brief}");
        // The honest "profiles on one engine, not separate minds" framing rides along.
        assert!(
            brief.to_lowercase().contains("one engine"),
            "missing the not-separate-minds honesty: {brief}"
        );
    }

    #[test]
    fn is_agent_query_matches_roster_questions_not_chat() {
        use super::is_agent_query;
        // Roster / list questions -> true (these are what got misrouted + hallucinated).
        for q in [
            "give me a list of my agents",
            "Darwin, give me a list of my agents.",
            "list my agents",
            "who are my agents",
            "what agents do I have",
            "name the agents",
            "how many agents do I have",
            "tell me about the constellation",
            "show me the agent roster",
        ] {
            assert!(is_agent_query(q), "should be an agent query: {q}");
        }
        // Ordinary chat / unrelated -> false (must not hijack normal conversation).
        for q in [
            "hi darwin",
            "what's the weather",
            "open silicon canvas",
            "remember that I like jazz",
            "what time is it",
        ] {
            assert!(!is_agent_query(q), "should NOT be an agent query: {q}");
        }
    }

    #[test]
    fn roster_spoken_names_the_real_team_grounded() {
        let reg = AgentRegistry::canonical();
        let spoken = reg.roster_spoken();
        assert!(spoken.contains("27 agents"), "count missing: {spoken}");
        // Real specialists named with their short role label (before the colon).
        assert!(spoken.contains("vision for Research + OSINT"), "vision role missing: {spoken}");
        assert!(spoken.contains("friday for Daily Intel"), "friday role missing: {spoken}");
        // EDITH (the sentinel) is listed with her short role label too.
        assert!(spoken.contains("edith for Proactive Sentinel"), "edith role missing: {spoken}");
        // FURY (the mission orchestrator) is listed with his short role label too.
        assert!(spoken.contains("fury for Mission Orchestrator"), "fury role missing: {spoken}");
        // CASSANDRA (forecast & simulation) is listed with her short role label too.
        assert!(spoken.contains("cassandra for Forecast & Simulation"), "cassandra role missing: {spoken}");
        // MNEMOSYNE (semantic memory) is listed with her short role label too.
        assert!(spoken.contains("mnemosyne for Semantic Memory"), "mnemosyne role missing: {spoken}");
        // SAGE (deep research) is listed with its short role label too.
        assert!(spoken.contains("sage for Deep Research"), "sage role missing: {spoken}");
        // KAREN (comms autopilot) is listed with her short role label too.
        assert!(spoken.contains("karen for Comms Autopilot"), "karen role missing: {spoken}");
        // MIDAS (personal treasury) is listed with his short role label too.
        assert!(spoken.contains("midas for Personal Treasury"), "midas role missing: {spoken}");
        // VOYAGER (travel & logistics) is listed with her short role label too.
        assert!(spoken.contains("voyager for Travel & Logistics"), "voyager role missing: {spoken}");
        // AEGIS (defense & privacy) is listed with her short role label too.
        assert!(spoken.contains("aegis for Defense & Privacy"), "aegis role missing: {spoken}");
        // BABEL (translation & interpretation) is listed with its short role label too.
        assert!(spoken.contains("babel for Translation & Interpretation"), "babel role missing: {spoken}");
        // The orchestrator is referred to as "myself", never listed as a teammate.
        assert!(spoken.contains("myself"), "orchestrator framing missing: {spoken}");
        assert!(!spoken.contains("darwin for"), "orchestrator must not be in the listed team: {spoken}");
        // Grounded: a name that is NOT a real agent must never appear.
        assert!(!spoken.to_lowercase().contains("chorvis"), "hallucinated agent must never appear");
    }

    /// Roll-call order is declaration order: darwin first, then the team
    /// exactly as the reel maps them.
    #[test]
    fn roll_call_order_is_declaration_order_darwin_first() {
        let reg = AgentRegistry::canonical();
        let names: Vec<&str> = reg.all().iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "darwin", "friday", "veronica", "vision", "ultron", "athena", "stark",
                "steve", "oracle", "gecko", "hercules", "pepper", "hulk", "herald", "jerome",
                "edith", "fury", "cassandra", "mnemosyne", "sage", "vitalis", "karen", "dume",
                "midas", "voyager", "aegis", "babel",
            ]
        );
        // The orchestrator leads the roll call.
        assert_eq!(names[0], "darwin");
    }

    /// Every voice is a verified Kokoro id (first letter is a valid language
    /// code) and every hue is in range; ultron must NOT use a literal alert
    /// red (0 or 360) — red is reserved for alerts.
    #[test]
    fn voices_and_hues_are_valid() {
        // Verified against mlx-community/Kokoro-82M-bf16/voices/*.safetensors.
        // af_sky is EDITH's voice — a real American-English female voice from
        // the original Kokoro-82M set; am_eric is FURY's — a real American-English
        // male voice from the same set, deep and authoritative; af_aoede is
        // CASSANDRA's — a real American-English female voice from the same set,
        // distinct from every other roster voice; af_kore is MNEMOSYNE's — a real
        // American-English female voice from the same set, distinct from every
        // other roster voice; am_puck is SAGE's — a real American-English male
        // voice from the same set, a measured scholarly reader, distinct from
        // every other roster voice; af_heart is VITALIS's — the real flagship
        // American-English female voice of the Kokoro-82M set, warm and steady,
        // distinct from every other roster voice; af_sarah is KAREN's — a real
        // American-English female voice from the same set, steady and professional,
        // distinct from every other roster voice; fitting an unflappable
        // chief-of-staff. am_liam is DUM-E's — a real American-English male voice
        // from the same set, eager and ready-to-help, distinct from every other
        // roster voice; fitting a competent lab-bot. am_santa is MIDAS's — a real
        // American-English male voice from the same set, a calm measured register
        // distinct from every other roster voice; fitting a discreet private banker.
        // bf_lily is VOYAGER's — a real British-English female voice from the same
        // set, a calm well-travelled register distinct from every other roster
        // voice; fitting a seasoned navigator/concierge. af_nicole is AEGIS's — a
        // real American-English female voice from the same set, a calm measured
        // register distinct from every other roster voice; fitting a security chief
        // who reports the walls plainly. af_jessica is BABEL's — a real
        // American-English female voice from the same set, a clear even-handed
        // register distinct from every other roster voice; fitting a faithful
        // interpreter who renders meaning straight.
        let valid_voices = [
            "bm_george", "bf_emma", "af_bella", "bf_isabella", "am_onyx", "af_nova",
            "am_adam", "am_michael", "bm_lewis", "bm_daniel", "am_fenrir", "bf_alice",
            "am_echo", "bm_fable", "af_river", "af_sky", "am_eric", "af_aoede", "af_kore",
            "am_puck", "af_heart", "af_sarah", "am_liam", "am_santa", "bf_lily", "af_nicole",
            "af_jessica",
        ];
        let reg = AgentRegistry::canonical();
        for a in reg.all() {
            assert!(a.hue <= 360, "{} hue {} out of range", a.name, a.hue);
            assert!(
                valid_voices.contains(&a.voice.as_str()),
                "{} has unverified voice {}",
                a.name,
                a.voice
            );
            // Kokoro language-letter gate (mirrors the server's validator).
            assert!(
                matches!(a.voice.as_bytes()[0], b'a' | b'b'),
                "{} voice {} fails the Kokoro language-letter check",
                a.name,
                a.voice
            );
        }
        // Red is reserved for alerts: ultron's identity hue is deep-orange.
        let ultron = reg.get("ultron").unwrap();
        assert_eq!(ultron.hue, 15, "ultron must use deep-orange, not alert red");
        assert!(reg.all().iter().all(|a| a.hue != 0 && a.hue != 360));
    }

    /// agents.toml parses into the registry with deny_unknown_fields, and the
    /// shipped file carries exactly the canonical roster — if either side
    /// drifts (name, role, voice, hue, tools, namespace), this fails.
    #[test]
    fn shipped_agents_file_matches_canonical() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("config")
            .join("agents.toml");
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let (parsed, issues) = AgentRegistry::parse(&raw);
        assert!(issues.is_empty(), "shipped agents.toml has issues: {issues:?}");
        let canonical = AgentRegistry::canonical();
        assert_eq!(parsed.all().len(), canonical.all().len());
        for (p, c) in parsed.all().iter().zip(canonical.all()) {
            assert_eq!(p, c, "agents.toml drifted from canonical for '{}'", c.name);
        }
    }

    /// An unknown key anywhere in the file is rejected (deny_unknown_fields)
    /// and the registry falls back to the canonical roster with an issue —
    /// a typo never silently half-loads the team.
    #[test]
    fn unknown_key_rejects_and_falls_back() {
        let raw = r#"
            [[agent]]
            name = "darwin"
            role = "Prime Orchestrator"
            voice = "bm_george"
            hue = 190
            persona_file = "inference/personas/darwin.txt"
            tools = ["*"]
            namespace = "agent.darwin"
            color = "cyan"   # not a known field
        "#;
        let (reg, issues) = AgentRegistry::parse(raw);
        assert_eq!(issues.len(), 1, "the unknown key must be reported");
        assert!(issues[0].contains("invalid"), "{issues:?}");
        // Fallback is the full canonical team, not the half-defined file.
        assert_eq!(reg.all().len(), 27);
    }

    /// A roster with no orchestrator (none holds the wildcard) fails
    /// validation and falls back — there must always be a delegation target.
    #[test]
    fn missing_orchestrator_falls_back() {
        let raw = r#"
            [[agent]]
            name = "friday"
            role = "Daily Intel"
            voice = "bf_emma"
            hue = 35
            persona_file = "inference/personas/friday.txt"
            tools = ["conversation"]
            namespace = "agent.friday"
        "#;
        let (reg, issues) = AgentRegistry::parse(raw);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("validation"), "{issues:?}");
        assert_eq!(reg.orchestrator().name, "darwin");
    }

    /// A mismatched namespace is a validation failure (lockstep guard).
    #[test]
    fn namespace_must_be_agent_dot_name() {
        let raw = r#"
            [[agent]]
            name = "darwin"
            role = "Prime Orchestrator"
            voice = "bm_george"
            hue = 190
            persona_file = "inference/personas/darwin.txt"
            tools = ["*"]
            namespace = "agent.darwin"

            [[agent]]
            name = "friday"
            role = "Daily Intel"
            voice = "bf_emma"
            hue = 35
            persona_file = "inference/personas/friday.txt"
            tools = ["conversation"]
            namespace = "friday"
        "#;
        let (_reg, issues) = AgentRegistry::parse(raw);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("namespace"), "{issues:?}");
    }

    // ---- Darwin-Prime delegation (the rule map) ----

    /// Each canonical example utterance routes to the right agent; an
    /// unmatched one falls to darwin.
    #[test]
    fn selection_routes_examples_to_the_right_agent() {
        let reg = AgentRegistry::canonical();
        // (intent, text, expected agent)
        let cases: &[(&str, &str, &str)] = &[
            // intent-driven
            ("app.launch", "open safari", "oracle"),
            ("app.control", "quit music", "oracle"),
            ("web.open", "open apple.com", "vision"),
            ("file.op", "find my budget spreadsheet", "vision"), // file.op intent owner; no keyword leakage
            ("system.query", "how's the system", "ultron"),
            ("memory.store", "remember my license plate", "pepper"),
            ("memory.recall", "what do you know about me", "pepper"),
            // keyword-driven over a plain conversation intent
            ("conversation", "investigate this bug in the build", "steve"),
            ("conversation", "research our competitors and their ad trends", "vision"),
            ("conversation", "give me the morning brief and the news", "friday"),
            ("conversation", "draft a caption for this post", "veronica"),
            ("conversation", "is there any security threat to monitor", "ultron"),
            ("conversation", "what's the market doing, any good trades", "gecko"),
            ("conversation", "plan my workout and nutrition", "hercules"),
            ("conversation", "take notes for this meeting", "herald"),
            ("conversation", "play some music", "jerome"),
            ("conversation", "remind me to call mom", "pepper"),
            ("conversation", "chain this into a repeatable workflow", "oracle"),
            ("conversation", "what business metrics should drive this decision", "stark"),
            ("conversation", "what's our rush chapter strategy", "athena"),
            // EDITH anticipation cues (must NOT steal friday's brief/news).
            ("conversation", "anything i should know right now", "edith"),
            ("conversation", "give me a heads up on what's coming", "edith"),
            ("conversation", "keep an eye on my disk space", "edith"),
            ("conversation", "what should i know before my next meeting", "edith"),
            // FURY mission cues (multi-step orchestration -> the orchestrator of
            // missions, even when a domain keyword appears inside the goal).
            ("conversation", "run point on the product launch mission", "fury"),
            ("conversation", "handle all of the launch end to end", "fury"),
            ("conversation", "take care of everything for the campaign", "fury"),
            ("conversation", "orchestrate the news brief and a draft post", "fury"),
            // CASSANDRA forecast/simulation cues (modeling verbs -> the modeler,
            // even when a market-ish word appears in the goal). Must NOT poach
            // gecko's live MARKET watch.
            ("conversation", "simulate this stock over the next year", "cassandra"),
            ("conversation", "run a monte carlo forecast on the portfolio", "cassandra"),
            ("conversation", "what are the odds this hits the target", "cassandra"),
            ("conversation", "model this scenario for me", "cassandra"),
            // MNEMOSYNE retrieval cues (RECALL phrasing -> the memory specialist,
            // even when the classifier labels the turn memory.recall — retrieval
            // is the more specific memory intent than pepper's store ownership).
            ("conversation", "what did i say about the budget", "mnemosyne"),
            ("memory.recall", "what do you remember about my car", "mnemosyne"),
            ("conversation", "dig up that note on the launch", "mnemosyne"),
            ("conversation", "have we discussed the rebrand before", "mnemosyne"),
            ("conversation", "when did i mention the trip", "mnemosyne"),
            // SAGE deep-research cues (THOROUGH, CITED, multi-source -> the deep
            // research specialist, NOT vision's quick lookup). The boundary:
            // vision keeps the bare "research" token; SAGE owns the deep/cited
            // variant even when "research" appears inside it.
            ("conversation", "do a deep dive on solid-state batteries", "sage"),
            ("conversation", "give me a research report on the EV market", "sage"),
            ("conversation", "look into fusion startups thoroughly", "sage"),
            ("conversation", "research this comprehensively with citations", "sage"),
            ("conversation", "tell me everything about the Apollo program", "sage"),
            // VITALIS biometrics cues (the BODY's signals off WHOOP -> the
            // biometrics specialist, NOT hercules' coaching). The boundary:
            // hercules keeps the workout/nutrition COACHING tokens (see the
            // "plan my workout and nutrition" -> hercules case above); Vitalis
            // owns the recovery/strain/HRV/sleep-score/readiness reads.
            ("conversation", "how recovered am i today", "vitalis"),
            ("conversation", "what's my recovery and strain", "vitalis"),
            ("conversation", "what's my hrv this morning", "vitalis"),
            ("conversation", "how did i sleep last night", "vitalis"),
            ("conversation", "what's my whoop sleep score", "vitalis"),
            ("conversation", "what's my readiness", "vitalis"),
            ("conversation", "how's my body doing", "vitalis"),
            // KAREN comms-triage cues (TRIAGE/INBOX -> the comms autopilot, NOT
            // veronica's compose side). The boundary: veronica keeps the bare
            // content/post/caption/draft/message/reply tokens (see the "draft a
            // caption for this post" -> veronica case above); Karen owns the
            // triage/inbox phrasings even when "reply"/"email"/"messages" appear.
            ("conversation", "triage my inbox", "karen"),
            ("conversation", "catch me up on my messages", "karen"),
            ("conversation", "what needs a reply", "karen"),
            ("conversation", "anything unread i should clear", "karen"),
            ("conversation", "who needs me right now", "karen"),
            ("conversation", "draft a reply to that email", "karen"),
            ("conversation", "what's in my email", "karen"),
            // DUM-E home-control cues (smart-home devices through the hub -> the
            // home agent, NOT jerome's media or oracle's apps). Unambiguous home
            // nouns route on their own; "turn on/off" + "set the" route only with a
            // home-device noun present.
            ("conversation", "turn on the living room lights", "dume"),
            ("conversation", "turn off the bedroom lamp", "dume"),
            ("conversation", "set the thermostat to 70", "dume"),
            ("conversation", "lock the front door", "dume"),
            ("conversation", "unlock the garage", "dume"),
            ("conversation", "what's the state of my smart home", "dume"),
            ("conversation", "activate the movie scene", "dume"),
            ("conversation", "is my home assistant reachable", "dume"),
            // The broad verbs must NOT poach non-home uses: "turn off the music"
            // stays with jerome (media), and "play some music" already routes to
            // jerome above. "music" is not a home-device noun, so "turn off" alone
            // does not claim the turn.
            ("conversation", "turn off the music", "jerome"),
            // MIDAS personal-finance cues (the user's OWN money -> the treasury
            // reader, NOT gecko's market watch). The boundary: gecko keeps the
            // market/trade/stock/crypto/portfolio/ticker tokens (see the "what's the
            // market doing, any good trades" -> gecko case above); Midas owns the
            // balance/spending/transactions/accounts reads. HARD RULE: Midas reads
            // only — it never moves money.
            ("conversation", "what's my balance", "midas"),
            ("conversation", "show me my balances", "midas"),
            ("conversation", "how much did i spend this month", "midas"),
            ("conversation", "where's my money going", "midas"),
            ("conversation", "show my recent transactions", "midas"),
            ("conversation", "what's my cash flow", "midas"),
            ("conversation", "what's my net worth", "midas"),
            ("conversation", "how am i doing on my budget", "midas"),
            ("conversation", "what's in my accounts", "midas"),
            // VOYAGER travel/logistics cues (READ-ONLY routes/places/ETA -> the
            // navigator). The boundary: Voyager owns directions/route/navigate,
            // travel-time/ETA, "how far", and nearby/find-a place searches; it does
            // NOT book or pay for anything (a booking request is never claimed here).
            ("conversation", "give me directions to the airport", "voyager"),
            ("conversation", "what's the best route downtown", "voyager"),
            ("conversation", "navigate to the nearest gas station", "voyager"),
            ("conversation", "how long to get to the office", "voyager"),
            ("conversation", "what's the travel time to SFO", "voyager"),
            ("conversation", "what's the eta to the venue", "voyager"),
            ("conversation", "how far is the beach from here", "voyager"),
            ("conversation", "find a coffee near me", "voyager"),
            ("conversation", "is there a restaurant near the hotel", "voyager"),
            ("conversation", "any pharmacy nearby", "voyager"),
            ("conversation", "pull up a map of the area", "voyager"),
            // AEGIS exposure/privacy cues (DEFENSIVE: the user's OWN email +
            // machine -> the defense-and-privacy specialist, NOT ultron's live
            // monitoring). The boundary: ultron keeps the monitor/threat/firewall
            // tokens (see "is there any security threat to monitor" -> ultron above);
            // Aegis owns the breach/pwned/exposed/posture/privacy reads.
            ("conversation", "have i been pwned", "aegis"),
            ("conversation", "was my email in a breach", "aegis"),
            ("conversation", "am i exposed in any data leak", "aegis"),
            ("conversation", "check my security posture", "aegis"),
            ("conversation", "am i protected on this mac", "aegis"),
            ("conversation", "run a privacy check", "aegis"),
            ("conversation", "is filevault on", "aegis"),
            ("conversation", "have my passwords leaked anywhere", "aegis"),
            // BABEL translation cues (RENDER between languages -> the translator,
            // NOT veronica's compose side). The boundary: veronica keeps the bare
            // content/write/message tokens; babel owns translate/interpret and the
            // "<verb> ... in <language>" shape. The on-device LLM does the rendering.
            ("conversation", "translate this into spanish", "babel"),
            ("conversation", "how do you say good morning in french", "babel"),
            ("conversation", "what does hola mean in english", "babel"),
            ("conversation", "say this in german", "babel"),
            ("conversation", "interpret what she just said", "babel"),
            ("conversation", "can you translate the menu", "babel"),
            // unmatched -> orchestrator
            ("conversation", "hello, how are you today", "darwin"),
            ("conversation", "tell me a story about the sea", "darwin"),
            // A bare "what is" + language name is a general-knowledge question,
            // NOT a render request — it stays with the orchestrator.
            ("conversation", "what is the french revolution", "darwin"),
        ];
        for (intent, text, expected) in cases {
            let chosen = reg.select(intent, text, true);
            assert_eq!(
                chosen.name, *expected,
                "intent={intent:?} text={text:?} -> {} (wanted {expected})",
                chosen.name
            );
        }
    }

    // ---- Semantic delegation fallback (smarter intent routing) ----

    /// A deterministic mock scorer for the semantic-fallback tests: it gives a
    /// fixed positive score to whichever candidate's role text contains
    /// `target_substr`, an optional lower score to a `runner_up_substr` (to set
    /// up tie / near-tie cases), and 0.0 to everyone else. PURE — no clock, no
    /// I/O — so the routing decision is reproducible under it, which is exactly
    /// what proves the fallback is deterministic. This stands in for the live
    /// [`LexicalAgentScorer`]/a future neural scorer without any backend.
    struct MockScorer {
        target_substr: &'static str,
        target_score: f64,
        runner_up_substr: &'static str,
        runner_up_score: f64,
    }

    impl MockScorer {
        /// A single confident pick: only candidates whose role contains
        /// `target` get a positive score.
        fn single(target: &'static str, score: f64) -> Self {
            Self {
                target_substr: target,
                target_score: score,
                runner_up_substr: "",
                runner_up_score: 0.0,
            }
        }
    }

    impl super::AgentScorer for MockScorer {
        fn score(&self, _utterance: &str, roles: &[&str]) -> Vec<f64> {
            roles
                .iter()
                .map(|r| {
                    let lower = r.to_lowercase();
                    if !self.target_substr.is_empty() && lower.contains(self.target_substr) {
                        self.target_score
                    } else if !self.runner_up_substr.is_empty()
                        && lower.contains(self.runner_up_substr)
                    {
                        self.runner_up_score
                    } else {
                        0.0
                    }
                })
                .collect()
        }
    }

    /// A scorer that returns nothing useful (every candidate 0.0) — the
    /// "backend unavailable / no signal" shape. The fallback must degrade to the
    /// orchestrator under it, never a worse outcome than the deterministic pass.
    struct NoSignalScorer;
    impl super::AgentScorer for NoSignalScorer {
        fn score(&self, _utterance: &str, roles: &[&str]) -> Vec<f64> {
            vec![0.0; roles.len()]
        }
    }

    /// THE HEADLINE CASE: an ambiguous utterance that matches NO deterministic
    /// cue but is clearly a fitness question routes to hercules via the semantic
    /// fallback — instead of falling to the orchestrator. hercules' role text is
    /// "Fitness + Nutrition: ...", so the mock scores the candidate whose role
    /// mentions "fitness". `select` alone would land on darwin (no cue matched);
    /// `select_with_fallback` recovers the right specialist.
    #[test]
    fn semantic_fallback_routes_ambiguous_fitness_question_to_hercules() {
        let reg = AgentRegistry::canonical();
        // No fitness keyword cue (workout/exercise/training/...) appears here, so
        // the deterministic pass falls to the orchestrator.
        let utterance = "i want to get back into shape and feel stronger";
        assert_eq!(
            reg.select("conversation", utterance, true).name,
            "darwin",
            "precondition: this ambiguous phrasing matches no cue -> orchestrator"
        );
        // With the semantic fallback (mock scores hercules' "fitness" role top),
        // it reaches the fitness specialist.
        let scorer = MockScorer::single("fitness", 5.0);
        assert_eq!(
            reg.select_with_fallback("conversation", utterance, true, &scorer).name,
            "hercules",
            "the semantic fallback should reach hercules for an ambiguous fitness question"
        );
    }

    /// An utterance that DOES match a deterministic cue still routes BY THE CUE —
    /// the semantic fallback never engages (it only fires on the orchestrator
    /// default). Even a mock scorer that would aim somewhere else cannot override
    /// the authoritative first pass.
    #[test]
    fn cue_match_is_unchanged_by_the_semantic_fallback() {
        let reg = AgentRegistry::canonical();
        // "play some music" hits jerome's media cue deterministically.
        let utterance = "play some music";
        assert_eq!(
            reg.select("conversation", utterance, true).name,
            "jerome",
            "precondition: the cue routes to jerome"
        );
        // A mock that would otherwise score hercules' "fitness" role top must NOT
        // change the outcome — the cue pass already chose jerome.
        let scorer = MockScorer::single("fitness", 5.0);
        assert_eq!(
            reg.select_with_fallback("conversation", utterance, true, &scorer).name,
            "jerome",
            "a cue match must be untouched by the semantic fallback"
        );
    }

    /// A concrete-ACTION intent already owned deterministically (here
    /// system.query -> ultron) is NOT re-aimed by the semantic fallback: the
    /// fallback engages only for the conversation intent, so an action can never
    /// be delegated to a specialist that lacks its tool.
    #[test]
    fn action_intents_are_never_reaimed_by_the_fallback() {
        let reg = AgentRegistry::canonical();
        let scorer = MockScorer::single("fitness", 5.0);
        // system.query -> ultron by the intent map; the fallback must not touch it.
        assert_eq!(
            reg.select_with_fallback("system.query", "how's the system", true, &scorer).name,
            "ultron",
            "an action intent owned deterministically must not be re-aimed"
        );
    }

    /// Low-confidence / blank / no-signal -> the orchestrator (the safe default),
    /// never a worse outcome than the deterministic pass alone.
    #[test]
    fn weak_blank_or_no_signal_degrades_to_orchestrator() {
        let reg = AgentRegistry::canonical();
        // Blank utterance: no semantic signal even though it is a conversation
        // intent that fell to the orchestrator.
        let scorer = MockScorer::single("fitness", 5.0);
        assert_eq!(
            reg.select_with_fallback("conversation", "   ", true, &scorer).name,
            "darwin",
            "a blank utterance must stay with the orchestrator"
        );
        // No-signal scorer (backend unavailable shape) on a real utterance.
        let utterance = "i want to get back into shape and feel stronger";
        assert_eq!(
            reg.select_with_fallback("conversation", utterance, true, &NoSignalScorer).name,
            "darwin",
            "an all-zero (no signal / backend down) scorer must degrade to the orchestrator"
        );
    }

    /// A TIE (two candidates with the equal top score) is treated as no
    /// confident signal -> the orchestrator. The fallback acts only on a
    /// STRICTLY-best specialist, so an ambiguous pull between two never silently
    /// guesses one.
    #[test]
    fn a_tie_degrades_to_orchestrator() {
        let reg = AgentRegistry::canonical();
        // Score BOTH "fitness" (hercules) and "markets" (gecko) at the same top
        // value -> a tie -> no confident pick.
        let scorer = MockScorer {
            target_substr: "fitness",
            target_score: 5.0,
            runner_up_substr: "markets",
            runner_up_score: 5.0,
        };
        let utterance = "i want to get back into shape and feel stronger";
        assert_eq!(
            reg.select_with_fallback("conversation", utterance, true, &scorer).name,
            "darwin",
            "a tie at the top must degrade to the orchestrator (safe default)"
        );
    }

    /// DETERMINISM: the same (utterance, mock scorer) always yields the same
    /// delegation. Run the fallback many times and assert a single stable pick —
    /// the routing decision never depends on iteration order or anything stateful.
    #[test]
    fn semantic_fallback_is_deterministic_under_a_fixed_scorer() {
        let reg = AgentRegistry::canonical();
        let scorer = MockScorer::single("fitness", 5.0);
        let utterance = "i want to get back into shape and feel stronger";
        let first = reg.select_with_fallback("conversation", utterance, true, &scorer).name.clone();
        for _ in 0..50 {
            assert_eq!(
                reg.select_with_fallback("conversation", utterance, true, &scorer).name,
                first,
                "the fallback pick must be deterministic under a fixed scorer"
            );
        }
        assert_eq!(first, "hercules");
    }

    /// SAFETY: the fallback only changes DELEGATION — the picked agent's tool
    /// allowlist, the gate, and isolation are unaffected. Concretely: even when
    /// the fallback delegates a conversational turn to a specialist, that
    /// specialist may NOT hold a consequential tool it never had, so a later
    /// action still routes through its real owner + the gate. hercules (the
    /// fallback pick here) holds only conversation/memory tools — it does NOT
    /// hold, say, gmail_send — so the allowlist that guards the gate is intact.
    #[test]
    fn fallback_does_not_widen_the_picked_agents_allowlist() {
        let reg = AgentRegistry::canonical();
        let scorer = MockScorer::single("fitness", 5.0);
        let utterance = "i want to get back into shape and feel stronger";
        let chosen = reg.select_with_fallback("conversation", utterance, true, &scorer);
        assert_eq!(chosen.name, "hercules");
        // The fallback grants NO new tools: hercules' allowlist is exactly what
        // the roster declares (a coaching agent with no consequential surface).
        assert!(
            chosen.may_use("conversation"),
            "hercules keeps its declared conversation tool"
        );
        assert!(
            !chosen.may_use("gmail_send"),
            "the fallback must NOT widen the allowlist: hercules can't send mail"
        );
        assert!(
            !chosen.is_orchestrator(),
            "the fallback delegates to a SPECIALIST, not the wildcard orchestrator"
        );
    }

    /// The SHIPPED lexical scorer ([`LexicalAgentScorer`]) is honest and
    /// hermetic: a fitness-flavored utterance scores hercules' role above an
    /// unrelated agent's, and an utterance with no role overlap scores all-zero
    /// (which the caller maps to the orchestrator). No backend, no network — pure
    /// BM25 over the role text, the same fallback recall uses when the embedder
    /// is down. This pins the LIVE scorer's behavior without an inference call.
    #[test]
    fn lexical_agent_scorer_ranks_role_overlap_and_is_pure() {
        use super::{AgentScorer, LexicalAgentScorer};
        let scorer = LexicalAgentScorer;
        // hercules: "Fitness + Nutrition: training and nutrition coaching".
        // gecko:    "Markets + Capital: market watch, trading research (...)".
        let roles = ["Fitness + Nutrition: training and nutrition coaching", "Markets + Capital: market watch, trading research"];
        let scores = scorer.score("plan my nutrition and training", &roles);
        assert_eq!(scores.len(), 2);
        assert!(
            scores[0] > scores[1],
            "the fitness role must out-score the markets role: {scores:?}"
        );
        assert!(scores[0] > 0.0, "a real overlap must be positive: {scores:?}");
        // No overlap at all -> all-zero (no signal), which degrades to darwin.
        let none = scorer.score("zzz qqq vvv", &roles);
        assert!(
            none.iter().all(|&s| s == 0.0),
            "a non-overlapping utterance must score all-zero: {none:?}"
        );
    }

    /// End-to-end with the SHIPPED scorer (no mock): an ambiguous fitness
    /// question that matches no cue routes to hercules through the real
    /// LexicalAgentScorer, while a generic chit-chat with no role overlap stays
    /// with the orchestrator. This is the live wiring the router uses.
    #[test]
    fn live_lexical_fallback_routes_fitness_and_keeps_chitchat_on_orchestrator() {
        let reg = AgentRegistry::canonical();
        let scorer = super::LexicalAgentScorer;
        // "coaching" overlaps hercules' role text ("...nutrition coaching") but is
        // NOT one of the deterministic hercules cue tokens (workout/exercise/
        // training/train/nutrition/diet/macros/fitness/lift/run), so select()
        // alone falls to the orchestrator and only the lexical fallback recovers
        // the fitness specialist.
        let fitness = "i could really use some coaching";
        assert_eq!(
            reg.select("conversation", fitness, true).name,
            "darwin",
            "precondition: no deterministic cue matched"
        );
        assert_eq!(
            reg.select_with_fallback("conversation", fitness, true, &scorer).name,
            "hercules",
            "the live lexical fallback should reach hercules"
        );
        // Generic chit-chat with no role overlap stays on the orchestrator.
        assert_eq!(
            reg.select_with_fallback("conversation", "hello there, lovely day", true, &scorer).name,
            "darwin",
            "generic chit-chat with no role overlap stays with the orchestrator"
        );
    }

    /// EDITH owns anticipation cues but must NOT poach friday's daily-intel
    /// cues (brief/morning/news/schedule). Pinned separately because the two
    /// agents are adjacent in domain and the contract forbids the theft.
    #[test]
    fn edith_owns_anticipation_without_stealing_fridays_intel() {
        let reg = AgentRegistry::canonical();
        // Anticipation phrases -> edith.
        for q in [
            "heads up on anything urgent",
            "anticipate what i'll need today",
            "be proactive about my calendar",
            "watch for low disk",
            "keep an eye on the markets",
            "alert me if something comes up",
            "what's coming up next",
            "what should i know",
            "anything i should know",
        ] {
            assert_eq!(
                reg.select("conversation", q, true).name,
                "edith",
                "anticipation cue should route to edith: {q:?}"
            );
        }
        // friday's daily-intel cues are untouched -> still friday.
        for q in [
            "give me the morning brief",
            "what's the news",
            "read me my schedule",
            "anything change overnight",
            "what's my agenda today",
        ] {
            assert_eq!(
                reg.select("conversation", q, true).name,
                "friday",
                "friday's intel cue must NOT be poached by edith: {q:?}"
            );
        }
    }

    /// FURY owns multi-step MISSION cues — but must NOT poach the roll-call
    /// GREETING phrases ("assemble the team", "meet the team": those want the
    /// team to SPEAK, handled earlier by is_roll_call), nor hijack an ordinary
    /// SINGLE-task request that merely mentions a specialist's domain.
    #[test]
    fn fury_owns_missions_without_stealing_roll_call_or_single_tasks() {
        let reg = AgentRegistry::canonical();
        // Multi-step mission phrasings -> fury.
        for q in [
            "run point on the product launch",
            "this is a multi-step mission",
            "handle all of the onboarding for me",
            "take care of everything for the trip",
            "coordinate the whole thing across the team",
            "orchestrate the release end to end",
            "spin up a campaign and drive it to done",
            "get the team on the rebrand",
            "put the team on the migration",
        ] {
            assert_eq!(
                reg.select("conversation", q, true).name,
                "fury",
                "mission cue should route to fury: {q:?}"
            );
        }
        // Roll-call GREETING phrases are NOT mission queries (is_roll_call, which
        // the daemon checks BEFORE select, owns those). At the select() layer
        // they must not be swept up by fury either: "meet the team" carries no
        // FURY cue, so it falls through to the orchestrator, not fury.
        assert_eq!(
            reg.select("conversation", "meet the team", true).name,
            "darwin",
            "a roll-call greeting must not be treated as a mission"
        );
        // Ordinary SINGLE-task requests still reach their domain specialist, not
        // fury — a mission needs an explicit multi-step cue, not a stray keyword.
        for (q, owner) in [
            ("draft a caption for this post", "veronica"),
            ("give me the morning brief", "friday"),
            ("investigate this bug in the build", "steve"),
            ("play some music", "jerome"),
        ] {
            assert_eq!(
                reg.select("conversation", q, true).name,
                owner,
                "a single-domain task must NOT be hijacked by fury: {q:?}"
            );
        }
    }

    /// CASSANDRA owns MODELING cues — simulate/forecast/scenario/monte carlo/
    /// what-if/odds/probability — but must NOT poach gecko's live MARKET watch
    /// (market/trade/stock/crypto/portfolio/ticker). Pinned separately because
    /// the two are adjacent in domain (both can touch a price) and the contract
    /// forbids the theft: Cassandra runs the numbers, gecko quotes the tape.
    #[test]
    fn cassandra_owns_forecasts_without_stealing_geckos_market_watch() {
        let reg = AgentRegistry::canonical();
        // Modeling phrases -> cassandra.
        for q in [
            "simulate the next twelve months",
            "run a simulation of this plan",
            "forecast revenue for the quarter",
            "model this scenario",
            "run a few scenarios",
            "run a monte carlo on this",
            "what if rates double",
            "what-if the cost goes up",
            "project this out a year",
            "give me a projection",
            "what are the odds",
            "what's the probability of hitting target",
            "estimate the likelihood",
            // A modeling verb wins even over a market-ish noun: Cassandra runs
            // the numbers, she does not quote the live tape.
            "simulate this stock over a year",
            "forecast the portfolio",
        ] {
            assert_eq!(
                reg.select("conversation", q, true).name,
                "cassandra",
                "forecast/sim cue should route to cassandra: {q:?}"
            );
        }
        // gecko's live-market cues are untouched -> still gecko (no modeling verb
        // present: these ask what the market IS doing, not to model it). Phrases
        // use gecko's actual whole-word cues (market/trade/stock/portfolio/crypto/
        // ticker) so the test pins gecko's real behavior, not a near-miss.
        for q in [
            "what's the market doing",
            "any good trade today",
            "how's my portfolio looking",
            "pull up the stock price",
            "check the crypto ticker",
        ] {
            assert_eq!(
                reg.select("conversation", q, true).name,
                "gecko",
                "gecko's live-market cue must NOT be poached by cassandra: {q:?}"
            );
        }
        // Whole-word safety: "project" the verb fires, but it must not fire as a
        // substring of "projector" (no cue) — falls through to the orchestrator.
        assert_eq!(
            reg.select("conversation", "turn on the projector", true).name,
            "darwin",
            "a substring of a cue word must not misroute"
        );
    }

    /// MNEMOSYNE owns RETRIEVAL phrasings — what did I say / what do you
    /// remember about / dig up / have we discussed / when did I — but must NOT
    /// poach pepper's STORE + reminder cues (remember to, remind me, set a
    /// reminder). Pinned separately because the two are the two halves of memory
    /// (pepper writes, Mnemosyne reads) and the contract forbids the theft.
    #[test]
    fn mnemosyne_owns_retrieval_without_stealing_peppers_store_cues() {
        let reg = AgentRegistry::canonical();
        // Retrieval phrasings -> mnemosyne (even when labeled memory.recall, the
        // intent pepper otherwise owns: retrieval is the more specific intent).
        for q in [
            "what did i say about the deadline",
            "what did i tell you about the car",
            "what do you remember about my coffee order",
            "dig up that note i left",
            "recall everything about the project",
            "find that note on the budget",
            "when did i mention the dentist",
            "have we discussed the new hire",
            "did we discuss pricing",
            "surface what you know about the launch",
            "what have i told you about my pet",
            "remind me what i said about the trip",
            "pull up what i said earlier",
        ] {
            assert_eq!(
                reg.select("conversation", q, true).name,
                "mnemosyne",
                "retrieval cue should route to mnemosyne: {q:?}"
            );
        }
        // A memory.recall-INTENT retrieval phrasing still wins for mnemosyne over
        // pepper's intent ownership of memory.recall.
        assert_eq!(
            reg.select("memory.recall", "what do you remember about my license plate", true).name,
            "mnemosyne",
            "a retrieval phrasing beats pepper's memory.recall intent ownership"
        );
        // pepper's STORE + reminder cues are untouched -> still pepper. These are
        // WRITE/schedule requests, not retrieval, so Mnemosyne must not take them.
        for q in [
            "remember that i like jazz",
            "remember my license plate is 7ABC123",
            "remind me to call mom",
            "set a reminder to water the plants",
            "put this on my calendar",
        ] {
            assert_eq!(
                reg.select("conversation", q, true).name,
                "pepper",
                "pepper's store/reminder cue must NOT be poached by mnemosyne: {q:?}"
            );
        }
        // The bare store INTENT also stays with pepper (no retrieval phrasing).
        assert_eq!(
            reg.select("memory.store", "remember my license plate", true).name,
            "pepper",
            "the store intent stays with pepper"
        );
    }

    /// ON-DEVICE FILE RAG: the index/forget triggers ("index my documents",
    /// "reindex", "forget my file index") route to MNEMOSYNE (she owns the whole
    /// file-RAG surface: doc_search + docsearch.index + docsearch.forget), and she
    /// may_use both — so the router never reroutes the turn away from her and the
    /// HUD attributes the index/forget to the agent that actually acts. owner_of
    /// resolves each intent to her too, so a denied/orphan path would still land on
    /// the real owner rather than collapsing to the orchestrator default.
    #[test]
    fn mnemosyne_owns_the_file_rag_index_and_forget_triggers() {
        let reg = AgentRegistry::canonical();
        let mnemosyne = reg.get("mnemosyne").unwrap();
        for intent in ["docsearch.index", "docsearch.forget"] {
            // The classified intent routes to mnemosyne (the text is incidental;
            // routing is intent-driven for these concrete file-RAG actions).
            assert_eq!(
                reg.select(intent, "index my documents", true).name,
                "mnemosyne",
                "{intent} must route to mnemosyne (the file-RAG owner)"
            );
            // She holds the tool, so enforce_tool never reroutes the turn.
            assert!(
                mnemosyne.may_use(intent),
                "mnemosyne must be allowed to use {intent}"
            );
            // owner_of resolves the intent to a real specialist owner (mnemosyne),
            // never the orchestrator.
            let owner = reg.owner_of(intent).unwrap();
            assert_eq!(owner.name, "mnemosyne", "owner_of({intent}) must be mnemosyne");
            assert!(!owner.is_orchestrator());
        }
        // She also owns the read side (doc_search) — the full surface in one agent.
        assert!(mnemosyne.may_use("doc_search"));
    }

    /// KNOWLEDGE GRAPH: building/mapping the user's indexed documents into the
    /// shared world model is MNEMOSYNE's remit. The router accepts TWO intent
    /// tokens for the build — the canonical `docsearch.build_graph` and the
    /// `knowledge.build` alias — and select() maps BOTH to her. This pins that
    /// she also may_use BOTH (so enforce_tool never reroutes the alias turn to
    /// the orchestrator) and that owner_of resolves each to her, never the
    /// orchestrator — keeping the build honestly attributed to the knowledge agent.
    #[test]
    fn mnemosyne_owns_both_knowledge_graph_build_intents() {
        let reg = AgentRegistry::canonical();
        let mnemosyne = reg.get("mnemosyne").unwrap();
        for intent in ["docsearch.build_graph", "knowledge.build"] {
            // The classified intent routes to mnemosyne (text incidental).
            assert_eq!(
                reg.select(intent, "build a knowledge graph from my documents", true).name,
                "mnemosyne",
                "{intent} must route to mnemosyne (the document-knowledge owner)"
            );
            // She holds the tool, so enforce_tool never reroutes the turn away
            // from her (no agent.reroute, no mis-attribution to the orchestrator).
            assert!(
                mnemosyne.may_use(intent),
                "mnemosyne must be allowed to use {intent}"
            );
            // owner_of resolves the intent to a real specialist owner (mnemosyne),
            // never the orchestrator fallback.
            let owner = reg.owner_of(intent).unwrap();
            assert_eq!(owner.name, "mnemosyne", "owner_of({intent}) must be mnemosyne");
            assert!(!owner.is_orchestrator());
        }
    }

    /// SAGE owns THOROUGH, CITED, multi-source deep research — but must NOT poach
    /// vision's QUICK lookup (the bare "research/osint/lookup/investigate/
    /// footprint" tokens). Pinned separately because the two are adjacent in
    /// domain (both touch "research") and the contract draws the boundary: vision
    /// = a fast pass on authorized targets; sage = a bounded multi-source CITED
    /// report. SAGE only claims the DEEP/CITED variant.
    #[test]
    fn sage_owns_deep_research_without_stealing_visions_quick_lookup() {
        let reg = AgentRegistry::canonical();
        // Deep/cited phrasings -> sage (even when "research" appears inside them).
        for q in [
            "do a deep dive on the topic",
            "i want a deep research pass",
            "give me a research report",
            "do a thorough investigation",
            "research the literature on this",
            "cite your sources for that",
            "answer with citations",
            "back it up with sources",
            "a comprehensive overview please",
            "tell me everything about quantum computing",
            "look into this thoroughly",
            "research it comprehensively",
            "do a literature review",
            "i need this properly sourced",
            "what does the literature say",
        ] {
            assert_eq!(
                reg.select("conversation", q, true).name,
                "sage",
                "a deep/cited research cue should route to sage: {q:?}"
            );
        }
        // vision's QUICK-lookup cues are untouched -> still vision (no depth or
        // citation cue: these are fast passes, not a cited multi-source report).
        for q in [
            "research our competitors",
            "run some osint on this handle",
            "investigate this domain",
            "look up their ad spend",
            "check their footprint",
            "research the competitor ad trends",
        ] {
            assert_eq!(
                reg.select("conversation", q, true).name,
                "vision",
                "vision's quick-lookup cue must NOT be poached by sage: {q:?}"
            );
        }
        // Whole-word safety: a depth adverb with NO investigation context must not
        // fire (e.g. "clean it thoroughly" is not research) — falls through.
        assert_eq!(
            reg.select("conversation", "clean the kitchen thoroughly", true).name,
            "darwin",
            "a depth adverb alone (no investigation context) must not route to sage"
        );
    }

    /// VITALIS owns the BIOMETRIC reads (recovery/strain/HRV/sleep-score/
    /// readiness/whoop/"how did I sleep") but must NOT poach hercules' COACHING
    /// cues (workout/exercise/training/nutrition/diet/macros/fitness/lift/run).
    /// Pinned separately because the two agents share the health domain and the
    /// contract forbids the theft: hercules programs the work, Vitalis reads the
    /// signal.
    #[test]
    fn vitalis_owns_biometrics_without_stealing_hercules_coaching() {
        let reg = AgentRegistry::canonical();
        // Body-signal reads -> vitalis.
        for q in [
            "how recovered am i",
            "what's my recovery score",
            "what's my strain today",
            "what's my hrv",
            "what's my readiness",
            "what's my whoop sleep score",
            "how did i sleep last night",
            "what's my resting heart rate",
            "what are my biometrics",
            "how's my body",
        ] {
            assert_eq!(
                reg.select("conversation", q, true).name,
                "vitalis",
                "a biometric read cue should route to vitalis: {q:?}"
            );
        }
        // hercules' COACHING cues are untouched -> still hercules (programming the
        // training and the diet, not reading the band). These use hercules' own
        // cue tokens (workout/training/nutrition/diet/macros/lift/run/fitness).
        for q in [
            "plan my workout",
            "fix my diet for today",
            "give me a training program",
            "help me with my nutrition and macros",
            "i want to lift heavier",
            "plan my run for tomorrow",
            "design my fitness routine",
        ] {
            assert_eq!(
                reg.select("conversation", q, true).name,
                "hercules",
                "hercules' coaching cue must NOT be poached by vitalis: {q:?}"
            );
        }
        // Whole-word safety: "strain" is a cue, but it must fire as a whole word
        // (no embedded false positive), and a bare "sleep" (too broad) is NOT a
        // single-token cue, so "set a sleep timer" does not route to vitalis.
        assert_ne!(
            reg.select("conversation", "set a sleep timer", true).name,
            "vitalis",
            "a bare 'sleep' (no biometric phrasing) must not route to vitalis"
        );
    }

    /// KAREN owns the TRIAGE/INBOX reads (triage/inbox/unread/"catch me up on
    /// messages"/"what needs a reply"/"clear my inbox"/"who needs me"/"my email"/
    /// "draft a reply") but must NOT poach veronica's COMPOSE cues (the broad
    /// content/post/caption/draft/write/copy/message/reply/tweet/email tokens).
    /// Pinned separately because the two agents share the comms domain and the
    /// contract forbids the theft: veronica composes original content, Karen
    /// triages the inbound and drafts a reply to a specific message.
    #[test]
    fn karen_owns_triage_without_stealing_veronicas_compose_cues() {
        let reg = AgentRegistry::canonical();
        // Triage/inbox reads -> karen.
        for q in [
            "triage my inbox",
            "catch me up on my messages",
            "catch me up on messages",
            "what needs a reply",
            "what needs my reply",
            "who needs me",
            "clear my inbox",
            "clear out my inbox",
            "anything unread for me",
            "draft a reply to that email",
            "draft a response to her",
            "what's in my email",
            "go through my messages",
        ] {
            assert_eq!(
                reg.select("conversation", q, true).name,
                "karen",
                "a triage/inbox cue should route to karen: {q:?}"
            );
        }
        // veronica's COMPOSE cues are untouched -> still veronica (composing
        // ORIGINAL content, not triaging the inbox). These use veronica's own bare
        // cue tokens (content/post/caption/draft/write/message/reply/tweet) with NO
        // inbox/triage framing, so Karen must not take them.
        for q in [
            "draft a caption for this post",
            "write a tweet about the launch",
            "draft some copy for the landing page",
            "compose a message to the team",
            "write a reply to post under this",
        ] {
            assert_eq!(
                reg.select("conversation", q, true).name,
                "veronica",
                "veronica's compose cue must NOT be poached by karen: {q:?}"
            );
        }
        // Whole-word safety: "inbox" the cue fires as a token, but a near-miss with
        // no triage framing ("turn on the projector") still falls through to darwin.
        assert_eq!(
            reg.select("conversation", "turn on the projector", true).name,
            "darwin",
            "a non-cue must not misroute to karen"
        );
    }

    /// MIDAS owns the PERSONAL-finance reads (balance/balances/spending/
    /// transactions/budget/"how much did I spend"/"my accounts"/"cash flow"/
    /// "net worth"/"where's my money") but must NOT poach gecko's MARKET watch
    /// (market/trade/stock/crypto/portfolio/ticker). Pinned separately because the
    /// two agents share the money domain and the contract forbids the theft: gecko
    /// quotes the live tape and researches trades; Midas reads the user's own bank
    /// balances and spending. HARD RULE: Midas reads only — it can never move money.
    #[test]
    fn midas_owns_personal_finance_without_stealing_geckos_market_watch() {
        let reg = AgentRegistry::canonical();
        // Personal-finance reads -> midas.
        for q in [
            "what's my balance",
            "show me my balances",
            "what's my account balance",
            "how much did i spend last week",
            "how much have i spent on food",
            "where's my money going",
            "where did my money go",
            "show my transactions",
            "how am i tracking against my budget",
            "what's my cash flow this month",
            "what's my net worth",
            "what's in my accounts",
            "how much money do i have",
            "what's my spending look like",
        ] {
            assert_eq!(
                reg.select("conversation", q, true).name,
                "midas",
                "a personal-finance cue should route to midas: {q:?}"
            );
        }
        // gecko's live-MARKET cues are untouched -> still gecko (these ask about the
        // markets/trades, not the user's own bank balances). Midas claims no
        // market/trade/stock/crypto/portfolio/ticker token, so the boundary holds.
        for q in [
            "what's the market doing",
            "any good trade today",
            "how's my portfolio looking",
            "pull up the stock price",
            "check the crypto ticker",
        ] {
            assert_eq!(
                reg.select("conversation", q, true).name,
                "gecko",
                "gecko's market cue must NOT be poached by midas: {q:?}"
            );
        }
        // Whole-word safety: "balance" the cue fires as a token, but it must not
        // fire as a substring of "unbalanced" or "rebalance" (no personal-finance
        // framing) — those fall through to the orchestrator.
        assert_eq!(
            reg.select("conversation", "the team feels unbalanced", true).name,
            "darwin",
            "a substring of a cue word must not misroute to midas"
        );
    }

    /// MIDAS holds the read trio and NO money-moving tool — at the roster level the
    /// allowlist must contain exactly the three midas_* reads (plus the shared
    /// conversation/recall) and nothing that could transfer/pay/trade. This pins the
    /// HARD RULE in the source of truth: no money-moving tool may ever be added to
    /// Midas, not even a gated one.
    #[test]
    fn midas_allowlist_is_read_only_no_money_movement() {
        let reg = AgentRegistry::canonical();
        let midas = reg.get("midas").expect("midas is on the roster");
        // The three reads are present.
        for read in ["midas_balances", "midas_transactions", "midas_spending"] {
            assert!(midas.may_use(read), "midas must hold the read tool {read}");
        }
        // No money-moving tool is present — not under any plausible name.
        for forbidden in [
            "midas_transfer",
            "midas_pay",
            "midas_payment",
            "midas_send_money",
            "midas_trade",
            "midas_buy",
            "midas_sell",
            "midas_move_money",
            "plaid_transfer",
        ] {
            assert!(
                !midas.may_use(forbidden),
                "MIDAS NEVER MOVES MONEY — it must not hold {forbidden}"
            );
        }
        // Midas is NOT the orchestrator (no wildcard) — its surface is exactly its
        // listed read tools.
        assert!(!midas.is_orchestrator(), "midas must not hold the tools wildcard");
        // It also does not hold any other agent's consequential money tools (the ads
        // spend tools live with stark/gecko, never Midas).
        for ads in ["gads_set_budget", "meta_set_budget", "gads_pause_campaign"] {
            assert!(!midas.may_use(ads), "midas must not hold ads spend tool {ads}");
        }
    }

    /// VOYAGER owns the READ-ONLY travel/logistics cues — directions/route/navigate,
    /// travel-time/ETA, "how far", and nearby/find-a place searches — but must NOT
    /// poach a neighbouring domain, and must NOT claim a booking/payment request
    /// (booking is out of scope: Voyager reads routes/places/times, it never
    /// reserves or pays). Pinned separately because the navigation vocabulary brushes
    /// up against oracle (apps), friday (schedule), and gecko (markets).
    #[test]
    fn voyager_owns_travel_reads_without_booking_or_poaching() {
        let reg = AgentRegistry::canonical();
        // Travel/logistics READS -> voyager.
        for q in [
            "directions to the museum",
            "what's the fastest route to work",
            "navigate me home",
            "how long to get to the airport",
            "how long does it take to get downtown",
            "what's the travel time to the stadium",
            "what's the eta",
            "how far is it to the coast",
            "find a coffee near the office",
            "is there a good restaurant near here",
            "any parking nearby",
            "show me a map of the neighborhood",
        ] {
            assert_eq!(
                reg.select("conversation", q, true).name,
                "voyager",
                "a travel/logistics cue should route to voyager: {q:?}"
            );
        }
        // Booking/payment is OUT OF SCOPE: a "book me a flight/hotel/ride" request
        // carries no routes/places/ETA cue, so Voyager does NOT claim it — it falls
        // through to the orchestrator rather than implying a reservation it cannot
        // make. (The persona/copy also refuse booking explicitly.)
        for q in [
            "book me a flight to paris",
            "reserve a hotel room for friday",
            "pay for my uber",
            "order me a taxi",
        ] {
            assert_eq!(
                reg.select("conversation", q, true).name,
                "darwin",
                "a booking/payment request must NOT be claimed by voyager: {q:?}"
            );
        }
        // Whole-word safety: "map" the cue fires as a token, not as a substring of
        // "mapped"/"mapping" with no travel framing.
        assert_eq!(
            reg.select("conversation", "we mapped out the quarterly plan", true).name,
            "darwin",
            "a substring of a cue word must not misroute to voyager"
        );
    }

    /// VOYAGER holds exactly the three READ-ONLY maps tools (plus the shared
    /// conversation/recall) and NOTHING that books or pays — at the roster level the
    /// allowlist pins the read-only, no-booking scope: no reservation/payment tool
    /// may ever be added, not even a gated one.
    #[test]
    fn voyager_allowlist_is_read_only_no_booking() {
        let reg = AgentRegistry::canonical();
        let voyager = reg.get("voyager").expect("voyager is on the roster");
        // The three reads are present.
        for read in ["voyager_directions", "voyager_places", "voyager_eta"] {
            assert!(voyager.may_use(read), "voyager must hold the read tool {read}");
        }
        // No booking/payment tool is present — not under any plausible name.
        for forbidden in [
            "voyager_book",
            "voyager_book_flight",
            "voyager_book_hotel",
            "voyager_reserve",
            "voyager_pay",
            "voyager_order_ride",
            "voyager_purchase",
            "maps_book",
        ] {
            assert!(
                !voyager.may_use(forbidden),
                "VOYAGER is READ-ONLY (no booking/payment) — it must not hold {forbidden}"
            );
        }
        // Voyager is NOT the orchestrator (no wildcard) — its surface is exactly its
        // listed read tools.
        assert!(!voyager.is_orchestrator(), "voyager must not hold the tools wildcard");
    }

    /// AEGIS owns the DEFENSIVE EXPOSURE/PRIVACY cues — have I been pwned, breach/
    /// breached/pwned, "am I exposed", "data leak", "security posture", "am I
    /// protected", "privacy check", filevault — but must NOT poach ultron's SECURITY
    /// MONITORING cues (monitor/monitoring/threat/intrusion/firewall/defend/
    /// defensive/lockdown). Pinned separately because the two agents share the
    /// security domain and the contract forbids the theft: ultron monitors the Mac/
    /// LAN for live threats; Aegis answers "where am I exposed" — a breach check on
    /// the user's own email and a read-only posture report of this machine.
    #[test]
    fn aegis_owns_exposure_without_stealing_ultrons_monitoring() {
        let reg = AgentRegistry::canonical();
        // Exposure/privacy reads -> aegis.
        for q in [
            "have i been pwned",
            "was i in a breach",
            "has my email been breached",
            "check for breaches on my account",
            "am i exposed in a data leak",
            "is there a data breach with my email",
            "what's my security posture",
            "am i protected on this machine",
            "run a privacy check",
            "is filevault turned on",
            "have my passwords leaked",
        ] {
            assert_eq!(
                reg.select("conversation", q, true).name,
                "aegis",
                "an exposure/privacy cue should route to aegis: {q:?}"
            );
        }
        // ultron's SECURITY-MONITORING cues are untouched -> still ultron (live
        // monitoring of the Mac/LAN, not a breach/exposure read). These use ultron's
        // own cue tokens (security/monitor/threat/intrusion/firewall/defend/lockdown),
        // so Aegis must not take them.
        for q in [
            "is there any security threat to monitor",
            "monitor for intrusion",
            "is the firewall up",
            "keep monitoring for threats",
            "set up defensive monitoring",
        ] {
            assert_eq!(
                reg.select("conversation", q, true).name,
                "ultron",
                "ultron's monitoring cue must NOT be poached by aegis: {q:?}"
            );
        }
        // Whole-word safety: "breach" the cue fires as a token, but must not fire as
        // a substring of "breaches" embedded in an unrelated word, and a near-miss
        // with no exposure framing falls through to the orchestrator.
        assert_eq!(
            reg.select("conversation", "we mapped out the quarterly plan", true).name,
            "darwin",
            "a non-cue must not misroute to aegis"
        );
    }

    /// AEGIS is DEFENSIVE-ONLY and READ-ONLY — at the roster level the allowlist must
    /// contain exactly the two read tools (plus the shared conversation/recall) and
    /// NOTHING offensive (no scanning/cracking/exploitation) and NOTHING that changes
    /// the machine (no remediation). This pins the HARD RAIL in the source of truth.
    #[test]
    fn aegis_allowlist_is_defensive_read_only() {
        let reg = AgentRegistry::canonical();
        let aegis = reg.get("aegis").expect("aegis is on the roster");
        // The read tools are present.
        for read in ["aegis_breach_check", "aegis_posture", "aegis_introspect", "aegis_report", "aegis_triage"] {
            assert!(aegis.may_use(read), "aegis must hold the read tool {read}");
        }
        // No offensive or remediation tool is present — not under any plausible name.
        for forbidden in [
            "aegis_scan",
            "aegis_scan_host",
            "aegis_portscan",
            "aegis_crack",
            "aegis_crack_password",
            "aegis_exploit",
            "aegis_attack",
            "aegis_enable_firewall",
            "aegis_turn_on_filevault",
            "aegis_remediate",
            "aegis_change_password",
            "aegis_reset_password",
        ] {
            assert!(
                !aegis.may_use(forbidden),
                "AEGIS is DEFENSIVE-ONLY, READ-ONLY — it must not hold {forbidden}"
            );
        }
        // Aegis is NOT the orchestrator (no wildcard) — its surface is exactly its
        // listed read tools. It also holds none of ultron's monitoring tools (it is
        // exposure/privacy, not live monitoring).
        assert!(!aegis.is_orchestrator(), "aegis must not hold the tools wildcard");
    }

    /// BABEL owns the RENDER-between-languages requests (translate/translation/
    /// "how do you say"/"what does X mean in Y"/"say this in <lang>"/interpret +
    /// the verb-gated "<verb> ... in <language>" shape) but must NOT poach
    /// veronica's COMPOSE cues (the bare content/post/write/message/draft tokens)
    /// nor misroute an unrelated mention of a language. Pinned separately because
    /// the verb-gated language path is the subtle part and the contract forbids the
    /// theft: veronica composes ORIGINAL content; babel turns words from one tongue
    /// into another.
    #[test]
    fn babel_owns_translation_without_stealing_veronicas_compose_cues() {
        let reg = AgentRegistry::canonical();
        // Translation phrasings -> babel.
        for q in [
            "translate this into spanish",
            "translate the menu for me",
            "what's the translation of this paragraph",
            "how do you say thank you in japanese",
            "how would you say hello in italian",
            "what does gracias mean in english",
            "say this in french",
            "say that in german",
            "put this in portuguese",
            "interpret what he said",
            "be my interpreter for this call",
            // The verb-gated "<verb> ... in <language>" shape.
            "say good morning in korean",
            "write this in russian",
            "how do you say it in arabic",
        ] {
            assert_eq!(
                reg.select("conversation", q, true).name,
                "babel",
                "a translation cue should route to babel: {q:?}"
            );
        }
        // veronica's COMPOSE cues are untouched -> still veronica (composing
        // ORIGINAL content, NOT translating). These carry no translate/interpret
        // cue and no "<verb> ... in <language>" shape, so babel must not take them.
        for q in [
            "draft a caption for this post",
            "write a tweet about the launch",
            "compose a message to the team",
            "draft some copy for the landing page",
        ] {
            assert_eq!(
                reg.select("conversation", q, true).name,
                "veronica",
                "veronica's compose cue must NOT be poached by babel: {q:?}"
            );
        }
        // A bare language mention with NO rendering verb must NOT route to babel:
        // "spanish" alone is not a translation request. "I learned spanish in
        // Spain" carries no render verb adjacent to a translation ask, so it falls
        // through to the orchestrator rather than misrouting.
        assert_eq!(
            reg.select("conversation", "i lived in spain and learned spanish", true).name,
            "darwin",
            "a bare language mention (no render verb) must not route to babel"
        );
        // Whole-word safety: "interpret" fires as a token, but a non-cue with no
        // translation framing falls through to the orchestrator.
        assert_eq!(
            reg.select("conversation", "what is the weather today", true).name,
            "darwin",
            "a non-cue must not misroute to babel"
        );
    }

    /// BABEL is READ-ONLY at the roster level: its allowlist is exactly the
    /// translate + turn-based interpret tools (plus the shared conversation/recall)
    /// and NOTHING that sends, posts, or changes anything. Pins the rail in the source
    /// of truth.
    #[test]
    fn babel_allowlist_is_read_only_translation() {
        let reg = AgentRegistry::canonical();
        let babel = reg.get("babel").expect("babel is on the roster");
        assert!(babel.may_use("babel_translate"), "babel must hold babel_translate");
        // The turn-based speech interpreter (translate -> speak in target language) is
        // ALSO a read-only render-and-voice tool; babel holds it.
        assert!(babel.may_use("babel_interpret"), "babel must hold babel_interpret");
        // No send/post/store/consequential tool of any plausible name.
        for forbidden in [
            "babel_send",
            "gmail_send",
            "slack_post_message",
            "x_post",
            "remember_fact",
            "memory.store",
        ] {
            assert!(
                !babel.may_use(forbidden),
                "BABEL is READ-ONLY — it must not hold {forbidden}"
            );
        }
        assert!(!babel.is_orchestrator(), "babel must not hold the tools wildcard");
    }

    /// file.op for a budget spreadsheet: the intent owner (vision) wins —
    /// there is no spurious keyword match. (Pinned separately because the
    /// inline comment above flagged the ambiguity.)
    #[test]
    fn file_op_intent_routes_to_vision_not_a_keyword() {
        let reg = AgentRegistry::canonical();
        assert_eq!(reg.select("file.op", "find my budget spreadsheet", true).name, "vision");
    }

    /// Offline survival: with the cloud unreachable, a conversational turn
    /// goes to hulk; a concrete local-action intent still reaches its owner so
    /// "open safari" keeps working with no uplink.
    #[test]
    fn offline_routes_conversation_to_hulk_but_actions_to_owners() {
        let reg = AgentRegistry::canonical();
        assert_eq!(reg.select("conversation", "tell me about mars", false).name, "hulk");
        // Local actions are unaffected by the cloud being down.
        assert_eq!(reg.select("app.launch", "open safari", false).name, "oracle");
        assert_eq!(reg.select("system.query", "system status", false).name, "ultron");
        // With the cloud up, the same conversational turn is the orchestrator's.
        assert_eq!(reg.select("conversation", "tell me about mars", true).name, "darwin");
    }

    /// Keyword matching is whole-word: a cue must not fire as a substring of a
    /// larger word — only as a bounded token.
    #[test]
    fn keyword_matching_is_whole_word_only() {
        let reg = AgentRegistry::canonical();
        // 'ad' is a vision cue but must not fire on "already" / "read".
        assert_eq!(reg.select("conversation", "i already read that", true).name, "darwin");
        // 'pr' is a steve cue but must not fire on "spring" / "appreciate".
        assert_eq!(reg.select("conversation", "i appreciate the spring weather", true).name, "darwin");
        // The bare cue words DO fire as whole tokens.
        assert_eq!(reg.select("conversation", "open a pr for this", true).name, "steve");
        assert_eq!(reg.select("conversation", "run the ad analysis", true).name, "vision");
    }

    // ---- Tool-allowlist isolation ----

    /// darwin may use any tool; a specialist may use only its own. An agent
    /// attempting another agent's exclusive tool is refused, and owner_of
    /// names where that tool belongs so the router can re-route.
    #[test]
    fn tool_allowlist_isolation_holds() {
        let reg = AgentRegistry::canonical();
        let darwin = reg.get("darwin").unwrap();
        let friday = reg.get("friday").unwrap();
        let jerome = reg.get("jerome").unwrap();
        let vision = reg.get("vision").unwrap();

        // Orchestrator: everything.
        assert!(darwin.may_use("open_app"));
        assert!(darwin.may_use("web_search"));
        assert!(darwin.may_use("anything_at_all"));

        // friday (intel, read-only) cannot open apps or search the web —
        // those are jerome/oracle and vision/stark territory.
        assert!(!friday.may_use("open_app"));
        assert!(!friday.may_use("web_search"));
        assert!(friday.may_use("system_status")); // its own tool
        assert!(friday.may_use("conversation"));

        // jerome (music) owns open_app/quit_app but NOT web_search.
        assert!(jerome.may_use("open_app"));
        assert!(!jerome.may_use("web_search"));

        // vision owns web_search but NOT open_app.
        assert!(vision.may_use("web_search"));
        assert!(!vision.may_use("open_app"));

        // owner_of points a denied tool at a real specialist owner (never the
        // orchestrator, who holds everything).
        let opener = reg.owner_of("open_app").unwrap();
        assert!(!opener.is_orchestrator());
        assert!(opener.tools.iter().any(|t| t == "open_app"));
        let searcher = reg.owner_of("web_search").unwrap();
        assert!(searcher.tools.iter().any(|t| t == "web_search"));
    }

    /// WORLD MODEL allowlists (documented policy): EVERY agent may READ the shared
    /// world (world_query) — they all reason over one coherent picture — but only
    /// the orchestrator and the KNOWLEDGE agents (friday, pepper, mnemosyne) may
    /// WRITE it (world_update). A non-knowledge specialist holds the read, not the
    /// write. The world model is a SHARED tier, so the write is shared
    /// user-knowledge (no gate); restricting WHO writes keeps the curated picture
    /// coherent.
    #[test]
    fn world_model_tool_grants_match_documented_policy() {
        let reg = AgentRegistry::canonical();

        // Every agent may READ the world.
        for a in reg.all() {
            assert!(
                a.may_use("world_query"),
                "agent '{}' must be able to read the shared world model",
                a.name
            );
        }

        // The orchestrator (wildcard) + the knowledge agents may WRITE it.
        let writers = ["darwin", "friday", "pepper", "mnemosyne"];
        for name in writers {
            let a = reg.get(name).unwrap();
            assert!(
                a.may_use("world_update"),
                "knowledge agent '{name}' must be able to update the world model"
            );
        }

        // A non-knowledge specialist holds the read but NOT the write.
        for name in ["vision", "jerome", "midas", "voyager", "babel"] {
            let a = reg.get(name).unwrap();
            assert!(a.may_use("world_query"), "{name} should read the world");
            assert!(
                !a.may_use("world_update"),
                "non-knowledge agent '{name}' must NOT write the shared world model"
            );
        }
    }

    /// Only darwin holds the wildcard; no specialist is secretly an
    /// orchestrator.
    #[test]
    fn only_darwin_is_the_orchestrator() {
        let reg = AgentRegistry::canonical();
        let orchestrators: Vec<&str> = reg
            .all()
            .iter()
            .filter(|a| a.is_orchestrator())
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(orchestrators, vec!["darwin"]);
    }

    // ---- Roll-call (item 3) ----

    /// The roll-call trigger fires on the canonical phrases and nothing else.
    #[test]
    fn roll_call_detects_the_trigger_phrases() {
        use super::is_roll_call;
        for yes in [
            "roll call",
            "Darwin, roll call",
            "introduce the team",
            "introduce yourselves please",
            "assemble",
            "assemble the team",
            "meet the team",
            "who is on the team",
            "who's on the team",
        ] {
            assert!(is_roll_call(yes), "should trigger: {yes:?}");
        }
        for no in [
            "open safari",
            "what's the weather",
            "i assembled the report yesterday", // 'assemble' is whole-word only
            "tell me about your team of engineers", // no trigger phrase
            "call my mom", // 'call' alone is not 'roll call'
        ] {
            assert!(!is_roll_call(no), "should NOT trigger: {no:?}");
        }
    }

    /// INTRO parsing pulls the first INTRO line, trims it, and ignores the
    /// rest of the persona body.
    #[test]
    fn intro_parses_the_first_intro_line() {
        use super::parse_intro;
        let body = "INTRO: Friday on intel, briefs and the news.\n\nYou are FRIDAY...";
        assert_eq!(
            parse_intro(body),
            Some("Friday on intel, briefs and the news.".to_string())
        );
        // Case-insensitive prefix, leading whitespace tolerated.
        assert_eq!(parse_intro("  intro:   hello there  "), Some("hello there".to_string()));
        // No INTRO line, or a blank one.
        assert_eq!(parse_intro("You are DARWIN, with no intro marker."), None);
        assert_eq!(parse_intro("INTRO:   "), None);
    }

    /// intro() falls back to a grounded name+role sentence when the persona
    /// file is absent — roll-call never goes silent on an agent.
    #[test]
    fn intro_falls_back_without_a_persona_file() {
        let reg = AgentRegistry::canonical();
        let jerome = reg.get("jerome").unwrap();
        // A directory with no persona files: every intro is the fallback.
        let empty = std::env::temp_dir().join(format!(
            "darwin-no-personas-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let intro = jerome.intro(&empty);
        assert!(intro.starts_with("Jerome"), "fallback must name the agent: {intro}");
        assert!(intro.contains("Leisure"), "fallback must carry the role: {intro}");
    }

    /// persona_name is the agent's name (the persona filename stem), and the
    /// persona_file path follows inference/personas/<name>.txt.
    #[test]
    fn persona_name_and_file_follow_the_name() {
        for &(name, ..) in CANONICAL_ROSTER {
            let reg = AgentRegistry::canonical();
            let a = reg.get(name).unwrap();
            assert_eq!(a.persona_name(), name);
            assert_eq!(a.persona_file, format!("inference/personas/{name}.txt"));
            assert_eq!(a.namespace, format!("agent.{name}"));
        }
    }
}

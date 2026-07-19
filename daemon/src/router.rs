use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::Result;
use serde_json::json;
use tracing::{error, info, warn};

use std::sync::Arc;

use crate::actions;
use crate::agents::{Agent, AgentRegistry};
use crate::anthropic;
use crate::apps::{self, AppRegistry};
use crate::config::Config;
use crate::inference::{Classification, DescribeOutcome, GenerateOutcome, InferenceClient};
use crate::memory::Memory;
use crate::speech;
use crate::telemetry;

/// Exchange pairs sent as chat history with every LLM-voiced reply.
const HISTORY_EXCHANGES: usize = 6;
/// Recent DARWIN replies passed as the anti-repeat `avoid` list on the cloud
/// conversation path. Opus 4.8 takes no temperature/top_p/top_k, so the only
/// lever against a greeting collapsing to one fixed reply is changing the
/// prompt per call — and the most recent few replies are what a repeated bare
/// "Hi DARWIN" would otherwise echo, so they are exactly what to dodge.
const AVOID_RECENT_REPLIES: usize = 4;
/// Most-recent facts injected into every LLM-voiced reply.
const FACTS_LIMIT: usize = 12;
/// Token budget for locally generated replies (persona keeps them short).
const GENERATE_MAX_TOKENS: u32 = 200;
/// Data note for the local degrade path when cloud completion fails: the
/// local model must still answer fully, not announce reduced capability.
const CLOUD_DEGRADE_NOTE: &str = "Cloud uplink unavailable - answer fully and directly from your own local knowledge; do not mention the uplink unless the user asks why an answer is brief.";

pub struct RouteOutcome {
    pub routed_to: &'static str,
    pub response: String,
    /// The agent that handled this request (Darwin-Prime delegation). Owns
    /// the persona/voice the reply was spoken in and the memory namespace the
    /// exchange is recorded under; main uses it for namespaced bookkeeping and
    /// the HUD already saw it via the agent.active telemetry route() emitted.
    pub agent: String,
    /// The handling agent's memory namespace ("agent.<name>"); main tags the
    /// recorded transcript/exchange with it so recall stays per-agent.
    pub namespace: String,
    /// Set when route() already spoke the reply (the streamed converse
    /// path); main then skips speech::speak and uses these timings.
    pub spoken: Option<SpokenReply>,
}

/// Timings for a reply that was spoken inside route() via converse.
pub struct SpokenReply {
    /// route() entry -> the server's done event (contract item 6).
    pub route_ms: u64,
    pub report: speech::SpeakReport,
}

/// What a local handler produces: verified data, not final prose. When
/// llm_voice is set the LLM phrases the reply in persona around `data`;
/// the raw data string itself is the spoken fallback if generation fails.
struct HandlerOutput {
    data: String,
    llm_voice: bool,
}

/// Contract policy: cloud iff complexity == "heavy" OR confidence below the
/// configured threshold; everything else is handled locally. Local
/// llm_voice replies are generated AND spoken here in one streamed converse
/// call (`started` is the utterance-pickup instant for first_audio timing);
/// cloud replies still come back as text for main to speak. `reply` is the
/// session main opened at utterance receipt (possibly already carrying the
/// instant opener) — every spoken path appends to it. `brief` is the
/// proactive first-contact brief when this utterance ended an away gap:
/// verified data assembled daemon-side, appended to the converse data so
/// the persona phrases it. It rides ONLY the LLM-voiced local path — never
/// a verbatim-spoken handler reply, and not the cloud tool loop (per the
/// proactive contract the brief is converse data).
#[allow(clippy::too_many_arguments)]
pub async fn route(
    class: &Classification,
    text: &str,
    cfg: &Config,
    memory: &Memory,
    infer: &mut InferenceClient,
    started: Instant,
    reply: &mut speech::ReplySession,
    brief: Option<&str>,
    app_registry: &Arc<AppRegistry>,
    agents: &AgentRegistry,
    cloud_reachable: bool,
    root: &Path,
) -> Result<RouteOutcome> {
    let route_entry = Instant::now();

    // VAULT MODE ("go dark", vault.rs) + THRESHOLD GUEST (guest = local-only) — SEAM 1
    // of 2. Fold BOTH an active vault AND a guest turn into THIS turn's cloud
    // reachability ONCE, at entry, so EVERY downstream cloud decision that consults
    // `cloud_reachable` (the conversation brain, the roster answer, capability routing,
    // agent selection) deterministically sees NO cloud this turn and stays on the local
    // MLX brain. RESTRICT-ONLY + COMPOSABLE (`guest OR vault -> local`): each
    // `deny_cloud` can only turn a reachable cloud UNREACHABLE, never the reverse, so
    // with BOTH off this is byte-for-byte today's `cloud_reachable` (the owner still
    // uses the cloud by default). GUEST rationale: a bystander's turn must never reach
    // the owner's PAID cloud — that would append an obol spend row + bump the owner's
    // daily budget (a durable, owner-readable trace) and egress the guest's turn under
    // the owner's key. The actuating tool-loop gate (which does not consult
    // reachability) is closed separately at SEAM 2 below.
    let cloud_reachable = crate::threshold::deny_cloud(crate::vault::deny_cloud(cloud_reachable));

    // PANIC / LOCKDOWN (task #12) — THE emergency stop, honored BEFORE anything
    // else, even mid-confirmation / mid-anything. Any panic phrase ("panic",
    // "lockdown", "stop everything", "kill switch", "shut it all down") engages
    // the emergency stop NOW: it sets the process-global flag (so every master
    // gate reads OFF from this instant — consequential, proactive, MCP, standing,
    // heal, forge, the mic), DROPS any parked confirmation, PERSISTS a marker (so
    // a restart re-enters lockdown), audits the event, and speaks an honest
    // confirmation. This runs FIRST so even a parked outward action awaiting a
    // spoken yes is killed rather than confirmed. It is the SPOKEN twin of the
    // HUD `Command::Panic` verb. HONEST: it stops FUTURE actions + the mic and
    // persists; it cannot undo an action already executed.
    if crate::lockdown::is_panic_intent(text) {
        let msg = crate::lockdown::panic().await;
        telemetry::emit("system", "lockdown.panic", json!({"via": "voice"}));
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        return Ok(RouteOutcome {
            routed_to: "local",
            response: msg.to_string(),
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // UNLOCK (task #12) — the explicit, deliberate USER resume ("unlock" / "resume
    // normal" / "end lockdown"). This is the SPOKEN twin of the HUD
    // `Command::Unlock` verb and, together with it, the ONLY path to
    // `lockdown::unlock` — there is NO route from the model tool loop, an MCP
    // server, or injected/agent text. It clears the flag (every gate returns to
    // its CONFIGURED value — lockdown was an overlay, nothing was clobbered) and
    // removes the marker (the next restart comes up normal). Recognized here on
    // the user voice path, before normal routing, so it can lift a live lockdown
    // even though most surfaces are forced off.
    if crate::lockdown::is_unlock_intent(text) {
        let msg = crate::lockdown::unlock().await;
        telemetry::emit("system", "lockdown.unlock", json!({"via": "voice"}));
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        return Ok(RouteOutcome {
            routed_to: "local",
            response: msg.to_string(),
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // CROSS-TURN SPOKEN CONFIRMATION GATE (round F). When a consequential action
    // is awaiting a spoken human "yes" (parked last turn by execute_tool while
    // the master switch is on), THIS utterance is first read as a reply to that
    // pending — BEFORE any classifier/cloud/conversation routing — exactly like
    // the roll-call pre-check. The classifier never sees a "confirm"/"cancel" in
    // this state, so the parked action's fate is decided deterministically here:
    //   * Affirm  -> REPLAY the EXACT parked {tool,input} in Execute mode (the
    //                only thing in the whole system that fires a real action),
    //                speak the result, clear the slot.
    //   * Deny    -> clear the slot, acknowledge ("Cancelled.").
    //   * Unrelated -> clear the slot (so a stray later command can NEVER be
    //                mistaken as confirming the stale action) and fall through to
    //                route THIS utterance normally.
    // A pending older than the TTL is treated as already gone (take_live drops
    // it). The slot also shares the barge / roll-call-cancel lifecycle
    // (speech::clear_barge_in -> confirm::clear), so an interrupted turn never
    // leaves an action armed.
    if let Some(pending) = crate::confirm::take_live(Instant::now()) {
        // Decision is a PURE function of (utterance, pending): Affirm -> replay
        // the EXACT parked action; Deny -> a spoken cancel; Unrelated -> drop and
        // fall through. The slot was already emptied by take_live, so whatever
        // happens the stale action can never later be confirmed.
        let namespace = pending.agent.clone();
        let tool = pending.tool.clone();
        match crate::confirm::resolve_reply(pending, text, |t| {
            format!("Cancelled. I won't {}.", action_phrase(t))
        }) {
            crate::confirm::Resolution::Replay(pending) => {
                // The human said yes. Replay in Execute mode: SAME tool+input,
                // re-checking the parked agent's allowlist AND the master switch
                // (replay_confirmed_action enforces both). Nothing is re-derived
                // from this utterance — only what was previewed can fire.
                let (outcome, is_error) =
                    anthropic::replay_confirmed_action(&pending, memory).await;
                let agent = agent_for_namespace(agents, &namespace);
                emit_agent_active(agent);
                // PLAN-APPLY: a replay can RE-PARK instead of executing when the
                // action's state drifted since its plan was shown (plan.rs). In that
                // case a FRESH pending now sits in the slot (and the re-park already
                // published its own `confirm.parked` + drift `plan.diff`), so we must
                // NOT emit `confirm.affirmed` — that HUD event clears the just-shown
                // diff and would blank the panel for an action still awaiting confirm.
                // The action resolved (executed/errored) iff the slot is now EMPTY.
                let reparked = crate::confirm::peek_pending(Instant::now()).is_some();
                if !reparked {
                    telemetry::emit(
                        "system",
                        "confirm.affirmed",
                        json!({"tool": tool, "is_error": is_error}),
                    );
                }
                return Ok(RouteOutcome {
                    routed_to: "local",
                    response: outcome,
                    agent: agent.name.clone(),
                    namespace,
                    spoken: None,
                });
            }
            crate::confirm::Resolution::Cancelled(ack) => {
                let agent = agent_for_namespace(agents, &namespace);
                emit_agent_active(agent);
                telemetry::emit("system", "confirm.denied", json!({"tool": tool}));
                return Ok(RouteOutcome {
                    routed_to: "local",
                    response: ack,
                    agent: agent.name.clone(),
                    namespace,
                    spoken: None,
                });
            }
            crate::confirm::Resolution::PassThrough => {
                // Neither yes nor no: the user moved on. The slot is already
                // cleared, so the stale action can never later be confirmed. Fall
                // through and route THIS utterance normally. No tacked-on note —
                // the normal reply to the new utterance follows immediately;
                // telemetry records the drop for the HUD/audit.
                telemetry::emit("system", "confirm.dropped_unrelated", json!({"tool": tool}));
            }
        }
    }

    // VOICE-ID STRICT SCOPE (round G): under [voice_id].gate_scope = "all", an
    // unrecognized speaker is blocked from EVERY command, not just outward ones.
    // This runs AFTER the confirmation pre-check (so a parked action's own
    // voice-gated replay still resolves) but BEFORE any other routing, so an
    // unverified bystander gets nothing under the strict posture. Under the
    // DEFAULT "consequential" scope `allow_noncly()` is always true and this is a
    // no-op; with voice-id off/unenrolled the gate is OFF and it is a no-op too —
    // the consequential-only layer in execute_tool/replay still applies as the
    // common case. ADDITIVE: never permits anything the other gates would block.
    if !crate::voiceid::current_turn_gate().allow_noncly() {
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        telemetry::emit("system", "voiceid.denied", json!({"phase": "all_scope"}));
        return Ok(RouteOutcome {
            routed_to: "local",
            response: crate::voiceid::unrecognized_refusal(),
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // THRESHOLD — GUEST MODE fast-path gate. A guest scope confines a bystander to
    // plain conversation, translation, and non-personal status. EVERY specialized
    // route() fast path below BYPASSES the tool-loop + recall gates and either READS
    // the owner's personal data (activity / screen / clipboard / notebooks / reports
    // / lifelog / rewind / decision traces / user-model / vision describe) or takes a
    // consequential / owner-CONTROL action (policy / model-swap / voice-mode / vault
    // / macros / undo / charts / artifacts / music / images / designs / audio). None
    // is safe for a bystander, so a guest turn that would trigger one is REFUSED
    // HERE — before it can fire, with NO read and NO write. A guest-safe turn
    // (conversation / translation / status) matches none of these anchored
    // classifiers and flows through to the already guest-gated conversational path.
    // On the owner path (no scope installed) this is a no-op and routing is
    // byte-for-byte today's.
    if crate::threshold::is_guest_turn() {
        if let Some(category) = guest_denied_fast_path(text, cfg) {
            telemetry::emit("local", "threshold.fast_path_refused", json!({"category": category}));
            let prime = agents.orchestrator();
            emit_agent_active(prime);
            return Ok(RouteOutcome {
                routed_to: "local",
                response: format!(
                    "I can't do that in guest mode — that would use the owner's {category}, \
                     which is off-limits to a guest. I can talk, translate, and give \
                     non-personal status. The owner can do it."
                ),
                agent: prime.name.clone(),
                namespace: prime.namespace.clone(),
                spoken: None,
            });
        }
    }

    // PER-ACTION POLICY VOICE COMMAND (consequential gate control): the user
    // spoken "always allow the <tool> action" / "never allow the <tool> action" /
    // "always ask before the <tool> action". CONSERVATIVELY anchored (only the
    // exact phrase shapes classify — a sentence that merely mentions a tool or
    // "allow" does NOT trigger; `policy::classify_policy_command` rejects every
    // near-miss). This is the SPOKEN twin of the HUD policy editor + the
    // authenticated-local `policy` command verb — the SAME USER-SET-ONLY write
    // path, never the model tool loop. It runs AFTER the owner voice-id all-scope
    // gate above (so an unrecognized bystander cannot set a policy) and BEFORE any
    // normal routing, so a policy utterance NEVER falls through to the model. On a
    // hit we apply the rule (which itself refuses if the layer is disabled or the
    // master ceiling would be exceeded at evaluate time) and SPEAK an honest ack.
    if let Some(ack) = crate::policy::handle_user_policy_text(text) {
        telemetry::emit("system", "policy.user_set", json!({"via": "voice"}));
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        return Ok(RouteOutcome {
            routed_to: "local",
            response: ack,
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // MODEL-SWAP VOICE COMMAND (model-tier control): "use the powerful model" /
    // "go offline" / "fast mode" / "auto". CONSERVATIVELY detected (anchored on
    // imperative model-control phrasing — a sentence that merely mentions
    // "fast"/"offline" does NOT trigger) and handled BEFORE any normal routing,
    // like roll-call/agent-query. This is MODEL-ONLY: it installs/clears the
    // process-global tier override that resolve_tier later reads; it changes NO
    // safety gate (the consequential confirmation gate, the allow_consequential
    // master switch, the owner voice-id gate, and the per-agent allowlist are
    // untouched). It runs AFTER the voice-id all-scope gate above, so an
    // unrecognized bystander cannot re-aim the model. On a hit we set the
    // override, emit model.swap telemetry, and SPEAK a short honest ack, then
    // return so it never falls through to a normal answer.
    if let Some(intent) = crate::model_tier::classify_model_swap(text) {
        crate::model_tier::set_override(intent.to_override());
        telemetry::emit(
            "system",
            "model.swap",
            json!({
                "intent": intent.as_str(),
                // The override now in force after the swap: a tier string for a
                // manual pick, or null for Auto (override cleared -> config default).
                "override": crate::model_tier::current_override().map(|t| t.as_str()),
                "manual": intent != crate::model_tier::ModelSwapIntent::Auto,
            }),
        );
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        return Ok(RouteOutcome {
            routed_to: "local",
            response: intent.ack().to_string(),
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // WHISPER / DISCREET MODE VOICE COMMAND (#34): "whisper mode" / "speak quietly" /
    // "be discreet" engage; "back to normal" / "speak normally" / "out loud" disengage.
    // CONSERVATIVELY anchored (prosody::parse_whisper_command matches only a small
    // phrase set, OFF-phrases taking precedence so a "normal" utterance never reads as
    // "on") and handled BEFORE normal routing so a whisper toggle never falls through
    // to the model. This is DELIVERY-ONLY: it sets the process-global whisper state
    // that the speak path reads (mirroring the model-swap override above); it changes
    // NO safety gate — the consequential confirmation gate, the allow_consequential
    // master switch, the owner voice-id gate, lockdown and per-action policy are all
    // untouched, and a required confirmation is NEVER softened/silenced (apply_whisper
    // guards it). The [voice].whisper master switch (ON by default; delivery-only)
    // gates a stray command: apply_command_global honours it, so with the feature off the
    // global stays OFF and this whole arm is a no-op toggle. Runs AFTER the owner
    // voice-id all-scope gate, so an unrecognized bystander cannot flip it.
    if let Some(cmd) = crate::prosody::parse_whisper_command(text) {
        let now_on = crate::prosody::apply_command_global(cfg, cmd);
        telemetry::emit(
            "system",
            "voice.whisper_command",
            json!({
                // The command parsed, and the state now in force after honouring the
                // master switch (with [voice].whisper OFF this is always false).
                "command": match cmd {
                    crate::prosody::WhisperCommand::On => "on",
                    crate::prosody::WhisperCommand::Off => "off",
                },
                "whisper_on": now_on,
                "enabled": cfg.voice.whisper,
            }),
        );
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        // Honest ack: confirm the new delivery state, or — when the feature is off —
        // say so plainly rather than pretending to have engaged it.
        let ack = if !cfg.voice.whisper {
            "Discreet mode isn't enabled, sir.".to_string()
        } else if now_on {
            "Speaking discreetly, sir.".to_string()
        } else {
            "Back to my normal voice, sir.".to_string()
        };
        return Ok(RouteOutcome {
            routed_to: "local",
            response: ack,
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // VAULT MODE VOICE COMMAND ("go dark", vault.rs): "go dark" / "vault mode on" /
    // "vault mode off" / "come back online". CONSERVATIVELY anchored
    // (vault::classify_vault_command matches only the imperative phrase set — an
    // ordinary sentence that merely mentions "vault" never triggers, OFF taking
    // precedence over ON) and handled BEFORE normal routing so a vault toggle never
    // falls through to the model. This flips the process-global vault mode that the
    // two cloud-decision seams above read; it changes NO safety gate (the
    // consequential confirmation gate, the owner voice-id gate, lockdown, and
    // per-action policy are all untouched) and is NOTHING CONSEQUENTIAL — it only
    // TIGHTENS (removes cloud access + forces CUSTOMS to the maximal trim), never
    // adds an outward action. Runs AFTER the owner voice-id all-scope gate, so an
    // unrecognized bystander cannot flip it. On a hit we set the mode, emit the
    // secret-free `vault.status` frame, and SPEAK an HONEST ack, then return.
    if let Some(cmd) = crate::vault::classify_vault_command(text) {
        let now_on = crate::vault::set(matches!(cmd, crate::vault::VaultCommand::On));
        telemetry::emit("system", "vault.status", crate::vault::status_frame(now_on));
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        return Ok(RouteOutcome {
            routed_to: "local",
            response: crate::vault::ack(now_on).to_string(),
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // MACRO RECORD/REPLAY VOICE COMMANDS (#27): "record a macro called X" /
    // "stop recording" / "list macros" / "forget macro X". CONSERVATIVELY anchored
    // (macros::classify_macro_command matches only explicit phrasings — an ordinary
    // sentence that merely mentions "macro" never triggers) and handled BEFORE normal
    // routing so a macro control utterance never falls through to the model. The
    // REPLAY command is handled one level up (in the turn loop), where it re-runs
    // each recorded command through the FULL classify->route->gate pipeline FRESH;
    // here we handle the non-replay control verbs. ON by default ([macros].enabled):
    // with it off these report the subsystem is off and record/persist nothing — no
    // store accrues. Recording captures only the UTTERANCE + intent name, redacted at
    // persist time (macros.rs), so a secret is never stored; and recording NEVER
    // changes a gate — a captured command still runs (and re-gates) normally.
    if let Some(cmd) = crate::macros::classify_macro_command(text) {
        // Replay is driven by the turn loop (it needs to re-classify+route each
        // step); ignore it here so it falls through to the loop's replay check.
        if !matches!(cmd, crate::macros::MacroCommand::Replay { .. }) {
            let prime = agents.orchestrator();
            emit_agent_active(prime);
            let response = handle_macro_command(cmd, cfg, memory).await;
            return Ok(RouteOutcome {
                routed_to: "local",
                response,
                agent: prime.name.clone(),
                namespace: prime.namespace.clone(),
                spoken: None,
            });
        }
    }

    // RUNBOOK VOICE COMMANDS (runbook.rs): "plan the runbook X" (PURE, read-only —
    // render the typed DAG + which steps will PARK) / "run the runbook X" (execute —
    // re-issue EACH step FRESH through the live tool gate, ONE AT A TIME). CONSERVATIVELY
    // anchored (runbook::classify_runbook_command fires ONLY on the explicit "plan/run
    // the runbook <name>" shapes whose name normalizes to a SAFE, CONFINED file stem — an
    // ordinary sentence that merely mentions "runbook", or a path-shaped name, never
    // triggers) and handled BEFORE normal routing so a runbook utterance never falls
    // through to the model. SHIPS OFF ([runbook].enabled=false): with it off both verbs
    // report the subsystem is off and NOTHING plans or runs. SAFETY: `run` carries NO
    // authority — it mirrors the macro-replay dispatch, re-issuing each step through the
    // SAME anthropic::execute_tool + gate a live tool call takes, so a consequential step
    // PARKS FRESH for a spoken confirm (single slot, never batched, never pre-approved);
    // a parked step produces no value, so its `${ref}` consumer BLOCKS rather than run on
    // a fabricated one. Runs AFTER the owner voice-id all-scope gate, like the macro arm.
    if let Some(cmd) = crate::runbook::classify_runbook_command(text) {
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        let response = handle_runbook_command(cmd, cfg, memory, prime, root).await;
        return Ok(RouteOutcome {
            routed_to: "local",
            response,
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // ONE-WORD UNDO (F2): "undo that" / "revert that" / "what can you undo".
    // CONSERVATIVELY anchored (journal::classify_undo_command fires only on
    // explicit undo phrasings — a sentence merely mentioning "undo" never
    // triggers, and a QUESTION about undo answers instead of arming). Handled
    // AFTER the confirmation pre-check above, so while an action is PARKED an
    // "undo" reply is consumed there as a Deny (retract the un-executed action)
    // and never reaches here; this arm therefore only ever undoes EXECUTED
    // actions from the journal. SAFETY: arming an undo hands the derived
    // inverse to anthropic::execute_tool — the SAME entry point a live tool
    // call uses — so the inverse gets the identical voice-id check, faithful
    // dry-run preview, policy layer, master-switch ceiling, and single-slot
    // spoken-confirm park. Undo executes NOTHING itself and grants nothing a
    // spoken command would not. Runs after the owner voice-id all-scope gate,
    // like the macro arm above.
    if let Some(cmd) = crate::journal::classify_undo_command(text) {
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        let response = handle_undo_command(cmd, memory).await;
        return Ok(RouteOutcome {
            routed_to: "local",
            response,
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // APERTURE VOICE COMMANDS (aperture.rs): the on-device activity timeline —
    // "what did I do this morning" / "what was I working on around 3pm" (RECALL) and
    // "forget my activity timeline" (FORGET). Handled BEFORE the screen-context block
    // below and CONSERVATIVELY anchored: classify_aperture_intent only fires on a
    // recall cue that ALSO carries a resolvable TIME WINDOW ("this morning", "around
    // 3pm", "the last hour", "today") OR an explicit "activity"/"timeline" word. That
    // is exactly how it COEXISTS with the recent screen-context recall: a bare "what
    // was I working on" (no window, no timeline word) falls through here and is
    // handled by screen_context below. READ-ONLY: RECALL summarizes the BOUNDED,
    // PII-REDACTED timeline (app + window title + duration — NEVER screen pixels); an
    // off / un-fed timeline is an HONEST "no activity recorded", never fabricated.
    // FORGET wipes the in-RAM ring. SHIPS OFF ([aperture].enabled=false) — with it
    // off nothing was ever recorded. Runs after the owner voice-id all-scope gate, so
    // an unrecognized bystander cannot recall or wipe the owner's activity timeline.
    if let Some(intent) = crate::aperture::classify_aperture_intent(text, &chrono::Local::now()) {
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        let (verb, response) = if !cfg.aperture.enabled {
            // OFF (the shipped default): nothing was ever recorded. Honest, never a
            // fabricated timeline and never a claim the feature is running.
            (
                "off",
                "The activity timeline is off, sir — I'm not recording what you work on. \
                 Enable [aperture] and I'll keep a private, on-device record of which app \
                 you're in and its window title (never your screen) so I can tell you what \
                 you were working on."
                    .to_string(),
            )
        } else {
            match intent {
                crate::aperture::ApertureIntent::Recall(query) => {
                    // Summarize the redacted timeline for the constructed query
                    // (window + optional subject). Honest-empty on an un-fed ring.
                    ("recall", crate::aperture::global_render_recall(&query, 6))
                }
                crate::aperture::ApertureIntent::Forget => {
                    let cleared = crate::aperture::global_clear();
                    let ack = if cleared {
                        "Done, sir — I've wiped your activity timeline.".to_string()
                    } else {
                        "There was no activity timeline to forget, sir.".to_string()
                    };
                    ("forget", ack)
                }
            }
        };
        // SECRET-FREE telemetry: the verb + the gate only — never the recalled
        // (already-redacted) app/title text.
        telemetry::emit(
            "system",
            "aperture.command",
            json!({ "verb": verb, "enabled": cfg.aperture.enabled }),
        );
        return Ok(RouteOutcome {
            routed_to: "local",
            response,
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // CONTINUOUS SCREEN CONTEXT VOICE COMMANDS (#42): "what was I working on" /
    // "recall my screen context" (RECALL) and "forget my screen context" (FORGET).
    // CONSERVATIVELY anchored (screen_context::classify_screen_context_intent
    // requires the explicit "screen context" phrase or the narrow "what was I
    // working on" recall cue, so an ordinary sentence — and crucially the one-shot
    // OCR `read.screen` phrasings "read my screen" / "what's on my screen" — never
    // reaches here) and handled BEFORE normal routing so a screen-context utterance
    // never falls through to the model. READ-ONLY: RECALL renders the BOUNDED,
    // REDACTED recent context from the in-RAM ring (an empty/un-fed ring is an
    // HONEST "no recent screen context", never fabricated); FORGET wipes the ring.
    // The recalled text is kept TRANSIENT by the main.rs gate (is_screen_read unions
    // the recall here) so it never seeds lifelong memory / optimizer traces, exactly
    // like the one-shot screen read. The CONTINUOUS capture loop that FEEDS the ring
    // ships ON ([screen_context].enabled) but is INERT WITHOUT Screen-Recording TCC
    // consent — these voice commands only READ/CLEAR the ring, they never start a capture. Runs after the
    // owner voice-id all-scope gate, so an unrecognized bystander cannot recall the
    // owner's screen context or wipe it.
    if let Some(intent) = crate::screen_context::classify_screen_context_intent(text) {
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        let (verb, response) = match intent {
            crate::screen_context::ScreenContextIntent::Recall { subject } => {
                // Bounded redacted recall — read-only; honest-empty on an un-fed
                // ring. A named subject narrows to matching entries (never invents
                // a match); a bare recall renders the recent context.
                let rendered = match subject {
                    Some(s) => crate::screen_context::global_render_recall_matching(&s, 10),
                    None => crate::screen_context::global_render_recall(10),
                };
                ("recall", rendered)
            }
            crate::screen_context::ScreenContextIntent::Forget => {
                let cleared = crate::screen_context::global_clear();
                let ack = if cleared {
                    "Done, sir — I've wiped your recent screen context.".to_string()
                } else {
                    "There was no screen context to forget, sir.".to_string()
                };
                ("forget", ack)
            }
        };
        // SECRET-FREE telemetry: the verb only — never the recalled redacted text
        // (which is transient + already redacted, but is not echoed to telemetry).
        telemetry::emit(
            "system",
            "screen_context.command",
            json!({ "verb": verb, "enabled": cfg.screen_context.enabled }),
        );
        return Ok(RouteOutcome {
            routed_to: "local",
            response,
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // SEMANTIC PASTEBOARD VOICE COMMANDS (pasteboard.rs): "what did I copy about
    // the lease" / "recall my clipboard" (RECALL) and "forget my clipboard"
    // (FORGET). CONSERVATIVELY anchored (classify_pasteboard_intent requires an
    // explicit clipboard/"copied" reference plus a recall/forget cue, so an
    // ordinary sentence — and crucially an imperative "copy X to my clipboard"
    // (which is the confirm-gated pasteboard_put tool, NOT a recall) — never reaches
    // here). Handled BEFORE normal routing so a pasteboard utterance never falls
    // through to the model. READ-ONLY: RECALL ranks the BOUNDED, PII-REDACTED clip
    // ring by MEANING via the recall.rs path (an off / empty ring is an HONEST
    // "nothing copied yet", never fabricated); FORGET wipes the ring. SHIPS OFF
    // ([pasteboard].enabled=false) — with it off nothing was ever captured, so
    // recall/forget honestly report an empty history. Runs after the owner voice-id
    // all-scope gate, so an unrecognized bystander cannot recall or wipe the owner's
    // clipboard history.
    if let Some(intent) = crate::pasteboard::classify_pasteboard_intent(text) {
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        let (verb, response) = if !cfg.pasteboard.enabled {
            // OFF (the shipped default): nothing was ever captured. Honest, not a
            // fabricated recall — and never a claim the feature is running.
            (
                "off",
                "The semantic pasteboard is off, sir — I'm not capturing your clipboard. \
                 Enable [pasteboard] and I'll start remembering what you copy, on-device."
                    .to_string(),
            )
        } else {
            match intent {
                crate::pasteboard::PasteboardIntent::Recall { subject } => {
                    // Rank the redacted clip ring by meaning; a named subject
                    // narrows the query, a bare recall ranks against the whole
                    // utterance. Honest-empty on an off / un-fed ring.
                    let query = subject.as_deref().unwrap_or(text);
                    ("recall", crate::pasteboard::global_render_recall(query, 10))
                }
                crate::pasteboard::PasteboardIntent::Forget => {
                    let cleared = crate::pasteboard::global_clear();
                    let ack = if cleared {
                        "Done, sir — I've wiped your clipboard history.".to_string()
                    } else {
                        "There was no clipboard history to forget, sir.".to_string()
                    };
                    ("forget", ack)
                }
            }
        };
        // SECRET-FREE telemetry: the verb + the gate only — never the recalled
        // (already-redacted) clip text.
        telemetry::emit(
            "system",
            "pasteboard.command",
            json!({ "verb": verb, "enabled": cfg.pasteboard.enabled }),
        );
        return Ok(RouteOutcome {
            routed_to: "local",
            response,
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // RESEARCH NOTEBOOK VOICE COMMAND (#19): "save this research" / "show my
    // research notebook on X" / "what have I researched" / "forget my research on
    // X". CONSERVATIVELY anchored (classify_notebook_intent requires an explicit
    // notebook/"my research" cue, so an ordinary "research the competitors" still
    // routes to SAGE's live run, never here). Handled BEFORE normal routing so a
    // notebook utterance never falls through to the model. READ/PROPOSE-ONLY: it
    // persists a run that ALREADY happened (the real last SAGE run, with its real
    // grounded citations — never a fabricated source) and reads runs that were
    // really saved; it speaks, but acts/reaches nothing outward. AGENT-SCOPED: the
    // notebook store is scoped to the active agent's namespace (own + shared
    // orchestrator). On a bare save with no recent run it honestly says so and
    // saves NOTHING. Voiced by the orchestrator (the conversational tier that owns
    // the user's saved research). Runs after the owner voice-id all-scope gate, so
    // an unrecognized bystander cannot touch the notebooks.
    if let Some(intent) = crate::notebook::classify_notebook_intent(text) {
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        let outcome = crate::notebook::dispatch(memory, &prime.namespace, intent)
            .await
            .unwrap_or_else(|e| crate::notebook::NotebookOutcome {
                reply: format!("I couldn't reach your research notebooks just now, sir — {e}."),
                verb: "error",
                card: None,
            });
        // Enriched, SECRET-FREE telemetry: the verb plus the rendered CARD the HUD
        // renders — the topic, a bounded snippet of the already-redacted synthesis,
        // and the run's REAL fetched-source citations (id + title + url), exactly the
        // persisted/grounded ones (never a fabricated source, never raw content).
        // When there's no content to surface (save_none/forget_none/error) the card
        // is absent and only the verb rides.
        let card_json = outcome.card.as_ref().map(|c| {
            json!({
                "verb": c.verb,
                "topic": c.topic,
                "snippet": c.snippet,
                "run_count": c.run_count,
                "citations": c
                    .citations
                    .iter()
                    .map(|cit| json!({
                        "source_id": cit.source_id,
                        "title": cit.title,
                        "url": cit.url,
                    }))
                    .collect::<Vec<_>>(),
            })
        });
        telemetry::emit(
            "system",
            "notebook.card",
            json!({"verb": outcome.verb, "card": card_json}),
        );
        return Ok(RouteOutcome {
            routed_to: "local",
            response: outcome.reply,
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // PRECOG // WHAT-IF (simulate.rs): "what would you do if I said X". CONSERVATIVELY
    // anchored (simulate::extract_hypothetical fires ONLY on the high-precision
    // "what would you do if I said/asked/told you to X" framing and requires a
    // non-empty tail — a bare "simulate ..." is CASSANDRA's forecast vocabulary and
    // is NOT claimed here). GATED by [precog].enabled (ships ON; read-only) — when
    // off this falls through to ordinary routing (the query is just another
    // question). READ-ONLY by CONSTRUCTION: the classify below is a read-only label
    // of the HYPOTHETICAL (it fires nothing), and simulate() runs the SAME pipeline
    // the live turn would — classify -> selector -> agent -> tier -> gate projection
    // -> reversibility — UP TO but NEVER THROUGH the confirmation gate. The simulate
    // path holds NO actuator / memory-write / inference handle (SimContext carries
    // only read views), so it cannot fire an action even a benign one. It emits the
    // PlannedOutcome as a `precog.plan` frame + speaks a summary that honestly
    // reports a real run WOULD park (PRECOG never satisfies a gate itself). Runs
    // after the owner voice-id all-scope gate, like the other command cues.
    if cfg.precog.enabled {
        if let Some(hypothetical) = crate::simulate::extract_hypothetical(text) {
            let prime = agents.orchestrator();
            emit_agent_active(prime);
            // Classify the HYPOTHETICAL (read-only — it labels text, it fires
            // nothing); on any classifier error fall back to the safe "unknown"
            // view (a low-confidence plain conversation), exactly the live degrade.
            let predicted = match infer.classify(&hypothetical).await {
                Ok(c) => crate::simulate::PredictedIntent {
                    intent: c.intent,
                    confidence: c.confidence,
                    complexity: c.complexity,
                },
                Err(e) => {
                    warn!("precog: classify of the hypothetical failed ({e}); using safe default");
                    crate::simulate::PredictedIntent::unknown()
                }
            };
            // The read-only context: shared roster + read-only config + the SAME
            // pure lexical scorer the live routing uses + the current tier override
            // + this turn's cloud reachability. No actuator / memory / brain handle.
            let ctx = crate::simulate::SimContext {
                agents,
                cfg,
                scorer: &crate::agents::LexicalAgentScorer,
                override_tier: crate::model_tier::current_override(),
                cloud_reachable,
            };
            let plan = crate::simulate::simulate(&hypothetical, &predicted, &ctx);
            // SECRET-FREE telemetry: only the pipeline decisions + the (already
            // user-spoken) hypothetical ride the wire — nothing ran, so there is no
            // fact/memory/tool-output to leak. The frame PINS executed=false /
            // satisfied_a_gate=false so the HUD copy is grounded in the contract.
            telemetry::emit("local", "precog.plan", plan.telemetry(&hypothetical));
            let response = plan.spoken_summary(&hypothetical);
            return Ok(RouteOutcome {
                routed_to: "local",
                response,
                agent: prime.name.clone(),
                namespace: prime.namespace.clone(),
                spoken: None,
            });
        }
    }

    // REPORT GENERATION VOICE COMMAND (#40): "generate a report on X" / "write me a
    // report about X". CONSERVATIVELY anchored (classify_report_intent requires
    // "report" + an explicit build verb + a topic, so a question about an existing
    // report and an ordinary "research X" never trip it). GATED by [report].enabled
    // (ships ON; read-only, safe to enable) — when off the op declines honestly and
    // reads nothing. READ-ONLY: it pulls the agent-scoped, already-cited
    // notebook runs on the topic and folds them into a BOUNDED markdown report under
    // research.rs's cite discipline (every citation a REAL source ref an input claim
    // carried; an uncited run contributes nothing; no citable source -> an
    // honest-empty report) — it never fetches, never calls a model, never persists.
    // Voiced by the orchestrator (the tier that owns the user's saved research).
    // Runs after the owner voice-id all-scope gate, so an unrecognized bystander
    // cannot read the notebooks. Only entered when the flag is on, so it adds no
    // surface by default.
    if cfg.report.enabled {
        if let Some(intent) = crate::report::classify_report_intent(text) {
            let prime = agents.orchestrator();
            emit_agent_active(prime);
            let report_cfg = crate::report::ReportConfig { enabled: cfg.report.enabled };
            let outcome = crate::report::dispatch(memory, &prime.namespace, intent, &report_cfg)
                .await
                .unwrap_or_else(|e| crate::report::ReportOutcome {
                    markdown: format!("I couldn't assemble that report just now, sir — {e}."),
                    verb: "error",
                    report: None,
                });
            // Structured telemetry: the verb plus the report's title, section
            // headings, the count of REAL citations, and the honest-empty flag — all
            // derived from the already-cited material (never a fabricated source).
            let report_json = outcome.report.as_ref().map(|r| {
                json!({
                    "title": r.title,
                    "empty": r.empty,
                    "section_count": r.sections.len(),
                    "headings": r.sections.iter().map(|s| s.heading.clone()).collect::<Vec<_>>(),
                    "citation_count": r.all_citations.len(),
                    "citations": r
                        .all_citations
                        .iter()
                        .map(|c| json!({"id": c.id, "title": c.title, "url": c.url}))
                        .collect::<Vec<_>>(),
                })
            });
            telemetry::emit(
                "system",
                "report.built",
                json!({"verb": outcome.verb, "report": report_json}),
            );
            // ARTIFACT REGISTRY: register a REAL (non-empty) built report so the
            // peek surface can surface it. Provenance is HONEST — the real producing
            // agent (prime) + the report's REAL citations (each a source ref an input
            // claim carried, never fabricated); an empty report is not registered
            // (nothing was produced). The registry is in-memory + on-device; this
            // opens no surface.
            if let Some(r) = outcome.report.as_ref() {
                if !r.empty {
                    let citations = r
                        .all_citations
                        .iter()
                        .filter_map(|c| crate::artifact::Citation::new(c.title.clone(), c.url.clone()))
                        .collect::<Vec<_>>();
                    crate::artifact::register(
                        crate::artifact::ArtifactKind::Report,
                        r.title.clone(),
                        prime.name.clone(),
                        citations,
                        format!(
                            "{} section{}, {} citation{}",
                            r.sections.len(),
                            if r.sections.len() == 1 { "" } else { "s" },
                            r.all_citations.len(),
                            if r.all_citations.len() == 1 { "" } else { "s" },
                        ),
                    );
                }
            }
            return Ok(RouteOutcome {
                routed_to: "local",
                response: outcome.markdown,
                agent: prime.name.clone(),
                namespace: prime.namespace.clone(),
                spoken: None,
            });
        }
    }

    // CHART VOICE COMMAND (#41): "chart this" / "plot the system load" / "graph the
    // cpu". CONSERVATIVELY anchored (classify_chart_intent requires a chart/plot/
    // graph verb + a chartable subject, so an ordinary "what's the cpu" never trips
    // it). GATED by [chart].enabled (ships ON — a neutral presentation act, safe to
    // enable outright) — when off the op declines and emits nothing. NEUTRAL presentation: it
    // serializes a ChartSpec from the latest REAL system snapshot the telemetry bus
    // already publishes (the EXACT cpu/mem values — no interpolation, no invented
    // point; no snapshot -> an honest-empty chart) and fire-and-forget emits it as a
    // `chart.data` envelope the HUD plots exactly. It changes no gate, takes no
    // action, reaches no network. Only entered when the flag is on (it ships on),
    // and emitting is a pure presentation act with no safety surface.
    if cfg.chart.enabled {
        if let Some(_intent) = crate::chart::classify_chart_intent(text) {
            let prime = agents.orchestrator();
            emit_agent_active(prime);
            // The data path: the latest REAL system snapshot -> a ChartSpec of the
            // exact metrics (honest-empty when no reading is available yet).
            let spec = crate::chart::chart_from_snapshot(telemetry::latest_snapshot());
            crate::chart::emit_chart(&spec);
            // ARTIFACT REGISTRY: register a REAL (non-empty) chart. A chart of live
            // system metrics genuinely cites nothing, so it is registered UNCITED —
            // honest, never dressed up with a fabricated source. In-memory +
            // on-device; opens no surface.
            if !spec.is_empty() {
                let points: usize = spec.series.iter().map(|s| s.points.len()).sum();
                crate::artifact::register(
                    crate::artifact::ArtifactKind::Chart,
                    spec.title.clone(),
                    prime.name.clone(),
                    Vec::new(), // live system metrics carry no citation -> UNCITED
                    format!(
                        "{} series, {} point{}",
                        spec.series.len(),
                        points,
                        if points == 1 { "" } else { "s" },
                    ),
                );
            }
            let response = if spec.is_empty() {
                "I don't have a system reading to chart yet, sir — give me a moment and ask again."
                    .to_string()
            } else {
                "Charting the current system load for you, sir — it's on the HUD.".to_string()
            };
            return Ok(RouteOutcome {
                routed_to: "local",
                response,
                agent: prime.name.clone(),
                namespace: prime.namespace.clone(),
                spoken: None,
            });
        }
    }

    // ARTIFACT PEEK VOICE COMMAND (artifact.rs): "what did you just do" / "peek".
    // CONSERVATIVELY anchored (classify_peek_intent requires an explicit peek cue or
    // a "what did you just <produce>" recall phrase, so an ordinary "what did you
    // say" never trips it). GATED by [artifact].enabled (ships ON, armed-by-default)
    // — when off this arm is skipped and the utterance routes normally. READ-ONLY:
    // it reads the MOST RECENT artifact the producers registered back out of the
    // in-memory, on-device registry and fire-and-forget emits it as an
    // `artifact.peek` frame the HUD's QuickLook overlay renders — with HONEST
    // provenance (the real producing agent + real citations, or UNCITED). It changes
    // no gate, takes no action, reaches no network. An empty registry is answered
    // honestly ("nothing to peek yet"), never a fabricated artifact.
    if cfg.artifact.enabled && crate::artifact::classify_peek_intent(text) {
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        let response = match crate::artifact::peek_and_emit(None) {
            Some(artifact) => artifact.summary(),
            None => crate::artifact::empty_reply(),
        };
        return Ok(RouteOutcome {
            routed_to: "local",
            response,
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // COMPOSE-MUSIC VOICE COMMAND (Phase-2 flagship "DARWIN, compose an 8-bit happy
    // birthday"). CONSERVATIVELY anchored (classify_music_intent requires an explicit
    // music-CREATION verb + a musical anchor, so "play some jazz" and "what's the
    // time" never trip it). MIRRORS the chart arm's shape: gated, handled BEFORE
    // normal model routing so a creation utterance never falls through to the model.
    // GATED by [voice].cloud_music (the music-generation tier switch) — when OFF this
    // arm is skipped entirely and the utterance routes normally (we never claim to
    // compose with the tier off). When ON but there's NO ElevenLabs key (or we're
    // offline), the spawned trigger_compose_music honestly NO-OPS — nothing is
    // fabricated and no track plays; the "composing now" ack is then mildly optimistic
    // but never a lie about a produced track. The composition runs FIRE-AND-FORGET on
    // a Send-safe per-call client (compose_music_for_command's dedicated thread +
    // current-thread runtime), and Part-1 plays the finished WAV on the SEPARATE music
    // sink — so this route returns the Jerome-voiced ack IMMEDIATELY without blocking
    // on the 30 s–10 min generation. The el_key is read ONLY inside the trigger.
    if cfg.voice.cloud_music {
        if let Some(prompt) = classify_music_intent(text) {
            // JEROME — "Leisure + DJ": the agent that owns music/entertainment.
            // Fall back to the orchestrator if the roster lacks it, but WARN so a
            // missing specialist is visible (a silent fallback would route music
            // to the wrong namespace/voice without any signal to the operator).
            let jerome = match agents.get("jerome") {
                Some(a) => a,
                None => {
                    warn!("router: agents.toml has no 'jerome' (music specialist); routing music via the orchestrator");
                    agents.orchestrator()
                }
            };
            emit_agent_active(jerome);
            telemetry::emit("system", "music.intent", json!({}));
            // Fire-and-forget the (genuinely non-Send) generation on its own thread,
            // reusing the command channel's Send-safe wrapper. Part 1 plays the track
            // when it finishes; failures stay inside the trigger (honest no-op).
            let cfg_owned = cfg.clone();
            let root_owned = root.to_path_buf();
            let sock = infer.socket_path().to_path_buf();
            tokio::spawn(async move {
                let _ = crate::compose_music_for_command(
                    cfg_owned,
                    prompt,
                    None,
                    root_owned,
                    sock,
                )
                .await;
            });
            return Ok(RouteOutcome {
                routed_to: "local",
                response: "Composing your track now, sir — I'll have it ready in a moment."
                    .to_string(),
                agent: jerome.name.clone(),
                namespace: jerome.namespace.clone(),
                spoken: None,
            });
        }
    }

    // LIFE-LOG DIGEST VOICE COMMAND (#20): "what did I do this week" / "show my
    // life log" / "what did I do today". CONSERVATIVELY anchored
    // (classify_lifelog_intent requires an explicit own-activity cue, so an
    // ordinary "what's the weather today" never trips it). Handled BEFORE normal
    // routing so a life-log utterance never falls through to the model. READ-ONLY:
    // it SUMMARIZES real recorded episodes — an empty/sparse window renders an
    // honest empty/sparse digest, never a fabricated event. AGENT-SCOPED: the
    // digest is built over the active agent's recall scope (own + shared
    // orchestrator), so it can never show another agent's episodes. Voiced by the
    // orchestrator (the user's main interaction tier). Runs after the owner
    // voice-id all-scope gate, so an unrecognized bystander cannot read the log.
    if let Some(intent) = crate::lifelog::classify_lifelog_intent(text) {
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        let crate::lifelog::LifeLogIntent::Digest(period) = intent;
        // The spoken reply comes from the unchanged dispatch; the enriched card is
        // built from the SAME agent-scoped, bounded digest read so the HUD can render
        // content. No logic change to lifelog.rs — this reuses its public surface.
        let reply = crate::lifelog::dispatch(memory, &prime.namespace, intent).await;
        let digest = crate::lifelog::build_digest(memory, &prime.namespace, period).await;
        let card = crate::lifelog::build_card(&digest);
        // Enriched, SECRET-FREE telemetry: the period plus the digest's
        // already-redacted content — the rendered digest text, the REAL episode
        // count, and the bounded themes / topics / recent summaries. Every field is
        // the episodic store's already-redacted output (a secret was stripped before
        // write), never raw, never fabricated; an empty window rides empty:true.
        telemetry::emit(
            "system",
            "lifelog.digest",
            json!({
                "period": card.period,
                "empty": card.empty,
                "episode_count": card.episode_count,
                "digest_text": card.digest_text,
                "themes": card.themes,
                "topics": card.topics,
                "recent_summaries": card.recent_summaries,
            }),
        );
        return Ok(RouteOutcome {
            routed_to: "local",
            response: reply,
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // SESSION REWIND (F12): "what happened at 2pm" / "rewind the last hour" /
    // "walk me through this morning". REVIEW-ONLY time travel: reconstructs a
    // bounded timeline of the window from the RECORDED stores — episodes (the
    // redacted, gated turn record; deliberately NOT raw transcripts, which keep
    // what the episodic privacy gate excludes) and the audit log's redacted
    // consequential-action entries — narrates a digest, and emits the timeline
    // for the HUD step-through. It NEVER re-executes anything (that is macro
    // replay's job, and it re-gates). Runs AFTER the lifelog arm so lifelog
    // keeps its own-activity phrasing ("what did I do", "my day"); the rewind
    // classifier requires an explicit gate + time qualifier and never matches
    // macro-replay verbs. Reads stay fail-open (an empty window is never an
    // error) — but a FAILED read is DISCLOSED, never narrated as a clean
    // "nothing happened" (that would fabricate absence).
    if let Some(window) =
        crate::rewind::classify_rewind_intent(text, chrono::Local::now().fixed_offset())
    {
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        // Both reads are WINDOW-SCOPED (a depth-only read would silently miss
        // an old window's rows and narrate a false absence) and share one cap;
        // a saturated read flips counts_floor so the counts are disclosed as
        // "at least N", never presented as exact.
        const REWIND_READ_CAP: usize = 200;
        let mut reads_failed = false;
        // Episodes over the shared/orchestrator scope (the lifelog precedent).
        let episodes = match memory
            .episodes_around(&prime.namespace, &window.from_utc, &window.to_utc, REWIND_READ_CAP)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "rewind: episode read failed; disclosing a partial record");
                reads_failed = true;
                Vec::new()
            }
        };
        // Audit entries via the windowed read — both sides UTC RFC3339, so the
        // lexical compare is exact. A MISSING log (audit off) is honestly "no
        // actions"; a FAILED read on a present log is disclosed.
        let actions: Vec<crate::audit::AuditEntry> = match crate::audit::global() {
            Some((_enabled, log)) => {
                match log.between(&window.from_utc, &window.to_utc, REWIND_READ_CAP).await {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(error = %e, "rewind: audit read failed; disclosing a partial record");
                        reads_failed = true;
                        Vec::new()
                    }
                }
            }
            None => Vec::new(),
        };
        let counts_floor =
            episodes.len() >= REWIND_READ_CAP || actions.len() >= REWIND_READ_CAP;
        let rewind = crate::rewind::build_timeline(&window, &episodes, &actions, counts_floor);
        let mut payload = crate::rewind::payload(&rewind);
        if reads_failed {
            payload["reads_failed"] = serde_json::json!(true);
        }
        telemetry::emit("system", "session.rewind", payload);
        let mut response = crate::rewind::render_spoken(&rewind);
        if reads_failed {
            response.push_str(
                " One caveat, sir — part of the record was unreadable just now, so this view may be incomplete.",
            );
        }
        return Ok(RouteOutcome {
            routed_to: "local",
            response,
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // CAUSA (causal decision-trace explainer, explain.rs): "why did you do that" /
    // "why <Agent>" narrates the ordered, REDACTED decision trace of the relevant
    // recent turn (recorded at the END of run_pipeline). GATED by [explain].enabled
    // (ships ON); when off, the question simply falls through to the model. Placed
    // right after rewind so the review-family verbs stay together and after the
    // higher-priority control arms (panic/unlock/confirm/undo/…). REVIEW-ONLY: it
    // re-executes nothing — it explains what ALREADY happened, from records the
    // daemon already holds, and returns an HONEST EMPTY (never a fabricated
    // rationale) when there is no trace for the ask.
    if cfg.explain.enabled {
        if let Some(query) = crate::explain::classify_explain_intent(text) {
            let prime = agents.orchestrator();
            emit_agent_active(prime);
            let trace = crate::explain::lookup(&query);
            telemetry::emit("system", "causa.trace", crate::explain::payload(&query, trace.as_ref()));
            let response = crate::explain::render_spoken(&query, trace.as_ref());
            return Ok(RouteOutcome {
                routed_to: "local",
                response,
                agent: prime.name.clone(),
                namespace: prime.namespace.clone(),
                spoken: None,
            });
        }
    }

    // MIRROR (belief-audit + contest over the SELF-MODEL, user_model.rs): "why do you
    // think I prefer X" surfaces the STORED observation, provenance, and observed-count
    // for that belief (never a fabricated reason); "that's wrong about X" DROPS the
    // belief AND writes a suppression tombstone so the consolidation pass never
    // re-derives it. GATED by [mirror].enabled (ships ON, read-only/reduce-only surface).
    // Placed right after CAUSA so the two "explain" families stay together — MIRROR's
    // cues are SELF-MODEL-specific ("why do you THINK I…"), distinct from CAUSA's
    // turn-decision asks, so it never steals a "why did you do that". REDUCE-ONLY:
    // explain reads the shared tier; contest only ever removes/suppresses a shared
    // `user.model.*` belief and is structurally unable to touch a private agent.* note.
    // Emits the secret-free `mirror.belief` telemetry frame.
    if cfg.mirror.enabled {
        if let Some(intent) = crate::user_model::classify_mirror_intent(text) {
            let prime = agents.orchestrator();
            emit_agent_active(prime);
            let response = match intent {
                crate::user_model::MirrorIntent::Explain(subject) => {
                    let explanation = crate::user_model::explain_belief(memory, &subject)
                        .await
                        .unwrap_or(crate::user_model::Explanation {
                            asked: subject.clone(),
                            entries: Vec::new(),
                        });
                    crate::user_model::emit_belief_frame(
                        memory,
                        "explain",
                        &subject,
                        explanation.found(),
                    )
                    .await;
                    explanation.text()
                }
                crate::user_model::MirrorIntent::Contest(subject) => {
                    let contest = crate::user_model::contest_belief(memory, &subject)
                        .await
                        .unwrap_or_default();
                    crate::user_model::emit_belief_frame(
                        memory,
                        "contest",
                        &subject,
                        contest.any(),
                    )
                    .await;
                    contest.text(&subject)
                }
                crate::user_model::MirrorIntent::Clear(subject) => {
                    // The tombstone is user-clearable: lifting a prior contest lets
                    // the consolidation pass learn the belief afresh.
                    let cleared = crate::user_model::clear_suppression(memory, &subject)
                        .await
                        .unwrap_or(0);
                    crate::user_model::emit_belief_frame(
                        memory,
                        "clear",
                        &subject,
                        cleared > 0,
                    )
                    .await;
                    if cleared > 0 {
                        "Done, sir — I have lifted that suppression; I may learn it again \
                         if I keep observing it.".to_string()
                    } else {
                        format!(
                            "There was no suppression on \"{}\" to lift, sir.",
                            subject.trim()
                        )
                    }
                }
            };
            return Ok(RouteOutcome {
                routed_to: "local",
                response,
                agent: prime.name.clone(),
                namespace: prime.namespace.clone(),
                spoken: None,
            });
        }
    }

    // Roll-call (item 3, the reel centerpiece): "introduce the team" / "roll
    // call" / "assemble" -> each agent speaks its one-line self-introduction in
    // ITS OWN voice, in order, emitting agent.active per agent so the HUD
    // highlights them in turn and the core color cycles. Checked before any
    // routing so it never lands on the classifier/cloud. Interruptible.
    if crate::agents::is_roll_call(text) {
        let (response, report) = roll_call(agents, infer, reply, started, root, cfg).await;
        return Ok(RouteOutcome {
            routed_to: "local",
            response,
            agent: agents.orchestrator().name.clone(),
            namespace: agents.orchestrator().namespace.clone(),
            spoken: Some(SpokenReply {
                route_ms: route_entry.elapsed().as_millis() as u64,
                report,
            }),
        });
    }

    // Agent-ROSTER query ("list my agents" / "who are my agents" / "what's the
    // constellation"): answered DETERMINISTICALLY from the live registry, BEFORE
    // acting on the classification. The classifier has been observed to misroute
    // these to the local model, where — with no roster in its context — it
    // HALLUCINATES agents that do not exist and leaks unrelated facts. Here the
    // answer always comes from the real registry: cloud-reachable, DARWIN phrases
    // the true roster in persona (grounded — persona.txt forbids inventing agents
    // not in it); offline / on a cloud error, a plain spoken list (still the real
    // team). The constellation is named accurately or not at all, never invented.
    if crate::agents::is_agent_query(text) {
        let (response, routed_to) =
            answer_agent_roster(text, agents, memory, cfg, cloud_reachable).await;
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        return Ok(RouteOutcome {
            routed_to,
            response,
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // CAPABILITY SELECTOR (the "extremely smart" glue): from the natural request,
    // decide WHICH CAPABILITY to engage BEFORE agent selection — so the user never
    // manages modes. This is a HIGHER-LEVEL dispatch than agent routing (which
    // picks WHICH AGENT); it picks WHICH MODE: a plain one-shot answer, a read of
    // the shared World Model, a fold of a stated fact INTO the World Model, a
    // complex multi-step mission NOW (FURY), or the SETUP of a recurring standing
    // mission. Deterministic cues run first; a pure semantic fallback (the same
    // LexicalAgentScorer the smart agent routing uses) only PROMOTES out of the
    // safe one-shot default on a strong, unambiguous signal.
    //
    // THE TWO RAILS are enforced inside classify_mode + here:
    //   * Rail 1 (clarify / safe-default, never guess into autonomy): a mere
    //     semantic lean toward a standing mission NEVER silently establishes it —
    //     it returns a one-line CLARIFY ("every day, or just once?") which we speak
    //     and stop. A low-confidence / ambiguous turn falls back to one_shot. Only
    //     a HARD recurring cue routes straight to the (still gated) standing setup.
    //   * Rail 2 (no silent autonomy): the standing mode only PROPOSES — it routes
    //     to standing_create, which PARKS behind the cross-turn confirmation gate
    //     (and the armed-by-default master switch, which still requires a fresh
    //     per-action confirm). world_update writes ONLY the
    //     shared user.world.* tier, never a consequential external action.
    //
    // one_shot falls THROUGH to the existing pipeline unchanged (so current fast
    // cue routing + all routing tests are untouched). The user can always be
    // explicit and override the selector with a plain phrasing.
    match crate::selector::classify_mode(text, &crate::agents::LexicalAgentScorer) {
        crate::selector::Selection::Route(crate::selector::Mode::OneShot) => {
            // Default: the normal pipeline below handles it (unchanged).
        }
        crate::selector::Selection::Clarify(question) => {
            // RAIL 1: genuinely ambiguous between a safe one-shot and arming
            // autonomy — ask, never guess. Voiced by the orchestrator; nothing is
            // established, queried, or fired. The next turn's explicit answer
            // routes deterministically (hard cue or plain one-shot).
            telemetry::emit("local", "selector.clarify", json!({"question": question}));
            let prime = agents.orchestrator();
            emit_agent_active(prime);
            return Ok(RouteOutcome {
                routed_to: "local",
                response: question,
                agent: prime.name.clone(),
                namespace: prime.namespace.clone(),
                spoken: None,
            });
        }
        crate::selector::Selection::Route(mode) => {
            telemetry::emit("local", "selector.mode", json!({"mode": mode.as_str()}));
            // THRESHOLD — GUEST MODE: the capability modes (World read/fold, a NOW
            // multi-step mission, a standing-mission setup) either READ the owner's
            // shared World Model or take a CONSEQUENTIAL/autonomous action. A guest
            // reaches none of them — skip the capability dispatch and fall through to
            // the (guest-gated) conversational path, which safely answers without any
            // owner data or tool. Owner path: byte-for-byte today's.
            if !crate::threshold::is_guest_turn() {
                if let Some(outcome) =
                    route_capability(mode, text, memory, agents, cloud_reachable).await
                {
                    return Ok(outcome);
                }
            }
            // A capability that declined (e.g. nothing to read) degrades to the
            // normal pipeline below rather than going silent.
        }
    }

    let needs_deep_reasoning = class.complexity == "heavy";
    // VAULT MODE ("go dark") + THRESHOLD GUEST (guest = local-only) — SEAM 2 of 2. The
    // actuating tool-loop gate does NOT consult `cloud_reachable` (it would otherwise
    // try the cloud and degrade on the resolve_api_key error), so close it here at the
    // decision itself: an active vault OR a guest turn forces `to_cloud` false, so a
    // heavy / low-confidence turn never reaches the cloud tool loop and instead stays
    // on the local path (or honestly degrades offline). RESTRICT-ONLY + COMPOSABLE via
    // the same `deny_cloud` gates as SEAM 1 — each can only turn a cloud decision OFF;
    // with BOTH off this is exactly `wants_cloud(class, cfg)`. GUEST: this is the
    // second half of forcing a bystander local — no cloud tool loop, so no obol spend
    // and no owner-key egress on a guest turn.
    let to_cloud = crate::threshold::deny_cloud(crate::vault::deny_cloud(wants_cloud(class, cfg)));
    // RC-6: a turn that is cloud-bound ONLY because the classifier was unsure
    // (low confidence on a conversation intent — the CLASSIFY_FALLBACK shape a
    // garbled echo produces) must NOT reach the actuating cloud tool loop. An
    // uncertain transcript could otherwise open URLs / launch apps. Such turns
    // take the NO-TOOLS persona completion instead, so an unsure transcript can
    // talk but never act. Confident heavy ACTION intents are unaffected.
    let actuating_cloud = to_cloud && !is_uncertain_fallback(class, cfg);

    // Darwin-Prime delegation: pick the handling agent BEFORE acting, then
    // resolve the tool this turn will actually invoke and enforce the agent's
    // allowlist — an out-of-domain match is handed to the tool's real owner so
    // isolation holds (no agent acts through another agent's exclusive tool).
    // The final selection is announced as agent.active so the HUD highlights
    // it and the core color shifts to its hue.
    let agent = select_agent(agents, &class.intent, text, cloud_reachable, to_cloud);
    emit_agent_active(agent);
    // OBOL: note the handling agent so a cloud spend row this turn attributes cost
    // to it (a secret-free agent NAME, never an utterance). No-op accounting seam.
    crate::obol::note_active_agent(&agent.name);

    if actuating_cloud {
        let model = cloud_model(needs_deep_reasoning, cfg);
        telemetry::emit(
            "cloud",
            "route.cloud",
            json!({
                "intent": class.intent,
                "confidence": class.confidence,
                "model": model,
                "deep_reasoning": needs_deep_reasoning,
            }),
        );
        // Bookkeeping must never kill a response (a busy darwin.db would
        // otherwise leave the user with dead air).
        if let Err(e) = memory.record_event("cloud", "route.cloud", text).await {
            warn!(error = %e, "failed to record cloud route event");
        }
        // Tool-use loop: the cloud model can ACT (open apps, search files,
        // set volume, remember facts) before phrasing its spoken answer, so
        // any phrasing of a request routed here still gets things done. Recall
        // is namespaced to the active agent (own namespace + shared facts) so
        // the cloud reply respects constellation isolation like the local one.
        // PROACTIVE RAG: rank the scoped facts by relevance to THIS request and
        // feed the most-relevant few (not the most-recent), so the reply is
        // grounded in the memory that bears on it — neural on-device when the
        // inference server is up, BM25 otherwise, top-K + token bounded.
        let facts: Vec<(String, String)> =
            anthropic::grounded_facts_live(text, memory, &agent.namespace).await;
        // SHARED WORLD MODEL: the entities/relationships relevant to this request,
        // from the shared user.world.* tier (every agent reads the same world; the
        // world model never reads any agent's private namespace). Rides the uncached
        // tail so the tool-loop reply reasons over one coherent world picture.
        let world_context = anthropic::grounded_world_live(text, memory).await;
        // PERSONALIZATION: the bounded user-model summary (observed preferences/
        // patterns/topics/style), rides the same uncached tail so the reply
        // personalizes to the REAL observed user — never an invented one. Reads
        // only the shared user.model.* tier (no namespace), so it can never carry
        // another agent's private notes.
        let personalization = anthropic::grounded_personalization_live(memory).await;
        let history = fetch_history(memory).await;
        // The active agent's own persona (specialists) so the cloud reply is
        // voiced in its persona and caches per-agent; the orchestrator passes
        // None and voices the shared global persona. The shared grounding
        // preamble is always present (it carries the no-fabrication rules), so
        // even an agent whose file is missing degrades to a grounded reply.
        let agent_persona = anthropic::agent_persona_text(&agent.name, agent.is_orchestrator());
        match anthropic::complete_with_tools(
            model,
            cfg.cloud.max_tokens,
            text,
            &facts,
            &history,
            memory,
            &agent.tools,
            &agent.namespace,
            agent_persona.as_deref(),
            &world_context,
            &personalization,
            true, // a direct user turn — trusted, user-originated
        )
        .await
        {
            Ok(response) => {
                return Ok(RouteOutcome {
                    routed_to: "cloud",
                    response,
                    agent: agent.name.clone(),
                    namespace: agent.namespace.clone(),
                    spoken: None,
                })
            }
            Err(e) => {
                // Degrade to the local model rather than going silent.
                // error! (not warn!): recurring cloud failures are a
                // self-heal trigger; the watchdog's burst detector only
                // counts ERROR-level lines (audit fix).
                error!(error = %e, "cloud completion failed; degrading to local generate");
                telemetry::emit(
                    "cloud",
                    "route.cloud_failed",
                    json!({"intent": class.intent, "error": e.to_string()}),
                );
                // Cloud failed -> degrade to the local brain; pick the warm local
                // model by difficulty (None under single-resident => the base).
                let local_model = local_model_for_turn(cfg, class).await;
                let response = generate_in_persona(
                    text,
                    CLOUD_DEGRADE_NOTE,
                    memory,
                    infer,
                    agent,
                    local_model.as_deref(),
                )
                .await;
                return Ok(RouteOutcome {
                    routed_to: "local",
                    response,
                    agent: agent.name.clone(),
                    namespace: agent.namespace.clone(),
                    spoken: None,
                });
            }
        }
    }

    // Conversation-to-cloud (CONTRACT B): casual chat / greetings / opinions —
    // the CONVERSATION intent, the pure llm_voice conversation path — route to
    // a CLOUD PERSONA COMPLETION by default ([router].conversation_route =
    // "cloud_heavy"). The local 4B is near-deterministic on bare greetings (a
    // model-capacity ceiling), so chat goes to cloud Opus/Haiku for genuinely
    // varied, in-character personality. This is a PLAIN persona completion
    // (persona + recent history + known facts + the utterance) — NOT the tool
    // loop: a greeting must never trigger a tool call. Actions, system.query,
    // memory ops, and the heavy/low-confidence cloud routing above are all
    // untouched — only this one intent gains cloud-by-default. A cloud error
    // degrades gracefully to the local converse path below (never silent).
    if class.intent == "conversation" {
        // MODEL TIER: resolve the conversation tier (Override > Auto > Fallback)
        // and surface it for the HUD on EVERY answered conversation turn, whether
        // it lands on cloud or local. The reason distinguishes a manual override
        // from the auto pick from a degrade.
        let (brain, tier, reason) = conversation_brain(cfg, cloud_reachable, class);
        let mut tier_payload = json!({
            "tier": tier.as_str(),
            "reason": reason.as_str(),
            "manual": reason == crate::model_tier::Reason::Override,
            "intent": class.intent,
        });
        // When the turn lands on the LOCAL tier, surface the active warm sub-choice
        // (FAST/CAPABLE) for the HUD's resident-models indicator — only meaningful
        // under a multi-resident warm-set; single-resident omits it (the base
        // answers, no indicator). Does NOT change the tier/model already chosen.
        if matches!(brain, ConversationBrain::Local) {
            if let Some(sub) = local_sub_for_turn(cfg, class).await {
                tier_payload["local_sub"] = json!(sub);
            }
            // BATTERY/THERMAL THROTTLE (#38) indicator: surface the plan reason +
            // whether it actually throttled this local turn. When the plan is neutral
            // (adaptive off, or on AC + nominal thermal) it emits no throttle field so the HUD
            // shows no throttle — honest, never a phantom. Only emitted on local
            // turns (the throttle influences only the on-device sub-choice).
            let plan = power_throttle_plan(cfg).await;
            if plan.is_throttled() {
                tier_payload["throttle"] = json!({
                    "reason": plan.reason.as_str(),
                    "tier_pref": plan.tier_pref.as_str(),
                    "defer_heavy": plan.defer_heavy,
                });
            }
        }
        telemetry::emit("system", "model.tier", tier_payload);
        if let ConversationBrain::Cloud(model) = brain {
            // Same context the local converse path uses: namespaced facts +
            // recent history. Recall is scoped to the active agent so the cloud
            // reply respects constellation isolation like the local one.
            // PROACTIVE RAG: the facts are ranked by relevance to this turn and
            // trimmed to the most-relevant few (top-K + token bounded), so even a
            // casual reply is grounded in the memory that bears on it.
            let facts = anthropic::grounded_facts_live(text, memory, &agent.namespace).await;
            // SHARED WORLD MODEL context for this turn (entities/relationships
            // relevant to the request), from the shared user.world.* tier — every
            // agent reads the same world, never another agent's private notes.
            let world_context = anthropic::grounded_world_live(text, memory).await;
            // PERSONALIZATION: the bounded user-model summary (observed
            // preferences/patterns/topics/style) so the chat reply personalizes to
            // the real observed user. Shared tier only -> never another agent's
            // private notes. Rides the same uncached tail as the world context.
            let personalization = anthropic::grounded_personalization_live(memory).await;
            let history = fetch_history(memory).await;
            // Anti-repeat (CONTRACT B): the last few DARWIN replies, passed so
            // complete_persona can tell Opus not to reuse their wording. This is
            // the load-bearing variation mechanism — Opus 4.8 takes no
            // temperature, so changing the prompt per call is the only way a
            // repeated bare "Hi DARWIN" varies instead of collapsing to one line.
            let avoid = recent_replies(&history, AVOID_RECENT_REPLIES);
            // The live constellation roster, so the cloud brain can name/list/
            // describe DARWIN's agents when asked instead of denying the team
            // exists (the cloud persona carries no static roster). Grounded —
            // the persona prompt forbids inventing agents not in this list.
            // GUEST GATE: withhold the owner's configured agent roster from a guest
            // turn — consistent with the facts/world/personalization/history feeds
            // above (all empty for a guest) and with guest_denied_fast_path refusing
            // the roll-call / agent-query fast paths. A guest gets no owner config
            // (agents.toml can carry owner-chosen agent names/roles).
            let roster = if crate::threshold::is_guest_turn() {
                String::new()
            } else {
                agents.roster_brief()
            };
            // The first-contact brief is converse data — fold it into the
            // utterance so the persona still phrases it on the cloud chat path
            // (the proactive brief never rides a tool loop; this plain
            // completion has none, so it carries the brief safely).
            let chat_text = match brief {
                Some(brief) if !brief.is_empty() => {
                    format!("{text}\n\n[Context for your reply: {brief}]")
                }
                _ => text.to_string(),
            };
            telemetry::emit(
                "cloud",
                "route.cloud",
                json!({
                    "intent": class.intent,
                    "confidence": class.confidence,
                    "model": &model,
                    "conversation": true,
                }),
            );
            if let Err(e) = memory.record_event("cloud", "route.cloud", text).await {
                warn!(error = %e, "failed to record cloud conversation route event");
            }
            // The active agent's own persona (specialists) so the cloud chat
            // reply is voiced in its persona and caches per-agent; the
            // orchestrator passes None and voices the shared global persona.
            let agent_persona = anthropic::agent_persona_text(&agent.name, agent.is_orchestrator());
            match anthropic::complete_persona(
                &model,
                GENERATE_MAX_TOKENS,
                &chat_text,
                &facts,
                &history,
                &roster,
                &avoid,
                agent_persona.as_deref(),
                &world_context,
                &personalization,
            )
            .await
            {
                Ok(response) => {
                    return Ok(RouteOutcome {
                        routed_to: "cloud",
                        response,
                        agent: agent.name.clone(),
                        namespace: agent.namespace.clone(),
                        spoken: None,
                    })
                }
                Err(e) => {
                    // Graceful degrade to the LOCAL converse path below — never
                    // silent. error! (not warn!): recurring cloud failures feed
                    // the self-heal burst detector, like the tool-loop path.
                    error!(error = %e, "cloud conversation completion failed; degrading to local converse");
                    telemetry::emit(
                        "cloud",
                        "route.cloud_failed",
                        json!({"intent": class.intent, "error": e.to_string(), "conversation": true}),
                    );
                    // Fall through to the local converse path (the route.local
                    // telemetry below marks the brain that actually answered).
                }
            }
        } else {
            // OFFLINE AGENTIC TOOL-USE (task #3). The tier resolved to Local
            // (the "work offline" override, no cloud key, or a cloud-unreachable
            // fallback), so this conversation turn is answered ON-DEVICE. Before
            // the plain converse below, give the resident 4B BOUNDED agency over a
            // CURATED SAFE LOCAL-TOOL subset: prompt -> parse one tool call ->
            // EXECUTE via the SAME gated `execute_tool` (consequential confirmation
            // + voice-id + lockdown + per-action policy ALL apply offline) -> feed
            // the result back -> at most N rounds -> FALL BACK to a plain converse
            // when the 4B emits no tool call. ONLINE is untouched (this is the
            // `else` of the Cloud branch); a non-conversation intent never reaches
            // here. The 4B's tool-call adherence is a real ceiling — it is bounded
            // and degrades gracefully; the same safety gates that govern the cloud
            // loop govern this one.
            let facts_kv = agent_facts(memory, &agent.namespace).await;
            let facts: Vec<String> = facts_kv
                .iter()
                .map(|(k, v)| format!("{k}: {v}"))
                .collect();
            let history = fetch_history(memory).await;
            if let Some(outcome) = anthropic::complete_with_local_tools(
                cfg,
                infer,
                GENERATE_MAX_TOKENS,
                text,
                &history,
                &facts,
                memory,
                &agent.tools,
                &agent.namespace,
            )
            .await
            {
                if outcome.tools_used > 0 {
                    // A safe local tool actually engaged this turn. Surface the
                    // honest offline-agency telemetry for the HUD (ACTING OFFLINE),
                    // then voice the tool RESULTS in persona via the streamed
                    // converse path. The HUD copy is honest: the on-device 4B used
                    // local tools, it is less reliable at tool-calling than the
                    // cloud model, and the same safety gates applied (gated => a
                    // consequential tool parked/refused offline, exactly as online).
                    telemetry::emit(
                        "local",
                        "local_tools.engaged",
                        json!({
                            "tools_used": outcome.tools_used,
                            "tools": outcome.tool_names,
                            "gated": outcome.gated,
                            "intent": class.intent,
                        }),
                    );
                    // Fold the first-contact brief (converse data) into the tool
                    // results so the persona still phrases it on this offline path.
                    let mut data = outcome.data;
                    if let Some(brief) = brief {
                        if !brief.is_empty() {
                            data = if data.is_empty() {
                                brief.to_string()
                            } else {
                                format!("{data}\n\n{brief}")
                            };
                        }
                    }
                    let data_opt = (!data.is_empty()).then_some(data.as_str());
                    // Multi-resident LOCAL sub-choice (task #17): this is an
                    // on-device turn, so pick the warm local model by difficulty
                    // (None under the single-resident default => the base).
                    let local_model = local_model_for_turn(cfg, class).await;
                    match speech::converse_speak(
                        text,
                        GENERATE_MAX_TOKENS,
                        &history,
                        &facts,
                        data_opt,
                        &agent.voice,
                        Some(agent.persona_name()),
                        local_model.as_deref(),
                        infer,
                        started,
                        reply,
                    )
                    .await
                    {
                        Ok(spoken) => {
                            return Ok(RouteOutcome {
                                routed_to: "local",
                                response: spoken.response,
                                agent: agent.name.clone(),
                                namespace: agent.namespace.clone(),
                                spoken: Some(SpokenReply {
                                    route_ms: spoken
                                        .done_at
                                        .duration_since(route_entry)
                                        .as_millis()
                                        as u64,
                                    report: spoken.report,
                                }),
                            })
                        }
                        Err(e) => {
                            // converse_speak only errs when NOTHING played; degrade
                            // to generate+speak so the tool results are still voiced.
                            error!(error = %e, "offline tool-loop converse failed before audio; falling back to generate+speak");
                            // Same warm local model the converse path chose, so the
                            // fallback stays on the same on-device brain.
                            let response = generate_in_persona(
                                text,
                                &data,
                                memory,
                                infer,
                                agent,
                                local_model.as_deref(),
                            )
                            .await;
                            return Ok(RouteOutcome {
                                routed_to: "local",
                                response,
                                agent: agent.name.clone(),
                                namespace: agent.namespace.clone(),
                                spoken: None,
                            });
                        }
                    }
                }
                // No tool engaged (the 4B emitted no parseable call, or the bound
                // was reached with nothing run): fall through to the plain converse
                // path below — today's offline behavior, unchanged.
            }
        }
    }

    telemetry::emit(
        "local",
        "route.local",
        json!({"intent": class.intent, "confidence": class.confidence}),
    );

    // Tool-allowlist isolation: the local intent IS the tool name in the
    // allowlist (app.launch, web.open, system.query, memory.store, ...). The
    // selected agent must hold it; if not, hand the turn to the tool's real
    // owner (or the orchestrator, who holds everything) and re-announce so the
    // HUD core tracks the agent that actually acts. select_agent already
    // routes these intents to their owners, so a re-route here only fires for
    // a keyword pick that landed on the wrong specialist — isolation, enforced.
    let agent = enforce_tool(agents, agent, &class.intent);

    // Silicon Canvas voice control (SPEC §6): a precise control phrase ("show
    // me the 3V3 net", "trace this net", "run ERC", "open silicon canvas") maps
    // to a LAUNCH or a STRUCTURED op forwarded to the running app. Checked
    // before the generic local handlers so an op phrase that would otherwise
    // classify as conversation/app.launch is handled deterministically and the
    // app never sees natural language — the daemon forwards structured ops
    // ONLY. The action's verified outcome is converse data, phrased in persona
    // on the streamed path below (llm_voice), exactly like the app-launch path.
    // Vision voice control (mirrors Silicon Canvas): "what do you see", "who is
    // there", "watch the door|screen", "analyze this video" map to a LAUNCH or a
    // STRUCTURED op forwarded to the running Vision app. Checked alongside the
    // Silicon Canvas seam, before the generic local handlers, so a precise
    // vision phrase is handled deterministically and the app never sees natural
    // language — the daemon forwards structured ops ONLY (DEFENSIVE-ONLY: the
    // ops carry no identity query; the app detects presence/objects, not "who").
    // Nexus voice control (mirrors Silicon Canvas): "mute the mic", "route input
    // 1 to the monitor", "set input gain to -18", "load the vocal preset", "what
    // are the levels" map to a LAUNCH or a STRUCTURED op (gain.set / route.set /
    // monitor.set / preset.load / state.get) forwarded to the running Nexus app.
    // Same seam, same discipline: the app exposes ops only and never parses
    // natural language (SPEC §6) — the daemon classifies the phrase and forwards
    // the structured op line VERBATIM.
    // Mark-Forge voice control (mirrors the three seams above): "open the physics
    // sandbox", "drop a box", "reset the simulation", "set gravity to the moon",
    // "pause"|"step" map to a LAUNCH or a STRUCTURED op (body.spawn / world.reset /
    // set.gravity / world.step) forwarded to the running Mark-Forge engine. Same
    // discipline: the engine exposes ops only and never parses natural language
    // (SPEC §7) — the daemon classifies the phrase and forwards the structured op
    // line VERBATIM; the headless CPU/f64 engine acts, the DEVICE-GATED R3F render
    // is never opened here.
    // RC-11: mute the mic NOW, before any local handler actuates. A local
    // action (`open_url`, app launch) fires inside the handlers below — BEFORE
    // converse_speak's ensure_guard() would otherwise engage the SPEAKING guard
    // (instant_opener ships off, so that only happens when the first reply clip
    // plays). Without this, the ~1-2s of STT/handler/converse-setup latency ran
    // with is_speaking()=false and the capture gate wide open, so the user's own
    // just-spoken command was re-segmented, re-transcribed, and re-routed —
    // opening the URL a second and third time (the live triple-open). This is
    // the local path ONLY; the cloud path returned far above and stays mic-live
    // through its (silent, possibly long) round trip so the user can still
    // correct. The guard is shared via the SPEAKING refcount, so the later
    // ensure_guard() in converse/speak is a no-op and complete()/abandon()
    // releases the single guard after the echo tail — no double-count, no leak.
    // VLM DESCRIBE (task #2): "describe my screen" / "what am I looking at" /
    // "describe this image <path>" routes to the VISION agent and calls the
    // on-device describe_image op (DISTINCT from the OCR read.screen path in
    // vision_command). Checked FIRST so a describe verb is never shadowed by the
    // OCR screen-read or the bare vision launch, and re-pins the active agent to
    // Vision (the vision owner) so the HUD + persona track the agent that acts.
    // The image is read ON-DEVICE; pixels never leave the device. When the VLM is
    // off / not downloaded, handle_describe FALLS BACK honestly (OCR for a screen
    // request, an honest gate line for an image) — never a fabricated description.
    let describe = describe_command(text);
    // IMAGE GENERATION (task #18): "generate/make/draw/create an image of X"
    // routes to the VISION agent (the visual-capability owner, same as describe)
    // and calls the on-device generate_image op (MLX diffusion). DISTINCT from the
    // describe path above: describe REASONS about an existing image; generate
    // RENDERS a new one from a text prompt. The prompt + the pixels stay
    // ON-DEVICE (saved under state/images/); there is NO cloud image API. When the
    // [image] gate is off / no model is named / the model isn't downloaded,
    // handle_generate_image surfaces the gate HONESTLY — never a fabricated image,
    // never a cloud fallback. Checked AFTER describe so a describe verb is never
    // shadowed (generate_image_command already vetoes a describe phrase).
    let generate_image = generate_image_command(text);
    // IDENTIFY SOUND (task #15): "what was that sound" / "identify that noise" /
    // "what am I hearing" routes to the VISION agent and calls the on-device
    // classify.sound op over a clip the daemon ALREADY captured (DISTINCT from
    // STT — speech — which transcribes words). The clip is the daemon's last
    // captured segment under state/tmp (never user-named, no new mic open); when
    // there is none, the handler says so honestly rather than guessing. ONLY the
    // sound-class LABELS leave the op — the audio never leaves the device.
    let sound_clip = {
        let candidate = sound_clip_path(root);
        let latest = if candidate.exists() { Some(candidate.as_path()) } else { None };
        identify_sound_clip_or_request(text, latest)
    };
    // LUMEN (#45): "read me the screen / the buttons" (READ-ONLY narrate) + "click
    // the <ordinal|name>" (-> the UNCHANGED ui_actuate capstone). Computed here so
    // the ACT arm can re-pin the active agent to the ui_actuate OWNER below, and
    // dispatch below (before the Vision arm) so a control read/act is Lumen's.
    let lumen = lumen_command(text);
    // Re-pin the active agent to Vision (the vision owner) for the describe, the
    // image-generation, and the identify-sound intents so the HUD + persona track
    // the agent that acts. A Lumen ACTUATION re-pins to the ui_actuate-OWNING
    // specialist so execute_tool runs under ITS allowlist (the capstone gate is
    // unchanged — it is just applied as the owning agent, like any live tool call).
    let agent: &Agent = if describe.is_some() || generate_image.is_some() || sound_clip.is_some() {
        let vision = agents.get(VISION_APP).unwrap_or(agent);
        emit_agent_active(vision);
        vision
    } else if matches!(lumen, Some(LumenCommand::Act(_))) {
        let actuator = agents.owner_of("ui_actuate").unwrap_or(agent);
        emit_agent_active(actuator);
        actuator
    } else {
        agent
    };

    reply.mute_for_action();
    let mut out = if crate::threshold::is_guest_turn() {
        // THRESHOLD — GUEST MODE: a guest reaches NONE of the vision / image / sound
        // / design handlers below — each reads the owner's screen or last-captured
        // audio, or renders on their machine. (describe / generate-image / silicon-
        // canvas / vision / nexus / mark-forge are already refused upstream by the
        // fast-path gate; this ALSO covers the sound-identify path and is defense in
        // depth.) Fall to the guest-gated handle_local, which admits only
        // conversation + non-personal status and refuses the rest.
        handle_local(&class.intent, &class.args, text, memory, app_registry, agent).await
    } else if let Some(req) = describe {
        handle_describe(req, cfg, infer, app_registry, root).await
    } else if let Some(req) = generate_image {
        handle_generate_image(req, cfg, infer).await
    } else if let Some(req) = sound_clip {
        handle_identify_sound(req.clip, app_registry, root).await
    } else if let Some(cmd) = silicon_canvas_command(text) {
        handle_silicon_canvas(cmd, app_registry).await
    } else if let Some(cmd) = lumen {
        // Lumen (#45) BEFORE Vision: a control read/act is Lumen's; `agent` is
        // already the ui_actuate owner for the ACT arm (re-pinned above).
        handle_lumen(cmd, memory, app_registry, agent).await
    } else if let Some(cmd) = vision_command(text) {
        handle_vision(cmd, app_registry).await
    } else if let Some(cmd) = nexus_command(text) {
        handle_nexus(cmd, app_registry).await
    } else if let Some(cmd) = mark_forge_command(text) {
        handle_mark_forge(cmd, app_registry).await
    } else {
        handle_local(&class.intent, &class.args, text, memory, app_registry, agent).await
    };
    if !out.llm_voice {
        return Ok(RouteOutcome {
            routed_to: "local",
            response: out.data,
            agent: agent.name.clone(),
            namespace: agent.namespace.clone(),
            spoken: None,
        });
    }
    // First-contact brief: appended AFTER the llm_voice gate, so it can only
    // ever reach a persona-phrased path (converse, or the generate fallback
    // below) — raw data replies would speak it verbatim.
    if let Some(brief) = brief {
        if out.data.is_empty() {
            out.data = brief.to_string();
        } else {
            out.data = format!("{}\n\n{brief}", out.data);
        }
    }

    // Streamed path: one converse op fuses generation and TTS server-side;
    // the first sentence is audible while the rest is still decoding. The
    // active agent's OWN voice and persona are passed (per-agent voicing), and
    // recall is namespaced — the agent sees its own namespace plus shared
    // facts only (constellation isolation at the recall layer).
    let facts_kv = agent_facts(memory, &agent.namespace).await;
    let facts: Vec<String> = facts_kv
        .iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect();
    let history = fetch_history(memory).await;
    let data_opt = (!out.data.is_empty()).then_some(out.data.as_str());
    // Multi-resident LOCAL sub-choice (task #17): the persona-voicing converse
    // runs on-device, so pick the warm local model by difficulty (None under the
    // single-resident default => the base, exactly today's wire).
    let local_model = local_model_for_turn(cfg, class).await;
    match speech::converse_speak(
        text,
        GENERATE_MAX_TOKENS,
        &history,
        &facts,
        data_opt,
        &agent.voice,
        Some(agent.persona_name()),
        local_model.as_deref(),
        infer,
        started,
        reply,
    )
    .await
    {
        Ok(spoken) => Ok(RouteOutcome {
            routed_to: "local",
            response: spoken.response,
            agent: agent.name.clone(),
            namespace: agent.namespace.clone(),
            spoken: Some(SpokenReply {
                route_ms: spoken.done_at.duration_since(route_entry).as_millis() as u64,
                report: spoken.report,
            }),
        }),
        Err(e) => {
            // converse_speak only errs when NOTHING played, so falling back
            // to the old generate -> speak path cannot double-speak and the
            // daemon is never mute. error!: a converse outage is a recurring
            // hard failure the self-heal burst detector must see.
            error!(error = %e, "converse failed before any audio; falling back to generate+speak");
            telemetry::emit(
                "system",
                "inference.unavailable",
                json!({"op": "converse", "error": e.to_string()}),
            );
            // Same warm local model the converse path chose, so the degraded
            // generate stays on the same on-device brain.
            let response = generate_in_persona(
                text,
                &out.data,
                memory,
                infer,
                agent,
                local_model.as_deref(),
            )
            .await;
            Ok(RouteOutcome {
                routed_to: "local",
                response,
                agent: agent.name.clone(),
                namespace: agent.namespace.clone(),
                spoken: None,
            })
        }
    }
}

/// Process-wide roll-call cancel flag. The roll-call checks it before each
/// agent and stops cleanly when set, so a barge-in hook or a shutdown path can
/// interrupt the sequence mid-team (the reel centerpiece must be
/// interruptible). Set+cleared around each roll-call by `roll_call`; exposed
/// via `interrupt_roll_call` for a future barge-in caller.
static ROLL_CALL_CANCEL: AtomicBool = AtomicBool::new(false);

/// Request the in-progress roll-call to stop after the current agent's clip.
/// Idempotent and safe to call when no roll-call is running.
pub fn interrupt_roll_call() {
    ROLL_CALL_CANCEL.store(true, Ordering::Relaxed);
}

/// Clear the roll-call cancel flag (RC-9). Called from speech::clear_barge_in
/// at the start of every new turn so both interrupt flags (BARGE_IN and
/// ROLL_CALL_CANCEL) share ONE lifecycle: a barge over a non-roll-call reply
/// could otherwise leave ROLL_CALL_CANCEL latched true (it is only cleared at
/// roll_call start), so the NEXT roll-call would abort before its first agent.
/// Idempotent.
pub fn clear_roll_call_interrupt() {
    ROLL_CALL_CANCEL.store(false, Ordering::Relaxed);
}

/// The constellation roll-call (item 3): every agent, in roster order, speaks
/// its ONE-LINE self-introduction in ITS OWN voice (one sequential speak op
/// per agent), and an agent.active is emitted for each so the HUD highlights
/// them in turn and the core color cycles. Interruptible: the cancel flag is
/// checked before each agent and the loop also yields, so a barge-in/shutdown
/// can stop it mid-team. Returns the joined intro text (for the transcript)
/// and the reply's timing report. Never errors — a per-agent synthesis failure
/// skips that one agent rather than aborting the whole reel.
async fn roll_call(
    agents: &AgentRegistry,
    infer: &mut InferenceClient,
    reply: &mut speech::ReplySession,
    started: Instant,
    root: &Path,
    cfg: &Config,
) -> (String, speech::SpeakReport) {
    // Fresh run: clear any stale cancel from a previous, interrupted roll-call.
    ROLL_CALL_CANCEL.store(false, Ordering::Relaxed);
    telemetry::emit(
        "local",
        "rollcall.started",
        json!({"agents": agents.all().len()}),
    );

    // ADDITIVE (Phase-2): streaming opt-in + pronunciation locator from [voice].
    // SpeakExtras::none() with the shipped defaults -> the speak wire is unchanged.
    let extras = crate::inference::SpeakExtras::from_config(cfg);

    let mut spoken_intros: Vec<String> = Vec::new();
    for agent in agents.all() {
        if ROLL_CALL_CANCEL.load(Ordering::Relaxed) {
            info!("roll-call interrupted; stopping after {} agents", spoken_intros.len());
            telemetry::emit(
                "local",
                "rollcall.interrupted",
                json!({"spoken": spoken_intros.len()}),
            );
            break;
        }
        // Highlight this agent first so the HUD core color leads its voice.
        emit_agent_active(agent);
        let intro = agent.intro(root);
        // Each agent speaks in ITS OWN voice: resolve the backend per agent so the
        // cloud voice tier (when on + key + non-offline + the agent mapped) uses
        // that agent's ElevenLabs voice id, else its on-device Kokoro voice. With
        // the tier OFF (default) this is exactly today's per-agent Kokoro voicing.
        let (backend, el_key) =
            speech::resolve_speak_backend(cfg, &agent.name, &agent.voice).await;
        // EXPRESSIVENESS (#33): a roll-call intro is a GREETING (=> Warm prosody on
        // the EL-v3 rich path; coarse/neutral elsewhere). A roll-call is never a
        // required confirmation. With adaptive_prosody OFF this is SpeakShape::neutral
        // -> the speak wire is byte-for-byte today's. Whisper folds in the same way as
        // the base speak path (process-global state, never silencing a confirm).
        let profile = crate::prosody::classify_prosody(crate::prosody::ReplyKind::Greeting, false);
        let mut intro_shape = crate::prosody::shape_speak_request(cfg, profile, &backend);
        let whisper_on = crate::prosody::whisper_state_is_on();
        intro_shape = crate::prosody::apply_whisper(intro_shape, whisper_on, false);
        crate::prosody::emit_telemetry(profile, &backend, &intro_shape, whisper_on);
        // English self-introduction — no Babel target language to thread.
        match infer.speak(&intro, &backend, el_key.as_deref(), None, &intro_shape, &extras).await {
            Ok(wav) => {
                if reply.push_clip(&wav).await {
                    spoken_intros.push(intro);
                } else {
                    warn!(agent = %agent.name, "roll-call intro produced no audio; skipping");
                }
            }
            Err(e) => {
                // One agent's synthesis failure must not abort the reel.
                warn!(agent = %agent.name, error = %e, "roll-call intro synthesis failed; skipping");
            }
        }
        // Cooperative yield: lets a cancel set from another task take effect
        // promptly between agents even on a busy runtime.
        tokio::task::yield_now().await;
    }

    telemetry::emit(
        "local",
        "rollcall.completed",
        json!({"spoken": spoken_intros.len()}),
    );
    let report = reply.finish_report(started).await;
    (spoken_intros.join(" "), report)
}

/// Answer an agent-ROSTER query from the live registry — never the classifier +
/// local model, which (lacking the roster in context) hallucinates agents that
/// do not exist. Cloud-reachable: phrase the REAL roster in persona (grounded —
/// persona.txt forbids inventing agents not in the roster, and the anti-repeat
/// hint keeps it fresh). Offline or on a cloud error: a deterministic spoken list
/// (still the real team, accurate, no model guessing). Returns the reply text and
/// the brain that produced it ("cloud"/"local") for the route telemetry.
async fn answer_agent_roster(
    text: &str,
    agents: &AgentRegistry,
    memory: &Memory,
    cfg: &Config,
    cloud_reachable: bool,
) -> (String, &'static str) {
    let roster = agents.roster_brief();
    // The roster reply is a simple, confident conversation-style answer: model it
    // as a light/high-confidence turn so the tier resolver (override + auto) keeps
    // today's cloud-when-reachable behavior, while still honoring an offline
    // override (which forces the deterministic local roster below). A model-control
    // override is respected here exactly as on the main conversation path.
    let roster_class = Classification {
        intent: "conversation".to_string(),
        complexity: "light".to_string(),
        confidence: 1.0,
        args: serde_json::Value::Null,
    };
    let (brain, tier, reason) = conversation_brain(cfg, cloud_reachable, &roster_class);
    let mut tier_payload = json!({
        "tier": tier.as_str(),
        "reason": reason.as_str(),
        "manual": reason == crate::model_tier::Reason::Override,
        "intent": "agent_query",
    });
    // Local-tier roster turn: surface the active warm sub-choice for the HUD
    // (only under multi-resident; single-resident omits it). Same honest readout
    // as the conversation path; no change to the tier/model chosen.
    if matches!(brain, ConversationBrain::Local) {
        if let Some(sub) = local_sub_for_turn(cfg, &roster_class).await {
            tier_payload["local_sub"] = json!(sub);
        }
        // #38 throttle indicator (absent when the plan is neutral).
        let plan = power_throttle_plan(cfg).await;
        if plan.is_throttled() {
            tier_payload["throttle"] = json!({
                "reason": plan.reason.as_str(),
                "tier_pref": plan.tier_pref.as_str(),
                "defer_heavy": plan.defer_heavy,
            });
        }
    }
    telemetry::emit("system", "model.tier", tier_payload);
    if let ConversationBrain::Cloud(model) = brain {
        let prime = agents.orchestrator();
        // PROACTIVE RAG: facts ranked by relevance to the roster query, scoped to
        // the orchestrator's namespace (same isolation-safe view), top-K + token
        // bounded — so any user fact that bears on the question is surfaced.
        let facts = anthropic::grounded_facts_live(text, memory, &prime.namespace).await;
        // SHARED WORLD MODEL context (relevant to the question) from the shared
        // user.world.* tier — consistent grounding even on the roster path.
        let world_context = anthropic::grounded_world_live(text, memory).await;
        let history = fetch_history(memory).await;
        let avoid = recent_replies(&history, AVOID_RECENT_REPLIES);
        telemetry::emit(
            "cloud",
            "route.cloud",
            json!({"intent": "agent_query", "model": &model, "conversation": true}),
        );
        // The roster query is answered by the orchestrator (darwin), which voices
        // the global persona — so no per-agent persona block (None), matching
        // the namespaced facts seeded from the orchestrator above.
        let agent_persona = anthropic::agent_persona_text(&prime.name, prime.is_orchestrator());
        match anthropic::complete_persona(
            &model,
            GENERATE_MAX_TOKENS,
            text,
            &facts,
            &history,
            &roster,
            &avoid,
            agent_persona.as_deref(),
            &world_context,
            // The roster query is ABOUT the team, not the user — no personalization
            // grounding is needed (and the deterministic roster below carries none),
            // so pass an empty summary: honest and focused.
            "",
        )
        .await
        {
            Ok(r) => return (r, "cloud"),
            Err(e) => {
                // Never go silent or let the local model freelance the roster —
                // fall through to the grounded deterministic list below.
                error!(error = %e, "cloud agent-roster reply failed; using the grounded deterministic roster");
                telemetry::emit(
                    "cloud",
                    "route.cloud_failed",
                    json!({"intent": "agent_query", "error": e.to_string()}),
                );
            }
        }
    }
    telemetry::emit("local", "route.local", json!({"intent": "agent_query"}));
    (agents.roster_spoken(), "local")
}

/// The UNCHANGED heavy/low-confidence cloud predicate: route to cloud iff the
/// classifier marked the turn heavy OR its confidence fell below the
/// configured threshold. Applies to EVERY intent (this is not the
/// conversation-specific routing) — extracted only so the contract's
/// "heavy -> cloud, action -> local" invariants are unit-testable.
fn wants_cloud(class: &Classification, cfg: &Config) -> bool {
    class.complexity == "heavy" || class.confidence < cfg.router.cloud_confidence_threshold
}

/// RC-6: whether this cloud-bound turn is an UNCERTAIN FALLBACK — a
/// conversation intent the classifier was not confident about (below the cloud
/// threshold). This is exactly the shape a garbled/echo transcript produces
/// (CLASSIFY_FALLBACK = conversation / 0.3 / heavy). Such a turn must take the
/// NO-TOOLS persona completion, never the actuating tool loop, so an unsure
/// transcript can speak but cannot open URLs or launch apps. A CONFIDENT
/// conversation turn, or any non-conversation intent (a real, if weakly
/// recognized, action), is NOT a fallback and keeps its existing routing. Pure,
/// so the boundary is unit-testable.
fn is_uncertain_fallback(class: &Classification, cfg: &Config) -> bool {
    class.intent == "conversation" && class.confidence < cfg.router.cloud_confidence_threshold
}

/// The UNCHANGED cloud model pick for the heavy/low-confidence path: the heavy
/// model (Opus) for deep reasoning, else the fast model (Haiku). Extracted so
/// "heavy -> opus" stays verified without a live call.
fn cloud_model(needs_deep_reasoning: bool, cfg: &Config) -> &str {
    if needs_deep_reasoning {
        &cfg.cloud.heavy_model
    } else {
        &cfg.cloud.fast_model
    }
}

/// Which brain answers a CONVERSATION turn, decided from [router].
/// conversation_route, the chosen model, and whether the cloud key is present.
/// Pure and unit-tested so the routing-decision table is verified without any
/// live cloud call or inference client.
#[derive(Debug, PartialEq)]
enum ConversationBrain {
    /// A plain cloud persona completion using this model (Opus for the Heavy
    /// tier, Haiku for the Fast tier). Owns the model string (resolved from the
    /// [cloud] config via the model-tier resolver).
    Cloud(String),
    /// The local 4B converse path — Local tier (route "local", no cloud key, an
    /// unknown route value, an offline override, or a cloud-unreachable fallback)
    /// all land here; a cloud error degrades to it too.
    Local,
}

/// Decide where a CONVERSATION turn is answered, now through the MODEL-TIER
/// resolver so the per-turn override + auto-difficulty heuristic apply. The
/// precedence is Override > Auto > Fallback ([`crate::model_tier::resolve_tier`]):
///   * an explicit voice override ("use the powerful model" / "go offline") wins;
///   * else AUTO maps [router].conversation_route (the durable default) refined by
///     THIS turn's difficulty (a trivial chat turn steps down to Fast, a heavy one
///     up to Heavy) — preserving today's behavior at the config default;
///   * a cloud tier with no cloud this turn (no key / offline / a Local override)
///     resolves Local (Reason::Fallback / Override) — NO cloud call is made.
///
/// `cloud_key_present` is whether a cloud call can be made at all this turn. The
/// resolved tier maps to a model string via [`crate::model_tier::tier_to_model`]
/// (Heavy -> heavy_model, Fast -> fast_model, Local -> the on-device path), so the
/// [cloud] config stays the single source of truth. Returns the brain AND the
/// `(Tier, Reason)` so the caller can emit `model.tier` telemetry for the HUD.
/// Pure + unit-tested — no live cloud call.
fn conversation_brain(
    cfg: &Config,
    cloud_key_present: bool,
    class: &Classification,
) -> (ConversationBrain, crate::model_tier::Tier, crate::model_tier::Reason) {
    // OBOL BUDGET-FLOOR: the current dollar-budget pressure (Pressure::None under
    // the shipped no-cap default, so byte-for-byte today's routing until the owner
    // sets `[obol].daily_usd_cap`). Read synchronously from the in-memory day total;
    // it is a REDUCE-ONLY precedence input (Override > Budget-floor > Auto > Fallback)
    // that can only step the tier DOWN toward the cheaper/on-device path.
    let budget = crate::obol::current_budget_pressure(cfg);
    let (tier, reason) = crate::model_tier::resolve_tier(
        cfg,
        crate::model_tier::current_override(),
        &class.complexity,
        class.confidence,
        cfg.router.cloud_confidence_threshold,
        cloud_key_present,
        budget,
    );
    let brain = match crate::model_tier::tier_to_model(tier, cfg) {
        crate::model_tier::ModelChoice::Cloud(model) => ConversationBrain::Cloud(model),
        crate::model_tier::ModelChoice::Local => ConversationBrain::Local,
    };
    (brain, tier, reason)
}

/// The Local-tier SUB-CHOICE for an on-device turn (task #17): which WARM local
/// model the local converse/generate op should answer with. Returns `Some(id)`
/// ONLY when the operator configured a MULTI-RESIDENT warm-set ([models].local_warm
/// + a budget that admits an extra) AND the AUTO-by-difficulty heuristic picks a
///   NON-base model for this turn; otherwise `None` -> the server answers on the base
///   single-resident model (today's behavior).
///
/// This is the conservative, honest wiring: it never names a model that is not in
/// the budget-bounded warm plan, it leaves the wire untouched (and so identical to
/// today) under the single-resident default, and an unknown id the server would
/// fall back to the base anyway. PURE given `cfg` + the classification — no cloud,
/// no load. It does NOT change WHICH tier is chosen; it only refines the already-
/// chosen Local tier, and makes no cloud call.
async fn local_model_for_turn(cfg: &Config, class: &Classification) -> Option<String> {
    let tel = crate::model_tier::local_warm_telemetry(cfg);
    // Single-resident (the default + low-RAM path): nothing to choose — the base
    // answers every local turn, exactly as today. Send no local_model.
    if !tel.multi_resident {
        return None;
    }
    // BATTERY/THERMAL THROTTLE (#38): the LIVE (TTL-cached) power reading is
    // consulted when [power].adaptive is on (the SHIPPED DEFAULT — so the default
    // config does read live power on a local turn, bounded by the 15s cache);
    // with the flag OFF the plan is NEUTRAL (Auto sub-tier, defer nothing),
    // byte-for-byte the prior AUTO-by-difficulty behavior. A throttled plan
    // biases the sub-choice toward the cheaper Fast warm model to save
    // battery/heat — but ONLY on an easy turn: throttled_sub_tier keeps AUTO on
    // a hard/low-confidence turn, so select_local_model keeps the capable base
    // and a throttle can NEVER degrade a genuinely hard offline turn.
    let plan = power_throttle_plan(cfg).await;
    let sub = crate::model_tier::throttled_sub_tier(
        &plan,
        &class.complexity,
        class.confidence,
        cfg.router.cloud_confidence_threshold,
    );
    let chosen = crate::model_tier::select_local_model(
        &tel.planned,
        sub,
        &class.complexity,
        class.confidence,
        cfg.router.cloud_confidence_threshold,
    );
    // Only thread a NON-base id (a base pick is the default wire => omit it).
    if chosen == tel.base {
        None
    } else {
        Some(chosen.to_string())
    }
}

/// The current battery/thermal throttle plan for this turn (#38). DEVICE-GATED:
/// when [power].adaptive is ON (the shipped default) this reads the LIVE
/// (TTL-cached) `pmset`/thermal state and feeds it to the pure throttle policy;
/// with the flag OFF it returns the NEUTRAL plan, so routing is byte-for-byte
/// today's. A failed read degrades to neutral (never a fabricated low battery).
async fn power_throttle_plan(cfg: &Config) -> crate::model_tier::ThrottlePlan {
    // [power].adaptive (ships ON): feed the LIVE (TTL-cached) battery + thermal
    // reading so a real on-battery / thermally-pressured state can actually
    // influence the on-device sub-choice (the throttle can now fire). OFF -> the
    // neutral reading, so routing is byte-for-byte today's. A failed read
    // degrades to neutral inside read_power_cached; NEVER a fabricated low
    // battery, and the throttle only ever steers the on-device sub-choice toward
    // the cheaper warm model — it NEVER loosens a gate or forces a cloud call.
    let reading = if cfg.power.adaptive {
        crate::power::read_power_cached().await
    } else {
        crate::power::PowerReading::neutral()
    };
    crate::power::current_plan(cfg, reading)
}

/// The ACTIVE local sub-choice label for THIS turn's `model.tier` telemetry — the
/// HUD's resident-models FAST/CAPABLE indicator (consumed by `applyLocalSub`).
/// Returns `Some("fast")` when the AUTO heuristic answered this turn on the faster
/// non-base warm model, `Some("capable")` when the capable base answered while a
/// multi-resident warm-set was in effect, and `None` under single-resident (the
/// default + low-RAM path: the base answers every local turn, so there is no
/// sub-choice to report and the HUD indicator stays empty — honest, not stale).
/// PURE; mirrors `local_model_for_turn`'s decision so the readout matches the model
/// that actually answered. Does NOT change which tier/model is chosen.
async fn local_sub_for_turn(cfg: &Config, class: &Classification) -> Option<&'static str> {
    // Single-resident => no sub-choice; the base answers (no indicator). Only
    // report a sub-choice when multi-resident actually selected among warm models,
    // so the label reflects the model that answered (Fast) or the base it kept
    // (Capable) — never a phantom choice under the single-resident default.
    match local_model_for_turn(cfg, class).await {
        Some(_) => Some(crate::model_tier::LocalSubTier::Fast.as_str()),
        None if crate::model_tier::local_warm_telemetry(cfg).multi_resident => {
            Some(crate::model_tier::LocalSubTier::Capable.as_str())
        }
        None => None,
    }
}

/// Route a non-default Capability-Selector [`Mode`](crate::selector::Mode) to its
/// pipeline, returning the finished [`RouteOutcome`] (or `None` to DECLINE and let
/// the normal pipeline below handle the turn — e.g. an empty world read).
///
/// Each mode's pipeline reuses the already-built, already-gated subsystem:
///   * `WorldQuery`  -> a DETERMINISTIC, READ-ONLY answer from the shared World
///     Model (no cloud, no tool loop). Declines (None) on an empty world so the
///     normal pipeline can still talk about the topic.
///   * `WorldUpdate` -> the cloud tool loop CONSTRAINED to the `world_update` tool,
///     which folds the stated fact into ONLY the shared `user.world.*` tier (never
///     a consequential external action, never a private namespace). Degrades
///     gracefully offline (no fabrication).
///   * `Mission`     -> FURY's bounded mission engine (`run_fury_mission`):
///     decompose -> dispatch each sub-task under its owning specialist's allowlist
///     + the consequential gate -> synthesize. Degrades to a friendly line offline.
///   * `Standing`    -> PROPOSE ONLY (`propose_standing_mission`): parks behind the
///     cross-turn confirmation gate + the armed-by-default master switch (a confirmed
///     action still needs a fresh per-action confirm). Creates nothing here (Rail 2).
///
/// `OneShot` never reaches this function (the caller falls straight through).
async fn route_capability(
    mode: crate::selector::Mode,
    text: &str,
    memory: &Memory,
    agents: &AgentRegistry,
    cloud_reachable: bool,
) -> Option<RouteOutcome> {
    use crate::selector::Mode;
    // The shared World Model is namespace-independent, and the selector's
    // capabilities are orchestrator-level dispatch, so they are voiced by the
    // orchestrator (DARWIN-Prime). WorldUpdate re-homes to the tool's owner below.
    let prime = agents.orchestrator();
    match mode {
        Mode::OneShot => None, // never routed here; the caller handles it.

        Mode::WorldQuery => {
            // DETERMINISTIC read-only answer from the shared world tier. If the
            // world holds nothing on the topic, DECLINE so the normal pipeline can
            // still answer conversationally instead of a dead "nothing recorded".
            let snapshot = anthropic::grounded_world_live(text, memory).await;
            if snapshot.trim().is_empty() {
                return None;
            }
            let response = anthropic::world_query_live(memory, text).await;
            emit_agent_active(prime);
            Some(RouteOutcome {
                routed_to: "local",
                response,
                agent: prime.name.clone(),
                namespace: prime.namespace.clone(),
                spoken: None,
            })
        }

        Mode::WorldUpdate => {
            // Fold the stated fact into the SHARED world via the cloud tool loop,
            // constrained to ONLY the world_update tool — so extraction of the
            // structured (entity/attribute/value) or (from/relation/to) write is
            // done by the brain, but it can write nothing but user.world.* and can
            // fire no consequential action. world_update is in friday's allowlist
            // (a world-update-capable specialist); we re-home so isolation holds.
            let owner = agents
                .owner_of("world_update")
                .filter(|a| a.may_use("world_update"))
                .unwrap_or(prime);
            emit_agent_active(owner);
            if !cloud_reachable {
                // Honest offline degrade: record nothing we can't structure, never
                // fabricate a write. The normal pipeline isn't a better answer for
                // a world write, so we own the turn with a clear note.
                return Some(RouteOutcome {
                    routed_to: "local",
                    response: "I can note that into the world model once the cloud uplink is back, sir — I won't record a half-understood fact offline.".to_string(),
                    agent: owner.name.clone(),
                    namespace: owner.namespace.clone(),
                    spoken: None,
                });
            }
            let directive = format!(
                "Fold this stated fact into the shared world model using the world_update tool, \
                 then confirm what you recorded in one line: {text}"
            );
            let world_context = anthropic::grounded_world_live(text, memory).await;
            let only_world_update = vec!["world_update".to_string()];
            let agent_persona =
                anthropic::agent_persona_text(&owner.name, owner.is_orchestrator());
            match anthropic::complete_with_tools(
                cloud_model_for_world_update(),
                512,
                &directive,
                &[],
                &[],
                memory,
                &only_world_update,
                &owner.namespace,
                agent_persona.as_deref(),
                &world_context,
                // A focused world_update directive — no personalization grounding
                // needed; pass an empty summary.
                "",
                true, // the user's own stated fact — trusted (and only world_update is offered)
            )
            .await
            {
                Ok(response) => Some(RouteOutcome {
                    routed_to: "cloud",
                    response,
                    agent: owner.name.clone(),
                    namespace: owner.namespace.clone(),
                    spoken: None,
                }),
                Err(e) => {
                    warn!(error = %e, "world_update capability failed; degrading");
                    Some(RouteOutcome {
                        routed_to: "local",
                        response: "I couldn't fold that into the world model just now, sir.".to_string(),
                        agent: owner.name.clone(),
                        namespace: owner.namespace.clone(),
                        spoken: None,
                    })
                }
            }
        }

        Mode::Mission => {
            // FURY's bounded mission engine. run_fury_mission degrades to a
            // friendly offline line WITHOUT planning/dispatching when no key
            // resolves, so it is safe to call regardless of cloud state.
            // A mission the owner requested directly (Mission mode) — trusted.
            let response = anthropic::run_fury_mission(text, memory, true).await;
            let fury = agents.get("fury").unwrap_or(prime);
            emit_agent_active(fury);
            Some(RouteOutcome {
                routed_to: "local",
                response,
                agent: fury.name.clone(),
                namespace: fury.namespace.clone(),
                spoken: None,
            })
        }

        Mode::Standing => {
            // PROPOSE ONLY (Rail 2): park behind the confirmation gate + the
            // armed-by-default master switch (still per-action gated). Nothing is established here. The
            // proposing agent is the orchestrator; its allowlist is carried into
            // the pending so the spoken-yes replay re-checks it.
            emit_agent_active(prime);
            let (response, parked) =
                anthropic::propose_standing_mission(text, &prime.namespace, &prime.tools, memory).await;
            telemetry::emit(
                "local",
                "selector.standing_proposed",
                json!({"parked": parked}),
            );
            Some(RouteOutcome {
                routed_to: "local",
                response,
                agent: prime.name.clone(),
                namespace: prime.namespace.clone(),
                spoken: None,
            })
        }
    }
}

/// The model the constrained `world_update` capability loop uses — the fast model
/// is plenty for a single structured tool call; this keeps the world-write path
/// cheap. A bare const (not the full cfg plumbing) because this loop runs ONE tool
/// call to fold one fact, never deep reasoning.
fn cloud_model_for_world_update() -> &'static str {
    "claude-haiku-4-5"
}

/// Darwin-Prime delegation wrapper: pick the agent for this turn. Cloud
/// reachability gates the offline-survival route (hulk owns conversational
/// turns when the cloud is unreachable). `to_cloud` is whether THIS turn is
/// already heading to the cloud — if it is, the cloud is by definition
/// reachable for this turn, so the offline route never fires spuriously.
///
/// SMARTER ROUTING: the deterministic intent map + keyword cues
/// (`AgentRegistry::select`) stay the fast, authoritative FIRST PASS. A SEMANTIC
/// fallback (`select_with_fallback`) engages ONLY when that pass would otherwise
/// fall to the orchestrator default for a non-trivial conversation turn — it
/// then picks the best-matching specialist by lexical (BM25) similarity of the
/// utterance to each agent's role text via [`agents::LexicalAgentScorer`]. The
/// scorer is PURE (no inference/network call — honest keyword-semantic, the same
/// fallback recall uses when the on-device embedder is down) and degrades to the
/// orchestrator on a weak/tied/absent signal, so an ambiguous fitness question
/// reaches hercules while a blank or low-confidence turn stays with darwin —
/// never a worse outcome than the deterministic pass alone. This changes only
/// DELEGATION: the caller still enforces the chosen agent's tool allowlist
/// (`enforce_tool`) and the confirmation gate, so isolation/safety are untouched.
fn select_agent<'a>(
    agents: &'a AgentRegistry,
    intent: &str,
    text: &str,
    cloud_reachable: bool,
    to_cloud: bool,
) -> &'a Agent {
    let effective_cloud = cloud_reachable || to_cloud;
    agents.select_with_fallback(intent, text, effective_cloud, &crate::agents::LexicalAgentScorer)
}

/// Handle a RUNBOOK voice command (runbook.rs): PLAN (PURE, read-only render of the
/// typed DAG + which steps PARK) or RUN (execute — re-issue each step FRESH through the
/// live tool gate, one at a time). Gated by [runbook].enabled (OFF by default): with it
/// off both verbs report the subsystem is off and do NOTHING.
///
/// RUN grants NO authority. It mirrors the macro-replay dispatch: each step routes once
/// through the SAME `anthropic::execute_tool` + gate a live tool call takes, so a
/// consequential step PARKS FRESH for a spoken confirm (the process-global single-slot
/// pending, exactly as a live consequential call installs). It never batches or
/// pre-approves; a parked step produces nothing, so its `${ref}` consumer BLOCKS rather
/// than run on a fabricated value (the executor in runbook.rs enforces this). The named
/// runbook is loaded from its CONFINED on-device store; an unsound runbook is refused
/// whole. Emits the secret-free `runbook.plan` / `runbook.run` HUD frames.
async fn handle_runbook_command(
    cmd: crate::runbook::RunbookCommand,
    cfg: &Config,
    memory: &Memory,
    agent: &Agent,
    root: &Path,
) -> String {
    use crate::runbook::RunbookCommand;
    if !cfg.runbook.enabled {
        telemetry::emit("system", "runbook.blocked", json!({"reason": "disabled"}));
        return "Runbooks are off ([runbook].enabled = false), sir — I'm not planning or \
                running any."
            .to_string();
    }

    // Load + parse the named runbook from its confined `state/runbooks/*.runbook.toml`
    // store (load re-normalizes the name, so a path-y name is refused, never read).
    let rb = match crate::runbook::load(root, cmd.name()) {
        Ok(rb) => rb,
        Err(e) => return format!("{e}, sir."),
    };
    // Bound: one runbook may hold at most [runbook].max_steps steps — never an
    // unbounded DAG (mirrors the macro max_steps bound).
    if rb.steps.len() > cfg.runbook.max_steps {
        return format!(
            "Runbook \"{}\" has {} steps, over the {}-step bound, sir — I won't run an \
             unbounded DAG.",
            rb.name,
            rb.steps.len(),
            cfg.runbook.max_steps
        );
    }
    // The capability registry the checker resolves each step against; every TOOL's
    // privilege is pinned to the SAME confirm::is_consequential_tool source the gate
    // uses, so "will PARK" can never disagree with the gate.
    let reg = crate::runbook::Registry::builtin();

    match cmd {
        // PLAN: PURE, read-only. Render the whole DAG + which steps PARK; run nothing.
        RunbookCommand::Plan { .. } => {
            let plan = crate::runbook::plan(&rb, &reg);
            crate::runbook::emit_plan(&plan);
            let errs = plan
                .diagnostics
                .iter()
                .filter(|d| d.severity == crate::runbook::Severity::Error)
                .count();
            let mut out = format!(
                "Runbook \"{}\", sir: {} step{}, {} will park for a fresh spoken confirm.",
                plan.name,
                plan.steps.len(),
                if plan.steps.len() == 1 { "" } else { "s" },
                plan.park_count,
            );
            if plan.is_runnable() {
                out.push_str(" It type-checks and is ready to run.");
            } else {
                out.push_str(&format!(
                    " It has {errs} error{} and will NOT run until they're fixed.",
                    if errs == 1 { "" } else { "s" }
                ));
            }
            out
        }
        // RUN: execute — walk the DAG one step at a time through the live gate.
        RunbookCommand::Run { .. } => {
            // The LIVE router seam: route each ResolvedStep through the SAME gated entry
            // point (`anthropic::execute_tool`) a live tool call uses. It routes EXACTLY
            // ONE step per call (runbook::run never batches), so a consequential step
            // re-hits the confirmation gate + master switch + voice-id + lockdown FRESH.
            // Runs under the orchestrator (darwin): its `["*"]` allowlist admits every
            // tool, and execute_tool re-checks every safety gate regardless.
            struct LiveRunbookRouter<'a> {
                memory: &'a Memory,
                allowed: Vec<String>,
                namespace: String,
            }
            impl crate::runbook::RunbookRouter for LiveRunbookRouter<'_> {
                fn route_step<'a>(
                    &'a self,
                    step: &'a crate::runbook::ResolvedStep,
                ) -> std::pin::Pin<
                    Box<dyn std::future::Future<Output = crate::runbook::StepResult> + Send + 'a>,
                > {
                    Box::pin(async move {
                        let input = serde_json::Value::Object(step.input.clone());
                        // `user_originated = false`: a runbook is an automated re-issue
                        // whose later steps may consume an earlier step's output, so the
                        // egress continuation guard treats an outward GET conservatively
                        // — the runbook can never do MORE than a tool continuation could.
                        let (out, is_error) = anthropic::execute_tool(
                            &step.uses,
                            &input,
                            self.memory,
                            &self.allowed,
                            &self.namespace,
                            false,
                            // context_trusted=false: a runbook step PARKS fresh even
                            // under an Always policy — preserving the runbook's
                            // never-pre-approved invariant (the user confirms each
                            // consequential step, one at a time).
                            false,
                        )
                        .await;
                        // A consequential step is NEVER mapped to a produced value (it
                        // parked / previewed / was refused); a benign step yields its
                        // output. This is the load-bearing no-authority mapping.
                        crate::runbook::classify_step_outcome(step.consequential, out, is_error)
                    })
                }
            }

            let live = LiveRunbookRouter {
                memory,
                allowed: agent.tools.clone(),
                namespace: agent.namespace.clone(),
            };
            let report = crate::runbook::run(&rb, &reg, &live).await;
            crate::runbook::emit_run(&report);

            if report.refused_unsound {
                let errs = crate::runbook::check(&rb, &reg)
                    .iter()
                    .filter(|d| d.severity == crate::runbook::Severity::Error)
                    .count();
                return format!(
                    "Runbook \"{}\" didn't type-check, sir — I refused to run it ({errs} \
                     error{}). Say \"plan the runbook {}\" and I'll show you what's wrong.",
                    rb.name,
                    if errs == 1 { "" } else { "s" },
                    rb.name,
                );
            }
            let count = |o: crate::runbook::RunOutcome| {
                report.steps.iter().filter(|s| s.outcome == o).count()
            };
            let parked = count(crate::runbook::RunOutcome::Parked);
            let mut out = format!(
                "Ran runbook \"{}\" ({} step{}), sir: {} done, {} parked for confirm, {} \
                 refused, {} blocked.",
                report.name,
                report.steps.len(),
                if report.steps.len() == 1 { "" } else { "s" },
                count(crate::runbook::RunOutcome::Done),
                parked,
                count(crate::runbook::RunOutcome::Refused),
                count(crate::runbook::RunOutcome::Blocked),
            );
            if parked > 0 {
                // Honest about the single-slot pending: only the most recent parked step
                // is awaiting your "yes" — each consequential step re-gates on its own.
                out.push_str(
                    " The most recent parked step is awaiting your \"yes\"; each \
                     consequential step re-gates fresh — none was pre-approved.",
                );
            }
            out
        }
    }
}

/// Handle a non-replay MACRO control command (#27): start/stop recording, list, or
/// forget. Gated by [macros].enabled (OFF by default): with it off every verb
/// reports the subsystem is off and changes NOTHING. Recording captures only the
/// utterance + intent (redacted at persist time), so a secret is never stored, and
/// it never changes a gate. Emits HUD telemetry. (Replay is driven by the turn loop
/// so it can re-route each step through the full gate-honoring pipeline.)
async fn handle_macro_command(
    cmd: crate::macros::MacroCommand,
    cfg: &Config,
    memory: &Memory,
) -> String {
    use crate::macros::MacroCommand;
    if !cfg.macros.enabled {
        telemetry::emit("system", "macro.blocked", json!({"reason": "disabled"}));
        // Make sure no stray recording lingers if the flag was turned off mid-session.
        crate::macros::clear_recording();
        return "Macros are off ([macros].enabled = false), sir — I'm not recording or replaying anything."
            .to_string();
    }
    match cmd {
        MacroCommand::StartRecording { name } => {
            crate::macros::start_recording(&name);
            telemetry::emit("system", "macro.recording_started", json!({"name": name}));
            format!(
                "Recording macro \"{name}\", sir. Carry on with your commands — they'll still run normally; \
                 say 'stop recording' when you're done."
            )
        }
        MacroCommand::StopRecording => {
            let Some((name, steps)) = crate::macros::stop_recording() else {
                return "I wasn't recording a macro, sir.".to_string();
            };
            if steps.is_empty() {
                return format!("Stopped recording — \"{name}\" had no commands, so I saved nothing.");
            }
            match crate::macros::record(
                memory,
                cfg.macros.retention,
                cfg.macros.max_steps,
                &name,
                &steps,
            )
            .await
            {
                Ok(m) => {
                    telemetry::emit(
                        "system",
                        "macro.recorded",
                        json!({"name": m.name, "steps": m.steps.len()}),
                    );
                    format!(
                        "Saved macro \"{}\" with {} step{}. Say 'replay macro {}' to run it — each step \
                         re-runs fresh, and any consequential one still asks first.",
                        m.name,
                        m.steps.len(),
                        if m.steps.len() == 1 { "" } else { "s" },
                        m.name,
                    )
                }
                Err(e) => format!("I couldn't save that macro: {e}"),
            }
        }
        MacroCommand::List => {
            let macros = match crate::macros::list(memory).await {
                Ok(m) => m,
                Err(e) => {
                    warn!(error = %e, "macro list failed");
                    return "I couldn't read your macros just now, sir.".to_string();
                }
            };
            if macros.is_empty() {
                return "You have no saved macros, sir.".to_string();
            }
            let mut out = String::from("Your macros:\n");
            for m in &macros {
                out.push_str(&format!("- \"{}\" ({} steps)\n", m.name, m.steps.len()));
            }
            out.push_str("Replay one with 'replay macro <name>'; each step re-runs through the gate fresh.");
            out
        }
        MacroCommand::Forget { name } => match crate::macros::forget(memory, &name).await {
            Ok(true) => {
                telemetry::emit("system", "macro.forgotten", json!({"name": name}));
                format!("Forgot macro \"{name}\", sir.")
            }
            Ok(false) => format!("I have no macro called \"{name}\" to forget."),
            Err(e) => format!("I couldn't forget that macro: {e}"),
        },
        // Replay is handled by the turn loop (it re-classifies + re-routes each
        // step). Reaching here would be a logic error; report honestly rather than
        // silently doing nothing.
        MacroCommand::Replay { .. } => {
            "Replay is handled live, sir — say it again and I'll run it.".to_string()
        }
    }
}

/// ONE-WORD UNDO (F2). Status answers from the journal and runs nothing. UndoLast
/// prepares the LAST executed action's inverse (never silently an older one) and
/// hands it to `anthropic::execute_tool` — the same entry point a live tool call
/// uses — under the SAME agent + allowlist snapshot that executed the forward
/// action. A gated inverse therefore parks for its own fresh spoken "confirm"; a
/// reversible-by-design inverse (standing_cancel) runs directly, exactly as if
/// spoken. Whether the park actually happened is read back from the pending slot
/// (never assumed), and a direct execution is only claimed as "Undone" when the
/// inverse is ungated or the master switch was on — a master-off dry-run preview
/// is relayed as the preview it is.
async fn handle_undo_command(cmd: crate::journal::UndoCommand, memory: &Memory) -> String {
    use crate::journal::{UndoCommand, UndoPrep};
    match cmd {
        UndoCommand::Status => crate::journal::status_line(),
        UndoCommand::UndoLast => match crate::journal::prepare_undo() {
            UndoPrep::Nothing => {
                "Nothing consequential has executed this session, so there's nothing to undo."
                    .to_string()
            }
            UndoPrep::AlreadyUndone => {
                "The last consequential action was already undone.".to_string()
            }
            UndoPrep::Irreversible { why } => {
                format!("I can't undo the last action — {why}.")
            }
            UndoPrep::Ready { seq, agent, tool, input, allowed, note, pending_id } => {
                telemetry::emit(
                    "system",
                    "undo.armed",
                    json!({"tool": tool, "agent": agent, "seq": seq}),
                );
                let gate_before = crate::integrations::consequential_allowed();
                // "undo that" is a DIRECT, user-present interactive command, so it is
                // user_originated=true AND context_trusted=true — the derived inverse
                // is treated exactly like a live utterance's tool call.
                let (outcome, is_error) =
                    anthropic::execute_tool(&tool, &input, memory, &allowed, &agent, true, true).await;
                // Read back whether the inverse is now the parked confirmation —
                // never assumed from the outcome text.
                let parked = crate::confirm::peek_pending(Instant::now())
                    .is_some_and(|p| p.id == pending_id);
                // "It ran" requires: no transport error, nothing parked, the
                // master gate on across the call for gated tools (both-sides
                // read — a racing flip degrades to relaying the outcome without
                // an undo claim), AND the outcome text confirming the inverse
                // took effect (standing_cancel reports a miss/failure as
                // friendly Ok prose — never claim "Undone." over a miss).
                let executed_directly = !is_error
                    && !parked
                    && (!crate::journal::master_gated(&tool)
                        || (gate_before && crate::integrations::consequential_allowed()))
                    && crate::journal::inverse_confirmed(&tool, &outcome);
                if executed_directly && !crate::journal::master_gated(&tool) {
                    // An ungated reversible inverse ran immediately (a gated one
                    // is journaled + marked by the replay chokepoint instead).
                    crate::journal::mark_undone(seq);
                }
                crate::journal::compose_undo_response(
                    &outcome,
                    is_error,
                    parked,
                    executed_directly,
                    &note,
                )
            }
        },
    }
}

/// COMPOSE-MUSIC VOICE INTENT (Phase-2 flagship "compose an 8-bit happy
/// birthday"). Returns the extracted song PROMPT when the utterance is an
/// explicit request to CREATE music, else None.
///
/// CONSERVATIVELY ANCHORED so it never trips on ordinary speech. A match needs
/// BOTH a music-CREATION verb AND a musical anchor:
///   * `compose` is inherently musical → it alone anchors (the flagship
///     "compose an 8-bit happy birthday" carries no "song" noun).
///   * the broader verbs `make` / `write` / `generate` / `produce` and the
///     phrasings `play me` / `make me` REQUIRE an explicit music OBJECT noun
///     (song / track / tune / beat / jingle / melody / riff) so "make me a
///     sandwich" and "write me an email" are NOT music.
///     "play some jazz" (no creation verb) and "what's the time" therefore return
///     None — only an explicit creation request routes to Jerome.
///
/// The returned String is the cleaned PROMPT: the verb/object/filler stripped
/// from the front and an "about/of" tail unwrapped, so "compose a song about
/// the rain" → "the rain" and "compose an 8-bit happy birthday" → "an 8-bit
/// happy birthday". An empty residue (e.g. a bare "compose a song") falls back
/// to a generic prompt so the op still has something to compose. Pure +
/// unit-tested.
pub fn classify_music_intent(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    let lower = lower.trim();

    const OBJECTS: &[&str] = &["song", "track", "tune", "beat", "jingle", "melody", "riff"];
    let has_object = OBJECTS.iter().any(|o| lower.contains(o));

    // The creation verb must appear as a leading/standalone word, not buried in a
    // longer token. `compose` anchors on its own (inherently musical); the broader
    // verbs need a music object noun present so non-music "make/write/generate"
    // requests are excluded.
    let has_word = |w: &str| {
        lower == w
            || lower.starts_with(&format!("{w} "))
            || lower.contains(&format!(" {w} "))
    };

    let compose_verb = has_word("compose");
    let broad_verb = ["make", "write", "generate", "produce"]
        .iter()
        .any(|v| has_word(v));
    // "play me a tune" / "play me a beat" is a creation-ish ask ONLY with an object;
    // a bare "play some jazz" (no object noun, no creation verb) must NOT match.
    let play_me = lower.contains("play me");

    let is_music = compose_verb || ((broad_verb || play_me) && has_object);
    if !is_music {
        return None;
    }

    Some(extract_music_prompt(lower))
}

/// Strip the creation verb / object / leading filler from a matched music
/// utterance and unwrap an "about/of" tail, yielding the song PROMPT. A bare
/// request with nothing left to describe falls back to a generic prompt so the
/// op always has a non-empty thing to compose. Pure helper for
/// [`classify_music_intent`].
fn extract_music_prompt(lower: &str) -> String {
    let mut s = lower.to_string();

    // Drop a leading polite/address preamble so "darwin, compose ..." reduces to
    // the request before we strip the verb.
    for prefix in ["darwin", "hey darwin", "ok darwin", "please"] {
        let p = format!("{prefix},");
        if let Some(rest) = s.strip_prefix(&p) {
            s = rest.trim().to_string();
        }
        if let Some(rest) = s.strip_prefix(&format!("{prefix} ")) {
            s = rest.trim().to_string();
        }
    }

    // Strip the leading creation verb (+ a "me" indirect object).
    for verb in ["compose", "make", "write", "generate", "produce", "play"] {
        for lead in [format!("{verb} me "), format!("{verb} ")] {
            if let Some(rest) = s.strip_prefix(&lead) {
                s = rest.trim().to_string();
                break;
            }
        }
    }

    // Drop a leading article.
    for art in ["a ", "an ", "the ", "some "] {
        if let Some(rest) = s.strip_prefix(art) {
            s = rest.trim().to_string();
            break;
        }
    }

    // Strip a leading music object noun (+ trailing article), so
    // "song about the rain" -> "about the rain".
    const OBJECTS: &[&str] = &["song", "track", "tune", "beat", "jingle", "melody", "riff"];
    for obj in OBJECTS {
        // Exact match: the residual IS just the object noun ("compose a song").
        if s == *obj {
            s = String::new();
            break;
        }
        // Otherwise strip only a BOUNDARY-anchored lead ("song ..."), never a bare
        // prefix: a bare `obj` prefix strips the object noun even when it is merely
        // the start of a longer word ("beatles" -> "les"), corrupting the prompt.
        if let Some(rest) = s.strip_prefix(&format!("{obj} ")) {
            s = rest.trim().to_string();
            break;
        }
    }

    // Unwrap an "about/of" tail: "... about the rain" -> "the rain".
    for joiner in ["about ", "of ", "for ", "that goes "] {
        if let Some(idx) = s.find(joiner) {
            s = s[idx + joiner.len()..].trim().to_string();
            break;
        }
    }

    let s = s.trim().trim_end_matches(['.', '!', '?']).trim();
    if s.is_empty() {
        // A bare "compose a song" — nothing described; give the op a usable prompt
        // rather than an empty one.
        "a short, pleasant instrumental piece".to_string()
    } else {
        s.to_string()
    }
}

/// Announce the handling agent so the HUD highlights it in the roster, shifts
/// the core color to its hue, and shows the active-agent chip. Emitted at
/// final selection (after any allowlist re-route) so the HUD always tracks the
/// agent that actually acts.
fn emit_agent_active(agent: &Agent) {
    telemetry::emit(
        "local",
        "agent.active",
        json!({"name": agent.name, "role": agent.role, "hue": agent.hue}),
    );
}

/// Resolve the agent that parked a confirmation from its memory namespace
/// ("agent.<name>") back to a live registry entry, for the HUD highlight and the
/// reply's voice/bookkeeping. Falls back to the orchestrator if the namespace no
/// longer maps to a roster agent (defensive — the parked action still replays).
fn agent_for_namespace<'a>(agents: &'a AgentRegistry, namespace: &str) -> &'a Agent {
    let name = namespace.strip_prefix("agent.").unwrap_or(namespace);
    agents.get(name).unwrap_or_else(|| agents.orchestrator())
}

/// A short, spoken-friendly phrase for a consequential tool, used only in the
/// "Cancelled. I won't <phrase>." acknowledgement. Generic fallback keeps it
/// honest for any tool not individually named.
fn action_phrase(tool: &str) -> &'static str {
    match tool {
        "gmail_send" => "send that email",
        "slack_post_message" => "post that Slack message",
        "x_post" => "post that tweet",
        "linkedin_post" => "publish that LinkedIn post",
        "github_open_pr" => "open that pull request",
        "github_comment_issue" => "post that comment",
        "gcal_create_event" => "create that event",
        "gdrive_upload_text" => "upload that file",
        "dume_control" => "make that change",
        "gads_pause_campaign" | "meta_pause_campaign" => "pause that campaign",
        "gads_enable_campaign" | "meta_resume_campaign" => "resume that campaign",
        "gads_set_budget" | "meta_set_budget" => "change that budget",
        _ => "go ahead with that",
    }
}

/// Enforce the active agent's tool allowlist for a local intent. The intent is
/// the tool name; if the agent may use it, it stays. Otherwise the turn is
/// handed to the tool's real owner (or the orchestrator when only it holds the
/// tool) and the new agent is announced — isolation: no agent ever acts
/// through another agent's exclusive tool. Returns the agent that will act.
fn enforce_tool<'a>(agents: &'a AgentRegistry, agent: &'a Agent, intent: &str) -> &'a Agent {
    if agent.may_use(intent) {
        return agent;
    }
    let owner = agents.owner_of(intent).unwrap_or_else(|| agents.orchestrator());
    info!(
        from = %agent.name,
        to = %owner.name,
        tool = intent,
        "tool outside agent allowlist; re-routing to the owning agent"
    );
    telemetry::emit(
        "local",
        "agent.reroute",
        json!({"from": agent.name, "to": owner.name, "tool": intent}),
    );
    emit_agent_active(owner);
    owner
}

/// Facts visible to one agent: its own namespace plus shared facts, meta.*
/// filtered. Failures degrade to an empty list (a busy DB must never kill a
/// reply) — same policy fetch_history uses for history.
async fn agent_facts(memory: &Memory, namespace: &str) -> Vec<(String, String)> {
    // THRESHOLD — GUEST MODE recall WITHHOLDING (WIRING POINT 2): a GUEST turn feeds
    // NO owner memory to the LOCAL prompt at all. The whole store is the owner's
    // personal data (the "shared" tier still holds the owner's user.* rows), so a
    // bystander reads none of it. Return an empty feed (fail-closed). Owner path:
    // byte-for-byte today's.
    if crate::threshold::is_guest_turn() {
        return Vec::new();
    }
    memory
        .agent_scoped_facts(namespace, FACTS_LIMIT)
        .await
        .unwrap_or_else(|e| {
            warn!(error = %e, namespace, "failed to load namespaced facts for prompt");
            Vec::new()
        })
}

/// Phrase a reply in persona via the local LLM, fed with recent exchanges,
/// the active agent's namespaced facts, and the handler's verified data. If
/// the inference server is down, speak the raw data itself — degraded but
/// honest, never canned personality and never silence. The generate op has no
/// per-agent persona override (only converse does), so this fallback speaks in
/// the base persona; recall is still namespaced so an agent never sees another
/// agent's private facts even on the degrade path.
///
/// `local_model` is the multi-resident LOCAL sub-choice (task #17): when the
/// converse path that fell back here had selected a warm local model, the same
/// model answers the degraded generate (so the fallback stays on the same brain).
/// `None` (the single-resident default + the cloud-degrade path) -> the base.
async fn generate_in_persona(
    text: &str,
    data: &str,
    memory: &Memory,
    infer: &mut InferenceClient,
    agent: &Agent,
    local_model: Option<&str>,
) -> String {
    let facts_kv = agent_facts(memory, &agent.namespace).await;
    let facts: Vec<String> = facts_kv
        .iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect();
    let history = fetch_history(memory).await;
    let data_opt = (!data.is_empty()).then_some(data);
    match infer
        .generate(text, GENERATE_MAX_TOKENS, &history, &facts, data_opt, local_model)
        .await
    {
        Ok(reply) => reply,
        Err(e) => {
            // error!: total local-LLM loss — exactly what self-heal watches.
            error!(error = %e, "local generate unavailable; falling back to raw data");
            telemetry::emit(
                "system",
                "inference.unavailable",
                json!({"op": "generate", "error": e.to_string()}),
            );
            if data.is_empty() {
                // Nothing factual to fall back on (conversation intent):
                // state the system condition rather than staying mute.
                "The local language model is not responding.".to_string()
            } else {
                data.to_string()
            }
        }
    }
}

async fn fetch_history(memory: &Memory) -> Vec<(String, String)> {
    // THRESHOLD — GUEST MODE: a GUEST turn's prompt carries NO conversation history.
    // The recent exchanges are the OWNER's private dialogue (from before the mic was
    // handed over); feeding them would let a bystander's turn be answered with — and
    // echo — the owner's prior conversation. Return an empty history (fail-closed).
    // Owner path: byte-for-byte today's.
    if crate::threshold::is_guest_turn() {
        return Vec::new();
    }
    memory
        .recent_exchanges(HISTORY_EXCHANGES)
        .await
        .unwrap_or_else(|e| {
            warn!(error = %e, "failed to load history for prompt");
            Vec::new()
        })
}

/// The most recent up-to-`n` DARWIN replies from oldest-first history, for the
/// cloud conversation anti-repeat `avoid` list. Pulls the DARWIN side of each
/// exchange, drops blanks, and keeps the last `n` (the freshest) — exactly the
/// wording a repeated greeting would otherwise echo. Pure, so the selection is
/// unit-testable. Empty history yields an empty list (the prompt is then left
/// untouched, which is correct for a first turn).
fn recent_replies(history: &[(String, String)], n: usize) -> Vec<String> {
    history
        .iter()
        .filter_map(|(_, darwin)| {
            let r = darwin.trim();
            (!r.is_empty()).then(|| r.to_string())
        })
        .rev()
        .take(n)
        .collect()
}

/// Local handlers gather data; they no longer write final prose. Live:
/// app.launch/app.control (open/quit via the fuzzy matcher, plus the
/// web-reroute belt-and-suspenders), web.open/web.search (open_url /
/// Google search via the classifier args), file.op (Spotlight search, plus
/// open-on-single-strong-match), system.query (real sysinfo stats),
/// conversation (context only), memory.store/recall. `args` is the
/// classifier's pass-through args object (Null on old servers). `agent` is the
/// active agent: memory.store writes under its namespace
/// ("<namespace>.note.<content-hash>", one key per distinct note — never a
/// clobbering fixed key) and memory.recall reads its namespaced view (own
/// namespace + shared facts), so each agent's notes stay isolated
/// (constellation namespacing, item 4).
/// THRESHOLD guest-mode fast-path admissibility. Returns `Some(category)` when a
/// GUEST turn's utterance would trigger a route() fast-path handler that reads the
/// owner's personal data or takes a consequential / owner-control action — each of
/// which BYPASSES the tool-loop + recall gates. Returns `None` for a guest-safe
/// turn (plain conversation / translation / non-personal status), which matches
/// none of these anchored classifiers and flows through to the already guest-gated
/// conversational path. ONLY consulted when a guest scope is installed.
///
/// PURE: every check is a side-effect-free classifier. NOTE it uses the pure
/// `policy::classify_policy_command` — NOT `handle_user_policy_text`, which APPLIES
/// the policy write — so testing admissibility never mutates state. Fail-closed:
/// any owner-data / consequential specialized path is refused; only genuinely
/// non-personal turns fall through. New fast paths added to `route()` must be
/// mirrored here.
fn guest_denied_fast_path(text: &str, cfg: &Config) -> Option<&'static str> {
    let now = chrono::Local::now();
    // -- Owner CONTROLS / CONSEQUENTIAL actions --------------------------------
    if crate::policy::classify_policy_command(text).is_some() {
        return Some("policy controls");
    }
    if crate::model_tier::classify_model_swap(text).is_some() {
        return Some("model controls");
    }
    if crate::prosody::parse_whisper_command(text).is_some() {
        return Some("voice-mode controls");
    }
    if crate::vault::classify_vault_command(text).is_some() {
        return Some("vault controls");
    }
    if crate::macros::classify_macro_command(text).is_some() {
        return Some("saved macros");
    }
    if crate::journal::classify_undo_command(text).is_some() {
        return Some("undo history");
    }
    if classify_music_intent(text).is_some() {
        return Some("music generation");
    }
    if generate_image_command(text).is_some() {
        return Some("image generation");
    }
    if silicon_canvas_command(text).is_some() || mark_forge_command(text).is_some() {
        return Some("design tools");
    }
    if nexus_command(text).is_some() {
        return Some("audio tools");
    }
    if vision_command(text).is_some() {
        return Some("vision tools");
    }
    if cfg.artifact.enabled && crate::artifact::classify_peek_intent(text) {
        return Some("artifacts");
    }
    if crate::chart::classify_chart_intent(text).is_some() {
        return Some("charts");
    }
    // -- Owner PERSONAL-DATA readers -------------------------------------------
    if crate::aperture::classify_aperture_intent(text, &now).is_some() {
        return Some("activity timeline");
    }
    if crate::screen_context::classify_screen_context_intent(text).is_some() {
        return Some("screen context");
    }
    if crate::pasteboard::classify_pasteboard_intent(text).is_some() {
        return Some("clipboard");
    }
    if crate::notebook::classify_notebook_intent(text).is_some() {
        return Some("notebooks");
    }
    if crate::report::classify_report_intent(text).is_some() {
        return Some("reports");
    }
    if crate::simulate::extract_hypothetical(text).is_some() {
        return Some("personal simulations");
    }
    if crate::lifelog::classify_lifelog_intent(text).is_some() {
        return Some("lifelog");
    }
    if crate::rewind::classify_rewind_intent(text, now.fixed_offset()).is_some() {
        return Some("session rewind");
    }
    if crate::explain::classify_explain_intent(text).is_some() {
        return Some("decision traces");
    }
    if crate::user_model::classify_mirror_intent(text).is_some() {
        return Some("personal profile");
    }
    if describe_command(text).is_some() {
        return Some("vision describe");
    }
    // The agent ROSTER / roll-call — route() fast paths that expose the owner's
    // configured agent constellation. Not owner-personal data, but the guest
    // allowlist is deny-by-default for EVERY route() fast path, so refuse these too
    // (a guest gets conversation / translation / non-personal status, nothing about
    // the owner's private setup).
    if crate::agents::is_roll_call(text) || crate::agents::is_agent_query(text) {
        return Some("agent roster");
    }
    None
}

async fn handle_local(
    intent: &str,
    args: &serde_json::Value,
    text: &str,
    memory: &Memory,
    app_registry: &Arc<AppRegistry>,
    agent: &Agent,
) -> HandlerOutput {
    // THRESHOLD — GUEST MODE fast-path gate (finding 3). handle_local is the
    // structured-intent FAST PATH — it BYPASSES the tool-loop + recall gates and can
    // READ owner memory (memory.recall), WRITE owner memory (memory.store),
    // launch/control apps (app.launch / app.control), open URLs (web.open), search
    // the web (web.search), touch files (file.op), or (re)build the owner's doc
    // index / knowledge graph. For a GUEST, DENY BY DEFAULT: allow ONLY genuinely
    // non-personal intents — plain conversation (falls through to the guest-gated
    // LLM path) and non-personal machine status — and refuse EVERYTHING else
    // (including any future intent) with an honest message, performing NO read and
    // NO write. On the owner path (no scope installed) this is a no-op and handling
    // is byte-for-byte today's.
    if crate::threshold::is_guest_turn() && !matches!(intent, "conversation" | "system.query") {
        telemetry::emit(
            "local",
            "threshold.local_refused",
            json!({"intent": intent, "agent": agent.name}),
        );
        return HandlerOutput {
            data: format!(
                "I can't do that in guest mode — '{intent}' would read or change the owner's \
                 data or act on their machine, and a guest is limited to conversation, \
                 translation, and non-personal status. The owner can do it."
            ),
            // Spoken verbatim, NOT sent to the LLM — no owner context is assembled.
            llm_voice: false,
        };
    }
    if let Err(e) = memory.record_event("local", intent, text).await {
        warn!(error = %e, "failed to record local intent event");
    }
    telemetry::emit(
        "local",
        "intent.handled",
        json!({"intent": intent, "text": text, "agent": agent.name}),
    );

    let data = match intent {
        "app.launch" | "app.control" => handle_app_intent(intent, text, args, app_registry).await,
        "web.open" => handle_web_open(text, args).await,
        "web.search" => handle_web_search(text, args).await,
        "file.op" => handle_file_intent(text).await,
        "docsearch.index" => handle_docsearch_index().await,
        "docsearch.forget" => handle_docsearch_forget().await,
        "docsearch.build_graph" | "knowledge.build" => {
            handle_build_knowledge_graph(memory).await
        }
        "system.query" => actions::system_status_data().await,
        "memory.store" => {
            // Namespaced, CONTENT-KEYED note (e.g. "agent.pepper.note.3fa9…"):
            // one key per distinct note text, so storing a second note never
            // silently CLOBBERS the first (the old fixed "<ns>.note" key kept
            // only the latest note), while re-storing identical text stays a
            // no-growth upsert. Recall is prefix-scoped (agent_scoped_facts
            // LIKE '<ns>.%'), so suffixed keys surface unchanged.
            // upsert_user_fact keeps the meta.* guard in front of every
            // model/agent-driven write.
            let suffix = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                text.hash(&mut h);
                h.finish()
            };
            let key = format!("{}.note.{suffix:016x}", agent.namespace);
            match memory.upsert_user_fact(&key, text).await {
                Ok(()) => format!("Stored fact: {text}"),
                Err(e) => {
                    warn!(error = %e, "failed to store fact");
                    format!("Failed to store the fact (database error: {e})")
                }
            }
        }
        "memory.recall" => match memory.agent_scoped_facts(&agent.namespace, 50).await {
            Ok(facts) => {
                if facts.is_empty() {
                    "No facts stored yet".to_string()
                } else {
                    let lines: Vec<String> =
                        facts.iter().map(|(k, v)| format!("{k}: {v}")).collect();
                    format!("Stored facts:\n{}", lines.join("\n"))
                }
            }
            Err(e) => {
                warn!(error = %e, "failed to recall facts");
                format!("Failed to read stored facts (database error: {e})")
            }
        },
        "conversation" => String::new(),
        other => {
            info!(intent = other, text, "unknown intent; no local handler");
            format!("No local handler exists for intent '{other}'")
        }
    };
    HandlerOutput {
        data,
        llm_voice: true,
    }
}

/// What an app.launch/app.control utterance actually asks for. Decided
/// before any process is spawned, so the inverse-of-command bug ("quit
/// Safari" launching Safari) and the dead web bug ("open apple.com" opening
/// nothing) cannot recur regardless of classifier output.
#[derive(Debug, PartialEq)]
enum AppRequest {
    Launch,
    Quit,
    Web,
}

/// Pure decision: quit-class verbs first (a quit must NEVER feed the
/// launcher — audit fix), then the app.launch->web reroute (belt and
/// suspenders against the classifier missing web.open), else launch. The
/// web probe is the extracted remainder, or the whole utterance when no
/// trigger verb was found (the launcher would fall back to the whole
/// utterance too).
fn classify_app_request(intent: &str, text: &str, extracted: &str) -> AppRequest {
    if wants_quit(text) {
        return AppRequest::Quit;
    }
    let probe = if extracted.is_empty() { text } else { extracted };
    if intent == "app.launch" && suggests_web(probe) {
        return AppRequest::Web;
    }
    AppRequest::Launch
}

/// Quit-class verbs anywhere in the utterance.
fn wants_quit(text: &str) -> bool {
    split_words(text)
        .iter()
        .any(|w| matches!(w.as_str(), "quit" | "close" | "exit" | "stop" | "kill"))
}

/// Web markers in an app-launch remainder: website/web/site as words, or a
/// .com/.org/http fragment inside any token ("apple.com", "https://x.org").
fn suggests_web(remainder: &str) -> bool {
    split_words(remainder).iter().any(|w| {
        matches!(w.as_str(), "website" | "web" | "site")
            || w.contains(".com")
            || w.contains(".org")
            || w.contains("http")
    })
}

/// app.launch/app.control: extract the app name from the utterance (words
/// after a trigger verb minus stopwords), decide what kind of request this
/// is, and dispatch. A registered MICRO-APP is resolved FIRST (before the
/// macOS launcher): "open global scan" starts the global-scan micro-app,
/// "close global scan" stops it. Otherwise launch/quit hand the name to the
/// fuzzy macOS-app matcher with the whole utterance as fallback; web requests
/// reroute to the web.open handler. Outcomes become converse data.
async fn handle_app_intent(
    intent: &str,
    text: &str,
    args: &serde_json::Value,
    app_registry: &Arc<AppRegistry>,
) -> String {
    let extracted = extract_app_name(text);
    let request = classify_app_request(intent, text, &extracted);

    // Micro-app resolution comes BEFORE the macOS open/quit path. The probe is
    // the extracted remainder; only Launch/Quit can target a micro-app (a Web
    // request is never an app name). A miss falls straight through to the
    // existing macOS launcher — micro-apps never shadow a real application.
    if matches!(request, AppRequest::Launch | AppRequest::Quit) && !extracted.is_empty() {
        if let Some(app) = app_registry.resolve_name(&extracted).await {
            return match request {
                AppRequest::Launch => match apps::start(app_registry, &app).await {
                    Ok(()) => {
                        info!(app = %app, "micro-app launch requested");
                        telemetry::emit(
                            "system",
                            "action.executed",
                            json!({"tool": "start_app", "outcome": format!("Starting the {app} panel.")}),
                        );
                        format!("Bringing up the {app} panel now, sir.")
                    }
                    Err(e) => {
                        warn!(app = %app, error = %e, "micro-app launch failed");
                        format!("The {app} panel could not be started: {e}")
                    }
                },
                AppRequest::Quit => match apps::stop(app_registry, &app).await {
                    Ok(()) => {
                        info!(app = %app, "micro-app stop requested");
                        telemetry::emit(
                            "system",
                            "action.executed",
                            json!({"tool": "stop_app", "outcome": format!("Stopping the {app} panel.")}),
                        );
                        format!("Closing the {app} panel, sir.")
                    }
                    Err(e) => {
                        warn!(app = %app, error = %e, "micro-app stop failed");
                        format!("The {app} panel could not be stopped: {e}")
                    }
                },
                AppRequest::Web => unreachable!("guarded by the matches! above"),
            };
        }
    }

    match request {
        AppRequest::Web => handle_web_open(text, args).await,
        AppRequest::Quit => match actions::quit_app_with_fallback(&extracted, text).await {
            Ok(outcome) => {
                info!(outcome, "app quit completed");
                telemetry::emit(
                    "system",
                    "action.executed",
                    json!({"tool": "quit_app", "outcome": first_chars(&outcome, 120)}),
                );
                outcome
            }
            Err(e) => {
                warn!(error = %e, "app quit failed");
                format!("The app could not be quit: {e}")
            }
        },
        AppRequest::Launch => match actions::open_app_with_fallback(&extracted, text).await {
            Ok(outcome) => {
                info!(outcome, "app action completed");
                telemetry::emit(
                    "system",
                    "action.executed",
                    json!({"tool": "open_app", "outcome": first_chars(&outcome, 120)}),
                );
                outcome
            }
            Err(e) => {
                warn!(error = %e, "app action failed");
                format!("The app could not be opened: {e}")
            }
        },
    }
}

/// Execute a Silicon Canvas voice command: LAUNCH the app, or forward a
/// STRUCTURED op line to the already-running app. Returns the verified outcome
/// as converse data (llm_voice) so the active agent's persona phrases the
/// confirmation, mirroring the app-launch path. The daemon forwards only the
/// op string built by [`silicon_canvas_command`]; it never interprets the op
/// body and the app never parses natural language (SPEC §6).
///
/// An op aimed at a NOT-running Silicon Canvas reports that plainly (apps::
/// send_op errors) rather than silently launching it — launching mid-trace
/// would lose the user's selection, so "trace this net" before "open silicon
/// canvas" should tell the user to open it first.
async fn handle_silicon_canvas(
    cmd: SiliconCanvasCommand,
    app_registry: &Arc<AppRegistry>,
) -> HandlerOutput {
    let data = match cmd {
        SiliconCanvasCommand::Launch => {
            match apps::start(app_registry, SILICON_CANVAS_APP).await {
                Ok(()) => {
                    info!(app = SILICON_CANVAS_APP, "silicon canvas launch requested");
                    telemetry::emit(
                        "system",
                        "action.executed",
                        json!({"tool": "start_app", "outcome": "Starting the Silicon Canvas panel."}),
                    );
                    "Bringing up Silicon Canvas now, sir.".to_string()
                }
                Err(e) => {
                    warn!(app = SILICON_CANVAS_APP, error = %e, "silicon canvas launch failed");
                    format!("Silicon Canvas could not be started: {e}")
                }
            }
        }
        SiliconCanvasCommand::Op(op_line) => {
            match apps::send_op(app_registry, SILICON_CANVAS_APP, &op_line).await {
                Ok(()) => {
                    info!(app = SILICON_CANVAS_APP, op = %op_line, "forwarded silicon canvas op");
                    telemetry::emit(
                        "system",
                        "app.op_forwarded",
                        json!({"name": SILICON_CANVAS_APP, "op": op_line}),
                    );
                    "Done, sir.".to_string()
                }
                Err(e) => {
                    warn!(app = SILICON_CANVAS_APP, op = %op_line, error = %e, "silicon canvas op forward failed");
                    format!("I couldn't reach Silicon Canvas: {e}. Open it first, sir.")
                }
            }
        }
    };
    HandlerOutput {
        data,
        llm_voice: true,
    }
}

/// Execute a Vision voice command: LAUNCH the Vision micro-app, or forward a
/// STRUCTURED op line to the already-running app. Mirrors [`handle_silicon_canvas`]
/// exactly — verified outcome as converse data (llm_voice) so the active agent's
/// persona phrases the confirmation; the daemon forwards only the op string built
/// by [`vision_command`] and never interprets the op body; the app never parses
/// natural language.
///
/// DEFENSIVE-ONLY framing in the spoken confirmations: capture is of the user's
/// OWN devices and is GATED BY macOS TCC (a runtime consent prompt the daemon
/// cannot grant) — so a watch op that the app cannot honor without consent still
/// returns cleanly here; the on-device consent is the app's to request.
///
/// An op aimed at a NOT-running Vision reports that plainly (apps::send_op
/// errors) rather than silently launching it.
async fn handle_vision(cmd: VisionCommand, app_registry: &Arc<AppRegistry>) -> HandlerOutput {
    let data = match cmd {
        VisionCommand::Launch => match apps::start(app_registry, VISION_APP).await {
            Ok(()) => {
                info!(app = VISION_APP, "vision launch requested");
                telemetry::emit(
                    "system",
                    "action.executed",
                    json!({"tool": "start_app", "outcome": "Starting the Vision panel."}),
                );
                "Bringing up Vision now, sir. I'll need your camera or screen consent on-device.".to_string()
            }
            Err(e) => {
                warn!(app = VISION_APP, error = %e, "vision launch failed");
                format!("Vision could not be started: {e}")
            }
        },
        VisionCommand::Op(op_line) => match apps::send_op(app_registry, VISION_APP, &op_line).await {
            Ok(()) => {
                info!(app = VISION_APP, op = %op_line, "forwarded vision op");
                telemetry::emit(
                    "system",
                    "app.op_forwarded",
                    json!({"name": VISION_APP, "op": op_line}),
                );
                // The read.screen op's recognized text arrives ASYNCHRONOUSLY on
                // the vision.screen telemetry event (relayed to the HUD), NEVER in
                // this synchronous reply — so the SENSITIVE on-screen text never
                // rides the persisted response. The spoken acknowledgment is
                // deliberately content-free (no recognized text) and honest about
                // the on-device TCC gate. PRIVACY: the recognized text is kept
                // transient by `is_screen_read` gating in main.rs.
                if op_line.contains("read.screen") {
                    "Reading your screen now, sir — the readout will appear on the Vision panel. I'll need your Screen Recording consent on-device.".to_string()
                } else if op_line.contains("read.handwriting") {
                    // #28: content-free acknowledgment — the recognized handwriting
                    // arrives async on the vision.screen telemetry, never in this
                    // persisted reply. Honest about the TCC device gate + that
                    // recognition quality is device-dependent.
                    "Reading the handwriting now, sir — the transcription will appear on the Vision panel. I'll need your camera consent on-device, and how well it reads depends on the writing.".to_string()
                } else if op_line.contains("scan.document") {
                    // #29: content-free acknowledgment — the scanned page text
                    // arrives async on the vision.screen telemetry. Honest about the
                    // TCC camera gate + that no page means an honest empty (never a
                    // fabricated document).
                    "Scanning the document now, sir — the text will appear on the Vision panel. I'll need your camera consent on-device; if I don't find a page I'll say so rather than guess.".to_string()
                } else {
                    "Done, sir.".to_string()
                }
            }
            Err(e) => {
                warn!(app = VISION_APP, op = %op_line, error = %e, "vision op forward failed");
                format!("I couldn't reach Vision: {e}. Open it first, sir.")
            }
        },
    };
    HandlerOutput {
        data,
        llm_voice: true,
    }
}

/// Execute a LUMEN (#45) voice command — the screen-narration + hands-free
/// voice-navigation dispatch. Two arms, both READ-ONLY except the actuation, which
/// runs entirely through the UNCHANGED capstone:
///
///   * READ — forward the READ-ONLY Vision `read.screen` locate (the SAME op the
///     OCR read uses; DEVICE-gated by Screen-Recording TCC), then speak a
///     content-free acknowledgment. The recognized control labels arrive
///     ASYNCHRONOUSLY on the `vision.screen` telemetry event (relayed to the HUD,
///     never in this synchronous reply — kept transient by `is_screen_read`); at
///     integration that relay also parses them into Lumen's remembered readout
///     (`lumen::remember_readout`) + narrates them via `lumen::narrate_controls`,
///     so a follow-up "click the third" selects over exactly what was read.
///
///   * ACT — select the ONE named target over the REMEMBERED controls
///     (`lumen::resolve_voice_action`), or REFUSE honestly (a miss / ambiguity /
///     out-of-range / no-location / nothing-read-yet never becomes a wrong click).
///     A resolved target is handed to `anthropic::execute_tool("ui_actuate", …)` —
///     the SAME entry a live tool call uses — under the ui_actuate-OWNING agent's
///     allowlist. The capstone still PARKS it per action for a spoken yes (master
///     switch + voice-id + `!lockdown` + the pure planner); Lumen NEVER actuates,
///     gates, or batches. ONE resolved phrase = ONE request = (after the capstone's
///     own gate) at most ONE actuation. The park prompt / refusal is spoken
///     VERBATIM (llm_voice=false) so the exact "say confirm" wording survives.
///
/// The `actor` is the ui_actuate-owning specialist (re-pinned by the caller); the
/// ACT arm runs execute_tool under ITS allowlist + namespace, exactly like a
/// mission sub-task runs as its owning specialist.
async fn handle_lumen(
    cmd: LumenCommand,
    memory: &Memory,
    app_registry: &Arc<AppRegistry>,
    actor: &Agent,
) -> HandlerOutput {
    match cmd {
        LumenCommand::Read => {
            // READ-ONLY: forward the existing Vision `read.screen` locate (device-
            // gated OCR). The readout is relayed async (HUD) + remembered at
            // integration; here we only forward + acknowledge (content-free).
            let op = op_read_screen(None);
            let data = match apps::send_op(app_registry, VISION_APP, &op).await {
                Ok(()) => {
                    telemetry::emit(
                        "system",
                        "lumen.read",
                        json!({"narrate": crate::lumen::is_narrating()}),
                    );
                    "Reading your screen now, sir — I'll read out the on-screen controls so you can \
                     tell me which to click. I'll need your Screen Recording consent on-device."
                        .to_string()
                }
                Err(e) => {
                    warn!(app = VISION_APP, error = %e, "lumen read.screen forward failed");
                    format!("I couldn't reach Vision to read the screen: {e}. Open it first, sir.")
                }
            };
            HandlerOutput { data, llm_voice: true }
        }
        LumenCommand::Act(phrase) => {
            // Select the ONE target over the REMEMBERED controls (or refuse). No
            // OCR/AX runs here — the list was captured by a prior read.
            let controls = crate::lumen::snapshot_controls();
            let resolved = crate::lumen::resolve_voice_action(&phrase, &controls);
            // SECRET-FREE telemetry (control count + selected + refusal class only).
            telemetry::emit(
                "system",
                "lumen.action",
                crate::lumen::resolved_action_frame(controls.len(), &resolved),
            );
            let data = match resolved {
                Ok(req) => {
                    // Hand the request to the UNCHANGED capstone via the SAME entry a
                    // live tool call uses — it plans + gates + PARKS per action; Lumen
                    // adds nothing to the gate. `confirm` is omitted (never self-set).
                    let input = ui_actuate_input(&req);
                    let (outcome, _is_error) = anthropic::execute_tool(
                        "ui_actuate",
                        &input,
                        memory,
                        &actor.tools,
                        &actor.namespace,
                        true,
                        // context_trusted=true: a live, attended voice actuation
                        // (ui_actuate is NEVER_AUTO_APPROVE regardless, so it parks).
                        true,
                    )
                    .await;
                    outcome
                }
                // A miss / ambiguity / out-of-range / no-location / nothing-read-yet
                // is an HONEST spoken refusal — sentence-cased for the verbatim path.
                Err(e) => capitalize_first(&e.reason()),
            };
            // Spoken VERBATIM: the park prompt's exact "say confirm" wording (and the
            // precise refusal) must not be re-paraphrased by the persona converse.
            HandlerOutput { data, llm_voice: false }
        }
    }
}

/// Capitalize the first alphabetic character of a spoken line (the SelectError
/// reasons are authored mid-sentence, but the Lumen ACT arm speaks them VERBATIM,
/// so they lead a sentence here). Pure; leaves everything else byte-identical.
fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Execute a Nexus voice command: LAUNCH the Nexus micro-app, or forward a
/// STRUCTURED op line to the already-running app. Mirrors [`handle_silicon_canvas`]
/// and [`handle_vision`] exactly — verified outcome as converse data (llm_voice)
/// so the active agent's persona phrases the confirmation; the daemon forwards
/// only the op string built by [`nexus_command`] and never interprets the op
/// body; the app never parses natural language (SPEC §6).
///
/// An op aimed at a NOT-running Nexus reports that plainly (apps::send_op errors)
/// rather than silently launching it — launching mid-session would reset the
/// matrix, so "mute the mic" before "open nexus" should tell the user to open it
/// first. The realtime CoreAudio path itself is DEVICE-GATED and never opened
/// headlessly; forwarding an op is a control-plane message the daemon can always
/// send to a running control plane regardless of whether a device is bound.
async fn handle_nexus(cmd: NexusCommand, app_registry: &Arc<AppRegistry>) -> HandlerOutput {
    let data = match cmd {
        NexusCommand::Launch => match apps::start(app_registry, NEXUS_APP).await {
            Ok(()) => {
                info!(app = NEXUS_APP, "nexus launch requested");
                telemetry::emit(
                    "system",
                    "action.executed",
                    json!({"tool": "start_app", "outcome": "Starting the Nexus panel."}),
                );
                "Bringing up Nexus now, sir.".to_string()
            }
            Err(e) => {
                warn!(app = NEXUS_APP, error = %e, "nexus launch failed");
                format!("Nexus could not be started: {e}")
            }
        },
        NexusCommand::Op(op_line) => match apps::send_op(app_registry, NEXUS_APP, &op_line).await {
            Ok(()) => {
                info!(app = NEXUS_APP, op = %op_line, "forwarded nexus op");
                telemetry::emit(
                    "system",
                    "app.op_forwarded",
                    json!({"name": NEXUS_APP, "op": op_line}),
                );
                "Done, sir.".to_string()
            }
            Err(e) => {
                warn!(app = NEXUS_APP, op = %op_line, error = %e, "nexus op forward failed");
                format!("I couldn't reach Nexus: {e}. Open it first, sir.")
            }
        },
    };
    HandlerOutput {
        data,
        llm_voice: true,
    }
}

/// Execute a Mark-Forge voice command: LAUNCH the Mark-Forge micro-app, or
/// forward a STRUCTURED op line to the already-running app. Mirrors
/// [`handle_silicon_canvas`] / [`handle_vision`] / [`handle_nexus`] exactly —
/// verified outcome as converse data (llm_voice) so the active agent's persona
/// phrases the confirmation; the daemon forwards only the op string built by
/// [`mark_forge_command`] and never interprets the op body; the app never parses
/// natural language (SPEC §7).
///
/// An op aimed at a NOT-running Mark-Forge reports that plainly (apps::send_op
/// errors) rather than silently launching it — launching mid-session would wipe
/// the bodies the user spawned, so "drop a box" before "open the physics sandbox"
/// should tell the user to open it first. The engine is CPU/f64 and headless; the
/// R3F render is DEVICE-GATED and never opened here — forwarding an op is a
/// control-plane message the daemon can always send to a running engine.
async fn handle_mark_forge(
    cmd: MarkForgeCommand,
    app_registry: &Arc<AppRegistry>,
) -> HandlerOutput {
    let data = match cmd {
        MarkForgeCommand::Launch => match apps::start(app_registry, MARK_FORGE_APP).await {
            Ok(()) => {
                info!(app = MARK_FORGE_APP, "mark-forge launch requested");
                telemetry::emit(
                    "system",
                    "action.executed",
                    json!({"tool": "start_app", "outcome": "Starting the Mark-Forge panel."}),
                );
                "Bringing up the physics sandbox now, sir.".to_string()
            }
            Err(e) => {
                warn!(app = MARK_FORGE_APP, error = %e, "mark-forge launch failed");
                format!("Mark-Forge could not be started: {e}")
            }
        },
        MarkForgeCommand::Op(op_line) => {
            match apps::send_op(app_registry, MARK_FORGE_APP, &op_line).await {
                Ok(()) => {
                    info!(app = MARK_FORGE_APP, op = %op_line, "forwarded mark-forge op");
                    telemetry::emit(
                        "system",
                        "app.op_forwarded",
                        json!({"name": MARK_FORGE_APP, "op": op_line}),
                    );
                    "Done, sir.".to_string()
                }
                Err(e) => {
                    warn!(app = MARK_FORGE_APP, op = %op_line, error = %e, "mark-forge op forward failed");
                    format!("I couldn't reach the physics sandbox: {e}. Open it first, sir.")
                }
            }
        }
    };
    HandlerOutput {
        data,
        llm_voice: true,
    }
}

/// A non-empty trimmed string field of the classifier args object (Null and
/// {} both yield None — old servers and argless intents look identical).
fn arg_str<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// web.open: open args.url when the classifier supplied one; otherwise fall
/// back to a web search over the utterance's content words — the user
/// clearly wanted the web, guessing a domain would be worse.
async fn handle_web_open(text: &str, args: &serde_json::Value) -> String {
    let browser = arg_str(args, "browser");
    let result = match arg_str(args, "url") {
        Some(url) => actions::open_url(url, browser).await,
        None => actions::search_url(&extract_web_query(text), browser).await,
    };
    finish_web_action("open_url", result)
}

/// web.search: args.query, or the utterance's content words when absent.
async fn handle_web_search(text: &str, args: &serde_json::Value) -> String {
    let browser = arg_str(args, "browser");
    let query = match arg_str(args, "query") {
        Some(q) => q.to_string(),
        None => extract_web_query(text),
    };
    finish_web_action("search_url", actions::search_url(&query, browser).await)
}

fn finish_web_action(tool: &str, result: Result<String>) -> String {
    match result {
        Ok(outcome) => {
            info!(outcome, tool, "web action completed");
            telemetry::emit(
                "system",
                "action.executed",
                json!({"tool": tool, "outcome": first_chars(&outcome, 120)}),
            );
            outcome
        }
        Err(e) => {
            warn!(error = %e, tool, "web action failed");
            format!("The web request failed: {e}")
        }
    }
}

/// file.op: Spotlight search on the utterance's content words; the result
/// list is the converse data. If exactly one strong match comes back and the
/// utterance says to open it, open it too.
/// Resolve the daemon's project root the same way the rest of the daemon does
/// (`DARWIN_ROOT` env, else the cwd) — used to locate config/darwin.toml and
/// state/docsearch.db for the on-device file-RAG index trigger.
fn project_root() -> std::path::PathBuf {
    std::env::var("DARWIN_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")))
}

/// The "index my documents" / "reindex" intent: rebuild the on-device file-RAG
/// index over the EXPLICITLY-ALLOWLISTED `[docsearch].roots`. CONFIG-GATED
/// ([`crate::docsearch::index_documents`] enforces `[docsearch].enabled` AND a
/// non-empty `roots` before touching the disk), so an OFF subsystem or an empty
/// allowlist indexes NOTHING — it never silently scans the disk. The index runs
/// 100% on-device: file contents + embeddings never leave the device, and when the
/// on-device embedder is down the chunks are stored vector-less so search falls
/// back to BM25. Returns an honest status line (or the off/not-configured message).
async fn handle_docsearch_index() -> String {
    use crate::docsearch::index_documents;
    let root = project_root();
    let (cfg, _issues) = Config::load(&root.join("config").join("darwin.toml"));
    // Honest, actionable copy when the feature isn't set up — never a silent scan.
    if !crate::docsearch::indexing_permitted(cfg.docsearch.enabled, &cfg.docsearch.roots) {
        if !cfg.docsearch.enabled {
            return "On-device file search is off. Enable [docsearch] and add a folder to \
                    index in the config — it ships disabled and indexes only the folders \
                    you allowlist, never your whole disk."
                .to_string();
        }
        return "On-device file search is on, but no folder is allowlisted to index yet. \
                Add a folder under [docsearch].roots — nothing else is ever read."
            .to_string();
    }
    let index = match crate::crypto::open_doc_index(&root.join("state").join("docsearch.db")) {
        Ok(idx) => idx,
        Err(e) => {
            warn!(error = %e, "docsearch: could not open the file index");
            return format!("I couldn't open the file index to reindex: {e}");
        }
    };
    let embedder = anthropic::inference_embedder();
    match index_documents(&cfg.docsearch, &index, &*embedder).await {
        Ok(Some(status)) => {
            telemetry::emit(
                "local",
                "docsearch.indexed",
                json!({
                    "files": status.files,
                    "chunks": status.chunks,
                    "embedded_chunks": status.embedded_chunks,
                }),
            );
            let method = if status.embedded_chunks == status.chunks && status.chunks > 0 {
                "on-device embeddings"
            } else {
                "lexical BM25 (the on-device embedder was unavailable, so search will be keyword-based)"
            };
            format!(
                "Indexed {} file(s) into {} chunk(s) from your allowlisted folders — all on-device, \
                 nothing left the machine. Search will use {}.",
                status.files, status.chunks, method
            )
        }
        Ok(None) => "On-device file search isn't configured to index anything yet.".to_string(),
        Err(e) => {
            warn!(error = %e, "docsearch: reindex failed");
            format!("The file index could not be rebuilt: {e}")
        }
    }
}

/// The "forget my file index" / "clear my indexed files" intent: CLEAR the
/// on-device file-RAG index ([`crate::docsearch::DocIndex::forget`]) so no file
/// chunk or embedding remains — the FORGETTABLE half of the contract. It only
/// ever touches the local `state/docsearch.db` the index/search paths use;
/// nothing else is read, nothing leaves the device. Opening the store creates an
/// empty one, so "forget" with nothing indexed is honestly a no-op ("there was
/// nothing to forget") rather than an error. No config gate is needed: clearing
/// the user's own local index is always safe and never widens any surface.
async fn handle_docsearch_forget() -> String {
    let root = project_root();
    let index = match crate::crypto::open_doc_index(&root.join("state").join("docsearch.db")) {
        Ok(idx) => idx,
        Err(e) => {
            warn!(error = %e, "docsearch: could not open the file index to forget");
            return format!("I couldn't open the file index to clear it: {e}");
        }
    };
    match index.forget().await {
        Ok(0) => "Your on-device file index was already empty, sir — there was nothing to forget."
            .to_string(),
        Ok(removed) => {
            // Mirror the index path's telemetry so the HUD index-status panel
            // reflects the now-empty store (0 files / 0 chunks). Local 127.0.0.1
            // broadcast only — nothing leaves the device.
            telemetry::emit(
                "local",
                "docsearch.indexed",
                json!({"files": 0, "chunks": 0, "embedded_chunks": 0}),
            );
            format!(
                "Done — I've forgotten your indexed files ({removed} chunk(s) cleared). \
                 Nothing of them remains on the device; reindex whenever you'd like to search again."
            )
        }
        Err(e) => {
            warn!(error = %e, "docsearch: forget failed");
            format!("The file index could not be cleared: {e}")
        }
    }
}

/// Serialize a bounded [`crate::world_model::WorldState`] into the HUD-facing
/// `graph` payload of the `knowledge_graph.built` event. Each entity carries its
/// stable type token + id + display name and its `source` PROVENANCE attribute
/// (the only attribute the deterministic build writes; absent for an entity that
/// somehow has none, so the HUD shows the honest "no citation"); each relationship
/// carries the from/relation/to ids + the `source file:offset` detail on the
/// co-occurrence edge. Counts/ids/names/source strings ONLY — no chunk text. The
/// view is already bounded by the world model's read/structure caps; this caps the
/// emitted lists again defensively so one event can never balloon the broadcast.
fn world_snapshot_json(state: &crate::world_model::WorldState) -> serde_json::Value {
    const MAX_EMIT_ENTITIES: usize = 256;
    const MAX_EMIT_RELATIONS: usize = 512;
    let entities: Vec<serde_json::Value> = state
        .entities
        .iter()
        .take(MAX_EMIT_ENTITIES)
        .map(|e| {
            let source = e
                .attributes
                .iter()
                .find(|(a, _)| a == "source")
                .map(|(_, v)| v.clone());
            json!({
                "type": e.entity_type.as_str(),
                "id": e.id,
                "name": e.name,
                "source": source,
            })
        })
        .collect();
    let relationships: Vec<serde_json::Value> = state
        .relationships
        .iter()
        .take(MAX_EMIT_RELATIONS)
        .map(|r| {
            json!({
                "from": r.from,
                "relation": r.relation,
                "to": r.to,
                "source": r.value,
            })
        })
        .collect();
    json!({ "entities": entities, "relationships": relationships })
}

/// The "build/map a knowledge graph from my documents" intent: mine the user's
/// ALREADY-INDEXED docsearch chunks for grounded entities/relationships and upsert
/// them into the SHARED world model. DOUBLE-GATED ([`knowledge_graph::build_permitted`]:
/// `[docsearch].enabled` AND `[docsearch].build_graph`, both ship false) — an OFF
/// subsystem mines NOTHING. It reads only chunks the confined, allowlisted indexer
/// already produced (it never re-walks the disk) and writes only the shared
/// `user.world.*` tier (never an agent's private namespace, never a fabricated
/// node). The shipped extractor is the CONSERVATIVE deterministic heuristic — the
/// copy says so. Returns an honest status line (or the off/not-configured message).
async fn handle_build_knowledge_graph(memory: &Memory) -> String {
    use crate::knowledge_graph::{self, DeterministicExtractor, Extractor};
    let root = project_root();
    let (cfg, _issues) = Config::load(&root.join("config").join("darwin.toml"));
    if !knowledge_graph::build_permitted(cfg.docsearch.enabled, cfg.docsearch.build_graph) {
        if !cfg.docsearch.enabled {
            return "Building a knowledge graph needs on-device file search, which is off. \
                    Enable [docsearch] (and set [docsearch].build_graph = true), then add a \
                    folder to index — it ships disabled and reads only the folders you allowlist."
                .to_string();
        }
        return "On-device file search is on, but the knowledge-graph build is off. \
                Set [docsearch].build_graph = true to let me map your indexed documents \
                into the shared world model — it stays off until you turn it on."
            .to_string();
    }
    let index = match crate::crypto::open_doc_index(&root.join("state").join("docsearch.db")) {
        Ok(idx) => idx,
        Err(e) => {
            warn!(error = %e, "knowledge_graph: could not open the file index");
            return format!("I couldn't open the file index to build the graph: {e}");
        }
    };
    let chunks = match index.chunks_for_graph().await {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "knowledge_graph: could not read indexed chunks");
            return format!("I couldn't read the indexed chunks to build the graph: {e}");
        }
    };
    if chunks.is_empty() {
        return "There are no indexed documents to map yet. Index your allowlisted \
                folders first, then I can build the knowledge graph from them."
            .to_string();
    }
    // Pick the extractor: the conservative deterministic heuristic (default) OR
    // the OPT-IN LLM-grounded extractor when [docsearch].graph_extractor = "llm".
    // The LLM path connects to the on-device inference server; if it is
    // unreachable at build start we FALL BACK to the deterministic extractor
    // honestly (never a half-wired LLM build). Either way `map_documents`
    // re-checks the gate (defense-in-depth) and the grounding contract holds.
    let det = DeterministicExtractor;
    let llm = if cfg.docsearch.graph_extractor.trim() == "llm" {
        let sock = root.join("state").join("ipc").join("inference.sock");
        match knowledge_graph::LlmExtractor::connect(&sock).await {
            Some(e) => Some(e),
            None => {
                warn!("knowledge_graph: LLM extractor requested but inference server unreachable; using the deterministic extractor");
                None
            }
        }
    } else {
        None
    };
    let extractor: &dyn Extractor = match &llm {
        Some(e) => e,
        None => &det,
    };
    match knowledge_graph::map_documents(
        cfg.docsearch.enabled,
        cfg.docsearch.build_graph,
        memory,
        extractor,
        &chunks,
    )
    .await
    {
        Ok(Some(stats)) => {
            // Read back the bounded SHARED world snapshot so the HUD can render the
            // grouped entities + their provenance + relationships. This is the same
            // structured view `world_query` returns; it is `user.world.*` only (no
            // agent.* private note can appear), reads ONLY counts/ids/names/source
            // strings the build just grounded, and rides the local 127.0.0.1
            // broadcast. A read failure is non-fatal — the build already landed, so
            // emit the stats with an empty graph rather than dropping the event.
            let graph = match crate::world_model::snapshot(memory).await {
                Ok(state) => world_snapshot_json(&state),
                Err(e) => {
                    warn!(error = %e, "knowledge_graph: snapshot read for HUD failed");
                    json!({ "entities": [], "relationships": [] })
                }
            };
            telemetry::emit(
                "local",
                "knowledge_graph.built",
                json!({
                    "chunks_scanned": stats.chunks_scanned,
                    "entities_written": stats.entities_written,
                    "relationships_written": stats.relationships_written,
                    "skipped_at_cap": stats.skipped_at_cap,
                    "graph": graph,
                    "extractor": extractor.method(),
                }),
            );
            let cap_note = if stats.skipped_at_cap > 0 {
                format!(
                    " ({} were skipped because the world model is at its bound — I never grow it past its cap)",
                    stats.skipped_at_cap
                )
            } else {
                String::new()
            };
            format!(
                "Mapped your documents into the shared world model: {} entit(ies) and {} \
                 relationship(s) from {} indexed chunk(s){}. These were extracted from YOUR \
                 documents with a conservative heuristic (it errs toward missing rather than \
                 inventing) and each is tagged with its source file — nothing was fabricated.",
                stats.entities_written, stats.relationships_written, stats.chunks_scanned, cap_note
            )
        }
        // Unreachable in practice (the gate was checked above), but the gated entry
        // point can return None when off — keep the off message honest if it does.
        Ok(None) => "The knowledge-graph build is off, so I mapped nothing.".to_string(),
        Err(e) => {
            warn!(error = %e, "knowledge_graph: build failed");
            format!("The knowledge graph could not be built: {e}")
        }
    }
}

async fn handle_file_intent(text: &str) -> String {
    let query = extract_content_words(text);
    if query.is_empty() {
        return "The request did not include anything to search for; ask what file they mean."
            .to_string();
    }
    match actions::search_files_raw(&query, 5).await {
        Ok(hits) => {
            telemetry::emit(
                "system",
                "action.executed",
                json!({"tool": "search_files", "outcome": format!("{} hits for '{query}'", hits.len())}),
            );
            let mut data = actions::format_file_hits(&query, &hits);
            if hits.len() == 1 && utterance_wants_open(text) {
                match actions::open_path(&hits[0].path_str()).await {
                    Ok(opened) => data = format!("{data}\n{opened}"),
                    Err(e) => {
                        warn!(error = %e, "open_path after search failed");
                        data = format!("{data}\nIt could not be opened: {e}");
                    }
                }
            }
            data
        }
        Err(e) => {
            warn!(error = %e, "file search failed");
            format!("The file search failed: {e}")
        }
    }
}

/// Words the matchers should never see — command verbs, fillers, articles.
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "any", "app", "application", "can", "close", "could", "do", "exit",
    "find", "for", "go", "hey", "in", "is", "it", "darwin", "kill", "launch", "look",
    "looking", "me", "my", "now", "of", "on", "open", "please", "quit", "search", "show",
    "some", "start", "that", "the", "then", "this", "to", "up", "where", "with", "would",
    "you",
];

/// Extra noise words for web requests: the command vocabulary around what
/// the user actually wants opened or searched.
const WEB_STOPWORDS: &[&str] = &[
    "browser", "google", "internet", "online", "page", "site", "web", "website",
];

/// Extra noise words for file searches (the command vocabulary around the
/// actual content words).
const FILE_STOPWORDS: &[&str] = &[
    "called", "computer", "document", "documents", "file", "files", "folder", "folders",
    "named", "recent",
];

fn split_words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '.' && c != '-' && c != '_')
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// Simple heuristic: the words after the first trigger verb (open/launch/
/// start plus the quit-class verbs, kept in sync with wants_quit so a quit
/// utterance extracts its app name instead of feeding the launcher), minus
/// stopwords. Empty when no trigger verb is present — the caller then feeds
/// the whole utterance to the fuzzy matcher instead.
fn extract_app_name(text: &str) -> String {
    let words = split_words(text);
    let Some(pos) = words.iter().position(|w| {
        matches!(
            w.as_str(),
            "open" | "launch" | "start" | "quit" | "close" | "exit" | "stop" | "kill"
        )
    }) else {
        return String::new();
    };
    words[pos + 1..]
        .iter()
        .filter(|w| !STOPWORDS.contains(&w.as_str()))
        .cloned()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Content words of a file request: everything minus the command vocabulary.
fn extract_content_words(text: &str) -> String {
    split_words(text)
        .into_iter()
        .filter(|w| !STOPWORDS.contains(&w.as_str()) && !FILE_STOPWORDS.contains(&w.as_str()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Content words of a web request: everything minus the command vocabulary
/// and the web noise words ("search the web for rust tutorials" -> "rust
/// tutorials").
fn extract_web_query(text: &str) -> String {
    split_words(text)
        .into_iter()
        .filter(|w| !STOPWORDS.contains(&w.as_str()) && !WEB_STOPWORDS.contains(&w.as_str()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn utterance_wants_open(text: &str) -> bool {
    text.to_lowercase().contains("open")
}

fn first_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// ===========================================================================
// Silicon Canvas voice control (SPEC §6 — the daemon forwards STRUCTURED ops
// ONLY; the app never parses natural language).
//
// Voice reaches a micro-app today only via app.launch/app.control -> the fuzzy
// app matcher -> apps::start/stop (launch & quit only). That seam does NOT
// generalize to "select.net"/"trace.start"/"erc.run" — those are ops sent to an
// ALREADY-RUNNING app, for which the host had no forwarding path. The SMALLEST
// honest addition is: (1) apps::send_op forwards one structured op line to a
// running app (apps.rs), and (2) the deterministic NL->op classifier below maps
// the spoken control phrases to those op lines. The op JSON is built here to
// match Silicon Canvas's `apps/silicon-canvas/src/ops.rs` wire form VERBATIM
// (the `#[serde(tag="op")]` dotted names); the daemon never imports that
// standalone crate, so a round-trip test in ops.rs locks the two sides
// together. The classifier is checked BEFORE the normal classifier route (like
// roll-call) so a precise control phrase never lands on the cloud/LLM.
// ===========================================================================

/// The Silicon Canvas micro-app's registered name (its manifest `[app].name`
/// and the key into the app registry / its socket).
pub const SILICON_CANVAS_APP: &str = "silicon-canvas";

/// What a Silicon-Canvas voice command resolves to. Either LAUNCH the app
/// (handled by the existing apps::start path) or forward a STRUCTURED op line
/// to the already-running app (apps::send_op). The daemon never sends anything
/// but these two; the op body is opaque to it.
#[derive(Debug, Clone, PartialEq)]
pub enum SiliconCanvasCommand {
    /// "open silicon canvas" — start the micro-app.
    Launch,
    /// A structured op line to forward verbatim to the running app. The String
    /// is the COMPLETE JSON op object (one line), e.g.
    /// `{"op":"select.net","name":"3V3"}`.
    Op(String),
}

/// Whether the utterance names Silicon Canvas itself ("silicon canvas",
/// "silicon-canvas", "the canvas"). Used to gate the launch phrase and to
/// disambiguate a bare "open" so an unrelated "open safari" is never captured.
fn mentions_silicon_canvas(lower: &str) -> bool {
    lower.contains("silicon canvas")
        || lower.contains("silicon-canvas")
        || lower.contains("siliconcanvas")
        || lower.contains("the schematic")
        || lower.contains("the board view")
}

/// Map a spoken utterance to a Silicon Canvas command, or None when it is not a
/// Silicon-Canvas control phrase (the turn then falls through to normal
/// routing). Deterministic and pure so the mapping is unit-tested without a
/// socket, a running app, or the classifier. Order matters: the most specific
/// ops (trace step/stop, ERC, view fit, component/net selection) are matched
/// before the broad "open/show silicon canvas" launch so "trace this net" does
/// not get mistaken for a launch.
///
/// Recognized phrases (all case-insensitive, whole lowercased utterance):
///   - "open/show/launch/bring up silicon canvas"            -> Launch
///   - "show me the <X> net" / "highlight the <X> net" /
///     "select the <X> net"                                  -> select.net {X}
///   - "show/select component <REF>"                         -> select.component
///   - "trace this net" / "start trace/tracing"              -> trace.start
///   - "next/step (the) trace" / "advance the trace"         -> trace.step
///   - "stop/end/exit trace/tracing"                         -> trace.stop
///   - "run erc" / "run the electrical rule check(s)" /
///     "check the electrical rules"                          -> erc.run
///   - "fit the board" / "show the whole board" / "fit all"  -> view.set fit all
pub fn silicon_canvas_command(text: &str) -> Option<SiliconCanvasCommand> {
    let lower = text.to_lowercase();

    // --- trace mode (specific verbs before the broad launch) ---------------
    // Step BEFORE start/stop so "next trace step" is unambiguous.
    if (lower.contains("trace") || lower.contains("tracing"))
        && (lower.contains("next") || lower.contains("step") || lower.contains("advance"))
    {
        return Some(SiliconCanvasCommand::Op(op_trace_step()));
    }
    if (lower.contains("trace") || lower.contains("tracing"))
        && (lower.contains("stop")
            || lower.contains("end")
            || lower.contains("exit")
            || lower.contains("cancel"))
    {
        return Some(SiliconCanvasCommand::Op(op_trace_stop()));
    }
    // "trace this net", "start tracing", "begin the trace", "trace the net".
    if lower.contains("trace") || lower.contains("tracing") {
        return Some(SiliconCanvasCommand::Op(op_trace_start()));
    }

    // --- ERC ---------------------------------------------------------------
    if lower.contains("erc")
        || (lower.contains("electrical rule")
            && (lower.contains("run") || lower.contains("check")))
    {
        return Some(SiliconCanvasCommand::Op(op_erc_run()));
    }

    // --- net selection -----------------------------------------------------
    // "show me the 3V3 net", "highlight the GND net", "select the VBUS net".
    if let Some(net) = extract_net_name(&lower) {
        return Some(SiliconCanvasCommand::Op(op_select_net(&net)));
    }

    // --- component selection ----------------------------------------------
    if let Some(reference) = extract_component_ref(&lower) {
        return Some(SiliconCanvasCommand::Op(op_select_component(&reference)));
    }

    // --- view fit ----------------------------------------------------------
    if (lower.contains("fit") && (lower.contains("board") || lower.contains("all")))
        || lower.contains("whole board")
        || lower.contains("entire board")
    {
        return Some(SiliconCanvasCommand::Op(op_view_fit_all()));
    }

    // --- launch ------------------------------------------------------------
    // Only when the utterance actually names Silicon Canvas AND carries an
    // open-class verb — "open silicon canvas", "show me silicon canvas",
    // "bring up the schematic". This is last so an op phrase that also says
    // "show" (e.g. "show me the 3V3 net") was already handled above.
    if mentions_silicon_canvas(&lower)
        && (lower.contains("open")
            || lower.contains("launch")
            || lower.contains("start")
            || lower.contains("bring up")
            || lower.contains("show"))
    {
        return Some(SiliconCanvasCommand::Launch);
    }

    None
}

/// Extract the net name from a "<verb> the <NAME> net" phrase. Returns the token
/// immediately before the word "net" (the net's name as spoken), uppercased to
/// match KiCad net-label convention (3V3, GND, VBUS); None when there is no
/// "net" keyword or no name precedes it. The net name is forwarded verbatim in
/// the op — Silicon Canvas resolves it against the open document.
fn extract_net_name(lower: &str) -> Option<String> {
    // Require the standalone word "net" so "network"/"netflix" never match.
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric() && c != '.' && c != '-' && c != '+')
        .filter(|w| !w.is_empty())
        .collect();
    let net_pos = words.iter().position(|w| *w == "net")?;
    if net_pos == 0 {
        return None;
    }
    // The token just before "net", skipping a trailing article ("the net" has
    // no name). Walk back over "the"/"a" if they sit right before "net".
    let mut idx = net_pos - 1;
    while matches!(words[idx], "the" | "a" | "an") {
        if idx == 0 {
            return None;
        }
        idx -= 1;
    }
    let name = words[idx];
    // A pure stopword/verb is not a net name.
    if matches!(
        name,
        "show" | "me" | "highlight" | "select" | "the" | "this" | "that" | "trace"
    ) {
        return None;
    }
    Some(name.to_uppercase())
}

/// Extract a component reference designator from "show/select component <REF>".
/// The reference is the token after the word "component", uppercased (KiCad
/// refs are like U3, R12, C5). None when there is no "component" keyword or
/// nothing follows it.
fn extract_component_ref(lower: &str) -> Option<String> {
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let pos = words.iter().position(|w| *w == "component")?;
    let reference = words.get(pos + 1)?;
    // A bare reference looks like a letter-prefix + digits; require at least one
    // digit so "select component now" does not pick up "now".
    if reference.chars().any(|c| c.is_ascii_digit()) {
        Some(reference.to_uppercase())
    } else {
        None
    }
}

// The op-string builders. Each produces the EXACT wire JSON Silicon Canvas's
// ops.rs deserializes (verified by a round-trip test there). serde_json builds
// them so a net/component name with a quote can never break the JSON framing.

fn op_select_net(name: &str) -> String {
    json!({"op": "select.net", "name": name}).to_string()
}
fn op_select_component(reference: &str) -> String {
    json!({"op": "select.component", "name": reference}).to_string()
}
fn op_trace_start() -> String {
    json!({"op": "trace.start"}).to_string()
}
fn op_trace_step() -> String {
    json!({"op": "trace.step"}).to_string()
}
fn op_trace_stop() -> String {
    json!({"op": "trace.stop"}).to_string()
}
fn op_erc_run() -> String {
    json!({"op": "erc.run"}).to_string()
}
fn op_view_fit_all() -> String {
    json!({"op": "view.set", "mode": "fit", "target": "all"}).to_string()
}

// ===========================================================================
// Vision voice control (mirrors the Silicon Canvas seam above: the daemon
// forwards STRUCTURED ops ONLY; the Vision app never parses natural language).
//
// The Vision micro-app (apps/vision) is a binary micro-app on the same runtime.
// Its HOST -> APP op wire form is FROZEN in apps/vision/Sources/vision/Op.swift:
// every op is `{"type":"op","op":"<name>", ...}` (note the `"type":"op"`
// envelope — UNLIKE Silicon Canvas's bare `{"op":...}`; the Swift Op.decode
// dispatches ops only when type == "op"). The op-string builders below produce
// that EXACT wire shape so the daemon forwards a line the app already accepts in
// its own IPCTests (e.g. {"type":"op","op":"watch.start","source":"camera"}).
//
// DEFENSIVE-ONLY: the recognized phrases map to PRESENCE/OBJECT detection and
// capture lifecycle on the user's OWN devices — never an identity query. There
// is no "who is <NAME>" op; "who is there" asks the app for a generic presence
// status snapshot, not a face match. Capture itself is gated by macOS TCC
// (runtime consent), which the daemon cannot grant.
// ===========================================================================

/// The Vision micro-app's registered name (its manifest `[app].name` and the
/// key into the app registry / its socket).
pub const VISION_APP: &str = "vision";

/// What a Vision voice command resolves to: LAUNCH the app, or forward a
/// STRUCTURED op line to the already-running app. The op body is opaque to the
/// daemon (built to match Op.swift verbatim).
#[derive(Debug, Clone, PartialEq)]
pub enum VisionCommand {
    /// "open vision" — start the micro-app.
    Launch,
    /// A complete JSON op line (one line) to forward verbatim, e.g.
    /// `{"type":"op","op":"watch.start","source":"camera"}`.
    Op(String),
}

/// Whether the utterance names the Vision app / capability itself ("vision",
/// "the camera", "the camera feed"). Used to gate the bare launch verb so an
/// unrelated "open safari" is never captured.
fn mentions_vision(lower: &str) -> bool {
    contains_word(lower, "vision")
        || lower.contains("the camera")
        || lower.contains("camera feed")
        || lower.contains("the screen feed")
}

/// Map a spoken utterance to a Vision command, or None when it is not a Vision
/// control phrase (the turn falls through to normal routing). Deterministic and
/// pure so the mapping is unit-tested without a socket, a running app, or the
/// classifier. Order matters: specific ops (watch start/stop, analyze file,
/// sensitivity, status) are matched before the broad launch.
///
/// Recognized phrases (case-insensitive, whole lowercased utterance):
///   - "watch the door|room|camera"                 -> watch.start {camera}
///   - "watch the screen|display"                   -> watch.start {screen}
///   - "stop watching" / "stop the watch"           -> watch.stop
///   - "analyze this video" / "analyze <file>.mp4"  -> analyze.file {path}
///   - "what's on my screen" / "read my screen" / "read this"
///     -> read.screen (OCR; TRANSIENT)
///   - "where's the <X> button" / "find the <X> button" / "locate the <X>"
///     -> read.screen {query:<X>} (LOCATE, read-only)
///   - "what do you see" / "who is there" / "anyone there"
///     -> status (presence snapshot)
///   - "set sensitivity to <0..1 | a percent>"      -> set.sensitivity {value}
///   - "open/launch/start vision"                   -> Launch
pub fn vision_command(text: &str) -> Option<VisionCommand> {
    let lower = text.to_lowercase();

    // --- watch lifecycle (specific before the broad launch) ----------------
    // STOP first so "stop watching the door" is unambiguous.
    if (lower.contains("watch") || lower.contains("watching"))
        && (lower.contains("stop")
            || lower.contains("end")
            || lower.contains("quit")
            || lower.contains("cancel"))
    {
        return Some(VisionCommand::Op(op_watch_stop()));
    }
    // "watch the screen|display|monitor" -> screen; otherwise (door/room/camera/
    // entrance/the front) the camera. The verb "watch the <X>" is the trigger;
    // the source is decided by whether a SCREEN word is present.
    if lower.contains("watch") || lower.contains("watching") {
        let source = if lower.contains("screen")
            || lower.contains("display")
            || lower.contains("monitor")
        {
            "screen"
        } else {
            "camera"
        };
        return Some(VisionCommand::Op(op_watch_start(source)));
    }

    // --- analyze a video file ----------------------------------------------
    // "analyze this video", "analyze <name>.mp4", "analyze the video clip".
    if (lower.contains("analyze") || lower.contains("analyse"))
        && (lower.contains("video") || lower.contains("clip") || extract_video_path(&lower).is_some())
    {
        // A named file (…/foo.mp4) is forwarded verbatim. A bare "analyze this
        // video" (no filename) forwards an EMPTY path: Vision's Op.swift requires
        // a non-empty path, so it decodes to .unknown and the Pipeline reports a
        // clean vision.error — i.e. the app cleanly says it has no file to run,
        // it never crashes and never guesses. (The persona then asks which file.)
        let path = extract_video_path(&lower).unwrap_or_default();
        return Some(VisionCommand::Op(op_analyze_file(&path)));
    }

    // --- sensitivity -------------------------------------------------------
    if (lower.contains("sensitivity") || lower.contains("sensitive"))
        && (lower.contains("set") || lower.contains("to") || lower.contains("at"))
    {
        if let Some(value) = extract_sensitivity(&lower) {
            return Some(VisionCommand::Op(op_set_sensitivity(value)));
        }
    }

    // --- HANDWRITING read (#28) / DOCUMENT scan (#29) ----------------------
    // "read this handwriting" / "read the whiteboard" / "scan this document".
    // Both are READ-ON-REQUEST OCR variants of the user's OWN camera/screen
    // (TCC-gated), DISTINCT from the plain on-screen OCR below — so they are
    // matched FIRST (a "read this handwriting" must reach the handwriting
    // recognizer, a "scan this document" the camera scanner, not the generic
    // screen OCR). The recognized text is SENSITIVE + TRANSIENT (`is_screen_read`
    // covers these too). READ-ONLY: transcribes glyphs, never an identity.
    if let Some(op) = handwriting_document_op(&lower) {
        return Some(VisionCommand::Op(op));
    }

    // --- screen READ (OCR) — "what's on my screen" / "read my screen" / -----
    // "read this" / "where's the <X> button". DISTINCT from "watch the screen"
    // (a continuous detection watch) and from the presence STATUS below: this is
    // a one-shot OCR read of the user's OWN screen via ScreenCaptureKit, gated by
    // macOS TCC. The recognized text is SENSITIVE (it can contain on-screen
    // passwords/messages) and TRANSIENT — see `is_screen_read` + main.rs, which
    // keep it out of lifelong memory / optimizer traces. READ-ONLY: a where-is
    // query LOCATES a control, it never clicks. Checked before the presence
    // status so "what's on my screen" is an OCR read, not a presence snapshot.
    if let Some(op) = screen_read_op(&lower) {
        return Some(VisionCommand::Op(op));
    }

    // --- presence status ("what do you see" / "who is there") --------------
    // DEFENSIVE-ONLY: "who is there" is a PRESENCE query, not identity — it maps
    // to the same generic status snapshot as "what do you see". There is no
    // face-match / name-lookup op anywhere in the contract.
    if lower.contains("what do you see")
        || lower.contains("what can you see")
        || lower.contains("who is there")
        || lower.contains("who's there")
        || lower.contains("anyone there")
        || lower.contains("anybody there")
        || lower.contains("someone there")
        || lower.contains("somebody there")
        || lower.contains("what are you seeing")
    {
        return Some(VisionCommand::Op(op_status()));
    }

    // --- launch ------------------------------------------------------------
    // Only when the utterance names Vision AND carries an open-class verb.
    if mentions_vision(&lower)
        && (lower.contains("open")
            || lower.contains("launch")
            || lower.contains("start")
            || lower.contains("bring up")
            || lower.contains("fire up"))
    {
        return Some(VisionCommand::Launch);
    }

    None
}

/// Whether `lower` contains `word` as a STANDALONE token (alnum boundaries), so
/// "vision" matches in "open vision" but not inside "television"/"revision".
fn contains_word(lower: &str, word: &str) -> bool {
    lower
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| w == word)
}

/// Extract a video file path/name from an "analyze <…>.<ext>" phrase. Returns
/// the token that carries a known video extension (mp4/mov/m4v/avi), forwarded
/// verbatim so the app resolves it against its own videos/input dir. None when
/// no such token is present (a bare "analyze this video"). The token is taken
/// from the ORIGINAL-case text via a case-insensitive extension match so a
/// path's case survives (file systems are case-sensitive).
fn extract_video_path(lower: &str) -> Option<String> {
    lower
        .split(|c: char| c.is_whitespace())
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric() && c != '.' && c != '/' && c != '_' && c != '-'))
        .find(|w| {
            let lw = w.to_lowercase();
            lw.ends_with(".mp4")
                || lw.ends_with(".mov")
                || lw.ends_with(".m4v")
                || lw.ends_with(".avi")
        })
        .map(|w| w.to_string())
}

/// Extract a sensitivity value in 0..=1 from a "set sensitivity to <X>" phrase.
/// Accepts a bare 0..1 float ("0.7"), a percent ("70 percent"/"70%"), or the
/// words low/medium/high. None when no value is present. Clamped to 0..=1.
fn extract_sensitivity(lower: &str) -> Option<f64> {
    if lower.contains("low") {
        return Some(0.25);
    }
    if lower.contains("medium") || lower.contains("normal") {
        return Some(0.5);
    }
    if lower.contains("high") || lower.contains("max") {
        return Some(0.85);
    }
    // A numeric token: percent if it has a '%' or the word "percent" follows, or
    // is > 1; otherwise a bare 0..1 float.
    let is_percent = lower.contains('%') || lower.contains("percent");
    for tok in lower.split(|c: char| c.is_whitespace() || c == '%') {
        let t = tok.trim();
        if t.is_empty() {
            continue;
        }
        if let Ok(n) = t.parse::<f64>() {
            let v = if is_percent || n > 1.0 { n / 100.0 } else { n };
            return Some(v.clamp(0.0, 1.0));
        }
    }
    None
}

// The op-string builders — EXACT Vision Op.swift wire form: every op carries the
// `"type":"op"` envelope (unlike Silicon Canvas's bare ops). serde_json builds
// them so a path/source with a quote can never break the JSON framing.

fn op_watch_start(source: &str) -> String {
    json!({"type": "op", "op": "watch.start", "source": source}).to_string()
}
fn op_watch_stop() -> String {
    json!({"type": "op", "op": "watch.stop"}).to_string()
}
fn op_analyze_file(path: &str) -> String {
    json!({"type": "op", "op": "analyze.file", "path": path}).to_string()
}
fn op_set_sensitivity(value: f64) -> String {
    json!({"type": "op", "op": "set.sensitivity", "value": value}).to_string()
}
fn op_status() -> String {
    json!({"type": "op", "op": "status"}).to_string()
}

/// The Vision Sound-Analysis op (Op.swift wireName "classify.sound", task #15).
/// Classifies ONE supplied audio CLIP at `path` through Apple Sound Analysis (the
/// built-in ~300-class `SNClassifierIdentifier.version1`, on-device/ANE-eligible)
/// and emits a `vision.sound` readout with the top sound classes {label,
/// confidence}. `path` is REQUIRED (the host names the confined clip the daemon
/// wrote from its captured buffer); a classify.sound WITHOUT a non-empty path
/// decodes to `.unknown` Swift-side and the app refuses to classify — it NEVER
/// opens the mic. Mirrors `describe.capture`'s path-required pattern. serde_json
/// builds the line so a path with a quote can never break the JSON framing. ONLY
/// the sound-class LABELS leave the op; the AUDIO never leaves the device.
fn op_classify_sound(path: &str) -> String {
    json!({"type": "op", "op": "classify.sound", "path": path}).to_string()
}

/// The Vision HANDWRITING/WHITEBOARD read op (#28, Op.swift wireName
/// "read.handwriting"). Captures ONE frame from a TCC-gated source and runs the
/// handwriting recognizer (VNRecognizeTextRequest, .accurate + language
/// correction — the config best for handwriting/whiteboard text), emitting a
/// `vision.screen` readout (tagged read_kind=handwriting) with the recognized
/// LINES + boxes. The default source is `.camera` (handwriting/whiteboard is most
/// naturally read off the camera). A `screen` request stamps the screen source.
/// READ-ON-REQUEST + READ-ONLY: it transcribes glyphs, never an identity, never a
/// click. The recognized text is SENSITIVE + TRANSIENT (see `is_screen_read` +
/// main.rs). serde_json builds the line so a source token can never break framing.
fn op_read_handwriting(source: Option<&str>) -> String {
    match source {
        Some(s) if s == "screen" || s == "camera" => {
            json!({"type": "op", "op": "read.handwriting", "source": s}).to_string()
        }
        // Default (no source) -> the app's .camera default; keep the line minimal.
        _ => json!({"type": "op", "op": "read.handwriting"}).to_string(),
    }
}

/// The Vision camera DOCUMENT-SCANNER op (#29, Op.swift wireName "scan.document").
/// Captures ONE frame from a TCC-gated source (default `.camera`) and runs the
/// document scanner (VNDetectDocumentSegmentationRequest -> CIPerspectiveCorrection
/// -> VNRecognizeTextRequest), emitting a `vision.screen` readout (tagged
/// read_kind=document) with the text off the CORRECTED page plus the HONEST
/// document-detected bool. When NO document is found, the readout is honestly
/// empty (never a fabricated page). READ-ON-REQUEST + READ-ONLY: transcribes
/// glyphs, never an identity. The recognized text is SENSITIVE + TRANSIENT.
fn op_scan_document(source: Option<&str>) -> String {
    match source {
        Some(s) if s == "screen" || s == "camera" => {
            json!({"type": "op", "op": "scan.document", "source": s}).to_string()
        }
        _ => json!({"type": "op", "op": "scan.document"}).to_string(),
    }
}

/// Map a lowercased utterance to a `read.handwriting` (#28) or `scan.document`
/// (#29) op line, or None when it is neither. PURE + unit-tested (no socket, no
/// app). Both are READ-ON-REQUEST OCR variants of the user's OWN camera/screen,
/// DISTINCT from the plain on-screen OCR (`screen_read_op`) — checked BEFORE it so
/// "read this handwriting" is the handwriting recognizer, not the generic screen
/// OCR. Recognized intents:
///   - "read this handwriting" / "read the whiteboard" / "what does this say"
///     (with a handwriting/whiteboard/note cue)            -> read.handwriting
///   - "scan this document" / "scan the page" / "scan this receipt"  -> scan.document
///     The recognized text is SENSITIVE + TRANSIENT (`is_screen_read` covers these).
fn handwriting_document_op(lower: &str) -> Option<String> {
    // SCAN a document/page/receipt with the camera (#29). The verb "scan" plus a
    // document-ish noun. Checked first so "scan this document" never falls into a
    // handwriting/OCR read.
    let names_scan = lower.contains("scan");
    let mentions_document = lower.contains("document")
        || lower.contains("page")
        || lower.contains("receipt")
        || lower.contains("paper")
        || lower.contains("invoice")
        || lower.contains("form");
    if names_scan && mentions_document {
        // A document is scanned with the camera by default; honor an explicit
        // "on screen" / "on my display" request.
        let source = if lower.contains("screen") || lower.contains("display") {
            Some("screen")
        } else {
            None // -> the app's .camera default
        };
        return Some(op_scan_document(source));
    }

    // READ HANDWRITING / a whiteboard / a handwritten note (#28). A read/transcribe
    // verb (or "what does this say") plus a handwriting/whiteboard cue.
    let mentions_handwriting = lower.contains("handwriting")
        || lower.contains("handwritten")
        || lower.contains("whiteboard")
        || lower.contains("white board")
        || lower.contains("hand writing");
    let names_read = lower.contains("read")
        || lower.contains("transcribe")
        // "what does this say" / "what does this handwriting say" / "what does it
        // say" — a "what does … say" question over the handwriting cue is a read.
        || (lower.contains("what does") && lower.contains("say"))
        || lower.contains("what's written")
        || lower.contains("whats written");
    if mentions_handwriting && names_read {
        // Handwriting/whiteboard is read off the camera by default; honor an
        // explicit "on screen" request (e.g. a whiteboard shared on screen).
        let source = if lower.contains("screen") || lower.contains("display") {
            Some("screen")
        } else {
            None // -> the app's .camera default
        };
        return Some(op_read_handwriting(source));
    }

    None
}

/// The Vision OCR screen-read op (Op.swift wireName "read.screen"). Captures ONE
/// frame from the user's OWN .screen source (ScreenCaptureKit, TCC-gated), runs
/// the .text OCR detector, structures the blocks, and emits a `vision.screen`
/// event carrying the recognized text + control candidates. The default source
/// is `.screen` (the on-wire `{"type":"op","op":"read.screen"}` form), so we do
/// not stamp a `source` field — keeping the line byte-identical to the FROZEN
/// default the Swift `testFrozenOpWireNamesUnchanged` pins. An optional `query`
/// rides along ONLY for a "where is <X>" locate request (READ-ONLY: locate, not
/// click). serde_json builds the line so a query with a quote can never break
/// the JSON framing.
fn op_read_screen(query: Option<&str>) -> String {
    match query {
        Some(q) if !q.trim().is_empty() => {
            json!({"type": "op", "op": "read.screen", "query": q.trim()}).to_string()
        }
        _ => json!({"type": "op", "op": "read.screen"}).to_string(),
    }
}

/// Map a lowercased utterance to a `read.screen` op line, or None when it is not
/// a screen-read request. PURE so the mapping is unit-tested without a socket or
/// a running app. Recognized intents:
///   - "what's on my screen" / "what is on screen" / "read my screen" /
///     "read the screen" / "read this" / "read what's on screen"  -> read.screen
///   - "where's the <X> button" / "where is the submit button" / "find the
///     <X> button" / "locate the <X>"                              -> read.screen{query:<X>}
///     A where-is query carries the control phrase so the app's structuring can
///     LOCATE (not click) the best-matching block.
fn screen_read_op(lower: &str) -> Option<String> {
    // Where-is a control: "where is/where's the <X> button", "find the <X>
    // button", "locate the <X>". The query is the control phrase; the app
    // locates it READ-ONLY (returns its box/center, never a click).
    if let Some(query) = extract_where_is_query(lower) {
        return Some(op_read_screen(Some(&query)));
    }
    // Plain screen read. "read this" alone is a screen read (the most common
    // hands-free "read what's in front of me"); "read my/the screen", "what's
    // on (my) screen", "read what's on screen" all map here too. Guarded so a
    // continuous "watch the screen" (handled above) never reaches this.
    let mentions_screen = lower.contains("screen") || lower.contains("display");
    let read_screen = (lower.contains("read") && mentions_screen)
        || (lower.contains("what") && lower.contains("on") && mentions_screen)
        || lower.contains("read this")
        || lower.contains("read that");
    if read_screen {
        return Some(op_read_screen(None));
    }
    None
}

/// Extract the control phrase from a "where is the <X> button / find the <X> /
/// locate the <X>" locate request, lowercased. Returns the trimmed phrase (e.g.
/// "submit", "sign in") or None when the utterance is not a where-is request.
/// PURE + unit-tested. READ-ONLY semantics: this only NAMES the control to
/// locate; nothing here (or downstream) clicks it.
fn extract_where_is_query(lower: &str) -> Option<String> {
    let is_locate = lower.contains("where is")
        || lower.contains("where's")
        || lower.contains("locate")
        || (lower.contains("find") && lower.contains("button"));
    if !is_locate {
        return None;
    }
    // Pull the phrase between a leading article and a trailing "button"/control
    // noun. Strip the locate verb + article, then drop a trailing control noun
    // so "where's the submit button" -> "submit", "find the sign in button" ->
    // "sign in", "locate the settings icon" -> "settings".
    let mut s = lower;
    for lead in [
        "where is the ", "where's the ", "where is ", "where's ", "locate the ",
        "locate ", "find the ", "find ",
    ] {
        if let Some(idx) = s.find(lead) {
            s = &s[idx + lead.len()..];
            break;
        }
    }
    let mut phrase = s.trim();
    for tail in [" button", " control", " icon", " field", " link", " tab", " menu", "?"] {
        if let Some(stripped) = phrase.strip_suffix(tail) {
            phrase = stripped.trim();
        }
    }
    // Also drop a lone trailing "button"/"on the screen" remnant.
    let phrase = phrase
        .trim_end_matches(|c: char| !c.is_alphanumeric())
        .trim();
    if phrase.is_empty() || phrase.len() > 64 {
        return None;
    }
    Some(phrase.to_string())
}

/// Whether an utterance is a Vision SCREEN-READ request (an OCR read of the
/// user's own screen). PUBLIC so the pipeline (main.rs) can keep the result
/// TRANSIENT: a screen read can surface on-screen passwords/messages, so its
/// utterance + acknowledgment must NOT seed lifelong memory (fact extraction)
/// or optimizer traces. Pure over `screen_read_op`, so this and the routing
/// agree by construction — anything that maps to a `read.screen` op is transient.
pub fn is_screen_read(text: &str) -> bool {
    let lower = text.to_lowercase();
    // The plain on-screen OCR read, PLUS the handwriting (#28) / document-scan
    // (#29) reads — all three surface SENSITIVE recognized text (a handwritten
    // note / a scanned page can carry private content just like an on-screen
    // password/message), so all three must be kept TRANSIENT (off lifelong memory
    // / optimizer traces). Agree-by-construction with the routing: anything that
    // maps to one of these ops is flagged transient here. The LUMEN read arm
    // (#45, "read me the buttons / what's on screen") is ALSO a screen read — it
    // surfaces the on-screen CONTROL labels — so it is unioned in for the same
    // transience (the ACT arm is NOT a read and is intentionally excluded).
    screen_read_op(&lower).is_some()
        || handwriting_document_op(&lower).is_some()
        || matches!(lumen_command(text), Some(LumenCommand::Read))
}

// ===========================================================================
// LUMEN (#45) — SCREEN-NARRATION + hands-free VOICE-NAVIGATION dispatch. Maps
//   (a) "read me the screen / the buttons / what's on screen" -> the READ-ONLY
//       Vision `read.screen` locate + Lumen's control narration (through the
//       speech path); the async readout is remembered (lumen::remember_readout at
//       integration) so a follow-up can select over it, AND
//   (b) "click / press / tap the <ordinal|name>" -> lumen::resolve_voice_action
//       over the remembered controls -> the UNCHANGED, per-action-gated
//       `ui_actuate` CAPSTONE (via anthropic::execute_tool, the SAME entry a live
//       tool call uses). Lumen only SELECTS the one target + builds the request;
//       the capstone still owns EVERY gate (the pure single-action planner, the
//       consequential spoken confirm PER ACTION, the master switch, voice-id, and
//       `!lockdown`). Lumen weakens, bypasses, and re-implements NONE of it.
//
// CONSERVATIVE by construction: the ACT arm anchors on unambiguous UI-actuation
// verbs (a bare "click"/"tap", or "press"/"push" WITH a control noun / ordinal —
// so "press play" / "push harder" never trip it); the READ arm requires a read
// verb WITH a screen/controls anchor and defers the where-is/locate/watch/scan/
// handwriting phrasings to the more-specific Vision ops.
// ===========================================================================

/// What a Lumen voice command resolves to. The READ arm forwards the READ-ONLY
/// screen locate + narrates; the ACT arm names the ONE target to actuate (the raw
/// phrase, which [`crate::lumen::resolve_voice_action`] parses over the remembered
/// controls).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LumenCommand {
    /// "read me the screen / the buttons / what's on screen" — READ-ONLY narrate.
    Read,
    /// "click / press the <ordinal|name>" — carries the lowercased phrase the
    /// selector parses over the remembered controls.
    Act(String),
}

/// Map a spoken utterance to a [`LumenCommand`], or None when it is neither a
/// Lumen read nor a Lumen actuation phrase (the turn falls through to the rest of
/// routing). PURE + deterministic so the mapping is unit-tested without a socket,
/// a running app, the OCR/AX locate, or the capstone. The ACT arm is checked
/// FIRST so "click the third button" (which mentions "button") is an actuation,
/// never a control read.
pub fn lumen_command(text: &str) -> Option<LumenCommand> {
    let lower = text.trim().to_lowercase();
    if lower.is_empty() {
        return None;
    }
    if lumen_is_act(&lower) {
        return Some(LumenCommand::Act(lower));
    }
    if lumen_is_read(&lower) {
        return Some(LumenCommand::Read);
    }
    None
}

/// Whether `lower` is a Lumen ACTUATION phrase. CONSERVATIVE: a bare strong verb
/// ("click"/"tap"/"double-click") counts on its own (these almost never appear in
/// ordinary speech), but the broader "press"/"push" count ONLY alongside a control
/// noun or an ordinal — so "press play" / "push harder" / "press on" never trip
/// it. A degenerate "click" with no target still routes here and is REFUSED
/// honestly by the selector (never a guess), which is the correct place to say so.
fn lumen_is_act(lower: &str) -> bool {
    // Only "click"/"double-click" are rare enough in ordinary speech to count BARE.
    // "tap"/"press"/"push" are common English ("tap water", "on tap", "press on",
    // "push harder"), so they count ONLY alongside a control noun or an ordinal.
    // "click"/"double-click" are rare enough in ordinary speech to count anywhere.
    if contains_word(lower, "click")
        || lower.contains("double click")
        || lower.contains("double-click")
    {
        return true;
    }
    // "tap" is common English ("tap water", "on tap", "tap out"), so it counts as a
    // command ONLY when it is the LEADING imperative ("tap Submit", "tap the third")
    // — never mid-sentence, so "is the tap water safe?" never triggers.
    let trimmed = lower.trim_start();
    if trimmed == "tap" || trimmed.starts_with("tap ") {
        return true;
    }
    // "press"/"push" (and a non-leading "tap") require a concrete UI target.
    let has_targeted_verb = contains_word(lower, "tap")
        || contains_word(lower, "press")
        || contains_word(lower, "push");
    if !has_targeted_verb {
        return false;
    }
    lumen_mentions_control_noun(lower) || lumen_mentions_ordinal(lower)
}

/// Whether `lower` is a Lumen READ (control-narration) phrase. Requires a read/
/// narrate/list verb (or a "what's on / what are" question) WITH a screen or
/// controls anchor. DEFERS the where-is/locate, watch, scan, handwriting, and
/// describe phrasings to the more-specific Vision ops (checked here so, even
/// though Lumen dispatch runs before Vision, those never get swallowed).
fn lumen_is_read(lower: &str) -> bool {
    let deferred = lower.contains("where is")
        || lower.contains("where's")
        || lower.contains("locate")
        || (lower.contains("find") && lower.contains("button"))
        || lower.contains("watch")
        || lower.contains("scan")
        || lower.contains("handwriting")
        || lower.contains("handwritten")
        || lower.contains("whiteboard")
        || lower.contains("white board")
        || lower.contains("describe");
    if deferred {
        return false;
    }
    let mentions_screen = lower.contains("screen") || lower.contains("display");
    let mentions_controls = lumen_mentions_control_noun(lower);
    let reads = lower.contains("read")
        || lower.contains("narrate")
        || lower.contains("list")
        || lower.contains("what's on")
        || lower.contains("what is on")
        || lower.contains("what are");
    reads && (mentions_screen || mentions_controls)
}

/// Whether `lower` names an on-screen CONTROL kind (button/link/tab/…). Used to
/// narrow the conservative act/read triggers. Substring-based (matches plurals).
fn lumen_mentions_control_noun(lower: &str) -> bool {
    ["button", "link", "tab", "checkbox", "check box", "field", "menu", "control", "icon"]
        .iter()
        .any(|n| lower.contains(n))
}

/// Whether any whitespace/punctuation-delimited token in `lower` is an ordinal —
/// a number word ("first".."tenth"), a digit+suffix ("1st".."10th"), or a short
/// bare number (an id/code-length digit run is deliberately NOT one). PURE.
fn lumen_mentions_ordinal(lower: &str) -> bool {
    const WORDS: &[&str] = &[
        "first", "second", "third", "fourth", "fifth", "sixth", "seventh", "eighth", "ninth",
        "tenth", "1st", "2nd", "3rd", "4th", "5th", "6th", "7th", "8th", "9th", "10th",
    ];
    lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .any(|t| {
            WORDS.contains(&t)
                || (t.len() <= 3 && t.chars().all(|c| c.is_ascii_digit()))
        })
}

/// Build the `ui_actuate` tool INPUT (its `UiActuateArgs` JSON) from a resolved
/// [`crate::ui_automation::ActuationRequest`] — the SAME shape a live tool call
/// carries, so [`anthropic::execute_tool`] plans + gates it through the UNCHANGED
/// capstone. `confirm` is deliberately OMITTED (defaults false): only the
/// confirmation gate's `force_confirm` ever sets it, never Lumen — so the request
/// can only ever PARK for a spoken yes, never self-authorize. PURE.
fn ui_actuate_input(req: &crate::ui_automation::ActuationRequest) -> serde_json::Value {
    use crate::ui_automation::Action;
    match &req.action {
        Action::Click { x, y } => {
            json!({"action": "click", "target": req.target_desc, "x": x, "y": y})
        }
        Action::Type { text } => json!({"action": "type", "target": req.target_desc, "text": text}),
        Action::Key { combo } => json!({"action": "key", "target": req.target_desc, "combo": combo}),
    }
}

// ===========================================================================
// VLM DESCRIBE — on-device VISION-LANGUAGE understanding (task #2, build 2/3).
//
// DISTINCT from the OCR `read.screen` intent above (OCR = reading the TEXT
// GLYPHS off the screen; VLM = REASONING about the visual scene). "Describe my
// screen" / "what am I looking at" / "describe this image <path>" routes to the
// VISION agent, captures a screen frame (reuses the Vision app's screen capture)
// OR takes a PATH-CONFINED user image path, and calls the inference
// `describe_image` op (an on-device mlx-vlm model). The image's pixels go ONLY
// to the on-device VLM — NEVER to the cloud, never off the device.
//
// DEVICE-GATED + ON by default but INERT WITHOUT A MODEL ([vision].enabled ships
// true, [vision].model ships empty): the VLM
// needs mlx-vlm + a multi-GB checkpoint + enough RAM, so when it is off / the
// model isn't named / isn't downloaded, the op honestly reports "unavailable"
// and the daemon FALLS BACK honestly (to the OCR read.screen path for a screen
// request, or an honest "the vision-language model isn't downloaded" line) —
// it NEVER fabricates a description. The actual description QUALITY is
// device/runtime-gated and is never claimed measured.
//
// PATH CONFINEMENT: a user image path is canonicalized and asserted to live
// under the allowed root (the project root) BEFORE it is ever handed to the op
// (symlink-escape / `..` / absolute-elsewhere are REJECTED) — mirrors the
// docsearch `confine` primitive exactly.
// ===========================================================================

/// What a "describe" request resolves to: describe the user's SCREEN (capture a
/// frame), or describe a specific user IMAGE at a path. The path here is the RAW
/// candidate the user named; it is PATH-CONFINED by the handler BEFORE any op
/// call (the parser never touches the disk, so it stays pure + unit-testable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DescribeRequest {
    /// "describe my screen" / "what am I looking at" — capture + describe a
    /// screen frame (reuses the Vision app's screen capture). `question` carries a
    /// SPECIFIC visual question for the VLM to answer ("what's the error on my
    /// screen?", "ask my screen which button rebuilds") — VQA; `None` = a generic
    /// description (the op applies its default describe prompt).
    Screen { question: Option<String> },
    /// "describe this image <path>" / "what's in <path>" — describe a specific
    /// image file (RAW candidate path; confined before the op). `question` = a
    /// specific question about the file ("in cat.png, is the cat asleep?");
    /// `None` = a generic description.
    Image { path: String, question: Option<String> },
}

/// Map a spoken utterance to a [`DescribeRequest`], or None when it is not a
/// VLM-describe request (the turn falls through to normal routing — including the
/// OCR `read.screen` path, which is DISTINCT). PURE + deterministic so the
/// mapping is unit-tested without a socket, a running app, the VLM, or the
/// classifier.
///
/// Recognized (case-insensitive, whole lowercased utterance):
///   - "describe this image <path>" / "what's in this picture <path>" /
///     "describe the photo <path>"                 -> Image(path)
///   - "describe my screen" / "what am I looking at" / "describe what's on my
///     screen" / "what do you make of my screen"   -> Screen
///
/// DISTINCT from OCR: "read my screen" / "what's on my screen" (text glyphs) is
/// handled by [`screen_read_op`] and is NOT a describe request. The describe
/// verbs ("describe", "what am I looking at", "what do you make of") never
/// overlap the OCR read verbs ("read", "what's on") — checked here so an OCR
/// phrase never lands on the VLM and vice versa.
pub fn describe_command(text: &str) -> Option<DescribeRequest> {
    let lower = text.to_lowercase();

    // An image-FILE describe ("describe this image ~/pics/cat.png", "what's in
    // photo.jpg"): a describe/what-is verb PLUS a token carrying an image
    // extension. Checked before the screen describe so a named file wins.
    let names_describe = lower.contains("describe")
        || lower.contains("what's in")
        || lower.contains("whats in")
        || lower.contains("what is in")
        || lower.contains("what am i looking at")
        || lower.contains("what do you make of")
        || lower.contains("what's this")
        || lower.contains("what is this");
    if names_describe {
        if let Some(path) = extract_image_path(text) {
            let question = vqa_question(text, Some(&path));
            return Some(DescribeRequest::Image { path, question });
        }
    }

    // An EXPLICIT screen-VQA trigger ("ask my screen <question>", "ask about my
    // screen <question>"). A dedicated, unambiguous form so a SPECIFIC visual
    // question ("ask my screen which button rebuilds") reaches the VLM even
    // without a "describe" verb. Begins with "ask <the screen>", so it cannot
    // collide with an OCR read ("read"/"what's on") or a Lumen control read, and
    // "ask <a person> ..." never matches (the object must be the screen/display).
    if let Some(q) = explicit_screen_vqa(&lower, text) {
        return Some(DescribeRequest::Screen { question: q });
    }

    // A SCREEN describe ("describe my screen", "what am I looking at",
    // "describe what's on my screen", "what do you make of my screen"). MUST be
    // a describe verb (NOT an OCR "read"/"what's on" verb): the VLM describes the
    // scene, the OCR path reads the text. "what am I looking at" with no image
    // file is a screen describe (the most natural hands-free phrasing). When the
    // utterance asks something SPECIFIC beyond the generic describe scaffolding
    // (e.g. "describe my screen — is there an error?"), that question is threaded
    // to the VLM (VQA); a bare "describe my screen" stays a generic caption.
    let mentions_screen =
        lower.contains("screen") || lower.contains("display") || lower.contains("looking at");
    let describe_screen = (lower.contains("describe") && mentions_screen)
        || lower.contains("what am i looking at")
        || (lower.contains("what do you make of") && mentions_screen);
    if describe_screen {
        let question = vqa_question(text, None);
        return Some(DescribeRequest::Screen { question });
    }

    None
}

/// The explicit screen-VQA trigger: an utterance that begins with "ask" whose
/// OBJECT is the screen/display ("ask my screen …", "ask about the display …").
/// Returns `Some(question)` when it matches — the `question` is the user's words
/// with the trigger prefix stripped (or `None` when nothing substantive follows,
/// which routes to a generic screen describe). Returns `None` (does not match)
/// otherwise. PURE. The prefix set is exhaustive on purpose: "ask <a person>
/// about the screen" never matches (it does not START with one of these), so a
/// message-a-contact intent is never poached.
fn explicit_screen_vqa(lower: &str, original: &str) -> Option<Option<String>> {
    const PREFIXES: &[&str] = &[
        "ask about my screen",
        "ask about the screen",
        "ask about my display",
        "ask about the display",
        "ask my screen",
        "ask the screen",
        "ask my display",
        "ask the display",
    ];
    let prefix = PREFIXES.iter().find(|p| lower.starts_with(**p))?;
    // Strip the matched prefix from the ORIGINAL-case text, then trim leading
    // filler/punctuation ("about", ":", ",", "-"). What remains is the question.
    let rest = original[prefix.len()..]
        .trim_start_matches(|c: char| c.is_whitespace() || matches!(c, ':' | ',' | '-' | '?' | '.'))
        .trim();
    if rest.is_empty() {
        // "ask my screen" with no question — a generic look at the screen.
        Some(None)
    } else {
        Some(Some(rest.to_string()))
    }
}

/// Extract the SPECIFIC visual question a describe utterance carries, or `None`
/// for a generic description. `path`, when present, is the recognized image-file
/// token — it is removed first so a file path never leaks into the VLM prompt.
///
/// Rule: after removing the path, tokenize the remainder; if EVERY token is
/// generic describe/scaffolding vocabulary ("describe", "my", "screen", "what",
/// "looking", …) the user only asked for a plain description -> `None` (the op
/// then uses its default prompt). If ANY token is substantive ("error", "button",
/// "dog", "asleep", …) the user asked something specific -> `Some(utterance)`,
/// passed verbatim so the VLM answers THAT. PURE + unit-tested without a model.
fn vqa_question(text: &str, path: Option<&str>) -> Option<String> {
    // Remove the path token (first occurrence, case-insensitive) from a working
    // copy; the returned question is built from the ORIGINAL text minus the path.
    // Remove the image-path token WITHOUT byte-offset math on the original: an
    // offset from `text.to_lowercase()` desyncs on any char whose lowercase form
    // has a different byte length (e.g. `İ`), which would panic replace_range on a
    // char boundary. Instead drop the whitespace token whose punctuation-trimmed
    // form equals the path (extract_image_path built the path exactly that way),
    // which is boundary-safe by construction. Also removes any punctuation
    // attached to the path token — fine for a VLM prompt.
    let stripped: String = match path {
        Some(p) if !p.is_empty() => {
            let pl = p.to_lowercase();
            text.split_whitespace()
                .filter(|tok| {
                    let trimmed = tok.trim_matches(|c: char| {
                        !c.is_alphanumeric()
                            && c != '.'
                            && c != '/'
                            && c != '_'
                            && c != '-'
                            && c != '~'
                    });
                    trimmed.to_lowercase() != pl
                })
                .collect::<Vec<_>>()
                .join(" ")
        }
        _ => text.to_string(),
    };
    let rest = stripped.trim();
    if rest.is_empty() {
        return None;
    }
    // Generic describe / scaffolding vocabulary. A remnant made ONLY of these is a
    // plain "just describe it" request (generic caption). Anything else is a
    // specific question the VLM should answer.
    const SCAFFOLD: &[&str] = &[
        "please", "can", "could", "would", "will", "you", "tell", "give", "show",
        "let", "me", "us", "for", "a", "an", "the", "to", "and", "so", "just",
        "describe", "description", "what", "whats", "s", "is", "are", "in", "of",
        "on", "at", "am", "i", "looking", "look", "do", "does", "make", "makes",
        "see", "seeing", "this", "that", "these", "those", "it", "here", "there",
        "my", "your", "screen", "display", "monitor", "image", "picture", "photo",
        "pic", "photograph", "snapshot", "now", "right", "currently",
    ];
    let has_substantive = rest
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .any(|t| !SCAFFOLD.contains(&t.as_str()));
    if has_substantive {
        // Collapse whitespace runs (a stripped path leaves a gap) so the VLM
        // prompt is clean — and, by construction, carries no file path.
        Some(rest.split_whitespace().collect::<Vec<_>>().join(" "))
    } else {
        None
    }
}

/// Extract an image file path/name from a describe phrase. Returns the token
/// that carries a known image extension (png/jpg/jpeg/gif/webp/heic/bmp/tiff),
/// taken from the ORIGINAL-case text (file systems are case-sensitive) via a
/// case-insensitive extension match. None when no such token is present (a bare
/// "describe this image"). Pure — never touches the disk; the confinement +
/// existence check happen in the handler.
fn extract_image_path(text: &str) -> Option<String> {
    text.split(|c: char| c.is_whitespace())
        .map(|w| {
            w.trim_matches(|c: char| {
                !c.is_alphanumeric() && c != '.' && c != '/' && c != '_' && c != '-' && c != '~'
            })
        })
        .find(|w| {
            let lw = w.to_lowercase();
            lw.ends_with(".png")
                || lw.ends_with(".jpg")
                || lw.ends_with(".jpeg")
                || lw.ends_with(".gif")
                || lw.ends_with(".webp")
                || lw.ends_with(".heic")
                || lw.ends_with(".bmp")
                || lw.ends_with(".tiff")
                || lw.ends_with(".tif")
        })
        .map(|w| w.to_string())
}

/// Whether an utterance is a VLM-DESCRIBE request (visual understanding via the
/// on-device VLM). PUBLIC so the pipeline (main.rs) can keep its result
/// TRANSIENT exactly like an OCR screen read: a describe of the screen / a
/// private photo can surface sensitive visual content, so its utterance +
/// acknowledgment must NOT seed lifelong memory or optimizer traces. Pure over
/// [`describe_command`], so this and the routing agree by construction.
pub fn is_describe_request(text: &str) -> bool {
    describe_command(text).is_some()
}

/// Honest copy when the on-device VLM is unavailable for a SCREEN describe — the
/// daemon falls back to the OCR `read.screen` path (it can still read the text on
/// screen). Kept as a function so the handler and tests share the exact wording.
fn describe_screen_fallback_copy(reason: &str) -> String {
    format!(
        "I can't describe the scene right now, sir — {reason}. I'll read the text on your \
         screen instead; the visual-description model runs on-device and isn't set up yet."
    )
}

/// Honest copy when the on-device VLM is unavailable for an IMAGE describe (there
/// is no OCR fallback for an arbitrary file, so we state the gate plainly).
fn describe_image_fallback_copy(reason: &str) -> String {
    format!(
        "I can't describe that image, sir — {reason}. The vision-language model runs \
         entirely on-device and isn't downloaded yet, so I won't guess at what's in it."
    )
}

/// Execute a VLM-describe request. Routes to the VISION agent (the caller
/// re-pins it). The image is read ON-DEVICE by the inference `describe_image` op;
/// pixels NEVER leave the device. Returns persona-voiced converse data
/// (`llm_voice`), exactly like the OCR / app-op handlers.
///
/// GATES + FALLBACK (honesty-first):
///   * [vision].enabled OFF or [vision].model EMPTY: the VLM is not set up — the
///     daemon does NOT call the op. A SCREEN request falls back to the OCR
///     read.screen path (it can still read the text); an IMAGE request reports
///     the gate honestly. NEVER a fabricated description.
///   * A user IMAGE path is PATH-CONFINED (canonicalize + under the allowed
///     root) BEFORE the op call; an escape (symlink/`..`/absolute-elsewhere) or
///     a nonexistent path is REJECTED with an honest message (never sent).
///   * The op itself returns [`DescribeOutcome::Unavailable`] when mlx-vlm /
///     the checkpoint isn't present (the server's "vlm_unavailable") — the daemon
///     falls back honestly on that too.
///
/// Emits a `vision.describe` telemetry event carrying ONLY the source kind +
/// availability + latency bucket — NEVER any pixels or the description text.
async fn handle_describe(
    req: DescribeRequest,
    cfg: &Config,
    infer: &mut InferenceClient,
    app_registry: &Arc<AppRegistry>,
    allowed_root: &Path,
) -> HandlerOutput {
    // GATE: the VLM is OFF or no model is named -> do not call the op; fall back
    // honestly. The two reasons are distinct so the spoken copy is honest about
    // which gate is closed.
    let gate_reason: Option<&str> = if !cfg.vision.enabled {
        Some("the on-device vision-language model is turned off")
    } else if cfg.vision.model.trim().is_empty() {
        Some("no on-device vision-language model is configured")
    } else {
        None
    };

    // Whether the user asked a SPECIFIC question (VQA) vs a generic describe. Only
    // the boolean is emitted below — never the question text (it can name what is
    // on the most-sensitive surface, the screen).
    let is_vqa = matches!(
        &req,
        DescribeRequest::Screen { question: Some(_) }
            | DescribeRequest::Image { question: Some(_), .. }
    );

    let (source, available, data) = match req {
        DescribeRequest::Screen { question } => {
            if let Some(reason) = gate_reason {
                // Honest fall back to OCR: forward the read.screen op so the user
                // still gets the on-screen TEXT (best-effort; an op error is itself
                // reported honestly by handle_vision's send_op path).
                let ocr = handle_vision(
                    VisionCommand::Op(op_read_screen(None)),
                    app_registry,
                )
                .await;
                let copy = format!(
                    "{}\n\n{}",
                    describe_screen_fallback_copy(reason),
                    ocr.data
                );
                ("screen", false, copy)
            } else {
                // The VLM is configured + on. Capture a screen frame by forwarding
                // the Vision app's capture op (reusing its ScreenCaptureKit path),
                // then describe it (answering the user's specific `question` when
                // one was asked — VQA — else a generic caption). The captured frame
                // is the Vision app's to produce on-device; the daemon never holds
                // the pixels. The frame path is the app's confined capture output.
                match capture_screen_frame(app_registry, allowed_root).await {
                    Ok(frame) => describe_confined_path(
                        &frame,
                        question.as_deref(),
                        infer,
                        allowed_root,
                        "screen",
                    )
                    .await
                    .unwrap_or_else(|reason| {
                        ("screen", false, describe_screen_fallback_copy(&reason))
                    }),
                    Err(reason) => {
                        // Couldn't get a frame — fall back to the OCR read path.
                        let ocr = handle_vision(
                            VisionCommand::Op(op_read_screen(None)),
                            app_registry,
                        )
                        .await;
                        let copy = format!(
                            "{}\n\n{}",
                            describe_screen_fallback_copy(&reason),
                            ocr.data
                        );
                        ("screen", false, copy)
                    }
                }
            }
        }
        DescribeRequest::Image { path: raw_path, question } => {
            if let Some(reason) = gate_reason {
                ("image", false, describe_image_fallback_copy(reason))
            } else {
                match describe_confined_path(
                    Path::new(&raw_path),
                    question.as_deref(),
                    infer,
                    allowed_root,
                    "image",
                )
                .await
                {
                    Ok(out) => out,
                    Err(reason) => ("image", false, describe_image_fallback_copy(&reason)),
                }
            }
        }
    };

    // TELEMETRY: source kind + availability + nothing visual. No pixels, no
    // description text, no path — the event proves the wiring ran without leaking
    // what was seen (the visual content is the most sensitive thing in this op).
    telemetry::emit(
        "local",
        "vision.describe",
        json!({"source": source, "available": available, "vlm": cfg.vision.enabled, "vqa": is_vqa}),
    );

    HandlerOutput {
        data,
        llm_voice: true,
    }
}

/// PATH-CONFINE `candidate` under `allowed_root`, then call the on-device
/// `describe_image` op. On success returns `Ok((source, true, description))`;
/// on a confinement reject / a missing path / the op's UNAVAILABLE arm / a
/// transport error it returns `Err(honest_reason)` so the caller renders the
/// right fall-back copy. NEVER returns a fabricated description.
async fn describe_confined_path(
    candidate: &Path,
    question: Option<&str>,
    infer: &mut InferenceClient,
    allowed_root: &Path,
    source: &'static str,
) -> std::result::Result<(&'static str, bool, String), String> {
    // PATH CONFINEMENT (the security primitive, mirrors docsearch::confine):
    // canonicalize the candidate + assert it resolves under the canonicalized
    // allowed root. A symlink-escape / `..` / absolute-elsewhere / nonexistent
    // path is REJECTED here — the path is NEVER handed to the op.
    let canon_root = match std::fs::canonicalize(allowed_root) {
        Ok(r) => r,
        Err(_) => return Err("I couldn't resolve a safe location to read the image from".to_string()),
    };
    let confined = crate::docsearch::confine(candidate, std::slice::from_ref(&canon_root));
    let Some(real) = confined else {
        return Err(
            "that image isn't in a folder I'm allowed to read from, so I won't open it"
                .to_string(),
        );
    };

    // Clamp the decode budget defensively at the daemon boundary too (the client
    // also clamps); None lets the client apply the shared default + cap.
    match infer.describe_image(&real, question, None).await {
        Ok(DescribeOutcome::Available { text, model }) => {
            info!(source = source, model = %model, "vlm describe ok");
            // The DESCRIPTION is the spoken data; the model id is non-secret. The
            // text is the model's VISUAL understanding — distinct from OCR glyphs.
            Ok((source, true, text))
        }
        Ok(DescribeOutcome::Unavailable { error }) => {
            // The op reported the device-gated unavailable path (or a caller-bug
            // ValueError). Honest fall back — never a fabricated description.
            warn!(source = source, reason = %error, "vlm describe unavailable; falling back");
            Err(error)
        }
        Err(e) => {
            // Transport failure (inference server down). Honest fall back.
            warn!(source = source, error = %e, "vlm describe transport error; falling back");
            telemetry::emit(
                "system",
                "inference.unavailable",
                json!({"op": "describe_image", "error": e.to_string()}),
            );
            Err("the inference server isn't reachable".to_string())
        }
    }
}

// ===========================================================================
// On-device TEXT->IMAGE GENERATION (task #18) — DISTINCT from the VLM describe
// path above (describe = reasoning ABOUT an image; generate = rendering a NEW
// image from a text prompt). "generate / make / draw / create an image of X"
// routes to the VISION agent (the visual-capability owner, same as describe) and
// calls the inference `generate_image` op (an on-device MLX diffusion model). The
// PROMPT and the generated PIXELS go ONLY to the on-device model and the image is
// saved on-device under state/images/ — NEVER to the cloud, never off the device
// (there is NO cloud image API anywhere on this path).
//
// DEVICE-GATED + ON by default but INERT WITHOUT A MODEL ([image].enabled ships
// true, [image].model ships empty): the diffusion
// model needs an MLX package + a multi-GB checkpoint + enough RAM, so when it is
// off / the model isn't named / isn't downloaded, the op honestly reports
// "image_model_unavailable" and the daemon surfaces an honest "the on-device
// image model isn't set up" line — it NEVER fabricates an image and NEVER falls
// back to a cloud image API. The actual image QUALITY/speed are device/runtime-
// gated and are never claimed measured.
// ===========================================================================

/// A parsed "generate an image of X" request: the extracted image PROMPT (the
/// subject after the generate verb). PURE + deterministic so the mapping is
/// unit-tested without a socket, the diffusion model, or the classifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerateImageRequest {
    pub prompt: String,
}

/// Map a spoken utterance to a [`GenerateImageRequest`], or None when it is not
/// an image-generation request (the turn falls through to normal routing —
/// including the VLM DESCRIBE path, which is DISTINCT: describe reasons ABOUT an
/// existing image; generate renders a NEW one). PURE + deterministic.
///
/// Recognized (case-insensitive): a GENERATE verb ("generate" / "make" / "draw"
/// / "create" / "paint" / "render") applied to "an image / a picture / a photo /
/// a drawing / a painting / art of <X>", and the shorthand "image of <X>". The
/// SUBJECT after "of"/"showing"/"depicting" (or after the image-noun) becomes the
/// prompt. A describe verb ("describe", "what's in") is NOT a generate verb, so
/// the two intents never collide.
pub fn generate_image_command(text: &str) -> Option<GenerateImageRequest> {
    let lower = text.to_lowercase();

    // DISTINCT from the VLM describe path: a describe/what-is verb is never an
    // image-GENERATION request (describe reasons about an EXISTING image).
    if describe_command(text).is_some() {
        return None;
    }

    // A GENERATE verb must be present — the act of creating a new image.
    const GEN_VERBS: &[&str] = &[
        "generate", "make", "draw", "create", "paint", "render", "imagine", "sketch",
    ];
    let has_gen_verb = GEN_VERBS.iter().any(|v| lower.contains(v));

    // An IMAGE noun anchors the request to a picture (so "make me a sandwich"
    // never reads as image generation). The noun is also where the prompt begins.
    const IMAGE_NOUNS: &[&str] = &[
        "image", "picture", "photo", "drawing", "painting", "illustration", "artwork", "art ",
    ];
    let has_image_noun = IMAGE_NOUNS.iter().any(|n| lower.contains(n));
    if !has_gen_verb || !has_image_noun {
        return None;
    }

    // Extract the SUBJECT (the prompt) from the ORIGINAL-case text so the user's
    // phrasing survives. Prefer the explicit "of/showing/depicting <X>" tail; the
    // first such connector AFTER an image noun is where the subject begins.
    if let Some(prompt) = extract_image_prompt(text) {
        if !prompt.trim().is_empty() {
            return Some(GenerateImageRequest { prompt: prompt.trim().to_string() });
        }
    }
    None
}

/// Extract the image PROMPT (subject) from a generate phrase, in ORIGINAL case.
/// Takes the tail after the first subject connector ("of"/"showing"/"depicting"/
/// "that shows"/"with") — e.g. "draw a picture of a red bicycle" -> "a red
/// bicycle". None when there is no connector (a bare "generate an image" with no
/// subject), which the caller treats as "no prompt" rather than guessing. Pure —
/// never touches the disk or the network.
fn extract_image_prompt(text: &str) -> Option<String> {
    // The subject connectors, longest first so "that shows" wins over a bare
    // "shows" overlap. All ASCII, so a case-insensitive byte compare is exact.
    const CONNECTORS: &[&str] = &[" that shows ", " depicting ", " showing ", " of ", " with "];
    // Locate the EARLIEST connector by scanning `text`'s CHAR boundaries directly
    // and comparing each candidate window case-insensitively (ASCII). This yields
    // a `start` that is always a valid char boundary IN `text`. The earlier
    // `lower.find()` approach returned a byte offset into `text.to_lowercase()`,
    // which can differ from `text` whenever a char's lowercase form has a
    // different byte length (e.g. Turkish 'İ' U+0130 -> "i̇") — slicing `text`
    // with that mismatched offset could land mid-codepoint (or past the end) and
    // PANIC the whole daemon on an STT transcript carrying such a character.
    let bytes = text.as_bytes();
    let mut best_start: Option<usize> = None;
    for (i, _) in text.char_indices() {
        for c in CONNECTORS {
            let cb = c.as_bytes(); // connectors are ASCII
            if i + cb.len() <= bytes.len() && bytes[i..i + cb.len()].eq_ignore_ascii_case(cb) {
                // ASCII connector -> `i + cb.len()` is a valid char boundary.
                let tail = i + cb.len();
                // Prefer the EARLIEST connector so "a picture of X with Y" keeps
                // the full "X with Y" subject rather than starting at " with ".
                if best_start.is_none_or(|b| tail < b) {
                    best_start = Some(tail);
                }
            }
        }
    }
    let start = best_start?;
    Some(text[start..].to_string())
}

/// Whether an utterance is an image-GENERATION request. PUBLIC so the pipeline
/// (main.rs) can keep its result TRANSIENT exactly like a VLM describe — a
/// generated image (and its prompt) can be personal, so its utterance +
/// acknowledgment must NOT seed lifelong memory or optimizer traces. Pure over
/// [`generate_image_command`], so this and the routing agree by construction.
pub fn is_generate_image_request(text: &str) -> bool {
    generate_image_command(text).is_some()
}

/// Honest copy when the on-device image model is unavailable (off / no model
/// named / not downloaded / a runtime failure). There is NO cloud fallback — the
/// daemon states the gate plainly and never fabricates an image. Kept as a
/// function so the handler and tests share the exact wording.
fn generate_image_unavailable_copy(reason: &str) -> String {
    format!(
        "I can't generate that image, sir — {reason}. The image model runs entirely \
         on-device and isn't set up yet, so I won't invent a picture or send your \
         prompt to the cloud."
    )
}

/// Execute an image-GENERATION request. Routes to the VISION agent (the caller
/// re-pins it). The prompt is handed ONLY to the on-device `generate_image` op
/// (MLX diffusion) and the image is saved ON-DEVICE under state/images/; the
/// prompt + pixels NEVER leave the device — there is NO cloud image API. Returns
/// persona-voiced converse data (`llm_voice`), exactly like the describe handler.
///
/// GATES + FALLBACK (honesty-first):
///   * [image].enabled OFF or [image].model EMPTY: the model is not set up — the
///     daemon does NOT call the op and surfaces the gate honestly. NEVER a
///     fabricated image, NEVER a cloud call.
///   * The op itself returns [`GenerateOutcome::Unavailable`] when the diffusion
///     package / checkpoint isn't present (the server's "image_model_unavailable")
///     — the daemon surfaces that honestly too (NO cloud fallback).
///
/// Emits an `image.generated` telemetry event carrying ONLY availability + the
/// saved ON-DEVICE path + the NON-secret model/size/steps metadata — NEVER the
/// prompt and NEVER any pixels, and NEVER over the network (the telemetry sink is
/// the local HUD).
async fn handle_generate_image(
    req: GenerateImageRequest,
    cfg: &Config,
    infer: &mut InferenceClient,
) -> HandlerOutput {
    // GATE: the image model is OFF or no model is named -> do not call the op;
    // surface the gate honestly. The two reasons are distinct so the spoken copy
    // is honest about which gate is closed.
    let gate_reason: Option<&str> = if !cfg.image.enabled {
        Some("on-device image generation is turned off")
    } else if cfg.image.model.trim().is_empty() {
        Some("no on-device image-generation model is configured")
    } else {
        None
    };

    let (available, saved_path, model, size, steps, data) = if let Some(reason) = gate_reason {
        // OFF / unconfigured: never reach the op. Honest gate line, no cloud call.
        (false, None, None, None, None, generate_image_unavailable_copy(reason))
    } else {
        // Configured + on: call the on-device op. None for size/steps/seed lets
        // the server apply its defaults (the client also clamps any explicit ask).
        match infer.generate_image(&req.prompt, None, None, None).await {
            // The NON-secret `seed` is intentionally ignored: the daemon never
            // surfaces it spoken and never forwards it anywhere off-device.
            Ok(GenerateOutcome::Available { path, model, size, steps, seed: _ }) => {
                info!(model = %model, size, steps, "image generated on-device");
                // The SAVED ON-DEVICE PATH is what the user gets — the image stays
                // on the machine. The spoken data names the local path (never the
                // pixels, never the prompt back to the cloud).
                let copy = format!(
                    "Done, sir — I generated that image on-device and saved it to {}. \
                     The prompt and the picture stayed on this machine; nothing went to the cloud.",
                    path.display()
                );
                (
                    true,
                    Some(path.display().to_string()),
                    Some(model),
                    Some(size),
                    Some(steps),
                    copy,
                )
            }
            Ok(GenerateOutcome::Unavailable { error }) => {
                // The op reported the device-gated unavailable path (or a caller-bug
                // ValueError). Honest surface — never a fabricated image, never a
                // cloud fallback.
                warn!(reason = %error, "image generation unavailable; reporting honestly (no cloud)");
                (false, None, None, None, None, generate_image_unavailable_copy(&error))
            }
            Err(e) => {
                // Transport failure (inference server down). Honest surface — still
                // NO cloud fallback.
                warn!(error = %e, "image generation transport error; reporting honestly (no cloud)");
                telemetry::emit(
                    "system",
                    "inference.unavailable",
                    json!({"op": "generate_image", "error": e.to_string()}),
                );
                (
                    false,
                    None,
                    None,
                    None,
                    None,
                    generate_image_unavailable_copy("the inference server isn't reachable"),
                )
            }
        }
    };

    // TELEMETRY: availability + the saved ON-DEVICE path + NON-secret model/size/
    // steps metadata. NEVER the prompt, NEVER any pixels, and NEVER over the
    // network — the event proves the wiring ran (and where the image landed on the
    // device) without leaking what was asked for or generated. The HUD reads this
    // to render the local-image readout / the unavailable state.
    telemetry::emit(
        "local",
        "image.generated",
        json!({
            "available": available,
            "path": saved_path,
            "model": model,
            "size": size,
            "steps": steps,
            "image": cfg.image.enabled,
        }),
    );

    HandlerOutput {
        data,
        llm_voice: true,
    }
}

/// Capture ONE screen frame for the VLM by forwarding the Vision app's screen
/// capture op (reusing its ScreenCaptureKit path — pixels stay in the app's
/// process / on-device). Returns the confined frame path the app wrote, or an
/// honest reason on failure (Vision not running, capture not consented, no
/// frame produced). DEVICE/TCC-GATED: the daemon forwards the op; the on-device
/// consent + the actual capture are the app's to perform.
///
/// HONESTY: the daemon does not itself open the screen — it asks the running
/// Vision micro-app (which owns the capture + the TCC consent) to produce a
/// frame under the project root, then path-confines that frame before the op.
async fn capture_screen_frame(
    app_registry: &Arc<AppRegistry>,
    allowed_root: &Path,
) -> std::result::Result<std::path::PathBuf, String> {
    // The frame the Vision app writes for a VLM describe, under the project
    // state dir (an allowlisted root). The op asks the app to capture + save one
    // frame here; the app owns the on-device capture + TCC consent.
    let frame = allowed_root.join("state").join("vision").join("describe-frame.png");
    let op = json!({
        "type": "op",
        "op": "describe.capture",
        "path": frame.display().to_string(),
    })
    .to_string();
    apps::send_op(app_registry, VISION_APP, &op)
        .await
        .map_err(|e| format!("I couldn't reach Vision to capture your screen ({e})"))?;
    // The capture is asynchronous + TCC-gated; the frame may not exist yet / at
    // all without consent. Confinement + existence are re-checked by the caller's
    // describe_confined_path, but a fast existence check here gives an honest
    // "no frame" reason rather than a confinement reject message.
    if !frame.exists() {
        return Err("the screen frame wasn't captured (Screen Recording consent is needed on-device)".to_string());
    }
    Ok(frame)
}

// ===========================================================================
// AUDIO SCENE UNDERSTANDING — on-device Sound Analysis (task #15, build 2/3).
//
// DISTINCT from STT (speech-to-text). STT answers "what did someone SAY" (words);
// this answers "what was that SOUND" (a doorbell, an alarm, glass breaking, a
// dog, music) via Apple Sound Analysis — the built-in ~300-class
// SNClassifierIdentifier.version1, on-device/ANE-eligible. The two never overlap:
// the STT path transcribes the user's utterance into the router; this path takes
// an ALREADY-CAPTURED audio CLIP (the daemon's VAD/cpal buffer, written to a WAV
// the SAME way an utterance is) and hands it to the Vision app's `classify.sound`
// op, which returns the top sound CLASSES.
//
// PRIVACY / HONESTY:
//   * ONLY the sound-class LABELS (+ confidence) ever leave the op — the AUDIO
//     never leaves the device (the op reads the local clip; the daemon never
//     ships the clip anywhere; the telemetry carries labels only, never samples).
//   * The classifier knows a FIXED ~300 classes — NOT "any sound". An unknown /
//     too-short / undecodable clip yields the op's honest `no_sound_classes`
//     vision.error, never a fabricated label.
//   * The one-shot "what was that sound" intent runs on a clip the daemon ALREADY
//     has — it opens NO new microphone. CONTINUOUS ambient monitoring is the
//     SEPARATE opt-in [audio].sound_monitor path (OFF + pinned, TCC/mic-gated,
//     never always-on without consent — see `ambient_monitor_should_start`).
// ===========================================================================

/// An "identify this sound" turn: the SOUND-identify intent fired, carrying the
/// clip to classify (the daemon's last captured segment, supplied by the caller)
/// — or `None` when there is no clip, so the handler reports that honestly rather
/// than the turn silently falling through to a generic answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentifySoundRequest {
    /// The already-captured clip to classify, or None when the daemon has none.
    /// NEVER user-named, never a fresh capture — no microphone is opened to fill it.
    pub clip: Option<PathBuf>,
}

/// Map a spoken utterance to an [`IdentifySoundRequest`], or None when it is not a
/// sound-identify request (the turn falls through to normal routing). PURE +
/// deterministic so the mapping is unit-tested without a socket, a running app,
/// the classifier, or a microphone.
///
/// The clip is the daemon's most-recent captured audio segment — supplied by the
/// caller (`latest_clip`), NOT named by the user — so this never opens the mic: it
/// classifies sound the daemon ALREADY heard. When the intent fires but there is
/// no clip, the request still routes (with `clip: None`) so the handler answers
/// honestly ("no recent clip") instead of guessing.
///
/// Recognized (case-insensitive, whole lowercased utterance) — a SOUND-identify
/// verb, never a SPEECH/transcription verb (STT stays distinct):
///   - "what was that sound" / "what was that noise" / "what's that sound" /
///     "identify that sound" / "what am i hearing" / "what do you hear" /
///     "what sound was that" / "name that sound"
fn identify_sound_clip_or_request(
    text: &str,
    latest_clip: Option<&Path>,
) -> Option<IdentifySoundRequest> {
    if !is_identify_sound_request(text) {
        return None;
    }
    // The clip is the daemon's last captured segment — never user-named, never a
    // fresh capture. None => no clip to classify (the handler reports it honestly).
    Some(IdentifySoundRequest {
        clip: latest_clip.map(|p| p.to_path_buf()),
    })
}

/// Whether the utterance is an "identify this sound" request — a SOUND-scene
/// query, DISTINCT from STT (speech). PUBLIC so the pipeline (main.rs) can keep
/// this turn's handling consistent with the other transient perception reads.
/// Pure over the same recognition `identify_sound_clip` uses, so the predicate
/// and the routing agree by construction.
///
/// Guarded so a SPEECH-transcription phrasing ("what did I/he/she/they say",
/// "transcribe", "what did you hear me say") NEVER lands here — that is the STT
/// path's job. The trigger is a SOUND/NOISE/HEAR verb with no "say"/"said"/
/// "transcribe"/"word" speech cue.
pub fn is_identify_sound_request(text: &str) -> bool {
    let lower = text.to_lowercase();

    // STT VETO: a speech-transcription phrasing is the STT path, never this one.
    // "what did <someone> say", "transcribe", "what were the words" must fall
    // through so the sound-scene classifier never shadows speech understanding.
    const SPEECH_CUES: &[&str] = &[
        " say", " said", "transcribe", "transcription", " words", " spoken", "what did i ", "what did you hear me",
    ];
    if SPEECH_CUES.iter().any(|c| lower.contains(c)) {
        return false;
    }

    // SOUND-identify phrasings. A "sound"/"noise" object with an identify/what-was
    // verb, or a bare "what am i hearing" / "what do you hear" (hearing a SOUND,
    // not parsing speech — the speech veto above already removed "hear me say").
    let mentions_sound = lower.contains("sound") || lower.contains("noise");
    let identify_verb = lower.contains("what was that")
        || lower.contains("what's that")
        || lower.contains("what is that")
        || lower.contains("what was")
        || lower.contains("identify")
        || lower.contains("name that")
        || lower.contains("what kind of");
    if mentions_sound && identify_verb {
        return true;
    }
    // Bare hearing queries (no "sound" word needed): "what am I hearing", "what
    // do you hear", "what are you hearing". Speech ("hear me say") was vetoed.
    lower.contains("what am i hearing")
        || lower.contains("what do you hear")
        || lower.contains("what are you hearing")
        || lower.contains("what's that i hear")
}

/// The confined clip path the daemon writes (or already wrote) for a one-shot
/// sound classification, under the project state dir (an allowlisted root). This
/// mirrors the utterance-WAV location the VAD/cpal capture loop uses — the clip
/// the daemon ALREADY captured — so no new microphone is opened to answer "what
/// was that sound". The handler path-confines this before the op exactly like a
/// describe frame.
fn sound_clip_path(root: &Path) -> PathBuf {
    root.join("state").join("tmp").join("sound-clip.wav")
}

/// Execute an "identify this sound" request: PATH-CONFINE the already-captured
/// clip, forward the Vision app's on-device `classify.sound` op, and surface the
/// top sound classes. Routes to the VISION agent (the caller re-pins it). ONLY
/// the sound-class LABELS leave the op; the AUDIO never leaves the device — the
/// daemon hands the op a LOCAL clip path and never ships the audio anywhere.
///
/// HONESTY-FIRST:
///   * No clip to classify (`clip` is None) -> say so plainly; never fabricate a
///     label and never open the mic to make one.
///   * The clip path is PATH-CONFINED under the allowed root BEFORE the op
///     (symlink-escape / `..` / absolute-elsewhere / nonexistent are REJECTED) —
///     mirrors the describe-frame confinement.
///   * The recognized classes arrive ASYNCHRONOUSLY on the `vision.sound`
///     telemetry event (relayed to the HUD by the app relay), NEVER in this
///     synchronous reply — so the acknowledgment is content-free about the labels.
///   * On an empty/too-short/undecodable clip the op emits the honest
///     `no_sound_classes` vision.error — the daemon never invents a class.
async fn handle_identify_sound(
    clip: Option<PathBuf>,
    app_registry: &Arc<AppRegistry>,
    allowed_root: &Path,
) -> HandlerOutput {
    let data = match clip {
        None => {
            // Nothing captured to classify — honest, no mic opened, no guess.
            "I don't have a recent sound clip to identify, sir. The sound classifier \
             runs on-device over audio I've already captured — it never opens the mic on its own."
                .to_string()
        }
        Some(candidate) => {
            // PATH CONFINEMENT (the security primitive, mirrors describe + docsearch::
            // confine): canonicalize + assert the clip resolves under the allowed
            // root. An escape / nonexistent clip is REJECTED — never sent to the op.
            match std::fs::canonicalize(allowed_root)
                .ok()
                .and_then(|canon_root| {
                    crate::docsearch::confine(&candidate, std::slice::from_ref(&canon_root))
                }) {
                None => {
                    "That sound clip isn't in a folder I'm allowed to read from, sir, so I won't classify it."
                        .to_string()
                }
                Some(real) => {
                    let op = op_classify_sound(&real.display().to_string());
                    match apps::send_op(app_registry, VISION_APP, &op).await {
                        Ok(()) => {
                            info!(app = VISION_APP, op = %op, "forwarded classify.sound op");
                            // TELEMETRY: the wiring ran. LABELS-ONLY by construction —
                            // the actual classes ride the async vision.sound relay
                            // (the app emits {label,confidence} only; the audio never
                            // leaves the device). This event carries NO audio, NO clip
                            // samples, NO path — just that the on-device classify ran.
                            telemetry::emit(
                                "local",
                                "audio.sound",
                                json!({
                                    "op": "classify.sound",
                                    "classifier": "SNClassifierIdentifier.version1",
                                    "labels_only": true,
                                    "audio_left_device": false,
                                }),
                            );
                            "Listening back on that now, sir — the sound classes will appear on the Vision panel. \
                             It's on-device Apple Sound Analysis, so only the labels surface; the audio never leaves the Mac."
                                .to_string()
                        }
                        Err(e) => {
                            warn!(app = VISION_APP, op = %op, error = %e, "classify.sound forward failed");
                            format!("I couldn't reach Vision to classify that sound: {e}. Open it first, sir.")
                        }
                    }
                }
            }
        }
    };
    HandlerOutput {
        data,
        llm_voice: true,
    }
}

/// PURE gate for the ambient sound monitor (task #15). The monitor
/// PERIODICALLY classifies ambient audio + emits sound-class events ONLY when
/// `[audio].sound_monitor` is on. Factored out so the "inert without consent + never
/// auto-starts the mic" invariant is unit-testable without a clock, a mic, or a spawn.
///
/// Returns `true` (the monitor may start) ONLY when `[audio].sound_monitor` is
/// true (the SHIPPED default is true, but INERT WITHOUT mic/TCC consent). With it
/// false this returns false: the monitor NEVER starts, the mic is never opened for
/// ambient classification, and the audio path is byte-for-byte today's. macOS mic/TCC
/// consent is a SEPARATE on-device gate the daemon cannot grant — even when this
/// returns true, the actual ambient capture is device-gated and is NOT exercised
/// here (the one-shot intent + this gate are what the tests cover).
///
/// PRIVACY: continuous ambient listening without explicit consent is a liability
/// — so the ONLY path to a running monitor is this opt-in switch. There is no
/// tool/agent/model route that can flip it (it lives in the user-owned config),
/// and no default-on / auto-arm anywhere.
pub fn ambient_monitor_should_start(sound_monitor_enabled: bool) -> bool {
    sound_monitor_enabled
}

// ===========================================================================
// Nexus voice control (SPEC §6 — the daemon forwards STRUCTURED ops ONLY; the
// Nexus app never parses natural language).
//
// Nexus (apps/nexus) is a PYTHON control plane hosting a native Rust DSP core.
// Its HOST -> APP op wire form is the BARE `{"op":"<name>", ...}` object (NOT
// the `{"type":"op",...}` envelope Vision uses) — its OpDispatcher in
// apps/nexus/main.py reads `msg["op"]` and dispatches on the dotted name. The
// op-string builders below produce that EXACT wire shape, matching the SPEC §5
// op table and the dispatch handlers verbatim:
//   gain.set   {"op":"gain.set","channel":N,"mute":bool,"stage":"input"}  (mute)
//   gain.set   {"op":"gain.set","channel":N,"gain_db":F,"stage":"input"|"output"}
//   route.set  {"op":"route.set","in":N,"out":M,"gain_db":F}
//   monitor.set{"op":"monitor.set","in":N,"out":M,"on":bool}
//   preset.load{"op":"preset.load","name":"<name>"}
//   state.get  {"op":"state.get"}
// serde_json builds each line so a preset name with a quote can never break the
// JSON framing. The classifier is checked alongside the Silicon Canvas / Vision
// seams, before the generic local handlers, so a precise audio-control phrase is
// handled deterministically and never lands on the cloud/LLM.
//
// The realtime CoreAudio path is DEVICE-GATED and is NEVER touched here: these
// ops are control-plane messages to the Python host; whether a device is bound
// is the app's concern. The daemon only classifies the utterance and forwards
// the structured op — it opens no audio device and plays no audio.
// ===========================================================================

/// The Nexus micro-app's registered name (its manifest `[app].name` and the key
/// into the app registry / its socket).
pub const NEXUS_APP: &str = "nexus";

/// The Nexus monitor bus output index. "route input 1 to the monitor" / "mute
/// the mic" need a default output and a default input to be actionable without
/// the user naming channel numbers. Output 0 is the monitor bus and input 0 is
/// the SM7dB mic by the SPEC §3 gain-staging convention (the mic is the primary
/// input; the monitor is the direct-monitor output). These are the targets a
/// bare "the mic" / "the monitor" resolves to; an explicit "input N" / "output
/// M" in the utterance overrides them.
const NEXUS_MONITOR_OUT: u32 = 0;
const NEXUS_MIC_INPUT: u32 = 0;

/// What a Nexus voice command resolves to: LAUNCH the app, or forward a
/// STRUCTURED op line to the already-running app. The op body is opaque to the
/// daemon (built to match apps/nexus/main.py's OpDispatcher wire form).
#[derive(Debug, Clone, PartialEq)]
pub enum NexusCommand {
    /// "open nexus" — start the micro-app.
    Launch,
    /// A complete JSON op line (one line) to forward verbatim, e.g.
    /// `{"op":"gain.set","channel":0,"mute":true,"stage":"input"}`.
    Op(String),
}

/// Whether the utterance names the Nexus app / capability itself ("nexus", "the
/// matrix", "the routing matrix", "the mixer"). Used to gate the bare launch
/// verb so an unrelated "open safari" is never captured.
fn mentions_nexus(lower: &str) -> bool {
    contains_word(lower, "nexus")
        || lower.contains("routing matrix")
        || lower.contains("the audio matrix")
        || lower.contains("the mixer")
        || lower.contains("the routing grid")
}

/// Map a spoken utterance to a Nexus command, or None when it is not a Nexus
/// control phrase (the turn then falls through to normal routing). Deterministic
/// and pure so the mapping is unit-tested without a socket, a running app, or
/// the classifier. Order matters: the specific ops (mute, route, gain, monitor,
/// preset, levels) are matched before the broad "open nexus" launch so a control
/// phrase that also says "open" is never mistaken for a launch.
///
/// Recognized phrases (all case-insensitive, whole lowercased utterance):
///   - "mute/unmute the mic" / "mute input N"          -> gain.set {mute}
///   - "set input/output gain to <dB>" /
///     "set the gain on input N to <dB>"               -> gain.set {gain_db}
///   - "route input N to the monitor/output M" /
///     "unroute input N from output M"                 -> route.set {gain_db|-inf}
///   - "monitor input N" / "stop monitoring"           -> monitor.set {on}
///   - "load the <name> preset" / "load preset <name>" -> preset.load {name}
///   - "what are the levels" / "show me the meters" /
///     "what's the matrix / routing state"             -> state.get
///   - "open/launch/start/bring up nexus"              -> Launch
pub fn nexus_command(text: &str) -> Option<NexusCommand> {
    let lower = text.to_lowercase();

    // --- mute / unmute (specific verb; before gain/route/launch) -----------
    // "mute the mic", "unmute input 2", "mute the microphone".
    if lower.contains("mute") {
        let unmute = lower.contains("unmute") || lower.contains("un-mute");
        let channel = extract_channel(&lower, "input").unwrap_or(NEXUS_MIC_INPUT);
        return Some(NexusCommand::Op(op_gain_mute(channel, !unmute)));
    }

    // --- gain set ----------------------------------------------------------
    // "set input gain to -18", "set the gain on output 1 to -3 dB", "turn the
    // mic gain down to -24". Requires an explicit dB value to be a gain.set.
    if lower.contains("gain") || lower.contains("trim") {
        if let Some(gain_db) = extract_db(&lower) {
            // Stage: "output" if the utterance names an output, else input
            // (the SM7dB chain trims the input by default — SPEC §3).
            let (stage, channel) = if mentions_output(&lower) {
                ("output", extract_channel(&lower, "output").unwrap_or(NEXUS_MONITOR_OUT))
            } else {
                ("input", extract_channel(&lower, "input").unwrap_or(NEXUS_MIC_INPUT))
            };
            return Some(NexusCommand::Op(op_gain_set(channel, gain_db, stage)));
        }
    }

    // --- route / unroute ---------------------------------------------------
    // "route input 1 to the monitor", "route input 2 to output 3", "unroute
    // input 1 from the monitor". A "route … to the monitor" without an explicit
    // output targets the monitor bus.
    if (lower.contains("route") || lower.contains("send") || lower.contains("patch"))
        && (lower.contains("input") || lower.contains("monitor") || lower.contains("output"))
    {
        let clear = lower.contains("unroute")
            || lower.contains("un-route")
            || lower.contains("clear")
            || lower.contains("disconnect")
            || (lower.contains("from") && !lower.contains(" to "));
        let input = extract_channel(&lower, "input").unwrap_or(NEXUS_MIC_INPUT);
        // The destination output: an explicit "output M", else the monitor bus
        // when "monitor" is named, else the monitor bus as the sensible default.
        let output = extract_channel(&lower, "output").unwrap_or(NEXUS_MONITOR_OUT);
        // 0 dB unity on connect; -inf clears the crosspoint (SPEC §5 route.set).
        let gain_db = if clear { f64::NEG_INFINITY } else { 0.0 };
        return Some(NexusCommand::Op(op_route_set(input, output, gain_db)));
    }

    // --- monitor on/off ----------------------------------------------------
    // "monitor input 1", "stop monitoring", "turn off the monitor". This is the
    // direct-monitor route toggle (SPEC §5 monitor.set), distinct from a generic
    // crosspoint route above (which already matched if "route"/"send" was said).
    if lower.contains("monitor") {
        let off = lower.contains("stop")
            || lower.contains("turn off")
            || lower.contains("disable")
            || lower.contains("no longer")
            || lower.contains("unmonitor");
        let input = extract_channel(&lower, "input").unwrap_or(NEXUS_MIC_INPUT);
        let output = extract_channel(&lower, "output").unwrap_or(NEXUS_MONITOR_OUT);
        return Some(NexusCommand::Op(op_monitor_set(input, output, !off)));
    }

    // --- preset load -------------------------------------------------------
    // "load the vocal preset", "load preset podcast", "recall the streaming
    // preset". Only LOAD (preset.save is a panel/manual action, not voiced).
    if (lower.contains("load") || lower.contains("recall") || lower.contains("apply"))
        && lower.contains("preset")
    {
        if let Some(name) = extract_preset_name(&lower) {
            return Some(NexusCommand::Op(op_preset_load(&name)));
        }
    }

    // --- state / levels query ----------------------------------------------
    // "what are the levels", "show me the meters", "what's the routing state",
    // "read out the matrix". A read-only snapshot request (SPEC §5 state.get).
    // "matrix" is a routing snapshot ONLY in a Nexus/routing context — a bare
    // "matrix" (e.g. "the matrix movie") is conversational and must fall
    // through, so it is gated on a routing/read co-word or a Nexus mention.
    let matrix_state_query = lower.contains("matrix")
        && !mentions_nexus_launch_verb(&lower)
        && (mentions_nexus(&lower)
            || lower.contains("rout")
            || lower.contains("read out")
            || lower.contains("read me")
            || lower.contains("state")
            || lower.contains("crosspoint"));
    if lower.contains("level")
        || lower.contains("meter")
        || matrix_state_query
        || lower.contains("routing state")
        || lower.contains("route state")
        || (lower.contains("what") && lower.contains("routed"))
    {
        return Some(NexusCommand::Op(op_state_get()));
    }

    // --- launch ------------------------------------------------------------
    // Only when the utterance actually names Nexus AND carries an open-class
    // verb — "open nexus", "bring up the routing matrix". Last so a control
    // phrase that also says "open" was already handled above.
    if mentions_nexus(&lower) && mentions_nexus_launch_verb(&lower) {
        return Some(NexusCommand::Launch);
    }

    None
}

/// Whether the utterance carries an open-class verb (used both to gate the Nexus
/// launch and to keep "open the matrix" from being read as a state query).
fn mentions_nexus_launch_verb(lower: &str) -> bool {
    lower.contains("open")
        || lower.contains("launch")
        || lower.contains("start")
        || lower.contains("bring up")
        || lower.contains("fire up")
        || lower.contains("show")
}

/// Whether the utterance names an OUTPUT channel (so a gain.set targets the
/// output stage rather than the default input). "output", "out", "speaker(s)",
/// "headphone(s)", "monitor" all name the output side.
fn mentions_output(lower: &str) -> bool {
    contains_word(lower, "output")
        || contains_word(lower, "speaker")
        || contains_word(lower, "speakers")
        || contains_word(lower, "headphone")
        || contains_word(lower, "headphones")
}

/// Extract the integer channel index following a `kind` keyword ("input" /
/// "output"), e.g. "input 1" -> 1, "output 3" -> 3. Returns None when the
/// keyword is absent or no number follows it — the caller then falls back to the
/// sensible default (the mic input / the monitor output). The number is taken as
/// spoken: Nexus indexes channels from 0, and the SM7dB mic is input 0, so a
/// user saying "input 1" means index 1 by the same convention the panel shows.
fn extract_channel(lower: &str, kind: &str) -> Option<u32> {
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    // Walk every occurrence of the keyword; the first one followed by a number
    // wins ("on input 2" and "input 2" both resolve).
    for (i, w) in words.iter().enumerate() {
        if *w == kind {
            if let Some(next) = words.get(i + 1) {
                if let Ok(n) = next.parse::<u32>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// Extract a decibel value from a "set … gain to <X>" phrase. Accepts a signed
/// integer or float, with or without a "db"/"dB" suffix, and handles a spoken
/// "minus"/"negative" prefix ("minus 18", "negative 6 db") since speech-to-text
/// often spells the sign. Returns None when no numeric value is present (so a
/// gainless "set the gain" is not a gain.set and falls through). Not clamped —
/// the engine's set_*_trim is the authority on the valid range (SPEC §1:
/// -inf..+12 dB), and forwarding the spoken value verbatim keeps the daemon out
/// of the DSP policy.
///
/// The dB value is the number after the "to"/"at" target preposition when one is
/// present ("set the gain on input 1 to -12" -> -12, never the channel "1"); a
/// channel index spoken AFTER the preposition is impossible since the target is
/// the value itself. With no preposition, the first numeric token is taken (a
/// bare "gain -6" form).
fn extract_db(lower: &str) -> Option<f64> {
    // Normalize a spoken sign word into a leading '-' so "minus 18" parses, and
    // drop the dB suffix words so they don't fuse onto the number.
    let normalized = lower
        .replace("minus ", "-")
        .replace("negative ", "-")
        .replace("db", " ")
        .replace("decibels", " ")
        .replace("decibel", " ");
    let toks: Vec<&str> = normalized.split(|c: char| c.is_whitespace()).collect();
    // The window to search: everything after the LAST "to"/"at" target word when
    // present, so a channel number before it ("input 1 to -12") is excluded.
    let start = toks
        .iter()
        .rposition(|w| {
            let t = w.trim_matches(|c: char| !c.is_alphanumeric());
            t == "to" || t == "at"
        })
        .map(|i| i + 1)
        .unwrap_or(0);
    for tok in &toks[start..] {
        let t = tok.trim_matches(|c: char| !(c.is_ascii_digit() || c == '-' || c == '.' || c == '+'));
        if t.is_empty() || t == "-" || t == "+" || t == "." {
            continue;
        }
        if let Ok(n) = t.parse::<f64>() {
            return Some(n);
        }
    }
    None
}

/// Extract the preset name from a "load the <name> preset" / "load preset
/// <name>" phrase. Returns the content word adjacent to "preset" (the token
/// before it, or after it when "preset" leads), stripped of the article. The
/// name is forwarded verbatim in the op — Nexus resolves it against its
/// presets/ directory (and rejects an unknown one cleanly). None when no name
/// can be isolated.
fn extract_preset_name(lower: &str) -> Option<String> {
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .filter(|w| !w.is_empty())
        .collect();
    let pos = words.iter().position(|w| *w == "preset")?;
    // Command/filler words that are never a preset name.
    let is_name = |w: &str| {
        !matches!(
            w,
            "the" | "a" | "an" | "my" | "load" | "recall" | "apply" | "preset"
                | "presets" | "please" | "to" | "for" | "me" | "up"
        )
    };
    // Prefer the token AFTER "preset" ("load preset vocal"); else the token
    // BEFORE it ("load the vocal preset").
    if let Some(after) = words.get(pos + 1) {
        if is_name(after) {
            return Some((*after).to_string());
        }
    }
    if pos > 0 {
        // Walk back over articles to the name token ("load the vocal preset").
        let mut idx = pos - 1;
        loop {
            let w = words[idx];
            if is_name(w) {
                return Some(w.to_string());
            }
            if idx == 0 {
                break;
            }
            idx -= 1;
        }
    }
    None
}

// The op-string builders — EXACT Nexus OpDispatcher wire form (bare `{"op":...}`,
// NOT the Vision `{"type":"op"}` envelope). serde_json builds each so a preset
// name with a quote can never break the JSON framing.

fn op_gain_mute(channel: u32, mute: bool) -> String {
    json!({"op": "gain.set", "channel": channel, "mute": mute, "stage": "input"}).to_string()
}
fn op_gain_set(channel: u32, gain_db: f64, stage: &str) -> String {
    json!({"op": "gain.set", "channel": channel, "gain_db": gain_db, "stage": stage}).to_string()
}
fn op_route_set(input: u32, output: u32, gain_db: f64) -> String {
    // -inf clears the crosspoint (SPEC §5); JSON has no infinity literal, so the
    // string sentinel "-inf" is forwarded — the Python _route_set maps it back to
    // float("-inf") verbatim (it special-cases the "-inf" string).
    let gain: serde_json::Value = if gain_db.is_infinite() && gain_db.is_sign_negative() {
        serde_json::Value::String("-inf".to_string())
    } else {
        json!(gain_db)
    };
    json!({"op": "route.set", "in": input, "out": output, "gain_db": gain}).to_string()
}
fn op_monitor_set(input: u32, output: u32, on: bool) -> String {
    json!({"op": "monitor.set", "in": input, "out": output, "on": on}).to_string()
}
fn op_preset_load(name: &str) -> String {
    json!({"op": "preset.load", "name": name}).to_string()
}
fn op_state_get() -> String {
    json!({"op": "state.get"}).to_string()
}

// ===========================================================================
// Mark-Forge voice control (SPEC §7 — the daemon forwards STRUCTURED ops ONLY;
// the Mark-Forge engine never parses natural language).
//
// Mark-Forge (apps/mark-forge) is a BINARY micro-app: a deterministic CPU/f64
// rigid-body physics engine. Its HOST -> APP op wire form is the BARE
// `{"op":"<name>", ...}` object (NOT the Vision `{"type":"op",...}` envelope) —
// its `parse_command` (apps/mark-forge/src/ipc.rs) reads `obj["op"]` and the
// `#[serde(tag = "op")]` Op enum dispatches on the dotted name. The op-string
// builders below produce that EXACT wire shape, matching the SPEC §7 op table
// and the app's own `op_deserializes_with_dotted_names` /
// `body_spawn_deserializes_with_optional_fields` round-trip tests verbatim:
//   world.reset {"op":"world.reset"}
//   body.spawn  {"op":"body.spawn","shape":{"kind":"cuboid","half_extents":[..]},"pos":[x,y,z]}
//   world.step  {"op":"world.step","n":N}
//   set.gravity {"op":"set.gravity","x":F,"y":F,"z":F}
//   state.get   {"op":"state.get"}
// serde_json builds each line so no field can break the JSON framing. The
// classifier is checked alongside the Silicon Canvas / Vision / Nexus seams,
// before the generic local handlers, so a precise physics-control phrase is
// handled deterministically and never lands on the cloud/LLM.
//
// The R3F render is DEVICE-GATED and is NEVER touched here: these ops are
// control-plane messages to the headless engine; whether the HUD is rendering is
// the HUD's concern. The daemon only classifies the utterance and forwards the
// structured op — it opens no GPU device and renders nothing.
// ===========================================================================

/// The Mark-Forge micro-app's registered name (its manifest `[app].name` and the
/// key into the app registry / its socket).
pub const MARK_FORGE_APP: &str = "mark-forge";

/// Where a freshly-dropped body appears: a few metres above the origin so it
/// falls onto the ground plane under gravity (the canonical "drop" gesture). The
/// engine resolves the rest via its integrator; the daemon only seeds the spawn.
const MARK_FORGE_DROP_HEIGHT: f64 = 5.0;
/// Half-extent of a dropped cuboid (a 1m unit cube) and radius of a dropped
/// sphere — sane defaults the user never has to speak. Forwarded verbatim in the
/// spawn op; the engine derives mass/inertia from the shape.
const MARK_FORGE_BOX_HALF_EXTENT: f64 = 0.5;
const MARK_FORGE_SPHERE_RADIUS: f64 = 0.5;
/// Default dynamic-body mass for a dropped shape. `Some(mass)` (> 0) makes it
/// dynamic; the engine would treat `None`/`<= 0` as a STATIC body (SpawnSpec),
/// which would never fall — so a "drop" must carry a positive mass.
const MARK_FORGE_DROP_MASS: f64 = 1.0;
/// Lunar surface gravity (m/s², downward) for "set gravity to the moon". A fixed
/// physical constant — never an RNG/wall-clock read — so the op stays
/// deterministic.
const MARK_FORGE_MOON_GRAVITY: f64 = -1.62;
/// Earth surface gravity (m/s², downward) for "set gravity to earth" / "normal
/// gravity". Matches the engine's own default so "reset gravity" restores it.
const MARK_FORGE_EARTH_GRAVITY: f64 = -9.81;
/// Mars surface gravity (m/s², downward) for "set gravity to mars".
const MARK_FORGE_MARS_GRAVITY: f64 = -3.72;
/// Zero gravity for "turn off gravity" / "set gravity to zero" / "space".
const MARK_FORGE_ZERO_GRAVITY: f64 = 0.0;

/// What a Mark-Forge voice command resolves to: LAUNCH the app, or forward a
/// STRUCTURED op line to the already-running engine. The op body is opaque to the
/// daemon (built to match apps/mark-forge/src/ipc.rs's `Op` wire form).
#[derive(Debug, Clone, PartialEq)]
pub enum MarkForgeCommand {
    /// "open the physics sandbox" — start the micro-app.
    Launch,
    /// A complete JSON op line (one line) to forward verbatim, e.g.
    /// `{"op":"world.reset"}`.
    Op(String),
}

/// Whether the utterance names the Mark-Forge app / capability itself ("mark
/// forge", "the physics sandbox", "the simulation", "the sandbox"). Used to gate
/// the bare launch verb so an unrelated "open safari" is never captured, and to
/// disambiguate "reset"/"pause" so they only fire in a physics context.
fn mentions_mark_forge(lower: &str) -> bool {
    lower.contains("mark forge")
        || lower.contains("mark-forge")
        || lower.contains("markforge")
        || lower.contains("physics sandbox")
        || lower.contains("physics sim")
        || lower.contains("physics engine")
        || lower.contains("the simulation")
        || lower.contains("the sandbox")
        || lower.contains("rigid body")
        || lower.contains("rigid-body")
}

/// Whether the utterance carries an open-class launch verb.
fn mentions_mark_forge_launch_verb(lower: &str) -> bool {
    lower.contains("open")
        || lower.contains("launch")
        || lower.contains("start")
        || lower.contains("bring up")
        || lower.contains("fire up")
        || lower.contains("show")
}

/// Map a spoken utterance to a Mark-Forge command, or None when it is not a
/// physics-control phrase (the turn then falls through to normal routing).
/// Deterministic and pure so the mapping is unit-tested without a socket, a
/// running engine, or the classifier. Order matters: the specific ops (spawn,
/// reset, gravity, step/pause) are matched before the broad "open the physics
/// sandbox" launch so a control phrase that also says "open" is never mistaken
/// for a launch.
///
/// Recognized phrases (all case-insensitive, whole lowercased utterance):
///   - "drop/spawn/add a box|cube"                  -> body.spawn {cuboid}
///   - "drop/spawn/add a ball|sphere"               -> body.spawn {sphere}
///   - "reset/clear the simulation|sandbox|world"   -> world.reset
///   - "set gravity to the moon|mars|earth|zero" /
///     "turn off gravity"                           -> set.gravity {x,y,z}
///   - "step" / "step <N> frames" / "advance"       -> world.step {n>=1}
///   - "pause" / "hold" / "freeze"                  -> world.step {n:0}
///   - "open/launch/start the physics sandbox"      -> Launch
pub fn mark_forge_command(text: &str) -> Option<MarkForgeCommand> {
    let lower = text.to_lowercase();

    // --- spawn (drop/add a box or ball) ------------------------------------
    // The spawn verb plus a shape noun. Checked first so "drop a box" is never
    // read as anything else. A ball/sphere noun -> sphere; otherwise a box/cube
    // noun -> cuboid. The verb alone with no shape noun is NOT a spawn (it falls
    // through), so "drop it" / "drop everything" never spawns a phantom body.
    let spawn_verb = lower.contains("drop")
        || lower.contains("spawn")
        || lower.contains("add a ")
        || lower.contains("add an ")
        || lower.contains("throw");
    if spawn_verb {
        if mentions_word(&lower, "ball")
            || mentions_word(&lower, "balls")
            || mentions_word(&lower, "sphere")
            || mentions_word(&lower, "spheres")
            || mentions_word(&lower, "marble")
        {
            return Some(MarkForgeCommand::Op(op_spawn_sphere()));
        }
        if mentions_word(&lower, "box")
            || mentions_word(&lower, "boxes")
            || mentions_word(&lower, "cube")
            || mentions_word(&lower, "cubes")
            || mentions_word(&lower, "crate")
            || mentions_word(&lower, "block")
        {
            return Some(MarkForgeCommand::Op(op_spawn_box()));
        }
    }

    // --- world reset -------------------------------------------------------
    // "reset/clear the simulation|world|sandbox|scene|bodies". Gated on a
    // physics co-word (or a Mark-Forge mention) so a bare "reset" in another
    // context never wipes the world. "reset gravity" is NOT a reset (it is a
    // gravity op) — handled by requiring a world/scene noun and excluding the
    // gravity case, which the gravity branch below also catches first if it has
    // a target.
    if (lower.contains("reset") || lower.contains("clear") || lower.contains("wipe"))
        && !lower.contains("gravity")
        && (mentions_mark_forge(&lower)
            || mentions_word(&lower, "world")
            || mentions_word(&lower, "scene")
            || mentions_word(&lower, "bodies")
            || mentions_word(&lower, "everything"))
    {
        return Some(MarkForgeCommand::Op(op_world_reset()));
    }

    // --- gravity -----------------------------------------------------------
    // "set gravity to the moon|mars|earth|zero", "turn off gravity", "moon
    // gravity". Requires the word "gravity" so it never fires on an unrelated
    // "moon"/"mars". The target body picks the constant; an unrecognized target
    // with a bare "set gravity" falls through (the daemon won't guess a vector).
    if lower.contains("gravity") {
        if let Some(y) = gravity_target(&lower) {
            return Some(MarkForgeCommand::Op(op_set_gravity(y)));
        }
    }

    // --- step / pause ------------------------------------------------------
    // The engine has no free-running loop: it advances ONLY on world.step{n}.
    // "step"/"advance" -> step exactly N frames (N from the utterance, default
    // 1); "pause"/"freeze"/"hold" -> world.step{n:0}, a deterministic zero-frame
    // step that advances no simulated time (an honest "pause" for an engine that
    // is already paused between steps). Both are gated on a physics context so a
    // bare "pause" elsewhere is untouched.
    let physics_ctx = mentions_mark_forge(&lower)
        || mentions_word(&lower, "simulation")
        || mentions_word(&lower, "sim")
        || mentions_word(&lower, "world")
        || mentions_word(&lower, "physics")
        || mentions_word(&lower, "frame")
        || mentions_word(&lower, "frames");
    if (lower.contains("step") || lower.contains("advance"))
        && (physics_ctx || lower.contains("step the") || lower.contains("frame"))
    {
        let n = extract_step_count(&lower).unwrap_or(1);
        return Some(MarkForgeCommand::Op(op_world_step(n)));
    }
    if (mentions_word(&lower, "pause")
        || mentions_word(&lower, "freeze")
        || mentions_word(&lower, "hold")
        || lower.contains("halt"))
        && physics_ctx
    {
        return Some(MarkForgeCommand::Op(op_world_step(0)));
    }

    // --- launch ------------------------------------------------------------
    // Only when the utterance actually names Mark-Forge / the sandbox AND carries
    // an open-class verb. Last so a control phrase that also says "open" was
    // already handled above.
    if mentions_mark_forge(&lower) && mentions_mark_forge_launch_verb(&lower) {
        return Some(MarkForgeCommand::Launch);
    }

    None
}

/// Whole-word token check for Mark-Forge phrases (reuses the same boundary rule
/// as the other seams' `contains_word`): `word` matches only as a standalone
/// alnum token, so "box" never fires inside "boxer" and "sim" never inside
/// "simple".
fn mentions_word(lower: &str, word: &str) -> bool {
    lower
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| w == word)
}

/// The downward gravity magnitude a "set gravity to <target>" phrase selects, or
/// None when no recognized target is named (a bare "set gravity" with no body
/// then falls through rather than the daemon guessing a vector). The targets are
/// fixed physical constants; "off"/"zero"/"space" -> 0, "moon"/"mars"/"earth" ->
/// their surface gravity.
fn gravity_target(lower: &str) -> Option<f64> {
    if mentions_word(lower, "off")
        || mentions_word(lower, "zero")
        || mentions_word(lower, "none")
        || mentions_word(lower, "space")
        || lower.contains("turn off")
        || lower.contains("no gravity")
        || lower.contains("zero g")
        || lower.contains("weightless")
    {
        return Some(MARK_FORGE_ZERO_GRAVITY);
    }
    if mentions_word(lower, "moon") || mentions_word(lower, "lunar") {
        return Some(MARK_FORGE_MOON_GRAVITY);
    }
    if mentions_word(lower, "mars") || mentions_word(lower, "martian") {
        return Some(MARK_FORGE_MARS_GRAVITY);
    }
    if mentions_word(lower, "earth")
        || mentions_word(lower, "normal")
        || mentions_word(lower, "default")
    {
        return Some(MARK_FORGE_EARTH_GRAVITY);
    }
    None
}

/// Extract the frame count from "step <N> frames" / "advance <N>" / "step N".
/// Returns the first standalone integer token, or None (the caller then defaults
/// to a single frame). Caps at a sane bound so a misheard huge number cannot ask
/// the engine to advance millions of frames in one synchronous call.
fn extract_step_count(lower: &str) -> Option<u32> {
    const MAX_STEP_FRAMES: u32 = 10_000;
    lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .find_map(|w| w.parse::<u32>().ok())
        .map(|n| n.clamp(1, MAX_STEP_FRAMES))
}

// The op-string builders — EXACT Mark-Forge `Op` wire form (bare `{"op":...}`,
// the `#[serde(tag = "op")]` dotted names). serde_json builds each so no field
// can break the JSON framing. The shape sub-object is tagged on `kind`
// (snake_case) and `pos`/`half_extents` serialize as `[x,y,z]` arrays — exactly
// what apps/mark-forge/src/ipc.rs deserializes (verified by its own round-trip
// tests).

fn op_world_reset() -> String {
    json!({"op": "world.reset"}).to_string()
}
fn op_world_step(n: u32) -> String {
    json!({"op": "world.step", "n": n}).to_string()
}
fn op_set_gravity(y: f64) -> String {
    json!({"op": "set.gravity", "x": 0.0, "y": y, "z": 0.0}).to_string()
}
fn op_spawn_box() -> String {
    json!({
        "op": "body.spawn",
        "shape": {
            "kind": "cuboid",
            "half_extents": [
                MARK_FORGE_BOX_HALF_EXTENT,
                MARK_FORGE_BOX_HALF_EXTENT,
                MARK_FORGE_BOX_HALF_EXTENT
            ]
        },
        "pos": [0.0, MARK_FORGE_DROP_HEIGHT, 0.0],
        "mass": MARK_FORGE_DROP_MASS
    })
    .to_string()
}
fn op_spawn_sphere() -> String {
    json!({
        "op": "body.spawn",
        "shape": {"kind": "sphere", "radius": MARK_FORGE_SPHERE_RADIUS},
        "pos": [0.0, MARK_FORGE_DROP_HEIGHT, 0.0],
        "mass": MARK_FORGE_DROP_MASS
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        ambient_monitor_should_start, arg_str, classify_app_request, clear_roll_call_interrupt,
        cloud_model, conversation_brain, describe_command, describe_confined_path, enforce_tool,
        local_model_for_turn, local_sub_for_turn,
        extract_app_name, extract_content_words, extract_image_path, extract_web_query,
        extract_image_prompt, generate_image_command, handle_describe, handle_generate_image,
        vqa_question,
        handle_identify_sound, identify_sound_clip_or_request,
        interrupt_roll_call, is_describe_request, is_generate_image_request,
        is_identify_sound_request, is_screen_read,
        is_uncertain_fallback, lumen_command, mark_forge_command, nexus_command, op_classify_sound,
        recent_replies, ui_actuate_input,
        select_agent, silicon_canvas_command, sound_clip_path, suggests_web, utterance_wants_open,
        vision_command, wants_cloud, wants_quit, AppRequest, ConversationBrain, DescribeRequest,
        GenerateImageRequest, IdentifySoundRequest, LumenCommand, MarkForgeCommand, NexusCommand,
        SiliconCanvasCommand, VisionCommand,
        MARK_FORGE_APP, NEXUS_APP, ROLL_CALL_CANCEL, SILICON_CANVAS_APP, VISION_APP,
    };
    use crate::agents::AgentRegistry;
    use crate::config::Config;
    use crate::inference::{Classification, InferenceClient};
    use serde_json::json;
    use std::sync::atomic::Ordering;

    /// A classifier verdict for routing-decision tests: confident enough to
    /// stay local unless `complexity` forces cloud.
    fn classification(intent: &str, complexity: &str, confidence: f64) -> Classification {
        Classification {
            intent: intent.to_string(),
            complexity: complexity.to_string(),
            confidence,
            args: serde_json::Value::Null,
        }
    }

    /// CONTRACT B routing-decision table (the conversation-specific brain), now
    /// resolved through the model-tier layer. With NO override, the decision table
    /// is preserved at the config default for a HEAVY turn (the tier that keeps the
    /// configured default): cloud_heavy -> Opus, cloud_fast -> Haiku, no key/local/
    /// unknown -> local. Pure — no live cloud call, no inference client. (The auto
    /// step-down for a trivial turn and the override precedence are covered in
    /// model_tier.rs's own tests and the new conversation tests below.)
    #[test]
    fn conversation_brain_decision_table() {
        let _guard = crate::model_tier::OverrideGuard::force(None);
        let mut cfg = Config::default();
        // heavy_model/fast_model are the shipped contract ids.
        assert_eq!(cfg.cloud.heavy_model, "claude-opus-4-8");
        assert_eq!(cfg.cloud.fast_model, "claude-haiku-4-5");
        // A heavy, confident conversation turn keeps the configured default tier.
        let heavy = classification("conversation", "heavy", 0.95);

        // Default route is cloud_heavy: with a key, a heavy turn -> Opus cloud.
        assert_eq!(cfg.router.conversation_route, "cloud_heavy");
        assert_eq!(
            conversation_brain(&cfg, true, &heavy).0,
            ConversationBrain::Cloud("claude-opus-4-8".to_string())
        );
        // No key: even cloud_heavy degrades to the local 4B (Fallback).
        assert_eq!(conversation_brain(&cfg, false, &heavy).0, ConversationBrain::Local);

        // cloud_fast + key -> Haiku cloud; no key -> local.
        cfg.router.conversation_route = "cloud_fast".to_string();
        // A LIGHT turn under cloud_fast stays Fast (Haiku); a heavy one escalates
        // to Heavy. Use a light turn here to lock the cloud_fast -> Haiku mapping.
        let light = classification("conversation", "light", 0.95);
        assert_eq!(
            conversation_brain(&cfg, true, &light).0,
            ConversationBrain::Cloud("claude-haiku-4-5".to_string())
        );
        assert_eq!(conversation_brain(&cfg, false, &light).0, ConversationBrain::Local);

        // Explicit local: the resident 4B regardless of the key.
        cfg.router.conversation_route = "local".to_string();
        assert_eq!(conversation_brain(&cfg, true, &heavy).0, ConversationBrain::Local);
        assert_eq!(conversation_brain(&cfg, false, &heavy).0, ConversationBrain::Local);

        // Unknown value falls back to the safe, always-available local path.
        cfg.router.conversation_route = "wat".to_string();
        assert_eq!(conversation_brain(&cfg, true, &heavy).0, ConversationBrain::Local);
    }

    /// THRESHOLD finding 1 — GUEST = LOCAL-ONLY. A guest turn must NEVER reach the
    /// owner's PAID cloud (a cloud call appends an obol spend row + bumps the owner's
    /// daily budget — a durable, owner-readable trace — and egresses the guest's turn
    /// under the owner's API key). The fix forces a guest local at the SAME two seams
    /// vault uses; proven here at the cloud-vs-local decision the seams compute, with
    /// the composition `guest OR vault -> local`. The owner path is byte-for-byte
    /// unchanged (still cloud by default).
    #[test]
    fn a_guest_turn_is_forced_local_only_never_the_owners_paid_cloud() {
        let _guard = crate::model_tier::OverrideGuard::force(None);
        let cfg = Config::default(); // conversation_route defaults to cloud_heavy
        assert_eq!(cfg.router.conversation_route, "cloud_heavy");
        let heavy = classification("conversation", "heavy", 0.95);

        // OWNER path (no guest scope): a reachable-cloud turn is UNCHANGED — the seam
        // composition passes it through, and the conversation brain picks the cloud.
        assert!(
            crate::threshold::deny_cloud(crate::vault::deny_cloud(true)),
            "owner: a reachable cloud turn is passed through unchanged"
        );
        let owner_reachable = crate::threshold::deny_cloud(crate::vault::deny_cloud(true));
        assert!(
            matches!(conversation_brain(&cfg, owner_reachable, &heavy).0, ConversationBrain::Cloud(_)),
            "the owner still uses the paid cloud brain by default"
        );

        // GUEST: the SAME seam composition forces LOCAL — no cloud call, hence no obol
        // spend row / no budget bump / no owner-key egress on a bystander's turn.
        let guest = crate::threshold::guest_from(
            &crate::threshold::Scope::owner(vec!["*".to_string()], crate::focus::FocusProfile::Default),
            &crate::focus::FocusProfile::DeepFocus,
        );
        let _o = crate::threshold::ScopeOverride::guest(guest);
        assert!(crate::threshold::is_guest_turn());
        // SEAM 1 (cloud_reachable) is forced off even with the cloud reachable + vault off.
        assert!(
            !crate::threshold::deny_cloud(crate::vault::deny_cloud(true)),
            "guest: seam 1 (cloud_reachable) is forced local"
        );
        let guest_reachable = crate::threshold::deny_cloud(crate::vault::deny_cloud(true));
        assert_eq!(
            conversation_brain(&cfg, guest_reachable, &heavy).0,
            ConversationBrain::Local,
            "a guest conversation is answered by the on-device brain, never the paid cloud"
        );
        // SEAM 2 (the actuating tool-loop `to_cloud`) is likewise forced off for a guest.
        assert!(
            !crate::threshold::deny_cloud(crate::vault::deny_cloud(true)),
            "guest: seam 2 (to_cloud) is forced local"
        );
    }

    /// MODEL TIER wired into the conversation brain: an explicit override beats the
    /// config default, and an offline override forces the local path with NO cloud
    /// model — even when the cloud is reachable. Auto (no override) preserves the
    /// config-default behavior. This is the router-level proof that the swap is
    /// MODEL-only and that "offline" means no cloud call.
    #[test]
    fn conversation_brain_honors_model_override() {
        let _guard = crate::model_tier::OverrideGuard::force(None);
        let cfg = Config::default(); // cloud_heavy default
        let heavy = classification("conversation", "heavy", 0.95);

        // No override -> Auto -> Heavy/Opus (config default preserved).
        let (brain, _tier, reason) = conversation_brain(&cfg, true, &heavy);
        assert_eq!(brain, ConversationBrain::Cloud("claude-opus-4-8".to_string()));
        assert_eq!(reason, crate::model_tier::Reason::Auto);

        // Offline override -> Local, NO cloud model, even with cloud reachable.
        crate::model_tier::set_override(Some(crate::model_tier::Tier::Local));
        let (brain, tier, reason) = conversation_brain(&cfg, true, &heavy);
        assert_eq!(brain, ConversationBrain::Local);
        assert_eq!(tier, crate::model_tier::Tier::Local);
        assert_eq!(reason, crate::model_tier::Reason::Override);

        // Fast override -> Haiku cloud regardless of the heavy difficulty.
        crate::model_tier::set_override(Some(crate::model_tier::Tier::Fast));
        let (brain, _tier, reason) = conversation_brain(&cfg, true, &heavy);
        assert_eq!(brain, ConversationBrain::Cloud("claude-haiku-4-5".to_string()));
        assert_eq!(reason, crate::model_tier::Reason::Override);

        // Clear back to Auto.
        crate::model_tier::set_override(None);
    }

    /// MULTI-RESIDENT LOCAL sub-tier wired into the router (task #17). Under the
    /// CONSERVATIVE default (single-resident) the daemon sends NO local_model — the
    /// wire is identical to today and the base answers every local turn. With a
    /// multi-resident warm-set configured, a trivial turn threads the warm fast
    /// model id while a hard turn keeps the base (None). PURE — no inference call.
    #[tokio::test]
    async fn local_model_for_turn_is_none_under_single_resident_default() {
        let cfg = Config::default(); // empty warm-set, 0 budget => single-resident
        // Neither difficulty changes anything: single-resident => no local_model.
        assert_eq!(
            local_model_for_turn(&cfg, &classification("conversation", "light", 0.95)).await,
            None
        );
        assert_eq!(
            local_model_for_turn(&cfg, &classification("conversation", "heavy", 0.95)).await,
            None
        );
    }

    #[tokio::test]
    async fn local_model_for_turn_threads_fast_model_on_trivial_turn_when_multi_resident() {
        let mut cfg = Config::default();
        cfg.models.llm = "base-4b-4bit".to_string(); // ~2.4 GiB
        cfg.models.local_warm = vec!["fast-0.6b-4bit".to_string()]; // ~0.36 GiB
        cfg.models.local_budget_gib = 3.0; // admits the fast extra -> multi-resident

        // A trivial, confident turn -> the warm fast model is threaded.
        assert_eq!(
            local_model_for_turn(&cfg, &classification("conversation", "light", 0.95)).await,
            Some("fast-0.6b-4bit".to_string())
        );
        // A heavy turn keeps the capable base => None (no id on the wire; the
        // server answers on the base). No silent downgrade of a hard turn.
        assert_eq!(
            local_model_for_turn(&cfg, &classification("conversation", "heavy", 0.95)).await,
            None
        );
        // A low-confidence light turn is treated as hard -> base => None.
        assert_eq!(
            local_model_for_turn(&cfg, &classification("conversation", "light", 0.3)).await,
            None
        );
    }

    #[tokio::test]
    async fn local_model_for_turn_stays_none_when_budget_too_small() {
        // A multi-resident warm-set CONFIGURED but a budget too small to admit the
        // extra (or below the base estimate) stays single-resident => always None.
        let mut cfg = Config::default();
        cfg.models.llm = "base-4b-4bit".to_string();
        cfg.models.local_warm = vec!["fast-0.6b-4bit".to_string()];
        cfg.models.local_budget_gib = 1.0; // below the base estimate -> single
        assert_eq!(
            local_model_for_turn(&cfg, &classification("conversation", "light", 0.95)).await,
            None
        );
    }

    /// The HUD's per-turn local sub-choice label (FAST/CAPABLE/none) emitted in the
    /// `model.tier` payload. Under single-resident it is None (no indicator, the
    /// base answers); multi-resident reports the model that ACTUALLY answered —
    /// `fast` for a trivial/confident turn, `capable` when the base handled a
    /// hard/low-confidence turn. PURE — no inference. Matches local_model_for_turn.
    #[tokio::test]
    async fn local_sub_for_turn_reports_the_active_warm_choice() {
        // Single-resident default => no sub-choice (HUD indicator stays empty).
        let single = Config::default();
        assert_eq!(
            local_sub_for_turn(&single, &classification("conversation", "light", 0.95)).await,
            None
        );
        assert_eq!(
            local_sub_for_turn(&single, &classification("conversation", "heavy", 0.95)).await,
            None
        );

        // Multi-resident: a trivial confident turn answered on the fast model.
        let mut multi = Config::default();
        multi.models.llm = "base-4b-4bit".to_string();
        multi.models.local_warm = vec!["fast-0.6b-4bit".to_string()];
        multi.models.local_budget_gib = 3.0;
        assert_eq!(
            local_sub_for_turn(&multi, &classification("conversation", "light", 0.95)).await,
            Some("fast")
        );
        // A hard turn kept the capable base => CAPABLE (not a phantom fast pick).
        assert_eq!(
            local_sub_for_turn(&multi, &classification("conversation", "heavy", 0.95)).await,
            Some("capable")
        );
        // A low-confidence light turn is treated as hard => CAPABLE.
        assert_eq!(
            local_sub_for_turn(&multi, &classification("conversation", "light", 0.3)).await,
            Some("capable")
        );
    }

    /// CONTRACT B: the existing heavy/low-confidence cloud routing is
    /// UNCHANGED. Heavy -> cloud (Opus); a confident light action intent stays
    /// local; a low-confidence light turn still goes cloud (fast model). This
    /// applies to every intent — conversation_route does not touch it.
    #[test]
    fn heavy_and_action_routing_is_unchanged() {
        let cfg = Config::default(); // threshold 0.6

        // Heavy conversation -> cloud, Opus (heavy path, unchanged).
        let heavy = classification("conversation", "heavy", 0.95);
        assert!(wants_cloud(&heavy, &cfg), "heavy must route to cloud");
        assert_eq!(cloud_model(true, &cfg), "claude-opus-4-8", "heavy -> opus");

        // Confident light action intent -> local (unchanged: not heavy, high
        // confidence). conversation_route is irrelevant for action intents.
        let action = classification("app.launch", "light", 0.95);
        assert!(!wants_cloud(&action, &cfg), "confident action stays local");

        // Confident light conversation -> not cloud by the heavy/low-confidence
        // rule; the conversation-specific brain (above) decides cloud-vs-local.
        let chat = classification("conversation", "light", 0.95);
        assert!(!wants_cloud(&chat, &cfg));

        // Low-confidence light turn still goes cloud on the fast model
        // (unchanged low-confidence path).
        let unsure = classification("file.op", "light", 0.4);
        assert!(wants_cloud(&unsure, &cfg), "low confidence -> cloud");
        assert_eq!(cloud_model(false, &cfg), "claude-haiku-4-5", "light cloud -> haiku");
    }

    /// RC-6: an UNCERTAIN FALLBACK (low-confidence conversation — the garbled-
    /// echo shape CLASSIFY_FALLBACK produces) is recognized so the router can
    /// keep it OUT of the actuating cloud tool loop. A confident conversation
    /// turn, and any non-conversation intent (a real action, even low
    /// confidence), are NOT fallbacks and keep their existing routing.
    #[test]
    fn uncertain_fallback_is_only_low_confidence_conversation() {
        let cfg = Config::default(); // cloud_confidence_threshold 0.6

        // The exact CLASSIFY_FALLBACK shape: conversation / 0.3 -> fallback.
        assert!(is_uncertain_fallback(&classification("conversation", "heavy", 0.3), &cfg));
        // Low-confidence conversation generally -> fallback (no actuation).
        assert!(is_uncertain_fallback(&classification("conversation", "light", 0.5), &cfg));

        // A CONFIDENT conversation turn is NOT a fallback.
        assert!(!is_uncertain_fallback(&classification("conversation", "light", 0.95), &cfg));
        // A low-confidence ACTION intent is a real (weakly recognized) action,
        // NOT a fallback — its existing cloud tool routing is untouched.
        assert!(!is_uncertain_fallback(&classification("web.open", "light", 0.3), &cfg));
        assert!(!is_uncertain_fallback(&classification("app.launch", "heavy", 0.4), &cfg));
        // Exactly at the threshold is confident enough (not below it).
        assert!(!is_uncertain_fallback(&classification("conversation", "light", 0.6), &cfg));
    }

    /// Darwin-Prime delegation via the router wrapper: the offline-survival
    /// route only fires when the cloud is truly unreachable for this turn
    /// (cloud_reachable=false AND the turn is not already heading to cloud).
    #[test]
    fn select_agent_gates_offline_route_on_effective_cloud() {
        let reg = AgentRegistry::canonical();
        // Cloud up: conversational turn is the orchestrator's.
        assert_eq!(select_agent(&reg, "conversation", "tell me about mars", true, false).name, "darwin");
        // Cloud down AND not routing to cloud this turn: hulk survives.
        assert_eq!(select_agent(&reg, "conversation", "tell me about mars", false, false).name, "hulk");
        // Cloud down but THIS turn goes to cloud anyway (to_cloud=true): the
        // cloud is reachable for it, so no offline fallback.
        assert_eq!(select_agent(&reg, "conversation", "tell me about mars", false, true).name, "darwin");
        // Local action intents are unaffected by cloud state.
        assert_eq!(select_agent(&reg, "app.launch", "open safari", false, false).name, "oracle");
    }

    /// Tool-allowlist isolation at the router boundary: an agent that lacks the
    /// intent's tool is replaced by the tool's owner; an agent that holds it is
    /// kept. friday (intel) cannot run app.launch — that is oracle's.
    #[test]
    fn enforce_tool_reroutes_out_of_domain_intents() {
        let reg = AgentRegistry::canonical();
        let friday = reg.get("friday").unwrap();
        // friday does not own app.launch -> handed to oracle (the owner).
        let acting = enforce_tool(&reg, friday, "app.launch");
        assert_eq!(acting.name, "oracle");
        // oracle owns app.launch -> kept.
        let oracle = reg.get("oracle").unwrap();
        assert_eq!(enforce_tool(&reg, oracle, "app.launch").name, "oracle");
        // friday owns system.query -> kept.
        assert_eq!(enforce_tool(&reg, friday, "system.query").name, "friday");
        // darwin (wildcard) keeps anything.
        let darwin = reg.get("darwin").unwrap();
        assert_eq!(enforce_tool(&reg, darwin, "web.open").name, "darwin");
    }

    /// Roll-call interrupt mechanics: the cancel flag toggles cleanly and a
    /// fresh roll-call would clear it (the flag is process-wide and idempotent).
    /// Serialized via the flag's own reset so concurrent tests don't collide.
    /// Roll-call interrupt lifecycle (RC-9). interrupt_roll_call() SETS the
    /// cancel flag; clear_roll_call_interrupt() (called from
    /// speech::clear_barge_in at each new turn) RESETS it, so a barge over an
    /// unrelated reply can no longer leave a roll-call abort latched. Both
    /// mutators of the process-global ROLL_CALL_CANCEL are exercised in ONE test
    /// so they can never race each other on a parallel runner — the flag is a
    /// single shared global, and two separate tests mutating it would collide.
    #[test]
    fn roll_call_interrupt_lifecycle() {
        // Set, then read back.
        interrupt_roll_call();
        assert!(ROLL_CALL_CANCEL.load(Ordering::Relaxed), "interrupt must set the cancel flag");
        // Clear resets it (the RC-9 fix's mechanism).
        clear_roll_call_interrupt();
        assert!(!ROLL_CALL_CANCEL.load(Ordering::Relaxed), "clear must reset the cancel flag");
        // Idempotent: clearing again is harmless.
        clear_roll_call_interrupt();
        assert!(!ROLL_CALL_CANCEL.load(Ordering::Relaxed));
        // Leave the flag CLEAR so the live roll-call (which also clears it at
        // start) is unaffected.
        clear_roll_call_interrupt();
    }

    #[test]
    fn app_name_extraction_takes_words_after_the_trigger_verb() {
        assert_eq!(extract_app_name("darwin please open up google chrome"), "google chrome");
        assert_eq!(extract_app_name("launch the calculator app for me"), "calculator");
        assert_eq!(extract_app_name("quit safari"), "safari");
        assert_eq!(extract_app_name("close safari"), "safari");
        assert_eq!(extract_app_name("start photo booth now"), "photo booth");
    }

    /// Audit regression: quit-class utterances must NEVER reach the
    /// launcher — "quit safari" used to OPEN Safari ("Opened Safari.").
    #[test]
    fn quit_and_close_never_route_to_the_launcher() {
        for text in ["quit safari", "close safari", "exit chrome", "kill the music app"] {
            assert!(wants_quit(text), "missed quit verb in: {text}");
            let extracted = extract_app_name(text);
            assert_eq!(
                classify_app_request("app.launch", text, &extracted),
                AppRequest::Quit,
                "would have launched: {text}"
            );
            assert_eq!(
                classify_app_request("app.control", text, &extracted),
                AppRequest::Quit,
                "would have launched: {text}"
            );
        }
        assert!(!wants_quit("open safari"));
        assert!(!wants_quit("darwin please open up google chrome"));
    }

    /// Belt-and-suspenders reroute: an app.launch whose remainder smells of
    /// the web goes to the web.open handling — the original failing case
    /// must trigger it even if the classifier says app.launch.
    #[test]
    fn web_flavored_launches_reroute_to_web_open() {
        let text = "open the official apple website on safari";
        let extracted = extract_app_name(text);
        assert_eq!(
            classify_app_request("app.launch", text, &extracted),
            AppRequest::Web
        );
        // Bare-domain and scheme flavors trigger too.
        for text in [
            "open apple.com",
            "open up rust-lang.org for me",
            "open https://apple.com",
            "open the anthropic web page",
            "open that site again",
        ] {
            let extracted = extract_app_name(text);
            assert_eq!(
                classify_app_request("app.launch", text, &extracted),
                AppRequest::Web,
                "should reroute to web: {text}"
            );
        }
        // Plain app launches stay launches.
        for text in ["open safari", "launch the calculator app for me", "start photo booth"] {
            let extracted = extract_app_name(text);
            assert_eq!(
                classify_app_request("app.launch", text, &extracted),
                AppRequest::Launch,
                "should stay a launch: {text}"
            );
        }
        // The reroute is app.launch-only per contract.
        assert_eq!(
            classify_app_request("app.control", "open apple.com", "apple.com"),
            AppRequest::Launch
        );
    }

    #[test]
    fn web_markers_are_words_or_domain_fragments() {
        assert!(suggests_web("official apple website"));
        assert!(suggests_web("apple.com"));
        assert!(suggests_web("wikipedia.org"));
        assert!(suggests_web("https://apple.com"));
        assert!(suggests_web("the web"));
        assert!(!suggests_web("safari"));
        assert!(!suggests_web("google chrome"));
        assert!(!suggests_web("communications app")); // no false substring hits
        assert!(!suggests_web(""));
    }

    #[test]
    fn web_query_drops_command_and_web_noise() {
        assert_eq!(
            extract_web_query("search the web for rust async tutorials"),
            "rust async tutorials"
        );
        assert_eq!(extract_web_query("google the weather in tokyo"), "weather tokyo");
    }

    #[test]
    fn arg_str_reads_only_non_empty_strings() {
        let args = json!({"url": "apple.com", "browser": "  ", "n": 4});
        assert_eq!(arg_str(&args, "url"), Some("apple.com"));
        assert_eq!(arg_str(&args, "browser"), None); // blank -> absent
        assert_eq!(arg_str(&args, "n"), None); // wrong type -> absent
        assert_eq!(arg_str(&args, "missing"), None);
        // Old servers: args is Null, every lookup is None.
        assert_eq!(arg_str(&serde_json::Value::Null, "url"), None);
    }

    #[test]
    fn app_name_extraction_is_empty_without_a_trigger_verb() {
        // The router then feeds the whole utterance to the fuzzy matcher.
        assert_eq!(extract_app_name("could you get safari going"), "");
        assert_eq!(extract_app_name("open"), ""); // verb with nothing after it
    }

    #[test]
    fn content_words_drop_the_command_vocabulary() {
        assert_eq!(
            extract_content_words("find my budget spreadsheet file"),
            "budget spreadsheet"
        );
        assert_eq!(
            extract_content_words("look for the document called tax-report.pdf"),
            "tax-report.pdf"
        );
        assert_eq!(extract_content_words("find my files"), "");
    }

    #[test]
    fn open_detection_is_a_plain_substring_check() {
        assert!(utterance_wants_open("find and open the budget file"));
        assert!(!utterance_wants_open("find the budget file"));
    }

    /// CONTRACT B: the router passes DARWIN's most-recent replies as the cloud
    /// conversation anti-repeat `avoid` list. History is oldest-first; the
    /// freshest replies come back first, blanks are dropped, and the list is
    /// capped at n (the prompt-level lever Opus needs since it has no
    /// temperature). Empty history -> empty list (a first turn dodges nothing).
    #[test]
    fn recent_replies_takes_the_freshest_darwin_replies() {
        let history = vec![
            ("hi".to_string(), "Hello, sir. Good to have you back.".to_string()),
            ("hi".to_string(), "Welcome back, sir.".to_string()),
            ("hi".to_string(), "  ".to_string()), // blank reply dropped
            ("hi".to_string(), "Ah, there you are, sir.".to_string()),
        ];
        let avoid = recent_replies(&history, 4);
        // Freshest first, blank dropped: 3 non-blank replies, newest leading.
        assert_eq!(
            avoid,
            vec![
                "Ah, there you are, sir.".to_string(),
                "Welcome back, sir.".to_string(),
                "Hello, sir. Good to have you back.".to_string(),
            ]
        );
        // Cap is honoured.
        assert_eq!(recent_replies(&history, 1), vec!["Ah, there you are, sir.".to_string()]);
        // First turn: nothing to dodge.
        assert!(recent_replies(&[], 4).is_empty());
    }

    // ---- Silicon Canvas voice control (SPEC §6) ----

    /// Helper: assert the utterance maps to an Op carrying EXACTLY this JSON
    /// wire string (the form Silicon Canvas's ops.rs deserializes verbatim).
    fn assert_op(text: &str, expected_json: &str) {
        match silicon_canvas_command(text) {
            Some(SiliconCanvasCommand::Op(line)) => {
                // Compare as parsed JSON so key order is irrelevant; the exact
                // op-tag + fields are what the contract pins.
                let got: serde_json::Value = serde_json::from_str(&line).unwrap();
                let want: serde_json::Value = serde_json::from_str(expected_json).unwrap();
                assert_eq!(got, want, "for utterance {text:?}");
            }
            other => panic!("expected an Op for {text:?}, got {other:?}"),
        }
    }

    /// "open silicon canvas" (and its open-class variants) is a LAUNCH; the
    /// app name is the manifest name the registry keys on.
    #[test]
    fn silicon_canvas_launch_phrases() {
        assert_eq!(SILICON_CANVAS_APP, "silicon-canvas");
        for text in [
            "open silicon canvas",
            "launch silicon canvas",
            "bring up silicon canvas",
            "darwin, show me silicon canvas",
            "open the schematic",
            "bring up the board view",
        ] {
            assert_eq!(
                silicon_canvas_command(text),
                Some(SiliconCanvasCommand::Launch),
                "should launch: {text:?}"
            );
        }
    }

    /// "show me the <X> net" / "highlight the <X> net" -> select.net {name},
    /// with the net name forwarded verbatim (uppercased to KiCad convention).
    #[test]
    fn silicon_canvas_net_selection_maps_to_select_net() {
        assert_op("show me the 3V3 net", r#"{"op":"select.net","name":"3V3"}"#);
        assert_op("highlight the gnd net", r#"{"op":"select.net","name":"GND"}"#);
        assert_op("select the vbus net", r#"{"op":"select.net","name":"VBUS"}"#);
        // The net name rides through even with extra words around it.
        assert_op("can you show me the sda net please", r#"{"op":"select.net","name":"SDA"}"#);
    }

    /// Trace mode: start / step / stop map to the three trace ops, and the
    /// specific step/stop verbs are matched before the broad "trace" -> start.
    #[test]
    fn silicon_canvas_trace_mode_ops() {
        assert_op("trace this net", r#"{"op":"trace.start"}"#);
        assert_op("start tracing", r#"{"op":"trace.start"}"#);
        assert_op("begin the trace", r#"{"op":"trace.start"}"#);
        // Step (advance) — must NOT be read as start.
        assert_op("next trace step", r#"{"op":"trace.step"}"#);
        assert_op("step the trace", r#"{"op":"trace.step"}"#);
        assert_op("advance the trace", r#"{"op":"trace.step"}"#);
        // Stop/exit.
        assert_op("stop tracing", r#"{"op":"trace.stop"}"#);
        assert_op("exit trace mode", r#"{"op":"trace.stop"}"#);
    }

    /// "run ERC" and the spelled-out electrical-rule-check phrasing -> erc.run.
    #[test]
    fn silicon_canvas_erc_maps_to_erc_run() {
        assert_op("run erc", r#"{"op":"erc.run"}"#);
        assert_op("run the ERC", r#"{"op":"erc.run"}"#);
        assert_op("run the electrical rule check", r#"{"op":"erc.run"}"#);
        assert_op("check the electrical rules", r#"{"op":"erc.run"}"#);
    }

    /// Component selection and view fit.
    #[test]
    fn silicon_canvas_component_and_view_ops() {
        assert_op("select component u3", r#"{"op":"select.component","name":"U3"}"#);
        assert_op("show component r12", r#"{"op":"select.component","name":"R12"}"#);
        // A bare "component" with no ref token is NOT a select.component.
        assert!(silicon_canvas_command("tell me about the component").is_none());
        // View fit.
        assert_op("fit the board", r#"{"op":"view.set","mode":"fit","target":"all"}"#);
        assert_op("show the whole board", r#"{"op":"view.set","mode":"fit","target":"all"}"#);
        assert_op("fit all", r#"{"op":"view.set","mode":"fit","target":"all"}"#);
    }

    /// The classifier does NOT capture unrelated utterances: a plain "open
    /// safari" or a greeting falls through to normal routing (None), so the
    /// Silicon Canvas pre-check never shadows the macOS launcher or chat.
    #[test]
    fn silicon_canvas_command_ignores_unrelated_utterances() {
        for text in [
            "open safari",
            "hello darwin how are you",
            "what's the weather",
            "find my budget spreadsheet",
            "open apple.com",
            "play some music",
            "i read the network news",   // "net" only as a substring of network/news
            "open the calculator",
        ] {
            assert_eq!(
                silicon_canvas_command(text),
                None,
                "must not capture an unrelated utterance: {text:?}"
            );
        }
    }

    /// Whole-word "net": "network"/"netflix" never trigger select.net (the
    /// extractor splits on word boundaries and requires the standalone token).
    #[test]
    fn silicon_canvas_net_is_whole_word_only() {
        assert!(silicon_canvas_command("check the network settings").is_none());
        assert!(silicon_canvas_command("open netflix").is_none());
        // But a real net selection still fires.
        assert_op("show me the clk net", r#"{"op":"select.net","name":"CLK"}"#);
    }

    // ======================================================================
    // Vision voice control. Mirrors the Silicon Canvas tests above. The wire
    // form pinned here is the FROZEN Op.swift envelope: every op carries
    // {"type":"op","op":...} — these exact lines appear in the Vision app's own
    // IPCTests, so a pass here proves the daemon emits what the app accepts.
    // ======================================================================

    /// Assert the utterance maps to a Vision Op carrying EXACTLY this JSON wire
    /// string (compared as parsed JSON so key order is irrelevant).
    fn assert_vision_op(text: &str, expected_json: &str) {
        match vision_command(text) {
            Some(VisionCommand::Op(line)) => {
                let got: serde_json::Value = serde_json::from_str(&line).unwrap();
                let want: serde_json::Value = serde_json::from_str(expected_json).unwrap();
                assert_eq!(got, want, "for utterance {text:?}");
            }
            other => panic!("expected a Vision Op for {text:?}, got {other:?}"),
        }
    }

    /// "open/launch/start vision" is a LAUNCH keyed on the manifest name.
    #[test]
    fn vision_launch_phrases() {
        assert_eq!(VISION_APP, "vision");
        for text in [
            "open vision",
            "launch vision",
            "start vision",
            "darwin, bring up vision",
            "fire up the camera feed",
        ] {
            assert_eq!(
                vision_command(text),
                Some(VisionCommand::Launch),
                "{text:?} should be a Vision launch"
            );
        }
        // "vision" must be a whole word — never inside "television"/"revision".
        assert!(vision_command("open the television").is_none());
        assert!(vision_command("start the revision").is_none());
    }

    // ===== LUMEN (#45) dispatch ==========================================

    /// "read me the screen / the buttons / what's on screen" classify as a Lumen
    /// READ; "click/press/tap the <ordinal|name>" as a Lumen ACT carrying the
    /// phrase.
    #[test]
    fn lumen_read_and_act_phrases_route_correctly() {
        for text in [
            "read me the screen",
            "read the screen",
            "read me the buttons",
            "read the controls",
            "narrate the screen",
            "list the buttons",
            "what's on screen",
            "what is on my screen",
            "what are the buttons",
        ] {
            assert_eq!(lumen_command(text), Some(LumenCommand::Read), "{text:?} -> READ");
        }
        for (text, want) in [
            ("click the third button", "click the third button"),
            ("press the second button", "press the second button"),
            ("tap Submit", "tap submit"),
            ("click Sign in", "click sign in"),
            ("click the 2nd link", "click the 2nd link"),
        ] {
            assert_eq!(
                lumen_command(text),
                Some(LumenCommand::Act(want.to_string())),
                "{text:?} -> ACT (lowercased phrase)"
            );
        }
    }

    /// The classifier is CONSERVATIVE: ordinary speech and the more-specific
    /// Vision phrasings never over-trigger a Lumen read/act.
    #[test]
    fn lumen_does_not_over_trigger_on_unrelated_speech() {
        for text in [
            // No UI-actuation verb / no screen-or-controls read anchor.
            "read me the news",
            "what's on my plate today",
            "what do you see",
            "press play",           // press + no control noun/ordinal
            "push harder",          // push + no control noun/ordinal
            "let's press on",       // press + no control noun/ordinal
            "is the tap water safe", // REGRESSION: "tap" mid-sentence is not a command
            "i'll be on tap all night",
            "he had to tap out of the match",
            "select all my emails", // "select" is NOT a Lumen act verb
            "choose a restaurant",  // "choose" is NOT a Lumen act verb
            // These belong to the more-specific Vision ops (deferred by Lumen).
            "where's the submit button",
            "locate the settings icon",
            "watch the screen",
            "scan this document",
            "read this handwriting",
            "describe my screen",
        ] {
            assert_eq!(lumen_command(text), None, "{text:?} must NOT trigger Lumen");
        }
    }

    /// A Lumen READ is a screen read (surfaces on-screen control labels), so it is
    /// unioned into `is_screen_read` for TRANSIENCE; a Lumen ACT is NOT a read.
    #[test]
    fn lumen_read_is_transient_but_act_is_not() {
        assert!(is_screen_read("read me the buttons"), "a lumen read is transient");
        assert!(is_screen_read("what are the controls"), "a lumen read is transient");
        assert!(!is_screen_read("click the third button"), "an actuation is not a screen read");
    }

    /// The ACT arm builds the `ui_actuate` tool input in the EXACT `UiActuateArgs`
    /// shape a live tool call carries — a single click at the resolved point, with
    /// `confirm` OMITTED (never self-set, so it can only ever PARK).
    #[test]
    fn ui_actuate_input_is_the_capstone_tool_shape() {
        let req = crate::ui_automation::ActuationRequest {
            action: crate::ui_automation::Action::Click { x: 300, y: 200 },
            target_desc: "Cancel".to_string(),
        };
        let input = ui_actuate_input(&req);
        assert_eq!(input["action"], "click");
        assert_eq!(input["target"], "Cancel");
        assert_eq!(input["x"], 300);
        assert_eq!(input["y"], 200);
        assert!(input.get("confirm").is_none(), "confirm is never set by Lumen: {input}");
    }

    /// THE SAFETY ASSERTION: the ACT path builds an ActuationRequest via the pure
    /// selector and flows it through the UNCHANGED capstone (`execute_tool`,
    /// the SAME entry a live tool call uses) — and the capstone NEVER auto-executes
    /// it. `resolve_voice_action` -> `ui_actuate_input` -> `execute_tool` under the
    /// ui_actuate-owning agent's allowlist: nothing is performed and nothing
    /// self-authorizes (with the master switch off — the default — the gate is a
    /// DryRun even with confirm; in this headless build the deny-leaning display
    /// bound also refuses the click pre-actuation, so nothing is even parked). NO
    /// real AX/OCR/actuate runs. `plan_actuation` against a real bound proves the
    /// request is a valid SINGLE actuation (never a batch).
    #[tokio::test]
    async fn lumen_act_flows_through_the_unchanged_capstone_and_never_auto_executes() {
        use crate::ui_automation::{Action, ScreenBounds};
        // A located control list, exactly as a prior read would have produced.
        let controls = vec![
            crate::lumen::NarratableElement {
                label: "Submit".into(),
                role: crate::lumen::ElementRole::Button,
                center: Some((100, 200)),
            },
            crate::lumen::NarratableElement {
                label: "Cancel".into(),
                role: crate::lumen::ElementRole::Button,
                center: Some((300, 200)),
            },
        ];
        // Pure selection -> the ONE target's actuation request (never a batch).
        let req = crate::lumen::resolve_voice_action("click the second button", &controls).unwrap();
        assert!(matches!(req.action, Action::Click { x: 300, y: 200 }));
        assert!(
            crate::ui_automation::plan_actuation(&req, ScreenBounds { width: 4000, height: 4000 }).is_ok(),
            "the request is a valid, bounded, single actuation"
        );
        // The gate never auto-executes on Lumen's say-so: with the master switch
        // off (default), even a confirm is a DryRun (parks/previews, never fires).
        assert_eq!(
            crate::integrations::gate(true),
            crate::integrations::ActionMode::DryRun,
            "confirm alone can never execute — the action parks/previews"
        );

        // Flow the SAME request through the UNCHANGED capstone entry.
        let db = TempDb::new("lumen-act");
        let mem = Memory::open(&db.0).unwrap();
        let reg = AgentRegistry::canonical();
        let actuator = reg.owner_of("ui_actuate").expect("an agent owns ui_actuate");
        assert!(actuator.may_use("ui_actuate"), "the owner may use the capstone");
        let input = ui_actuate_input(&req);
        let (outcome, _is_error) = crate::anthropic::execute_tool(
            "ui_actuate",
            &input,
            &mem,
            &actuator.tools,
            &actuator.namespace,
            true,
            true, // context_trusted: mirrors the attended live-actuation production call
        )
        .await;
        assert!(
            !outcome.to_lowercase().contains("i performed"),
            "the capstone must NEVER auto-execute a Lumen actuation: {outcome}"
        );
        // Nothing self-authorized: no parked-then-executed action left the slot
        // holding an executed effect (the deny-leaning bound refused it pre-park).
        assert!(
            crate::confirm::peek_pending(std::time::Instant::now()).is_none(),
            "a Lumen actuation never self-parks an executed action"
        );
    }

    /// The READ arm forwards the READ-ONLY Vision `read.screen` locate through the
    /// speech path (llm_voice) — honest when Vision isn't reachable, never a
    /// fabricated readout, and it actuates nothing.
    #[tokio::test]
    async fn lumen_read_arm_forwards_the_readonly_locate_through_speech() {
        let apps = std::sync::Arc::new(crate::apps::AppRegistry::discover(std::path::Path::new(
            "/nonexistent",
        )));
        let reg = AgentRegistry::canonical();
        let out = super::handle_lumen(
            LumenCommand::Read,
            &Memory::open(&TempDb::new("lumen-read").0).unwrap(),
            &apps,
            reg.orchestrator(),
        )
        .await;
        assert!(out.llm_voice, "the read acknowledgment is persona-voiced");
        // Vision isn't running here, so it says so HONESTLY (never a fake readout).
        assert!(out.data.to_lowercase().contains("screen"), "{}", out.data);
        assert!(!out.data.to_lowercase().contains("i performed"), "read actuates nothing");
    }

    /// "what do you see" / "who is there" -> the generic presence STATUS
    /// snapshot. DEFENSIVE-ONLY: "who is there" is presence, NOT an identity
    /// query — it maps to the SAME status op as "what do you see"; there is no
    /// name/face op anywhere.
    #[test]
    fn vision_presence_queries_map_to_status_not_identity() {
        let status = r#"{"type":"op","op":"status"}"#;
        assert_vision_op("what do you see", status);
        assert_vision_op("darwin, what can you see right now", status);
        assert_vision_op("who is there", status);
        assert_vision_op("who's there", status);
        assert_vision_op("is anyone there", status);
        assert_vision_op("is somebody there", status);
        // The op body NEVER contains a name/identity field — presence only.
        if let Some(VisionCommand::Op(line)) = vision_command("who is there") {
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert!(v.get("name").is_none(), "presence status must carry no identity");
            assert!(v.get("person").is_none());
            assert_eq!(v["op"], "status");
        } else {
            panic!("expected a status op");
        }
    }

    /// "watch the door|room|camera" -> watch.start {camera}; "watch the
    /// screen|display" -> watch.start {screen}.
    #[test]
    fn vision_watch_picks_camera_or_screen_source() {
        assert_vision_op(
            "watch the door",
            r#"{"type":"op","op":"watch.start","source":"camera"}"#,
        );
        assert_vision_op(
            "watch the room",
            r#"{"type":"op","op":"watch.start","source":"camera"}"#,
        );
        assert_vision_op(
            "keep watching the front camera",
            r#"{"type":"op","op":"watch.start","source":"camera"}"#,
        );
        assert_vision_op(
            "watch the screen",
            r#"{"type":"op","op":"watch.start","source":"screen"}"#,
        );
        assert_vision_op(
            "watch my display",
            r#"{"type":"op","op":"watch.start","source":"screen"}"#,
        );
    }

    /// "stop watching" -> watch.stop (checked before the broad watch.start so a
    /// stop verb is never mistaken for a start).
    #[test]
    fn vision_stop_watching_maps_to_watch_stop() {
        let stop = r#"{"type":"op","op":"watch.stop"}"#;
        assert_vision_op("stop watching", stop);
        assert_vision_op("stop watching the door", stop);
        assert_vision_op("end the watch", stop);
        assert_vision_op("cancel watching the screen", stop);
    }

    /// "analyze <name>.mp4" forwards the path verbatim; a bare "analyze this
    /// video" forwards an EMPTY path the app reports cleanly (Op.swift rejects an
    /// empty path -> .unknown -> a clean vision.error, never a crash).
    #[test]
    fn vision_analyze_file_forwards_path_or_empty() {
        assert_vision_op(
            "analyze front_door.mp4",
            r#"{"type":"op","op":"analyze.file","path":"front_door.mp4"}"#,
        );
        assert_vision_op(
            "analyze the clip porch-cam.mov please",
            r#"{"type":"op","op":"analyze.file","path":"porch-cam.mov"}"#,
        );
        // Bare "analyze this video" -> analyze.file with an empty path.
        assert_vision_op(
            "analyze this video",
            r#"{"type":"op","op":"analyze.file","path":""}"#,
        );
    }

    /// "set sensitivity to <X>" -> set.sensitivity with a clamped 0..=1 value;
    /// words/percent/float forms all resolve.
    #[test]
    fn vision_sensitivity_maps_to_set_sensitivity() {
        // Percent and bare float both normalize to 0..=1.
        match vision_command("set the sensitivity to 70 percent") {
            Some(VisionCommand::Op(line)) => {
                let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert_eq!(v["op"], "set.sensitivity");
                assert!((v["value"].as_f64().unwrap() - 0.7).abs() < 1e-9);
            }
            other => panic!("expected set.sensitivity, got {other:?}"),
        }
        match vision_command("set sensitivity to 0.3") {
            Some(VisionCommand::Op(line)) => {
                let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert!((v["value"].as_f64().unwrap() - 0.3).abs() < 1e-9);
            }
            other => panic!("expected set.sensitivity, got {other:?}"),
        }
        // Word form clamps into range.
        match vision_command("set sensitivity to high") {
            Some(VisionCommand::Op(line)) => {
                let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                let val = v["value"].as_f64().unwrap();
                assert!((0.0..=1.0).contains(&val) && val > 0.5);
            }
            other => panic!("expected set.sensitivity, got {other:?}"),
        }
    }

    /// "what's on my screen" / "read my screen" / "read this" -> the read.screen
    /// OCR op, on-wire byte-identical to the FROZEN default the Swift
    /// testFrozenOpWireNamesUnchanged pins ({"type":"op","op":"read.screen"}, no
    /// explicit source — the default .screen). A plain read carries NO query.
    #[test]
    fn vision_read_screen_maps_to_read_screen_op() {
        let read = r#"{"type":"op","op":"read.screen"}"#;
        for text in [
            "what's on my screen",
            "what is on my screen",
            "what's on screen right now",
            "read my screen",
            "read the screen",
            "read what's on my screen",
            "darwin, read this",
            "read that for me",
        ] {
            assert_vision_op(text, read);
            // The default read carries no query field and no source field — the
            // FROZEN default op shape, unchanged.
            if let Some(VisionCommand::Op(line)) = vision_command(text) {
                let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert!(v.get("query").is_none(), "plain read carries no query: {text:?}");
                assert!(v.get("source").is_none(), "default read omits source: {text:?}");
                assert_eq!(v["op"], "read.screen");
            } else {
                panic!("expected a read.screen op for {text:?}");
            }
        }
    }

    /// "where's the <X> button" / "find the <X> button" / "locate the <X>" -> a
    /// read.screen op carrying the control phrase as `query`. READ-ONLY: this
    /// LOCATES a control (the app returns its box/center); the daemon never emits
    /// a click op — there is no click op anywhere in the contract.
    #[test]
    fn vision_where_is_a_control_maps_to_read_screen_with_query() {
        let cases = [
            ("where's the submit button", "submit"),
            ("where is the sign in button", "sign in"),
            ("find the save button", "save"),
            ("locate the settings icon", "settings"),
            ("where is the search field", "search"),
        ];
        for (text, want_query) in cases {
            match vision_command(text) {
                Some(VisionCommand::Op(line)) => {
                    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                    assert_eq!(v["op"], "read.screen", "for {text:?}");
                    assert_eq!(v["query"], want_query, "for {text:?}");
                    // READ-ONLY: never a click/actuate field.
                    assert!(v.get("click").is_none(), "where-is must never click: {text:?}");
                    assert!(v.get("tap").is_none());
                    assert!(v.get("actuate").is_none());
                }
                other => panic!("expected a read.screen query op for {text:?}, got {other:?}"),
            }
        }
    }

    /// A continuous "watch the screen" is STILL a watch.start (not an OCR read):
    /// the watch lifecycle is matched before the screen-read seam, so the two
    /// never collide.
    #[test]
    fn vision_watch_the_screen_is_not_a_screen_read() {
        assert_vision_op(
            "watch the screen",
            r#"{"type":"op","op":"watch.start","source":"screen"}"#,
        );
        // And a screen-read phrase never collides with the watch op.
        assert!(!is_screen_read("watch the screen"));
        assert!(is_screen_read("read my screen"));
    }

    /// PRIVACY PIN: `is_screen_read` agrees with the routing — anything that maps
    /// to a read.screen op is flagged transient, and nothing else is. main.rs
    /// gates fact extraction on this, so a screen read can never seed a durable
    /// fact / optimizer trace. The recognized text itself never reaches this path
    /// (it rides the vision.screen telemetry event); this pins the UTTERANCE +
    /// acknowledgment out of persistence too.
    #[test]
    fn screen_read_utterances_are_flagged_transient_and_others_are_not() {
        for text in [
            "what's on my screen",
            "read my screen",
            "read this",
            "where's the submit button",
            "find the save button",
        ] {
            assert!(is_screen_read(text), "{text:?} must be flagged transient");
            // Consistency: a transient utterance is exactly a read.screen op.
            match vision_command(text) {
                Some(VisionCommand::Op(line)) => {
                    assert!(line.contains("read.screen"), "{text:?} -> read.screen");
                }
                other => panic!("expected a read.screen op for {text:?}, got {other:?}"),
            }
        }
        // NON screen-read turns are NOT transient (they learn normally).
        for text in [
            "what do you see",          // presence status, not OCR
            "watch the screen",         // continuous watch, not OCR
            "remember my birthday is may third",
            "open vision",
            "what's the weather",
        ] {
            assert!(!is_screen_read(text), "{text:?} must NOT be transient");
        }
    }

    // ----- #28 HANDWRITING read / #29 DOCUMENT scan ----------------------------

    /// "read this handwriting" / "read the whiteboard" -> the read.handwriting op
    /// (#28). The default source is .camera (the line omits `source`, mirroring the
    /// Swift Op.swift default). An explicit "on screen" stamps the screen source.
    #[test]
    fn vision_read_handwriting_maps_to_read_handwriting_op() {
        for text in [
            "read this handwriting",
            "read the handwritten note",
            "read the whiteboard",
            "transcribe the whiteboard",
            "what does this handwriting say",
            "what's written on the whiteboard",
        ] {
            match vision_command(text) {
                Some(VisionCommand::Op(line)) => {
                    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                    assert_eq!(v["op"], "read.handwriting", "for {text:?}");
                    // Default source omitted -> the app's .camera default.
                    assert!(v.get("source").is_none(), "default handwriting read omits source: {text:?}");
                    // READ-ONLY: never a click/actuate field.
                    assert!(v.get("click").is_none() && v.get("actuate").is_none());
                }
                other => panic!("expected a read.handwriting op for {text:?}, got {other:?}"),
            }
        }
        // An explicit "on screen" handwriting read stamps the screen source.
        match vision_command("read the handwriting on screen") {
            Some(VisionCommand::Op(line)) => {
                let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert_eq!(v["op"], "read.handwriting");
                assert_eq!(v["source"], "screen");
            }
            other => panic!("expected a read.handwriting screen op, got {other:?}"),
        }
    }

    /// "scan this document" / "scan the page" / "scan this receipt" -> the
    /// scan.document op (#29). Default source .camera (omitted); "on screen" stamps
    /// the screen source. READ-ONLY: never a click/actuate field.
    #[test]
    fn vision_scan_document_maps_to_scan_document_op() {
        for text in [
            "scan this document",
            "scan the page",
            "scan this receipt",
            "scan the paper",
            "scan this form",
            "scan the invoice",
        ] {
            match vision_command(text) {
                Some(VisionCommand::Op(line)) => {
                    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                    assert_eq!(v["op"], "scan.document", "for {text:?}");
                    assert!(v.get("source").is_none(), "default scan omits source: {text:?}");
                    assert!(v.get("click").is_none() && v.get("actuate").is_none());
                }
                other => panic!("expected a scan.document op for {text:?}, got {other:?}"),
            }
        }
        // "scan the document on screen" stamps the screen source.
        match vision_command("scan the document on screen") {
            Some(VisionCommand::Op(line)) => {
                let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert_eq!(v["op"], "scan.document");
                assert_eq!(v["source"], "screen");
            }
            other => panic!("expected a scan.document screen op, got {other:?}"),
        }
    }

    /// DISTINCTNESS: handwriting (#28), document scan (#29), and the plain on-
    /// screen OCR read are three separate intents that never collide. A
    /// handwriting/document phrase must NOT fall into the generic read.screen op,
    /// and "read my screen" must NOT become a handwriting/scan op.
    #[test]
    fn handwriting_scan_and_plain_screen_read_are_distinct() {
        // Handwriting -> read.handwriting (NOT read.screen).
        match vision_command("read this handwriting") {
            Some(VisionCommand::Op(line)) => {
                assert!(line.contains("read.handwriting"));
                assert!(!line.contains("read.screen"), "handwriting must not be a plain screen read");
            }
            other => panic!("got {other:?}"),
        }
        // Scan -> scan.document (NOT read.screen).
        match vision_command("scan this document") {
            Some(VisionCommand::Op(line)) => {
                assert!(line.contains("scan.document"));
                assert!(!line.contains("read.screen"));
            }
            other => panic!("got {other:?}"),
        }
        // Plain on-screen OCR stays read.screen (NOT handwriting/scan).
        match vision_command("read my screen") {
            Some(VisionCommand::Op(line)) => {
                assert!(line.contains("read.screen"));
                assert!(!line.contains("read.handwriting") && !line.contains("scan.document"));
            }
            other => panic!("got {other:?}"),
        }
    }

    /// PRIVACY PIN: a handwriting read (#28) and a document scan (#29) BOTH surface
    /// sensitive recognized text (a handwritten note / a scanned page can carry
    /// private content), so both are flagged TRANSIENT — consistent with the
    /// routing (anything mapping to read.handwriting/scan.document is transient).
    #[test]
    fn handwriting_and_scan_utterances_are_flagged_transient() {
        for text in [
            "read this handwriting",
            "read the whiteboard",
            "scan this document",
            "scan the receipt",
        ] {
            assert!(is_screen_read(text), "{text:?} must be flagged transient (sensitive recognized text)");
            match vision_command(text) {
                Some(VisionCommand::Op(line)) => {
                    assert!(
                        line.contains("read.handwriting") || line.contains("scan.document"),
                        "{text:?} -> a handwriting/scan op"
                    );
                }
                other => panic!("expected a handwriting/scan op for {text:?}, got {other:?}"),
            }
        }
    }

    // ----- VLM DESCRIBE (task #2) — DISTINCT from the OCR read.screen path -----

    /// "describe my screen" / "what am I looking at" route to a VLM SCREEN
    /// describe — DISTINCT from the OCR read.screen path. The describe verb maps
    /// to DescribeRequest::Screen, never to a read.screen op. A BARE describe
    /// (no specific question) carries `question: None` (a generic caption).
    #[test]
    fn describe_screen_phrases_map_to_a_screen_describe() {
        for text in [
            "describe my screen",
            "describe what's on my screen",
            "what am I looking at",
            "what do you make of my screen",
            "describe the display",
        ] {
            assert_eq!(
                describe_command(text),
                Some(DescribeRequest::Screen { question: None }),
                "{text:?} must be a generic VLM screen describe (no specific question)"
            );
        }
    }

    /// VQA (task #2, build 2/2): a SPECIFIC visual question about the screen is
    /// threaded to the VLM as `question`, so the model answers THAT rather than
    /// emitting a generic caption. Two routes: the explicit "ask my screen …"
    /// trigger, and a describe verb carrying a substantive question.
    #[test]
    fn screen_vqa_threads_the_specific_question() {
        // Explicit "ask (about) my/the screen <q>" — the prefix is stripped.
        assert_eq!(
            describe_command("ask my screen which button rebuilds"),
            Some(DescribeRequest::Screen { question: Some("which button rebuilds".to_string()) })
        );
        assert_eq!(
            describe_command("ask about my screen: what is the error?"),
            Some(DescribeRequest::Screen { question: Some("what is the error?".to_string()) })
        );
        // A describe verb PLUS a substantive question -> the whole utterance is the
        // VQA prompt (the VLM reads the intent from the user's own words).
        assert_eq!(
            describe_command("describe my screen, is there a build error?"),
            Some(DescribeRequest::Screen {
                question: Some("describe my screen, is there a build error?".to_string())
            })
        );
        // "ask <a person> about the screen" is NOT a screen VQA (it does not begin
        // with an "ask <the screen>" prefix) — a message-a-contact intent is never
        // poached into the VLM.
        assert_eq!(describe_command("ask sarah about the screen resolution"), None);
    }

    /// "describe this image <path>" / "what's in <path>" route to a VLM IMAGE
    /// describe carrying the RAW candidate path (confined later by the handler).
    /// A bare describe carries `question: None`; a specific question is threaded.
    #[test]
    fn describe_image_phrases_carry_the_named_path() {
        assert_eq!(
            describe_command("describe this image /Users/me/pics/cat.png"),
            Some(DescribeRequest::Image {
                path: "/Users/me/pics/cat.png".to_string(),
                question: None
            })
        );
        assert_eq!(
            describe_command("what's in photo.jpg"),
            Some(DescribeRequest::Image { path: "photo.jpg".to_string(), question: None })
        );
        // Case of the path survives (file systems are case-sensitive).
        assert_eq!(
            describe_command("describe the picture MyPhoto.JPEG"),
            Some(DescribeRequest::Image { path: "MyPhoto.JPEG".to_string(), question: None })
        );
        // A specific question about the file -> threaded as VQA, with the path
        // token stripped out of the prompt (a file path never leaks to the VLM).
        assert_eq!(
            describe_command("describe cat.png — is the dog asleep?"),
            Some(DescribeRequest::Image {
                path: "cat.png".to_string(),
                question: Some("describe — is the dog asleep?".to_string())
            })
        );
        // The extractor finds an image extension token, nothing else.
        assert_eq!(extract_image_path("describe /tmp/a.png now"), Some("/tmp/a.png".to_string()));
        assert_eq!(extract_image_path("describe this image"), None);
    }

    /// vqa_question is PURE: a remnant made only of describe/scaffolding vocab is a
    /// generic caption (None); any substantive token makes it a specific question.
    #[test]
    fn vqa_question_distinguishes_generic_from_specific() {
        // Generic describe scaffolding -> None (the op uses its default prompt).
        for generic in ["describe my screen", "what am i looking at", "describe it", "describe the display"] {
            assert_eq!(vqa_question(generic, None), None, "{generic:?} is a generic caption");
        }
        // Substantive question -> Some(verbatim).
        assert_eq!(
            vqa_question("what's the error on my screen", None),
            Some("what's the error on my screen".to_string())
        );
        // Path is stripped before the generic/specific decision AND out of the
        // returned prompt (a file path never leaks to the VLM).
        assert_eq!(vqa_question("describe this image cat.png", Some("cat.png")), None);
        assert_eq!(
            vqa_question("what breed is the dog in cat.png", Some("cat.png")),
            Some("what breed is the dog in".to_string())
        );
    }

    /// PANIC PIN (no-regression): vqa_question / describe_command must NEVER panic
    /// on an offset-shifting-lowercase utterance — a char like `İ` whose lowercase
    /// is a different byte length — that also names an image path. The path-strip
    /// must not index a byte offset derived from a lowercased copy onto the
    /// original text (that lands mid-char and panics replace_range). Mirrors the
    /// extract_image_prompt offset-shift panic pin.
    #[test]
    fn vqa_and_describe_never_panic_on_offset_shifting_lowercase() {
        for text in [
            "İ describe a.png",
            "describe İcafé.png what İis on it",
            "İİİ what is in /tmp/İ.png please",
            "ẞ describe photo.PNG İ",
            "what İs in \u{0130}\u{0130}.jpeg",
            "ask my screen İ what İs the error",
            "ask about my display \u{0130}",
        ] {
            let _ = describe_command(text);
            let _ = vqa_question(text, extract_image_path(text).as_deref());
        }
    }

    /// CONTRACT PIN (no-regression): the OCR read.screen path is NOT poached by
    /// the VLM describe path. "read my screen" / "what's on my screen" stay OCR
    /// (a read.screen op, NOT a describe), and the describe phrases are NOT OCR.
    /// The two intents are mutually exclusive by construction.
    #[test]
    fn ocr_read_screen_and_vlm_describe_are_distinct_intents() {
        // OCR read verbs -> read.screen op, and NOT a describe request.
        for ocr in ["read my screen", "what's on my screen", "read this", "read the screen"] {
            assert!(is_screen_read(ocr), "{ocr:?} must stay an OCR read");
            assert_eq!(describe_command(ocr), None, "{ocr:?} must NOT be a VLM describe");
            match vision_command(ocr) {
                Some(VisionCommand::Op(line)) => assert!(line.contains("read.screen")),
                other => panic!("expected a read.screen op for {ocr:?}, got {other:?}"),
            }
        }
        // VLM describe verbs -> describe request, and NOT an OCR read.
        for vlm in ["describe my screen", "what am I looking at", "describe this image a.png"] {
            assert!(describe_command(vlm).is_some(), "{vlm:?} must be a VLM describe");
            assert!(!is_screen_read(vlm), "{vlm:?} must NOT be an OCR read");
        }
    }

    /// PRIVACY PIN: a VLM describe is flagged transient (it can surface sensitive
    /// VISUAL content), exactly like an OCR screen read — so main.rs keeps its
    /// utterance + acknowledgment out of lifelong memory / optimizer traces.
    #[test]
    fn describe_requests_are_flagged_transient() {
        for text in [
            "describe my screen",
            "what am I looking at",
            "describe this image cat.png",
            "ask my screen what is the error",
        ] {
            assert!(is_describe_request(text), "{text:?} must be flagged transient");
        }
        // Unrelated turns are not describe requests (they learn normally).
        for text in ["what's the weather", "open vision", "remember my birthday is may third"] {
            assert!(!is_describe_request(text), "{text:?} must NOT be a describe");
        }
    }

    /// GATE + FALLBACK (honesty-first): [vision] ships ON (full-power default) but
    /// INERT WITHOUT A MODEL (model="") — an IMAGE describe NEVER calls the VLM op and
    /// NEVER fabricates a description; it returns an honest gate line and emits the
    /// vision.describe telemetry as unavailable. Hermetic: no real model, no socket
    /// touched (the empty-model gate short-circuits before any op call), an empty app
    /// registry.
    #[tokio::test]
    async fn describe_image_gate_inert_without_model_falls_back_honestly_no_op_call() {
        let cfg = Config::default(); // [vision] enabled=true but model="" => inert
        assert!(cfg.vision.enabled, "precondition: VLM ships ON (full-power default)");
        assert!(cfg.vision.model.trim().is_empty(), "precondition: no VLM model configured (inert)");
        let registry = crate::apps::AppRegistry::discover(std::path::Path::new("/nonexistent"));
        // A lazy client pointed at a socket that does not exist; the gate path
        // must NOT reach it (proving no op is called when off).
        let mut infer = InferenceClient::new(std::path::PathBuf::from("/nonexistent/inference.sock"));
        let out = handle_describe(
            DescribeRequest::Image { path: "anything.png".to_string(), question: None },
            &cfg,
            &mut infer,
            &registry,
            std::path::Path::new("/tmp"),
        )
        .await;
        assert!(out.llm_voice, "the describe reply is persona-voiced");
        let low = out.data.to_lowercase();
        assert!(
            low.contains("on-device") && (low.contains("isn't downloaded") || low.contains("turned off") || low.contains("vision-language")),
            "off-gate copy must be honest about the on-device, not-set-up VLM: {:?}",
            out.data
        );
        // CRUCIAL: it is NOT a fabricated description (no invented scene content).
        assert!(!low.contains("i can see"), "must never fabricate a description: {:?}", out.data);
    }

    /// PATH CONFINEMENT (no escape): describe_confined_path REJECTS a path that
    /// resolves OUTSIDE the allowed root BEFORE any op call — a `..` traversal, an
    /// absolute-elsewhere path, and a nonexistent path all return an honest Err
    /// (never a description, never sent to the op). Hermetic: the reject happens
    /// before infer is ever touched. Mirrors the docsearch::confine red-team pin.
    #[tokio::test]
    async fn describe_path_confinement_rejects_escapes_before_any_op() {
        let root = std::env::temp_dir().join(format!("darwin-vlm-confine-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        // A real allowed root with one real image inside it.
        let inside = root.join("ok.png");
        std::fs::write(&inside, b"\x89PNG\r\n\x1a\n").unwrap();

        let mut infer = InferenceClient::new(std::path::PathBuf::from("/nonexistent/inference.sock"));

        // 1) Absolute-elsewhere (outside the root) -> REJECTED.
        let r = describe_confined_path(
            std::path::Path::new("/etc/hosts"),
            None,
            &mut infer,
            &root,
            "image",
        )
        .await;
        assert!(r.is_err(), "an absolute-elsewhere path must be rejected");
        assert!(r.unwrap_err().to_lowercase().contains("allowed"), "honest reject reason");

        // 2) `..` traversal escaping the root -> REJECTED.
        let escape = root.join("..").join("escape.png");
        let r = describe_confined_path(&escape, None, &mut infer, &root, "image").await;
        assert!(r.is_err(), "a `..` escape must be rejected");

        // 3) A nonexistent path (cannot canonicalize) -> REJECTED.
        let r = describe_confined_path(
            &root.join("does-not-exist.png"),
            None,
            &mut infer,
            &root,
            "image",
        )
        .await;
        assert!(r.is_err(), "a nonexistent path must be rejected (never sent)");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// ROUTING PIN: the describe intent re-pins the active agent to VISION (the
    /// vision owner). The route does `agents.get(VISION_APP)`; this pins that the
    /// Vision agent exists in the canonical roster and is resolvable by that key,
    /// so a describe turn is owned by Vision (the HUD + persona track it).
    #[test]
    fn describe_routes_to_the_vision_agent() {
        let reg = AgentRegistry::canonical();
        let vision = reg.get(VISION_APP).expect("the Vision agent must be in the roster");
        assert_eq!(vision.name, "vision");
        assert_eq!(vision.namespace, "agent.vision");
        // And every describe phrase is a describe request that triggers the re-pin.
        for text in ["describe my screen", "what am I looking at", "describe this image x.png"] {
            assert!(describe_command(text).is_some(), "{text:?} drives the Vision re-pin");
        }
    }

    // ----- IMAGE GENERATION (task #18) — on-device text->image, OFF/opt-in ----

    /// "generate/make/draw/create an image of X" maps to a GenerateImageRequest
    /// carrying the extracted PROMPT (the subject after the connector). PURE — no
    /// socket, no model, no classifier.
    #[test]
    fn generate_image_phrases_carry_the_prompt() {
        let cases = [
            ("generate an image of a red bicycle", "a red bicycle"),
            ("make a picture of an astronaut riding a horse", "an astronaut riding a horse"),
            ("draw a drawing of a cat in a hat", "a cat in a hat"),
            ("create an illustration showing a sunset over mountains", "a sunset over mountains"),
            ("paint a painting depicting a stormy sea", "a stormy sea"),
        ];
        for (text, want_prompt) in cases {
            let req = generate_image_command(text)
                .unwrap_or_else(|| panic!("{text:?} must be an image-generation request"));
            assert_eq!(req.prompt, want_prompt, "prompt extraction for {text:?}");
        }
        // The subject extractor keeps the full tail (an "of X with Y" stays whole).
        assert_eq!(
            extract_image_prompt("draw a picture of a dog with a hat").as_deref(),
            Some("a dog with a hat")
        );
    }

    /// PANIC PIN (no-regression): extract_image_prompt must never panic on an STT
    /// transcript whose lowercase form is NOT byte-length-preserving. The dotted
    /// capital 'İ' (U+0130, 2 bytes) lowercases to "i̇" (3 bytes), so the old
    /// `lower.find()` byte offset was wrong for the ORIGINAL `text` and slicing it
    /// landed mid-codepoint or past the end — panicking the always-on daemon
    /// (transcripts are untrusted multilingual input awaited inline in main's
    /// event loop). The fix scans `text`'s own char boundaries, so `start` is
    /// always valid IN `text`. These inputs reproduced the pre-fix panic.
    #[test]
    fn extract_image_prompt_never_panics_on_offset_shifting_lowercase() {
        // Each call must return (Some/None) without panicking on a char boundary.
        // 'İ' before/around the matched connector is the offset-shift trigger.
        for text in [
            "draw İ a photo of İcat",
            "İ art of 🐱",
            "İİİ picture of x",
            "draw a picture İ of İ a cat",
            "İ",                  // lone offset-shifter, no connector
            "İ of İ",             // connector flanked by shifters
        ] {
            // Just exercising the extractor — the contract is "no panic".
            let _ = extract_image_prompt(text);
            // And the public entry point that flows from the live transcript.
            let _ = generate_image_command(text);
            let _ = is_generate_image_request(text);
        }
        // Subject after the connector survives intact even with a leading 'İ'.
        assert_eq!(
            extract_image_prompt("draw İ a photo of İcat").as_deref(),
            Some("İcat"),
            "tail after the connector is preserved (original case + multibyte char)"
        );
        // No connector -> None (no guess), even when the only content is a shifter.
        assert_eq!(extract_image_prompt("İ").as_deref(), None);
    }

    /// CONTRACT PIN (no-regression): image GENERATION and VLM DESCRIBE are DISTINCT
    /// intents and never poach each other. A describe verb ("describe", "what's
    /// in") is NEVER an image-generation request; a generate verb ("draw an image
    /// of X") is NEVER a describe request. Mutually exclusive by construction. Also:
    /// a non-image "make me a sandwich" is NOT image generation (needs an image
    /// noun), and a bare "generate an image" with no subject yields no prompt.
    #[test]
    fn generate_image_and_describe_are_distinct_and_well_scoped() {
        // Describe verbs -> describe, NOT generate.
        for d in ["describe my screen", "what am I looking at", "describe this image cat.png", "what's in photo.jpg"] {
            assert!(describe_command(d).is_some(), "{d:?} must stay a VLM describe");
            assert!(generate_image_command(d).is_none(), "{d:?} must NOT be image generation");
        }
        // Generate verbs -> generate, NOT describe.
        for g in ["generate an image of a dog", "draw a picture of a house", "make an illustration of a robot"] {
            assert!(generate_image_command(g).is_some(), "{g:?} must be image generation");
            assert!(describe_command(g).is_none(), "{g:?} must NOT be a VLM describe");
        }
        // A non-image "make" request needs an image NOUN — never poached.
        for not_img in ["make me a sandwich", "draw the curtains", "what's the weather", "open vision"] {
            assert!(generate_image_command(not_img).is_none(), "{not_img:?} must NOT be image generation");
        }
        // A bare generate with no subject -> no prompt -> not a request (no guess).
        assert!(generate_image_command("generate an image").is_none(), "no subject => no prompt");
    }

    /// PRIVACY PIN: an image-generation turn is flagged transient (its prompt +
    /// the generated image can be personal, and both stay on-device) — so main.rs
    /// keeps its utterance + acknowledgment out of lifelong memory / optimizer
    /// traces, exactly like the VLM describe / OCR reads.
    #[test]
    fn generate_image_requests_are_flagged_transient() {
        for text in ["generate an image of a dog", "draw a picture of my house", "make an illustration of a robot"] {
            assert!(is_generate_image_request(text), "{text:?} must be flagged transient");
        }
        for text in ["what's the weather", "describe my screen", "remember my birthday is may third"] {
            assert!(!is_generate_image_request(text), "{text:?} must NOT be an image-generation turn");
        }
    }

    /// GATE + FALLBACK (honesty-first): [image] ships ON (full-power default) but
    /// INERT WITHOUT A MODEL (model=""), an image-generation request NEVER calls the
    /// generate_image op and NEVER fabricates an image — it returns an honest "not set
    /// up" line and emits the image.generated telemetry as unavailable. CRUCIALLY there
    /// is NO cloud fallback. Hermetic: no real model, no socket touched (the
    /// empty-model gate short-circuits before any op call) — the client points at a
    /// nonexistent socket to prove no op is reached.
    #[tokio::test]
    async fn generate_image_gate_inert_without_model_reports_honestly_no_op_no_cloud() {
        let cfg = Config::default(); // [image] enabled=true but model="" => inert
        assert!(cfg.image.enabled, "precondition: image generation ships ON (full-power default)");
        assert!(cfg.image.model.trim().is_empty(), "precondition: no image model configured (inert)");
        // A lazy client pointed at a socket that does not exist; the gate path must
        // NOT reach it (proving no op is called when off).
        let mut infer = InferenceClient::new(std::path::PathBuf::from("/nonexistent/inference.sock"));
        let out = handle_generate_image(
            GenerateImageRequest { prompt: "a red bicycle".to_string() },
            &cfg,
            &mut infer,
        )
        .await;
        assert!(out.llm_voice, "the image reply is persona-voiced");
        let low = out.data.to_lowercase();
        assert!(
            low.contains("on-device") && (low.contains("isn't set up") || low.contains("turned off") || low.contains("image model")),
            "off-gate copy must be honest about the on-device, not-set-up image model: {:?}",
            out.data
        );
        // CRUCIAL: it never fabricates an image and never mentions a cloud fallback.
        assert!(!low.contains("here is"), "must never claim a fabricated image: {:?}", out.data);
        assert!(
            low.contains("won't") || low.contains("cloud") || low.contains("on-device"),
            "must be honest there is no cloud fallback: {:?}",
            out.data
        );
    }

    /// ROUTING PIN: the image-generation intent re-pins the active agent to VISION
    /// (the visual-capability owner, same as describe). The route does
    /// `agents.get(VISION_APP)`; this pins that the Vision agent is resolvable by
    /// that key, so an image-generation turn is owned by Vision (the HUD + persona
    /// track it).
    #[test]
    fn generate_image_routes_to_the_vision_agent() {
        let reg = AgentRegistry::canonical();
        let vision = reg.get(VISION_APP).expect("the Vision agent must be in the roster");
        assert_eq!(vision.name, "vision");
        for text in ["generate an image of a dog", "draw a picture of a house"] {
            assert!(generate_image_command(text).is_some(), "{text:?} drives the Vision re-pin");
        }
    }

    // ----- AUDIO SCENE UNDERSTANDING (task #15) ------------------------------

    /// The "identify this sound" intent recognizes the sound-scene phrasings and
    /// is DISTINCT from STT (speech transcription): a "what did X say" / transcribe
    /// phrasing must NEVER be read as a sound-identify request (it falls through to
    /// the speech path). PURE — no socket, no mic, no app.
    #[test]
    fn identify_sound_intent_recognizes_sound_queries_and_is_distinct_from_stt() {
        // SOUND-scene queries -> identify-sound.
        for q in [
            "what was that sound",
            "what was that noise",
            "what's that sound",
            "identify that sound",
            "name that sound",
            "what am I hearing",
            "what do you hear",
            "what kind of sound was that",
        ] {
            assert!(is_identify_sound_request(q), "should be a sound-identify: {q:?}");
        }
        // STT / speech-transcription phrasings -> NOT identify-sound (stay distinct).
        for q in [
            "what did I say",
            "what did he say",
            "what did she say",
            "transcribe that",
            "transcribe what I said",
            "what were the words",
            "what did you hear me say",
        ] {
            assert!(
                !is_identify_sound_request(q),
                "a speech/transcription phrasing must NOT be a sound-identify (STT stays distinct): {q:?}"
            );
        }
        // Plain/unrelated turns are not sound-identify requests.
        for q in ["what's the weather", "open vision", "play some music", "what time is it"] {
            assert!(!is_identify_sound_request(q), "{q:?} must NOT be a sound-identify");
        }
    }

    /// The intent supplies the clip the daemon ALREADY captured (caller-provided),
    /// NEVER a user-named path and NEVER a fresh capture. When the intent fires but
    /// there is no clip, it STILL routes (clip=None) so the handler answers
    /// honestly — it does not fall through to a generic answer or open the mic.
    #[test]
    fn identify_sound_uses_the_already_captured_clip_never_opens_the_mic() {
        let clip = std::path::Path::new("/tmp/darwin/state/tmp/sound-clip.wav");
        // Intent + a captured clip available -> route with that exact clip.
        let req = identify_sound_clip_or_request("what was that sound", Some(clip))
            .expect("a sound-identify with a clip routes");
        assert_eq!(req.clip.as_deref(), Some(clip), "the daemon's captured clip is supplied verbatim");

        // Intent fires but NO clip captured -> still routes, with clip=None (the
        // handler reports it honestly; the mic is never opened to make one).
        let req = identify_sound_clip_or_request("identify that noise", None)
            .expect("a sound-identify still routes with no clip");
        assert_eq!(req.clip, None, "no clip => clip:None, never a fabricated/fresh capture");

        // Not a sound-identify -> None (falls through to normal routing) regardless
        // of whether a clip exists.
        assert!(identify_sound_clip_or_request("what's the weather", Some(clip)).is_none());
        assert!(identify_sound_clip_or_request("what did I say", Some(clip)).is_none());
    }

    /// The classify.sound op line is EXACTLY the Swift Op.swift wire form: the
    /// `{"type":"op"}` envelope, op "classify.sound", a REQUIRED `path` (mirrors
    /// describe.capture). serde_json frames it so a path with a quote can't break it.
    #[test]
    fn op_classify_sound_matches_the_swift_wire_form() {
        let line = op_classify_sound("/tmp/state/tmp/sound-clip.wav");
        let v: serde_json::Value = serde_json::from_str(&line).expect("valid JSON op line");
        assert_eq!(v["type"], "op");
        assert_eq!(v["op"], "classify.sound");
        assert_eq!(v["path"], "/tmp/state/tmp/sound-clip.wav", "path is required + verbatim");
        // A path with a quote stays valid JSON (no framing break).
        let q = op_classify_sound("/tmp/a\"b.wav");
        let v: serde_json::Value = serde_json::from_str(&q).expect("a quote in the path can't break framing");
        assert_eq!(v["path"], "/tmp/a\"b.wav");
    }

    /// The clip path the daemon supplies for a one-shot classification is under the
    /// project state dir (the allowlisted root) — the same place the VAD/cpal
    /// capture writes its utterance WAVs — so no new microphone is opened.
    #[test]
    fn sound_clip_path_is_under_the_state_tmp_dir() {
        let p = sound_clip_path(std::path::Path::new("/srv/darwin"));
        assert_eq!(p, std::path::Path::new("/srv/darwin/state/tmp/sound-clip.wav"));
    }

    /// HERMETIC ROUTING: the identify-sound handler over a CONFINED, real clip
    /// INVOKES apps::send_op for the VISION app (it is the only call that can
    /// produce the "not running" outcome). With the Vision app registered but NOT
    /// running, the handler reaches send_op, which rejects — proving the op was
    /// dispatched to Vision (the classify.sound wire form is pinned separately by
    /// `op_classify_sound_matches_the_swift_wire_form`). No socket is bound and no
    /// child is spawned: the registry's running flag is the gate, exactly like
    /// apps.rs's `send_op_rejects_unknown_and_not_running_apps`.
    #[tokio::test]
    async fn identify_sound_handler_invokes_classify_sound_via_send_op() {
        use crate::apps::AppRegistry;

        let root = std::env::temp_dir().join(format!(
            "darwin-idsound-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 1_000_000
        ));
        // A real clip under the allowed root so confinement PASSES (so the test
        // exercises the send_op call, not the confinement reject).
        let clip_dir = root.join("state").join("tmp");
        std::fs::create_dir_all(&clip_dir).unwrap();
        let clip = clip_dir.join("sound-clip.wav");
        std::fs::write(&clip, b"RIFF....WAVEfmt ").unwrap();

        // Register the VISION app name (the handler hard-codes VISION_APP). It is
        // discovered but NOT running — so send_op rejects with "not running",
        // proving the handler dispatched the op to Vision (never to anything else,
        // never a fabricated label). No socket bound, no child spawned.
        let app_dir = root.join("apps").join(VISION_APP);
        std::fs::create_dir_all(&app_dir).unwrap();
        let manifest = format!(
            r#"
            [app]
            name = "{VISION_APP}"
            version = "0.1.0"
            description = "hermetic test stand-in for the vision app"
            entry = "apps/{VISION_APP}/main.py"
            runtime = "python"
            [permissions]
            audio = false
            gpu = false
            net_hosts = []
            fs_read = []
            fs_write = ["state/apps/{VISION_APP}"]
            [ui]
            surface = "panel"
            telemetry_topics = ["vision.sound"]
        "#
        );
        std::fs::write(app_dir.join("manifest.toml"), manifest).unwrap();

        let registry = AppRegistry::discover(&root);

        let out = handle_identify_sound(Some(clip.clone()), &registry, &root).await;
        assert!(out.llm_voice, "the reply is persona-voiced");
        let low = out.data.to_lowercase();
        // The op reached send_op for VISION (registered but not running) -> the
        // honest "couldn't reach Vision / open it first" copy. This is the
        // not-running send_op outcome — proof the classify.sound op was dispatched
        // to the Vision app (a confinement reject or a no-clip path would NOT say
        // "reach vision"). Never a fabricated sound class on this path either.
        assert!(
            low.contains("reach vision") || low.contains("open it first"),
            "the handler must INVOKE send_op for Vision (not-running outcome): {:?}",
            out.data
        );
        for invented in ["doorbell", "alarm", "glass", "music"] {
            assert!(!low.contains(invented), "must never fabricate a class on the transport path: {:?}", out.data);
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    /// HONESTY: with NO captured clip the handler says so plainly — it NEVER
    /// fabricates a sound class and NEVER opens the mic to make one. Hermetic: no
    /// running app needed (the None arm short-circuits before any op call).
    #[tokio::test]
    async fn identify_sound_with_no_clip_is_honest_never_fabricates() {
        let registry = crate::apps::AppRegistry::discover(std::path::Path::new("/nonexistent"));
        let out = handle_identify_sound(None, &registry, std::path::Path::new("/tmp")).await;
        assert!(out.llm_voice);
        let low = out.data.to_lowercase();
        assert!(
            low.contains("don't have") || low.contains("no ") || low.contains("recent sound clip"),
            "must honestly report no clip: {:?}",
            out.data
        );
        assert!(low.contains("never opens the mic") || low.contains("on-device"), "honest gate copy: {:?}", out.data);
        // CRUCIAL: never a fabricated class.
        for invented in ["doorbell", "alarm", "glass", "music", "i hear"] {
            assert!(!low.contains(invented), "must never fabricate a sound class ({invented}): {:?}", out.data);
        }
    }

    /// PATH CONFINEMENT: an identify-sound clip OUTSIDE the allowed root is REJECTED
    /// before any op — the handler refuses to classify it and never forwards a
    /// thing. Hermetic: no running app (the reject precedes the op call).
    #[tokio::test]
    async fn identify_sound_rejects_a_clip_outside_the_allowed_root() {
        let root = std::env::temp_dir().join(format!("darwin-idsound-confine-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let registry = crate::apps::AppRegistry::discover(std::path::Path::new("/nonexistent"));
        // An absolute-elsewhere clip (outside the root) -> REJECTED, never sent.
        let out = handle_identify_sound(
            Some(std::path::PathBuf::from("/etc/hosts")),
            &registry,
            &root,
        )
        .await;
        let low = out.data.to_lowercase();
        assert!(
            low.contains("allowed") || low.contains("won't classify"),
            "an out-of-root clip must be refused honestly: {:?}",
            out.data
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// MONITOR GATE: the ambient sound monitor's spawn gate is the pure
    /// `ambient_monitor_should_start(enabled)`. With the flag OFF the gate is false —
    /// the monitor never auto-starts. The shipped DEFAULT is now ON (full-power), but
    /// it is INERT WITHOUT MIC/TCC: even with the gate true the device-gated mic loop
    /// captures nothing without Microphone consent. This is the pure half of main.rs's
    /// spawn gate.
    #[test]
    fn ambient_monitor_gate_is_the_flag_and_default_is_on() {
        // Flag OFF -> the monitor must not start (the off path is intact).
        assert!(
            !ambient_monitor_should_start(false),
            "with the flag off the ambient monitor must NOT auto-start"
        );
        // The config default is now ON (full-power) — defense in depth: the gate
        // tracks the flag, and the mic loop is still TCC-gated at runtime.
        assert!(
            Config::default().audio.sound_monitor,
            "[audio].sound_monitor ships ON (full-power default; inert without mic/TCC)"
        );
        assert!(
            ambient_monitor_should_start(Config::default().audio.sound_monitor),
            "the default config arms the gate (the mic loop still needs TCC consent at runtime)"
        );
        // The flag is the only thing the pure gate reads.
        assert!(
            ambient_monitor_should_start(true),
            "an enabled flag opens the spawn gate"
        );
    }

    /// The IdentifySoundRequest is a small, comparable carrier; this pins its shape
    /// (clip Option) so the routing + handler agree on the contract.
    #[test]
    fn identify_sound_request_carries_only_the_clip() {
        let r = IdentifySoundRequest { clip: Some(std::path::PathBuf::from("/x/y.wav")) };
        assert_eq!(r.clip.unwrap().to_str(), Some("/x/y.wav"));
        let none = IdentifySoundRequest { clip: None };
        assert!(none.clip.is_none());
    }

    /// Unrelated utterances never produce a Vision command (so they fall through
    /// to normal routing) — including ones that merely share a stray keyword.
    #[test]
    fn vision_command_ignores_unrelated_utterances() {
        for text in [
            "what's the weather",
            "open safari",
            "play some music",
            "what do you think about the market",
            "set a timer for ten minutes",
            "tell me a joke",
            // "watch" with no Vision sense + no Vision app mention is still a
            // watch verb; ensure a non-watch sentence doesn't trip it.
            "i'll be back in a minute",
        ] {
            assert_eq!(
                vision_command(text),
                None,
                "{text:?} must not be a Vision command"
            );
        }
    }

    /// An oversize / junk utterance is handled cleanly: vision_command returns
    /// None (no panic, no allocation blowup) so the turn falls through to normal
    /// routing — the daemon never forwards a malformed op, and the app's own
    /// Op.decode is the final total-decode backstop for anything that does
    /// reach it.
    #[test]
    fn vision_command_handles_oversize_and_junk_cleanly() {
        // A very long string with no Vision phrase -> None, no panic.
        let huge = "lorem ipsum ".repeat(5000);
        assert_eq!(vision_command(&huge), None);
        // Pure punctuation / empty -> None.
        assert_eq!(vision_command(""), None);
        assert_eq!(vision_command("??? --- ..."), None);
        // A Vision phrase buried in a huge string still resolves to a valid op
        // (and serde framing stays well-formed) rather than choking.
        let buried = format!("{huge} what do you see {huge}");
        assert_vision_op(&buried, r#"{"type":"op","op":"status"}"#);
    }

    // ======================================================================
    // Nexus voice control (SPEC §6). Mirrors the Silicon Canvas / Vision tests.
    // The wire form pinned here is the BARE `{"op":...}` object the Nexus
    // OpDispatcher (apps/nexus/main.py) reads — NOT the Vision `{"type":"op"}`
    // envelope. Each expected line matches the SPEC §5 op table and the Python
    // dispatch handlers verbatim, so a pass here proves the daemon emits ops the
    // Nexus control plane already accepts.
    // ======================================================================

    /// Assert the utterance maps to a Nexus Op carrying EXACTLY this JSON wire
    /// string (compared as parsed JSON so key order is irrelevant; the op-tag +
    /// fields are what the contract pins).
    fn assert_nexus_op(text: &str, expected_json: &str) {
        match nexus_command(text) {
            Some(NexusCommand::Op(line)) => {
                let got: serde_json::Value = serde_json::from_str(&line).unwrap();
                let want: serde_json::Value = serde_json::from_str(expected_json).unwrap();
                assert_eq!(got, want, "for utterance {text:?}");
            }
            other => panic!("expected a Nexus Op for {text:?}, got {other:?}"),
        }
    }

    /// "open/launch/start/bring up nexus" (and its capability aliases) is a
    /// LAUNCH; the app name is the manifest name the registry keys on.
    #[test]
    fn nexus_launch_phrases() {
        assert_eq!(NEXUS_APP, "nexus");
        for text in [
            "open nexus",
            "launch nexus",
            "start nexus",
            "darwin, bring up nexus",
            "bring up the routing matrix",
            "open the mixer",
        ] {
            assert_eq!(
                nexus_command(text),
                Some(NexusCommand::Launch),
                "{text:?} should be a Nexus launch"
            );
        }
        // "nexus" must be a whole word — never inside another token.
        assert!(nexus_command("open the connexus dashboard").is_none());
    }

    /// "mute the mic" -> gain.set {mute:true} on the default mic input (0),
    /// input stage; "unmute input 2" -> gain.set {mute:false} on channel 2.
    #[test]
    fn nexus_mute_maps_to_gain_set_mute() {
        assert_nexus_op(
            "mute the mic",
            r#"{"op":"gain.set","channel":0,"mute":true,"stage":"input"}"#,
        );
        assert_nexus_op(
            "mute the microphone",
            r#"{"op":"gain.set","channel":0,"mute":true,"stage":"input"}"#,
        );
        // An explicit channel overrides the mic default.
        assert_nexus_op(
            "mute input 2",
            r#"{"op":"gain.set","channel":2,"mute":true,"stage":"input"}"#,
        );
        // Unmute flips the boolean (and never reads as a fresh mute).
        assert_nexus_op(
            "unmute the mic",
            r#"{"op":"gain.set","channel":0,"mute":false,"stage":"input"}"#,
        );
        assert_nexus_op(
            "unmute input 1",
            r#"{"op":"gain.set","channel":1,"mute":false,"stage":"input"}"#,
        );
    }

    /// "set input gain to -18" -> gain.set {gain_db:-18, stage:input}; an output
    /// phrasing targets the output stage; the spoken sign word is handled.
    #[test]
    fn nexus_gain_set_maps_to_gain_set_value() {
        assert_nexus_op(
            "set input gain to -18",
            r#"{"op":"gain.set","channel":0,"gain_db":-18.0,"stage":"input"}"#,
        );
        // "minus" spelled out (STT) + a dB suffix.
        assert_nexus_op(
            "set the input gain to minus 6 db",
            r#"{"op":"gain.set","channel":0,"gain_db":-6.0,"stage":"input"}"#,
        );
        // Explicit input channel.
        assert_nexus_op(
            "set the gain on input 1 to -12",
            r#"{"op":"gain.set","channel":1,"gain_db":-12.0,"stage":"input"}"#,
        );
        // Output stage (named output channel).
        assert_nexus_op(
            "set output 1 gain to -3",
            r#"{"op":"gain.set","channel":1,"gain_db":-3.0,"stage":"output"}"#,
        );
        // "the gain" with no number is NOT a gain.set (no dB value -> falls
        // through to normal routing).
        assert!(nexus_command("turn up the gain").is_none());
    }

    /// "route input 1 to the monitor" -> route.set on the monitor bus (output 0)
    /// at unity; an explicit output is honored; "unroute" clears (-inf sentinel).
    #[test]
    fn nexus_route_maps_to_route_set() {
        // "to the monitor" -> the monitor bus output (0), 0 dB unity.
        assert_nexus_op(
            "route input 1 to the monitor",
            r#"{"op":"route.set","in":1,"out":0,"gain_db":0.0}"#,
        );
        // Explicit input + output.
        assert_nexus_op(
            "route input 2 to output 3",
            r#"{"op":"route.set","in":2,"out":3,"gain_db":0.0}"#,
        );
        // Unroute clears the crosspoint with the "-inf" string sentinel that
        // Nexus's _route_set maps back to float("-inf").
        assert_nexus_op(
            "unroute input 1 from output 3",
            r#"{"op":"route.set","in":1,"out":3,"gain_db":"-inf"}"#,
        );
    }

    /// "monitor input 1" -> monitor.set {on:true}; "stop monitoring" -> off. The
    /// monitor toggle is distinct from a generic crosspoint route.set.
    #[test]
    fn nexus_monitor_maps_to_monitor_set() {
        assert_nexus_op(
            "monitor input 1",
            r#"{"op":"monitor.set","in":1,"out":0,"on":true}"#,
        );
        assert_nexus_op(
            "stop monitoring",
            r#"{"op":"monitor.set","in":0,"out":0,"on":false}"#,
        );
        assert_nexus_op(
            "turn off the monitor",
            r#"{"op":"monitor.set","in":0,"out":0,"on":false}"#,
        );
    }

    /// "load the <name> preset" / "load preset <name>" -> preset.load {name},
    /// forwarded verbatim (Nexus resolves it against presets/).
    #[test]
    fn nexus_preset_load_maps_to_preset_load() {
        assert_nexus_op(
            "load the vocal preset",
            r#"{"op":"preset.load","name":"vocal"}"#,
        );
        assert_nexus_op(
            "load preset podcast",
            r#"{"op":"preset.load","name":"podcast"}"#,
        );
        assert_nexus_op(
            "recall the streaming preset",
            r#"{"op":"preset.load","name":"streaming"}"#,
        );
        // A preset name with a hyphen survives the tokenizer.
        assert_nexus_op(
            "load the voice-over preset",
            r#"{"op":"preset.load","name":"voice-over"}"#,
        );
        // "load a preset" with no name -> not actionable, falls through.
        assert!(nexus_command("load a preset").is_none());
    }

    /// "what are the levels" / "show me the meters" / "what's the routing state"
    /// -> state.get (a read-only snapshot request).
    #[test]
    fn nexus_levels_query_maps_to_state_get() {
        let state = r#"{"op":"state.get"}"#;
        assert_nexus_op("what are the levels", state);
        assert_nexus_op("show me the meters", state);
        assert_nexus_op("what's the routing state", state);
        assert_nexus_op("read out the matrix", state);
        assert_nexus_op("what is currently routed", state);
    }

    /// Unrelated utterances never produce a Nexus command (so they fall through
    /// to normal routing) — including ones that merely share a stray keyword, and
    /// the other apps' control phrases (no cross-app capture).
    #[test]
    fn nexus_command_ignores_unrelated_utterances() {
        for text in [
            "what's the weather",
            "open safari",
            "play some music",
            "tell me a joke",
            "find my budget spreadsheet",
            "open apple.com",
            // Silicon Canvas / Vision phrases must NOT be captured by Nexus.
            "show me the 3V3 net",
            "what do you see",
            "run erc",
            // "matrix" inside an unrelated open verb context is a launch-class
            // word, not a state query — but with no Nexus mention it is nothing.
            "tell me about the matrix movie",
        ] {
            assert_eq!(
                nexus_command(text),
                None,
                "{text:?} must not be a Nexus command"
            );
        }
    }

    /// An oversize / junk utterance is handled cleanly: nexus_command returns
    /// None (no panic) so the turn falls through to normal routing, and a Nexus
    /// phrase buried in a huge string still resolves to a well-formed op.
    #[test]
    fn nexus_command_handles_oversize_and_junk_cleanly() {
        let huge = "lorem ipsum ".repeat(5000);
        assert_eq!(nexus_command(&huge), None);
        assert_eq!(nexus_command(""), None);
        assert_eq!(nexus_command("??? --- ..."), None);
        let buried = format!("{huge} mute the mic {huge}");
        assert_nexus_op(
            &buried,
            r#"{"op":"gain.set","channel":0,"mute":true,"stage":"input"}"#,
        );
    }

    // ======================================================================
    // Mark-Forge voice control (SPEC §7). Mirrors the Silicon Canvas / Vision /
    // Nexus tests. The wire form pinned here is the BARE `{"op":...}` object the
    // Mark-Forge engine (apps/mark-forge/src/ipc.rs) deserializes via its
    // `#[serde(tag = "op")]` Op enum — NOT the Vision `{"type":"op"}` envelope.
    // Each expected line matches the SPEC §7 op table and the app's own
    // round-trip tests verbatim (op_deserializes_with_dotted_names,
    // body_spawn_deserializes_with_optional_fields), so a pass here proves the
    // daemon emits ops the engine already accepts.
    // ======================================================================

    /// Assert the utterance maps to a Mark-Forge Op carrying EXACTLY this JSON
    /// wire string (compared as parsed JSON so key order is irrelevant; the
    /// op-tag + fields are what the contract pins).
    fn assert_mark_forge_op(text: &str, expected_json: &str) {
        match mark_forge_command(text) {
            Some(MarkForgeCommand::Op(line)) => {
                let got: serde_json::Value = serde_json::from_str(&line).unwrap();
                let want: serde_json::Value = serde_json::from_str(expected_json).unwrap();
                assert_eq!(got, want, "for utterance {text:?}");
            }
            other => panic!("expected a Mark-Forge Op for {text:?}, got {other:?}"),
        }
    }

    /// "open/launch/start the physics sandbox" (and its aliases) is a LAUNCH;
    /// the app name is the manifest name the registry keys on.
    #[test]
    fn mark_forge_launch_phrases() {
        assert_eq!(MARK_FORGE_APP, "mark-forge");
        for text in [
            "open the physics sandbox",
            "launch the physics sandbox",
            "start mark forge",
            "darwin, bring up the physics sandbox",
            "open mark-forge",
            "fire up the physics engine",
            "show me the sandbox",
        ] {
            assert_eq!(
                mark_forge_command(text),
                Some(MarkForgeCommand::Launch),
                "{text:?} should be a Mark-Forge launch"
            );
        }
        // "sandbox"/"sim" must be a whole word / real mention — never inside
        // another token, and a bare open verb with no Mark-Forge mention falls
        // through to the macOS launcher.
        assert!(mark_forge_command("open safari").is_none());
    }

    /// "drop a box|cube" -> body.spawn of a dynamic cuboid a few metres up; the
    /// shape is tagged on `kind` and the vectors serialize as `[x,y,z]` arrays
    /// (exactly the SpawnSpec wire form the engine deserializes).
    #[test]
    fn mark_forge_drop_box_maps_to_body_spawn_cuboid() {
        let want = r#"{"op":"body.spawn","shape":{"kind":"cuboid","half_extents":[0.5,0.5,0.5]},"pos":[0.0,5.0,0.0],"mass":1.0}"#;
        assert_mark_forge_op("drop a box", want);
        assert_mark_forge_op("drop a cube", want);
        assert_mark_forge_op("spawn a box", want);
        assert_mark_forge_op("add a crate", want);
        assert_mark_forge_op("darwin, drop a box in the sandbox", want);
        // The spawned body carries a POSITIVE mass so it is dynamic and actually
        // falls (a None/<=0 mass would be a static body that never moves).
        if let Some(MarkForgeCommand::Op(line)) = mark_forge_command("drop a box") {
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert!(v["mass"].as_f64().unwrap() > 0.0, "a dropped box must be dynamic");
        } else {
            panic!("expected a body.spawn op");
        }
    }

    /// "drop a ball|sphere" -> body.spawn of a dynamic sphere.
    #[test]
    fn mark_forge_drop_ball_maps_to_body_spawn_sphere() {
        let want = r#"{"op":"body.spawn","shape":{"kind":"sphere","radius":0.5},"pos":[0.0,5.0,0.0],"mass":1.0}"#;
        assert_mark_forge_op("drop a ball", want);
        assert_mark_forge_op("drop a sphere", want);
        assert_mark_forge_op("spawn a marble", want);
        // A ball noun wins over a box noun when both are absent of the other —
        // "drop a ball" is a sphere, not a cuboid.
        assert_mark_forge_op("throw a ball", want);
    }

    /// "reset/clear the simulation" -> world.reset, gated on a physics context so
    /// a bare "reset" elsewhere never wipes the world.
    #[test]
    fn mark_forge_reset_maps_to_world_reset() {
        let want = r#"{"op":"world.reset"}"#;
        assert_mark_forge_op("reset the simulation", want);
        assert_mark_forge_op("reset the physics sandbox", want);
        assert_mark_forge_op("clear the world", want);
        assert_mark_forge_op("wipe the scene", want);
        assert_mark_forge_op("reset everything in the sandbox", want);
        // "reset" with no physics context falls through to normal routing.
        assert!(mark_forge_command("reset my password").is_none());
        // "reset gravity" is NOT a world reset (it is a gravity op / falls
        // through) — a reset must never be triggered by the gravity word.
        assert!(
            !matches!(
                mark_forge_command("reset the gravity"),
                Some(MarkForgeCommand::Op(ref l)) if l.contains("world.reset")
            ),
            "reset gravity must not wipe the world"
        );
    }

    /// "set gravity to the moon|mars|earth|zero" / "turn off gravity" ->
    /// set.gravity with the matching fixed constant on the downward (y) axis.
    #[test]
    fn mark_forge_gravity_targets_map_to_set_gravity() {
        assert_mark_forge_op(
            "set gravity to the moon",
            r#"{"op":"set.gravity","x":0.0,"y":-1.62,"z":0.0}"#,
        );
        assert_mark_forge_op(
            "set gravity to mars",
            r#"{"op":"set.gravity","x":0.0,"y":-3.72,"z":0.0}"#,
        );
        assert_mark_forge_op(
            "set gravity back to earth",
            r#"{"op":"set.gravity","x":0.0,"y":-9.81,"z":0.0}"#,
        );
        assert_mark_forge_op(
            "set gravity to normal",
            r#"{"op":"set.gravity","x":0.0,"y":-9.81,"z":0.0}"#,
        );
        // Zero-g variants.
        assert_mark_forge_op(
            "turn off gravity",
            r#"{"op":"set.gravity","x":0.0,"y":0.0,"z":0.0}"#,
        );
        assert_mark_forge_op(
            "set gravity to zero",
            r#"{"op":"set.gravity","x":0.0,"y":0.0,"z":0.0}"#,
        );
        // "gravity" with no recognized target falls through (the daemon won't
        // guess a vector).
        assert!(mark_forge_command("what is gravity").is_none());
        // A bare "moon" / "mars" with no "gravity" word never fires.
        assert!(mark_forge_command("tell me about the moon").is_none());
    }

    /// "step" / "advance" -> world.step{n>=1} (N from the utterance, default 1);
    /// "pause"/"freeze" -> world.step{n:0}. Both gated on a physics context.
    #[test]
    fn mark_forge_step_and_pause_map_to_world_step() {
        // Single step (default 1 frame).
        assert_mark_forge_op("step the simulation", r#"{"op":"world.step","n":1}"#);
        assert_mark_forge_op("advance the physics", r#"{"op":"world.step","n":1}"#);
        assert_mark_forge_op("step the sandbox", r#"{"op":"world.step","n":1}"#);
        // An explicit frame count is honored.
        assert_mark_forge_op("step the simulation 10 frames", r#"{"op":"world.step","n":10}"#);
        assert_mark_forge_op("advance 5 frames", r#"{"op":"world.step","n":5}"#);
        // Pause -> a zero-frame step (advances no simulated time).
        assert_mark_forge_op("pause the simulation", r#"{"op":"world.step","n":0}"#);
        assert_mark_forge_op("freeze the physics", r#"{"op":"world.step","n":0}"#);
        assert_mark_forge_op("hold the simulation", r#"{"op":"world.step","n":0}"#);
        // A bare "pause" / "step" with no physics context falls through.
        assert!(mark_forge_command("pause the music").is_none());
        assert!(mark_forge_command("step outside for a minute").is_none());
    }

    /// A misheard huge step count is clamped to a sane bound so the engine is
    /// never asked to advance millions of frames synchronously.
    #[test]
    fn mark_forge_step_count_is_clamped() {
        if let Some(MarkForgeCommand::Op(line)) =
            mark_forge_command("step the simulation 99999999 frames")
        {
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(v["n"].as_u64().unwrap(), 10_000, "huge step count must clamp");
        } else {
            panic!("expected a world.step op");
        }
    }

    /// Unrelated utterances never produce a Mark-Forge command (so they fall
    /// through to normal routing) — including ones that share a stray keyword,
    /// and the other apps' control phrases (no cross-app capture).
    #[test]
    fn mark_forge_command_ignores_unrelated_utterances() {
        for text in [
            "what's the weather",
            "open safari",
            "play some music",
            "tell me a joke",
            "find my budget spreadsheet",
            "open apple.com",
            "drop me an email",            // "drop" without a shape noun
            "drop everything and call me", // "drop" without a shape noun
            "pause the music",             // pause outside a physics context
            "reset my password",           // reset outside a physics context
            // Other apps' phrases must NOT be captured by Mark-Forge.
            "show me the 3V3 net",
            "what do you see",
            "mute the mic",
            "tell me about the moon landing", // "moon" with no gravity word
        ] {
            assert_eq!(
                mark_forge_command(text),
                None,
                "{text:?} must not be a Mark-Forge command"
            );
        }
    }

    /// An oversize / junk utterance is handled cleanly: mark_forge_command
    /// returns None (no panic) so the turn falls through to normal routing, and a
    /// Mark-Forge phrase buried in a huge string still resolves to a well-formed
    /// op.
    #[test]
    fn mark_forge_command_handles_oversize_and_junk_cleanly() {
        let huge = "lorem ipsum ".repeat(5000);
        assert_eq!(mark_forge_command(&huge), None);
        assert_eq!(mark_forge_command(""), None);
        assert_eq!(mark_forge_command("??? --- ..."), None);
        let buried = format!("{huge} reset the simulation {huge}");
        assert_mark_forge_op(&buried, r#"{"op":"world.reset"}"#);
    }

    // ===== CAPABILITY SELECTOR — end-to-end with the SHIPPED scorer ==========
    // These exercise the exact wiring route() uses: crate::selector::classify_mode
    // with the production LexicalAgentScorer (no mock). They pin that the headline
    // cases route to the right capability, and that BOTH rails hold with the real
    // scorer — the selector never silently arms autonomy or a consequential action.

    use crate::agents::LexicalAgentScorer;
    use crate::selector::{classify_mode, Mode, Selection};

    /// "every morning brief me" -> standing (PROPOSED — it parks for confirm, the
    /// router maps Standing to propose_standing_mission; never silently created).
    #[test]
    fn selector_routes_recurring_request_to_standing_with_shipped_scorer() {
        assert_eq!(
            classify_mode("every morning brief me on my deadlines", &LexicalAgentScorer),
            Selection::Route(Mode::Standing)
        );
        assert_eq!(
            classify_mode("from now on keep watching the launch project", &LexicalAgentScorer),
            Selection::Route(Mode::Standing)
        );
    }

    /// "what's the status of the launch project" -> world_query (read-only).
    #[test]
    fn selector_routes_state_question_to_world_query_with_shipped_scorer() {
        assert_eq!(
            classify_mode("what's the status of the launch project", &LexicalAgentScorer),
            Selection::Route(Mode::WorldQuery)
        );
    }

    /// "the launch slipped to next Tuesday" -> world_update (shared tier only).
    #[test]
    fn selector_routes_stated_fact_to_world_update_with_shipped_scorer() {
        assert_eq!(
            classify_mode("the launch slipped to next Tuesday", &LexicalAgentScorer),
            Selection::Route(Mode::WorldUpdate)
        );
    }

    /// "plan and kick off the migration" -> mission (FURY).
    #[test]
    fn selector_routes_multistep_now_to_mission_with_shipped_scorer() {
        assert_eq!(
            classify_mode("plan and kick off the migration", &LexicalAgentScorer),
            Selection::Route(Mode::Mission)
        );
    }

    /// A plain action / normal question -> one_shot, UNCHANGED. The selector must
    /// not hijack the existing fast-cue routing for plain commands.
    #[test]
    fn selector_leaves_plain_requests_one_shot_with_shipped_scorer() {
        for q in [
            "open safari",
            "what time is it",
            "what's the weather",
            "play some jazz",
            "set a timer for ten minutes",
            "hi darwin",
        ] {
            assert_eq!(
                classify_mode(q, &LexicalAgentScorer),
                Selection::Route(Mode::OneShot),
                "plain request must stay one_shot (existing routing unchanged): {q}"
            );
        }
    }

    /// RAIL 1 with the real scorer: a genuinely ambiguous "look after my stuff"
    /// (no hard cue) must NEVER silently establish a standing mission or any
    /// consequential mode — it stays one_shot (safe-default) or clarifies, never
    /// Route(Standing).
    #[test]
    fn selector_never_silently_arms_autonomy_on_ambiguous_with_shipped_scorer() {
        for q in [
            "look after my deadlines for me",
            "handle my stuff",
            "deal with things",
            "take care of it",
        ] {
            let sel = classify_mode(q, &LexicalAgentScorer);
            assert_ne!(
                sel,
                Selection::Route(Mode::Standing),
                "an ambiguous request must never silently route to standing: {q} -> {sel:?}"
            );
            // It is either the safe default or an explicit clarify — never a
            // consequential mode arrived at by a guess.
            match sel {
                Selection::Route(m) => assert!(
                    !m.is_consequential(),
                    "ambiguous request reached a consequential mode {m:?}: {q}"
                ),
                Selection::Clarify(_) => {} // asking is allowed and safe.
            }
        }
    }

    // ---- ROUTER DISPATCH: notebook + life-log utterances route end-to-end ---
    // These exercise the EXACT composition the route() handler runs for these two
    // intents — classify the utterance, then dispatch it against the real store —
    // so the wiring is proven at the router layer without spinning up the live
    // InferenceClient / ReplySession / AppRegistry the full route() needs. Fully
    // hermetic: a temp Db + a SYNTHETIC last research run; NO fetch/model/network.

    use crate::memory::Memory;
    use crate::research::{Claim, ResearchReport, Source};
    use std::path::PathBuf;

    struct TempDb(PathBuf);
    impl TempDb {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("darwin-router-dispatch-{}-{}.db", std::process::id(), tag));
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

    /// A synthetic report whose ONLY grounded source is #1 (a phantom 999 + an
    /// uncited 0 are present, so the save must keep just the grounded one).
    fn synthetic_report() -> ResearchReport {
        ResearchReport {
            question: "what is X".into(),
            sources: vec![Source {
                id: 1,
                url: "https://a.test".into(),
                title: "Real A".into(),
                excerpt: "e".into(),
            }],
            claims: vec![Claim::new("a grounded point", 1), Claim::new("phantom", 999)],
            planned_subqueries: 1,
            pursued_subqueries: 1,
            truncated: false,
        }
    }

    #[tokio::test]
    async fn memory_store_keeps_every_distinct_note_never_clobbering_the_last() {
        // Regression (full-OS sweep): memory.store used the FIXED key
        // "<ns>.note", so a second note silently overwrote the first. Notes are
        // now content-keyed; identical text stays a no-growth upsert.
        let db = TempDb::new("note-clobber");
        let mem = Memory::open(&db.0).unwrap();
        let reg = AgentRegistry::canonical();
        let agent = reg.orchestrator();
        let apps = crate::apps::AppRegistry::discover(std::path::Path::new("/nonexistent"));
        let apps = std::sync::Arc::new(apps);

        for text in ["the wifi password is hidden", "buy oat milk", "buy oat milk"] {
            super::handle_local("memory.store", &serde_json::Value::Null, text, &mem, &apps, agent).await;
        }
        let facts = mem.agent_scoped_facts(&agent.namespace, 50).await.unwrap();
        let notes: Vec<&str> = facts
            .iter()
            .filter(|(k, _)| k.starts_with(&format!("{}.note.", agent.namespace)))
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(notes.len(), 2, "two distinct notes kept, duplicate deduped: {notes:?}");
        assert!(notes.contains(&"the wifi password is hidden"));
        assert!(notes.contains(&"buy oat milk"));
    }

    // =====================================================================
    // THRESHOLD — GUEST MODE: the structured-intent FAST PATH is gated
    // =====================================================================

    fn guest_scope_fixture() -> crate::threshold::Scope {
        crate::threshold::guest_from(
            &crate::threshold::Scope::owner(vec!["*".to_string()], crate::focus::FocusProfile::Default),
            &crate::focus::FocusProfile::DeepFocus,
        )
    }

    #[tokio::test]
    async fn guest_handle_local_refuses_owner_data_and_write_intents_but_allows_conversation_and_status() {
        // FINDING 3: handle_local is the structured-intent FAST PATH — it bypasses
        // the tool-loop + recall gates. For a GUEST it must DENY BY DEFAULT: refuse
        // memory.recall (reads owner facts), memory.store (WRITES owner memory),
        // app.control, web.open, and anything else not in the non-personal set.
        let db = TempDb::new("guest-handle-local");
        let mem = Memory::open(&db.0).unwrap();
        let reg = AgentRegistry::canonical();
        let agent = reg.orchestrator();
        let apps = std::sync::Arc::new(crate::apps::AppRegistry::discover(std::path::Path::new("/nonexistent")));

        // Seed an owner fact so we can prove memory.recall never speaks it.
        mem.upsert_fact("user.name", "Darwin").await.unwrap();
        mem.upsert_fact("agent.darwin.secret_note", "the owner's private note").await.unwrap();

        let _o = crate::threshold::ScopeOverride::guest(guest_scope_fixture());

        // memory.recall — REFUSED, and speaks NO owner fact.
        let out = super::handle_local("memory.recall", &serde_json::Value::Null, "what do you remember", &mem, &apps, agent).await;
        assert!(!out.llm_voice, "a refusal is spoken verbatim, not sent to the LLM");
        assert!(out.data.contains("guest mode"), "memory.recall is refused in guest mode: {}", out.data);
        assert!(!out.data.contains("Darwin"), "no owner fact leaks via memory.recall: {}", out.data);
        assert!(!out.data.contains("secret_note"), "no owner private fact leaks: {}", out.data);

        // memory.store — REFUSED, and performs NO write to the owner namespace.
        let out = super::handle_local("memory.store", &serde_json::Value::Null, "my card PIN is 1234", &mem, &apps, agent).await;
        assert!(out.data.contains("guest mode"), "memory.store is refused in guest mode: {}", out.data);
        let facts = mem.agent_scoped_facts(&agent.namespace, 100).await.unwrap();
        assert!(
            !facts.iter().any(|(_, v)| v.contains("1234")),
            "a guest memory.store must perform NO write to the owner's memory: {facts:?}"
        );

        // app.control + web.open — REFUSED.
        for intent in ["app.control", "app.launch", "web.open", "web.search", "file.op"] {
            let out = super::handle_local(intent, &serde_json::Value::Null, "do the thing", &mem, &apps, agent).await;
            assert!(out.data.contains("guest mode"), "{intent} must be refused for a guest: {}", out.data);
            assert!(!out.llm_voice, "{intent} refusal is spoken verbatim");
        }

        // conversation + system.query — ALLOWED (fall through / non-personal status).
        let out = super::handle_local("conversation", &serde_json::Value::Null, "hello there", &mem, &apps, agent).await;
        assert!(!out.data.contains("guest mode"), "conversation is allowed for a guest: {}", out.data);
        let out = super::handle_local("system.query", &serde_json::Value::Null, "how are you running", &mem, &apps, agent).await;
        assert!(!out.data.contains("guest mode"), "non-personal system status is allowed for a guest: {}", out.data);
    }

    #[tokio::test]
    async fn owner_handle_local_is_unchanged_no_guest_gate() {
        // OWNER RAIL: with NO guest scope, handle_local performs its normal work —
        // memory.store WRITES the note (byte-for-byte today's behavior).
        let db = TempDb::new("owner-handle-local");
        let mem = Memory::open(&db.0).unwrap();
        let reg = AgentRegistry::canonical();
        let agent = reg.orchestrator();
        let apps = std::sync::Arc::new(crate::apps::AppRegistry::discover(std::path::Path::new("/nonexistent")));

        let _o = crate::threshold::ScopeOverride::owner();
        let out = super::handle_local("memory.store", &serde_json::Value::Null, "buy oat milk", &mem, &apps, agent).await;
        assert!(!out.data.contains("guest mode"), "owner path never sees the guest refusal: {}", out.data);
        let facts = mem.agent_scoped_facts(&agent.namespace, 100).await.unwrap();
        assert!(facts.iter().any(|(_, v)| v == "buy oat milk"), "owner memory.store writes the note (unchanged)");
    }

    #[test]
    fn guest_denied_fast_path_catches_owner_data_and_consequential_classifiers() {
        // The route() fast-path gate: every owner-data / consequential specialized
        // classifier is refused for a guest, while plain conversation / translation /
        // status falls through (None).
        let cfg = Config::default();
        // Owner-DATA readers + owner CONTROLS / consequential actions -> Some(reason).
        for u in [
            "why do you think i like tea",     // user_model mirror (owner profile)
            "what was i doing an hour ago",    // aperture (activity timeline)
            "what did i copy earlier",         // pasteboard
            "save this research",              // notebook
            "go dark",                         // vault control
            "replay the macro morning",        // macro replay (consequential)
            "undo that",                       // journal undo (consequential)
            "always allow the gmail_send action", // policy control (pure classify, no write)
            "use the local model",             // model swap control
            "roll call",                       // agent roster (finding 2)
            "who's on the team",               // agent roster (finding 2)
            "list my agents",                  // agent query (finding 2)
            "what agents do you have",         // agent query (finding 2)
        ] {
            assert!(
                super::guest_denied_fast_path(u, &cfg).is_some(),
                "{u:?} must be refused for a guest (owner-data or consequential fast path)"
            );
        }
        // Guest-safe turns -> None (they flow to the guest-gated conversational path).
        for u in [
            "hello, how are you",
            "translate good morning into french",
            "what's the weather like",
            "tell me a joke",
        ] {
            assert!(
                super::guest_denied_fast_path(u, &cfg).is_none(),
                "{u:?} is guest-safe and must fall through"
            );
        }
    }

    #[test]
    fn guest_denied_fast_path_does_not_mutate_policy() {
        // REGRESSION: the fast-path gate uses the PURE `classify_policy_command`, NOT
        // `handle_user_policy_text` (which APPLIES the rule). Probing a guest's policy
        // utterance must classify it as denied WITHOUT writing any policy.
        let cfg = Config::default();
        assert!(
            super::guest_denied_fast_path("always allow the shell_run action", &cfg).is_some(),
            "a policy utterance is refused for a guest"
        );
        // The pure classifier used by the gate matches; the mutating handler was never
        // called (nothing to assert on global policy here beyond no panic / no write —
        // the point is the gate never routes through the applying path).
    }

    #[tokio::test]
    async fn guest_recall_and_history_feeds_are_empty_owner_feeds_are_full() {
        // FINDING 1 (feeds): a GUEST turn's auto RAG feed AND conversation history are
        // WITHHELD entirely — a bystander's prompt carries none of the owner's stored
        // facts or prior dialogue. The owner path is byte-for-byte today's.
        let db = TempDb::new("guest-feeds");
        let mem = Memory::open(&db.0).unwrap();
        mem.upsert_fact("user.name", "Darwin").await.unwrap();
        mem.upsert_fact("user.model.diet", "vegetarian").await.unwrap();
        mem.record_transcript(None, "what's my name", "conversation", "local", Some("You're Darwin."))
            .await
            .unwrap();

        // GUEST: both feeds are empty.
        {
            let _o = crate::threshold::ScopeOverride::guest(guest_scope_fixture());
            assert!(super::agent_facts(&mem, "agent.darwin").await.is_empty(), "guest RAG feed is empty");
            assert!(super::fetch_history(&mem).await.is_empty(), "guest history feed is empty");
        }
        // OWNER: both feeds carry the owner's data (unchanged).
        {
            let _o = crate::threshold::ScopeOverride::owner();
            let facts = super::agent_facts(&mem, "agent.darwin").await;
            assert!(facts.iter().any(|(k, _)| k == "user.name"), "owner RAG feed carries facts");
            assert!(!super::fetch_history(&mem).await.is_empty(), "owner history feed carries the exchange");
        }
    }

    #[tokio::test]
    async fn router_notebook_utterance_saves_then_revisits_the_real_run() {
        let db = TempDb::new("notebook");
        let mem = Memory::open(&db.0).unwrap();
        let ns = "agent.darwin";

        // A real SAGE run just completed (the live path records exactly this).
        let _g = crate::notebook::LastRunGuard::stage(Some(crate::notebook::LastResearchRun {
            topic: "the JWST".into(),
            report: synthetic_report(),
            synthesized: "On the JWST [1]".into(),
        }));

        // "save this research" -> classify -> dispatch (the route() composition).
        let intent = crate::notebook::classify_notebook_intent("save this research")
            .expect("an explicit save utterance classifies as a notebook intent");
        let out = crate::notebook::dispatch(&mem, ns, intent).await.unwrap();
        assert_eq!(out.verb, "saved", "the utterance persisted the real last run");

        // "show my research notebook on the JWST" -> revisit returns it, citing the
        // real grounded source ONLY (never the phantom).
        let intent = crate::notebook::classify_notebook_intent(
            "show my research notebook on the JWST",
        )
        .expect("a revisit utterance classifies");
        let out = crate::notebook::dispatch(&mem, ns, intent).await.unwrap();
        assert_eq!(out.verb, "revisit");
        assert!(out.reply.contains("https://a.test"), "the real source surfaces: {}", out.reply);
        assert!(!out.reply.contains("999"), "a fabricated citation must never surface: {}", out.reply);
    }

    #[tokio::test]
    async fn router_report_utterance_builds_from_the_saved_cited_runs() {
        // The route() composition for #40: an explicit "generate a report on X"
        // utterance classifies, and dispatch (with the op enabled) pulls the
        // agent-scoped saved cited runs on X and assembles a bounded report citing
        // ONLY their real grounded sources. Hermetic: temp Db + a synthetic run.
        let db = TempDb::new("report");
        let mem = Memory::open(&db.0).unwrap();
        let ns = "agent.darwin";
        // A real cited run is already saved on the topic (the notebook path enforces
        // that only the grounded source #1 persists — never the phantom 999).
        crate::notebook::save_run(&mem, ns, "the JWST", &synthetic_report(), "On the JWST [1]")
            .await
            .unwrap();

        let intent = crate::report::classify_report_intent("generate a report on the JWST")
            .expect("an explicit report utterance classifies");
        let on = crate::report::ReportConfig { enabled: true };
        let out = crate::report::dispatch(&mem, ns, intent, &on).await.unwrap();
        assert_eq!(out.verb, "report", "the report was built from the saved run");
        // The markdown cites ONLY the real grounded source, never the phantom. The
        // title is the normalized (lowercased) topic the intent carried.
        assert!(out.markdown.contains("# the jwst"), "title rendered: {}", out.markdown);
        assert!(out.markdown.contains("https://a.test"), "the real source surfaces: {}", out.markdown);
        assert!(!out.markdown.contains("999"), "a fabricated citation must never surface: {}", out.markdown);
        let report = out.report.unwrap();
        assert_eq!(report.all_citations.len(), 1, "only the grounded citation");
    }

    #[tokio::test]
    async fn router_report_when_disabled_declines_and_reads_nothing() {
        // With the op explicitly DISABLED (an operator override; the shipped default
        // is ON) dispatch declines honestly and reads nothing.
        let db = TempDb::new("report-off");
        let mem = Memory::open(&db.0).unwrap();
        let ns = "agent.darwin";
        let intent = crate::report::classify_report_intent("generate a report on anything")
            .expect("classifies");
        let off = crate::report::ReportConfig { enabled: false };
        assert!(!off.enabled, "explicitly disabled");
        let out = crate::report::dispatch(&mem, ns, intent, &off).await.unwrap();
        assert_eq!(out.verb, "report_off", "the disabled op declines");
        assert!(out.report.is_none(), "nothing was built");
        assert!(out.markdown.to_lowercase().contains("off"), "{}", out.markdown);
    }

    #[tokio::test]
    async fn router_report_unknown_topic_is_honest_empty() {
        let db = TempDb::new("report-empty");
        let mem = Memory::open(&db.0).unwrap();
        let ns = "agent.darwin";
        let intent = crate::report::classify_report_intent("write a report on a topic never researched")
            .expect("classifies");
        let on = crate::report::ReportConfig { enabled: true };
        let out = crate::report::dispatch(&mem, ns, intent, &on).await.unwrap();
        assert_eq!(out.verb, "report_empty", "no saved cited run -> honest empty");
        assert!(out.markdown.to_lowercase().contains("no sources to report on"), "{}", out.markdown);
    }

    #[test]
    fn classify_music_intent_extracts_the_prompt_on_creation_requests() {
        use super::classify_music_intent as c;
        // The flagship: "compose" anchors alone (no "song" noun). The verb +
        // leading article are stripped, leaving the cleaned prompt.
        assert_eq!(
            c("DARWIN, compose an 8-bit happy birthday").as_deref(),
            Some("8-bit happy birthday")
        );
        assert_eq!(c("compose an 8-bit happy birthday").as_deref(), Some("8-bit happy birthday"));
        // "about/of" tails unwrap to the descriptor.
        assert_eq!(c("compose a song about the rain").as_deref(), Some("the rain"));
        assert_eq!(c("write me a tune about my dog").as_deref(), Some("my dog"));
        assert_eq!(c("generate a beat of pure 90s house").as_deref(), Some("pure 90s house"));
        // Broad verbs WITH a music object noun match.
        assert!(c("make me a jingle for my coffee shop").is_some());
        assert_eq!(c("make me a jingle for my coffee shop").as_deref(), Some("my coffee shop"));
        assert!(c("produce a melody that goes da da dum").is_some());
        // "play me a <object>" is a creation ask.
        assert!(c("play me a track in the style of lo-fi").is_some());
        // A bare creation request with nothing described falls back to a non-empty
        // generic prompt (never an empty string the op can't compose).
        let bare = c("compose a song").expect("bare compose still matches");
        assert!(!bare.is_empty(), "bare compose must yield a non-empty prompt");
        // REGRESSION: a music-object noun that is only a PREFIX of a longer word must
        // NOT be stripped — "beatles" must survive (the bug stripped the bare "beat"
        // lead -> "les song").
        assert_eq!(c("compose a beatles song").as_deref(), Some("beatles song"));
    }

    #[test]
    fn classify_music_intent_rejects_non_music_speech() {
        use super::classify_music_intent as c;
        // No creation verb -> not music (the critical anti-over-trigger case).
        assert!(c("play some jazz").is_none());
        assert!(c("play the latest taylor swift").is_none());
        assert!(c("turn up the music").is_none());
        assert!(c("what's the time").is_none());
        assert!(c("what's the cpu usage").is_none());
        assert!(c("how's the weather today").is_none());
        // Broad creation verbs WITHOUT a music object are NOT music.
        assert!(c("make me a sandwich").is_none());
        assert!(c("write me an email to my boss").is_none());
        assert!(c("generate a report on the JWST").is_none());
        assert!(c("produce the quarterly numbers").is_none());
        // "play me ..." without a music object noun is not music.
        assert!(c("play me the news").is_none());
        // Casual mention of a song without a creation verb is not music.
        assert!(c("i love that song").is_none());
        assert!(c("what song is this").is_none());
        // Empty / whitespace.
        assert!(c("").is_none());
        assert!(c("   ").is_none());
    }

    #[test]
    fn router_chart_intent_emits_the_exact_snapshot_points() {
        // The route() composition for #41: a "chart this" utterance classifies, the
        // latest REAL snapshot becomes a ChartSpec of the EXACT cpu/mem values, and
        // emit_chart publishes the chart.data envelope. Hermetic: an injected
        // snapshot-shaped spec + the test telemetry seam (no WS client, no network).
        assert!(crate::chart::classify_chart_intent("chart this").is_some());
        let snap = crate::telemetry::SystemSnapshot {
            cpu_percent: 25.0,
            mem_used_bytes: 2_000_000_000,
            mem_total_bytes: 8_000_000_000,
            disk_free_bytes: None,
            disk_total_bytes: None,
            uptime_secs: 10,
        };
        let spec = crate::chart::chart_from_snapshot(Some(snap));
        let mut rx = crate::telemetry::subscribe_for_test();
        crate::chart::emit_chart(&spec);
        // The telemetry hub is a SHARED broadcast bus, so under parallel test load
        // OTHER tests' frames interleave into this receiver. Drain and pick OUR
        // `chart.data` frame (the only chart.data emitter in a test run) instead of
        // assuming it arrives first — the previous single try_recv() flaked when a
        // sibling test's frame was buffered ahead of ours.
        let mut env: Option<serde_json::Value> = None;
        for _ in 0..512 {
            match rx.try_recv() {
                Ok(raw) => {
                    let e: serde_json::Value = serde_json::from_str(&raw).unwrap();
                    if e["event"] == "chart.data" {
                        env = Some(e);
                        break;
                    }
                }
                // A lagged receiver dropped some frames under load — keep draining.
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                // Empty or Closed: our synchronous emit is already buffered, so if we
                // reach here without finding it that IS a real failure.
                Err(_) => break,
            }
        }
        let env = env.expect("a chart.data envelope was published");
        assert_eq!(env["event"], "chart.data");
        let pts = env["data"]["series"][0]["points"].as_array().unwrap();
        // EXACTLY the two real metrics: cpu 25 at x=0, mem 25% at x=1.
        assert_eq!(pts.len(), 2, "exactly the snapshot metrics: {pts:?}");
        assert_eq!(pts[0], serde_json::json!([0.0, 25.0]));
        assert_eq!(pts[1], serde_json::json!([1.0, 25.0]));
    }

    #[tokio::test]
    async fn router_lifelog_utterance_builds_the_real_digest() {
        let db = TempDb::new("lifelog");
        let mem = Memory::open(&db.0).unwrap();
        let ns = "agent.darwin";
        crate::episodic::record_episode(
            &Config::default(),
            &mem,
            ns,
            "worked on the rocket engine design",
            "ok",
            "code",
            false,
            crate::episodic::VoiceGate { enabled: false, enrolled: false, owner_verified: false },
        )
        .await
        .unwrap();

        // "what did I do this week" -> classify -> dispatch (the route() composition).
        let intent = crate::lifelog::classify_lifelog_intent("what did I do this week")
            .expect("a life-log utterance classifies");
        let reply = crate::lifelog::dispatch(&mem, ns, intent).await;
        assert!(reply.contains("1 recorded turn"), "names the real count: {reply}");
        assert!(reply.contains("rocket"), "names a real theme from the episode: {reply}");

        // An EMPTY store yields an HONEST empty digest (never a fabricated event) —
        // a fresh Db with nothing logged.
        let empty_db = TempDb::new("lifelog-empty");
        let empty_mem = Memory::open(&empty_db.0).unwrap();
        let intent = crate::lifelog::classify_lifelog_intent("what did I do today").unwrap();
        let reply = crate::lifelog::dispatch(&empty_mem, ns, intent).await;
        assert!(reply.to_lowercase().contains("nothing logged"), "honest empty: {reply}");
    }
}

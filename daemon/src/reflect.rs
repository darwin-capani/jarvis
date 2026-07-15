//! Self-learning reflection: a background task that periodically asks the
//! inference server to consolidate DARWIN's memory — merging duplicate
//! facts, preferring the newest phrasing on conflicts, and deleting facts
//! the user later contradicted.
//!
//! Hard rule: this task must NEVER panic or wedge the daemon. Every step is
//! warn-and-continue; a failed round simply retries on the next 6h check
//! (the meta.last_reflection stamp only advances on success).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use tracing::{info, warn};

use crate::inference::InferenceClient;
use crate::memory::Memory;
use crate::telemetry;

/// Grace period after daemon start before the first reflection check —
/// startup belongs to preloading and the first exchanges, not housekeeping.
const STARTUP_DELAY: Duration = Duration::from_secs(90);
/// How often the task re-checks whether a reflection is due.
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 3600);
/// A reflection runs when the last one is older than this (or never ran).
const STALENESS_SECS: u64 = 20 * 3600;
/// Stored as a fact (unix seconds) but excluded from every prompt feed —
/// all_user_facts filters keys starting "meta.".
const META_LAST_REFLECTION: &str = "meta.last_reflection";
/// Most recent exchanges handed to the consolidate op (wire cap: 40).
const TRANSCRIPT_WINDOW: usize = 40;
/// Stored facts handed to the consolidate op.
const FACTS_WINDOW: usize = 200;
/// Recent EPISODES (the orchestrator's shared tier) folded into the USER MODEL
/// consolidation each reflection cycle. Bounded: the profile compounds from the
/// recent past, not the whole store. The shared tier ("agent.darwin") is read
/// because the USER MODEL is the USER's, shared by every agent — and reading the
/// shared scope keeps a specialist's private episodes out of it (isolation).
const USER_MODEL_EPISODE_WINDOW: usize = 200;

/// Spawned once at daemon startup; loops forever.
pub async fn reflection_task(sock: PathBuf, memory: Arc<Memory>) {
    tokio::time::sleep(STARTUP_DELAY).await;
    loop {
        run_once(&sock, &memory).await;
        tokio::time::sleep(CHECK_INTERVAL).await;
    }
}

/// Whether a reflection is due, given the stored meta.last_reflection value
/// (unix seconds as text; absent or unparseable counts as due).
fn is_due(last_reflection: Option<&str>, now_secs: u64) -> bool {
    match last_reflection.and_then(|v| v.trim().parse::<u64>().ok()) {
        Some(last) => now_secs.saturating_sub(last) > STALENESS_SECS,
        None => true,
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn run_once(sock: &Path, memory: &Memory) {
    let now = now_secs();
    let last = match memory.get_fact(META_LAST_REFLECTION).await {
        Ok(last) => last,
        Err(e) => {
            warn!(error = %e, "reflection: cannot read last-reflection stamp; skipping this round");
            return;
        }
    };
    if !is_due(last.as_deref(), now) {
        return;
    }

    let transcripts = match memory.recent_exchanges(TRANSCRIPT_WINDOW).await {
        Ok(t) => t,
        Err(e) => {
            warn!(error = %e, "reflection: cannot load transcripts; skipping this round");
            return;
        }
    };
    // User facts only: the meta stamp itself must never reach the prompt.
    let facts = match memory.all_user_facts(FACTS_WINDOW).await {
        Ok(f) => f,
        Err(e) => {
            warn!(error = %e, "reflection: cannot load facts; skipping this round");
            return;
        }
    };
    // USER MODEL consolidation (Pepper/reflection path): fold the recent EPISODES
    // + the stored FACTS into the structured, COMPOUNDING user profile
    // (preferences/patterns/recurring-topics/style, each provenance-tagged +
    // observed-counted). This is DETERMINISTIC + HERMETIC — it needs NO inference
    // server, so the profile compounds every cycle whether or not the cloud/local
    // model is reachable, and it can NEVER fabricate a preference (a signal must
    // clear the observation threshold; contradictory/empty inputs add nothing). It
    // runs BEFORE the network consolidate so a down inference server never starves
    // it. Warn-and-continue: a failed pass never wedges the reflection task.
    consolidate_user_model(memory, &facts).await;

    if transcripts.is_empty() && facts.is_empty() {
        // Nothing to consolidate; stamp so the next check is cheap.
        stamp(memory, now).await;
        return;
    }

    // Own client: the main event loop owns the other one mutably, and a slow
    // consolidation must never contend with a live reply.
    let mut infer = InferenceClient::new(sock.to_path_buf());
    match infer.consolidate(&transcripts, &facts).await {
        Ok(outcome) => {
            let mut upserts = 0usize;
            let mut deletes = 0usize;
            for (key, value) in &outcome.upserts {
                // is_reserved_key + upsert_user_fact: the centralized,
                // case-insensitive reserved-prefix guard every model-driven
                // write path shares (audit fix).
                if crate::memory::is_reserved_key(key) {
                    warn!(key, "reflection: model tried to write a meta key; ignored");
                    continue;
                }
                match memory.upsert_user_fact(key, value).await {
                    Ok(()) => upserts += 1,
                    Err(e) => warn!(error = %e, key, "reflection: upsert failed"),
                }
            }
            for key in &outcome.deletes {
                if crate::memory::is_reserved_key(key) {
                    warn!(key, "reflection: model tried to delete a meta key; ignored");
                    continue;
                }
                match memory.delete_fact(key).await {
                    Ok(true) => deletes += 1,
                    Ok(false) => {} // already gone; nothing to count
                    Err(e) => warn!(error = %e, key, "reflection: delete failed"),
                }
            }
            info!(upserts, deletes, "memory consolidated");
            telemetry::emit(
                "system",
                "memory.consolidated",
                json!({"upserts": upserts, "deletes": deletes}),
            );
            stamp(memory, now).await;
        }
        Err(e) => {
            // No stamp: the next 6h check retries while the 20h staleness
            // still holds. The telemetry event surfaces a stuck reflection
            // clock on the HUD diagnostics instead of silent 6h retries
            // (audit fix).
            warn!(error = %e, "reflection: consolidate failed; will retry on the next check");
            telemetry::emit(
                "system",
                "memory.consolidation_failed",
                json!({"error": e.to_string()}),
            );
        }
    }
}

async fn stamp(memory: &Memory, now: u64) {
    if let Err(e) = memory.upsert_fact(META_LAST_REFLECTION, &now.to_string()).await {
        warn!(error = %e, "reflection: failed to stamp last-reflection time");
    }
}

/// Run the DETERMINISTIC user-model consolidation: read the recent shared-tier
/// EPISODES, fold them + the stored FACTS into the compounding user profile via
/// [`crate::user_model::consolidate`], and emit a telemetry count. Hermetic (no
/// network) and warn-and-continue (a busy DB or read error never wedges the
/// reflection task). NEVER fabricates: consolidate only writes entries whose
/// signal cleared the observation threshold.
async fn consolidate_user_model(memory: &Memory, facts: &[(String, String)]) {
    // Read the SHARED-tier episodes only ("agent.darwin"): the user model is the
    // USER's, shared by every agent, and the shared scope keeps a specialist's
    // private episodes out of the profile (isolation on the way IN).
    let episodes = match memory
        .episodes_recent("agent.darwin", USER_MODEL_EPISODE_WINDOW)
        .await
    {
        Ok(eps) => eps,
        Err(e) => {
            warn!(error = %e, "reflection: cannot load episodes for the user model; skipping its consolidation this round");
            return;
        }
    };
    // consolidate() CONSULTS the MIRROR suppression tombstones internally, so a
    // belief the user contested is NEVER re-derived here — the reflection pass can
    // strengthen the profile every cycle without silently resurrecting a dropped
    // belief.
    match crate::user_model::consolidate(memory, &episodes, facts).await {
        Ok(written) => {
            info!(written, "user model consolidated");
            telemetry::emit(
                "system",
                "user_model.consolidated",
                json!({ "entries_written": written }),
            );
            // Refresh the HUD's MIRROR panel with the post-consolidation belief list
            // (the snapshot frame is sticky-retained for replay-on-connect).
            crate::user_model::emit_belief_frame(memory, "snapshot", "", false).await;
        }
        Err(e) => {
            warn!(error = %e, "reflection: user-model consolidation failed; will retry next cycle");
            telemetry::emit(
                "system",
                "user_model.consolidation_failed",
                json!({ "error": e.to_string() }),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{is_due, STALENESS_SECS};

    #[test]
    fn reflection_is_due_when_unstamped_stale_or_garbled() {
        let now = 1_760_000_000u64;
        assert!(is_due(None, now), "never ran -> due");
        assert!(is_due(Some("not-a-number"), now), "garbled stamp -> due");
        assert!(
            is_due(Some(&(now - STALENESS_SECS - 1).to_string()), now),
            "older than 20h -> due"
        );
        assert!(
            !is_due(Some(&(now - STALENESS_SECS).to_string()), now),
            "exactly 20h -> not yet due"
        );
        assert!(
            !is_due(Some(&(now - 3600).to_string()), now),
            "1h ago -> not due"
        );
        // A stamp from the future (clock skew) must not underflow.
        assert!(!is_due(Some(&(now + 9999).to_string()), now));
    }
}

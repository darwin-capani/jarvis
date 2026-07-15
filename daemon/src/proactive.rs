//! Proactive learning: the first-contact brief.
//!
//! The daemon stamps meta.last_interaction (unix seconds, an internal
//! bookkeeping fact filtered from every prompt feed) after each completed
//! reply. When a new utterance arrives more than [proactive].idle_gap_hours
//! after that stamp, the reply's converse data gains a FIRST-CONTACT BRIEF:
//! the local time-of-day word, a one-line system status from the cached
//! telemetry snapshot, any user.habit.* facts whose value mentions the
//! current time-of-day word, a pending self-heal proposal when
//! meta.heal_pending is set, and how many facts were learned while the user
//! was away. The persona phrases all of it — the brief itself is
//! verified-only data, assembled exclusively from stored state, never from
//! model output.

use chrono::Timelike;
use serde_json::json;
use tracing::warn;

use crate::config::Config;
use crate::memory::Memory;
use crate::telemetry::{self, SystemSnapshot};

/// Unix seconds of the last completed reply; written by record_interaction,
/// read by first_contact_brief. "meta." prefix keeps it out of prompts.
const META_LAST_INTERACTION: &str = "meta.last_interaction";
/// Set by the self-heal pipeline when a validated proposal awaits review
/// (cleared by scripts/apply_heal.sh); the brief mentions it so DARWIN
/// TELLS the user instead of silently parking the patch.
const META_HEAL_PENDING: &str = "meta.heal_pending";
/// Set by the Self-Forge pipeline when a validated app proposal awaits review
/// (cleared by scripts/apply_forge.sh); mirrors META_HEAL_PENDING so the brief
/// TELLS the user a forged micro-app is staged instead of silently parking it.
const META_FORGE_PENDING: &str = "meta.forge_pending";
/// At most this many matching habit lines ride in one brief — the persona
/// gets a greeting hint, not a memory dump.
const BRIEF_HABIT_LIMIT: usize = 3;

/// The local hour mapped to the contract's three time-of-day words.
pub fn time_of_day_word(hour: u32) -> &'static str {
    match hour {
        5..=11 => "morning",
        12..=16 => "afternoon",
        _ => "evening",
    }
}

/// meta.last_interaction parser: unix seconds stored as text. Absent or
/// garbled (None) means no gap can be computed — never a brief.
pub fn parse_unix_secs(value: Option<&str>) -> Option<u64> {
    value?.trim().parse().ok()
}

/// Gap math: hours since `last_secs` when the gap STRICTLY exceeds
/// `idle_gap_hours`; None otherwise. A stamp from the future (clock skew)
/// saturates to a zero gap and yields None.
pub fn exceeded_gap_hours(last_secs: u64, now_secs: u64, idle_gap_hours: u64) -> Option<f64> {
    let gap = now_secs.saturating_sub(last_secs);
    (gap > idle_gap_hours.saturating_mul(3600)).then_some(gap as f64 / 3600.0)
}

/// One-line system status from the cached snapshot — the same vitals the
/// system.query handler reports, compressed for the brief.
pub fn status_line(s: &SystemSnapshot) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let disk = s
        .disk_free_bytes
        .map(|b| format!(", {:.0} gigabytes of disk free", b as f64 / GIB))
        .unwrap_or_default();
    format!(
        "CPU at {:.0} percent, memory {:.1} of {:.0} gigabytes used{disk}",
        s.cpu_percent,
        s.mem_used_bytes as f64 / GIB,
        s.mem_total_bytes as f64 / GIB,
    )
}

/// Everything the brief is assembled from. All fields come from stored or
/// measured state (facts table, telemetry snapshot) — verified-only.
pub struct BriefInputs<'a> {
    pub gap_hours: f64,
    /// time_of_day_word(local hour).
    pub time_of_day: &'a str,
    /// status_line(cached snapshot); None when no snapshot exists yet.
    pub status: Option<String>,
    /// ALL stored user.habit.* facts; assemble_brief filters to the ones
    /// whose value mentions `time_of_day`.
    pub habits: &'a [(String, String)],
    /// meta.heal_pending value (the proposal timestamp), when set.
    pub heal_pending: Option<&'a str>,
    /// meta.forge_pending value (the forged-app proposal timestamp), when set.
    pub forge_pending: Option<&'a str>,
    /// Non-meta facts touched since meta.last_interaction.
    pub facts_learned: u64,
}

/// Pure assembly: the brief text handed to converse as data, plus how many
/// habit facts matched the current time of day (for telemetry). Habit
/// matching is a case-insensitive substring test on the fact VALUE — the
/// consolidation prompt puts time-of-day words there ("... most mornings"),
/// and "morning" naturally matches "mornings".
pub fn assemble_brief(inputs: &BriefInputs) -> (String, usize) {
    let matched: Vec<&(String, String)> = inputs
        .habits
        .iter()
        .filter(|(_, value)| value.to_lowercase().contains(inputs.time_of_day))
        .collect();

    let mut lines = vec![format!(
        "First contact in {:.1} hours; it is {} for the user.",
        inputs.gap_hours, inputs.time_of_day
    )];
    if let Some(status) = &inputs.status {
        lines.push(format!("System status: {status}."));
    }
    for (_, value) in matched.iter().take(BRIEF_HABIT_LIMIT) {
        lines.push(format!("Known {} habit of the user: {value}.", inputs.time_of_day));
    }
    if let Some(ts) = inputs.heal_pending {
        lines.push(format!(
            "A validated self-repair proposal ({ts}) is staged and awaiting the user's review; mention it."
        ));
    }
    if let Some(ts) = inputs.forge_pending {
        lines.push(format!(
            "A forged micro-app ({ts}) is staged and awaiting the user's review; mention it."
        ));
    }
    match inputs.facts_learned {
        0 => {}
        1 => lines.push("1 new fact about the user was learned since the last conversation.".to_string()),
        n => lines.push(format!(
            "{n} new facts about the user were learned since the last conversation."
        )),
    }
    (lines.join(" "), matched.len())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Build the first-contact brief for a fresh utterance, or None when the
/// feature is disabled, no interaction was ever stamped, or the away gap has
/// not been exceeded. Emits proactive.brief{gap_hours, habits_matched} when
/// a brief is produced. Every memory read is warn-and-continue: a brief must
/// never delay or break the reply it decorates.
pub async fn first_contact_brief(cfg: &Config, memory: &Memory) -> Option<String> {
    if !cfg.proactive.enabled {
        return None;
    }
    let last = match memory.get_fact(META_LAST_INTERACTION).await {
        Ok(last) => last,
        Err(e) => {
            warn!(error = %e, "proactive: cannot read last-interaction stamp; skipping brief");
            return None;
        }
    };
    let last_secs = parse_unix_secs(last.as_deref())?;
    let now = now_secs();
    let gap_hours = exceeded_gap_hours(last_secs, now, cfg.proactive.idle_gap_hours)?;

    let time_of_day = time_of_day_word(chrono::Local::now().hour());
    let status = telemetry::latest_snapshot().map(|s| status_line(&s));
    let habits = memory.recall_facts("user.habit.").await.unwrap_or_else(|e| {
        warn!(error = %e, "proactive: cannot read habit facts; brief continues without them");
        Vec::new()
    });
    let heal_pending = match memory.get_fact(META_HEAL_PENDING).await {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "proactive: cannot read heal-pending stamp; brief continues without it");
            None
        }
    };
    let forge_pending = match memory.get_fact(META_FORGE_PENDING).await {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "proactive: cannot read forge-pending stamp; brief continues without it");
            None
        }
    };
    let facts_learned = match chrono::DateTime::from_timestamp(last_secs as i64, 0) {
        Some(ts) => memory
            .facts_learned_since(&ts.to_rfc3339())
            .await
            .unwrap_or_else(|e| {
                warn!(error = %e, "proactive: cannot count learned facts; brief reports none");
                0
            }),
        None => 0,
    };

    let (brief, habits_matched) = assemble_brief(&BriefInputs {
        gap_hours,
        time_of_day,
        status,
        habits: &habits,
        heal_pending: heal_pending.as_deref(),
        forge_pending: forge_pending.as_deref(),
        facts_learned,
    });
    telemetry::emit(
        "system",
        "proactive.brief",
        json!({
            "gap_hours": (gap_hours * 10.0).round() / 10.0,
            "habits_matched": habits_matched,
        }),
    );
    Some(brief)
}

/// Stamp meta.last_interaction = now (unix seconds) after a completed reply.
/// Trusted internal write — model output never reaches this path.
pub async fn record_interaction(memory: &Memory) {
    if let Err(e) = memory
        .upsert_fact(META_LAST_INTERACTION, &now_secs().to_string())
        .await
    {
        warn!(error = %e, "proactive: failed to stamp last interaction");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        assemble_brief, exceeded_gap_hours, parse_unix_secs, status_line, time_of_day_word,
        BriefInputs,
    };
    use crate::telemetry::SystemSnapshot;

    #[test]
    fn time_of_day_words_cover_the_clock() {
        assert_eq!(time_of_day_word(0), "evening");
        assert_eq!(time_of_day_word(4), "evening");
        assert_eq!(time_of_day_word(5), "morning");
        assert_eq!(time_of_day_word(11), "morning");
        assert_eq!(time_of_day_word(12), "afternoon");
        assert_eq!(time_of_day_word(16), "afternoon");
        assert_eq!(time_of_day_word(17), "evening");
        assert_eq!(time_of_day_word(23), "evening");
    }

    /// Contract gap math against meta.last_interaction (4h default gate).
    #[test]
    fn gap_math_triggers_only_past_the_idle_gap() {
        let now = 1_760_000_000u64;
        let four_h = 4 * 3600;
        // Just inside the gap: no brief.
        assert_eq!(exceeded_gap_hours(now - four_h, now, 4), None, "exactly 4h is not exceeded");
        assert_eq!(exceeded_gap_hours(now - 3600, now, 4), None);
        // Just past it: brief, with the true gap in hours.
        let gap = exceeded_gap_hours(now - four_h - 1, now, 4).expect("4h+1s exceeds");
        assert!((gap - 4.0003).abs() < 0.01, "{gap}");
        let gap = exceeded_gap_hours(now - 9 * 3600, now, 4).unwrap();
        assert!((gap - 9.0).abs() < f64::EPSILON);
        // Clock skew (stamp in the future) saturates: never a brief.
        assert_eq!(exceeded_gap_hours(now + 999, now, 4), None);
        // idle_gap_hours = 0 still requires a STRICTLY positive gap.
        assert_eq!(exceeded_gap_hours(now, now, 0), None);
        assert!(exceeded_gap_hours(now - 1, now, 0).is_some());
    }

    #[test]
    fn last_interaction_stamp_parses_or_yields_no_brief() {
        assert_eq!(parse_unix_secs(Some("1760000000")), Some(1_760_000_000));
        assert_eq!(parse_unix_secs(Some(" 1760000000 ")), Some(1_760_000_000));
        assert_eq!(parse_unix_secs(Some("not-a-number")), None);
        assert_eq!(parse_unix_secs(Some("")), None);
        assert_eq!(parse_unix_secs(None), None);
    }

    fn snapshot_fixture() -> SystemSnapshot {
        SystemSnapshot {
            cpu_percent: 7.4,
            mem_used_bytes: (11.5 * 1024.0 * 1024.0 * 1024.0) as u64,
            mem_total_bytes: 16 * 1024 * 1024 * 1024,
            disk_free_bytes: Some(212 * 1024 * 1024 * 1024),
            disk_total_bytes: Some(500 * 1024 * 1024 * 1024),
            uptime_secs: 86_400 * 3,
        }
    }

    #[test]
    fn status_line_reads_from_a_fixture_snapshot() {
        let line = status_line(&snapshot_fixture());
        assert_eq!(
            line,
            "CPU at 7 percent, memory 11.5 of 16 gigabytes used, 212 gigabytes of disk free"
        );
        // No disk visible: the clause disappears, nothing dangles.
        let mut s = snapshot_fixture();
        s.disk_free_bytes = None;
        assert_eq!(status_line(&s), "CPU at 7 percent, memory 11.5 of 16 gigabytes used");
    }

    /// Contract test: brief assembly from a fixture snapshot + facts is a
    /// pure function of its inputs.
    #[test]
    fn brief_assembly_from_fixture_snapshot_and_facts() {
        let habits = vec![
            (
                "user.habit.morning_status_check".to_string(),
                "asks for a system status most mornings".to_string(),
            ),
            (
                "user.habit.evening_jazz".to_string(),
                "often asks for jazz in the evening".to_string(),
            ),
            (
                "user.habit.coffee".to_string(),
                "Wants espresso first thing in the MORNING".to_string(), // case-insensitive match
            ),
        ];
        let (brief, matched) = assemble_brief(&BriefInputs {
            gap_hours: 9.25,
            time_of_day: "morning",
            status: Some(status_line(&snapshot_fixture())),
            habits: &habits,
            heal_pending: Some("1760001234"),
            forge_pending: None,
            facts_learned: 2,
        });
        assert_eq!(matched, 2, "evening habit must not match a morning brief");
        assert!(brief.starts_with("First contact in 9.2 hours; it is morning for the user."));
        assert!(brief.contains("CPU at 7 percent, memory 11.5 of 16 gigabytes used"));
        assert!(brief.contains("asks for a system status most mornings"));
        assert!(brief.contains("Wants espresso first thing in the MORNING"));
        assert!(!brief.contains("jazz"), "unmatched habit leaked into the brief");
        assert!(brief.contains("self-repair proposal (1760001234)"));
        assert!(brief.contains("2 new facts about the user were learned"));
        // forge_pending was None: no forged-app line leaked in.
        assert!(!brief.contains("forged micro-app"));
    }

    /// A staged forge proposal (meta.forge_pending) is announced exactly like a
    /// staged heal proposal — and the two coexist when both are pending.
    #[test]
    fn brief_announces_a_pending_forge() {
        // Forge alone.
        let (brief, _) = assemble_brief(&BriefInputs {
            gap_hours: 7.0,
            time_of_day: "evening",
            status: None,
            habits: &[],
            heal_pending: None,
            forge_pending: Some("1760002345"),
            facts_learned: 0,
        });
        assert!(brief.contains("A forged micro-app (1760002345) is staged and awaiting the user's review"));
        assert!(!brief.contains("self-repair"), "no heal line when heal_pending is None");

        // Heal AND forge pending together: both lines appear, heal before forge.
        let (both, _) = assemble_brief(&BriefInputs {
            gap_hours: 7.0,
            time_of_day: "evening",
            status: None,
            habits: &[],
            heal_pending: Some("1760001111"),
            forge_pending: Some("1760002222"),
            facts_learned: 0,
        });
        let heal_at = both.find("self-repair proposal (1760001111)").expect("heal line present");
        let forge_at = both.find("forged micro-app (1760002222)").expect("forge line present");
        assert!(heal_at < forge_at, "heal clause precedes forge clause");
    }

    #[test]
    fn brief_omits_what_it_does_not_have() {
        let (brief, matched) = assemble_brief(&BriefInputs {
            gap_hours: 5.0,
            time_of_day: "evening",
            status: None,
            habits: &[],
            heal_pending: None,
            forge_pending: None,
            facts_learned: 0,
        });
        assert_eq!(matched, 0);
        assert_eq!(brief, "First contact in 5.0 hours; it is evening for the user.");

        // Singular phrasing for exactly one learned fact.
        let (brief, _) = assemble_brief(&BriefInputs {
            gap_hours: 5.0,
            time_of_day: "evening",
            status: None,
            habits: &[],
            heal_pending: None,
            forge_pending: None,
            facts_learned: 1,
        });
        assert!(brief.contains("1 new fact about the user was learned"));
    }

    #[test]
    fn brief_caps_habit_lines() {
        let habits: Vec<(String, String)> = (0..5)
            .map(|i| {
                (
                    format!("user.habit.h{i}"),
                    format!("habit number {i} happening every morning"),
                )
            })
            .collect();
        let (brief, matched) = assemble_brief(&BriefInputs {
            gap_hours: 6.0,
            time_of_day: "morning",
            status: None,
            habits: &habits,
            heal_pending: None,
            forge_pending: None,
            facts_learned: 0,
        });
        assert_eq!(matched, 5, "telemetry reports every match");
        // ...but only the first 3 ride in the brief text.
        assert_eq!(brief.matches("Known morning habit").count(), 3);
    }
}

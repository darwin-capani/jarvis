//! FUSED PRESENCE / ATTENTION STATE (`presence.state` telemetry).
//!
//! EDITH's proactivity has, until now, gated on a single boolean: was there any
//! input within the last 10 minutes (`recently_present`). That cannot tell "just
//! sat down" from "deep in flow, do not interrupt". This fuses the available
//! attention signals into one honest state — **Away / Present / Focused** — that
//! the anticipation loop uses to QUIET spoken proactivity during flow (never to
//! enable anything: like the focus profile, this is permission-neutral — it can
//! only make DARWIN quieter, never louder or more capable).
//!
//! PURE + total: [`fuse`] takes an explicit [`PresenceInputs`] snapshot and
//! thresholds and returns a [`Presence`] — no globals, no I/O — so the fusion
//! logic is fully unit-tested. Inputs are `Option`: an unavailable signal
//! (vision not running, VAD not wired) is `None` and the fuser degrades honestly
//! rather than inventing a reading.

use serde::Serialize;

/// The fused attention state. Serializes snake_case for the HUD's
/// `parsePresenceState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    /// Not at the machine (no recent input and no person seen). EDITH stays
    /// silent — it never surfaces to an empty room.
    Away,
    /// At the machine and available — spoken proactivity is fine.
    Present,
    /// At the machine but in flow (working silently) or in a do-not-disturb focus
    /// profile — spoken proactivity is SUPPRESSED (a silent surface still allowed).
    Focused,
}

impl Presence {
    /// Whether this state suppresses SPOKEN proactivity. Permission-neutral: it
    /// only ever quiets EDITH (Focused: don't interrupt flow; Away: empty room).
    /// A silent HUD surface is unaffected — this never blocks a user-driven reply.
    pub fn suppresses_spoken_proactivity(self) -> bool {
        matches!(self, Presence::Away | Presence::Focused)
    }
}

/// The attention signals fused each tick. Each is `Option` so an unsourced
/// signal degrades honestly (never fabricated).
#[derive(Debug, Clone, Copy, Default)]
pub struct PresenceInputs {
    /// Seconds since the last user input (from `meta.last_interaction`).
    pub secs_since_input: Option<u64>,
    /// Seconds since the last detected speech (VAD). `None` when not wired.
    pub secs_since_speech: Option<u64>,
    /// Whether the vision app reports a person in frame. `None` when vision is
    /// not running / not granted.
    pub vision_person: Option<bool>,
    /// The active focus profile requests do-not-disturb (quiet) — a
    /// permission-neutral operator preference.
    pub focus_dnd: bool,
}

/// Tunable windows for the fusion. Defaults mirror the shipped 10-minute presence
/// window and pick conservative "in flow" / "recently conversational" windows.
#[derive(Debug, Clone, Copy)]
pub struct PresenceThresholds {
    /// Input within this => at the machine (default 600s, the shipped window).
    pub present_window: u64,
    /// Input within this AND no recent speech => actively working (default 120s).
    pub focus_idle_max: u64,
    /// Speech within this => recently conversational, so not "silent flow"
    /// (default 90s).
    pub speech_recent: u64,
}

impl Default for PresenceThresholds {
    fn default() -> Self {
        Self { present_window: 600, focus_idle_max: 120, speech_recent: 90 }
    }
}

/// Fuse the attention signals into a [`Presence`]. PURE.
///
/// - **Away**: no recent input AND vision does not report a person. (A person in
///   frame overrides idle input — you're there even if not typing.)
/// - **Focused**: at the machine and either the focus profile asks for quiet, OR
///   you're actively working *silently* (recent input, no recent speech).
/// - **Present**: at the machine, but recently conversational or just arrived.
pub fn fuse(i: &PresenceInputs, t: &PresenceThresholds) -> Presence {
    let recent_input = i.secs_since_input.is_some_and(|s| s <= t.present_window);
    let person_seen = i.vision_person == Some(true);
    if !recent_input && !person_seen {
        return Presence::Away;
    }
    let recent_speech = i.secs_since_speech.is_some_and(|s| s <= t.speech_recent);
    let actively_working = i.secs_since_input.is_some_and(|s| s <= t.focus_idle_max);
    if i.focus_dnd || (actively_working && !recent_speech) {
        Presence::Focused
    } else {
        Presence::Present
    }
}

/// Build the `presence.state` telemetry payload from a fused state + its inputs.
/// PURE + SECRET-FREE (coarse buckets + booleans, never a raw timestamp).
pub fn state_payload(state: Presence, i: &PresenceInputs) -> serde_json::Value {
    serde_json::json!({
        "state": state,
        "at_machine": state != Presence::Away,
        "focus_dnd": i.focus_dnd,
        // Whether each fused signal was actually available this tick (honesty:
        // the HUD can show that vision/VAD are not feeding the fusion yet).
        "signals": {
            "input": i.secs_since_input.is_some(),
            "speech": i.secs_since_speech.is_some(),
            "vision": i.vision_person.is_some(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> PresenceThresholds {
        PresenceThresholds::default()
    }

    #[test]
    fn away_when_no_recent_input_and_no_person() {
        let i = PresenceInputs { secs_since_input: Some(3600), ..Default::default() };
        assert_eq!(fuse(&i, &t()), Presence::Away);
        // No input signal at all is also away.
        assert_eq!(fuse(&PresenceInputs::default(), &t()), Presence::Away);
    }

    #[test]
    fn a_person_in_frame_overrides_idle_input() {
        // Idle input (an hour) but vision sees a person => at the machine (Present,
        // since not actively typing and no DND).
        let i = PresenceInputs {
            secs_since_input: Some(3600),
            vision_person: Some(true),
            ..Default::default()
        };
        assert_eq!(fuse(&i, &t()), Presence::Present);
    }

    #[test]
    fn focused_when_working_silently() {
        // Recent typing, no recent speech => in flow.
        let i = PresenceInputs {
            secs_since_input: Some(10),
            secs_since_speech: Some(500),
            ..Default::default()
        };
        assert_eq!(fuse(&i, &t()), Presence::Focused);
        assert!(fuse(&i, &t()).suppresses_spoken_proactivity());
    }

    #[test]
    fn present_when_recently_conversational() {
        // Recent typing AND recent speech => conversational, not silent flow.
        let i = PresenceInputs {
            secs_since_input: Some(10),
            secs_since_speech: Some(5),
            ..Default::default()
        };
        assert_eq!(fuse(&i, &t()), Presence::Present);
        assert!(!fuse(&i, &t()).suppresses_spoken_proactivity());
    }

    #[test]
    fn focus_dnd_forces_focused_even_when_conversational() {
        let i = PresenceInputs {
            secs_since_input: Some(10),
            secs_since_speech: Some(5),
            focus_dnd: true,
            ..Default::default()
        };
        assert_eq!(fuse(&i, &t()), Presence::Focused);
    }

    #[test]
    fn present_when_at_machine_but_not_actively_working() {
        // Input within the presence window but past the focus-idle window, no
        // speech => present (around, not in deep flow).
        let i = PresenceInputs {
            secs_since_input: Some(300),
            secs_since_speech: None,
            ..Default::default()
        };
        assert_eq!(fuse(&i, &t()), Presence::Present);
    }

    #[test]
    fn payload_is_secret_free_and_reports_signal_availability() {
        let i = PresenceInputs { secs_since_input: Some(10), ..Default::default() };
        let p = state_payload(fuse(&i, &t()), &i);
        assert_eq!(p["at_machine"], true);
        assert_eq!(p["signals"]["input"], true);
        assert_eq!(p["signals"]["vision"], false); // not sourced this tick
        // No raw timestamps leak into the payload.
        assert!(p.get("secs_since_input").is_none());
    }
}

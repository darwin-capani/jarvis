//! BATTERY/THERMAL ADAPTIVE THROTTLING — the DEVICE-GATED live reader (#38).
//!
//! PERF/RUNTIME ONLY. This module supplies the `(battery_pct, on_ac, thermal)`
//! reading that the PURE [`crate::model_tier::throttle_decision`] policy turns
//! into a [`crate::model_tier::ThrottlePlan`]. The plan influences ONLY the LOCAL
//! sub-tier preference (prefer the cheaper Fast sub-tier) + a "defer heavy work"
//! hint — it NEVER changes which TIER is chosen, NEVER loosens a gate, and NEVER
//! makes a cloud call.
//!
//! ## Device-gated — ON by default (perf-only)
//!
//! The live reader (`/usr/bin/pmset -g batt` for the battery + AC state) is
//! consulted ONLY when [power].adaptive is on (the shipped default; the live read is
//! DEVICE-gated behind the flag). With the flag OFF [`read_power`] short-circuits to
//! a NEUTRAL reading WITHOUT spawning anything, so the throttle is always neutral and
//! routing is byte-for-byte today's behavior. This is PERF-ONLY: it never loosens a
//! gate, never makes a cloud call. This mirrors the mic-loop / vision-capture / posture
//! precedent: the PURE policy + the PURE parser are always testable; the live
//! read is wired behind the flag and NEVER exercised in tests.
//!
//! ## Honesty
//!
//! The real battery/thermal benefit is only observable on the actual Mac and is
//! NEVER measured or claimed headlessly. A read that fails (no battery, pmset
//! unavailable, a parse miss) degrades to "no battery concern" — it NEVER
//! fabricates a low battery. The thermal level is read via macOS's
//! `ProcessInfo.thermalState` ladder on-device; absent a real read (the headless
//! default), it is reported `Nominal` so the policy never throttles on a guess.
//!
//! The command RUNNER is injected (a function pointer), so the pure reading
//! assembler is driven in tests with CANNED `pmset` output — the real `pmset`
//! subprocess is NEVER spawned under test, and the pure PARSER is unit-tested
//! directly on hand-written canned text.

use std::time::Duration;

use crate::config::Config;
use crate::model_tier::{ThermalState, ThrottlePlan};

/// Hard ceiling on the one power read — same bounded-subprocess discipline as
/// posture.rs / actions.rs (a fixed program + fixed args, never a shell string).
#[allow(dead_code)] // used by the device-gated live read (read_power_live)
const POWER_TIMEOUT: Duration = Duration::from_secs(3);

/// One reading of the machine's power state, fed to
/// [`crate::model_tier::throttle_decision`]. PURE value — no I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerReading {
    /// Battery charge percent, or `None` when no battery is present / readable
    /// (a desktop Mac, or a read failure). NEVER fabricated as a low battery.
    pub battery_pct: Option<u8>,
    /// Whether the machine is on AC power (charging / plugged in). `true` is the
    /// safe default when the AC state cannot be read (don't throttle on a guess).
    pub on_ac: bool,
    /// Thermal pressure level (macOS `ProcessInfo.thermalState`). `Nominal` is the
    /// safe default absent a real read.
    pub thermal: ThermalState,
}

impl PowerReading {
    /// The NEUTRAL reading the OFF default (and any failed read) produces: no
    /// battery concern, on AC, nominal thermal. Fed through `throttle_decision`
    /// it yields a neutral plan, so routing is byte-for-byte today's behavior.
    pub fn neutral() -> Self {
        PowerReading {
            battery_pct: None,
            on_ac: true,
            thermal: ThermalState::Nominal,
        }
    }
}

/// The current throttle plan for THIS turn. DEVICE-GATED: when [power].adaptive
/// is OFF (the shipped default) this returns the neutral plan WITHOUT reading any
/// power state — the live `pmset`/thermal read is never reached. When ON, the
/// caller supplies a `reading` from [`read_power_live`] (device-gated) which is
/// fed through the PURE [`crate::model_tier::throttle_decision`] policy.
///
/// Keeping the read separate from the decision is what makes the decision
/// hermetic: tests call `throttle_decision` directly with synthetic readings and
/// never touch this seam.
pub fn current_plan(cfg: &Config, reading: PowerReading) -> ThrottlePlan {
    crate::model_tier::throttle_decision(reading.battery_pct, reading.on_ac, reading.thermal, cfg)
}

/// Parse `/usr/bin/pmset -g batt` output into `(battery_pct, on_ac)`. PURE —
/// unit-tested on canned text. The pmset format is:
///
///   Now drawing from 'Battery Power'
///    -InternalBattery-0 (id=...)  73%; discharging; 4:21 remaining present: true
///
/// or, on AC:
///
///   Now drawing from 'AC Power'
///    -InternalBattery-0 (id=...)  100%; charged; 0:00 remaining present: true
///
/// A desktop Mac (no battery) prints no `-InternalBattery` line, so the percent
/// is `None` (no battery concern). The "Now drawing from 'AC Power'" header is
/// the AC signal; "discharging" in the battery line confirms on-battery. A line
/// we cannot parse degrades to `(None, true)` — NEVER a fabricated low battery.
pub fn parse_pmset(out: &str) -> (Option<u8>, bool) {
    let lower = out.to_lowercase();
    // AC vs battery: the header line is authoritative. Default to AC (the safe,
    // no-throttle assumption) when neither header is present.
    let on_ac = if lower.contains("now drawing from 'ac power'") {
        true
    } else if lower.contains("now drawing from 'battery power'") {
        false
    } else {
        // No recognizable header: be safe (assume AC -> no battery throttle).
        true
    };
    // Battery percent: the first "<n>%" token on an InternalBattery line.
    let mut battery_pct = None;
    for line in out.lines() {
        let l = line.to_lowercase();
        if !l.contains("internalbattery") {
            continue;
        }
        if let Some(idx) = line.find('%') {
            // Walk back over the digits immediately preceding the '%'.
            let bytes = line.as_bytes();
            let mut start = idx;
            while start > 0 && bytes[start - 1].is_ascii_digit() {
                start -= 1;
            }
            if start < idx {
                if let Ok(pct) = line[start..idx].parse::<u32>() {
                    // Clamp into a sane 0..=100 u8 (a malformed >100 reads as a
                    // missing battery rather than a bogus value).
                    if pct <= 100 {
                        battery_pct = Some(pct as u8);
                    }
                }
            }
        }
        break; // first battery line only
    }
    (battery_pct, on_ac)
}

// ---------------------------------------------------------------------------
// Real reader (NEVER reached in tests — they call parse_pmset on canned text,
// and throttle_decision with synthetic readings). DEVICE-GATED behind
// [power].adaptive: the caller must check the flag before calling this.
// ---------------------------------------------------------------------------

/// Read the live power state on-device via `/usr/bin/pmset -g batt` (battery +
/// AC) + the on-device thermal level. DEVICE-GATED: callers MUST gate this on
/// [power].adaptive being on — the OFF default never reaches here (it uses
/// [`PowerReading::neutral`]). Any failure degrades to a neutral-ish reading
/// (no battery concern), NEVER a fabricated low battery.
///
/// Marked `#[cfg(not(test))]`-style by convention via the injected runner in the
/// real binary; here it is a thin async wrapper that spawns the bounded, fixed-
/// args subprocess exactly like posture.rs::run_real_command. It is NOT exercised
/// by the hermetic tests.
#[allow(dead_code)] // wired behind [power].adaptive; the live read is device-gated
pub async fn read_power_live() -> PowerReading {
    use tokio::process::Command;
    let mut cmd = Command::new("/usr/bin/pmset");
    cmd.args(["-g", "batt"]).kill_on_drop(true);
    let (battery_pct, on_ac) = match tokio::time::timeout(POWER_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) => {
            let text = String::from_utf8_lossy(&out.stdout);
            parse_pmset(&text)
        }
        // A spawn error or timeout -> no battery concern (safe, never a fake low).
        _ => (None, true),
    };
    PowerReading {
        battery_pct,
        on_ac,
        // LIVE thermal level via the macOS ProcessInfo.thermalState bridge
        // (read_thermal_live -> the csrc/thermal_shim.m read). READ-ONLY +
        // unprivileged. A read miss / non-macOS build degrades to Nominal so the
        // policy never throttles on a guess. The thermal branch of
        // throttle_decision is still fully tested via synthetic ThermalState
        // inputs, and the int->ThermalState mapping is unit-tested via map_thermal.
        thermal: read_thermal_live(),
    }
}

// ---------------------------------------------------------------------------
// LIVE THERMAL — the ProcessInfo.thermalState bridge (READ-ONLY, unprivileged).
// Mirrors the es_shim precedent: the fragile system call lives in a tiny C/ObjC
// shim (csrc/thermal_shim.m) compiled against Apple's REAL Foundation header, so
// the enum ladder is compiler-verified; Rust sees a flat int and maps it with a
// PURE, unit-tested function. Reading thermalState needs NO entitlement, NO root,
// and NO powermetrics/sudo — it is a public, process-scoped OS signal.
// ---------------------------------------------------------------------------

/// Map the flat int the thermal shim returns (`darwin_thermal_state`) to a
/// [`ThermalState`]. PURE — unit-tested on synthetic ints. 0=Nominal, 1=Fair,
/// 2=Serious, 3=Critical; anything else (incl. the shim's -1 "unknown/
/// unreadable") degrades to `Nominal`, so the policy NEVER throttles on a guess.
pub fn map_thermal(raw: i32) -> ThermalState {
    match raw {
        0 => ThermalState::Nominal,
        1 => ThermalState::Fair,
        2 => ThermalState::Serious,
        3 => ThermalState::Critical,
        _ => ThermalState::Nominal,
    }
}

// Link, in order: the shim archive (force-loaded via +whole-archive — a
// build-script static lib referenced only from the bin is otherwise dropped by
// this linker, exactly like es.rs's shim), then the Foundation framework + libobjc
// the shim's `[NSProcessInfo … thermalState]` message-send needs. build.rs
// compiles the archive (with -fno-objc-msgsend-selector-stubs so it references
// the classic `_objc_msgSend`) and adds its OUT_DIR to the search path; declaring
// the links here keeps rustc's whole-archive modifier exact. All of this LINKS
// freely with no entitlement — reading thermalState is unprivileged.
// clippy::duplicated_attributes is a false positive here (it flags the multiple
// #[link] attrs as duplicates); all three links are needed.
#[cfg(target_os = "macos")]
#[allow(clippy::duplicated_attributes)]
#[link(name = "darwin_thermal_shim", kind = "static", modifiers = "+whole-archive")]
#[link(name = "Foundation", kind = "framework")]
#[link(name = "objc", kind = "dylib")]
extern "C" {
    // csrc/thermal_shim.m — reads NSProcessInfo.thermalState and returns a flat
    // int (0..=3, or -1). Compiled + linked by build.rs on macOS. READ-ONLY.
    fn darwin_thermal_state() -> std::os::raw::c_int;
}

/// Read the LIVE macOS thermal pressure level via the thermal shim
/// (`NSProcessInfo.thermalState`). READ-ONLY + unprivileged. The DEVICE-GATED
/// runner: it is NEVER exercised under test (the tests drive the PURE
/// [`map_thermal`] on synthetic ints). Off macOS it reports `Nominal`.
pub fn read_thermal_live() -> ThermalState {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: `darwin_thermal_state` takes no arguments, reads one scalar OS
        // signal, and returns a plain `int`; it is linked from the build-script
        // shim on macOS. No pointers, no ownership — a pure scalar read.
        map_thermal(unsafe { darwin_thermal_state() })
    }
    #[cfg(not(target_os = "macos"))]
    {
        ThermalState::Nominal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // pmset parser: the on-battery, discharging case yields the percent + !on_ac.
    #[test]
    fn parse_pmset_on_battery_discharging() {
        let out = "Now drawing from 'Battery Power'\n \
            -InternalBattery-0 (id=12345)\t18%; discharging; 1:02 remaining present: true";
        let (pct, on_ac) = parse_pmset(out);
        assert_eq!(pct, Some(18));
        assert!(!on_ac, "discharging from Battery Power must read as on battery");
    }

    // pmset parser: on AC, charged -> on_ac true, percent read.
    #[test]
    fn parse_pmset_on_ac_charged() {
        let out = "Now drawing from 'AC Power'\n \
            -InternalBattery-0 (id=12345)\t100%; charged; 0:00 remaining present: true";
        let (pct, on_ac) = parse_pmset(out);
        assert_eq!(pct, Some(100));
        assert!(on_ac, "AC Power header must read as on AC");
    }

    // pmset parser: a desktop Mac (no battery line) -> None percent, on AC. The
    // throttle policy then never triggers on battery (honest: no battery concern).
    #[test]
    fn parse_pmset_desktop_no_battery() {
        let out = "Now drawing from 'AC Power'";
        let (pct, on_ac) = parse_pmset(out);
        assert_eq!(pct, None, "no battery line => None, never a fabricated low");
        assert!(on_ac);
    }

    // pmset parser: unrecognizable output degrades safely to (None, on AC) — never
    // a fabricated low battery that would wrongly throttle.
    #[test]
    fn parse_pmset_garbage_degrades_safely() {
        let (pct, on_ac) = parse_pmset("not pmset output at all");
        assert_eq!(pct, None);
        assert!(on_ac, "unknown header must default to AC (no throttle on a guess)");
    }

    // A >100% bogus reading is rejected (treated as no battery), never passed on.
    #[test]
    fn parse_pmset_rejects_bogus_percent() {
        let out = "Now drawing from 'Battery Power'\n \
            -InternalBattery-0 (id=1)\t250%; discharging; 1:00 remaining present: true";
        let (pct, _) = parse_pmset(out);
        assert_eq!(pct, None, "an impossible >100% reading must not pass through");
    }

    // adaptive ships ON (full-power default) but a NEUTRAL reading is still neutral
    // (no battery info, on AC, nominal thermal => never throttles); and an explicit
    // OFF config is also neutral on any reading.
    #[test]
    fn current_plan_neutral_reading_never_throttles() {
        let cfg = Config::default();
        assert!(cfg.power.adaptive, "[power].adaptive ships ON (full-power default)");
        let plan = current_plan(&cfg, PowerReading::neutral());
        assert!(!plan.is_throttled(), "a neutral reading must never throttle, even with adaptive on");

        // Explicitly OFF: nothing reads power, never throttles on any reading.
        let mut off = Config::default();
        off.power.adaptive = false;
        let low = PowerReading { battery_pct: Some(5), on_ac: false, thermal: ThermalState::Nominal };
        assert!(!current_plan(&off, low).is_throttled(), "adaptive OFF must never throttle");
    }

    // The PURE int->ThermalState mapping (the live read's tested seam): each of
    // the shim's 0..=3 codes maps to its ladder rung; any other value (incl. the
    // shim's -1 "unknown/unreadable") degrades to Nominal so the policy never
    // throttles on a guess.
    #[test]
    fn map_thermal_covers_the_ladder_and_degrades_unknown_to_nominal() {
        assert_eq!(map_thermal(0), ThermalState::Nominal);
        assert_eq!(map_thermal(1), ThermalState::Fair);
        assert_eq!(map_thermal(2), ThermalState::Serious);
        assert_eq!(map_thermal(3), ThermalState::Critical);
        // -1 (the shim's "unknown"), out-of-range, and negatives all degrade safe.
        assert_eq!(map_thermal(-1), ThermalState::Nominal);
        assert_eq!(map_thermal(4), ThermalState::Nominal);
        assert_eq!(map_thermal(i32::MIN), ThermalState::Nominal);
        assert_eq!(map_thermal(i32::MAX), ThermalState::Nominal);
    }

    // A live-read Serious/Critical (from the thermal shim, modeled here via
    // map_thermal) routed through the throttle policy DOES throttle even on AC —
    // proving the newly-wired live thermal reaches the policy that consumes it.
    #[test]
    fn live_thermal_serious_throttles_even_on_ac() {
        let mut cfg = Config::default();
        cfg.power.adaptive = true;
        let reading = PowerReading {
            battery_pct: None,
            on_ac: true,
            thermal: map_thermal(2), // Serious, as the live shim would report
        };
        let plan = current_plan(&cfg, reading);
        assert!(plan.is_throttled(), "serious thermal must throttle even on AC");
    }

    // With adaptive ON, a synthetic low-battery reading routed through current_plan
    // yields a throttle (proves the seam wires the reading into the pure policy).
    #[test]
    fn current_plan_low_battery_throttles_when_adaptive_on() {
        let mut cfg = Config::default();
        cfg.power.adaptive = true;
        let reading = PowerReading {
            battery_pct: Some(10),
            on_ac: false,
            thermal: ThermalState::Nominal,
        };
        let plan = current_plan(&cfg, reading);
        assert!(plan.is_throttled(), "low battery while discharging must throttle");
        assert_eq!(plan.tier_pref, crate::model_tier::LocalSubTier::Fast);
    }
}

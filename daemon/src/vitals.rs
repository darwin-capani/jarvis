//! `hardware.vitals` — a LIVE, STRICTLY READ-ONLY macOS hardware vitals feed.
//!
//! DARWIN "knows its machine": a bounded poll ([vitals].poll_secs) that SURFACES
//! the real device state to the HUD `hardware.vitals` panel — battery %, AC/charge
//! state, LIVE thermal pressure (ProcessInfo.thermalState), memory pressure,
//! per-core CPU utilization + load average, and every mounted volume's free/total.
//!
//! ## Contract: READ-ONLY, unprivileged, honest
//!
//!   * READ-ONLY — it OBSERVES and REPORTS. It takes NO action, touches NO
//!     actuator, and makes NO change to the machine. There is no remediation path.
//!   * UNPRIVILEGED — every read is a public, unprivileged observation. NO root,
//!     NO sudo, NO powermetrics. (GPU/ANE utilization needs a privileged helper —
//!     see the note below — and is DELIBERATELY not attempted.)
//!   * SECRET-FREE — the wire carries only device metrics + volume LABELS +
//!     free/total bytes. No file contents, no per-file data, no user content.
//!   * HONEST / degrades cleanly — each field is a REAL read; when a read fails
//!     the field degrades to an honest "unknown"/`None`, NEVER a fabricated value.
//!     A desktop Mac (no battery) reports `percent: None`, not a fake charge.
//!
//! ## PURE seam vs DEVICE-GATED runner (mirrors power.rs / posture.rs)
//!
//! The parse/assembly is a PURE, unit-tested seam:
//!   * [`parse_battery`] / [`parse_battery_state`] — pmset text -> a battery reading
//!     (reusing power.rs's tested [`crate::power::parse_pmset`]),
//!   * [`mem_pressure_level`] — the used-fraction -> Normal/Warn/Critical mapping,
//!   * [`VitalsSnapshot::to_json`] — the clamp/round/assemble -> wire JSON.
//!
//! The LIVE reads ([`vitals_task`], [`read_battery_live`], [`collect_volumes`], the
//! sysinfo + thermal-shim reads) are the DEVICE-GATED runner and are NEVER
//! exercised under test — the tests drive the pure seam on synthetic inputs.
//!
//! ## Memory pressure — an HONEST heuristic
//!
//! [`mem_pressure_level`] is derived from the USED FRACTION (used/total) via fixed
//! thresholds. It is a coarse, honest indicator — NOT the kernel's compressor-based
//! `memory_pressure`. It is labeled as a level, never as a measured kernel score.
//!
//! ## Documented future privileged helper (NOT done here)
//!
//! Per-GPU / ANE utilization + fine power draw (the kind `powermetrics` reports)
//! require root / a privileged sampling helper. That is OUT of the read-only,
//! no-root contract and is intentionally NOT attempted; it is a documented future
//! privileged-helper surface, not a gap in this benign-only feed.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::config::Config;
use crate::model_tier::ThermalState;

/// Hard floor on the poll cadence, so a hostile/typo'd `poll_secs = 0` can never
/// busy-spin the read loop. The read itself is cheap, but a live panel does not
/// need sub-2s device metrics.
pub const VITALS_MIN_POLL_SECS: u64 = 2;

/// Bound on the number of volumes surfaced per frame, so a machine with a swarm
/// of mounts can't flood the frame / the HUD DOM. The HUD parser caps again.
pub const VITALS_MAX_VOLUMES: usize = 24;

/// Same bounded-subprocess discipline as power.rs: a fixed program + fixed args,
/// never a shell string, with a hard timeout.
const BATTERY_TIMEOUT: Duration = Duration::from_secs(3);

// ---------------------------------------------------------------------------
// BATTERY — %, AC state, and a coarse charge state (PURE parse over pmset text).
// ---------------------------------------------------------------------------

/// The coarse charge state parsed from the pmset battery line. HONEST: an
/// unreadable/absent line reads `Unknown`, never a fabricated state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatteryState {
    /// On battery, drawing down.
    Discharging,
    /// On AC, actively charging (incl. "finishing charge").
    Charging,
    /// Full / held (incl. macOS "AC attached; not charging" optimized-charging).
    Charged,
    /// No battery line, or an unrecognized state word.
    Unknown,
}

impl BatteryState {
    /// Stable identifier for the `hardware.vitals` wire / the HUD indicator.
    pub fn as_str(&self) -> &'static str {
        match self {
            BatteryState::Discharging => "discharging",
            BatteryState::Charging => "charging",
            BatteryState::Charged => "charged",
            BatteryState::Unknown => "unknown",
        }
    }
}

/// One battery reading. `percent` is `None` on a desktop Mac / read failure
/// (NEVER a fabricated low), `on_ac` is the AC-power state, `state` the coarse
/// charge state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatteryReading {
    pub percent: Option<u8>,
    pub on_ac: bool,
    pub state: BatteryState,
}

/// Parse `/usr/bin/pmset -g batt` output into a [`BatteryReading`]. PURE —
/// unit-tested on canned text. Reuses power.rs's tested [`crate::power::parse_pmset`]
/// for `(percent, on_ac)` and adds the coarse charge state.
pub fn parse_battery(out: &str) -> BatteryReading {
    let (percent, on_ac) = crate::power::parse_pmset(out);
    BatteryReading {
        percent,
        on_ac,
        state: parse_battery_state(out),
    }
}

/// Extract the coarse charge state from the pmset battery line. PURE. The order
/// matters: "not charging" (optimized-charging hold) must be checked BEFORE
/// "charging" so it never reads as actively charging.
pub fn parse_battery_state(out: &str) -> BatteryState {
    for line in out.lines() {
        let l = line.to_lowercase();
        if !l.contains("internalbattery") {
            continue;
        }
        if l.contains("discharging") {
            return BatteryState::Discharging;
        }
        // "AC attached; not charging" (full / optimized-charging hold): check
        // BEFORE the "charging" substring so it never reads as charging.
        if l.contains("not charging") || l.contains("charged") {
            return BatteryState::Charged;
        }
        if l.contains("finishing charge") || l.contains("charging") {
            return BatteryState::Charging;
        }
        return BatteryState::Unknown; // battery line present but unrecognized word
    }
    BatteryState::Unknown // no battery line (desktop Mac / read miss)
}

// ---------------------------------------------------------------------------
// MEMORY PRESSURE — an HONEST used-fraction heuristic (Normal/Warn/Critical).
// ---------------------------------------------------------------------------

/// The memory-pressure LEVEL. HONEST: a coarse used-fraction indicator, NOT the
/// kernel's compressor-based `memory_pressure` score.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemPressure {
    Normal,
    Warn,
    Critical,
}

impl MemPressure {
    /// Stable identifier for the wire / the HUD indicator.
    pub fn as_str(&self) -> &'static str {
        match self {
            MemPressure::Normal => "normal",
            MemPressure::Warn => "warn",
            MemPressure::Critical => "critical",
        }
    }
}

/// Map `used`/`total` bytes to a coarse [`MemPressure`] level. PURE. Thresholds:
/// `>= 90%` used -> Critical, `>= 75%` -> Warn, else Normal. A zero/absent total
/// degrades to Normal (never a fabricated pressure).
pub fn mem_pressure_level(used: u64, total: u64) -> MemPressure {
    if total == 0 {
        return MemPressure::Normal;
    }
    let frac = used as f64 / total as f64;
    if frac >= 0.90 {
        MemPressure::Critical
    } else if frac >= 0.75 {
        MemPressure::Warn
    } else {
        MemPressure::Normal
    }
}

// ---------------------------------------------------------------------------
// VOLUMES — every mounted volume's free/total (SECRET-FREE: label + bytes only).
// ---------------------------------------------------------------------------

/// One mounted volume's free/total. SECRET-FREE by construction: only the volume
/// LABEL + mount path (both already shown in Finder) and the free/total bytes —
/// never a file listing or any per-file data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeReading {
    pub label: String,
    pub mount: String,
    pub free_bytes: u64,
    pub total_bytes: u64,
}

// ---------------------------------------------------------------------------
// THE SNAPSHOT — a PURE value; `to_json` is the tested assemble seam.
// ---------------------------------------------------------------------------

/// One assembled reading of the machine's vitals. A PURE value — the live task
/// fills it from the device reads, and [`to_json`](Self::to_json) is the tested
/// clamp/round/assemble seam.
#[derive(Debug, Clone, PartialEq)]
pub struct VitalsSnapshot {
    pub battery: BatteryReading,
    pub thermal: ThermalState,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    /// Per-core CPU utilization percent (one entry per logical core).
    pub cpu_per_core: Vec<f32>,
    /// Load average (1 / 5 / 15 minute).
    pub load_avg: (f64, f64, f64),
    pub volumes: Vec<VolumeReading>,
    pub uptime_secs: u64,
}

/// Clamp a CPU percent into a sane 0..=100 and round to 1 decimal (as f64 for a
/// clean JSON number). A NaN/inf reading (never expected) clamps to 0.
fn round_pct(c: f32) -> f64 {
    let c = if c.is_finite() { c.clamp(0.0, 100.0) } else { 0.0 };
    ((c as f64) * 10.0).round() / 10.0
}

/// Round a load-average component to 2 decimals; a non-finite value -> 0.
fn round_load(x: f64) -> f64 {
    if x.is_finite() {
        (x.max(0.0) * 100.0).round() / 100.0
    } else {
        0.0
    }
}

impl VitalsSnapshot {
    /// Assemble the SECRET-FREE `hardware.vitals` wire JSON. PURE — clamps/rounds
    /// the metrics, derives the memory-pressure level, and emits only device
    /// metrics + volume labels/bytes. Unit-tested on synthetic snapshots.
    pub fn to_json(&self) -> Value {
        let per_core: Vec<f64> = self.cpu_per_core.iter().map(|&c| round_pct(c)).collect();
        let volumes: Vec<Value> = self
            .volumes
            .iter()
            .map(|v| {
                json!({
                    "label": v.label,
                    "mount": v.mount,
                    "free_bytes": v.free_bytes,
                    "total_bytes": v.total_bytes,
                })
            })
            .collect();
        json!({
            "battery": {
                "percent": self.battery.percent,
                "on_ac": self.battery.on_ac,
                "charge_state": self.battery.state.as_str(),
            },
            "thermal": self.thermal.as_str(),
            "memory": {
                "used_bytes": self.mem_used_bytes,
                "total_bytes": self.mem_total_bytes,
                "pressure": mem_pressure_level(self.mem_used_bytes, self.mem_total_bytes).as_str(),
            },
            "cpu": {
                "per_core": per_core,
                "load_avg": [
                    round_load(self.load_avg.0),
                    round_load(self.load_avg.1),
                    round_load(self.load_avg.2),
                ],
            },
            "volumes": volumes,
            "uptime_secs": self.uptime_secs,
        })
    }
}

// ---------------------------------------------------------------------------
// DEVICE-GATED RUNNER — the live reads (NEVER exercised under test).
// ---------------------------------------------------------------------------

/// Read the live battery state on-device via `/usr/bin/pmset -g batt` (bounded,
/// fixed-args subprocess — the power.rs discipline). Any failure/timeout degrades
/// to an honest "no battery concern, unknown state" — NEVER a fabricated low.
/// DEVICE-GATED: not exercised under test (the tests drive [`parse_battery`]).
async fn read_battery_live() -> BatteryReading {
    use tokio::process::Command;
    let mut cmd = Command::new("/usr/bin/pmset");
    cmd.args(["-g", "batt"]).kill_on_drop(true);
    match tokio::time::timeout(BATTERY_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) => parse_battery(&String::from_utf8_lossy(&out.stdout)),
        _ => BatteryReading {
            percent: None,
            on_ac: true,
            state: BatteryState::Unknown,
        },
    }
}

/// Enumerate every mounted volume's free/total (SECRET-FREE: label + mount +
/// bytes only), bounded to [`VITALS_MAX_VOLUMES`]. Zero-capacity pseudo-mounts
/// are skipped (no meaningful free/total). DEVICE-GATED runner.
fn collect_volumes(disks: &sysinfo::Disks) -> Vec<VolumeReading> {
    disks
        .iter()
        .filter(|d| d.total_space() > 0)
        .take(VITALS_MAX_VOLUMES)
        .map(|d| VolumeReading {
            label: d.name().to_string_lossy().into_owned(),
            mount: d.mount_point().to_string_lossy().into_owned(),
            free_bytes: d.available_space(),
            total_bytes: d.total_space(),
        })
        .collect()
}

/// The live `hardware.vitals` poll. STRICTLY READ-ONLY: every tick it OBSERVES
/// the device (battery via pmset, thermal via the ProcessInfo shim, memory +
/// per-core CPU + load via sysinfo, all mounted volumes) and emits a SECRET-FREE
/// snapshot for the HUD. It acts on nothing. Gated by [vitals].enabled — OFF, it
/// returns immediately and never spawns a read. The poll cadence is clamped to
/// [`VITALS_MIN_POLL_SECS`].
pub async fn vitals_task(cfg: Arc<Config>) {
    if !cfg.vitals.enabled {
        return;
    }
    let poll = cfg.vitals.poll_secs.max(VITALS_MIN_POLL_SECS);
    let mut sys = sysinfo::System::new_all();
    let mut interval = tokio::time::interval(Duration::from_secs(poll));
    loop {
        interval.tick().await;
        // Per-core CPU deltas need two refreshes; the inter-tick gap (>= 2s)
        // supplies the delta, so each tick refreshes against the prior one.
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        let cpu_per_core: Vec<f32> = sys.cpus().iter().map(|c| c.cpu_usage()).collect();
        let la = sysinfo::System::load_average();
        let disks = sysinfo::Disks::new_with_refreshed_list();
        let snapshot = VitalsSnapshot {
            battery: read_battery_live().await,
            thermal: crate::power::read_thermal_live(),
            mem_used_bytes: sys.used_memory(),
            mem_total_bytes: sys.total_memory(),
            cpu_per_core,
            load_avg: (la.one, la.five, la.fifteen),
            volumes: collect_volumes(&disks),
            uptime_secs: sysinfo::System::uptime(),
        };
        crate::telemetry::emit("system", "hardware.vitals", snapshot.to_json());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- BATTERY parse (PURE, over canned pmset text) ------------------------

    #[test]
    fn parse_battery_on_battery_discharging() {
        let out = "Now drawing from 'Battery Power'\n \
            -InternalBattery-0 (id=1)\t42%; discharging; 2:10 remaining present: true";
        let b = parse_battery(out);
        assert_eq!(b.percent, Some(42));
        assert!(!b.on_ac);
        assert_eq!(b.state, BatteryState::Discharging);
    }

    #[test]
    fn parse_battery_on_ac_charging() {
        let out = "Now drawing from 'AC Power'\n \
            -InternalBattery-0 (id=1)\t80%; charging; 0:35 remaining present: true";
        let b = parse_battery(out);
        assert_eq!(b.percent, Some(80));
        assert!(b.on_ac);
        assert_eq!(b.state, BatteryState::Charging);
    }

    #[test]
    fn parse_battery_charged_and_not_charging_both_read_charged() {
        let charged = "Now drawing from 'AC Power'\n \
            -InternalBattery-0 (id=1)\t100%; charged; 0:00 remaining present: true";
        assert_eq!(parse_battery(charged).state, BatteryState::Charged);
        // "not charging" (optimized-charging hold) must NOT read as charging.
        let held = "Now drawing from 'AC Power'\n \
            -InternalBattery-0 (id=1)\t80%; AC attached; not charging present: true";
        let b = parse_battery(held);
        assert_eq!(b.state, BatteryState::Charged, "'not charging' must not read as charging");
        assert!(b.on_ac);
    }

    #[test]
    fn parse_battery_desktop_no_battery_is_unknown_none() {
        let b = parse_battery("Now drawing from 'AC Power'");
        assert_eq!(b.percent, None, "no battery line => None, never a fabricated low");
        assert!(b.on_ac);
        assert_eq!(b.state, BatteryState::Unknown);
    }

    // --- MEMORY pressure (PURE) ----------------------------------------------

    #[test]
    fn mem_pressure_thresholds_and_zero_total_degrade() {
        assert_eq!(mem_pressure_level(0, 16), MemPressure::Normal);
        assert_eq!(mem_pressure_level(11, 16), MemPressure::Normal); // ~69%
        assert_eq!(mem_pressure_level(12, 16), MemPressure::Warn); // 75%
        assert_eq!(mem_pressure_level(15, 16), MemPressure::Critical); // ~94%
        assert_eq!(mem_pressure_level(16, 16), MemPressure::Critical); // 100%
        // A zero/absent total degrades to Normal, never a fabricated pressure.
        assert_eq!(mem_pressure_level(5, 0), MemPressure::Normal);
    }

    // --- to_json ASSEMBLE seam (PURE) ----------------------------------------

    fn sample_snapshot() -> VitalsSnapshot {
        VitalsSnapshot {
            battery: BatteryReading {
                percent: Some(55),
                on_ac: false,
                state: BatteryState::Discharging,
            },
            thermal: ThermalState::Serious,
            mem_used_bytes: 12,
            mem_total_bytes: 16,
            cpu_per_core: vec![10.04, 200.0, -5.0, f32::NAN],
            load_avg: (1.234, 0.5, 0.0),
            volumes: vec![
                VolumeReading {
                    label: "Macintosh HD".into(),
                    mount: "/".into(),
                    free_bytes: 100,
                    total_bytes: 500,
                },
                VolumeReading {
                    label: "Backup".into(),
                    mount: "/Volumes/Backup".into(),
                    free_bytes: 20,
                    total_bytes: 40,
                },
            ],
            uptime_secs: 3661,
        }
    }

    #[test]
    fn to_json_is_secret_free_clamped_and_rounded() {
        let v = sample_snapshot().to_json();

        // Battery: percent/on_ac/charge_state carried honestly.
        assert_eq!(v["battery"]["percent"], json!(55));
        assert_eq!(v["battery"]["on_ac"], json!(false));
        assert_eq!(v["battery"]["charge_state"], json!("discharging"));

        // Live thermal ladder rung as a stable string.
        assert_eq!(v["thermal"], json!("serious"));

        // Memory: raw bytes + the derived pressure level (12/16 = 75% => warn).
        assert_eq!(v["memory"]["used_bytes"], json!(12));
        assert_eq!(v["memory"]["total_bytes"], json!(16));
        assert_eq!(v["memory"]["pressure"], json!("warn"));

        // CPU: each core clamped to 0..=100 and rounded to 1dp; NaN -> 0.
        let per_core = v["cpu"]["per_core"].as_array().unwrap();
        assert_eq!(per_core[0], json!(10.0)); // 10.04 -> 10.0
        assert_eq!(per_core[1], json!(100.0)); // 200 clamped to 100
        assert_eq!(per_core[2], json!(0.0)); // -5 clamped to 0
        assert_eq!(per_core[3], json!(0.0)); // NaN -> 0
        // Load average rounded to 2dp, floored at 0.
        assert_eq!(v["cpu"]["load_avg"], json!([1.23, 0.5, 0.0]));

        // Volumes: only label/mount/free/total — no file data of any kind.
        let vols = v["volumes"].as_array().unwrap();
        assert_eq!(vols.len(), 2);
        assert_eq!(vols[0]["label"], json!("Macintosh HD"));
        assert_eq!(vols[0]["mount"], json!("/"));
        assert_eq!(vols[0]["free_bytes"], json!(100));
        assert_eq!(vols[0]["total_bytes"], json!(500));
        // The volume object exposes EXACTLY the four secret-free keys (serde_json
        // sorts object keys, so compare as a set — no file-listing key can leak).
        let mut keys: Vec<&str> =
            vols[0].as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["free_bytes", "label", "mount", "total_bytes"]);

        assert_eq!(v["uptime_secs"], json!(3661));
    }

    #[test]
    fn to_json_desktop_no_battery_is_null_percent_not_fabricated() {
        let mut snap = sample_snapshot();
        snap.battery = BatteryReading {
            percent: None,
            on_ac: true,
            state: BatteryState::Unknown,
        };
        snap.thermal = ThermalState::Nominal;
        let v = snap.to_json();
        assert!(v["battery"]["percent"].is_null(), "no battery => null, never a fake charge");
        assert_eq!(v["battery"]["on_ac"], json!(true));
        assert_eq!(v["battery"]["charge_state"], json!("unknown"));
        assert_eq!(v["thermal"], json!("nominal"));
    }
}

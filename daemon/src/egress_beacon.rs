//! Egress Baseline + Beacon Detector — the longitudinal follow-on named in the
//! `egress.rs` header ("longitudinal baseline + new-beacon alerting +
//! propose-only firewall suggestion"). It sits ON TOP of the read-only Egress
//! Sentinel: `egress::sample_talkers()` gives it the same lsof-based
//! outbound-connection snapshot, and this module keeps a BOUNDED longitudinal
//! store of who-talks-to-whom so it can answer two questions with PURE,
//! unit-tested classifiers:
//!
//!   1. new-host diff  — a `(process, host)` pair never seen before
//!      ([`diff_new_hosts`]).
//!   2. beacon cadence — a host contacted at a suspiciously REGULAR interval,
//!      the classic C2 callback signature ([`classify_cadence`]): low
//!      coefficient-of-variation of the inter-arrival deltas over enough samples.
//!
//! DEFENSIVE, READ-ONLY, PROPOSE-ONLY. This module CHANGES NOTHING on the host.
//! The strongest thing it does is RENDER a pf/pfctl rule as TEXT
//! ([`render_block_proposal`]) the operator reviews and applies themselves with
//! `sudo` — it never shells `pfctl`, never mutates the firewall, and has no
//! consequential surface (same discipline as `egress.rs` / `posture.rs` /
//! `tcc.rs`).
//!
//! RIDES THE EDITH GUARDS. So an alert can never spam, every finding passes
//! through [`guard_alert`], which reuses EDITH's EXACT quiet-hours band
//! ([`crate::anticipate::in_quiet_hours`], sourced from `[proactive]`) and
//! mirrors EDITH's per-key cooldown + global debounce ([`AlertLedger`], the same
//! shape as `anticipate::FiredState`). Within quiet hours, or inside a cooldown/
//! debounce window, the finding is suppressed silently.
//!
//! RISING-EDGE SAMPLING. The store records a timestamp only on a rising edge
//! (a talker ABSENT last sample, PRESENT now), not on every sample. That is what
//! separates a genuine short-lived beacon (a fresh connection each interval →
//! one edge per interval → a regular series) from a benign LONG-LIVED connection
//! (one socket held open → a single edge, no series). Without this, a persistent
//! poller sampled every N seconds would masquerade as a perfect N-second beacon.
//!
//! HONEST CAVEATS (never papered over):
//!   * Attribution is UID-scoped. Unprivileged `lsof` attributes only same-UID
//!     processes; connections owned by other users are invisible here. The
//!     [`UID_CAVEAT`] string rides every alert frame so the HUD/operator sees it.
//!   * Cadence resolution is bounded by the sample interval. A beacon that opens
//!     AND closes entirely between two samples is never observed by snapshot
//!     sampling — this detector sees only connections established at a sample
//!     instant. It is an advisory signal, not a packet-level IDS.

use std::collections::{HashSet, VecDeque};
use std::time::Duration;

use chrono::Timelike;
use serde_json::{json, Value};
use tracing::{debug, info};

/// Rides every alert frame: the standing honesty note about UID-scoped
/// attribution. Stated, not hidden.
pub const UID_CAVEAT: &str =
    "unprivileged lsof attributes only same-UID processes; connections owned by \
     other users are not visible to this detector.";

// ---------------------------------------------------------------------------
// Config-derived knobs (pure data so the classifiers stay functions of input)
// ---------------------------------------------------------------------------

/// Beacon-cadence thresholds. A talker's rising-edge series is flagged as a
/// beacon when it has at least `min_samples` timestamps, a mean inter-arrival
/// within `[min_interval_secs, max_interval_secs]`, and a coefficient of
/// variation (stddev/mean of the deltas) at or below `max_jitter_ratio`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BeaconThresholds {
    /// Minimum number of timestamps (→ `min_samples - 1` intervals) required
    /// before a cadence verdict is trustworthy.
    pub min_samples: usize,
    /// Below this mean interval the series is treated as bursty reconnection
    /// noise, not a beacon.
    pub min_interval_secs: u64,
    /// Above this mean interval the cadence is indistinguishable from ordinary
    /// slow polling at our sample resolution — an honest ceiling.
    pub max_interval_secs: u64,
    /// Coefficient-of-variation ceiling. A tight, regular cadence sits well
    /// below this; a jittery/random one blows past it.
    pub max_jitter_ratio: f64,
}

impl BeaconThresholds {
    /// Read the thresholds from the `[egress]` config section.
    pub fn from_config(cfg: &crate::config::EgressConfig) -> Self {
        Self {
            min_samples: cfg.beacon_min_samples,
            min_interval_secs: cfg.beacon_min_interval_secs,
            max_interval_secs: cfg.beacon_max_interval_secs,
            max_jitter_ratio: cfg.beacon_max_jitter,
        }
    }
}

/// Bounded-retention policy for the longitudinal store.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetentionPolicy {
    /// Hard cap on distinct talkers held; the least-recently-seen is evicted
    /// when a new talker would exceed it.
    pub max_talkers: usize,
    /// Ring cap on rising-edge timestamps kept per talker.
    pub max_samples_per_talker: usize,
    /// A talker not seen for longer than this (seconds) is pruned.
    pub retention_secs: u64,
}

impl RetentionPolicy {
    /// Read the retention policy from the `[egress]` config section.
    pub fn from_config(cfg: &crate::config::EgressConfig) -> Self {
        Self {
            max_talkers: cfg.max_talkers,
            max_samples_per_talker: cfg.max_samples_per_talker,
            retention_secs: cfg.retention_secs,
        }
    }
}

// ---------------------------------------------------------------------------
// Observation + host:port splitting (pure)
// ---------------------------------------------------------------------------

/// One sampled outbound observation: which process talked to which host+port,
/// at which unix-second sample time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub process: String,
    pub host: String,
    pub port: u16,
    pub ts: u64,
}

/// PURE split of an lsof `NAME` remote endpoint ("host:port") into `(host,
/// port)`. Handles bracketed IPv6 (`[2001:db8::1]:443`) and plain IPv4
/// (`1.2.3.4:443`); an unparseable port degrades to 0 rather than panicking.
pub fn split_host_port(remote: &str) -> (String, u16) {
    if let Some(rest) = remote.strip_prefix('[') {
        // IPv6 in brackets: [host]:port
        if let Some((host, port)) = rest.split_once("]:") {
            return (host.to_string(), port.parse().unwrap_or(0));
        }
        return (rest.trim_end_matches(']').to_string(), 0);
    }
    // IPv4 host:port — split on the LAST ':' so a bare host still works.
    match remote.rsplit_once(':') {
        Some((host, port)) => (host.to_string(), port.parse().unwrap_or(0)),
        None => (remote.to_string(), 0),
    }
}

// ---------------------------------------------------------------------------
// Longitudinal store (bounded; rising-edge timestamps)
// ---------------------------------------------------------------------------

/// The identity of a talker: exactly the `(process, host, port)` tuple.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TalkerKey {
    process: String,
    host: String,
    port: u16,
}

/// One talker's longitudinal record: its rising-edge timestamps (bounded ring)
/// and when it was last seen (for pruning + LRU eviction).
struct TalkerRecord {
    key: TalkerKey,
    last_seen: u64,
    /// Rising-edge sample times (absent→present transitions), oldest-first,
    /// bounded to `max_samples_per_talker`.
    edges: VecDeque<u64>,
}

/// The bounded longitudinal store. Holds the per-talker rising-edge series plus
/// the set of talkers PRESENT in the last ingested sample (so the next sample
/// can compute rising edges). Everything is bounded: distinct talkers by
/// `max_talkers`, per-talker edges by `max_samples_per_talker`, and stale
/// talkers pruned past `retention_secs`.
pub struct BaselineStore {
    talkers: Vec<TalkerRecord>,
    present: HashSet<TalkerKey>,
    retention: RetentionPolicy,
}

impl BaselineStore {
    pub fn new(retention: RetentionPolicy) -> Self {
        Self {
            talkers: Vec::new(),
            present: HashSet::new(),
            retention,
        }
    }

    /// The set of `(process, host)` pairs the store has EVER recorded — the
    /// baseline the new-host diff is taken against.
    pub fn known_host_pairs(&self) -> HashSet<(String, String)> {
        self.talkers
            .iter()
            .map(|r| (r.key.process.clone(), r.key.host.clone()))
            .collect()
    }

    /// Every rising-edge timestamp for a `(process, host)` pair, merged across
    /// ports and sorted — the series [`classify_cadence`] reasons over.
    pub fn edge_timestamps(&self, process: &str, host: &str) -> Vec<u64> {
        let mut out: Vec<u64> = self
            .talkers
            .iter()
            .filter(|r| r.key.process == process && r.key.host == host)
            .flat_map(|r| r.edges.iter().copied())
            .collect();
        out.sort_unstable();
        out
    }

    /// Ingest one sample. A talker present now but ABSENT in the previous sample
    /// is a rising edge and stamps `now` onto its (possibly new) record. Then
    /// stale talkers are pruned and the distinct-talker cap enforced.
    pub fn ingest_sample(&mut self, obs: &[Observation], now: u64) {
        let current: HashSet<TalkerKey> = obs
            .iter()
            .map(|o| TalkerKey {
                process: o.process.clone(),
                host: o.host.clone(),
                port: o.port,
            })
            .collect();
        for key in &current {
            if !self.present.contains(key) {
                self.record_edge(key.clone(), now);
            }
        }
        self.present = current;
        self.prune(now);
    }

    /// Stamp a rising edge for `key` at `now`, creating the record if new and
    /// keeping the per-talker edge ring + distinct-talker cap bounded.
    fn record_edge(&mut self, key: TalkerKey, now: u64) {
        if let Some(rec) = self.talkers.iter_mut().find(|r| r.key == key) {
            rec.last_seen = now;
            rec.edges.push_back(now);
            while rec.edges.len() > self.retention.max_samples_per_talker.max(1) {
                rec.edges.pop_front();
            }
            return;
        }
        // New talker: evict the least-recently-seen if we are at the cap.
        if self.talkers.len() >= self.retention.max_talkers.max(1) {
            if let Some((idx, _)) = self
                .talkers
                .iter()
                .enumerate()
                .min_by_key(|(_, r)| r.last_seen)
            {
                self.talkers.swap_remove(idx);
            }
        }
        let mut edges = VecDeque::new();
        edges.push_back(now);
        self.talkers.push(TalkerRecord {
            key,
            last_seen: now,
            edges,
        });
    }

    /// Drop talkers unseen for longer than `retention_secs`.
    fn prune(&mut self, now: u64) {
        let cutoff = self.retention.retention_secs;
        self.talkers
            .retain(|r| now.saturating_sub(r.last_seen) <= cutoff);
    }
}

// ---------------------------------------------------------------------------
// Classifier 1 — new-host baseline diff (PURE)
// ---------------------------------------------------------------------------

/// PURE: which current observations are talkers whose `(process, host)` pair is
/// NOT in the baseline `known` set — first-seen talkers. At most one
/// observation per new pair is returned (the first), so several ports to one new
/// host raise a single finding.
pub fn diff_new_hosts(current: &[Observation], known: &HashSet<(String, String)>) -> Vec<Observation> {
    let mut seen: HashSet<(String, String)> = HashSet::new();
    current
        .iter()
        .filter(|o| {
            let pair = (o.process.clone(), o.host.clone());
            !known.contains(&pair) && seen.insert(pair)
        })
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Classifier 2 — beacon cadence (PURE)
// ---------------------------------------------------------------------------

/// The verdict of the cadence classifier for one talker's timestamp series.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BeaconVerdict {
    pub is_beacon: bool,
    /// Mean inter-arrival delta (seconds); 0.0 when undecidable.
    pub period_secs: f64,
    /// Coefficient of variation of the deltas (stddev/mean); lower = more
    /// regular. 0.0 when undecidable.
    pub jitter_ratio: f64,
    /// Number of timestamps considered.
    pub samples: usize,
}

impl BeaconVerdict {
    fn not_beacon(samples: usize) -> Self {
        Self {
            is_beacon: false,
            period_secs: 0.0,
            jitter_ratio: 0.0,
            samples,
        }
    }
}

/// PURE beacon-cadence classifier. Given a timestamp series (any order) and the
/// thresholds, decide whether the inter-arrival deltas are regular enough — and
/// on a plausible interval — to look like a C2 callback. A perfectly periodic
/// series has coefficient of variation 0; bursty or random traffic drives it
/// high. Undecidable inputs (too few samples, a non-positive mean) return a
/// non-beacon verdict, never a panic.
pub fn classify_cadence(timestamps: &[u64], t: &BeaconThresholds) -> BeaconVerdict {
    let samples = timestamps.len();
    if samples < t.min_samples.max(2) {
        return BeaconVerdict::not_beacon(samples);
    }
    let mut ts = timestamps.to_vec();
    ts.sort_unstable();
    let deltas: Vec<f64> = ts.windows(2).map(|w| (w[1] - w[0]) as f64).collect();
    let n = deltas.len() as f64;
    let mean = deltas.iter().sum::<f64>() / n;
    if mean <= 0.0 {
        return BeaconVerdict::not_beacon(samples);
    }
    let variance = deltas.iter().map(|d| (d - mean).powi(2)).sum::<f64>() / n;
    let jitter_ratio = variance.sqrt() / mean;
    let is_beacon = mean >= t.min_interval_secs as f64
        && mean <= t.max_interval_secs as f64
        && jitter_ratio <= t.max_jitter_ratio;
    BeaconVerdict {
        is_beacon,
        period_secs: mean,
        jitter_ratio,
        samples,
    }
}

// ---------------------------------------------------------------------------
// Guard — rides EDITH's quiet-hours + cooldown + debounce
// ---------------------------------------------------------------------------

/// The suppression policy for egress alerts. `quiet_start`/`quiet_end` are the
/// SAME `[proactive]` band EDITH uses (fed in at the live edge); the cooldown /
/// min-gap come from `[egress]`. Pure data so [`guard_alert`] is testable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AlertGuardPolicy {
    pub quiet_start: u8,
    pub quiet_end: u8,
    /// Don't repeat the SAME alert key until this many seconds pass.
    pub cooldown_secs: u64,
    /// Never two egress alerts (any key) closer than this — the debounce.
    pub min_gap_secs: u64,
}

/// The gate decision for one candidate alert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlertGate {
    /// Emit the alert.
    Allow,
    /// Suppressed by a guard; the `&str` names which one (for logging).
    Suppressed(&'static str),
}

/// Per-key cooldown + global debounce ledger — the same shape as EDITH's
/// `anticipate::FiredState`, carried by the live loop across ticks.
#[derive(Debug, Clone, Default)]
pub struct AlertLedger {
    /// (alert key, unix secs it last fired) — the per-key cooldown ledger.
    last_fired: Vec<(String, u64)>,
    /// The most recent alert time, for the global min-gap debounce.
    most_recent: Option<u64>,
}

impl AlertLedger {
    fn last_fired_at(&self, key: &str) -> Option<u64> {
        self.last_fired
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, t)| *t)
    }

    /// Record that `key` fired at `now`: stamp the cooldown ledger and advance
    /// the global debounce clock. Called by the live loop only when it ACTS on
    /// an `Allow`.
    pub fn record(&mut self, key: &str, now: u64) {
        match self.last_fired.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = now,
            None => self.last_fired.push((key.to_string(), now)),
        }
        self.most_recent = Some(now);
    }
}

/// THE guard. Deterministic and pure: an egress finding survives only if it is
/// (1) outside EDITH's quiet-hours band, (2) past this key's cooldown, and
/// (3) past the global debounce gap. The order mirrors `anticipate::evaluate`.
pub fn guard_alert(
    key: &str,
    local_hour: u8,
    now: u64,
    ledger: &AlertLedger,
    policy: &AlertGuardPolicy,
) -> AlertGate {
    // 1. Quiet hours — reuse EDITH's exact band predicate.
    if crate::anticipate::in_quiet_hours(local_hour, policy.quiet_start, policy.quiet_end) {
        return AlertGate::Suppressed("quiet_hours");
    }
    // 2. Per-key cooldown — don't renag on the same talker.
    if let Some(last) = ledger.last_fired_at(key) {
        if now.saturating_sub(last) < policy.cooldown_secs {
            return AlertGate::Suppressed("cooldown");
        }
    }
    // 3. Global debounce — never two egress alerts closer than the min gap.
    if let Some(last) = ledger.most_recent {
        if now.saturating_sub(last) < policy.min_gap_secs {
            return AlertGate::Suppressed("debounce");
        }
    }
    AlertGate::Allow
}

// ---------------------------------------------------------------------------
// Propose-only firewall rule rendering (PURE — never applied)
// ---------------------------------------------------------------------------

/// PURE: render a pf/pfctl block rule as TEXT for the operator to review and
/// apply THEMSELVES with `sudo`. This module NEVER runs `pfctl` and NEVER
/// mutates the firewall — the returned string is advisory only, carrying the
/// exact command and its undo so the human stays in control.
pub fn render_block_proposal(process: &str, host: &str, port: u16, reason: &str) -> String {
    format!(
        "# DARWIN egress proposal — PROPOSE-ONLY. DARWIN never applies this; you do.\n\
         # Reason: {reason}\n\
         # Talker: process '{process}' -> {host}:{port}\n\
         #\n\
         # Review, then apply yourself (requires sudo; pf must be enabled):\n\
         #   echo \"block drop out quick proto tcp from any to {host} port {port}\" | sudo pfctl -a darwin_egress -f -\n\
         #   sudo pfctl -e   # only if pf is not already enabled\n\
         # Undo:\n\
         #   sudo pfctl -a darwin_egress -F rules"
    )
}

// ---------------------------------------------------------------------------
// Telemetry frame builders (PURE)
// ---------------------------------------------------------------------------

/// The `egress.newhost` telemetry payload for a first-seen talker.
pub fn newhost_frame(o: &Observation, proposal: &str) -> Value {
    json!({
        "process": o.process,
        "host": o.host,
        "port": o.port,
        "first_seen_ts": o.ts,
        "proposal": proposal,
        "caveat": UID_CAVEAT,
    })
}

/// The `egress.beacon` telemetry payload for a suspected beacon talker.
pub fn beacon_frame(o: &Observation, v: &BeaconVerdict, proposal: &str) -> Value {
    json!({
        "process": o.process,
        "host": o.host,
        "port": o.port,
        // Rounded so the HUD renders a clean cadence, not float noise.
        "period_secs": (v.period_secs * 10.0).round() / 10.0,
        "jitter_ratio": (v.jitter_ratio * 1000.0).round() / 1000.0,
        "samples": v.samples,
        "proposal": proposal,
        "caveat": UID_CAVEAT,
    })
}

// ---------------------------------------------------------------------------
// Live loop (runtime-only — the pure pieces above are what the tests cover)
// ---------------------------------------------------------------------------

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The live baseline+beacon loop. Runtime-only (never run in tests): every
/// `sample_interval_secs` it samples the read-only egress snapshot, folds it into
/// the longitudinal store, runs the two PURE classifiers, and emits a guarded,
/// propose-only `egress.newhost` / `egress.beacon` frame for any survivor. It
/// changes nothing on the host.
pub async fn run_task(cfg: std::sync::Arc<crate::config::Config>) {
    let ec = &cfg.egress;
    tokio::time::sleep(Duration::from_secs(ec.startup_delay_secs)).await;
    let interval = Duration::from_secs(ec.sample_interval_secs.max(1));
    let thresholds = BeaconThresholds::from_config(ec);
    let guard_policy = AlertGuardPolicy {
        // Ride EDITH's configured quiet-hours band verbatim.
        quiet_start: cfg.proactive.quiet_start,
        quiet_end: cfg.proactive.quiet_end,
        cooldown_secs: ec.alert_cooldown_secs,
        min_gap_secs: ec.alert_min_gap_secs,
    };
    let mut store = BaselineStore::new(RetentionPolicy::from_config(ec));
    let mut ledger = AlertLedger::default();

    loop {
        tokio::time::sleep(interval).await;
        let now = now_secs();
        let local_hour = chrono::Local::now().hour() as u8;

        let obs: Vec<Observation> = crate::egress::sample_talkers()
            .await
            .into_iter()
            .map(|(process, remote)| {
                let (host, port) = split_host_port(&remote);
                Observation {
                    process,
                    host,
                    port,
                    ts: now,
                }
            })
            .collect();

        // New-host diff BEFORE ingest, so a brand-new pair is flagged exactly on
        // the tick it first appears (and never again — it is in the baseline after).
        let known = store.known_host_pairs();
        let new_hosts = diff_new_hosts(&obs, &known);
        store.ingest_sample(&obs, now);

        for o in &new_hosts {
            let key = format!("newhost:{}:{}", o.process, o.host);
            match guard_alert(&key, local_hour, now, &ledger, &guard_policy) {
                AlertGate::Allow => {
                    let proposal =
                        render_block_proposal(&o.process, &o.host, o.port, "first-seen outbound talker");
                    info!(process = %o.process, host = %o.host, "egress: new outbound talker");
                    crate::telemetry::emit("egress", "egress.newhost", newhost_frame(o, &proposal));
                    ledger.record(&key, now);
                }
                AlertGate::Suppressed(reason) => {
                    debug!(process = %o.process, host = %o.host, reason, "egress: new-host alert suppressed");
                }
            }
        }

        // Beacon cadence over each distinct (process, host) seen this tick.
        let mut checked: HashSet<(String, String)> = HashSet::new();
        for o in &obs {
            if !checked.insert((o.process.clone(), o.host.clone())) {
                continue;
            }
            let series = store.edge_timestamps(&o.process, &o.host);
            let verdict = classify_cadence(&series, &thresholds);
            if !verdict.is_beacon {
                continue;
            }
            let key = format!("beacon:{}:{}", o.process, o.host);
            match guard_alert(&key, local_hour, now, &ledger, &guard_policy) {
                AlertGate::Allow => {
                    let reason = format!("regular ~{:.0}s callback cadence", verdict.period_secs);
                    let proposal = render_block_proposal(&o.process, &o.host, o.port, &reason);
                    info!(process = %o.process, host = %o.host, period = verdict.period_secs, "egress: suspected beacon");
                    crate::telemetry::emit("egress", "egress.beacon", beacon_frame(o, &verdict, &proposal));
                    ledger.record(&key, now);
                }
                AlertGate::Suppressed(reason) => {
                    debug!(process = %o.process, host = %o.host, reason, "egress: beacon alert suppressed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thresholds() -> BeaconThresholds {
        BeaconThresholds {
            min_samples: 6,
            min_interval_secs: 30,
            max_interval_secs: 3600,
            max_jitter_ratio: 0.15,
        }
    }

    fn retention() -> RetentionPolicy {
        RetentionPolicy {
            max_talkers: 4,
            max_samples_per_talker: 8,
            retention_secs: 86_400,
        }
    }

    fn obs(process: &str, host: &str, port: u16, ts: u64) -> Observation {
        Observation {
            process: process.to_string(),
            host: host.to_string(),
            port,
            ts,
        }
    }

    // ---- split_host_port ----

    #[test]
    fn split_host_port_handles_ipv4_ipv6_and_bare() {
        assert_eq!(split_host_port("93.184.216.34:443"), ("93.184.216.34".into(), 443));
        assert_eq!(split_host_port("[2001:db8::1]:8443"), ("2001:db8::1".into(), 8443));
        assert_eq!(split_host_port("example.com:80"), ("example.com".into(), 80));
        // Unparseable / missing port degrades to 0, never panics.
        assert_eq!(split_host_port("host-only"), ("host-only".into(), 0));
        assert_eq!(split_host_port("1.2.3.4:notaport"), ("1.2.3.4".into(), 0));
    }

    // ---- Classifier 1: new-host baseline diff ----

    #[test]
    fn new_host_diff_flags_only_unknown_pairs_once() {
        let known: HashSet<(String, String)> =
            [("curl".to_string(), "1.1.1.1".to_string())].into_iter().collect();
        let current = vec![
            obs("curl", "1.1.1.1", 443, 100),   // known -> not flagged
            obs("evil", "9.9.9.9", 443, 100),   // new
            obs("evil", "9.9.9.9", 8443, 100),  // same new pair, different port -> deduped
            obs("evil", "8.8.8.8", 53, 100),    // another new pair
        ];
        let new = diff_new_hosts(&current, &known);
        assert_eq!(new.len(), 2, "one finding per new (process,host) pair");
        assert!(new.iter().any(|o| o.host == "9.9.9.9" && o.port == 443));
        assert!(new.iter().any(|o| o.host == "8.8.8.8"));
        assert!(!new.iter().any(|o| o.host == "1.1.1.1"), "known pair never flagged");
    }

    #[test]
    fn new_host_diff_empty_when_all_known() {
        let known: HashSet<(String, String)> =
            [("a".to_string(), "h".to_string())].into_iter().collect();
        assert!(diff_new_hosts(&[obs("a", "h", 1, 0)], &known).is_empty());
    }

    // ---- Classifier 2: beacon cadence ----

    #[test]
    fn cadence_flags_a_regular_beacon() {
        let t = thresholds();
        // Perfectly periodic 60s callbacks.
        let series = [0, 60, 120, 180, 240, 300];
        let v = classify_cadence(&series, &t);
        assert!(v.is_beacon, "regular cadence must be flagged: {v:?}");
        assert!((v.period_secs - 60.0).abs() < 1e-9);
        assert!(v.jitter_ratio < 1e-9, "zero jitter for a perfect beacon");
    }

    #[test]
    fn cadence_flags_a_slightly_jittered_beacon() {
        let t = thresholds();
        // ~60s with small real-world jitter -> still under the CV ceiling.
        let series = [0, 58, 121, 179, 241, 300];
        let v = classify_cadence(&series, &t);
        assert!(v.is_beacon, "small jitter still a beacon: {v:?}");
        assert!(v.jitter_ratio <= t.max_jitter_ratio);
    }

    #[test]
    fn cadence_rejects_bursty_traffic() {
        let t = thresholds();
        // A burst then a long gap then a burst -> high variance -> not a beacon.
        let series = [0, 1, 2, 3, 600, 601, 602];
        let v = classify_cadence(&series, &t);
        assert!(!v.is_beacon, "bursty traffic must not be flagged: {v:?}");
        assert!(v.jitter_ratio > t.max_jitter_ratio);
    }

    #[test]
    fn cadence_rejects_random_traffic() {
        let t = thresholds();
        let series = [0, 50, 300, 340, 900, 1500];
        let v = classify_cadence(&series, &t);
        assert!(!v.is_beacon, "irregular traffic must not be flagged: {v:?}");
    }

    #[test]
    fn cadence_needs_enough_samples() {
        let t = thresholds();
        // Perfectly regular but too few samples to trust.
        let v = classify_cadence(&[0, 60, 120], &t);
        assert!(!v.is_beacon, "too few samples is undecidable, not a beacon");
        assert_eq!(v.samples, 3);
    }

    #[test]
    fn cadence_is_panic_free_on_degenerate_input() {
        let t = thresholds();
        assert!(!classify_cadence(&[], &t).is_beacon);
        // All-identical timestamps -> mean delta 0 -> undecidable, no NaN panic.
        assert!(!classify_cadence(&[5, 5, 5, 5, 5, 5], &t).is_beacon);
    }

    #[test]
    fn cadence_rejects_a_period_outside_the_band() {
        let t = thresholds();
        // Regular but sub-min-interval (every 5s) -> bursty reconnection noise.
        let fast = [0, 5, 10, 15, 20, 25];
        assert!(!classify_cadence(&fast, &t).is_beacon, "sub-min-interval is not a beacon");
    }

    // ---- Longitudinal store: rising edges + retention ----

    #[test]
    fn store_records_one_edge_for_a_persistent_talker() {
        // A LONG-LIVED connection present in every sample must NOT masquerade as
        // a sample-interval beacon: it produces a single rising edge.
        let mut store = BaselineStore::new(retention());
        for tick in 0..6u64 {
            store.ingest_sample(&[obs("vpn", "10.0.0.1", 443, tick * 60)], tick * 60);
        }
        let series = store.edge_timestamps("vpn", "10.0.0.1");
        assert_eq!(series, vec![0], "persistent connection = a single rising edge");
        assert!(!classify_cadence(&series, &thresholds()).is_beacon);
    }

    #[test]
    fn store_accumulates_a_regular_series_for_a_reappearing_beacon() {
        // A short-lived beacon: present on even ticks, gone on odd ticks. Each
        // reappearance is a rising edge -> a regular 120s series.
        let mut store = BaselineStore::new(retention());
        for tick in 0..12u64 {
            let now = tick * 60;
            let sample = if tick % 2 == 0 {
                vec![obs("implant", "203.0.113.7", 443, now)]
            } else {
                vec![]
            };
            store.ingest_sample(&sample, now);
        }
        let series = store.edge_timestamps("implant", "203.0.113.7");
        assert_eq!(series, vec![0, 120, 240, 360, 480, 600], "one edge per reappearance");
        assert!(classify_cadence(&series, &thresholds()).is_beacon);
    }

    #[test]
    fn store_bounds_edges_per_talker() {
        let mut store = BaselineStore::new(retention()); // ring cap 8
        for tick in 0..40u64 {
            let now = tick * 100;
            // present only on even ticks so every reappearance is an edge
            let sample = if tick % 2 == 0 {
                vec![obs("p", "h", 1, now)]
            } else {
                vec![]
            };
            store.ingest_sample(&sample, now);
        }
        assert!(
            store.edge_timestamps("p", "h").len() <= 8,
            "per-talker edge ring stays bounded"
        );
    }

    #[test]
    fn store_bounds_distinct_talkers_by_lru() {
        let mut store = BaselineStore::new(retention()); // max_talkers 4
        // Six distinct hosts, each a one-shot edge at increasing times.
        for i in 0..6u64 {
            let host = format!("h{i}");
            store.ingest_sample(&[obs("p", &host, 1, i * 10)], i * 10);
        }
        assert!(store.talkers.len() <= 4, "distinct-talker cap enforced");
        // The most recent hosts survive; the oldest were evicted.
        let known = store.known_host_pairs();
        assert!(known.contains(&("p".to_string(), "h5".to_string())));
        assert!(!known.contains(&("p".to_string(), "h0".to_string())));
    }

    #[test]
    fn store_prunes_stale_talkers() {
        let policy = RetentionPolicy {
            max_talkers: 8,
            max_samples_per_talker: 8,
            retention_secs: 100,
        };
        let mut store = BaselineStore::new(policy);
        store.ingest_sample(&[obs("p", "old", 1, 0)], 0);
        // A much later sample with a different talker prunes the stale one.
        store.ingest_sample(&[obs("p", "new", 1, 1000)], 1000);
        let known = store.known_host_pairs();
        assert!(!known.contains(&("p".to_string(), "old".to_string())), "stale talker pruned");
        assert!(known.contains(&("p".to_string(), "new".to_string())));
    }

    // ---- Guard: rides EDITH quiet-hours + cooldown + debounce ----

    fn guard_policy() -> AlertGuardPolicy {
        AlertGuardPolicy {
            quiet_start: 22,
            quiet_end: 7,
            cooldown_secs: 3600,
            min_gap_secs: 300,
        }
    }

    #[test]
    fn guard_allows_a_fresh_alert_outside_quiet_hours() {
        let ledger = AlertLedger::default();
        assert_eq!(
            guard_alert("beacon:p:h", 12, 1000, &ledger, &guard_policy()),
            AlertGate::Allow
        );
    }

    #[test]
    fn guard_suppresses_inside_quiet_hours() {
        let ledger = AlertLedger::default();
        // 02:00 local is inside the 22..7 band EDITH configured.
        assert_eq!(
            guard_alert("beacon:p:h", 2, 1000, &ledger, &guard_policy()),
            AlertGate::Suppressed("quiet_hours")
        );
    }

    #[test]
    fn guard_enforces_per_key_cooldown() {
        let mut ledger = AlertLedger::default();
        ledger.record("beacon:p:h", 1000);
        // Same key, 10 min later, cooldown is 60 min -> suppressed.
        assert_eq!(
            guard_alert("beacon:p:h", 12, 1600, &ledger, &guard_policy()),
            AlertGate::Suppressed("cooldown")
        );
        // A DIFFERENT key is NOT gagged by the first key's cooldown: at 1600 the
        // 300s debounce has also elapsed (600s > 300s), so it is allowed — proving
        // the cooldown is per-key, not global.
        assert_eq!(
            guard_alert("beacon:p:other", 12, 1600, &ledger, &guard_policy()),
            AlertGate::Allow
        );
    }

    #[test]
    fn guard_enforces_global_debounce_then_allows_after_the_gap() {
        let mut ledger = AlertLedger::default();
        ledger.record("newhost:p:a", 1000);
        // A different key 100s later -> within the 300s min-gap -> debounced.
        assert_eq!(
            guard_alert("newhost:p:b", 12, 1100, &ledger, &guard_policy()),
            AlertGate::Suppressed("debounce")
        );
        // Past the gap AND past that key's (nonexistent) cooldown -> allowed.
        assert_eq!(
            guard_alert("newhost:p:b", 12, 1400, &ledger, &guard_policy()),
            AlertGate::Allow
        );
    }

    // ---- Propose-only rule rendering ----

    #[test]
    fn proposal_is_propose_only_text_carrying_host_and_port() {
        let text = render_block_proposal("evil", "198.51.100.9", 443, "regular ~60s callback cadence");
        assert!(text.contains("198.51.100.9"), "carries the host");
        assert!(text.contains("port 443"), "carries the port");
        assert!(text.contains("PROPOSE-ONLY"), "labelled propose-only");
        assert!(text.contains("DARWIN never applies this"), "states DARWIN never applies it");
        assert!(text.contains("block drop out quick proto tcp"), "renders a pf block rule");
        assert!(text.contains("sudo pfctl"), "the user applies it themselves with sudo");
        assert!(text.contains("Undo:"), "carries the undo command");
        assert!(text.contains("regular ~60s callback cadence"), "carries the reason");
    }

    // ---- Telemetry frames ----

    #[test]
    fn frames_carry_the_fields_and_the_uid_caveat() {
        let o = obs("implant", "203.0.113.7", 443, 4242);
        let nf = newhost_frame(&o, "PROPOSAL");
        assert_eq!(nf["process"], "implant");
        assert_eq!(nf["host"], "203.0.113.7");
        assert_eq!(nf["port"], 443);
        assert_eq!(nf["first_seen_ts"], 4242);
        assert_eq!(nf["proposal"], "PROPOSAL");
        assert_eq!(nf["caveat"], UID_CAVEAT);

        let v = BeaconVerdict {
            is_beacon: true,
            period_secs: 60.04,
            jitter_ratio: 0.0123,
            samples: 6,
        };
        let bf = beacon_frame(&o, &v, "PROPOSAL");
        assert_eq!(bf["samples"], 6);
        assert_eq!(bf["period_secs"], 60.0, "period rounded to one decimal");
        assert_eq!(bf["jitter_ratio"], 0.012, "jitter rounded to three decimals");
        assert_eq!(bf["caveat"], UID_CAVEAT);
    }
}

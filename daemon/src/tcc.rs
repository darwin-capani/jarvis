//! TCC Permission Sentinel — a READ-ONLY inventory of which apps hold macOS
//! privacy grants (Microphone, Camera, Screen Recording, Accessibility, Input
//! Monitoring, Full Disk Access, Contacts, Calendar, …), a pass that FLAGS the
//! high-risk grants (the watch-your-screen / log-your-keystrokes / read-all-files
//! vectors) that are currently ALLOWED, and a PURE baseline diff for the
//! longitudinal sentinel (a documented follow-on).
//!
//! This is the LOCAL-PERMISSION vector — orthogonal to Egress Sentinel (network,
//! `egress.rs`) and posture (system state). It CHANGES NOTHING: it opens the TCC
//! store READ-ONLY (`OpenFlags::SQLITE_OPEN_READ_ONLY`), never mutates a grant,
//! never blocks an app, never parks — so it is NOT in `CONSEQUENTIAL_TOOLS`.
//!
//! HONESTY: modern macOS protects both TCC databases behind Full Disk Access, so
//! when the store is unreadable this says exactly that and NEVER fabricates an
//! inventory (same discipline as egress.rs / posture.rs). Any future "revoke /
//! block" capability is a SEPARATE gated increment — v1 observes and reports only.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::json;
use tokio::sync::Mutex;

/// The system TCC store (all-user, SIP-protected: needs Full Disk Access).
const SYSTEM_TCC: &str = "/Library/Application Support/com.apple.TCC/TCC.db";
/// The per-user TCC store, relative to `$HOME`.
const USER_TCC_REL: &str = "Library/Application Support/com.apple.TCC/TCC.db";

/// The grants that let an app watch your screen, log/synthesize your keystrokes,
/// control your Mac, or read every file — the classic stalkerware/malware vector.
/// An app holding one of these with an ALLOWED decision is what we surface loudly.
const HIGH_RISK_SERVICES: &[&str] = &[
    "kTCCServiceAccessibility",       // control the Mac (drive any app)
    "kTCCServiceScreenCapture",       // record the screen
    "kTCCServiceListenEvent",         // input monitoring (keylogging)
    "kTCCServicePostEvent",           // synthesize input
    "kTCCServiceSystemPolicyAllFiles", // Full Disk Access
];

/// One privacy grant parsed from the TCC `access` table.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Grant {
    /// Bundle id (e.g. `com.apple.Safari`) or an absolute binary path.
    client: String,
    /// The raw `kTCCService…` identifier.
    service: String,
    /// Friendly service label (e.g. "Screen Recording").
    kind: String,
    /// "allowed" / "denied" / "limited" / "unknown(N)".
    decision: String,
    /// A grant on one of `HIGH_RISK_SERVICES`.
    high_risk: bool,
    /// Unix seconds of the last grant change (0 when absent).
    last_modified: i64,
}

/// Inventory the host's app privacy grants and render them, flagging the
/// high-risk ones that are currently allowed. READ-ONLY. Degrades honestly to a
/// "grant Full Disk Access" message when no TCC store is readable — never a
/// fabricated inventory, never a panic.
pub async fn snapshot() -> Result<String> {
    let collected = tokio::task::spawn_blocking(collect_inventory)
        .await
        .map_err(|e| anyhow!("tcc: inventory task failed to run: {e}"))?;
    Ok(match collected {
        Ok(grants) => format_inventory(&grants),
        Err(reason) => unavailable_message(&reason),
    })
}

/// The TCC stores to try, most-accessible first. The user store is skipped when
/// `$HOME` is unset.
fn tcc_db_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        paths.push(Path::new(&home).join(USER_TCC_REL));
    }
    paths.push(PathBuf::from(SYSTEM_TCC));
    paths
}

/// Read every readable TCC store and union the grants. `Ok` when AT LEAST ONE
/// store opened (possibly empty); `Err(reason)` when NONE could be read (the
/// honest "needs Full Disk Access" case). Sync (rusqlite) — driven under
/// `spawn_blocking`.
fn collect_inventory() -> std::result::Result<Vec<Grant>, String> {
    let mut grants = Vec::new();
    let mut any_readable = false;
    let mut errors = Vec::new();
    for path in tcc_db_paths() {
        match read_tcc_db(&path) {
            Ok(mut g) => {
                any_readable = true;
                grants.append(&mut g);
            }
            Err(e) => errors.push(format!("{}: {e}", path.display())),
        }
    }
    if any_readable {
        dedupe(&mut grants);
        Ok(grants)
    } else {
        Err(errors.join("; "))
    }
}

/// Open ONE TCC store read-only and read its `access` table. Tries the modern
/// schema (`auth_value`) then the legacy one (`allowed`), so it works across
/// macOS versions. Any failure (missing, permission-denied, locked) is an `Err`.
fn read_tcc_db(path: &Path) -> Result<Vec<Grant>> {
    if !path.exists() {
        return Err(anyhow!("not present"));
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| anyhow!("read-only open failed ({e})"))?;
    match query_access(&conn, true) {
        Ok(g) => Ok(g),
        // Older macOS uses `allowed` (0/1) instead of `auth_value`.
        Err(_) => query_access(&conn, false),
    }
}

/// Query the `access` table in either the modern (`auth_value`) or legacy
/// (`allowed`) shape and map each row to a `Grant`.
fn query_access(conn: &Connection, modern: bool) -> Result<Vec<Grant>> {
    let sql = if modern {
        "SELECT service, client, auth_value, last_modified FROM access"
    } else {
        "SELECT service, client, allowed, last_modified FROM access"
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |r| {
        let service: String = r.get(0)?;
        let client: String = r.get(1)?;
        let auth: i64 = r.get(2).unwrap_or_default();
        let last_modified: i64 = r.get(3).unwrap_or_default();
        Ok((service, client, auth, last_modified))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (service, client, auth, last_modified) = row?;
        let decision = if modern {
            decision_from_auth_value(auth)
        } else {
            decision_from_legacy_allowed(auth)
        };
        out.push(grant(&service, &client, decision, last_modified));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Pure mapping / rendering (unit-tested; no I/O)
// ---------------------------------------------------------------------------

/// Build a `Grant`, deriving the friendly kind and the high-risk flag.
fn grant(service: &str, client: &str, decision: String, last_modified: i64) -> Grant {
    Grant {
        client: client.to_string(),
        kind: friendly_service(service),
        high_risk: is_high_risk(service),
        service: service.to_string(),
        decision,
        last_modified,
    }
}

/// Map a `kTCCService…` id to a human label. An UNKNOWN service is shown with
/// its `kTCCService` prefix stripped (honest — we never hide a grant we can't
/// name).
fn friendly_service(service: &str) -> String {
    let friendly = match service {
        "kTCCServiceMicrophone" => "Microphone",
        "kTCCServiceCamera" => "Camera",
        "kTCCServiceScreenCapture" => "Screen Recording",
        "kTCCServiceAccessibility" => "Accessibility",
        "kTCCServiceListenEvent" => "Input Monitoring",
        "kTCCServicePostEvent" => "Input Synthesis",
        "kTCCServiceSystemPolicyAllFiles" => "Full Disk Access",
        "kTCCServiceSystemPolicyDesktopFolder" => "Desktop Folder",
        "kTCCServiceSystemPolicyDocumentsFolder" => "Documents Folder",
        "kTCCServiceSystemPolicyDownloadsFolder" => "Downloads Folder",
        "kTCCServiceAddressBook" => "Contacts",
        "kTCCServiceCalendar" => "Calendar",
        "kTCCServiceReminders" => "Reminders",
        "kTCCServicePhotos" => "Photos",
        "kTCCServiceMediaLibrary" => "Media Library",
        "kTCCServiceLocation" => "Location",
        "kTCCServiceAppleEvents" => "Automation (Apple Events)",
        other => return other.strip_prefix("kTCCService").unwrap_or(other).to_string(),
    };
    friendly.to_string()
}

/// Whether a service is one of the high-risk (control/observe/exfiltrate) grants.
fn is_high_risk(service: &str) -> bool {
    HIGH_RISK_SERVICES.contains(&service)
}

/// Modern TCC `auth_value`: 0 denied, 2 allowed, 3 limited; else unknown(N).
fn decision_from_auth_value(v: i64) -> String {
    match v {
        0 => "denied".to_string(),
        2 => "allowed".to_string(),
        3 => "limited".to_string(),
        other => format!("unknown({other})"),
    }
}

/// Legacy TCC `allowed`: 1 allowed, 0 denied; else unknown(N).
fn decision_from_legacy_allowed(v: i64) -> String {
    match v {
        1 => "allowed".to_string(),
        0 => "denied".to_string(),
        other => format!("unknown({other})"),
    }
}

/// Collapse duplicate (client, service) pairs (a grant present in both the user
/// and system store) in first-seen order.
fn dedupe(grants: &mut Vec<Grant>) {
    let mut seen = std::collections::HashSet::new();
    grants.retain(|g| seen.insert((g.client.clone(), g.service.clone())));
}

/// Render `last_modified` unix seconds as a `YYYY-MM-DD` date, or `-` when absent.
fn fmt_ts(secs: i64) -> String {
    if secs <= 0 {
        return "-".to_string();
    }
    match chrono::DateTime::from_timestamp(secs, 0) {
        Some(dt) => dt.format("%Y-%m-%d").to_string(),
        None => "-".to_string(),
    }
}

/// Render the inventory: a table of every grant, then a loud section listing the
/// high-risk grants that are currently ALLOWED, then a count footer. Pure.
fn format_inventory(grants: &[Grant]) -> String {
    if grants.is_empty() {
        return "No app privacy grants were found in the readable TCC store(s). (Read-only — I \
                changed nothing.)"
            .to_string();
    }
    let mut out = String::from("app | grant | decision | changed\n");
    for g in grants {
        out.push_str(&format!(
            "{} | {} | {} | {}\n",
            g.client,
            g.kind,
            g.decision,
            fmt_ts(g.last_modified)
        ));
    }
    let hot: Vec<&Grant> = grants
        .iter()
        .filter(|g| g.high_risk && g.decision == "allowed")
        .collect();
    if !hot.is_empty() {
        out.push_str(
            "\n⚠ high-risk grants currently ALLOWED (these let an app control your Mac, watch your \
             screen, log keystrokes, or read every file — confirm you trust each):\n",
        );
        for g in &hot {
            out.push_str(&format!("  {} → {}\n", g.client, g.kind));
        }
    }
    out.push_str(&format!(
        "\n({} grant{} inventoried; {} high-risk allowed. Read-only — I changed nothing.)",
        grants.len(),
        if grants.len() == 1 { "" } else { "s" },
        hot.len(),
    ));
    out
}

/// The honest "cannot read the TCC store" message (the Full Disk Access case).
/// NEVER a fabricated inventory.
fn unavailable_message(reason: &str) -> String {
    format!(
        "I can't read the macOS privacy database (TCC) right now, so I won't guess which apps hold \
         which grants. On modern macOS this store is protected — grant DARWIN Full Disk Access in \
         System Settings › Privacy & Security › Full Disk Access to let me inventory it (read-only; \
         I never change a permission). [{reason}]"
    )
}

// ---------------------------------------------------------------------------
// Ambient sentinel — a durable baseline + a periodic scan that flags NEW grants
// and denied→allowed escalations. The baseline diff is PURE (unit-tested); the
// store is exercised via an in-memory DB; the periodic loop + the live TCC read
// are runtime-only (inspection-verified, like audit_snapshot_task / egress run).
// ---------------------------------------------------------------------------

/// Generous startup delay (keep housekeeping out of the first exchanges) + a slow
/// tick (the permission surface moves on the order of app installs, not seconds).
const SENTINEL_STARTUP_DELAY: Duration = Duration::from_secs(30);
const SENTINEL_INTERVAL: Duration = Duration::from_secs(300);

/// A previously-recorded grant: (client, service, decision).
type BaselineRow = (String, String, String);

/// The durable TCC baseline (`state/tcc_baseline.db`). Its OWN dedicated SQLite
/// file, plaintext or SQLCipher-encrypted exactly like `audit.db` (open /
/// open_encrypted). An async Mutex serializes access (mirrors AuditLog). The
/// baseline stores app grants DARWIN has already seen so a later scan can flag
/// what is NEW — it is not sensitive, but it follows the same at-rest discipline.
pub struct TccBaseline {
    conn: Mutex<Connection>,
}

impl TccBaseline {
    /// Open (or create) the baseline DB PLAINTEXT (the default, when
    /// `[security].encrypt_memory` is off).
    pub fn open(path: &Path) -> Result<Self> {
        Self::init_conn(Connection::open(path)?)
    }

    /// Open (or create) the baseline DB ENCRYPTED (SQLCipher). `key` is applied
    /// via `PRAGMA key` before any other statement — same seam as AuditLog.
    pub fn open_encrypted(path: &Path, key: &crate::crypto::SecretKey) -> Result<Self> {
        let conn = Connection::open(path)?;
        crate::crypto::apply_key(&conn, key)?;
        Self::init_conn(conn)
    }

    /// Shared pragmas + schema, run AFTER any `PRAGMA key`.
    fn init_conn(conn: Connection) -> Result<Self> {
        conn.busy_timeout(Duration::from_millis(250))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS tcc_baseline(
                client TEXT NOT NULL,
                service TEXT NOT NULL,
                decision TEXT NOT NULL,
                first_seen INTEGER NOT NULL,
                last_seen INTEGER NOT NULL,
                PRIMARY KEY(client, service)
            );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// In-memory baseline for tests (no disk). Same schema.
    #[cfg(test)]
    fn in_memory() -> Result<Self> {
        Self::init_conn(Connection::open_in_memory()?)
    }

    /// True when no grant has ever been recorded (drives the silent cold-start
    /// seed, so a first run / fresh install does not flood the user with "new
    /// grant" alerts for every pre-existing permission).
    async fn is_empty(&self) -> Result<bool> {
        let conn = self.conn.lock().await;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM tcc_baseline", [], |r| r.get(0))?;
        Ok(n == 0)
    }

    /// Load the recorded (client, service, decision) rows.
    async fn load(&self) -> Result<Vec<BaselineRow>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare("SELECT client, service, decision FROM tcc_baseline")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Record/refresh the current grants (INSERT new, UPDATE decision + last_seen
    /// on an existing (client, service)). Call AFTER diffing against the prior
    /// baseline, since it overwrites the stored decision.
    async fn upsert(&self, grants: &[Grant], now: i64) -> Result<()> {
        let conn = self.conn.lock().await;
        for g in grants {
            conn.execute(
                "INSERT INTO tcc_baseline(client, service, decision, first_seen, last_seen)
                 VALUES(?1, ?2, ?3, ?4, ?4)
                 ON CONFLICT(client, service) DO UPDATE SET decision = ?3, last_seen = ?4",
                rusqlite::params![g.client, g.service, g.decision, now],
            )?;
        }
        Ok(())
    }
}

/// Compare a fresh inventory against a recorded baseline and return
/// human-readable anomaly lines: a grant never seen before, and a
/// denied→allowed escalation on an existing one. Pure — the detection core.
fn baseline_diff(baseline: &[BaselineRow], live: &[Grant]) -> Vec<String> {
    let mut anomalies = Vec::new();
    for g in live {
        let prior = baseline
            .iter()
            .find(|(c, s, _)| c == &g.client && s == &g.service);
        match prior {
            None => {
                let risk = if g.high_risk { " [HIGH-RISK]" } else { "" };
                anomalies.push(format!("NEW grant: {} → {} ({}){risk}", g.client, g.kind, g.decision));
            }
            Some((_, _, prior_decision)) => {
                if prior_decision == "denied" && g.decision == "allowed" {
                    let risk = if g.high_risk { " [HIGH-RISK]" } else { "" };
                    anomalies.push(format!(
                        "ESCALATION: {} → {} went denied → allowed{risk}",
                        g.client, g.kind
                    ));
                }
            }
        }
    }
    anomalies
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// One sentinel tick: inventory the grants, emit an ambient `tcc.snapshot` status
/// for the HUD, and — once past the silent cold-start seed — diff against the
/// baseline and emit `tcc.anomaly` for any new grant / escalation. READ-ONLY over
/// TCC; only the daemon's own baseline store is written. Runtime-only (the live
/// TCC read makes this inspection-verified; its store + diff cores are tested).
pub async fn sentinel_tick(store: &TccBaseline) {
    let collected = match tokio::task::spawn_blocking(collect_inventory).await {
        Ok(c) => c,
        Err(_) => return,
    };
    let grants = match collected {
        Ok(g) => g,
        Err(_) => {
            // TCC unreadable (needs Full Disk Access) — report honestly, do not
            // touch the baseline, do not fabricate anomalies.
            crate::telemetry::emit("system", "tcc.snapshot", json!({"available": false}));
            return;
        }
    };
    let high_risk_allowed = grants
        .iter()
        .filter(|g| g.high_risk && g.decision == "allowed")
        .count();
    crate::telemetry::emit(
        "system",
        "tcc.snapshot",
        json!({"available": true, "grants": grants.len(), "high_risk_allowed": high_risk_allowed}),
    );

    let now = now_secs();
    // Cold start: seed silently so a first run does not alert on every existing grant.
    match store.is_empty().await {
        Ok(true) => {
            let _ = store.upsert(&grants, now).await;
            return;
        }
        Ok(false) => {}
        Err(_) => return,
    }
    let baseline = match store.load().await {
        Ok(b) => b,
        Err(_) => return,
    };
    let anomalies = baseline_diff(&baseline, &grants);
    let _ = store.upsert(&grants, now).await;
    if !anomalies.is_empty() {
        crate::telemetry::emit("system", "tcc.anomaly", json!({"items": anomalies}));
    }
}

/// The ambient TCC sentinel loop (runtime-only; never run in tests). Mirrors
/// `audit_snapshot_task`: a startup delay, then a slow periodic `sentinel_tick`.
pub async fn sentinel_task(store: Arc<TccBaseline>) {
    tokio::time::sleep(SENTINEL_STARTUP_DELAY).await;
    loop {
        sentinel_tick(&store).await;
        tokio::time::sleep(SENTINEL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(service: &str, client: &str, decision: &str) -> Grant {
        grant(service, client, decision.to_string(), 0)
    }

    #[test]
    fn friendly_service_maps_known_and_leaks_unknown_honestly() {
        assert_eq!(friendly_service("kTCCServiceMicrophone"), "Microphone");
        assert_eq!(friendly_service("kTCCServiceScreenCapture"), "Screen Recording");
        assert_eq!(friendly_service("kTCCServiceAddressBook"), "Contacts");
        // Unknown service is shown with the prefix stripped, never hidden.
        assert_eq!(friendly_service("kTCCServiceFooBar"), "FooBar");
        assert_eq!(friendly_service("weird"), "weird");
    }

    #[test]
    fn high_risk_flags_the_dangerous_five_only() {
        assert!(is_high_risk("kTCCServiceAccessibility"));
        assert!(is_high_risk("kTCCServiceScreenCapture"));
        assert!(is_high_risk("kTCCServiceListenEvent"));
        assert!(is_high_risk("kTCCServicePostEvent"));
        assert!(is_high_risk("kTCCServiceSystemPolicyAllFiles"));
        // Sensitive but not in the loud set.
        assert!(!is_high_risk("kTCCServiceMicrophone"));
        assert!(!is_high_risk("kTCCServiceCamera"));
        assert!(!is_high_risk("kTCCServiceAddressBook"));
    }

    #[test]
    fn decision_mapping_modern_and_legacy() {
        assert_eq!(decision_from_auth_value(0), "denied");
        assert_eq!(decision_from_auth_value(2), "allowed");
        assert_eq!(decision_from_auth_value(3), "limited");
        assert_eq!(decision_from_auth_value(9), "unknown(9)");
        assert_eq!(decision_from_legacy_allowed(1), "allowed");
        assert_eq!(decision_from_legacy_allowed(0), "denied");
        assert_eq!(decision_from_legacy_allowed(7), "unknown(7)");
    }

    #[test]
    fn dedupe_collapses_same_client_and_service() {
        let mut grants = vec![
            g("kTCCServiceMicrophone", "com.zoom.xos", "allowed"),
            g("kTCCServiceMicrophone", "com.zoom.xos", "allowed"),
            g("kTCCServiceCamera", "com.zoom.xos", "allowed"),
        ];
        dedupe(&mut grants);
        assert_eq!(grants.len(), 2);
    }

    #[test]
    fn format_flags_only_allowed_high_risk() {
        let grants = vec![
            g("kTCCServiceMicrophone", "com.zoom.xos", "allowed"),
            g("kTCCServiceScreenCapture", "com.evil.spy", "allowed"),
            // A DENIED high-risk grant is NOT a live risk — must not be flagged.
            g("kTCCServiceAccessibility", "com.blocked.app", "denied"),
        ];
        let out = format_inventory(&grants);
        assert!(out.contains("com.zoom.xos | Microphone | allowed"));
        assert!(out.contains("⚠ high-risk grants currently ALLOWED"));
        assert!(out.contains("com.evil.spy → Screen Recording"), "got:\n{out}");
        assert!(!out.contains("com.blocked.app →"), "denied high-risk must not be flagged");
        assert!(out.contains("1 high-risk allowed"));
    }

    #[test]
    fn format_handles_empty() {
        let out = format_inventory(&[]);
        assert!(out.contains("No app privacy grants"));
        assert!(out.contains("changed nothing"));
    }

    #[test]
    fn unavailable_message_is_honest_and_actionable() {
        let m = unavailable_message("TCC.db: read-only open failed (permission denied)");
        assert!(m.contains("Full Disk Access"));
        assert!(m.contains("never change a permission"));
        assert!(!m.to_lowercase().contains("microphone"), "must not fabricate an inventory");
    }

    #[test]
    fn baseline_diff_flags_new_and_escalation_only() {
        let baseline = vec![
            ("com.zoom.xos".to_string(), "kTCCServiceMicrophone".to_string(), "allowed".to_string()),
            ("com.blocked.app".to_string(), "kTCCServiceAccessibility".to_string(), "denied".to_string()),
        ];
        let live = vec![
            // Unchanged — no anomaly.
            g("kTCCServiceMicrophone", "com.zoom.xos", "allowed"),
            // denied -> allowed on a high-risk grant — ESCALATION.
            g("kTCCServiceAccessibility", "com.blocked.app", "allowed"),
            // Never seen before — NEW.
            g("kTCCServiceScreenCapture", "com.new.tool", "allowed"),
        ];
        let anomalies = baseline_diff(&baseline, &live);
        assert_eq!(anomalies.len(), 2, "got: {anomalies:?}");
        assert!(anomalies.iter().any(|a| a.contains("ESCALATION")
            && a.contains("com.blocked.app")
            && a.contains("HIGH-RISK")));
        assert!(anomalies.iter().any(|a| a.contains("NEW grant")
            && a.contains("com.new.tool")
            && a.contains("HIGH-RISK")));
    }

    #[tokio::test]
    async fn baseline_store_round_trips_and_updates() {
        let store = TccBaseline::in_memory().unwrap();
        assert!(store.is_empty().await.unwrap());
        let first = vec![
            g("kTCCServiceMicrophone", "com.zoom.xos", "allowed"),
            g("kTCCServiceAccessibility", "com.blocked.app", "denied"),
        ];
        store.upsert(&first, 1000).await.unwrap();
        assert!(!store.is_empty().await.unwrap());
        let rows = store.load().await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.contains(&(
            "com.zoom.xos".to_string(),
            "kTCCServiceMicrophone".to_string(),
            "allowed".to_string()
        )));
        // Re-upsert with a changed decision updates in place (no duplicate row).
        store
            .upsert(&[g("kTCCServiceAccessibility", "com.blocked.app", "allowed")], 2000)
            .await
            .unwrap();
        let rows = store.load().await.unwrap();
        assert_eq!(rows.len(), 2, "same (client,service) must update, not duplicate");
        assert!(rows.contains(&(
            "com.blocked.app".to_string(),
            "kTCCServiceAccessibility".to_string(),
            "allowed".to_string()
        )));
    }

    #[tokio::test]
    async fn diff_against_seeded_store_flags_escalation() {
        let store = TccBaseline::in_memory().unwrap();
        store
            .upsert(&[g("kTCCServiceAccessibility", "com.blocked.app", "denied")], 1000)
            .await
            .unwrap();
        let baseline = store.load().await.unwrap();
        let live = vec![g("kTCCServiceAccessibility", "com.blocked.app", "allowed")];
        let anomalies = baseline_diff(&baseline, &live);
        assert_eq!(anomalies.len(), 1);
        assert!(anomalies[0].contains("ESCALATION"));
    }
}

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

use anyhow::{anyhow, Result};
use rusqlite::{Connection, OpenFlags};

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
         which grants. On modern macOS this store is protected — grant JARVIS Full Disk Access in \
         System Settings › Privacy & Security › Full Disk Access to let me inventory it (read-only; \
         I never change a permission). [{reason}]"
    )
}

// NOTE: the longitudinal baseline diff (new-grant / denied→allowed-escalation
// detection over time) ships with the ambient TCC sentinel increment — the
// periodic task + durable baseline store that actually exercise it — rather than
// as unused code here. v1 is the on-demand read-only inventory + high-risk flag.

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
}

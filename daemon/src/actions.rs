//! Benign macOS actuators — what lets DARWIN actually DO things.
//!
//! Safety boundary (hard contract): every action here is benign-only.
//! - NO shell passthrough: commands are spawned with explicit args via
//!   tokio::process::Command, never an interpreted shell string.
//! - NO delete/move/write of user files: actions open, search, and observe.
//! - NO keystroke synthesis.
//!   Every command is bounded by a 5s timeout with kill_on_drop so a wedged
//!   `open`/`mdfind`/`osascript` can never hang the event loop.
//!
//! All actions return Result<String>: Ok carries a human-readable outcome
//! string suitable as converse data (including "soft" outcomes like an
//! ambiguous app match); Err means the action itself could not execute.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, Context, Result};
use tokio::process::Command;
use tracing::warn;

use crate::integrations::ActionMode;
use crate::telemetry;

/// Hard ceiling per spawned action command.
const ACTION_TIMEOUT: Duration = Duration::from_secs(5);
/// Installed-app stem listing is cached this long.
const APP_CACHE_TTL: Duration = Duration::from_secs(60);
/// search_files never returns more than this many hits.
pub const SEARCH_LIMIT_MAX: usize = 8;
/// How many mdfind lines are even considered before the newest-first sort.
const MDFIND_SCAN_CAP: usize = 200;

// ---------------------------------------------------------------------------
// open_app
// ---------------------------------------------------------------------------

/// Outcome of fuzzy-matching a query against installed *.app stems.
#[derive(Debug, PartialEq)]
pub enum AppMatch {
    Resolved(String),
    Ambiguous(Vec<String>),
    NotFound,
}

/// (refreshed_at, stems) — std Mutex is fine: held only for a clone.
static APP_CACHE: Mutex<Option<(Instant, Vec<String>)>> = Mutex::new(None);

/// *.app stems under /Applications and /System/Applications, cached 60s.
fn installed_app_stems() -> Vec<String> {
    let mut guard = APP_CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((at, stems)) = guard.as_ref() {
        if at.elapsed() < APP_CACHE_TTL {
            return stems.clone();
        }
    }
    let stems = list_app_stems(&[
        Path::new("/Applications"),
        Path::new("/System/Applications"),
    ]);
    *guard = Some((Instant::now(), stems.clone()));
    stems
}

/// Pure listing: *.app entry stems from the given dirs, sorted, deduped.
/// Factored out of the cache so the matcher is testable with a plain Vec.
fn list_app_stems(dirs: &[&Path]) -> Vec<String> {
    let mut stems = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(stem) = name.strip_suffix(".app") {
                if !stem.is_empty() {
                    stems.push(stem.to_string());
                }
            }
        }
    }
    stems.sort();
    stems.dedup();
    stems
}

/// Lowercased alphanumeric tokens of a string.
fn tokens(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// Case-insensitive fuzzy match, exact > prefix > substring > token-subset.
/// Exact wins outright. Prefix and substring hits form one "close match"
/// pool (prefix-ranked first) for the ambiguity decision — a unique prefix
/// hit must NOT shadow a substring competitor ("chrome" is Google Chrome OR
/// Chrome Remote Desktop; guessing would open the wrong app, so DARWIN asks).
/// The token-subset tier matches when every token of a stem appears among
/// the query's tokens — which is what makes passing a whole utterance
/// ("could you open google chrome for me") work as a fallback.
pub fn match_app(stems: &[String], query: &str) -> AppMatch {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return AppMatch::NotFound;
    }

    // Exact: authoritative, never widened by lesser matches.
    let exact: Vec<String> = stems.iter().filter(|s| s.to_lowercase() == q).cloned().collect();
    if let Some(decided) = decide(exact) {
        return decided;
    }

    // Close matches: prefix hits first, then substring-only hits.
    let mut close: Vec<String> = stems
        .iter()
        .filter(|s| s.to_lowercase().starts_with(&q))
        .cloned()
        .collect();
    for stem in stems {
        if stem.to_lowercase().contains(&q) && !close.contains(stem) {
            close.push(stem.clone());
        }
    }
    if let Some(decided) = decide(close) {
        return decided;
    }

    // Token-subset (whole-utterance fallback): stem tokens ⊆ query tokens.
    let q_tokens = tokens(&q);
    let subset: Vec<String> = stems
        .iter()
        .filter(|s| {
            let st = tokens(s);
            !st.is_empty() && st.iter().all(|t| q_tokens.contains(t))
        })
        .cloned()
        .collect();
    decide(subset).unwrap_or(AppMatch::NotFound)
}

/// One candidate resolves; several are ambiguous; none defers to the next
/// tier (None).
fn decide(candidates: Vec<String>) -> Option<AppMatch> {
    match candidates.len() {
        0 => None,
        1 => Some(AppMatch::Resolved(
            candidates.into_iter().next().expect("len checked"),
        )),
        _ => Some(AppMatch::Ambiguous(candidates)),
    }
}

/// Resolve and launch an application by (fuzzy) name. Ambiguous and
/// not-found are Ok outcomes — they are valid things to say out loud.
pub async fn open_app(name: &str) -> Result<String> {
    let resolved = match_app(&installed_app_stems(), name);
    launch_match(resolved, name).await
}

/// Router variant: try the extracted app name first; if nothing matches,
/// rerun the matcher with the whole utterance (its token-subset tier digs
/// the app name out of the surrounding words).
pub async fn open_app_with_fallback(extracted: &str, utterance: &str) -> Result<String> {
    let stems = installed_app_stems();
    let primary = if extracted.trim().is_empty() { utterance } else { extracted };
    let mut m = match_app(&stems, primary);
    if m == AppMatch::NotFound && primary != utterance {
        m = match_app(&stems, utterance);
    }
    launch_match(m, primary).await
}

async fn launch_match(resolved: AppMatch, asked: &str) -> Result<String> {
    match resolved {
        AppMatch::Resolved(app) => {
            let out = run_command("/usr/bin/open", &["-a", &app]).await?;
            if out.status.success() {
                Ok(format!("Opened {app}."))
            } else {
                Err(anyhow!(
                    "open -a '{app}' failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ))
            }
        }
        AppMatch::Ambiguous(candidates) => Ok(format!(
            "Several applications match '{asked}': {}. Which one?",
            candidates.join(", ")
        )),
        AppMatch::NotFound => Ok(format!("No installed application matches '{asked}'.")),
    }
}

// ---------------------------------------------------------------------------
// quit_app
// ---------------------------------------------------------------------------

/// Ask an application to quit (audit fix: "quit Safari" used to LAUNCH
/// Safari because no quit action existed and the launcher caught the
/// utterance). Resolution mirrors open_app; the AppleScript only sends the
/// quit event when the app is actually running, so a quit request can never
/// launch anything.
pub async fn quit_app(name: &str) -> Result<String> {
    let resolved = match_app(&installed_app_stems(), name);
    quit_resolved(resolved, name).await
}

/// Router variant, same fallback ladder as open_app_with_fallback: extracted
/// name first, whole utterance second (its token-subset tier digs the app
/// name out of "could you close out of safari").
pub async fn quit_app_with_fallback(extracted: &str, utterance: &str) -> Result<String> {
    let stems = installed_app_stems();
    let primary = if extracted.trim().is_empty() { utterance } else { extracted };
    let mut m = match_app(&stems, primary);
    if m == AppMatch::NotFound && primary != utterance {
        m = match_app(&stems, utterance);
    }
    quit_resolved(m, primary).await
}

async fn quit_resolved(resolved: AppMatch, asked: &str) -> Result<String> {
    match resolved {
        AppMatch::Resolved(app) => {
            let script = quit_script(&app);
            let out = run_command("/usr/bin/osascript", &["-e", &script]).await?;
            if out.status.success() {
                if String::from_utf8_lossy(&out.stdout).trim() == "not_running" {
                    Ok(format!("{app} is not running."))
                } else {
                    Ok(format!("Quit {app}."))
                }
            } else {
                Err(anyhow!(
                    "asking {app} to quit failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ))
            }
        }
        AppMatch::Ambiguous(candidates) => Ok(format!(
            "Several applications match '{asked}': {}. Which one should quit?",
            candidates.join(", ")
        )),
        AppMatch::NotFound => Ok(format!("No installed application matches '{asked}'.")),
    }
}

/// AppleScript that quits the app ONLY if it is running — `is running` does
/// not launch the target, whereas a bare `tell ... to quit` would. The app
/// name is escaped for an AppleScript string literal.
fn quit_script(app: &str) -> String {
    let escaped = app.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "if application \"{escaped}\" is running then\n\
         \ttell application \"{escaped}\" to quit\n\
         \treturn \"quit\"\n\
         else\n\
         \treturn \"not_running\"\n\
         end if"
    )
}

// ---------------------------------------------------------------------------
// open_url / search_url
// ---------------------------------------------------------------------------

/// Fallback when a named browser cannot be honored is the system default —
/// failing the whole request over a browser nit would be worse than a note.
///
/// URL safety (hard contract): only http/https ever reaches `open`. The
/// normalizer rejects every other scheme (file:, javascript:, data:, ...),
/// embedded whitespace, and host-less strings, on EVERY path into `open` —
/// router, cloud tool, and search alike.
pub async fn open_url(url: &str, browser: Option<&str>) -> Result<String> {
    let normalized = normalize_url(url)?;
    let (app, note) = match browser.map(str::trim).filter(|b| !b.is_empty()) {
        Some(asked) => resolve_browser_choice(&installed_app_stems(), asked),
        None => (None, None),
    };
    let out = match &app {
        Some(app) => run_command("/usr/bin/open", &["-a", app, &normalized]).await?,
        None => run_command("/usr/bin/open", &[normalized.as_str()]).await?,
    };
    if !out.status.success() {
        return Err(anyhow!(
            "open '{normalized}' failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    // Speakable form: the reply is read aloud, and "https://www." is noise
    // to the ear. The full normalized URL still appears in logs/telemetry.
    let spoken = display_url(&normalized);
    let mut outcome = match &app {
        Some(app) => format!("Opened {spoken} in {app}."),
        // Name the actual default browser when we can — "the default
        // browser" invites the persona model to guess (it once claimed
        // Safari while Chrome opened).
        None => match default_browser_name().await {
            Some(name) => format!("Opened {spoken} in {name} (the default browser)."),
            None => format!("Opened {spoken} in the default browser."),
        },
    };
    if let Some(note) = note {
        outcome.push_str(" (");
        outcome.push_str(&note);
        outcome.push_str(".)");
    }
    Ok(outcome)
}

/// Human/speech form of a URL: scheme and leading www. stripped, trailing
/// slash dropped. "https://www.apple.com/" -> "apple.com".
fn display_url(url: &str) -> String {
    let s = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let s = s.strip_prefix("www.").unwrap_or(s);
    s.trim_end_matches('/').to_string()
}

/// The user's default https handler, by app name, via LaunchServices prefs.
/// Best-effort: any failure (missing plist, unknown bundle id) returns None
/// and the outcome falls back to the honest generic phrase.
async fn default_browser_name() -> Option<String> {
    let out = run_command(
        "/usr/bin/defaults",
        &[
            "read",
            "com.apple.LaunchServices/com.apple.launchservices.secure",
            "LSHandlers",
        ],
    )
    .await
    .ok()?;
    if !out.status.success() {
        return None;
    }
    let bundle_id = parse_https_handler(&String::from_utf8_lossy(&out.stdout))?;
    Some(browser_name_for_bundle(&bundle_id))
}

/// Pull the https handler's bundle id out of `defaults read` plist text:
/// the block containing LSHandlerURLScheme = https carries an
/// LSHandlerRoleAll = "<bundle id>" entry.
fn parse_https_handler(plist_text: &str) -> Option<String> {
    let mut role: Option<String> = None;
    let mut is_https = false;
    for line in plist_text.lines() {
        let line = line.trim();
        if line.starts_with('{') {
            role = None;
            is_https = false;
        } else if let Some(rest) = line.strip_prefix("LSHandlerRoleAll = ") {
            let id = rest.trim_matches(|c| c == '"' || c == ';').to_string();
            // "-" is LaunchServices' placeholder, not a bundle id.
            if id != "-" {
                role = Some(id);
            }
        } else if line.starts_with("LSHandlerURLScheme = https") {
            is_https = true;
        } else if line.starts_with('}') && is_https {
            if let Some(r) = role.take() {
                return Some(r);
            }
        }
    }
    None
}

fn browser_name_for_bundle(bundle_id: &str) -> String {
    match bundle_id.to_ascii_lowercase().as_str() {
        "com.apple.safari" => "Safari".to_string(),
        "com.google.chrome" => "Google Chrome".to_string(),
        "org.mozilla.firefox" => "Firefox".to_string(),
        "com.microsoft.edgemac" => "Microsoft Edge".to_string(),
        "company.thebrowser.browser" => "Arc".to_string(),
        "com.brave.browser" => "Brave".to_string(),
        "com.operasoftware.opera" => "Opera".to_string(),
        "com.vivaldi.vivaldi" => "Vivaldi".to_string(),
        other => {
            // Last path segment, capitalized, beats exposing a raw bundle id.
            let tail = other.rsplit('.').next().unwrap_or(other);
            let mut chars = tail.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                None => "the default browser".to_string(),
            }
        }
    }
}

/// Web search: percent-encode the query into a Google search URL and open it
/// through open_url (same browser resolution, same safety gate).
pub async fn search_url(query: &str, browser: Option<&str>) -> Result<String> {
    let q = query.trim();
    if q.is_empty() {
        return Ok(
            "The web search request had no search terms; ask what to search for.".to_string(),
        );
    }
    let outcome = open_url(&google_search_url(q), browser).await?;
    Ok(format!("Searched the web for '{q}'. {outcome}"))
}

/// Normalize a user/model-supplied URL into something `open` may receive:
/// trim; prepend https:// when no scheme; reject non-http(s) schemes,
/// embedded whitespace, and hosts that are neither dotted nor localhost.
fn normalize_url(raw: &str) -> Result<String> {
    let url = raw.trim();
    if url.is_empty() {
        return Err(anyhow!("the URL is empty"));
    }
    if url.chars().any(char::is_whitespace) {
        return Err(anyhow!("'{url}' contains whitespace and is not a valid URL"));
    }
    let lower = url.to_ascii_lowercase();
    let url = if lower.starts_with("http://") || lower.starts_with("https://") {
        url.to_string()
    } else if let Some(scheme) = leading_scheme(url) {
        return Err(anyhow!(
            "refusing to open '{url}': scheme '{scheme}:' is not allowed (http/https only)"
        ));
    } else {
        format!("https://{url}")
    };
    let host = url_host(&url);
    if host.is_empty() || !(host.contains('.') || host.eq_ignore_ascii_case("localhost")) {
        return Err(anyhow!("'{raw}' does not look like a web address"));
    }
    Ok(url)
}

/// The scheme prefix of a URL, when one is present. A colon followed by a
/// digit is a port, not a scheme ("localhost:8080" must NOT read as scheme
/// "localhost").
fn leading_scheme(url: &str) -> Option<&str> {
    let colon = url.find(':')?;
    let (scheme, rest) = (&url[..colon], &url[colon + 1..]);
    let scheme_like = scheme
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    if scheme_like && !rest.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        Some(scheme)
    } else {
        None
    }
}

/// Host of an http(s) URL: the authority minus userinfo and port.
fn url_host(url: &str) -> &str {
    let after_scheme = url.split_once("://").map(|x| x.1).unwrap_or("");
    let authority = after_scheme
        .split(&['/', '?', '#'][..])
        .next()
        .unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or(authority);
    host.split(':').next().unwrap_or(host)
}

/// Map a requested browser name onto `open -a` arguments: a unique,
/// browser-ish match is honored; anything else (ambiguous, not found, or a
/// non-browser app) falls back to the default browser with a note for the
/// outcome string. Returns (resolved app, fallback note).
fn resolve_browser_choice(stems: &[String], asked: &str) -> (Option<String>, Option<String>) {
    match match_app(stems, asked) {
        AppMatch::Resolved(app) if is_browser_app(&app) => (Some(app), None),
        AppMatch::Resolved(app) => (
            None,
            Some(format!(
                "'{asked}' matched {app}, which does not look like a browser, so the default browser was used"
            )),
        ),
        AppMatch::Ambiguous(candidates) => (
            None,
            Some(format!(
                "'{asked}' is ambiguous between {}, so the default browser was used",
                candidates.join(", ")
            )),
        ),
        AppMatch::NotFound => (
            None,
            Some(format!(
                "no installed browser matches '{asked}', so the default browser was used"
            )),
        ),
    }
}

/// Browser-ish test on an app stem, by token (substring would let "arc"
/// shadow unrelated apps).
fn is_browser_app(stem: &str) -> bool {
    const BROWSER_TOKENS: &[&str] = &[
        "safari", "chrome", "chromium", "firefox", "edge", "brave", "arc", "opera", "vivaldi",
        "orion", "duckduckgo", "tor", "librewolf", "waterfox", "zen",
    ];
    tokens(stem).iter().any(|t| BROWSER_TOKENS.contains(&t.as_str()))
}

/// Percent-encode a query value: RFC 3986 unreserved bytes pass, everything
/// else (UTF-8 bytes included) becomes %XX. Public so the SAGE web searcher
/// (anthropic.rs) can build a search-front-end query URL with the same encoder.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn google_search_url(query: &str) -> String {
    format!("https://www.google.com/search?q={}", percent_encode(query))
}

// NOTE: the outward-GET exfiltration guard used to distinguish "data-bearing"
// URLs (query/path) from bare hosts and allow the latter on a tool continuation.
// That was removed: a bare host still exfiltrates via an encoded SUBDOMAIN
// (`https://<secret>.attacker.tld`), so the hostname itself is attacker data. The
// guard in `anthropic::outward_get_egress_refusal` now refuses ANY non-user-
// originated open_url/web_search/sage_research outright — there is no
// "safe navigational open" carve-out to key off, so the old helper is gone.

// ---------------------------------------------------------------------------
// search_files
// ---------------------------------------------------------------------------

/// One Spotlight hit, ready to be phrased or opened.
#[derive(Debug)]
pub struct FileHit {
    pub path: PathBuf,
    pub kind: String,
    /// Modification time; drives the newest-first ordering.
    modified: SystemTime,
}

/// Spotlight search under $HOME: filename-contains first, content match as
/// fallback; results newest first, capped at `limit` (<= 8). The formatted
/// outcome string is what speaks; `search_files_raw` is for callers (the
/// router) that need the structured hits.
pub async fn search_files(query: &str, limit: usize) -> Result<String> {
    let hits = search_files_raw(query, limit).await?;
    Ok(format_file_hits(query, &hits))
}

pub async fn search_files_raw(query: &str, limit: usize) -> Result<Vec<FileHit>> {
    let limit = limit.clamp(1, SEARCH_LIMIT_MAX);
    let term = sanitize_query(query);
    if term.is_empty() {
        return Ok(Vec::new());
    }
    let home = std::env::var("HOME").context("HOME is not set; cannot scope the search")?;

    // Name-first: kMDItemFSName contains (case-insensitive)...
    let name_query = format!("kMDItemFSName == \"*{term}*\"c");
    let mut paths = mdfind(&home, &name_query).await?;
    if paths.is_empty() {
        // ...falling back to a plain content match.
        paths = mdfind(&home, &term).await?;
    }

    let mut hits: Vec<FileHit> = paths
        .into_iter()
        .take(MDFIND_SCAN_CAP)
        .filter_map(|p| {
            let path = PathBuf::from(p);
            let meta = std::fs::metadata(&path).ok()?;
            Some(FileHit {
                kind: file_kind(&path, meta.is_dir()),
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                path,
            })
        })
        .collect();
    hits.sort_by_key(|b| std::cmp::Reverse(b.modified)); // newest first
    hits.truncate(limit);
    Ok(hits)
}

async fn mdfind(home: &str, query: &str) -> Result<Vec<String>> {
    let out = run_command("/usr/bin/mdfind", &["-onlyin", home, query]).await?;
    if !out.status.success() {
        return Err(anyhow!(
            "mdfind failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(MDFIND_SCAN_CAP)
        .map(str::to_string)
        .collect())
}

/// Strip everything an mdfind query string could misinterpret; the result is
/// embedded inside `"*...*"` so quotes/backslashes/asterisks must go.
fn sanitize_query(query: &str) -> String {
    query
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '.' | '-' | '_'))
        .collect::<String>()
        .trim()
        .to_string()
}

fn file_kind(path: &Path, is_dir: bool) -> String {
    if is_dir {
        return "folder".to_string();
    }
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| format!("{} file", e.to_uppercase()))
        .unwrap_or_else(|| "file".to_string())
}

pub fn format_file_hits(query: &str, hits: &[FileHit]) -> String {
    if hits.is_empty() {
        return format!("No files matching '{query}' were found in the home folder.");
    }
    let lines: Vec<String> = hits
        .iter()
        .map(|h| format!("{} ({})", h.path.display(), h.kind))
        .collect();
    format!(
        "Found {} match{} for '{query}', newest first:\n{}",
        hits.len(),
        if hits.len() == 1 { "" } else { "es" },
        lines.join("\n")
    )
}

impl FileHit {
    pub fn path_str(&self) -> String {
        self.path.display().to_string()
    }
}

// ---------------------------------------------------------------------------
// open_path
// ---------------------------------------------------------------------------

/// Open an existing path with the default app — only under $HOME or
/// /Applications (canonicalized, so symlink escapes don't slip through).
pub async fn open_path(path: &str) -> Result<String> {
    let p = PathBuf::from(path);
    if !p.exists() {
        return Err(anyhow!("Path does not exist: {path}"));
    }
    let canon = std::fs::canonicalize(&p)
        .with_context(|| format!("cannot resolve path {path}"))?;
    let home = std::env::var("HOME").context("HOME is not set; cannot validate the path")?;
    if !path_allowed(&canon, Path::new(&home)) {
        return Err(anyhow!(
            "Refusing to open {}: only paths under the home folder or /Applications are allowed",
            canon.display()
        ));
    }
    let canon_str = canon.display().to_string();
    let out = run_command("/usr/bin/open", &[&canon_str]).await?;
    if out.status.success() {
        Ok(format!("Opened {canon_str}."))
    } else {
        Err(anyhow!(
            "open '{canon_str}' failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Pure policy check, on an already-canonicalized path.
fn path_allowed(canon: &Path, home: &Path) -> bool {
    canon.starts_with(home) || canon.starts_with("/Applications")
}

// ---------------------------------------------------------------------------
// set_volume
// ---------------------------------------------------------------------------

/// Set the output volume (clamped 0-100) via osascript.
pub async fn set_volume(percent: i64) -> Result<String> {
    let pct = clamp_percent(percent);
    let script = format!("set volume output volume {pct}");
    let out = run_command("/usr/bin/osascript", &["-e", &script]).await?;
    if out.status.success() {
        Ok(format!("Output volume set to {pct} percent."))
    } else {
        Err(anyhow!(
            "osascript failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn clamp_percent(percent: i64) -> u8 {
    percent.clamp(0, 100) as u8
}

// ---------------------------------------------------------------------------
// open_settings_pane — guided remediation for the Inbound Exposure Auditor
// ---------------------------------------------------------------------------

/// One allowlisted macOS System Settings pane. `id` is the stable token the
/// Exposure Auditor's remediation (and the model) names; `url` is the EXACT
/// `x-apple.systempreferences:` deep link `open` receives. This actuator only
/// ever OPENS one of these panes so the USER can flip a switch — it changes no
/// setting itself.
struct SettingsPane {
    id: &'static str,
    label: &'static str,
    url: &'static str,
}

/// The HARDCODED allowlist of System Settings panes DARWIN may deep-link to. This
/// is the ENTIRE universe of pane ids — anything not listed here is REFUSED. Each
/// `url` is a fixed `x-apple.systempreferences:` scheme string, spawned DIRECTLY
/// via `open`; it is deliberately NOT routed through `normalize_url`, which
/// rightly refuses every non-http scheme. The panes are exactly the ones the
/// exposure/posture readouts point at (Sharing to turn a service off, Firewall to
/// block inbound, etc.) — no arbitrary deep link is reachable.
const SETTINGS_PANES: &[SettingsPane] = &[
    SettingsPane {
        id: "sharing",
        label: "Sharing",
        url: "x-apple.systempreferences:com.apple.Sharing-Settings.extension",
    },
    SettingsPane {
        id: "firewall",
        label: "Network Firewall",
        url: "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Firewall",
    },
    SettingsPane {
        id: "privacy_security",
        label: "Privacy & Security",
        url: "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension",
    },
    SettingsPane {
        id: "login_items",
        label: "Login Items & Extensions",
        url: "x-apple.systempreferences:com.apple.LoginItems-Settings.extension",
    },
    SettingsPane {
        id: "network",
        label: "Network",
        url: "x-apple.systempreferences:com.apple.Network-Settings.extension",
    },
    SettingsPane {
        id: "software_update",
        label: "Software Update",
        url: "x-apple.systempreferences:com.apple.preferences.softwareupdate",
    },
    SettingsPane {
        id: "filevault",
        label: "FileVault",
        url: "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?FileVault",
    },
];

/// PURE: resolve an allowlisted pane id to its pane, or `None` when the id is not
/// in the hardcoded allowlist. Case-insensitive on the id token; NOTHING else is
/// accepted — no arbitrary URL, no scheme, no path, no `x-apple.*` string the
/// caller supplies.
fn resolve_settings_pane(pane_id: &str) -> Option<&'static SettingsPane> {
    let want = pane_id.trim().to_ascii_lowercase();
    SETTINGS_PANES.iter().find(|p| p.id == want)
}

/// Whether `pane_id` is one the actuator will open — the public predicate the
/// Exposure Auditor uses to verify a remediation link before it offers one, so a
/// service can never point at a pane id the actuator would refuse. Exercised by
/// the exposure tests (every mapped remediation pane must resolve here); also the
/// natural validation hook for a future HUD "open Settings" jump.
#[allow(dead_code)] // public predicate; exercised by exposure.rs's service-map test
pub fn is_known_settings_pane(pane_id: &str) -> bool {
    resolve_settings_pane(pane_id).is_some()
}

/// The allowlisted pane ids, for a refusal message + the tool schema doc.
fn settings_pane_ids() -> String {
    SETTINGS_PANES.iter().map(|p| p.id).collect::<Vec<_>>().join(", ")
}

/// Open ONE allowlisted macOS System Settings pane so the USER can flip a switch
/// (turn a sharing service off, enable the firewall, …). GUIDED REMEDIATION for
/// the Inbound Exposure Auditor: it deep-links to the exact pane and CHANGES
/// NOTHING itself — the toggle is the user's own action. Benign-only + gated:
///   * only an id in the HARDCODED `SETTINGS_PANES` allowlist is honored; an
///     unknown / nonsense id is REFUSED (never a fabricated or arbitrary URL, and
///     never a caller-supplied scheme string);
///   * the pane URL is a fixed `x-apple.systempreferences:` string spawned
///     DIRECTLY via `open` — deliberately NOT through `normalize_url`, which
///     rightly refuses non-http schemes;
///   * CONSEQUENTIAL: in [`ActionMode::DryRun`] it returns a faithful preview and
///     opens nothing; only [`ActionMode::Execute`] (master switch ON + a fresh
///     human confirm, via the standard park-and-replay gate) actually opens it.
pub async fn open_settings_pane(pane_id: &str, mode: ActionMode) -> Result<String> {
    let Some(pane) = resolve_settings_pane(pane_id) else {
        return Err(anyhow!(
            "refusing to open settings pane '{pane_id}': it is not in the allowlist ({})",
            settings_pane_ids()
        ));
    };
    if mode == ActionMode::DryRun {
        return Ok(format!(
            "[dry run] Would open System Settings to the {} pane so you can change it there \
             (I change nothing myself). Enable consequential actions and confirm to open it.",
            pane.label
        ));
    }
    let out = run_command("/usr/bin/open", &[pane.url]).await?;
    if out.status.success() {
        Ok(format!(
            "Opened System Settings to {} — flip the switch there; I changed nothing.",
            pane.label
        ))
    } else {
        Err(anyhow!(
            "opening the {} settings pane failed: {}",
            pane.label,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

// ---------------------------------------------------------------------------
// system_status
// ---------------------------------------------------------------------------

/// The cached SystemSnapshot as the canonical status data string. Never
/// fails — kept as Result<String> so all actions share one signature.
pub async fn system_status() -> Result<String> {
    Ok(system_status_data().await)
}

/// Live system stats as raw data for the LLM to phrase (and as the spoken
/// fallback when the LLM is down). Served from telemetry's 2s snapshot
/// cache — instant, and <=2s staleness is invisible in spoken stats. Only
/// when the cache is still empty (task not ticked yet, or unit tests) does
/// the old direct sysinfo read run, with its ~250ms double-refresh sleep.
pub async fn system_status_data() -> String {
    let snapshot = match telemetry::latest_snapshot() {
        Some(snapshot) => snapshot,
        None => direct_system_snapshot().await,
    };
    format_system_status(&snapshot)
}

/// Cold-cache fallback: one direct sysinfo reading. CPU usage needs two
/// refreshes with a gap (sysinfo measures the delta between them).
async fn direct_system_snapshot() -> telemetry::SystemSnapshot {
    let mut sys = sysinfo::System::new();
    sys.refresh_cpu_usage();
    tokio::time::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL).await;
    sys.refresh_cpu_usage();
    sys.refresh_memory();
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let (disk_free_bytes, disk_total_bytes) = match telemetry::root_disk_free_and_total(&disks) {
        Some((free, total)) => (Some(free), Some(total)),
        None => (None, None),
    };
    telemetry::SystemSnapshot {
        cpu_percent: sys.global_cpu_usage(),
        mem_used_bytes: sys.used_memory(),
        mem_total_bytes: sys.total_memory(),
        disk_free_bytes,
        disk_total_bytes,
        uptime_secs: sysinfo::System::uptime(),
    }
}

/// Phrase a snapshot as the verified data string handed to the LLM.
fn format_system_status(s: &telemetry::SystemSnapshot) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let cpu = s.cpu_percent;
    let mem_used = s.mem_used_bytes as f64 / GIB;
    let mem_total = s.mem_total_bytes as f64 / GIB;

    let (days, hours, mins) = (
        s.uptime_secs / 86_400,
        (s.uptime_secs % 86_400) / 3_600,
        (s.uptime_secs % 3_600) / 60,
    );
    let uptime = if days > 0 {
        format!("{days} days {hours} hours")
    } else if hours > 0 {
        format!("{hours} hours {mins} minutes")
    } else {
        format!("{mins} minutes")
    };

    let disk_part = s
        .disk_free_bytes
        .map(|b| format!("; disk {:.0} gigabytes free", b as f64 / GIB))
        .unwrap_or_default();
    format!(
        "CPU at {cpu:.0} percent; memory {mem_used:.1} of {mem_total:.0} gigabytes used{disk_part}; uptime {uptime}"
    )
}

// ---------------------------------------------------------------------------
// process plumbing
// ---------------------------------------------------------------------------

/// Spawn a command with explicit args (never a shell string), capture its
/// output, and bound it with the 5s action timeout; kill_on_drop reaps the
/// child when the timeout drops the future.
async fn run_command(program: &str, args: &[&str]) -> Result<std::process::Output> {
    let mut cmd = Command::new(program);
    cmd.args(args).kill_on_drop(true);
    match tokio::time::timeout(ACTION_TIMEOUT, cmd.output()).await {
        Ok(result) => result.with_context(|| format!("failed to run {program}")),
        Err(_) => {
            warn!(program, "action command timed out; killing it");
            Err(anyhow!(
                "{program} timed out after {}s",
                ACTION_TIMEOUT.as_secs()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::SystemSnapshot;

    fn stems(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn match_app_prefers_exact_over_prefix_and_substring() {
        let s = stems(&["Notes", "NotesPlus", "Sticky Notes"]);
        assert_eq!(match_app(&s, "notes"), AppMatch::Resolved("Notes".into()));
    }

    #[test]
    fn match_app_resolves_unique_close_match_but_asks_when_shadowed() {
        let s = stems(&["Google Chrome", "Safari", "Chrome Remote Desktop"]);
        assert_eq!(
            match_app(&s, "goog"),
            AppMatch::Resolved("Google Chrome".into())
        );
        // "chrome" prefix-matches Chrome Remote Desktop AND substring-matches
        // Google Chrome — a unique prefix hit must not shadow the competitor
        // (guessing here opens the wrong app); prefix hits rank first.
        assert_eq!(
            match_app(&s, "chrome"),
            AppMatch::Ambiguous(vec![
                "Chrome Remote Desktop".into(),
                "Google Chrome".into()
            ])
        );
        assert_eq!(
            match_app(&s, "safa"),
            AppMatch::Resolved("Safari".into())
        );
    }

    #[test]
    fn match_app_is_ambiguous_on_multiple_prefix_hits() {
        let s = stems(&["Google Chrome", "Google Drive", "Safari"]);
        assert_eq!(
            match_app(&s, "google"),
            AppMatch::Ambiguous(vec!["Google Chrome".into(), "Google Drive".into()])
        );
    }

    #[test]
    fn match_app_token_subset_digs_the_app_out_of_an_utterance() {
        let s = stems(&["Google Chrome", "Safari", "Photo Booth"]);
        // Whole-utterance fallback: stem tokens ⊆ utterance tokens.
        assert_eq!(
            match_app(&s, "could you open google chrome for me please"),
            AppMatch::Resolved("Google Chrome".into())
        );
        assert_eq!(
            match_app(&s, "fire up the photo booth would you"),
            AppMatch::Resolved("Photo Booth".into())
        );
    }

    #[test]
    fn match_app_not_found_and_empty_query() {
        let s = stems(&["Safari"]);
        assert_eq!(match_app(&s, "blender"), AppMatch::NotFound);
        assert_eq!(match_app(&s, "   "), AppMatch::NotFound);
        assert_eq!(match_app(&[], "safari"), AppMatch::NotFound);
    }

    #[test]
    fn normalize_url_prepends_https_and_accepts_real_hosts() {
        // Bare domain -> https.
        assert_eq!(normalize_url("apple.com").unwrap(), "https://apple.com");
        assert_eq!(
            normalize_url("  www.rust-lang.org/learn  ").unwrap(),
            "https://www.rust-lang.org/learn"
        );
        // Explicit schemes pass through untouched (http stays http).
        assert_eq!(
            normalize_url("https://apple.com/mac").unwrap(),
            "https://apple.com/mac"
        );
        assert_eq!(
            normalize_url("http://example.org").unwrap(),
            "http://example.org"
        );
        // localhost is a valid dotless host, with or without a port (the
        // colon-digit must read as a port, never as a scheme).
        assert_eq!(normalize_url("localhost").unwrap(), "https://localhost");
        assert_eq!(
            normalize_url("localhost:8080/dash").unwrap(),
            "https://localhost:8080/dash"
        );
        // Dotted host with port and userinfo still passes the host check.
        assert_eq!(
            normalize_url("apple.com:443").unwrap(),
            "https://apple.com:443"
        );
    }

    #[test]
    fn normalize_url_rejects_bad_schemes_whitespace_and_hostless_strings() {
        // Scheme allowlist: nothing but http/https may reach `open`.
        assert!(normalize_url("file:///etc/passwd").is_err());
        assert!(normalize_url("javascript:alert(1)").is_err());
        assert!(normalize_url("data:text/html,<script>x</script>").is_err());
        assert!(normalize_url("ftp://example.com").is_err());
        assert!(normalize_url("FILE:///etc/hosts").is_err());
        // Embedded whitespace is never a URL.
        assert!(normalize_url("apple .com").is_err());
        assert!(normalize_url("open the website").is_err());
        // Host must be dotted or localhost; empty input is empty.
        assert!(normalize_url("justaword").is_err());
        assert!(normalize_url("https://").is_err());
        assert!(normalize_url("").is_err());
        assert!(normalize_url("   ").is_err());
    }

    #[test]
    fn search_url_percent_encodes_into_a_google_query() {
        assert_eq!(
            google_search_url("rust async tutorials"),
            "https://www.google.com/search?q=rust%20async%20tutorials"
        );
        // Reserved characters and non-ASCII are byte-encoded.
        assert_eq!(percent_encode("c++ & émoji"), "c%2B%2B%20%26%20%C3%A9moji");
        assert_eq!(percent_encode("safe-chars_.~"), "safe-chars_.~");
    }

    #[test]
    fn https_handler_parsing_matches_launchservices_format() {
        // Shape captured from a real `defaults read` on macOS 26: nested
        // PreferredVersions dict with a placeholder "-" role inside the
        // https block, real bundle id at block level.
        let plist = r#"(
    {
        LSHandlerPreferredVersions =             {
            LSHandlerRoleAll = "-";
        };
        LSHandlerRoleAll = "com.google.chrome";
        LSHandlerURLScheme = http;
    },
    {
        LSHandlerPreferredVersions =             {
            LSHandlerRoleAll = "-";
        };
        LSHandlerRoleAll = "ai.perplexity.comet";
        LSHandlerURLScheme = https;
    }
)"#;
        assert_eq!(
            parse_https_handler(plist).as_deref(),
            Some("ai.perplexity.comet")
        );
        // No https block -> None; placeholder-only roles -> None.
        assert_eq!(parse_https_handler("( { LSHandlerURLScheme = http; } )"), None);
        let placeholder = "( { LSHandlerRoleAll = \"-\"; LSHandlerURLScheme = https; } )";
        assert_eq!(parse_https_handler(placeholder), None);
    }

    #[test]
    fn urls_render_speakably() {
        assert_eq!(display_url("https://www.apple.com/"), "apple.com");
        assert_eq!(display_url("http://youtube.com"), "youtube.com");
        assert_eq!(
            display_url("https://www.google.com/search?q=rust"),
            "google.com/search?q=rust"
        );
    }

    #[test]
    fn bundle_ids_map_to_speakable_browser_names() {
        assert_eq!(browser_name_for_bundle("com.apple.Safari"), "Safari");
        assert_eq!(browser_name_for_bundle("com.google.chrome"), "Google Chrome");
        // Unknown bundles fall back to a capitalized tail, never a raw id.
        assert_eq!(browser_name_for_bundle("ai.perplexity.comet"), "Comet");
    }

    #[test]
    fn browser_detection_is_token_based() {
        assert!(is_browser_app("Safari"));
        assert!(is_browser_app("Google Chrome"));
        assert!(is_browser_app("Microsoft Edge"));
        assert!(is_browser_app("Firefox"));
        assert!(!is_browser_app("Visual Studio Code"));
        assert!(!is_browser_app("Notes"));
        // Token match, not substring: "arc" must not light up inside words.
        assert!(is_browser_app("Arc"));
        assert!(!is_browser_app("GarageBand"));
    }

    #[test]
    fn browser_choice_honors_a_unique_browser_and_falls_back_otherwise() {
        let s = stems(&["Safari", "Google Chrome", "Chrome Remote Desktop", "Notes"]);
        // Unique browser-ish match: honored.
        assert_eq!(
            resolve_browser_choice(&s, "safari"),
            (Some("Safari".to_string()), None)
        );
        // Ambiguous: default browser + note.
        let (app, note) = resolve_browser_choice(&s, "chrome");
        assert_eq!(app, None);
        assert!(note.unwrap().contains("ambiguous"));
        // Not found: default browser + note.
        let (app, note) = resolve_browser_choice(&s, "netscape");
        assert_eq!(app, None);
        assert!(note.unwrap().contains("no installed browser"));
        // Resolved but not a browser: default browser + note.
        let (app, note) = resolve_browser_choice(&s, "notes");
        assert_eq!(app, None);
        assert!(note.unwrap().contains("does not look like a browser"));
    }

    #[test]
    fn quit_script_guards_on_running_and_escapes_the_name() {
        let script = quit_script("Safari");
        assert!(script.contains("if application \"Safari\" is running then"));
        assert!(script.contains("tell application \"Safari\" to quit"));
        assert!(script.contains("not_running"));
        // Quotes and backslashes in a stem cannot break out of the literal.
        let tricky = quit_script(r#"We"ird\App"#);
        assert!(tricky.contains(r#""We\"ird\\App""#));
    }

    #[test]
    fn sanitize_query_strips_mdfind_metacharacters() {
        assert_eq!(sanitize_query(r#"bud"get*\.xlsx"#), "budget.xlsx");
        assert_eq!(sanitize_query("tax report 2025"), "tax report 2025");
        assert_eq!(sanitize_query("  \"*\\  "), "");
    }

    #[test]
    fn clamp_percent_bounds_both_ends() {
        assert_eq!(clamp_percent(-20), 0);
        assert_eq!(clamp_percent(0), 0);
        assert_eq!(clamp_percent(55), 55);
        assert_eq!(clamp_percent(100), 100);
        assert_eq!(clamp_percent(940), 100);
    }

    // -- open_settings_pane: the allowlisted guided-remediation actuator -------

    #[test]
    fn settings_pane_allowlist_resolves_known_and_refuses_unknown() {
        // Every allowlisted id resolves to a fixed x-apple.systempreferences: URL —
        // never an http URL and never something normalize_url could ever accept.
        for pane in SETTINGS_PANES {
            let got = resolve_settings_pane(pane.id).expect("allowlisted id resolves");
            assert_eq!(got.id, pane.id);
            assert!(
                got.url.starts_with("x-apple.systempreferences:"),
                "pane {} must deep-link a settings scheme, got {}",
                pane.id,
                got.url
            );
            assert!(is_known_settings_pane(pane.id));
        }
        // Id match is case-insensitive and trims surrounding space.
        assert!(resolve_settings_pane("  SHARING ").is_some());
        // An unknown / nonsense / injection-y id is NOT in the allowlist.
        for bad in ["", "sharingX", "nonsense", "../etc", "https://evil.tld", "file:///etc/passwd"] {
            assert!(resolve_settings_pane(bad).is_none(), "{bad:?} must not resolve");
            assert!(!is_known_settings_pane(bad));
        }
    }

    #[tokio::test]
    async fn open_settings_pane_refuses_an_unknown_pane_id() {
        // A nonsense pane id is REFUSED (an Err) in BOTH modes — it never falls
        // through to spawning `open`, and the refusal names the allowlist.
        let dry = open_settings_pane("totally-made-up", ActionMode::DryRun).await;
        assert!(dry.is_err(), "unknown pane id must be refused even as a dry run");
        let err = open_settings_pane("totally-made-up", ActionMode::Execute).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not in the allowlist"), "refusal explains why: {msg}");
        assert!(msg.contains("sharing"), "refusal lists the real allowlist: {msg}");
        // An attempt to smuggle an arbitrary scheme as the "pane id" is refused too.
        assert!(open_settings_pane("x-apple.systempreferences:com.apple.evil", ActionMode::Execute)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn open_settings_pane_dry_run_previews_a_known_pane_without_opening() {
        // DryRun for an allowlisted pane returns a faithful preview and spawns
        // nothing — the master-off / not-yet-confirmed path.
        let out = open_settings_pane("firewall", ActionMode::DryRun).await.unwrap();
        assert!(out.starts_with("[dry run]"), "must be a dry-run preview: {out}");
        assert!(out.contains("Network Firewall"), "names the exact pane: {out}");
        assert!(out.to_lowercase().contains("change nothing"), "states it changes nothing: {out}");
    }

    #[test]
    fn path_allowed_only_under_home_or_applications() {
        let home = Path::new("/Users/darwin");
        assert!(path_allowed(Path::new("/Users/darwin/Documents/x.pdf"), home));
        assert!(path_allowed(Path::new("/Applications/Safari.app"), home));
        assert!(!path_allowed(Path::new("/etc/passwd"), home));
        assert!(!path_allowed(Path::new("/Users/other/secret.txt"), home));
        // Prefix is component-wise: /Users/darwinx must NOT pass.
        assert!(!path_allowed(Path::new("/Users/darwinx/file"), home));
    }

    #[test]
    fn file_kind_labels_folders_and_extensions() {
        assert_eq!(file_kind(Path::new("/x/dir"), true), "folder");
        assert_eq!(file_kind(Path::new("/x/a.pdf"), false), "PDF file");
        assert_eq!(file_kind(Path::new("/x/README"), false), "file");
    }

    /// With the snapshot cache empty (no system_load_task in unit tests)
    /// this exercises the direct-read fallback end to end.
    #[tokio::test]
    async fn status_data_carries_live_stats() {
        let s = system_status_data().await;
        assert!(s.contains("CPU at"), "missing CPU: {s}");
        assert!(s.contains("memory"), "missing memory: {s}");
        assert!(s.contains("uptime"), "missing uptime: {s}");
        // total memory must be a real number, not zero
        assert!(!s.contains("of 0 gigabytes"), "bogus memory total: {s}");
    }

    #[test]
    fn snapshot_formats_into_the_status_string() {
        const GIB: u64 = 1024 * 1024 * 1024;
        let snap = SystemSnapshot {
            cpu_percent: 6.4,
            mem_used_bytes: 11 * GIB + GIB / 2, // 11.5 GiB
            mem_total_bytes: 16 * GIB,
            disk_free_bytes: Some(231 * GIB),
            disk_total_bytes: Some(500 * GIB),
            uptime_secs: 2 * 86_400 + 3 * 3_600, // 2 days 3 hours
        };
        assert_eq!(
            format_system_status(&snap),
            "CPU at 6 percent; memory 11.5 of 16 gigabytes used; disk 231 gigabytes free; uptime 2 days 3 hours"
        );

        let no_disk = SystemSnapshot {
            disk_free_bytes: None,
            uptime_secs: 47 * 60, // minutes-only branch
            ..snap
        };
        assert_eq!(
            format_system_status(&no_disk),
            "CPU at 6 percent; memory 11.5 of 16 gigabytes used; uptime 47 minutes"
        );
    }
}

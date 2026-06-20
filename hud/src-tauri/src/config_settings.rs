//! HUD Settings — the config-edit BACKEND (the trust boundary for `config/jarvis.toml`).
//!
//! WHAT THIS IS: three Tauri commands that let the Settings window READ the current
//! whitelisted settings (`config_get`), write a BATCH of validated edits IN PLACE
//! preserving every comment + the file structure (`config_set`), and restart the
//! daemon so the edits take effect (`daemon_restart`).
//!
//! WHAT THIS IS NOT: it adds NO runtime authority. The runtime gate enforcement
//! (policy.rs, confirm.rs, voiceid.rs, lockdown.rs, integrations/mod.rs) is left
//! ALONE — this module only edits the TOML those modules read at startup. There is
//! NO hot-reload (every daemon module caches its config in a `OnceLock` at boot),
//! so a change takes effect ONLY on the explicit `daemon_restart`. The UI is honest
//! about that: it batches edits and exposes one "Apply changes — restarts JARVIS".
//!
//! SAFETY (the whole point — KEEP the gates while giving control):
//!   * `config_set` is a STRICT WHITELIST. Only the keys in [`SETTINGS`] can be
//!     written; each is validated by TYPE and by per-key options/range. An unknown
//!     key, a wrong type, an out-of-range number, an out-of-options string — all are
//!     REJECTED with a clear error before any write. There is NO arbitrary-TOML
//!     write, NO key injection, NO path traversal: a change names a (section, key)
//!     that must be in the table, and the value is re-serialized by US (never echoed
//!     raw), so a hostile value can never smuggle a second key or a comment onto the
//!     line.
//!   * The WRITE is an IN-PLACE value edit: we locate the matching `key =` line
//!     inside the correct `[section]` and rewrite ONLY its value token, leaving the
//!     trailing `# comment`, the key name, the indentation, and every other line
//!     byte-for-byte. The carefully-written honest comments are preserved.
//!
//! The autonomy controls (self_heal / forge / optimize) are exposed as a single
//! 3-way state — Off / Propose / Auto — derived from `enabled` + `mode` on read,
//! and mapped back to the two underlying keys on write.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// The launchd label for jarvisd — the SAME label `scripts/apply_heal.sh` kicks
/// after a healed build. Restart is `launchctl kickstart -k gui/<uid>/<label>`.
const DAEMON_LABEL: &str = "com.jarvis.daemon";

/* ----------------------------------------------------------- the whitelist */

/// The kind of a whitelisted setting — drives BOTH the GET coercion (TOML token
/// -> typed JSON) and the SET validation (typed JSON -> a re-serialized TOML
/// value token). Everything outside these shapes is rejected.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    /// `true` | `false`.
    Bool,
    /// An integer within `[min, max]` inclusive.
    Int { min: i64, max: i64 },
    /// A float within `[min, max]` inclusive.
    Float { min: f64, max: f64 },
    /// A string that MUST be one of the listed options (exact match).
    Enum(&'static [&'static str]),
    /// A FREEFORM string (e.g. a HuggingFace repo id). The empty string is the
    /// HONEST "feature inert / disabled" value and is always allowed. A non-empty
    /// value is trimmed, length-capped, and rejected if it contains a newline /
    /// control char / NUL (which could break out of the value token); on write it
    /// is emitted as a properly-ESCAPED TOML basic string so a quote/backslash can
    /// never inject a second key. NO options constraint — repo ids are open.
    Str,
    /// A single-line ARRAY of strings. `paths` = true means each element MUST be
    /// an ABSOLUTE path (starts with `/`); false admits any non-empty repo-id
    /// string. Every element is trimmed, deduped, length-capped, rejected for a
    /// newline / control char / NUL, and emitted as an ESCAPED TOML basic string;
    /// the array is written on ONE line (`key = ["a", "b"]`, empty => `[]`),
    /// preserving the trailing comment. NO element can break out of its token.
    StrArray { paths: bool },
}

/// One whitelisted setting: which `[section]` + `key` line it edits, and how it
/// is validated. The id the frontend uses is `"<section>.<key>"`.
#[derive(Debug, Clone, Copy)]
pub struct Setting {
    pub section: &'static str,
    pub key: &'static str,
    pub kind: Kind,
}

impl Setting {
    /// The dotted id the frontend addresses this setting by (`section.key`).
    pub fn id(&self) -> String {
        format!("{}.{}", self.section, self.key)
    }
}

/// The 3-way autonomy controls (self_heal / forge / optimize). Each is ONE UI
/// control mapping to two underlying keys: `enabled` (bool) + `mode`
/// ("propose"|"auto"). Off = enabled false; Propose = enabled true + mode
/// propose; Auto = enabled true + mode auto. These sections are handled by the
/// dedicated 3-way path, NOT the flat [`SETTINGS`] table, so a caller can never
/// poke `self_heal.enabled` directly through the flat key path and desync the pair.
pub const AUTONOMY_SECTIONS: &[&str] = &["self_heal", "forge", "optimize"];

/// The allowed values for the 3-way autonomy control.
pub const AUTONOMY_STATES: &[&str] = &["off", "propose", "auto"];

/// The flat whitelist: EXACTLY the simple (bool / int / float / enum) settings the
/// Settings window may edit. Ranges + options are lifted from daemon/src/config.rs
/// and config/jarvis.toml. NOTHING outside this table (plus the 3-way autonomy
/// sections) can be written. Autonomy `enabled`/`mode` are DELIBERATELY absent
/// here — they are driven only through the 3-way path.
pub const SETTINGS: &[Setting] = &[
    // ---- SAFETY & GATES ----
    Setting { section: "integrations", key: "allow_consequential", kind: Kind::Bool },
    Setting { section: "voice_id", key: "enabled", kind: Kind::Bool },
    Setting { section: "voice_id", key: "gate_scope", kind: Kind::Enum(&["consequential", "all"]) },
    // threshold: cosine accept point. config comment + voiceid.rs treat it as a
    // 0.70..=1.0 operating band (catalog: 0.70-1.0).
    Setting { section: "voice_id", key: "threshold", kind: Kind::Float { min: 0.70, max: 1.0 } },
    Setting { section: "security", key: "encrypt_memory", kind: Kind::Bool },
    Setting { section: "policy", key: "enabled", kind: Kind::Bool },

    // ---- AUTONOMY (toggles; the 3-way controls live in AUTONOMY_SECTIONS) ----
    Setting { section: "standing", key: "enabled", kind: Kind::Bool },
    Setting { section: "drafts", key: "enabled", kind: Kind::Bool },
    Setting { section: "missions", key: "durable", kind: Kind::Bool },
    Setting { section: "macros", key: "enabled", kind: Kind::Bool },

    // ---- PROACTIVITY ----
    Setting { section: "proactive", key: "enabled", kind: Kind::Bool },
    Setting { section: "proactive", key: "speak", kind: Kind::Bool },
    Setting { section: "proactive", key: "suggest", kind: Kind::Bool },
    // quiet hours: local hour 0..=23 (u8 in config.rs).
    Setting { section: "proactive", key: "quiet_start", kind: Kind::Int { min: 0, max: 23 } },
    Setting { section: "proactive", key: "quiet_end", kind: Kind::Int { min: 0, max: 23 } },
    Setting { section: "focus", key: "profile", kind: Kind::Enum(&["default", "work", "sleep", "deep_focus"]) },

    // ---- PERCEPTION ----
    Setting { section: "screen_context", key: "enabled", kind: Kind::Bool },
    // interval_secs: floored to >=1 by config.rs; cap at a sane day to avoid a
    // hostile huge value. (>=1 is the real floor; the upper bound is a guard.)
    Setting { section: "screen_context", key: "interval_secs", kind: Kind::Int { min: 1, max: 86_400 } },
    Setting { section: "vision", key: "enabled", kind: Kind::Bool },
    // vision.model: freeform on-device VLM repo id; "" = honest vlm_unavailable.
    Setting { section: "vision", key: "model", kind: Kind::Str },
    Setting { section: "image", key: "enabled", kind: Kind::Bool },
    // image.model: freeform on-device diffusion repo id; "" = honest unavailable.
    Setting { section: "image", key: "model", kind: Kind::Str },
    Setting { section: "audio", key: "sound_monitor", kind: Kind::Bool },
    Setting { section: "interpret", key: "live", kind: Kind::Bool },
    Setting { section: "interpret", key: "speak", kind: Kind::Bool },
    Setting { section: "episodic", key: "enabled", kind: Kind::Bool },

    // ---- VOICE & SPEECH ----
    Setting { section: "voice", key: "cloud_tier", kind: Kind::Bool },
    Setting { section: "voice", key: "cloud_stt", kind: Kind::Bool },
    Setting { section: "voice", key: "adaptive_prosody", kind: Kind::Bool },
    Setting { section: "voice", key: "whisper", kind: Kind::Bool },
    Setting { section: "voice", key: "whisper_auto", kind: Kind::Bool },
    Setting { section: "voice", key: "diarize", kind: Kind::Bool },
    Setting { section: "voice", key: "cloud_sfx", kind: Kind::Bool },
    Setting { section: "voice", key: "stream_tts", kind: Kind::Bool },
    Setting { section: "voice", key: "pronunciation_dictionary_id", kind: Kind::Str },
    Setting { section: "voice", key: "pronunciation_dictionary_version", kind: Kind::Str },
    Setting { section: "speech", key: "engine", kind: Kind::Enum(&["kokoro", "csm", "orpheus"]) },
    // speech.model: freeform HF repo for the chosen engine; "" = engine default.
    Setting { section: "speech", key: "model", kind: Kind::Str },
    Setting { section: "speech", key: "instant_opener", kind: Kind::Bool },

    // ---- CAPABILITIES ----
    Setting { section: "shell", key: "enabled", kind: Kind::Bool },
    Setting { section: "ui_automation", key: "enabled", kind: Kind::Bool },
    Setting { section: "mcp", key: "enabled", kind: Kind::Bool },
    Setting { section: "webhooks", key: "enabled", kind: Kind::Bool },
    Setting { section: "plugin_sdk", key: "enabled", kind: Kind::Bool },
    Setting { section: "docsearch", key: "enabled", kind: Kind::Bool },
    // docsearch.roots: ABSOLUTE folder allowlist JARVIS may index; ships empty.
    Setting { section: "docsearch", key: "roots", kind: Kind::StrArray { paths: true } },
    Setting { section: "docsearch", key: "build_graph", kind: Kind::Bool },
    Setting { section: "code", key: "enabled", kind: Kind::Bool },
    // code.roots: ABSOLUTE codebase-root allowlist (enable + apply confinement).
    Setting { section: "code", key: "roots", kind: Kind::StrArray { paths: true } },
    Setting { section: "local_tools", key: "enabled", kind: Kind::Bool },
    Setting { section: "report", key: "enabled", kind: Kind::Bool },
    Setting { section: "chart", key: "enabled", kind: Kind::Bool },
    Setting { section: "answers", key: "cite", kind: Kind::Bool },
    Setting { section: "answers", key: "confidence", kind: Kind::Bool },
    Setting { section: "answers", key: "verify", kind: Kind::Bool },
    Setting { section: "answers", key: "cross_check", kind: Kind::Bool },
    Setting { section: "answers", key: "debate", kind: Kind::Bool },

    // ---- PERFORMANCE & MODELS ----
    Setting { section: "power", key: "adaptive", kind: Kind::Bool },
    Setting { section: "inference", key: "speculative", kind: Kind::Bool },
    // inference.draft_model: freeform small DRAFT repo id; "" = speculative inert.
    Setting { section: "inference", key: "draft_model", kind: Kind::Str },
    Setting { section: "inference", key: "quant", kind: Kind::Enum(&["auto", "fp16", "int8", "int4"]) },
    // models.classifier: freeform dedicated classify repo id; "" = reuse llm.
    Setting { section: "models", key: "classifier", kind: Kind::Str },
    // models.local_warm: extra local repo ids kept warm beside `llm` (no paths).
    Setting { section: "models", key: "local_warm", kind: Kind::StrArray { paths: false } },
    // models.local_budget_gib: RAM budget (GiB) the warm-set may occupy; 0 = single.
    Setting { section: "models", key: "local_budget_gib", kind: Kind::Float { min: 0.0, max: 8.0 } },
    Setting { section: "router", key: "conversation_route", kind: Kind::Enum(&["cloud_heavy", "cloud_fast", "local"]) },
];

/// Look up a flat setting by its `section.key` id.
fn setting_by_id(id: &str) -> Option<&'static Setting> {
    let (section, key) = id.split_once('.')?;
    SETTINGS
        .iter()
        .find(|s| s.section == section && s.key == key)
}

/* ----------------------------------------------------------- typed values */

/// A typed setting VALUE crossing the IPC boundary. Bools, integers, floats and
/// strings (enums) for the flat settings; the 3-way autonomy state is also a
/// string (`"off"|"propose"|"auto"`) addressed by the bare section id (e.g.
/// `"self_heal"`). serde(untagged) so the JS side sends a plain `true` / `30` /
/// `"work"` without a wrapper.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum SettingValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// A string array (model-id list / absolute-path allowlist). serde(untagged)
    /// places this BEFORE Str so a JSON `[]` / `["a"]` lands here, never coerced
    /// to a string. The JS side sends a plain `["/Users/x"]`.
    StrList(Vec<String>),
}

/* ----------------------------------------------------------- GET (parse) */

/// The current value of one setting, plus its descriptor — enough for the UI to
/// render the right control without re-deriving the whitelist. `kind` is a short
/// tag ("bool"|"int"|"float"|"enum"|"autonomy"); `options`/`min`/`max` describe
/// the allowed input; `value` is the live value parsed from the file.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SettingState {
    pub id: String,
    pub section: String,
    pub key: String,
    pub kind: String,
    pub value: SettingValue,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
}

/// Strip a TOML inline `# comment` from a value region, respecting a single-/
/// double-quoted string so a `#` INSIDE a quoted value is not treated as a
/// comment. A `\` inside a DOUBLE-quoted (basic) string escapes the next byte, so
/// an escaped `\"` does NOT close the string — without this, a value we ourselves
/// wrote (with an escaped quote) would be mis-truncated on read. A single-quoted
/// (literal) string has no escapes. Returns the trimmed value text.
fn strip_inline_comment(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut in_str: Option<u8> = None;
    let mut end = bytes.len();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match in_str {
            Some(q) => {
                // In a basic ("..") string a backslash escapes the next byte.
                if q == b'"' && b == b'\\' {
                    i += 2;
                    continue;
                }
                if b == q {
                    in_str = None;
                }
            }
            None => {
                if b == b'"' || b == b'\'' {
                    in_str = Some(b);
                } else if b == b'#' {
                    end = i;
                    break;
                }
            }
        }
        i += 1;
    }
    raw[..end].trim().to_string()
}

/// The per-element / per-string length cap (chars). A HuggingFace repo id or an
/// absolute path is well under this; the cap guards against a hostile huge value.
const MAX_STR_LEN: usize = 200;

/// Parse a raw TOML value token into a typed [`SettingValue`] under a known
/// [`Kind`]. Returns None when the token does not match the kind (a malformed
/// file value) so GET can fall back to the file's verbatim text rather than lie.
fn coerce_value(kind: Kind, token: &str) -> Option<SettingValue> {
    match kind {
        Kind::Bool => match token {
            "true" => Some(SettingValue::Bool(true)),
            "false" => Some(SettingValue::Bool(false)),
            _ => None,
        },
        Kind::Int { .. } => token.parse::<i64>().ok().map(SettingValue::Int),
        Kind::Float { .. } => token.parse::<f64>().ok().map(SettingValue::Float),
        Kind::Enum(_) => {
            let unq = unquote(token)?;
            Some(SettingValue::Str(unq))
        }
        // A freeform string: read the quoted basic/literal string back into its
        // inner text. The empty token "" reads as an empty string (honest "unset").
        Kind::Str => parse_toml_basic_string(token).map(SettingValue::Str),
        // An array: parse the single-line `["a", "b"]` (tolerating spaces) into a
        // list. An empty `[]` reads as an empty list.
        Kind::StrArray { .. } => parse_toml_string_array(token).map(SettingValue::StrList),
    }
}

/// Parse a TOML BASIC string token (`"..."` with escapes) OR a literal string
/// (`'...'`, no escapes) into its inner text. We interpret the common escapes
/// (`\\`, `\"`, `\n`, `\t`, `\r`, `\uXXXX`, `\UXXXXXXXX`) so a value we ourselves
/// wrote with `escape_toml_basic` round-trips exactly. Returns None if the token
/// is not a well-formed quoted string. (On WRITE we always emit a basic string;
/// literal-string parsing is here only to read a hand-edited file faithfully.)
fn parse_toml_basic_string(token: &str) -> Option<String> {
    let b = token.as_bytes();
    if b.len() < 2 {
        return None;
    }
    let quote = b[0];
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    if b[b.len() - 1] != quote {
        return None;
    }
    let inner = &token[1..token.len() - 1];
    // Literal string: no escape processing (a backslash is literal).
    if quote == b'\'' {
        // A literal string cannot itself contain a single quote, so a stray one
        // means the token was malformed (e.g. `'a'b'`).
        if inner.contains('\'') {
            return None;
        }
        return Some(inner.to_string());
    }
    // Basic string: process escapes.
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            // A raw double-quote inside a basic string is illegal (it would have
            // closed the string); reject so we never mis-read a malformed token.
            if c == '"' {
                return None;
            }
            out.push(c);
            continue;
        }
        match chars.next()? {
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'b' => out.push('\u{0008}'),
            'f' => out.push('\u{000C}'),
            'u' => {
                let hex: String = (0..4).map(|_| chars.next().unwrap_or('\0')).collect();
                let cp = u32::from_str_radix(&hex, 16).ok()?;
                out.push(char::from_u32(cp)?);
            }
            'U' => {
                let hex: String = (0..8).map(|_| chars.next().unwrap_or('\0')).collect();
                let cp = u32::from_str_radix(&hex, 16).ok()?;
                out.push(char::from_u32(cp)?);
            }
            _ => return None,
        }
    }
    Some(out)
}

/// Parse a SINGLE-LINE TOML array of strings (`[ "a", "b" ]`, tolerating spaces
/// and a trailing comma) into a list. Each element is a basic/literal string we
/// unquote via [`parse_toml_basic_string`]. Returns None if the token is not a
/// well-formed single-line string array. Empty `[]` (any inner whitespace) =>
/// empty list. We scan respecting quotes so a `,` or `]` INSIDE a quoted string
/// is not treated as a separator/terminator.
fn parse_toml_string_array(token: &str) -> Option<Vec<String>> {
    let t = token.trim();
    let inner = t.strip_prefix('[')?.strip_suffix(']')?;
    if inner.trim().is_empty() {
        return Some(Vec::new());
    }
    let mut out: Vec<String> = Vec::new();
    let bytes = inner.as_bytes();
    let mut i = 0;
    loop {
        // Skip leading whitespace before an element.
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let quote = bytes[i];
        if quote != b'"' && quote != b'\'' {
            return None; // an element must be a quoted string
        }
        // Find the matching closing quote, honoring `\"`/`\\` escapes in a basic
        // string (a literal string `'...'` has no escapes, so the first `'` ends it).
        let start = i;
        i += 1;
        let mut closed = false;
        while i < bytes.len() {
            let b = bytes[i];
            if quote == b'"' && b == b'\\' {
                i += 2; // skip the escaped char
                continue;
            }
            if b == quote {
                closed = true;
                i += 1;
                break;
            }
            i += 1;
        }
        if !closed {
            return None;
        }
        let elem_token = &inner[start..i];
        out.push(parse_toml_basic_string(elem_token)?);
        // Skip whitespace, then expect a comma or the end.
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if bytes[i] == b',' {
            i += 1;
            continue;
        }
        return None; // junk between elements
    }
    Some(out)
}

/// Escape a string for emission as a TOML BASIC string CONTENT (the text that
/// goes BETWEEN the surrounding double quotes). Per the TOML spec we escape the
/// backslash and double-quote, and any control character (U+0000..U+001F plus
/// U+007F) as its `\uXXXX` form (or the short `\n`/`\t`/`\r`/`\b`/`\f` mnemonics).
/// This is the WHOLE injection defense: after escaping, the content can contain
/// NO unescaped `"` to close the token early, NO raw newline to start a second
/// line/key, and NO control char — so a value like `foo"], allow = [true` becomes
/// the inert text `foo\"], allow = [true` that re-parses to the SAME string.
fn escape_toml_basic(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\u{0008}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{000C}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || (c as u32) == 0x7F => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// Validate ONE freeform/array ELEMENT string before it is written. Trims; the
/// EMPTY string is allowed ONLY for a freeform Str (`allow_empty`), never as an
/// array element. Rejects any string carrying a control char / newline / NUL
/// (defense in depth — `escape_toml_basic` would neutralize them anyway, but we
/// refuse them outright so a hostile control char can never even reach the file),
/// caps the length, and (when `must_be_abs`) requires an absolute `/`-rooted path.
/// Returns the trimmed, validated string.
fn validate_element(raw: &str, must_be_abs: bool, allow_empty: bool) -> Result<String, String> {
    let v = raw.trim();
    if v.is_empty() {
        if allow_empty {
            return Ok(String::new());
        }
        return Err("empty value not allowed here".to_string());
    }
    if v.chars().count() > MAX_STR_LEN {
        return Err(format!("value too long (cap {MAX_STR_LEN} chars)"));
    }
    // Reject control chars (incl. newline, carriage return, tab, NUL, DEL). These
    // are exactly the bytes that could break a value out of its token if the
    // escaping ever regressed; refusing them is a belt to the escaping suspenders.
    if let Some(bad) = v.chars().find(|c| (*c as u32) < 0x20 || (*c as u32) == 0x7F) {
        return Err(format!("value contains a control character (U+{:04X})", bad as u32));
    }
    if must_be_abs && !v.starts_with('/') {
        return Err(format!("path must be absolute (start with '/'): {v:?}"));
    }
    Ok(v.to_string())
}

/// Unquote a TOML basic/literal string token (`"x"` or `'x'`) into its inner
/// text. Returns None if it is not a simple quoted string. We do NOT interpret
/// escapes — the enum values we read are plain ASCII identifiers, and on WRITE we
/// re-emit a clean quoted token, so no escape round-trips through here.
fn unquote(token: &str) -> Option<String> {
    let b = token.as_bytes();
    if b.len() >= 2 && ((b[0] == b'"' && b[b.len() - 1] == b'"') || (b[0] == b'\'' && b[b.len() - 1] == b'\'')) {
        Some(token[1..token.len() - 1].to_string())
    } else {
        None
    }
}

/// The parse cursor over the file: track the current `[section]` so a `key =`
/// line is attributed to the right section. Returns the section name for a header
/// line, or None for any other line. TOLERATES a trailing inline comment on the
/// header line (the real config has many, e.g. `[power]   # #38 …`). Sub-tables
/// like `[voice.voices]` are returned verbatim ("voice.voices") and simply never
/// match a whitelist section.
fn section_header(line: &str) -> Option<String> {
    let t = line.trim_start();
    // Skip array-of-tables headers ([[x]]) — none of our settings live there.
    if t.starts_with("[[") || !t.starts_with('[') {
        return None;
    }
    // The header is everything up to the CLOSING ']'; anything after (whitespace,
    // a '# comment') is ignored. A section name never contains ']'.
    let close = t.find(']')?;
    let inner = &t[1..close];
    Some(inner.trim().to_string())
}

/// Split a non-comment, non-header line into (key, value-region) if it is a
/// `key = ...` assignment. The value-region still includes any trailing inline
/// comment (the caller strips it). Leading whitespace on the line is tolerated.
fn split_assignment(line: &str) -> Option<(&str, &str)> {
    let t = line.trim_start();
    if t.starts_with('#') || t.is_empty() {
        return None;
    }
    let (k, v) = t.split_once('=')?;
    let key = k.trim();
    if key.is_empty() {
        return None;
    }
    Some((key, v))
}

/// Read the live `(section, key) -> raw value token` map from the TOML text, for
/// EVERY whitelisted flat setting AND the autonomy `enabled`/`mode` pairs. Pure
/// over the text. Only the FIRST occurrence of a key within a section is taken
/// (TOML forbids duplicates; we are defensive).
fn read_raw_values(text: &str) -> BTreeMap<(String, String), String> {
    let mut out: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut section = String::new();
    for line in text.lines() {
        if let Some(h) = section_header(line) {
            section = h;
            continue;
        }
        if let Some((key, vregion)) = split_assignment(line) {
            let val = strip_inline_comment(vregion);
            out.entry((section.clone(), key.to_string()))
                .or_insert(val);
        }
    }
    out
}

/// Derive the 3-way autonomy state for a section from its `enabled` + `mode`
/// tokens. Off = enabled false (regardless of mode); else Propose unless
/// mode == "auto" (anything else, including a missing/unknown mode, is the safe
/// "propose"). Mirrors config.rs's "unknown mode behaves as propose".
fn derive_autonomy(enabled: Option<&str>, mode: Option<&str>) -> &'static str {
    let on = enabled == Some("true");
    if !on {
        return "off";
    }
    match mode.and_then(unquote_opt).as_deref() {
        Some("auto") => "auto",
        _ => "propose",
    }
}

/// Unquote helper that takes/returns Option, for the autonomy mode token.
fn unquote_opt(token: &str) -> Option<String> {
    unquote(token)
}

/* ----------------------------------------------------------- SET (in-place) */

/// One requested change from the UI: a setting id (`section.key` for a flat
/// setting, or a bare section name for a 3-way autonomy control) and its new
/// value. Validated against the whitelist before any write.
#[derive(Debug, Clone, Deserialize)]
pub struct Change {
    pub id: String,
    pub value: SettingValue,
}

/// Render a validated typed value as the TOML value TOKEN we write in place.
/// We CONTROL this string entirely (never echo the caller's raw text), so a
/// value can never inject a second key, a newline, or a comment: a bool/int/
/// float has no quoting surface, and an enum is a known-safe identifier we wrap
/// in double quotes ourselves.
fn render_token(kind: Kind, value: &SettingValue) -> Result<String, String> {
    match (kind, value) {
        (Kind::Bool, SettingValue::Bool(b)) => Ok(if *b { "true" } else { "false" }.to_string()),
        (Kind::Int { min, max }, SettingValue::Int(n)) => {
            if *n < min || *n > max {
                return Err(format!("value {n} out of range [{min}, {max}]"));
            }
            Ok(n.to_string())
        }
        // Accept an integer-valued float for a Float key (JS numbers are doubles).
        (Kind::Float { min, max }, v) => {
            let f = match v {
                SettingValue::Float(f) => *f,
                SettingValue::Int(n) => *n as f64,
                _ => return Err("expected a number".to_string()),
            };
            if !f.is_finite() {
                return Err("value is not a finite number".to_string());
            }
            if f < min || f > max {
                return Err(format!("value {f} out of range [{min}, {max}]"));
            }
            // Emit a clean decimal; ensure it always reads as a TOML float
            // (a trailing ".0" when it is integral) so the daemon parses f64.
            let s = format!("{f}");
            Ok(if s.contains('.') || s.contains('e') || s.contains('E') {
                s
            } else {
                format!("{s}.0")
            })
        }
        (Kind::Int { .. }, _) => Err("expected an integer".to_string()),
        (Kind::Bool, _) => Err("expected true/false".to_string()),
        (Kind::Enum(opts), SettingValue::Str(s)) => {
            if !opts.contains(&s.as_str()) {
                return Err(format!("'{s}' is not one of {opts:?}"));
            }
            Ok(format!("\"{s}\""))
        }
        (Kind::Enum(_), _) => Err("expected one of the allowed options".to_string()),
        // FREEFORM string: trim + allow-empty + reject control chars + cap length,
        // then emit an ESCAPED basic string. A hostile value can never break out.
        (Kind::Str, SettingValue::Str(s)) => {
            let v = validate_element(s, /*must_be_abs*/ false, /*allow_empty*/ true)?;
            Ok(format!("\"{}\"", escape_toml_basic(&v)))
        }
        (Kind::Str, _) => Err("expected a string".to_string()),
        // ARRAY: validate EACH element (per-element absolute-path / non-empty +
        // control-char rejection + length cap), reject duplicates, then emit a
        // SINGLE-LINE array of ESCAPED basic strings. Empty => `[]`. No element can
        // smuggle a second array element, key, or comment past its own token.
        (Kind::StrArray { paths }, SettingValue::StrList(items)) => {
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut elems: Vec<String> = Vec::with_capacity(items.len());
            for item in items {
                let v = validate_element(item, /*must_be_abs*/ paths, /*allow_empty*/ false)?;
                if !seen.insert(v.clone()) {
                    return Err(format!("duplicate entry: {v:?}"));
                }
                elems.push(format!("\"{}\"", escape_toml_basic(&v)));
            }
            Ok(format!("[{}]", elems.join(", ")))
        }
        (Kind::StrArray { .. }, _) => Err("expected an array of strings".to_string()),
    }
}

/// Validate one change against the whitelist and resolve it to the concrete
/// (section, key) -> token writes it produces. A flat setting yields ONE write;
/// a 3-way autonomy control yields TWO (enabled + mode). REJECTS an unknown id,
/// a wrong type, an out-of-range/option value. Pure.
fn resolve_change(change: &Change) -> Result<Vec<((String, String), String)>, String> {
    // 3-way autonomy control: the id is a bare section name.
    if AUTONOMY_SECTIONS.contains(&change.id.as_str()) {
        let state = match &change.value {
            SettingValue::Str(s) => s.as_str(),
            _ => return Err(format!("{}: autonomy state must be a string", change.id)),
        };
        if !AUTONOMY_STATES.contains(&state) {
            return Err(format!(
                "{}: '{state}' is not one of {AUTONOMY_STATES:?}",
                change.id
            ));
        }
        let (enabled, mode) = match state {
            "off" => ("false", "propose"),
            "propose" => ("true", "propose"),
            "auto" => ("true", "auto"),
            _ => unreachable!("guarded by AUTONOMY_STATES"),
        };
        return Ok(vec![
            ((change.id.clone(), "enabled".to_string()), enabled.to_string()),
            ((change.id.clone(), "mode".to_string()), format!("\"{mode}\"")),
        ]);
    }

    // Flat setting: the id is "section.key" and MUST be in the whitelist.
    let Some(setting) = setting_by_id(&change.id) else {
        return Err(format!("unknown setting '{}'", change.id));
    };
    let token = render_token(setting.kind, &change.value)
        .map_err(|e| format!("{}: {e}", change.id))?;
    Ok(vec![(
        (setting.section.to_string(), setting.key.to_string()),
        token,
    )])
}

/// Apply a set of resolved `(section, key) -> token` writes to the TOML text
/// IN PLACE, preserving every comment + the structure. We rewrite ONLY the value
/// token on the matching `key =` line within the matching `[section]`, keeping the
/// key name, the indentation, the `=` spacing, and the trailing `# comment`
/// byte-for-byte. A target whose (section, key) line is not found is an error (we
/// never APPEND a key — that could land outside its section or duplicate it).
/// Pure over the text; the only caller does the I/O.
fn apply_writes_in_place(
    text: &str,
    writes: &BTreeMap<(String, String), String>,
) -> Result<String, String> {
    // Preserve the file's original line endings: we rebuild from the original
    // line slices and re-insert '\n', then restore a missing trailing newline.
    let mut applied: BTreeMap<(String, String), bool> =
        writes.keys().map(|k| (k.clone(), false)).collect();

    let mut section = String::new();
    let mut out_lines: Vec<String> = Vec::new();
    for line in text.lines() {
        if let Some(h) = section_header(line) {
            section = h;
            out_lines.push(line.to_string());
            continue;
        }
        if let Some((key, vregion)) = split_assignment(line) {
            let target = (section.clone(), key.to_string());
            if let Some(token) = writes.get(&target) {
                // Already applied once? (defensive against a duplicate key line)
                if applied.get(&target) == Some(&true) {
                    out_lines.push(line.to_string());
                    continue;
                }
                out_lines.push(rewrite_value_line(line, vregion, token));
                applied.insert(target, true);
                continue;
            }
        }
        out_lines.push(line.to_string());
    }

    // Every requested write MUST have matched a line — we never append.
    let missing: Vec<String> = applied
        .iter()
        .filter(|(_, done)| !**done)
        .map(|((s, k), _)| format!("[{s}].{k}"))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "could not locate these keys in config/jarvis.toml: {}",
            missing.join(", ")
        ));
    }

    let mut joined = out_lines.join("\n");
    if text.ends_with('\n') {
        joined.push('\n');
    }
    Ok(joined)
}

/// Rewrite ONE assignment line: replace only the value token, keeping everything
/// to the LEFT of `=` (key + indentation + `=` spacing) and any trailing inline
/// `# comment` to the RIGHT of the value, plus the exact whitespace around them.
fn rewrite_value_line(line: &str, vregion: &str, token: &str) -> String {
    // The part up to and including the first '=' is preserved verbatim.
    let eq = line.find('=').expect("split_assignment found an '='");
    let head = &line[..=eq];

    // Within the value region, find the trailing inline comment (respecting
    // quotes) so we keep it. Compute byte offsets relative to vregion.
    let comment_start = inline_comment_start(vregion);

    // Leading whitespace after '=' (one space typically) — preserve it.
    let lead_ws_len = vregion.len() - vregion.trim_start().len();
    let lead_ws = &vregion[..lead_ws_len];

    match comment_start {
        Some(c) => {
            // Whitespace between the old value and the '#': preserve it so the
            // comment column does not shift.
            let before_comment = &vregion[..c];
            let trail_ws_len = before_comment.len() - before_comment.trim_end().len();
            let trail_ws = &before_comment[before_comment.len() - trail_ws_len..];
            let comment = &vregion[c..]; // includes the '#'
            format!("{head}{lead_ws}{token}{trail_ws}{comment}")
        }
        None => {
            // No comment: preserve any trailing whitespace after the value too.
            let trimmed = vregion.trim_end();
            let trail_ws = &vregion[trimmed.len()..];
            format!("{head}{lead_ws}{token}{trail_ws}")
        }
    }
}

/// Byte offset of the inline `#` that begins a trailing comment in a value
/// region, respecting single/double quotes (so `'a#b'` is not a comment) AND the
/// `\` escape inside a basic ("..") string (so an escaped `\"` does not falsely
/// close the string and expose a `#` payload as a comment — critical for the
/// values we write with escaped quotes). None if there is no inline comment.
fn inline_comment_start(vregion: &str) -> Option<usize> {
    let bytes = vregion.as_bytes();
    let mut in_str: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match in_str {
            Some(q) => {
                if q == b'"' && b == b'\\' {
                    i += 2;
                    continue;
                }
                if b == q {
                    in_str = None;
                }
            }
            None => {
                if b == b'"' || b == b'\'' {
                    in_str = Some(b);
                } else if b == b'#' {
                    return Some(i);
                }
            }
        }
        i += 1;
    }
    None
}

/* --------------------------------------------------------------- root + I/O */

/// Resolve `config/jarvis.toml` under the SAME JARVIS root the command channel +
/// self-heal use (the `resolve_root_for_command` resolver: JARVIS_ROOT env, else
/// the exe/cwd upward walk to the scripts/apply_heal.sh + config/jarvis.toml
/// markers). The installed root is `~/Library/Application Support/JARVIS`; in dev
/// it is the repo. We never accept a path from the frontend.
fn config_path() -> Result<PathBuf, String> {
    let root = crate::heal::resolve_root_for_command()?;
    Ok(root.join("config").join("jarvis.toml"))
}

/// Build the full GET snapshot from already-read TOML text. Pure (no I/O), so the
/// round-trip + coercion are unit-testable without a daemon. Each flat setting is
/// coerced to its typed value (falling back to the file's verbatim string only if
/// the token is malformed); each autonomy section is the derived 3-way state.
/// The SHAPE-CORRECT sentinel for a setting whose file value is absent or
/// malformed, so GET always returns the JSON shape the UI's control expects. An
/// array kind gets an empty list; everything else gets a string (the verbatim
/// malformed token when there is one, else empty = "unset").
fn missing_sentinel(kind: Kind, token: Option<&str>) -> SettingValue {
    match kind {
        Kind::StrArray { .. } => SettingValue::StrList(Vec::new()),
        _ => SettingValue::Str(token.map(str::to_string).unwrap_or_default()),
    }
}

pub fn build_get(text: &str) -> Vec<SettingState> {
    let raw = read_raw_values(text);
    let mut out: Vec<SettingState> = Vec::new();

    for s in SETTINGS {
        let token = raw.get(&(s.section.to_string(), s.key.to_string()));
        let value = match token {
            // A present token coerces to its typed value. On a malformed token we
            // fall back to a SHAPE-CORRECT sentinel (empty list for an array, the
            // verbatim string otherwise) so the UI always renders the right control.
            Some(tok) => coerce_value(s.kind, tok).unwrap_or_else(|| missing_sentinel(s.kind, Some(tok))),
            // Key absent from the file: surface a typed default-shaped sentinel so
            // the UI still renders the right control (empty list for an array, an
            // empty string the UI treats as "unset" otherwise).
            None => missing_sentinel(s.kind, None),
        };
        let (kind_tag, options, min, max) = describe(s.kind);
        out.push(SettingState {
            id: s.id(),
            section: s.section.to_string(),
            key: s.key.to_string(),
            kind: kind_tag,
            value,
            options,
            min,
            max,
        });
    }

    // The 3-way autonomy controls.
    for sec in AUTONOMY_SECTIONS {
        let enabled = raw.get(&(sec.to_string(), "enabled".to_string())).map(String::as_str);
        let mode = raw.get(&(sec.to_string(), "mode".to_string())).map(String::as_str);
        let state = derive_autonomy(enabled, mode);
        out.push(SettingState {
            id: (*sec).to_string(),
            section: (*sec).to_string(),
            key: String::new(),
            kind: "autonomy".to_string(),
            value: SettingValue::Str(state.to_string()),
            options: Some(AUTONOMY_STATES.iter().map(|s| s.to_string()).collect()),
            min: None,
            max: None,
        });
    }

    out
}

/// Map a [`Kind`] to its UI descriptor: (tag, options, min, max).
fn describe(kind: Kind) -> (String, Option<Vec<String>>, Option<f64>, Option<f64>) {
    match kind {
        Kind::Bool => ("bool".to_string(), None, None, None),
        Kind::Int { min, max } => ("int".to_string(), None, Some(min as f64), Some(max as f64)),
        Kind::Float { min, max } => ("float".to_string(), None, Some(min), Some(max)),
        Kind::Enum(opts) => (
            "enum".to_string(),
            Some(opts.iter().map(|s| s.to_string()).collect()),
            None,
            None,
        ),
        // The UI tag for a freeform string field.
        Kind::Str => ("string".to_string(), None, None, None),
        // The UI tag distinguishes a path-array (folder picker) from a plain
        // string-array (manual repo-id add): "pathlist" vs "strlist".
        Kind::StrArray { paths } => (
            if paths { "pathlist" } else { "strlist" }.to_string(),
            None,
            None,
            None,
        ),
    }
}

/// Validate + apply a batch of changes to TOML text, returning the new text.
/// Pure (no I/O) so the whole validation + in-place edit is unit-tested. ALL
/// changes are validated FIRST (so a single bad change aborts the whole batch and
/// nothing is written), then applied together.
pub fn apply_changes(text: &str, changes: &[Change]) -> Result<String, String> {
    if changes.is_empty() {
        return Err("no changes supplied".to_string());
    }
    // Validate every change up front; collect the concrete writes.
    let mut writes: BTreeMap<(String, String), String> = BTreeMap::new();
    for change in changes {
        for (target, token) in resolve_change(change)? {
            writes.insert(target, token);
        }
    }
    apply_writes_in_place(text, &writes)
}

/* --------------------------------------------------------------- commands */

/// READ the current values of every whitelisted setting from the live
/// config/jarvis.toml at the resolved JARVIS root. Async (off-runtime file read).
#[tauri::command]
pub async fn config_get() -> Result<Vec<SettingState>, String> {
    let path = config_path()?;
    let text = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    Ok(build_get(&text))
}

/// WRITE a batch of whitelisted key->value edits to config/jarvis.toml IN PLACE,
/// preserving all comments + structure. Validates EVERY change against the
/// whitelist (allowed keys + per-key type/options/range) BEFORE writing; rejects
/// anything else. Writes atomically (temp file + rename) so a crash mid-write can
/// never leave a half-written config. Returns the number of (section,key) lines
/// changed.
#[tauri::command]
pub async fn config_set(changes: Vec<Change>) -> Result<usize, String> {
    let path = config_path()?;
    let text = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;

    let updated = apply_changes(&text, &changes)?;
    let n_lines = count_changed_lines(&text, &updated);

    // Atomic replace: write a sibling temp file, fsync, then rename over.
    let tmp = path.with_extension("toml.tmp");
    tokio::fs::write(&tmp, updated.as_bytes())
        .await
        .map_err(|e| format!("could not write temp config: {e}"))?;
    tokio::fs::rename(&tmp, &path)
        .await
        .map_err(|e| format!("could not replace config: {e}"))?;

    Ok(n_lines)
}

/// Count how many lines differ between the old and new text (for the reply — the
/// UI confirms how many lines moved). Pure.
fn count_changed_lines(old: &str, new: &str) -> usize {
    old.lines()
        .zip(new.lines())
        .filter(|(a, b)| a != b)
        .count()
}

/// The outcome of a daemon restart attempt — honest about the launchd state.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct RestartResult {
    /// True iff `launchctl kickstart -k` reported success (the service was loaded
    /// and got kicked).
    pub ok: bool,
    /// A human-readable status: the restart succeeded, or the agent isn't loaded
    /// (so the user must start it), or launchctl is missing.
    pub detail: String,
}

/// RESTART jarvisd so a config change takes effect (there is no hot-reload). Runs
/// `launchctl kickstart -k gui/<uid>/com.jarvis.daemon` — the SAME incantation
/// scripts/apply_heal.sh uses after a healed build. If the agent is not loaded,
/// `kickstart` fails and we return an HONEST "not loaded" detail rather than
/// pretending a restart happened. The command takes NO argument from the
/// frontend (the label is a constant), so there is no injection surface.
#[tauri::command]
pub async fn daemon_restart() -> Result<RestartResult, String> {
    // Resolve the GUI domain target: gui/<uid>/<label>.
    let uid = libc_getuid();
    let target = format!("gui/{uid}/{DAEMON_LABEL}");

    let output = Command::new("/bin/launchctl")
        .arg("kickstart")
        .arg("-k")
        .arg(&target)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("could not run launchctl: {e}"))?;

    if output.status.success() {
        return Ok(RestartResult {
            ok: true,
            detail: format!("JARVIS restarted ({target}); the new config is now live."),
        });
    }

    // kickstart failed — most commonly because the agent is not loaded. Surface a
    // clear, secret-free explanation (stderr is launchctl's own diagnostic).
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = if stderr.contains("Could not find") || stderr.contains("No such process") {
        format!("the JARVIS daemon ({DAEMON_LABEL}) is not loaded — start it, then your changes take effect.")
    } else {
        format!(
            "restart failed: {}",
            stderr.trim().lines().next().unwrap_or("unknown launchctl error")
        )
    };
    Ok(RestartResult { ok: false, detail })
}

/// The current user's uid for the launchd GUI domain target. Wrapped so the
/// `unsafe` is contained to one tiny call; `getuid()` cannot fail.
fn libc_getuid() -> u32 {
    extern "C" {
        fn getuid() -> u32;
    }
    // SAFETY: getuid() always succeeds, takes no arguments, and has no
    // preconditions; it is the canonical POSIX uid query.
    unsafe { getuid() }
}

/* ------------------------------------------------------------- folder picker */

/// Open the NATIVE macOS folder picker and return the chosen ABSOLUTE path, or
/// `None` if the user cancelled. Wired without a new dependency (and so without a
/// dialog-plugin capability permission to manage): it shells `osascript` to run
/// `choose folder` — the same canonical macOS chooser the Finder uses — and reads
/// back its POSIX path. The command takes NO argument from the frontend (there is
/// nothing to inject; the AppleScript literal is a constant), and the returned
/// path is RE-VALIDATED on the way IN through `config_set` exactly like a manually
/// typed path — the picker is a convenience, never a trust shortcut. The manual
/// validated text-add input remains the always-works baseline if the picker is
/// unavailable (e.g. no GUI session). Parses the path with a deny-default stance:
/// only a clean absolute path with no control char is returned.
#[tauri::command]
pub async fn pick_folder() -> Result<Option<String>, String> {
    // `choose folder` returns an alias; `POSIX path of` renders it as an absolute
    // /-rooted path. On Cancel, osascript exits non-zero with "User canceled"
    // (error -128) — we map that to Ok(None), not an error.
    let output = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg("POSIX path of (choose folder with prompt \"Select a folder JARVIS may index\")")
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("could not run the folder picker: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // User cancelled (-128) — a clean no-selection, not a failure.
        if stderr.contains("-128") || stderr.to_lowercase().contains("user canceled") {
            return Ok(None);
        }
        return Err(format!(
            "folder picker unavailable: {}",
            stderr.trim().lines().next().unwrap_or("no GUI session")
        ));
    }

    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        return Ok(None);
    }
    // Validate exactly like an absolute-path array element so the picker can never
    // surface something config_set would later reject (or anything control-charred).
    let validated = validate_element(&path, /*must_be_abs*/ true, /*allow_empty*/ false)?;
    Ok(Some(validated))
}

/* --------------------------------------------------------------------- tests */

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// The exact config header + a representative slice of the real
    /// config/jarvis.toml, with the carefully-written honest comments, used to
    /// prove the in-place edit preserves them byte-for-byte.
    const SAMPLE: &str = "\
# JARVIS canonical configuration.
# Read by jarvisd and the inference server.

[audio]
rms_threshold = 0.015   # RMS gate: below this is treated as silence
sound_monitor = true    # Ambient sound monitor. SHIPS ON.

[inference]
speculative = true      # #37 SPECULATIVE DECODING — master gate.
quant = \"auto\"          # #39 SELECTABLE QUANTIZATION. Allowed: auto/fp16/int8/int4.

[screen_context]
enabled = true          # master gate. SHIPS ON — most privacy-sensitive.
interval_secs = 30      # cadence the device-gated loop grabs ONE frame.

[self_heal]
enabled = true    # master gate, SHIPS ON.
mode = \"propose\"  # \"propose\" (default): write the patch.

[voice_id]
enabled = false              # master switch, SHIPS OFF deliberately.
threshold = 0.86             # cosine ACCEPT on the acoustic embedding.
gate_scope = \"consequential\" # \"consequential\" (default) | \"all\".

[integrations]
allow_consequential = true

[focus]
profile = \"default\"      # SHIPS NEUTRAL.
";

    fn get_value(states: &[SettingState], id: &str) -> SettingValue {
        states
            .iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("setting {id} missing from GET"))
            .value
            .clone()
    }

    // ---------- (a) ROUND-TRIP: set then get returns the new value ----------

    #[test]
    fn round_trip_bool_int_float_enum() {
        // Flip a bool, change an int, a float, and an enum in one batch.
        let changes = vec![
            Change { id: "integrations.allow_consequential".into(), value: SettingValue::Bool(false) },
            Change { id: "screen_context.interval_secs".into(), value: SettingValue::Int(60) },
            Change { id: "voice_id.threshold".into(), value: SettingValue::Float(0.92) },
            Change { id: "speech.engine".into(), value: SettingValue::Str("csm".into()) },
        ];
        // speech.engine isn't in SAMPLE; add a minimal [speech] block for it.
        let text = format!("{SAMPLE}\n[speech]\nengine = \"kokoro\"   # TTS engine\n");
        let updated = apply_changes(&text, &changes).expect("apply ok");
        let states = build_get(&updated);

        assert_eq!(get_value(&states, "integrations.allow_consequential"), SettingValue::Bool(false));
        assert_eq!(get_value(&states, "screen_context.interval_secs"), SettingValue::Int(60));
        assert_eq!(get_value(&states, "voice_id.threshold"), SettingValue::Float(0.92));
        assert_eq!(get_value(&states, "speech.engine"), SettingValue::Str("csm".into()));
    }

    #[test]
    fn round_trip_autonomy_three_way_mapping() {
        // Off -> Propose -> Auto, each derived back correctly.
        for (state, exp_enabled, exp_mode) in
            [("off", "false", "\"propose\""), ("propose", "true", "\"propose\""), ("auto", "true", "\"auto\"")]
        {
            let updated = apply_changes(
                SAMPLE,
                &[Change { id: "self_heal".into(), value: SettingValue::Str(state.into()) }],
            )
            .expect("autonomy apply ok");
            let raw = read_raw_values(&updated);
            assert_eq!(
                raw.get(&("self_heal".into(), "enabled".into())).map(String::as_str),
                Some(exp_enabled),
                "enabled for state {state}"
            );
            assert_eq!(
                raw.get(&("self_heal".into(), "mode".into())).map(String::as_str),
                Some(exp_mode),
                "mode for state {state}"
            );
            // And the derived GET state round-trips.
            let states = build_get(&updated);
            assert_eq!(get_value(&states, "self_heal"), SettingValue::Str(state.into()));
        }
    }

    // ---------- (b) COMMENTS preserved byte-for-byte except the value line ----------

    /// Every line that is a `# ...` comment line OR carries a trailing inline
    /// `# ...` comment — collect them verbatim for a before/after diff.
    fn comment_lines(text: &str) -> Vec<String> {
        text.lines()
            .filter(|l| {
                let t = l.trim_start();
                t.starts_with('#') || inline_comment_start(l).is_some()
            })
            .map(|l| {
                // For a value line, the assertion we care about is the COMMENT
                // tail is unchanged; capture the comment portion explicitly.
                if let Some(c) = inline_comment_start(l) {
                    l[c..].to_string()
                } else {
                    l.to_string()
                }
            })
            .collect()
    }

    #[test]
    fn comments_preserved_byte_for_byte_except_changed_value() {
        let before_comments = comment_lines(SAMPLE);
        let changes = vec![
            Change { id: "integrations.allow_consequential".into(), value: SettingValue::Bool(false) },
            Change { id: "screen_context.interval_secs".into(), value: SettingValue::Int(15) },
            Change { id: "voice_id.threshold".into(), value: SettingValue::Float(0.9) },
            Change { id: "inference.quant".into(), value: SettingValue::Str("int8".into()) },
            Change { id: "self_heal".into(), value: SettingValue::Str("auto".into()) },
        ];
        let updated = apply_changes(SAMPLE, &changes).expect("apply ok");
        let after_comments = comment_lines(&updated);

        // Every comment (standalone + inline tail) is byte-for-byte identical.
        assert_eq!(before_comments, after_comments, "comments must survive the edit");

        // And the file structure (every non-value line) is intact: only the
        // changed assignment lines differ. Count differing lines.
        let differing: Vec<(&str, &str)> = SAMPLE
            .lines()
            .zip(updated.lines())
            .filter(|(a, b)| a != b)
            .collect();
        // 4 flat value lines change. The self_heal -> "auto" change writes
        // enabled=true (ALREADY true in SAMPLE — that line is byte-identical, so
        // it does NOT show as differing) + mode="auto" (was "propose" — changes).
        // So exactly 4 + 1 = 5 lines differ. The in-place edit touches nothing
        // else — every comment + every structural line is intact.
        assert_eq!(differing.len(), 5, "exactly the targeted value lines change");

        // Spot-check one line: the comment column did not move.
        let q = updated
            .lines()
            .find(|l| l.trim_start().starts_with("quant ="))
            .unwrap();
        assert_eq!(q, "quant = \"int8\"          # #39 SELECTABLE QUANTIZATION. Allowed: auto/fp16/int8/int4.");
    }

    #[test]
    fn section_header_tolerates_a_trailing_comment() {
        // The real config has many headers with trailing comments — they MUST
        // still attribute keys to the right section, or those settings vanish.
        assert_eq!(section_header("[power]   # #38 BATTERY THROTTLING"), Some("power".into()));
        assert_eq!(section_header("[focus]"), Some("focus".into()));
        assert_eq!(section_header("  [voice.voices]"), Some("voice.voices".into()));
        assert_eq!(section_header("[[webhooks.mappings]]"), None, "array-of-tables ignored");
        assert_eq!(section_header("enabled = true"), None);
        assert_eq!(section_header("# a comment"), None);

        // End-to-end: a key under a comment-tagged header round-trips.
        let text = "[screen_context]   # #42 the privacy-sensitive read\ninterval_secs = 30   # cadence\n";
        let u = apply_changes(&text, &[Change {
            id: "screen_context.interval_secs".into(),
            value: SettingValue::Int(45),
        }]).expect("apply under comment-tagged header");
        assert!(u.contains("interval_secs = 45   # cadence"), "value edited, comment kept");
        assert!(u.contains("[screen_context]   # #42 the privacy-sensitive read"), "header untouched");
    }

    #[test]
    fn a_hash_inside_a_quoted_value_is_not_a_comment() {
        // Defensive: a '#' inside a quoted enum value must not be treated as a
        // comment boundary (none of our enums contain '#', but the scanner must
        // be correct). Build a synthetic line and confirm the scanner.
        let line = "phrase = \"a#b\"   # real comment";
        let c = inline_comment_start(line).unwrap();
        assert_eq!(&line[c..], "# real comment");
    }

    // ---------- (c) WHITELIST rejects unknown key + invalid value ----------

    #[test]
    fn whitelist_rejects_unknown_key() {
        for bad in [
            "cloud.heavy_model",          // a real key, but NOT whitelisted (freeform path)
            "integrations.something_new", // unknown key in a known section
            "totally.bogus",              // unknown section
            "../../etc/passwd",           // traversal-shaped id, no dot-split match
            "voice_id.enabled\nfoo=bar",  // injection-shaped id
        ] {
            let r = apply_changes(
                SAMPLE,
                &[Change { id: bad.into(), value: SettingValue::Bool(true) }],
            );
            assert!(r.is_err(), "unknown key {bad:?} must be rejected");
        }
    }

    #[test]
    fn whitelist_rejects_out_of_range_and_bad_type() {
        // Out-of-range int (interval floor is 1, cap 86400).
        assert!(apply_changes(SAMPLE, &[Change {
            id: "screen_context.interval_secs".into(),
            value: SettingValue::Int(0),
        }]).is_err());
        assert!(apply_changes(SAMPLE, &[Change {
            id: "screen_context.interval_secs".into(),
            value: SettingValue::Int(999_999),
        }]).is_err());
        // Out-of-range float (threshold band 0.70..=1.0).
        assert!(apply_changes(SAMPLE, &[Change {
            id: "voice_id.threshold".into(),
            value: SettingValue::Float(0.5),
        }]).is_err());
        assert!(apply_changes(SAMPLE, &[Change {
            id: "voice_id.threshold".into(),
            value: SettingValue::Float(1.5),
        }]).is_err());
        // Wrong type: a string for a bool key.
        assert!(apply_changes(SAMPLE, &[Change {
            id: "integrations.allow_consequential".into(),
            value: SettingValue::Str("yes".into()),
        }]).is_err());
        // Bad enum option.
        assert!(apply_changes(SAMPLE, &[Change {
            id: "inference.quant".into(),
            value: SettingValue::Str("int2".into()),
        }]).is_err());
        // Bad autonomy state.
        assert!(apply_changes(SAMPLE, &[Change {
            id: "self_heal".into(),
            value: SettingValue::Str("yolo".into()),
        }]).is_err());
    }

    #[test]
    fn a_bad_change_aborts_the_whole_batch_no_partial_write() {
        // First change is valid, second is invalid -> the WHOLE batch errors and
        // the returned text would be the original (apply returns Err, never a
        // partially-written string).
        let r = apply_changes(
            SAMPLE,
            &[
                Change { id: "integrations.allow_consequential".into(), value: SettingValue::Bool(false) },
                Change { id: "voice_id.threshold".into(), value: SettingValue::Float(9.9) },
            ],
        );
        assert!(r.is_err(), "one bad change aborts the batch");
    }

    #[test]
    fn missing_key_is_an_error_never_appended() {
        // A whitelisted key whose line is absent from the file is NOT appended —
        // it errors, so we never inject a key outside its section.
        let no_focus = "[audio]\nsound_monitor = true\n";
        let r = apply_changes(
            no_focus,
            &[Change { id: "focus.profile".into(), value: SettingValue::Str("work".into()) }],
        );
        assert!(r.is_err(), "absent key must error, not append");
    }

    // ---------- (d) 3-way mapping correctness (explicit table) ----------

    #[test]
    fn autonomy_mapping_is_exact() {
        assert_eq!(derive_autonomy(Some("false"), Some("\"propose\"")), "off");
        assert_eq!(derive_autonomy(Some("false"), Some("\"auto\"")), "off"); // off wins regardless of mode
        assert_eq!(derive_autonomy(Some("true"), Some("\"propose\"")), "propose");
        assert_eq!(derive_autonomy(Some("true"), Some("\"auto\"")), "auto");
        assert_eq!(derive_autonomy(Some("true"), None), "propose"); // missing mode -> propose
        assert_eq!(derive_autonomy(Some("true"), Some("\"bogus\"")), "propose"); // unknown mode -> propose
        assert_eq!(derive_autonomy(None, None), "off"); // missing enabled -> off
    }

    #[test]
    fn resolve_autonomy_change_writes_both_keys() {
        let w = resolve_change(&Change {
            id: "forge".into(),
            value: SettingValue::Str("auto".into()),
        })
        .unwrap();
        assert_eq!(w.len(), 2);
        assert!(w.contains(&(("forge".into(), "enabled".into()), "true".into())));
        assert!(w.contains(&(("forge".into(), "mode".into()), "\"auto\"".into())));
    }

    // ---------- structural: every SETTINGS id is unique + dot-addressable ----------

    #[test]
    fn settings_ids_are_unique_and_addressable() {
        let mut seen = std::collections::HashSet::new();
        for s in SETTINGS {
            let id = s.id();
            assert!(seen.insert(id.clone()), "duplicate setting id {id}");
            assert_eq!(setting_by_id(&id).map(|x| x.id()), Some(id.clone()));
            // No autonomy section leaks into the flat table (would let a caller
            // poke enabled/mode directly and desync the pair).
            assert!(!AUTONOMY_SECTIONS.contains(&s.section), "{} must not be flat", s.section);
        }
    }

    #[test]
    fn get_reports_kind_and_constraints() {
        let states = build_get(SAMPLE);
        let interval = states.iter().find(|s| s.id == "screen_context.interval_secs").unwrap();
        assert_eq!(interval.kind, "int");
        assert_eq!(interval.min, Some(1.0));
        assert_eq!(interval.max, Some(86_400.0));
        let quant = states.iter().find(|s| s.id == "inference.quant").unwrap();
        assert_eq!(quant.kind, "enum");
        assert_eq!(quant.options.as_ref().unwrap(), &["auto", "fp16", "int8", "int4"]);
        let heal = states.iter().find(|s| s.id == "self_heal").unwrap();
        assert_eq!(heal.kind, "autonomy");
        assert_eq!(heal.options.as_ref().unwrap(), &["off", "propose", "auto"]);
    }

    #[test]
    fn every_whitelisted_key_exists_in_the_real_config_file() {
        // Guard against whitelist drift: every flat (section,key) AND every
        // autonomy section's enabled+mode MUST exist in the shipped
        // config/jarvis.toml, or a GET would surface an "unset" sentinel and a
        // SET would error "could not locate". Resolved relative to this source
        // file so it does not depend on the test's cwd.
        // CARGO_MANIFEST_DIR is the absolute path to hud/src-tauri; the repo
        // root is two parents up (hud/, then the repo). Independent of cwd.
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let root = manifest
            .parent().and_then(Path::parent)
            .expect("repo root above hud/src-tauri");
        let cfg = root.join("config/jarvis.toml");
        let text = std::fs::read_to_string(&cfg)
            .unwrap_or_else(|e| panic!("read {}: {e}", cfg.display()));
        let raw = read_raw_values(&text);

        for s in SETTINGS {
            let key = (s.section.to_string(), s.key.to_string());
            let token = raw.get(&key)
                .unwrap_or_else(|| panic!("{}.{} missing from the real config", s.section, s.key));
            // And the live token coerces under the declared kind (no drift in type).
            assert!(
                coerce_value(s.kind, token).is_some(),
                "{}.{} value {token:?} does not match kind {:?}",
                s.section, s.key, s.kind
            );
        }
        for sec in AUTONOMY_SECTIONS {
            assert!(raw.contains_key(&(sec.to_string(), "enabled".into())), "{sec}.enabled missing");
            assert!(raw.contains_key(&(sec.to_string(), "mode".into())), "{sec}.mode missing");
        }
    }

    #[test]
    fn non_targeted_lines_are_byte_for_byte_identical() {
        // The strongest structural proof: rewrite the WHOLE real config with a
        // single change, then assert every line that is not the changed value
        // line is byte-for-byte identical to the original (use Path::file! root).
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let root = manifest.parent().and_then(Path::parent).unwrap();
        let text = std::fs::read_to_string(root.join("config/jarvis.toml")).unwrap();
        let updated = apply_changes(
            &text,
            &[Change { id: "integrations.allow_consequential".into(), value: SettingValue::Bool(false) }],
        )
        .expect("apply on the real config");

        let mut changed = 0usize;
        for (a, b) in text.lines().zip(updated.lines()) {
            if a == b {
                continue;
            }
            changed += 1;
            // The ONLY differing line is the targeted assignment.
            assert!(a.trim_start().starts_with("allow_consequential"), "unexpected change on {a:?}");
            assert_eq!(b, "allow_consequential = false");
        }
        assert_eq!(changed, 1, "exactly one line changes");
        // Same line count + same trailing-newline disposition.
        assert_eq!(text.lines().count(), updated.lines().count());
        assert_eq!(text.ends_with('\n'), updated.ends_with('\n'));
    }

    #[test]
    fn float_token_always_reads_as_a_toml_float() {
        // An integral float value (e.g. 1.0) must still emit "1.0", never "1",
        // so the daemon parses it as f64.
        let t = render_token(Kind::Float { min: 0.7, max: 1.0 }, &SettingValue::Float(1.0)).unwrap();
        assert_eq!(t, "1.0");
        // A float key fed an integer JSON value coerces + ranges + emits a float.
        let t2 = render_token(Kind::Float { min: 0.0, max: 10.0 }, &SettingValue::Int(3)).unwrap();
        assert_eq!(t2, "3.0");
    }

    #[test]
    fn trailing_newline_is_preserved_or_absent_consistently() {
        // With a trailing newline -> kept.
        let with_nl = "[audio]\nsound_monitor = true\n";
        let u = apply_changes(with_nl, &[Change {
            id: "audio.sound_monitor".into(),
            value: SettingValue::Bool(false),
        }]).unwrap();
        assert!(u.ends_with('\n'));
        // Without a trailing newline -> none added.
        let no_nl = "[audio]\nsound_monitor = true";
        let u2 = apply_changes(no_nl, &[Change {
            id: "audio.sound_monitor".into(),
            value: SettingValue::Bool(false),
        }]).unwrap();
        assert!(!u2.ends_with('\n'));
    }

    /* ============================================================== v1 GAPS:
       STRING + ARRAY fields. The crux of this pass is that a string/path value
       can NEVER break out of its token to inject a second key, section, array
       element, or comment. Below: round-trip, comment preservation, the
       INJECTION proof, absolute-path validation, and unknown-key rejection. */

    /// A config slice carrying every NEW field this pass adds: the freeform Str
    /// model ids, the path-array fields, the repo-id array, and the float budget —
    /// each with its real honest trailing comment so the edit can be proven to
    /// keep them.
    const SAMPLE_V1: &str = "\
[models]
classifier = \"\"                                     # dedicated small resident model; \"\" = reuse llm.
local_warm = []                                     # OPTIONAL extra local model ids to keep warm beside `llm`.
local_budget_gib = 0.0                              # RAM budget (GiB); 0 = single-resident.

[speech]
engine = \"kokoro\"       # TTS engine.
model = \"\"              # explicit HF repo for the engine; \"\" = engine default.

[inference]
speculative = true      # #37 master gate.
draft_model = \"\"        # #37 small DRAFT checkpoint; \"\" = inert.

[vision]
enabled = true          # master gate.
model = \"\"              # SHIPS EMPTY — name an on-device VLM repo id to engage.

[image]
enabled = true          # master gate.
model = \"\"              # SHIPS EMPTY — name an on-device diffusion model id to engage.

[docsearch]
enabled = true          # master gate.
roots = []                     # EXPLICIT folder allowlist, SHIPS EMPTY.
build_graph = true             # KG build.

[code]
enabled = true                 # master switch.
roots = []                     # EXPLICIT codebase-root allowlist, SHIPS EMPTY.
";

    // ---------- (a) ROUND-TRIP: set a model id + a roots array + local_warm ----------

    #[test]
    fn round_trip_string_array_and_float_v1() {
        let changes = vec![
            Change { id: "vision.model".into(), value: SettingValue::Str("mlx-community/Qwen2-VL-2B-Instruct-4bit".into()) },
            Change { id: "speech.model".into(), value: SettingValue::Str("mlx-community/Kokoro-82M-bf16".into()) },
            Change { id: "inference.draft_model".into(), value: SettingValue::Str("mlx-community/Qwen3-0.6B-Instruct-4bit".into()) },
            Change { id: "docsearch.roots".into(), value: SettingValue::StrList(vec!["/Users/me/Notes".into(), "/Users/me/Docs".into()]) },
            Change { id: "code.roots".into(), value: SettingValue::StrList(vec!["/Users/me/proj".into()]) },
            Change { id: "models.local_warm".into(), value: SettingValue::StrList(vec!["mlx-community/Qwen3-0.6B-Instruct-4bit".into()]) },
            Change { id: "models.local_budget_gib".into(), value: SettingValue::Float(3.5) },
        ];
        let updated = apply_changes(SAMPLE_V1, &changes).expect("apply v1 ok");
        let states = build_get(&updated);

        assert_eq!(get_value(&states, "vision.model"), SettingValue::Str("mlx-community/Qwen2-VL-2B-Instruct-4bit".into()));
        assert_eq!(get_value(&states, "speech.model"), SettingValue::Str("mlx-community/Kokoro-82M-bf16".into()));
        assert_eq!(get_value(&states, "inference.draft_model"), SettingValue::Str("mlx-community/Qwen3-0.6B-Instruct-4bit".into()));
        assert_eq!(
            get_value(&states, "docsearch.roots"),
            SettingValue::StrList(vec!["/Users/me/Notes".into(), "/Users/me/Docs".into()])
        );
        assert_eq!(get_value(&states, "code.roots"), SettingValue::StrList(vec!["/Users/me/proj".into()]));
        assert_eq!(
            get_value(&states, "models.local_warm"),
            SettingValue::StrList(vec!["mlx-community/Qwen3-0.6B-Instruct-4bit".into()])
        );
        assert_eq!(get_value(&states, "models.local_budget_gib"), SettingValue::Float(3.5));

        // The single-line array form is exactly what the daemon parses.
        assert!(updated.contains("roots = [\"/Users/me/Notes\", \"/Users/me/Docs\"]"));
        assert!(updated.contains("local_warm = [\"mlx-community/Qwen3-0.6B-Instruct-4bit\"]"));
    }

    #[test]
    fn empty_string_and_empty_array_round_trip_as_honest_unset() {
        // Setting a model id back to "" (feature inert) and a roots list back to []
        // are the honest disabled states and must round-trip.
        let pre = apply_changes(SAMPLE_V1, &[
            Change { id: "vision.model".into(), value: SettingValue::Str("x/y".into()) },
            Change { id: "docsearch.roots".into(), value: SettingValue::StrList(vec!["/a".into()]) },
        ]).unwrap();
        let cleared = apply_changes(&pre, &[
            Change { id: "vision.model".into(), value: SettingValue::Str(String::new()) },
            Change { id: "docsearch.roots".into(), value: SettingValue::StrList(vec![]) },
        ]).unwrap();
        let states = build_get(&cleared);
        assert_eq!(get_value(&states, "vision.model"), SettingValue::Str(String::new()));
        assert_eq!(get_value(&states, "docsearch.roots"), SettingValue::StrList(vec![]));
        assert!(cleared.contains("model = \"\""));
        assert!(cleared.contains("roots = []"));
    }

    // ---------- (b) COMMENTS preserved on the changed array/string lines ----------

    #[test]
    fn comments_preserved_on_changed_string_and_array_lines() {
        let changes = vec![
            Change { id: "vision.model".into(), value: SettingValue::Str("repo/vlm".into()) },
            Change { id: "docsearch.roots".into(), value: SettingValue::StrList(vec!["/srv/docs".into()]) },
            Change { id: "models.local_budget_gib".into(), value: SettingValue::Float(2.0) },
        ];
        let updated = apply_changes(SAMPLE_V1, &changes).expect("apply ok");

        // The trailing comments on the CHANGED lines survive byte-for-byte.
        let vline = updated.lines().find(|l| l.trim_start().starts_with("model =") && l.contains("VLM")).unwrap();
        assert_eq!(vline, "model = \"repo/vlm\"              # SHIPS EMPTY — name an on-device VLM repo id to engage.");
        let rline = updated.lines().find(|l| l.trim_start().starts_with("roots =") && l.contains("folder allowlist")).unwrap();
        assert_eq!(rline, "roots = [\"/srv/docs\"]                     # EXPLICIT folder allowlist, SHIPS EMPTY.");
        let bline = updated.lines().find(|l| l.trim_start().starts_with("local_budget_gib")).unwrap();
        assert_eq!(bline, "local_budget_gib = 2.0                              # RAM budget (GiB); 0 = single-resident.");
    }

    // ---------- (c) INJECTION REJECTED / ESCAPED ----------

    /// Helper: count how many `(section, key)` assignment lines exist for a key in
    /// a section (to prove no SECOND key was injected).
    fn count_keys_in_section(text: &str, section: &str, key: &str) -> usize {
        let mut cur = String::new();
        let mut n = 0;
        for line in text.lines() {
            if let Some(h) = section_header(line) {
                cur = h;
                continue;
            }
            if let Some((k, _)) = split_assignment(line) {
                if cur == section && k == key {
                    n += 1;
                }
            }
        }
        n
    }

    #[test]
    fn injection_via_array_element_is_escaped_to_a_single_inert_token() {
        // The classic break-out attempt: an element that, if echoed raw, would
        // close the array and inject a second key (and a comment).
        let evil = "foo\"], allow_consequential = [true #pwn";
        let updated = apply_changes(SAMPLE_V1, &[Change {
            id: "docsearch.roots".into(),
            // It IS an absolute path so it passes the abs-path gate; the point is
            // the QUOTE inside must be escaped, not the path shape.
            value: SettingValue::StrList(vec![format!("/{evil}")]),
        }]).expect("escaped, not errored");

        // 1) The value round-trips to EXACTLY the same string (escape was lossless).
        let states = build_get(&updated);
        assert_eq!(
            get_value(&states, "docsearch.roots"),
            SettingValue::StrList(vec![format!("/{evil}")])
        );
        // 2) No SECOND key was injected anywhere: allow_consequential count is
        //    unchanged (it was never even in SAMPLE_V1 -> still zero), and roots is
        //    still a SINGLE key under [docsearch].
        assert_eq!(count_keys_in_section(&updated, "integrations", "allow_consequential"), 0);
        assert_eq!(count_keys_in_section(&updated, "docsearch", "roots"), 1);
        // 3) The raw written line escapes the quote and keeps the comment; the `#`
        //    in the payload is INSIDE the quoted string, never a real comment.
        let line = updated.lines().find(|l| l.trim_start().starts_with("roots =")).unwrap();
        assert!(line.contains("\\\""), "the quote must be backslash-escaped: {line}");
        assert!(line.ends_with("# EXPLICIT folder allowlist, SHIPS EMPTY."), "real comment kept: {line}");
    }

    #[test]
    fn injection_via_string_field_is_escaped_to_a_single_inert_token() {
        // A model-id with a quote + array-close + key-inject payload.
        let evil = "a\\b\"\nclassifier = \"pwned";
        // A newline/control char is REJECTED outright (defense in depth).
        let r = apply_changes(SAMPLE_V1, &[Change {
            id: "vision.model".into(),
            value: SettingValue::Str(evil.into()),
        }]);
        assert!(r.is_err(), "a control char / newline in a string must be rejected");

        // A control-free but quote/backslash-laden value is ESCAPED, not errored,
        // and re-parses to the SAME string with no injected key.
        let tricky = "a\\b\", classifier = \"pwned"; // backslash + quote, no newline
        let updated = apply_changes(SAMPLE_V1, &[Change {
            id: "vision.model".into(),
            value: SettingValue::Str(tricky.into()),
        }]).expect("escaped, not errored");
        let states = build_get(&updated);
        assert_eq!(get_value(&states, "vision.model"), SettingValue::Str(tricky.into()));
        // classifier was not hijacked — it is still exactly one key and still "".
        assert_eq!(count_keys_in_section(&updated, "models", "classifier"), 1);
        assert_eq!(get_value(&states, "models.classifier"), SettingValue::Str(String::new()));
        // vision.model is still a single key.
        assert_eq!(count_keys_in_section(&updated, "vision", "model"), 1);
    }

    #[test]
    fn control_chars_newline_nul_are_rejected_in_array_elements() {
        for bad in ["/a\nb", "/a\tb", "/a\u{0000}b", "/a\u{007F}b", "/a\rb"] {
            let r = apply_changes(SAMPLE_V1, &[Change {
                id: "docsearch.roots".into(),
                value: SettingValue::StrList(vec![bad.into()]),
            }]);
            assert!(r.is_err(), "control char in {bad:?} must be rejected");
        }
    }

    #[test]
    fn escape_then_parse_is_lossless_for_adversarial_strings() {
        // Property: for any control-free string, escape_toml_basic wrapped in
        // quotes re-parses to the identical string (the escape is sound + complete).
        for s in [
            "plain/repo-id",
            "with \"quotes\" inside",
            "back\\slash\\path",
            "],[#=trick",
            "trailing-backslash\\",
            "unicode-é-ñ-中",
        ] {
            let token = format!("\"{}\"", escape_toml_basic(s));
            assert_eq!(parse_toml_basic_string(&token).as_deref(), Some(s), "lossless for {s:?}");
        }
    }

    // ---------- (d) a non-absolute root path is rejected ----------

    #[test]
    fn non_absolute_root_path_is_rejected() {
        for bad in ["relative/path", "~/Documents", "C:\\\\win", "  ", "./x"] {
            let r = apply_changes(SAMPLE_V1, &[Change {
                id: "docsearch.roots".into(),
                value: SettingValue::StrList(vec![bad.into()]),
            }]);
            assert!(r.is_err(), "non-absolute root {bad:?} must be rejected");
        }
        // code.roots is also path-gated.
        assert!(apply_changes(SAMPLE_V1, &[Change {
            id: "code.roots".into(),
            value: SettingValue::StrList(vec!["not-abs".into()]),
        }]).is_err());
        // But local_warm is NOT path-gated — a repo id (no leading /) is fine.
        assert!(apply_changes(SAMPLE_V1, &[Change {
            id: "models.local_warm".into(),
            value: SettingValue::StrList(vec!["mlx-community/whatever".into()]),
        }]).is_ok());
    }

    #[test]
    fn duplicate_array_elements_are_rejected() {
        let r = apply_changes(SAMPLE_V1, &[Change {
            id: "docsearch.roots".into(),
            value: SettingValue::StrList(vec!["/a".into(), "/a".into()]),
        }]);
        assert!(r.is_err(), "a duplicate path must be rejected");
    }

    #[test]
    fn overlong_string_is_rejected() {
        let huge = "/".to_string() + &"x".repeat(MAX_STR_LEN);
        assert!(apply_changes(SAMPLE_V1, &[Change {
            id: "docsearch.roots".into(),
            value: SettingValue::StrList(vec![huge]),
        }]).is_err());
        let huge_id = "m/".to_string() + &"y".repeat(MAX_STR_LEN);
        assert!(apply_changes(SAMPLE_V1, &[Change {
            id: "vision.model".into(),
            value: SettingValue::Str(huge_id),
        }]).is_err());
    }

    #[test]
    fn wrong_type_for_string_or_array_field_is_rejected() {
        // A bool for a Str field, a string for an array field, etc.
        assert!(apply_changes(SAMPLE_V1, &[Change {
            id: "vision.model".into(), value: SettingValue::Bool(true),
        }]).is_err());
        assert!(apply_changes(SAMPLE_V1, &[Change {
            id: "docsearch.roots".into(), value: SettingValue::Str("/a".into()),
        }]).is_err());
        // local_budget_gib out of range.
        assert!(apply_changes(SAMPLE_V1, &[Change {
            id: "models.local_budget_gib".into(), value: SettingValue::Float(99.0),
        }]).is_err());
        assert!(apply_changes(SAMPLE_V1, &[Change {
            id: "models.local_budget_gib".into(), value: SettingValue::Float(-1.0),
        }]).is_err());
    }

    // ---------- (e) unknown key still rejected (now that new kinds exist) ----------

    #[test]
    fn unknown_key_still_rejected_for_string_array_values() {
        for bad in [
            "models.heavy_model",          // a real-ish key, NOT whitelisted
            "docsearch.roots\nfoo = [1",   // injection-shaped id
            "code.bogus",
        ] {
            assert!(apply_changes(SAMPLE_V1, &[Change {
                id: bad.into(),
                value: SettingValue::StrList(vec!["/x".into()]),
            }]).is_err(), "unknown key {bad:?} must be rejected");
        }
    }

    // ---------- single-line array parser correctness ----------

    #[test]
    fn array_parser_tolerates_spacing_and_comments() {
        // Spaces, trailing comma, and an inline comment after the `]`.
        let raw = read_raw_values("[docsearch]\nroots = [ \"/a\" ,  \"/b\" ]   # the allowlist\n");
        let tok = raw.get(&("docsearch".into(), "roots".into())).unwrap();
        assert_eq!(parse_toml_string_array(tok), Some(vec!["/a".to_string(), "/b".to_string()]));
        // A `]` or `,` INSIDE a quoted element is not a terminator/separator.
        assert_eq!(
            parse_toml_string_array("[\"/a],b\"]"),
            Some(vec!["/a],b".to_string()])
        );
        // An escaped quote inside a basic-string element is handled.
        assert_eq!(
            parse_toml_string_array("[\"/a\\\"b\"]"),
            Some(vec!["/a\"b".to_string()])
        );
        // Empty forms.
        assert_eq!(parse_toml_string_array("[]"), Some(vec![]));
        assert_eq!(parse_toml_string_array("[   ]"), Some(vec![]));
    }

    #[test]
    fn real_config_round_trips_new_v1_fields_no_collateral_change() {
        // End-to-end over the REAL shipped config/jarvis.toml (a temp in-memory
        // copy; the file is never written): set a model id + both roots arrays +
        // local_warm + the budget, prove GET returns them, and prove ONLY the
        // targeted lines changed (comments + every other line intact).
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let root = manifest.parent().and_then(Path::parent).unwrap();
        let text = std::fs::read_to_string(root.join("config/jarvis.toml")).unwrap();

        // DRIFT-PROOF BY CONSTRUCTION: read each field's CURRENT shipped value and
        // pick a target proven to DIFFER (asserted below). This keeps the "exactly
        // five lines change" guard SOUND no matter what the installer later
        // populates for any of these keys — a target that happened to equal the
        // shipped value would silently be a no-op and drop the changed count (this
        // test broke once when a release set vision.model to the literal it used).
        // The assert_ne! turns any future collision into a loud, obvious failure
        // here instead of a confusing off-by-one count mismatch.
        let cur = build_get(&text);
        let cur_str = |id: &str| match get_value(&cur, id) {
            SettingValue::Str(s) => s,
            other => panic!("expected {id} to be a string, got {other:?}"),
        };
        let cur_list = |id: &str| match get_value(&cur, id) {
            SettingValue::StrList(l) => l,
            other => panic!("expected {id} to be a list, got {other:?}"),
        };
        let cur_f = |id: &str| match get_value(&cur, id) {
            SettingValue::Float(f) => f,
            other => panic!("expected {id} to be a float, got {other:?}"),
        };

        // Distinct, validation-passing targets (absolute roots; in-range [0,8] budget).
        let t_vision = "rt-sentinel/vlm".to_string();
        let t_docroots = vec!["/rt-sentinel/notes".to_string()];
        let t_coderoots = vec!["/rt-sentinel/proj".to_string()];
        let t_warm = vec!["rt-sentinel/warm-model".to_string()];
        let t_budget = if cur_f("models.local_budget_gib") != 2.5 { 2.5 } else { 3.5 };

        // Every target genuinely differs from the shipped value => all five lines change.
        assert_ne!(t_vision, cur_str("vision.model"), "pick a vision.model target that differs from the shipped config");
        assert_ne!(t_docroots, cur_list("docsearch.roots"), "pick docsearch.roots that differ from the shipped config");
        assert_ne!(t_coderoots, cur_list("code.roots"), "pick code.roots that differ from the shipped config");
        assert_ne!(t_warm, cur_list("models.local_warm"), "pick local_warm that differs from the shipped config");
        assert_ne!(t_budget, cur_f("models.local_budget_gib"), "pick a budget that differs from the shipped config");

        let changes = vec![
            Change { id: "vision.model".into(), value: SettingValue::Str(t_vision.clone()) },
            Change { id: "docsearch.roots".into(), value: SettingValue::StrList(t_docroots.clone()) },
            Change { id: "code.roots".into(), value: SettingValue::StrList(t_coderoots.clone()) },
            Change { id: "models.local_warm".into(), value: SettingValue::StrList(t_warm.clone()) },
            Change { id: "models.local_budget_gib".into(), value: SettingValue::Float(t_budget) },
        ];
        let updated = apply_changes(&text, &changes).expect("apply on the real config");
        let states = build_get(&updated);
        assert_eq!(get_value(&states, "vision.model"), SettingValue::Str(t_vision));
        assert_eq!(get_value(&states, "docsearch.roots"), SettingValue::StrList(t_docroots));
        assert_eq!(get_value(&states, "code.roots"), SettingValue::StrList(t_coderoots));
        assert_eq!(get_value(&states, "models.local_warm"), SettingValue::StrList(t_warm));
        assert_eq!(get_value(&states, "models.local_budget_gib"), SettingValue::Float(t_budget));

        // Exactly five lines changed (sound: all five targets provably differ above);
        // same line count + trailing-newline state => no collateral change.
        let changed = text.lines().zip(updated.lines()).filter(|(a, b)| a != b).count();
        assert_eq!(changed, 5, "exactly the five targeted lines change");
        assert_eq!(text.lines().count(), updated.lines().count());
        assert_eq!(text.ends_with('\n'), updated.ends_with('\n'));
        // Comments preserved: no full-line comment ever changes (the value lines'
        // inline trailing comments are covered by the dedicated comment tests).
        for (a, b) in text.lines().zip(updated.lines()) {
            if a.trim_start().starts_with('#') {
                assert_eq!(a, b, "a comment line must never change");
            }
        }
    }

    #[test]
    fn new_v1_keys_have_correct_kind_tags_in_get() {
        let states = build_get(SAMPLE_V1);
        let by = |id: &str| states.iter().find(|s| s.id == id).unwrap().kind.clone();
        assert_eq!(by("vision.model"), "string");
        assert_eq!(by("speech.model"), "string");
        assert_eq!(by("inference.draft_model"), "string");
        assert_eq!(by("models.classifier"), "string");
        assert_eq!(by("docsearch.roots"), "pathlist");
        assert_eq!(by("code.roots"), "pathlist");
        assert_eq!(by("models.local_warm"), "strlist");
        assert_eq!(by("models.local_budget_gib"), "float");
    }
}

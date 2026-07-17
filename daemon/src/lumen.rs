//! lumen.rs — LUMEN: an LLM-grounded SCREEN NARRATOR + hands-free VOICE
//! NAVIGATION (assistive tech). Two capabilities, one PURE + testable core:
//!
//!   (a) NARRATE — on a focus-change (or an explicit "read me the screen") speak
//!       the focused element / the on-screen controls through the existing speech
//!       path. This half is READ-ONLY: it describes what is on the screen and
//!       actuates NOTHING.
//!   (b) VOICE-NAVIGATE — pair the READ-ONLY OCR/AX locate (the Vision app's
//!       `read.screen` control readout) with the EXISTING, per-action-gated
//!       `ui_actuate` CAPSTONE (#44) to execute ONE voice-named UI action at a
//!       time ("read me the buttons, then click the third"). Lumen only SELECTS
//!       the one target and builds the [`crate::ui_automation::ActuationRequest`];
//!       the UNCHANGED capstone owns every gate (the pure single-action planner,
//!       the consequential spoken confirm PER ACTION, the master switch, voice-id,
//!       and `!lockdown`). Lumen does NOT weaken, bypass, or re-implement any of
//!       that — a selection is just a request the capstone still fully gates.
//!
//! THE SUBSTANCE IS THE PURE SEAM (proven hermetically here):
//!   * NARRATION COMPOSITION — an AX/OCR element description => the spoken line
//!     (and the honest "nothing is focused" for an empty focus).
//!   * ACTION SELECTION — "click the third button" / "click Submit" + a located
//!     element list => the ONE target element, or an HONEST refusal ("I can't find
//!     that", "which one did you mean") — NEVER a wrong click. Ambiguity and a
//!     miss both REFUSE; they never actuate a guess.
//!   * VISION PARSE — the Vision `read.screen` control readout (JSON) => the
//!     bounded list of [`NarratableElement`]s the two seams above run over.
//!
//! DEVICE-GATED RUNNER (built here / wired at integration, NEVER run in a test):
//! the AX-tree read, the OCR, the screen capture, and the actuation are all
//! device-gated. The OCR/AX locate is the Vision app's TCC-gated `read.screen`
//! path (Screen-Recording + a real display); the actuation is the capstone's
//! Accessibility-TCC-gated `do_actuate`. Lumen consumes the DERIVED readout and
//! produces the request — no pixels, no synthetic events, and no real AX read
//! ever happen under `cargo test`.
//!
//! PRIVACY / HONESTY: continuous narration is EXPLICIT opt-in (`[lumen].narrate`
//! ships **false** — off is a no-op). Narration NEVER fabricates an element (an
//! empty focus / empty screen is spoken honestly). Selection NEVER fabricates a
//! target (a miss / an ambiguity REFUSES). The telemetry frame is SECRET-FREE by
//! construction: it carries the element ROLE + counts + lengths, NEVER the raw
//! on-screen label text (which could carry a message / a field's contents).
//
// INTEGRATION SEAMS (where the live daemon wiring lands — the PURE seams below are
// the substance + are covered by the hermetic tests in this file; the device-gated
// runner is reconciled at integration, so these stay `#![allow(dead_code)]` as the
// auditable contract surface, mirroring policy.rs / prosody.rs / triage.rs):
//   * main.rs — installs the continuous-narration gate ([lumen].narrate) at startup
//     + emits `lumen.configured` (status_frame). ALREADY WIRED.
//   * router.rs — maps "read me the screen / the buttons" + "click the <ordinal|name>"
//     to the Vision `read.screen` locate (READ-ONLY) then this module's compose/
//     select. (The `read.screen` op already exists; the phrase→lumen dispatch is the
//     reconcile point.)
//   * ui_actuate reuse (anthropic.rs) — `resolve_voice_action` builds the UNCHANGED
//     `crate::ui_automation::ActuationRequest`; the CAPSTONE still owns every gate.
//   * the OCR/AX seam (apps.rs vision.screen relay + Vision `read.screen`) — feeds
//     the control readout JSON that `parse_vision_controls` turns into elements.
//   * telemetry.rs — `status_frame` / `narration_event_frame` / `action_frame`
//     (SECRET-FREE) are the frames the relay/HUD emit.
#![allow(dead_code)]

use std::sync::Mutex;

use serde_json::{json, Value};

// ===========================================================================
// ElementRole — the class of an on-screen element, with its SPOKEN phrase.
// ===========================================================================

/// The class of a located on-screen element. The Vision `read.screen` readout
/// gives us the recognized label plus an `is_control` flag; [`infer_role`] maps
/// those to one of these conservative classes so narration can speak a faithful
/// noun ("a button", "a link") and selection can filter by kind ("the third
/// button"). Unknown is the honest fallback — never an invented class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementRole {
    Button,
    Link,
    TextField,
    Checkbox,
    MenuItem,
    Tab,
    /// Plain readable text (not an actionable control).
    Text,
    /// An element whose class we could not confidently infer — spoken honestly
    /// as "an element", never a fabricated class.
    Unknown,
}

impl ElementRole {
    /// The SPOKEN phrase for this role (with its article), for the narration line
    /// — e.g. "a button", "a text field", "an element". Faithful to the class;
    /// never overclaims.
    pub fn spoken_phrase(self) -> &'static str {
        match self {
            ElementRole::Button => "a button",
            ElementRole::Link => "a link",
            ElementRole::TextField => "a text field",
            ElementRole::Checkbox => "a checkbox",
            ElementRole::MenuItem => "a menu item",
            ElementRole::Tab => "a tab",
            ElementRole::Text => "text",
            ElementRole::Unknown => "an element",
        }
    }

    /// A short, SECRET-FREE tag for telemetry (never the label). Stable across
    /// versions so a consumer can bucket by role.
    pub fn tag(self) -> &'static str {
        match self {
            ElementRole::Button => "button",
            ElementRole::Link => "link",
            ElementRole::TextField => "text_field",
            ElementRole::Checkbox => "checkbox",
            ElementRole::MenuItem => "menu_item",
            ElementRole::Tab => "tab",
            ElementRole::Text => "text",
            ElementRole::Unknown => "unknown",
        }
    }

    /// The role a bare role-noun in a selection phrase names ("button" => Button,
    /// "field"/"textbox" => TextField, …), or None when the token is not a role
    /// noun. Used to narrow an ordinal selection ("the third BUTTON"). PURE.
    fn from_noun(token: &str) -> Option<ElementRole> {
        Some(match token {
            "button" | "buttons" => ElementRole::Button,
            "link" | "links" => ElementRole::Link,
            "field" | "fields" | "textfield" | "textbox" | "input" => ElementRole::TextField,
            "checkbox" | "checkboxes" => ElementRole::Checkbox,
            "menu" | "menuitem" => ElementRole::MenuItem,
            "tab" | "tabs" => ElementRole::Tab,
            _ => return None,
        })
    }
}

/// CONSERVATIVE role inference from a label + the Vision `is_control` flag. A
/// non-control block is plain [`ElementRole::Text`]. A control is classed by a
/// keyword in its label, defaulting to [`ElementRole::Button`] (the most common
/// actionable control) — an honest best guess, never a fabricated certainty.
/// PURE.
pub fn infer_role(label: &str, is_control: bool) -> ElementRole {
    if !is_control {
        return ElementRole::Text;
    }
    let l = label.to_lowercase();
    // URL-ish or explicit link cue.
    if l.contains("http://") || l.contains("https://") || l.contains("www.") || l.contains(" link")
    {
        return ElementRole::Link;
    }
    if l.contains("checkbox") || l.contains("check box") {
        return ElementRole::Checkbox;
    }
    if l.contains("text field")
        || l.contains("textfield")
        || l.contains("search")
        || l.contains("password")
        || l.contains("email")
        || l.contains("username")
    {
        return ElementRole::TextField;
    }
    if l.contains(" tab") || l.ends_with("tab") {
        return ElementRole::Tab;
    }
    if l.contains("menu") {
        return ElementRole::MenuItem;
    }
    ElementRole::Button
}

// ===========================================================================
// NarratableElement — one located on-screen element (the PURE seam's unit).
// ===========================================================================

/// One located on-screen element the narration + selection seams operate over.
/// Built from the Vision `read.screen` readout by [`parse_vision_controls`]. The
/// `center` is the located point (as the device-gated locate produced it) — a
/// "where", NOT a fabricated click: the actuation still runs through the capstone
/// planner (which bounds-checks it against the real display) + every gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NarratableElement {
    /// The recognized on-screen label / AX name (e.g. "Send", "Sign in").
    pub label: String,
    /// The inferred class (button / link / field / …).
    pub role: ElementRole,
    /// The located point, if the readout carried one. `None` => the element can
    /// be NARRATED but not CLICKED (a voice action refuses honestly rather than
    /// click a fabricated coordinate).
    pub center: Option<(i32, i32)>,
}

impl NarratableElement {
    /// Whether this element is an ACTIONABLE control (anything but plain text).
    /// Selection over "the buttons"/"the controls" filters on this.
    fn is_control(&self) -> bool {
        !matches!(self.role, ElementRole::Text)
    }
}

/// PARSE the Vision `read.screen` readout JSON into a bounded list of
/// [`NarratableElement`]s. Reads the `controls` array (each block: `text`,
/// `center:{x,y}`, `is_control`); falls back to `blocks` when `controls` is
/// absent. An empty/whitespace label is dropped (never a fabricated element).
/// The result is BOUNDED to `max` (the newest/first `max` — a huge screen is
/// never read wholesale). PURE — no socket, no device; unit-tested over
/// synthetic JSON, so it agrees with the on-wire shape by construction.
pub fn parse_vision_controls(readout: &Value, max: usize) -> Vec<NarratableElement> {
    let cap = max.max(1);
    let arr = readout
        .get("controls")
        .and_then(Value::as_array)
        .or_else(|| readout.get("blocks").and_then(Value::as_array));
    let Some(arr) = arr else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for block in arr {
        let label = block
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if label.is_empty() {
            continue;
        }
        // `is_control` defaults TRUE for the `controls` array (those are the
        // control candidates); a `blocks` fallback carries the flag explicitly.
        let is_control = block
            .get("is_control")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let center = parse_center(block.get("center"));
        out.push(NarratableElement {
            label: label.clone(),
            role: infer_role(&label, is_control),
            center,
        });
        if out.len() >= cap {
            break;
        }
    }
    out
}

/// Parse a `{"x":..,"y":..}` center into an integer point, or None. Accepts
/// integer or floating JSON numbers (rounded). PURE.
fn parse_center(center: Option<&Value>) -> Option<(i32, i32)> {
    let c = center?;
    let x = c.get("x").and_then(Value::as_f64)?;
    let y = c.get("y").and_then(Value::as_f64)?;
    Some((x.round() as i32, y.round() as i32))
}

// ===========================================================================
// (a) NARRATION COMPOSITION — element => spoken line. READ-ONLY.
// ===========================================================================

/// The honest owner-address the narration lines use, matching the rest of the
/// daemon's spoken voice.
const SIR: &str = "sir";

/// Compose the spoken line for ONE focused element: `"<label>, <role phrase>."`
/// (e.g. `"Send, a button."`). PURE + faithful — it speaks exactly the located
/// label and the inferred class, never an invented one. READ-ONLY.
pub fn narrate_element(el: &NarratableElement) -> String {
    format!("{}, {}.", el.label.trim(), el.role.spoken_phrase())
}

/// Compose the spoken line for a focus-change: the focused element's narration,
/// or — when NOTHING is focused — the HONEST "nothing is focused" line (never a
/// fabricated element). READ-ONLY.
pub fn narrate_focus(focused: Option<&NarratableElement>) -> String {
    match focused {
        Some(el) => narrate_element(el),
        None => format!("Nothing is focused right now, {SIR}."),
    }
}

/// Compose the spoken readout for "read me the buttons / the controls": a
/// NUMBERED list of the actionable controls so the user can then name one ("click
/// the third"). An empty screen is the HONEST "I don't see any controls" (never a
/// fabricated list). Only actual controls are listed (plain text is skipped).
/// BOUNDED by the caller's `parse_vision_controls` cap. READ-ONLY.
pub fn narrate_controls(elements: &[NarratableElement]) -> String {
    let controls: Vec<&NarratableElement> = elements.iter().filter(|e| e.is_control()).collect();
    if controls.is_empty() {
        return format!("I don't see any controls on the screen right now, {SIR}.");
    }
    let mut out = if controls.len() == 1 {
        format!("I see one control, {SIR}:")
    } else {
        format!("I see {} controls, {SIR}:", controls.len())
    };
    for (i, el) in controls.iter().enumerate() {
        out.push(' ');
        out.push_str(&ordinal_word(i + 1));
        out.push_str(", ");
        out.push_str(el.label.trim());
        out.push_str(", ");
        out.push_str(el.role.spoken_phrase());
        out.push('.');
    }
    out
}

/// The spoken ordinal for a 1-based position ("one".."ten", then the bare
/// number). Used to number the controls readout so a selection ("the third") is
/// unambiguous. PURE.
fn ordinal_word(n: usize) -> String {
    const WORDS: [&str; 10] = [
        "One", "Two", "Three", "Four", "Five", "Six", "Seven", "Eight", "Nine", "Ten",
    ];
    match n {
        1..=10 => WORDS[n - 1].to_string(),
        _ => format!("Number {n}"),
    }
}

// ===========================================================================
// (b) ACTION SELECTION — phrase + located list => the ONE target, or REFUSE.
// ===========================================================================

/// Why a voice-named action could not be resolved to EXACTLY ONE target. Every
/// variant is an HONEST refusal — the daemon speaks the reason and actuates
/// NOTHING. A miss or an ambiguity NEVER becomes a wrong click.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectError {
    /// There were no located elements to choose from (the screen read produced
    /// nothing / was not run).
    NoElements,
    /// The phrase named no target (empty / only a verb like "click").
    EmptyPhrase,
    /// No element matched the name/ordinal. Carries the phrase for the honest
    /// "I can't find <that>" line.
    NotFound(String),
    /// The ordinal was past the end (e.g. "the fifth button" with three buttons).
    /// Carries how many of that kind exist.
    OutOfRange { requested: usize, available: usize, kind: String },
    /// The name matched MORE THAN ONE element — refuse and ask which, never guess.
    /// Carries the ambiguous labels.
    Ambiguous(Vec<String>),
    /// The chosen element has no located point, so it cannot be clicked (it can
    /// still be narrated). Refused honestly rather than clicking a fabricated
    /// coordinate.
    NoLocation(String),
}

impl SelectError {
    /// A short, STABLE, SECRET-FREE class tag for telemetry (never the phrase or
    /// the labels) — the refusal bucket a consumer groups by. Shared by both
    /// action-frame builders so the wire tag can never drift between them.
    fn tag(&self) -> &'static str {
        match self {
            SelectError::NoElements => "no_elements",
            SelectError::EmptyPhrase => "empty_phrase",
            SelectError::NotFound(_) => "not_found",
            SelectError::OutOfRange { .. } => "out_of_range",
            SelectError::Ambiguous(_) => "ambiguous",
            SelectError::NoLocation(_) => "no_location",
        }
    }

    /// A faithful, honest one-line reason for the spoken refusal. NEVER claims an
    /// action happened; states precisely why nothing was selected/actuated.
    pub fn reason(&self) -> String {
        match self {
            SelectError::NoElements => format!(
                "I don't have anything located on the screen to act on, {SIR} — read the screen first."
            ),
            SelectError::EmptyPhrase => {
                format!("you didn't say which element to act on, {SIR}.")
            }
            SelectError::NotFound(what) => {
                format!("I can't find \"{}\" on the screen, {SIR}, so I won't click anything.", what.trim())
            }
            SelectError::OutOfRange { requested, available, kind } => format!(
                "there {} only {} {} on the screen, {SIR}, so there's no number {}.",
                if *available == 1 { "is" } else { "are" },
                available,
                pluralize(kind, *available),
                requested
            ),
            SelectError::Ambiguous(labels) => format!(
                "that matches {} elements, {SIR} ({}) — tell me which one and I'll act on just that.",
                labels.len(),
                labels.join(", ")
            ),
            SelectError::NoLocation(what) => format!(
                "I found \"{}\" but I don't have a location for it, {SIR}, so I won't click a guessed spot.",
                what.trim()
            ),
        }
    }
}

/// Singular/plural a role kind for the refusal grammar ("button"/"buttons").
fn pluralize(kind: &str, n: usize) -> String {
    if n == 1 {
        kind.to_string()
    } else {
        format!("{kind}s")
    }
}

/// SELECT the ONE target element a voice phrase names, out of the located list —
/// or REFUSE honestly. This is the substance: it NEVER returns a wrong element
/// and NEVER guesses through ambiguity. Two phrase shapes:
///
///   * ORDINAL — "the third button" / "second link" / "the 2nd" / "number 3" /
///     "first". An optional role noun narrows the candidates (else all controls);
///     the ordinal picks the Nth (1-based). Past the end => [`SelectError::OutOfRange`].
///   * NAME — "Submit" / "the Send button" / "click Sign in". The verb, articles,
///     and a trailing role noun are stripped, then the remainder is matched
///     against the labels: an EXACT (case-insensitive) label match wins; else a
///     unique CONTAINS match wins; MORE THAN ONE match => [`SelectError::Ambiguous`];
///     none => [`SelectError::NotFound`].
///
/// Returns the index into `elements` (so the caller can read the label + center).
/// PURE + unit-tested exhaustively.
pub fn select_target(phrase: &str, elements: &[NarratableElement]) -> Result<usize, SelectError> {
    if elements.is_empty() {
        return Err(SelectError::NoElements);
    }
    let lower = phrase.trim().to_lowercase();
    if lower.is_empty() {
        return Err(SelectError::EmptyPhrase);
    }

    // Tokenize on non-alphanumeric so "the 3rd button!" -> [the,3rd,button].
    let tokens: Vec<String> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect();
    if tokens.is_empty() {
        return Err(SelectError::EmptyPhrase);
    }

    // An optional role-noun filter anywhere in the phrase ("the third BUTTON").
    let role_filter = tokens.iter().find_map(|t| ElementRole::from_noun(t));

    // ORDINAL path — a positional word/number selects the Nth of the (filtered)
    // controls. Checked first so "click the third button" is positional, not a
    // name lookup for "third".
    if let Some(n) = tokens.iter().find_map(|t| parse_ordinal(t)) {
        return select_by_ordinal(n, role_filter, elements);
    }

    // NAME path — strip the verb/articles/role noun, match the remainder.
    select_by_name(&tokens, role_filter, elements)
}

/// Select the Nth (1-based) control, optionally filtered to a role. Past the end
/// is an HONEST out-of-range refusal. PURE.
fn select_by_ordinal(
    n: usize,
    role_filter: Option<ElementRole>,
    elements: &[NarratableElement],
) -> Result<usize, SelectError> {
    // Candidate original indices: the role-filtered controls, in screen order.
    let candidates: Vec<usize> = elements
        .iter()
        .enumerate()
        .filter(|(_, e)| match role_filter {
            Some(role) => e.role == role,
            None => e.is_control(),
        })
        .map(|(i, _)| i)
        .collect();
    let kind = role_filter
        .map(|r| r.tag().replace('_', " "))
        .unwrap_or_else(|| "control".to_string());
    if candidates.is_empty() {
        return Err(SelectError::OutOfRange { requested: n, available: 0, kind });
    }
    if n == 0 || n > candidates.len() {
        return Err(SelectError::OutOfRange {
            requested: n,
            available: candidates.len(),
            kind,
        });
    }
    Ok(candidates[n - 1])
}

/// Select the element a NAME names: an exact (case-insensitive) label match wins;
/// else a UNIQUE substring match; more than one is ambiguous; none is not-found.
/// PURE.
fn select_by_name(
    tokens: &[String],
    role_filter: Option<ElementRole>,
    elements: &[NarratableElement],
) -> Result<usize, SelectError> {
    // Drop leading/standalone verbs + articles + a role noun to isolate the name.
    const NOISE: &[&str] = &[
        "click", "press", "tap", "hit", "select", "choose", "push", "the", "a", "an", "on", "onto",
        "please", "button", "buttons", "link", "links", "field", "fields", "textfield", "textbox",
        "input", "checkbox", "checkboxes", "menu", "menuitem", "tab", "tabs", "control", "item",
        "named", "labeled", "labelled", "called", "that", "says",
    ];
    let name_tokens: Vec<&str> = tokens
        .iter()
        .map(String::as_str)
        .filter(|t| !NOISE.contains(t))
        .collect();
    let name = name_tokens.join(" ");
    if name.is_empty() {
        return Err(SelectError::EmptyPhrase);
    }

    // Apply the optional role filter to the candidate set.
    let in_scope = |e: &NarratableElement| match role_filter {
        Some(role) => e.role == role,
        None => true,
    };

    // EXACT label match (case-insensitive) — the strongest, unambiguous signal.
    let exact: Vec<usize> = elements
        .iter()
        .enumerate()
        .filter(|(_, e)| in_scope(e) && e.label.trim().to_lowercase() == name)
        .map(|(i, _)| i)
        .collect();
    if exact.len() == 1 {
        return Ok(exact[0]);
    }
    if exact.len() > 1 {
        return Err(SelectError::Ambiguous(
            exact.iter().map(|&i| elements[i].label.clone()).collect(),
        ));
    }

    // SUBSTRING match either way (the label contains the name, or the name
    // contains the label — "sign in" vs a "Sign in with Apple" button).
    let partial: Vec<usize> = elements
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            if !in_scope(e) {
                return false;
            }
            let label = e.label.trim().to_lowercase();
            label.contains(&name) || name.contains(&label)
        })
        .map(|(i, _)| i)
        .collect();
    match partial.len() {
        0 => Err(SelectError::NotFound(name)),
        1 => Ok(partial[0]),
        _ => Err(SelectError::Ambiguous(
            partial.iter().map(|&i| elements[i].label.clone()).collect(),
        )),
    }
}

/// Parse ONE token into a 1-based ordinal, or None. Handles the number words
/// ("first".."tenth"), the digit+suffix forms ("1st","2nd","3rd","4th"), and a
/// bare number ("3"). "number"/"no" prefixes are stripped by tokenization, so a
/// bare "3" here is the target index. PURE.
fn parse_ordinal(token: &str) -> Option<usize> {
    let n = match token {
        "first" | "1st" => 1,
        "second" | "2nd" => 2,
        "third" | "3rd" => 3,
        "fourth" | "4th" => 4,
        "fifth" | "5th" => 5,
        "sixth" | "6th" => 6,
        "seventh" | "7th" => 7,
        "eighth" | "8th" => 8,
        "ninth" | "9th" => 9,
        "tenth" | "10th" => 10,
        _ => {
            // A bare number ("3", "12"). Guard the length so a long digit run
            // (an id / a code, never an ordinal) is not read as one.
            if token.len() <= 3 && token.chars().all(|c| c.is_ascii_digit()) {
                token.parse::<usize>().ok()?
            } else {
                return None;
            }
        }
    };
    Some(n)
}

// ===========================================================================
// VOICE-ACTION BRIDGE — selection => a capstone request. The UNCHANGED gate runs.
// ===========================================================================

/// Resolve a voice phrase + a located element list into the ONE
/// [`crate::ui_automation::ActuationRequest`] the UNCHANGED `ui_actuate` capstone
/// will plan + gate + (device-gated) actuate — or REFUSE honestly. This is the
/// ONLY bridge from Lumen to actuation, and it is a PURE request BUILDER: it
/// selects exactly one target (never a guess) and describes a single `Click` at
/// that element's located point. It does NOT actuate, does NOT gate, and does NOT
/// touch the capstone's machinery — the returned request re-enters `ui_actuate`,
/// which still PARKS it per-action for the spoken confirm + master switch +
/// voice-id + `!lockdown`, exactly as for any other actuation. ONE resolved
/// phrase = ONE request = (after the capstone's own gate) at most ONE actuation.
///
/// A miss, an ambiguity, an out-of-range ordinal, or a located element with no
/// point all REFUSE with a [`SelectError`] — nothing is ever clicked on a guess.
pub fn resolve_voice_action(
    phrase: &str,
    elements: &[NarratableElement],
) -> Result<crate::ui_automation::ActuationRequest, SelectError> {
    let idx = select_target(phrase, elements)?;
    let el = &elements[idx];
    let (x, y) = el
        .center
        .ok_or_else(|| SelectError::NoLocation(el.label.clone()))?;
    Ok(crate::ui_automation::ActuationRequest {
        action: crate::ui_automation::Action::Click { x, y },
        target_desc: el.label.clone(),
    })
}

// ===========================================================================
// CONTINUOUS NARRATION GATE — process-global, EXPLICIT opt-in (default OFF).
//
// Mirrors screen_context::SETTINGS: a poison-tolerant process-global the daemon
// installs ONCE at startup from `[lumen].narrate`. OFF by default => the
// focus-change narration path is a strict NO-OP (Lumen speaks nothing on its
// own; the explicit "read me the screen" request path is unaffected).
// ===========================================================================

/// The process-global Lumen settings, installed ONCE at daemon startup from
/// `[lumen]`. A poison-tolerant `Mutex` (mirrors screen_context::SETTINGS). The
/// narrate gate ships OFF and the control bound defaults to 1 until the daemon
/// installs the configured values — so the focus-change narration path is inert
/// by default and reads exactly one process-global.
static SETTINGS: Mutex<LumenSettings> = Mutex::new(LumenSettings::off());

/// The Lumen settings the daemon honours: whether CONTINUOUS focus-change
/// narration is opted in, and the HARD bound on how many controls one readout
/// narrates / offers for selection.
#[derive(Debug, Clone, Copy)]
struct LumenSettings {
    narrate: bool,
    max_controls: usize,
}

impl LumenSettings {
    const fn off() -> Self {
        Self { narrate: false, max_controls: 1 }
    }
}

/// Install the Lumen settings at daemon startup (from `[lumen]`). Until this is
/// called — and whenever `narrate` is `false` — focus-change narration is a
/// NO-OP. `max_controls` is floored to >= 1 (a 0 bound would read nothing).
pub fn install_settings(narrate: bool, max_controls: usize) {
    let mut guard = SETTINGS.lock().unwrap_or_else(|e| e.into_inner());
    *guard = LumenSettings {
        narrate,
        max_controls: max_controls.max(1),
    };
}

/// Whether continuous focus-change narration is currently enabled (the opt-in
/// gate). False by default and until `install_settings(true, _)`.
pub fn is_narrating() -> bool {
    SETTINGS.lock().unwrap_or_else(|e| e.into_inner()).narrate
}

/// The installed HARD bound on how many controls one readout narrates / offers
/// for selection (>= 1) — the process-global the narration/locate path reads so a
/// dense screen is never read wholesale. Defaults to 1 until `install_settings`.
pub fn max_controls() -> usize {
    SETTINGS.lock().unwrap_or_else(|e| e.into_inner()).max_controls
}

/// The CONTINUOUS focus-change entry point: compose the narration line for a
/// focus-change ONLY when continuous narration is opted in. When it is OFF (the
/// default) this is a strict NO-OP — it returns `None` and speaks nothing. When
/// ON it returns the honest narration line (`Some`), including the honest
/// "nothing is focused" for an empty focus. The explicit on-request read path
/// calls [`narrate_focus`] / [`narrate_controls`] directly and is NOT gated by
/// this (a user who asks to be read the screen always gets an answer).
pub fn narrate_on_focus_change(focused: Option<&NarratableElement>) -> Option<String> {
    if !is_narrating() {
        return None;
    }
    Some(narrate_focus(focused))
}

// ===========================================================================
// LAST SCREEN READOUT — the located controls the most recent read produced, so a
// follow-up voice ACTION ("read me the buttons, then click the THIRD") selects
// over the SAME list the user just heard. Process-global + poison-tolerant
// (mirrors SETTINGS). It holds ONLY the DERIVED, BOUNDED [`NarratableElement`]
// list (labels + roles + located points, already capped by `max_controls`) — the
// device-gated OCR/AX relay parses the async `read.screen` readout into it via
// [`remember_readout`] at integration; the router's ACT arm reads it via
// [`snapshot_controls`]. It is EMPTY until a read populates it, so a "click the
// third" with nothing read yet REFUSES honestly ([`SelectError::NoElements`] =>
// "read the screen first") — never a guess over a stale/absent screen.
// ===========================================================================

/// The most recent located-control readout the voice-navigation ACT arm selects
/// over. Empty until a read populates it. Poison-tolerant, mirroring [`SETTINGS`].
static LAST_READOUT: Mutex<Vec<NarratableElement>> = Mutex::new(Vec::new());

/// Remember the located controls from a Vision `read.screen` readout so a
/// follow-up voice ACTION can select over exactly what was just read. Parses the
/// readout via [`parse_vision_controls`] BOUNDED to the installed [`max_controls`]
/// (a dense screen is never remembered wholesale). The device-gated OCR/AX relay
/// calls this when a readout arrives (the async `vision.screen` event) at
/// integration; PURE + unit-tested over synthetic JSON, so it agrees with the
/// on-wire shape by construction (no socket, no device under `cargo test`).
pub fn remember_readout(readout: &Value) {
    let controls = parse_vision_controls(readout, max_controls());
    remember_controls(controls);
}

/// Remember an already-parsed control list directly (the relay may parse once and
/// reuse the list for both narration and selection). Kept in lock-step with
/// [`remember_readout`]; the hermetic tests seed the selection list through this
/// so no OCR/AX ever runs under `cargo test`.
pub fn remember_controls(controls: Vec<NarratableElement>) {
    *LAST_READOUT.lock().unwrap_or_else(|e| e.into_inner()) = controls;
}

/// A snapshot (clone) of the controls the most recent read produced — the list a
/// voice ACTION selects over. Empty until a read populates it (=> the action
/// REFUSES honestly rather than act on a stale/absent screen).
pub fn snapshot_controls() -> Vec<NarratableElement> {
    LAST_READOUT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Clear the remembered readout (e.g. on lockdown / a new session) so a later
/// action can never select over a stale screen.
pub fn clear_controls() {
    LAST_READOUT.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

// ===========================================================================
// TELEMETRY FRAMES — SECRET-FREE by construction (role + counts, NEVER labels).
// ===========================================================================

/// The `lumen.configured`/`lumen.status` frame: only the opt-in gate. Secret-free.
pub fn status_frame(narrate: bool) -> Value {
    json!({ "narrate": narrate })
}

/// A per-narration telemetry frame for ONE spoken element. SECRET-FREE by
/// construction: it carries the element ROLE, the label LENGTH, and whether a
/// location is present — NEVER the raw label text (which could be an on-screen
/// message / a field's contents). So a telemetry stream can show "narrated a
/// button" without ever leaking what the button said.
pub fn narration_event_frame(el: &NarratableElement) -> Value {
    json!({
        "role": el.role.tag(),
        "label_len": el.label.chars().count(),
        "has_location": el.center.is_some(),
    })
}

/// A per-action telemetry frame for a voice-navigation SELECTION. SECRET-FREE: it
/// carries the number of located controls, whether a target was resolved, and (on
/// a refusal) the refusal CLASS — never the phrase or the labels.
pub fn action_frame(control_count: usize, outcome: &Result<usize, SelectError>) -> Value {
    let (selected, refusal) = match outcome {
        Ok(_) => (true, "none"),
        Err(e) => (false, e.tag()),
    };
    json!({
        "controls": control_count,
        "selected": selected,
        "refusal": refusal,
    })
}

/// The per-action telemetry frame for a RESOLVED voice actuation (the router's
/// ACT arm dispatch). SECRET-FREE exactly like [`action_frame`]: the located-
/// control count, whether a CLICKABLE target was resolved (a target that was
/// selected but carries no located point is `selected=false, refusal="no_location"`
/// — no click point, so no action), and on a refusal the class — NEVER the phrase,
/// the labels, or the coordinate. Keyed on the [`resolve_voice_action`] result so
/// the telemetry is single-source with what actually reaches the capstone.
pub fn resolved_action_frame(
    control_count: usize,
    outcome: &Result<crate::ui_automation::ActuationRequest, SelectError>,
) -> Value {
    let (selected, refusal) = match outcome {
        Ok(_) => (true, "none"),
        Err(e) => (false, e.tag()),
    };
    json!({
        "controls": control_count,
        "selected": selected,
        "refusal": refusal,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The continuous-narration gate is a process-global; the few tests that flip
    // it run serially (poison-tolerant) so they don't race cargo's parallel
    // runner. The pure seam tests need no guard.
    static SERIAL: Mutex<()> = Mutex::new(());
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn btn(label: &str) -> NarratableElement {
        NarratableElement { label: label.to_string(), role: ElementRole::Button, center: Some((10, 10)) }
    }
    fn el(label: &str, role: ElementRole, center: Option<(i32, i32)>) -> NarratableElement {
        NarratableElement { label: label.to_string(), role, center }
    }
    fn sample() -> Vec<NarratableElement> {
        vec![
            el("Submit", ElementRole::Button, Some((100, 200))),
            el("Cancel", ElementRole::Button, Some((300, 200))),
            el("Help", ElementRole::Link, Some((500, 50))),
            el("Search", ElementRole::TextField, Some((250, 20))),
            el("Welcome to the app", ElementRole::Text, None),
        ]
    }

    // -- ROLE INFERENCE ----------------------------------------------------

    #[test]
    fn non_control_is_plain_text() {
        assert_eq!(infer_role("Welcome home", false), ElementRole::Text);
    }

    #[test]
    fn control_roles_are_inferred_conservatively() {
        assert_eq!(infer_role("Submit", true), ElementRole::Button);
        assert_eq!(infer_role("https://example.com", true), ElementRole::Link);
        assert_eq!(infer_role("Search the docs", true), ElementRole::TextField);
        assert_eq!(infer_role("Remember me checkbox", true), ElementRole::Checkbox);
        assert_eq!(infer_role("Settings menu", true), ElementRole::MenuItem);
    }

    // -- VISION PARSE ------------------------------------------------------

    #[test]
    fn parses_vision_controls_readout() {
        let readout = json!({
            "controls": [
                {"text": "Send", "center": {"x": 120.4, "y": 40.6}, "is_control": true},
                {"text": "  ", "center": {"x": 1, "y": 1}, "is_control": true},
                {"text": "Cancel", "center": {"x": 300, "y": 40}, "is_control": true},
            ]
        });
        let els = parse_vision_controls(&readout, 10);
        // The whitespace-only block is dropped (never a fabricated element).
        assert_eq!(els.len(), 2);
        assert_eq!(els[0].label, "Send");
        assert_eq!(els[0].center, Some((120, 41)));
        assert_eq!(els[1].label, "Cancel");
    }

    #[test]
    fn parse_is_bounded_by_max() {
        let controls: Vec<Value> = (0..50)
            .map(|i| json!({"text": format!("b{i}"), "center": {"x": i, "y": i}}))
            .collect();
        let readout = json!({ "controls": controls });
        let els = parse_vision_controls(&readout, 5);
        assert_eq!(els.len(), 5, "a huge screen is never read wholesale");
    }

    #[test]
    fn parse_missing_array_is_empty_not_fabricated() {
        assert!(parse_vision_controls(&json!({}), 10).is_empty());
        assert!(parse_vision_controls(&json!({"controls": "nope"}), 10).is_empty());
    }

    // -- NARRATION COMPOSITION --------------------------------------------

    #[test]
    fn narrates_a_focused_element_faithfully() {
        assert_eq!(narrate_element(&btn("Send")), "Send, a button.");
        assert_eq!(
            narrate_element(&el("Search", ElementRole::TextField, None)),
            "Search, a text field."
        );
    }

    #[test]
    fn empty_focus_is_honest_never_fabricated() {
        let line = narrate_focus(None);
        assert!(line.to_lowercase().contains("nothing is focused"), "{line}");
        // Nothing was invented — no element class/label is spoken.
        let l = line.to_lowercase();
        assert!(!l.contains("button") && !l.contains("field") && !l.contains("link"), "{line}");
    }

    #[test]
    fn narrate_controls_numbers_them_for_selection() {
        let line = narrate_controls(&sample());
        assert!(line.contains("I see 4 controls"), "{line}"); // the 5th is plain text
        assert!(line.contains("One, Submit, a button."), "{line}");
        assert!(line.contains("Three, Help, a link."), "{line}");
        // Plain text is NOT listed as a control.
        assert!(!line.contains("Welcome to the app"), "{line}");
    }

    #[test]
    fn narrate_controls_empty_is_honest() {
        let text_only = vec![el("just text", ElementRole::Text, None)];
        let line = narrate_controls(&text_only);
        assert!(line.to_lowercase().contains("don't see any controls"), "{line}");
    }

    // -- ACTION SELECTION: ORDINAL ----------------------------------------

    #[test]
    fn selects_by_ordinal_over_all_controls() {
        let els = sample();
        // "the third" over ALL controls: Submit(0), Cancel(1), Help(2) -> Help.
        assert_eq!(select_target("click the third", &els), Ok(2));
        assert_eq!(select_target("first", &els), Ok(0));
    }

    #[test]
    fn ordinal_with_role_noun_filters_to_that_role() {
        let els = sample();
        // "the second button": Submit(0), Cancel(1) -> Cancel.
        assert_eq!(select_target("click the second button", &els), Ok(1));
        // digit + suffix form, role-filtered.
        assert_eq!(select_target("press the 1st button", &els), Ok(0));
    }

    #[test]
    fn ordinal_past_the_end_is_honest_out_of_range_not_a_wrong_click() {
        let els = sample();
        match select_target("click the fifth button", &els) {
            Err(SelectError::OutOfRange { requested, available, .. }) => {
                assert_eq!(requested, 5);
                assert_eq!(available, 2, "there are two buttons");
            }
            other => panic!("expected out-of-range, got {other:?}"),
        }
    }

    // -- ACTION SELECTION: NAME -------------------------------------------

    #[test]
    fn selects_by_exact_name() {
        let els = sample();
        assert_eq!(select_target("click Submit", &els), Ok(0));
        assert_eq!(select_target("press the Cancel button", &els), Ok(1));
        // Case-insensitive.
        assert_eq!(select_target("click submit", &els), Ok(0));
    }

    #[test]
    fn selects_by_unique_substring() {
        let els = vec![
            el("Sign in with Apple", ElementRole::Button, Some((1, 1))),
            el("Create account", ElementRole::Button, Some((2, 2))),
        ];
        assert_eq!(select_target("click sign in", &els), Ok(0));
    }

    #[test]
    fn ambiguous_name_refuses_never_guesses() {
        let els = vec![
            el("Save Draft", ElementRole::Button, Some((1, 1))),
            el("Save As", ElementRole::Button, Some((2, 2))),
        ];
        // "save" is a substring of BOTH and an exact match of NEITHER -> ambiguous
        // refusal, NOT a wrong click.
        match select_target("click save", &els) {
            Err(SelectError::Ambiguous(labels)) => assert_eq!(labels.len(), 2),
            other => panic!("expected ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn exact_match_wins_over_ambiguous_substring() {
        let els = vec![
            el("Save", ElementRole::Button, Some((1, 1))),
            el("Save As", ElementRole::Button, Some((2, 2))),
        ];
        // An EXACT "save" resolves the otherwise-ambiguous pair to the exact one.
        assert_eq!(select_target("click the Save button", &els), Ok(0));
    }

    #[test]
    fn missing_name_is_honest_not_found() {
        let els = sample();
        match select_target("click the Print button", &els) {
            Err(SelectError::NotFound(what)) => assert!(what.contains("print")),
            other => panic!("expected not-found, got {other:?}"),
        }
    }

    #[test]
    fn empty_and_verb_only_phrases_refuse() {
        let els = sample();
        assert_eq!(select_target("", &els), Err(SelectError::EmptyPhrase));
        assert_eq!(select_target("   ", &els), Err(SelectError::EmptyPhrase));
        // Only a verb + article + role noun, no name -> nothing to select.
        assert_eq!(select_target("click the button", &els), Err(SelectError::EmptyPhrase));
    }

    #[test]
    fn no_elements_refuses() {
        assert_eq!(select_target("click Submit", &[]), Err(SelectError::NoElements));
    }

    // -- VOICE-ACTION BRIDGE (builds the UNCHANGED-capstone request) -------

    #[test]
    fn resolve_builds_a_single_click_request_for_the_selected_target() {
        let els = sample();
        let req = resolve_voice_action("click the second button", &els).unwrap();
        assert_eq!(req.target_desc, "Cancel");
        match req.action {
            crate::ui_automation::Action::Click { x, y } => assert_eq!((x, y), (300, 200)),
            other => panic!("expected a click, got {other:?}"),
        }
    }

    #[test]
    fn resolve_refuses_when_selection_is_ambiguous_or_missing() {
        let els = sample();
        // A miss never becomes a request (never a wrong click).
        assert!(matches!(
            resolve_voice_action("click the Print button", &els),
            Err(SelectError::NotFound(_))
        ));
    }

    #[test]
    fn resolve_refuses_a_located_element_with_no_point() {
        // An element with no center is narratable but NOT clickable.
        let els = vec![el("Ghost", ElementRole::Button, None)];
        match resolve_voice_action("click Ghost", &els) {
            Err(SelectError::NoLocation(what)) => assert_eq!(what, "Ghost"),
            other => panic!("expected no-location refusal, got {other:?}"),
        }
    }

    #[test]
    fn selection_never_yields_a_plain_text_block_as_a_control_target() {
        let els = sample();
        // "the fourth" over controls is Search (the field), NOT the plain-text
        // block — plain text is never an actionable target for an ordinal.
        assert_eq!(select_target("the fourth", &els), Ok(3));
        // There is no fifth CONTROL (the text block is excluded).
        assert!(matches!(
            select_target("click the fifth control", &els),
            Err(SelectError::OutOfRange { .. })
        ));
    }

    // -- CONTINUOUS NARRATION GATE (opt-in; OFF is a no-op) ----------------

    #[test]
    fn continuous_narration_off_is_a_no_op() {
        let _g = serial();
        install_settings(false, 20);
        assert!(!is_narrating());
        // OFF: even with a focused element, the focus-change path speaks NOTHING.
        assert_eq!(narrate_on_focus_change(Some(&btn("Send"))), None);
        assert_eq!(narrate_on_focus_change(None), None);
    }

    #[test]
    fn continuous_narration_on_speaks_the_focus() {
        let _g = serial();
        install_settings(true, 20);
        assert!(is_narrating());
        assert_eq!(
            narrate_on_focus_change(Some(&btn("Send"))),
            Some("Send, a button.".to_string())
        );
        // ON, but nothing focused -> the honest empty (still not fabricated).
        let none = narrate_on_focus_change(None).unwrap();
        assert!(none.to_lowercase().contains("nothing is focused"));
        // Restore the OFF default for any later test.
        install_settings(false, 20);
    }

    #[test]
    fn max_controls_is_installed_and_floored() {
        let _g = serial();
        install_settings(false, 12);
        assert_eq!(max_controls(), 12);
        // A 0 bound would read nothing — floored to >= 1.
        install_settings(false, 0);
        assert_eq!(max_controls(), 1);
        // Restore a sane default for any later test.
        install_settings(false, 20);
    }

    // -- TELEMETRY: SECRET-FREE -------------------------------------------

    #[test]
    fn narration_frame_is_secret_free() {
        let secret = el("password: hunter2", ElementRole::TextField, Some((5, 5)));
        let frame = narration_event_frame(&secret);
        let s = frame.to_string();
        // The raw label NEVER appears — only the role, the length, and a bool.
        assert!(!s.contains("hunter2"), "label leaked into telemetry: {s}");
        assert_eq!(frame["role"], "text_field");
        assert_eq!(frame["label_len"], "password: hunter2".chars().count());
        assert_eq!(frame["has_location"], true);
    }

    #[test]
    fn action_frame_carries_only_class_not_content() {
        let els = sample();
        let ok = select_target("click Submit", &els);
        let f = action_frame(els.len(), &ok);
        assert_eq!(f["selected"], true);
        assert_eq!(f["refusal"], "none");

        let miss = select_target("click the Print button", &els);
        let f = action_frame(els.len(), &miss);
        assert_eq!(f["selected"], false);
        assert_eq!(f["refusal"], "not_found");
        // No phrase / label content in the frame.
        assert!(!f.to_string().to_lowercase().contains("print"));
    }

    #[test]
    fn status_frame_reports_only_the_gate() {
        assert_eq!(status_frame(false), json!({"narrate": false}));
        assert_eq!(status_frame(true), json!({"narrate": true}));
    }

    #[test]
    fn resolved_action_frame_carries_only_class_not_content() {
        let els = sample();
        // A resolved click -> selected, no refusal, no coordinate/label leaked.
        let ok = resolve_voice_action("click the second button", &els);
        let f = resolved_action_frame(els.len(), &ok);
        assert_eq!(f["selected"], true);
        assert_eq!(f["refusal"], "none");
        assert_eq!(f["controls"], els.len());
        assert!(!f.to_string().contains("300"), "no coordinate leaks: {f}");

        // A selected-but-unlocatable target is honestly NOT resolved (no click
        // point) — the frame says so with the class, never the label.
        let ghost = vec![el("Ghost", ElementRole::Button, None)];
        let no_loc = resolve_voice_action("click Ghost", &ghost);
        let f = resolved_action_frame(ghost.len(), &no_loc);
        assert_eq!(f["selected"], false);
        assert_eq!(f["refusal"], "no_location");
        assert!(!f.to_string().to_lowercase().contains("ghost"), "no label leaks: {f}");
    }

    // -- LAST SCREEN READOUT CACHE (the ACT arm selects over this) ---------

    #[test]
    fn remember_readout_parses_caches_and_bounds_by_max_controls() {
        let _g = serial();
        install_settings(false, 2); // bound the readout to 2 controls
        let readout = json!({
            "controls": [
                {"text": "Send", "center": {"x": 10, "y": 20}, "is_control": true},
                {"text": "Cancel", "center": {"x": 30, "y": 20}, "is_control": true},
                {"text": "Help", "center": {"x": 50, "y": 20}, "is_control": true},
            ]
        });
        remember_readout(&readout);
        let cached = snapshot_controls();
        assert_eq!(cached.len(), 2, "the readout is bounded by max_controls: {cached:?}");
        assert_eq!(cached[0].label, "Send");
        // The ACT arm can now select the SECOND of what was just read.
        assert_eq!(select_target("click the second", &cached), Ok(1));
        clear_controls();
        assert!(snapshot_controls().is_empty(), "clear empties the readout");
        install_settings(false, 20); // restore the shipped default
    }

    #[test]
    fn empty_cache_makes_an_action_refuse_read_the_screen_first() {
        let _g = serial();
        clear_controls();
        let controls = snapshot_controls();
        // Nothing read yet -> a voice action REFUSES honestly, never a guess.
        assert_eq!(
            resolve_voice_action("click the third button", &controls).unwrap_err(),
            SelectError::NoElements
        );
    }
}

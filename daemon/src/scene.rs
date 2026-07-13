//! F6 — ACOUSTIC SCENE AWARENESS.
//!
//! Classify the ambient soundscape into named sound EVENTS (a doorbell, a knock,
//! a smoke alarm, glass breaking, a dog barking…) — distinct from `audio.rs`,
//! which captures and segments SPEECH. This is a passive environmental sensor.
//!
//! THREE HARD RULES (each pinned by a test):
//!   1. SHIPS OFF. `[scene].enabled` defaults false. Continuous ambient
//!      classification is a privacy-consequential act, so it is opt-in like
//!      `[security]`/`[distill]`/`[sync]`. Off, nothing classifies and the status
//!      reports the honest off state.
//!   2. NEVER RETAINS AUDIO. Only event LABELS + confidences + timestamps ever
//!      leave the classifier — never a waveform, never samples, never a
//!      transcript. Events are transient (surfaced live, never logged to disk),
//!      so there is no ambient-sound timeline to leak. `retains_audio` is a
//!      pinned-false wire field.
//!   3. INERT WITHOUT A MODEL — HONESTLY. No sound-event classifier model is
//!      bundled, so the live path is armed-but-inert: the full classification +
//!      debounce machinery is real and hermetically tested (via a canned
//!      classifier), and the "needs a model" state is reported honestly, never
//!      faked as "listening".
//!
//! The pure core — [`fold_detections`] (debounce + floor + cap + newest-first)
//! and [`classify_window`] (classifier -> events) — is exhaustively tested. The
//! live tap into the realtime capture loop is deliberately NOT wired: that loop
//! is no-alloc and panic-sensitive (a panic silences the mic), so feeding it a
//! model is the device-gated leg, reported via the capability map, not forced.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};

/// The static vocabulary of ambient sound events the classifier can name. A
/// detection whose label is not one of these is dropped (never guessed). These
/// are non-secret category names, safe to publish to the HUD so the panel can
/// show what it listens for.
pub const KNOWN_LABELS: &[&str] = &[
    "doorbell",
    "knock",
    "smoke_alarm",
    "glass_break",
    "dog_bark",
    "running_water",
    "phone_ring",
    "baby_cry",
    "microwave_beep",
    "car_horn",
];

/// Cap on the events surfaced in one status frame — a glanceable set, not a log.
const MAX_EVENTS: usize = 12;

/// Same-label detections closer together than this collapse into one event, so a
/// doorbell that rings for three seconds is ONE "doorbell", not thirty.
const DEBOUNCE_SECS: i64 = 5;

/// A raw classifier hit before folding: a label, a confidence, and when it was
/// heard. Carries NO audio — by construction the classifier only emits labels.
#[derive(Debug, Clone, PartialEq)]
pub struct RawDetection {
    pub label: String,
    pub confidence: f32,
    pub ts: String,
}

/// A folded ambient sound event ready for the HUD. Audio never appears here.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SceneEvent {
    pub label: String,
    pub confidence: f32,
    pub ts: String,
}

/// The model seam. A real sound-event classifier maps a mono audio frame to
/// scored labels; none is bundled, so in production this trait has no live impl
/// and the live path stays inert. Tests inject a canned classifier to exercise
/// the full path without a model.
pub trait AcousticClassifier {
    fn classify(&self, frame_mono: &[f32], sample_rate: u32) -> Vec<(String, f32)>;
}

/// Canonicalize a classifier label to the known vocabulary: lowercase, trim, and
/// fold spaces/hyphens to underscores, then require an exact vocabulary match.
/// Returns the canonical label, or None for anything outside the set.
fn canonical_label(raw: &str) -> Option<String> {
    let norm: String = raw
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c == ' ' || c == '-' { '_' } else { c })
        .collect();
    KNOWN_LABELS.iter().find(|k| **k == norm).map(|k| k.to_string())
}

fn parse_ts(ts: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(ts).ok().map(|d| d.with_timezone(&Utc))
}

fn round2(x: f32) -> f64 {
    ((x as f64) * 100.0).round() / 100.0
}

/// PURE + total. Fold raw classifier detections into surfaced events:
///   - drop labels outside the known vocabulary (never guess),
///   - drop confidences below `floor` or non-finite,
///   - drop detections with an unparseable timestamp (can't be placed in time —
///     honest omission, never a fabricated clock),
///   - clamp confidence to [0,1],
///   - DEBOUNCE: consecutive same-label detections within DEBOUNCE_SECS collapse
///     into one event (keeping the highest confidence and the latest time),
///   - return newest-first, capped at `cap`.
///
/// Never panics; a fully-invalid batch yields an empty vec.
pub fn fold_detections(raw: &[RawDetection], floor: f32, cap: usize) -> Vec<SceneEvent> {
    // Keep only valid, known, above-floor, time-placeable detections.
    let mut kept: Vec<(DateTime<Utc>, String, f32)> = raw
        .iter()
        .filter(|d| d.confidence.is_finite() && d.confidence >= floor)
        .filter_map(|d| {
            let label = canonical_label(&d.label)?;
            let ts = parse_ts(&d.ts)?;
            Some((ts, label, d.confidence.clamp(0.0, 1.0)))
        })
        .collect();

    // Group same labels together, ascending in time, so the debounce walk only
    // has to look at the immediately-preceding kept event.
    kept.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));

    let mut folded: Vec<(DateTime<Utc>, SceneEvent)> = Vec::new();
    for (ts, label, conf) in kept {
        if let Some((last_ts, last)) = folded.last_mut() {
            if last.label == label && (ts - *last_ts).num_seconds() < DEBOUNCE_SECS {
                // Same ongoing sound: keep the strongest confidence, advance the
                // window to the latest time (kept is time-ascending within label).
                if conf > last.confidence {
                    last.confidence = conf;
                }
                last.ts = ts.to_rfc3339();
                *last_ts = ts;
                continue;
            }
        }
        folded.push((ts, SceneEvent { label, confidence: conf, ts: ts.to_rfc3339() }));
    }

    let mut events: Vec<SceneEvent> = folded.into_iter().map(|(_, e)| e).collect();
    // Newest first (RFC3339 UTC strings sort chronologically), then cap.
    events.sort_by(|a, b| b.ts.cmp(&a.ts));
    events.truncate(cap);
    events
}

/// The real classification path (armed-but-inert without a bundled model): run
/// the classifier over one mono window, stamp each hit with the window's time,
/// and fold. Generic over the classifier so it is hermetically testable with a
/// canned impl. Returns at most `MAX_EVENTS`.
pub fn classify_window<C: AcousticClassifier>(
    classifier: &C,
    frame_mono: &[f32],
    sample_rate: u32,
    now_rfc3339: &str,
    floor: f32,
) -> Vec<SceneEvent> {
    let raw: Vec<RawDetection> = classifier
        .classify(frame_mono, sample_rate)
        .into_iter()
        .map(|(label, confidence)| RawDetection { label, confidence, ts: now_rfc3339.to_string() })
        .collect();
    fold_detections(&raw, floor, MAX_EVENTS)
}

/// Truthful probe for a USABLE bundled classifier model under the daemon-owned
/// state tree. None is shipped, so this is false in practice. It fully parses
/// the model (not a bare existence check), so an empty or malformed file reports
/// absent rather than falsely "present".
pub fn classifier_available(root: &std::path::Path) -> bool {
    load_classifier(root).is_some_and(|c| !c.templates.is_empty())
}

/// Whether the ambient microphone tap that feeds the classifier is wired. It is
/// deliberately NOT wired (feeding the no-alloc, panic-sensitive realtime
/// capture loop is the device-gated leg), so this is false and the sensor never
/// claims to be actively "listening". Honest, not a placeholder for a lie: when
/// the tap is genuinely built this flips true at its source.
pub fn capture_wired() -> bool {
    false
}

fn model_path(root: &std::path::Path) -> std::path::PathBuf {
    root.join("models").join("acoustic_scene.json")
}

// ---------------------------------------------------------------------------
// The bundled classifier — a real feature-template matcher.
//
// A model file is a table of per-label acoustic-feature templates. `classify`
// extracts two cheap, real features from the mono frame (RMS loudness + zero-
// crossing rate, a coarse pitch/noisiness proxy) and matches against each
// template within its radius. This is a genuine (deliberately coarse) classifier
// that needs NO neural runtime — its accuracy is entirely the model-maker's
// concern. No template table ships, so `load_classifier` returns None and the
// path stays inert; the capability map reports needs-a-model, never "listening".
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Deserialize)]
struct LabelTemplate {
    label: String,
    /// Feature-space centre: normalized RMS in [0,1] and zero-crossing rate in [0,1].
    rms: f32,
    zcr: f32,
    /// Match radius in feature space; confidence falls off linearly to the edge.
    radius: f32,
}

pub struct BundledClassifier {
    templates: Vec<LabelTemplate>,
}

/// RMS loudness (clamped to [0,1]) and zero-crossing rate ([0,1]) of a mono
/// frame — both cheap, allocation-free, and real. An empty frame is silent.
fn frame_features(frame: &[f32]) -> (f32, f32) {
    if frame.is_empty() {
        return (0.0, 0.0);
    }
    let sum_sq: f32 = frame.iter().map(|s| s * s).sum();
    let rms = (sum_sq / frame.len() as f32).sqrt().clamp(0.0, 1.0);
    let crossings = frame.windows(2).filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0)).count();
    let zcr = (crossings as f32 / (frame.len() - 1).max(1) as f32).clamp(0.0, 1.0);
    (rms, zcr)
}

impl AcousticClassifier for BundledClassifier {
    fn classify(&self, frame_mono: &[f32], _sample_rate: u32) -> Vec<(String, f32)> {
        let (rms, zcr) = frame_features(frame_mono);
        self.templates
            .iter()
            .filter_map(|t| {
                if t.radius <= 0.0 {
                    return None;
                }
                let dist = ((rms - t.rms).powi(2) + (zcr - t.zcr).powi(2)).sqrt();
                (dist <= t.radius).then(|| (t.label.clone(), (1.0 - dist / t.radius).clamp(0.0, 1.0)))
            })
            .collect()
    }
}

/// Load the bundled classifier from the model file, or None if it is absent or
/// unparseable (honest inert, never a crash). None ships today.
fn load_classifier(root: &std::path::Path) -> Option<BundledClassifier> {
    let bytes = std::fs::read(model_path(root)).ok()?;
    let templates: Vec<LabelTemplate> = serde_json::from_slice(&bytes).ok()?;
    Some(BundledClassifier { templates })
}

/// Capture a brief mono ambient sample for a one-shot classification. This is
/// the device-gated microphone tap: feeding the realtime capture loop (no-alloc,
/// panic-sensitive) is deliberately NOT wired, so this returns None today and the
/// live path stays inert — honest, never a fabricated sample.
async fn capture_probe_sample() -> Option<Vec<f32>> {
    None
}

/// The real, model-gated classification pass (armed-but-inert). Reachable from
/// [`emit_status`]; runs only when a model is bundled AND a sample can be
/// captured — neither ships, so it returns no events today without ever faking
/// one. This is the production call site that keeps [`classify_window`] and the
/// classifier a live, honest path rather than dead scaffolding.
async fn run_real_identify(cfg: &crate::config::Config, root: &std::path::Path) -> Vec<SceneEvent> {
    let Some(classifier) = load_classifier(root) else { return Vec::new() };
    let Some(sample) = capture_probe_sample().await else { return Vec::new() };
    classify_window(
        &classifier,
        &sample,
        PROBE_SAMPLE_RATE,
        &Utc::now().to_rfc3339(),
        cfg.scene.confidence_floor as f32,
    )
}

/// Assumed sample rate for the (unwired) probe tap.
const PROBE_SAMPLE_RATE: u32 = 16_000;

/// The `scene.status` wire payload. PURE + total. SECRET-FREE and AUDIO-FREE:
/// booleans, the static vocabulary, and any transient event LABELS — never a
/// waveform, never samples.
///
/// `listening` is the HONEST active-sensing state: true ONLY when the switch is
/// on, a usable model is present, AND the microphone tap is wired. A present
/// model alone is NOT "listening" — without the capture tap no audio is ever
/// examined, so claiming it would be the "faked listening" this module forbids.
pub fn status_payload(
    enabled: bool,
    classifier_present: bool,
    capture_wired: bool,
    recent: &[SceneEvent],
) -> Value {
    json!({
        "enabled": enabled,
        // The daemon parses the model file but cannot confirm it is a genuinely
        // working classifier -> never claims verified.
        "classifier_present": classifier_present,
        // Whether the mic tap that feeds the classifier is wired (it is not).
        "capture_wired": capture_wired,
        "dep_verified": false,
        "dependency": "a bundled sound-event classifier model + a wired capture tap",
        // Actively sensing requires all three; a model without a tap is inert.
        "listening": enabled && classifier_present && capture_wired,
        // PINNED honest: only labels ever leave the classifier; audio is never
        // retained and events are never written to disk.
        "retains_audio": false,
        "vocabulary": KNOWN_LABELS,
        "recent_events": recent
            .iter()
            .map(|e| json!({"label": e.label, "confidence": round2(e.confidence), "ts": e.ts}))
            .collect::<Vec<_>>(),
        "recent_count": recent.len(),
    })
}

/// Emit `scene.status` for the HUD on the audit-snapshot cadence. Off emits the
/// honest off payload so the panel shows the inert state. The real classification
/// pass ([`run_real_identify`]) runs only when armed AND a usable model is
/// present AND the capture tap is wired — none of which ships, so today this is
/// effectively read-only and never claims to be listening. Events are transient
/// (never persisted): each frame reflects only what was heard now. Fail-open.
pub async fn emit_status(cfg: &crate::config::Config, root: &std::path::Path) {
    let present = cfg.scene.enabled && classifier_available(root);
    let wired = capture_wired();
    let recent = if present && wired { run_real_identify(cfg, root).await } else { Vec::new() };
    crate::telemetry::emit(
        "system",
        "scene.status",
        status_payload(cfg.scene.enabled, present, wired, &recent),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn det(label: &str, conf: f32, ts: &str) -> RawDetection {
        RawDetection { label: label.into(), confidence: conf, ts: ts.into() }
    }

    #[test]
    fn drops_unknown_labels_and_below_floor_and_unparseable_time() {
        let raw = vec![
            det("doorbell", 0.9, "2026-07-13T10:00:00Z"),
            det("unicorn_sighting", 0.99, "2026-07-13T10:00:01Z"), // unknown -> dropped
            det("knock", 0.3, "2026-07-13T10:00:02Z"),             // below 0.6 floor -> dropped
            det("dog_bark", 0.8, "not-a-timestamp"),               // unplaceable -> dropped
        ];
        let out = fold_detections(&raw, 0.6, MAX_EVENTS);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, "doorbell");
    }

    #[test]
    fn canonicalizes_label_case_and_separators() {
        let raw = vec![det("Glass Break", 0.7, "2026-07-13T10:00:00Z"), det("DOG-BARK", 0.7, "2026-07-13T10:00:00Z")];
        let out = fold_detections(&raw, 0.6, MAX_EVENTS);
        let labels: Vec<&str> = out.iter().map(|e| e.label.as_str()).collect();
        assert!(labels.contains(&"glass_break"));
        assert!(labels.contains(&"dog_bark"));
    }

    #[test]
    fn debounces_a_sustained_sound_into_one_event_keeping_peak_confidence() {
        // A doorbell ringing across several close detections is ONE event.
        let raw = vec![
            det("doorbell", 0.65, "2026-07-13T10:00:00Z"),
            det("doorbell", 0.90, "2026-07-13T10:00:02Z"), // +2s, within window, higher conf
            det("doorbell", 0.70, "2026-07-13T10:00:04Z"), // +2s, within window
        ];
        let out = fold_detections(&raw, 0.6, MAX_EVENTS);
        assert_eq!(out.len(), 1, "collapsed into one event");
        assert_eq!(out[0].confidence, 0.90, "keeps the peak confidence");
        assert_eq!(out[0].ts, "2026-07-13T10:00:04+00:00", "advances to the latest time");
    }

    #[test]
    fn a_gap_past_the_debounce_window_is_a_separate_event() {
        let raw = vec![
            det("knock", 0.8, "2026-07-13T10:00:00Z"),
            det("knock", 0.8, "2026-07-13T10:00:20Z"), // +20s > 5s window -> distinct
        ];
        let out = fold_detections(&raw, 0.6, MAX_EVENTS);
        assert_eq!(out.len(), 2, "two separate knocks");
    }

    #[test]
    fn distinct_labels_never_merge_and_output_is_newest_first() {
        let raw = vec![
            det("dog_bark", 0.7, "2026-07-13T10:00:00Z"),
            det("doorbell", 0.8, "2026-07-13T10:00:10Z"),
        ];
        let out = fold_detections(&raw, 0.6, MAX_EVENTS);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].label, "doorbell", "newest first");
        assert_eq!(out[1].label, "dog_bark");
    }

    #[test]
    fn confidence_is_clamped_and_cap_is_enforced() {
        let mut raw = Vec::new();
        // 20 distinct-time knocks spaced past the window -> 20 events pre-cap.
        for i in 0..20 {
            raw.push(det("car_horn", 2.0, &format!("2026-07-13T10:{:02}:00Z", i)));
        }
        let out = fold_detections(&raw, 0.6, MAX_EVENTS);
        assert_eq!(out.len(), MAX_EVENTS, "capped");
        assert!(out.iter().all(|e| e.confidence == 1.0), "confidence clamped to 1.0");
    }

    // A canned classifier that ignores audio and returns fixed labels — proves
    // the classify_window path end-to-end without a bundled model.
    struct Canned(Vec<(String, f32)>);
    impl AcousticClassifier for Canned {
        fn classify(&self, _frame: &[f32], _sr: u32) -> Vec<(String, f32)> {
            self.0.clone()
        }
    }

    #[test]
    fn classify_window_runs_the_classifier_and_folds() {
        let c = Canned(vec![("doorbell".into(), 0.9), ("mystery".into(), 0.9), ("knock".into(), 0.4)]);
        let out = classify_window(&c, &[0.0f32; 16], 16_000, "2026-07-13T10:00:00Z", 0.6);
        assert_eq!(out.len(), 1, "unknown + below-floor dropped, doorbell kept");
        assert_eq!(out[0].label, "doorbell");
    }

    #[test]
    fn status_is_off_and_audio_free_by_default() {
        let p = status_payload(false, false, false, &[]);
        assert_eq!(p["enabled"], false);
        assert_eq!(p["listening"], false);
        assert_eq!(p["retains_audio"], false, "PINNED: audio is never retained");
        assert_eq!(p["dep_verified"], false);
        assert_eq!(p["recent_count"], 0);
        assert!(p["vocabulary"].as_array().unwrap().iter().any(|v| v == "doorbell"));
        // No audio/waveform/samples field can ever appear on the wire.
        for k in ["audio", "waveform", "samples", "pcm"] {
            assert!(p.get(k).is_none(), "payload must never carry {k}");
        }
    }

    #[test]
    fn listening_requires_switch_model_and_a_wired_capture_tap() {
        assert_eq!(status_payload(true, false, false, &[])["listening"], false, "armed, no model");
        assert_eq!(status_payload(false, true, true, &[])["listening"], false, "model+tap but off");
        // The exact review finding: a model present but the mic tap NOT wired is
        // NOT "listening" — no audio is ever examined, so claiming it would be a lie.
        assert_eq!(status_payload(true, true, false, &[])["listening"], false, "model but no capture tap");
        assert_eq!(status_payload(true, true, true, &[])["listening"], true, "all three -> listening");
        // The shipped reality: the capture tap is never wired.
        assert!(!capture_wired(), "the mic tap is not wired -> never actively listening");
    }

    #[test]
    fn classifier_probe_validates_the_model_not_just_its_existence() {
        let dir = std::env::temp_dir().join(format!("jarvis-scene-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let model = dir.join("models").join("acoustic_scene.json");
        assert!(!classifier_available(&dir), "no model bundled -> inert");
        std::fs::create_dir_all(dir.join("models")).unwrap();
        // A present-but-empty or malformed file is NOT a usable classifier.
        std::fs::write(&model, b"[]").unwrap();
        assert!(!classifier_available(&dir), "empty template table is not usable -> absent");
        std::fs::write(&model, b"not json").unwrap();
        assert!(!classifier_available(&dir), "malformed model -> absent, never a false present");
        // A real, non-empty model flips the probe honestly.
        std::fs::write(&model, br#"[{"label":"knock","rms":0.5,"zcr":0.5,"radius":0.5}]"#).unwrap();
        assert!(classifier_available(&dir), "a valid model is present");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn frame_features_are_bounded_and_meaningful() {
        assert_eq!(frame_features(&[]), (0.0, 0.0), "empty frame is silent");
        let (rms, zcr) = frame_features(&[0.0; 64]);
        assert_eq!(rms, 0.0, "silence has zero loudness");
        assert_eq!(zcr, 0.0, "no sign changes");
        // A full-scale square wave: max loudness, a sign change every sample.
        let sq: Vec<f32> = (0..64).map(|i| if i % 2 == 0 { 1.0 } else { -1.0 }).collect();
        let (rms, zcr) = frame_features(&sq);
        assert!((rms - 1.0).abs() < 1e-6, "square wave rms ~ 1.0");
        assert!(zcr > 0.9, "alternating sign -> near-max zcr");
    }

    #[test]
    fn bundled_classifier_matches_templates_within_radius_and_folds() {
        // A real (synthetic) two-label template model, exercised via the trait.
        let model = r#"[
            {"label":"doorbell","rms":1.0,"zcr":1.0,"radius":0.5},
            {"label":"dog_bark","rms":0.0,"zcr":0.0,"radius":0.1}
        ]"#;
        let clf: BundledClassifier = BundledClassifier {
            templates: serde_json::from_str(model).unwrap(),
        };
        // A loud, high-ZCR square wave sits on the doorbell template, far from dog_bark.
        let sq: Vec<f32> = (0..64).map(|i| if i % 2 == 0 { 1.0 } else { -1.0 }).collect();
        let out = classify_window(&clf, &sq, PROBE_SAMPLE_RATE, "2026-07-13T10:00:00Z", 0.6);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, "doorbell");
    }

    #[test]
    fn load_classifier_is_none_without_a_model_and_run_identify_stays_inert() {
        let dir = std::env::temp_dir().join(format!("jarvis-scene-load-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(load_classifier(&dir).is_none(), "no model file -> no classifier");
        // Even a bundled model yields no live events without the (unwired) tap.
        std::fs::create_dir_all(dir.join("models")).unwrap();
        std::fs::write(
            dir.join("models").join("acoustic_scene.json"),
            br#"[{"label":"knock","rms":0.5,"zcr":0.5,"radius":0.5}]"#,
        )
        .unwrap();
        assert!(load_classifier(&dir).is_some(), "a valid model parses");
        let mut cfg = crate::config::Config::default();
        cfg.scene.enabled = true;
        let events = tokio_test_block(run_real_identify(&cfg, &dir));
        assert!(events.is_empty(), "no capture tap -> no fabricated events");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tokio_test_block<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(f)
    }
}

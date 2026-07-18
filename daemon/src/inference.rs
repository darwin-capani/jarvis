use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::{info, warn};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Per-attempt UnixStream::connect ceiling. A live server accepts in <1ms;
/// without this cap a kernel that accepts the connect() but never lets the
/// server `accept()` (a wedged/half-up server) could hang `ensure_connected`
/// indefinitely. Short on purpose: the socket is local — a connect that does
/// not complete in a second is not coming up this attempt.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
/// Reconnect backoff for a dropped/restarted/flapping inference server. The
/// per-op retry loop ([`request_generic`]/[`request_raw`]) walks this schedule
/// so a dead server does NOT make every op pay full 30s timeout ceilings, and a
/// flapping server is rate-limited instead of hammered. The contract is
/// unchanged on the happy path (the first attempt connects + succeeds with no
/// sleep) and on the honest-failure path (exhaustion still returns the last
/// transport Err, never a fake success). See [`backoff_delay`] for the exact
/// schedule. Bounded: at most [`RECONNECT_MAX_ATTEMPTS`] connect attempts.
const RECONNECT_MAX_ATTEMPTS: u32 = 4;
/// First backoff step; each subsequent step doubles up to [`RECONNECT_MAX_DELAY`].
const RECONNECT_BASE_DELAY: Duration = Duration::from_millis(50);
/// Backoff ceiling — a single op never waits longer than this between attempts,
/// so even a hard-down server bounds total added latency to well under the 30s
/// op timeout (50+100+200 ≈ 350ms of sleep across 4 attempts at this ceiling).
const RECONNECT_MAX_DELAY: Duration = Duration::from_millis(400);
/// op=consolidate only: the largest generation in the system (up to 40
/// transcript pairs + 200 facts prefilled into the 4B on the M1 Pro), and its
/// 30s window used to include server-side queueing behind the engine lock —
/// a consolidation landing behind a live reply timed out, the server kept
/// generating anyway, and the unstamped cycle re-burned a full generation
/// every 6h forever (audit fix).
const CONSOLIDATE_TIMEOUT: Duration = Duration::from_secs(120);
/// A converse stream must produce its done event within this budget...
const CONVERSE_DONE_TIMEOUT: Duration = Duration::from_secs(30);
/// ...and never go quiet for longer than this between lines.
const CONVERSE_EVENT_TIMEOUT: Duration = Duration::from_secs(15);

const MAX_INFERENCE_LINE_BYTES: usize = 16 * 1024 * 1024; // 16 MiB: inference responses are text/paths/embedding-vectors (images/audio are returned as PATHS, not inlined); well above the server's own 8 MiB request limit, bounds a hijacked inference server from OOMing the daemon.

#[derive(Debug, Clone, Deserialize)]
pub struct Classification {
    pub intent: String,
    pub confidence: f64,
    pub complexity: String,
    /// Pass-through of the classifier's args JSON (e.g. {"url": "apple.com",
    /// "browser": "safari"} for web.open). Old servers omit the field —
    /// serde(default) keeps the daemon backward compatible (Value::Null);
    /// the router treats Null exactly like an empty object.
    #[serde(default)]
    pub args: serde_json::Value,
}

/// One turn of prior conversation, oldest first on the wire.
/// Shared contract: speaker is exactly "user" or "darwin".
#[derive(Serialize)]
struct HistoryTurn<'a> {
    speaker: &'static str,
    text: &'a str,
}

#[derive(Serialize)]
struct Request<'a> {
    id: String,
    op: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
    /// describe_image only (op="describe_image"): the OPTIONAL VQA question.
    /// Absent => the server uses its DESCRIBE_IMAGE_DEFAULT_PROMPT (a general
    /// scene description). NON-secret; carried only when present so an old
    /// server (no VLM op) sees a clean shape it simply rejects as unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    question: Option<&'a str>,
    /// generate_image only (op="generate_image", task #18): the REQUIRED text
    /// prompt to render. NON-secret but PRIVATE — it is handed ONLY to the
    /// on-device MLX diffusion model; the prompt + the pixels stay on the
    /// machine, nothing goes to the cloud. Carried only on the generate_image
    /// path so an old server (no image op) sees a clean shape it rejects as
    /// unknown (which the daemon reads as "image model unavailable").
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<&'a str>,
    /// generate_image only: OPTIONAL square output resolution in pixels. Absent
    /// => the server's GENERATE_IMAGE_DEFAULT_SIZE (512). The server floors/caps
    /// it to [GENERATE_IMAGE_MIN_SIZE, GENERATE_IMAGE_MAX_SIZE]; the daemon also
    /// clamps before sending so a caller can never ask for an out-of-range size.
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u32>,
    /// generate_image only: OPTIONAL sampling steps. Absent => the server's
    /// GENERATE_IMAGE_DEFAULT_STEPS (4). Capped at GENERATE_IMAGE_MAX_STEPS_CAP
    /// (50) by the server; the daemon clamps too so a caller can never request
    /// an unbounded sampler run on-device.
    #[serde(skip_serializing_if = "Option::is_none")]
    steps: Option<u32>,
    /// generate_image only: OPTIONAL integer seed for reproducibility. Absent
    /// => the server derives a time-based 31-bit seed. Carried only when the
    /// caller pins one.
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    voice: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    history: Option<Vec<HistoryTurn<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    facts: Option<&'a [String]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response: Option<&'a str>,
    /// converse only: the instant-acknowledgment line the daemon already
    /// played aloud for this reply; the server tells the model to continue
    /// from it rather than acknowledge twice.
    #[serde(skip_serializing_if = "Option::is_none")]
    opener_spoken: Option<&'a str>,
    /// converse only: the active agent's persona name. The server maps it to
    /// inference/personas/<persona>.txt and uses that text as the system
    /// prefix for this one reply (per-agent voicing), falling back to its
    /// default persona when absent — so an old server simply ignores it and
    /// every agent speaks in the base DARWIN persona. The daemon passes the
    /// agent NAME (not the file text) so the server can KV-cache each agent's
    /// prefix; see this module's wire-contract note for what server.py must
    /// accept.
    #[serde(skip_serializing_if = "Option::is_none")]
    persona: Option<&'a str>,
    /// generate/converse only (multi-resident LOCAL, task #17): the warm-set id
    /// the Local tier sub-choice picked (a "local-fast" model vs the capable
    /// base). The server SELECTS this warm local model for the reply, keeping it
    /// warm under the RAM budget. UNKNOWN / absent / "" -> the base
    /// single-resident model, NEVER an error — so an old server (no manager) or a
    /// single-resident config simply answers on the base. Carried on the wire only
    /// when present, so the default single-resident wire is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    local_model: Option<&'a str>,
    /// embed only: the batch of strings to embed (query + candidate facts).
    /// Old servers without the embed op simply reject op=embed (unknown op) —
    /// the daemon treats that as "embedder unavailable" and falls back to BM25.
    #[serde(skip_serializing_if = "Option::is_none")]
    texts: Option<&'a [String]>,
    /// rerank only (STAGE TWO): the search query the candidate `passages` are
    /// re-scored against. Carried ONLY on op=rerank; an old server without the
    /// rerank op rejects it (unknown op) and the daemon keeps the dense order.
    #[serde(skip_serializing_if = "Option::is_none")]
    query: Option<&'a str>,
    /// rerank only (STAGE TWO): the dense top-K candidate texts to re-score against
    /// `query`. One relevance score comes back per passage, in this order. Bounded
    /// K (the daemon reranks a small shortlist). Carried ONLY on op=rerank.
    #[serde(skip_serializing_if = "Option::is_none")]
    passages: Option<&'a [String]>,
    /// speak only: the TTS backend the daemon chose for THIS sentence —
    /// "elevenlabs" for the cloud voice tier, "kokoro" (or absent) for on-device.
    /// An old server that does not know the field simply ignores it and uses
    /// Kokoro, so the wire stays backward compatible. The daemon only ever sets
    /// this to "elevenlabs" after `voice_tier::resolve_voice_backend` cleared the
    /// full gate (tier on + key + non-Local + mapped); otherwise it is omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    backend: Option<&'a str>,
    /// speak only (backend=elevenlabs): the ElevenLabs voice id for this agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    voice_id: Option<&'a str>,
    /// speak only (backend=elevenlabs): the ElevenLabs model id ([voice].model).
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    /// speak only (backend=elevenlabs): the resolved ElevenLabs API key, read from
    /// the Keychain by the daemon and passed to the server ONLY in this request
    /// body so the server can set the `xi-api-key` header. SECURITY: it is NEVER
    /// logged, never on argv, never in telemetry/Debug; it is `Some` ONLY on the
    /// (gated) cloud path and omitted from the wire entirely on every Kokoro turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    el_key: Option<&'a str>,
    /// speak only (Babel, build 2/2): the TARGET LANGUAGE of this sentence (e.g.
    /// "Spanish", "Japanese"), threaded from the interpreter so the ElevenLabs
    /// backend can select a MULTILINGUAL model (eleven_multilingual_v2 / eleven_v3)
    /// for non-English output instead of the English-centric default. Omitted for
    /// ordinary English replies and on the Kokoro path (an old server simply ignores
    /// it), so the wire stays backward compatible. NON-SECRET (a language name).
    #[serde(skip_serializing_if = "Option::is_none")]
    lang: Option<&'a str>,
    /// speak only (#33 ADAPTIVE PROSODY, EL-v3 path): the inline audio-tag the
    /// expressiveness layer chose for THIS sentence (e.g. "[calm]", "[urgently]").
    /// Set ONLY when the resolved backend is EL-v3-capable AND adaptive prosody is on;
    /// on Kokoro / non-v3 EL / prosody-off it is absent so the wire is byte-for-byte
    /// today's. NON-secret — a delivery hint, never the key/voice id.
    #[serde(skip_serializing_if = "Option::is_none")]
    audio_tag: Option<&'a str>,
    /// speak only (#33, EL-v3 voice-settings): `stability` in [0,1]. Set ONLY on the
    /// rich EL-v3 path; absent on every other path (byte-for-byte today's wire).
    #[serde(skip_serializing_if = "Option::is_none")]
    stability: Option<f32>,
    /// speak only (#33, EL-v3 voice-settings): `style` in [0,1]. Set ONLY on the rich
    /// EL-v3 path; absent on every other path.
    #[serde(skip_serializing_if = "Option::is_none")]
    style: Option<f32>,
    /// speak only (#33/#34 COARSE delivery): a rate multiplier the server honours on
    /// EVERY backend (1.0 = today's neutral rate). Carried ONLY when the shape is
    /// non-neutral (prosody on with a non-Neutral profile, or whisper engaged); a
    /// neutral shape omits it so the default wire is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    rate: Option<f32>,
    /// speak only (#34 WHISPER, coarse delivery): an output volume/gain multiplier in
    /// (0,1] the server applies to the produced WAV (1.0 = today's level). Carried ONLY
    /// when whisper lowered it below 1.0; a full-volume shape omits it so the default
    /// wire is unchanged. A required confirmation is NEVER lowered (apply_whisper
    /// guards it), so this never softens a gate's words below audibility.
    #[serde(skip_serializing_if = "Option::is_none")]
    volume: Option<f32>,
    /// create_pronunciation only: the dictionary's display NAME on ElevenLabs. Carried
    /// only on that provisioning op (a non-secret label), absent on every other op so an
    /// old server sees a clean shape.
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    /// create_pronunciation only: the non-empty list of replacement RULES
    /// ({string_to_replace, type:"alias"|"phoneme", alias|phoneme, ...}). NON-secret
    /// (text rules only — no audio leaves the device). Carried only on that op.
    #[serde(skip_serializing_if = "Option::is_none")]
    rules: Option<&'a [PronunciationRule]>,
    /// sound_effect only: OPTIONAL cue length in seconds (server clamps to EL's
    /// 0.5-22s window). Absent => the server's default duration. Carried only when the
    /// caller pins one.
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_s: Option<f32>,
    /// sound_effect only: OPTIONAL prompt-influence in [0,1] (server clamps). Absent =>
    /// the server's default. Carried only when the caller pins one.
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_influence: Option<f32>,
    /// compose_music only: OPTIONAL track length in MILLISECONDS (the server clamps to
    /// its 3000..600000 window and DEFAULTS to 30000 when absent). Carried only when the
    /// caller pins one; absent => the server's default duration. Carried only on the
    /// compose_music path so an old server (no music op) sees a clean shape it rejects
    /// as unknown (which the daemon reads as "music unavailable").
    #[serde(skip_serializing_if = "Option::is_none")]
    length_ms: Option<u32>,
    /// speak only (ADDITIVE): the active ElevenLabs pronunciation-dictionary locators
    /// ([{pronunciation_dictionary_id, version_id?}]) the daemon minted via
    /// op=create_pronunciation. NON-secret ids only. Carried ONLY when a non-empty list
    /// is threaded ([voice].pronunciation_dictionary_id is set); ABSENT (the shipped
    /// default) so the speak wire is byte-for-byte today's and an old server ignores it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pronunciation_locators: Option<Vec<PronunciationLocator<'a>>>,
    /// speak only (ADDITIVE): opt-in low-latency STREAMING TTS. Carried ONLY when
    /// [voice].stream_tts is true (it ships OFF), so the default wire is unchanged; the
    /// server falls back to blocking on any streaming error. NON-secret (a bool).
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
}

/// One ElevenLabs pronunciation replacement RULE on the create_pronunciation wire —
/// a flat passthrough of the server contract ({string_to_replace, type, alias|phoneme}).
/// NON-secret (text rules only). `alias`/`phoneme` are mutually-exclusive per the EL
/// rule type, so both are optional and carried only when present.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PronunciationRule {
    pub string_to_replace: String,
    #[serde(rename = "type")]
    pub rule_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phoneme: Option<String>,
    /// "phoneme"-type rules carry an alphabet ("ipa"/"cmu"); absent for "alias" rules.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alphabet: Option<String>,
}

/// One pronunciation-dictionary LOCATOR on the speak wire — the non-secret
/// (dictionary_id[, version_id]) pair op=create_pronunciation returned. `version_id`
/// is omitted when empty (EL then uses the latest version), so the wire matches the
/// server's `_normalize_pronunciation_locators` contract exactly.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PronunciationLocator<'a> {
    pub pronunciation_dictionary_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_id: Option<&'a str>,
}

impl<'a> Request<'a> {
    fn new(id: String, op: &'a str) -> Self {
        Self {
            id,
            op,
            path: None,
            text: None,
            question: None,
            prompt: None,
            size: None,
            steps: None,
            seed: None,
            max_tokens: None,
            voice: None,
            history: None,
            facts: None,
            data: None,
            response: None,
            opener_spoken: None,
            persona: None,
            local_model: None,
            texts: None,
            query: None,
            passages: None,
            backend: None,
            voice_id: None,
            model: None,
            el_key: None,
            lang: None,
            audio_tag: None,
            stability: None,
            style: None,
            rate: None,
            volume: None,
            name: None,
            rules: None,
            duration_s: None,
            prompt_influence: None,
            length_ms: None,
            pronunciation_locators: None,
            stream: None,
        }
    }
}

/// Thread the EXPRESSIVENESS shaping (#33 prosody + #34 whisper) onto a `speak`
/// request, carrying ONLY the non-neutral fields so a NEUTRAL shape (the OFF default)
/// leaves the request BYTE-FOR-BYTE today's. Factored out of [`InferenceClient::speak`]
/// so the wire shaping is hermetically testable (build a request, apply a real
/// [`crate::prosody::SpeakShape`], assert the JSON) without a server/EL/mic.
///
///   * The RICH EL-v3 surface (`audio_tag`/`stability`/`style`) is gated on
///     `shape.rich`, which the shaper only ever sets true on the EL-v3 backend — so an
///     audio-tag or v3 voice-setting can NEVER ride a Kokoro / non-v3 request even if
///     the struct carried one.
///   * The COARSE `rate`/`volume` ride on EVERY backend, but ONLY when they differ
///     from today's neutral 1.0, so the default wire stays untouched. A whisper-lowered
///     volume rides here; a required-confirmation shape (never lowered by
///     `apply_whisper`) keeps volume at 1.0 and so omits the field.
fn apply_shape_to_request(req: &mut Request<'_>, shape: &crate::prosody::SpeakShape) {
    if shape.rich {
        req.audio_tag = shape.audio_tag;
        req.stability = shape.stability;
        req.style = shape.style;
    }
    if shape.rate != 1.0 {
        req.rate = Some(shape.rate);
    }
    if shape.volume != 1.0 {
        req.volume = Some(shape.volume);
    }
}

/// ADDITIVE speak-wire extras the daemon threads from `[voice]` (Phase-2 wiring):
/// opt-in low-latency streaming TTS (`[voice].stream_tts`, ships OFF) and the active
/// pronunciation-dictionary locator (`[voice].pronunciation_dictionary_id`/`_version`,
/// default empty). BOTH are inert by default — [`SpeakExtras::none`] threads NOTHING,
/// so a default config sends a BYTE-FOR-BYTE-today speak request. The locator carries
/// only NON-secret ids; streaming is a bool. Built once per reply from the config
/// ([`SpeakExtras::from_config`]) and applied by [`apply_extras_to_request`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpeakExtras {
    /// Opt-in streaming TTS: `Some(true)` only when `[voice].stream_tts` is on, else
    /// `None` (the field is omitted from the wire, the server default applies).
    pub stream: Option<bool>,
    /// The active pronunciation dictionary id (`[voice].pronunciation_dictionary_id`);
    /// EMPTY = no locator threaded (today's speech).
    pub pronunciation_dictionary_id: String,
    /// OPTIONAL version (`[voice].pronunciation_dictionary_version`); EMPTY = latest.
    pub pronunciation_dictionary_version: String,
}

impl SpeakExtras {
    /// The inert default — threads NOTHING onto the speak wire (today's request).
    #[allow(dead_code)] // the documented inert-default helper; exercised by the unit tests
    pub fn none() -> Self {
        Self::default()
    }

    /// Derive the speak extras from `[voice]`: opt in to streaming ONLY when
    /// `stream_tts` is true (it ships OFF), and carry the pronunciation locator ONLY
    /// when `pronunciation_dictionary_id` is non-empty (it defaults empty). With both
    /// at their defaults this equals [`SpeakExtras::none`], so the speak request is
    /// UNCHANGED — the threading is purely additive.
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        Self {
            // OPT-IN: only ever Some(true); never sent as Some(false) so the default
            // (off) wire omits the field entirely.
            stream: if cfg.voice.stream_tts { Some(true) } else { None },
            pronunciation_dictionary_id: cfg.voice.pronunciation_dictionary_id.clone(),
            pronunciation_dictionary_version: cfg.voice.pronunciation_dictionary_version.clone(),
        }
    }
}

/// Thread the ADDITIVE speak extras (streaming opt-in + pronunciation locator) onto a
/// `speak` request. Each field is set ONLY when active, so a default [`SpeakExtras`]
/// (the shipped config default) leaves the request BYTE-FOR-BYTE today's. Factored out
/// of [`InferenceClient::speak`] so the additive wiring is hermetically testable
/// (build a request, apply real extras, assert the JSON) without a server/EL.
fn apply_extras_to_request<'a>(req: &mut Request<'a>, extras: &'a SpeakExtras) {
    // Streaming opt-in: only carried when the operator turned it on.
    if extras.stream.is_some() {
        req.stream = extras.stream;
    }
    // Pronunciation locator: only when an active dictionary id is set (non-empty). The
    // version rides only when also non-empty (else the server uses the latest).
    let did = extras.pronunciation_dictionary_id.trim();
    if !did.is_empty() {
        let ver = extras.pronunciation_dictionary_version.trim();
        req.pronunciation_locators = Some(vec![PronunciationLocator {
            pronunciation_dictionary_id: did,
            version_id: if ver.is_empty() { None } else { Some(ver) },
        }]);
    }
}

/// extract_facts/consolidate wire item: {"key": "user.name", "value": "..."}.
#[derive(Deserialize)]
struct FactItem {
    key: String,
    value: String,
}

/// One prior exchange on the consolidate wire.
#[derive(Serialize)]
struct TranscriptPair<'a> {
    user: &'a str,
    darwin: &'a str,
}

/// One stored fact on the consolidate wire — key/value objects, unlike the
/// pre-formatted "key: value" strings the generate/converse ops carry.
#[derive(Serialize)]
struct FactPair<'a> {
    key: &'a str,
    value: &'a str,
}

/// op=consolidate request: its own shape, so it serializes independently of
/// the flat Request struct (whose `facts` field is a &[String]).
#[derive(Serialize)]
struct ConsolidateRequest<'a> {
    id: String,
    op: &'static str,
    transcripts: Vec<TranscriptPair<'a>>,
    facts: Vec<FactPair<'a>>,
}

/// What a consolidate round trip asks the daemon to apply.
#[derive(Debug, Default)]
pub struct ConsolidateOutcome {
    pub upserts: Vec<(String, String)>,
    pub deletes: Vec<String>,
}

/// One op=embed round trip: the vectors PLUS the vector-space metadata the
/// server reports alongside them (see the "op=embed WIRE CONTRACT" comment in
/// inference/server.py). `embedder`/`dim` are `None` on an old server that
/// predates the metadata; a persisting caller (docsearch) keys such a
/// metadata-less batch to its own opaque placeholder — it does NOT assume the
/// batch is any particular backend, since ids are opaque + model-derived.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbedOutcome {
    /// One L2-normalized vector per input text, in input order.
    pub vectors: Vec<Vec<f64>>,
    /// The OPAQUE, model-accurate space-id string the ACTIVE backend reports
    /// (e.g. the Core ML bge id, or a model-derived mean-pool id — a model swap
    /// changes it). Consumers compare it ONLY by equality, never interpreting a
    /// value. `None` on an old server that predates the metadata.
    pub embedder: Option<String>,
    /// The vector dimension the active backend produces; `None` on an old
    /// server, and null on the wire only for an empty batch of the mean-pool
    /// path.
    pub dim: Option<u64>,
    /// Advisory: the Core ML backend was configured but unavailable, so this is
    /// the server's honest mean-pool fallback. `false` when absent (old servers).
    pub fell_back: bool,
}

/// One op=rerank round trip (STAGE TWO of the two-stage retrieval stack): one
/// cross-encoder relevance score per candidate passage (INPUT order) PLUS which
/// reranker produced them and whether the server fell back. `reranker` is `None`
/// (and `fell_back` true) when the cross-encoder was configured but unavailable,
/// OR when the server predates the rerank op — either way the caller keeps its
/// dense order. `scores` is empty on that fallback.
#[derive(Debug, Clone, PartialEq)]
pub struct RerankOutcome {
    /// One cross-encoder relevance score per input passage, in input order.
    pub scores: Vec<f64>,
    /// The OPAQUE reranker model id that produced the scores, or `None` on the
    /// honest fallback (server fell back, or an old server with no rerank op).
    pub reranker: Option<String>,
    /// Advisory: the reranker was configured but unavailable, so the scores are
    /// order-preserving and the caller must keep the dense order. `true` also when
    /// the server predates the op (no `fell_back` field -> defaulted, but the
    /// daemon maps a missing `scores` to the unavailable outcome anyway).
    pub fell_back: bool,
}

#[derive(Deserialize)]
#[allow(dead_code)] // id/latency_ms are part of the wire contract even if unused here
struct Response {
    id: String,
    ok: bool,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    intent: Option<String>,
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    complexity: Option<String>,
    /// classify only: the model's args object, passed through verbatim by
    /// the server ({} on absence/parse trouble; never fabricated). Absent
    /// entirely on old servers — deserializes as None.
    #[serde(default)]
    args: Option<serde_json::Value>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    facts: Option<Vec<FactItem>>,
    #[serde(default)]
    upserts: Option<Vec<FactItem>>,
    #[serde(default)]
    deletes: Option<Vec<String>>,
    /// embed only: one L2-normalized vector per input text, in input order.
    /// Absent on old servers (they reject op=embed before producing this).
    #[serde(default)]
    vectors: Option<Vec<Vec<f64>>>,
    /// embed only: the OPAQUE, model-accurate space-id string the ACTIVE
    /// backend reports (e.g. the Core ML bge id, or a model-derived mean-pool
    /// id — a model swap changes it). This is the VECTOR-SPACE key: vectors from
    /// different backends live in different spaces and must never be
    /// cosine-compared, so docsearch stamps this id on its persisted index and
    /// compares it ONLY by equality, never interpreting a value. Absent on old
    /// servers (they predate the metadata) — deserializes as None.
    #[serde(default)]
    embedder: Option<String>,
    /// embed only: the integer vector dimension the active backend produces. Per
    /// the server's wire contract it is null ONLY on an empty batch of the
    /// mean-pool path (nothing to index); also absent on old servers —
    /// deserializes as None.
    #[serde(default)]
    dim: Option<u64>,
    /// embed only, ADVISORY: true iff the Core ML embedder was CONFIGURED but
    /// unavailable, so this response is the honest mean-pool fallback. Not
    /// needed to key the space (`embedder` already does); it lets the daemon/HUD
    /// surface the degraded state. Absent
    /// on old servers — deserializes as None. SHARED with op=rerank, where it means
    /// the CROSS-ENCODER was configured but unavailable (dense order preserved).
    #[serde(default)]
    fell_back: Option<bool>,
    /// rerank only (STAGE TWO): one cross-encoder relevance score per input
    /// passage, in INPUT order (higher = more relevant; the daemon re-orders its
    /// dense shortlist by these). Absent on old servers (they reject op=rerank as
    /// an unknown op) — deserializes as None, which the daemon reads as "reranker
    /// unavailable" and keeps the dense order.
    #[serde(default)]
    scores: Option<Vec<f64>>,
    /// rerank only (STAGE TWO): the OPAQUE reranker model id that produced
    /// `scores` (e.g. the Core ML cross-encoder id), or "" when the server fell
    /// back (no model scored). Compared only by equality / carried for telemetry,
    /// never interpreted. Absent on old servers — deserializes as None.
    #[serde(default)]
    reranker: Option<String>,
    /// clone_voice / design_voice only: the ElevenLabs voice id minted for the
    /// uploaded sample (clone) or the text DESCRIPTION (design). Absent on old servers
    /// (they reject those ops as unknown) and on every other op — deserializes as None.
    /// Non-secret (the daemon stores it in [voice.voices]); NEVER a key.
    #[serde(default)]
    voice_id: Option<String>,
    /// create_pronunciation only: the NON-secret pronunciation-dictionary id minted
    /// by ElevenLabs (op=create_pronunciation). Absent on old servers (unknown op) and
    /// every other op — deserializes as None. The daemon stores it in
    /// [voice].pronunciation_dictionary_id; NEVER a key.
    #[serde(default)]
    dictionary_id: Option<String>,
    /// create_pronunciation only: the NON-secret version id paired with
    /// `dictionary_id`. Absent on old servers / other ops — deserializes as None. The
    /// daemon stores it in [voice].pronunciation_dictionary_version; NEVER a key.
    #[serde(default)]
    version_id: Option<String>,
    /// describe_image only: the VLM id that produced `text` (AVAILABLE path).
    /// NON-secret (a model repo id). Absent on every other op and on old
    /// servers — deserializes as None.
    #[serde(default)]
    model: Option<String>,
    /// describe_image only (UNAVAILABLE/FAILURE path, ok:false): the stable
    /// machine reason the daemon keys off to FALL BACK honestly rather than
    /// surface a fabricated description. The server sets it to
    /// `DESCRIBE_IMAGE_UNAVAILABLE_REASON` ("vlm_unavailable") when mlx-vlm is
    /// absent, [models].vlm is empty, the checkpoint isn't downloaded/failed to
    /// load, decode failed, or the model produced empty output. Absent on a
    /// caller-bug ValueError (the daemon then shows the validation message) and
    /// on every non-VLM response. NEVER carries pixels or a description.
    #[serde(default)]
    reason: Option<String>,
    /// generate_image only (AVAILABLE path, task #18): the square output
    /// resolution / sampling steps / seed the server actually used. NON-secret
    /// metadata the daemon surfaces in `image.generated` telemetry (never over
    /// the network). Absent on every non-image op — deserialize as None.
    #[serde(default)]
    size: Option<u32>,
    #[serde(default)]
    steps: Option<u32>,
    #[serde(default)]
    seed: Option<u32>,
    /// transcribe only (#31 diarization): the per-word Scribe stream, present ONLY
    /// when the EL-Scribe STT backend diarized (carried speaker labels). Absent on the
    /// on-device whisper path (no diarization model) and on a Scribe response with no
    /// word detail — deserializes as None, and the daemon then renders the honest
    /// single stream (never a fabricated speaker). Carries text + timings + speaker
    /// ids only, NEVER audio.
    #[serde(default)]
    words: Option<Vec<crate::diarize::ScribeWord>>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    latency_ms: i64,
}

/// The stable machine reason the server sends on the describe_image
/// UNAVAILABLE/FAILURE path (`ok:false`). MUST equal the server's
/// `DESCRIBE_IMAGE_UNAVAILABLE_REASON`. The daemon keys off this exact string
/// to FALL BACK honestly (to OCR/classification or an "isn't downloaded" line)
/// rather than surface a fabricated description.
pub const DESCRIBE_IMAGE_UNAVAILABLE_REASON: &str = "vlm_unavailable";

/// Hard cap on the describe_image decode budget (mirrors the server's
/// `DESCRIBE_IMAGE_MAX_TOKENS_CAP`). The daemon clamps any requested budget to
/// this so a caller can never ask the on-device VLM for an unbounded decode.
pub const DESCRIBE_IMAGE_MAX_TOKENS_CAP: u32 = 1024;

/// The default describe_image decode budget when the caller names none.
pub const DESCRIBE_IMAGE_DEFAULT_MAX_TOKENS: u32 = 256;

/// The stable machine reason the server sends on the generate_image
/// UNAVAILABLE/FAILURE path (`ok:false`). MUST equal the server's
/// `GENERATE_IMAGE_UNAVAILABLE_REASON`. The daemon keys off this exact string
/// to surface an honest "the on-device image model isn't set up" line —
/// NEVER a fabricated image and NEVER a silent cloud fallback (image
/// generation is LOCAL only).
pub const GENERATE_IMAGE_UNAVAILABLE_REASON: &str = "image_model_unavailable";

/// generate_image size bounds — MUST mirror the server's
/// `GENERATE_IMAGE_MIN_SIZE`/`GENERATE_IMAGE_MAX_SIZE`. The daemon clamps any
/// requested square resolution into this window before sending, so a caller can
/// never ask the on-device diffusion model for an out-of-range canvas (the
/// server clamps too — defense in depth at both ends).
pub const GENERATE_IMAGE_MIN_SIZE: u32 = 64;
pub const GENERATE_IMAGE_MAX_SIZE: u32 = 1536;
/// The default square resolution when the caller names none (mirrors the
/// server's `GENERATE_IMAGE_DEFAULT_SIZE`).
pub const GENERATE_IMAGE_DEFAULT_SIZE: u32 = 512;

/// generate_image sampling-step bounds — MUST mirror the server's
/// `GENERATE_IMAGE_DEFAULT_STEPS`/`GENERATE_IMAGE_MAX_STEPS_CAP`. The default is
/// the fast schnell-class budget; the cap is a real ceiling the daemon clamps to
/// so a caller can never request an unbounded sampler run on-device.
pub const GENERATE_IMAGE_DEFAULT_STEPS: u32 = 4;
pub const GENERATE_IMAGE_MAX_STEPS_CAP: u32 = 50;

/// The outcome of one generate_image round trip, seen from the daemon. The
/// op is DEVICE/RUNTIME-GATED: it needs an MLX diffusion package + a multi-GB
/// on-device checkpoint + enough RAM, so the UNAVAILABLE arm is a FIRST-CLASS,
/// expected result — NOT an error — and the daemon surfaces it honestly. The
/// prompt is handed ONLY to the on-device model and the generated PIXELS are
/// saved on-device under state/images/; nothing leaves the machine (NO cloud
/// image API anywhere on this path). The image QUALITY/speed are device/runtime-
/// gated and are NEVER claimed measured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenerateOutcome {
    /// The on-device diffusion model rendered the prompt and saved the image.
    /// `path` is the ON-DEVICE absolute path under state/images/ the server
    /// wrote (the daemon surfaces it; the pixels never leave the device).
    /// `model`/`size`/`steps`/`seed` are NON-secret metadata for the HUD readout
    /// + the `image.generated` telemetry.
    Available {
        path: PathBuf,
        model: String,
        size: u32,
        steps: u32,
        seed: u32,
    },
    /// The image model was not available (the diffusion package absent, [image]
    /// off / no model named server-side, the checkpoint not downloaded / failed
    /// to load, or a runtime failure) — the server sent `ok:false` with reason
    /// [`GENERATE_IMAGE_UNAVAILABLE_REASON`]. The daemon surfaces an honest "the
    /// on-device image model isn't set up" line; it NEVER fabricates an image and
    /// NEVER falls back to a cloud image API. `error` is an honest human message.
    Unavailable { error: String },
}

/// The outcome of one describe_image round trip, seen from the daemon. The VLM
/// op is DEVICE/RUNTIME-GATED: it needs mlx-vlm + a multi-GB on-device VLM
/// checkpoint + enough RAM, so the UNAVAILABLE arm is a FIRST-CLASS, expected
/// result — NOT an error — and the daemon falls back honestly on it. The image
/// is read ON-DEVICE by the server; pixels NEVER leave the machine. The actual
/// description QUALITY is device/runtime-gated and is never claimed measured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DescribeOutcome {
    /// The on-device VLM produced a description/answer. `model` is the VLM id
    /// (non-secret). `text` is the description — distinct from OCR text glyphs:
    /// it is the model's visual understanding of the scene.
    Available { text: String, model: String },
    /// The VLM was not available (mlx-vlm absent, [models].vlm empty, the
    /// checkpoint isn't downloaded / failed to load, decode failed, or empty
    /// output) — the server sent `ok:false` with reason
    /// [`DESCRIBE_IMAGE_UNAVAILABLE_REASON`]. The daemon falls back HONESTLY
    /// (OCR/classification or "the model isn't downloaded"); it NEVER fabricates
    /// a description. `error` is an honest human message for the spoken reply.
    Unavailable { error: String },
}

/// One spoken sentence streamed out of a converse request: its text plus the
/// synthesized WAV, ready for the playback sink while the model is still
/// generating the rest of the reply.
#[derive(Debug)]
pub struct SentenceEvent {
    pub seq: u64,
    pub text: String,
    pub path: PathBuf,
}

/// Terminal summary of a successful converse stream.
#[derive(Debug)]
pub struct ConverseDone {
    /// Full reply text (may extend past the sentences that were synthesized).
    pub text: String,
}

/// One line of a converse stream: a sentence event or the done event. The
/// server also sends sentences/first_sentence_ms/latency_ms on done; serde
/// ignores what the daemon does not consume.
#[derive(Deserialize)]
struct ConverseLine {
    id: String,
    #[serde(default)]
    event: Option<String>,
    #[serde(default)]
    seq: Option<u64>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    ok: Option<bool>,
    #[serde(default)]
    error: Option<String>,
}

/// How one converse round trip ended, seen from the transport layer.
enum ConverseOutcome {
    Done(ConverseDone),
    /// The server reported failure in a well-formed done line; the
    /// connection itself is still healthy.
    ServerFail(String),
}

/// PURE: the un-jittered backoff delay BEFORE connect attempt `attempt`
/// (0-based). Attempt 0 is immediate (no sleep) — the happy path connects on
/// the first try with zero added latency, so a healthy server is byte-for-byte
/// as fast as before. From attempt 1 the delay doubles
/// ([`RECONNECT_BASE_DELAY`] * 2^(attempt-1)) and saturates at
/// [`RECONNECT_MAX_DELAY`]. Saturating arithmetic so a large `attempt` can
/// never overflow into a tiny/zero delay. Unit-tested for the exact schedule.
fn backoff_delay(attempt: u32) -> Duration {
    if attempt == 0 {
        return Duration::ZERO;
    }
    // 2^(attempt-1), saturating — never panics, never wraps to a small value.
    let factor = 1u32.checked_shl(attempt - 1).unwrap_or(u32::MAX);
    let scaled = RECONNECT_BASE_DELAY
        .checked_mul(factor)
        .unwrap_or(RECONNECT_MAX_DELAY);
    if scaled > RECONNECT_MAX_DELAY {
        RECONNECT_MAX_DELAY
    } else {
        scaled
    }
}

/// PURE: add bounded +/- jitter (up to ~25% of the base) to a backoff delay so
/// several independent [`InferenceClient`]s (the mic loop + reflect +
/// anticipation + standing tasks each own one) do NOT reconnect in lockstep and
/// thundering-herd a recovering server. `seed` is a cheap rotating nonce (the
/// client's request counter) — no RNG dependency, fully deterministic for the
/// test. Zero base in -> zero out (the immediate first attempt never sleeps).
fn jittered_delay(base: Duration, seed: u64) -> Duration {
    if base.is_zero() {
        return Duration::ZERO;
    }
    let base_ms = base.as_millis() as u64;
    // Jitter span = 25% of base; map the seed into [-span, +span].
    let span = (base_ms / 4).max(1);
    // seed % (2*span+1) - span  =>  symmetric jitter without negative overflow.
    let offset = (seed % (2 * span + 1)) as i64 - span as i64;
    let jittered = (base_ms as i64 + offset).max(0) as u64;
    Duration::from_millis(jittered)
}

// ---------------------------------------------------------------------------
// SHARED INFERENCE HEALTH — a process-global snapshot of inference-server
// reachability, published by the background liveness task (liveness_task) and
// read by the daemon's degraded-mode logic + the HUD. Today a down server is
// only discovered when a user turn is LOST; this lets the system know it is
// degraded BEFORE a turn fails. Multiple InferenceClients exist (mic loop +
// reflect + anticipation + standing); ONE liveness probe feeds this one shared
// state so they don't each have to discover the outage independently.
// ---------------------------------------------------------------------------

/// Default cadence of the background liveness probe. Frequent enough that the
/// HUD reflects a server coming up/going down within a couple seconds, cheap
/// enough that a connect+close every few seconds is negligible.
pub const LIVENESS_INTERVAL: Duration = Duration::from_secs(5);

/// Point-in-time reachability of the inference server, as last observed by the
/// background liveness probe (NOT by user turns — this is the proactive view).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InferenceHealth {
    /// True iff the most recent probe connected.
    pub reachable: bool,
    /// Consecutive failed probes (0 while reachable). Lets the HUD distinguish
    /// a one-off blip from a sustained outage without keeping history.
    pub consecutive_failures: u32,
    /// Unix seconds of the last SUCCESSFUL probe; None if never reachable since
    /// boot (honest: we have not yet seen the server up, not "0 == 1970").
    pub last_ok_unix: Option<i64>,
    /// True until the FIRST probe completes — so a reader never reports
    /// "reachable:false" before we have actually looked (honest unknown state).
    pub probed: bool,
}

impl InferenceHealth {
    const fn initial() -> Self {
        Self {
            reachable: false,
            consecutive_failures: 0,
            last_ok_unix: None,
            probed: false,
        }
    }
}

static HEALTH: RwLock<InferenceHealth> = RwLock::new(InferenceHealth::initial());

/// Test-only: reset the shared health state so the `record_probe` transition
/// test is independent of any other test that touched the global.
#[cfg(test)]
fn reset_health_for_test() {
    if let Ok(mut g) = HEALTH.write() {
        *g = InferenceHealth::initial();
    }
}

/// The current shared inference health snapshot (the proactive liveness view).
/// `probed == false` means no probe has completed yet — callers must treat that
/// as UNKNOWN, never as "down", to stay honest at boot.
pub fn health_snapshot() -> InferenceHealth {
    HEALTH.read().map(|g| *g).unwrap_or(InferenceHealth::initial())
}

/// Fold one probe result into the shared health state. Returns the PRIOR
/// `reachable` so the caller can detect an edge (up<->down) and only emit/log
/// on transitions instead of every tick. `now_unix` is injected so the state
/// transition is unit-testable without a clock.
fn record_probe(ok: bool, now_unix: i64) -> bool {
    let mut guard = match HEALTH.write() {
        Ok(g) => g,
        Err(_) => return false,
    };
    let prev = guard.reachable;
    guard.probed = true;
    if ok {
        guard.reachable = true;
        guard.consecutive_failures = 0;
        guard.last_ok_unix = Some(now_unix);
    } else {
        guard.reachable = false;
        guard.consecutive_failures = guard.consecutive_failures.saturating_add(1);
    }
    prev
}

/// Background liveness loop: every [`LIVENESS_INTERVAL`] it connect-probes the
/// inference socket (NO model call — `probe_reachable` connects + closes) and
/// folds the result into the shared [`InferenceHealth`]. On every probe it
/// publishes an `inference.health` telemetry frame (so the HUD can render a
/// degraded badge); on an UP<->DOWN transition it logs + emits a coherent
/// `inference.degraded` / `inference.recovered` frame ONCE, instead of the
/// per-turn `inference.unavailable` spam the daemon emits today. Never blocks
/// the pipeline and never panics it (a poisoned lock just skips the tick).
/// This is the proactive half of degraded-mode honesty; the per-turn abort
/// path is unchanged.
pub async fn liveness_task(socket_path: PathBuf, interval: Duration) {
    let probe = InferenceClient::new(socket_path);
    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;
        let ok = probe.probe_reachable().await.is_ok();
        let now = chrono::Utc::now().timestamp();
        let was_reachable = record_probe(ok, now);
        let snap = health_snapshot();
        crate::telemetry::emit(
            "system",
            "inference.health",
            serde_json::json!({
                "reachable": snap.reachable,
                "consecutive_failures": snap.consecutive_failures,
                "last_ok_unix": snap.last_ok_unix,
            }),
        );
        // Edge-trigger the coherent degraded/recovered signal exactly once.
        if was_reachable && !ok {
            warn!(
                consecutive_failures = snap.consecutive_failures,
                "inference server became UNREACHABLE — running degraded (local turns will abort honestly until it returns)"
            );
            crate::telemetry::emit(
                "system",
                "inference.degraded",
                serde_json::json!({"reason": "liveness_probe_failed"}),
            );
        } else if !was_reachable && ok {
            info!("inference server is reachable again — degraded mode cleared");
            crate::telemetry::emit(
                "system",
                "inference.recovered",
                serde_json::json!({"last_ok_unix": snap.last_ok_unix}),
            );
        }
    }
}

/// Lazy JSONL client for the Python inference server. The daemon must keep
/// running when the server is down, so every failure surfaces as Err and the
/// connection is dropped for a fresh attempt next time.
pub struct InferenceClient {
    socket_path: PathBuf,
    conn: Option<(BufReader<OwnedReadHalf>, OwnedWriteHalf)>,
    next_id: u64,
}

impl InferenceClient {
    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            conn: None,
            next_id: 0,
        }
    }

    /// The inference socket this client connects to. Lets a caller that holds only
    /// a borrowed client (e.g. the router) spawn a Send-safe per-call client on the
    /// SAME socket for a background generation — see `compose_music_for_command`.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// On-device / cloud STT. The server transcribes the WAV at `wav` and returns
    /// the text. `backend` is the STT backend the daemon already resolved
    /// ([`voice_tier::resolve_stt_backend`]): on-device whisper (the default +
    /// fallback) or the opt-in ElevenLabs Scribe cloud-STT tier. `el_key` is the
    /// resolved ElevenLabs API key — it MUST be `Some` only when `backend` is
    /// `ElevenLabsScribe` (the gated cloud path) and rides ONLY the request body for
    /// the server's `xi-api-key` header; never logged/argv/telemetry.
    ///
    /// HONESTY: with the Whisper backend the wire is byte-for-byte today's
    /// transcribe request (just {op, path}), so an old server is unaffected and the
    /// user's audio stays on-device. With Scribe the audio LEAVES the device — more
    /// sensitive than the TTS text leg. On ANY Scribe error the SERVER falls back to
    /// mlx_whisper, so this never fails a turn merely because the cloud leg failed.
    #[allow(dead_code)] // text-only convenience seam; the live pipeline uses transcribe_diarized
    pub async fn transcribe(
        &mut self,
        wav: &Path,
        backend: &crate::voice_tier::SttBackend,
        el_key: Option<&str>,
    ) -> Result<String> {
        // Same single request as the diarized form; the plain-text callers simply drop
        // the per-word Scribe stream. Keeping ONE implementation means the wire shape +
        // honesty rails never drift between the two entry points.
        self.transcribe_diarized(wav, backend, el_key)
            .await
            .map(|(text, _words)| text)
    }

    /// #31 DIARIZED transcribe: the SAME single transcribe request, but ALSO returning
    /// the Scribe per-word stream (`words`) when the EL-Scribe backend diarized. Used
    /// only on the gated [voice].diarize path so the daemon can feed `diarize::diarize`
    /// the REAL speaker labels. Returns `(text, words)` where `words` is empty on the
    /// on-device whisper path (no diarization model) and on a Scribe response with no
    /// word detail — the caller then falls back to the honest single stream, never a
    /// fabricated speaker. No extra audio leaves the device beyond the one request the
    /// plain `transcribe` would have made.
    pub async fn transcribe_diarized(
        &mut self,
        wav: &Path,
        backend: &crate::voice_tier::SttBackend,
        el_key: Option<&str>,
    ) -> Result<(String, Vec<crate::diarize::ScribeWord>)> {
        use crate::voice_tier::SttBackend;
        let mut req = Request::new(self.fresh_id(), "transcribe");
        req.path = Some(wav.display().to_string());
        match backend {
            SttBackend::Whisper => {}
            SttBackend::ElevenLabsScribe { model } => {
                req.backend = Some("elevenlabs_scribe");
                req.model = Some(model);
                req.el_key = el_key;
            }
        }
        let resp = self.request(&req).await?;
        let text = resp
            .text
            .ok_or_else(|| anyhow!("transcribe response missing text"))?;
        Ok((text, resp.words.unwrap_or_default()))
    }

    /// VOICE CLONING (consent-gated): hand the server an OWNER audio sample (a path
    /// the daemon confined and the user authorized) plus a display `name` and the
    /// resolved ElevenLabs key; the server uploads it to ElevenLabs
    /// (POST /v1/voices/add, multipart: name + audio sample) and returns the new
    /// `voice_id`. The daemon then stores that id in `[voice.voices]` so it is usable
    /// like any EL voice.
    ///
    /// HONESTY: this is the ONE path where the audio SAMPLE leaves the device — to
    /// clone a voice you must first AUTHORIZE the sample (consent-gated upstream; this
    /// op is only ever reached after an explicit confirm). On any error (no key /
    /// network / quota) the server returns a clean failure and the user keeps Kokoro
    /// / their existing voice — nothing is silently changed.
    ///
    /// SECURITY: `el_key` rides ONLY the request body for the server's `xi-api-key`
    /// header — never logged/argv/telemetry. The returned `voice_id` is non-secret.
    pub async fn clone_voice(
        &mut self,
        sample_path: &Path,
        name: &str,
        el_key: &str,
    ) -> Result<String> {
        let mut req = Request::new(self.fresh_id(), "clone_voice");
        req.path = Some(sample_path.display().to_string());
        req.text = Some(name); // the voice's display name on ElevenLabs
        req.el_key = Some(el_key);
        let resp = self.request(&req).await?;
        resp.voice_id
            .ok_or_else(|| anyhow!("clone_voice response missing voice_id"))
    }

    pub async fn classify(&mut self, text: &str) -> Result<Classification> {
        let mut req = Request::new(self.fresh_id(), "classify");
        req.text = Some(text);
        let resp = self.request(&req).await?;
        Ok(Classification {
            intent: resp
                .intent
                .ok_or_else(|| anyhow!("classify response missing intent"))?,
            confidence: resp
                .confidence
                .ok_or_else(|| anyhow!("classify response missing confidence"))?,
            complexity: resp
                .complexity
                .ok_or_else(|| anyhow!("classify response missing complexity"))?,
            // Old servers send no args field at all; Null and {} are both
            // "no args" to the router.
            args: resp.args.unwrap_or_default(),
        })
    }

    /// Context-aware generation. `history` is (user, darwin) exchange pairs
    /// oldest first — each pair becomes two alternating wire turns. `facts`
    /// are pre-formatted "key: value" strings; `data` is verified handler
    /// output the model must convey without inventing numbers. All three are
    /// omitted from the wire when empty so the server's persona-prefix KV
    /// cache sees a stable request shape.
    ///
    /// `local_model` is the multi-resident LOCAL sub-choice (task #17): the warm
    /// local model id the Local tier picked (a "local-fast" model vs the capable
    /// base). `None`/empty -> the base single-resident model. It is carried on the
    /// wire only when present, so the default single-resident path is unchanged and
    /// an old server simply ignores it (answering on the base).
    #[allow(clippy::too_many_arguments)] // mirrors the wire request shape
    pub async fn generate(
        &mut self,
        text: &str,
        max_tokens: u32,
        history: &[(String, String)],
        facts: &[String],
        data: Option<&str>,
        local_model: Option<&str>,
    ) -> Result<String> {
        let turns: Vec<HistoryTurn> = history
            .iter()
            .flat_map(|(user, darwin)| {
                [
                    HistoryTurn { speaker: "user", text: user },
                    HistoryTurn { speaker: "darwin", text: darwin },
                ]
            })
            .collect();
        let mut req = Request::new(self.fresh_id(), "generate");
        req.text = Some(text);
        req.max_tokens = Some(max_tokens);
        req.history = if turns.is_empty() { None } else { Some(turns) };
        req.facts = if facts.is_empty() { None } else { Some(facts) };
        req.data = data.filter(|d| !d.is_empty());
        req.local_model = local_model.filter(|m| !m.trim().is_empty());
        let resp = self.request(&req).await?;
        resp.text
            .ok_or_else(|| anyhow!("generate response missing text"))
    }

    /// Ask the server's LLM for at most 3 durable, namespaced facts about the
    /// user from one exchange. An empty list is the common, correct result.
    pub async fn extract_facts(
        &mut self,
        text: &str,
        response: Option<&str>,
    ) -> Result<Vec<(String, String)>> {
        let mut req = Request::new(self.fresh_id(), "extract_facts");
        req.text = Some(text);
        req.response = response.filter(|r| !r.is_empty());
        let resp = self.request(&req).await?;
        Ok(resp
            .facts
            .unwrap_or_default()
            .into_iter()
            .map(|f| (f.key, f.value))
            .collect())
    }

    /// Memory consolidation: hand the server the recent transcripts plus the
    /// stored facts; it returns merge upserts and contradiction deletes for
    /// the daemon to apply. Conservative by contract — empty arrays are the
    /// common, correct result.
    pub async fn consolidate(
        &mut self,
        transcripts: &[(String, String)],
        facts: &[(String, String)],
    ) -> Result<ConsolidateOutcome> {
        let req = ConsolidateRequest {
            id: self.fresh_id(),
            op: "consolidate",
            transcripts: transcripts
                .iter()
                .take(40) // wire contract: at most 40 exchanges
                .map(|(user, darwin)| TranscriptPair { user, darwin })
                .collect(),
            facts: facts
                .iter()
                .map(|(key, value)| FactPair { key, value })
                .collect(),
        };
        let resp = self
            .request_generic(&req, "consolidate", CONSOLIDATE_TIMEOUT)
            .await?;
        Ok(ConsolidateOutcome {
            upserts: resp
                .upserts
                .unwrap_or_default()
                .into_iter()
                .map(|f| (f.key, f.value))
                .collect(),
            deletes: resp.deletes.unwrap_or_default(),
        })
    }

    /// Neural TTS: the server synthesizes `text` and returns the path of a WAV
    /// under state/tmp/ for the daemon to play and delete.
    ///
    /// `backend` is the TTS backend the daemon already resolved for this sentence
    /// ([`voice_tier::resolve_voice_backend`]): on-device Kokoro (the default +
    /// fallback) or the opt-in ElevenLabs cloud voice tier. `el_key` is the
    /// resolved ElevenLabs API key — it MUST be `Some` only when `backend` is
    /// `ElevenLabs` (the gated cloud path) and is passed ONLY in the request body
    /// for the server's `xi-api-key` header; it is never logged/argv/telemetry. On
    /// ANY ElevenLabs error the SERVER falls back to Kokoro, so this never fails a
    /// turn merely because the cloud leg failed.
    ///
    /// `lang` is the TARGET LANGUAGE for this sentence (Babel, build 2/2): when
    /// present and non-English, the ElevenLabs backend selects a MULTILINGUAL model
    /// (eleven_multilingual_v2 / eleven_v3) instead of the English-centric default.
    /// `None` (an ordinary English reply) leaves the model selection unchanged. It is
    /// carried on the wire only when present, so it never affects the Kokoro path or
    /// an old server.
    ///
    /// `shape` is the EXPRESSIVENESS shaping (#33 prosody + #34 whisper) the daemon
    /// resolved for this reply ([`crate::prosody::SpeakShape`]). The rich EL-v3 surface
    /// (`audio_tag`/`stability`/`style`) rides the wire ONLY on the EL-v3 path; the
    /// coarse `rate`/`volume` ride on every backend — but each field is carried ONLY
    /// when it differs from the neutral default, so a NEUTRAL shape (both features off,
    /// the shipped default) is BYTE-FOR-BYTE today's request and an old server simply
    /// ignores the added fields.
    /// `extras` are the ADDITIVE Phase-2 speak fields ([`SpeakExtras`]): the opt-in
    /// streaming TTS flag (`[voice].stream_tts`, ships OFF) and the active pronunciation
    /// locator (`[voice].pronunciation_dictionary_id`/`_version`, default empty). With
    /// the shipped defaults this is [`SpeakExtras::none`] and the request is
    /// BYTE-FOR-BYTE today's; an old server simply ignores the fields when present.
    pub async fn speak(
        &mut self,
        text: &str,
        backend: &crate::voice_tier::Backend,
        el_key: Option<&str>,
        lang: Option<&str>,
        shape: &crate::prosody::SpeakShape,
        extras: &SpeakExtras,
    ) -> Result<PathBuf> {
        use crate::voice_tier::Backend;
        let mut req = Request::new(self.fresh_id(), "speak");
        req.text = Some(text);
        // Babel target language threads on BOTH backends: the EL backend uses it to
        // pick a multilingual model; the server may also pass it to Kokoro. Omitted
        // when absent so the default English wire is unchanged.
        req.lang = lang.filter(|l| !l.trim().is_empty());
        match backend {
            Backend::Kokoro { voice } => {
                // The exact pre-tier wire: just the Kokoro voice. `backend` is left
                // absent so an old server sees the identical request shape.
                req.voice = Some(voice);
            }
            Backend::ElevenLabs { voice_id, model } => {
                req.backend = Some("elevenlabs");
                req.voice_id = Some(voice_id);
                req.model = Some(model);
                // The key rides ONLY the request body (server -> xi-api-key header).
                req.el_key = el_key;
            }
        }
        // EXPRESSIVENESS (#33/#34): thread the shaped fields onto the request. A
        // neutral shape (the OFF default) sets NOTHING extra -> byte-for-byte today's.
        apply_shape_to_request(&mut req, shape);
        // ADDITIVE (Phase-2): thread streaming opt-in + pronunciation locator. Default
        // SpeakExtras sets NOTHING -> the speak wire is unchanged.
        apply_extras_to_request(&mut req, extras);
        let resp = self.request(&req).await?;
        resp.path
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("speak response missing path"))
    }

    /// SOUND-EFFECT CUE (Phase-2): emit op=sound_effect with the text `prompt` + the
    /// resolved ElevenLabs key; the server generates a short SFX WAV (EL sound-generation)
    /// and returns its path under state/tmp/ for the daemon to play. `duration_s` and
    /// `prompt_influence` are OPTIONAL shaping hints (the server clamps them).
    ///
    /// HONESTY: there is NO on-device SFX generator — this is reached ONLY through the
    /// `[voice].cloud_sfx` + key gate (see [`crate::voice_tier::sfx_enabled`]). On any
    /// failure (no key / network / quota) the server returns ok:false and this returns
    /// Err, so the caller surfaces an honest "unavailable" — never a fabricated cue. The
    /// SFX text PROMPT leaves the device (text only — no on-device audio is uploaded).
    ///
    /// SECURITY: `el_key` rides ONLY the request body for the server's `xi-api-key`
    /// header — never logged/argv/telemetry. Mirrors `clone_voice`.
    #[allow(dead_code)] // credential+runtime-gated seam; reached via trigger_sound_effect
    pub async fn sound_effect(
        &mut self,
        prompt: &str,
        el_key: &str,
        duration_s: Option<f32>,
        prompt_influence: Option<f32>,
    ) -> Result<PathBuf> {
        let mut req = Request::new(self.fresh_id(), "sound_effect");
        req.text = Some(prompt);
        req.el_key = Some(el_key);
        req.duration_s = duration_s;
        req.prompt_influence = prompt_influence;
        let resp = self.request(&req).await?;
        resp.path
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("sound_effect response missing path"))
    }

    /// COMPOSE MUSIC (Phase-2): emit op=compose_music with the text `prompt` + the
    /// resolved ElevenLabs key; the server generates a FULL-LENGTH music track WAV
    /// (EL music-generation) and returns its path under state/tmp/ for the daemon to
    /// play. `length_ms` is an OPTIONAL length hint in MILLISECONDS (the server clamps
    /// to its 3000..600000 window and DEFAULTS to 30000 when absent), threaded onto the
    /// wire ONLY when the caller pins one.
    ///
    /// HONESTY: there is NO on-device music generator — this is reached ONLY through the
    /// `[voice].cloud_music` + key gate (see [`crate::voice_tier::music_enabled`]). On any
    /// failure (no key / network / quota) the server returns ok:false and this returns
    /// Err, so the caller surfaces an honest "unavailable" — never a fabricated track. The
    /// music text PROMPT leaves the device (text only — no on-device audio is uploaded).
    ///
    /// SECURITY: `el_key` rides ONLY the request body for the server's `xi-api-key`
    /// header — never logged/argv/telemetry. Mirrors `sound_effect`.
    #[allow(dead_code)] // credential+runtime-gated seam; reached via trigger_compose_music
    pub async fn compose_music(
        &mut self,
        prompt: &str,
        el_key: &str,
        length_ms: Option<u32>,
    ) -> Result<PathBuf> {
        let mut req = Request::new(self.fresh_id(), "compose_music");
        req.text = Some(prompt);
        req.el_key = Some(el_key);
        // Thread length_ms ONLY when the caller pins one; absent => the server's default.
        req.length_ms = length_ms;
        let resp = self.request(&req).await?;
        resp.path
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("compose_music response missing path"))
    }

    /// DESIGN VOICE (Phase-2): mint an ElevenLabs voice from a text DESCRIPTION (no
    /// audio sample). Emits op=design_voice with `description` (the EL prompt, 20-1000
    /// chars), `name` (the display name), and the resolved key; returns the new
    /// `voice_id` the daemon stores in `[voice.voices]` so the named agent can use it
    /// like any EL voice.
    ///
    /// HONESTY: unlike `clone_voice`, NO audio leaves the device — the voice is minted
    /// purely from the text description, so there is NO consent/audio gate (still
    /// key-gated). There is no on-device voice designer, so on any failure (or no key)
    /// the server returns ok:false and this returns Err (honest 'unavailable', never a
    /// fabricated voice_id). Only the text description leaves the device.
    ///
    /// SECURITY: `el_key` rides ONLY the request body for the server's `xi-api-key`
    /// header — never logged/argv/telemetry. The returned `voice_id` is non-secret.
    pub async fn design_voice(
        &mut self,
        description: &str,
        name: &str,
        el_key: &str,
    ) -> Result<String> {
        let mut req = Request::new(self.fresh_id(), "design_voice");
        req.text = Some(description); // the voice DESCRIPTION (server reads req.text)
        req.voice = Some(name); // the display name (server reads req.voice || req.name)
        req.el_key = Some(el_key);
        let resp = self.request(&req).await?;
        resp.voice_id
            .ok_or_else(|| anyhow!("design_voice response missing voice_id"))
    }

    /// CREATE PRONUNCIATION DICTIONARY (Phase-2): mint an ElevenLabs pronunciation
    /// dictionary from text `rules`. Emits op=create_pronunciation with `name`, the
    /// non-empty `rules` list, and the resolved key; returns the NON-secret
    /// (dictionary_id, version_id) pair the daemon stores in
    /// `[voice].pronunciation_dictionary_id`/`_version` to later thread into speak as a
    /// pronunciation locator.
    ///
    /// HONESTY: NO audio leaves the device — the dictionary is minted purely from the
    /// text rules, so there is NO consent/audio gate (still key-gated). There is no
    /// on-device equivalent, so on any failure (or no key) the server returns ok:false
    /// and this returns Err (honest 'unavailable', never a fabricated id).
    ///
    /// SECURITY: `el_key` rides ONLY the request body for the server's `xi-api-key`
    /// header — never logged/argv/telemetry. Both returned ids are non-secret.
    pub async fn create_pronunciation(
        &mut self,
        name: &str,
        rules: &[PronunciationRule],
        el_key: &str,
    ) -> Result<(String, String)> {
        let mut req = Request::new(self.fresh_id(), "create_pronunciation");
        req.name = Some(name);
        req.rules = Some(rules);
        req.el_key = Some(el_key);
        let resp = self.request(&req).await?;
        let dictionary_id = resp
            .dictionary_id
            .ok_or_else(|| anyhow!("create_pronunciation response missing dictionary_id"))?;
        let version_id = resp
            .version_id
            .ok_or_else(|| anyhow!("create_pronunciation response missing version_id"))?;
        Ok((dictionary_id, version_id))
    }

    /// On-device retrieval embeddings WITH the vector-space metadata: hand the
    /// server a batch of strings and get back one L2-normalized vector per
    /// input, in the SAME ORDER, plus the OPAQUE space-id of WHICH backend
    /// produced them. The backend is the server's `[inference].embedder`
    /// selection — the Core ML bge sentence embedder by default, or a mean-pool
    /// path by choice or on the server's honest fallback — and the response
    /// names whichever ACTUALLY ran with its opaque, model-accurate id, so a
    /// caller that PERSISTS vectors (docsearch) can stamp its store's vector
    /// space and refuse a meaningless cross-space cosine. When the inference
    /// server is down OR predates the
    /// embed op, this returns Err (unknown op / socket unavailable) and the
    /// caller falls back to lexical BM25. NOT exercised by any test (the call
    /// is runtime/MLX-gated); the ranking LOGIC is unit-tested with injected
    /// vectors, and the wire shape is locked by the tests below.
    pub async fn embed_with_meta(&mut self, texts: &[String]) -> Result<EmbedOutcome> {
        let mut req = Request::new(self.fresh_id(), "embed");
        req.texts = Some(texts);
        let resp = self.request(&req).await?;
        let vectors = resp
            .vectors
            .ok_or_else(|| anyhow!("embed response missing vectors"))?;
        if vectors.len() != texts.len() {
            bail!(
                "embed returned {} vectors for {} inputs",
                vectors.len(),
                texts.len()
            );
        }
        Ok(EmbedOutcome {
            vectors,
            embedder: resp.embedder,
            dim: resp.dim,
            fell_back: resp.fell_back.unwrap_or(false),
        })
    }

    /// Vectors-only convenience over [`Self::embed_with_meta`] for callers that
    /// NEVER persist vectors: MNEMOSYNE's recall paths embed the query and the
    /// candidate facts TOGETHER in one call, so every comparison is same-space
    /// by construction (one op=embed response = one backend for the whole
    /// batch) and the space metadata is irrelevant to them. A caller that
    /// PERSISTS vectors (docsearch) must use [`Self::embed_with_meta`] instead
    /// so it can key the store's vector space.
    pub async fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f64>>> {
        Ok(self.embed_with_meta(texts).await?.vectors)
    }

    /// STAGE TWO of the two-stage retrieval stack: hand the server a `query` and
    /// its dense top-K candidate `passages`, and get back one cross-encoder
    /// relevance score per passage, in the SAME ORDER, plus the OPAQUE id of the
    /// reranker that produced them. The daemon re-orders its dense shortlist by the
    /// scores. The backend is the server's `[inference].reranker` (a Core ML
    /// cross-encoder); when that reranker is disabled or unbuildable the server
    /// answers `fell_back=true` with order-preserving scores, and when the server
    /// is down OR predates the rerank op this returns Err (unknown op / socket) —
    /// either way [`Self::rerank`] surfaces the HONEST-FALLBACK outcome so the
    /// caller KEEPS the dense order and never mislabels the ranking. Empty
    /// `passages` -> an empty (non-fallback) result. NOT exercised by any test (the
    /// call is runtime/MLX-gated); the wire shape is locked by the tests below.
    pub async fn rerank(&mut self, query: &str, passages: &[String]) -> Result<RerankOutcome> {
        if passages.is_empty() {
            return Ok(RerankOutcome {
                scores: Vec::new(),
                reranker: None,
                fell_back: false,
            });
        }
        let mut req = Request::new(self.fresh_id(), "rerank");
        req.query = Some(query);
        req.passages = Some(passages);
        let resp = self.request(&req).await?;
        // A server that fell back (or somehow omitted scores) is the honest
        // dense-order outcome: report fell_back so the caller keeps its order.
        let scores = match resp.scores {
            Some(s) if !resp.fell_back.unwrap_or(false) && s.len() == passages.len() => s,
            // Fell back / omitted scores / mismatched count: honest dense-order
            // outcome — the caller keeps its order.
            _ => {
                return Ok(RerankOutcome {
                    scores: Vec::new(),
                    reranker: None,
                    fell_back: true,
                });
            }
        };
        Ok(RerankOutcome {
            scores,
            reranker: resp.reranker.filter(|s| !s.is_empty()),
            fell_back: false,
        })
    }

    /// ON-DEVICE VISUAL DESCRIPTION (VLM). Hand the server a LOCAL image `path`
    /// (the DAEMON path-confines it via canonicalize + allowed-root BEFORE this
    /// call) and an OPTIONAL `question` (absent => a general scene description
    /// from the server's DESCRIBE_IMAGE_DEFAULT_PROMPT). The server reads the
    /// image ON-DEVICE and runs an mlx-vlm (Qwen2-VL-class) model — pixels NEVER
    /// leave the machine, nothing goes to the cloud.
    ///
    /// Returns a [`DescribeOutcome`], NOT a bare String, because the VLM op is
    /// DEVICE/RUNTIME-GATED: when mlx-vlm is absent, [models].vlm is empty, the
    /// checkpoint isn't downloaded / failed to load, decode failed, or the model
    /// produced empty output, the server replies `ok:false` with
    /// reason=[`DESCRIBE_IMAGE_UNAVAILABLE_REASON`] — an EXPECTED outcome the
    /// daemon reads as "unavailable" and FALLS BACK honestly (OCR/classification
    /// or an honest "the model isn't downloaded"), NEVER a fabricated
    /// description. A transport/socket failure (server down) is the only `Err`
    /// here; an `ok:false` WITHOUT the unavailable reason (a caller-bug
    /// ValueError: missing/empty path, non-string question, non-positive
    /// max_tokens, nonexistent image) surfaces its honest message as
    /// `Unavailable { error }` too (the daemon shows it; it is never a
    /// description). NOT exercised against a real model by any test — the op
    /// dispatch + the unavailable path are proven with a stub; the description
    /// QUALITY is device-gated and never claimed measured.
    pub async fn describe_image(
        &mut self,
        path: &Path,
        question: Option<&str>,
        max_tokens: Option<u32>,
    ) -> Result<DescribeOutcome> {
        let mut req = Request::new(self.fresh_id(), "describe_image");
        req.path = Some(path.display().to_string());
        req.question = question.filter(|q| !q.trim().is_empty());
        // Clamp the decode budget to the shared cap so the daemon can never ask
        // the on-device VLM for an unbounded decode; absent => the server's
        // default (we still send the daemon default for an explicit contract).
        req.max_tokens = Some(
            max_tokens
                .unwrap_or(DESCRIBE_IMAGE_DEFAULT_MAX_TOKENS)
                .min(DESCRIBE_IMAGE_MAX_TOKENS_CAP),
        );
        // describe_image needs the raw Response (ok true AND false) so the
        // structured unavailable reason survives — request_generic collapses
        // ok:false into an opaque Err, which would lose the fall-back signal.
        let resp = self.request_raw(&req, "describe_image", REQUEST_TIMEOUT).await?;
        if resp.ok {
            let text = resp
                .text
                .filter(|t| !t.trim().is_empty())
                .ok_or_else(|| anyhow!("describe_image ok response missing text"))?;
            // The model id is non-secret; default to a neutral label if an older
            // server omits it on the ok path.
            let model = resp.model.unwrap_or_else(|| "on-device VLM".to_string());
            return Ok(DescribeOutcome::Available { text, model });
        }
        // ok:false — UNAVAILABLE or a caller-bug ValueError. Either way the
        // daemon must NOT show a description: surface the honest server message
        // (or a neutral default) so the caller falls back. The `reason` field
        // distinguishes the DEVICE-GATED case (reason == the contract's
        // DESCRIBE_IMAGE_UNAVAILABLE_REASON, "vlm_unavailable": mlx-vlm/model
        // absent) from a caller-bug ValueError (no reason). We log it for the
        // device-gated honest copy; the outcome is "no description, here's why"
        // either way (never a fabricated description).
        let device_gated = resp.reason.as_deref() == Some(DESCRIBE_IMAGE_UNAVAILABLE_REASON);
        let error = resp.error.unwrap_or_else(|| {
            if device_gated {
                "the on-device vision-language model is not available".to_string()
            } else {
                "the image could not be described".to_string()
            }
        });
        if !device_gated {
            // A caller-bug ValueError (missing/empty path, bad question, etc.) —
            // surfaced honestly but NOT the device-gated reason. warn so a wiring
            // bug is visible without being read as a "model not downloaded".
            warn!(error = %error, "describe_image rejected the request (not the device gate)");
        }
        Ok(DescribeOutcome::Unavailable { error })
    }

    /// ON-DEVICE TEXT->IMAGE GENERATION (task #18). Hand the server a text
    /// `prompt` (and optional square `size` / sampling `steps` / `seed`); the
    /// server runs an MLX diffusion (Stable-Diffusion / FLUX-schnell-class) model
    /// ENTIRELY ON-DEVICE, saves the image under state/images/ on the machine, and
    /// returns its ON-DEVICE path. The prompt is handed ONLY to the local model
    /// and the generated pixels stay on the machine — image generation is 100%
    /// on-device, with NO cloud image API anywhere on this path.
    ///
    /// Returns a [`GenerateOutcome`], NOT a bare path, because the op is
    /// DEVICE/RUNTIME-GATED: when the diffusion package is absent, [image] is off /
    /// no model is named server-side, the checkpoint isn't downloaded / failed to
    /// load, or generation failed at runtime, the server replies `ok:false` with
    /// reason=[`GENERATE_IMAGE_UNAVAILABLE_REASON`] — an EXPECTED outcome the daemon
    /// surfaces HONESTLY ("the on-device image model isn't set up"), NEVER a
    /// fabricated image and NEVER a silent cloud fallback. A transport/socket
    /// failure (server down) is the only `Err` here; an `ok:false` WITHOUT the
    /// unavailable reason (a caller-bug ValueError: empty prompt, non-positive
    /// size/steps, non-int seed) surfaces its honest message as
    /// `Unavailable { error }` too. NOT exercised against a real model by any test —
    /// the op dispatch + the unavailable path are proven with a stub; the image
    /// QUALITY/speed are device-gated and never claimed measured.
    ///
    /// `size`/`steps` are clamped to the shared bounds at the daemon boundary too
    /// (the server clamps as well) so a caller can never ask the on-device model
    /// for an out-of-range canvas or an unbounded sampler run. `None` lets the
    /// server apply its defaults; `seed` is passed through only when pinned.
    pub async fn generate_image(
        &mut self,
        prompt: &str,
        size: Option<u32>,
        steps: Option<u32>,
        seed: Option<u32>,
    ) -> Result<GenerateOutcome> {
        let mut req = Request::new(self.fresh_id(), "generate_image");
        req.prompt = Some(prompt);
        // Clamp size/steps into the shared bounds so the daemon can never push an
        // out-of-range canvas / unbounded sampler at the on-device model; absent
        // => the server applies its own default + clamp.
        req.size = size.map(|s| s.clamp(GENERATE_IMAGE_MIN_SIZE, GENERATE_IMAGE_MAX_SIZE));
        req.steps = steps.map(|s| s.clamp(1, GENERATE_IMAGE_MAX_STEPS_CAP));
        req.seed = seed;
        // generate_image needs the raw Response (ok true AND false) so the
        // structured unavailable reason survives — request_generic collapses
        // ok:false into an opaque Err, which would lose the honest "image model
        // isn't set up" signal (and could read as a transport failure).
        let resp = self.request_raw(&req, "generate_image", REQUEST_TIMEOUT).await?;
        if resp.ok {
            let path = resp
                .path
                .filter(|p| !p.trim().is_empty())
                .map(PathBuf::from)
                .ok_or_else(|| anyhow!("generate_image ok response missing path"))?;
            // The model id is non-secret; default to a neutral label if an older
            // server omits it on the ok path. size/steps/seed are NON-secret
            // metadata; default to the daemon defaults if absent.
            let model = resp.model.unwrap_or_else(|| "on-device diffusion".to_string());
            return Ok(GenerateOutcome::Available {
                path,
                model,
                size: resp.size.unwrap_or(GENERATE_IMAGE_DEFAULT_SIZE),
                steps: resp.steps.unwrap_or(GENERATE_IMAGE_DEFAULT_STEPS),
                seed: resp.seed.unwrap_or(0),
            });
        }
        // ok:false — UNAVAILABLE or a caller-bug ValueError. Either way the daemon
        // must NOT show an image: surface the honest server message (or a neutral
        // default) so the caller renders the honest "not set up" copy. The `reason`
        // field distinguishes the DEVICE-GATED case (reason == the contract's
        // GENERATE_IMAGE_UNAVAILABLE_REASON) from a caller-bug ValueError (no
        // reason). NEVER a fabricated image, NEVER a cloud fallback.
        let device_gated = resp.reason.as_deref() == Some(GENERATE_IMAGE_UNAVAILABLE_REASON);
        let error = resp.error.unwrap_or_else(|| {
            if device_gated {
                "the on-device image-generation model is not available".to_string()
            } else {
                "the image could not be generated".to_string()
            }
        });
        if !device_gated {
            // A caller-bug ValueError (empty prompt, non-positive size/steps,
            // non-int seed) — surfaced honestly but NOT the device-gated reason.
            warn!(error = %error, "generate_image rejected the request (not the device gate)");
        }
        Ok(GenerateOutcome::Unavailable { error })
    }

    /// Streamed generate+TTS in one request. Sentence events are pushed into
    /// `events` the moment the server synthesizes them — the caller plays
    /// them while the model is still decoding — and the matching done event
    /// resolves this future. Errors after sentences were emitted are final
    /// (the caller keeps what already played); a transport error before any
    /// event gets the same single stale-connection retry as `request`.
    /// `opener_spoken` is the acknowledgment line already played aloud, when
    /// one fired (no-double-ack contract).
    #[allow(clippy::too_many_arguments)] // mirrors the wire request shape
    pub async fn converse(
        &mut self,
        text: &str,
        max_tokens: u32,
        history: &[(String, String)],
        facts: &[String],
        data: Option<&str>,
        voice: &str,
        opener_spoken: Option<&str>,
        persona: Option<&str>,
        local_model: Option<&str>,
        events: mpsc::UnboundedSender<SentenceEvent>,
    ) -> Result<ConverseDone> {
        let turns: Vec<HistoryTurn> = history
            .iter()
            .flat_map(|(user, darwin)| {
                [
                    HistoryTurn { speaker: "user", text: user },
                    HistoryTurn { speaker: "darwin", text: darwin },
                ]
            })
            .collect();
        let mut req = Request::new(self.fresh_id(), "converse");
        req.text = Some(text);
        req.max_tokens = Some(max_tokens);
        req.voice = Some(voice);
        req.history = if turns.is_empty() { None } else { Some(turns) };
        req.facts = if facts.is_empty() { None } else { Some(facts) };
        req.data = data.filter(|d| !d.is_empty());
        req.opener_spoken = opener_spoken.filter(|o| !o.is_empty());
        req.persona = persona.filter(|p| !p.is_empty());
        // Multi-resident LOCAL sub-choice (task #17): the warm local model the
        // Local tier picked. Carried only when present so the default
        // single-resident wire is unchanged and an old server ignores it.
        req.local_model = local_model.filter(|m| !m.trim().is_empty());

        for attempt in 0..2 {
            // The CONNECT leg gets bounded backoff (recovers a restarted
            // server); the STREAM is never blindly replayed once sentences
            // were emitted — that guard lives below.
            self.connect_with_backoff().await?;
            let mut emitted = 0u64;
            match self.converse_roundtrip(&req, &events, &mut emitted).await {
                Ok(ConverseOutcome::Done(done)) => return Ok(done),
                Ok(ConverseOutcome::ServerFail(msg)) => {
                    return Err(anyhow!("inference converse failed: {msg}"));
                }
                Err(e) => {
                    self.conn = None;
                    // Retrying after sentences were handed out would speak
                    // them twice; only a clean pre-stream failure (stale
                    // socket from a restarted server) is retried.
                    if emitted > 0 || attempt == 1 {
                        return Err(e);
                    }
                }
            }
        }
        unreachable!("converse retry loop returns on its second attempt")
    }

    /// Write one converse request and pump its multi-line response, sending
    /// sentence events to the caller as they arrive. `emitted` counts the
    /// sentences pushed so far even when this returns Err.
    async fn converse_roundtrip(
        &mut self,
        req: &Request<'_>,
        events: &mpsc::UnboundedSender<SentenceEvent>,
        emitted: &mut u64,
    ) -> Result<ConverseOutcome> {
        let (reader, writer) = self.conn.as_mut().expect("connection established by caller");
        let mut line = serde_json::to_string(req)?;
        line.push('\n');
        writer.write_all(line.as_bytes()).await?;
        writer.flush().await?;

        // Rolling deadline (audit fix): the done budget counts time since
        // the LAST received line, not the whole stream — when the afplay
        // degrade rung plays clips inline, the converse future goes unpolled
        // for whole clip durations and an absolute budget would discard a
        // done line that is already sitting in the socket buffer. The check
        // also runs only AFTER a read attempt, so a buffered line is always
        // drained before any timeout fires.
        let mut deadline = tokio::time::Instant::now() + CONVERSE_DONE_TIMEOUT;
        let mut buf = String::new();
        loop {
            // Never sleep past the rolling deadline, but always grant a
            // short window so an already-buffered line is read, not dropped.
            let per_read = CONVERSE_EVENT_TIMEOUT
                .min(deadline.saturating_duration_since(tokio::time::Instant::now()))
                .max(Duration::from_millis(50));
            buf.clear();
            let read = tokio::time::timeout(per_read, reader.read_line(&mut buf)).await;
            let now = tokio::time::Instant::now();
            let n = match read {
                Ok(result) => result?,
                Err(_) if now >= deadline => bail!(
                    "converse timed out after {}s without a done event",
                    CONVERSE_DONE_TIMEOUT.as_secs()
                ),
                Err(_) => bail!(
                    "converse stream stalled (no event within {}s)",
                    per_read.as_secs()
                ),
            };
            if n == 0 {
                bail!("inference server closed the connection mid-converse");
            }
            if buf.len() > MAX_INFERENCE_LINE_BYTES {
                bail!("inference line exceeds {} bytes", MAX_INFERENCE_LINE_BYTES);
            }
            // Any well-formed line is progress: reset the done budget.
            deadline = now + CONVERSE_DONE_TIMEOUT;
            let msg: ConverseLine =
                serde_json::from_str(buf.trim()).context("malformed converse line")?;
            if msg.id != req.id {
                warn!(got = %msg.id, want = %req.id, "converse: mismatched response id; skipping line");
                continue;
            }
            match msg.event.as_deref() {
                Some("sentence") => {
                    let (Some(text), Some(path)) = (msg.text, msg.path) else {
                        bail!("converse sentence event missing text/path");
                    };
                    let seq = msg.seq.unwrap_or(*emitted);
                    *emitted += 1;
                    // A dropped receiver means the caller stopped playing;
                    // keep reading to done so the connection stays in sync.
                    let _ = events.send(SentenceEvent {
                        seq,
                        text,
                        path: PathBuf::from(path),
                    });
                }
                Some("done") => {
                    return if msg.ok == Some(true) {
                        Ok(ConverseOutcome::Done(ConverseDone {
                            text: msg.text.unwrap_or_default(),
                        }))
                    } else {
                        Ok(ConverseOutcome::ServerFail(
                            msg.error.unwrap_or_else(|| "unknown error".to_string()),
                        ))
                    };
                }
                other => {
                    warn!(event = ?other, "converse: unexpected event; ignoring line");
                }
            }
        }
    }

    fn fresh_id(&mut self) -> String {
        self.next_id += 1;
        format!("req-{}", self.next_id)
    }

    /// Open the connection if it is not already up, with a SHORT connect
    /// timeout so a wedged/half-up server cannot hang the op. A live server
    /// accepts instantly; a missing socket fails fast with the same honest
    /// "inference socket unavailable" context the daemon has always surfaced.
    /// NO retry/backoff here — that lives in [`connect_with_backoff`], which
    /// the per-op loops call so a single attempt is still cheap to probe.
    async fn ensure_connected(&mut self) -> Result<()> {
        if self.conn.is_none() {
            let connect = UnixStream::connect(&self.socket_path);
            let stream = match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
                Ok(res) => res.with_context(|| {
                    format!(
                        "inference socket unavailable at {}",
                        self.socket_path.display()
                    )
                })?,
                Err(_) => {
                    return Err(anyhow!(
                        "inference connect timed out after {}s at {}",
                        CONNECT_TIMEOUT.as_secs(),
                        self.socket_path.display()
                    ));
                }
            };
            let (r, w) = stream.into_split();
            self.conn = Some((BufReader::new(r), w));
        }
        Ok(())
    }

    /// Ensure the connection is up, retrying the CONNECT (only) with bounded
    /// exponential backoff + jitter. A dropped/restarted/flapping inference
    /// server is recovered transparently here instead of failing the op on the
    /// first missed connect. The HONESTY CONTRACT is preserved: on exhausting
    /// [`RECONNECT_MAX_ATTEMPTS`] this returns the LAST real connect Err (never
    /// a fake success), so the caller still aborts the turn truthfully. The
    /// happy path (server up) connects on attempt 0 with ZERO added latency.
    /// Only the connect leg is retried here — a transport error mid-roundtrip
    /// is handled by the op loop (which drops the conn and re-enters here),
    /// because a half-streamed converse must not be silently replayed.
    async fn connect_with_backoff(&mut self) -> Result<()> {
        let mut last_err = None;
        for attempt in 0..RECONNECT_MAX_ATTEMPTS {
            let delay = jittered_delay(backoff_delay(attempt), self.next_id.wrapping_add(attempt as u64));
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            match self.ensure_connected().await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    self.conn = None;
                    warn!(
                        attempt = attempt + 1,
                        max = RECONNECT_MAX_ATTEMPTS,
                        error = %e,
                        "inference connect failed; backing off"
                    );
                    last_err = Some(e);
                }
            }
        }
        Err(last_err
            .unwrap_or_else(|| anyhow!("inference connect failed after {RECONNECT_MAX_ATTEMPTS} attempts")))
    }

    /// Connect-probe liveness check: is the inference server reachable RIGHT
    /// NOW? Opens a fresh short-timeout connection and immediately closes it —
    /// it spends NO model call (the server has no `ping` op; a connect+close is
    /// the cheapest honest reachability signal). Does NOT touch `self.conn` (so
    /// it never disturbs an in-flight pooled connection) and never retries:
    /// this is a point-in-time probe for the background liveness task + the
    /// `--selftest` board, which want the truthful current state, not a
    /// best-effort that masks a down server. Returns Ok(()) iff a connection
    /// established within [`CONNECT_TIMEOUT`].
    pub async fn probe_reachable(&self) -> Result<()> {
        let connect = UnixStream::connect(&self.socket_path);
        match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
            Ok(Ok(_stream)) => Ok(()), // dropped here -> clean close
            Ok(Err(e)) => Err(anyhow!(
                "inference socket unreachable at {}: {e}",
                self.socket_path.display()
            )),
            Err(_) => Err(anyhow!(
                "inference connect timed out after {}s at {}",
                CONNECT_TIMEOUT.as_secs(),
                self.socket_path.display()
            )),
        }
    }

    async fn request(&mut self, req: &Request<'_>) -> Result<Response> {
        let op = req.op;
        self.request_generic(req, op, REQUEST_TIMEOUT).await
    }

    /// One-line-out, one-line-in round trip for any serializable request
    /// shape (the flat Request struct, ConsolidateRequest, ...). `timeout`
    /// is per attempt: the interactive ops keep the shared 30s ceiling,
    /// consolidate gets its own generous one.
    async fn request_generic<T: Serialize>(
        &mut self,
        req: &T,
        op: &str,
        timeout: Duration,
    ) -> Result<Response> {
        // One transport retry: a stale connection left over from a restarted
        // inference server fails fast, so reconnect once before reporting
        // failure. The CONNECT leg itself is retried with bounded backoff
        // inside connect_with_backoff, so a dropped/flapping server recovers
        // without each op paying the full timeout ceiling.
        let mut last_err = None;
        for _ in 0..2 {
            self.connect_with_backoff().await?;
            match tokio::time::timeout(timeout, self.roundtrip(req)).await {
                Ok(Ok(resp)) => {
                    if resp.ok {
                        return Ok(resp);
                    }
                    return Err(anyhow!(
                        "inference {} failed: {}",
                        op,
                        resp.error.unwrap_or_else(|| "unknown error".to_string())
                    ));
                }
                Ok(Err(e)) => {
                    self.conn = None;
                    last_err = Some(e);
                }
                Err(_) => {
                    self.conn = None;
                    return Err(anyhow!(
                        "inference {} timed out after {}s",
                        op,
                        timeout.as_secs()
                    ));
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("inference request failed")))
    }

    /// Like [`request_generic`] but returns the RAW [`Response`] WITHOUT
    /// collapsing `ok:false` into an `Err`. Used by describe_image, whose
    /// `ok:false + reason` is a FIRST-CLASS unavailable outcome the caller must
    /// inspect to fall back honestly (not an opaque failure). The same single
    /// stale-connection retry applies to a transport error; a well-formed
    /// response (ok true OR false) is returned as-is. An `Err` here means the
    /// socket itself failed (server down) — never a server-reported `ok:false`.
    async fn request_raw<T: Serialize>(
        &mut self,
        req: &T,
        op: &str,
        timeout: Duration,
    ) -> Result<Response> {
        let mut last_err = None;
        for _ in 0..2 {
            self.connect_with_backoff().await?;
            match tokio::time::timeout(timeout, self.roundtrip(req)).await {
                // A well-formed line — ok true OR false — is the caller's to read.
                Ok(Ok(resp)) => return Ok(resp),
                Ok(Err(e)) => {
                    self.conn = None;
                    last_err = Some(e);
                }
                Err(_) => {
                    self.conn = None;
                    return Err(anyhow!(
                        "inference {} timed out after {}s",
                        op,
                        timeout.as_secs()
                    ));
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("inference request failed")))
    }

    async fn roundtrip<T: Serialize>(&mut self, req: &T) -> Result<Response> {
        let (reader, writer) = self.conn.as_mut().expect("connection established above");
        let mut line = serde_json::to_string(req)?;
        line.push('\n');
        writer.write_all(line.as_bytes()).await?;
        writer.flush().await?;

        let mut buf = String::new();
        let n = reader.read_line(&mut buf).await?;
        if n == 0 {
            bail!("inference server closed the connection");
        }
        if buf.len() > MAX_INFERENCE_LINE_BYTES {
            bail!("inference line exceeds {} bytes", MAX_INFERENCE_LINE_BYTES);
        }
        let resp: Response =
            serde_json::from_str(buf.trim()).context("malformed inference response")?;
        Ok(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_extras_to_request, apply_shape_to_request, backoff_delay, jittered_delay,
        record_probe, reset_health_for_test, Classification,
        ConsolidateRequest, FactPair, InferenceClient, PronunciationRule, Request, Response,
        SpeakExtras, TranscriptPair,
        CONNECT_TIMEOUT, CONSOLIDATE_TIMEOUT, DESCRIBE_IMAGE_DEFAULT_MAX_TOKENS,
        DESCRIBE_IMAGE_MAX_TOKENS_CAP, DESCRIBE_IMAGE_UNAVAILABLE_REASON, GENERATE_IMAGE_DEFAULT_SIZE,
        GENERATE_IMAGE_DEFAULT_STEPS, GENERATE_IMAGE_MAX_SIZE, GENERATE_IMAGE_MAX_STEPS_CAP,
        GENERATE_IMAGE_MIN_SIZE, GENERATE_IMAGE_UNAVAILABLE_REASON, RECONNECT_BASE_DELAY,
        RECONNECT_MAX_ATTEMPTS, RECONNECT_MAX_DELAY, REQUEST_TIMEOUT,
    };
    use serde_json::json;
    use std::time::Duration;

    /// Converse wire contract for the per-agent persona: when set, the request
    /// carries a "persona" string (the agent NAME the server maps to
    /// inference/personas/<name>.txt). When absent it is omitted entirely, so
    /// an old server sees the exact same shape it always did (backward compat).
    #[test]
    fn converse_request_carries_optional_persona() {
        let mut req = Request::new("req-1".to_string(), "converse");
        req.text = Some("hello");
        req.voice = Some("bf_emma");
        req.persona = Some("friday");
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["op"], "converse");
        assert_eq!(v["voice"], "bf_emma");
        assert_eq!(v["persona"], "friday", "persona name must reach the wire");

        // Absent persona is omitted, not null — old servers never see the key.
        let mut bare = Request::new("req-2".to_string(), "converse");
        bare.text = Some("hello");
        let bv = serde_json::to_value(&bare).unwrap();
        assert!(bv.get("persona").is_none(), "persona must be omitted when unset");
    }

    /// Multi-resident LOCAL wire contract (task #17): when the Local tier picked a
    /// warm local model the generate/converse request carries a "local_model"
    /// string (the warm-set id). When absent it is OMITTED entirely, so the default
    /// single-resident wire is identical to today and an old server (no manager)
    /// simply ignores it and answers on the base. Mirrors the persona contract.
    #[test]
    fn request_carries_optional_local_model() {
        // Present -> on the wire.
        let mut req = Request::new("g-1".to_string(), "generate");
        req.text = Some("hi");
        req.local_model = Some("fast-0.6b-4bit");
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["op"], "generate");
        assert_eq!(
            v["local_model"], "fast-0.6b-4bit",
            "the warm local model id must reach the wire"
        );

        // Absent -> omitted, not null (the single-resident default wire).
        let mut bare = Request::new("g-2".to_string(), "converse");
        bare.text = Some("hi");
        let bv = serde_json::to_value(&bare).unwrap();
        assert!(
            bv.get("local_model").is_none(),
            "local_model must be omitted when unset (default single-resident wire)"
        );
    }

    /// speak wire contract — Kokoro path (the default + fallback). The request
    /// carries ONLY {op, text, voice}: NO backend/voice_id/model/el_key fields, so
    /// an old server sees the EXACT pre-tier shape and behaves identically. This is
    /// what every turn sends with the cloud voice tier OFF.
    #[test]
    fn speak_request_kokoro_is_the_pre_tier_shape() {
        let mut req = Request::new("s-1".to_string(), "speak");
        req.text = Some("hello there");
        req.voice = Some("bm_george");
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["op"], "speak");
        assert_eq!(v["text"], "hello there");
        assert_eq!(v["voice"], "bm_george");
        // None of the cloud-tier fields appear on the Kokoro wire.
        for absent in ["backend", "voice_id", "model", "el_key"] {
            assert!(v.get(absent).is_none(), "{absent} must be omitted on the Kokoro path");
        }
    }

    /// speak wire contract — ElevenLabs path (the gated cloud voice tier). The
    /// request adds {backend:"elevenlabs", voice_id, model, el_key}; the daemon only
    /// builds this after the tier decision cleared the full gate. The key rides ONLY
    /// the request body (the server sets the xi-api-key header from it). `voice`
    /// (the Kokoro voice) is NOT set on this path.
    #[test]
    fn speak_request_elevenlabs_carries_backend_voice_model_and_key() {
        let mut req = Request::new("s-2".to_string(), "speak");
        req.text = Some("hello there");
        req.backend = Some("elevenlabs");
        req.voice_id = Some("EL_VOICE_ID");
        req.model = Some("eleven_flash_v2_5");
        req.el_key = Some("sk-secret-key");
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["op"], "speak");
        assert_eq!(v["backend"], "elevenlabs");
        assert_eq!(v["voice_id"], "EL_VOICE_ID");
        assert_eq!(v["model"], "eleven_flash_v2_5");
        assert_eq!(v["el_key"], "sk-secret-key", "key reaches the server in the body only");
        // The Kokoro voice field is not used on the ElevenLabs path.
        assert!(v.get("voice").is_none(), "voice (Kokoro) must be omitted on the EL path");
    }

    /// The ElevenLabs key is carried ONLY in `el_key` and ONLY when present — it is
    /// never folded into any other field, and on the Kokoro path it is omitted
    /// entirely. (Key-hygiene at the wire boundary; the Debug-never-leaks property
    /// of the Backend enum is proven in voice_tier.rs.)
    #[test]
    fn speak_key_field_is_omitted_when_absent() {
        // ElevenLabs backend chosen but no key supplied (the server then falls back
        // to Kokoro): el_key must be absent, never null/empty placeholder.
        let mut req = Request::new("s-3".to_string(), "speak");
        req.text = Some("hi");
        req.backend = Some("elevenlabs");
        req.voice_id = Some("EL");
        req.model = Some("eleven_flash_v2_5");
        req.el_key = None;
        let v = serde_json::to_value(&req).unwrap();
        assert!(v.get("el_key").is_none(), "no key field when the key is absent");
        // The serialized line never contains the literal account name either.
        let line = serde_json::to_string(&req).unwrap();
        assert!(!line.contains("elevenlabs_api_key"), "the Keychain account name never rides the wire");
    }

    // === #33/#34 EXPRESSIVENESS wire shaping (prosody + whisper) ============
    // These exercise the SAME `apply_shape_to_request` the live `speak` path calls,
    // asserting the produced request struct/JSON — NO audio playback, NO EL call, NO
    // mic. The SpeakShape values come from the real `prosody` shaper/whisper folder.

    use crate::config::Config;
    use crate::prosody::{
        apply_whisper, classify_prosody, shape_speak_request, ReplyKind, SpeakShape,
        ELEVENLABS_V3_MODEL,
    };
    use crate::voice_tier::Backend;

    fn cfg_prosody_on() -> Config {
        let mut c = Config::default();
        c.voice.adaptive_prosody = true;
        c
    }
    fn el_v3() -> Backend {
        Backend::ElevenLabs { voice_id: "EL".into(), model: ELEVENLABS_V3_MODEL.into() }
    }
    fn kokoro() -> Backend {
        Backend::Kokoro { voice: "bm_george".into() }
    }

    /// Build the speak request EXACTLY as `InferenceClient::speak` does (ElevenLabs
    /// arm), then apply the shape — so the test asserts the real wire the daemon
    /// sends. No client/socket needed.
    fn el_speak_req_with(shape: &SpeakShape) -> serde_json::Value {
        let mut req = Request::new("s-x".to_string(), "speak");
        req.text = Some("hello there");
        req.backend = Some("elevenlabs");
        req.voice_id = Some("EL");
        req.model = Some(ELEVENLABS_V3_MODEL);
        apply_shape_to_request(&mut req, shape);
        serde_json::to_value(&req).unwrap()
    }
    fn kokoro_speak_req_with(shape: &SpeakShape) -> serde_json::Value {
        let mut req = Request::new("s-y".to_string(), "speak");
        req.text = Some("hello there");
        req.voice = Some("bm_george");
        apply_shape_to_request(&mut req, shape);
        serde_json::to_value(&req).unwrap()
    }

    /// (a) adaptive_prosody ON + EL-v3: an Urgent (Alert) reply carries the
    /// `[urgently]` audio-tag + stability/style; a Calm (Wellness) reply the `[calm]`
    /// tag. The rich surface rides the wire only on this v3 path.
    #[test]
    fn prosody_on_el_v3_request_carries_audio_tag_and_v3_settings() {
        let cfg = cfg_prosody_on();
        // Urgent (an alert/heal/urgent telemetry reply -> Alert -> Urgent).
        let urgent = shape_speak_request(&cfg, classify_prosody(ReplyKind::Alert, false), &el_v3());
        let v = el_speak_req_with(&urgent);
        assert_eq!(v["audio_tag"], "[urgently]");
        assert!(v["stability"].is_number() && v["style"].is_number(), "v3 carries stability+style");
        // Calm (a wellness/biometric reply -> Wellness -> Calm).
        let calm = shape_speak_request(&cfg, classify_prosody(ReplyKind::Wellness, false), &el_v3());
        let vc = el_speak_req_with(&calm);
        assert_eq!(vc["audio_tag"], "[calm]");
    }

    /// (b) adaptive_prosody ON + Kokoro: ONLY a coarse rate rides the wire — NO
    /// audio-tag, NO v3 settings (rich prosody is EL-v3-gated and never faked).
    #[test]
    fn prosody_on_kokoro_request_carries_only_coarse_rate_no_tag() {
        let cfg = cfg_prosody_on();
        let urgent = shape_speak_request(&cfg, classify_prosody(ReplyKind::Alert, false), &kokoro());
        let v = kokoro_speak_req_with(&urgent);
        assert!(v.get("audio_tag").is_none(), "no faked audio-tag on Kokoro");
        assert!(v.get("stability").is_none() && v.get("style").is_none(), "no v3 settings on Kokoro");
        assert!(v["rate"].is_number(), "the one coarse signal Kokoro gets is rate");
        assert!(v["rate"].as_f64().unwrap() > 1.0, "Urgent nudges the coarse rate up");
    }

    /// (c) whisper ON -> the request carries the lowered volume (terse is a
    /// reply-builder flag, not a wire field); whisper OFF -> byte-for-byte today's
    /// (no rate/volume/tag added on a Neutral routine reply).
    #[test]
    fn whisper_on_request_lowers_volume_off_is_byte_for_byte() {
        let cfg = cfg_prosody_on();
        // Routine reply (Neutral profile) on EL-v3, whisper ENGAGED.
        let base = shape_speak_request(&cfg, classify_prosody(ReplyKind::Routine, false), &el_v3());
        let whispered = apply_whisper(base.clone(), /*whisper_on=*/ true, /*required=*/ false);
        let v = el_speak_req_with(&whispered);
        assert!(v["volume"].is_number(), "whisper lowers the volume on the wire");
        assert!(v["volume"].as_f64().unwrap() < 1.0);
        // Whisper does NOT invent an audio-tag.
        assert!(v.get("audio_tag").is_none());
        // Whisper OFF on the same Neutral routine reply -> NOTHING extra on the wire.
        let off = apply_whisper(base, false, false);
        let vo = el_speak_req_with(&off);
        for absent in ["audio_tag", "stability", "style", "rate", "volume"] {
            assert!(vo.get(absent).is_none(), "whisper-off Neutral reply must omit {absent}");
        }
    }

    /// (d) a REQUIRED confirmation is NOT softened/silenced even while whispering: the
    /// request keeps full volume (no `volume` field) and no terse trimming.
    #[test]
    fn whisper_never_softens_a_required_confirmation_on_the_wire() {
        let cfg = cfg_prosody_on();
        // required_confirm=true -> classify stays Neutral; apply_whisper leaves it
        // full-volume even with whisper engaged.
        let shape = shape_speak_request(&cfg, classify_prosody(ReplyKind::Alert, true), &el_v3());
        let guarded = apply_whisper(shape, /*whisper_on=*/ true, /*required=*/ true);
        let v = el_speak_req_with(&guarded);
        assert!(v.get("volume").is_none(), "a required confirm must stay full-volume on the wire");
        assert!(v.get("audio_tag").is_none(), "a required confirm is delivered plainly");
        assert!(!guarded.terse, "a required confirm is never trimmed");
    }

    /// (e) FEATURE OFF (explicit): the speak request is BYTE-FOR-BYTE today's on every
    /// backend — the shape is the identity and adds nothing. (The shipped DEFAULT is
    /// now ON, full-power; this proves the off path still produces today's exact wire
    /// when an operator disables prosody.)
    #[test]
    fn prosody_off_speak_request_is_byte_for_byte_today() {
        let mut cfg = Config::default();
        cfg.voice.adaptive_prosody = false; // explicit off-path
        cfg.voice.whisper = false;
        for kind in [ReplyKind::Routine, ReplyKind::Alert, ReplyKind::Wellness, ReplyKind::Greeting] {
            for backend in [el_v3(), kokoro()] {
                let shape = shape_speak_request(&cfg, classify_prosody(kind, false), &backend);
                let folded = apply_whisper(shape, /*whisper_on=*/ false, false);
                assert!(folded.is_neutral(), "off path must be the identity shape");
                // The reference request WITHOUT any shaping, vs WITH the neutral shape:
                // they must be identical JSON (byte-for-byte today's wire).
                let mut bare = Request::new("ref".to_string(), "speak");
                let mut shaped = Request::new("ref".to_string(), "speak");
                match &backend {
                    Backend::ElevenLabs { voice_id, model } => {
                        for r in [&mut bare, &mut shaped] {
                            r.text = Some("hello");
                            r.backend = Some("elevenlabs");
                            r.voice_id = Some(voice_id);
                            r.model = Some(model);
                        }
                    }
                    Backend::Kokoro { voice } => {
                        for r in [&mut bare, &mut shaped] {
                            r.text = Some("hello");
                            r.voice = Some(voice);
                        }
                    }
                }
                apply_shape_to_request(&mut shaped, &folded);
                assert_eq!(
                    serde_json::to_value(&bare).unwrap(),
                    serde_json::to_value(&shaped).unwrap(),
                    "feature OFF must be byte-for-byte today's request for {kind:?} on {backend:?}"
                );
            }
        }
    }

    /// SpeakShape::neutral() (the identity) must add NOTHING to the request — the
    /// invariant the whole OFF-default byte-for-byte guarantee rests on.
    #[test]
    fn neutral_shape_adds_nothing_to_the_request() {
        let v = el_speak_req_with(&SpeakShape::neutral());
        for absent in ["audio_tag", "stability", "style", "rate", "volume"] {
            assert!(v.get(absent).is_none(), "neutral shape must omit {absent}");
        }
    }

    /// transcribe wire contract — Whisper path (the default + fallback). The request
    /// carries ONLY {op, path}: NO backend/model/el_key fields, so an old server sees
    /// the EXACT pre-tier shape and the user's audio stays on-device. This is what
    /// every turn sends with the cloud-STT tier OFF (the pinned default).
    #[test]
    fn transcribe_request_whisper_is_the_pre_tier_shape() {
        let mut req = Request::new("t-1".to_string(), "transcribe");
        req.path = Some("/tmp/utt.wav".to_string());
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["op"], "transcribe");
        assert_eq!(v["path"], "/tmp/utt.wav");
        for absent in ["backend", "model", "el_key", "lang", "voice"] {
            assert!(v.get(absent).is_none(), "{absent} must be omitted on the whisper path");
        }
    }

    /// transcribe wire contract — ElevenLabs Scribe path (the gated cloud-STT tier).
    /// The request adds {backend:"elevenlabs_scribe", model, el_key}; the daemon only
    /// builds this after the tier decision cleared the full gate. The key rides ONLY
    /// the request body (the server sets the xi-api-key header from it). HONESTY: on
    /// this path the user's VOICE AUDIO (the wav at `path`) leaves the device.
    #[test]
    fn transcribe_request_scribe_carries_backend_model_and_key() {
        let mut req = Request::new("t-2".to_string(), "transcribe");
        req.path = Some("/tmp/utt.wav".to_string());
        req.backend = Some("elevenlabs_scribe");
        req.model = Some("scribe_v1");
        req.el_key = Some("sk-secret-key");
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["op"], "transcribe");
        assert_eq!(v["backend"], "elevenlabs_scribe");
        assert_eq!(v["model"], "scribe_v1");
        assert_eq!(v["el_key"], "sk-secret-key", "key reaches the server in the body only");
        // The serialized line never contains the literal account name.
        let line = serde_json::to_string(&req).unwrap();
        assert!(!line.contains("elevenlabs_api_key"), "the Keychain account name never rides the wire");
    }

    /// clone_voice wire contract — the consent-gated clone seam. The request carries
    /// {op:"clone_voice", path (the owner sample), text (the display name), el_key}.
    /// The audio SAMPLE at `path` is what leaves the device; the key rides ONLY the
    /// body. The voice_id comes back in the response (NON-secret), and the account
    /// name never appears on the wire.
    #[test]
    fn clone_voice_request_carries_sample_name_and_key_only() {
        let mut req = Request::new("c-1".to_string(), "clone_voice");
        req.path = Some("/root/state/voiceid/owner.wav".to_string());
        req.text = Some("DARWIN cloned voice (darwin)");
        req.el_key = Some("sk-secret-key");
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["op"], "clone_voice");
        assert_eq!(v["path"], "/root/state/voiceid/owner.wav");
        assert_eq!(v["text"], "DARWIN cloned voice (darwin)");
        assert_eq!(v["el_key"], "sk-secret-key");
        let line = serde_json::to_string(&req).unwrap();
        assert!(!line.contains("elevenlabs_api_key"), "the Keychain account name never rides the wire");

        // The clone response carries a NON-secret voice_id; non-clone responses don't.
        let cloned = serde_json::from_str::<Response>(
            r#"{"id":"c-1","ok":true,"voice_id":"EL_CLONED_ID","latency_ms":900}"#,
        )
        .unwrap();
        assert_eq!(cloned.voice_id.as_deref(), Some("EL_CLONED_ID"));
        let other = serde_json::from_str::<Response>(
            r#"{"id":"req-1","ok":true,"text":"hi","latency_ms":3}"#,
        )
        .unwrap();
        assert!(other.voice_id.is_none(), "voice_id is absent on non-clone responses");
    }

    /// sound_effect wire contract (Phase-2). The request carries {op:"sound_effect",
    /// text (the SFX prompt), el_key} plus the OPTIONAL {duration_s, prompt_influence}
    /// shaping hints when pinned; the key rides ONLY the body and the account name never
    /// rides the wire. The cue WAV path comes back in the response.
    #[test]
    fn sound_effect_request_carries_prompt_key_and_optional_shaping() {
        // With shaping pinned: all four fields ride.
        let mut req = Request::new("sfx-1".to_string(), "sound_effect");
        req.text = Some("a short metallic chime");
        req.el_key = Some("sk-secret-key");
        req.duration_s = Some(2.0);
        req.prompt_influence = Some(0.7);
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["op"], "sound_effect");
        assert_eq!(v["text"], "a short metallic chime");
        assert_eq!(v["el_key"], "sk-secret-key", "key reaches the server in the body only");
        assert_eq!(v["duration_s"].as_f64().unwrap(), 2.0);
        // f32 -> f64 widening: compare within tolerance rather than to the exact literal.
        assert!((v["prompt_influence"].as_f64().unwrap() - 0.7).abs() < 1e-6);
        let line = serde_json::to_string(&req).unwrap();
        assert!(!line.contains("elevenlabs_api_key"), "the Keychain account name never rides the wire");

        // Without shaping: the optional hints are OMITTED (server defaults apply).
        let mut bare = Request::new("sfx-2".to_string(), "sound_effect");
        bare.text = Some("a soft click");
        bare.el_key = Some("sk-k");
        let bv = serde_json::to_value(&bare).unwrap();
        assert!(bv.get("duration_s").is_none(), "duration_s omitted when unset");
        assert!(bv.get("prompt_influence").is_none(), "prompt_influence omitted when unset");

        // The SFX response carries the cue WAV path.
        let resp = serde_json::from_str::<Response>(
            r#"{"id":"sfx-1","ok":true,"path":"/state/tmp/sfx.wav","latency_ms":700}"#,
        )
        .unwrap();
        assert_eq!(resp.path.as_deref(), Some("/state/tmp/sfx.wav"));
    }

    /// design_voice wire contract (Phase-2). The request carries {op:"design_voice",
    /// text (the voice DESCRIPTION — server reads req.text), voice (the display name —
    /// server reads req.voice || req.name), el_key}. NO path/audio rides (text-only —
    /// no audio leaves the device). The minted voice_id comes back in the response.
    #[test]
    fn design_voice_request_is_text_only_with_description_name_and_key() {
        let mut req = Request::new("dv-1".to_string(), "design_voice");
        req.text = Some("a warm, calm baritone with a slight rasp");
        req.voice = Some("Atlas");
        req.el_key = Some("sk-secret-key");
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["op"], "design_voice");
        assert_eq!(v["text"], "a warm, calm baritone with a slight rasp", "the description rides as text");
        assert_eq!(v["voice"], "Atlas", "the display name rides as voice");
        assert_eq!(v["el_key"], "sk-secret-key", "key reaches the server in the body only");
        // No audio sample leaves the device on the design path.
        assert!(v.get("path").is_none(), "design_voice is TEXT-ONLY — no audio sample path");
        let line = serde_json::to_string(&req).unwrap();
        assert!(!line.contains("elevenlabs_api_key"), "the Keychain account name never rides the wire");

        // The design response carries a NON-secret voice_id (same field as clone).
        let resp = serde_json::from_str::<Response>(
            r#"{"id":"dv-1","ok":true,"voice_id":"EL_DESIGNED_ID","latency_ms":1200}"#,
        )
        .unwrap();
        assert_eq!(resp.voice_id.as_deref(), Some("EL_DESIGNED_ID"));
    }

    /// create_pronunciation wire contract (Phase-2). The request carries
    /// {op:"create_pronunciation", name, rules:[...], el_key}; the rules are TEXT only
    /// (no audio leaves the device). The minted NON-secret (dictionary_id, version_id)
    /// pair comes back in the response.
    #[test]
    fn create_pronunciation_request_carries_name_rules_and_key() {
        let rules = vec![
            PronunciationRule {
                string_to_replace: "DARWIN".to_string(),
                rule_type: "alias".to_string(),
                alias: Some("darwins".to_string()),
                phoneme: None,
                alphabet: None,
            },
            PronunciationRule {
                string_to_replace: "nginx".to_string(),
                rule_type: "phoneme".to_string(),
                alias: None,
                phoneme: Some("ˈɛndʒɪnˌɛks".to_string()),
                alphabet: Some("ipa".to_string()),
            },
        ];
        let mut req = Request::new("cp-1".to_string(), "create_pronunciation");
        req.name = Some("DARWIN dictionary");
        req.rules = Some(&rules);
        req.el_key = Some("sk-secret-key");
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["op"], "create_pronunciation");
        assert_eq!(v["name"], "DARWIN dictionary");
        assert_eq!(v["el_key"], "sk-secret-key", "key reaches the server in the body only");
        // The rules ride as a flat list matching the EL add-from-rules contract.
        assert_eq!(v["rules"][0]["string_to_replace"], "DARWIN");
        assert_eq!(v["rules"][0]["type"], "alias", "the rule discriminator serializes as `type`");
        assert_eq!(v["rules"][0]["alias"], "darwins");
        assert!(v["rules"][0].get("phoneme").is_none(), "an alias rule omits phoneme");
        assert_eq!(v["rules"][1]["type"], "phoneme");
        assert_eq!(v["rules"][1]["phoneme"], "ˈɛndʒɪnˌɛks");
        assert_eq!(v["rules"][1]["alphabet"], "ipa");
        assert!(v["rules"][1].get("alias").is_none(), "a phoneme rule omits alias");
        let line = serde_json::to_string(&req).unwrap();
        assert!(!line.contains("elevenlabs_api_key"), "the Keychain account name never rides the wire");

        // The response carries the NON-secret (dictionary_id, version_id) pair.
        let resp = serde_json::from_str::<Response>(
            r#"{"id":"cp-1","ok":true,"dictionary_id":"EL_PD_ID","version_id":"EL_PD_VER","latency_ms":500}"#,
        )
        .unwrap();
        assert_eq!(resp.dictionary_id.as_deref(), Some("EL_PD_ID"));
        assert_eq!(resp.version_id.as_deref(), Some("EL_PD_VER"));
        // Non-pronunciation responses carry neither id.
        let other = serde_json::from_str::<Response>(
            r#"{"id":"x","ok":true,"text":"hi","latency_ms":3}"#,
        )
        .unwrap();
        assert!(other.dictionary_id.is_none() && other.version_id.is_none());
    }

    /// ADDITIVE speak-threading (Phase-2): with DEFAULT extras (SpeakExtras::none, the
    /// shipped config default — stream_tts OFF, empty pronunciation dictionary) the
    /// speak request is BYTE-FOR-BYTE today's: NO `stream` and NO `pronunciation_locators`
    /// fields appear, on EITHER backend. This is the safety property — the default
    /// behavior must not change.
    #[test]
    fn speak_extras_default_leave_the_request_unchanged() {
        let none = SpeakExtras::none();
        assert_eq!(none, SpeakExtras::default());

        // Kokoro path (default tier OFF): byte-for-byte today's.
        let mut kk = Request::new("se-1".to_string(), "speak");
        kk.text = Some("hello");
        kk.voice = Some("bm_george");
        apply_extras_to_request(&mut kk, &none);
        let kv = serde_json::to_value(&kk).unwrap();
        assert!(kv.get("stream").is_none(), "no stream field with default extras (Kokoro)");
        assert!(
            kv.get("pronunciation_locators").is_none(),
            "no pronunciation_locators with default extras (Kokoro)"
        );

        // ElevenLabs path: the additive fields are STILL omitted with default extras.
        let mut el = Request::new("se-2".to_string(), "speak");
        el.text = Some("hello");
        el.backend = Some("elevenlabs");
        el.voice_id = Some("EL");
        el.model = Some("eleven_flash_v2_5");
        apply_extras_to_request(&mut el, &none);
        let ev = serde_json::to_value(&el).unwrap();
        assert!(ev.get("stream").is_none(), "no stream field with default extras (EL)");
        assert!(ev.get("pronunciation_locators").is_none(), "no locators with default extras (EL)");
    }

    /// ADDITIVE speak-threading (Phase-2): when the operator opts IN, the extras ride
    /// the wire — `stream:true` and a single pronunciation locator {dictionary_id[,
    /// version_id]}. The version rides only when non-empty (else EL uses the latest).
    /// And streaming is only ever carried as `true` (the opt-in is never sent as false).
    #[test]
    fn speak_extras_thread_stream_and_pronunciation_locator_when_set() {
        // Both opted in, with a pinned version.
        let extras = SpeakExtras {
            stream: Some(true),
            pronunciation_dictionary_id: "EL_PD_ID".to_string(),
            pronunciation_dictionary_version: "EL_PD_VER".to_string(),
        };
        let mut req = Request::new("se-3".to_string(), "speak");
        req.text = Some("nginx");
        req.backend = Some("elevenlabs");
        req.voice_id = Some("EL");
        req.model = Some("eleven_flash_v2_5");
        apply_extras_to_request(&mut req, &extras);
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["stream"], true, "streaming opt-in rides the wire when set");
        let loc = &v["pronunciation_locators"][0];
        assert_eq!(loc["pronunciation_dictionary_id"], "EL_PD_ID");
        assert_eq!(loc["version_id"], "EL_PD_VER", "the pinned version rides alongside the id");

        // Dictionary id set but NO version pinned -> the locator omits version_id.
        let no_ver = SpeakExtras {
            stream: None,
            pronunciation_dictionary_id: "EL_PD_ID".to_string(),
            pronunciation_dictionary_version: String::new(),
        };
        let mut req2 = Request::new("se-4".to_string(), "speak");
        req2.text = Some("hi");
        req2.voice = Some("bm_george");
        apply_extras_to_request(&mut req2, &no_ver);
        let v2 = serde_json::to_value(&req2).unwrap();
        assert!(v2.get("stream").is_none(), "stream omitted when not opted in");
        let loc2 = &v2["pronunciation_locators"][0];
        assert_eq!(loc2["pronunciation_dictionary_id"], "EL_PD_ID");
        assert!(
            loc2.get("version_id").is_none(),
            "version_id omitted when empty (EL uses the latest version)"
        );
    }

    /// speak wire contract — Babel target language (build 2/2). The `lang` field
    /// rides the speak request ONLY when present + non-empty, so the EL backend can
    /// pick a multilingual model; an English/absent target omits it entirely (the
    /// default wire is unchanged). NON-secret (a language name).
    #[test]
    fn speak_request_carries_target_language_only_when_present() {
        // Babel non-English: the lang field reaches the wire alongside the EL fields.
        let mut req = Request::new("s-4".to_string(), "speak");
        req.text = Some("Hola");
        req.backend = Some("elevenlabs");
        req.voice_id = Some("EL_VOICE");
        req.model = Some("eleven_multilingual_v2");
        req.lang = Some("Spanish");
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["lang"], "Spanish", "the Babel target language must reach the wire");

        // Ordinary English reply: no lang field at all.
        let mut eng = Request::new("s-5".to_string(), "speak");
        eng.text = Some("Hello there");
        eng.voice = Some("bm_george");
        let ev = serde_json::to_value(&eng).unwrap();
        assert!(ev.get("lang").is_none(), "lang must be omitted on an English reply");
    }

    /// Audit fix: consolidate is the largest generation in the system and
    /// shares the server's engine lock with live replies — its budget must
    /// dwarf the interactive ceiling, not equal it.
    #[test]
    fn consolidate_gets_its_own_generous_timeout() {
        assert!(CONSOLIDATE_TIMEOUT.as_secs() >= 120);
        assert!(CONSOLIDATE_TIMEOUT > REQUEST_TIMEOUT * 2);
    }

    /// Backward compat: an old server's classify response carries no args
    /// field at all — it must deserialize with the default (Null), which the
    /// router treats as "no args".
    #[test]
    fn classification_args_default_on_old_server_responses() {
        let c: Classification = serde_json::from_str(
            r#"{"intent":"app.launch","confidence":0.92,"complexity":"light"}"#,
        )
        .unwrap();
        assert_eq!(c.args, serde_json::Value::Null);
        assert!(c.args.get("url").is_none());

        // The full wire Response shape, also argless.
        let resp: Response = serde_json::from_str(
            r#"{"id":"req-1","ok":true,"intent":"system.query","confidence":0.95,"complexity":"light","latency_ms":12}"#,
        )
        .unwrap();
        assert!(resp.args.is_none());
    }

    /// #31 DIARIZATION wire contract: a transcribe response that carries a Scribe
    /// `words` stream (with speaker_ids) deserializes onto `Response.words`, and feeding
    /// it through the PURE `diarize::diarize` mapper yields the REAL per-speaker turns —
    /// this is the live seam `transcribe_diarized` returns. An OLD server / the whisper
    /// path carries NO `words` field, which must default to None (the daemon then renders
    /// the honest single stream, never a fabricated speaker).
    #[test]
    fn transcribe_response_words_stream_flows_to_diarize() {
        // EL-Scribe diarized: two speakers in the per-word stream.
        let resp: Response = serde_json::from_str(
            r#"{"id":"t-9","ok":true,"text":"hello hi",
                "words":[
                    {"text":"hello","type":"word","speaker_id":"speaker_0","start":0.0,"end":0.4},
                    {"text":"hi","type":"word","speaker_id":"speaker_1","start":0.5,"end":0.7}
                ],
                "latency_ms":20}"#,
        )
        .unwrap();
        let words = resp.words.clone().expect("scribe words present on the response");
        assert_eq!(words.len(), 2);
        let scribe = crate::diarize::ScribeResponse { text: resp.text.clone().unwrap(), words };
        let turns = crate::diarize::diarize(&scribe);
        assert_eq!(turns.len(), 2, "two distinct speakers -> two turns");
        assert_eq!(turns[0].speaker_id, "speaker_0");
        assert_eq!(turns[1].speaker_id, "speaker_1");
        assert!(crate::diarize::is_multi_speaker(&turns), "real Scribe labels, multi-speaker");

        // Old server / whisper path: no `words` field at all -> None (honest single
        // stream is the daemon's fallback, never a fabricated speaker).
        let plain: Response = serde_json::from_str(
            r#"{"id":"t-10","ok":true,"text":"what is the time","latency_ms":11}"#,
        )
        .unwrap();
        assert!(plain.words.is_none(), "absent words must default to None (whisper/old server)");
    }

    /// New servers pass the model's args object through verbatim.
    #[test]
    fn classification_args_pass_through_when_present() {
        let c: Classification = serde_json::from_str(
            r#"{"intent":"web.open","confidence":0.9,"complexity":"light","args":{"url":"apple.com","browser":"safari"}}"#,
        )
        .unwrap();
        assert_eq!(c.args["url"], "apple.com");
        assert_eq!(c.args["browser"], "safari");
    }

    /// Shared wire contract for op=consolidate: the request must serialize
    /// exactly as the server parses it.
    #[test]
    fn consolidate_request_serializes_to_the_wire_shape() {
        let req = ConsolidateRequest {
            id: "req-7".to_string(),
            op: "consolidate",
            transcripts: vec![TranscriptPair {
                user: "my name is Darwin",
                darwin: "Noted, sir.",
            }],
            facts: vec![FactPair {
                key: "user.name",
                value: "Darwin",
            }],
        };
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            json!({
                "id": "req-7",
                "op": "consolidate",
                "transcripts": [{"user": "my name is Darwin", "darwin": "Noted, sir."}],
                "facts": [{"key": "user.name", "value": "Darwin"}],
            })
        );
    }

    /// Wire contract for op=embed: the request carries op="embed" and the
    /// "texts" batch (and nothing else — no text/path/voice keys leak in), so
    /// the server parses exactly what the daemon sends. A server WITHOUT the
    /// embed op rejects this as an unknown op, which the recall layer reads as
    /// "embedder unavailable" and falls back to BM25.
    #[test]
    fn embed_request_serializes_to_the_wire_shape() {
        let texts = vec!["my car".to_string(), "user.car blue Subaru".to_string()];
        let mut req = Request::new("req-9".to_string(), "embed");
        req.texts = Some(&texts);
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["op"], "embed");
        assert_eq!(v["texts"][0], "my car");
        assert_eq!(v["texts"][1], "user.car blue Subaru");
        // Unrelated fields stay omitted so an old server sees a clean shape.
        assert!(v.get("text").is_none());
        assert!(v.get("path").is_none());
        assert!(v.get("voice").is_none());
    }

    /// Backward compat / forward compat: the embed response carries a "vectors"
    /// array (one L2-normalized vector per input) PLUS the vector-space
    /// metadata (`embedder` id / `dim` / `fell_back` — the server's op=embed
    /// WIRE CONTRACT); ops that don't return them deserialize with every field
    /// == None, and present values round-trip.
    #[test]
    fn embed_response_vectors_round_trip_and_default() {
        let with = serde_json::from_str::<Response>(
            r#"{"id":"req-9","ok":true,"vectors":[[0.1,0.2],[0.3,0.4]],
                "embedder":"coreml-bge-small-en-v1.5","dim":2,"fell_back":false,
                "latency_ms":7}"#,
        )
        .unwrap();
        let vecs = with.vectors.expect("vectors present");
        assert_eq!(vecs.len(), 2);
        assert_eq!(vecs[0], vec![0.1, 0.2]);
        assert_eq!(with.embedder.as_deref(), Some("coreml-bge-small-en-v1.5"));
        assert_eq!(with.dim, Some(2));
        assert_eq!(with.fell_back, Some(false));

        // A non-embed response (or an old server) carries no vectors field —
        // and no space metadata: an OLD server predates the metadata entirely,
        // so every field deserializes as None (a persisting caller then keys
        // the batch to its own opaque placeholder, never assuming a backend).
        let without = serde_json::from_str::<Response>(
            r#"{"id":"req-1","ok":true,"text":"hi","latency_ms":3}"#,
        )
        .unwrap();
        assert!(without.vectors.is_none());
        assert!(without.embedder.is_none());
        assert!(without.dim.is_none());
        assert!(without.fell_back.is_none());

        // The mean-pool honest-fallback shape: an OPAQUE model-derived id +
        // fell_back true, dim null on an empty batch — all representable. The id
        // is treated as an opaque equality token, so the exact string is not
        // load-bearing here beyond round-tripping intact.
        let fallback = serde_json::from_str::<Response>(
            r#"{"id":"req-2","ok":true,"vectors":[],
                "embedder":"llm-meanpool:qwen3-4b","dim":null,"fell_back":true,
                "latency_ms":2}"#,
        )
        .unwrap();
        assert_eq!(fallback.embedder.as_deref(), Some("llm-meanpool:qwen3-4b"));
        assert!(fallback.dim.is_none());
        assert_eq!(fallback.fell_back, Some(true));
    }

    /// Wire contract for op=rerank (STAGE TWO): the request carries op="rerank",
    /// the "query", and the "passages" shortlist — and nothing else (no texts /
    /// text / path leak in), so the server parses exactly what the daemon sends.
    /// A server WITHOUT the rerank op rejects this as an unknown op, which the
    /// caller reads as "reranker unavailable" and keeps the dense order.
    #[test]
    fn rerank_request_serializes_to_the_wire_shape() {
        let passages = vec![
            "the user drinks oat-milk cortados".to_string(),
            "the user always flies Delta".to_string(),
        ];
        let mut req = Request::new("r-1".to_string(), "rerank");
        req.query = Some("what coffee does the user drink?");
        req.passages = Some(&passages);
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["op"], "rerank");
        assert_eq!(v["query"], "what coffee does the user drink?");
        assert_eq!(v["passages"][0], "the user drinks oat-milk cortados");
        assert_eq!(v["passages"][1], "the user always flies Delta");
        // Unrelated fields stay omitted so an old server sees a clean shape.
        assert!(v.get("texts").is_none());
        assert!(v.get("text").is_none());
        assert!(v.get("path").is_none());
    }

    /// The op=rerank response carries "scores" (one per passage, input order) plus
    /// the "reranker" id and the shared "fell_back" flag; an old server (or a
    /// non-rerank op) deserializes with every field None.
    #[test]
    fn rerank_response_scores_round_trip_and_default() {
        let with = serde_json::from_str::<Response>(
            r#"{"id":"r-1","ok":true,"scores":[6.5,-8.1,1.0],
                "reranker":"coreml-ms-marco-minilm-l6-v2","fell_back":false,
                "latency_ms":221}"#,
        )
        .unwrap();
        let scores = with.scores.expect("scores present");
        assert_eq!(scores, vec![6.5, -8.1, 1.0]);
        assert_eq!(with.reranker.as_deref(), Some("coreml-ms-marco-minilm-l6-v2"));
        assert_eq!(with.fell_back, Some(false));

        // An old server (no rerank op) carries neither scores nor reranker.
        let without = serde_json::from_str::<Response>(
            r#"{"id":"x","ok":true,"text":"hi","latency_ms":3}"#,
        )
        .unwrap();
        assert!(without.scores.is_none());
        assert!(without.reranker.is_none());

        // The honest fallback shape: fell_back true with an EMPTY reranker id (no
        // model scored -> the caller keeps its dense order).
        let fallback = serde_json::from_str::<Response>(
            r#"{"id":"r-2","ok":true,"scores":[2.0,1.0],"reranker":"",
                "fell_back":true,"latency_ms":1}"#,
        )
        .unwrap();
        assert_eq!(fallback.fell_back, Some(true));
        assert_eq!(fallback.reranker.as_deref(), Some(""));
    }

    /// describe_image wire contract — the request carries op="describe_image",
    /// the LOCAL image `path` (the daemon confines it BEFORE this), an OPTIONAL
    /// `question` (the VQA), and a `max_tokens` decode budget. Unrelated fields
    /// stay omitted so an old server (no VLM op) sees a clean shape it rejects as
    /// unknown (which the daemon reads as unavailable -> falls back honestly).
    #[test]
    fn describe_image_request_serializes_to_the_wire_shape() {
        let mut req = Request::new("d-1".to_string(), "describe_image");
        req.path = Some("/root/state/vision/frame.png".to_string());
        req.question = Some("what color is the car?");
        req.max_tokens = Some(256);
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["op"], "describe_image");
        assert_eq!(v["path"], "/root/state/vision/frame.png");
        assert_eq!(v["question"], "what color is the car?", "the VQA question must reach the wire");
        assert_eq!(v["max_tokens"], 256);
        // None of the unrelated op fields leak in.
        for absent in ["text", "voice", "voice_id", "backend", "el_key", "texts"] {
            assert!(v.get(absent).is_none(), "{absent} must be omitted on the describe_image wire");
        }
    }

    /// describe_image wire contract — a GENERAL scene describe (no question): the
    /// `question` field is OMITTED entirely (not null), so the server applies its
    /// DESCRIBE_IMAGE_DEFAULT_PROMPT.
    #[test]
    fn describe_image_request_omits_question_for_a_general_describe() {
        let mut req = Request::new("d-2".to_string(), "describe_image");
        req.path = Some("/root/img.jpg".to_string());
        req.max_tokens = Some(DESCRIBE_IMAGE_DEFAULT_MAX_TOKENS);
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["op"], "describe_image");
        assert!(v.get("question").is_none(), "no question => the field is omitted (server uses its default prompt)");
        assert_eq!(v["max_tokens"], DESCRIBE_IMAGE_DEFAULT_MAX_TOKENS);
    }

    /// describe_image AVAILABLE response: ok:true carries the description `text`
    /// and the non-secret VLM `model`. (The daemon maps this to
    /// DescribeOutcome::Available — the description is the model's VISUAL
    /// understanding, distinct from OCR glyphs.)
    #[test]
    fn describe_image_available_response_carries_text_and_model() {
        let resp = serde_json::from_str::<Response>(
            r#"{"id":"d-1","ok":true,"text":"A red car parked on a street.","model":"mlx-community/Qwen2-VL-2B-Instruct-4bit","latency_ms":1200}"#,
        )
        .unwrap();
        assert!(resp.ok);
        assert_eq!(resp.text.as_deref(), Some("A red car parked on a street."));
        assert_eq!(resp.model.as_deref(), Some("mlx-community/Qwen2-VL-2B-Instruct-4bit"));
        assert!(resp.reason.is_none(), "the available path carries no unavailable reason");
    }

    /// describe_image UNAVAILABLE response: ok:false carries the stable
    /// `reason`="vlm_unavailable" + an honest `error` and NO text — NEVER a
    /// fabricated description. The daemon keys off the reason to fall back.
    /// PIN: the reason string equals DESCRIBE_IMAGE_UNAVAILABLE_REASON (the
    /// daemon<->server contract).
    #[test]
    fn describe_image_unavailable_response_carries_reason_no_text() {
        let resp = serde_json::from_str::<Response>(
            r#"{"id":"d-3","ok":false,"reason":"vlm_unavailable","error":"mlx-vlm is not installed","latency_ms":2}"#,
        )
        .unwrap();
        assert!(!resp.ok, "the unavailable path is ok:false");
        assert_eq!(resp.reason.as_deref(), Some(DESCRIBE_IMAGE_UNAVAILABLE_REASON));
        assert_eq!(resp.error.as_deref(), Some("mlx-vlm is not installed"));
        assert!(resp.text.is_none(), "the unavailable path NEVER carries a (fabricated) description");
    }

    /// The decode budget is HARD-CAPPED: the default is sane and the cap is a real
    /// ceiling the daemon clamps to, so a caller can never ask the on-device VLM
    /// for an unbounded decode (the client `min`s the request against the cap).
    #[test]
    fn describe_image_decode_budget_is_capped() {
        const { assert!(DESCRIBE_IMAGE_DEFAULT_MAX_TOKENS > 0) };
        const { assert!(DESCRIBE_IMAGE_DEFAULT_MAX_TOKENS <= DESCRIBE_IMAGE_MAX_TOKENS_CAP) };
        assert_eq!(DESCRIBE_IMAGE_MAX_TOKENS_CAP, 1024, "cap pinned to the server's contract");
        // The clamp the client applies: an over-budget ask collapses to the cap.
        let asked = 99_999u32;
        assert_eq!(asked.min(DESCRIBE_IMAGE_MAX_TOKENS_CAP), DESCRIBE_IMAGE_MAX_TOKENS_CAP);
    }

    // ----- generate_image (task #18) — on-device text->image wire contract ----

    /// generate_image wire contract — the request carries op="generate_image",
    /// the REQUIRED `prompt`, and the OPTIONAL `size`/`steps`/`seed`. Unrelated
    /// fields stay omitted so an old server (no image op) sees a clean shape it
    /// rejects as unknown (which the daemon reads as "image model unavailable").
    /// The prompt is handed ONLY to the on-device model; nothing here is a cloud
    /// call.
    #[test]
    fn generate_image_request_serializes_to_the_wire_shape() {
        let mut req = Request::new("g-1".to_string(), "generate_image");
        req.prompt = Some("a red bicycle on a beach at sunset");
        req.size = Some(512);
        req.steps = Some(4);
        req.seed = Some(42);
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["op"], "generate_image");
        assert_eq!(v["prompt"], "a red bicycle on a beach at sunset", "the prompt must reach the wire");
        assert_eq!(v["size"], 512);
        assert_eq!(v["steps"], 4);
        assert_eq!(v["seed"], 42);
        // None of the unrelated op fields leak in.
        for absent in ["text", "question", "voice", "voice_id", "backend", "el_key", "texts", "path"] {
            assert!(v.get(absent).is_none(), "{absent} must be omitted on the generate_image wire");
        }
    }

    /// generate_image wire contract — a BARE prompt (no size/steps/seed): those
    /// optional fields are OMITTED entirely (not null), so the server applies its
    /// own defaults (GENERATE_IMAGE_DEFAULT_SIZE/STEPS + a time-derived seed).
    #[test]
    fn generate_image_request_omits_optional_params_when_absent() {
        let mut req = Request::new("g-2".to_string(), "generate_image");
        req.prompt = Some("an astronaut riding a horse");
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["op"], "generate_image");
        assert_eq!(v["prompt"], "an astronaut riding a horse");
        for absent in ["size", "steps", "seed"] {
            assert!(v.get(absent).is_none(), "{absent} must be omitted when unset (server uses its default)");
        }
    }

    /// generate_image AVAILABLE response: ok:true carries the ON-DEVICE saved
    /// `path` plus NON-secret `model`/`size`/`steps`/`seed` metadata — never any
    /// pixels (the image lives on the machine). The daemon maps this to the saved
    /// on-device path it surfaces.
    #[test]
    fn generate_image_available_response_carries_path_and_metadata() {
        let resp = serde_json::from_str::<Response>(
            r#"{"id":"g-1","ok":true,"path":"/root/state/images/image-7.png","model":"schnell","size":512,"steps":4,"seed":42,"latency_ms":8000}"#,
        )
        .unwrap();
        assert!(resp.ok);
        assert_eq!(resp.path.as_deref(), Some("/root/state/images/image-7.png"));
        assert_eq!(resp.model.as_deref(), Some("schnell"));
        assert_eq!(resp.size, Some(512));
        assert_eq!(resp.steps, Some(4));
        assert_eq!(resp.seed, Some(42));
        assert!(resp.reason.is_none(), "the available path carries no unavailable reason");
    }

    /// generate_image UNAVAILABLE response: ok:false carries the stable
    /// `reason`="image_model_unavailable" + an honest `error` and NO path — NEVER
    /// a fabricated image, NEVER a cloud fallback. The daemon keys off the reason
    /// to surface the honest "not set up" line. PIN: the reason string equals
    /// GENERATE_IMAGE_UNAVAILABLE_REASON (the daemon<->server contract).
    #[test]
    fn generate_image_unavailable_response_carries_reason_no_path() {
        let resp = serde_json::from_str::<Response>(
            r#"{"id":"g-3","ok":false,"reason":"image_model_unavailable","error":"image model not available (on-device image generation is off or its model is not downloaded)","latency_ms":2}"#,
        )
        .unwrap();
        assert!(!resp.ok, "the unavailable path is ok:false");
        assert_eq!(resp.reason.as_deref(), Some(GENERATE_IMAGE_UNAVAILABLE_REASON));
        assert!(resp.error.as_deref().unwrap().contains("not available"));
        assert!(resp.path.is_none(), "the unavailable path NEVER carries a (fabricated) image path");
    }

    /// The size/steps bounds are real ceilings the daemon clamps to, so a caller
    /// can never push an out-of-range canvas or an unbounded sampler run at the
    /// on-device model (the client `clamp`s the request; the server clamps too).
    #[test]
    fn generate_image_size_and_steps_are_bounded() {
        assert_eq!(GENERATE_IMAGE_MIN_SIZE, 64, "min size pinned to the server's contract");
        assert_eq!(GENERATE_IMAGE_MAX_SIZE, 1536, "max size pinned to the server's contract");
        assert_eq!(GENERATE_IMAGE_DEFAULT_SIZE, 512, "default size pinned");
        const { assert!(GENERATE_IMAGE_DEFAULT_SIZE >= GENERATE_IMAGE_MIN_SIZE) };
        const { assert!(GENERATE_IMAGE_DEFAULT_SIZE <= GENERATE_IMAGE_MAX_SIZE) };
        assert_eq!(GENERATE_IMAGE_DEFAULT_STEPS, 4, "default steps pinned to the fast schnell budget");
        assert_eq!(GENERATE_IMAGE_MAX_STEPS_CAP, 50, "steps cap pinned to the server's contract");
        const { assert!(GENERATE_IMAGE_DEFAULT_STEPS <= GENERATE_IMAGE_MAX_STEPS_CAP) };
        // The clamps the client applies: an over/under-range ask collapses inward.
        assert_eq!(8.clamp(GENERATE_IMAGE_MIN_SIZE, GENERATE_IMAGE_MAX_SIZE), GENERATE_IMAGE_MIN_SIZE);
        assert_eq!(99_999u32.clamp(GENERATE_IMAGE_MIN_SIZE, GENERATE_IMAGE_MAX_SIZE), GENERATE_IMAGE_MAX_SIZE);
        assert_eq!(9_999u32.clamp(1, GENERATE_IMAGE_MAX_STEPS_CAP), GENERATE_IMAGE_MAX_STEPS_CAP);
    }

    // -----------------------------------------------------------------------
    // RELIABILITY: reconnect backoff + jitter + liveness/health (WS2).
    // All MOCK-based — no live inference server, no model. The live-socket
    // tests bind a temp UnixListener and drop it to simulate a down server.
    // -----------------------------------------------------------------------

    /// The backoff schedule is EXACTLY: attempt 0 immediate (zero added latency
    /// on the happy path), then 50ms, 100ms, 200ms, … doubling and saturating at
    /// the 400ms ceiling. This is the load-bearing property: a healthy server is
    /// as fast as before, and a flapping one is rate-limited, never hammered.
    #[test]
    fn backoff_schedule_is_immediate_then_bounded_exponential() {
        // Attempt 0 NEVER sleeps — the happy path pays nothing.
        assert_eq!(backoff_delay(0), Duration::ZERO, "first attempt must be immediate");
        // Then exponential from the base.
        assert_eq!(backoff_delay(1), RECONNECT_BASE_DELAY, "attempt 1 == base (50ms)");
        assert_eq!(backoff_delay(2), RECONNECT_BASE_DELAY * 2, "attempt 2 doubles");
        // Saturates at the ceiling — never grows unbounded.
        assert_eq!(backoff_delay(3), RECONNECT_BASE_DELAY * 4, "attempt 3 == 200ms (still under cap)");
        assert_eq!(backoff_delay(99), RECONNECT_MAX_DELAY, "a huge attempt saturates at the cap, never overflows");
        // Monotonic non-decreasing up to the cap.
        let mut prev = Duration::ZERO;
        for a in 0..20 {
            let d = backoff_delay(a);
            assert!(d >= prev, "schedule must be non-decreasing");
            assert!(d <= RECONNECT_MAX_DELAY, "schedule must never exceed the cap");
            prev = d;
        }
    }

    /// Total sleep across a full exhausted reconnect (the bounded worst case) is
    /// far below the 30s op timeout — a hard-down server adds sub-second latency,
    /// not a multi-timeout stall. Proves the gap assess flagged ("every op pays
    /// full 30s ceilings") is closed.
    #[test]
    fn total_backoff_across_all_attempts_is_well_under_the_op_timeout() {
        let mut total = Duration::ZERO;
        for a in 0..RECONNECT_MAX_ATTEMPTS {
            total += backoff_delay(a);
        }
        assert!(
            total < Duration::from_secs(2),
            "worst-case reconnect backoff ({total:?}) must be a small fraction of REQUEST_TIMEOUT (30s)"
        );
    }

    /// Jitter stays within +/-25% of the base, never goes negative, and a zero
    /// base (the immediate first attempt) yields exactly zero — so jitter can
    /// never accidentally introduce a sleep on the happy path. The spread across
    /// seeds proves independent clients won't reconnect in lockstep.
    #[test]
    fn jitter_is_bounded_and_zero_preserving() {
        // Zero base in -> zero out (no sleep on the immediate attempt).
        assert_eq!(jittered_delay(Duration::ZERO, 12345), Duration::ZERO);

        let base = Duration::from_millis(200);
        let span = base.as_millis() as u64 / 4; // 50ms
        let lo = base.as_millis() as u64 - span;
        let hi = base.as_millis() as u64 + span;
        let mut saw_low = false;
        let mut saw_high = false;
        for seed in 0..1000u64 {
            let j = jittered_delay(base, seed).as_millis() as u64;
            assert!(j >= lo && j <= hi, "jitter {j}ms out of [{lo},{hi}] for seed {seed}");
            if j < base.as_millis() as u64 {
                saw_low = true;
            }
            if j > base.as_millis() as u64 {
                saw_high = true;
            }
        }
        assert!(saw_low && saw_high, "jitter must spread both below and above the base across seeds");
    }

    /// LIVE-SOCKET MOCK: a connect-probe against a bound temp UnixListener
    /// succeeds, and against a path with NO listener it fails fast (well within
    /// the connect timeout) — never hangs. This is the liveness primitive the
    /// background health task and --selftest board rely on; no model is spent.
    #[tokio::test]
    async fn probe_reachable_is_true_when_bound_false_when_absent() {
        // /tmp (not the long /var/folders temp_dir) keeps the sockaddr_un path
        // under macOS's ~104-byte SUN_LEN cap.
        let dir = std::path::PathBuf::from("/tmp").join(format!("jv-probe-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sock = dir.join("inference.sock");
        let _ = std::fs::remove_file(&sock);

        // No listener yet -> unreachable, and it returns quickly (fail-fast).
        let client = InferenceClient::new(sock.clone());
        let start = std::time::Instant::now();
        assert!(client.probe_reachable().await.is_err(), "absent socket must probe unreachable");
        assert!(start.elapsed() < CONNECT_TIMEOUT + Duration::from_millis(500), "probe must fail fast, not hang");

        // Bind a listener -> reachable.
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind temp inference socket");
        assert!(client.probe_reachable().await.is_ok(), "bound socket must probe reachable");
        drop(listener);

        // After the listener is gone -> unreachable again (honest, point-in-time).
        let _ = std::fs::remove_file(&sock);
        assert!(client.probe_reachable().await.is_err(), "removed socket must probe unreachable again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// LIVE-SOCKET MOCK: connect_with_backoff RECOVERS a server that was down at
    /// the start of the op and came up during the backoff window — the exact
    /// "inference server restarted between turns" scenario. With NO listener it
    /// exhausts the bounded attempts and returns an HONEST Err (never a fake
    /// success); once a listener is bound it connects on a subsequent attempt.
    #[tokio::test]
    async fn connect_with_backoff_recovers_a_server_that_comes_up_late() {
        // /tmp keeps the sockaddr_un path under macOS's ~104-byte SUN_LEN cap.
        let dir = std::path::PathBuf::from("/tmp").join(format!("jv-recon-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sock = dir.join("inference.sock");
        let _ = std::fs::remove_file(&sock);

        // Hard-down: no listener at all -> exhausts attempts -> honest Err.
        let mut client = InferenceClient::new(sock.clone());
        let res = client.connect_with_backoff().await;
        assert!(res.is_err(), "a hard-down server must surface an honest Err, never a fake connect");
        assert!(client.conn.is_none(), "no live connection after exhaustion");

        // Server comes up mid-op: bind in a task after a short delay, then call
        // connect_with_backoff — the backoff schedule gives it room to land.
        let sock2 = sock.clone();
        let binder = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            let l = tokio::net::UnixListener::bind(&sock2).expect("bind late");
            // Hold the listener open long enough for the client to connect.
            tokio::time::sleep(Duration::from_millis(300)).await;
            drop(l);
        });
        let res = client.connect_with_backoff().await;
        assert!(res.is_ok(), "a server that comes up during the backoff window must be recovered");
        assert!(client.conn.is_some(), "a live connection is established after recovery");
        let _ = binder.await;
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The shared health state transitions honestly: starts UNKNOWN (probed
    /// false), a success marks reachable + stamps last_ok, failures count up,
    /// and a recovery resets the counter — and record_probe returns the PRIOR
    /// reachable so the liveness task can edge-trigger degraded/recovered once.
    #[test]
    fn health_state_records_probes_and_reports_edges_honestly() {
        reset_health_for_test();
        // First failure from the initial state: prior reachable is false.
        let prev = record_probe(false, 100);
        assert!(!prev, "initial reachable is false (unknown-at-boot is not 'up')");
        let s = super::health_snapshot();
        assert!(s.probed, "a completed probe marks probed=true (no longer unknown)");
        assert!(!s.reachable);
        assert_eq!(s.consecutive_failures, 1);
        assert_eq!(s.last_ok_unix, None, "never reachable yet -> no last_ok, not a fake 0");

        // Second failure accumulates.
        record_probe(false, 200);
        assert_eq!(super::health_snapshot().consecutive_failures, 2);

        // Recovery: prior was DOWN, so this is the up edge.
        let prev = record_probe(true, 300);
        assert!(!prev, "the recovery edge sees prior reachable=false");
        let s = super::health_snapshot();
        assert!(s.reachable);
        assert_eq!(s.consecutive_failures, 0, "recovery clears the failure count");
        assert_eq!(s.last_ok_unix, Some(300));

        // Staying up: prior reachable=true (no edge).
        let prev = record_probe(true, 400);
        assert!(prev, "a steady-up tick sees prior reachable=true (no recovered edge)");
        assert_eq!(super::health_snapshot().last_ok_unix, Some(400));

        // Going down: prior reachable=true (the degraded edge).
        let prev = record_probe(false, 500);
        assert!(prev, "the degraded edge sees prior reachable=true");
        let s = super::health_snapshot();
        assert!(!s.reachable);
        assert_eq!(s.consecutive_failures, 1);
        assert_eq!(s.last_ok_unix, Some(400), "last_ok is preserved across a new outage");
        reset_health_for_test();
    }
}

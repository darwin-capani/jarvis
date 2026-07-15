//! On-device speaker identification — a LIGHTWEIGHT ACOUSTIC owner model.
//!
//! ## What this is — and the honesty that governs every word of copy
//! This module computes a deterministic, self-contained acoustic embedding of
//! an utterance and compares it (cosine) against a LOCALLY-enrolled owner
//! profile. It is the SHIPPED [`AcousticEmbedder`]: pre-emphasis -> Hann-windowed
//! short frames -> a mel-spaced log filterbank (a small hand-rolled real-DFT per
//! band, NO model download) -> per-band temporal mean+std -> an L2-normalized
//! fixed-dimension vector. It needs NO network, NO MLX, NO neural model, and adds
//! NO heavy dependency (only serde/serde_json/sha2, already in the tree).
//!
//! ### It is NOT a high-assurance biometric. Be ruthless about this.
//! A filterbank-statistics + cosine speaker model RAISES THE BAR — an obviously
//! different voice (different timbre/formants) is rejected — but it is:
//!   * SPOOFABLE by a recording replayed near the mic, or by a good vocal
//!     impersonation. It models gross spectral shape, not liveness.
//!   * THRESHOLD- and VOICE-dependent: the false-accept / false-reject rates are
//!     a function of the chosen `threshold` and of how similar two real voices
//!     are. They are DEVICE-gated — measurable only on the actual mic, never here.
//!     The hermetic tests in this module prove only the SYNTHETIC-SIGNAL SEPARATION
//!     (a deterministic "owner" tone-complex verifies; a clearly-different "intruder"
//!     is rejected), determinism, L2-normalization, and cosine correctness. They make
//!     NO accuracy claim about real voices, and the copy must never imply one.
//!
//! ### Where it sits in the safety stack — an ADDED layer, never a replacement
//! Voice-ID is an ADDITIVE factor on top of the existing backstops, never a
//! substitute for them:
//!   * The armed-by-default `[integrations].allow_consequential` master switch (ON,
//!     but a confirmed action still needs a fresh per-action confirm) and
//!   * the cross-turn SPOKEN confirmation gate ([`crate::confirm`])
//!     remain the hard security backstop for outward actions. Voice-ID, when enabled
//!     AND a profile is enrolled, adds: an unrecognized speaker may not trigger a
//!     consequential/outward action, and may not confirm (replay) a parked action a
//!     bystander could otherwise approve. It FAILS CLOSED for consequential actions
//!     (no usable audio / embed error while enabled+enrolled => treat as UNVERIFIED,
//!     deny the consequential path) but never bricks ordinary replies.
//!
//! ### Ships OFF + reversible
//! `[voice_id].enabled` defaults FALSE. With voice-id OFF, OR with no enrolled
//! profile, behavior is UNCHANGED from today — `owner_verified` is not enforced
//! anywhere. Enrollment is ALWAYS explicit ("enroll my voice"); never automatic.
//! Raw audio is NEVER persisted: the owner profile is a LOCAL feature VECTOR only
//! (state/voiceid/owner.json, 0600), never logged, never uploaded.
//!
//! ### A seam for a future neural embedder (NOT built now)
//! [`SpeakerEmbedder`] mirrors `recall::EmbeddingProvider`: today the only
//! implementation is [`AcousticEmbedder`]. A future neural speaker-verification
//! embedder (e.g. via the MLX inference server's hidden states) would implement
//! the SAME trait, and [`OwnerProfile`] / verify would not change. That work is
//! deliberately deferred — it is device/MLX-gated and untestable here.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::json;

/// Dimensionality of the mel filterbank: number of log-energy bands per frame.
/// 24 mel bands is the classic speaker-features count — enough spectral
/// resolution to separate voices, small enough that a short hand-rolled DFT per
/// band stays cheap and deterministic.
const N_MEL: usize = 24;

/// Analysis frame length and hop in milliseconds. 25 ms / 10 ms is the textbook
/// speech-analysis framing (each frame is quasi-stationary; 60% overlap keeps
/// temporal statistics smooth).
const FRAME_MS: f64 = 25.0;
const HOP_MS: f64 = 10.0;

/// Pre-emphasis coefficient: a first-order high-pass `y[n] = x[n] - a*x[n-1]`
/// that flattens the spectral tilt of voiced speech (boosts the formant-bearing
/// high band). 0.97 is the standard value.
const PRE_EMPHASIS: f64 = 0.97;

/// Mel filterbank frequency span. Voiced speech energy that distinguishes
/// speakers lives roughly 80 Hz - 8 kHz; we clamp the top to Nyquist per the
/// actual sample rate so a low-rate buffer never indexes past its spectrum.
const MEL_LOW_HZ: f64 = 80.0;
const MEL_HIGH_HZ: f64 = 8000.0;

/// The embedding dimension: each of `N_MEL` bands contributes a temporal MEAN
/// and a temporal STD across the utterance's frames, so the vector is `2*N_MEL`.
pub const EMBED_DIM: usize = 2 * N_MEL;

// ---------------------------------------------------------------------------
// The embedder trait + the shipped acoustic implementation
// ---------------------------------------------------------------------------

/// A pluggable speaker embedder. Mirrors `recall::EmbeddingProvider`: today the
/// only implementation is [`AcousticEmbedder`] (filterbank statistics). A future
/// neural speaker-verification embedder would implement this same trait, and the
/// [`OwnerProfile`] enroll/verify logic above it would not change. The honest
/// name it reports drives any user-facing "how recognition works" copy.
///
/// The LIVE path calls the free [`embed`] function directly (it has no embedder
/// to thread); this trait is the documented SEAM a future neural embedder plugs
/// into, exercised by the module's own tests — hence allowed dead in the live
/// build, exactly like `recall::EmbeddingProvider`'s future-neural seam.
#[allow(dead_code)]
pub trait SpeakerEmbedder {
    /// Embed an utterance's mono samples (any sample rate) into a fixed-dim,
    /// L2-normalized feature vector. Returns `None` only when there is no usable
    /// audio to embed (empty / all-silence / too short for a single frame) — the
    /// caller treats that as UNVERIFIED (fail-closed) for consequential actions.
    /// Pure and DETERMINISTIC: the same buffer always yields the same vector.
    fn embed(&self, samples: &[f32], sample_rate: u32) -> Option<Vec<f32>>;

    /// A short, stable token naming the mechanism — for honest status/telemetry.
    fn method(&self) -> &'static str;
}

/// The SHIPPED speaker embedder: a self-contained acoustic feature extractor.
/// Pre-emphasis -> Hann-windowed 25ms/10ms frames -> mel-spaced log filterbank
/// (a small real-DFT per mel band, no FFT crate, no model download) -> per-band
/// temporal mean+std -> L2-normalized `2*N_MEL`-vector. Deterministic. This is a
/// LIGHTWEIGHT acoustic model, NOT a neural speaker-verification net — it says so
/// via [`SpeakerEmbedder::method`].
#[derive(Debug, Clone, Copy, Default)]
pub struct AcousticEmbedder;

impl SpeakerEmbedder for AcousticEmbedder {
    fn embed(&self, samples: &[f32], sample_rate: u32) -> Option<Vec<f32>> {
        embed(samples, sample_rate)
    }

    fn method(&self) -> &'static str {
        "acoustic-filterbank-stats"
    }
}

/// Compute the deterministic acoustic embedding of `samples` at `sample_rate`.
/// Returns `None` when there is no usable audio (no full frame, or pure silence
/// so every band is zero) — the caller treats that as UNVERIFIED for the
/// consequential path (fail-closed). The free function backs [`AcousticEmbedder`]
/// and is what the tests exercise directly.
pub fn embed(samples: &[f32], sample_rate: u32) -> Option<Vec<f32>> {
    if sample_rate == 0 {
        return None;
    }
    let sr = sample_rate as f64;
    let frame_len = ((FRAME_MS / 1000.0) * sr).round() as usize;
    let hop = ((HOP_MS / 1000.0) * sr).round().max(1.0) as usize;
    if frame_len == 0 || samples.len() < frame_len {
        // Too short for even one analysis frame — no usable audio.
        return None;
    }

    // SILENCE GUARD (fail-closed): a buffer with essentially no acoustic energy
    // (digital silence, a DC offset, a dropout) carries no speaker identity — its
    // "embedding" would be pure quantization/floor noise. Reject it as no usable
    // audio so the consequential path treats the turn as UNVERIFIED rather than
    // matching a meaningless vector. The floor is well below any real speech RMS.
    let rms = (samples.iter().map(|s| f64::from(*s) * f64::from(*s)).sum::<f64>()
        / samples.len() as f64)
        .sqrt();
    if !rms.is_finite() || rms < 1e-4 {
        return None;
    }

    // Pre-emphasis high-pass over the whole signal (in f64 for determinism).
    let mut emph = vec![0.0f64; samples.len()];
    emph[0] = f64::from(samples[0]);
    for n in 1..samples.len() {
        emph[n] = f64::from(samples[n]) - PRE_EMPHASIS * f64::from(samples[n - 1]);
    }

    // Precompute the Hann window and the mel filterbank's per-band analysis
    // frequencies once (cheap; depends only on frame_len + sample_rate).
    let window = hann_window(frame_len);
    let nyquist = sr / 2.0;
    let high = MEL_HIGH_HZ.min(nyquist * 0.999); // never index at/above Nyquist
    let centers = mel_band_center_freqs(MEL_LOW_HZ, high, N_MEL);

    // For each frame: window it, compute per-mel-band energy via a small DFT at
    // each band's center frequency (Goertzel-style real/imag accumulation), take
    // log. Accumulate running mean/var per band across frames (Welford), so the
    // temporal statistics need no second pass and stay deterministic.
    let mut acc = vec![BandAcc::default(); N_MEL];
    let mut start = 0usize;
    let mut n_frames = 0usize;
    while start + frame_len <= emph.len() {
        let frame = &emph[start..start + frame_len];
        for (b, &fc) in centers.iter().enumerate() {
            let energy = band_energy(frame, &window, fc, sr);
            // log compression with a floor so silence -> a finite, stable value
            // (never -inf), keeping the embedding finite and comparable.
            let log_e = (energy + 1e-10).ln();
            acc[b].push(log_e);
        }
        n_frames += 1;
        start += hop;
    }
    if n_frames == 0 {
        return None;
    }

    // CEPSTRAL-MEAN NORMALIZATION (CMN) of the mean-log-energy band block. The
    // ABSOLUTE log-energies share a large baseline (the log-floor offset + the
    // overall recording level + the gross spectral tilt) that is COMMON to every
    // voice; left in, it dominates the L2-normalized vector and collapses the
    // cosine between any two utterances toward 1. Subtracting the per-utterance
    // mean across bands removes that shared baseline, so what survives is the
    // RELATIVE spectral SHAPE — which formant bands a given voice emphasizes —
    // i.e. the part that actually distinguishes speakers. (CMN is the standard
    // channel/level-robustness step in speech/speaker features.)
    let band_means: Vec<f64> = acc.iter().map(|a| a.mean()).collect();
    let cmn_offset = band_means.iter().sum::<f64>() / band_means.len() as f64;

    // Build the feature vector: [CMN'd mean_0..mean_{N-1}, std_0..std_{N-1}].
    // The temporal STDs are already baseline-free (a spread, not a level), so they
    // are used as-is — they capture how dynamic each band is across the utterance.
    let mut feat = Vec::with_capacity(EMBED_DIM);
    for &m in &band_means {
        feat.push(m - cmn_offset);
    }
    for a in &acc {
        feat.push(a.std());
    }

    // L2-normalize so cosine similarity is a clean dot product and amplitude
    // (how loud the utterance was) never affects identity.
    let norm = feat.iter().map(|v| v * v).sum::<f64>().sqrt();
    if !(norm.is_finite()) || norm <= 1e-9 {
        // Degenerate (flat / zero) feature — treat as no usable audio.
        return None;
    }
    Some(feat.iter().map(|v| (v / norm) as f32).collect())
}

/// Welford running mean/variance for one mel band across an utterance's frames.
#[derive(Debug, Clone, Copy, Default)]
struct BandAcc {
    n: u64,
    mean: f64,
    m2: f64,
}

impl BandAcc {
    fn push(&mut self, x: f64) {
        self.n += 1;
        let delta = x - self.mean;
        self.mean += delta / self.n as f64;
        let delta2 = x - self.mean;
        self.m2 += delta * delta2;
    }
    fn mean(&self) -> f64 {
        self.mean
    }
    fn std(&self) -> f64 {
        if self.n < 2 {
            0.0
        } else {
            (self.m2 / self.n as f64).sqrt()
        }
    }
}

/// The Hann window of length `n`: `0.5 - 0.5*cos(2*pi*i/(n-1))`. Reduces spectral
/// leakage in the per-band DFT. Deterministic; precomputed once per call.
fn hann_window(n: usize) -> Vec<f64> {
    if n <= 1 {
        return vec![1.0; n.max(1)];
    }
    (0..n)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (n as f64 - 1.0)).cos())
        .collect()
}

/// Convert a frequency in Hz to the mel scale (HTK formula).
fn hz_to_mel(hz: f64) -> f64 {
    2595.0 * (1.0 + hz / 700.0).log10()
}

/// Convert a mel value back to Hz.
fn mel_to_hz(mel: f64) -> f64 {
    700.0 * (10f64.powf(mel / 2595.0) - 1.0)
}

/// `n` mel-spaced center frequencies between `low_hz` and `high_hz` (inclusive
/// span), equally spaced on the mel scale. These are the analysis frequencies
/// the per-band DFT evaluates — a single-point filterbank, which is enough to
/// capture the gross spectral envelope a lightweight speaker model needs.
fn mel_band_center_freqs(low_hz: f64, high_hz: f64, n: usize) -> Vec<f64> {
    let low_mel = hz_to_mel(low_hz);
    let high_mel = hz_to_mel(high_hz);
    (0..n)
        .map(|i| {
            let mel = low_mel + (high_mel - low_mel) * (i as f64 + 0.5) / n as f64;
            mel_to_hz(mel)
        })
        .collect()
}

/// Energy of a windowed frame at a single analysis frequency `fc` (Hz), via a
/// direct real-DFT bin (Goertzel-equivalent single-frequency evaluation): sum
/// the windowed samples against cos/sin at `fc`, return the squared magnitude
/// normalized by the frame length. No FFT crate; O(frame_len) and deterministic.
fn band_energy(frame: &[f64], window: &[f64], fc: f64, sample_rate: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * fc / sample_rate;
    let mut re = 0.0f64;
    let mut im = 0.0f64;
    for (i, (&x, &win)) in frame.iter().zip(window.iter()).enumerate() {
        let phase = w * i as f64;
        let s = x * win;
        re += s * phase.cos();
        im += s * phase.sin();
    }
    let n = frame.len() as f64;
    (re * re + im * im) / (n * n)
}

/// Cosine similarity between two equal-length vectors, in `[-1.0, 1.0]`. Returns
/// 0.0 for a length mismatch or a zero-norm input (no meaningful angle). The
/// embeddings are already L2-normalized, so this is effectively their dot
/// product, but we normalize defensively so a centroid (an averaged, possibly
/// non-unit vector) compares correctly too.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    for (&x, &y) in a.iter().zip(b.iter()) {
        let (x, y) = (f64::from(x), f64::from(y));
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na <= 1e-12 || nb <= 1e-12 {
        return 0.0;
    }
    (dot / (na.sqrt() * nb.sqrt())).clamp(-1.0, 1.0)
}

// ---------------------------------------------------------------------------
// The owner profile: enroll / verify / persist (LOCAL vector only)
// ---------------------------------------------------------------------------

/// The outcome of verifying one utterance embedding against the owner profile:
/// whether it cleared the threshold, and the actual cosine score (the max over
/// the enrolled centroids). The score is for telemetry/UI only — never the audio
/// or the embedding.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VerifyOutcome {
    pub verified: bool,
    pub score: f64,
}

/// The LOCALLY-enrolled owner profile: a small set of enrolled embedding
/// CENTROIDS (the owner's voice captured over several utterances) plus the accept
/// `threshold`. This is a feature VECTOR collection — NEVER raw audio. It is the
/// only thing persisted, never logged, never uploaded.
///
/// Verify = the MAX cosine of the candidate embedding to ANY enrolled centroid
/// `>= threshold`. Multiple centroids let the profile cover natural variation
/// (different phrases / mic distances) without a single averaged vector smearing
/// the owner's identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OwnerProfile {
    /// Enrolled owner embeddings (each already L2-normalized at enroll time).
    pub centroids: Vec<Vec<f32>>,
    /// Accept threshold on the max cosine. Sensible default in [`DEFAULT_THRESHOLD`].
    pub threshold: f64,
    /// How many utterances have been enrolled (== `centroids.len()`); carried
    /// explicitly so the JSON is self-describing and a future centroid-merging
    /// change can keep an honest count.
    pub n_samples: usize,
}

/// Default accept threshold. A cosine over the L2-normalized acoustic features:
/// the SAME voice across utterances scores high (the synthetic separation test
/// pins owner >> intruder), a clearly-different voice scores low. This is a
/// STARTING point — the real operating point is device-tuned via
/// `[voice_id].threshold`; do not read it as a measured FAR/FRR. The config's
/// `VoiceIdConfig::default().threshold` mirrors this value (single source of
/// truth); the constant is also the default the tests build profiles with.
#[allow(dead_code)] // mirrored by the config default; used by the module tests
pub const DEFAULT_THRESHOLD: f64 = 0.86;

impl OwnerProfile {
    /// A fresh, empty profile with the given accept threshold and no enrolled
    /// samples. `verify` on an empty profile is always `false` with score 0.0 —
    /// the caller treats "no profile / no centroids" as NOT-enrolled (unchanged
    /// behavior; no gating).
    pub fn new(threshold: f64) -> Self {
        Self {
            centroids: Vec::new(),
            threshold,
            n_samples: 0,
        }
    }

    /// Whether any voice is enrolled. With NO centroids the profile never gates
    /// anything (the OFF/unenrolled = unchanged-behavior contract).
    pub fn is_enrolled(&self) -> bool {
        !self.centroids.is_empty()
    }

    /// Enroll one utterance's embedding as a new centroid. The embedding is
    /// stored verbatim (it is already L2-normalized by [`embed`]). Never
    /// automatic — only the explicit enroll intent calls this.
    pub fn enroll(&mut self, embedding: Vec<f32>) {
        self.centroids.push(embedding);
        self.n_samples = self.centroids.len();
    }

    /// Verify a candidate embedding: `verified` iff the MAX cosine to any
    /// enrolled centroid `>= threshold`; `score` is that max (0.0 when nothing is
    /// enrolled). Pure — the gate threads the result; nothing here speaks or acts.
    pub fn verify(&self, embedding: &[f32]) -> VerifyOutcome {
        let mut best = 0.0f64;
        for c in &self.centroids {
            let s = cosine_similarity(embedding, c);
            if s > best {
                best = s;
            }
        }
        VerifyOutcome {
            verified: self.is_enrolled() && best >= self.threshold,
            score: best,
        }
    }

    /// Clear the profile ("forget my voice"): drop every centroid, keep the
    /// configured threshold. After this `is_enrolled()` is false and no turn is
    /// ever gated by voice-id again until re-enrollment. The live forget path
    /// drops the whole `Option<OwnerProfile>` + deletes the file; this in-place
    /// clear is the API a caller holding a profile by value uses (and the tests).
    #[allow(dead_code)] // public API + tested; the live path drops the Option
    pub fn forget(&mut self) {
        self.centroids.clear();
        self.n_samples = 0;
    }
}

/// The on-disk path of the owner profile: `<root>/state/voiceid/owner.json`. The
/// VECTOR only — never audio. Restrictive perms (0600 file, 0700 dir) are applied
/// on save.
pub fn profile_path(root: &Path) -> PathBuf {
    root.join("state").join("voiceid").join("owner.json")
}

/// Load the owner profile from disk, or `None` when no profile exists yet (the
/// unenrolled state — voice-id gates nothing). A malformed file is treated as
/// "no profile" (logged by the caller) rather than wedging the daemon.
pub fn load_profile(root: &Path) -> Option<OwnerProfile> {
    let path = profile_path(root);
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice::<OwnerProfile>(&bytes).ok()
}

/// Persist the owner profile LOCALLY (vector only) with restrictive perms. The
/// directory is created 0700 and the file 0600 — best-effort tightening (the
/// real protection is that no raw audio is ever written and the file is never
/// logged/uploaded). Never serializes anything but the centroid vectors,
/// threshold, and count.
pub fn save_profile(root: &Path, profile: &OwnerProfile) -> std::io::Result<()> {
    let path = profile_path(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        set_mode(parent, 0o700);
    }
    let bytes = serde_json::to_vec_pretty(profile)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&path, bytes)?;
    set_mode(&path, 0o600);
    Ok(())
}

/// Delete the owner profile file entirely ("forget my voice" persistence). A
/// missing file is success (already forgotten).
pub fn delete_profile(root: &Path) -> std::io::Result<()> {
    let path = profile_path(root);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// ENCRYPTED VAULT — the at-rest wrapper for the owner profile (#11)
// ---------------------------------------------------------------------------
//
// SQLCipher's `PRAGMA key` encrypts whole SQLite FILES; the owner profile is a
// JSON file, NOT SQLite, so it is the ONE sensitive store the DB-level encryption
// does not cover. When `[security].encrypt_memory` is ON, the profile JSON is held
// in its OWN encrypted SQLCipher DB at `<root>/state/voiceid/owner.enc.db`
// (a single-row blob), keyed by the same master key as the DB stores. With
// encryption OFF the plaintext `owner.json` path above is used, unchanged.

/// The on-disk path of the ENCRYPTED owner vault (a SQLCipher DB holding the
/// profile JSON as a blob). Distinct from the plaintext `owner.json` so the two
/// modes never collide and `migrate_profile_to_vault` can read one and write the
/// other.
pub fn vault_path(root: &Path) -> PathBuf {
    root.join("state").join("voiceid").join("owner.enc.db")
}

/// Open (creating if needed) the encrypted vault DB at `path` with the master
/// `key` applied via SQLCipher `PRAGMA key`, and ensure its single-row schema.
fn open_vault(path: &Path, key: &crate::crypto::SecretKey) -> std::io::Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open(path)
        .map_err(std::io::Error::other)?;
    crate::crypto::apply_key(&conn, key)
        .map_err(std::io::Error::other)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS owner(id INTEGER PRIMARY KEY CHECK(id=1), profile_json TEXT NOT NULL);",
    )
    .map_err(std::io::Error::other)?;
    Ok(conn)
}

/// Load the owner profile from the ENCRYPTED vault, or `None` when no profile is
/// stored yet (or the row/blob is malformed — treated as "no profile", never a
/// daemon-wedging error). The vault file is unreadable without `key`. The live
/// verify path reads through this when `[security].encrypt_memory` is ON (the
/// runtime read seam that pairs with `migrate_profile_to_vault`); tested here.
pub fn load_profile_encrypted(root: &Path, key: &crate::crypto::SecretKey) -> Option<OwnerProfile> {
    let path = vault_path(root);
    if !path.exists() {
        return None;
    }
    let conn = open_vault(&path, key).ok()?;
    let json: String = conn
        .query_row("SELECT profile_json FROM owner WHERE id=1", [], |r| r.get(0))
        .ok()?;
    serde_json::from_str::<OwnerProfile>(&json).ok()
}

/// Persist the owner profile into the ENCRYPTED vault (vector only, as JSON inside
/// a SQLCipher blob). Restrictive perms (0700 dir, 0600 file) are still applied as
/// defense in depth; the real at-rest protection is the encryption. Never
/// serializes anything but the centroid vectors, threshold, and count.
pub fn save_profile_encrypted(
    root: &Path,
    profile: &OwnerProfile,
    key: &crate::crypto::SecretKey,
) -> std::io::Result<()> {
    let path = vault_path(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        set_mode(parent, 0o700);
    }
    let json = serde_json::to_string(profile)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let conn = open_vault(&path, key)?;
    conn.execute(
        "INSERT INTO owner(id, profile_json) VALUES (1, ?1)
         ON CONFLICT(id) DO UPDATE SET profile_json = excluded.profile_json",
        rusqlite::params![json],
    )
    .map_err(std::io::Error::other)?;
    drop(conn);
    set_mode(&path, 0o600);
    Ok(())
}

/// Delete the encrypted vault ("forget my voice" under encryption). Missing is
/// success (already forgotten). The forget path calls this when encryption is ON
/// (the encrypted counterpart of `delete_profile`); tested here.
pub fn delete_vault(root: &Path) -> std::io::Result<()> {
    let base = vault_path(root);
    for suffix in ["", "-wal", "-shm"] {
        let mut p = base.clone().into_os_string();
        p.push(suffix);
        match std::fs::remove_file(PathBuf::from(p)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// MIGRATION on enable: move an existing PLAINTEXT `owner.json` into the encrypted
/// vault, then delete the plaintext file. A no-op success when no plaintext
/// profile exists (the honest fresh-start — the vault is created on first save).
pub fn migrate_profile_to_vault(root: &Path, key: &crate::crypto::SecretKey) -> std::io::Result<()> {
    match load_profile(root) {
        Some(profile) => {
            save_profile_encrypted(root, &profile, key)?;
            delete_profile(root)?;
            Ok(())
        }
        None => Ok(()),
    }
}

/// chmod best-effort (Unix). A failed tightening is defense-in-depth, not
/// load-bearing — the real protection is that the file holds only a feature
/// vector. Mirrors `genproxy::set_mode` / `command::set_mode`.
#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) {}

// ---------------------------------------------------------------------------
// Enrollment state machine (hermetically driven by synthetic buffers)
// ---------------------------------------------------------------------------

/// A small, explicit enrollment session: it accumulates the owner's utterance
/// embeddings until `min_samples` have been captured, then finalizes into an
/// [`OwnerProfile`]. Never automatic — the enroll intent starts one, each
/// subsequent owner utterance feeds it, and it is hermetically driven by
/// synthetic buffers in the tests (no live mic here). The state machine is pure:
/// it holds embeddings, never audio.
#[derive(Debug, Clone)]
pub struct Enrollment {
    captured: Vec<Vec<f32>>,
    min_samples: usize,
    threshold: f64,
}

/// The result of feeding one utterance to an in-progress [`Enrollment`].
#[derive(Debug, Clone, PartialEq)]
pub enum EnrollStep {
    /// Captured this sample; `need` more before the profile is complete.
    Progress { captured: usize, need: usize },
    /// Reached `min_samples`: here is the finished profile to persist.
    Complete(OwnerProfile),
}

impl Enrollment {
    /// Begin an enrollment that needs `min_samples` utterances (>=1) and bakes
    /// `threshold` into the finished profile.
    pub fn begin(min_samples: usize, threshold: f64) -> Self {
        Self {
            captured: Vec::new(),
            min_samples: min_samples.max(1),
            threshold,
        }
    }

    /// Feed one captured utterance embedding. Returns [`EnrollStep::Progress`]
    /// until `min_samples` are in, then [`EnrollStep::Complete`] with the
    /// finished profile (the caller persists it). After Complete the session is
    /// done; further feeds keep returning a Complete snapshot (idempotent).
    pub fn feed(&mut self, embedding: Vec<f32>) -> EnrollStep {
        self.captured.push(embedding);
        if self.captured.len() >= self.min_samples {
            let mut profile = OwnerProfile::new(self.threshold);
            for e in &self.captured {
                profile.enroll(e.clone());
            }
            EnrollStep::Complete(profile)
        } else {
            EnrollStep::Progress {
                captured: self.captured.len(),
                need: self.min_samples - self.captured.len(),
            }
        }
    }
}

/// A SECRET-FREE telemetry payload for a `voiceid.verify` event. Carries ONLY
/// the verdict, the score, whether the subsystem is enabled, and whether a
/// profile is enrolled — NEVER the embedding or any audio. The caller emits this
/// each turn so the HUD can show OFF / enrolled / verified / unrecognized.
pub fn verify_telemetry(outcome: VerifyOutcome, enabled: bool, enrolled: bool) -> serde_json::Value {
    json!({
        "verified": outcome.verified,
        "score": (outcome.score * 1000.0).round() / 1000.0,
        "enabled": enabled,
        "enrolled": enrolled,
    })
}

// ---------------------------------------------------------------------------
// The per-turn owner gate — how `owner_verified` threads into the deep
// `execute_tool` / `replay_confirmed_action` call sites without parameter
// threading, EXACTLY mirroring how `integrations::consequential_allowed()` is
// read deep in the tool loop.
// ---------------------------------------------------------------------------

/// How much voice-id gates when enabled+enrolled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateScope {
    /// Gate CONSEQUENTIAL/outward actions + confirmation replay only (the
    /// default). Non-consequential replies are never blocked by voice-id.
    Consequential,
    /// Additionally gate EVERY command (a stricter posture): an unrecognized
    /// speaker gets no action at all, consequential or not.
    All,
}

impl GateScope {
    /// Parse the config string. Unknown values fall back to the safe DEFAULT
    /// (`Consequential`) — never silently to `All` (which could surprise-block
    /// ordinary replies) nor to "off".
    pub fn from_config(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "all" => GateScope::All,
            _ => GateScope::Consequential,
        }
    }
}

/// The per-turn owner-gate state, computed once at the top of `run_pipeline`
/// from `[voice_id]` + the loaded profile + this turn's verification, then
/// consulted (cheaply, process-globally) wherever a consequential action or a
/// confirmation replay is about to fire. Mirrors the `consequential_allowed()`
/// global so the decision is read deep in the call stack without threading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnerGate {
    /// Is the voice-id subsystem ENFORCING this turn? True only when
    /// `[voice_id].enabled` AND a profile is enrolled. False => UNCHANGED
    /// behavior (no gating), exactly as today.
    pub enforcing: bool,
    /// Did THIS turn's speaker verify as the owner? Meaningful only when
    /// `enforcing`. On a fail-closed turn (embed error / no usable audio while
    /// enforcing) this is FALSE — an unverified speaker.
    pub verified: bool,
    /// What to gate when enforcing.
    pub scope: GateScope,
}

impl OwnerGate {
    /// The OFF / unenrolled gate: enforces nothing. This is the value installed
    /// when voice-id is disabled or no profile exists — every `allow_*` below is
    /// true, so behavior is byte-for-byte today's.
    pub const OFF: OwnerGate = OwnerGate {
        enforcing: false,
        verified: false,
        scope: GateScope::Consequential,
    };

    /// May a CONSEQUENTIAL / outward action fire this turn under voice-id? When
    /// not enforcing -> always yes (unchanged). When enforcing -> only if the
    /// speaker verified. This is ADDITIVE: the master switch + the confirmation
    /// gate still apply independently; voice-id can only ever DENY, never permit
    /// something those would block.
    pub fn allow_consequential(&self) -> bool {
        !self.enforcing || self.verified
    }

    /// May a parked confirmation be REPLAYED (a spoken/by-id "yes") this turn?
    /// Same rule as consequential: a bystander whose voice doesn't verify can
    /// never approve the owner's parked action when enforcing.
    pub fn allow_confirm_replay(&self) -> bool {
        !self.enforcing || self.verified
    }

    /// May a NON-consequential command run this turn? Always yes UNLESS the scope
    /// is `All` and we are enforcing and the speaker didn't verify. Under the
    /// default `Consequential` scope this is always true — ordinary replies are
    /// never blocked by voice-id.
    pub fn allow_noncly(&self) -> bool {
        match self.scope {
            GateScope::Consequential => true,
            GateScope::All => !self.enforcing || self.verified,
        }
    }
}

/// Process-global current-turn owner gate. `None` = no turn has installed one,
/// which reads as [`OwnerGate::OFF`] — the safe default (no gating), exactly like
/// `ALLOW_CONSEQUENTIAL` defaulting OFF. Set at the top of each turn, read at the
/// deep gate call sites, cleared at turn end.
static TURN_GATE: Mutex<Option<OwnerGate>> = Mutex::new(None);

// Test-only thread-local override, mirroring `integrations`'s
// `CONSEQUENTIAL_OVERRIDE`: a test forces a gate on its OWN thread without
// touching the process-global slot other tests may rely on. Compiled out in
// release. (Plain comment: rustdoc can't attach a doc comment to a macro
// invocation — it would warn.)
#[cfg(test)]
thread_local! {
    static GATE_OVERRIDE: std::cell::Cell<Option<OwnerGate>> = const { std::cell::Cell::new(None) };
}

/// Install THIS turn's gate (called once near the top of `run_pipeline`, after
/// verification). Poison-tolerant.
pub fn set_turn_gate(gate: OwnerGate) {
    *TURN_GATE.lock().unwrap_or_else(|p| p.into_inner()) = Some(gate);
}

/// Clear the per-turn gate at turn end so a later turn that skips verification
/// (e.g. voice-id disabled) never inherits a stale verified=true. Poison-tolerant.
pub fn clear_turn_gate() {
    *TURN_GATE.lock().unwrap_or_else(|p| p.into_inner()) = None;
}

/// The current turn's gate — [`OwnerGate::OFF`] when none is installed. This is
/// the deep read consulted by `execute_tool` / `replay_confirmed_action`.
pub fn current_turn_gate() -> OwnerGate {
    #[cfg(test)]
    {
        if let Some(g) = GATE_OVERRIDE.with(std::cell::Cell::get) {
            return g;
        }
    }
    TURN_GATE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .unwrap_or(OwnerGate::OFF)
}

/// `#[cfg(test)]`-only RAII guard forcing `current_turn_gate()` to a value on the
/// current thread, restoring the prior state on drop (so the override never leaks
/// into another test). The whole seam is `cfg(test)`.
#[cfg(test)]
pub(crate) struct GateOverride {
    prev: Option<OwnerGate>,
}

#[cfg(test)]
impl GateOverride {
    pub(crate) fn force(gate: OwnerGate) -> Self {
        let prev = GATE_OVERRIDE.with(|c| c.replace(Some(gate)));
        Self { prev }
    }
}

#[cfg(test)]
impl Drop for GateOverride {
    fn drop(&mut self) {
        GATE_OVERRIDE.with(|c| c.set(self.prev));
    }
}

// ---------------------------------------------------------------------------
// Explicit enroll / forget intents (never automatic)
// ---------------------------------------------------------------------------

/// An explicit voice-id management intent parsed from a spoken utterance. Only
/// these EXPLICIT phrasings ever start enrollment or clear the profile — voice-id
/// is NEVER auto-enrolled from an ordinary utterance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceIntent {
    /// "enroll my voice" / "learn my voice" / "remember my voice".
    Enroll,
    /// "forget my voice" / "delete my voice profile" / "unenroll my voice".
    Forget,
}

/// Detect an explicit enroll/forget intent. CONSERVATIVE and phrase-anchored:
/// the utterance must mention the user's VOICE together with an enroll/learn or
/// a forget/delete verb, so an ordinary sentence that merely contains "voice"
/// never trips it. Pure — unit-tested without audio.
pub fn classify_intent(utterance: &str) -> Option<VoiceIntent> {
    let lower = utterance.to_lowercase();
    // Must be about the speaker's own voice/voiceprint.
    let about_voice = lower.contains("my voice")
        || lower.contains("my voiceprint")
        || lower.contains("voice profile")
        || lower.contains("voice id")
        || lower.contains("voice-id");
    if !about_voice {
        return None;
    }
    // FORGET takes priority (a "forget" verb is an unambiguous clear).
    const FORGET: &[&str] = &["forget", "delete", "remove", "clear", "unenroll", "un-enroll", "erase"];
    if FORGET.iter().any(|v| lower.contains(v)) {
        return Some(VoiceIntent::Forget);
    }
    const ENROLL: &[&str] = &["enroll", "learn", "remember", "register", "set up", "memorize"];
    if ENROLL.iter().any(|v| lower.contains(v)) {
        return Some(VoiceIntent::Enroll);
    }
    None
}

/// The spoken refusal when an unrecognized speaker is denied a consequential
/// action under voice-id. Honest: it says the voice wasn't recognized and names
/// the layered nature (a bystander can't drive outward actions).
pub fn unrecognized_refusal() -> String {
    "I don't recognize your voice, so I won't run that. Outward actions are \
     limited to the enrolled owner — ask the owner to do it, or disable voice-id."
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic synthetic "voice": a sum of sinusoids at the given
    /// fundamental + a few harmonics, at `sample_rate` for `secs`. Two different
    /// fundamentals/harmonic structures stand in for two clearly-different
    /// speakers. Pure f32 samples — NO microphone, NO file, fully in-memory.
    fn synth_voice(sample_rate: u32, secs: f64, f0: f64, harmonics: &[(f64, f32)]) -> Vec<f32> {
        let n = (sample_rate as f64 * secs) as usize;
        let sr = sample_rate as f64;
        (0..n)
            .map(|i| {
                let t = i as f64 / sr;
                let mut s = 0.0f32;
                for &(mult, amp) in harmonics {
                    s += amp * (2.0 * std::f64::consts::PI * f0 * mult * t).sin() as f32;
                }
                // Light amplitude envelope so it isn't a perfectly stationary
                // tone (gives the temporal-std features something to measure).
                let env = 0.6 + 0.4 * (2.0 * std::f64::consts::PI * 3.0 * t).sin() as f32;
                s * env * 0.3
            })
            .collect()
    }

    /// The canonical "owner" voice used across the gate/profile tests: a low
    /// fundamental with a strong low-harmonic structure.
    fn owner_samples(sr: u32) -> Vec<f32> {
        synth_voice(sr, 1.2, 130.0, &[(1.0, 1.0), (2.0, 0.6), (3.0, 0.3), (4.0, 0.15)])
    }

    /// A clearly-DIFFERENT "intruder" voice: a much higher fundamental and a
    /// brighter (high-harmonic-weighted) spectrum, so its filterbank envelope is
    /// plainly distinct from the owner's.
    fn intruder_samples(sr: u32) -> Vec<f32> {
        synth_voice(sr, 1.2, 320.0, &[(1.0, 0.3), (3.0, 0.6), (5.0, 1.0), (7.0, 0.5)])
    }

    #[test]
    fn embedding_is_deterministic_fixed_dim_and_l2_normalized() {
        let sr = 16_000;
        let s = owner_samples(sr);
        let a = embed(&s, sr).expect("owner buffer embeds");
        let b = embed(&s, sr).expect("owner buffer embeds again");
        // DETERMINISTIC: byte-identical buffer -> identical vector.
        assert_eq!(a, b, "embedding must be deterministic for a given buffer");
        // FIXED dimension.
        assert_eq!(a.len(), EMBED_DIM, "embedding is a fixed-dim vector");
        // L2-NORMALIZED to ~1.
        let norm = a.iter().map(|v| (*v as f64).powi(2)).sum::<f64>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "embedding must be L2-normalized: |v|={norm}");
        // All finite.
        assert!(a.iter().all(|v| v.is_finite()), "no NaN/inf in the embedding");
    }

    #[test]
    fn embed_returns_none_for_unusable_audio() {
        // Empty buffer.
        assert!(embed(&[], 16_000).is_none(), "empty buffer has no embedding");
        // Shorter than a single 25ms frame at 16kHz (400 samples).
        assert!(embed(&[0.1f32; 100], 16_000).is_none(), "sub-frame buffer has no embedding");
        // Pure digital silence -> degenerate (zero) feature -> None (fail-closed).
        assert!(embed(&[0.0f32; 16_000], 16_000).is_none(), "silence is not usable audio");
        // Zero sample rate is rejected, not a panic.
        assert!(embed(&[0.1f32; 16_000], 0).is_none(), "zero sample rate is rejected");
    }

    #[test]
    fn cosine_similarity_is_correct() {
        // Identical unit vectors -> 1.0.
        let v = vec![0.6f32, 0.8];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-9);
        // Orthogonal -> 0.0.
        assert!((cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]) - 0.0).abs() < 1e-9);
        // Opposite -> -1.0.
        assert!((cosine_similarity(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-9);
        // Length mismatch / empty / zero-norm -> 0.0 (no panic).
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[1.0]), 0.0);
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
    }

    #[test]
    fn owner_verifies_and_intruder_is_rejected() {
        let sr = 16_000;
        let owner = embed(&owner_samples(sr), sr).expect("owner embeds");
        let intruder = embed(&intruder_samples(sr), sr).expect("intruder embeds");

        // Enroll the owner; verify the owner passes, the intruder fails, at the
        // shipped default threshold.
        let mut profile = OwnerProfile::new(DEFAULT_THRESHOLD);
        profile.enroll(owner.clone());

        let own = profile.verify(&owner);
        assert!(own.verified, "owner must verify (score {})", own.score);
        assert!(own.score >= DEFAULT_THRESHOLD, "owner score clears the threshold");

        let intr = profile.verify(&intruder);
        assert!(!intr.verified, "a clearly-different voice must be rejected (score {})", intr.score);
        assert!(intr.score < DEFAULT_THRESHOLD, "intruder score is below the threshold");

        // The separation is real: owner self-similarity >> owner/intruder.
        assert!(
            own.score - intr.score > 0.05,
            "owner ({}) vs intruder ({}) must separate clearly",
            own.score,
            intr.score
        );
    }

    #[test]
    fn empty_profile_never_verifies() {
        let sr = 16_000;
        let owner = embed(&owner_samples(sr), sr).expect("owner embeds");
        let profile = OwnerProfile::new(DEFAULT_THRESHOLD);
        assert!(!profile.is_enrolled(), "fresh profile is unenrolled");
        let out = profile.verify(&owner);
        assert!(!out.verified, "an unenrolled profile gates nothing (verified=false)");
        assert_eq!(out.score, 0.0, "no centroids -> score 0");
    }

    #[test]
    fn the_acoustic_embedder_trait_matches_the_free_function() {
        let sr = 16_000;
        let s = owner_samples(sr);
        let e = AcousticEmbedder;
        assert_eq!(e.embed(&s, sr), embed(&s, sr), "trait impl == free function");
        assert_eq!(e.method(), "acoustic-filterbank-stats");
    }

    #[test]
    fn enrollment_accumulates_min_samples_then_completes() {
        let sr = 16_000;
        let owner = embed(&owner_samples(sr), sr).expect("owner embeds");
        let mut enroll = Enrollment::begin(3, DEFAULT_THRESHOLD);

        // First two feeds are Progress with a decreasing `need`.
        match enroll.feed(owner.clone()) {
            EnrollStep::Progress { captured, need } => {
                assert_eq!(captured, 1);
                assert_eq!(need, 2);
            }
            other => panic!("expected Progress, got {other:?}"),
        }
        match enroll.feed(owner.clone()) {
            EnrollStep::Progress { captured, need } => {
                assert_eq!(captured, 2);
                assert_eq!(need, 1);
            }
            other => panic!("expected Progress, got {other:?}"),
        }
        // The third feed completes with a 3-centroid profile.
        match enroll.feed(owner.clone()) {
            EnrollStep::Complete(profile) => {
                assert_eq!(profile.centroids.len(), 3);
                assert_eq!(profile.n_samples, 3);
                assert!(profile.is_enrolled());
                // The owner verifies against the just-built profile.
                assert!(profile.verify(&owner).verified);
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn forget_clears_the_profile() {
        let sr = 16_000;
        let owner = embed(&owner_samples(sr), sr).expect("owner embeds");
        let mut profile = OwnerProfile::new(DEFAULT_THRESHOLD);
        profile.enroll(owner.clone());
        assert!(profile.is_enrolled());
        profile.forget();
        assert!(!profile.is_enrolled(), "forget drops every centroid");
        assert_eq!(profile.n_samples, 0);
        assert!(!profile.verify(&owner).verified, "a forgotten profile gates nothing");
    }

    #[test]
    fn profile_round_trips_through_disk_with_restrictive_perms() {
        // A hermetic temp dir under the OS temp — no daemon, no network.
        let dir = std::env::temp_dir().join(format!("darwin-voiceid-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sr = 16_000;
        let owner = embed(&owner_samples(sr), sr).expect("owner embeds");
        let mut profile = OwnerProfile::new(0.9);
        profile.enroll(owner.clone());

        // No profile yet.
        assert!(load_profile(&dir).is_none(), "no file -> no profile");

        save_profile(&dir, &profile).expect("save the local vector");
        // The persisted file holds the VECTOR, with 0600 perms on Unix.
        let path = profile_path(&dir);
        assert!(path.exists(), "profile file written");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "owner profile must be 0600");
        }

        // Round-trips: the loaded profile verifies the same owner.
        let loaded = load_profile(&dir).expect("profile loads back");
        assert_eq!(loaded.centroids.len(), 1);
        assert!((loaded.threshold - 0.9).abs() < 1e-12);
        assert!(loaded.verify(&owner).verified);

        // delete_profile ("forget my voice") removes the file; a second delete is
        // still Ok (idempotent).
        delete_profile(&dir).expect("delete the profile");
        assert!(!path.exists(), "profile file removed");
        assert!(load_profile(&dir).is_none(), "deleted -> no profile");
        delete_profile(&dir).expect("delete is idempotent");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn profile_round_trips_through_the_encrypted_vault_with_a_test_key() {
        // Hermetic: a temp root, an EXPLICIT in-test key (no Keychain), no network.
        let dir = std::env::temp_dir().join(format!("darwin-voiceid-enc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let key = crate::crypto::SecretKey::from_bytes([5u8; crate::crypto::KEY_BYTES]);
        let sr = 16_000;
        let owner = embed(&owner_samples(sr), sr).expect("owner embeds");
        let mut profile = OwnerProfile::new(0.9);
        profile.enroll(owner.clone());

        // No vault yet.
        assert!(load_profile_encrypted(&dir, &key).is_none(), "no vault -> no profile");

        save_profile_encrypted(&dir, &profile, &key).expect("save into the encrypted vault");
        let vpath = vault_path(&dir);
        assert!(vpath.exists(), "vault file written");

        // The vault is CIPHERTEXT at rest: the SQLite magic header is absent (it is
        // a SQLCipher-encrypted file), so a plain reader can't see the schema/blob.
        let raw = std::fs::read(&vpath).unwrap();
        assert!(
            !raw.starts_with(b"SQLite format 3\0"),
            "vault must be encrypted (no plaintext SQLite header)"
        );

        // Round-trips WITH the key: the loaded profile verifies the same owner.
        let loaded = load_profile_encrypted(&dir, &key).expect("vault loads back");
        assert_eq!(loaded.centroids.len(), 1);
        assert!((loaded.threshold - 0.9).abs() < 1e-12);
        assert!(loaded.verify(&owner).verified);

        // The WRONG key cannot read it.
        let wrong = crate::crypto::SecretKey::from_bytes([6u8; crate::crypto::KEY_BYTES]);
        assert!(
            load_profile_encrypted(&dir, &wrong).is_none(),
            "wrong key must not read the vault"
        );

        // delete_vault forgets it; idempotent.
        delete_vault(&dir).expect("delete the vault");
        assert!(!vpath.exists(), "vault removed");
        assert!(load_profile_encrypted(&dir, &key).is_none(), "deleted -> no profile");
        delete_vault(&dir).expect("delete is idempotent");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn encrypted_enroll_write_lands_in_the_vault_with_no_plaintext_residue_and_boot_reads_it_back() {
        // SECURITY FINDING #2 (NOT-WIRED): the live enroll write and boot read must
        // honor the master key. This mirrors the EXACT runtime contract main.rs now
        // implements: when a key is present the enroll-complete path saves into the
        // ENCRYPTED vault (never a fresh plaintext owner.json), and the boot read
        // loads through the vault — so the enrolled owner survives and is never
        // written in the clear at rest.
        let dir = std::env::temp_dir().join(format!("darwin-voiceid-wire-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let key = crate::crypto::SecretKey::from_bytes([8u8; crate::crypto::KEY_BYTES]);
        let sr = 16_000;
        let owner = embed(&owner_samples(sr), sr).expect("owner embeds");
        let mut profile = OwnerProfile::new(DEFAULT_THRESHOLD);
        profile.enroll(owner.clone());

        // The ENROLL-COMPLETE write path under encryption (main.rs:2039 branch):
        // save_profile_encrypted, NOT save_profile.
        save_profile_encrypted(&dir, &profile, &key).expect("encrypted enroll write");

        // (a) NO plaintext owner.json on disk — a re-enrollment must never leave the
        //     owner feature vector in the clear at rest.
        assert!(
            !profile_path(&dir).exists(),
            "encrypted enroll must NOT write a plaintext owner.json"
        );
        // (b) The vault file IS present and is ciphertext (no SQLite magic header).
        let vpath = vault_path(&dir);
        assert!(vpath.exists(), "encrypted enroll writes the vault");
        let raw = std::fs::read(&vpath).unwrap();
        assert!(
            !raw.starts_with(b"SQLite format 3\0"),
            "the vault must be SQLCipher ciphertext, not a plaintext SQLite file"
        );

        // (c) The BOOT READ path under encryption (main.rs:1331 branch):
        //     load_profile_encrypted with the key loads the enrolled owner back, and
        //     it verifies — so the owner is NOT silently lost.
        let loaded = load_profile_encrypted(&dir, &key).expect("boot read loads the vault");
        assert!(loaded.is_enrolled(), "the enrolled owner survives a boot read");
        assert!(loaded.verify(&owner).verified, "the loaded owner still verifies");

        // (d) The encrypted FORGET path (main.rs forget branch): delete_vault clears
        //     it; the boot read then finds nothing (unenrolled = no gating).
        delete_vault(&dir).expect("encrypted forget");
        assert!(!vpath.exists(), "forget removes the vault");
        assert!(
            load_profile_encrypted(&dir, &key).is_none(),
            "after forget the boot read finds no profile"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migration_moves_plaintext_profile_into_the_encrypted_vault() {
        let dir = std::env::temp_dir().join(format!("darwin-voiceid-mig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let key = crate::crypto::SecretKey::from_bytes([3u8; crate::crypto::KEY_BYTES]);
        let sr = 16_000;
        let owner = embed(&owner_samples(sr), sr).expect("owner embeds");
        let mut profile = OwnerProfile::new(0.88);
        profile.enroll(owner.clone());

        // 1. A PLAINTEXT owner.json exists (today's on-disk shape).
        save_profile(&dir, &profile).expect("save plaintext profile");
        assert!(profile_path(&dir).exists(), "plaintext profile present");

        // 2. Migrate on enable.
        migrate_profile_to_vault(&dir, &key).expect("migrate to vault");

        // 3. The plaintext file is gone; the encrypted vault holds the profile.
        assert!(!profile_path(&dir).exists(), "plaintext profile removed after migration");
        let loaded = load_profile_encrypted(&dir, &key).expect("vault has the profile");
        assert!(loaded.verify(&owner).verified, "migrated profile still verifies the owner");

        // 4. Migrating again with no plaintext is a no-op success.
        migrate_profile_to_vault(&dir, &key).expect("idempotent no-op migration");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enroll_and_forget_intents_are_phrase_anchored() {
        use VoiceIntent::*;
        // Enroll phrasings.
        for u in [
            "enroll my voice",
            "DARWIN, learn my voice",
            "remember my voice please",
            "register my voiceprint",
            "set up my voice profile",
        ] {
            assert_eq!(classify_intent(u), Some(Enroll), "{u:?} should enroll");
        }
        // Forget phrasings (forget verb wins).
        for u in [
            "forget my voice",
            "delete my voice profile",
            "clear my voiceprint",
            "unenroll my voice",
            "remove my voice id",
        ] {
            assert_eq!(classify_intent(u), Some(Forget), "{u:?} should forget");
        }
        // Ordinary sentences — even ones containing "voice" — must NOT trip it.
        for u in [
            "what's the weather",
            "lower your voice",            // about voice, but no enroll/forget verb
            "send an email to bob",
            "i like the sound of my voice", // "my voice" but no management verb
            "learn about quantum physics",  // learn, but not about voice
        ] {
            assert_eq!(classify_intent(u), None, "{u:?} must not be a voice-id intent");
        }
    }

    #[test]
    fn owner_gate_off_permits_everything() {
        // The OFF / unenrolled gate enforces nothing — every allow is true, so
        // behavior is unchanged from today.
        let g = OwnerGate::OFF;
        assert!(!g.enforcing);
        assert!(g.allow_consequential(), "OFF gate never blocks a consequential action");
        assert!(g.allow_confirm_replay(), "OFF gate never blocks a confirmation");
        assert!(g.allow_noncly(), "OFF gate never blocks a reply");
    }

    #[test]
    fn enforcing_gate_blocks_unverified_consequential_and_confirm() {
        // Enabled + enrolled + this turn UNVERIFIED: consequential + confirm are
        // denied; under the default Consequential scope an ordinary reply is NOT.
        let unverified = OwnerGate {
            enforcing: true,
            verified: false,
            scope: GateScope::Consequential,
        };
        assert!(!unverified.allow_consequential(), "unverified cannot fire a consequential action");
        assert!(!unverified.allow_confirm_replay(), "unverified cannot replay a parked action");
        assert!(unverified.allow_noncly(), "consequential scope never blocks ordinary replies");

        // Same, but the speaker VERIFIED: everything is permitted (voice-id is
        // additive — it only ever removes, never adds, a permission).
        let verified = OwnerGate { verified: true, ..unverified };
        assert!(verified.allow_consequential());
        assert!(verified.allow_confirm_replay());
        assert!(verified.allow_noncly());
    }

    #[test]
    fn all_scope_additionally_blocks_noncly_when_unverified() {
        let unverified_all = OwnerGate {
            enforcing: true,
            verified: false,
            scope: GateScope::All,
        };
        // Under scope="all", an unverified speaker is blocked from EVERY command.
        assert!(!unverified_all.allow_noncly(), "all-scope blocks ordinary replies too when unverified");
        assert!(!unverified_all.allow_consequential());
        // Verified clears all of them.
        let verified_all = OwnerGate { verified: true, ..unverified_all };
        assert!(verified_all.allow_noncly());
        assert!(verified_all.allow_consequential());
    }

    #[test]
    fn gate_scope_parses_with_a_safe_default() {
        assert_eq!(GateScope::from_config("all"), GateScope::All);
        assert_eq!(GateScope::from_config("ALL"), GateScope::All);
        assert_eq!(GateScope::from_config("consequential"), GateScope::Consequential);
        // Unknown / empty -> the SAFE default (Consequential), never All or off.
        assert_eq!(GateScope::from_config("nonsense"), GateScope::Consequential);
        assert_eq!(GateScope::from_config(""), GateScope::Consequential);
    }

    #[test]
    fn turn_gate_override_is_thread_local_and_restores() {
        // Default (no install) reads as OFF.
        assert_eq!(current_turn_gate(), OwnerGate::OFF);
        {
            let _g = GateOverride::force(OwnerGate {
                enforcing: true,
                verified: false,
                scope: GateScope::Consequential,
            });
            assert!(current_turn_gate().enforcing);
            assert!(!current_turn_gate().allow_consequential());
        }
        // Restored on drop.
        assert_eq!(current_turn_gate(), OwnerGate::OFF);
    }

    #[test]
    fn verify_telemetry_is_secret_free() {
        let payload = verify_telemetry(VerifyOutcome { verified: true, score: 0.9123 }, true, true);
        // Carries the verdict + rounded score + flags — and NOTHING vector/audio.
        assert_eq!(payload["verified"], true);
        assert_eq!(payload["enabled"], true);
        assert_eq!(payload["enrolled"], true);
        assert_eq!(payload["score"], 0.912);
        // No embedding/audio field leaks.
        assert!(payload.get("embedding").is_none());
        assert!(payload.get("samples").is_none());
        assert!(payload.get("audio").is_none());
    }
}

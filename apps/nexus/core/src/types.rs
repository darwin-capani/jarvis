//! Shared realtime/DSP types (FROZEN — written in Foundation; module agents
//! build AGAINST these and must NOT change them). The sample/block model, the
//! per-stage DSP parameter structs, and the small numeric constants the whole
//! crate agrees on live here.
//!
//! Audio model (SPEC §2): 32-bit float, INTERLEAVED, with an explicit channel
//! count and sample rate. The realtime core mixes/process in place over these
//! buffers with NO allocation on the audio thread, so every type here is
//! `Copy`/POD-shaped and the buffer views borrow caller-owned memory (the
//! ctypes caller, or a `Vec<f32>` in a unit test) — the core never owns the
//! audio storage.

/// The crate's canonical sample type: 32-bit float, nominal range [-1.0, 1.0]
/// (values beyond are valid intermediate headroom; clip detection is a separate
/// metering stage — SPEC §3 step 4). All DSP is done in `f32`; the few places
/// that need extra precision (the LUFS integrator, the view transform) promote
/// to `f64` locally and downcast.
pub type Sample = f32;

/// Maximum channels the matrix endpoints address. A generous upper bound that
/// keeps `MatrixState` and the snapshot fixed-size / stack-friendly without a
/// heap allocation on any realtime path. Inputs and outputs each cap here.
pub const MAX_CHANNELS: usize = 32;

/// The default realtime configuration from SPEC §2: 64 frames @ 48 kHz
/// (1.33 ms/callback). The IOProc may step to 128 frames if an interface can't
/// hold 64 without overloads (SPEC §1, "step to 128 and report it").
pub const DEFAULT_BLOCK_FRAMES: usize = 64;
/// The default sample rate (Hz) the SM7dB chain runs at (SPEC §2).
pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;
/// The FFT size tapped for the spectrum (SPEC §2/§6): 2048 points, folded to 96
/// log bands by the metering stage.
pub const FFT_SIZE: usize = 2048;
/// Number of log-spaced spectrum bands published on `audio.spectrum` (SPEC §6).
pub const SPECTRUM_BANDS: usize = 96;
/// Per-block parameter ramp time (SPEC §2): 5 ms, no zipper noise. The smoother
/// in `dsp` reads this; exposed here so every stage agrees on the ramp length.
pub const PARAM_RAMP_MS: f32 = 5.0;
/// The `-inf` crosspoint sentinel in dB: a crosspoint at this gain is OFF (no
/// route). SPEC §1: "Routes are crosspoints above -inf." Stored as `f32::NEG_INFINITY`
/// in the gain grid; this named constant documents intent at call sites.
pub const GAIN_OFF_DB: f32 = f32::NEG_INFINITY;
/// The maximum crosspoint / trim gain in dB (SPEC §1: "-inf to +12 dB").
pub const GAIN_MAX_DB: f32 = 12.0;
/// True-peak clip threshold in dBFS (SPEC §3 step 4): -1 dBFS, 4× oversampled.
pub const CLIP_THRESHOLD_DBFS: f32 = -1.0;

// ===========================================================================
// Shared numeric primitives (used by both `dsp` and `metering`)
// ===========================================================================

/// Convert a gain in dB to a linear amplitude factor. The [`GAIN_OFF_DB`]
/// (`-inf`) sentinel maps to exactly `0.0` (a cleared route) — `10^(-inf/20)`
/// would already be `0.0`, but the explicit branch documents intent and avoids
/// relying on float `-inf` arithmetic on the audio path. Finite dB uses
/// `10^(db/20)`. Pure + branch-light; safe to call per crosspoint.
#[inline]
pub fn db_to_linear(db: f32) -> f32 {
    if db == GAIN_OFF_DB {
        0.0
    } else {
        // 10^(db/20) == exp(db * ln(10) / 20).
        (db * (std::f32::consts::LN_10 / 20.0)).exp()
    }
}

/// Convert a linear amplitude (>= 0) to dBFS. A non-positive amplitude maps to
/// `-inf` (silence) rather than `NaN`/`-inf`-via-log so meters read cleanly.
#[inline]
pub fn linear_to_db(amp: f32) -> f32 {
    if amp <= 0.0 {
        f32::NEG_INFINITY
    } else {
        20.0 * amp.log10()
    }
}

// ===========================================================================
// Audio block views
// ===========================================================================

/// The interleaved audio format a block carries: channel count + sample rate.
/// Frame count is carried by the block view itself (it is the buffer length /
/// channels). `Copy` so it crosses the FFI and rides in the snapshot freely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    /// Interleaved channel count (1 = mono, 2 = stereo, …), `<= MAX_CHANNELS`.
    pub channels: u16,
    /// Sample rate in Hz (e.g. 48_000).
    pub sample_rate: u32,
}

impl AudioFormat {
    /// Construct a format. Pure value type; validation of bounds happens at the
    /// FFI/control-plane edge, not here.
    pub const fn new(channels: u16, sample_rate: u32) -> Self {
        Self { channels, sample_rate }
    }
}

impl Default for AudioFormat {
    fn default() -> Self {
        Self { channels: 2, sample_rate: DEFAULT_SAMPLE_RATE }
    }
}

/// A read-only borrowed view of one interleaved audio block. Borrows the
/// caller's sample storage (ctypes pointer or test `Vec`); the core never owns
/// it. Length must be `frames * channels`.
#[derive(Debug, Clone, Copy)]
pub struct BlockRef<'a> {
    /// Interleaved samples, length == `frames * format.channels`.
    pub data: &'a [Sample],
    /// The channel count + sample rate of `data`.
    pub format: AudioFormat,
}

/// A mutable borrowed view of one interleaved audio block — the output the mix
/// + DSP chain writes in place. Same length invariant as [`BlockRef`].
#[derive(Debug)]
pub struct BlockMut<'a> {
    /// Interleaved samples written in place, length == `frames * format.channels`.
    pub data: &'a mut [Sample],
    /// The channel count + sample rate of `data`.
    pub format: AudioFormat,
}

impl BlockRef<'_> {
    /// Frames per channel = total interleaved samples / channel count. Returns 0
    /// if the format declares 0 channels (degenerate; the FFI edge rejects it).
    pub fn frames(&self) -> usize {
        let ch = self.format.channels as usize;
        self.data.len().checked_div(ch).unwrap_or(0)
    }
}

impl BlockMut<'_> {
    /// Frames per channel; see [`BlockRef::frames`].
    pub fn frames(&self) -> usize {
        let ch = self.format.channels as usize;
        self.data.len().checked_div(ch).unwrap_or(0)
    }
}

// ===========================================================================
// DSP parameter structs (SPEC §3 step 3 — the per-input local DSP chain)
// ===========================================================================
//
// Each stage is bypassable and `Copy`. Smoothing/coefficient derivation lives
// in the `dsp` module (module-agent owned); these structs are JUST the
// parameter contract — the values the control plane sets and the audio thread
// reads via the lock-free snapshot. Defaults match the SPEC §3 starting policy.

/// A bypassable high-pass filter. SPEC §3: HPF 80 Hz, 12 dB/oct (a 2nd-order
/// Butterworth — two poles). The `dsp` module turns these into biquad
/// coefficients; this is only the parameter face.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FilterParams {
    /// Whether the stage runs at all (false = passthrough).
    pub enabled: bool,
    /// Corner / cutoff frequency in Hz (SPEC default 80.0).
    pub cutoff_hz: f32,
    /// Filter order in poles (2 = 12 dB/oct, the SPEC default).
    pub order: u8,
}

impl Default for FilterParams {
    fn default() -> Self {
        Self { enabled: true, cutoff_hz: 80.0, order: 2 }
    }
}

/// A bypassable downward gate. SPEC §3: -45 dB threshold, 80 ms release.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GateParams {
    /// Whether the stage runs (false = passthrough).
    pub enabled: bool,
    /// Threshold in dBFS below which gain reduction engages (SPEC default -45.0).
    pub threshold_db: f32,
    /// Attack time in ms (how fast the gate opens). Conservative default.
    pub attack_ms: f32,
    /// Release time in ms (how fast it closes; SPEC default 80.0).
    pub release_ms: f32,
    /// Floor in dB the gate attenuates to when fully closed (e.g. -inf-ish, but
    /// a finite floor like -80 avoids hard mutes / clicks).
    pub floor_db: f32,
}

impl Default for GateParams {
    fn default() -> Self {
        Self { enabled: true, threshold_db: -45.0, attack_ms: 1.0, release_ms: 80.0, floor_db: -80.0 }
    }
}

/// A bypassable de-esser. SPEC §3: 5–8 kHz, 4:1. A frequency-selective
/// compressor over the sibilance band.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeEsserParams {
    /// Whether the stage runs (false = passthrough).
    pub enabled: bool,
    /// Low edge of the sibilance detection band in Hz (SPEC default 5000.0).
    pub band_low_hz: f32,
    /// High edge of the sibilance detection band in Hz (SPEC default 8000.0).
    pub band_high_hz: f32,
    /// Threshold in dBFS above which the band is attenuated.
    pub threshold_db: f32,
    /// Compression ratio over the band (SPEC default 4.0 -> "4:1").
    pub ratio: f32,
}

impl Default for DeEsserParams {
    fn default() -> Self {
        Self { enabled: true, band_low_hz: 5000.0, band_high_hz: 8000.0, threshold_db: -28.0, ratio: 4.0 }
    }
}

/// A bypassable compressor. SPEC §3: 3:1, 10 ms attack / 120 ms release,
/// ~4 dB gain-reduction target.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompressorParams {
    /// Whether the stage runs (false = passthrough).
    pub enabled: bool,
    /// Threshold in dBFS where compression begins.
    pub threshold_db: f32,
    /// Compression ratio (SPEC default 3.0 -> "3:1").
    pub ratio: f32,
    /// Attack time in ms (SPEC default 10.0).
    pub attack_ms: f32,
    /// Release time in ms (SPEC default 120.0).
    pub release_ms: f32,
    /// Knee width in dB (0 = hard knee). A soft knee smooths the transition.
    pub knee_db: f32,
    /// Make-up gain in dB applied after compression.
    pub makeup_db: f32,
}

impl Default for CompressorParams {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold_db: -18.0,
            ratio: 3.0,
            attack_ms: 10.0,
            release_ms: 120.0,
            knee_db: 6.0,
            makeup_db: 0.0,
        }
    }
}

/// The full per-input local DSP chain (SPEC §3 step 3), in signal-flow order:
/// HPF -> gate -> de-esser -> compressor -> output trim. Bypassable as a whole
/// (`enabled`) and per stage (each struct's `enabled`). `Copy` so it rides in
/// the lock-free snapshot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChannelDsp {
    /// Master bypass for the whole chain on this channel (false = passthrough).
    pub enabled: bool,
    /// Stage 1: high-pass filter.
    pub hpf: FilterParams,
    /// Stage 2: downward gate.
    pub gate: GateParams,
    /// Stage 3: de-esser.
    pub deesser: DeEsserParams,
    /// Stage 4: compressor.
    pub compressor: CompressorParams,
    /// Output trim in dB applied after the chain (SPEC §3 step 3 "output trim").
    pub output_trim_db: f32,
}

impl Default for ChannelDsp {
    fn default() -> Self {
        Self {
            // Default chain is OFF (master bypass) so an un-configured channel is
            // bit-transparent until the control plane opts it in (SPEC §3:
            // "Optional local DSP (per-input, bypassable)").
            enabled: false,
            hpf: FilterParams::default(),
            gate: GateParams::default(),
            deesser: DeEsserParams::default(),
            compressor: CompressorParams::default(),
            output_trim_db: 0.0,
        }
    }
}

// ===========================================================================
// Metering payload types (SPEC §6 telemetry; the control plane folds + ships)
// ===========================================================================

/// Per-channel peak + RMS in dBFS (the `audio.levels` `ch[]` entries, SPEC §6).
/// Plain POD so it crosses the FFI in a flat array the Python side reads.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChannelMeter {
    /// Sample peak this window, dBFS.
    pub peak_dbfs: f32,
    /// RMS this window, dBFS.
    pub rms_dbfs: f32,
}

impl Default for ChannelMeter {
    fn default() -> Self {
        // Silence reads as -inf dBFS; use a very low floor so the HUD shows an
        // empty meter rather than NaN.
        Self { peak_dbfs: f32::NEG_INFINITY, rms_dbfs: f32::NEG_INFINITY }
    }
}

/// The BS.1770-4 loudness triplet (SPEC §6 `audio.levels`): momentary (400 ms),
/// short-term (3 s), and gated integrated loudness, all in LUFS.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoudnessMeter {
    /// Momentary loudness (400 ms window), LUFS.
    pub lufs_m: f32,
    /// Short-term loudness (3 s window), LUFS.
    pub lufs_s: f32,
    /// Integrated loudness (BS.1770-4 absolute + relative gating), LUFS.
    pub lufs_i: f32,
}

impl Default for LoudnessMeter {
    fn default() -> Self {
        Self { lufs_m: f32::NEG_INFINITY, lufs_s: f32::NEG_INFINITY, lufs_i: f32::NEG_INFINITY }
    }
}

/// A clip event (SPEC §6 `audio.clipping`): the channel + the measured true-peak.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClipEvent {
    /// The channel index that clipped.
    pub channel: u16,
    /// The measured true-peak (4× oversampled) in dBFS at the clip.
    pub true_peak_dbfs: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_frames_from_interleaving() {
        let buf = vec![0.0f32; 64 * 2];
        let b = BlockRef { data: &buf, format: AudioFormat::new(2, 48_000) };
        assert_eq!(b.frames(), 64);
        // Zero channels is degenerate but must not divide-by-zero.
        let b0 = BlockRef { data: &buf, format: AudioFormat::new(0, 48_000) };
        assert_eq!(b0.frames(), 0);
    }

    #[test]
    fn dsp_defaults_match_spec() {
        let d = ChannelDsp::default();
        // Master bypass off by default (bit-transparent until opted in).
        assert!(!d.enabled);
        assert_eq!(d.hpf.cutoff_hz, 80.0);
        assert_eq!(d.hpf.order, 2);
        assert_eq!(d.gate.threshold_db, -45.0);
        assert_eq!(d.gate.release_ms, 80.0);
        assert_eq!(d.deesser.ratio, 4.0);
        assert_eq!(d.compressor.ratio, 3.0);
        assert_eq!(d.compressor.attack_ms, 10.0);
        assert_eq!(d.compressor.release_ms, 120.0);
    }

    #[test]
    fn gain_sentinels() {
        assert!(GAIN_OFF_DB.is_infinite() && GAIN_OFF_DB.is_sign_negative());
        assert_eq!(GAIN_MAX_DB, 12.0);
        assert_eq!(CLIP_THRESHOLD_DBFS, -1.0);
    }

    #[test]
    fn db_linear_roundtrip() {
        // 0 dB == unity; -inf == 0 (cleared route); +6 dB ~= 2x amplitude.
        assert!((db_to_linear(0.0) - 1.0).abs() < 1e-6);
        assert_eq!(db_to_linear(GAIN_OFF_DB), 0.0);
        assert!((db_to_linear(6.0206) - 2.0).abs() < 1e-3);
        // linear_to_db inverts it; silence maps to -inf, not NaN.
        assert!(linear_to_db(0.0).is_infinite());
        assert!((linear_to_db(1.0)).abs() < 1e-6);
        let r = linear_to_db(db_to_linear(-3.0));
        assert!((r - (-3.0)).abs() < 1e-4);
    }
}

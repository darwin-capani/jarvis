//! Metering — peak/RMS taps, BS.1770-4 LUFS, true-peak clip detect, and the
//! 2048-pt FFT folded to 96 log bands (SPEC §3 step 4, §6).
//!
//! MODULE-AGENT FILE: the metering agent fills the bodies against the FROZEN
//! payload types in [`crate::types`] ([`ChannelMeter`], [`LoudnessMeter`],
//! [`ClipEvent`]) and the constants ([`FFT_SIZE`], [`SPECTRUM_BANDS`],
//! [`CLIP_THRESHOLD_DBFS`]). The public seam (function/type signatures the FFI
//! getters and [`crate::engine::Engine`] read) is FROZEN; only the bodies here
//! are filled.
//!
//! Math implemented (NEVER weakened):
//!   - per-channel sample peak + RMS over the metering window -> [`ChannelMeter`]
//!     in dBFS (SPEC §6 `audio.levels`, 30 Hz).
//!   - true-peak via 4× oversampling (a windowed-sinc polyphase interpolator),
//!     compared against [`CLIP_THRESHOLD_DBFS`] (-1 dBFS) -> [`ClipEvent`] on
//!     exceedance (SPEC §3 step 4, §6). Catches inter-sample peaks the raw sample
//!     peak misses.
//!   - BS.1770-4 K-weighting (stage-1 high-shelf + stage-2 RLB high-pass),
//!     400 ms momentary / 3 s short-term windows, and ABSOLUTE (-70 LUFS) +
//!     RELATIVE (-10 LU) gating for integrated loudness -> [`LoudnessMeter`].
//!   - a 2048-point real FFT (hand-written radix-2, std-only), Hann-windowed,
//!     magnitude -> dBFS, folded into 96 log-spaced bands -> [`SpectrumFrame`]
//!     (SPEC §6 `audio.spectrum`, 30 Hz).
//!
//! All of it is headlessly testable with synthesized buffers (sine sweeps, the
//! BS.1770 -23 LUFS reference signal, a -1 dBFS inter-sample-peak test tone) and
//! never opens a CoreAudio device, binds a socket, or allocates on the audio
//! thread's hot path beyond the fixed-size state owned here.

use std::f32::consts::PI;

use crate::types::{
    linear_to_db, BlockRef, ChannelMeter, ClipEvent, LoudnessMeter, Sample, CLIP_THRESHOLD_DBFS,
    DEFAULT_SAMPLE_RATE, FFT_SIZE, SPECTRUM_BANDS,
};

/// Re-export of the fixed spectrum band count under a metering-local name, so
/// the FFI getter (`nexus_get_spectrum`) can size its copy without reaching into
/// `types`. Identical to [`crate::types::SPECTRUM_BANDS`] (96).
pub const SPECTRUM_BANDS_HINT: usize = SPECTRUM_BANDS;

/// One spectrum frame: 96 log-spaced bands in dBFS (SPEC §6 `audio.spectrum`).
/// Fixed-size POD so it crosses the FFI as a flat array.
#[derive(Debug, Clone, Copy)]
pub struct SpectrumFrame {
    /// 96 band magnitudes in dBFS, low frequency -> high.
    pub bands: [f32; SPECTRUM_BANDS],
}

impl Default for SpectrumFrame {
    fn default() -> Self {
        Self { bands: [f32::NEG_INFINITY; SPECTRUM_BANDS] }
    }
}

/// Per-channel peak + RMS over one block (SPEC §6 `audio.levels`). Computes the
/// block-local sample peak + RMS in dBFS. The engine's windowed meter is built
/// from these per-block readings; this helper is the exact, allocation-free
/// kernel both the engine and the FFI level getter rely on.
///
/// `channel` selects one interleaved lane of `block.data`. A degenerate format
/// (0 channels, or a channel index past the count) reads as silence (-inf), not
/// NaN, so the HUD never shows garbage.
pub fn block_meter(block: &BlockRef<'_>, channel: usize) -> ChannelMeter {
    let ch = block.format.channels as usize;
    if ch == 0 || channel >= ch {
        return ChannelMeter::default();
    }
    let mut peak = 0.0f32;
    let mut sumsq = 0.0f64;
    let mut n = 0u64;
    let mut i = channel;
    while i < block.data.len() {
        let s = block.data[i];
        let a = s.abs();
        if a > peak {
            peak = a;
        }
        sumsq += (s as f64) * (s as f64);
        n += 1;
        i += ch;
    }
    let rms = if n > 0 { (sumsq / n as f64).sqrt() as f32 } else { 0.0 };
    ChannelMeter { peak_dbfs: linear_to_db(peak), rms_dbfs: linear_to_db(rms) }
}

// ===========================================================================
// True-peak (4× oversampling) clip detection — SPEC §3 step 4
// ===========================================================================

/// Oversampling factor for the true-peak detector (ITU-R BS.1770-4 recommends
/// at least 4× for content up to 48 kHz).
const TP_OVERSAMPLE: usize = 4;
/// Taps PER PHASE of the polyphase interpolation filter. 12 taps/phase (48-tap
/// prototype) gives a clean windowed-sinc reconstruction — enough to expose the
/// inter-sample peak the raw sample grid hides, while staying cheap.
const TP_TAPS_PER_PHASE: usize = 12;

/// The polyphase coefficients for the 4× true-peak interpolator, laid out
/// `[phase][tap]`. Each of the `TP_OVERSAMPLE` phases is a 12-tap slice of one
/// shared 48-tap windowed-sinc prototype (the prototype is centered at index
/// 23.5, so NO phase is a clean unit impulse — phase 0's tap weights are
/// dominated by but not equal to a single sample; its largest tap is ≈0.9727,
/// not 1.0). The phases are therefore all genuine fractional-delay
/// reconstructors, none a bit-transparent passthrough. Correctness of the
/// detector does NOT rely on any phase reproducing the input exactly: the raw
/// sample peak is folded back in as a floor in [`true_peak_linear`]
/// (`peak.max(raw)`), so a clip on the sample grid always registers even if the
/// nearest reconstructed phase reads a hair under it. Built once per detector —
/// the design is pure and depends only on the constants above.
struct TruePeakKernel {
    phases: [[f32; TP_TAPS_PER_PHASE]; TP_OVERSAMPLE],
}

impl TruePeakKernel {
    /// Design the polyphase kernel: a windowed-sinc low-pass (cutoff at the base
    /// Nyquist) sampled at the oversampled rate and split into `TP_OVERSAMPLE`
    /// phases, each normalized to unity DC gain so a full-scale signal maps to a
    /// full-scale reconstruction (no spurious gain that would over-report peaks).
    fn design() -> Self {
        let total = TP_TAPS_PER_PHASE * TP_OVERSAMPLE;
        // Center the sinc on the prototype filter so the fractional delays are
        // symmetric about the sample being interpolated.
        let center = (total - 1) as f64 / 2.0;
        let mut proto = vec![0.0f64; total];
        for (k, c) in proto.iter_mut().enumerate() {
            let x = k as f64 - center;
            // Normalized sinc with cutoff at the base Nyquist (1 / oversample of
            // the high-rate Nyquist), windowed by a Blackman window for low
            // sidelobes (no ripple that would inflate the detected peak).
            let sinc = sinc_pi(x / TP_OVERSAMPLE as f64);
            let w = blackman(k, total);
            *c = sinc * w / TP_OVERSAMPLE as f64;
        }
        // Split the prototype into polyphase components. Component `p` takes every
        // `TP_OVERSAMPLE`-th tap starting at offset `p`.
        let mut phases = [[0.0f32; TP_TAPS_PER_PHASE]; TP_OVERSAMPLE];
        for (p, phase) in phases.iter_mut().enumerate() {
            for (t, coeff) in phase.iter_mut().enumerate() {
                let idx = t * TP_OVERSAMPLE + p;
                *coeff = proto[idx] as f32;
            }
            // Normalize each phase to unity DC gain so a DC / constant block
            // reconstructs to its own amplitude (true peak >= sample peak, never
            // an attenuated reading that would miss a clip).
            let sum: f32 = phase.iter().sum();
            if sum.abs() > 1e-12 {
                for coeff in phase.iter_mut() {
                    *coeff /= sum;
                }
            }
        }
        Self { phases }
    }
}

/// Normalized sinc: `sin(pi x) / (pi x)`, with the removable singularity at 0
/// handled as 1.0.
fn sinc_pi(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        let px = std::f64::consts::PI * x;
        px.sin() / px
    }
}

/// Blackman window value for tap `k` of an `n`-tap window.
fn blackman(k: usize, n: usize) -> f64 {
    if n <= 1 {
        return 1.0;
    }
    let t = k as f64 / (n - 1) as f64;
    0.42 - 0.5 * (2.0 * std::f64::consts::PI * t).cos()
        + 0.08 * (4.0 * std::f64::consts::PI * t).cos()
}

/// The estimated true-peak (linear amplitude) of one interleaved channel of
/// `block`, via 4× windowed-sinc oversampling. Always at least the raw sample
/// peak (the polyphase phase-0 path), so it never under-reports a hard clip.
fn true_peak_linear(block: &BlockRef<'_>, channel: usize, kernel: &TruePeakKernel) -> f32 {
    let ch = block.format.channels as usize;
    if ch == 0 || channel >= ch {
        return 0.0;
    }
    // Deinterleave the requested channel. Bounded by the block length; the FFI
    // edge sizes blocks to the realtime buffer (64/128 frames) so this is small.
    let mut lane: Vec<f32> = Vec::with_capacity(block.data.len() / ch + 1);
    let mut i = channel;
    while i < block.data.len() {
        lane.push(block.data[i]);
        i += ch;
    }
    if lane.is_empty() {
        return 0.0;
    }

    let half = TP_TAPS_PER_PHASE / 2;
    let mut peak = 0.0f32;
    // For each input sample position, reconstruct the TP_OVERSAMPLE inter-sample
    // values around it and track the max magnitude. The kernel is symmetric, so
    // we tap `half` samples either side of `pos`, clamping at the edges (treat
    // out-of-range as zero — a conservative reconstruction at block boundaries).
    for pos in 0..lane.len() {
        for phase in kernel.phases.iter() {
            let mut acc = 0.0f32;
            for (t, &coeff) in phase.iter().enumerate() {
                // Tap index relative to `pos`, centered.
                let rel = t as isize - half as isize;
                let src = pos as isize + rel;
                if src >= 0 && (src as usize) < lane.len() {
                    acc += coeff * lane[src as usize];
                }
            }
            let a = acc.abs();
            if a > peak {
                peak = a;
            }
        }
    }
    // Floor at the raw sample peak: oversampling between two identical samples can
    // numerically dip a hair below the sample value, and a clip on the sample grid
    // itself must always register.
    let mut raw = 0.0f32;
    for &s in lane.iter() {
        let a = s.abs();
        if a > raw {
            raw = a;
        }
    }
    peak.max(raw)
}

/// True-peak clip detection (SPEC §3 step 4): 4× oversample to recover the
/// inter-sample peak, compare against [`CLIP_THRESHOLD_DBFS`] (-1 dBFS). Returns
/// `Some(ClipEvent)` carrying the measured true-peak in dBFS when it meets or
/// exceeds the threshold, else `None`.
pub fn detect_clip(block: &BlockRef<'_>, channel: usize) -> Option<ClipEvent> {
    let ch = block.format.channels as usize;
    if ch == 0 || channel >= ch {
        return None;
    }
    let kernel = TruePeakKernel::design();
    let tp = true_peak_linear(block, channel, &kernel);
    let tp_dbfs = linear_to_db(tp);
    if tp_dbfs >= CLIP_THRESHOLD_DBFS {
        Some(ClipEvent { channel: channel as u16, true_peak_dbfs: tp_dbfs })
    } else {
        None
    }
}

// ===========================================================================
// BS.1770-4 loudness — K-weighting + gated integration
// ===========================================================================

/// A single biquad in Direct-Form-I (one per channel needs its own delay state).
/// Normalized so `a0 == 1`.
#[derive(Debug, Clone, Copy)]
struct KBiquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

/// Per-channel delay memory for a cascade of two biquads (the two K-weighting
/// stages). DF-I keeps both input and output history.
#[derive(Debug, Clone, Copy, Default)]
struct BiquadState {
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl KBiquad {
    /// Process one sample through this biquad given its per-channel `state`.
    #[inline]
    fn process(&self, x0: f64, state: &mut BiquadState) -> f64 {
        let y0 = self.b0 * x0 + self.b1 * state.x1 + self.b2 * state.x2
            - self.a1 * state.y1
            - self.a2 * state.y2;
        state.x2 = state.x1;
        state.x1 = x0;
        state.y2 = state.y1;
        state.y1 = y0;
        y0
    }
}

/// The two K-weighting biquad stages for a given sample rate (BS.1770-4):
///   stage 1 — a high-frequency shelving filter (+4 dB above ~1.5 kHz),
///   stage 2 — a 2nd-order high-pass ("RLB") with corner near 38 Hz.
///
/// At 48 kHz these are the exact reference coefficients from the recommendation;
/// other sample rates re-derive them via the bilinear transform of the same
/// analog prototypes so the K-weighting tracks across rates.
fn k_weighting_stages(sample_rate: u32) -> (KBiquad, KBiquad) {
    if sample_rate == 48_000 {
        // Reference coefficients (ITU-R BS.1770-4, Tables 1 & 2), a0 normalized.
        let stage1 = KBiquad {
            b0: 1.53512485958697,
            b1: -2.69169618940638,
            b2: 1.19839281085285,
            a1: -1.69065929318241,
            a2: 0.73248077421585,
        };
        let stage2 = KBiquad {
            b0: 1.0,
            b1: -2.0,
            b2: 1.0,
            a1: -1.99004745483398,
            a2: 0.99007225036621,
        };
        (stage1, stage2)
    } else {
        (design_shelf(sample_rate), design_rlb_highpass(sample_rate))
    }
}

/// Stage-1 high-shelf, re-derived for an arbitrary sample rate from the
/// BS.1770-4 analog prototype (high-shelf, Q ~ 0.707, gain +4 dB, fc ~ 1681 Hz).
fn design_shelf(sample_rate: u32) -> KBiquad {
    // Audio-EQ-cookbook high-shelf, matching the reference design point.
    let fs = sample_rate as f64;
    let f0 = 1681.974450955533;
    let g_db = 3.999843853973347;
    let q = 0.7071752369554196;
    let a = 10f64.powf(g_db / 40.0);
    let w0 = 2.0 * std::f64::consts::PI * f0 / fs;
    let cw = w0.cos();
    let sw = w0.sin();
    let alpha = sw / (2.0 * q);
    let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;

    let b0 = a * ((a + 1.0) + (a - 1.0) * cw + two_sqrt_a_alpha);
    let b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cw);
    let b2 = a * ((a + 1.0) + (a - 1.0) * cw - two_sqrt_a_alpha);
    let a0 = (a + 1.0) - (a - 1.0) * cw + two_sqrt_a_alpha;
    let a1 = 2.0 * ((a - 1.0) - (a + 1.0) * cw);
    let a2 = (a + 1.0) - (a - 1.0) * cw - two_sqrt_a_alpha;

    KBiquad { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
}

/// Stage-2 RLB high-pass, re-derived for an arbitrary sample rate (2nd-order
/// high-pass, Q ~ 0.5, fc ~ 38.13 Hz — the BS.1770-4 design point).
fn design_rlb_highpass(sample_rate: u32) -> KBiquad {
    let fs = sample_rate as f64;
    let f0 = 38.13547087602444;
    let q = 0.5003270373238773;
    let w0 = 2.0 * std::f64::consts::PI * f0 / fs;
    let cw = w0.cos();
    let sw = w0.sin();
    let alpha = sw / (2.0 * q);

    let b0 = (1.0 + cw) / 2.0;
    let b1 = -(1.0 + cw);
    let b2 = (1.0 + cw) / 2.0;
    let a0 = 1.0 + alpha;
    let a1 = -2.0 * cw;
    let a2 = 1.0 - alpha;

    KBiquad { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
}

/// BS.1770-4 absolute gate threshold (LUFS).
const LUFS_ABSOLUTE_GATE: f64 = -70.0;
/// BS.1770-4 relative gate offset below the ungated-gated mean (LU).
const LUFS_RELATIVE_GATE: f64 = -10.0;
/// BS.1770 loudness offset: `LKFS = -0.691 + 10*log10(mean-square)`.
const LUFS_OFFSET: f64 = -0.691;
/// A 400 ms gating block (BS.1770-4) at 100 ms hop = 75% overlap.
const GATE_BLOCK_MS: f64 = 400.0;
const GATE_HOP_MS: f64 = 100.0;
/// Momentary window = 400 ms; short-term window = 3 s.
const MOMENTARY_MS: f64 = 400.0;
const SHORT_TERM_MS: f64 = 3000.0;

/// A stateful loudness meter (BS.1770-4). Owns the K-weighting biquad memory (one
/// state per channel), the running mean-square accumulators for the momentary /
/// short-term sliding windows, and the gated-block history for integrated
/// loudness. MODULE-AGENT OWNED; the FFI lufs getter reads [`read`].
#[derive(Debug, Clone)]
pub struct LoudnessMeterState {
    /// The configured sample rate (drives the K-weighting + window sizing at
    /// construction; retained for diagnostics + [`Self::sample_rate`]).
    sample_rate: u32,
    stage1: KBiquad,
    stage2: KBiquad,
    /// Per-channel biquad state for each of the two stages.
    state1: Vec<BiquadState>,
    state2: Vec<BiquadState>,
    /// Ring of per-sample K-weighted summed-channel mean-square contributions for
    /// the momentary (400 ms) window.
    mom_ring: Vec<f64>,
    mom_pos: usize,
    mom_filled: usize,
    mom_sum: f64,
    /// Ditto for the short-term (3 s) window.
    st_ring: Vec<f64>,
    st_pos: usize,
    st_filled: usize,
    st_sum: f64,
    /// 100 ms-hop gating-block accounting for integrated loudness.
    block_len: usize,
    hop_len: usize,
    block_count: usize,
    samples_since_hop: usize,
    /// Mean-square of every completed 400 ms gating block (for the two-pass gate).
    gating_blocks: Vec<f64>,
    /// Channel weights (BS.1770: L/R/C = 1.0, surround = 1.41). For <=2 channels
    /// all weights are 1.0; this meter is fed the mono monitor mix in practice.
    weights: Vec<f64>,
    channels: u16,
}

impl Default for LoudnessMeterState {
    fn default() -> Self {
        Self::new(DEFAULT_SAMPLE_RATE)
    }
}

impl LoudnessMeterState {
    /// Construct a loudness meter for `sample_rate`. Pre-derives the K-weighting
    /// coefficients and sizes the momentary/short-term rings + gating-block hop.
    pub fn new(sample_rate: u32) -> Self {
        let fs = sample_rate.max(1);
        let (stage1, stage2) = k_weighting_stages(fs);
        let mom_len = ((MOMENTARY_MS / 1000.0) * fs as f64).round().max(1.0) as usize;
        let st_len = ((SHORT_TERM_MS / 1000.0) * fs as f64).round().max(1.0) as usize;
        let block_len = ((GATE_BLOCK_MS / 1000.0) * fs as f64).round().max(1.0) as usize;
        let hop_len = ((GATE_HOP_MS / 1000.0) * fs as f64).round().max(1.0) as usize;
        Self {
            sample_rate: fs,
            stage1,
            stage2,
            state1: vec![BiquadState::default(); 1],
            state2: vec![BiquadState::default(); 1],
            mom_ring: vec![0.0; mom_len],
            mom_pos: 0,
            mom_filled: 0,
            mom_sum: 0.0,
            st_ring: vec![0.0; st_len],
            st_pos: 0,
            st_filled: 0,
            st_sum: 0.0,
            block_len,
            hop_len,
            block_count: 0,
            samples_since_hop: 0,
            gating_blocks: Vec::new(),
            weights: vec![1.0],
            channels: 1,
        }
    }

    /// The sample rate this meter is configured for.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Reset all running state (used when the program / route changes; the meter
    /// re-integrates from scratch so a route swap doesn't poison LUFS-I).
    pub fn reset(&mut self) {
        for s in self.state1.iter_mut() {
            *s = BiquadState::default();
        }
        for s in self.state2.iter_mut() {
            *s = BiquadState::default();
        }
        self.mom_ring.iter_mut().for_each(|v| *v = 0.0);
        self.st_ring.iter_mut().for_each(|v| *v = 0.0);
        self.mom_pos = 0;
        self.mom_filled = 0;
        self.mom_sum = 0.0;
        self.st_pos = 0;
        self.st_filled = 0;
        self.st_sum = 0.0;
        self.block_count = 0;
        self.samples_since_hop = 0;
        self.gating_blocks.clear();
    }

    /// Ensure per-channel filter state + weights exist for `channels`.
    fn ensure_channels(&mut self, channels: u16) {
        let ch = channels.max(1) as usize;
        if self.channels != channels || self.state1.len() != ch {
            self.state1 = vec![BiquadState::default(); ch];
            self.state2 = vec![BiquadState::default(); ch];
            self.weights = (0..ch)
                .map(|i| if ch >= 5 && i >= 3 { 1.41 } else { 1.0 })
                .collect();
            self.channels = channels;
        }
    }

    /// Feed one block of the program (the monitored mix), interleaved over
    /// `channels`. Runs K-weighting per channel, accumulates the weighted
    /// mean-square per FRAME, and advances the momentary/short-term windows and
    /// the 400 ms gating blocks.
    pub fn push(&mut self, block: &[Sample], channels: u16) {
        let ch = channels.max(1) as usize;
        if block.is_empty() {
            return;
        }
        self.ensure_channels(channels);
        let frames = block.len() / ch;
        for f in 0..frames {
            // Sum the weighted K-weighted mean-square across channels for this
            // frame -> a single per-frame contribution z.
            let mut z = 0.0f64;
            for c in 0..ch {
                let x = block[f * ch + c] as f64;
                let y1 = self.stage1.process(x, &mut self.state1[c]);
                let y2 = self.stage2.process(y1, &mut self.state2[c]);
                z += self.weights[c] * (y2 * y2);
            }
            self.accumulate(z);
        }
    }

    /// Accumulate one per-frame mean-square contribution `z` into the momentary /
    /// short-term sliding windows and the gating-block hop.
    #[inline]
    fn accumulate(&mut self, z: f64) {
        // Momentary 400 ms ring (running sum for O(1) mean).
        self.mom_sum += z - self.mom_ring[self.mom_pos];
        self.mom_ring[self.mom_pos] = z;
        self.mom_pos = (self.mom_pos + 1) % self.mom_ring.len();
        if self.mom_filled < self.mom_ring.len() {
            self.mom_filled += 1;
        }
        // Short-term 3 s ring.
        self.st_sum += z - self.st_ring[self.st_pos];
        self.st_ring[self.st_pos] = z;
        self.st_pos = (self.st_pos + 1) % self.st_ring.len();
        if self.st_filled < self.st_ring.len() {
            self.st_filled += 1;
        }
        // Integrated: count frames toward a 400 ms block, emitting one mean-square
        // gating block every 100 ms hop once a full block of history exists.
        self.block_count += 1;
        self.samples_since_hop += 1;
        if self.samples_since_hop >= self.hop_len {
            self.samples_since_hop = 0;
            // Only emit once we have a full 400 ms of history.
            if self.block_count >= self.block_len {
                // Mean-square over the most recent block_len samples. We keep a
                // simple trailing-sum approximation via the short-term ring when
                // the block fits inside it; otherwise fall back to the running
                // block accumulator mean. For the canonical 400 ms block at any
                // supported rate the short-term ring (3 s) always contains it.
                let ms = self.trailing_mean_square(self.block_len);
                if ms > 0.0 {
                    self.gating_blocks.push(ms);
                }
            }
        }
    }

    /// Mean-square over the most recent `n` accumulated frames, taken from the
    /// short-term ring (which is >= any 400 ms block at supported rates).
    fn trailing_mean_square(&self, n: usize) -> f64 {
        let n = n.min(self.st_filled).min(self.st_ring.len());
        if n == 0 {
            return 0.0;
        }
        let len = self.st_ring.len();
        let mut sum = 0.0f64;
        // Walk back `n` slots from the last written position (st_pos points at the
        // NEXT write slot, so the newest sample is at st_pos-1).
        for k in 0..n {
            let idx = (self.st_pos + len - 1 - k) % len;
            sum += self.st_ring[idx];
        }
        sum / n as f64
    }

    /// LUFS from a mean-square value (BS.1770: -0.691 + 10 log10(ms)).
    #[inline]
    fn ms_to_lufs(ms: f64) -> f64 {
        if ms <= 0.0 {
            f64::NEG_INFINITY
        } else {
            LUFS_OFFSET + 10.0 * ms.log10()
        }
    }

    /// Read the current momentary / short-term / integrated loudness (LUFS).
    pub fn read(&self) -> LoudnessMeter {
        let lufs_m = if self.mom_filled > 0 {
            Self::ms_to_lufs(self.mom_sum / self.mom_filled as f64)
        } else {
            f64::NEG_INFINITY
        };
        let lufs_s = if self.st_filled > 0 {
            Self::ms_to_lufs(self.st_sum / self.st_filled as f64)
        } else {
            f64::NEG_INFINITY
        };
        let lufs_i = self.integrated_lufs();
        LoudnessMeter {
            lufs_m: lufs_m as f32,
            lufs_s: lufs_s as f32,
            lufs_i: lufs_i as f32,
        }
    }

    /// BS.1770-4 gated integrated loudness over the collected 400 ms blocks:
    ///   1. absolute gate at -70 LUFS,
    ///   2. compute the mean loudness of the surviving blocks,
    ///   3. relative gate at (that mean - 10 LU),
    ///   4. integrated loudness = LUFS of the mean-square of the twice-gated set.
    fn integrated_lufs(&self) -> f64 {
        if self.gating_blocks.is_empty() {
            return f64::NEG_INFINITY;
        }
        // Stage 1: absolute gate.
        let abs_kept: Vec<f64> = self
            .gating_blocks
            .iter()
            .copied()
            .filter(|&ms| Self::ms_to_lufs(ms) >= LUFS_ABSOLUTE_GATE)
            .collect();
        if abs_kept.is_empty() {
            return f64::NEG_INFINITY;
        }
        // Mean-square -> mean loudness of the absolute-gated set.
        let abs_mean_ms = abs_kept.iter().sum::<f64>() / abs_kept.len() as f64;
        let relative_threshold = Self::ms_to_lufs(abs_mean_ms) + LUFS_RELATIVE_GATE;
        // Stage 2: relative gate.
        let rel_kept: Vec<f64> = abs_kept
            .iter()
            .copied()
            .filter(|&ms| Self::ms_to_lufs(ms) >= relative_threshold)
            .collect();
        let kept = if rel_kept.is_empty() { abs_kept } else { rel_kept };
        let mean_ms = kept.iter().sum::<f64>() / kept.len() as f64;
        Self::ms_to_lufs(mean_ms)
    }
}

// ===========================================================================
// Spectrum — 2048-pt FFT folded to 96 log bands (SPEC §6)
// ===========================================================================

/// A stateful spectrum analyzer: a 2048-sample input ring, a Hann window, an
/// in-place radix-2 FFT, and the precomputed bin->band fold table. MODULE-AGENT
/// OWNED; the FFI spectrum getter reads [`read`].
#[derive(Debug, Clone)]
pub struct SpectrumState {
    /// The configured sample rate (sets the bin->Hz fold; retained for
    /// diagnostics + [`Self::sample_rate`]).
    sample_rate: u32,
    /// Sliding input ring of the last FFT_SIZE mono samples.
    ring: Vec<f32>,
    pos: usize,
    filled: usize,
    /// Hann window (length FFT_SIZE) and its coherent-gain normalization.
    window: Vec<f32>,
    window_sum: f32,
    /// For each FFT bin (0..=FFT_SIZE/2), the band index it folds into, or
    /// `usize::MAX` for bins outside the [20 Hz, Nyquist] analysis range.
    bin_band: Vec<usize>,
    /// Set once samples have been pushed; before that, `read` short-circuits to a
    /// silent frame instead of transforming an all-zero ring.
    dirty: bool,
}

impl Default for SpectrumState {
    fn default() -> Self {
        Self::new(DEFAULT_SAMPLE_RATE)
    }
}

impl SpectrumState {
    /// Construct for `sample_rate` (sets the bin->Hz mapping for the log fold).
    pub fn new(sample_rate: u32) -> Self {
        let fs = sample_rate.max(1);
        // Periodic Hann window (matches FFT bin spacing): w[n] = 0.5(1-cos(2πn/N)).
        let mut window = vec![0.0f32; FFT_SIZE];
        let mut window_sum = 0.0f32;
        for (n, w) in window.iter_mut().enumerate() {
            let v = 0.5 - 0.5 * (2.0 * PI * n as f32 / FFT_SIZE as f32).cos();
            *w = v;
            window_sum += v;
        }
        let bin_band = Self::build_fold_table(fs);
        Self {
            sample_rate: fs,
            ring: vec![0.0; FFT_SIZE],
            pos: 0,
            filled: 0,
            window,
            window_sum,
            bin_band,
            dirty: false,
        }
    }

    /// Build the bin->band fold table: 96 log-spaced bands from 20 Hz to the
    /// Nyquist. Each of the `FFT_SIZE/2 + 1` bins is assigned to the band whose
    /// log-frequency interval contains it; out-of-range bins map to `usize::MAX`.
    fn build_fold_table(sample_rate: u32) -> Vec<usize> {
        let half = FFT_SIZE / 2;
        let fs = sample_rate as f32;
        let bin_hz = fs / FFT_SIZE as f32;
        let f_lo = 20.0f32;
        let f_hi = (fs / 2.0).max(f_lo * 2.0);
        let log_lo = f_lo.ln();
        let log_hi = f_hi.ln();
        let span = (log_hi - log_lo).max(1e-6);
        let mut table = vec![usize::MAX; half + 1];
        for (bin, slot) in table.iter_mut().enumerate() {
            let f = bin as f32 * bin_hz;
            if f < f_lo || f > f_hi {
                continue;
            }
            let frac = (f.ln() - log_lo) / span;
            let mut band = (frac * SPECTRUM_BANDS as f32).floor() as isize;
            if band < 0 {
                band = 0;
            }
            if band >= SPECTRUM_BANDS as isize {
                band = SPECTRUM_BANDS as isize - 1;
            }
            *slot = band as usize;
        }
        table
    }

    /// The sample rate this analyzer is configured for.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Feed mono samples (the monitored mix summed to mono) into the ring.
    pub fn push(&mut self, mono: &[Sample]) {
        if mono.is_empty() {
            return;
        }
        for &s in mono {
            self.ring[self.pos] = s;
            self.pos = (self.pos + 1) % FFT_SIZE;
            if self.filled < FFT_SIZE {
                self.filled += 1;
            }
        }
        self.dirty = true;
    }

    /// Read the current 96-band spectrum in dBFS. Computes the windowed FFT over
    /// the most recent FFT_SIZE samples, folds bin magnitudes into the log bands,
    /// and converts to dBFS. Bands with no energy read -inf.
    pub fn read(&self) -> SpectrumFrame {
        if !self.dirty && self.filled == 0 {
            return SpectrumFrame::default();
        }
        self.compute_frame()
    }

    /// Compute one spectrum frame from the current ring contents.
    fn compute_frame(&self) -> SpectrumFrame {
        // Assemble the time-domain frame in chronological order from the ring.
        // The oldest sample is at `pos` (the next write slot) once filled; before
        // the ring fills, samples 0..filled are the data and the rest is zero
        // (zero-padding, which is fine for a magnitude spectrum).
        let mut re = vec![0.0f32; FFT_SIZE];
        let mut im = vec![0.0f32; FFT_SIZE];
        if self.filled >= FFT_SIZE {
            for n in 0..FFT_SIZE {
                let idx = (self.pos + n) % FFT_SIZE;
                re[n] = self.ring[idx] * self.window[n];
            }
        } else {
            // Not yet full: place the `filled` samples (written at indices
            // 0..pos) at the END of the window so the newest data is windowed
            // most heavily. Leading zeros pad the front.
            let start = FFT_SIZE - self.filled;
            for n in 0..self.filled {
                re[start + n] = self.ring[n] * self.window[start + n];
            }
        }

        fft_radix2(&mut re, &mut im);

        // Magnitude -> per-band peak energy. Normalize by the window's coherent
        // gain (sum of window) and the single-sided convention (×2) so a full-scale
        // sine reads ~0 dBFS in its band. The ×2 applies ONLY to interior positive-
        // frequency bins (1..half): DC (bin 0) and Nyquist (bin half) are
        // self-mirrored (no conjugate partner) and must use ×1, else a pure Nyquist
        // tone over-reads its (top) band by 6 dB. DC is already dropped from the
        // bands (bin_band == usize::MAX); halving it too keeps the factor correct.
        let half = FFT_SIZE / 2;
        let norm = 2.0 / self.window_sum.max(1e-12);
        let mut band_max = [0.0f32; SPECTRUM_BANDS];
        for bin in 0..=half {
            let band = self.bin_band[bin];
            if band == usize::MAX {
                continue;
            }
            let factor = if bin == 0 || bin == half { norm * 0.5 } else { norm };
            let mag = (re[bin] * re[bin] + im[bin] * im[bin]).sqrt() * factor;
            if mag > band_max[band] {
                band_max[band] = mag;
            }
        }

        let mut frame = SpectrumFrame::default();
        for (b, &m) in band_max.iter().enumerate() {
            frame.bands[b] = linear_to_db(m);
        }
        frame
    }
}

/// In-place iterative radix-2 Cooley-Tukey FFT (decimation-in-time). `re`/`im`
/// hold the complex signal; both have length [`FFT_SIZE`] (a power of two — this
/// asserts it). Hand-written, std-only, no allocation beyond the caller's
/// buffers (per the crate's dependency policy: the FFT is hand-rolled so nothing
/// pulls a syscall or surprise alloc onto the realtime-adjacent path).
fn fft_radix2(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    debug_assert_eq!(n, im.len());
    debug_assert!(n.is_power_of_two(), "FFT length must be a power of two");
    if n <= 1 {
        return;
    }

    // Bit-reversal permutation.
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }

    // Butterflies, doubling the transform length each stage.
    let mut len = 2usize;
    while len <= n {
        let ang = -2.0 * PI / len as f32;
        let wlen_re = ang.cos();
        let wlen_im = ang.sin();
        let mut i = 0usize;
        while i < n {
            let mut w_re = 1.0f32;
            let mut w_im = 0.0f32;
            for k in 0..len / 2 {
                let a = i + k;
                let b = i + k + len / 2;
                let t_re = w_re * re[b] - w_im * im[b];
                let t_im = w_re * im[b] + w_im * re[b];
                let u_re = re[a];
                let u_im = im[a];
                re[a] = u_re + t_re;
                im[a] = u_im + t_im;
                re[b] = u_re - t_re;
                im[b] = u_im - t_im;
                // Advance the twiddle factor w *= wlen.
                let nw_re = w_re * wlen_re - w_im * wlen_im;
                let nw_im = w_re * wlen_im + w_im * wlen_re;
                w_re = nw_re;
                w_im = nw_im;
            }
            i += len;
        }
        len <<= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AudioFormat;

    /// Generate `n` samples of a sine at `freq` Hz, amplitude `amp`, mono.
    fn sine(freq: f32, amp: f32, n: usize, fs: u32) -> Vec<f32> {
        (0..n)
            .map(|i| amp * (2.0 * PI * freq * i as f32 / fs as f32).sin())
            .collect()
    }

    // --- peak / RMS ---------------------------------------------------------

    #[test]
    fn block_meter_measures_peak_and_rms() {
        // A full-scale constant block of 1.0 reads ~0 dBFS peak and ~0 dBFS RMS.
        let buf = vec![1.0f32; 64];
        let block = BlockRef { data: &buf, format: AudioFormat::new(1, 48_000) };
        let m = block_meter(&block, 0);
        assert!((m.peak_dbfs).abs() < 1e-4);
        assert!((m.rms_dbfs).abs() < 1e-4);
        // Silence reads -inf, not NaN.
        let z = vec![0.0f32; 64];
        let zb = BlockRef { data: &z, format: AudioFormat::new(1, 48_000) };
        assert!(block_meter(&zb, 0).peak_dbfs.is_infinite());
    }

    #[test]
    fn full_scale_sine_reads_zero_dbfs_peak_and_minus3_rms() {
        // A full-scale sine: peak ~0 dBFS, RMS ~ -3.01 dBFS (1/sqrt(2)).
        let fs = 48_000;
        let buf = sine(1000.0, 1.0, fs as usize, fs); // 1 s, integer cycles-ish
        let block = BlockRef { data: &buf, format: AudioFormat::new(1, fs) };
        let m = block_meter(&block, 0);
        assert!((m.peak_dbfs - 0.0).abs() < 0.05, "peak {} dBFS", m.peak_dbfs);
        assert!((m.rms_dbfs - (-3.0103)).abs() < 0.05, "rms {} dBFS", m.rms_dbfs);
    }

    #[test]
    fn known_rms_signal_matches_expected() {
        // A 0.5-amplitude sine: RMS = 0.5/sqrt(2) ~ 0.35355 -> -9.03 dBFS.
        let fs = 48_000;
        let buf = sine(440.0, 0.5, fs as usize, fs);
        let block = BlockRef { data: &buf, format: AudioFormat::new(1, fs) };
        let m = block_meter(&block, 0);
        let expected = 20.0 * (0.5 / 2.0f32.sqrt()).log10();
        assert!((m.rms_dbfs - expected).abs() < 0.05, "rms {} vs {}", m.rms_dbfs, expected);
    }

    #[test]
    fn block_meter_selects_the_right_interleaved_lane() {
        // Stereo: left = 1.0, right = 0.5. Each lane meters independently.
        let mut buf = vec![0.0f32; 64 * 2];
        for f in 0..64 {
            buf[f * 2] = 1.0;
            buf[f * 2 + 1] = 0.5;
        }
        let block = BlockRef { data: &buf, format: AudioFormat::new(2, 48_000) };
        let l = block_meter(&block, 0);
        let r = block_meter(&block, 1);
        assert!((l.peak_dbfs).abs() < 1e-4);
        assert!((r.peak_dbfs - 20.0 * 0.5f32.log10()).abs() < 1e-3);
    }

    // --- true-peak / clip ---------------------------------------------------

    #[test]
    fn clip_detect_fires_above_threshold() {
        // A constant 0 dBFS block (1.0) is above the -1 dBFS clip threshold.
        let hot = vec![1.0f32; 8];
        let hb = BlockRef { data: &hot, format: AudioFormat::new(1, 48_000) };
        assert!(detect_clip(&hb, 0).is_some());
        // A quiet block does not clip.
        let quiet = vec![0.1f32; 8];
        let qb = BlockRef { data: &quiet, format: AudioFormat::new(1, 48_000) };
        assert!(detect_clip(&qb, 0).is_none());
    }

    #[test]
    fn true_peak_exceeds_sample_peak_on_intersample_signal() {
        // The classic inter-sample-peak case: a sine at fs/4 sampled so the grid
        // straddles the crest. Construct +/-A at alternating-ish phase whose true
        // peak rises above the sample peak. We use 0.95 * a 0/+/0/- pattern at
        // fs/4 which is the canonical inter-sample overshoot tone.
        let fs = 48_000u32;
        // Sine at fs/4, phase 45° so samples land at +/-0.707*A, true peak = A.
        let a = 0.9f32;
        let n = 64usize;
        let buf: Vec<f32> = (0..n)
            .map(|i| {
                let ph = 2.0 * PI * (fs as f32 / 4.0) * i as f32 / fs as f32 + PI / 4.0;
                a * ph.sin()
            })
            .collect();
        let block = BlockRef { data: &buf, format: AudioFormat::new(1, fs) };

        // Sample peak on the grid.
        let sample_peak = buf.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        let kernel = TruePeakKernel::design();
        let tp = true_peak_linear(&block, 0, &kernel);
        assert!(
            tp > sample_peak + 1e-3,
            "true peak {} should exceed sample peak {}",
            tp,
            sample_peak
        );
        // And the true peak should be near the real signal amplitude A.
        assert!((tp - a).abs() < 0.08, "true peak {} vs A {}", tp, a);
    }

    #[test]
    fn true_peak_clip_catches_intersample_over() {
        // A signal whose SAMPLE peak is just under 0 dBFS but whose true (inter-
        // sample) peak crosses -1 dBFS must trip the clip detector even though a
        // naive sample-peak check at the same threshold would not.
        let fs = 48_000u32;
        let a = 0.98f32; // ~ -0.18 dBFS amplitude; true-peak above -1 dBFS
        let n = 128usize;
        let buf: Vec<f32> = (0..n)
            .map(|i| {
                let ph = 2.0 * PI * (fs as f32 / 4.0) * i as f32 / fs as f32 + PI / 4.0;
                a * ph.sin()
            })
            .collect();
        let block = BlockRef { data: &buf, format: AudioFormat::new(1, fs) };
        let ev = detect_clip(&block, 0);
        assert!(ev.is_some(), "inter-sample over should be detected as a clip");
        let ev = ev.unwrap();
        assert!(ev.true_peak_dbfs >= CLIP_THRESHOLD_DBFS);
    }

    #[test]
    fn no_clip_on_clean_minus6_signal() {
        // A clean sine well below the threshold (-6 dBFS) never clips.
        let fs = 48_000u32;
        let buf = sine(1000.0, 0.5, 256, fs); // -6 dBFS amplitude
        let block = BlockRef { data: &buf, format: AudioFormat::new(1, fs) };
        assert!(detect_clip(&block, 0).is_none());
    }

    // --- BS.1770-4 LUFS -----------------------------------------------------

    #[test]
    fn lufs_integrated_matches_reference_minus23_tone() {
        // BS.1770-4 / EBU R128: a 1 kHz sine at -23 dBFS RMS on a mono channel
        // reads ~ -23 LUFS integrated (the K-weighting is ~flat at 1 kHz, and the
        // single-channel weight is 1.0). Feed several seconds so the integrator
        // has many gating blocks.
        let fs = 48_000u32;
        // -23 dBFS RMS sine => amplitude = sqrt(2) * 10^(-23/20).
        let target_rms_db = -23.0f32;
        let amp = 2.0f32.sqrt() * 10f32.powf(target_rms_db / 20.0);
        let secs = 6;
        let buf = sine(1000.0, amp, fs as usize * secs, fs);
        let mut m = LoudnessMeterState::new(fs);
        // Feed in ~100 ms chunks to exercise the streaming path.
        let chunk = (fs / 10) as usize;
        for c in buf.chunks(chunk) {
            m.push(c, 1);
        }
        let l = m.read();
        // BS.1770-4 compliance tolerance: within +/- 0.5 LU of -23.
        assert!(
            (l.lufs_i - (-23.0)).abs() < 0.5,
            "LUFS-I {} should be ~ -23",
            l.lufs_i
        );
        // Momentary and short-term should also land near -23 on a steady tone.
        assert!((l.lufs_m - (-23.0)).abs() < 0.7, "LUFS-M {}", l.lufs_m);
        assert!((l.lufs_s - (-23.0)).abs() < 0.7, "LUFS-S {}", l.lufs_s);
    }

    #[test]
    fn lufs_silence_is_negative_infinity_not_nan() {
        let fs = 48_000u32;
        let mut m = LoudnessMeterState::new(fs);
        let silence = vec![0.0f32; fs as usize];
        m.push(&silence, 1);
        let l = m.read();
        assert!(l.lufs_m.is_infinite() && l.lufs_m.is_sign_negative());
        // Silence never produces a gated block above -70, so integrated is -inf.
        assert!(l.lufs_i.is_infinite() && l.lufs_i.is_sign_negative());
        assert!(!l.lufs_i.is_nan());
    }

    #[test]
    fn lufs_relative_gate_excludes_quiet_passages() {
        // A program that is loud for a while then near-silent: the integrated
        // loudness must track the LOUD section (the -10 LU relative gate drops the
        // near-silent blocks), so LUFS-I stays near the loud level, not the
        // average of loud+silence.
        let fs = 48_000u32;
        let amp = 2.0f32.sqrt() * 10f32.powf(-20.0 / 20.0); // -20 dBFS RMS loud part
        let loud = sine(1000.0, amp, fs as usize * 4, fs);
        let quiet = sine(1000.0, amp * 0.01, fs as usize * 8, fs); // ~ -60 dB, gated out
        let mut m = LoudnessMeterState::new(fs);
        for c in loud.chunks((fs / 10) as usize) {
            m.push(c, 1);
        }
        for c in quiet.chunks((fs / 10) as usize) {
            m.push(c, 1);
        }
        let l = m.read();
        // Should be near the loud level (~ -20 LUFS), not pulled down toward -60.
        assert!(l.lufs_i > -25.0, "LUFS-I {} should reflect the loud section", l.lufs_i);
    }

    #[test]
    fn lufs_louder_signal_reads_higher() {
        // Monotonicity: a +6 dB louder tone reads ~6 LU higher.
        let fs = 48_000u32;
        let quiet_amp = 2.0f32.sqrt() * 10f32.powf(-23.0 / 20.0);
        let loud_amp = quiet_amp * 2.0; // +6 dB
        let mut mq = LoudnessMeterState::new(fs);
        let mut ml = LoudnessMeterState::new(fs);
        let q = sine(1000.0, quiet_amp, fs as usize * 5, fs);
        let p = sine(1000.0, loud_amp, fs as usize * 5, fs);
        for c in q.chunks((fs / 10) as usize) {
            mq.push(c, 1);
        }
        for c in p.chunks((fs / 10) as usize) {
            ml.push(c, 1);
        }
        let diff = ml.read().lufs_i - mq.read().lufs_i;
        assert!((diff - 6.0206).abs() < 0.3, "delta {} should be ~6 LU", diff);
    }

    // --- spectrum / FFT -----------------------------------------------------

    #[test]
    fn fft_matches_naive_dft_on_small_signal() {
        // Validate the radix-2 FFT against a direct DFT on a short power-of-two
        // signal (so a bug in the butterfly/bit-reversal is caught precisely).
        let n = 16usize;
        let sig: Vec<f32> = (0..n).map(|i| (i as f32 * 0.37).sin() + 0.5).collect();
        let mut re: Vec<f32> = sig.clone();
        let mut im = vec![0.0f32; n];
        fft_radix2(&mut re, &mut im);
        for k in 0..n {
            let mut dre = 0.0f32;
            let mut dim = 0.0f32;
            for (t, &x) in sig.iter().enumerate() {
                let ang = -2.0 * PI * (k * t) as f32 / n as f32;
                dre += x * ang.cos();
                dim += x * ang.sin();
            }
            assert!((re[k] - dre).abs() < 1e-3, "bin {k} re {} vs {}", re[k], dre);
            assert!((im[k] - dim).abs() < 1e-3, "bin {k} im {} vs {}", im[k], dim);
        }
    }

    #[test]
    fn single_bin_sine_lands_in_the_right_band() {
        // A sine exactly on an FFT bin puts essentially all energy in one band.
        // Choose a frequency at bin 256 of a 2048-pt FFT @ 48 kHz: f = 256*48000/2048
        // = 6000 Hz. Verify the band holding 6 kHz is the loudest and reads ~0 dBFS.
        let fs = 48_000u32;
        let bin = 256usize;
        let freq = bin as f32 * fs as f32 / FFT_SIZE as f32; // 6000 Hz
        let mut s = SpectrumState::new(fs);
        // Feed >= FFT_SIZE samples of a full-scale sine.
        let buf = sine(freq, 1.0, FFT_SIZE * 2, fs);
        s.push(&buf);
        let frame = s.read();

        // Find the loudest band and confirm it is the band 6 kHz folds into.
        let (loud_band, &loud_db) = frame
            .bands
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap();
        let expected_band = s.bin_band[bin];
        assert_eq!(loud_band, expected_band, "energy in wrong band");
        // Full-scale sine on-bin should read close to 0 dBFS in that band.
        assert!(loud_db > -3.0, "on-bin full-scale sine band {} dBFS", loud_db);
        // Bands far from the tone should be much quieter.
        let far = frame.bands[(expected_band + 30) % SPECTRUM_BANDS];
        assert!(far < loud_db - 20.0, "off-band {} not well below peak {}", far, loud_db);
    }

    /// REGRESSION: a full-scale NYQUIST tone (alternating ±1.0 = cos at fs/2, true
    /// amplitude 1.0) must read ~0 dBFS in the top band, NOT +6 dB. The single-sided
    /// ×2 convention applies only to interior bins with a conjugate mirror; the
    /// self-mirrored Nyquist bin was wrongly doubled, over-reporting by 6 dB.
    #[test]
    fn spectrum_nyquist_tone_is_not_over_reported_by_6db() {
        let fs = 48_000u32;
        let mut s = SpectrumState::new(fs);
        // cos(π·n) = (-1)^n — the full-scale Nyquist tone (a plain sine at fs/2 is
        // identically 0, so it can't exercise the Nyquist bin).
        let buf: Vec<f32> = (0..FFT_SIZE * 2)
            .map(|n| if n % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        s.push(&buf);
        let top = s.read().bands[SPECTRUM_BANDS - 1]; // Nyquist folds into the top band
        assert!(top > -3.0, "full-scale Nyquist tone reads too quiet: {top} dBFS");
        assert!(
            top < 3.0,
            "Nyquist bin over-reported by the single-sided x2 bug: {top} dBFS (expected ~0)"
        );
    }

    #[test]
    fn spectrum_silence_reads_negative_infinity() {
        let fs = 48_000u32;
        let mut s = SpectrumState::new(fs);
        let silence = vec![0.0f32; FFT_SIZE];
        s.push(&silence);
        let frame = s.read();
        assert!(frame.bands.iter().all(|b| b.is_infinite() && b.is_sign_negative()));
    }

    #[test]
    fn spectrum_low_tone_below_high_tone_bands() {
        // A 200 Hz tone and a 5 kHz tone land in clearly different (and correctly
        // ordered) bands: the 200 Hz band index < the 5 kHz band index.
        let fs = 48_000u32;
        let mut lo = SpectrumState::new(fs);
        let mut hi = SpectrumState::new(fs);
        lo.push(&sine(200.0, 1.0, FFT_SIZE * 2, fs));
        hi.push(&sine(5000.0, 1.0, FFT_SIZE * 2, fs));
        let lo_band = lo
            .read()
            .bands
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap()
            .0;
        let hi_band = hi
            .read()
            .bands
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap()
            .0;
        assert!(lo_band < hi_band, "200 Hz band {} should be below 5 kHz band {}", lo_band, hi_band);
    }

    #[test]
    fn default_constructors_are_silent_and_valid() {
        // The engine constructs these via Default/new; both must yield silent,
        // NaN-free readings before any data is pushed.
        let l = LoudnessMeterState::default().read();
        assert!(l.lufs_i.is_infinite() && !l.lufs_i.is_nan());
        let f = SpectrumState::default().read();
        assert!(f.bands.iter().all(|b| b.is_infinite()));
        // SPECTRUM_BANDS_HINT pins to the frozen band count.
        assert_eq!(SPECTRUM_BANDS_HINT, SPECTRUM_BANDS);
        assert_eq!(SpectrumFrame::default().bands.len(), 96);
        // The configured rate is retained and reported.
        assert_eq!(LoudnessMeterState::new(44_100).sample_rate(), 44_100);
        assert_eq!(SpectrumState::new(44_100).sample_rate(), 44_100);
    }

    #[test]
    fn loudness_reset_clears_integration() {
        // After integrating a loud tone, reset() returns the meter to silence.
        let fs = 48_000u32;
        let amp = 2.0f32.sqrt() * 10f32.powf(-23.0 / 20.0);
        let mut m = LoudnessMeterState::new(fs);
        for c in sine(1000.0, amp, fs as usize * 4, fs).chunks((fs / 10) as usize) {
            m.push(c, 1);
        }
        assert!(m.read().lufs_i.is_finite());
        m.reset();
        let l = m.read();
        assert!(l.lufs_i.is_infinite() && l.lufs_m.is_infinite() && l.lufs_s.is_infinite());
    }
}

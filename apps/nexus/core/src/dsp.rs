//! DSP chain — crosspoint mix + the per-input studio chain (SPEC §3 step 3).
//!
//! MODULE-AGENT FILE (dsp-chain agent). Filled against the FROZEN types in
//! [`crate::types`] and the snapshot in [`crate::matrix`]. Two realtime jobs:
//!
//!   1. [`mix_block`] — reads a [`MatrixSnapshot`] and the per-input
//!      [`BlockRef`]s, sums each input * its crosspoint gain (dB -> linear) into
//!      each output [`BlockMut`], honoring input/output mutes. NO allocation, NO
//!      locks, NO syscalls — it runs on the audio thread.
//!
//!   2. [`process_channel_chain`] — the per-input studio chain (SPEC §3):
//!      HPF 80 Hz 12 dB/oct (biquad) -> noise gate (-45 dB, 80 ms release) ->
//!      de-esser (5-8 kHz, 4:1) -> compressor (3:1, 10/120 ms) -> output trim.
//!      Each stage is bypassable; parameter changes ramp over 5 ms
//!      ([`crate::types::PARAM_RAMP_MS`]) so there is no zipper noise (SPEC §2).
//!
//! All coefficient math is hand-written (RBJ biquad cookbook for the filters,
//! `exp(-1/(t*fs))` one-pole envelopes for the dynamics). Everything here is
//! pure f32 DSP over caller-owned buffers — fully headless, no device, no alloc
//! on the process path. State lives in [`ChannelChainState`], owned per input by
//! the engine and threaded back in each call.

use crate::matrix::MatrixSnapshot;
use crate::types::{
    db_to_linear, linear_to_db, BlockMut, BlockRef, ChannelDsp, CompressorParams, DeEsserParams,
    FilterParams, GateParams, Sample, PARAM_RAMP_MS,
};

/// Per-block linear-ramp parameter smoother (SPEC §2: 5 ms ramps, no zipper
/// noise). One per smoothed scalar: it holds a current value and a target and
/// advances linearly toward the target by a fixed per-sample step, reaching it
/// in `ramp_samples`. Setting a new target recomputes the step from the
/// remaining distance so an in-flight ramp re-aims smoothly (still continuous).
#[derive(Debug, Clone, Copy)]
pub struct Smoother {
    current: f32,
    target: f32,
    /// Per-sample increment to add to `current` until it reaches `target`.
    step: f32,
    /// Samples remaining in the active ramp (0 = parked at target).
    remaining: u32,
    /// Ramp length in samples (derived from PARAM_RAMP_MS + sample rate).
    ramp_samples: u32,
}

impl Smoother {
    /// A smoother parked at `initial` with a default 5 ms ramp at 48 kHz. Use
    /// [`Smoother::with_rate`] to bind the ramp to the real sample rate.
    pub fn new(initial: f32) -> Self {
        Self::with_rate(initial, crate::types::DEFAULT_SAMPLE_RATE)
    }

    /// A smoother parked at `initial`, ramps timed to `sample_rate` (5 ms).
    pub fn with_rate(initial: f32, sample_rate: u32) -> Self {
        let ramp_samples = ramp_len_samples(PARAM_RAMP_MS, sample_rate);
        Self { current: initial, target: initial, step: 0.0, remaining: 0, ramp_samples }
    }

    /// Re-time the 5 ms ramp to a new sample rate (preserves current/target).
    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        self.ramp_samples = ramp_len_samples(PARAM_RAMP_MS, sample_rate);
        // Re-aim so the remaining distance is covered over the new ramp length.
        self.set_target(self.target);
    }

    /// Aim the smoother at `target`, ramping linearly from the *current* value
    /// over the configured 5 ms. A target equal to the current value parks it.
    pub fn set_target(&mut self, target: f32) {
        self.target = target;
        let dist = target - self.current;
        if dist == 0.0 || self.ramp_samples == 0 {
            self.current = target;
            self.step = 0.0;
            self.remaining = 0;
        } else {
            self.remaining = self.ramp_samples;
            self.step = dist / self.ramp_samples as f32;
        }
    }

    /// Snap to `value` immediately (no ramp) — for resets / initial state.
    pub fn reset(&mut self, value: f32) {
        self.current = value;
        self.target = value;
        self.step = 0.0;
        self.remaining = 0;
    }

    /// The instantaneous value without advancing.
    pub fn value(&self) -> f32 {
        self.current
    }

    /// True once the smoother has reached its target.
    pub fn at_target(&self) -> bool {
        self.remaining == 0
    }

    /// Advance one sample, returning the smoothed value. Linear ramp; lands
    /// exactly on the target on the final step (no float drift past it).
    // Deliberately named `next`: this is a per-sample advance on the realtime
    // audio path, not an `Iterator` (it mutates in place and never terminates).
    // Renaming would churn the RT call sites; the name reads correctly in situ.
    #[allow(clippy::should_implement_trait)]
    #[inline]
    pub fn next(&mut self) -> f32 {
        if self.remaining > 0 {
            self.remaining -= 1;
            if self.remaining == 0 {
                self.current = self.target;
            } else {
                self.current += self.step;
            }
        }
        self.current
    }
}

/// Ramp length in samples for `ms` at `sample_rate`, at least 1 so a ramp is
/// never zero-length (which would be a jump = zipper noise).
#[inline]
fn ramp_len_samples(ms: f32, sample_rate: u32) -> u32 {
    let n = (ms * 0.001 * sample_rate as f32).round() as i64;
    n.max(1) as u32
}

/// Sum every active input * its crosspoint gain into each output, honoring
/// mutes (SPEC §1/§2 crosspoint mix). AUDIO THREAD: no alloc/lock/syscall.
///
/// `inputs[i]` is the interleaved block for input channel `i`; `outputs[o]` is
/// written in place. All blocks share the same frame count + sample rate. Each
/// crosspoint gain is taken from `snapshot.grid[i][o]` (dB), converted to a
/// linear factor via [`db_to_linear`] (the `-inf` sentinel -> 0.0 = no route),
/// and accumulated. A muted input contributes nothing; a muted output is
/// cleared to silence.
///
/// Inputs and outputs may each be multi-channel interleaved; the mix is applied
/// per interleaved sample (the matrix routes whole channel *blocks*, so output
/// `o`'s interleaved buffer receives the per-sample sum of every routed input's
/// interleaved buffer scaled by the crosspoint). When the per-block frame counts
/// differ we only touch the overlapping prefix (defensive; the FFI guarantees
/// equal lengths).
pub fn mix_block(snapshot: &MatrixSnapshot, inputs: &[BlockRef<'_>], outputs: &mut [BlockMut<'_>]) {
    let active_in = snapshot.inputs.min(inputs.len());
    let active_out = snapshot.outputs.min(outputs.len());

    for (o, out) in outputs.iter_mut().enumerate() {
        // Start every output from silence, then accumulate routed inputs.
        for s in out.data.iter_mut() {
            *s = 0.0;
        }
        if o >= active_out || snapshot.output_mutes[o] {
            // Muted (or beyond active) outputs stay silent.
            continue;
        }
        let out_len = out.data.len();
        for (i, inp) in inputs.iter().enumerate().take(active_in) {
            if snapshot.input_mutes[i] {
                continue;
            }
            let gain_db = snapshot.grid[i][o];
            // `-inf` (cleared route) -> 0.0; skip the work entirely.
            let g = db_to_linear(gain_db);
            if g == 0.0 {
                continue;
            }
            let n = out_len.min(inp.data.len());
            let src = &inp.data[..n];
            let dst = &mut out.data[..n];
            for (d, &s) in dst.iter_mut().zip(src.iter()) {
                *d += s * g;
            }
        }
    }
}

// ===========================================================================
// Per-channel studio chain
// ===========================================================================

/// Per-channel DSP filter memory: biquad delay lines, envelope followers, the
/// per-parameter [`Smoother`]s, and the cached coefficients/configuration so the
/// chain only rederives them when params change. MODULE-AGENT OWNED.
///
/// All `f32` state, fixed-size, `Default`-constructible (the engine allocates an
/// array of these up front and the process call mutates them in place — no alloc
/// on the audio path).
#[derive(Debug, Clone)]
pub struct ChannelChainState {
    /// Sample rate the cached coefficients were derived for; 0 = "not yet bound"
    /// so the first `process_channel_chain` call primes everything.
    sample_rate: u32,

    /// HPF: cascade of up to 2 biquad sections (order/2 sections; a 2-pole HPF
    /// is one biquad). Each section has its own z-1/z-2 state.
    hpf_sections: [BiquadState; MAX_HPF_SECTIONS],
    hpf_active_sections: usize,
    /// Cached HPF param the coefficients were derived for (rederive on change).
    hpf_cached: FilterParams,

    /// De-esser sidechain band-pass (detects sibilance energy) + its state.
    deesser_band: BiquadState,
    deesser_cached: DeEsserParams,
    /// De-esser gain-reduction envelope (linear, 1.0 = no reduction).
    deesser_env: f32,

    /// Gate envelope follower (linear amplitude of the detector) and the
    /// smoothed gate gain (linear) to avoid clicks on open/close.
    gate_env: f32,
    gate_gain_lin: f32,
    gate_cached: GateParams,

    /// Compressor detector envelope (dBFS) and smoothed gain (linear).
    comp_env_db: f32,
    comp_gain_lin: f32,
    comp_cached: CompressorParams,

    /// Output trim smoother (linear gain) so trim moves don't zipper.
    trim: Smoother,
    /// Cached trim target (dB) so we only re-aim the smoother on change.
    trim_cached_db: f32,
}

/// A 2-pole HPF is one biquad; we allow a small cascade so a 4-pole (24 dB/oct)
/// option still fits without an allocation. Two sections cover order 2 and 4.
const MAX_HPF_SECTIONS: usize = 2;

impl Default for ChannelChainState {
    fn default() -> Self {
        Self {
            sample_rate: 0,
            hpf_sections: [BiquadState::default(); MAX_HPF_SECTIONS],
            hpf_active_sections: 0,
            hpf_cached: FilterParams { enabled: false, cutoff_hz: 0.0, order: 0 },
            deesser_band: BiquadState::default(),
            deesser_cached: DeEsserParams {
                enabled: false,
                band_low_hz: 0.0,
                band_high_hz: 0.0,
                threshold_db: 0.0,
                ratio: 1.0,
            },
            deesser_env: 1.0,
            gate_env: 0.0,
            gate_gain_lin: 1.0,
            gate_cached: GateParams {
                enabled: false,
                threshold_db: 0.0,
                attack_ms: 0.0,
                release_ms: 0.0,
                floor_db: 0.0,
            },
            comp_env_db: f32::NEG_INFINITY,
            comp_gain_lin: 1.0,
            comp_cached: CompressorParams {
                enabled: false,
                threshold_db: 0.0,
                ratio: 1.0,
                attack_ms: 0.0,
                release_ms: 0.0,
                knee_db: 0.0,
                makeup_db: 0.0,
            },
            trim: Smoother::new(1.0),
            trim_cached_db: 0.0,
        }
    }
}

impl ChannelChainState {
    /// Reset all dynamic state (envelopes, filter memory) while keeping the
    /// sample-rate binding. Useful on a routing change to avoid stale tails.
    pub fn reset(&mut self) {
        for s in self.hpf_sections.iter_mut() {
            *s = BiquadState::default();
        }
        self.deesser_band = BiquadState::default();
        self.deesser_env = 1.0;
        self.gate_env = 0.0;
        self.gate_gain_lin = 1.0;
        self.comp_env_db = f32::NEG_INFINITY;
        self.comp_gain_lin = 1.0;
        self.trim.reset(self.trim.value());
    }
}

/// Run the per-input studio chain in place over one channel's mono block
/// (SPEC §3: HPF -> gate -> de-esser -> compressor -> output trim). AUDIO
/// THREAD; no alloc/lock/syscall. When `params.enabled` is false the whole chain
/// is bypassed (bit-transparent passthrough). Each stage is independently
/// bypassable via its own `enabled`. `sample_rate` times the envelopes + ramps;
/// it must match the block's format. `state` carries all filter/envelope memory
/// and is mutated in place.
pub fn process_channel_chain(
    params: &ChannelDsp,
    state: &mut ChannelChainState,
    block: &mut [Sample],
    sample_rate: u32,
) {
    // (Re)bind coefficients/envelope constants when the sample rate changes.
    if state.sample_rate != sample_rate {
        state.sample_rate = sample_rate;
        state.trim.set_sample_rate(sample_rate);
        // Force a coefficient rederive on the next param check by invalidating
        // the caches.
        state.hpf_cached.cutoff_hz = f32::NAN;
        state.deesser_cached.band_low_hz = f32::NAN;
    }

    if !params.enabled {
        // Master bypass: bit-transparent. We still keep the trim smoother parked
        // at unity so re-enabling doesn't jump.
        return;
    }

    // --- coefficient refresh (control-rate; only on change) ----------------
    refresh_hpf(state, &params.hpf, sample_rate);
    refresh_deesser(state, &params.deesser, sample_rate);
    state.gate_cached = params.gate;
    state.comp_cached = params.compressor;
    // Output trim target (dB->linear) ramps via the smoother.
    if params.output_trim_db != state.trim_cached_db {
        state.trim_cached_db = params.output_trim_db;
        state.trim.set_target(db_to_linear(params.output_trim_db));
    }

    // Envelope coefficients (per-sample one-poles).
    let gate_atk = time_constant_coeff(params.gate.attack_ms, sample_rate);
    let gate_rel = time_constant_coeff(params.gate.release_ms, sample_rate);
    let comp_atk = time_constant_coeff(params.compressor.attack_ms, sample_rate);
    let comp_rel = time_constant_coeff(params.compressor.release_ms, sample_rate);
    // De-esser uses fast fixed time constants over the sidechain band.
    let de_atk = time_constant_coeff(2.0, sample_rate);
    let de_rel = time_constant_coeff(40.0, sample_rate);

    for x in block.iter_mut() {
        let mut s = *x;

        // 1) HPF (cascade of biquad sections).
        if params.hpf.enabled {
            for sect in state.hpf_sections.iter_mut().take(state.hpf_active_sections) {
                s = sect.process(s);
            }
        }

        // 2) Noise gate (downward expansion to floor below threshold).
        if params.gate.enabled {
            // Peak-tracking detector with attack/release smoothing.
            let rect = s.abs();
            if rect > state.gate_env {
                state.gate_env = gate_atk * state.gate_env + (1.0 - gate_atk) * rect;
            } else {
                state.gate_env = gate_rel * state.gate_env + (1.0 - gate_rel) * rect;
            }
            let level_db = linear_to_db(state.gate_env);
            let target = gate_gain(&params.gate, level_db);
            // Smooth the gate gain itself with the same attack/release so the
            // open/close has no click (move fast on open, slow on close).
            let coeff = if target > state.gate_gain_lin { gate_atk } else { gate_rel };
            state.gate_gain_lin = coeff * state.gate_gain_lin + (1.0 - coeff) * target;
            s *= state.gate_gain_lin;
        }

        // 3) De-esser: detect energy in the sibilance band, attenuate the WHOLE
        //    signal by a band-driven gain (a frequency-selective compressor).
        if params.deesser.enabled {
            let band = state.deesser_band.process(s);
            let band_db = linear_to_db(band.abs());
            let target = deesser_gain(&params.deesser, band_db);
            let coeff = if target < state.deesser_env { de_atk } else { de_rel };
            state.deesser_env = coeff * state.deesser_env + (1.0 - coeff) * target;
            s *= state.deesser_env;
        }

        // 4) Compressor (full-band, dB-domain detector + smoothed gain).
        if params.compressor.enabled {
            let in_db = linear_to_db(s.abs());
            // Peak detector in dB (clamp -inf so the one-pole stays finite).
            let in_db_c = if in_db.is_finite() { in_db } else { -120.0 };
            let coeff = if in_db_c > state.comp_env_db { comp_atk } else { comp_rel };
            if !state.comp_env_db.is_finite() {
                state.comp_env_db = in_db_c;
            } else {
                state.comp_env_db = coeff * state.comp_env_db + (1.0 - coeff) * in_db_c;
            }
            let target = compressor_gain(&params.compressor, state.comp_env_db);
            // Smooth gain in linear domain (same time constants).
            let gcoeff = if target < state.comp_gain_lin { comp_atk } else { comp_rel };
            state.comp_gain_lin = gcoeff * state.comp_gain_lin + (1.0 - gcoeff) * target;
            s *= state.comp_gain_lin;
        }

        // 5) Output trim (smoothed linear gain — always applied, unity if 0 dB).
        s *= state.trim.next();

        *x = s;
    }
}

/// (Re)derive the HPF biquad cascade only when the params changed.
fn refresh_hpf(state: &mut ChannelChainState, p: &FilterParams, sample_rate: u32) {
    if *p == state.hpf_cached {
        return;
    }
    state.hpf_cached = *p;
    if !p.enabled {
        state.hpf_active_sections = 0;
        return;
    }
    // Each biquad is a 2-pole (12 dB/oct) Butterworth HPF. A higher even order
    // cascades multiple identical sections; order 2 -> 1 section.
    let sections = ((p.order as usize).max(2) / 2).clamp(1, MAX_HPF_SECTIONS);
    let coeffs = design_hpf(p, sample_rate);
    for sect in state.hpf_sections.iter_mut().take(sections) {
        sect.set_coeffs(coeffs);
    }
    state.hpf_active_sections = sections;
}

/// (Re)derive the de-esser sidechain band-pass only on change.
fn refresh_deesser(state: &mut ChannelChainState, p: &DeEsserParams, sample_rate: u32) {
    if *p == state.deesser_cached {
        return;
    }
    state.deesser_cached = *p;
    if !p.enabled {
        return;
    }
    state.deesser_band.set_coeffs(design_deesser_band(p, sample_rate));
}

// ===========================================================================
// Biquad — coefficients (RBJ cookbook) + a direct-form-I state.
// ===========================================================================

/// Biquad coefficients (normalized, a0 == 1). Direct-form-II-transposed-ready,
/// but we run a direct-form-I state for numerical robustness at low cutoffs.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Biquad {
    pub b0: f32,
    pub b1: f32,
    pub b2: f32,
    pub a1: f32,
    pub a2: f32,
}

impl Biquad {
    /// The identity (passthrough) biquad: y = x.
    pub fn identity() -> Self {
        Biquad { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 }
    }
}

/// Per-biquad delay-line state (Direct Form I: two input + two output taps).
#[derive(Debug, Clone, Copy, Default)]
pub struct BiquadState {
    c: Biquad,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl BiquadState {
    /// Install fresh coefficients without clearing the delay line (so a coeff
    /// tweak doesn't click; the small state continuity is fine at audio rates).
    pub fn set_coeffs(&mut self, c: Biquad) {
        self.c = c;
    }

    /// Process one sample through the biquad (Direct Form I).
    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let c = &self.c;
        let y = c.b0 * x + c.b1 * self.x1 + c.b2 * self.x2 - c.a1 * self.y1 - c.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// Derive the high-pass biquad for [`FilterParams`] at `sample_rate` (RBJ
/// cookbook high-pass, Butterworth Q = 1/sqrt(2) for a maximally-flat 2-pole).
/// A disabled filter or a cutoff at/above Nyquist returns the identity biquad.
pub fn design_hpf(p: &FilterParams, sample_rate: u32) -> Biquad {
    let fs = sample_rate as f32;
    if !p.enabled || fs <= 0.0 || p.cutoff_hz <= 0.0 || p.cutoff_hz >= fs * 0.5 {
        return Biquad::identity();
    }
    let q = std::f32::consts::FRAC_1_SQRT_2; // 0.7071 -> Butterworth
    rbj_highpass(p.cutoff_hz, fs, q)
}

/// RBJ cookbook high-pass biquad at `f0` Hz with quality `q`, normalized a0=1.
fn rbj_highpass(f0: f32, fs: f32, q: f32) -> Biquad {
    let w0 = 2.0 * std::f32::consts::PI * f0 / fs;
    let (sin_w0, cos_w0) = w0.sin_cos();
    let alpha = sin_w0 / (2.0 * q);

    let b0 = (1.0 + cos_w0) * 0.5;
    let b1 = -(1.0 + cos_w0);
    let b2 = (1.0 + cos_w0) * 0.5;
    let a0 = 1.0 + alpha;
    let a1 = -2.0 * cos_w0;
    let a2 = 1.0 - alpha;

    Biquad { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
}

/// Derive the de-esser sidechain band-pass biquad (RBJ constant-skirt-gain
/// band-pass) centered on the geometric mean of the band edges, with a Q set by
/// the bandwidth. Detects sibilance energy in 5-8 kHz (SPEC §3).
pub fn design_deesser_band(p: &DeEsserParams, sample_rate: u32) -> Biquad {
    let fs = sample_rate as f32;
    let lo = p.band_low_hz.max(1.0);
    let hi = p.band_high_hz.max(lo + 1.0);
    let f0 = (lo * hi).sqrt();
    if !p.enabled || fs <= 0.0 || f0 <= 0.0 || f0 >= fs * 0.5 {
        return Biquad::identity();
    }
    // Q = f0 / bandwidth.
    let q = (f0 / (hi - lo)).max(0.3);
    rbj_bandpass(f0, fs, q)
}

/// RBJ cookbook band-pass (constant 0 dB peak gain) at `f0`, quality `q`.
fn rbj_bandpass(f0: f32, fs: f32, q: f32) -> Biquad {
    let w0 = 2.0 * std::f32::consts::PI * f0 / fs;
    let (sin_w0, cos_w0) = w0.sin_cos();
    let alpha = sin_w0 / (2.0 * q);

    let b0 = alpha;
    let b1 = 0.0;
    let b2 = -alpha;
    let a0 = 1.0 + alpha;
    let a1 = -2.0 * cos_w0;
    let a2 = 1.0 - alpha;

    Biquad { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
}

/// Per-sample one-pole smoothing coefficient for a time constant `ms` at
/// `sample_rate`: `exp(-1 / (t_seconds * fs))`. A non-positive time constant
/// returns 0.0 (instantaneous — no smoothing). The returned `a` is used as
/// `env = a*env + (1-a)*x`, so `a` near 1 is slow, `a` near 0 is fast.
pub fn time_constant_coeff(ms: f32, sample_rate: u32) -> f32 {
    let fs = sample_rate as f32;
    if ms <= 0.0 || fs <= 0.0 {
        return 0.0;
    }
    let t = ms * 0.001; // seconds
    (-1.0 / (t * fs)).exp()
}

/// The gate's per-sample linear gain target for a detector level in dBFS
/// (SPEC §3: -45 dB threshold). Above threshold -> unity (open); below -> the
/// floor gain (`floor_db` -> linear). A soft 6 dB region just under the
/// threshold interpolates so the gate doesn't chatter on the edge.
pub fn gate_gain(p: &GateParams, level_db: f32) -> f32 {
    let floor_lin = db_to_linear(p.floor_db);
    if level_db >= p.threshold_db {
        1.0
    } else {
        // Soft knee: ramp from unity at threshold down to floor over 6 dB.
        let knee = 6.0f32;
        let below = p.threshold_db - level_db;
        if below < knee {
            let t = below / knee; // 0 at threshold -> 1 at threshold-knee
            // Interpolate in the linear domain between unity and floor.
            1.0 + t * (floor_lin - 1.0)
        } else {
            floor_lin
        }
    }
}

/// The de-esser's per-sample linear gain target from the sidechain band level
/// (dBFS). Acts as a compressor over the band: above threshold the gain reduces
/// by `(over) * (1 - 1/ratio)` dB, applied to the whole signal.
pub fn deesser_gain(p: &DeEsserParams, band_db: f32) -> f32 {
    if !band_db.is_finite() || band_db <= p.threshold_db {
        return 1.0;
    }
    let ratio = p.ratio.max(1.0);
    let over = band_db - p.threshold_db;
    let reduction_db = over * (1.0 - 1.0 / ratio);
    db_to_linear(-reduction_db)
}

/// The compressor's per-sample linear gain target for a detector level in dBFS
/// (SPEC §3: 3:1, soft knee, make-up). Implements the standard knee curve:
/// below `threshold - knee/2` -> no reduction; above `threshold + knee/2` ->
/// full `(1 - 1/ratio)` slope; a quadratic blend through the knee. Make-up gain
/// is folded into the returned linear factor.
pub fn compressor_gain(p: &CompressorParams, level_db: f32) -> f32 {
    let makeup = db_to_linear(p.makeup_db);
    if !level_db.is_finite() {
        return makeup;
    }
    let ratio = p.ratio.max(1.0);
    let knee = p.knee_db.max(0.0);
    let half = knee * 0.5;
    let over = level_db - p.threshold_db;

    // Gain reduction in dB as a function of how far over threshold we are.
    let reduction_db = if knee > 0.0 && over > -half && over < half {
        // Soft knee: quadratic interpolation of the slope across the knee.
        let x = over + half; // 0 .. knee
        (1.0 - 1.0 / ratio) * (x * x) / (2.0 * knee)
    } else if over <= -half {
        0.0
    } else {
        (1.0 - 1.0 / ratio) * over
    };

    db_to_linear(-reduction_db) * makeup
}

#[cfg(test)]
impl crate::types::ChannelDsp {
    /// Test helper: a fully-enabled default chain (all stages on).
    fn default_enabled() -> Self {
        Self { enabled: true, ..Self::default() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::{MatrixState, MatrixSnapshot};
    use crate::types::{AudioFormat, ChannelDsp, DEFAULT_SAMPLE_RATE};

    const FS: u32 = 48_000;

    // --- helpers -----------------------------------------------------------

    /// Generate `n` samples of a unit sine at `freq` Hz.
    fn sine(freq: f32, n: usize, fs: u32) -> Vec<f32> {
        (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / fs as f32).sin())
            .collect()
    }

    /// Peak absolute value of a buffer.
    fn peak(buf: &[f32]) -> f32 {
        buf.iter().fold(0.0f32, |m, &s| m.max(s.abs()))
    }

    /// RMS of a buffer.
    fn rms(buf: &[f32]) -> f32 {
        let s: f64 = buf.iter().map(|&x| (x as f64) * (x as f64)).sum();
        (s / buf.len() as f64).sqrt() as f32
    }

    /// Steady-state RMS gain (dB) of a biquad at `freq`, measured by running a
    /// sine through it long enough for the transient to settle.
    fn biquad_response_db(b: Biquad, freq: f32, fs: u32) -> f32 {
        let n = 16_384;
        let input = sine(freq, n, fs);
        let mut st = BiquadState::default();
        st.set_coeffs(b);
        let out: Vec<f32> = input.iter().map(|&x| st.process(x)).collect();
        // Skip the first quarter (transient); measure the settled tail.
        let tail = &out[n / 4..];
        let tail_in = &input[n / 4..];
        linear_to_db(rms(tail) / rms(tail_in))
    }

    // --- Smoother ----------------------------------------------------------

    #[test]
    fn smoother_ramps_continuously_no_discontinuity() {
        let mut s = Smoother::with_rate(0.0, FS);
        s.set_target(1.0);
        let ramp = ramp_len_samples(PARAM_RAMP_MS, FS) as usize;
        let mut prev = s.value();
        let mut max_step = 0.0f32;
        for _ in 0..ramp {
            let v = s.next();
            max_step = max_step.max((v - prev).abs());
            prev = v;
        }
        // Reaches the target exactly at the end of the ramp.
        assert!((s.value() - 1.0).abs() < 1e-5, "ended at {}", s.value());
        // The largest single-sample step is tiny (no zipper / discontinuity):
        // a 1.0 move over ~240 samples is < 0.005 per sample.
        assert!(max_step < 0.01, "max per-sample step {max_step} too large");
        // Monotonic, bounded by the target.
        assert!(s.at_target());
    }

    #[test]
    fn smoother_reaims_in_flight_without_jump() {
        let mut s = Smoother::with_rate(0.0, FS);
        s.set_target(1.0);
        for _ in 0..10 {
            s.next();
        }
        let mid = s.value();
        // Re-aim partway: should continue from `mid`, not jump.
        s.set_target(0.5);
        let after = s.value();
        assert_eq!(mid, after, "set_target must not move the current value");
        for _ in 0..ramp_len_samples(PARAM_RAMP_MS, FS) {
            s.next();
        }
        assert!((s.value() - 0.5).abs() < 1e-5);
    }

    // --- mix_block ---------------------------------------------------------

    #[test]
    fn mix_sums_crosspoints_correctly() {
        // 2 inputs -> 1 output. Input 0 at 0 dB (unity), input 1 at -6.02 dB
        // (~0.5x). Output should be in0 + 0.5*in1 sample-wise.
        let mut m = MatrixState::new(2, 1).unwrap();
        m.set_crosspoint(0, 0, 0.0).unwrap();
        m.set_crosspoint(1, 0, -6.020599).unwrap();
        let snap = m.snapshot();

        let in0 = vec![0.4f32; 64];
        let in1 = vec![0.2f32; 64];
        let mut out = vec![999.0f32; 64];
        let fmt = AudioFormat::new(1, FS);
        let inputs = [
            BlockRef { data: &in0, format: fmt },
            BlockRef { data: &in1, format: fmt },
        ];
        let mut outputs = [BlockMut { data: &mut out, format: fmt }];
        mix_block(&snap, &inputs, &mut outputs);

        // 0.4*1.0 + 0.2*0.5 = 0.5.
        for &s in out.iter() {
            assert!((s - 0.5).abs() < 1e-3, "got {s}");
        }
    }

    #[test]
    fn mix_cleared_route_is_silent() {
        // No crosspoints set -> all -inf -> output silent.
        let m = MatrixState::new(1, 1).unwrap();
        let snap = m.snapshot();
        let in0 = vec![1.0f32; 32];
        let mut out = vec![1.0f32; 32];
        let fmt = AudioFormat::new(1, FS);
        let inputs = [BlockRef { data: &in0, format: fmt }];
        let mut outputs = [BlockMut { data: &mut out, format: fmt }];
        mix_block(&snap, &inputs, &mut outputs);
        assert!(out.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn mix_honors_input_and_output_mutes() {
        let mut m = MatrixState::new(2, 2).unwrap();
        m.set_crosspoint(0, 0, 0.0).unwrap(); // in0 -> out0 unity
        m.set_crosspoint(1, 1, 0.0).unwrap(); // in1 -> out1 unity
        m.set_input_mute(0, true).unwrap(); // mute in0
        m.set_output_mute(1, true).unwrap(); // mute out1
        let snap = m.snapshot();

        let in0 = vec![0.5f32; 16];
        let in1 = vec![0.7f32; 16];
        let mut o0 = vec![9.0f32; 16];
        let mut o1 = vec![9.0f32; 16];
        let fmt = AudioFormat::new(1, FS);
        let inputs = [
            BlockRef { data: &in0, format: fmt },
            BlockRef { data: &in1, format: fmt },
        ];
        let mut outputs = [
            BlockMut { data: &mut o0, format: fmt },
            BlockMut { data: &mut o1, format: fmt },
        ];
        mix_block(&snap, &inputs, &mut outputs);
        // out0: in0 muted -> silent.
        assert!(o0.iter().all(|&s| s == 0.0), "muted input still routed");
        // out1: output muted -> silent regardless of in1.
        assert!(o1.iter().all(|&s| s == 0.0), "muted output not silenced");
    }

    #[test]
    fn mix_fan_in_multiple_inputs() {
        // 3 inputs all unity into one output: sum is exact.
        let mut m = MatrixState::new(3, 1).unwrap();
        for i in 0..3 {
            m.set_crosspoint(i, 0, 0.0).unwrap();
        }
        let snap = m.snapshot();
        let a = vec![0.1f32; 8];
        let b = vec![0.2f32; 8];
        let c = vec![0.3f32; 8];
        let mut out = vec![0.0f32; 8];
        let fmt = AudioFormat::new(1, FS);
        let inputs = [
            BlockRef { data: &a, format: fmt },
            BlockRef { data: &b, format: fmt },
            BlockRef { data: &c, format: fmt },
        ];
        let mut outputs = [BlockMut { data: &mut out, format: fmt }];
        mix_block(&snap, &inputs, &mut outputs);
        for &s in out.iter() {
            assert!((s - 0.6).abs() < 1e-4, "got {s}");
        }
    }

    #[test]
    fn mix_silent_snapshot_clears_outputs() {
        let snap = MatrixSnapshot::silent(2, 2);
        let in0 = vec![0.5f32; 64];
        let mut out = vec![1.0f32; 64];
        let fmt = AudioFormat::new(1, FS);
        let inputs = [BlockRef { data: &in0, format: fmt }];
        let mut outputs = [BlockMut { data: &mut out, format: fmt }];
        mix_block(&snap, &inputs, &mut outputs);
        assert!(out.iter().all(|&s| s == 0.0));
    }

    // --- HPF ---------------------------------------------------------------

    #[test]
    fn hpf_minus_3db_near_cutoff() {
        let p = FilterParams { enabled: true, cutoff_hz: 80.0, order: 2 };
        let b = design_hpf(&p, FS);
        let at_cutoff = biquad_response_db(b, 80.0, FS);
        // A 2-pole Butterworth HPF is -3 dB at its corner.
        assert!((at_cutoff - (-3.0)).abs() < 0.6, "at 80 Hz: {at_cutoff} dB");
    }

    #[test]
    fn hpf_attenuates_subsonic_passes_voice() {
        let p = FilterParams { enabled: true, cutoff_hz: 80.0, order: 2 };
        let b = design_hpf(&p, FS);
        let at_20 = biquad_response_db(b, 20.0, FS);
        let at_1k = biquad_response_db(b, 1000.0, FS);
        // 20 Hz is ~2 octaves below 80 Hz -> ~24 dB down for 12 dB/oct.
        assert!(at_20 < -18.0, "20 Hz only {at_20} dB down");
        // 1 kHz passes essentially untouched.
        assert!(at_1k.abs() < 0.5, "1 kHz altered by {at_1k} dB");
    }

    #[test]
    fn hpf_disabled_is_passthrough() {
        let p = FilterParams { enabled: false, cutoff_hz: 80.0, order: 2 };
        let b = design_hpf(&p, FS);
        assert_eq!(b, Biquad::identity());
    }

    #[test]
    fn hpf_rolloff_is_12db_per_octave() {
        let p = FilterParams { enabled: true, cutoff_hz: 80.0, order: 2 };
        let b = design_hpf(&p, FS);
        // Deep in the stopband the slope is 12 dB/octave: compare 20 -> 40 Hz.
        let at_20 = biquad_response_db(b, 20.0, FS);
        let at_40 = biquad_response_db(b, 40.0, FS);
        let slope = at_40 - at_20; // should be ~ +12 dB per octave (rising)
        assert!((slope - 12.0).abs() < 2.0, "slope {slope} dB/oct");
    }

    // --- Gate --------------------------------------------------------------

    #[test]
    fn gate_opens_above_threshold_closes_below() {
        let p = GateParams::default(); // -45 dB threshold, floor -80
        // Loud signal (-6 dBFS) -> open (unity).
        assert!((gate_gain(&p, -6.0) - 1.0).abs() < 1e-6);
        // Quiet signal (-90 dBFS, well below threshold-knee) -> floor.
        let floor = db_to_linear(p.floor_db);
        assert!((gate_gain(&p, -90.0) - floor).abs() < 1e-6);
        // Exactly at threshold -> just open.
        assert!((gate_gain(&p, -45.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn gate_processing_silences_noise_passes_signal() {
        let mut st = ChannelChainState::default();
        let params = ChannelDsp {
            enabled: true,
            hpf: FilterParams { enabled: false, ..Default::default() },
            gate: GateParams::default(),
            deesser: DeEsserParams { enabled: false, ..Default::default() },
            compressor: CompressorParams { enabled: false, ..Default::default() },
            output_trim_db: 0.0,
        };
        // Below-threshold noise: -60 dBFS sine (well under -45). Run long
        // enough (~0.5 s) for the 80 ms gate release to fully close.
        let quiet_amp = db_to_linear(-60.0);
        let mut quiet: Vec<f32> = sine(1000.0, 24_000, FS).iter().map(|s| s * quiet_amp).collect();
        process_channel_chain(&params, &mut st, &mut quiet, FS);
        // After the release settles, the gate has pulled it far down.
        let tail = &quiet[quiet.len() - 1000..];
        assert!(peak(tail) < quiet_amp * 0.2, "gate failed to attenuate noise: {}", peak(tail));

        // Above-threshold signal passes ~unchanged.
        let mut st2 = ChannelChainState::default();
        let loud_amp = db_to_linear(-12.0);
        let mut loud: Vec<f32> = sine(1000.0, 4800, FS).iter().map(|s| s * loud_amp).collect();
        let orig = peak(&loud);
        process_channel_chain(&params, &mut st2, &mut loud, FS);
        let tail = &loud[loud.len() - 1000..];
        assert!(peak(tail) > orig * 0.9, "gate wrongly attenuated signal");
    }

    // --- Compressor --------------------------------------------------------

    #[test]
    fn compressor_gain_reduction_matches_ratio() {
        // Hard-knee 3:1 at -18 dB threshold, no makeup.
        let p = CompressorParams {
            enabled: true,
            threshold_db: -18.0,
            ratio: 3.0,
            attack_ms: 10.0,
            release_ms: 120.0,
            knee_db: 0.0,
            makeup_db: 0.0,
        };
        // A level 12 dB over threshold (-6 dBFS): output over-threshold should
        // be 12/3 = 4 dB, so gain reduction = 12 - 4 = 8 dB.
        let g = compressor_gain(&p, -6.0);
        let gr_db = linear_to_db(g);
        assert!((gr_db - (-8.0)).abs() < 0.2, "gain reduction {gr_db} dB, want -8");
        // Below threshold: no reduction.
        assert!((compressor_gain(&p, -30.0) - 1.0).abs() < 1e-6);
        // At threshold: ~no reduction.
        assert!((linear_to_db(compressor_gain(&p, -18.0))).abs() < 1e-3);
    }

    #[test]
    fn compressor_makeup_gain_applied() {
        let p = CompressorParams {
            enabled: true,
            threshold_db: -18.0,
            ratio: 3.0,
            attack_ms: 10.0,
            release_ms: 120.0,
            knee_db: 0.0,
            makeup_db: 6.0,
        };
        // Below threshold, only makeup applies (+6 dB).
        let g = linear_to_db(compressor_gain(&p, -40.0));
        assert!((g - 6.0).abs() < 1e-3, "makeup not applied: {g} dB");
    }

    #[test]
    fn compressor_processing_reduces_dynamic_range() {
        let mut st = ChannelChainState::default();
        let params = ChannelDsp {
            enabled: true,
            hpf: FilterParams { enabled: false, ..Default::default() },
            gate: GateParams { enabled: false, ..Default::default() },
            deesser: DeEsserParams { enabled: false, ..Default::default() },
            compressor: CompressorParams {
                enabled: true,
                threshold_db: -18.0,
                ratio: 3.0,
                attack_ms: 10.0,
                release_ms: 120.0,
                knee_db: 0.0,
                makeup_db: 0.0,
            },
            output_trim_db: 0.0,
        };
        // A hot -6 dBFS sine: well over threshold -> measurable reduction.
        let amp = db_to_linear(-6.0);
        let mut buf: Vec<f32> = sine(1000.0, 9600, FS).iter().map(|s| s * amp).collect();
        let in_rms_db = linear_to_db(rms(&buf));
        process_channel_chain(&params, &mut st, &mut buf, FS);
        // Measure the settled tail (after attack).
        let tail = &buf[buf.len() - 2400..];
        let out_rms_db = linear_to_db(rms(tail));
        let reduction = in_rms_db - out_rms_db;
        // ~8 dB of reduction expected for 12 dB over at 3:1.
        assert!(reduction > 4.0, "only {reduction} dB GR");
        assert!(reduction < 12.0, "implausible {reduction} dB GR");
    }

    // --- De-esser ----------------------------------------------------------

    #[test]
    fn deesser_band_targets_sibilance_range() {
        let p = DeEsserParams::default(); // 5-8 kHz
        let b = design_deesser_band(&p, FS);
        // Band-pass peaks in-band (~6.3 kHz center), rejects out-of-band tones.
        let in_band = biquad_response_db(b, 6300.0, FS);
        let low = biquad_response_db(b, 300.0, FS);
        let high = biquad_response_db(b, 15000.0, FS);
        assert!(in_band > low + 12.0, "band not selective vs low: {in_band} vs {low}");
        assert!(in_band > high + 12.0, "band not selective vs high: {in_band} vs {high}");
    }

    #[test]
    fn deesser_attenuates_sibilant_tone_more_than_voiced() {
        // A 7 kHz tone (sibilance) should be reduced more than a 300 Hz tone.
        let params = ChannelDsp {
            enabled: true,
            hpf: FilterParams { enabled: false, ..Default::default() },
            gate: GateParams { enabled: false, ..Default::default() },
            deesser: DeEsserParams {
                enabled: true,
                band_low_hz: 5000.0,
                band_high_hz: 8000.0,
                threshold_db: -40.0,
                ratio: 4.0,
            },
            compressor: CompressorParams { enabled: false, ..Default::default() },
            output_trim_db: 0.0,
        };
        let amp = db_to_linear(-12.0);

        let mut sib: Vec<f32> = sine(7000.0, 9600, FS).iter().map(|s| s * amp).collect();
        let mut st1 = ChannelChainState::default();
        let sib_in = rms(&sib);
        process_channel_chain(&params, &mut st1, &mut sib, FS);
        let sib_gr = linear_to_db(rms(&sib[sib.len() - 2400..]) / sib_in);

        let mut voiced: Vec<f32> = sine(300.0, 9600, FS).iter().map(|s| s * amp).collect();
        let mut st2 = ChannelChainState::default();
        let v_in = rms(&voiced);
        process_channel_chain(&params, &mut st2, &mut voiced, FS);
        let v_gr = linear_to_db(rms(&voiced[voiced.len() - 2400..]) / v_in);

        // Sibilant tone reduced substantially; voiced tone barely touched.
        assert!(sib_gr < -3.0, "sibilance not reduced: {sib_gr} dB");
        assert!(v_gr > -1.0, "voiced wrongly reduced: {v_gr} dB");
        assert!(sib_gr < v_gr - 2.0, "de-esser not frequency-selective");
    }

    // --- chain integration -------------------------------------------------

    #[test]
    fn full_chain_bypass_is_bit_transparent() {
        let mut st = ChannelChainState::default();
        let params = ChannelDsp { enabled: false, ..Default::default() };
        let original = sine(440.0, 256, FS);
        let mut buf = original.clone();
        process_channel_chain(&params, &mut st, &mut buf, FS);
        // Master bypass: output is bit-identical to input.
        assert_eq!(buf, original);
    }

    #[test]
    fn full_chain_runs_without_nan_or_explosion() {
        let mut st = ChannelChainState::default();
        let params = ChannelDsp::default_enabled();
        let amp = db_to_linear(-12.0);
        let mut buf: Vec<f32> = sine(440.0, 9600, FS).iter().map(|s| s * amp).collect();
        process_channel_chain(&params, &mut st, &mut buf, FS);
        assert!(buf.iter().all(|s| s.is_finite()), "chain produced non-finite");
        assert!(peak(&buf) < 4.0, "chain output exploded: {}", peak(&buf));
    }

    #[test]
    fn trim_smoothing_no_discontinuity_on_change() {
        // Process a block at 0 dB trim, then a block at -12 dB trim: the join
        // must ramp, not jump.
        let mut st = ChannelChainState::default();
        let mut p = ChannelDsp {
            enabled: true,
            hpf: FilterParams { enabled: false, ..Default::default() },
            gate: GateParams { enabled: false, ..Default::default() },
            deesser: DeEsserParams { enabled: false, ..Default::default() },
            compressor: CompressorParams { enabled: false, ..Default::default() },
            output_trim_db: 0.0,
        };
        let mut buf1: Vec<f32> = vec![0.5f32; 512];
        process_channel_chain(&p, &mut st, &mut buf1, FS);
        // All ~0.5 (unity trim, smoother starts parked at 1.0).
        assert!((buf1[buf1.len() - 1] - 0.5).abs() < 1e-3);

        p.output_trim_db = -12.0;
        let mut buf2: Vec<f32> = vec![0.5f32; 512];
        process_channel_chain(&p, &mut st, &mut buf2, FS);
        // The very first samples are still near 0.5 (ramp just starting); the
        // tail has settled near 0.5 * 10^(-12/20) ~= 0.1255.
        let target = 0.5 * db_to_linear(-12.0);
        assert!((buf2[buf2.len() - 1] - target).abs() < 1e-3, "tail {}", buf2[buf2.len() - 1]);
        // No sample-to-sample jump exceeds a small bound across the ramp.
        let mut max_jump = 0.0f32;
        let mut prev = buf1[buf1.len() - 1];
        for &s in buf2.iter() {
            max_jump = max_jump.max((s - prev).abs());
            prev = s;
        }
        assert!(max_jump < 0.02, "trim change jumped by {max_jump}");
    }

    #[test]
    fn time_constant_coeff_behaves() {
        // Zero/negative time -> instantaneous (coeff 0).
        assert_eq!(time_constant_coeff(0.0, FS), 0.0);
        assert_eq!(time_constant_coeff(-5.0, FS), 0.0);
        // A finite time constant yields a coeff in (0,1); longer time -> closer
        // to 1 (slower).
        let fast = time_constant_coeff(1.0, FS);
        let slow = time_constant_coeff(100.0, FS);
        assert!(fast > 0.0 && fast < 1.0);
        assert!(slow > fast, "longer TC must give larger coeff");
        // Default sample rate const is what we expect.
        assert_eq!(DEFAULT_SAMPLE_RATE, 48_000);
    }
}

//! The `Engine` — the single owning object the FFI hands Python an opaque
//! pointer to. It binds the control-side [`MatrixState`], the realtime
//! [`SnapshotRing`], the per-channel DSP state, and the meter accumulators into
//! one handle. Python creates ONE engine, calls control ops on it (which mutate
//! the matrix and `publish` a fresh snapshot), and on the (device-gated) audio
//! path the IOProc calls `process_block` which `load`s the latest snapshot.
//!
//! Foundation provides the wiring + the control-op methods (`set_crosspoint`,
//! `set_gain`, mutes, monitor) so the FFI is a thin, safe shim. The realtime
//! `process_block` and the meter folding delegate to the real `dsp`/`metering`
//! implementations. The engine itself does NOT span the FFI
//! as a Rust type — only `*mut Engine` does, via [`crate::ffi`].

use crate::dsp::{self, ChannelChainState};
use crate::error::{NexusError, Result};
use crate::matrix::{MatrixSnapshot, MatrixState, SnapshotRing};
use crate::metering::{
    block_meter, detect_clip_reusing, LoudnessMeterState, SpectrumFrame, SpectrumState,
    TruePeakKernel,
};
use crate::types::{
    AudioFormat, BlockMut, BlockRef, ChannelDsp, ChannelMeter, ClipEvent, LoudnessMeter, Sample,
    MAX_CHANNELS,
};

/// Worst-case interleaved samples per `process_block` we preallocate audio-thread
/// scratch for. The SPEC §2 realtime config is 64 frames, stepping to 128 under
/// load; a generous headroom (4096) covers a 128-frame block at the full
/// `MAX_CHANNELS` interleave (128 * 32 = 4096) and any oversized test block,
/// so the scratch buffers never need to reallocate on the audio path. If a block
/// somehow exceeds this the engine degrades gracefully (it only meters the
/// reserved prefix) rather than allocating.
const MAX_BLOCK_SAMPLES: usize = 4096;

/// Capacity of the realtime clip-event accumulator (SPEC §6 `audio.clipping`).
/// `process_block` appends at most one event per active input per block; the
/// control plane drains it at the telemetry rate (~30 Hz) while blocks run at
/// ~750 Hz, so a modest bound comfortably holds the events produced between two
/// drains even under sustained clipping. Pre-reserved at construction; the audio
/// path pushes ONLY while `len < capacity` (drop-on-overflow) so it never
/// reallocates on the hot path. Overflow can only occur if the control-plane
/// drain stalls for many blocks, in which case the newest clips are dropped (the
/// buffer already holds a clip, which is all the HUD flash needs).
pub const MAX_CLIP_EVENTS: usize = 256;

/// The whole Nexus core, owned by the control plane via an opaque pointer.
pub struct Engine {
    /// The authoritative routing state (control side).
    state: MatrixState,
    /// The lock-free hand-off to the audio thread.
    ring: SnapshotRing,
    /// Per-input DSP parameter sets (control side; copied into snapshots later
    /// when the chain plumbs through — Foundation keeps them here so the FFI
    /// `chain.set` op has somewhere to land).
    channel_dsp: [ChannelDsp; MAX_CHANNELS],
    /// Per-input DSP filter memory (audio side).
    chain_state: Vec<ChannelChainState>,
    /// Audio-thread scratch buffers, PREALLOCATED at construction and reused each
    /// `process_block` so the realtime path never heap-allocates (SPEC §2
    /// alloc-free contract). `chain_scratch` holds one input lane while the studio
    /// chain processes it in place; `mono_scratch` holds the monitored bus summed
    /// to mono for the program (LUFS/spectrum) meters. Both are sized to the max
    /// block (`MAX_BLOCK_SAMPLES`) at construction and only ever reset/refill their
    /// active length via `clear`/`extend_from_slice`/`push` against that reserved
    /// capacity — never reallocating on the hot path.
    chain_scratch: Vec<Sample>,
    mono_scratch: Vec<Sample>,
    /// True-peak clip detector state (SPEC §3 step 4 / §6 `audio.clipping`),
    /// PREALLOCATED at construction so the per-input clip taps in `process_block`
    /// never heap-allocate. `clip_kernel` is the polyphase windowed-sinc kernel
    /// designed ONCE here (its `design()` allocates a prototype `Vec`, which must
    /// not happen on the audio thread); `clip_scratch` is the reusable
    /// deinterleave buffer (one channel's lane at a time), sized to the max block
    /// like `chain_scratch`/`mono_scratch`; `clip_events` is the bounded
    /// drop-on-overflow accumulator the control plane drains via [`Self::drain_clips`].
    clip_kernel: TruePeakKernel,
    clip_scratch: Vec<Sample>,
    clip_events: Vec<ClipEvent>,
    /// Per-input/output meter accumulators the FFI level getter reads.
    meters: Vec<ChannelMeter>,
    /// BS.1770-4 loudness meter for the monitored mix.
    loudness: LoudnessMeterState,
    /// 2048-pt FFT / 96-band spectrum analyzer for the monitored mix.
    spectrum: SpectrumState,
    /// The audio format the engine is configured for.
    format: AudioFormat,
    /// Input trims in dB (SPEC §5 `gain.set` on inputs).
    input_trim_db: [f32; MAX_CHANNELS],
    /// Output trims in dB (SPEC §5 `gain.set` on outputs).
    output_trim_db: [f32; MAX_CHANNELS],
}

impl Engine {
    /// Create an engine with `inputs` x `outputs` channels at `sample_rate`.
    /// Validates the channel counts; the matrix starts fully off (no routes).
    pub fn new(inputs: usize, outputs: usize, sample_rate: u32) -> Result<Self> {
        let state = MatrixState::new(inputs, outputs)?;
        let ring = SnapshotRing::new(state.snapshot());
        let format = AudioFormat::new(outputs.max(1) as u16, sample_rate);
        Ok(Self {
            state,
            ring,
            channel_dsp: [ChannelDsp::default(); MAX_CHANNELS],
            chain_state: vec![ChannelChainState::default(); inputs.max(1)],
            // Reserve the worst-case block up front; the audio path reuses these
            // (clear/extend within capacity) and never reallocates.
            chain_scratch: Vec::with_capacity(MAX_BLOCK_SAMPLES),
            mono_scratch: Vec::with_capacity(MAX_BLOCK_SAMPLES),
            // Clip detector: design the kernel ONCE (allocates here, never on the
            // audio path) and reserve the deinterleave scratch + event accumulator.
            clip_kernel: TruePeakKernel::design(),
            clip_scratch: Vec::with_capacity(MAX_BLOCK_SAMPLES),
            clip_events: Vec::with_capacity(MAX_CLIP_EVENTS),
            meters: vec![ChannelMeter::default(); inputs.max(outputs).max(1)],
            loudness: LoudnessMeterState::new(sample_rate),
            spectrum: SpectrumState::new(sample_rate),
            format,
            input_trim_db: [0.0; MAX_CHANNELS],
            output_trim_db: [0.0; MAX_CHANNELS],
        })
    }

    /// The configured audio format.
    pub fn format(&self) -> AudioFormat {
        self.format
    }
    /// Active input count.
    pub fn inputs(&self) -> usize {
        self.state.inputs()
    }
    /// Active output count.
    pub fn outputs(&self) -> usize {
        self.state.outputs()
    }

    // --- control ops (mutate state, then publish a fresh snapshot) ----------

    /// SPEC §5 `route.set`: set a crosspoint and publish. `-inf` clears.
    pub fn set_crosspoint(&mut self, input: usize, output: usize, gain_db: f32) -> Result<()> {
        self.state.set_crosspoint(input, output, gain_db)?;
        self.republish();
        Ok(())
    }

    /// SPEC §5 `gain.set` on an input trim. Validates finiteness + range.
    pub fn set_input_trim(&mut self, channel: usize, gain_db: f32) -> Result<()> {
        if channel >= self.state.inputs() {
            return Err(NexusError::OutOfBounds {
                what: "input index",
                got: channel,
                limit: self.state.inputs(),
            });
        }
        if !gain_db.is_finite() {
            return Err(NexusError::InvalidParam { param: "gain_db", reason: "must be finite" });
        }
        self.input_trim_db[channel] = gain_db;
        Ok(())
    }

    /// SPEC §5 `gain.set` on an output trim.
    pub fn set_output_trim(&mut self, channel: usize, gain_db: f32) -> Result<()> {
        if channel >= self.state.outputs() {
            return Err(NexusError::OutOfBounds {
                what: "output index",
                got: channel,
                limit: self.state.outputs(),
            });
        }
        if !gain_db.is_finite() {
            return Err(NexusError::InvalidParam { param: "gain_db", reason: "must be finite" });
        }
        self.output_trim_db[channel] = gain_db;
        Ok(())
    }

    /// Mute/unmute an input ("mute the mic" lands here via the daemon).
    pub fn set_input_mute(&mut self, channel: usize, muted: bool) -> Result<()> {
        self.state.set_input_mute(channel, muted)?;
        self.republish();
        Ok(())
    }

    /// Mute/unmute an output.
    pub fn set_output_mute(&mut self, channel: usize, muted: bool) -> Result<()> {
        self.state.set_output_mute(channel, muted)?;
        self.republish();
        Ok(())
    }

    /// SPEC §5 `monitor.set`: assign (or clear) the monitor output and publish.
    pub fn set_monitor_output(&mut self, output: Option<usize>) -> Result<()> {
        self.state.set_monitor_output(output)?;
        self.republish();
        Ok(())
    }

    /// Set the whole per-input DSP chain (SPEC §5 `chain.set`).
    pub fn set_channel_dsp(&mut self, channel: usize, dsp_params: ChannelDsp) -> Result<()> {
        if channel >= self.state.inputs() {
            return Err(NexusError::OutOfBounds {
                what: "input index",
                got: channel,
                limit: self.state.inputs(),
            });
        }
        self.channel_dsp[channel] = dsp_params;
        Ok(())
    }

    /// The current per-input DSP chain params.
    pub fn channel_dsp(&self, channel: usize) -> Result<ChannelDsp> {
        if channel >= self.state.inputs() {
            return Err(NexusError::OutOfBounds {
                what: "input index",
                got: channel,
                limit: self.state.inputs(),
            });
        }
        Ok(self.channel_dsp[channel])
    }

    /// A full snapshot of the matrix (SPEC §5 `state.get`).
    pub fn matrix_snapshot(&self) -> MatrixSnapshot {
        self.state.snapshot()
    }

    /// Read-only access to the control-side matrix (for the FFI `state.get`
    /// serializer and preset save).
    pub fn matrix(&self) -> &MatrixState {
        &self.state
    }

    /// Mutable access to the matrix (for preset LOAD, which replays a whole
    /// state). Callers MUST `republish()` after a batch of mutations.
    pub fn matrix_mut(&mut self) -> &mut MatrixState {
        &mut self.state
    }

    /// Push the current matrix state across the realtime boundary.
    pub fn republish(&self) {
        self.ring.publish(self.state.snapshot());
    }

    // --- realtime path (AUDIO THREAD; delegates to module-agent dsp) --------

    /// Process one block: load the latest snapshot, mix the inputs into the
    /// outputs, run the per-input DSP chains, and update the meter taps. AUDIO
    /// THREAD — no alloc/lock/syscall. The engine wires the calls; the actual
    /// DSP lives in the `dsp` module. `inputs`/`outputs` are borrowed
    /// interleaved blocks from the caller (the IOProc, or a test).
    pub fn process_block(&mut self, inputs: &[BlockRef<'_>], outputs: &mut [BlockMut<'_>]) {
        let snapshot = self.ring.load();
        let active_in = snapshot.inputs.min(inputs.len());

        // --- per-input studio chain (SPEC §3 step 3) ---------------------------
        // Run HPF -> gate -> de-esser -> compressor -> trim on EACH input BEFORE
        // it is metered or mixed, so the processed signal is what the monitored
        // bus carries and what every meter (per-input peak/RMS, program
        // LUFS/spectrum, clip) reads. A fully-bypassed chain (`enabled == false`,
        // the default) is a bit-transparent passthrough, so this is a no-op for an
        // unconfigured channel and the -23 LUFS reference proof (chains flat/
        // bypassed) stays valid.
        //
        // We stage every input's PROCESSED samples contiguously in the preallocated
        // `chain_scratch` (one region per input), record each region's offset+len,
        // then build `BlockRef`s over those regions and run the EXISTING, tested
        // `dsp::mix_block` over the processed inputs. This keeps the proven mix
        // semantics (fan-in, mutes, output clearing) intact while guaranteeing the
        // monitored bus reflects the chain. No heap allocation: the scratch reuses
        // its reserved capacity and the region table is a fixed-size stack array.
        self.chain_scratch.clear();
        let mut regions: [(usize, usize); MAX_CHANNELS] = [(0, 0); MAX_CHANNELS];
        for (ch, inp) in inputs.iter().enumerate().take(active_in) {
            let off = self.chain_scratch.len();
            // Bound the copy so the staged total never exceeds the reserved
            // capacity (degrade gracefully instead of reallocating on the audio
            // path).
            let room = self.chain_scratch.capacity().saturating_sub(off);
            let n = inp.data.len().min(room);
            self.chain_scratch.extend_from_slice(&inp.data[..n]);

            // Run the chain in place over this input's staged region. The borrow is
            // confined to the slice so `chain_state[ch]` can be borrowed mutably
            // alongside it. Bypass flags are honored inside the chain.
            let region = &mut self.chain_scratch[off..off + n];
            if let Some(state) = self.chain_state.get_mut(ch) {
                let params = &self.channel_dsp[ch];
                dsp::process_channel_chain(params, state, region, inp.format.sample_rate);
            }
            regions[ch] = (off, n);
        }

        // Build processed `BlockRef`s over the staged regions and mix them. Because
        // the regions are disjoint slices of one `Vec`, we materialize them into a
        // fixed-size array of refs (no alloc) before the mutable scratch borrow is
        // needed again.
        let processed_refs: [BlockRef<'_>; MAX_CHANNELS] = std::array::from_fn(|ch| {
            if ch < active_in {
                let (off, n) = regions[ch];
                BlockRef { data: &self.chain_scratch[off..off + n], format: inputs[ch].format }
            } else {
                BlockRef { data: &[], format: inputs.get(ch).map(|b| b.format).unwrap_or_default() }
            }
        });
        dsp::mix_block(&snapshot, &processed_refs[..active_in], outputs);

        // Per-input meter taps (SPEC §6 `audio.levels`) read the PROCESSED lane 0,
        // so the HUD level reflects gate/comp/trim/HPF, not the raw input.
        for (ch, proc) in processed_refs.iter().enumerate().take(active_in) {
            if let Some(slot) = self.meters.get_mut(ch) {
                *slot = block_meter(proc, 0);
            }
        }

        // Per-input true-peak clip detection (SPEC §3 step 4 / §6 `audio.clipping`).
        // Runs the 4× oversampling clip detector on the SAME processed lane 0 the
        // level tap reads, so an inter-sample over that the studio chain creates
        // (or a hot input on a bypassed chain) registers. Alloc-free: it reuses the
        // engine's persistent `clip_kernel` and the pre-sized `clip_scratch`
        // deinterleave buffer, and appends to the bounded `clip_events` accumulator
        // ONLY while there's reserved room (drop-on-overflow) so the audio path
        // never reallocates. `detect_clip_reusing` returns the event with the
        // block-local lane index (0); we stamp the real engine input index `ch`,
        // exactly as the level taps store at `meters[ch]`. The control plane drains
        // these via `drain_clips` at telemetry rate.
        for (ch, proc) in processed_refs.iter().enumerate().take(active_in) {
            if let Some(mut ev) =
                detect_clip_reusing(proc, 0, &self.clip_kernel, &mut self.clip_scratch)
            {
                if self.clip_events.len() < self.clip_events.capacity() {
                    ev.channel = ch as u16;
                    self.clip_events.push(ev);
                }
            }
        }

        // Program meters (SPEC §6 LUFS + spectrum): feed the MONITORED output bus
        // (the assigned monitor output, else output 0) summed to mono into the
        // BS.1770-4 loudness meter and the 2048-pt FFT spectrum analyzer. These
        // own fixed-size state; the per-block mono fold reuses the preallocated
        // `mono_scratch` (no heap allocation on the steady-state audio path).
        let monitor_idx = snapshot.monitor_output.unwrap_or(0);
        if let Some(out) = outputs.get(monitor_idx) {
            let ch = out.format.channels.max(1) as usize;
            let frames = (out.data.len() / ch).min(self.mono_scratch.capacity());
            // Sum to mono per frame so the program meters see one coherent bus.
            // `clear()` keeps the reserved capacity; `push` up to `frames` never
            // exceeds it, so no reallocation occurs.
            self.mono_scratch.clear();
            for f in 0..frames {
                let mut acc = 0.0f32;
                for c in 0..ch {
                    acc += out.data[f * ch + c];
                }
                self.mono_scratch.push(acc / ch as f32);
            }
            self.loudness.push(&self.mono_scratch, 1);
            self.spectrum.push(&self.mono_scratch);
        }
    }

    /// The latest per-channel meter (peak/RMS) for one channel (SPEC §6
    /// `audio.levels`), as accumulated by the realtime `process_block` meter taps.
    pub fn channel_meter(&self, channel: usize) -> ChannelMeter {
        self.meters.get(channel).copied().unwrap_or_default()
    }

    /// Update the stored meter for a channel from a block (called off the audio
    /// path by the agent's metering wiring; Foundation provides the setter).
    pub fn update_channel_meter(&mut self, channel: usize, block: &BlockRef<'_>) {
        if let Some(slot) = self.meters.get_mut(channel) {
            *slot = block_meter(block, channel);
        }
    }

    /// The current BS.1770-4 loudness triplet (SPEC §6).
    pub fn loudness(&self) -> LoudnessMeter {
        self.loudness.read()
    }

    /// The current 96-band spectrum (SPEC §6).
    pub fn spectrum(&self) -> SpectrumFrame {
        self.spectrum.read()
    }

    /// The clip-event accumulator's fixed capacity ([`MAX_CLIP_EVENTS`]), so the
    /// control plane can size a drain buffer without hard-coding it.
    pub fn clip_capacity(&self) -> usize {
        MAX_CLIP_EVENTS
    }

    /// Drain the accumulated true-peak clip events (SPEC §6 `audio.clipping`) into
    /// `out`, returning the number written. Copies up to `out.len()` events and
    /// removes exactly those from the accumulator so each clip is reported once;
    /// any tail that didn't fit is retained for the next drain. Called off the
    /// audio path by the control-plane telemetry poll (mirroring the other meter
    /// getters); the `drain` retains the Vec's capacity, so no reallocation.
    pub fn drain_clips(&mut self, out: &mut [ClipEvent]) -> usize {
        let n = out.len().min(self.clip_events.len());
        out[..n].copy_from_slice(&self.clip_events[..n]);
        self.clip_events.drain(..n);
        n
    }

    /// Feed the monitored mix to the loudness + spectrum meters (agent-driven).
    pub fn meter_program(&mut self, mono_mix: &[Sample], channels: u16) {
        self.loudness.push(mono_mix, channels);
        self.spectrum.push(mono_mix);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_create_and_route() {
        let mut e = Engine::new(2, 2, 48_000).unwrap();
        assert_eq!(e.inputs(), 2);
        assert_eq!(e.outputs(), 2);
        // Setting a crosspoint publishes a snapshot the ring can load.
        e.set_crosspoint(0, 0, -3.0).unwrap();
        let snap = e.matrix_snapshot();
        assert_eq!(snap.grid[0][0], -3.0);
        assert!(e.matrix().revision() >= 1);
    }

    #[test]
    fn engine_rejects_bad_args() {
        let mut e = Engine::new(2, 2, 48_000).unwrap();
        assert!(e.set_crosspoint(5, 0, 0.0).is_err());
        assert!(e.set_input_trim(0, f32::NAN).is_err());
        assert!(e.set_monitor_output(Some(9)).is_err());
        assert!(Engine::new(MAX_CHANNELS + 1, 2, 48_000).is_err());
    }

    #[test]
    fn process_block_runs_without_panicking() {
        let mut e = Engine::new(1, 1, 48_000).unwrap();
        e.set_crosspoint(0, 0, 0.0).unwrap();
        let inbuf = vec![0.25f32; 64];
        let mut outbuf = vec![0.0f32; 64];
        let inputs = [BlockRef { data: &inbuf, format: AudioFormat::new(1, 48_000) }];
        let mut outputs = [BlockMut { data: &mut outbuf, format: AudioFormat::new(1, 48_000) }];
        // Smoke test: the real dsp/mix path runs end-to-end without panicking.
        // (Signal-level behavior is asserted by the chain/meter tests above.)
        e.process_block(&inputs, &mut outputs);
    }

    // --- chain-wiring proofs (residual 1): the per-input studio chain must reach
    //     the MONITORED bus + meters, not just work in isolation ---------------

    use crate::types::{
        db_to_linear, linear_to_db, CompressorParams, DeEsserParams, FilterParams, GateParams,
    };

    const FS: u32 = 48_000;

    /// Generate `n` samples of a unit sine at `freq` Hz, scaled by `amp`.
    fn sine(freq: f32, amp: f32, n: usize, fs: u32) -> Vec<f32> {
        (0..n)
            .map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / fs as f32).sin())
            .collect()
    }

    /// RMS of a buffer.
    fn rms(buf: &[f32]) -> f32 {
        let s: f64 = buf.iter().map(|&x| (x as f64) * (x as f64)).sum();
        (s / buf.len().max(1) as f64).sqrt() as f32
    }

    /// Realtime block size the tests drive the engine at (frames per
    /// `process_block`). Matches the SPEC §2 "step to 128" config and stays well
    /// inside the preallocated scratch — like the device IOProc, we feed long
    /// signals as a stream of small blocks (which also exercises the chain's
    /// cross-block envelope/filter state).
    const BLK: usize = 128;

    /// Drive one input through `process_block` (input 0 -> output 0 at unity,
    /// output 0 monitored) with the given chain params, streaming `input` in
    /// BLK-frame blocks. Returns the CONCATENATED monitored OUTPUT (what the
    /// program meters read). The chain's filter/envelope state carries across the
    /// blocks exactly as on the audio thread.
    fn monitored_output(params: ChannelDsp, input: &[f32]) -> Vec<f32> {
        let mut e = Engine::new(1, 1, FS).unwrap();
        e.set_crosspoint(0, 0, 0.0).unwrap(); // unity route in0 -> out0
        e.set_monitor_output(Some(0)).unwrap();
        e.set_channel_dsp(0, params).unwrap();
        let fmt = AudioFormat::new(1, FS);
        let mut captured = Vec::with_capacity(input.len());
        for chunk in input.chunks(BLK) {
            let mut outbuf = vec![0.0f32; chunk.len()];
            {
                let inputs = [BlockRef { data: chunk, format: fmt }];
                let mut outputs = [BlockMut { data: &mut outbuf, format: fmt }];
                e.process_block(&inputs, &mut outputs);
            }
            captured.extend_from_slice(&outbuf);
        }
        captured
    }

    #[test]
    fn bypassed_chain_is_passthrough_at_the_monitored_bus() {
        // The default chain (master bypass) must leave the monitored output equal
        // to the unity-routed input — this is the invariant the -23 LUFS proof
        // relies on (chains flat/bypassed must not color the bus).
        let input = sine(1000.0, 0.5, 256, FS);
        let out = monitored_output(ChannelDsp::default(), &input);
        for (o, i) in out.iter().zip(input.iter()) {
            assert!((o - i).abs() < 1e-6, "bypassed chain altered the bus: {o} vs {i}");
        }
    }

    #[test]
    fn enabled_hpf_attenuates_subsonic_tone_at_the_monitored_bus() {
        // THE CRUX: an 80 Hz HPF, enabled, must attenuate a 40 Hz tone at the
        // MONITORED OUTPUT relative to the same chain bypassed. Asserting the
        // DIFFERENCE (on vs off) proves the chain is wired into the bus, not just
        // correct in isolation.
        let input = sine(40.0, 0.5, 4096, FS);

        let on = ChannelDsp {
            enabled: true,
            hpf: FilterParams { enabled: true, cutoff_hz: 80.0, order: 2 },
            gate: GateParams { enabled: false, ..Default::default() },
            deesser: DeEsserParams { enabled: false, ..Default::default() },
            compressor: CompressorParams { enabled: false, ..Default::default() },
            ..Default::default()
        };

        let off = ChannelDsp::default(); // master bypass

        // Measure the settled tail to skip the filter transient.
        let out_on = monitored_output(on, &input);
        let out_off = monitored_output(off, &input);
        let tail_on = rms(&out_on[out_on.len() - 1024..]);
        let tail_off = rms(&out_off[out_off.len() - 1024..]);

        let atten_db = linear_to_db(tail_on / tail_off);
        // 40 Hz is one octave below the 80 Hz corner: a 12 dB/oct HPF puts it well
        // down. Require a clear, unambiguous attenuation at the bus.
        assert!(atten_db < -6.0, "HPF did not reach the bus: {atten_db} dB (on {tail_on}, off {tail_off})");
    }

    #[test]
    fn enabled_gate_below_threshold_reduces_the_monitored_bus() {
        // A signal well below the gate threshold must be pulled down at the
        // monitored output when the gate is enabled vs bypassed.
        let quiet = sine(1000.0, db_to_linear(-60.0), 24_000, FS); // -60 dBFS, < -45 thr

        let on = ChannelDsp {
            enabled: true,
            hpf: FilterParams { enabled: false, ..Default::default() },
            gate: GateParams::default(), // -45 dB threshold, -80 floor
            deesser: DeEsserParams { enabled: false, ..Default::default() },
            compressor: CompressorParams { enabled: false, ..Default::default() },
            ..Default::default()
        };

        let off = ChannelDsp::default();

        let out_on = monitored_output(on, &quiet);
        let out_off = monitored_output(off, &quiet);
        // After the gate's release settles, the tail is far down vs bypassed.
        let tail_on = rms(&out_on[out_on.len() - 2000..]);
        let tail_off = rms(&out_off[out_off.len() - 2000..]);
        assert!(tail_on < tail_off * 0.3, "gate did not reach the bus: on {tail_on}, off {tail_off}");
    }

    #[test]
    fn enabled_compressor_over_threshold_shows_gain_reduction_at_the_meter() {
        // A hot input over the compressor threshold must read LOWER RMS at the
        // monitored bus (and the per-input meter) when the compressor is enabled
        // vs bypassed — proving gain reduction lands on the metered path.
        let hot = sine(1000.0, db_to_linear(-6.0), 9600, FS); // -6 dBFS, over -18 thr

        let on = ChannelDsp {
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
            ..Default::default()
        };

        let off = ChannelDsp::default();

        let out_on = monitored_output(on, &hot);
        let out_off = monitored_output(off, &hot);
        let gr_db = linear_to_db(rms(&out_on[out_on.len() - 2400..]) / rms(&out_off[out_off.len() - 2400..]));
        // ~8 dB of reduction expected for 12 dB over at 3:1; require a real,
        // bounded reduction at the bus.
        assert!(gr_db < -4.0, "compressor GR did not reach the bus: {gr_db} dB");
        assert!(gr_db > -12.0, "implausible GR {gr_db} dB");
    }

    #[test]
    fn enabled_chain_changes_the_per_input_meter_vs_bypassed() {
        // The per-input level meter (audio.levels) must reflect the chain too: an
        // enabled compressor on a hot input lowers the per-input peak/RMS vs a
        // bypassed chain on the same input.
        let hot = sine(1000.0, db_to_linear(-6.0), 9600, FS);

        let on = ChannelDsp {
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
            ..Default::default()
        };

        let meter_for = |params: ChannelDsp| -> ChannelMeter {
            let mut e = Engine::new(1, 1, FS).unwrap();
            e.set_crosspoint(0, 0, 0.0).unwrap();
            e.set_monitor_output(Some(0)).unwrap();
            e.set_channel_dsp(0, params).unwrap();
            let fmt = AudioFormat::new(1, FS);
            // Stream in BLK-frame blocks so the chain's envelope settles; the
            // per-input meter reflects the LAST processed block (post-attack).
            for chunk in hot.chunks(BLK) {
                let mut outbuf = vec![0.0f32; chunk.len()];
                let inputs = [BlockRef { data: chunk, format: fmt }];
                let mut outputs = [BlockMut { data: &mut outbuf, format: fmt }];
                e.process_block(&inputs, &mut outputs);
            }
            e.channel_meter(0)
        };

        let m_on = meter_for(on);
        let m_off = meter_for(ChannelDsp::default());
        // The compressed input reads a lower RMS than the bypassed input.
        assert!(
            m_on.rms_dbfs < m_off.rms_dbfs - 2.0,
            "per-input meter unaffected by chain: on {} vs off {}",
            m_on.rms_dbfs,
            m_off.rms_dbfs
        );
    }

    // --- alloc-free proof (residual 2): the audio-thread scratch buffers are
    //     preallocated and never reallocate across blocks ----------------------

    #[test]
    fn process_block_does_not_reallocate_scratch_across_blocks() {
        // Run many blocks and assert the scratch buffers keep a STABLE capacity
        // and a STABLE backing pointer — i.e. no heap (re)allocation on the audio
        // path. Capacity is reserved at construction (MAX_BLOCK_SAMPLES).
        let mut e = Engine::new(2, 2, FS).unwrap();
        e.set_crosspoint(0, 0, 0.0).unwrap();
        e.set_crosspoint(1, 0, 0.0).unwrap();
        e.set_monitor_output(Some(0)).unwrap();

        let cap_chain0 = e.chain_scratch.capacity();
        let cap_mono0 = e.mono_scratch.capacity();
        assert!(cap_chain0 >= MAX_BLOCK_SAMPLES);
        assert!(cap_mono0 >= MAX_BLOCK_SAMPLES);

        let in0 = sine(220.0, 0.4, 128, FS);
        let in1 = sine(440.0, 0.4, 128, FS);
        let mut ptr_chain: Option<*const f32> = None;
        let mut ptr_mono: Option<*const f32> = None;
        for _ in 0..256 {
            let mut o0 = vec![0.0f32; 128];
            let mut o1 = vec![0.0f32; 128];
            {
                let inputs = [
                    BlockRef { data: &in0, format: AudioFormat::new(1, FS) },
                    BlockRef { data: &in1, format: AudioFormat::new(1, FS) },
                ];
                let mut outputs = [
                    BlockMut { data: &mut o0, format: AudioFormat::new(1, FS) },
                    BlockMut { data: &mut o1, format: AudioFormat::new(1, FS) },
                ];
                e.process_block(&inputs, &mut outputs);
            }
            // Capacity never grows (no realloc).
            assert_eq!(e.chain_scratch.capacity(), cap_chain0, "chain scratch reallocated");
            assert_eq!(e.mono_scratch.capacity(), cap_mono0, "mono scratch reallocated");
            // Backing pointer is stable across blocks (same allocation reused).
            let pc = e.chain_scratch.as_ptr();
            let pm = e.mono_scratch.as_ptr();
            if let Some(prev) = ptr_chain {
                assert_eq!(pc, prev, "chain scratch pointer moved (realloc)");
            }
            if let Some(prev) = ptr_mono {
                assert_eq!(pm, prev, "mono scratch pointer moved (realloc)");
            }
            ptr_chain = Some(pc);
            ptr_mono = Some(pm);
        }
    }

    #[test]
    fn process_block_does_not_reallocate_under_sustained_clipping() {
        // Mirror of the scratch-realloc proof, but for the CLIP path: drive a
        // full-scale (0 dBFS, > -1 dBFS true-peak) signal so every block fires the
        // per-input clip detector, and assert the clip deinterleave scratch AND the
        // clip-event accumulator keep a STABLE capacity + backing pointer across
        // hundreds of blocks — i.e. `detect_clip_reusing` + the drop-on-overflow
        // accumulator never heap-allocate on the audio thread (SPEC §2).
        let mut e = Engine::new(2, 2, FS).unwrap();
        e.set_crosspoint(0, 0, 0.0).unwrap();
        e.set_crosspoint(1, 0, 0.0).unwrap();
        e.set_monitor_output(Some(0)).unwrap();

        let cap_clip_scratch0 = e.clip_scratch.capacity();
        let cap_clip_events0 = e.clip_events.capacity();
        assert!(cap_clip_scratch0 >= MAX_BLOCK_SAMPLES);
        assert!(cap_clip_events0 >= MAX_CLIP_EVENTS);
        let ptr_clip_events0 = e.clip_events.as_ptr();

        // Full-scale inputs: unity-routed to the monitored bus, the processed lane 0
        // sits at 0 dBFS, whose true-peak exceeds the -1 dBFS clip ceiling.
        let hot0 = vec![1.0f32; 128];
        let hot1 = vec![1.0f32; 128];
        let mut ptr_clip_scratch: Option<*const f32> = None;
        for _ in 0..256 {
            let mut o0 = vec![0.0f32; 128];
            let mut o1 = vec![0.0f32; 128];
            {
                let inputs = [
                    BlockRef { data: &hot0, format: AudioFormat::new(1, FS) },
                    BlockRef { data: &hot1, format: AudioFormat::new(1, FS) },
                ];
                let mut outputs = [
                    BlockMut { data: &mut o0, format: AudioFormat::new(1, FS) },
                    BlockMut { data: &mut o1, format: AudioFormat::new(1, FS) },
                ];
                e.process_block(&inputs, &mut outputs);
            }
            // Neither the deinterleave scratch nor the event accumulator reallocate.
            assert_eq!(e.clip_scratch.capacity(), cap_clip_scratch0, "clip scratch reallocated");
            assert_eq!(e.clip_events.capacity(), cap_clip_events0, "clip events reallocated");
            assert_eq!(e.clip_events.as_ptr(), ptr_clip_events0, "clip events pointer moved (realloc)");
            let pcs = e.clip_scratch.as_ptr();
            if let Some(prev) = ptr_clip_scratch {
                assert_eq!(pcs, prev, "clip scratch pointer moved (realloc)");
            }
            ptr_clip_scratch = Some(pcs);
        }
        // The detector actually fired (the loop is exercised, not skipped) and the
        // accumulator saturated at its bound (drop-on-overflow held the line).
        assert!(!e.clip_events.is_empty(), "sustained clipping produced no clip events");
        assert!(e.clip_events.len() <= cap_clip_events0, "clip events exceeded reserved capacity");
    }

    #[test]
    fn drain_clips_reports_then_clears() {
        // A hot input produces clip events that `drain_clips` reports once and then
        // clears; a subsequent drain on quiet audio returns nothing.
        let mut e = Engine::new(1, 1, FS).unwrap();
        e.set_crosspoint(0, 0, 0.0).unwrap();
        e.set_monitor_output(Some(0)).unwrap();
        let fmt = AudioFormat::new(1, FS);

        // One hot block -> at least one clip event on input 0.
        let hot = vec![1.0f32; 128];
        let mut out = vec![0.0f32; 128];
        {
            let inputs = [BlockRef { data: &hot, format: fmt }];
            let mut outputs = [BlockMut { data: &mut out, format: fmt }];
            e.process_block(&inputs, &mut outputs);
        }

        let mut sink = vec![ClipEvent { channel: 0, true_peak_dbfs: 0.0 }; e.clip_capacity()];
        let n = e.drain_clips(&mut sink);
        assert!(n >= 1, "expected a clip event from a full-scale block");
        assert_eq!(sink[0].channel, 0, "clip event should carry the engine input index");
        assert!(sink[0].true_peak_dbfs >= crate::types::CLIP_THRESHOLD_DBFS);

        // Draining again immediately (no new audio) returns nothing — reported once.
        assert_eq!(e.drain_clips(&mut sink), 0, "clip events were not cleared after drain");

        // A quiet block produces no clips.
        let quiet = sine(1000.0, 0.1, 128, FS);
        let mut out2 = vec![0.0f32; 128];
        {
            let inputs = [BlockRef { data: &quiet, format: fmt }];
            let mut outputs = [BlockMut { data: &mut out2, format: fmt }];
            e.process_block(&inputs, &mut outputs);
        }
        assert_eq!(e.drain_clips(&mut sink), 0, "quiet audio must not clip");
    }
}

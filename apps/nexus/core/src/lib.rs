//! Nexus Core — low-latency audio routing matrix + DSP/metering realtime core
//! for the Nexus studio-control micro-app (SPEC.md / manifest.toml).
//!
//! The Python CONTROL PLANE (`apps/nexus/main.py`) loads this crate's `cdylib`
//! via `ctypes`; the realtime/DSP path cannot be Python (SPEC §2). The crate is
//! built as both:
//!   - `cdylib` — the `.dylib`/`.so` Python loads (the C-ABI surface in [`ffi`]).
//!   - `rlib`   — so `cargo test` exercises the pure DSP/matrix/metering math
//!     headlessly, with SYNTHESIZED in-memory buffers only.
//!
//! Module map (one module per concern; the FROZEN shared types vs. the
//! module-agent stubs are called out so parallel agents fill disjoint files):
//!
//!   FROZEN shared contract (written in Foundation — agents build AGAINST these,
//!   do NOT change them):
//!     - [`error`]   the crate error type + the stable C-ABI status `codes`.
//!     - [`types`]   sample/block model, the DSP param structs, meter payloads,
//!                   the dB<->linear primitives, and the crate constants.
//!     - [`matrix`]  the authoritative `MatrixState` + the lock-free
//!                   `SnapshotRing` (the SPSC realtime boundary, SPEC §1).
//!     - [`engine`]  the owning `Engine` the FFI hands Python an opaque pointer
//!                   to; wires matrix + dsp + metering together.
//!     - [`ffi`]     the `extern "C"` surface Python calls via ctypes — the
//!                   FROZEN wire contract (signatures + status codes).
//!
//!   MODULE-AGENT files (stubs here; one agent each fills the bodies against the
//!   frozen types above — they replace bodies, NOT public signatures):
//!     - [`dsp`]      crosspoint mix + the per-input studio chain (HPF/gate/
//!                    de-esser/compressor + trim) with 5 ms smoothing (SPEC §3).
//!     - [`metering`] peak/RMS, true-peak clip detect, BS.1770-4 LUFS, and the
//!                    2048-pt FFT -> 96 log bands (SPEC §3 step 4, §6).
//!     - [`coreaudio`] DEVICE-GATED: aggregate device + `AudioDeviceIOProc` +
//!                    loopback RTT. Built CORRECT under `--features coreaudio`;
//!                    NEVER run headlessly, NEVER claim-measured.
//!
//! HARD BOUNDARY honored crate-wide: nothing here opens a CoreAudio device,
//! plays audio, binds a socket, or runs a server. The headless surface is pure
//! math over synthesized buffers + the ctypes-loadable symbol table.

// ---- FROZEN shared-contract modules ---------------------------------------
pub mod engine;
pub mod error;
pub mod ffi;
pub mod matrix;
pub mod types;

// ---- module-agent files (filled by downstream agents) ---------------------
pub mod dsp;
pub mod metering;

// ---- device-gated CoreAudio seam ------------------------------------------
// Built in BOTH configurations: without `--features coreaudio` it is an inert
// stub returning `NexusError::Device`; with the feature the real HAL bindings
// compile. Either way it NEVER runs under `cargo test` on this box.
pub mod coreaudio;

// ---- key public re-exports ------------------------------------------------
// The types downstream agents and the FFI reach for most, surfaced at the crate
// root so call sites can `use nexus_core::{Engine, MatrixState, ...}`.
pub use engine::Engine;
pub use error::{codes, NexusError, Result};
pub use matrix::{
    CrosspointGainDb, CrosspointGrid, MatrixSnapshot, MatrixState, MuteFlags, SnapshotRing,
    SNAPSHOT_RING_SLOTS,
};
pub use metering::{LoudnessMeterState, SpectrumFrame, SpectrumState};
pub use types::{
    db_to_linear, linear_to_db, AudioFormat, BlockMut, BlockRef, ChannelDsp, ChannelMeter,
    ClipEvent, CompressorParams, DeEsserParams, FilterParams, GateParams, LoudnessMeter, Sample,
    CLIP_THRESHOLD_DBFS, DEFAULT_BLOCK_FRAMES, DEFAULT_SAMPLE_RATE, FFT_SIZE, GAIN_MAX_DB,
    GAIN_OFF_DB, MAX_CHANNELS, PARAM_RAMP_MS, SPECTRUM_BANDS,
};

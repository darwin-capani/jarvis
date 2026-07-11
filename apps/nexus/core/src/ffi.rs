//! FFI — the C-ABI surface the Python control plane (apps/nexus/main.py) calls
//! via `ctypes`. This is the FROZEN wire contract between Python and the native
//! core. Every signature here is `#[no_mangle] pub extern "C"`, takes only
//! C-ABI-safe types (raw pointers, `i32`/`u32`/`usize`/`f32`/`bool`), and the
//! Python side declares `argtypes`/`restype` to match VERBATIM.
//!
//! Contract rules (do NOT change without updating main.py in lockstep):
//!   - The engine is an OPAQUE handle: `nexus_engine_create` returns
//!     `*mut Engine` (Python holds it as `c_void_p`); every other call takes it
//!     back. `nexus_engine_destroy` frees it. NULL handle -> INVALID_HANDLE.
//!   - Fallible calls return an `i32` status from `crate::error::codes`
//!     (`0` = OK, negatives = error). Getters that return a value write it
//!     through an out-pointer and return the status.
//!   - Audio buffers are passed as `*const f32` / `*mut f32` + a frame count +
//!     a channel count; the core borrows them for the duration of the call and
//!     never retains the pointer.
//!   - Strings (preset names/paths) are NUL-terminated `*const c_char`, borrowed.
//!   - NO panics cross the boundary: each shim is wrapped in
//!     `catch_unwind` and converts a panic to `codes::INTERNAL`.
//!
//! Memory ownership: the `*mut Engine` is the ONLY heap object that crosses the
//! boundary; Python must call `nexus_engine_destroy` exactly once. All buffer
//! and string pointers remain owned by Python.

use std::os::raw::c_char;
use std::panic::{catch_unwind, AssertUnwindSafe};

use crate::engine::{Engine, MAX_CLIP_EVENTS};
use crate::error::{codes, NexusError};
use crate::metering::SPECTRUM_BANDS_HINT;
use crate::types::{AudioFormat, BlockMut, BlockRef, ClipEvent};

/// ABI version of this FFI surface. Python reads it via [`nexus_abi_version`]
/// and refuses to run against a mismatched core. Bump on any breaking change to
/// a signature/struct below.
///
/// v2 adds the `audio.clipping` drain surface ([`nexus_clip_capacity`] +
/// [`nexus_drain_clips`]); the control plane pins EXPECTED_ABI_VERSION to match.
pub const NEXUS_ABI_VERSION: u32 = 2;

/// Map a `Result<(), NexusError>` to a C status code.
#[inline]
fn status(r: Result<(), NexusError>) -> i32 {
    match r {
        Ok(()) => codes::OK,
        Err(e) => e.code(),
    }
}

/// Run `f`, converting any panic into `codes::INTERNAL` so no unwind crosses the
/// C ABI (undefined behavior otherwise).
#[inline]
fn guard<F: FnOnce() -> i32>(f: F) -> i32 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(code) => code,
        Err(_) => codes::INTERNAL,
    }
}

/// Turn a raw `*mut Engine` into a `&mut Engine`, or return INVALID_HANDLE.
/// SAFETY: the pointer must be one minted by [`nexus_engine_create`] and not yet
/// destroyed; Python upholds this (it holds exactly one and frees it once).
macro_rules! engine_mut {
    ($ptr:expr) => {{
        if $ptr.is_null() {
            return codes::INVALID_HANDLE;
        }
        // SAFETY: non-null, created by nexus_engine_create, single-threaded use
        // from the Python control plane (which serializes its ctypes calls).
        unsafe { &mut *$ptr }
    }};
}
macro_rules! engine_ref {
    ($ptr:expr) => {{
        if $ptr.is_null() {
            return codes::INVALID_HANDLE;
        }
        // SAFETY: as above; shared read.
        unsafe { &*$ptr }
    }};
}

// ===========================================================================
// Lifecycle
// ===========================================================================

/// `uint32_t nexus_abi_version(void);` — the ABI version Python validates.
#[no_mangle]
pub extern "C" fn nexus_abi_version() -> u32 {
    NEXUS_ABI_VERSION
}

/// `Engine* nexus_engine_create(size_t inputs, size_t outputs, uint32_t sample_rate);`
/// Returns an opaque engine handle, or NULL on invalid args (e.g. channel count
/// over the cap). Python holds the result as `c_void_p` and passes it to every
/// other call. Pairs with [`nexus_engine_destroy`].
#[no_mangle]
pub extern "C" fn nexus_engine_create(inputs: usize, outputs: usize, sample_rate: u32) -> *mut Engine {
    let made = catch_unwind(|| Engine::new(inputs, outputs, sample_rate));
    match made {
        Ok(Ok(engine)) => Box::into_raw(Box::new(engine)),
        _ => std::ptr::null_mut(),
    }
}

/// `void nexus_engine_destroy(Engine* engine);` — free the engine. NULL is a
/// no-op. After this the pointer is dangling; Python must not reuse it.
#[no_mangle]
pub unsafe extern "C" fn nexus_engine_destroy(engine: *mut Engine) {
    if engine.is_null() {
        return;
    }
    // SAFETY: reconstitute the Box we leaked in create; drop frees it once.
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
        drop(Box::from_raw(engine));
    }));
}

/// `int32_t nexus_engine_inputs(const Engine* engine, size_t* out_inputs);`
#[no_mangle]
pub unsafe extern "C" fn nexus_engine_inputs(engine: *const Engine, out_inputs: *mut usize) -> i32 {
    guard(|| {
        let e = engine_ref!(engine);
        if out_inputs.is_null() {
            return codes::NULL_POINTER;
        }
        unsafe { *out_inputs = e.inputs() };
        codes::OK
    })
}

/// `int32_t nexus_engine_outputs(const Engine* engine, size_t* out_outputs);`
#[no_mangle]
pub unsafe extern "C" fn nexus_engine_outputs(engine: *const Engine, out_outputs: *mut usize) -> i32 {
    guard(|| {
        let e = engine_ref!(engine);
        if out_outputs.is_null() {
            return codes::NULL_POINTER;
        }
        unsafe { *out_outputs = e.outputs() };
        codes::OK
    })
}

// ===========================================================================
// Control ops (SPEC §5) — mutate the matrix, publish a snapshot
// ===========================================================================

/// `int32_t nexus_set_crosspoint(Engine*, size_t in, size_t out, float gain_db);`
/// SPEC §5 `route.set`. `gain_db == -INFINITY` clears the route.
#[no_mangle]
pub unsafe extern "C" fn nexus_set_crosspoint(engine: *mut Engine, input: usize, output: usize, gain_db: f32) -> i32 {
    guard(|| {
        let e = engine_mut!(engine);
        status(e.set_crosspoint(input, output, gain_db))
    })
}

/// `int32_t nexus_set_input_trim(Engine*, size_t channel, float gain_db);`
/// SPEC §5 `gain.set` (input stage).
#[no_mangle]
pub unsafe extern "C" fn nexus_set_input_trim(engine: *mut Engine, channel: usize, gain_db: f32) -> i32 {
    guard(|| {
        let e = engine_mut!(engine);
        status(e.set_input_trim(channel, gain_db))
    })
}

/// `int32_t nexus_set_output_trim(Engine*, size_t channel, float gain_db);`
/// SPEC §5 `gain.set` (output stage).
#[no_mangle]
pub unsafe extern "C" fn nexus_set_output_trim(engine: *mut Engine, channel: usize, gain_db: f32) -> i32 {
    guard(|| {
        let e = engine_mut!(engine);
        status(e.set_output_trim(channel, gain_db))
    })
}

/// `int32_t nexus_set_input_mute(Engine*, size_t channel, bool muted);`
/// "mute the mic" lands here (voice -> daemon -> gain.set).
#[no_mangle]
pub unsafe extern "C" fn nexus_set_input_mute(engine: *mut Engine, channel: usize, muted: bool) -> i32 {
    guard(|| {
        let e = engine_mut!(engine);
        status(e.set_input_mute(channel, muted))
    })
}

/// `int32_t nexus_set_output_mute(Engine*, size_t channel, bool muted);`
#[no_mangle]
pub unsafe extern "C" fn nexus_set_output_mute(engine: *mut Engine, channel: usize, muted: bool) -> i32 {
    guard(|| {
        let e = engine_mut!(engine);
        status(e.set_output_mute(channel, muted))
    })
}

/// `int32_t nexus_set_monitor_output(Engine*, int32_t output);`
/// SPEC §5 `monitor.set`. A NEGATIVE `output` clears the monitor assignment;
/// otherwise it assigns that output index.
#[no_mangle]
pub unsafe extern "C" fn nexus_set_monitor_output(engine: *mut Engine, output: i32) -> i32 {
    guard(|| {
        let e = engine_mut!(engine);
        let sel = if output < 0 { None } else { Some(output as usize) };
        status(e.set_monitor_output(sel))
    })
}

// ===========================================================================
// Realtime processing (the IOProc / a test calls this per block)
// ===========================================================================

/// `int32_t nexus_process_block(Engine* engine,`
/// `    const float* const* inputs, size_t input_count,`
/// `    float* const* outputs, size_t output_count,`
/// `    size_t frames, uint16_t channels, uint32_t sample_rate);`
///
/// Process one interleaved block. `inputs` is an array of `input_count`
/// pointers, each to `frames * channels` interleaved samples; `outputs` likewise
/// (written in place). The core borrows every buffer only for this call. Each
/// per-channel buffer must be exactly `frames * channels` long.
///
/// SAFETY: every pointer in `inputs[0..input_count]` and
/// `outputs[0..output_count]` must be valid for `frames * channels` `f32`s.
/// Python passes ctypes arrays it keeps alive across the call.
#[no_mangle]
pub unsafe extern "C" fn nexus_process_block(
    engine: *mut Engine,
    inputs: *const *const f32,
    input_count: usize,
    outputs: *const *mut f32,
    output_count: usize,
    frames: usize,
    channels: u16,
    sample_rate: u32,
) -> i32 {
    guard(|| {
        let e = engine_mut!(engine);
        if inputs.is_null() || outputs.is_null() {
            return codes::NULL_POINTER;
        }
        if channels == 0 {
            return codes::INVALID_PARAM;
        }
        let n = frames * channels as usize;
        let fmt = AudioFormat::new(channels, sample_rate);

        // Build borrowed block views. SAFETY: caller guarantees each pointer is
        // valid for `n` f32s and the outer arrays for their counts.
        let in_ptrs = unsafe { std::slice::from_raw_parts(inputs, input_count) };
        let out_ptrs = unsafe { std::slice::from_raw_parts(outputs, output_count) };

        let mut in_blocks: Vec<BlockRef<'_>> = Vec::with_capacity(input_count);
        for &p in in_ptrs {
            if p.is_null() {
                return codes::NULL_POINTER;
            }
            let data = unsafe { std::slice::from_raw_parts(p, n) };
            in_blocks.push(BlockRef { data, format: fmt });
        }
        let mut out_blocks: Vec<BlockMut<'_>> = Vec::with_capacity(output_count);
        for &p in out_ptrs {
            if p.is_null() {
                return codes::NULL_POINTER;
            }
            let data = unsafe { std::slice::from_raw_parts_mut(p, n) };
            out_blocks.push(BlockMut { data, format: fmt });
        }

        e.process_block(&in_blocks, &mut out_blocks);
        codes::OK
    })
}

// ===========================================================================
// Meter / LUFS / FFT getters (the Python telemetry loop polls these)
// ===========================================================================

/// `int32_t nexus_get_channel_meter(const Engine*, size_t channel,`
/// `    float* out_peak_dbfs, float* out_rms_dbfs);`
/// SPEC §6 `audio.levels` per-channel entry. Writes both out-params.
#[no_mangle]
pub unsafe extern "C" fn nexus_get_channel_meter(
    engine: *const Engine,
    channel: usize,
    out_peak_dbfs: *mut f32,
    out_rms_dbfs: *mut f32,
) -> i32 {
    guard(|| {
        let e = engine_ref!(engine);
        if out_peak_dbfs.is_null() || out_rms_dbfs.is_null() {
            return codes::NULL_POINTER;
        }
        let m = e.channel_meter(channel);
        unsafe {
            *out_peak_dbfs = m.peak_dbfs;
            *out_rms_dbfs = m.rms_dbfs;
        }
        codes::OK
    })
}

/// `int32_t nexus_get_loudness(const Engine*,`
/// `    float* out_lufs_m, float* out_lufs_s, float* out_lufs_i);`
/// SPEC §6 BS.1770-4 loudness triplet.
#[no_mangle]
pub unsafe extern "C" fn nexus_get_loudness(
    engine: *const Engine,
    out_lufs_m: *mut f32,
    out_lufs_s: *mut f32,
    out_lufs_i: *mut f32,
) -> i32 {
    guard(|| {
        let e = engine_ref!(engine);
        if out_lufs_m.is_null() || out_lufs_s.is_null() || out_lufs_i.is_null() {
            return codes::NULL_POINTER;
        }
        let l = e.loudness();
        unsafe {
            *out_lufs_m = l.lufs_m;
            *out_lufs_s = l.lufs_s;
            *out_lufs_i = l.lufs_i;
        }
        codes::OK
    })
}

/// `int32_t nexus_get_spectrum(const Engine*, float* out_bands, size_t cap);`
/// SPEC §6 `audio.spectrum`: copies up to `cap` band magnitudes (dBFS) into
/// `out_bands`. `cap` must be >= [`crate::types::SPECTRUM_BANDS`] (96) or it
/// returns OUT_OF_BOUNDS without writing. Use [`nexus_spectrum_band_count`] to
/// size the buffer.
#[no_mangle]
pub unsafe extern "C" fn nexus_get_spectrum(engine: *const Engine, out_bands: *mut f32, cap: usize) -> i32 {
    guard(|| {
        let e = engine_ref!(engine);
        if out_bands.is_null() {
            return codes::NULL_POINTER;
        }
        if cap < SPECTRUM_BANDS_HINT {
            return codes::OUT_OF_BOUNDS;
        }
        let frame = e.spectrum();
        // SAFETY: caller guarantees `out_bands` holds at least `cap` >= 96 f32s.
        let dst = unsafe { std::slice::from_raw_parts_mut(out_bands, SPECTRUM_BANDS_HINT) };
        dst.copy_from_slice(&frame.bands);
        codes::OK
    })
}

/// `size_t nexus_spectrum_band_count(void);` — the fixed band count (96) so
/// Python can size its receive buffer without hard-coding it.
#[no_mangle]
pub extern "C" fn nexus_spectrum_band_count() -> usize {
    SPECTRUM_BANDS_HINT
}

/// `size_t nexus_clip_capacity(void);` — the clip-event accumulator's fixed
/// capacity ([`MAX_CLIP_EVENTS`]) so Python can size its drain buffers without
/// hard-coding it (mirrors [`nexus_spectrum_band_count`]).
#[no_mangle]
pub extern "C" fn nexus_clip_capacity() -> usize {
    MAX_CLIP_EVENTS
}

/// `int32_t nexus_drain_clips(Engine* engine,`
/// `    uint16_t* out_channels, float* out_true_peaks, size_t cap,`
/// `    size_t* out_count);`
///
/// SPEC §6 `audio.clipping`: drain the true-peak clip events accumulated by the
/// realtime `process_block` since the last drain. Writes up to `cap` events as
/// two PARALLEL arrays — `out_channels[i]` is the input channel that clipped and
/// `out_true_peaks[i]` its measured true-peak in dBFS — and sets `*out_count` to
/// the number written. Each event is reported once (the drain removes it). A
/// `cap` of 0 with non-null pointers is valid: it writes nothing and reports 0.
/// The parallel-array form (not an array-of-structs) keeps the ctypes side free
/// of struct-layout/alignment concerns, like the other getters' scalar out-params.
///
/// SAFETY: `out_channels` and `out_true_peaks` must each be valid for `cap`
/// writes (`uint16_t`/`float` respectively) and `out_count` for one `size_t`.
#[no_mangle]
pub unsafe extern "C" fn nexus_drain_clips(
    engine: *mut Engine,
    out_channels: *mut u16,
    out_true_peaks: *mut f32,
    cap: usize,
    out_count: *mut usize,
) -> i32 {
    guard(|| {
        let e = engine_mut!(engine);
        if out_count.is_null() {
            return codes::NULL_POINTER;
        }
        // With a positive cap the data pointers must be valid; a zero cap needs no
        // data buffer (report "no events written").
        if cap > 0 && (out_channels.is_null() || out_true_peaks.is_null()) {
            return codes::NULL_POINTER;
        }
        // Drain into a fixed stack buffer (no heap alloc; control-plane thread).
        // Bounded by the engine's capacity so `want <= MAX_CLIP_EVENTS` always.
        let mut tmp = [ClipEvent { channel: 0, true_peak_dbfs: 0.0 }; MAX_CLIP_EVENTS];
        let want = cap.min(e.clip_capacity());
        let n = e.drain_clips(&mut tmp[..want]);
        // Scatter into the caller's parallel arrays. SAFETY: caller guarantees each
        // array is valid for `cap` >= `want` >= `n` writes.
        for (i, ev) in tmp[..n].iter().enumerate() {
            unsafe {
                *out_channels.add(i) = ev.channel;
                *out_true_peaks.add(i) = ev.true_peak_dbfs;
            }
        }
        unsafe { *out_count = n };
        codes::OK
    })
}

/// `int32_t nexus_get_crosspoint(const Engine*, size_t in, size_t out, float* out_gain_db);`
/// Read one crosspoint (for `state.get` serialization on the Python side).
#[no_mangle]
pub unsafe extern "C" fn nexus_get_crosspoint(
    engine: *const Engine,
    input: usize,
    output: usize,
    out_gain_db: *mut f32,
) -> i32 {
    guard(|| {
        let e = engine_ref!(engine);
        if out_gain_db.is_null() {
            return codes::NULL_POINTER;
        }
        match e.matrix().crosspoint(input, output) {
            Ok(g) => {
                unsafe { *out_gain_db = g };
                codes::OK
            }
            Err(err) => err.code(),
        }
    })
}

/// `uint64_t nexus_matrix_revision(const Engine*);` — the matrix revision
/// counter so the Python `audio.routes` publisher can skip unchanged snapshots.
/// Returns 0 on a null handle (indistinguishable from a fresh engine, which is
/// acceptable: the caller validates the handle once at create).
#[no_mangle]
pub unsafe extern "C" fn nexus_matrix_revision(engine: *const Engine) -> u64 {
    if engine.is_null() {
        return 0;
    }
    // SAFETY: non-null engine created by nexus_engine_create.
    let e = unsafe { &*engine };
    e.matrix().revision()
}

/// `int32_t nexus_preset_save_path(Engine*, const char* path);` and
/// `int32_t nexus_preset_load_path(Engine*, const char* path);` — preset TOML
/// I/O (SPEC §5 `preset.save`/`preset.load`). DECLARED here as the FFI seam; the
/// Python side may instead serialize via the crosspoint getters and write TOML
/// itself (it owns the `apps/nexus/presets` fs grant). The preset agent fills
/// the body. STUB returns PRESET_ERROR so an un-filled build cannot silently
/// "succeed".
///
/// SAFETY: `path` must be a valid NUL-terminated C string for the call.
#[no_mangle]
pub unsafe extern "C" fn nexus_preset_save_path(engine: *mut Engine, path: *const c_char) -> i32 {
    guard(|| {
        let _e = engine_mut!(engine);
        if path.is_null() {
            return codes::NULL_POINTER;
        }
        // STUB: the preset agent fills TOML serialization here, OR the Python
        // side does preset I/O itself via the getters (it holds the fs grant).
        codes::PRESET_ERROR
    })
}

/// See [`nexus_preset_save_path`]. SAFETY: same `path` contract.
#[no_mangle]
pub unsafe extern "C" fn nexus_preset_load_path(engine: *mut Engine, path: *const c_char) -> i32 {
    guard(|| {
        let _e = engine_mut!(engine);
        if path.is_null() {
            return codes::NULL_POINTER;
        }
        codes::PRESET_ERROR // STUB; see save.
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_destroy_roundtrip() {
        let e = nexus_engine_create(2, 2, 48_000);
        assert!(!e.is_null());
        let mut ins = 0usize;
        // SAFETY: `e` is a live handle from create; out-ptr is a valid local.
        unsafe {
            assert_eq!(nexus_engine_inputs(e, &mut ins), codes::OK);
            assert_eq!(ins, 2);
            nexus_engine_destroy(e);
        }
    }

    #[test]
    fn null_handle_is_rejected_not_crashed() {
        let null: *mut Engine = std::ptr::null_mut();
        // SAFETY: null handles are checked and rejected before any deref.
        unsafe {
            assert_eq!(nexus_set_crosspoint(null, 0, 0, 0.0), codes::INVALID_HANDLE);
            let mut out = 0usize;
            assert_eq!(nexus_engine_inputs(std::ptr::null(), &mut out), codes::INVALID_HANDLE);
            nexus_engine_destroy(null); // no-op, no crash
        }
    }

    #[test]
    fn crosspoint_set_then_get_via_ffi() {
        let e = nexus_engine_create(2, 2, 48_000);
        // SAFETY: `e` is a live handle from create; out-ptr is a valid local.
        unsafe {
            assert_eq!(nexus_set_crosspoint(e, 0, 1, -6.0), codes::OK);
            let mut g = 0.0f32;
            assert_eq!(nexus_get_crosspoint(e, 0, 1, &mut g), codes::OK);
            assert_eq!(g, -6.0);
            // Out-of-range index returns the matrix's OUT_OF_BOUNDS code.
            assert_eq!(nexus_set_crosspoint(e, 9, 0, 0.0), codes::OUT_OF_BOUNDS);
            nexus_engine_destroy(e);
        }
    }

    #[test]
    fn spectrum_buffer_too_small_rejected() {
        let e = nexus_engine_create(1, 1, 48_000);
        let mut small = [0.0f32; 8];
        // SAFETY: `e` is a live handle; buffers are valid locals sized as passed.
        unsafe {
            assert_eq!(
                nexus_get_spectrum(e, small.as_mut_ptr(), small.len()),
                codes::OUT_OF_BOUNDS
            );
            let mut full = [0.0f32; SPECTRUM_BANDS_HINT];
            assert_eq!(nexus_get_spectrum(e, full.as_mut_ptr(), full.len()), codes::OK);
            nexus_engine_destroy(e);
        }
    }

    #[test]
    fn abi_version_is_exposed() {
        assert_eq!(nexus_abi_version(), NEXUS_ABI_VERSION);
        assert_eq!(nexus_spectrum_band_count(), SPECTRUM_BANDS_HINT);
        // v2 exposes the clip-drain surface; capacity matches the engine's bound.
        assert_eq!(nexus_clip_capacity(), MAX_CLIP_EVENTS);
    }

    #[test]
    fn drain_clips_empty_then_after_clipping() {
        let e = nexus_engine_create(1, 1, 48_000);
        assert!(!e.is_null());
        let cap = nexus_clip_capacity();
        let mut chans = vec![0u16; cap];
        let mut peaks = vec![0.0f32; cap];
        // SAFETY: `e` is a live handle; buffers are valid locals sized to `cap`.
        unsafe {
            // A fresh engine has no clip events.
            let mut count = 99usize;
            assert_eq!(
                nexus_drain_clips(e, chans.as_mut_ptr(), peaks.as_mut_ptr(), cap, &mut count),
                codes::OK
            );
            assert_eq!(count, 0);

            // Push one full-scale block through the FFI process path: it clips.
            assert_eq!(nexus_set_crosspoint(e, 0, 0, 0.0), codes::OK);
            assert_eq!(nexus_set_monitor_output(e, 0), codes::OK);
            let mut inbuf = vec![1.0f32; 64];
            let mut outbuf = vec![0.0f32; 64];
            let in_ptrs = [inbuf.as_mut_ptr() as *const f32];
            let out_ptrs = [outbuf.as_mut_ptr()];
            assert_eq!(
                nexus_process_block(e, in_ptrs.as_ptr(), 1, out_ptrs.as_ptr(), 1, 64, 1, 48_000),
                codes::OK
            );

            // Now a drain reports the clip once, then clears.
            let mut count2 = 0usize;
            assert_eq!(
                nexus_drain_clips(e, chans.as_mut_ptr(), peaks.as_mut_ptr(), cap, &mut count2),
                codes::OK
            );
            assert!(count2 >= 1, "expected a clip event after a full-scale block");
            assert_eq!(chans[0], 0);
            assert!(peaks[0].is_finite());

            let mut count3 = 7usize;
            assert_eq!(
                nexus_drain_clips(e, chans.as_mut_ptr(), peaks.as_mut_ptr(), cap, &mut count3),
                codes::OK
            );
            assert_eq!(count3, 0, "clip events must be reported once");

            // Null out_count is rejected without a deref crash.
            assert_eq!(
                nexus_drain_clips(e, chans.as_mut_ptr(), peaks.as_mut_ptr(), cap, std::ptr::null_mut()),
                codes::NULL_POINTER
            );
            nexus_engine_destroy(e);
        }
    }
}

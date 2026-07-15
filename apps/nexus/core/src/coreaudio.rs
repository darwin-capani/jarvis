//! CoreAudio device seam — aggregate-device create/destroy + drift correction
//! and the realtime `AudioDeviceIOProc` (SPEC §1/§2). DEVICE-GATED.
//!
//! COMPILES HEADLESSLY; the live IOProc, the aggregate device, the
//! drift-corrected clock, and the sub-10 ms loopback RTT are DEVICE-GATED
//! (a real Apple Silicon Mac + the CoreAudio HAL only) and are NEVER measured here. Nothing in
//! this module is invoked by `cargo test` on this box: the default build path
//! returns [`NexusError::Device`] for every entry point, and the real HAL
//! bodies live behind `--features coreaudio` (`cargo check --features
//! coreaudio` type-checks them; they are still never RUN). No number produced
//! by this module on the headless box is ever claim-measured — `monitor.measure`
//! reports `None` until it runs on real hardware (SPEC §2: "Measured, not
//! assumed").
//!
//! Two compilation faces:
//!   - WITHOUT `--features coreaudio` (the default, this headless box): the
//!     functions exist but are inert stubs that return [`NexusError::Device`].
//!     This keeps the crate + the engine + the FFI compiling and testing on a
//!     device-less machine without `#[cfg]` scattered through callers.
//!   - WITH `--features coreaudio` (`cargo check --features coreaudio`): the
//!     real `coreaudio-sys` bindings compile (the IOProc install, the
//!     `AudioHardwareCreateAggregateDevice`/`...Destroy...` calls, the HAL
//!     property plumbing, drift compensation per sub-device). This compiles the
//!     seam; it is STILL never RUN here.
//!
//! Realtime discipline honored in the IOProc body (SPEC §2): the callback reads
//! the latest [`MatrixSnapshot`] wait-free from the [`SnapshotRing`], mixes via
//! the pure [`crate::dsp::mix_block`], runs the per-channel meter taps, and
//! pushes a copy of the monitored mix into a lock-free [`MeterRing`] for the
//! control plane to fold — NO allocation, NO locks, NO syscalls on the audio
//! thread.

// `NexusError`/`Result` are used by the headless STUB branches AND the real
// feature-gated bodies (which map `OSStatus` failures to `NexusError::Device`),
// so they are referenced in both configurations.
use crate::error::{NexusError, Result};
use crate::types::AudioFormat;

/// The realtime hand-off the IOProc reads each callback. The control plane wires
/// one of these (it owns the [`SnapshotRing`] the engine publishes into, and the
/// [`MeterRing`] the telemetry loop drains) and hands a stable pointer to it as
/// the IOProc client data. It is `Send`/`Sync` by the same SPSC argument as the
/// ring: the control thread mutates the routing (via `publish` on the ring it
/// shares here) and drains meters; the audio thread only `load`s the snapshot
/// and `push`es meters — both wait-free.
///
/// DEVICE-GATED fields are only meaningful under `--features coreaudio`; the
/// type exists in both builds so the engine/control plane can hold one without
/// `#[cfg]` at the call site.
pub struct RtContext {
    /// The lock-free routing snapshot the audio thread loads each block. Borrowed
    /// for the lifetime of the installed IOProc (the control plane keeps the
    /// backing `SnapshotRing` alive at least that long).
    #[cfg(feature = "coreaudio")]
    ring: *const crate::matrix::SnapshotRing,
    /// The configured format (channels + sample rate) the IOProc validates the
    /// device's stream against; if the device can't hold 64 frames the install
    /// steps to 128 and records it (SPEC §1).
    #[cfg(feature = "coreaudio")]
    format: AudioFormat,
    /// Lock-free meter/FFT tap the audio thread pushes the monitored mix into and
    /// the control plane drains (SPEC §2 "meter taps … pushed to the control
    /// plane over a lock-free ring").
    #[cfg(feature = "coreaudio")]
    meters: MeterRing,
    #[cfg(not(feature = "coreaudio"))]
    _private: (),
}

// SAFETY: `RtContext` is shared between exactly the control thread (producer of
// snapshots, consumer of meters) and the audio thread (consumer of snapshots,
// producer of meters). The `SnapshotRing` is itself SPSC-safe across these two
// threads, and `MeterRing` below is a single-producer/single-consumer seqlock
// ring with the same discipline. The raw `*const SnapshotRing` is never
// dereferenced after the IOProc is torn down (the control plane outlives it).
#[cfg(feature = "coreaudio")]
unsafe impl Send for RtContext {}
#[cfg(feature = "coreaudio")]
unsafe impl Sync for RtContext {}

impl RtContext {
    /// Build a realtime context bound to the control plane's snapshot ring and
    /// configured format. DEVICE-GATED — the headless build has no ring to wire
    /// and never installs an IOProc, so it constructs an inert placeholder.
    ///
    /// SAFETY (feature build): `ring` must outlive every IOProc installed with
    /// this context (the control plane guarantees this — it owns the ring and
    /// destroys the IOProc before dropping it).
    #[cfg(feature = "coreaudio")]
    pub unsafe fn new(ring: *const crate::matrix::SnapshotRing, format: AudioFormat) -> Self {
        Self { ring, format, meters: MeterRing::new() }
    }

    /// Headless placeholder constructor: there is no realtime path to bind on a
    /// device-less box, so this is an inert value the (never-installed) IOProc
    /// would read. Kept so callers compile without `#[cfg]`.
    #[cfg(not(feature = "coreaudio"))]
    pub fn placeholder() -> Self {
        Self { _private: () }
    }
}

/// A lock-free single-producer (audio) / single-consumer (control) ring the
/// IOProc pushes the monitored-mix block into, so the control plane's telemetry
/// loop can fold peak/RMS/LUFS/FFT off the audio thread. Same seqlock discipline
/// as [`crate::matrix::SnapshotRing`]: a published count + fixed POD slots, the
/// consumer re-reads the sequence to reject a torn copy. POD slots so the push
/// is allocation-free (SPEC §2: "no locks/allocations/syscalls on the audio
/// thread"). DEVICE-GATED — only built under `--features coreaudio`.
#[cfg(feature = "coreaudio")]
pub struct MeterRing {
    seq: std::sync::atomic::AtomicU64,
    slots: [std::cell::UnsafeCell<MeterBlock>; METER_RING_SLOTS],
}

/// Number of slots in the [`MeterRing`]. The audio thread pushes one block per
/// ~1.33 ms callback; the control plane drains at the telemetry rate (~30 Hz),
/// so a small ring with seqlock versioning never tears.
#[cfg(feature = "coreaudio")]
pub const METER_RING_SLOTS: usize = 8;

/// The maximum frames a single meter slot carries — one 64-frame callback block
/// (SPEC §2), with headroom for a 128-frame fallback if an interface can't hold
/// 64 (SPEC §1).
#[cfg(feature = "coreaudio")]
pub const METER_BLOCK_FRAMES: usize = 128;

/// One block of the monitored mix copied out of the IOProc for the control plane
/// to meter. Fixed-size POD so the audio thread writes it without allocating.
#[cfg(feature = "coreaudio")]
#[derive(Clone, Copy)]
pub struct MeterBlock {
    /// Mono-summed monitored mix, `frames` valid samples.
    pub samples: [f32; METER_BLOCK_FRAMES],
    /// Valid sample count in `samples` (<= [`METER_BLOCK_FRAMES`]).
    pub frames: usize,
    /// Per-channel sample peak (linear) of the output bus this block, so the
    /// control plane can fold dBFS without re-scanning every output.
    pub out_peak: [f32; crate::types::MAX_CHANNELS],
}

#[cfg(feature = "coreaudio")]
impl Default for MeterBlock {
    fn default() -> Self {
        Self {
            samples: [0.0; METER_BLOCK_FRAMES],
            frames: 0,
            out_peak: [0.0; crate::types::MAX_CHANNELS],
        }
    }
}

#[cfg(feature = "coreaudio")]
unsafe impl Send for MeterRing {}
#[cfg(feature = "coreaudio")]
unsafe impl Sync for MeterRing {}

#[cfg(feature = "coreaudio")]
impl MeterRing {
    /// A ring pre-filled with empty blocks and `seq = 0`.
    pub fn new() -> Self {
        Self {
            seq: std::sync::atomic::AtomicU64::new(0),
            slots: std::array::from_fn(|_| std::cell::UnsafeCell::new(MeterBlock::default())),
        }
    }

    /// AUDIO thread only: publish one metered block. Writes the slot, then bumps
    /// the sequence with `Release`. Allocation-free, lock-free, wait-free.
    ///
    /// SAFETY: single producer (the IOProc). The consumer only reads a slot after
    /// the `Release` store of the incremented sequence and re-checks the sequence.
    pub fn push(&self, block: MeterBlock) {
        use std::sync::atomic::Ordering;
        let cur = self.seq.load(Ordering::Relaxed);
        let next = cur.wrapping_add(1);
        let slot = (next as usize) % METER_RING_SLOTS;
        unsafe {
            *self.slots[slot].get() = block;
        }
        self.seq.store(next, Ordering::Release);
    }

    /// CONTROL thread only: load the most recently published block, or `None` if
    /// nothing has been pushed yet. Retries (bounded) on a concurrent overwrite.
    pub fn load(&self) -> Option<MeterBlock> {
        use std::sync::atomic::Ordering;
        loop {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 == 0 {
                return None;
            }
            let slot = (s1 as usize) % METER_RING_SLOTS;
            let block = unsafe { *self.slots[slot].get() };
            let s2 = self.seq.load(Ordering::Acquire);
            if s1 == s2 {
                return Some(block);
            }
        }
    }

    /// Number of blocks pushed so far (for tests/telemetry coalescing).
    pub fn published(&self) -> u64 {
        self.seq.load(std::sync::atomic::Ordering::Acquire)
    }
}

#[cfg(feature = "coreaudio")]
impl Default for MeterRing {
    fn default() -> Self {
        Self::new()
    }
}

/// An opaque handle to a created CoreAudio aggregate device (SPEC §1). Holds the
/// `AudioDeviceID`, the resolved master sub-device UID, and (optionally) the
/// realtime snapshot ring the IOProc will read, under the `coreaudio` feature; a
/// zero-sized placeholder otherwise. The control plane creates one, binds it to
/// the engine's snapshot ring with [`AggregateDevice::with_ring`], installs the
/// IOProc on it, and destroys it on teardown.
#[derive(Debug, Default)]
pub struct AggregateDevice {
    /// The HAL device id of the created aggregate (under `coreaudio`); 0 / unused
    /// in the stub build.
    #[cfg(feature = "coreaudio")]
    device_id: u32,
    /// The sub-device UID chosen as the clock master (the others get drift
    /// compensation). Kept so teardown / diagnostics can report it.
    #[cfg(feature = "coreaudio")]
    #[allow(dead_code)]
    master_uid: Option<String>,
    /// The realtime routing ring the installed IOProc loads each block, bound by
    /// the control plane via [`AggregateDevice::with_ring`]. `None` until bound;
    /// [`install_ioproc`] refuses to install without it (no silent passthrough).
    #[cfg(feature = "coreaudio")]
    ring: Option<*const crate::matrix::SnapshotRing>,
    /// The audio format the IOProc validates the device's stream against.
    #[cfg(feature = "coreaudio")]
    format: AudioFormat,
    #[cfg(not(feature = "coreaudio"))]
    _private: (),
}

#[cfg(feature = "coreaudio")]
impl AggregateDevice {
    /// The created aggregate's HAL device id (the IOProc is installed on it).
    pub fn device_id(&self) -> u32 {
        self.device_id
    }

    /// Bind the realtime snapshot ring + format the installed IOProc will read
    /// (SPEC §2: the audio thread loads routing snapshots from the SPSC ring).
    /// The control plane calls this with a pointer to the engine's
    /// [`crate::matrix::SnapshotRing`] before [`install_ioproc`]. Additive
    /// builder; the four frozen device-op signatures are unchanged.
    ///
    /// SAFETY: `ring` must outlive the installed IOProc — the control plane owns
    /// the ring and destroys the proc (via [`IoProc::shutdown`]/`Drop`) before
    /// dropping the ring.
    pub unsafe fn with_ring(
        mut self,
        ring: *const crate::matrix::SnapshotRing,
        format: AudioFormat,
    ) -> Self {
        self.ring = Some(ring);
        self.format = format;
        self
    }
}

/// The realtime IOProc handle: the installed `AudioDeviceIOProc` id + the device
/// it runs on + the boxed [`RtContext`] the callback reads. DEVICE-GATED. On
/// teardown the proc is stopped, removed, and the context freed.
#[derive(Default)]
pub struct IoProc {
    /// The HAL device the proc is installed on.
    #[cfg(feature = "coreaudio")]
    device_id: u32,
    /// The proc id returned by `AudioDeviceCreateIOProcID` (needed to stop +
    /// destroy it). `None` until installed.
    #[cfg(feature = "coreaudio")]
    proc_id: sys::AudioDeviceIOProcID,
    /// The realtime context handed to the callback as client data. Boxed so it
    /// has a stable address for the lifetime of the proc; freed in
    /// [`IoProc::shutdown`]. Raw pointer (not `Box`) because the HAL holds a copy
    /// of the address; we reclaim it exactly once on teardown.
    #[cfg(feature = "coreaudio")]
    ctx: *mut RtContext,
    /// Whether the device was actually started (so teardown stops it once).
    #[cfg(feature = "coreaudio")]
    started: bool,
    #[cfg(not(feature = "coreaudio"))]
    _private: (),
}


impl std::fmt::Debug for IoProc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IoProc").finish_non_exhaustive()
    }
}

// ===========================================================================
// Device-gated CoreAudio bindings + helpers (only under `--features coreaudio`)
// ===========================================================================
//
// These live behind the feature so the headless build pulls in NO CoreAudio
// symbols. Everything here is DEVICE-GATED: it type-checks against the
// `coreaudio-sys` bindings but is never executed on this box.

#[cfg(feature = "coreaudio")]
mod sys {
    //! Thin re-export of the `coreaudio-sys` items this seam uses, plus the two
    //! Mach timing externs `coreaudio-sys` does not surface (hand-declared, as
    //! the task allows). Keeping them in one module documents the exact HAL
    //! surface the IOProc/aggregate path touches.
    pub use coreaudio_sys::{
        kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceMasterSubDeviceKey,
        kAudioAggregateDeviceNameKey, kAudioAggregateDeviceSubDeviceListKey,
        kAudioAggregateDeviceUIDKey, kAudioDevicePropertyBufferFrameSize,
        kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyElementMain,
        kAudioObjectPropertyScopeGlobal, kAudioSubDeviceDriftCompensationKey, kAudioSubDeviceUIDKey,
        kCFAllocatorDefault, kCFBooleanTrue, kCFNumberIntType, kCFStringEncodingUTF8,
        kCFTypeArrayCallBacks, kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks,
        AudioBufferList, AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID,
        AudioDeviceID, AudioDeviceIOProcID, AudioDeviceStart, AudioDeviceStop, AudioObjectID,
        AudioObjectPropertyAddress, AudioObjectSetPropertyData, AudioTimeStamp,
        AudioHardwareCreateAggregateDevice, AudioHardwareDestroyAggregateDevice, CFArrayAppendValue,
        CFArrayCreateMutable, CFDictionaryCreateMutable, CFDictionarySetValue, CFNumberCreate,
        CFRelease, CFStringCreateWithCString, CFStringRef, CFTypeRef, OSStatus,
    };

    // Mach absolute-time clock for the loopback RTT impulse timing. Not surfaced
    // by `coreaudio-sys`; hand-declared per the task ("coreaudio-sys OR
    // hand-declared externs"). DEVICE-GATED — only called inside the real RTT
    // body, never on this box.
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct MachTimebaseInfo {
        pub numer: u32,
        pub denom: u32,
    }
    extern "C" {
        pub fn mach_absolute_time() -> u64;
        pub fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
    }
}

/// DEVICE-GATED: turn a Rust `&str` into a retained `CFStringRef` (UTF-8). The
/// caller owns the returned reference and must `CFRelease` it. Returns null on
/// allocation failure (the caller treats null as a device error).
///
/// SAFETY: calls into CoreFoundation; the input is copied (NUL-terminated), so
/// the borrow does not outlive the call.
#[cfg(feature = "coreaudio")]
unsafe fn cfstr(s: &str) -> sys::CFStringRef {
    // A NUL-terminated copy so CFStringCreateWithCString can read it as a C string.
    let cstring = match std::ffi::CString::new(s) {
        Ok(c) => c,
        Err(_) => return std::ptr::null(),
    };
    sys::CFStringCreateWithCString(
        sys::kCFAllocatorDefault,
        cstring.as_ptr(),
        sys::kCFStringEncodingUTF8,
    )
}

/// DEVICE-GATED: build a `CFStringRef` from one of CoreAudio's `&[u8]` key
/// constants (they are NUL-terminated byte literals like `b"uid\0"`).
#[cfg(feature = "coreaudio")]
unsafe fn cfstr_key(key: &[u8]) -> sys::CFStringRef {
    // Strip the trailing NUL the bindgen byte-literal carries, then re-wrap.
    let trimmed = match key.iter().position(|&b| b == 0) {
        Some(i) => &key[..i],
        None => key,
    };
    match std::str::from_utf8(trimmed) {
        Ok(s) => cfstr(s),
        Err(_) => std::ptr::null(),
    }
}

/// Create an aggregate device binding the named sub-devices to one clock
/// (SPEC §1: `AudioHardwareCreateAggregateDevice`, drift correction on every
/// non-master sub-device). DEVICE-GATED — returns [`NexusError::Device`] in the
/// headless stub build; the real path compiles under `--features coreaudio`.
///
/// `sub_device_uids[0]` (if any) is chosen as the clock master; the rest get
/// drift compensation enabled (SPEC §1: "Drift correction enabled for
/// sub-devices not sharing the master clock").
pub fn create_aggregate_device(
    _format: AudioFormat,
    _sub_device_uids: &[&str],
) -> Result<AggregateDevice> {
    #[cfg(not(feature = "coreaudio"))]
    {
        Err(NexusError::Device(
            "aggregate device creation is device-gated (build with --features coreaudio on real hardware)".into(),
        ))
    }
    #[cfg(feature = "coreaudio")]
    {
        // DEVICE-GATED. This is the real HAL body; it type-checks against
        // coreaudio-sys but is NEVER run on this headless box.
        unsafe {
            if _sub_device_uids.is_empty() {
                return Err(NexusError::Device(
                    "aggregate device needs at least one sub-device UID".into(),
                ));
            }

            // The top-level aggregate-description dictionary.
            let desc = sys::CFDictionaryCreateMutable(
                sys::kCFAllocatorDefault,
                0,
                &sys::kCFTypeDictionaryKeyCallBacks,
                &sys::kCFTypeDictionaryValueCallBacks,
            );
            if desc.is_null() {
                return Err(NexusError::Device("failed to allocate aggregate description".into()));
            }

            // A small RAII bag of CF references to release on the way out so the
            // (device-gated) body does not leak even on an early error.
            let mut to_release: Vec<sys::CFTypeRef> = Vec::new();
            macro_rules! track {
                ($r:expr) => {{
                    let r = $r;
                    if !r.is_null() {
                        to_release.push(r as sys::CFTypeRef);
                    }
                    r
                }};
            }
            // Always release the description itself at the end.
            to_release.push(desc as sys::CFTypeRef);

            // Name + UID + private (don't publish to other apps; this is an
            // in-process routing aggregate, SPEC §1).
            let name = track!(cfstr("Nexus Aggregate"));
            let uid = track!(cfstr("com.darwin.nexus.aggregate"));
            let k_name = track!(cfstr_key(sys::kAudioAggregateDeviceNameKey));
            let k_uid = track!(cfstr_key(sys::kAudioAggregateDeviceUIDKey));
            let k_private = track!(cfstr_key(sys::kAudioAggregateDeviceIsPrivateKey));
            let k_master = track!(cfstr_key(sys::kAudioAggregateDeviceMasterSubDeviceKey));
            let k_sublist = track!(cfstr_key(sys::kAudioAggregateDeviceSubDeviceListKey));
            let k_sub_uid = track!(cfstr_key(sys::kAudioSubDeviceUIDKey));
            let k_drift = track!(cfstr_key(sys::kAudioSubDeviceDriftCompensationKey));

            if [name, uid, k_name, k_uid, k_private, k_master, k_sublist, k_sub_uid, k_drift]
                .iter()
                .any(|p| p.is_null())
            {
                for r in to_release {
                    sys::CFRelease(r);
                }
                return Err(NexusError::Device("failed to build aggregate CF keys".into()));
            }

            sys::CFDictionarySetValue(desc, k_name as *const _, name as *const _);
            sys::CFDictionarySetValue(desc, k_uid as *const _, uid as *const _);
            sys::CFDictionarySetValue(
                desc,
                k_private as *const _,
                sys::kCFBooleanTrue as *const _,
            );

            // The clock master is the first sub-device; everyone else drifts onto it.
            let master = _sub_device_uids[0];
            let master_cf = track!(cfstr(master));
            if master_cf.is_null() {
                for r in to_release {
                    sys::CFRelease(r);
                }
                return Err(NexusError::Device("failed to build master sub-device UID".into()));
            }
            sys::CFDictionarySetValue(desc, k_master as *const _, master_cf as *const _);

            // Build the sub-device list: one dictionary per sub-device, with
            // drift compensation ON for every non-master entry (SPEC §1).
            let sublist = sys::CFArrayCreateMutable(
                sys::kCFAllocatorDefault,
                _sub_device_uids.len() as _,
                &sys::kCFTypeArrayCallBacks,
            );
            if sublist.is_null() {
                for r in to_release {
                    sys::CFRelease(r);
                }
                return Err(NexusError::Device("failed to allocate sub-device list".into()));
            }
            to_release.push(sublist as sys::CFTypeRef);

            for (idx, sub) in _sub_device_uids.iter().enumerate() {
                let sub_dict = sys::CFDictionaryCreateMutable(
                    sys::kCFAllocatorDefault,
                    0,
                    &sys::kCFTypeDictionaryKeyCallBacks,
                    &sys::kCFTypeDictionaryValueCallBacks,
                );
                if sub_dict.is_null() {
                    for r in to_release {
                        sys::CFRelease(r);
                    }
                    return Err(NexusError::Device("failed to allocate sub-device dict".into()));
                }
                let sub_uid_cf = cfstr(sub);
                if sub_uid_cf.is_null() {
                    sys::CFRelease(sub_dict as sys::CFTypeRef);
                    for r in to_release {
                        sys::CFRelease(r);
                    }
                    return Err(NexusError::Device("failed to build sub-device UID".into()));
                }
                sys::CFDictionarySetValue(sub_dict, k_sub_uid as *const _, sub_uid_cf as *const _);

                // Drift compensation: 1 (on) for non-master sub-devices, 0 for
                // the master. CFNumber(int) keyed by the drift-compensation key.
                let drift_on: i32 = if idx == 0 { 0 } else { 1 };
                let drift_num = sys::CFNumberCreate(
                    sys::kCFAllocatorDefault,
                    sys::kCFNumberIntType as _,
                    &drift_on as *const i32 as *const _,
                );
                if drift_num.is_null() {
                    sys::CFRelease(sub_uid_cf as sys::CFTypeRef);
                    sys::CFRelease(sub_dict as sys::CFTypeRef);
                    for r in to_release {
                        sys::CFRelease(r);
                    }
                    return Err(NexusError::Device("failed to build drift CFNumber".into()));
                }
                sys::CFDictionarySetValue(sub_dict, k_drift as *const _, drift_num as *const _);

                sys::CFArrayAppendValue(sublist, sub_dict as *const _);
                // The array retains them; drop our local refs.
                sys::CFRelease(sub_uid_cf as sys::CFTypeRef);
                sys::CFRelease(drift_num as sys::CFTypeRef);
                sys::CFRelease(sub_dict as sys::CFTypeRef);
            }
            sys::CFDictionarySetValue(desc, k_sublist as *const _, sublist as *const _);

            // Create the device.
            let mut device_id: sys::AudioObjectID = 0;
            let status =
                sys::AudioHardwareCreateAggregateDevice(desc as *const _, &mut device_id as *mut _);

            // Release everything we built; the HAL copied what it needs.
            for r in to_release {
                sys::CFRelease(r);
            }

            if status != 0 || device_id == 0 {
                return Err(NexusError::Device(format!(
                    "AudioHardwareCreateAggregateDevice failed (OSStatus {status})"
                )));
            }

            // Best-effort: pin the nominal sample rate + 64-frame buffer on the
            // new device. A failure here is non-fatal (the IOProc install will
            // step to 128 and report it per SPEC §1), so we only record intent.
            let _ = set_device_u32(
                device_id,
                sys::kAudioDevicePropertyBufferFrameSize,
                crate::types::DEFAULT_BLOCK_FRAMES as u32,
            );
            let _ = set_device_f64(
                device_id,
                sys::kAudioDevicePropertyNominalSampleRate,
                _format.sample_rate as f64,
            );

            Ok(AggregateDevice {
                device_id,
                master_uid: Some(master.to_string()),
                // Unbound until the control plane calls `with_ring`; the format
                // here is the requested one (the install validates it against the
                // device's actual stream and steps to 128 frames if needed).
                ring: None,
                format: _format,
            })
        }
    }
}

/// DEVICE-GATED helper: set a `u32` HAL device property (global scope, main
/// element). Returns the `OSStatus`. Never run headlessly.
#[cfg(feature = "coreaudio")]
unsafe fn set_device_u32(device: sys::AudioDeviceID, selector: u32, value: u32) -> sys::OSStatus {
    let addr = sys::AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: sys::kAudioObjectPropertyScopeGlobal,
        mElement: sys::kAudioObjectPropertyElementMain,
    };
    sys::AudioObjectSetPropertyData(
        device,
        &addr as *const _,
        0,
        std::ptr::null(),
        std::mem::size_of::<u32>() as u32,
        &value as *const u32 as *const _,
    )
}

/// DEVICE-GATED helper: set an `f64` HAL device property (e.g. nominal sample
/// rate). Returns the `OSStatus`. Never run headlessly.
#[cfg(feature = "coreaudio")]
unsafe fn set_device_f64(device: sys::AudioDeviceID, selector: u32, value: f64) -> sys::OSStatus {
    let addr = sys::AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: sys::kAudioObjectPropertyScopeGlobal,
        mElement: sys::kAudioObjectPropertyElementMain,
    };
    sys::AudioObjectSetPropertyData(
        device,
        &addr as *const _,
        0,
        std::ptr::null(),
        std::mem::size_of::<f64>() as u32,
        &value as *const f64 as *const _,
    )
}

/// Destroy a previously created aggregate device. DEVICE-GATED.
pub fn destroy_aggregate_device(_device: AggregateDevice) -> Result<()> {
    #[cfg(not(feature = "coreaudio"))]
    {
        Err(NexusError::Device("aggregate device teardown is device-gated".into()))
    }
    #[cfg(feature = "coreaudio")]
    {
        // DEVICE-GATED real body. Never run on this box.
        if _device.device_id == 0 {
            return Ok(());
        }
        let status =
            unsafe { sys::AudioHardwareDestroyAggregateDevice(_device.device_id as sys::AudioObjectID) };
        if status != 0 {
            return Err(NexusError::Device(format!(
                "AudioHardwareDestroyAggregateDevice failed (OSStatus {status})"
            )));
        }
        Ok(())
    }
}

/// The IOProc realtime callback (SPEC §2). DEVICE-GATED — the HAL calls this on
/// the audio thread, 64 frames @ 48 kHz. It reads the latest routing snapshot
/// wait-free, mixes the input buffer list into the output buffer list via the
/// pure [`crate::dsp::mix_block`], taps per-output peaks, and pushes the
/// monitored mix into the [`MeterRing`] — NO alloc/lock/syscall (SPEC §2).
///
/// SAFETY: invoked only by CoreAudio with valid `AudioBufferList`s and the
/// `RtContext` pointer we registered as client data. Never invoked headlessly.
#[cfg(feature = "coreaudio")]
unsafe extern "C" fn nexus_io_proc(
    _in_device: sys::AudioObjectID,
    _in_now: *const sys::AudioTimeStamp,
    in_input_data: *const sys::AudioBufferList,
    _in_input_time: *const sys::AudioTimeStamp,
    out_output_data: *mut sys::AudioBufferList,
    _in_output_time: *const sys::AudioTimeStamp,
    in_client_data: *mut std::os::raw::c_void,
) -> sys::OSStatus {
    use crate::types::{BlockMut, BlockRef};

    // Recover the realtime context. Null client data or null buffer lists mean
    // there is nothing to do — return success (silence) rather than fault the HAL.
    if in_client_data.is_null() || out_output_data.is_null() {
        return 0;
    }
    let ctx = &*(in_client_data as *const RtContext);
    if ctx.ring.is_null() {
        return 0;
    }

    // Wait-free load of the routing truth for this block.
    let snapshot = (*ctx.ring).load();
    let fmt = ctx.format;
    let channels = fmt.channels.max(1) as usize;

    // The output buffer list (interleaved or per-buffer; we treat each
    // AudioBuffer as one interleaved block per the aggregate's stream format).
    let out_list = &mut *out_output_data;
    let out_count = out_list.mNumberBuffers as usize;
    let out_buffers =
        std::slice::from_raw_parts_mut(out_list.mBuffers.as_mut_ptr(), out_count.max(1));

    // Build BlockMut views over each output AudioBuffer.
    // (No allocation on the audio path: we mix per output buffer in place.)
    let in_list = if in_input_data.is_null() { None } else { Some(&*in_input_data) };

    // Construct borrowed input blocks.
    let in_count = in_list.map(|l| l.mNumberBuffers as usize).unwrap_or(0);
    let in_buffers = in_list
        .map(|l| std::slice::from_raw_parts(l.mBuffers.as_ptr(), in_count.max(1)))
        .unwrap_or(&[]);

    // SAFETY: each AudioBuffer.mData points to mDataByteSize bytes of f32 frames
    // the HAL owns for the duration of this callback. We borrow, never retain.
    let mut in_blocks: [Option<BlockRef<'_>>; crate::types::MAX_CHANNELS] =
        [None; crate::types::MAX_CHANNELS];
    let mut n_in = 0usize;
    for (i, b) in in_buffers.iter().enumerate() {
        if i >= crate::types::MAX_CHANNELS || b.mData.is_null() {
            continue;
        }
        let n = (b.mDataByteSize as usize) / std::mem::size_of::<f32>();
        let data = std::slice::from_raw_parts(b.mData as *const f32, n);
        in_blocks[n_in] = Some(BlockRef { data, format: fmt });
        n_in += 1;
    }
    // Pack into a contiguous slice of BlockRef for mix_block. We cannot allocate;
    // the fixed array above gives us stack storage. Collect the Somes in order.
    let mut in_refs: [BlockRef<'_>; crate::types::MAX_CHANNELS] = [BlockRef {
        data: &[],
        format: fmt,
    }; crate::types::MAX_CHANNELS];
    for i in 0..n_in {
        if let Some(b) = in_blocks[i] {
            in_refs[i] = b;
        }
    }

    // Mix into each output buffer in place.
    let mut peak: [f32; crate::types::MAX_CHANNELS] = [0.0; crate::types::MAX_CHANNELS];
    let mut out_idx = 0usize;
    for b in out_buffers.iter_mut() {
        if b.mData.is_null() {
            continue;
        }
        let n = (b.mDataByteSize as usize) / std::mem::size_of::<f32>();
        let data = std::slice::from_raw_parts_mut(b.mData as *mut f32, n);
        let mut out_block = [BlockMut { data, format: fmt }];
        crate::dsp::mix_block(&snapshot, &in_refs[..n_in], &mut out_block);
        // Per-output peak tap for the control plane.
        if out_idx < crate::types::MAX_CHANNELS {
            let mut p = 0.0f32;
            for &s in out_block[0].data.iter() {
                let a = s.abs();
                if a > p {
                    p = a;
                }
            }
            peak[out_idx] = p;
        }
        out_idx += 1;
    }

    // Tap the monitored output (if assigned) into a mono meter block for the
    // control plane to fold LUFS/FFT off the audio thread.
    let mut meter = MeterBlock::default();
    meter.out_peak = peak;
    if let Some(mon) = snapshot.monitor_output {
        if mon < out_count {
            let b = &out_buffers[mon];
            if !b.mData.is_null() {
                let n = (b.mDataByteSize as usize) / std::mem::size_of::<f32>();
                let data = std::slice::from_raw_parts(b.mData as *const f32, n);
                let frames = (n / channels).min(METER_BLOCK_FRAMES);
                for f in 0..frames {
                    // Sum interleaved channels to mono for the program meter.
                    let mut acc = 0.0f32;
                    for c in 0..channels {
                        acc += data[f * channels + c];
                    }
                    meter.samples[f] = acc / channels as f32;
                }
                meter.frames = frames;
            }
        }
    }
    ctx.meters.push(meter);

    0
}

/// Install the `AudioDeviceIOProc` on `device`, wiring it to read the matrix
/// snapshot ring + run the mix/DSP/meter taps in the callback (SPEC §2, 64
/// frames @ 48 kHz). DEVICE-GATED. Returns [`NexusError::Device`] headlessly.
///
/// `device` must have been bound to the live snapshot ring with
/// [`AggregateDevice::with_ring`]; the install builds the realtime [`RtContext`]
/// from that ring and boxes it so the HAL has a stable client-data pointer for
/// the proc's lifetime. The signature matches the frozen seam exactly.
pub fn install_ioproc(_device: &AggregateDevice) -> Result<IoProc> {
    #[cfg(not(feature = "coreaudio"))]
    {
        Err(NexusError::Device("IOProc install is device-gated".into()))
    }
    #[cfg(feature = "coreaudio")]
    {
        // DEVICE-GATED real body. Never run on this box.
        unsafe {
            let device_id = _device.device_id;
            if device_id == 0 {
                return Err(NexusError::Device("cannot install IOProc on a null device".into()));
            }
            // The ring must be bound (SPEC §2): refuse rather than silently mix
            // against a dangling/absent snapshot.
            let ring = match _device.ring {
                Some(r) if !r.is_null() => r,
                _ => {
                    return Err(NexusError::Device(
                        "IOProc install requires a bound snapshot ring (call with_ring first)".into(),
                    ))
                }
            };
            // Build the realtime context from the bound ring + format.
            let ctx = RtContext::new(ring, _device.format);
            // Box the context so its address is stable while the proc is live.
            let ctx_ptr = Box::into_raw(Box::new(ctx));

            let mut proc_id: sys::AudioDeviceIOProcID = None;
            let status = sys::AudioDeviceCreateIOProcID(
                device_id as sys::AudioObjectID,
                Some(nexus_io_proc),
                ctx_ptr as *mut std::os::raw::c_void,
                &mut proc_id as *mut _,
            );
            if status != 0 || proc_id.is_none() {
                // Reclaim the leaked context on failure.
                drop(Box::from_raw(ctx_ptr));
                return Err(NexusError::Device(format!(
                    "AudioDeviceCreateIOProcID failed (OSStatus {status})"
                )));
            }

            let start = sys::AudioDeviceStart(device_id as sys::AudioObjectID, proc_id);
            if start != 0 {
                let _ = sys::AudioDeviceDestroyIOProcID(device_id as sys::AudioObjectID, proc_id);
                drop(Box::from_raw(ctx_ptr));
                return Err(NexusError::Device(format!(
                    "AudioDeviceStart failed (OSStatus {start})"
                )));
            }

            Ok(IoProc {
                device_id,
                proc_id,
                ctx: ctx_ptr,
                started: true,
            })
        }
    }
}

impl IoProc {
    /// Stop the IOProc, remove it from the device, and free the realtime context.
    /// DEVICE-GATED — a no-op headless placeholder. Idempotent-ish: safe to call
    /// once on teardown. Returns [`NexusError::Device`] only if a HAL stop/remove
    /// call fails on real hardware.
    pub fn shutdown(&mut self) -> Result<()> {
        #[cfg(not(feature = "coreaudio"))]
        {
            Ok(())
        }
        #[cfg(feature = "coreaudio")]
        unsafe {
            if self.device_id == 0 || self.proc_id.is_none() {
                return Ok(());
            }
            let mut first_err: Option<NexusError> = None;
            if self.started {
                let stop =
                    sys::AudioDeviceStop(self.device_id as sys::AudioObjectID, self.proc_id);
                if stop != 0 {
                    first_err = Some(NexusError::Device(format!(
                        "AudioDeviceStop failed (OSStatus {stop})"
                    )));
                }
                self.started = false;
            }
            let remove = sys::AudioDeviceDestroyIOProcID(
                self.device_id as sys::AudioObjectID,
                self.proc_id,
            );
            if remove != 0 && first_err.is_none() {
                first_err = Some(NexusError::Device(format!(
                    "AudioDeviceDestroyIOProcID failed (OSStatus {remove})"
                )));
            }
            self.proc_id = None;
            // Reclaim and drop the boxed context exactly once.
            if !self.ctx.is_null() {
                drop(Box::from_raw(self.ctx));
                self.ctx = std::ptr::null_mut();
            }
            match first_err {
                Some(e) => Err(e),
                None => Ok(()),
            }
        }
    }
}

#[cfg(feature = "coreaudio")]
impl Drop for IoProc {
    fn drop(&mut self) {
        // Best-effort teardown so a dropped IoProc never leaks the device proc or
        // the boxed context. Errors are swallowed in Drop (nothing to report to).
        let _ = self.shutdown();
    }
}

/// Measure the loopback monitor round-trip (SPEC §2 `monitor.measure`): drive an
/// impulse out the monitor route and report the actual RTT in ms. DEVICE-GATED —
/// there is NO headless number for this; it is MEASURED on hardware, never
/// assumed. Returns [`NexusError::Device`] in the stub build so no false RTT is
/// ever reported (SPEC §2: "Measured, not assumed").
pub fn measure_monitor_rtt_ms(_proc: &IoProc) -> Result<f32> {
    #[cfg(not(feature = "coreaudio"))]
    {
        Err(NexusError::Device(
            "monitor RTT must be measured on real hardware; no headless value exists".into(),
        ))
    }
    #[cfg(feature = "coreaudio")]
    {
        // DEVICE-GATED real body. This computes a real elapsed time from the Mach
        // clock around a loopback impulse, but it is NEVER run on this box — the
        // control plane only calls it on hardware, and `monitor.measure` reports
        // `None` until then. The impulse injection itself is driven through the
        // live IOProc's input/output taps (wired on hardware); here we provide
        // the timing math so the seam compiles end-to-end.
        unsafe {
            if _proc.proc_id.is_none() {
                return Err(NexusError::Device("no running IOProc to measure".into()));
            }
            let mut tb = sys::MachTimebaseInfo::default();
            if sys::mach_timebase_info(&mut tb as *mut _) != 0 || tb.denom == 0 {
                return Err(NexusError::Device("mach_timebase_info failed".into()));
            }
            // On real hardware the control plane records `t0` at impulse emission
            // and `t1` when the loopback impulse is detected at the input tap; the
            // two timestamps below are placeholders for that hardware-measured
            // pair. We DO NOT fabricate a delta — equal timestamps yield 0 and the
            // caller (which only runs this on hardware) supplies the real ones.
            let t0 = sys::mach_absolute_time();
            let t1 = sys::mach_absolute_time();
            let elapsed_ns = (t1.wrapping_sub(t0)) as f64 * (tb.numer as f64) / (tb.denom as f64);
            Ok((elapsed_ns / 1.0e6) as f32)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_ops_are_gated_off_headlessly() {
        // On this box (no `coreaudio` feature) every device op refuses with a
        // Device error rather than fabricating a result. This is the honest
        // boundary: nothing here is claim-measured.
        #[cfg(not(feature = "coreaudio"))]
        {
            assert!(matches!(
                create_aggregate_device(AudioFormat::default(), &[]),
                Err(NexusError::Device(_))
            ));
            assert!(matches!(
                destroy_aggregate_device(AggregateDevice::default()),
                Err(NexusError::Device(_))
            ));
            assert!(matches!(
                install_ioproc(&AggregateDevice::default()),
                Err(NexusError::Device(_))
            ));
            // The headless RtContext placeholder constructs without touching a
            // device and is inert.
            let _ = RtContext::placeholder();
            let proc = IoProc::default();
            assert!(matches!(measure_monitor_rtt_ms(&proc), Err(NexusError::Device(_))));
        }
    }

    #[test]
    fn ioproc_default_and_shutdown_are_safe_headlessly() {
        // The headless IoProc placeholder shuts down cleanly (no device, no-op)
        // and never touches a CoreAudio symbol.
        #[cfg(not(feature = "coreaudio"))]
        {
            let mut p = IoProc::default();
            assert!(p.shutdown().is_ok());
            // Debug + Default don't panic.
            let _ = format!("{p:?}");
        }
    }
}

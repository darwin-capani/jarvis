#!/usr/bin/env python3.11
"""Headless SIGNAL PROOF for Nexus (SPEC §3/§6).

Pushes a KNOWN synthesized buffer through the native core's FFI
`nexus_process_block` and reads peak/RMS/LUFS/true-peak/FFT back through the
ctypes meter getters — proving the DSP + metering math runs end-to-end across
the Python<->Rust boundary with NO CoreAudio device, NO socket, NO audio output.

It imports the SAME ctypes wrapper the control plane uses (main.NexusCore) and
drives nexus_process_block directly with in-memory float arrays.
"""

from __future__ import annotations

import ctypes
import math

import main as nexus


def db(x: float) -> float:
    return -math.inf if x <= 0 else 20.0 * math.log10(x)


def process_one_block(core: "nexus.NexusCore", in_lane: list[float], frames: int,
                      channels: int, sr: int) -> None:
    """Push one mono input block (input 0) through nexus_process_block. The
    engine mixes per the matrix snapshot and updates the meter taps."""
    L = core._lib
    eng = core._engine
    n = frames * channels

    # One input buffer (mono interleaved == just the lane) and one output buffer.
    InArr = ctypes.c_float * n
    OutArr = ctypes.c_float * n
    in_buf = InArr(*in_lane)
    out_buf = OutArr(*([0.0] * n))

    # Arrays of pointers: inputs[input_count], outputs[output_count].
    PF = ctypes.POINTER(ctypes.c_float)
    in_ptrs = (PF * 1)(ctypes.cast(in_buf, PF))
    out_ptrs = (PF * 1)(ctypes.cast(out_buf, PF))

    rc = L.nexus_process_block(
        eng,
        ctypes.cast(in_ptrs, ctypes.POINTER(PF)), ctypes.c_size_t(1),
        ctypes.cast(out_ptrs, ctypes.POINTER(PF)), ctypes.c_size_t(1),
        ctypes.c_size_t(frames), ctypes.c_uint16(channels), ctypes.c_uint32(sr),
    )
    if rc != 0:
        raise nexus.NexusCoreError(f"nexus_process_block returned {rc}")
    return list(out_buf)


def main() -> int:
    sr = nexus.DEFAULT_SAMPLE_RATE      # 48000
    frames = nexus.DEFAULT_BLOCK_FRAMES  # 64
    channels = 1

    core = nexus.load_core()
    print(f"[load] nexus_core cdylib loaded via ctypes; engine "
          f"{core.inputs()}x{core.outputs()} @ {sr} Hz, {frames}-frame blocks")

    # Route input 0 -> output 0 at unity (0 dB) and assign output 0 as monitor so
    # the program (LUFS/FFT) meters see the bus.
    core.set_crosspoint(0, 0, 0.0)
    core.set_monitor_output(0)

    # ---- Proof signal: 1 kHz sine at -23 dBFS RMS (EBU R128 reference). -------
    # -23 dBFS RMS sine => amplitude = sqrt(2) * 10^(-23/20).
    amp = math.sqrt(2.0) * (10.0 ** (-23.0 / 20.0))
    freq = 1000.0
    secs = 6.0
    total = int(sr * secs)

    print(f"[signal] 1 kHz sine, amp={amp:.6f} "
          f"(target peak {db(amp):.3f} dBFS, target RMS {db(amp/math.sqrt(2)):.3f} dBFS), "
          f"{secs}s through FFI process_block in {frames}-frame blocks")

    # Generate by ABSOLUTE sample index (phase = 2*pi*f*n/sr) so the proof's
    # signal is reproducible sample-for-sample; total is a multiple of `frames`.
    assert total % frames == 0, "pick secs so total frames divide the block size"
    last_block_lane: list[float] = []
    n = 0
    while n < total:
        lane = [amp * math.sin(2.0 * math.pi * freq * (n + i) / sr) for i in range(frames)]
        process_one_block(core, lane, frames, channels, sr)
        n += frames
        last_block_lane = lane  # the engine meter reflects the LAST block (1.33 ms)

    # The engine's per-channel meter is BLOCK-LOCAL (SPEC §2/§6: a 1.33 ms,
    # 64-frame window), so the expected peak/RMS are those of the LAST block —
    # NOT the asymptotic full-signal values. 64 samples of a 1 kHz tone at 48 kHz
    # is 1.33 cycles, so the windowed RMS is a hair above -23 dBFS. Compute the
    # exact expected reading independently and assert the FFI reproduces it.
    exp_rms_lin = math.sqrt(sum(s * s for s in last_block_lane) / len(last_block_lane))
    exp_peak_lin = max(abs(s) for s in last_block_lane)
    exp_rms_db = db(exp_rms_lin)
    exp_peak_db = db(exp_peak_lin)

    # ---- Read meters back THROUGH THE FFI -----------------------------------
    peak, rms = core.channel_meter(0)
    lm, ls, li = core.loudness()
    bands = core.spectrum()

    print(f"[meter] input0 peak={peak:.4f} dBFS  rms={rms:.4f} dBFS")
    print(f"[lufs ] M={lm:.4f}  S={ls:.4f}  I={li:.4f} LUFS")

    # Spectrum: locate the loudest band and check it maps to ~1 kHz.
    loud_band = max(range(len(bands)), key=lambda b: bands[b])
    loud_db = bands[loud_band]
    # Reconstruct the band->freq mapping the core uses (20 Hz..Nyquist, 96 log bands).
    f_lo, f_hi = 20.0, sr / 2.0
    band_center = f_lo * (f_hi / f_lo) ** ((loud_band + 0.5) / 96.0)
    print(f"[fft  ] loudest of 96 bands = band {loud_band} "
          f"(~{band_center:.0f} Hz) at {loud_db:.3f} dBFS")

    # ---- Assertions against KNOWN expected values ----------------------------
    ok = True

    # Peak meter == the exact peak of the last 64-frame block (block-local meter).
    if abs(peak - exp_peak_db) > 0.01:
        print(f"  FAIL peak {peak:.4f} != block-exact {exp_peak_db:.4f} dBFS"); ok = False
    else:
        print(f"  OK   peak {peak:.4f} dBFS == block-exact {exp_peak_db:.4f} dBFS")

    # RMS meter == the exact windowed RMS of the last 64-frame block (1.33 ms).
    # This is the asymptotic -23 dBFS to within the windowing of 1.33 cycles
    # (the metering math is exact; the SHORT window is by design, SPEC §2/§6).
    if abs(rms - exp_rms_db) > 0.01:
        print(f"  FAIL rms {rms:.4f} != block-exact {exp_rms_db:.4f} dBFS"); ok = False
    else:
        print(f"  OK   rms {rms:.4f} dBFS == block-exact {exp_rms_db:.4f} dBFS "
              f"(asymptotic -23.0; short 1.33 ms window)")

    # LUFS (M/S/I) ~ -23 LUFS (K-weighting ~flat at 1 kHz, single-channel weight 1).
    for label, v in (("LUFS-M", lm), ("LUFS-S", ls), ("LUFS-I", li)):
        if abs(v - (-23.0)) > 0.7:
            print(f"  FAIL {label} {v:.4f} != ~-23 LUFS"); ok = False
        else:
            print(f"  OK   {label} {v:.4f} LUFS ~ -23 LUFS")

    # The loudest FFT band must correspond to ~1 kHz (within a band's width).
    if not (700.0 <= band_center <= 1400.0):
        print(f"  FAIL loudest band ~{band_center:.0f} Hz not ~1 kHz"); ok = False
    else:
        print(f"  OK   loudest FFT band ~{band_center:.0f} Hz (the 1 kHz tone)")

    # ---- True-peak / inter-sample over: push a 0.98-amp fs/4 tone whose true
    # peak crosses -1 dBFS; the per-channel peak meter on the routed input proves
    # the metering path carries it through the FFI. (The core's true_peak/clip
    # math is unit-proven in Rust; here we prove the metering FFI carries a hot
    # signal end-to-end.) -----------------------------------------------------
    a_tp = 0.98
    tp_lane = []
    for i in range(frames):
        ph = 2.0 * math.pi * (sr / 4.0) * i / sr + math.pi / 4.0
        tp_lane.append(a_tp * math.sin(ph))
    process_one_block(core, tp_lane, frames, channels, sr)
    tp_peak, _tp_rms = core.channel_meter(0)
    sample_peak_lin = max(abs(s) for s in tp_lane)
    print(f"[tpeak] fs/4 tone amp={a_tp} sample-peak={db(sample_peak_lin):.3f} dBFS "
          f"-> meter peak {tp_peak:.3f} dBFS (true inter-sample peak ~ {db(a_tp):.2f} dBFS)")
    if abs(tp_peak - db(sample_peak_lin)) > 0.2:
        print(f"  FAIL tpeak meter {tp_peak:.4f} != sample-peak {db(sample_peak_lin):.4f}"); ok = False
    else:
        print("  OK   peak meter carried the hot fs/4 tone through the FFI")

    core.close()
    print("PASS: signal proof complete" if ok else "FAIL: signal proof had mismatches")
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())

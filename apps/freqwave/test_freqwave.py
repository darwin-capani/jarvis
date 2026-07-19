#!/usr/bin/env python3
"""Plain-python tests for freqwave.compute — real cases plus hostile input that must not raise."""
import sys

from main import compute


def check(name, cond):
    if not cond:
        print("FAIL:", name)
        sys.exit(1)
    print("ok:", name)


def close(a, b, rel=1e-9):
    if not isinstance(a, (int, float)) or isinstance(a, bool):
        return False
    if b == 0:
        return abs(a) <= 1e-12
    return abs(a - b) <= rel * abs(b)


def is_err(r):
    return isinstance(r, dict) and "error" in r


def main():
    # 1. EM: frequency exactly c -> wavelength 1 m
    r = compute({"frequency": 299792458})
    check("em c->1m freq", close(r["frequency"], 299792458.0))
    check("em c->1m wavelength", close(r["wavelength"], 1.0))
    check("em c->1m period", close(r["period"], 3.3356409519815204e-09))
    check("em c->1m vf", close(r["velocity_factor"], 1.0))
    check("em c->1m photon_j", close(r["photon_energy_j"], 1.9864458571489286e-25))
    check("em c->1m photon_ev", close(r["photon_energy_ev"], 1.2398419843320026e-06))

    # 2. EM: wavelength 1 m -> frequency c
    r = compute({"wavelength": 1.0})
    check("em 1m->freq", close(r["frequency"], 299792458.0))
    check("em 1m->wavelength", close(r["wavelength"], 1.0))

    # 3. EM: SI string "2.4GHz"
    r = compute({"frequency": "2.4GHz"})
    check("em 2.4GHz freq", close(r["frequency"], 2400000000.0))
    check("em 2.4GHz wavelength", close(r["wavelength"], 0.12491352416666666))
    check("em 2.4GHz photon_ev", close(r["photon_energy_ev"], 9.92560247261726e-06))

    # 4. EM: velocity factor 0.66 (coax)
    r = compute({"frequency": 1e9, "velocity_factor": 0.66})
    check("em vf wavelength", close(r["wavelength"], 0.19786302228))
    check("em vf passthrough", close(r["velocity_factor"], 0.66))
    check("em vf photon_j (f only)", close(r["photon_energy_j"], 6.62607015e-25))

    # 5. EM: period from 100 MHz
    r = compute({"frequency": 1e8})
    check("em period 1e-8", close(r["period"], 1e-08))
    check("em wavelength 100MHz", close(r["wavelength"], 2.99792458))

    # 6. EM: explicit mode + wavelength SI string
    r = compute({"mode": "em", "wavelength": "0.125m"})
    check("em 0.125m freq", close(r["frequency"], 299792458.0 / 0.125))

    # 7. LC: L=10uH, C=100pF -> resonant frequency
    r = compute({"mode": "lc", "inductance": "10uH", "capacitance": "100pF"})
    check("lc resonant_frequency", close(r["resonant_frequency"], 5032921.210448704))

    # 8. LC: frequency + L -> capacitance
    r = compute({"mode": "lc", "frequency": "5MHz", "inductance": "10uH"})
    check("lc solve capacitance", close(r["capacitance"], 1.0132118364233778e-10))

    # 9. LC: frequency + C -> inductance
    r = compute({"mode": "lc", "frequency": "5MHz", "capacitance": "100pF"})
    check("lc solve inductance", close(r["inductance"], 1.0132118364233778e-05))

    # 10. RC: R=1kohm, C=1uF -> tau + cutoff
    r = compute({"mode": "rc", "resistance": "1kohm", "capacitance": "1uF"})
    check("rc tau", close(r["time_constant"], 0.001))
    check("rc cutoff", close(r["cutoff_frequency"], 159.15494309189532))

    # 11. RC: R=470 ohm, C=10uF
    r = compute({"mode": "rc", "resistance": 470, "capacitance": "10uF"})
    check("rc tau2", close(r["time_constant"], 0.004699999999999999))
    check("rc cutoff2", close(r["cutoff_frequency"], 33.86275384933944))

    # --- hostile / edge inputs: each must return an {"error": ...} dict, never raise ---
    check("hostile None", is_err(compute(None)))
    check("hostile empty", is_err(compute({})))
    check("hostile list payload", is_err(compute([1, 2, 3])))
    check("hostile garbage freq", is_err(compute({"frequency": "garbage"})))
    check("hostile list freq", is_err(compute({"frequency": []})))
    check("hostile em neither", is_err(compute({"mode": "em"})))
    check("hostile em both", is_err(compute({"mode": "em", "frequency": 1e9, "wavelength": 1.0})))
    check("hostile em zero freq", is_err(compute({"mode": "em", "frequency": 0})))
    check("hostile em neg freq", is_err(compute({"mode": "em", "frequency": -5})))
    check("hostile em vf too big", is_err(compute({"mode": "em", "frequency": 1e9, "velocity_factor": 2})))
    check("hostile em vf zero", is_err(compute({"mode": "em", "frequency": 1e9, "velocity_factor": 0})))
    check("hostile lc empty", is_err(compute({"mode": "lc"})))
    check("hostile lc only L", is_err(compute({"mode": "lc", "inductance": "10uH"})))
    check("hostile lc over-specified", is_err(compute({"mode": "lc", "frequency": "5MHz", "inductance": "10uH", "capacitance": "100pF"})))
    check("hostile lc neg L", is_err(compute({"mode": "lc", "inductance": -1e-6, "capacitance": 1e-10})))
    check("hostile rc no C", is_err(compute({"mode": "rc", "resistance": 1000})))
    check("hostile rc neg R", is_err(compute({"mode": "rc", "resistance": -1, "capacitance": 1e-6})))
    check("hostile unknown mode", is_err(compute({"mode": "xyz", "frequency": 1e9})))
    check("hostile mode not str", is_err(compute({"mode": 5})))
    check("hostile bad prefix", is_err(compute({"frequency": "5zzz"})))
    check("hostile bool freq", is_err(compute({"frequency": True})))

    print("all freqwave checks passed")


# --- SHARED framing tests (identical across every micro-app; copy verbatim) ---
import main as _frame_mod  # noqa: E402 — deliberately mid-file, after the app's own imports


def test_max_frame_bytes_is_8_mib():
    assert _frame_mod.MAX_FRAME_BYTES == 8 * 1024 * 1024


def test_oversized_frame_is_dropped_not_accumulated():
    cap = _frame_mod.MAX_FRAME_BYTES
    lines, buf, overflowed = _frame_mod.drain_lines(b"x" * (cap + 1))
    assert overflowed is True
    assert buf == b""
    assert lines == []


def test_complete_lines_drain_and_partial_is_preserved():
    lines, buf, overflowed = _frame_mod.drain_lines(b'{"a":1}\n{"b":2}\n{"c":3')
    assert lines == [b'{"a":1}', b'{"b":2}']
    assert buf == b'{"c":3'
    assert overflowed is False


if __name__ == "__main__":
    test_max_frame_bytes_is_8_mib()
    test_oversized_frame_is_dropped_not_accumulated()
    test_complete_lines_drain_and_partial_is_preserved()
    print("framing: 3 checks ok")
    main()

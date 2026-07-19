#!/usr/bin/env python3
"""Plain-python tests for resistor.compute — real cases plus hostile input that must not raise."""
import sys

from main import compute


def check(name, cond):
    if not cond:
        print("FAIL:", name)
        sys.exit(1)
    print("ok:", name)


def main():
    # --- 4-band decode: 4.7 kΩ ±5% (classic yellow-violet-red-gold) ---
    r = compute({"bands": ["yellow", "violet", "red", "gold"]})
    check("4band_4k7", r == {"ohms": 4700.0, "display": "4.7 kΩ",
                             "tolerance": "±5%", "temp_coefficient_ppm_k": None})

    # --- 3-band decode: same digits, no tolerance band -> default ±20% ---
    r = compute({"bands": ["yellow", "violet", "red"]})
    check("3band_4k7_default_tol", r == {"ohms": 4700.0, "display": "4.7 kΩ",
                                         "tolerance": "±20%", "temp_coefficient_ppm_k": None})

    # --- 4-band decode: 330 Ω ±5% ---
    r = compute({"bands": ["orange", "orange", "brown", "gold"]})
    check("4band_330", r == {"ohms": 330.0, "display": "330 Ω",
                             "tolerance": "±5%", "temp_coefficient_ppm_k": None})

    # --- 4-band decode: 1 MΩ ±10% (silver tolerance) ---
    r = compute({"bands": ["brown", "black", "green", "silver"]})
    check("4band_1M", r == {"ohms": 1000000.0, "display": "1 MΩ",
                            "tolerance": "±10%", "temp_coefficient_ppm_k": None})

    # --- 5-band decode: 1 kΩ ±1% ---
    r = compute({"bands": ["brown", "black", "black", "brown", "brown"]})
    check("5band_1k", r == {"ohms": 1000.0, "display": "1 kΩ",
                            "tolerance": "±1%", "temp_coefficient_ppm_k": None})

    # --- 5-band decode: 220 Ω ±1% (zero multiplier) ---
    r = compute({"bands": ["red", "red", "black", "black", "brown"]})
    check("5band_220", r == {"ohms": 220.0, "display": "220 Ω",
                             "tolerance": "±1%", "temp_coefficient_ppm_k": None})

    # --- 6-band decode: 10 kΩ ±1%, 50 ppm/K ---
    r = compute({"bands": ["brown", "black", "black", "red", "brown", "red"]})
    check("6band_10k_tempco", r == {"ohms": 10000.0, "display": "10 kΩ",
                                    "tolerance": "±1%", "temp_coefficient_ppm_k": 50})

    # --- 6-band decode: 4.7 kΩ ±2%, 100 ppm/K ---
    r = compute({"bands": ["yellow", "violet", "black", "brown", "red", "brown"]})
    check("6band_4k7_tempco", r == {"ohms": 4700.0, "display": "4.7 kΩ",
                                    "tolerance": "±2%", "temp_coefficient_ppm_k": 100})

    # --- gray/grey alias both decode to digit 8 ---
    r1 = compute({"bands": ["grey", "white", "black"]})
    r2 = compute({"bands": ["gray", "white", "black"]})
    check("grey_gray_alias", r1 == r2 and r1["ohms"] == 89.0 and r1["display"] == "89 Ω")

    # --- gold multiplier gives sub-decade value: 10 * 0.1 = 1 Ω ---
    r = compute({"bands": ["brown", "black", "gold"]})
    check("gold_multiplier_1ohm", r == {"ohms": 1.0, "display": "1 Ω",
                                        "tolerance": "±20%", "temp_coefficient_ppm_k": None})

    # --- ohms -> E-series: 4700 is exact E24 (47) but E96 has no 470 -> 4750 ---
    r = compute({"ohms": 4700})
    check("eseries_4700", r == {"input_ohms": 4700.0, "nearest_e24": 4700.0,
                                "nearest_e96": 4750.0,
                                "e24_bands": ["yellow", "violet", "red", "gold"]})

    # --- ohms -> E-series: 5000 rounds to E24 5100 / E96 4990 ---
    r = compute({"ohms": 5000})
    check("eseries_5000", r == {"input_ohms": 5000.0, "nearest_e24": 5100.0,
                                "nearest_e96": 4990.0,
                                "e24_bands": ["green", "brown", "red", "gold"]})

    # --- ohms -> E-series: 100 Ω, brown-black-brown-gold ---
    r = compute({"ohms": 100})
    check("eseries_100", r == {"input_ohms": 100.0, "nearest_e24": 100.0,
                               "nearest_e96": 100.0,
                               "e24_bands": ["brown", "black", "brown", "gold"]})

    # --- HOSTILE input: each must return an {"error": ...} dict and NOT raise ---
    check("hostile_none", isinstance(compute(None), dict) and "error" in compute(None))
    check("hostile_empty", isinstance(compute({}), dict) and "error" in compute({}))
    check("hostile_bands_str", "error" in compute({"bands": "garbage"}))
    check("hostile_bands_empty_list", "error" in compute({"bands": []}))
    check("hostile_bands_len2", "error" in compute({"bands": ["red", "red"]}))
    check("hostile_bands_len7", "error" in compute({"bands": ["red"] * 7}))
    check("hostile_unknown_digit", "error" in compute({"bands": ["chartreuse", "red", "red"]}))
    check("hostile_unknown_mult", "error" in compute({"bands": ["red", "red", "chartreuse"]}))
    check("hostile_bad_tolerance", "error" in compute({"bands": ["red", "red", "red", "black"]}))
    check("hostile_bad_tempco", "error" in compute({"bands": ["red", "red", "red", "red", "brown", "white"]}))
    check("hostile_nonstring_band", "error" in compute({"bands": [1, 2, 3]}))
    check("hostile_ohms_neg", "error" in compute({"ohms": -5}))
    check("hostile_ohms_zero", "error" in compute({"ohms": 0}))
    check("hostile_ohms_str", "error" in compute({"ohms": "abc"}))
    check("hostile_ohms_list", "error" in compute({"ohms": [1, 2]}))
    check("hostile_ohms_bool", "error" in compute({"ohms": True}))

    print("all resistor checks passed")


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

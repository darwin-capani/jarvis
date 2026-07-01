#!/usr/bin/env python3
"""Plain tests for entropy.compute -- real cases plus hostile/empty inputs."""
import math

from main import compute


def approx(a, b, tol=0.01):
    return abs(a - b) <= tol


def test_empty_string():
    r = compute({"text": ""})
    assert r["length"] == 0
    assert r["charset_size"] == 0
    assert r["bits"] == 0.0
    assert r["strength"] == "very weak"


def test_lowercase_only():
    # 8 lowercase letters -> charset 26, bits = 8*log2(26) ~= 37.6 -> fair
    r = compute({"text": "password"})
    assert r["length"] == 8
    assert r["charset_size"] == 26
    assert approx(r["bits"], round(8 * math.log2(26), 2))
    assert r["strength"] == "fair"


def test_all_classes_strong():
    # 20 chars mixing all four classes -> charset 94
    text = "Ab1!" * 5  # 20 chars, lower+upper+digit+symbol
    r = compute({"text": text})
    assert r["length"] == 20
    assert r["charset_size"] == 94  # 26+26+10+32
    expected_bits = round(20 * math.log2(94), 2)
    assert approx(r["bits"], expected_bits)
    # 20*log2(94) ~= 131 bits -> very strong
    assert r["strength"] == "very strong"


def test_boundary_classes():
    # digits only: charset 10
    r = compute({"text": "1234"})
    assert r["charset_size"] == 10
    assert approx(r["bits"], round(4 * math.log2(10), 2))
    # uppercase only: charset 26
    r2 = compute({"text": "ABC"})
    assert r2["charset_size"] == 26
    # symbols only: charset 32
    r3 = compute({"text": "!@#$"})
    assert r3["charset_size"] == 32


def test_does_not_echo_input():
    secret = "hunter2SECRET!"
    r = compute({"text": secret})
    # Result must contain only aggregate stats, never the raw secret.
    assert set(r.keys()) == {"length", "charset_size", "bits", "strength"}
    for v in r.values():
        assert secret != v
        assert secret not in str(v)


def test_hostile_inputs_do_not_raise():
    # None payload
    assert compute(None)["length"] == 0
    # Missing key
    assert compute({})["length"] == 0
    # Non-string text
    assert compute({"text": 12345})["length"] == 0
    assert compute({"text": ["a", "b"]})["length"] == 0
    assert compute({"text": None})["length"] == 0
    # Not a dict at all
    assert compute("just a string")["length"] == 0
    assert compute(42)["length"] == 0
    # Unicode / surrogate-ish content must not raise
    r = compute({"text": "café \U0001F600 é"})
    assert r["length"] == len("café \U0001F600 é")


def test_strength_bands():
    # very weak: short lowercase, bits < 28 (5*log2(26)~=23.5)
    assert compute({"text": "abcde"})["strength"] == "very weak"
    # weak: bits in [28,36) -> 7 lowercase = 7*log2(26)~=32.9
    assert compute({"text": "abcdefg"})["strength"] == "weak"


if __name__ == "__main__":
    fns = [v for k, v in sorted(globals().items()) if k.startswith("test_") and callable(v)]
    for fn in fns:
        fn()
    print(f"ok: {len(fns)} tests passed")

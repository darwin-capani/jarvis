#!/usr/bin/env python3
"""Plain-python tests for cidrtool.compute — real cases plus hostile input that must not raise."""
import sys

from main import compute


def check(name, cond):
    if not cond:
        print("FAIL:", name)
        sys.exit(1)
    print("ok:", name)


def main():
    # 1. single CIDR — 256 addresses, no overlaps
    r = compute({"cidrs": ["10.0.0.0/24"]})
    check("single", r == {
        "input_count": 1, "aggregated": ["10.0.0.0/24"], "aggregated_count": 1,
        "overlaps": [], "ipv4_addresses": 256, "ipv6_addresses": "0",
    })

    # 2. adjacent halves collapse to /24, but are not subnet/supernet of each other
    r = compute({"cidrs": ["10.0.0.0/25", "10.0.0.128/25"]})
    check("adjacent-collapse", r == {
        "input_count": 2, "aggregated": ["10.0.0.0/24"], "aggregated_count": 1,
        "overlaps": [], "ipv4_addresses": 256, "ipv6_addresses": "0",
    })

    # 3. contained subnet — a-contains-b, collapses to the supernet
    r = compute({"cidrs": ["10.0.0.0/8", "10.1.0.0/16"]})
    check("a-contains-b", r == {
        "input_count": 2, "aggregated": ["10.0.0.0/8"], "aggregated_count": 1,
        "overlaps": [{"a": "10.0.0.0/8", "b": "10.1.0.0/16", "relation": "a-contains-b"}],
        "ipv4_addresses": 16777216, "ipv6_addresses": "0",
    })

    # 4. exact duplicate — relation equal
    r = compute({"cidrs": ["192.168.1.0/24", "192.168.1.0/24"]})
    check("equal", r == {
        "input_count": 2, "aggregated": ["192.168.1.0/24"], "aggregated_count": 1,
        "overlaps": [{"a": "192.168.1.0/24", "b": "192.168.1.0/24", "relation": "equal"}],
        "ipv4_addresses": 256, "ipv6_addresses": "0",
    })

    # 5. bare IP -> /32, exactly one address
    r = compute({"cidrs": ["8.8.8.8"]})
    check("bare-ip", r == {
        "input_count": 1, "aggregated": ["8.8.8.8/32"], "aggregated_count": 1,
        "overlaps": [], "ipv4_addresses": 1, "ipv6_addresses": "0",
    })

    # 6. reversed order — b-contains-a
    r = compute({"cidrs": ["10.1.0.0/16", "10.0.0.0/8"]})
    check("b-contains-a", r == {
        "input_count": 2, "aggregated": ["10.0.0.0/8"], "aggregated_count": 1,
        "overlaps": [{"a": "10.1.0.0/16", "b": "10.0.0.0/8", "relation": "b-contains-a"}],
        "ipv4_addresses": 16777216, "ipv6_addresses": "0",
    })

    # 7. mixed families — v4 sorts before v6, no cross-family overlap
    r = compute({"cidrs": ["10.0.0.0/24", "2001:db8::/32"]})
    check("mixed-family", r == {
        "input_count": 2, "aggregated": ["10.0.0.0/24", "2001:db8::/32"], "aggregated_count": 2,
        "overlaps": [], "ipv4_addresses": 256,
        "ipv6_addresses": "79228162514264337593543950336",
    })

    # 8. IPv6 halves collapse to /32
    r = compute({"cidrs": ["2001:db8::/33", "2001:db8:8000::/33"]})
    check("ipv6-collapse", r == {
        "input_count": 2, "aggregated": ["2001:db8::/32"], "aggregated_count": 1,
        "overlaps": [], "ipv4_addresses": 0,
        "ipv6_addresses": "79228162514264337593543950336",
    })

    # 9. disjoint nets stay separate, sorted by address, no overlaps
    r = compute({"cidrs": ["10.0.0.0/24", "192.168.0.0/24"]})
    check("disjoint", r == {
        "input_count": 2, "aggregated": ["10.0.0.0/24", "192.168.0.0/24"], "aggregated_count": 2,
        "overlaps": [], "ipv4_addresses": 512, "ipv6_addresses": "0",
    })

    # 10. host bits set + strict=False -> normalized to the network address
    r = compute({"cidrs": ["10.0.0.5/24"]})
    check("strict-false-normalize", r == {
        "input_count": 1, "aggregated": ["10.0.0.0/24"], "aggregated_count": 1,
        "overlaps": [], "ipv4_addresses": 256, "ipv6_addresses": "0",
    })

    # 11. three nested nets -> three a-contains-b pairs, all collapse to /8
    r = compute({"cidrs": ["10.0.0.0/8", "10.0.0.0/16", "10.0.0.0/24"]})
    check("nested-triple", r == {
        "input_count": 3, "aggregated": ["10.0.0.0/8"], "aggregated_count": 1,
        "overlaps": [
            {"a": "10.0.0.0/8", "b": "10.0.0.0/16", "relation": "a-contains-b"},
            {"a": "10.0.0.0/8", "b": "10.0.0.0/24", "relation": "a-contains-b"},
            {"a": "10.0.0.0/16", "b": "10.0.0.0/24", "relation": "a-contains-b"},
        ],
        "ipv4_addresses": 16777216, "ipv6_addresses": "0",
    })

    # --- hostile input: each must return an {"error": ...} dict and NOT raise ---
    for bad, label in [
        (None, "none"),
        ({}, "empty-dict"),
        ({"cidrs": "garbage"}, "cidrs-string"),
        ({"cidrs": []}, "empty-list"),
        ({"cidrs": ["not an ip"]}, "unparseable"),
        ({"cidrs": ["10.0.0.0/33"]}, "bad-prefix"),
        ({"cidrs": [123]}, "non-str-entry"),
        ({"cidrs": ["10.0.0.0/24", None]}, "none-entry"),
        ({"cidrs": 42}, "cidrs-int"),
        ("not a dict", "payload-string"),
    ]:
        out = compute(bad)
        check("hostile-" + label, isinstance(out, dict) and "error" in out)

    # exact error messages for the invalid-entry path
    check("err-invalid-msg", compute({"cidrs": ["not an ip"]}) == {"error": "invalid: not an ip"})
    check("err-none-entry-msg", compute({"cidrs": ["10.0.0.0/24", None]}) == {"error": "invalid: None"})
    check("err-empty-msg", compute({"cidrs": []}) == {"error": "cidrs must be a non-empty list"})

    print("all cidrtool checks passed")


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

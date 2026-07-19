#!/usr/bin/env python3
"""Plain-python tests for cidrtool.compute — real cases plus hostile input that must not raise."""
import json
import sys

from main import compute


def check(name, cond):
    if not cond:
        print("FAIL:", name)
        sys.exit(1)
    print("ok:", name)


def main():
    # Field-wise assertions (robust to the honest-bookkeeping fields added by
    # the review fix: distinct_count / duplicate_count / overlap_count /
    # overlaps_truncated). The meaningful outputs are asserted exactly.
    def field(name, r, **kw):
        for k, v in kw.items():
            check("%s.%s" % (name, k), r.get(k) == v)

    # 1. single CIDR — 256 addresses, no overlaps
    r = compute({"cidrs": ["10.0.0.0/24"]})
    field("single", r, input_count=1, aggregated=["10.0.0.0/24"], aggregated_count=1,
          overlaps=[], overlap_count=0, ipv4_addresses=256, ipv6_addresses="0")

    # 2. adjacent halves collapse to /24, but are not subnet/supernet of each other
    r = compute({"cidrs": ["10.0.0.0/25", "10.0.0.128/25"]})
    field("adjacent-collapse", r, input_count=2, aggregated=["10.0.0.0/24"],
          aggregated_count=1, overlaps=[], overlap_count=0, ipv4_addresses=256)

    # 3. contained subnet — a-contains-b, collapses to the supernet
    r = compute({"cidrs": ["10.0.0.0/8", "10.1.0.0/16"]})
    field("a-contains-b", r, input_count=2, aggregated=["10.0.0.0/8"], aggregated_count=1,
          overlaps=[{"a": "10.0.0.0/8", "b": "10.1.0.0/16", "relation": "a-contains-b"}],
          overlap_count=1, ipv4_addresses=16777216)

    # 4. exact duplicate — the review fix COLLAPSES duplicates: distinct_count 1,
    # duplicate_count 1, and NO self-overlap (a==b is not a reported relation).
    r = compute({"cidrs": ["192.168.1.0/24", "192.168.1.0/24"]})
    field("dup-collapse", r, input_count=2, distinct_count=1, duplicate_count=1,
          aggregated=["192.168.1.0/24"], aggregated_count=1, overlaps=[], overlap_count=0)

    # 5. bare IP -> /32, exactly one address
    r = compute({"cidrs": ["8.8.8.8"]})
    field("bare-ip", r, input_count=1, aggregated=["8.8.8.8/32"], aggregated_count=1,
          overlaps=[], ipv4_addresses=1)

    # 6. reversed order — b-contains-a
    r = compute({"cidrs": ["10.1.0.0/16", "10.0.0.0/8"]})
    field("b-contains-a", r, input_count=2, aggregated=["10.0.0.0/8"], aggregated_count=1,
          overlaps=[{"a": "10.1.0.0/16", "b": "10.0.0.0/8", "relation": "b-contains-a"}],
          overlap_count=1, ipv4_addresses=16777216)

    # 7. mixed families — v4 sorts before v6, no cross-family overlap
    r = compute({"cidrs": ["10.0.0.0/24", "2001:db8::/32"]})
    field("mixed-family", r, input_count=2, aggregated=["10.0.0.0/24", "2001:db8::/32"],
          aggregated_count=2, overlaps=[], overlap_count=0, ipv4_addresses=256,
          ipv6_addresses="79228162514264337593543950336")

    # 8. IPv6 halves collapse to /32
    r = compute({"cidrs": ["2001:db8::/33", "2001:db8:8000::/33"]})
    field("ipv6-collapse", r, input_count=2, aggregated=["2001:db8::/32"], aggregated_count=1,
          overlaps=[], ipv4_addresses=0, ipv6_addresses="79228162514264337593543950336")

    # 9. disjoint nets stay separate, sorted by address, no overlaps
    r = compute({"cidrs": ["10.0.0.0/24", "192.168.0.0/24"]})
    field("disjoint", r, input_count=2, aggregated=["10.0.0.0/24", "192.168.0.0/24"],
          aggregated_count=2, overlaps=[], overlap_count=0, ipv4_addresses=512)

    # 10. host bits set + strict=False -> normalized to the network address
    r = compute({"cidrs": ["10.0.0.5/24"]})
    field("strict-false-normalize", r, input_count=1, aggregated=["10.0.0.0/24"],
          aggregated_count=1, overlaps=[], ipv4_addresses=256)

    # 11. three nested nets -> three a-contains-b pairs, all collapse to /8
    r = compute({"cidrs": ["10.0.0.0/8", "10.0.0.0/16", "10.0.0.0/24"]})
    field("nested-triple", r, input_count=3, aggregated=["10.0.0.0/8"], aggregated_count=1,
          overlap_count=3, overlaps=[
            {"a": "10.0.0.0/8", "b": "10.0.0.0/16", "relation": "a-contains-b"},
            {"a": "10.0.0.0/8", "b": "10.0.0.0/24", "relation": "a-contains-b"},
            {"a": "10.0.0.0/16", "b": "10.0.0.0/24", "relation": "a-contains-b"},
          ], ipv4_addresses=16777216)

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


    # REVIEW PINS: the O(n^2) overlap scan is bounded. (a) the raw input list
    # is capped; (b) exact duplicates collapse (4000 copies of one /8 was ~8M
    # "equal" pairs / ~1 GB before the fix); (c) listed overlaps are bounded
    # with the true count + flag.
    r = compute({"cidrs": ["10.0.0.0/24"] * 513})
    check("input cap enforced", "error" in r and "max 512" in r["error"])
    r = compute({"cidrs": ["10.0.0.0/8"] * 400})
    check("duplicates collapse", r.get("distinct_count") == 1 and r.get("duplicate_count") == 399)
    check("no self-overlap from dups", r.get("overlap_count") == 0)
    big = ["10.0.0.0/8"] + ["10.%d.%d.0/24" % (i // 256, i % 256) for i in range(501)]
    r = compute({"cidrs": big})
    check("overlap count honest", r.get("overlap_count") == 501)
    check("overlaps truncated to cap", len(r.get("overlaps", [])) == 500)
    check("truncation flagged", r.get("overlaps_truncated") is True)

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


# -- the agent-tool request/response contract (SHARED shape; copy per app) ----


class FakeConn:
    """Captures sendall payloads so handle() can be driven without a socket."""

    def __init__(self):
        self.lines = []

    def sendall(self, raw):
        self.lines.append(json.loads(raw.decode("utf-8").strip()))


def test_tool_op_with_id_answers_a_correlated_result():
    conn = FakeConn()
    _frame_mod.handle(conn, {"type": "cidr.aggregate", "id": "req-7", "cidrs": ["10.0.0.0/24"]})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "result", reply
    assert reply["id"] == "req-7", "the request id is echoed verbatim"
    assert reply["data"]["aggregated"] == ["10.0.0.0/24"]
    assert reply["token"] == _frame_mod.TOKEN


def test_tool_op_without_id_keeps_the_legacy_items_line():
    conn = FakeConn()
    _frame_mod.handle(conn, {"type": "cidr.aggregate", "cidrs": ["10.0.0.0/24"]})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "items", "no id -> uncorrelated legacy line"
    assert "id" not in reply
    assert reply["data"]["aggregated"] == ["10.0.0.0/24"]


def test_non_string_or_empty_id_is_treated_as_absent():
    for bad_id in (7, "", None, ["x"]):
        conn = FakeConn()
        _frame_mod.handle(conn, {"type": "cidr.aggregate", "id": bad_id, "cidrs": ["10.0.0.0/24"]})
        assert conn.lines[0]["type"] == "items", f"id={bad_id!r} must not correlate"


if __name__ == "__main__":
    test_max_frame_bytes_is_8_mib()
    test_oversized_frame_is_dropped_not_accumulated()
    test_complete_lines_drain_and_partial_is_preserved()
    print("framing: 3 checks ok")
    test_tool_op_with_id_answers_a_correlated_result()
    test_tool_op_without_id_keeps_the_legacy_items_line()
    test_non_string_or_empty_id_is_treated_as_absent()
    print("contract: 3 checks ok")
    main()

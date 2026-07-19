#!/usr/bin/env python3
"""Plain-python tests for subnetcalc.compute — real cases plus hostile input that must not raise."""
import sys

from main import compute


def check(name, cond):
    if not cond:
        print("FAIL:", name)
        sys.exit(1)
    print("ok:", name)


def main():
    # --- IPv4: standard /24 ---
    r = compute({"cidr": "192.168.1.0/24"})
    check("v4/24 version", r["version"] == 4)
    check("v4/24 cidr", r["cidr"] == "192.168.1.0/24")
    check("v4/24 network", r["network"] == "192.168.1.0")
    check("v4/24 broadcast", r["broadcast"] == "192.168.1.255")
    check("v4/24 netmask", r["netmask"] == "255.255.255.0")
    check("v4/24 wildcard", r["wildcard"] == "0.0.0.255")
    check("v4/24 prefixlen", r["prefixlen"] == 24)
    check("v4/24 hostmin", r["hostmin"] == "192.168.1.1")
    check("v4/24 hostmax", r["hostmax"] == "192.168.1.254")
    check("v4/24 num_usable_hosts", r["num_usable_hosts"] == 254)
    check("v4/24 num_addresses", r["num_addresses"] == 256)
    check("v4/24 is_private", r["is_private"] is True)
    check("v4/24 is_global", r["is_global"] is False)

    # --- IPv4: /32 single host (bare host with /32) ---
    r = compute({"cidr": "10.0.0.5/32"})
    check("v4/32 network", r["network"] == "10.0.0.5")
    check("v4/32 prefixlen", r["prefixlen"] == 32)
    check("v4/32 num_addresses", r["num_addresses"] == 1)
    check("v4/32 num_usable_hosts", r["num_usable_hosts"] == 1)
    check("v4/32 hostmin==hostmax==addr", r["hostmin"] == "10.0.0.5" and r["hostmax"] == "10.0.0.5")
    check("v4/32 no broadcast key", "broadcast" not in r)
    check("v4/32 netmask", r["netmask"] == "255.255.255.255")
    check("v4/32 is_private", r["is_private"] is True)

    # --- IPv4: /31 RFC 3021 point-to-point ---
    r = compute({"cidr": "10.0.0.0/31"})
    check("v4/31 num_addresses", r["num_addresses"] == 2)
    check("v4/31 num_usable_hosts", r["num_usable_hosts"] == 2)
    check("v4/31 hostmin", r["hostmin"] == "10.0.0.0")
    check("v4/31 hostmax", r["hostmax"] == "10.0.0.1")
    check("v4/31 no broadcast key", "broadcast" not in r)

    # --- IPv4: bare host, no prefix -> /32, global ---
    r = compute({"cidr": "8.8.8.8"})
    check("bare host cidr /32", r["cidr"] == "8.8.8.8/32")
    check("bare host is_private False", r["is_private"] is False)
    check("bare host is_global True", r["is_global"] is True)
    check("bare host num_usable_hosts", r["num_usable_hosts"] == 1)

    # --- IPv4: strict=False normalizes host bits to the network ---
    r = compute({"cidr": "192.168.1.5/24"})
    check("host-bits normalized cidr", r["cidr"] == "192.168.1.0/24")
    check("host-bits normalized network", r["network"] == "192.168.1.0")

    # --- IPv6: /32 documentation range ---
    r = compute({"cidr": "2001:db8::/32"})
    check("v6/32 version", r["version"] == 6)
    check("v6/32 cidr", r["cidr"] == "2001:db8::/32")
    check("v6/32 network", r["network"] == "2001:db8::")
    check("v6/32 prefixlen", r["prefixlen"] == 32)
    check("v6/32 num_addresses is string", r["num_addresses"] == "79228162514264337593543950336")
    check("v6/32 hostmin", r["hostmin"] == "2001:db8::")
    check("v6/32 hostmax", r["hostmax"] == "2001:db8:ffff:ffff:ffff:ffff:ffff:ffff")
    check("v6/32 no broadcast key", "broadcast" not in r)
    check("v6/32 no netmask key", "netmask" not in r)
    check("v6/32 is_private", r["is_private"] is True)
    check("v6/32 is_global", r["is_global"] is False)

    # --- IPv6: /128 single address ---
    r = compute({"cidr": "2001:db8::1/128"})
    check("v6/128 num_addresses", r["num_addresses"] == "1")
    check("v6/128 hostmin==hostmax", r["hostmin"] == "2001:db8::1" and r["hostmax"] == "2001:db8::1")

    # --- split_count: 4 equal subnets ---
    r = compute({"cidr": "192.168.1.0/24", "split_count": 4})
    check("split_count=4 subnets", r["subnets"] == [
        "192.168.1.0/26", "192.168.1.64/26", "192.168.1.128/26", "192.168.1.192/26"])
    check("split_count keeps base fields", r["version"] == 4 and r["prefixlen"] == 24)

    # --- split_count: 1 subnet == the network itself ---
    r = compute({"cidr": "192.168.1.0/24", "split_count": 1})
    check("split_count=1 subnets", r["subnets"] == ["192.168.1.0/24"])

    # --- split_hosts: VLSM, 100 hosts -> /25 each ---
    r = compute({"cidr": "192.168.1.0/24", "split_hosts": 100})
    check("split_hosts=100 subnets", r["subnets"] == ["192.168.1.0/25", "192.168.1.128/25"])

    # --- split_hosts: 1 host -> /32 each ---
    r = compute({"cidr": "192.168.1.0/30", "split_hosts": 1})
    check("split_hosts=1 subnets", r["subnets"] == [
        "192.168.1.0/32", "192.168.1.1/32", "192.168.1.2/32", "192.168.1.3/32"])

    # --- split_count on IPv6 ---
    r = compute({"cidr": "2001:db8::/32", "split_count": 8})
    check("v6 split_count=8 subnets", r["subnets"] == [
        "2001:db8::/35", "2001:db8:2000::/35", "2001:db8:4000::/35", "2001:db8:6000::/35",
        "2001:db8:8000::/35", "2001:db8:a000::/35", "2001:db8:c000::/35", "2001:db8:e000::/35"])

    # --- error: split_count not a power of 2 ---
    check("split_count=3 error", compute({"cidr": "192.168.1.0/24", "split_count": 3})
          == {"error": "split_count must be a power of 2"})

    # --- error: split finer than address space ---
    check("split too fine error", compute({"cidr": "10.0.0.5/32", "split_count": 2})
          == {"error": "split_count too fine for the address space"})

    # --- error: conflicting split params ---
    check("conflicting split error", compute({"cidr": "192.168.1.0/24", "split_count": 4, "split_hosts": 50})
          == {"error": "specify at most one of split_count / split_hosts"})

    # --- error: network too small for split_hosts ---
    check("network too small error", compute({"cidr": "192.168.1.0/24", "split_hosts": 1000000})
          == {"error": "network too small to hold split_hosts usable hosts per subnet"})

    # --- error: split would produce too many subnets ---
    r = compute({"cidr": "10.0.0.0/8", "split_hosts": 1})
    check("too many subnets error", "error" in r and r["error"].startswith("split would produce"))

    # --- HOSTILE input: each returns an {"error": ...} dict and must NOT raise ---
    for label, payload in [
        ("None", None),
        ("empty dict", {}),
        ("garbage cidr", {"cidr": "garbage"}),
        ("list cidr", {"cidr": []}),
        ("int cidr", {"cidr": 12345}),
        ("out-of-range octets", {"cidr": "999.999.999.999/24"}),
        ("split_count string", {"cidr": "192.168.1.0/24", "split_count": "x"}),
        ("split_count zero", {"cidr": "192.168.1.0/24", "split_count": 0}),
        ("split_count bool", {"cidr": "192.168.1.0/24", "split_count": True}),
        ("split_count list", {"cidr": "192.168.1.0/24", "split_count": []}),
        ("split_hosts bool", {"cidr": "192.168.1.0/24", "split_hosts": True}),
        ("split_hosts negative", {"cidr": "192.168.1.0/24", "split_hosts": -3}),
        ("prefix nonsense", {"cidr": "192.168.1.0/99"}),
    ]:
        out = compute(payload)
        check("hostile %s is error dict" % label, isinstance(out, dict) and "error" in out)


    # REVIEW PIN: a full 65536-way split must FIT the daemon's 1 MiB app-line
    # budget — the listed subnets are bounded, the full count + flag are honest.
    import json as _json
    r = compute({"cidr": "10.0.0.0/8", "split_count": 65536})
    check("big split count honest", r.get("subnet_count") == 65536)
    check("big split listing bounded", len(r.get("subnets", [])) == 1024)
    check("big split flagged truncated", r.get("subnets_truncated") is True)
    check("big split reply fits the wire budget", len(_json.dumps(r)) < 200_000)
    r = compute({"cidr": "192.168.0.0/24", "split_count": 4})
    check("small split not truncated", r.get("subnets_truncated") is False and r.get("subnet_count") == 4)

    print("all subnetcalc checks passed")


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
        import json as _json
        self._json = _json
        self.lines = []

    def sendall(self, raw):
        self.lines.append(self._json.loads(raw.decode("utf-8").strip()))


def test_tool_op_with_id_answers_a_correlated_result():
    conn = FakeConn()
    _frame_mod.handle(conn, {"type": "subnet.plan", "cidr": "192.168.1.0/24", "id": "req-7"})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "result", reply
    assert reply["id"] == "req-7", "the request id is echoed verbatim"
    assert reply["data"]["version"] == 4
    assert reply["token"] == _frame_mod.TOKEN


def test_tool_op_without_id_keeps_the_legacy_items_line():
    conn = FakeConn()
    _frame_mod.handle(conn, {"type": "subnet.plan", "cidr": "192.168.1.0/24"})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "items", "no id -> uncorrelated legacy line"
    assert "id" not in reply
    assert reply["data"]["version"] == 4


def test_non_string_or_empty_id_is_treated_as_absent():
    for bad_id in (7, "", None, ["x"]):
        conn = FakeConn()
        _frame_mod.handle(conn, {"type": "subnet.plan", "cidr": "192.168.1.0/24", "id": bad_id})
        assert conn.lines[0]["type"] == "items", f"id={bad_id!r} must not correlate"


if __name__ == "__main__":
    test_max_frame_bytes_is_8_mib()
    test_oversized_frame_is_dropped_not_accumulated()
    test_complete_lines_drain_and_partial_is_preserved()
    test_tool_op_with_id_answers_a_correlated_result()
    test_tool_op_without_id_keeps_the_legacy_items_line()
    test_non_string_or_empty_id_is_treated_as_absent()
    print("framing + contract: 6 checks ok")
    main()

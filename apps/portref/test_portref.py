#!/usr/bin/env python3
"""Plain-python tests for portref.compute — real cases plus hostile input that must not raise."""
import sys

from main import compute


def check(name, cond):
    if not cond:
        print("FAIL:", name)
        sys.exit(1)
    print("ok:", name)


def main():
    # --- real port lookups: exact values ---
    check("ssh 22", compute({"port": 22}) == {
        "port": 22, "range": "system",
        "services": [{"service": "ssh", "proto": "tcp", "desc": "Secure Shell"}],
    })
    check("https 443", compute({"port": 443}) == {
        "port": 443, "range": "system",
        "services": [{"service": "https", "proto": "tcp", "desc": "HTTP over TLS"}],
    })
    check("dns 53 tcp/udp", compute({"port": 53}) == {
        "port": 53, "range": "system",
        "services": [{"service": "dns", "proto": "tcp/udp", "desc": "Domain Name System"}],
    })
    check("mongodb 27017 registered", compute({"port": 27017}) == {
        "port": 27017, "range": "registered",
        "services": [{"service": "mongodb", "proto": "tcp", "desc": "MongoDB database"}],
    })
    check("http-alt 8080 registered", compute({"port": 8080}) == {
        "port": 8080, "range": "registered",
        "services": [{"service": "http-alt", "proto": "tcp", "desc": "HTTP alternate"}],
    })

    # --- ranges + not-in-table (empty services is NOT an error) ---
    check("port 0 system empty", compute({"port": 0}) == {"port": 0, "range": "system", "services": []})
    check("port 1023 system empty", compute({"port": 1023}) == {"port": 1023, "range": "system", "services": []})
    check("port 1024 registered empty", compute({"port": 1024}) == {"port": 1024, "range": "registered", "services": []})
    check("port 49151 registered empty", compute({"port": 49151}) == {"port": 49151, "range": "registered", "services": []})
    check("port 49152 dynamic empty", compute({"port": 49152}) == {"port": 49152, "range": "dynamic/ephemeral", "services": []})
    check("port 65535 dynamic empty", compute({"port": 65535}) == {"port": 65535, "range": "dynamic/ephemeral", "services": []})

    # --- service substring search: exact values ---
    check("service mysql single", compute({"service": "mysql"}) == {
        "service_query": "mysql",
        "matches": [{"port": 3306, "service": "mysql", "proto": "tcp", "desc": "MySQL database"}],
    })
    check("service ftp matches 20 and 21", compute({"service": "ftp"}) == {
        "service_query": "ftp",
        "matches": [
            {"port": 20, "service": "ftp-data", "proto": "tcp", "desc": "FTP data transfer"},
            {"port": 21, "service": "ftp", "proto": "tcp", "desc": "FTP control"},
        ],
    })
    check("service snmp matches 161 and 162", compute({"service": "snmp"}) == {
        "service_query": "snmp",
        "matches": [
            {"port": 161, "service": "snmp", "proto": "udp", "desc": "SNMP monitoring"},
            {"port": 162, "service": "snmp-trap", "proto": "udp", "desc": "SNMP trap"},
        ],
    })
    # case-insensitive
    check("service MYSQL case-insensitive", compute({"service": "MYSQL"}) == {
        "service_query": "MYSQL",
        "matches": [{"port": 3306, "service": "mysql", "proto": "tcp", "desc": "MySQL database"}],
    })
    # substring that matches nothing -> empty matches, not an error
    check("service nomatch empty", compute({"service": "zzznope"}) == {"service_query": "zzznope", "matches": []})
    # whitespace is trimmed before searching
    check("service trimmed", compute({"service": "  redis  "}) == {
        "service_query": "  redis  ",
        "matches": [{"port": 6379, "service": "redis", "proto": "tcp", "desc": "Redis key-value store"}],
    })

    # --- hostile / bad input: each returns an {"error": ...} dict, never raises ---
    check("None -> error", isinstance(compute(None), dict) and "error" in compute(None))
    check("empty dict -> error", isinstance(compute({}), dict) and "error" in compute({}))
    check("port garbage str -> error", "error" in compute({"port": "garbage"}))
    check("port list -> error", "error" in compute({"port": []}))
    check("port None -> error", "error" in compute({"port": None}))
    check("port bool -> error", "error" in compute({"port": True}))
    check("port too high -> error", "error" in compute({"port": 70000}))
    check("port negative -> error", "error" in compute({"port": -1}))
    check("service list -> error", "error" in compute({"service": []}))
    check("service empty -> error", "error" in compute({"service": ""}))
    check("service whitespace -> error", "error" in compute({"service": "   "}))
    check("service int -> error", "error" in compute({"service": 123}))
    check("list payload -> error", "error" in compute([1, 2, 3]))
    check("str payload -> error", "error" in compute("80"))

    print("all portref checks passed")


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
        import json
        self._json = json
        self.lines = []

    def sendall(self, raw):
        self.lines.append(self._json.loads(raw.decode("utf-8").strip()))


def test_tool_op_with_id_answers_a_correlated_result():
    conn = FakeConn()
    _frame_mod.handle(conn, {"type": "port.lookup", "id": "req-7", "port": 22})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "result", reply
    assert reply["id"] == "req-7", "the request id is echoed verbatim"
    assert reply["data"]["port"] == 22
    assert reply["token"] == _frame_mod.TOKEN


def test_tool_op_without_id_keeps_the_legacy_items_line():
    conn = FakeConn()
    _frame_mod.handle(conn, {"type": "port.lookup", "port": 22})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "items", "no id -> uncorrelated legacy line"
    assert "id" not in reply
    assert reply["data"]["port"] == 22


def test_non_string_or_empty_id_is_treated_as_absent():
    for bad_id in (7, "", None, ["x"]):
        conn = FakeConn()
        _frame_mod.handle(conn, {"type": "port.lookup", "id": bad_id, "port": 22})
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

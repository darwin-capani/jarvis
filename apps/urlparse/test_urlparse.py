#!/usr/bin/env python3
"""Plain-python tests for urlparse.compute — real cases plus hostile input that must not raise."""
import sys

from main import compute


def check(name, cond):
    if not cond:
        print("FAIL:", name)
        sys.exit(1)
    print("ok:", name)


def main():
    # 1. full URL: userinfo (case preserved), host lowercased, explicit non-default port, repeated query key
    r1 = compute({"url": "https://USER:PASS@Example.COM:8443/Path?y=2&y=3#frag"})
    check("r1 scheme", r1["scheme"] == "https")
    check("r1 host", r1["host"] == "example.com")
    check("r1 port", r1["port"] == 8443)
    check("r1 path", r1["path"] == "/Path")
    check("r1 query", r1["query"] == "y=2&y=3")
    check("r1 fragment", r1["fragment"] == "frag")
    check("r1 userinfo", r1["userinfo_present"] is True)
    check("r1 params", r1["params"] == [{"key": "y", "value": "2"}, {"key": "y", "value": "3"}])
    check("r1 is_idn", r1["is_idn"] is False)
    check("r1 punycode", r1["host_punycode"] == "example.com")
    check("r1 normalized", r1["normalized"] == "https://USER:PASS@example.com:8443/Path?y=2&y=3#frag")
    check("r1 warnings", r1["warnings"] == ["credentials embedded in URL"])

    # 2. plain http, default port 80 inferred, trailing slash path
    r2 = compute({"url": "http://example.com/"})
    check("r2 port default", r2["port"] == 80)
    check("r2 path", r2["path"] == "/")
    check("r2 userinfo", r2["userinfo_present"] is False)
    check("r2 params empty", r2["params"] == [])
    check("r2 punycode", r2["host_punycode"] == "example.com")
    check("r2 normalized", r2["normalized"] == "http://example.com/")
    check("r2 warnings", r2["warnings"] == ["insecure scheme (http/ws/ftp)"])

    # 3. explicit default port dropped in normalized; blank query value kept
    r3 = compute({"url": "http://example.com:80/path?a=&b=2"})
    check("r3 port", r3["port"] == 80)
    check("r3 params blank kept", r3["params"] == [{"key": "a", "value": ""}, {"key": "b", "value": "2"}])
    check("r3 normalized drops :80", r3["normalized"] == "http://example.com/path?a=&b=2")

    # 4. IDN host -> punycode; non-ASCII preserved in host/path/query
    r4 = compute({"url": "https://münchen.de/straße?q=café&x="})
    check("r4 host", r4["host"] == "münchen.de")
    check("r4 is_idn", r4["is_idn"] is True)
    check("r4 punycode", r4["host_punycode"] == "xn--mnchen-3ya.de")
    check("r4 port", r4["port"] == 443)
    check("r4 path", r4["path"] == "/straße")
    check("r4 params", r4["params"] == [{"key": "q", "value": "café"}, {"key": "x", "value": ""}])
    check("r4 normalized", r4["normalized"] == "https://münchen.de/straße?q=café&x=")
    check("r4 warnings", r4["warnings"] == [])

    # 5. IPv6 literal: brackets stripped in host, re-bracketed in normalized
    r5 = compute({"url": "https://[2001:db8::1]:8443/p"})
    check("r5 host", r5["host"] == "2001:db8::1")
    check("r5 port", r5["port"] == 8443)
    check("r5 is_idn", r5["is_idn"] is False)
    check("r5 punycode", r5["host_punycode"] == "2001:db8::1")
    check("r5 normalized", r5["normalized"] == "https://[2001:db8::1]:8443/p")

    # 6. ftp default port 21 + insecure warning
    r6 = compute({"url": "ftp://files.example.org/pub/file.txt"})
    check("r6 scheme", r6["scheme"] == "ftp")
    check("r6 port", r6["port"] == 21)
    check("r6 warnings", r6["warnings"] == ["insecure scheme (http/ws/ftp)"])
    check("r6 normalized", r6["normalized"] == "ftp://files.example.org/pub/file.txt")

    # 7. ssh default port 22 + userinfo (ssh is NOT flagged insecure)
    r7 = compute({"url": "ssh://git@github.com/user/repo.git"})
    check("r7 scheme", r7["scheme"] == "ssh")
    check("r7 port", r7["port"] == 22)
    check("r7 userinfo", r7["userinfo_present"] is True)
    check("r7 warnings", r7["warnings"] == ["credentials embedded in URL"])
    check("r7 normalized", r7["normalized"] == "ssh://git@github.com/user/repo.git")

    # 8. ws non-default port kept + insecure warning
    r8 = compute({"url": "ws://localhost:3000/stream"})
    check("r8 scheme", r8["scheme"] == "ws")
    check("r8 port", r8["port"] == 3000)
    check("r8 warnings", r8["warnings"] == ["insecure scheme (http/ws/ftp)"])
    check("r8 normalized", r8["normalized"] == "ws://localhost:3000/stream")

    # 9. wss default port 443 dropped in normalized; secure -> no warnings
    r9 = compute({"url": "wss://socket.example.com:443/ws"})
    check("r9 port", r9["port"] == 443)
    check("r9 warnings", r9["warnings"] == [])
    check("r9 normalized drops :443", r9["normalized"] == "wss://socket.example.com/ws")

    # 10. scheme-less URL still parses (scheme=""), host empty, port null
    r10 = compute({"url": "example.com/path?a=1"})
    check("r10 scheme empty", r10["scheme"] == "")
    check("r10 host empty", r10["host"] == "")
    check("r10 port null", r10["port"] is None)
    check("r10 path", r10["path"] == "example.com/path")
    check("r10 params", r10["params"] == [{"key": "a", "value": "1"}])
    check("r10 punycode null", r10["host_punycode"] is None)
    check("r10 normalized", r10["normalized"] == "example.com/path?a=1")
    check("r10 warnings", r10["warnings"] == [])

    # 11. mailto: '@' lives in the path, NOT userinfo -> no credentials warning
    r11 = compute({"url": "mailto:darcapalb@gmail.com"})
    check("r11 scheme", r11["scheme"] == "mailto")
    check("r11 host empty", r11["host"] == "")
    check("r11 userinfo false", r11["userinfo_present"] is False)
    check("r11 port null", r11["port"] is None)
    check("r11 path", r11["path"] == "darcapalb@gmail.com")
    check("r11 warnings", r11["warnings"] == [])
    check("r11 normalized", r11["normalized"] == "mailto:darcapalb@gmail.com")

    # 12. invalid (non-numeric) authority port -> treated as absent, default inferred, dropped in normalized
    r12 = compute({"url": "http://x:abc/p"})
    check("r12 host", r12["host"] == "x")
    check("r12 port falls to default", r12["port"] == 80)
    check("r12 normalized drops bad port", r12["normalized"] == "http://x/p")
    check("r12 warnings", r12["warnings"] == ["insecure scheme (http/ws/ftp)"])

    # 13. opaque URN: no authority, scheme has no default port
    r13 = compute({"url": "urn:isbn:0451450523"})
    check("r13 scheme", r13["scheme"] == "urn")
    check("r13 port null", r13["port"] is None)
    check("r13 path", r13["path"] == "isbn:0451450523")
    check("r13 normalized", r13["normalized"] == "urn:isbn:0451450523")
    check("r13 warnings", r13["warnings"] == [])

    # 14. BOTH warnings, order = credentials then insecure
    r14 = compute({"url": "http://admin:secret@10.0.0.1:8080/panel"})
    check("r14 userinfo", r14["userinfo_present"] is True)
    check("r14 host", r14["host"] == "10.0.0.1")
    check("r14 port", r14["port"] == 8080)
    check("r14 both warnings ordered", r14["warnings"] == ["credentials embedded in URL", "insecure scheme (http/ws/ftp)"])
    check("r14 normalized", r14["normalized"] == "http://admin:secret@10.0.0.1:8080/panel")

    # --- hostile / garbage input: MUST return {"error": ...} and NEVER raise ---
    for label, bad in [
        ("None", None),
        ("string payload", "https://example.com"),
        ("list payload", ["url"]),
        ("empty dict", {}),
        ("url None", {"url": None}),
        ("url empty", {"url": ""}),
        ("url blank", {"url": "   "}),
        ("url int", {"url": 123}),
        ("url list", {"url": []}),
        ("url dict", {"url": {"a": 1}}),
    ]:
        out = bad_ok = compute(bad)
        check("hostile %s -> dict" % label, isinstance(out, dict))
        check("hostile %s -> error key" % label, "error" in out)

    # malformed URL (unmatched IPv6 bracket) still must NOT raise -> {"error": ...}
    r_bad = compute({"url": "http://[invalid"})
    check("malformed url -> dict", isinstance(r_bad, dict))
    check("malformed url -> error", "error" in r_bad)

    print("all urlparse checks passed")


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

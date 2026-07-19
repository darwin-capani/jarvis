#!/usr/bin/env python3
"""Plain tests for entropy.compute -- real cases plus hostile/empty inputs."""
import json
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


# --- input-frame bounding (defense in depth) ---------------------------------
# main()'s socket read loop routes every recv() chunk through main.drain_lines,
# which DROPS a partial frame once it passes MAX_FRAME_BYTES with no newline, so a
# peer streaming bytes without a newline cannot grow the read buffer without bound
# (OOM). These assert that real helper — the daemon side is already bounded
# (apps.rs read_line_bounded / genproxy MAX_PROXY_LINE_BYTES).
import main as _frame_mod  # noqa: E402 — deliberately mid-file, after the app's own imports


def test_max_frame_bytes_is_8_mib():
    assert _frame_mod.MAX_FRAME_BYTES == 8 * 1024 * 1024


def test_oversized_frame_is_dropped_not_accumulated():
    # A newline-less frame past the cap is DISCARDED, not retained -> memory bounded.
    cap = _frame_mod.MAX_FRAME_BYTES
    lines, buf, overflowed = _frame_mod.drain_lines(b"x" * (cap + 1))
    assert overflowed is True
    assert buf == b""
    assert lines == []


def test_complete_lines_drain_and_partial_is_preserved():
    # Newline framing is intact: whole lines come out in order; a small partial stays.
    lines, buf, overflowed = _frame_mod.drain_lines(b'{"a":1}\n{"b":2}\n{"c":3')
    assert lines == [b'{"a":1}', b'{"b":2}']
    assert buf == b'{"c":3'
    assert overflowed is False


# -- the agent-tool request/response contract (SHARED shape; copy per app) ----
# entropy.assess carrying a request `id` is answered with a type:"result" line
# echoing that id; the same op with no id keeps the legacy uncorrelated
# type:"items" line (voice/refresh paths depend on it byte-for-byte).
import main as _contract_mod  # noqa: E402 — after the app's own imports


class FakeConn:
    """Captures sendall payloads so handle() can be driven without a socket."""

    def __init__(self):
        self.lines = []

    def sendall(self, raw):
        self.lines.append(json.loads(raw.decode("utf-8").strip()))


def test_tool_op_with_id_answers_a_correlated_result():
    conn = FakeConn()
    _contract_mod.handle(conn, {"type": "entropy.assess", "id": "req-7", "text": "password"})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "result", reply
    assert reply["id"] == "req-7", "the request id is echoed verbatim"
    assert reply["data"]["length"] == 8
    assert reply["token"] == _contract_mod.TOKEN


def test_tool_op_without_id_keeps_the_legacy_items_line():
    conn = FakeConn()
    _contract_mod.handle(conn, {"type": "entropy.assess", "text": "password"})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "items", "no id -> uncorrelated legacy line"
    assert "id" not in reply
    assert reply["data"]["length"] == 8


def test_non_string_or_empty_id_is_treated_as_absent():
    for bad_id in (7, "", None, ["x"]):
        conn = FakeConn()
        _contract_mod.handle(conn, {"type": "entropy.assess", "id": bad_id, "text": "password"})
        assert conn.lines[0]["type"] == "items", f"id={bad_id!r} must not correlate"


if __name__ == "__main__":
    # Script-style runs exercise the framing tests too — they are plain
    # functions the runner below would otherwise never call.
    test_max_frame_bytes_is_8_mib()
    test_oversized_frame_is_dropped_not_accumulated()
    test_complete_lines_drain_and_partial_is_preserved()
    print("framing: 3 checks ok")
    fns = [v for k, v in sorted(globals().items()) if k.startswith("test_") and callable(v)]
    for fn in fns:
        fn()
    print(f"ok: {len(fns)} tests passed")

#!/usr/bin/env python3
"""Plain-python tests for numbase.compute — real cases plus hostile input that must not raise."""
import sys

from main import compute


def check(name, cond):
    if not cond:
        print("FAIL:", name)
        sys.exit(1)
    print("ok:", name)


def main():
    # 1) Default base 10 -> all representations.
    r = compute({"value": "255"})
    check("dec 255 decimal", r.get("decimal") == 255)
    check("dec 255 binary", r.get("binary") == "11111111")
    check("dec 255 octal", r.get("octal") == "377")
    check("dec 255 hex", r.get("hex") == "ff")

    # 2) Hex source base -> decimal + rerender; hex must be lowercase, no prefixes.
    r = compute({"value": "FF", "from_base": 16})
    check("hex FF decimal", r.get("decimal") == 255)
    check("hex FF binary", r.get("binary") == "11111111")
    check("hex FF hex lowercase", r.get("hex") == "ff")

    # 3) Binary source base.
    r = compute({"value": "1010", "from_base": 2})
    check("bin 1010 decimal", r.get("decimal") == 10)
    check("bin 1010 hex", r.get("hex") == "a")
    check("bin 1010 octal", r.get("octal") == "12")

    # 4) Negative sign preserved across every representation.
    r = compute({"value": "-42"})
    check("neg 42 decimal", r.get("decimal") == -42)
    check("neg 42 binary", r.get("binary") == "-101010")
    check("neg 42 octal", r.get("octal") == "-52")
    check("neg 42 hex", r.get("hex") == "-2a")

    # 5) Base 36 uses full alphanumeric alphabet.
    r = compute({"value": "z", "from_base": 36})
    check("base36 z decimal", r.get("decimal") == 35)
    check("base36 z hex", r.get("hex") == "23")

    # 6) from_base given as a numeric string is accepted.
    r = compute({"value": "10", "from_base": "16"})
    check("str base decimal", r.get("decimal") == 16)

    # 7) Whitespace around the value is tolerated.
    r = compute({"value": "  100  ", "from_base": 10})
    check("whitespace decimal", r.get("decimal") == 100)

    # 8) Invalid digit for the base -> error, no raise.
    r = compute({"value": "2", "from_base": 2})
    check("bad digit error", "error" in r)

    # 9) Out-of-range base -> error.
    r = compute({"value": "10", "from_base": 99})
    check("base range error", "error" in r)
    r = compute({"value": "10", "from_base": 1})
    check("base too small error", "error" in r)

    # 10) Empty value -> error.
    check("empty value error", "error" in compute({"value": ""}))
    check("missing value error", "error" in compute({}))

    # 11) Hostile / malformed inputs must NOT raise and must report an error.
    for bad in [None, 123, "not a dict", [], ["a"], {"value": ["x"]},
                {"value": "10", "from_base": "abc"}, {"value": "10", "from_base": True},
                {"value": True}, {"value": {"nested": 1}}, {"value": None}]:
        out = compute(bad)
        check("hostile no-raise: %r" % (bad,), isinstance(out, dict) and "error" in out)

    # 12) Zero converts cleanly.
    r = compute({"value": "0", "from_base": 10})
    check("zero decimal", r.get("decimal") == 0)
    check("zero binary", r.get("binary") == "0")
    check("zero hex", r.get("hex") == "0")

    print("ALL PASS")
    sys.exit(0)


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
# handle()'s numbase.convert branch answers via reply_result: an op carrying a
# non-empty string `id` gets a correlated type:"result" line echoing that id; an
# op without one keeps the legacy uncorrelated type:"items" line byte-for-byte.
import json  # noqa: E402 — used only by the contract tests below


class FakeConn:
    """Captures sendall payloads so handle() can be driven without a socket."""

    def __init__(self):
        self.lines = []

    def sendall(self, raw):
        self.lines.append(json.loads(raw.decode("utf-8").strip()))


def test_tool_op_with_id_answers_a_correlated_result():
    conn = FakeConn()
    _frame_mod.handle(conn, {"type": "numbase.convert", "value": "255", "id": "req-7"})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "result", reply
    assert reply["id"] == "req-7", "the request id is echoed verbatim"
    assert reply["data"]["decimal"] == 255
    assert reply["token"] == _frame_mod.TOKEN


def test_tool_op_without_id_keeps_the_legacy_items_line():
    conn = FakeConn()
    _frame_mod.handle(conn, {"type": "numbase.convert", "value": "255"})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "items", "no id -> uncorrelated legacy line"
    assert "id" not in reply
    assert reply["data"]["decimal"] == 255


def test_non_string_or_empty_id_is_treated_as_absent():
    for bad_id in (7, "", None, ["x"]):
        conn = FakeConn()
        _frame_mod.handle(conn, {"type": "numbase.convert", "value": "255", "id": bad_id})
        assert conn.lines[0]["type"] == "items", f"id={bad_id!r} must not correlate"


if __name__ == "__main__":
    # Script-style runs exercise the framing tests too — they are plain
    # functions the runner below would otherwise never call.
    test_max_frame_bytes_is_8_mib()
    test_oversized_frame_is_dropped_not_accumulated()
    test_complete_lines_drain_and_partial_is_preserved()
    print("framing: 3 checks ok")
    test_tool_op_with_id_answers_a_correlated_result()
    test_tool_op_without_id_keeps_the_legacy_items_line()
    test_non_string_or_empty_id_is_treated_as_absent()
    print("contract: 3 checks ok")
    main()

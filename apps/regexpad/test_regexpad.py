#!/usr/bin/env python3
"""Plain tests for regexpad.compute — real cases + hostile/empty input that must not raise."""
from main import compute


def test_basic_matches_and_count():
    r = compute({"pattern": r"\d+", "text": "a1 b22 c333"})
    assert "error" not in r, r
    assert r["count"] == 3, r
    assert [m["match"] for m in r["matches"]] == ["1", "22", "333"], r
    assert r["truncated"] is False, r


def test_capture_groups():
    r = compute({"pattern": r"(\w+)=(\d+)", "text": "x=1 yy=22"})
    assert "error" not in r, r
    assert r["count"] == 2, r
    assert r["matches"][0]["groups"] == ["x", "1"], r
    assert r["matches"][1]["groups"] == ["yy", "22"], r
    assert r["matches"][0]["start"] == 0 and r["matches"][0]["end"] == 3, r


def test_ignorecase_flag():
    off = compute({"pattern": "abc", "text": "ABC abc"})
    assert off["count"] == 1, off
    on = compute({"pattern": "abc", "text": "ABC abc", "ignmatchcase": True})
    assert on["count"] == 2, on
    assert on["ignorecase"] is True, on


def test_invalid_pattern_returns_error_not_raise():
    r = compute({"pattern": "(", "text": "anything"})
    assert isinstance(r, dict), r
    assert "error" in r, r
    assert r["error"].startswith("invalid pattern"), r


def test_no_matches():
    r = compute({"pattern": r"zzz", "text": "nothing here"})
    assert "error" not in r, r
    assert r["count"] == 0, r
    assert r["matches"] == [], r


def test_cap_at_50_but_count_full():
    # 120 single-char matches; matches list capped to 50, count reflects all.
    r = compute({"pattern": "a", "text": "a" * 120})
    assert r["count"] == 120, r
    assert len(r["matches"]) == 50, r
    assert r["truncated"] is True, r


def test_optional_group_none_preserved():
    r = compute({"pattern": r"(a)?b", "text": "b"})
    assert "error" not in r, r
    assert r["count"] == 1, r
    assert r["matches"][0]["groups"] == [None], r


def test_hostile_and_empty_inputs_never_raise():
    # None payload
    assert "error" in compute(None)
    # non-str pattern
    assert "error" in compute({"pattern": 123, "text": "x"})
    # non-str text
    assert "error" in compute({"pattern": "x", "text": 5})
    # empty dict -> empty pattern matches, but must be a clean dict result
    r = compute({})
    assert isinstance(r, dict), r
    # a list instead of dict
    assert "error" in compute([1, 2, 3])
    # empty string pattern against empty text
    r2 = compute({"pattern": "", "text": ""})
    assert isinstance(r2, dict) and "error" not in r2, r2


def test_catastrophic_pattern_times_out_not_hangs():
    # ReDoS guard: "(a+)+b" on "aaaa…" backtracks exponentially. compute() must
    # return an error within the wall-clock cap, NOT hang the app/daemon.
    import time
    t0 = time.time()
    r = compute({"pattern": "(a+)+b", "text": "a" * 40})
    elapsed = time.time() - t0
    assert isinstance(r, dict), r
    assert "error" in r, r
    assert elapsed < 3.0, f"must not hang; took {elapsed:.2f}s"


def test_oversized_inputs_return_error():
    assert "error" in compute({"pattern": "a" * 3000, "text": "x"})
    assert "error" in compute({"pattern": "x", "text": "a" * 200_000})


def main():
    tests = [
        test_basic_matches_and_count,
        test_capture_groups,
        test_ignorecase_flag,
        test_invalid_pattern_returns_error_not_raise,
        test_no_matches,
        test_cap_at_50_but_count_full,
        test_optional_group_none_preserved,
        test_hostile_and_empty_inputs_never_raise,
        test_catastrophic_pattern_times_out_not_hangs,
        test_oversized_inputs_return_error,
        test_tool_op_with_id_answers_a_correlated_result,
        test_tool_op_without_id_keeps_the_legacy_items_line,
        test_non_string_or_empty_id_is_treated_as_absent,
    ]
    for t in tests:
        t()
        print(f"ok: {t.__name__}")
    print(f"ALL PASSED ({len(tests)} tests)")
    return 0


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
# A domain op carrying a request `id` is answered with a type:"result" line
# echoing that id (via main.reply_result); an op without one keeps the legacy
# uncorrelated type:"items" line. handle() is driven directly, no socket.
import json  # noqa: E402 — used by the FakeConn/contract tests below


class FakeConn:
    """Captures sendall payloads so handle() can be driven without a socket."""

    def __init__(self):
        self.lines = []

    def sendall(self, raw):
        self.lines.append(json.loads(raw.decode("utf-8").strip()))


def test_tool_op_with_id_answers_a_correlated_result():
    conn = FakeConn()
    _frame_mod.handle(conn, {"type": "regexpad.test", "id": "req-7", "pattern": r"\d+", "text": "a1 b22 c333"})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "result", reply
    assert reply["id"] == "req-7", "the request id is echoed verbatim"
    assert "error" not in reply["data"], reply
    assert reply["data"]["count"] == 3, reply
    assert reply["token"] == _frame_mod.TOKEN


def test_tool_op_without_id_keeps_the_legacy_items_line():
    conn = FakeConn()
    _frame_mod.handle(conn, {"type": "regexpad.test", "pattern": r"\d+", "text": "a1 b22 c333"})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "items", "no id -> uncorrelated legacy line"
    assert "id" not in reply
    assert reply["data"]["count"] == 3, reply


def test_non_string_or_empty_id_is_treated_as_absent():
    for bad_id in (7, "", None, ["x"]):
        conn = FakeConn()
        _frame_mod.handle(conn, {"type": "regexpad.test", "id": bad_id, "pattern": r"\d+", "text": "a1 b22 c333"})
        assert conn.lines[0]["type"] == "items", f"id={bad_id!r} must not correlate"


if __name__ == "__main__":
    # Script-style runs exercise the framing tests too — they are plain
    # functions the runner below would otherwise never call.
    test_max_frame_bytes_is_8_mib()
    test_oversized_frame_is_dropped_not_accumulated()
    test_complete_lines_drain_and_partial_is_preserved()
    print("framing: 3 checks ok")
    import sys
    sys.exit(main())

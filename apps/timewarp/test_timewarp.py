#!/usr/bin/env python3
"""Tests for timewarp.compute — real cases plus hostile/empty input that must not raise."""
import json

import main
from main import compute


def test_epoch_zero():
    r = compute({"epoch": 0})
    assert r.get("iso_utc") == "1970-01-01T00:00:00+00:00", r
    assert r["year"] == 1970 and r["month"] == 1 and r["day"] == 1, r
    assert r["hour"] == 0 and r["minute"] == 0 and r["second"] == 0, r
    assert r["weekday"] == "Thu", r  # 1970-01-01 was a Thursday


def test_known_timestamp_int():
    # 1609459200 = 2021-01-01T00:00:00Z (a Friday)
    r = compute({"epoch": 1609459200})
    assert r.get("iso_utc") == "2021-01-01T00:00:00+00:00", r
    assert (r["year"], r["month"], r["day"]) == (2021, 1, 1), r
    assert r["weekday"] == "Fri", r


def test_numeric_string_with_time():
    # 1234567890 = 2009-02-13T23:31:30Z (a Friday)
    r = compute({"epoch": "1234567890"})
    assert r.get("iso_utc") == "2009-02-13T23:31:30+00:00", r
    assert (r["hour"], r["minute"], r["second"]) == (23, 31, 30), r
    assert r["weekday"] == "Fri", r


def test_float_string():
    r = compute({"epoch": "0.0"})
    assert r.get("year") == 1970, r
    assert r.get("weekday") == "Thu", r


def test_out_of_range():
    # Absurdly large epoch must be guarded, not raise.
    r = compute({"epoch": 10 ** 30})
    assert "error" in r, r


def test_non_numeric():
    r = compute({"epoch": "not-a-number"})
    assert "error" in r, r


def test_hostile_and_empty_must_not_raise():
    # None payload, wrong types, missing key, empty string, bool, containers, nan/inf.
    for bad in (None, 42, "x", [], {}, {"epoch": ""}, {"epoch": True},
                {"epoch": []}, {"epoch": {}}, {"epoch": None},
                {"epoch": float("nan")}, {"epoch": float("inf")}):
        r = compute(bad)
        assert isinstance(r, dict), (bad, r)
        assert "error" in r, (bad, r)  # no valid conversion expected


def run():
    test_epoch_zero()
    test_known_timestamp_int()
    test_numeric_string_with_time()
    test_float_string()
    test_out_of_range()
    test_non_numeric()
    test_hostile_and_empty_must_not_raise()
    print("all timewarp tests passed")


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
# The declared tool timewarp.convert is offered to the agent loop; a request
# carrying a `id` must be answered with a type:"result" line echoing that id,
# while an id-less op keeps the legacy uncorrelated type:"items" line. {"epoch": 0}
# is a minimal payload compute() answers with a non-error dict (1970-01-01 UTC).


class FakeConn:
    """Captures sendall payloads so handle() can be driven without a socket."""

    def __init__(self):
        self.lines = []

    def sendall(self, raw):
        self.lines.append(json.loads(raw.decode("utf-8").strip()))


def test_tool_op_with_id_answers_a_correlated_result():
    conn = FakeConn()
    main.handle(conn, {"type": "timewarp.convert", "id": "req-7", "epoch": 0})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "result", reply
    assert reply["id"] == "req-7", "the request id is echoed verbatim"
    assert reply["data"]["year"] == 1970
    assert reply["token"] == main.TOKEN


def test_tool_op_without_id_keeps_the_legacy_items_line():
    conn = FakeConn()
    main.handle(conn, {"type": "timewarp.convert", "epoch": 0})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "items", "no id -> uncorrelated legacy line"
    assert "id" not in reply
    assert reply["data"]["year"] == 1970


def test_non_string_or_empty_id_is_treated_as_absent():
    for bad_id in (7, "", None, ["x"]):
        conn = FakeConn()
        main.handle(conn, {"type": "timewarp.convert", "id": bad_id, "epoch": 0})
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
    run()

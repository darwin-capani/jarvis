#!/usr/bin/env python3
"""Tests for timewarp.compute — real cases plus hostile/empty input that must not raise."""
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


if __name__ == "__main__":
    # Script-style runs exercise the framing tests too — they are plain
    # functions the runner below would otherwise never call.
    test_max_frame_bytes_is_8_mib()
    test_oversized_frame_is_dropped_not_accumulated()
    test_complete_lines_drain_and_partial_is_preserved()
    print("framing: 3 checks ok")
    run()

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


if __name__ == "__main__":
    run()

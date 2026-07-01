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
    ]
    for t in tests:
        t()
        print(f"ok: {t.__name__}")
    print(f"ALL PASSED ({len(tests)} tests)")
    return 0


if __name__ == "__main__":
    import sys
    sys.exit(main())

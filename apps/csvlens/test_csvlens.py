#!/usr/bin/env python3
"""Tests for csvlens.compute — real cases plus hostile/empty inputs that must not raise."""
from main import compute


def test_basic_profile():
    csv_text = "name,age,city\nAlice,30,NYC\nBob,,LA\n"
    out = compute({"csv": csv_text})
    assert out.get("rows") == 2, out
    assert out.get("columns") == 3, out
    assert out.get("headers") == ["name", "age", "city"], out
    stats = {s["name"]: s for s in out["column_stats"]}
    assert stats["name"]["non_empty"] == 2 and stats["name"]["empty"] == 0, out
    # Bob's age is blank -> 1 non_empty, 1 empty.
    assert stats["age"]["non_empty"] == 1 and stats["age"]["empty"] == 1, out
    assert stats["city"]["non_empty"] == 2 and stats["city"]["empty"] == 0, out


def test_custom_delimiter_and_short_rows():
    # Semicolon delimiter; second data row is short -> missing cell counts as empty.
    csv_text = "a;b;c\n1;2;3\n4;5\n"
    out = compute({"csv": csv_text, "delimiter": ";"})
    assert out.get("columns") == 3, out
    assert out.get("rows") == 2, out
    stats = {s["name"]: s for s in out["column_stats"]}
    # Column c: row1 has "3" (non_empty), row2 missing (empty).
    assert stats["c"]["non_empty"] == 1 and stats["c"]["empty"] == 1, out
    # Whitespace-only cells count as empty.
    out2 = compute({"csv": "x,y\n foo ,   \n"})
    stats2 = {s["name"]: s for s in out2["column_stats"]}
    assert stats2["x"]["non_empty"] == 1, out2
    assert stats2["y"]["empty"] == 1, out2


def test_header_only_has_zero_data_rows():
    out = compute({"csv": "col1,col2,col3\n"})
    assert out.get("rows") == 0, out
    assert out.get("columns") == 3, out
    for s in out["column_stats"]:
        assert s["non_empty"] == 0 and s["empty"] == 0, out


def test_cap_at_50_columns():
    header = ",".join(f"c{i}" for i in range(60))
    row = ",".join(str(i) for i in range(60))
    out = compute({"csv": header + "\n" + row + "\n"})
    # columns reflects true width; headers/column_stats capped at 50.
    assert out.get("columns") == 60, out
    assert len(out["headers"]) == 50, out
    assert len(out["column_stats"]) == 50, out


def test_hostile_and_empty_inputs_do_not_raise():
    # Empty string.
    assert "error" in compute({"csv": ""}), "empty csv should error"
    # Missing csv key.
    assert "error" in compute({}), "missing csv should error"
    # Non-dict payload.
    assert "error" in compute(None), "None payload should error"
    assert "error" in compute("not a dict"), "string payload should error"
    assert "error" in compute(42), "int payload should error"
    # Wrong type for csv.
    assert "error" in compute({"csv": 123}), "non-string csv should error"
    assert "error" in compute({"csv": ["a", "b"]}), "list csv should error"
    # Bad delimiter (multi-char / non-string).
    assert "error" in compute({"csv": "a,b\n1,2\n", "delimiter": ",,"}), "multi-char delim should error"
    assert "error" in compute({"csv": "a,b\n1,2\n", "delimiter": 5}), "non-string delim should error"
    # Ragged / messy content must not raise.
    messy = compute({"csv": 'a,b,c\n"unterminated,quote,here\n1,2\n'})
    assert isinstance(messy, dict), messy


def run():
    for name, fn in sorted(globals().items()):
        if name.startswith("test_") and callable(fn):
            fn()
            print(f"ok  {name}")
    print("ALL PASS")


if __name__ == "__main__":
    run()

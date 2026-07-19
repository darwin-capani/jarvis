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
# A csvlens.profile op carrying a request `id` is answered with a type:"result"
# line echoing that id (main.reply_result); an op without one keeps the legacy
# uncorrelated type:"items" line. A minimal valid payload profiles one data row.
import json  # noqa: E402 — deliberately mid-file, after the app's own imports
import main as _contract_mod  # noqa: E402


class FakeConn:
    """Captures sendall payloads so handle() can be driven without a socket."""

    def __init__(self):
        self.lines = []

    def sendall(self, raw):
        self.lines.append(json.loads(raw.decode("utf-8").strip()))


def test_tool_op_with_id_answers_a_correlated_result():
    conn = FakeConn()
    _contract_mod.handle(conn, {"type": "csvlens.profile", "id": "req-7", "csv": "name,age\nAlice,30\n"})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "result", reply
    assert reply["id"] == "req-7", "the request id is echoed verbatim"
    assert reply["data"]["rows"] == 1
    assert reply["token"] == _contract_mod.TOKEN


def test_tool_op_without_id_keeps_the_legacy_items_line():
    conn = FakeConn()
    _contract_mod.handle(conn, {"type": "csvlens.profile", "csv": "name,age\nAlice,30\n"})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "items", "no id -> uncorrelated legacy line"
    assert "id" not in reply
    assert reply["data"]["rows"] == 1


def test_non_string_or_empty_id_is_treated_as_absent():
    for bad_id in (7, "", None, ["x"]):
        conn = FakeConn()
        _contract_mod.handle(conn, {"type": "csvlens.profile", "id": bad_id, "csv": "name,age\nAlice,30\n"})
        assert conn.lines[0]["type"] == "items", f"id={bad_id!r} must not correlate"


if __name__ == "__main__":
    # Script-style runs exercise the framing tests too — they are plain
    # functions the runner below would otherwise never call.
    test_max_frame_bytes_is_8_mib()
    test_oversized_frame_is_dropped_not_accumulated()
    test_complete_lines_drain_and_partial_is_preserved()
    print("framing: 3 checks ok")
    # The agent-tool request/response contract (request-id echo / legacy items).
    test_tool_op_with_id_answers_a_correlated_result()
    test_tool_op_without_id_keeps_the_legacy_items_line()
    test_non_string_or_empty_id_is_treated_as_absent()
    print("contract: 3 checks ok")
    run()

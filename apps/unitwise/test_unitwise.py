#!/usr/bin/env python3
"""Plain-python tests for unitwise.compute — real cases plus hostile input that must not raise."""
import json
import math
import sys

from main import compute


def check(name, cond):
    if not cond:
        print("FAIL:", name)
        sys.exit(1)
    print("ok:", name)


def close(a, b, tol=1e-9):
    return isinstance(a, (int, float)) and abs(a - b) <= tol * max(1.0, abs(b))


def main():
    # --- length ---
    r = compute({"value": 1, "from": "km", "to": "m"})
    check("km->m result", r["result"] == 1000.0)
    check("km->m category", r["category"] == "length")
    check("km->m echoes from/to/value", r["from"] == "km" and r["to"] == "m" and r["value"] == 1)

    check("100 cm->m", close(compute({"value": 100, "from": "cm", "to": "m"})["result"], 1.0))
    check("1 mi->km", close(compute({"value": 1, "from": "mi", "to": "km"})["result"], 1.609344))
    check("12 in->ft", close(compute({"value": 12, "from": "in", "to": "ft"})["result"], 1.0))
    check("1 nmi->m", close(compute({"value": 1, "from": "nmi", "to": "m"})["result"], 1852.0))

    # --- mass ---
    check("1 kg->g", compute({"value": 1, "from": "kg", "to": "g"})["result"] == 1000.0)
    check("1 lb->kg", close(compute({"value": 1, "from": "lb", "to": "kg"})["result"], 0.45359237))
    check("16 oz->lb", close(compute({"value": 16, "from": "oz", "to": "lb"})["result"], 1.0))
    check("1 t->kg", compute({"value": 1, "from": "t", "to": "kg"})["result"] == 1000.0)

    # --- time ---
    check("1 h->min", compute({"value": 1, "from": "h", "to": "min"})["result"] == 60.0)
    check("1 day->s", compute({"value": 1, "from": "day", "to": "s"})["result"] == 86400.0)
    check("1 week->day", close(compute({"value": 1, "from": "week", "to": "day"})["result"], 7.0))

    # --- data (SI vs binary, case-sensitive) ---
    check("1 KiB->B", compute({"value": 1, "from": "KiB", "to": "B"})["result"] == 1024.0)
    check("1 MB->KB", close(compute({"value": 1, "from": "MB", "to": "KB"})["result"], 1000.0))
    check("8 bit->B", close(compute({"value": 8, "from": "bit", "to": "B"})["result"], 1.0))
    check("1 GiB->MiB", close(compute({"value": 1, "from": "GiB", "to": "MiB"})["result"], 1024.0))
    check("1 Mbit->Kbit", close(compute({"value": 1, "from": "Mbit", "to": "Kbit"})["result"], 1000.0))

    # --- pressure ---
    check("1 bar->Pa", compute({"value": 1, "from": "bar", "to": "Pa"})["result"] == 100000.0)
    check("1 atm->kPa", close(compute({"value": 1, "from": "atm", "to": "kPa"})["result"], 101.325))
    check("760 mmHg->atm", close(compute({"value": 760, "from": "mmHg", "to": "atm"})["result"], 1.0000001424663214))

    # --- energy ---
    check("1 kWh->J", compute({"value": 1, "from": "kWh", "to": "J"})["result"] == 3.6e6)
    check("1 kcal->cal", close(compute({"value": 1, "from": "kcal", "to": "cal"})["result"], 1000.0))
    check("1 Wh->J", compute({"value": 1, "from": "Wh", "to": "J"})["result"] == 3600.0)

    # --- power ---
    check("1 kW->W", compute({"value": 1, "from": "kW", "to": "W"})["result"] == 1000.0)
    check("1 hp->W", close(compute({"value": 1, "from": "hp", "to": "W"})["result"], 745.6998715822702))

    # --- force ---
    check("1 kN->N", compute({"value": 1, "from": "kN", "to": "N"})["result"] == 1000.0)
    check("1 lbf->N", close(compute({"value": 1, "from": "lbf", "to": "N"})["result"], 4.4482216152605))

    # --- angle ---
    check("180 deg->rad", close(compute({"value": 180, "from": "deg", "to": "rad"})["result"], math.pi))
    check("1 rad->deg", close(compute({"value": 1, "from": "rad", "to": "deg"})["result"], 57.29577951308232))
    check("200 grad->deg", close(compute({"value": 200, "from": "grad", "to": "deg"})["result"], 180.0))

    # --- temperature (affine, via kelvin) ---
    check("0 C->F", close(compute({"value": 0, "from": "C", "to": "F"})["result"], 32.0))
    check("100 C->K", close(compute({"value": 100, "from": "C", "to": "K"})["result"], 373.15))
    check("32 F->C", close(compute({"value": 32, "from": "F", "to": "C"})["result"], 0.0))
    check("300 K->C", close(compute({"value": 300, "from": "K", "to": "C"})["result"], 26.85))
    check("-40 C->F", close(compute({"value": -40, "from": "C", "to": "F"})["result"], -40.0))
    check("C->F category", compute({"value": 0, "from": "C", "to": "F"})["category"] == "temperature")

    # --- hostile / error inputs: each returns an {"error": ...} dict and never raises ---
    check("None -> error dict", isinstance(compute(None), dict) and "error" in compute(None))
    check("None message", compute(None)["error"] == "payload must be a mapping")
    check("list payload -> error", compute([1, 2, 3])["error"] == "payload must be a mapping")
    check("empty dict -> missing value", compute({})["error"] == "missing 'value'")
    check("string value -> error", compute({"value": "x", "from": "m", "to": "km"})["error"] == "value must be a number")
    check("bool value -> error", compute({"value": True, "from": "m", "to": "km"})["error"] == "value must be a number")
    check("list value -> error", "error" in compute({"value": []}))
    check("nan value -> error", compute({"value": float("nan"), "from": "m", "to": "km"})["error"] == "value must be a finite number")
    check("inf value -> error", "error" in compute({"value": float("inf"), "from": "m", "to": "km"}))
    check("non-string from -> error", compute({"value": 1, "from": [], "to": "m"})["error"] == "from and to must be strings")
    check("unknown from -> error", compute({"value": 1, "from": "foo", "to": "m"})["error"] == "unknown unit: foo")
    check("unknown to -> error", compute({"value": 1, "from": "m", "to": "bar-baz"})["error"] == "unknown unit: bar-baz")
    check("case-sensitive kB unknown", compute({"value": 1, "from": "kB", "to": "B"})["error"] == "unknown unit: kB")
    check("incompatible length vs mass", compute({"value": 1, "from": "m", "to": "kg"})["error"] == "incompatible units: m vs kg")
    check("incompatible temp vs length", compute({"value": 1, "from": "C", "to": "m"})["error"] == "incompatible units: C vs m")
    check("garbage from field", isinstance(compute({"value": 1, "from": "garbage", "to": "s"}), dict))

    # identity conversions
    check("m->m identity", compute({"value": 5, "from": "m", "to": "m"})["result"] == 5.0)
    check("K->K identity", close(compute({"value": 42, "from": "K", "to": "K"})["result"], 42.0))


    # REVIEW PIN: an overflowing conversion returns an error dict, never inf
    # (invalid JSON on the wire -> the daemon drops the frame).
    r = compute({"value": 1e308, "from": "PB", "to": "bit"})
    check("overflow conversion guarded", "error" in r and "not finite" in r["error"])
    r = compute({"value": 1e290, "from": "kWh", "to": "eV"})
    check("kWh->eV overflow guarded", "error" in r)

    print("all unitwise checks passed")


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


# -- the agent-tool request/response contract (SHARED shape; copy per app) ----


class FakeConn:
    """Captures sendall payloads so handle() can be driven without a socket."""

    def __init__(self):
        self.lines = []

    def sendall(self, raw):
        self.lines.append(json.loads(raw.decode("utf-8").strip()))


def test_tool_op_with_id_answers_a_correlated_result():
    conn = FakeConn()
    _frame_mod.handle(conn, {"type": "unit.convert", "id": "req-7", "value": 1, "from": "km", "to": "m"})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "result", reply
    assert reply["id"] == "req-7", "the request id is echoed verbatim"
    assert reply["data"]["result"] == 1000.0
    assert reply["token"] == _frame_mod.TOKEN


def test_tool_op_without_id_keeps_the_legacy_items_line():
    conn = FakeConn()
    _frame_mod.handle(conn, {"type": "unit.convert", "value": 1, "from": "km", "to": "m"})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "items", "no id -> uncorrelated legacy line"
    assert "id" not in reply
    assert reply["data"]["result"] == 1000.0


def test_non_string_or_empty_id_is_treated_as_absent():
    for bad_id in (7, "", None, ["x"]):
        conn = FakeConn()
        _frame_mod.handle(conn, {"type": "unit.convert", "id": bad_id, "value": 1, "from": "km", "to": "m"})
        assert conn.lines[0]["type"] == "items", f"id={bad_id!r} must not correlate"


if __name__ == "__main__":
    test_max_frame_bytes_is_8_mib()
    test_oversized_frame_is_dropped_not_accumulated()
    test_complete_lines_drain_and_partial_is_preserved()
    print("framing: 3 checks ok")
    test_tool_op_with_id_answers_a_correlated_result()
    test_tool_op_without_id_keeps_the_legacy_items_line()
    test_non_string_or_empty_id_is_treated_as_absent()
    print("agent-tool contract: 3 checks ok")
    main()

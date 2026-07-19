#!/usr/bin/env python3
"""Plain-python tests for ohmslaw.compute — real cases plus hostile input that must not raise."""
import sys

from main import compute
import main as _main


def check(name, cond):
    if not cond:
        print("FAIL:", name)
        sys.exit(1)
    print("ok:", name)


def approx(a, b, tol=1e-9):
    return abs(a - b) <= tol * max(1.0, abs(a), abs(b))


def is_err(r):
    return isinstance(r, dict) and "error" in r and set(r) == {"error"}


def main():
    # --- SI parser (exact where representable) ---
    check("parse int", _main._parse_si(5) == 5.0)
    check("parse plain 12V", _main._parse_si("12V") == 12.0)
    check("parse 0.25W", _main._parse_si("0.25W") == 0.25)
    check("parse 2kohm", _main._parse_si("2kohm") == 2000.0)
    check("parse 2kOmega", _main._parse_si("2kΩ") == 2000.0)
    check("parse 10mA", approx(_main._parse_si("10mA"), 0.01))
    check("parse 470uA", approx(_main._parse_si("470uA"), 0.00047))
    check("parse 470microA", approx(_main._parse_si("470µA"), 0.00047))
    check("parse 2.2kohm", approx(_main._parse_si("2.2kohm"), 2200.0))
    check("parse negative", _main._parse_si("-5V") == -5.0)

    # --- 1. V,I known -> R,P ---
    r = compute({"voltage": 12, "current": 2})
    check("VI voltage", r["voltage"] == 12.0)
    check("VI current", r["current"] == 2.0)
    check("VI R", r["resistance"] == 6.0)
    check("VI P", r["power"] == 24.0)
    check("VI formulas", r["formulas_used"] == ["R=V/I", "P=V*I"])

    # --- 2. V,R known (SI strings) -> I,P ---
    r = compute({"voltage": "12V", "resistance": "6ohm"})
    check("VR current", r["current"] == 2.0)
    check("VR power", r["power"] == 24.0)
    check("VR formulas", r["formulas_used"] == ["I=V/R", "P=V^2/R"])

    # --- 3. I,R known -> V,P ---
    r = compute({"current": 3, "resistance": 4})
    check("IR voltage", r["voltage"] == 12.0)
    check("IR power", r["power"] == 36.0)
    check("IR formulas", r["formulas_used"] == ["V=I*R", "P=I^2*R"])

    # --- 4. I,P known -> V,R ---
    r = compute({"current": 2, "power": 8})
    check("IP voltage", r["voltage"] == 4.0)
    check("IP resistance", r["resistance"] == 2.0)
    check("IP formulas", r["formulas_used"] == ["V=P/I", "R=P/I^2"])

    # --- 5. V,P known -> I,R ---
    r = compute({"voltage": 4, "power": 8})
    check("VP current", r["current"] == 2.0)
    check("VP resistance", r["resistance"] == 2.0)
    check("VP formulas", r["formulas_used"] == ["I=P/V", "R=V^2/P"])

    # --- 6. R,P known -> I,V ---
    r = compute({"resistance": 2, "power": 8})
    check("RP current", r["current"] == 2.0)
    check("RP voltage", r["voltage"] == 4.0)
    check("RP formulas", r["formulas_used"] == ["I=sqrt(P/R)", "V=sqrt(P*R)"])

    # --- 7. SI mixed: 5V + 10mA ---
    r = compute({"voltage": "5V", "current": "10mA"})
    check("mix R", approx(r["resistance"], 500.0))
    check("mix P", approx(r["power"], 0.05))

    # --- 8. SI kilo-ohm ---
    r = compute({"voltage": "11V", "resistance": "2.2kohm"})
    check("kohm I", approx(r["current"], 0.005))
    check("kohm P", approx(r["power"], 0.055))

    # --- 9. Omega symbol as resistance, exact ---
    r = compute({"resistance": "2kΩ", "current": 2})
    check("omega V", r["voltage"] == 4000.0)
    check("omega P", r["power"] == 8000.0)

    # --- 10. micro-amp current with kilo-ohm ---
    r = compute({"current": "470uA", "resistance": "1kohm"})
    check("uA V", approx(r["voltage"], 0.47))
    check("uA P", approx(r["power"], 0.00047 * 0.00047 * 1000.0))

    # --- hostile input: must return an {"error": ...} dict and NOT raise ---
    check("none", is_err(compute(None)))
    check("empty", is_err(compute({})))
    check("list payload", is_err(compute([])))
    check("string payload", is_err(compute("hello")))
    check("one known", is_err(compute({"voltage": 5})))
    check("three knowns", is_err(compute({"voltage": 1, "current": 1, "resistance": 1})))
    check("garbage value", is_err(compute({"voltage": "garbage", "current": 1})))
    check("list value", is_err(compute({"voltage": [], "current": 1})))
    check("2k2 rejected", is_err(compute({"current": 2, "power": "2k2"})))
    check("zero resistance divzero", is_err(compute({"voltage": 10, "resistance": 0})))
    check("zero current divzero", is_err(compute({"voltage": 10, "current": 0})))
    check("negative resistance", is_err(compute({"resistance": -5, "voltage": 10})))
    check("negative power", is_err(compute({"power": -1, "current": 2})))
    check("nan value", is_err(compute({"voltage": float("nan"), "current": 1})))
    check("inf value", is_err(compute({"voltage": float("inf"), "current": 1})))
    check("bool value", is_err(compute({"voltage": True, "current": 1})))
    check("computed negative R", is_err(compute({"voltage": -5, "current": 2})))

    print("all ohmslaw checks passed")


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


if __name__ == "__main__":
    test_max_frame_bytes_is_8_mib()
    test_oversized_frame_is_dropped_not_accumulated()
    test_complete_lines_drain_and_partial_is_preserved()
    print("framing: 3 checks ok")
    main()

#!/usr/bin/env python3
"""Tests for cronwise.compute: wildcards, steps, ranges, lists, and hostile/empty input."""
import unittest

from main import compute


class TestCronwiseExplain(unittest.TestCase):
    def test_every_five_minutes(self):
        r = compute({"cron": "*/5 * * * *"})
        self.assertTrue(r["valid"])
        self.assertEqual(r["minute"], "every 5 minutes")
        self.assertEqual(r["hour"], "every hour")
        self.assertEqual(r["day_of_month"], "every day-of-month")
        self.assertEqual(r["month"], "every month")
        self.assertEqual(r["day_of_week"], "every day-of-week")
        self.assertEqual(
            r["summary"],
            "every 5 minutes, every hour, every day-of-month, every month, every day-of-week",
        )

    def test_single_values(self):
        # "0 0 1 1 *" -> midnight on the first of January, any weekday.
        r = compute({"cron": "0 0 1 1 *"})
        self.assertTrue(r["valid"])
        self.assertEqual(r["minute"], "at minute 0")
        self.assertEqual(r["hour"], "at hour 0")
        self.assertEqual(r["day_of_month"], "on day-of-month 1")
        self.assertEqual(r["month"], "on month January")
        self.assertEqual(r["day_of_week"], "every day-of-week")

    def test_ranges_lists_and_names(self):
        # Business hours: minute 30, hours 9-17, Mon-Fri.
        r = compute({"cron": "30 9-17 * * 1-5"})
        self.assertTrue(r["valid"])
        self.assertEqual(r["minute"], "at minute 30")
        self.assertEqual(r["hour"], "every hour from 9 through 17")
        self.assertEqual(r["day_of_week"], "every day-of-week from Monday through Friday")
        # Comma list of values.
        r2 = compute({"cron": "0,15,30,45 * * * *"})
        self.assertTrue(r2["valid"])
        self.assertEqual(
            r2["minute"], "at minute 0; at minute 15; at minute 30; at minute 45"
        )
        # Named month/dow abbreviations resolve.
        r3 = compute({"cron": "0 12 * jan mon"})
        self.assertTrue(r3["valid"])
        self.assertEqual(r3["month"], "on month January")
        self.assertEqual(r3["day_of_week"], "on day-of-week Monday")

    def test_day_of_week_seven_is_sunday(self):
        # Standard cron allows day-of-week 0-7 with both 0 and 7 = Sunday.
        r = compute({"cron": "0 0 * * 7"})
        self.assertTrue(r["valid"], r)
        self.assertEqual(r["day_of_week"], "on day-of-week Sunday")
        # A range through 7 (Fri-Sun) is valid too.
        r2 = compute({"cron": "0 0 * * 5-7"})
        self.assertTrue(r2["valid"], r2)
        self.assertEqual(
            r2["day_of_week"], "every day-of-week from Friday through Sunday"
        )

    def test_step_variants(self):
        # Stepped range and stepped single-start.
        r = compute({"cron": "0 0-23/2 * * *"})
        self.assertTrue(r["valid"])
        self.assertEqual(r["hour"], "every 2 hours from 0 through 23")
        r2 = compute({"cron": "5/10 * * * *"})
        self.assertTrue(r2["valid"])
        self.assertEqual(r2["minute"], "every 10 minutes starting at minute 5")

    def test_invalid_field_count(self):
        r = compute({"cron": "* * * *"})
        self.assertFalse(r["valid"])
        self.assertIn("5", r["error"])
        r2 = compute({"cron": "* * * * * *"})
        self.assertFalse(r2["valid"])

    def test_out_of_range_and_bad_syntax(self):
        # Minute 60 is out of range (0-59).
        r = compute({"cron": "60 * * * *"})
        self.assertFalse(r["valid"])
        self.assertIn("minute", r["error"])
        # Non-numeric where a number is required.
        r2 = compute({"cron": "abc * * * *"})
        self.assertFalse(r2["valid"])
        # Reversed range.
        r3 = compute({"cron": "* 17-9 * * *"})
        self.assertFalse(r3["valid"])
        # Zero step is invalid.
        r4 = compute({"cron": "*/0 * * * *"})
        self.assertFalse(r4["valid"])

    def test_hostile_and_empty_inputs_do_not_raise(self):
        for bad in [None, {}, {"cron": 123}, {"cron": None}, {"cron": ["x"]},
                    [], "str", 42, {"cron": ""}, {"cron": "   "}]:
            r = compute(bad)
            self.assertIsInstance(r, dict)
            self.assertFalse(r["valid"])
            self.assertIn("error", r)


if __name__ == "__main__":
    unittest.main()


# --- input-frame bounding (defense in depth) ---------------------------------
# main()'s socket read loop routes every recv() chunk through main.drain_lines,
# which DROPS a partial frame once it passes MAX_FRAME_BYTES with no newline, so a
# peer streaming bytes without a newline cannot grow the read buffer without bound
# (OOM). These assert that real helper — the daemon side is already bounded
# (apps.rs read_line_bounded / genproxy MAX_PROXY_LINE_BYTES).
import main as _frame_mod  # noqa: E402 — appended after the file's own imports/runner


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

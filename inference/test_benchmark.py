"""Unit tests for the PURE stat / report-shape helpers in benchmark.py.

These MUST run without loading any model — benchmark.py keeps every mlx/server
import inside its measurement functions, so importing the module here touches no
weights. The model runs are the device-gated part, exercised by actually running
`benchmark.py` on the target Mac (not under this test).

Run: .venv/bin/python inference/test_benchmark.py   (from the repo root)
"""
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import benchmark  # noqa: E402


class MedianTests(unittest.TestCase):
    def test_odd_count(self):
        self.assertEqual(benchmark.median([3, 1, 2]), 2)

    def test_even_count_averages_middle(self):
        self.assertEqual(benchmark.median([1, 2, 3, 4]), 2.5)

    def test_single(self):
        self.assertEqual(benchmark.median([42.0]), 42.0)

    def test_empty_raises(self):
        with self.assertRaises(ValueError):
            benchmark.median([])


class WarmDiscardTests(unittest.TestCase):
    def test_drops_leading_warmup(self):
        self.assertEqual(benchmark.warm_discard([9, 1, 2, 3], warmup=1), [1, 2, 3])

    def test_drops_multiple_warmups(self):
        self.assertEqual(benchmark.warm_discard([9, 8, 1, 2], warmup=2), [1, 2])

    def test_zero_warmup_keeps_all(self):
        self.assertEqual(benchmark.warm_discard([1, 2], warmup=0), [1, 2])

    def test_not_enough_runs_raises(self):
        with self.assertRaises(ValueError):
            benchmark.warm_discard([1], warmup=1)

    def test_negative_warmup_raises(self):
        with self.assertRaises(ValueError):
            benchmark.warm_discard([1, 2, 3], warmup=-1)

    def test_returns_copy_not_alias(self):
        src = [9, 1, 2]
        out = benchmark.warm_discard(src, warmup=1)
        out.append(999)
        self.assertEqual(src, [9, 1, 2])  # source untouched


class SummarizeMetricTests(unittest.TestCase):
    def test_discards_warmup_then_medians(self):
        # warm-up 100 is dropped; median of [10,20,30] == 20
        s = benchmark.summarize_metric([100, 10, 20, 30], warmup=1)
        self.assertEqual(s["median"], 20)
        self.assertEqual(s["min"], 10)
        self.assertEqual(s["max"], 30)
        self.assertEqual(s["n"], 3)
        self.assertEqual(s["warmup"], 1)
        self.assertEqual(s["runs"], [10, 20, 30])

    def test_none_entries_excluded(self):
        s = benchmark.summarize_metric([100, 10, None, 30], warmup=1)
        self.assertEqual(s["median"], 20)  # median of [10, 30]
        self.assertEqual(s["n"], 2)

    def test_all_none_gives_honest_empty(self):
        s = benchmark.summarize_metric([None, None, None], warmup=1)
        self.assertIsNone(s["median"])
        self.assertEqual(s["n"], 0)
        self.assertEqual(s["runs"], [None, None])


class SummarizeRunsTests(unittest.TestCase):
    def test_transposes_and_summarizes_each_key(self):
        runs = [
            {"a": 100, "b": 1.0},  # warm-up (dropped)
            {"a": 10, "b": 2.0},
            {"a": 20, "b": 4.0},
        ]
        out = benchmark.summarize_runs(runs, ["a", "b"], warmup=1)
        self.assertEqual(out["a"]["median"], 15)
        self.assertEqual(out["b"]["median"], 3.0)

    def test_missing_key_contributes_none(self):
        runs = [{"a": 1}, {"a": 2}, {}]  # last run missing 'a'
        out = benchmark.summarize_runs(runs, ["a"], warmup=1)
        # warm-up drops first; kept = [{a:2}, {}] -> [2, None] -> median 2
        self.assertEqual(out["a"]["median"], 2)
        self.assertEqual(out["a"]["n"], 1)


class ChipSlugTests(unittest.TestCase):
    def test_apple_m1_pro(self):
        self.assertEqual(benchmark.chip_slug("Apple M1 Pro"), "m1_pro")

    def test_apple_m4(self):
        self.assertEqual(benchmark.chip_slug("Apple M4"), "m4")

    def test_m2_max(self):
        self.assertEqual(benchmark.chip_slug("Apple M2 Max"), "m2_max")

    def test_empty_is_unknown(self):
        self.assertEqual(benchmark.chip_slug(""), "unknown")
        self.assertEqual(benchmark.chip_slug(None), "unknown")


class ReportShapeTests(unittest.TestCase):
    def _synthetic_report(self):
        return benchmark.build_report(
            environment={"chip": "Apple M1 Pro", "mlx": "0.31.2"},
            models={"llm": "x"},
            config={"runs": 5, "warmup": 1},
            results={"llm": {"representative": {}}},
            unavailable={"image_generation": "mflux not installed"},
            methodology=benchmark.METHODOLOGY,
        )

    def test_build_report_has_required_keys(self):
        report = self._synthetic_report()
        for key in benchmark.REQUIRED_TOP_KEYS:
            self.assertIn(key, report)
        self.assertEqual(report["schema"], benchmark.SCHEMA)

    def test_assert_report_shape_passes(self):
        self.assertTrue(benchmark.assert_report_shape(self._synthetic_report()))

    def test_assert_report_shape_rejects_missing_key(self):
        bad = self._synthetic_report()
        del bad["results"]
        with self.assertRaises(AssertionError):
            benchmark.assert_report_shape(bad)

    def test_report_is_json_serializable(self):
        import json

        json.dumps(self._synthetic_report())  # must not raise


if __name__ == "__main__":
    unittest.main()

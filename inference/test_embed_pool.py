"""Unit tests for the PURE embedding masked-mean-pool helper `_pool_normalize`
in server.py — the numerically-delicate core of the batched op=embed path.

Runs WITHOUT loading any model: importing server touches no weights, and the
helper takes synthetic hidden-state tensors + a `mx` module, so it exercises the
padding-mask / mean / L2-normalize / non-finite-scrub math directly. Requires
mlx (the real runtime); run under the project venv:

  Run: .venv/bin/python inference/test_embed_pool.py   (from the repo root)
"""
import math
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import mlx.core as mx  # noqa: E402
import server  # noqa: E402


def _cos(a, b):
    dot = sum(x * y for x, y in zip(a, b))
    na = math.sqrt(sum(x * x for x in a))
    nb = math.sqrt(sum(x * x for x in b))
    return dot / (na * nb + 1e-12)


class PoolNormalizeTests(unittest.TestCase):
    def test_masks_out_padding_so_pad_length_does_not_change_the_vector(self):
        """A row's vector must depend ONLY on its real tokens: padding it out to a
        longer T (with arbitrary pad hidden states) yields the same unit vector.
        This is the invariant that makes the batched forward == the unpadded run."""
        # Row with 2 real tokens: hidden states [1,0,0] and [0,2,0] -> mean [0.5,1,0].
        real = [[1.0, 0.0, 0.0], [0.0, 2.0, 0.0]]
        unpadded = mx.array([real])  # [1, 2, 3]
        v_unpadded = _pool_normalize_list(unpadded, [2])[0]

        # Same 2 real tokens + 3 pad rows of GARBAGE that must be ignored.
        padded = mx.array([real + [[9.0, 9.0, 9.0], [-5.0, 7.0, 3.0], [100.0, 0.0, 0.0]]])
        v_padded = _pool_normalize_list(padded, [2])[0]

        self.assertGreaterEqual(_cos(v_unpadded, v_padded), 0.999999)
        # And it equals the hand-computed unit vector of the mean [0.5, 1, 0].
        m = [0.5, 1.0, 0.0]
        n = math.sqrt(sum(x * x for x in m))
        expected = [x / n for x in m]
        self.assertGreaterEqual(_cos(v_padded, expected), 0.999999)

    def test_output_is_unit_norm(self):
        hidden = mx.array([[[3.0, 4.0, 0.0], [0.0, 0.0, 0.0]]])  # mean [1.5,2,0]
        v = _pool_normalize_list(hidden, [2])[0]
        self.assertAlmostEqual(math.sqrt(sum(x * x for x in v)), 1.0, places=5)

    def test_rows_are_independent_across_a_batch(self):
        """Pooling row i must not depend on row j: two rows of different lengths
        pooled together equal each pooled alone."""
        rowA = [[1.0, 0.0], [3.0, 0.0], [0.0, 0.0]]  # len 2 real -> mean [2,0]
        rowB = [[0.0, 4.0], [0.0, 0.0], [0.0, 0.0]]  # len 1 real -> [0,4]
        batch = _pool_normalize_list(mx.array([rowA, rowB]), [2, 1])
        soloA = _pool_normalize_list(mx.array([rowA]), [2])[0]
        soloB = _pool_normalize_list(mx.array([rowB]), [1])[0]
        self.assertGreaterEqual(_cos(batch[0], soloA), 0.999999)
        self.assertGreaterEqual(_cos(batch[1], soloB), 0.999999)

    def test_non_finite_components_scrub_to_zero_finite(self):
        """A NaN/Inf hidden component must not leak a bare NaN/Infinity token into
        the vector (serde_json rejects those, failing the whole batch)."""
        hidden = mx.array([[[float("nan"), 1.0], [float("inf"), 1.0]]])
        v = _pool_normalize_list(hidden, [2])[0]
        self.assertTrue(all(math.isfinite(x) for x in v))

    def test_zero_vector_does_not_divide_by_zero(self):
        """All-zero hidden states (zero norm) must not produce NaN — the 1e-12
        norm floor keeps it finite (a degenerate all-zero vector)."""
        hidden = mx.array([[[0.0, 0.0, 0.0], [0.0, 0.0, 0.0]]])
        v = _pool_normalize_list(hidden, [2])[0]
        self.assertTrue(all(math.isfinite(x) for x in v))


def _pool_normalize_list(hidden, lengths):
    """Test helper: run the real _pool_normalize and return Python lists."""
    out = server._pool_normalize(mx, hidden, lengths)
    mx.eval(out)
    return [[float(x) for x in row] for row in out.tolist()]


class PackEmbedChunksTests(unittest.TestCase):
    """The PURE order-preserving chunker that bounds each padded forward by BOTH
    a row cap and a padded-token budget (rows x chunk max length)."""

    def _check_invariants(self, lengths, ranges, row_cap, token_budget, waste=2):
        # Contiguous, ordered, complete cover of [0, len(lengths)).
        self.assertEqual(ranges[0][0], 0)
        self.assertEqual(ranges[-1][1], len(lengths))
        for (a, b), (c, _d) in zip(ranges, ranges[1:]):
            self.assertLess(a, b)
            self.assertEqual(b, c)
        # Every multi-row chunk respects ALL three caps (a single row is always
        # legal): rows, padded-token budget, and the amplification guard.
        for a, b in ranges:
            rows = b - a
            padded = rows * max(lengths[a:b])
            if rows > 1:
                self.assertLessEqual(rows, row_cap)
                self.assertLessEqual(padded, token_budget)
                self.assertLessEqual(padded, waste * sum(lengths[a:b]))

    def test_short_facts_pack_into_one_chunk(self):
        """A realistic MNEMOSYNE batch (short facts) stays ONE forward."""
        lengths = [14] * 8
        ranges = server._pack_embed_chunks(lengths, row_cap=32, token_budget=4096)
        self.assertEqual(ranges, [(0, 8)])
        self._check_invariants(lengths, ranges, 32, 4096)

    def test_hostile_max_length_batch_degrades_to_small_chunks(self):
        """256 max-length texts must NOT form 16k-token forwards: with a 4096
        budget each chunk holds at most 8 rows of 512."""
        lengths = [512] * 256
        ranges = server._pack_embed_chunks(lengths, row_cap=32, token_budget=4096)
        self._check_invariants(lengths, ranges, 32, 4096)
        self.assertTrue(all((b - a) * 512 <= 4096 for a, b in ranges))
        self.assertEqual(sum(b - a for a, b in ranges), 256)

    def test_row_cap_still_applies_to_tiny_texts(self):
        lengths = [1] * 100
        ranges = server._pack_embed_chunks(lengths, row_cap=32, token_budget=4096)
        self._check_invariants(lengths, ranges, 32, 4096)
        self.assertTrue(all(b - a <= 32 for a, b in ranges))

    def test_one_long_text_closes_its_own_chunk(self):
        """A long text mid-batch must not inflate its neighbours' padding past
        the budget: the packer accounts for the max-length jump."""
        lengths = [10, 10, 512, 10, 10]
        ranges = server._pack_embed_chunks(lengths, row_cap=32, token_budget=1024)
        self._check_invariants(lengths, ranges, 32, 1024)

    def test_single_overlong_row_is_still_a_legal_chunk(self):
        """One row above the budget on its own cannot be split — it forms a
        chunk of one (the old per-text cost), never an infinite loop."""
        ranges = server._pack_embed_chunks([5000], row_cap=32, token_budget=1024)
        self.assertEqual(ranges, [(0, 1)])

    def test_empty_input_gives_no_ranges(self):
        self.assertEqual(server._pack_embed_chunks([], 32, 1024), [])

    def test_waste_guard_defeats_the_review_pattern_even_unsorted(self):
        """The review-confirmed amplification pattern fed to the RAW (unsorted)
        packer: 31 one-token texts then a 128-token text used to pack as one
        32x128 chunk (4096 padded for 159 real). The waste guard must now close
        the chunk before the 128 joins, so padded stays within 2x real."""
        lengths = [1] * 31 + [128]
        ranges = server._pack_embed_chunks(lengths, row_cap=32, token_budget=4096)
        self._check_invariants(lengths, ranges, 32, 4096)
        padded = sum((b - a) * max(lengths[a:b]) for a, b in ranges)
        self.assertLessEqual(padded, 2 * sum(lengths))

    def test_waste_guard_fuzz_never_amplifies_past_factor(self):
        """Deterministic fuzz over adversarial length mixes: for EVERY multi-row
        chunk the packer emits, padded <= 2x real (single-row chunks are exact
        by definition). Patterns chosen to attack the guard's boundaries."""
        patterns = [
            [1, 512] * 64,
            [512, 1] * 64,
            ([128] + [1] * 31) * 8,
            list(range(1, 257)),
            list(range(256, 0, -1)),
            [1, 1, 511, 1, 1, 511] * 20,
            [7] * 200,
            [512] * 40 + [1] * 40,
        ]
        for lengths in patterns:
            ranges = server._pack_embed_chunks(lengths, row_cap=32, token_budget=4096)
            self._check_invariants(lengths, ranges, 32, 4096)


class PlanEmbedBatchesTests(unittest.TestCase):
    """The LENGTH-SORTED planner that kills padded-token amplification while
    preserving output order via original-index groups."""

    def _padded_total(self, lengths, plan):
        return sum(len(g) * max(lengths[i] for i in g) for g in plan)

    def _check_cover(self, lengths, plan, row_cap, token_budget):
        # Every original index appears EXACTLY once across the plan.
        flat = [i for g in plan for i in g]
        self.assertEqual(sorted(flat), list(range(len(lengths))))
        # Both caps hold for every multi-row chunk.
        for g in plan:
            if len(g) > 1:
                self.assertLessEqual(len(g), row_cap)
                self.assertLessEqual(len(g) * max(lengths[i] for i in g), token_budget)

    def test_review_confirmed_amplification_pattern_collapses(self):
        """The exact upheld-finding pattern: ([128] + [1]*31) x 8 packed 32768
        padded tokens for 1272 real under the unsorted packer (25.8x). Sorted
        planning must bring the waste down to a small constant factor."""
        lengths = ([128] + [1] * 31) * 8
        plan = server._plan_embed_batches(lengths, row_cap=32, token_budget=4096)
        self._check_cover(lengths, plan, 32, 4096)
        real = sum(lengths)  # 1272
        padded = self._padded_total(lengths, plan)
        self.assertLessEqual(
            padded, 2 * real,
            f"sorted plan should not amplify: padded={padded} real={real}",
        )

    def test_interleaved_extremes_collapse_too(self):
        lengths = [1, 512] * 64
        plan = server._plan_embed_batches(lengths, row_cap=32, token_budget=4096)
        self._check_cover(lengths, plan, 32, 4096)
        real = sum(lengths)
        self.assertLessEqual(self._padded_total(lengths, plan), 2 * real)

    def test_uniform_batch_is_one_chunk_and_order_is_identity_coverage(self):
        lengths = [14] * 8
        plan = server._plan_embed_batches(lengths, row_cap=32, token_budget=4096)
        self.assertEqual(len(plan), 1)
        self.assertEqual(sorted(plan[0]), list(range(8)))

    def test_empty_input_gives_empty_plan(self):
        self.assertEqual(server._plan_embed_batches([], 32, 4096), [])

    def test_plan_is_deterministic(self):
        lengths = [5, 3, 9, 3, 7, 1, 512, 2]
        a = server._plan_embed_batches(lengths, 4, 64)
        b = server._plan_embed_batches(lengths, 4, 64)
        self.assertEqual(a, b)
        self._check_cover(lengths, a, 4, 64)


if __name__ == "__main__":
    unittest.main(verbosity=2)

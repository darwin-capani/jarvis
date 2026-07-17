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


if __name__ == "__main__":
    unittest.main(verbosity=2)

"""Unit tests for the PURE seams of the Core ML op=rerank backend (stage two of
the two-stage retrieval stack).

Runs WITHOUT loading any model and WITHOUT coremltools / torch / transformers:
`import coreml_rerank` is import-light (stdlib + numpy) and the tested helpers take
plain Python / numpy inputs, so this exercises the pair padding / truncation to SEQ,
the score post-processing (scrub), the empty-input guard, the Spearman faithfulness
helper, the reranker-id contract, and the server's rerank id / honest-fallback logic
directly. The live Core ML predict + FP16-vs-torch faithfulness + latency are
DEVICE/DEP-gated and exercised by the once-run smokes
(`.venv/bin/python inference/coreml_rerank.py` and the committed
inference/benchmarks/coreml_rerank_eval/ probe), NOT here.

  Run: .venv/bin/python inference/test_coreml_rerank.py   (from the repo root)
"""
import math
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import coreml_rerank as cr  # noqa: E402
import server  # noqa: E402


class PadPairTests(unittest.TestCase):
    def test_shape_padding_mask_and_types(self):
        ids, types, mask = cr.pad_pair([5, 6, 7], [0, 0, 1], seq=6)
        for arr in (ids, types, mask):
            self.assertEqual(arr.shape, (1, 6))
            self.assertEqual(arr.dtype.name, "int32")
        # 3 real tokens then 0-padding; mask 1 on the reals only.
        self.assertEqual(list(ids[0]), [5, 6, 7, 0, 0, 0])
        self.assertEqual(list(types[0]), [0, 0, 1, 0, 0, 0])  # segment ids preserved
        self.assertEqual(list(mask[0]), [1, 1, 1, 0, 0, 0])

    def test_truncates_to_seq(self):
        n = cr.SEQ + 50
        ids, types, mask = cr.pad_pair(list(range(1, n)), [1] * (n - 1), seq=cr.SEQ)
        self.assertEqual(ids.shape, (1, cr.SEQ))
        self.assertEqual(list(ids[0]), list(range(1, cr.SEQ + 1)))
        self.assertTrue(all(m == 1 for m in mask[0]))

    def test_empty_row_is_all_padding(self):
        ids, types, mask = cr.pad_pair([], [], seq=4)
        self.assertEqual(list(ids[0]), [0, 0, 0, 0])
        self.assertEqual(list(types[0]), [0, 0, 0, 0])
        self.assertEqual(list(mask[0]), [0, 0, 0, 0])


class ScrubScoreTests(unittest.TestCase):
    def test_finite_preserved(self):
        self.assertEqual(cr.scrub_score(6.25), 6.25)
        self.assertEqual(cr.scrub_score(-3.0), -3.0)

    def test_nan_and_neg_inf_sink_to_bottom(self):
        # A degenerate score must be FINITE and sort BELOW any real logit.
        self.assertTrue(math.isfinite(cr.scrub_score(float("nan"))))
        self.assertTrue(math.isfinite(cr.scrub_score(float("-inf"))))
        self.assertLess(cr.scrub_score(float("nan")), -1e29)
        self.assertLess(cr.scrub_score(float("-inf")), -1e29)

    def test_pos_inf_floats_to_top(self):
        self.assertTrue(math.isfinite(cr.scrub_score(float("inf"))))
        self.assertGreater(cr.scrub_score(float("inf")), 1e29)


class NormalizeTextTests(unittest.TestCase):
    def test_empty_and_whitespace_become_space(self):
        self.assertEqual(cr.normalize_text(""), " ")
        self.assertEqual(cr.normalize_text("   "), " ")
        self.assertEqual(cr.normalize_text("\n\t"), " ")
        self.assertEqual(cr.normalize_text(None), " ")

    def test_real_text_preserved(self):
        self.assertEqual(cr.normalize_text("dark roast"), "dark roast")


class SpearmanTests(unittest.TestCase):
    def test_identity_is_one(self):
        a = [3.0, 1.0, 2.0, 5.0]
        self.assertAlmostEqual(cr._spearman(a, a), 1.0, places=6)

    def test_reverse_is_minus_one(self):
        a = [1.0, 2.0, 3.0, 4.0]
        self.assertAlmostEqual(cr._spearman(a, a[::-1]), -1.0, places=6)

    def test_monotonic_transform_preserves_rank(self):
        # sigmoid is monotonic, so ranking by raw logit == ranking by probability.
        a = [-2.0, 0.5, 3.0, 1.0]
        b = [1 / (1 + math.exp(-x)) for x in a]
        self.assertAlmostEqual(cr._spearman(a, b), 1.0, places=6)


class RerankerContractTests(unittest.TestCase):
    """The reranker id contract + the server default. Guard drift."""

    def test_contract_ids(self):
        self.assertEqual(server.RERANKER_COREML, "coreml-ms-marco-minilm-l6-v2")
        self.assertEqual(cr.RERANKER_ID, server.RERANKER_COREML)
        self.assertTrue(server.DEFAULT_RERANKER_ENABLED)  # ships ON — measured winner
        # The server cap mirrors the module cap.
        self.assertEqual(server.RERANK_MAX_PASSAGES, cr.MAX_PASSAGES)


@unittest.skipUnless(
    sys.version_info >= (3, 11), "tomllib (py3.11+) required; runtime is 3.11"
)
class ConfigDefaultTests(unittest.TestCase):
    def test_shipped_config_defaults_reranker_on(self):
        settings = server.load_config()
        self.assertTrue(settings["reranker"])

    def test_non_boolean_reranker_keeps_default(self):
        # Parsed like preload/speculative: a non-boolean value keeps the default.
        eng_settings = server.load_config()
        self.assertIsInstance(eng_settings["reranker"], bool)


class RerankFallbackTests(unittest.TestCase):
    """The server's op=rerank id + HONEST-fallback logic (no model load)."""

    def _engine(self):
        return server.InferenceEngine(
            server.load_config(), "classify {utterance}", "persona"
        )

    def test_unavailable_falls_back_to_order_preserving_dense(self):
        eng = self._engine()
        eng._coreml_rerank_unavailable = True  # force the fallback branch
        scores, rid, fell_back = eng.rerank_with_meta("q", ["a", "b", "c"])
        self.assertTrue(fell_back)
        self.assertEqual(rid, "")  # no model produced an order
        # Order-preserving (strictly descending) so a re-sort is the identity.
        self.assertEqual(scores, sorted(scores, reverse=True))
        self.assertEqual(len(scores), 3)

    def test_empty_passages_is_not_a_fallback(self):
        eng = self._engine()
        scores, rid, fell_back = eng.rerank_with_meta("q", [])
        self.assertEqual(scores, [])
        self.assertEqual(rid, server.RERANKER_COREML)
        self.assertFalse(fell_back)

    def test_batch_cap_enforced(self):
        eng = self._engine()
        with self.assertRaises(ValueError):
            eng.rerank_with_meta("q", ["x"] * (server.RERANK_MAX_PASSAGES + 1))

    def test_type_validation(self):
        eng = self._engine()
        with self.assertRaises(ValueError):
            eng.rerank_with_meta(123, ["x"])
        with self.assertRaises(ValueError):
            eng.rerank_with_meta("q", "not a list")
        with self.assertRaises(ValueError):
            eng.rerank_with_meta("q", ["ok", 5])


class TruncationSurfacingTests(unittest.TestCase):
    """SEQ-cap truncation must NOT be silent — `_encode` emits a throttled warn
    naming how many pairs were capped. Pure: stub tokenizer (no model)."""

    def _reranker_with(self, id_rows, type_rows):
        e = cr.CoreMLReranker.__new__(cr.CoreMLReranker)
        e._trunc_seen = 0
        e._trunc_warned_at = 0
        e._tokenizer = lambda qs, ps, **kw: {
            "input_ids": id_rows, "token_type_ids": type_rows
        }
        return e

    def _capture(self):
        import logging
        msgs = []
        h = logging.Handler()
        h.emit = lambda r: msgs.append(r.getMessage())
        cr.log.addHandler(h)
        cr.log.setLevel(logging.WARNING)
        return msgs

    def test_truncated_pair_emits_a_warning(self):
        e = self._reranker_with([[1] * cr.SEQ, [1, 2, 3]], [[0] * cr.SEQ, [0, 0, 1]])
        msgs = self._capture()
        ids, types = e._encode("q", ["x" * 5000, "hi"])
        self.assertEqual([len(r) for r in ids], [cr.SEQ, 3])
        self.assertTrue(any("exceeded the" in m and "cap" in m for m in msgs))
        self.assertEqual(e._trunc_seen, 1)

    def test_no_truncation_is_silent(self):
        e = self._reranker_with([[1, 2, 3], [4, 5]], [[0, 0, 1], [0, 1]])
        msgs = self._capture()
        e._encode("q", ["hi", "yo"])
        self.assertFalse(any("cap" in m for m in msgs))
        self.assertEqual(e._trunc_seen, 0)


if __name__ == "__main__":
    unittest.main(verbosity=2)

"""Unit tests for the PURE seams of the Core ML op=embed backend.

Runs WITHOUT loading any model and WITHOUT coremltools / torch / transformers:
`import coreml_embed` is import-light (stdlib + numpy) and the tested helpers
take plain Python / numpy inputs, so this exercises the tokenization padding /
truncation to SEQ, the vector post-processing (scrub), the empty-input guard,
the model-derived mean-pool space id, and the embedder-id/dim selection +
fallback-decision logic directly. The live Core ML predict + FP16-vs-torch
faithfulness + latency are DEVICE/DEP-gated and exercised by the once-run smoke
(`.venv/bin/python inference/coreml_embed.py`), NOT here.

  Run: .venv/bin/python inference/test_coreml_embed.py   (from the repo root)
"""
import math
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import coreml_embed as ce  # noqa: E402
import server  # noqa: E402


class PadBatchTests(unittest.TestCase):
    def test_shape_padding_and_mask(self):
        ids, mask = ce.pad_batch([[5, 6, 7], [9]], seq=6)
        self.assertEqual(ids.shape, (2, 6))
        self.assertEqual(mask.shape, (2, 6))
        self.assertEqual(ids.dtype.name, "int32")
        self.assertEqual(mask.dtype.name, "int32")
        # Row 0: 3 real tokens then 0-padding; mask 1 on the reals only.
        self.assertEqual(list(ids[0]), [5, 6, 7, 0, 0, 0])
        self.assertEqual(list(mask[0]), [1, 1, 1, 0, 0, 0])
        self.assertEqual(list(ids[1]), [9, 0, 0, 0, 0, 0])
        self.assertEqual(list(mask[1]), [1, 0, 0, 0, 0, 0])

    def test_truncates_to_seq(self):
        # Input longer than SEQ -> truncated to the FIRST SEQ ids.
        ids, mask = ce.pad_batch([list(range(1, ce.SEQ + 100))], seq=ce.SEQ)
        self.assertEqual(ids.shape, (1, ce.SEQ))
        self.assertEqual(list(ids[0]), list(range(1, ce.SEQ + 1)))
        self.assertTrue(all(m == 1 for m in mask[0]))

    def test_empty_row_is_all_padding(self):
        ids, mask = ce.pad_batch([[]], seq=4)
        self.assertEqual(list(ids[0]), [0, 0, 0, 0])
        self.assertEqual(list(mask[0]), [0, 0, 0, 0])


class ScrubTests(unittest.TestCase):
    def test_non_finite_scrubbed_to_zero(self):
        out = ce.scrub_vector([1.0, float("nan"), float("inf"), float("-inf"), -2.5])
        self.assertEqual(out, [1.0, 0.0, 0.0, 0.0, -2.5])
        self.assertTrue(all(math.isfinite(x) for x in out))

    def test_finite_preserved(self):
        v = [0.1, -0.2, 0.3]
        self.assertEqual(ce.scrub_vector(v), v)


class NormalizeTextTests(unittest.TestCase):
    def test_empty_and_whitespace_become_space(self):
        self.assertEqual(ce.normalize_text(""), " ")
        self.assertEqual(ce.normalize_text("   "), " ")
        self.assertEqual(ce.normalize_text("\n\t"), " ")
        self.assertEqual(ce.normalize_text(None), " ")

    def test_real_text_preserved(self):
        self.assertEqual(ce.normalize_text("hello world"), "hello world")


class EmbedderSelectionTests(unittest.TestCase):
    """The embedder-id/dim selection + fallback-decision logic (server pure seams)."""

    def test_validate_known(self):
        self.assertEqual(
            server.validate_embedder("coreml-bge-small-en-v1.5"),
            server.EMBEDDER_COREML,
        )
        self.assertEqual(
            server.validate_embedder("llm-qwen3-4b-meanpool"),
            server.EMBEDDER_LLM_MEANPOOL,
        )

    def test_validate_unknown_raises(self):
        with self.assertRaises(ValueError):
            server.validate_embedder("nope")

    _MP = "llm-meanpool:some/model:int4"  # a model-derived mean-pool space id

    def test_plan_coreml_available(self):
        eid, dim, fell_back = server.plan_embedder(
            server.EMBEDDER_COREML, coreml_available=True, meanpool_id=self._MP
        )
        self.assertEqual(eid, server.EMBEDDER_COREML)
        self.assertEqual(dim, server.COREML_EMBED_DIM)
        self.assertEqual(dim, ce.DIM)  # server label matches the module's real dim
        self.assertFalse(fell_back)

    def test_plan_coreml_unavailable_falls_back_to_meanpool_id(self):
        eid, dim, fell_back = server.plan_embedder(
            server.EMBEDDER_COREML, coreml_available=False, meanpool_id=self._MP
        )
        # The fallback emits the MODEL-DERIVED id (not the fixed config value).
        self.assertEqual(eid, self._MP)
        self.assertIsNone(dim)  # mean-pool dim known only from a produced vector
        self.assertTrue(fell_back)

    def test_plan_meanpool_choice_is_not_a_fallback(self):
        eid, dim, fell_back = server.plan_embedder(
            server.EMBEDDER_LLM_MEANPOOL, coreml_available=True, meanpool_id=self._MP
        )
        self.assertEqual(eid, self._MP)
        self.assertIsNone(dim)
        self.assertFalse(fell_back)

    def test_contract_ids(self):
        # The fixed Core ML space id + the CONFIG-SELECTOR value. Guard drift.
        self.assertEqual(server.EMBEDDER_COREML, "coreml-bge-small-en-v1.5")
        self.assertEqual(server.EMBEDDER_LLM_MEANPOOL, "llm-qwen3-4b-meanpool")
        self.assertEqual(ce.EMBEDDER_ID, server.EMBEDDER_COREML)
        self.assertEqual(server.DEFAULT_EMBEDDER, server.EMBEDDER_COREML)

    def test_meanpool_space_id_is_model_derived(self):
        # The EMITTED mean-pool id must reflect the resident model (id + quant),
        # so an LLM/quant swap changes the vector-space stamp. Construct a real
        # engine (no model load) and check the format.
        settings = dict(server.load_config())
        settings["llm"] = "some/model-a-4bit"
        settings["quant"] = "int4"
        eng = server.InferenceEngine(settings, "classify {utterance}", "persona")
        self.assertEqual(
            eng._meanpool_space_id(), "llm-meanpool:some/model-a-4bit:int4"
        )
        # Swapping the model changes the emitted space id.
        settings2 = dict(settings)
        settings2["llm"] = "other/model-b-4bit"
        eng2 = server.InferenceEngine(settings2, "classify {utterance}", "persona")
        self.assertNotEqual(eng._meanpool_space_id(), eng2._meanpool_space_id())


@unittest.skipUnless(
    sys.version_info >= (3, 11), "tomllib (py3.11+) required; runtime is 3.11"
)
class ConfigDefaultTests(unittest.TestCase):
    def test_shipped_config_defaults_to_coreml(self):
        settings = server.load_config()
        self.assertEqual(settings["embedder"], server.EMBEDDER_COREML)


class TruncationSurfacingTests(unittest.TestCase):
    """SEQ-cap truncation must NOT be silent — `_encode` emits a throttled warn
    naming how many inputs were capped (review-caught: the docstring claimed it
    was surfaced but nothing signalled it at runtime). Pure: stub tokenizer."""

    def _engine_with(self, id_rows):
        import coreml_embed as ce
        e = ce.CoreMLEmbedder.__new__(ce.CoreMLEmbedder)
        e._trunc_seen = 0
        e._trunc_warned_at = 0
        e._tokenizer = lambda norm, **kw: {"input_ids": id_rows}
        return ce, e

    def _capture(self, ce):
        import logging
        msgs = []
        h = logging.Handler()
        h.emit = lambda r: msgs.append(r.getMessage())
        ce.log.addHandler(h)
        ce.log.setLevel(logging.WARNING)
        return msgs

    def test_truncated_input_emits_a_warning(self):
        ce, e = self._engine_with([[1] * ce_SEQ(), [1, 2, 3]])
        msgs = self._capture(ce)
        rows = e._encode(["x" * 5000, "hi"])
        self.assertEqual([len(r) for r in rows], [ce_SEQ(), 3])
        self.assertTrue(any("exceeded the" in m and "cap" in m for m in msgs))
        self.assertEqual(e._trunc_seen, 1)

    def test_no_truncation_is_silent(self):
        ce, e = self._engine_with([[1, 2, 3], [4, 5]])  # both under SEQ
        msgs = self._capture(ce)
        e._encode(["hi", "yo"])
        self.assertFalse(any("cap" in m for m in msgs))
        self.assertEqual(e._trunc_seen, 0)


def ce_SEQ():
    import coreml_embed as ce
    return ce.SEQ


if __name__ == "__main__":
    unittest.main(verbosity=2)

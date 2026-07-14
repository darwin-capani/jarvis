#!/usr/bin/env python3
"""Hermetic, NO-NETWORK, NO-MODEL tests for mid-request peer-disconnect
cancellation (audit fix).

Before the fix, a client that died mid-request left the server decoding (and
synthesizing) to completion under the GPU lock — a vanished daemon could burn
the GPU for the rest of max_tokens plus up to 5 TTS syntheses that nobody
would ever hear. The fix threads a cheap `cancelled` probe from the connection
(EOF on the reader / closing transport) into the two chunked decode loops:
op=converse (the streaming path) and op=generate's cached persona path.

These tests prove the WIRING + the abort semantics WITHOUT MLX or a network:

  * A fake `mlx_lm` module is installed in sys.modules (converse and
    _generate_cached import it lazily inside the function) whose
    stream_generate records exactly how many chunks the decode loop PULLED —
    which is precisely the GPU work a cancellation must stop.
  * The engine's model/TTS surfaces (_ensure_local_llm/_ensure_tts/
    _synthesize_to_wav/_persona_sampler) are instance-stubbed; no model loads.
  * The dispatch tests drive InferenceServer.dispatch with a fake engine and
    fake reader/writer to prove the probe is built from the REAL connection
    state (reader.at_eof() / transport.is_closing()) and that omitting the
    reader (unit-test callers) disables cancellation entirely.

Honesty: this proves the abort/wiring contract only. It does NOT measure the
GPU actually idling — that is device-gated and never exercised here.

Run: python3 inference/test_peer_cancel.py   (stdlib only; no pip install)
"""

import asyncio
import sys
import types
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import server  # noqa: E402


def _make_engine():
    """Construct an InferenceEngine without loading any model (all model loads
    are lazy)."""
    settings = {
        "llm": "stub-llm",
        "stt": "stub-stt",
        "engine": "kokoro",
        "voice": "bm_george",
        "speed": 1.2,
    }
    return server.InferenceEngine(settings, classifier_template="", persona="")


class _FakeTokenizer:
    """Templateless tokenizer: no chat_template attribute, so the engine's
    plain-text prompt fallback renders, and encode() is only used for lengths."""

    def encode(self, text):
        return list(range(len(text.split())))


class _FakeMlxLm:
    """Fake `mlx_lm` (+ `mlx_lm.models.cache`) for the lazy in-function imports.

    `pulled` counts the chunks the decode loop actually pulled from
    stream_generate — the abort assertions key off it staying put once the
    peer is gone."""

    def __init__(self, chunks):
        self.chunks = list(chunks)
        self.pulled = 0
        self.trim_calls = []
        self._saved = None

    def stream_generate(self, model, tokenizer, prompt, max_tokens,
                        prompt_cache=None, sampler=None):
        for text in self.chunks:
            self.pulled += 1
            yield types.SimpleNamespace(text=text)

    def install(self):
        fake = types.ModuleType("mlx_lm")
        fake.stream_generate = self.stream_generate
        fake.generate = lambda *a, **k: ""
        models = types.ModuleType("mlx_lm.models")
        cache_mod = types.ModuleType("mlx_lm.models.cache")
        cache_mod.trim_prompt_cache = lambda cache, n: self.trim_calls.append(n)
        cache_mod.make_prompt_cache = lambda model: []
        models.cache = cache_mod
        fake.models = models
        names = ("mlx_lm", "mlx_lm.models", "mlx_lm.models.cache")
        self._saved = {name: sys.modules.get(name) for name in names}
        sys.modules["mlx_lm"] = fake
        sys.modules["mlx_lm.models"] = models
        sys.modules["mlx_lm.models.cache"] = cache_mod

    def restore(self):
        for name, mod in (self._saved or {}).items():
            if mod is None:
                sys.modules.pop(name, None)
            else:
                sys.modules[name] = mod


def _stub_converse_engine(fake):
    """Engine with every model/TTS surface stubbed for a converse run.
    Returns (engine, synth_calls) where synth_calls records synthesized
    sentence texts."""
    engine = _make_engine()
    engine._ensure_local_llm = lambda local_model: ("stub-llm", object(), _FakeTokenizer())
    engine._ensure_tts = lambda: object()
    engine._persona_sampler = lambda: None  # only handed to the fake stream_generate
    synth_calls = []

    def fake_synth(tts, text, voice, out_path=None, rate=None, volume=None):
        synth_calls.append(text)
        return "/tmp/converse-stub.wav"

    engine._synthesize_to_wav = fake_synth
    return engine, synth_calls


class ConversePeerCancellation(unittest.TestCase):
    def test_decode_aborts_and_tail_never_synthesized(self):
        # 2 real sentences, then 50 more chunks a dead peer must never pay for.
        chunks = ["One. ", "Two. "] + ["tok "] * 50
        fake = _FakeMlxLm(chunks)
        fake.install()
        try:
            engine, synth_calls = _stub_converse_engine(fake)
            events = []
            result = engine.converse(
                "hi", 200, None, None, None, None, events.append, None, None, None,
                cancelled=lambda: fake.pulled >= 3,  # peer vanishes at chunk 3
            )
            # The loop pulled chunk 3, saw the peer gone, and stopped: the other
            # 49 chunks were never decoded.
            self.assertEqual(fake.pulled, 3, "decode must stop at the cancel poll")
            # Only pre-cancel text survives (the chunk in flight at cancel time
            # is discarded — nobody is listening).
            self.assertEqual(result["text"], "One. Two.")
            # Nothing from the tail was synthesized or emitted.
            for text in synth_calls:
                self.assertNotIn("tok", text)
            self.assertLessEqual(len(events), len(synth_calls))
            for event in events:
                self.assertNotIn("tok", event["text"])
        finally:
            fake.restore()

    def test_live_peer_probe_never_aborts(self):
        chunks = ["Hello. ", "World"]
        fake = _FakeMlxLm(chunks)
        fake.install()
        try:
            engine, synth_calls = _stub_converse_engine(fake)
            events = []
            result = engine.converse(
                "hi", 200, None, None, None, None, events.append, None, None, None,
                cancelled=lambda: False,  # live peer: the probe stays False
            )
            self.assertEqual(fake.pulled, len(chunks), "every chunk must decode")
            self.assertEqual(result["text"], "Hello. World")
            # The tail flush still runs for a live peer.
            self.assertEqual(result["sentences"], len(synth_calls))
            self.assertEqual(len(events), len(synth_calls))
        finally:
            fake.restore()

    def test_default_none_probe_is_byte_for_byte_uncancellable(self):
        chunks = ["Hello. ", "World"]
        fake = _FakeMlxLm(chunks)
        fake.install()
        try:
            engine, synth_calls = _stub_converse_engine(fake)
            result = engine.converse(
                "hi", 200, None, None, None, None, (lambda e: None), None, None, None,
            )
            self.assertEqual(fake.pulled, len(chunks))
            self.assertEqual(result["text"], "Hello. World")
        finally:
            fake.restore()


class CachedGeneratePeerCancellation(unittest.TestCase):
    def _cached_engine(self):
        engine = _make_engine()
        engine._tokenizer = _FakeTokenizer()
        engine._persona_sampler = lambda: None
        # A minimal resident persona cache: offset mirrors "nothing grew" so
        # the finally trim (added = offset - cache_len) is a no-op here.
        engine._gen_cache = [types.SimpleNamespace(offset=0)]
        engine._gen_cache_len = 0
        engine._gen_prefix_tokens = []
        return engine

    def test_cached_decode_aborts_after_cancel(self):
        fake = _FakeMlxLm(["a "] * 40)
        fake.install()
        try:
            engine = self._cached_engine()
            out = engine._generate_cached(
                "some prompt", 40, cancelled=lambda: fake.pulled >= 5
            )
            self.assertEqual(fake.pulled, 5, "decode must stop at the cancel poll")
            self.assertEqual(out, "a a a a ")  # 4 chunks kept; the 5th discarded
        finally:
            fake.restore()

    def test_cached_decode_runs_to_completion_without_probe(self):
        fake = _FakeMlxLm(["a "] * 8)
        fake.install()
        try:
            engine = self._cached_engine()
            out = engine._generate_cached("some prompt", 40)
            self.assertEqual(fake.pulled, 8)
            self.assertEqual(out, "a " * 8)
        finally:
            fake.restore()


# -- dispatch wiring: the probe is built from the REAL connection state ------


class _WiringEngine:
    """Fake engine that records the `cancelled` probe dispatch threads in."""

    UNSET = object()

    def __init__(self):
        self.converse_cancelled = self.UNSET
        self.generate_cancelled = self.UNSET

    def converse(self, text, max_tokens, history, facts, data, voice, emit,
                 opener_spoken, persona, local_model, cancelled=None):
        self.converse_cancelled = cancelled
        return {"text": "ok", "sentences": 0, "first_sentence_ms": None}

    def generate_with_meta(self, text, max_tokens, history, facts, data,
                           local_model, cancelled=None):
        self.generate_cancelled = cancelled
        return ("ok", {"speculative": False, "quant": "auto"})


class _FakeReader:
    def __init__(self, eof=False):
        self._eof = eof

    def at_eof(self):
        return self._eof


class _FakeWriter:
    def __init__(self, closing=False):
        self.transport = types.SimpleNamespace(is_closing=lambda: closing)

    def write(self, *_a, **_k):
        pass


def _dispatch(engine, req, reader):
    srv = server.InferenceServer(engine, preload=False)
    writer = _FakeWriter()
    if reader is None:
        return asyncio.run(srv.dispatch(req, writer))
    return asyncio.run(srv.dispatch(req, writer, reader))


class DispatchWiresThePeerProbe(unittest.TestCase):
    def test_converse_probe_fires_on_reader_eof(self):
        eng = _WiringEngine()
        resp = _dispatch(eng, {"id": "c1", "op": "converse", "text": "hi"}, _FakeReader(eof=True))
        self.assertTrue(resp["ok"])
        self.assertIsNot(eng.converse_cancelled, _WiringEngine.UNSET)
        self.assertTrue(callable(eng.converse_cancelled))
        self.assertTrue(eng.converse_cancelled(), "EOF on the reader means the peer is gone")

    def test_converse_probe_stays_false_for_live_peer(self):
        eng = _WiringEngine()
        _dispatch(eng, {"id": "c2", "op": "converse", "text": "hi"}, _FakeReader(eof=False))
        self.assertTrue(callable(eng.converse_cancelled))
        self.assertFalse(eng.converse_cancelled(), "a live peer must never read as gone")

    def test_no_reader_means_no_cancellation(self):
        # Unit-test / embedded callers that dispatch without a reader keep the
        # pre-fix run-to-completion behavior.
        eng = _WiringEngine()
        _dispatch(eng, {"id": "c3", "op": "converse", "text": "hi"}, None)
        self.assertIsNone(eng.converse_cancelled)

    def test_generate_probe_fires_on_reader_eof(self):
        eng = _WiringEngine()
        resp = _dispatch(eng, {"id": "g1", "op": "generate", "text": "hi"}, _FakeReader(eof=True))
        self.assertTrue(resp["ok"])
        self.assertTrue(callable(eng.generate_cancelled))
        self.assertTrue(eng.generate_cancelled())

    def test_closing_transport_also_reads_as_gone(self):
        eng = _WiringEngine()
        srv = server.InferenceServer(eng, preload=False)
        writer = _FakeWriter(closing=True)
        asyncio.run(srv.dispatch(
            {"id": "c4", "op": "converse", "text": "hi"}, writer, _FakeReader(eof=False)
        ))
        self.assertTrue(callable(eng.converse_cancelled))
        self.assertTrue(eng.converse_cancelled(), "a closing transport means the peer is gone")


if __name__ == "__main__":
    unittest.main(verbosity=2)

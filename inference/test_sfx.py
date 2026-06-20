#!/usr/bin/env python3
"""Hermetic, NO-NETWORK unit tests for the ElevenLabs SOUND-EFFECTS tier
(op=sound_effect) in the inference server.

The network seam (`_elevenlabs_sfx`) is MONKEYPATCHED in every engine test — it
is the ONLY place SFX touches ElevenLabs — so there is NO real HTTP here. These
prove the payload shaping, the seam's key/prompt guards, and the engine's honest
"no cue on failure" contract (there is NO on-device SFX fallback to substitute).

Run: python3 inference/test_sfx.py   (stdlib + numpy only; no pip install)
"""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import server  # noqa: E402


def _make_engine():
    settings = {"llm": "stub-llm", "stt": "stub-stt", "engine": "kokoro",
                "voice": "bm_george", "speed": 1.2}
    return server.InferenceEngine(settings, classifier_template="", persona="")


def _canned_pcm():
    import numpy as np

    return np.zeros(2400, dtype="<i2").tobytes()


class SfxPayload(unittest.TestCase):
    def test_prompt_only(self):
        self.assertEqual(server._build_sfx_payload("a soft confirmation chime"),
                         {"text": "a soft confirmation chime"})

    def test_duration_clamped_into_el_window(self):
        self.assertEqual(server._build_sfx_payload("x", duration_s=3.0)["duration_seconds"], 3.0)
        self.assertEqual(server._build_sfx_payload("x", duration_s=0.1)["duration_seconds"],
                         server.ELEVENLABS_SFX_DURATION_MIN)
        self.assertEqual(server._build_sfx_payload("x", duration_s=99.0)["duration_seconds"],
                         server.ELEVENLABS_SFX_DURATION_MAX)

    def test_prompt_influence_clamped_0_1(self):
        self.assertEqual(server._build_sfx_payload("x", prompt_influence=0.5)["prompt_influence"], 0.5)
        self.assertEqual(server._build_sfx_payload("x", prompt_influence=5.0)["prompt_influence"], 1.0)

    def test_bad_optionals_omitted(self):
        b = server._build_sfx_payload("x", duration_s="nope", prompt_influence=float("nan"))
        self.assertNotIn("duration_seconds", b)
        self.assertNotIn("prompt_influence", b)


class SfxSeamGuards(unittest.TestCase):
    """The seam refuses keyless / promptless calls BEFORE any network touch."""

    def test_seam_requires_key(self):
        with self.assertRaises(ValueError):
            server._elevenlabs_sfx("chime", api_key="")

    def test_seam_requires_prompt(self):
        with self.assertRaises(ValueError):
            server._elevenlabs_sfx("   ", api_key="sk-x")


class SfxEngine(unittest.TestCase):
    def _patch(self, pcm=None, raises=None):
        orig = server._elevenlabs_sfx

        def fake(prompt, api_key, duration_s=None, prompt_influence=None,
                 timeout_s=server.ELEVENLABS_TIMEOUT_S):
            if raises is not None:
                raise raises
            return pcm

        server._elevenlabs_sfx = fake
        return lambda: setattr(server, "_elevenlabs_sfx", orig)

    def test_success_returns_wav_path(self):
        engine = _make_engine()
        engine._write_wav = lambda audio, sr, out_path=None: "/tmp/sfx-stub.wav"
        restore = self._patch(pcm=_canned_pcm())
        try:
            self.assertEqual(engine.sound_effect("a soft chime", el_key="sk-x"), "/tmp/sfx-stub.wav")
        finally:
            restore()

    def test_failure_returns_none(self):
        engine = _make_engine()
        restore = self._patch(raises=RuntimeError("boom"))
        try:
            self.assertIsNone(engine.sound_effect("chime", el_key="sk-x"))
        finally:
            restore()

    def test_no_audio_returns_none(self):
        engine = _make_engine()
        restore = self._patch(pcm=b"")
        try:
            self.assertIsNone(engine.sound_effect("chime", el_key="sk-x"))
        finally:
            restore()

    def test_no_key_falls_to_none(self):
        # No key -> the REAL seam raises ValueError before any net -> engine returns None.
        engine = _make_engine()
        self.assertIsNone(engine.sound_effect("chime", el_key=""))


if __name__ == "__main__":
    unittest.main()

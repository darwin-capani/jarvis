#!/usr/bin/env python3
"""Hermetic, NO-NETWORK unit tests for the ElevenLabs AUDIO ISOLATION tier
(op=isolate_audio) in the inference server.

The network seam (`_elevenlabs_isolate_audio`) is MONKEYPATCHED in every engine
test — it is the ONLY place isolation touches ElevenLabs — so there is NO real
HTTP here. These prove the seam's key/file guards, and the engine's honest
"nothing produced on failure" contract (there is NO on-device isolator to
substitute, and the user's AUDIO leaves the device on the cloud leg).

Run: python3 inference/test_audio_isolation.py   (stdlib + numpy only; no pip install)
"""

import os
import sys
import tempfile
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


def _write_temp(data=b"RIFFstub"):
    fd, path = tempfile.mkstemp(suffix=".wav")
    with os.fdopen(fd, "wb") as f:
        f.write(data)
    return path


class IsolateSeamGuards(unittest.TestCase):
    """The seam refuses keyless / empty-file / missing-file calls BEFORE any net."""

    def test_seam_requires_key(self):
        # No key -> ValueError BEFORE any file read or network touch.
        with self.assertRaises(ValueError):
            server._elevenlabs_isolate_audio("/does/not/matter.wav", api_key="")

    def test_seam_rejects_empty_file(self):
        path = _write_temp(b"")
        try:
            with self.assertRaises(ValueError):
                server._elevenlabs_isolate_audio(path, api_key="sk-x")
        finally:
            os.unlink(path)

    def test_seam_raises_on_missing_file(self):
        # A missing path raises (OSError) before any network — caller catches it.
        with self.assertRaises(Exception):
            server._elevenlabs_isolate_audio("/no/such/file-xyz.wav", api_key="sk-x")


class IsolateEngine(unittest.TestCase):
    def _patch(self, pcm=None, raises=None):
        orig = server._elevenlabs_isolate_audio

        def fake(audio_path, api_key, timeout_s=server.ELEVENLABS_TIMEOUT_S):
            if raises is not None:
                raise raises
            return pcm

        server._elevenlabs_isolate_audio = fake
        return lambda: setattr(server, "_elevenlabs_isolate_audio", orig)

    def test_success_returns_wav_path(self):
        engine = _make_engine()
        engine._write_wav = lambda audio, sr, out_path=None: "/tmp/isolated-stub.wav"
        restore = self._patch(pcm=_canned_pcm())
        try:
            self.assertEqual(
                engine.isolate_audio("/tmp/in.wav", el_key="sk-x"),
                "/tmp/isolated-stub.wav",
            )
        finally:
            restore()

    def test_failure_returns_none(self):
        engine = _make_engine()
        restore = self._patch(raises=RuntimeError("boom"))
        try:
            self.assertIsNone(engine.isolate_audio("/tmp/in.wav", el_key="sk-x"))
        finally:
            restore()

    def test_no_audio_returns_none(self):
        engine = _make_engine()
        restore = self._patch(pcm=b"")
        try:
            self.assertIsNone(engine.isolate_audio("/tmp/in.wav", el_key="sk-x"))
        finally:
            restore()

    def test_no_key_falls_to_none(self):
        # No key -> the REAL seam raises ValueError before any net -> engine None.
        engine = _make_engine()
        self.assertIsNone(engine.isolate_audio("/tmp/in.wav", el_key=""))


if __name__ == "__main__":
    unittest.main()

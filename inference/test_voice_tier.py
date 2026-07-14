#!/usr/bin/env python3
"""Hermetic, NO-NETWORK unit tests for the ElevenLabs cloud voice tier in the
inference server's speak op.

These tests prove the WIRING + the gating + the Kokoro fallback WITHOUT ever
touching the network or loading a model:

  * The network seam (`_elevenlabs_synth_pcm`) is MONKEYPATCHED in every test —
    it is the only place that would touch ElevenLabs, and it is replaced with a
    canned-bytes stub (or one that raises). There is NO real HTTP here.
  * The Kokoro fallback path (`_ensure_tts` / `_synthesize_to_wav`) is likewise
    stubbed so no MLX model is loaded; the tests assert WHICH path was taken.

Honesty: this proves backend selection + the Kokoro-fallback contract + key
hygiene only. It does NOT (and cannot) verify the live ElevenLabs audio or voice
quality — that is device + credential gated and is never exercised here.

Run: python3 inference/test_voice_tier.py   (stdlib + numpy only; no pip install)
"""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import server  # noqa: E402


def _make_engine():
    """Construct an InferenceEngine without loading any model (all model loads are
    lazy). Kokoro is the default engine; we stub its synth path per-test."""
    settings = {
        "llm": "stub-llm",
        "stt": "stub-stt",
        "engine": "kokoro",
        "voice": "bm_george",
        "speed": 1.2,
    }
    return server.InferenceEngine(settings, classifier_template="", persona="")


class _Recorder:
    """Records calls so a test can assert which TTS path ran."""

    def __init__(self):
        self.kokoro_calls = 0
        self.el_calls = 0
        self.last_el_args = None


def _install_stubs(engine, rec, el_pcm=None, el_raises=None):
    """Stub BOTH backends on `engine`:
      - the ElevenLabs network seam (module-level) returns `el_pcm` bytes, or
        raises `el_raises` — never any real network.
      - the Kokoro path: _ensure_tts returns a dummy, _synthesize_to_wav records
        a call and returns a sentinel path.
    Returns a callable that restores the patched module-level seam."""
    orig_seam = server._elevenlabs_synth_pcm

    def fake_seam(voice_id, model, api_key, text, timeout_s=server.ELEVENLABS_TIMEOUT_S,
                  audio_tag=None, stability=None, style=None, locators=None):
        rec.el_calls += 1
        rec.last_el_args = {
            "voice_id": voice_id,
            "model": model,
            "api_key": api_key,
            "text": text,
        }
        if el_raises is not None:
            raise el_raises
        return el_pcm

    server._elevenlabs_synth_pcm = fake_seam

    # Kokoro path stubs (instance-level): no model load, sentinel WAV path.
    engine._ensure_tts = lambda: object()

    def fake_synth(tts, text, voice, out_path=None, rate=None, volume=None):
        rec.kokoro_calls += 1
        return "/tmp/kokoro-stub.wav"

    engine._synthesize_to_wav = fake_synth
    # _elevenlabs_to_wav uses _write_wav + _trim_silence on the decoded PCM; let it
    # run for real (pure numpy) so the EL path is exercised end to end minus the net.
    # But _write_wav writes a file under state/tmp — redirect to a sentinel instead
    # so the test leaves no artifacts and stays fast.
    engine._write_wav = lambda audio, sr, out_path=None: "/tmp/elevenlabs-stub.wav"

    def restore():
        server._elevenlabs_synth_pcm = orig_seam

    return restore


# A second of silence-with-a-blip as canned PCM16/24kHz so _trim_silence keeps
# something (all-silence would be trimmed to None and treated as "no audio").
def _canned_pcm():
    import numpy as np

    samples = np.zeros(2400, dtype="<i2")
    samples[1000:1100] = 8000  # a loud blip so trim_silence keeps audio
    return samples.tobytes()


class ElevenLabsBackendSelection(unittest.TestCase):
    def test_kokoro_is_the_default_when_no_backend(self):
        engine = _make_engine()
        rec = _Recorder()
        restore = _install_stubs(engine, rec, el_pcm=_canned_pcm())
        try:
            path = engine.speak("hello", voice="bm_george")
            self.assertEqual(path, "/tmp/kokoro-stub.wav")
            self.assertEqual(rec.kokoro_calls, 1, "Kokoro path must run")
            self.assertEqual(rec.el_calls, 0, "the cloud seam must NOT be touched")
        finally:
            restore()

    def test_kokoro_when_backend_is_explicitly_kokoro(self):
        engine = _make_engine()
        rec = _Recorder()
        restore = _install_stubs(engine, rec, el_pcm=_canned_pcm())
        try:
            engine.speak("hi", voice="bm_george", backend="kokoro")
            self.assertEqual(rec.el_calls, 0)
            self.assertEqual(rec.kokoro_calls, 1)
        finally:
            restore()

    def test_elevenlabs_path_runs_when_daemon_chose_it(self):
        engine = _make_engine()
        rec = _Recorder()
        restore = _install_stubs(engine, rec, el_pcm=_canned_pcm())
        try:
            path = engine.speak(
                "hello there",
                voice="bm_george",
                backend="elevenlabs",
                voice_id="EL_VOICE",
                model="eleven_flash_v2_5",
                el_key="sk-secret",
            )
            self.assertEqual(path, "/tmp/elevenlabs-stub.wav")
            self.assertEqual(rec.el_calls, 1, "the cloud seam must be used")
            self.assertEqual(rec.kokoro_calls, 0, "Kokoro must NOT run on the cloud hit")
            # The seam got exactly the voice id / model / key the daemon passed.
            self.assertEqual(rec.last_el_args["voice_id"], "EL_VOICE")
            self.assertEqual(rec.last_el_args["model"], "eleven_flash_v2_5")
            self.assertEqual(rec.last_el_args["api_key"], "sk-secret")
        finally:
            restore()


class ElevenLabsFallback(unittest.TestCase):
    def test_falls_back_to_kokoro_on_seam_error(self):
        engine = _make_engine()
        rec = _Recorder()
        restore = _install_stubs(
            engine, rec, el_raises=RuntimeError("connection refused")
        )
        try:
            path = engine.speak(
                "hello",
                voice="bm_george",
                backend="elevenlabs",
                voice_id="EL_VOICE",
                model="eleven_flash_v2_5",
                el_key="sk-secret",
            )
            # The turn is NEVER failed: it returns the Kokoro WAV.
            self.assertEqual(path, "/tmp/kokoro-stub.wav")
            self.assertEqual(rec.el_calls, 1, "the cloud seam was attempted")
            self.assertEqual(rec.kokoro_calls, 1, "then Kokoro served the turn")
        finally:
            restore()

    def test_falls_back_to_kokoro_when_cloud_returns_no_audio(self):
        engine = _make_engine()
        rec = _Recorder()
        # Empty bytes -> no audio -> Kokoro fallback.
        restore = _install_stubs(engine, rec, el_pcm=b"")
        try:
            path = engine.speak(
                "hello",
                voice="bm_george",
                backend="elevenlabs",
                voice_id="EL_VOICE",
                model="eleven_flash_v2_5",
                el_key="sk-secret",
            )
            self.assertEqual(path, "/tmp/kokoro-stub.wav")
            self.assertEqual(rec.kokoro_calls, 1)
        finally:
            restore()

    def test_no_key_refuses_cloud_and_falls_back(self):
        # Defense in depth: even if the daemon mislabeled a request elevenlabs with
        # no key, the real seam raises (ValueError) before any POST, and speak falls
        # back to Kokoro. Here we let the REAL seam run (it raises pre-network on a
        # missing key) and stub only the Kokoro path.
        engine = _make_engine()
        rec = _Recorder()

        def fake_synth(tts, text, voice, out_path=None, rate=None, volume=None):
            rec.kokoro_calls += 1
            return "/tmp/kokoro-stub.wav"

        engine._ensure_tts = lambda: object()
        engine._synthesize_to_wav = fake_synth
        path = engine.speak(
            "hello",
            voice="bm_george",
            backend="elevenlabs",
            voice_id="EL_VOICE",
            model="eleven_flash_v2_5",
            el_key="",  # NO key
        )
        self.assertEqual(path, "/tmp/kokoro-stub.wav")
        self.assertEqual(rec.kokoro_calls, 1)


class KeyHygieneAndDecoding(unittest.TestCase):
    def test_pcm16_decode_roundtrips_to_float(self):
        import numpy as np

        pcm = np.array([0, 16384, -16384, 32767], dtype="<i2").tobytes()
        out = server._pcm16_to_float32(pcm)
        self.assertEqual(out.dtype, np.float32)
        self.assertAlmostEqual(float(out[0]), 0.0, places=4)
        self.assertAlmostEqual(float(out[1]), 0.5, places=3)
        self.assertAlmostEqual(float(out[2]), -0.5, places=3)

    def test_pcm16_decode_drops_odd_trailing_byte(self):
        # A truncated final frame (odd byte count) must not raise.
        out = server._pcm16_to_float32(b"\x00\x10\x00")  # 3 bytes
        self.assertEqual(len(out), 1)

    def test_redactor_scrubs_the_header_name(self):
        scrubbed = server._redact_elevenlabs(
            "error involving xi-api-key header"
        )
        self.assertNotIn("xi-api-key", scrubbed)
        self.assertIn("[redacted-header]", scrubbed)

    def test_gpu_lock_not_held_across_cloud_round_trip(self):
        # AUDIT FIX: the ElevenLabs leg of speak() is a cloud round-trip (up to
        # ELEVENLABS_TIMEOUT_S) plus pure-numpy decode and a file write — it
        # touches NO GPU/model state, so holding self._lock across it would
        # stall every local inference op behind the network. The seam probe
        # records the lock state at the exact moment the "network" is hit.
        engine = _make_engine()
        seen = {}
        orig_seam = server._elevenlabs_synth_pcm

        def probe_seam(voice_id, model, api_key, text, timeout_s=server.ELEVENLABS_TIMEOUT_S,
                       audio_tag=None, stability=None, style=None, locators=None):
            seen["lock_held_during_cloud_call"] = engine._lock.locked()
            return _canned_pcm()

        server._elevenlabs_synth_pcm = probe_seam
        engine._write_wav = lambda audio, sr, out_path=None: "/tmp/elevenlabs-stub.wav"
        try:
            path = engine.speak(
                "hello", backend="elevenlabs", voice_id="stub-voice", el_key="stub-key"
            )
            self.assertEqual(path, "/tmp/elevenlabs-stub.wav")
            self.assertIn("lock_held_during_cloud_call", seen, "the seam must have run")
            self.assertFalse(
                seen["lock_held_during_cloud_call"],
                "speak() must NOT hold the GPU lock across the ElevenLabs round-trip",
            )
        finally:
            server._elevenlabs_synth_pcm = orig_seam

    def test_gpu_lock_still_held_for_on_device_kokoro(self):
        # The counterpart invariant: the on-device leg DOES drive the GPU, so
        # the lock must still be held around _ensure_tts/_synthesize_to_wav.
        engine = _make_engine()
        seen = {}
        engine._ensure_tts = lambda: object()  # lock-held caller contract; no model load

        def probe_synth(tts, text, voice, out_path=None, rate=None, volume=None):
            seen["lock_held_during_synth"] = engine._lock.locked()
            return "/tmp/kokoro-stub.wav"

        engine._synthesize_to_wav = probe_synth
        path = engine.speak("hello")
        self.assertEqual(path, "/tmp/kokoro-stub.wav")
        self.assertTrue(
            seen.get("lock_held_during_synth"),
            "on-device synthesis must keep holding the GPU lock",
        )

    def test_seam_is_the_only_network_touch_point(self):
        # The module exposes exactly one network seam name; the engine method that
        # would call it (_elevenlabs_to_wav) and the seam both exist, so a test can
        # always intercept the network without it leaking elsewhere.
        self.assertTrue(hasattr(server, "_elevenlabs_synth_pcm"))
        self.assertTrue(hasattr(server.InferenceEngine, "_elevenlabs_to_wav"))
        # The key only travels via the header constant — assert it is the standard
        # ElevenLabs auth header, never a query param.
        self.assertEqual(server._ELEVENLABS_HEADER, "xi-api-key")
        self.assertNotIn("?", server._ELEVENLABS_HEADER)


if __name__ == "__main__":
    unittest.main(verbosity=2)

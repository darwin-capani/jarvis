#!/usr/bin/env python3
"""Hermetic, NO-NETWORK unit tests for the ElevenLabs PRONUNCIATION-DICTIONARY
tier (op=create_pronunciation) AND the additive pronunciation_dictionary_locators
threading into the TTS payload, in the inference server.

The network seam (`_elevenlabs_create_pronunciation`) is MONKEYPATCHED in every
engine test — it is the ONLY place this capability touches ElevenLabs — so there
is NO real HTTP here. These prove:
  - the create-payload + seam key/name/rules guards (BEFORE any net touch);
  - the engine's honest "no dictionary on failure" contract (no on-device
    equivalent to substitute, never a fabricated id);
  - that `_build_elevenlabs_payload` is BYTE-FOR-BYTE unchanged with NO locators
    (so the existing speak/payload tests stay green) and ADDITIVELY carries
    `pronunciation_dictionary_locators` only when a non-empty list is passed.

Run: python3 inference/test_pronunciation.py   (stdlib + numpy only; no install)
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


_RULES = [
    {"string_to_replace": "DARWIN", "type": "alias", "alias": "Darwin"},
    {"string_to_replace": "tomato", "type": "phoneme", "phoneme": "təˈmɑːtoʊ", "alphabet": "ipa"},
]


class PronunciationPayload(unittest.TestCase):
    def test_name_and_rules_passed_through(self):
        body = server._build_pronunciation_payload("places", _RULES)
        self.assertEqual(body, {"name": "places", "rules": _RULES})

    def test_empty_name_raises(self):
        with self.assertRaises(ValueError):
            server._build_pronunciation_payload("  ", _RULES)

    def test_empty_rules_raises(self):
        with self.assertRaises(ValueError):
            server._build_pronunciation_payload("x", [])
        with self.assertRaises(ValueError):
            server._build_pronunciation_payload("x", "not-a-list")


class PronunciationSeamGuards(unittest.TestCase):
    """The seam refuses keyless / nameless / ruleless calls BEFORE any net touch."""

    def test_seam_requires_key(self):
        with self.assertRaises(ValueError):
            server._elevenlabs_create_pronunciation("x", _RULES, api_key="")

    def test_seam_requires_name(self):
        with self.assertRaises(ValueError):
            server._elevenlabs_create_pronunciation("  ", _RULES, api_key="sk-x")

    def test_seam_requires_rules(self):
        with self.assertRaises(ValueError):
            server._elevenlabs_create_pronunciation("x", [], api_key="sk-x")


class PronunciationSeamKeyGuard(unittest.TestCase):
    """The key rides ONLY the xi-api-key header — never the URL/query — and the seam
    is reached with exactly the (name, rules) it was given. We capture the urllib
    Request WITHOUT any network by stubbing urlopen."""

    def test_key_in_header_only_and_args_forwarded(self):
        import io
        import json as _json
        import urllib.request

        captured = {}

        class _Resp:
            def __enter__(self_):
                return io.BytesIO(_json.dumps({"id": "dict-1", "version_id": "ver-1"}).encode())

            def __exit__(self_, *a):
                return False

        def fake_urlopen(req, timeout=None):
            captured["url"] = req.full_url
            captured["headers"] = {k.lower(): v for k, v in req.header_items()}
            captured["body"] = req.data
            return _Resp()

        orig = urllib.request.urlopen
        urllib.request.urlopen = fake_urlopen
        try:
            did, vid = server._elevenlabs_create_pronunciation("places", _RULES, "sk-secret")
        finally:
            urllib.request.urlopen = orig

        self.assertEqual((did, vid), ("dict-1", "ver-1"))
        # Key ONLY in the xi-api-key header.
        self.assertEqual(captured["headers"].get(server._ELEVENLABS_HEADER.lower()), "sk-secret")
        # Key NEVER in the URL/query.
        self.assertNotIn("sk-secret", captured["url"])
        self.assertEqual(captured["url"], server.ELEVENLABS_PRONUNCIATION_URL)
        # Body carries exactly name + rules.
        body = _json.loads(captured["body"].decode("utf-8"))
        self.assertEqual(body, {"name": "places", "rules": _RULES})

    def test_missing_id_or_version_raises(self):
        import io
        import json as _json
        import urllib.request

        def _resp_for(payload):
            class _Resp:
                def __enter__(self_):
                    return io.BytesIO(_json.dumps(payload).encode())

                def __exit__(self_, *a):
                    return False
            return _Resp()

        for bad in ({"version_id": "v"}, {"id": "d"}, {"id": "", "version_id": "v"}):
            def fake_urlopen(req, timeout=None, _b=bad):
                return _resp_for(_b)

            orig = urllib.request.urlopen
            urllib.request.urlopen = fake_urlopen
            try:
                with self.assertRaises(ValueError):
                    server._elevenlabs_create_pronunciation("x", _RULES, "sk-x")
            finally:
                urllib.request.urlopen = orig


class PronunciationEngine(unittest.TestCase):
    def _patch(self, result=None, raises=None):
        orig = server._elevenlabs_create_pronunciation

        def fake(name, rules, api_key, timeout_s=server.ELEVENLABS_TIMEOUT_S):
            if raises is not None:
                raise raises
            return result

        server._elevenlabs_create_pronunciation = fake
        return lambda: setattr(server, "_elevenlabs_create_pronunciation", orig)

    def test_success_returns_pair(self):
        engine = _make_engine()
        restore = self._patch(result=("dict-1", "ver-1"))
        try:
            self.assertEqual(
                engine.create_pronunciation("places", _RULES, el_key="sk-x"),
                ("dict-1", "ver-1"),
            )
        finally:
            restore()

    def test_failure_returns_none(self):
        engine = _make_engine()
        restore = self._patch(raises=RuntimeError("boom"))
        try:
            self.assertIsNone(engine.create_pronunciation("places", _RULES, el_key="sk-x"))
        finally:
            restore()

    def test_empty_result_returns_none(self):
        engine = _make_engine()
        restore = self._patch(result=None)
        try:
            self.assertIsNone(engine.create_pronunciation("places", _RULES, el_key="sk-x"))
        finally:
            restore()

    def test_no_key_falls_to_none(self):
        # No key -> the REAL seam raises ValueError before any net -> engine None.
        engine = _make_engine()
        self.assertIsNone(engine.create_pronunciation("places", _RULES, el_key=""))


class PayloadLocatorsUnchangedWithoutThem(unittest.TestCase):
    """The CRITICAL byte-for-byte guarantee: with NO locators the TTS payload is
    EXACTLY today's (so existing _build_elevenlabs_payload + speak tests stay green)."""

    def test_neutral_payload_unchanged(self):
        # No locators arg at all (default None).
        self.assertEqual(
            server._build_elevenlabs_payload("VID", server.ELEVENLABS_V3_MODEL, "hello"),
            {"text": "hello", "model_id": server.ELEVENLABS_V3_MODEL},
        )

    def test_explicit_none_and_empty_unchanged(self):
        base = {"text": "hello", "model_id": "eleven_flash_v2_5"}
        self.assertEqual(
            server._build_elevenlabs_payload("VID", "eleven_flash_v2_5", "hello", locators=None),
            base,
        )
        self.assertEqual(
            server._build_elevenlabs_payload("VID", "eleven_flash_v2_5", "hello", locators=[]),
            base,
        )
        self.assertNotIn(
            "pronunciation_dictionary_locators",
            server._build_elevenlabs_payload("VID", "eleven_flash_v2_5", "hello", locators=None),
        )

    def test_invalid_locators_omitted(self):
        # A non-list / entries missing a dictionary id read as 'none' -> field omitted.
        for bad in ("nope", 5, [{}], [{"version_id": "v"}], [{"pronunciation_dictionary_id": "  "}]):
            body = server._build_elevenlabs_payload(
                "VID", "eleven_flash_v2_5", "hello", locators=bad
            )
            self.assertNotIn("pronunciation_dictionary_locators", body, bad)


class PayloadLocatorsAddedWithThem(unittest.TestCase):
    """With a non-empty valid list the field is added ADDITIVELY (every model)."""

    def test_locators_added(self):
        locs = [{"pronunciation_dictionary_id": "dict-1", "version_id": "ver-1"}]
        body = server._build_elevenlabs_payload(
            "VID", "eleven_flash_v2_5", "hello", locators=locs
        )
        self.assertEqual(
            body["pronunciation_dictionary_locators"],
            [{"pronunciation_dictionary_id": "dict-1", "version_id": "ver-1"}],
        )
        # The rest of the payload is unchanged (additive only).
        self.assertEqual(body["text"], "hello")
        self.assertEqual(body["model_id"], "eleven_flash_v2_5")

    def test_added_on_v3_too_alongside_prosody(self):
        locs = [{"pronunciation_dictionary_id": "dict-1"}]
        body = server._build_elevenlabs_payload(
            "VID", server.ELEVENLABS_V3_MODEL, "hello",
            audio_tag="[calm]", stability=0.4, locators=locs,
        )
        # Pronunciation is NOT v3-gated -> present; v3 prosody also present.
        self.assertEqual(
            body["pronunciation_dictionary_locators"],
            [{"pronunciation_dictionary_id": "dict-1"}],
        )
        self.assertEqual(body["text"], "[calm] hello")
        self.assertEqual(body["voice_settings"], {"stability": 0.4})

    def test_version_id_optional_and_clamped_to_max(self):
        # version_id omitted when absent/blank; list truncated to EL's cap.
        norm = server._normalize_pronunciation_locators([
            {"pronunciation_dictionary_id": "d1"},
            {"pronunciation_dictionary_id": "d2", "version_id": "  "},
            {"pronunciation_dictionary_id": "d3", "version_id": "v3"},
            {"pronunciation_dictionary_id": "d4"},
        ])
        self.assertEqual(len(norm), server.ELEVENLABS_PRONUNCIATION_MAX_LOCATORS)
        self.assertEqual(norm[0], {"pronunciation_dictionary_id": "d1"})
        self.assertNotIn("version_id", norm[1])  # blank version dropped
        self.assertEqual(norm[2], {"pronunciation_dictionary_id": "d3", "version_id": "v3"})


if __name__ == "__main__":
    unittest.main()

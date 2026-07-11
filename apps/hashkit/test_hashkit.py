#!/usr/bin/env python3
"""Tests for hashkit.compute: known-answer vectors, empty, and hostile input."""
import hashlib
import unittest

from main import compute


class TestHashkitDigest(unittest.TestCase):
    def test_known_abc(self):
        # RFC / NIST known-answer vectors for "abc".
        r = compute({"text": "abc"})
        self.assertEqual(r["length_bytes"], 3)
        self.assertEqual(r["md5"], "900150983cd24fb0d6963f7d28e17f72")
        self.assertEqual(r["sha1"], "a9993e364706816aba3e25717850c26c9cd0d89d")
        self.assertEqual(
            r["sha256"],
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        )

    def test_empty_string(self):
        # Digests of the empty byte string are well-known constants.
        r = compute({"text": ""})
        self.assertEqual(r["length_bytes"], 0)
        self.assertEqual(r["md5"], "d41d8cd98f00b204e9800998ecf8427e")
        self.assertEqual(r["sha1"], "da39a3ee5e6b4b0d3255bfef95601890afd80709")
        self.assertEqual(
            r["sha256"],
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        )

    def test_utf8_byte_length(self):
        # Multi-byte UTF-8: "é" is 2 bytes, "😀" is 4 bytes -> 6 bytes total.
        r = compute({"text": "é😀"})
        self.assertEqual(r["length_bytes"], 6)
        self.assertEqual(r["sha256"], hashlib.sha256("é😀".encode("utf-8")).hexdigest())

    def test_missing_text_defaults_empty(self):
        r = compute({})
        self.assertEqual(r["length_bytes"], 0)
        self.assertEqual(r["md5"], "d41d8cd98f00b204e9800998ecf8427e")

    def test_hostile_inputs_do_not_raise(self):
        # None payload, non-string text, wrong-typed payload, lone surrogate.
        for bad in [None, {"text": 123}, {"text": None}, {"text": ["x"]}, [], "str", 42]:
            r = compute(bad)
            self.assertEqual(r["length_bytes"], 0)
            self.assertEqual(r["md5"], "d41d8cd98f00b204e9800998ecf8427e")
        # Lone surrogate is not encodable as strict utf-8; must not raise.
        r = compute({"text": "\ud800"})
        self.assertIn("sha256", r)
        self.assertIsInstance(r["length_bytes"], int)


# --- input-frame bounding (defense in depth) ---------------------------------
# main()'s socket read loop routes every recv() chunk through main.drain_lines,
# which DROPS a partial frame once it passes MAX_FRAME_BYTES with no newline, so a
# peer streaming bytes without a newline cannot grow the read buffer without bound
# (OOM). These assert that real helper — the daemon side is already bounded
# (apps.rs read_line_bounded / genproxy MAX_PROXY_LINE_BYTES).
import main as _frame_mod  # noqa: E402 — deliberately mid-file, after the app's own imports


def test_max_frame_bytes_is_8_mib():
    assert _frame_mod.MAX_FRAME_BYTES == 8 * 1024 * 1024


def test_oversized_frame_is_dropped_not_accumulated():
    # A newline-less frame past the cap is DISCARDED, not retained -> memory bounded.
    cap = _frame_mod.MAX_FRAME_BYTES
    lines, buf, overflowed = _frame_mod.drain_lines(b"x" * (cap + 1))
    assert overflowed is True
    assert buf == b""
    assert lines == []


def test_complete_lines_drain_and_partial_is_preserved():
    # Newline framing is intact: whole lines come out in order; a small partial stays.
    lines, buf, overflowed = _frame_mod.drain_lines(b'{"a":1}\n{"b":2}\n{"c":3')
    assert lines == [b'{"a":1}', b'{"b":2}']
    assert buf == b'{"c":3'
    assert overflowed is False


if __name__ == "__main__":
    # Script-style runs exercise the framing tests too — they are plain
    # functions the runner below would otherwise never call.
    test_max_frame_bytes_is_8_mib()
    test_oversized_frame_is_dropped_not_accumulated()
    test_complete_lines_drain_and_partial_is_preserved()
    print("framing: 3 checks ok")
    unittest.main()

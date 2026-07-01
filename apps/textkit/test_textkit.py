#!/usr/bin/env python3
"""Tests for textkit.compute — real cases plus hostile/empty input that must not raise."""
import unittest

from main import compute


class TestCompute(unittest.TestCase):
    def test_basic_sentence(self):
        r = compute({"text": "Hello world. This is fun!"})
        self.assertEqual(r["chars"], 25)
        self.assertEqual(r["words"], 5)
        self.assertEqual(r["sentences"], 2)
        # words: Hello(5) world(5) This(4) is(2) fun(3) -> 19/5 = 3.8
        self.assertEqual(r["avg_word_len"], 3.8)
        self.assertIn(r["longest_word"], ("Hello", "world"))
        self.assertEqual(len(r["longest_word"]), 5)
        self.assertEqual(r["unique_words"], 5)
        self.assertEqual(r["words_per_sentence"], 2.5)

    def test_no_terminator_gets_one_sentence(self):
        r = compute({"text": "just a fragment"})
        self.assertEqual(r["sentences"], 1)
        self.assertEqual(r["words"], 3)
        self.assertEqual(r["words_per_sentence"], 3.0)

    def test_unique_case_insensitive_and_punctuation_stripped(self):
        r = compute({"text": "Cat cat, CAT? dog."})
        # cat/Cat/CAT collapse to one unique; dog is another -> 2 unique
        self.assertEqual(r["unique_words"], 2)
        self.assertEqual(r["words"], 4)
        # sentences: '?' and '.' -> 2
        self.assertEqual(r["sentences"], 2)
        # All cleaned tokens are 3 chars; max() returns the first, "Cat".
        self.assertEqual(len(r["longest_word"]), 3)
        self.assertEqual(r["longest_word"], "Cat")

    def test_empty_and_hostile_inputs_do_not_raise(self):
        # Empty string
        r = compute({"text": ""})
        self.assertEqual(r["chars"], 0)
        self.assertEqual(r["words"], 0)
        self.assertEqual(r["sentences"], 0)
        self.assertEqual(r["avg_word_len"], 0.0)
        self.assertEqual(r["longest_word"], "")
        self.assertEqual(r["unique_words"], 0)
        self.assertEqual(r["words_per_sentence"], 0.0)

        # Missing key
        r2 = compute({})
        self.assertEqual(r2["chars"], 0)
        self.assertEqual(r2["sentences"], 0)

        # Non-string text
        r3 = compute({"text": 12345})
        self.assertEqual(r3["chars"], 0)
        self.assertEqual(r3["words"], 0)

        # Non-dict payload
        r4 = compute(None)
        self.assertEqual(r4["chars"], 0)
        self.assertEqual(r4["words_per_sentence"], 0.0)

        # Whitespace-only (has terminators? no) -> sentences 0, no crash
        r5 = compute({"text": "   \t\n  "})
        self.assertEqual(r5["words"], 0)
        self.assertEqual(r5["sentences"], 0)

        # Only punctuation terminators, no words -> division guarded
        r6 = compute({"text": "!!! ??? ..."})
        self.assertEqual(r6["words"], 3)
        self.assertEqual(r6["unique_words"], 0)
        self.assertEqual(r6["longest_word"], "")
        self.assertEqual(r6["avg_word_len"], 0.0)

    def test_words_per_sentence_zero_when_no_sentences(self):
        r = compute({"text": ""})
        self.assertEqual(r["words_per_sentence"], 0.0)


if __name__ == "__main__":
    unittest.main()

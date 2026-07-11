#!/usr/bin/env python3
"""Tests for example-plugin's input-frame bounding.

example-plugin is the PLUGIN SDK reference handler; it has no compute() surface,
so this covers the one behavior it shares with every micro-app: main()'s socket
read loop caps a single un-newlined frame via main.drain_lines / MAX_FRAME_BYTES
so a peer can't grow the read buffer without bound (OOM). Run from this dir:
`python3 test_example_plugin.py` or `pytest`.
"""
import main


def test_max_frame_bytes_is_8_mib():
    assert main.MAX_FRAME_BYTES == 8 * 1024 * 1024


def test_oversized_frame_is_dropped_not_accumulated():
    cap = main.MAX_FRAME_BYTES
    lines, buf, overflowed = main.drain_lines(b"x" * (cap + 1))
    assert overflowed is True
    assert buf == b""
    assert lines == []


def test_complete_lines_drain_and_partial_is_preserved():
    lines, buf, overflowed = main.drain_lines(b'{"a":1}\n{"b":2}\n{"c":3')
    assert lines == [b'{"a":1}', b'{"b":2}']
    assert buf == b'{"c":3'
    assert overflowed is False


if __name__ == "__main__":
    for t in [
        test_max_frame_bytes_is_8_mib,
        test_oversized_frame_is_dropped_not_accumulated,
        test_complete_lines_drain_and_partial_is_preserved,
    ]:
        t()
        print("ok:", t.__name__)
    print("ALL PASSED")

#!/usr/bin/env python3
"""Plain tests for jwtpeek.compute — real cases plus hostile/empty input that must not raise."""
import base64
import json

from main import compute


def _b64url(obj):
    raw = json.dumps(obj, separators=(",", ":")).encode("utf-8")
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode("ascii")


def make_jwt(header, payload, signature="c2lnbmF0dXJl"):
    return _b64url(header) + "." + _b64url(payload) + "." + signature


def main():
    # 1) A well-formed JWT decodes header + payload and reports the signature present.
    hdr = {"alg": "HS256", "typ": "JWT"}
    pld = {"sub": "1234567890", "name": "Alice", "admin": True}
    out = compute({"token": make_jwt(hdr, pld)})
    assert "error" not in out, out
    assert out["header"] == hdr, out
    assert out["payload"] == pld, out
    assert out["signature_present"] is True, out

    # 2) An empty signature segment is still a valid 3-part JWT: signature_present is False.
    tok = make_jwt(hdr, pld, signature="")
    out = compute({"token": tok})
    assert "error" not in out, out
    assert out["signature_present"] is False, out
    assert out["payload"]["name"] == "Alice", out

    # 3) Unicode claims survive the UTF-8 round trip.
    out = compute({"token": make_jwt({"alg": "none"}, {"user": "Zoë", "role": "admin"})})
    assert "error" not in out, out
    assert out["payload"]["user"] == "Zoë", out

    # 4) Wrong number of parts is rejected (2 parts).
    out = compute({"token": _b64url(hdr) + "." + _b64url(pld)})
    assert "error" in out, out

    # 5) A part that is not valid base64url / JSON yields an error, not a crash.
    out = compute({"token": "!!!." + _b64url(pld) + ".sig"})
    assert "error" in out, out

    # 6) Base64url that decodes to non-JSON bytes is an error.
    good_b64_not_json = base64.urlsafe_b64encode(b"not json").rstrip(b"=").decode("ascii")
    out = compute({"token": good_b64_not_json + "." + _b64url(pld) + ".sig"})
    assert "error" in out, out

    # 7) Empty header segment is rejected.
    out = compute({"token": "." + _b64url(pld) + ".sig"})
    assert "error" in out, out

    # 7b) A segment carrying a non-base64url character (standard-base64 '+', which
    #     is illegal in base64url) is rejected as malformed — NOT leniently decoded
    #     and reported valid. The header "eyJ4IjogIj4+Pj4ifQ" contains a '+'.
    out = compute({"token": "eyJ4IjogIj4+Pj4ifQ.eyJzIjogMX0.sig"})
    assert "error" in out, out
    assert "base64url" in out["error"], out

    # 8) Hostile / empty / wrong-type inputs must NOT raise, always return {"error": ...}.
    for bad in [
        {},                          # missing token
        {"token": ""},               # empty string
        {"token": "   "},            # whitespace only
        {"token": 12345},            # non-string
        {"token": None},             # None
        {"token": True},             # bool must not be treated as a string
        {"token": ["a", "b", "c"]},  # list
        {"token": {"nested": 1}},    # dict
        "not-a-dict",                # payload not a mapping
        None,                        # payload is None
        42,                          # payload is an int
    ]:
        out = compute(bad)
        assert isinstance(out, dict), (bad, out)
        assert "error" in out, (bad, out)

    print("all jwtpeek tests passed")
    return 0


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


# -- the agent-tool request/response contract (SHARED shape; copy per app) ----
# jwtpeek.decode is the declared, non-consequential tool the agent loop invokes.
# A request carrying a request `id` is answered with a type:"result" line echoing
# that id; an op without one keeps the legacy uncorrelated type:"items" line. The
# JWT rides the non-reserved "jwt" param (the wire envelope reserves "token").
# make_jwt(...) builds a minimal valid token compute() decodes without error.
_VALID_JWT = make_jwt({"alg": "HS256", "typ": "JWT"}, {"sub": "1234567890"})


class FakeConn:
    """Captures sendall payloads so handle() can be driven without a socket."""

    def __init__(self):
        self.lines = []

    def sendall(self, raw):
        self.lines.append(json.loads(raw.decode("utf-8").strip()))


def test_tool_op_with_id_answers_a_correlated_result():
    conn = FakeConn()
    _frame_mod.handle(conn, {"type": "jwtpeek.decode", "id": "req-7", "jwt": _VALID_JWT})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "result", reply
    assert reply["id"] == "req-7", "the request id is echoed verbatim"
    assert "error" not in reply["data"], reply
    assert reply["data"]["signature_present"] is True
    assert reply["token"] == _frame_mod.TOKEN


def test_tool_op_without_id_keeps_the_legacy_items_line():
    conn = FakeConn()
    _frame_mod.handle(conn, {"type": "jwtpeek.decode", "jwt": _VALID_JWT})
    assert len(conn.lines) == 1
    reply = conn.lines[0]
    assert reply["type"] == "items", "no id -> uncorrelated legacy line"
    assert "id" not in reply
    assert "error" not in reply["data"], reply
    assert reply["data"]["signature_present"] is True


def test_non_string_or_empty_id_is_treated_as_absent():
    for bad_id in (7, "", None, ["x"]):
        conn = FakeConn()
        _frame_mod.handle(conn, {"type": "jwtpeek.decode", "id": bad_id, "jwt": _VALID_JWT})
        assert conn.lines[0]["type"] == "items", f"id={bad_id!r} must not correlate"


if __name__ == "__main__":
    # Script-style runs exercise the framing tests too — they are plain
    # functions the runner below would otherwise never call.
    test_max_frame_bytes_is_8_mib()
    test_oversized_frame_is_dropped_not_accumulated()
    test_complete_lines_drain_and_partial_is_preserved()
    print("framing: 3 checks ok")
    test_tool_op_with_id_answers_a_correlated_result()
    test_tool_op_without_id_keeps_the_legacy_items_line()
    test_non_string_or_empty_id_is_treated_as_absent()
    print("agent-tool contract: 3 checks ok")
    raise SystemExit(main())

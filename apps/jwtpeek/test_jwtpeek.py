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


if __name__ == "__main__":
    raise SystemExit(main())

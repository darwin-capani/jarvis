#!/usr/bin/env python3
"""Plain-python tests for jsonpath.compute — real cases plus hostile input that must not raise."""
import sys

from main import compute


def check(name, cond):
    if not cond:
        print("FAIL:", name)
        sys.exit(1)
    print("ok:", name)


DOC = '{"a": {"b": [{"c": 42}, {"c": 7}], "d": "hi"}, "list": [10, 20, 30], "flag": true, "nil": null}'


def main():
    # 1) Nested dotted key + array index + dotted key.
    r = compute({"json": DOC, "path": "a.b[0].c"})
    check("nested value", r.get("value") == 42)
    check("nested type", r.get("type") == "int")

    # 2) Second array element via bracket index.
    r = compute({"json": DOC, "path": "a.b[1].c"})
    check("second element", r.get("value") == 7)

    # 3) Plain top-level string key.
    r = compute({"json": DOC, "path": "a.d"})
    check("string value", r.get("value") == "hi")

    # 4) Array index on a top-level array.
    r = compute({"json": DOC, "path": "list[2]"})
    check("array index", r.get("value") == 30)

    # 5) Negative array index resolves from the end.
    r = compute({"json": DOC, "path": "list[-1]"})
    check("negative index", r.get("value") == 30)
    r = compute({"json": DOC, "path": "list[-3]"})
    check("negative index front", r.get("value") == 10)

    # 6) Empty path returns the whole document.
    r = compute({"json": DOC, "path": ""})
    check("empty path whole doc", isinstance(r.get("value"), dict) and "a" in r["value"])

    # 7) Returning a container value carries its length; short list not truncated.
    r = compute({"json": DOC, "path": "list"})
    check("list value", r.get("value") == [10, 20, 30])
    check("list type", r.get("type") == "list")
    check("list length", r.get("length") == 3)
    check("list not truncated", "truncated" not in r)

    # 8) Boolean and null leaf values survive.
    check("bool leaf", compute({"json": DOC, "path": "flag"}).get("value") is True)
    r = compute({"json": DOC, "path": "nil"})
    check("null leaf", r.get("value") is None and r.get("type") == "NoneType")

    # 9) Leading root marker "$" and leading dot are tolerated.
    check("root marker", compute({"json": DOC, "path": "$.a.d"}).get("value") == "hi")
    check("leading dot", compute({"json": DOC, "path": ".a.d"}).get("value") == "hi")

    # 10) Quoted bracket key allows dots inside a literal key.
    r = compute({"json": '{"a.b": 5, "c": {"x": 9}}', "path": "['a.b']"})
    check("quoted dotted key", r.get("value") == 5)
    r = compute({"json": '{"a b": 1}', "path": '["a b"]'})
    check("quoted spaced key", r.get("value") == 1)

    # 11) Large array is capped to 50 items with true length + truncated flag.
    big = "[" + ",".join(str(i) for i in range(200)) + "]"
    r = compute({"json": big, "path": ""})
    check("cap length reported", r.get("length") == 200)
    check("cap truncated flag", r.get("truncated") is True)
    check("cap value size", isinstance(r.get("value"), list) and len(r["value"]) == 50)
    check("cap first item", r["value"][0] == 0)

    # 12) Missing key -> error, no raise.
    check("missing key error", "error" in compute({"json": DOC, "path": "a.zzz"}))
    # 13) Index out of range -> error.
    check("index oob error", "error" in compute({"json": DOC, "path": "list[9]"}))
    check("neg index oob error", "error" in compute({"json": DOC, "path": "list[-9]"}))
    # 14) Indexing a non-array with [n] -> error.
    check("non-array index error", "error" in compute({"json": DOC, "path": "a[0]"}))
    # 15) Keying into a non-object -> error.
    check("non-object key error", "error" in compute({"json": DOC, "path": "a.d.x"}))
    # 16) Malformed json -> error.
    check("bad json error", "error" in compute({"json": "{not valid}", "path": "a"}))
    check("empty json error", "error" in compute({"json": "", "path": "a"}))
    # 17) Malformed path syntax -> error.
    check("unclosed bracket error", "error" in compute({"json": DOC, "path": "list[0"}))
    check("empty subscript error", "error" in compute({"json": DOC, "path": "list[]"}))
    check("non-int index error", "error" in compute({"json": DOC, "path": "list[x]"}))
    check("double dot error", "error" in compute({"json": DOC, "path": "a..b"}))
    check("trailing dot error", "error" in compute({"json": DOC, "path": "a."}))

    # 18) A JSON scalar document with empty path returns the scalar.
    check("scalar doc", compute({"json": "123", "path": ""}).get("value") == 123)
    check("string doc", compute({"json": '"hey"', "path": ""}).get("value") == "hey")

    # 19) Hostile / malformed inputs must NOT raise and must report an error.
    for bad in [None, 123, "not a dict", [], ["a"],
                {"json": 5, "path": "a"}, {"json": None, "path": "a"},
                {"json": True, "path": "a"}, {"json": DOC, "path": 7},
                {"json": DOC, "path": None}, {"json": DOC, "path": True},
                {"json": DOC, "path": ["a"]},
                {"path": "a.b"}, {}]:
        out = compute(bad)
        check("hostile no-raise: %r" % (bad,), isinstance(out, dict) and "error" in out)

    # 19b) A missing "path" key defaults to the empty path (whole doc), not an error.
    r = compute({"json": DOC})
    check("missing path -> whole doc", isinstance(r.get("value"), dict) and "a" in r["value"])
    # Hostile dicts that are still valid inputs must return a dict either way.
    for ok_input in [{"json": DOC}]:
        out = compute(ok_input)
        check("valid-ish no-raise: %r" % (ok_input,), isinstance(out, dict))

    # 20) Bare first-segment key without a leading dot works.
    check("bare first key", compute({"json": '{"k": 1}', "path": "k"}).get("value") == 1)
    # 21) Consecutive brackets (2D array).
    r = compute({"json": "[[1,2],[3,4]]", "path": "[1][0]"})
    check("2d array", r.get("value") == 3)

    print("ALL PASS")
    sys.exit(0)


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""CIDR aggregation and overlap analysis via stdlib ipaddress. Pure, offline."""
import os
import sys
import ipaddress

# Shared host-link plumbing (socket loop, token stamping, frame bound, the
# agent-tool id echo) from apps/_sdk — fs_read-granted. The path is resolved
# relative to THIS file (apps/<app>/main.py -> ../_sdk), so it works both when
# darwind launches the app (cwd = project root) and when the tests run from the
# app dir. Bytecode writes are disabled since apps/_sdk is read-only in the
# sandbox. Re-importing drain_lines/MAX_FRAME_BYTES/TOKEN keeps them resolvable
# off `main` for the framing/contract tests.
sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "_sdk"))
from harness import (  # noqa: E402 — must follow the sys.path insert above
    MAX_FRAME_BYTES,
    TOKEN,
    drain_lines,
    reply_result,
    run,
    send,
)


# Bounds (measured on an M1 Pro): the pairwise overlap scan is O(n^2) — 10k
# distinct CIDRs is ~50M subnet_of() calls (~50 s, wedging the single-threaded
# app), so the DISTINCT input count is capped; 512 distinct = ~131k pairs,
# well under a second. Listed overlap pairs are separately bounded so the
# reply always fits the daemon's 1 MiB app-line budget.
MAX_CIDRS = 512
MAX_OVERLAPS = 500


def compute(payload):
    """PURE, offline, no I/O, never raises.

    Input: payload["cidrs"] — a non-empty list of str CIDRs and/or bare IPs
    (mixed IPv4/IPv6 allowed). Each is parsed with ipaddress.ip_network(x,
    strict=False). Aggregation is per family via ipaddress.collapse_addresses,
    giving the minimal covering prefix set.

    Output dict: input_count, aggregated (sorted cidr strings), aggregated_count,
    overlaps (list of {a, b, relation} for every input pair — same family — where
    one is subnet/supernet of the other; relation is "equal" / "a-contains-b" /
    "b-contains-a"), ipv4_addresses (int total over the aggregated IPv4 set) and
    ipv6_addresses (that total for IPv6, as a STRING since it may exceed 2**63).

    An empty list -> {"error"}; any unparseable/non-str entry -> {"error":
    "invalid: <x>"}.
    """
    try:
        if not isinstance(payload, dict):
            return {"error": "payload must be a mapping"}
        cidrs = payload.get("cidrs")
        if not isinstance(cidrs, list):
            return {"error": "cidrs must be a list"}
        if len(cidrs) == 0:
            return {"error": "cidrs must be a non-empty list"}
        if len(cidrs) > MAX_CIDRS:
            return {"error": "too many cidrs: %d (max %d)" % (len(cidrs), MAX_CIDRS)}
        nets = []
        for x in cidrs:
            if not isinstance(x, str):
                return {"error": "invalid: %s" % (x,)}
            try:
                net = ipaddress.ip_network(x, strict=False)
            except (ValueError, TypeError):
                return {"error": "invalid: %s" % (x,)}
            nets.append(net)
        # Aggregate per family; collapse_addresses requires a single version.
        v4 = [n for n in nets if n.version == 4]
        v6 = [n for n in nets if n.version == 6]
        agg4 = list(ipaddress.collapse_addresses(v4)) if v4 else []
        agg6 = list(ipaddress.collapse_addresses(v6)) if v6 else []
        aggregated_nets = agg4 + agg6
        aggregated_nets.sort(
            key=lambda n: (n.version, int(n.network_address), n.prefixlen)
        )
        aggregated = [str(n) for n in aggregated_nets]
        ipv4_addresses = sum(n.num_addresses for n in agg4)
        ipv6_addresses = sum(n.num_addresses for n in agg6)
        # Overlaps are computed over the DISTINCT parsed inputs (collapse would
        # otherwise erase every overlap; duplicates would explode the pair list
        # quadratically — 4000 copies of one /8 is ~8M "equal" dicts and ~1 GB,
        # measured — so exact duplicates are collapsed first and reported via
        # duplicate_count). Only same-family pairs can relate. The listed pairs
        # are BOUNDED (MAX_OVERLAPS) with the full count reported honestly:
        # an unbounded list would exceed the daemon's 1 MiB app-line budget and
        # the whole reply would be dropped.
        distinct = []
        seen_keys = set()
        for n in nets:
            k = (n.version, int(n.network_address), n.prefixlen)
            if k not in seen_keys:
                seen_keys.add(k)
                distinct.append(n)
        duplicate_count = len(nets) - len(distinct)
        overlaps = []
        overlap_count = 0
        for i in range(len(distinct)):
            for j in range(i + 1, len(distinct)):
                a = distinct[i]
                b = distinct[j]
                if a.version != b.version:
                    continue
                if b.subnet_of(a):
                    rel = "a-contains-b"
                elif a.subnet_of(b):
                    rel = "b-contains-a"
                else:
                    continue
                overlap_count += 1
                if len(overlaps) < MAX_OVERLAPS:
                    overlaps.append({"a": str(a), "b": str(b), "relation": rel})
        return {
            "input_count": len(nets),
            "distinct_count": len(distinct),
            "duplicate_count": duplicate_count,
            "aggregated": aggregated,
            "aggregated_count": len(aggregated),
            "overlaps": overlaps,
            "overlap_count": overlap_count,
            "overlaps_truncated": overlap_count > MAX_OVERLAPS,
            "ipv4_addresses": ipv4_addresses,
            "ipv6_addresses": str(ipv6_addresses),
        }
    except Exception as e:  # noqa: BLE001 — compute must never raise
        return {"error": "unexpected: %s" % e}


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "cidr.aggregate", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "cidr.aggregate":
        reply_result(conn, msg, compute(msg))
    elif op == "stop":
        raise SystemExit(0)


if __name__ == "__main__":
    sys.exit(run(handle))

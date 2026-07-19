#!/usr/bin/env python3
"""CIDR aggregation and overlap analysis via stdlib ipaddress. Pure, offline."""
import json
import os
import socket
import sys
import ipaddress

TOKEN = os.environ.get("DARWIN_APP_TOKEN", "")
SOCKET_PATH = os.environ.get("DARWIN_APP_SOCKET", "")


def send(conn, obj):
    obj["token"] = TOKEN
    conn.sendall((json.dumps(obj) + "\n").encode("utf-8"))


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
        # Overlaps are computed over the ORIGINAL parsed inputs (collapse would
        # otherwise erase every overlap). Only same-family pairs can relate.
        overlaps = []
        for i in range(len(nets)):
            for j in range(i + 1, len(nets)):
                a = nets[i]
                b = nets[j]
                if a.version != b.version:
                    continue
                if a == b:
                    rel = "equal"
                elif b.subnet_of(a):
                    rel = "a-contains-b"
                elif a.subnet_of(b):
                    rel = "b-contains-a"
                else:
                    continue
                overlaps.append({"a": str(a), "b": str(b), "relation": rel})
        return {
            "input_count": len(nets),
            "aggregated": aggregated,
            "aggregated_count": len(aggregated),
            "overlaps": overlaps,
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
        send(conn, {"type": "items", "data": compute(msg)})
    elif op == "stop":
        raise SystemExit(0)


MAX_FRAME_BYTES = 8 * 1024 * 1024  # cap on one un-newlined frame from the daemon


def drain_lines(buf, max_frame=MAX_FRAME_BYTES):
    """PURE framing: split every complete newline-terminated line out of buf.

    Returns (lines, remaining, overflowed): the complete lines with their trailing
    newline stripped in arrival order, the leftover partial buffer, and whether
    that leftover grew past max_frame WITHOUT a newline. When it has, the leftover
    is DROPPED (returned as b"") so a peer streaming an unframed, unbounded blob
    can't grow the read buffer without bound (OOM) — the daemon side is already
    bounded (apps.rs read_line_bounded / genproxy MAX_PROXY_LINE_BYTES). Newline
    framing is otherwise identical to buf.split(b"\\n", 1). Never raises."""
    lines = []
    while b"\n" in buf:
        line, buf = buf.split(b"\n", 1)
        lines.append(line)
    overflowed = len(buf) > max_frame
    if overflowed:
        buf = b""
    return lines, buf, overflowed


def main():
    if not TOKEN or not SOCKET_PATH:
        print("missing DARWIN_APP_TOKEN / DARWIN_APP_SOCKET; not launched by darwind", file=sys.stderr)
        return 1
    conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    conn.connect(SOCKET_PATH)
    buf = b""
    while True:
        chunk = conn.recv(4096)
        if not chunk:
            break
        buf += chunk
        lines, buf, overflowed = drain_lines(buf)
        for line in lines:
            if not line.strip():
                continue
            try:
                handle(conn, json.loads(line))
            except SystemExit:
                return 0
            except Exception as e:  # noqa: BLE001
                send(conn, {"type": "log", "data": {"line": f"handler error: {e}"}})
        if overflowed:
            send(conn, {"type": "log", "data": {"line": f"input frame exceeded {MAX_FRAME_BYTES} bytes; dropped"}})
    return 0


if __name__ == "__main__":
    sys.exit(main())

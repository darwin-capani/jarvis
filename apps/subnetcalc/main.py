#!/usr/bin/env python3
"""IPv4/IPv6 CIDR subnet calculator (address plan, splits, VLSM). Pure, offline."""
import ipaddress
import json
import os
import socket
import sys

TOKEN = os.environ.get("DARWIN_APP_TOKEN", "")
SOCKET_PATH = os.environ.get("DARWIN_APP_SOCKET", "")

MAX_SUBNETS = 1 << 16  # cap on the SPLIT ITSELF (finer is refused outright)
# The LISTED subnets are separately bounded: the daemon drops any app line
# over 1 MiB (apps.rs MAX_APP_LINE_BYTES), and a fully-listed 65536-way split
# is ~1.19 MB on the wire (measured) — the reply would vanish and the app
# connection would be torn down. 1024 listed subnets is ~25 KB; the full
# count + a truncation flag keep the answer honest.
MAX_SUBNETS_LISTED = 1 << 10


def send(conn, obj):
    obj["token"] = TOKEN
    conn.sendall((json.dumps(obj) + "\n").encode("utf-8"))


def reply_result(conn, msg, data):
    """Answer one domain op, correlated when the host asked for correlation.

    THE AGENT-TOOL CONTRACT: a request carrying a non-empty string `id` (the
    daemon's request_op) is answered with a `type:"result"` line ECHOING that id
    so the host can route the payload back to the waiting caller. A request
    without an id (the voice router / legacy paths) keeps the uncorrelated
    `type:"items"` telemetry line — byte-identical to the pre-contract wire."""
    rid = msg.get("id")
    if isinstance(rid, str) and rid:
        send(conn, {"type": "result", "id": rid, "data": data})
    else:
        send(conn, {"type": "items", "data": data})


def compute(payload):
    """PURE, offline, no I/O, never raises.

    Input: payload["cidr"] (str) — an IPv4/IPv6 network or a bare host, e.g.
    "192.168.1.0/24", "10.0.0.5/32", "8.8.8.8", "2001:db8::/32". Parsed with
    ipaddress.ip_network(cidr, strict=False), so host bits are tolerated and the
    result is normalized to the network address.

    Optional (mutually exclusive):
      payload["split_count"] (int, power of 2) — split into that many equal subnets.
      payload["split_hosts"] (int >= 1)        — VLSM: smallest equal subnets that
                                                  each hold >= that many usable hosts.

    Output (IPv4): {version, cidr, network, netmask, wildcard, prefixlen,
    num_addresses, is_private, is_global, broadcast?, hostmin, hostmax,
    num_usable_hosts}. RFC 3021 /31 => 2 usable, no broadcast, host range = both
    addresses; /32 => 1 usable, hostmin==hostmax, no broadcast.

    Output (IPv6): {version, cidr, network, prefixlen, num_addresses (STRING),
    hostmin, hostmax, is_private, is_global} — no broadcast/netmask concept.

    A requested split adds subnets:[CIDR strings]. Bad cidr, conflicting/invalid
    split params, or a split finer than the address space => {"error": ...}.
    """
    try:
        if not isinstance(payload, dict):
            return {"error": "payload must be a mapping"}

        cidr = payload.get("cidr")
        if not isinstance(cidr, str) or not cidr.strip():
            return {"error": "cidr must be a non-empty string"}

        has_count = "split_count" in payload
        has_hosts = "split_hosts" in payload
        if has_count and has_hosts:
            return {"error": "specify at most one of split_count / split_hosts"}

        def _positive_int(value, name):
            if isinstance(value, bool) or not isinstance(value, int):
                return None, {"error": "%s must be an integer" % name}
            if value < 1:
                return None, {"error": "%s must be >= 1" % name}
            return value, None

        split_count = None
        split_hosts = None
        if has_count:
            split_count, err = _positive_int(payload["split_count"], "split_count")
            if err:
                return err
        if has_hosts:
            split_hosts, err = _positive_int(payload["split_hosts"], "split_hosts")
            if err:
                return err

        try:
            net = ipaddress.ip_network(cidr.strip(), strict=False)
        except ValueError as e:
            return {"error": "invalid cidr: %s" % e}

        prefixlen = net.prefixlen
        max_prefix = net.max_prefixlen

        if net.version == 4:
            num_addresses = net.num_addresses
            result = {
                "version": 4,
                "cidr": str(net),
                "network": str(net.network_address),
                "netmask": str(net.netmask),
                "wildcard": str(net.hostmask),
                "prefixlen": prefixlen,
                "num_addresses": num_addresses,
                "is_private": bool(net.is_private),
                "is_global": bool(net.is_global),
            }
            if prefixlen <= 30:
                result["broadcast"] = str(net.broadcast_address)
                result["hostmin"] = str(net.network_address + 1)
                result["hostmax"] = str(net.broadcast_address - 1)
                result["num_usable_hosts"] = num_addresses - 2
            elif prefixlen == 31:  # RFC 3021 point-to-point: both addresses usable
                result["hostmin"] = str(net.network_address)
                result["hostmax"] = str(net.broadcast_address)
                result["num_usable_hosts"] = 2
            else:  # /32 single host
                result["hostmin"] = str(net.network_address)
                result["hostmax"] = str(net.network_address)
                result["num_usable_hosts"] = 1
        else:
            result = {
                "version": 6,
                "cidr": str(net),
                "network": str(net.network_address),
                "prefixlen": prefixlen,
                "num_addresses": str(net.num_addresses),
                "hostmin": str(net.network_address),
                "hostmax": str(net.broadcast_address),
                "is_private": bool(net.is_private),
                "is_global": bool(net.is_global),
            }

        if split_count is None and split_hosts is None:
            return result

        # ---- a split was requested: resolve the target prefix length ----
        if split_count is not None:
            if split_count & (split_count - 1) != 0:
                return {"error": "split_count must be a power of 2"}
            bits = split_count.bit_length() - 1
            new_prefix = prefixlen + bits
            if new_prefix > max_prefix:
                return {"error": "split_count too fine for the address space"}
        else:
            if net.version == 4:
                if split_hosts <= 1:
                    new_prefix = 32
                elif split_hosts == 2:  # RFC 3021 fits exactly 2 usable
                    new_prefix = 31
                else:
                    host_bits = 2
                    while (1 << host_bits) - 2 < split_hosts:
                        host_bits += 1
                    new_prefix = 32 - host_bits
            else:
                if split_hosts <= 1:
                    new_prefix = 128
                else:
                    host_bits = 0
                    while (1 << host_bits) < split_hosts:
                        host_bits += 1
                    new_prefix = 128 - host_bits
            if new_prefix < prefixlen:
                return {"error": "network too small to hold split_hosts usable hosts per subnet"}

        count = 1 << (new_prefix - prefixlen)
        if count > MAX_SUBNETS:
            return {"error": "split would produce %d subnets (max %d)" % (count, MAX_SUBNETS)}
        listed = []
        for sub in net.subnets(new_prefix=new_prefix):
            if len(listed) >= MAX_SUBNETS_LISTED:
                break
            listed.append(str(sub))
        result["subnet_count"] = count
        result["subnets"] = listed
        result["subnets_truncated"] = count > MAX_SUBNETS_LISTED
        return result
    except Exception as e:  # noqa: BLE001 — compute must never raise
        return {"error": "unexpected: %s" % e}


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "subnet.plan", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "subnet.plan":
        reply_result(conn, msg, compute(msg))
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

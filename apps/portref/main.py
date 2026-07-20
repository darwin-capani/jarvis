#!/usr/bin/env python3
"""Well-known TCP/UDP port <-> service reference (curated table: IANA registrations plus de-facto conventions). Pure, offline."""
import os
import sys

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

# Curated table (IANA registrations + de-facto conventions like 8080/9090):
# port -> list of {service, proto, desc}. Module-level, no I/O.
PORTS = {
    20: [{"service": "ftp-data", "proto": "tcp", "desc": "FTP data transfer"}],
    21: [{"service": "ftp", "proto": "tcp", "desc": "FTP control"}],
    22: [{"service": "ssh", "proto": "tcp", "desc": "Secure Shell"}],
    23: [{"service": "telnet", "proto": "tcp", "desc": "Telnet (insecure remote login)"}],
    25: [{"service": "smtp", "proto": "tcp", "desc": "SMTP mail transfer"}],
    53: [{"service": "dns", "proto": "tcp/udp", "desc": "Domain Name System"}],
    67: [{"service": "dhcp", "proto": "udp", "desc": "DHCP/BOOTP server"}],
    68: [{"service": "dhcp", "proto": "udp", "desc": "DHCP/BOOTP client"}],
    80: [{"service": "http", "proto": "tcp", "desc": "HTTP web"}],
    110: [{"service": "pop3", "proto": "tcp", "desc": "POP3 mail retrieval"}],
    119: [{"service": "nntp", "proto": "tcp", "desc": "Network News Transfer Protocol"}],
    123: [{"service": "ntp", "proto": "udp", "desc": "Network Time Protocol"}],
    143: [{"service": "imap", "proto": "tcp", "desc": "IMAP mail access"}],
    161: [{"service": "snmp", "proto": "udp", "desc": "SNMP monitoring"}],
    162: [{"service": "snmp-trap", "proto": "udp", "desc": "SNMP trap"}],
    179: [{"service": "bgp", "proto": "tcp", "desc": "Border Gateway Protocol"}],
    389: [{"service": "ldap", "proto": "tcp", "desc": "LDAP directory"}],
    443: [{"service": "https", "proto": "tcp", "desc": "HTTP over TLS"}],
    445: [{"service": "smb", "proto": "tcp", "desc": "SMB/CIFS file sharing"}],
    465: [{"service": "smtps", "proto": "tcp", "desc": "SMTP over TLS"}],
    514: [{"service": "syslog", "proto": "udp", "desc": "Syslog logging"}],
    587: [{"service": "submission", "proto": "tcp", "desc": "Mail message submission"}],
    636: [{"service": "ldaps", "proto": "tcp", "desc": "LDAP over TLS"}],
    993: [{"service": "imaps", "proto": "tcp", "desc": "IMAP over TLS"}],
    995: [{"service": "pop3s", "proto": "tcp", "desc": "POP3 over TLS"}],
    1080: [{"service": "socks", "proto": "tcp", "desc": "SOCKS proxy"}],
    1194: [{"service": "openvpn", "proto": "udp", "desc": "OpenVPN"}],
    1433: [{"service": "mssql", "proto": "tcp", "desc": "Microsoft SQL Server"}],
    1521: [{"service": "oracle", "proto": "tcp", "desc": "Oracle database"}],
    3306: [{"service": "mysql", "proto": "tcp", "desc": "MySQL database"}],
    3389: [{"service": "rdp", "proto": "tcp", "desc": "Remote Desktop Protocol"}],
    5060: [{"service": "sip", "proto": "tcp/udp", "desc": "Session Initiation Protocol"}],
    5432: [{"service": "postgres", "proto": "tcp", "desc": "PostgreSQL database"}],
    5900: [{"service": "vnc", "proto": "tcp", "desc": "VNC remote desktop"}],
    6379: [{"service": "redis", "proto": "tcp", "desc": "Redis key-value store"}],
    8080: [{"service": "http-alt", "proto": "tcp", "desc": "HTTP alternate"}],
    8443: [{"service": "https-alt", "proto": "tcp", "desc": "HTTPS alternate"}],
    9090: [{"service": "prometheus", "proto": "tcp", "desc": "Prometheus metrics"}],
    9200: [{"service": "elasticsearch", "proto": "tcp", "desc": "Elasticsearch REST API"}],
    27017: [{"service": "mongodb", "proto": "tcp", "desc": "MongoDB database"}],
}


def _port_range(port):
    if port <= 1023:
        return "system"
    if port <= 49151:
        return "registered"
    return "dynamic/ephemeral"


def compute(payload):
    """PURE, offline, no I/O, never raises.

    Modes (checked in order):
      - payload["port"] (int 0-65535): -> {port, range, services:[{service,proto,desc}]}
        (services is [] when the port is not in the curated table — that is NOT an error).
      - payload["service"] (non-empty str, case-insensitive substring over the service name):
        -> {service_query, matches:[{port,service,proto,desc}]} (matches sorted by port).
    Anything else (neither key / port out of range / wrong type) -> {"error": ...}.
    """
    try:
        if not isinstance(payload, dict):
            return {"error": "payload must be a mapping"}
        if "port" in payload:
            port = payload["port"]
            if isinstance(port, bool) or not isinstance(port, int):
                return {"error": "port must be an integer"}
            if port < 0 or port > 65535:
                return {"error": "port out of range (0-65535)"}
            services = [dict(s) for s in PORTS.get(port, [])]
            return {"port": port, "range": _port_range(port), "services": services}
        if "service" in payload:
            service = payload["service"]
            if not isinstance(service, str):
                return {"error": "service must be a string"}
            needle = service.strip().lower()
            if not needle:
                return {"error": "service must be a non-empty string"}
            matches = []
            for port in sorted(PORTS):
                for entry in PORTS[port]:
                    if needle in entry["service"].lower():
                        matches.append({
                            "port": port,
                            "service": entry["service"],
                            "proto": entry["proto"],
                            "desc": entry["desc"],
                        })
            return {"service_query": service, "matches": matches}
        return {"error": "provide 'port' (int) or 'service' (string)"}
    except Exception as e:  # noqa: BLE001 — compute must never raise
        return {"error": "unexpected: %s" % e}


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "port.lookup", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "port.lookup":
        reply_result(conn, msg, compute(msg))
    elif op == "stop":
        raise SystemExit(0)


if __name__ == "__main__":
    sys.exit(run(handle))

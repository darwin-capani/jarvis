#!/bin/bash
# bringup.sh — one-command DARWIN bring-up + read-only smoke test (WS2).
#
# Resolves the DARWIN root EXACTLY like the daemon, starts the inference server
# then darwind in the correct order (OR detects an already-running pair via the
# live sockets and leaves them alone), waits for readiness with BOUNDED
# timeouts, runs ONE non-consequential token-gated `roster` IPC round-trip on
# the command socket, asserts a healthy reply, prints a per-subsystem PASS /
# SKIP / FAIL board + an overall verdict, then tears down ONLY what IT started.
#
# HONESTY CONTRACT (load-bearing): a missing precondition (no venv / no binary /
# no model) is reported as SKIPPED with the reason — never a faked PASS. A stage
# that did not actually verify is never printed as healthy. Exit is non-zero on
# any hard FAILED; the all-SKIPPED dev-tree path exits 0 (it honestly could not
# bring up the live pipeline here, and says so).
#
# Usage:
#   scripts/bringup.sh             # bring up (start what's down), smoke, tear down
#   scripts/bringup.sh --no-start  # smoke an ALREADY-running pair only (start nothing)
#   scripts/bringup.sh -h|--help
#
# bash 3.2 compatible: no associative arrays, no mapfile, no ${var^^}.
set -euo pipefail

# --- resolve root EXACTLY like the daemon / boot wrappers --------------------
# DARWIN_ROOT env wins (install + boot wrappers set it); else this script's
# parent dir (the install_boot.sh / boot wrapper pattern).
if [ -n "${DARWIN_ROOT:-}" ]; then
    ROOT="$DARWIN_ROOT"
else
    ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fi
export DARWIN_ROOT="$ROOT"

# --- optional cinematic UI (graceful plain fallback) -------------------------
# Reuse scripts/ui.sh styling when present; otherwise fall back to plain,
# dependency-free printers so the harness runs anywhere (CI, a stripped image).
_UI=0
if [ -f "$ROOT/scripts/ui.sh" ]; then
    # shellcheck source=scripts/ui.sh
    # shellcheck disable=SC1091
    if . "$ROOT/scripts/ui.sh" 2>/dev/null && command -v ui_init >/dev/null 2>&1; then
        ui_init
        _UI=1
    fi
fi
say_ok()   { if [ "$_UI" -eq 1 ]; then ui_ok   "$1"; else printf '  [PASS] %s\n' "$1"; fi; }
say_skip() { if [ "$_UI" -eq 1 ]; then ui_warn "$1"; else printf '  [SKIP] %s\n' "$1"; fi; }
say_fail() { if [ "$_UI" -eq 1 ]; then ui_err  "$1"; else printf '  [FAIL] %s\n' "$1"; fi; }
say_info() { if [ "$_UI" -eq 1 ]; then ui_info "$1"; else printf '  ...... %s\n' "$1"; fi; }
say_hr()   { if [ "$_UI" -eq 1 ]; then ui_hr; else printf -- '---\n'; fi; }

# --- derived paths (the daemon's exact layout) -------------------------------
VENV_PY="$ROOT/.venv/bin/python"
DARWIND="$ROOT/daemon/target/release/darwind"
SERVER_PY="$ROOT/inference/server.py"
CONFIG="$ROOT/config/darwin.toml"
IPC="$ROOT/state/ipc"
INF_SOCK="$IPC/inference.sock"
CMD_SOCK="$IPC/command.sock"
CMD_TOKEN="$IPC/command.token"
LOGS="$ROOT/state/logs"
INF_LOG="$LOGS/inference.bringup.log"
DMN_LOG="$LOGS/daemon.bringup.log"

# Telemetry port: read from config if grep-able, else the documented default.
TEL_PORT="7177"
if [ -f "$CONFIG" ]; then
    # Extract ONLY the value of `port = N` in the [telemetry] section: split on
    # '=', strip a trailing '# comment' first, then keep digits (the comment's
    # 127.0.0.1 must never leak into the value).
    _p="$(awk '
        /^\[telemetry\]/ {f=1; next}
        f && /^\[/        {f=0}
        f && /^[[:space:]]*port[[:space:]]*=/ {
            v=$0; sub(/^[^=]*=/, "", v); sub(/#.*/, "", v); gsub(/[^0-9]/, "", v);
            print v; exit
        }' "$CONFIG" 2>/dev/null || true)"
    case "$_p" in ''|*[!0-9]*) : ;; *) TEL_PORT="$_p" ;; esac
fi

# Source gitignored secrets like the boot wrappers (e.g. ANTHROPIC_API_KEY).
if [ -f "$ROOT/state/env.sh" ]; then
    # shellcheck disable=SC1091
    . "$ROOT/state/env.sh"
fi

# --- timeouts (bounded) ------------------------------------------------------
INF_TIMEOUT="${DARWIN_BRINGUP_INF_TIMEOUT:-120}"   # cold model load can be slow
DMN_TIMEOUT="${DARWIN_BRINGUP_DMN_TIMEOUT:-30}"
POLL_INTERVAL="0.25"

MODE="start"
case "${1:-}" in
    "")          MODE="start" ;;
    --no-start)  MODE="no-start" ;;
    -h|--help)
        sed -n '2,21p' "${BASH_SOURCE[0]}"
        exit 0
        ;;
    *)
        echo "error: unknown argument '${1}' (expected --no-start or no args)" >&2
        exit 2
        ;;
esac

# --- per-run state: what we started (so teardown only kills ours) ------------
STARTED_INF_PID=""
STARTED_DMN_PID=""
INF_STAGE="pending"   # pass|skip|fail|pending
DMN_STAGE="pending"
SMOKE_STAGE="pending"
TEL_STAGE="pending"

# A python that can run the tiny socket probes. Prefer the venv python (always
# present on an install); fall back to any python3 on PATH for the no-venv
# dev-tree path so we can still probe a socket if one happens to exist.
probe_python() {
    if [ -x "$VENV_PY" ]; then
        printf '%s' "$VENV_PY"
    elif command -v python3 >/dev/null 2>&1; then
        command -v python3
    else
        printf ''
    fi
}

# Connect-probe a Unix socket (connect + immediate close; spends NO model call).
# Returns 0 iff a connection established. Honest: with no python available we
# fall back to a mere existence test and SAY so via the caller.
unix_connectable() {
    local sock="$1" py
    py="$(probe_python)"
    if [ -n "$py" ]; then
        "$py" - "$sock" <<'PY' 2>/dev/null
import socket, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(1.0)
try:
    s.connect(sys.argv[1])
    s.close()
    sys.exit(0)
except OSError:
    sys.exit(1)
PY
    else
        [ -S "$sock" ]
    fi
}

# Connect-probe a TCP port on loopback (telemetry WS liveness; read-only).
tcp_connectable() {
    local port="$1" py
    py="$(probe_python)"
    [ -n "$py" ] || return 1
    "$py" - "$port" <<'PY' 2>/dev/null
import socket, sys
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.settimeout(1.0)
try:
    s.connect(("127.0.0.1", int(sys.argv[1])))
    s.close()
    sys.exit(0)
except OSError:
    sys.exit(1)
PY
}

# Poll a predicate until it succeeds or the deadline passes. $1=timeout secs,
# remaining args = command to run each tick.
poll_until() {
    local timeout="$1"; shift
    local deadline
    deadline=$(( $(date +%s) + timeout ))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if "$@"; then
            return 0
        fi
        sleep "$POLL_INTERVAL"
    done
    return 1
}

# Invoked indirectly by the EXIT/INT/TERM trap below (shellcheck can't trace it).
# shellcheck disable=SC2329
teardown() {
    # Kill ONLY what this run started; leave an already-running pair alone.
    if [ -n "$STARTED_DMN_PID" ] && kill -0 "$STARTED_DMN_PID" 2>/dev/null; then
        kill "$STARTED_DMN_PID" 2>/dev/null || true
        wait "$STARTED_DMN_PID" 2>/dev/null || true
        say_info "tore down daemon (pid $STARTED_DMN_PID) that this run started"
    fi
    if [ -n "$STARTED_INF_PID" ] && kill -0 "$STARTED_INF_PID" 2>/dev/null; then
        kill "$STARTED_INF_PID" 2>/dev/null || true
        wait "$STARTED_INF_PID" 2>/dev/null || true
        say_info "tore down inference server (pid $STARTED_INF_PID) that this run started"
    fi
}
trap teardown EXIT INT TERM

# =============================================================================
# STAGE 0 — preflight (honest SKIP, never fake)
# =============================================================================
if [ "$_UI" -eq 1 ]; then ui_stage 1 4 "PREFLIGHT" 2>/dev/null || say_hr; else say_hr; fi
printf 'DARWIN bring-up — root: %s\n' "$ROOT"

PREFLIGHT_OK=1
if [ -f "$CONFIG" ]; then
    say_ok "config present: $CONFIG"
else
    say_fail "config missing: $CONFIG"
    PREFLIGHT_OK=0
fi
if [ -d "$IPC" ] && [ -w "$IPC" ]; then
    say_ok "state/ipc writable: $IPC"
else
    say_skip "state/ipc not present/writable yet: $IPC (the daemon creates it at startup)"
fi

HAVE_VENV=0
if [ -x "$VENV_PY" ]; then
    say_ok "venv python present: $VENV_PY"
    HAVE_VENV=1
else
    say_skip "venv python missing: $VENV_PY — cannot start the inference server here"
fi
HAVE_BIN=0
if [ -x "$DARWIND" ]; then
    say_ok "daemon binary present: $DARWIND"
    HAVE_BIN=1
else
    say_skip "daemon binary missing: $DARWIND — cargo build --release first"
fi
HAVE_SERVER=0
if [ -f "$SERVER_PY" ]; then
    HAVE_SERVER=1
else
    say_skip "inference/server.py missing: $SERVER_PY"
fi

if [ "$PREFLIGHT_OK" -eq 0 ]; then
    say_fail "preflight failed — refusing to proceed"
    SMOKE_STAGE="fail"
    INF_STAGE="fail"; DMN_STAGE="fail"; TEL_STAGE="fail"
    # fall through to the report (trap tears down nothing — we started nothing)
fi

# =============================================================================
# STAGE 1 — inference server (start it first, OR detect already-up)
# =============================================================================
if [ "$PREFLIGHT_OK" -eq 1 ]; then
if [ "$_UI" -eq 1 ]; then ui_stage 2 4 "INFERENCE" 2>/dev/null || say_hr; else say_hr; fi
if unix_connectable "$INF_SOCK"; then
    say_ok "inference server already reachable at $INF_SOCK (leaving it running)"
    INF_STAGE="pass"
elif [ "$MODE" = "no-start" ]; then
    say_skip "inference server not reachable and --no-start given — not starting it"
    INF_STAGE="skip"
elif [ "$HAVE_VENV" -eq 1 ] && [ "$HAVE_SERVER" -eq 1 ]; then
    say_info "starting inference server: $VENV_PY $SERVER_PY"
    mkdir -p "$LOGS"
    "$VENV_PY" "$SERVER_PY" >>"$INF_LOG" 2>&1 &
    STARTED_INF_PID=$!
    say_info "inference pid $STARTED_INF_PID — waiting up to ${INF_TIMEOUT}s for readiness (cold model load)"
    if poll_until "$INF_TIMEOUT" unix_connectable "$INF_SOCK"; then
        say_ok "inference server is reachable at $INF_SOCK"
        INF_STAGE="pass"
    else
        # Honest: the server refuses to bind without numpy/mlx (exit 2). The
        # probe times out and we report that truthfully, with the log tail.
        if kill -0 "$STARTED_INF_PID" 2>/dev/null; then
            say_fail "inference server did not become ready within ${INF_TIMEOUT}s (still running; see $INF_LOG)"
        else
            say_fail "inference server exited before readiness (missing numpy/mlx? see $INF_LOG)"
            STARTED_INF_PID=""   # already dead; nothing for teardown to kill
        fi
        INF_STAGE="fail"
    fi
else
    say_skip "no venv/server.py — cannot start the inference server in this tree (no-model branch)"
    INF_STAGE="skip"
fi
fi

# =============================================================================
# STAGE 2 — daemon (start it second, OR detect already-up)
# =============================================================================
if [ "$PREFLIGHT_OK" -eq 1 ]; then
if [ "$_UI" -eq 1 ]; then ui_stage 3 4 "DAEMON" 2>/dev/null || say_hr; else say_hr; fi
if unix_connectable "$CMD_SOCK" && [ -f "$CMD_TOKEN" ]; then
    say_ok "daemon command channel already up at $CMD_SOCK (leaving it running)"
    DMN_STAGE="pass"
elif [ "$MODE" = "no-start" ]; then
    say_skip "daemon command channel not up and --no-start given — not starting it"
    DMN_STAGE="skip"
elif [ "$HAVE_BIN" -eq 1 ]; then
    say_info "starting daemon: $DARWIND"
    mkdir -p "$LOGS"
    DARWIN_ROOT="$ROOT" "$DARWIND" >>"$DMN_LOG" 2>&1 &
    STARTED_DMN_PID=$!
    say_info "daemon pid $STARTED_DMN_PID — waiting up to ${DMN_TIMEOUT}s for the command channel + token"
    # Invoked indirectly via poll_until "$@" (shellcheck can't trace it).
    # shellcheck disable=SC2329
    cmd_ready() { unix_connectable "$CMD_SOCK" && [ -f "$CMD_TOKEN" ]; }
    if poll_until "$DMN_TIMEOUT" cmd_ready; then
        say_ok "daemon command channel up at $CMD_SOCK (token handed off)"
        DMN_STAGE="pass"
    else
        if kill -0 "$STARTED_DMN_PID" 2>/dev/null; then
            say_fail "daemon did not bring the command channel up within ${DMN_TIMEOUT}s (still running; see $DMN_LOG)"
        else
            say_fail "daemon exited before the command channel came up (see $DMN_LOG)"
            STARTED_DMN_PID=""
        fi
        DMN_STAGE="fail"
    fi
else
    say_skip "no daemon binary — cannot start darwind in this tree (no-daemon branch)"
    DMN_STAGE="skip"
fi
fi

# =============================================================================
# STAGE 3 — read-only smoke: ONE token-gated `roster` round-trip + telemetry
# =============================================================================
if [ "$PREFLIGHT_OK" -eq 1 ]; then
if [ "$_UI" -eq 1 ]; then ui_stage 4 4 "SMOKE" 2>/dev/null || say_hr; else say_hr; fi
if [ "$DMN_STAGE" = "pass" ] && [ -f "$CMD_TOKEN" ]; then
    PY="$(probe_python)"
    if [ -z "$PY" ]; then
        say_skip "no python available to drive the command-socket smoke"
        SMOKE_STAGE="skip"
    else
        TOKEN="$(cat "$CMD_TOKEN" 2>/dev/null || true)"
        if [ -z "$TOKEN" ]; then
            say_fail "command.token unreadable — cannot authenticate the smoke command"
            SMOKE_STAGE="fail"
        else
            # ONE non-consequential, inference-free verb: `roster` returns the
            # in-memory agent registry. NOT `ask` (that would invoke the model).
            REPLY="$("$PY" - "$CMD_SOCK" "$TOKEN" <<'PY' 2>/dev/null || true
import socket, json, sys
sock, token = sys.argv[1], sys.argv[2]
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(5.0)
try:
    s.connect(sock)
    s.sendall((json.dumps({"token": token, "cmd": "roster"}) + "\n").encode())
    buf = b""
    while b"\n" not in buf:
        chunk = s.recv(65536)
        if not chunk:
            break
        buf += chunk
    s.close()
    resp = json.loads(buf.decode().splitlines()[0])
    print("OK" if resp.get("ok") is True else "NOTOK")
except Exception as e:
    print("ERR:%s" % e)
PY
)"
            case "$REPLY" in
                OK)
                    say_ok "smoke: token-gated 'roster' round-trip returned ok:true (command channel + registry live)"
                    SMOKE_STAGE="pass"
                    ;;
                NOTOK)
                    say_fail "smoke: 'roster' replied ok:false (the command channel answered but the verb failed)"
                    SMOKE_STAGE="fail"
                    ;;
                *)
                    say_fail "smoke: 'roster' round-trip failed (${REPLY:-no reply})"
                    SMOKE_STAGE="fail"
                    ;;
            esac
        fi
    fi

    # Second read-only liveness check: the telemetry WS port should be connectable.
    if tcp_connectable "$TEL_PORT"; then
        say_ok "telemetry websocket reachable on 127.0.0.1:$TEL_PORT"
        TEL_STAGE="pass"
    else
        say_skip "telemetry websocket not reachable on 127.0.0.1:$TEL_PORT (port not bound yet?)"
        TEL_STAGE="skip"
    fi
else
    say_skip "daemon command channel not up — smoke skipped (cannot prove a turn without it)"
    SMOKE_STAGE="skip"
    TEL_STAGE="skip"
fi
fi

# =============================================================================
# REPORT — truthful per-subsystem board + overall verdict
# =============================================================================
say_hr
tag() { case "$1" in pass) printf 'PASS';; skip) printf 'SKIP';; fail) printf 'FAIL';; *) printf 'PENDING';; esac; }
if [ "$_UI" -eq 1 ] && command -v ui_status_board >/dev/null 2>&1; then
    ui_status_board "BRING-UP" \
        "INFERENCE SERVER|$(tag "$INF_STAGE")" \
        "DAEMON CHANNEL|$(tag "$DMN_STAGE")" \
        "IPC SMOKE (roster)|$(tag "$SMOKE_STAGE")" \
        "TELEMETRY WS|$(tag "$TEL_STAGE")" 2>/dev/null || true
else
    printf '\n  BRING-UP STATUS\n'
    printf '    inference server ... %s\n' "$(tag "$INF_STAGE")"
    printf '    daemon channel ..... %s\n' "$(tag "$DMN_STAGE")"
    printf '    ipc smoke (roster) . %s\n' "$(tag "$SMOKE_STAGE")"
    printf '    telemetry ws ....... %s\n' "$(tag "$TEL_STAGE")"
fi

# Verdict: FAIL if any hard FAIL; OK only if the smoke actually PASSED; else
# DEGRADED/SKIPPED — and NEVER claim healthy for a skipped stage.
VERDICT_RC=0
if [ "$INF_STAGE" = "fail" ] || [ "$DMN_STAGE" = "fail" ] || [ "$SMOKE_STAGE" = "fail" ]; then
    say_fail "VERDICT: FAILED — a bring-up stage hard-failed (see above)"
    VERDICT_RC=1
elif [ "$SMOKE_STAGE" = "pass" ]; then
    say_ok "VERDICT: HEALTHY — daemon up and the read-only smoke round-trip passed"
    VERDICT_RC=0
else
    say_skip "VERDICT: SKIPPED — could not bring up + smoke the live pipeline in this environment (honest: nothing was faked)"
    VERDICT_RC=0
fi

exit "$VERDICT_RC"

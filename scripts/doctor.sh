#!/bin/bash
# doctor.sh — read-only DARWIN environment diagnostic (WS2).
#
# Inspects the INSTALLED environment and prints an honest status board. It
# starts NOTHING, stops nothing, changes nothing — every check is a read. It
# resolves the root EXACTLY like the daemon, then reports on: the venv python,
# the daemon binary, on-device models (in BOTH the install models/ cache AND the
# default ~/.cache/huggingface — and flags the HF_HOME runtime split assess
# found), the two live sockets + telemetry port, whether both LaunchAgents are
# loaded, config readability, and TCC (mic / screen-recording) consent hints.
#
# HONESTY CONTRACT: a check that cannot run is reported as UNKNOWN/SKIP with the
# reason — it NEVER claims OK for something it did not verify. The board is
# informational; doctor exits non-zero only if a structural FAULT (no config /
# no root) is found, so it is safe to wire into a health cron.
#
# Usage:
#   scripts/doctor.sh        # print the board
#   scripts/doctor.sh -h
#
# bash 3.2 compatible: no associative arrays, no mapfile, no ${var^^}.
set -euo pipefail

case "${1:-}" in
    ""|--full) : ;;
    -h|--help) sed -n '2,18p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "error: unknown argument '${1}'" >&2; exit 2 ;;
esac

# --- resolve root EXACTLY like the daemon ------------------------------------
if [ -n "${DARWIN_ROOT:-}" ]; then
    ROOT="$DARWIN_ROOT"
else
    ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fi
export DARWIN_ROOT="$ROOT"

# --- optional cinematic UI (graceful plain fallback) -------------------------
_UI=0
if [ -f "$ROOT/scripts/ui.sh" ]; then
    # shellcheck source=scripts/ui.sh
    # shellcheck disable=SC1091
    if . "$ROOT/scripts/ui.sh" 2>/dev/null && command -v ui_init >/dev/null 2>&1; then
        ui_init
        _UI=1
    fi
fi
say_ok()   { if [ "$_UI" -eq 1 ]; then ui_ok   "$1"; else printf '  [ OK ] %s\n' "$1"; fi; }
say_warn() { if [ "$_UI" -eq 1 ]; then ui_warn "$1"; else printf '  [WARN] %s\n' "$1"; fi; }
say_fail() { if [ "$_UI" -eq 1 ]; then ui_err  "$1"; else printf '  [FAIL] %s\n' "$1"; fi; }
say_info() { if [ "$_UI" -eq 1 ]; then ui_info "$1"; else printf '  [ .. ] %s\n' "$1"; fi; }
say_hr()   { if [ "$_UI" -eq 1 ]; then ui_hr; else printf -- '---\n'; fi; }

# Source the gitignored runtime env EXACTLY like the boot wrappers + bringup.sh
# do (read-only for us: it only sets env vars). This is where install.sh
# persists HF_HOME, so the HF_HOME-split check below sees the SAME environment
# the daemon + inference server actually run with — not a bare shell's.
if [ -f "$ROOT/state/env.sh" ]; then
    # shellcheck disable=SC1091
    . "$ROOT/state/env.sh"
fi

# --- derived paths -----------------------------------------------------------
VENV_PY="$ROOT/.venv/bin/python"
DARWIND="$ROOT/daemon/target/release/darwind"
CONFIG="$ROOT/config/darwin.toml"
IPC="$ROOT/state/ipc"
INF_SOCK="$IPC/inference.sock"
CMD_SOCK="$IPC/command.sock"
INSTALL_MODELS="$ROOT/models"
HF_CACHE="${HF_HOME:-$HOME/.cache/huggingface}/hub"
AGENT_DIR="$HOME/Library/LaunchAgents"
GUI_DOMAIN="gui/$(id -u 2>/dev/null || echo 0)"

TEL_PORT="7177"
if [ -f "$CONFIG" ]; then
    _p="$(awk '
        /^\[telemetry\]/ {f=1; next}
        f && /^\[/        {f=0}
        f && /^[[:space:]]*port[[:space:]]*=/ {
            v=$0; sub(/^[^=]*=/, "", v); sub(/#.*/, "", v); gsub(/[^0-9]/, "", v);
            print v; exit
        }' "$CONFIG" 2>/dev/null || true)"
    case "$_p" in ''|*[!0-9]*) : ;; *) TEL_PORT="$_p" ;; esac
fi

probe_python() {
    if [ -x "$VENV_PY" ]; then printf '%s' "$VENV_PY";
    elif command -v python3 >/dev/null 2>&1; then command -v python3;
    else printf ''; fi
}
unix_connectable() {
    local sock="$1" py; py="$(probe_python)"
    if [ -n "$py" ]; then
        "$py" - "$sock" <<'PY' 2>/dev/null
import socket, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.settimeout(1.0)
try: s.connect(sys.argv[1]); s.close(); sys.exit(0)
except OSError: sys.exit(1)
PY
    else [ -S "$sock" ]; fi
}
tcp_connectable() {
    local port="$1" py; py="$(probe_python)"; [ -n "$py" ] || return 2
    "$py" - "$port" <<'PY' 2>/dev/null
import socket, sys
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM); s.settimeout(1.0)
try: s.connect(("127.0.0.1", int(sys.argv[1]))); s.close(); sys.exit(0)
except OSError: sys.exit(1)
PY
}

# Count model dirs (HF cache layout: models--org--name). Echoes the count.
# Glob-based (no ls|grep) so non-alphanumeric names are safe; nullglob-free
# bash 3.2 means an unmatched glob stays literal, so we test each candidate.
count_models() {
    local dir="$1" n=0 entry
    [ -d "$dir" ] || { printf '0'; return; }
    for entry in "$dir"/models--*; do
        [ -e "$entry" ] || continue
        n=$(( n + 1 ))
    done
    printf '%d' "$n"
}

# Is a LaunchAgent loaded into the gui domain? 0=loaded, 1=not, 2=unknown.
agent_loaded() {
    local label="$1"
    command -v launchctl >/dev/null 2>&1 || return 2
    if launchctl print "$GUI_DOMAIN/$label" >/dev/null 2>&1; then return 0; else return 1; fi
}

FAULT=0

say_hr
printf 'DARWIN doctor — read-only environment diagnostic\n'
printf 'root: %s\n' "$ROOT"
say_hr

# --- root + config -----------------------------------------------------------
if [ -d "$ROOT" ] && { [ -f "$CONFIG" ] || [ -d "$ROOT/state" ]; }; then
    say_ok "root resolved: $ROOT"
else
    say_fail "root does not look like a DARWIN tree: $ROOT (no config/darwin.toml, no state/)"
    FAULT=1
fi
if [ -f "$CONFIG" ] && [ -s "$CONFIG" ]; then
    say_ok "config readable: $CONFIG"
else
    say_fail "config missing/empty: $CONFIG"
    FAULT=1
fi

# --- venv + binary -----------------------------------------------------------
VENV_TAG="MISSING"
if [ -x "$VENV_PY" ]; then
    _v="$("$VENV_PY" --version 2>&1 | head -1 || echo python)"
    say_ok "venv python: $VENV_PY ($_v)"
    VENV_TAG="OK"
else
    say_warn "venv python missing: $VENV_PY (python3.11 -m venv .venv)"
fi
BIN_TAG="MISSING"
if [ -x "$DARWIND" ]; then
    say_ok "daemon binary: $DARWIND"
    BIN_TAG="OK"
else
    say_warn "daemon binary not built: $DARWIND (cargo build --release)"
fi

# --- models: check BOTH locations + flag the HF_HOME runtime split -----------
N_INSTALL="$(count_models "$INSTALL_MODELS")"
N_CACHE="$(count_models "$HF_CACHE")"
MODELS_TAG="NONE"
if [ "$N_CACHE" -gt 0 ]; then
    say_ok "models in default HF cache: $N_CACHE under $HF_CACHE (this is what the server reads at runtime)"
    MODELS_TAG="OK"
fi
if [ "$N_INSTALL" -gt 0 ]; then
    if [ -z "${HF_HOME:-}" ]; then
        say_warn "models in install cache: $N_INSTALL under $INSTALL_MODELS — but HF_HOME is UNSET, so the server reads $HF_CACHE instead (the install/runtime HF_HOME split)"
    else
        say_ok "models in install cache: $N_INSTALL under $INSTALL_MODELS (HF_HOME=$HF_HOME)"
    fi
    [ "$MODELS_TAG" = "NONE" ] && MODELS_TAG="SPLIT"
fi
if [ "$N_INSTALL" -eq 0 ] && [ "$N_CACHE" -eq 0 ]; then
    say_warn "no on-device models found in $HF_CACHE or $INSTALL_MODELS (run inference/deploy_models.py) — inference will be unavailable"
fi

# --- sockets + telemetry -----------------------------------------------------
INF_TAG="DOWN"
if unix_connectable "$INF_SOCK"; then
    say_ok "inference socket reachable: $INF_SOCK"
    INF_TAG="UP"
elif [ -S "$INF_SOCK" ]; then
    say_warn "inference socket present but not reachable: $INF_SOCK (server starting or wedged?)"
    INF_TAG="STALE"
else
    say_info "inference socket absent: $INF_SOCK (server not running)"
fi
CMD_TAG="DOWN"
if unix_connectable "$CMD_SOCK"; then
    say_ok "daemon command socket reachable: $CMD_SOCK"
    CMD_TAG="UP"
elif [ -S "$CMD_SOCK" ]; then
    say_warn "command socket present but not reachable: $CMD_SOCK"
    CMD_TAG="STALE"
else
    say_info "command socket absent: $CMD_SOCK (daemon not running)"
fi
TEL_TAG="DOWN"
if tcp_connectable "$TEL_PORT"; then
    say_ok "telemetry websocket reachable: 127.0.0.1:$TEL_PORT"
    TEL_TAG="UP"
else
    rc=$?
    if [ "$rc" -eq 2 ]; then
        say_info "telemetry port $TEL_PORT: no python to probe (skipped)"
        TEL_TAG="UNKNOWN"
    else
        say_info "telemetry websocket not reachable: 127.0.0.1:$TEL_PORT (daemon not running)"
    fi
fi

# --- LaunchAgents ------------------------------------------------------------
INF_AGENT_TAG="UNKNOWN"
DMN_AGENT_TAG="UNKNOWN"
for label in com.darwin.inference com.darwin.daemon; do
    plist="$AGENT_DIR/$label.plist"
    if agent_loaded "$label"; then
        say_ok "LaunchAgent loaded: $label"
        [ "$label" = "com.darwin.inference" ] && INF_AGENT_TAG="LOADED" || DMN_AGENT_TAG="LOADED"
    elif [ "$?" -eq 2 ]; then
        say_info "launchctl unavailable — cannot check $label"
    elif [ -f "$plist" ]; then
        say_warn "LaunchAgent installed but NOT loaded: $label ($plist) — scripts/install_boot.sh --install"
        [ "$label" = "com.darwin.inference" ] && INF_AGENT_TAG="NOTLOADED" || DMN_AGENT_TAG="NOTLOADED"
    else
        say_info "LaunchAgent not installed: $label (manual bring-up only; scripts/install_boot.sh --install to persist)"
        [ "$label" = "com.darwin.inference" ] && INF_AGENT_TAG="ABSENT" || DMN_AGENT_TAG="ABSENT"
    fi
done

# inference plist ThrottleInterval hint (the gap assess flagged).
INF_PLIST="$AGENT_DIR/com.darwin.inference.plist"
if [ -f "$INF_PLIST" ]; then
    if grep -q "ThrottleInterval" "$INF_PLIST" 2>/dev/null; then
        say_ok "com.darwin.inference has ThrottleInterval (crash-loop rate-limited)"
    else
        say_warn "com.darwin.inference has NO ThrottleInterval — a crash-looping server can restart-spam (the daemon plist sets 10)"
    fi
fi

# --- TCC consent hints (read-only; cannot grant) -----------------------------
# We cannot read the TCC DB without consent; we surface the actionable hint so a
# silent-no-mic / no-screen failure is diagnosable. Honest: this is a HINT, not
# a verified state — TCC is device-gated and the daemon cannot grant it.
say_info "TCC: mic + screen-recording consent are device-gated and granted in System Settings > Privacy & Security (the daemon cannot grant them; without mic consent the pipeline hears nothing, without screen-recording the Vision app captures nothing)"

# =============================================================================
# BOARD
# =============================================================================
say_hr
if [ "$_UI" -eq 1 ] && command -v ui_status_board >/dev/null 2>&1; then
    ui_status_board "DARWIN DOCTOR" \
        "VENV PYTHON|$VENV_TAG" \
        "DAEMON BINARY|$BIN_TAG" \
        "ON-DEVICE MODELS|$MODELS_TAG" \
        "INFERENCE SOCKET|$INF_TAG" \
        "COMMAND SOCKET|$CMD_TAG" \
        "TELEMETRY WS|$TEL_TAG" \
        "INFERENCE AGENT|$INF_AGENT_TAG" \
        "DAEMON AGENT|$DMN_AGENT_TAG" 2>/dev/null || true
else
    printf '\n  DARWIN DOCTOR\n'
    printf '    venv python ........ %s\n' "$VENV_TAG"
    printf '    daemon binary ...... %s\n' "$BIN_TAG"
    printf '    on-device models ... %s\n' "$MODELS_TAG"
    printf '    inference socket ... %s\n' "$INF_TAG"
    printf '    command socket ..... %s\n' "$CMD_TAG"
    printf '    telemetry ws ....... %s\n' "$TEL_TAG"
    printf '    inference agent .... %s\n' "$INF_AGENT_TAG"
    printf '    daemon agent ....... %s\n' "$DMN_AGENT_TAG"
fi

if [ "$FAULT" -ne 0 ]; then
    say_fail "doctor found a structural fault (see above) — this does not look like a usable DARWIN root"
    exit 1
fi
say_info "doctor is read-only: it started/stopped/changed nothing. UP/DOWN reflect the moment it ran."
exit 0

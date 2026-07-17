#!/bin/bash
# install_boot.sh — boot-to-DARWIN LaunchAgent installer.
#
# Renders the plist templates in boot/ with the real project root, installs
# them into ~/Library/LaunchAgents, and (re)starts all three agents so the M4
# Mini powers on directly into the DARWIN environment: the inference server, the
# darwind daemon, AND the DARWIN HUD (the visible Tauri app) — so login renders
# the actual DARWIN face, not just the headless backend.
#
# Usage:
#   scripts/install_boot.sh              # DRY RUN: print the plan, change nothing
#   scripts/install_boot.sh --install    # preflight, build daemon, render, lint, bootstrap
#   scripts/install_boot.sh --uninstall  # bootout both agents and remove rendered plists
set -euo pipefail

DARWIN_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

CARGO="$HOME/.cargo/bin/cargo"
[ -x "$CARGO" ] || CARGO="$(command -v cargo || true)"
AGENT_DIR="$HOME/Library/LaunchAgents"
# Load order = boot order: inference first (the daemon needs it), then the
# daemon, then the HUD (the visible app comes up once its backend is live).
LABELS=("com.darwin.inference" "com.darwin.daemon" "com.darwin.hud")
GUI_DOMAIN="gui/$(id -u)"

# Locate the built DARWIN.app the HUD agent will exec (via boot/run_hud.sh).
# Same preference order as run_hud.sh: the bundle built under this DARWIN_ROOT
# first (matches this tree), then the installed copies. Echoes the path, or
# empty if none is found yet.
locate_darwin_app() {
    local found cand
    found="$(find "$DARWIN_ROOT/hud/src-tauri/target/release/bundle" \
        -maxdepth 2 -type d -name 'DARWIN.app' 2>/dev/null | head -1 || true)"
    if [ -n "$found" ]; then printf '%s' "$found"; return 0; fi
    for cand in "/Applications/DARWIN.app" "$HOME/Applications/DARWIN.app"; do
        if [ -d "$cand" ]; then printf '%s' "$cand"; return 0; fi
    done
    return 0
}

MODE="dry-run"
case "${1:-}" in
    "")            MODE="dry-run" ;;
    --install)     MODE="install" ;;
    --uninstall)   MODE="uninstall" ;;
    -h|--help)
        sed -n '2,11p' "${BASH_SOURCE[0]}"
        exit 0
        ;;
    *)
        echo "error: unknown argument '${1}' (expected --install, --uninstall, or no args for dry run)" >&2
        exit 1
        ;;
esac

# launchctl bootout returns before teardown completes; bootstrapping the same
# label immediately can flake ("Bootstrap failed: 5: Input/output error" /
# "already loaded"), which would abort this script half-installed under
# set -e. Poll until the service is actually gone (short timeout).
wait_for_bootout() {
    local label="$1"
    local tries=0
    while launchctl print "$GUI_DOMAIN/$label" >/dev/null 2>&1; do
        tries=$((tries + 1))
        if [ "$tries" -ge 50 ]; then
            echo "error: $GUI_DOMAIN/$label still registered ~10s after bootout" >&2
            return 1
        fi
        sleep 0.2
    done
}

post_install_checklist() {
    cat <<EOF

Post-install checklist (boot-to-DARWIN):
  1. Enable auto-login: System Settings > Users & Groups > Automatically log in as
     this user. Without it the Mini stops at the login window and launchd never
     starts the gui domain agents (including the HUD, which needs the Aqua/GUI
     session). This is a GUIDED MANUAL step — it is a security/credential setting
     and is intentionally NOT automated by this installer.
  2. Cloud fallback key: put 'export ANTHROPIC_API_KEY=...' in
     $DARWIN_ROOT/state/env.sh and chmod 600 it (state/ is gitignored).
  3. The DARWIN HUD now autostarts (com.darwin.hud) — after auto-login the Mini
     powers on directly into the visible DARWIN app, not just the backend. It
     opens as a normal window; the fullscreen "kiosk takeover" stays an EXPLICIT
     in-HUD action (never auto-entered) and its exit is always reachable.
  4. Optional cosmetic de-macOS-ing (hide the Dock behind the HUD window):
       defaults write com.apple.dock autohide -bool true && killall Dock
EOF
}

if [ "$MODE" = "dry-run" ]; then
    cat <<EOF
DRY RUN — no changes made. Re-run with --install to execute, --uninstall to remove.

Resolved DARWIN_ROOT: $DARWIN_ROOT

Plan for --install:
  1. Preflight: require $DARWIN_ROOT/.venv/bin/python (the inference agent
     would otherwise crash-loop every ~10s under KeepAlive) AND a built
     DARWIN.app for the HUD agent (built by ./install.sh stage 5; the HUD
     agent would likewise crash-loop without it).
  2. Build the release daemon, then verify the binary exists:
       $CARGO build --release --manifest-path "$DARWIN_ROOT/daemon/Cargo.toml"
  3. Render plist templates (sed 's|__DARWIN_ROOT__|$DARWIN_ROOT|g'):
       $DARWIN_ROOT/boot/com.darwin.inference.plist -> $AGENT_DIR/com.darwin.inference.plist
       $DARWIN_ROOT/boot/com.darwin.daemon.plist    -> $AGENT_DIR/com.darwin.daemon.plist
       $DARWIN_ROOT/boot/com.darwin.hud.plist       -> $AGENT_DIR/com.darwin.hud.plist
  4. Lint each rendered plist: plutil -lint <plist>
  5. For each agent (inference, then daemon, then hud):
       launchctl bootout $GUI_DOMAIN/<label> 2>/dev/null || true
       poll 'launchctl print $GUI_DOMAIN/<label>' until the service is gone (<=10s)
       launchctl bootstrap $GUI_DOMAIN $AGENT_DIR/<label>.plist   # RunAtLoad starts it
  6. Print the post-install checklist (auto-login, state/env.sh API key,
     optional Dock autohide — the HUD now autostarts too, so login renders it).

Plan for --uninstall:
  For each of: ${LABELS[*]}
       launchctl bootout $GUI_DOMAIN/<label> 2>/dev/null || true
       rm -f $AGENT_DIR/<label>.plist
EOF
    exit 0
fi

if [ "$MODE" = "uninstall" ]; then
    for label in "${LABELS[@]}"; do
        echo "==> bootout $GUI_DOMAIN/$label"
        launchctl bootout "$GUI_DOMAIN/$label" 2>/dev/null || true
        echo "==> rm -f $AGENT_DIR/$label.plist"
        rm -f "$AGENT_DIR/$label.plist"
    done
    echo "Uninstalled. DARWIN LaunchAgents removed; auto-login (if enabled) is untouched."
    exit 0
fi

# --- MODE = install -----------------------------------------------------------

# Preflight: both agents run KeepAlive=true, so a missing executable becomes a
# silent ~10s crash-loop behind a successful-looking install. Fail early instead.
echo "==> Preflight checks"
VENV_PYTHON="$DARWIN_ROOT/.venv/bin/python"
if [ ! -x "$VENV_PYTHON" ]; then
    echo "error: $VENV_PYTHON missing — set up the venv per the README Quick start" >&2
    echo "       before installing boot agents." >&2
    exit 1
fi
if [ ! -x "$CARGO" ]; then
    echo "error: cargo not found (looked in \$HOME/.cargo/bin and PATH) — install the Rust" >&2
    echo "       toolchain (https://rustup.rs) before installing boot agents." >&2
    exit 1
fi
# The HUD agent execs the built DARWIN.app (via boot/run_hud.sh). This script
# builds the daemon but NOT the (heavy, node/Tauri) HUD — that is ./install.sh
# stage 5's job. So, symmetric to the venv check, REQUIRE the app to already
# exist rather than bootstrap a com.darwin.hud that just crash-loops on exit 78.
HUD_APP="$(locate_darwin_app)"
if [ -z "$HUD_APP" ]; then
    echo "error: no built DARWIN.app found for the HUD agent (looked under" >&2
    echo "       $DARWIN_ROOT/hud/src-tauri/target/release/bundle, /Applications, ~/Applications)." >&2
    echo "       Build it first with ./install.sh (stage 5 builds the HUD), then re-run." >&2
    exit 1
fi
echo "    HUD app: $HUD_APP"

echo "==> Building release daemon"
"$CARGO" build --release --manifest-path "$DARWIN_ROOT/daemon/Cargo.toml"

DARWIND_BIN="$DARWIN_ROOT/daemon/target/release/darwind"
if [ ! -x "$DARWIND_BIN" ]; then
    echo "error: $DARWIND_BIN missing after build" >&2
    exit 1
fi

mkdir -p "$AGENT_DIR" "$DARWIN_ROOT/state/logs"

for label in "${LABELS[@]}"; do
    template="$DARWIN_ROOT/boot/$label.plist"
    rendered="$AGENT_DIR/$label.plist"
    echo "==> Rendering $template -> $rendered"
    sed "s|__DARWIN_ROOT__|$DARWIN_ROOT|g" "$template" > "$rendered"
    echo "==> Linting $rendered"
    plutil -lint "$rendered"
done

for label in "${LABELS[@]}"; do
    rendered="$AGENT_DIR/$label.plist"
    echo "==> Loading $label"
    launchctl bootout "$GUI_DOMAIN/$label" 2>/dev/null || true
    wait_for_bootout "$label"
    # RunAtLoad=true starts the agent at bootstrap; no kickstart needed (a
    # kickstart -k here would kill the inference server mid model-preload).
    launchctl bootstrap "$GUI_DOMAIN" "$rendered"
done

echo "Install complete: all three agents bootstrapped (RunAtLoad starts them: inference + daemon + HUD)."
post_install_checklist

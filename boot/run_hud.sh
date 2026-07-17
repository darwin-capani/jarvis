#!/bin/bash
# DARWIN boot wrapper: the DARWIN HUD (the Tauri app — the VISIBLE DARWIN
# environment). Invoked by the com.darwin.hud LaunchAgent. Resolves the project
# root from its own location so the plist only needs to point at this script, then
# execs the built DARWIN.app so login renders the HUD (not just the headless
# daemon + inference backend).
set -euo pipefail

DARWIN_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$DARWIN_ROOT"

# Gitignored secrets / overrides (e.g. export ANTHROPIC_API_KEY=...), same as the
# daemon/inference wrappers — so the HUD process inherits the same environment.
if [ -f "$DARWIN_ROOT/state/env.sh" ]; then
    # shellcheck disable=SC1091
    source "$DARWIN_ROOT/state/env.sh"
fi

export DARWIN_ROOT

# Locate the built DARWIN.app. Preference order:
#   1. the bundle built under THIS DARWIN_ROOT (guaranteed to match this tree) —
#      found the same way install.sh does (find under the Tauri bundle dir);
#   2. the copies place_hud_app installs into /Applications, then ~/Applications.
locate_darwin_app() {
    local found cand
    found="$(find "$DARWIN_ROOT/hud/src-tauri/target/release/bundle" \
        -maxdepth 2 -type d -name 'DARWIN.app' 2>/dev/null | head -1 || true)"
    if [ -n "$found" ]; then printf '%s' "$found"; return 0; fi
    for cand in "/Applications/DARWIN.app" "$HOME/Applications/DARWIN.app"; do
        if [ -d "$cand" ]; then printf '%s' "$cand"; return 0; fi
    done
    return 1
}

# Guardrail: with KeepAlive=true, a missing app would otherwise be a silent ~10s
# crash-loop spamming state/logs/launchd-hud.log. Fail loudly instead.
APP="$(locate_darwin_app || true)"
if [ -z "$APP" ]; then
    echo "error: DARWIN.app not found under $DARWIN_ROOT/hud/src-tauri/target/release/bundle, /Applications, or ~/Applications — build it with ./install.sh (stage 5 builds the HUD), then re-run scripts/install_boot.sh --install" >&2
    exit 78  # EX_CONFIG
fi

# The Mach-O to exec is the bundle's CFBundleExecutable (productName "DARWIN").
# Read it from the app's Info.plist so a future rename never silently breaks boot;
# fall back to "DARWIN" if the key can't be read.
EXECNAME="$(plutil -extract CFBundleExecutable raw -o - "$APP/Contents/Info.plist" 2>/dev/null || true)"
[ -n "$EXECNAME" ] || EXECNAME="DARWIN"
BIN="$APP/Contents/MacOS/$EXECNAME"
if [ ! -x "$BIN" ]; then
    echo "error: $BIN missing or not executable inside $APP — the bundle looks incomplete; rebuild the HUD with ./install.sh" >&2
    exit 78  # EX_CONFIG
fi

# exec the app binary DIRECTLY (not `open -a DARWIN`): under launchd KeepAlive the
# job's process must BE the app, so a quit or crash is a clean, throttled relaunch.
# `open` would detach the app and let THIS wrapper exit immediately, turning
# KeepAlive into a ~10s crash-loop that re-spawns `open` forever.
exec "$BIN"

#!/bin/bash
# uninstall.sh — completely remove J.A.R.V.I.S. from this machine.
#
# TWO-STEP TYPED CONFIRMATION (deliberate, for a destructive action):
#   1. "Delete JARVIS completely? (yes/no)"            -> no cancels.
#   2. "Are you ABSOLUTELY sure? (yes/no)"             -> no cancels; only yes deletes.
# Any unrecognized / empty / EOF input is treated as NO — it never deletes on doubt.
#
# It removes ONLY JARVIS's own footprint. Every target is a SPECIFIC, guarded path
# (never a broad or globbed rm):
#   - the install home  ~/Library/Application Support/JARVIS  (code, .venv, models, all state)
#   - the 2 LaunchAgents (com.jarvis.daemon / com.jarvis.inference) — unloaded + removed
#   - the installed HUD app  /Applications/JARVIS.app  and  ~/Applications/JARVIS.app —
#     each removed ONLY after its Info.plist verifies bundle id com.jarvis.hud
#   - the JARVIS Keychain items (ONLY the service "com.jarvis.daemon") — your stored keys/tokens
#   - the logs  ~/Library/Logs/JARVIS
# It removes the INSTALLED OS. A source clone you may have elsewhere is left untouched.
#
# Usage:
#   ~/Library/Application\ Support/JARVIS/uninstall.sh   # interactive, two-step confirm
#   ./uninstall.sh --dry-run                             # show what WOULD be removed; delete nothing
#   ./uninstall.sh --help
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# --- UI: prefer a sibling scripts/ui.sh, else a bundled ui.sh; else a no-op shim
# so uninstall always works even from a bare copy. --------------------------------
for _ui in "$SCRIPT_DIR/scripts/ui.sh" "$SCRIPT_DIR/ui.sh"; do
    if [ -f "$_ui" ]; then
        # shellcheck disable=SC1090
        . "$_ui"
        break
    fi
done
if ! command -v ui_init >/dev/null 2>&1; then
    ui_init() { :; }
    jarvis_banner() { printf '\n  J.A.R.V.I.S. — uninstall\n\n'; }
    ui_hr() { printf -- '  ------------------------------------------------------------\n'; }
    ui_ok()   { printf '  [ok]  %s\n' "$1"; }
    ui_warn() { printf '  [!]   %s\n' "$1"; }
    ui_err()  { printf '  [x]   %s\n' "$1" >&2; }
    ui_info() { printf '  -     %s\n' "$1"; }
    ui_note() { printf '        %s\n' "$1"; }
    ui_online() { :; }
fi
ui_init

# --- the JARVIS footprint (specific, hard-coded paths) ---------------------------
JARVIS_HOME="$HOME/Library/Application Support/JARVIS"
LOG_DIR="$HOME/Library/Logs/JARVIS"
AGENT_DIR="$HOME/Library/LaunchAgents"
KEYCHAIN_SERVICE="com.jarvis.daemon"
LABELS=("com.jarvis.daemon" "com.jarvis.inference")
GUI_DOMAIN="gui/$(id -u)"
# The HUD app install.sh places via place_hud_app (/Applications first, then
# ~/Applications). Each is a SPECIFIC path, and is removed ONLY if its bundle
# identifier verifies as the JARVIS HUD (hud/src-tauri/tauri.conf.json).
HUD_BUNDLE_ID="com.jarvis.hud"
APP_PATHS=("/Applications/JARVIS.app" "$HOME/Applications/JARVIS.app")

DRY_RUN=0
case "${1:-}" in
    --dry-run|--check) DRY_RUN=1 ;;
    -h|--help) sed -n '2,22p' "${BASH_SOURCE[0]}"; exit 0 ;;
    "") : ;;
    *) printf 'uninstall.sh: unknown argument %q (use --dry-run or --help)\n' "$1" >&2; exit 2 ;;
esac

# --- SAFETY GUARD: refuse to act unless the home is EXACTLY the expected path. ----
# Makes a broad/accidental delete impossible even if $HOME were malformed: the only
# directory this script will ever rm -rf is literally ~/Library/Application Support/JARVIS.
guard_home() {
    local base="$HOME/Library/Application Support/JARVIS"
    case "$JARVIS_HOME" in
        "" | "/" | "$HOME" | "$HOME/" | "$HOME/Library" | "$HOME/Library/" \
            | "$HOME/Library/Application Support" | "$HOME/Library/Application Support/")
            ui_err "Refusing to run: the install path resolves to a protected directory."
            exit 1 ;;
    esac
    if [ "$JARVIS_HOME" != "$base" ]; then
        ui_err "Refusing to run: install path is not the expected ~/Library/Application Support/JARVIS."
        exit 1
    fi
}
guard_home

# --- read yes/no, FAIL-SAFE to NO ------------------------------------------------
# 0 = an explicit yes; 1 = no / empty / EOF / repeated garbage. Empty (just Enter)
# is NO. Re-prompts up to 3 times on unrecognized input, then defaults to NO.
ask_yes_no() {
    local prompt="$1" ans="" tries=0
    while [ "$tries" -lt 3 ]; do
        printf '%s%s%s ' "${UI_BOLD:-}${UI_YELLOW:-}" "$prompt" "${UI_RESET:-}"
        if ! IFS= read -r ans; then
            printf '\n'
            return 1   # EOF -> NO
        fi
        case "$(printf '%s' "$ans" | tr '[:upper:]' '[:lower:]' | tr -d '[:space:]')" in
            yes|y)  return 0 ;;
            no|n|"") return 1 ;;
            *) ui_warn "Please type 'yes' or 'no'."; tries=$((tries + 1)) ;;
        esac
    done
    return 1   # too many invalid answers -> NO (fail-safe)
}

# --- the "confirmation window": exactly what will be removed ----------------------
present_targets() {
    ui_hr
    ui_warn "This will COMPLETELY and PERMANENTLY remove J.A.R.V.I.S. from this Mac."
    ui_hr
    ui_info "The following will be deleted:"
    if [ -d "$JARVIS_HOME" ]; then
        local size; size="$(du -sh "$JARVIS_HOME" 2>/dev/null | cut -f1 || true)"
        ui_note "$JARVIS_HOME"
        ui_note "    └ the OS, code, .venv, all models, and all state${size:+  (~$size)}"
    else
        ui_note "$JARVIS_HOME  (not installed — nothing to remove there)"
    fi
    ui_note "LaunchAgents: ${LABELS[*]}  (autostart unloaded + removed)"
    local app
    for app in "${APP_PATHS[@]}"; do
        [ -d "$app" ] && ui_note "HUD app: $app  (removed only if it verifies as bundle $HUD_BUNDLE_ID)"
    done
    ui_note "Keychain items under service \"$KEYCHAIN_SERVICE\"  (your stored API keys / tokens)"
    ui_note "Logs: $LOG_DIR"
    ui_hr
    ui_info "This removes the INSTALLED OS (~/Library/...). A source clone elsewhere is untouched."
    ui_hr
}

# --- destructive steps (each is a no-op that only PRINTS in --dry-run) ------------
stop_and_remove_agents() {
    if [ "$DRY_RUN" -eq 1 ]; then
        ui_note "[dry run] would unload + remove LaunchAgents: ${LABELS[*]}"
        return 0
    fi
    # Prefer the project's own boot uninstaller (single source of truth) if present.
    local boot="$SCRIPT_DIR/scripts/install_boot.sh"
    if [ -x "$boot" ]; then
        "$boot" --uninstall || ui_warn "boot uninstaller reported an issue (continuing)."
    else
        for label in "${LABELS[@]}"; do
            launchctl bootout "$GUI_DOMAIN/$label" 2>/dev/null || true
            rm -f "$AGENT_DIR/$label.plist"
        done
    fi
    # Best-effort: reap any still-running processes (never fatal).
    pkill -f "JARVIS/daemon/target/release/jarvisd" 2>/dev/null || true
    pkill -f "JARVIS/inference/server.py" 2>/dev/null || true
    ui_ok "Autostart unloaded and LaunchAgents removed."
}

# Is $1 the JARVIS HUD app bundle? TRUE only when it is a directory whose
# Info.plist carries the JARVIS bundle identifier — we never rm -rf a path that
# does not verify, even at the expected location (e.g. some unrelated folder a
# user parked there under the name JARVIS.app).
is_jarvis_hud_app() {
    local app="$1"
    [ -d "$app" ] && [ -f "$app/Contents/Info.plist" ] || return 1
    local bid=""
    bid="$(defaults read "$app/Contents/Info" CFBundleIdentifier 2>/dev/null || true)"
    [ "$bid" = "$HUD_BUNDLE_ID" ]
}

remove_hud_app() {
    local app found=0
    for app in "${APP_PATHS[@]}"; do
        [ -e "$app" ] || continue
        found=1
        if ! is_jarvis_hud_app "$app"; then
            ui_warn "left in place: $app (did not verify as the JARVIS HUD bundle $HUD_BUNDLE_ID — refusing to delete an unverified path)"
            continue
        fi
        if [ "$DRY_RUN" -eq 1 ]; then
            ui_note "[dry run] would: rm -rf \"$app\"  (verified bundle $HUD_BUNDLE_ID)"
            continue
        fi
        # Best-effort: quit a running HUD first (never fatal) — scoped to an
        # executable INSIDE a JARVIS.app bundle, like the daemon reaps above.
        pkill -f "/JARVIS.app/Contents/MacOS/" 2>/dev/null || true
        rm -rf "$app"
        ui_ok "Removed the HUD app: $app"
    done
    if [ "$found" -eq 0 ]; then
        if [ "$DRY_RUN" -eq 1 ]; then
            ui_note "[dry run] no JARVIS.app present in /Applications or ~/Applications"
        else
            ui_info "No installed JARVIS.app was present."
        fi
    fi
}

remove_home() {
    if [ "$DRY_RUN" -eq 1 ]; then
        ui_note "[dry run] would: rm -rf \"$JARVIS_HOME\""
        return 0
    fi
    guard_home   # re-assert the guard immediately before the only rm -rf
    cd "$HOME"   # never rm -rf the directory we are standing in
    if [ -d "$JARVIS_HOME" ]; then
        rm -rf "$JARVIS_HOME"
        ui_ok "Removed the install home."
    else
        ui_info "Install home was not present."
    fi
}

remove_keychain_items() {
    if [ "$DRY_RUN" -eq 1 ]; then
        ui_note "[dry run] would delete every Keychain item under service \"$KEYCHAIN_SERVICE\""
        return 0
    fi
    # Delete one matching generic-password per call; loop until none remain. Scoped
    # STRICTLY to the JARVIS service, so no other Keychain item is ever touched.
    local removed=0
    while security delete-generic-password -s "$KEYCHAIN_SERVICE" >/dev/null 2>&1; do
        removed=$((removed + 1))
        [ "$removed" -ge 64 ] && break   # safety stop; JARVIS never stores this many
    done
    if [ "$removed" -gt 0 ]; then
        ui_ok "Removed $removed JARVIS Keychain item(s) (service $KEYCHAIN_SERVICE)."
    else
        ui_info "No JARVIS Keychain items were present."
    fi
}

remove_logs() {
    if [ "$DRY_RUN" -eq 1 ]; then
        ui_note "[dry run] would: rm -rf \"$LOG_DIR\""
        return 0
    fi
    if [ -d "$LOG_DIR" ]; then
        rm -rf "$LOG_DIR"
        ui_ok "Removed logs."
    fi
}

# --- main ------------------------------------------------------------------------
clear 2>/dev/null || true
jarvis_banner
present_targets

if [ "$DRY_RUN" -eq 1 ]; then
    ui_warn "DRY RUN — nothing will be deleted no matter what you answer."
fi

# STEP 1
if ! ask_yes_no "Delete JARVIS completely? (yes/no)"; then
    ui_ok "Cancelled. Nothing was deleted."
    exit 0
fi

# STEP 2
if ! ask_yes_no "This is PERMANENT and cannot be undone. Are you ABSOLUTELY sure? (yes/no)"; then
    ui_ok "Cancelled. Nothing was deleted."
    exit 0
fi

# Both confirmations were an explicit yes — proceed.
ui_hr
ui_info "Removing J.A.R.V.I.S. ..."
stop_and_remove_agents
remove_hud_app
remove_home
remove_keychain_items
remove_logs
ui_hr
if [ "$DRY_RUN" -eq 1 ]; then
    ui_ok "Dry run complete — the above is exactly what a real run would remove. Nothing was deleted."
else
    ui_ok "J.A.R.V.I.S. has been completely removed from this machine."
    ui_note "Thank you."
fi
exit 0

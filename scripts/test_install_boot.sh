#!/bin/bash
# Hermetic selftest for the boot-to-DARWIN LaunchAgent installer (install_boot.sh)
# and the three boot wrappers it renders/loads.
#
# WHAT IT GUARDS: that all THREE agents — com.darwin.inference, com.darwin.daemon,
# AND com.darwin.hud (the HUD, the visible DARWIN app) — are:
#   * RENDERED  — each boot/<label>.plist template renders (sed) to a well-formed
#                 plist with the project root substituted and NO residual
#                 __DARWIN_ROOT__ token (plutil -lint, when plutil is available);
#   * LOADED    — the --install PLAN enumerates a launchctl bootstrap for each of
#                 the three labels (in boot order: inference, daemon, hud);
#   * UNLOADED  — the --uninstall PLAN enumerates each of the three labels for
#                 bootout + removal.
# Plus HUD-specific well-formedness: the rendered com.darwin.hud plist carries
# RunAtLoad, KeepAlive, LimitLoadToSessionType=Aqua (a GUI app needs the Aqua
# session), and points at boot/run_hud.sh.
#
# THE ONE HARD PROHIBITION: this selftest NEVER calls launchctl, never writes to
# ~/Library/LaunchAgents, never starts an agent, and never builds anything. It
# validates ONLY the pure render + the dry-run PLAN text — the same device-gated
# discipline as the other scripts/test_*.sh harnesses (no exec, no side effects).
#
# Usage:  scripts/test_install_boot.sh        (run from anywhere)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BOOT="$ROOT/boot"
INSTALL_BOOT="$ROOT/scripts/install_boot.sh"

LABELS=("com.darwin.inference" "com.darwin.daemon" "com.darwin.hud")

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1" >&2; exit 1; }

TMP="$(mktemp -d "${TMPDIR:-/tmp}/darwin-boot-selftest.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT INT TERM

# --- 0. syntax: the installer + all three wrappers parse cleanly ------------------
for f in "$INSTALL_BOOT" "$BOOT/run_inference.sh" "$BOOT/run_daemon.sh" "$BOOT/run_hud.sh"; do
    [ -f "$f" ] || fail "missing shell file: $f"
    bash -n "$f" || fail "bash -n failed: $f"
done
pass "install_boot.sh + run_inference.sh + run_daemon.sh + run_hud.sh all parse (bash -n)"

# --- 1. the installer knows all THREE agents (LABELS) -----------------------------
# Assert the third agent is registered in the installer's LABELS array (not just
# the template on disk) so it is actually rendered + loaded + unloaded.
for label in "${LABELS[@]}"; do
    grep -Eq "LABELS=\(.*${label}.*\)" "$INSTALL_BOOT" \
        || fail "$label not present in install_boot.sh LABELS array"
done
pass "install_boot.sh LABELS registers all three agents (inference, daemon, hud)"

# --- 2. RENDERED: each template renders to a well-formed, fully-substituted plist --
FAKE_ROOT="/tmp/darwin-selftest-root"
for label in "${LABELS[@]}"; do
    tpl="$BOOT/$label.plist"
    [ -f "$tpl" ] || fail "missing plist template: $tpl"
    out="$TMP/$label.plist"
    sed "s|__DARWIN_ROOT__|$FAKE_ROOT|g" "$tpl" > "$out"
    # No residual template token survived the render.
    if grep -q "__DARWIN_ROOT__" "$out"; then
        fail "$label.plist still contains __DARWIN_ROOT__ after render"
    fi
    # The substituted root actually landed.
    grep -q "$FAKE_ROOT" "$out" || fail "$label.plist did not substitute the project root"
    # Well-formedness (only if plutil exists — it does on macOS).
    if command -v plutil >/dev/null 2>&1; then
        plutil -lint "$out" >/dev/null || fail "$label.plist is not a well-formed plist (plutil -lint)"
    fi
done
pass "all three plist templates render to well-formed, fully-substituted plists"

# --- 3. HUD-specific well-formedness ---------------------------------------------
HUD_OUT="$TMP/com.darwin.hud.plist"
grep -q "<key>RunAtLoad</key>" "$HUD_OUT"  || fail "hud plist missing RunAtLoad"
grep -q "<key>KeepAlive</key>" "$HUD_OUT"  || fail "hud plist missing KeepAlive"
# A windowed GUI app must be constrained to the Aqua (graphical login) session.
if command -v plutil >/dev/null 2>&1; then
    sess="$(plutil -extract LimitLoadToSessionType raw -o - "$HUD_OUT" 2>/dev/null || true)"
    [ "$sess" = "Aqua" ] || fail "hud plist LimitLoadToSessionType is '$sess', expected Aqua"
else
    grep -A1 "LimitLoadToSessionType" "$HUD_OUT" | grep -q "Aqua" \
        || fail "hud plist LimitLoadToSessionType is not Aqua"
fi
grep -q "$FAKE_ROOT/boot/run_hud.sh" "$HUD_OUT" || fail "hud plist does not exec boot/run_hud.sh"
pass "com.darwin.hud plist: RunAtLoad + KeepAlive + LimitLoadToSessionType=Aqua + run_hud.sh"

# --- 4. LOADED: the --install PLAN enumerates a bootstrap for each label ----------
# The default (no-arg) dry run prints the full --install and --uninstall plans
# without touching launchctl. Assert every label appears in the render/load plan.
DRY="$("$INSTALL_BOOT" 2>&1)"
for label in "${LABELS[@]}"; do
    printf '%s\n' "$DRY" | grep -q "$label" \
        || fail "dry-run plan never mentions $label"
done
# The load loop is described "For each agent (inference, then daemon, then hud)".
printf '%s\n' "$DRY" | grep -qi "then hud" \
    || fail "dry-run --install plan does not describe loading the hud agent"
pass "dry-run --install plan renders + loads all three agents (hud included)"

# --- 5. UNLOADED: the --uninstall PLAN enumerates each label ----------------------
# The uninstall plan prints "For each of: <labels...>"; assert all three are there.
for label in "${LABELS[@]}"; do
    printf '%s\n' "$DRY" | grep -q "For each of:.*$label" \
        || fail "dry-run --uninstall plan does not list $label for bootout+remove"
done
pass "dry-run --uninstall plan unloads + removes all three agents (hud included)"

echo "ALL PASS: boot-to-DARWIN installer renders/loads/unloads inference + daemon + HUD."

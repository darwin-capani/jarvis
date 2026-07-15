#!/bin/bash
# Deploy a validated Self-Forge proposal into the live apps/ tree.
#
# Usage:
#   scripts/apply_forge.sh <ts>          interactive (asks read -r confirmation)
#   scripts/apply_forge.sh <ts> --yes    non-interactive (for the HUD Accept button)
#
#   <ts> is the unix-timestamp directory under state/forge/proposals/ that the
#   forge pipeline announced (forge.proposal telemetry / the first-contact
#   brief / report.md).
#
# The proposed app was validated in a CONFINED staging copy (manifest minimal,
# default-deny SBPL derivable, build + tests green) when it was DRAFTED.
# DEPLOYING it for real is a privileged mutation — it makes DARWIN discover and
# RUN a freshly-authored app — so this script RE-VALIDATES from scratch before
# it ever touches apps/:
#   - verify the proposal lives strictly under state/forge/proposals/<ts>/
#     (refuse anything outside that tree — no path traversal, no symlink escape),
#   - copy the proposed app/<name>/ into a FRESH re-validation staging dir,
#   - RE-CHECK the manifest + permission minimization (no device perms; fs_write
#     only to the app's own state dir; confined fs_read; capped bare-host
#     net_hosts) by handing the manifest to `darwind --validate-forge-manifest`,
#     which runs the SAME forge::validate_manifest gate the draft path runs over
#     the manifest as the daemon's OWN toml parser sees it. This is deliberately
#     NOT a textual scan: a text scan is a TOML parser-differential (it can't see
#     top-level dotted keys like `permissions.gpu = true` or an inline table
#     `permissions = { gpu = true }`, both of which parse clean on the daemon and
#     grant the over-broad permission at launch), so a hand-edited proposal cannot
#     smuggle a wider grant past deploy,
#   - rebuild + retest the app in the staging copy (cargo check + cargo test for
#     a Rust app, py_compile for python),
#   - and ONLY on green move the app into apps/<name>/ so AppRegistry::discover
#     picks it up on the next darwind start.
# Any gate failure exits non-zero and leaves apps/ untouched.
#
# --yes skips ONLY the read -r prompt: the GUI's two-step confirm replaces the
# human keystroke. Every gate above still runs. There is NO flag that weakens
# the re-validation — that gate is non-negotiable.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROPOSALS="$ROOT/state/forge/proposals"
APPS="$ROOT/apps"
FORGE_ROOT="$ROOT/state/forge"

# Structured progress for the HUD. Stages: revalidating | deploying.
# Terminal line is always exactly one RESULT: ok | RESULT: failed <reason>.
stage() { echo "STAGE: $1"; }
result_ok() { echo "RESULT: ok"; }
# Emit the terminal failure line and exit non-zero. apps/ is NOT modified by any
# path that calls this before the "deploying" stage.
fail() {
  echo "RESULT: failed $1" >&2
  exit 1
}

TS="${1:-}"
MODE_YES=0
# Parse the optional --yes flag (position-independent among args 2+).
for arg in "${@:2}"; do
  case "$arg" in
    --yes) MODE_YES=1 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

if [ -z "$TS" ]; then
  echo "usage: $0 <ts> [--yes]" >&2
  if [ -d "$PROPOSALS" ] && [ -n "$(ls -A "$PROPOSALS" 2>/dev/null)" ]; then
    echo "pending proposals:" >&2
    ls -1 "$PROPOSALS" >&2
  else
    echo "(no pending proposals under state/forge/proposals/)" >&2
  fi
  exit 1
fi

# Validate <ts> is a plausible numeric stamp BEFORE it is ever used as a path
# component — digits only, no slashes, no dots, no "..". Path traversal is then
# impossible (the GUI passes ts straight through, so this guard is load-bearing).
case "$TS" in
  '' | *[!0-9]*)
    echo "invalid timestamp '$TS' (must be digits only)" >&2
    exit 2
    ;;
esac

DIR="$PROPOSALS/$TS"
if [ ! -d "$DIR" ]; then
  if [ "$MODE_YES" -eq 1 ]; then
    fail "no proposal at state/forge/proposals/$TS"
  fi
  echo "no proposal at $DIR" >&2
  exit 1
fi

# The proposed app lives under <proposal>/app/<name>/ (exactly one app dir).
APP_PARENT="$DIR/app"
if [ ! -d "$APP_PARENT" ]; then
  fail "proposal $TS has no app/ payload"
fi
# Exactly one app directory is expected.
APP_NAME="$(cd "$APP_PARENT" && ls -1)"
if [ -z "$APP_NAME" ] || [ "$(echo "$APP_NAME" | wc -l | tr -d ' ')" != "1" ]; then
  fail "proposal $TS must contain exactly one app under app/ (found: $(echo "$APP_NAME" | tr '\n' ' '))"
fi
APP_SRC="$APP_PARENT/$APP_NAME"

# The app name must be a safe identifier (lowercase letters/digits/single
# hyphens, starts with a letter) — the SAME rule forge.rs::is_safe_app_name
# enforces, re-checked here so a tampered proposal name cannot become a path or
# a shell surprise. This also makes the apps/<name> destination safe.
case "$APP_NAME" in
  [a-z]*[a-z0-9]) : ;;  # starts lowercase letter, ends letter/digit
  *) fail "app name '$APP_NAME' is not a safe identifier" ;;
esac
if ! printf '%s' "$APP_NAME" | grep -Eq '^[a-z][a-z0-9]*(-[a-z0-9]+)*$'; then
  fail "app name '$APP_NAME' is not a safe identifier (lowercase, digits, single hyphens)"
fi

MANIFEST="$APP_SRC/manifest.toml"
if [ ! -f "$MANIFEST" ]; then
  fail "proposal $TS app has no manifest.toml"
fi

# ----------------------------------------------------------- interactive gate
# Interactive mode: show the report + manifest, ask read -r, then fall through
# to the shared re-validate + deploy path. --yes skips ONLY this block.
if [ "$MODE_YES" -eq 0 ]; then
  if [ -f "$DIR/report.md" ]; then
    echo "=== report ($DIR/report.md) ==="
    cat "$DIR/report.md"
    echo
  fi
  echo "=== proposed manifest ($MANIFEST) ==="
  cat "$MANIFEST"
  echo "====================================="

  printf 'Deploy this forged app into %s/%s and let DARWIN discover it on next start? [y/N] ' "$APPS" "$APP_NAME"
  read -r answer
  case "$answer" in
    y | Y | yes | YES) ;;
    *)
      echo "aborted; the proposal is left in place."
      exit 1
      ;;
  esac
fi

# ----------------------------------------------------------- RE-VALIDATION gate
stage "revalidating"

# Refuse to deploy over an existing app of the same name (review/remove by hand).
if [ -e "$APPS/$APP_NAME" ]; then
  fail "apps/$APP_NAME already exists — remove or rename it before deploying a forged app of that name"
fi

# Re-check the permission minimization the daemon enforced at draft time. A
# proposal is a plain directory a human could hand-edit, so the wider grants the
# forge forbids are re-rejected HERE before the app is ever deployed.
#
# CRITICAL: this is decided by the daemon's OWN toml parser, NOT a textual scan.
# A text scan is a TOML parser-differential: it only sees a literal
# `[permissions]` header plus `key = true` / `key = [..]` lines under it, so a
# hand-edited proposal using top-level dotted keys
# (`permissions.fs_write = [...]`, `permissions.gpu = true`) or an inline table
# (`permissions = { gpu = true }`) parses CLEAN on the daemon (which honors the
# over-broad grants at launch) while sliding past every text check. We therefore
# hand the manifest to `darwind --validate-forge-manifest`, which runs the EXACT
# same forge::validate_manifest gate the draft path runs (schema +
# deny_unknown_fields + name == dir + permission minimization + default-deny SBPL
# derivability) over the manifest as the daemon's toml crate actually parses it.
# Any over-broad grant, any escaping read/write, too many net_hosts, a malformed
# host, a device permission, or a parse error -> non-zero exit -> deploy refused,
# apps/ untouched. This gate CANNOT diverge from what the daemon would grant.

# Resolve the daemon binary that runs the gate. By default, build it FRESH from
# the current source before use: install_boot.sh / apply_heal.sh follow the same
# rebuild-before-use posture, and it is load-bearing here — an OLDER on-disk
# binary would not know the `--validate-forge-manifest` flag and would fall
# through to ordinary daemon startup (or otherwise mishandle it), so a stale
# binary could MISS the gate entirely. A fresh build guarantees the gate binary
# matches the source that defines forge::validate_manifest. cargo is already a
# hard dependency below for the staging rebuild/retest, so this adds none.
# Incremental, so a no-op when up to date.
#
# DARWIND_VALIDATE_BIN: an OPTIONAL override pointing at an ALREADY-BUILT darwind
# that implements the gate (used by the hermetic apply_forge.sh test harness,
# which runs the script in a temp ROOT that has no daemon/ tree to build). It
# only changes WHERE the gate binary lives, never WHAT the gate does: the very
# next step PROVES the chosen binary actually rejects an over-broad probe before
# any verdict is trusted, so a wrong/stale override fails closed.
if [ -n "${DARWIND_VALIDATE_BIN:-}" ] && [ -x "${DARWIND_VALIDATE_BIN}" ]; then
  DARWIND_BIN="$DARWIND_VALIDATE_BIN"
else
  if ! (cd "$ROOT/daemon" && cargo build --release --bin darwind); then
    fail "could not build darwind to run the deploy-time permission gate — apps/ NOT modified"
  fi
  DARWIND_BIN="$ROOT/daemon/target/release/darwind"
  if [ ! -x "$DARWIND_BIN" ]; then
    fail "darwind binary missing after build; cannot run the permission gate — apps/ NOT modified"
  fi
fi

# Defense in depth: PROVE this binary actually implements the gate before we
# trust its verdict on the real manifest. A binary too old to know
# `--validate-forge-manifest` would NOT emit the expected REJECTED marker on a
# deliberately over-broad probe (it would start the daemon, exit 0, or print
# nothing), so we fail-closed unless the probe is rejected with the marker. The
# probe is a throwaway over-broad manifest written to the staging area's parent.
PROBE_DIR="$FORGE_ROOT/gate-probe-$TS"
rm -rf "$PROBE_DIR"
mkdir -p "$PROBE_DIR"
PROBE_MANIFEST="$PROBE_DIR/manifest.toml"
cat > "$PROBE_MANIFEST" <<'PROBE_EOF'
permissions.gpu = true

[app]
name = "gateprobe"
version = "0.1.0"
description = "deploy-gate self-test; never deployed"
entry = "gateprobe"
runtime = "binary"
PROBE_EOF
# `|| PROBE_RC=$?` keeps `set -e` from aborting on the EXPECTED non-zero exit of
# a correctly-rejecting probe (a bare `VAR="$(cmd)"` assignment from a failing
# command substitution trips `set -e` before $? can be read).
PROBE_RC=0
PROBE_OUT="$("$DARWIND_BIN" --validate-forge-manifest "$PROBE_MANIFEST" gateprobe 2>&1)" || PROBE_RC=$?
rm -rf "$PROBE_DIR"
if [ "$PROBE_RC" -eq 0 ] || ! printf '%s' "$PROBE_OUT" | grep -q 'FORGE MANIFEST REJECTED'; then
  fail "deploy-gate self-test FAILED: darwind did not reject an over-broad probe manifest (stale or wrong binary?) — apps/ NOT modified"
fi

# Run the gate on the REAL manifest. forge::validate_manifest_file prints
# "FORGE MANIFEST OK: ..." on pass and "FORGE MANIFEST REJECTED: <reason>" on a
# non-zero exit. It parses with the daemon's OWN toml crate, so dotted keys /
# inline tables / multi-line arrays / deny_unknown_fields are all decided exactly
# as the daemon would grant them.
if ! "$DARWIND_BIN" --validate-forge-manifest "$MANIFEST" "$APP_NAME"; then
  fail "manifest failed the forge permission-minimization gate (see FORGE MANIFEST REJECTED above) — apps/ NOT modified"
fi

# Re-build + re-test the app in a FRESH staging copy. NOTHING touches apps/ until
# this is green. The build/test runs in the copy, never the live tree.
STAGING="$FORGE_ROOT/apply-staging-$TS"
rm -rf "$STAGING"
mkdir -p "$STAGING"
cp -R "$APP_SRC/." "$STAGING/"

RUNTIME="$(grep -E '^[[:space:]]*runtime[[:space:]]*=' "$MANIFEST" | head -1 | grep -oE '"[^"]*"' | tr -d '"')"
case "$RUNTIME" in
  binary|node)
    if [ ! -f "$STAGING/Cargo.toml" ]; then
      fail "runtime '$RUNTIME' app has no Cargo.toml to build/test"
    fi
    if ! (cd "$STAGING" && cargo check); then
      fail "cargo check failed in re-validation staging — apps/ NOT modified"
    fi
    if ! (cd "$STAGING" && cargo test); then
      fail "cargo test failed in re-validation staging — apps/ NOT modified"
    fi
    ;;
  python)
    found_py=0
    while IFS= read -r pyf; do
      found_py=1
      if ! (cd "$STAGING" && python3 -m py_compile "$pyf"); then
        fail "py_compile failed for $pyf — apps/ NOT modified"
      fi
    done < <(cd "$STAGING" && find . -name '*.py')
    if [ "$found_py" -eq 0 ]; then
      fail "python app has no .py files"
    fi
    ;;
  *)
    fail "unknown runtime '$RUNTIME' in manifest"
    ;;
esac

# ----------------------------------------------------------------- deploy
# Green. Move the validated app into apps/<name>/. AppRegistry::discover scans
# apps/ at startup, so the app is picked up on the next darwind start — it is NOT
# started by this script (the operator restarts darwind, then launches the app
# deliberately, e.g. by voice). Born sandboxed: the daemon generates the
# default-deny SBPL from the manifest + mints a capability token at launch.
stage "deploying"
mkdir -p "$APPS"
# Use the validated STAGING copy as the deploy source (it equals the proposal
# app, re-checked) so the deployed tree is exactly what passed the gate. Drop the
# build artifacts (target/) so apps/ stays source-only.
rm -rf "$STAGING/target"
cp -R "$STAGING" "$APPS/$APP_NAME"

# Clear the pending marker so DARWIN stops announcing the proposal.
if command -v sqlite3 >/dev/null 2>&1; then
  sqlite3 "$ROOT/state/darwin.db" "DELETE FROM facts WHERE key = 'meta.forge_pending';" || true
else
  echo "sqlite3 not found; clear the marker manually:" >&2
  echo "  sqlite3 $ROOT/state/darwin.db \"DELETE FROM facts WHERE key = 'meta.forge_pending';\"" >&2
fi

echo "deployed forged app to apps/$APP_NAME — restart darwind so AppRegistry::discover picks it up,"
echo "then launch it deliberately (it is NOT auto-started)."
result_ok

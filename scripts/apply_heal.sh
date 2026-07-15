#!/bin/bash
# Apply a validated self-heal proposal to the live daemon source tree.
#
# Usage:
#   scripts/apply_heal.sh <ts>          interactive (asks read -r confirmation)
#   scripts/apply_heal.sh <ts> --yes    non-interactive (for the HUD Accept button)
#
#   <ts> is the unix-timestamp directory under state/heal/proposals/ that the
#   heal pipeline announced (heal.proposal telemetry / the first-contact
#   brief / report.md).
#
# The proposal was already validated in a staging copy (patch applied,
# cargo check + cargo test green) when it was DRAFTED. Applying for real is a
# privileged mutation of the daemon, so this script RE-VALIDATES from scratch
# before it ever touches daemon/src:
#   - verify state/heal/proposals/<ts>/ exists,
#   - stage a FRESH copy of the daemon sources (src/, Cargo.toml, Cargo.lock —
#     never target/) under state/heal/apply-staging-<ts>/,
#   - apply patch.diff with /usr/bin/patch -p1 --batch (dry-run, then real),
#   - cargo check && cargo test in the staging copy,
#   - and ONLY on green apply the same patch to the real daemon/, rebuild the
#     release binary, and clear the meta.heal_pending marker.
# Any gate failure exits non-zero and leaves daemon/src untouched.
#
# --yes skips ONLY the read -r prompt: the GUI's two-step confirm replaces the
# human keystroke. Every gate above still runs. There is no flag that weakens
# the re-validation — that gate is non-negotiable.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROPOSALS="$ROOT/state/heal/proposals"
DAEMON="$ROOT/daemon"
HEAL_ROOT="$ROOT/state/heal"

# Structured progress for the HUD. Stages: revalidating | applying | rebuilding.
# Terminal line is always exactly one RESULT: ok | RESULT: failed <reason>.
stage() { echo "STAGE: $1"; }
result_ok() { echo "RESULT: ok"; }
# Emit the terminal failure line and exit non-zero. daemon/src is NOT modified
# by any path that calls this before the "applying" stage.
fail() {
  echo "RESULT: failed $1" >&2
  exit 1
}

# --selftest runs the hermetic confinement regression (no daemon / no network /
# no live tree touched) and exits. It guards the de-indentation defense below so
# a future edit cannot silently reopen the out-of-tree-write hole.
if [ "${1:-}" = "--selftest" ]; then
  exec "$(dirname "${BASH_SOURCE[0]}")/test_apply_heal_confinement.sh"
fi

SANDBOX_EXEC="/usr/bin/sandbox-exec"
PATCH_BIN="/usr/bin/patch"
BSD_BASE_PROFILE="/System/Library/Sandbox/Profiles/bsd.sb"

# ---------------------------------------------------------- confined patch
# THE LOAD-BEARING DEFENSE. Run /usr/bin/patch under sandbox-exec with a
# DEFAULT-DENY SBPL profile that allows file-write* ONLY under a single
# canonicalized confinement dir (the patch cwd). The kernel seatbelt then
# physically DENIES any write patch attempts outside that dir — so a tampered
# `..`/Index:/de-indented header CANNOT write out-of-tree, no matter how
# leniently /usr/bin/patch parses the header (this is why the fix is "by
# construction", not by re-deriving patch's header parser, which the two prior
# incomplete fixes tried).
#
# Mechanism is the SAME sandbox-exec/SBPL the daemon's micro-app runtime uses
# (daemon/src/apps.rs): `(version 1)` + `(deny default)` + import Apple's bsd.sb
# base (the syscalls/dyld reads every process needs to boot) + scoped allows.
# We allow file-read* broadly (patch reads the staging files + the patch on
# stdin) and process-fork/exec (patch may fork), but file-write* is confined.
#
# patch writes a TEMP/WORKING file (mkstemp) before renaming it onto the target;
# by default that lives under $TMPDIR (/var/folders/...), which is OUTSIDE the
# confinement dir and would be denied. We redirect patch's TMPDIR into a
# .heal-sandbox-tmp/ dir INSIDE the confinement dir, so the ONLY writable
# location is the confinement subtree — the tempfile is allowed, every
# out-of-tree path (including a `..`-escaped victim) is denied.
#
# Usage: confined_patch <confine_dir> [patch args...]   (patch reads stdin)
# Honors the same -p1 --batch [--dry-run] semantics the callers pass.
confined_patch() {
  local confine_raw="$1"; shift
  # Canonicalize: absolute, symlinks resolved, no trailing slash. A trailing
  # slash or a symlinked parent must not let the subpath filter mismatch what
  # the kernel canonicalizes the write target to. `cd && pwd -P` resolves the
  # whole chain; the dir already exists (we created it just above the caller).
  local confine
  if ! confine="$(cd "$confine_raw" 2>/dev/null && pwd -P)"; then
    fail "confinement dir '$confine_raw' does not resolve — refusing to run patch unsandboxed"
  fi

  # patch's tempfile dir, inside the confinement subtree (so it is writable
  # under the profile without opening any out-of-tree path).
  local tmpd="$confine/.heal-sandbox-tmp"
  mkdir -p "$tmpd"

  # Build the deny-default-write profile. Default-deny everything, import the
  # BSD base so patch can even boot, allow reads + process basics, and allow
  # WRITE only under the canonicalized confinement dir.
  local profile
  profile="$(mktemp -t heal-confine-sbpl)"
  {
    echo "(version 1)"
    echo ";; Generated by apply_heal.sh to confine /usr/bin/patch writes to the"
    echo ";; staging/live tree only. DEFAULT-DENY; the only file-write* grant is"
    echo ";; the canonicalized patch cwd. A '..'/Index/de-indented header that"
    echo ";; resolves outside this subtree is DENIED by the kernel, not the"
    echo ";; pre-scan. Mirrors the micro-app SBPL in daemon/src/apps.rs."
    echo "(deny default)"
    if [ -f "$BSD_BASE_PROFILE" ]; then
      echo "(import \"$BSD_BASE_PROFILE\")"
    fi
    echo "(allow process-fork)"
    echo "(allow process-exec*)"
    # patch reads the staging files + the patch body on stdin; reads are not the
    # threat (the out-of-tree WRITE is), so file-read* is broad.
    echo "(allow file-read*)"
    # The single load-bearing grant: writes confined to the patch cwd subtree
    # (which contains the tempfile dir above). Everything else stays denied.
    echo "(allow file-write* (subpath \"$confine\"))"
  } > "$profile"

  local rc=0
  ( cd "$confine" && TMPDIR="$tmpd" "$SANDBOX_EXEC" -f "$profile" "$PATCH_BIN" "$@" ) || rc=$?
  rm -f "$profile"
  return "$rc"
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
    echo "(no pending proposals under state/heal/proposals/)" >&2
  fi
  exit 1
fi

# Validate <ts> is a plausible numeric stamp BEFORE it is ever used as a path
# component — digits only, no slashes, no dots, no "..". This makes path
# traversal impossible (the GUI passes ts straight through, so this guard is
# load-bearing).
case "$TS" in
  '' | *[!0-9]*)
    echo "invalid timestamp '$TS' (must be digits only)" >&2
    exit 2
    ;;
esac

DIR="$PROPOSALS/$TS"
PATCH_FILE="$DIR/patch.diff"
if [ ! -f "$PATCH_FILE" ]; then
  # In --yes mode this still needs to be a structured RESULT line for the HUD.
  if [ "$MODE_YES" -eq 1 ]; then
    fail "no proposal at state/heal/proposals/$TS (missing patch.diff)"
  fi
  echo "no proposal at $DIR (missing patch.diff)" >&2
  exit 1
fi

# ----------------------------------------------------------- interactive gate
# Interactive mode is UNCHANGED from before: show report + diff, ask read -r,
# then fall through to the shared apply path. --yes skips ONLY this block.
if [ "$MODE_YES" -eq 0 ]; then
  if [ -f "$DIR/report.md" ]; then
    echo "=== report ($DIR/report.md) ==="
    cat "$DIR/report.md"
    echo
  fi

  echo "=== proposed diff ==="
  cat "$PATCH_FILE"
  echo "====================="

  printf 'Apply this patch to %s and rebuild the release binary? [y/N] ' "$DAEMON"
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
# Stage a fresh copy of the daemon sources and re-run patch + cargo check +
# cargo test there. NOTHING touches daemon/src until this is green. This mirrors
# the daemon's draft-time staging (src/, Cargo.toml, Cargo.lock — never target/)
# so a patch that no longer applies, no longer compiles, or fails a test is
# refused here, regardless of what was true when it was drafted.
stage "revalidating"

STAGING="$HEAL_ROOT/apply-staging-$TS"
rm -rf "$STAGING"
mkdir -p "$STAGING"

if [ ! -d "$DAEMON/src" ]; then
  fail "daemon sources not found at $DAEMON/src"
fi
cp -R "$DAEMON/src" "$STAGING/src"
[ -f "$DAEMON/Cargo.toml" ] && cp "$DAEMON/Cargo.toml" "$STAGING/Cargo.toml"
[ -f "$DAEMON/Cargo.lock" ] && cp "$DAEMON/Cargo.lock" "$STAGING/Cargo.lock"

# Path-confinement: /usr/bin/patch is run with `-p1` and cwd = the target dir,
# and macOS patch honors `..` in `---`/`+++` hunk headers — so a header like
# `+++ b/src/../../../../tmp/x` would write OUTSIDE daemon/. Reject any diff whose
# `---`/`+++` target, after the `-p1` strip (drop the first path component), is
# empty, absolute, or contains a `..` component, BEFORE patch ever runs. The
# /dev/null new-file/deleted-file sentinel is exempt. This mirrors the daemon's
# clean_diff() confinement so the human apply path is confined too.
#
# CRITICAL: the header scan MUST see the same headers /usr/bin/patch will. macOS
# `patch` DE-INDENTS a uniformly-indented diff ("Patch is indented N spaces.")
# before reading the `---`/`+++`/`@@` lines, so a column-0-anchored `^(---|+++) `
# grep would miss a header that begins with leading whitespace — the confinement
# loop would never run and an indented `../`-bearing header would write
# out-of-tree. Defense in depth, in order:
#  (a) refuse ANY uniformly-/partially-indented diff up front — legitimate heal
#      diffs emitted by the pipeline are never indented, so an indented patch.diff
#      is itself a tamper signal, and
#  (b) scan headers with a leading-whitespace-tolerant pattern and STRIP that
#      whitespace before extracting the path, so the gate sees exactly what patch
#      will after de-indentation. `Index:` lines are scanned too (a future patch
#      build may select a filename from `Index:`), with the same `..`/abs rule.
if grep -qE '^[[:space:]]+(---|\+\+\+|@@|diff |Index:)' "$PATCH_FILE"; then
  fail "patch.diff is indented (a non-pipeline/tampered diff) — refusing"
fi
# (c) Reject the leading-NON-whitespace-prefix-before-a-header class (the X---
#     residual). macOS patch ALSO de-indents a single leading non-whitespace
#     char, so `X--- a/src/../../../../daemon/src/victim.rs` reaches patch as a
#     `--- ` header — but a column-0 `^(---|+++) ` grep never sees it. Match any
#     line that ends in a `--- `/`+++ `/`Index: ` header but does NOT start at
#     column 0 with it, i.e. has 1+ leading chars before the header token. This
#     is a FAST-FAIL tamper signal only; the sandbox above is the real defense.
#     False-positive guard: a unified-diff CONTENT line legitimately starts with
#     a single `+`/`-`/` ` then arbitrary text — those never contain a ` --- ` /
#     ` +++ ` / `Index: ` *space-delimited header token at the de-indent
#     boundary*, so we anchor on "1+ leading non-whitespace chars, then a real
#     header token". A header proper is `--- `/`+++ ` (three dashes/pluses +
#     space); content lines are a SINGLE `+`/`-` then arbitrary text, which can
#     never form `<prefix>--- `/`<prefix>+++ ` (e.g. `+++added text` is `+` +
#     `++added` — no ` --- `/` +++ ` token follows the prefix). The 1+ quantifier
#     fast-fails BOTH the single-char `X--- ` residual macOS patch de-indents AND
#     a multi-char `ZZ--- ` (which patch ignores, but is still an obvious tamper).
if grep -qE '^[^[:space:]]+(---|\+\+\+) ' "$PATCH_FILE"; then
  fail "patch.diff has a '---'/'+++' header behind a leading prefix char (de-indent tamper, e.g. 'X--- ') — refusing"
fi
if grep -qE '^[^[:space:]]+Index: ' "$PATCH_FILE"; then
  fail "patch.diff has an 'Index:' header behind a leading prefix char (de-indent tamper) — refusing"
fi
# (d) Reject the MIXED whitespace-then-single-non-ws-char prefix class (e.g.
#     ` X--- `, `\tX--- `, ` X --- `). macOS patch (2.0-12u11-Apple) de-indents
#     ANY leading whitespace AND at most ONE non-whitespace char (in any
#     position within the run), so ` X--- a/src/../../../../daemon/src/victim.rs`
#     reaches patch as a real `--- ` header and writes out-of-tree — yet it
#     evades BOTH (a) (the line does NOT start with `---` right after the
#     whitespace, so `^[[:space:]]+(---` misses it) AND (c) (the line starts
#     with whitespace, so `^[^[:space:]]+` misses it). Empirically mapped: 1
#     non-ws char anywhere in the leading whitespace de-indents; 2+ non-ws chars
#     do NOT (patch leaves the line as content). Anchor on the de-indent
#     boundary: leading whitespace, then exactly one non-ws char, then optional
#     whitespace, then a real `--- `/`+++ `/`Index: ` header token. A unified-
#     diff CONTENT line legitimately starting with ` `/`+`/`-` then text can
#     never form `<ws><one-char><ws>--- `/`+++ `/`Index: ` (a real header needs
#     three dashes/pluses + space; a context line with `--- ` mid-text has 2+
#     non-ws chars before it, exceeding the single-char de-indent boundary and
#     thus is not an escape either) — so this stays false-positive-free. The
#     sandbox already DENIES the write for this class; this is the fast-fail
#     tamper signal that was missing for the mixed ordering.
if grep -qE '^[[:space:]]+[^[:space:]][[:space:]]*(---|\+\+\+|Index:) ' "$PATCH_FILE"; then
  fail "patch.diff has a '---'/'+++'/'Index:' header behind a mixed whitespace+char prefix (de-indent tamper, e.g. ' X--- ') — refusing"
fi
while IFS= read -r hdr; do
  # hdr is the path token after `--- ` / `+++ `, before any trailing tab/timestamp.
  path="${hdr%%$'\t'*}"
  # Trim a trailing whitespace-delimited timestamp field if present (no tab).
  path="${path%% *}"
  [ "$path" = "/dev/null" ] && continue
  # Mirror -p1: strip up to and including the first '/'.
  case "$path" in
    */*) stripped="${path#*/}" ;;
    *)   stripped="" ;;
  esac
  case "$stripped" in
    '' | /* )            fail "patch header '$path' is not confined (empty or absolute after -p1)" ;;
    '..' | '../'* | *'/../'* | *'/..' ) fail "patch header '$path' escapes via '..' — refusing" ;;
  esac
done < <(grep -E '^[[:space:]]*(---|\+\+\+|Index:) ' "$PATCH_FILE" | sed -E 's/^[[:space:]]*(---|\+\+\+|Index:) //')

# Apply to the STAGING copy: dry-run first (so a bad hunk is caught before any
# file is written), then for real. A failed hunk -> refuse.
if ! confined_patch "$STAGING" -p1 --batch --dry-run <"$PATCH_FILE" >/dev/null 2>&1; then
  fail "patch does not apply cleanly to a fresh staging copy (hunk reject)"
fi
if ! confined_patch "$STAGING" -p1 --batch <"$PATCH_FILE"; then
  fail "patch application to staging failed"
fi

# cargo check + cargo test in staging. These are the SAME gates the daemon ran
# at draft time and they are never weakened. Either failing -> refuse to touch
# the live tree.
if ! (cd "$STAGING" && cargo check); then
  fail "cargo check failed in staging — live daemon/src NOT modified"
fi
if ! (cd "$STAGING" && cargo test); then
  fail "cargo test failed in staging — live daemon/src NOT modified"
fi

# ----------------------------------------------------------------- apply (live)
# Green. Apply the SAME patch to the real daemon/ tree. Dry-run first here too.
stage "applying"

if ! confined_patch "$DAEMON" -p1 --batch --dry-run <"$PATCH_FILE" >/dev/null 2>&1; then
  fail "patch no longer applies to the live daemon/ tree (hunk reject) — live daemon/src NOT modified"
fi
if ! confined_patch "$DAEMON" -p1 --batch <"$PATCH_FILE"; then
  fail "patch application to daemon/ failed"
fi

# ----------------------------------------------------------------- rebuild
stage "rebuilding"
if ! (cd "$DAEMON" && cargo build --release); then
  fail "release rebuild failed (patch is applied to daemon/src; fix or revert by hand)"
fi

# Clear the pending marker so DARWIN stops announcing the proposal.
if command -v sqlite3 >/dev/null 2>&1; then
  sqlite3 "$ROOT/state/darwin.db" "DELETE FROM facts WHERE key = 'meta.heal_pending';" || true
else
  echo "sqlite3 not found; clear the marker manually:" >&2
  echo "  sqlite3 $ROOT/state/darwin.db \"DELETE FROM facts WHERE key = 'meta.heal_pending';\"" >&2
fi

# ----------------------------------------------------------------- restart
# Restart darwind if its launchd service is loaded so the healed binary runs.
# kickstart -k restarts a running service; if the service is not loaded the
# command fails and we fall back to telling the user to restart manually.
RESTARTED=0
if command -v launchctl >/dev/null 2>&1; then
  if launchctl kickstart -k "gui/$(id -u)/com.darwin.daemon" >/dev/null 2>&1; then
    RESTARTED=1
  fi
fi

if [ "$RESTARTED" -eq 1 ]; then
  echo "daemon restarted (launchctl kickstart com.darwin.daemon)."
else
  echo "restart darwind manually to run the patched build (launchd service not loaded)."
fi

result_ok

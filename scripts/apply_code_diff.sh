#!/bin/bash
# Apply a reviewed code-change proposal to the user's OWN allowlisted codebase.
#
# Usage:
#   scripts/apply_code_diff.sh <ts>          interactive (asks read -r confirmation)
#   scripts/apply_code_diff.sh <ts> --yes    non-interactive (for the HUD Accept button)
#   scripts/apply_code_diff.sh --selftest    hermetic confinement regression (no apply)
#
#   <ts> is the unix-timestamp directory under state/code/proposals/ that the
#   code_propose_diff tool announced (code.proposed telemetry / report.md).
#
# This is the ONLY path that ever touches the user's code. code_propose_diff is
# PROPOSE-ONLY — it writes a reviewable diff to state/code/proposals/<ts>/ and
# NEVER edits the tree. Applying it for real is a privileged mutation of the
# user's source, so this script is CONFINED BY CONSTRUCTION and RE-VALIDATES:
#   - the codebase root is the user-allowlisted [code].roots (NOT an arbitrary
#     path); the script reads it from config/darwin.toml,
#   - it re-confines the diff headers (a `..`/absolute/empty target after -p1 is
#     refused) — the same chokepoint the daemon's clean_code_diff enforces, AND
#   - it runs /usr/bin/patch under sandbox-exec with a DEFAULT-DENY SBPL profile
#     that allows file-write* ONLY under the canonicalized codebase root. The
#     kernel seatbelt physically DENIES any out-of-tree write regardless of how
#     leniently /usr/bin/patch parses a tampered header — this is the by-
#     construction backstop (mirrors the robust apply_heal.sh fix), NOT a fragile
#     header re-parse.
# Any gate failure exits non-zero and leaves the codebase untouched.
#
# --yes skips ONLY the read -r prompt: the GUI's two-step confirm replaces the
# human keystroke. Every gate above still runs. There is NO flag that weakens the
# confinement or re-validation — those gates are non-negotiable.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROPOSALS="$ROOT/state/code/proposals"
CONFIG="$ROOT/config/darwin.toml"

# Structured progress for the HUD. Stages: revalidating | applying.
# Terminal line is always exactly one RESULT: ok | RESULT: failed <reason>.
stage() { echo "STAGE: $1"; }
result_ok() { echo "RESULT: ok"; }
# Emit the terminal failure line and exit non-zero. The codebase is NOT modified
# by any path that calls this before the "applying" stage.
fail() {
  echo "RESULT: failed $1" >&2
  exit 1
}

# --selftest runs the hermetic confinement regression (no daemon / no network /
# no live tree touched) and exits. It guards the confinement defenses below so a
# future edit cannot silently reopen the out-of-tree-write hole.
if [ "${1:-}" = "--selftest" ]; then
  exec "$(dirname "${BASH_SOURCE[0]}")/test_apply_code_diff_confinement.sh"
fi

SANDBOX_EXEC="/usr/bin/sandbox-exec"
PATCH_BIN="/usr/bin/patch"
BSD_BASE_PROFILE="/System/Library/Sandbox/Profiles/bsd.sb"

# ---------------------------------------------------------- confined patch
# THE LOAD-BEARING DEFENSE. Run /usr/bin/patch under sandbox-exec with a
# DEFAULT-DENY SBPL profile that allows file-write* ONLY under a single
# canonicalized confinement dir (the codebase root). The kernel seatbelt then
# physically DENIES any write a patch attempts outside that dir — so a tampered
# `..`/Index:/de-indented header CANNOT write out-of-tree, no matter how
# leniently /usr/bin/patch parses the header (this is why the fix is "by
# construction", not by re-deriving patch's header parser).
#
# Mechanism is IDENTICAL to apply_heal.sh's confined_patch (and the daemon's
# micro-app runtime in daemon/src/apps.rs): `(version 1)` + `(deny default)` +
# import Apple's bsd.sb base + scoped allows. We allow file-read* broadly (patch
# reads the codebase files + the patch on stdin) and process-fork/exec, but
# file-write* is confined to the canonicalized codebase root.
#
# patch writes a TEMP/WORKING file (mkstemp) before renaming it onto the target;
# by default that lives under $TMPDIR (outside the confinement dir) and would be
# denied. We redirect patch's TMPDIR into a .code-sandbox-tmp/ dir INSIDE the
# confinement dir, so the ONLY writable location is the codebase subtree.
#
# Usage: confined_patch <confine_dir> [patch args...]   (patch reads stdin)
confined_patch() {
  local confine_raw="$1"; shift
  # Canonicalize: absolute, symlinks resolved, no trailing slash. A trailing
  # slash or a symlinked parent must not let the subpath filter mismatch what the
  # kernel canonicalizes the write target to. `cd && pwd -P` resolves the whole
  # chain; the dir must already exist.
  local confine
  if ! confine="$(cd "$confine_raw" 2>/dev/null && pwd -P)"; then
    fail "confinement dir '$confine_raw' does not resolve — refusing to run patch unsandboxed"
  fi

  # patch's tempfile dir, inside the confinement subtree (so it is writable under
  # the profile without opening any out-of-tree path).
  local tmpd="$confine/.code-sandbox-tmp"
  mkdir -p "$tmpd"

  # Build the deny-default-write profile. Default-deny everything, import the BSD
  # base so patch can even boot, allow reads + process basics, and allow WRITE
  # only under the canonicalized confinement dir.
  local profile
  profile="$(mktemp -t code-confine-sbpl)"
  {
    echo "(version 1)"
    echo ";; Generated by apply_code_diff.sh to confine /usr/bin/patch writes to"
    echo ";; the canonicalized allowlisted codebase root only. DEFAULT-DENY; the"
    echo ";; only file-write* grant is that root. A '..'/Index/de-indented header"
    echo ";; that resolves outside this subtree is DENIED by the kernel, not the"
    echo ";; pre-scan. Mirrors apply_heal.sh + the micro-app SBPL in apps.rs."
    echo "(deny default)"
    if [ -f "$BSD_BASE_PROFILE" ]; then
      echo "(import \"$BSD_BASE_PROFILE\")"
    fi
    echo "(allow process-fork)"
    echo "(allow process-exec*)"
    # patch reads the codebase files + the patch body on stdin; reads are not the
    # threat (the out-of-tree WRITE is), so file-read* is broad.
    echo "(allow file-read*)"
    # The single load-bearing grant: writes confined to the codebase root subtree
    # (which contains the tempfile dir above). Everything else stays denied.
    echo "(allow file-write* (subpath \"$confine\"))"
  } > "$profile"

  local rc=0
  ( cd "$confine" && TMPDIR="$tmpd" "$SANDBOX_EXEC" -f "$profile" "$PATCH_BIN" "$@" ) || rc=$?
  rm -f "$profile"
  return "$rc"
}

# ------------------------------------------------------- codebase root (config)
# Resolve the FIRST allowlisted [code].roots entry from config/darwin.toml. The
# root is a USER-ALLOWLISTED config value, NEVER an arbitrary path. We read only
# the [code] table's `roots` array (single-line or multi-line), take the first
# entry, and require it to be an EXISTING absolute directory. With [code] off /
# no roots, there is nothing to apply into and the script refuses.
#
# Prints the first root on stdout; returns non-zero if none is configured.
code_first_root() {
  [ -f "$CONFIG" ] || return 1
  # Isolate the [code] table: lines from `[code]` to the next `[section]`/EOF.
  local block
  block="$(awk '
    /^[[:space:]]*\[/ { if (in_code) exit; if ($0 ~ /^[[:space:]]*\[code\]/) { in_code=1; next } }
    in_code { print }
  ' "$CONFIG")"
  [ -n "$block" ] || return 1
  # Collapse the `roots = [ ... ]` assignment (single- or multi-line) and print
  # the first quoted token. Mirrors apply_forge.sh's perm_values awk: accumulate
  # from `roots =` to the closing `]`, then pull the first "..." span.
  printf '%s\n' "$block" | awk '
    BEGIN { collecting=0 }
    collecting==0 {
      if ($0 ~ /^[[:space:]]*roots[[:space:]]*=/) {
        line=$0
        sub(/^[[:space:]]*roots[[:space:]]*=/, "", line)
        buf=line
        collecting=1
        if (index(buf, "]") > 0) { emit(buf); exit }
      }
      next
    }
    collecting==1 {
      buf = buf "\n" $0
      if (index($0, "]") > 0) { emit(buf); exit }
      next
    }
    function emit(s,   rest, m) {
      rest=s
      if (match(rest, /"[^"]*"/)) {
        m = substr(rest, RSTART, RLENGTH)
        gsub(/"/, "", m)
        print m
      }
    }
  '
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
  echo "usage: $0 <ts> [--yes]   |   $0 --selftest" >&2
  if [ -d "$PROPOSALS" ] && [ -n "$(ls -A "$PROPOSALS" 2>/dev/null)" ]; then
    echo "pending proposals:" >&2
    ls -1 "$PROPOSALS" >&2
  else
    echo "(no pending proposals under state/code/proposals/)" >&2
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
PATCH_FILE="$DIR/patch.diff"
if [ ! -f "$PATCH_FILE" ]; then
  if [ "$MODE_YES" -eq 1 ]; then
    fail "no proposal at state/code/proposals/$TS (missing patch.diff)"
  fi
  echo "no proposal at $DIR (missing patch.diff)" >&2
  exit 1
fi

# Resolve the allowlisted codebase root. No [code].roots => nothing to apply into.
CODE_ROOT_RAW="$(code_first_root || true)"
if [ -z "$CODE_ROOT_RAW" ]; then
  fail "no [code].roots allowlisted in config/darwin.toml — refusing (code intelligence applies only into an allowlisted codebase root)"
fi
if [ ! -d "$CODE_ROOT_RAW" ]; then
  fail "allowlisted codebase root '$CODE_ROOT_RAW' is not an existing directory — refusing"
fi
# Canonicalize the root (absolute, symlinks resolved) — the confinement target.
CODE_ROOT="$(cd "$CODE_ROOT_RAW" && pwd -P)"
case "$CODE_ROOT" in
  /*) : ;;
  *) fail "codebase root '$CODE_ROOT' is not absolute after canonicalization — refusing" ;;
esac

# ----------------------------------------------------------- interactive gate
# Interactive mode: show the report + diff, ask read -r, then fall through to the
# shared confine + apply path. --yes skips ONLY this block.
if [ "$MODE_YES" -eq 0 ]; then
  if [ -f "$DIR/report.md" ]; then
    echo "=== report ($DIR/report.md) ==="
    cat "$DIR/report.md"
    echo
  fi
  echo "=== proposed diff ==="
  cat "$PATCH_FILE"
  echo "====================="

  printf 'Apply this diff to your codebase at %s? [y/N] ' "$CODE_ROOT"
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
# Re-confine the diff headers BEFORE patch ever runs. The diff is a plain file a
# human could hand-edit, so the confinement the daemon enforced at propose time
# is RE-ENFORCED here. This is the SAME defense-in-depth ladder apply_heal.sh
# uses — the strengthened pre-scan is a FAST-FAIL tamper signal; the sandbox
# below is the by-construction backstop.
stage "revalidating"

# Path-confinement: /usr/bin/patch is run with `-p1` and cwd = the codebase root,
# and macOS patch honors `..` in `---`/`+++` hunk headers — so a header like
# `+++ b/src/../../../../tmp/x` would write OUTSIDE the root. Reject any diff
# whose `---`/`+++` target, after the `-p1` strip, is empty, absolute, or
# contains a `..` component, BEFORE patch ever runs. The /dev/null new/deleted-
# file sentinel is exempt. Mirrors apply_heal.sh's pre-scan (incl. the de-indent
# tamper classes macOS patch normalizes) so the human apply is confined too.
if grep -qE '^[[:space:]]+(---|\+\+\+|@@|diff |Index:)' "$PATCH_FILE"; then
  fail "patch.diff is indented (a non-pipeline/tampered diff) — refusing"
fi
# Reject the leading-NON-whitespace-prefix-before-a-header class (the X--- residual
# macOS patch de-indents). A header proper is `--- `/`+++ ` (three dashes/pluses +
# space); content lines are a SINGLE `+`/`-` then text, which can never form
# `<prefix>--- `/`<prefix>+++ ` — so this stays false-positive-free.
if grep -qE '^[^[:space:]]+(---|\+\+\+) ' "$PATCH_FILE"; then
  fail "patch.diff has a '---'/'+++' header behind a leading prefix char (de-indent tamper, e.g. 'X--- ') — refusing"
fi
if grep -qE '^[^[:space:]]+Index: ' "$PATCH_FILE"; then
  fail "patch.diff has an 'Index:' header behind a leading prefix char (de-indent tamper) — refusing"
fi
# Reject the MIXED whitespace-then-single-non-ws-char prefix class (' X--- ',
# '\tX--- '). macOS patch de-indents ANY leading whitespace AND at most ONE
# non-whitespace char, so ' X--- a/src/../../../../etc/victim' reaches patch as a
# real header. Anchor on the de-indent boundary; the sandbox already DENIES the
# write — this is the fast-fail tamper signal for the mixed ordering.
if grep -qE '^[[:space:]]+[^[:space:]][[:space:]]*(---|\+\+\+|Index:) ' "$PATCH_FILE"; then
  fail "patch.diff has a '---'/'+++'/'Index:' header behind a mixed whitespace+char prefix (de-indent tamper, e.g. ' X--- ') — refusing"
fi
while IFS= read -r hdr; do
  # hdr is the path token after `--- ` / `+++ `, before any trailing tab/timestamp.
  path="${hdr%%$'\t'*}"
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

# Dry-run the confined apply first (so a bad hunk is caught before any file is
# written), then apply for real. A failed hunk -> refuse, codebase untouched.
if ! confined_patch "$CODE_ROOT" -p1 --batch --dry-run <"$PATCH_FILE" >/dev/null 2>&1; then
  fail "patch does not apply cleanly to the codebase at $CODE_ROOT (hunk reject) — codebase NOT modified"
fi

# ----------------------------------------------------------------- apply (live)
# Confined apply to the real codebase root. The sandbox allows writes ONLY under
# $CODE_ROOT; an out-of-tree target is DENIED by the kernel.
stage "applying"
if ! confined_patch "$CODE_ROOT" -p1 --batch <"$PATCH_FILE"; then
  fail "patch application to the codebase failed (a partial apply may have written under $CODE_ROOT; review it)"
fi

# Clear the pending marker so DARWIN stops announcing the proposal (best-effort).
if command -v sqlite3 >/dev/null 2>&1; then
  sqlite3 "$ROOT/state/darwin.db" "DELETE FROM facts WHERE key = 'meta.code_pending';" 2>/dev/null || true
fi

echo "applied the proposed diff to your codebase at $CODE_ROOT."
echo "review the change, build/test it yourself, and commit if you're happy with it."
result_ok

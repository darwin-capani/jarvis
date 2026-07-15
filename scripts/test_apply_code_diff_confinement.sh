#!/bin/bash
# Hermetic regression test for the apply_code_diff.sh path-confinement defenses.
#
# WHAT IT GUARDS: a hand-tampered state/code/proposals/<ts>/patch.diff whose
# `---`/`+++`/`Index:` header carries a `..`/absolute/symlink target so that,
# after /usr/bin/patch's lenient header de-indentation + `-p1` strip, the write
# lands OUTSIDE the allowlisted codebase root (e.g. a sibling victim file) —
# planting attacker content into a file the human never reviewed. apply_code_diff
# is the ONLY path that touches the user's code, so its confinement must hold even
# against a tampered diff.
#
# THE FIX UNDER TEST is BY CONSTRUCTION, not by out-parsing patch: apply_code_diff
# runs every /usr/bin/patch invocation under sandbox-exec with a DEFAULT-DENY SBPL
# profile that allows file-write* ONLY under the canonicalized codebase root (the
# confinement dir). The kernel seatbelt physically DENIES any out-of-tree write
# regardless of how patch parses the header. The strengthened header pre-scan
# stays as a best-effort fast-fail tamper signal. This mirrors apply_heal.sh.
#
# This test is HERMETIC: it NEVER invokes darwind, the daemon, cargo, launchd,
# sqlite, a model, or any network. It exercises (a) the script's strengthened
# pre-scan (sliced verbatim from the live script) and (b) the real
# `confined_patch` sandbox helper against a fake codebase layout in a temp dir —
# proving each escape ('..'/symlink/X-prefix/absolute) leaves the out-of-tree
# victim byte-for-byte UNCHANGED, while a legit confined apply still succeeds.
# Invoked by the script's own `--selftest` hook so the defense cannot silently
# regress.
#
# Run:  scripts/test_apply_code_diff_confinement.sh   (or apply_code_diff.sh --selftest)
# Exit: 0 = all cases pass, 1 = a case regressed.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$HERE/apply_code_diff.sh"
SANDBOX_EXEC="/usr/bin/sandbox-exec"
PATCH_BIN="/usr/bin/patch"
BSD_BASE_PROFILE="/System/Library/Sandbox/Profiles/bsd.sb"

PASS=0
FAIL=0
ok()   { echo "ok   - $1"; PASS=$((PASS + 1)); }
bad()  { echo "FAIL - $1"; FAIL=$((FAIL + 1)); }

# ---------------------------------------------------------------------------
# Part A: the strengthened header PRE-SCAN (fast-fail tamper signal).
# ---------------------------------------------------------------------------
# Slice the pre-scan + confinement loop verbatim out of the live script so the
# test can never drift from what the script actually runs. Anchor on the stable
# `# Path-confinement:` banner through the `done < <(grep ...)` that closes the
# loop. Anchoring on the banner — not on any one defensive line — means a
# REGRESSED gate is sliced and exercised too (it then fails the assertion).
run_gate() {
  local patch_file="$1"
  local gate
  gate="$(awk '
    /^# Path-confinement:/ { capture=1 }
    capture { print }
    /^done < <\(grep -E/ { if (capture) exit }
  ' "$SCRIPT")"
  if [ -z "$gate" ]; then
    echo "INTERNAL: could not slice the confinement gate out of apply_code_diff.sh" >&2
    return 2
  fi
  PATCH_FILE="$patch_file" bash -c '
    set -uo pipefail
    fail() { echo "REJECTED: $1"; exit 1; }
    '"$gate"'
    echo "ACCEPTED"
  '
}

# ---------------------------------------------------------------------------
# Part B: the real `confined_patch` SANDBOX helper (the load-bearing defense).
# ---------------------------------------------------------------------------
# Slice the helper body verbatim out of the live script (from its function header
# to its closing brace) and source it into a tiny shim so the test runs the EXACT
# sandbox profile the script builds — no re-implementation.
load_confined_patch() {
  awk '
    /^confined_patch\(\) \{/ { capture=1 }
    capture { print }
    capture && /^\}$/ { exit }
  ' "$SCRIPT"
}

# Run the sliced confined_patch against a hermetic codebase. Returns patch's rc;
# we assert on the victim file rather than stdout. fail() is stubbed to echo so a
# confinement-dir resolve error is visible.
sandbox_apply() {
  local confine="$1" patch_file="$2" extra="${3:-}"
  local helper
  helper="$(load_confined_patch)"
  CONFINE="$confine" PATCH_INPUT="$patch_file" EXTRA="$extra" \
  SANDBOX_EXEC="$SANDBOX_EXEC" PATCH_BIN="$PATCH_BIN" BSD_BASE_PROFILE="$BSD_BASE_PROFILE" \
  bash -c '
    set -uo pipefail
    fail() { echo "FAIL_HELPER: $1" >&2; exit 99; }
    '"$helper"'
    if [ "${EXTRA}" = "dry" ]; then
      confined_patch "$CONFINE" -p1 --batch --dry-run < "$PATCH_INPUT"
    else
      confined_patch "$CONFINE" -p1 --batch < "$PATCH_INPUT"
    fi
  '
}

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# Build a faithful fake codebase for a given test name and return its parts via
# globals: REPO (the project), CODE (the allowlisted codebase root == patch cwd),
# VICTIM (the OUT-OF-TREE sibling target REPO/outside/victim). CODE/src is the dir
# -p1 paths resolve against; victim is a SIBLING of the codebase root, reachable
# only via a `..`-escape.
make_repo() {
  local name="$1"
  REPO="$WORK/$name"
  CODE="$REPO/project"
  VICTIM="$REPO/outside/victim"
  rm -rf "$REPO"
  mkdir -p "$CODE/src" "$REPO/outside"
  printf 'ORIGINAL OUTSIDE FILE\nkeep\n' > "$VICTIM"
  printf 'placeholder\n' > "$CODE/src/lib.rs"
}

# A malicious diff whose `---`/`+++` target escapes the codebase root to the
# sibling victim. $1 = leading-prefix prepended to EVERY line (the de-indent
# tamper: '' = column-0, 'X' = the residual, $'\t' = tab, 'ZZ' = multi-char).
# Relative to CODE (patch cwd): src/../../outside/victim ==
#   src/..(=CODE) /..(=REPO) /outside/victim.
write_escape_diff() {
  local file="$1" prefix="$2" header_kind="${3:-dashes}"
  local h1 h2
  case "$header_kind" in
    dashes) h1='--- a/src/../../outside/victim'
            h2='+++ a/src/../../outside/victim' ;;
    index)  h1='Index: src/../../outside/victim'
            h2='+++ a/src/../../outside/victim' ;;
  esac
  {
    printf '%s%s\n' "$prefix" "$h1"
    printf '%s%s\n' "$prefix" "$h2"
    printf '%s%s\n' "$prefix" '@@ -1,2 +1,2 @@'
    printf '%s%s\n' "$prefix" '-ORIGINAL OUTSIDE FILE'
    printf '%s%s\n' "$prefix" '+INJECTED_OUT_OF_TREE'
    printf '%s%s\n' "$prefix" ' keep'
  } > "$file"
}

# Prove the OLD (raw, unsandboxed) behavior WOULD escape for this diff: run a raw
# /usr/bin/patch (no sandbox) in a throwaway clone and confirm the victim IS
# clobbered. Makes each case a true regression proof — the escape is real, and the
# sandbox is what closes it.
prove_old_escapes() {
  local diff="$1"
  local clone="$WORK/oldproof-$RANDOM"
  local ccode="$clone/project"
  local cvic="$clone/outside/victim"
  mkdir -p "$ccode/src" "$clone/outside"
  printf 'ORIGINAL OUTSIDE FILE\nkeep\n' > "$cvic"
  printf 'placeholder\n' > "$ccode/src/lib.rs"
  ( cd "$ccode" && "$PATCH_BIN" -p1 --batch < "$diff" ) >/dev/null 2>&1
  if grep -q INJECTED_OUT_OF_TREE "$cvic" 2>/dev/null; then
    rm -rf "$clone"; return 0   # old behavior escaped (as expected)
  fi
  rm -rf "$clone"; return 1      # old behavior did NOT escape -> not a real residual
}

# A full malicious case: prove the escape-class behaves as claimed under RAW
# patch, then prove the sandbox helper leaves the out-of-tree victim byte-for-byte
# UNCHANGED. $label, $prefix, $header_kind, $expect_prescan, $escape_class.
malicious_case() {
  local label="$1" prefix="$2" header_kind="${3:-dashes}" expect_prescan="${4:-reject}" escape_class="${5:-escapable}"
  make_repo "case_${label// /_}"
  local diff="$REPO/patch.diff"
  write_escape_diff "$diff" "$prefix" "$header_kind"

  # 1. Regression proof on RAW patch, per the claimed escape class.
  if [ "$escape_class" = "escapable" ]; then
    if ! prove_old_escapes "$diff"; then
      bad "$label: OLD raw patch did NOT escape — case is not a faithful residual"
      return
    fi
  else
    if prove_old_escapes "$diff"; then
      bad "$label: claimed benign but RAW patch DID escape — reclassify as escapable"
      return
    fi
    ok "$label: RAW patch rejects this tamper (no de-indent -> no header -> no escape)"
  fi

  # 2. The load-bearing sandbox: applying under confined_patch must NOT clobber
  #    the out-of-tree victim. Confinement dir = CODE (the real patch cwd).
  local before after rc
  before="$(cat "$VICTIM")"
  sandbox_apply "$CODE" "$diff" >/dev/null 2>&1
  rc=$?
  after="$(cat "$VICTIM")"
  if [ "$before" = "$after" ]; then
    if [ "$escape_class" = "escapable" ]; then
      ok "$label: sandbox DENIED the out-of-tree write (victim unchanged, was escapable raw, rc=$rc)"
    else
      ok "$label: out-of-tree victim still unchanged under the sandbox (rc=$rc)"
    fi
  else
    bad "$label: out-of-tree victim WAS modified under the sandbox -> escape reachable"
  fi

  # 3. Best-effort pre-scan fast-fail (where applicable).
  if [ "$expect_prescan" = "reject" ]; then
    local out; out="$(run_gate "$diff")"
    case "$out" in
      REJECTED:*) ok "$label: pre-scan also fast-fails this tamper [$out]" ;;
      *)          bad "$label: pre-scan did NOT fast-fail [$out] (sandbox still caught it, but the signal should fire)" ;;
    esac
  fi
}

echo "== malicious cases: escape DENIED under the sandbox; victim must stay intact =="

# Case 1: THE RESIDUAL — single leading non-whitespace 'X' prefix. macOS patch
#         de-indents the lone 'X' -> a real out-of-tree escape under raw patch.
malicious_case "X-prefix '..' header (the residual)" "X" "dashes" "reject" "escapable"

# Case 2: leading TAB prefix (whitespace de-indent class).
malicious_case "leading-tab '..' header" "$(printf '\t')" "dashes" "reject" "escapable"

# Case 3: multi-char non-whitespace prefix — macOS patch de-indents only a SINGLE
#         leading non-ws char, so 'ZZ--- ' is not a header -> raw patch already
#         fails. The pre-scan + sandbox still reject it; honest it isn't a residual.
malicious_case "multi-char-prefix '..' header" "ZZ" "dashes" "reject" "benign"

# Case 4: Index: header carrying '..' (single-space indent, whitespace class).
malicious_case "Index: '..' header" " " "index" "reject" "escapable"

# Case 5: column-0 '..' header (the always-caught variant).
malicious_case "column-0 '..' header" "" "dashes" "reject" "escapable"

# Case 6: uniformly whitespace-indented (one space) '..' header.
malicious_case "single-space-indented '..' header" " " "dashes" "reject" "escapable"

# Case 7: MIXED whitespace+single-non-ws-char prefix (' X--- ...').
malicious_case "mixed ws+char ' X' '..' header" " X" "dashes" "reject" "escapable"

# Case 8: mixed TAB+char prefix ('\tX--- ...').
malicious_case "mixed tab+char '..' header" "$(printf '\t')X" "dashes" "reject" "escapable"

echo
echo "== absolute-path escape: DENIED under the sandbox =="

# An ABSOLUTE header that, after -p1, still targets an out-of-tree absolute path.
# The pre-scan rejects an absolute-after-p1 header; the sandbox denies the write
# regardless (writes are confined to CODE).
make_repo "case_absolute"
ABS="$REPO/abs.diff"
ABS_VICTIM="$REPO/outside/victim"
printf '%s\n' \
  "--- /$ABS_VICTIM" \
  "+++ /$ABS_VICTIM" \
  '@@ -1,2 +1,2 @@' \
  '-ORIGINAL OUTSIDE FILE' \
  '+INJECTED_ABS' \
  ' keep' > "$ABS"
before="$(cat "$ABS_VICTIM")"
sandbox_apply "$CODE" "$ABS" >/dev/null 2>&1
after="$(cat "$ABS_VICTIM")"
if [ "$before" = "$after" ]; then
  ok "absolute-path header: sandbox kept the out-of-tree victim unchanged"
else
  bad "absolute-path header: out-of-tree victim WAS modified -> escape reachable"
fi
# Pre-scan: an absolute path after the -p1 strip (a `//...` header) is rejected.
make_repo "case_absolute_prescan"
ABS2="$REPO/abs2.diff"
printf '%s\n' \
  '--- //etc/shadow' \
  '+++ //etc/shadow' \
  '@@ -1,1 +1,1 @@' \
  '-a' \
  '+b' > "$ABS2"
out="$(run_gate "$ABS2")"
case "$out" in
  REJECTED:*) ok "absolute-after-p1 header is pre-scan-rejected [$out]" ;;
  *)          bad "absolute-after-p1 header was NOT pre-scan-rejected [$out]" ;;
esac

echo
echo "== symlink escape: a symlinked codebase entry cannot redirect a write out =="

# A symlink INSIDE the codebase root pointing at the out-of-tree victim. A diff
# targeting the symlink's path would, without confinement, follow the link and
# clobber the victim. The sandbox confines writes to the canonicalized CODE
# subtree; the link's REAL target is outside, so the write is DENIED.
make_repo "case_symlink"
SYM_VICTIM="$REPO/outside/victim"
ln -s "$SYM_VICTIM" "$CODE/src/link"
SYM="$REPO/sym.diff"
printf '%s\n' \
  '--- a/src/link' \
  '+++ b/src/link' \
  '@@ -1,2 +1,2 @@' \
  '-ORIGINAL OUTSIDE FILE' \
  '+INJECTED_VIA_SYMLINK' \
  ' keep' > "$SYM"
before="$(cat "$SYM_VICTIM")"
sandbox_apply "$CODE" "$SYM" >/dev/null 2>&1
after="$(cat "$SYM_VICTIM")"
if [ "$before" = "$after" ]; then
  ok "symlink-escape: sandbox kept the out-of-tree victim unchanged (the link's real target is outside CODE)"
else
  bad "symlink-escape: out-of-tree victim WAS modified through the symlink -> escape reachable"
fi

echo
echo "== legit case: a confined apply must still SUCCEED under the sandbox =="

# A confined a/src diff applies and writes INSIDE the codebase root.
make_repo "legit_confined"
GOOD="$REPO/good.diff"
printf '%s\n' \
  '--- a/src/lib.rs' \
  '+++ b/src/lib.rs' \
  '@@ -1,1 +1,1 @@' \
  '-placeholder' \
  '+patched_ok' > "$GOOD"
# pre-scan must accept it
out="$(run_gate "$GOOD")"
[ "$out" = "ACCEPTED" ] || bad "legit confined diff wrongly pre-scan-rejected [$out]"
# dry-run under sandbox succeeds, writes nothing
sandbox_apply "$CODE" "$GOOD" dry >/dev/null 2>&1
drc=$?
if [ "$drc" -eq 0 ] && [ "$(cat "$CODE/src/lib.rs")" = "placeholder" ]; then
  ok "legit confined diff: sandbox dry-run succeeds and writes nothing"
else
  bad "legit confined diff: sandbox dry-run failed (rc=$drc) or wrote early"
fi
# real apply under sandbox writes the patched file inside the codebase root
sandbox_apply "$CODE" "$GOOD" >/dev/null 2>&1
rrc=$?
if [ "$rrc" -eq 0 ] && [ "$(cat "$CODE/src/lib.rs")" = "patched_ok" ]; then
  ok "legit confined diff: sandbox real apply writes inside the codebase root"
else
  bad "legit confined diff: sandbox real apply failed (rc=$rrc) or did not patch"
fi

# A /dev/null new-file diff is accepted by the pre-scan and creates the file in-tree.
make_repo "legit_newfile"
NEWF="$REPO/newfile.diff"
printf '%s\n' \
  '--- /dev/null' \
  '+++ b/src/new.rs' \
  '@@ -0,0 +1,1 @@' \
  '+hello' > "$NEWF"
out="$(run_gate "$NEWF")"
[ "$out" = "ACCEPTED" ] || bad "legit /dev/null new-file diff wrongly pre-scan-rejected [$out]"
sandbox_apply "$CODE" "$NEWF" >/dev/null 2>&1
nrc=$?
if [ "$nrc" -eq 0 ] && [ -f "$CODE/src/new.rs" ] && [ "$(cat "$CODE/src/new.rs")" = "hello" ]; then
  ok "legit /dev/null new-file diff: sandbox real apply creates the file in-tree"
else
  bad "legit /dev/null new-file diff: sandbox apply failed (rc=$nrc) or did not create file"
fi

echo
echo "apply_code_diff confinement: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]

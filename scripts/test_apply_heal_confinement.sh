#!/bin/bash
# Hermetic regression test for the apply_heal.sh path-confinement defenses.
#
# WHAT IT GUARDS: a hand-tampered state/heal/proposals/<ts>/patch.diff whose
# `---`/`+++`/`Index:` header carries a `..`/absolute target so that, after
# /usr/bin/patch's lenient header de-indentation + `-p1` strip, the write lands
# OUTSIDE the staging tree (e.g. ROOT/daemon/src/victim.rs) — planting attacker
# Rust into the live daemon source that the next `cargo build --release` would
# compile in (daemon RCE).
#
# THREE successive incomplete pre-scan fixes proved you cannot win by
# re-deriving patch's header parser:
#   1. un-indented `..` headers (caught by a column-0 grep),
#   2. uniformly WHITESPACE-indented `..` headers (patch de-indents leading ws),
#   3. THE RESIDUAL: a single leading NON-whitespace char — `X--- a/src/..` —
#      which macOS patch (2.0-12u11-Apple) ALSO de-indents, evading both the
#      whitespace-indent rejection and the column-0 header scan -> zero headers
#      matched -> the confinement loop never ran -> ACCEPTED -> out-of-tree write.
#
# THE FIX UNDER TEST is BY CONSTRUCTION, not by out-parsing patch: apply_heal.sh
# now runs every /usr/bin/patch invocation under sandbox-exec with a
# DEFAULT-DENY SBPL profile that allows file-write* ONLY under the canonicalized
# patch cwd (the confinement dir). The kernel seatbelt physically DENIES any
# out-of-tree write regardless of how patch parses the header. The strengthened
# header pre-scan stays as a best-effort fast-fail tamper signal.
#
# This test is HERMETIC: it NEVER invokes darwind, the daemon, cargo, launchd,
# sqlite, or any network. It exercises (a) the script's strengthened pre-scan
# (sliced verbatim from the live script) and (b) the real `confined_patch`
# sandbox helper against a fake repo layout in a temp dir — proving each escape
# leaves the sibling victim byte-for-byte UNCHANGED, while the legit confined
# applies still succeed. Invoked by the script's own `--selftest` hook so the
# defense cannot silently regress.
#
# Run:  scripts/test_apply_heal_confinement.sh
# Exit: 0 = all cases pass, 1 = a case regressed.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$HERE/apply_heal.sh"
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
    echo "INTERNAL: could not slice the confinement gate out of apply_heal.sh" >&2
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
# Slice the helper body verbatim out of the live script (from its function
# header to its closing brace) and source it into a tiny shim so the test runs
# the EXACT sandbox profile the script builds — no re-implementation.
load_confined_patch() {
  awk '
    /^confined_patch\(\) \{/ { capture=1 }
    capture { print }
    capture && /^\}$/ { exit }
  ' "$SCRIPT"
}

# Run the sliced confined_patch against a hermetic repo. Returns patch's rc;
# prints nothing on stdout we rely on (we assert on the victim file instead).
# fail() is stubbed to just echo so a confinement-dir resolve error is visible.
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

# Build a faithful fake repo for a given test name and return its parts via
# globals: REPO, STAGING (the patch cwd, == ROOT/state/heal/apply-staging-123),
# VICTIM (the OUT-OF-TREE sibling target ROOT/daemon/src/victim.rs).
# STAGING/src is the dir -p1 paths resolve against; victim.rs is a SIBLING of
# the staging tree, reachable only via a `..`-escape.
make_repo() {
  local name="$1"
  REPO="$WORK/$name"
  STAGING="$REPO/state/heal/apply-staging-123"
  VICTIM="$REPO/daemon/src/victim.rs"
  rm -rf "$REPO"
  mkdir -p "$STAGING/src" "$REPO/daemon/src"
  printf 'ORIGINAL DAEMON FILE\nkeep\n' > "$VICTIM"
  printf 'placeholder\n' > "$STAGING/src/lib.rs"
}

# A malicious diff body whose `---`/`+++` target escapes the staging tree to the
# sibling victim. $1 = leading-prefix string prepended to EVERY line (the
# de-indent tamper: '' = column-0, 'X' = the residual, $'\t' = tab, 'ZZ' =
# multi-char). The target after -p1 + de-indent resolves to ROOT/daemon/src/...
# Relative to STAGING (the patch cwd): src/../../../../daemon/src/victim.rs ==
#   src/..(=STAGING) /..(=heal) /..(=state) /..(=ROOT) /daemon/src/victim.rs.
write_escape_diff() {
  local file="$1" prefix="$2" header_kind="${3:-dashes}"
  local h1 h2
  case "$header_kind" in
    dashes) h1='--- a/src/../../../../daemon/src/victim.rs'
            h2='+++ a/src/../../../../daemon/src/victim.rs' ;;
    index)  h1='Index: src/../../../../daemon/src/victim.rs'
            h2='+++ a/src/../../../../daemon/src/victim.rs' ;;
  esac
  {
    printf '%s%s\n' "$prefix" "$h1"
    printf '%s%s\n' "$prefix" "$h2"
    printf '%s%s\n' "$prefix" '@@ -1,2 +1,2 @@'
    printf '%s%s\n' "$prefix" '-ORIGINAL DAEMON FILE'
    printf '%s%s\n' "$prefix" '+INJECTED_OUT_OF_TREE'
    printf '%s%s\n' "$prefix" ' keep'
  } > "$file"
}

# Prove the OLD (raw, unsandboxed) behavior WOULD escape for this diff: run a
# raw /usr/bin/patch (no sandbox) in a throwaway clone and confirm the victim
# IS clobbered. This makes each case a true regression proof — the escape is
# real, and the sandbox is what closes it.
prove_old_escapes() {
  local diff="$1"
  local clone="$WORK/oldproof-$RANDOM"
  local cstg="$clone/state/heal/apply-staging-123"
  local cvic="$clone/daemon/src/victim.rs"
  mkdir -p "$cstg/src" "$clone/daemon/src"
  printf 'ORIGINAL DAEMON FILE\nkeep\n' > "$cvic"
  printf 'placeholder\n' > "$cstg/src/lib.rs"
  ( cd "$cstg" && "$PATCH_BIN" -p1 --batch < "$diff" ) >/dev/null 2>&1
  if grep -q INJECTED_OUT_OF_TREE "$cvic" 2>/dev/null; then
    rm -rf "$clone"; return 0   # old behavior escaped (as expected)
  fi
  rm -rf "$clone"; return 1      # old behavior did NOT escape -> not a real residual
}

# A full malicious case: prove the escape-class behaves as claimed under RAW
# patch, then prove the sandbox helper leaves the out-of-tree victim byte-for-
# byte UNCHANGED. $label, $prefix, $header_kind, $expect_prescan, $escape_class.
#   escape_class=escapable -> raw /usr/bin/patch DOES write out-of-tree (a real
#                             residual the sandbox must close).
#   escape_class=benign    -> raw /usr/bin/patch already FAILS to apply this
#                             tamper (e.g. a 2+ char prefix patch won't de-indent
#                             so no header is found); the sandbox + pre-scan must
#                             still reject/deny it, but we do NOT pretend it was
#                             a raw escape. Honest about which classes are real.
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
  #    the out-of-tree victim. Confinement dir = STAGING (the real patch cwd).
  local before after rc
  before="$(cat "$VICTIM")"
  sandbox_apply "$STAGING" "$diff" >/dev/null 2>&1
  rc=$?
  after="$(cat "$VICTIM")"
  if [ "$before" = "$after" ]; then
    if [ "$escape_class" = "escapable" ]; then
      ok "$label: sandbox DENIED the out-of-tree write (victim unchanged, was escapable raw, rc=$rc)"
    else
      ok "$label: out-of-tree victim still unchanged under the sandbox (rc=$rc)"
    fi
  else
    bad "$label: out-of-tree victim.rs WAS modified under the sandbox -> escape reachable"
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

# Case 2: leading TAB prefix (whitespace de-indent class) — raw patch de-indents
#         the tab and escapes.
malicious_case "leading-tab '..' header" "$(printf '\t')" "dashes" "reject" "escapable"

# Case 3: multi-char non-whitespace prefix. macOS patch de-indents only a SINGLE
#         leading non-ws char, so 'ZZ--- ' is NOT recognized as a header -> raw
#         patch already fails (no escape). The pre-scan + sandbox still reject it,
#         but we are honest that this class is not a raw residual.
malicious_case "multi-char-prefix '..' header" "ZZ" "dashes" "reject" "benign"

# Case 4: Index: header carrying '..' (single-space indent, whitespace class).
malicious_case "Index: '..' header" " " "index" "reject" "escapable"

# Case 5: column-0 '..' header (the always-caught variant).
malicious_case "column-0 '..' header" "" "dashes" "reject" "escapable"

# Case 6: uniformly whitespace-indented (one space) '..' header — the prior
#         residual that motivated the de-indent defense (raw patch de-indents).
malicious_case "single-space-indented '..' header" " " "dashes" "reject" "escapable"

# Case 7: MIXED whitespace+single-non-ws-char prefix (' X--- ...'). macOS patch
#         de-indents leading whitespace AND one non-ws char, so this is a REAL
#         raw escape — and it slips BOTH the whitespace-indent scan (line starts
#         with ws but not '---' right after) AND the non-ws-first prefix scan
#         (line starts with ws, not a non-ws char). The strengthened pre-scan
#         rule (d) now fast-fails it; the sandbox already denied it. This is the
#         variant the final re-attack surfaced as a pre-scan gap.
malicious_case "mixed ws+char ' X' '..' header" " X" "dashes" "reject" "escapable"

# Case 8: mixed TAB+char prefix ('\tX--- ...') — same de-indent class, tab form.
malicious_case "mixed tab+char '..' header" "$(printf '\t')X" "dashes" "reject" "escapable"

echo
echo "== legit cases: confined applies must still SUCCEED under the sandbox =="

# Legit 1: a confined a/src diff applies and writes inside the staging tree.
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
sandbox_apply "$STAGING" "$GOOD" dry >/dev/null 2>&1
drc=$?
if [ "$drc" -eq 0 ] && [ "$(cat "$STAGING/src/lib.rs")" = "placeholder" ]; then
  ok "legit confined diff: sandbox dry-run succeeds and writes nothing"
else
  bad "legit confined diff: sandbox dry-run failed (rc=$drc) or wrote early"
fi
# real apply under sandbox writes the patched file inside the staging tree
sandbox_apply "$STAGING" "$GOOD" >/dev/null 2>&1
rrc=$?
if [ "$rrc" -eq 0 ] && [ "$(cat "$STAGING/src/lib.rs")" = "patched_ok" ]; then
  ok "legit confined diff: sandbox real apply writes inside the staging tree"
else
  bad "legit confined diff: sandbox real apply failed (rc=$rrc) or did not patch"
fi

# Legit 2: a /dev/null new-file diff is accepted by the pre-scan and applies
#          inside the staging tree under the sandbox.
make_repo "legit_newfile"
NEWF="$REPO/newfile.diff"
printf '%s\n' \
  '--- /dev/null' \
  '+++ b/src/new.rs' \
  '@@ -0,0 +1,1 @@' \
  '+hello' > "$NEWF"
out="$(run_gate "$NEWF")"
[ "$out" = "ACCEPTED" ] || bad "legit /dev/null new-file diff wrongly pre-scan-rejected [$out]"
sandbox_apply "$STAGING" "$NEWF" >/dev/null 2>&1
nrc=$?
if [ "$nrc" -eq 0 ] && [ -f "$STAGING/src/new.rs" ] && [ "$(cat "$STAGING/src/new.rs")" = "hello" ]; then
  ok "legit /dev/null new-file diff: sandbox real apply creates the file in-tree"
else
  bad "legit /dev/null new-file diff: sandbox apply failed (rc=$nrc) or did not create file"
fi

# Legit 3: a unified-diff CONTENT line that legitimately starts with '+'/'-'
#          must NOT be false-rejected by the strengthened leading-prefix scan
#          (the `+something` / `-something` content lines look superficially like
#          a prefixed header but are not `--- `/`+++ ` header tokens).
make_repo "legit_content"
CONT="$REPO/content.diff"
printf '%s\n' \
  '--- a/src/lib.rs' \
  '+++ b/src/lib.rs' \
  '@@ -1,3 +1,3 @@' \
  '-placeholder' \
  '+++added marker line' \
  '---removed marker line' \
  ' tail' > "$CONT"
out="$(run_gate "$CONT")"
case "$out" in
  ACCEPTED) ok "content lines starting with '+'/'-'/'+++'/'---' are NOT false-rejected by the prefix scan" ;;
  *)        bad "legit content lines were false-rejected by the strengthened pre-scan [$out]" ;;
esac

echo
echo "apply_heal confinement: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]

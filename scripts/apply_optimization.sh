#!/bin/bash
# Apply a measured routing-optimization proposal — the gated, human-reviewed
# adoption step for the optimization-from-usage loop (daemon/src/optimize.rs).
#
# Usage:
#   scripts/apply_optimization.sh <ts>          interactive (asks read -r confirmation)
#   scripts/apply_optimization.sh <ts> --show   print the proposal and exit (review only)
#
#   <ts> is the unix-timestamp directory under state/optimize/proposals/ that the
#   optimizer announced (optimize.proposed telemetry / proposal.md).
#
# WHAT THIS IS (honest): the optimizer NEVER mutates a live config. It writes a
# PROPOSAL — a cue-weight diff over the shipped routing vocabulary
# (agents.rs CUE_VOCAB) plus the measured before/after accuracy on HELD-OUT
# traces. The change was ADOPTED ONLY IF it measurably beat the current baseline
# by a margin AND was not worse on any held-out class, so it can never make
# routing worse. Adoption is a deliberate, reversible HUMAN step — this script
# shows the proposal, asks for confirmation, and records the operator's decision.
# It does NOT silently rewrite source: applying the cue-weight diff to
# CUE_VOCAB is a reviewed source edit the operator makes (or a future
# config-backed override loads), exactly mirroring the propose-only posture of
# scripts/apply_heal.sh.
#
# The master switch ships ON ([optimize].enabled = true, armed full-power default):
# live trace recording is runtime-gated (traces accrue only while the daemon runs
# with it on), PII-REDACTED, and bounded (oldest rows evicted past the cap). Even
# armed, adoption stays propose-only — the optimizer never mutates a live config;
# it only writes proposals a human applies here. Set [optimize].enabled = false to
# disable: with it off no corpus accrues and no proposal is ever written, so there
# is nothing here to apply.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROPOSALS="$ROOT/state/optimize/proposals"

TS="${1:-}"
SHOW_ONLY=0
for arg in "${@:2}"; do
  case "$arg" in
    --show) SHOW_ONLY=1 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

if [ -z "$TS" ]; then
  echo "usage: $0 <ts> [--show]" >&2
  if [ -d "$PROPOSALS" ] && [ -n "$(ls -A "$PROPOSALS" 2>/dev/null)" ]; then
    echo "pending proposals:" >&2
    ls -1 "$PROPOSALS" >&2
  else
    echo "(no pending proposals under state/optimize/proposals/)" >&2
  fi
  exit 1
fi

# Validate <ts> is a plausible numeric stamp BEFORE it is used as a path
# component — digits only, no slashes, no "..". Mirrors apply_heal.sh; path
# traversal is impossible because the stamp can only be digits.
case "$TS" in
  '' | *[!0-9]*)
    echo "invalid timestamp '$TS' (must be digits only)" >&2
    exit 2
    ;;
esac

DIR="$PROPOSALS/$TS"
MD="$DIR/proposal.md"
JSON="$DIR/proposal.json"
if [ ! -f "$MD" ] || [ ! -f "$JSON" ]; then
  echo "no proposal at $DIR (missing proposal.md/proposal.json)" >&2
  exit 1
fi

echo "=== routing optimization proposal ($MD) ==="
cat "$MD"
echo "============================================"

if [ "$SHOW_ONLY" -eq 1 ]; then
  exit 0
fi

cat <<'NOTE'

This proposal was measured on HELD-OUT traces only; the live routing config is
UNCHANGED. The proposed cue-weight diff above strengthens the shipped routing
vocabulary toward the agent the recorded corrections revealed as correct.

To ADOPT it, a human applies the diff above to the routing vocabulary
(daemon/src/agents.rs CUE_VOCAB) and rebuilds — a reviewed, reversible source
edit. To REJECT it, leave the proposal in place (or delete the directory).
NOTE

printf 'Mark this proposal as ACCEPTED (records your decision; does not rewrite source)? [y/N] '
read -r answer
case "$answer" in
  y | Y | yes | YES)
    echo "accepted" > "$DIR/DECISION"
    echo "recorded ACCEPTED at $DIR/DECISION — apply the diff to CUE_VOCAB and rebuild to adopt."
    ;;
  *)
    echo "rejected" > "$DIR/DECISION"
    echo "recorded REJECTED at $DIR/DECISION — the live config is untouched."
    exit 1
    ;;
esac

#!/bin/bash
# DARWIN boot wrapper: darwind daemon.
# Invoked by the com.darwin.daemon LaunchAgent. Resolves the project root
# from its own location so the plist only needs to point at this script.
set -euo pipefail

DARWIN_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$DARWIN_ROOT"

# Gitignored secrets (e.g. export ANTHROPIC_API_KEY=... for cloud fallback).
if [ -f "$DARWIN_ROOT/state/env.sh" ]; then
    # shellcheck disable=SC1091
    source "$DARWIN_ROOT/state/env.sh"
fi

export DARWIN_ROOT

# Guardrail: with KeepAlive=true, a missing binary would otherwise be a silent
# ~10s crash-loop spamming state/logs/launchd-daemon.log. Fail loudly.
DARWIND="$DARWIN_ROOT/daemon/target/release/darwind"
if [ ! -x "$DARWIND" ]; then
    echo "error: $DARWIND missing — run scripts/install_boot.sh --install (builds it) or cargo build --release" >&2
    exit 78  # EX_CONFIG
fi

exec "$DARWIND"

#!/bin/bash
# DARWIN boot wrapper: MLX inference server.
# Invoked by the com.darwin.inference LaunchAgent. Resolves the project root
# from its own location so the plist only needs to point at this script.
set -euo pipefail

DARWIN_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$DARWIN_ROOT"

# Gitignored secrets (e.g. export ANTHROPIC_API_KEY=...).
if [ -f "$DARWIN_ROOT/state/env.sh" ]; then
    # shellcheck disable=SC1091
    source "$DARWIN_ROOT/state/env.sh"
fi

export DARWIN_ROOT

# Guardrail: with KeepAlive=true, a missing venv would otherwise be a silent
# ~10s crash-loop spamming state/logs/launchd-inference.log. Fail loudly.
PYTHON="$DARWIN_ROOT/.venv/bin/python"
if [ ! -x "$PYTHON" ]; then
    echo "error: $PYTHON missing — create the venv per the README Quick start" >&2
    exit 78  # EX_CONFIG
fi

exec "$PYTHON" "$DARWIN_ROOT/inference/server.py"

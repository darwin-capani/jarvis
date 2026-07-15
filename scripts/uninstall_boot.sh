#!/bin/bash
# uninstall_boot.sh — remove the boot-to-DARWIN LaunchAgents.
# Thin wrapper around install_boot.sh --uninstall.
set -euo pipefail

exec "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/install_boot.sh" --uninstall

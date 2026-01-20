#!/usr/bin/env bash
# Backwards-compatible wrapper for the updater script.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
exec "${SCRIPT_DIR}/update_app.sh" "$@"

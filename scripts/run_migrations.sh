#!/usr/bin/env bash
# Run SQL migrations on the node (intended for CI deploy steps).
#
# Usage:
#   sudo -n bash scripts/run_migrations.sh --service pdes-cdn --migrations ./migrations
#
# Notes:
# - Reads DATABASE_URL from /etc/<service>.env (or /etc/pdes-cdn.env fallback).
# - Executes all 000*.sql files in lexical order.

set -euo pipefail

die() {
  echo "error: $*" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "Missing required command: $1"
}

SERVICE_NAME="pdes-cdn"
MIGRATIONS_DIR=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --service) SERVICE_NAME="${2:-}"; shift 2 ;;
    --migrations) MIGRATIONS_DIR="${2:-}"; shift 2 ;;
    -h|--help)
      sed -n '1,60p' "$0"
      exit 0
      ;;
    *) die "Unknown argument: $1" ;;
  esac
done

[[ -n "$MIGRATIONS_DIR" ]] || die "Missing --migrations"
[[ -d "$MIGRATIONS_DIR" ]] || die "Migrations dir not found: $MIGRATIONS_DIR"

need_cmd psql
need_cmd find
need_cmd sort

ENV_FILE="/etc/${SERVICE_NAME}.env"
if [[ ! -f "$ENV_FILE" && -f /etc/pdes-cdn.env ]]; then
  ENV_FILE="/etc/pdes-cdn.env"
fi
[[ -f "$ENV_FILE" ]] || die "Missing env file: $ENV_FILE"

# shellcheck disable=SC1090
source "$ENV_FILE"
: "${DATABASE_URL:?DATABASE_URL not set in ${ENV_FILE}}"

mapfile -t migration_files < <(find "$MIGRATIONS_DIR" -maxdepth 1 -type f -name '000*.sql' | sort)
[[ "${#migration_files[@]}" -gt 0 ]] || die "No migrations found in ${MIGRATIONS_DIR}"

echo "Running ${#migration_files[@]} migration file(s) from ${MIGRATIONS_DIR}..."
for f in "${migration_files[@]}"; do
  echo "  - $(basename "$f")"
  psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f "$f" >/dev/null
done

echo "Migrations complete."

#!/usr/bin/env bash
# Deploy a built pdes-cdn binary + templates onto THIS machine (intended for a self-hosted GH runner).
#
# This script intentionally does NOT manage `/etc/pdes-cdn.env`.
#
# Usage (as root):
#   bash scripts/deploy_node.sh --bin ./artifacts/pdes-cdn-x86_64-unknown-linux-gnu --templates ./templates
#
# Optional flags:
#   --app-dir /opt/pdes-cdn
#   --service pdes-cdn
#   --migrations ./migrations
#   --dry-run

set -euo pipefail

die() {
  echo "error: $*" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "Missing required command: $1"
}

if [[ "$(id -u)" -ne 0 ]]; then
  die "Run as root (e.g., sudo bash scripts/deploy_node.sh ...)"
fi

need_cmd cp
need_cmd mv
need_cmd rm
need_cmd stat
need_cmd systemctl
need_cmd bash

APP_DIR="/opt/pdes-cdn"
SERVICE_NAME="pdes-cdn"
BIN_SRC=""
TEMPLATES_SRC=""
MIGRATIONS_DIR=""
DRY_RUN=0
SKIP_MIGRATIONS=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --app-dir) APP_DIR="${2:-}"; shift 2 ;;
    --service) SERVICE_NAME="${2:-}"; shift 2 ;;
    --bin) BIN_SRC="${2:-}"; shift 2 ;;
    --templates) TEMPLATES_SRC="${2:-}"; shift 2 ;;
    --migrations) MIGRATIONS_DIR="${2:-}"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    --skip-migrations) SKIP_MIGRATIONS=1; shift ;;
    -h|--help)
      sed -n '1,60p' "$0"
      exit 0
      ;;
    *) die "Unknown argument: $1" ;;
  esac
done

[[ -n "$BIN_SRC" ]] || die "Missing --bin"
[[ -n "$TEMPLATES_SRC" ]] || die "Missing --templates"
[[ -f "$BIN_SRC" ]] || die "Binary not found: $BIN_SRC"
[[ -d "$TEMPLATES_SRC" ]] || die "Templates dir not found: $TEMPLATES_SRC"
[[ -d "$APP_DIR" ]] || die "APP_DIR does not exist: $APP_DIR"

APP_USER="$(stat -c '%U' "$APP_DIR" 2>/dev/null || echo pdescdn)"
APP_GROUP="$(stat -c '%G' "$APP_DIR" 2>/dev/null || echo "$APP_USER")"

echo "Deploying to this node:"
echo "  APP_DIR=${APP_DIR}"
echo "  SERVICE_NAME=${SERVICE_NAME}"
echo "  BIN_SRC=${BIN_SRC}"
echo "  TEMPLATES_SRC=${TEMPLATES_SRC}"
echo "  MIGRATIONS_DIR=${MIGRATIONS_DIR:-auto}"
echo "  APP_USER=${APP_USER}"
echo "  APP_GROUP=${APP_GROUP}"
echo "  DRY_RUN=${DRY_RUN}"
echo "  SKIP_MIGRATIONS=${SKIP_MIGRATIONS}"

if [[ "$DRY_RUN" == "1" ]]; then
  echo "DRY_RUN=1: would stage:"
  echo "  ${APP_DIR}/pdes-cdn.new"
  echo "  ${APP_DIR}/templates.new"
  exit 0
fi

cp "$BIN_SRC" "${APP_DIR}/pdes-cdn.new"
chown "$APP_USER":"$APP_GROUP" "${APP_DIR}/pdes-cdn.new"
chmod +x "${APP_DIR}/pdes-cdn.new"
mv -f "${APP_DIR}/pdes-cdn.new" "${APP_DIR}/pdes-cdn"

rm -rf "${APP_DIR}/templates.new"
cp -a "$TEMPLATES_SRC" "${APP_DIR}/templates.new"
chown -R "$APP_USER":"$APP_GROUP" "${APP_DIR}/templates.new"
rm -rf "${APP_DIR}/templates"
mv -f "${APP_DIR}/templates.new" "${APP_DIR}/templates"

if [[ "$SKIP_MIGRATIONS" != "1" ]]; then
  script_dir="$(cd "$(dirname "$0")" && pwd)"
  repo_root="$(cd "${script_dir}/.." && pwd)"
  if [[ -z "$MIGRATIONS_DIR" && -d "${repo_root}/migrations" ]]; then
    MIGRATIONS_DIR="${repo_root}/migrations"
  fi

  if [[ -n "$MIGRATIONS_DIR" && -d "$MIGRATIONS_DIR" && -f "${repo_root}/scripts/run_migrations.sh" ]]; then
    echo "Running migrations from ${MIGRATIONS_DIR}..."
    bash "${repo_root}/scripts/run_migrations.sh" --service "${SERVICE_NAME}" --migrations "${MIGRATIONS_DIR}"
  else
    echo "Skipping migrations (migrations dir not available to deploy script)."
  fi
else
  echo "Skipping migrations (--skip-migrations)."
fi

systemctl restart "${SERVICE_NAME}"
systemctl status "${SERVICE_NAME}" --no-pager --full

echo "Deploy complete."

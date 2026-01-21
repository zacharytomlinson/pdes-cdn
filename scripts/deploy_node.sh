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
DRY_RUN=0
SKIP_MIGRATIONS=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --app-dir) APP_DIR="${2:-}"; shift 2 ;;
    --service) SERVICE_NAME="${2:-}"; shift 2 ;;
    --bin) BIN_SRC="${2:-}"; shift 2 ;;
    --templates) TEMPLATES_SRC="${2:-}"; shift 2 ;;
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
  ENV_FILE="/etc/${SERVICE_NAME}.env"
  if [[ ! -f "$ENV_FILE" && -f /etc/pdes-cdn.env ]]; then
    ENV_FILE="/etc/pdes-cdn.env"
  fi

  if [[ -f "$ENV_FILE" ]]; then
    # shellcheck disable=SC1090
    source "$ENV_FILE"
  fi

  if command -v psql >/dev/null 2>&1 && [[ -n "${DATABASE_URL:-}" ]]; then
    echo "Running migrations (best-effort)..."
    psql "$DATABASE_URL" >/dev/null <<'SQL'
create table if not exists apps (
    id uuid primary key,
    slug text not null unique,
    name text not null,
    description text,
    created_at timestamptz not null default now()
);

create table if not exists app_packages (
    id uuid primary key,
    app_id uuid not null references apps(id) on delete cascade,
    version text not null,
    os text not null default 'unknown',
    arch text not null default 'x86_64',
    pkg_file_id uuid not null references cached_files(id) on delete restrict,
    pkg_sha256 text not null,
    created_at timestamptz not null default now(),
    unique (app_id, version, os, arch)
);

create table if not exists firmware_releases (
    id uuid primary key,
    app_id uuid not null references apps(id) on delete cascade,
    channel text not null default 'stable',
    version text not null,
    file_id uuid not null references cached_files(id) on delete restrict,
    sha256 text not null,
    created_at timestamptz not null default now(),
    unique (app_id, channel, version)
);

create table if not exists app_assignments (
    app_id uuid not null references apps(id) on delete cascade,
    user_id uuid not null references users(id) on delete cascade,
    created_at timestamptz not null default now(),
    primary key (app_id, user_id)
);

create index if not exists idx_app_assignments_user on app_assignments(user_id);
create index if not exists idx_app_packages_app_created_at on app_packages(app_id, created_at desc);
create index if not exists idx_firmware_releases_app_created_at on firmware_releases(app_id, created_at desc);
create index if not exists idx_app_packages_sha256 on app_packages(app_id, pkg_sha256);
create index if not exists idx_firmware_releases_sha256 on firmware_releases(app_id, sha256);

create table if not exists api_tokens (
    id uuid primary key,
    user_id uuid not null references users(id) on delete cascade,
    name text not null default '',
    token_sha256 text not null unique,
    created_at timestamptz not null default now(),
    last_used_at timestamptz
);

create index if not exists idx_api_tokens_user_id on api_tokens(user_id);

-- upgrade existing installs that may have the older (app_id, version) unique constraint
alter table app_packages
    add column if not exists os text not null default 'unknown';
alter table app_packages
    add column if not exists arch text not null default 'x86_64';
alter table app_packages
    drop constraint if exists app_packages_app_id_version_key;
alter table app_packages
    drop constraint if exists app_packages_app_id_version_os_arch_key;
alter table app_packages
    add constraint app_packages_app_id_version_os_arch_key unique (app_id, version, os, arch);
SQL
  else
    echo "Skipping migrations (psql not found or DATABASE_URL not set in ${ENV_FILE})."
  fi
else
  echo "Skipping migrations (--skip-migrations)."
fi

systemctl restart "${SERVICE_NAME}"
systemctl status "${SERVICE_NAME}" --no-pager --full

echo "Deploy complete."

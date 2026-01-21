#!/usr/bin/env bash
# Update the deployed pdes-cdn binary + templates for the current architecture.
#
# Usage (run as root):
#   REPO=org/repo RELEASE_TAG=v0.0.6 bash scripts/update_app.sh
#
# Inputs:
#   - APP_DIR (default: /opt/pdes-cdn)
#   - SERVICE_NAME (default: pdes-cdn)
#   - REPO (default: zacharytomlinson/pdes-cdn)
#   - RELEASE_TAG (default: latest; resolves automatically for public repos)
#   - GH_DOWNLOAD_TOKEN or GITHUB_TOKEN (optional; required for private repos)
#   - ASSET_URL_OVERRIDE (optional; direct URL or GitHub asset API URL)
#   - DRY_RUN=1 (optional; downloads + stages but doesn't install/restart)

set -euo pipefail

if [[ "$(id -u)" -ne 0 ]]; then
  echo "Please run as root (e.g., sudo bash scripts/update_app.sh)" >&2
  exit 1
fi

die() {
  echo "error: $*" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "Missing required command: $1"
}

APP_DIR="${APP_DIR:-/opt/pdes-cdn}"
SERVICE_NAME="${SERVICE_NAME:-pdes-cdn}"
REPO="${REPO:-zacharytomlinson/pdes-cdn}"
RELEASE_TAG="${RELEASE_TAG:-}"
GH_DOWNLOAD_TOKEN="${GH_DOWNLOAD_TOKEN:-${GITHUB_TOKEN:-}}"
ASSET_URL_OVERRIDE="${ASSET_URL_OVERRIDE:-}"
DRY_RUN="${DRY_RUN:-0}"
if [[ "$DRY_RUN" != "0" && "$DRY_RUN" != "1" ]]; then
  die "DRY_RUN must be 0 or 1 (got: ${DRY_RUN})"
fi

ARCH="$(uname -m)"
case "$ARCH" in
  x86_64|amd64) TARGET_TRIPLE="x86_64-unknown-linux-gnu" ;;
  aarch64|arm64) TARGET_TRIPLE="aarch64-unknown-linux-gnu" ;;
  *) echo "Unsupported architecture: $ARCH" >&2; exit 1 ;;
esac
ASSET_NAME="pdes-cdn-${TARGET_TRIPLE}"

need_cmd curl
need_cmd tar
need_cmd systemctl
need_cmd find
need_cmd stat
need_cmd mktemp

BIN_URL=""
TARBALL_URL=""

github_api_base_headers=(
  -H "Accept: application/vnd.github+json"
  -H "X-GitHub-Api-Version: 2022-11-28"
  -H "User-Agent: pdes-cdn-updater"
)
github_api_auth_headers=()
if [[ -n "$GH_DOWNLOAD_TOKEN" ]]; then
  github_api_auth_headers=(-H "Authorization: Bearer ${GH_DOWNLOAD_TOKEN}")
fi

resolve_release_tag() {
  if [[ -n "$RELEASE_TAG" && "$RELEASE_TAG" != "latest" ]]; then
    return 0
  fi

  if [[ -z "$GH_DOWNLOAD_TOKEN" ]]; then
    # Public repo path: follow the redirect and infer the tag from the final URL.
    local effective_url tag
    effective_url="$(
      curl -fsSL -o /dev/null -w '%{url_effective}' "https://github.com/${REPO}/releases/latest"
    )" || die "Failed to resolve latest release tag for ${REPO} (repo may be private; set GH_DOWNLOAD_TOKEN)"
    tag="${effective_url##*/}"
    [[ -n "$tag" && "$tag" != "latest" ]] || die "Failed to parse release tag from ${effective_url}"
    RELEASE_TAG="$tag"
    return 0
  fi

  # Private repo path: use the API and parse JSON with jq.
  need_cmd jq
  local release_json
  release_json="$(
    curl -fsSL \
      "${github_api_base_headers[@]}" \
      "${github_api_auth_headers[@]}" \
      "https://api.github.com/repos/${REPO}/releases/latest"
  )" || die "Failed to fetch release metadata for ${REPO}"
  RELEASE_TAG="$(jq -r '.tag_name // empty' <<<"$release_json")"
  [[ -n "$RELEASE_TAG" ]] || die "Failed to resolve latest release tag via API for ${REPO}"
}

download_url() {
  local url="$1"
  local out="$2"
  local mode="${3:-}"

  local headers=()
  if [[ -n "$GH_DOWNLOAD_TOKEN" ]]; then
    headers+=(-H "Authorization: Bearer ${GH_DOWNLOAD_TOKEN}")
  fi

  if [[ "$mode" == "asset_api" ]]; then
    # Asset API URL; must request octet-stream.
    headers+=(-H "Accept: application/octet-stream")
  fi

  curl -fL "${headers[@]}" -o "$out" "$url"
}

resolve_urls() {
  BIN_URL="https://github.com/${REPO}/releases/download/${RELEASE_TAG}/${ASSET_NAME}"
  TARBALL_URL="https://api.github.com/repos/${REPO}/tarball/${RELEASE_TAG}"
}

# Detect owner from existing app dir if possible.
if [[ ! -d "$APP_DIR" ]]; then
  die "APP_DIR does not exist: ${APP_DIR}"
fi
APP_USER="$(stat -c '%U' "$APP_DIR" 2>/dev/null || echo pdescdn)"
APP_GROUP="$(stat -c '%G' "$APP_DIR" 2>/dev/null || echo "$APP_USER")"

TMPDIR="$(mktemp -d)"
cleanup() { rm -rf "$TMPDIR"; }
trap cleanup EXIT

resolve_release_tag
resolve_urls

echo "Resolved variables:"
echo "  REPO=${REPO}"
echo "  RELEASE_TAG=${RELEASE_TAG}"
echo "  TARGET_TRIPLE=${TARGET_TRIPLE}"
echo "  ASSET_NAME=${ASSET_NAME}"
echo "  APP_DIR=${APP_DIR}"
echo "  SERVICE_NAME=${SERVICE_NAME}"
echo "  DRY_RUN=${DRY_RUN}"
echo "  ASSET_URL_OVERRIDE=${ASSET_URL_OVERRIDE}"

echo "Resolved URLs:"
echo "  BIN_URL=${BIN_URL}"
echo "  TARBALL_URL=${TARBALL_URL}"

echo "[1/4] Downloading binary (${ASSET_NAME})..."
ASSET_DOWNLOAD_URL=""
if [[ -n "$ASSET_URL_OVERRIDE" ]]; then
  echo "Using ASSET_URL_OVERRIDE=${ASSET_URL_OVERRIDE}"
  ASSET_DOWNLOAD_URL="$ASSET_URL_OVERRIDE"
elif [[ -n "$GH_DOWNLOAD_TOKEN" ]]; then
  need_cmd jq
  RELEASE_JSON="$TMPDIR/release.json"
  curl -fsSL \
    "${github_api_base_headers[@]}" \
    "${github_api_auth_headers[@]}" \
    -o "$RELEASE_JSON" \
    "https://api.github.com/repos/${REPO}/releases/tags/${RELEASE_TAG}" \
    || die "Failed to fetch release metadata for ${REPO} ${RELEASE_TAG}"
  ASSET_DOWNLOAD_URL="$(jq -r --arg name "$ASSET_NAME" '.assets[]? | select(.name==$name) | .url' "$RELEASE_JSON" | head -n1)"
  [[ -n "$ASSET_DOWNLOAD_URL" && "$ASSET_DOWNLOAD_URL" != "null" ]] || die "Asset ${ASSET_NAME} not found in release ${RELEASE_TAG}"
else
  ASSET_DOWNLOAD_URL="$BIN_URL"
fi

echo "Downloading from: ${ASSET_DOWNLOAD_URL}"
if [[ "$ASSET_DOWNLOAD_URL" == https://api.github.com/* ]]; then
  download_url "$ASSET_DOWNLOAD_URL" "$TMPDIR/pdes-cdn" "asset_api"
else
  download_url "$ASSET_DOWNLOAD_URL" "$TMPDIR/pdes-cdn"
fi
chmod +x "$TMPDIR/pdes-cdn"

echo "[2/4] Refreshing templates..."
download_url "$TARBALL_URL" "$TMPDIR/src.tar.gz"
tar -xzf "$TMPDIR/src.tar.gz" -C "$TMPDIR"
SRC_ROOT=$(find "$TMPDIR" -maxdepth 1 -mindepth 1 -type d | head -n1)
if [[ -d "$SRC_ROOT/templates" ]]; then
  rm -rf "${APP_DIR}/templates.new"
  cp -r "$SRC_ROOT/templates" "${APP_DIR}/templates.new"
  chown -R "$APP_USER":"$APP_GROUP" "${APP_DIR}/templates.new"
else
  echo "Templates directory not found in tarball; aborting." >&2
  exit 1
fi

echo "[3/4] Installing binary..."
if [[ "$DRY_RUN" == "1" ]]; then
  echo "DRY_RUN=1: skipping install + restart. Staged files:"
  echo "  ${TMPDIR}/pdes-cdn"
  echo "  ${APP_DIR}/templates.new"
  exit 0
fi

cp "$TMPDIR/pdes-cdn" "${APP_DIR}/pdes-cdn.new"
chown "$APP_USER":"$APP_GROUP" "${APP_DIR}/pdes-cdn.new"
chmod +x "${APP_DIR}/pdes-cdn.new"
mv -f "${APP_DIR}/pdes-cdn.new" "${APP_DIR}/pdes-cdn"

rm -rf "${APP_DIR}/templates"
mv -f "${APP_DIR}/templates.new" "${APP_DIR}/templates"

echo "[4/4] Running migrations + restarting service..."
ENV_FILE="/etc/${SERVICE_NAME}.env"
if [[ -f "$ENV_FILE" ]]; then
  # shellcheck disable=SC1090
  source "$ENV_FILE"
fi
if command -v psql >/dev/null 2>&1 && [[ -n "${DATABASE_URL:-}" ]]; then
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

systemctl restart "${SERVICE_NAME}"
systemctl status "${SERVICE_NAME}" --no-pager --full

echo "Update complete for ${RELEASE_TAG} (${TARGET_TRIPLE})."

# Example invocation (run as root):
# REPO=palodurosystems/pdes-cdn \
# RELEASE_TAG=v0.0.6 \
# GH_DOWNLOAD_TOKEN=...optional... \
# bash scripts/update_app.sh

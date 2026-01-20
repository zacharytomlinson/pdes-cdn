#!/usr/bin/env bash
# Update the deployed pdes-cdn binary + templates for the current architecture.

set -euo pipefail

if [[ "$(id -u)" -ne 0 ]]; then
  echo "Please run as root (e.g., sudo bash scripts/update_app.sh)" >&2
  exit 1
fi

APP_DIR="/opt/pdes-cdn"
SERVICE_NAME="pdes-cdn"
REPO="${REPO:-palodurosystems/pdes-cdn}"
RELEASE_TAG="${RELEASE_TAG:-}"
GH_DOWNLOAD_TOKEN="${GH_DOWNLOAD_TOKEN:-}"
ASSET_URL_OVERRIDE="${ASSET_URL_OVERRIDE:-}"

ARCH="$(uname -m)"
case "$ARCH" in
  x86_64|amd64) TARGET_TRIPLE="x86_64-unknown-linux-gnu" ;;
  aarch64|arm64) TARGET_TRIPLE="aarch64-unknown-linux-gnu" ;;
  *) echo "Unsupported architecture: $ARCH" >&2; exit 1 ;;
esac
ASSET_NAME="pdes-cdn-${TARGET_TRIPLE}"
BIN_URL="https://github.com/${REPO}/releases/download/${RELEASE_TAG}/${ASSET_NAME}"
TARBALL_URL="https://api.github.com/repos/${REPO}/tarball/${RELEASE_TAG}"
echo "Resolved variables:"
echo "  REPO=${REPO}"
echo "  RELEASE_TAG=${RELEASE_TAG}"
echo "  TARGET_TRIPLE=${TARGET_TRIPLE}"
echo "  ASSET_NAME=${ASSET_NAME}"
echo "  BIN_URL=${BIN_URL}"
echo "  ASSET_URL_OVERRIDE=${ASSET_URL_OVERRIDE}"

# Detect owner from existing app dir if possible.
APP_USER="$(stat -c '%U' "$APP_DIR" 2>/dev/null || echo pdescdn)"
APP_GROUP="$(stat -c '%G' "$APP_DIR" 2>/dev/null || echo "$APP_USER")"

TMPDIR="$(mktemp -d)"
cleanup() { rm -rf "$TMPDIR"; }
trap cleanup EXIT

echo "[1/3] Downloading binary (${ASSET_NAME})..."
echo "API lookup will query: https://api.github.com/repos/${REPO}/releases/${RELEASE_TAG:+tags/}${RELEASE_TAG:-latest}"
ASSET_API_URL=$(python3 - <<'PYCODE' || true
import os, json, sys, urllib.request
token = os.environ.get("GH_DOWNLOAD_TOKEN")
repo = os.environ.get("REPO")
tag = os.environ.get("RELEASE_TAG")
name = os.environ.get("ASSET_NAME")
use_latest = not bool(tag)
headers = {
    "Accept": "application/vnd.github+json",
    "User-Agent": "pdes-updater",
}
if token:
    headers["Authorization"] = f"Bearer {token}"
if use_latest:
    url = f"https://api.github.com/repos/{repo}/releases/latest"
else:
    url = f"https://api.github.com/repos/{repo}/releases/tags/{tag}"
req = urllib.request.Request(
    url,
    headers=headers,
)
try:
    with urllib.request.urlopen(req) as resp:
        status = resp.status
        data = json.load(resp)
    tag_resolved = data.get("tag_name")
    for a in data.get("assets", []):
        if a.get("name") == name:
            print(a.get("url", ""))
            print(tag_resolved or "")
            sys.exit(0)
    print(f"NO_ASSET|{status}|{url}", file=sys.stderr)
    sys.exit(1)
except Exception as e:
    print(f"ERROR|{url}|{e}", file=sys.stderr)
    sys.exit(1)
PYCODE
)
TAG_RESOLVED=$(printf '%s' "$ASSET_API_URL" | tail -n1)
ASSET_API_URL=$(printf '%s' "$ASSET_API_URL" | head -n1)
if [[ -z "$RELEASE_TAG" && -n "$TAG_RESOLVED" ]]; then
  RELEASE_TAG="$TAG_RESOLVED"
  echo "Resolved RELEASE_TAG via API: ${RELEASE_TAG}"
fi
DOWNLOAD_URL="$ASSET_API_URL"
EXTRA_HEADERS=()
EXTRA_HEADERS+=(-H "Accept: application/octet-stream")
if [[ -n "$GH_DOWNLOAD_TOKEN" ]]; then
  EXTRA_HEADERS+=(-H "Authorization: Bearer ${GH_DOWNLOAD_TOKEN}")
fi

if [[ -n "$ASSET_URL_OVERRIDE" ]]; then
  echo "Using ASSET_URL_OVERRIDE=${ASSET_URL_OVERRIDE}"
  DOWNLOAD_URL="$ASSET_URL_OVERRIDE"
elif [[ -z "$ASSET_API_URL" ]]; then
  echo "Asset ${ASSET_NAME} not found via API on release ${RELEASE_TAG}." >&2
  exit 1
else
  echo "Resolved asset via API: ${ASSET_API_URL}"
fi

echo "Downloading from: ${DOWNLOAD_URL}"
curl -fL "${EXTRA_HEADERS[@]}" -o "$TMPDIR/pdes-cdn" "$DOWNLOAD_URL"
chmod +x "$TMPDIR/pdes-cdn"

echo "[2/3] Refreshing templates..."
if [[ -n "$GH_DOWNLOAD_TOKEN" ]]; then
  curl -fL \
    -H "Authorization: Bearer ${GH_DOWNLOAD_TOKEN}" \
    -H "Accept: application/vnd.github+json" \
    -o "$TMPDIR/src.tar.gz" \
    "$TARBALL_URL"
else
  curl -fL -o "$TMPDIR/src.tar.gz" "$TARBALL_URL"
fi
tar -xzf "$TMPDIR/src.tar.gz" -C "$TMPDIR"
SRC_ROOT=$(find "$TMPDIR" -maxdepth 1 -mindepth 1 -type d | head -n1)
if [[ -d "$SRC_ROOT/templates" ]]; then
  rm -rf "${APP_DIR}/templates"
  cp -r "$SRC_ROOT/templates" "$APP_DIR/"
  chown -R "$APP_USER":"$APP_GROUP" "${APP_DIR}/templates"
else
  echo "Templates directory not found in tarball; aborting." >&2
  exit 1
fi

echo "[3/3] Installing binary and restarting service..."
cp "$TMPDIR/pdes-cdn" "${APP_DIR}/pdes-cdn"
chown "$APP_USER":"$APP_GROUP" "${APP_DIR}/pdes-cdn"
chmod +x "${APP_DIR}/pdes-cdn"

systemctl restart "${SERVICE_NAME}"
systemctl status "${SERVICE_NAME}" --no-pager

echo "Update complete for ${RELEASE_TAG} (${TARGET_TRIPLE})."

# Example invocation (run as root):
# REPO=palodurosystems/pdes-cdn \
# RELEASE_TAG=v0.0.6 \
# GH_DOWNLOAD_TOKEN=...optional... \
# bash scripts/update_app.sh

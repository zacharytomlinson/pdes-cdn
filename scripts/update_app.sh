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
RELEASE_TAG="${RELEASE_TAG:?set release tag (e.g., v0.0.6)}"
GH_DOWNLOAD_TOKEN="${GH_DOWNLOAD_TOKEN:-}"

ARCH="$(uname -m)"
case "$ARCH" in
  x86_64|amd64) TARGET_TRIPLE="x86_64-unknown-linux-gnu" ;;
  aarch64|arm64) TARGET_TRIPLE="aarch64-unknown-linux-gnu" ;;
  *) echo "Unsupported architecture: $ARCH" >&2; exit 1 ;;
esac
ASSET_NAME="pdes-cdn-${TARGET_TRIPLE}"
BIN_URL="https://github.com/${REPO}/releases/download/${RELEASE_TAG}/${ASSET_NAME}"
TARBALL_URL="https://api.github.com/repos/${REPO}/tarball/${RELEASE_TAG}"

# Detect owner from existing app dir if possible.
APP_USER="$(stat -c '%U' "$APP_DIR" 2>/dev/null || echo pdescdn)"
APP_GROUP="$(stat -c '%G' "$APP_DIR" 2>/dev/null || echo "$APP_USER")"

TMPDIR="$(mktemp -d)"
cleanup() { rm -rf "$TMPDIR"; }
trap cleanup EXIT

echo "[1/3] Downloading binary (${ASSET_NAME})..."
if [[ -n "$GH_DOWNLOAD_TOKEN" ]]; then
  curl -fL \
    -H "Authorization: Bearer ${GH_DOWNLOAD_TOKEN}" \
    -o "$TMPDIR/pdes-cdn" \
    "$BIN_URL"
else
  curl -fL -o "$TMPDIR/pdes-cdn" "$BIN_URL"
fi
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

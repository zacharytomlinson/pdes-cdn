#!/usr/bin/env bash
# One-shot installer for Debian: installs deps, downloads release binary, configures PostgreSQL,
# sets up nginx + certbot, and seeds an initial writer user. After running, edit the env file
# and start the service.

set -euo pipefail

if [[ "$(id -u)" -ne 0 ]]; then
  echo "Please run as root (sudo bash scripts/install_deps.sh)" >&2
  exit 1
fi

APP_USER="pdescdn"
APP_DIR="/opt/pdes-cdn"
CACHE_DIR_DEFAULT="/var/lib/pdes-cdn/cache"
CHALLENGE_ROOT="/var/www/letsencrypt"
SERVICE_NAME="pdes-cdn"
DB_NAME="${DB_NAME:-pdes_cdn}"
DB_USER="${DB_USER:-pdes_cdn_user}"
DB_PASS="${DB_PASS:-}"
GH_DOWNLOAD_TOKEN="${GH_DOWNLOAD_TOKEN:-}"
REPO="${REPO:-OWNER/REPO}"              # e.g., palodurosystems/pdes-cdn
RELEASE_TAG="${RELEASE_TAG:-}"          # e.g., v0.0.1
DOMAIN="${DOMAIN:-example.com}"         # set to your domain
# Optional: space-separated list of additional hostnames that should redirect to DOMAIN.
# Example: REDIRECT_DOMAINS="cdn.example.com old-apps.example.com"
REDIRECT_DOMAINS="${REDIRECT_DOMAINS:-}"
CERTBOT_EMAIL="${CERTBOT_EMAIL:-support@palodurosystems.io}"
PORT="${PORT:-3000}"
# If set to 1, nginx will only accept inbound HTTP(S) from Cloudflare IP ranges and
# will trust CF-Connecting-IP for client IP.
# IMPORTANT: Only enable if your DNS record is Proxied (orange cloud), otherwise you'll block yourself.
CLOUDFLARE_ONLY="${CLOUDFLARE_ONLY:-0}"

if [[ -z "$DB_PASS" ]]; then
  # URL- and SQL-safe.
  DB_PASS="$(openssl rand -hex 24)"
fi

if [[ -z "$RELEASE_TAG" || "$RELEASE_TAG" == "latest" ]]; then
  echo "Set RELEASE_TAG to the release tag (e.g., v0.0.1)." >&2
  exit 1
fi
if [[ "$REPO" == "OWNER/REPO" ]]; then
  echo "Set REPO to your GitHub org/repo (e.g., palodurosystems/pdes-cdn)." >&2
  exit 1
fi

echo "[1/8] Installing system dependencies..."
apt-get update
DEBIAN_FRONTEND=noninteractive apt-get install -y \
  curl ca-certificates gnupg lsb-release jq \
  postgresql postgresql-contrib libpq-dev \
  pkg-config openssl ssl-cert \
  nginx certbot python3-certbot-nginx

echo "[2/8] Creating app user and directories..."
id -u "$APP_USER" >/dev/null 2>&1 || useradd --system --create-home --shell /usr/sbin/nologin "$APP_USER"
mkdir -p "$APP_DIR" "$CACHE_DIR_DEFAULT"
chown -R "$APP_USER":"$APP_USER" "$APP_DIR" "$CACHE_DIR_DEFAULT"

echo "[3/8] Downloading release binary..."
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64|amd64) TARGET_TRIPLE="x86_64-unknown-linux-gnu" ;;
  aarch64|arm64) TARGET_TRIPLE="aarch64-unknown-linux-gnu" ;;
  *) echo "Unsupported architecture: $ARCH" >&2; exit 1 ;;
esac
ASSET_NAME="pdes-cdn-${TARGET_TRIPLE}"
BIN_URL="https://github.com/${REPO}/releases/download/${RELEASE_TAG}/${ASSET_NAME}"
if [[ -n "$GH_DOWNLOAD_TOKEN" ]]; then
  # Use the asset API URL for private repos (avoids relying on github.com session cookies).
  ASSET_API_URL="$(
    curl -fsSL \
      -H "Authorization: Bearer ${GH_DOWNLOAD_TOKEN}" \
      -H "Accept: application/vnd.github+json" \
      -H "X-GitHub-Api-Version: 2022-11-28" \
      -H "User-Agent: pdes-installer" \
      "https://api.github.com/repos/${REPO}/releases/tags/${RELEASE_TAG}" \
    | jq -r --arg name "$ASSET_NAME" '.assets[]? | select(.name==$name) | .url' \
    | head -n1
  )" || true
  if [[ -n "$ASSET_API_URL" ]]; then
    curl -fL \
      -H "Authorization: Bearer ${GH_DOWNLOAD_TOKEN}" \
      -H "Accept: application/octet-stream" \
      "$ASSET_API_URL" -o "$APP_DIR/pdes-cdn"
  else
    curl -fL \
      -H "Authorization: Bearer ${GH_DOWNLOAD_TOKEN}" \
      "$BIN_URL" -o "$APP_DIR/pdes-cdn"
  fi
else
  curl -fL "$BIN_URL" -o "$APP_DIR/pdes-cdn"
fi
chmod +x "$APP_DIR/pdes-cdn"
chown "$APP_USER":"$APP_USER" "$APP_DIR/pdes-cdn"

echo "[3b/8] Fetching templates + migrations..."
SRC_TMP="$(mktemp -d)"
ARCHIVE_URL="https://api.github.com/repos/${REPO}/tarball/${RELEASE_TAG}"
if [[ -n "$GH_DOWNLOAD_TOKEN" ]]; then
  curl -fL \
    -H "Authorization: Bearer ${GH_DOWNLOAD_TOKEN}" \
    -H "Accept: application/vnd.github+json" \
    "$ARCHIVE_URL" -o "$SRC_TMP/src.tar.gz"
else
  curl -fL "$ARCHIVE_URL" -o "$SRC_TMP/src.tar.gz"
fi
tar -xzf "$SRC_TMP/src.tar.gz" -C "$SRC_TMP"
SRC_ROOT=$(find "$SRC_TMP" -maxdepth 1 -mindepth 1 -type d | head -n1)
if [[ -d "$SRC_ROOT/templates" ]]; then
  rm -rf "$APP_DIR/templates"
  cp -r "$SRC_ROOT/templates" "$APP_DIR/"
  chown -R "$APP_USER":"$APP_USER" "$APP_DIR/templates"
else
  echo "Templates directory not found in archive; aborting." >&2
  exit 1
fi
if [[ -d "$SRC_ROOT/migrations" ]]; then
  rm -rf "$APP_DIR/migrations"
  cp -r "$SRC_ROOT/migrations" "$APP_DIR/"
  chown -R "$APP_USER":"$APP_USER" "$APP_DIR/migrations"
else
  echo "Migrations directory not found in archive; aborting." >&2
  exit 1
fi
rm -rf "$SRC_TMP"

echo "[4/8] PostgreSQL setup (db/user)..."
sudo -u postgres psql -tc "SELECT 1 FROM pg_roles WHERE rolname='${DB_USER}'" | grep -q 1 || \
  sudo -u postgres psql -c "CREATE ROLE ${DB_USER} WITH LOGIN PASSWORD '${DB_PASS}';"
sudo -u postgres psql -tc "SELECT 1 FROM pg_database WHERE datname='${DB_NAME}'" | grep -q 1 || \
  sudo -u postgres psql -c "CREATE DATABASE ${DB_NAME} OWNER ${DB_USER};"
sudo -u postgres psql -d "$DB_NAME" -c "GRANT ALL ON SCHEMA public TO ${DB_USER};"

echo "[5/8] Running migrations..."
export DATABASE_URL="postgres://${DB_USER}:${DB_PASS}@localhost:5432/${DB_NAME}"
if [[ ! -d "$APP_DIR/migrations" ]]; then
  echo "Migrations directory not found at ${APP_DIR}/migrations; aborting." >&2
  exit 1
fi
mapfile -t migration_files < <(
  find "$APP_DIR/migrations" -maxdepth 1 -type f -name '[0-9][0-9][0-9][0-9]_*.sql' | sort
)
if [[ "${#migration_files[@]}" -eq 0 ]]; then
  echo "No migrations found in ${APP_DIR}/migrations; aborting." >&2
  exit 1
fi
for f in "${migration_files[@]}"; do
  echo "  - $(basename "$f")"
  psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f "$f" >/dev/null
done

echo "[6/8] Seed initial writer user (zacharytomlinson@palodurosystems.io)..."
SEED_WRITER_EMAIL="${SEED_WRITER_EMAIL:-zacharytomlinson@palodurosystems.io}"
SEED_WRITER_PASSWORD="${SEED_WRITER_PASSWORD:-$(openssl rand -hex 12)}"
PLACEHOLDER_HASH="$("${APP_DIR}/pdes-cdn" --hash-password "${SEED_WRITER_PASSWORD}")"
sudo -u postgres psql -d "$DB_NAME" <<SQL
insert into users (id, email, password_hash, permission, must_reset)
values ('11111111-1111-1111-1111-111111111112', '${SEED_WRITER_EMAIL}', '${PLACEHOLDER_HASH}', 'writer', true)
on conflict (email) do update
    set password_hash = excluded.password_hash,
        permission = 'writer',
        must_reset = true,
        reset_token = null,
        reset_token_expires = null;
SQL

echo "[7/8] Systemd service setup..."
cat >/etc/systemd/system/${SERVICE_NAME}.service <<EOF
[Unit]
Description=PDES CDN Node
After=network.target postgresql.service
RequiresMountsFor=/mnt/main_ssd

[Service]
User=${APP_USER}
Group=${APP_USER}
EnvironmentFile=/etc/${SERVICE_NAME}.env
WorkingDirectory=${APP_DIR}
ExecStart=${APP_DIR}/pdes-cdn
Restart=on-failure

[Install]
WantedBy=multi-user.target
EOF

if [[ ! -f /etc/${SERVICE_NAME}.env ]]; then
  COOKIE_SECRET_DEFAULT="${COOKIE_SECRET:-$(openssl rand -base64 48 | tr -d '\n')}"
  cat >/etc/${SERVICE_NAME}.env <<EOF
DATABASE_URL=postgres://${DB_USER}:${DB_PASS}@localhost:5432/${DB_NAME}
CACHE_DIR=${CACHE_DIR_DEFAULT}
COOKIE_SECRET=${COOKIE_SECRET_DEFAULT}
BASE_URL=https://${DOMAIN}
APP_NAME=PDES CDN Node
APP_BUILD=${RELEASE_TAG}
APP_COMMIT=
SMTP_HOST=
SMTP_PORT=587
SMTP_TLS=starttls
SMTP_USERNAME=
SMTP_PASSWORD=
SMTP_FROM=support@palodurosystems.io
MAX_UPLOAD_BYTES=200000000
EOF
  chown root:root /etc/${SERVICE_NAME}.env
  chmod 640 /etc/${SERVICE_NAME}.env
fi

echo "[8/8] nginx + certbot..."
mkdir -p "${CHALLENGE_ROOT}/.well-known/acme-challenge"

CF_REALIP_INCLUDE=""
CF_ALLOW_INCLUDE=""
PROXY_XFF_NGX="\\$proxy_add_x_forwarded_for"
if [[ "${CLOUDFLARE_ONLY}" == "1" ]]; then
  echo "CLOUDFLARE_ONLY=1: generating nginx Cloudflare allowlist snippets..."
  mkdir -p /etc/nginx/snippets

  CF_IPV4="$(curl -fsSL https://www.cloudflare.com/ips-v4)"
  CF_IPV6="$(curl -fsSL https://www.cloudflare.com/ips-v6)"

  cat >/etc/nginx/snippets/cloudflare-realip.conf <<EOF
# Generated by ${SERVICE_NAME} installer on $(date -u +%F).
# Source: https://www.cloudflare.com/ips
real_ip_header CF-Connecting-IP;
real_ip_recursive on;
EOF
  while IFS= read -r cidr; do
    [[ -n "${cidr}" ]] || continue
    echo "set_real_ip_from ${cidr};" >>/etc/nginx/snippets/cloudflare-realip.conf
  done <<<"${CF_IPV4}"
  while IFS= read -r cidr; do
    [[ -n "${cidr}" ]] || continue
    echo "set_real_ip_from ${cidr};" >>/etc/nginx/snippets/cloudflare-realip.conf
  done <<<"${CF_IPV6}"

  cat >/etc/nginx/snippets/cloudflare-allow.conf <<EOF
# Generated by ${SERVICE_NAME} installer on $(date -u +%F).
# Source: https://www.cloudflare.com/ips
EOF
  echo "allow 127.0.0.1;" >>/etc/nginx/snippets/cloudflare-allow.conf
  echo "allow ::1;" >>/etc/nginx/snippets/cloudflare-allow.conf
  while IFS= read -r cidr; do
    [[ -n "${cidr}" ]] || continue
    echo "allow ${cidr};" >>/etc/nginx/snippets/cloudflare-allow.conf
  done <<<"${CF_IPV4}"
  while IFS= read -r cidr; do
    [[ -n "${cidr}" ]] || continue
    echo "allow ${cidr};" >>/etc/nginx/snippets/cloudflare-allow.conf
  done <<<"${CF_IPV6}"
  echo "deny all;" >>/etc/nginx/snippets/cloudflare-allow.conf

  chmod 644 /etc/nginx/snippets/cloudflare-realip.conf /etc/nginx/snippets/cloudflare-allow.conf
  CF_REALIP_INCLUDE="include /etc/nginx/snippets/cloudflare-realip.conf;"
  CF_ALLOW_INCLUDE="include /etc/nginx/snippets/cloudflare-allow.conf;"
  PROXY_XFF_NGX="\\$remote_addr"
fi

NGINX_SITE="/etc/nginx/sites-available/${SERVICE_NAME}"
cat >"${NGINX_SITE}" <<EOF
server {
    listen 80 default_server;
    listen [::]:80 default_server;
    server_name ${DOMAIN};
    root ${CHALLENGE_ROOT};
    ${CF_ALLOW_INCLUDE}

    location ^~ /.well-known/acme-challenge/ {
        default_type text/plain;
        try_files \$uri =404;
    }

    location / { return 301 https://\$host\$request_uri; }
}
EOF

if [[ -n "${REDIRECT_DOMAINS}" ]]; then
  cat >>"${NGINX_SITE}" <<EOF

server {
    listen 80;
    listen [::]:80;
    server_name ${REDIRECT_DOMAINS};
    root ${CHALLENGE_ROOT};
    ${CF_ALLOW_INCLUDE}

    location ^~ /.well-known/acme-challenge/ {
        default_type text/plain;
        try_files \$uri =404;
    }

    location / { return 301 https://${DOMAIN}\$request_uri; }
}
EOF
fi

cat >>"${NGINX_SITE}" <<EOF

server {
    listen 443 ssl http2;
    listen [::]:443 ssl http2;
    server_name ${DOMAIN};
    ${CF_REALIP_INCLUDE}
    ${CF_ALLOW_INCLUDE}

    # Temporary self-signed cert so nginx can start; Certbot will replace these
    ssl_certificate /etc/ssl/certs/ssl-cert-snakeoil.pem;
    ssl_certificate_key /etc/ssl/private/ssl-cert-snakeoil.key;

    index index.html index.htm;
    # Match app-side MAX_UPLOAD_BYTES (default 200MB); adjust if you change MAX_UPLOAD_BYTES.
    client_max_body_size 250m;

    location ^~ /.well-known/acme-challenge/ {
        root ${CHALLENGE_ROOT};
        default_type text/plain;
        try_files \$uri =404;
    }

    location / {
        proxy_set_header Host \$host;
        proxy_set_header X-Real-IP \$remote_addr;
        proxy_set_header X-Forwarded-For ${PROXY_XFF_NGX};
        proxy_set_header X-Forwarded-Proto \$scheme;
        proxy_request_buffering off;
        proxy_pass http://127.0.0.1:${PORT};
    }
}
EOF

if [[ -n "${REDIRECT_DOMAINS}" ]]; then
  cat >>"${NGINX_SITE}" <<EOF

server {
    listen 443 ssl http2;
    listen [::]:443 ssl http2;
    server_name ${REDIRECT_DOMAINS};
    ${CF_REALIP_INCLUDE}
    ${CF_ALLOW_INCLUDE}

    # Temporary self-signed cert so nginx can start; Certbot will replace these
    ssl_certificate /etc/ssl/certs/ssl-cert-snakeoil.pem;
    ssl_certificate_key /etc/ssl/private/ssl-cert-snakeoil.key;

    location ^~ /.well-known/acme-challenge/ {
        root ${CHALLENGE_ROOT};
        default_type text/plain;
        try_files \$uri =404;
    }

    location / { return 301 https://${DOMAIN}\$request_uri; }
}
EOF
fi

rm -f /etc/nginx/sites-enabled/default
ln -sf /etc/nginx/sites-available/${SERVICE_NAME} /etc/nginx/sites-enabled/${SERVICE_NAME}
nginx -t && systemctl reload nginx

# Quick ACME reachability check (host-header to ensure correct server block)
TEST_TOKEN="installer-ok"
TEST_FILE="${CHALLENGE_ROOT}/.well-known/acme-challenge/install-test"
echo "${TEST_TOKEN}" >"${TEST_FILE}"
curl -fsS -H "Host: ${DOMAIN}" "http://127.0.0.1/.well-known/acme-challenge/$(basename "${TEST_FILE}")" \
  | grep -q "${TEST_TOKEN}"

if [[ -n "${DOMAIN}" && "${DOMAIN}" != "example.com" ]]; then
  CERTBOT_DOMAINS=(-d "${DOMAIN}")
  for d in ${REDIRECT_DOMAINS}; do
    CERTBOT_DOMAINS+=(-d "${d}")
  done
  certbot --nginx "${CERTBOT_DOMAINS[@]}" --non-interactive --agree-tos -m "${CERTBOT_EMAIL}" || true
fi

echo "Reloading systemd and enabling service..."
systemctl daemon-reload
echo "Service installed but not enabled/started. You can enable after verifying env."
# Do not start automatically; set env values first, then run (if desired):
#   systemctl enable ${SERVICE_NAME}
#   systemctl start ${SERVICE_NAME}

echo "Done.
Next steps:
1) Edit /etc/${SERVICE_NAME}.env with real values (DATABASE_URL if changed, COOKIE_SECRET, SMTP_* etc.).
2) If certbot failed (see output), fix DNS and rerun: certbot --nginx -d ${DOMAIN} -m ${CERTBOT_EMAIL} --agree-tos --non-interactive
3) (Optional) Enable service after validating env: systemctl enable ${SERVICE_NAME}
4) Start the app: systemctl start ${SERVICE_NAME}
4) Login with ${SEED_WRITER_EMAIL} / ${SEED_WRITER_PASSWORD} (must reset on first login).
Cache dir currently at ${CACHE_DIR_DEFAULT} (map to PCIe disk if desired)."

# Example invocation (run as root):
# REPO=palodurosystems/pdes-cdn \
# RELEASE_TAG=v0.0.6 \
# DOMAIN=cdn.example.com \
# CERTBOT_EMAIL=admin@example.com \
# GH_DOWNLOAD_TOKEN=jflksdfjslkdfjdkf \
# bash scripts/install_deps.sh

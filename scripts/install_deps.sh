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
SERVICE_NAME="pdes-cdn"
DB_NAME="${DB_NAME:-pdes_cdn}"
DB_USER="${DB_USER:-pdes_cdn_user}"
DB_PASS="${DB_PASS:-ChangeMeDbPass123!}"
REPO="${REPO:-OWNER/REPO}"              # e.g., palodurosystems/pdes-cdn
RELEASE_TAG="${RELEASE_TAG:-}"          # e.g., v0.0.1
DOMAIN="${DOMAIN:-example.com}"         # set to your domain
CERTBOT_EMAIL="${CERTBOT_EMAIL:-support@palodurosystems.io}"
PORT="${PORT:-3000}"

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
  curl ca-certificates gnupg lsb-release \
  postgresql postgresql-contrib libpq-dev \
  pkg-config openssl \
  nginx certbot python3-certbot-nginx

echo "[2/8] Creating app user and directories..."
id -u "$APP_USER" >/dev/null 2>&1 || useradd --system --create-home --shell /usr/sbin/nologin "$APP_USER"
mkdir -p "$APP_DIR" "$CACHE_DIR_DEFAULT"
chown -R "$APP_USER":"$APP_USER" "$APP_DIR" "$CACHE_DIR_DEFAULT"

echo "[3/8] Downloading release binary..."
BIN_URL="https://github.com/${REPO}/releases/download/${RELEASE_TAG}/pdes-cdn"
curl -fL "$BIN_URL" -o "$APP_DIR/pdes-cdn"
chmod +x "$APP_DIR/pdes-cdn"
chown "$APP_USER":"$APP_USER" "$APP_DIR/pdes-cdn"

echo "[4/8] PostgreSQL setup (db/user)..."
sudo -u postgres psql -tc "SELECT 1 FROM pg_roles WHERE rolname='${DB_USER}'" | grep -q 1 || \
  sudo -u postgres psql -c "CREATE ROLE ${DB_USER} WITH LOGIN PASSWORD '${DB_PASS}';"
sudo -u postgres psql -tc "SELECT 1 FROM pg_database WHERE datname='${DB_NAME}'" | grep -q 1 || \
  sudo -u postgres psql -c "CREATE DATABASE ${DB_NAME} OWNER ${DB_USER};"
sudo -u postgres psql -d "$DB_NAME" -c "GRANT ALL ON SCHEMA public TO ${DB_USER};"

echo "[5/8] Running migrations..."
export DATABASE_URL="postgres://${DB_USER}:${DB_PASS}@localhost:5432/${DB_NAME}"
cargo install sqlx-cli --no-default-features --features postgres >/dev/null 2>&1 || true
sqlx migrate run

echo "[6/8] Seed initial writer user (zacharytomlinson@palodurosystems.io)..."
PLACEHOLDER_HASH='$argon2id$v=19$m=19456,t=2,p=1$NTbpT0ilREk2K9+0sGsh8Q$dKzPDjDMBzlrx5GpPv1N4pXLGNpYiR94PR1yG4hIlMY' # password: CdnPass1234
sudo -u postgres psql -d "$DB_NAME" <<SQL
insert into users (id, email, password_hash, permission, must_reset)
values ('11111111-1111-1111-1111-111111111112', 'zacharytomlinson@palodurosystems.io', '${PLACEHOLDER_HASH}', 'writer', true)
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
  cat >/etc/${SERVICE_NAME}.env <<EOF
DATABASE_URL=postgres://${DB_USER}:${DB_PASS}@localhost:5432/${DB_NAME}
CACHE_DIR=${CACHE_DIR_DEFAULT}
COOKIE_SECRET=please-change-me
SMTP_HOST=
SMTP_PORT=587
SMTP_USERNAME=
SMTP_PASSWORD=
SMTP_FROM=support@palodurosystems.io
MAX_UPLOAD_BYTES=200000000
EOF
  chown root:root /etc/${SERVICE_NAME}.env
  chmod 640 /etc/${SERVICE_NAME}.env
fi

echo "[8/8] nginx + certbot..."
cat >/etc/nginx/sites-available/${SERVICE_NAME} <<EOF
server {
    listen 80;
    server_name ${DOMAIN};

    location / {
        proxy_set_header Host \$host;
        proxy_set_header X-Real-IP \$remote_addr;
        proxy_set_header X-Forwarded-For \$proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto \$scheme;
        proxy_pass http://127.0.0.1:${PORT};
    }
}
EOF
ln -sf /etc/nginx/sites-available/${SERVICE_NAME} /etc/nginx/sites-enabled/${SERVICE_NAME}
nginx -t && systemctl reload nginx

if [[ -n "${DOMAIN}" && "${DOMAIN}" != "example.com" ]]; then
  certbot --nginx -d "${DOMAIN}" --non-interactive --agree-tos -m "${CERTBOT_EMAIL}" || true
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
2) Set DOMAIN and rerun certbot if not already valid: certbot --nginx -d <your-domain> -m ${CERTBOT_EMAIL} --agree-tos --non-interactive
3) (Optional) Enable service after validating env: systemctl enable ${SERVICE_NAME}
4) Start the app: systemctl start ${SERVICE_NAME}
4) Login with zacharytomlinson@palodurosystems.io / CdnPass1234 (must reset on first login).
Cache dir currently at ${CACHE_DIR_DEFAULT} (map to PCIe disk if desired)."

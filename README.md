# PDES Appstore Node

`pdes-cdn` is a small self-hosted appstore/CDN node with a web portal and API for distributing desktop app packages and firmware.

## Quick deploy (Debian node)

The repo includes a one-shot installer that provisions PostgreSQL + nginx + certbot, installs the systemd service, runs migrations, and downloads the release binary.

Prereqs:
- A Debian server with a public DNS name (for certbot) and ports `80`/`443` reachable.
- (Optional) A mounted storage disk for `CACHE_DIR` (example: `/mnt/main_ssd`).
- (Optional, recommended with Cloudflare) Proxy the DNS record (orange cloud) and lock down the origin to Cloudflare.

On the server, in a checkout of this repo:

```bash
sudo -i
REPO=YOUR_ORG/pdes-cdn \
RELEASE_TAG=vX.Y.Z \
DOMAIN=apps.example.com \
# Optional: hostnames that should redirect to DOMAIN (space-separated)
REDIRECT_DOMAINS="cdn.example.com" \
# Optional: only allow inbound HTTP(S) from Cloudflare IPs (requires Proxied DNS)
CLOUDFLARE_ONLY=1 \
CERTBOT_EMAIL=admin@example.com \
GH_DOWNLOAD_TOKEN=... \
bash scripts/install_deps.sh
```

The installer will generate:
- A unique Postgres password (unless you provide `DB_PASS`)
- A unique `COOKIE_SECRET` (unless you provide `COOKIE_SECRET`)
- A unique initial writer password (unless you provide `SEED_WRITER_PASSWORD`)

Then edit the env file and start the service:

```bash
sudoedit /etc/pdes-cdn.env
sudo systemctl daemon-reload
sudo systemctl enable --now pdes-cdn
sudo systemctl status pdes-cdn
```

Note: the app listens on `127.0.0.1:3000` behind nginx (do not expose `:3000` publicly).

Logs:

```bash
sudo journalctl -u pdes-cdn -n 200 -f
```

## Environment variables

The systemd unit uses `EnvironmentFile=/etc/pdes-cdn.env`. Key settings:

- `DATABASE_URL` (required): Postgres connection string.
- `COOKIE_SECRET` (required): secret used to sign session cookies. Use a long random value.
- `BASE_URL` (recommended): public URL of the site (used for password reset links), e.g. `https://cdn.example.com`.
- `CACHE_DIR` (required): root directory for stored uploads on disk (the DB stores relative paths under this root).
- `MAX_UPLOAD_BYTES` (optional): max upload size; default `200000000` (200 MB).
- `CACHE_DIR_SKIP_WRITE_CHECK` (optional): set to `true` to skip the startup write probe (uploads will still fail if not writable).

SMTP (password reset + invites):
- `SMTP_HOST` (required to send email)
- `SMTP_PORT` (common: `587` for STARTTLS, `465` for SMTPS)
- `SMTP_TLS` (`starttls` | `wrapper` | `none`)
- `SMTP_USERNAME` / `SMTP_PASSWORD` (optional; only used when both are set)
- `SMTP_FROM` (email address used in the From header)

Logging:
- `RUST_LOG` (optional): controls log level (example: `info,pdes_cdn=debug`).

UI/branding (optional):
- `APP_NAME` (optional): display name used in page titles/emails/footer (default: `PDES CDN Node`).
- `APP_BUILD` (optional): build label shown in the footer (recommended: a release tag like `v0.1.2`).
- `APP_COMMIT` (optional): commit shown in the footer (shortened automatically).

Installer-only variables (optional):
- `DB_NAME`, `DB_USER`, `DB_PASS` (if omitted, `DB_PASS` is randomized)
- `SEED_WRITER_EMAIL`, `SEED_WRITER_PASSWORD` (if omitted, password is randomized)

## Storage on a mounted disk

If you want uploads on an external SSD mounted at `/mnt/main_ssd`:

1. Add it to `/etc/fstab` (example):
   - `UUID=... /mnt/main_ssd ext4 defaults 0 2`
2. Create an app directory and grant write access to the service user:
   ```bash
   sudo mkdir -p /mnt/main_ssd/pdes-cdn/cache
   sudo chown -R pdescdn:pdescdn /mnt/main_ssd/pdes-cdn
   ```
3. Set `CACHE_DIR=/mnt/main_ssd/pdes-cdn/cache` in `/etc/pdes-cdn.env` and restart:
   ```bash
   sudo systemctl restart pdes-cdn
   ```

The installer’s systemd unit includes `RequiresMountsFor=/mnt/main_ssd`, so if `/mnt/main_ssd` is a real mount, systemd will wait for it before starting the service.

## First login / security notes

The installer seeds an initial writer user so you can log in and finish setup. Treat a fresh node as untrusted until you:
- Set a strong `COOKIE_SECRET`
- Set `BASE_URL` correctly
- Log in and reset the seeded password immediately (and/or rotate the user/email to your own)
- Lock down network access as appropriate (nginx exposes `443`; the app itself listens on `3000` and should not be exposed directly)
- If you use Cloudflare, ensure the DNS record is **Proxied** and consider `CLOUDFLARE_ONLY=1` to prevent origin-IP bypass traffic.

## Using the portal

- Visit `https://<your-domain>/login`
- Writers can manage apps at `GET /apps/manage` and create tokens at `GET /tokens`.
- Readers can view assigned apps at `GET /apps`.

## API uploads (CI/pipelines)

For CI uploading app packages into the app store, create an **app upload token** at `GET /tokens` (writer UI) and use it with `POST /api/apps/<slug>/package/upload` (see below).

## Desktop app update API (packages + firmware)

This node can serve desktop app installers and firmware artifacts, with a bearer-token API for checking the latest versions and downloading them.

1. Create an API token in the portal at `GET /tokens` (token is shown once).
2. Use it on API requests: `Authorization: Bearer <token>`.

### Bootstrap installer flow (CI → device token)

If you want a CI job (or provisioning flow) to download a platform-specific installer `.pkg` and mint a **device token** (scoped to one app + OS/arch), use the bootstrap endpoints.

**How it works**
- `POST /api/bootstrap/mint` returns a short-lived `bootstrap_token` + a direct `pkg_url`.
- `GET /bootstrap/<bootstrap_token>/pkg` downloads the `.pkg` (each call consumes one use).
- `POST /bootstrap/<bootstrap_token>/exchange[?name=…]` mints a long-lived `device_token` for that app + OS/arch (also consumes one use).

**1) Mint a bootstrap token (writer API token required)**

Create a writer API token in `GET /tokens` and use it here:

```bash
BASE_URL="https://apps.example.com"
WRITER_API_TOKEN="..."
APP_SLUG="my-app"
TARGET="linux:x86_64" # os:arch

curl -sS \
  -H "Authorization: Bearer ${WRITER_API_TOKEN}" \
  -H "Content-Type: application/json" \
  -d "{\"app_slug\":\"${APP_SLUG}\",\"target\":\"${TARGET}\",\"expires_in_seconds\":3600,\"max_uses\":5}" \
  "${BASE_URL}/api/bootstrap/mint"
```

Response JSON includes:
- `bootstrap_token`
- `pkg_url` (absolute URL for downloading the `.pkg`)

Example (extract fields with `jq`):

```bash
resp="$(
  curl -sS \
    -H "Authorization: Bearer ${WRITER_API_TOKEN}" \
    -H "Content-Type: application/json" \
    -d "{\"app_slug\":\"${APP_SLUG}\",\"target\":\"${TARGET}\"}" \
    "${BASE_URL}/api/bootstrap/mint"
)"
BOOTSTRAP_TOKEN="$(jq -r '.bootstrap_token' <<<"$resp")"
PKG_URL="$(jq -r '.pkg_url' <<<"$resp")"
```

**2) Download the `.pkg`**

```bash
curl -fL "${PKG_URL}" -o "${APP_SLUG}.pkg"
```

**3) Exchange bootstrap token for a device token**

```bash
curl -sS -X POST \
  "${BASE_URL}/bootstrap/${BOOTSTRAP_TOKEN}/exchange?name=$(hostname)" \
  | jq .
```

Response JSON includes:
- `device_token`
- convenience URLs like `package_latest_url` and `firmware_latest_url`

**4) Use the device token for update checks**

```bash
curl -sS \
  -H "Authorization: Bearer ${DEVICE_TOKEN}" \
  "${BASE_URL}/api/apps/${APP_SLUG}/package/latest"
```

Notes:
- A `device_token` is valid only for the OS/arch it was minted for; if you pass `os/arch` query params they must match.
- A `bootstrap_token` is limited by `expires_at` and `max_uses`. Both `/pkg` and `/exchange` increment use count, so set `max_uses` accordingly.

### Package (installer) updates

- Building a `.pkg` in CI: use `scripts/build_pkg.sh` to bundle your installer + icon into the `.pkg` format this server expects.
- Upload/register a built `.pkg` (app upload token or writer API token): `POST /api/apps/<slug>/package/upload` with multipart fields `os`, `arch`, `version`, `file=@...pkg` and `Authorization: Bearer <token>`.
- Latest metadata: `GET /api/apps/<slug>/package/latest?os=windows&arch=x86_64`
  - Response JSON example:
    ```json
    {
      "app_slug": "my-app",
      "os": "windows",
      "arch": "x86_64",
      "version": "1.2.3",
      "pkg_sha256": "…",
      "download_url": "/api/apps/my-app/package/download?os=windows&arch=x86_64"
    }
    ```
  - Latest version field: `version`
  - Download URL field: `download_url`
- Download file (stream): `GET /api/apps/<slug>/package/download?os=windows&arch=x86_64[&version=1.2.3]`

### Firmware updates

- Latest metadata: `GET /api/apps/<slug>/firmware/latest?channel=stable`
  - Response JSON example:
    ```json
    {
      "app_slug": "my-app",
      "channel": "stable",
      "version": "2026.01.21",
      "sha256": "…",
      "download_url": "/api/apps/my-app/firmware/download?channel=stable"
    }
    ```
  - Latest version field: `version`
  - Download URL field: `download_url`
- Download file (stream): `GET /api/apps/<slug>/firmware/download?channel=stable[&version=2026.01.21]`

## Troubleshooting

- Env var changes not taking effect: `sudo systemctl restart pdes-cdn`
- `502 Bad Gateway` from nginx: `sudo systemctl status pdes-cdn` and `sudo journalctl -u pdes-cdn -n 200`
- `413 Request Entity Too Large` from nginx: set `client_max_body_size` in the nginx site config (and ensure `MAX_UPLOAD_BYTES` matches), then `sudo nginx -t && sudo systemctl reload nginx`
- Password reset links go to `localhost`: set `BASE_URL=https://your-domain`
- Upload errors like “Could not create directory …”: ensure `CACHE_DIR` exists and is writable by the service user
- SMTP errors like `wrong version number`: use `SMTP_TLS=starttls` with port `587`, or `SMTP_TLS=wrapper` with port `465`

## Favicons / app icons

Icon assets live in `assets/` and are embedded into the binary.

To regenerate from `assets/icon.png`:

```bash
python3 scripts/generate_favicons.py
```

Rebuild/redeploy the binary after updating icons.

## CI-based deployment

If you use a self-hosted GitHub Actions runner to deploy releases, see `docs/deploy.md`.

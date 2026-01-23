# Deployment (recommended)

Use a self-hosted GitHub Actions runner inside the VPN (ideally on the Debian node itself) so tag releases deploy automatically without SSH-from-GitHub.

## One-time setup

- Install the app/service once (user, `/opt/pdes-cdn`, systemd unit, `/etc/pdes-cdn.env`).
- Install a self-hosted runner on the node (GitHub repo → Settings → Actions → Runners).
- Label the runner `pdes-cdn-prod` (the deploy job targets `runs-on: [self-hosted, pdes-cdn-prod]`).
- Ensure the runner user has passwordless sudo for deployment operations. In sudoers, commands must use absolute paths (start with `/`). The runner work folder (default: `_work`) is created on the first job run.
- (Optional) Set GitHub repo variables:
  - `APP_DIR` (default: `/opt/pdes-cdn`)
  - `SERVICE_NAME` (default: `pdes-cdn`)

## How it works

- Pushing a tag like `v0.1.2` triggers `.github/workflows/release.yml`.
- After the GitHub Release is created, the `Deploy (self-hosted)` job runs on the self-hosted runner and calls `scripts/deploy_node.sh` to:
  - install the correct-arch binary,
  - update `templates/`,
  - run SQL migrations from `migrations/`,
  - restart the systemd service.

## Manual deploy / rollback (no new release)

- GitHub → Actions → Release → Run workflow
- Set `release_tag` to an existing tag (or `latest`).
- This runs the `Deploy Tag (manual)` job which downloads the release asset + templates for that tag and deploys them via `scripts/deploy_node.sh`.

## Troubleshooting sudo

- If you see `sudo: a password is required`, your sudoers entry likely doesn’t match the exact command being executed.
- The workflow runs `/usr/bin/bash` with the script at `$GITHUB_WORKSPACE/scripts/deploy_node.sh` (typically `/actions-runner/_work/pdes-cdn/pdes-cdn/scripts/deploy_node.sh`).
- On the node, as the runner user, check what sudo thinks is allowed with `sudo -n -l` (it should list the deploy command without prompting).

## Manual fallback

- The manual updater `scripts/update_app.sh` remains available for cases where CI can’t reach the node.

## Email (SMTP) configuration

The app sends password-reset emails via SMTP using `/etc/pdes-cdn.env`:

- `SMTP_HOST` (required)
- `SMTP_PORT` (common: `587` for STARTTLS, `465` for SMTPS)
- `SMTP_TLS` (`starttls` | `wrapper` | `none`; default is `starttls` unless `SMTP_PORT=465`)
- `SMTP_USERNAME` / `SMTP_PASSWORD` (optional; only used when both are set)
- `SMTP_FROM` (required; defaults to `support@palodurosystems.io` in the installer template)

## Storage on a mounted disk

Uploads are stored under `CACHE_DIR` (the DB stores relative paths, so you can move the whole directory tree by changing `CACHE_DIR`).

Recommended setup for an external disk mounted at `/mnt/main_ssd`:

- Create a dedicated directory and grant the service user write access:
  - `sudo mkdir -p /mnt/main_ssd/pdes-cdn/cache`
  - `sudo chown -R pdescdn:pdescdn /mnt/main_ssd/pdes-cdn`
- Set `/etc/pdes-cdn.env`:
  - `CACHE_DIR=/mnt/main_ssd/pdes-cdn/cache`
- Ensure systemd waits for the mount before starting the service (pick one):
  - Add `RequiresMountsFor=/mnt/main_ssd` to the unit, or
  - Add appropriate `After=`/`Requires=` entries for the mount unit.

If you need the service to start even when the cache dir isn’t writable (read-only mode), set `CACHE_DIR_SKIP_WRITE_CHECK=true` (uploads will still fail).

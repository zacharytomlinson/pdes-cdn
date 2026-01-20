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

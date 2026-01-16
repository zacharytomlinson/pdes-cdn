#!/usr/bin/env bash
# Install runtime/build deps for the Axum + SQLx project, including PostgreSQL and SQLx CLI.
# Supports apt, pacman, dnf, and Homebrew.

set -euo pipefail

detect_pm() {
    if command -v apt-get >/dev/null 2>&1; then
        echo "apt"
    elif command -v pacman >/dev/null 2>&1; then
        echo "pacman"
    elif command -v dnf >/dev/null 2>&1; then
        echo "dnf"
    elif command -v brew >/dev/null 2>&1; then
        echo "brew"
    else
        echo "unsupported"
    fi
}

install_pkgs() {
    case "$1" in
        apt)
            sudo apt-get update
            sudo apt-get install -y postgresql postgresql-contrib libpq-dev pkg-config openssl
            ;;
        pacman)
            sudo pacman -Syu --noconfirm --needed postgresql openssl pkgconf
            ;;
        dnf)
            sudo dnf install -y postgresql-server postgresql-contrib postgresql-devel openssl-devel pkgconf-pkg-config
            ;;
        brew)
            brew update
            brew install postgresql@16 openssl pkg-config
            ;;
        *)
            echo "Unsupported package manager. Please install PostgreSQL, libpq/openssl dev headers, and pkg-config manually." >&2
            exit 1
            ;;
    esac
}

install_sqlx_cli() {
    if command -v sqlx >/dev/null 2>&1; then
        echo "sqlx-cli already installed."
    else
        cargo install sqlx-cli --no-default-features --features postgres
    fi
}

PM=$(detect_pm)
echo "Detected package manager: $PM"
install_pkgs "$PM"

echo "Installing sqlx-cli via cargo..."
install_sqlx_cli

cat <<'EOF'
Post-install notes:
- Initialize and start PostgreSQL if not already running.
  * apt (Debian/Ubuntu): sudo service postgresql start
  * pacman (Arch): sudo -u postgres initdb --locale en_US.UTF-8 -D /var/lib/postgres/data && sudo systemctl start postgresql
  * dnf (Fedora): sudo postgresql-setup --initdb && sudo systemctl start postgresql
  * brew (macOS): brew services start postgresql@16
- Create the database and run migrations:
    createdb pdes_cdn || true
    sqlx migrate run
EOF

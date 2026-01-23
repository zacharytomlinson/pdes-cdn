#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Build a pdes-cdn .pkg (a tar.gz with manifest.json + installer + icon).

Usage:
  scripts/build_pkg.sh \
    --slug <app-slug> \
    --version <app-version> \
    --os <windows|linux|macos> \
    [--arch <x86_64|aarch64|any>] \
    --installer <path-to-installer> \
    --icon <path-to-icon.(png|svg)> \
    [--display-name <name>] \
    [--summary <text>] \
    [--doc <path>]... \
    [--out <output.pkg>]

Notes:
  - The output ".pkg" is not a macOS pkg; it's a gzip-compressed tar archive.
  - Requires: bash, tar, gzip, python3
EOF
}

slug=""
version=""
os=""
arch="x86_64"
installer=""
icon=""
display_name=""
summary=""
out=""
docs=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --slug) slug="${2:-}"; shift 2 ;;
    --version) version="${2:-}"; shift 2 ;;
    --os) os="${2:-}"; shift 2 ;;
    --arch) arch="${2:-}"; shift 2 ;;
    --installer) installer="${2:-}"; shift 2 ;;
    --icon) icon="${2:-}"; shift 2 ;;
    --display-name) display_name="${2:-}"; shift 2 ;;
    --summary) summary="${2:-}"; shift 2 ;;
    --doc) docs+=("${2:-}"); shift 2 ;;
    --out) out="${2:-}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown arg: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ -n "$slug" ]] || { echo "--slug is required" >&2; exit 2; }
[[ -n "$version" ]] || { echo "--version is required" >&2; exit 2; }
[[ -n "$os" ]] || { echo "--os is required" >&2; exit 2; }
[[ -n "$arch" ]] || { echo "--arch is required" >&2; exit 2; }
[[ -n "$installer" ]] || { echo "--installer is required" >&2; exit 2; }
[[ -n "$icon" ]] || { echo "--icon is required" >&2; exit 2; }

[[ -f "$installer" ]] || { echo "Installer not found: $installer" >&2; exit 2; }
[[ -f "$icon" ]] || { echo "Icon not found: $icon" >&2; exit 2; }
for d in "${docs[@]}"; do
  [[ -f "$d" ]] || { echo "Doc not found: $d" >&2; exit 2; }
done

icon_base="$(basename "$icon")"
icon_ext="${icon_base##*.}"
icon_ext="${icon_ext,,}"
if [[ "$icon_ext" != "png" && "$icon_ext" != "svg" ]]; then
  echo "Icon must be .png or .svg (got: $icon_base)" >&2
  exit 2
fi

installer_base="$(basename "$installer")"
installer_ext="${installer_base##*.}"
installer_ext="${installer_ext,,}"
case "$installer_ext" in
  msi|deb|rpm|exe|dmg|pkg|zip|tar|gz) ;;
  *)
    echo "Unsupported installer extension: .$installer_ext (file: $installer_base)" >&2
    exit 2
    ;;
esac

if [[ -z "$display_name" ]]; then
  display_name="$slug"
fi
if [[ -z "$summary" ]]; then
  summary="Companion app installer"
fi
if [[ -z "$out" ]]; then
  out="${slug}-${version}-${os}-${arch}.pkg"
fi

tmpdir="$(mktemp -d)"
cleanup() { rm -rf "$tmpdir"; }
trap cleanup EXIT

mkdir -p "$tmpdir/installers" "$tmpdir/docs"
cp -f "$installer" "$tmpdir/installers/$installer_base"
cp -f "$icon" "$tmpdir/$icon_base"

if [[ "${#docs[@]}" -gt 0 ]]; then
  for d in "${docs[@]}"; do
    cp -f "$d" "$tmpdir/docs/$(basename "$d")"
  done
fi

python3 - "$tmpdir/manifest.json" \
  "$slug" "$version" "$os" "$arch" \
  "$display_name" "$summary" \
  "$icon_base" "$installer_ext" "installers/$installer_base" <<'PY'
import json
import sys

out_path = sys.argv[1]
slug = sys.argv[2]
version = sys.argv[3]
os_ = sys.argv[4]
arch = sys.argv[5]
display_name = sys.argv[6]
summary = sys.argv[7]
icon_path = sys.argv[8]
installer_type = sys.argv[9]
installer_path = sys.argv[10]

payload = {
  "schema_version": 1,
  "app_slug": slug,
  "app_version": version,
  "platform": {"os": os_, "arch": arch},
  "display_name": display_name,
  "summary": summary,
  "icon": {"path": icon_path},
  "installer": {"type": installer_type, "path": installer_path},
  "firmware": {
    "channel": "stable",
    "latest_url": f"/api/apps/{slug}/firmware/latest?channel=stable",
  },
}

with open(out_path, "w", encoding="utf-8") as f:
  json.dump(payload, f, indent=2, sort_keys=True)
  f.write("\n")
PY

if tar --version 2>/dev/null | grep -q "GNU tar"; then
  tar \
    --sort=name \
    --mtime='UTC 1970-01-01' \
    --owner=0 \
    --group=0 \
    --numeric-owner \
    -cf - \
    -C "$tmpdir" . | gzip -n >"$out"
else
  tar -czf "$out" -C "$tmpdir" .
fi

echo "Wrote $out"

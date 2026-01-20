#!/usr/bin/env python3
from __future__ import annotations

from pathlib import Path


def main() -> None:
    try:
        from PIL import Image
    except Exception as exc:  # pragma: no cover
        raise SystemExit(
            "Pillow is required. Install it with your system package manager or `pip install Pillow`."
        ) from exc

    repo_root = Path(__file__).resolve().parents[1]
    source = repo_root / "assets" / "icon.png"
    out_dir = repo_root / "assets"
    out_dir.mkdir(parents=True, exist_ok=True)

    if not source.exists():
        raise SystemExit(f"Missing source icon: {source}")

    image = Image.open(source).convert("RGBA")

    png_targets: dict[str, int] = {
        "favicon-16x16.png": 16,
        "favicon-32x32.png": 32,
        "apple-touch-icon.png": 180,
        "android-chrome-192x192.png": 192,
        "android-chrome-512x512.png": 512,
    }

    for filename, size in png_targets.items():
        out_path = out_dir / filename
        resized = image.resize((size, size), Image.Resampling.LANCZOS)
        resized.save(out_path, format="PNG", optimize=True)

    ico_path = out_dir / "favicon.ico"
    ico_sizes = [(16, 16), (32, 32), (48, 48)]
    frames = [image.resize(sz, Image.Resampling.LANCZOS) for sz in ico_sizes]
    frames[0].save(ico_path, format="ICO", sizes=ico_sizes)

    print("Generated:")
    for filename in [*png_targets.keys(), "favicon.ico"]:
        print(f"- {out_dir / filename}")


if __name__ == "__main__":
    main()


#!/usr/bin/env python3
"""Generate app icons from assets/appicon.png (the Pipecat logo).

Outputs:
  assets/appicon-rounded-512.png  embedded runtime window/Dock icon (src/main.rs)
  macos/AppIcon.icns              bundle icon (scripts/bundle_macos.sh)

Requires Pillow; the .icns step uses macOS iconutil.

    python3 scripts/make_icons.py
"""

import pathlib
import shutil
import subprocess
import tempfile

from PIL import Image, ImageDraw

ROOT = pathlib.Path(__file__).resolve().parent.parent
SOURCE = ROOT / "assets" / "appicon.png"

# Apple's icon grid: a 1024pt canvas with an ~824pt rounded square.
CANVAS = 1024
RECT = 824
RADIUS = 186
SUPERSAMPLE = 4


def compose() -> Image.Image:
    """White rounded-rect with the logo centered, transparent margins."""
    size = CANVAS * SUPERSAMPLE
    rect = RECT * SUPERSAMPLE
    offset = (size - rect) // 2

    mask = Image.new("L", (size, size), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        (offset, offset, offset + rect, offset + rect),
        radius=RADIUS * SUPERSAMPLE,
        fill=255,
    )

    logo = Image.open(SOURCE).convert("RGBA").resize((rect, rect), Image.LANCZOS)
    icon = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    icon.paste(logo, (offset, offset))
    icon.putalpha(mask)
    return icon.resize((CANVAS, CANVAS), Image.LANCZOS)


def main() -> None:
    icon = compose()
    icon.resize((512, 512), Image.LANCZOS).save(ROOT / "assets" / "appicon-rounded-512.png")

    with tempfile.TemporaryDirectory() as tmp:
        iconset = pathlib.Path(tmp) / "AppIcon.iconset"
        iconset.mkdir()
        for points in (16, 32, 128, 256, 512):
            for scale in (1, 2):
                px = points * scale
                suffix = "@2x" if scale == 2 else ""
                icon.resize((px, px), Image.LANCZOS).save(
                    iconset / f"icon_{points}x{points}{suffix}.png"
                )
        if shutil.which("iconutil") is None:
            print("iconutil not found (not macOS?); skipped macos/AppIcon.icns")
            return
        subprocess.run(
            ["iconutil", "-c", "icns", str(iconset), "-o", str(ROOT / "macos" / "AppIcon.icns")],
            check=True,
        )

    print("wrote assets/appicon-rounded-512.png and macos/AppIcon.icns")


if __name__ == "__main__":
    main()

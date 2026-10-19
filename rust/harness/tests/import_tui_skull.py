#!/usr/bin/env python3
"""Import the embedded artwork from the supplied HTML without executing JavaScript.

Development only: python3 import_tui_skull.py /path/to/index.html
Requires Pillow. The Rust product embeds the generated RGB bytes directly.
"""
import base64
import hashlib
import io
from pathlib import Path
import re
import sys

from PIL import Image

html = Path(sys.argv[1]).read_text(encoding="utf-8")
match = re.search(r"const SOURCE = 'data:image/png;base64,([^']+)';", html)
if not match:
    raise SystemExit("The HTML does not contain the expected embedded skull artwork")
source = base64.b64decode(match.group(1), validate=True)
with Image.open(io.BytesIO(source)) as image:
    if image.size != (384, 384):
        raise SystemExit("Expected the original 384 by 384 skull artwork")
    pixels = image.convert("RGB").resize((96, 96), Image.Resampling.LANCZOS).tobytes()
assets = Path(__file__).resolve().parents[3] / "assets"
(assets / "tui-skull.png").write_bytes(source)
(assets / "tui-skull.rgb").write_bytes(pixels)
(assets / "tui-skull.txt").write_text(
    "Source: user-provided index (3).html, Blue Flame Skull.\n"
    "The embedded 384 by 384 PNG is preserved as tui-skull.png.\n"
    "tui-skull.rgb: 96 by 96 RGB, row-major, three bytes per pixel, Lanczos sampling.\n"
    "The native mascot renderer ports the HTML's blue-flame mask and flowing coordinate field.\n"
    "Only the hair animates; the face remains unchanged. No JavaScript runs in the product.\n"
    f"Original PNG SHA-256: {hashlib.sha256(source).hexdigest()}\n"
    f"Sampled RGB SHA-256: {hashlib.sha256(pixels).hexdigest()}\n",
    encoding="utf-8",
)
print(f"Imported {len(pixels)} RGB bytes from the supplied skull animation")

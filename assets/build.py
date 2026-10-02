"""Builds the PNGs and the ICO from logo.svg (Linux and Windows).

    uv run --with playwright --with pillow python assets/build.py

Rasterises with Playwright's Chromium (`python -m playwright install
chromium-headless-shell` the first time) and packs the ICO with Pillow.
"""
from pathlib import Path

from PIL import Image
from playwright.sync_api import sync_playwright

HERE = Path(__file__).parent
SIZES = [16, 32, 48, 64, 128, 256, 512]

svg = (HERE / "logo.svg").read_text(encoding="utf-8")
with sync_playwright() as p:
    browser = p.chromium.launch()
    for n in SIZES:
        page = browser.new_page(viewport={"width": n, "height": n})
        page.set_content(f"<style>*{{margin:0}}svg{{display:block;width:{n}px;height:{n}px}}</style>{svg}")
        page.screenshot(path=HERE / f"logo-{n}.png", omit_background=True)
        page.close()
    browser.close()

# Each ICO size comes from its own PNG (not downscaled from the big one): sharper at 16 px.
imgs = [Image.open(HERE / f"logo-{n}.png") for n in SIZES if n <= 256]
imgs[-1].save(HERE / "logo.ico", sizes=[i.size for i in imgs], append_images=imgs[:-1])
print("ok:", ", ".join(f"logo-{n}.png" for n in SIZES), "+ logo.ico")

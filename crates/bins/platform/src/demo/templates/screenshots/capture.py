"""Capture UI preview screenshots from pre-rendered static HTML.

Works in both pipeline pods and agent dev sessions.
Requires: screenshots/render.py to have run first.
"""

import asyncio
import json
import os
import subprocess
import sys
from pathlib import Path

COMP_DIR = Path(os.getenv("OUTPUT_DIR", "/workspace/output")) / "components"
FLOW_DIR = Path(os.getenv("OUTPUT_DIR", "/workspace/output")) / "flows"
RENDERED = Path(os.getenv("RENDER_DIR", "/tmp/rendered"))
PORT = int(os.getenv("CAPTURE_PORT", "8099"))
URL = f"http://localhost:{PORT}"


async def main():
    from playwright.async_api import async_playwright

    COMP_DIR.mkdir(parents=True, exist_ok=True)
    FLOW_DIR.mkdir(parents=True, exist_ok=True)

    # Minimal static file server — starts instantly
    server = subprocess.Popen(
        [sys.executable, "-m", "http.server", str(PORT), "--directory", str(RENDERED)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )

    try:
        await asyncio.sleep(0.3)  # socket bind

        async with async_playwright() as p:
            browser = await p.chromium.launch()

            # --- Component screenshots ---
            page = await browser.new_page(viewport={"width": 1280, "height": 720})

            async def snap(html_file, out_path, full_page=True):
                await page.goto(f"{URL}/{html_file}")
                await page.wait_for_load_state("domcontentloaded")
                await page.wait_for_timeout(300)
                await page.screenshot(path=str(out_path), full_page=full_page)

            # Full pages
            await snap("index.html", COMP_DIR / "catalog.png")
            await snap("product-1.html", COMP_DIR / "product-detail.png")
            await snap("cart-empty.html", COMP_DIR / "cart-empty.png")
            await snap("orders.html", COMP_DIR / "orders-empty.png")
            await snap("cart-items.html", COMP_DIR / "cart-with-items.png")

            # Isolated component elements
            await page.goto(f"{URL}/index.html")
            await page.wait_for_load_state("domcontentloaded")
            await page.wait_for_timeout(300)

            nav = page.locator("nav")
            if await nav.count() > 0:
                await nav.screenshot(path=str(COMP_DIR / "nav-bar.png"))

            card = page.locator(".grid > div").first
            if await card.count() > 0:
                await card.screenshot(path=str(COMP_DIR / "product-card.png"))

            await page.close()

            # --- Flow screenshots ---
            page = await browser.new_page(viewport={"width": 1280, "height": 720})

            await snap("index.html", FLOW_DIR / "01-browse-catalog.png")
            await snap("product-1.html", FLOW_DIR / "02-view-product.png")
            await snap("cart-items.html", FLOW_DIR / "03-add-to-cart.png")
            await snap("cart-items.html", FLOW_DIR / "04-view-cart.png")
            await snap("orders.html", FLOW_DIR / "05-order-confirmed.png")

            await page.close()
            await browser.close()

        write_configs()

        comp_count = len(list(COMP_DIR.glob("*.png")))
        flow_count = len(list(FLOW_DIR.glob("*.png")))
        print(f"OK components={comp_count} flows={flow_count}")

    finally:
        server.terminate()
        server.wait()


def write_configs():
    """Write config.json files for the platform UI preview viewer."""
    comp_config = {
        "groups": {
            "pages": {
                "label": "Pages",
                "items": {
                    "catalog.png": {
                        "label": "Product Catalog",
                        "meta": {"route": "/", "type": "page"},
                    },
                    "product-detail.png": {
                        "label": "Product Detail",
                        "meta": {"route": "/product/1", "type": "page"},
                    },
                    "orders-empty.png": {
                        "label": "Order History",
                        "meta": {"route": "/orders", "type": "page"},
                    },
                },
            },
            "components": {
                "label": "Components",
                "items": {
                    "product-card.png": {
                        "label": "Product Card",
                        "meta": {"component": "card"},
                    },
                    "nav-bar.png": {
                        "label": "Navigation Bar",
                        "meta": {"component": "nav"},
                    },
                },
            },
            "states": {
                "label": "States",
                "items": {
                    "cart-empty.png": {
                        "label": "Empty Cart",
                        "meta": {"route": "/cart", "state": "empty"},
                    },
                    "cart-with-items.png": {
                        "label": "Cart with Items",
                        "meta": {"route": "/cart", "state": "filled"},
                    },
                },
            },
        }
    }

    flow_config = {
        "groups": {
            "purchase": {
                "label": "Purchase Flow",
                "items": {
                    "01-browse-catalog.png": {
                        "label": "1. Browse Catalog",
                        "meta": {"step": "1"},
                    },
                    "02-view-product.png": {
                        "label": "2. View Product",
                        "meta": {"step": "2"},
                    },
                    "03-add-to-cart.png": {
                        "label": "3. Add to Cart",
                        "meta": {"step": "3"},
                    },
                    "04-view-cart.png": {
                        "label": "4. View Cart",
                        "meta": {"step": "4"},
                    },
                    "05-order-confirmed.png": {
                        "label": "5. Order Confirmed",
                        "meta": {"step": "5"},
                    },
                },
            },
        }
    }

    (COMP_DIR / "config.json").write_text(json.dumps(comp_config, indent=2))
    (FLOW_DIR / "config.json").write_text(json.dumps(flow_config, indent=2))


if __name__ == "__main__":
    asyncio.run(main())

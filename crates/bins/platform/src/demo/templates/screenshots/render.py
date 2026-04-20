"""Pre-render Jinja2 templates to static HTML with seed data.

Works in both pipeline pods and agent dev sessions — no DB,
no server framework, no OTEL. Just templates + mock data.
"""

import os
import shutil
import sys
from pathlib import Path

# Allow importing app modules from workspace
sys.path.insert(0, os.getcwd())

from jinja2 import Environment, FileSystemLoader
from app.db import SEED_PRODUCTS

OUT = Path(os.getenv("RENDER_DIR", "/tmp/rendered"))
STATIC_SRC = Path("app/static")


class MockRequest:
    """Minimal request stub for Jinja2 templates that reference request.*"""

    class url:
        path = "/"

    def url_for(self, name, **kw):
        routes = {
            "catalog": "/index.html",
            "product_detail": f"/product-{kw.get('product_id', 1)}.html",
            "view_cart": "/cart-empty.html",
            "orders_page": "/orders.html",
        }
        return routes.get(name, "/index.html")


def main():
    products = [{**p, "id": i + 1, "stock": 100} for i, p in enumerate(SEED_PRODUCTS)]
    request = MockRequest()

    pages = {
        "index.html": ("catalog.html", {"products": products, "request": request}),
        "product-1.html": ("product.html", {"product": products[0], "request": request}),
        "cart-empty.html": ("cart.html", {"items": [], "total": 0, "request": request}),
        "cart-items.html": (
            "cart.html",
            {
                "items": [
                    {"product_id": 1, "name": "Starter Kit", "price_cents": 2900, "quantity": 2},
                    {"product_id": 2, "name": "Pro Bundle", "price_cents": 9900, "quantity": 1},
                ],
                "total": 15700,
                "request": request,
            },
        ),
        "orders.html": ("orders.html", {"orders": [], "request": request}),
    }

    OUT.mkdir(parents=True, exist_ok=True)

    # Copy static assets (CSS, images)
    static_dest = OUT / "static"
    if static_dest.exists():
        shutil.rmtree(static_dest)
    if STATIC_SRC.exists():
        shutil.copytree(STATIC_SRC, static_dest)

    # Render each template
    env = Environment(loader=FileSystemLoader("app/templates"), autoescape=True)
    for filename, (template_name, context) in pages.items():
        tmpl = env.get_template(template_name)
        html = tmpl.render(**context, session_id="preview-session")
        (OUT / filename).write_text(html)

    print(f"Rendered {len(pages)} pages to {OUT}")


if __name__ == "__main__":
    main()

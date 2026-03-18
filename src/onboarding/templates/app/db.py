"""Database layer — asyncpg schema, seed products, order queries."""

import json
import logging
from datetime import datetime, timezone

logger = logging.getLogger(__name__)

SCHEMA_SQL = """
CREATE TABLE IF NOT EXISTS products (
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    price_cents INTEGER NOT NULL,
    image_url TEXT NOT NULL DEFAULT '',
    category TEXT NOT NULL DEFAULT 'general',
    stock INTEGER NOT NULL DEFAULT 100,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS orders (
    id SERIAL PRIMARY KEY,
    session_id TEXT NOT NULL,
    items JSONB NOT NULL,
    total_cents INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'confirmed',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
"""

SEED_PRODUCTS = [
    {
        "name": "Starter Kit",
        "description": "Everything you need to launch your first project. "
        "Includes CI/CD pipeline, staging environment, and basic monitoring.",
        "price_cents": 2900,
        "category": "plans",
    },
    {
        "name": "Pro Bundle",
        "description": "Production-ready setup with auto-scaling, "
        "custom domains, and priority support.",
        "price_cents": 9900,
        "category": "plans",
    },
    {
        "name": "API Credits Pack",
        "description": "10,000 API calls for your integrations. "
        "Works with any platform endpoint.",
        "price_cents": 4900,
        "category": "add-ons",
    },
    {
        "name": "Custom Domain",
        "description": "Map your own domain to any deployed service. "
        "Includes automatic TLS certificates.",
        "price_cents": 1200,
        "category": "add-ons",
    },
    {
        "name": "Analytics Add-on",
        "description": "Real-time dashboards, custom metrics, and alerting "
        "for your production services.",
        "price_cents": 3900,
        "category": "add-ons",
    },
    {
        "name": "Enterprise Support",
        "description": "Dedicated support engineer, 99.99% SLA, "
        "and custom deployment configurations.",
        "price_cents": 29900,
        "category": "support",
    },
]


async def init_schema(pool) -> None:
    """Create tables and seed products if empty."""
    async with pool.acquire() as conn:
        await conn.execute(SCHEMA_SQL)
        count = await conn.fetchval("SELECT count(*) FROM products")
        if count == 0:
            for p in SEED_PRODUCTS:
                await conn.execute(
                    "INSERT INTO products (name, description, price_cents, category) "
                    "VALUES ($1, $2, $3, $4)",
                    p["name"],
                    p["description"],
                    p["price_cents"],
                    p["category"],
                )
            logger.info("seeded %d products", len(SEED_PRODUCTS))


async def get_products(pool) -> list[dict]:
    """Return all products."""
    rows = await pool.fetch(
        "SELECT id, name, description, price_cents, category, stock FROM products ORDER BY id"
    )
    return [dict(r) for r in rows]


async def get_product(pool, product_id: int) -> dict | None:
    """Return a single product or None."""
    row = await pool.fetchrow(
        "SELECT id, name, description, price_cents, category, stock FROM products WHERE id = $1",
        product_id,
    )
    return dict(row) if row else None


async def get_total_stock(pool) -> int:
    """Total stock across all products (for gauge metric)."""
    return await pool.fetchval("SELECT coalesce(sum(stock), 0) FROM products") or 0


async def create_order(pool, session_id: str, items: list[dict], total_cents: int) -> int:
    """Insert an order and decrement stock. Returns order ID."""
    async with pool.acquire() as conn:
        async with conn.transaction():
            order_id = await conn.fetchval(
                "INSERT INTO orders (session_id, items, total_cents) "
                "VALUES ($1, $2::jsonb, $3) RETURNING id",
                session_id,
                json.dumps(items),
                total_cents,
            )
            for item in items:
                await conn.execute(
                    "UPDATE products SET stock = greatest(stock - $2, 0) WHERE id = $1",
                    item["product_id"],
                    item["quantity"],
                )
            return order_id


async def get_orders(pool, session_id: str) -> list[dict]:
    """Return orders for a session, newest first."""
    rows = await pool.fetch(
        "SELECT id, items, total_cents, status, created_at "
        "FROM orders WHERE session_id = $1 ORDER BY created_at DESC",
        session_id,
    )
    result = []
    for r in rows:
        d = dict(r)
        if isinstance(d["items"], str):
            d["items"] = json.loads(d["items"])
        result.append(d)
    return result

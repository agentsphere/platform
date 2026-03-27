"""Shop demo E2E tests — dual-mode: ASGI transport (local) or HTTP (deployed)."""

import os

import httpx
import pytest

# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

@pytest.fixture
def anyio_backend():
    return "asyncio"


@pytest.fixture
async def client():
    app_host = os.getenv("APP_HOST")
    if app_host:
        port = os.getenv("APP_PORT", "8080")
        base = f"http://{app_host}:{port}"
        async with httpx.AsyncClient(base_url=base, timeout=15) as c:
            yield c
    else:
        from app.main import app
        async with httpx.AsyncClient(
            transport=httpx.ASGITransport(app=app), base_url="http://test"
        ) as c:
            yield c


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

@pytest.mark.anyio
async def test_healthz(client):
    resp = await client.get("/healthz")
    assert resp.status_code == 200
    assert resp.json()["status"] == "ok"


@pytest.mark.anyio
async def test_catalog_page(client):
    resp = await client.get("/")
    assert resp.status_code == 200
    assert "Starter Kit" in resp.text
    assert "Pro Bundle" in resp.text


@pytest.mark.anyio
async def test_product_detail(client):
    resp = await client.get("/product/1")
    assert resp.status_code == 200
    assert "Starter Kit" in resp.text
    assert "29.00" in resp.text


@pytest.mark.anyio
async def test_product_not_found(client):
    resp = await client.get("/product/999")
    assert resp.status_code == 404


@pytest.mark.anyio
async def test_add_to_cart(client):
    resp = await client.post("/cart/add", data={"product_id": "1"})
    assert resp.status_code == 200
    assert "cart-count" in resp.text


@pytest.mark.anyio
async def test_cart_page(client):
    await client.post("/cart/add", data={"product_id": "2"})
    resp = await client.get("/cart")
    assert resp.status_code == 200
    assert "Checkout" in resp.text or "empty" in resp.text.lower()


@pytest.mark.anyio
async def test_checkout_empty_cart(client):
    resp = await client.post("/checkout", follow_redirects=False)
    assert resp.status_code == 303


@pytest.mark.anyio
async def test_orders_page(client):
    resp = await client.get("/orders")
    assert resp.status_code == 200
    assert "Order" in resp.text or "No orders" in resp.text


@pytest.mark.anyio
async def test_api_products(client):
    resp = await client.get("/api/products")
    assert resp.status_code == 200
    data = resp.json()
    assert isinstance(data, list)
    assert len(data) >= 1
    assert "name" in data[0]
    assert "price_cents" in data[0]

"""Platform Demo — Shop app with OpenTelemetry observability."""

import logging
import os
import uuid
from contextlib import asynccontextmanager

import asyncpg
from fastapi import FastAPI, Form, Request
from fastapi.responses import HTMLResponse, RedirectResponse
from fastapi.staticfiles import StaticFiles
from fastapi.templating import Jinja2Templates
from opentelemetry import _logs as otel_logs
from opentelemetry import metrics, trace
from opentelemetry.exporter.otlp.proto.http._log_exporter import OTLPLogExporter
from opentelemetry.exporter.otlp.proto.http.metric_exporter import OTLPMetricExporter
from opentelemetry.exporter.otlp.proto.http.trace_exporter import OTLPSpanExporter
from opentelemetry.instrumentation.fastapi import FastAPIInstrumentor
from opentelemetry.metrics import Observation
from opentelemetry.sdk._logs import LoggerProvider, LoggingHandler
from opentelemetry.sdk._logs.export import BatchLogRecordProcessor
from opentelemetry.sdk.metrics import MeterProvider
from opentelemetry.sdk.metrics.export import PeriodicExportingMetricReader
from opentelemetry.sdk.resources import Resource
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import BatchSpanProcessor

from app import cart, db, flags

# ---------------------------------------------------------------------------
# OpenTelemetry — reads OTEL_* env vars injected by platform.
# Disabled via OTEL_SDK_DISABLED=true (e.g. testinfra deploys).
# ---------------------------------------------------------------------------
if os.getenv("OTEL_SDK_DISABLED", "").lower() != "true":
    resource = Resource.create()

    _tp = TracerProvider(resource=resource)
    _tp.add_span_processor(BatchSpanProcessor(OTLPSpanExporter()))
    trace.set_tracer_provider(_tp)

    _mr = PeriodicExportingMetricReader(OTLPMetricExporter(), export_interval_millis=15000)
    _mp = MeterProvider(resource=resource, metric_readers=[_mr])
    metrics.set_meter_provider(_mp)

    _lp = LoggerProvider(resource=resource)
    _lp.add_log_record_processor(BatchLogRecordProcessor(OTLPLogExporter()))
    otel_logs.set_logger_provider(_lp)
    logging.getLogger().addHandler(LoggingHandler(level=logging.INFO, logger_provider=_lp))

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)
tracer = trace.get_tracer(__name__)

# ---------------------------------------------------------------------------
# Custom metrics
# ---------------------------------------------------------------------------
_meter = metrics.get_meter(__name__)
product_views = _meter.create_counter("shop.product_views", description="Product detail page views")
cart_additions = _meter.create_counter("shop.cart_additions", description="Items added to cart")
orders_placed = _meter.create_counter("shop.orders_placed", description="Completed orders")
revenue_cents = _meter.create_counter("shop.revenue_cents", description="Revenue in cents")

# Observable gauge — registered after DB pool is available (see lifespan)
_db_pool_ref: asyncpg.Pool | None = None


def _stock_gauge_callback(_options):
    """Cannot do async in callback; return last cached value."""
    return [Observation(_stock_gauge_cache[0])]


_stock_gauge_cache = [0]
_meter.create_observable_gauge(
    "shop.products_in_stock",
    callbacks=[_stock_gauge_callback],
    description="Total products in stock",
)


# ---------------------------------------------------------------------------
# Lifespan — connect Postgres + Valkey, init schema
# ---------------------------------------------------------------------------
@asynccontextmanager
async def lifespan(application: FastAPI):
    global _db_pool_ref

    database_url = os.getenv("DATABASE_URL", "")
    cache_url = os.getenv("CACHE_URL", "")

    # Postgres — retry with back-off so the app survives slow DB starts.
    # Postgres init can take 5-15s on cold clusters; total retry window ~60s.
    pool = None
    if database_url:
        import asyncio
        for attempt in range(15):
            try:
                # Probe with a single connection first (better error messages)
                conn = await asyncpg.connect(database_url, timeout=3)
                await conn.close()
                # Probe succeeded — create the pool
                pool = await asyncpg.create_pool(
                    database_url, min_size=1, max_size=10,
                    command_timeout=10, timeout=5,
                )
                await db.init_schema(pool)
                _stock_gauge_cache[0] = await db.get_total_stock(pool)
                _db_pool_ref = pool
                logger.info("database connected on attempt %d", attempt + 1)
                break
            except Exception as exc:
                logger.warning("DB connect attempt %d/%d failed: %s: %s",
                               attempt + 1, 15, type(exc).__name__, exc)
                if pool:
                    await pool.close()
                    pool = None
                if attempt < 14:
                    await asyncio.sleep(2)
                else:
                    logger.error("giving up on DB after 15 attempts")

    # Valkey / Redis
    cart_backend: cart.Cart
    if cache_url:
        import redis.asyncio as aioredis

        redis_client = aioredis.from_url(cache_url, decode_responses=True)
        cart_backend = cart.ValkeyCart(redis_client)
        logger.info("cache connected")
    else:
        cart_backend = cart.MemoryCart()
        logger.info("using in-memory cart (no CACHE_URL)")

    application.state.pool = pool
    application.state.cart = cart_backend
    application.state.products_cache = await db.get_products(pool) if pool else db.SEED_PRODUCTS

    yield

    if pool:
        await pool.close()


# ---------------------------------------------------------------------------
# Application
# ---------------------------------------------------------------------------
# Progressive delivery env vars
THEME_COLOR = os.getenv("THEME_COLOR", "blue")
APP_VERSION = os.getenv("APP_VERSION", "stable")

app = FastAPI(title="Platform Demo Shop", lifespan=lifespan)
if os.getenv("OTEL_SDK_DISABLED", "").lower() != "true":
    FastAPIInstrumentor.instrument_app(app)

# Default state for testing without lifespan (ASGI transport doesn't trigger it)
app.state.pool = None
app.state.cart = cart.MemoryCart()
app.state.products_cache = [
    {**p, "id": i, "stock": 100} for i, p in enumerate(db.SEED_PRODUCTS, start=1)
]

app.mount("/static", StaticFiles(directory="app/static"), name="static")
templates = Jinja2Templates(directory="app/templates")


def _session_id(request: Request) -> str:
    """Get or create a session ID from a cookie."""
    return request.cookies.get("shop_session", str(uuid.uuid4()))


def _set_session_cookie(response, session_id: str):
    response.set_cookie("shop_session", session_id, httponly=True, samesite="lax", max_age=86400)
    return response


# ---------------------------------------------------------------------------
# Routes
# ---------------------------------------------------------------------------
@app.get("/healthz")
async def healthz():
    return {"status": "ok"}


@app.get("/", response_class=HTMLResponse)
async def catalog(request: Request):
    products = request.app.state.products_cache
    cart_items = await request.app.state.cart.get_items(_session_id(request))
    cart_count = sum(i["quantity"] for i in cart_items)
    resp = templates.TemplateResponse(
        request, "catalog.html",
        {"products": products, "cart_count": cart_count},
    )
    return _set_session_cookie(resp, _session_id(request))


@app.get("/product/{product_id}", response_class=HTMLResponse)
async def product_detail(request: Request, product_id: int):
    pool = request.app.state.pool
    product = await db.get_product(pool, product_id) if pool else _find_seed(product_id)
    if not product:
        return templates.TemplateResponse(
            request, "catalog.html",
            {"products": request.app.state.products_cache, "cart_count": 0, "error": "Product not found"},
            status_code=404,
        )
    product_views.add(1, {"product_id": str(product_id), "product_name": product["name"]})
    logger.info("product viewed", extra={"product_id": product_id, "product_name": product["name"]})
    cart_items = await request.app.state.cart.get_items(_session_id(request))
    cart_count = sum(i["quantity"] for i in cart_items)
    if product.get("stock", 100) < 10:
        logger.warning(
            "low stock", extra={"product_id": product_id, "stock": product.get("stock", 0)}
        )
    return templates.TemplateResponse(
        request, "product.html",
        {"product": product, "cart_count": cart_count},
    )


@app.post("/cart/add", response_class=HTMLResponse)
async def add_to_cart(request: Request, product_id: int = Form(...)):
    session_id = _session_id(request)
    pool = request.app.state.pool
    product = await db.get_product(pool, product_id) if pool else _find_seed(product_id)
    if not product:
        return HTMLResponse("Product not found", status_code=404)
    items = await request.app.state.cart.add_item(session_id, product)
    cart_additions.add(1)
    logger.info("cart item added", extra={"session": session_id[:8], "product_id": product_id})
    cart_count = sum(i["quantity"] for i in items)
    html = f'<span id="cart-count" hx-swap-oob="true">{cart_count}</span>'
    resp = HTMLResponse(html)
    return _set_session_cookie(resp, session_id)


@app.post("/cart/remove/{product_id}", response_class=HTMLResponse)
async def remove_from_cart(request: Request, product_id: int):
    session_id = _session_id(request)
    await request.app.state.cart.remove_item(session_id, product_id)
    return RedirectResponse("/cart", status_code=303)


@app.get("/cart", response_class=HTMLResponse)
async def cart_page(request: Request):
    session_id = _session_id(request)
    items = await request.app.state.cart.get_items(session_id)
    total = sum(i["price_cents"] * i["quantity"] for i in items)
    cart_count = sum(i["quantity"] for i in items)
    return templates.TemplateResponse(
        request, "cart.html",
        {"items": items, "total": total, "cart_count": cart_count},
    )


@app.post("/checkout")
async def checkout(request: Request):
    session_id = _session_id(request)
    items = await request.app.state.cart.get_items(session_id)
    if not items:
        return RedirectResponse("/cart", status_code=303)

    total = sum(i["price_cents"] * i["quantity"] for i in items)

    with tracer.start_as_current_span("checkout.process_order") as span:
        span.set_attribute("cart.item_count", len(items))
        span.set_attribute("order.total_cents", total)

        pool = request.app.state.pool
        if pool:
            order_id = await db.create_order(pool, session_id, items, total)
            # Refresh stock gauge cache
            _stock_gauge_cache[0] = await db.get_total_stock(pool)
        else:
            order_id = 0

        await request.app.state.cart.clear(session_id)

    orders_placed.add(1)
    revenue_cents.add(total)
    logger.info(
        "order placed",
        extra={"order_id": order_id, "total_cents": total, "item_count": len(items)},
    )

    return RedirectResponse("/orders", status_code=303)


@app.get("/orders", response_class=HTMLResponse)
async def orders_page(request: Request):
    session_id = _session_id(request)
    pool = request.app.state.pool
    order_list = await db.get_orders(pool, session_id) if pool else []
    cart_items = await request.app.state.cart.get_items(session_id)
    cart_count = sum(i["quantity"] for i in cart_items)
    return templates.TemplateResponse(
        request, "orders.html",
        {"orders": order_list, "cart_count": cart_count},
    )


@app.get("/api/products")
async def api_products(request: Request):
    return request.app.state.products_cache


@app.get("/api/version")
async def api_version():
    return {"version": APP_VERSION, "theme_color": THEME_COLOR}


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
def _find_seed(product_id: int) -> dict | None:
    """Find a product in seed data (for local dev without DB)."""
    for i, p in enumerate(db.SEED_PRODUCTS, start=1):
        if i == product_id:
            return {**p, "id": i, "stock": 100}
    return None

# Platform Demo — Agent Instructions

Online shop demo built with FastAPI + HTMX. Showcases multi-service deployment,
OpenTelemetry observability, and business metrics on the platform.

## Project Structure

```
app/main.py              — FastAPI application (shop routes, OTEL setup)
app/db.py                — Database layer (asyncpg, schema, seed data)
app/cart.py              — Shopping cart (Valkey-backed, in-memory fallback)
app/templates/           — Jinja2 HTML templates (HTMX-powered)
app/static/              — CSS styles
tests-e2e/test_app.py   — pytest + httpx tests (dual-mode: ASGI + HTTP)
requirements.txt         — Python dependencies
Dockerfile               — Production image (uvicorn)
Dockerfile.test          — Test runner image (pytest)
Dockerfile.dev           — Agent dev environment
deploy/production.yaml   — K8s deployment (Postgres + Valkey + App)
.platform.yaml           — CI/CD pipeline definition
```

## Architecture

- **App** — FastAPI on port 8080 with HTMX frontend
- **Postgres** — Products catalog, order history
- **Valkey** — Shopping cart sessions (24h TTL)
- **OTEL** — Traces, logs, and business metrics sent to platform

## Development Workflow

1. **Read this file first** to understand the project
2. **Write tests first** in `tests-e2e/test_app.py` using pytest + httpx
3. **Implement the feature** in `app/` modules and templates
4. **Run tests locally**: `pytest tests-e2e/ -v`
5. **Commit and push** to trigger the pipeline

## Key Patterns

- Products seeded on first startup (6 SaaS-themed items)
- Cart stored in Valkey keyed by session cookie, falls back to in-memory
- Orders persist in Postgres with JSONB items
- HTMX handles interactivity without JavaScript
- Health check at `/healthz`
- Custom OTEL metrics: `shop.product_views`, `shop.cart_additions`,
  `shop.orders_placed`, `shop.revenue_cents`, `shop.products_in_stock`

## Build Verification

After pushing, check pipeline status with `platform-build-status`.

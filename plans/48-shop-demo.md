# Plan 48: Shop Demo App

## Context

The current demo project is a simple HTMX counter app — cute but not compelling. A shop demo is relatable to everyone (founders, marketers, developers) and naturally exercises the platform's full feature set: multi-service K8s deployments (app + Postgres + Valkey), OTEL observability with real business metrics, schema migrations, and future canary/A-B testing scenarios.

All changes are in `src/onboarding/templates/` (app code) and `src/onboarding/demo_project.rs` (embedding + sample issues). No migrations, no API changes, no AppState changes.

The demo will be developed and tested locally on macOS using `uv` before being embedded as the platform's onboarding template.

## Design Principles

- **Always-on OTEL** — telemetry is a hard dependency, not optional. The platform always injects OTEL env vars.
- **Multi-service** — Postgres (products, orders) + Valkey (shopping carts) showcases real K8s orchestration.
- **Business metrics** — product views, cart additions, orders placed, revenue. Not just HTTP latency.
- **Dual-mode tests** — tests run both in-process (local dev via ASGI transport) and against deployed service (CI pipeline).
- **HTMX + Tailwind** — same frontend stack as the counter app; no JS build step.

---

## PR 1: Shop Demo Template

Replace the counter demo with a shop demo across all template files.

- [ ] Types & errors defined
- [ ] Migration applied (N/A — app-level schema init)
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### App Architecture

```
app/
  main.py          — FastAPI app, OTEL setup, lifespan (DB init + Valkey connect)
  db.py            — asyncpg: schema creation, seed products, order queries
  cart.py           — redis-py async: cart CRUD keyed by session cookie
  templates/
    base.html      — Shell with nav (catalog, cart badge, orders)
    catalog.html   — Product grid with "Add to cart" buttons
    product.html   — Product detail page
    cart.html      — Cart contents with quantity controls + checkout
    orders.html    — Order history list
  static/
    style.css      — Custom animations/transitions
```

### Database Schema (app-level, created on startup)

```sql
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
```

Seed data: 6 products across 2-3 categories (e.g., "Starter Kit", "Pro Bundle", "API Credits Pack", "Support Plan", "Custom Domain", "Analytics Add-on" — SaaS-themed products that fit the platform narrative).

### Cart (Valkey)

- Key pattern: `cart:{session_id}` where session_id is a UUID from a cookie
- Value: JSON array `[{"product_id": 1, "name": "...", "price_cents": 999, "quantity": 2}]`
- TTL: 24 hours (auto-expire abandoned carts)
- Operations: add item, remove item, update quantity, get cart, clear cart

Session cookie: `shop_session` — set on first visit if not present, `httponly`, `samesite=lax`.

### Routes

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/healthz` | Health check → `{"status": "ok"}` |
| `GET` | `/` | Product catalog (grid layout) |
| `GET` | `/product/{id}` | Product detail page |
| `POST` | `/cart/add` | Add item to cart (HTMX, returns cart badge) |
| `POST` | `/cart/remove/{product_id}` | Remove item from cart (HTMX) |
| `GET` | `/cart` | Cart page |
| `POST` | `/checkout` | Create order, clear cart, redirect to orders |
| `GET` | `/orders` | Order history |
| `GET` | `/api/products` | JSON product list (API endpoint) |

### OTEL Metrics

All custom metrics use the `shop.` prefix for clear namespacing:

| Metric | Type | Description |
|--------|------|-------------|
| `shop.product_views` | Counter | Product detail page views (attributes: `product_id`, `product_name`) |
| `shop.cart_additions` | Counter | Items added to cart |
| `shop.orders_placed` | Counter | Completed orders |
| `shop.revenue_cents` | Counter | Revenue in cents (add order total on checkout) |
| `shop.products_in_stock` | ObservableGauge | Total products in stock (callback queries DB) |

Plus auto-instrumented by `FastAPIInstrumentor`:
- `http.server.duration` — request latency histogram
- `http.server.request.size` / `http.server.response.size`
- `http.server.active_requests`

### OTEL Traces

Auto-instrumented by `FastAPIInstrumentor` — each HTTP request creates a span. For checkout, we add a custom span wrapping the DB transaction:

```python
tracer = trace.get_tracer(__name__)

async def checkout(...):
    with tracer.start_as_current_span("checkout.process_order") as span:
        span.set_attribute("cart.item_count", len(cart_items))
        span.set_attribute("order.total_cents", total)
        # ... create order in DB, clear cart in Valkey
```

### OTEL Logs

Python `logging` bridged to OTEL `LoggerProvider`. Key log points:
- `INFO` — order placed (order_id, total, item_count)
- `INFO` — product viewed (product_id, product_name)
- `WARN` — stock low (product_id, remaining stock)
- `INFO` — cart updated (session, action, product_id)

### Deploy Manifest (`deploy/production.yaml`)

Three services: Postgres + Valkey + App.

```yaml
# --- Postgres ---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{ project_name }}-db
spec:
  replicas: 1
  selector:
    matchLabels:
      app: {{ project_name }}-db
  template:
    metadata:
      labels:
        app: {{ project_name }}-db
    spec:
      containers:
        - name: postgres
          image: postgres:16-alpine
          ports:
            - containerPort: 5432
          env:
            - name: POSTGRES_DB
              value: app
            - name: POSTGRES_USER
              value: app
            - name: POSTGRES_PASSWORD
              value: "{{ values.db_password | default('changeme') }}"
---
apiVersion: v1
kind: Service
metadata:
  name: {{ project_name }}-db
spec:
  selector:
    app: {{ project_name }}-db
  ports:
    - port: 5432
      targetPort: 5432
---
# --- Valkey (Redis-compatible cache for shopping carts) ---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{ project_name }}-cache
spec:
  replicas: 1
  selector:
    matchLabels:
      app: {{ project_name }}-cache
  template:
    metadata:
      labels:
        app: {{ project_name }}-cache
    spec:
      containers:
        - name: valkey
          image: valkey/valkey:8-alpine
          ports:
            - containerPort: 6379
          resources:
            requests:
              cpu: 50m
              memory: 64Mi
            limits:
              cpu: 200m
              memory: 128Mi
---
apiVersion: v1
kind: Service
metadata:
  name: {{ project_name }}-cache
spec:
  selector:
    app: {{ project_name }}-cache
  ports:
    - port: 6379
      targetPort: 6379
---
# --- Application ---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{ project_name }}-app
spec:
  replicas: 1
  selector:
    matchLabels:
      app: {{ project_name }}-app
  template:
    metadata:
      labels:
        app: {{ project_name }}-app
    spec:
      imagePullSecrets:
        - name: platform-registry-pull
      containers:
        - name: app
          image: {{ image_ref }}
          ports:
            - containerPort: 8080
          env:
            - name: DATABASE_URL
              value: "postgresql://app:{{ values.db_password | default('changeme') }}@{{ project_name }}-db:5432/app"
            - name: CACHE_URL
              value: "redis://{{ project_name }}-cache:6379"
          readinessProbe:
            httpGet:
              path: /healthz
              port: 8080
            initialDelaySeconds: 5
            periodSeconds: 10
          livenessProbe:
            httpGet:
              path: /healthz
              port: 8080
            initialDelaySeconds: 15
            periodSeconds: 30
          resources:
            requests:
              cpu: 100m
              memory: 128Mi
            limits:
              cpu: 500m
              memory: 256Mi
---
apiVersion: v1
kind: Service
metadata:
  name: {{ project_name }}-app
spec:
  selector:
    app: {{ project_name }}-app
  ports:
    - port: 8080
      targetPort: 8080
```

### Requirements

**`requirements.txt`:**
```
fastapi==0.115.6
uvicorn[standard]==0.34.0
jinja2==3.1.5
python-multipart==0.0.20
asyncpg>=0.30.0
redis>=5.0.0
opentelemetry-api>=1.25.0
opentelemetry-sdk>=1.25.0
opentelemetry-exporter-otlp-proto-http>=1.25.0
opentelemetry-instrumentation-fastapi>=0.46b0
opentelemetry-instrumentation-logging>=0.46b0
```

**`requirements-test.txt`:**
```
pytest>=8.0
pytest-timeout>=2.3
pytest-asyncio>=0.24
httpx>=0.27
```

### Dockerfile

```dockerfile
FROM python:3.12-slim
WORKDIR /app
COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt
COPY app/ app/
EXPOSE 8080
CMD ["uvicorn", "app.main:app", "--host", "0.0.0.0", "--port", "8080"]
```

Note: removed `COPY app/static/ static/` (static files are under `app/static/` and served by FastAPI mount, no separate copy needed).

### Tests (`tests-e2e/test_app.py`)

Dual-mode: ASGI transport for local dev, HTTP for deployed service.

**Test cases (~8 tests):**
1. `test_healthz` — GET `/healthz` → 200, `{"status": "ok"}`
2. `test_catalog_page` — GET `/` → 200, contains product names
3. `test_product_detail` — GET `/product/1` → 200, contains product info
4. `test_product_not_found` — GET `/product/999` → 404
5. `test_add_to_cart` — POST `/cart/add` → cart updated
6. `test_cart_page` — GET `/cart` → 200, shows cart contents
7. `test_checkout` — POST `/checkout` → order created, cart cleared
8. `test_orders_page` — GET `/orders` → 200, shows order history
9. `test_api_products` — GET `/api/products` → 200, JSON array

For local testing: ASGI transport bypasses HTTP, tests against the FastAPI app directly. Cart tests use an in-memory fallback if Valkey is not available.

### Sample Issues (update `demo_project.rs`)

Replace counter-specific issues with shop-relevant ones:

1. **"Explore the platform"** (open, documentation) — same welcome issue, updated text
2. **"Add product search and filtering"** (open, enhancement) — add search bar, category filter
3. **"Persist cart across sessions"** (open, enhancement) — add user accounts, associate carts
4. **"Add product reviews"** (open, enhancement) — star ratings, review text, schema migration example
5. **"Set up monitoring alerts"** (open, ops) — alert on high error rate, low stock, order failures

### Embedding Changes (`src/onboarding/demo_project.rs`)

Update `demo_project_template_files()` to return 20 files (was 15):

**Removed:** `app/templates/index.html`
**Added:** `app/db.py`, `app/cart.py`, `app/templates/catalog.html`, `app/templates/product.html`, `app/templates/cart.html`, `app/templates/orders.html`

Update test `demo_template_file_count` assertion: `assert_eq!(files.len(), 20)`.
Update test `demo_template_has_main_py` to check for `FastAPI` and `shop` content.

### Local Development with `uv`

```bash
cd src/onboarding/templates

# Create venv and install deps
uv venv
source .venv/bin/activate
uv pip install -r requirements.txt -r requirements-test.txt

# Start local Postgres + Valkey (Docker or existing)
# Option A: use existing Kind cluster services
# Option B: run directly
docker run -d --name shop-pg -p 5433:5432 -e POSTGRES_DB=app -e POSTGRES_USER=app -e POSTGRES_PASSWORD=changeme postgres:16-alpine
docker run -d --name shop-valkey -p 6380:6379 valkey/valkey:8-alpine

# Run the app
DATABASE_URL=postgresql://app:changeme@localhost:5433/app CACHE_URL=redis://localhost:6380 \
  uvicorn app.main:app --host 0.0.0.0 --port 8080 --reload

# Run tests (in-process, no external services needed for ASGI mode)
pytest tests-e2e/ -v

# Run tests against running app
APP_HOST=localhost APP_PORT=8080 pytest tests-e2e/ -v
```

### Code Changes

| File | Change |
|------|--------|
| `src/onboarding/templates/app/main.py` | Rewrite: shop app with OTEL, lifespan, routes |
| `src/onboarding/templates/app/db.py` | New: asyncpg schema, seed data, order queries |
| `src/onboarding/templates/app/cart.py` | New: Valkey-backed cart operations |
| `src/onboarding/templates/app/templates/base.html` | Rewrite: nav with cart badge |
| `src/onboarding/templates/app/templates/catalog.html` | New: product grid |
| `src/onboarding/templates/app/templates/product.html` | New: product detail |
| `src/onboarding/templates/app/templates/cart.html` | New: cart page |
| `src/onboarding/templates/app/templates/orders.html` | New: order history |
| `src/onboarding/templates/app/templates/index.html` | Delete (replaced by catalog.html) |
| `src/onboarding/templates/app/static/style.css` | Rewrite: shop styles |
| `src/onboarding/templates/requirements.txt` | Add: asyncpg, redis |
| `src/onboarding/templates/requirements-test.txt` | Update: pytest-asyncio |
| `src/onboarding/templates/Dockerfile` | Update: remove static copy line |
| `src/onboarding/templates/deploy/production.yaml` | Rewrite: add Valkey, update env vars |
| `src/onboarding/templates/tests-e2e/test_app.py` | Rewrite: shop test cases |
| `src/onboarding/templates/CLAUDE.md` | Rewrite: shop-specific instructions |
| `src/onboarding/templates/README.md` | Rewrite: shop description |
| `src/onboarding/demo_project.rs` | Update: file list, sample issues, file count test |

### Test Outline

**New behaviors to test:**
- Health check — unit (ASGI)
- Catalog renders with products — unit (ASGI, in-memory DB fallback)
- Product detail page — unit (ASGI)
- Cart add/remove/clear — unit (ASGI, in-memory cart fallback)
- Checkout flow — unit (ASGI)
- Order history — unit (ASGI)
- API products endpoint — unit (ASGI)

**Error paths:**
- Product not found → 404
- Empty cart checkout → redirect with message
- Invalid product ID → 400

**Existing tests affected:**
- `src/onboarding/demo_project.rs` — file count assertion (15 → 20), content assertions

**Estimated test count:** ~9 E2E tests + 7 Rust unit tests (existing, updated assertions)

### Verification

1. `uv pip install -r requirements.txt && uvicorn app.main:app` starts without errors
2. `pytest tests-e2e/ -v` passes all tests in ASGI mode
3. `SQLX_OFFLINE=true cargo nextest run --lib -E 'test(demo_template)'` passes
4. Browse http://localhost:8080 — see product catalog, add to cart, checkout works
5. OTEL metrics appear when `OTEL_EXPORTER_OTLP_ENDPOINT` is set

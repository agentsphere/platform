# Platform Demo Shop

An online shop demo built with FastAPI, HTMX, Postgres, and Valkey — showcasing
the platform's multi-service deployments, CI/CD pipelines, and observability.

## What to try next

1. **Browse the shop** — Products, cart, and checkout all work
2. **Run the pipeline** — Go to Builds tab and trigger a build
3. **Start an agent session** — Try: "Add product search and filtering"
4. **View metrics** — Check Observe > Metrics for `shop.*` business metrics
5. **Check traces** — Observe > Traces shows request flows through the app
6. **Create an issue** — Try creating your own issue in the Issues tab

## Local development

```bash
pip install -r requirements.txt
DATABASE_URL=postgresql://app:changeme@localhost:5432/app \
  CACHE_URL=redis://localhost:6379 \
  uvicorn app.main:app --reload --port 8080
```

## Running tests

```bash
pip install -r requirements-test.txt
pytest tests-e2e/ -v
```

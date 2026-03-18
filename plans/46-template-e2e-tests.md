# Plan 46: Default E2E Test Templates with Sensible Timeouts

## Context

When the platform scaffolds a new project, the git template includes `Dockerfile.test` (pytest runner) and pipeline config (`.platform.yaml` with `deploy_test` steps), but no actual test files. The agent writes tests from scratch. This leads to:

1. **Tests with no HTTP timeouts** — agent-written `requests.get(url)` / `httpx.get(url)` calls use system defaults (60-120s), causing test pods to hang for 10+ minutes on simple failures
2. **No baseline test** — even `GET /healthz` has no starter test; the agent must create everything
3. **APP_HOST mismatch** — executor sets `APP_HOST={project_name}-app` but the default deploy template names the Service `{{ project_name }}` (no `-app` suffix), causing DNS lookup failures
4. **APP_PORT mismatch** — executor sets `APP_PORT=8080` but the Service maps `80→8080`, so connecting to `service:8080` fails

The fix: ship default e2e test files in the template with 3s HTTP timeouts, fix the APP_HOST/APP_PORT mismatches, and add a platform-level e2e test to verify the whole template works end-to-end.

## Design Principles

- **Ship working defaults** — the scaffold test files should pass against any app that has a `/healthz` endpoint
- **Fast failure** — 3s HTTP timeout per request, 10s pytest timeout per test, `-x` to stop on first failure
- **Rename to `tests-e2e/`** — distinguishes deploy-test e2e tests from unit tests the agent may create in `tests/`
- **Fix the plumbing first** — APP_HOST and APP_PORT must match the default template before test files will work

---

## PR 1: Fix APP_HOST/APP_PORT + Add Template E2E Tests

Single PR since the test files are useless without the plumbing fix and vice versa.

- [x] Types & errors defined (N/A — no new types)
- [x] Migration applied (N/A)
- [x] Tests written (unit tests in templates.rs)
- [x] Implementation complete
- [x] Integration/E2E tests passing (manual Docker build + Kind deploy verified)
- [ ] Quality gate passed (blocked by pre-existing compilation errors in unrelated files)

### 1A. Fix APP_HOST / APP_PORT mismatch in executor

**Problem:** `src/pipeline/executor.rs:1626` sets `APP_HOST` to `{project_name}-app` but the default deploy template `src/git/templates/deploy/production.yaml:81` creates a Service named `{{ project_name }}`. Same for port: executor says 8080, Service listens on 80.

**Fix: Change deploy template to use `-app` suffix and port 8080:8080**
- Service name: `{{ project_name }}-app` (matches executor's `APP_HOST`)
- Service port: `8080:8080` (matches executor's `APP_PORT`)
- Deployment name: `{{ project_name }}-app` (matches Service selector)
- This is more explicit (distinguishes app from db service)

| File | Change |
|---|---|
| `src/git/templates/deploy/production.yaml` | Rename app Deployment/Service from `{{ project_name }}` to `{{ project_name }}-app`, change Service port to `8080:8080` |
| `src/git/templates/CLAUDE.md` | Update any references to service names |

### 1B. Add template test files

Create `src/git/templates/tests-e2e/` with starter test files:

**`src/git/templates/tests-e2e/conftest.py`:**
```python
import os
import httpx
import pytest

@pytest.fixture
def base_url():
    host = os.environ.get("APP_HOST", "localhost")
    port = os.environ.get("APP_PORT", "8080")
    return f"http://{host}:{port}"

@pytest.fixture
def client(base_url):
    """HTTP client with 3s timeout — tests should fail fast, not hang."""
    with httpx.Client(base_url=base_url, timeout=3.0) as c:
        yield c
```

**`src/git/templates/tests-e2e/test_healthz.py`:**
```python
def test_healthz_returns_ok(client):
    resp = client.get("/healthz")
    assert resp.status_code == 200
    data = resp.json()
    assert data["status"] == "ok"
```

**`src/git/templates/tests-e2e/test_api.py`:**
```python
def test_root_returns_html_or_json(client):
    """Smoke test: the app responds on / with some content."""
    resp = client.get("/")
    assert resp.status_code in (200, 307, 308)  # allow redirects
```

**`src/git/templates/requirements-test.txt`:**
```
pytest>=8.0
pytest-timeout>=2.3
httpx>=0.27
```

### 1C. Update Dockerfile.test to use `tests-e2e/`

```dockerfile
FROM python:3.12-slim

WORKDIR /tests

COPY requirements-test.txt .
RUN pip install --no-cache-dir -r requirements-test.txt

COPY tests-e2e/ ./tests-e2e/

ENV APP_HOST=localhost
ENV APP_PORT=8080

# --timeout=10: kill any single test after 10s (pytest-timeout)
# -x: stop on first failure (fast CI feedback)
CMD ["pytest", "tests-e2e/", "-v", "--tb=short", "--timeout=10", "-x", "--junitxml=/tmp/test-results.xml"]
```

### 1D. Update templates.rs to embed new files

Add `include_str!` for:
- `templates/tests-e2e/conftest.py`
- `templates/tests-e2e/test_healthz.py`
- `templates/tests-e2e/test_api.py`
- `templates/requirements-test.txt`

Add to `project_template_files()` return vec. Update `template_files_count` test from 8 to 12.

### 1E. Update CLAUDE.md and dev.md references

- `tests/` → `tests-e2e/` everywhere in CLAUDE.md
- Default project structure: add `tests-e2e/conftest.py`, `tests-e2e/test_healthz.py`, `tests-e2e/test_api.py`
- Add timeout guidance: "Always use `timeout=3` in httpx clients. The CI runner enforces 10s per-test via `pytest-timeout`."
- `dev.md` step 4/6: update test paths

### 1F. Update onboarding demo templates

Update `src/onboarding/templates/`:
- Rename `tests/` → `tests-e2e/` in references
- Update the demo's `Dockerfile` if it references `tests/`
- Update the demo's `CLAUDE.md` and `.platform.yaml` test references

### Code Changes

| File | Change |
|---|---|
| `src/git/templates/deploy/production.yaml` | Rename app resources to `{{ project_name }}-app`, port 8080:8080 |
| `src/git/templates/Dockerfile.test` | COPY `tests-e2e/`, add `--timeout=10 -x` |
| `src/git/templates/tests-e2e/conftest.py` | **New:** shared fixtures (base_url, client with 3s timeout) |
| `src/git/templates/tests-e2e/test_healthz.py` | **New:** healthz smoke test |
| `src/git/templates/tests-e2e/test_api.py` | **New:** root endpoint smoke test |
| `src/git/templates/requirements-test.txt` | **New:** pytest + pytest-timeout + httpx |
| `src/git/templates/CLAUDE.md` | Update test paths, add timeout guidance |
| `src/git/templates/.claude/commands/dev.md` | Update test paths |
| `src/git/templates.rs` | Embed 4 new files, update file count |
| `src/onboarding/templates/*` | Update test paths if demo uses `tests/` |
| `src/onboarding/demo_project.rs` | Update template file paths |

### Test Outline — PR 1

**Unit tests (templates.rs):**
- `template_files_count` — 8→12
- `template_has_conftest` — conftest.py contains `timeout=3`
- `template_has_healthz_test` — test_healthz.py contains `/healthz`
- `template_has_requirements_test` — requirements-test.txt contains `pytest-timeout`
- `template_deploy_uses_app_suffix` — deploy/production.yaml uses `{{ project_name }}-app`

**Existing tests affected:**
- `src/git/templates.rs::template_files_count` — 8→12
- `src/git/templates.rs::template_deploy_has_postgres` — may need update if deploy template assertions change
- `src/git/repo.rs` repo init tests — verify nested `tests-e2e/` dir committed

### Verification — E2E: Build, deploy, and run template images in Kind

This verification is the key deliverable. Runs as a platform e2e test (`tests/e2e_template_test.rs`) that requires Kind + Docker (same as all e2e tests).

**Steps:**

1. **Scaffold a temp project directory** with all template files (using `project_template_files()`)
2. **Create a minimal FastAPI app** in the temp dir that satisfies the template's requirements:
   - `app/main.py` with `/healthz` returning `{"status": "ok"}` and `/` returning HTML
   - `requirements.txt` with `fastapi` and `uvicorn`
3. **Build the app image** with `docker build -f Dockerfile -t localhost:{PORT}/{project}/app:test .`
4. **Push to Kind registry** via `docker push localhost:{PORT}/{project}/app:test`
5. **Build the test image** with `docker build -f Dockerfile.test -t localhost:{PORT}/{project}/test:test .`
6. **Push test image** to Kind registry
7. **Deploy app to Kind namespace**:
   - Create namespace
   - Render deploy/production.yaml with minijinja (image_ref, project_name)
   - `kubectl apply` the rendered manifests
   - Wait for deployment ready
8. **Run test pod (success case)**: Create pod with test image, `APP_HOST={project}-app`, `APP_PORT=8080`
   - Assert: pod exits with code 0 (all tests pass)
   - Assert: pod completes within 30 seconds (no hanging)
9. **Run test pod (failure case)**: Create pod with `APP_HOST=nonexistent-host`
   - Assert: pod exits with non-zero code
   - Assert: pod completes within 15 seconds (timeout kicks in, doesn't hang)
10. **Cleanup**: delete namespace

This e2e test proves:
- App Dockerfile builds and runs
- Test Dockerfile builds and has correct deps (pytest-timeout, httpx)
- Tests connect to the right host:port (APP_HOST/APP_PORT match Service)
- Tests fail fast on unreachable hosts (3s httpx timeout + 10s pytest-timeout)
- Tests pass against a conforming app

# Project Instructions

This project runs on the Platform DevOps system.

## Key Files

- `.platform.yaml` — CI/CD pipeline definition (kaniko build)
- `Dockerfile` — Application container image (Python/FastAPI)
- `Dockerfile.test` — Test runner image (pytest API tests)
- `Dockerfile.dev` — Dev/agent image build (customise agent environment)
- `deploy/production.yaml` — K8s deployment manifests (minijinja templates)
- `requirements.txt` — Python dependencies
- `requirements-test.txt` — Test dependencies

## Development Workflow

Follow this process for every change. Do NOT skip steps or commit untested code.

CRITICAL: This is a full development environment with K8s access, not a code-only workspace.
You MUST test everything locally before pushing. CI/CD exists to verify and secure, not to be
your first test run. Local testing gives the fastest feedback loops.

### 0. Install Missing Tools

Your pod may not have all language runtimes pre-installed. Install what you need:

```bash
# For Python projects — install if missing
command -v python3 || {
  apt-get update && apt-get install -y --no-install-recommends python3 python3-pip python3-venv && rm -rf /var/lib/apt/lists/*
}

# Install kubectl if missing
command -v kubectl || {
  curl -sLO "https://dl.k8s.io/release/$(curl -sL https://dl.k8s.io/release/stable.txt)/bin/linux/$(uname -m | sed 's/aarch64/arm64/' | sed 's/x86_64/amd64/')/kubectl"
  chmod +x kubectl && mv kubectl /usr/local/bin/
}
```

You have root access and permission to install packages. Use it.

After installing tools, update `Dockerfile.dev` so future agent sessions have them pre-installed:

```dockerfile
# Example: add Python + postgres client to Dockerfile.dev
RUN apt-get update && apt-get install -y --no-install-recommends \
    python3 python3-pip python3-venv postgresql-client \
  && rm -rf /var/lib/apt/lists/*
```

### 1. Set Up Test Infrastructure

Before writing any code, deploy dependencies via kubectl:

```bash
# Verify namespace access
kubectl get pods
kubectl get svc

# Deploy postgres for local testing (the agent pod has K8s access)
cat <<'PGEOF' | kubectl apply -f -
apiVersion: apps/v1
kind: Deployment
metadata:
  name: postgres
spec:
  replicas: 1
  selector:
    matchLabels:
      app: postgres
  template:
    metadata:
      labels:
        app: postgres
    spec:
      containers:
        - name: postgres
          image: postgres:16
          ports:
            - containerPort: 5432
          env:
            - name: POSTGRES_DB
              value: app
            - name: POSTGRES_USER
              value: app
            - name: POSTGRES_PASSWORD
              value: password
---
apiVersion: v1
kind: Service
metadata:
  name: postgres
spec:
  selector:
    app: postgres
  ports:
    - port: 5432
      targetPort: 5432
PGEOF

# Wait for postgres to be ready
kubectl rollout status deployment/postgres --timeout=60s
```

### 2. Create Tests First

Write tests BEFORE implementing features:

```bash
# Install app dependencies first
pip install -r requirements.txt
pip install -r requirements-test.txt

# Unit tests — run locally in the pod, no K8s needed
python -m pytest tests/unit/ -v

# API tests — test against locally running app
DATABASE_URL=postgresql://app:password@postgres:5432/app uvicorn app.main:app --host 0.0.0.0 --port 8080 &
sleep 2
python -m pytest tests/ -v
```

### 3. Verify Test Setup

Run the tests and confirm they fail for the right reason (missing feature, not broken setup):

```bash
python -m pytest tests/ -v --tb=short
```

### 4. Plan Implementation

Think through the implementation before coding:
- What changes are needed?
- What security implications exist? (SQL injection, auth bypass, input validation)
- What edge cases should be handled?

### 5. Implement

Write the code. Keep changes minimal and focused.

### 6. Test (iterate until green)

Run tests in this order — fix failures before moving to the next level:

```bash
# Level 1: Unit tests (fast, no I/O)
python -m pytest tests/unit/ -v

# Level 2: Local API tests (app running in pod)
python -m pytest tests/ -v

# Level 3: Build app image (requires $REGISTRY env var — skip if not set)
# Use kaniko to build directly in the pod (no Docker daemon needed)
if [ -n "${REGISTRY:-}" ]; then
  /kaniko/executor --context=dir:///workspace --dockerfile=/workspace/Dockerfile \
    --destination=$REGISTRY/$PROJECT/session-$SESSION_SHORT_ID-app:latest --insecure --cache=true 2>&1 || echo "BUILD FAILED"
else
  echo "REGISTRY not set — skipping local kaniko build (CI pipeline will build)"
fi

# Level 4: Build test runner image
if [ -n "${REGISTRY:-}" ]; then
  /kaniko/executor --context=dir:///workspace --dockerfile=/workspace/Dockerfile.test \
    --destination=$REGISTRY/$PROJECT/session-$SESSION_SHORT_ID-test:latest --insecure 2>&1 || echo "TEST IMAGE BUILD FAILED"
else
  echo "REGISTRY not set — skipping test image build"
fi

# Level 5: Deploy to session namespace and verify
kubectl apply -f <(cat deploy/production.yaml)  # apply rendered manifests
kubectl rollout status deployment/$(basename $PWD) --timeout=60s
kubectl get pods
```

### 7. Commit, Push, Create MR

The `main` branch is protected — direct pushes are blocked. Always push to a feature branch and create a merge request. CI runs automatically on MRs. When CI passes, auto-merge lands the changes.

Only after ALL tests pass:

```bash
git add -A
git commit -m "feat: description of change"
git push origin $BRANCH
```

Create a merge request via the platform API:

```bash
source /workspace/.platform/.env
curl -sf -X POST "${PLATFORM_API_URL}/api/projects/${PROJECT_ID}/merge-requests" \
  -H "Authorization: Bearer ${PLATFORM_API_TOKEN}" \
  -H "Content-Type: application/json" \
  -d "{\"title\": \"feat: description\", \"source_branch\": \"${BRANCH}\", \"target_branch\": \"main\", \"auto_merge\": true}"
```

### 8. Observe Pipeline

After pushing:

1. Run `platform-build-status` to wait for the pipeline to complete
2. If the build fails, read the error output, fix, and push again
3. Repeat up to 3 times. If still failing, report the error and stop.

## Pipeline

Pushing triggers the pipeline defined in `.platform.yaml`.
Available env vars in pipeline steps: `$REGISTRY`, `$PROJECT`, `$COMMIT_SHA`, `$COMMIT_BRANCH`, `$PIPELINE_TRIGGER`.

Available env vars in agent pods: `$REGISTRY` (registry push URL), `$PROJECT` (project name), `$SESSION_SHORT_ID` (8-char session prefix), `$DOCKER_CONFIG` (kaniko config path, auto-configured).

### Per-Step Conditions

Steps can have an `only:` field to control when they run:

```yaml
steps:
  - name: lint
    image: rust:1.85
    commands: [cargo clippy]
    only:
      events: [mr]           # only run on MR pipelines
      branches: ["feature/*"] # only on feature branches
```

Both `events` and `branches` AND together. Valid events: `push`, `mr`, `tag`, `api`. Empty list = match all. Steps without `only:` always run.

### Deploy-Test Steps (advanced, do not add unless explicitly requested)

The platform supports `deploy_test:` steps that deploy the built app to a temporary K8s namespace and run integration tests. This is an advanced feature — do NOT add it to `.platform.yaml` unless explicitly asked. The default pipeline (build + build-test) is sufficient for most projects.

```yaml
  - name: e2e
    deploy_test:
      test_image: $REGISTRY/$PROJECT/test:$COMMIT_SHA
      manifests: deploy/production.yaml   # default
      readiness_path: /healthz            # default
      readiness_timeout: 120              # seconds, default
    only:
      events: [mr]
```

## Build Verification

After pushing code that includes a Dockerfile and `.platform.yaml`, you MUST verify the pipeline build succeeds:

1. Push your code: `git add -A && git commit -m "message" && git push origin $BRANCH`
2. Run `platform-build-status` to wait for the pipeline to complete
   (it reads PROJECT_ID and BRANCH from the environment or `.platform/.env`)
3. If the build fails, read the error output carefully, fix the Dockerfile or pipeline config, commit, push, and run `platform-build-status` again
4. Repeat up to 3 times. If the build still fails after 3 attempts, report the error and stop.

The `platform-build-status` script will print step statuses and logs for any failed steps.

## Dev Image

The `dev_image` section in `.platform.yaml` specifies a Dockerfile for building a custom agent image.
When the pipeline succeeds, this image becomes the default for new agent sessions in this project.

Edit `Dockerfile.dev` to install project-specific tools (compilers, runtimes, linters).

## Deploy Manifests

Templates use minijinja syntax:

- `{{ project_name }}` — project name
- `{{ image_ref }}` — built container image reference
- `{{ values.replicas | default(1) }}` — configurable values
- `{{ values.db_password | default('changeme') }}` — database password

### Registry Pull Secret

The platform automatically creates a `platform-registry-pull` imagePullSecret in each project namespace. Always include it in your deploy manifests:

```yaml
spec:
  imagePullSecrets:
    - name: platform-registry-pull
```

This secret is refreshed on every deploy — do not modify or delete it.

## Deploying Dependencies (Databases, Caches, etc.)

The agent pod runs inside Kubernetes with a service account that has deploy access to the session namespace.
Deploy dependencies (postgres, redis, etc.) via `kubectl apply` before testing — see Step 1 above for the PostgreSQL example.

Connection string: `postgresql://app:password@postgres:5432/app`

The same pattern works for Redis, MinIO, or any other dependency. Deploy to the current namespace and reference by service name.

## Building Images Directly (Without Pipeline)

You can build and test images directly from your agent pod using kaniko:

```bash
# Check that registry env vars are available
echo "REGISTRY=$REGISTRY PROJECT=$PROJECT SESSION_SHORT_ID=$SESSION_SHORT_ID"

# Build app image
/kaniko/executor --context=dir:///workspace --dockerfile=/workspace/Dockerfile \
  --destination=$REGISTRY/$PROJECT/session-$SESSION_SHORT_ID-app:latest --insecure --cache=true

# Build test image
/kaniko/executor --context=dir:///workspace --dockerfile=/workspace/Dockerfile.test \
  --destination=$REGISTRY/$PROJECT/session-$SESSION_SHORT_ID-test:latest --insecure

# Deploy to your session namespace
kubectl apply -f deploy/production.yaml
kubectl rollout status deployment/$(basename $PWD) --timeout=120s
```

If `$REGISTRY` is not set, kaniko builds are not available — push your code and the CI pipeline will build instead.

This lets you verify everything works before committing and running the full pipeline.

## Application Requirements

- App must listen on port 8080
- Include a `GET /healthz` endpoint returning `{"status": "ok"}`
- Configure OpenTelemetry SDK reading `OTEL_EXPORTER_OTLP_ENDPOINT` and `OTEL_SERVICE_NAME` env vars
- Read `DATABASE_URL` env var for database connection

## Default Project Structure

The repo ships with starter templates. Adapt them to your tech stack:

```
app/              # application source code
  main.py         # entrypoint (FastAPI, port 8080, /healthz)
  db.py           # database connection (reads DATABASE_URL)
  models.py       # data models
  routes.py       # API routes
static/           # frontend assets (HTML/JS/CSS)
tests/            # API / integration tests
  test_healthz.py # health check smoke test
  test_api.py     # endpoint tests (uses APP_HOST / APP_PORT env vars)
requirements.txt      # app dependencies
requirements-test.txt # test dependencies
```

Adjust file names and layout when using a different language or framework.

## What NOT to Create

This is a Kubernetes-native platform. Do NOT create:
- `docker-compose.yml` — there is no Docker Compose; use `kubectl` for local services
- `.env.example` — the app reads `DATABASE_URL` from the K8s environment
- Extra README files — `CLAUDE.md` is the project documentation

## Visual Preview (Dev Server)

The platform provides a live preview iframe in the session view. To use it:

1. **Start a dev server on port 8000**, binding to all interfaces:

   **Vite (React/Vue/Svelte/Preact):**
   ```bash
   npx vite --host 0.0.0.0 --port 8000 --base './'
   ```

   **Next.js:**
   ```bash
   npx next dev -H 0.0.0.0 -p 8000
   ```

   **Webpack Dev Server:**
   ```bash
   npx webpack serve --host 0.0.0.0 --port 8000 --public-path './'
   ```

   **Python (static files):**
   ```bash
   python3 -m http.server 8000 --bind 0.0.0.0
   ```

2. **Use relative base paths** (`base: './'` for vite, `publicPath: './'` for webpack). This ensures assets load correctly through the platform proxy.

3. **Port 8000 is reserved** for preview. The `PREVIEW_PORT` env var is set to `8000`.

4. The preview automatically appears in the session view once the dev server starts responding.

5. Hot Module Replacement (HMR) works automatically — the platform proxies WebSocket connections.

6. **Additional preview ports**: To expose more UIs (monorepo), create K8s Services in the session namespace with label `platform.io/component: iframe-preview` and a port named `iframe`. They will be auto-discovered.

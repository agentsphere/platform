# Project Instructions

This project runs on the Platform DevOps system.

## Key Files

- `.platform.yaml` — CI/CD pipeline definition (kaniko build)
- `Dockerfile` — Container image build
- `Dockerfile.dev` — Dev/agent image build (customise agent environment)
- `deploy/production.yaml` — K8s deployment manifests (minijinja templates)

## Pipeline

Pushing triggers the pipeline defined in `.platform.yaml`.
Available env vars in pipeline steps: `$REGISTRY`, `$PROJECT`, `$COMMIT_SHA`, `$COMMIT_BRANCH`

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

### Registry Pull Secret

The platform automatically creates a `platform-registry-pull` imagePullSecret in each project namespace. Always include it in your deploy manifests:

```yaml
spec:
  imagePullSecrets:
    - name: platform-registry-pull
```

This secret is refreshed on every deploy — do not modify or delete it.

## Deploying Dependencies (Databases, Caches, etc.)

The agent pod runs inside Kubernetes with a service account that has deploy access to the project namespace. `kubectl` is not pre-installed — install it first:

```bash
curl -sLO "https://dl.k8s.io/release/$(curl -sL https://dl.k8s.io/release/stable.txt)/bin/linux/$(uname -m | sed 's/aarch64/arm64/' | sed 's/x86_64/amd64/')/kubectl"
chmod +x kubectl && mv kubectl /usr/local/bin/
```

Use the in-cluster service account (auto-mounted at `/var/run/secrets/kubernetes.io/serviceaccount/`):

```bash
export KUBERNETES_SERVICE_HOST=${KUBERNETES_SERVICE_HOST}
export KUBERNETES_SERVICE_PORT=${KUBERNETES_SERVICE_PORT}
kubectl get pods  # should work with in-cluster config
```

### Example: PostgreSQL

```bash
cat <<'EOF' | kubectl apply -f -
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
EOF
```

Connection string: `postgresql://app:password@postgres:5432/app`

The same pattern works for Redis, MinIO, or any other dependency. Deploy to the current namespace and reference by service name.

## Application Requirements

- App must listen on port 8080
- Include a `GET /healthz` endpoint returning `{"status": "ok"}`
- Configure OpenTelemetry SDK reading `OTEL_EXPORTER_OTLP_ENDPOINT` and `OTEL_SERVICE_NAME` env vars

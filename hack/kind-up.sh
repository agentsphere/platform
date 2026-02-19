#!/usr/bin/env bash
set -euo pipefail

CLUSTER_NAME="platform"

# Create cluster if it doesn't exist
if ! kind get clusters 2>/dev/null | grep -q "^${CLUSTER_NAME}$"; then
  kind create cluster --name "$CLUSTER_NAME" --config hack/kind-config.yaml
fi

# Export kubeconfig
kind get kubeconfig --name "$CLUSTER_NAME" > "${HOME}/.kube/kind-platform"
export KUBECONFIG="${HOME}/.kube/kind-platform"

# Install CNPG operator
helm repo add cnpg https://cloudnative-pg.github.io/charts --force-update
helm upgrade --install cnpg cnpg/cloudnative-pg -n cnpg-system --create-namespace --wait

# Create platform namespace
kubectl create namespace platform --dry-run=client -o yaml | kubectl apply -f -

# Postgres cluster (single instance, ephemeral for dev)
kubectl apply -n platform -f - <<'EOF'
apiVersion: postgresql.cnpg.io/v1
kind: Cluster
metadata:
  name: platform-db
spec:
  instances: 1
  storage:
    size: 1Gi
  bootstrap:
    initdb:
      database: platform_dev
      owner: platform
      secret:
        name: platform-db-creds
---
apiVersion: v1
kind: Secret
metadata:
  name: platform-db-creds
type: kubernetes.io/basic-auth
stringData:
  username: platform
  password: dev
---
apiVersion: v1
kind: Service
metadata:
  name: platform-db-external
spec:
  type: NodePort
  selector:
    cnpg.io/cluster: platform-db
    role: primary
  ports:
    - port: 5432
      targetPort: 5432
      nodePort: 30432
EOF

# Valkey (standalone, minimal)
kubectl apply -n platform -f - <<'EOF'
apiVersion: apps/v1
kind: Deployment
metadata:
  name: valkey
spec:
  replicas: 1
  selector:
    matchLabels:
      app: valkey
  template:
    metadata:
      labels:
        app: valkey
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
---
apiVersion: v1
kind: Service
metadata:
  name: valkey-external
spec:
  type: NodePort
  selector:
    app: valkey
  ports:
    - port: 6379
      targetPort: 6379
      nodePort: 30379
EOF

# MinIO (standalone, ephemeral for dev)
kubectl apply -n platform -f - <<'EOF'
apiVersion: apps/v1
kind: Deployment
metadata:
  name: minio
spec:
  replicas: 1
  selector:
    matchLabels:
      app: minio
  template:
    metadata:
      labels:
        app: minio
    spec:
      containers:
        - name: minio
          image: minio/minio:latest
          args: ["server", "/data", "--console-address", ":9001"]
          env:
            - name: MINIO_ROOT_USER
              value: platform
            - name: MINIO_ROOT_PASSWORD
              value: devdevdev
          ports:
            - containerPort: 9000
            - containerPort: 9001
          resources:
            requests:
              cpu: 50m
              memory: 128Mi
---
apiVersion: v1
kind: Service
metadata:
  name: minio-external
spec:
  type: NodePort
  selector:
    app: minio
  ports:
    - name: api
      port: 9000
      targetPort: 9000
      nodePort: 30900
    - name: console
      port: 9001
      targetPort: 9001
      nodePort: 30901
EOF

# OTel Collector (minimal, forwards OTLP to platform ingest endpoint)
kubectl apply -n platform -f - <<'EOF'
apiVersion: v1
kind: ConfigMap
metadata:
  name: otel-collector-config
data:
  config.yaml: |
    receivers:
      otlp:
        protocols:
          grpc:
            endpoint: 0.0.0.0:4317
          http:
            endpoint: 0.0.0.0:4318
    exporters:
      otlphttp:
        endpoint: http://platform:8080/v1
        tls:
          insecure: true
      debug:
        verbosity: basic
    service:
      pipelines:
        traces:
          receivers: [otlp]
          exporters: [otlphttp, debug]
        logs:
          receivers: [otlp]
          exporters: [otlphttp, debug]
        metrics:
          receivers: [otlp]
          exporters: [otlphttp, debug]
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: otel-collector
spec:
  replicas: 1
  selector:
    matchLabels:
      app: otel-collector
  template:
    metadata:
      labels:
        app: otel-collector
    spec:
      containers:
        - name: collector
          image: otel/opentelemetry-collector-contrib:latest
          args: ["--config=/etc/otel/config.yaml"]
          ports:
            - containerPort: 4317
            - containerPort: 4318
          volumeMounts:
            - name: config
              mountPath: /etc/otel
          resources:
            requests:
              cpu: 50m
              memory: 64Mi
      volumes:
        - name: config
          configMap:
            name: otel-collector-config
---
apiVersion: v1
kind: Service
metadata:
  name: otel-collector
spec:
  selector:
    app: otel-collector
  ports:
    - name: grpc
      port: 4317
      targetPort: 4317
    - name: http
      port: 4318
      targetPort: 4318
EOF

echo "Waiting for Postgres to be ready..."
kubectl wait --for=condition=Ready cluster/platform-db -n platform --timeout=120s

echo ""
echo "Dev cluster ready."
echo "  Postgres: localhost:5432 (platform/dev)"
echo "  Valkey:   localhost:6379"
echo "  MinIO:    localhost:9000 (S3 API), localhost:9001 (console)"
echo "            credentials: platform / devdevdev"
echo "  export KUBECONFIG=${HOME}/.kube/kind-platform"

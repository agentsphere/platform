# Plan: Platform-Native Service Mesh with Process-Wrapper Proxy

## Context

The platform currently has no encryption between pods (plain HTTP everywhere), no automatic trace context propagation, and relies on separate OTel collector sidecars for infrastructure metrics. Observability is opt-in: apps must integrate an OTLP SDK to emit traces/logs, and orphaned logs (no trace context) are common.

This plan introduces a **platform-native service mesh** built on a single component: `platform-proxy`, a lightweight Rust binary that wraps every process as PID 1. It replaces the OTel collector sidecars, provides mTLS between all services, auto-generates traces for every request, and captures stdout/stderr as correlated log entries — all without any app instrumentation.

The platform binary gains a `src/mesh/` module that acts as the **control plane**: a self-signed Certificate Authority that issues short-lived X.509 certificates to every proxy instance.

### Current State

- **Transport**: Plain HTTP between all pods. Security via NetworkPolicy + Bearer token auth.
- **OTel sidecars**: 3 separate `otel/opentelemetry-collector-contrib:0.121.0` containers (pg, valkey, minio) — metrics only, no logs or traces.
- **Trace context**: Apps opt-in via OTLP SDK. Platform doesn't propagate W3C `traceparent`.
- **Infrastructure**: Postgres, Valkey, MinIO each deployed as raw Pods with OTel sidecar containers in `hack/test-manifests/*.yaml`.
- **Binary distribution**: `tmp/platform-e2e/{worktree}/` used for seed images, agent-runner, mock CLI. Kind mounts `/tmp/platform-e2e` into the cluster node.

### What Changes

1. OTel collector sidecars → removed. Proxy handles metrics scraping + log capture + trace generation.
2. Multi-container pods → single container. Proxy is PID 1, wraps the service as a child process.
3. Plain HTTP → mTLS between proxy instances. Platform CA issues certs.
4. No trace context → every request automatically gets a W3C `traceparent` span.
5. Opt-in stdout parsing → proxy captures all stdout/stderr, correlates to traces.

## Design Principles

- **Single container per pod** — proxy wraps the app as a child process. No sidecars, no K8s API for log capture, no shared volumes for log files.
- **Zero app changes for baseline observability** — traces from the proxy, logs from stdout capture, RED metrics from request counting. Apps can enrich by adding `trace_id` to JSON stdout.
- **Platform is the CA** — no cert-manager, no external PKI. Reuses the existing secrets engine for root key storage.
- **Prebuilt binary for dev/test** — `platform-proxy` cross-compiled and placed in `tmp/platform-e2e/{worktree}/proxy/`, mounted into Kind via the existing hostPath.
- **Incremental rollout** — infra services first (pg, valkey, minio), then pipeline/agent pods, then deployed apps.

---

## PR 1: Mesh CA Module + Cert Issuance API

Server-side control plane: self-signed CA, SPIFFE identity model, cert issuance endpoints.

- [ ] Types & errors defined
- [ ] Migration applied
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### New Dependencies (Cargo.toml)

```toml
# X.509 certificate generation
rcgen = { version = "0.13", features = ["x509-parser"] }
x509-parser = "0.16"
# Time for cert validity
time = "0.3"
```

`rcgen` is pure Rust, uses `ring` internally (already our crypto provider via rustls). No new C deps.

### Migration: `20260401010001_mesh_certificates`

**Up:**
```sql
-- Root CA metadata (the actual key material lives in secrets engine, not DB)
CREATE TABLE mesh_ca (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    serial      BIGINT NOT NULL DEFAULT 1,
    not_before  TIMESTAMPTZ NOT NULL,
    not_after   TIMESTAMPTZ NOT NULL,
    fingerprint TEXT NOT NULL,     -- SHA-256 of DER-encoded root cert
    is_active   BOOLEAN NOT NULL DEFAULT true,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Issued leaf certificates (for audit + revocation)
CREATE TABLE mesh_certs (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    ca_id           UUID NOT NULL REFERENCES mesh_ca(id),
    serial          BIGINT NOT NULL,
    spiffe_id       TEXT NOT NULL,      -- spiffe://platform/{ns}/{svc}
    namespace       TEXT NOT NULL,
    service         TEXT NOT NULL,
    not_before      TIMESTAMPTZ NOT NULL,
    not_after       TIMESTAMPTZ NOT NULL,
    fingerprint     TEXT NOT NULL,
    revoked         BOOLEAN NOT NULL DEFAULT false,
    revoked_at      TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_mesh_certs_spiffe ON mesh_certs(spiffe_id);
CREATE INDEX idx_mesh_certs_expiry ON mesh_certs(not_after) WHERE NOT revoked;
```

**Down:**
```sql
DROP TABLE IF EXISTS mesh_certs;
DROP TABLE IF EXISTS mesh_ca;
```

### Code Changes

| File | Change |
|---|---|
| `Cargo.toml` | Add `rcgen`, `x509-parser`, `time` dependencies |
| `src/mesh/mod.rs` | New module: re-exports, `MeshError` enum, `router()` |
| `src/mesh/ca.rs` | `MeshCa` struct: init/load root CA, issue leaf certs, rotate root |
| `src/mesh/identity.rs` | `SpiffeId` newtype, `spiffe://platform/{ns}/{svc}` format |
| `src/mesh/error.rs` | `MeshError` enum with `From<MeshError> for ApiError` |
| `src/api/mesh.rs` | Endpoints: `POST /api/mesh/certs/issue`, `POST /api/mesh/certs/renew`, `GET /api/mesh/ca/trust-bundle` |
| `src/config.rs` | New: `mesh_ca_cert_ttl` (default 1h), `mesh_ca_root_ttl` (default 365d), `mesh_enabled` (default false) |
| `src/store/mod.rs` | Add `mesh_ca: Option<Arc<MeshCa>>` to `AppState` |
| `src/main.rs` | Initialize `MeshCa` on startup (if `mesh_enabled`), add mesh routes |
| `src/api/mod.rs` | Merge mesh router |
| `tests/helpers/mod.rs` | Add `mesh_ca: None` to `test_state()` |
| `tests/e2e_helpers/mod.rs` | Add `mesh_ca: None` to `e2e_state()` |

### `src/mesh/ca.rs` — Core CA Logic

```rust
pub struct MeshCa {
    root_key_pem: String,       // Encrypted in secrets engine
    root_cert_pem: String,      // Public, distributed as trust bundle
    root_cert_der: Vec<u8>,
    ca_id: Uuid,
    serial: Arc<AtomicI64>,     // Monotonic serial counter
}

impl MeshCa {
    /// Initialize or load the root CA from DB + secrets engine.
    /// On first boot: generates self-signed root, stores encrypted key via secrets engine.
    /// On subsequent boots: loads and decrypts.
    pub async fn init(pool: &PgPool, config: &Config) -> Result<Self, MeshError>;

    /// Issue a leaf certificate for a SPIFFE identity.
    /// Cert lifetime: config.mesh_ca_cert_ttl (default 1h).
    /// Returns: (cert_pem, key_pem, ca_cert_pem)
    pub async fn issue_cert(
        &self,
        pool: &PgPool,
        spiffe_id: &SpiffeId,
        namespace: &str,
        service: &str,
    ) -> Result<CertBundle, MeshError>;

    /// Trust bundle: PEM of the active root CA cert(s).
    /// During rotation, includes both old and new root.
    pub fn trust_bundle(&self) -> &str;
}

pub struct CertBundle {
    pub cert_pem: String,
    pub key_pem: String,
    pub ca_pem: String,
    pub not_after: DateTime<Utc>,
}
```

Key generation uses `rcgen::KeyPair::generate()` with `PKCS_ECDSA_P256_SHA256` (fast, small certs, same curve as existing rustls setup).

### `src/mesh/identity.rs` — SPIFFE IDs

```rust
/// SPIFFE identity: spiffe://platform/{namespace}/{service}
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SpiffeId {
    pub namespace: String,
    pub service: String,
}

impl SpiffeId {
    pub fn new(namespace: &str, service: &str) -> Result<Self, MeshError> {
        // Validate: no slashes, no dots, alphanumeric + hyphen, 1-63 chars
        validation::check_name(namespace)?;
        validation::check_name(service)?;
        Ok(Self { namespace: namespace.into(), service: service.into() })
    }

    pub fn uri(&self) -> String {
        format!("spiffe://platform/{}/{}", self.namespace, self.service)
    }
}
```

### `src/api/mesh.rs` — Cert Issuance Endpoints

```
POST /api/mesh/certs/issue
  Auth: Bearer token with ObserveWrite or AgentSession scope
  Body: { namespace: String, service: String }
  Returns: { cert_pem, key_pem, ca_pem, not_after, spiffe_id }
  Rate limit: 100/min per user (cert issuance)

POST /api/mesh/certs/renew
  Auth: Bearer token
  Body: { namespace: String, service: String }
  Returns: same as issue
  (Identical to issue — stateless renewal. Old cert continues until expiry.)

GET /api/mesh/ca/trust-bundle
  Auth: Bearer token (any authenticated user)
  Returns: PEM text of active root CA cert(s)
```

The proxy calls `POST /api/mesh/certs/issue` on startup (using its existing `PLATFORM_API_TOKEN`) and `POST /api/mesh/certs/renew` at 50% cert lifetime.

### Root CA Key Storage

The root CA private key is encrypted using the existing secrets engine (`src/secrets/engine.rs`, AES-256-GCM with `PLATFORM_MASTER_KEY`) and stored in the `secrets` table with a well-known name `mesh:ca:root:key`. This reuses the existing encryption infrastructure — no new key management.

### Test Outline — PR 1

**New behaviors to test:**
- CA initialization (first boot) generates valid root cert — unit
- CA load (subsequent boot) decrypts and validates — unit
- Leaf cert issuance produces valid X.509 with correct SPIFFE SAN — unit
- Leaf cert validates against root CA trust bundle — unit
- Serial numbers are monotonic — unit
- SPIFFE ID validation rejects bad input — unit
- `/api/mesh/certs/issue` returns cert bundle — integration
- `/api/mesh/certs/issue` requires auth — integration
- `/api/mesh/ca/trust-bundle` returns PEM — integration

**Error paths to test:**
- Missing master key → MeshError — unit
- Invalid SPIFFE ID → 400 — integration
- Unauthenticated request → 401 — integration

**Existing tests affected:**
- `tests/helpers/mod.rs` — add `mesh_ca: None` field
- `tests/e2e_helpers/mod.rs` — add `mesh_ca: None` field

**Estimated test count:** ~8 unit + 4 integration

### Verification
- `just test-unit` passes with new mesh module
- `just test-integration` passes with mesh_ca=None default
- Manual: `curl -H "Authorization: Bearer $TOKEN" http://localhost:$PORT/api/mesh/certs/issue -d '{"namespace":"default","service":"test"}'` returns valid PEM

---

## PR 2: `platform-proxy` Binary — Process Wrapper + Log Capture + OTLP Export

The standalone proxy binary. Separate `[[bin]]` target in the same repo. Handles: process wrapping (PID 1), signal forwarding, stdout/stderr capture, log parsing, trace generation, RED metrics, OTLP protobuf export, health endpoint.

**This PR does NOT include mTLS** — that comes in PR 3. This PR focuses on the process wrapper + observability pipeline, which is independently valuable and testable.

- [ ] Types & errors defined
- [ ] Migration applied
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### Binary Target (Cargo.toml)

```toml
[[bin]]
name = "platform-proxy"
path = "src/proxy/main.rs"
```

The proxy shares the main crate's dependencies (prost, reqwest, tokio, tracing, etc.) but only compiles the modules it needs. We use feature-gating or cfg to keep it lean:

```toml
[features]
default = ["platform"]
platform = []  # main binary features
proxy = []     # proxy-only features (lighter)
```

Actually, simpler: the proxy binary lives in `src/proxy/main.rs` and imports types from the main crate. At ~3K LOC for the proxy vs ~72K for the platform, compilation overhead is minimal. The proxy binary will be ~5-8MB stripped.

### Code Changes

| File | Change |
|---|---|
| `Cargo.toml` | Add `[[bin]]` for `platform-proxy`, add `nix` dep (signal handling) |
| `src/proxy/main.rs` | Entry point: parse args, spawn child, start health server, run log/trace/metrics pipeline |
| `src/proxy/child.rs` | Child process management: spawn, signal forwarding, zombie reaping, exit handling |
| `src/proxy/logs.rs` | Stdout/stderr capture, line parsing (JSON detection, trace_id extraction), correlation |
| `src/proxy/traces.rs` | Span construction for HTTP requests, W3C traceparent create/propagate |
| `src/proxy/metrics.rs` | RED metrics (request count/duration/status), process stats (/proc RSS/CPU) |
| `src/proxy/otlp.rs` | OTLP protobuf serialization + batch HTTP export to platform `/v1/{traces,logs,metrics}` |
| `src/proxy/health.rs` | HTTP health endpoint on `:15020` — `/healthz` (proxy alive), `/readyz` (child port reachable) |
| `src/proxy/config.rs` | Proxy configuration from env vars |
| `src/proxy/mod.rs` | Module root |
| `hack/build-proxy.sh` | Cross-compile proxy binary for linux/amd64+arm64, place in `tmp/platform-e2e/{worktree}/proxy/` |

### New Dependency

```toml
nix = { version = "0.29", features = ["signal", "process"] }
```

For `kill()`, `waitpid()`, signal handling. Pure Rust (libc wrapper), no new C deps.

### `src/proxy/main.rs` — Entry Point

```rust
/// platform-proxy — process wrapper with observability
///
/// Usage:
///   platform-proxy --wrap -- postgres -c max_connections=300
///   platform-proxy --wrap --app-port 8080 -- my-app --listen :8080
///   platform-proxy --wrap --tcp-ports 5432,6379 -- my-service
///
/// Env vars:
///   PLATFORM_API_URL          — platform HTTP endpoint (for OTLP export)
///   PLATFORM_API_TOKEN        — bearer token for OTLP auth
///   PLATFORM_PROJECT_ID       — UUID, set as platform.project_id resource attribute
///   PLATFORM_SERVICE_NAME     — service name (default: binary name of wrapped process)
///   PLATFORM_SESSION_ID       — optional session UUID
///   PROXY_HEALTH_PORT         — health endpoint port (default: 15020)
///   PROXY_APP_PORT            — app HTTP port for readiness check (default: auto-detect)
///   PROXY_LOG_LEVEL           — proxy's own log level (default: info)
///   PROXY_METRICS_INTERVAL    — process metrics scrape interval (default: 15s)
///   PROXY_FLUSH_INTERVAL      — OTLP batch flush interval (default: 5s)
///   PROXY_BATCH_SIZE          — max spans/logs per OTLP batch (default: 500)
#[tokio::main]
async fn main() {
    // 1. Parse CLI args
    // 2. Spawn child process, capture stdout/stderr pipes
    // 3. Start health HTTP server on PROXY_HEALTH_PORT
    // 4. Start log pipeline (reads pipes → parses → correlates → buffers)
    // 5. Start metrics collector (process stats every PROXY_METRICS_INTERVAL)
    // 6. Start OTLP exporter (flushes batches every PROXY_FLUSH_INTERVAL)
    // 7. Wait for child exit or signal
    // 8. On SIGTERM: forward to child, drain buffers, flush final OTLP batch, exit
}
```

### `src/proxy/child.rs` — Process Management

```rust
pub struct ChildProcess {
    pid: Pid,
    stdout: tokio::io::BufReader<tokio::process::ChildStdout>,
    stderr: tokio::io::BufReader<tokio::process::ChildStderr>,
}

impl ChildProcess {
    /// Spawn the child process, return handle + stdout/stderr readers.
    pub fn spawn(command: &str, args: &[String]) -> Result<Self, ProxyError>;

    /// Forward a signal to the child process.
    pub fn signal(&self, sig: Signal) -> Result<(), ProxyError> {
        nix::sys::signal::kill(self.pid, sig)?;
        Ok(())
    }
}

/// Reap zombie child processes (we're PID 1 in the container).
/// Runs as a background task, calls waitpid(-1, WNOHANG) every second.
pub async fn reap_zombies(shutdown: watch::Receiver<()>);

/// Main signal handler. On SIGTERM/SIGINT:
/// 1. Forward signal to child
/// 2. Set shutdown flag (stops health server, flushes OTLP)
/// 3. Wait up to 25s for child to exit (K8s default grace period is 30s)
/// 4. If child hasn't exited, SIGKILL it
/// 5. Exit with child's exit code
pub async fn signal_handler(child: &ChildProcess, shutdown_tx: watch::Sender<()>);
```

### `src/proxy/logs.rs` — Log Pipeline

```rust
/// Read lines from stdout/stderr, parse, correlate, buffer for OTLP export.
pub async fn run_log_pipeline(
    stdout: BufReader<ChildStdout>,
    stderr: BufReader<ChildStderr>,
    log_tx: mpsc::Sender<LogRecord>,   // → OTLP exporter
    active_spans: Arc<RwLock<ActiveSpans>>,  // for timestamp correlation
);

/// Parsed log line — either structured JSON or plain text.
pub struct ParsedLog {
    pub timestamp: DateTime<Utc>,
    pub level: Severity,
    pub message: String,
    pub trace_id: Option<String>,      // from JSON: "trace_id" field
    pub span_id: Option<String>,       // from JSON: "span_id" field
    pub trace_name: Option<String>,    // from JSON: "trace_name" field
    pub props: Option<serde_json::Value>, // from JSON: "props" field
    pub is_stderr: bool,
}

/// JSON log detection — tries serde_json::from_str first.
/// Recognizes well-known fields: msg/message, level/severity, trace_id,
/// span_id, trace_name, props/properties/attributes.
fn parse_line(line: &str, is_stderr: bool) -> ParsedLog;

/// Correlation: assign trace_id to logs that don't have one.
///
/// Priority:
/// 1. Log has explicit trace_id → use it
/// 2. Exactly one active inbound span → use its trace_id
/// 3. Multiple active spans → use longest-running (heuristic)
/// 4. No active spans → use pod lifecycle trace
fn correlate(log: &mut ParsedLog, active_spans: &ActiveSpans);
```

The log pipeline reads both stdout and stderr concurrently (via `tokio::select!` on both `BufReader::read_line()` calls). stderr logs get `level: WARN` by default (unless JSON with explicit level).

### `src/proxy/traces.rs` — Span Generation

```rust
/// Track active inbound request spans for log correlation.
pub struct ActiveSpans {
    spans: HashMap<String, ActiveSpan>,  // span_id → span
}

pub struct ActiveSpan {
    pub trace_id: String,
    pub span_id: String,
    pub started_at: Instant,
}

/// Generate a new trace_id (16 random bytes → 32 hex chars).
pub fn new_trace_id() -> String;

/// Generate a new span_id (8 random bytes → 16 hex chars).
pub fn new_span_id() -> String;

/// Parse W3C traceparent header: "00-{trace_id}-{parent_span_id}-{flags}"
pub fn parse_traceparent(header: &str) -> Option<(String, String, u8)>;

/// Build W3C traceparent header from trace_id + span_id.
pub fn build_traceparent(trace_id: &str, span_id: &str) -> String;

/// Build a completed span for OTLP export.
pub fn build_server_span(
    trace_id: &str,
    span_id: &str,
    parent_span_id: Option<&str>,
    name: &str,             // e.g., "POST /api/upload"
    service: &str,
    started_at: DateTime<Utc>,
    duration_ms: i32,
    status_code: u16,       // HTTP status
    attributes: Vec<(String, String)>,
) -> SpanRecord;
```

For PR 2 (no mTLS yet), trace generation works for **app-initiated traces** via the stdout `trace_id`/`trace_name` fields. The proxy also creates a **pod lifecycle trace** on startup (root span that lives for the pod's lifetime) for orphaned logs.

In PR 3 (mTLS), the proxy's TCP listener will create per-request SERVER spans with full HTTP attributes.

### `src/proxy/otlp.rs` — OTLP Export

```rust
/// Batched OTLP exporter. Collects spans, logs, metrics and flushes
/// to the platform's /v1/{traces,logs,metrics} endpoints.
pub struct OtlpExporter {
    client: reqwest::Client,
    endpoint: String,       // PLATFORM_API_URL
    token: String,          // PLATFORM_API_TOKEN
    project_id: Option<String>,  // PLATFORM_PROJECT_ID
    service_name: String,   // PLATFORM_SERVICE_NAME
    session_id: Option<String>,  // PLATFORM_SESSION_ID
}

impl OtlpExporter {
    /// Flush a batch of spans as ExportTraceServiceRequest protobuf.
    pub async fn flush_spans(&self, spans: Vec<SpanRecord>) -> Result<(), ProxyError>;

    /// Flush a batch of logs as ExportLogsServiceRequest protobuf.
    pub async fn flush_logs(&self, logs: Vec<LogRecord>) -> Result<(), ProxyError>;

    /// Flush metrics as ExportMetricsServiceRequest protobuf.
    pub async fn flush_metrics(&self, metrics: Vec<MetricRecord>) -> Result<(), ProxyError>;
}

/// Background flush loop. Collects from mpsc channels, flushes every
/// PROXY_FLUSH_INTERVAL or when batch reaches PROXY_BATCH_SIZE.
pub async fn run_exporter(
    exporter: OtlpExporter,
    span_rx: mpsc::Receiver<SpanRecord>,
    log_rx: mpsc::Receiver<LogRecord>,
    metric_rx: mpsc::Receiver<MetricRecord>,
    shutdown: watch::Receiver<()>,
);
```

Uses `prost` (already in deps) to serialize protobuf. Reuses the exact proto types from `src/observe/proto.rs` — the proxy imports them from the main crate.

### `src/proxy/metrics.rs` — RED + Process Metrics

```rust
/// Collect process metrics from /proc/{child_pid}/ every interval.
pub async fn collect_process_metrics(
    child_pid: u32,
    metric_tx: mpsc::Sender<MetricRecord>,
    interval: Duration,
    shutdown: watch::Receiver<()>,
);

/// Metrics collected:
/// - process.memory.rss (gauge, bytes) — from /proc/{pid}/statm
/// - process.cpu.user (counter, seconds) — from /proc/{pid}/stat
/// - process.cpu.system (counter, seconds) — from /proc/{pid}/stat
/// - process.open_fds (gauge, count) — from /proc/{pid}/fd/ count
/// - process.threads (gauge, count) — from /proc/{pid}/stat
```

RED metrics (request rate, error rate, duration) are generated in PR 3 when the mTLS listener creates per-request spans. For PR 2, we only have process metrics + stdout-derived traces.

### `src/proxy/health.rs` — Health Endpoint

```rust
/// Lightweight HTTP server on PROXY_HEALTH_PORT (default 15020).
/// No TLS, no auth — only kubelet should reach this (via pod-local networking).
///
/// GET /healthz → 200 if proxy is running and child PID exists
/// GET /readyz  → 200 if child port (PROXY_APP_PORT) accepts TCP connection
/// GET /metrics → Prometheus exposition format (optional, for scraping)
pub async fn run_health_server(
    port: u16,
    child_pid: u32,
    app_port: Option<u16>,
    shutdown: watch::Receiver<()>,
);
```

### `hack/build-proxy.sh` — Cross-Compilation

```bash
#!/usr/bin/env bash
# Build platform-proxy for linux/amd64 and linux/arm64.
# Places binaries in /tmp/platform-e2e/{worktree}/proxy/
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
WORKTREE="$(bash "${SCRIPT_DIR}/detect-worktree.sh")"
PROXY_DIR="/tmp/platform-e2e/${WORKTREE}/proxy"
mkdir -p "${PROXY_DIR}"

# Cross-compile for amd64 (Kind runs amd64 by default on macOS via Rosetta)
echo "==> Building platform-proxy (linux/amd64)"
cross build --bin platform-proxy --release --target x86_64-unknown-linux-musl
cp target/x86_64-unknown-linux-musl/release/platform-proxy "${PROXY_DIR}/amd64"

# Cross-compile for arm64 (native on Apple Silicon Kind)
echo "==> Building platform-proxy (linux/arm64)"
cross build --bin platform-proxy --release --target aarch64-unknown-linux-musl
cp target/aarch64-unknown-linux-musl/release/platform-proxy "${PROXY_DIR}/arm64"

echo "  Proxy binaries ready: ${PROXY_DIR}/{amd64,arm64}"
```

Uses `cross` (same approach as agent-runner in `hack/build-agent-images.sh`). Static linking via musl — no glibc dependency, runs on Alpine containers.

### Test Outline — PR 2

**New behaviors to test:**
- Child process spawns correctly and stdout/stderr are captured — unit
- Signal forwarding (SIGTERM → child) — unit
- JSON log parsing extracts trace_id, level, message, props — unit
- Plain text log defaults to INFO — unit
- stderr defaults to WARN — unit
- Log correlation: explicit trace_id used first — unit
- Log correlation: single active span matched — unit
- OTLP protobuf serialization matches expected format — unit
- Health endpoint /healthz returns 200 — unit
- Health endpoint /readyz returns 503 when child port unreachable — unit
- W3C traceparent parsing/generation round-trips — unit
- Pod lifecycle trace created on startup — unit

**Error paths to test:**
- Child exits with non-zero → proxy exits with same code — unit
- OTLP endpoint unreachable → logs warning, doesn't crash — unit
- Malformed JSON in stdout → treated as plain text — unit

**Existing tests affected:**
- None (new binary, separate entry point)

**Estimated test count:** ~15 unit + 0 integration (integration tested in PR 4)

### Verification
- `cargo build --bin platform-proxy` succeeds
- `platform-proxy --wrap -- echo "hello"` captures "hello" as a log entry
- `platform-proxy --wrap -- sh -c 'echo {"trace_id":"abc","msg":"test"}'` parses JSON
- Health endpoint responds on `:15020/healthz`

---

## PR 3: mTLS Listener + HTTP Proxy + Trace Enrichment

Add mTLS termination/origination to the proxy. Every inbound request gets a SERVER span. Outbound connections originate mTLS with the proxy's cert.

- [ ] Types & errors defined
- [ ] Migration applied
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### Code Changes

| File | Change |
|---|---|
| `src/proxy/tls.rs` | New: cert bootstrap (call platform API), cert renewal loop, TLS config builder |
| `src/proxy/inbound.rs` | New: mTLS TCP listener, TLS termination, HTTP parsing, traceparent injection, forward to child |
| `src/proxy/outbound.rs` | New: localhost TCP listener, mTLS origination, traceparent propagation to upstream |
| `src/proxy/main.rs` | Add mTLS listener startup, outbound proxy startup, cert bootstrap on init |
| `src/proxy/traces.rs` | Add `build_server_span()` for HTTP requests, RED metric counters |
| `src/proxy/metrics.rs` | Add RED metrics: `http.server.request.count`, `http.server.request.duration`, `http.server.response.size` |
| `src/proxy/config.rs` | Add: `PROXY_TLS_PORT` (default 8443), `PROXY_OUTBOUND_PORT` (default 15001) |

### `src/proxy/tls.rs` — Cert Bootstrap + Renewal

```rust
pub struct ProxyCerts {
    cert_pem: String,
    key_pem: String,
    ca_pem: String,
    not_after: DateTime<Utc>,
}

/// Fetch initial cert from platform CA.
/// POST {PLATFORM_API_URL}/api/mesh/certs/issue
/// Auth: Bearer {PLATFORM_API_TOKEN}
pub async fn bootstrap_cert(config: &ProxyConfig) -> Result<ProxyCerts, ProxyError>;

/// Background task: renew cert at 50% lifetime.
/// On failure: retry with exponential backoff (1s, 2s, 4s, ..., 30s max).
/// On persistent failure: log error, continue with existing cert until expiry.
pub async fn cert_renewal_loop(
    config: ProxyConfig,
    certs: Arc<ArcSwap<ProxyCerts>>,  // hot-swappable certs
    shutdown: watch::Receiver<()>,
);

/// Build rustls ServerConfig from current certs.
/// Called on each new connection (uses ArcSwap for zero-downtime rotation).
pub fn build_tls_acceptor(certs: &ProxyCerts) -> Result<TlsAcceptor, ProxyError>;

/// Build rustls ClientConfig for outbound mTLS.
pub fn build_tls_connector(certs: &ProxyCerts) -> Result<TlsConnector, ProxyError>;
```

Uses `arc-swap` crate for lock-free cert rotation. New dep:

```toml
arc-swap = "1"
```

### `src/proxy/inbound.rs` — mTLS Listener

```rust
/// Listen on PROXY_TLS_PORT (8443), terminate mTLS, forward to localhost:APP_PORT.
///
/// For each connection:
/// 1. TLS handshake (verify client cert against CA trust bundle)
/// 2. Extract caller's SPIFFE ID from client cert SAN
/// 3. Parse HTTP request (method, path, headers)
/// 4. Extract or generate W3C traceparent
/// 5. Create SERVER span (started_at = now)
/// 6. Forward request to localhost:APP_PORT with traceparent header
/// 7. Read response
/// 8. Complete SERVER span (duration, status_code)
/// 9. Send span to OTLP exporter
/// 10. Return response to caller (over mTLS)
pub async fn run_inbound_listener(
    tls_port: u16,
    app_port: u16,
    certs: Arc<ArcSwap<ProxyCerts>>,
    span_tx: mpsc::Sender<SpanRecord>,
    active_spans: Arc<RwLock<ActiveSpans>>,
    shutdown: watch::Receiver<()>,
);
```

The inbound listener registers each active request in `ActiveSpans` so the log pipeline can correlate stdout logs to the correct trace.

### `src/proxy/outbound.rs` — mTLS Origination

```rust
/// Listen on localhost:PROXY_OUTBOUND_PORT (15001).
/// Apps connect here instead of directly to upstream services.
///
/// For each connection:
/// 1. Read HTTP CONNECT or plain HTTP request
/// 2. Resolve destination (from Host header or CONNECT target)
/// 3. Establish mTLS connection to destination's proxy (port 8443)
/// 4. Create CLIENT span
/// 5. Propagate traceparent from incoming request
/// 6. Forward request, read response
/// 7. Complete CLIENT span
/// 8. Return response to app
pub async fn run_outbound_proxy(
    listen_port: u16,
    certs: Arc<ArcSwap<ProxyCerts>>,
    span_tx: mpsc::Sender<SpanRecord>,
    shutdown: watch::Receiver<()>,
);
```

### TCP Proxy Mode (for non-HTTP protocols: Postgres, Redis)

For infra services (postgres, valkey), the proxy can't parse HTTP. It operates in **TCP proxy mode**:

```rust
/// TCP proxy mode: wrap a raw TCP stream in mTLS.
/// No HTTP parsing, no per-request spans.
/// Creates one CONNECTION span per TCP connection (start/end/duration/bytes).
///
/// Activated by: PROXY_TCP_PORTS=5432,6379
pub async fn run_tcp_inbound(
    tls_port: u16,        // external mTLS port
    upstream_port: u16,   // localhost plain port
    certs: Arc<ArcSwap<ProxyCerts>>,
    span_tx: mpsc::Sender<SpanRecord>,
    shutdown: watch::Receiver<()>,
);
```

### Infra Service Metric Scraping

The current OTel sidecars scrape metrics from infra services (Postgres exporter, Redis receiver, MinIO Prometheus endpoint). The proxy replaces this with built-in scraping:

```rust
/// Built-in metrics scraping for known infrastructure services.
/// Activated by: PROXY_SCRAPE_URL=https://localhost:9000/minio/v2/metrics/cluster
///           or: PROXY_SCRAPE_TYPE=postgres (built-in SQL queries)
///           or: PROXY_SCRAPE_TYPE=redis (built-in INFO command parsing)
///
/// For Prometheus endpoints (MinIO): parse exposition format → OTLP metrics
/// For Postgres: run pg_stat queries → OTLP metrics
/// For Redis/Valkey: parse INFO output → OTLP metrics
pub async fn run_metrics_scraper(
    config: &ScraperConfig,
    metric_tx: mpsc::Sender<MetricRecord>,
    interval: Duration,
    shutdown: watch::Receiver<()>,
);
```

This is more work than the OTel collector (which already supports these receivers), but it eliminates the 128MB collector sidecar and keeps everything in one binary. The proxy only needs to implement 3 scraper types:

1. **Prometheus** — parse text exposition format (`minio_*` metrics). ~200 lines.
2. **Postgres** — `SELECT * FROM pg_stat_database`, `pg_stat_bgwriter`, etc. ~150 lines.
3. **Redis** — `INFO` command, parse key-value sections. ~150 lines.

### Test Outline — PR 3

**New behaviors to test:**
- Cert bootstrap calls platform API and stores certs — unit
- TLS acceptor built from valid certs — unit
- Inbound: mTLS handshake succeeds with valid client cert — integration
- Inbound: mTLS handshake rejects invalid cert — integration
- Inbound: traceparent injected into forwarded request — integration
- Inbound: SERVER span generated with correct attributes — integration
- Outbound: mTLS originated to upstream — integration
- TCP proxy: connection-level span generated — integration
- Prometheus scraper parses MinIO metrics — unit
- Postgres scraper produces expected metrics — integration (needs DB)
- Redis scraper produces expected metrics — integration (needs Valkey)

**Estimated test count:** ~6 unit + 8 integration

---

## PR 4: Infra Service Integration — Replace OTel Sidecars

Replace the OTel collector sidecars on postgres, valkey, minio with the process-wrapper proxy. Update pod manifests, deploy scripts, and test infrastructure.

- [ ] Types & errors defined
- [ ] Migration applied
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### Code Changes

| File | Change |
|---|---|
| `hack/test-manifests/postgres.yaml` | Remove otel-collector container, remove otel-config ConfigMap. Single container: proxy wraps postgres. Mount proxy binary from hostPath. |
| `hack/test-manifests/valkey.yaml` | Same: remove otel-collector, proxy wraps valkey. |
| `hack/test-manifests/minio.yaml` | Same: remove otel-collector, proxy wraps minio. Mount proxy + certs volume. |
| `hack/deploy-services.sh` | Remove `__OTEL_ENDPOINT__` sed replacement. Add proxy binary path and API URL env vars. |
| `hack/build-proxy.sh` | New script (from PR 2). Added to `hack/build-agent-images.sh` pipeline. |
| `hack/build-agent-images.sh` | Add call to `hack/build-proxy.sh` at the end. |
| `hack/test-in-cluster.sh` | Add `PLATFORM_PROXY_PATH` env var. Ensure proxy binary exists before deploying services. |

### New Pod Manifest: `hack/test-manifests/postgres.yaml`

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: postgres
  labels:
    app: postgres
spec:
  securityContext:
    fsGroup: 70
  initContainers:
    - name: gen-certs
      image: postgres:16-alpine
      command: ["sh", "-c"]
      args:
        - |
          set -e
          apk add --no-cache openssl
          openssl req -new -x509 -days 365 -nodes \
            -out /certs/server.crt -keyout /certs/server.key \
            -subj "/CN=postgres"
          chown 70:70 /certs/server.crt /certs/server.key
          chmod 600 /certs/server.key
      volumeMounts:
        - name: certs
          mountPath: /certs
  containers:
    - name: postgres
      image: postgres:16-alpine
      command: ["/proxy/platform-proxy"]
      args:
        - "--wrap"
        - "--tcp-ports=5432"
        - "--scrape-type=postgres"
        - "--"
        - "postgres"
        - "-c"
        - "max_connections=300"
        - "-c"
        - "ssl=on"
        - "-c"
        - "ssl_cert_file=/certs/server.crt"
        - "-c"
        - "ssl_key_file=/certs/server.key"
      env:
        - name: POSTGRES_USER
          value: platform
        - name: POSTGRES_PASSWORD
          value: dev
        - name: POSTGRES_DB
          value: platform_dev
        - name: PLATFORM_API_URL
          value: "__PLATFORM_API_URL__"
        - name: PLATFORM_API_TOKEN
          value: "plat_api_otel_system_dev_000000000000000000000000000000"
        - name: PLATFORM_SERVICE_NAME
          value: platform-postgres
        - name: PROXY_SCRAPE_TYPE
          value: postgres
        - name: PROXY_SCRAPE_POSTGRES_URL
          value: "postgres://platform:dev@localhost:5432/platform_dev"
      ports:
        - containerPort: 5432
        - containerPort: 15020
      readinessProbe:
        httpGet:
          path: /readyz
          port: 15020
        initialDelaySeconds: 10
        periodSeconds: 2
      volumeMounts:
        - name: certs
          mountPath: /certs
          readOnly: true
        - name: proxy
          mountPath: /proxy
          readOnly: true
      resources:
        requests:
          cpu: 100m
          memory: 160Mi     # 128Mi app + 32Mi proxy overhead
        limits:
          memory: 544Mi     # 512Mi app + 32Mi proxy overhead
  volumes:
    - name: certs
      emptyDir: {}
    - name: proxy
      hostPath:
        path: __PROXY_PATH__
        type: Directory
```

Key changes from current:
- **otel-collector container: removed**
- **otel-config ConfigMap: removed**
- **command**: `["/proxy/platform-proxy"]` instead of default entrypoint
- **args**: `--wrap --tcp-ports=5432 --scrape-type=postgres -- postgres <args>`
- **readinessProbe**: now hits proxy's health endpoint, not pg_isready
- **volumes**: `proxy` hostPath replaces `otel-config` ConfigMap
- **resources**: slightly padded for proxy overhead (+32Mi RAM)

### Valkey and MinIO: Same Pattern

Valkey:
```
command: ["/proxy/platform-proxy"]
args: ["--wrap", "--tcp-ports=6379", "--scrape-type=redis", "--",
       "valkey-server", "--save", "", "--appendonly", "no", "--requirepass", "dev"]
env:
  PROXY_SCRAPE_REDIS_URL: "redis://:dev@localhost:6379"
```

MinIO:
```
command: ["/proxy/platform-proxy"]
args: ["--wrap", "--scrape-url=https://localhost:9000/minio/v2/metrics/cluster",
       "--scrape-tls-insecure", "--",
       "minio", "server", "/data", "--certs-dir", "/certs"]
```

### `hack/deploy-services.sh` Changes

Replace `__OTEL_ENDPOINT__` sed with `__PLATFORM_API_URL__` and `__PROXY_PATH__`:

```bash
# Before: sed "s|__OTEL_ENDPOINT__|${OTEL_ENDPOINT}|g"
# After:
PLATFORM_URL="http://${REGISTRY_BACKEND_HOST:-host.docker.internal}:${REGISTRY_BACKEND_PORT:-8080}"
PROXY_PATH="/tmp/platform-e2e/${WORKTREE:-main}/proxy"

for f in postgres.yaml valkey.yaml minio.yaml; do
  sed -e "s|__PLATFORM_API_URL__|${PLATFORM_URL}|g" \
      -e "s|__PROXY_PATH__|${PROXY_PATH}|g" \
    "${SCRIPT_DIR}/test-manifests/${f}" \
    | kubectl apply -n "${NS}" -f -
done
```

### Test Outline — PR 4

**New behaviors to test:**
- Platform receives postgres metrics via proxy (same metric names as before) — integration
- Platform receives valkey metrics via proxy — integration
- Platform receives minio metrics via proxy — integration
- Postgres stdout logs appear in platform log_entries — integration
- Valkey stdout logs appear in platform log_entries — integration
- Readiness probes work (proxy /readyz) — integration
- Signal forwarding: pod delete → postgres graceful shutdown — integration

**Existing tests affected:**
- All integration tests using `test_state()` — infra pods now use proxy (transparent, no code changes needed)
- E2E tests — same (proxy is invisible to tests that don't check pod specs)

**Estimated test count:** ~0 new unit + 5 integration (verify telemetry flows)

### Verification
- `just dev-up` deploys services with proxy wrapper
- `kubectl logs -n platform-dev-main postgres` shows proxy + postgres interleaved logs
- Platform DB: `SELECT count(*) FROM metric_series WHERE name LIKE 'postgresql.%'` — same metrics as before
- Platform DB: `SELECT count(*) FROM log_entries WHERE service = 'platform-postgres'` — new! Postgres logs captured
- No otel-collector containers running: `kubectl get pods -n platform-dev-main -o jsonpath='{.items[*].spec.containers[*].name}'` — only `postgres`, `valkey`, `minio`

---

## PR 5: Pipeline + Agent Pod Integration

Update the pipeline executor and agent service to use the proxy wrapper for pipeline steps, agent sessions, and deployed apps.

- [ ] Types & errors defined
- [ ] Migration applied
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### Code Changes

| File | Change |
|---|---|
| `src/pipeline/executor.rs` | `build_pod_spec()`: wrap step command with proxy, inject proxy env vars, mount proxy binary, remove OTEL env vars |
| `src/agent/claude_code/pod.rs` | `build_agent_pod()`: wrap agent-runner command with proxy, inject proxy env vars, mount proxy binary |
| `src/deployer/reconciler.rs` | For deployed apps: inject proxy wrapper into rendered manifests (new helper) |
| `src/deployer/applier.rs` | `validate_pod_spec()`: allow proxy-injected hostPath volume (dev mode only) |
| `src/config.rs` | New: `proxy_binary_path` (env `PLATFORM_PROXY_PATH`, default from `PLATFORM_SEED_IMAGES_PATH/../proxy`) |
| `hack/build-agent-images.sh` | Add proxy build step |

### Pipeline Pod Changes (`src/pipeline/executor.rs`)

In `build_pod_spec()`, the step container currently has:
```rust
command: vec!["/bin/sh".into(), "-c".into()],
args: vec![shell_script],  // user's build commands
```

After change:
```rust
// If mesh_enabled and proxy binary available
command: vec!["/proxy/platform-proxy".into()],
args: vec![
    "--wrap".into(),
    "--app-port=0".into(),  // no HTTP port for build steps
    "--".into(),
    "/bin/sh".into(), "-c".into(),
    shell_script,
],
```

Plus volume mount for proxy binary (same hostPath pattern as agent-runner):
```rust
volumes.push(Volume {
    name: "proxy".into(),
    host_path: Some(HostPathVolumeSource {
        path: config.proxy_binary_path.clone(),
        type_: Some("Directory".into()),
    }),
    ..Default::default()
});
```

And env vars:
```rust
env.push(EnvVar { name: "PLATFORM_SERVICE_NAME".into(), value: Some(format!("pipeline/{}/{}", project_name, step_name)), ..});
env.push(EnvVar { name: "PROXY_HEALTH_PORT".into(), value: Some("15020".into()), ..});
// PLATFORM_API_URL and PLATFORM_API_TOKEN already injected
```

### Agent Pod Changes (`src/agent/claude_code/pod.rs`)

Similar: wrap `agent-runner` command with proxy:
```rust
command: vec!["/proxy/platform-proxy".into()],
args: vec![
    "--wrap".into(),
    "--".into(),
    // original agent-runner command + args
    "/workspace/.platform/bin/agent-runner".into(),
    ...runner_args,
],
```

### Deployed App Changes (`src/deployer/reconciler.rs`)

For user-deployed apps, the deployer injects the proxy at manifest render time. The renderer has access to template variables; we add a new one:

```rust
// In render_manifests():
vars.insert("proxy_image", format!("{}/platform-proxy:{}", registry, version));
vars.insert("proxy_enabled", mesh_enabled.to_string());
```

Users can opt-in via their Kustomize overlay or the deployment config. For platform-managed deployments (onboarding templates), the proxy is injected automatically.

### Test Outline — PR 5

**New behaviors to test:**
- Pipeline step runs with proxy wrapper — integration (spawn executor)
- Pipeline step stdout captured as log entries — integration
- Agent session runs with proxy wrapper — E2E (real pod lifecycle)
- Agent logs correlated to session trace — E2E
- Deployed app with proxy: traces generated — E2E

**Existing tests affected:**
- Pipeline integration tests — pod spec now includes proxy volume mount
- Agent integration tests — pod spec changes
- Tests with `cli_spawn_enabled=true` — mock CLI still works through proxy

**Estimated test count:** ~3 integration + 3 E2E

---

## PR 6: Trust Bundle Distribution + NetworkPolicy Updates

Distribute the CA trust bundle to all namespaces and update NetworkPolicies to allow mTLS traffic on the proxy's TLS port.

- [ ] Types & errors defined
- [ ] Migration applied
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### Code Changes

| File | Change |
|---|---|
| `src/deployer/namespace.rs` | `ensure_session_namespace()`: create ConfigMap `mesh-ca-bundle` with CA PEM, mount into proxy |
| `src/deployer/namespace.rs` | `build_namespace_network_policy()`: add egress rule for TLS port 8443 to platform + services namespaces |
| `src/mesh/mod.rs` | Background task: sync trust bundle ConfigMap to all managed namespaces on CA rotation |
| `src/pipeline/executor.rs` | Mount `mesh-ca-bundle` ConfigMap as volume, set `PROXY_CA_BUNDLE_PATH` env var |
| `src/agent/claude_code/pod.rs` | Same: mount CA bundle, set env var |

### Trust Bundle ConfigMap

Created in each managed namespace:
```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: mesh-ca-bundle
  labels:
    platform.io/managed-by: platform
data:
  ca.pem: |
    -----BEGIN CERTIFICATE-----
    ...root CA cert PEM...
    -----END CERTIFICATE-----
```

The proxy reads this at `PROXY_CA_BUNDLE_PATH=/mesh-ca/ca.pem` for TLS client verification.

### NetworkPolicy Changes

Current egress allows TCP 8080 to platform namespace. Add:
```yaml
# Allow mTLS traffic to/from services
- to:
  - namespaceSelector:
      matchLabels:
        platform.io/managed-by: platform
  ports:
    - port: 8443
      protocol: TCP
```

### Test Outline — PR 6

**New behaviors to test:**
- ConfigMap created in session namespace — integration
- NetworkPolicy allows 8443 — integration
- Trust bundle refresh on CA rotation — integration

**Estimated test count:** ~0 unit + 3 integration

---

## Summary

| PR | What | LOC est. | Tests |
|----|------|----------|-------|
| 1 | Mesh CA module + cert API | ~800 | 8 unit + 4 int |
| 2 | Proxy binary: wrapper + logs + OTLP | ~2,500 | 15 unit |
| 3 | Proxy mTLS + HTTP proxy + scrapers | ~1,500 | 6 unit + 8 int |
| 4 | Infra service integration (pg/valkey/minio) | ~200 (YAML) | 5 int |
| 5 | Pipeline + agent + deployed app integration | ~400 | 3 int + 3 E2E |
| 6 | Trust bundle distribution + NetworkPolicy | ~300 | 3 int |

**Total: ~5,700 LOC, ~55 tests**

### Dependency Chain

```
PR 1 (CA) ─────────────────────────────────────┐
                                                 │
PR 2 (proxy binary: no mTLS) ──→ PR 3 (mTLS) ──┤
                                                 │
                                  PR 4 (infra) ──┤ (needs PR 2 + PR 3)
                                                 │
                                  PR 5 (pods)  ──┤ (needs PR 2 + PR 3)
                                                 │
                                  PR 6 (trust) ──┘ (needs PR 1 + PR 3)
```

PR 1 and PR 2 can proceed in parallel. PR 3 depends on both. PRs 4, 5, 6 depend on PR 3.

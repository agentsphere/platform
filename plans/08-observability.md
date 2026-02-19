# 08 — Observability: Ingest, Store, Query, Alert

## Prerequisite
- 01-foundation complete (store, AppState, MinIO operator)
- 02-identity-auth complete (AuthUser, RequirePermission)

## Blocks
- Nothing — self-contained module

## Can Parallelize With
- 03-git-server, 04-project-mgmt, 05-build-engine, 06-deployer, 07-agent, 09-secrets-notify

---

## Scope

OTLP ingest endpoint (receives traces, logs, metrics from OTel Collector), hot storage in Postgres, cold storage in MinIO as Parquet, query API for logs/traces/metrics, alert rule evaluation with notification dispatch. Replaces OpenObserve.

This is the largest module (~2,200 LOC). Consider splitting into sub-tasks if implementing with Claude Code.

---

## Deliverables

### 1. `src/observe/mod.rs` — Module Root
Re-exports ingest, store, parquet, query, alert, correlation. Spawns background tasks (log rotation, alert evaluation).

### 2. `src/observe/ingest.rs` — OTLP HTTP Receiver

Receive OpenTelemetry Protocol data over HTTP (protobuf format):

- `POST /v1/traces` — receive trace/span data
  - Content-Type: `application/x-protobuf`
  - Decode with `prost` using OTLP proto definitions
  - Extract spans, parse attributes, create `traces` + `spans` rows
- `POST /v1/logs` — receive log data
  - Decode OTLP LogRecord messages
  - Extract: timestamp, severity, body, attributes, resource attributes
  - Create `log_entries` rows with correlation envelope
- `POST /v1/metrics` — receive metric data
  - Decode OTLP metric messages
  - Upsert `metric_series` (by name + labels), insert `metric_samples`

All ingest endpoints:
- Extract correlation fields from resource/span attributes: `trace_id`, `span_id`, `session_id`, `project_id`, `user_id`, `service`
- Batch writes (buffer in memory, flush every 1s or 100 records)
- Return OTLP ExportResponse (protobuf)

### 3. `src/observe/correlation.rs` — Correlation Envelope

Inject and resolve correlation metadata:

- `pub fn extract_correlation(attributes: &HashMap<String, Value>) -> CorrelationEnvelope`
  - Pull `trace_id`, `span_id`, `session_id`, `project_id`, `user_id` from OTLP attributes
  - Resolve: if `session_id` present, look up `project_id` and `user_id` from `agent_sessions` table

- `CorrelationEnvelope`:
  ```rust
  pub struct CorrelationEnvelope {
      pub trace_id: Option<String>,
      pub span_id: Option<String>,
      pub session_id: Option<Uuid>,
      pub project_id: Option<Uuid>,
      pub user_id: Option<Uuid>,
      pub service: String,
  }
  ```

### 4. `src/observe/store.rs` — Hot Storage (Postgres)

Write telemetry to Postgres hot tier:

- `pub async fn write_spans(pool, spans: Vec<SpanRecord>) -> Result<()>` — bulk insert spans
- `pub async fn write_logs(pool, logs: Vec<LogRecord>) -> Result<()>` — bulk insert log_entries
- `pub async fn write_metrics(pool, metrics: Vec<MetricSample>) -> Result<()>` — upsert series + insert samples

Buffer and flush strategy:
- In-memory buffer per signal type (traces, logs, metrics)
- Flush on: buffer size >= 100, or 1 second elapsed
- Use `sqlx::query!` with `COPY` or multi-row `INSERT` for throughput

### 5. `src/observe/parquet.rs` — Cold Storage (MinIO Parquet)

Background task: rotate old data from Postgres to MinIO as Parquet files:

- `pub async fn rotate_logs(state: &AppState) -> Result<()>`
  - Query `log_entries` older than 48h
  - Convert to Arrow RecordBatch using `arrow` crate
  - Write as Parquet file using `parquet` crate (with Snappy compression)
  - Upload to MinIO: `otel/logs/{date}/logs_{batch_id}.parquet`
  - Delete rotated rows from Postgres

- `pub async fn rotate_metrics(state: &AppState) -> Result<()>`
  - Same pattern for `metric_samples` older than 1h
  - MinIO path: `otel/metrics/{date}/metrics_{batch_id}.parquet`

- `pub async fn rotate_spans(state: &AppState) -> Result<()>`
  - Spans older than 48h → Parquet
  - MinIO path: `otel/traces/{date}/spans_{batch_id}.parquet`

Arrow schema for logs:
```rust
let schema = Schema::new(vec![
    Field::new("id", DataType::Utf8, false),
    Field::new("timestamp", DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())), false),
    Field::new("trace_id", DataType::Utf8, true),
    Field::new("span_id", DataType::Utf8, true),
    Field::new("project_id", DataType::Utf8, true),
    Field::new("session_id", DataType::Utf8, true),
    Field::new("service", DataType::Utf8, false),
    Field::new("level", DataType::Utf8, false),
    Field::new("message", DataType::Utf8, false),
    Field::new("attributes", DataType::Utf8, true),  // JSON string
]);
```

Rotation schedule: run every 15 minutes as a tokio interval task.

### 6. `src/observe/query.rs` — Query Engine

Query API for logs, traces, and metrics:

**Log search**:
- `GET /api/observe/logs`
  - Params: `project_id`, `session_id`, `trace_id`, `level`, `service`, `q` (full-text), `from`, `to`, `limit`, `offset`
  - Query Postgres hot tier first
  - If time range extends beyond 48h: also query MinIO Parquet files (read with `parquet` crate + row group filtering)
  - Returns: array of log entries with all correlation fields

**Trace view**:
- `GET /api/observe/traces`
  - Params: `project_id`, `session_id`, `service`, `status`, `from`, `to`, `limit`
  - Returns: trace summaries (root_span, duration, status, service)
- `GET /api/observe/traces/:trace_id`
  - Returns: full trace with all spans (tree structure for waterfall view)
  - Each span: name, service, kind, status, duration, attributes, events

**Metric query**:
- `GET /api/observe/metrics`
  - Params: `name`, `labels` (JSON), `project_id`, `from`, `to`, `step` (aggregation interval)
  - Returns: time-series data points
  - Aggregation: avg, sum, max, min, count per step interval
- `GET /api/observe/metrics/names`
  - Returns: distinct metric names with labels

**Session replay**:
- `GET /api/observe/sessions/:session_id/timeline`
  - Query all logs + spans for a session_id, ordered by timestamp
  - Returns: combined timeline of what the agent did

All observe endpoints require `observe:read` permission (project-scoped if `project_id` specified).

### 7. `src/observe/alert.rs` — Alert Evaluation

Background task that evaluates alert rules:

- `pub async fn evaluate_alerts(state: &AppState) -> Result<()>`
  - Run every 30 seconds
  - For each enabled `alert_rules` row:
    - Execute the `query` (metric query or log filter)
    - Compare result against `threshold` using `condition` (gt, lt, eq, absent)
    - If condition met for >= `for_seconds`: fire alert
    - If condition not met and alert was firing: resolve alert

- Alert state machine:
  - `pending` → condition met → start timer
  - `pending` + held for `for_seconds` → `firing` → create `alert_events` row, dispatch notifications
  - `firing` → condition no longer met → `resolved` → update `alert_events.resolved_at`, dispatch resolution notification

- Alert API:
  - `GET /api/observe/alerts` — list alert rules
  - `POST /api/observe/alerts` — create alert rule (requires `alert:manage`)
  - `PATCH /api/observe/alerts/:id` — update rule
  - `DELETE /api/observe/alerts/:id` — delete rule
  - `GET /api/observe/alerts/:id/events` — list alert events (firing/resolved history)

### 8. Live Tail (Valkey pub/sub)

Real-time log streaming for the UI:

- When logs are ingested, publish to Valkey channel: `logs:{project_id}`
- WebSocket endpoint: `GET /api/observe/logs/tail?project_id=X`
  - Subscribe to Valkey channel
  - Stream new log entries to connected WebSocket clients
  - Filter by level, service in real-time

---

## Data Flow

```
App/Agent → OTel Collector (DaemonSet)
  → OTLP HTTP POST to platform /v1/{traces,logs,metrics}
  → prost protobuf decode
  → extract correlation envelope
  → buffer in memory
  → flush to Postgres (hot tier)
  → publish to Valkey (live tail)

Background rotation (every 15min):
  → query old data from Postgres
  → convert to Arrow RecordBatch
  → write Parquet file
  → upload to MinIO
  → delete from Postgres

Query:
  → check Postgres hot tier (recent data)
  → if time range extends beyond hot window: read Parquet from MinIO
  → merge and return results
```

---

## Testing

- Unit: protobuf decoding (mock OTLP payloads), correlation extraction, Parquet schema building, alert condition evaluation
- Integration:
  - Send OTLP traces → query traces API → see spans in waterfall
  - Send OTLP logs → query logs → filter by project/session/level
  - Send OTLP metrics → query metrics → get time-series data
  - Log rotation: insert old logs → trigger rotation → verify Parquet in MinIO → verify deleted from Postgres
  - Alert: create rule → send metrics exceeding threshold → verify alert fires → send metrics below → verify resolved
  - Live tail: connect WebSocket → ingest logs → receive in real-time

## Done When

1. OTLP ingest endpoints accept traces, logs, metrics (protobuf)
2. Correlation envelope extracted and stored with all telemetry
3. Hot storage in Postgres queryable via API
4. Cold storage rotation to MinIO Parquet files
5. Log search with full-text, trace waterfall, metric time-series
6. Session replay (timeline of agent actions)
7. Alert rules evaluate and fire notifications
8. Live log tail via WebSocket + Valkey pub/sub

## Security Context (from security hardening)

All new handlers must follow the security patterns established in the codebase:

- **Input validation**: Validate all query parameters (time ranges, limits, offsets, filter strings). `q` full-text search parameter must be length-limited (1-1000). Metric names, label keys/values, service names should be validated. See `CLAUDE.md` Security Patterns for field limits.
- **Authorization on reads**: All observe endpoints must check project-level read access when `project_id` is specified. Use `require_project_read()` pattern — return 404 for unauthorized private projects.
- **OTLP ingest auth**: The `/v1/{traces,logs,metrics}` endpoints receive data from OTel Collectors. Decide on auth model: API token in `Authorization` header, or network-level restriction (K8s NetworkPolicy). At minimum, require a Bearer token.
- **Rate limiting / backpressure**: OTLP ingest can receive high volumes. Implement backpressure — reject with 429 if buffer is full, rather than OOM. Set per-project or per-service ingestion limits.
- **Alert rule validation**: Alert rule `query` field is user-supplied. Ensure it's parameterized, not raw SQL. Validate threshold values, condition operators, and `for_seconds` range.
- **Parquet path safety**: When constructing MinIO paths for Parquet files, use sanitized batch IDs and dates — never interpolate user-supplied strings into object storage paths.
- **Live tail WebSocket auth**: Re-validate auth on WebSocket connection, not just upgrade. Enforce project-level read permission for the subscribed `project_id`.
- **Audit logging**: Log alert rule create/update/delete. Don't log raw telemetry data in audit entries.
- **Sensitive data in telemetry**: Be aware that ingested logs/traces may contain secrets. Don't index or expose attributes that commonly contain secrets (e.g., `http.request.header.authorization`).

## Estimated LOC
~2,200 Rust

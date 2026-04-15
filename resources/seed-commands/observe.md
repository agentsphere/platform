RUNTIME: sandbox (full repo access, kubectl)
ROLE: dev

You are an observability engineer working in /workspace.
Your job: instrument the application with OpenTelemetry tracing, structured logging, and metrics so the team can monitor, debug, and understand system behavior.

== STEP 1: READ CONTEXT ==
Read CLAUDE.md for observability conventions.
Read existing instrumentation (if any).
Read project profile for observability requirements.
Read $ARGUMENTS for specific focus.

== STEP 2: INSTRUMENTATION LEVEL (profile-conditional) ==

If profile.observability: full →
  TRACING:
  - Every incoming HTTP request gets a trace span
  - Every outbound call (DB, HTTP, cache, queue) gets a child span
  - Spans include: operation name, status, duration, relevant IDs
  - Errors attached to spans with stack context
  - Trace context propagated across service boundaries (W3C traceparent)

  LOGGING:
  - Structured JSON logs (not printf-style)
  - Every log line includes: trace_id, span_id, service, level
  - Log levels used correctly: ERROR (actionable), WARN (degraded), INFO (business events), DEBUG (developer)
  - Sensitive data NEVER logged (passwords, tokens, PII)

  METRICS:
  - Request rate, error rate, latency (RED method)
  - Custom business metrics where relevant (signups, orders, etc.)
  - Resource metrics (connection pool size, queue depth)

  ALERTS:
  - Error rate > threshold → alert
  - Latency p99 > threshold → alert
  - Custom alerts for business-critical paths

  LOG AGGREGATION TO TRACES:
  - Template-based log deduplication (group identical log patterns)
  - Attach logs to their parent trace span
  - Store log templates + counts instead of raw lines (reduces storage)

If profile.observability: standard →
  TRACING:
  - HTTP request tracing (inbound only)
  - DB query spans (slow query logging)

  LOGGING:
  - Structured logs with trace_id correlation
  - INFO + ERROR levels

  METRICS:
  - Basic RED metrics only

  ALERTS:
  - Error rate alert only

If profile.observability: minimal →
  LOGGING:
  - stdout/stderr, basic format
  - Errors logged with enough context to debug

  No tracing, no metrics, no alerts.
  Skip the rest of this skill.

== STEP 3: SET UP OTEL SDK ==
Install the OpenTelemetry SDK for the project's language:
- Configure OTLP exporter pointing to platform endpoint
- Set service name, version, environment as resource attributes
- Configure trace sampling (100% in dev/staging, 10-50% in production)
- Configure log bridge (route framework logs through OTEL)

Environment variables to set in deployment:
```
OTEL_EXPORTER_OTLP_ENDPOINT=http://platform.platform.svc.cluster.local:8080
OTEL_SERVICE_NAME=<project-name>
OTEL_RESOURCE_ATTRIBUTES=service.version=<version>,deployment.environment=<env>
```

== STEP 4: INSTRUMENT CODE ==
Add instrumentation following the project's patterns:

HTTP handlers:
- Middleware/interceptor for automatic request tracing
- Record: method, path, status_code, duration, user_id (if authenticated)

Database:
- Wrap DB client with tracing (most ORMs support this)
- Record: operation (SELECT/INSERT/etc), table, duration
- Flag slow queries (> 100ms)

External HTTP calls:
- Wrap HTTP client to propagate trace context
- Record: method, URL (sanitized), status_code, duration

Background jobs:
- Create root span for each job execution
- Link to triggering trace if applicable

== STEP 5: CONFIGURE ALERTS ==
Use platform alert API to create alert rules:

```bash
source /workspace/.platform/.env
curl -sf -X POST "${PLATFORM_API_URL}/api/projects/${PROJECT_ID}/alerts" \
  -H "Authorization: Bearer ${PLATFORM_API_TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{"name": "High Error Rate", "metric": "http_errors_total", "condition": "rate > 0.05", "window": "5m"}'
```

== STEP 6: VERIFY ==
- Deploy instrumented app
- Make requests
- Check platform observe UI: traces appearing? logs correlated? metrics graphing?
- Trigger an error: does the alert fire?

== STEP 7: PUSH ==
Commit instrumentation code + deployment config changes, push, create MR.

== REQUIREMENTS ==
$ARGUMENTS

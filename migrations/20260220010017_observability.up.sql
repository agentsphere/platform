CREATE TABLE traces (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    trace_id    TEXT NOT NULL UNIQUE,
    project_id  UUID REFERENCES projects(id),
    session_id  UUID REFERENCES agent_sessions(id),
    user_id     UUID REFERENCES users(id),
    root_span   TEXT NOT NULL,
    service     TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'ok'
                CHECK (status IN ('ok', 'error', 'unset')),
    duration_ms INTEGER,
    started_at  TIMESTAMPTZ NOT NULL,
    finished_at TIMESTAMPTZ,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE spans (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    trace_id       TEXT NOT NULL,
    span_id        TEXT NOT NULL UNIQUE,
    parent_span_id TEXT,
    name           TEXT NOT NULL,
    service        TEXT NOT NULL,
    kind           TEXT NOT NULL DEFAULT 'internal'
                   CHECK (kind IN ('internal', 'server', 'client', 'producer', 'consumer')),
    status         TEXT NOT NULL DEFAULT 'ok'
                   CHECK (status IN ('ok', 'error', 'unset')),
    attributes     JSONB,
    events         JSONB,
    duration_ms    INTEGER,
    started_at     TIMESTAMPTZ NOT NULL,
    finished_at    TIMESTAMPTZ
);

CREATE INDEX idx_spans_trace ON spans(trace_id);

CREATE TABLE log_entries (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    timestamp  TIMESTAMPTZ NOT NULL DEFAULT now(),
    trace_id   TEXT,
    span_id    TEXT,
    project_id UUID REFERENCES projects(id),
    session_id UUID REFERENCES agent_sessions(id),
    user_id    UUID REFERENCES users(id),
    service    TEXT NOT NULL,
    level      TEXT NOT NULL DEFAULT 'info'
               CHECK (level IN ('trace', 'debug', 'info', 'warn', 'error', 'fatal')),
    message    TEXT NOT NULL,
    attributes JSONB,
    namespace  TEXT,
    pod        TEXT,
    container  TEXT
);

CREATE INDEX idx_log_ts ON log_entries(timestamp DESC);
CREATE INDEX idx_log_project ON log_entries(project_id, timestamp DESC);
CREATE INDEX idx_log_session ON log_entries(session_id, timestamp DESC);
CREATE INDEX idx_log_trace ON log_entries(trace_id);
CREATE INDEX idx_log_level ON log_entries(level, timestamp DESC);

CREATE TABLE metric_series (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name        TEXT NOT NULL,
    labels      JSONB NOT NULL DEFAULT '{}',
    metric_type TEXT NOT NULL DEFAULT 'gauge'
                CHECK (metric_type IN ('gauge', 'counter', 'histogram', 'summary')),
    unit        TEXT,
    project_id  UUID REFERENCES projects(id),
    last_value  DOUBLE PRECISION,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (name, labels)
);

CREATE TRIGGER trg_metric_series_updated_at
    BEFORE UPDATE ON metric_series
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

CREATE TABLE metric_samples (
    series_id UUID        NOT NULL REFERENCES metric_series(id) ON DELETE CASCADE,
    timestamp TIMESTAMPTZ NOT NULL,
    value     DOUBLE PRECISION NOT NULL,
    PRIMARY KEY (series_id, timestamp)
);

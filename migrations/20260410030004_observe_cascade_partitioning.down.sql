-- Revert: traces FK back to RESTRICT
ALTER TABLE traces DROP CONSTRAINT traces_project_id_fkey;
ALTER TABLE traces ADD CONSTRAINT traces_project_id_fkey
    FOREIGN KEY (project_id) REFERENCES projects(id);

-- Revert: spans back to non-partitioned with RESTRICT FK
CREATE TABLE _bak_spans AS SELECT * FROM spans;
DROP TABLE spans;

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
    finished_at    TIMESTAMPTZ,
    project_id     UUID REFERENCES projects(id),
    session_id     UUID REFERENCES agent_sessions(id),
    user_id        UUID REFERENCES users(id)
);

CREATE INDEX idx_spans_trace ON spans(trace_id);
CREATE INDEX idx_spans_project_kind_started ON spans(project_id, kind, started_at);
CREATE INDEX idx_spans_session_started ON spans(session_id, started_at) WHERE session_id IS NOT NULL;
CREATE INDEX idx_spans_status_kind_started ON spans(status, kind, started_at);

INSERT INTO spans (id, trace_id, span_id, parent_span_id, name, service, kind, status,
                   attributes, events, duration_ms, started_at, finished_at,
                   project_id, session_id, user_id)
SELECT id, trace_id, span_id, parent_span_id, name, service, kind, status,
       attributes, events, duration_ms, started_at, finished_at,
       project_id, session_id, user_id
FROM _bak_spans;
DROP TABLE _bak_spans;

-- Revert: log_entries back to non-partitioned with RESTRICT FK
CREATE TABLE _bak_log_entries AS SELECT * FROM log_entries;
DROP TABLE log_entries;

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
    container  TEXT,
    source     TEXT NOT NULL DEFAULT 'external'
               CHECK (source IN ('system', 'api', 'session', 'external'))
);

CREATE INDEX idx_log_ts      ON log_entries(timestamp DESC);
CREATE INDEX idx_log_project  ON log_entries(project_id, timestamp DESC);
CREATE INDEX idx_log_session  ON log_entries(session_id, timestamp DESC);
CREATE INDEX idx_log_trace    ON log_entries(trace_id);
CREATE INDEX idx_log_level    ON log_entries(level, timestamp DESC);
CREATE INDEX idx_log_attrs    ON log_entries USING gin (attributes jsonb_path_ops);
CREATE INDEX idx_log_source   ON log_entries(source, timestamp DESC);

INSERT INTO log_entries (id, timestamp, trace_id, span_id, project_id, session_id, user_id,
                         service, level, message, attributes, namespace, pod, container, source)
SELECT id, timestamp, trace_id, span_id, project_id, session_id, user_id,
       service, level, message, attributes, namespace, pod, container, source
FROM _bak_log_entries;
DROP TABLE _bak_log_entries;

-- Revert: metric_samples back to non-partitioned
CREATE TABLE _bak_metric_samples AS SELECT * FROM metric_samples;
DROP TABLE metric_samples;

CREATE TABLE metric_samples (
    series_id UUID NOT NULL REFERENCES metric_series(id) ON DELETE CASCADE,
    timestamp TIMESTAMPTZ NOT NULL,
    value     DOUBLE PRECISION NOT NULL,
    PRIMARY KEY (series_id, timestamp)
);

INSERT INTO metric_samples (series_id, timestamp, value)
SELECT series_id, timestamp, value FROM _bak_metric_samples;
DROP TABLE _bak_metric_samples;

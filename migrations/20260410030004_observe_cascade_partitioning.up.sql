-- #13: FK CASCADE on project_id for GDPR deletion support
-- #14: Partition spans, log_entries, metric_samples by time range
--
-- traces is NOT partitioned because upsert_trace uses ON CONFLICT (trace_id)
-- and different spans in the same trace can have different started_at values,
-- which would create duplicate trace rows with UNIQUE(trace_id, started_at).

-- ============================================================
-- Part 1: traces — FK cascade only (no partitioning)
-- ============================================================
ALTER TABLE traces DROP CONSTRAINT traces_project_id_fkey;
ALTER TABLE traces ADD CONSTRAINT traces_project_id_fkey
    FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE;

-- ============================================================
-- Part 2: spans — backup, drop, recreate as partitioned
-- ============================================================
CREATE TABLE _bak_spans AS SELECT * FROM spans;
DROP TABLE spans;

CREATE TABLE spans (
    id             UUID NOT NULL DEFAULT gen_random_uuid(),
    trace_id       TEXT NOT NULL,
    span_id        TEXT NOT NULL,
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
    project_id     UUID REFERENCES projects(id) ON DELETE CASCADE,
    session_id     UUID REFERENCES agent_sessions(id),
    user_id        UUID REFERENCES users(id),
    PRIMARY KEY (id, started_at),
    UNIQUE (span_id, started_at)
) PARTITION BY RANGE (started_at);

CREATE TABLE spans_p_hist   PARTITION OF spans FOR VALUES FROM (MINVALUE)     TO ('2026-04-01');
CREATE TABLE spans_p_202604 PARTITION OF spans FOR VALUES FROM ('2026-04-01') TO ('2026-05-01');
CREATE TABLE spans_p_202605 PARTITION OF spans FOR VALUES FROM ('2026-05-01') TO ('2026-06-01');
CREATE TABLE spans_p_202606 PARTITION OF spans FOR VALUES FROM ('2026-06-01') TO ('2026-07-01');
CREATE TABLE spans_p_202607 PARTITION OF spans FOR VALUES FROM ('2026-07-01') TO ('2026-08-01');
CREATE TABLE spans_p_202608 PARTITION OF spans FOR VALUES FROM ('2026-08-01') TO ('2026-09-01');
CREATE TABLE spans_p_202609 PARTITION OF spans FOR VALUES FROM ('2026-09-01') TO ('2026-10-01');

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

-- ============================================================
-- Part 3: log_entries — backup, drop, recreate as partitioned
-- ============================================================
CREATE TABLE _bak_log_entries AS SELECT * FROM log_entries;
DROP TABLE log_entries;

CREATE TABLE log_entries (
    id         UUID NOT NULL DEFAULT gen_random_uuid(),
    timestamp  TIMESTAMPTZ NOT NULL DEFAULT now(),
    trace_id   TEXT,
    span_id    TEXT,
    project_id UUID REFERENCES projects(id) ON DELETE CASCADE,
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
               CHECK (source IN ('system', 'api', 'session', 'external')),
    PRIMARY KEY (id, timestamp)
) PARTITION BY RANGE (timestamp);

CREATE TABLE log_entries_p_hist   PARTITION OF log_entries FOR VALUES FROM (MINVALUE)     TO ('2026-04-01');
CREATE TABLE log_entries_p_202604 PARTITION OF log_entries FOR VALUES FROM ('2026-04-01') TO ('2026-05-01');
CREATE TABLE log_entries_p_202605 PARTITION OF log_entries FOR VALUES FROM ('2026-05-01') TO ('2026-06-01');
CREATE TABLE log_entries_p_202606 PARTITION OF log_entries FOR VALUES FROM ('2026-06-01') TO ('2026-07-01');
CREATE TABLE log_entries_p_202607 PARTITION OF log_entries FOR VALUES FROM ('2026-07-01') TO ('2026-08-01');
CREATE TABLE log_entries_p_202608 PARTITION OF log_entries FOR VALUES FROM ('2026-08-01') TO ('2026-09-01');
CREATE TABLE log_entries_p_202609 PARTITION OF log_entries FOR VALUES FROM ('2026-09-01') TO ('2026-10-01');

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

-- ============================================================
-- Part 4: metric_samples — backup, drop, recreate as partitioned
-- ============================================================
CREATE TABLE _bak_metric_samples AS SELECT * FROM metric_samples;
DROP TABLE metric_samples;

CREATE TABLE metric_samples (
    series_id UUID NOT NULL REFERENCES metric_series(id) ON DELETE CASCADE,
    timestamp TIMESTAMPTZ NOT NULL,
    value     DOUBLE PRECISION NOT NULL,
    PRIMARY KEY (series_id, timestamp)
) PARTITION BY RANGE (timestamp);

CREATE TABLE metric_samples_p_hist   PARTITION OF metric_samples FOR VALUES FROM (MINVALUE)     TO ('2026-04-01');
CREATE TABLE metric_samples_p_202604 PARTITION OF metric_samples FOR VALUES FROM ('2026-04-01') TO ('2026-05-01');
CREATE TABLE metric_samples_p_202605 PARTITION OF metric_samples FOR VALUES FROM ('2026-05-01') TO ('2026-06-01');
CREATE TABLE metric_samples_p_202606 PARTITION OF metric_samples FOR VALUES FROM ('2026-06-01') TO ('2026-07-01');
CREATE TABLE metric_samples_p_202607 PARTITION OF metric_samples FOR VALUES FROM ('2026-07-01') TO ('2026-08-01');
CREATE TABLE metric_samples_p_202608 PARTITION OF metric_samples FOR VALUES FROM ('2026-08-01') TO ('2026-09-01');
CREATE TABLE metric_samples_p_202609 PARTITION OF metric_samples FOR VALUES FROM ('2026-09-01') TO ('2026-10-01');

INSERT INTO metric_samples (series_id, timestamp, value)
SELECT series_id, timestamp, value FROM _bak_metric_samples;
DROP TABLE _bak_metric_samples;

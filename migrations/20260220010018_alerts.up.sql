CREATE TABLE alert_rules (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name            TEXT NOT NULL,
    description     TEXT,
    query           TEXT NOT NULL,
    condition       TEXT NOT NULL,
    threshold       DOUBLE PRECISION,
    for_seconds     INTEGER NOT NULL DEFAULT 60,
    severity        TEXT NOT NULL DEFAULT 'warning'
                    CHECK (severity IN ('info', 'warning', 'critical')),
    notify_channels TEXT[] NOT NULL DEFAULT '{}',
    project_id      UUID REFERENCES projects(id),
    enabled         BOOLEAN NOT NULL DEFAULT true,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE alert_events (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    rule_id     UUID NOT NULL REFERENCES alert_rules(id) ON DELETE CASCADE,
    status      TEXT NOT NULL CHECK (status IN ('firing', 'resolved')),
    value       DOUBLE PRECISION,
    message     TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    resolved_at TIMESTAMPTZ
);

CREATE INDEX idx_alert_events_status ON alert_events(status, created_at DESC);

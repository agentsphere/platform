CREATE TABLE ops_repos (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name            TEXT NOT NULL UNIQUE,
    repo_url        TEXT NOT NULL,
    branch          TEXT NOT NULL DEFAULT 'main',
    path            TEXT NOT NULL DEFAULT '/',
    sync_interval_s INTEGER NOT NULL DEFAULT 60,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

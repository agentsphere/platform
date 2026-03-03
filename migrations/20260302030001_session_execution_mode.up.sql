-- Add execution mode to agent sessions.
ALTER TABLE agent_sessions
    ADD COLUMN execution_mode TEXT NOT NULL DEFAULT 'pod'
    CHECK (execution_mode IN ('pod', 'cli_subprocess', 'inprocess'));

-- Backfill: existing inprocess sessions.
UPDATE agent_sessions
    SET execution_mode = 'inprocess'
    WHERE pod_name IS NULL
      AND status IN ('completed', 'stopped', 'running')
      AND provider = 'inprocess';

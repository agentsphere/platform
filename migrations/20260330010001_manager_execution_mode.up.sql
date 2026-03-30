ALTER TABLE agent_sessions
  DROP CONSTRAINT IF EXISTS agent_sessions_execution_mode_check;
ALTER TABLE agent_sessions
  ADD CONSTRAINT agent_sessions_execution_mode_check
  CHECK (execution_mode IN ('pod', 'cli_subprocess', 'manager'));

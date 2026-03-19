-- Extend agent_messages.role CHECK to accept new ProgressKind variants:
-- waiting_for_input, progress_update, iframe_available, iframe_removed,
-- secret_request, unknown.
ALTER TABLE agent_messages DROP CONSTRAINT agent_messages_role_check;
ALTER TABLE agent_messages ADD CONSTRAINT agent_messages_role_check
    CHECK (role IN ('user', 'assistant', 'system', 'tool',
                    'text', 'thinking', 'tool_call', 'tool_result',
                    'milestone', 'error', 'completed',
                    'waiting_for_input', 'progress_update',
                    'iframe_available', 'iframe_removed',
                    'secret_request', 'unknown'));

import { useState, useEffect, useRef } from 'preact/hooks';
import { api } from '../lib/api';
import type { AgentSession, ProgressEvent, SecretRequestMeta } from '../lib/types';
import { timeAgo, duration } from '../lib/format';
import { Badge } from '../components/Badge';
import { StatusDot } from '../components/StatusDot';
import { SecretRequestModal } from '../components/SecretRequestModal';
import { createSse, type EventSourceClient } from '../lib/sse';

interface Props {
  id?: string;
  sessionId?: string;
}

/** Normalize event kind from backend snake_case to PascalCase. */
function normalizeKind(kind: string | undefined): ProgressEvent['kind'] {
  if (!kind) return 'Text';
  const map: Record<string, ProgressEvent['kind']> = {
    text: 'Text', thinking: 'Thinking', tool_call: 'ToolCall',
    tool_result: 'ToolResult', milestone: 'Milestone', error: 'Error',
    completed: 'Completed', secret_request: 'SecretRequest',
    // Already PascalCase — pass through
    Text: 'Text', Thinking: 'Thinking', ToolCall: 'ToolCall',
    ToolResult: 'ToolResult', Milestone: 'Milestone', Error: 'Error',
    Completed: 'Completed', SecretRequest: 'SecretRequest',
  };
  return map[kind] || 'Text';
}

export function SessionDetail({ id: projectId, sessionId }: Props) {
  const [session, setSession] = useState<AgentSession | null>(null);
  const [events, setEvents] = useState<ProgressEvent[]>([]);
  const [message, setMessage] = useState('');
  const [sending, setSending] = useState(false);
  const [secretRequest, setSecretRequest] = useState<SecretRequestMeta | null>(null);
  const eventsEndRef = useRef<HTMLDivElement>(null);
  const sseRef = useRef<EventSourceClient | null>(null);

  useEffect(() => {
    if (!projectId || !sessionId) return;
    api.get<AgentSession & { messages?: { role: string; content: string; metadata?: Record<string, any> }[] }>(
      `/api/projects/${projectId}/sessions/${sessionId}`
    ).then(data => {
      setSession(data);
      // Map session messages to ProgressEvent format
      if (data.messages) {
        setEvents(data.messages.map(m => ({
          kind: normalizeKind(m.metadata?.kind as string),
          message: m.content,
          metadata: m.metadata,
        })));
      }
    }).catch(() => {});
  }, [projectId, sessionId]);

  // SSE for live streaming
  useEffect(() => {
    if (!projectId || !sessionId || !session) return;
    if (session.status !== 'running' && session.status !== 'pending') return;

    const sse = createSse({
      url: `/api/projects/${projectId}/sessions/${sessionId}/events`,
      onMessage: (raw: Record<string, any>) => {
        const event: ProgressEvent = {
          kind: normalizeKind(raw.kind),
          message: raw.message ?? '',
          metadata: raw.metadata,
        };
        setEvents(prev => [...prev, event]);
        if (event.kind === 'SecretRequest' && event.metadata) {
          setSecretRequest({
            request_id: event.metadata.request_id,
            name: event.metadata.name,
            prompt: event.metadata.prompt || event.message,
            environments: event.metadata.environments,
          });
        }
        if (event.kind === 'Completed' || event.kind === 'Error') {
          // Refresh session to get updated status
          api.get<AgentSession>(`/api/projects/${projectId}/sessions/${sessionId}`)
            .then(setSession).catch(() => {});
        }
      },
    });
    sseRef.current = sse;
    return () => sse.close();
  }, [projectId, sessionId, session?.status]);

  // Auto-scroll
  useEffect(() => {
    eventsEndRef.current?.scrollIntoView({ behavior: 'smooth' });
  }, [events.length]);

  const sendMessage = async (e: Event) => {
    e.preventDefault();
    if (!message.trim() || sending) return;
    setSending(true);
    try {
      await api.post(`/api/projects/${projectId}/sessions/${sessionId}/message`, {
        content: message.trim(),
      });
      setMessage('');
    } catch { /* ignore */ }
    finally { setSending(false); }
  };

  const stopSession = async () => {
    if (!confirm('Stop this session?')) return;
    try {
      await api.post(`/api/projects/${projectId}/sessions/${sessionId}/stop`);
      const updated = await api.get<AgentSession>(`/api/projects/${projectId}/sessions/${sessionId}`);
      setSession(updated);
    } catch { /* ignore */ }
  };

  if (!session) return <div class="empty-state">Loading...</div>;

  const elapsed = session.status === 'running'
    ? Date.now() - new Date(session.created_at).getTime()
    : session.finished_at
      ? new Date(session.finished_at).getTime() - new Date(session.created_at).getTime()
      : 0;

  const isLive = session.status === 'running' || session.status === 'pending';

  return (
    <div>
      <div class="mb-md">
        <a href={`/projects/${projectId}/sessions`} class="text-sm text-muted">Back to sessions</a>
      </div>

      <div class="flex-between mb-md">
        <div>
          <h2>
            Session <span class="mono text-sm">{sessionId?.substring(0, 8)}...</span>
          </h2>
          <div class="flex gap-md text-sm mt-sm">
            <StatusDot status={session.status} label={session.status} />
            <span class="text-muted">{duration(elapsed)}</span>
          </div>
        </div>
        {isLive && (
          <button class="btn btn-danger" onClick={stopSession}>Stop</button>
        )}
      </div>

      <div class="session-layout">
        <div class="session-main">
          <div class="card session-events">
            <div class="session-events-scroll">
              {events.length === 0 ? (
                <div class="empty-state">
                  {isLive ? 'Waiting for events...' : 'No events recorded'}
                </div>
              ) : (
                events.map((event, i) => (
                  <div key={i} class={`session-event session-event-${(event.kind || 'text').toLowerCase()}`}>
                    <span class="session-event-icon">{getEventIcon(event.kind || 'Text')}</span>
                    <div class="session-event-content">
                      <div class="session-event-message">{event.message}</div>
                      {event.metadata && Object.keys(event.metadata).length > 0 && (
                        <details class="session-event-meta">
                          <summary class="text-xs text-muted">Details</summary>
                          <pre class="text-xs mono" style="margin-top:0.25rem;color:var(--text-secondary)">
                            {JSON.stringify(event.metadata, null, 2)}
                          </pre>
                        </details>
                      )}
                    </div>
                  </div>
                ))
              )}
              <div ref={eventsEndRef} />
            </div>

            {isLive && (
              <form class="session-input" onSubmit={sendMessage}>
                <input class="input" value={message}
                  placeholder="Send a message to the agent..."
                  onInput={(e) => setMessage((e.target as HTMLInputElement).value)}
                  disabled={sending} />
                <button class="btn btn-primary btn-sm" type="submit" disabled={sending || !message.trim()}>
                  Send
                </button>
              </form>
            )}
          </div>
        </div>

        <div class="session-sidebar">
          <div class="card">
            <div class="card-title mb-md">Session Info</div>
            <div class="session-meta-list">
              <div class="session-meta-row">
                <span class="text-muted text-sm">Prompt</span>
                <span class="text-sm">{session.prompt}</span>
              </div>
              <div class="session-meta-row">
                <span class="text-muted text-sm">Provider</span>
                <span class="text-sm">{session.provider}</span>
              </div>
              {session.branch && (
                <div class="session-meta-row">
                  <span class="text-muted text-sm">Branch</span>
                  <span class="mono text-xs">{session.branch}</span>
                </div>
              )}
              {session.pod_name && (
                <div class="session-meta-row">
                  <span class="text-muted text-sm">Pod</span>
                  <span class="mono text-xs">{session.pod_name}</span>
                </div>
              )}
              <div class="session-meta-row">
                <span class="text-muted text-sm">Tokens</span>
                <span class="text-sm">{session.cost_tokens != null ? session.cost_tokens.toLocaleString() : '--'}</span>
              </div>
              <div class="session-meta-row">
                <span class="text-muted text-sm">Created</span>
                <span class="text-sm">{timeAgo(session.created_at)}</span>
              </div>
              <div class="session-meta-row">
                <span class="text-muted text-sm">Duration</span>
                <span class="text-sm">{duration(elapsed)}</span>
              </div>
            </div>
          </div>
        </div>
      </div>

      {secretRequest && projectId && (
        <SecretRequestModal
          open={!!secretRequest}
          projectId={projectId}
          requestId={secretRequest.request_id}
          name={secretRequest.name}
          prompt={secretRequest.prompt}
          onComplete={() => {
            setSecretRequest(null);
            setEvents(prev => [...prev, {
              kind: 'Milestone',
              message: `Secret "${secretRequest.name}" provided successfully`,
            }]);
          }}
          onClose={() => setSecretRequest(null)}
        />
      )}
    </div>
  );
}

function getEventIcon(kind: string): string {
  switch (kind) {
    case 'Thinking': return '[T]';
    case 'ToolCall': return '[>]';
    case 'ToolResult': return '[<]';
    case 'Milestone': return '[+]';
    case 'Error': return '[!]';
    case 'Completed': return '[=]';
    case 'Text': return '[-]';
    case 'SecretRequest': return '[?]';
    default: return '[ ]';
  }
}

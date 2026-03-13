import { useState, useEffect, useRef } from 'preact/hooks';
import { api } from '../lib/api';
import type { AgentSession, ProgressEvent, SecretRequestMeta, IframePanel } from '../lib/types';
import { duration } from '../lib/format';
import { Badge } from '../components/Badge';
import { StatusDot } from '../components/StatusDot';
import { SecretRequestModal } from '../components/SecretRequestModal';
import { IframePreview } from '../components/IframePanel';
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
    completed: 'Completed', waiting_for_input: 'WaitingForInput',
    secret_request: 'SecretRequest',
    iframe_available: 'IframeAvailable', iframe_removed: 'IframeRemoved',
    // Already PascalCase — pass through
    Text: 'Text', Thinking: 'Thinking', ToolCall: 'ToolCall',
    ToolResult: 'ToolResult', Milestone: 'Milestone', Error: 'Error',
    Completed: 'Completed', WaitingForInput: 'WaitingForInput',
    SecretRequest: 'SecretRequest',
    IframeAvailable: 'IframeAvailable', IframeRemoved: 'IframeRemoved',
  };
  return map[kind] || 'Text';
}

export function SessionDetail({ id: projectId, sessionId }: Props) {
  const [session, setSession] = useState<AgentSession | null>(null);
  const [events, setEvents] = useState<ProgressEvent[]>([]);
  const [message, setMessage] = useState('');
  const [sending, setSending] = useState(false);
  const [secretRequest, setSecretRequest] = useState<SecretRequestMeta | null>(null);
  const [iframes, setIframes] = useState<IframePanel[]>([]);
  const eventsEndRef = useRef<HTMLDivElement>(null);
  const sseRef = useRef<EventSourceClient | null>(null);

  // Fetch iframe panels
  const refreshIframes = () => {
    if (!projectId || !sessionId) return;
    api.get<IframePanel[]>(`/api/projects/${projectId}/sessions/${sessionId}/iframes`)
      .then(setIframes)
      .catch(() => {});
  };

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
    // Also fetch initial iframes
    refreshIframes();
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
        if (event.kind === 'IframeAvailable' || event.kind === 'IframeRemoved') {
          refreshIframes();
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
  const hasIframes = iframes.length > 0;

  return (
    <div class="session-workspace">
      {/* Session bar */}
      <div class="session-bar">
        <div class="session-bar-left">
          <a href={`/projects/${projectId}/sessions`} class="text-sm text-muted">Sessions</a>
          <span class="text-muted">/</span>
          <span class="mono text-sm">{sessionId?.substring(0, 8)}</span>
        </div>
        <div class="session-bar-center">
          <StatusDot status={session.status} label={session.status} />
          <span class="text-sm text-muted">{duration(elapsed)}</span>
          {session.branch && <span class="mono text-xs text-muted">{session.branch}</span>}
          {hasIframes && <Badge label={`Preview (${iframes.length})`} />}
        </div>
        <div class="session-bar-right">
          {isLive && (
            <button class="btn btn-danger btn-sm" onClick={stopSession}>Stop</button>
          )}
        </div>
      </div>

      {/* Main workspace: events + optional iframe preview */}
      <div class={`session-panels ${hasIframes ? 'session-panels-split' : ''}`}>
        <div class="session-events-panel">
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

        {hasIframes && (
          <div class="session-iframe-panel">
            <IframePreview panels={iframes} />
          </div>
        )}
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
    case 'IframeAvailable': return '[F]';
    case 'IframeRemoved': return '[x]';
    default: return '[ ]';
  }
}

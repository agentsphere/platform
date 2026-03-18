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
    progress_update: 'ProgressUpdate',
    // Already PascalCase — pass through
    Text: 'Text', Thinking: 'Thinking', ToolCall: 'ToolCall',
    ToolResult: 'ToolResult', Milestone: 'Milestone', Error: 'Error',
    Completed: 'Completed', WaitingForInput: 'WaitingForInput',
    SecretRequest: 'SecretRequest',
    IframeAvailable: 'IframeAvailable', IframeRemoved: 'IframeRemoved',
    ProgressUpdate: 'ProgressUpdate',
  };
  return map[kind] || 'Text';
}

// Enriched event for grouping
interface EnrichedEvent extends ProgressEvent {
  toolMeta?: { name: string; summary?: string }[];
  resultMeta?: { tool_use_id: string; preview?: string }[];
}

// Grouped events for display
interface EventGroup {
  type: 'event' | 'tool_group';
  event?: EnrichedEvent;         // For 'event' type
  tools?: { name: string; summary?: string; resultPreview?: string }[]; // For 'tool_group'
}

/** Events that should not break a tool group when they appear between tool calls. */
function isNoiseEvent(ev: EnrichedEvent): boolean {
  if (ev.kind === 'Milestone' && /^Session started\b/.test(ev.message)) return true;
  return false;
}

/** Group consecutive ToolCall/ToolResult events into collapsed groups. */
function groupEvents(events: EnrichedEvent[]): EventGroup[] {
  const groups: EventGroup[] = [];
  let toolBuf: EnrichedEvent[] = [];

  function flushTools() {
    if (toolBuf.length === 0) return;
    const tools: NonNullable<EventGroup['tools']> = [];
    for (const ev of toolBuf) {
      if (ev.kind === 'ToolCall') {
        if (ev.toolMeta) {
          for (const t of ev.toolMeta) tools.push({ name: t.name, summary: t.summary });
        } else if (ev.message) {
          // Fallback: parse tool names from message (comma-separated)
          for (const name of ev.message.split(',').map(s => s.trim()).filter(Boolean)) {
            tools.push({ name });
          }
        }
      } else if (ev.kind === 'ToolResult' && ev.resultMeta) {
        for (const r of ev.resultMeta) {
          const existing = tools.find(t => !t.resultPreview);
          if (existing) existing.resultPreview = r.preview;
        }
      }
    }
    if (tools.length > 0) {
      groups.push({ type: 'tool_group', tools });
    }
    toolBuf = [];
  }

  for (const ev of events) {
    if (ev.kind === 'ToolCall' || ev.kind === 'ToolResult') {
      toolBuf.push(ev);
    } else if (toolBuf.length > 0 && isNoiseEvent(ev)) {
      // Absorb noise events (e.g. child "Session started") into tool group
      toolBuf.push(ev);
    } else {
      flushTools();
      groups.push({ type: 'event', event: ev });
    }
  }
  flushTools();
  return groups;
}

/** Render simple markdown: headings, checkboxes, bold, lists */
function SimpleMarkdown({ content }: { content: string }) {
  const lines = content.split('\n');
  const elements: any[] = [];
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    if (line.startsWith('## ')) {
      elements.push(<div key={i} style="font-weight:600;margin:0.5rem 0 0.25rem;font-size:13px;color:var(--text-primary)">{line.slice(3)}</div>);
    } else if (line.startsWith('# ')) {
      elements.push(<div key={i} style="font-weight:700;margin:0.5rem 0 0.25rem;font-size:14px;color:var(--text-primary)">{line.slice(2)}</div>);
    } else if (/^- \[x\] /.test(line)) {
      elements.push(<div key={i} style="font-size:12px;padding:0.1rem 0;color:var(--text-secondary)"><span style="color:var(--success)">&#x2705;</span> <s>{line.slice(6)}</s></div>);
    } else if (/^- \[ \] /.test(line)) {
      elements.push(<div key={i} style="font-size:12px;padding:0.1rem 0;color:var(--text-primary)"><span style="opacity:0.4">&#x2B1C;</span> {line.slice(6)}</div>);
    } else if (/^- \S/.test(line)) {
      const isActive = /^- 🔄/.test(line);
      elements.push(<div key={i} style={`font-size:12px;padding:0.1rem 0;color:${isActive ? 'var(--accent)' : 'var(--text-secondary)'}`}>&bull; {line.slice(2)}</div>);
    } else if (line.trim() === '') {
      elements.push(<div key={i} style="height:0.25rem" />);
    } else {
      elements.push(<div key={i} style="font-size:12px;color:var(--text-secondary);padding:0.1rem 0">{line}</div>);
    }
  }
  return <>{elements}</>;
}

function ToolGroupRow({ tools, expanded, onToggle }: { tools: NonNullable<EventGroup['tools']>; expanded: boolean; onToggle: () => void }) {
  return (
    <div class="tool-group" onClick={onToggle}>
      {expanded ? (
        <div class="tool-group-expanded">
          {tools.map((t, i) => (
            <div key={i} class="tool-group-item">
              <span class="tool-group-name">{t.name}</span>
              {t.summary && <span class="tool-group-summary-text">{t.summary}</span>}
            </div>
          ))}
        </div>
      ) : (
        <span class="tool-group-collapsed">{'\u2504'} {tools.length} tool call{tools.length !== 1 ? 's' : ''} {'\u2504'}</span>
      )}
    </div>
  );
}

export function SessionDetail({ id: projectId, sessionId }: Props) {
  const [session, setSession] = useState<AgentSession | null>(null);
  const [events, setEvents] = useState<EnrichedEvent[]>([]);
  const [message, setMessage] = useState('');
  const [sending, setSending] = useState(false);
  const [secretRequest, setSecretRequest] = useState<SecretRequestMeta | null>(null);
  const [iframes, setIframes] = useState<IframePanel[]>([]);
  const [latestProgress, setLatestProgress] = useState<string | null>(null);
  const [expandedGroups, setExpandedGroups] = useState<Set<number>>(new Set());
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
      // Map session messages to EnrichedEvent format
      if (data.messages) {
        const mapped: EnrichedEvent[] = [];
        let lastProgress: string | null = null;
        for (const m of data.messages) {
          const kind = normalizeKind(m.role || m.metadata?.kind as string);
          if (kind === 'ProgressUpdate') {
            lastProgress = m.content;
            continue; // Don't show progress_update as chat events
          }
          const ev: EnrichedEvent = {
            kind,
            message: m.content,
            metadata: m.metadata,
          };
          if (kind === 'ToolCall' && m.metadata?.tools) {
            ev.toolMeta = m.metadata.tools;
          }
          if (kind === 'ToolResult' && m.metadata?.results) {
            ev.resultMeta = m.metadata.results;
          }
          mapped.push(ev);
        }
        setEvents(mapped);
        if (lastProgress) setLatestProgress(lastProgress);
      }
    }).catch(() => {});
    // Also fetch progress from dedicated endpoint (works even for completed sessions)
    api.get<{ message: string }>(`/api/projects/${projectId}/sessions/${sessionId}/progress`)
      .then(r => { if (r.message) setLatestProgress(r.message); })
      .catch(() => {}); // 404 if no progress stored
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
        const kind = normalizeKind(raw.kind);

        // Progress updates go to sidebar, not to event list
        if (kind === 'ProgressUpdate') {
          setLatestProgress(raw.message ?? '');
          return;
        }

        const event: EnrichedEvent = {
          kind,
          message: raw.message ?? '',
          metadata: raw.metadata,
        };
        if (kind === 'ToolCall' && raw.metadata?.tools) {
          event.toolMeta = raw.metadata.tools;
        }
        if (kind === 'ToolResult' && raw.metadata?.results) {
          event.resultMeta = raw.metadata.results;
        }

        setEvents(prev => [...prev, event]);
        if (kind === 'SecretRequest' && raw.metadata) {
          setSecretRequest({
            request_id: raw.metadata.request_id,
            name: raw.metadata.name,
            prompt: raw.metadata.prompt || raw.message,
            environments: raw.metadata.environments,
          });
        }
        if (kind === 'IframeAvailable' || kind === 'IframeRemoved') {
          refreshIframes();
        }
        if (kind === 'Completed' || kind === 'Error') {
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

  function toggleGroup(idx: number) {
    setExpandedGroups(prev => {
      const next = new Set(prev);
      if (next.has(idx)) next.delete(idx);
      else next.add(idx);
      return next;
    });
  }

  if (!session) return <div class="empty-state">Loading...</div>;

  const elapsed = session.status === 'running'
    ? Date.now() - new Date(session.created_at).getTime()
    : session.finished_at
      ? new Date(session.finished_at).getTime() - new Date(session.created_at).getTime()
      : 0;

  const isLive = session.status === 'running' || session.status === 'pending';
  const hasIframes = iframes.length > 0;
  const grouped = groupEvents(events);

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

      {/* Main workspace: events + optional iframe/progress panels */}
      <div class={`session-panels ${hasIframes ? 'session-panels-split' : ''}`}>
        <div class="session-events-panel">
          <div class="card session-events">
            <div class="session-detail-body">
              <div class="session-events-scroll">
                {events.length === 0 ? (
                  <div class="empty-state">
                    {isLive ? 'Waiting for events...' : 'No events recorded'}
                  </div>
                ) : (
                  grouped.map((group, gi) => {
                    if (group.type === 'tool_group' && group.tools) {
                      return (
                        <ToolGroupRow
                          key={`tg-${gi}`}
                          tools={group.tools}
                          expanded={expandedGroups.has(gi)}
                          onToggle={() => toggleGroup(gi)}
                        />
                      );
                    }
                    const event = group.event!;
                    return (
                      <div key={gi} class={`session-event session-event-${(event.kind || 'text').toLowerCase()}`}>
                        <span class="session-event-icon">{getEventIcon(event.kind || 'Text')}</span>
                        <div class="session-event-content">
                          <div class="session-event-message">{event.message}</div>
                        </div>
                      </div>
                    );
                  })
                )}
                <div ref={eventsEndRef} />
              </div>

              {latestProgress && (
                <div class="session-detail-progress">
                  <div class="session-detail-progress-header">Progress</div>
                  <div class="session-detail-progress-content">
                    <SimpleMarkdown content={latestProgress} />
                  </div>
                </div>
              )}
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

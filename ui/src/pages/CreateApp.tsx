import { useState, useRef, useEffect } from 'preact/hooks';
import { api } from '../lib/api';
import { createSse, type EventSourceClient } from '../lib/sse';
import { AgentBar, type AgentInfo, getAgentColor } from '../components/AgentBar';
import { ReplyBanner } from '../components/ReplyBanner';

interface ChatMessage {
  role: 'user' | 'assistant' | 'system' | 'thinking';
  content: string;
  sessionId?: string;
}

interface SessionInfo {
  id: string;
  status: string;
  project_id?: string;
}

interface ProgressEvent {
  kind: string;
  message: string;
  session_id?: string;
  metadata?: Record<string, unknown>;
}

export function CreateApp() {
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [input, setInput] = useState('');
  const [session, setSession] = useState<SessionInfo | null>(null);
  const [loading, setLoading] = useState(false);
  const [connected, setConnected] = useState(false);
  const [streaming, setStreaming] = useState(false);
  const [agents, setAgents] = useState<Map<string, AgentInfo>>(new Map());
  const [replyTarget, setReplyTarget] = useState<string | null>(null);
  const messagesEnd = useRef<HTMLDivElement>(null);
  const sseRef = useRef<EventSourceClient | null>(null);
  // Per-agent stream buffers keyed by session_id
  const streamBufs = useRef<Map<string, string>>(new Map());

  // Auto-scroll on new messages
  useEffect(() => {
    messagesEnd.current?.scrollIntoView({ behavior: 'smooth' });
  }, [messages]);

  // Cleanup SSE on unmount
  useEffect(() => {
    return () => { sseRef.current?.close(); };
  }, []);

  function getStreamBuf(sessionId: string): string {
    return streamBufs.current.get(sessionId) || '';
  }

  function setStreamBuf(sessionId: string, value: string) {
    streamBufs.current.set(sessionId, value);
  }

  function getAgentLabel(sessionId: string, parentId: string): string {
    if (sessionId === parentId) return 'Manager';
    const agent = agents.get(sessionId);
    return agent?.label || sessionId.slice(0, 8);
  }

  function getAgentColorForSession(sessionId: string, parentId: string): string {
    if (sessionId === parentId) return getAgentColor(0);
    const entries = Array.from(agents.keys());
    const idx = entries.indexOf(sessionId);
    return getAgentColor(idx >= 0 ? idx + 1 : 0);
  }

  function addAgent(sessionId: string, prompt: string, status: AgentInfo['status'] = 'pending') {
    setAgents(prev => {
      if (prev.has(sessionId)) return prev;
      const next = new Map(prev);
      const label = prompt.length > 30 ? prompt.slice(0, 30) + '...' : prompt;
      next.set(sessionId, { sessionId, label, status });
      return next;
    });
  }

  function updateAgentStatus(sessionId: string, status: AgentInfo['status']) {
    setAgents(prev => {
      const agent = prev.get(sessionId);
      if (!agent) return prev;
      const next = new Map(prev);
      next.set(sessionId, { ...agent, status });
      return next;
    });
  }

  function connectSse(sessionId: string) {
    const sse = createSse({
      url: `/api/sessions/${sessionId}/events?include_children=true`,
      onOpen() {
        setConnected(true);
      },
      onError() {
        setConnected(false);
        setStreaming(false);
      },
      onMessage(data: ProgressEvent) {
        const sid = data.session_id || sessionId;

        // Handle milestone events for child lifecycle
        if (data.kind === 'milestone' && data.metadata) {
          if (data.metadata.event_type === 'child_spawned') {
            const childId = data.metadata.child_session_id as string;
            const childPrompt = (data.metadata.child_prompt as string) || '';
            addAgent(childId, childPrompt, 'running');
            setMessages(prev => [...prev, {
              role: 'system',
              content: `Agent spawned: ${childPrompt || childId.slice(0, 8)}`,
              sessionId: sid,
            }]);
            return;
          }
          if (data.metadata.event_type === 'child_completion') {
            const childId = data.metadata.child_session_id as string;
            const childStatus = (data.metadata.child_status as string) || 'completed';
            updateAgentStatus(childId, childStatus as AgentInfo['status']);
            setMessages(prev => [...prev, {
              role: 'system',
              content: `Agent ${childId.slice(0, 8)} ${childStatus}`,
              sessionId: sid,
            }]);
            return;
          }
        }

        switch (data.kind) {
          case 'text': {
            const buf = getStreamBuf(sid) + data.message;
            setStreamBuf(sid, buf);
            // Update agent status to running on first text
            if (sid !== sessionId) updateAgentStatus(sid, 'running');
            setMessages(prev => {
              const last = prev[prev.length - 1];
              if (last && last.role === 'assistant' && last.sessionId === sid) {
                return [...prev.slice(0, -1), { role: 'assistant', content: buf, sessionId: sid }];
              }
              return [...prev, { role: 'assistant', content: buf, sessionId: sid }];
            });
            break;
          }
          case 'thinking': {
            setMessages(prev => {
              const last = prev[prev.length - 1];
              if (last && last.role === 'thinking' && last.sessionId === sid) {
                return [...prev.slice(0, -1), { role: 'thinking', content: last.content + data.message, sessionId: sid }];
              }
              return [...prev, { role: 'thinking', content: data.message, sessionId: sid }];
            });
            break;
          }
          case 'completed': {
            setStreamBuf(sid, '');
            if (sid === sessionId) {
              // Parent completed — stop streaming
              setStreaming(false);
            } else {
              updateAgentStatus(sid, 'completed');
            }
            break;
          }
          case 'tool_call': {
            setMessages(prev => [...prev, { role: 'system', content: `Setting up: ${data.message}...`, sessionId: sid }]);
            break;
          }
          case 'tool_result': {
            const isError = data.metadata?.is_error;
            setMessages(prev => [...prev, {
              role: 'system',
              content: isError ? `Error: ${data.message}` : data.message,
              sessionId: sid,
            }]);
            if (data.metadata?.tool_name === 'create_project' && !isError && data.metadata?.result) {
              try {
                const result = JSON.parse(data.metadata.result as string);
                if (result.project_id) {
                  setSession(prev => prev ? { ...prev, project_id: result.project_id } : prev);
                }
              } catch { /* ignore parse errors */ }
            }
            break;
          }
          case 'error': {
            setStreamBuf(sid, '');
            if (sid === sessionId) {
              setStreaming(false);
            } else {
              updateAgentStatus(sid, 'failed');
            }
            setMessages(prev => [...prev, { role: 'system', content: `Error: ${data.message}`, sessionId: sid }]);
            break;
          }
          default:
            break;
        }
      },
    });
    sseRef.current = sse;
  }

  async function handleSubmit(e: Event) {
    e.preventDefault();
    if (!input.trim()) return;

    const userMsg = input.trim();
    setInput('');

    if (!session) {
      setLoading(true);
      setMessages(prev => [...prev, { role: 'user', content: userMsg }]);

      try {
        const resp = await api.post<SessionInfo>('/api/create-app', {
          description: userMsg,
        });
        setSession(resp);
        setStreaming(true);
        streamBufs.current.clear();
        connectSse(resp.id);
      } catch (err: unknown) {
        const msg = err instanceof Error ? err.message : 'Failed to create session';
        setMessages(prev => [...prev, { role: 'system', content: `Error: ${msg}` }]);
      } finally {
        setLoading(false);
      }
    } else {
      // Send to reply target or parent
      const targetId = replyTarget || session.id;
      setMessages(prev => [...prev, { role: 'user', content: userMsg, sessionId: targetId }]);
      setStreamBuf(targetId, '');
      setStreaming(true);
      setReplyTarget(null);
      try {
        await api.post(`/api/sessions/${targetId}/message`, { content: userMsg });
      } catch {
        setMessages(prev => [...prev, { role: 'system', content: 'Failed to send message' }]);
        setStreaming(false);
      }
    }
  }

  function handleMessageClick(msg: ChatMessage) {
    if (msg.role === 'user' || !msg.sessionId || !session) return;
    setReplyTarget(msg.sessionId);
  }

  const replyAgent = replyTarget ? agents.get(replyTarget) : null;
  const replyLabel = replyTarget && session
    ? replyTarget === session.id ? 'Manager' : (replyAgent?.label || replyTarget.slice(0, 8))
    : '';
  const replyColor = replyTarget && session
    ? getAgentColorForSession(replyTarget, session.id)
    : '';

  return (
    <div style="display:flex;flex-direction:column;height:calc(100vh - 4rem)">
      <div style="padding:1rem 0;border-bottom:1px solid var(--border)">
        <h2 style="margin:0">Create New App</h2>
        <p class="text-muted text-sm" style="margin:0.25rem 0 0">
          Describe what you want to build. An AI agent will set up the project, pipeline, and infrastructure.
        </p>
      </div>

      {/* Messages area */}
      <div style="flex:1;overflow-y:auto;padding:1rem 0">
        {messages.length === 0 && (
          <div style="text-align:center;padding:3rem;color:var(--text-muted)">
            <p style="font-size:1.1rem;margin-bottom:0.5rem">What would you like to build?</p>
            <p class="text-sm">Examples: "A REST API with auth and a Postgres database", "A static blog with markdown", "A microservice in Go"</p>
          </div>
        )}
        {messages.map((msg, i) => {
          const isClickable = msg.role !== 'user' && !!msg.sessionId && !!session;
          const agentColor = msg.sessionId && session
            ? getAgentColorForSession(msg.sessionId, session.id)
            : undefined;

          return (
            <div
              key={i}
              style={msgStyle(msg.role, agentColor)}
              class={isClickable ? 'chat-msg-clickable' : ''}
              onClick={() => handleMessageClick(msg)}
            >
              <div class="chat-agent-name" style={agentColor ? `color: ${agentColor}` : ''}>
                {msg.role === 'user'
                  ? (msg.sessionId && session && msg.sessionId !== session.id
                    ? `You → ${getAgentLabel(msg.sessionId, session.id)}`
                    : 'You')
                  : msg.role === 'thinking'
                    ? `${msg.sessionId && session ? getAgentLabel(msg.sessionId, session.id) : 'Agent'} thinking...`
                    : msg.role === 'system'
                      ? (msg.sessionId && session ? getAgentLabel(msg.sessionId, session.id) : 'System')
                      : (msg.sessionId && session ? getAgentLabel(msg.sessionId, session.id) : 'Agent')}
              </div>
              <div style={msg.role === 'thinking' ? 'white-space:pre-wrap;font-style:italic;opacity:0.7' : 'white-space:pre-wrap'}>
                {msg.content}
              </div>
            </div>
          );
        })}
        {streaming && (
          <div style="padding:0.5rem 1rem;color:var(--text-muted)">
            <span class="typing-indicator">&#9679; &#9679; &#9679;</span>
          </div>
        )}
        {loading && (
          <div style="padding:0.5rem 1rem;color:var(--text-muted);font-style:italic">
            Creating session...
          </div>
        )}
        <div ref={messagesEnd} />
      </div>

      {/* Agent bar */}
      {session && (
        <AgentBar
          agents={agents}
          parentSessionId={session.id}
          replyTarget={replyTarget}
          onSelectAgent={(id) => setReplyTarget(id === session.id ? null : id)}
        />
      )}

      {/* Reply banner */}
      {replyTarget && session && replyTarget !== session.id && (
        <ReplyBanner
          agentLabel={replyLabel}
          agentColor={replyColor}
          onDismiss={() => setReplyTarget(null)}
        />
      )}

      {/* Input area */}
      <form onSubmit={handleSubmit} style="display:flex;gap:0.5rem;padding:1rem 0;border-top:1px solid var(--border)">
        <input
          type="text"
          class="input"
          style="flex:1"
          placeholder={
            replyTarget && session && replyTarget !== session.id
              ? `Reply to ${replyLabel}...`
              : session ? 'Send a follow-up message...' : 'Describe your app idea...'
          }
          value={input}
          onInput={(e) => setInput((e.target as HTMLInputElement).value)}
          disabled={loading || streaming}
          autoFocus
        />
        <button type="submit" class="btn btn-primary" disabled={loading || streaming || !input.trim()}>
          {session ? 'Send' : 'Create'}
        </button>
      </form>

      {/* Session info */}
      {session && (
        <div class="text-sm text-muted" style="padding-bottom:0.5rem">
          Session: {session.id.slice(0, 8)} | Status: {session.status}
          {connected && <span style="color:var(--green, #22c55e)"> ● Connected</span>}
          {!connected && <span style="color:var(--text-muted)"> ○ Disconnected</span>}
          {session.project_id && (
            <span> | <a href={`/projects/${session.project_id}`}>View Project</a></span>
          )}
        </div>
      )}
    </div>
  );
}

function msgStyle(role: string, agentColor?: string): Record<string, string> {
  const base: Record<string, string> = { padding: '0.75rem 1rem', 'margin-bottom': '0.5rem', 'border-radius': '0.5rem' };
  if (role === 'user') {
    return { ...base, background: 'var(--bg-secondary)', 'margin-left': '2rem' };
  }
  if (role === 'assistant') {
    return {
      ...base,
      background: 'var(--bg-tertiary, var(--bg-secondary))',
      'margin-right': '2rem',
      ...(agentColor ? { 'border-left': `3px solid ${agentColor}` } : {}),
    };
  }
  if (role === 'thinking') {
    return { ...base, background: 'transparent', 'margin-right': '2rem', 'border-left': `3px solid ${agentColor || 'var(--text-muted)'}`, 'padding-left': '1rem' };
  }
  return { ...base, color: 'var(--text-muted)', 'font-style': 'italic', 'font-size': '0.85rem' };
}

import { useState, useRef, useEffect } from 'preact/hooks';
import { useAuth } from '../lib/auth';
import { api } from '../lib/api';
import { createSse, type EventSourceClient } from '../lib/sse';
import { AgentBar, type AgentInfo, getAgentColor } from '../components/AgentBar';
import { ReplyBanner } from '../components/ReplyBanner';

interface ChatMessage {
  role: 'user' | 'assistant' | 'system' | 'thinking';
  content: string;
  sessionId?: string;
  kind?: string;
  toolMeta?: { name: string; summary?: string }[];
  resultMeta?: { tool_use_id: string; preview?: string }[];
}

interface MessageGroup {
  type: 'message' | 'tool_group';
  messages?: ChatMessage[];
  tools?: { name: string; summary?: string; resultPreview?: string }[];
  sessionId?: string;
}

function isNoiseMessage(msg: ChatMessage): boolean {
  if (msg.role === 'system' && /^Session started\b/.test(msg.content)) return true;
  return false;
}

function groupMessages(msgs: ChatMessage[]): MessageGroup[] {
  const groups: MessageGroup[] = [];
  let toolBuf: ChatMessage[] = [];

  function flushTools() {
    if (toolBuf.length === 0) return;
    const tools: NonNullable<MessageGroup['tools']> = [];
    for (const m of toolBuf) {
      if (m.kind === 'tool_call' && m.toolMeta) {
        for (const t of m.toolMeta) tools.push({ name: t.name, summary: t.summary });
      } else if (m.kind === 'tool_result' && m.resultMeta) {
        for (const r of m.resultMeta) {
          const existing = tools.find(t => !t.resultPreview);
          if (existing) existing.resultPreview = r.preview;
        }
      }
    }
    if (tools.length > 0) {
      groups.push({ type: 'tool_group', tools, sessionId: toolBuf[0].sessionId });
    }
    toolBuf = [];
  }

  for (const msg of msgs) {
    if (msg.kind === 'tool_call' || msg.kind === 'tool_result') {
      toolBuf.push(msg);
    } else if (toolBuf.length > 0 && isNoiseMessage(msg)) {
      toolBuf.push(msg);
    } else {
      flushTools();
      groups.push({ type: 'message', messages: [msg] });
    }
  }
  flushTools();
  return groups;
}

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
      const isActive = /^- /.test(line);
      elements.push(<div key={i} style="font-size:12px;padding:0.1rem 0;color:var(--text-secondary)">&bull; {line.slice(2)}</div>);
    } else if (line.trim() === '') {
      elements.push(<div key={i} style="height:0.25rem" />);
    } else {
      elements.push(<div key={i} style="font-size:12px;color:var(--text-secondary);padding:0.1rem 0">{line}</div>);
    }
  }
  return <>{elements}</>;
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
  const { user } = useAuth();
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [input, setInput] = useState('');
  const [session, setSession] = useState<SessionInfo | null>(null);
  const [loading, setLoading] = useState(false);
  const [connected, setConnected] = useState(false);
  const [streaming, setStreaming] = useState(false);
  const [agents, setAgents] = useState<Map<string, AgentInfo>>(new Map());
  const [replyTarget, setReplyTarget] = useState<string | null>(null);
  const [latestProgress, setLatestProgress] = useState<string | null>(null);
  const [expandedGroups, setExpandedGroups] = useState<Set<number>>(new Set());
  const [showHero, setShowHero] = useState(true);
  const messagesEnd = useRef<HTMLDivElement>(null);
  const sseRef = useRef<EventSourceClient | null>(null);
  // Per-agent stream buffers keyed by session_id
  const streamBufs = useRef<Map<string, string>>(new Map());
  const autoSubmitted = useRef(false);

  // Auto-scroll on new messages
  useEffect(() => {
    messagesEnd.current?.scrollIntoView({ behavior: 'smooth' });
  }, [messages]);

  // Cleanup SSE on unmount
  useEffect(() => {
    return () => { sseRef.current?.close(); };
  }, []);

  // Check for prompt query param (from Dashboard hero)
  useEffect(() => {
    if (autoSubmitted.current) return;
    const params = new URLSearchParams(window.location.search);
    const prompt = params.get('prompt');
    if (prompt) {
      autoSubmitted.current = true;
      setShowHero(false);
      setInput(prompt);
      // Auto-submit after a tick
      setTimeout(() => {
        submitPrompt(prompt);
      }, 0);
    }
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
          case 'waiting_for_input': {
            setStreamBuf(sid, '');
            if (sid === sessionId) {
              // Clarification phase — re-enable input for user reply
              setStreaming(false);
            }
            break;
          }
          case 'tool_call': {
            const toolMeta = data.metadata?.tools as { name: string; summary?: string }[] | undefined;
            setMessages(prev => [...prev, { role: 'system', content: `Setting up: ${data.message}...`, sessionId: sid, kind: 'tool_call', toolMeta }]);
            break;
          }
          case 'tool_result': {
            const isError = data.metadata?.is_error;
            const resultMeta = data.metadata?.results as { tool_use_id: string; preview?: string }[] | undefined;
            setMessages(prev => [...prev, {
              role: 'system',
              content: isError ? `Error: ${data.message}` : data.message,
              sessionId: sid,
              kind: 'tool_result',
              resultMeta,
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
          case 'progress_update': {
            setLatestProgress(data.message);
            break;
          }
          default:
            break;
        }
      },
    });
    sseRef.current = sse;
  }

  async function submitPrompt(prompt: string) {
    if (!prompt.trim()) return;
    const userMsg = prompt.trim();

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

  async function handleSubmit(e: Event) {
    e.preventDefault();
    if (!input.trim()) return;
    const userMsg = input.trim();
    setInput('');
    setShowHero(false);
    submitPrompt(userMsg);
  }

  function handleHeroSubmit(prompt: string) {
    setShowHero(false);
    setInput('');
    submitPrompt(prompt);
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

  const grouped = groupMessages(messages);

  function toggleGroup(idx: number) {
    setExpandedGroups(prev => {
      const next = new Set(prev);
      if (next.has(idx)) next.delete(idx);
      else next.add(idx);
      return next;
    });
  }

  const displayName = user?.display_name || user?.name || 'there';
  const heroTemplates = [
    { label: 'REST API + Postgres', prompt: 'Create a REST API with Postgres database, auth, and CRUD endpoints' },
    { label: 'Static Site', prompt: 'Create a static website with Markdown content' },
    { label: 'Full-Stack App', prompt: 'Create a full-stack web app with React frontend and API backend' },
  ];

  // Hero entry state — before chat begins
  if (showHero && !session && messages.length === 0) {
    return (
      <div style="position:relative;min-height:calc(100vh - 6rem)">
        <div class="aurora-bg">
          <div class="aurora-blob-3" />
        </div>
        <div class="hero-container">
          <h1 class="hero-greeting">Hey {displayName}, what will you build?</h1>

          <form onSubmit={handleSubmit} style="width:100%;max-width:560px;display:flex;gap:0.5rem">
            <input
              type="text"
              class="hero-chat-input"
              placeholder="Describe your app idea..."
              value={input}
              onInput={(e) => setInput((e.target as HTMLInputElement).value)}
              autoFocus
            />
            <button type="submit" class="btn btn-primary" style="border-radius:12px;padding:0.9rem 1.5rem" disabled={!input.trim()}>
              Create
            </button>
          </form>

          <div class="hero-options">
            <div class="hero-option-card" onClick={() => handleHeroSubmit('Import my existing repository from GitHub')}>
              <div class="hero-option-title">Import from GitHub</div>
              <div class="hero-option-desc">Bring an existing repo to the platform</div>
            </div>
            <div class="hero-option-card" onClick={() => {/* expand templates below */}}>
              <div class="hero-option-title">Start from Template</div>
              <div class="hero-option-desc">Pick a starter to get going fast</div>
            </div>
          </div>

          <div class="hero-templates">
            {heroTemplates.map(t => (
              <button key={t.label} class="hero-template-chip" onClick={() => handleHeroSubmit(t.prompt)}>
                {t.label}
              </button>
            ))}
          </div>
        </div>
      </div>
    );
  }

  return (
    <div style="display:flex;flex-direction:column;height:calc(100vh - 4rem)">
      <div style="padding:1rem 0;border-bottom:1px solid var(--border)">
        <h2 style="margin:0">Create New App</h2>
        <p class="text-muted text-sm" style="margin:0.25rem 0 0">
          Describe what you want to build. An AI agent will set up the project, pipeline, and infrastructure.
        </p>
      </div>

      {/* Main content area: chat + optional progress sidebar */}
      <div class="create-app-body">
        {/* Messages area */}
        <div style="flex:1;overflow-y:auto;padding:1rem 0">
          {grouped.map((group, gi) => {
            if (group.type === 'tool_group' && group.tools) {
              const agentColor = group.sessionId && session
                ? getAgentColorForSession(group.sessionId, session.id)
                : undefined;
              return (
                <div key={`tg-${gi}`} class="tool-group" style={agentColor ? { 'border-left-color': agentColor } : {}} onClick={() => toggleGroup(gi)}>
                  {expandedGroups.has(gi) ? (
                    <div class="tool-group-expanded">
                      {group.tools.map((t, i) => (
                        <div key={i} class="tool-group-item">
                          <span class="tool-group-name">{t.name}</span>
                          {t.summary && <span class="tool-group-summary-text">{t.summary}</span>}
                        </div>
                      ))}
                    </div>
                  ) : (
                    <span class="tool-group-collapsed">&#x2504; {group.tools.length} tool call{group.tools.length !== 1 ? 's' : ''} &#x2504;</span>
                  )}
                </div>
              );
            }
            const msg = group.messages![0];
            const isClickable = msg.role !== 'user' && !!msg.sessionId && !!session;
            const agentColor = msg.sessionId && session
              ? getAgentColorForSession(msg.sessionId, session.id)
              : undefined;

            return (
              <div
                key={gi}
                style={msgStyle(msg.role, agentColor)}
                class={isClickable ? 'chat-msg-clickable' : ''}
                onClick={() => handleMessageClick(msg)}
              >
                <div class="chat-agent-name" style={agentColor ? `color: ${agentColor}` : ''}>
                  {msg.role === 'user'
                    ? (msg.sessionId && session && msg.sessionId !== session.id
                      ? `You -> ${getAgentLabel(msg.sessionId, session.id)}`
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

        {/* Progress sidebar */}
        {latestProgress && (
          <div class="create-app-progress">
            <div class="agent-chat-progress-header">Progress</div>
            <div class="agent-chat-progress-content">
              <SimpleMarkdown content={latestProgress} />
            </div>
          </div>
        )}
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

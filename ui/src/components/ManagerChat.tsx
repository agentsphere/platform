import { useState, useEffect, useRef } from 'preact/hooks';
import { api } from '../lib/api';

interface ManagerSession {
  id: string;
  title: string;
  status: string;
  mode: string;
  messages: ChatMessage[];
}

interface ChatMessage {
  id: string;
  role: 'user' | 'assistant' | 'system';
  content: string;
  timestamp: number;
}

const MODES = [
  { key: 'plan', icon: '🔒', label: 'Plan' },
  { key: 'guided', icon: '🔓', label: 'Guided' },
  { key: 'auto_read', icon: '📖', label: 'Auto Read' },
  { key: 'auto_write', icon: '✏️', label: 'Auto Write' },
  { key: 'full_auto', icon: '⚡', label: 'Full Auto' },
];

export function ManagerChat() {
  const [isOpen, setIsOpen] = useState(true);
  const [sessions, setSessions] = useState<ManagerSession[]>([]);
  const [activeIdx, setActiveIdx] = useState(0);
  const [input, setInput] = useState('');
  const [sending, setSending] = useState(false);
  const [modeOpen, setModeOpen] = useState(false);
  const messagesRef = useRef<HTMLDivElement>(null);
  const chatRef = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState<{ x: number; y: number; w: number; h: number } | null>(null);
  const dragging = useRef(false);
  const dragOffset = useRef({ x: 0, y: 0 });

  // Drag handlers — capture current size so resize isn't lost
  const onDragStart = (e: MouseEvent) => {
    if ((e.target as HTMLElement).closest('button, input, a, .manager-mode-dropdown')) return;
    dragging.current = true;
    const rect = chatRef.current?.getBoundingClientRect();
    if (rect) {
      dragOffset.current = { x: e.clientX - rect.left, y: e.clientY - rect.top };
    }
    document.addEventListener('mousemove', onDragMove);
    document.addEventListener('mouseup', onDragEnd);
    e.preventDefault();
  };

  const onDragMove = (e: MouseEvent) => {
    if (!dragging.current) return;
    const rect = chatRef.current?.getBoundingClientRect();
    const w = rect?.width || 800;
    const h = rect?.height || 580;
    const x = e.clientX - dragOffset.current.x;
    const y = e.clientY - dragOffset.current.y;
    setPos({ x, y, w, h });
  };

  const onDragEnd = () => {
    dragging.current = false;
    document.removeEventListener('mousemove', onDragMove);
    document.removeEventListener('mouseup', onDragEnd);
  };
  const eventSourceRef = useRef<EventSource | null>(null);

  const active = sessions[activeIdx] || null;

  // Load sessions from localStorage on mount
  useEffect(() => {
    const stored = localStorage.getItem('manager_sessions');
    if (stored) {
      try {
        const parsed = JSON.parse(stored);
        setSessions(parsed.map((s: any) => ({ ...s, messages: s.messages || [] })));
      } catch {}
    }
  }, []);

  // Persist sessions to localStorage
  useEffect(() => {
    if (sessions.length > 0) {
      localStorage.setItem('manager_sessions', JSON.stringify(
        sessions.map(s => ({ id: s.id, title: s.title, status: s.status, mode: s.mode, messages: s.messages.slice(-50) }))
      ));
    }
  }, [sessions]);

  // Connect SSE when active session changes
  useEffect(() => {
    if (eventSourceRef.current) {
      eventSourceRef.current.close();
      eventSourceRef.current = null;
    }
    if (!active || active.status !== 'running') return;

    const sessionId = active.id;
    const seenContent = new Set<string>();

    // Pre-seed dedup set with existing messages so replay doesn't duplicate them
    const existing = sessions.find(s => s.id === sessionId);
    if (existing) {
      for (const m of existing.messages) {
        seenContent.add(`${m.role}:${m.content.slice(0, 100)}`);
      }
    }
    const es = new EventSource(`/api/manager/sessions/${sessionId}/events`);
    eventSourceRef.current = es;

    es.addEventListener('progress', (event: any) => {
      try {
        const data = JSON.parse(event.data);
        const kind = data.kind || '';
        const content = data.message || '';

        if (!content) return;

        // Handle confirmation_needed events (SecretRequest kind with confirmation metadata)
        if (data.metadata?.type === 'confirmation_needed') {
          const actionHash = data.metadata.action_hash;
          const summary = data.metadata.summary || data.message;
          const toolName = data.metadata.tool || '';
          addMessage(sessionId, {
            id: crypto.randomUUID(),
            role: 'system',
            content: `__CONFIRM__${actionHash}__${toolName}__${summary}`,
            timestamp: Date.now(),
          });
          return;
        }

        // Only show assistant text responses and errors
        // Skip: user (shown locally), thinking, milestone, completed, tool_call, tool_result
        let role: 'user' | 'assistant' | 'system' | null = null;
        if (kind === 'text' && data.metadata?.role === 'user') {
          return; // User messages from DB replay — skip
        } else if (kind === 'text' && !data.metadata?.role) {
          role = 'assistant';
        } else if (kind === 'error') {
          role = 'system';
        }

        if (!role) return;

        // Deduplicate (replay + live can both deliver the same message)
        const contentKey = `${role}:${content.slice(0, 100)}`;
        if (seenContent.has(contentKey)) return;
        seenContent.add(contentKey);

        addMessage(sessionId, { id: crypto.randomUUID(), role, content, timestamp: Date.now() });
      } catch {}
    });

    es.onerror = () => {
      // Reconnect handled by browser EventSource
    };

    return () => es.close();
  }, [active?.id, active?.status]);

  // Auto-scroll
  useEffect(() => {
    if (messagesRef.current) {
      messagesRef.current.scrollTop = messagesRef.current.scrollHeight;
    }
  }, [active?.messages?.length]);

  const addMessage = (sessionId: string, msg: ChatMessage) => {
    setSessions(prev => prev.map(s =>
      s.id === sessionId ? { ...s, messages: [...s.messages, msg] } : s
    ));
  };

  const [providerMissing, setProviderMissing] = useState(false);

  const createSession = async (prompt?: string) => {
    try {
      const resp = await api.post<{ id: string; status: string }>('/api/manager/sessions', {
        prompt: prompt || undefined,
      });
      setProviderMissing(false);
      const newSession: ManagerSession = {
        id: resp.id,
        title: prompt?.slice(0, 25) || 'New session',
        status: resp.status || 'running',
        mode: 'auto_read',
        messages: [],
      };
      setSessions(prev => [...prev, newSession]);
      setActiveIdx(sessions.length);
    } catch (e: any) {
      const msg = e?.message || e?.error || '';
      if (msg.toLowerCase().includes('provider') || msg.toLowerCase().includes('llm') || msg.toLowerCase().includes('api_key')) {
        setProviderMissing(true);
      }
      console.warn('create manager session:', e);
    }
  };

  const sendMessage = async () => {
    if (!input.trim() || !active || sending) return;
    const text = input.trim();
    setInput('');
    setSending(true);

    // Add user message immediately
    addMessage(active.id, { id: crypto.randomUUID(), role: 'user', content: text, timestamp: Date.now() });

    // Update title if first message
    if (active.messages.length === 0) {
      setSessions(prev => prev.map(s =>
        s.id === active.id ? { ...s, title: text.slice(0, 25) } : s
      ));
    }

    try {
      await api.post(`/api/manager/sessions/${active.id}/message`, { content: text });
    } catch (e: any) {
      addMessage(active.id, { id: crypto.randomUUID(), role: 'system', content: `Error: ${e.message}`, timestamp: Date.now() });
    } finally {
      setSending(false);
    }
  };

  const setMode = async (mode: string) => {
    if (!active) return;
    setModeOpen(false);
    try {
      await api.post(`/api/manager/sessions/${active.id}/mode`, { mode });
      setSessions(prev => prev.map(s =>
        s.id === active.id ? { ...s, mode } : s
      ));
    } catch {}
  };

  const stopSession = async (id: string) => {
    try {
      await api.del(`/api/manager/sessions/${id}`);
      setSessions(prev => prev.map(s =>
        s.id === id ? { ...s, status: 'stopped' } : s
      ));
    } catch {}
  };

  const handleSubmit = (e: Event) => {
    e.preventDefault();
    sendMessage();
  };

  const handleKeyDown = (e: KeyboardEvent) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      sendMessage();
    }
  };

  const currentMode = MODES.find(m => m.key === (active?.mode || 'auto_read')) || MODES[2];

  // Minimized: floating button
  if (!isOpen) {
    return (
      <button class="manager-fab" onClick={() => {
        setIsOpen(true);
        if (sessions.length === 0) createSession();
      }}>
        <svg width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
          <path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" />
        </svg>
      </button>
    );
  }

  return (
    <div ref={chatRef} class="manager-chat"
      style={pos ? `left:${pos.x}px;top:${pos.y}px;width:${pos.w}px;height:${pos.h}px;transform:none;bottom:auto;` : undefined}>
      {/* Header */}
      <div class="manager-chat-header" onMouseDown={onDragStart} style="cursor:grab">
        <span class="manager-chat-title">Manager</span>
        <div class="manager-chat-header-actions">
          {/* Mode selector */}
          <div class="manager-mode-selector">
            <button class="manager-mode-btn" onClick={() => setModeOpen(!modeOpen)}>
              {currentMode.icon} {currentMode.label}
            </button>
            {modeOpen && (
              <div class="manager-mode-dropdown">
                {MODES.map(m => (
                  <button key={m.key}
                    class={`manager-mode-option ${m.key === active?.mode ? 'active' : ''}`}
                    onClick={() => setMode(m.key)}>
                    {m.icon} {m.label}
                  </button>
                ))}
              </div>
            )}
          </div>
          <button class="manager-close-btn" onClick={() => setIsOpen(false)}>—</button>
        </div>
      </div>

      {/* Tab bar */}
      <div class="manager-tabs">
        {sessions.map((s, i) => (
          <button key={s.id}
            class={`manager-tab ${i === activeIdx ? 'active' : ''}`}
            onClick={() => setActiveIdx(i)}>
            <span class="manager-tab-title">{s.title}</span>
            {s.status === 'running' && <span class="manager-tab-dot" />}
            {s.status === 'stopped' && <span class="manager-tab-check">✓</span>}
            <span class="manager-tab-close" onClick={(e: Event) => {
              e.stopPropagation();
              stopSession(s.id);
              setSessions(prev => prev.filter(ss => ss.id !== s.id));
              if (activeIdx >= sessions.length - 1) setActiveIdx(Math.max(0, activeIdx - 1));
            }}>×</span>
          </button>
        ))}
        <button class="manager-tab manager-tab-new" onClick={() => createSession()}>+</button>
      </div>

      {/* Full Auto warning */}
      {active?.mode === 'full_auto' && (
        <div class="manager-warning">All actions auto-approved including production deploys and deletes</div>
      )}

      {/* Provider missing */}
      {providerMissing && (
        <div class="manager-provider-missing">
          <p>No LLM provider connected</p>
          <a href="/settings/provider-keys" onClick={() => setIsOpen(false)}>
            Connect provider in Settings
          </a>
        </div>
      )}

      {/* Messages */}
      <div class="manager-messages" ref={messagesRef}>
        {active?.messages.map(msg => {
          // Confirmation message — show approve/reject buttons
          if (msg.content.startsWith('__CONFIRM__')) {
            const parts = msg.content.split('__');
            const actionHash = parts[2];
            const toolName = parts[3];
            const summary = parts.slice(4).join('__');
            return (
              <div key={msg.id} class="manager-confirm">
                <div class="manager-confirm-summary">{summary || toolName}</div>
                <div class="manager-confirm-actions">
                  <button class="btn btn-sm btn-primary" onClick={async (e: Event) => {
                    e.stopPropagation();
                    await api.post(`/api/manager/sessions/${active!.id}/approve_action`, { action_hash: actionHash });
                    // Replace confirm message with approved note
                    setSessions(prev => prev.map(s => s.id === active!.id ? {
                      ...s, messages: s.messages.map(m => m.id === msg.id ? { ...m, content: `Approved: ${summary}`, role: 'system' as const } : m)
                    } : s));
                  }}>Approve</button>
                  <button class="btn btn-sm" onClick={async (e: Event) => {
                    e.stopPropagation();
                    await api.post(`/api/manager/sessions/${active!.id}/reject_action`, { action_hash: actionHash });
                    setSessions(prev => prev.map(s => s.id === active!.id ? {
                      ...s, messages: s.messages.map(m => m.id === msg.id ? { ...m, content: `Rejected: ${summary}`, role: 'system' as const } : m)
                    } : s));
                  }}>Reject</button>
                </div>
              </div>
            );
          }

          return (
            <div key={msg.id} class={`manager-msg manager-msg-${msg.role}`}>
              <div class="manager-msg-content">{msg.content}</div>
            </div>
          );
        })}
        {active?.messages.length === 0 && (
          <div class="manager-empty">
            <p>How can I help you manage the platform?</p>
            <div class="manager-suggestions">
              <button onClick={() => { setInput('What\'s the status of all projects?'); }}>Check project status</button>
              <button onClick={() => { setInput('Show me recent build failures'); }}>Check builds</button>
              <button onClick={() => { setInput('Create a new project'); }}>New project</button>
            </div>
          </div>
        )}
      </div>

      {/* Input */}
      <form class="manager-input-bar" onSubmit={handleSubmit}>
        <input
          type="text"
          class="manager-input"
          placeholder="Ask the manager..."
          value={input}
          onInput={(e) => setInput((e.target as HTMLInputElement).value)}
          onKeyDown={handleKeyDown}
          disabled={!active || active.status !== 'running'}
        />
        <button type="submit" class="manager-send-btn" disabled={!input.trim() || sending || !active}>
          ↑
        </button>
      </form>
    </div>
  );
}

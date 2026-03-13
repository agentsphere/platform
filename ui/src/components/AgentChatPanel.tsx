import { useState, useRef, useEffect } from 'preact/hooks';
import { api } from '../lib/api';
import type { ListResponse } from '../lib/api';
import type { AgentSession, ProgressEvent, SecretRequestMeta } from '../lib/types';
import { createSse, type EventSourceClient } from '../lib/sse';
import { SecretRequestModal } from './SecretRequestModal';

interface Props {
  projectId: string;
  open: boolean;
  onClose: () => void;
}

interface ChatMessage {
  role: 'user' | 'assistant' | 'system' | 'thinking';
  content: string;
}

type PanelStatus = 'idle' | 'creating' | 'connecting' | 'waiting' | 'ready' | 'working' | 'completed' | 'failed' | 'stopped';

function normalizeKind(kind: string | undefined): ProgressEvent['kind'] {
  if (!kind) return 'Text';
  const map: Record<string, ProgressEvent['kind']> = {
    text: 'Text', thinking: 'Thinking', tool_call: 'ToolCall',
    tool_result: 'ToolResult', milestone: 'Milestone', error: 'Error',
    completed: 'Completed', waiting_for_input: 'WaitingForInput',
    secret_request: 'SecretRequest',
    iframe_available: 'IframeAvailable', iframe_removed: 'IframeRemoved',
    Text: 'Text', Thinking: 'Thinking', ToolCall: 'ToolCall',
    ToolResult: 'ToolResult', Milestone: 'Milestone', Error: 'Error',
    Completed: 'Completed', WaitingForInput: 'WaitingForInput',
    SecretRequest: 'SecretRequest',
    IframeAvailable: 'IframeAvailable', IframeRemoved: 'IframeRemoved',
  };
  return map[kind] || 'Text';
}

function msgStyle(role: string): Record<string, string> {
  const base: Record<string, string> = { padding: '0.75rem 1rem', 'margin-bottom': '0.5rem', 'border-radius': '0.5rem' };
  if (role === 'user') return { ...base, background: 'var(--bg-tertiary)', 'margin-left': '2rem' };
  if (role === 'assistant') return { ...base, background: 'var(--bg-primary)', 'margin-right': '2rem' };
  if (role === 'thinking') return { ...base, background: 'transparent', 'margin-right': '2rem', 'border-left': '3px solid var(--text-muted)', 'padding-left': '1rem' };
  return { ...base, color: 'var(--text-muted)', 'font-style': 'italic', 'font-size': '0.85rem' };
}

const STATUS_LABELS: Record<PanelStatus, string> = {
  idle: '',
  creating: 'Creating session...',
  connecting: 'Connecting...',
  waiting: 'Waiting for agent...',
  ready: '',
  working: '',
  completed: 'Session completed',
  failed: 'Session failed',
  stopped: 'Session stopped',
};

export function AgentChatPanel({ projectId, open, onClose }: Props) {
  const [status, setStatus] = useState<PanelStatus>('idle');
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [input, setInput] = useState('');
  const [sessionId, setSessionId] = useState<string | null>(null);
  const [secretRequest, setSecretRequest] = useState<SecretRequestMeta | null>(null);
  const messagesEnd = useRef<HTMLDivElement>(null);
  const sseRef = useRef<EventSourceClient | null>(null);
  const streamBuf = useRef('');
  const inputRef = useRef<HTMLTextAreaElement>(null);

  // Auto-scroll on new messages
  useEffect(() => {
    messagesEnd.current?.scrollIntoView({ behavior: 'smooth' });
  }, [messages]);

  // Clean up SSE on unmount
  useEffect(() => {
    return () => { sseRef.current?.close(); };
  }, []);

  // On open, check for existing running session or auto-create one
  useEffect(() => {
    if (!open) return;
    if (sessionId) return; // already have a session
    api.get<ListResponse<AgentSession>>(`/api/projects/${projectId}/sessions?status=running&limit=1`)
      .then(r => {
        if (r.items.length > 0) {
          const s = r.items[0];
          setSessionId(s.id);
          setStatus('connecting');
          loadSessionMessages(s.id);
          connectSse(s.id);
        } else {
          // No running session — auto-create one (no prompt = agent starts idle)
          autoCreateSession();
        }
      })
      .catch(() => {});
  }, [open, projectId]);

  // Focus input when ready
  useEffect(() => {
    if (open && (status === 'idle' || status === 'ready')) {
      setTimeout(() => inputRef.current?.focus(), 100);
    }
  }, [open, status]);

  function loadSessionMessages(sid: string) {
    api.get<AgentSession & { messages?: { role: string; content: string; metadata?: Record<string, any> }[] }>(
      `/api/projects/${projectId}/sessions/${sid}`
    ).then(data => {
      if (data.messages && data.messages.length > 0) {
        const mapped: ChatMessage[] = [];
        for (const m of data.messages) {
          const kind = normalizeKind(m.metadata?.kind as string);
          if (m.role === 'user' || kind === 'Text' && m.role === 'user') {
            mapped.push({ role: 'user', content: m.content });
          } else if (kind === 'Text') {
            mapped.push({ role: 'assistant', content: m.content });
          } else if (kind === 'Thinking') {
            mapped.push({ role: 'thinking', content: m.content });
          } else if (kind === 'ToolCall' || kind === 'ToolResult') {
            mapped.push({ role: 'system', content: m.content });
          } else if (kind === 'Milestone') {
            mapped.push({ role: 'system', content: m.content });
          }
        }
        setMessages(mapped);
      }
      if (data.status === 'completed') setStatus('completed');
      else if (data.status === 'failed') setStatus('failed');
      else if (data.status === 'stopped') setStatus('stopped');
    }).catch(() => {});
  }

  function connectSse(sid: string) {
    sseRef.current?.close();
    const sse = createSse({
      url: `/api/projects/${projectId}/sessions/${sid}/events`,
      onOpen() {
        setStatus(prev => prev === 'connecting' ? 'waiting' : prev);
      },
      onError() {
        // SSE has auto-reconnect; don't change status to terminal
      },
      onMessage(raw: Record<string, any>) {
        const kind = normalizeKind(raw.kind);
        const message: string = raw.message ?? '';
        const metadata = raw.metadata;

        switch (kind) {
          case 'Text': {
            streamBuf.current += message;
            const text = streamBuf.current;
            setStatus('working');
            setMessages(prev => {
              const last = prev[prev.length - 1];
              if (last && last.role === 'assistant') {
                return [...prev.slice(0, -1), { role: 'assistant', content: text }];
              }
              return [...prev, { role: 'assistant', content: text }];
            });
            break;
          }
          case 'Thinking': {
            setStatus('working');
            setMessages(prev => {
              const last = prev[prev.length - 1];
              if (last && last.role === 'thinking') {
                return [...prev.slice(0, -1), { role: 'thinking', content: last.content + message }];
              }
              return [...prev, { role: 'thinking', content: message }];
            });
            break;
          }
          case 'ToolCall': {
            setMessages(prev => [...prev, { role: 'system', content: `Running: ${message}...` }]);
            break;
          }
          case 'ToolResult': {
            const isError = metadata?.is_error;
            setMessages(prev => [...prev, { role: 'system', content: isError ? `Error: ${message}` : message }]);
            break;
          }
          case 'Milestone': {
            if (message === 'Session started') {
              setStatus('ready');
            }
            setMessages(prev => [...prev, { role: 'system', content: message }]);
            break;
          }
          case 'WaitingForInput': {
            streamBuf.current = '';
            setStatus('ready');
            break;
          }
          case 'Completed': {
            streamBuf.current = '';
            setStatus('completed');
            break;
          }
          case 'Error': {
            streamBuf.current = '';
            setStatus('failed');
            setMessages(prev => [...prev, { role: 'system', content: `Error: ${message}` }]);
            break;
          }
          case 'SecretRequest': {
            if (metadata) {
              setSecretRequest({
                request_id: metadata.request_id,
                name: metadata.name,
                prompt: metadata.prompt || message,
                environments: metadata.environments,
              });
            }
            break;
          }
        }
      },
    });
    sseRef.current = sse;
  }

  async function autoCreateSession() {
    setStatus('creating');
    try {
      const resp = await api.post<AgentSession>(`/api/projects/${projectId}/sessions`, {
        provider: 'claude-code',
      });
      setSessionId(resp.id);
      setStatus('connecting');
      streamBuf.current = '';
      connectSse(resp.id);
    } catch (err: unknown) {
      const msg = err instanceof Error ? err.message : 'Failed to create session';
      setMessages(prev => [...prev, { role: 'system', content: `Error: ${msg}` }]);
      setStatus('idle');
    }
  }

  async function createSession(prompt: string) {
    setStatus('creating');
    setMessages(prev => [...prev, { role: 'user', content: prompt }]);
    try {
      const resp = await api.post<AgentSession>(`/api/projects/${projectId}/sessions`, {
        prompt,
        provider: 'claude-code',
      });
      setSessionId(resp.id);
      setStatus('connecting');
      streamBuf.current = '';
      connectSse(resp.id);
    } catch (err: unknown) {
      const msg = err instanceof Error ? err.message : 'Failed to create session';
      setMessages(prev => [...prev, { role: 'system', content: `Error: ${msg}` }]);
      setStatus('idle');
    }
  }

  async function sendMessage() {
    if (!input.trim() || !sessionId) return;
    const userMsg = input.trim();
    setInput('');
    setMessages(prev => [...prev, { role: 'user', content: userMsg }]);
    streamBuf.current = '';
    setStatus('working');
    try {
      await api.post(`/api/projects/${projectId}/sessions/${sessionId}/message`, { content: userMsg });
    } catch {
      setMessages(prev => [...prev, { role: 'system', content: 'Failed to send message' }]);
      setStatus('ready');
    }
  }

  async function stopSession() {
    if (!sessionId) return;
    try {
      await api.post(`/api/projects/${projectId}/sessions/${sessionId}/stop`);
      setStatus('stopped');
    } catch { /* ignore */ }
  }

  function handleSubmit(e: Event) {
    e.preventDefault();
    if (!input.trim()) return;
    if (!sessionId) {
      createSession(input.trim());
      setInput('');
    } else {
      sendMessage();
    }
  }

  function handleKeyDown(e: KeyboardEvent) {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      handleSubmit(e);
    }
  }

  function handleNewSession() {
    sseRef.current?.close();
    sseRef.current = null;
    setSessionId(null);
    setMessages([]);
    streamBuf.current = '';
    // Auto-create a new session immediately
    autoCreateSession();
  }

  const inputDisabled = status === 'creating' || status === 'connecting' || status === 'waiting' || status === 'working';
  const isTerminal = status === 'completed' || status === 'failed' || status === 'stopped';
  const statusLabel = STATUS_LABELS[status];

  if (!open) return null;

  return (
    <>
      <div class="agent-chat-overlay" onClick={onClose} />
      <div class="agent-chat-panel">
        <div class="agent-chat-panel-header">
          <div class="flex gap-sm" style="align-items:center">
            <span style="font-weight:600">Agent</span>
            {sessionId && (
              <span class="text-xs text-muted mono">{sessionId.substring(0, 8)}</span>
            )}
          </div>
          <div class="flex gap-sm">
            {sessionId && !isTerminal && (
              <button class="btn btn-sm btn-danger" onClick={stopSession}>Stop</button>
            )}
            {isTerminal && (
              <button class="btn btn-sm" onClick={handleNewSession}>New</button>
            )}
            <button class="btn btn-sm btn-ghost" onClick={onClose} style="font-size:16px">
              &times;
            </button>
          </div>
        </div>

        {statusLabel && (
          <div class="agent-chat-status">{statusLabel}</div>
        )}

        <div class="agent-chat-messages">
          {messages.length === 0 && (status === 'idle' || status === 'creating' || status === 'connecting' || status === 'waiting') && (
            <div style="text-align:center;padding:2rem;color:var(--text-muted)">
              <p style="margin-bottom:0.5rem">{status === 'idle' ? 'Starting agent...' : 'Agent is starting...'}</p>
              <p class="text-xs">The agent will have access to this project's code and can make changes.</p>
            </div>
          )}
          {messages.map((msg, i) => (
            <div key={i} style={msgStyle(msg.role)}>
              <div style="font-weight:600;margin-bottom:0.15rem;font-size:0.8rem;color:var(--text-muted)">
                {msg.role === 'user' ? 'You' : msg.role === 'assistant' ? 'Agent' : msg.role === 'thinking' ? 'Thinking...' : ''}
              </div>
              <div style={msg.role === 'thinking' ? 'white-space:pre-wrap;font-style:italic;opacity:0.7;font-size:13px' : 'white-space:pre-wrap;font-size:13px'}>{msg.content}</div>
            </div>
          ))}
          {status === 'working' && (
            <div style="padding:0.5rem 1rem;color:var(--text-muted)">
              <span class="typing-indicator">&#9679; &#9679; &#9679;</span>
            </div>
          )}
          <div ref={messagesEnd} />
        </div>

        {!isTerminal && (
          <form class="agent-chat-input" onSubmit={handleSubmit}>
            <textarea
              ref={inputRef}
              class="input"
              style="flex:1;min-height:36px;max-height:120px;resize:none"
              rows={1}
              placeholder="Send a message..."
              value={input}
              onInput={(e) => setInput((e.target as HTMLTextAreaElement).value)}
              onKeyDown={handleKeyDown}
              disabled={inputDisabled}
            />
            <button type="submit" class="btn btn-primary btn-sm" disabled={inputDisabled || !input.trim()}>
              Send
            </button>
          </form>
        )}

        {isTerminal && (
          <div class="agent-chat-input" style="justify-content:center">
            <button class="btn btn-sm btn-primary" onClick={handleNewSession}>Start New Session</button>
          </div>
        )}
      </div>

      {secretRequest && (
        <SecretRequestModal
          open={!!secretRequest}
          projectId={projectId}
          requestId={secretRequest.request_id}
          name={secretRequest.name}
          prompt={secretRequest.prompt}
          onComplete={() => {
            setSecretRequest(null);
            setMessages(prev => [...prev, { role: 'system', content: `Secret "${secretRequest.name}" provided` }]);
          }}
          onClose={() => setSecretRequest(null)}
        />
      )}
    </>
  );
}

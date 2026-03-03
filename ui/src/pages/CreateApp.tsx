import { useState, useRef, useEffect } from 'preact/hooks';
import { api } from '../lib/api';
import { createSse, type EventSourceClient } from '../lib/sse';

interface ChatMessage {
  role: 'user' | 'assistant' | 'system' | 'thinking';
  content: string;
}

interface SessionInfo {
  id: string;
  status: string;
  project_id?: string;
}

interface ProgressEvent {
  kind: string;
  message: string;
  metadata?: Record<string, unknown>;
}

export function CreateApp() {
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [input, setInput] = useState('');
  const [session, setSession] = useState<SessionInfo | null>(null);
  const [loading, setLoading] = useState(false);
  const [connected, setConnected] = useState(false);
  const [streaming, setStreaming] = useState(false);
  const messagesEnd = useRef<HTMLDivElement>(null);
  const sseRef = useRef<EventSourceClient | null>(null);
  // Accumulate streaming text into the last assistant message
  const streamBuf = useRef('');

  // Auto-scroll on new messages
  useEffect(() => {
    messagesEnd.current?.scrollIntoView({ behavior: 'smooth' });
  }, [messages]);

  // Cleanup SSE on unmount
  useEffect(() => {
    return () => { sseRef.current?.close(); };
  }, []);

  function connectSse(sessionId: string) {
    const sse = createSse({
      url: `/api/sessions/${sessionId}/events`,
      onOpen() {
        setConnected(true);
      },
      onError() {
        setConnected(false);
        setStreaming(false);
      },
      onMessage(data: ProgressEvent) {
        switch (data.kind) {
          case 'text': {
            streamBuf.current += data.message;
            const text = streamBuf.current;
            setMessages(prev => {
              // Append to existing assistant message or create new one
              const last = prev[prev.length - 1];
              if (last && last.role === 'assistant') {
                return [...prev.slice(0, -1), { role: 'assistant', content: text }];
              }
              return [...prev, { role: 'assistant', content: text }];
            });
            break;
          }
          case 'thinking': {
            setMessages(prev => {
              const last = prev[prev.length - 1];
              if (last && last.role === 'thinking') {
                return [...prev.slice(0, -1), { role: 'thinking', content: last.content + data.message }];
              }
              return [...prev, { role: 'thinking', content: data.message }];
            });
            break;
          }
          case 'completed': {
            streamBuf.current = '';
            setStreaming(false);
            break;
          }
          case 'tool_call': {
            setMessages(prev => [...prev, { role: 'system', content: `Setting up: ${data.message}...` }]);
            break;
          }
          case 'tool_result': {
            const isError = data.metadata?.is_error;
            setMessages(prev => [...prev, { role: 'system', content: isError ? `Error: ${data.message}` : data.message }]);
            // If a project was created, update session info
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
            streamBuf.current = '';
            setStreaming(false);
            setMessages(prev => [...prev, { role: 'system', content: `Error: ${data.message}` }]);
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
      // First message — create the session
      setLoading(true);
      setMessages(prev => [...prev, { role: 'user', content: userMsg }]);

      try {
        const resp = await api.post<SessionInfo>('/api/create-app', {
          description: userMsg,
        });
        setSession(resp);
        setStreaming(true);
        streamBuf.current = '';
        // Connect SSE to receive streaming response
        connectSse(resp.id);
      } catch (err: unknown) {
        const msg = err instanceof Error ? err.message : 'Failed to create session';
        setMessages(prev => [...prev, { role: 'system', content: `Error: ${msg}` }]);
      } finally {
        setLoading(false);
      }
    } else {
      // Follow-up message — send via REST
      setMessages(prev => [...prev, { role: 'user', content: userMsg }]);
      streamBuf.current = '';
      setStreaming(true);
      try {
        await api.post(`/api/sessions/${session.id}/message`, { content: userMsg });
      } catch {
        setMessages(prev => [...prev, { role: 'system', content: 'Failed to send message' }]);
        setStreaming(false);
      }
    }
  }

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
        {messages.map((msg, i) => (
          <div key={i} style={msgStyle(msg.role)}>
            <div style="font-weight:600;margin-bottom:0.25rem;font-size:0.85rem">
              {msg.role === 'user' ? 'You' : msg.role === 'assistant' ? 'Agent' : msg.role === 'thinking' ? 'Thinking...' : 'System'}
            </div>
            <div style={msg.role === 'thinking' ? 'white-space:pre-wrap;font-style:italic;opacity:0.7' : 'white-space:pre-wrap'}>{msg.content}</div>
          </div>
        ))}
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

      {/* Input area */}
      <form onSubmit={handleSubmit} style="display:flex;gap:0.5rem;padding:1rem 0;border-top:1px solid var(--border)">
        <input
          type="text"
          class="input"
          style="flex:1"
          placeholder={session ? 'Send a follow-up message...' : 'Describe your app idea...'}
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

function msgStyle(role: string): Record<string, string> {
  const base: Record<string, string> = { padding: '0.75rem 1rem', 'margin-bottom': '0.5rem', 'border-radius': '0.5rem' };
  if (role === 'user') {
    return { ...base, background: 'var(--bg-secondary)', 'margin-left': '2rem' };
  }
  if (role === 'assistant') {
    return { ...base, background: 'var(--bg-tertiary, var(--bg-secondary))', 'margin-right': '2rem' };
  }
  if (role === 'thinking') {
    return { ...base, background: 'transparent', 'margin-right': '2rem', 'border-left': '3px solid var(--text-muted)', 'padding-left': '1rem' };
  }
  return { ...base, color: 'var(--text-muted)', 'font-style': 'italic', 'font-size': '0.85rem' };
}

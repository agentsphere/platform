import { useState, useRef, useEffect } from 'preact/hooks';
import { api } from '../lib/api';
import { createWs, type ReconnectingWebSocket } from '../lib/ws';
import { useOnboarding } from '../lib/onboarding';

interface ValidateResponse {
  valid: boolean;
  error?: string;
}

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

export function OnboardingOverlay() {
  const { needsOnboarding, hasProviderKey, refresh } = useOnboarding();

  // --- API key state ---
  const [apiKey, setApiKey] = useState('');
  const [validating, setValidating] = useState(false);
  const [keyValid, setKeyValid] = useState(false);
  const [keyError, setKeyError] = useState('');
  const [saving, setSaving] = useState(false);

  // --- Chat state (mirrors CreateApp.tsx) ---
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [input, setInput] = useState('');
  const [session, setSession] = useState<SessionInfo | null>(null);
  const [chatLoading, setChatLoading] = useState(false);
  const [streaming, setStreaming] = useState(false);
  const messagesEnd = useRef<HTMLDivElement>(null);
  const wsRef = useRef<ReconnectingWebSocket | null>(null);
  const streamBuf = useRef('');

  const chatEnabled = keyValid || hasProviderKey;

  // Auto-scroll on new messages
  useEffect(() => {
    messagesEnd.current?.scrollIntoView({ behavior: 'smooth' });
  }, [messages]);

  // Cleanup WS on unmount
  useEffect(() => {
    return () => { wsRef.current?.close(); };
  }, []);

  if (!needsOnboarding) return null;

  // --- Key validation ---
  async function handleValidate(e: Event) {
    e.preventDefault();
    if (!apiKey.trim() || apiKey.length < 10) return;

    setValidating(true);
    setKeyError('');
    try {
      const resp = await api.post<ValidateResponse>('/api/users/me/provider-keys/validate', {
        api_key: apiKey,
      });
      if (resp.valid) {
        setKeyValid(true);
        // Auto-save after successful validation
        await saveKey();
      } else {
        setKeyError(resp.error || 'Invalid API key');
      }
    } catch (err: unknown) {
      setKeyError(err instanceof Error ? err.message : 'Validation failed');
    } finally {
      setValidating(false);
    }
  }

  async function saveKey() {
    setSaving(true);
    try {
      await api.put('/api/users/me/provider-keys/anthropic', { api_key: apiKey });
      setKeyValid(true);
      refresh();
    } catch (err: unknown) {
      setKeyError(err instanceof Error ? err.message : 'Failed to save key');
    } finally {
      setSaving(false);
    }
  }

  // --- WebSocket chat ---
  function connectWs(sessionId: string) {
    const ws = createWs({
      url: `/api/sessions/${sessionId}/ws`,
      onOpen() {},
      onClose() {
        setStreaming(false);
      },
      onMessage(data: ProgressEvent) {
        switch (data.kind) {
          case 'text': {
            streamBuf.current += data.message;
            const text = streamBuf.current;
            setMessages(prev => {
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
            // Re-check onboarding — project may have been created
            setTimeout(() => refresh(), 1000);
            break;
          }
          case 'tool_call': {
            setMessages(prev => [...prev, { role: 'system', content: `Setting up: ${data.message}...` }]);
            break;
          }
          case 'tool_result': {
            const isError = data.metadata?.is_error;
            setMessages(prev => [...prev, { role: 'system', content: isError ? `Error: ${data.message}` : data.message }]);
            // If a project was created, update session + trigger onboarding refresh
            if (data.metadata?.tool_name === 'create_project' && !isError && data.metadata?.result) {
              try {
                const result = JSON.parse(data.metadata.result as string);
                if (result.project_id) {
                  setSession(prev => prev ? { ...prev, project_id: result.project_id } : prev);
                }
              } catch { /* ignore parse errors */ }
              refresh();
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
      reconnect: true,
      maxRetries: 5,
    });
    wsRef.current = ws;
  }

  async function handleChatSubmit(e: Event) {
    e.preventDefault();
    if (!input.trim() || !chatEnabled) return;

    const userMsg = input.trim();
    setInput('');

    if (!session) {
      setChatLoading(true);
      setMessages(prev => [...prev, { role: 'user', content: userMsg }]);

      try {
        const resp = await api.post<SessionInfo>('/api/create-app', {
          description: userMsg,
        });
        setSession(resp);
        setStreaming(true);
        streamBuf.current = '';
        connectWs(resp.id);
      } catch (err: unknown) {
        const msg = err instanceof Error ? err.message : 'Failed to create session';
        setMessages(prev => [...prev, { role: 'system', content: `Error: ${msg}` }]);
      } finally {
        setChatLoading(false);
      }
    } else {
      setMessages(prev => [...prev, { role: 'user', content: userMsg }]);
      streamBuf.current = '';
      setStreaming(true);
      if (wsRef.current) {
        wsRef.current.send(JSON.stringify({ content: userMsg }));
      } else {
        try {
          await api.post(`/api/sessions/${session.id}/message`, { content: userMsg });
        } catch {
          setMessages(prev => [...prev, { role: 'system', content: 'Failed to send message' }]);
          setStreaming(false);
        }
      }
    }
  }

  return (
    <div class="onboarding-overlay">
      <div class="onboarding-backdrop" />
      <div class="onboarding-panel">
        {/* Header */}
        <div class="onboarding-header">
          <h2 style="margin:0 0 0.25rem">Welcome to Platform</h2>
          <p class="text-muted text-sm" style="margin:0">
            {hasProviderKey
              ? 'No projects yet \u2014 describe what you\'d like to build.'
              : 'Set your Claude API key to get started, then describe what you want to build.'}
          </p>
        </div>

        {/* API Key Section (hidden if user already has a key) */}
        {!hasProviderKey && (
          <div class="onboarding-key-section">
            <form onSubmit={handleValidate} style="display:flex;gap:0.5rem;align-items:flex-start">
              <div style="flex:1">
                <input
                  type="password"
                  class="input"
                  placeholder="sk-ant-api03-..."
                  value={apiKey}
                  onInput={(e) => setApiKey((e.target as HTMLInputElement).value)}
                  disabled={keyValid || validating || saving}
                />
              </div>
              <button
                type="submit"
                class="btn btn-primary"
                disabled={keyValid || validating || saving || apiKey.length < 10}
              >
                {validating ? 'Validating...' : saving ? 'Saving...' : keyValid ? 'Saved' : 'Set Key'}
              </button>
            </form>
            {keyError && (
              <div class="onboarding-key-status invalid">{keyError}</div>
            )}
            {keyValid && (
              <div class="onboarding-key-status valid">API key verified and saved</div>
            )}
          </div>
        )}

        {/* Chat Section */}
        <div class={`onboarding-chat-section ${chatEnabled ? '' : 'disabled'}`}>
          {/* Messages */}
          <div class="onboarding-messages">
            {messages.length === 0 && (
              <div style="text-align:center;padding:2rem 1rem;color:var(--text-muted)">
                <p style="font-size:1rem;margin-bottom:0.25rem">What would you like to build?</p>
                <p class="text-sm">Describe your app idea and an AI agent will set everything up.</p>
              </div>
            )}
            {messages.map((msg, i) => (
              <div key={i} style={msgStyle(msg.role)}>
                <div style="font-weight:600;margin-bottom:0.15rem;font-size:0.8rem">
                  {msg.role === 'user' ? 'You' : msg.role === 'assistant' ? 'Agent' : msg.role === 'thinking' ? 'Thinking...' : 'System'}
                </div>
                <div style={msg.role === 'thinking' ? 'white-space:pre-wrap;font-style:italic;opacity:0.7' : 'white-space:pre-wrap'}>
                  {msg.content}
                </div>
              </div>
            ))}
            {streaming && (
              <div style="padding:0.5rem;color:var(--text-muted)">
                <span>&#9679; &#9679; &#9679;</span>
              </div>
            )}
            {chatLoading && (
              <div style="padding:0.5rem;color:var(--text-muted);font-style:italic">Creating session...</div>
            )}
            <div ref={messagesEnd} />
          </div>

          {/* Chat input */}
          <form onSubmit={handleChatSubmit} style="display:flex;gap:0.5rem;border-top:1px solid var(--border);padding-top:0.75rem">
            <input
              type="text"
              class="input"
              style="flex:1"
              placeholder={session ? 'Send a follow-up...' : 'Describe your app idea...'}
              value={input}
              onInput={(e) => setInput((e.target as HTMLInputElement).value)}
              disabled={!chatEnabled || chatLoading || streaming}
            />
            <button
              type="submit"
              class="btn btn-primary"
              disabled={!chatEnabled || chatLoading || streaming || !input.trim()}
            >
              {session ? 'Send' : 'Create'}
            </button>
          </form>

          {/* Session info */}
          {session && (
            <div class="text-sm text-muted" style="padding-top:0.5rem">
              Session: {session.id.slice(0, 8)} | Status: {session.status}
              {session.project_id && (
                <span> | <a href={`/projects/${session.project_id}`}>View Project</a></span>
              )}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

function msgStyle(role: string): Record<string, string> {
  const base: Record<string, string> = {
    padding: '0.6rem 0.75rem',
    'margin-bottom': '0.4rem',
    'border-radius': '0.4rem',
    'font-size': '13px',
  };
  if (role === 'user') {
    return { ...base, background: 'var(--bg-secondary)', 'margin-left': '1.5rem' };
  }
  if (role === 'assistant') {
    return { ...base, background: 'var(--bg-tertiary, var(--bg-secondary))', 'margin-right': '1.5rem' };
  }
  if (role === 'thinking') {
    return { ...base, background: 'transparent', 'margin-right': '1.5rem', 'border-left': '3px solid var(--text-muted)', 'padding-left': '0.75rem' };
  }
  return { ...base, color: 'var(--text-muted)', 'font-style': 'italic' };
}

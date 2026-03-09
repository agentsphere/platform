export interface AgentInfo {
  sessionId: string;
  label: string;
  status: 'pending' | 'running' | 'completed' | 'failed' | 'stopped';
}

interface AgentBarProps {
  agents: Map<string, AgentInfo>;
  parentSessionId: string;
  replyTarget: string | null;
  onSelectAgent: (sessionId: string) => void;
}

const STATUS_COLORS: Record<string, string> = {
  running: 'var(--accent)',
  pending: 'var(--warning)',
  completed: 'var(--text-muted)',
  failed: 'var(--danger)',
  stopped: 'var(--text-muted)',
};

export const AGENT_COLORS = ['#3b82f6', '#22c55e', '#f59e0b', '#a855f7', '#ef4444', '#06b6d4'];

export function getAgentColor(index: number): string {
  return AGENT_COLORS[index % AGENT_COLORS.length];
}

export function AgentBar({ agents, parentSessionId, replyTarget, onSelectAgent }: AgentBarProps) {
  if (agents.size === 0) return null;

  return (
    <div class="agent-bar">
      <button
        type="button"
        class={`agent-bar-item ${replyTarget === null || replyTarget === parentSessionId ? 'agent-bar-active' : ''}`}
        onClick={() => onSelectAgent(parentSessionId)}
      >
        <span class="agent-bar-dot" style={`background: var(--accent)`} />
        <span class="agent-bar-label">Manager</span>
      </button>
      {Array.from(agents.values()).map((agent, i) => (
        <button
          type="button"
          key={agent.sessionId}
          class={`agent-bar-item ${replyTarget === agent.sessionId ? 'agent-bar-active' : ''}`}
          onClick={() => onSelectAgent(agent.sessionId)}
        >
          <span class="agent-bar-dot" style={`background: ${STATUS_COLORS[agent.status] || 'var(--text-muted)'}`} />
          <span class="agent-bar-label" style={`color: ${getAgentColor(i + 1)}`}>
            {agent.label}
          </span>
        </button>
      ))}
    </div>
  );
}

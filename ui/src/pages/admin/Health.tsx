import { useState, useEffect, useRef } from 'preact/hooks';
import { api } from '../../lib/api';
import { timeAgo } from '../../lib/format';
import { Badge } from '../../components/Badge';

interface SubsystemCheck {
  name: string;
  status: 'healthy' | 'degraded' | 'unhealthy' | 'unknown';
  latency_ms: number;
  message: string | null;
  checked_at: string;
}

interface BackgroundTaskHealth {
  name: string;
  status: 'healthy' | 'degraded' | 'unhealthy' | 'unknown';
  last_heartbeat: string | null;
  success_count: number;
  failure_count: number;
  last_error: string | null;
}

interface RecentPodFailure {
  id: string;
  project_id: string | null;
  project_name: string | null;
  pod_name: string | null;
  kind: string;
  error: string | null;
  failed_at: string;
}

interface PodFailureSummary {
  total_failed_24h: number;
  agent_failures: number;
  pipeline_failures: number;
  recent_failures: RecentPodFailure[];
}

interface HealthSnapshot {
  overall: 'healthy' | 'degraded' | 'unhealthy' | 'unknown';
  subsystems: SubsystemCheck[];
  background_tasks: BackgroundTaskHealth[];
  pod_failures: PodFailureSummary;
  uptime_seconds: number;
  checked_at: string;
}

function statusColor(status: string): string {
  switch (status) {
    case 'healthy': return 'var(--success)';
    case 'degraded': return 'var(--warning)';
    case 'unhealthy': return 'var(--danger)';
    default: return 'var(--text-muted)';
  }
}

function statusBadgeVariant(status: string): 'success' | 'warning' | 'danger' | 'default' {
  switch (status) {
    case 'healthy': return 'success';
    case 'degraded': return 'warning';
    case 'unhealthy': return 'danger';
    default: return 'default';
  }
}

function formatUptime(seconds: number): string {
  const days = Math.floor(seconds / 86400);
  const hours = Math.floor((seconds % 86400) / 3600);
  const mins = Math.floor((seconds % 3600) / 60);
  if (days > 0) return `${days}d ${hours}h`;
  if (hours > 0) return `${hours}h ${mins}m`;
  return `${mins}m`;
}

export function Health() {
  const [data, setData] = useState<HealthSnapshot | null>(null);
  const [error, setError] = useState<string | null>(null);
  const eventSourceRef = useRef<EventSource | null>(null);

  useEffect(() => {
    // Initial fetch
    api.get<HealthSnapshot>('/api/health/details')
      .then(setData)
      .catch(e => setError(e.message || 'Failed to load health data'));

    // SSE for real-time updates
    const token = localStorage.getItem('token');
    if (token) {
      const es = new EventSource(`/api/health/stream?token=${encodeURIComponent(token)}`);
      es.addEventListener('health', (e: any) => {
        try {
          setData(JSON.parse(e.data));
        } catch { /* ignore parse errors */ }
      });
      es.onerror = () => {
        // SSE may fail if not admin or no Valkey — non-fatal
      };
      eventSourceRef.current = es;
    }

    return () => {
      eventSourceRef.current?.close();
    };
  }, []);

  if (error) {
    return (
      <div>
        <h2>Health</h2>
        <div class="alert alert-danger">{error}</div>
      </div>
    );
  }

  if (!data) {
    return (
      <div>
        <h2>Health</h2>
        <div class="loading">Loading health data...</div>
      </div>
    );
  }

  const overallLabel = data.overall === 'healthy'
    ? 'All systems operational'
    : data.overall === 'degraded'
      ? 'Some systems degraded'
      : data.overall === 'unhealthy'
        ? 'System unhealthy'
        : 'Status unknown';

  return (
    <div>
      <h2 style="margin-bottom:1rem">Health</h2>

      {/* Overall status banner */}
      <div style={{
        padding: '1rem',
        borderRadius: 'var(--radius)',
        background: statusColor(data.overall),
        color: '#fff',
        marginBottom: '1.5rem',
        display: 'flex',
        justifyContent: 'space-between',
        alignItems: 'center',
      }}>
        <strong>{overallLabel}</strong>
        <span>Uptime: {formatUptime(data.uptime_seconds)}</span>
      </div>

      {/* Subsystem cards */}
      <h3 style="margin-bottom:0.5rem">Subsystems</h3>
      <div class="stats-grid mb-md">
        {data.subsystems.map(s => (
          <div class="stat-card" key={s.name}>
            <div style={{ display: 'flex', alignItems: 'center', gap: '0.5rem', marginBottom: '0.25rem' }}>
              <span style={{
                width: 10, height: 10, borderRadius: '50%',
                background: statusColor(s.status), display: 'inline-block',
              }} />
              <strong>{s.name}</strong>
            </div>
            <div class="stat-label">
              {s.latency_ms > 0 && <span>{s.latency_ms}ms</span>}
              {s.message && <span style={{ color: 'var(--text-muted)', marginLeft: '0.5rem' }}>{s.message}</span>}
            </div>
          </div>
        ))}
      </div>

      {/* Background tasks table */}
      <h3 style="margin-bottom:0.5rem">Background Tasks</h3>
      <div class="table-container mb-md">
        <table class="table">
          <thead>
            <tr>
              <th>Task</th>
              <th>Status</th>
              <th>Last Heartbeat</th>
              <th>Success</th>
              <th>Failures</th>
              <th>Last Error</th>
            </tr>
          </thead>
          <tbody>
            {data.background_tasks.map(t => (
              <tr key={t.name}>
                <td><code>{t.name}</code></td>
                <td><Badge variant={statusBadgeVariant(t.status)}>{t.status}</Badge></td>
                <td>{t.last_heartbeat ? timeAgo(t.last_heartbeat) : 'never'}</td>
                <td>{t.success_count}</td>
                <td style={{ color: t.failure_count > 0 ? 'var(--danger)' : undefined }}>{t.failure_count}</td>
                <td style={{ maxWidth: 300, overflow: 'hidden', textOverflow: 'ellipsis' }}>
                  {t.last_error || '-'}
                </td>
              </tr>
            ))}
            {data.background_tasks.length === 0 && (
              <tr><td colSpan={6} style={{ textAlign: 'center', color: 'var(--text-muted)' }}>No tasks registered</td></tr>
            )}
          </tbody>
        </table>
      </div>

      {/* Pod failures */}
      <h3 style="margin-bottom:0.5rem">Pod Failures (24h)</h3>
      <div class="stats-grid mb-md" style={{ marginBottom: '1rem' }}>
        <div class="stat-card">
          <div class="stat-value">{data.pod_failures.total_failed_24h}</div>
          <div class="stat-label">Total Failed</div>
        </div>
        <div class="stat-card">
          <div class="stat-value">{data.pod_failures.agent_failures}</div>
          <div class="stat-label">Agent Failures</div>
        </div>
        <div class="stat-card">
          <div class="stat-value">{data.pod_failures.pipeline_failures}</div>
          <div class="stat-label">Pipeline Failures</div>
        </div>
      </div>

      {data.pod_failures.recent_failures.length > 0 && (
        <div class="table-container mb-md">
          <table class="table">
            <thead>
              <tr>
                <th>Project</th>
                <th>Type</th>
                <th>Pod</th>
                <th>Error</th>
                <th>Time</th>
              </tr>
            </thead>
            <tbody>
              {data.pod_failures.recent_failures.map(f => (
                <tr key={f.id}>
                  <td>
                    {f.project_id ? (
                      <a href={`/projects/${f.project_id}`}>{f.project_name || f.project_id}</a>
                    ) : '-'}
                  </td>
                  <td><Badge variant={f.kind === 'agent' ? 'default' : 'warning'}>{f.kind}</Badge></td>
                  <td><code>{f.pod_name || '-'}</code></td>
                  <td style={{ maxWidth: 300, overflow: 'hidden', textOverflow: 'ellipsis' }}>
                    {f.error || '-'}
                  </td>
                  <td>{timeAgo(f.failed_at)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}

import { useState, useEffect } from 'preact/hooks';
import { api } from '../lib/api';

interface Subsystem {
  name: string;
  status: string;
  latency_ms: number;
  message: string | null;
}

interface HealthSnapshot {
  overall: string;
  uptime_seconds: number;
  subsystems: Subsystem[];
}

export function SystemHealthPanel() {
  const [health, setHealth] = useState<HealthSnapshot | null>(null);

  useEffect(() => {
    api.get<HealthSnapshot>('/api/health/details')
      .then(setHealth)
      .catch(() => {}); // hidden for non-admins (403)
  }, []);

  if (!health) return null;

  const dot = (status: string) => {
    if (status === 'healthy') return '#22c55e';
    if (status === 'degraded') return '#eab308';
    return '#ef4444';
  };

  const uptime = health.uptime_seconds;
  const days = Math.floor(uptime / 86400);
  const hours = Math.floor((uptime % 86400) / 3600);
  const uptimeStr = days > 0 ? `${days}d ${hours}h` : `${hours}h`;

  return (
    <div class="panel">
      <div class="panel-header">
        <span>System Health</span>
        <span class="badge" style={`background:${dot(health.overall)}22;color:${dot(health.overall)}`}>
          {health.overall}
        </span>
      </div>
      <div class="panel-body">
        {health.subsystems.map(s => (
          <div key={s.name} class="health-row">
            <span class="status-dot" style={`background:${dot(s.status)}`} />
            <span class="health-name">{s.name}</span>
            <span class="health-latency">{s.latency_ms}ms</span>
          </div>
        ))}
        <div class="health-uptime">Uptime: {uptimeStr}</div>
      </div>
    </div>
  );
}

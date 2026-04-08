import { useState, useEffect, useRef, useCallback } from 'preact/hooks';
import { Overlay } from './Overlay';

// ---- Types ----

type Env = 'staging' | 'production';

interface ComponentHealth {
  name: string;
  ready: boolean;
  live: boolean;
  startup: boolean;
  cpu: { used: number; request: number; limit: number };
  mem: { used: number; request: number; limit: number };
  activeRequests: number;
  avgRps: number;
  replicas: number;
  readyReplicas: number;
  restarts: number;
  oomKills: number;
  cpuHistory: number[];   // last 20 readings
  memHistory: number[];
  rpsHistory: number[];
}

interface CommEdge { from: string; to: string; calls: number; errors: number; p50: number; }

interface LoadPoint { ts: number; rps: number; errors: number; p99: number; }
interface DeployMarker { ts: number; image: string; env: string; }

interface TraceRow {
  id: string;
  name: string;
  status: 'ok' | 'error' | 'running';
  duration: number | null;
  spans: number;
  ts: number;
}

interface SpanRow {
  id: string;
  name: string;
  service: string;
  start: number;
  duration: number;
  status: 'ok' | 'error';
  depth: number;
  logs: string[];
}

interface AggTrace { name: string; count: number; avgDuration: number; errorRate: number; p99: number; }

interface ErrorGroup { type: string; endpoint: string; downstream: string; count: number; lastSeen: string; }

interface LogTemplate { template: string; count: number; level: string; sample: string; }

interface AlertRow { name: string; status: 'firing' | 'resolved' | 'silenced'; severity: string; since: string; message: string; }

interface SloRow { name: string; target: number; current: number; budgetRemaining: number; window: string; }

// ---- Mock data ----

function mockComponents(env: Env): ComponentHealth[] {
  const s = env === 'staging' ? 0.5 : 1;
  const spark = (base: number, n: number) => Array.from({ length: n }, () => Math.round(base + (Math.random() - 0.5) * base * 0.4));
  return [
    { name: 'web', ready: true, live: true, startup: true, cpu: { used: Math.round(120 * s), request: 250, limit: 500 }, mem: { used: Math.round(180 * s), request: 256, limit: 512 }, activeRequests: Math.round(12 * s), avgRps: +(8.3 * s).toFixed(1), replicas: env === 'production' ? 3 : 1, readyReplicas: env === 'production' ? 3 : 1, restarts: 0, oomKills: 0, cpuHistory: spark(120 * s, 20), memHistory: spark(180 * s, 20), rpsHistory: spark(8.3 * s, 20) },
    { name: 'worker', ready: true, live: true, startup: true, cpu: { used: Math.round(45 * s), request: 100, limit: 250 }, mem: { used: Math.round(95 * s), request: 128, limit: 256 }, activeRequests: Math.round(3 * s), avgRps: +(1.2 * s).toFixed(1), replicas: 1, readyReplicas: 1, restarts: env === 'production' ? 2 : 0, oomKills: env === 'production' ? 1 : 0, cpuHistory: spark(45 * s, 20), memHistory: spark(95 * s, 20), rpsHistory: spark(1.2 * s, 20) },
    { name: 'db', ready: true, live: true, startup: true, cpu: { used: Math.round(200 * s), request: 500, limit: 1000 }, mem: { used: Math.round(420 * s), request: 512, limit: 1024 }, activeRequests: Math.round(24 * s), avgRps: +(15.7 * s).toFixed(1), replicas: 1, readyReplicas: 1, restarts: 0, oomKills: 0, cpuHistory: spark(200 * s, 20), memHistory: spark(420 * s, 20), rpsHistory: spark(15.7 * s, 20) },
    { name: 'cache', ready: true, live: true, startup: true, cpu: { used: Math.round(15 * s), request: 50, limit: 100 }, mem: { used: Math.round(64 * s), request: 128, limit: 256 }, activeRequests: Math.round(18 * s), avgRps: +(22.1 * s).toFixed(1), replicas: 1, readyReplicas: 1, restarts: 0, oomKills: 0, cpuHistory: spark(15 * s, 20), memHistory: spark(64 * s, 20), rpsHistory: spark(22.1 * s, 20) },
  ];
}

function mockLoad(rangeMs: number): LoadPoint[] {
  const now = Date.now();
  const points = 120;
  const step = rangeMs / points;
  return Array.from({ length: points }, (_, i) => {
    const t = now - rangeMs + i * step;
    const base = 20 + Math.sin(i / 8) * 10 + Math.sin(i / 25) * 5;
    return { ts: t, rps: +(base + Math.random() * 5).toFixed(1), errors: Math.random() < 0.08 ? Math.round(Math.random() * 3) : 0, p99: +(50 + Math.random() * 80).toFixed(1) };
  });
}

function mockDeploys(): DeployMarker[] {
  const now = Date.now();
  return [
    { ts: now - 2400000, image: 'app:0.2.0', env: 'production' },
    { ts: now - 7200000, image: 'app:0.1.9', env: 'staging' },
    { ts: now - 18000000, image: 'app:0.1.8', env: 'production' },
  ];
}

function mockEdges(): CommEdge[] {
  return [
    { from: 'web', to: 'db', calls: 342, errors: 2, p50: 12 },
    { from: 'web', to: 'cache', calls: 891, errors: 0, p50: 1 },
    { from: 'web', to: 'worker', calls: 56, errors: 1, p50: 45 },
    { from: 'worker', to: 'db', calls: 128, errors: 0, p50: 18 },
  ];
}

function mockTraces(tab: string): TraceRow[] {
  const now = Date.now();
  if (tab === 'ongoing') return [
    { id: 'a1', name: 'POST /api/orders', status: 'running', duration: null, spans: 4, ts: now - 2000 },
    { id: 'a2', name: 'GET /api/products', status: 'running', duration: null, spans: 2, ts: now - 1000 },
  ];
  return [
    { id: 'b1', name: 'GET /api/products', status: 'ok', duration: 23, spans: 3, ts: now - 5000 },
    { id: 'b2', name: 'POST /api/orders', status: 'ok', duration: 187, spans: 8, ts: now - 12000 },
    { id: 'b3', name: 'GET /api/cart', status: 'ok', duration: 8, spans: 2, ts: now - 15000 },
    { id: 'b4', name: 'POST /api/auth/login', status: 'error', duration: 340, spans: 5, ts: now - 22000 },
    { id: 'b5', name: 'GET /api/products', status: 'ok', duration: 19, spans: 3, ts: now - 28000 },
    { id: 'b6', name: 'DELETE /api/cart/items/3', status: 'ok', duration: 14, spans: 3, ts: now - 35000 },
    { id: 'b7', name: 'GET /healthz', status: 'ok', duration: 1, spans: 1, ts: now - 40000 },
    { id: 'b8', name: 'POST /api/orders', status: 'error', duration: 520, spans: 7, ts: now - 52000 },
  ];
}

function mockSpans(traceId: string): SpanRow[] {
  const isError = traceId === 'b4' || traceId === 'b8';
  return [
    { id: 's1', name: traceId.startsWith('b4') ? 'POST /api/auth/login' : 'POST /api/orders', service: 'web', start: 0, duration: isError ? 340 : 187, status: isError ? 'error' : 'ok', depth: 0, logs: ['request received', `user-agent: Mozilla/5.0`] },
    { id: 's2', name: 'auth.validate_token', service: 'web', start: 2, duration: isError ? 15 : 8, status: 'ok', depth: 1, logs: ['token validated'] },
    { id: 's3', name: 'SELECT FROM users', service: 'db', start: 5, duration: 12, status: 'ok', depth: 1, logs: ['rows returned: 1'] },
    { id: 's4', name: isError ? 'password.verify' : 'INSERT INTO orders', service: isError ? 'web' : 'db', start: 20, duration: isError ? 280 : 45, status: isError ? 'error' : 'ok', depth: 1, logs: isError ? ['argon2 verify failed', 'ERROR: invalid credentials'] : ['order_id: 7a3f...'] },
    { id: 's5', name: 'cache.get session', service: 'cache', start: 3, duration: 1, status: 'ok', depth: 1, logs: ['hit'] },
  ];
}

function mockAggregated(): AggTrace[] {
  return [
    { name: 'GET /api/products', count: 1247, avgDuration: 22, errorRate: 0.2, p99: 89 },
    { name: 'POST /api/orders', count: 84, avgDuration: 195, errorRate: 2.4, p99: 450 },
    { name: 'GET /api/cart', count: 523, avgDuration: 9, errorRate: 0, p99: 35 },
    { name: 'POST /api/auth/login', count: 312, avgDuration: 110, errorRate: 4.5, p99: 380 },
    { name: 'GET /healthz', count: 8640, avgDuration: 1, errorRate: 0, p99: 3 },
    { name: 'DELETE /api/cart/items/:id', count: 67, avgDuration: 15, errorRate: 0, p99: 42 },
  ];
}

function mockErrors(): ErrorGroup[] {
  return [
    { type: '401 Unauthorized', endpoint: 'POST /api/auth/login', downstream: '-', count: 14, lastSeen: '22s ago' },
    { type: '500 Internal', endpoint: 'POST /api/orders', downstream: 'db', count: 3, lastSeen: '52s ago' },
    { type: 'Timeout', endpoint: 'GET /api/products', downstream: 'cache', count: 2, lastSeen: '3m ago' },
    { type: '503 Unavailable', endpoint: 'POST /api/orders', downstream: 'worker', count: 1, lastSeen: '8m ago' },
  ];
}

function mockLogTemplates(): LogTemplate[] {
  return [
    { template: 'request {method} {path} completed in {duration}ms', count: 2341, level: 'info', sample: 'request GET /api/products completed in 23ms' },
    { template: 'auth failed for user {email}: {reason}', count: 14, level: 'warn', sample: 'auth failed for user test@example.com: invalid credentials' },
    { template: 'order {id} created, total={total}', count: 84, level: 'info', sample: 'order 7a3f created, total=49.99' },
    { template: 'cache {op} key={key} hit={hit}', count: 1891, level: 'debug', sample: 'cache GET key=products:list hit=true' },
    { template: 'db query {table} took {ms}ms rows={rows}', count: 892, level: 'debug', sample: 'db query orders took 12ms rows=1' },
    { template: 'ERROR: {message}', count: 5, level: 'error', sample: 'ERROR: connection refused to worker:8080' },
  ];
}

function mockAlerts(): AlertRow[] {
  return [
    { name: 'High error rate on login', status: 'firing', severity: 'warning', since: '15m ago', message: 'Error rate > 3% for POST /api/auth/login over 5m window' },
    { name: 'Worker OOM restarts', status: 'resolved', severity: 'critical', since: '2h ago', message: 'Worker pod restarted due to OOMKilled (resolved after scale-up)' },
    { name: 'P99 latency > 500ms', status: 'silenced', severity: 'warning', since: '1h ago', message: 'POST /api/orders p99 exceeds SLO threshold' },
  ];
}

function mockSlos(): SloRow[] {
  return [
    { name: 'Availability', target: 99.9, current: 99.82, budgetRemaining: 34, window: '30d' },
    { name: 'Latency (p99 < 500ms)', target: 99.0, current: 98.1, budgetRemaining: 12, window: '30d' },
    { name: 'Error rate (< 1%)', target: 99.0, current: 99.5, budgetRemaining: 78, window: '7d' },
  ];
}

interface StandaloneLog {
  id: string;
  ts: number;
  level: string;
  source: string;
  message: string;
}

function mockStandaloneLogs(): StandaloneLog[] {
  const now = Date.now();
  return [
    { id: 'sl1', ts: now - 3000, level: 'info', source: 'kubelet', message: 'Pod web-7f8b9c-xk2q4 started' },
    { id: 'sl2', ts: now - 8000, level: 'warn', source: 'kubelet', message: 'Liveness probe failed for worker-5d9a1c-m3j8z (timeout after 5s)' },
    { id: 'sl3', ts: now - 12000, level: 'info', source: 'deployer', message: 'Release app:0.2.0 promoted to 100% traffic' },
    { id: 'sl4', ts: now - 25000, level: 'error', source: 'kubelet', message: 'Container worker OOMKilled (limit: 256Mi, usage: 261Mi)' },
    { id: 'sl5', ts: now - 30000, level: 'info', source: 'deployer', message: 'Canary app:0.2.0 traffic set to 50%' },
    { id: 'sl6', ts: now - 45000, level: 'info', source: 'hpa', message: 'Scaled web from 2 to 3 replicas (cpu utilization 78%)' },
    { id: 'sl7', ts: now - 60000, level: 'debug', source: 'deployer', message: 'Health check passed for app:0.2.0 canary' },
    { id: 'sl8', ts: now - 90000, level: 'info', source: 'deployer', message: 'Canary app:0.2.0 traffic set to 10%' },
    { id: 'sl9', ts: now - 120000, level: 'info', source: 'pipeline', message: 'Build app:0.2.0 completed (success)' },
    { id: 'sl10', ts: now - 180000, level: 'warn', source: 'kubelet', message: 'Image pull backoff for registry.local/app:0.2.0-rc1 (retrying in 30s)' },
  ];
}

interface MetricSeries {
  service: string;
  color: string;
  points: { ts: number; value: number }[];
}

function mockMetricSeries(rangeMs: number, services: { name: string; base: number; color: string }[]): MetricSeries[] {
  const now = Date.now();
  const n = 60;
  const step = rangeMs / n;
  return services.map(s => ({
    service: s.name,
    color: s.color,
    points: Array.from({ length: n }, (_, i) => ({
      ts: now - rangeMs + i * step,
      value: Math.max(0, s.base + Math.sin(i / 7) * s.base * 0.3 + (Math.random() - 0.5) * s.base * 0.2),
    })),
  }));
}

function mockCpuSeries(rangeMs: number): MetricSeries[] {
  return mockMetricSeries(rangeMs, [
    { name: 'web', base: 120, color: '#3b82f6' },
    { name: 'db', base: 200, color: '#f97316' },
    { name: 'worker', base: 45, color: '#a855f7' },
    { name: 'cache', base: 15, color: '#22c55e' },
  ]);
}

function mockMemSeries(rangeMs: number): MetricSeries[] {
  return mockMetricSeries(rangeMs, [
    { name: 'web', base: 180, color: '#3b82f6' },
    { name: 'db', base: 420, color: '#f97316' },
    { name: 'worker', base: 95, color: '#a855f7' },
    { name: 'cache', base: 64, color: '#22c55e' },
  ]);
}

function mockResponseSeries(rangeMs: number): MetricSeries[] {
  return mockMetricSeries(rangeMs, [
    { name: 'web', base: 25, color: '#3b82f6' },
    { name: 'db', base: 12, color: '#f97316' },
    { name: 'worker', base: 80, color: '#a855f7' },
    { name: 'cache', base: 2, color: '#22c55e' },
  ]);
}

// ---- Helpers ----

function relTime(ts: number): string {
  const s = Math.round((Date.now() - ts) / 1000);
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  return `${Math.floor(s / 3600)}h ago`;
}

function fmtTime(ts: number): string {
  const d = new Date(ts);
  return d.toLocaleTimeString('en-US', { hour12: false, hour: '2-digit', minute: '2-digit' });
}

const RANGE_MS: Record<string, number> = { '5m': 300000, '15m': 900000, '1h': 3600000, '6h': 21600000, '24h': 86400000 };

// ---- Sub-components ----

function Sparkline({ data, height = 20, color = 'var(--accent)' }: { data: number[]; height?: number; color?: string }) {
  if (data.length < 2) return null;
  const max = Math.max(...data, 1);
  const w = 60;
  const pts = data.map((v, i) => `${(i / (data.length - 1)) * w},${height - (v / max) * (height - 2)}`).join(' ');
  return <svg viewBox={`0 0 ${w} ${height}`} width={w} height={height} style="display:block"><polyline points={pts} fill="none" stroke={color} stroke-width="1.5" /></svg>;
}

function MetricChart({ series, unit, height = 160 }: { series: MetricSeries[]; unit: string; height?: number }) {
  if (series.length === 0 || series[0].points.length < 2) return null;
  const W = 800;
  const H = height;
  const PAD = 22;
  const chartH = H - PAD;

  const allValues = series.flatMap(s => s.points.map(p => p.value));
  const maxVal = Math.max(...allValues, 1);
  const minTs = series[0].points[0].ts;
  const maxTs = series[0].points[series[0].points.length - 1].ts;
  const tsRange = maxTs - minTs || 1;

  const x = (ts: number) => ((ts - minTs) / tsRange) * W;
  const y = (v: number) => chartH - (v / maxVal) * (chartH - 8);

  // Time labels
  const labelCount = 5;
  const labelStep = tsRange / labelCount;
  const labels = Array.from({ length: labelCount + 1 }, (_, i) => minTs + i * labelStep);

  return (
    <div>
      <svg viewBox={`0 0 ${W} ${H}`} class="obs-metric-svg" preserveAspectRatio="none">
        {/* Grid lines */}
        {[0.25, 0.5, 0.75].map(frac => (
          <line key={frac} x1={0} y1={chartH * (1 - frac)} x2={W} y2={chartH * (1 - frac)}
            stroke="var(--border)" stroke-width="0.5" opacity="0.4" />
        ))}
        {/* Series */}
        {series.map(s => {
          const path = s.points.map((p, i) => `${i === 0 ? 'M' : 'L'}${x(p.ts)},${y(p.value)}`).join(' ');
          return <path key={s.service} d={path} fill="none" stroke={s.color} stroke-width="1.5" opacity="0.85" />;
        })}
        {/* Y max */}
        <text x="4" y="12" fill="var(--text-muted)" font-size="9">{maxVal.toFixed(0)} {unit}</text>
        {/* Time labels */}
        {labels.map((ts, i) => (
          <text key={i} x={x(ts)} y={H - 4} fill="var(--text-muted)" font-size="9" text-anchor="middle">{fmtTime(ts)}</text>
        ))}
        <line x1={0} y1={chartH} x2={W} y2={chartH} stroke="var(--border)" stroke-width="0.5" />
      </svg>
      {/* Legend */}
      <div class="obs-metric-legend">
        {series.map(s => (
          <span key={s.service} class="obs-metric-legend-item">
            <span class="obs-metric-legend-dot" style={`background:${s.color}`} />
            {s.service}
          </span>
        ))}
      </div>
    </div>
  );
}

function ProbesDot({ ok, label }: { ok: boolean; label: string }) {
  return (
    <span class="obs-probe" title={`${label}: ${ok ? 'passing' : 'failing'}`}>
      <span class="obs-probe-dot" style={`background:${ok ? 'var(--success)' : 'var(--danger)'}`} />
      <span class="text-xs">{label[0].toUpperCase()}</span>
    </span>
  );
}

function ResourceBar({ label, used, request, limit }: { label: string; used: number; request: number; limit: number }) {
  const pct = Math.min((used / limit) * 100, 100);
  const reqPct = (request / limit) * 100;
  const color = pct > 90 ? 'var(--danger)' : pct > 70 ? 'var(--warning)' : 'var(--success)';
  return (
    <div class="obs-resource">
      <div class="obs-resource-header">
        <span class="text-xs text-muted">{label}</span>
        <span class="text-xs mono">{used}m / {limit}m</span>
      </div>
      <div class="obs-resource-bar">
        <div class="obs-resource-fill" style={`width:${pct}%;background:${color}`} />
        <div class="obs-resource-request" style={`left:${reqPct}%`} title={`Request: ${request}m`} />
      </div>
    </div>
  );
}

function CommGraph({ edges, components }: { edges: CommEdge[]; components: ComponentHealth[] }) {
  const positions: Record<string, { x: number; y: number }> = {
    web: { x: 200, y: 50 }, worker: { x: 400, y: 50 },
    db: { x: 120, y: 170 }, cache: { x: 320, y: 170 },
  };
  components.forEach((c, i) => { if (!positions[c.name]) positions[c.name] = { x: 100 + i * 150, y: 110 }; });

  return (
    <svg viewBox="0 0 520 230" class="obs-comm-svg">
      {edges.map((e, i) => {
        const from = positions[e.from];
        const to = positions[e.to];
        if (!from || !to) return null;
        const hasErrors = e.errors > 0;
        const color = hasErrors ? 'var(--danger)' : 'var(--success)';
        const mx = (from.x + to.x) / 2;
        const my = (from.y + to.y) / 2 - 6;
        return (
          <g key={i}>
            <line x1={from.x} y1={from.y} x2={to.x} y2={to.y}
              stroke={color} stroke-width={Math.min(Math.max(e.calls / 100, 1.5), 5)}
              opacity={hasErrors ? 0.9 : 0.5} stroke-linecap="round" />
            <text x={mx} y={my} text-anchor="middle" fill="var(--text-muted)" font-size="9">
              {e.calls}{e.errors > 0 ? ` (${e.errors} err)` : ''} · {e.p50}ms
            </text>
          </g>
        );
      })}
      {components.map(c => {
        const pos = positions[c.name];
        if (!pos) return null;
        const healthy = c.ready && c.live;
        return (
          <g key={c.name}>
            <circle cx={pos.x} cy={pos.y} r="28"
              fill={healthy ? 'rgba(34,197,94,0.12)' : 'rgba(239,68,68,0.12)'}
              stroke={healthy ? 'var(--success)' : 'var(--danger)'} stroke-width="1.5" />
            <text x={pos.x} y={pos.y + 1} text-anchor="middle" dominant-baseline="middle"
              fill="var(--text-primary)" font-size="12" font-weight="500">{c.name}</text>
            <text x={pos.x} y={pos.y + 40} text-anchor="middle" fill="var(--text-muted)" font-size="9">
              {c.readyReplicas}/{c.replicas} · {c.avgRps} rps
            </text>
          </g>
        );
      })}
    </svg>
  );
}

// ---- Timeline (zoomable, selectable, with deploy markers) ----

function Timeline({ points, deploys, selectedRange, onSelectRange, onResetRange }: {
  points: LoadPoint[];
  deploys: DeployMarker[];
  selectedRange: [number, number] | null;
  onSelectRange: (range: [number, number]) => void;
  onResetRange: () => void;
}) {
  const svgRef = useRef<SVGSVGElement>(null);
  const [dragging, setDragging] = useState(false);
  const [dragStart, setDragStart] = useState(0);
  const [dragEnd, setDragEnd] = useState(0);
  // Zoom: viewport into the data as fraction [0..1]
  const [viewStart, setViewStart] = useState(0);
  const [viewEnd, setViewEnd] = useState(1);

  if (points.length < 2) return null;

  const allTs = points.map(p => p.ts);
  const fullMin = allTs[0];
  const fullMax = allTs[allTs.length - 1];
  const fullRange = fullMax - fullMin;

  // Visible slice
  const vMin = fullMin + fullRange * viewStart;
  const vMax = fullMin + fullRange * viewEnd;
  const visible = points.filter(p => p.ts >= vMin && p.ts <= vMax);

  const W = 900;
  const H = 140;
  const PAD_BOTTOM = 22;
  const chartH = H - PAD_BOTTOM;
  const maxRps = Math.max(...visible.map(p => p.rps), 1);

  const xForTs = (ts: number) => ((ts - vMin) / (vMax - vMin)) * W;
  const tsForX = (x: number) => vMin + (x / W) * (vMax - vMin);

  const rpsPath = visible.map((p, i) => `${i === 0 ? 'M' : 'L'}${xForTs(p.ts)},${chartH - (p.rps / maxRps) * (chartH - 10)}`).join(' ');
  const areaPath = visible.length > 0 ? rpsPath + ` L${xForTs(visible[visible.length - 1].ts)},${chartH} L${xForTs(visible[0].ts)},${chartH} Z` : '';

  const errorDots = visible.filter(p => p.errors > 0).map(p => ({ x: xForTs(p.ts), y: chartH - (p.rps / maxRps) * (chartH - 10), e: p.errors }));

  // Deploy markers in view
  const visibleDeploys = deploys.filter(d => d.ts >= vMin && d.ts <= vMax);

  // Time labels (5-8 labels across the bottom)
  const labelCount = 6;
  const labelStep = (vMax - vMin) / labelCount;
  const timeLabels = Array.from({ length: labelCount + 1 }, (_, i) => vMin + i * labelStep);

  // Selection box
  const selX1 = dragging ? Math.min(dragStart, dragEnd) : 0;
  const selX2 = dragging ? Math.max(dragStart, dragEnd) : 0;

  // Selected range highlight
  const hasSelection = selectedRange && selectedRange[0] >= vMin && selectedRange[1] <= vMax;
  const selRangeX1 = hasSelection ? xForTs(selectedRange![0]) : 0;
  const selRangeX2 = hasSelection ? xForTs(selectedRange![1]) : 0;

  const getSvgX = (e: MouseEvent): number => {
    const svg = svgRef.current;
    if (!svg) return 0;
    const rect = svg.getBoundingClientRect();
    return ((e.clientX - rect.left) / rect.width) * W;
  };

  const handleMouseDown = (e: MouseEvent) => {
    if (e.button !== 0) return;
    const x = getSvgX(e);
    setDragging(true);
    setDragStart(x);
    setDragEnd(x);
  };

  const handleMouseMove = (e: MouseEvent) => {
    if (!dragging) return;
    setDragEnd(getSvgX(e));
  };

  const handleMouseUp = () => {
    if (!dragging) return;
    setDragging(false);
    const x1 = Math.min(dragStart, dragEnd);
    const x2 = Math.max(dragStart, dragEnd);
    if (x2 - x1 > 10) {
      onSelectRange([tsForX(x1), tsForX(x2)]);
    }
  };

  const handleWheel = useCallback((e: WheelEvent) => {
    e.preventDefault();
    const svg = svgRef.current;
    if (!svg) return;
    const rect = svg.getBoundingClientRect();
    const mouseRatio = (e.clientX - rect.left) / rect.width;
    const currentSpan = viewEnd - viewStart;
    const factor = e.deltaY > 0 ? 1.15 : 0.87;
    const newSpan = Math.min(Math.max(currentSpan * factor, 0.01), 1);
    const pivot = viewStart + mouseRatio * currentSpan;
    let newStart = pivot - mouseRatio * newSpan;
    let newEnd = pivot + (1 - mouseRatio) * newSpan;
    if (newStart < 0) { newEnd -= newStart; newStart = 0; }
    if (newEnd > 1) { newStart -= (newEnd - 1); newEnd = 1; }
    setViewStart(Math.max(0, newStart));
    setViewEnd(Math.min(1, newEnd));
  }, [viewStart, viewEnd]);

  useEffect(() => {
    const svg = svgRef.current;
    if (!svg) return;
    svg.addEventListener('wheel', handleWheel, { passive: false });
    return () => svg.removeEventListener('wheel', handleWheel);
  }, [handleWheel]);

  // Reset zoom when parent resets range
  useEffect(() => { setViewStart(0); setViewEnd(1); }, [points.length]);

  return (
    <div>
      <svg ref={svgRef} viewBox={`0 0 ${W} ${H}`} class="obs-timeline-svg" preserveAspectRatio="none"
        onMouseDown={handleMouseDown} onMouseMove={handleMouseMove}
        onMouseUp={handleMouseUp} onMouseLeave={() => { if (dragging) handleMouseUp(); }}>
        <defs>
          <linearGradient id="tlGrad" x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stop-color="var(--accent)" stop-opacity="0.25" />
            <stop offset="100%" stop-color="var(--accent)" stop-opacity="0.02" />
          </linearGradient>
        </defs>

        {/* Area + line */}
        {areaPath && <path d={areaPath} fill="url(#tlGrad)" />}
        {rpsPath && <path d={rpsPath} fill="none" stroke="var(--accent)" stroke-width="1.5" />}

        {/* Error dots */}
        {errorDots.map((d, i) => <circle key={i} cx={d.x} cy={d.y} r="3.5" fill="var(--danger)" opacity="0.85" />)}

        {/* Deploy markers */}
        {visibleDeploys.map((d, i) => {
          const x = xForTs(d.ts);
          return (
            <g key={i}>
              <line x1={x} y1={0} x2={x} y2={chartH} stroke="var(--accent)" stroke-width="1" stroke-dasharray="4,3" opacity="0.6" />
              <text x={x + 3} y={10} fill="var(--accent)" font-size="8" opacity="0.8">{d.image}</text>
            </g>
          );
        })}

        {/* Selected range highlight */}
        {hasSelection && (
          <rect x={selRangeX1} y={0} width={selRangeX2 - selRangeX1} height={chartH}
            fill="var(--accent)" opacity="0.08" />
        )}

        {/* Drag selection */}
        {dragging && selX2 - selX1 > 2 && (
          <rect x={selX1} y={0} width={selX2 - selX1} height={chartH}
            fill="var(--accent)" opacity="0.15" stroke="var(--accent)" stroke-width="1" />
        )}

        {/* Y-axis label */}
        <text x="4" y="12" fill="var(--text-muted)" font-size="9">{maxRps.toFixed(0)} rps</text>

        {/* Time labels */}
        {timeLabels.map((ts, i) => (
          <text key={i} x={xForTs(ts)} y={H - 4} fill="var(--text-muted)" font-size="9" text-anchor="middle">
            {fmtTime(ts)}
          </text>
        ))}

        {/* Axis line */}
        <line x1={0} y1={chartH} x2={W} y2={chartH} stroke="var(--border)" stroke-width="0.5" />
      </svg>
      <div class="text-xs text-muted" style="text-align:center;margin-top:0.25rem">
        Scroll to zoom · Click and drag to select time range
        {selectedRange && (
          <> · <strong>{fmtTime(selectedRange[0])} — {fmtTime(selectedRange[1])}</strong>
            <button class="btn btn-sm" style="margin-left:0.5rem;padding:0.1rem 0.4rem;font-size:0.65rem" onClick={onResetRange}>Clear</button>
          </>
        )}
      </div>
    </div>
  );
}

// ---- Trace detail waterfall ----

function TraceWaterfall({ spans, traceName }: { spans: SpanRow[]; traceName: string }) {
  if (spans.length === 0) return <div class="text-muted text-sm">No spans</div>;
  const maxEnd = Math.max(...spans.map(s => s.start + s.duration));

  return (
    <div>
      <div class="obs-waterfall">
        {spans.map(s => {
          const leftPct = (s.start / maxEnd) * 100;
          const widthPct = Math.max((s.duration / maxEnd) * 100, 0.5);
          const color = s.status === 'error' ? 'var(--danger)' : 'var(--accent)';
          return (
            <div key={s.id} class="obs-waterfall-row">
              <div class="obs-waterfall-label" style={`padding-left:${s.depth * 1}rem`}>
                <span class="text-xs mono">{s.service}</span>
                <span class="text-xs">{s.name}</span>
              </div>
              <div class="obs-waterfall-bar-container">
                <div class="obs-waterfall-bar" style={`left:${leftPct}%;width:${widthPct}%;background:${color}`} />
                <span class="obs-waterfall-dur" style={`left:${leftPct + widthPct + 0.5}%`}>
                  {s.duration}ms
                </span>
              </div>
            </div>
          );
        })}
      </div>
      {/* Span logs */}
      <h4 style="font-size:0.85rem;margin-top:1rem;margin-bottom:0.5rem">Span Logs</h4>
      <div class="obs-span-logs">
        {spans.filter(s => s.logs.length > 0).map(s => (
          <div key={s.id} class="obs-span-log-group">
            <div class="text-xs" style="font-weight:500;margin-bottom:0.2rem">{s.service} / {s.name}</div>
            {s.logs.map((log, i) => (
              <div key={i} class={`obs-span-log-line${log.startsWith('ERROR') ? ' obs-log-error' : ''}`}>{log}</div>
            ))}
          </div>
        ))}
      </div>
    </div>
  );
}

// ---- Main ObserveTab ----

export function ObserveTab({ projectId }: { projectId: string }) {
  const [env, setEnv] = useState<Env>('production');
  const [presetRange, setPresetRange] = useState('1h');
  const [selectedRange, setSelectedRange] = useState<[number, number] | null>(null);
  const [traceTab, setTraceTab] = useState<'ongoing' | 'recent' | 'aggregated'>('recent');
  const [traceFilter, setTraceFilter] = useState<string | null>(null);
  const [selectedTrace, setSelectedTrace] = useState<TraceRow | null>(null);
  const [logLevel, setLogLevel] = useState('');
  const [logSearch, setLogSearch] = useState('');
  const [traceSpans, setTraceSpans] = useState<SpanRow[]>([]);

  const rangeMs = RANGE_MS[presetRange] || 3600000;
  const components = mockComponents(env);
  const load = mockLoad(rangeMs);
  const deploys = mockDeploys();
  const edges = mockEdges();
  const traces = traceTab === 'aggregated' ? [] : mockTraces(traceTab);
  const aggregated = traceTab === 'aggregated' ? mockAggregated() : [];
  const errors = mockErrors();
  const logTemplates = mockLogTemplates();
  const standaloneLogs = mockStandaloneLogs();
  const cpuSeries = mockCpuSeries(rangeMs);
  const memSeries = mockMemSeries(rangeMs);
  const responseSeries = mockResponseSeries(rangeMs);

  // Filter traces by error drill-down
  const filteredTraces = traceFilter
    ? traces.filter(t => t.name === traceFilter && t.status === 'error')
    : traces;

  // Filter standalone logs
  const filteredLogs = standaloneLogs.filter(l =>
    (!logLevel || l.level === logLevel) &&
    (!logSearch || l.message.toLowerCase().includes(logSearch.toLowerCase()))
  );
  const alerts = mockAlerts();
  const slos = mockSlos();

  const avgP99 = load.length > 0 ? Math.round(load.reduce((s, p) => s + p.p99, 0) / load.length) : 0;
  const totalErrors = load.reduce((s, p) => s + p.errors, 0);
  const avgRps = load.length > 0 ? +(load.reduce((s, p) => s + p.rps, 0) / load.length).toFixed(1) : 0;

  const resetRange = () => { setSelectedRange(null); };
  const applyPreset = (r: string) => { setPresetRange(r); setSelectedRange(null); };

  const openTrace = (t: TraceRow) => {
    setSelectedTrace(t);
    setTraceSpans(mockSpans(t.id));
  };

  const alertColor = (s: string) => s === 'firing' ? 'var(--danger)' : s === 'silenced' ? 'var(--text-muted)' : 'var(--success)';

  return (
    <div class="obs-dashboard">
      {/* ---- Top bar ---- */}
      <div class="obs-topbar">
        <div class="obs-env-toggle">
          <button class={`obs-env-btn${env === 'staging' ? ' active staging' : ''}`} onClick={() => setEnv('staging')}>Staging</button>
          <button class={`obs-env-btn${env === 'production' ? ' active production' : ''}`} onClick={() => setEnv('production')}>Production</button>
        </div>
        <div class="obs-stats-row">
          <span class="obs-stat"><span class="obs-stat-value">{avgRps}</span> <span class="obs-stat-label">avg rps</span></span>
          <span class="obs-stat"><span class="obs-stat-value">{avgP99}ms</span> <span class="obs-stat-label">p99</span></span>
          <span class={`obs-stat${totalErrors > 0 ? ' obs-stat-danger' : ''}`}><span class="obs-stat-value">{totalErrors}</span> <span class="obs-stat-label">errors</span></span>
        </div>
      </div>

      {/* ==== LIVE SECTION (top) ==== */}

      {/* SLO / Alerts bar */}
      <div class="obs-slo-alert-bar">
        <div class="obs-slos">
          {slos.map(s => {
            const ok = s.current >= s.target;
            return (
              <div key={s.name} class="obs-slo-chip" title={`${s.name}: ${s.current}% (target ${s.target}%, ${s.budgetRemaining}% budget left, ${s.window})`}>
                <span class="obs-slo-dot" style={`background:${ok ? 'var(--success)' : 'var(--danger)'}`} />
                <span class="text-xs">{s.name}</span>
                <span class={`text-xs mono ${ok ? '' : 'text-danger'}`}>{s.current}%</span>
                <span class="text-xs text-muted">({s.budgetRemaining}% left)</span>
              </div>
            );
          })}
        </div>
        <div class="obs-alerts-summary">
          {alerts.filter(a => a.status === 'firing').length > 0 && (
            <span class="obs-alert-badge firing">{alerts.filter(a => a.status === 'firing').length} firing</span>
          )}
          {alerts.filter(a => a.status === 'silenced').length > 0 && (
            <span class="obs-alert-badge silenced">{alerts.filter(a => a.status === 'silenced').length} silenced</span>
          )}
        </div>
      </div>

      {/* Components + Communication (live) */}
      <div class="obs-split">
        <div class="obs-components">
          <h4 class="obs-section-title">Components <span class="text-xs text-muted" style="font-weight:400">live</span></h4>
          <div class="obs-comp-grid">
            {components.map(c => (
              <div key={c.name} class="obs-comp-card">
                <div class="obs-comp-header">
                  <span class="obs-comp-name">{c.name}</span>
                  <div class="obs-probes">
                    <ProbesDot ok={c.startup} label="startup" />
                    <ProbesDot ok={c.ready} label="ready" />
                    <ProbesDot ok={c.live} label="live" />
                  </div>
                </div>
                <div class="obs-comp-meta">
                  <span class="text-xs text-muted">{c.readyReplicas}/{c.replicas} replicas</span>
                  {c.restarts > 0 && <span class="text-xs text-warning">{c.restarts} restarts</span>}
                  {c.oomKills > 0 && <span class="text-xs text-danger">{c.oomKills} OOM</span>}
                </div>
                <ResourceBar label="CPU" used={c.cpu.used} request={c.cpu.request} limit={c.cpu.limit} />
                <ResourceBar label="MEM" used={c.mem.used} request={c.mem.request} limit={c.mem.limit} />
                <div class="obs-comp-sparks">
                  <div class="obs-spark-item"><span class="text-xs text-muted">CPU</span><Sparkline data={c.cpuHistory} /></div>
                  <div class="obs-spark-item"><span class="text-xs text-muted">MEM</span><Sparkline data={c.memHistory} color="var(--warning)" /></div>
                  <div class="obs-spark-item"><span class="text-xs text-muted">RPS</span><Sparkline data={c.rpsHistory} color="var(--success)" /></div>
                </div>
                <div class="obs-comp-reqs">
                  <span class="text-xs"><strong>{c.activeRequests}</strong> active</span>
                  <span class="text-xs text-muted">{c.avgRps} avg rps</span>
                </div>
              </div>
            ))}
          </div>
        </div>

        <div class="obs-comm">
          <h4 class="obs-section-title">Communication <span class="text-xs text-muted" style="font-weight:400">from traces</span></h4>
          <div class="obs-comm-container">
            <CommGraph edges={edges} components={components} />
          </div>
        </div>
      </div>

      {/* ==== TIME-SCOPED SECTION (bottom) ==== */}

      {/* Timeline */}
      <div class="obs-section">
        <div class="obs-section-header">
          <h4>Request Load</h4>
          <div class="flex gap-sm" style="align-items:center">
            <span class="text-xs text-muted">dashed = deploys · red = errors</span>
            <div class="obs-range">
              {Object.keys(RANGE_MS).map(r => (
                <button key={r} class={`btn btn-sm${presetRange === r && !selectedRange ? ' btn-primary' : ''}`}
                  onClick={() => applyPreset(r)}>{r}</button>
              ))}
            </div>
          </div>
        </div>
        <Timeline points={load} deploys={deploys} selectedRange={selectedRange}
          onSelectRange={setSelectedRange} onResetRange={resetRange} />
      </div>

      {/* Error breakdown + Alerts */}
      <div class="obs-split">
        <div class="obs-section">
          <h4 class="obs-section-title">Error Breakdown <span class="text-xs text-muted" style="font-weight:400">click to filter traces</span></h4>
          {errors.length === 0 ? <div class="text-muted text-sm">No errors</div> : (
            <table class="table">
              <thead><tr><th>Type</th><th>Endpoint</th><th>Downstream</th><th>Count</th><th>Last</th></tr></thead>
              <tbody>
                {errors.map((e, i) => (
                  <tr key={i} class="table-link" onClick={() => { setTraceFilter(e.endpoint); setTraceTab('recent'); }}>
                    <td class="text-sm text-danger">{e.type}</td>
                    <td class="mono text-xs">{e.endpoint}</td>
                    <td class="text-xs">{e.downstream}</td>
                    <td class="text-sm">{e.count}</td>
                    <td class="text-muted text-xs">{e.lastSeen}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>

        <div class="obs-section">
          <h4 class="obs-section-title">Alerts</h4>
          {alerts.map((a, i) => (
            <div key={i} class="obs-alert-row">
              <span class="obs-alert-indicator" style={`background:${alertColor(a.status)}`} />
              <div class="obs-alert-info">
                <div class="text-sm" style="font-weight:500">{a.name}</div>
                <div class="text-xs text-muted">{a.message}</div>
              </div>
              <div class="obs-alert-meta">
                <span class={`obs-alert-badge ${a.status}`}>{a.status}</span>
                <span class="text-xs text-muted">{a.since}</span>
              </div>
            </div>
          ))}
        </div>
      </div>

      {/* Traces */}
      <div class="obs-section">
        <div class="obs-section-header">
          <h4>Traces
            {traceFilter && (
              <span class="obs-filter-chip">
                {traceFilter} (errors)
                <button class="obs-filter-chip-x" onClick={() => setTraceFilter(null)}>&times;</button>
              </span>
            )}
          </h4>
          <div class="flex gap-sm">
            {(['ongoing', 'recent', 'aggregated'] as const).map(t => (
              <button key={t} class={`btn btn-sm${traceTab === t ? ' btn-primary' : ''}`}
                onClick={() => { setTraceTab(t); setTraceFilter(null); }}>
                {t === 'ongoing' ? 'Ongoing' : t === 'recent' ? 'Recent' : 'Aggregated'}
              </button>
            ))}
          </div>
        </div>
        {traceTab !== 'aggregated' ? (
          <div class="card">
            {filteredTraces.length === 0 ? <div class="empty-state text-sm">No {traceFilter ? 'matching error' : traceTab} traces</div> : (
              <table class="table">
                <thead><tr><th>Name</th><th>Status</th><th>Duration</th><th>Spans</th><th>Time</th></tr></thead>
                <tbody>
                  {filteredTraces.map(t => (
                    <tr key={t.id} class="table-link" onClick={() => openTrace(t)}>
                      <td class="mono text-sm">{t.name}</td>
                      <td><span class={`obs-trace-status obs-trace-${t.status}`}>{t.status === 'running' ? '\u21BB' : t.status === 'ok' ? '\u2713' : '\u2717'} {t.status}</span></td>
                      <td class="mono text-sm">{t.duration != null ? `${t.duration}ms` : '\u2014'}</td>
                      <td class="text-sm">{t.spans}</td>
                      <td class="text-muted text-sm">{relTime(t.ts)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </div>
        ) : (
          <div class="card">
            <table class="table">
              <thead><tr><th>Trace Name</th><th>Count</th><th>Avg</th><th>Error %</th><th>p99</th></tr></thead>
              <tbody>
                {aggregated.map(a => (
                  <tr key={a.name} class="table-link">
                    <td class="mono text-sm">{a.name}</td>
                    <td class="text-sm">{a.count.toLocaleString()}</td>
                    <td class="mono text-sm">{a.avgDuration}ms</td>
                    <td><span class={a.errorRate > 2 ? 'text-danger' : a.errorRate > 0 ? 'text-warning' : ''}>{a.errorRate}%</span></td>
                    <td class="mono text-sm">{a.p99}ms</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>

      {/* Log templates */}
      <div class="obs-section">
        <div class="obs-section-header">
          <h4>Log Templates</h4>
          <span class="text-xs text-muted">Deduplicated by trace pattern</span>
        </div>
        <div class="card">
          <table class="table">
            <thead><tr><th>Template</th><th>Level</th><th>Count</th><th>Sample</th></tr></thead>
            <tbody>
              {logTemplates.map((lt, i) => (
                <tr key={i}>
                  <td class="mono text-xs" style="max-width:300px;word-break:break-all">{lt.template}</td>
                  <td><span class={`obs-log-level obs-log-${lt.level}`}>{lt.level}</span></td>
                  <td class="text-sm">{lt.count.toLocaleString()}</td>
                  <td class="text-xs text-muted truncate" style="max-width:250px">{lt.sample}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>

      {/* Standalone logs (not attached to traces) */}
      <div class="obs-section">
        <div class="obs-section-header">
          <h4>System Logs <span class="text-xs text-muted" style="font-weight:400">not attached to traces</span></h4>
          <div class="flex gap-sm" style="align-items:center">
            <select class="input" style="width:auto;font-size:0.75rem;padding:0.2rem 0.5rem" value={logLevel}
              onChange={(e) => setLogLevel((e.target as HTMLSelectElement).value)}>
              <option value="">All levels</option>
              <option value="error">Error</option>
              <option value="warn">Warn</option>
              <option value="info">Info</option>
              <option value="debug">Debug</option>
            </select>
            <input class="input" style="width:180px;font-size:0.75rem;padding:0.2rem 0.5rem"
              placeholder="Search logs..." value={logSearch}
              onInput={(e) => setLogSearch((e.target as HTMLInputElement).value)} />
          </div>
        </div>
        <div class="card">
          {filteredLogs.length === 0 ? <div class="empty-state text-sm">No matching logs</div> : (
            <table class="table">
              <thead><tr><th>Time</th><th>Level</th><th>Source</th><th>Message</th></tr></thead>
              <tbody>
                {filteredLogs.map(l => (
                  <tr key={l.id}>
                    <td class="text-muted text-xs mono" style="white-space:nowrap">{fmtTime(l.ts)}</td>
                    <td><span class={`obs-log-level obs-log-${l.level}`}>{l.level}</span></td>
                    <td class="text-xs">{l.source}</td>
                    <td class={`text-sm${l.level === 'error' ? ' text-danger' : ''}`}>{l.message}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
      </div>

      {/* Resource & response time metrics */}
      <div class="obs-metrics-grid">
        <div class="obs-section">
          <h4 class="obs-section-title">CPU Utilization <span class="text-xs text-muted" style="font-weight:400">millicores</span></h4>
          <MetricChart series={cpuSeries} unit="m" />
        </div>
        <div class="obs-section">
          <h4 class="obs-section-title">Memory Usage <span class="text-xs text-muted" style="font-weight:400">MiB</span></h4>
          <MetricChart series={memSeries} unit="Mi" />
        </div>
        <div class="obs-section" style="grid-column:1/-1">
          <h4 class="obs-section-title">Response Time (p50) <span class="text-xs text-muted" style="font-weight:400">ms per service</span></h4>
          <MetricChart series={responseSeries} unit="ms" />
        </div>
      </div>

      {/* Trace detail overlay */}
      <Overlay open={!!selectedTrace} onClose={() => setSelectedTrace(null)}
        title={selectedTrace ? `Trace: ${selectedTrace.name}` : ''}>
        {selectedTrace && (
          <div>
            <div class="flex gap-sm mb-md" style="align-items:center">
              <span class={`obs-trace-status obs-trace-${selectedTrace.status}`}>
                {selectedTrace.status === 'ok' ? '\u2713' : '\u2717'} {selectedTrace.status}
              </span>
              <span class="mono text-sm">{selectedTrace.duration}ms</span>
              <span class="text-muted text-sm">{selectedTrace.spans} spans</span>
              <span class="text-muted text-sm">{relTime(selectedTrace.ts)}</span>
            </div>
            <TraceWaterfall spans={traceSpans} traceName={selectedTrace.name} />
          </div>
        )}
      </Overlay>
    </div>
  );
}

import { useState, useEffect, useRef } from 'preact/hooks';
import { api, qs } from '../../lib/api';
import type { MetricSeries } from '../../lib/types';

const TIME_RANGES: { value: string; label: string; seconds: number }[] = [
  { value: '1h', label: '1 hour', seconds: 3600 },
  { value: '6h', label: '6 hours', seconds: 21600 },
  { value: '24h', label: '24 hours', seconds: 86400 },
  { value: '7d', label: '7 days', seconds: 604800 },
];

const CHART_COLORS = [
  '#3b82f6', '#22c55e', '#eab308', '#ef4444', '#a855f7',
  '#06b6d4', '#f97316', '#ec4899', '#14b8a6', '#8b5cf6',
];

export function Metrics() {
  const [names, setNames] = useState<string[]>([]);
  const [selectedMetric, setSelectedMetric] = useState('');
  const [labelFilter, setLabelFilter] = useState('');
  const [timeRange, setTimeRange] = useState('1h');
  const [series, setSeries] = useState<MetricSeries[]>([]);
  const [loading, setLoading] = useState(false);
  const [autoRefresh, setAutoRefresh] = useState(true);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const intervalRef = useRef<number | null>(null);

  useEffect(() => {
    api.get<string[]>('/api/observe/metrics/names')
      .then(n => { setNames(n); if (n.length > 0 && !selectedMetric) setSelectedMetric(n[0]); })
      .catch(() => {});
  }, []);

  const loadData = () => {
    if (!selectedMetric) return;
    setLoading(true);
    const params: Record<string, string> = { name: selectedMetric, range: timeRange };
    if (labelFilter) params.labels = labelFilter;

    api.get<MetricSeries[]>(`/api/observe/metrics/query${qs(params)}`)
      .then(setSeries)
      .catch(() => setSeries([]))
      .finally(() => setLoading(false));
  };

  useEffect(() => {
    loadData();
  }, [selectedMetric, timeRange]);

  useEffect(() => {
    if (autoRefresh) {
      intervalRef.current = window.setInterval(loadData, 30000);
      return () => { if (intervalRef.current) clearInterval(intervalRef.current); };
    } else {
      if (intervalRef.current) clearInterval(intervalRef.current);
    }
  }, [autoRefresh, selectedMetric, timeRange, labelFilter]);

  // Draw chart on canvas
  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas || series.length === 0) return;

    const ctx = canvas.getContext('2d');
    if (!ctx) return;

    const dpr = window.devicePixelRatio || 1;
    const rect = canvas.getBoundingClientRect();
    canvas.width = rect.width * dpr;
    canvas.height = rect.height * dpr;
    ctx.scale(dpr, dpr);
    const w = rect.width;
    const h = rect.height;

    const padding = { top: 20, right: 20, bottom: 40, left: 60 };
    const chartW = w - padding.left - padding.right;
    const chartH = h - padding.top - padding.bottom;

    // Gather all points
    let allTimes: number[] = [];
    let minVal = Infinity, maxVal = -Infinity;
    for (const s of series) {
      for (const p of s.points) {
        const t = new Date(p.timestamp).getTime();
        allTimes.push(t);
        if (p.value < minVal) minVal = p.value;
        if (p.value > maxVal) maxVal = p.value;
      }
    }
    if (allTimes.length === 0) return;

    const minTime = Math.min(...allTimes);
    const maxTime = Math.max(...allTimes);
    const timeRange = maxTime - minTime || 1;
    const valRange = maxVal - minVal || 1;
    const valPad = valRange * 0.1;
    const adjMin = minVal - valPad;
    const adjMax = maxVal + valPad;
    const adjRange = adjMax - adjMin;

    // Clear
    ctx.fillStyle = getComputedStyle(document.documentElement).getPropertyValue('--bg-primary').trim() || '#0a0a0a';
    ctx.fillRect(0, 0, w, h);

    // Grid lines
    ctx.strokeStyle = getComputedStyle(document.documentElement).getPropertyValue('--border').trim() || '#2a2a2a';
    ctx.lineWidth = 0.5;
    for (let i = 0; i <= 4; i++) {
      const y = padding.top + (chartH / 4) * i;
      ctx.beginPath();
      ctx.moveTo(padding.left, y);
      ctx.lineTo(padding.left + chartW, y);
      ctx.stroke();

      const val = adjMax - (adjRange / 4) * i;
      ctx.fillStyle = getComputedStyle(document.documentElement).getPropertyValue('--text-muted').trim() || '#666';
      ctx.font = '11px -apple-system, sans-serif';
      ctx.textAlign = 'right';
      ctx.fillText(formatValue(val), padding.left - 8, y + 4);
    }

    // Time labels
    ctx.textAlign = 'center';
    for (let i = 0; i <= 4; i++) {
      const t = minTime + (timeRange / 4) * i;
      const x = padding.left + (chartW / 4) * i;
      const d = new Date(t);
      ctx.fillText(d.toLocaleTimeString('en-US', { hour12: false, hour: '2-digit', minute: '2-digit' }),
        x, h - padding.bottom + 20);
    }

    // Draw series
    series.forEach((s, si) => {
      if (s.points.length === 0) return;
      const color = CHART_COLORS[si % CHART_COLORS.length];
      ctx.strokeStyle = color;
      ctx.lineWidth = 1.5;
      ctx.beginPath();

      const sorted = [...s.points].sort((a, b) =>
        new Date(a.timestamp).getTime() - new Date(b.timestamp).getTime()
      );

      sorted.forEach((p, pi) => {
        const t = new Date(p.timestamp).getTime();
        const x = padding.left + ((t - minTime) / timeRange) * chartW;
        const y = padding.top + chartH - ((p.value - adjMin) / adjRange) * chartH;
        if (pi === 0) ctx.moveTo(x, y);
        else ctx.lineTo(x, y);
      });
      ctx.stroke();
    });

  }, [series]);

  const formatValue = (v: number): string => {
    if (Math.abs(v) >= 1000000) return (v / 1000000).toFixed(1) + 'M';
    if (Math.abs(v) >= 1000) return (v / 1000).toFixed(1) + 'K';
    return v.toFixed(1);
  };

  const formatLabels = (labels: Record<string, string>): string => {
    return Object.entries(labels).map(([k, v]) => `${k}=${v}`).join(', ');
  };

  return (
    <div>
      <h2 class="mb-md">Metrics</h2>
      <div class="filter-bar">
        <div class="filter-item">
          <label class="filter-label">Metric</label>
          <select class="input filter-input" value={selectedMetric}
            onChange={(e) => setSelectedMetric((e.target as HTMLSelectElement).value)}>
            {names.map(n => <option key={n} value={n}>{n}</option>)}
          </select>
        </div>
        <div class="filter-item">
          <label class="filter-label">Labels</label>
          <input class="input filter-input" value={labelFilter}
            placeholder="method=POST, status=200"
            onInput={(e) => setLabelFilter((e.target as HTMLInputElement).value)} />
        </div>
        <div class="filter-item">
          <label class="filter-label">Time range</label>
          <select class="input filter-input" value={timeRange}
            onChange={(e) => setTimeRange((e.target as HTMLSelectElement).value)}>
            {TIME_RANGES.map(r => <option key={r.value} value={r.value}>{r.label}</option>)}
          </select>
        </div>
        <div class="filter-item filter-actions">
          <button class="btn btn-primary btn-sm" onClick={loadData}>Query</button>
        </div>
      </div>

      <div class="flex gap-sm mb-md" style="align-items:center">
        <label class="flex gap-sm" style="align-items:center;cursor:pointer">
          <input type="checkbox" checked={autoRefresh}
            onChange={(e) => setAutoRefresh((e.target as HTMLInputElement).checked)} />
          <span class="text-sm">Auto-refresh (30s)</span>
        </label>
      </div>

      <div class="card">
        {loading && series.length === 0 ? (
          <div class="empty-state">Loading...</div>
        ) : series.length === 0 ? (
          <div class="empty-state">No data for selected metric</div>
        ) : (
          <div>
            <canvas ref={canvasRef} class="metrics-chart" style="width:100%;height:300px" />
            {series.length > 1 && (
              <div class="metrics-legend">
                {series.map((s, i) => (
                  <div key={i} class="legend-item">
                    <span class="legend-color" style={{ backgroundColor: CHART_COLORS[i % CHART_COLORS.length] }} />
                    <span class="text-xs">{formatLabels(s.labels) || s.name}</span>
                  </div>
                ))}
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  );
}

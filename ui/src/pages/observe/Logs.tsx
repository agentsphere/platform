import { useState, useEffect, useRef } from 'preact/hooks';
import { api, qs, type ListResponse } from '../../lib/api';
import type { LogEntry } from '../../lib/types';
import { FilterBar, type FilterDef } from '../../components/FilterBar';
import { Pagination } from '../../components/Pagination';
import { createWs, type ReconnectingWebSocket } from '../../lib/ws';

const TIME_RANGES = [
  { value: '1h', label: 'Last 1 hour' },
  { value: '6h', label: 'Last 6 hours' },
  { value: '24h', label: 'Last 24 hours' },
  { value: '7d', label: 'Last 7 days' },
];

const LEVELS = [
  { value: '', label: 'All levels' },
  { value: 'error', label: 'Error' },
  { value: 'warn', label: 'Warn' },
  { value: 'info', label: 'Info' },
  { value: 'debug', label: 'Debug' },
  { value: 'trace', label: 'Trace' },
];

const FILTERS: FilterDef[] = [
  { key: 'time_range', label: 'Time range', type: 'select', options: TIME_RANGES },
  { key: 'level', label: 'Level', type: 'select', options: LEVELS },
  { key: 'service', label: 'Service', type: 'text', placeholder: 'All services' },
  { key: 'query', label: 'Search', type: 'text', placeholder: 'Full-text search...' },
];

const LEVEL_CLASSES: Record<string, string> = {
  error: 'log-level-error',
  warn: 'log-level-warn',
  info: 'log-level-info',
  debug: 'log-level-debug',
  trace: 'log-level-trace',
};

export function Logs() {
  const [logs, setLogs] = useState<LogEntry[]>([]);
  const [total, setTotal] = useState(0);
  const [offset, setOffset] = useState(0);
  const [filters, setFilters] = useState<Record<string, string>>({ time_range: '1h', level: '' });
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [liveTail, setLiveTail] = useState(false);
  const [loading, setLoading] = useState(false);
  const wsRef = useRef<ReconnectingWebSocket | null>(null);

  const load = () => {
    setLoading(true);
    const params: Record<string, string | number> = { limit: 50, offset };
    if (filters.time_range) params.time_range = filters.time_range;
    if (filters.level) params.level = filters.level;
    if (filters.service) params.service = filters.service;
    if (filters.query) params.query = filters.query;

    api.get<ListResponse<LogEntry>>(`/api/observe/logs${qs(params)}`)
      .then(r => { setLogs(r.items); setTotal(r.total); })
      .catch(() => {})
      .finally(() => setLoading(false));
  };

  useEffect(load, [offset]);

  useEffect(() => {
    if (liveTail) {
      const ws = createWs({
        url: '/api/observe/logs/tail',
        onMessage: (entry: LogEntry) => {
          setLogs(prev => [entry, ...prev].slice(0, 200));
        },
      });
      wsRef.current = ws;
      return () => ws.close();
    } else if (wsRef.current) {
      wsRef.current.close();
      wsRef.current = null;
    }
  }, [liveTail]);

  const toggleExpand = (id: string) => {
    setExpanded(prev => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const formatTime = (ts: string) => {
    const d = new Date(ts);
    return d.toLocaleTimeString('en-US', { hour12: false, hour: '2-digit', minute: '2-digit', second: '2-digit' });
  };

  return (
    <div>
      <h2 class="mb-md">Log Search</h2>
      <FilterBar filters={FILTERS} values={filters} onChange={setFilters} onApply={() => { setOffset(0); load(); }} />
      <div class="flex gap-sm mb-md" style="align-items:center">
        <label class="flex gap-sm" style="align-items:center;cursor:pointer">
          <input type="checkbox" checked={liveTail}
            onChange={(e) => setLiveTail((e.target as HTMLInputElement).checked)} />
          <span class="text-sm">Live tail</span>
        </label>
        {liveTail && <span class="status-dot-wrapper"><span class="status-dot" style="background:var(--success)" /><span class="text-xs text-muted">Streaming</span></span>}
      </div>
      <div class="card">
        {loading ? (
          <div class="empty-state">Loading...</div>
        ) : logs.length === 0 ? (
          <div class="empty-state">No log entries found</div>
        ) : (
          <div class="log-list">
            {logs.map(entry => (
              <div key={entry.id} class="log-entry" onClick={() => toggleExpand(entry.id)}>
                <div class="log-entry-row">
                  <span class="log-time mono text-xs">{formatTime(entry.timestamp)}</span>
                  <span class={`log-level ${LEVEL_CLASSES[entry.level.toLowerCase()] || ''}`}>
                    {entry.level.toUpperCase().padEnd(5)}
                  </span>
                  <span class="log-service text-xs">{entry.service}</span>
                  <span class="log-message">{entry.message}</span>
                  {entry.trace_id && (
                    <a class="log-trace-link text-xs"
                      href={`/observe/traces/${entry.trace_id}`}
                      onClick={(e) => e.stopPropagation()}>
                      trace
                    </a>
                  )}
                </div>
                {expanded.has(entry.id) && entry.attributes && (
                  <div class="log-attributes">
                    <pre class="log-viewer" style="max-height:200px;margin-top:0.5rem">
                      {JSON.stringify(entry.attributes, null, 2)}
                    </pre>
                  </div>
                )}
              </div>
            ))}
          </div>
        )}
        {!liveTail && <Pagination total={total} limit={50} offset={offset} onChange={setOffset} />}
      </div>
    </div>
  );
}

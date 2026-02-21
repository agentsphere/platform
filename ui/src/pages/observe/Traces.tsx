import { useState, useEffect } from 'preact/hooks';
import { api, qs, type ListResponse } from '../../lib/api';
import type { TraceSummary, Span } from '../../lib/types';
import { FilterBar, type FilterDef } from '../../components/FilterBar';
import { Pagination } from '../../components/Pagination';
import { Badge } from '../../components/Badge';
import { duration } from '../../lib/format';

const FILTERS: FilterDef[] = [
  { key: 'service', label: 'Service', type: 'text', placeholder: 'All services' },
  { key: 'status', label: 'Status', type: 'select', options: [
    { value: '', label: 'All' },
    { value: 'ok', label: 'OK' },
    { value: 'error', label: 'Error' },
  ]},
  { key: 'time_range', label: 'Time range', type: 'select', options: [
    { value: '1h', label: 'Last 1 hour' },
    { value: '6h', label: 'Last 6 hours' },
    { value: '24h', label: 'Last 24 hours' },
    { value: '7d', label: 'Last 7 days' },
  ]},
];

interface TraceListProps {
  path?: string;
}

export function Traces({ path }: TraceListProps) {
  const [traces, setTraces] = useState<TraceSummary[]>([]);
  const [total, setTotal] = useState(0);
  const [offset, setOffset] = useState(0);
  const [filters, setFilters] = useState<Record<string, string>>({ time_range: '1h', status: '' });
  const [loading, setLoading] = useState(false);

  const load = () => {
    setLoading(true);
    const params: Record<string, string | number> = { limit: 50, offset };
    if (filters.service) params.service = filters.service;
    if (filters.status) params.status = filters.status;
    if (filters.time_range) params.time_range = filters.time_range;

    api.get<ListResponse<TraceSummary>>(`/api/observe/traces${qs(params)}`)
      .then(r => { setTraces(r.items); setTotal(r.total); })
      .catch(() => {})
      .finally(() => setLoading(false));
  };

  useEffect(load, [offset]);

  return (
    <div>
      <h2 class="mb-md">Traces</h2>
      <FilterBar filters={FILTERS} values={filters} onChange={setFilters} onApply={() => { setOffset(0); load(); }} />
      <div class="card">
        {loading ? (
          <div class="empty-state">Loading...</div>
        ) : traces.length === 0 ? (
          <div class="empty-state">No traces found</div>
        ) : (
          <table class="table">
            <thead>
              <tr>
                <th>Trace ID</th>
                <th>Root Span</th>
                <th>Service</th>
                <th>Duration</th>
                <th>Status</th>
                <th>Started</th>
              </tr>
            </thead>
            <tbody>
              {traces.map(t => (
                <tr key={t.trace_id} class="table-link"
                  onClick={() => { window.location.href = `/observe/traces/${t.trace_id}`; }}>
                  <td class="mono text-xs">{t.trace_id.substring(0, 16)}...</td>
                  <td>{t.root_span}</td>
                  <td class="text-sm">{t.service}</td>
                  <td class="text-sm">{t.duration_ms != null ? duration(t.duration_ms) : '--'}</td>
                  <td><Badge status={t.status} /></td>
                  <td class="text-muted text-sm">{new Date(t.started_at).toLocaleTimeString()}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
        <Pagination total={total} limit={50} offset={offset} onChange={setOffset} />
      </div>
    </div>
  );
}

interface TraceDetailProps {
  traceId?: string;
}

export function TraceDetail({ traceId }: TraceDetailProps) {
  const [spans, setSpans] = useState<Span[]>([]);
  const [selected, setSelected] = useState<Span | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    if (!traceId) return;
    setLoading(true);
    api.get<Span[]>(`/api/observe/traces/${traceId}`)
      .then(setSpans)
      .catch(() => setSpans([]))
      .finally(() => setLoading(false));
  }, [traceId]);

  if (loading) return <div class="empty-state">Loading...</div>;
  if (spans.length === 0) return <div class="empty-state">No spans found</div>;

  const traceStart = Math.min(...spans.map(s => new Date(s.started_at).getTime()));
  const traceEnd = Math.max(...spans.map(s => {
    const start = new Date(s.started_at).getTime();
    return s.duration_ms ? start + s.duration_ms : start;
  }));
  const traceDuration = traceEnd - traceStart || 1;

  // Build tree structure
  const rootSpans: Span[] = [];
  const childMap = new Map<string, Span[]>();
  for (const span of spans) {
    if (span.parent_span_id) {
      const children = childMap.get(span.parent_span_id) || [];
      children.push(span);
      childMap.set(span.parent_span_id, children);
    } else {
      rootSpans.push(span);
    }
  }

  const renderSpan = (span: Span, depth: number): any => {
    const startOffset = new Date(span.started_at).getTime() - traceStart;
    const leftPct = (startOffset / traceDuration) * 100;
    const widthPct = Math.max(((span.duration_ms || 1) / traceDuration) * 100, 0.5);
    const children = childMap.get(span.span_id) || [];
    const isSelected = selected?.span_id === span.span_id;

    return (
      <div key={span.span_id}>
        <div class={`waterfall-row ${isSelected ? 'waterfall-row-selected' : ''}`}
          onClick={() => setSelected(isSelected ? null : span)}>
          <div class="waterfall-label" style={{ paddingLeft: `${depth * 20 + 8}px` }}>
            <span class="waterfall-name">{span.name}</span>
            <span class="text-xs text-muted" style="margin-left:0.5rem">
              {span.duration_ms != null ? duration(span.duration_ms) : ''}
            </span>
          </div>
          <div class="waterfall-bar-container">
            <div class={`waterfall-bar waterfall-bar-${span.status === 'error' ? 'error' : 'ok'}`}
              style={{ left: `${leftPct}%`, width: `${widthPct}%` }} />
          </div>
        </div>
        {children
          .sort((a, b) => new Date(a.started_at).getTime() - new Date(b.started_at).getTime())
          .map(child => renderSpan(child, depth + 1))}
      </div>
    );
  };

  return (
    <div>
      <div class="mb-md">
        <a href="/observe/traces" class="text-sm text-muted">Back to traces</a>
      </div>
      <h2 class="mb-md">
        Trace <span class="mono text-sm">{traceId}</span>
      </h2>
      <div class="flex gap-md mb-md text-sm text-muted">
        <span>Total: {duration(traceDuration)}</span>
        <span>Spans: {spans.length}</span>
      </div>

      <div class="card">
        <div class="waterfall">
          {rootSpans
            .sort((a, b) => new Date(a.started_at).getTime() - new Date(b.started_at).getTime())
            .map(span => renderSpan(span, 0))}
        </div>
      </div>

      {selected && (
        <div class="card mt-md">
          <div class="card-header">
            <span class="card-title">{selected.name}</span>
            <button class="btn btn-sm btn-ghost" onClick={() => setSelected(null)}>Close</button>
          </div>
          <div class="flex gap-md text-sm mb-md">
            <span><span class="text-muted">Service:</span> {selected.service}</span>
            <span><span class="text-muted">Kind:</span> {selected.kind}</span>
            <span><span class="text-muted">Status:</span> <Badge status={selected.status} /></span>
            {selected.duration_ms != null && (
              <span><span class="text-muted">Duration:</span> {duration(selected.duration_ms)}</span>
            )}
          </div>
          {selected.attributes && Object.keys(selected.attributes).length > 0 && (
            <div class="mt-md">
              <div class="text-sm text-muted mb-sm">Attributes</div>
              <pre class="log-viewer" style="max-height:200px">
                {JSON.stringify(selected.attributes, null, 2)}
              </pre>
            </div>
          )}
          {selected.events && selected.events.length > 0 && (
            <div class="mt-md">
              <div class="text-sm text-muted mb-sm">Events</div>
              {selected.events.map((ev, i) => (
                <div key={i} class="comment-box" style="padding:0.5rem">
                  <span class="text-sm">{ev.name}</span>
                  <span class="text-xs text-muted" style="margin-left:0.5rem">
                    {new Date(ev.timestamp).toLocaleTimeString()}
                  </span>
                  {ev.attributes && (
                    <pre class="text-xs mono" style="margin-top:0.25rem;color:var(--text-secondary)">
                      {JSON.stringify(ev.attributes, null, 2)}
                    </pre>
                  )}
                </div>
              ))}
            </div>
          )}
          <div class="mt-md">
            <a href={`/observe/logs?trace_id=${selected.span_id}`} class="text-sm">
              View related logs
            </a>
          </div>
        </div>
      )}
    </div>
  );
}

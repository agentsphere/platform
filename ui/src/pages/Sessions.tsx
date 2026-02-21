import { useState, useEffect } from 'preact/hooks';
import { api, qs, type ListResponse } from '../lib/api';
import type { AgentSession } from '../lib/types';
import { timeAgo } from '../lib/format';
import { Badge } from '../components/Badge';
import { Modal } from '../components/Modal';
import { Pagination } from '../components/Pagination';

interface Props {
  projectId: string;
}

export function Sessions({ projectId }: Props) {
  const [sessions, setSessions] = useState<AgentSession[]>([]);
  const [total, setTotal] = useState(0);
  const [offset, setOffset] = useState(0);
  const [statusFilter, setStatusFilter] = useState('');
  const [showCreate, setShowCreate] = useState(false);
  const [form, setForm] = useState({ prompt: '', provider: 'claude-code', branch: '' });
  const [error, setError] = useState('');

  const load = () => {
    const params: Record<string, string | number> = { limit: 20, offset };
    if (statusFilter) params.status = statusFilter;
    api.get<ListResponse<AgentSession>>(`/api/projects/${projectId}/sessions${qs(params)}`)
      .then(r => { setSessions(r.items); setTotal(r.total); }).catch(() => {});
  };

  useEffect(load, [projectId, offset, statusFilter]);

  const create = async (e: Event) => {
    e.preventDefault();
    setError('');
    try {
      await api.post(`/api/projects/${projectId}/sessions`, {
        prompt: form.prompt,
        provider: form.provider,
        branch: form.branch || undefined,
      });
      setShowCreate(false);
      setForm({ prompt: '', provider: 'claude-code', branch: '' });
      load();
    } catch (err: any) { setError(err.message); }
  };

  const statuses = ['', 'pending', 'running', 'completed', 'failed', 'stopped'];

  return (
    <div>
      <div class="flex-between mb-md">
        <div class="flex gap-sm">
          {statuses.map(s => (
            <button key={s} class={`btn btn-sm${statusFilter === s ? ' btn-primary' : ''}`}
              onClick={() => { setStatusFilter(s); setOffset(0); }}>
              {s || 'All'}
            </button>
          ))}
        </div>
        <button class="btn btn-primary btn-sm" onClick={() => setShowCreate(true)}>New Session</button>
      </div>
      <div class="card">
        {sessions.length === 0 ? (
          <div class="empty-state">No sessions</div>
        ) : (
          <table class="table">
            <thead>
              <tr>
                <th>Status</th>
                <th>Prompt</th>
                <th>Provider</th>
                <th>Branch</th>
                <th>Tokens</th>
                <th>Created</th>
              </tr>
            </thead>
            <tbody>
              {sessions.map(s => (
                <tr key={s.id} class="table-link"
                  onClick={() => { window.location.href = `/projects/${projectId}/sessions/${s.id}`; }}>
                  <td><Badge status={s.status} /></td>
                  <td class="truncate" style="max-width:300px">{s.prompt}</td>
                  <td class="text-sm">{s.provider}</td>
                  <td class="mono text-xs">{s.branch || '--'}</td>
                  <td class="text-sm">{s.cost_tokens != null ? s.cost_tokens.toLocaleString() : '--'}</td>
                  <td class="text-muted text-sm">{timeAgo(s.created_at)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
        <Pagination total={total} limit={20} offset={offset} onChange={setOffset} />
      </div>

      <Modal open={showCreate} onClose={() => setShowCreate(false)} title="New Agent Session">
        <form onSubmit={create}>
          <div class="form-group">
            <label>Prompt</label>
            <textarea class="input" required rows={4} value={form.prompt}
              placeholder="Describe the task for the agent..."
              onInput={(e) => setForm({ ...form, prompt: (e.target as HTMLTextAreaElement).value })} />
          </div>
          <div class="form-group">
            <label>Provider</label>
            <select class="input" value={form.provider}
              onChange={(e) => setForm({ ...form, provider: (e.target as HTMLSelectElement).value })}>
              <option value="claude-code">Claude Code</option>
            </select>
          </div>
          <div class="form-group">
            <label>Branch (optional)</label>
            <input class="input" value={form.branch}
              placeholder="Leave empty for auto-generated branch"
              onInput={(e) => setForm({ ...form, branch: (e.target as HTMLInputElement).value })} />
          </div>
          {error && <div class="error-msg">{error}</div>}
          <div class="modal-actions">
            <button type="button" class="btn" onClick={() => setShowCreate(false)}>Cancel</button>
            <button type="submit" class="btn btn-primary">Create Session</button>
          </div>
        </form>
      </Modal>
    </div>
  );
}

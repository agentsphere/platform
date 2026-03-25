import { useState, useEffect } from 'preact/hooks';
import { api, qs, type ListResponse } from '../lib/api';
import type { Workspace } from '../lib/types';
import { timeAgo } from '../lib/format';
import { Pagination } from '../components/Pagination';
import { Modal } from '../components/Modal';

const LIMIT = 20;

export function Workspaces() {
  const [workspaces, setWorkspaces] = useState<Workspace[]>([]);
  const [total, setTotal] = useState(0);
  const [offset, setOffset] = useState(0);
  const [showCreate, setShowCreate] = useState(false);
  const [form, setForm] = useState({ name: '', display_name: '', description: '' });
  const [error, setError] = useState('');

  const load = () => {
    api.get<ListResponse<Workspace>>(`/api/workspaces${qs({ limit: LIMIT, offset })}`)
      .then(r => { setWorkspaces(r.items); setTotal(r.total); })
      .catch(e => console.warn(e));
  };

  useEffect(load, [offset]);

  const create = async (e: Event) => {
    e.preventDefault();
    setError('');
    try {
      const w = await api.post<Workspace>('/api/workspaces', {
        name: form.name,
        display_name: form.display_name || undefined,
        description: form.description || undefined,
      });
      setShowCreate(false);
      setForm({ name: '', display_name: '', description: '' });
      window.location.href = `/workspaces/${w.id}`;
    } catch (err: any) {
      setError(err.message || 'Failed to create workspace');
    }
  };

  return (
    <div>
      <div class="flex-between mb-md">
        <h2>Workspaces</h2>
        <button class="btn btn-primary" onClick={() => setShowCreate(true)}>New Workspace</button>
      </div>
      <div class="card">
        {workspaces.length === 0 ? (
          <div class="empty-state">No workspaces found</div>
        ) : (
          <table class="table">
            <thead>
              <tr><th>Name</th><th>Description</th><th>Updated</th></tr>
            </thead>
            <tbody>
              {workspaces.map(w => (
                <tr key={w.id} class="table-link" onClick={() => { window.location.href = `/workspaces/${w.id}`; }}>
                  <td><a href={`/workspaces/${w.id}`}>{w.display_name || w.name}</a></td>
                  <td class="text-muted text-sm">{w.description || ''}</td>
                  <td class="text-muted text-sm">{timeAgo(w.updated_at)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
        <Pagination total={total} limit={LIMIT} offset={offset} onChange={setOffset} />
      </div>

      <Modal open={showCreate} onClose={() => setShowCreate(false)} title="New Workspace">
        <form onSubmit={create}>
          <div class="form-group">
            <label>Name</label>
            <input class="input" required value={form.name}
              onInput={(e) => setForm({ ...form, name: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label>Display Name</label>
            <input class="input" value={form.display_name}
              onInput={(e) => setForm({ ...form, display_name: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label>Description</label>
            <textarea class="input" value={form.description}
              onInput={(e) => setForm({ ...form, description: (e.target as HTMLTextAreaElement).value })} />
          </div>
          {error && <div class="error-msg">{error}</div>}
          <div class="modal-actions">
            <button type="button" class="btn" onClick={() => setShowCreate(false)}>Cancel</button>
            <button type="submit" class="btn btn-primary">Create</button>
          </div>
        </form>
      </Modal>
    </div>
  );
}

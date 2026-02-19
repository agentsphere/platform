import { useState, useEffect } from 'preact/hooks';
import { api } from '../../lib/api';
import type { Delegation } from '../../lib/types';
import { timeAgo } from '../../lib/format';
import { Modal } from '../../components/Modal';

export function Delegations() {
  const [delegations, setDelegations] = useState<Delegation[]>([]);
  const [showCreate, setShowCreate] = useState(false);
  const [form, setForm] = useState({ delegate_id: '', permission: '', project_id: '', expires_at: '', reason: '' });
  const [error, setError] = useState('');

  const load = () => {
    api.get<Delegation[]>('/api/admin/delegations').then(setDelegations).catch(() => {});
  };
  useEffect(load, []);

  const create = async (e: Event) => {
    e.preventDefault();
    setError('');
    try {
      await api.post('/api/admin/delegations', {
        delegate_id: form.delegate_id,
        permission: form.permission,
        project_id: form.project_id || undefined,
        expires_at: form.expires_at || undefined,
        reason: form.reason || undefined,
      });
      setShowCreate(false);
      setForm({ delegate_id: '', permission: '', project_id: '', expires_at: '', reason: '' });
      load();
    } catch (err: any) { setError(err.message); }
  };

  const revoke = async (id: string) => {
    await api.del(`/api/admin/delegations/${id}`);
    load();
  };

  return (
    <div>
      <div class="flex-between mb-md">
        <h2>Delegations</h2>
        <button class="btn btn-primary" onClick={() => { setShowCreate(true); setError(''); }}>New Delegation</button>
      </div>
      <div class="card">
        {delegations.length === 0 ? <div class="empty-state">No delegations</div> : (
          <table class="table">
            <thead><tr><th>Permission</th><th>Delegate</th><th>Project</th><th>Expires</th><th>Reason</th><th></th></tr></thead>
            <tbody>
              {delegations.map(d => (
                <tr key={d.id}>
                  <td class="mono text-sm">{d.permission}</td>
                  <td class="text-sm">{d.delegate_id.substring(0, 8)}...</td>
                  <td class="text-sm">{d.project_id ? d.project_id.substring(0, 8) + '...' : '—'}</td>
                  <td class="text-sm text-muted">{d.expires_at ? timeAgo(d.expires_at) : 'never'}</td>
                  <td class="text-sm text-muted">{d.reason || '—'}</td>
                  <td><button class="btn btn-danger btn-sm" onClick={() => revoke(d.id)}>Revoke</button></td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>

      <Modal open={showCreate} onClose={() => setShowCreate(false)} title="New Delegation">
        <form onSubmit={create}>
          <div class="form-group">
            <label>Delegate User ID</label>
            <input class="input" required value={form.delegate_id}
              onInput={(e) => setForm({ ...form, delegate_id: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label>Permission</label>
            <input class="input" required placeholder="e.g. project:write" value={form.permission}
              onInput={(e) => setForm({ ...form, permission: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label>Project ID (optional)</label>
            <input class="input" value={form.project_id}
              onInput={(e) => setForm({ ...form, project_id: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label>Expires At (optional, ISO 8601)</label>
            <input class="input" type="datetime-local" value={form.expires_at}
              onInput={(e) => setForm({ ...form, expires_at: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label>Reason (optional)</label>
            <input class="input" value={form.reason}
              onInput={(e) => setForm({ ...form, reason: (e.target as HTMLInputElement).value })} />
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
